//! Arithmetic, bitwise and comparison operators.
//!
//! Every operator has a fast inline integer / float path, and falls back to
//! `BigInt` when an operand is already big or the `i64` result overflows.
//! Results always go through `Value::from_bigint`, which demotes back to `Int`
//! whenever the answer fits.

use num_bigint::{BigInt, BigUint, Sign};
use num_integer::Integer as _;
use num_traits::{FromPrimitive, Signed, ToPrimitive, Zero};
use std::borrow::Cow;

use crate::vm::value::{Value, ValueView};

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

pub fn to_bigint(v: &Value) -> Result<BigInt, String> {
    match v.view() {
        ValueView::Int(i) => Ok(BigInt::from(i)),
        ValueView::BigInt(i) => Ok(i.clone()),
        ValueView::Float(f) => {
            BigInt::from_f64(f.trunc()).ok_or_else(|| "cannot convert float to integer".to_string())
        }
        _ => Err("expected integer".to_string()),
    }
}

#[inline]
pub fn to_bigint_cow<'a>(v: &'a Value) -> Result<Cow<'a, BigInt>, String> {
    match v.view() {
        ValueView::Int(i) => Ok(Cow::Owned(BigInt::from(i))),
        ValueView::BigInt(i) => Ok(Cow::Borrowed(i)),
        ValueView::Float(f) => BigInt::from_f64(f.trunc())
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
    if let Some(u) = bi.magnitude().to_u128() {
        return (u % (divisor as u128)) as u64;
    }
    let mut rem: u128 = 0;
    let div128 = divisor as u128;
    for digit in bi.iter_u64_digits().rev() {
        rem = ((rem << 64) | (digit as u128)) % div128;
    }
    rem as u64
}

/// Non-negative `BigInt` divided by a positive `u64` using hardware 128-bit
/// limb division, avoiding `num_bigint`'s intermediate remainder allocation.
#[inline]
fn bigint_div_u64(ia: &BigInt, divisor: u64) -> Value {
    if let Some(u) = ia.to_u128() {
        return Value::from_u128(u / divisor as u128);
    }
    let div128 = divisor as u128;
    let mut rem: u128 = 0;
    let u64_len = ia.iter_u64_digits().len();
    let mut q_u32 = vec![0u32; u64_len * 2];
    for (i, digit) in ia.iter_u64_digits().enumerate().rev() {
        let cur = (rem << 64) | (digit as u128);
        let q = (cur / div128) as u64;
        rem = cur % div128;
        q_u32[i * 2] = q as u32;
        q_u32[i * 2 + 1] = (q >> 32) as u32;
    }
    Value::from_bigint(BigInt::from_biguint(Sign::Plus, BigUint::new(q_u32)))
}

/// Mutates `stack[slot]` in place when it holds a uniquely-owned `Rc<BigInt>`
/// (`strong_count == 1`), then demotes back to `Int` if it fits in `i64`.
#[inline]
fn mutate_unique_bigint(stack: &mut [Value], slot: usize, f: impl FnOnce(&mut BigInt)) -> bool {
    stack[slot].with_unique_bigint_mut(f)
}

/// Mutates `stack[dst]` in place against `stack[other]` (`dst != other`) when
/// `stack[dst]` holds a uniquely-owned `Rc<BigInt>`, then demotes back to
/// `Int` if it fits in `i64`.
#[inline]
fn mutate_unique_bigint_with(
    stack: &mut [Value],
    dst: usize,
    other: usize,
    f: impl FnOnce(&mut BigInt, &Value),
) -> bool {
    debug_assert_ne!(dst, other);
    if !stack[dst].is_bigint() {
        return false;
    }
    let (dst_slot, other_slot): (&mut Value, &Value) = if dst < other {
        let (left, right) = stack.split_at_mut(other);
        (&mut left[dst], &right[0])
    } else {
        let (left, right) = stack.split_at_mut(dst);
        (&mut right[0], &left[other])
    };
    dst_slot.with_unique_bigint_mut(|big| f(big, other_slot))
}

/// Evaluates `stack[dst] = stack[a] + stack[b]` in one pass (no redundant type
/// or bit-width checks), mutating in place when `dst` aliases a uniquely-owned
/// `BigInt` operand.
#[inline]
pub fn eval_add_into(stack: &mut [Value], dst: usize, a: usize, b: usize) -> Result<(), String> {
    if let (Some(ia), Some(ib)) = (stack[a].as_int(), stack[b].as_int()) {
        if let Some(sum) = ia.checked_add(ib) {
            if !stack[dst].try_set_unique_heap_int(sum) {
                stack[dst] = Value::int(sum);
            }
        } else {
            stack[dst] = Value::from_i128(ia as i128 + ib as i128);
        }
        return Ok(());
    }
    let bits_needed = match (stack[a].view(), stack[b].view()) {
        (ValueView::BigInt(ia), ValueView::BigInt(ib)) => Some(ia.bits().max(ib.bits()) + 1),
        (ValueView::BigInt(ia), ValueView::Int(_)) => Some(ia.bits().max(64) + 1),
        (ValueView::Int(_), ValueView::BigInt(ib)) => Some(ib.bits().max(64) + 1),
        _ => None,
    };
    if let Some(bits) = bits_needed {
        check_int_bits(bits)?;
        if dst == a && dst == b {
            if mutate_unique_bigint(stack, dst, |big| *big <<= 1) {
                return Ok(());
            }
        } else if dst == a || dst == b {
            let other = if dst == a { b } else { a };
            if mutate_unique_bigint_with(stack, dst, other, |big, rhs| match rhs.view() {
                ValueView::BigInt(rb) => *big += rb,
                ValueView::Int(ib) => *big += ib,
                _ => unreachable!("checked above"),
            }) {
                return Ok(());
            }
        }
        let res = match (stack[a].view(), stack[b].view()) {
            (ValueView::BigInt(ia), ValueView::Int(ib)) => Value::from_bigint(ia + ib),
            (ValueView::Int(ia), ValueView::BigInt(ib)) => Value::from_bigint(ia + ib),
            (ValueView::BigInt(ia), ValueView::BigInt(ib)) => Value::from_bigint(ia + ib),
            _ => unreachable!("checked above"),
        };
        stack[dst] = res;
        return Ok(());
    }
    stack[dst] = eval_add(&stack[a], &stack[b])?;
    Ok(())
}

