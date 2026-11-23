//! Release-only threaded tail-call bytecode interpreter (`TRAP == false`).
//!
//! Each opcode handler takes six scalar/pointer arguments in registers:
//! `(vm, regs, pc, consts, dtbl, raw)` where `regs = vm.stack.as_mut_ptr() + base`,
//! `pc` points to the next instruction after `raw`, `consts = proto.values().as_ptr()`,
//! and `tbl` carries `DISPATCH_FAST.as_ptr()`, ending every fast path with a tail call.
//! Slow paths, error construction, and `Drop`-bearing values stay in cold helpers
//! or inner blocks so no destructors remain live across a tail call.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use num_integer::Integer as _;

use crate::bytecode::exec::{AlignedExec, ExecInst, ExecProgram, NUM_EXEC_OPS, TableCacheSlot};
use crate::bytecode::instruction::MULTRET;
use crate::vm::frame::CallFrame;
use crate::vm::machine::{HookEvent, RUNNING_FRAME, VM};
use crate::vm::ops;
use crate::vm::value::{Upvalue, Value, VmClosure, VmTable};

#[repr(transparent)]
#[derive(Clone, Copy)]
struct DispatchTbl(*const FastHandler);

type FastHandler = fn(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8;

const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;

static DISPATCH_FAST: [FastHandler; NUM_EXEC_OPS] = [
    op_load_nil,           // 0
    op_load_bool,          // 1
    op_load_int,           // 2
    op_load_k,             // 3
    op_move,               // 4
    op_get_global,         // 5
    op_get_global_dynamic, // 6
    op_set_global,         // 7
    op_get_upval,          // 8
    op_set_upval,          // 9
    op_new_table,          // 10
    op_get_table,          // 11
    op_set_table,          // 12
    op_get_table_k,        // 13
    op_get_table_k_dyn,    // 14
    op_set_table_k,        // 15
    op_set_table_k_dyn,    // 16
    op_append_array,       // 17
    op_get_import,         // 18
    op_add,                // 19
    op_add_int,            // 20
    op_add_float,          // 21
    op_add_generic,        // 22
    op_add_k,              // 23
    op_add_int_k,          // 24
    op_add_float_k,        // 25
    op_add_k_generic,      // 26
    op_sub,                // 27
    op_sub_int,            // 28
    op_sub_float,          // 29
    op_sub_generic,        // 30
    op_sub_k,              // 31
    op_sub_int_k,          // 32
    op_sub_float_k,        // 33
    op_sub_k_generic,      // 34
    op_mul,                // 35
    op_mul_int,            // 36
    op_mul_float,          // 37
    op_mul_generic,        // 38
    op_mul_k,              // 39
    op_mul_int_k,          // 40
    op_mul_float_k,        // 41
    op_mul_k_generic,      // 42
    op_div,                // 43
    op_idiv,               // 44
    op_idiv_k,             // 45
    op_idiv_int_pow2,      // 46
    op_idiv_int_k,         // 47
    op_idiv_k_generic,     // 48
    op_mod,                // 49
    op_mod_k,              // 50
    op_mod_int_pow2,       // 51
    op_mod_int_k,          // 52
    op_mod_k_generic,      // 53
    op_pow,                // 54
    op_bit_and,            // 55
    op_bit_or,             // 56
    op_bit_xor,            // 57
    op_shl,                // 58
    op_shr,                // 59
    op_lshl,               // 60
    op_lshr,               // 61
    op_concat,             // 62
    op_unm,                // 63
    op_not,                // 64
    op_len,                // 65
    op_bit_not,            // 66
    op_coalesce,           // 67
    op_eq,                 // 68
    op_ne,                 // 69
    op_lt,                 // 70
    op_lt_int,             // 71
    op_lt_generic,         // 72
    op_le,                 // 73
    op_le_int,             // 74
    op_le_generic,         // 75
    op_gt,                 // 76
    op_ge,                 // 77
    op_eq_k,               // 78
    op_eq_int_k,           // 79
    op_eq_k_generic,       // 80
    op_ne_k,               // 81
    op_lt_k,               // 82
    op_lt_int_k,           // 83
    op_lt_k_generic,       // 84
    op_le_k,               // 85
    op_le_int_k,           // 86
    op_le_k_generic,       // 87
    op_gt_k,               // 88
    op_ge_k,               // 89
    op_test,               // 90
    op_jump,               // 91
    op_call,               // 92
    op_tail_call,          // 93
    op_return,             // 94
    op_closure,            // 95
    op_vararg,             // 96
    op_for_prep,           // 97
    op_for_loop,           // 98
    op_for_loop_int,       // 99
    op_for_loop_generic,   // 100
    op_set_list,           // 101
    op_tfor_call,          // 102
    op_tfor_loop,          // 103
    op_close_upval,        // 104
    op_add_i,              // 105
    op_add_int_i,          // 106
    op_add_i_generic,      // 107
    op_sub_i,              // 108
    op_sub_int_i,          // 109
    op_sub_i_generic,      // 110
    op_mul_i,              // 111
    op_mul_int_i,          // 112
    op_mul_i_generic,      // 113
    op_shl_i,              // 114
    op_shr_i,              // 115
    op_eq_i,               // 116
    op_ne_i,               // 117
    op_lt_i,               // 118
    op_lt_int_i,           // 119
    op_lt_i_generic,       // 120
    op_le_i,               // 121
    op_le_int_i,           // 122
    op_le_i_generic,       // 123
    op_gt_i,               // 124
    op_ge_i,               // 125
    op_end_sentinel,       // 126
    op_for_loop_count,     // 127
    op_for_loop_count1,    // 128
];

macro_rules! dispatch {
    ($vm:expr, $regs:expr, $pc:expr, $consts:expr, $dtbl:expr) => {{
        let next = (&*$pc).get();
        let handler = *$dtbl.0.add(next.0.tag());
        return handler($vm, $regs, $pc.add(1), $consts, $dtbl, next);
    }};
}

#[inline(always)]
unsafe fn ip_of(vm: &VM, pc: *const Cell<AlignedExec>) -> usize {
    unsafe { pc.offset_from((*vm.cur_exec).ops.as_ptr()) as usize }
}

#[inline(always)]
unsafe fn base_of(vm: &VM, regs: *mut Value) -> usize {
    unsafe { regs.offset_from(vm.stack.as_ptr()) as usize }
}

#[inline(always)]
fn is_trivial_drop(v: &Value) -> bool {
    matches!(
        v,
        Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Float(_) | Value::Native(_)
    )
}

#[cold]
#[inline(never)]
fn cold_write_int_dispatch(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
    val: i64,
) -> u8 {
    unsafe {
        *regs.add(dst as usize) = Value::Int(val);
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn cold_write_float_dispatch(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
    val: f64,
) -> u8 {
    unsafe {
        *regs.add(dst as usize) = Value::Float(val);
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn cold_write_bool_dispatch(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
    val: bool,
) -> u8 {
    unsafe {
        *regs.add(dst as usize) = Value::Bool(val);
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

macro_rules! write_int_dispatch {
    ($vm:expr, $regs:expr, $pc:expr, $consts:expr, $dtbl:expr, $dst:expr, $val:expr) => {{
        let _slot = $regs.add($dst as usize);
        let _v = $val;
        if let Value::Int(old) = &mut *_slot {
            *old = _v;
        } else if is_trivial_drop(&*_slot) {
            std::ptr::write(_slot, Value::Int(_v));
        } else {
            return cold_write_int_dispatch($vm, $regs, $pc, $consts, $dtbl, $dst, _v);
        }
        dispatch!($vm, $regs, $pc, $consts, $dtbl);
    }};
}

macro_rules! write_float_dispatch {
    ($vm:expr, $regs:expr, $pc:expr, $consts:expr, $dtbl:expr, $dst:expr, $val:expr) => {{
        let _slot = $regs.add($dst as usize);
        let _v = $val;
        if let Value::Float(old) = &mut *_slot {
            *old = _v;
        } else if is_trivial_drop(&*_slot) {
            std::ptr::write(_slot, Value::Float(_v));
        } else {
            return cold_write_float_dispatch($vm, $regs, $pc, $consts, $dtbl, $dst, _v);
        }
        dispatch!($vm, $regs, $pc, $consts, $dtbl);
    }};
}

macro_rules! write_bool_dispatch {
    ($vm:expr, $regs:expr, $pc:expr, $consts:expr, $dtbl:expr, $dst:expr, $val:expr) => {{
        let _slot = $regs.add($dst as usize);
        let _v = $val;
        if let Value::Bool(old) = &mut *_slot {
            *old = _v;
        } else if is_trivial_drop(&*_slot) {
            std::ptr::write(_slot, Value::Bool(_v));
        } else {
            return cold_write_bool_dispatch($vm, $regs, $pc, $consts, $dtbl, $dst, _v);
        }
        dispatch!($vm, $regs, $pc, $consts, $dtbl);
    }};
}

#[inline(always)]
unsafe fn write_int(slot: *mut Value, val: i64) {
    unsafe {
        let s = &mut *slot;
        match s {
            Value::Int(old) => *old = val,
            Value::Nil | Value::Bool(_) | Value::Float(_) | Value::Native(_) => {
                std::ptr::write(slot, Value::Int(val));
            }
            _ => *s = Value::Int(val),
        }
    }
}

#[inline(always)]
unsafe fn write_float(slot: *mut Value, val: f64) {
    unsafe {
        let s = &mut *slot;
        match s {
            Value::Float(old) => *old = val,
            Value::Nil | Value::Bool(_) | Value::Int(_) | Value::Native(_) => {
                std::ptr::write(slot, Value::Float(val));
            }
            _ => *s = Value::Float(val),
        }
    }
}

#[inline(always)]
unsafe fn write_bool(slot: *mut Value, val: bool) {
    unsafe {
        let s = &mut *slot;
        match s {
            Value::Bool(old) => *old = val,
            Value::Nil | Value::Int(_) | Value::Float(_) | Value::Native(_) => {
                std::ptr::write(slot, Value::Bool(val));
            }
            _ => *s = Value::Bool(val),
        }
    }
}

#[inline(always)]
unsafe fn set_slot_val(slot: *mut Value, val: Value) {
    unsafe {
        let s = &mut *slot;
        match val {
            Value::Int(n) => write_int(slot, n),
            Value::Float(f) => write_float(slot, f),
            Value::Bool(b) => write_bool(slot, b),
            other => match s {
                Value::Nil
                | Value::Bool(_)
                | Value::Int(_)
                | Value::Float(_)
                | Value::Native(_) => {
                    std::ptr::write(slot, other);
                }
                _ => *s = other,
            },
        }
    }
}

#[inline(always)]
unsafe fn copy_slot(dst: *mut Value, src: *const Value) {
    unsafe {
        match &*src {
            Value::Int(n) => write_int(dst, *n),
            Value::Float(f) => write_float(dst, *f),
            Value::Bool(b) => write_bool(dst, *b),
            Value::Nil => set_slot_val(dst, Value::Nil),
            Value::Native(def) => set_slot_val(dst, Value::Native(*def)),
            Value::Closure(c) => {
                if let Value::Closure(d) = &*dst
                    && Rc::ptr_eq(c, d)
                {
                    return;
                }
                set_slot_val(dst, Value::Closure(Rc::clone(c)));
            }
            Value::String(st) => {
                if let Value::String(d) = &*dst
                    && Rc::ptr_eq(&st.0, &d.0)
                {
                    return;
                }
                set_slot_val(dst, Value::String(st.clone()));
            }
            other => set_slot_val(dst, other.clone()),
        }
    }
}

#[cold]
#[inline(never)]
fn fail_with(vm: &mut VM, pc: *const Cell<AlignedExec>, err: String) -> u8 {
    let ip = unsafe { ip_of(vm, pc) };
    if let Some(frame) = vm.frames.last_mut() {
        frame.ip = ip;
    }
    vm.run_err = Some(err);
    STATUS_ERR
}

#[cold]
#[inline(never)]
fn fail_msg(vm: &mut VM, pc: *const Cell<AlignedExec>, msg: &str) -> u8 {
    fail_with(vm, pc, msg.to_string())
}

#[inline(always)]
fn sync_frame_pc(vm: &mut VM, pc: *const Cell<AlignedExec>) {
    let ip = unsafe { ip_of(vm, pc) };
    unsafe {
        vm.frames.last_mut().unwrap_unchecked().ip = ip;
    }
}

/// Saves and restores VM active-dispatch state across nested `run_fast_threaded`
/// invocations (e.g. `require`, `coroutine.resume`, `pcall`, metamethods).
struct SavedDispatchState {
    cur_consts: *const Value,
    cur_exec: *const ExecProgram,
    cur_closure: *const VmClosure,
    cur_upvals: *const Rc<Upvalue>,
    run_target_depth: usize,
    run_result: Option<Value>,
    run_err: Option<String>,
}

impl SavedDispatchState {
    fn save(vm: &mut VM) -> Self {
        Self {
            cur_consts: vm.cur_consts,
            cur_exec: vm.cur_exec,
            cur_closure: vm.cur_closure,
            cur_upvals: vm.cur_upvals,
            run_target_depth: vm.run_target_depth,
            run_result: vm.run_result.take(),
            run_err: vm.run_err.take(),
        }
    }

    fn restore(self, vm: &mut VM) {
        vm.cur_consts = self.cur_consts;
        vm.cur_exec = self.cur_exec;
        vm.cur_closure = self.cur_closure;
        vm.cur_upvals = self.cur_upvals;
        vm.run_target_depth = self.run_target_depth;
        vm.run_result = self.run_result;
        vm.run_err = self.run_err;
    }
}

pub(crate) fn run_fast_threaded(vm: &mut VM, target_depth: usize) -> Result<Option<Value>, String> {
    let (closure, base, ip) = {
        let f = vm.frames.last().expect(RUNNING_FRAME);
        (f.closure.clone(), f.base, f.ip)
    };
    if !closure.proto.is_verified() {
        crate::bytecode::verify_proto(&closure.proto).map_err(|e| e.to_string())?;
        closure.proto.mark_verified();
    }
    let window_end = base + closure.proto.max_registers as usize;
    if vm.stack.len() < window_end {
        vm.stack.resize(window_end, Value::Nil);
    }
    let saved = SavedDispatchState::save(vm);

    let exec = closure.proto.exec();
    let consts = closure.proto.values().as_ptr();
    vm.cur_exec = exec as *const ExecProgram;
    vm.cur_consts = consts;
    vm.cur_closure = Rc::as_ptr(&closure);
    vm.cur_upvals = closure.upvalues.as_ptr();
    vm.run_target_depth = target_depth;

    let regs = unsafe { vm.stack.as_mut_ptr().add(base) };
    let start_pc = unsafe { exec.ops.as_ptr().add(ip) };
    drop(closure);

    let status = unsafe {
        let dtbl = DispatchTbl(DISPATCH_FAST.as_ptr());
        let first_inst = (&*start_pc).get();
        let handler = *dtbl.0.add(first_inst.0.tag());
        handler(vm, regs, start_pc.add(1), consts, dtbl, first_inst)
    };
    let out = if status == STATUS_OK {
        Ok(vm.run_result.take())
    } else {
        Err(vm.run_err.take().unwrap_or_else(|| "VM error".to_string()))
    };
    saved.restore(vm);
    out
}

fn op_end_sentinel(
    vm: &mut VM,
    _regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    _consts: *const Value,
    dtbl: DispatchTbl,
    _raw: AlignedExec,
) -> u8 {
    unsafe {
        sync_frame_pc(vm, pc.sub(1));
        if vm.debug_active
            && vm.hook.is_some()
            && let Err(e) = vm.fire_hook(HookEvent::Return)
        {
            vm.run_err = Some(e);
            return STATUS_ERR;
        }
        let frame = vm.frames.pop().expect(RUNNING_FRAME);
        let frame_base = frame.base;
        drop(frame);
        vm.close_upvalues_if_needed(frame_base);
        if vm.frames.len() == vm.run_target_depth {
            vm.returned.clear();
            vm.run_result = Some(Value::Nil);
            return STATUS_OK;
        }
        let (caller_base, caller_ip, caller_exec, caller_consts, caller_closure, caller_upvals) = {
            let cf = vm.frames.last().expect(RUNNING_FRAME);
            let exec = cf.closure.proto.exec();
            (
                cf.base,
                cf.ip,
                exec as *const ExecProgram,
                cf.closure.proto.values().as_ptr(),
                Rc::as_ptr(&cf.closure),
                cf.closure.upvalues.as_ptr(),
            )
        };
        if vm.trap {
            vm.run_result = None;
            return STATUS_OK;
        }
        vm.cur_exec = caller_exec;
        vm.cur_consts = caller_consts;
        vm.cur_closure = caller_closure;
        vm.cur_upvals = caller_upvals;
        let new_pc = (*caller_exec).ops.as_ptr().add(caller_ip);
        let new_regs = vm.stack.as_mut_ptr().add(caller_base);
        dispatch!(vm, new_regs, new_pc, caller_consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn cold_write_nil_dispatch(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
) -> u8 {
    unsafe {
        *regs.add(dst as usize) = Value::Nil;
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn cold_copy_slot_dispatch(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    unsafe {
        match raw.0 {
            ExecInst::LoadK { dst, k } => {
                copy_slot(regs.add(dst as usize), consts.add(k as usize));
            }
            ExecInst::Move { dst, src } => {
                copy_slot(regs.add(dst as usize), regs.add(src as usize));
            }
            _ => std::hint::unreachable_unchecked(),
        }
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn cold_get_global_cached_dispatch(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (dst, name_k, site) = match raw.0 {
        ExecInst::GetGlobal { dst, name_k, site } => (dst, name_k, site),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    unsafe {
        if let Some(val) = (*vm.cur_exec).global_cache_get(site, vm.globals_epoch) {
            set_slot_val(regs.add(dst as usize), val);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    cold_get_global(vm, regs, pc, consts, dtbl, dst, name_k, Some(site))
}

fn op_load_nil(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::LoadNil { dst } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let slot = regs.add(dst as usize);
        if is_trivial_drop(&*slot) {
            std::ptr::write(slot, Value::Nil);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    cold_write_nil_dispatch(vm, regs, pc, consts, dtbl, dst)
}

fn op_load_bool(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::LoadBool { dst, val } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        write_bool_dispatch!(vm, regs, pc, consts, dtbl, dst, val);
    }
}

fn op_load_int(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::LoadInt { dst, val } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, val as i64);
    }
}

fn op_load_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::LoadK { dst, k } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let src = &*consts.add(k as usize);
        match src {
            Value::Int(n) => write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, *n),
            Value::Float(f) => write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *f),
            Value::Bool(b) => write_bool_dispatch!(vm, regs, pc, consts, dtbl, dst, *b),
            Value::Nil => {
                let slot = regs.add(dst as usize);
                if is_trivial_drop(&*slot) {
                    std::ptr::write(slot, Value::Nil);
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
            }
            Value::String(st) => {
                let slot = regs.add(dst as usize);
                if let Value::String(d) = &*slot
                    && Rc::ptr_eq(&st.0, &d.0)
                {
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
            }
            _ => {}
        }
    }
    cold_copy_slot_dispatch(vm, regs, pc, consts, dtbl, raw)
}

fn op_move(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Move { dst, src } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        match &*regs.add(src as usize) {
            Value::Int(n) => write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, *n),
            Value::Float(f) => write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *f),
            Value::Bool(b) => write_bool_dispatch!(vm, regs, pc, consts, dtbl, dst, *b),
            Value::Nil => {
                let slot = regs.add(dst as usize);
                if is_trivial_drop(&*slot) {
                    std::ptr::write(slot, Value::Nil);
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
            }
            Value::Native(def) => {
                let slot = regs.add(dst as usize);
                if is_trivial_drop(&*slot) {
                    std::ptr::write(slot, Value::Native(*def));
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
            }
            Value::Closure(c) => {
                let slot = regs.add(dst as usize);
                if let Value::Closure(d) = &*slot
                    && Rc::ptr_eq(c, d)
                {
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                if is_trivial_drop(&*slot) {
                    std::ptr::write(slot, Value::Closure(Rc::clone(c)));
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
            }
            Value::String(st) => {
                let slot = regs.add(dst as usize);
                if let Value::String(d) = &*slot
                    && Rc::ptr_eq(&st.0, &d.0)
                {
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                if is_trivial_drop(&*slot) {
                    std::ptr::write(slot, Value::String(st.clone()));
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
            }
            _ => {}
        }
    }
    cold_copy_slot_dispatch(vm, regs, pc, consts, dtbl, raw)
}

fn op_get_global(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GetGlobal {
        dst,
        name_k: _,
        site,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let Some(val) = (*vm.cur_exec).global_cache_ref(site, vm.globals_epoch) {
            match val {
                Value::Native(def) => {
                    let slot = regs.add(dst as usize);
                    if is_trivial_drop(&*slot) {
                        std::ptr::write(slot, Value::Native(*def));
                        dispatch!(vm, regs, pc, consts, dtbl);
                    }
                }
                Value::Int(n) => write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, *n),
                Value::Float(f) => write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *f),
                Value::Bool(b) => write_bool_dispatch!(vm, regs, pc, consts, dtbl, dst, *b),
                _ => {}
            }
        }
    }
    cold_get_global_cached_dispatch(vm, regs, pc, consts, dtbl, raw)
}

fn op_get_global_dynamic(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GetGlobalDynamic { dst, name_k } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    return cold_get_global(vm, regs, pc, consts, dtbl, dst, name_k, None);
}

#[cold]
#[inline(never)]
fn cold_get_global(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
    name_k: u16,
    site: Option<u16>,
) -> u8 {
    unsafe {
        let name = {
            let Value::String(name) = &*consts.add(name_k as usize) else {
                return fail_msg(vm, pc, "global name must be string");
            };
            name.clone()
        };
        let val = vm.globals.get(&*name).cloned().unwrap_or(Value::Nil);
        if let Some(site) = site {
            (*vm.cur_exec).global_cache_set(site, vm.globals_epoch, val.clone());
        }
        set_slot_val(regs.add(dst as usize), val);
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

fn op_set_global(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::SetGlobal { src, name_k } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    if !set_global_inner(vm, regs, consts, src, name_k) {
        return fail_msg(vm, pc, "global name must be string");
    }
    unsafe {
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

#[inline(always)]
fn set_global_inner(
    vm: &mut VM,
    regs: *mut Value,
    consts: *const Value,
    src: u8,
    name_k: u16,
) -> bool {
    unsafe {
        let Value::String(name) = &*consts.add(name_k as usize) else {
            return false;
        };
        let val = (*regs.add(src as usize)).clone();
        if let Some(slot) = vm.globals.get_mut(&**name) {
            *slot = val;
        } else {
            vm.globals.insert(name.clone(), val);
        }
        vm.globals_epoch = vm.globals_epoch.wrapping_add(1);
        true
    }
}

#[cold]
#[inline(never)]
fn cold_get_upval_dispatch(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GetUpval { dst, upval_idx } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let uv = &*vm.cur_upvals.add(upval_idx as usize);
        match uv.location.get() {
            Some(idx) => {
                copy_slot(regs.add(dst as usize), vm.stack.as_ptr().add(idx));
            }
            None => {
                uv.with_closed(|v| copy_slot(regs.add(dst as usize), v));
            }
        }
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn cold_set_upval_dispatch(
    vm: &mut VM,
    mut regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::SetUpval { src, upval_idx } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let uv = &*vm.cur_upvals.add(upval_idx as usize);
        match uv.location.get() {
            Some(idx) => {
                if idx < vm.stack.len() {
                    copy_slot(vm.stack.as_mut_ptr().add(idx), regs.add(src as usize));
                } else {
                    let base = base_of(vm, regs);
                    let val = (*regs.add(src as usize)).clone();
                    vm.set_reg_grow(idx, val);
                    regs = vm.stack.as_mut_ptr().add(base);
                }
            }
            None => {
                uv.set_closed_ref(&*regs.add(src as usize));
            }
        }
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

fn op_get_upval(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GetUpval { dst, upval_idx } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let uv = &*vm.cur_upvals.add(upval_idx as usize);
        let src_ptr: *const Value = match uv.location.get() {
            Some(idx) => vm.stack.as_ptr().add(idx),
            None => uv.closed_ptr() as *const Value,
        };
        match &*src_ptr {
            Value::Int(n) => write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, *n),
            Value::Float(f) => write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *f),
            Value::Bool(b) => write_bool_dispatch!(vm, regs, pc, consts, dtbl, dst, *b),
            Value::Nil => {
                let slot = regs.add(dst as usize);
                if is_trivial_drop(&*slot) {
                    std::ptr::write(slot, Value::Nil);
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
            }
            Value::Closure(c) => {
                let slot = regs.add(dst as usize);
                if let Value::Closure(d) = &*slot
                    && Rc::ptr_eq(c, d)
                {
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                if is_trivial_drop(&*slot) {
                    std::ptr::write(slot, Value::Closure(Rc::clone(c)));
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
            }
            Value::String(st) => {
                let slot = regs.add(dst as usize);
                if let Value::String(d) = &*slot
                    && Rc::ptr_eq(&st.0, &d.0)
                {
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                if is_trivial_drop(&*slot) {
                    std::ptr::write(slot, Value::String(st.clone()));
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
            }
            _ => {}
        }
    }
    cold_get_upval_dispatch(vm, regs, pc, consts, dtbl, raw)
}

fn op_set_upval(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::SetUpval { src, upval_idx } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let uv = &*vm.cur_upvals.add(upval_idx as usize);
        let dst_ptr: *mut Value = match uv.location.get() {
            Some(idx) if idx < vm.stack.len() => vm.stack.as_mut_ptr().add(idx),
            None => uv.closed_ptr(),
            _ => return cold_set_upval_dispatch(vm, regs, pc, consts, dtbl, raw),
        };
        match (&*regs.add(src as usize), &mut *dst_ptr) {
            (Value::Int(n), Value::Int(o)) => {
                *o = *n;
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            (Value::Float(f), Value::Float(o)) => {
                *o = *f;
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            (Value::Bool(b), Value::Bool(o)) => {
                *o = *b;
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            (Value::Int(n), d) if is_trivial_drop(d) => {
                std::ptr::write(dst_ptr, Value::Int(*n));
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            (Value::Float(f), d) if is_trivial_drop(d) => {
                std::ptr::write(dst_ptr, Value::Float(*f));
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            (Value::Bool(b), d) if is_trivial_drop(d) => {
                std::ptr::write(dst_ptr, Value::Bool(*b));
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            _ => {}
        }
    }
    cold_set_upval_dispatch(vm, regs, pc, consts, dtbl, raw)
}

#[inline(always)]
fn new_table_inner(
    vm: &mut VM,
    base: usize,
    dst: u8,
    array_cap: u8,
    fields_cap: u8,
) -> Result<*mut Value, String> {
    if vm.gc.should_collect() || vm.gc.sweeping {
        vm.gc_auto_step()?;
    }
    let table = if array_cap == 0 && fields_cap == 0 {
        VmTable::new()
    } else {
        VmTable::with_capacity(array_cap as usize, fields_cap as usize)
    };
    let rc = Rc::new(RefCell::new(table));
    let size = 64 + (array_cap as usize) * std::mem::size_of::<Value>();
    vm.gc.register_table_sized(&rc, size);
    unsafe {
        let new_regs = vm.stack.as_mut_ptr().add(base);
        set_slot_val(new_regs.add(dst as usize), Value::Table(rc));
        Ok(new_regs)
    }
}

fn op_new_table(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::NewTable {
        dst,
        array_cap,
        fields_cap,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    match new_table_inner(vm, base, dst, array_cap, fields_cap) {
        Ok(new_regs) => unsafe {
            dispatch!(vm, new_regs, pc, consts, dtbl);
        },
        Err(e) => fail_with(vm, pc, e),
    }
}

fn op_get_table(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GetTable { dst, table, key } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let tbl_val = &*regs.add(table as usize);
        let k = &*regs.add(key as usize);
        if let (Value::Table(t), Value::Int(i)) = (tbl_val, k)
            && *i >= 1
            && let Ok(tbl) = t.try_borrow()
        {
            let idx = *i as usize - 1;
            if idx < tbl.array.len() {
                match &tbl.array[idx] {
                    Value::Int(n) => {
                        let n = *n;
                        drop(tbl);
                        write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, n);
                    }
                    Value::Float(f) => {
                        let f = *f;
                        drop(tbl);
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, f);
                    }
                    Value::Bool(b) => {
                        let b = *b;
                        drop(tbl);
                        write_bool_dispatch!(vm, regs, pc, consts, dtbl, dst, b);
                    }
                    _ => {}
                }
            }
        }
    }
    cold_get_table(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_get_table(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GetTable { dst, table, key } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    let res = unsafe {
        let tbl_val = &*regs.add(table as usize);
        let k = &*regs.add(key as usize);
        match VM::table_get_fast(tbl_val, k) {
            Some(v) => Ok(v),
            None => {
                let (tbl, kv) = (tbl_val.clone(), k.clone());
                sync_frame_pc(vm, pc);
                vm.table_get(&tbl, &kv)
            }
        }
    };
    match res {
        Ok(val) => {
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), val);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
        }
        Err(e) => fail_with(vm, pc, e),
    }
}

fn op_get_table_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GetTableK {
        dst,
        table,
        key_k,
        site,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let tbl_val = &*regs.add(table as usize);
        let k = &*consts.add(key_k as usize);
        if let (Value::Table(t), Value::String(s)) = (tbl_val, k)
            && let Ok(tbl) = t.try_borrow()
        {
            let ptr = Rc::as_ptr(t) as usize;
            let shape = tbl.fields.shape_version();
            let exec = &*vm.cur_exec;
            if let Some(ic) = exec.table_cache_get(site)
                && ic.table_ptr == ptr
                && ic.shape_version == shape
                && let Some(v) = tbl.fields.get_slot_keyed_ptr(ic.slot_idx as usize, s)
            {
                match v {
                    Value::Int(n) => {
                        let n = *n;
                        drop(tbl);
                        write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, n);
                    }
                    Value::Float(f) => {
                        let f = *f;
                        drop(tbl);
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, f);
                    }
                    Value::Bool(b) => {
                        let b = *b;
                        drop(tbl);
                        write_bool_dispatch!(vm, regs, pc, consts, dtbl, dst, b);
                    }
                    _ => {}
                }
            }
        }
    }
    cold_get_table_k(vm, regs, pc, consts, dtbl, raw)
}

fn op_get_table_k_dyn(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    cold_get_table_k(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_get_table_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (dst, table, key_k, site_opt) = match raw.0 {
        ExecInst::GetTableK {
            dst,
            table,
            key_k,
            site,
        } => (dst, table, key_k, Some(site)),
        ExecInst::GetTableKDynamic { dst, table, key_k } => (dst, table, key_k, None),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    if let Some(site) = site_opt {
        unsafe {
            let tbl_val = &*regs.add(table as usize);
            let k = &*consts.add(key_k as usize);
            if let (Value::Table(t), Value::String(s)) = (tbl_val, k) {
                let ptr = Rc::as_ptr(t) as usize;
                let tbl = t.borrow();
                let shape = tbl.fields.shape_version();
                let exec = &*vm.cur_exec;
                if let Some(slot_idx) = tbl.fields.find_index_by_strref(s)
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
                    set_slot_val(regs.add(dst as usize), val);
                    dispatch!(vm, regs, pc, consts, dtbl);
                } else if tbl.metatable.is_none() {
                    drop(tbl);
                    set_slot_val(regs.add(dst as usize), Value::Nil);
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
            }
        }
    }
    let base = unsafe { base_of(vm, regs) };
    let res = unsafe {
        let tbl_val = &*regs.add(table as usize);
        let k = &*consts.add(key_k as usize);
        match VM::table_get_fast(tbl_val, k) {
            Some(v) => Ok(v),
            None => {
                let tbl = tbl_val.clone();
                let kv = k.clone();
                sync_frame_pc(vm, pc);
                vm.table_get(&tbl, &kv)
            }
        }
    };
    match res {
        Ok(val) => {
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), val);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
        }
        Err(e) => fail_with(vm, pc, e),
    }
}

#[inline(always)]
unsafe fn try_set_table_fast(regs: *mut Value, table: u8, key: u8, val: u8) -> bool {
    unsafe {
        let tbl_val = &*regs.add(table as usize);
        let k = &*regs.add(key as usize);
        let v = &*regs.add(val as usize);
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
                    return true;
                } else if idx == tbl.array.len() && tbl.metatable.is_none() && tbl.general.is_none()
                {
                    tbl.array.push(v.clone());
                    return true;
                }
            }
        } else if let (Value::Table(t), Value::String(s)) = (tbl_val, k)
            && !matches!(v, Value::Nil)
        {
            let mut tbl = t.borrow_mut();
            if !tbl.frozen {
                if tbl.metatable.is_none() {
                    tbl.fields.set_by_strref(s, v);
                    return true;
                } else if let Some(slot) = tbl.fields.get_mut_by_strref(s) {
                    crate::vm::table::assign_value(slot, v);
                    return true;
                }
            }
        }
        VM::table_set_fast(tbl_val, k, v)
    }
}

fn op_set_table(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::SetTable { table, key, val } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let tbl_val = &*regs.add(table as usize);
        let k = &*regs.add(key as usize);
        let v = &*regs.add(val as usize);
        if let (Value::Table(t), Value::Int(i)) = (tbl_val, k)
            && *i >= 1
            && !matches!(v, Value::Nil)
            && let Ok(mut tbl) = t.try_borrow_mut()
            && !tbl.frozen
        {
            let idx = *i as usize - 1;
            if idx < tbl.array.len() {
                let slot = &mut tbl.array[idx];
                match (v, slot) {
                    (Value::Int(n), Value::Int(o)) => {
                        *o = *n;
                        drop(tbl);
                        dispatch!(vm, regs, pc, consts, dtbl);
                    }
                    (Value::Float(n), Value::Float(o)) => {
                        *o = *n;
                        drop(tbl);
                        dispatch!(vm, regs, pc, consts, dtbl);
                    }
                    (Value::Bool(n), Value::Bool(o)) => {
                        *o = *n;
                        drop(tbl);
                        dispatch!(vm, regs, pc, consts, dtbl);
                    }
                    _ => {}
                }
            }
        }
    }
    cold_set_table(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_set_table(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::SetTable { table, key, val } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if try_set_table_fast(regs, table, key, val) {
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    let base = unsafe { base_of(vm, regs) };
    let res = unsafe {
        let (tbl, k, v) = (
            (*regs.add(table as usize)).clone(),
            (*regs.add(key as usize)).clone(),
            (*regs.add(val as usize)).clone(),
        );
        sync_frame_pc(vm, pc);
        vm.table_set(&tbl, k, v)
    };
    match res {
        Ok(()) => {
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
        }
        Err(e) => fail_with(vm, pc, e),
    }
}

#[inline(always)]
unsafe fn try_set_table_k_fast(
    vm: &VM,
    regs: *mut Value,
    consts: *const Value,
    table: u8,
    key_k: u16,
    val: u8,
    site: u16,
) -> bool {
    unsafe {
        let tbl_val = &*regs.add(table as usize);
        let k = &*consts.add(key_k as usize);
        let v = &*regs.add(val as usize);
        if let (Value::Table(t), Value::String(s)) = (tbl_val, k)
            && !matches!(v, Value::Nil)
        {
            let ptr = Rc::as_ptr(t) as usize;
            let mut tbl = t.borrow_mut();
            if !tbl.frozen {
                let shape = tbl.fields.shape_version();
                let exec = &*vm.cur_exec;
                if let Some(ic) = exec.table_cache_get(site)
                    && ic.table_ptr == ptr
                    && ic.shape_version == shape
                    && tbl
                        .fields
                        .set_slot_keyed(ic.slot_idx as usize, s, v.clone())
                {
                    return true;
                }
                if let Some(slot_idx) = tbl.fields.find_index_by_strref(s) {
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
                    return true;
                } else if tbl.metatable.is_none() {
                    let slot_idx = tbl.fields.insert_cached(s.clone(), v.clone());
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
                    return true;
                }
            }
        }
        VM::table_set_fast(tbl_val, k, v)
    }
}

fn op_set_table_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::SetTableK {
        table,
        key_k,
        val,
        site,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let tbl_val = &*regs.add(table as usize);
        let k = &*consts.add(key_k as usize);
        let v = &*regs.add(val as usize);
        if let (Value::Table(t), Value::String(s)) = (tbl_val, k)
            && !matches!(v, Value::Nil)
            && let Ok(mut tbl) = t.try_borrow_mut()
            && !tbl.frozen
        {
            let ptr = Rc::as_ptr(t) as usize;
            let shape = tbl.fields.shape_version();
            let exec = &*vm.cur_exec;
            if let Some(ic) = exec.table_cache_get(site)
                && ic.table_ptr == ptr
                && ic.shape_version == shape
                && match v {
                    Value::Int(n) => {
                        if let Some(slot_v) =
                            tbl.fields.get_slot_keyed_mut_ptr(ic.slot_idx as usize, s)
                        {
                            match slot_v {
                                Value::Int(o) => {
                                    *o = *n;
                                    true
                                }
                                ref d if is_trivial_drop(d) => {
                                    std::ptr::write(slot_v, Value::Int(*n));
                                    true
                                }
                                _ => false,
                            }
                        } else {
                            false
                        }
                    }
                    Value::Float(f) => {
                        if let Some(slot_v) =
                            tbl.fields.get_slot_keyed_mut_ptr(ic.slot_idx as usize, s)
                        {
                            match slot_v {
                                Value::Float(o) => {
                                    *o = *f;
                                    true
                                }
                                ref d if is_trivial_drop(d) => {
                                    std::ptr::write(slot_v, Value::Float(*f));
                                    true
                                }
                                _ => false,
                            }
                        } else {
                            false
                        }
                    }
                    Value::Bool(b) => {
                        if let Some(slot_v) =
                            tbl.fields.get_slot_keyed_mut_ptr(ic.slot_idx as usize, s)
                        {
                            match slot_v {
                                Value::Bool(o) => {
                                    *o = *b;
                                    true
                                }
                                ref d if is_trivial_drop(d) => {
                                    std::ptr::write(slot_v, Value::Bool(*b));
                                    true
                                }
                                _ => false,
                            }
                        } else {
                            false
                        }
                    }
                    _ => false,
                }
            {
                drop(tbl);
                dispatch!(vm, regs, pc, consts, dtbl);
            }
        }
    }
    cold_set_table_k(vm, regs, pc, consts, dtbl, raw)
}

fn op_set_table_k_dyn(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    cold_set_table_k(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_set_table_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (table, key_k, val, site_opt) = match raw.0 {
        ExecInst::SetTableK {
            table,
            key_k,
            val,
            site,
        } => (table, key_k, val, Some(site)),
        ExecInst::SetTableKDynamic { table, key_k, val } => (table, key_k, val, None),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    if let Some(site) = site_opt {
        unsafe {
            if try_set_table_k_fast(vm, regs, consts, table, key_k, val, site) {
                dispatch!(vm, regs, pc, consts, dtbl);
            }
        }
    } else {
        unsafe {
            let tbl_val = &*regs.add(table as usize);
            let k = &*consts.add(key_k as usize);
            let v = &*regs.add(val as usize);
            if VM::table_set_fast(tbl_val, k, v) {
                dispatch!(vm, regs, pc, consts, dtbl);
            }
        }
    }
    let base = unsafe { base_of(vm, regs) };
    let res = unsafe {
        let (tbl, k, v) = (
            (*regs.add(table as usize)).clone(),
            (*consts.add(key_k as usize)).clone(),
            (*regs.add(val as usize)).clone(),
        );
        sync_frame_pc(vm, pc);
        vm.table_set(&tbl, k, v)
    };
    match res {
        Ok(()) => {
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
        }
        Err(e) => fail_with(vm, pc, e),
    }
}

#[inline(always)]
unsafe fn append_array_inner(regs: *mut Value, table: u8, src: u8) -> bool {
    unsafe {
        let val = (&*regs.add(src as usize)).clone();
        if let Value::Table(t) = &*regs.add(table as usize) {
            t.borrow_mut().array.push(val);
            true
        } else {
            false
        }
    }
}

#[cold]
#[inline(never)]
fn cold_append_array(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let ExecInst::AppendArray { table, src } = raw.0 else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if !append_array_inner(regs, table, src) {
            return fail_msg(vm, pc, "cannot append to non-table");
        }
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

fn op_append_array(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::AppendArray { table, src } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let Value::Table(t) = &*regs.add(table as usize)
            && let Ok(mut tbl) = t.try_borrow_mut()
            && tbl.array.len() < tbl.array.capacity()
        {
            let len = tbl.array.len();
            match &*regs.add(src as usize) {
                Value::Int(n) => {
                    std::ptr::write(tbl.array.as_mut_ptr().add(len), Value::Int(*n));
                    tbl.array.set_len(len + 1);
                    drop(tbl);
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                Value::Float(f) => {
                    std::ptr::write(tbl.array.as_mut_ptr().add(len), Value::Float(*f));
                    tbl.array.set_len(len + 1);
                    drop(tbl);
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                Value::Bool(b) => {
                    std::ptr::write(tbl.array.as_mut_ptr().add(len), Value::Bool(*b));
                    tbl.array.set_len(len + 1);
                    drop(tbl);
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                _ => {}
            }
        }
    }
    cold_append_array(vm, regs, pc, consts, dtbl, raw)
}

fn op_get_import(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GetImport {
        dst,
        mod_k: _,
        field_k: _,
        site,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let Some(cached) = (*vm.cur_exec).import_cached(site as usize, vm.globals_epoch) {
            set_slot_val(regs.add(dst as usize), cached);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    return cold_get_import(vm, regs, pc, consts, dtbl, raw);
}

#[cold]
#[inline(never)]
fn cold_get_import(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let ExecInst::GetImport {
        dst,
        mod_k,
        field_k,
        site,
    } = raw.0
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    let res: Result<Value, String> = unsafe {
        let (mname, fname) = match (&*consts.add(mod_k as usize), &*consts.add(field_k as usize)) {
            (Value::String(m), Value::String(f)) => (m.clone(), f.clone()),
            _ => return fail_msg(vm, pc, "import names must be strings"),
        };
        let module_val = vm.globals.get(&*mname).cloned().unwrap_or(Value::Nil);
        let fkey = Value::String(fname);
        let value = match VM::table_get_fast(&module_val, &fkey) {
            Some(v) => v,
            None => {
                sync_frame_pc(vm, pc);
                match vm.table_get(&module_val, &fkey) {
                    Ok(v) => v,
                    Err(e) => return fail_with(vm, pc, e),
                }
            }
        };
        if let Value::Table(t) = &module_val {
            let cacheable = {
                let tbl = t.borrow();
                tbl.frozen && tbl.metatable.is_none() && !matches!(value, Value::Nil)
            };
            if cacheable {
                (*vm.cur_exec).import_store(
                    site as usize,
                    vm.globals_epoch,
                    t.clone(),
                    value.clone(),
                );
            }
        }
        Ok(value)
    };
    match res {
        Ok(value) => {
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), value);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
        }
        Err(e) => fail_with(vm, pc, e),
    }
}

#[cold]
#[inline(never)]
fn cold_arith_binop(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (dst, a, b, slow, event, into): (
        u8,
        u8,
        u8,
        fn(&Value, &Value) -> Result<Value, String>,
        &'static str,
        Option<fn(&mut [Value], usize, usize, usize) -> Result<(), String>>,
    ) = match raw.0 {
        ExecInst::Add { dst, a, b }
        | ExecInst::AddInt { dst, a, b }
        | ExecInst::AddFloat { dst, a, b }
        | ExecInst::AddGeneric { dst, a, b } => {
            (dst, a, b, ops::eval_add, "__add", Some(ops::eval_add_into))
        }
        ExecInst::Sub { dst, a, b }
        | ExecInst::SubInt { dst, a, b }
        | ExecInst::SubFloat { dst, a, b }
        | ExecInst::SubGeneric { dst, a, b } => {
            (dst, a, b, ops::eval_sub, "__sub", Some(ops::eval_sub_into))
        }
        ExecInst::Mul { dst, a, b }
        | ExecInst::MulInt { dst, a, b }
        | ExecInst::MulFloat { dst, a, b }
        | ExecInst::MulGeneric { dst, a, b } => (dst, a, b, ops::eval_mul, "__mul", None),
        ExecInst::Div { dst, a, b } => (dst, a, b, ops::eval_div, "__div", None),
        ExecInst::Pow { dst, a, b } => (dst, a, b, ops::eval_pow, "__pow", None),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if (matches!(ra, Value::Table(_)) || matches!(rb, Value::Table(_)))
            && let Some(mm) = vm.get_binop_metamethod(ra, rb, event)
        {
            let (va, vb) = (ra.clone(), rb.clone());
            sync_frame_pc(vm, pc);
            if let Err(e) = vm.call_binop_mm(mm, va, vb, base, dst) {
                return fail_with(vm, pc, e);
            }
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            let new_regs = vm.stack.as_mut_ptr().add(base);
            dispatch!(vm, new_regs, pc, consts, dtbl);
        }
        if let Some(into_fn) = into {
            if let Err(e) = into_fn(
                &mut vm.stack,
                base + dst as usize,
                base + a as usize,
                base + b as usize,
            ) {
                return fail_with(vm, pc, e);
            }
        } else {
            match slow(ra, rb) {
                Ok(res) => set_slot_val(regs.add(dst as usize), res),
                Err(e) => return fail_with(vm, pc, e),
            }
        }
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn cold_arith_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (dst, a, k, slow, event): (
        u8,
        u8,
        u16,
        fn(&Value, &Value) -> Result<Value, String>,
        &'static str,
    ) = match raw.0 {
        ExecInst::AddK { dst, a, k }
        | ExecInst::AddIntK { dst, a, k }
        | ExecInst::AddFloatK { dst, a, k }
        | ExecInst::AddKGeneric { dst, a, k } => (dst, a, k, ops::eval_add, "__add"),
        ExecInst::SubK { dst, a, k }
        | ExecInst::SubIntK { dst, a, k }
        | ExecInst::SubFloatK { dst, a, k }
        | ExecInst::SubKGeneric { dst, a, k } => (dst, a, k, ops::eval_sub, "__sub"),
        ExecInst::MulK { dst, a, k }
        | ExecInst::MulIntK { dst, a, k }
        | ExecInst::MulFloatK { dst, a, k }
        | ExecInst::MulKGeneric { dst, a, k } => (dst, a, k, ops::eval_mul, "__mul"),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        let rb = &*consts.add(k as usize);
        if (matches!(ra, Value::Table(_)) || matches!(rb, Value::Table(_)))
            && let Some(mm) = vm.get_binop_metamethod(ra, rb, event)
        {
            let (va, vb) = (ra.clone(), rb.clone());
            sync_frame_pc(vm, pc);
            if let Err(e) = vm.call_binop_mm(mm, va, vb, base, dst) {
                return fail_with(vm, pc, e);
            }
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            let new_regs = vm.stack.as_mut_ptr().add(base);
            dispatch!(vm, new_regs, pc, consts, dtbl);
        }
        match slow(ra, rb) {
            Ok(res) => {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), res);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
            Err(e) => fail_with(vm, pc, e),
        }
    }
}

#[cold]
#[inline(never)]
fn cold_arith_imm(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (dst, a, imm, slow, event): (
        u8,
        u8,
        i16,
        fn(&Value, &Value) -> Result<Value, String>,
        &'static str,
    ) = match raw.0 {
        ExecInst::AddI { dst, a, imm }
        | ExecInst::AddIntI { dst, a, imm }
        | ExecInst::AddIGeneric { dst, a, imm } => (dst, a, imm, ops::eval_add, "__add"),
        ExecInst::SubI { dst, a, imm }
        | ExecInst::SubIntI { dst, a, imm }
        | ExecInst::SubIGeneric { dst, a, imm } => (dst, a, imm, ops::eval_sub, "__sub"),
        ExecInst::MulI { dst, a, imm }
        | ExecInst::MulIntI { dst, a, imm }
        | ExecInst::MulIGeneric { dst, a, imm } => (dst, a, imm, ops::eval_mul, "__mul"),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        let rb = Value::Int(imm as i64);
        if matches!(ra, Value::Table(_))
            && let Some(mm) = vm.get_binop_metamethod(ra, &rb, event)
        {
            let va = ra.clone();
            sync_frame_pc(vm, pc);
            if let Err(e) = vm.call_binop_mm(mm, va, rb, base, dst) {
                return fail_with(vm, pc, e);
            }
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            let new_regs = vm.stack.as_mut_ptr().add(base);
            dispatch!(vm, new_regs, pc, consts, dtbl);
        }
        match slow(ra, &rb) {
            Ok(res) => {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), res);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
            Err(e) => fail_with(vm, pc, e),
        }
    }
}

macro_rules! def_arith_handlers {
    (
        $fn_gen:ident, $fn_int:ident, $fn_flt:ident, $fn_deopt:ident,
        $var_gen:ident, $var_int:ident, $var_flt:ident, $var_deopt:ident,
        $checked:ident, $fop:tt, $slow:path, $event:literal, $into:expr
    ) => {
        fn $fn_int(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_int { dst, a, b } = inst else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*regs.add(b as usize))
                    && let Some(r) = x.$checked(*y)
                {
                    write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                }
                (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_deopt { dst, a, b }));
            }
            return cold_arith_binop(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_flt(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_flt { dst, a, b } = inst else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                if let (Value::Float(x), Value::Float(y)) =
                    (&*regs.add(a as usize), &*regs.add(b as usize))
                {
                    write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *x $fop *y);
                }
                (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_deopt { dst, a, b }));
            }
            return cold_arith_binop(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_gen(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_gen { dst, a, b } = inst else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match (&*regs.add(a as usize), &*regs.add(b as usize)) {
                    (Value::Int(x), Value::Int(y)) => {
                        if let Some(r) = x.$checked(*y) {
                            (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int { dst, a, b }));
                            write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                        }
                    }
                    (Value::Float(x), Value::Float(y)) => {
                        (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_flt { dst, a, b }));
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *x $fop *y);
                    }
                    _ => {}
                }
            }
            return cold_arith_binop(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_deopt(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_deopt { dst, a, b } = inst else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match (&*regs.add(a as usize), &*regs.add(b as usize)) {
                    (Value::Int(x), Value::Int(y)) => {
                        if let Some(r) = x.$checked(*y) {
                            write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                        }
                    }
                    (Value::Float(x), Value::Float(y)) => {
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *x $fop *y);
                    }
                    (Value::Int(x), Value::Float(y)) => {
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *x as f64 $fop *y);
                    }
                    (Value::Float(x), Value::Int(y)) => {
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *x $fop *y as f64);
                    }
                    _ => {}
                }
            }
            return cold_arith_binop(vm, regs, pc, consts, dtbl, raw);
        }
    };
}

macro_rules! def_arith_k_handlers {
    (
        $fn_gen:ident, $fn_int:ident, $fn_flt:ident, $fn_deopt:ident,
        $var_gen:ident, $var_int:ident, $var_flt:ident, $var_deopt:ident,
        $checked:ident, $fop:tt, $slow:path, $event:literal
    ) => {
        fn $fn_int(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_int { dst, a, k } = inst else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                if let (Value::Int(x), Value::Int(y)) =
                    (&*regs.add(a as usize), &*consts.add(k as usize))
                    && let Some(r) = x.$checked(*y)
                {
                    write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                }
                (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_deopt { dst, a, k }));
            }
            return cold_arith_k(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_flt(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_flt { dst, a, k } = inst else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                if let (Value::Float(x), Value::Float(y)) =
                    (&*regs.add(a as usize), &*consts.add(k as usize))
                {
                    write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *x $fop *y);
                }
                (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_deopt { dst, a, k }));
            }
            return cold_arith_k(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_gen(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_gen { dst, a, k } = inst else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match (&*regs.add(a as usize), &*consts.add(k as usize)) {
                    (Value::Int(x), Value::Int(y)) => {
                        if let Some(r) = x.$checked(*y) {
                            (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int { dst, a, k }));
                            write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                        }
                    }
                    (Value::Float(x), Value::Float(y)) => {
                        (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_flt { dst, a, k }));
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *x $fop *y);
                    }
                    _ => {}
                }
            }
            return cold_arith_k(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_deopt(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_deopt { dst, a, k } = inst else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match (&*regs.add(a as usize), &*consts.add(k as usize)) {
                    (Value::Int(x), Value::Int(y)) => {
                        if let Some(r) = x.$checked(*y) {
                            write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                        }
                    }
                    (Value::Float(x), Value::Float(y)) => {
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *x $fop *y);
                    }
                    (Value::Int(x), Value::Float(y)) => {
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, (*x as f64) $fop *y);
                    }
                    (Value::Float(x), Value::Int(y)) => {
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *x $fop (*y as f64));
                    }
                    _ => {}
                }
            }
            return cold_arith_k(vm, regs, pc, consts, dtbl, raw);
        }
    };
}

macro_rules! def_arith_imm_handlers {
    (
        $fn_gen:ident, $fn_int:ident, $fn_deopt:ident,
        $var_gen:ident, $var_int:ident, $var_deopt:ident,
        $checked:ident, $fop:tt, $slow:path, $event:literal
    ) => {
        fn $fn_int(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_int { dst, a, imm } = inst else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                if let Value::Int(x) = &*regs.add(a as usize)
                    && let Some(r) = x.$checked(imm as i64)
                {
                    write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                }
                (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_deopt { dst, a, imm }));
            }
            return cold_arith_imm(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_gen(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_gen { dst, a, imm } = inst else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match &*regs.add(a as usize) {
                    Value::Int(x) => {
                        if let Some(r) = x.$checked(imm as i64) {
                            (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int { dst, a, imm }));
                            write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                        }
                    }
                    Value::Float(x) => {
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *x $fop (imm as f64));
                    }
                    _ => {}
                }
            }
            return cold_arith_imm(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_deopt(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_deopt { dst, a, imm } = inst else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match &*regs.add(a as usize) {
                    Value::Int(x) => {
                        if let Some(r) = x.$checked(imm as i64) {
                            write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                        }
                    }
                    Value::Float(x) => {
                        write_float_dispatch!(vm, regs, pc, consts, dtbl, dst, *x $fop (imm as f64));
                    }
                    _ => {}
                }
            }
            return cold_arith_imm(vm, regs, pc, consts, dtbl, raw);
        }
    };
}

def_arith_handlers!(
    op_add, op_add_int, op_add_float, op_add_generic,
    Add, AddInt, AddFloat, AddGeneric,
    checked_add, +, ops::eval_add, "__add", Some(ops::eval_add_into)
);
def_arith_k_handlers!(
    op_add_k, op_add_int_k, op_add_float_k, op_add_k_generic,
    AddK, AddIntK, AddFloatK, AddKGeneric,
    checked_add, +, ops::eval_add, "__add"
);
def_arith_imm_handlers!(
    op_add_i, op_add_int_i, op_add_i_generic,
    AddI, AddIntI, AddIGeneric,
    checked_add, +, ops::eval_add, "__add"
);

def_arith_handlers!(
    op_sub, op_sub_int, op_sub_float, op_sub_generic,
    Sub, SubInt, SubFloat, SubGeneric,
    checked_sub, -, ops::eval_sub, "__sub", Some(ops::eval_sub_into)
);
def_arith_k_handlers!(
    op_sub_k, op_sub_int_k, op_sub_float_k, op_sub_k_generic,
    SubK, SubIntK, SubFloatK, SubKGeneric,
    checked_sub, -, ops::eval_sub, "__sub"
);
def_arith_imm_handlers!(
    op_sub_i, op_sub_int_i, op_sub_i_generic,
    SubI, SubIntI, SubIGeneric,
    checked_sub, -, ops::eval_sub, "__sub"
);

def_arith_handlers!(
    op_mul, op_mul_int, op_mul_float, op_mul_generic,
    Mul, MulInt, MulFloat, MulGeneric,
    checked_mul, *, ops::eval_mul, "__mul", None
);
def_arith_k_handlers!(
    op_mul_k, op_mul_int_k, op_mul_float_k, op_mul_k_generic,
    MulK, MulIntK, MulFloatK, MulKGeneric,
    checked_mul, *, ops::eval_mul, "__mul"
);
def_arith_imm_handlers!(
    op_mul_i, op_mul_int_i, op_mul_i_generic,
    MulI, MulIntI, MulIGeneric,
    checked_mul, *, ops::eval_mul, "__mul"
);

fn op_div(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Div { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let _ = (dst, a, b);
    cold_arith_binop(vm, regs, pc, consts, dtbl, raw)
}

fn op_pow(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Pow { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let _ = (dst, a, b);
    cold_arith_binop(vm, regs, pc, consts, dtbl, raw)
}

fn op_idiv(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::IDiv { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*regs.add(b as usize))
            && *y != 0
            && !(*x == i64::MIN && *y == -1)
        {
            let val = if *x >= 0 && *y > 0 {
                *x / *y
            } else {
                x.div_floor(y)
            };
            write_int(regs.add(dst as usize), val);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    cold_eval_binop_plain(vm, regs, pc, consts, dtbl, raw)
}

fn op_idiv_int_pow2(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::IDivIntPow2 { dst, a, shift } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let Value::Int(x) = &*regs.add(a as usize) {
            write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, *x >> shift);
        }
    }
    cold_eval_idiv_pow2(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_eval_idiv_pow2(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::IDivIntPow2 { dst, a, shift } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        match ops::eval_idiv(ra, &Value::Int(1i64 << shift)) {
            Ok(res) => {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), res);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
            Err(e) => fail_with(vm, pc, e),
        }
    }
}

fn op_idiv_int_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::IDivIntK { dst, a, k } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*consts.add(k as usize)) {
            let val = if *x >= 0 { *x / *y } else { x.div_floor(y) };
            write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, val);
        }
        (&*pc.sub(1)).set(AlignedExec(ExecInst::IDivKGeneric { dst, a, k }));
    }
    cold_eval_k_plain(vm, regs, pc, consts, dtbl, raw)
}

fn op_idiv_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::IDivK { dst, a, k } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*consts.add(k as usize))
            && *y > 0
        {
            if (*y as u64).is_power_of_two() {
                let shift = y.trailing_zeros() as u8;
                (&*pc.sub(1)).set(AlignedExec(ExecInst::IDivIntPow2 { dst, a, shift }));
                write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, *x >> shift);
            } else {
                (&*pc.sub(1)).set(AlignedExec(ExecInst::IDivIntK { dst, a, k }));
                let val = if *x >= 0 { *x / *y } else { x.div_floor(y) };
                write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, val);
            }
        }
    }
    cold_eval_k_plain(vm, regs, pc, consts, dtbl, raw)
}

