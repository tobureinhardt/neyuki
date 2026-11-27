//! `Old` region `SlotArena` (`Memory Follows Work` Stage 3).
//!
//! Replaces global mark-and-sweep tracing with slot-indexed ownership:
//! - `Unowned` (`rc == 0`): lives solely on registers / open upvalues / roots.
//! - `UniqueOwner` (`rc == 1`): single `Old` owner; overwriting/deleting the
//!   owner slot queues deferred subtree reclamation (`pending_free`) and runs
//!   `__gc` destructors deterministically newest-first.
//! - `Shared` (`rc >= 2`): promoted automatically on a second `Old` store (or
//!   explicitly via `share(x)`). When decremented while remaining `> 0`,
//!   enters the Bacon-Rajan `purple_buffer` for local cycle collection.
//! - `Look(slot, epoch)`: generational weak handle stored in `Value` or
//!   `__mode = "v"` weak tables; returns `None` (`nil`) once `epoch` advances.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use crate::vm::buffer::VmBuffer;
use crate::vm::hash::{FxHashMap, FxHashSet};
use crate::vm::value::{Upvalue, Value, ValueView, VmClosure, VmTable};

pub(crate) const MAX_LOOK_EPOCH: u16 = 0x7FFF;
const BASE_TABLE_BYTES: usize = 64;
const MAX_POOLED_PER_CLASS: usize = 256;
const MAX_POOL_ARRAY_CAP: usize = 4096;
const MAX_POOL_FIELD_CAP: usize = 256;

/// Packed generational slot identifier `(slot, epoch)`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SlotId {
    pub slot: u32,
    pub epoch: u16,
}

impl SlotId {
    #[inline(always)]
    pub const fn pack(self) -> u64 {
        ((self.epoch as u64) << 32) | ((self.slot as u64) + 1)
    }

