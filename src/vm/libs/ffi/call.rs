//! Calling a C function pointer with arguments known only at run time.
//!
//! There is no libffi here. Each supported calling convention is covered by
//! a small, fixed set of function-pointer shapes that put every argument in
//! the register the callee reads it from:
//!
//! - **System V x86-64 and AArch64** (Linux, macOS, BSD, Windows on Arm)
//!   assign integer and floating-point arguments to two independent register
//!   files in order. One shape, `fn(u64 × 8, f64 × 8)`, therefore reaches any
//!   mix of up to 6 (x86-64) or 8 (AArch64) integer arguments and 8 float
//!   arguments; the callee ignores the registers it does not use.
//! - **Windows x64** assigns the first four arguments by position, each to
//!   either an integer or an XMM register, and the rest to 8-byte stack
//!   slots. Sixteen shapes cover every int/float pattern of the first four,
//!   and four trailing `u64` stack slots carry arguments five to eight.
//!
//! An `f32` is passed in the low 32 bits of a float register (or stack slot),
//! which is where the callee reads it. Return values are read as a raw `u64`
//! or `f64` and narrowed to the declared type, since the upper bits of a
//! narrower return register are unspecified.
//!
//! Variadic functions (`printf`), structs passed by value, and callbacks are
//! not supported.

use std::ffi::{CStr, c_char, c_void};

use super::FfiType;
use crate::vm::value::Value;

/// One argument after conversion to its C representation.
#[derive(Clone, Copy, Debug)]
pub(super) enum AbiArg {
    Int(u64),
    F32(f32),
    F64(f64),
}

impl AbiArg {
    fn is_float(self) -> bool {
        matches!(self, AbiArg::F32(_) | AbiArg::F64(_))
    }

    /// The bits for a float register: an `f32` sits in the low 32 bits.
    fn as_f64(self) -> f64 {
        match self {
            AbiArg::Int(i) => f64::from_bits(i),
            AbiArg::F32(f) => f64::from_bits(u64::from(f.to_bits())),
            AbiArg::F64(f) => f,
        }
    }

    /// The bits for an integer register or an 8-byte stack slot.
    fn as_u64(self) -> u64 {
        match self {
            AbiArg::Int(i) => i,
            AbiArg::F32(f) => u64::from(f.to_bits()),
            AbiArg::F64(f) => f.to_bits(),
        }
    }
}

/// The raw return register.
enum RawRet {
    Int(u64),
    Float(f64),
}

#[cfg(all(not(windows), target_arch = "x86_64"))]
const MAX_INT_ARGS: usize = 6;
#[cfg(target_arch = "aarch64")]
const MAX_INT_ARGS: usize = 8;
#[cfg(any(target_arch = "aarch64", all(not(windows), target_arch = "x86_64")))]
const MAX_FLOAT_ARGS: usize = 8;
#[cfg(all(windows, target_arch = "x86_64"))]
const MAX_ARGS: usize = 8;

/// Checks a signature against what this platform's dispatcher can reach, so
/// `ffi.open` rejects it up front instead of on the first call.
pub(super) fn check_signature(args: &[FfiType]) -> Result<(), String> {
    let floats = args
        .iter()
        .filter(|t| matches!(t, FfiType::F32 | FfiType::F64))
        .count();
    let ints = args.len() - floats;
    #[cfg(any(target_arch = "aarch64", all(not(windows), target_arch = "x86_64")))]
    {
        if ints > MAX_INT_ARGS {
            return Err(format!(
                "FFI supports at most {} integer/pointer arguments on this platform (got {})",
                MAX_INT_ARGS, ints
            ));
        }
        if floats > MAX_FLOAT_ARGS {
            return Err(format!(
                "FFI supports at most {} floating-point arguments on this platform (got {})",
                MAX_FLOAT_ARGS, floats
            ));
        }
        Ok(())
    }
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        let _ = (ints, floats);
        if args.len() > MAX_ARGS {
            return Err(format!(
                "FFI supports at most {} arguments on this platform (got {})",
                MAX_ARGS,
                args.len()
            ));
        }
        Ok(())
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        let _ = (ints, floats);
        Err("ffi.open is not supported on this CPU architecture".to_string())
    }
}

