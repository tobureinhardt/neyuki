//! The `math` library.

use std::cell::RefCell;

use num_bigint::{BigInt, Sign};
use num_traits::{FromPrimitive, Signed, ToPrimitive};
use rand::rngs::StdRng;
use rand::{Rng, RngCore, SeedableRng};

use crate::vm::libs::args::number_arg;
use crate::vm::machine::VM;
use crate::vm::value::{Value, ValueView, VmTable};

thread_local! {
    /// Seeded from the OS until a script calls `math.randomseed`.
    static RNG: RefCell<StdRng> = RefCell::new(StdRng::from_entropy());
}

fn with_rng<T>(f: impl FnOnce(&mut StdRng) -> T) -> T {
    RNG.with(|rng| f(&mut rng.borrow_mut()))
}

fn num(args: &[Value], index: usize, name: &str) -> Result<f64, String> {
    number_arg(args, index, name)
}

fn float(value: f64) -> Result<Vec<Value>, String> {
    Ok(vec![Value::float(value)])
}

fn float_to_integer(value: f64) -> Result<Value, String> {
    BigInt::from_f64(value)
        .map(Value::from_bigint)
        .ok_or_else(|| "value cannot be converted to an integer".to_string())
}

/// Integers pass through rounding unchanged; floats are rounded by `f`.
fn round_with(args: &[Value], name: &str, f: fn(f64) -> f64) -> Result<Vec<Value>, String> {
    match args.first() {
        Some(v) if v.is_int() => Ok(vec![v.clone()]),
        _ => Ok(vec![float_to_integer(f(num(args, 0, name)?))?]),
    }
}

fn math_floor(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    round_with(args, "n", f64::floor)
}

fn math_ceil(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    round_with(args, "n", f64::ceil)
}

fn math_round(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    round_with(args, "n", f64::round)
}

fn math_trunc(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    round_with(args, "n", f64::trunc)
}

fn math_abs(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match args.first().map(Value::view) {
        Some(ValueView::Int(i)) => match i.checked_abs() {
            Some(a) => Value::int(a),
            None => Value::from_bigint(BigInt::from(i).abs()),
        },
        Some(ValueView::BigInt(i)) => Value::from_bigint(i.abs()),
        _ => Value::float(num(args, 0, "n")?.abs()),
    }])
}

fn math_sign(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match args.first().map(Value::view) {
        Some(ValueView::Int(i)) => Value::int(i.signum()),
        Some(ValueView::BigInt(i)) => Value::int(if i.is_negative() { -1 } else { 1 }),
        _ => {
            let n = num(args, 0, "n")?;
            if n.is_nan() {
                Value::float(n)
            } else if n > 0.0 {
                Value::int(1)
            } else if n < 0.0 {
                Value::int(-1)
            } else {
                Value::int(0)
            }
        }
    }])
}

fn math_sqrt(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let n = num(args, 0, "n")?;
    if n < 0.0 {
        return Err("n must be non-negative".to_string());
    }
    float(n.sqrt())
}

fn math_cbrt(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    float(num(args, 0, "n")?.cbrt())
}

fn math_pow(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    float(num(args, 0, "base")?.powf(num(args, 1, "exponent")?))
}

fn math_exp(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    float(num(args, 0, "n")?.exp())
}

fn math_log(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let value = num(args, 0, "n")?;
    if value < 0.0 {
        return Err("log expects a non-negative number".to_string());
    }
    match args.get(1) {
        None => float(value.ln()),
        Some(v) if v.is_nil() => float(value.ln()),
        Some(_) => {
            let base = num(args, 1, "base")?;
            if base <= 0.0 || base == 1.0 {
                return Err("log base must be positive and not equal to 1".to_string());
            }
            if base == 2.0 {
                return float(value.log2());
            }
            if base == 10.0 {
                return float(value.log10());
            }
            float(value.log(base))
        }
    }
}

fn math_log10(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let value = num(args, 0, "n")?;
    if value < 0.0 {
        return Err("log10 expects a non-negative number".to_string());
    }
    float(value.log10())
}

fn math_log2(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let value = num(args, 0, "n")?;
    if value < 0.0 {
        return Err("log2 expects a non-negative number".to_string());
    }
    float(value.log2())
}

macro_rules! unary {
    ($name:ident, $f:expr) => {
        fn $name(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
            float($f(num(args, 0, "n")?))
        }
    };
}

