//! `Job` region arena (`Memory Follows Work` Stage 3).
//!
//! Each `job(fn)` call or coroutine owns a dedicated `JobRegion` identified by
//! a nonzero `job_id: u32` (`0` = global `Old`). Non-Beat allocations within an
//! active `Job` register into its `allocs` list:
//! - Escaping values (return values of `job(fn)`, writes into outer `Old` or
//!   parent `Job` containers) promote transitively into the destination tier.
//! - On `job(fn)` exit or coroutine death, all remaining `Job` allocations
//!   (including arbitrary cycles) tear down in O(dead allocs) with reverse-
//!   allocation `__gc` destructors—completely skipping `Old` cycle collection.

use crate::vm::hash::FxHashMap;
use crate::vm::old::{OldArena, ReclaimPlan, SlotId};

#[derive(Default)]
pub struct JobRegion {
    pub id: u32,
    pub parent_job: u32,
    pub allocs: Vec<SlotId>,
}

/// Manages active and suspended `Job` lifetimes inside [`crate::vm::heap::VmHeap`].
pub struct JobManager {
    next_job_id: u32,
    active_stack: Vec<u32>,
    regions: FxHashMap<u32, JobRegion>,
}

impl Default for JobManager {
    fn default() -> Self {
        Self::new()
    }
}

impl JobManager {
    pub fn new() -> Self {
        Self {
            next_job_id: 1,
            active_stack: Vec::new(),
            regions: FxHashMap::default(),
        }
    }

    #[inline(always)]
    pub fn current_job_id(&self) -> u32 {
        self.active_stack.last().copied().unwrap_or(0)
    }

    #[inline(always)]
    pub fn active_depth(&self) -> usize {
        self.active_stack.len()
    }

    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    /// Creates a new `JobRegion` parented to `current_job_id()` without yet
    /// activating it on `active_stack`.
    pub fn create_job(&mut self) -> u32 {
        let id = self.next_job_id;
        self.next_job_id = self.next_job_id.wrapping_add(1).max(1);
        let parent_job = self.current_job_id();
        self.regions.insert(
            id,
            JobRegion {
                id,
                parent_job,
                allocs: Vec::new(),
            },
        );
        id
    }

    /// Enters an existing `job_id` onto the active stack (returns previous depth
    /// or pushes on resume).
    #[inline]
    pub fn push_active(&mut self, job_id: u32) {
        debug_assert!(job_id != 0 && self.regions.contains_key(&job_id));
        self.active_stack.push(job_id);
    }

    /// Leaves the innermost active job (on `job(fn)` exit, error, or coroutine yield).
    #[inline]
    pub fn pop_active(&mut self) -> Option<u32> {
        self.active_stack.pop()
    }

    #[inline]
    pub fn record_alloc(&mut self, job_id: u32, slot: SlotId) {
        if let Some(reg) = self.regions.get_mut(&job_id) {
            reg.allocs.push(slot);
        }
    }

    #[inline]
    pub fn parent_of(&self, job_id: u32) -> u32 {
        self.regions.get(&job_id).map_or(0, |r| r.parent_job)
    }

    /// Re-homes one escaping `SlotId` (and any children inside `from_job`) into
    /// `to_job` (`0` = `Old`), updating destination `JobRegion::allocs` when `to_job != 0`.
    pub fn escape_subgraph(
        &mut self,
        old: &mut OldArena,
        root: SlotId,
        from_job: u32,
        to_job: u32,
    ) {
        if from_job == 0 || from_job == to_job {
            return;
        }
        let moved = old.promote_job_subgraph(root, from_job, to_job);
        if to_job != 0
            && let Some(reg) = self.regions.get_mut(&to_job)
        {
            reg.allocs.extend(moved);
        }
    }

    /// Removes `job_id` and delegates immediate O(dead allocs) reclamation
    /// (with reverse-`alloc_id` `__gc` finalizers) to [`OldArena`].
    pub fn teardown_job(&mut self, old: &mut OldArena, job_id: u32) -> ReclaimPlan {
        let Some(reg) = self.regions.remove(&job_id) else {
            return ReclaimPlan::default();
        };
        old.collect_job_exit(job_id, reg.allocs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::Value;

    #[test]
    fn job_teardown_frees_cycles_in_o1_while_preserving_escaping_subgraph() {
        let mut old = OldArena::new();
        let mut jobs = JobManager::new();

        let j = jobs.create_job();
        jobs.push_active(j);

        // Allocate a cyclic garbage pair inside job `j`:
        let (a_id, a_rc) = old.alloc_with_capacity(0, 2, j);
        jobs.record_alloc(j, a_id);
        let (b_id, b_rc) = old.alloc_with_capacity(0, 2, j);
        jobs.record_alloc(j, b_id);
        a_rc.borrow_mut()
            .set_str("peer", Value::table(b_rc.clone()));
        b_rc.borrow_mut().set_str("peer", Value::table(a_rc));

        // Allocate an escaping result + child inside job `j`:
        let (ret_id, ret_rc) = old.alloc_with_capacity(0, 2, j);
        jobs.record_alloc(j, ret_id);
        let (child_id, child_rc) = old.alloc_with_capacity(0, 2, j);
        jobs.record_alloc(j, child_id);
        ret_rc.borrow_mut().set_str("child", Value::table(child_rc));

        assert_eq!(old.live_count(), 4);
        assert_eq!(old.purple_buffer.len(), 0);

        // Promote `ret_id` subgraph to `Old` (`0`) on job return.
        jobs.escape_subgraph(&mut old, ret_id, j, 0);
        assert_eq!(jobs.pop_active(), Some(j));

        let plan = jobs.teardown_job(&mut old, j);
        assert!(plan.finalizers.is_empty());
        assert!(old.is_alive(ret_id));
        assert!(old.is_alive(child_id));
        assert!(!old.is_alive(a_id));
        assert!(!old.is_alive(b_id));
        assert_eq!(old.live_count(), 2);
        // Zero `purple_buffer` pollution during job cycle teardown!
        assert!(old.purple_buffer.is_empty());
    }
}
