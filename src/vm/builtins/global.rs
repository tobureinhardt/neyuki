//! The global functions: print, assert, type, tostring, tonumber, error,
//! pcall, require and friends.

use num_bigint::BigInt;
use num_traits::{FromPrimitive, ToPrimitive};
use std::str::FromStr;

use crate::vm::machine::VM;
use crate::vm::value::Value;

pub fn builtin_print(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    use std::io::Write;
    let mut out = String::new();
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            out.push('\t');
        }
        match arg {
            Value::String(s) => out.push_str(s),
            other => out.push_str(&tostring(vm, other)?),
        }
    }
    out.push('\n');
    if vm.dap.is_some() {
        crate::dap::output(vm, "stdout", &out);
    } else {
        // A closed pipe (`neyuki run x | head`) is not the script's error.
        let _ = std::io::stdout().lock().write_all(out.as_bytes());
    }
    Ok(Vec::new())
}

pub fn builtin_assert(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let cond = args.first().cloned().unwrap_or(Value::Nil);
    if !cond.is_truthy() {
        let msg = args
            .get(1)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "assertion failed".to_string());
        return Err(msg);
    }
    Ok(vec![cond])
}

pub fn builtin_type(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args.first().unwrap_or(&Value::Nil);
    Ok(vec![Value::String((val.type_name().to_string()).into())])
}

pub fn builtin_typeof(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args.first().unwrap_or(&Value::Nil);
    Ok(vec![Value::String((val.typeof_name().to_string()).into())])
}

pub fn builtin_checktype(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args.first().unwrap_or(&Value::Nil);
    let ty_str = match args.get(1) {
        Some(Value::String(s)) => &**s,
        _ => return Err("checktype expects a type string as argument #2".to_string()),
    };
    let ty = crate::ast::ty::TypeExpr::parse(ty_str);
    Ok(vec![Value::Bool(val.matches_type_expr(&ty))])
}

pub fn builtin_asserttype(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args.first().unwrap_or(&Value::Nil);
    let ty_str = match args.get(1) {
        Some(Value::String(s)) => &**s,
        _ => return Err("asserttype expects a type string as argument #2".to_string()),
    };
    let ty = crate::ast::ty::TypeExpr::parse(ty_str);
    if !val.matches_type_expr(&ty) {
        let msg = args.get(2).map(|v| v.to_string()).unwrap_or_else(|| {
            format!(
                "type assertion failed: expected '{}', got '{}'",
                ty_str,
                val.typeof_name()
            )
        });
        return Err(msg);
    }
    Ok(vec![val.clone()])
}

/// A value as `tostring` prints it, honoring `__tostring`.
pub fn tostring(vm: &mut VM, val: &Value) -> Result<String, String> {
    if let Value::Table(t) = val {
        let handler = t.borrow().metamethod("__tostring");
        if let Some(h) = handler {
            let res = vm.call_function(h, std::slice::from_ref(val))?;
            return Ok(res.into_iter().next().unwrap_or(Value::Nil).to_string());
        }
    }
    Ok(val.to_string())
}

pub fn builtin_tostring(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args.first().unwrap_or(&Value::Nil);
    if let Value::String(_) = val {
        return Ok(vec![val.clone()]);
    }
    Ok(vec![Value::string(tostring(vm, val)?)])
}

pub fn builtin_tonumber(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args.first().unwrap_or(&Value::Nil);
    let base_opt = args.get(1);

    if let Some(base_val) = base_opt
        && !matches!(base_val, Value::Nil)
    {
        let base = match base_val {
            Value::Int(i) => i.to_i64().unwrap_or(0),
            Value::Float(f) => *f as i64,
            _ => return Err("bad argument #2 to 'tonumber' (base out of range)".to_string()),
        };
        if !(2..=36).contains(&base) {
            return Err("bad argument #2 to 'tonumber' (base out of range)".to_string());
        }
        let s = match val {
            Value::String(s) => &**s,
            _ => return Ok(vec![Value::Nil]),
        };
        if s.len() > 65_536 {
            return Ok(vec![Value::Nil]);
        }
        let s_trimmed = s.trim();
        let (sign, s_digits) = if let Some(stripped) = s_trimmed.strip_prefix('-') {
            (-1, stripped.trim_start())
        } else if let Some(stripped) = s_trimmed.strip_prefix('+') {
            (1, stripped.trim_start())
        } else {
            (1, s_trimmed)
        };
        let s_digits = if base == 16 {
            if let Some(stripped) = s_digits
                .strip_prefix("0x")
                .or_else(|| s_digits.strip_prefix("0X"))
            {
                stripped
            } else {
                s_digits
            }
        } else {
            s_digits
        };
        if s_digits.is_empty() {
            return Ok(vec![Value::Nil]);
        }
        return match BigInt::parse_bytes(s_digits.as_bytes(), base as u32) {
            Some(bi) => {
                let bi = if sign < 0 { -bi } else { bi };
                Ok(vec![Value::from_bigint(bi)])
            }
            None => Ok(vec![Value::Nil]),
        };
    }

    match val {
        Value::Int(i) => Ok(vec![Value::Int(*i)]),
        Value::BigInt(_) => Ok(vec![val.clone()]),
        Value::Float(f) => Ok(vec![Value::Float(*f)]),
        Value::String(s) => {
            if s.len() > 65_536 {
                return Ok(vec![Value::Nil]);
            }
            let s_trimmed = s.trim();
            let (sign, s_rest) = if let Some(stripped) = s_trimmed.strip_prefix('-') {
                (-1, stripped.trim_start())
            } else if let Some(stripped) = s_trimmed.strip_prefix('+') {
                (1, stripped.trim_start())
            } else {
                (1, s_trimmed)
            };
            if let Some(stripped_hex) = s_rest
                .strip_prefix("0x")
                .or_else(|| s_rest.strip_prefix("0X"))
                && !stripped_hex.is_empty()
                && let Some(bi) = BigInt::parse_bytes(stripped_hex.as_bytes(), 16)
            {
                let bi = if sign < 0 { -bi } else { bi };
                return Ok(vec![Value::from_bigint(bi)]);
            }
            if let Ok(i) = s_trimmed.parse::<i64>() {
                Ok(vec![Value::from_bigint(BigInt::from(i))])
            } else if let Ok(bi) = BigInt::from_str(s_trimmed) {
                Ok(vec![Value::from_bigint(bi)])
            } else if let Ok(f) = s_trimmed.parse::<f64>() {
                Ok(vec![Value::Float(f)])
            } else {
                Ok(vec![Value::Nil])
            }
        }
        _ => Ok(vec![Value::Nil]),
    }
}