    #[inline(always)]
    pub fn unpack(arena_id: u64) -> Option<Self> {
        if arena_id == 0 {
            return None;
        }
        let low = arena_id as u32;
        if low == 0 {
            return None;
        }
        let slot = low - 1;
        let epoch = (arena_id >> 32) as u16;
        if epoch == 0 {
            return None;
        }
        Some(Self { slot, epoch })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ownership {
    Free,
    Unowned,
    UniqueOwner,
    Shared,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CycleColor {
    Black,
    Gray,
    White,
    Purple,
}

pub struct OldSlot {
    pub epoch: u16,
    pub ownership: Ownership,
    pub rc: u32,
    pub color: CycleColor,
    pub buffered: bool,
    pub is_suspect: bool,
    pub has_old_children: bool,
    pub in_pending_free: bool,
    pub has_gc: bool,
    pub is_weak_v: bool,
    pub finalized: bool,
    pub alloc_id: u64,
    pub job_id: u32,
    pub approx_bytes: usize,
    pub table: Option<Rc<RefCell<VmTable>>>,
}

impl OldSlot {
    #[inline(always)]
    pub fn is_live(&self) -> bool {
        self.ownership != Ownership::Free && self.table.is_some()
    }
}

/// Work batch produced by [`OldArena::collect_cycles`] or [`OldArena::flush_pending`]
/// for the VM driver to run `__gc` finalizers in reverse allocation order.
#[derive(Default)]
pub struct ReclaimPlan {
    /// Finalizer handlers to invoke (`(slot_id, table_rc, __gc_handler)`),
    /// ordered newest-first (`alloc_id` descending).
    pub finalizers: Vec<(SlotId, Rc<RefCell<VmTable>>, Value)>,
    /// Slots whose finalizers were deferred so they remain pinned until settled.
    pub finalizing_slots: Vec<SlotId>,
    /// Total bytes freed eagerly in this pass.
    pub bytes_freed: usize,
}

/// Size-classed table capacity pool (small / medium / large) to reuse inner
/// `Vec`/`FieldMap` storage on immediate `UniqueOwner` subtree reclamation.
#[derive(Default)]
struct TablePool {
    small: Vec<VmTable>,
    medium: Vec<VmTable>,
    large: Vec<VmTable>,
}

impl TablePool {
    fn class_for(array_cap: usize, fields_cap: usize) -> usize {
        let total = array_cap + fields_cap;
        if total <= 16 {
            0
        } else if total <= 128 {
            1
        } else {
            2
        }
    }

    fn take(&mut self, array_cap: usize, fields_cap: usize) -> VmTable {
        let cls = Self::class_for(array_cap, fields_cap);
        let bucket = match cls {
            0 => &mut self.small,
            1 => &mut self.medium,
            _ => &mut self.large,
        };
        if let Some(mut tbl) = bucket.pop() {
            if tbl.array.capacity() < array_cap {
                tbl.array.reserve(array_cap - tbl.array.len());
            }
            if tbl.fields.capacity() < fields_cap {
                tbl.fields.reserve_at_least(fields_cap);
            }
            tbl.arena_id = 0;
            tbl
        } else {
            VmTable::with_capacity(array_cap, fields_cap)
        }
    }

    fn recycle(&mut self, mut tbl: VmTable) {
        if tbl.array.capacity() > MAX_POOL_ARRAY_CAP || tbl.fields.capacity() > MAX_POOL_FIELD_CAP {
            return;
        }
        let cls = Self::class_for(tbl.array.capacity(), tbl.fields.capacity());
        let bucket = match cls {
            0 => &mut self.small,
            1 => &mut self.medium,
            _ => &mut self.large,
        };
        if bucket.len() >= MAX_POOLED_PER_CLASS {
            return;
        }
        tbl.array.clear();
        tbl.fields.clear();
        tbl.general = None;
        tbl.array_peak = 0;
        tbl.frozen = false;
        tbl.metatable = None;
        tbl.co_state = None;
        tbl.arena_id = 0;
        bucket.push(tbl);
    }
}

/// Slot-indexed arena managing `Old` mutable tables (`job_id == 0`) and `Job`
/// region tables (`job_id > 0`).
pub struct OldArena {
    pub slots: Vec<OldSlot>,
    free_slots: Vec<u32>,
    pool: TablePool,
    pub next_alloc_id: u64,

    pub bytes_allocated: usize,
    pub total_allocations: usize,
    pub threshold: usize,
    pub step_multiplier: usize,
    pub pause_multiplier: usize,
    pub is_running: bool,
    pub last_freed: usize,

    /// Deferred `rc == 0` candidates queued when an `Old` owner slot drops to 0.
    pub pending_free: Vec<SlotId>,
    /// Newly created `Unowned` (`rc == 0`) slots held only on stack/registers.
    pub unowned_live: Vec<SlotId>,
    /// Bacon-Rajan `Purple` buffer: `Shared` nodes whose `rc` decremented while remaining `> 0`.
    pub purple_buffer: Vec<SlotId>,
    /// Potential `0 -> 1` cycle entry points (node already owning `Old` children stored into `Old`).
    pub suspect_roots: Vec<SlotId>,
    /// Tables with weak-value metatables (`__mode` containing `'v'`).
    pub weak_tables: Vec<SlotId>,
    /// Closed upvalues currently holding an `Old` table slot, to detect closure drop.
    pub upvalue_edges: Vec<(Weak<Upvalue>, SlotId)>,
    /// Live buffers tracked for memory accounting.
    pub buffers: Vec<(Weak<RefCell<VmBuffer>>, usize)>,
    /// External Rust pins (`ffi/ext.rs` handles keeping a table alive across FFI).
    pub ext_pins: FxHashMap<u32, u32>,
    /// Slots currently kept alive while their `__gc` finalizer executes.
    finalizing_live: FxHashSet<u32>,
}

impl Default for OldArena {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for OldArena {
    fn drop(&mut self) {
        // Clear container fields before dropping `Rc`s when no external `Rc`
        // (such as a `Value` returned from `VM::execute`) outlives the arena,
        // breaking arbitrary internal cycles cleanly without recursive drop overflows.
        for slot in &mut self.slots {
            if let Some(rc) = &slot.table
                && Rc::strong_count(rc) == 1
                && let Ok(mut tbl) = rc.try_borrow_mut()
            {
                tbl.array.clear();
                tbl.fields.clear();
                tbl.general = None;
                tbl.metatable = None;
                tbl.co_state = None;
                tbl.arena_id = 0;
            }
        }
        self.slots.clear();
    }
}

impl OldArena {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            free_slots: Vec::new(),
            pool: TablePool::default(),
            next_alloc_id: 1,
            bytes_allocated: 0,
            total_allocations: 0,
            threshold: 1024 * 1024,
            step_multiplier: 200,
            pause_multiplier: 200,
            is_running: true,
            last_freed: 0,
            pending_free: Vec::new(),
            unowned_live: Vec::new(),
            purple_buffer: Vec::new(),
            suspect_roots: Vec::new(),
            weak_tables: Vec::new(),
            upvalue_edges: Vec::new(),
            buffers: Vec::new(),
            ext_pins: FxHashMap::default(),
            finalizing_live: FxHashSet::default(),
        }
    }

    pub fn stop(&mut self) {
        self.is_running = false;
    }

    pub fn restart(&mut self) {
        self.is_running = true;
    }

    pub fn count_kb(&self) -> f64 {
        self.bytes_allocated as f64 / 1024.0
    }

    #[inline]
    pub fn should_collect(&self) -> bool {
        self.is_running && self.bytes_allocated > self.threshold
    }

    #[inline]
    pub fn has_pending_work(&self) -> bool {
        !self.pending_free.is_empty() || !self.purple_buffer.is_empty()
    }

    /// Approximate byte footprint used for GC pacing & `heap_limit_bytes`.
    pub fn table_bytes(t: &VmTable) -> usize {
        BASE_TABLE_BYTES
            + t.array.capacity() * std::mem::size_of::<Value>()
            + t.fields.len() * 24
            + t.general.as_ref().map_or(0, |g| g.len() * 32)
    }

    #[inline]
    fn next_epoch(epoch: u16) -> u16 {
        if epoch >= MAX_LOOK_EPOCH || epoch == 0 {
            1
        } else {
            epoch + 1
        }
    }

    #[inline]
    pub fn slot_id_of_table(t: &VmTable) -> Option<SlotId> {
        SlotId::unpack(t.arena_id)
    }

    #[inline]
    pub fn slot_of_value(val: &Value) -> Option<SlotId> {
        let t = val.as_table()?;
        let id = t.borrow().arena_id;
        SlotId::unpack(id)
    }

    /// Allocates an empty table with requested capacities into `job_id`
    /// (`0` for global `Old`), reusing pooled `VmTable` buffers when available.
    pub fn alloc_with_capacity(
        &mut self,
        array_cap: usize,
        fields_cap: usize,
        job_id: u32,
    ) -> (SlotId, Rc<RefCell<VmTable>>) {
        let tbl = self.pool.take(array_cap, fields_cap);
        self.insert_table(tbl, job_id)
    }

    /// Registers a populated `VmTable` into the arena under `job_id` (`0` = `Old`).
    pub fn insert_table(
        &mut self,
        mut tbl: VmTable,
        job_id: u32,
    ) -> (SlotId, Rc<RefCell<VmTable>>) {
        let approx_bytes = Self::table_bytes(&tbl);
        let has_gc = table_gc_handler(&tbl).is_some();
        let is_weak_v = table_is_weak_v(&tbl);
        let has_old_children = table_has_any_old_child(&tbl);

        let slot_idx = if let Some(free_idx) = self.free_slots.pop() {
            free_idx
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(OldSlot {
                epoch: 1,
                ownership: Ownership::Free,
                rc: 0,
                color: CycleColor::Black,
                buffered: false,
                is_suspect: false,
                has_old_children: false,
                in_pending_free: false,
                has_gc: false,
                is_weak_v: false,
                finalized: false,
                alloc_id: 0,
                job_id: 0,
                approx_bytes: 0,
                table: None,
            });
            idx
        };

        let epoch = self.slots[slot_idx as usize].epoch.max(1);
        let id = SlotId {
            slot: slot_idx,
            epoch,
        };
        tbl.arena_id = id.pack();
        let rc = Rc::new(RefCell::new(tbl));
        let alloc_id = self.next_alloc_id;
        self.next_alloc_id += 1;

        let slot = &mut self.slots[slot_idx as usize];
        slot.ownership = Ownership::Unowned;
        slot.rc = 0;
        slot.color = CycleColor::Black;
        slot.buffered = false;
        slot.is_suspect = false;
        slot.has_old_children = has_old_children;
        slot.in_pending_free = false;
        slot.has_gc = has_gc;
        slot.is_weak_v = is_weak_v;
        slot.finalized = false;
        slot.alloc_id = alloc_id;
        slot.job_id = job_id;
        slot.approx_bytes = approx_bytes;
        slot.table = Some(rc.clone());

        self.bytes_allocated = self.bytes_allocated.saturating_add(approx_bytes);
        self.total_allocations = self.total_allocations.saturating_add(1);
        if job_id == 0 {
            self.unowned_live.push(id);
        }
        if is_weak_v {
            self.weak_tables.push(id);
        }
        (id, rc)
    }

    pub fn register_buffer(&mut self, buf: &Rc<RefCell<VmBuffer>>) {
        let sz = 32 + buf.borrow().data.capacity();
        self.bytes_allocated = self.bytes_allocated.saturating_add(sz);
        self.total_allocations = self.total_allocations.saturating_add(1);
        self.buffers.push((Rc::downgrade(buf), sz));
    }

    pub fn reap_dead_buffers(&mut self) -> usize {
        let mut freed = 0;
        self.buffers.retain(|(weak, sz)| {
            if weak.strong_count() == 0 {
                self.bytes_allocated = self.bytes_allocated.saturating_sub(*sz);
                freed += *sz;
                false
            } else {
                true
            }
        });
        freed
    }

    /// Resolves a `(slot, epoch)` `Look` handle if its target is still alive.
    #[inline]
    pub fn resolve_look(&self, id: SlotId) -> Option<Rc<RefCell<VmTable>>> {
        let slot = self.slots.get(id.slot as usize)?;
        if slot.epoch == id.epoch && slot.is_live() {
            slot.table.clone()
        } else {
            None
        }
    }

    #[inline]
    pub fn is_alive(&self, id: SlotId) -> bool {
        self.slots
            .get(id.slot as usize)
            .is_some_and(|s| s.epoch == id.epoch && s.is_live())
    }

    #[inline]
    pub fn job_of(&self, id: SlotId) -> Option<u32> {
        let slot = self.slots.get(id.slot as usize)?;
        (slot.epoch == id.epoch && slot.is_live()).then_some(slot.job_id)
    }

    /// Explicitly marks a live slot as `Shared` (for `share(x)` builtin).
    pub fn promote_to_shared(&mut self, id: SlotId) -> bool {
        let Some(slot) = self.slots.get_mut(id.slot as usize) else {
            return false;
        };
        if slot.epoch != id.epoch || !slot.is_live() {
            return false;
        }
        if slot.rc < 2 {
            // Retain shared semantics; if `rc == 0` it still stays collectible
            // only if unreferenced, or `Shared` once an `Old` edge owns it.
            if slot.ownership == Ownership::UniqueOwner {
                slot.ownership = Ownership::Shared;
            }
        } else {
            slot.ownership = Ownership::Shared;
        }
        true
    }

    /// Records a write of `child_id` into an `Old`/root owner slot (`container_slot`
    /// is `Some(parent)` for a table/metatable field, `None` for globals/upvalues/etc.).
    pub fn on_old_store(&mut self, container_slot: Option<SlotId>, child_id: SlotId) {
        if let Some(parent) = container_slot
            && let Some(ps) = self.slots.get_mut(parent.slot as usize)
            && ps.epoch == parent.epoch
            && ps.is_live()
        {
            ps.has_old_children = true;
        }

        let Some(cs) = self.slots.get_mut(child_id.slot as usize) else {
            return;
        };
        if cs.epoch != child_id.epoch || !cs.is_live() {
            return;
        }
        cs.rc = cs.rc.saturating_add(1);
        cs.in_pending_free = false;
        if cs.rc == 1 {
            cs.ownership = Ownership::UniqueOwner;
            let self_loop = container_slot.is_some_and(|p| p.slot == child_id.slot);
            if (self_loop || cs.has_old_children) && cs.job_id == 0 && !cs.is_suspect {
                cs.is_suspect = true;
                self.suspect_roots.push(child_id);
            }
        } else {
            cs.ownership = Ownership::Shared;
        }
        cs.color = CycleColor::Black;
    }

    /// Records that `container_slot` now holds a closure/external child that could
    /// participate in cycles (e.g. a `VmClosure` whose upvalues close back onto `Old`).
    pub fn mark_has_old_children(&mut self, id: SlotId) {
        if let Some(slot) = self.slots.get_mut(id.slot as usize)
            && slot.epoch == id.epoch
            && slot.is_live()
        {
            slot.has_old_children = true;
            if slot.ownership != Ownership::Unowned && slot.job_id == 0 && !slot.is_suspect {
                slot.is_suspect = true;
                self.suspect_roots.push(id);
            }
        }
    }

    /// Releases one `Old`/root ownership edge from `child_id`.
    /// - `rc: 1 -> 0`: transitions to `Unowned` and queues `pending_free`.
    /// - `rc > 1`: transitions/remains `Shared`/`UniqueOwner` and pushes onto
    ///   Bacon-Rajan `purple_buffer` if not already buffered.
    pub fn on_old_release(&mut self, child_id: SlotId) {
        let Some(cs) = self.slots.get_mut(child_id.slot as usize) else {
            return;
        };
        if cs.epoch != child_id.epoch || !cs.is_live() {
            return;
        }
        if cs.rc == 0 {
            if cs.job_id == 0 && !cs.in_pending_free {
                cs.in_pending_free = true;
                self.pending_free.push(child_id);
            }
            return;
        }
        cs.rc -= 1;
        if cs.rc == 0 {
            cs.ownership = Ownership::Unowned;
            if cs.job_id == 0 && !cs.in_pending_free {
                cs.in_pending_free = true;
                self.pending_free.push(child_id);
            }
        } else {
            cs.ownership = if cs.rc == 1 {
                Ownership::UniqueOwner
            } else {
                Ownership::Shared
            };
            if cs.job_id == 0 && !cs.buffered {
                cs.buffered = true;
                cs.color = CycleColor::Purple;
                self.purple_buffer.push(child_id);
            }
        }
    }

    /// Updates cached `__gc` / `__mode = "v"` metadata after mutating `table`
    /// or its metatable.
    pub fn refresh_meta(&mut self, id: SlotId) {
        let Some(slot) = self.slots.get(id.slot as usize) else {
            return;
        };
        if slot.epoch != id.epoch || !slot.is_live() {
            return;
        }
        let Some(rc) = slot.table.clone() else {
            return;
        };
        let (has_gc, is_weak_v) = {
            let tbl = rc.borrow();
            (table_gc_handler(&tbl).is_some(), table_is_weak_v(&tbl))
        };
        let was_weak_v = self.slots[id.slot as usize].is_weak_v;
        let container_job = self.slots[id.slot as usize].job_id;
        let mut downgraded = Vec::new();
        if is_weak_v && !was_weak_v {
            let mut tbl = rc.borrow_mut();
            for elem in &mut tbl.array {
                if let Some(cid) = Self::slot_of_value(elem)
                    && self.job_of(cid) == Some(container_job)
                {
                    downgraded.push(cid);
                    *elem = Value::look(cid.slot, cid.epoch);
                }
            }
            for fv in tbl.fields.values_mut() {
                if let Some(cid) = Self::slot_of_value(fv)
                    && self.job_of(cid) == Some(container_job)
                {
                    downgraded.push(cid);
                    *fv = Value::look(cid.slot, cid.epoch);
                }
            }
            if let Some(gen_map) = &mut tbl.general {
                for (_, v) in gen_map.values_mut() {
                    if let Some(cid) = Self::slot_of_value(v)
                        && self.job_of(cid) == Some(container_job)
                    {
                        downgraded.push(cid);
                        *v = Value::look(cid.slot, cid.epoch);
                    }
                }
            }
        }
        let slot_mut = &mut self.slots[id.slot as usize];
        slot_mut.has_gc = has_gc;
        if is_weak_v && !slot_mut.is_weak_v {
            slot_mut.is_weak_v = true;
            self.weak_tables.push(id);
        } else {
            slot_mut.is_weak_v = is_weak_v;
        }
        if container_job == 0 {
            for cid in downgraded {
                self.on_old_release(cid);
            }
        }
    }

    /// Records a closed upvalue holding `child_id` so closure drop or cycle scans
    /// track ownership cleanly.
    pub fn record_upvalue_edge(&mut self, uv: &Rc<Upvalue>, child_id: SlotId) {
        self.on_old_store(None, child_id);
        self.upvalue_edges.push((Rc::downgrade(uv), child_id));
    }

    /// Checks closed `upvalue_edges`, releasing ownership for any `Upvalue` whose
    /// owning closures have all dropped.
    pub fn reap_dead_upvalues(&mut self) {
        if self.upvalue_edges.is_empty() {
            return;
        }
        let mut released = Vec::new();
        self.upvalue_edges.retain(|(weak, child_id)| {
            if weak.strong_count() == 0 {
                released.push(*child_id);
                false
            } else {
                true
            }
        });
        for id in released {
            self.on_old_release(id);
        }
    }

    pub fn add_ext_pin(&mut self, id: SlotId) {
        if self.is_alive(id) {
            *self.ext_pins.entry(id.slot).or_insert(0) += 1;
        }
    }

    pub fn release_ext_pin(&mut self, id: SlotId) {
        if let Some(c) = self.ext_pins.get_mut(&id.slot) {
            if *c <= 1 {
                self.ext_pins.remove(&id.slot);
                if self
                    .slots
                    .get(id.slot as usize)
                    .is_some_and(|s| s.rc == 0 && s.job_id == 0)
                {
                    self.pending_free.push(id);
                }
            } else {
                *c -= 1;
            }
        }
    }

    /// Reclaims any queued `pending_free` / unowned `rc == 0` subtrees that are
    /// not pinned by active execution roots (`pinned_slots`), ordering any dying
    /// `__gc` destructors newest-first.
    pub fn flush_pending(
        &mut self,
        pinned_slots: &FxHashSet<u32>,
        include_all_unowned: bool,
    ) -> ReclaimPlan {
        self.reap_dead_upvalues();
        let mut plan = ReclaimPlan::default();
        plan.bytes_freed += self.reap_dead_buffers();

        let mut work = Vec::new();
        if !self.pending_free.is_empty() {
            work.extend(std::mem::take(&mut self.pending_free));
        }
        if include_all_unowned && !self.unowned_live.is_empty() {
            let unowned = std::mem::take(&mut self.unowned_live);
            let mut still_unowned = Vec::new();
            for id in unowned {
                if let Some(s) = self.slots.get(id.slot as usize)
                    && s.epoch == id.epoch
                    && s.is_live()
                    && s.rc == 0
                    && s.job_id == 0
                {
                    if pinned_slots.contains(&id.slot) || self.is_pinned(id.slot) {
                        still_unowned.push(id);
                    } else {
                        work.push(id);
                    }
                }
            }
            self.unowned_live = still_unowned;
        }

        if !work.is_empty() {
            self.reclaim_rc_zero_worklist(work, pinned_slots, &mut plan);
        }
        plan.finalizers.sort_unstable_by(|a, b| {
            let ka = self.slots.get(a.0.slot as usize).map_or(0, |s| s.alloc_id);
            let kb = self.slots.get(b.0.slot as usize).map_or(0, |s| s.alloc_id);
            kb.cmp(&ka)
        });
        self.last_freed = plan.bytes_freed;
        plan
    }

    /// Iteratively reclaims `rc == 0` trees starting from `work`.
    fn reclaim_rc_zero_worklist(
        &mut self,
        mut work: Vec<SlotId>,
        pinned_slots: &FxHashSet<u32>,
        plan: &mut ReclaimPlan,
    ) {
        let mut queued_fin: FxHashSet<u32> = FxHashSet::default();
        while let Some(id) = work.pop() {
            let ext_pinned = pinned_slots.contains(&id.slot) || self.is_pinned(id.slot);
            let finalizing =
                queued_fin.contains(&id.slot) || self.finalizing_live.contains(&id.slot);
            let Some(slot) = self.slots.get_mut(id.slot as usize) else {
                continue;
            };
            if slot.epoch != id.epoch || !slot.is_live() {
                continue;
            }
            slot.in_pending_free = false;
            if slot.rc > 0 || slot.job_id != 0 {
                continue;
            }
            if ext_pinned || finalizing {
                continue;
            }

            let Some(rc_tbl) = slot.table.as_ref().cloned() else {
                continue;
            };

            // Check dynamically for callable `__gc` if not yet finalized.
            if !slot.finalized
                && let Some(handler) = {
                    let b = rc_tbl.borrow();
                    table_gc_handler(&b)
                }
            {
                slot.finalized = true;
                self.finalizing_live.insert(id.slot);
                queued_fin.insert(id.slot);
                plan.finalizers.push((id, rc_tbl, handler));
                plan.finalizing_slots.push(id);
                continue;
            }

            // Plain reclamation: extract outgoing Old children, clear table
            // contents, free slot & pool buffers, then release children.
            let children = self.clear_and_free_slot(id, &rc_tbl, &mut plan.bytes_freed);
            for child in children {
                self.on_release_during_subtree(child, &mut work);
            }
        }
    }

    fn on_release_during_subtree(&mut self, child_id: SlotId, work: &mut Vec<SlotId>) {
        let Some(cs) = self.slots.get_mut(child_id.slot as usize) else {
            return;
        };
        if cs.epoch != child_id.epoch || !cs.is_live() || cs.rc == 0 {
            return;
        }
        cs.rc -= 1;
        if cs.rc == 0 {
            cs.ownership = Ownership::Unowned;
            if cs.job_id == 0 {
                work.push(child_id);
            }
        } else {
            cs.ownership = if cs.rc == 1 {
                Ownership::UniqueOwner
            } else {
                Ownership::Shared
            };
            if cs.job_id == 0 && !cs.buffered {
                cs.buffered = true;
                cs.color = CycleColor::Purple;
                self.purple_buffer.push(child_id);
            }
        }
    }

    fn clear_and_free_slot(
        &mut self,
        id: SlotId,
        rc_tbl: &Rc<RefCell<VmTable>>,
        bytes_freed: &mut usize,
    ) -> Vec<SlotId> {
        let job_id = self.slots[id.slot as usize].job_id;
        let children = extract_old_table_children(&rc_tbl.borrow(), job_id, self);
        let approx = self.slots[id.slot as usize].approx_bytes;
        self.bytes_allocated = self.bytes_allocated.saturating_sub(approx);
        *bytes_freed += approx;

        let slot = &mut self.slots[id.slot as usize];
        slot.ownership = Ownership::Free;
        slot.rc = 0;
        slot.color = CycleColor::Black;
        slot.buffered = false;
        slot.is_suspect = false;
        slot.has_old_children = false;
        slot.in_pending_free = false;
        slot.has_gc = false;
        slot.is_weak_v = false;
        slot.finalized = false;
        slot.approx_bytes = 0;
        slot.epoch = Self::next_epoch(slot.epoch);
        let taken = slot.table.take();
        self.free_slots.push(id.slot);

        if let Some(rc) = taken {
            if Rc::strong_count(&rc) == 1 {
                if let Ok(mut borrowed) = rc.try_borrow_mut() {
                    borrowed.array.clear();
                    borrowed.fields.clear();
                    borrowed.general = None;
                    borrowed.metatable = None;
                    borrowed.co_state = None;
                    borrowed.arena_id = 0;
                }
                if let Ok(inner) = Rc::try_unwrap(rc) {
                    self.pool.recycle(inner.into_inner());
                }
            } else if let Ok(mut borrowed) = rc.try_borrow_mut() {
                borrowed.arena_id = 0;
            }
        }

        children
    }

    /// Runs Bacon-Rajan local trial-deletion cycle collection seeded ONLY from
    /// `purple_buffer` and `suspect_roots` (never scans disjoint live Old objects).
    pub fn collect_cycles(&mut self, root_pinned: &FxHashSet<u32>) -> ReclaimPlan {
        self.reap_dead_upvalues();
        let mut plan = ReclaimPlan::default();
        plan.bytes_freed += self.reap_dead_buffers();

        // First flush any pending `rc == 0` subtrees and unreachable `unowned_live`.
        let pre = self.flush_pending(root_pinned, true);
        plan.bytes_freed += pre.bytes_freed;
        plan.finalizers.extend(pre.finalizers);
        plan.finalizing_slots.extend(pre.finalizing_slots);

        let mut roots = Vec::new();
        for id in std::mem::take(&mut self.purple_buffer) {
            if let Some(s) = self.slots.get_mut(id.slot as usize)
                && s.epoch == id.epoch
                && s.is_live()
            {
                s.buffered = false;
                if s.job_id == 0 && (s.color == CycleColor::Purple || s.rc > 0) {
                    roots.push(id);
                }
            }
        }
        for id in std::mem::take(&mut self.suspect_roots) {
            if let Some(s) = self.slots.get_mut(id.slot as usize)
                && s.epoch == id.epoch
                && s.is_live()
            {
                s.is_suspect = false;
                if s.job_id == 0 && s.rc > 0 {
                    roots.push(id);
                }
            }
        }

        if roots.is_empty() {
            self.prune_weak_tables();
            self.last_freed = plan.bytes_freed;
            return plan;
        }

        // Local subgraph trial-deletion over `roots`:
        // 1. Mark gray & count internal references in `trial_rc`.
        let mut trial_rc: FxHashMap<u32, i32> = FxHashMap::default();
        let mut visited: FxHashSet<u32> = FxHashSet::default();
        let mut closure_refs: FxHashMap<*const VmClosure, usize> = FxHashMap::default();
        let mut closure_objs: FxHashMap<*const VmClosure, Rc<VmClosure>> = FxHashMap::default();
        let mut gray_q: Vec<u32> = Vec::new();

        for r in &roots {
            if visited.insert(r.slot) {
                gray_q.push(r.slot);
                let base_rc = self.initial_trial_rc(r.slot, root_pinned);
                trial_rc.insert(r.slot, base_rc);
            }
        }

        let mut q_idx = 0;
        let mut processed_closures: FxHashSet<*const VmClosure> = FxHashSet::default();
        loop {
            while q_idx < gray_q.len() {
                let s_idx = gray_q[q_idx];
                q_idx += 1;
                let Some(rc_tbl) = self.slots.get(s_idx as usize).and_then(|s| s.table.clone())
                else {
                    continue;
                };
                let is_weak_v = self.slots[s_idx as usize].is_weak_v;
                let (child_slots, closures) = collect_table_refs(&rc_tbl.borrow(), is_weak_v, self);
                for c_slot in child_slots {
                    if !visited.contains(&c_slot) {
                        visited.insert(c_slot);
                        let base_rc = self.initial_trial_rc(c_slot, root_pinned);
                        trial_rc.insert(c_slot, base_rc);
                        gray_q.push(c_slot);
                    }
                    if let Some(trc) = trial_rc.get_mut(&c_slot) {
                        *trc -= 1;
                    }
                }
                for cls in closures {
                    let ptr = Rc::as_ptr(&cls);
                    closure_objs.entry(ptr).or_insert(cls);
                    *closure_refs.entry(ptr).or_insert(0) += 1;
                }
            }

            // Propagate internal edges through closures held strictly within
            // the gray subgraph (`closure_refs[c] == Rc::strong_count(c)`).
            let mut added = false;
            let closure_keys: Vec<*const VmClosure> = closure_objs.keys().copied().collect();
            for c_ptr in closure_keys {
                if processed_closures.contains(&c_ptr) {
                    continue;
                }
                let cls = closure_objs[&c_ptr].clone();
                // `closure_objs` and local `cls` together hold +2 strong refs while scanning.
                let live_ext = Rc::strong_count(&cls).saturating_sub(2);
                let internal = closure_refs.get(&c_ptr).copied().unwrap_or(0);
                if internal >= live_ext {
                    processed_closures.insert(c_ptr);
                    for uv in &cls.upvalues {
                        if uv.is_open() {
                            continue;
                        }
                        uv.with_closed(|val| match val.view() {
                            ValueView::Closure(inner) => {
                                let iptr = Rc::as_ptr(&inner.to_rc());
                                if let std::collections::hash_map::Entry::Vacant(e) =
                                    closure_objs.entry(iptr)
                                {
                                    e.insert(inner.to_rc());
                                    added = true;
                                }
                                *closure_refs.entry(iptr).or_insert(0) += 1;
                            }
                            ValueView::Table(t) => {
                                if let Some(old_tbl) = t.as_old()
                                    && let Some(cid) = SlotId::unpack(old_tbl.borrow().arena_id)
                                    && self.is_alive(cid)
                                {
                                    if !visited.contains(&cid.slot) {
                                        visited.insert(cid.slot);
                                        let base_rc = self.initial_trial_rc(cid.slot, root_pinned);
                                        trial_rc.insert(cid.slot, base_rc);
                                        gray_q.push(cid.slot);
                                        added = true;
                                    }
                                    if let Some(trc) = trial_rc.get_mut(&cid.slot) {
                                        *trc -= 1;
                                    }
                                }
                            }
                            _ => {}
                        });
                    }
                }
            }
            if !added && q_idx >= gray_q.len() {
                break;
            }
        }
        drop(closure_objs);

        // 2. Scan: any node with `trial_rc > 0` (external ref) stays Black and
        // preserves everything reachable from it.
        let mut black: FxHashSet<u32> = FxHashSet::default();
        let mut black_q: Vec<u32> = trial_rc
            .iter()
            .filter_map(|(&slot, &trc)| (trc > 0).then_some(slot))
            .collect();
        for &s in &black_q {
            black.insert(s);
        }
        let mut b_idx = 0;
        while b_idx < black_q.len() {
            let s_idx = black_q[b_idx];
            b_idx += 1;
            if let Some(rc_tbl) = self.slots.get(s_idx as usize).and_then(|s| s.table.clone()) {
                let is_weak_v = self.slots[s_idx as usize].is_weak_v;
                let (children, closures) = collect_table_refs(&rc_tbl.borrow(), is_weak_v, self);
                for c in children {
                    if trial_rc.contains_key(&c) && black.insert(c) {
                        black_q.push(c);
                    }
                }
                for cls in closures {
                    collect_closure_tables(&cls, &mut |cid: SlotId| {
                        if trial_rc.contains_key(&cid.slot) && black.insert(cid.slot) {
                            black_q.push(cid.slot);
                        }
                    });
                }
            }
        }

        // Keep any white slot that has an unfinalized `__gc` (plus its
        // dependencies) until settled so `__gc(self)` sees valid fields/metatable.
        let mut fin_candidates: Vec<(SlotId, Rc<RefCell<VmTable>>, Value, u64)> = Vec::new();
        let white_slots: Vec<u32> = gray_q
            .iter()
            .copied()
            .filter(|s| !black.contains(s))
            .collect();

        for &s_idx in &white_slots {
            let slot = &self.slots[s_idx as usize];
            if !slot.finalized
                && let Some(rc_tbl) = slot.table.as_ref()
                && let Some(handler) = table_gc_handler(&rc_tbl.borrow())
            {
                let id = SlotId {
                    slot: s_idx,
                    epoch: slot.epoch,
                };
                fin_candidates.push((id, rc_tbl.clone(), handler, slot.alloc_id));
            }
        }

        if !fin_candidates.is_empty() {
            // Preserve everything reachable from a dying object whose `__gc`
            // is queued in this cycle (e.g. `a.__gc` touches `a.other = b` and `mt`).
            let mut fin_keep: Vec<u32> =
                fin_candidates.iter().map(|(id, _, _, _)| id.slot).collect();
            let mut fk_idx = 0;
            while fk_idx < fin_keep.len() {
                let s_idx = fin_keep[fk_idx];
                fk_idx += 1;
                if black.insert(s_idx)
                    && let Some(rc_tbl) =
                        self.slots.get(s_idx as usize).and_then(|s| s.table.clone())
                {
                    let is_weak_v = self.slots[s_idx as usize].is_weak_v;
                    let (children, closures) =
                        collect_table_refs(&rc_tbl.borrow(), is_weak_v, self);
                    for c in children {
                        if !black.contains(&c) {
                            fin_keep.push(c);
                        }
                    }
                    for cls in closures {
                        collect_closure_tables(&cls, &mut |cid: SlotId| {
                            if !black.contains(&cid.slot) {
                                fin_keep.push(cid.slot);
                            }
                        });
                    }
                }
            }
            fin_candidates.sort_unstable_by_key(|a| std::cmp::Reverse(a.3));
            for (id, rc_tbl, handler, _) in fin_candidates {
                let s = &mut self.slots[id.slot as usize];
                s.finalized = true;
                self.finalizing_live.insert(id.slot);
                plan.finalizers.push((id, rc_tbl, handler));
                plan.finalizing_slots.push(id);
            }
        }

        // 3. Collect White: clear tables, decrement outgoing surviving `Black`
        // refs (pushing them to `purple` if needed), bump generation & pool!
        let mut black_decrements = Vec::new();
        for &s_idx in &white_slots {
            if black.contains(&s_idx) {
                continue;
            }
            let Some(slot) = self.slots.get(s_idx as usize) else {
                continue;
            };
            if !slot.is_live() {
                continue;
            }
            let id = SlotId {
                slot: s_idx,
                epoch: slot.epoch,
            };
            let Some(rc_tbl) = slot.table.clone() else {
                continue;
            };
            let outgoing = self.clear_and_free_slot(id, &rc_tbl, &mut plan.bytes_freed);
            for cid in outgoing {
                if !visited.contains(&cid.slot) || black.contains(&cid.slot) {
                    black_decrements.push(cid);
                }
            }
        }

        for cid in black_decrements {
            self.on_old_release(cid);
        }

        // Restore remaining Black nodes cleanly; if a node stayed alive solely
        // because an execution root (`root_pinned`) held an alias to a self-
        // sufficient heap cycle, keep it in `suspect_roots` for when the stack drops.
        for &s_idx in &black {
            if let Some(s) = self.slots.get_mut(s_idx as usize)
                && s.is_live()
            {
                s.color = CycleColor::Black;
                if root_pinned.contains(&s_idx) && s.rc > 0 && s.job_id == 0 && !s.is_suspect {
                    s.is_suspect = true;
                    self.suspect_roots.push(SlotId {
                        slot: s_idx,
                        epoch: s.epoch,
                    });
                }
            }
        }

        self.prune_weak_tables();
        self.last_freed = plan.bytes_freed;
        plan
    }

    /// Completes finalization for a table whose `__gc(tbl)` just finished.
    /// If not resurrected by `__gc` (`rc > 0` or `pinned_slots`), schedules it
    /// (and its dying children) for immediate reclaim without re-running `__gc`.
    pub fn settle_finalized(&mut self, id: SlotId, pinned_slots: &FxHashSet<u32>) -> usize {
        self.finalizing_live.remove(&id.slot);
        let ext_pinned = pinned_slots.contains(&id.slot) || self.is_pinned(id.slot);
        let Some(s) = self.slots.get_mut(id.slot as usize) else {
            return 0;
        };
        if s.epoch != id.epoch || !s.is_live() {
            return 0;
        }
        if s.rc > 0 || ext_pinned {
            if s.rc > 0 && !s.buffered {
                s.buffered = true;
                s.color = CycleColor::Purple;
                self.purple_buffer.push(id);
            }
            return 0;
        }
        // Not resurrected: reclaim immediately!
        let mut plan = ReclaimPlan::default();
        self.reclaim_rc_zero_worklist(vec![id], pinned_slots, &mut plan);
        plan.bytes_freed
    }

    /// Removes dead entries from all registered `__mode = "v"` weak tables
    /// (supporting late/dynamic `__mode = "v"` toggles as well as native `Look`).
    pub fn prune_weak_tables(&mut self) {
        if self.weak_tables.is_empty() {
            return;
        }
        let ids = std::mem::take(&mut self.weak_tables);
        let mut keep_weak = Vec::new();
        let mut downgraded = Vec::new();

        for id in ids {
            let Some(slot) = self.slots.get(id.slot as usize) else {
                continue;
            };
            if slot.epoch != id.epoch || !slot.is_live() {
                continue;
            }
            let Some(rc_tbl) = slot.table.clone() else {
                continue;
            };
            let is_weak = {
                let b = rc_tbl.borrow();
                table_is_weak_v(&b)
            };
            if !is_weak {
                if let Some(s) = self.slots.get_mut(id.slot as usize) {
                    s.is_weak_v = false;
                }
                continue;
            }
            keep_weak.push(id);

            let mut tbl = rc_tbl.borrow_mut();
            // Prune or downgrade weak array slots (note: trimming trailing nils).
            let mut trimmed = false;
            for elem in &mut tbl.array {
                if let Some(lid) = elem.look_slot_id() {
                    if !self.is_alive(lid) {
                        *elem = Value::NIL;
                        trimmed = true;
                    }
                } else if let Some(cid) = Self::slot_of_value(elem)
                    && self.job_of(cid) == Some(self.slots[id.slot as usize].job_id)
                {
                    if let Some(cs) = self.slots.get(cid.slot as usize)
                        && cs.rc <= 1
                        && !self.finalizing_live.contains(&cid.slot)
                    {
                        // Strong child whose only remaining strong edge is this late weak table:
                        downgraded.push(cid);
                        *elem = Value::NIL;
                        trimmed = true;
                    } else {
                        downgraded.push(cid);
                        *elem = Value::look(cid.slot, cid.epoch);
                    }
                }
            }
            if trimmed {
                while tbl.array.last().is_some_and(Value::is_nil) {
                    tbl.array.pop();
                }
            }

            let dead_fields: Vec<crate::vm::value::StrRef> = tbl
                .fields
                .iter()
                .filter_map(|(k, v)| {
                    v.look_slot_id()
                        .and_then(|lid| (!self.is_alive(lid)).then(|| k.clone()))
                })
                .collect();
            for k in dead_fields {
                tbl.fields.remove(&k.0.str);
            }

            if let Some(gen_map) = tbl.general.as_mut() {
                let mut dead_keys = Vec::new();
                for (tk, (k, v)) in gen_map.iter_mut() {
                    if let Some(lid) = v.look_slot_id()
                        && !self.is_alive(lid)
                    {
                        if let Some(kcid) = Self::slot_of_value(k)
                            && self.job_of(kcid) == Some(self.slots[id.slot as usize].job_id)
                        {
                            downgraded.push(kcid);
                        }
                        dead_keys.push(tk.clone());
                    }
                }
                for k in dead_keys {
                    gen_map.remove(&k);
                }
                if gen_map.is_empty() {
                    tbl.general = None;
                }
            }
        }
        self.weak_tables = keep_weak;
        for cid in downgraded {
            self.on_old_release(cid);
        }
    }

    #[inline]
    fn initial_trial_rc(&self, slot_idx: u32, root_pinned: &FxHashSet<u32>) -> i32 {
        let s = &self.slots[slot_idx as usize];
        let base = s.rc as i32;
        if root_pinned.contains(&slot_idx) || self.is_pinned(slot_idx) {
            base + 1
        } else {
            base
        }
    }

    #[inline]
    fn is_pinned(&self, slot_idx: u32) -> bool {
        self.ext_pins.contains_key(&slot_idx) || self.finalizing_live.contains(&slot_idx)
    }

    /// Reassigns `id`'s region to `new_job` (`0` = `Old`), recursively updating
    /// any children still belonging to `from_job`. Returns every promoted [`SlotId`].
    pub fn promote_job_subgraph(
        &mut self,
        root_id: SlotId,
        from_job: u32,
        new_job: u32,
    ) -> Vec<SlotId> {
        if from_job == new_job {
            return Vec::new();
        }
        let mut stack = vec![root_id];
        let mut promoted = Vec::new();
        while let Some(id) = stack.pop() {
            let Some(slot) = self.slots.get_mut(id.slot as usize) else {
                continue;
            };
            if slot.epoch != id.epoch || !slot.is_live() || slot.job_id != from_job {
                continue;
            }
            slot.job_id = new_job;
            let Some(rc_tbl) = slot.table.clone() else {
                continue;
            };
            promoted.push(id);
            let children = extract_old_table_children(&rc_tbl.borrow(), from_job, self);
            stack.extend(children);
        }
        if new_job == 0 {
            for &id in &promoted {
                let Some(rc_tbl) = self
                    .slots
                    .get(id.slot as usize)
                    .and_then(|s| s.table.clone())
                else {
                    continue;
                };
                if let Some(s) = self.slots.get(id.slot as usize)
                    && s.rc == 0
                {
                    self.unowned_live.push(id);
                }
                let children = {
                    let b = rc_tbl.borrow();
                    let is_weak = table_is_weak_v(&b);
                    collect_child_slot_ids(&b, is_weak, self)
                };
                for cid in children {
                    self.on_old_store(Some(id), cid);
                }
            }
        }
        promoted
    }

    /// Reclaims ALL slots still inside `job_id`, collecting finalizers in
    /// reverse allocation order (`alloc_id` descending).
    pub fn collect_job_exit(&mut self, job_id: u32, job_allocs: Vec<SlotId>) -> ReclaimPlan {
        let mut plan = ReclaimPlan::default();
        if job_allocs.is_empty() {
            return plan;
        }

        // First collect any callable `__gc` finalizers in reverse creation order
        // while tables remain intact so handlers can inspect their fields.
        let mut dying_plain = Vec::new();
        for id in job_allocs.into_iter().rev() {
            let Some(slot) = self.slots.get_mut(id.slot as usize) else {
                continue;
            };
            if slot.epoch != id.epoch || !slot.is_live() || slot.job_id != job_id {
                continue;
            }
            let handler_opt = slot
                .table
                .as_ref()
                .and_then(|rc| table_gc_handler(&rc.borrow()));
            if !slot.finalized
                && let Some(rc_tbl) = slot.table.as_ref().cloned()
                && let Some(handler) = handler_opt
            {
                slot.finalized = true;
                slot.job_id = 0; // Settles against `Old` if resurrected.
                self.finalizing_live.insert(id.slot);
                plan.finalizers.push((id, rc_tbl, handler));
                plan.finalizing_slots.push(id);
            } else {
                dying_plain.push(id);
            }
        }

        // Only free plain objects that don't have a queued finalizer in this job
        // keeping their child dependencies temporarily intact if any finalizer exists.
        if plan.finalizers.is_empty() {
            for id in dying_plain {
                let Some(rc_tbl) = self
                    .slots
                    .get(id.slot as usize)
                    .filter(|s| s.epoch == id.epoch && s.is_live() && s.job_id == job_id)
                    .and_then(|s| s.table.clone())
                else {
                    continue;
                };
                let outgoing = self.clear_and_free_slot(id, &rc_tbl, &mut plan.bytes_freed);
                for cid in outgoing {
                    if self.job_of(cid) == Some(0) {
                        self.on_old_release(cid);
                    }
                }
            }
        } else {
            // Re-home remaining job slots to `0` + pending_free so they reclaim cleanly
            // right after all dying `__gc` handlers execute in `settle_finalized`.
            for id in dying_plain {
                if let Some(s) = self.slots.get_mut(id.slot as usize)
                    && s.epoch == id.epoch
                    && s.is_live()
                    && s.job_id == job_id
                {
                    s.job_id = 0;
                    s.rc = 0;
                    s.ownership = Ownership::Unowned;
                    if !s.in_pending_free {
                        s.in_pending_free = true;
                        self.pending_free.push(id);
                    }
                }
            }
        }
        plan
    }

    pub fn update_threshold(&mut self) {
        let live_threshold = (self.bytes_allocated * self.pause_multiplier) / 100;
        self.threshold = live_threshold.max(1024 * 1024);
    }

    /// Number of currently live tables tracked in `OldArena`.
    pub fn live_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_live()).count()
    }
}