/// Evaluates `stack[dst] = stack[a] - stack[b]` in one pass, mutating in place
/// when `dst == a` holds a uniquely-owned `BigInt`.
#[inline]
pub fn eval_sub_into(stack: &mut [Value], dst: usize, a: usize, b: usize) -> Result<(), String> {
    if let (Some(ia), Some(ib)) = (stack[a].as_int(), stack[b].as_int()) {
        if let Some(diff) = ia.checked_sub(ib) {
            if !stack[dst].try_set_unique_heap_int(diff) {
                stack[dst] = Value::int(diff);
            }
        } else {
            stack[dst] = Value::from_i128(ia as i128 - ib as i128);
        }
        return Ok(());
    }
    let bits_needed = match (stack[a].view(), stack[b].view()) {
        (ValueView::BigInt(ia), ValueView::BigInt(ib)) => Some(ia.bits().max(ib.bits()) + 1),
        (ValueView::BigInt(ia), ValueView::Int(_)) => Some(ia.bits().max(64) + 1),
        (ValueView::Int(_), ValueView::BigInt(ib)) => Some(ib.bits().max(64) + 1),
        _ => None,
    };
    if let Some(bits) = bits_needed {
        check_int_bits(bits)?;
        if dst == a && dst == b {
            stack[dst] = Value::small_int(0);
            return Ok(());
        }
        if dst == a
            && mutate_unique_bigint_with(stack, dst, b, |big, rhs| match rhs.view() {
                ValueView::BigInt(rb) => *big -= rb,
                ValueView::Int(ib) => *big -= ib,
                _ => unreachable!("checked above"),
            })
        {
            return Ok(());
        }
        let res = match (stack[a].view(), stack[b].view()) {
            (ValueView::BigInt(ia), ValueView::Int(ib)) => Value::from_bigint(ia - ib),
            (ValueView::Int(ia), ValueView::BigInt(ib)) => Value::from_bigint(ia - ib),
            (ValueView::BigInt(ia), ValueView::BigInt(ib)) => Value::from_bigint(ia - ib),
            _ => unreachable!("checked above"),
        };
        stack[dst] = res;
        return Ok(());
    }
    stack[dst] = eval_sub(&stack[a], &stack[b])?;
    Ok(())
}

/// Shared single-pass evaluator for commutative bitwise operators (`&`, `|`,
/// `^`) on `BigInt` operands, reusing a uniquely-owned destination operand.
#[inline]
fn eval_bitop_into(
    stack: &mut [Value],
    dst: usize,
    a: usize,
    b: usize,
    assign_op: fn(&mut BigInt, &BigInt),
    fallback: fn(&Value, &Value) -> Result<Value, String>,
) -> Result<(), String> {
    if (dst == a || dst == b)
        && ((stack[a].is_bigint() && (stack[b].is_bigint() || stack[b].is_int()))
            || (stack[a].is_int() && stack[b].is_bigint()))
    {
        if dst == a && dst == b {
            // `x & x == x`, `x | x == x`, `x ^ x == 0` handled cleanly by fallback.
        } else {
            let other = if dst == a { b } else { a };
            if mutate_unique_bigint_with(stack, dst, other, |big, rhs| match rhs.view() {
                ValueView::BigInt(rb) => assign_op(big, rb),
                ValueView::Int(ib) => assign_op(big, &BigInt::from(ib)),
                _ => unreachable!("checked above"),
            }) {
                return Ok(());
            }
        }
    }
    stack[dst] = fallback(&stack[a], &stack[b])?;
    Ok(())
}

#[inline]
pub fn eval_bitand_into(stack: &mut [Value], dst: usize, a: usize, b: usize) -> Result<(), String> {
    eval_bitop_into(stack, dst, a, b, |x, y| *x &= y, eval_bitand)
}

#[inline]
pub fn eval_bitor_into(stack: &mut [Value], dst: usize, a: usize, b: usize) -> Result<(), String> {
    eval_bitop_into(stack, dst, a, b, |x, y| *x |= y, eval_bitor)
}

#[inline]
pub fn eval_bitxor_into(stack: &mut [Value], dst: usize, a: usize, b: usize) -> Result<(), String> {
    eval_bitop_into(stack, dst, a, b, |x, y| *x ^= y, eval_bitxor)
}

/// Evaluates `stack[dst] = -stack[src]`, flipping the sign of a uniquely-owned
/// `BigInt` in place without allocating when `dst == src`.
#[inline]
pub fn eval_unm_into(stack: &mut [Value], dst: usize, src: usize) -> Result<(), String> {
    if dst == src && mutate_unique_bigint(stack, dst, |big| *big = -std::mem::take(big)) {
        return Ok(());
    }
    stack[dst] = eval_unm(&stack[src])?;
    Ok(())
}

/// Evaluates `stack[dst] = stack[a] << shift` for a non-negative `u64` shift
/// on a `BigInt`, mutating in place when `dst == a` is uniquely owned.
#[inline]
pub fn eval_shl_bigint_into(
    stack: &mut [Value],
    dst: usize,
    a: usize,
    shift: u64,
) -> Result<(), String> {
    let Some(src) = stack[a].as_bigint() else {
        unreachable!("caller matched BigInt");
    };
    if src.is_zero() {
        stack[dst] = Value::small_int(0);
        return Ok(());
    }
    // Check total bit width in `u64` before casting `shift` to `usize` so
    // shifts >= 2^32 never truncate on 32-bit targets.
    check_int_bits(src.bits().saturating_add(shift))?;
    let shift_usize = shift as usize;
    if dst == a && mutate_unique_bigint(stack, dst, |big| *big <<= shift_usize) {
        return Ok(());
    }
    let Some(src) = stack[a].as_bigint() else {
        unreachable!("caller matched BigInt");
    };
    stack[dst] = Value::from_bigint(src << shift_usize);
    Ok(())
}

