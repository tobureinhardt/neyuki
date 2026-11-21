//! Metamethod lookups, metamethod dispatch, and table/buffer indexing paths.

use crate::vm::machine::VM;
use crate::vm::value::Value;

impl VM {
    pub(crate) fn get_binop_metamethod(&self, a: &Value, b: &Value, event: &str) -> Option<Value> {
        if let Value::Table(t) = a
            && let Some(h) = t.borrow().metamethod(event)
        {
            return Some(h);
        }
        if let Value::Table(t) = b
            && let Some(h) = t.borrow().metamethod(event)
        {
            return Some(h);
        }
        None
    }

    pub(crate) fn get_unop_metamethod(&self, val: &Value, event: &str) -> Option<Value> {
        if let Value::Table(t) = val {
            return t.borrow().metamethod(event);
        }
        None
    }

    /// Calls a binary metamethod and stores its first result in `dst`.
    pub(crate) fn call_binop_mm(
        &mut self,
        mm: Value,
        va: Value,
        vb: Value,
        base: usize,
        dst: u8,
    ) -> Result<(), String> {
        let res = self.call_function(mm, &[va, vb])?;
        self.set_reg_grow(
            base + dst as usize,
            res.into_iter().next().unwrap_or(Value::Nil),
        );
        Ok(())
    }

    /// Calls a comparison metamethod and reports whether its result is truthy.
    pub(crate) fn call_cmp_mm(&mut self, mm: Value, va: Value, vb: Value) -> Result<bool, String> {
        let res = self.call_function(mm, &[va, vb])?;
        Ok(res.first().is_some_and(|v| v.is_truthy()))
    }

    /// Raw lookup of an existing, non-nil entry. `None` means the slow path
    /// (metamethods, buffers, strings, errors) has to decide.
    #[inline]
    pub(crate) fn table_get_fast(table: &Value, key: &Value) -> Option<Value> {
        let Value::Table(t) = table else {
            return None;
        };
        let tbl = t.borrow();
        let found = match key {
            Value::Int(i) if *i >= 1 && (*i as u64) <= tbl.array.len() as u64 => {
                tbl.array[*i as usize - 1].clone()
            }
            Value::String(k) => tbl.fields.get_by_strref(k).cloned().unwrap_or(Value::Nil),
            Value::BigInt(_) => return None,
            other => tbl.raw_get(other),
        };
        if matches!(found, Value::Nil) && tbl.metatable.is_some() {
            return None;
        }
        Some(found)
    }