/// Callable `__gc` metatable entry on `tbl`, if any.
pub fn table_gc_handler(tbl: &VmTable) -> Option<Value> {
    let mt = tbl.metatable.as_ref()?;
    let handler = mt.borrow().fields.get("__gc").cloned()?;
    if handler.is_callable() {
        Some(handler)
    } else {
        None
    }
}

/// True when `tbl`'s metatable declares weak values (`__mode` contains `'v'`).
pub fn table_is_weak_v(tbl: &VmTable) -> bool {
    tbl.metatable
        .as_ref()
        .and_then(|mt| mt.borrow().fields.get("__mode").cloned())
        .and_then(|v| v.as_str().map(|s| s.contains('v')))
        .unwrap_or(false)
}

fn table_has_any_old_child(tbl: &VmTable) -> bool {
    let has_child_val =
        |v: &Value| -> bool { matches!(v.view(), ValueView::Table(_) | ValueView::Closure(_)) };
    if tbl.metatable.is_some()
        || tbl.array.iter().any(&has_child_val)
        || tbl.fields.values().any(&has_child_val)
    {
        return true;
    }
    if let Some(gen_map) = &tbl.general {
        return gen_map
            .values()
            .any(|(k, v)| has_child_val(k) || has_child_val(v));
    }
    false
}