#[cfg(any(target_arch = "aarch64", all(not(windows), target_arch = "x86_64")))]
unsafe fn call_raw(ptr: *const c_void, args: &[AbiArg], float_ret: bool) -> RawRet {
    let mut ints = [0u64; 8];
    let mut floats = [0f64; 8];
    let (mut ni, mut nf) = (0, 0);
    for arg in args {
        if arg.is_float() {
            floats[nf] = arg.as_f64();
            nf += 1;
        } else {
            ints[ni] = arg.as_u64();
            ni += 1;
        }
    }
    let [i0, i1, i2, i3, i4, i5, i6, i7] = ints;
    let [f0, f1, f2, f3, f4, f5, f6, f7] = floats;
    type IntFn = unsafe extern "C" fn(
        u64,
        u64,
        u64,
        u64,
        u64,
        u64,
        u64,
        u64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
    ) -> u64;
    type FloatFn = unsafe extern "C" fn(
        u64,
        u64,
        u64,
        u64,
        u64,
        u64,
        u64,
        u64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
        f64,
    ) -> f64;
    // SAFETY: the caller guarantees `ptr` is a non-variadic C function whose
    // parameters are the classes in `args`; see the module docs for why the
    // wider shape reaches exactly those registers.
    unsafe {
        if float_ret {
            let f: FloatFn = std::mem::transmute(ptr);
            RawRet::Float(f(
                i0, i1, i2, i3, i4, i5, i6, i7, f0, f1, f2, f3, f4, f5, f6, f7,
            ))
        } else {
            let f: IntFn = std::mem::transmute(ptr);
            RawRet::Int(f(
                i0, i1, i2, i3, i4, i5, i6, i7, f0, f1, f2, f3, f4, f5, f6, f7,
            ))
        }
    }
}

