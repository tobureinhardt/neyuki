//! The `debug` library: stack inspection, locals and upvalues, hooks and
//! tracebacks.
//!
//! Levels count stack frames from the function that called the debug
//! function (level 1) outwards. Native functions have no frames of their
//! own.

use std::cell::RefCell;
use std::rc::Rc;

use crate::bytecode::proto::{LocalVarInfo, Proto};
use crate::vm::frame::CallFrame;
use crate::vm::machine::VM;
use crate::vm::value::{Value, VmTable};

pub fn create_debug_lib() -> Value {
    let mut table = VmTable::new();
    macro_rules! reg {
        ($($name:literal => $func:ident),* $(,)?) => {
            $(table.set_str($name, crate::native!(concat!("debug.", $name), $func));)*
        };
    }
    reg! {
        "getinfo" => debug_getinfo,
        "getlocal" => debug_getlocal,
        "getmetatable" => debug_getmetatable,
        "gethook" => debug_gethook,
        "getupvalue" => debug_getupvalue,
        "setlocal" => debug_setlocal,
        "setmetatable" => debug_setmetatable,
        "sethook" => debug_sethook,
        "setupvalue" => debug_setupvalue,
        "traceback" => debug_traceback,
        "upvalueid" => debug_upvalueid,
    }
    crate::vm::libs::args::frozen(table)
}

/// A non-negative level or index argument.
fn count_arg(args: &[Value], index: usize, name: &str, default: i64) -> Result<i64, String> {
    match args.get(index).map(crate::vm::value::Value::view) {
        None | Some(crate::vm::value::ValueView::Nil) => Ok(default),
        Some(crate::vm::value::ValueView::Int(i)) => Ok(i),
        Some(crate::vm::value::ValueView::Float(f)) if f.fract() == 0.0 => Ok(f as i64),
        Some(_) => Err(format!(
            "{} must be an integer, got {}",
            name,
            args[index].type_name()
        )),
    }
}

/// The frame `level` steps out from the caller of the debug function.
fn frame_at(vm: &VM, level: i64) -> Option<&CallFrame> {
    if level < 1 {
        return None;
    }
    let level = level as usize;
    vm.frames.len().checked_sub(level).map(|i| &vm.frames[i])
}

/// The instruction a frame is executing (its ip points past it).
fn current_pc(frame: &CallFrame) -> usize {
    frame.ip.saturating_sub(1)
}

fn current_line(frame: &CallFrame) -> i64 {
    let proto = &frame.closure.proto;
    proto
        .lines
        .get(current_pc(frame))
        .map(|l| *l as i64)
        .unwrap_or(-1)
}

/// `debug.traceback(message?, level?)`: the message followed by the stack,
/// one `file:line in function` entry per frame.
fn debug_traceback(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let message = match args.first() {
        None => None,
        Some(v) if v.is_nil() => None,
        Some(v) if v.is_string() => Some(v.as_str().unwrap().to_string()),
        // A non-string message is passed through untouched, as in Lua.
        Some(other) => return Ok(vec![other.clone()]),
    };
    let level = count_arg(args, 1, "level", 1)?.max(1) as usize;
    let mut out = message.map(|m| format!("{}\n", m)).unwrap_or_default();
    out.push_str("stack traceback:");
    for frame in vm.capture_trace().into_iter().skip(level - 1) {
        out.push_str("\n    ");
        if frame.source.is_none() && frame.line == 0 {
            out.push_str(frame.function.as_deref().unwrap_or(""));
        } else {
            out.push_str(&frame.to_string());
        }
    }
    Ok(vec![Value::string(out)])
}