unary!(math_sin, f64::sin);
unary!(math_cos, f64::cos);
unary!(math_tan, f64::tan);
unary!(math_sinh, f64::sinh);
unary!(math_cosh, f64::cosh);
unary!(math_tanh, f64::tanh);
unary!(math_deg, f64::to_degrees);
unary!(math_rad, f64::to_radians);

fn math_asin(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let n = num(args, 0, "n")?;
    if !(-1.0..=1.0).contains(&n) {
        return Err("asin expects a number between -1 and 1".to_string());
    }
    float(n.asin())
}

fn math_acos(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let n = num(args, 0, "n")?;
    if !(-1.0..=1.0).contains(&n) {
        return Err("acos expects a number between -1 and 1".to_string());
    }
    float(n.acos())
}

/// `atan(y)` or, like Lua 5.4, `atan(y, x)` for the full-circle angle.
fn math_atan(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let y = num(args, 0, "y")?;
    match args.get(1) {
        None => float(y.atan()),
        Some(v) if v.is_nil() => float(y.atan()),
        Some(_) => float(y.atan2(num(args, 1, "x")?)),
    }
}

fn math_atan2(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    float(num(args, 0, "y")?.atan2(num(args, 1, "x")?))
}

fn math_fmod(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    if let (Some(a), Some(b)) = (
        args.first().and_then(Value::as_int),
        args.get(1).and_then(Value::as_int),
    ) {
        if b == 0 {
            return Err("fmod divisor must not be zero".to_string());
        }
        return Ok(vec![Value::int(a.checked_rem(b).unwrap_or(0))]);
    }
    let a = num(args, 0, "a")?;
    let b = num(args, 1, "b")?;
    if b == 0.0 {
        return Err("fmod divisor must not be zero".to_string());
    }
    float(a % b)
}

fn math_modf(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let value = num(args, 0, "n")?;
    if value.is_infinite() {
        return Ok(vec![Value::float(value), Value::float(0.0)]);
    }
    Ok(vec![
        Value::float(value.trunc()),
        Value::float(value.fract()),
    ])
}

fn math_frexp(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let value = num(args, 0, "n")?;
    if value == 0.0 || !value.is_finite() {
        return Ok(vec![Value::float(value), Value::int(0)]);
    }
    let exponent_mask = 0x7ffu64 << 52;
    // Subnormals are scaled up first so the exponent field is populated.
    let (bits, bias) = if value.to_bits() & exponent_mask == 0 {
        ((value * 2f64.powi(64)).to_bits(), 1022 + 64)
    } else {
        (value.to_bits(), 1022)
    };
    let raw_exponent = ((bits & exponent_mask) >> 52) as i64;
    let mantissa = f64::from_bits((bits & !exponent_mask) | (1022u64 << 52));
    Ok(vec![
        Value::float(mantissa),
        Value::int(raw_exponent - bias),
    ])
}

fn math_ldexp(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mantissa = num(args, 0, "mantissa")?;
    let exponent = num(args, 1, "exponent")?;
    if !exponent.is_finite() || exponent.fract() != 0.0 {
        return Err("ldexp exponent must be an integer".to_string());
    }
    let exponent = exponent.clamp(i32::MIN as f64, i32::MAX as f64) as i32;
    float(mantissa * 2f64.powi(exponent))
}

fn math_isfinite(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::bool(match args.first() {
        Some(v) if v.is_int() => true,
        _ => num(args, 0, "n")?.is_finite(),
    })])
}

fn math_isinf(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::bool(match args.first() {
        Some(v) if v.is_int() => false,
        _ => num(args, 0, "n")?.is_infinite(),
    })])
}

fn math_isnan(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::bool(match args.first() {
        Some(v) if v.is_int() => false,
        _ => num(args, 0, "n")?.is_nan(),
    })])
}

