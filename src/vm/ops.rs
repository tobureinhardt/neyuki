//! Arithmetic, bitwise and comparison operators.
//
// Every operator has an `i64` fast path that the interpreter loop inlines,
// and falls back to `BigInt` when an operand is already big or the `i64`
// result overflows. Results always go through `Value::from_bigint`, which
// demotes back to `Int` whenever the answer fits.

use num_bigint::{BigInt, Sign};
use num_integer::Integer as _;
use num_traits::{FromPrimitive, Signed, ToPrimitive, Zero};
use std::rc::Rc;

use crate::vm::value::Value;

/// The largest integer, in bits, that arithmetic will produce (about 315,000
/// decimal digits). Past this, multiplying, raising to a power or shifting
/// left would let one expression allocate gigabytes or run for hours, so it
/// is a catchable error instead.
pub const MAX_INT_BITS: u64 = 1 << 20;

/// Errors when a result of `bits` bits would exceed `MAX_INT_BITS`.
pub fn check_int_bits(bits: u64) -> Result<(), String> {
    if bits > MAX_INT_BITS {
        return Err(format!(
            "integer result is too large (more than {} bits)",
            MAX_INT_BITS
        ));
    }
    Ok(())
}

/// An estimate of the bits in `base ^ exp`, for `check_int_bits`.
pub fn pow_result_bits(base: &BigInt, exp: u64) -> u64 {
    if base.bits() <= 1 {
        // 0, 1 and -1 stay that size.
        return 1;
    }
    let log2 = base
        .to_f64()
        .filter(|f| f.is_finite())
        .map(|f| f.abs().log2())
        .unwrap_or(base.bits() as f64);
    (log2 * exp as f64).ceil() as u64
}

use std::borrow::Cow;

pub fn to_bigint(v: &Value) -> Result<BigInt, String> {
    match v {
        Value::Int(i) => Ok(BigInt::from(*i)),
        Value::BigInt(i) => Ok((**i).clone()),
        Value::Float(f) => {
            BigInt::from_f64(f.trunc()).ok_or_else(|| "cannot convert float to integer".to_string())
        }
        _ => Err("expected integer".to_string()),
    }
}

#[inline]
pub fn to_bigint_cow<'a>(v: &'a Value) -> Result<Cow<'a, BigInt>, String> {
    match v {
        Value::Int(i) => Ok(Cow::Owned(BigInt::from(*i))),
        Value::BigInt(i) => Ok(Cow::Borrowed(&**i)),
        Value::Float(f) => BigInt::from_f64(f.trunc())
            .map(Cow::Owned)
            .ok_or_else(|| "cannot convert float to integer".to_string()),
        _ => Err("expected integer".to_string()),
    }
}

#[inline]
fn i64_bits(val: i64) -> u64 {
    if val == 0 {
        0
    } else {
        64 - val.unsigned_abs().leading_zeros() as u64
    }
}

#[inline]
pub fn bigint_rem_u64(bi: &BigInt, divisor: u64) -> u64 {
    let mut rem: u128 = 0;
    let div128 = divisor as u128;
    for digit in bi.iter_u64_digits().rev() {
        rem = ((rem << 64) | (digit as u128)) % div128;
    }
    rem as u64
}

#[inline]
pub fn bigint_mod_floor_i64(ia: &BigInt, ib: i64) -> i64 {
    if ib == -1 || ia.is_zero() {
        return 0;
    }
    let u_div = ib.unsigned_abs();
    let mag_rem = bigint_rem_u64(ia, u_div);
    if mag_rem == 0 {
        return 0;
    }
    if !ia.is_negative() {
        if ib > 0 {
            mag_rem as i64
        } else {
            (mag_rem as i128 + ib as i128) as i64
        }
    } else if ib > 0 {
        (ib as u64 - mag_rem) as i64
    } else {
        -(mag_rem as i64)
    }
}

#[inline]
pub fn to_f64(v: &Value) -> Result<f64, String> {
    match v {
        Value::Float(f) => Ok(*f),
        Value::Int(i) => Ok(*i as f64),
        Value::BigInt(i) => i
            .to_f64()
            .ok_or_else(|| "integer overflow in float conversion".to_string()),
        _ => Err("expected number".to_string()),
    }
}