#[cfg(all(windows, target_arch = "x86_64"))]
unsafe fn call_raw(ptr: *const c_void, args: &[AbiArg], float_ret: bool) -> RawRet {
    let mut slots = [AbiArg::Int(0); 8];
    slots[..args.len()].copy_from_slice(args);
    let mask = slots[..4]
        .iter()
        .enumerate()
        .fold(0u8, |m, (i, a)| m | (u8::from(a.is_float()) << i));
    let [s4, s5, s6, s7] = [
        slots[4].as_u64(),
        slots[5].as_u64(),
        slots[6].as_u64(),
        slots[7].as_u64(),
    ];

    trait Slot {
        fn from_arg(arg: AbiArg) -> Self;
    }
    impl Slot for u64 {
        fn from_arg(arg: AbiArg) -> Self {
            arg.as_u64()
        }
    }
    impl Slot for f64 {
        fn from_arg(arg: AbiArg) -> Self {
            arg.as_f64()
        }
    }

    macro_rules! shape {
        ($t0:ty, $t1:ty, $t2:ty, $t3:ty) => {{
            let (a0, a1, a2, a3) = (
                <$t0>::from_arg(slots[0]),
                <$t1>::from_arg(slots[1]),
                <$t2>::from_arg(slots[2]),
                <$t3>::from_arg(slots[3]),
            );
            if float_ret {
                let f: unsafe extern "C" fn($t0, $t1, $t2, $t3, u64, u64, u64, u64) -> f64 =
                    std::mem::transmute(ptr);
                RawRet::Float(f(a0, a1, a2, a3, s4, s5, s6, s7))
            } else {
                let f: unsafe extern "C" fn($t0, $t1, $t2, $t3, u64, u64, u64, u64) -> u64 =
                    std::mem::transmute(ptr);
                RawRet::Int(f(a0, a1, a2, a3, s4, s5, s6, s7))
            }
        }};
    }

    // SAFETY: as for the System V version; bit `i` of `mask` picks the
    // register file for argument `i`.
    unsafe {
        match mask {
            0b0000 => shape!(u64, u64, u64, u64),
            0b0001 => shape!(f64, u64, u64, u64),
            0b0010 => shape!(u64, f64, u64, u64),
            0b0011 => shape!(f64, f64, u64, u64),
            0b0100 => shape!(u64, u64, f64, u64),
            0b0101 => shape!(f64, u64, f64, u64),
            0b0110 => shape!(u64, f64, f64, u64),
            0b0111 => shape!(f64, f64, f64, u64),
            0b1000 => shape!(u64, u64, u64, f64),
            0b1001 => shape!(f64, u64, u64, f64),
            0b1010 => shape!(u64, f64, u64, f64),
            0b1011 => shape!(f64, f64, u64, f64),
            0b1100 => shape!(u64, u64, f64, f64),
            0b1101 => shape!(f64, u64, f64, f64),
            0b1110 => shape!(u64, f64, f64, f64),
            _ => shape!(f64, f64, f64, f64),
        }
    }
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
unsafe fn call_raw(_ptr: *const c_void, _args: &[AbiArg], _float_ret: bool) -> RawRet {
    unreachable!("check_signature rejects every signature on this architecture")
}

fn unsigned(v: u64) -> Value {
    if v <= i64::MAX as u64 {
        Value::Int(v as i64)
    } else {
        Value::from_bigint(num_bigint::BigInt::from(v))
    }
}

/// Calls `ptr` with `args` and converts the return register to `ret`.
///
/// # Safety
/// `ptr` must be a live, non-variadic C function whose parameter list
/// matches `args` (which `check_signature` accepted) and whose return type
/// is `ret`. Pointer arguments must stay valid for the call.
pub(super) unsafe fn call(
    ptr: *const c_void,
    args: &[AbiArg],
    ret: FfiType,
) -> Result<Value, String> {
    let float_ret = matches!(ret, FfiType::F32 | FfiType::F64);
    let raw = unsafe { call_raw(ptr, args, float_ret) };
    Ok(match (ret, raw) {
        (FfiType::Void, _) => Value::Nil,
        (FfiType::F32, RawRet::Float(f)) => {
            Value::Float(f64::from(f32::from_bits(f.to_bits() as u32)))
        }
        (FfiType::F64, RawRet::Float(f)) => Value::Float(f),
        (_, RawRet::Float(_)) => unreachable!("float return for an integer type"),
        (ty, RawRet::Int(r)) => match ty {
            FfiType::Bool => Value::Bool(r as u8 != 0),
            FfiType::I8 => Value::Int(i64::from(r as i8)),
            FfiType::U8 => Value::Int(i64::from(r as u8)),
            FfiType::I16 => Value::Int(i64::from(r as i16)),
            FfiType::U16 => Value::Int(i64::from(r as u16)),
            FfiType::I32 => Value::Int(i64::from(r as i32)),
            FfiType::U32 => Value::Int(i64::from(r as u32)),
            FfiType::I64 => Value::Int(r as i64),
            FfiType::ISize => Value::Int(r as usize as isize as i64),
            FfiType::U64 => unsigned(r),
            FfiType::USize => unsigned(r as usize as u64),
            FfiType::Ptr if r as usize == 0 => Value::Nil,
            FfiType::Ptr => Value::Int(r as usize as i64),
            FfiType::CStr => {
                let p = r as usize as *const c_char;
                if p.is_null() {
                    Value::Nil
                } else {
                    // SAFETY: the signature declared a C string return.
                    Value::string(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
                }
            }
            FfiType::Void | FfiType::F32 | FfiType::F64 => unreachable!(),
        },
    })
}
