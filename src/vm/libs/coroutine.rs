//! The `coroutine` library.

use std::cell::RefCell;
use std::rc::Rc;

use crate::bytecode::instruction::MULTRET;
use crate::vm::frame::CallFrame;
use crate::vm::machine::{CoroutineState, VM};
use crate::vm::value::{Value, ValueView, VmTable};

pub fn create_coroutine_lib() -> Value {
    let t = Rc::new(RefCell::new(VmTable::new()));
    let mut b = t.borrow_mut();

    b.set_str(
        "create",
        crate::native!("coroutine.create", coroutine_create),
    );
    b.set_str(
        "resume",
        crate::native!("coroutine.resume", coroutine_resume),
    );
    b.set_str("yield", crate::native!("coroutine.yield", coroutine_yield));
    b.set_str(
        "status",
        crate::native!("coroutine.status", coroutine_status),
    );
    b.set_str(
        "running",
        crate::native!("coroutine.running", coroutine_running),
    );
    b.set_str("wrap", crate::native!("coroutine.wrap", coroutine_wrap));
    b.set_str(
        "isyieldable",
        crate::native!("coroutine.isyieldable", coroutine_isyieldable),
    );
    b.set_str("close", crate::native!("coroutine.close", coroutine_close));
    b.frozen = true;

    drop(b);
    Value::table(t)
}

pub(crate) fn coroutine_create(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(func) = args.first().filter(|f| f.is_callable()).cloned() else {
        return Err("bad argument #1 to 'coroutine.create' (function expected)".to_string());
    };
    Ok(vec![new_coroutine(vm, func)])
}

/// Registers a suspended coroutine that will run `func` and returns its
/// handle. The handle is frozen, and `resume` and `status` go by its identity
/// rather than its fields, so a script cannot forge or revive a coroutine by
/// editing or copying one.
pub(crate) fn new_coroutine(vm: &mut VM, func: Value) -> Value {
    let id = vm.next_co_id;
    vm.next_co_id += 1;

    let mut handle = VmTable::new();
    handle.set_str("__type", Value::str("thread"));
    handle.set_str("_id", Value::from_usize(id));
    handle.frozen = true;
    let handle = Rc::new(RefCell::new(handle));

    let state = CoroutineState {
        stack: Vec::new(),
        frames: Vec::new(),
        status: "suspended".to_string(),
        func,
        handle: Rc::downgrade(&handle),
        yield_callee: 0,
        yield_retc: 0,
        yield_values: Vec::new(),
        open_upvalues: Vec::new(),
        resume_error: None,
    };
    let state_rc = Rc::new(RefCell::new(state));
    // The map holds only a WEAK reference: the handle owns the state
    // strongly, so an abandoned suspended coroutine dies with its handle
    // instead of leaking its whole stack/frames in this map. The state only
    // points back at the handle weakly, so the two do not form a cycle.
    vm.co_states.insert(id, Rc::downgrade(&state_rc));
    // Dead-weak pruning is amortized: full collections prune, and creation
    // prunes once the map doubles past the last prune, so a loop that never
    // collects still cannot pile dead entries unboundedly.
    if vm.co_states.len() > vm.co_states_pruned_at.saturating_mul(2).saturating_add(64) {
        vm.prune_co_states();
    }
    handle.borrow_mut().co_state = Some(state_rc);
    Value::table(handle)
}

/// The id of the live coroutine whose handle is `val`. Finished coroutines
/// are dropped from `vm.co_states`, so they, like tables that merely look
/// like a handle, give `None`.
fn live_coroutine(vm: &VM, val: Option<&Value>, func: &str) -> Result<Option<usize>, String> {
    let Some(t) = val.and_then(|v| v.as_table()) else {
        return Err(format!("bad argument #1 to '{}' (thread expected)", func));
    };
    let id = t.borrow().get_str("_id").as_usize().unwrap_or(0);
    let is_handle = vm.co_state(id).is_some_and(|co| {
        co.borrow()
            .handle
            .upgrade()
            .is_some_and(|h| std::ptr::eq(Rc::as_ptr(&h), t.as_ptr()))
    });
    Ok(is_handle.then_some(id))
}