#[inline]
pub fn eval_add(a: &Value, b: &Value) -> Result<Value, String> {
    match (a, b) {
        (Value::Int(ia), Value::Int(ib)) => {
            if let Some(r) = ia.checked_add(*ib) {
                Ok(Value::Int(r))
            } else {
                Ok(Value::from_i128(*ia as i128 + *ib as i128))
            }
        }
        (Value::BigInt(ia), Value::Int(ib)) => {
            check_int_bits(ia.bits().max(64) + 1)?;
            Ok(Value::from_bigint(&**ia + *ib))
        }
        (Value::Int(ia), Value::BigInt(ib)) => {
            check_int_bits(ib.bits().max(64) + 1)?;
            Ok(Value::from_bigint(*ia + &**ib))
        }
        (Value::BigInt(ia), Value::BigInt(ib)) => {
            check_int_bits(ia.bits().max(ib.bits()) + 1)?;
            if ia.is_negative() == ib.is_negative() {
                Ok(Value::BigInt(Rc::new(&**ia + &**ib)))
            } else {
                Ok(Value::from_bigint(&**ia + &**ib))
            }
        }
        _ => Ok(Value::Float(to_f64(a)? + to_f64(b)?)),
    }
}

#[inline]
pub fn eval_sub(a: &Value, b: &Value) -> Result<Value, String> {
    match (a, b) {
        (Value::Int(ia), Value::Int(ib)) => {
            if let Some(r) = ia.checked_sub(*ib) {
                Ok(Value::Int(r))
            } else {
                Ok(Value::from_i128(*ia as i128 - *ib as i128))
            }
        }
        (Value::BigInt(ia), Value::Int(ib)) => {
            check_int_bits(ia.bits().max(64) + 1)?;
            Ok(Value::from_bigint(&**ia - *ib))
        }
        (Value::Int(ia), Value::BigInt(ib)) => {
            check_int_bits(ib.bits().max(64) + 1)?;
            Ok(Value::from_bigint(*ia - &**ib))
        }
        (Value::BigInt(ia), Value::BigInt(ib)) => {
            check_int_bits(ia.bits().max(ib.bits()) + 1)?;
            if ia.is_negative() != ib.is_negative() {
                Ok(Value::BigInt(Rc::new(&**ia - &**ib)))
            } else {
                Ok(Value::from_bigint(&**ia - &**ib))
            }
        }
        _ => Ok(Value::Float(to_f64(a)? - to_f64(b)?)),
    }
}

#[inline]
pub fn eval_mul(a: &Value, b: &Value) -> Result<Value, String> {
    match (a, b) {
        (Value::Int(ia), Value::Int(ib)) => {
            if let Some(r) = ia.checked_mul(*ib) {
                Ok(Value::Int(r))
            } else {
                Ok(Value::from_i128(*ia as i128 * *ib as i128))
            }
        }
        (Value::BigInt(ia), Value::Int(ib)) => {
            check_int_bits(ia.bits() + i64_bits(*ib))?;
            if *ib == 0 {
                Ok(Value::Int(0))
            } else if *ib == 1 {
                Ok(Value::BigInt(ia.clone()))
            } else if *ib == -1 {
                Ok(Value::from_bigint(-&**ia))
            } else {
                Ok(Value::BigInt(Rc::new(&**ia * *ib)))
            }
        }
        (Value::Int(ia), Value::BigInt(ib)) => {
            check_int_bits(i64_bits(*ia) + ib.bits())?;
            if *ia == 0 {
                Ok(Value::Int(0))
            } else if *ia == 1 {
                Ok(Value::BigInt(ib.clone()))
            } else if *ia == -1 {
                Ok(Value::from_bigint(-&**ib))
            } else {
                Ok(Value::BigInt(Rc::new(*ia * &**ib)))
            }
        }
        (Value::BigInt(ia), Value::BigInt(ib)) => {
            check_int_bits(ia.bits() + ib.bits())?;
            Ok(Value::BigInt(Rc::new(&**ia * &**ib)))
        }
        _ => Ok(Value::Float(to_f64(a)? * to_f64(b)?)),
    }
}

#[inline]
pub fn eval_div(a: &Value, b: &Value) -> Result<Value, String> {
    // `/` is always floating point and follows IEEE 754, so dividing by zero
    // gives an infinity or NaN. Only `//` and `%` treat it as an error.
    Ok(Value::Float(to_f64(a)? / to_f64(b)?))
}