/// Fast arithmetic right shift on a `BigInt` for a `u64` shift count.
#[inline]
pub fn bigint_shr_u64(b: &BigInt, shift: u64) -> Value {
    if shift >= b.bits() {
        return Value::small_int(if b.is_negative() { -1 } else { 0 });
    }
    let shift_usize = shift as usize;
    if b.bits() <= 128 {
        if !b.is_negative() {
            if let Some(u) = b.to_u128() {
                return Value::from_u128(u >> shift_usize);
            }
        } else if let Some(m) = b.magnitude().to_u128() {
            // Floor arithmetic shift for negative `-m` (m >= 1):
            // `(-m) >> shift == -(((m - 1) >> shift) + 1)`.
            let mag = ((m - 1) >> shift_usize) + 1;
            const I64_MIN_MAG: u128 = 1u128 << 63;
            if mag < I64_MIN_MAG {
                return Value::int(-(mag as i64));
            } else if mag == I64_MIN_MAG {
                return Value::int(i64::MIN);
            } else {
                return Value::from_bigint(BigInt::from_biguint(Sign::Minus, BigUint::from(mag)));
            }
        }
    }
    Value::from_bigint(b >> shift_usize)
}

/// Evaluates `stack[dst] = stack[a] >> shift` for a non-negative `u64` shift
/// on a `BigInt` in one pass, mutating in place when `dst == a` is uniquely
/// owned.
#[inline]
pub fn eval_shr_bigint_into(stack: &mut [Value], dst: usize, a: usize, shift: u64) {
    let Some(src) = stack[a].as_bigint() else {
        unreachable!("caller matched BigInt");
    };
    if shift == 0 {
        if dst != a {
            stack[dst] = stack[a].clone();
        }
        return;
    }
    if shift >= src.bits() {
        stack[dst] = Value::small_int(if src.is_negative() { -1 } else { 0 });
        return;
    }
    if src.bits() <= 128 && src.bits() - shift <= 63 {
        stack[dst] = bigint_shr_u64(src, shift);
        return;
    }
    let shift_usize = shift as usize;
    if dst == a && mutate_unique_bigint(stack, dst, |big| *big >>= shift_usize) {
        return;
    }
    let Some(src) = stack[a].as_bigint() else {
        unreachable!("caller matched BigInt");
    };
    stack[dst] = bigint_shr_u64(src, shift);
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
    if v.is_float() {
        return Ok(v.as_float_unchecked());
    }
    if v.is_inline_int() {
        return Ok(v.as_inline_int_unchecked() as f64);
    }
    match v.view() {
        ValueView::Int(i) => Ok(i as f64),
        ValueView::BigInt(i) => i
            .to_f64()
            .ok_or_else(|| "integer overflow in float conversion".to_string()),
        _ => Err("expected number".to_string()),
    }
}

#[inline]
pub fn eval_add(a: &Value, b: &Value) -> Result<Value, String> {
    if a.is_inline_int() && b.is_inline_int() {
        let ia = a.as_inline_int_unchecked();
        let ib = b.as_inline_int_unchecked();
        return Ok(Value::int(ia + ib));
    }
    if a.is_float() && b.is_float() {
        return Ok(Value::float(
            a.as_float_unchecked() + b.as_float_unchecked(),
        ));
    }
    match (a.view(), b.view()) {
        (ValueView::Int(ia), ValueView::Int(ib)) => match ia.checked_add(ib) {
            Some(r) => Ok(Value::int(r)),
            None => Ok(Value::from_i128(ia as i128 + ib as i128)),
        },
        (ValueView::BigInt(ia), ValueView::Int(ib)) => {
            check_int_bits(ia.bits().max(64) + 1)?;
            Ok(Value::from_bigint(ia + ib))
        }
        (ValueView::Int(ia), ValueView::BigInt(ib)) => {
            check_int_bits(ib.bits().max(64) + 1)?;
            Ok(Value::from_bigint(ia + ib))
        }
        (ValueView::BigInt(ia), ValueView::BigInt(ib)) => {
            check_int_bits(ia.bits().max(ib.bits()) + 1)?;
            Ok(Value::from_bigint(ia + ib))
        }
        _ => Ok(Value::float(to_f64(a)? + to_f64(b)?)),
    }
}

#[inline]
pub fn eval_sub(a: &Value, b: &Value) -> Result<Value, String> {
    if a.is_inline_int() && b.is_inline_int() {
        let ia = a.as_inline_int_unchecked();
        let ib = b.as_inline_int_unchecked();
        return Ok(Value::int(ia - ib));
    }
    if a.is_float() && b.is_float() {
        return Ok(Value::float(
            a.as_float_unchecked() - b.as_float_unchecked(),
        ));
    }
    match (a.view(), b.view()) {
        (ValueView::Int(ia), ValueView::Int(ib)) => match ia.checked_sub(ib) {
            Some(r) => Ok(Value::int(r)),
            None => Ok(Value::from_i128(ia as i128 - ib as i128)),
        },
        (ValueView::BigInt(ia), ValueView::Int(ib)) => {
            check_int_bits(ia.bits().max(64) + 1)?;
            Ok(Value::from_bigint(ia - ib))
        }
        (ValueView::Int(ia), ValueView::BigInt(ib)) => {
            check_int_bits(ib.bits().max(64) + 1)?;
            Ok(Value::from_bigint(ia - ib))
        }
        (ValueView::BigInt(ia), ValueView::BigInt(ib)) => {
            check_int_bits(ia.bits().max(ib.bits()) + 1)?;
            Ok(Value::from_bigint(ia - ib))
        }
        _ => Ok(Value::float(to_f64(a)? - to_f64(b)?)),
    }
}