fn push_slot_if_same_job(val: &Value, expected_job: u32, arena: &OldArena, out: &mut Vec<SlotId>) {
    if let Some(t) = val.as_table()
        && let Some(cid) = SlotId::unpack(t.borrow().arena_id)
        && arena.job_of(cid) == Some(expected_job)
    {
        out.push(cid);
    }
}

fn extract_old_table_children(tbl: &VmTable, expected_job: u32, arena: &OldArena) -> Vec<SlotId> {
    let mut out = Vec::new();
    let is_weak = table_is_weak_v(tbl);
    if !is_weak {
        for v in &tbl.array {
            push_slot_if_same_job(v, expected_job, arena, &mut out);
        }
        for v in tbl.fields.values() {
            push_slot_if_same_job(v, expected_job, arena, &mut out);
        }
    }
    if let Some(gen_map) = &tbl.general {
        for (k, v) in gen_map.values() {
            push_slot_if_same_job(k, expected_job, arena, &mut out);
            if !is_weak {
                push_slot_if_same_job(v, expected_job, arena, &mut out);
            }
        }
    }
    if let Some(mt) = &tbl.metatable
        && let Some(mid) = SlotId::unpack(mt.borrow().arena_id)
        && arena.job_of(mid) == Some(expected_job)
    {
        out.push(mid);
    }
    out
}

