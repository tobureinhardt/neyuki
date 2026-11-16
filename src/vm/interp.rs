//! Bytecode interpreter dispatch loop and loop-control helpers.

use std::cell::RefCell;
use std::rc::Rc;

use num_integer::Integer as _;
use num_traits::Signed as _;

use crate::bytecode::instruction::{Instruction, MULTRET};
use crate::vm::frame::CallFrame;
use crate::vm::machine::{HookEvent, RUNNING_FRAME, VM};
use crate::vm::ops;
use crate::vm::value::{Upvalue, Value, VmClosure, VmTable};

/// A register of the running frame. The interpreter loop only calls this
/// with indices below `base + max_registers` of a verified prototype, and
/// sizes the stack to cover that window before dispatching, so the bounds
/// check is skipped.
#[inline(always)]
fn slot(stack: &[Value], idx: usize) -> &Value {
    debug_assert!(idx < stack.len());
    // SAFETY: see above; the caller upholds `idx < stack.len()`.
    unsafe { stack.get_unchecked(idx) }
}

#[inline(always)]
fn slot_mut(stack: &mut [Value], idx: usize) -> &mut Value {
    debug_assert!(idx < stack.len());
    // SAFETY: as for `slot`.
    unsafe { stack.get_unchecked_mut(idx) }
}