#[inline]
pub fn eval_mul(a: &Value, b: &Value) -> Result<Value, String> {
    if a.is_float() && b.is_float() {
        return Ok(Value::float(
            a.as_float_unchecked() * b.as_float_unchecked(),
        ));
    }
    match (a.view(), b.view()) {
        (ValueView::Int(ia), ValueView::Int(ib)) => match ia.checked_mul(ib) {
            Some(r) => Ok(Value::int(r)),
            None => Ok(Value::from_i128(ia as i128 * ib as i128)),
        },
        (ValueView::BigInt(ia), ValueView::Int(ib)) => {
            check_int_bits(ia.bits() + i64_bits(ib))?;
            if ib == 0 {
                Ok(Value::small_int(0))
            } else if ib == 1 {
                Ok(a.clone())
            } else if ib == -1 {
                Ok(Value::from_bigint(-ia))
            } else {
                Ok(Value::from_bigint(ia * ib))
            }
        }
        (ValueView::Int(ia), ValueView::BigInt(ib)) => {
            check_int_bits(i64_bits(ia) + ib.bits())?;
            if ia == 0 {
                Ok(Value::small_int(0))
            } else if ia == 1 {
                Ok(b.clone())
            } else if ia == -1 {
                Ok(Value::from_bigint(-ib))
            } else {
                Ok(Value::from_bigint(ia * ib))
            }
        }
        (ValueView::BigInt(ia), ValueView::BigInt(ib)) => {
            check_int_bits(ia.bits() + ib.bits())?;
            Ok(Value::from_bigint(ia * ib))
        }
        _ => Ok(Value::float(to_f64(a)? * to_f64(b)?)),
    }
}

#[inline]
pub fn eval_div(a: &Value, b: &Value) -> Result<Value, String> {
    // `/` is always floating point and follows IEEE 754, so dividing by zero
    // gives an infinity or NaN. Only `//` and `%` treat it as an error.
    Ok(Value::float(to_f64(a)? / to_f64(b)?))
}

#[inline]
pub fn eval_idiv(a: &Value, b: &Value) -> Result<Value, String> {
    if a.is_inline_int() && b.is_inline_int() {
        let ia = a.as_inline_int_unchecked();
        let ib = b.as_inline_int_unchecked();
        if ib == 0 {
            return Err("division by zero".to_string());
        }
        return Ok(Value::int(num_integer::Integer::div_floor(&ia, &ib)));
    }
    match (a.view(), b.view()) {
        (ValueView::Int(ia), ValueView::Int(ib)) => {
            if ib == 0 {
                return Err("division by zero".to_string());
            }
            if ia == i64::MIN && ib == -1 {
                return Ok(Value::from_i128(ia as i128 / ib as i128));
            }
            Ok(Value::int(num_integer::Integer::div_floor(&ia, &ib)))
        }
        (ValueView::BigInt(ia), ValueView::Int(ib)) => {
            if ib == 0 {
                return Err("division by zero".to_string());
            }
            if ib == 1 {
                return Ok(a.clone());
            }
            if ib == -1 {
                return Ok(Value::from_bigint(-ia));
            }
            if !ia.is_negative() && ib > 0 {
                return Ok(bigint_div_u64(ia, ib as u64));
            }
            Ok(Value::from_bigint(ia.div_floor(&BigInt::from(ib))))
        }
        (ValueView::Int(ia), ValueView::BigInt(ib)) => {
            if ib.is_zero() {
                return Err("division by zero".to_string());
            }
            if ia == 0 || (ia > 0) == (!ib.is_negative()) {
                Ok(Value::small_int(0))
            } else {
                Ok(Value::small_int(-1))
            }
        }
        (ValueView::BigInt(ia), ValueView::BigInt(ib)) => {
            if ib.is_zero() {
                return Err("division by zero".to_string());
            }
            if !ia.is_negative() && !ib.is_negative() {
                if let (Some(ua), Some(ub)) = (ia.to_u128(), ib.to_u128()) {
                    return Ok(Value::from_u128(ua / ub));
                }
                if let Some(ub) = ib.to_u64() {
                    return Ok(bigint_div_u64(ia, ub));
                }
                Ok(Value::from_bigint(ia / ib))
            } else {
                Ok(Value::from_bigint(ia.div_floor(ib)))
            }
        }
        _ => {
            let fa = to_f64(a)?;
            let fb = to_f64(b)?;
            if fb == 0.0 {
                return Err("division by zero".to_string());
            }
            Ok(Value::float((fa / fb).floor()))
        }
    }
}

#[inline]
pub fn eval_mod(a: &Value, b: &Value) -> Result<Value, String> {
    if a.is_inline_int() && b.is_inline_int() {
        let ia = a.as_inline_int_unchecked();
        let ib = b.as_inline_int_unchecked();
        if ib == 0 {
            return Err("modulo by zero".to_string());
        }
        return Ok(Value::int(ia.mod_floor(&ib)));
    }
    match (a.view(), b.view()) {
        (ValueView::Int(ia), ValueView::Int(ib)) => {
            if ib == 0 {
                return Err("modulo by zero".to_string());
            }
            if ib == -1 {
                Ok(Value::small_int(0))
            } else {
                Ok(Value::int(ia.mod_floor(&ib)))
            }
        }
        (ValueView::BigInt(ia), ValueView::Int(ib)) => {
            if ib == 0 {
                return Err("modulo by zero".to_string());
            }
            Ok(Value::int(bigint_mod_floor_i64(ia, ib)))
        }
        (ValueView::Int(ia), ValueView::BigInt(ib)) => {
            if ib.is_zero() {
                return Err("modulo by zero".to_string());
            }
            if ia == 0 {
                Ok(Value::small_int(0))
            } else if (ia > 0) == (!ib.is_negative()) {
                Ok(Value::int(ia))
            } else {
                Ok(Value::from_bigint(ia + ib))
            }
        }
        (ValueView::BigInt(ia), ValueView::BigInt(ib)) => {
            if ib.is_zero() {
                return Err("modulo by zero".to_string());
            }
            if !ia.is_negative() && !ib.is_negative() {
                Ok(Value::from_bigint(ia % ib))
            } else {
                Ok(Value::from_bigint(ia.mod_floor(ib)))
            }
        }
        _ => {
            let fa = to_f64(a)?;
            let fb = to_f64(b)?;
            if fb == 0.0 {
                return Err("modulo by zero".to_string());
            }
            Ok(Value::float(fa - (fa / fb).floor() * fb))
        }
    }
}

