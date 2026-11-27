//! VM heap allocation and container mutation choke points.
//!
//! `Memory Follows Work` tiers:
//! - `Beat` (`BeatArena`): short-lived bump allocation per loop/frame.
//! - `Job` (`JobManager` + `OldArena` region): job/coroutine-scoped memory
//!   with O(1) teardown on exit.
//! - `Old` (`OldArena` SlotArena): `UniqueOwner` immediate subtree
//!   reclamation + auto-`Shared` local Bacon-Rajan cycle collection +
//!   generational `Look(slot, epoch)` handles.

use std::cell::RefCell;
use std::rc::Rc;

use crate::bytecode::instruction::MULTRET;
use crate::bytecode::liveness::LoopSafepoint;
use crate::bytecode::proto::Proto;
use crate::vm::beat::{ActiveBeatLoop, BeatArena, BeatMark, BeatRemTarget, BeatTable};
use crate::vm::buffer::VmBuffer;
use crate::vm::hash::{FxHashMap, FxHashSet};
use crate::vm::job::JobManager;
use crate::vm::machine::VM;
use crate::vm::old::{OldArena, ReclaimPlan, SlotId, table_is_weak_v};
use crate::vm::table::TableKey;
use crate::vm::value::{Upvalue, Value, ValueView, VmClosure, VmTable};

const BEAT_DIRECT_OLD_STRING_BYTES: usize = 256;
const BEAT_INLINE_ARR_CAP_MAX: usize = 32;
const BEAT_INLINE_FLD_CAP_MAX: usize = 8;

/// Heap facade wired onto [`VM`].
#[derive(Default)]
pub struct VmHeap {
    pub beat: BeatArena,
    pub old: OldArena,
    pub jobs: JobManager,
}

impl VmHeap {
    #[inline]
    pub fn new() -> Self {
        Self {
            beat: BeatArena::new(),
            old: OldArena::new(),
            jobs: JobManager::new(),
        }
    }

    #[inline]
    pub fn alloc_table_sized(
        old: &mut OldArena,
        jobs: &mut JobManager,
        array_cap: u8,
        fields_cap: u8,
    ) -> Value {
        let job_id = jobs.current_job_id();
        let (id, rc) = old.alloc_with_capacity(array_cap as usize, fields_cap as usize, job_id);
        if job_id != 0 {
            jobs.record_alloc(job_id, id);
        }
        Value::arena_table(Rc::as_ptr(&rc))
    }

