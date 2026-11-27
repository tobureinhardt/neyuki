//! VM heap allocation and container mutation choke points.
//!
//! Stage 1-2 of the Memory Follows Work migration routes heap allocation and
//! container mutations through `VmHeap` / `VM`, adding the non-moving `Beat`
//! bump arena (`BeatArena`), nested Beat frames, write barriers, and lazy
//! evacuation to `Old` (`Rc` + `GcTracker` until Stage 3).

use std::cell::RefCell;
use std::rc::Rc;

use crate::bytecode::instruction::MULTRET;
use crate::bytecode::liveness::LoopSafepoint;
use crate::bytecode::proto::Proto;
use crate::vm::beat::{ActiveBeatLoop, BeatArena, BeatMark, BeatRemTarget, BeatTable};
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
            let rc = Rc::new(RefCell::new(VmTable::new()));
            self.gc.register_table(&rc);
            let old_val = Value::table(rc.clone());
            unsafe {
                (*bt_ptr).forwarded = old_val.bits();
            }
            // Pin one strong ref until `alloc_mark` rewinds so un-rewritten
            // BIT_BEAT aliases keep a live `forwarded` target even if the first
            // Old owner is overwritten or dropped mid-Beat.
            self.heap
                .beat
                .promoted_refs
                .push((alloc_mark, old_val.clone()));
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
                    if let Some(rc) = dead_tbl.to_table() {
                        self.gc.mark_already_finalized(&rc);
                    }
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
            let bits = self.stack[i].bits();
            if bits >= TAG_RC_START && (bits & BIT_BEAT) != 0 {
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

    /// Appends one element to an array table (Beat or Old with escape barrier).
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
        let Some(t) = tbl_val.as_table() else {
            return Err("cannot append to non-table".to_string());
        };
        let t_rc = t.to_rc();
        self.beat_escape_if_needed(&mut val);
        if let Some(slot_tbl) = self.stack.get(table_slot) {
            self.gc.write_barrier(slot_tbl, &val);
        }
        t_rc.borrow_mut().array.push(val);
        Ok(())
    }

    /// Appends stack slice `base+list .. count` into `base+table` (Beat or Old).
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
        let Some(t_rc) = self.stack[table_slot].as_table().map(|t| t.to_rc()) else {
            return Err("cannot SetList to non-table".to_string());
        };
        let mut items: Vec<Value> = self.stack[start..end].to_vec();
        for item in &mut items {
            self.beat_escape_if_needed(item);
            self.gc.write_barrier(&self.stack[table_slot], item);
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
        let had_gc = mt_rc
            .as_ref()
            .and_then(|m| m.borrow().fields.get("__gc").cloned())
            .is_some_and(|v| v.is_callable());
        {
            t.borrow_mut().metatable = mt_rc;
        }
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