/// Numbers from the arguments, with arrays of numbers flattened, so both
/// `math.max(1, 5, 3)` and `math.max(scores)` work.
fn collect_numbers(args: &[Value], name: &str) -> Result<Vec<Value>, String> {
    fn push(value: &Value, out: &mut Vec<Value>, depth: usize) -> Result<(), String> {
        if value.is_number() {
            out.push(value.clone());
            return Ok(());
        }
        if let Some(t) = value.as_table()
            && depth < 16
        {
            for item in &t.borrow().array {
                push(item, out, depth + 1)?;
            }
            return Ok(());
        }
        Err(format!(
            "expected numbers or arrays of numbers, got {}",
            value.type_name()
        ))
    }
    let mut out = Vec::with_capacity(args.len());
    for value in args {
        push(value, &mut out, 0)?;
    }
    if out.is_empty() {
        return Err(format!("{} expects at least one number", name));
    }
    Ok(out)
}

fn extreme(args: &[Value], name: &str, want_greater: bool) -> Result<Vec<Value>, String> {
    let values = collect_numbers(args, name)?;
    let mut best = values[0].clone();
    for value in &values[1..] {
        let greater = crate::vm::ops::eval_lt(&best, value)?;
        let less = crate::vm::ops::eval_lt(value, &best)?;
        if (want_greater && greater) || (!want_greater && less) {
            best = value.clone();
        }
    }
    Ok(vec![best])
}

fn math_max(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    extreme(args, "math.max", true)
}

fn math_min(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    extreme(args, "math.min", false)
}

fn math_clamp(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let n = args.first().cloned().unwrap_or(Value::NIL);
    let min = args.get(1).cloned().unwrap_or(Value::NIL);
    let max = args.get(2).cloned().unwrap_or(Value::NIL);
    for (value, name) in [(&n, "n"), (&min, "min"), (&max, "max")] {
        if !value.is_number() {
            return Err(format!("{} must be a number", name));
        }
    }
    if crate::vm::ops::eval_lt(&max, &min)? {
        return Err("min must be less than or equal to max".to_string());
    }
    Ok(vec![if crate::vm::ops::eval_lt(&n, &min)? {
        min
    } else if crate::vm::ops::eval_lt(&max, &n)? {
        max
    } else {
        n
    }])
}

fn math_lerp(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let a = num(args, 0, "a")?;
    let b = num(args, 1, "b")?;
    let t = num(args, 2, "t")?;
    float(a + (b - a) * t)
}

fn math_map(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let n = num(args, 0, "n")?;
    let in_min = num(args, 1, "inMin")?;
    let in_max = num(args, 2, "inMax")?;
    let out_min = num(args, 3, "outMin")?;
    let out_max = num(args, 4, "outMax")?;
    if in_min == in_max {
        return Err("inMin and inMax must differ".to_string());
    }
    float(out_min + (n - in_min) * (out_max - out_min) / (in_max - in_min))
}

fn integer_bound(value: &Value, name: &str) -> Result<BigInt, String> {
    match value.view() {
        ValueView::Int(i) => Ok(BigInt::from(i)),
        ValueView::BigInt(i) => Ok(i.clone()),
        ValueView::Float(f) if f.is_finite() && f.fract() == 0.0 => {
            BigInt::from_f64(f).ok_or_else(|| format!("{} must be an integer", name))
        }
        ValueView::Nil => Err(format!("{} must be provided", name)),
        _ => Err(format!("{} must be an integer", name)),
    }
}

/// A uniform integer in `min..=max`, with bignum bounds by rejection
/// sampling over the range's bytes.
fn random_between(min: BigInt, max: BigInt) -> Value {
    if let (Some(lo), Some(hi)) = (min.to_i64(), max.to_i64()) {
        return Value::int(with_rng(|rng| rng.gen_range(lo..=hi)));
    }
    let range = &max - &min;
    let bit_count = range.bits();
    let byte_count = bit_count.div_ceil(8) as usize;
    let excess_bits = (byte_count as u64 * 8).saturating_sub(bit_count);
    let mut bytes = vec![0; byte_count];
    let offset = with_rng(|rng| {
        loop {
            rng.fill_bytes(&mut bytes);
            if excess_bits > 0 {
                bytes[0] &= u8::MAX >> excess_bits;
            }
            let offset = BigInt::from_bytes_be(Sign::Plus, &bytes);
            if offset <= range {
                break offset;
            }
        }
    });
    Value::from_bigint(min + offset)
}

