//! Metatables and raw access: setmetatable, getmetatable, rawget, rawset,
//! rawequal, rawlen.

use crate::vm::machine::VM;
use crate::vm::value::{BorrowedRc, Value, ValueView};

pub fn builtin_setmetatable(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let tbl_val = args
        .first()
        .ok_or_else(|| "setmetatable expects table as first argument".to_string())?;
    let mt_val = args
        .get(1)
        .ok_or_else(|| "setmetatable expects 2 arguments".to_string())?;
    let Some(t) = tbl_val.as_table() else {
        return Err("setmetatable expects a table as first argument".to_string());
    };
    if let Some(existing_mt) = &t.borrow().metatable
        && let Some(metaname) = existing_mt.borrow().fields.get("__metatable")
        && !metaname.is_nil()
    {
        return Err("cannot change a protected metatable".to_string());
    }
    let mt = match mt_val.view() {
        ValueView::Nil => None,
        ValueView::Table(_) => Some(mt_val.clone()),
        _ => return Err("metatable must be a table or nil".to_string()),
    };
    vm.table_set_metatable(tbl_val, mt)?;
    Ok(vec![tbl_val.clone()])
}

pub fn builtin_getmetatable(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let tbl_val = args
        .first()
        .ok_or_else(|| "getmetatable expects 1 argument".to_string())?;
    match tbl_val.as_table() {
        Some(t) => {
            if let Some(mt) = &t.borrow().metatable {
                if let Some(metaname) = mt.borrow().fields.get("__metatable")
                    && !metaname.is_nil()
                {
                    return Ok(vec![metaname.clone()]);
                }
                Ok(vec![Value::table(mt.clone())])
            } else {
                Ok(vec![Value::NIL])
            }
        }
        None => Ok(vec![Value::NIL]),
    }
}

pub fn builtin_rawget(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(t) = args.first().and_then(|v| v.as_table()) else {
        return Err("rawget expects a table".to_string());
    };
    let nil = Value::NIL;
    let key = args.get(1).unwrap_or(&nil);
    Ok(vec![t.borrow().raw_get(key)])
}

pub fn builtin_rawset(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let nil = Value::NIL;
    let table = args.first().unwrap_or(&nil);
    let Some(t) = table.as_table() else {
        return Err("rawset expects a table".to_string());
    };
    let key = args.get(1).cloned().unwrap_or(Value::NIL);
    let val = args.get(2).cloned().unwrap_or(Value::NIL);
    if t.borrow().frozen {
        return Err("attempted to mutate a frozen table".to_string());
    }
    if key.is_bigint() {
        return Err("table index too large".to_string());
    }
    vm.gc.write_barrier(table, &val);
    t.borrow_mut().raw_set(key, val)?;
    Ok(vec![table.clone()])
}

pub fn builtin_rawequal(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let nil = Value::NIL;
    let a = args.first().unwrap_or(&nil);
    let b = args.get(1).unwrap_or(&nil);
    let eq = match (a.view(), b.view()) {
        (ValueView::Nil, ValueView::Nil) => true,
        (ValueView::Bool(x), ValueView::Bool(y)) => x == y,
        (ValueView::Int(x), ValueView::Int(y)) => x == y,
        (ValueView::BigInt(x), ValueView::BigInt(y)) => x == y,
        (ValueView::Float(x), ValueView::Float(y)) => x == y,
        (ValueView::String(x), ValueView::String(y)) => x == y,
        (ValueView::Table(x), ValueView::Table(y)) => BorrowedRc::ptr_eq(x, y),
        (ValueView::Buffer(x), ValueView::Buffer(y)) => BorrowedRc::ptr_eq(x, y),
        (ValueView::Closure(x), ValueView::Closure(y)) => BorrowedRc::ptr_eq(x, y),
        (ValueView::Native(na), ValueView::Native(nb)) => na.name == nb.name,
        _ => false,
    };
    Ok(vec![Value::bool(eq)])
}

pub fn builtin_rawlen(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args
        .first()
        .ok_or_else(|| "rawlen expects 1 argument".to_string())?;
    match val.view() {
        ValueView::Table(t) => Ok(vec![Value::from_usize(t.borrow().array.len())]),
        ValueView::String(s) => Ok(vec![Value::from_usize(s.len())]),
        ValueView::Buffer(b) => Ok(vec![Value::from_usize(b.borrow().len())]),
        _ => Err("rawlen expects table, string, or buffer".to_string()),
    }
}
