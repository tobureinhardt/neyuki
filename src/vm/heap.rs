//! VM heap allocation and container mutation choke points.
//!
//! Stage 1 of the Memory Follows Work migration isolates all heap object
//! creation (`VmTable`, `VmBuffer`, `VmClosure`) and container writes behind
//! `VmHeap` / `VM` methods while preserving the existing `Rc` + `GcTracker`
//! representation underneath.

use std::cell::RefCell;
use std::rc::Rc;

use crate::bytecode::proto::Proto;
use crate::vm::buffer::VmBuffer;
use crate::vm::gc::GcTracker;
use crate::vm::machine::VM;
use crate::vm::value::{Upvalue, Value, VmClosure, VmTable};

/// Heap facade wired onto [`VM`].
///
/// Stage 1 keeps allocation metadata minimal (`GcTracker` remains the active
/// nursery/cycle collector on `VM`). Later stages host the `Beat`, `Job`, and
/// `Old` arenas here without changing stdlib/builtin callers.
#[derive(Default)]
pub struct VmHeap;

impl VmHeap {
    #[inline]
    pub const fn new() -> Self {
        Self
    }

    #[inline]
    pub fn alloc_table_sized(gc: &mut GcTracker, array_cap: u8, fields_cap: u8) -> Value {
        let table = if array_cap == 0 && fields_cap == 0 {
            VmTable::new()
        } else {
            VmTable::with_capacity(array_cap as usize, fields_cap as usize)
        };
        let rc = Rc::new(RefCell::new(table));
        let size = 64 + (array_cap as usize) * std::mem::size_of::<Value>();
        gc.register_table_sized(&rc, size);
        Value::table(rc)
    }

    #[inline]
    pub fn alloc_table(gc: &mut GcTracker, table: VmTable) -> Value {
        let rc = Rc::new(RefCell::new(table));
        gc.register_table(&rc);
        Value::table(rc)
    }

    #[inline]
    pub fn alloc_table_untracked(table: VmTable) -> Value {
        Value::table(Rc::new(RefCell::new(table)))
    }

    #[inline]
    pub fn alloc_frozen(mut table: VmTable) -> Value {
        table.frozen = true;
        Value::table(Rc::new(RefCell::new(table)))
    }

    #[inline]
    pub fn alloc_buffer(gc: &mut GcTracker, buf: VmBuffer) -> Value {
        let rc = Rc::new(RefCell::new(buf));
        gc.register_buffer(&rc);
        Value::buffer(rc)
    }

    #[inline]
    pub fn alloc_buffer_untracked(buf: VmBuffer) -> Value {
        Value::buffer(Rc::new(RefCell::new(buf)))
    }

    #[inline]
    pub fn alloc_closure(proto: Rc<Proto>, upvalues: Vec<Rc<Upvalue>>) -> Value {
        Value::closure(Rc::new(VmClosure { proto, upvalues }))
    }
}

impl VM {
    /// Allocates a mutable GC-tracked table with precomputed `NewTable` capacities.
    #[inline]
    pub fn alloc_table_sized(&mut self, array_cap: u8, fields_cap: u8) -> Value {
        VmHeap::alloc_table_sized(&mut self.gc, array_cap, fields_cap)
    }

    /// Allocates a mutable GC-tracked table.
    #[inline]
    pub fn alloc_table(&mut self, table: VmTable) -> Value {
        VmHeap::alloc_table(&mut self.gc, table)
    }

    /// Allocates a mutable GC-tracked table with initial array/field capacity.
    #[inline]
    pub fn alloc_table_cap(&mut self, array: usize, fields: usize) -> Value {
        self.alloc_table(VmTable::with_capacity(array, fields))
    }

    /// Allocates an array-backed table initialized with `values`.
    #[inline]
    pub fn alloc_array(&mut self, values: Vec<Value>) -> Value {
        let mut table = VmTable::new();
        table.array = values;
        self.alloc_table(table)
    }

    /// Allocates a frozen (immutable) library/constant table.
    #[inline]
    pub fn alloc_frozen(&mut self, table: VmTable) -> Value {
        let _ = &self.heap;
        VmHeap::alloc_frozen(table)
    }

    /// Allocates a GC-tracked byte buffer.
    #[inline]
    pub fn alloc_buffer(&mut self, buf: VmBuffer) -> Value {
        VmHeap::alloc_buffer(&mut self.gc, buf)
    }

    /// Allocates a closure value.
    #[inline]
    pub fn alloc_closure(&mut self, proto: Rc<Proto>, upvalues: Vec<Rc<Upvalue>>) -> Value {
        let _ = &self.heap;
        VmHeap::alloc_closure(proto, upvalues)
    }