impl VM {
    /// Line and count events for the instruction at `ip` of the running
    /// frame, and the debugger's line breakpoints and stepping.
    fn instruction_hook(&mut self, ip: usize) -> Result<(), String> {
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
            self.call_hook(&hook.func, &[Value::str("line"), Value::Int(line as i64)])?;
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
            return Ok(Some(Value::Nil));
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
            self.stack.resize(window_end, Value::Nil);
        }
        let mut current_closure = closure;
        let mut code: &[Instruction] = &current_closure.proto.instructions;
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
                let slot = slot_mut(&mut self.stack, base + $r as usize);
                match v {
                    Value::Int(new) => match slot {
                        Value::Int(old) => *old = new,
                        s => *s = Value::Int(new),
                    },
                    Value::Float(new) => match slot {
                        Value::Float(old) => *old = new,
                        s => *s = Value::Float(new),
                    },
                    Value::Bool(new) => match slot {
                        Value::Bool(old) => *old = new,
                        s => *s = Value::Bool(new),
                    },
                    other => *slot = other,
                }
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
                ($dst:expr, $a:expr, $b:expr, $checked:ident, $fop:tt, $slow:path, $event:literal) => {{
                    let ra = &reg!($a);
                    let rb = &reg!($b);
                    match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => match x.$checked(*y) {
                            Some(r) => {
                                let slot = slot_mut(&mut self.stack, base + $dst as usize);
                                if let Value::Int(old) = slot {
                                    *old = r;
                                } else {
                                    *slot = Value::Int(r);
                                }
                            }
                            None => {
                                let res = attempt!($slow(ra, rb));
                                set!($dst, res);
                            }
                        },
                        (Value::Float(x), Value::Float(y)) => {
                            let r = x $fop y;
                            let slot = slot_mut(&mut self.stack, base + $dst as usize);
                            if let Value::Float(old) = slot {
                                *old = r;
                            } else {
                                *slot = Value::Float(r);
                            }
                        }
                        (Value::Table(_), _) | (_, Value::Table(_)) => {
                            if let Some(mm) = self.get_binop_metamethod(ra, rb, $event) {
                                let (va, vb) = (ra.clone(), rb.clone());
                                sync_ip!();
                                self.call_binop_mm(mm, va, vb, base, $dst)?;
                                continue;
                            }
                            let res = attempt!($slow(ra, rb));
                            set!($dst, res);
                        }
                        _ => match (ra, rb) {
                            (Value::Int(x), Value::Float(y)) => {
                                let r = *x as f64 $fop *y;
                                let slot = slot_mut(&mut self.stack, base + $dst as usize);
                                if let Value::Float(old) = slot {
                                    *old = r;
                                } else {
                                    *slot = Value::Float(r);
                                }
                            }
                            (Value::Float(x), Value::Int(y)) => {
                                let r = *x $fop *y as f64;
                                let slot = slot_mut(&mut self.stack, base + $dst as usize);
                                if let Value::Float(old) = slot {
                                    *old = r;
                                } else {
                                    *slot = Value::Float(r);
                                }
                            }
                            _ => {
                                let res = attempt!($slow(ra, rb));
                                set!($dst, res);
                            }
                        },
                    }
                }};
            }
        // Arithmetic without an inline fast path (division, modulo,
        // power): straight to the evaluator, metamethods first.
        macro_rules! arith_slow {
            ($dst:expr, $a:expr, $b:expr, $slow:path, $event:literal) => {{
                let ra = &reg!($a);
                let rb = &reg!($b);
                if matches!(ra, Value::Table(_)) || matches!(rb, Value::Table(_)) {
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
                    let holds = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => x $op y,
                        (Value::Float(x), Value::Float(y)) => x $op y,
                        (Value::Table(_), _) | (_, Value::Table(_)) => {
                            let (l, r) = if $mm_swap { (rb, ra) } else { (ra, rb) };
                            if let Some(mm) = self.get_binop_metamethod(l, r, $event) {
                                let (vl, vr) = (l.clone(), r.clone());
                                sync_ip!();
                                self.call_cmp_mm(mm, vl, vr)?
                            } else {
                                attempt!($slow(ra, rb))
                            }
                        }
                        _ => attempt!($slow(ra, rb)),
                    };
                    if !holds {
                        jump!($jump);
                    }
                }};
            }

        loop {
            let Some(&inst) = code.get(ip) else {
                // Fell off the end without a `Return`: treat as returning nothing.
                sync_ip!();
                if self.hook.is_some() {
                    self.fire_hook(HookEvent::Return)?;
                }
                let frame = self.frames.pop().expect(RUNNING_FRAME);
                if !self.open_upvalues.is_empty() {
                    self.close_upvalues(frame.base);
                }
                if self.frames.len() == target_depth {
                    self.returned.clear();
                    return Ok(Some(Value::Nil));
                }
                let caller_frame = self.frames.last().expect(RUNNING_FRAME);
                current_closure = caller_frame.closure.clone();
                base = caller_frame.base;
                ip = caller_frame.ip;
                if self.trap != TRAP {
                    sync_ip!();
                    return Ok(None);
                }
                code = &current_closure.proto.instructions;
                consts = current_closure.proto.values();
                continue;
            };
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
                Instruction::LoadNil { dst } => set!(dst, Value::Nil),
                Instruction::LoadBool { dst, val } => set!(dst, Value::Bool(val)),
                Instruction::LoadInt { dst, val } => set!(dst, Value::Int(val as i64)),
                Instruction::LoadK { dst, k } => set!(dst, consts[k as usize].clone()),
                Instruction::Move { dst, src } => {
                    let src_slot = slot(&self.stack, base + src as usize);
                    match src_slot {
                        Value::Int(i) => {
                            let val = *i;
                            let d = slot_mut(&mut self.stack, base + dst as usize);
                            if let Value::Int(old) = d {
                                *old = val;
                            } else {
                                *d = Value::Int(val);
                            }
                        }
                        Value::Float(f) => {
                            let val = *f;
                            let d = slot_mut(&mut self.stack, base + dst as usize);
                            if let Value::Float(old) = d {
                                *old = val;
                            } else {
                                *d = Value::Float(val);
                            }
                        }
                        Value::Bool(b) => {
                            let val = *b;
                            let d = slot_mut(&mut self.stack, base + dst as usize);
                            if let Value::Bool(old) = d {
                                *old = val;
                            } else {
                                *d = Value::Bool(val);
                            }
                        }
                        _ => {
                            let v = reg!(src).clone();
                            set!(dst, v);
                        }
                    }
                }
                Instruction::GetGlobal { dst, name_k } => {
                    let Value::String(name) = &consts[name_k as usize] else {
                        throw!("global name must be string");
                    };
                    let val = self.globals.get(&**name).cloned().unwrap_or(Value::Nil);
                    set!(dst, val);
                }
                Instruction::SetGlobal { src, name_k } => {
                    let Value::String(name) = &consts[name_k as usize] else {
                        throw!("global name must be string");
                    };
                    let val = reg!(src).clone();
                    // Re-inserting an existing key would drop the old Rc for
                    // an identical one; a lookup first avoids that churn.
                    if let Some(slot) = self.globals.get_mut(&**name) {
                        *slot = val;
                    } else {
                        self.globals.insert(name.clone(), val);
                    }
                    self.globals_epoch = self.globals_epoch.wrapping_add(1);
                }
                Instruction::GetUpval { dst, upval_idx } => {
                    let uv = &current_closure.upvalues[upval_idx as usize];
                    match uv.location.get() {
                        Some(idx) => {
                            let src = &self.stack[idx];
                            match src {
                                Value::Int(i) => {
                                    let val = *i;
                                    let slot = slot_mut(&mut self.stack, base + dst as usize);
                                    if let Value::Int(old) = slot {
                                        *old = val;
                                    } else {
                                        *slot = Value::Int(val);
                                    }
                                }
                                Value::Float(f) => {
                                    let val = *f;
                                    let slot = slot_mut(&mut self.stack, base + dst as usize);
                                    if let Value::Float(old) = slot {
                                        *old = val;
                                    } else {
                                        *slot = Value::Float(val);
                                    }
                                }
                                Value::Bool(b) => {
                                    let val = *b;
                                    let slot = slot_mut(&mut self.stack, base + dst as usize);
                                    if let Value::Bool(old) = slot {
                                        *old = val;
                                    } else {
                                        *slot = Value::Bool(val);
                                    }
                                }
                                other => {
                                    let v = other.clone();
                                    set!(dst, v);
                                }
                            }
                        }
                        None => {
                            let val = uv.closed.borrow().clone();
                            set!(dst, val);
                        }
                    }
                }
                Instruction::SetUpval { src, upval_idx } => {
                    let val = reg!(src).clone();
                    self.upvalue_set(&current_closure.upvalues[upval_idx as usize], val);
                }
                Instruction::NewTable {
                    dst,
                    array_cap,
                    fields_cap,
                } => {
                    if self.gc.should_collect() || self.gc.sweeping {
                        self.gc_auto_step().inspect_err(|_| {
                            sync_ip!();
                        })?;
                    }
                    let table = if array_cap == 0 && fields_cap == 0 {
                        VmTable::new()
                    } else {
                        VmTable::with_capacity(array_cap as usize, fields_cap as usize)
                    };
                    let rc = Rc::new(RefCell::new(table));
                    let size = 64 + (array_cap as usize) * std::mem::size_of::<Value>();
                    self.gc.register_table_sized(&rc, size);
                    set!(dst, Value::Table(rc));
                }
                Instruction::GetTable { dst, table, key } => {
                    let tbl_val = &reg!(table);
                    let k = &reg!(key);
                    if let (Value::Table(t), Value::Int(i)) = (tbl_val, k)
                        && *i >= 1
                    {
                        let tbl = t.borrow();
                        let idx = *i as usize - 1;
                        if idx < tbl.array.len() {
                            let val = match &tbl.array[idx] {
                                Value::Int(n) => Value::Int(*n),
                                Value::Float(f) => Value::Float(*f),
                                Value::Bool(b) => Value::Bool(*b),
                                other => other.clone(),
                            };
                            drop(tbl);
                            set!(dst, val);
                            continue;
                        }
                    } else if let (Value::Table(t), Value::String(s)) = (tbl_val, k) {
                        let tbl = t.borrow();
                        if let Some(v) = tbl.fields.get_by_strref(s) {
                            let val = match v {
                                Value::Int(n) => Value::Int(*n),
                                Value::Float(f) => Value::Float(*f),
                                Value::Bool(b) => Value::Bool(*b),
                                other => other.clone(),
                            };
                            drop(tbl);
                            set!(dst, val);
                            continue;
                        } else if tbl.metatable.is_none() {
                            drop(tbl);
                            set!(dst, Value::Nil);
                            continue;
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
                Instruction::GetTableK { dst, table, key_k } => {
                    let tbl_val = &reg!(table);
                    let k = &consts[key_k as usize];
                    if let (Value::Table(t), Value::String(s)) = (tbl_val, k) {
                        let tbl = t.borrow();
                        if let Some(v) = tbl.fields.get_by_strref(s) {
                            let val = match v {
                                Value::Int(n) => Value::Int(*n),
                                Value::Float(f) => Value::Float(*f),
                                Value::Bool(b) => Value::Bool(*b),
                                other => other.clone(),
                            };
                            drop(tbl);
                            set!(dst, val);
                            continue;
                        } else if tbl.metatable.is_none() {
                            drop(tbl);
                            set!(dst, Value::Nil);
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
                Instruction::GetImport {
                    dst,
                    mod_k,
                    field_k,
                    site,
                } => {
                    let site = site as usize;
                    // Fast path: resolved under this epoch with the
                    // module still metatable-free. One epoch read, one
                    // pointer check, one clone instead of two hashes.
                    if let Some(cached) = current_closure
                        .proto
                        .import_cached(site, self.globals_epoch)
                    {
                        set!(dst, cached);
                    } else {
                        let (mname, fname) =
                            match (&consts[mod_k as usize], &consts[field_k as usize]) {
                                (Value::String(m), Value::String(f)) => (m.clone(), f.clone()),
                                _ => throw!("import names must be strings"),
                            };
                        // Dynamic resolution, exactly what GetGlobal
                        // plus GetTableK do: missing globals read nil.
                        let module_val = self.globals.get(&*mname).cloned().unwrap_or(Value::Nil);
                        let fkey = Value::String(fname);
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
                        if let Value::Table(t) = &module_val {
                            let cacheable = {
                                let tbl = t.borrow();
                                tbl.frozen
                                    && tbl.metatable.is_none()
                                    && !matches!(value, Value::Nil)
                            };
                            if cacheable {
                                current_closure.proto.import_store(
                                    site,
                                    self.globals_epoch,
                                    t.clone(),
                                    value.clone(),
                                );
                            }
                        }
                        set!(dst, value);
                    }
                }
                Instruction::SetTable { table, key, val } => {
                    let tbl_val = &reg!(table);
                    let k = &reg!(key);
                    let v = &reg!(val);
                    if let (Value::Table(t), Value::Int(i)) = (tbl_val, k)
                        && *i >= 1
                        && !matches!(v, Value::Nil)
                    {
                        let mut tbl = t.borrow_mut();
                        if !tbl.frozen {
                            let idx = *i as usize - 1;
                            if idx < tbl.array.len() {
                                match (v, &mut tbl.array[idx]) {
                                    (Value::Int(n), Value::Int(o)) => *o = *n,
                                    (Value::Float(n), Value::Float(o)) => *o = *n,
                                    (Value::Bool(n), Value::Bool(o)) => *o = *n,
                                    (v_other, slot) => *slot = v_other.clone(),
                                }
                                continue;
                            } else if idx == tbl.array.len()
                                && tbl.metatable.is_none()
                                && tbl.general.is_none()
                            {
                                tbl.array.push(v.clone());
                                continue;
                            }
                        }
                    } else if let (Value::Table(t), Value::String(s)) = (tbl_val, k)
                        && !matches!(v, Value::Nil)
                    {
                        let mut tbl = t.borrow_mut();
                        if !tbl.frozen {
                            if tbl.metatable.is_none() {
                                if let Some(slot) = tbl.fields.get_mut_by_strref(s) {
                                    match (v, slot) {
                                        (Value::Int(n), Value::Int(o)) => *o = *n,
                                        (Value::Float(n), Value::Float(o)) => *o = *n,
                                        (Value::Bool(n), Value::Bool(o)) => *o = *n,
                                        (val, slot) => *slot = val.clone(),
                                    }
                                } else {
                                    tbl.fields.insert(s.clone(), v.clone());
                                }
                                continue;
                            } else if let Some(slot) = tbl.fields.get_mut_by_strref(s) {
                                match (v, slot) {
                                    (Value::Int(n), Value::Int(o)) => *o = *n,
                                    (Value::Float(n), Value::Float(o)) => *o = *n,
                                    (Value::Bool(n), Value::Bool(o)) => *o = *n,
                                    (val, slot) => *slot = val.clone(),
                                }
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
                Instruction::SetTableK { table, key_k, val } => {
                    let tbl_val = &reg!(table);
                    let k = &consts[key_k as usize];
                    let v = &reg!(val);
                    if let (Value::Table(t), Value::String(s)) = (tbl_val, k)
                        && !matches!(v, Value::Nil)
                    {
                        let mut tbl = t.borrow_mut();
                        if !tbl.frozen {
                            if tbl.metatable.is_none() {
                                if let Some(slot) = tbl.fields.get_mut_by_strref(s) {
                                    match (v, slot) {
                                        (Value::Int(n), Value::Int(o)) => *o = *n,
                                        (Value::Float(n), Value::Float(o)) => *o = *n,
                                        (Value::Bool(n), Value::Bool(o)) => *o = *n,
                                        (val, slot) => *slot = val.clone(),
                                    }
                                } else {
                                    tbl.fields.insert(s.clone(), v.clone());
                                }
                                continue;
                            } else if let Some(slot) = tbl.fields.get_mut_by_strref(s) {
                                match (v, slot) {
                                    (Value::Int(n), Value::Int(o)) => *o = *n,
                                    (Value::Float(n), Value::Float(o)) => *o = *n,
                                    (Value::Bool(n), Value::Bool(o)) => *o = *n,
                                    (val, slot) => *slot = val.clone(),
                                }
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
                Instruction::AppendArray { table, src } => {
                    let val = reg!(src).clone();
                    if let Value::Table(t) = &reg!(table) {
                        t.borrow_mut().array.push(val);
                    } else {
                        throw!("cannot append to non-table");
                    }
                }
                Instruction::SetList {
                    table,
                    base: list,
                    count,
                } => {
                    let start = base + list as usize;
                    let end = if count == MULTRET {
                        self.top.max(start)
                    } else {
                        start + count as usize
                    };
                    let end = end.min(self.stack.len());
                    if let Value::Table(t) = &reg!(table) {
                        let t = t.clone();
                        let mut tbl = t.borrow_mut();
                        tbl.array.reserve(end.saturating_sub(start));
                        tbl.array.extend(self.stack[start..end].iter().cloned());
                    } else {
                        throw!("cannot SetList to non-table");
                    }
                }
                Instruction::TForCall { base: b, retc } => {
                    sync_ip!();
                    self.tfor_call(base, b, retc)?;
                }
                Instruction::TForLoop { base: b, jump } => {
                    let first_var = reg!(b + 3).clone();
                    if matches!(first_var, Value::Nil) {
                        // Exit loop: jump forward past the back-jump
                        jump!(jump);
                    } else {
                        // Update ctrl to first result, continue
                        set!(b + 2, first_var);
                    }
                }
                Instruction::CloseUpval { reg } => {
                    if !self.open_upvalues.is_empty() {
                        self.close_upvalue_at(base + reg as usize);
                    }
                }
                Instruction::Add { dst, a, b } => {
                    arith!(dst, a, b, checked_add, +, ops::eval_add, "__add")
                }
                Instruction::Sub { dst, a, b } => {
                    arith!(dst, a, b, checked_sub, -, ops::eval_sub, "__sub")
                }
                Instruction::Mul { dst, a, b } => {
                    arith!(dst, a, b, checked_mul, *, ops::eval_mul, "__mul")
                }
                Instruction::Div { dst, a, b } => {
                    arith_slow!(dst, a, b, ops::eval_div, "__div")
                }
                Instruction::IDiv { dst, a, b } => {
                    // Fast path for small ints; the zero divisor,
                    // MIN/-1 overflow, floats, BigInts and metamethods
                    // keep going through the generic evaluator so
                    // semantics cannot drift.
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => {
                            if *y == 0 || (*x == i64::MIN && *y == -1) {
                                attempt!(ops::eval_idiv(ra, rb))
                            } else {
                                Value::Int(x.div_floor(y))
                            }
                        }
                        _ => attempt!(ops::eval_idiv(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::Mod { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => {
                            if *y == 0 {
                                attempt!(ops::eval_mod(ra, rb))
                            } else if *y == -1 {
                                Value::Int(0)
                            } else {
                                Value::Int(x.mod_floor(y))
                            }
                        }
                        (Value::BigInt(x), Value::Int(y)) => {
                            if *y == 0 {
                                attempt!(ops::eval_mod(ra, rb))
                            } else {
                                Value::Int(ops::bigint_mod_floor_i64(x, *y))
                            }
                        }
                        _ => attempt!(ops::eval_mod(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::Pow { dst, a, b } => {
                    arith_slow!(dst, a, b, ops::eval_pow, "__pow")
                }
                Instruction::BitAnd { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => Value::Int(x & y),
                        _ => attempt!(ops::eval_bitand(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::BitOr { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => Value::Int(x | y),
                        _ => attempt!(ops::eval_bitor(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::BitXor { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => Value::Int(x ^ y),
                        _ => attempt!(ops::eval_bitxor(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::Shl { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) if *x >= 0 && *y >= 0 => {
                            if *x == 0 {
                                Value::Int(0)
                            } else {
                                let headroom = x.leading_zeros().saturating_sub(1) as i64;
                                let headroom128 = (64 + x.leading_zeros() as i64).saturating_sub(1);
                                if *y <= headroom {
                                    Value::Int(x << *y)
                                } else if *y <= headroom128 {
                                    Value::from_i128((*x as i128) << *y)
                                } else {
                                    let x_bits = 64 - x.leading_zeros() as u64;
                                    if (*y as u64) <= ops::MAX_INT_BITS
                                        && x_bits.saturating_add(*y as u64) <= ops::MAX_INT_BITS
                                    {
                                        Value::BigInt(std::rc::Rc::new(
                                            num_bigint::BigInt::from(*x) << (*y as usize),
                                        ))
                                    } else {
                                        attempt!(ops::eval_shl(ra, rb))
                                    }
                                }
                            }
                        }
                        (Value::BigInt(x), Value::Int(y)) if *y >= 0 => {
                            if (*y as u64) <= ops::MAX_INT_BITS
                                && x.bits().saturating_add(*y as u64) <= ops::MAX_INT_BITS
                            {
                                Value::BigInt(std::rc::Rc::new(&**x << (*y as usize)))
                            } else {
                                attempt!(ops::eval_shl(ra, rb))
                            }
                        }
                        _ => attempt!(ops::eval_shl(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::Shr { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => {
                            if *y < 0 {
                                attempt!(ops::eval_shl(ra, rb))
                            } else if *y >= 64 {
                                Value::Int(if *x < 0 { -1 } else { 0 })
                            } else {
                                Value::Int(x >> *y)
                            }
                        }
                        (Value::BigInt(x), Value::Int(y)) if *y >= 0 => {
                            if (*y as u64) >= x.bits() {
                                Value::Int(if x.is_negative() { -1 } else { 0 })
                            } else {
                                Value::from_bigint(&**x >> (*y as usize))
                            }
                        }
                        _ => attempt!(ops::eval_shr(ra, rb)),
                    };
                    set!(dst, res);
                }

                Instruction::LShl { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => {
                            let word = *x as u64;
                            let bits = if *y < 0 { usize::MAX } else { *y as usize };
                            let val = if bits >= 64 { 0 } else { word << bits };
                            Value::from_u64(val)
                        }
                        _ => attempt!(ops::eval_lshl(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::LShr { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => {
                            let word = *x as u64;
                            let bits = if *y < 0 { usize::MAX } else { *y as usize };
                            let val = if bits >= 64 { 0 } else { word >> bits };
                            Value::from_u64(val)
                        }
                        _ => attempt!(ops::eval_lshr(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::Concat { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::String(x), Value::String(y)) => {
                            let mut s = String::with_capacity(x.len() + y.len());
                            s.push_str(x);
                            s.push_str(y);
                            Value::string(s)
                        }
                        (Value::Table(_), _) | (_, Value::Table(_)) => {
                            if let Some(mm) = self.get_binop_metamethod(ra, rb, "__concat") {
                                let (va, vb) = (ra.clone(), rb.clone());
                                sync_ip!();
                                self.call_binop_mm(mm, va, vb, base, dst)?;
                                continue;
                            }
                            Value::string(format!("{}{}", ra, rb))
                        }
                        _ => Value::string(format!("{}{}", ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::Unm { dst, src } => {
                    let v = &reg!(src);
                    let res = match v {
                        Value::Int(i) if *i != i64::MIN => Value::Int(-i),
                        Value::Float(f) => Value::Float(-f),
                        Value::Table(_) => {
                            if let Some(mm) = self.get_unop_metamethod(v, "__unm") {
                                let v = v.clone();
                                sync_ip!();
                                let res = self.call_function(mm, &[v])?;
                                set!(dst, res.into_iter().next().unwrap_or(Value::Nil));
                                continue;
                            }
                            throw!("unary minus expects a number");
                        }
                        _ => attempt!(ops::eval_unm(v)),
                    };
                    set!(dst, res);
                }
                Instruction::Not { dst, src } => {
                    let truthy = reg!(src).is_truthy();
                    set!(dst, Value::Bool(!truthy));
                }
                Instruction::Len { dst, src } => {
                    let v = &reg!(src);
                    let len = match v {
                        Value::String(s) => s.len(),
                        Value::Table(t) => {
                            if let Some(mm) = self.get_unop_metamethod(v, "__len") {
                                let v = v.clone();
                                sync_ip!();
                                let res = self.call_function(mm, &[v])?;
                                set!(dst, res.into_iter().next().unwrap_or(Value::Nil));
                                continue;
                            }
                            t.borrow().array.len()
                        }
                        Value::Buffer(b) => b.borrow().len(),
                        _ => throw!("len expects string, table or buffer"),
                    };
                    set!(dst, Value::from_usize(len));
                }
                Instruction::BitNot { dst, src } => {
                    let res = attempt!(ops::eval_bitnot(&reg!(src)));
                    set!(dst, res);
                }
                Instruction::Coalesce { dst, a, b } => {
                    let va = &reg!(a);
                    let res = if matches!(va, Value::Nil) {
                        reg!(b).clone()
                    } else {
                        va.clone()
                    };
                    set!(dst, res);
                }
                Instruction::Eq {
                    a,
                    b,
                    jump_if_false,
                } => {
                    let is_eq = self.values_equal(base, a, b, ip)?;
                    if !is_eq {
                        jump!(jump_if_false);
                    }
                }
                Instruction::Ne {
                    a,
                    b,
                    jump_if_false,
                } => {
                    let is_eq = self.values_equal(base, a, b, ip)?;
                    if is_eq {
                        jump!(jump_if_false);
                    }
                }
                Instruction::Lt {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                Instruction::Le {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                Instruction::Gt {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, >, ops::eval_gt, "__lt", true)
                }
                Instruction::Ge {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, >=, ops::eval_ge, "__le", true)
                }
                Instruction::Test { reg, jump_if_false } => {
                    if !reg!(reg).is_truthy() {
                        jump!(jump_if_false);
                    }
                }
                Instruction::Jump { offset } => jump!(offset),
                Instruction::ForPrep { base: b, jump } => {
                    let step = &reg!(b + 2);
                    match step {
                        Value::Int(0) => throw!("numeric for step cannot be zero"),
                        Value::Float(f) if *f == 0.0 => {
                            throw!("numeric for step cannot be zero")
                        }
                        _ => {}
                    }
                    let init = &reg!(b);
                    if let (Value::Int(i), Value::Int(s)) = (init, step)
                        && let Some(diff) = i.checked_sub(*s)
                    {
                        let slot = slot_mut(&mut self.stack, base + b as usize);
                        if let Value::Int(old) = slot {
                            *old = diff;
                        } else {
                            *slot = Value::Int(diff);
                        }
                        jump!(jump);
                        continue;
                    }
                    let init_minus_step = attempt!(ops::eval_sub(init, step));
                    set!(b, init_minus_step);
                    jump!(jump);
                }
                Instruction::ForLoop { base: b, jump } => {
                    let idx = &reg!(b);
                    let limit = &reg!(b + 1);
                    let step = &reg!(b + 2);
                    // All-integer loops are the common case and need no
                    // allocation or generic dispatch at all.
                    if let (Value::Int(i), Value::Int(lim), Value::Int(st)) = (idx, limit, step)
                        && let Some(next) = i.checked_add(*st)
                    {
                        let again = if *st >= 0 { next <= *lim } else { next >= *lim };
                        let slot_b = slot_mut(&mut self.stack, base + b as usize);
                        if let Value::Int(old) = slot_b {
                            *old = next;
                        } else {
                            *slot_b = Value::Int(next);
                        }
                        if again {
                            let slot_v = slot_mut(&mut self.stack, base + (b + 3) as usize);
                            if let Value::Int(old) = slot_v {
                                *old = next;
                            } else {
                                *slot_v = Value::Int(next);
                            }
                            jump!(jump);
                        }
                        continue;
                    }
                    match step {
                        Value::Int(0) => throw!("numeric for step cannot be zero"),
                        Value::Float(f) if *f == 0.0 => {
                            throw!("numeric for step cannot be zero")
                        }
                        _ => {}
                    }
                    let is_positive = match step {
                        Value::Int(i) => *i >= 0,
                        Value::BigInt(i) => num_traits::Signed::is_positive(&**i),
                        Value::Float(f) => *f >= 0.0,
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
                Instruction::Closure { dst, proto_idx } => {
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
                                .unwrap_or_else(|| Upvalue::closed(Value::Nil));
                            upvalues.push(up);
                        }
                    }
                    let new_closure = Value::Closure(Rc::new(VmClosure {
                        proto: child_proto,
                        upvalues,
                    }));
                    set!(dst, new_closure);
                }
                Instruction::Call { callee, argc, retc } => {
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
                        self.stack.resize(args_end, Value::Nil);
                    }
                    sync_ip!();

                    match &reg!(callee) {
                        Value::Closure(callee_closure) => {
                            // Tail call: this `Call` is immediately
                            // followed by `Return` of its own results, so
                            // the current frame is replaced instead of
                            // pushing a new one, giving proper tail calls
                            // constant stack space. Only the exact shape
                            // the compilers emit for `return f(...)`
                            // qualifies (MULTRET on both sides with the
                            // results starting at the callee slot): any
                            // extra value would need an instruction
                            // between the two, which fails the adjacency
                            // check. A frame with no caller (base 0)
                            // cannot be replaced.
                            let tail = retc == MULTRET
                                && base >= 1
                                && matches!(
                                    code.get(ip),
                                    Some(Instruction::Return {
                                        base: ret,
                                        count: MULTRET,
                                    }) if *ret == callee
                                );
                            if !tail && self.call_depth() >= self.max_call_depth {
                                throw!(
                                    "call stack overflow: exceeded maximum call depth of {}",
                                    self.max_call_depth
                                );
                            }
                            let callee_closure = callee_closure.clone();
                            let proto = &callee_closure.proto;
                            // For a tail call the callee window slides
                            // down one slot so it sits exactly where a
                            // call from our caller would put it, and the
                            // callee inherits our return obligation.
                            let (new_base, want_ret) = if tail {
                                let cur = self.frames.pop().expect(RUNNING_FRAME);
                                if !self.open_upvalues.is_empty() {
                                    self.close_upvalues(cur.base);
                                }
                                // Relocate the whole callee window so the
                                // callee sits where our caller put us
                                // (`cur.base - 1`) and the arguments
                                // follow; the new frame keeps our base so
                                // its results land in our caller's slot.
                                let caller_dest = cur.base - 1;
                                let callee_abs = cur.base + callee as usize;
                                let shift = callee_abs - caller_dest;
                                let len = args_end - callee_abs;
                                for k in 0..len {
                                    let v = std::mem::replace(
                                        &mut self.stack[callee_abs + k],
                                        Value::Nil,
                                    );
                                    self.stack[caller_dest + k] = v;
                                }
                                if self.top > callee_abs {
                                    self.top -= shift;
                                }
                                (cur.base, cur.want_ret)
                            } else {
                                (args_start, retc)
                            };
                            let needed = new_base + proto.max_registers as usize;
                            if needed >= self.stack.len() {
                                self.stack.resize(needed + 1, Value::Nil);
                            }
                            let num_params = proto.num_params as usize;
                            for i in argc..num_params {
                                self.stack[new_base + i] = Value::Nil;
                            }
                            let varargs = if proto.is_vararg && argc > num_params {
                                self.stack[new_base + num_params..new_base + argc].to_vec()
                            } else {
                                Vec::new()
                            };
                            let same_closure = Rc::ptr_eq(&current_closure, &callee_closure);
                            let new_frame =
                                CallFrame::with_varargs(callee_closure.clone(), new_base, varargs)
                                    .wanting(want_ret);
                            self.frames.push(new_frame);
                            if self.hook.is_some() {
                                let event = if tail {
                                    HookEvent::TailCall
                                } else {
                                    HookEvent::Call
                                };
                                if let Err(err) = self.fire_hook(event) {
                                    self.frames.pop();
                                    sync_ip!();
                                    return Err(err);
                                }
                            }
                            if self.dap.is_some()
                                && let Err(err) = crate::dap::check_break(
                                    self,
                                    proto.name.as_deref().unwrap_or("<anonymous>"),
                                )
                            {
                                self.frames.pop();
                                sync_ip!();
                                return Err(err);
                            }
                            if !callee_closure.proto.is_verified() {
                                crate::bytecode::verify_proto(&callee_closure.proto)
                                    .map_err(|e| e.to_string())?;
                                callee_closure.proto.mark_verified();
                            }
                            if !same_closure {
                                current_closure = callee_closure;
                                code = &current_closure.proto.instructions;
                                consts = current_closure.proto.values();
                            }
                            base = new_base;
                            ip = 0;
                            continue;
                        }
                        Value::Native(def) => {
                            let func = def.func;
                            // Natives take a slice of owned values; copying
                            // the argument window is cheaper than exposing
                            // the stack to code that may resize it.
                            let args: Vec<Value> = self.stack[args_start..args_end].to_vec();
                            if self.hook.is_some()
                                && let Err(err) = self.fire_hook(HookEvent::Call)
                            {
                                sync_ip!();
                                return Err(err);
                            }
                            match func(self, &args) {
                                Ok(results) => {
                                    if self.hook.is_some()
                                        && let Err(err) = self.fire_hook(HookEvent::Return)
                                    {
                                        sync_ip!();
                                        return Err(err);
                                    }
                                    self.store_call_results(callee, retc, &results);
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
                        Value::Table(t) => {
                            let handler = t.borrow().metamethod("__call");
                            let callee_val = reg!(callee).clone();
                            if let Some(handler) = handler {
                                let mut call_args = Vec::with_capacity(argc + 1);
                                call_args.push(callee_val);
                                call_args.extend_from_slice(&self.stack[args_start..args_end]);
                                let results = self.call_function(handler, &call_args)?;
                                self.store_call_results(callee, retc, &results);
                                if self.trap != TRAP {
                                    sync_ip!();
                                    return Ok(None);
                                }
                            } else {
                                throw!("attempted to call non-function ({:?})", callee_val);
                            }
                        }
                        other => {
                            throw!("attempted to call non-function ({:?})", other);
                        }
                    }
                }
                Instruction::Return { base: ret, count } => {
                    sync_ip!();
                    if self.hook.is_some() {
                        self.fire_hook(HookEvent::Return)?;
                    }
                    let frame = self.frames.pop().expect(RUNNING_FRAME);
                    if !self.open_upvalues.is_empty() {
                        self.close_upvalues(frame.base);
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
                        let top_val = self.returned.first().cloned().unwrap_or(Value::Nil);
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
                        self.stack.resize(needed, Value::Nil);
                    }
                    // Values move down the stack in order; the source and
                    // destination windows may overlap, which is fine when
                    // copying front to back since caller_dest < ret_start.
                    if wanted == 1 && nret == 1 {
                        let src = &mut self.stack[ret_start];
                        match src {
                            Value::Int(n) => {
                                let val = *n;
                                let dest_slot = &mut self.stack[caller_dest];
                                if let Value::Int(old) = dest_slot {
                                    *old = val;
                                } else {
                                    *dest_slot = Value::Int(val);
                                }
                            }
                            Value::Float(f) => {
                                let val = *f;
                                let dest_slot = &mut self.stack[caller_dest];
                                if let Value::Float(old) = dest_slot {
                                    *old = val;
                                } else {
                                    *dest_slot = Value::Float(val);
                                }
                            }
                            _ => {
                                let v = std::mem::replace(src, Value::Nil);
                                self.stack[caller_dest] = v;
                            }
                        }
                    } else {
                        for i in 0..wanted {
                            if i < nret {
                                let src = &mut self.stack[ret_start + i];
                                match src {
                                    Value::Int(n) => {
                                        let val = *n;
                                        let dest_slot = &mut self.stack[caller_dest + i];
                                        if let Value::Int(old) = dest_slot {
                                            *old = val;
                                        } else {
                                            *dest_slot = Value::Int(val);
                                        }
                                    }
                                    Value::Float(f) => {
                                        let val = *f;
                                        let dest_slot = &mut self.stack[caller_dest + i];
                                        if let Value::Float(old) = dest_slot {
                                            *old = val;
                                        } else {
                                            *dest_slot = Value::Float(val);
                                        }
                                    }
                                    _ => {
                                        let v = std::mem::replace(src, Value::Nil);
                                        self.stack[caller_dest + i] = v;
                                    }
                                }
                            } else {
                                self.stack[caller_dest + i] = Value::Nil;
                            }
                        }
                    }
                    if frame.want_ret == MULTRET {
                        self.top = caller_dest + nret;
                    }
                    let caller_frame = self.frames.last().expect(RUNNING_FRAME);
                    if !Rc::ptr_eq(&current_closure, &caller_frame.closure) {
                        current_closure = caller_frame.closure.clone();
                        code = &current_closure.proto.instructions;
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
                Instruction::Vararg { dst, count } => {
                    let all = count == 0 || count == MULTRET;
                    let frame = self.frames.last().expect(RUNNING_FRAME);
                    let cnt = if all {
                        frame.varargs.len()
                    } else {
                        count as usize
                    };
                    let dest = base + dst as usize;
                    if dest + cnt > self.stack.len() {
                        self.stack.resize(dest + cnt, Value::Nil);
                    }
                    let frame = self.frames.last().expect(RUNNING_FRAME);
                    for i in 0..cnt {
                        let val = frame.varargs.get(i).cloned().unwrap_or(Value::Nil);
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
    fn values_equal(&mut self, base: usize, a: u8, b: u8, ip: usize) -> Result<bool, String> {
        let va = &self.stack[base + a as usize];
        let vb = &self.stack[base + b as usize];
        if va == vb {
            return Ok(true);
        }
        if let (Value::Table(_), Value::Table(_)) = (va, vb)
            && let Some(mm) = self.get_binop_metamethod(va, vb, "__eq")
        {
            let (va, vb) = (va.clone(), vb.clone());
            self.frames.last_mut().expect(RUNNING_FRAME).ip = ip;
            return self.call_cmp_mm(mm, va, vb);
        }
        Ok(false)
    }

    /// The generic-for step. `b` is the loop's base register: iterator,
    /// state, control, then the loop variables.
    fn tfor_call(&mut self, base: usize, b: u8, retc: u8) -> Result<(), String> {
        let iter_fn = self.stack[base + b as usize].clone();
        let state = self.stack[base + b as usize + 1].clone();
        let ctrl = self.stack[base + b as usize + 2].clone();
        let results = match &iter_fn {
            Value::Table(t) if t.borrow().metamethod("__call").is_none() => {
                let t = t.clone();
                self.table_for_step(&t, base + b as usize + 1, retc)
            }
            // `pairs(t)` hands back `next`: iterate the table directly,
            // which visits every key once without re-sorting per step.
            Value::Native(def)
                if std::ptr::eq(*def, crate::vm::builtins::NEXT) && matches!(ctrl, Value::Nil) =>
            {
                match &state {
                    Value::Table(t) => {
                        let t = t.clone();
                        self.stack[base + b as usize] = state.clone();
                        self.stack[base + b as usize + 1] = Value::Nil;
                        self.table_for_step(&t, base + b as usize + 1, retc)
                    }
                    _ => self.call_function(iter_fn, &[state, ctrl])?,
                }
            }
            _ => self.call_function(iter_fn, &[state, ctrl])?,
        };
        let var_base = base + b as usize + 3;
        if var_base + retc as usize > self.stack.len() {
            self.stack.resize(var_base + retc as usize, Value::Nil);
        }
        for i in 0..(retc as usize) {
            self.stack[var_base + i] = results.get(i).cloned().unwrap_or(Value::Nil);
        }
        Ok(())
    }

    /// One step of `for k, v in t`. The state register holds the progress:
    /// nil before the first step, an integer while walking the array part,
    /// then a snapshot of the remaining keys (stored reversed, popped from
    /// the end). Entries removed during the loop are skipped; keys added
    /// after the snapshot are not visited.
    fn table_for_step(
        &mut self,
        t: &Rc<RefCell<VmTable>>,
        state_reg: usize,
        retc: u8,
    ) -> Vec<Value> {
        let entry = |key: Value, value: Value| {
            if retc > 1 {
                vec![key, value]
            } else {
                vec![value]
            }
        };
        let tbl = t.borrow();
        let mut next_index = match &self.stack[state_reg] {
            Value::Int(i) => *i as usize,
            Value::Table(_) => usize::MAX,
            _ => 0,
        };
        while next_index < tbl.array.len() {
            let v = &tbl.array[next_index];
            next_index += 1;
            if !matches!(v, Value::Nil) {
                let v = v.clone();
                drop(tbl);
                self.stack[state_reg] = Value::from_usize(next_index);
                return entry(Value::from_usize(next_index), v);
            }
        }
        let snapshot = match &self.stack[state_reg] {
            Value::Table(snapshot) => snapshot.clone(),
            _ => {
                if tbl.fields.is_empty() && tbl.general.is_none() {
                    return vec![Value::Nil];
                }
                let mut keys = crate::vm::table::sorted_keys(&tbl);
                keys.reverse();
                let mut snapshot = VmTable::new();
                snapshot.array = keys;
                let snapshot = Rc::new(RefCell::new(snapshot));
                self.stack[state_reg] = Value::Table(snapshot.clone());
                snapshot
            }
        };
        let mut snapshot = snapshot.borrow_mut();
        while let Some(key) = snapshot.array.pop() {
            let value = tbl.raw_get(&key);
            if !matches!(value, Value::Nil) {
                return entry(key, value);
            }
        }
        vec![Value::Nil]
    }
}