#[inline]
pub fn eval_idiv(a: &Value, b: &Value) -> Result<Value, String> {
    match (a, b) {
        (Value::Int(ia), Value::Int(ib)) => {
            if *ib == 0 {
                return Err("division by zero".to_string());
            }
            if *ia == i64::MIN && *ib == -1 {
                return Ok(Value::from_i128(*ia as i128 / *ib as i128));
            }
            Ok(Value::Int(ia.div_floor(ib)))
        }
        (Value::BigInt(ia), Value::Int(ib)) => {
            if *ib == 0 {
                return Err("division by zero".to_string());
            }
            if *ib == 1 {
                return Ok(Value::BigInt(ia.clone()));
            }
            if *ib == -1 {
                return Ok(Value::from_bigint(-&**ia));
            }
            if !ia.is_negative() && *ib > 0 {
                return Ok(Value::from_bigint(&**ia / *ib));
            }
            Ok(Value::from_bigint(ia.div_floor(&BigInt::from(*ib))))
        }
        (Value::Int(ia), Value::BigInt(ib)) => {
            if ib.is_zero() {
                return Err("division by zero".to_string());
            }
            if *ia == 0 || (*ia > 0) == (!ib.is_negative()) {
                Ok(Value::Int(0))
            } else {
                Ok(Value::Int(-1))
            }
        }
        (Value::BigInt(ia), Value::BigInt(ib)) => {
            if ib.is_zero() {
                return Err("division by zero".to_string());
            }
            if !ia.is_negative() && !ib.is_negative() {
                Ok(Value::from_bigint(&**ia / &**ib))
            } else {
                Ok(Value::from_bigint(ia.div_floor(&**ib)))
            }
        }
        _ => {
            let fa = to_f64(a)?;
            let fb = to_f64(b)?;
            if fb == 0.0 {
                return Err("division by zero".to_string());
            }
            Ok(Value::Float((fa / fb).floor()))
        }
    }
}

#[inline]
pub fn eval_mod(a: &Value, b: &Value) -> Result<Value, String> {
    match (a, b) {
        (Value::Int(ia), Value::Int(ib)) => {
            if *ib == 0 {
                return Err("modulo by zero".to_string());
            }
            if *ib == -1 {
                Ok(Value::Int(0))
            } else {
                Ok(Value::Int(ia.mod_floor(ib)))
            }
        }
        (Value::BigInt(ia), Value::Int(ib)) => {
            if *ib == 0 {
                return Err("modulo by zero".to_string());
            }
            Ok(Value::Int(bigint_mod_floor_i64(ia, *ib)))
        }
        (Value::Int(ia), Value::BigInt(ib)) => {
            if ib.is_zero() {
                return Err("modulo by zero".to_string());
            }
            if *ia == 0 {
                Ok(Value::Int(0))
            } else if (*ia > 0) == (!ib.is_negative()) {
                Ok(Value::Int(*ia))
            } else {
                Ok(Value::from_bigint(*ia + &**ib))
            }
        }
        (Value::BigInt(ia), Value::BigInt(ib)) => {
            if ib.is_zero() {
                return Err("modulo by zero".to_string());
            }
            if !ia.is_negative() && !ib.is_negative() {
                Ok(Value::from_bigint(&**ia % &**ib))
            } else {
                Ok(Value::from_bigint(ia.mod_floor(&**ib)))
            }
        }
        _ => {
            let fa = to_f64(a)?;
            let fb = to_f64(b)?;
            if fb == 0.0 {
                return Err("modulo by zero".to_string());
            }
            Ok(Value::Float(fa - (fa / fb).floor() * fb))
        }
    }
}