fn proto_info(table: &mut VmTable, proto: &Proto) {
    let name = proto.name.as_deref();
    table.set_str(
        "name",
        match name {
            Some("main") | None => Value::NIL,
            Some(n) => Value::str(n),
        },
    );
    table.set_str(
        "what",
        Value::str(if name == Some("main") {
            "main"
        } else {
            "Neyuki"
        }),
    );
    let source = proto.source.as_deref().unwrap_or("?");
    table.set_str("source", Value::str(source));
    table.set_str(
        "short_src",
        Value::str(source.rsplit(['/', '\\']).next().unwrap_or(source)),
    );
    let first = proto.lines.iter().copied().filter(|l| *l > 0).min();
    let last = proto.lines.iter().copied().max();
    table.set_str("linedefined", Value::int(first.unwrap_or(0) as i64));
    table.set_str("lastlinedefined", Value::int(last.unwrap_or(0) as i64));
    table.set_str("numparams", Value::int(proto.num_params as i64));
    table.set_str("isvararg", Value::bool(proto.is_vararg));
    table.set_str("nups", Value::int(proto.upvalues.len() as i64));
}

fn native_info(table: &mut VmTable, name: &str) {
    table.set_str("name", Value::str(name));
    table.set_str("what", Value::str("native"));
    table.set_str("source", Value::str("=[native]"));
    table.set_str("short_src", Value::str("[native]"));
    table.set_str("linedefined", Value::int(-1));
    table.set_str("lastlinedefined", Value::int(-1));
    table.set_str("currentline", Value::int(-1));
    table.set_str("numparams", Value::int(0));
    table.set_str("isvararg", Value::TRUE);
    table.set_str("nups", Value::int(0));
}

/// `debug.getinfo(level | function)`: a table describing a running frame or
/// a function: name, what, source, short_src, linedefined, lastlinedefined,
/// currentline (frames only), numparams, isvararg, nups and func.
fn debug_getinfo(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut table = VmTable::new();
    match args.first().map(crate::vm::value::Value::view) {
        Some(crate::vm::value::ValueView::Closure(c)) => {
            proto_info(&mut table, &c.proto);
            table.set_str("currentline", Value::int(-1));
        }
        Some(crate::vm::value::ValueView::Native(def)) => native_info(&mut table, def.name),
        Some(crate::vm::value::ValueView::Int(_) | crate::vm::value::ValueView::Float(_)) => {
            let level = count_arg(args, 0, "level", 1)?;
            if level == 0 {
                native_info(&mut table, "debug.getinfo");
            } else {
                let Some(frame) = frame_at(vm, level) else {
                    return Ok(vec![Value::NIL]);
                };
                proto_info(&mut table, &frame.closure.proto);
                table.set_str("currentline", Value::int(current_line(frame)));
                table.set_str("func", Value::closure(frame.closure.clone()));
            }
        }
        _ => return Err("getinfo expects a level or a function".to_string()),
    }
    if let Some(f) = args.first().filter(|v| v.is_callable()) {
        table.set_str("func", f.clone());
    }
    Ok(vec![Value::table(Rc::new(RefCell::new(table)))])
}

/// The locals in scope at the frame's current instruction, in declaration
/// order.
fn active_locals(frame: &CallFrame) -> Vec<&LocalVarInfo> {
    let pc = current_pc(frame) as u32;
    frame
        .closure
        .proto
        .local_names
        .iter()
        .filter(|info| info.from_pc <= pc && pc < info.to_pc)
        .collect()
}

/// `debug.getlocal(level, n)`: the name and value of the `n`th local in
/// scope; a negative `n` reads the `-n`th vararg. `debug.getlocal(f, n)`
/// names a function's `n`th parameter.
fn debug_getlocal(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let n = count_arg(args, 1, "n", 1)?;
    if let Some(c) = args.first().and_then(Value::as_closure) {
        let name = c
            .proto
            .local_names
            .iter()
            .filter(|info| info.from_pc == 0 && (info.reg as i64) < c.proto.num_params as i64)
            .nth((n - 1).max(0) as usize)
            .filter(|_| n >= 1)
            .map(|info| Value::str(&info.name));
        return Ok(vec![name.unwrap_or(Value::NIL)]);
    }
    let level = count_arg(args, 0, "level", 1)?;
    let Some(frame) = frame_at(vm, level) else {
        return Err("level out of range".to_string());
    };
    if n < 0 {
        let index = (-n - 1) as usize;
        return Ok(match frame.varargs().get(index) {
            Some(v) => vec![Value::str("(vararg)"), v.clone()],
            None => vec![Value::NIL],
        });
    }
    let locals = active_locals(frame);
    let Some(info) = (n >= 1).then(|| locals.get(n as usize - 1)).flatten() else {
        return Ok(vec![Value::NIL]);
    };
    let value = vm
        .stack
        .get(frame.base + info.reg as usize)
        .cloned()
        .unwrap_or(Value::NIL);
    Ok(vec![Value::str(&info.name), value])
}

