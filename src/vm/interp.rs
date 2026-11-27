//! Bytecode interpreter dispatch loop and loop-control helpers.

use std::cell::Cell;
use std::rc::Rc;

use num_integer::Integer as _;

use crate::bytecode::exec::{AlignedExec, ExecInst, TableCacheSlot};
use crate::bytecode::instruction::MULTRET;
use crate::vm::frame::CallFrame;
use crate::vm::machine::{HookEvent, RUNNING_FRAME, VM};
use crate::vm::ops;
use crate::vm::value::{TAG_RC_START, Upvalue, Value, ValueView, VmTable};

/// A register of the running frame. The interpreter loop only calls this
/// with indices below `base + max_registers` of a verified prototype, and
/// sizes the stack to cover that window before dispatching, so the bounds
/// check is skipped.
#[inline(always)]
pub(crate) fn slot(stack: &[Value], idx: usize) -> &Value {
    debug_assert!(idx < stack.len());
    // SAFETY: see above; the caller upholds `idx < stack.len()`.
    unsafe { stack.get_unchecked(idx) }
}

#[inline(always)]
pub(crate) fn slot_mut(stack: &mut [Value], idx: usize) -> &mut Value {
    debug_assert!(idx < stack.len());
    // SAFETY: as for `slot`.
    unsafe { stack.get_unchecked_mut(idx) }
}

impl VM {
    /// Line and count events for the instruction at `ip` of the running
    /// frame, and the debugger's line breakpoints and stepping.
    fn instruction_hook(&mut self, ip: usize) -> Result<(), String> {
        if !self.in_eval {
            if let Some(limit) = self.limits.heap_limit_bytes
                && self.gc.bytes_allocated > limit
            {
                self.gc_collect()?;
            }
            self.limits.step(self.gc.bytes_allocated)?;
        }
        if self.in_hook || self.in_eval {
            return Ok(());
        }
        let frame = self.frames.last_mut().expect(RUNNING_FRAME);
        let line = frame.closure.proto.lines.get(ip).copied().unwrap_or(0);
        // A new line, or a jump back into the same one (a loop), starts a
        // new line event.
        let new_line = line != 0 && (line != frame.hook_line || ip <= frame.hook_ip);
        frame.hook_line = line;
        frame.hook_ip = ip;

        if self.dap.is_some() && new_line {
            crate::dap::check_line(self, line)?;
        }
        let Some(hook) = self.hook.clone() else {
            return Ok(());
        };
        if hook.count > 0 {
            if self.hook_countdown <= 1 {
                self.hook_countdown = hook.count;
                self.call_hook(&hook.func, &[Value::str("count")])?;
            } else {
                self.hook_countdown -= 1;
            }
        }
        if hook.on_line && new_line {
            self.call_hook(&hook.func, &[Value::str("line"), Value::int(line as i64)])?;
        }
        Ok(())
    }

    pub fn run(&mut self) -> Result<Value, String> {
        self.run_to_depth(0)
    }

    /// The interpreter loop.
    ///
    /// The outer loop (re)loads the current frame's code and register base
    /// into locals; the inner loop dispatches instructions against them.
    /// Anything that changes the current frame breaks out to the outer loop,
    /// and anything that can run other code (natives, metamethods) writes
    /// the instruction pointer back first so that code sees a consistent
    /// frame.
    pub fn run_to_depth(&mut self, target_depth: usize) -> Result<Value, String> {
        let result = self.run_loop(target_depth);
        if result.is_err() && !self.is_yielding && self.error_trace.is_none() {
            self.error_trace = Some(self.capture_trace());
        }
        result
    }

    fn run_loop(&mut self, target_depth: usize) -> Result<Value, String> {
        // Hooks and the debugger need a check before every instruction, so
        // the loop is compiled twice and code without them never pays for
        // it. Either copy hands over (`None`) when `trap` changes.
        loop {
            let done = if self.trap {
                self.run_mode::<true>(target_depth)?
            } else {
                self.run_mode::<false>(target_depth)?
            };
            if let Some(value) = done {
                return Ok(value);
            }
        }
    }

