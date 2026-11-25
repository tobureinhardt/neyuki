//! Threaded tail-call bytecode interpreter (`TRAP == false`).
//!
//! Compiled only under `cfg(neyuki_threaded)`, which build.rs sets for the
//! opt-in `threaded` feature in optimized 64-bit x86/ARM builds without debug
//! assertions. Rust does not guarantee tail calls; those are the builds where
//! the handler-to-handler calls are known to become jumps.
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
use crate::vm::value::{
    INLINE_INT_MAX, INLINE_INT_MIN, TAG_RC_START, Upvalue, Value, ValueView, VmClosure, VmTable,
};

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

type ValueBinOpFn = fn(&Value, &Value) -> Result<Value, String>;
type ValueBinOpIntoFn = fn(&mut [Value], usize, usize, usize) -> Result<(), String>;
type ValueCmpFn = fn(&Value, &Value) -> Result<bool, String>;
type ArithBinOpSpec = (
    u8,
    u8,
    u8,
    ValueBinOpFn,
    &'static str,
    Option<ValueBinOpIntoFn>,
);
type ArithConstSpec = (u8, u8, u16, ValueBinOpFn, &'static str);
type ArithImmSpec = (u8, u8, i16, ValueBinOpFn, &'static str);
type EvalBinOpSpec = (u8, u8, u8, ValueBinOpFn);
type EvalConstSpec = (u8, u8, u16, ValueBinOpFn);
type CmpBinOpSpec = (u8, u8, i16, ValueCmpFn, &'static str, bool);
type CmpConstSpec = (u8, u16, i16, ValueCmpFn, &'static str, bool);
type CmpImmSpec = (u8, i16, i16, ValueCmpFn, &'static str, bool);

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

// What a slow path that does not dispatch hands back to its dispatcher: the
// register window to continue with, or one of the two sentinels below.
// Keeping the work in a separate non-inlined function means the dispatcher
// has no locals at all, so nothing (no `Drop`, no escaped stack address)
// stops its dispatch from being a tail call.
const STEP_ERR: *mut Value = std::ptr::null_mut();
const STEP_TRAP: *mut Value = std::ptr::without_provenance_mut(1);

macro_rules! dispatch_step {
    ($vm:expr, $step:expr, $pc:expr, $consts:expr, $dtbl:expr) => {{
        let regs: *mut Value = $step;
        if regs == STEP_ERR {
            return STATUS_ERR;
        }
        if regs == STEP_TRAP {
            return STATUS_OK;
        }
        unsafe { dispatch!($vm, regs, $pc, $consts, $dtbl) }
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
const fn fits_inline_int(i: i64) -> bool {
    i >= INLINE_INT_MIN && i <= INLINE_INT_MAX
}

// The slow paths below overwrite a slot that may hold a heap value, which
// must be dropped. They return instead of dispatching: a cold helper with
// more than the six handler arguments cannot be sibling-called, so a handler
// that ends in a call to one grows the native stack on every dispatch.
#[cold]
#[inline(never)]
unsafe fn cold_set_int(slot: *mut Value, val: i64) {
    unsafe { (*slot).set_inplace(Value::int(val)) }
}

#[cold]
#[inline(never)]
unsafe fn cold_set_trivial(slot: *mut Value, bits: u64) {
    unsafe { (*slot).set_inplace(Value::from_trivial_bits(bits)) }
}

macro_rules! write_int_dispatch {
    ($vm:expr, $regs:expr, $pc:expr, $consts:expr, $dtbl:expr, $dst:expr, $val:expr) => {{
        let _slot = $regs.add($dst as usize);
        let _v: i64 = $val;
        if (*_slot).is_trivial() && fits_inline_int(_v) {
            std::ptr::write(_slot, Value::int(_v));
            dispatch!($vm, $regs, $pc, $consts, $dtbl);
        }
        cold_set_int($regs.add($dst as usize), _v);
        dispatch!($vm, $regs, $pc, $consts, $dtbl);
    }};
}

macro_rules! write_float_dispatch {
    ($vm:expr, $regs:expr, $pc:expr, $consts:expr, $dtbl:expr, $dst:expr, $val:expr) => {{
        let _slot = $regs.add($dst as usize);
        // Bits, not a `Value`: a live `Value` would be dropped after the
        // dispatch and keep it from being a tail call.
        let _bits = Value::float($val).bits();
        if (*_slot).is_trivial() {
            std::ptr::write(_slot, Value::from_trivial_bits(_bits));
            dispatch!($vm, $regs, $pc, $consts, $dtbl);
        }
        cold_set_trivial(_slot, _bits);
        dispatch!($vm, $regs, $pc, $consts, $dtbl);
    }};
}

macro_rules! write_bool_dispatch {
    ($vm:expr, $regs:expr, $pc:expr, $consts:expr, $dtbl:expr, $dst:expr, $val:expr) => {{
        let _slot = $regs.add($dst as usize);
        // Bits, not a `Value`: a live `Value` would be dropped after the
        // dispatch and keep it from being a tail call.
        let _bits = Value::bool($val).bits();
        if (*_slot).is_trivial() {
            std::ptr::write(_slot, Value::from_trivial_bits(_bits));
            dispatch!($vm, $regs, $pc, $consts, $dtbl);
        }
        cold_set_trivial(_slot, _bits);
        dispatch!($vm, $regs, $pc, $consts, $dtbl);
    }};
}

#[inline(always)]
unsafe fn write_int(slot: *mut Value, val: i64) {
    unsafe {
        (&mut *slot).set_inplace(Value::int(val));
    }
}

#[inline(always)]
unsafe fn write_float(slot: *mut Value, val: f64) {
    unsafe {
        (&mut *slot).set_inplace(Value::float(val));
    }
}

#[inline(always)]
unsafe fn write_bool(slot: *mut Value, val: bool) {
    unsafe {
        (&mut *slot).set_inplace(Value::bool(val));
    }
}

#[inline(always)]
unsafe fn set_slot_val(slot: *mut Value, val: Value) {
    unsafe {
        (&mut *slot).set_inplace(val);
    }
}

#[inline(always)]
unsafe fn copy_slot(dst: *mut Value, src: *const Value) {
    unsafe {
        (&mut *dst).assign_ref(&*src);
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
        vm.stack.resize(window_end, Value::NIL);
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
            vm.run_result = Some(Value::NIL);
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
    let (dst, site) = match raw.0 {
        ExecInst::GetGlobal { dst, site, .. } => (dst, site),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    unsafe {
        if let Some(val) = (*vm.cur_exec).global_cache_get(site, vm.globals_epoch) {
            set_slot_val(regs.add(dst as usize), val);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    cold_get_global(vm, regs, pc, consts, dtbl, raw)
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
        if (*slot).is_trivial() {
            std::ptr::write(slot, Value::NIL);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    unsafe {
        cold_set_trivial(regs.add(dst as usize), Value::NIL.bits());
        dispatch!(vm, regs, pc, consts, dtbl);
    }
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
        let src_bits = src.bits();
        let slot = regs.add(dst as usize);
        if src_bits < TAG_RC_START && (*slot).is_trivial() {
            std::ptr::write(slot, Value::from_trivial_bits(src_bits));
            dispatch!(vm, regs, pc, consts, dtbl);
        }
        if (*slot).bits() == src_bits {
            dispatch!(vm, regs, pc, consts, dtbl);
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
        let src_ref = &*regs.add(src as usize);
        let src_bits = src_ref.bits();
        let slot = regs.add(dst as usize);
        if (*slot).is_trivial() {
            if src_bits < TAG_RC_START {
                std::ptr::write(slot, Value::from_trivial_bits(src_bits));
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            match src_ref.view() {
                ValueView::Closure(c) => {
                    std::ptr::write(slot, Value::closure(c.to_rc()));
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                ValueView::String(st) => {
                    std::ptr::write(slot, Value::str_ref(st.to_str_ref()));
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                _ => {}
            }
        } else if (*slot).bits() == src_bits {
            dispatch!(vm, regs, pc, consts, dtbl);
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
            let v_bits = val.bits();
            let slot = regs.add(dst as usize);
            if v_bits < TAG_RC_START && (*slot).is_trivial() {
                std::ptr::write(slot, Value::from_trivial_bits(v_bits));
                dispatch!(vm, regs, pc, consts, dtbl);
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
    cold_get_global(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_get_global(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (dst, name_k, site) = match raw.0 {
        ExecInst::GetGlobal { dst, name_k, site } => (dst, name_k, Some(site)),
        ExecInst::GetGlobalDynamic { dst, name_k } => (dst, name_k, None),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    unsafe {
        // `name` is dropped before the dispatch so it can be a tail call.
        let val = {
            let Some(name) = (&*consts.add(name_k as usize)).as_str_ref() else {
                return fail_msg(vm, pc, "global name must be string");
            };
            let name = name.to_str_ref();
            vm.globals.get(&*name).cloned().unwrap_or(Value::NIL)
        };
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
        let Some(name) = (&*consts.add(name_k as usize)).as_str_ref() else {
            return false;
        };
        let val = (*regs.add(src as usize)).clone();
        if let Some(slot) = vm.globals.get_mut(name.as_str()) {
            *slot = val;
        } else {
            vm.globals.insert(name.to_str_ref(), val);
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
            Some(idx) if idx < vm.stack.len() => {
                copy_slot(regs.add(dst as usize), vm.stack.as_ptr().add(idx));
            }
            // An upvalue still open past the end of the stack reads as nil,
            // matching the loop interpreter's `upvalue_get`.
            Some(_) => set_slot_val(regs.add(dst as usize), Value::NIL),
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
            Some(idx) if idx < vm.stack.len() => vm.stack.as_ptr().add(idx),
            None => uv.closed_ptr() as *const Value,
            _ => return cold_get_upval_dispatch(vm, regs, pc, consts, dtbl, raw),
        };
        let src_ref = &*src_ptr;
        let src_bits = src_ref.bits();
        let slot = regs.add(dst as usize);
        if (*slot).is_trivial() {
            if src_bits < TAG_RC_START {
                std::ptr::write(slot, Value::from_trivial_bits(src_bits));
                dispatch!(vm, regs, pc, consts, dtbl);
            }
            match src_ref.view() {
                ValueView::Closure(c) => {
                    std::ptr::write(slot, Value::closure(c.to_rc()));
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                ValueView::String(st) => {
                    std::ptr::write(slot, Value::str_ref(st.to_str_ref()));
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                _ => {}
            }
        } else if (*slot).bits() == src_bits {
            dispatch!(vm, regs, pc, consts, dtbl);
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
        let src_bits = (*regs.add(src as usize)).bits();
        if src_bits < TAG_RC_START && (*dst_ptr).is_trivial() {
            std::ptr::write(dst_ptr, Value::from_trivial_bits(src_bits));
            dispatch!(vm, regs, pc, consts, dtbl);
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
        set_slot_val(new_regs.add(dst as usize), Value::table(rc));
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
        if let (Some(t), Some(i)) = (tbl_val.as_table(), k.as_int())
            && i >= 1
            && let Ok(tbl) = t.try_borrow()
        {
            let idx = i as usize - 1;
            if idx < tbl.array.len() {
                let v_bits = tbl.array[idx].bits();
                // A nil hole goes to the cold path so `__index` runs.
                if v_bits < TAG_RC_START && (v_bits != Value::NIL.bits() || tbl.metatable.is_none())
                {
                    drop(tbl);
                    let slot = regs.add(dst as usize);
                    if (*slot).is_trivial() {
                        std::ptr::write(slot, Value::from_trivial_bits(v_bits));
                        dispatch!(vm, regs, pc, consts, dtbl);
                    }
                    cold_set_trivial(regs.add(dst as usize), v_bits);
                    dispatch!(vm, regs, pc, consts, dtbl);
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
        if let (Some(t), Some(s)) = (tbl_val.as_table(), k.as_str_ref())
            && let Ok(tbl) = t.try_borrow()
        {
            let ptr = t.as_ptr() as usize;
            let shape = tbl.fields.shape_version();
            let exec = &*vm.cur_exec;
            if let Some(ic) = exec.table_cache_get(site)
                && ic.table_ptr == ptr
                && ic.shape_version == shape
                && let Some(v) =
                    tbl.fields
                        .get_slot_keyed_vmstr(ic.slot_idx as usize, s.as_ptr(), s.vm_str())
            {
                let v_bits = v.bits();
                if v_bits < TAG_RC_START && (v_bits != Value::NIL.bits() || tbl.metatable.is_none())
                {
                    drop(tbl);
                    let slot = regs.add(dst as usize);
                    if (*slot).is_trivial() {
                        std::ptr::write(slot, Value::from_trivial_bits(v_bits));
                        dispatch!(vm, regs, pc, consts, dtbl);
                    }
                    cold_set_trivial(regs.add(dst as usize), v_bits);
                    dispatch!(vm, regs, pc, consts, dtbl);
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
            if let (Some(t), Some(s)) = (tbl_val.as_table(), k.as_str_ref()) {
                let ptr = t.as_ptr() as usize;
                let tbl = t.borrow();
                let shape = tbl.fields.shape_version();
                let exec = &*vm.cur_exec;
                if let Some(slot_idx) = tbl.fields.find_index_by_vmstr(s.as_ptr(), s.vm_str())
                    && let Some(v) = tbl.fields.get_slot(slot_idx)
                    && (!v.is_nil() || tbl.metatable.is_none())
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
                    set_slot_val(regs.add(dst as usize), Value::NIL);
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
        if let (Some(t), Some(i)) = (tbl_val.as_table(), k.as_int())
            && i >= 1
            && !v.is_nil()
        {
            let mut tbl = t.borrow_mut();
            if !tbl.frozen {
                let idx = i as usize - 1;
                if idx < tbl.array.len() {
                    tbl.array[idx].assign_ref(v);
                    return true;
                } else if idx == tbl.array.len() && tbl.metatable.is_none() && tbl.general.is_none()
                {
                    tbl.array.push(v.clone());
                    return true;
                }
            }
        } else if let (Some(t), Some(s)) = (tbl_val.as_table(), k.as_str_ref())
            && !v.is_nil()
        {
            let mut tbl = t.borrow_mut();
            if !tbl.frozen {
                if tbl.metatable.is_none() {
                    tbl.fields.set_by_vmstr(s.as_ptr(), s.vm_str(), v);
                    return true;
                } else if let Some(slot) = tbl.fields.get_mut_by_vmstr(s.as_ptr(), s.vm_str()) {
                    slot.assign_ref(v);
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
        if let (Some(t), Some(i)) = (tbl_val.as_table(), k.as_int())
            && i >= 1
            && !v.is_nil()
            && v.is_trivial()
            && let Ok(mut tbl) = t.try_borrow_mut()
            && !tbl.frozen
        {
            let idx = i as usize - 1;
            if idx < tbl.array.len() && tbl.array[idx].is_trivial() {
                std::ptr::write(&mut tbl.array[idx], Value::from_trivial_bits(v.bits()));
                drop(tbl);
                dispatch!(vm, regs, pc, consts, dtbl);
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
        if let (Some(t), Some(s)) = (tbl_val.as_table(), k.as_str_ref())
            && !v.is_nil()
        {
            let ptr = t.as_ptr() as usize;
            let mut tbl = t.borrow_mut();
            if !tbl.frozen {
                let shape = tbl.fields.shape_version();
                let exec = &*vm.cur_exec;
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
                    return true;
                }
                if let Some(slot_idx) = tbl.fields.find_index_by_vmstr(s.as_ptr(), s.vm_str()) {
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
        if let (Some(t), Some(s)) = (tbl_val.as_table(), k.as_str_ref())
            && !v.is_nil()
            && v.is_trivial()
            && let Ok(mut tbl) = t.try_borrow_mut()
            && !tbl.frozen
        {
            let ptr = t.as_ptr() as usize;
            let shape = tbl.fields.shape_version();
            let exec = &*vm.cur_exec;
            if let Some(ic) = exec.table_cache_get(site)
                && ic.table_ptr == ptr
                && ic.shape_version == shape
                && let Some(slot_v) = tbl
                    .fields
                    .get_slot_keyed_mut_vmstr_ptr(ic.slot_idx as usize, s.as_ptr())
                && slot_v.is_trivial()
            {
                std::ptr::write(slot_v, Value::from_trivial_bits(v.bits()));
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
        if let Some(t) = (&*regs.add(table as usize)).as_table() {
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
        if let Some(t) = (&*regs.add(table as usize)).as_table()
            && let Ok(mut tbl) = t.try_borrow_mut()
            && tbl.array.len() < tbl.array.capacity()
        {
            let v_bits = (&*regs.add(src as usize)).bits();
            if v_bits < TAG_RC_START {
                let len = tbl.array.len();
                std::ptr::write(
                    tbl.array.as_mut_ptr().add(len),
                    Value::from_trivial_bits(v_bits),
                );
                tbl.array.set_len(len + 1);
                drop(tbl);
                dispatch!(vm, regs, pc, consts, dtbl);
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
    cold_get_import(vm, regs, pc, consts, dtbl, raw)
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
        let (mname, fname) = match (
            (&*consts.add(mod_k as usize)).as_str_ref(),
            (&*consts.add(field_k as usize)).as_str_ref(),
        ) {
            (Some(m), Some(f)) => (m.to_str_ref(), f.to_str_ref()),
            _ => return fail_msg(vm, pc, "import names must be strings"),
        };
        let module_val = vm.globals.get(&*mname).cloned().unwrap_or(Value::NIL);
        let fkey = Value::str_ref(fname);
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
        if let Some(t) = module_val.as_table() {
            let cacheable = {
                let tbl = t.borrow();
                tbl.frozen && tbl.metatable.is_none() && !value.is_nil()
            };
            if cacheable {
                (*vm.cur_exec).import_store(
                    site as usize,
                    vm.globals_epoch,
                    t.to_rc(),
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
fn cold_arith_binop_ok(
    vm: &mut VM,
    base: usize,
    pc: *const Cell<AlignedExec>,
    raw: AlignedExec,
) -> bool {
    let (dst, a, b, slow, event, into): ArithBinOpSpec = match raw.0 {
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
    unsafe {
        let regs = vm.stack.as_mut_ptr().add(base);
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if (ra.is_table() || rb.is_table())
            && let Some(mm) = vm.get_binop_metamethod(ra, rb, event)
        {
            let (va, vb) = (ra.clone(), rb.clone());
            sync_frame_pc(vm, pc);
            if let Err(e) = vm.call_binop_mm(mm, va, vb, base, dst) {
                let _ = fail_with(vm, pc, e);
                return false;
            }
            return true;
        }
        if let Some(into_fn) = into {
            if let Err(e) = into_fn(
                &mut vm.stack,
                base + dst as usize,
                base + a as usize,
                base + b as usize,
            ) {
                let _ = fail_with(vm, pc, e);
                return false;
            }
        } else {
            match slow(ra, rb) {
                Ok(res) => set_slot_val(regs.add(dst as usize), res),
                Err(e) => {
                    let _ = fail_with(vm, pc, e);
                    return false;
                }
            }
        }
        true
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
    let base = unsafe { base_of(vm, regs) };
    if !cold_arith_binop_ok(vm, base, pc, raw) {
        return STATUS_ERR;
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

#[cold]
#[inline(never)]
fn cold_arith_k_ok(
    vm: &mut VM,
    base: usize,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    raw: AlignedExec,
) -> bool {
    let (dst, a, k, slow, event): ArithConstSpec = match raw.0 {
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
    unsafe {
        let regs = vm.stack.as_mut_ptr().add(base);
        let ra = &*regs.add(a as usize);
        let rb = &*consts.add(k as usize);
        if (ra.is_table() || rb.is_table())
            && let Some(mm) = vm.get_binop_metamethod(ra, rb, event)
        {
            let (va, vb) = (ra.clone(), rb.clone());
            sync_frame_pc(vm, pc);
            if let Err(e) = vm.call_binop_mm(mm, va, vb, base, dst) {
                let _ = fail_with(vm, pc, e);
                return false;
            }
            return true;
        }
        match slow(ra, rb) {
            Ok(res) => {
                set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res);
                true
            }
            Err(e) => {
                let _ = fail_with(vm, pc, e);
                false
            }
        }
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
    let base = unsafe { base_of(vm, regs) };
    if !cold_arith_k_ok(vm, base, pc, consts, raw) {
        return STATUS_ERR;
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

#[cold]
#[inline(never)]
fn cold_arith_imm_ok(
    vm: &mut VM,
    base: usize,
    pc: *const Cell<AlignedExec>,
    raw: AlignedExec,
) -> bool {
    let (dst, a, imm, slow, event): ArithImmSpec = match raw.0 {
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
    unsafe {
        let regs = vm.stack.as_mut_ptr().add(base);
        let ra = &*regs.add(a as usize);
        let rb = Value::int(imm as i64);
        if ra.is_table()
            && let Some(mm) = vm.get_binop_metamethod(ra, &rb, event)
        {
            let va = ra.clone();
            sync_frame_pc(vm, pc);
            if let Err(e) = vm.call_binop_mm(mm, va, rb, base, dst) {
                let _ = fail_with(vm, pc, e);
                return false;
            }
            return true;
        }
        match slow(ra, &rb) {
            Ok(res) => {
                set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res);
                true
            }
            Err(e) => {
                let _ = fail_with(vm, pc, e);
                false
            }
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
    let base = unsafe { base_of(vm, regs) };
    if !cold_arith_imm_ok(vm, base, pc, raw) {
        return STATUS_ERR;
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
                let ra = &*regs.add(a as usize);
                let rb = &*regs.add(b as usize);
                if ra.is_inline_int()
                    && rb.is_inline_int()
                    && let Some(r) = ra
                        .as_inline_int_unchecked()
                        .$checked(rb.as_inline_int_unchecked())
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
                let ra = &*regs.add(a as usize);
                let rb = &*regs.add(b as usize);
                if ra.is_float() && rb.is_float() {
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        ra.as_float_unchecked() $fop rb.as_float_unchecked()
                    );
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
                let ra = &*regs.add(a as usize);
                let rb = &*regs.add(b as usize);
                if ra.is_inline_int() && rb.is_inline_int() {
                    if let Some(r) = ra
                        .as_inline_int_unchecked()
                        .$checked(rb.as_inline_int_unchecked())
                    {
                        (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int { dst, a, b }));
                        write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                    }
                } else if ra.is_float() && rb.is_float() {
                    (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_flt { dst, a, b }));
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        ra.as_float_unchecked() $fop rb.as_float_unchecked()
                    );
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
                let ra = &*regs.add(a as usize);
                let rb = &*regs.add(b as usize);
                if ra.is_inline_int() && rb.is_inline_int() {
                    if let Some(r) = ra
                        .as_inline_int_unchecked()
                        .$checked(rb.as_inline_int_unchecked())
                    {
                        write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                    }
                } else if ra.is_float() && rb.is_float() {
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        ra.as_float_unchecked() $fop rb.as_float_unchecked()
                    );
                } else if ra.is_inline_int() && rb.is_float() {
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        ra.as_inline_int_unchecked() as f64 $fop rb.as_float_unchecked()
                    );
                } else if ra.is_float() && rb.is_inline_int() {
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        ra.as_float_unchecked() $fop rb.as_inline_int_unchecked() as f64
                    );
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
                let ra = &*regs.add(a as usize);
                let rb = &*consts.add(k as usize);
                if ra.is_inline_int()
                    && rb.is_inline_int()
                    && let Some(r) = ra
                        .as_inline_int_unchecked()
                        .$checked(rb.as_inline_int_unchecked())
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
                let ra = &*regs.add(a as usize);
                let rb = &*consts.add(k as usize);
                if ra.is_float() && rb.is_float() {
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        ra.as_float_unchecked() $fop rb.as_float_unchecked()
                    );
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
                let ra = &*regs.add(a as usize);
                let rb = &*consts.add(k as usize);
                if ra.is_inline_int() && rb.is_inline_int() {
                    if let Some(r) = ra
                        .as_inline_int_unchecked()
                        .$checked(rb.as_inline_int_unchecked())
                    {
                        (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int { dst, a, k }));
                        write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                    }
                } else if ra.is_float() && rb.is_float() {
                    (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_flt { dst, a, k }));
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        ra.as_float_unchecked() $fop rb.as_float_unchecked()
                    );
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
                let ra = &*regs.add(a as usize);
                let rb = &*consts.add(k as usize);
                if ra.is_inline_int() && rb.is_inline_int() {
                    if let Some(r) = ra
                        .as_inline_int_unchecked()
                        .$checked(rb.as_inline_int_unchecked())
                    {
                        write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                    }
                } else if ra.is_float() && rb.is_float() {
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        ra.as_float_unchecked() $fop rb.as_float_unchecked()
                    );
                } else if ra.is_inline_int() && rb.is_float() {
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        (ra.as_inline_int_unchecked() as f64) $fop rb.as_float_unchecked()
                    );
                } else if ra.is_float() && rb.is_inline_int() {
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        ra.as_float_unchecked() $fop (rb.as_inline_int_unchecked() as f64)
                    );
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
                let ra = &*regs.add(a as usize);
                if ra.is_inline_int()
                    && let Some(r) = ra.as_inline_int_unchecked().$checked(imm as i64)
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
                let ra = &*regs.add(a as usize);
                if ra.is_inline_int() {
                    if let Some(r) = ra.as_inline_int_unchecked().$checked(imm as i64) {
                        (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int { dst, a, imm }));
                        write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                    }
                } else if ra.is_float() {
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        ra.as_float_unchecked() $fop (imm as f64)
                    );
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
                let ra = &*regs.add(a as usize);
                if ra.is_inline_int() {
                    if let Some(r) = ra.as_inline_int_unchecked().$checked(imm as i64) {
                        write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, r);
                    }
                } else if ra.is_float() {
                    write_float_dispatch!(
                        vm,
                        regs,
                        pc,
                        consts,
                        dtbl,
                        dst,
                        ra.as_float_unchecked() $fop (imm as f64)
                    );
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
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let x = ra.as_inline_int_unchecked();
            let y = rb.as_inline_int_unchecked();
            if y != 0 {
                let val = if x >= 0 && y > 0 {
                    x / y
                } else {
                    num_integer::Integer::div_floor(&x, &y)
                };
                write_int(regs.add(dst as usize), val);
                dispatch!(vm, regs, pc, consts, dtbl);
            }
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
        let ra = &*regs.add(a as usize);
        if ra.is_inline_int() {
            write_int_dispatch!(
                vm,
                regs,
                pc,
                consts,
                dtbl,
                dst,
                ra.as_inline_int_unchecked() >> shift
            );
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
    dispatch_step!(
        vm,
        cold_eval_idiv_pow2_slow(vm, regs, pc, raw),
        pc,
        consts,
        dtbl
    )
}

#[inline(never)]
fn cold_eval_idiv_pow2_slow(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    raw: AlignedExec,
) -> *mut Value {
    let inst = raw.0;
    let ExecInst::IDivIntPow2 { dst, a, shift } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        // Bound first: a temporary in the scrutinee would be dropped after
        // the dispatch in the arm, so the dispatch could not be a tail call.
        let res = ops::eval_idiv(ra, &Value::int(1i64 << shift));
        match res {
            Ok(res) => {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), res);
                new_regs
            }
            Err(e) => {
                fail_with(vm, pc, e);
                STEP_ERR
            }
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
        let ra = &*regs.add(a as usize);
        let rb = &*consts.add(k as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let x = ra.as_inline_int_unchecked();
            let y = rb.as_inline_int_unchecked();
            let val = if x >= 0 {
                x / y
            } else {
                num_integer::Integer::div_floor(&x, &y)
            };
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
        let ra = &*regs.add(a as usize);
        let rb = &*consts.add(k as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let x = ra.as_inline_int_unchecked();
            let y = rb.as_inline_int_unchecked();
            if y > 0 {
                if (y as u64).is_power_of_two() {
                    let shift = y.trailing_zeros() as u8;
                    (&*pc.sub(1)).set(AlignedExec(ExecInst::IDivIntPow2 { dst, a, shift }));
                    write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, x >> shift);
                } else {
                    (&*pc.sub(1)).set(AlignedExec(ExecInst::IDivIntK { dst, a, k }));
                    let val = if x >= 0 {
                        x / y
                    } else {
                        num_integer::Integer::div_floor(&x, &y)
                    };
                    write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, val);
                }
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
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let x = ra.as_inline_int_unchecked();
            let y = rb.as_inline_int_unchecked();
            if y != 0 {
                let val = if y == -1 {
                    0
                } else if x >= 0 && y > 0 {
                    x % y
                } else {
                    x.mod_floor(&y)
                };
                write_int(regs.add(dst as usize), val);
                dispatch!(vm, regs, pc, consts, dtbl);
            }
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
        let ra = &*regs.add(a as usize);
        if ra.is_inline_int() {
            write_int_dispatch!(
                vm,
                regs,
                pc,
                consts,
                dtbl,
                dst,
                ra.as_inline_int_unchecked() & ((1i64 << shift) - 1)
            );
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
    dispatch_step!(
        vm,
        cold_eval_mod_pow2_slow(vm, regs, pc, raw),
        pc,
        consts,
        dtbl
    )
}

#[inline(never)]
fn cold_eval_mod_pow2_slow(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    raw: AlignedExec,
) -> *mut Value {
    let inst = raw.0;
    let ExecInst::ModIntPow2 { dst, a, shift } = inst else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        // Bound first: a temporary in the scrutinee would be dropped after
        // the dispatch in the arm, so the dispatch could not be a tail call.
        let res = ops::eval_mod(ra, &Value::int(1i64 << shift));
        match res {
            Ok(res) => {
                let new_regs = vm.stack.as_mut_ptr().add(base);
                set_slot_val(new_regs.add(dst as usize), res);
                new_regs
            }
            Err(e) => {
                fail_with(vm, pc, e);
                STEP_ERR
            }
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
        let ra = &*regs.add(a as usize);
        let rb = &*consts.add(k as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let x = ra.as_inline_int_unchecked();
            let y = rb.as_inline_int_unchecked();
            let val = if x >= 0 {
                if x < y { x } else { x % y }
            } else {
                x.mod_floor(&y)
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
        let ra = &*regs.add(a as usize);
        let rb = &*consts.add(k as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let x = ra.as_inline_int_unchecked();
            let y = rb.as_inline_int_unchecked();
            if y > 0 {
                if (y as u64).is_power_of_two() {
                    let shift = y.trailing_zeros() as u8;
                    (&*pc.sub(1)).set(AlignedExec(ExecInst::ModIntPow2 { dst, a, shift }));
                    write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, x & ((1i64 << shift) - 1));
                } else {
                    (&*pc.sub(1)).set(AlignedExec(ExecInst::ModIntK { dst, a, k }));
                    let val = if x >= 0 {
                        if x < y { x } else { x % y }
                    } else {
                        x.mod_floor(&y)
                    };
                    write_int_dispatch!(vm, regs, pc, consts, dtbl, dst, val);
                }
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
fn cold_eval_binop_plain_ok(
    vm: &mut VM,
    base: usize,
    pc: *const Cell<AlignedExec>,
    raw: AlignedExec,
) -> bool {
    let (dst, a, b, eval): EvalBinOpSpec = match raw.0 {
        ExecInst::IDiv { dst, a, b } => (dst, a, b, ops::eval_idiv),
        ExecInst::Mod { dst, a, b } => (dst, a, b, ops::eval_mod),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    unsafe {
        let regs = vm.stack.as_mut_ptr().add(base);
        match eval(&*regs.add(a as usize), &*regs.add(b as usize)) {
            Ok(res) => {
                set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res);
                true
            }
            Err(e) => {
                let _ = fail_with(vm, pc, e);
                false
            }
        }
    }
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
    let base = unsafe { base_of(vm, regs) };
    if !cold_eval_binop_plain_ok(vm, base, pc, raw) {
        return STATUS_ERR;
    }
    unsafe {
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn cold_eval_k_plain_ok(
    vm: &mut VM,
    base: usize,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    raw: AlignedExec,
) -> bool {
    let (dst, a, k, eval): EvalConstSpec = match raw.0 {
        ExecInst::IDivK { dst, a, k }
        | ExecInst::IDivIntK { dst, a, k }
        | ExecInst::IDivKGeneric { dst, a, k } => (dst, a, k, ops::eval_idiv),
        ExecInst::ModK { dst, a, k }
        | ExecInst::ModIntK { dst, a, k }
        | ExecInst::ModKGeneric { dst, a, k } => (dst, a, k, ops::eval_mod),
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    unsafe {
        let regs = vm.stack.as_mut_ptr().add(base);
        match eval(&*regs.add(a as usize), &*consts.add(k as usize)) {
            Ok(res) => {
                set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res);
                true
            }
            Err(e) => {
                let _ = fail_with(vm, pc, e);
                false
            }
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
    let base = unsafe { base_of(vm, regs) };
    if !cold_eval_k_plain_ok(vm, base, pc, consts, raw) {
        return STATUS_ERR;
    }
    unsafe {
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, pc, consts, dtbl);
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
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            write_int(
                regs.add(dst as usize),
                ra.as_inline_int_unchecked() & rb.as_inline_int_unchecked(),
            );
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
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            write_int(
                regs.add(dst as usize),
                ra.as_inline_int_unchecked() | rb.as_inline_int_unchecked(),
            );
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
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            write_int(
                regs.add(dst as usize),
                ra.as_inline_int_unchecked() ^ rb.as_inline_int_unchecked(),
            );
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
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let x = ra.as_inline_int_unchecked();
            let y = rb.as_inline_int_unchecked();
            if x >= 0 && y >= 0 {
                if x == 0 {
                    write_int(regs.add(dst as usize), 0);
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
                let headroom = x.leading_zeros().saturating_sub(1) as i64;
                if y <= headroom {
                    write_int(regs.add(dst as usize), x << y);
                    dispatch!(vm, regs, pc, consts, dtbl);
                }
            }
        }
    }
    cold_shl(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_shl(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let ExecInst::Shl { dst, a, b } = raw.0 else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        match (ra.view(), rb.view()) {
            (ValueView::Int(x), ValueView::Int(y)) if x >= 0 && y >= 0 => {
                let headroom128 = (64 + x.leading_zeros() as i64).saturating_sub(1);
                let res = if y <= headroom128 {
                    Value::from_i128((x as i128) << y)
                } else {
                    let x_bits = 64 - x.leading_zeros() as u64;
                    if (y as u64) <= ops::MAX_INT_BITS
                        && x_bits.saturating_add(y as u64) <= ops::MAX_INT_BITS
                    {
                        Value::from_bigint_rc(Rc::new(num_bigint::BigInt::from(x) << (y as usize)))
                    } else {
                        match ops::eval_shl(ra, rb) {
                            Ok(v) => v,
                            Err(e) => return fail_with(vm, pc, e),
                        }
                    }
                };
                set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res);
            }
            (ValueView::BigInt(_), ValueView::Int(y)) if y >= 0 => {
                if let Err(e) = ops::eval_shl_bigint_into(
                    &mut vm.stack,
                    base + dst as usize,
                    base + a as usize,
                    y as u64,
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
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let x = ra.as_inline_int_unchecked();
            let y = rb.as_inline_int_unchecked();
            if y >= 0 {
                let val = if y >= 64 {
                    if x < 0 { -1 } else { 0 }
                } else {
                    x >> y
                };
                write_int(regs.add(dst as usize), val);
                dispatch!(vm, regs, pc, consts, dtbl);
            }
        }
    }
    cold_shr(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_shr(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let ExecInst::Shr { dst, a, b } = raw.0 else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        match (ra.view(), rb.view()) {
            (ValueView::BigInt(_), ValueView::Int(y)) if y >= 0 => {
                ops::eval_shr_bigint_into(
                    &mut vm.stack,
                    base + dst as usize,
                    base + a as usize,
                    y as u64,
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
        let ra = &*regs.add(a as usize);
        if ra.is_inline_int() {
            let x = ra.as_inline_int_unchecked();
            if x >= 0 {
                if x == 0 {
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
    }
    cold_shl_i(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_shl_i_ok(vm: &mut VM, base: usize, pc: *const Cell<AlignedExec>, raw: AlignedExec) -> bool {
    let ExecInst::ShlI { dst, a, imm } = raw.0 else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let ra = &*vm.stack.as_ptr().add(base + a as usize);
        match ra.view() {
            ValueView::Int(x) if x >= 0 => {
                let shift = imm as u32;
                let headroom128 = (64 + x.leading_zeros()).saturating_sub(1);
                let res = if shift <= headroom128 {
                    Value::from_i128((x as i128) << shift)
                } else {
                    let x_bits = 64 - x.leading_zeros() as u64;
                    if (shift as u64) <= ops::MAX_INT_BITS
                        && x_bits.saturating_add(shift as u64) <= ops::MAX_INT_BITS
                    {
                        Value::from_bigint_rc(Rc::new(
                            num_bigint::BigInt::from(x) << (imm as usize),
                        ))
                    } else {
                        let rb = Value::int(imm as i64);
                        match ops::eval_shl(ra, &rb) {
                            Ok(v) => v,
                            Err(e) => {
                                let _ = fail_with(vm, pc, e);
                                return false;
                            }
                        }
                    }
                };
                set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res);
            }
            ValueView::BigInt(_) => {
                if let Err(e) = ops::eval_shl_bigint_into(
                    &mut vm.stack,
                    base + dst as usize,
                    base + a as usize,
                    imm as u64,
                ) {
                    let _ = fail_with(vm, pc, e);
                    return false;
                }
            }
            _ => {
                let rb = Value::int(imm as i64);
                match ops::eval_shl(ra, &rb) {
                    Ok(res) => set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res),
                    Err(e) => {
                        let _ = fail_with(vm, pc, e);
                        return false;
                    }
                }
            }
        }
        true
    }
}

#[cold]
#[inline(never)]
fn cold_shl_i(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let base = unsafe { base_of(vm, regs) };
    if !cold_shl_i_ok(vm, base, pc, raw) {
        return STATUS_ERR;
    }
    unsafe {
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
        let ra = &*regs.add(a as usize);
        if ra.is_inline_int() {
            let x = ra.as_inline_int_unchecked();
            let val = if imm >= 64 {
                if x < 0 { -1 } else { 0 }
            } else {
                x >> imm
            };
            write_int(regs.add(dst as usize), val);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    cold_shr_i(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_shr_i(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let ExecInst::ShrI { dst, a, imm } = raw.0 else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let ra = &*regs.add(a as usize);
        if ra.is_bigint() {
            ops::eval_shr_bigint_into(
                &mut vm.stack,
                base + dst as usize,
                base + a as usize,
                imm as u64,
            );
        } else {
            let rb = Value::int(imm as i64);
            match ops::eval_shr(ra, &rb) {
                Ok(res) => set_slot_val(vm.stack.as_mut_ptr().add(base + dst as usize), res),
                Err(e) => return fail_with(vm, pc, e),
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
        let res = if let (Some(x), Some(y)) = (ra.as_int(), rb.as_int()) {
            let word = x as u64;
            let bits = if y < 0 { usize::MAX } else { y as usize };
            let val = if bits >= 64 {
                0
            } else if left {
                word << bits
            } else {
                word >> bits
            };
            Value::from_u64(val)
        } else if left {
            ops::eval_lshl(ra, rb)?
        } else {
            ops::eval_lshr(ra, rb)?
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
        let res = if let (Some(x), Some(y)) = (ra.as_str(), rb.as_str()) {
            let mut s = String::with_capacity(x.len() + y.len());
            s.push_str(x);
            s.push_str(y);
            Value::string(s)
        } else if ra.is_table() || rb.is_table() {
            if let Some(mm) = vm.get_binop_metamethod(ra, rb, "__concat") {
                let (va, vb) = (ra.clone(), rb.clone());
                sync_frame_pc(vm, pc);
                return vm.call_binop_mm(mm, va, vb, base, dst);
            }
            Value::string(format!("{}{}", ra, rb))
        } else {
            Value::string(format!("{}{}", ra, rb))
        };
        set_slot_val(regs.add(dst as usize), res);
        Ok(())
    }
}

#[inline(never)]
unsafe fn concat_strings_inplace(regs: *mut Value, dst: u8, a: u8, b: u8) {
    unsafe {
        if let (Some(x), Some(y)) = (
            (&*regs.add(a as usize)).as_str(),
            (&*regs.add(b as usize)).as_str(),
        ) {
            let mut s = String::with_capacity(x.len() + y.len());
            s.push_str(x);
            s.push_str(y);
            set_slot_val(regs.add(dst as usize), Value::string(s));
        }
    }
}

#[cold]
#[inline(never)]
fn cold_concat_ok(
    vm: &mut VM,
    base: usize,
    pc: *const Cell<AlignedExec>,
    raw: AlignedExec,
) -> bool {
    let ExecInst::Concat { dst, a, b } = raw.0 else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    unsafe {
        let regs = vm.stack.as_mut_ptr().add(base);
        if let Err(e) = concat_inner(vm, regs, pc, base, dst, a, b) {
            let _ = fail_with(vm, pc, e);
            return false;
        }
        true
    }
}

#[cold]
#[inline(never)]
fn cold_concat(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let base = unsafe { base_of(vm, regs) };
    if !cold_concat_ok(vm, base, pc, raw) {
        return STATUS_ERR;
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
        if (&*regs.add(a as usize)).is_string() && (&*regs.add(b as usize)).is_string() {
            concat_strings_inplace(regs, dst, a, b);
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    cold_concat(vm, regs, pc, consts, dtbl, raw)
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
        let rv = &*regs.add(src as usize);
        if rv.is_inline_int() {
            let i = rv.as_inline_int_unchecked();
            if i != INLINE_INT_MIN {
                write_int(regs.add(dst as usize), -i);
                dispatch!(vm, regs, pc, consts, dtbl);
            }
        } else if rv.is_float() {
            write_float(regs.add(dst as usize), -rv.as_float_unchecked());
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    cold_unm(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_unm(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    dispatch_step!(vm, cold_unm_slow(vm, regs, pc, raw), pc, consts, dtbl)
}

#[inline(never)]
fn cold_unm_slow(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    raw: AlignedExec,
) -> *mut Value {
    let ExecInst::Unm { dst, src } = raw.0 else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let v = &*regs.add(src as usize);
        if v.is_table() {
            if let Some(mm) = vm.get_unop_metamethod(v, "__unm") {
                let v = v.clone();
                sync_frame_pc(vm, pc);
                let called = vm.call_function(mm, &[v]);
                match called {
                    Ok(res) => {
                        if vm.trap {
                            sync_frame_pc(vm, pc);
                            vm.run_result = None;
                            return STEP_TRAP;
                        }
                        let new_regs = vm.stack.as_mut_ptr().add(base);
                        set_slot_val(
                            new_regs.add(dst as usize),
                            res.into_iter().next().unwrap_or(Value::NIL),
                        );
                        return new_regs;
                    }
                    Err(e) => {
                        fail_with(vm, pc, e);
                        return STEP_ERR;
                    }
                }
            }
            fail_msg(vm, pc, "unary minus expects a number");
            return STEP_ERR;
        }
        if let Err(e) = ops::eval_unm_into(&mut vm.stack, base + dst as usize, base + src as usize)
        {
            {
                fail_with(vm, pc, e);
                return STEP_ERR;
            }
        }
        vm.stack.as_mut_ptr().add(base)
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
        match (&*regs.add(src as usize)).view() {
            ValueView::String(s) => Some(s.len()),
            ValueView::Table(t) => {
                let tbl = t.borrow();
                if tbl.metatable.is_none() {
                    Some(tbl.array.len())
                } else {
                    None
                }
            }
            ValueView::Buffer(b) => Some(b.borrow().len()),
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
    cold_len(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_len(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    dispatch_step!(vm, cold_len_slow(vm, regs, pc, raw), pc, consts, dtbl)
}

#[inline(never)]
fn cold_len_slow(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    raw: AlignedExec,
) -> *mut Value {
    let ExecInst::Len { dst, src } = raw.0 else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let v = &*regs.add(src as usize);
        let len = match v.view() {
            ValueView::String(s) => s.len(),
            ValueView::Table(t) => {
                if let Some(mm) = vm.get_unop_metamethod(v, "__len") {
                    let v = v.clone();
                    sync_frame_pc(vm, pc);
                    let called = vm.call_function(mm, &[v]);
                    match called {
                        Ok(res) => {
                            if vm.trap {
                                sync_frame_pc(vm, pc);
                                vm.run_result = None;
                                return STEP_TRAP;
                            }
                            let new_regs = vm.stack.as_mut_ptr().add(base);
                            set_slot_val(
                                new_regs.add(dst as usize),
                                res.into_iter().next().unwrap_or(Value::NIL),
                            );
                            return new_regs;
                        }
                        Err(e) => {
                            fail_with(vm, pc, e);
                            return STEP_ERR;
                        }
                    }
                }
                t.borrow().array.len()
            }
            ValueView::Buffer(b) => b.borrow().len(),
            _ => {
                fail_msg(vm, pc, "len expects string, table or buffer");
                return STEP_ERR;
            }
        };
        let new_regs = vm.stack.as_mut_ptr().add(base);
        set_slot_val(new_regs.add(dst as usize), Value::from_usize(len));
        new_regs
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
        let rv = &*regs.add(src as usize);
        if rv.is_inline_int() {
            write_int(regs.add(dst as usize), !rv.as_inline_int_unchecked());
            dispatch!(vm, regs, pc, consts, dtbl);
        }
    }
    cold_bit_not(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_bit_not(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let ExecInst::BitNot { dst, src } = raw.0 else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    unsafe {
        let res = ops::eval_bitnot(&*regs.add(src as usize));
        match res {
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
        let src_reg = if (&*regs.add(a as usize)).is_nil() {
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
fn cold_cmp_slow_eval(
    vm: &mut VM,
    base: usize,
    pc: *const Cell<AlignedExec>,
    raw: AlignedExec,
) -> i8 {
    let (a, b, _, slow, event, mm_swap): CmpBinOpSpec = match raw.0 {
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
    let holds = unsafe {
        let regs = vm.stack.as_ptr().add(base);
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if ra.is_table() || rb.is_table() {
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
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            let _ = fail_with(vm, pc, e);
            -1
        }
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
    let jump_if_false = match raw.0 {
        ExecInst::Lt { jump_if_false, .. }
        | ExecInst::LtInt { jump_if_false, .. }
        | ExecInst::LtGeneric { jump_if_false, .. }
        | ExecInst::Le { jump_if_false, .. }
        | ExecInst::LeInt { jump_if_false, .. }
        | ExecInst::LeGeneric { jump_if_false, .. }
        | ExecInst::Gt { jump_if_false, .. }
        | ExecInst::Ge { jump_if_false, .. } => jump_if_false,
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    let res = cold_cmp_slow_eval(vm, base, pc, raw);
    if res < 0 {
        return STATUS_ERR;
    }
    if vm.trap {
        sync_frame_pc(vm, pc);
        vm.run_result = None;
        return STATUS_OK;
    }
    unsafe {
        let next_pc = if res == 0 {
            pc.offset(jump_if_false as isize)
        } else {
            pc
        };
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, next_pc, consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn cold_cmp_k_slow_eval(
    vm: &mut VM,
    base: usize,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    raw: AlignedExec,
) -> i8 {
    let (a, k, _, slow, event, mm_swap): CmpConstSpec = match raw.0 {
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
    let holds = unsafe {
        let ra = &*vm.stack.as_ptr().add(base + a as usize);
        let rb = &*consts.add(k as usize);
        if ra.is_table() || rb.is_table() {
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
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            let _ = fail_with(vm, pc, e);
            -1
        }
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
    let jump_if_false = match raw.0 {
        ExecInst::LtK { jump_if_false, .. }
        | ExecInst::LtIntK { jump_if_false, .. }
        | ExecInst::LtKGeneric { jump_if_false, .. }
        | ExecInst::LeK { jump_if_false, .. }
        | ExecInst::LeIntK { jump_if_false, .. }
        | ExecInst::LeKGeneric { jump_if_false, .. }
        | ExecInst::GtK { jump_if_false, .. }
        | ExecInst::GeK { jump_if_false, .. } => jump_if_false,
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    let res = cold_cmp_k_slow_eval(vm, base, pc, consts, raw);
    if res < 0 {
        return STATUS_ERR;
    }
    if vm.trap {
        sync_frame_pc(vm, pc);
        vm.run_result = None;
        return STATUS_OK;
    }
    unsafe {
        let next_pc = if res == 0 {
            pc.offset(jump_if_false as isize)
        } else {
            pc
        };
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, next_pc, consts, dtbl);
    }
}

#[cold]
#[inline(never)]
fn cold_cmp_imm_slow_eval(
    vm: &mut VM,
    base: usize,
    pc: *const Cell<AlignedExec>,
    raw: AlignedExec,
) -> i8 {
    let (a, imm, _, slow, event, mm_swap): CmpImmSpec = match raw.0 {
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
    let holds = unsafe {
        let ra = &*vm.stack.as_ptr().add(base + a as usize);
        let rb = Value::int(imm as i64);
        if ra.is_table() {
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
        Ok(true) => 1,
        Ok(false) => 0,
        Err(e) => {
            let _ = fail_with(vm, pc, e);
            -1
        }
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
    let jump_if_false = match raw.0 {
        ExecInst::LtI { jump_if_false, .. }
        | ExecInst::LtIntI { jump_if_false, .. }
        | ExecInst::LtIGeneric { jump_if_false, .. }
        | ExecInst::LeI { jump_if_false, .. }
        | ExecInst::LeIntI { jump_if_false, .. }
        | ExecInst::LeIGeneric { jump_if_false, .. }
        | ExecInst::GtI { jump_if_false, .. }
        | ExecInst::GeI { jump_if_false, .. } => jump_if_false,
        _ => unsafe { std::hint::unreachable_unchecked() },
    };
    let base = unsafe { base_of(vm, regs) };
    let res = cold_cmp_imm_slow_eval(vm, base, pc, raw);
    if res < 0 {
        return STATUS_ERR;
    }
    if vm.trap {
        sync_frame_pc(vm, pc);
        vm.run_result = None;
        return STATUS_OK;
    }
    unsafe {
        let next_pc = if res == 0 {
            pc.offset(jump_if_false as isize)
        } else {
            pc
        };
        let new_regs = vm.stack.as_mut_ptr().add(base);
        dispatch!(vm, new_regs, next_pc, consts, dtbl);
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
                let ra = &*regs.add(a as usize);
                let rb = &*regs.add(b as usize);
                if ra.is_inline_int() && rb.is_inline_int() {
                    let next_pc =
                        if !(ra.as_inline_int_unchecked() $op rb.as_inline_int_unchecked()) {
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
                let ra = &*regs.add(a as usize);
                let rb = &*regs.add(b as usize);
                if ra.is_inline_int() && rb.is_inline_int() {
                    (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int {
                        a,
                        b,
                        jump_if_false,
                    }));
                    let next_pc =
                        if !(ra.as_inline_int_unchecked() $op rb.as_inline_int_unchecked()) {
                            pc.offset(jump_if_false as isize)
                        } else {
                            pc
                        };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
                } else if ra.is_float() && rb.is_float() {
                    let next_pc = if ra.as_float_unchecked() $op rb.as_float_unchecked() {
                        pc
                    } else {
                        pc.offset(jump_if_false as isize)
                    };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
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
                let ra = &*regs.add(a as usize);
                let rb = &*regs.add(b as usize);
                if ra.is_inline_int() && rb.is_inline_int() {
                    let next_pc =
                        if !(ra.as_inline_int_unchecked() $op rb.as_inline_int_unchecked()) {
                            pc.offset(jump_if_false as isize)
                        } else {
                            pc
                        };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
                } else if ra.is_float() && rb.is_float() {
                    let next_pc = if ra.as_float_unchecked() $op rb.as_float_unchecked() {
                        pc
                    } else {
                        pc.offset(jump_if_false as isize)
                    };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
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
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let next_pc = if !(ra.as_inline_int_unchecked() > rb.as_inline_int_unchecked()) {
                pc.offset(jump_if_false as isize)
            } else {
                pc
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
        } else if ra.is_float() && rb.is_float() {
            let next_pc = if ra.as_float_unchecked() > rb.as_float_unchecked() {
                pc
            } else {
                pc.offset(jump_if_false as isize)
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
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
        let ra = &*regs.add(a as usize);
        let rb = &*regs.add(b as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let next_pc = if !(ra.as_inline_int_unchecked() >= rb.as_inline_int_unchecked()) {
                pc.offset(jump_if_false as isize)
            } else {
                pc
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
        } else if ra.is_float() && rb.is_float() {
            let next_pc = if ra.as_float_unchecked() >= rb.as_float_unchecked() {
                pc
            } else {
                pc.offset(jump_if_false as isize)
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
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
        let ra = &*regs.add(a as usize);
        let rb = &*consts.add(k as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let next_pc = if ra.as_inline_int_unchecked() != rb.as_inline_int_unchecked() {
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
    cold_eq_k(vm, regs, pc, consts, dtbl, raw)
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
        let ra = &*regs.add(a as usize);
        let rb = &*consts.add(k as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            (&*pc.sub(1)).set(AlignedExec(ExecInst::EqIntK {
                a,
                k,
                jump_if_false,
            }));
            let next_pc = if ra.as_inline_int_unchecked() != rb.as_inline_int_unchecked() {
                pc.offset(jump_if_false as isize)
            } else {
                pc
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
        }
    }
    cold_eq_k(vm, regs, pc, consts, dtbl, raw)
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
    cold_eq_k(vm, regs, pc, consts, dtbl, raw)
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
    cold_eq_k(vm, regs, pc, consts, dtbl, raw)
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
                let ra = &*regs.add(a as usize);
                let rb = &*consts.add(k as usize);
                if ra.is_inline_int() && rb.is_inline_int() {
                    let next_pc =
                        if !(ra.as_inline_int_unchecked() $op rb.as_inline_int_unchecked()) {
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
                let ra = &*regs.add(a as usize);
                let rb = &*consts.add(k as usize);
                if ra.is_inline_int() && rb.is_inline_int() {
                    (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int {
                        a,
                        k,
                        jump_if_false,
                    }));
                    let next_pc =
                        if !(ra.as_inline_int_unchecked() $op rb.as_inline_int_unchecked()) {
                            pc.offset(jump_if_false as isize)
                        } else {
                            pc
                        };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
                } else if ra.is_float() && rb.is_float() {
                    let next_pc = if ra.as_float_unchecked() $op rb.as_float_unchecked() {
                        pc
                    } else {
                        pc.offset(jump_if_false as isize)
                    };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
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
                let ra = &*regs.add(a as usize);
                let rb = &*consts.add(k as usize);
                if ra.is_inline_int() && rb.is_inline_int() {
                    let next_pc =
                        if !(ra.as_inline_int_unchecked() $op rb.as_inline_int_unchecked()) {
                            pc.offset(jump_if_false as isize)
                        } else {
                            pc
                        };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
                } else if ra.is_float() && rb.is_float() {
                    let next_pc = if ra.as_float_unchecked() $op rb.as_float_unchecked() {
                        pc
                    } else {
                        pc.offset(jump_if_false as isize)
                    };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
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
        let ra = &*regs.add(a as usize);
        let rb = &*consts.add(k as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let next_pc = if !(ra.as_inline_int_unchecked() > rb.as_inline_int_unchecked()) {
                pc.offset(jump_if_false as isize)
            } else {
                pc
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
        } else if ra.is_float() && rb.is_float() {
            let next_pc = if ra.as_float_unchecked() > rb.as_float_unchecked() {
                pc
            } else {
                pc.offset(jump_if_false as isize)
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
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
        let ra = &*regs.add(a as usize);
        let rb = &*consts.add(k as usize);
        if ra.is_inline_int() && rb.is_inline_int() {
            let next_pc = if !(ra.as_inline_int_unchecked() >= rb.as_inline_int_unchecked()) {
                pc.offset(jump_if_false as isize)
            } else {
                pc
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
        } else if ra.is_float() && rb.is_float() {
            let next_pc = if ra.as_float_unchecked() >= rb.as_float_unchecked() {
                pc
            } else {
                pc.offset(jump_if_false as isize)
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
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
        let ra = &*regs.add(a as usize);
        let is_eq = if ra.is_inline_int() {
            ra.as_inline_int_unchecked() == imm as i64
        } else if ra.is_float() {
            ra.as_float_unchecked() == imm as f64
        } else {
            false
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
        let ra = &*regs.add(a as usize);
        let is_eq = if ra.is_inline_int() {
            ra.as_inline_int_unchecked() == imm as i64
        } else if ra.is_float() {
            ra.as_float_unchecked() == imm as f64
        } else {
            false
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
                let ra = &*regs.add(a as usize);
                if ra.is_inline_int() {
                    let next_pc = if !(ra.as_inline_int_unchecked() $op imm as i64) {
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
                let ra = &*regs.add(a as usize);
                if ra.is_inline_int() {
                    (&*pc.sub(1)).set(AlignedExec(ExecInst::$var_int {
                        a,
                        imm,
                        jump_if_false,
                    }));
                    let next_pc = if !(ra.as_inline_int_unchecked() $op imm as i64) {
                        pc.offset(jump_if_false as isize)
                    } else {
                        pc
                    };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
                } else if ra.is_float() {
                    let next_pc = if ra.as_float_unchecked() $op imm as f64 {
                        pc
                    } else {
                        pc.offset(jump_if_false as isize)
                    };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
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
                let ra = &*regs.add(a as usize);
                if ra.is_inline_int() {
                    let next_pc = if !(ra.as_inline_int_unchecked() $op imm as i64) {
                        pc.offset(jump_if_false as isize)
                    } else {
                        pc
                    };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
                } else if ra.is_float() {
                    let next_pc = if ra.as_float_unchecked() $op imm as f64 {
                        pc
                    } else {
                        pc.offset(jump_if_false as isize)
                    };
                    dispatch!(vm, regs, next_pc, consts, dtbl);
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
        let ra = &*regs.add(a as usize);
        if ra.is_inline_int() {
            let next_pc = if !(ra.as_inline_int_unchecked() > imm as i64) {
                pc.offset(jump_if_false as isize)
            } else {
                pc
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
        } else if ra.is_float() {
            let next_pc = if ra.as_float_unchecked() > imm as f64 {
                pc
            } else {
                pc.offset(jump_if_false as isize)
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
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
        let ra = &*regs.add(a as usize);
        if ra.is_inline_int() {
            let next_pc = if !(ra.as_inline_int_unchecked() >= imm as i64) {
                pc.offset(jump_if_false as isize)
            } else {
                pc
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
        } else if ra.is_float() {
            let next_pc = if ra.as_float_unchecked() >= imm as f64 {
                pc
            } else {
                pc.offset(jump_if_false as isize)
            };
            dispatch!(vm, regs, next_pc, consts, dtbl);
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
                let target_pc = pc.offset(jump as isize);
                if fits_inline_int(diff)
                    && (0..=INLINE_INT_MAX as i128).contains(&iters)
                    && let ExecInst::ForLoop { base: fb, jump: fj }
                    | ExecInst::ForLoopInt { base: fb, jump: fj }
                    | ExecInst::ForLoopCount { base: fb, jump: fj }
                    | ExecInst::ForLoopCount1 { base: fb, jump: fj }
                    | ExecInst::ForLoopGeneric { base: fb, jump: fj } = (&*target_pc).get().0
                    && fb == b
                {
                    let specialized = if s == 1 {
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
    }
    cold_for_prep(vm, regs, pc, consts, dtbl, raw)
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
        let rem_ptr = regs.add(b as usize + 1);
        let i_ptr = regs.add(b as usize);
        let st_ref = &*regs.add(b as usize + 2);
        if (*rem_ptr).is_inline_int() && (*i_ptr).is_inline_int() && st_ref.is_inline_int() {
            let rem = (*rem_ptr).as_inline_int_unchecked();
            if rem > 0 {
                let next = (*i_ptr)
                    .as_inline_int_unchecked()
                    .wrapping_add(st_ref.as_inline_int_unchecked());
                if fits_inline_int(next) {
                    std::ptr::write(rem_ptr, Value::int(rem - 1));
                    std::ptr::write(i_ptr, Value::int(next));
                    let next_pc = pc.offset(jump as isize);
                    write_int_dispatch!(vm, regs, next_pc, consts, dtbl, b + 3, next);
                }
            } else {
                dispatch!(vm, regs, pc, consts, dtbl);
            }
        }
        (&*pc.sub(1)).set(AlignedExec(ExecInst::ForLoopGeneric { base: b, jump }));
    }
    cold_for_loop(vm, regs, pc, consts, dtbl, raw)
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
        let rem_ptr = regs.add(b as usize + 1);
        let i_ptr = regs.add(b as usize);
        if (*rem_ptr).is_inline_int() && (*i_ptr).is_inline_int() {
            let rem = (*rem_ptr).as_inline_int_unchecked();
            if rem > 0 {
                let next = (*i_ptr).as_inline_int_unchecked().wrapping_add(1);
                if fits_inline_int(next) {
                    std::ptr::write(rem_ptr, Value::int(rem - 1));
                    std::ptr::write(i_ptr, Value::int(next));
                    let next_pc = pc.offset(jump as isize);
                    write_int_dispatch!(vm, regs, next_pc, consts, dtbl, b + 3, next);
                }
            } else {
                dispatch!(vm, regs, pc, consts, dtbl);
            }
        }
        (&*pc.sub(1)).set(AlignedExec(ExecInst::ForLoopGeneric { base: b, jump }));
    }
    cold_for_loop(vm, regs, pc, consts, dtbl, raw)
}

// Slow numeric-for paths are split in two: the work happens in a helper
// that returns the next pc (null after an error), and a six-argument
// dispatcher with no live locals tail-calls the next handler. A `Value` or
// `Result` alive across the dispatch would turn it into a real call.
#[cold]
#[inline(never)]
fn cold_for_prep(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let ExecInst::ForPrep { base: b, jump } = raw.0 else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let next_pc = for_prep_slow(vm, regs, pc, b, jump);
    if next_pc.is_null() {
        return STATUS_ERR;
    }
    unsafe { dispatch!(vm, regs, next_pc, consts, dtbl) }
}

#[inline(never)]
fn for_prep_slow(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    b: u8,
    jump: i16,
) -> *const Cell<AlignedExec> {
    unsafe {
        let target_pc = pc.offset(jump as isize);
        if let ExecInst::ForLoopCount { base: fb, jump: fj }
        | ExecInst::ForLoopCount1 { base: fb, jump: fj } = (&*target_pc).get().0
        {
            (&*target_pc).set(AlignedExec(ExecInst::ForLoop { base: fb, jump: fj }));
        }
        let init = &*regs.add(b as usize);
        let step = &*regs.add(b as usize + 2);
        match step.view() {
            ValueView::Int(0) => {
                fail_msg(vm, pc, "numeric for step cannot be zero");
                return std::ptr::null();
            }
            ValueView::Float(0.0) => {
                {
                    fail_msg(vm, pc, "numeric for step cannot be zero");
                    return std::ptr::null();
                };
            }
            _ => {}
        }
        match ops::eval_sub(init, step) {
            Ok(init_minus_step) => {
                set_slot_val(regs.add(b as usize), init_minus_step);
                target_pc
            }
            Err(e) => {
                fail_with(vm, pc, e);
                std::ptr::null()
            }
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
        let idx = &*regs.add(b as usize);
        let limit = &*regs.add(b as usize + 1);
        let step = &*regs.add(b as usize + 2);
        if idx.is_inline_int() && limit.is_inline_int() && step.is_inline_int() {
            let i = idx.as_inline_int_unchecked();
            let lim = limit.as_inline_int_unchecked();
            let st = step.as_inline_int_unchecked();
            if let Some(next) = i.checked_add(st) {
                let again = if st >= 0 { next <= lim } else { next >= lim };
                write_int(regs.add(b as usize), next);
                if again {
                    write_int(regs.add(b as usize + 3), next);
                    let next_pc = pc.offset(jump as isize);
                    dispatch!(vm, regs, next_pc, consts, dtbl);
                }
                dispatch!(vm, regs, pc, consts, dtbl);
            }
        }
        (&*pc.sub(1)).set(AlignedExec(ExecInst::ForLoopGeneric { base: b, jump }));
    }
    cold_for_loop(vm, regs, pc, consts, dtbl, raw)
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
        let idx = &*regs.add(b as usize);
        let limit = &*regs.add(b as usize + 1);
        let step = &*regs.add(b as usize + 2);
        if idx.is_inline_int() && limit.is_inline_int() && step.is_inline_int() {
            let i = idx.as_inline_int_unchecked();
            let lim = limit.as_inline_int_unchecked();
            let st = step.as_inline_int_unchecked();
            if let Some(next) = i.checked_add(st) {
                (&*pc.sub(1)).set(AlignedExec(ExecInst::ForLoopInt { base: b, jump }));
                let again = if st >= 0 { next <= lim } else { next >= lim };
                write_int(regs.add(b as usize), next);
                if again {
                    write_int(regs.add(b as usize + 3), next);
                    let next_pc = pc.offset(jump as isize);
                    dispatch!(vm, regs, next_pc, consts, dtbl);
                }
                dispatch!(vm, regs, pc, consts, dtbl);
            }
        }
    }
    cold_for_loop(vm, regs, pc, consts, dtbl, raw)
}

fn op_for_loop_generic(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    cold_for_loop(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_for_loop(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let (ExecInst::ForLoop { base: b, jump }
    | ExecInst::ForLoopInt { base: b, jump }
    | ExecInst::ForLoopCount { base: b, jump }
    | ExecInst::ForLoopCount1 { base: b, jump }
    | ExecInst::ForLoopGeneric { base: b, jump }) = raw.0
    else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let next_pc = for_loop_slow(vm, regs, pc, b, jump);
    if next_pc.is_null() {
        return STATUS_ERR;
    }
    unsafe { dispatch!(vm, regs, next_pc, consts, dtbl) }
}

#[inline(never)]
fn for_loop_slow(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    b: u8,
    jump: i16,
) -> *const Cell<AlignedExec> {
    unsafe {
        let idx = &*regs.add(b as usize);
        let limit = &*regs.add(b as usize + 1);
        let step = &*regs.add(b as usize + 2);
        if let (Some(i), Some(lim), Some(st)) = (idx.as_int(), limit.as_int(), step.as_int())
            && let Some(next) = i.checked_add(st)
        {
            let again = if st >= 0 { next <= lim } else { next >= lim };
            write_int(regs.add(b as usize), next);
            if again {
                write_int(regs.add(b as usize + 3), next);
                return pc.offset(jump as isize);
            }
            return pc;
        }
        match step.view() {
            ValueView::Int(0) => {
                fail_msg(vm, pc, "numeric for step cannot be zero");
                return std::ptr::null();
            }
            ValueView::Float(0.0) => {
                fail_msg(vm, pc, "numeric for step cannot be zero");
                return std::ptr::null();
            }
            _ => {}
        }
        let is_positive = match step.view() {
            ValueView::Int(i) => i >= 0,
            ValueView::BigInt(i) => num_traits::Signed::is_positive(i),
            ValueView::Float(f) => f >= 0.0,
            _ => true,
        };
        let next_idx = match ops::eval_add(idx, step) {
            Ok(v) => v,
            Err(e) => {
                fail_with(vm, pc, e);
                return std::ptr::null();
            }
        };
        let loop_again = if is_positive {
            ops::eval_le(&next_idx, limit)
        } else {
            ops::eval_ge(&next_idx, limit)
        };
        let loop_again = match loop_again {
            Ok(b) => b,
            Err(e) => {
                fail_with(vm, pc, e);
                return std::ptr::null();
            }
        };
        set_slot_val(regs.add(b as usize), next_idx.clone());
        if loop_again {
            set_slot_val(regs.add(b as usize + 3), next_idx);
            pc.offset(jump as isize)
        } else {
            pc
        }
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
                .unwrap_or_else(|| Upvalue::closed(Value::NIL));
            upvalues.push(up);
        }
    }
    let val = Value::closure(Rc::new(VmClosure {
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
        let Some(c) = (unsafe { vm.stack.get_unchecked(base + callee as usize) }).as_closure()
        else {
            unreachable!();
        };
        let is_same = c.as_ptr() == vm.cur_closure;
        if !is_same && !c.proto.is_verified() {
            crate::bytecode::verify_proto(&c.proto).map_err(|e| e.to_string())?;
            c.proto.mark_verified();
        }
        is_same
    };
    let Ok(callee_closure) =
        std::mem::replace(&mut vm.stack[base + callee as usize], Value::NIL).into_closure()
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
        vm.stack.resize(needed + 1, Value::NIL);
    }
    for i in argc..num_params {
        vm.stack[new_base + i] = Value::NIL;
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
        vm.stack.resize(args_end, Value::NIL);
    }
    sync_frame_pc(vm, pc);

    unsafe {
        if (&*vm.stack.as_ptr().add(base + callee as usize)).is_closure() {
            match call_closure_inner(vm, pc, consts, base, callee, actual_argc, retc) {
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
            if let Some(c) = (&*callee_ptr).as_closure() {
                let c_ptr = c.as_ptr();
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
                    let callee_closure = match std::ptr::read(callee_ptr).into_closure() {
                        Ok(cl) => cl,
                        Err(other) => {
                            std::mem::forget(other);
                            std::hint::unreachable_unchecked()
                        }
                    };
                    std::ptr::write(callee_ptr, Value::NIL);
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
    if let Some(def) = vm.stack[base + callee as usize].as_native() {
        let func = def.func;
        if argc == 1 && vm.stack[args_start].is_trivial() {
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
        let mut inline_args = [const { Value::NIL }; INLINE_NATIVE_ARGS];
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
    } else if let Some(handler) = vm.stack[base + callee as usize]
        .as_table()
        .map(|t| t.borrow().metamethod("__call"))
    {
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
    } else {
        Err(format!(
            "attempted to call non-function ({:?})",
            vm.stack[base + callee as usize]
        ))
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
        let Some(callee_closure) = (&*regs.add(callee as usize)).as_closure() else {
            return Ok(None);
        };
        if callee_closure.as_ptr() != vm.cur_closure || callee_closure.proto.is_vararg {
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
            let src_bits = (*src_ptr).bits();
            if src_bits < TAG_RC_START && (*dst_ptr).is_trivial() {
                std::ptr::write(dst_ptr, Value::from_trivial_bits(src_bits));
            } else {
                return Ok(None);
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
    // Six arguments, so this stays a tail call; `regs` is re-derived in
    // case the self-tail-call attempt moved the stack.
    let regs = unsafe { vm.stack.as_mut_ptr().add(base) };
    cold_tail_call_general(vm, regs, pc, consts, dtbl, raw)
}

#[cold]
#[inline(never)]
fn cold_tail_call_step(
    vm: &mut VM,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    base: usize,
    callee: u8,
    argc: u8,
) -> Option<(*mut Value, *const Cell<AlignedExec>, *const Value, bool)> {
    let args_start = base + callee as usize + 1;
    let args_end = if argc == MULTRET {
        vm.top.max(args_start)
    } else {
        args_start + argc as usize
    };
    let actual_argc = args_end - args_start;
    if args_end > vm.stack.len() {
        vm.stack.resize(args_end, Value::NIL);
    }
    sync_frame_pc(vm, pc);
    if base >= 1 && vm.stack[base + callee as usize].is_closure() {
        if let Err(e) = vm.limits.check_interrupt() {
            let _ = fail_with(vm, pc, e);
            return None;
        }
        let callee_closure = vm.stack[base + callee as usize].to_closure().unwrap();
        if !callee_closure.proto.is_verified()
            && let Err(e) = crate::bytecode::verify_proto(&callee_closure.proto)
        {
            let _ = fail_with(vm, pc, e.to_string());
            return None;
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
            let v = std::mem::replace(&mut vm.stack[callee_abs + k], Value::NIL);
            vm.stack[caller_dest + k] = v;
        }
        if vm.top > callee_abs {
            vm.top -= shift;
        }
        let needed = base + max_registers;
        if needed >= vm.stack.len() {
            vm.stack.resize(needed + 1, Value::NIL);
        }
        for i in actual_argc..num_params {
            vm.stack[base + i] = Value::NIL;
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
                let _ = fail_with(vm, pc, e);
                return None;
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
                    let _ = fail_with(vm, pc, e);
                    return None;
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
        let new_regs = unsafe { vm.stack.as_mut_ptr().add(base) };
        return Some((new_regs, new_code, new_consts, false));
    }

    if vm.stack[base + callee as usize].is_closure() {
        return match call_closure_inner(vm, pc, consts, base, callee, actual_argc, MULTRET) {
            Ok((new_regs, new_pc, new_consts)) => Some((new_regs, new_pc, new_consts, false)),
            Err(e) => {
                let _ = fail_with(vm, pc, e);
                None
            }
        };
    }
    if call_non_closure_ok(vm, base, callee, args_start, args_end, actual_argc, MULTRET) {
        let new_regs = unsafe { vm.stack.as_mut_ptr().add(base) };
        Some((new_regs, pc, consts, true))
    } else {
        if !vm.is_yielding {
            sync_frame_pc(vm, pc);
        }
        None
    }
}

#[cold]
#[inline(never)]
fn cold_tail_call_general(
    vm: &mut VM,
    regs: *mut Value,
    pc: *const Cell<AlignedExec>,
    consts: *const Value,
    dtbl: DispatchTbl,
    raw: AlignedExec,
) -> u8 {
    let ExecInst::TailCall { callee, argc } = raw.0 else {
        unsafe { std::hint::unreachable_unchecked() }
    };
    let base = unsafe { base_of(vm, regs) };
    match cold_tail_call_step(vm, pc, consts, base, callee, argc) {
        Some((new_regs, new_pc, new_consts, check_trap)) => {
            if check_trap && vm.trap {
                sync_frame_pc(vm, pc);
                vm.run_result = None;
                return STATUS_OK;
            }
            unsafe {
                dispatch!(vm, new_regs, new_pc, new_consts, dtbl);
            }
        }
        None => STATUS_ERR,
    }
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
        let src_bits = (*src_ptr).bits();
        if !(*dst_ptr).is_trivial() || src_bits >= TAG_RC_START {
            return None;
        }
        std::ptr::write(dst_ptr, Value::from_trivial_bits(src_bits));
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
    cold_return_general(vm, pc, dtbl, ret, count)
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
        vm.run_result = Some(vm.returned.first().cloned().unwrap_or(Value::NIL));
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
        vm.stack.resize(needed, Value::NIL);
    }
    unsafe {
        let s = vm.stack.as_mut_ptr();
        if wanted == 1 && nret == 1 {
            let src_ptr = s.add(ret_start);
            let dst_ptr = s.add(caller_dest);
            let v = std::mem::replace(&mut *src_ptr, Value::NIL);
            (*dst_ptr).set_inplace(v);
        } else {
            for i in 0..wanted {
                if i < nret {
                    let src_ptr = s.add(ret_start + i);
                    let dst_ptr = s.add(caller_dest + i);
                    let v = std::mem::replace(&mut *src_ptr, Value::NIL);
                    (*dst_ptr).set_inplace(v);
                } else {
                    *s.add(caller_dest + i) = Value::NIL;
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
        vm.stack.resize(dest + cnt, Value::NIL);
    }
    let varargs: Vec<Value> = vm.frames.last().expect(RUNNING_FRAME).varargs().to_vec();
    for i in 0..cnt {
        let val = varargs.get(i).cloned().unwrap_or(Value::NIL);
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
    if let Some(t) = vm.stack[base + table as usize].as_table() {
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
        if (&*src).is_nil() {
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