pub fn eval_pow(a: &Value, b: &Value) -> Result<Value, String> {
    if let (Value::Int(ia), Value::Int(ib)) = (a, b)
        && *ib >= 0
        && let Ok(exp) = u32::try_from(*ib)
    {
        return Ok(match ia.checked_pow(exp) {
            Some(r) => Value::Int(r),
            None => {
                if *ia == 2 {
                    check_int_bits(exp as u64 + 1)?;
                    Value::from_bigint(BigInt::from(1) << exp)
                } else if *ia == -2 {
                    check_int_bits(exp as u64 + 1)?;
                    let bi = BigInt::from(1) << exp;
                    Value::from_bigint(if exp & 1 == 0 { bi } else { -bi })
                } else {
                    let base = BigInt::from(*ia);
                    check_int_bits(pow_result_bits(&base, exp as u64))?;
                    Value::from_bigint(base.pow(exp))
                }
            }
        });
    }
    match (a, b) {
        (Value::BigInt(ia), Value::Int(ib)) if *ib >= 0 => {
            if ia.bits() <= 1 {
                return Ok(Value::Int(if ia.is_zero() {
                    if *ib == 0 { 1 } else { 0 }
                } else if ia.is_negative() && (ib & 1 != 0) {
                    -1
                } else {
                    1
                }));
            }
            let exp = *ib as u64;
            check_int_bits(pow_result_bits(ia, exp))?;
            let exp =
                u32::try_from(exp).map_err(|_| "integer exponent is too large".to_string())?;
            Ok(Value::from_bigint(ia.pow(exp)))
        }
        (Value::Int(ia), Value::BigInt(ib)) if ib.sign() != Sign::Minus => {
            if *ia == 0 || *ia == 1 || *ia == -1 {
                return Ok(Value::Int(if *ia == 0 {
                    if ib.is_zero() { 1 } else { 0 }
                } else if *ia == -1 && ib.is_odd() {
                    -1
                } else {
                    1
                }));
            }
            let base = BigInt::from(*ia);
            let exp = ib.to_u64().unwrap_or(u64::MAX);
            check_int_bits(pow_result_bits(&base, exp))?;
            let exp =
                u32::try_from(exp).map_err(|_| "integer exponent is too large".to_string())?;
            Ok(Value::from_bigint(base.pow(exp)))
        }
        (Value::BigInt(ia), Value::BigInt(ib)) if ib.sign() != Sign::Minus => {
            if ia.bits() <= 1 {
                return Ok(Value::Int(if ia.is_zero() {
                    if ib.is_zero() { 1 } else { 0 }
                } else if ia.is_negative() && ib.is_odd() {
                    -1
                } else {
                    1
                }));
            }
            let exp = ib.to_u64().unwrap_or(u64::MAX);
            check_int_bits(pow_result_bits(ia, exp))?;
            let exp =
                u32::try_from(exp).map_err(|_| "integer exponent is too large".to_string())?;
            Ok(Value::from_bigint(ia.pow(exp)))
        }

        _ => Ok(Value::Float(to_f64(a)?.powf(to_f64(b)?))),
    }
}

pub fn eval_bitand(a: &Value, b: &Value) -> Result<Value, String> {
    match (a, b) {
        (Value::Int(ia), Value::Int(ib)) => Ok(Value::Int(ia & ib)),
        (Value::BigInt(ia), Value::Int(ib)) => {
            if *ib >= 0 {
                Ok(Value::Int((low_word(a)? as i64) & *ib))
            } else {
                Ok(Value::from_bigint(&**ia & BigInt::from(*ib)))
            }
        }
        (Value::Int(ia), Value::BigInt(ib)) => {
            if *ia >= 0 {
                Ok(Value::Int(*ia & (low_word(b)? as i64)))
            } else {
                Ok(Value::from_bigint(BigInt::from(*ia) & &**ib))
            }
        }
        (Value::BigInt(ia), Value::BigInt(ib)) => Ok(Value::from_bigint(&**ia & &**ib)),
        _ => {
            let ca = to_bigint_cow(a)?;
            let cb = to_bigint_cow(b)?;
            Ok(Value::from_bigint(&*ca & &*cb))
        }
    }
}

pub fn eval_bitor(a: &Value, b: &Value) -> Result<Value, String> {
    match (a, b) {
        (Value::Int(ia), Value::Int(ib)) => Ok(Value::Int(ia | ib)),
        (Value::BigInt(ia), Value::Int(ib)) => Ok(Value::from_bigint(&**ia | BigInt::from(*ib))),
        (Value::Int(ia), Value::BigInt(ib)) => Ok(Value::from_bigint(BigInt::from(*ia) | &**ib)),
        (Value::BigInt(ia), Value::BigInt(ib)) => Ok(Value::from_bigint(&**ia | &**ib)),
        _ => {
            let ca = to_bigint_cow(a)?;
            let cb = to_bigint_cow(b)?;
            Ok(Value::from_bigint(&*ca | &*cb))
        }
    }
}