    fn run_mode<const TRAP: bool>(&mut self, target_depth: usize) -> Result<Option<Value>, String> {
        if self.frames.len() <= target_depth {
            return Ok(Some(Value::NIL));
        }
        #[cfg(not(debug_assertions))]
        if !TRAP {
            return crate::vm::threaded::run_fast_threaded(self, target_depth);
        }
        let (closure, mut base, mut ip) = {
            let f = self.frames.last().expect(RUNNING_FRAME);
            (f.closure.clone(), f.base, f.ip)
        };
        // Verified once per prototype: afterwards every register operand
        // is known to be below `max_registers`, and with the stack sized
        // to cover the frame's window the register accessors below can
        // skip bounds checks. Nothing shrinks the stack while a frame is
        // live: callers restore it to at least its earlier length.
        if !closure.proto.is_verified() {
            crate::bytecode::verify_proto(&closure.proto).map_err(|e| e.to_string())?;
            closure.proto.mark_verified();
        }
        let window_end = base + closure.proto.max_registers as usize;
        if self.stack.len() < window_end {
            self.stack.resize(window_end, Value::NIL);
        }
        let mut current_closure = closure;
        let mut exec = current_closure.proto.exec();
        let mut code: &[Cell<AlignedExec>] = &exec.ops;
        let mut consts: &[Value] = current_closure.proto.values();

        // Register access relative to this frame. `reg!` reads, `set!` writes.
        macro_rules! reg {
            ($r:expr) => {
                *slot(&self.stack, base + $r as usize)
            };
        }
        macro_rules! set {
            ($r:expr, $v:expr) => {{
                let v = $v;
                slot_mut(&mut self.stack, base + $r as usize).set_inplace(v);
            }};
        }
        macro_rules! sync_ip {
            () => {
                self.frames.last_mut().expect(RUNNING_FRAME).ip = ip;
            };
        }
        macro_rules! jump {
            ($off:expr) => {{
                ip = (ip as isize + $off as isize) as usize;
            }};
        }
        macro_rules! throw {
                ($($arg:tt)*) => {{
                    sync_ip!();
                    return Err(format!($($arg)*));
                }};
            }
        // Runs a fallible expression, syncing the ip before propagating
        // an error so tracebacks point at the failing instruction.
        macro_rules! attempt {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => {
                        sync_ip!();
                        return Err(e);
                    }
                }
            };
        }
        // Binary arithmetic: `i64` fast path, then floats, then the
        // generic evaluator (bignums, mixed types, errors); a table
        // operand goes through its metamethod.
        macro_rules! arith {
            ($dst:expr, $a:expr, $b:expr, $checked:ident, $fop:tt, $slow:path, $event:literal) => {
                arith!(@impl $dst, $a, $b, $checked, $fop, $slow, $event, {
                    let res = attempt!($slow(&reg!($a), &reg!($b)));
                    set!($dst, res);
                })
            };
            ($dst:expr, $a:expr, $b:expr, $checked:ident, $fop:tt, $slow:path, $event:literal, into $into:path) => {
                arith!(@impl $dst, $a, $b, $checked, $fop, $slow, $event, {
                    attempt!($into(
                        &mut self.stack,
                        base + $dst as usize,
                        base + $a as usize,
                        base + $b as usize,
                    ));
                })
            };
            (@impl $dst:expr, $a:expr, $b:expr, $checked:ident, $fop:tt, $slow:path, $event:literal, $fallback:block) => {{
                let ra = &reg!($a);
                let rb = &reg!($b);
                if ra.is_inline_int() && rb.is_inline_int() {
                    let x = ra.as_inline_int_unchecked();
                    let y = rb.as_inline_int_unchecked();
                    if let Some(r) = x.$checked(y) {
                        set!($dst, Value::int(r));
                    } else {
                        let res = attempt!($slow(ra, rb));
                        set!($dst, res);
                    }
                } else if ra.is_float() && rb.is_float() {
                    let r = ra.as_float_unchecked() $fop rb.as_float_unchecked();
                    set!($dst, Value::float(r));
                } else if ra.is_table() || rb.is_table() {
                    if let Some(mm) = self.get_binop_metamethod(ra, rb, $event) {
                        let (va, vb) = (ra.clone(), rb.clone());
                        sync_ip!();
                        self.call_binop_mm(mm, va, vb, base, $dst)?;
                        continue;
                    }
                    let res = attempt!($slow(ra, rb));
                    set!($dst, res);
                } else if ra.is_inline_int() && rb.is_float() {
                    let r = (ra.as_inline_int_unchecked() as f64) $fop rb.as_float_unchecked();
                    set!($dst, Value::float(r));
                } else if ra.is_float() && rb.is_inline_int() {
                    let r = ra.as_float_unchecked() $fop (rb.as_inline_int_unchecked() as f64);
                    set!($dst, Value::float(r));
                } else {
                    $fallback
                }
            }};
        }
        // Arithmetic without an inline fast path (division, modulo,
        // power): straight to the evaluator, metamethods first.
        macro_rules! arith_slow {
            ($dst:expr, $a:expr, $b:expr, $slow:path, $event:literal) => {{
                let ra = &reg!($a);
                let rb = &reg!($b);
                if ra.is_table() || rb.is_table() {
                    if let Some(mm) = self.get_binop_metamethod(ra, rb, $event) {
                        let (va, vb) = (ra.clone(), rb.clone());
                        sync_ip!();
                        self.call_binop_mm(mm, va, vb, base, $dst)?;
                        continue;
                    }
                }
                let res = attempt!($slow(ra, rb));
                set!($dst, res);
            }};
        }
        // Ordered comparison with a conditional jump. `$mm_swap` is true
        // for `>`/`>=`, which are `<`/`<=` with the operands swapped.
        macro_rules! compare {
                ($a:expr, $b:expr, $jump:expr, $op:tt, $slow:path, $event:literal, $mm_swap:expr) => {{
                    let ra = &reg!($a);
                    let rb = &reg!($b);
                    let holds = if ra.is_inline_int() && rb.is_inline_int() {
                        ra.as_inline_int_unchecked() $op rb.as_inline_int_unchecked()
                    } else if ra.is_float() && rb.is_float() {
                        ra.as_float_unchecked() $op rb.as_float_unchecked()
                    } else if ra.is_table() || rb.is_table() {
                        let (l, r) = if $mm_swap { (rb, ra) } else { (ra, rb) };
                        if let Some(mm) = self.get_binop_metamethod(l, r, $event) {
                            let (vl, vr) = (l.clone(), r.clone());
                            sync_ip!();
                            self.call_cmp_mm(mm, vl, vr)?
                        } else {
                            attempt!($slow(ra, rb))
                        }
                    } else {
                        attempt!($slow(ra, rb))
                    };
                    if !holds {
                        jump!($jump);
                    }
                }};
            }
        macro_rules! arith_k {
            ($dst:expr, $a:expr, $k:expr, $checked:ident, $fop:tt, $slow:path, $event:literal) => {{
                let ra = &reg!($a);
                let rb = &consts[$k as usize];
                if ra.is_table() || rb.is_table() {
                    if let Some(mm) = self.get_binop_metamethod(ra, rb, $event) {
                        let (va, vb) = (ra.clone(), rb.clone());
                        sync_ip!();
                        self.call_binop_mm(mm, va, vb, base, $dst)?;
                    } else {
                        let res = attempt!($slow(ra, rb));
                        set!($dst, res);
                    }
                } else if ra.is_inline_int() && rb.is_inline_int() {
                    if let Some(r) = ra.as_inline_int_unchecked().$checked(rb.as_inline_int_unchecked()) {
                        set!($dst, Value::int(r));
                    } else {
                        let res = attempt!($slow(ra, rb));
                        set!($dst, res);
                    }
                } else if ra.is_float() && rb.is_float() {
                    set!($dst, Value::float(ra.as_float_unchecked() $fop rb.as_float_unchecked()));
                } else if ra.is_inline_int() && rb.is_float() {
                    set!($dst, Value::float((ra.as_inline_int_unchecked() as f64) $fop rb.as_float_unchecked()));
                } else if ra.is_float() && rb.is_inline_int() {
                    set!($dst, Value::float(ra.as_float_unchecked() $fop (rb.as_inline_int_unchecked() as f64)));
                } else {
                    let res = attempt!($slow(ra, rb));
                    set!($dst, res);
                }
            }};
        }
        macro_rules! compare_k {
            ($a:expr, $k:expr, $jump:expr, $op:tt, $slow:path, $event:literal, $mm_swap:expr) => {{
                let ra = &reg!($a);
                let rb = &consts[$k as usize];
                let holds = if ra.is_inline_int() && rb.is_inline_int() {
                    ra.as_inline_int_unchecked() $op rb.as_inline_int_unchecked()
                } else if ra.is_float() && rb.is_float() {
                    ra.as_float_unchecked() $op rb.as_float_unchecked()
                } else if ra.is_table() || rb.is_table() {
                    let (l, r) = if $mm_swap { (rb, ra) } else { (ra, rb) };
                    if let Some(mm) = self.get_binop_metamethod(l, r, $event) {
                        let (vl, vr) = (l.clone(), r.clone());
                        sync_ip!();
                        self.call_cmp_mm(mm, vl, vr)?
                    } else {
                        attempt!($slow(ra, rb))
                    }
                } else {
                    attempt!($slow(ra, rb))
                };
                if !holds {
                    jump!($jump);
                }
            }};
        }

        // Immediate binary arithmetic: `i64` fast path, then `f64`, then
        // metamethod / generic evaluator with `Value::int(imm as i64)`.
        macro_rules! arith_imm {
            ($dst:expr, $a:expr, $imm:expr, $checked:ident, $fop:tt, $slow:path, $event:literal) => {{
                let ra = &reg!($a);
                let imm_i64 = $imm as i64;
                if ra.is_inline_int() {
                    if let Some(r) = ra.as_inline_int_unchecked().$checked(imm_i64) {
                        set!($dst, Value::int(r));
                    } else {
                        let rb = Value::int(imm_i64);
                        let res = attempt!($slow(ra, &rb));
                        set!($dst, res);
                    }
                } else if ra.is_float() {
                    set!($dst, Value::float(ra.as_float_unchecked() $fop (imm_i64 as f64)));
                } else if ra.is_table() {
                    let rb = Value::int(imm_i64);
                    if let Some(mm) = self.get_binop_metamethod(ra, &rb, $event) {
                        let va = ra.clone();
                        sync_ip!();
                        self.call_binop_mm(mm, va, rb, base, $dst)?;
                        continue;
                    }
                    let res = attempt!($slow(ra, &rb));
                    set!($dst, res);
                } else {
                    let rb = Value::int(imm_i64);
                    let res = attempt!($slow(ra, &rb));
                    set!($dst, res);
                }
            }};
        }
        // Ordered comparison with an `i16` immediate and conditional jump.
        macro_rules! compare_imm {
            ($a:expr, $imm:expr, $jump:expr, $op:tt, $slow:path, $event:literal, $mm_swap:expr) => {{
                let ra = &reg!($a);
                let imm_i64 = $imm as i64;
                let holds = if ra.is_inline_int() {
                    ra.as_inline_int_unchecked() $op imm_i64
                } else if ra.is_float() {
                    ra.as_float_unchecked() $op (imm_i64 as f64)
                } else if ra.is_table() {
                    let rb = Value::int(imm_i64);
                    let (l, r) = if $mm_swap { (&rb, ra) } else { (ra, &rb) };
                    if let Some(mm) = self.get_binop_metamethod(l, r, $event) {
                        let (vl, vr) = (l.clone(), r.clone());
                        sync_ip!();
                        self.call_cmp_mm(mm, vl, vr)?
                    } else {
                        attempt!($slow(ra, &rb))
                    }
                } else {
                    let rb = Value::int(imm_i64);
                    attempt!($slow(ra, &rb))
                };
                if !holds {
                    jump!($jump);
                }
            }};
        }

        loop {
            let cur_ip = ip;
            let inst = unsafe { code.get_unchecked(cur_ip) }.get().0;
            if matches!(inst, ExecInst::EndSentinel) {
                // Fell off the end without a `Return`: treat as returning nothing.
                sync_ip!();
                if self.debug_active && self.hook.is_some() {
                    self.fire_hook(HookEvent::Return)?;
                }
                let frame = self.frames.pop().expect(RUNNING_FRAME);
                self.beat_unwind_frames(self.frames.len() + 1);
                self.close_upvalues_if_needed(frame.base);
                if self.frames.len() == target_depth {
                    self.returned.clear();
                    return Ok(Some(Value::NIL));
                }
                let caller_frame = self.frames.last().expect(RUNNING_FRAME);
                current_closure = caller_frame.closure.clone();
                base = caller_frame.base;
                ip = caller_frame.ip;
                if self.trap != TRAP {
                    sync_ip!();
                    return Ok(None);
                }
                exec = current_closure.proto.exec();
                code = &exec.ops;
                consts = current_closure.proto.values();
                continue;
            }
            if TRAP {
                // The frame's ip points past the instruction about to run,
                // as it does while that instruction executes.
                self.frames.last_mut().expect(RUNNING_FRAME).ip = ip + 1;
                self.instruction_hook(ip)?;
                if !self.trap {
                    sync_ip!();
                    return Ok(None);
                }
            }
            ip += 1;

            match inst {
                ExecInst::EndSentinel => unreachable!(),
                ExecInst::LoadNil { dst } => set!(dst, Value::NIL),
                ExecInst::LoadBool { dst, val } => set!(dst, Value::bool(val)),
                ExecInst::LoadInt { dst, val } => set!(dst, Value::int(val as i64)),
                ExecInst::LoadK { dst, k } => set!(dst, consts[k as usize].clone()),
                ExecInst::Move { dst, src } => {
                    let bits = slot(&self.stack, base + src as usize).bits();
                    let val = if bits < TAG_RC_START {
                        unsafe { Value::from_trivial_bits(bits) }
                    } else {
                        reg!(src).clone()
                    };
                    set!(dst, val);
                }
                ExecInst::GetGlobal { dst, name_k, site } => {
                    if let Some(val) = exec.global_cache_get(site, self.globals_epoch) {
                        set!(dst, val);
                        continue;
                    }
                    let Some(name) = consts[name_k as usize].as_str_ref() else {
                        throw!("global name must be string");
                    };
                    let val = self.globals.get(&*name).cloned().unwrap_or(Value::NIL);
                    exec.global_cache_set(site, self.globals_epoch, val.clone());
                    set!(dst, val);
                }
                ExecInst::GetGlobalDynamic { dst, name_k } => {
                    let Some(name) = consts[name_k as usize].as_str_ref() else {
                        throw!("global name must be string");
                    };
                    let val = self.globals.get(&*name).cloned().unwrap_or(Value::NIL);
                    set!(dst, val);
                }
                ExecInst::SetGlobal { src, name_k } => {
                    let Some(name) = consts[name_k as usize].as_str_ref() else {
                        throw!("global name must be string");
                    };
                    let mut val = reg!(src).clone();
                    self.beat_escape_if_needed(&mut val);
                    // Re-inserting an existing key would drop the old Rc for
                    // an identical one; a lookup first avoids that churn.
                    if let Some(slot) = self.globals.get_mut(&*name) {
                        slot.set_inplace(val);
                    } else {
                        self.globals.insert(name.to_str_ref(), val);
                    }
                    self.globals_epoch = self.globals_epoch.wrapping_add(1);
                }
                ExecInst::GetUpval { dst, upval_idx } => {
                    let uv = &current_closure.upvalues[upval_idx as usize];
                    match uv.location.get() {
                        Some(idx) => {
                            let bits = self.stack[idx].bits();
                            let v = if bits < TAG_RC_START {
                                unsafe { Value::from_trivial_bits(bits) }
                            } else {
                                self.stack[idx].clone()
                            };
                            set!(dst, v);
                        }
                        None => {
                            let val = uv.get_closed();
                            set!(dst, val);
                        }
                    }
                }
                ExecInst::SetUpval { src, upval_idx } => {
                    let val = reg!(src).clone();
                    self.upvalue_set(&current_closure.upvalues[upval_idx as usize], val);
                }
                ExecInst::NewTable {
                    dst,
                    array_cap,
                    fields_cap,
                } => {
                    if self.gc.should_collect() || self.gc.sweeping {
                        self.gc_auto_step().inspect_err(|_| {
                            sync_ip!();
                        })?;
                    }
                    let table_val = self.alloc_beat_table_sized(array_cap, fields_cap);
                    set!(dst, table_val);
                }
                ExecInst::GetTable { dst, table, key } => {
                    let tbl_val = &reg!(table);
                    let k = &reg!(key);
                    if let Some(t) = tbl_val.as_table() {
                        if let Some(i) = k.as_int()
                            && i >= 1
                        {
                            let tbl = t.borrow();
                            let idx = i as usize - 1;
                            if idx < tbl.array.len() {
                                let val = tbl.array[idx].clone();
                                drop(tbl);
                                set!(dst, val);
                                continue;
                            }
                        } else if let Some(s) = k.as_str_ref() {
                            let tbl = t.borrow();
                            let found = if let Some((s_ptr, s_vms)) = s.as_old_vmstr() {
                                tbl.fields.get_by_vmstr(s_ptr, s_vms)
                            } else {
                                tbl.fields.get(s.as_str())
                            };
                            if let Some(v) = found {
                                let val = v.clone();
                                drop(tbl);
                                set!(dst, val);
                                continue;
                            } else if tbl.metatable.is_none() {
                                drop(tbl);
                                set!(dst, Value::NIL);
                                continue;
                            }
                        }
                    }
                    let val = match Self::table_get_fast(tbl_val, k) {
                        Some(v) => v,
                        None => {
                            let (tbl, k) = (tbl_val.clone(), k.clone());
                            sync_ip!();
                            self.table_get(&tbl, &k)?
                        }
                    };
                    set!(dst, val);
                }
                ExecInst::GetTableK {
                    dst,
                    table,
                    key_k,
                    site,
                } => {
                    let tbl_val = &reg!(table);
                    let k = &consts[key_k as usize];
                    if let (Some(t), Some(s)) = (tbl_val.as_table(), k.as_str_ref()) {
                        let ptr = t.as_ptr() as usize;
                        let tbl = t.borrow();
                        let shape = tbl.fields.shape_version();
                        if let Some(ic) = exec.table_cache_get(site)
                            && ic.table_ptr == ptr
                            && ic.shape_version == shape
                            && let Some(v) = tbl.fields.get_slot_keyed_vmstr(
                                ic.slot_idx as usize,
                                s.as_ptr(),
                                s.vm_str(),
                            )
                        {
                            let val = v.clone();
                            drop(tbl);
                            set!(dst, val);
                            continue;
                        }
                        if let Some(slot_idx) =
                            tbl.fields.find_index_by_vmstr(s.as_ptr(), s.vm_str())
                            && let Some(v) = tbl.fields.get_slot(slot_idx)
                        {
                            let val = v.clone();
                            drop(tbl);
                            exec.table_cache_set(
                                site,
                                TableCacheSlot {
                                    table_ptr: ptr,
                                    shape_version: shape,
                                    slot_idx: slot_idx as u32,
                                },
                            );
                            set!(dst, val);
                            continue;
                        } else if tbl.metatable.is_none() {
                            drop(tbl);
                            set!(dst, Value::NIL);
                            continue;
                        }
                    }
                    let val = match Self::table_get_fast(tbl_val, k) {
                        Some(v) => v,
                        None => {
                            let tbl = tbl_val.clone();
                            sync_ip!();
                            self.table_get(&tbl, k)?
                        }
                    };
                    set!(dst, val);
                }
                ExecInst::GetTableKDynamic { dst, table, key_k } => {
                    let tbl_val = &reg!(table);
                    let k = &consts[key_k as usize];
                    let val = match Self::table_get_fast(tbl_val, k) {
                        Some(v) => v,
                        None => {
                            let tbl = tbl_val.clone();
                            sync_ip!();
                            self.table_get(&tbl, k)?
                        }
                    };
                    set!(dst, val);
                }
                ExecInst::GetImport {
                    dst,
                    mod_k,
                    field_k,
                    site,
                } => {
                    let site = site as usize;
                    // Fast path: resolved under this epoch with the
                    // module still metatable-free. One epoch read, one
                    // pointer check, one clone instead of two hashes.
                    if let Some(cached) = exec.import_cached(site, self.globals_epoch) {
                        set!(dst, cached);
                    } else {
                        let (mname, fname) =
                            match (&consts[mod_k as usize], &consts[field_k as usize]) {
                                (m, f) if m.is_string() && f.is_string() => {
                                    (m.to_str_ref().unwrap(), f.to_str_ref().unwrap())
                                }
                                _ => throw!("import names must be strings"),
                            };
                        // Dynamic resolution, exactly what GetGlobal
                        // plus GetTableK do: missing globals read nil.
                        let module_val = self.globals.get(&*mname).cloned().unwrap_or(Value::NIL);
                        let fkey = Value::str_ref(fname);
                        let value = match Self::table_get_fast(&module_val, &fkey) {
                            Some(v) => v,
                            None => {
                                sync_ip!();
                                self.table_get(&module_val, &fkey)?
                            }
                        };
                        // Cache only what cannot change behind our back:
                        // a frozen, metatable-free module with a present
                        // non-nil field. Misses, user tables and anything
                        // with a metatable stay fully dynamic.
                        if let Some(t) = module_val.as_table() {
                            let cacheable = {
                                let tbl = t.borrow();
                                tbl.frozen && tbl.metatable.is_none() && !value.is_nil()
                            };
                            if cacheable {
                                exec.import_store(
                                    site,
                                    self.globals_epoch,
                                    t.to_rc(),
                                    value.clone(),
                                );
                            }
                        }
                        set!(dst, value);
                    }
                }
                ExecInst::SetTable { table, key, val } => {
                    let tbl_val = &reg!(table);
                    let k = &reg!(key);
                    let v = &reg!(val);
                    if let Some(bt_ptr) = tbl_val.beat_table_ptr()
                        && unsafe { (*bt_ptr).extra.is_null() }
                        && !k.is_bigint()
                    {
                        let (kv, vv) = (k.clone(), v.clone());
                        attempt!(unsafe { self.heap.beat.table_raw_set(bt_ptr, kv, vv) });
                        continue;
                    }
                    if let Some(t) = tbl_val.as_table()
                        && !v.is_nil()
                        && !v.is_beat()
                        && !k.is_beat()
                    {
                        if let Some(i) = k.as_int()
                            && i >= 1
                        {
                            let mut tbl = t.borrow_mut();
                            if !tbl.frozen {
                                let idx = i as usize - 1;
                                if idx < tbl.array.len() {
                                    tbl.array[idx].assign_ref(v);
                                    continue;
                                } else if idx == tbl.array.len()
                                    && tbl.metatable.is_none()
                                    && tbl.general.is_none()
                                {
                                    tbl.array.push(v.clone());
                                    continue;
                                }
                            }
                        } else if let Some(s) = k.as_str_ref() {
                            let mut tbl = t.borrow_mut();
                            if !tbl.frozen {
                                if tbl.metatable.is_none() {
                                    tbl.fields.set_by_vmstr(s.as_ptr(), s.vm_str(), v);
                                    continue;
                                } else if let Some(slot) =
                                    tbl.fields.get_mut_by_vmstr(s.as_ptr(), s.vm_str())
                                {
                                    crate::vm::table::assign_value(slot, v);
                                    continue;
                                }
                            }
                        }
                    }
                    if !Self::table_set_fast(tbl_val, k, v) {
                        let (tbl, k, v) = (tbl_val.clone(), k.clone(), v.clone());
                        sync_ip!();
                        self.table_set(&tbl, k, v)?;
                    }
                }
                ExecInst::SetTableK {
                    table,
                    key_k,
                    val,
                    site,
                } => {
                    let tbl_val = &reg!(table);
                    let k = &consts[key_k as usize];
                    let v = &reg!(val);
                    if let Some(bt_ptr) = tbl_val.beat_table_ptr()
                        && unsafe { (*bt_ptr).extra.is_null() }
                    {
                        let (kv, vv) = (k.clone(), v.clone());
                        attempt!(unsafe { self.heap.beat.table_raw_set(bt_ptr, kv, vv) });
                        continue;
                    }
                    if let (Some(t), Some(s)) = (tbl_val.as_table(), k.as_str_ref())
                        && !v.is_nil()
                        && !v.is_beat()
                    {
                        let ptr = t.as_ptr() as usize;
                        let mut tbl = t.borrow_mut();
                        if !tbl.frozen {
                            let shape = tbl.fields.shape_version();
                            if let Some(ic) = exec.table_cache_get(site)
                                && ic.table_ptr == ptr
                                && ic.shape_version == shape
                                && let Some(slot_v) = tbl.fields.get_slot_keyed_mut_vmstr(
                                    ic.slot_idx as usize,
                                    s.as_ptr(),
                                    s.vm_str(),
                                )
                            {
                                slot_v.assign_ref(v);
                                continue;
                            }
                            if let Some(slot_idx) =
                                tbl.fields.find_index_by_vmstr(s.as_ptr(), s.vm_str())
                            {
                                tbl.fields.set_slot(slot_idx, v.clone());
                                drop(tbl);
                                exec.table_cache_set(
                                    site,
                                    TableCacheSlot {
                                        table_ptr: ptr,
                                        shape_version: shape,
                                        slot_idx: slot_idx as u32,
                                    },
                                );
                                continue;
                            } else if tbl.metatable.is_none() {
                                let slot_idx = tbl.fields.insert_cached(s.to_str_ref(), v.clone());
                                let new_shape = tbl.fields.shape_version();
                                drop(tbl);
                                exec.table_cache_set(
                                    site,
                                    TableCacheSlot {
                                        table_ptr: ptr,
                                        shape_version: new_shape,
                                        slot_idx: slot_idx as u32,
                                    },
                                );
                                continue;
                            }
                        }
                    }
                    if !Self::table_set_fast(tbl_val, k, v) {
                        let (tbl, k, v) = (tbl_val.clone(), k.clone(), v.clone());
                        sync_ip!();
                        self.table_set(&tbl, k, v)?;
                    }
                }
                ExecInst::SetTableKDynamic { table, key_k, val } => {
                    let tbl_val = &reg!(table);
                    let k = &consts[key_k as usize];
                    let v = &reg!(val);
                    if !Self::table_set_fast(tbl_val, k, v) {
                        let (tbl, k, v) = (tbl_val.clone(), k.clone(), v.clone());
                        sync_ip!();
                        self.table_set(&tbl, k, v)?;
                    }
                }
                ExecInst::AppendArray { table, src } => {
                    let val = reg!(src).clone();
                    attempt!(self.beat_append_array(base + table as usize, val));
                }
                ExecInst::SetList {
                    table,
                    base: list,
                    count,
                } => {
                    attempt!(self.beat_set_list(base, table, list, count));
                }
                ExecInst::TForCall { base: b, retc } => {
                    sync_ip!();
                    self.tfor_call(base, b, retc)?;
                }
                ExecInst::TForLoop { base: b, jump } => {
                    let first_var = reg!(b + 3).clone();
                    if first_var.is_nil() {
                        jump!(jump);
                    } else {
                        set!(b + 2, first_var);
                    }
                }
                ExecInst::TForLoopBeat { base: b, jump, .. } => {
                    let first_var = reg!(b + 3).clone();
                    if first_var.is_nil() {
                        let target_ip = (ip as isize + jump as isize) as usize;
                        self.beat_pop_exited_loops(self.frames.len(), target_ip);
                        jump!(jump);
                    } else {
                        set!(b + 2, first_var);
                    }
                }
                ExecInst::CloseUpval { reg } => {
                    if !self.open_upvalues.is_empty() {
                        self.close_upvalue_at(base + reg as usize);
                    }
                }
                ExecInst::AddInt { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int()
                        && rb.is_inline_int()
                        && let Some(r) = ra
                            .as_inline_int_unchecked()
                            .checked_add(rb.as_inline_int_unchecked())
                    {
                        set!(dst, Value::int(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::AddGeneric { dst, a, b }));
                    arith!(dst, a, b, checked_add, +, ops::eval_add, "__add", into ops::eval_add_into)
                }
                ExecInst::AddFloat { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_float() && rb.is_float() {
                        let r = ra.as_float_unchecked() + rb.as_float_unchecked();
                        set!(dst, Value::float(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::AddGeneric { dst, a, b }));
                    arith!(dst, a, b, checked_add, +, ops::eval_add, "__add", into ops::eval_add_into)
                }
                ExecInst::Add { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int() && rb.is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::AddInt { dst, a, b }));
                    } else if ra.is_float() && rb.is_float() {
                        code[cur_ip].set(AlignedExec(ExecInst::AddFloat { dst, a, b }));
                    }
                    arith!(dst, a, b, checked_add, +, ops::eval_add, "__add", into ops::eval_add_into)
                }
                ExecInst::AddGeneric { dst, a, b } => {
                    arith!(dst, a, b, checked_add, +, ops::eval_add, "__add", into ops::eval_add_into)
                }
                ExecInst::AddIntI { dst, a, imm } => {
                    let ra = &reg!(a);
                    if ra.is_inline_int()
                        && let Some(r) = ra.as_inline_int_unchecked().checked_add(imm as i64)
                    {
                        set!(dst, Value::int(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::AddIGeneric { dst, a, imm }));
                    arith_imm!(dst, a, imm, checked_add, +, ops::eval_add, "__add")
                }
                ExecInst::AddI { dst, a, imm } => {
                    if reg!(a).is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::AddIntI { dst, a, imm }));
                    }
                    arith_imm!(dst, a, imm, checked_add, +, ops::eval_add, "__add")
                }
                ExecInst::AddIGeneric { dst, a, imm } => {
                    arith_imm!(dst, a, imm, checked_add, +, ops::eval_add, "__add")
                }
                ExecInst::AddIntK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int()
                        && rb.is_inline_int()
                        && let Some(r) = ra
                            .as_inline_int_unchecked()
                            .checked_add(rb.as_inline_int_unchecked())
                    {
                        set!(dst, Value::int(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::AddKGeneric { dst, a, k }));
                    arith_k!(dst, a, k, checked_add, +, ops::eval_add, "__add")
                }
                ExecInst::AddFloatK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_float() && rb.is_float() {
                        let r = ra.as_float_unchecked() + rb.as_float_unchecked();
                        set!(dst, Value::float(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::AddKGeneric { dst, a, k }));
                    arith_k!(dst, a, k, checked_add, +, ops::eval_add, "__add")
                }
                ExecInst::AddK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int() && rb.is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::AddIntK { dst, a, k }));
                    } else if ra.is_float() && rb.is_float() {
                        code[cur_ip].set(AlignedExec(ExecInst::AddFloatK { dst, a, k }));
                    }
                    arith_k!(dst, a, k, checked_add, +, ops::eval_add, "__add")
                }
                ExecInst::AddKGeneric { dst, a, k } => {
                    arith_k!(dst, a, k, checked_add, +, ops::eval_add, "__add")
                }
                ExecInst::SubInt { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int()
                        && rb.is_inline_int()
                        && let Some(r) = ra
                            .as_inline_int_unchecked()
                            .checked_sub(rb.as_inline_int_unchecked())
                    {
                        set!(dst, Value::int(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::SubGeneric { dst, a, b }));
                    arith!(dst, a, b, checked_sub, -, ops::eval_sub, "__sub", into ops::eval_sub_into)
                }
                ExecInst::SubFloat { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_float() && rb.is_float() {
                        let r = ra.as_float_unchecked() - rb.as_float_unchecked();
                        set!(dst, Value::float(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::SubGeneric { dst, a, b }));
                    arith!(dst, a, b, checked_sub, -, ops::eval_sub, "__sub", into ops::eval_sub_into)
                }
                ExecInst::Sub { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int() && rb.is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::SubInt { dst, a, b }));
                    } else if ra.is_float() && rb.is_float() {
                        code[cur_ip].set(AlignedExec(ExecInst::SubFloat { dst, a, b }));
                    }
                    arith!(dst, a, b, checked_sub, -, ops::eval_sub, "__sub", into ops::eval_sub_into)
                }
                ExecInst::SubGeneric { dst, a, b } => {
                    arith!(dst, a, b, checked_sub, -, ops::eval_sub, "__sub", into ops::eval_sub_into)
                }
                ExecInst::SubIntI { dst, a, imm } => {
                    let ra = &reg!(a);
                    if ra.is_inline_int()
                        && let Some(r) = ra.as_inline_int_unchecked().checked_sub(imm as i64)
                    {
                        set!(dst, Value::int(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::SubIGeneric { dst, a, imm }));
                    arith_imm!(dst, a, imm, checked_sub, -, ops::eval_sub, "__sub")
                }
                ExecInst::SubI { dst, a, imm } => {
                    if reg!(a).is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::SubIntI { dst, a, imm }));
                    }
                    arith_imm!(dst, a, imm, checked_sub, -, ops::eval_sub, "__sub")
                }
                ExecInst::SubIGeneric { dst, a, imm } => {
                    arith_imm!(dst, a, imm, checked_sub, -, ops::eval_sub, "__sub")
                }
                ExecInst::SubIntK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int()
                        && rb.is_inline_int()
                        && let Some(r) = ra
                            .as_inline_int_unchecked()
                            .checked_sub(rb.as_inline_int_unchecked())
                    {
                        set!(dst, Value::int(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::SubKGeneric { dst, a, k }));
                    arith_k!(dst, a, k, checked_sub, -, ops::eval_sub, "__sub")
                }
                ExecInst::SubFloatK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_float() && rb.is_float() {
                        let r = ra.as_float_unchecked() - rb.as_float_unchecked();
                        set!(dst, Value::float(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::SubKGeneric { dst, a, k }));
                    arith_k!(dst, a, k, checked_sub, -, ops::eval_sub, "__sub")
                }
                ExecInst::SubK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int() && rb.is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::SubIntK { dst, a, k }));
                    } else if ra.is_float() && rb.is_float() {
                        code[cur_ip].set(AlignedExec(ExecInst::SubFloatK { dst, a, k }));
                    }
                    arith_k!(dst, a, k, checked_sub, -, ops::eval_sub, "__sub")
                }
                ExecInst::SubKGeneric { dst, a, k } => {
                    arith_k!(dst, a, k, checked_sub, -, ops::eval_sub, "__sub")
                }
                ExecInst::MulInt { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int()
                        && rb.is_inline_int()
                        && let Some(r) = ra
                            .as_inline_int_unchecked()
                            .checked_mul(rb.as_inline_int_unchecked())
                    {
                        set!(dst, Value::int(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::MulGeneric { dst, a, b }));
                    arith!(dst, a, b, checked_mul, *, ops::eval_mul, "__mul")
                }
                ExecInst::MulFloat { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_float() && rb.is_float() {
                        let r = ra.as_float_unchecked() * rb.as_float_unchecked();
                        set!(dst, Value::float(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::MulGeneric { dst, a, b }));
                    arith!(dst, a, b, checked_mul, *, ops::eval_mul, "__mul")
                }
                ExecInst::Mul { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int() && rb.is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::MulInt { dst, a, b }));
                    } else if ra.is_float() && rb.is_float() {
                        code[cur_ip].set(AlignedExec(ExecInst::MulFloat { dst, a, b }));
                    }
                    arith!(dst, a, b, checked_mul, *, ops::eval_mul, "__mul")
                }
                ExecInst::MulGeneric { dst, a, b } => {
                    arith!(dst, a, b, checked_mul, *, ops::eval_mul, "__mul")
                }
                ExecInst::MulIntI { dst, a, imm } => {
                    let ra = &reg!(a);
                    if ra.is_inline_int()
                        && let Some(r) = ra.as_inline_int_unchecked().checked_mul(imm as i64)
                    {
                        set!(dst, Value::int(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::MulIGeneric { dst, a, imm }));
                    arith_imm!(dst, a, imm, checked_mul, *, ops::eval_mul, "__mul")
                }
                ExecInst::MulI { dst, a, imm } => {
                    if reg!(a).is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::MulIntI { dst, a, imm }));
                    }
                    arith_imm!(dst, a, imm, checked_mul, *, ops::eval_mul, "__mul")
                }
                ExecInst::MulIGeneric { dst, a, imm } => {
                    arith_imm!(dst, a, imm, checked_mul, *, ops::eval_mul, "__mul")
                }
                ExecInst::MulIntK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int()
                        && rb.is_inline_int()
                        && let Some(r) = ra
                            .as_inline_int_unchecked()
                            .checked_mul(rb.as_inline_int_unchecked())
                    {
                        set!(dst, Value::int(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::MulKGeneric { dst, a, k }));
                    arith_k!(dst, a, k, checked_mul, *, ops::eval_mul, "__mul")
                }
                ExecInst::MulFloatK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_float() && rb.is_float() {
                        let r = ra.as_float_unchecked() * rb.as_float_unchecked();
                        set!(dst, Value::float(r));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::MulKGeneric { dst, a, k }));
                    arith_k!(dst, a, k, checked_mul, *, ops::eval_mul, "__mul")
                }
                ExecInst::MulK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int() && rb.is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::MulIntK { dst, a, k }));
                    } else if ra.is_float() && rb.is_float() {
                        code[cur_ip].set(AlignedExec(ExecInst::MulFloatK { dst, a, k }));
                    }
                    arith_k!(dst, a, k, checked_mul, *, ops::eval_mul, "__mul")
                }
                ExecInst::MulKGeneric { dst, a, k } => {
                    arith_k!(dst, a, k, checked_mul, *, ops::eval_mul, "__mul")
                }
                ExecInst::Div { dst, a, b } => {
                    arith_slow!(dst, a, b, ops::eval_div, "__div")
                }
                ExecInst::IDiv { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int() && rb.is_inline_int() {
                        let x = ra.as_inline_int_unchecked();
                        let y = rb.as_inline_int_unchecked();
                        let val = if x >= 0 && y > 0 {
                            x / y
                        } else if y == 0 || (x == i64::MIN && y == -1) {
                            let res = attempt!(ops::eval_idiv(ra, rb));
                            set!(dst, res);
                            continue;
                        } else {
                            num_integer::Integer::div_floor(&x, &y)
                        };
                        set!(dst, Value::int(val));
                    } else {
                        let res = attempt!(ops::eval_idiv(ra, rb));
                        set!(dst, res);
                    }
                }
                ExecInst::IDivIntPow2 { dst, a, shift } => {
                    let ra = &reg!(a);
                    if ra.is_inline_int() {
                        set!(dst, Value::int(ra.as_inline_int_unchecked() >> shift));
                        continue;
                    }
                    let y = 1i64 << shift;
                    let res = attempt!(ops::eval_idiv(ra, &Value::int(y)));
                    set!(dst, res);
                }
                ExecInst::IDivIntK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int() && rb.is_inline_int() {
                        let x = ra.as_inline_int_unchecked();
                        let y = rb.as_inline_int_unchecked();
                        let val = if x >= 0 {
                            x / y
                        } else {
                            num_integer::Integer::div_floor(&x, &y)
                        };
                        set!(dst, Value::int(val));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::IDivKGeneric { dst, a, k }));
                    let res = attempt!(ops::eval_idiv(ra, rb));
                    set!(dst, res);
                }
                ExecInst::IDivK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int() && rb.is_inline_int() && rb.as_inline_int_unchecked() > 0
                    {
                        let x = ra.as_inline_int_unchecked();
                        let y = rb.as_inline_int_unchecked();
                        if (y as u64).is_power_of_two() {
                            let shift = y.trailing_zeros() as u8;
                            code[cur_ip].set(AlignedExec(ExecInst::IDivIntPow2 { dst, a, shift }));
                            set!(dst, Value::int(x >> shift));
                        } else {
                            code[cur_ip].set(AlignedExec(ExecInst::IDivIntK { dst, a, k }));
                            let val = if x >= 0 {
                                x / y
                            } else {
                                num_integer::Integer::div_floor(&x, &y)
                            };
                            set!(dst, Value::int(val));
                        }
                    } else {
                        let res = attempt!(ops::eval_idiv(ra, rb));
                        set!(dst, res);
                    }
                }
                ExecInst::IDivKGeneric { dst, a, k } => {
                    let res = attempt!(ops::eval_idiv(&reg!(a), &consts[k as usize]));
                    set!(dst, res);
                }
                ExecInst::ModIntPow2 { dst, a, shift } => {
                    let ra = &reg!(a);
                    if ra.is_inline_int() {
                        set!(
                            dst,
                            Value::int(ra.as_inline_int_unchecked() & ((1i64 << shift) - 1))
                        );
                        continue;
                    }
                    let y = 1i64 << shift;
                    let res = attempt!(ops::eval_mod(ra, &Value::int(y)));
                    set!(dst, res);
                }
                ExecInst::ModIntK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int() && rb.is_inline_int() {
                        let x = ra.as_inline_int_unchecked();
                        let y = rb.as_inline_int_unchecked();
                        let val = if x >= 0 { x % y } else { x.mod_floor(&y) };
                        set!(dst, Value::int(val));
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::ModKGeneric { dst, a, k }));
                    let res = attempt!(ops::eval_mod(ra, rb));
                    set!(dst, res);
                }
                ExecInst::ModK { dst, a, k } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int() && rb.is_inline_int() && rb.as_inline_int_unchecked() > 0
                    {
                        let x = ra.as_inline_int_unchecked();
                        let y = rb.as_inline_int_unchecked();
                        if (y as u64).is_power_of_two() {
                            let shift = y.trailing_zeros() as u8;
                            code[cur_ip].set(AlignedExec(ExecInst::ModIntPow2 { dst, a, shift }));
                            set!(dst, Value::int(x & ((1i64 << shift) - 1)));
                        } else {
                            code[cur_ip].set(AlignedExec(ExecInst::ModIntK { dst, a, k }));
                            let val = if x >= 0 { x % y } else { x.mod_floor(&y) };
                            set!(dst, Value::int(val));
                        }
                    } else {
                        let res = attempt!(ops::eval_mod(ra, rb));
                        set!(dst, res);
                    }
                }
                ExecInst::ModKGeneric { dst, a, k } => {
                    let res = attempt!(ops::eval_mod(&reg!(a), &consts[k as usize]));
                    set!(dst, res);
                }
                ExecInst::Mod { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int() && rb.is_inline_int() {
                        let x = ra.as_inline_int_unchecked();
                        let y = rb.as_inline_int_unchecked();
                        let val = if y == 0 {
                            let res = attempt!(ops::eval_mod(ra, rb));
                            set!(dst, res);
                            continue;
                        } else if y == -1 {
                            0
                        } else if x >= 0 && y > 0 {
                            x % y
                        } else {
                            x.mod_floor(&y)
                        };
                        set!(dst, Value::int(val));
                    } else if let (Some(x), Some(y)) = (ra.as_bigint(), rb.as_int()) {
                        let res = if y == 0 {
                            attempt!(ops::eval_mod(ra, rb))
                        } else {
                            Value::int(ops::bigint_mod_floor_i64(x, y))
                        };
                        set!(dst, res);
                    } else {
                        let res = attempt!(ops::eval_mod(ra, rb));
                        set!(dst, res);
                    }
                }
                ExecInst::Pow { dst, a, b } => {
                    arith_slow!(dst, a, b, ops::eval_pow, "__pow")
                }
                ExecInst::BitAnd { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int() && rb.is_inline_int() {
                        set!(
                            dst,
                            Value::int(ra.as_inline_int_unchecked() & rb.as_inline_int_unchecked())
                        );
                    } else {
                        attempt!(ops::eval_bitand_into(
                            &mut self.stack,
                            base + dst as usize,
                            base + a as usize,
                            base + b as usize,
                        ));
                    }
                }
                ExecInst::BitOr { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int() && rb.is_inline_int() {
                        set!(
                            dst,
                            Value::int(ra.as_inline_int_unchecked() | rb.as_inline_int_unchecked())
                        );
                    } else {
                        attempt!(ops::eval_bitor_into(
                            &mut self.stack,
                            base + dst as usize,
                            base + a as usize,
                            base + b as usize,
                        ));
                    }
                }
                ExecInst::BitXor { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int() && rb.is_inline_int() {
                        set!(
                            dst,
                            Value::int(ra.as_inline_int_unchecked() ^ rb.as_inline_int_unchecked())
                        );
                    } else {
                        attempt!(ops::eval_bitxor_into(
                            &mut self.stack,
                            base + dst as usize,
                            base + a as usize,
                            base + b as usize,
                        ));
                    }
                }
                ExecInst::Shl { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    match (ra.view(), rb.view()) {
                        (ValueView::Int(x), ValueView::Int(y)) if x >= 0 && y >= 0 => {
                            let res = if x == 0 {
                                Value::int(0)
                            } else {
                                let headroom = x.leading_zeros().saturating_sub(1) as i64;
                                let headroom128 = (64 + x.leading_zeros() as i64).saturating_sub(1);
                                if y <= headroom {
                                    Value::int(x << y)
                                } else if y <= headroom128 {
                                    Value::from_i128((x as i128) << y)
                                } else {
                                    let x_bits = 64 - x.leading_zeros() as u64;
                                    if (y as u64) <= ops::MAX_INT_BITS
                                        && x_bits.saturating_add(y as u64) <= ops::MAX_INT_BITS
                                    {
                                        Value::from_bigint(
                                            num_bigint::BigInt::from(x) << (y as usize),
                                        )
                                    } else {
                                        attempt!(ops::eval_shl(ra, rb))
                                    }
                                }
                            };
                            set!(dst, res);
                        }
                        (ValueView::BigInt(_), ValueView::Int(y)) if y >= 0 => {
                            let shift = y as u64;
                            attempt!(ops::eval_shl_bigint_into(
                                &mut self.stack,
                                base + dst as usize,
                                base + a as usize,
                                shift,
                            ));
                        }
                        _ => {
                            let res = attempt!(ops::eval_shl(ra, rb));
                            set!(dst, res);
                        }
                    }
                }
                ExecInst::Shr { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    match (ra.view(), rb.view()) {
                        (ValueView::Int(x), ValueView::Int(y)) => {
                            if y < 0 {
                                let res = attempt!(ops::eval_shr(ra, rb));
                                set!(dst, res);
                            } else {
                                let res = if y >= 64 {
                                    Value::int(if x < 0 { -1 } else { 0 })
                                } else {
                                    Value::int(x >> y)
                                };
                                set!(dst, res);
                            }
                        }
                        (ValueView::BigInt(_), ValueView::Int(y)) if y >= 0 => {
                            let shift = y as u64;
                            ops::eval_shr_bigint_into(
                                &mut self.stack,
                                base + dst as usize,
                                base + a as usize,
                                shift,
                            );
                        }
                        _ => {
                            let res = attempt!(ops::eval_shr(ra, rb));
                            set!(dst, res);
                        }
                    }
                }

                ExecInst::ShlI { dst, a, imm } => {
                    let ra = &reg!(a);
                    match ra.view() {
                        ValueView::Int(x) if x >= 0 => {
                            let res = if x == 0 {
                                Value::int(0)
                            } else {
                                let shift = imm as u32;
                                let headroom = x.leading_zeros().saturating_sub(1);
                                let headroom128 = (64 + x.leading_zeros()).saturating_sub(1);
                                if shift <= headroom {
                                    Value::int(x << shift)
                                } else if shift <= headroom128 {
                                    Value::from_i128((x as i128) << shift)
                                } else {
                                    let x_bits = 64 - x.leading_zeros() as u64;
                                    if (shift as u64) <= ops::MAX_INT_BITS
                                        && x_bits.saturating_add(shift as u64) <= ops::MAX_INT_BITS
                                    {
                                        Value::from_bigint(
                                            num_bigint::BigInt::from(x) << (imm as usize),
                                        )
                                    } else {
                                        let rb = Value::int(imm as i64);
                                        attempt!(ops::eval_shl(ra, &rb))
                                    }
                                }
                            };
                            set!(dst, res);
                        }
                        ValueView::BigInt(_) => {
                            attempt!(ops::eval_shl_bigint_into(
                                &mut self.stack,
                                base + dst as usize,
                                base + a as usize,
                                imm as u64,
                            ));
                        }
                        _ => {
                            let rb = Value::int(imm as i64);
                            let res = attempt!(ops::eval_shl(ra, &rb));
                            set!(dst, res);
                        }
                    }
                }
                ExecInst::ShrI { dst, a, imm } => {
                    let ra = &reg!(a);
                    match ra.view() {
                        ValueView::Int(x) => {
                            let res = if imm >= 64 {
                                Value::int(if x < 0 { -1 } else { 0 })
                            } else {
                                Value::int(x >> imm)
                            };
                            set!(dst, res);
                        }
                        ValueView::BigInt(_) => {
                            ops::eval_shr_bigint_into(
                                &mut self.stack,
                                base + dst as usize,
                                base + a as usize,
                                imm as u64,
                            );
                        }
                        _ => {
                            let rb = Value::int(imm as i64);
                            let res = attempt!(ops::eval_shr(ra, &rb));
                            set!(dst, res);
                        }
                    }
                }

                ExecInst::LShl { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = if let (Some(x), Some(y)) = (ra.as_int(), rb.as_int()) {
                        let word = x as u64;
                        let bits = if y < 0 { usize::MAX } else { y as usize };
                        let val = if bits >= 64 { 0 } else { word << bits };
                        Value::from_u64(val)
                    } else {
                        attempt!(ops::eval_lshl(ra, rb))
                    };
                    set!(dst, res);
                }
                ExecInst::LShr { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = if let (Some(x), Some(y)) = (ra.as_int(), rb.as_int()) {
                        let word = x as u64;
                        let bits = if y < 0 { usize::MAX } else { y as usize };
                        let val = if bits >= 64 { 0 } else { word >> bits };
                        Value::from_u64(val)
                    } else {
                        attempt!(ops::eval_lshr(ra, rb))
                    };
                    set!(dst, res);
                }
                ExecInst::Concat { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = if let (Some(x), Some(y)) = (ra.as_str(), rb.as_str()) {
                        self.heap.alloc_beat_concat(x, y)
                    } else if ra.is_table() || rb.is_table() {
                        if let Some(mm) = self.get_binop_metamethod(ra, rb, "__concat") {
                            let (va, vb) = (ra.clone(), rb.clone());
                            sync_ip!();
                            self.call_binop_mm(mm, va, vb, base, dst)?;
                            continue;
                        }
                        Value::string(format!("{}{}", ra, rb))
                    } else {
                        Value::string(format!("{}{}", ra, rb))
                    };
                    set!(dst, res);
                }
                ExecInst::Unm { dst, src } => {
                    let v = &reg!(src);
                    if v.is_inline_int() {
                        set!(dst, Value::int(-v.as_inline_int_unchecked()));
                    } else if v.is_float() {
                        set!(dst, Value::float(-v.as_float_unchecked()));
                    } else if v.is_table() {
                        if let Some(mm) = self.get_unop_metamethod(v, "__unm") {
                            let v = v.clone();
                            sync_ip!();
                            let res = self.call_function(mm, &[v])?;
                            set!(dst, res.into_iter().next().unwrap_or(Value::NIL));
                            continue;
                        }
                        throw!("unary minus expects a number");
                    } else {
                        attempt!(ops::eval_unm_into(
                            &mut self.stack,
                            base + dst as usize,
                            base + src as usize,
                        ));
                    }
                }
                ExecInst::Not { dst, src } => {
                    let truthy = reg!(src).is_truthy();
                    set!(dst, Value::bool(!truthy));
                }
                ExecInst::Len { dst, src } => {
                    let v = &reg!(src);
                    let len = match v.view() {
                        ValueView::String(s) => s.len(),
                        ValueView::Table(t) => {
                            if let Some(mm) = self.get_unop_metamethod(v, "__len") {
                                let v = v.clone();
                                sync_ip!();
                                let res = self.call_function(mm, &[v])?;
                                set!(dst, res.into_iter().next().unwrap_or(Value::NIL));
                                continue;
                            }
                            t.borrow().array_len()
                        }
                        ValueView::Buffer(b) => b.borrow().len(),
                        _ => throw!("len expects string, table or buffer"),
                    };
                    set!(dst, Value::from_usize(len));
                }
                ExecInst::BitNot { dst, src } => {
                    let res = attempt!(ops::eval_bitnot(&reg!(src)));
                    set!(dst, res);
                }
                ExecInst::Coalesce { dst, a, b } => {
                    let va = &reg!(a);
                    let res = if va.is_nil() {
                        reg!(b).clone()
                    } else {
                        va.clone()
                    };
                    set!(dst, res);
                }
                ExecInst::Eq {
                    a,
                    b,
                    jump_if_false,
                } => {
                    let is_eq = self.values_equal(base, a, b, ip)?;
                    if !is_eq {
                        jump!(jump_if_false);
                    }
                }
                ExecInst::Ne {
                    a,
                    b,
                    jump_if_false,
                } => {
                    let is_eq = self.values_equal(base, a, b, ip)?;
                    if is_eq {
                        jump!(jump_if_false);
                    }
                }
                ExecInst::LtInt {
                    a,
                    b,
                    jump_if_false,
                } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int() && rb.is_inline_int() {
                        if !(ra.as_inline_int_unchecked() < rb.as_inline_int_unchecked()) {
                            jump!(jump_if_false);
                        }
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::LtGeneric {
                        a,
                        b,
                        jump_if_false,
                    }));
                    compare!(a, b, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                ExecInst::Lt {
                    a,
                    b,
                    jump_if_false,
                } => {
                    if reg!(a).is_inline_int() && reg!(b).is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::LtInt {
                            a,
                            b,
                            jump_if_false,
                        }));
                    }
                    compare!(a, b, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                ExecInst::LtGeneric {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                ExecInst::LeInt {
                    a,
                    b,
                    jump_if_false,
                } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if ra.is_inline_int() && rb.is_inline_int() {
                        if !(ra.as_inline_int_unchecked() <= rb.as_inline_int_unchecked()) {
                            jump!(jump_if_false);
                        }
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::LeGeneric {
                        a,
                        b,
                        jump_if_false,
                    }));
                    compare!(a, b, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                ExecInst::Le {
                    a,
                    b,
                    jump_if_false,
                } => {
                    if reg!(a).is_inline_int() && reg!(b).is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::LeInt {
                            a,
                            b,
                            jump_if_false,
                        }));
                    }
                    compare!(a, b, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                ExecInst::LeGeneric {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                ExecInst::Gt {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, >, ops::eval_gt, "__lt", true)
                }
                ExecInst::Ge {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, >=, ops::eval_ge, "__le", true)
                }
                ExecInst::EqIntK {
                    a,
                    k,
                    jump_if_false,
                } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int() && rb.is_inline_int() {
                        if ra.as_inline_int_unchecked() != rb.as_inline_int_unchecked() {
                            jump!(jump_if_false);
                        }
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::EqKGeneric {
                        a,
                        k,
                        jump_if_false,
                    }));
                    let va = ra.clone();
                    let vb = rb.clone();
                    let is_eq = self.values_equal_val(&va, &vb, ip)?;
                    if !is_eq {
                        jump!(jump_if_false);
                    }
                }
                ExecInst::EqK {
                    a,
                    k,
                    jump_if_false,
                } => {
                    let va = self.stack[base + a as usize].clone();
                    let vb = consts[k as usize].clone();
                    if va.is_inline_int() && vb.is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::EqIntK {
                            a,
                            k,
                            jump_if_false,
                        }));
                    }
                    let is_eq = self.values_equal_val(&va, &vb, ip)?;
                    if !is_eq {
                        jump!(jump_if_false);
                    }
                }
                ExecInst::EqKGeneric {
                    a,
                    k,
                    jump_if_false,
                } => {
                    let va = self.stack[base + a as usize].clone();
                    let vb = consts[k as usize].clone();
                    let is_eq = self.values_equal_val(&va, &vb, ip)?;
                    if !is_eq {
                        jump!(jump_if_false);
                    }
                }
                ExecInst::NeK {
                    a,
                    k,
                    jump_if_false,
                } => {
                    let va = self.stack[base + a as usize].clone();
                    let vb = consts[k as usize].clone();
                    let is_eq = self.values_equal_val(&va, &vb, ip)?;
                    if is_eq {
                        jump!(jump_if_false);
                    }
                }
                ExecInst::LtIntK {
                    a,
                    k,
                    jump_if_false,
                } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int() && rb.is_inline_int() {
                        if !(ra.as_inline_int_unchecked() < rb.as_inline_int_unchecked()) {
                            jump!(jump_if_false);
                        }
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::LtKGeneric {
                        a,
                        k,
                        jump_if_false,
                    }));
                    compare_k!(a, k, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                ExecInst::LtK {
                    a,
                    k,
                    jump_if_false,
                } => {
                    if reg!(a).is_inline_int() && consts[k as usize].is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::LtIntK {
                            a,
                            k,
                            jump_if_false,
                        }));
                    }
                    compare_k!(a, k, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                ExecInst::LtKGeneric {
                    a,
                    k,
                    jump_if_false,
                } => {
                    compare_k!(a, k, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                ExecInst::LeIntK {
                    a,
                    k,
                    jump_if_false,
                } => {
                    let ra = &reg!(a);
                    let rb = &consts[k as usize];
                    if ra.is_inline_int() && rb.is_inline_int() {
                        if !(ra.as_inline_int_unchecked() <= rb.as_inline_int_unchecked()) {
                            jump!(jump_if_false);
                        }
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::LeKGeneric {
                        a,
                        k,
                        jump_if_false,
                    }));
                    compare_k!(a, k, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                ExecInst::LeK {
                    a,
                    k,
                    jump_if_false,
                } => {
                    if reg!(a).is_inline_int() && consts[k as usize].is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::LeIntK {
                            a,
                            k,
                            jump_if_false,
                        }));
                    }
                    compare_k!(a, k, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                ExecInst::LeKGeneric {
                    a,
                    k,
                    jump_if_false,
                } => {
                    compare_k!(a, k, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                ExecInst::GtK {
                    a,
                    k,
                    jump_if_false,
                } => {
                    compare_k!(a, k, jump_if_false, >, ops::eval_gt, "__lt", true)
                }
                ExecInst::GeK {
                    a,
                    k,
                    jump_if_false,
                } => {
                    compare_k!(a, k, jump_if_false, >=, ops::eval_ge, "__le", true)
                }
                ExecInst::EqI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    let ra = &reg!(a);
                    let is_eq = if ra.is_inline_int() {
                        ra.as_inline_int_unchecked() == imm as i64
                    } else if ra.is_float() {
                        ra.as_float_unchecked() == imm as f64
                    } else {
                        false
                    };
                    if !is_eq {
                        jump!(jump_if_false);
                    }
                }
                ExecInst::NeI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    let ra = &reg!(a);
                    let is_eq = if ra.is_inline_int() {
                        ra.as_inline_int_unchecked() == imm as i64
                    } else if ra.is_float() {
                        ra.as_float_unchecked() == imm as f64
                    } else {
                        false
                    };
                    if is_eq {
                        jump!(jump_if_false);
                    }
                }
                ExecInst::LtIntI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    let ra = &reg!(a);
                    if ra.is_inline_int() {
                        if !(ra.as_inline_int_unchecked() < imm as i64) {
                            jump!(jump_if_false);
                        }
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::LtIGeneric {
                        a,
                        imm,
                        jump_if_false,
                    }));
                    compare_imm!(a, imm, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                ExecInst::LtI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    if reg!(a).is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::LtIntI {
                            a,
                            imm,
                            jump_if_false,
                        }));
                    }
                    compare_imm!(a, imm, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                ExecInst::LtIGeneric {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    compare_imm!(a, imm, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                ExecInst::LeIntI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    let ra = &reg!(a);
                    if ra.is_inline_int() {
                        if !(ra.as_inline_int_unchecked() <= imm as i64) {
                            jump!(jump_if_false);
                        }
                        continue;
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::LeIGeneric {
                        a,
                        imm,
                        jump_if_false,
                    }));
                    compare_imm!(a, imm, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                ExecInst::LeI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    if reg!(a).is_inline_int() {
                        code[cur_ip].set(AlignedExec(ExecInst::LeIntI {
                            a,
                            imm,
                            jump_if_false,
                        }));
                    }
                    compare_imm!(a, imm, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                ExecInst::LeIGeneric {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    compare_imm!(a, imm, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                ExecInst::GtI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    compare_imm!(a, imm, jump_if_false, >, ops::eval_gt, "__lt", true)
                }
                ExecInst::GeI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    compare_imm!(a, imm, jump_if_false, >=, ops::eval_ge, "__le", true)
                }
                ExecInst::Test { reg, jump_if_false } => {
                    if !reg!(reg).is_truthy() {
                        jump!(jump_if_false);
                    }
                }
                ExecInst::Jump { offset } => {
                    if !TRAP && offset < 0 {
                        attempt!(self.limits.check_interrupt());
                    }
                    jump!(offset);
                }
                ExecInst::JumpBeat { offset, site } => {
                    if offset < 0 {
                        if !TRAP {
                            attempt!(self.limits.check_interrupt());
                        }
                        if let Some(sf) = exec.safepoint(site).copied() {
                            let max_r = current_closure.proto.max_registers as usize;
                            sync_ip!();
                            self.beat_safepoint_step(sf, base, max_r, cur_ip, site, true);
                        }
                    }
                    jump!(offset);
                }
                ExecInst::ForPrepBeat {
                    base: b,
                    jump,
                    site,
                } => {
                    let step = &reg!(b + 2);
                    match step.view() {
                        ValueView::Int(0) => throw!("numeric for step cannot be zero"),
                        ValueView::Float(0.0) => throw!("numeric for step cannot be zero"),
                        _ => {}
                    }
                    let init = &reg!(b);
                    let init_minus_step = if init.is_inline_int()
                        && step.is_inline_int()
                        && let Some(diff) = init
                            .as_inline_int_unchecked()
                            .checked_sub(step.as_inline_int_unchecked())
                    {
                        Value::int(diff)
                    } else {
                        attempt!(ops::eval_sub(init, step))
                    };
                    set!(b, init_minus_step);
                    if let Some(sf) = exec.safepoint(site) {
                        self.beat_enter_for_prep(cur_ip, sf.loop_end, site);
                    }
                    jump!(jump);
                }
                ExecInst::ForPrep { base: b, jump } => {
                    let step = &reg!(b + 2);
                    match step.view() {
                        ValueView::Int(0) => throw!("numeric for step cannot be zero"),
                        ValueView::Float(0.0) => {
                            throw!("numeric for step cannot be zero")
                        }
                        _ => {}
                    }
                    let init = &reg!(b);
                    let limit = &reg!(b + 1);
                    let target_ip = (ip as isize + jump as isize) as usize;
                    if init.is_inline_int() && limit.is_inline_int() && step.is_inline_int() {
                        let i = init.as_inline_int_unchecked();
                        let lim = limit.as_inline_int_unchecked();
                        let s = step.as_inline_int_unchecked();
                        if s != 0
                            && let Some(diff) = i.checked_sub(s)
                        {
                            let iters: i128 = if (s > 0 && i <= lim) || (s < 0 && i >= lim) {
                                ((lim as i128 - i as i128) / (s as i128)) + 1
                            } else {
                                0
                            };
                            if (0..=i64::MAX as i128).contains(&iters)
                                && let Some(
                                    ExecInst::ForLoop { base: fb, jump: fj }
                                    | ExecInst::ForLoopInt { base: fb, jump: fj }
                                    | ExecInst::ForLoopCount { base: fb, jump: fj }
                                    | ExecInst::ForLoopCount1 { base: fb, jump: fj }
                                    | ExecInst::ForLoopGeneric { base: fb, jump: fj },
                                ) = code.get(target_ip).map(|c| c.get().0)
                                && fb == b
                            {
                                let specialized = if s == 1 {
                                    ExecInst::ForLoopCount1 { base: fb, jump: fj }
                                } else {
                                    ExecInst::ForLoopCount { base: fb, jump: fj }
                                };
                                code[target_ip].set(AlignedExec(specialized));
                                set!(b, Value::int(diff));
                                set!(b + 1, Value::int(iters as i64));
                                jump!(jump);
                                continue;
                            }
                        }
                    }
                    if let Some(
                        ExecInst::ForLoopCount { base: fb, jump: fj }
                        | ExecInst::ForLoopCount1 { base: fb, jump: fj },
                    ) = code.get(target_ip).map(|c| c.get().0)
                    {
                        code[target_ip].set(AlignedExec(ExecInst::ForLoop { base: fb, jump: fj }));
                    }
                    if init.is_inline_int()
                        && step.is_inline_int()
                        && let Some(diff) = init
                            .as_inline_int_unchecked()
                            .checked_sub(step.as_inline_int_unchecked())
                    {
                        set!(b, Value::int(diff));
                        jump!(jump);
                        continue;
                    }
                    let init_minus_step = attempt!(ops::eval_sub(init, step));
                    set!(b, init_minus_step);
                    jump!(jump);
                }
                ExecInst::ForLoopCount { base: b, jump } => {
                    let rem_val = &reg!(b + 1);
                    let rem = if rem_val.is_inline_int() {
                        rem_val.as_inline_int_unchecked()
                    } else {
                        code[cur_ip].set(AlignedExec(ExecInst::ForLoopGeneric { base: b, jump }));
                        throw!("invalid loop state");
                    };
                    if rem > 0 {
                        let idx_val = &reg!(b);
                        let step_val = &reg!(b + 2);
                        if !idx_val.is_inline_int() || !step_val.is_inline_int() {
                            code[cur_ip]
                                .set(AlignedExec(ExecInst::ForLoopGeneric { base: b, jump }));
                            throw!("invalid loop state");
                        }
                        let next = idx_val
                            .as_inline_int_unchecked()
                            .wrapping_add(step_val.as_inline_int_unchecked());
                        set!(b + 1, Value::int(rem - 1));
                        set!(b, Value::int(next));
                        set!(b + 3, Value::int(next));
                        jump!(jump);
                    }
                    continue;
                }
                ExecInst::ForLoopCount1 { base: b, jump } => {
                    let rem_val = &reg!(b + 1);
                    let rem = if rem_val.is_inline_int() {
                        rem_val.as_inline_int_unchecked()
                    } else {
                        code[cur_ip].set(AlignedExec(ExecInst::ForLoopGeneric { base: b, jump }));
                        throw!("invalid loop state");
                    };
                    if rem > 0 {
                        let idx_val = &reg!(b);
                        if !idx_val.is_inline_int() {
                            code[cur_ip]
                                .set(AlignedExec(ExecInst::ForLoopGeneric { base: b, jump }));
                            throw!("invalid loop state");
                        }
                        let next = idx_val.as_inline_int_unchecked().wrapping_add(1);
                        set!(b + 1, Value::int(rem - 1));
                        set!(b, Value::int(next));
                        set!(b + 3, Value::int(next));
                        jump!(jump);
                    }
                    continue;
                }
                ExecInst::ForLoopInt { base: b, jump } => {
                    let idx_v = &reg!(b);
                    let lim_v = &reg!(b + 1);
                    let step_v = &reg!(b + 2);
                    if idx_v.is_inline_int() && lim_v.is_inline_int() && step_v.is_inline_int() {
                        let i = idx_v.as_inline_int_unchecked();
                        let lim = lim_v.as_inline_int_unchecked();
                        let st = step_v.as_inline_int_unchecked();
                        if let Some(next) = i.checked_add(st) {
                            let again = if st >= 0 { next <= lim } else { next >= lim };
                            set!(b, Value::int(next));
                            if again {
                                set!(b + 3, Value::int(next));
                                jump!(jump);
                            }
                            continue;
                        }
                    }
                    code[cur_ip].set(AlignedExec(ExecInst::ForLoopGeneric { base: b, jump }));
                    let step = &reg!(b + 2);
                    match step.view() {
                        ValueView::Int(0) => throw!("numeric for step cannot be zero"),
                        ValueView::Float(0.0) => {
                            throw!("numeric for step cannot be zero")
                        }
                        _ => {}
                    }
                    let is_positive = match step.view() {
                        ValueView::Int(i) => i >= 0,
                        ValueView::BigInt(i) => num_traits::Signed::is_positive(i),
                        ValueView::Float(f) => f >= 0.0,
                        _ => true,
                    };
                    let idx = &reg!(b);
                    let limit = &reg!(b + 1);
                    let next_idx = attempt!(ops::eval_add(idx, step));
                    let loop_again = if is_positive {
                        attempt!(ops::eval_le(&next_idx, limit))
                    } else {
                        attempt!(ops::eval_ge(&next_idx, limit))
                    };
                    set!(b, next_idx.clone());
                    if loop_again {
                        set!(b + 3, next_idx);
                        jump!(jump);
                    }
                }
                ExecInst::ForLoop { base: b, jump } => {
                    let idx = &reg!(b);
                    let limit = &reg!(b + 1);
                    let step = &reg!(b + 2);
                    if idx.is_inline_int() && limit.is_inline_int() && step.is_inline_int() {
                        let i = idx.as_inline_int_unchecked();
                        let lim = limit.as_inline_int_unchecked();
                        let st = step.as_inline_int_unchecked();
                        if let Some(next) = i.checked_add(st) {
                            code[cur_ip].set(AlignedExec(ExecInst::ForLoopInt { base: b, jump }));
                            let again = if st >= 0 { next <= lim } else { next >= lim };
                            set!(b, Value::int(next));
                            if again {
                                set!(b + 3, Value::int(next));
                                jump!(jump);
                            }
                            continue;
                        }
                    }
                    match step.view() {
                        ValueView::Int(0) => throw!("numeric for step cannot be zero"),
                        ValueView::Float(0.0) => {
                            throw!("numeric for step cannot be zero")
                        }
                        _ => {}
                    }
                    let is_positive = match step.view() {
                        ValueView::Int(i) => i >= 0,
                        ValueView::BigInt(i) => num_traits::Signed::is_positive(i),
                        ValueView::Float(f) => f >= 0.0,
                        _ => true,
                    };
                    let next_idx = attempt!(ops::eval_add(idx, step));
                    let loop_again = if is_positive {
                        attempt!(ops::eval_le(&next_idx, limit))
                    } else {
                        attempt!(ops::eval_ge(&next_idx, limit))
                    };
                    set!(b, next_idx.clone());
                    if loop_again {
                        set!(b + 3, next_idx);
                        jump!(jump);
                    }
                }
                ExecInst::ForLoopGeneric { base: b, jump } => {
                    let idx = &reg!(b);
                    let limit = &reg!(b + 1);
                    let step = &reg!(b + 2);
                    if idx.is_inline_int() && limit.is_inline_int() && step.is_inline_int() {
                        let i = idx.as_inline_int_unchecked();
                        let lim = limit.as_inline_int_unchecked();
                        let st = step.as_inline_int_unchecked();
                        if let Some(next) = i.checked_add(st) {
                            let again = if st >= 0 { next <= lim } else { next >= lim };
                            set!(b, Value::int(next));
                            if again {
                                set!(b + 3, Value::int(next));
                                jump!(jump);
                            }
                            continue;
                        }
                    }
                    match step.view() {
                        ValueView::Int(0) => throw!("numeric for step cannot be zero"),
                        ValueView::Float(0.0) => {
                            throw!("numeric for step cannot be zero")
                        }
                        _ => {}
                    }
                    let is_positive = match step.view() {
                        ValueView::Int(i) => i >= 0,
                        ValueView::BigInt(i) => num_traits::Signed::is_positive(i),
                        ValueView::Float(f) => f >= 0.0,
                        _ => true,
                    };
                    let next_idx = attempt!(ops::eval_add(idx, step));
                    let loop_again = if is_positive {
                        attempt!(ops::eval_le(&next_idx, limit))
                    } else {
                        attempt!(ops::eval_ge(&next_idx, limit))
                    };
                    set!(b, next_idx.clone());
                    if loop_again {
                        set!(b + 3, next_idx);
                        jump!(jump);
                    }
                }
                ExecInst::ForLoopBeat {
                    base: b,
                    jump,
                    site,
                } => {
                    let idx = &reg!(b);
                    let limit = &reg!(b + 1);
                    let step = &reg!(b + 2);
                    let (next_idx, loop_again) =
                        if idx.is_inline_int() && limit.is_inline_int() && step.is_inline_int() {
                            let i = idx.as_inline_int_unchecked();
                            let lim = limit.as_inline_int_unchecked();
                            let st = step.as_inline_int_unchecked();
                            if let Some(next) = i.checked_add(st) {
                                let again = if st >= 0 { next <= lim } else { next >= lim };
                                (Value::int(next), again)
                            } else {
                                let next = attempt!(ops::eval_add(idx, step));
                                let again = if st >= 0 {
                                    attempt!(ops::eval_le(&next, limit))
                                } else {
                                    attempt!(ops::eval_ge(&next, limit))
                                };
                                (next, again)
                            }
                        } else {
                            match step.view() {
                                ValueView::Int(0) => throw!("numeric for step cannot be zero"),
                                ValueView::Float(0.0) => {
                                    throw!("numeric for step cannot be zero")
                                }
                                _ => {}
                            }
                            let is_positive = match step.view() {
                                ValueView::Int(i) => i >= 0,
                                ValueView::BigInt(i) => num_traits::Signed::is_positive(i),
                                ValueView::Float(f) => f >= 0.0,
                                _ => true,
                            };
                            let next = attempt!(ops::eval_add(idx, step));
                            let again = if is_positive {
                                attempt!(ops::eval_le(&next, limit))
                            } else {
                                attempt!(ops::eval_ge(&next, limit))
                            };
                            (next, again)
                        };
                    set!(b, next_idx.clone());
                    if loop_again {
                        set!(b + 3, next_idx);
                    }
                    if let Some(sf) = exec.safepoint(site).copied() {
                        let max_r = current_closure.proto.max_registers as usize;
                        sync_ip!();
                        self.beat_safepoint_step(sf, base, max_r, cur_ip, site, loop_again);
                    }
                    if loop_again {
                        jump!(jump);
                    }
                }
                ExecInst::Closure { dst, proto_idx } => {
                    let child_proto = current_closure.proto.protos[proto_idx as usize].clone();
                    let mut upvalues = Vec::with_capacity(child_proto.upvalues.len());
                    for updesc in &child_proto.upvalues {
                        if updesc.in_stack {
                            upvalues.push(self.capture_upvalue(base + updesc.index as usize));
                        } else {
                            let up = current_closure
                                .upvalues
                                .get(updesc.index as usize)
                                .cloned()
                                .unwrap_or_else(|| Upvalue::closed(Value::NIL));
                            upvalues.push(up);
                        }
                    }
                    let new_closure = self.alloc_closure(child_proto, upvalues);
                    set!(dst, new_closure);
                }
                ExecInst::TailCall { callee, argc } => {
                    let args_start = base + callee as usize + 1;
                    let args_end = if argc == MULTRET {
                        self.top.max(args_start)
                    } else {
                        args_start + argc as usize
                    };
                    let actual_argc = args_end - args_start;
                    if args_end > self.stack.len() {
                        self.stack.resize(args_end, Value::NIL);
                    }
                    sync_ip!();

                    if base >= 1
                        && let Some(callee_closure) = reg!(callee).as_closure()
                    {
                        let callee_closure = callee_closure.to_rc();
                        let same_closure = Rc::ptr_eq(&current_closure, &callee_closure);
                        let (max_registers, num_params, is_vararg) = {
                            let proto = &callee_closure.proto;
                            (
                                proto.max_registers as usize,
                                proto.num_params as usize,
                                proto.is_vararg,
                            )
                        };
                        if same_closure && !is_vararg && argc != MULTRET && !self.debug_active {
                            if !TRAP {
                                attempt!(self.limits.check_interrupt());
                            }
                            self.beat_unwind_frames(self.frames.len());
                            self.close_upvalues_if_needed(base);
                            let needed = base + max_registers;
                            if needed >= self.stack.len() {
                                self.stack.resize(needed + 1, Value::NIL);
                            }
                            let copy_n = actual_argc.min(num_params);
                            for k in 0..copy_n {
                                let src_idx = args_start + k;
                                let dst_idx = base + k;
                                let bits = slot(&self.stack, src_idx).bits();
                                let v = if bits < TAG_RC_START {
                                    unsafe { Value::from_trivial_bits(bits) }
                                } else {
                                    std::mem::replace(
                                        slot_mut(&mut self.stack, src_idx),
                                        Value::NIL,
                                    )
                                };
                                slot_mut(&mut self.stack, dst_idx).set_inplace(v);
                            }
                            for k in copy_n..num_params {
                                slot_mut(&mut self.stack, base + k).set_inplace(Value::NIL);
                            }
                            ip = 0;
                            continue;
                        }

                        if !TRAP {
                            attempt!(self.limits.check_interrupt());
                        }
                        let mut next_closure = if same_closure {
                            None
                        } else {
                            Some(callee_closure)
                        };
                        self.beat_unwind_frames(self.frames.len());
                        self.close_upvalues_if_needed(base);
                        let caller_dest = base - 1;
                        let callee_abs = base + callee as usize;
                        let shift = callee_abs - caller_dest;
                        let len = args_end - callee_abs;
                        for k in 0..len {
                            let v = std::mem::replace(&mut self.stack[callee_abs + k], Value::NIL);
                            self.stack[caller_dest + k] = v;
                        }
                        if self.top > callee_abs {
                            self.top -= shift;
                        }
                        let needed = base + max_registers;
                        if needed >= self.stack.len() {
                            self.stack.resize(needed + 1, Value::NIL);
                        }
                        for i in actual_argc..num_params {
                            self.stack[base + i] = Value::NIL;
                        }
                        if is_vararg && actual_argc > num_params {
                            let varargs =
                                self.stack[base + num_params..base + actual_argc].to_vec();
                            let frame_closure = match &next_closure {
                                Some(c) => Rc::clone(c),
                                None => Rc::clone(&current_closure),
                            };
                            let want_ret = self.frames.last().expect(RUNNING_FRAME).want_ret;
                            *self.frames.last_mut().expect(RUNNING_FRAME) =
                                CallFrame::with_varargs(frame_closure, base, varargs)
                                    .wanting(want_ret);
                        } else if let Some(c) = &next_closure {
                            let frame = self.frames.last_mut().expect(RUNNING_FRAME);
                            frame.closure = Rc::clone(c);
                            frame.ip = 0;
                            frame.clear_varargs();
                        } else {
                            let frame = self.frames.last_mut().expect(RUNNING_FRAME);
                            frame.ip = 0;
                            frame.clear_varargs();
                        }
                        if self.debug_active {
                            if self.hook.is_some() {
                                self.fire_hook(HookEvent::TailCall)?;
                            }
                            if self.dap.is_some() {
                                let active = next_closure.as_ref().unwrap_or(&current_closure);
                                let func_name =
                                    active.proto.name.as_deref().unwrap_or("<anonymous>");
                                crate::dap::check_break(self, func_name)?;
                            }
                        }
                        if let Some(next) = next_closure.take() {
                            if !next.proto.is_verified() {
                                crate::bytecode::verify_proto(&next.proto)
                                    .map_err(|e| e.to_string())?;
                                next.proto.mark_verified();
                            }
                            current_closure = next;
                            exec = current_closure.proto.exec();
                            code = &exec.ops;
                            consts = current_closure.proto.values();
                        }
                        ip = 0;
                        continue;
                    }

                    // Fallback when `base == 0` or callee is Native / Table: run regular Call with MULTRET
                    // and let the trailing `Return { count: MULTRET }` at `ip` return results.
                    match reg!(callee).view() {
                        ValueView::Closure(callee_closure) => {
                            let callee_closure = callee_closure.to_rc();
                            if self.call_depth() >= self.max_call_depth {
                                throw!(
                                    "call stack overflow: exceeded maximum call depth of {}",
                                    self.max_call_depth
                                );
                            }
                            let same_closure = Rc::ptr_eq(&current_closure, &callee_closure);
                            let (max_registers, num_params, is_vararg) = {
                                let proto = &callee_closure.proto;
                                (
                                    proto.max_registers as usize,
                                    proto.num_params as usize,
                                    proto.is_vararg,
                                )
                            };
                            let mut next_closure = if same_closure {
                                None
                            } else {
                                Some(callee_closure)
                            };
                            let new_base = args_start;
                            let needed = new_base + max_registers;
                            if needed >= self.stack.len() {
                                self.stack.resize(needed + 1, Value::NIL);
                            }
                            for i in actual_argc..num_params {
                                self.stack[new_base + i] = Value::NIL;
                            }
                            let frame_closure = match &next_closure {
                                Some(c) => Rc::clone(c),
                                None => Rc::clone(&current_closure),
                            };
                            let new_frame = if is_vararg && actual_argc > num_params {
                                let varargs = self.stack
                                    [new_base + num_params..new_base + actual_argc]
                                    .to_vec();
                                CallFrame::with_varargs(frame_closure, new_base, varargs)
                            } else {
                                CallFrame::new(frame_closure, new_base)
                            }
                            .wanting(MULTRET);
                            self.frames.push(new_frame);
                            if self.debug_active {
                                if self.hook.is_some()
                                    && let Err(err) = self.fire_hook(HookEvent::Call)
                                {
                                    self.frames.pop();
                                    sync_ip!();
                                    return Err(err);
                                }
                                if self.dap.is_some() {
                                    let active = next_closure.as_ref().unwrap_or(&current_closure);
                                    let func_name =
                                        active.proto.name.as_deref().unwrap_or("<anonymous>");
                                    if let Err(err) = crate::dap::check_break(self, func_name) {
                                        self.frames.pop();
                                        sync_ip!();
                                        return Err(err);
                                    }
                                }
                            }
                            if let Some(next) = next_closure.take() {
                                if !next.proto.is_verified() {
                                    crate::bytecode::verify_proto(&next.proto)
                                        .map_err(|e| e.to_string())?;
                                    next.proto.mark_verified();
                                }
                                current_closure = next;
                                exec = current_closure.proto.exec();
                                code = &exec.ops;
                                consts = current_closure.proto.values();
                            }
                            base = new_base;
                            ip = 0;
                            continue;
                        }
                        ValueView::Native(def) => {
                            let func = def.func;
                            const INLINE_NATIVE_ARGS: usize = 8;
                            let mut inline_args = [const { Value::NIL }; INLINE_NATIVE_ARGS];
                            let heap_args: Vec<Value>;
                            let args: &[Value] = if actual_argc <= INLINE_NATIVE_ARGS {
                                for (dst, src) in inline_args[..actual_argc]
                                    .iter_mut()
                                    .zip(&self.stack[args_start..args_end])
                                {
                                    *dst = src.clone();
                                }
                                &inline_args[..actual_argc]
                            } else {
                                heap_args = self.stack[args_start..args_end].to_vec();
                                &heap_args
                            };
                            if self.debug_active
                                && self.hook.is_some()
                                && let Err(err) = self.fire_hook(HookEvent::Call)
                            {
                                sync_ip!();
                                return Err(err);
                            }
                            match func(self, args) {
                                Ok(results) => {
                                    if self.debug_active
                                        && self.hook.is_some()
                                        && let Err(err) = self.fire_hook(HookEvent::Return)
                                    {
                                        sync_ip!();
                                        return Err(err);
                                    }
                                    self.store_call_results(callee, MULTRET, results);
                                    if self.trap != TRAP {
                                        sync_ip!();
                                        return Ok(None);
                                    }
                                }
                                Err(err) if self.is_yielding => {
                                    if let Some(co_id) = self.current_co
                                        && let Some(co_rc) = self.co_state(co_id)
                                    {
                                        let mut cs = co_rc.borrow_mut();
                                        cs.yield_callee = callee;
                                        cs.yield_retc = MULTRET;
                                    }
                                    return Err(err);
                                }
                                Err(err) => return Err(err),
                            }
                        }
                        ValueView::Table(t) => {
                            let handler = t.borrow().metamethod("__call");
                            let callee_val = reg!(callee).clone();
                            if let Some(handler) = handler {
                                let mut call_args = Vec::with_capacity(actual_argc + 1);
                                call_args.push(callee_val);
                                call_args.extend_from_slice(&self.stack[args_start..args_end]);
                                let results = self.call_function(handler, &call_args)?;
                                self.store_call_results(callee, MULTRET, results);
                                if self.trap != TRAP {
                                    sync_ip!();
                                    return Ok(None);
                                }
                            } else {
                                throw!("attempted to call non-function ({:?})", callee_val);
                            }
                        }
                        _ => {
                            throw!("attempted to call non-function ({:?})", reg!(callee));
                        }
                    }
                }
                ExecInst::Call { callee, argc, retc } => {
                    let args_start = base + callee as usize + 1;
                    // MULTRET args run from the first argument register up to
                    // whatever the previous instruction left on the stack.
                    let args_end = if argc == MULTRET {
                        self.top.max(args_start)
                    } else {
                        args_start + argc as usize
                    };
                    let argc = args_end - args_start;
                    if args_end > self.stack.len() {
                        self.stack.resize(args_end, Value::NIL);
                    }
                    sync_ip!();

                    match reg!(callee).view() {
                        ValueView::Closure(callee_closure) => {
                            let callee_closure = callee_closure.to_rc();
                            if self.call_depth() >= self.max_call_depth {
                                throw!(
                                    "call stack overflow: exceeded maximum call depth of {}",
                                    self.max_call_depth
                                );
                            }
                            let same_closure = Rc::ptr_eq(&current_closure, &callee_closure);
                            let (max_registers, num_params, is_vararg) = {
                                let proto = &callee_closure.proto;
                                (
                                    proto.max_registers as usize,
                                    proto.num_params as usize,
                                    proto.is_vararg,
                                )
                            };
                            let mut next_closure = if same_closure {
                                None
                            } else {
                                Some(callee_closure)
                            };
                            let new_base = args_start;
                            let needed = new_base + max_registers;
                            if needed >= self.stack.len() {
                                self.stack.resize(needed + 1, Value::NIL);
                            }
                            for i in argc..num_params {
                                self.stack[new_base + i] = Value::NIL;
                            }
                            let frame_closure = match &next_closure {
                                Some(c) => Rc::clone(c),
                                None => Rc::clone(&current_closure),
                            };
                            let new_frame = if is_vararg && argc > num_params {
                                let varargs =
                                    self.stack[new_base + num_params..new_base + argc].to_vec();
                                CallFrame::with_varargs(frame_closure, new_base, varargs)
                            } else {
                                CallFrame::new(frame_closure, new_base)
                            }
                            .wanting(retc);
                            self.frames.push(new_frame);
                            if self.debug_active {
                                if self.hook.is_some()
                                    && let Err(err) = self.fire_hook(HookEvent::Call)
                                {
                                    self.frames.pop();
                                    sync_ip!();
                                    return Err(err);
                                }
                                if self.dap.is_some() {
                                    let active = next_closure.as_ref().unwrap_or(&current_closure);
                                    let func_name =
                                        active.proto.name.as_deref().unwrap_or("<anonymous>");
                                    if let Err(err) = crate::dap::check_break(self, func_name) {
                                        self.frames.pop();
                                        sync_ip!();
                                        return Err(err);
                                    }
                                }
                            }
                            if let Some(next) = next_closure.take() {
                                if !next.proto.is_verified() {
                                    crate::bytecode::verify_proto(&next.proto)
                                        .map_err(|e| e.to_string())?;
                                    next.proto.mark_verified();
                                }
                                current_closure = next;
                                exec = current_closure.proto.exec();
                                code = &exec.ops;
                                consts = current_closure.proto.values();
                            }
                            base = new_base;
                            ip = 0;
                            continue;
                        }
                        ValueView::Native(def) => {
                            let func = def.func;
                            const INLINE_NATIVE_ARGS: usize = 8;
                            let mut inline_args = [const { Value::NIL }; INLINE_NATIVE_ARGS];
                            let heap_args: Vec<Value>;
                            let args: &[Value] = if argc <= INLINE_NATIVE_ARGS {
                                for (dst, src) in inline_args[..argc]
                                    .iter_mut()
                                    .zip(&self.stack[args_start..args_end])
                                {
                                    *dst = src.clone();
                                }
                                &inline_args[..argc]
                            } else {
                                heap_args = self.stack[args_start..args_end].to_vec();
                                &heap_args
                            };
                            if self.debug_active
                                && self.hook.is_some()
                                && let Err(err) = self.fire_hook(HookEvent::Call)
                            {
                                sync_ip!();
                                return Err(err);
                            }
                            match func(self, args) {
                                Ok(results) => {
                                    if self.debug_active
                                        && self.hook.is_some()
                                        && let Err(err) = self.fire_hook(HookEvent::Return)
                                    {
                                        sync_ip!();
                                        return Err(err);
                                    }
                                    self.store_call_results(callee, retc, results);
                                    if self.trap != TRAP {
                                        sync_ip!();
                                        return Ok(None);
                                    }
                                }
                                Err(err) if self.is_yielding => {
                                    if let Some(co_id) = self.current_co
                                        && let Some(co_rc) = self.co_state(co_id)
                                    {
                                        let mut cs = co_rc.borrow_mut();
                                        cs.yield_callee = callee;
                                        cs.yield_retc = retc;
                                    }
                                    return Err(err);
                                }
                                Err(err) => return Err(err),
                            }
                        }
                        ValueView::Table(t) => {
                            let handler = t.borrow().metamethod("__call");
                            let callee_val = reg!(callee).clone();
                            if let Some(handler) = handler {
                                let mut call_args = Vec::with_capacity(argc + 1);
                                call_args.push(callee_val);
                                call_args.extend_from_slice(&self.stack[args_start..args_end]);
                                let results = self.call_function(handler, &call_args)?;
                                self.store_call_results(callee, retc, results);
                                if self.trap != TRAP {
                                    sync_ip!();
                                    return Ok(None);
                                }
                            } else {
                                throw!("attempted to call non-function ({:?})", callee_val);
                            }
                        }
                        _ => {
                            throw!("attempted to call non-function ({:?})", reg!(callee));
                        }
                    }
                }
                ExecInst::Return { base: ret, count } => {
                    if self.debug_active && self.hook.is_some() {
                        sync_ip!();
                        self.fire_hook(HookEvent::Return)?;
                    }
                    let frame = self.frames.pop().expect(RUNNING_FRAME);
                    self.beat_unwind_frames(self.frames.len() + 1);
                    if !self.open_upvalues.is_empty() {
                        self.close_upvalues(frame.base);
                    }
                    if count == 1 && frame.want_ret <= 1 && self.frames.len() > target_depth {
                        let ret_idx = frame.base + ret as usize;
                        let caller_dest = frame.base - 1;
                        let bits = slot(&self.stack, ret_idx).bits();
                        let v = if bits < TAG_RC_START {
                            unsafe { Value::from_trivial_bits(bits) }
                        } else {
                            std::mem::replace(slot_mut(&mut self.stack, ret_idx), Value::NIL)
                        };
                        slot_mut(&mut self.stack, caller_dest).set_inplace(v);
                        let caller_frame = self.frames.last().expect(RUNNING_FRAME);
                        if !Rc::ptr_eq(&current_closure, &caller_frame.closure) {
                            current_closure = caller_frame.closure.clone();
                            exec = current_closure.proto.exec();
                            code = &exec.ops;
                            consts = current_closure.proto.values();
                        }
                        base = caller_frame.base;
                        ip = caller_frame.ip;
                        if self.trap != TRAP {
                            sync_ip!();
                            return Ok(None);
                        }
                        continue;
                    }
                    let ret_start = frame.base + ret as usize;
                    let ret_end = if count == MULTRET {
                        self.top.max(ret_start)
                    } else {
                        ret_start + count as usize
                    };
                    let ret_end = ret_end.min(self.stack.len());
                    let nret = ret_end.saturating_sub(ret_start);

                    if self.frames.len() == target_depth {
                        self.returned.clear();
                        self.returned
                            .extend(self.stack[ret_start..ret_end].iter().cloned());
                        let top_val = self.returned.first().cloned().unwrap_or(Value::NIL);
                        return Ok(Some(top_val));
                    }
                    // The results land where the caller's `Call` put the callee.
                    let caller_dest = frame.base - 1;
                    let wanted = match frame.want_ret {
                        MULTRET => nret,
                        0 => 1,
                        n => n as usize,
                    };
                    let needed = caller_dest + wanted;
                    if needed > self.stack.len() {
                        self.stack.resize(needed, Value::NIL);
                    }
                    if wanted == 1 && nret == 1 {
                        let bits = self.stack[ret_start].bits();
                        let v = if bits < TAG_RC_START {
                            unsafe { Value::from_trivial_bits(bits) }
                        } else {
                            std::mem::replace(&mut self.stack[ret_start], Value::NIL)
                        };
                        self.stack[caller_dest].set_inplace(v);
                    } else {
                        for i in 0..wanted {
                            if i < nret {
                                let bits = self.stack[ret_start + i].bits();
                                let v = if bits < TAG_RC_START {
                                    unsafe { Value::from_trivial_bits(bits) }
                                } else {
                                    std::mem::replace(&mut self.stack[ret_start + i], Value::NIL)
                                };
                                self.stack[caller_dest + i].set_inplace(v);
                            } else {
                                self.stack[caller_dest + i].set_inplace(Value::NIL);
                            }
                        }
                    }
                    if frame.want_ret == MULTRET {
                        self.top = caller_dest + nret;
                    }
                    let caller_frame = self.frames.last().expect(RUNNING_FRAME);
                    if !Rc::ptr_eq(&current_closure, &caller_frame.closure) {
                        current_closure = caller_frame.closure.clone();
                        exec = current_closure.proto.exec();
                        code = &exec.ops;
                        consts = current_closure.proto.values();
                    }
                    base = caller_frame.base;
                    ip = caller_frame.ip;
                    if self.trap != TRAP {
                        sync_ip!();
                        return Ok(None);
                    }
                    continue;
                }
                ExecInst::Vararg { dst, count } => {
                    let all = count == 0 || count == MULTRET;
                    let frame = self.frames.last().expect(RUNNING_FRAME);
                    let cnt = if all {
                        frame.varargs().len()
                    } else {
                        count as usize
                    };
                    let dest = base + dst as usize;
                    if dest + cnt > self.stack.len() {
                        self.stack.resize(dest + cnt, Value::NIL);
                    }
                    let frame = self.frames.last().expect(RUNNING_FRAME);
                    let varargs = frame.varargs();
                    for i in 0..cnt {
                        let val = varargs.get(i).cloned().unwrap_or(Value::NIL);
                        self.stack[dest + i] = val;
                    }
                    if all {
                        self.top = dest + cnt;
                    }
                }
            }
        }
    }

    /// `==` with the `__eq` metamethod fallback for two tables.
    #[inline]
    pub(crate) fn values_equal(
        &mut self,
        base: usize,
        a: u8,
        b: u8,
        ip: usize,
    ) -> Result<bool, String> {
        let va = &self.stack[base + a as usize];
        let vb = &self.stack[base + b as usize];
        if va == vb {
            return Ok(true);
        }
        if va.is_table()
            && vb.is_table()
            && let Some(mm) = self.get_binop_metamethod(va, vb, "__eq")
        {
            let (va, vb) = (va.clone(), vb.clone());
            self.frames.last_mut().expect(RUNNING_FRAME).ip = ip;
            return self.call_cmp_mm(mm, va, vb);
        }
        Ok(false)
    }

    #[inline]
    pub(crate) fn values_equal_val(
        &mut self,
        va: &Value,
        vb: &Value,
        ip: usize,
    ) -> Result<bool, String> {
        if va == vb {
            return Ok(true);
        }
        if va.is_table()
            && vb.is_table()
            && let Some(mm) = self.get_binop_metamethod(va, vb, "__eq")
        {
            self.frames.last_mut().expect(RUNNING_FRAME).ip = ip;
            return self.call_cmp_mm(mm, va.clone(), vb.clone());
        }
        Ok(false)
    }

    /// The generic-for step. `b` is the loop's base register: iterator,
    /// state, control, then the loop variables.
    pub(crate) fn tfor_call(&mut self, base: usize, b: u8, retc: u8) -> Result<(), String> {
        let iter_idx = base + b as usize;
        let var_base = iter_idx + 3;
        let retc_usize = retc as usize;
        if var_base + retc_usize > self.stack.len() {
            self.stack.resize(var_base + retc_usize, Value::NIL);
        }

        // Fast path: direct iteration over a plain table (`for k, v in t`) or
        // after `pairs(t)` rewrites the iterator slot to the table on step 1.
        let direct_table = match self.stack[iter_idx].view() {
            ValueView::Table(t) => t.borrow().metamethod("__call").is_none(),
            ValueView::Native(def)
                if std::ptr::eq(def, crate::vm::builtins::NEXT)
                    && self.stack[iter_idx + 2].is_nil()
                    && self.stack[iter_idx + 1].is_table() =>
            {
                let tbl = std::mem::replace(&mut self.stack[iter_idx + 1], Value::NIL);
                self.stack[iter_idx] = tbl;
                true
            }
            _ => false,
        };
        if direct_table {
            let (head, tail) = self.stack.split_at_mut(iter_idx + 1);
            if let Some(t) = head[iter_idx].table_borrow() {
                let (state_slice, rest) = tail.split_at_mut(2);
                Self::table_for_step_into(&t, &mut state_slice[0], &mut rest[..retc_usize]);
                return Ok(());
            }
        }

        let iter_fn = self.stack[iter_idx].clone();
        let state = self.stack[iter_idx + 1].clone();
        let ctrl = self.stack[iter_idx + 2].clone();
        let results = self.call_function(iter_fn, &[state, ctrl])?;
        if var_base + retc_usize > self.stack.len() {
            self.stack.resize(var_base + retc_usize, Value::NIL);
        }
        for i in 0..retc_usize {
            self.stack[var_base + i] = results.get(i).cloned().unwrap_or(Value::NIL);
        }
        Ok(())
    }

    /// One step of `for k, v in t`, writing results straight into `vars` with
    /// zero intermediate allocations. `state_slot` holds the progress: nil
    /// before the first step, an integer while walking the array part, then a
    /// snapshot of the remaining keys (stored reversed, popped from the end).
    /// Entries removed during the loop are skipped; keys added after the
    /// snapshot are not visited.
    fn table_for_step_into(
        tbl: &crate::vm::value::TableBorrow<'_>,
        state_slot: &mut Value,
        vars: &mut [Value],
    ) {
        #[inline]
        fn write_entry(vars: &mut [Value], key: Value, value: Value) {
            if vars.len() > 1 {
                vars[0] = key;
                vars[1] = value;
                for slot in &mut vars[2..] {
                    *slot = Value::NIL;
                }
            } else if let Some(slot) = vars.first_mut() {
                *slot = value;
            }
        }

        let mut next_index = match state_slot.view() {
            ValueView::Int(i) => i as usize,
            ValueView::Table(_) => usize::MAX,
            _ => 0,
        };
        let arr = tbl.array_slice();
        while next_index < arr.len() {
            let v = &arr[next_index];
            next_index += 1;
            if !v.is_nil() {
                let v = v.clone();
                let idx_val = Value::from_usize(next_index);
                state_slot.set_inplace(idx_val.clone());
                write_entry(vars, idx_val, v);
                return;
            }
        }
        let snapshot = if let Some(snapshot) = state_slot.as_table() {
            snapshot.to_rc()
        } else {
            if tbl.fields_is_empty() && !tbl.has_general() {
                for slot in vars {
                    *slot = Value::NIL;
                }
                return;
            }
            let mut keys = tbl.sorted_keys();
            for k in &mut keys {
                if let Some(s) = k.as_str_ref()
                    && s.is_beat()
                {
                    *k = Value::str_ref(s.to_str_ref());
                }
            }
            keys.reverse();
            let mut snapshot = VmTable::new();
            snapshot.array = keys;
            *state_slot = crate::vm::heap::VmHeap::alloc_table_untracked(snapshot);
            state_slot.as_table().unwrap().to_rc()
        };
        let mut snapshot = snapshot.borrow_mut();
        while let Some(key) = snapshot.array.pop() {
            let value = tbl.raw_get(&key);
            if !value.is_nil() {
                drop(snapshot);
                write_entry(vars, key, value);
                return;
            }
        }
        drop(snapshot);
        for slot in vars {
            *slot = Value::NIL;
        }
    }
}