    /// Raw store into an existing array slot or field, or an append, on a
    /// table that nothing can intercept. Returns false to defer to the slow
    /// path, which also handles nil (removal) and reports errors.
    #[inline]
    pub(crate) fn table_set_fast(table: &Value, key: &Value, val: &Value) -> bool {
        let Value::Table(t) = table else {
            return false;
        };
        if matches!(val, Value::Nil) {
            return false;
        }
        let mut tbl = t.borrow_mut();
        if tbl.frozen {
            return false;
        }
        match key {
            Value::Int(i) if *i >= 1 => {
                let idx = *i as usize - 1;
                if idx < tbl.array.len() {
                    match (val, &mut tbl.array[idx]) {
                        (Value::Int(n), Value::Int(o)) => *o = *n,
                        (Value::Float(n), Value::Float(o)) => *o = *n,
                        (Value::Bool(n), Value::Bool(o)) => *o = *n,
                        (v, slot) => *slot = v.clone(),
                    }
                    true
                } else if idx == tbl.array.len() && tbl.metatable.is_none() && tbl.general.is_none()
                {
                    tbl.array.push(val.clone());
                    true
                } else {
                    false
                }
            }
            Value::String(k) => {
                if tbl.metatable.is_none() {
                    tbl.fields.set_by_strref(k, val);
                    true
                } else if let Some(slot) = tbl.fields.get_mut_by_strref(k) {
                    *slot = val.clone();
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    pub fn table_get(&mut self, table: &Value, key: &Value) -> Result<Value, String> {
        self.table_get_depth(table, key, 0)
    }

    fn table_get_depth(
        &mut self,
        table: &Value,
        key: &Value,
        depth: usize,
    ) -> Result<Value, String> {
        if depth > 100 {
            return Err("loop in gettable / __index chain".to_string());
        }
        match table {
            Value::Table(t) => {
                if let Value::BigInt(b) = key
                    && num_traits::Signed::is_positive(&**b)
                {
                    return Err("table index too large".to_string());
                }
                let val = t.borrow().raw_get(key);
                if !matches!(val, Value::Nil) {
                    return Ok(val);
                }

                // Check __index metamethod
                let index_handler = t.borrow().metamethod("__index");
                match index_handler {
                    Some(handler @ Value::Table(_)) => {
                        return self.table_get_depth(&handler, key, depth + 1);
                    }
                    Some(handler @ (Value::Native(_) | Value::Closure(_))) => {
                        let res = self.call_function(handler, &[table.clone(), key.clone()])?;
                        return Ok(res.into_iter().next().unwrap_or(Value::Nil));
                    }
                    _ => {}
                }
                Ok(Value::Nil)
            }
            Value::Buffer(b) => {
                let buf = b.borrow();
                match key {
                    Value::Int(idx) => {
                        let i = usize::try_from(*idx)
                            .map_err(|_| "buffer index too large".to_string())?;
                        let val = buf.read_u8(i)?;
                        Ok(Value::Int(val as i64))
                    }
                    Value::BigInt(_) => Err("buffer index too large".to_string()),
                    _ => Ok(Value::Nil),
                }
            }
            // Strings index the string library, so `s:upper()` works.
            Value::String(_) => match (&self.string_lib, key) {
                (Value::Table(lib), Value::String(name)) => Ok(lib
                    .borrow()
                    .fields
                    .get_by_strref(name)
                    .cloned()
                    .unwrap_or(Value::Nil)),
                _ => Ok(Value::Nil),
            },
            other => Err(format!("attempted to index a {} value", other.type_name())),
        }
    }

    pub fn table_set(&mut self, table: &Value, key: Value, val: Value) -> Result<(), String> {
        self.table_set_depth(table, key, val, 0)
    }

    fn table_set_depth(
        &mut self,
        table: &Value,
        key: Value,
        val: Value,
        depth: usize,
    ) -> Result<(), String> {
        if depth > 100 {
            return Err("loop in settable / __newindex chain".to_string());
        }
        self.gc.write_barrier(table, &val);
        match table {
            Value::Table(t) => {
                if t.borrow().frozen {
                    return Err("attempted to mutate a frozen table".to_string());
                }

                if let Value::BigInt(b) = &key
                    && num_traits::Signed::is_positive(&**b)
                {
                    return Err("table index too large".to_string());
                }
                let key_exists = !matches!(t.borrow().raw_get(&key), Value::Nil);
                if !key_exists {
                    let newindex_handler = t.borrow().metamethod("__newindex");
                    match newindex_handler {
                        Some(handler @ Value::Table(_)) => {
                            return self.table_set_depth(&handler, key, val, depth + 1);
                        }
                        Some(handler @ (Value::Native(_) | Value::Closure(_))) => {
                            self.call_function(handler, &[table.clone(), key, val])?;
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                t.borrow_mut().raw_set(key, val)
            }
            Value::Buffer(b) => {
                let mut buf = b.borrow_mut();
                match key {
                    Value::Int(idx) => {
                        let i = usize::try_from(idx)
                            .map_err(|_| "buffer index too large".to_string())?;
                        let byte_val = match val {
                            Value::Int(v) => u8::try_from(v)
                                .map_err(|_| "value out of byte range".to_string())?,
                            Value::Float(f) => f as u8,
                            _ => return Err("buffer value expects byte".to_string()),
                        };
                        buf.write_u8(i, byte_val)?;
                        Ok(())
                    }
                    Value::BigInt(_) => Err("buffer index too large".to_string()),
                    _ => Err("invalid buffer index".to_string()),
                }
            }
            other => Err(format!(
                "attempted to set a field on a {} value",
                other.type_name()
            )),
        }
    }
}