pub fn eval_bitxor(a: &Value, b: &Value) -> Result<Value, String> {
    match (a, b) {
        (Value::Int(ia), Value::Int(ib)) => Ok(Value::Int(ia ^ ib)),
        (Value::BigInt(ia), Value::Int(ib)) => Ok(Value::from_bigint(&**ia ^ BigInt::from(*ib))),
        (Value::Int(ia), Value::BigInt(ib)) => Ok(Value::from_bigint(BigInt::from(*ia) ^ &**ib)),
        (Value::BigInt(ia), Value::BigInt(ib)) => Ok(Value::from_bigint(&**ia ^ &**ib)),
        _ => {
            let ca = to_bigint_cow(a)?;
            let cb = to_bigint_cow(b)?;
            Ok(Value::from_bigint(&*ca ^ &*cb))
        }
    }
}

/// Arithmetic shift left that widens instead of dropping bits.
fn shift_left(ia: &Value, shift: usize) -> Result<Value, String> {
    match ia {
        Value::Int(i) => {
            if *i == 0 {
                return Ok(Value::Int(0));
            }
            // Shifting must not change the sign or lose a set bit: the number of
            // leading sign bits says how far it can go without widening.
            let headroom = if *i >= 0 {
                i.leading_zeros() - 1
            } else {
                i.leading_ones() - 1
            } as usize;
            if shift <= headroom {
                return Ok(Value::Int(i << shift));
            }
            let val128 = *i as i128;
            let headroom128 = if *i >= 0 {
                val128.leading_zeros() - 1
            } else {
                val128.leading_ones() - 1
            } as usize;
            if shift <= headroom128 && ((val128 << shift) >> shift) == val128 {
                return Ok(Value::from_i128(val128 << shift));
            }
            let bits = i64_bits(*i);
            check_int_bits(bits.saturating_add(shift as u64))?;
            Ok(Value::from_bigint(BigInt::from(*i) << shift))
        }
        Value::BigInt(b) => {
            if b.is_zero() {
                return Ok(Value::Int(0));
            }
            check_int_bits(b.bits().saturating_add(shift as u64))?;
            Ok(Value::BigInt(Rc::new(&**b << shift)))
        }
        _ => {
            let big = to_bigint_cow(ia)?;
            if big.is_zero() {
                return Ok(Value::Int(0));
            }
            check_int_bits(big.bits().saturating_add(shift as u64))?;
            Ok(Value::from_bigint(&*big << shift))
        }
    }
}

/// Arithmetic shift right (floors towards negative infinity, like BigInt).
fn shift_right(ia: &Value, shift: usize) -> Result<Value, String> {
    match ia {
        Value::Int(i) => Ok(Value::Int(if shift >= 64 {
            if *i < 0 { -1 } else { 0 }
        } else {
            i >> shift
        })),
        Value::BigInt(b) => {
            if shift as u64 >= b.bits() {
                Ok(Value::Int(if b.is_negative() { -1 } else { 0 }))
            } else {
                Ok(Value::from_bigint(&**b >> shift))
            }
        }
        _ => {
            let big = to_bigint_cow(ia)?;
            if shift as u64 >= big.bits() {
                Ok(Value::Int(if big.is_negative() { -1 } else { 0 }))
            } else {
                Ok(Value::from_bigint(&*big >> shift))
            }
        }
    }
}

fn shift_amount(b: &Value) -> Result<(bool, usize), String> {
    match b {
        Value::Int(i) => Ok((*i < 0, i.unsigned_abs() as usize)),
        Value::BigInt(_) => Err("shift is too large".to_string()),
        Value::Float(f) => {
            if !f.is_finite() {
                return Err("cannot convert float to integer".to_string());
            }
            let t = f.trunc();
            Ok((t < 0.0, t.abs() as usize))
        }
        _ => Err("expected integer".to_string()),
    }
}

pub fn eval_shl(a: &Value, b: &Value) -> Result<Value, String> {
    if !a.is_number() {
        return Err("expected integer".to_string());
    }
    let (negative, shift) = shift_amount(b)?;
    if negative {
        shift_right(a, shift)
    } else {
        shift_left(a, shift)
    }
}

pub fn eval_shr(a: &Value, b: &Value) -> Result<Value, String> {
    if !a.is_number() {
        return Err("expected integer".to_string());
    }
    let (negative, shift) = shift_amount(b)?;
    if negative {
        shift_left(a, shift)
    } else {
        shift_right(a, shift)
    }
}