    #[inline]
    pub fn alloc_table(old: &mut OldArena, jobs: &mut JobManager, table: VmTable) -> Value {
        let job_id = jobs.current_job_id();
        let (id, rc) = old.insert_table(table, job_id);
        if job_id != 0 {
            jobs.record_alloc(job_id, id);
        }
        Value::arena_table(Rc::as_ptr(&rc))
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
    pub fn alloc_buffer(old: &mut OldArena, buf: VmBuffer) -> Value {
        let rc = Rc::new(RefCell::new(buf));
        old.register_buffer(&rc);
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

    /// Concatenates two short strings directly into Beat with zero intermediate
    /// `String` allocation when within inline threshold and under soft cap.
    #[inline]
    pub fn alloc_beat_concat(&mut self, a: &str, b: &str) -> Value {
        if a.len() + b.len() > BEAT_DIRECT_OLD_STRING_BYTES || self.beat.needs_spill() {
            let mut s = String::with_capacity(a.len() + b.len());
            s.push_str(a);
            s.push_str(b);
            return Value::string(s);
        }
        let ptr = self.beat.alloc_string_concat(a, b);
        Value::beat_string(ptr)
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
        let cur = self.heap.beat.enter_nested();
        self.heap
            .beat
            .sync_barrier_bounds(self.beat_loops.first().map(|l| l.mark));
        cur
    }

    /// Restores the enclosing Beat frame on return from a nested callback.
    #[inline(always)]
    pub fn beat_exit_nested(&mut self) {
        self.heap.beat.exit_nested();
        self.heap
            .beat
            .sync_barrier_bounds(self.beat_loops.first().map(|l| l.mark));
    }

    /// Sets the active loop rewind watermark, returning the previous watermark.
    #[inline(always)]
    pub fn beat_set_rewind_mark(&mut self, mark: BeatMark) -> BeatMark {
        let prev = self.heap.beat.rewind_mark;
        self.heap.beat.rewind_mark = mark;
        self.heap
            .beat
            .sync_barrier_bounds(self.beat_loops.first().map(|l| l.mark));
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

    /// Concatenates two short strings directly into Beat with zero intermediate
    /// `String` allocation when within inline threshold and under soft cap.
    #[inline]
    pub fn alloc_beat_concat(&mut self, a: &str, b: &str) -> Value {
        self.heap.alloc_beat_concat(a, b)
    }

    /// Allocates a mutable tier-tracked table with precomputed `NewTable` capacities.
    #[inline]
    pub fn alloc_table_sized(&mut self, array_cap: u8, fields_cap: u8) -> Value {
        VmHeap::alloc_table_sized(
            &mut self.heap.old,
            &mut self.heap.jobs,
            array_cap,
            fields_cap,
        )
    }

    /// Allocates a mutable tier-tracked table in the active `Job` or `Old`.
    #[inline]
    pub fn alloc_table(&mut self, table: VmTable) -> Value {
        VmHeap::alloc_table(&mut self.heap.old, &mut self.heap.jobs, table)
    }

    /// Allocates a mutable tier-tracked table with initial array/field capacity.
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

    /// Allocates a tracked byte buffer.
    #[inline]
    pub fn alloc_buffer(&mut self, buf: VmBuffer) -> Value {
        VmHeap::alloc_buffer(&mut self.heap.old, buf)
    }

    /// Allocates a closure value.
    #[inline]
    pub fn alloc_closure(&mut self, proto: Rc<Proto>, upvalues: Vec<Rc<Upvalue>>) -> Value {
        let _ = &self.heap;
        VmHeap::alloc_closure(proto, upvalues)
    }

    /// Promotes a `BeatString` or `BeatTable` (and its reachable Beat subgraph,
    /// handling shared DAGs and cycles via `forwarded`) into the active `Job` or `Old`.
    pub fn beat_promote_value(&mut self, val: &mut Value) {
        if !val.is_beat() {
            return;
        }
        if let Some(bs_ptr) = val.raw_beat_str_ptr() {
            let (fwd, alloc_mark) = unsafe { ((*bs_ptr).forwarded, (*bs_ptr).alloc_mark) };
            if fwd != 0 {
                *val = unsafe { Value::clone_from_bits(fwd) };
                return;
            }
            let old_val = Value::str(unsafe { &*bs_ptr }.as_str());
            unsafe {
                (*bs_ptr).forwarded = old_val.bits();
            }
            self.heap
                .beat
                .promoted_refs
                .push((alloc_mark, old_val.clone()));
            *val = old_val;
            return;
        }
        if let Some(bt_ptr) = val.raw_beat_table_ptr() {
            let (fwd, alloc_mark) = unsafe { ((*bt_ptr).forwarded, (*bt_ptr).alloc_mark) };
            if fwd != 0 {
                *val = unsafe { Value::clone_from_bits(fwd) };
                return;
            }
            let target_job = self.heap.jobs.current_job_id();
            let (id, rc) = self.heap.old.alloc_with_capacity(0, 0, target_job);
            if target_job != 0 {
                self.heap.jobs.record_alloc(target_job, id);
            }
            let old_val = Value::arena_table(Rc::as_ptr(&rc));
            unsafe {
                (*bt_ptr).forwarded = old_val.bits();
            }
            // Pin one strong ref until `alloc_mark` rewinds so un-rewritten
            // BIT_BEAT aliases keep a live `forwarded` target until Beat teardown.
            self.heap
                .beat
                .promoted_refs
                .push((alloc_mark, old_val.clone()));
            *val = old_val;

            let mut vm_tbl = unsafe {
                let bt = &mut *bt_ptr;
                let extracted = bt.take_as_vm_table();
                bt.drop_inline_old_refs();
                extracted
            };
            vm_tbl.arena_id = id.pack();
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
            let new_bytes = OldArena::table_bytes(&vm_tbl);
            let old_bytes = self.heap.old.slots[id.slot as usize].approx_bytes;
            if new_bytes > old_bytes {
                self.heap.old.bytes_allocated = self
                    .heap
                    .old
                    .bytes_allocated
                    .saturating_add(new_bytes - old_bytes);
                self.heap.old.slots[id.slot as usize].approx_bytes = new_bytes;
            }
            let is_weak = table_is_weak_v(&vm_tbl);
            // Weak-value tables immediately downgrade strong child refs into
            // generational `Look` handles BEFORE `refresh_meta` so it never
            // emits spurious `on_old_release` calls for Beat-resident entries.
            if is_weak {
                for elem in &mut vm_tbl.array {
                    if let Some(cid) = OldArena::slot_of_value(elem)
                        && !elem.table_borrow().is_some_and(|t| t.is_frozen())
                    {
                        *elem = Value::look(cid.slot, cid.epoch);
                    }
                }
                for fv in vm_tbl.fields.values_mut() {
                    if let Some(cid) = OldArena::slot_of_value(fv)
                        && !fv.table_borrow().is_some_and(|t| t.is_frozen())
                    {
                        *fv = Value::look(cid.slot, cid.epoch);
                    }
                }
                if let Some(gen_map) = &mut vm_tbl.general {
                    for (_, v) in gen_map.values_mut() {
                        if let Some(cid) = OldArena::slot_of_value(v)
                            && !v.table_borrow().is_some_and(|t| t.is_frozen())
                        {
                            *v = Value::look(cid.slot, cid.epoch);
                        }
                    }
                }
            }
            *rc.borrow_mut() = vm_tbl;
            self.heap.old.refresh_meta(id);
            let b = rc.borrow();
            if target_job == 0 {
                if !is_weak {
                    for elem in &b.array {
                        self.register_promoted_child(id, elem);
                    }
                    for fv in b.fields.values() {
                        self.register_promoted_child(id, fv);
                    }
                }
                if let Some(gen_map) = &b.general {
                    for (k, v) in gen_map.values() {
                        self.register_promoted_child(id, k);
                        if !is_weak {
                            self.register_promoted_child(id, v);
                        }
                    }
                }
                if let Some(mt) = &b.metatable
                    && let Some(mid) = SlotId::unpack(mt.borrow().arena_id)
                    && self.heap.old.job_of(mid) == Some(0)
                {
                    self.heap.old.on_old_store(Some(id), mid);
                }
            }
        }
    }

    #[inline]
    fn register_promoted_child(&mut self, parent_id: SlotId, val: &Value) {
        if let Some(cid) = OldArena::slot_of_value(val) {
            if let Some(cj) = self.heap.old.job_of(cid) {
                if cj != 0 {
                    self.heap
                        .jobs
                        .escape_subgraph(&mut self.heap.old, cid, cj, 0);
                }
                self.heap.old.on_old_store(Some(parent_id), cid);
            }
        } else if val.is_closure() {
            self.heap.old.mark_has_old_children(parent_id);
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

    fn beat_promote_outer_table_children(
        &mut self,
        table: *mut BeatTable,
        mark: BeatMark,
    ) -> BeatMark {
        let bt = unsafe { &mut *table };
        let mut any_old_promoted = false;
        let mut max_younger = BeatMark::ZERO;

        if let Some(ext) = bt.extra_mut() {
            for elem in &mut ext.array {
                if let Some(m) = elem.beat_alloc_mark() {
                    if m >= mark {
                        self.beat_promote_value(elem);
                    } else if m > max_younger {
                        max_younger = m;
                    }
                }
            }
            for fv in ext.fields.values_mut() {
                if let Some(m) = fv.beat_alloc_mark() {
                    if m >= mark {
                        self.beat_promote_value(fv);
                    } else if m > max_younger {
                        max_younger = m;
                    }
                }
            }
            if let Some(gen_map) = ext.general.take() {
                let mut updated =
                    FxHashMap::with_capacity_and_hasher(gen_map.len(), Default::default());
                for (_, (mut k, mut v)) in *gen_map {
                    if let Some(m) = k.beat_alloc_mark() {
                        if m >= mark {
                            self.beat_promote_value(&mut k);
                        } else if m > max_younger {
                            max_younger = m;
                        }
                    }
                    if let Some(m) = v.beat_alloc_mark() {
                        if m >= mark {
                            self.beat_promote_value(&mut v);
                        } else if m > max_younger {
                            max_younger = m;
                        }
                    }
                    if let Ok(tk) = TableKey::from_value(&k) {
                        updated.insert(tk, (k, v));
                    }
                }
                if !updated.is_empty() {
                    ext.general = Some(Box::new(updated));
                }
            }
            return max_younger;
        }

        let arr = unsafe { std::slice::from_raw_parts_mut(bt.arr_ptr, bt.arr_len as usize) };
        for elem in arr {
            if let Some(m) = elem.beat_alloc_mark() {
                if m >= mark {
                    self.beat_promote_value(elem);
                    any_old_promoted = true;
                } else if m > max_younger {
                    max_younger = m;
                }
            }
        }
        let flds = unsafe { std::slice::from_raw_parts_mut(bt.fld_ptr, bt.fld_len as usize) };
        for f in flds {
            if let Some(m) = f.key.beat_alloc_mark() {
                if m >= mark {
                    self.beat_promote_value(&mut f.key);
                    any_old_promoted = true;
                } else if m > max_younger {
                    max_younger = m;
                }
            }
            if let Some(m) = f.val.beat_alloc_mark() {
                if m >= mark {
                    self.beat_promote_value(&mut f.val);
                    any_old_promoted = true;
                } else if m > max_younger {
                    max_younger = m;
                }
            }
        }
        if any_old_promoted {
            unsafe { self.heap.beat.mark_old_ref_holder(table) };
        }
        max_younger
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
                    let (alloc_mark, forwarded) =
                        unsafe { ((*table).alloc_mark, (*table).forwarded) };
                    if forwarded != 0 {
                        unsafe { (*table).in_rem_set = false };
                        continue;
                    }
                    if alloc_mark < mark {
                        let max_younger = self.beat_promote_outer_table_children(table, mark);
                        if max_younger > alloc_mark
                            && max_younger >= self.heap.beat.barrier_outer_min
                        {
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
                    && let Some(mut gc_fn) = ext.metamethod("__gc")
                    && gc_fn.is_callable()
                {
                    let mut dead_tbl = Value::beat_table(ptr);
                    self.beat_promote_value(&mut dead_tbl);
                    self.beat_promote_value(&mut gc_fn);
                    if let Some(cid) = OldArena::slot_of_value(&dead_tbl)
                        && let Some(slot) = self.heap.old.slots.get_mut(cid.slot as usize)
                    {
                        slot.finalized = true;
                    }
                    to_run.push((dead_tbl, gc_fn));
                }
            } else {
                kept.push((m, ptr));
            }
        }
        self.heap.beat.finalizers = kept;
        to_run.reverse();
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
            if !self.stack[slot_idx].is_beat() {
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
        if !self.heap.old.pending_free.is_empty() {
            self.flush_pending_old(false);
        }
        if !finalizers.is_empty() {
            self.beat_run_finalizers(finalizers);
        }
    }

    /// Flushes Beat allocations at or above `floor` (`promote_all = true` also
    /// promotes Beat refs `< floor` on coroutine yield/suspend).
    pub fn beat_flush_to_floor(&mut self, floor: BeatMark, promote_all: bool) {
        if self.heap.beat.cursor() == BeatMark::ZERO
            && self.heap.beat.rem_set.is_empty()
            && self.heap.beat.promoted_refs.is_empty()
        {
            return;
        }
        self.beat_drain_rem_set(floor);

        let stack_len = self.stack.len();
        let mut live_windows: Vec<(usize, usize)> = Vec::with_capacity(self.frames.len());
        for f in &self.frames {
            let start = f.base.saturating_sub(1);
            let end = (f.base + f.closure.proto.max_registers as usize).min(stack_len);
            if start < end {
                if let Some(last) = live_windows.last_mut()
                    && start <= last.1
                {
                    last.1 = last.1.max(end);
                } else {
                    live_windows.push((start, end));
                }
            }
        }
        let mut open_slots: Vec<usize> = self
            .open_upvalues
            .iter()
            .filter_map(|&(idx, _)| (idx < stack_len).then_some(idx))
            .collect();
        open_slots.sort_unstable();
        open_slots.dedup();
        let is_live_slot = |idx: usize| -> bool {
            live_windows.iter().any(|&(s, e)| idx >= s && idx < e)
                || open_slots.binary_search(&idx).is_ok()
        };

        for i in 0..stack_len {
            if self.stack[i].is_beat() {
                if is_live_slot(i) {
                    let mut tmp = std::mem::replace(&mut self.stack[i], Value::NIL);
                    if promote_all {
                        self.beat_promote_value(&mut tmp);
                    } else {
                        self.beat_promote_if_after(&mut tmp, floor);
                    }
                    self.stack[i] = tmp;
                } else if promote_all || self.stack[i].beat_alloc_mark().is_some_and(|m| m >= floor)
                {
                    self.stack[i] = Value::NIL;
                }
            }
        }
        let ret_len = self.returned.len();
        for i in 0..ret_len {
            if self.returned[i].is_beat() {
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
        let clamped = self.heap.beat.cursor();
        for lp in &mut self.beat_loops {
            if lp.mark > clamped {
                lp.mark = clamped;
            }
            if lp.prev_rewind > clamped {
                lp.prev_rewind = clamped;
            }
        }
        if self.heap.beat.rewind_mark > clamped {
            self.heap.beat.rewind_mark = clamped;
        }
        self.heap
            .beat
            .sync_barrier_bounds(self.beat_loops.first().map(|l| l.mark));
        if !finalizers.is_empty() {
            self.beat_run_finalizers(finalizers);
        }
    }

    /// Pops any active inner loop safepoints belonging to `frame_depth` whose
    /// bytecode back-edge precedes `cur_ip` (e.g. after `break` or early exit).
    #[inline]
    pub(crate) fn beat_pop_exited_loops(&mut self, frame_depth: usize, cur_ip: usize) {
        while let Some(&top) = self.beat_loops.last() {
            if top.frame_depth != frame_depth || cur_ip <= top.loop_end {
                break;
            }
            self.beat_loops.pop();
            self.beat_set_rewind_mark(top.prev_rewind);
        }
    }

    /// Unwinds active Beat loop entries belonging to frames at or above `target_depth`
    /// on `Return`, `EndSentinel`, or `TailCall`.
    #[inline(always)]
    pub(crate) fn beat_unwind_frames(&mut self, target_depth: usize) {
        if self.beat_loops.is_empty() {
            return;
        }
        while let Some(&top) = self.beat_loops.last() {
            if top.frame_depth < target_depth {
                break;
            }
            self.beat_loops.pop();
            self.beat_set_rewind_mark(top.prev_rewind);
        }
    }

    /// Enters a numeric Beat-guarded loop at `ForPrepBeat`.
    #[inline]
    pub(crate) fn beat_enter_for_prep(&mut self, cur_ip: usize, loop_end: usize, site: u16) {
        let frame_depth = self.frames.len();
        self.beat_pop_exited_loops(frame_depth, cur_ip);
        let mark = self.beat_cursor();
        let prev_rewind = self.heap.beat.rewind_mark;
        self.beat_loops.push(ActiveBeatLoop {
            frame_depth,
            site,
            loop_end,
            mark,
            prev_rewind,
        });
        self.beat_set_rewind_mark(mark);
    }

    /// Handles a loop back-edge safepoint (`ForLoopBeat` / backward `JumpBeat`).
    #[inline]
    pub(crate) fn beat_safepoint_step(
        &mut self,
        sf: LoopSafepoint,
        frame_base: usize,
        max_regs: usize,
        cur_ip: usize,
        site: u16,
        loop_again: bool,
    ) {
        let frame_depth = self.frames.len();
        self.beat_pop_exited_loops(frame_depth, cur_ip);
        if let Some(&top) = self.beat_loops.last()
            && top.frame_depth == frame_depth
            && top.site == site
        {
            self.beat_evacuate_and_reset(top.mark, &sf, frame_base, max_regs);
            if !loop_again {
                self.beat_loops.pop();
                self.beat_set_rewind_mark(top.prev_rewind);
            }
        } else if loop_again {
            let mark = self.beat_cursor();
            let prev_rewind = self.heap.beat.rewind_mark;
            self.beat_loops.push(ActiveBeatLoop {
                frame_depth,
                site,
                loop_end: sf.loop_end,
                mark,
                prev_rewind,
            });
            self.beat_set_rewind_mark(mark);
        }
    }

    /// Appends one element to an array table (Beat, Job, or Old with barriers).
    #[inline]
    pub(crate) fn beat_append_array(
        &mut self,
        table_slot: usize,
        mut val: Value,
    ) -> Result<(), String> {
        let tbl_val = &self.stack[table_slot];
        if let Some(bt_ptr) = tbl_val.beat_table_ptr() {
            return unsafe { self.heap.beat.table_push_array(bt_ptr, val) };
        }
        let tbl_copy = tbl_val.clone();
        let Some(t_rc) = tbl_copy.to_table() else {
            return Err("cannot append to non-table".to_string());
        };
        if t_rc.borrow().frozen {
            return Err("attempted to mutate a frozen table".to_string());
        }
        self.container_write_barrier(&tbl_copy, None, &mut val);
        t_rc.borrow_mut().array.push(val);
        Ok(())
    }

    /// Appends stack slice `base+list .. count` into `base+table` (Beat, Job, or Old).
    #[inline]
    pub(crate) fn beat_set_list(
        &mut self,
        base: usize,
        table: u8,
        list: u8,
        count: u8,
    ) -> Result<(), String> {
        let start = base + list as usize;
        let end = if count == MULTRET {
            self.top.max(start)
        } else {
            start + count as usize
        };
        let end = end.min(self.stack.len());
        let table_slot = base + table as usize;
        if let Some(bt_ptr) = self.stack[table_slot].beat_table_ptr() {
            for idx in start..end {
                let val = self.stack[idx].clone();
                unsafe {
                    self.heap.beat.table_push_array(bt_ptr, val)?;
                }
            }
            return Ok(());
        }
        let tbl_copy = self.stack[table_slot].clone();
        let Some(t_rc) = tbl_copy.to_table() else {
            return Err("cannot SetList to non-table".to_string());
        };
        let mut items: Vec<Value> = self.stack[start..end].to_vec();
        for item in &mut items {
            self.container_write_barrier(&tbl_copy, None, item);
        }
        t_rc.borrow_mut().array.extend(items);
        Ok(())
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
        let prev = {
            let b = t.borrow();
            if b.frozen {
                return false;
            }
            b.fields.get(key).cloned()
        };
        self.container_write_barrier(table, prev, &mut val);
        t.borrow_mut().set_str(key, val);
        if key == "__mode" || key == "__gc" {
            self.refresh_meta_dependents(t.as_ptr());
        }
        true
    }

    /// Performs a raw write (`table[key] = val`) bypassing metamethods,
    /// rejecting frozen tables and running Beat/Job/Old barriers.
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
        let container_job = OldArena::slot_of_value(table)
            .and_then(|cid| self.heap.old.job_of(cid))
            .unwrap_or(0);
        if let Some(kid) = OldArena::slot_of_value(&key)
            && let Some(kj) = self.heap.old.job_of(kid)
            && kj != 0
            && kj != container_job
        {
            self.heap
                .jobs
                .escape_subgraph(&mut self.heap.old, kid, kj, container_job);
        }

        let prev = if !val.is_trivial() || t.borrow().metatable.is_some() {
            Some(t.borrow().raw_get(&key))
        } else {
            let cur = t.borrow().raw_get(&key);
            (!cur.is_trivial()).then_some(cur)
        };
        let is_meta_key = key.as_str().is_some_and(|s| s == "__mode" || s == "__gc");
        let tbl_ptr = t.as_ptr();
        let container_id = OldArena::slot_of_value(table);
        let key_slot = if container_job == 0 && !key.is_nil() && !key.is_int() && !key.is_string() {
            OldArena::slot_of_value(&key).filter(|kid| self.heap.old.job_of(*kid) == Some(0))
        } else {
            None
        };
        let key_existed = if key_slot.is_some() {
            TableKey::from_value(&key)
                .ok()
                .is_some_and(|tk| t.borrow().get_general(&tk).is_some())
        } else {
            false
        };
        let deleting = val.is_nil();
        self.container_write_barrier(table, prev, &mut val);
        t.borrow_mut().raw_set(key, val)?;
        if let Some(kid) = key_slot {
            if deleting {
                if key_existed {
                    self.heap.old.on_old_release(kid);
                }
            } else if !key_existed {
                self.heap.old.on_old_store(container_id, kid);
            }
        }
        if is_meta_key {
            self.refresh_meta_dependents(tbl_ptr);
        }
        Ok(())
    }

    /// Updates `table`'s metatable, managing Beat extra storage, Old edges,
    /// `__gc` flags, and weak-value registration.
    pub fn table_set_metatable(
        &mut self,
        table: &Value,
        mt: Option<Value>,
    ) -> Result<bool, String> {
        let mut mt_val = match mt {
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
                Some(v)
            }
        };

        if let Some(bt_ptr) = table.beat_table_ptr() {
            let mt_rc = mt_val.take().map(|mut v| {
                self.escape_value_to_job(&mut v, self.heap.jobs.current_job_id(), 0);
                v.as_table()
                    .expect("promoted metatable must be non-Beat")
                    .to_rc()
            });
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
        let container_id = OldArena::slot_of_value(table);
        let container_job = container_id
            .and_then(|cid| self.heap.old.job_of(cid))
            .unwrap_or(0);

        if let Some(ref mut v) = mt_val
            && let Some(mid) = OldArena::slot_of_value(v)
            && let Some(mj) = self.heap.old.job_of(mid)
            && mj != 0
            && mj != container_job
        {
            self.heap
                .jobs
                .escape_subgraph(&mut self.heap.old, mid, mj, container_job);
        }

        let old_mt_id = t
            .borrow()
            .metatable
            .as_ref()
            .and_then(|m| SlotId::unpack(m.borrow().arena_id));

        let mt_rc = mt_val.map(|v| {
            v.as_table()
                .expect("promoted metatable must be non-Beat")
                .to_rc()
        });
        let new_mt_id = mt_rc
            .as_ref()
            .and_then(|m| SlotId::unpack(m.borrow().arena_id));

        t.borrow_mut().metatable = mt_rc;

        if container_job == 0 {
            if let Some(omid) = old_mt_id
                && self.heap.old.job_of(omid) == Some(0)
            {
                self.heap.old.on_old_release(omid);
            }
            if let Some(nmid) = new_mt_id
                && self.heap.old.job_of(nmid) == Some(0)
            {
                self.heap.old.on_old_store(container_id, nmid);
            }
        }
        if let Some(cid) = container_id {
            self.heap.old.refresh_meta(cid);
        }
        Ok(true)
    }

    /// Write barrier for writing `new_val` into `container` (where `prev_val`
    /// is the replaced slot value if any): escapes Beat/`Job` values into
    /// `container`'s region, converts weak-value writes into `Look`, and tracks
    /// `Old` ownership transitions.
    pub fn container_write_barrier(
        &mut self,
        container: &Value,
        prev_val: Option<Value>,
        new_val: &mut Value,
    ) {
        self.beat_escape_if_needed(new_val);
        let container_id = OldArena::slot_of_value(container);
        let container_job = container_id
            .and_then(|cid| self.heap.old.job_of(cid))
            .unwrap_or(0);

        if let Some(child_id) = OldArena::slot_of_value(new_val) {
            if let Some(child_job) = self.heap.old.job_of(child_id)
                && child_job != 0
                && child_job != container_job
            {
                self.heap.jobs.escape_subgraph(
                    &mut self.heap.old,
                    child_id,
                    child_job,
                    container_job,
                );
            }
            let is_weak_v = container
                .as_table()
                .is_some_and(|t| table_is_weak_v(&t.borrow()));
            if is_weak_v {
                *new_val = Value::look(child_id.slot, child_id.epoch);
            } else if container_job == 0 {
                self.heap.old.on_old_store(container_id, child_id);
            }
        } else if new_val.is_closure() && container_job == 0 {
            self.escape_value_to_job(new_val, self.heap.jobs.current_job_id(), 0);
            if let Some(cid) = container_id {
                self.heap.old.mark_has_old_children(cid);
            }
        }

        if container_job == 0
            && let Some(prev) = prev_val
            && let Some(old_cid) = OldArena::slot_of_value(&prev)
            && self.heap.old.job_of(old_cid) == Some(0)
        {
            self.heap.old.on_old_release(old_cid);
        }
    }

    /// Write barrier for `vm.globals` stores/overwrites.
    #[inline]
    pub fn global_write_barrier(&mut self, prev: Option<Value>, new_val: &mut Value) {
        let cur_job = self.heap.jobs.current_job_id();
        self.escape_value_to_job(new_val, cur_job, 0);
        if let Some(cid) = OldArena::slot_of_value(new_val)
            && self.heap.old.job_of(cid) == Some(0)
        {
            self.heap.old.on_old_store(None, cid);
        }
        if let Some(old) = prev
            && let Some(old_cid) = OldArena::slot_of_value(&old)
            && self.heap.old.job_of(old_cid) == Some(0)
        {
            self.heap.old.on_old_release(old_cid);
        }
    }

    /// Promotes `val` (Beat, `Job` table, or closed upvalues in a `Closure`) from
    /// `from_job` out into `to_job` (`0` = `Old`).
    pub fn escape_value_to_job(&mut self, val: &mut Value, from_job: u32, to_job: u32) {
        if val.is_beat() {
            // Beat promotion creates in `current_job_id()`; temporarily ensure target job.
            if self.heap.jobs.current_job_id() != to_job && to_job != 0 {
                self.heap.jobs.push_active(to_job);
                self.beat_promote_value(val);
                self.heap.jobs.pop_active();
            } else {
                let mut popped = Vec::new();
                while to_job == 0 && self.heap.jobs.current_job_id() != 0 {
                    if let Some(j) = self.heap.jobs.pop_active() {
                        popped.push(j);
                    }
                }
                self.beat_promote_value(val);
                for j in popped.into_iter().rev() {
                    self.heap.jobs.push_active(j);
                }
            }
        }
        if let Some(cid) = OldArena::slot_of_value(val) {
            if let Some(cur_job) = self.heap.old.job_of(cid)
                && cur_job != 0
                && (cur_job == from_job || to_job == 0 || cur_job > to_job)
            {
                self.heap
                    .jobs
                    .escape_subgraph(&mut self.heap.old, cid, cur_job, to_job);
            }
            return;
        }
        if let ValueView::Closure(c) = val.view() {
            let root_cls = c.to_rc();
            let mut pending = vec![root_cls];
            let mut seen: FxHashSet<*const VmClosure> = FxHashSet::default();
            while let Some(cls) = pending.pop() {
                if !seen.insert(Rc::as_ptr(&cls)) {
                    continue;
                }
                for uv in &cls.upvalues {
                    if uv.is_open() {
                        continue;
                    }
                    let mut inner = uv.get_closed();
                    let changed = inner.is_beat()
                        || OldArena::slot_of_value(&inner)
                            .and_then(|cid| self.heap.old.job_of(cid))
                            .is_some_and(|j| j != 0 && (j == from_job || to_job == 0));
                    if changed {
                        self.escape_value_to_job(&mut inner, from_job, to_job);
                        if to_job == 0
                            && let Some(cid) = OldArena::slot_of_value(&inner)
                        {
                            self.heap.old.record_upvalue_edge(uv, cid);
                        }
                        uv.set_closed(inner);
                    } else if let ValueView::Closure(sub) = inner.view() {
                        pending.push(sub.to_rc());
                    }
                }
            }
        }
    }

    #[inline]
    pub fn resolve_look_value(&self, val: &Value) -> Value {
        if let Some(id) = val.look_slot_id()
            && let Some(rc) = self.heap.old.resolve_look(id)
        {
            return Value::arena_table(Rc::as_ptr(&rc));
        }
        Value::NIL
    }

    #[inline]
    pub fn unwrap_weak_read_val(&self, table: &Value, val: Value) -> Value {
        if val.is_look()
            && table
                .table_borrow()
                .is_some_and(|t| t.metatable().is_some_and(|m| table_is_weak_v(&m.borrow())))
        {
            return self.resolve_look_value(&val);
        }
        val
    }

    fn refresh_meta_dependents(&mut self, mt_ptr: *const RefCell<VmTable>) {
        let len = self.heap.old.slots.len();
        for idx in 0..len {
            let slot = &self.heap.old.slots[idx];
            if !slot.is_live() {
                continue;
            }
            let Some(rc) = slot.table.as_ref() else {
                continue;
            };
            let matches = {
                let b = rc.borrow();
                b.metatable
                    .as_ref()
                    .is_some_and(|m| std::ptr::eq(Rc::as_ptr(m), mt_ptr))
            };
            if matches {
                let id = SlotId {
                    slot: idx as u32,
                    epoch: slot.epoch,
                };
                self.heap.old.refresh_meta(id);
            }
        }
    }

    /// Collects direct `Old` slots reachable from live stack frames, open
    /// upvalues, globals, modules, `returned`, and suspended coroutines
    /// (including `Old` slots stored inside live `BeatTable`s).
    pub fn collect_root_pins(&self) -> FxHashSet<u32> {
        let mut pins = FxHashSet::default();
        let mut seen_closures: FxHashSet<*const VmClosure> = FxHashSet::default();
        let mut seen_beat: FxHashSet<*const BeatTable> = FxHashSet::default();
        let beat = &self.heap.beat;

        let stack_len = self.stack.len();
        if self.frames.is_empty() {
            for v in &self.stack {
                mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
            }
        } else {
            for (fi, f) in self.frames.iter().enumerate() {
                let start = f.base.saturating_sub(1);
                let mut end = f.base + f.closure.proto.max_registers as usize;
                if let Some(next) = self.frames.get(fi + 1) {
                    end = end.min(next.base);
                } else {
                    end = end.max(self.top);
                }
                let end = end.min(stack_len);
                for v in &self.stack[start.min(end)..end] {
                    mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
                }
            }
        }
        for &(idx, _) in &self.open_upvalues {
            if let Some(v) = self.stack.get(idx) {
                mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
            }
        }
        for caller in &self.suspended_callers {
            let c_len = caller.stack.len();
            if caller.frames.is_empty() {
                for v in &caller.stack {
                    mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
                }
            } else {
                for (fi, f) in caller.frames.iter().enumerate() {
                    let start = f.base.saturating_sub(1);
                    let mut end = f.base + f.closure.proto.max_registers as usize;
                    if let Some(next) = caller.frames.get(fi + 1) {
                        end = end.min(next.base);
                    }
                    let end = end.min(c_len);
                    for v in &caller.stack[start.min(end)..end] {
                        mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
                    }
                }
            }
            for (idx, uv) in &caller.open_upvalues {
                if !uv.is_open() {
                    uv.with_closed(|v| {
                        mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
                    });
                } else if let Some(v) = caller.stack.get(*idx) {
                    mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
                }
            }
        }
        for v in &self.returned {
            mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
        }
        for v in self.globals.values() {
            mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
        }
        for v in self.modules.values() {
            mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
        }
        self.tasks.visit_roots(|v| {
            mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
        });
        for weak in self.co_states.values() {
            if let Some(co_rc) = weak.upgrade()
                && let Ok(cs) = co_rc.try_borrow()
            {
                mark_pin_val(
                    &cs.func,
                    beat,
                    &mut pins,
                    &mut seen_closures,
                    &mut seen_beat,
                );
                for v in &cs.stack {
                    mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
                }
                for v in &cs.yield_values {
                    mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
                }
                for (_, uv) in &cs.open_upvalues {
                    if !uv.is_open() {
                        uv.with_closed(|v| {
                            mark_pin_val(v, beat, &mut pins, &mut seen_closures, &mut seen_beat);
                        });
                    }
                }
            }
        }
        pins
    }

    pub fn flush_pending_old(&mut self, include_all_unowned: bool) -> Option<String> {
        let pins = self.collect_root_pins();
        let plan = self.heap.old.flush_pending(&pins, include_all_unowned);
        self.run_old_reclaim_plan(plan)
    }

    pub(crate) fn run_old_reclaim_plan(&mut self, plan: ReclaimPlan) -> Option<String> {
        if plan.finalizers.is_empty() {
            return None;
        }
        let mut first_err = None;
        for (slot_id, rc_tbl, handler) in plan.finalizers {
            let arg = Value::arena_table(Rc::as_ptr(&rc_tbl));
            if let Err(e) = self.call_function(handler, std::slice::from_ref(&arg)) {
                first_err.get_or_insert(e);
            }
            drop(arg);
            let pins = self.collect_root_pins();
            self.heap.old.settle_finalized(slot_id, &pins);
        }
        first_err
    }

    /// Converts any `arena_table` references reachable from `val` (only used at
    /// top-level `VM::execute` exit) into owned `Rc`-backed `Value::table` handles
    /// so returned values remain valid even if the caller drops `VM` afterwards.
    pub(crate) fn detach_return_value(&self, val: &mut Value) {
        if !val.is_table() && !val.is_closure() {
            return;
        }
        let mut seen_tbl: FxHashSet<*const RefCell<VmTable>> = FxHashSet::default();
        let mut seen_cls: FxHashSet<*const VmClosure> = FxHashSet::default();
        detach_val_recursive(val, &mut seen_tbl, &mut seen_cls);
    }

    pub(crate) fn teardown_job_and_finalize(&mut self, job_id: u32) -> Option<String> {
        let plan = self.heap.jobs.teardown_job(&mut self.heap.old, job_id);
        let err = self.run_old_reclaim_plan(plan);
        if !self.heap.old.pending_free.is_empty() {
            let err2 = self.flush_pending_old(false);
            return err.or(err2);
        }
        err
    }
}

fn mark_pin_val(
    v: &Value,
    beat: &BeatArena,
    pins: &mut FxHashSet<u32>,
    seen_closures: &mut FxHashSet<*const VmClosure>,
    seen_beat: &mut FxHashSet<*const BeatTable>,
) {
    // Rewound/stale stack slots inside `[base..max_registers]` may still hold
    // a raw BIT_BEAT pointer past `beat.cursor()`; never dereference it.
    if let Some(raw_ptr) = v.raw_beat_table_ptr()
        && !beat.is_live_ptr(raw_ptr as *const u8)
    {
        return;
    }
    if let Some(cid) = OldArena::slot_of_value(v) {
        pins.insert(cid.slot);
    } else if let Some(bt_ptr) = v.beat_table_ptr() {
        if !seen_beat.insert(bt_ptr) {
            return;
        }
        let bt = unsafe { &*bt_ptr };
        // Always traverse nested BeatTables even when `!bt.has_old_refs`:
        // a BeatTable child may later be promoted/forwarded into `Old` while
        // the enclosing BeatTable stays in `Beat`.
        for elem in bt.array_slice() {
            mark_pin_val(elem, beat, pins, seen_closures, seen_beat);
        }
        if let Some(ext) = bt.extra_ref() {
            for fv in ext.fields.values() {
                mark_pin_val(fv, beat, pins, seen_closures, seen_beat);
            }
            if let Some(gm) = &ext.general {
                for (gk, gv) in gm.values() {
                    mark_pin_val(gk, beat, pins, seen_closures, seen_beat);
                    mark_pin_val(gv, beat, pins, seen_closures, seen_beat);
                }
            }
            if let Some(mt) = &ext.metatable
                && let Some(mid) = SlotId::unpack(mt.borrow().arena_id)
            {
                pins.insert(mid.slot);
            }
        } else {
            for f in bt.fields_slice() {
                mark_pin_val(&f.key, beat, pins, seen_closures, seen_beat);
                mark_pin_val(&f.val, beat, pins, seen_closures, seen_beat);
            }
        }
    } else if let ValueView::Closure(c) = v.view() {
        let mut q = vec![c.to_rc()];
        while let Some(cur) = q.pop() {
            if !seen_closures.insert(Rc::as_ptr(&cur)) {
                continue;
            }
            for uv in &cur.upvalues {
                if !uv.is_open() {
                    uv.with_closed(|iv| {
                        mark_pin_val(iv, beat, pins, seen_closures, seen_beat);
                    });
                }
            }
        }
    }
}

fn detach_val_recursive(
    val: &mut Value,
    seen_tbl: &mut FxHashSet<*const RefCell<VmTable>>,
    seen_cls: &mut FxHashSet<*const VmClosure>,
) {
    if val.is_arena_table()
        && let Some(rc) = val.to_table()
    {
        *val = Value::table(rc);
    }
    if let Some(t) = val.as_table() {
        let ptr = t.as_ptr();
        if !seen_tbl.insert(ptr) {
            return;
        }
        let rc = t.to_rc();
        if let Ok(mut b) = rc.try_borrow_mut() {
            for elem in &mut b.array {
                detach_val_recursive(elem, seen_tbl, seen_cls);
            }
            for fv in b.fields.values_mut() {
                detach_val_recursive(fv, seen_tbl, seen_cls);
            }
            if let Some(gen_map) = &mut b.general {
                for (_, v) in gen_map.values_mut() {
                    detach_val_recursive(v, seen_tbl, seen_cls);
                }
            }
        }
    } else if let Some(c) = val.as_closure() {
        let ptr = c.as_ptr();
        if !seen_cls.insert(ptr) {
            return;
        }
        let rc = c.to_rc();
        for uv in &rc.upvalues {
            if !uv.is_open() {
                let mut inner = uv.get_closed();
                detach_val_recursive(&mut inner, seen_tbl, seen_cls);
                uv.set_closed(inner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::proto::Proto;

    #[test]
    fn alloc_helpers_create_expected_values() {
        let mut vm = VM::new();
        let before_allocs = vm.heap.old.total_allocations;

        let t = vm.alloc_table_cap(2, 2);
        assert!(t.is_table());
        assert!(t.is_arena_table());
        assert_eq!(vm.heap.old.total_allocations, before_allocs + 1);

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
    fn table_set_metatable_updates_has_gc() {
        let mut vm = VM::new();
        let t = vm.alloc_table(VmTable::new());
        let mt = vm.alloc_table(VmTable::new());
        let func = vm.alloc_closure(Rc::new(Proto::new(None, 0, false)), Vec::new());
        assert!(vm.table_set_str(&mt, "__gc", func));

        assert!(vm.table_set_metatable(&t, Some(mt)).unwrap());
        let cid = OldArena::slot_of_value(&t).unwrap();
        assert!(vm.heap.old.slots[cid.slot as usize].has_gc);

        assert!(vm.table_set_metatable(&t, None).unwrap());
        assert!(!vm.heap.old.slots[cid.slot as usize].has_gc);
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
            loop_end: 0,
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
