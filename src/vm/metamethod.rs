//! Metamethod lookups, metamethod dispatch, and table/buffer indexing paths.

use crate::vm::machine::VM;
use crate::vm::value::{Value, ValueView};

impl VM {
    pub(crate) fn get_binop_metamethod(&self, a: &Value, b: &Value, event: &str) -> Option<Value> {
        if let Some(t) = a.as_table()
            && let Some(h) = t.borrow().metamethod(event)
        {
            return Some(h);
        }
        if let Some(t) = b.as_table()
            && let Some(h) = t.borrow().metamethod(event)
        {
            return Some(h);
        }
        None
    }

    pub(crate) fn get_unop_metamethod(&self, val: &Value, event: &str) -> Option<Value> {
        if let Some(t) = val.as_table() {
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
            res.into_iter().next().unwrap_or(Value::NIL),
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
        let t = table.as_table()?;
        let tbl = t.borrow();
        let found = if key.is_inline_int() {
            let i = key.as_inline_int_unchecked();
            if i >= 1 && (i as u64) <= tbl.array.len() as u64 {
                tbl.array[i as usize - 1].clone()
            } else {
                tbl.raw_get(key)
            }
        } else if let Some(k) = key.as_str_ref() {
            tbl.fields
                .get_by_vmstr(k.as_ptr(), k.vm_str())
                .cloned()
                .unwrap_or(Value::NIL)
        } else if key.is_bigint() {
            return None;
        } else {
            tbl.raw_get(key)
        };
        if found.is_nil() && tbl.metatable.is_some() {
            return None;
        }
        Some(found)
    }

    /// Raw store into an existing array slot or field, or an append, on a
    /// table that nothing can intercept. Returns false to defer to the slow
    /// path, which also handles nil (removal) and reports errors.
    #[inline]
    pub(crate) fn table_set_fast(table: &Value, key: &Value, val: &Value) -> bool {
        let Some(t) = table.as_table() else {
            return false;
        };
        if val.is_nil() {
            return false;
        }
        let mut tbl = t.borrow_mut();
        if tbl.frozen {
            return false;
        }
        if key.is_inline_int() {
            let i = key.as_inline_int_unchecked();
            if i >= 1 {
                let idx = i as usize - 1;
                if idx < tbl.array.len() {
                    tbl.array[idx].assign_ref(val);
                    return true;
                } else if idx == tbl.array.len() && tbl.metatable.is_none() && tbl.general.is_none()
                {
                    tbl.array.push(val.clone());
                    return true;
                }
            }
            return false;
        }
        if let Some(k) = key.as_str_ref() {
            if tbl.metatable.is_none() {
                tbl.fields.set_by_vmstr(k.as_ptr(), k.vm_str(), val);
                return true;
            } else if let Some(slot) = tbl.fields.get_mut_by_vmstr(k.as_ptr(), k.vm_str()) {
                slot.assign_ref(val);
                return true;
            }
        }
        false
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
        match table.view() {
            ValueView::Table(t) => {
                if let Some(b) = key.as_bigint()
                    && num_traits::Signed::is_positive(b)
                {
                    return Err("table index too large".to_string());
                }
                let val = t.borrow().raw_get(key);
                if !val.is_nil() {
                    return Ok(val);
                }

                // Check __index metamethod
                let index_handler = t.borrow().metamethod("__index");
                if let Some(handler) = index_handler {
                    if handler.is_table() {
                        return self.table_get_depth(&handler, key, depth + 1);
                    }
                    if handler.is_callable() {
                        let res = self.call_function(handler, &[table.clone(), key.clone()])?;
                        return Ok(res.into_iter().next().unwrap_or(Value::NIL));
                    }
                }
                Ok(Value::NIL)
            }
            ValueView::Buffer(b) => {
                let buf = b.borrow();
                match key.view() {
                    ValueView::Int(idx) => {
                        let i = usize::try_from(idx)
                            .map_err(|_| "buffer index too large".to_string())?;
                        let val = buf.read_u8(i)?;
                        Ok(Value::int(val as i64))
                    }
                    ValueView::BigInt(_) => Err("buffer index too large".to_string()),
                    _ => Ok(Value::NIL),
                }
            }
            // Strings index the string library, so `s:upper()` works.
            ValueView::String(_) => match (self.string_lib.as_table(), key.as_str_ref()) {
                (Some(lib), Some(name)) => Ok(lib
                    .borrow()
                    .fields
                    .get_by_vmstr(name.as_ptr(), name.vm_str())
                    .cloned()
                    .unwrap_or(Value::NIL)),
                _ => Ok(Value::NIL),
            },
            _ => Err(format!("attempted to index a {} value", table.type_name())),
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
        match table.view() {
            ValueView::Table(t) => {
                if t.borrow().frozen {
                    return Err("attempted to mutate a frozen table".to_string());
                }

                if let Some(b) = key.as_bigint()
                    && num_traits::Signed::is_positive(b)
                {
                    return Err("table index too large".to_string());
                }
                let key_exists = !t.borrow().raw_get(&key).is_nil();
                if !key_exists {
                    let newindex_handler = t.borrow().metamethod("__newindex");
                    if let Some(handler) = newindex_handler {
                        if handler.is_table() {
                            return self.table_set_depth(&handler, key, val, depth + 1);
                        }
                        if handler.is_callable() {
                            self.call_function(handler, &[table.clone(), key, val])?;
                            return Ok(());
                        }
                    }
                }
                t.borrow_mut().raw_set(key, val)
            }
            ValueView::Buffer(b) => {
                let mut buf = b.borrow_mut();
                match key.view() {
                    ValueView::Int(idx) => {
                        let i = usize::try_from(idx)
                            .map_err(|_| "buffer index too large".to_string())?;
                        let byte_val = match val.view() {
                            ValueView::Int(v) => u8::try_from(v)
                                .map_err(|_| "value out of byte range".to_string())?,
                            ValueView::Float(f) => f as u8,
                            _ => return Err("buffer value expects byte".to_string()),
                        };
                        buf.write_u8(i, byte_val)?;
                        Ok(())
                    }
                    ValueView::BigInt(_) => Err("buffer index too large".to_string()),
                    _ => Err("invalid buffer index".to_string()),
                }
            }
            _ => Err(format!(
                "attempted to set a field on a {} value",
                table.type_name()
            )),
        }
    }
}