pub fn builtin_int(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args.first().unwrap_or(&Value::Nil);
    match val {
        Value::Int(i) => Ok(vec![Value::Int(*i)]),
        Value::BigInt(_) => Ok(vec![val.clone()]),
        Value::Float(f) => Ok(vec![Value::from_bigint(
            BigInt::from_f64(f.trunc()).unwrap_or_default(),
        )]),
        Value::String(s) => {
            if s.len() > 65_536 {
                return Err("integer string exceeds maximum length limit (65536 bytes)".to_string());
            }
            let bi = BigInt::parse_bytes(s.as_bytes(), 10)
                .ok_or_else(|| "invalid integer string".to_string())?;
            Ok(vec![Value::from_bigint(bi)])
        }
        _ => Err("int expects number or string".to_string()),
    }
}

pub fn builtin_float(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args.first().unwrap_or(&Value::Nil);
    match val {
        Value::Float(f) => Ok(vec![Value::Float(*f)]),
        Value::Int(i) => Ok(vec![Value::Float(i.to_f64().unwrap_or(0.0))]),
        Value::String(s) => {
            let f = s
                .parse::<f64>()
                .map_err(|_| "invalid float string".to_string())?;
            Ok(vec![Value::Float(f)])
        }
        _ => Err("float expects number or string".to_string()),
    }
}

pub fn builtin_error(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let msg = args
        .first()
        .map(|v| v.to_string())
        .unwrap_or_else(|| "error".to_string());
    Err(msg)
}

pub fn builtin_pcall(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let func = args
        .first()
        .ok_or_else(|| "pcall expects at least 1 argument".to_string())?;
    match vm.call_function(func.clone(), &args[1..]) {
        Ok(mut res) => {
            res.insert(0, Value::Bool(true));
            Ok(res)
        }
        Err(err) => {
            if vm.limits.is_tripped() {
                return Err(err);
            }
            vm.error_trace = None;
            Ok(vec![Value::Bool(false), Value::string(err)])
        }
    }
}

/// `try(f, ...)` is `pcall` under the name the bundled libraries use: it
/// returns `true` followed by the results, or `false` and the error message.
pub fn builtin_try(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let func = args
        .first()
        .ok_or_else(|| "try expects a function".to_string())?;
    match vm.call_function(func.clone(), &args[1..]) {
        Ok(mut res) => {
            res.insert(0, Value::Bool(true));
            Ok(res)
        }
        Err(err) => {
            if vm.limits.is_tripped() {
                return Err(err);
            }
            vm.error_trace = None;
            Ok(vec![Value::Bool(false), Value::string(err)])
        }
    }
}

pub fn builtin_xpcall(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let func = args
        .first()
        .ok_or_else(|| "xpcall expects at least 2 arguments".to_string())?;
    let err_handler = args
        .get(1)
        .ok_or_else(|| "xpcall expects at least 2 arguments".to_string())?;
    let call_args = if args.len() > 2 { &args[2..] } else { &[] };
    match vm.call_function(func.clone(), call_args) {
        Ok(mut res) => {
            res.insert(0, Value::Bool(true));
            Ok(res)
        }
        Err(err) => {
            if vm.limits.is_tripped() {
                return Err(err);
            }
            vm.error_trace = None;
            let err_val = Value::string(err);
            match vm.call_function(err_handler.clone(), std::slice::from_ref(&err_val)) {
                Ok(h_res) => {
                    let ret = h_res.into_iter().next().unwrap_or(err_val);
                    Ok(vec![Value::Bool(false), ret])
                }
                Err(h_err) => {
                    if vm.limits.is_tripped() {
                        return Err(h_err);
                    }
                    Ok(vec![
                        Value::Bool(false),
                        Value::String((format!("error in error handling: {}", h_err)).into()),
                    ])
                }
            }
        }
    }
}