/// Whether `val` is the handle of a coroutine that `resume` would start or
/// continue.
pub(crate) fn is_suspended(vm: &VM, val: &Value) -> bool {
    live_coroutine(vm, Some(val), "coroutine.resume")
        .ok()
        .flatten()
        .and_then(|id| vm.co_state(id))
        .is_some_and(|co| co.borrow().status == "suspended")
}

/// The handle of the running coroutine, if `coroutine.yield` would succeed
/// here.
pub(crate) fn yieldable_handle(vm: &VM) -> Option<Rc<RefCell<VmTable>>> {
    if vm.native_calls > vm.yield_floor {
        return None;
    }
    vm.current_co
        .and_then(|co_id| vm.co_state(co_id))
        .and_then(|co| co.borrow().handle.upgrade())
}

/// `coroutine.resume(co, ...)`. Yields are only allowed at the native-call
/// level the coroutine is resumed from; see `VM::yield_floor`.
pub(crate) fn coroutine_resume(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    // The caller's frames are put aside while the coroutine runs, but the
    // Rust stack still nests, so they keep counting toward the call depth.
    if vm.call_depth() >= vm.max_call_depth() {
        return Err(format!(
            "call stack overflow: exceeded maximum call depth of {}",
            vm.max_call_depth()
        ));
    }
    let saved_floor = std::mem::replace(&mut vm.yield_floor, vm.native_calls);
    let saved_below = vm.frames_below;
    vm.frames_below += vm.frames.len();
    let res = resume(vm, args);
    vm.frames_below = saved_below;
    vm.yield_floor = saved_floor;
    res
}

/// Resumes a suspended coroutine by raising `message` at the call it is
/// suspended in, as if that call had failed, instead of returning values
/// there. The scheduler uses it for background work that failed. Returns
/// what `coroutine.resume` would.
pub(crate) fn resume_with_error(
    vm: &mut VM,
    thread: &Value,
    message: String,
) -> Result<Vec<Value>, String> {
    let id = live_coroutine(vm, Some(thread), "coroutine.resume")?;
    if let Some(co) = id.and_then(|id| vm.co_state(id)) {
        // Only a coroutine that has started is suspended at a call.
        let started = !co.borrow().frames.is_empty();
        if started {
            co.borrow_mut().resume_error = Some(message);
        }
    }
    coroutine_resume(vm, std::slice::from_ref(thread))
}