fn op_idiv_k_generic(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    cold_eval_k_plain(vm, regs, pc, consts, dtbl, raw)
}

fn op_mod(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Mod { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*regs.add(b as usize))
            && *y != 0
        {
            let val = if *y == -1 {
                0
            } else if *x >= 0 && *y > 0 {
                *x % *y
            } else {
                x.mod_floor(y)
            };
            write_int(regs.add(dst as usize), val);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    cold_eval_binop_plain(vm, regs, pc, consts, dtbl, raw)
}

fn op_mod_int_pow2(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ModIntPow2 { dst, a, shift } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let Value::Int(x) = &*regs.add(a as usize) {
            write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, *x & ((1i64 << shift) - 1));
        }
    }
    cold_eval_mod_pow2(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_eval_mod_pow2(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ModIntPow2 { dst, a, shift } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        match ops::eval_mod(ra, &Value::Int(1i64 << shift)) {
            Ok(res) => {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), res);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
            Err(e) => fail_with(vm, pc, e),
        }
    }
}

fn op_mod_int_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ModIntK { dst, a, k } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*consts.add(k as usize)) {
            let val = if *x >= 0 {
                if *x < *y { *x } else { *x % *y }
            } else {
                x.mod_floor(y)
            };
            write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, val);
        }
        (&*pc.sub(1)).set(AlignedExec(ExecInst::ModKGeneric { dst, a, k }));
    }
    cold_eval_k_plain(vm, regs, pc, consts, dtbl, raw)
}

