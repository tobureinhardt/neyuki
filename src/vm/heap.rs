//! VM heap allocation and container mutation choke points.
//!
//! Stage 1-2 of the Memory Follows Work migration routes heap allocation and
//! container mutations through `VmHeap` / `VM`, adding the non-moving `Beat`
//! bump arena (`BeatArena`), nested Beat frames, write barriers, and lazy
//! evacuation to `Old` (`Rc` + `GcTracker` until Stage 3).

use std::cell::RefCell;
use std::rc::Rc;

use crate::bytecode::liveness::LoopSafepoint;
use crate::bytecode::proto::Proto;
use crate::vm::beat::{BeatArena, BeatMark, BeatRemTarget, BeatTable};
use crate::vm::buffer::VmBuffer;
use crate::vm::gc::GcTracker;
use crate::vm::hash::FxHashMap;
use crate::vm::machine::VM;
use crate::vm::table::TableKey;
use crate::vm::value::{BIT_BEAT, TAG_RC_START, Upvalue, Value, VmClosure, VmTable};

const BEAT_DIRECT_OLD_STRING_BYTES: usize = 256;
const BEAT_INLINE_ARR_CAP_MAX: usize = 32;
const BEAT_INLINE_FLD_CAP_MAX: usize = 8;

/// Heap facade wired onto [`VM`].
#[derive(Default)]
pub struct VmHeap {
    pub beat: BeatArena,
}

