//! The global functions: print, assert, type, tostring, tonumber, error,
//! pcall, require and friends.

use num_bigint::BigInt;
use num_traits::{FromPrimitive, ToPrimitive};
use std::cell::RefCell;
use std::rc::Rc;
use std::str::FromStr;

use crate::bytecode::Proto;
use crate::vm::hash::FxHashMap;
use crate::vm::machine::VM;
use crate::vm::value::{StrRef, Value};

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

/// Interns static type names (`"nil"`, `"number"`, `"bigint"`, ...) once per
/// thread so `type(v)` and `typeof(v)` return a refcount bump instead of
/// allocating and hashing a fresh `String` on every call.
fn static_type_str(name: &'static str) -> Value {
    thread_local! {
        static CACHE: RefCell<FxHashMap<&'static str, StrRef>> = RefCell::new(FxHashMap::default());
    }
    CACHE.with(|cache| {
        let mut map = cache.borrow_mut();
        let s = map.entry(name).or_insert_with(|| StrRef::from(name));
        Value::String(s.clone())
    })
}

pub fn builtin_type(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args.first().unwrap_or(&Value::Nil);
    Ok(vec![static_type_str(val.type_name())])
}

pub fn builtin_typeof(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args.first().unwrap_or(&Value::Nil);
    Ok(vec![static_type_str(val.typeof_name())])
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
/// optional), or returns `None` when `name` is neither. Bundled sources are
/// compiled and verified once per thread as an immutable template; each VM
/// clones an isolated prototype tree with fresh `GetImport` caches so dropped
/// VMs never leak tables and alternating VMs never thrash each other's caches.
fn load_bundled_module(vm: &mut VM, name: &str) -> Result<Option<Value>, String> {
    let bare = name.strip_prefix("@neyuki/").unwrap_or(name);
    if let Some(cached) = vm.modules.get(bare).cloned() {
        return Ok(Some(cached));
    }
    let Some(&(bundled_name, source)) = crate::vm::libs::BUNDLED_LIBRARIES
        .iter()
        .find(|(bundled, _)| bundled.strip_prefix("@neyuki/") == Some(bare))
    else {
        return match crate::vm::libs::moved_library_error(name) {
            Some(moved) => Err(moved),
            None => Ok(None),
        };
    };
    thread_local! {
        static BUNDLED_PROTOS: RefCell<FxHashMap<&'static str, Proto>> =
            RefCell::new(FxHashMap::default());
    }
    let proto = BUNDLED_PROTOS.with(|cache| -> Result<Rc<Proto>, String> {
        let mut map = cache.borrow_mut();
        if let Some(template) = map.get(bundled_name) {
            return Ok(template.clone_isolated());
        }
        let compiled = crate::compiler::compile_program(
            source,
            bundled_name,
            crate::compiler::Options::UNCHECKED,
        )
        .map_err(|err| format!("error in module '{}': {}", name, err))?;
        crate::bytecode::verify_proto(&compiled).map_err(|e| e.to_string())?;
        compiled.mark_verified();
        compiled.warm_values();
        let isolated = compiled.clone_isolated();
        map.insert(bundled_name, compiled);
        Ok(isolated)
    })?;
    let value = vm.execute_module_rc(proto)?;
    vm.modules.insert(bare.to_string(), value.clone());
    Ok(Some(value))
}

/// Loads a package's native extension library (`[native]` in its
/// manifest) and wraps it like `ffi.loadExtension` does. Native code runs
/// with the process's full rights, so this needs `--allow-ffi` (a
/// restricted allow-list must contain the library) when sandboxed.
fn load_native_package(
    vm: &mut VM,
    package: &str,
    file: &crate::module_loader::Located,
) -> Result<Value, String> {
    use crate::module_loader::Located;
    let path = match file {
        Located::Disk(path) => path.clone(),
        Located::Bundled(path) => {
            // The OS loader cannot open a file inside the executable, so a
            // built app writes its libraries out once (see pkg::native).
            let bytes = crate::app::read(path)
                .ok_or_else(|| format!("cannot find {}'s native library '{}'", package, path))?
                .map_err(|e| format!("cannot read {}'s native library: {}", package, e))?;
            crate::pkg::native::extract_bundled_library(
                crate::pkg::native::file_name(path),
                &bytes,
            )?
        }
        Located::Native { .. } => return Err("nested native library".to_string()),
    };
    let shown = path.to_string_lossy().into_owned();
    let resolved = vm
        .permissions
        .resolve_ffi_library(&shown)
        .map_err(|e| format!("cannot load {}'s native library: {}", package, e))?;
    let (id, fn_map) = crate::vm::libs::ffi::ext::load_extension(vm, &resolved, &shown)
        .map_err(|e| format!("cannot load {}'s native library: {}", package, e))?;
    let ffi = load_bundled_module(vm, "@neyuki/ffi")?
        .ok_or_else(|| "@neyuki/ffi is not available".to_string())?;
    let bind = match &ffi {
        Value::Table(t) => t.borrow().get_str("_bindExtension"),
        _ => Value::Nil,
    };
    let fn_map = Value::Table(std::rc::Rc::new(std::cell::RefCell::new(fn_map)));
    let mut out = vm.call_function(bind, &[Value::from_usize(id as usize), fn_map])?;
    Ok(if out.is_empty() {
        Value::Nil
    } else {
        out.swap_remove(0)
    })
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
    // `@pkg/user/name` (project) and `@user/name` (global) are registry
    // packages; everything else is a file next to the script.
    let located = match crate::module_loader::packages::parse(pkg) {
        Some(request) => {
            crate::module_loader::packages::locate(&request?, vm.script_dir.as_deref())?
        }
        None => loader
            .locate_module(pkg)
            .map_err(|_| format!("cannot find module '{}'", pkg))?,
    };
    let key = located.cache_key();
    // Soundness invariant: `vm.modules` starts empty, `vm.permissions` is fixed
    // for the lifetime of the VM, and entries are only cached below after passing
    // `Located::Disk` `check_read` (or coming from bundled/trusted memory modules).
    if let Some(cached) = vm.modules.get(&key).cloned() {
        return Ok(vec![cached]);
    }
    if let crate::module_loader::Located::Native { package, file } = &located {
        let value = load_native_package(vm, package, file)?;
        vm.modules.insert(key, value.clone());
        return Ok(vec![value]);
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
    match vm.execute_module_rc(proto) {
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

    #[test]
    fn test_bundled_cache_does_not_leak_vm_tables_on_drop() {
        // 1. Acyclic bundled module (`@neyuki/crypto`): dropping the VM alone
        // must immediately release both the module table and stdlib tables.
        let crypto_proto = crate::compiler::compile_program(
            r#"
                local crypto = require("@neyuki/crypto")
                return crypto.hash("abc", "sha256")
            "#,
            "leak_crypto",
            crate::compiler::Options::RUN,
        )
        .expect("compile");
        let (weak_crypto_table, weak_crypto_mod) = {
            let mut vm = VM::new();
            let weak_table = match vm.globals.get("table") {
                Some(Value::Table(t)) => Rc::downgrade(t),
                _ => panic!("missing table global"),
            };
            let _ = vm.execute(crypto_proto).expect("exec");
            let weak_mod = match vm.modules.get("crypto") {
                Some(Value::Table(t)) => Rc::downgrade(t),
                _ => panic!("missing crypto module"),
            };
            (weak_table, weak_mod)
        };
        assert!(weak_crypto_table.upgrade().is_none());
        assert!(weak_crypto_mod.upgrade().is_none());

        // 2. Bundled module executing `GetImport` against frozen global `debug`
        // (`@neyuki/profile` caches `debug.sethook` / `debug.getinfo` in its
        // `ProtoCache::imports`). Once the VM and module's own closure cycle
        // are dropped, the thread-local `BUNDLED_PROTOS` cache must not retain
        // the dropped VM's `debug` table.
        let profile_proto = crate::compiler::compile_program(
            r#"
                local profile = require("@neyuki/profile")
                local function work(n)
                    if n <= 0 then return 0 end
                    return work(n - 1)
                end
                profile.start()
                work(2)
                return profile.stop()
            "#,
            "leak_profile",
            crate::compiler::Options::RUN,
        )
        .expect("compile");
        let weak_debug = {
            let mut vm = VM::new();
            let weak_debug = match vm.globals.get("debug") {
                Some(Value::Table(t)) => Rc::downgrade(t),
                _ => panic!("missing debug global"),
            };
            let out = vm.execute(profile_proto).expect("exec");
            assert!(out.to_string().contains("work"));
            // Break `profile`'s internal table<->closure cycle (`tracer`
            // captures `profile`); any remaining strong ref to `debug` would
            // come from a leaked `ImportSlot` in `BUNDLED_PROTOS`.
            if let Some(Value::Table(profile_tbl)) = vm.modules.get("profile") {
                profile_tbl.borrow_mut().fields.clear();
            }
            weak_debug
        };
        assert!(
            weak_debug.upgrade().is_none(),
            "bundled proto cache retained dropped VM's `debug` table"
        );
    }

    #[test]
    fn test_bundled_cache_isolates_getimport_and_prevents_thrashing() {
        // Each VM replaces global `debug` with its own frozen table so
        // `GetImport` inside `@neyuki/profile` caches `debug.sethook` per VM.
        // Alternating calls across VMs must hit each VM's own cached hook
        // without leaking across VMs or overwriting the other VM's slots.
        let setup = |delta: i64| {
            let code = format!(
                r#"
                local hits = 0
                debug = table.freeze({{
                    sethook = function() hits += {delta} end,
                    getinfo = function() return nil end,
                }})
                local profile = require("@neyuki/profile")
                return function()
                    profile.start()
                    profile.stop()
                    return hits
                end
            "#
            );
            let proto =
                crate::compiler::compile_program(&code, "iso", crate::compiler::Options::RUN)
                    .expect("compile");
            let mut vm = VM::new();
            let runner = vm.execute(proto).expect("exec");
            (vm, runner)
        };

        let (mut vm1, run1) = setup(1);
        let (mut vm2, run2) = setup(10);
        assert_eq!(
            vm1.call_function(run1.clone(), &[]).unwrap(),
            vec![Value::Int(2)]
        );
        assert_eq!(
            vm2.call_function(run2.clone(), &[]).unwrap(),
            vec![Value::Int(20)]
        );
        assert_eq!(
            vm1.call_function(run1.clone(), &[]).unwrap(),
            vec![Value::Int(4)]
        );
        assert_eq!(vm2.call_function(run2, &[]).unwrap(), vec![Value::Int(40)]);
        assert_eq!(vm1.call_function(run1, &[]).unwrap(), vec![Value::Int(6)]);

        // Verify the two VMs' `profile.start` closures own distinct `Proto`s
        // (and thus unshared `ImportSlot` vectors).
        let proto_ptr = |vm: &VM| -> *const Proto {
            let Value::Table(t) = vm.modules.get("profile").expect("profile module") else {
                panic!("profile not a table");
            };
            let Value::Closure(c) = t.borrow().fields.get("start").expect("start fn").clone()
            else {
                panic!("start not a closure");
            };
            Rc::as_ptr(&c.proto)
        };
        assert_ne!(proto_ptr(&vm1), proto_ptr(&vm2));
    }

    #[test]
    fn test_vm_templates_and_bundled_cache_are_isolated_across_vms() {
        let code = r#"
            local crypto = require("@neyuki/crypto")
            setmetatable(math, { __index = function() return 99 end })
            return crypto.hash("abc", "sha256")
        "#;
        let proto = crate::compiler::compile_program(code, "test", crate::compiler::Options::RUN)
            .expect("compile");

        let mut vm1 = VM::new();
        let h1 = vm1.execute(proto).expect("vm1 exec");

        let check_code = r#"
            local crypto = require("@neyuki/crypto")
            assert(getmetatable(math) == nil, "metatable leaked across VMs")
            return crypto.hash("abc", "sha256")
        "#;
        let check_proto =
            crate::compiler::compile_program(check_code, "test2", crate::compiler::Options::RUN)
                .expect("compile2");
        let mut vm2 = VM::new();
        let h2 = vm2.execute(check_proto).expect("vm2 exec");

        assert_eq!(h1, h2);
        assert_eq!(
            h1.to_string(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