fn op_mod_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ModK { dst, a, k } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*consts.add(k as usize))
            && *y > 0
        {
            if (*y as u64).is_power_of_two() {
                let shift = y.trailing_zeros() as u8;
                (&*pc.sub(1)).set(AlignedExec(ExecInst::ModIntPow2 { dst, a, shift }));
                write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, *x & ((1i64 << shift) - 1));
            } else {
                (&*pc.sub(1)).set(AlignedExec(ExecInst::ModIntK { dst, a, k }));
                let val = if *x >= 0 {
                    if *x < *y { *x } else { *x % *y }
                } else {
                    x.mod_floor(y)
                };
                write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, val);
            }
        }
    }
    cold_eval_k_plain(vm, regs, pc, consts, dtbl, raw)
}

fn op_mod_k_generic(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    cold_eval_k_plain(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_eval_binop_plain(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (dst, a, b, eval): (u8, u8, u8, fn(&Value, &Value) -> Result<Value, String>) = match raw.0 {
        ExecInst::IDiv { dst, a, b } => (dst, a, b, ops::eval_idiv),
        ExecInst::Mod { dst, a, b } => (dst, a, b, ops::eval_mod),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        match eval(&*regs.add(a as usize), &*regs.add(b as usize)) {
            Ok(res) => {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), res);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
            Err(e) => fail_with(vm, pc, e),
        }
    }
}

#[cold]
#[inline(never)]
fn cold_eval_k_plain(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (dst, a, k, eval): (u8, u8, u16, fn(&Value, &Value) -> Result<Value, String>) = match raw.0
    {
        ExecInst::IDivK { dst, a, k }
        | ExecInst::IDivIntK { dst, a, k }
        | ExecInst::IDivKGeneric { dst, a, k } => (dst, a, k, ops::eval_idiv),
        ExecInst::ModK { dst, a, k }
        | ExecInst::ModIntK { dst, a, k }
        | ExecInst::ModKGeneric { dst, a, k } => (dst, a, k, ops::eval_mod),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        match eval(&*regs.add(a as usize), &*consts.add(k as usize)) {
            Ok(res) => {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), res);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
            Err(e) => fail_with(vm, pc, e),
        }
    }
}