/// The low 64 bits of an integer, in two's complement.
#[inline]
pub fn low_word(v: &Value) -> Result<u64, String> {
    match v {
        Value::Int(i) => Ok(*i as u64),
        Value::BigInt(b) => {
            let low = b.iter_u64_digits().next().unwrap_or(0);
            if b.is_negative() {
                Ok(low.wrapping_neg())
            } else {
                Ok(low)
            }
        }
        _ => {
            let big = to_bigint_cow(v)?;
            let low = big.iter_u64_digits().next().unwrap_or(0);
            if big.is_negative() {
                Ok(low.wrapping_neg())
            } else {
                Ok(low)
            }
        }
    }
}

pub fn eval_lshl(a: &Value, b: &Value) -> Result<Value, String> {
    let word = low_word(a)?;
    let bits = match b {
        Value::Int(i) => {
            if *i < 0 {
                usize::MAX
            } else {
                usize::try_from(*i).unwrap_or(usize::MAX)
            }
        }
        Value::BigInt(bi) => {
            if bi.is_negative() {
                usize::MAX
            } else {
                bi.to_usize().unwrap_or(usize::MAX)
            }
        }
        Value::Float(f) => {
            if !f.is_finite() {
                return Err("cannot convert float to integer".to_string());
            }
            let t = f.trunc();
            if t < 0.0 || t > usize::MAX as f64 {
                usize::MAX
            } else {
                t as usize
            }
        }
        _ => to_bigint(b)?.to_usize().unwrap_or(usize::MAX),
    };
    let res = if bits >= 64 { 0 } else { word << bits };
    Ok(Value::from_u64(res))
}

pub fn eval_lshr(a: &Value, b: &Value) -> Result<Value, String> {
    let word = low_word(a)?;
    let bits = match b {
        Value::Int(i) => {
            if *i < 0 {
                usize::MAX
            } else {
                usize::try_from(*i).unwrap_or(usize::MAX)
            }
        }
        Value::BigInt(bi) => {
            if bi.is_negative() {
                usize::MAX
            } else {
                bi.to_usize().unwrap_or(usize::MAX)
            }
        }
        Value::Float(f) => {
            if !f.is_finite() {
                return Err("cannot convert float to integer".to_string());
            }
            let t = f.trunc();
            if t < 0.0 || t > usize::MAX as f64 {
                usize::MAX
            } else {
                t as usize
            }
        }
        _ => to_bigint(b)?.to_usize().unwrap_or(usize::MAX),
    };
    let res = if bits >= 64 { 0 } else { word >> bits };
    Ok(Value::from_u64(res))
}

pub fn eval_bitnot(a: &Value) -> Result<Value, String> {
    match a {
        Value::Int(i) => Ok(Value::Int(!i)),
        Value::BigInt(b) => Ok(Value::from_bigint(!&**b)),
        _ => {
            let big = to_bigint_cow(a)?;
            Ok(Value::from_bigint(!&*big))
        }
    }
}

pub fn eval_unm(a: &Value) -> Result<Value, String> {
    match a {
        Value::Int(i) => Ok(match i.checked_neg() {
            Some(r) => Value::Int(r),
            None => Value::from_i128(-(*i as i128)),
        }),
        Value::BigInt(b) => Ok(Value::from_bigint(-&**b)),
        Value::Float(f) => Ok(Value::Float(-f)),
        _ => Err("unary minus expects a number".to_string()),
    }
}

/// Orders two numbers, or `None` when a NaN makes them unordered. An
/// out-of-range `BigInt` compares against an `Int` by its sign alone, so no
/// digits are touched on that path either.
#[inline]
fn num_cmp(a: &Value, b: &Value) -> Result<Option<std::cmp::Ordering>, String> {
    use std::cmp::Ordering;
    Ok(Some(match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Int(_), Value::BigInt(y)) => {
            if y.is_negative() {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Value::BigInt(x), Value::Int(_)) => {
            if x.is_negative() {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Value::BigInt(x), Value::BigInt(y)) => x.cmp(y),
        _ => return Ok(to_f64(a)?.partial_cmp(&to_f64(b)?)),
    }))
}

