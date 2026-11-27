//! Memory-region builtins: `job(fn, ...)`, `share(x)`, and `look(x)`.

use crate::vm::machine::VM;
use crate::vm::old::OldArena;
use crate::vm::value::Value;

/// Runs `fn(...)` inside a private `Job` memory region.
/// Escaping return values promote to the caller's tier, while all remaining
/// job-local allocations (including cycles) tear down in O(dead allocs) with
/// reverse-allocation `__gc` destructors on exit.
pub fn builtin_job(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(func) = args.first().filter(|f| f.is_callable()).cloned() else {
        return Err("bad argument #1 to 'job' (function expected)".to_string());
    };
    let job_id = vm.heap.jobs.create_job();
    vm.heap.jobs.push_active(job_id);
    let beat_floor = vm.beat_enter_nested();
    let res = vm.call_function(func, &args[1..]);

    // Leave `job_id` before promoting surviving results so Beat/Job escapes land
    // directly in `parent_job`.
    vm.heap.jobs.pop_active();
    let parent_job = vm.heap.jobs.parent_of(job_id);

    match res {
        Ok(mut vals) => {
            for v in &mut vals {
                vm.escape_value_to_job(v, job_id, parent_job);
            }
            vm.beat_flush_to_floor(beat_floor, false);
            vm.beat_exit_nested();
            let fin_err = vm.teardown_job_and_finalize(job_id);
            if let Some(e) = fin_err {
                return Err(e);
            }
            Ok(vals)
        }
        Err(err) => {
            vm.beat_flush_to_floor(beat_floor, false);
            vm.beat_exit_nested();
            let _ = vm.teardown_job_and_finalize(job_id);
            Err(err)
        }
    }
}

/// Explicitly promotes `x` into `Old` with `Shared` ownership semantics.
/// Immutable/static values and scalars are returned unchanged.
pub fn builtin_share(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut val = args.first().cloned().unwrap_or(Value::NIL);
    vm.escape_value_to_job(&mut val, vm.heap.jobs.current_job_id(), 0);
    if let Some(cid) = OldArena::slot_of_value(&val) {
        vm.heap.old.promote_to_shared(cid);
    }
    Ok(vec![val])
}

/// Returns a lightweight generational `(slot, epoch)` weak handle `w` for a
/// mutable table `x`. Calling `w()` (or reading `w.k`) returns `x` while live,
/// or `nil` once `x` is reclaimed. Frozen/immutable tables and `nil` pass through.
pub fn builtin_look(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(mut val) = args.first().cloned() else {
        return Ok(vec![Value::NIL]);
    };
    if val.is_nil() || val.is_look() {
        return Ok(vec![val]);
    }
    if !val.is_table() {
        return Err(format!(
            "bad argument #1 to 'look' (table expected, got {})",
            val.type_name()
        ));
    }
    if val.table_borrow().is_some_and(|t| t.is_frozen()) {
        return Ok(vec![val]);
    }
    if val.is_beat() {
        vm.beat_promote_value(&mut val);
    }
    if let Some(cid) = OldArena::slot_of_value(&val) {
        Ok(vec![Value::look(cid.slot, cid.epoch)])
    } else {
        Ok(vec![val])
    }
}