fn collect_child_slot_ids(tbl: &VmTable, is_weak_v: bool, arena: &OldArena) -> Vec<SlotId> {
    let mut out = Vec::new();
    if !is_weak_v {
        for v in &tbl.array {
            if let Some(cid) = OldArena::slot_of_value(v)
                && arena.is_alive(cid)
            {
                out.push(cid);
            }
        }
        for v in tbl.fields.values() {
            if let Some(cid) = OldArena::slot_of_value(v)
                && arena.is_alive(cid)
            {
                out.push(cid);
            }
        }
    }
    if let Some(gen_map) = &tbl.general {
        for (k, v) in gen_map.values() {
            if let Some(cid) = OldArena::slot_of_value(k)
                && arena.is_alive(cid)
            {
                out.push(cid);
            }
            if !is_weak_v
                && let Some(cid) = OldArena::slot_of_value(v)
                && arena.is_alive(cid)
            {
                out.push(cid);
            }
        }
    }
    if let Some(mt) = &tbl.metatable
        && let Some(mid) = SlotId::unpack(mt.borrow().arena_id)
        && arena.is_alive(mid)
    {
        out.push(mid);
    }
    out
}

fn collect_table_refs(
    tbl: &VmTable,
    is_weak_v: bool,
    arena: &OldArena,
) -> (Vec<u32>, Vec<Rc<VmClosure>>) {
    let mut child_slots = Vec::new();
    let mut closures = Vec::new();
    let mut visit_val = |v: &Value| {
        if let Some(cid) = OldArena::slot_of_value(v) {
            if arena.job_of(cid) == Some(0) {
                child_slots.push(cid.slot);
            }
        } else if let ValueView::Closure(c) = v.view() {
            closures.push(c.to_rc());
        }
    };
    if !is_weak_v {
        for v in &tbl.array {
            visit_val(v);
        }
        for v in tbl.fields.values() {
            visit_val(v);
        }
    }
    if let Some(gen_map) = &tbl.general {
        for (k, v) in gen_map.values() {
            visit_val(k);
            if !is_weak_v {
                visit_val(v);
            }
        }
    }
    if let Some(mt) = &tbl.metatable
        && let Some(mid) = SlotId::unpack(mt.borrow().arena_id)
        && arena.job_of(mid) == Some(0)
    {
        child_slots.push(mid.slot);
    }
    (child_slots, closures)
}