macro_rules! compare_op {
    ($name:ident, $op:tt, $($pat:pat_param)|+) => {
        #[inline]
        pub fn $name(a: &Value, b: &Value) -> Result<bool, String> {
            if let (Value::Int(ia), Value::Int(ib)) = (a, b) {
                return Ok(ia $op ib);
            }
            if let (Value::String(sa), Value::String(sb)) = (a, b) {
                return Ok(sa $op sb);
            }
            Ok(matches!(num_cmp(a, b)?, Some($($pat)|+)))
        }
    };
}

compare_op!(eval_lt, <, std::cmp::Ordering::Less);
compare_op!(eval_le, <=, std::cmp::Ordering::Less | std::cmp::Ordering::Equal);
compare_op!(eval_gt, >, std::cmp::Ordering::Greater);
compare_op!(eval_ge, >=, std::cmp::Ordering::Greater | std::cmp::Ordering::Equal);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ops_i128_overflow_and_demotion() {
        let max = Value::Int(i64::MAX);
        let one = Value::Int(1);
        let add = eval_add(&max, &one).unwrap();
        assert_eq!(add.typeof_name(), "bigint");
        let sub = eval_sub(&add, &one).unwrap();
        assert_eq!(sub, Value::Int(i64::MAX));

        let min = Value::Int(i64::MIN);
        let neg_one = Value::Int(-1);
        let idiv = eval_idiv(&min, &neg_one).unwrap();
        assert_eq!(idiv.typeof_name(), "bigint");

        let unm = eval_unm(&min).unwrap();
        assert_eq!(unm.typeof_name(), "bigint");
    }

    #[test]
    fn test_ops_bigint_borrowed_arithmetic() {
        let b = Value::from_bigint(BigInt::from(1) << 100);
        let two = Value::Int(2);
        let mul = eval_mul(&b, &two).unwrap();
        assert_eq!(mul, Value::from_bigint(BigInt::from(1) << 101));

        let div = eval_idiv(&mul, &two).unwrap();
        assert_eq!(div, b);

        let rem = eval_mod(&b, &Value::Int(1000)).unwrap();
        assert!(matches!(rem, Value::Int(_)));
    }

    #[test]
    fn test_bigint_mod_floor_and_div_equivalence() {
        let values = [
            BigInt::from(1i64) << 100,
            -&(BigInt::from(1i64) << 100u32),
            (BigInt::from(1i64) << 65) | BigInt::from(123456789i64),
            -&((BigInt::from(1i64) << 65u32) | BigInt::from(123456789i64)),
            BigInt::from(i64::MAX as u64) + 1i64,
            BigInt::from(i64::MIN) - 1i64,
        ];
        let divisors = [
            1i64,
            -1,
            2,
            -2,
            7,
            -7,
            1000000007,
            -1000000007,
            i64::MAX,
            i64::MIN,
        ];

        for bi in &values {
            for &div in &divisors {
                let fast_mod = bigint_mod_floor_i64(bi, div);
                let std_mod = bi.mod_floor(&BigInt::from(div)).to_i64().unwrap();
                assert_eq!(fast_mod, std_mod, "mismatch for {bi} % {div}");

                let v_bi = Value::BigInt(Rc::new(bi.clone()));
                let v_div = Value::Int(div);
                let eval_m = eval_mod(&v_bi, &v_div).unwrap();
                assert_eq!(eval_m, Value::Int(std_mod));

                let eval_d = eval_idiv(&v_bi, &v_div).unwrap();
                let std_d = Value::from_bigint(bi.div_floor(&BigInt::from(div)));
                assert_eq!(eval_d, std_d, "idiv mismatch for {bi} // {div}");
            }
        }
    }

    #[test]
    fn test_low_word_equivalence() {
        let test_cases = [
            (Value::Int(0), 0u64),
            (Value::Int(-1), u64::MAX),
            (Value::Int(-8), (-8i64) as u64),
            (Value::Int(12345), 12345u64),
            (
                Value::from_bigint((BigInt::from(1i64) << 70) | BigInt::from(5i64)),
                5u64,
            ),
            (
                Value::from_bigint(-&((BigInt::from(1i64) << 70u32) | BigInt::from(5i64))),
                (-5i64) as u64,
            ),
        ];

        for (val, expected) in test_cases {
            assert_eq!(low_word(&val).unwrap(), expected);
        }
    }
}