    /// Writes a string field onto `table` if `table` is a non-frozen table.
    #[inline]
    pub fn table_set_str(&mut self, table: &Value, key: &str, val: Value) -> bool {
        let Some(t) = table.as_table() else {
            return false;
        };
        let mut tbl = t.borrow_mut();
        if tbl.frozen {
            return false;
        }
        tbl.set_str(key, val);
        true
    }

    /// Performs a raw write (`table[key] = val`) bypassing metamethods,
    /// rejecting frozen tables.
    #[inline]
    pub fn table_raw_set(&mut self, table: &Value, key: Value, val: Value) -> Result<(), String> {
        let Some(t) = table.as_table() else {
            return Err(format!("expected table, got {}", table.type_name()));
        };
        let mut tbl = t.borrow_mut();
        if tbl.frozen {
            return Err("attempted to mutate a frozen table".to_string());
        }
        tbl.raw_set(key, val)
    }

    /// Updates `table`'s metatable, managing `__gc` GC pins as needed.
    pub fn table_set_metatable(
        &mut self,
        table: &Value,
        mt: Option<Value>,
    ) -> Result<bool, String> {
        let Some(t) = table.as_table() else {
            return Err(format!("expected table, got {}", table.type_name()));
        };
        let mt_rc = match mt {
            None => None,
            Some(v) if v.is_nil() => None,
            Some(v) => {
                let Some(m) = v.as_table() else {
                    return Err(format!(
                        "metatable must be a table or nil, got {}",
                        v.type_name()
                    ));
                };
                Some(m.to_rc())
            }
        };
        let had_gc = {
            let mut tbl = t.borrow_mut();
            tbl.metatable = mt_rc.clone();
            mt_rc
                .as_ref()
                .and_then(|m| m.borrow().fields.get("__gc").cloned())
                .is_some_and(|v| v.is_callable())
        };
        let rc = t.to_rc();
        if had_gc {
            self.gc.pin_table(&rc);
        } else {
            self.gc.unpin_table(&rc);
        }
        if crate::vm::gc::table_weak_values(&rc) {
            self.gc.has_weak_tables = true;
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::proto::Proto;

    #[test]
    fn alloc_helpers_create_expected_values() {
        let mut vm = VM::new();
        let before_allocs = vm.gc.total_allocations;

        let t = vm.alloc_table_cap(2, 2);
        assert!(t.is_table());
        assert_eq!(vm.gc.total_allocations, before_allocs + 1);

        assert!(vm.table_set_str(&t, "answer", Value::int(42)));
        assert_eq!(
            t.as_table().unwrap().borrow().get_str("answer"),
            Value::int(42)
        );

        assert!(
            vm.table_raw_set(&t, Value::int(1), Value::str("ok"))
                .is_ok()
        );
        assert_eq!(
            t.as_table()
                .unwrap()
                .borrow()
                .raw_get(&Value::int(1))
                .as_str(),
            Some("ok")
        );

        let arr = vm.alloc_array(vec![Value::int(10), Value::int(20)]);
        assert_eq!(arr.as_table().unwrap().borrow().array.len(), 2);

        let mut f = VmTable::new();
        f.set_str("k", Value::TRUE);
        let frozen = vm.alloc_frozen(f);
        assert!(frozen.as_table().unwrap().borrow().frozen);
        assert!(!vm.table_set_str(&frozen, "k", Value::FALSE));
        assert!(
            vm.table_raw_set(&frozen, Value::str("k"), Value::FALSE)
                .is_err()
        );

        let buf = vm.alloc_buffer(VmBuffer::new(16));
        assert!(buf.is_buffer());

        let cls = vm.alloc_closure(Rc::new(Proto::new(None, 0, false)), Vec::new());
        assert!(cls.is_closure());
    }

    #[test]
    fn table_set_metatable_updates_gc_pin() {
        let mut vm = VM::new();
        let t = vm.alloc_table(VmTable::new());
        let mt = vm.alloc_table(VmTable::new());
        let func = vm.alloc_closure(Rc::new(Proto::new(None, 0, false)), Vec::new());
        assert!(vm.table_set_str(&mt, "__gc", func));

        assert!(vm.table_set_metatable(&t, Some(mt)).unwrap());
        let ptr = t.table_ptr().unwrap();
        assert!(vm.gc.pinned.contains_key(&ptr));

        assert!(vm.table_set_metatable(&t, None).unwrap());
        assert!(!vm.gc.pinned.contains_key(&ptr));
    }
}