fn resume(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(co_id) = live_coroutine(vm, args.first(), "coroutine.resume")? else {
        return Ok(vec![
            Value::FALSE,
            Value::str("cannot resume dead coroutine"),
        ]);
    };

    let co_rc = vm.co_state(co_id).expect("live_coroutine found its state");

    let current_status = co_rc.borrow().status.clone();
    if current_status != "suspended" {
        return Ok(vec![
            Value::FALSE,
            Value::string(format!("cannot resume {} coroutine", current_status)),
        ]);
    }

    // The caller's open upvalues are parked while its stack is put aside, so
    // closures that captured its locals keep working from inside the coroutine.
    let caller_open_upvalues = vm.park_upvalues();
    let caller_stack = std::mem::take(&mut vm.stack);
    let caller_frames = std::mem::take(&mut vm.frames);
    let caller_co = vm.current_co;
    // A coroutine that resumes another is "normal" until it gets control back.
    let caller_state = caller_co.and_then(|id| vm.co_state(id));
    if let Some(caller) = &caller_state {
        caller.borrow_mut().status = "normal".to_string();
    }
    let restore_caller = || {
        if let Some(caller) = &caller_state {
            caller.borrow_mut().status = "running".to_string();
        }
    };

    let is_initial = co_rc.borrow().frames.is_empty();
    {
        let mut co_state = co_rc.borrow_mut();
        vm.stack = std::mem::take(&mut co_state.stack);
        vm.frames = std::mem::take(&mut co_state.frames);
        let parked = std::mem::take(&mut co_state.open_upvalues);
        vm.current_co = Some(co_id);
        drop(co_state);
        vm.unpark_upvalues(parked);
        let mut co_state = co_rc.borrow_mut();
        co_state.status = "running".to_string();
    }

    let resume_args = &args[1..];
    let pending_error = if is_initial {
        None
    } else {
        co_rc.borrow_mut().resume_error.take()
    };

    if pending_error.is_some() {
        // Raised at the call the coroutine is suspended in, so the traceback
        // points there.
        vm.error_trace = Some(vm.capture_trace());
    } else if is_initial {
        let func = co_rc.borrow().func.clone();
        match func.view() {
            ValueView::Closure(c) => {
                let c = c.to_rc();
                let base = 0;
                let needed = base + c.proto.max_registers as usize + resume_args.len() + 1;
                vm.stack.resize(needed, Value::NIL);
                for (i, arg) in resume_args.iter().enumerate() {
                    vm.stack[base + i] = arg.clone();
                }
                let num_params = c.proto.num_params as usize;
                let varargs = if resume_args.len() > num_params {
                    resume_args[num_params..].to_vec()
                } else {
                    Vec::new()
                };
                vm.frames.push(CallFrame::with_varargs(c, base, varargs));
            }
            ValueView::Native(def) => {
                let res = (def.func)(vm, resume_args);
                co_rc.borrow_mut().status = "dead".to_string();
                vm.co_states.remove(&co_id);
                vm.close_upvalues(0);
                vm.stack = caller_stack;
                vm.frames = caller_frames;
                vm.current_co = caller_co;
                vm.unpark_upvalues(caller_open_upvalues);
                restore_caller();
                vm.error_trace = None;
                return match res {
                    Ok(vals) => {
                        let mut out = vec![Value::TRUE];
                        out.extend(vals);
                        Ok(out)
                    }
                    Err(e) => Ok(vec![Value::FALSE, Value::string(e)]),
                };
            }
            _ => {
                vm.close_upvalues(0);
                vm.stack = caller_stack;
                vm.frames = caller_frames;
                vm.current_co = caller_co;
                vm.unpark_upvalues(caller_open_upvalues);
                restore_caller();
                return Err("coroutine function is not callable".to_string());
            }
        }
    } else {
        let (callee, retc) = {
            let cs = co_rc.borrow();
            (cs.yield_callee, cs.yield_retc)
        };
        // Same result layout as any other native call; see
        // `store_call_results`.
        let count = match retc {
            MULTRET => resume_args.len(),
            0 => 1,
            n => n as usize,
        };
        let base = vm.frames.last().map(|f| f.base).unwrap_or(0);
        for i in 0..count {
            let val = resume_args.get(i).cloned().unwrap_or(Value::NIL);
            let slot = base + callee as usize + i;
            if slot >= vm.stack.len() {
                vm.stack.resize(slot + 1, Value::NIL);
            }
            vm.stack[slot] = val;
        }
        if retc == MULTRET {
            vm.top = base + callee as usize + count;
        }
    }

    let run_res = match pending_error {
        Some(err) => Err(err),
        None => vm.run_to_depth(0),
    };

    match run_res {
        Err(ref _err) if vm.is_yielding => {
            vm.is_yielding = false;
            let parked = vm.park_upvalues();
            let mut cs = co_rc.borrow_mut();
            cs.stack = std::mem::take(&mut vm.stack);
            cs.frames = std::mem::take(&mut vm.frames);
            cs.open_upvalues = parked;
            cs.status = "suspended".to_string();
            let yielded = std::mem::take(&mut cs.yield_values);
            drop(cs);

            vm.stack = caller_stack;
            vm.frames = caller_frames;
            vm.current_co = caller_co;
            vm.unpark_upvalues(caller_open_upvalues);
            restore_caller();

            let mut out = vec![Value::TRUE];
            out.extend(yielded);
            Ok(out)
        }
        Ok(first) => {
            let mut returned = vm.take_returned();
            if returned.is_empty() {
                returned.push(first);
            }
            vm.close_upvalues(0);
            let mut cs = co_rc.borrow_mut();
            cs.status = "dead".to_string();
            drop(cs);
            vm.co_states.remove(&co_id);

            vm.stack = caller_stack;
            vm.frames = caller_frames;
            vm.current_co = caller_co;
            vm.unpark_upvalues(caller_open_upvalues);
            restore_caller();

            let mut out = vec![Value::TRUE];
            out.extend(returned);
            Ok(out)
        }
        Err(err) => {
            vm.close_upvalues(0);
            let mut cs = co_rc.borrow_mut();
            cs.status = "dead".to_string();
            drop(cs);
            vm.co_states.remove(&co_id);

            vm.stack = caller_stack;
            vm.frames = caller_frames;
            vm.current_co = caller_co;
            vm.unpark_upvalues(caller_open_upvalues);
            restore_caller();
            if vm.limits.is_tripped() {
                return Err(err);
            }
            vm.error_trace = None;

            Ok(vec![Value::FALSE, Value::string(err)])
        }
    }
}