/// `debug.setlocal(level, n, value)`: assigns the `n`th local in scope and
/// returns its name, or nil when there is no such local.
fn debug_setlocal(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let level = count_arg(args, 0, "level", 1)?;
    let n = count_arg(args, 1, "n", 1)?;
    let value = args.get(2).cloned().unwrap_or(Value::NIL);
    let Some(frame) = frame_at(vm, level) else {
        return Err("level out of range".to_string());
    };
    if n < 0 {
        let index = (-n - 1) as usize;
        let at = vm.frames.len() - level as usize;
        return Ok(match vm.frames[at].varargs_mut().get_mut(index) {
            Some(slot) => {
                *slot = value;
                vec![Value::str("(vararg)")]
            }
            None => vec![Value::NIL],
        });
    }
    let target = active_locals(frame)
        .get((n - 1).max(0) as usize)
        .filter(|_| n >= 1)
        .map(|info| (info.name.clone(), frame.base + info.reg as usize));
    let Some((name, slot)) = target else {
        return Ok(vec![Value::NIL]);
    };
    if slot >= vm.stack.len() {
        vm.stack.resize(slot + 1, Value::NIL);
    }
    vm.stack[slot] = value;
    Ok(vec![Value::string(name)])
}

fn upvalue_name(c: &crate::vm::value::VmClosure, index: usize) -> Value {
    match c.proto.upvalue_names.get(index) {
        Some(name) if !name.is_empty() => Value::str(name),
        _ => Value::str("?"),
    }
}

/// `debug.getupvalue(f, n)`: the name and value of `f`'s `n`th upvalue.
fn debug_getupvalue(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(c) = args.first().and_then(Value::as_closure) else {
        return Ok(vec![Value::NIL]);
    };
    let n = count_arg(args, 1, "n", 1)?;
    if n < 1 || n as usize > c.upvalues.len() {
        return Ok(vec![Value::NIL]);
    }
    let value = vm.upvalue_get(&c.upvalues[n as usize - 1]);
    Ok(vec![upvalue_name(&c, n as usize - 1), value])
}

/// `debug.setupvalue(f, n, value)`: assigns `f`'s `n`th upvalue and returns
/// its name.
fn debug_setupvalue(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(c) = args.first().and_then(Value::as_closure) else {
        return Ok(vec![Value::NIL]);
    };
    let n = count_arg(args, 1, "n", 1)?;
    if n < 1 || n as usize > c.upvalues.len() {
        return Ok(vec![Value::NIL]);
    }
    let value = args.get(2).cloned().unwrap_or(Value::NIL);
    let uv = c.upvalues[n as usize - 1].clone();
    vm.upvalue_set(&uv, value);
    Ok(vec![upvalue_name(&c, n as usize - 1)])
}

/// `debug.upvalueid(f, n)`: a number identifying the variable behind `f`'s
/// `n`th upvalue, equal for closures that share it.
fn debug_upvalueid(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(c) = args.first().and_then(Value::as_closure) else {
        return Err("upvalueid expects a function".to_string());
    };
    let n = count_arg(args, 1, "n", 1)?;
    match (n >= 1).then(|| c.upvalues.get(n as usize - 1)).flatten() {
        Some(uv) => Ok(vec![Value::int(Rc::as_ptr(uv) as i64)]),
        None => Ok(vec![Value::NIL]),
    }
}