fn op_bit_and(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::BitAnd { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*regs.add(b as usize)) {
            write_int(regs.add(dst as usize), x & y);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
        let base = base_of(vm, regs);
        if let Err(e) = ops::eval_bitand_into(
            &mut vm.stack,
            base + dst as usize,
            base + a as usize,
            base + b as usize,
        ) {
            return fail_with(vm, pc, e);
        }
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

fn op_bit_or(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::BitOr { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*regs.add(b as usize)) {
            write_int(regs.add(dst as usize), x | y);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
        let base = base_of(vm, regs);
        if let Err(e) = ops::eval_bitor_into(
            &mut vm.stack,
            base + dst as usize,
            base + a as usize,
            base + b as usize,
        ) {
            return fail_with(vm, pc, e);
        }
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

fn op_bit_xor(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::BitXor { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*regs.add(b as usize)) {
            write_int(regs.add(dst as usize), x ^ y);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
        let base = base_of(vm, regs);
        if let Err(e) = ops::eval_bitxor_into(
            &mut vm.stack,
            base + dst as usize,
            base + a as usize,
            base + b as usize,
        ) {
            return fail_with(vm, pc, e);
        }
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

fn op_shl(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Shl { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*regs.add(b as usize))
            && *x >= 0
            && *y >= 0
        {
            if *x == 0 {
                write_int(regs.add(dst as usize), 0);
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            let headroom = x.leading_zeros().saturating_sub(1) as i64;
            if *y <= headroom {
                write_int(regs.add(dst as usize), x << *y);
                dispatch!(vm, regs, pc, consts, dtbl);
            }
        }
    }
    return cold_shl(vm, regs, pc, consts, dtbl, dst, a, b);
}

#[cold]
#[inline(never)]
fn cold_shl(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
    a: u8,
    b: u8,
) -> u8 {
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        match (ra, rb) {
            (Value::Int(x), Value::Int(y)) if *x >= 0 && *y >= 0 => {
                let headroom128 = (64 + x.leading_zeros() as i64).saturating_sub(1);
                let res = if *y <= headroom128 {
                    Value::from_i128((*x as i128) << *y)
                } else {
                    let x_bits = 64 - x.leading_zeros() as u64;
                    if (*y as u64) <= ops::MAX_INT_BITS
                        && x_bits.saturating_add(*y as u64) <= ops::MAX_INT_BITS
                    {
                        Value::BigInt(Rc::new(num_bigint::BigInt::from(*x) << (*y as usize)))
                    } else {
                        match ops::eval_shl(ra, rb) {
                            Ok(v) => v,
                            Err(e) => return fail_with(vm, pc, e),
                        }
                    }
                };
                set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res);
            }
            (Value::BigInt(_), Value::Int(y)) if *y >= 0 => {
                if let Err(e) = ops::eval_shl_bigint_into(
                    &mut vm.stack,
                    base + dst as usize,
                    base + a as usize,
                    *y as u64,
                ) {
                    return fail_with(vm, pc, e);
                }
            }
            _ => match ops::eval_shl(ra, rb) {
                Ok(res) => set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res),
                Err(e) => return fail_with(vm, pc, e),
            },
        }
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

fn op_shr(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Shr { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*regs.add(b as usize))
            && *y >= 0
        {
            let val = if *y >= 64 {
                if *x < 0 { -1 } else { 0 }
            } else {
                x >> *y
            };
            write_int(regs.add(dst as usize), val);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    return cold_shr(vm, regs, pc, consts, dtbl, dst, a, b);
}

#[cold]
#[inline(never)]
fn cold_shr(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
    a: u8,
    b: u8,
) -> u8 {
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        match (ra, rb) {
            (Value::BigInt(_), Value::Int(y)) if *y >= 0 => {
                ops::eval_shr_bigint_into(
                    &mut vm.stack,
                    base + dst as usize,
                    base + a as usize,
                    *y as u64,
                );
            }
            _ => match ops::eval_shr(ra, rb) {
                Ok(res) => set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res),
                Err(e) => return fail_with(vm, pc, e),
            },
        }
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

fn op_shl_i(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ShlI { dst, a, imm } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let Value::Int(x) = &*regs.add(a as usize)
            && *x >= 0
        {
            if *x == 0 {
                write_int(regs.add(dst as usize), 0);
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            let shift = imm as u32;
            let headroom = x.leading_zeros().saturating_sub(1);
            if shift <= headroom {
                write_int(regs.add(dst as usize), x << shift);
                dispatch!(vm, regs, pc, consts, dtbl);
            }
        }
    }
    return cold_shl_i(vm, regs, pc, consts, dtbl, dst, a, imm);
}

#[cold]
#[inline(never)]
fn cold_shl_i(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
    a: u8,
    imm: u8,
) -> u8 {
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        match ra {
            Value::Int(x) if *x >= 0 => {
                let shift = imm as u32;
                let headroom128 = (64 + x.leading_zeros()).saturating_sub(1);
                let res = if shift <= headroom128 {
                    Value::from_i128((*x as i128) << shift)
                } else {
                    let x_bits = 64 - x.leading_zeros() as u64;
                    if (shift as u64) <= ops::MAX_INT_BITS
                        && x_bits.saturating_add(shift as u64) <= ops::MAX_INT_BITS
                    {
                        Value::BigInt(Rc::new(num_bigint::BigInt::from(*x) << (imm as usize)))
                    } else {
                        let rb = Value::Int(imm as i64);
                        match ops::eval_shl(ra, &rb) {
                            Ok(v) => v,
                            Err(e) => return fail_with(vm, pc, e),
                        }
                    }
                };
                set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res);
            }
            Value::BigInt(_) => {
                if let Err(e) = ops::eval_shl_bigint_into(
                    &mut vm.stack,
                    base + dst as usize,
                    base + a as usize,
                    imm as u64,
                ) {
                    return fail_with(vm, pc, e);
                }
            }
            _ => {
                let rb = Value::Int(imm as i64);
                match ops::eval_shl(ra, &rb) {
                    Ok(res) => set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res),
                    Err(e) => return fail_with(vm, pc, e),
                }
            }
        }
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

fn op_shr_i(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ShrI { dst, a, imm } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let Value::Int(x) = &*regs.add(a as usize) {
            let val = if imm >= 64 {
                if *x < 0 { -1 } else { 0 }
            } else {
                x >> imm
            };
            write_int(regs.add(dst as usize), val);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    return cold_shr_i(vm, regs, pc, consts, dtbl, dst, a, imm);
}

#[cold]
#[inline(never)]
fn cold_shr_i(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
    a: u8,
    imm: u8,
) -> u8 {
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        match ra {
            Value::BigInt(_) => {
                ops::eval_shr_bigint_into(
                    &mut vm.stack,
                    base + dst as usize,
                    base + a as usize,
                    imm as u64,
                );
            }
            _ => {
                let rb = Value::Int(imm as i64);
                match ops::eval_shr(ra, &rb) {
                    Ok(res) => set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res),
                    Err(e) => return fail_with(vm, pc, e),
                }
            }
        }
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

fn op_lshl(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::LShl { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    if let Err(e) = lshift_helper(regs, dst, a, b, true) {
        return fail_with(vm, pc, e);
    }
    unsafe {
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

fn op_lshr(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::LShr { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    if let Err(e) = lshift_helper(regs, dst, a, b, false) {
        return fail_with(vm, pc, e);
    }
    unsafe {
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

#[inline(always)]
fn lshift_helper(regs: *mut Value, dst: u8, a: u8, b: u8, left: bool) -> Result<(), String> {
    unsafe {
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        let res = match (ra, rb) {
            (Value::Int(x), Value::Int(y)) => {
                let word = *x as u64;
                let bits = if *y < 0 { usize::MAX } else { *y as usize };
                let val = if bits >= 64 {
                    0
                } else if left {
                    word << bits
                } else {
                    word >> bits
                };
                Value::from_u64(val)
            }
            _ => {
                if left {
                    ops::eval_lshl(ra, rb)?
                } else {
                    ops::eval_lshr(ra, rb)?
                }
            }
        };
        set_slot_val(regs.add(dst as usize), res);
        Ok(())
    }
}

#[inline(always)]
unsafe fn concat_inner(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    base: usize,
    dst: u8,
    a: u8,
    b: u8,
) -> Result<(), String> {
    unsafe {
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        let res = match (ra, rb) {
            (Value::String(x), Value::String(y)) => {
                let mut s = String::with_capacity(x.len() + y.len());
                s.push_str(x);
                s.push_str(y);
                Value::string(s)
            }
            (Value::Table(_), _) | (_, Value::Table(_)) => {
                if let Some(mm) = vm.get_binop_metamethod(ra, rb, "__concat") {
                    let (va, vb) = (ra.clone(), rb.clone());
                    sync_frame_pc(vm, pc);
                    return vm.call_binop_mm(mm, va, vb, base, dst);
                }
                Value::string(format!("{}{}", ra, rb))
            }
            _ => Value::string(format!("{}{}", ra, rb)),
        };
        set_slot_val(regs.add(dst as usize), res);
        Ok(())
    }
}

fn op_concat(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Concat { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::String(x), Value::String(y)) =
            (&*regs.add(a as usize), &*regs.add(b as usize))
        {
            let mut s = String::with_capacity(x.len() + y.len());
            s.push_str(x);
            s.push_str(y);
            set_slot_val(regs.add(dst as usize), Value::string(s));
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        if let Err(e) = concat_inner(vm, regs, pc, base, dst, a, b) {
            return fail_with(vm, pc, e);
        }
        if vm.trap {
            sync_frame_pc(vm, pc);
            vm.run_result = None;
            return STATUS_OK;
        }
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

fn op_unm(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Unm { dst, src } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        match &*regs.add(src as usize) {
            Value::Int(i) if *i != i64::MIN => {
                write_int(regs.add(dst as usize), -i);
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            Value::Float(f) => {
                write_float(regs.add(dst as usize), -f);
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            _ => {}
        }
    }
    return cold_unm(vm, regs, pc, consts, dtbl, dst, src);
}

#[cold]
#[inline(never)]
fn cold_unm(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
    src: u8,
) -> u8 {
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let v = &*regs.add(src as usize);
        if matches!(v, Value::Table(_)) {
            if let Some(mm) = vm.get_unop_metamethod(v, "__unm") {
                let v = v.clone();
                sync_frame_pc(vm, pc);
                match vm.call_function(mm, &[v]) {
                    Ok(res) => {
                        if vm.trap {
                            sync_frame_pc(vm, pc);
                            vm.run_result = None;
                            return STATUS_OK;
                        }
                        let new_regs = vm.stack.as_mut_ptr().add(base);
                        set_slot_val(
                            new_regs.add(dst as usize),
                            res.into_iter().next().unwrap_or(Value::Nil),
                        );
                        dispatch!(vm, new_regs, pc, consts, dtbl);
                    }
                    Err(e) => return fail_with(vm, pc, e),
                }
            }
            return fail_msg(vm, pc, "unary minus expects a number");
        }
        if let Err(e) = ops::eval_unm_into(&mut vm.stack, base + dst as usize, base + src as usize)
        {
            return fail_with(vm, pc, e);
        }
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

fn op_not(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Not { dst, src } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let truthy = (&*regs.add(src as usize)).is_truthy();
        write_bool(regs.add(dst as usize), !truthy);
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

#[inline(always)]
unsafe fn len_fast(regs: *mut Value, src: u8) -> Option<usize> {
    unsafe {
        match &*regs.add(src as usize) {
            Value::String(s) => Some(s.len()),
            Value::Table(t) => {
                let tbl = t.borrow();
                if tbl.metatable.is_none() {
                    Some(tbl.array.len())
                } else {
                    None
                }
            }
            Value::Buffer(b) => Some(b.borrow().len()),
            _ => None,
        }
    }
}

fn op_len(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Len { dst, src } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let Some(len) = len_fast(regs, src) {
            if let Ok(small) = i64::try_from(len) {
                write_int(regs.add(dst as usize), small);
            } else {
                set_slot_val(regs.add(dst as usize), Value::from_usize(len));
            }
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    return cold_len(vm, regs, pc, consts, dtbl, dst, src);
}

#[cold]
#[inline(never)]
fn cold_len(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
    src: u8,
) -> u8 {
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let v = &*regs.add(src as usize);
        let len = match v {
            Value::String(s) => s.len(),
            Value::Table(t) => {
                if let Some(mm) = vm.get_unop_metamethod(v, "__len") {
                    let v = v.clone();
                    sync_frame_pc(vm, pc);
                    match vm.call_function(mm, &[v]) {
                        Ok(res) => {
                            if vm.trap {
                                sync_frame_pc(vm, pc);
                                vm.run_result = None;
                                return STATUS_OK;
                            }
                            let new_regs = vm.stack.as_mut_ptr().add(base);
                            set_slot_val(
                                new_regs.add(dst as usize),
                                res.into_iter().next().unwrap_or(Value::Nil),
                            );
                            dispatch!(vm, new_regs, pc, consts, dtbl);
                        }
                        Err(e) => return fail_with(vm, pc, e),
                    }
                }
                t.borrow().array.len()
            }
            Value::Buffer(b) => b.borrow().len(),
            _ => return fail_msg(vm, pc, "len expects string, table or buffer"),
        };
        let new_regs = vm.stack.as_mut_ptr().add(base);
        set_slot_val(new_regs.add(dst as usize), Value::from_usize(len));
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

fn op_bit_not(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::BitNot { dst, src } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let Value::Int(x) = &*regs.add(src as usize) {
            write_int(regs.add(dst as usize), !x);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    return cold_bit_not(vm, regs, pc, consts, dtbl, dst, src);
}

#[cold]
#[inline(never)]
fn cold_bit_not(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    dst: u8,
    src: u8,
) -> u8 {
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        match ops::eval_bitnot(&*regs.add(src as usize)) {
            Ok(res) => {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), res);
                dispatch!(vm, new_regs, pc, consts, dtbl);
            }
            Err(e) => fail_with(vm, pc, e),
        }
    }
}

fn op_coalesce(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Coalesce { dst, a, b } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let src_reg = if matches!(&*regs.add(a as usize), Value::Nil) {
            b
        } else {
            a
        };
        copy_slot(regs.add(dst as usize), regs.add(src_reg as usize));
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

fn op_eq(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Eq {
        a,
        b,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    match vm.values_equal(base, a, b, unsafe { ip_of(vm, pc) }) {
        Ok(is_eq) => {
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                let next_pc = if !is_eq {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                let new_regs = vm.stack.as_mut_ptr().add(base);
                dispatch!(vm, new_regs, next_pc, consts, dtbl);
            }
        }
        Err(e) => fail_with(vm, pc, e),
    }
}

fn op_ne(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Ne {
        a,
        b,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    match vm.values_equal(base, a, b, unsafe { ip_of(vm, pc) }) {
        Ok(is_eq) => {
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                let next_pc = if is_eq {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                let new_regs = vm.stack.as_mut_ptr().add(base);
                dispatch!(vm, new_regs, next_pc, consts, dtbl);
            }
        }
        Err(e) => fail_with(vm, pc, e),
    }
}

#[cold]
#[inline(never)]
fn cold_cmp_slow(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (a, b, jump_if_false, slow, event, mm_swap): (
        u8,
        u8,
        i16,
        fn(&Value, &Value) -> Result<bool, String>,
        &'static str,
        bool,
    ) = match raw.0 {
        ExecInst::Lt {
            a,
            b,
            jump_if_false,
        }
        | ExecInst::LtInt {
            a,
            b,
            jump_if_false,
        }
        | ExecInst::LtGeneric {
            a,
            b,
            jump_if_false,
        } => (a, b, jump_if_false, ops::eval_lt, "__lt", false),
        ExecInst::Le {
            a,
            b,
            jump_if_false,
        }
        | ExecInst::LeInt {
            a,
            b,
            jump_if_false,
        }
        | ExecInst::LeGeneric {
            a,
            b,
            jump_if_false,
        } => (a, b, jump_if_false, ops::eval_le, "__le", false),
        ExecInst::Gt {
            a,
            b,
            jump_if_false,
        } => (a, b, jump_if_false, ops::eval_gt, "__lt", true),
        ExecInst::Ge {
            a,
            b,
            jump_if_false,
        } => (a, b, jump_if_false, ops::eval_ge, "__le", true),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    let holds = unsafe {
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if matches!(ra, Value::Table(_)) || matches!(rb, Value::Table(_)) {
            let (l, r) = if mm_swap { (rb, ra) } else { (ra, rb) };
            if let Some(mm) = vm.get_binop_metamethod(l, r, event) {
                let (vl, vr) = (l.clone(), r.clone());
                sync_frame_pc(vm, pc);
                vm.call_cmp_mm(mm, vl, vr)
            } else {
                slow(ra, rb)
            }
        } else {
            slow(ra, rb)
        }
    };
    match holds {
        Ok(h) => {
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                let next_pc = if !h {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                let new_regs = vm.stack.as_mut_ptr().add(base);
                dispatch!(vm, new_regs, next_pc, consts, dtbl);
            }
        }
        Err(e) => fail_with(vm, pc, e),
    }
}

#[cold]
#[inline(never)]
fn cold_cmp_k_slow(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (a, k, jump_if_false, slow, event, mm_swap): (
        u8,
        u16,
        i16,
        fn(&Value, &Value) -> Result<bool, String>,
        &'static str,
        bool,
    ) = match raw.0 {
        ExecInst::LtK {
            a,
            k,
            jump_if_false,
        }
        | ExecInst::LtIntK {
            a,
            k,
            jump_if_false,
        }
        | ExecInst::LtKGeneric {
            a,
            k,
            jump_if_false,
        } => (a, k, jump_if_false, ops::eval_lt, "__lt", false),
        ExecInst::LeK {
            a,
            k,
            jump_if_false,
        }
        | ExecInst::LeIntK {
            a,
            k,
            jump_if_false,
        }
        | ExecInst::LeKGeneric {
            a,
            k,
            jump_if_false,
        } => (a, k, jump_if_false, ops::eval_le, "__le", false),
        ExecInst::GtK {
            a,
            k,
            jump_if_false,
        } => (a, k, jump_if_false, ops::eval_gt, "__lt", true),
        ExecInst::GeK {
            a,
            k,
            jump_if_false,
        } => (a, k, jump_if_false, ops::eval_ge, "__le", true),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    let holds = unsafe {
        let ra = &*regs.add(a as usize);
        let rb = &*consts.add(k as usize);
        if matches!(ra, Value::Table(_)) || matches!(rb, Value::Table(_)) {
            let (l, r) = if mm_swap { (rb, ra) } else { (ra, rb) };
            if let Some(mm) = vm.get_binop_metamethod(l, r, event) {
                let (vl, vr) = (l.clone(), r.clone());
                sync_frame_pc(vm, pc);
                vm.call_cmp_mm(mm, vl, vr)
            } else {
                slow(ra, rb)
            }
        } else {
            slow(ra, rb)
        }
    };
    match holds {
        Ok(h) => {
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                let next_pc = if !h {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                let new_regs = vm.stack.as_mut_ptr().add(base);
                dispatch!(vm, new_regs, next_pc, consts, dtbl);
            }
        }
        Err(e) => fail_with(vm, pc, e),
    }
}

#[cold]
#[inline(never)]
fn cold_cmp_imm_slow(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (a, imm, jump_if_false, slow, event, mm_swap): (
        u8,
        i16,
        i16,
        fn(&Value, &Value) -> Result<bool, String>,
        &'static str,
        bool,
    ) = match raw.0 {
        ExecInst::LtI {
            a,
            imm,
            jump_if_false,
        }
        | ExecInst::LtIntI {
            a,
            imm,
            jump_if_false,
        }
        | ExecInst::LtIGeneric {
            a,
            imm,
            jump_if_false,
        } => (a, imm, jump_if_false, ops::eval_lt, "__lt", false),
        ExecInst::LeI {
            a,
            imm,
            jump_if_false,
        }
        | ExecInst::LeIntI {
            a,
            imm,
            jump_if_false,
        }
        | ExecInst::LeIGeneric {
            a,
            imm,
            jump_if_false,
        } => (a, imm, jump_if_false, ops::eval_le, "__le", false),
        ExecInst::GtI {
            a,
            imm,
            jump_if_false,
        } => (a, imm, jump_if_false, ops::eval_gt, "__lt", true),
        ExecInst::GeI {
            a,
            imm,
            jump_if_false,
        } => (a, imm, jump_if_false, ops::eval_ge, "__le", true),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    let holds = unsafe {
        let ra = &*regs.add(a as usize);
        let rb = Value::Int(imm as i64);
        if matches!(ra, Value::Table(_)) {
            let (l, r) = if mm_swap { (&rb, ra) } else { (ra, &rb) };
            if let Some(mm) = vm.get_binop_metamethod(l, r, event) {
                let (vl, vr) = (l.clone(), r.clone());
                sync_frame_pc(vm, pc);
                vm.call_cmp_mm(mm, vl, vr)
            } else {
                slow(ra, &rb)
            }
        } else {
            slow(ra, &rb)
        }
    };
    match holds {
        Ok(h) => {
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                let next_pc = if !h {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                let new_regs = vm.stack.as_mut_ptr().add(base);
                dispatch!(vm, new_regs, next_pc, consts, dtbl);
            }
        }
        Err(e) => fail_with(vm, pc, e),
    }
}

macro_rules! def_cmp_quick_handlers {
    (
        $fn_gen:ident, $fn_int:ident, $fn_deopt:ident,
        $var_gen:ident, $var_int:ident, $var_deopt:ident,
        $op:tt, $slow:path, $event:literal
    ) => {
        fn $fn_int(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_int {
                a,
                b,
                jump_if_false,
            } = inst
            else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                if let (Value::Int(x), Value::Int(y)) =
                    (&*regs.add(a as usize), &*regs.add(b as usize))
                {
                    let next_pc = if !(*x $op *y) {
                        pc.offset(jump_if_false as isize)
                    } else {
                    pc
                    };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
                }
                (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_deopt {
                    a,
                    b,
                    jump_if_false,
                }));
            }
            return cold_cmp_slow(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_gen(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_gen {
                a,
                b,
                jump_if_false,
            } = inst
            else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match (&*regs.add(a as usize), &*regs.add(b as usize)) {
                    (Value::Int(x), Value::Int(y)) => {
                        (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int {
                            a,
                            b,
                            jump_if_false,
                        }));
                        let next_pc = if !(*x $op *y) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    (Value::Float(x), Value::Float(y)) => {
                        let next_pc = if !(*x $op *y) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    _ => {}
                }
            }
            return cold_cmp_slow(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_deopt(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_deopt {
                a,
                b,
                jump_if_false,
            } = inst
            else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match (&*regs.add(a as usize), &*regs.add(b as usize)) {
                    (Value::Int(x), Value::Int(y)) => {
                        let next_pc = if !(*x $op *y) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    (Value::Float(x), Value::Float(y)) => {
                        let next_pc = if !(*x $op *y) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    _ => {}
                }
            }
            return cold_cmp_slow(vm, regs, pc, consts, dtbl, raw);
        }
    };
}

def_cmp_quick_handlers!(op_lt, op_lt_int, op_lt_generic, Lt, LtInt, LtGeneric, <, ops::eval_lt, "__lt");
def_cmp_quick_handlers!(op_le, op_le_int, op_le_generic, Le, LeInt, LeGeneric, <=, ops::eval_le, "__le");

fn op_gt(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Gt {
        a,
        b,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        match (&*regs.add(a as usize), &*regs.add(b as usize)) {
            (Value::Int(x), Value::Int(y)) => {
                let next_pc = if !(*x > *y) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            (Value::Float(x), Value::Float(y)) => {
                let next_pc = if !(*x > *y) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            _ => {}
        }
    }
    let _ = (a, b, jump_if_false);
    cold_cmp_slow(vm, regs, pc, consts, dtbl, raw)
}

fn op_ge(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Ge {
        a,
        b,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        match (&*regs.add(a as usize), &*regs.add(b as usize)) {
            (Value::Int(x), Value::Int(y)) => {
                let next_pc = if !(*x >= *y) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            (Value::Float(x), Value::Float(y)) => {
                let next_pc = if !(*x >= *y) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            _ => {}
        }
    }
    let _ = (a, b, jump_if_false);
    cold_cmp_slow(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_eq_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (a, k, jump_if_false, invert) = match raw.0 {
        ExecInst::EqK {
            a,
            k,
            jump_if_false,
        }
        | ExecInst::EqIntK {
            a,
            k,
            jump_if_false,
        }
        | ExecInst::EqKGeneric {
            a,
            k,
            jump_if_false,
        } => (a, k, jump_if_false, false),
        ExecInst::NeK {
            a,
            k,
            jump_if_false,
        } => (a, k, jump_if_false, true),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    let is_eq = unsafe {
        let va = (*regs.add(a as usize)).clone();
        let vb = (*consts.add(k as usize)).clone();
        vm.values_equal_val(&va, &vb, ip_of(vm, pc))
    };
    match is_eq {
        Ok(eq) => {
            if vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                let take = if invert { eq } else { !eq };
                let next_pc = if take {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                let new_regs = vm.stack.as_mut_ptr().add(base);
                dispatch!(vm, new_regs, next_pc, consts, dtbl);
            }
        }
        Err(e) => fail_with(vm, pc, e),
    }
}

fn op_eq_int_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::EqIntK {
        a,
        k,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*consts.add(k as usize)) {
            let next_pc = if *x != *y {
                pc.offset(jump_if_false as isize)
            } else {
                pc
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
        }
        (&*pc.sub(1)).set(AlignedExec(ExecInst::EqKGeneric {
            a,
            k,
            jump_if_false,
        }));
    }
    return cold_eq_k(vm, regs, pc, consts, dtbl, raw);
}

fn op_eq_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::EqK {
        a,
        k,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(x), Value::Int(y)) = (&*regs.add(a as usize), &*consts.add(k as usize)) {
            (&*pc.sub(1)).set(AlignedExec(ExecInst::EqIntK {
                a,
                k,
                jump_if_false,
            }));
            let next_pc = if *x != *y {
                pc.offset(jump_if_false as isize)
            } else {
                pc
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
        }
    }
    return cold_eq_k(vm, regs, pc, consts, dtbl, raw);
}

fn op_eq_k_generic(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::EqKGeneric {
        a: _,
        k: _,
        jump_if_false: _,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    return cold_eq_k(vm, regs, pc, consts, dtbl, raw);
}

fn op_ne_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::NeK {
        a,
        k,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let _ = (a, k, jump_if_false);
    return cold_eq_k(vm, regs, pc, consts, dtbl, raw);
}

macro_rules! def_cmp_k_quick_handlers {
    (
        $fn_gen:ident, $fn_int:ident, $fn_deopt:ident,
        $var_gen:ident, $var_int:ident, $var_deopt:ident,
        $op:tt, $slow:path, $event:literal
    ) => {
        fn $fn_int(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_int {
                a,
                k,
                jump_if_false,
            } = inst
            else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                if let (Value::Int(x), Value::Int(y)) =
                    (&*regs.add(a as usize), &*consts.add(k as usize))
                {
                    let next_pc = if !(*x $op *y) {
                        pc.offset(jump_if_false as isize)
                    } else {
                    pc
                    };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
                }
                (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_deopt {
                    a,
                    k,
                    jump_if_false,
                }));
            }
            return cold_cmp_k_slow(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_gen(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_gen {
                a,
                k,
                jump_if_false,
            } = inst
            else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match (&*regs.add(a as usize), &*consts.add(k as usize)) {
                    (Value::Int(x), Value::Int(y)) => {
                        (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int {
                            a,
                            k,
                            jump_if_false,
                        }));
                        let next_pc = if !(*x $op *y) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    (Value::Float(x), Value::Float(y)) => {
                        let next_pc = if !(*x $op *y) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    _ => {}
                }
            }
            return cold_cmp_k_slow(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_deopt(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_deopt {
                a,
                k,
                jump_if_false,
            } = inst
            else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match (&*regs.add(a as usize), &*consts.add(k as usize)) {
                    (Value::Int(x), Value::Int(y)) => {
                        let next_pc = if !(*x $op *y) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    (Value::Float(x), Value::Float(y)) => {
                        let next_pc = if !(*x $op *y) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    _ => {}
                }
            }
            return cold_cmp_k_slow(vm, regs, pc, consts, dtbl, raw);
        }
    };
}

def_cmp_k_quick_handlers!(op_lt_k, op_lt_int_k, op_lt_k_generic, LtK, LtIntK, LtKGeneric, <, ops::eval_lt, "__lt");
def_cmp_k_quick_handlers!(op_le_k, op_le_int_k, op_le_k_generic, LeK, LeIntK, LeKGeneric, <=, ops::eval_le, "__le");

fn op_gt_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GtK {
        a,
        k,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        match (&*regs.add(a as usize), &*consts.add(k as usize)) {
            (Value::Int(x), Value::Int(y)) => {
                let next_pc = if !(*x > *y) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            (Value::Float(x), Value::Float(y)) => {
                let next_pc = if !(*x > *y) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            _ => {}
        }
    }
    let _ = (a, k, jump_if_false);
    cold_cmp_k_slow(vm, regs, pc, consts, dtbl, raw)
}

fn op_ge_k(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GeK {
        a,
        k,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        match (&*regs.add(a as usize), &*consts.add(k as usize)) {
            (Value::Int(x), Value::Int(y)) => {
                let next_pc = if !(*x >= *y) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            (Value::Float(x), Value::Float(y)) => {
                let next_pc = if !(*x >= *y) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            _ => {}
        }
    }
    let _ = (a, k, jump_if_false);
    cold_cmp_k_slow(vm, regs, pc, consts, dtbl, raw)
}

fn op_eq_i(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::EqI {
        a,
        imm,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let is_eq = match &*regs.add(a as usize) {
            Value::Int(x) => *x == imm as i64,
            Value::Float(x) => *x == imm as f64,
            _ => false,
        };
        let next_pc = if !is_eq {
            pc.offset(jump_if_false as isize)
        } else {
            pc
        };
        dispatch!(vm, regs, next_pc, consts, dtbl);
    }
}

fn op_ne_i(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::NeI {
        a,
        imm,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let is_eq = match &*regs.add(a as usize) {
            Value::Int(x) => *x == imm as i64,
            Value::Float(x) => *x == imm as f64,
            _ => false,
        };
        let next_pc = if is_eq {
            pc.offset(jump_if_false as isize)
        } else {
            pc
        };
        dispatch!(vm, regs, next_pc, consts, dtbl);
    }
}

macro_rules! def_cmp_imm_quick_handlers {
    (
        $fn_gen:ident, $fn_int:ident, $fn_deopt:ident,
        $var_gen:ident, $var_int:ident, $var_deopt:ident,
        $op:tt, $slow:path, $event:literal
    ) => {
        fn $fn_int(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_int {
                a,
                imm,
                jump_if_false,
            } = inst
            else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                if let Value::Int(x) = &*regs.add(a as usize) {
                    let next_pc = if !(*x $op imm as i64) {
                        pc.offset(jump_if_false as isize)
                    } else {
                    pc
                    };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
                }
                (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_deopt {
                    a,
                    imm,
                    jump_if_false,
                }));
            }
            return cold_cmp_imm_slow(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_gen(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_gen {
                a,
                imm,
                jump_if_false,
            } = inst
            else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match &*regs.add(a as usize) {
                    Value::Int(x) => {
                        (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int {
                            a,
                            imm,
                            jump_if_false,
                        }));
                        let next_pc = if !(*x $op imm as i64) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    Value::Float(x) => {
                        let next_pc = if !(*x $op imm as f64) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    _ => {}
                }
            }
            return cold_cmp_imm_slow(vm, regs, pc, consts, dtbl, raw);
        }

        fn $fn_deopt(
            vm: &mut VM,
            regs: *mut Value,
            pc: *const Cell<AlignedExec>,
            consts: *const Value,
            dtbl: DispatchTbl,
            raw: AlignedExec,
        ) -> u8 {
            let inst = raw.0;
            let ExecInst::$var_deopt {
                a,
                imm,
                jump_if_false,
            } = inst
            else {
                unsafe { std::hint::unreachable_unchecked() }
            };
            unsafe {
                match &*regs.add(a as usize) {
                    Value::Int(x) => {
                        let next_pc = if !(*x $op imm as i64) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    Value::Float(x) => {
                        let next_pc = if !(*x $op imm as f64) {
                            pc.offset(jump_if_false as isize)
                        } else {
                        pc
                        };
                        dispatch!(vm, regs, next_pc, consts, dtbl);
                    }
                    _ => {}
                }
            }
            return cold_cmp_imm_slow(vm, regs, pc, consts, dtbl, raw);
        }
    };
}

def_cmp_imm_quick_handlers!(op_lt_i, op_lt_int_i, op_lt_i_generic, LtI, LtIntI, LtIGeneric, <, ops::eval_lt, "__lt");
def_cmp_imm_quick_handlers!(op_le_i, op_le_int_i, op_le_i_generic, LeI, LeIntI, LeIGeneric, <=, ops::eval_le, "__le");

fn op_gt_i(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GtI {
        a,
        imm,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        match &*regs.add(a as usize) {
            Value::Int(x) => {
                let next_pc = if !(*x > imm as i64) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            Value::Float(x) => {
                let next_pc = if !(*x > imm as f64) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            _ => {}
        }
    }
    let _ = (a, imm, jump_if_false);
    cold_cmp_imm_slow(vm, regs, pc, consts, dtbl, raw)
}

fn op_ge_i(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::GeI {
        a,
        imm,
        jump_if_false,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        match &*regs.add(a as usize) {
            Value::Int(x) => {
                let next_pc = if !(*x >= imm as i64) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            Value::Float(x) => {
                let next_pc = if !(*x >= imm as f64) {
                    pc.offset(jump_if_false as isize)
                } else {
                    pc
                };
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            _ => {}
        }
    }
    let _ = (a, imm, jump_if_false);
    cold_cmp_imm_slow(vm, regs, pc, consts, dtbl, raw)
}

fn op_test(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Test { reg, jump_if_false } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let next_pc = if !(&*regs.add(reg as usize)).is_truthy() {
            pc.offset(jump_if_false as isize)
        } else {
            pc
        };
        dispatch!(vm, regs, next_pc, consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn cold_jump_interrupt(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Jump { offset } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    if let Err(e) = vm.limits.check_interrupt() {
        return fail_with(vm, pc, e);
    }
    unsafe {
        let next_pc = pc.offset(offset as isize);
        dispatch!(vm, regs, next_pc, consts, dtbl);
    }
}

fn op_jump(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Jump { offset } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    if offset < 0 && vm.limits.is_tripped() {
        return cold_jump_interrupt(vm, regs, pc, consts, dtbl, raw);
    }
    unsafe {
        let next_pc = pc.offset(offset as isize);
        dispatch!(vm, regs, next_pc, consts, dtbl);
    }
}

fn op_for_prep(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ForPrep { base: b, jump } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let init = &*regs.add(b as usize);
        let limit = &*regs.add(b as usize + 1);
        let step = &*regs.add(b as usize + 2);
        if let (Value::Int(i), Value::Int(lim), Value::Int(s)) = (init, limit, step)
            && *s != 0
            && let Some(diff) = i.checked_sub(*s)
        {
            let iters: i128 = if (*s > 0 && *i <= *lim) || (*s < 0 && *i >= *lim) {
                ((*lim as i128 - *i as i128) / (*s as i128)) + 1
            } else {
                0
            };
            let target_pc = pc.offset(jump as isize);
            if (0..=i64::MAX as i128).contains(&iters)
                && let ExecInst::ForLoop { base: fb, jump: fj }
                | ExecInst::ForLoopInt { base: fb, jump: fj }
                | ExecInst::ForLoopCount { base: fb, jump: fj }
                | ExecInst::ForLoopCount1 { base: fb, jump: fj }
                | ExecInst::ForLoopGeneric { base: fb, jump: fj } = (&*target_pc).get().0
                && fb == b
            {
                let specialized = if *s == 1 {
                    ExecInst::ForLoopCount1 { base: fb, jump: fj }
                } else {
                    ExecInst::ForLoopCount { base: fb, jump: fj }
                };
                (&*target_pc).set(AlignedExec(specialized));
                write_int(regs.add(b as usize), diff);
                write_int(regs.add(b as usize + 1), iters as i64);
                dispatch!(vm, regs, target_pc, consts, dtbl);
            }
            if let ExecInst::ForLoopCount { base: fb, jump: fj }
            | ExecInst::ForLoopCount1 { base: fb, jump: fj } = (&*target_pc).get().0
            {
                (&*target_pc).set(AlignedExec(ExecInst::ForLoop { base: fb, jump: fj }));
            }
            write_int(regs.add(b as usize), diff);
            dispatch!(vm, regs, target_pc, consts, dtbl);
        }
    }
    return cold_for_prep(vm, regs, pc, consts, dtbl, b, jump);
}

fn op_for_loop_count(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ForLoopCount { base: b, jump } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(rem), Value::Int(i), Value::Int(st)) = (
            &mut *regs.add(b as usize + 1),
            &mut *regs.add(b as usize),
            &*regs.add(b as usize + 2),
        ) {
            if *rem > 0 {
                *rem -= 1;
                let next = i.wrapping_add(*st);
                *i = next;
                let next_pc = pc.offset(jump as isize);
                write_int_dispatch!(vm, regs, next_pc, consts, dtbl, b + 3, next);
            }
            dispatch!(vm, regs, pc, consts, dtbl);
        }
        (&*pc.sub(1)).set(AlignedExec(ExecInst::ForLoopGeneric { base: b, jump }));
    }
    return cold_for_loop(vm, regs, pc, consts, dtbl, b, jump);
}

fn op_for_loop_count1(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ForLoopCount1 { base: b, jump } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(rem), Value::Int(i)) =
            (&mut *regs.add(b as usize + 1), &mut *regs.add(b as usize))
        {
            if *rem > 0 {
                *rem -= 1;
                let next = i.wrapping_add(1);
                *i = next;
                let next_pc = pc.offset(jump as isize);
                write_int_dispatch!(vm, regs, next_pc, consts, dtbl, b + 3, next);
            }
            dispatch!(vm, regs, pc, consts, dtbl);
        }
        (&*pc.sub(1)).set(AlignedExec(ExecInst::ForLoopGeneric { base: b, jump }));
    }
    cold_for_loop(vm, regs, pc, consts, dtbl, b, jump)
}

#[cold]
#[inline(never)]
fn cold_for_prep(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    b: u8,
    jump: i16,
) -> u8 {
    unsafe {
        let target_pc = pc.offset(jump as isize);
        if let ExecInst::ForLoopCount { base: fb, jump: fj }
        | ExecInst::ForLoopCount1 { base: fb, jump: fj } = (&*target_pc).get().0
        {
            (&*target_pc).set(AlignedExec(ExecInst::ForLoop { base: fb, jump: fj }));
        }
        let init = &*regs.add(b as usize);
        let step = &*regs.add(b as usize + 2);
        match step {
            Value::Int(0) => return fail_msg(vm, pc, "numeric for step cannot be zero"),
            Value::Float(f) if *f == 0.0 => {
                return fail_msg(vm, pc, "numeric for step cannot be zero");
            }
            _ => {}
        }
        match ops::eval_sub(init, step) {
            Ok(init_minus_step) => {
                set_slot_val(regs.add(b as usize), init_minus_step);
                dispatch!(vm, regs, target_pc, consts, dtbl);
            }
            Err(e) => fail_with(vm, pc, e),
        }
    }
}

fn op_for_loop_int(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ForLoopInt { base: b, jump } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(i), Value::Int(lim), Value::Int(st)) = (
            &*regs.add(b as usize),
            &*regs.add(b as usize + 1),
            &*regs.add(b as usize + 2),
        ) && let Some(next) = i.checked_add(*st)
        {
            let again = if *st >= 0 { next <= *lim } else { next >= *lim };
            write_int(regs.add(b as usize), next);
            if again {
                write_int(regs.add(b as usize + 3), next);
                let next_pc = pc.offset(jump as isize);
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            dispatch!(vm, regs, pc, consts, dtbl);
        }
        (&*pc.sub(1)).set(AlignedExec(ExecInst::ForLoopGeneric { base: b, jump }));
    }
    return cold_for_loop(vm, regs, pc, consts, dtbl, b, jump);
}

fn op_for_loop(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ForLoop { base: b, jump } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let (Value::Int(i), Value::Int(lim), Value::Int(st)) = (
            &*regs.add(b as usize),
            &*regs.add(b as usize + 1),
            &*regs.add(b as usize + 2),
        ) && let Some(next) = i.checked_add(*st)
        {
            (&*pc.sub(1)).set(AlignedExec(ExecInst::ForLoopInt { base: b, jump }));
            let again = if *st >= 0 { next <= *lim } else { next >= *lim };
            write_int(regs.add(b as usize), next);
            if again {
                write_int(regs.add(b as usize + 3), next);
                let next_pc = pc.offset(jump as isize);
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    return cold_for_loop(vm, regs, pc, consts, dtbl, b, jump);
}

fn op_for_loop_generic(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::ForLoopGeneric { base: b, jump } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    return cold_for_loop(vm, regs, pc, consts, dtbl, b, jump);
}

#[cold]
#[inline(never)]
fn cold_for_loop(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    b: u8,
    jump: i16,
) -> u8 {
    unsafe {
        let idx = &*regs.add(b as usize);
        let limit = &*regs.add(b as usize + 1);
        let step = &*regs.add(b as usize + 2);
        if let (Value::Int(i), Value::Int(lim), Value::Int(st)) = (idx, limit, step)
            && let Some(next) = i.checked_add(*st)
        {
            let again = if *st >= 0 { next <= *lim } else { next >= *lim };
            write_int(regs.add(b as usize), next);
            if again {
                write_int(regs.add(b as usize + 3), next);
                let next_pc = pc.offset(jump as isize);
                dispatch!(vm, regs, next_pc, consts, dtbl);
            }
            dispatch!(vm, regs, pc, consts, dtbl);
        }
        match step {
            Value::Int(0) => return fail_msg(vm, pc, "numeric for step cannot be zero"),
            Value::Float(f) if *f == 0.0 => {
                return fail_msg(vm, pc, "numeric for step cannot be zero");
            }
            _ => {}
        }
        let is_positive = match step {
            Value::Int(i) => *i >= 0,
            Value::BigInt(i) => num_traits::Signed::is_positive(&**i),
            Value::Float(f) => *f >= 0.0,
            _ => true,
        };
        let next_idx = match ops::eval_add(idx, step) {
            Ok(v) => v,
            Err(e) => return fail_with(vm, pc, e),
        };
        let loop_again = if is_positive {
            ops::eval_le(&next_idx, limit)
        } else {
            ops::eval_ge(&next_idx, limit)
        };
        let loop_again = match loop_again {
            Ok(b) => b,
            Err(e) => return fail_with(vm, pc, e),
        };
        set_slot_val(regs.add(b as usize), next_idx.clone());
        let next_pc = if loop_again {
            set_slot_val(regs.add(b as usize + 3), next_idx);
            pc.offset(jump as isize)
        } else {
            pc
        };
        dispatch!(vm, regs, next_pc, consts, dtbl);
    }
}

#[inline(always)]
fn closure_inner(vm: &mut VM, base: usize, dst: u8, proto_idx: u16) {
    let (child_proto, new_uv) = {
        let cur = &vm.frames.last().expect(RUNNING_FRAME).closure;
        let child = cur.proto.protos[proto_idx as usize].clone();
        let mut ups = Vec::with_capacity(child.upvalues.len());
        for updesc in &child.upvalues {
            ups.push((updesc.in_stack, updesc.index));
        }
        (child, ups)
    };
    let mut upvalues = Vec::with_capacity(new_uv.len());
    for (in_stack, idx) in new_uv {
        if in_stack {
            upvalues.push(vm.capture_upvalue(base + idx as usize));
        } else {
            let cur = &vm.frames.last().expect(RUNNING_FRAME).closure;
            let up = cur
                .upvalues
                .get(idx as usize)
                .cloned()
                .unwrap_or_else(|| Upvalue::closed(Value::Nil));
            upvalues.push(up);
        }
    }
    let val = Value::Closure(Rc::new(VmClosure {
        proto: child_proto,
        upvalues,
    }));
    unsafe {
        set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), val);
    }
}

fn op_closure(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Closure { dst, proto_idx } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    closure_inner(vm, base, dst, proto_idx);
    let new_regs = unsafe { vm.stack.as_mut_ptr().add(base) };
    unsafe {
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn call_closure_inner(
    vm: &mut VM,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    _dtbl: DispatchTbl,
    base: usize,
    callee: u8,
    argc: usize,
    retc: u8,
) -> Result<(*mut Value, *const Cell<AlignedExec>, *const Value), String> {
    if vm.call_depth() >= vm.max_call_depth {
        return Err(format!(
            "call stack overflow: exceeded maximum call depth of {}",
            vm.max_call_depth
        ));
    }
    let args_start = base + callee as usize + 1;
    let same_closure = {
        let Value::Closure(c) = (unsafe { vm.stack.get_unchecked(base + callee as usize) }) else {
            unreachable!();
        };
        let is_same = Rc::as_ptr(c) == vm.cur_closure;
        if !is_same && !c.proto.is_verified() {
            crate::bytecode::verify_proto(&c.proto).map_err(|e| e.to_string())?;
            c.proto.mark_verified();
        }
        is_same
    };
    let Value::Closure(callee_closure) =
        std::mem::replace(&mut vm.stack[base + callee as usize], Value::Nil)
    else {
        unreachable!();
    };
    let proto = &callee_closure.proto;
    let max_registers = proto.max_registers as usize;
    let num_params = proto.num_params as usize;
    let is_vararg = proto.is_vararg;
    let new_base = args_start;
    let needed = new_base + max_registers;
    if needed >= vm.stack.len() {
        vm.stack.resize(needed + 1, Value::Nil);
    }
    for i in argc..num_params {
        vm.stack[new_base + i] = Value::Nil;
    }
    let new_frame = if is_vararg && argc > num_params {
        let varargs = vm.stack[new_base + num_params..new_base + argc].to_vec();
        CallFrame::with_varargs(callee_closure, new_base, varargs)
    } else {
        CallFrame::new(callee_closure, new_base)
    }
    .wanting(retc);
    vm.frames.push(new_frame);
    if vm.debug_active {
        if vm.hook.is_some()
            && let Err(err) = vm.fire_hook(HookEvent::Call)
        {
            vm.frames.pop();
            return Err(err);
        }
        if vm.dap.is_some() {
            let func_name = vm
                .frames
                .last()
                .expect(RUNNING_FRAME)
                .closure
                .proto
                .name
                .as_deref()
                .unwrap_or("<anonymous>")
                .to_string();
            if let Err(err) = crate::dap::check_break(vm, &func_name) {
                vm.frames.pop();
                return Err(err);
            }
        }
    }
    let (new_pc, new_consts) = if same_closure {
        let _ = pc;
        unsafe { ((*vm.cur_exec).ops.as_ptr(), consts) }
    } else {
        let cf = unsafe { vm.frames.last().unwrap_unchecked() };
        let exec = cf.closure.proto.exec();
        let ex_ptr = exec as *const ExecProgram;
        let nc = cf.closure.proto.values().as_ptr();
        vm.cur_exec = ex_ptr;
        vm.cur_consts = nc;
        vm.cur_closure = Rc::as_ptr(&cf.closure);
        vm.cur_upvals = cf.closure.upvalues.as_ptr();
        (exec.ops.as_ptr(), nc)
    };
    let new_regs = unsafe { vm.stack.as_mut_ptr().add(new_base) };
    Ok((new_regs, new_pc, new_consts))
}

#[cold]
#[inline(never)]
fn cold_call_slow(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Call { callee, argc, retc } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    let args_start = base + callee as usize + 1;
    let args_end = if argc == MULTRET {
        vm.top.max(args_start)
    } else {
        args_start + argc as usize
    };
    let actual_argc = args_end - args_start;
    if args_end > vm.stack.len() {
        vm.stack.resize(args_end, Value::Nil);
    }
    sync_frame_pc(vm, pc);

    unsafe {
        if matches!(
            &*vm.stack.as_ptr().add(base + callee as usize),
            Value::Closure(_)
        ) {
            match call_closure_inner(vm, pc, consts, dtbl, base, callee, actual_argc, retc) {
                Ok((new_regs, new_pc, new_consts)) => {
                    dispatch!(vm, new_regs, new_pc, new_consts, dtbl);
                }
                Err(e) => return fail_with(vm, pc, e),
            }
        }
    }
    let ok = call_non_closure_ok(vm, base, callee, args_start, args_end, actual_argc, retc);
    finish_non_closure_dispatch(vm, pc, consts, dtbl, base, ok)
}

fn op_call(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Call { callee, argc, retc } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let base = base_of(vm, regs);
        let len = vm.frames.len();
        if argc != MULTRET
            && !vm.debug_active
            && vm.frames_below == 0
            && len < vm.max_call_depth
            && len < vm.frames.capacity()
        {
            let args_start = base + callee as usize + 1;
            let callee_ptr = regs.add(callee as usize);
            if let Value::Closure(c) = &*callee_ptr {
                let c_ptr = Rc::as_ptr(c);
                let same_closure = c_ptr == vm.cur_closure;
                let proto = &c.proto;
                let max_regs = proto.max_registers as usize;
                let num_params = proto.num_params as usize;
                if (same_closure || proto.is_verified())
                    && !proto.is_vararg
                    && (argc as usize) >= num_params
                    && args_start + max_regs < vm.stack.len()
                    && args_start + (argc as usize) <= vm.stack.len()
                    && (same_closure
                        || (proto.exec_ready().is_some() && proto.values_ready().is_some()))
                {
                    let ip = ip_of(vm, pc);
                    let frames_ptr = vm.frames.as_mut_ptr();
                    (*frames_ptr.add(len - 1)).ip = ip;
                    let callee_closure = match std::ptr::read(callee_ptr) {
                        Value::Closure(cl) => cl,
                        other => {
                            std::mem::forget(other);
                            std::hint::unreachable_unchecked()
                        }
                    };
                    std::ptr::write(callee_ptr, Value::Nil);
                    std::ptr::write(
                        frames_ptr.add(len),
                        CallFrame::new(callee_closure, args_start).wanting(retc),
                    );
                    vm.frames.set_len(len + 1);
                    let (new_pc, new_consts) = if same_closure {
                        ((*vm.cur_exec).ops.as_ptr(), consts)
                    } else {
                        let cf = &*frames_ptr.add(len);
                        let exec = cf.closure.proto.exec_ready().unwrap_unchecked();
                        let nc = cf.closure.proto.values_ready().unwrap_unchecked().as_ptr();
                        vm.cur_exec = exec as *const ExecProgram;
                        vm.cur_consts = nc;
                        vm.cur_closure = Rc::as_ptr(&cf.closure);
                        vm.cur_upvals = cf.closure.upvalues.as_ptr();
                        (exec.ops.as_ptr(), nc)
                    };
                    let new_regs = vm.stack.as_mut_ptr().add(args_start);
                    dispatch!(vm, new_regs, new_pc, new_consts, dtbl);
                }
            }
        }
    }
    cold_call_slow(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn call_non_closure_ok(
    vm: &mut VM,
    base: usize,
    callee: u8,
    args_start: usize,
    args_end: usize,
    argc: usize,
    retc: u8,
) -> bool {
    match call_non_closure_inner(vm, base, callee, args_start, args_end, argc, retc) {
        Ok(()) => true,
        Err(e) => {
            vm.run_err = Some(e);
            false
        }
    }
}

#[inline(always)]
fn call_non_closure_inner(
    vm: &mut VM,
    base: usize,
    callee: u8,
    args_start: usize,
    args_end: usize,
    argc: usize,
    retc: u8,
) -> Result<(), String> {
    match &vm.stack[base + callee as usize] {
        Value::Native(def) => {
            let func = def.func;
            if argc == 1 && is_trivial_drop(&vm.stack[args_start]) {
                let single = [unsafe { std::ptr::read(&vm.stack[args_start]) }];
                if vm.debug_active && vm.hook.is_some() {
                    vm.fire_hook(HookEvent::Call)?;
                }
                return match func(vm, &single) {
                    Ok(results) => {
                        if vm.debug_active && vm.hook.is_some() {
                            vm.fire_hook(HookEvent::Return)?;
                        }
                        vm.store_call_results(callee, retc, results);
                        Ok(())
                    }
                    Err(err) if vm.is_yielding => {
                        if let Some(co_id) = vm.current_co
                            && let Some(co_rc) = vm.co_state(co_id)
                        {
                            let mut cs = co_rc.borrow_mut();
                            cs.yield_callee = callee;
                            cs.yield_retc = retc;
                        }
                        Err(err)
                    }
                    Err(err) => Err(err),
                };
            }
            const INLINE_NATIVE_ARGS: usize = 8;
            let mut inline_args = [const { Value::Nil }; INLINE_NATIVE_ARGS];
            let heap_args: Vec<Value>;
            let args: &[Value] = if argc <= INLINE_NATIVE_ARGS {
                for (dst, src) in inline_args[..argc]
                    .iter_mut()
                    .zip(&vm.stack[args_start..args_end])
                {
                    *dst = src.clone();
                }
                &inline_args[..argc]
            } else {
                heap_args = vm.stack[args_start..args_end].to_vec();
                &heap_args
            };
            if vm.debug_active && vm.hook.is_some() {
                vm.fire_hook(HookEvent::Call)?;
            }
            match func(vm, args) {
                Ok(results) => {
                    if vm.debug_active && vm.hook.is_some() {
                        vm.fire_hook(HookEvent::Return)?;
                    }
                    vm.store_call_results(callee, retc, results);
                    Ok(())
                }
                Err(err) if vm.is_yielding => {
                    if let Some(co_id) = vm.current_co
                        && let Some(co_rc) = vm.co_state(co_id)
                    {
                        let mut cs = co_rc.borrow_mut();
                        cs.yield_callee = callee;
                        cs.yield_retc = retc;
                    }
                    Err(err)
                }
                Err(err) => Err(err),
            }
        }
        Value::Table(t) => {
            let handler = t.borrow().metamethod("__call");
            let callee_val = vm.stack[base + callee as usize].clone();
            if let Some(handler) = handler {
                let mut call_args = Vec::with_capacity(argc + 1);
                call_args.push(callee_val);
                call_args.extend_from_slice(&vm.stack[args_start..args_end]);
                let results = vm.call_function(handler, &call_args)?;
                vm.store_call_results(callee, retc, results);
                Ok(())
            } else {
                Err(format!("attempted to call non-function ({:?})", callee_val))
            }
        }
        other => Err(format!("attempted to call non-function ({:?})", other)),
    }
}

fn finish_non_closure_dispatch(
    vm: &mut VM,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    base: usize,
    ok: bool,
) -> u8 {
    if ok {
        if vm.trap {
            sync_frame_pc(vm, pc);
            vm.run_result = None;
            return STATUS_OK;
        }
        unsafe {
            let new_regs = vm.stack.as_mut_ptr().add(base);
            dispatch!(vm, new_regs, pc, consts, dtbl);
        }
    } else {
        if !vm.is_yielding {
            sync_frame_pc(vm, pc);
        }
        STATUS_ERR
    }
}

#[inline(always)]
unsafe fn try_self_tail_call(
    vm: &mut VM,
    regs: *mut Value,
    base: usize,
    callee: u8,
    argc: u8,
) -> Result<Option<*mut Value>, String> {
    if base == 0 || argc == MULTRET || vm.debug_active {
        return Ok(None);
    }
    unsafe {
        let Value::Closure(callee_closure) = &*regs.add(callee as usize) else {
            return Ok(None);
        };
        if Rc::as_ptr(callee_closure) != vm.cur_closure || callee_closure.proto.is_vararg {
            return Ok(None);
        }
        let max_registers = callee_closure.proto.max_registers as usize;
        let num_params = callee_closure.proto.num_params as usize;
        if vm.limits.is_tripped() || vm.open_upvalues.last().is_some_and(|(idx, _)| *idx >= base) {
            return Ok(None);
        }
        let needed = base + max_registers;
        if needed >= vm.stack.len() || (argc as usize) < num_params {
            return Ok(None);
        }
        let copy_n = num_params;
        let args_offset = callee as usize + 1;
        for k in 0..copy_n {
            let src_ptr = regs.add(args_offset + k);
            let dst_ptr = regs.add(k);
            match (&*src_ptr, &mut *dst_ptr) {
                (Value::Int(n), Value::Int(o)) => *o = *n,
                (Value::Float(f), Value::Float(o)) => *o = *f,
                (Value::Bool(b), Value::Bool(o)) => *o = *b,
                (Value::Int(n), d) if is_trivial_drop(d) => {
                    std::ptr::write(dst_ptr, Value::Int(*n))
                }
                (Value::Float(f), d) if is_trivial_drop(d) => {
                    std::ptr::write(dst_ptr, Value::Float(*f))
                }
                (Value::Bool(b), d) if is_trivial_drop(d) => {
                    std::ptr::write(dst_ptr, Value::Bool(*b))
                }
                _ => return Ok(None),
            }
        }
        Ok(Some(regs))
    }
}

fn op_tail_call(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::TailCall { callee, argc } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        match try_self_tail_call(vm, regs, base, callee, argc) {
            Ok(Some(new_regs)) => {
                let start_pc = (*vm.cur_exec).ops.as_ptr();
                dispatch!(vm, new_regs, start_pc, consts, dtbl);
            }
            Ok(None) => {}
            Err(e) => return fail_with(vm, pc, e),
        }
    }
    return cold_tail_call_general(vm, pc, consts, dtbl, base, callee, argc);
}

#[cold]
#[inline(never)]
fn cold_tail_call_general(
    vm: &mut VM,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    base: usize,
    callee: u8,
    argc: u8,
) -> u8 {
    let args_start = base + callee as usize + 1;
    let args_end = if argc == MULTRET {
        vm.top.max(args_start)
    } else {
        args_start + argc as usize
    };
    let actual_argc = args_end - args_start;
    if args_end > vm.stack.len() {
        vm.stack.resize(args_end, Value::Nil);
    }
    sync_frame_pc(vm, pc);
    if base >= 1 && matches!(&vm.stack[base + callee as usize], Value::Closure(_)) {
        if let Err(e) = vm.limits.check_interrupt() {
            return fail_with(vm, pc, e);
        }
        let callee_closure = {
            let Value::Closure(c) = &vm.stack[base + callee as usize] else {
                unreachable!()
            };
            Rc::clone(c)
        };
        if !callee_closure.proto.is_verified()
            && let Err(e) = crate::bytecode::verify_proto(&callee_closure.proto)
        {
            return fail_with(vm, pc, e.to_string());
        }
        callee_closure.proto.mark_verified();
        let max_registers = callee_closure.proto.max_registers as usize;
        let num_params = callee_closure.proto.num_params as usize;
        let is_vararg = callee_closure.proto.is_vararg;
        vm.close_upvalues_if_needed(base);
        let caller_dest = base - 1;
        let callee_abs = base + callee as usize;
        let shift = callee_abs - caller_dest;
        let len = args_end - callee_abs;
        for k in 0..len {
            let v = std::mem::replace(&mut vm.stack[callee_abs + k], Value::Nil);
            vm.stack[caller_dest + k] = v;
        }
        if vm.top > callee_abs {
            vm.top -= shift;
        }
        let needed = base + max_registers;
        if needed >= vm.stack.len() {
            vm.stack.resize(needed + 1, Value::Nil);
        }
        for i in actual_argc..num_params {
            vm.stack[base + i] = Value::Nil;
        }
        if is_vararg && actual_argc > num_params {
            let varargs = vm.stack[base + num_params..base + actual_argc].to_vec();
            let want_ret = vm.frames.last().expect(RUNNING_FRAME).want_ret;
            *vm.frames.last_mut().expect(RUNNING_FRAME) =
                CallFrame::with_varargs(callee_closure, base, varargs).wanting(want_ret);
        } else {
            let frame = vm.frames.last_mut().expect(RUNNING_FRAME);
            frame.closure = callee_closure;
            frame.ip = 0;
            frame.clear_varargs();
        }
        if vm.debug_active {
            if vm.hook.is_some()
                && let Err(e) = vm.fire_hook(HookEvent::TailCall)
            {
                return fail_with(vm, pc, e);
            }
            if vm.dap.is_some() {
                let func_name = vm
                    .frames
                    .last()
                    .expect(RUNNING_FRAME)
                    .closure
                    .proto
                    .name
                    .as_deref()
                    .unwrap_or("<anonymous>")
                    .to_string();
                if let Err(e) = crate::dap::check_break(vm, &func_name) {
                    return fail_with(vm, pc, e);
                }
            }
        }
        let (new_code, new_consts) = {
            let cf = vm.frames.last().expect(RUNNING_FRAME);
            let exec = cf.closure.proto.exec();
            let nc = cf.closure.proto.values().as_ptr();
            vm.cur_exec = exec as *const ExecProgram;
            vm.cur_consts = nc;
            vm.cur_closure = Rc::as_ptr(&cf.closure);
            vm.cur_upvals = cf.closure.upvalues.as_ptr();
            (exec.ops.as_ptr(), nc)
        };
        unsafe {
            let new_regs = vm.stack.as_mut_ptr().add(base);
            dispatch!(vm, new_regs, new_code, new_consts, dtbl);
        }
    }

    if matches!(&vm.stack[base + callee as usize], Value::Closure(_)) {
        match call_closure_inner(vm, pc, consts, dtbl, base, callee, actual_argc, MULTRET) {
            Ok((new_regs, new_pc, new_consts)) => unsafe {
                dispatch!(vm, new_regs, new_pc, new_consts, dtbl);
            },
            Err(e) => return fail_with(vm, pc, e),
        }
    }
    let ok = call_non_closure_ok(vm, base, callee, args_start, args_end, actual_argc, MULTRET);
    finish_non_closure_dispatch(vm, pc, consts, dtbl, base, ok)
}

#[inline(always)]
unsafe fn try_fast_return(
    vm: &mut VM,
    regs: *mut Value,
    consts: *const Value,
    ret: u8,
    count: u8,
) -> Option<(*mut Value, *const Cell<AlignedExec>, *const Value)> {
    let len = vm.frames.len();
    if count != 1 || vm.debug_active || len <= vm.run_target_depth + 1 {
        return None;
    }
    unsafe {
        let frames_ptr = vm.frames.as_mut_ptr();
        let top_ptr = frames_ptr.add(len - 1);
        if (*top_ptr).want_ret > 1 || (*top_ptr).has_varargs() {
            return None;
        }
        let caller_frame_chk = &*frames_ptr.add(len - 2);
        let same_caller = Rc::as_ptr(&caller_frame_chk.closure) == vm.cur_closure;
        if !same_caller
            && (caller_frame_chk.closure.proto.exec_ready().is_none()
                || caller_frame_chk.closure.proto.values_ready().is_none())
        {
            return None;
        }
        let frame_base = (*top_ptr).base;
        if vm
            .open_upvalues
            .last()
            .is_some_and(|(idx, _)| *idx >= frame_base)
        {
            return None;
        }
        let src_ptr = regs.add(ret as usize);
        let dst_ptr = regs.sub(1);
        if !is_trivial_drop(&*dst_ptr) {
            return None;
        }
        match &*src_ptr {
            Value::Int(n) => std::ptr::write(dst_ptr, Value::Int(*n)),
            Value::Float(f) => std::ptr::write(dst_ptr, Value::Float(*f)),
            Value::Bool(b) => std::ptr::write(dst_ptr, Value::Bool(*b)),
            _ => return None,
        }
        if Rc::strong_count(&(*top_ptr).closure) <= 1 {
            return None;
        }
        let frame_cl = std::ptr::read(&(*top_ptr).closure);
        vm.frames.set_len(len - 1);
        drop(frame_cl);
        let caller_frame = &*frames_ptr.add(len - 2);
        let caller_closure_ptr = Rc::as_ptr(&caller_frame.closure);
        let caller_base = caller_frame.base;
        let caller_ip = caller_frame.ip;
        let (new_pc, new_consts) = if caller_closure_ptr != vm.cur_closure {
            let exec = caller_frame.closure.proto.exec_ready().unwrap_unchecked();
            let nc = caller_frame
                .closure
                .proto
                .values_ready()
                .unwrap_unchecked()
                .as_ptr();
            vm.cur_exec = exec as *const ExecProgram;
            vm.cur_consts = nc;
            vm.cur_closure = caller_closure_ptr;
            vm.cur_upvals = caller_frame.closure.upvalues.as_ptr();
            (exec.ops.as_ptr().add(caller_ip), nc)
        } else {
            ((*vm.cur_exec).ops.as_ptr().add(caller_ip), consts)
        };
        let new_regs = vm.stack.as_mut_ptr().add(caller_base);
        Some((new_regs, new_pc, new_consts))
    }
}

fn op_return(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Return { base: ret, count } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        if let Some((new_regs, new_pc, new_consts)) = try_fast_return(vm, regs, consts, ret, count)
        {
            dispatch!(vm, new_regs, new_pc, new_consts, dtbl);
        }
    }
    return cold_return_general(vm, pc, dtbl, ret, count);
}

#[cold]
#[inline(never)]
fn cold_return_general(
    vm: &mut VM,
    pc: *const Cell<AlignedExec>,
    dtbl: DispatchTbl,
    ret: u8,
    count: u8,
) -> u8 {
    if vm.debug_active && vm.hook.is_some() {
        sync_frame_pc(vm, pc);
        if let Err(e) = vm.fire_hook(HookEvent::Return) {
            return fail_with(vm, pc, e);
        }
    }
    let frame = vm.frames.pop().expect(RUNNING_FRAME);
    let frame_base = frame.base;
    let want_ret = frame.want_ret;
    drop(frame);
    vm.close_upvalues_if_needed(frame_base);

    let ret_start = frame_base + ret as usize;
    let ret_end = if count == MULTRET {
        vm.top.max(ret_start)
    } else {
        ret_start + count as usize
    };
    let ret_end = ret_end.min(vm.stack.len());
    let nret = ret_end.saturating_sub(ret_start);

    if vm.frames.len() == vm.run_target_depth {
        vm.returned.clear();
        vm.returned
            .extend(vm.stack[ret_start..ret_end].iter().cloned());
        vm.run_result = Some(vm.returned.first().cloned().unwrap_or(Value::Nil));
        return STATUS_OK;
    }

    let caller_dest = frame_base - 1;
    let wanted = match want_ret {
        MULTRET => nret,
        0 => 1,
        n => n as usize,
    };
    let needed = caller_dest + wanted;
    if needed > vm.stack.len() {
        vm.stack.resize(needed, Value::Nil);
    }
    unsafe {
        let s = vm.stack.as_mut_ptr();
        if wanted == 1 && nret == 1 {
            let src_ptr = s.add(ret_start);
            let dst_ptr = s.add(caller_dest);
            match &*src_ptr {
                Value::Int(n) => write_int(dst_ptr, *n),
                Value::Float(f) => write_float(dst_ptr, *f),
                _ => {
                    let v = std::mem::replace(&mut *src_ptr, Value::Nil);
                    *dst_ptr = v;
                }
            }
        } else {
            for i in 0..wanted {
                if i < nret {
                    let src_ptr = s.add(ret_start + i);
                    let dst_ptr = s.add(caller_dest + i);
                    match &*src_ptr {
                        Value::Int(n) => write_int(dst_ptr, *n),
                        Value::Float(f) => write_float(dst_ptr, *f),
                        _ => {
                            let v = std::mem::replace(&mut *src_ptr, Value::Nil);
                            *dst_ptr = v;
                        }
                    }
                } else {
                    *s.add(caller_dest + i) = Value::Nil;
                }
            }
        }
    }
    if want_ret == MULTRET {
        vm.top = caller_dest + nret;
    }
    let (new_code, new_ip, new_base, new_consts) = {
        let cf = vm.frames.last().expect(RUNNING_FRAME);
        let exec = cf.closure.proto.exec();
        let nc = cf.closure.proto.values().as_ptr();
        let ex_ptr = exec as *const ExecProgram;
        if ex_ptr != vm.cur_exec || Rc::as_ptr(&cf.closure) != vm.cur_closure {
            vm.cur_exec = ex_ptr;
            vm.cur_consts = nc;
            vm.cur_closure = Rc::as_ptr(&cf.closure);
            vm.cur_upvals = cf.closure.upvalues.as_ptr();
        }
        (exec.ops.as_ptr(), cf.ip, cf.base, nc)
    };
    if vm.trap {
        vm.run_result = None;
        return STATUS_OK;
    }
    unsafe {
        let new_regs = vm.stack.as_mut_ptr().add(new_base);
        let new_pc = new_code.add(new_ip);
        dispatch!(vm, new_regs, new_pc, new_consts, dtbl);
    }
}

#[inline(always)]
fn vararg_inner(vm: &mut VM, base: usize, dst: u8, count: u8) {
    let all = count == 0 || count == MULTRET;
    let cnt = {
        let frame = vm.frames.last().expect(RUNNING_FRAME);
        if all {
            frame.varargs().len()
        } else {
            count as usize
        }
    };
    let dest = base + dst as usize;
    if dest + cnt > vm.stack.len() {
        vm.stack.resize(dest + cnt, Value::Nil);
    }
    let varargs: Vec<Value> = vm.frames.last().expect(RUNNING_FRAME).varargs().to_vec();
    for i in 0..cnt {
        let val = varargs.get(i).cloned().unwrap_or(Value::Nil);
        vm.stack[dest + i] = val;
    }
    if all {
        vm.top = dest + cnt;
    }
}

fn op_vararg(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::Vararg { dst, count } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    vararg_inner(vm, base, dst, count);
    unsafe {
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

#[inline(always)]
fn set_list_inner(vm: &mut VM, base: usize, table: u8, list: u8, count: u8) -> Result<(), String> {
    let start = base + list as usize;
    let end = if count == MULTRET {
        vm.top.max(start)
    } else {
        start + count as usize
    };
    let end = end.min(vm.stack.len());
    if let Value::Table(t) = &vm.stack[base + table as usize] {
        let mut tbl = t.borrow_mut();
        tbl.array.reserve(end.saturating_sub(start));
        tbl.array.extend(vm.stack[start..end].iter().cloned());
        Ok(())
    } else {
        Err("cannot SetList to non-table".to_string())
    }
}

fn op_set_list(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::SetList {
        table,
        base: list,
        count,
    } = inst
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    if let Err(e) = set_list_inner(vm, base, table, list, count) {
        return fail_with(vm, pc, e);
    }
    unsafe {
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

fn op_tfor_call(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::TForCall { base: b, retc } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    sync_frame_pc(vm, pc);
    if let Err(e) = vm.tfor_call(base, b, retc) {
        return fail_with(vm, pc, e);
    }
    if vm.trap {
        sync_frame_pc(vm, pc);
        vm.run_result = None;
        return STATUS_OK;
    }
    unsafe {
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

fn op_tfor_loop(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::TForLoop { base: b, jump } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let src = regs.add(b as usize + 3);
        if matches!(&*src, Value::Nil) {
            let next_pc = pc.offset(jump as isize);
            dispatch!(vm, regs, next_pc, consts, dtbl);
        }
        copy_slot(regs.add(b as usize + 2), src);
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}

fn op_close_upval(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let inst = raw.0;
    let ExecInst::CloseUpval { reg } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    if !vm.open_upvalues.is_empty() {
        let base = unsafe { base_of(vm, regs) };
        vm.close_upvalue_at(base + reg as usize);
    }
    unsafe {
        dispatch!(vm, regs, pc, consts, dtbl);
    }
}