/// Resolves a standard or bundled `@neyuki/...` module (the prefix is
/// optional), or returns `None` when `name` is neither. Bundled sources run
/// once; every later `require` gets the same table.
fn load_bundled_module(vm: &mut VM, name: &str) -> Result<Option<Value>, String> {
    let bare = name.strip_prefix("@neyuki/").unwrap_or(name);
    if let Some(cached) = vm.modules.get(bare).cloned() {
        return Ok(Some(cached));
    }
    let Some((_, source)) = crate::vm::libs::BUNDLED_LIBRARIES
        .iter()
        .find(|(bundled, _)| bundled.strip_prefix("@neyuki/") == Some(bare))
    else {
        return Ok(None);
    };
    let proto = crate::compiler::compile_program(source, name, crate::compiler::Options::UNCHECKED)
        .map_err(|err| format!("error in module '{}': {}", name, err))?;
    let value = vm.execute_module(proto)?;
    vm.modules.insert(bare.to_string(), value.clone());
    Ok(Some(value))
}

pub fn builtin_require(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let pkg = match args
        .first()
        .ok_or_else(|| "require expects a module path".to_string())?
    {
        Value::String(s) => &**s,
        _ => return Err("require expects string argument".to_string()),
    };
    if let Some(val) = load_bundled_module(vm, pkg)? {
        return Ok(vec![val]);
    }
    let path_obj = std::path::Path::new(pkg);
    if pkg.contains('\0')
        || pkg.contains("..")
        || pkg.starts_with('/')
        || pkg.starts_with('\\')
        || path_obj.is_absolute()
        || path_obj.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "security error: path traversal forbidden in require: '{}'",
            pkg
        ));
    }

    let mut loader = crate::module_loader::ModuleLoader::new();
    if let Some(dir) = &vm.script_dir {
        loader = loader.with_base_dir(dir.clone());
    }
    let located = loader
        .locate_module(pkg)
        .map_err(|_| format!("cannot find module '{}'", pkg))?;
    let key = located.cache_key();
    if let Some(cached) = vm.modules.get(&key).cloned() {
        return Ok(vec![cached]);
    }
    if let crate::module_loader::Located::Disk(path) = &located {
        vm.permissions.check_read(&path.to_string_lossy())?;
    }
    let (resolved_path, bytes) = loader.read_located(&located)?;
    let proto = loader.compile_bytes(&resolved_path, &bytes, pkg)?;
    // While the module runs, a `require` cycle back into it gets an empty
    // table instead of running it again (docs/modules.md, §3.2).
    let pending = Value::Table(std::rc::Rc::new(std::cell::RefCell::new(
        crate::vm::value::VmTable::new(),
    )));
    vm.modules.insert(key.clone(), pending);
    // Run on top of the current frames: `execute` would reset the
    // stack out from under whatever called `require`.
    match vm.execute_module((*proto).clone()) {
        Ok(val) => {
            vm.modules.insert(key, val.clone());
            Ok(vec![val])
        }
        Err(err) => {
            // A failed load is not cached: the next `require` tries again.
            vm.modules.remove(&key);
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_checktype_and_asserttype() {
        let mut vm = VM::new();

        // checktype
        let res = builtin_checktype(&mut vm, &[Value::Int(42), Value::str("int")]).unwrap();
        assert_eq!(res, vec![Value::Bool(true)]);

        let res = builtin_checktype(&mut vm, &[Value::str("hello"), Value::str("int")]).unwrap();
        assert_eq!(res, vec![Value::Bool(false)]);

        let res =
            builtin_checktype(&mut vm, &[Value::str("hello"), Value::str("int | string")]).unwrap();
        assert_eq!(res, vec![Value::Bool(true)]);

        // void maps to Never, so it never matches
        let res = builtin_checktype(&mut vm, &[Value::Nil, Value::str("void")]).unwrap();
        assert_eq!(res, vec![Value::Bool(false)]);

        // unknown named type must return false, not true
        let res = builtin_checktype(&mut vm, &[Value::Int(42), Value::str("UnknownType")]).unwrap();
        assert_eq!(res, vec![Value::Bool(false)]);

        // asserttype success
        let res = builtin_asserttype(&mut vm, &[Value::Int(42), Value::str("int")]).unwrap();
        assert_eq!(res, vec![Value::Int(42)]);

        // asserttype failure
        let err =
            builtin_asserttype(&mut vm, &[Value::str("hello"), Value::str("int")]).unwrap_err();
        assert!(err.contains("type assertion failed"));
    }
}