/// `debug.getmetatable(t)`, ignoring a `__metatable` field.
fn debug_getmetatable(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match args.first().and_then(Value::as_table) {
        Some(t) => t
            .borrow()
            .metatable
            .clone()
            .map(Value::table)
            .unwrap_or(Value::NIL),
        _ => Value::NIL,
    }])
}

/// `debug.setmetatable(t, mt)`, ignoring a `__metatable` field.
fn debug_setmetatable(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(t) = args.first().and_then(Value::as_table) else {
        return Err("setmetatable expects a table".to_string());
    };
    let mt = match args.get(1) {
        None => None,
        Some(v) if v.is_nil() => None,
        Some(v) if v.is_table() => Some(v.as_table().unwrap().to_rc()),
        Some(other) => {
            return Err(format!(
                "metatable must be a table or nil, got {}",
                other.type_name()
            ));
        }
    };
    let gc_handler = mt
        .as_ref()
        .is_some_and(|mt| mt.borrow().get_str("__gc").is_callable());
    t.borrow_mut().metatable = mt;
    let t_rc = t.to_rc();
    if gc_handler {
        vm.gc.pin_table(&t_rc);
    } else {
        vm.gc.unpin_table(&t_rc);
    }
    if crate::vm::gc::table_weak_values(&t_rc) {
        vm.gc.has_weak_tables = true;
    }
    Ok(vec![args[0].clone()])
}

/// `debug.sethook(hook, mask?, count?)`, or `debug.sethook()` to remove it.
/// The mask holds `c` (calls, including tail calls), `r` (returns) and `l`
/// (each new line, and each jump back into a loop); with a count the hook
/// also gets a "count" event every `count` instructions. The hook receives
/// the event name, and the line number for line events. Hooks never trigger
/// hooks.
fn debug_sethook(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let func = args.first().cloned().unwrap_or(Value::NIL);
    if func.is_nil() {
        vm.hook = None;
        vm.update_trap();
        return Ok(Vec::new());
    }
    if !func.is_callable() {
        return Err(format!("hook must be a function, got {}", func.type_name()));
    }
    let mask = match args.get(1) {
        // Without a mask or count, report calls and returns.
        None if args.get(2).is_none() => "cr",
        Some(v) if v.is_nil() && args.get(2).is_none() => "cr",
        None => "",
        Some(v) if v.is_nil() => "",
        Some(v) if v.is_string() => v.as_str().unwrap(),
        Some(other) => {
            return Err(format!("mask must be a string, got {}", other.type_name()));
        }
    };
    let (mut on_call, mut on_return, mut on_line) = (false, false, false);
    for ch in mask.chars() {
        match ch {
            'c' => on_call = true,
            'r' => on_return = true,
            'l' => on_line = true,
            _ => return Err(format!("invalid hook mask character '{}'", ch)),
        }
    }
    let count = count_arg(args, 2, "count", 0)?.clamp(0, u32::MAX as i64) as u32;
    vm.hook = Some(crate::vm::machine::DebugHook {
        func,
        on_call,
        on_return,
        on_line,
        count,
    });
    vm.hook_countdown = count;
    // The line that called sethook has already started.
    if let Some(frame) = vm.frames.last_mut() {
        let pc = current_pc(frame);
        frame.hook_line = frame.closure.proto.lines.get(pc).copied().unwrap_or(0);
        frame.hook_ip = pc;
    }
    vm.update_trap();
    Ok(Vec::new())
}

/// `debug.gethook()`: the hook function, its mask and its count.
fn debug_gethook(vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(hook) = &vm.hook else {
        return Ok(vec![Value::NIL]);
    };
    let mut mask = String::new();
    if hook.on_call {
        mask.push('c');
    }
    if hook.on_return {
        mask.push('r');
    }
    if hook.on_line {
        mask.push('l');
    }
    Ok(vec![
        hook.func.clone(),
        Value::string(mask),
        Value::int(hook.count as i64),
    ])
}