impl VmHeap {
    #[inline]
    pub fn new() -> Self {
        Self {
            beat: BeatArena::new(),
        }
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
    /// Current bump cursor in the active Beat arena.
    #[inline(always)]
    pub fn beat_cursor(&self) -> BeatMark {
        self.heap.beat.cursor()
    }

    /// Pushes a nested Beat frame (`pcall`, `table.sort`, metamethods,
    /// `coroutine.resume`) keeping outer Beat objects stationary on inner rewinds.
    #[inline(always)]
    pub fn beat_enter_nested(&mut self) -> BeatMark {
        self.heap.beat.enter_nested()
    }

    /// Restores the enclosing Beat frame on return from a nested callback.
    #[inline(always)]
    pub fn beat_exit_nested(&mut self) {
        self.heap.beat.exit_nested();
    }

    /// Sets the active loop rewind watermark, returning the previous watermark.
    #[inline(always)]
    pub fn beat_set_rewind_mark(&mut self, mark: BeatMark) -> BeatMark {
        let prev = self.heap.beat.rewind_mark;
        self.heap.beat.rewind_mark = mark;
        prev
    }

    /// Allocates a table in Beat unless its initial size hint or current arena
    /// pressure calls for allocating directly into `Old`.
    #[inline]
    pub fn alloc_beat_table_sized(&mut self, array_cap: u8, fields_cap: u8) -> Value {
        if (array_cap as usize) > BEAT_INLINE_ARR_CAP_MAX
            || (fields_cap as usize) > BEAT_INLINE_FLD_CAP_MAX
            || self.heap.beat.needs_spill()
        {
            return self.alloc_table_sized(array_cap, fields_cap);
        }
        let ptr = self
            .heap
            .beat
            .alloc_table(array_cap as usize, fields_cap as usize);
        Value::beat_table(ptr)
    }

    /// Allocates a short string in Beat unless large or over the Beat soft cap.
    #[inline]
    pub fn alloc_beat_string(&mut self, s: &str) -> Value {
        if s.len() > BEAT_DIRECT_OLD_STRING_BYTES || self.heap.beat.needs_spill() {
            return Value::str(s);
        }
        let ptr = self.heap.beat.alloc_string(s);
        Value::beat_string(ptr)
    }

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

    /// Promotes a `BeatString` or `BeatTable` (and its reachable Beat subgraph,
    /// handling shared DAGs and cycles via `forwarded`) into `Old`.
    pub fn beat_promote_value(&mut self, val: &mut Value) {
        if !val.is_beat() {
            return;
        }
        if let Some(bs_ptr) = val.beat_str_ptr() {
            let fwd = unsafe { (*bs_ptr).forwarded };
            if fwd != 0 {
                *val = unsafe { Value::clone_from_bits(fwd) };
                return;
            }
            let old_val = Value::str(unsafe { &*bs_ptr }.as_str());
            unsafe {
                (*bs_ptr).forwarded = old_val.bits();
            }
            *val = old_val;
            return;
        }
        if let Some(bt_ptr) = val.beat_table_ptr() {
            let fwd = unsafe { (*bt_ptr).forwarded };
            if fwd != 0 {
                *val = unsafe { Value::clone_from_bits(fwd) };
                return;
            }
            let rc = Rc::new(RefCell::new(VmTable::new()));
            self.gc.register_table(&rc);
            let old_val = Value::table(rc.clone());
            unsafe {
                (*bt_ptr).forwarded = old_val.bits();
            }
            // Assign into `*val` right away so `rc` keeps strong_count >= 1
            // even while promoting nested children/cycles.
            *val = old_val;

            let mut vm_tbl = unsafe {
                let bt = &mut *bt_ptr;
                let extracted = bt.take_as_vm_table();
                bt.drop_inline_old_refs();
                extracted
            };
            for elem in &mut vm_tbl.array {
                self.beat_promote_value(elem);
            }
            for fv in vm_tbl.fields.values_mut() {
                self.beat_promote_value(fv);
            }
            if let Some(gen_map) = vm_tbl.general.take() {
                let mut promoted_map =
                    FxHashMap::with_capacity_and_hasher(gen_map.len(), Default::default());
                for (_, (mut k, mut v)) in *gen_map {
                    self.beat_promote_value(&mut k);
                    self.beat_promote_value(&mut v);
                    if let Ok(tk) = TableKey::from_value(&k) {
                        promoted_map.insert(tk, (k, v));
                    }
                }
                if !promoted_map.is_empty() {
                    vm_tbl.general = Some(Box::new(promoted_map));
                }
            }
            let has_gc = vm_tbl
                .metatable
                .as_ref()
                .and_then(|m| m.borrow().fields.get("__gc").cloned())
                .is_some_and(|v| v.is_callable());
            *rc.borrow_mut() = vm_tbl;
            if has_gc {
                self.gc.pin_table(&rc);
            }
            if crate::vm::gc::table_weak_values(&rc) {
                self.gc.has_weak_tables = true;
            }
        }
    }

    #[inline(always)]
    pub fn beat_promote_if_after(&mut self, val: &mut Value, mark: BeatMark) {
        if val.beat_alloc_mark().is_some_and(|m| m >= mark) {
            self.beat_promote_value(val);
        }
    }

    /// Promotes any `Old`-container escape value right at write time so `Old`
    /// never retains raw `BIT_BEAT` pointers.
    #[inline(always)]
    pub fn beat_escape_if_needed(&mut self, val: &mut Value) {
        if val.is_beat() {
            self.beat_promote_value(val);
        }
    }

    fn beat_promote_outer_table_children(&mut self, table: *mut BeatTable, mark: BeatMark) -> bool {
        let bt = unsafe { &mut *table };
        let mut any_old_promoted = false;
        let mut still_has_younger = false;

        if let Some(ext) = bt.extra_mut() {
            for elem in &mut ext.array {
                if elem.beat_alloc_mark().is_some_and(|m| m >= mark) {
                    self.beat_promote_value(elem);
                } else if elem.is_beat() {
                    still_has_younger = true;
                }
            }
            for fv in ext.fields.values_mut() {
                if fv.beat_alloc_mark().is_some_and(|m| m >= mark) {
                    self.beat_promote_value(fv);
                } else if fv.is_beat() {
                    still_has_younger = true;
                }
            }
            if let Some(gen_map) = ext.general.take() {
                let mut updated =
                    FxHashMap::with_capacity_and_hasher(gen_map.len(), Default::default());
                for (_, (mut k, mut v)) in *gen_map {
                    if k.beat_alloc_mark().is_some_and(|m| m >= mark) {
                        self.beat_promote_value(&mut k);
                    } else if k.is_beat() {
                        still_has_younger = true;
                    }
                    if v.beat_alloc_mark().is_some_and(|m| m >= mark) {
                        self.beat_promote_value(&mut v);
                    } else if v.is_beat() {
                        still_has_younger = true;
                    }
                    if let Ok(tk) = TableKey::from_value(&k) {
                        updated.insert(tk, (k, v));
                    }
                }
                if !updated.is_empty() {
                    ext.general = Some(Box::new(updated));
                }
            }
            return still_has_younger;
        }

        let arr = unsafe { std::slice::from_raw_parts_mut(bt.arr_ptr, bt.arr_len as usize) };
        for elem in arr {
            if elem.beat_alloc_mark().is_some_and(|m| m >= mark) {
                self.beat_promote_value(elem);
                any_old_promoted = true;
            } else if elem.is_beat() {
                still_has_younger = true;
            }
        }
        let flds = unsafe { std::slice::from_raw_parts_mut(bt.fld_ptr, bt.fld_len as usize) };
        for f in flds {
            if f.key.beat_alloc_mark().is_some_and(|m| m >= mark) {
                self.beat_promote_value(&mut f.key);
                any_old_promoted = true;
            } else if f.key.is_beat() {
                still_has_younger = true;
            }
            if f.val.beat_alloc_mark().is_some_and(|m| m >= mark) {
                self.beat_promote_value(&mut f.val);
                any_old_promoted = true;
            } else if f.val.is_beat() {
                still_has_younger = true;
            }
        }
        if any_old_promoted {
            unsafe { self.heap.beat.mark_old_ref_holder(table) };
        }
        still_has_younger
    }

    fn beat_drain_rem_set(&mut self, mark: BeatMark) {
        if self.heap.beat.rem_set.is_empty() {
            return;
        }
        let rem = std::mem::take(&mut self.heap.beat.rem_set);
        let mut retained = Vec::new();
        for target in rem {
            match target {
                BeatRemTarget::OuterBeatTable { table } => {
                    let alloc_mark = unsafe { (*table).alloc_mark };
                    if alloc_mark < mark {
                        let has_older_beat_refs =
                            self.beat_promote_outer_table_children(table, mark);
                        if has_older_beat_refs && alloc_mark < self.heap.beat.floor {
                            retained.push(BeatRemTarget::OuterBeatTable { table });
                        } else {
                            unsafe { (*table).in_rem_set = false };
                        }
                    } else {
                        unsafe { (*table).in_rem_set = false };
                    }
                }
            }
        }
        if !retained.is_empty() {
            self.heap.beat.rem_set.extend(retained);
        }
    }

    fn beat_collect_finalizers(&mut self, mark: BeatMark) -> Vec<(Value, Value)> {
        if self.heap.beat.finalizers.is_empty() {
            return Vec::new();
        }
        let mut to_run = Vec::new();
        let fins = std::mem::take(&mut self.heap.beat.finalizers);
        let mut kept = Vec::new();
        for (m, ptr) in fins {
            if m >= mark {
                let forwarded = unsafe { (*ptr).forwarded };
                if forwarded == 0
                    && let Some(ext) = unsafe { (*ptr).extra_ref() }
                    && let Some(gc_fn) = ext.metamethod("__gc")
                    && gc_fn.is_callable()
                {
                    let mut dead_tbl = Value::beat_table(ptr);
                    self.beat_promote_value(&mut dead_tbl);
                    to_run.push((dead_tbl, gc_fn));
                }
            } else {
                kept.push((m, ptr));
            }
        }
        self.heap.beat.finalizers = kept;
        to_run
    }

    fn beat_run_finalizers(&mut self, finalizers: Vec<(Value, Value)>) {
        for (tbl, gc_fn) in finalizers {
            let _ = self.call_function(gc_fn, &[tbl]);
        }
    }

    /// Loop safepoint cleanup: evacuates surviving Beat objects reachable from
    /// outer containers (`rem_set`), live registers in `[loop_base .. loop_base + max_regs]`,
    /// or open upvalues; clears dead Beat registers (`!live_mask`); runs any dying
    /// Beat `__gc` finalizers; and rewinds the Beat cursor in O(1).
    pub fn beat_evacuate_and_reset(
        &mut self,
        mark: BeatMark,
        safepoint: &LoopSafepoint,
        loop_base: usize,
        max_regs: usize,
    ) {
        if self.heap.beat.needs_spill() {
            self.beat_flush_to_floor(self.heap.beat.floor, true);
            return;
        }
        if self.heap.beat.cursor() <= mark && self.heap.beat.rem_set.is_empty() {
            return;
        }
        self.beat_drain_rem_set(mark);

        // Promote any Beat values captured in open upvalues within this frame or
        // anywhere else so stale-register clearing cannot drop captured locals.
        let mut open_slots: Vec<usize> = self
            .open_upvalues
            .iter()
            .filter_map(|&(idx, _)| (idx >= loop_base).then_some(idx))
            .collect();
        open_slots.sort_unstable();
        open_slots.dedup();
        for &idx in &open_slots {
            if idx < self.stack.len() {
                let mut tmp = std::mem::replace(&mut self.stack[idx], Value::NIL);
                self.beat_promote_if_after(&mut tmp, mark);
                self.stack[idx] = tmp;
            }
        }

        let end = (loop_base + max_regs).min(self.stack.len());
        let reg_count = end.saturating_sub(loop_base);
        for r in 0..reg_count {
            let slot_idx = loop_base + r;
            let bits = self.stack[slot_idx].bits();
            if bits < TAG_RC_START || (bits & BIT_BEAT) == 0 {
                continue;
            }
            if safepoint.is_live(r as u8) || open_slots.binary_search(&slot_idx).is_ok() {
                let mut tmp = std::mem::replace(&mut self.stack[slot_idx], Value::NIL);
                self.beat_promote_if_after(&mut tmp, mark);
                self.stack[slot_idx] = tmp;
            } else if self.stack[slot_idx]
                .beat_alloc_mark()
                .is_some_and(|m| m >= mark)
            {
                self.stack[slot_idx] = Value::NIL;
            }
        }

        let finalizers = self.beat_collect_finalizers(mark);
        self.heap.beat.rewind(mark);
        if !finalizers.is_empty() {
            self.beat_run_finalizers(finalizers);
        }
    }

    /// Flushes Beat allocations at or above `floor` (`promote_all = true` also
    /// promotes Beat refs `< floor` on coroutine yield/suspend).
    pub fn beat_flush_to_floor(&mut self, floor: BeatMark, promote_all: bool) {
        if self.heap.beat.cursor() == BeatMark::ZERO && self.heap.beat.rem_set.is_empty() {
            return;
        }
        self.beat_drain_rem_set(floor);

        let stack_len = self.stack.len();
        for i in 0..stack_len {
            let bits = self.stack[i].bits();
            if bits >= TAG_RC_START && (bits & BIT_BEAT) != 0 {
                let mut tmp = std::mem::replace(&mut self.stack[i], Value::NIL);
                if promote_all {
                    self.beat_promote_value(&mut tmp);
                } else {
                    self.beat_promote_if_after(&mut tmp, floor);
                }
                self.stack[i] = tmp;
            }
        }
        let ret_len = self.returned.len();
        for i in 0..ret_len {
            let bits = self.returned[i].bits();
            if bits >= TAG_RC_START && (bits & BIT_BEAT) != 0 {
                let mut tmp = std::mem::replace(&mut self.returned[i], Value::NIL);
                if promote_all {
                    self.beat_promote_value(&mut tmp);
                } else {
                    self.beat_promote_if_after(&mut tmp, floor);
                }
                self.returned[i] = tmp;
            }
        }

        let finalizers = self.beat_collect_finalizers(floor);
        self.heap.beat.rewind(floor);
        if !finalizers.is_empty() {
            self.beat_run_finalizers(finalizers);
        }
    }

    /// Writes a string field onto `table` if `table` is a non-frozen table.
    #[inline]
    pub fn table_set_str(&mut self, table: &Value, key: &str, mut val: Value) -> bool {
        if let Some(bt_ptr) = table.beat_table_ptr() {
            if unsafe { (*bt_ptr).is_frozen() } {
                return false;
            }
            let k_val = self.alloc_beat_string(key);
            return unsafe { self.heap.beat.table_raw_set(bt_ptr, k_val, val) }.is_ok();
        }
        let Some(t) = table.as_table() else {
            return false;
        };
        if t.borrow().frozen {
            return false;
        }
        self.beat_escape_if_needed(&mut val);
        self.gc.write_barrier(table, &val);
        t.borrow_mut().set_str(key, val);
        true
    }

    /// Performs a raw write (`table[key] = val`) bypassing metamethods,
    /// rejecting frozen tables and handling Beat -> Old promotion barriers.
    #[inline]
    pub fn table_raw_set(
        &mut self,
        table: &Value,
        mut key: Value,
        mut val: Value,
    ) -> Result<(), String> {
        if let Some(bt_ptr) = table.beat_table_ptr() {
            return unsafe { self.heap.beat.table_raw_set(bt_ptr, key, val) };
        }
        let Some(t) = table.as_table() else {
            return Err(format!("expected table, got {}", table.type_name()));
        };
        if t.borrow().frozen {
            return Err("attempted to mutate a frozen table".to_string());
        }
        self.beat_escape_if_needed(&mut key);
        self.beat_escape_if_needed(&mut val);
        self.gc.write_barrier(table, &val);
        t.borrow_mut().raw_set(key, val)
    }

    /// Updates `table`'s metatable, managing Beat extra storage and `__gc` pins.
    pub fn table_set_metatable(
        &mut self,
        table: &Value,
        mt: Option<Value>,
    ) -> Result<bool, String> {
        let mt_rc = match mt {
            None => None,
            Some(v) if v.is_nil() => None,
            Some(mut v) => {
                if !v.is_table() {
                    return Err(format!(
                        "metatable must be a table or nil, got {}",
                        v.type_name()
                    ));
                }
                self.beat_escape_if_needed(&mut v);
                Some(
                    v.as_table()
                        .expect("promoted metatable must be Old")
                        .to_rc(),
                )
            }
        };

        if let Some(bt_ptr) = table.beat_table_ptr() {
            let had_gc = mt_rc
                .as_ref()
                .and_then(|m| m.borrow().fields.get("__gc").cloned())
                .is_some_and(|v| v.is_callable());
            let alloc_mark = unsafe { (*bt_ptr).alloc_mark };
            {
                let bt = unsafe { &mut *bt_ptr };
                let ext = self.heap.beat.ensure_extra(bt);
                ext.metatable = mt_rc;
            }
            if had_gc {
                self.heap.beat.finalizers.push((alloc_mark, bt_ptr));
            }
            return Ok(true);
        }

        let Some(t) = table.as_table() else {
            return Err(format!("expected table, got {}", table.type_name()));
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

    #[test]
    fn beat_evacuates_dag_and_cycle_and_clears_stale_registers() {
        let mut vm = VM::new();
        vm.stack.resize(4, Value::NIL);

        let outer = vm.alloc_beat_table_sized(4, 4);
        assert!(outer.is_beat_table());
        let mark = vm.beat_cursor();
        let prev_rw = vm.beat_set_rewind_mark(mark);

        let a = vm.alloc_beat_table_sized(4, 4);
        let b = vm.alloc_beat_table_sized(4, 4);
        let dead = vm.alloc_beat_table_sized(4, 4);
        let key_self = vm.alloc_beat_string("other");

        vm.table_raw_set(&a, key_self, b.clone()).unwrap();
        vm.table_raw_set(&b, Value::str("back"), a.clone()).unwrap();
        vm.table_raw_set(&b, Value::int(1), Value::int(99)).unwrap();
        vm.table_raw_set(&outer, Value::int(1), a.clone()).unwrap();
        vm.table_raw_set(&dead, Value::int(1), Value::int(7))
            .unwrap();

        // Register 0 is live (`a`), register 1 (`dead`) is stale and not live.
        vm.stack[0] = a;
        vm.stack[1] = dead;

        let sf = LoopSafepoint {
            loop_start: 0,
            live_mask: [1, 0, 0, 0], // only register 0 is live
        };
        vm.beat_evacuate_and_reset(mark, &sf, 0, 4);
        vm.beat_set_rewind_mark(prev_rw);

        // Stale register cleared cleanly; live register & outer child promoted cleanly.
        assert!(vm.stack[1].is_nil());
        assert!(vm.stack[0].is_old_table());
        assert!(outer.is_beat_table());

        let promoted_a = outer.table_borrow().unwrap().raw_get(&Value::int(1));
        assert!(promoted_a.is_old_table());
        let promoted_b = promoted_a
            .table_borrow()
            .unwrap()
            .raw_get(&Value::str("other"));
        assert!(promoted_b.is_old_table());
        assert_eq!(
            promoted_b.table_borrow().unwrap().raw_get(&Value::int(1)),
            Value::int(99)
        );
        // Cycle preserved without infinite recursion or broken identity.
        let back = promoted_b
            .table_borrow()
            .unwrap()
            .raw_get(&Value::str("back"));
        assert_eq!(back, promoted_a);
    }

    #[test]
    fn beat_soft_cap_spills_safely() {
        let mut vm = VM::new();
        vm.stack.resize(2, Value::NIL);
        // Allocate small Beat tables until active_bytes crosses 4 MiB soft cap.
        let mut last = Value::NIL;
        for i in 0..30_000 {
            let t = vm.alloc_beat_table_sized(4, 4);
            last = t;
            if i == 10 {
                vm.stack[0] = last.clone();
            }
            if !last.is_beat_table() {
                assert!(vm.heap.beat.needs_spill());
                break;
            }
        }
        assert!(last.is_table());
        assert!(vm.stack[0].is_table());
    }
}