fn collect_closure_tables(root: &Rc<VmClosure>, mut on_slot: impl FnMut(SlotId)) {
    let mut pending = vec![root.clone()];
    let mut seen: FxHashSet<*const VmClosure> = FxHashSet::default();
    while let Some(c) = pending.pop() {
        if !seen.insert(Rc::as_ptr(&c)) {
            continue;
        }
        for uv in &c.upvalues {
            if uv.is_open() {
                continue;
            }
            uv.with_closed(|v| match v.view() {
                ValueView::Closure(inner) => pending.push(inner.to_rc()),
                ValueView::Table(t) => {
                    if let Some(old_tbl) = t.as_old()
                        && let Some(cid) = SlotId::unpack(old_tbl.borrow().arena_id)
                    {
                        on_slot(cid);
                    }
                }
                _ => {}
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_pins() -> FxHashSet<u32> {
        FxHashSet::default()
    }

    #[test]
    fn unique_owner_subtree_reclaims_immediately_with_reverse_gc() {
        let mut arena = OldArena::new();
        let (p_id, p_rc) = arena.alloc_with_capacity(2, 2, 0);
        let (c1_id, c1_rc) = arena.alloc_with_capacity(2, 2, 0);
        let (c2_id, c2_rc) = arena.alloc_with_capacity(2, 2, 0);
        assert_eq!(arena.live_count(), 3);

        p_rc.borrow_mut().set_str("a", Value::table(c1_rc.clone()));
        arena.on_old_store(Some(p_id), c1_id);
        c1_rc.borrow_mut().set_str("b", Value::table(c2_rc.clone()));
        arena.on_old_store(Some(c1_id), c2_id);

        assert_eq!(
            arena.slots[c1_id.slot as usize].ownership,
            Ownership::UniqueOwner
        );
        assert_eq!(
            arena.slots[c2_id.slot as usize].ownership,
            Ownership::UniqueOwner
        );

        // Overwrite `p.a = nil` while `p` is still pinned.
        p_rc.borrow_mut().fields.remove("a");
        arena.on_old_release(c1_id);
        let mut pins = empty_pins();
        pins.insert(p_id.slot);

        let plan = arena.flush_pending(&pins, false);
        assert!(plan.finalizers.is_empty());
        assert_eq!(arena.live_count(), 1);
        assert!(arena.is_alive(p_id));
        assert!(!arena.is_alive(c1_id));
        assert!(!arena.is_alive(c2_id));
    }

    #[test]
    fn stack_alias_survives_pending_free_then_reclaims_when_unpinned() {
        let mut arena = OldArena::new();
        let (p_id, p_rc) = arena.alloc_with_capacity(0, 2, 0);
        let (c_id, c_rc) = arena.alloc_with_capacity(0, 2, 0);
        p_rc.borrow_mut().set_str("x", Value::table(c_rc));
        arena.on_old_store(Some(p_id), c_id);

        // `local tmp = p.x; p.x = nil` — `c` is still pinned on stack.
        p_rc.borrow_mut().fields.remove("x");
        arena.on_old_release(c_id);

        let mut pins = empty_pins();
        pins.insert(p_id.slot);
        pins.insert(c_id.slot);
        let _ = arena.flush_pending(&pins, false);
        assert!(arena.is_alive(c_id), "stack-pinned alias must stay alive");

        // Now stack register is cleared -> `c` reclaims.
        pins.remove(&c_id.slot);
        let _ = arena.flush_pending(&pins, true);
        assert!(!arena.is_alive(c_id));
    }

    #[test]
    fn second_store_auto_promotes_shared_and_purple_collects_cycle() {
        let mut arena = OldArena::new();
        let (a_id, a_rc) = arena.alloc_with_capacity(0, 2, 0);
        let (b_id, b_rc) = arena.alloc_with_capacity(0, 2, 0);
        let (r_id, r_rc) = arena.alloc_with_capacity(0, 2, 0);

        a_rc.borrow_mut()
            .set_str("next", Value::table(b_rc.clone()));
        arena.on_old_store(Some(a_id), b_id);
        b_rc.borrow_mut()
            .set_str("next", Value::table(a_rc.clone()));
        arena.on_old_store(Some(b_id), a_id);
        // External root edge makes `a` `Shared(rc = 2)`:
        r_rc.borrow_mut().set_str("entry", Value::table(a_rc));
        arena.on_old_store(Some(r_id), a_id);
        assert_eq!(arena.slots[a_id.slot as usize].ownership, Ownership::Shared);
        assert_eq!(arena.slots[a_id.slot as usize].rc, 2);

        // Remove root edge -> `a` drops `2 -> 1`, pushed to `purple_buffer`.
        r_rc.borrow_mut().fields.remove("entry");
        arena.on_old_release(a_id);
        assert!(!arena.purple_buffer.is_empty());

        let mut pins = empty_pins();
        pins.insert(r_id.slot);
        let plan = arena.collect_cycles(&pins);
        assert!(plan.bytes_freed > 0);
        assert!(arena.is_alive(r_id));
        assert!(!arena.is_alive(a_id));
        assert!(!arena.is_alive(b_id));
        assert_eq!(arena.live_count(), 1);
    }

    #[test]
    fn mixed_shared_and_unique_owner_cycle_collects_without_leaking() {
        let mut arena = OldArena::new();
        let (a_id, a_rc) = arena.alloc_with_capacity(0, 2, 0);
        let (b_id, b_rc) = arena.alloc_with_capacity(0, 2, 0);

        // Zero->one 2-cycle: `a` (`UniqueOwner`) -> `b` (`UniqueOwner`) -> `a`.
        a_rc.borrow_mut()
            .set_str("next", Value::table(b_rc.clone()));
        arena.on_old_store(Some(a_id), b_id);
        b_rc.borrow_mut()
            .set_str("next", Value::table(a_rc.clone()));
        arena.on_old_store(Some(b_id), a_id);
        assert_eq!(
            arena.slots[a_id.slot as usize].ownership,
            Ownership::UniqueOwner
        );
        assert_eq!(
            arena.slots[b_id.slot as usize].ownership,
            Ownership::UniqueOwner
        );
        assert!(!arena.suspect_roots.is_empty());

        // Still on stack -> must stay alive!
        let mut pins = empty_pins();
        pins.insert(a_id.slot);
        let _ = arena.collect_cycles(&pins);
        assert!(arena.is_alive(a_id));
        assert!(arena.is_alive(b_id));

        // Now mark as suspect via second shared release & drop stack pin -> must collect!
        pins.remove(&a_id.slot);
        let _ = arena.collect_cycles(&pins);
        assert!(!arena.is_alive(a_id));
        assert!(!arena.is_alive(b_id));
        assert_eq!(arena.live_count(), 0);
    }

    #[test]
    fn look_generation_invalidates_after_slot_reuse() {
        let mut arena = OldArena::new();
        let (id1, _) = arena.alloc_with_capacity(0, 0, 0);
        assert!(arena.resolve_look(id1).is_some());

        let _ = arena.flush_pending(&empty_pins(), true);
        assert!(arena.resolve_look(id1).is_none());

        let (id2, _) = arena.alloc_with_capacity(0, 0, 0);
        assert_eq!(id1.slot, id2.slot, "free slot reused");
        assert_ne!(id1.epoch, id2.epoch, "generation advanced on free");
        assert!(arena.resolve_look(id1).is_none());
        assert!(arena.resolve_look(id2).is_some());
    }
}