/// `random()` is a float in [0, 1), `random(m)` an integer in 1..=m and
/// `random(min, max)` an integer in min..=max. Bounds may be any size.
fn math_random(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let nil = Value::NIL;
    let first = args.first().unwrap_or(&nil);
    let second = args.get(1).unwrap_or(&nil);
    match (first.view(), second.view()) {
        (ValueView::Nil, ValueView::Nil) => float(with_rng(|rng| rng.gen_range(0.0..1.0))),
        (_, ValueView::Nil) => {
            let m = integer_bound(first, "m")?;
            if m < BigInt::from(1) {
                return Err("m must be greater than or equal to 1".to_string());
            }
            Ok(vec![random_between(BigInt::from(1), m)])
        }
        _ => {
            let min = integer_bound(first, "min")?;
            let max = integer_bound(second, "max")?;
            if min > max {
                return Err("min must be less than or equal to max".to_string());
            }
            Ok(vec![random_between(min, max)])
        }
    }
}

/// Reseeds `math.random`, making its sequence repeatable; without a seed
/// it goes back to OS entropy.
fn math_randomseed(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let rng = match args.first().map(Value::view) {
        None | Some(ValueView::Nil) => StdRng::from_entropy(),
        Some(ValueView::Int(i)) => StdRng::seed_from_u64(i as u64),
        Some(ValueView::Float(f)) => StdRng::seed_from_u64(f.to_bits()),
        Some(ValueView::BigInt(i)) => {
            let low = i.iter_u64_digits().next().unwrap_or(0);
            StdRng::seed_from_u64(low)
        }
        Some(_) => {
            return Err(format!(
                "seed must be a number, got {}",
                args[0].type_name()
            ));
        }
    };
    RNG.with(|cell| *cell.borrow_mut() = rng);
    Ok(Vec::new())
}

// Improved Perlin noise (Ken Perlin, 2002). Values fall in roughly [-1, 1]
// and integer lattice points always yield 0.
const PERLIN_PERMUTATION: [u8; 256] = [
    151, 160, 137, 91, 90, 15, 131, 13, 201, 95, 96, 53, 194, 233, 7, 225, 140, 36, 103, 30, 69,
    142, 8, 99, 37, 240, 21, 10, 23, 190, 6, 148, 247, 120, 234, 75, 0, 26, 197, 62, 94, 252, 219,
    203, 117, 35, 11, 32, 57, 177, 33, 88, 237, 149, 56, 87, 174, 20, 125, 136, 171, 168, 68, 175,
    74, 165, 71, 134, 139, 48, 27, 166, 77, 146, 158, 231, 83, 111, 229, 122, 60, 211, 133, 230,
    220, 105, 92, 41, 55, 46, 245, 40, 244, 102, 143, 54, 65, 25, 63, 161, 1, 216, 80, 73, 209, 76,
    132, 187, 208, 89, 18, 169, 200, 196, 135, 130, 116, 188, 159, 86, 164, 100, 109, 198, 173,
    186, 3, 64, 52, 217, 226, 250, 124, 123, 5, 202, 38, 147, 118, 126, 255, 82, 85, 212, 207, 206,
    59, 227, 47, 16, 58, 17, 182, 189, 28, 42, 223, 183, 170, 213, 119, 248, 152, 2, 44, 154, 163,
    70, 221, 153, 101, 155, 167, 43, 172, 9, 129, 22, 39, 253, 19, 98, 108, 110, 79, 113, 224, 232,
    178, 185, 112, 104, 218, 246, 97, 228, 251, 34, 242, 193, 238, 210, 144, 12, 191, 179, 162,
    241, 81, 51, 145, 235, 249, 14, 239, 107, 49, 192, 214, 31, 181, 199, 106, 157, 184, 84, 204,
    176, 115, 121, 50, 45, 127, 4, 150, 254, 138, 236, 205, 93, 222, 114, 67, 29, 24, 72, 243, 141,
    128, 195, 78, 66, 215, 61, 156, 180,
];

fn perlin_hash(index: i64) -> i64 {
    PERLIN_PERMUTATION[index.rem_euclid(256) as usize] as i64
}

fn perlin_fade(t: f64) -> f64 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

fn perlin_lerp(t: f64, a: f64, b: f64) -> f64 {
    a + t * (b - a)
}

fn perlin_grad(hash: i64, x: f64, y: f64, z: f64) -> f64 {
    let h = hash & 15;
    let u = if h < 8 { x } else { y };
    let v = if h < 4 {
        y
    } else if h == 12 || h == 14 {
        x
    } else {
        z
    };
    (if h & 1 == 0 { u } else { -u }) + (if h & 2 == 0 { v } else { -v })
}

