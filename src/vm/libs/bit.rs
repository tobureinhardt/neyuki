//! The `bit` library: 32-bit bitwise operations.

use num_bigint::BigInt;
use num_traits::{ToPrimitive, Zero};

use crate::vm::machine::VM;
use crate::vm::value::{Value, ValueView, VmTable};

/// The low 32 bits of an integer, so negative and oversized values wrap
/// modulo 2^32 as in Lua's bit32.
fn low_bits(value: &BigInt) -> u32 {
    (value & BigInt::from(u32::MAX)).to_u32().unwrap_or(0)
}

fn to_u32(val: &Value) -> Result<u32, String> {
    match val.view() {
        ValueView::Int(i) => Ok(i as u32),
        ValueView::BigInt(b) => Ok(low_bits(b)),
        ValueView::Float(f) if f.is_finite() => Ok(f as i64 as u32),
        _ => Err("bit library expects a finite number".to_string()),
    }
}

/// Evaluates `bit.<name>(args...)` on integer constants exactly as the
/// natives below would, for the constant folder. `None` when the native would
/// raise an error, or for functions the folder leaves to runtime.
pub(crate) fn fold_call(name: &str, args: &[BigInt]) -> Option<BigInt> {
    let words: Vec<u32> = args.iter().map(low_bits).collect();
    let arg = |i: usize| words.get(i).copied();
    let res = match name {
        "band" => words.iter().fold(u32::MAX, |acc, w| acc & w),
        "bor" => words.iter().fold(0, |acc, w| acc | w),
        "bxor" => words.iter().fold(0, |acc, w| acc ^ w),
        "bnot" => !arg(0)?,
        "lshift" => arg(0)? << (arg(1)? % 32),
        "rshift" => arg(0)? >> (arg(1)? % 32),
        _ => return None,
    };
    Some(BigInt::from(res))
}

fn to_i32(val: &Value) -> Result<i32, String> {
    to_u32(val).map(|u| u as i32)
}

fn bit_band(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    if args.is_empty() {
        return Ok(vec![Value::from_bigint(BigInt::from(0xFFFFFFFFu32))]);
    }
    let mut res = to_u32(&args[0])?;
    for a in &args[1..] {
        res &= to_u32(a)?;
    }
    Ok(vec![Value::from_bigint(BigInt::from(res))])
}

fn bit_bor(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut res = 0u32;
    for a in args {
        res |= to_u32(a)?;
    }
    Ok(vec![Value::from_bigint(BigInt::from(res))])
}

fn bit_bxor(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut res = 0u32;
    for a in args {
        res ^= to_u32(a)?;
    }
    Ok(vec![Value::from_bigint(BigInt::from(res))])
}

fn bit_bnot(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args
        .first()
        .ok_or_else(|| "bit.bnot expects 1 argument".to_string())?;
    let u = to_u32(val)?;
    Ok(vec![Value::from_bigint(BigInt::from(!u))])
}

fn bit_lshift(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args
        .first()
        .ok_or_else(|| "bit.lshift expects 2 arguments".to_string())?;
    let disp = args
        .get(1)
        .ok_or_else(|| "bit.lshift expects 2 arguments".to_string())?;
    let u = to_u32(val)?;
    let s = to_u32(disp)? % 32;
    Ok(vec![Value::from_bigint(BigInt::from(u << s))])
}

fn bit_rshift(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args
        .first()
        .ok_or_else(|| "bit.rshift expects 2 arguments".to_string())?;
    let disp = args
        .get(1)
        .ok_or_else(|| "bit.rshift expects 2 arguments".to_string())?;
    let u = to_u32(val)?;
    let s = to_u32(disp)? % 32;
    Ok(vec![Value::from_bigint(BigInt::from(u >> s))])
}

fn bit_arshift(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args
        .first()
        .ok_or_else(|| "bit.arshift expects 2 arguments".to_string())?;
    let disp = args
        .get(1)
        .ok_or_else(|| "bit.arshift expects 2 arguments".to_string())?;
    let i = to_i32(val)?;
    let s = to_u32(disp)? % 32;
    Ok(vec![Value::from_bigint(BigInt::from(i >> s))])
}

fn bit_rol(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args
        .first()
        .ok_or_else(|| "bit.rol expects 2 arguments".to_string())?;
    let disp = args
        .get(1)
        .ok_or_else(|| "bit.rol expects 2 arguments".to_string())?;
    let u = to_u32(val)?;
    let s = to_u32(disp)? % 32;
    Ok(vec![Value::from_bigint(BigInt::from(u.rotate_left(s)))])
}