pub(crate) fn coroutine_yield(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(co_id) = vm.current_co else {
        return Err("attempt to yield from outside a coroutine".to_string());
    };
    if vm.native_calls > vm.yield_floor {
        // A native function (`try`, `table.sort`, a metamethod, ...) is
        // between this call and the coroutine's own frames. Its Rust stack
        // cannot be saved, so unwinding through it would lose its state.
        return Err("attempt to yield across a native function call".to_string());
    }
    if let Some(co_rc) = vm.co_state(co_id) {
        co_rc.borrow_mut().yield_values = args.to_vec();
    }
    vm.is_yielding = true;
    Err("__NEYUKI_COROUTINE_YIELD__".to_string())
}

fn coroutine_status(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let status = match live_coroutine(vm, args.first(), "coroutine.status")? {
        Some(co_id) => match vm.co_state(co_id) {
            Some(co) => co.borrow().status.clone(),
            None => "dead".to_string(),
        },
        None => "dead".to_string(),
    };
    Ok(vec![Value::string(status)])
}

fn coroutine_running(vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    let handle = vm
        .current_co
        .and_then(|co_id| vm.co_state(co_id))
        .and_then(|co| co.borrow().handle.upgrade());
    match handle {
        Some(h) => Ok(vec![Value::table(h), Value::FALSE]),
        None => Ok(vec![Value::NIL, Value::TRUE]),
    }
}

fn wrapped_coroutine_call(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    if args.is_empty() {
        return Err("coroutine wrapper called without self".to_string());
    }
    let Some(wrap_tbl) = args[0].as_table() else {
        return Err("coroutine wrapper called on non-table".to_string());
    };
    let co_table = wrap_tbl.borrow().get_str("_co");
    if co_table.is_nil() {
        return Err("invalid coroutine wrapper".to_string());
    }
    let mut resume_args = vec![co_table];
    resume_args.extend_from_slice(&args[1..]);
    let res = coroutine_resume(vm, &resume_args)?;
    if res.first().and_then(|v| v.as_bool()) == Some(true) {
        Ok(res[1..].to_vec())
    } else {
        let err_msg = res
            .get(1)
            .map(|v| v.to_string())
            .unwrap_or_else(|| "error in coroutine".to_string());
        Err(err_msg)
    }
}

fn coroutine_wrap(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let create_res = coroutine_create(vm, args)?;
    let co_table = create_res
        .into_iter()
        .next()
        .expect("coroutine.create returns the coroutine");

    let wrapper_table = Rc::new(RefCell::new(VmTable::new()));
    wrapper_table.borrow_mut().set_str("_co", co_table);
    wrapper_table.borrow_mut().frozen = true;

    let mt = Rc::new(RefCell::new(VmTable::new()));
    mt.borrow_mut().set_str(
        "__call",
        crate::native!("wrapped_coroutine_call", wrapped_coroutine_call),
    );
    wrapper_table.borrow_mut().metatable = Some(mt);

    Ok(vec![Value::table(wrapper_table)])
}

/// Whether `coroutine.yield` would succeed here: inside a coroutine and not
/// under a native function that called back into Neyuki.
fn coroutine_isyieldable(vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::bool(
        vm.current_co.is_some() && vm.native_calls <= vm.yield_floor,
    )])
}

/// `coroutine.close(co)`: kills a suspended or finished coroutine, freeing
/// its stack. Returns true, or false and a message for one still running.
pub(crate) fn coroutine_close(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(co_id) = live_coroutine(vm, args.first(), "coroutine.close")? else {
        return Ok(vec![Value::TRUE]);
    };
    let co = vm.co_state(co_id).expect("live_coroutine found its state");
    let status = co.borrow().status.clone();
    if status != "suspended" {
        return Ok(vec![
            Value::FALSE,
            Value::string(format!("cannot close a {} coroutine", status)),
        ]);
    }
    let mut state = co.borrow_mut();
    state.status = "dead".to_string();
    state.stack = Vec::new();
    state.frames = Vec::new();
    state.open_upvalues = Vec::new();
    drop(state);
    vm.co_states.remove(&co_id);
    Ok(vec![Value::TRUE])
}