pub(crate) fn perlin_noise(x: f64, y: f64, z: f64) -> f64 {
    // The permutation repeats every 256 cells, so reducing the cell indices
    // first changes no result and keeps the sums below from overflowing.
    let cell = |c: f64| c.floor().rem_euclid(256.0) as i64;
    let (xi, yi, zi) = (cell(x), cell(y), cell(z));
    let (x, y, z) = (x - x.floor(), y - y.floor(), z - z.floor());
    let (u, v, w) = (perlin_fade(x), perlin_fade(y), perlin_fade(z));
    let a = perlin_hash(xi) + yi;
    let aa = perlin_hash(a) + zi;
    let ab = perlin_hash(a + 1) + zi;
    let b = perlin_hash(xi + 1) + yi;
    let ba = perlin_hash(b) + zi;
    let bb = perlin_hash(b + 1) + zi;
    let lerp_x = |h0: i64, h1: i64, y: f64, z: f64| {
        perlin_lerp(
            u,
            perlin_grad(perlin_hash(h0), x, y, z),
            perlin_grad(perlin_hash(h1), x - 1.0, y, z),
        )
    };
    perlin_lerp(
        w,
        perlin_lerp(v, lerp_x(aa, ba, y, z), lerp_x(ab, bb, y - 1.0, z)),
        perlin_lerp(
            v,
            lerp_x(aa + 1, ba + 1, y, z - 1.0),
            lerp_x(ab + 1, bb + 1, y - 1.0, z - 1.0),
        ),
    )
}

fn math_noise(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let coordinate = |index: usize, name: &str| -> Result<f64, String> {
        match args.get(index) {
            None if index > 0 => Ok(0.0),
            Some(v) if index > 0 && v.is_nil() => Ok(0.0),
            _ => {
                let value = num(args, index, name)?;
                if !value.is_finite() {
                    return Err(format!("{} must be finite", name));
                }
                Ok(value)
            }
        }
    };
    float(perlin_noise(
        coordinate(0, "x")?,
        coordinate(1, "y")?,
        coordinate(2, "z")?,
    ))
}

pub fn create_math_lib() -> Value {
    let mut table = VmTable::new();
    macro_rules! reg {
        ($($name:literal => $func:ident),* $(,)?) => {
            $(table.set_str($name, crate::native!(concat!("math.", $name), $func));)*
        };
    }
    reg! {
        "abs" => math_abs,
        "acos" => math_acos,
        "asin" => math_asin,
        "atan" => math_atan,
        "atan2" => math_atan2,
        "cbrt" => math_cbrt,
        "ceil" => math_ceil,
        "clamp" => math_clamp,
        "cos" => math_cos,
        "cosh" => math_cosh,
        "deg" => math_deg,
        "exp" => math_exp,
        "floor" => math_floor,
        "fmod" => math_fmod,
        "frexp" => math_frexp,
        "isfinite" => math_isfinite,
        "isinf" => math_isinf,
        "isnan" => math_isnan,
        "ldexp" => math_ldexp,
        "lerp" => math_lerp,
        "log" => math_log,
        "log10" => math_log10,
        "log2" => math_log2,
        "map" => math_map,
        "max" => math_max,
        "min" => math_min,
        "modf" => math_modf,
        "noise" => math_noise,
        "pow" => math_pow,
        "rad" => math_rad,
        "random" => math_random,
        "randomseed" => math_randomseed,
        "round" => math_round,
        "sign" => math_sign,
        "sin" => math_sin,
        "sinh" => math_sinh,
        "sqrt" => math_sqrt,
        "tan" => math_tan,
        "tanh" => math_tanh,
        "trunc" => math_trunc,
    }
    table.set_str("pi", Value::float(std::f64::consts::PI));
    table.set_str("tau", Value::float(std::f64::consts::TAU));
    table.set_str("e", Value::float(std::f64::consts::E));
    table.set_str("phi", Value::float(std::f64::consts::GOLDEN_RATIO));
    table.set_str("sqrt2", Value::float(std::f64::consts::SQRT_2));
    table.set_str("huge", Value::float(f64::INFINITY));
    table.set_str("nan", Value::float(f64::NAN));
    crate::vm::libs::args::frozen(table)
}