pub fn eval_pow(a: &Value, b: &Value) -> Result<Value, String> {
    if let (ValueView::Int(ia), ValueView::Int(ib)) = (a.view(), b.view())
        && ib >= 0
        && let Ok(exp) = u32::try_from(ib)
    {
        return Ok(match ia.checked_pow(exp) {
            Some(r) => Value::int(r),
            None => {
                if ia == 2 {
                    check_int_bits(exp as u64 + 1)?;
                    Value::from_bigint(BigInt::from(1) << exp)
                } else if ia == -2 {
                    check_int_bits(exp as u64 + 1)?;
                    let bi = BigInt::from(1) << exp;
                    Value::from_bigint(if exp & 1 == 0 { bi } else { -bi })
                } else {
                    let base = BigInt::from(ia);
                    check_int_bits(pow_result_bits(&base, exp as u64))?;
                    Value::from_bigint(base.pow(exp))
                }
            }
        });
    }
    match (a.view(), b.view()) {
        (ValueView::BigInt(ia), ValueView::Int(ib)) if ib >= 0 => {
            if ia.bits() <= 1 {
                return Ok(Value::small_int(if ia.is_zero() {
                    if ib == 0 { 1 } else { 0 }
                } else if ia.is_negative() && (ib & 1 != 0) {
                    -1
                } else {
                    1
                }));
            }
            let exp = ib as u64;
            check_int_bits(pow_result_bits(ia, exp))?;
            let exp =
                u32::try_from(exp).map_err(|_| "integer exponent is too large".to_string())?;
            Ok(Value::from_bigint(ia.pow(exp)))
        }
        (ValueView::Int(ia), ValueView::BigInt(ib)) if ib.sign() != Sign::Minus => {
            if ia == 0 || ia == 1 || ia == -1 {
                return Ok(Value::small_int(if ia == 0 {
                    if ib.is_zero() { 1 } else { 0 }
                } else if ia == -1 && ib.is_odd() {
                    -1
                } else {
                    1
                }));
            }
            let base = BigInt::from(ia);
            let exp = ib.to_u64().unwrap_or(u64::MAX);
            check_int_bits(pow_result_bits(&base, exp))?;
            let exp =
                u32::try_from(exp).map_err(|_| "integer exponent is too large".to_string())?;
            Ok(Value::from_bigint(base.pow(exp)))
        }
        (ValueView::BigInt(ia), ValueView::BigInt(ib)) if ib.sign() != Sign::Minus => {
            if ia.bits() <= 1 {
                return Ok(Value::small_int(if ia.is_zero() {
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

        _ => Ok(Value::float(to_f64(a)?.powf(to_f64(b)?))),
    }
}

pub fn eval_bitand(a: &Value, b: &Value) -> Result<Value, String> {
    if a.is_inline_int() && b.is_inline_int() {
        return Ok(Value::int(
            a.as_inline_int_unchecked() & b.as_inline_int_unchecked(),
        ));
    }
    match (a.view(), b.view()) {
        (ValueView::Int(ia), ValueView::Int(ib)) => Ok(Value::int(ia & ib)),
        (ValueView::BigInt(ia), ValueView::Int(ib)) => {
            if ib >= 0 {
                Ok(Value::int((low_word(a)? as i64) & ib))
            } else {
                Ok(Value::from_bigint(ia & BigInt::from(ib)))
            }
        }
        (ValueView::Int(ia), ValueView::BigInt(ib)) => {
            if ia >= 0 {
                Ok(Value::int(ia & (low_word(b)? as i64)))
            } else {
                Ok(Value::from_bigint(BigInt::from(ia) & ib))
            }
        }
        (ValueView::BigInt(ia), ValueView::BigInt(ib)) => Ok(Value::from_bigint(ia & ib)),
        _ => {
            let ca = to_bigint_cow(a)?;
            let cb = to_bigint_cow(b)?;
            Ok(Value::from_bigint(&*ca & &*cb))
        }
    }
}

pub fn eval_bitor(a: &Value, b: &Value) -> Result<Value, String> {
    if a.is_inline_int() && b.is_inline_int() {
        return Ok(Value::int(
            a.as_inline_int_unchecked() | b.as_inline_int_unchecked(),
        ));
    }
    match (a.view(), b.view()) {
        (ValueView::Int(ia), ValueView::Int(ib)) => Ok(Value::int(ia | ib)),
        (ValueView::BigInt(ia), ValueView::Int(ib)) => {
            Ok(Value::from_bigint(ia | BigInt::from(ib)))
        }
        (ValueView::Int(ia), ValueView::BigInt(ib)) => {
            Ok(Value::from_bigint(BigInt::from(ia) | ib))
        }
        (ValueView::BigInt(ia), ValueView::BigInt(ib)) => Ok(Value::from_bigint(ia | ib)),
        _ => {
            let ca = to_bigint_cow(a)?;
            let cb = to_bigint_cow(b)?;
            Ok(Value::from_bigint(&*ca | &*cb))
        }
    }
}

pub fn eval_bitxor(a: &Value, b: &Value) -> Result<Value, String> {
    if a.is_inline_int() && b.is_inline_int() {
        return Ok(Value::int(
            a.as_inline_int_unchecked() ^ b.as_inline_int_unchecked(),
        ));
    }
    match (a.view(), b.view()) {
        (ValueView::Int(ia), ValueView::Int(ib)) => Ok(Value::int(ia ^ ib)),
        (ValueView::BigInt(ia), ValueView::Int(ib)) => {
            Ok(Value::from_bigint(ia ^ BigInt::from(ib)))
        }
        (ValueView::Int(ia), ValueView::BigInt(ib)) => {
            Ok(Value::from_bigint(BigInt::from(ia) ^ ib))
        }
        (ValueView::BigInt(ia), ValueView::BigInt(ib)) => Ok(Value::from_bigint(ia ^ ib)),
        _ => {
            let ca = to_bigint_cow(a)?;
            let cb = to_bigint_cow(b)?;
            Ok(Value::from_bigint(&*ca ^ &*cb))
        }
    }
}

/// Arithmetic shift left that widens instead of dropping bits.
fn shift_left(ia: &Value, shift: usize) -> Result<Value, String> {
    match ia.view() {
        ValueView::Int(i) => {
            if i == 0 {
                return Ok(Value::small_int(0));
            }
            let headroom = if i >= 0 {
                i.leading_zeros() - 1
            } else {
                i.leading_ones() - 1
            } as usize;
            if shift <= headroom {
                return Ok(Value::int(i << shift));
            }
            let val128 = i as i128;
            let headroom128 = if i >= 0 {
                val128.leading_zeros() - 1
            } else {
                val128.leading_ones() - 1
            } as usize;
            if shift <= headroom128 && ((val128 << shift) >> shift) == val128 {
                return Ok(Value::from_i128(val128 << shift));
            }
            let bits = i64_bits(i);
            check_int_bits(bits.saturating_add(shift as u64))?;
            Ok(Value::from_bigint(BigInt::from(i) << shift))
        }
        ValueView::BigInt(b) => {
            if b.is_zero() {
                return Ok(Value::small_int(0));
            }
            check_int_bits(b.bits().saturating_add(shift as u64))?;
            Ok(Value::from_bigint(b << shift))
        }
        _ => {
            let big = to_bigint_cow(ia)?;
            if big.is_zero() {
                return Ok(Value::small_int(0));
            }
            check_int_bits(big.bits().saturating_add(shift as u64))?;
            Ok(Value::from_bigint(&*big << shift))
        }
    }
}

/// Arithmetic shift right (floors towards negative infinity, like BigInt).
fn shift_right(ia: &Value, shift: usize) -> Result<Value, String> {
    match ia.view() {
        ValueView::Int(i) => Ok(Value::int(if shift >= 64 {
            if i < 0 { -1 } else { 0 }
        } else {
            i >> shift
        })),
        ValueView::BigInt(b) => Ok(bigint_shr_u64(b, shift as u64)),
        _ => {
            let big = to_bigint_cow(ia)?;
            if shift as u64 >= big.bits() {
                Ok(Value::small_int(if big.is_negative() { -1 } else { 0 }))
            } else {
                Ok(Value::from_bigint(&*big >> shift))
            }
        }
    }
}

fn shift_amount(b: &Value) -> Result<(bool, usize), String> {
    match b.view() {
        ValueView::Int(i) => Ok((i < 0, i.unsigned_abs() as usize)),
        ValueView::BigInt(_) => Err("shift is too large".to_string()),
        ValueView::Float(f) => {
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
    match v.view() {
        ValueView::Int(i) => Ok(i as u64),
        ValueView::BigInt(b) => {
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
    let bits = match b.view() {
        ValueView::Int(i) => {
            if i < 0 {
                usize::MAX
            } else {
                usize::try_from(i).unwrap_or(usize::MAX)
            }
        }
        ValueView::BigInt(bi) => {
            if bi.is_negative() {
                usize::MAX
            } else {
                bi.to_usize().unwrap_or(usize::MAX)
            }
        }
        ValueView::Float(f) => {
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
    let bits = match b.view() {
        ValueView::Int(i) => {
            if i < 0 {
                usize::MAX
            } else {
                usize::try_from(i).unwrap_or(usize::MAX)
            }
        }
        ValueView::BigInt(bi) => {
            if bi.is_negative() {
                usize::MAX
            } else {
                bi.to_usize().unwrap_or(usize::MAX)
            }
        }
        ValueView::Float(f) => {
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
    match a.view() {
        ValueView::Int(i) => Ok(Value::int(!i)),
        ValueView::BigInt(b) => Ok(Value::from_bigint(!b)),
        _ => {
            let big = to_bigint_cow(a)?;
            Ok(Value::from_bigint(!&*big))
        }
    }
}

pub fn eval_unm(a: &Value) -> Result<Value, String> {
    if a.is_inline_int() {
        return Ok(Value::int(-a.as_inline_int_unchecked()));
    }
    if a.is_float() {
        return Ok(Value::float(-a.as_float_unchecked()));
    }
    match a.view() {
        ValueView::Int(i) => Ok(match i.checked_neg() {
            Some(r) => Value::int(r),
            None => Value::from_i128(-(i as i128)),
        }),
        ValueView::BigInt(b) => Ok(Value::from_bigint(-b)),
        ValueView::Float(f) => Ok(Value::float(-f)),
        _ => Err("unary minus expects a number".to_string()),
    }
}

/// Orders two numbers, or `None` when a NaN makes them unordered. An
/// out-of-range `BigInt` compares against an `Int` by its sign alone, so no
/// digits are touched on that path either.
#[inline]
fn num_cmp(a: &Value, b: &Value) -> Result<Option<std::cmp::Ordering>, String> {
    use std::cmp::Ordering;
    Ok(Some(match (a.view(), b.view()) {
        (ValueView::Int(x), ValueView::Int(y)) => x.cmp(&y),
        (ValueView::Int(_), ValueView::BigInt(y)) => {
            if y.is_negative() {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (ValueView::BigInt(x), ValueView::Int(_)) => {
            if x.is_negative() {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (ValueView::BigInt(x), ValueView::BigInt(y)) => x.cmp(y),
        _ => return Ok(to_f64(a)?.partial_cmp(&to_f64(b)?)),
    }))
}

macro_rules! compare_op {
    ($name:ident, $op:tt, $($pat:pat_param)|+) => {
        #[inline]
        pub fn $name(a: &Value, b: &Value) -> Result<bool, String> {
            if a.is_inline_int() && b.is_inline_int() {
                return Ok(a.as_inline_int_unchecked() $op b.as_inline_int_unchecked());
            }
            if let (Some(sa), Some(sb)) = (a.as_str(), b.as_str()) {
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
    use std::rc::Rc;

    #[test]
    fn test_ops_i128_overflow_and_demotion() {
        let max = Value::int(i64::MAX);
        let one = Value::int(1);
        let add = eval_add(&max, &one).unwrap();
        assert_eq!(add.typeof_name(), "bigint");
        let sub = eval_sub(&add, &one).unwrap();
        assert_eq!(sub, Value::int(i64::MAX));

        let min = Value::int(i64::MIN);
        let neg_one = Value::int(-1);
        let idiv = eval_idiv(&min, &neg_one).unwrap();
        assert_eq!(idiv.typeof_name(), "bigint");

        let unm = eval_unm(&min).unwrap();
        assert_eq!(unm.typeof_name(), "bigint");
    }

    #[test]
    fn test_ops_bigint_borrowed_arithmetic() {
        let b = Value::from_bigint(BigInt::from(1) << 100);
        let two = Value::int(2);
        let mul = eval_mul(&b, &two).unwrap();
        assert_eq!(mul, Value::from_bigint(BigInt::from(1) << 101));

        let div = eval_idiv(&mul, &two).unwrap();
        assert_eq!(div, b);

        let rem = eval_mod(&b, &Value::int(1000)).unwrap();
        assert!(rem.is_int());
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

                let v_bi = Value::from_bigint(bi.clone());
                let v_div = Value::int(div);
                let eval_m = eval_mod(&v_bi, &v_div).unwrap();
                assert_eq!(eval_m, Value::int(std_mod));

                let eval_d = eval_idiv(&v_bi, &v_div).unwrap();
                let std_d = Value::from_bigint(bi.div_floor(&BigInt::from(div)));
                assert_eq!(eval_d, std_d, "idiv mismatch for {bi} // {div}");
            }
        }
    }

    #[test]
    fn test_low_word_equivalence() {
        let test_cases = [
            (Value::int(0), 0u64),
            (Value::int(-1), u64::MAX),
            (Value::int(-8), (-8i64) as u64),
            (Value::int(12345), 12345u64),
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

    #[test]
    fn test_bigint_shr_u64_equivalence_and_32bit_shift_range() {
        let values = [
            BigInt::from(1i64) << 63u32,
            (BigInt::from(1i64) << 64u32) - 1i64,
            BigInt::from(1i64) << 64u32,
            (BigInt::from(1i64) << 64u32) + 1i64,
            (BigInt::from(1i64) << 100u32) + BigInt::from(0xdead_beef_u64),
            (BigInt::from(1i64) << 128u32) - 1i64,
            BigInt::from(1i64) << 128u32,
            (BigInt::from(1i64) << 200u32) + BigInt::from(123456789i64),
            -((BigInt::from(1i64) << 63u32) + 1i64),
            -((BigInt::from(1i64) << 64u32) - 1i64),
            -(BigInt::from(1i64) << 64u32),
            -((BigInt::from(1i64) << 64u32) + 1i64),
            -((BigInt::from(1i64) << 100u32) + BigInt::from(0xdead_beef_u64)),
            -((BigInt::from(1i64) << 128u32) - 1i64),
            -(BigInt::from(1i64) << 128u32),
            -((BigInt::from(1i64) << 200u32) + BigInt::from(123456789i64)),
        ];
        let shifts = [0u64, 1, 2, 15, 20, 63, 64, 65, 127, 128, 200, 256];

        for b in &values {
            for &shift in &shifts {
                let got = bigint_shr_u64(b, shift);
                let expected = Value::from_bigint(b >> (shift as usize));
                assert_eq!(got, expected, "shr mismatch for {b} >> {shift}");
                if let Some(bi) = got.as_bigint() {
                    assert!(
                        bi.to_i64().is_none(),
                        "unnormalized BigInt from {b} >> {shift}: {bi}"
                    );
                }
            }
            let huge_shift = 1u64 << 32;
            let expected_huge = Value::int(if b.is_negative() { -1 } else { 0 });
            assert_eq!(bigint_shr_u64(b, huge_shift), expected_huge);
            let mut stack = vec![Value::from_bigint(b.clone())];
            eval_shr_bigint_into(&mut stack, 0, 0, huge_shift);
            assert_eq!(stack[0], expected_huge);
        }
        assert_eq!(
            bigint_shr_u64(&(-(BigInt::from(1i64) << 64u32)), 1),
            Value::int(i64::MIN)
        );
        assert_eq!(
            bigint_shr_u64(&(-((BigInt::from(1i64) << 64u32) - 1i64)), 1),
            Value::int(i64::MIN)
        );
        assert!(bigint_shr_u64(&(-((BigInt::from(1i64) << 64u32) + 1i64)), 1).is_bigint());
    }

    #[test]
    fn test_bigint_idiv_u128_and_u64_limb_equivalence() {
        let dividends = [
            BigInt::from(1i64) << 63u32,
            (BigInt::from(1i64) << 100u32) + BigInt::from(987654321i64),
            (BigInt::from(1i64) << 128u32) - 1i64,
            BigInt::from(1i64) << 128u32,
            (BigInt::from(1i64) << 192u32) + (BigInt::from(0xffff_ffff_u64) << 64u32) + 42i64,
            (BigInt::from(1i64) << 256u32) - 1i64,
            -((BigInt::from(1i64) << 130u32) + 17i64),
        ];
        let divisors_big = [
            BigInt::from(1i64) << 63u32,
            (BigInt::from(1i64) << 64u32) - 1i64,
            (BigInt::from(1i64) << 80u32) + 12345i64,
            (BigInt::from(1i64) << 128u32) - 1i64,
            (BigInt::from(1i64) << 130u32) + 7i64,
            -((BigInt::from(1i64) << 64u32) + 3i64),
        ];
        let divisors_i64 = [2i64, 3, 7, 1_000_000_007, i64::MAX, -2, -7, i64::MIN];

        for a in &dividends {
            let va = Value::from_bigint(a.clone());
            for &d in &divisors_i64 {
                let got = eval_idiv(&va, &Value::int(d)).unwrap();
                let expected = Value::from_bigint(a.div_floor(&BigInt::from(d)));
                assert_eq!(got, expected, "idiv mismatch for {a} // {d}");
            }
            for b in &divisors_big {
                let vb = Value::from_bigint(b.clone());
                let got = eval_idiv(&va, &vb).unwrap();
                let expected = Value::from_bigint(a.div_floor(b));
                assert_eq!(got, expected, "idiv mismatch for {a} // {b}");
            }
        }
    }

    #[test]
    fn test_in_place_bigint_aliasing_sharing_and_demotion() {
        let two_63 = BigInt::from(1i64) << 63u32; // i64::MAX + 1
        let two_100 = BigInt::from(1i64) << 100u32;
        let neg_min_minus_1 = BigInt::from(i64::MIN) - 1i64;

        // 1. Unique owner dst == a mutates in place; demotes cleanly to i64::MAX / i64::MIN.
        let mut stack = vec![Value::from_bigint(two_63.clone()), Value::int(-1)];
        eval_add_into(&mut stack, 0, 0, 1).unwrap();
        assert_eq!(stack[0], Value::int(i64::MAX));

        let mut stack = vec![Value::from_bigint(neg_min_minus_1.clone()), Value::int(1)];
        eval_add_into(&mut stack, 0, 0, 1).unwrap();
        assert_eq!(stack[0], Value::int(i64::MIN));

        // Unary negation in place: -(2^63) demotes cleanly to i64::MIN.
        let mut stack = vec![Value::from_bigint(two_63.clone())];
        eval_unm_into(&mut stack, 0, 0).unwrap();
        assert_eq!(stack[0], Value::int(i64::MIN));

        // 2. Shared alias (strong_count >= 2) MUST NOT be mutated!
        let shared_rc = Rc::new(two_100.clone());
        let mut stack = vec![Value::from_bigint_rc(Rc::clone(&shared_rc)), Value::int(42)];
        eval_add_into(&mut stack, 0, 0, 1).unwrap();
        assert_eq!(stack[0], Value::from_bigint(&two_100 + 42i64));
        assert_eq!(*shared_rc, two_100);

        stack[0] = Value::from_bigint_rc(Rc::clone(&shared_rc));
        eval_sub_into(&mut stack, 0, 0, 1).unwrap();
        assert_eq!(stack[0], Value::from_bigint(&two_100 - 42i64));
        assert_eq!(*shared_rc, two_100);

        stack[0] = Value::from_bigint_rc(Rc::clone(&shared_rc));
        eval_shr_bigint_into(&mut stack, 0, 0, 10);
        assert_eq!(stack[0], Value::from_bigint(&two_100 >> 10u32));
        assert_eq!(*shared_rc, two_100);

        stack[0] = Value::from_bigint_rc(Rc::clone(&shared_rc));
        eval_shl_bigint_into(&mut stack, 0, 0, 5).unwrap();
        assert_eq!(stack[0], Value::from_bigint(&two_100 << 5u32));
        assert_eq!(*shared_rc, two_100);

        stack[0] = Value::from_bigint_rc(Rc::clone(&shared_rc));
        eval_bitor_into(&mut stack, 0, 0, 1).unwrap();
        assert_eq!(stack[0], Value::from_bigint(&two_100 | BigInt::from(42i64)));
        assert_eq!(*shared_rc, two_100);

        // 3. Unique-owner dst == a reuses the exact Rc allocation (ptr_eq).
        let unique_rc = Rc::new(two_100.clone());
        let orig_ptr = Rc::as_ptr(&unique_rc);
        let mut stack = vec![Value::from_bigint_rc(unique_rc), Value::int(7)];
        eval_add_into(&mut stack, 0, 0, 1).unwrap();
        let after_add = stack[0].as_bigint().expect("expected BigInt");
        assert_eq!(after_add as *const BigInt, orig_ptr);
        assert_eq!(*after_add, &two_100 + 7i64);

        eval_bitor_into(&mut stack, 0, 0, 1).unwrap();
        eval_bitxor_into(&mut stack, 0, 0, 1).unwrap();
        eval_shl_bigint_into(&mut stack, 0, 0, 4).unwrap();
        eval_shr_bigint_into(&mut stack, 0, 0, 4);
        let after_ops = stack[0].as_bigint().expect("expected BigInt");
        assert_eq!(after_ops as *const BigInt, orig_ptr);

        // 4. dst == b (commutative add) and dst == a == b (doubling / self-sub).
        let mut stack = vec![Value::int(7), Value::from_bigint(two_100.clone())];
        eval_add_into(&mut stack, 1, 0, 1).unwrap();
        assert_eq!(stack[1], Value::from_bigint(&two_100 + 7i64));

        let mut stack = vec![Value::from_bigint(two_100.clone())];
        eval_add_into(&mut stack, 0, 0, 0).unwrap();
        assert_eq!(stack[0], Value::from_bigint(&two_100 << 1u32));

        let mut stack = vec![Value::from_bigint(two_100.clone())];
        eval_sub_into(&mut stack, 0, 0, 0).unwrap();
        assert_eq!(stack[0], Value::int(0));

        // 5. Bitwise AND demotion back to Int when masked to small positive int.
        let mut stack = vec![
            Value::from_bigint(&two_100 | BigInt::from(0xffi64)),
            Value::int(0x0f),
        ];
        eval_bitand_into(&mut stack, 0, 0, 1).unwrap();
        assert_eq!(stack[0], Value::int(0x0f));

        // 6. MAX_INT_BITS overflow check happens BEFORE mutation.
        let huge = BigInt::from(1i64) << (MAX_INT_BITS as usize);
        let mut stack = vec![Value::from_bigint(huge.clone()), Value::int(1)];
        assert!(eval_add_into(&mut stack, 0, 0, 1).is_err());
        assert_eq!(stack[0], Value::from_bigint(huge.clone()));
        assert!(eval_shl_bigint_into(&mut stack, 0, 0, 1).is_err());
        assert_eq!(stack[0], Value::from_bigint(huge));
    }
}