fn bit_ror(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args
        .first()
        .ok_or_else(|| "bit.ror expects 2 arguments".to_string())?;
    let disp = args
        .get(1)
        .ok_or_else(|| "bit.ror expects 2 arguments".to_string())?;
    let u = to_u32(val)?;
    let s = to_u32(disp)? % 32;
    Ok(vec![Value::from_bigint(BigInt::from(u.rotate_right(s)))])
}

fn bit_btest(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let res = bit_band(_vm, args)?;
    if let Some(i) = res.first().and_then(|v| v.as_int()) {
        Ok(vec![Value::bool(!i.is_zero())])
    } else {
        Ok(vec![Value::FALSE])
    }
}

fn bit_extract(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args
        .first()
        .ok_or_else(|| "bit.extract expects at least 2 arguments".to_string())?;
    let field = args
        .get(1)
        .ok_or_else(|| "bit.extract expects at least 2 arguments".to_string())?;
    let width = args.get(2);
    let u = to_u32(val)?;
    let f = to_u32(field)?;
    let w = if let Some(wv) = width { to_u32(wv)? } else { 1 };
    if f.saturating_add(w) > 32 || w == 0 {
        return Err("invalid bit field or width".to_string());
    }
    let mask = (1u64 << w) - 1;
    let res = ((u >> f) as u64 & mask) as u32;
    Ok(vec![Value::from_bigint(BigInt::from(res))])
}

fn bit_replace(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args
        .first()
        .ok_or_else(|| "bit.replace expects at least 3 arguments".to_string())?;
    let rep = args
        .get(1)
        .ok_or_else(|| "bit.replace expects at least 3 arguments".to_string())?;
    let field = args
        .get(2)
        .ok_or_else(|| "bit.replace expects at least 3 arguments".to_string())?;
    let width = args.get(3);
    let u = to_u32(val)?;
    let v = to_u32(rep)?;
    let f = to_u32(field)?;
    let w = if let Some(wv) = width { to_u32(wv)? } else { 1 };
    if f.saturating_add(w) > 32 || w == 0 {
        return Err("invalid bit field or width".to_string());
    }
    let mask = (((1u64 << w) - 1) << f) as u32;
    let res = (u & !mask) | ((v << f) & mask);
    Ok(vec![Value::from_bigint(BigInt::from(res))])
}

fn bit_tohex(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args
        .first()
        .ok_or_else(|| "bit.tohex expects at least 1 argument".to_string())?;
    let u = to_u32(val)?;
    let n = if let Some(nv) = args.get(1) {
        to_i32(nv)?
    } else {
        8
    };
    if n.unsigned_abs() > 64 {
        return Err("bit.tohex: width out of range (-64..64)".to_string());
    }
    let hex_full = format!("{:08x}", u);
    let len = n.unsigned_abs() as usize;
    let res = if len <= 8 {
        hex_full[8 - len..].to_string()
    } else {
        format!("{:0width$x}", u, width = len)
    };
    Ok(vec![Value::string(if n < 0 {
        res.to_uppercase()
    } else {
        res
    })])
}

fn bit_tobit(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let val = args
        .first()
        .ok_or_else(|| "bit.tobit expects 1 argument".to_string())?;
    let i = to_i32(val)?;
    Ok(vec![Value::from_bigint(BigInt::from(i))])
}

pub fn create_bit_lib() -> Value {
    let mut table = VmTable::new();
    table.set_str("band", crate::native!("bit.band", bit_band));
    table.set_str("bor", crate::native!("bit.bor", bit_bor));
    table.set_str("bxor", crate::native!("bit.bxor", bit_bxor));
    table.set_str("bnot", crate::native!("bit.bnot", bit_bnot));
    table.set_str("lshift", crate::native!("bit.lshift", bit_lshift));
    table.set_str("rshift", crate::native!("bit.rshift", bit_rshift));
    table.set_str("arshift", crate::native!("bit.arshift", bit_arshift));
    table.set_str("rol", crate::native!("bit.rol", bit_rol));
    table.set_str("ror", crate::native!("bit.ror", bit_ror));
    table.set_str("btest", crate::native!("bit.btest", bit_btest));
    table.set_str("extract", crate::native!("bit.extract", bit_extract));
    table.set_str("replace", crate::native!("bit.replace", bit_replace));
    table.set_str("tohex", crate::native!("bit.tohex", bit_tohex));
    table.set_str("tobit", crate::native!("bit.tobit", bit_tobit));
    crate::vm::libs::args::frozen(table)
}
