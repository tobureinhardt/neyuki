//! The `buffer` library: fixed blocks of bytes for binary data.
//!
//! Offsets are 0-based byte positions. Numbers are little-endian unless the
//! optional last argument `bigEndian` is true. Writes keep the low bits of
//! an integer that does not fit, as in Luau.

use std::cell::RefCell;
use std::rc::Rc;

use base64::Engine;
use num_bigint::BigInt;
use num_traits::ToPrimitive;

use crate::vm::buffer::VmBuffer;
use crate::vm::libs::args::{bool_arg, str_arg};
use crate::vm::machine::VM;
use crate::vm::value::{Value, VmTable};

pub(crate) const MAX_SIZE: usize = 1024 * 1024 * 1024;

fn buffer_arg<'a>(
    args: &'a [Value],
    index: usize,
    name: &str,
) -> Result<&'a Rc<RefCell<VmBuffer>>, String> {
    match args.get(index) {
        Some(Value::Buffer(b)) => Ok(b),
        None | Some(Value::Nil) => Err(format!("{} must be provided", name)),
        Some(other) => Err(format!(
            "{} must be a buffer, got {}",
            name,
            other.type_name()
        )),
    }
}

fn size_arg(args: &[Value], index: usize, name: &str) -> Result<usize, String> {
    match args.get(index) {
        Some(Value::Int(i)) if *i >= 0 => Ok(*i as usize),
        Some(Value::Float(f)) if *f >= 0.0 && f.fract() == 0.0 && *f < 9e15 => Ok(*f as usize),
        Some(Value::Int(_) | Value::Float(_) | Value::BigInt(_)) => {
            Err(format!("{} must be a non-negative integer", name))
        }
        None | Some(Value::Nil) => Err(format!("{} must be provided", name)),
        Some(other) => Err(format!(
            "{} must be a number, got {}",
            name,
            other.type_name()
        )),
    }
}

fn opt_size_arg(args: &[Value], index: usize, name: &str, default: usize) -> Result<usize, String> {
    match args.get(index) {
        None | Some(Value::Nil) => Ok(default),
        _ => size_arg(args, index, name),
    }
}

/// The low 64 bits of an integer argument, in two's complement.
fn bits_arg(args: &[Value], index: usize, name: &str) -> Result<u64, String> {
    match args.get(index) {
        Some(Value::Int(i)) => Ok(*i as u64),
        Some(Value::BigInt(b)) => {
            let low = (&**b & BigInt::from(u64::MAX)).to_u64().unwrap_or(0);
            Ok(low)
        }
        Some(Value::Float(f)) if f.fract() == 0.0 && f.is_finite() => Ok(*f as i64 as u64),
        None | Some(Value::Nil) => Err(format!("{} must be provided", name)),
        Some(other) => Err(format!(
            "{} must be an integer, got {}",
            name,
            other.type_name()
        )),
    }
}

fn float_arg(args: &[Value], index: usize, name: &str) -> Result<f64, String> {
    crate::vm::libs::args::number_arg(args, index, name)
}

pub(crate) fn new_buffer(vm: &mut VM, data: Vec<u8>) -> Result<Vec<Value>, String> {
    if vm.gc.should_collect() || vm.gc.sweeping {
        vm.gc_auto_step()?;
    }
    let rc = Rc::new(RefCell::new(VmBuffer::from_bytes(data)));
    vm.gc.register_buffer(&rc);
    Ok(vec![Value::Buffer(rc)])
}

fn check_size(size: usize) -> Result<(), String> {
    if size > MAX_SIZE {
        return Err(format!("size {} exceeds the 1 GiB limit", size));
    }
    Ok(())
}

/// `buffer.create(size)`: `size` zero bytes.
fn buf_create(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let size = size_arg(args, 0, "size")?;
    check_size(size)?;
    new_buffer(vm, vec![0; size])
}

/// `buffer.fromstring(s)`: the bytes of `s`.
fn buf_fromstring(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = str_arg(args, 0, "s")?;
    new_buffer(vm, s.as_bytes().to_vec())
}

fn bytes_to_string(bytes: &[u8]) -> Value {
    Value::string(String::from_utf8_lossy(bytes).into_owned())
}

/// `buffer.tostring(b)`: the bytes as text; bytes that are not valid UTF-8
/// become U+FFFD.
fn buf_tostring(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let b = buffer_arg(args, 0, "b")?.borrow();
    Ok(vec![bytes_to_string(&b.data)])
}

fn buf_len(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::from_usize(
        buffer_arg(args, 0, "b")?.borrow().len(),
    )])
}

fn read_int(args: &[Value], size: usize, signed: bool) -> Result<Vec<Value>, String> {
    let b = buffer_arg(args, 0, "b")?.borrow();
    let offset = size_arg(args, 1, "offset")?;
    let big_endian = bool_arg(args, 2, "bigEndian")?;
    let range = b.range(offset, size)?;
    let mut raw = [0u8; 8];
    if big_endian {
        raw[8 - size..].copy_from_slice(&b.data[range]);
        raw.reverse();
    } else {
        raw[..size].copy_from_slice(&b.data[range]);
    }
    let unsigned = u64::from_le_bytes(raw);
    let value = if signed {
        let shift = 64 - size * 8;
        Value::Int(((unsigned << shift) as i64) >> shift)
    } else {
        Value::from_u64(unsigned)
    };
    Ok(vec![value])
}

fn write_int(args: &[Value], size: usize) -> Result<Vec<Value>, String> {
    let mut b = buffer_arg(args, 0, "b")?.borrow_mut();
    let offset = size_arg(args, 1, "offset")?;
    let value = bits_arg(args, 2, "value")?;
    let big_endian = bool_arg(args, 3, "bigEndian")?;
    let range = b.range(offset, size)?;
    let bytes = value.to_le_bytes();
    let dest = &mut b.data[range];
    dest.copy_from_slice(&bytes[..size]);
    if big_endian {
        dest.reverse();
    }
    Ok(Vec::new())
}

macro_rules! ints {
    ($($read:ident, $write:ident, $size:literal, $signed:literal;)*) => {
        $(
            fn $read(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
                read_int(args, $size, $signed)
            }
            fn $write(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
                write_int(args, $size)
            }
        )*
    };
}

ints! {
    buf_readi8, buf_writei8, 1, true;
    buf_readu8, buf_writeu8, 1, false;
    buf_readi16, buf_writei16, 2, true;
    buf_readu16, buf_writeu16, 2, false;
    buf_readi32, buf_writei32, 4, true;
    buf_readu32, buf_writeu32, 4, false;
    buf_readi64, buf_writei64, 8, true;
    buf_readu64, buf_writeu64, 8, false;
}

fn buf_readf32(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let b = buffer_arg(args, 0, "b")?.borrow();
    let range = b.range(size_arg(args, 1, "offset")?, 4)?;
    let raw: [u8; 4] = b.data[range].try_into().expect("range() returned 4 bytes");
    let value = if bool_arg(args, 2, "bigEndian")? {
        f32::from_be_bytes(raw)
    } else {
        f32::from_le_bytes(raw)
    };
    Ok(vec![Value::Float(value as f64)])
}

fn buf_writef32(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut b = buffer_arg(args, 0, "b")?.borrow_mut();
    let range = b.range(size_arg(args, 1, "offset")?, 4)?;
    let value = float_arg(args, 2, "value")? as f32;
    let raw = if bool_arg(args, 3, "bigEndian")? {
        value.to_be_bytes()
    } else {
        value.to_le_bytes()
    };
    b.data[range].copy_from_slice(&raw);
    Ok(Vec::new())
}

fn buf_readf64(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let b = buffer_arg(args, 0, "b")?.borrow();
    let range = b.range(size_arg(args, 1, "offset")?, 8)?;
    let raw: [u8; 8] = b.data[range].try_into().expect("range() returned 8 bytes");
    let value = if bool_arg(args, 2, "bigEndian")? {
        f64::from_be_bytes(raw)
    } else {
        f64::from_le_bytes(raw)
    };
    Ok(vec![Value::Float(value)])
}

fn buf_writef64(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut b = buffer_arg(args, 0, "b")?.borrow_mut();
    let range = b.range(size_arg(args, 1, "offset")?, 8)?;
    let value = float_arg(args, 2, "value")?;
    let raw = if bool_arg(args, 3, "bigEndian")? {
        value.to_be_bytes()
    } else {
        value.to_le_bytes()
    };
    b.data[range].copy_from_slice(&raw);
    Ok(Vec::new())
}

/// `buffer.readstring(b, offset, count)`.
fn buf_readstring(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let b = buffer_arg(args, 0, "b")?.borrow();
    let range = b.range(size_arg(args, 1, "offset")?, size_arg(args, 2, "count")?)?;
    Ok(vec![bytes_to_string(&b.data[range])])
}

/// `buffer.writestring(b, offset, s, count?)`: the first `count` bytes of
/// `s` (all of them by default).
fn buf_writestring(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut b = buffer_arg(args, 0, "b")?.borrow_mut();
    let offset = size_arg(args, 1, "offset")?;
    let s = str_arg(args, 2, "s")?.as_bytes();
    let count = opt_size_arg(args, 3, "count", s.len())?;
    if count > s.len() {
        return Err(format!("count {} is longer than the string", count));
    }
    let range = b.range(offset, count)?;
    b.data[range].copy_from_slice(&s[..count]);
    Ok(Vec::new())
}

/// `buffer.readbits(b, bitOffset, bitCount)`: up to 32 bits starting at
/// any bit, least significant bit first.
fn buf_readbits(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let b = buffer_arg(args, 0, "b")?.borrow();
    let bit_offset = size_arg(args, 1, "bitOffset")?;
    let bit_count = size_arg(args, 2, "bitCount")?;
    if bit_count > 32 {
        return Err("bitCount must be at most 32".to_string());
    }
    b.range(0, (bit_offset + bit_count).div_ceil(8))?;
    let mut value = 0u64;
    for i in 0..bit_count {
        let bit = bit_offset + i;
        if b.data[bit / 8] >> (bit % 8) & 1 == 1 {
            value |= 1 << i;
        }
    }
    Ok(vec![Value::Int(value as i64)])
}

/// `buffer.writebits(b, bitOffset, bitCount, value)`.
fn buf_writebits(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut b = buffer_arg(args, 0, "b")?.borrow_mut();
    let bit_offset = size_arg(args, 1, "bitOffset")?;
    let bit_count = size_arg(args, 2, "bitCount")?;
    let value = bits_arg(args, 3, "value")?;
    if bit_count > 32 {
        return Err("bitCount must be at most 32".to_string());
    }
    b.range(0, (bit_offset + bit_count).div_ceil(8))?;
    for i in 0..bit_count {
        let bit = bit_offset + i;
        let mask = 1u8 << (bit % 8);
        if value >> i & 1 == 1 {
            b.data[bit / 8] |= mask;
        } else {
            b.data[bit / 8] &= !mask;
        }
    }
    Ok(Vec::new())
}

/// `buffer.copy(target, targetOffset, source, sourceOffset?, count?)`:
/// copies `count` bytes (the rest of the source by default). The two may
/// be the same buffer, with overlapping ranges.
fn buf_copy(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let target = buffer_arg(args, 0, "target")?;
    let target_offset = size_arg(args, 1, "targetOffset")?;
    let source = buffer_arg(args, 2, "source")?;
    let source_offset = opt_size_arg(args, 3, "sourceOffset", 0)?;
    if Rc::ptr_eq(target, source) {
        let mut b = target.borrow_mut();
        let rest = b.len().saturating_sub(source_offset);
        let count = opt_size_arg(args, 4, "count", rest)?;
        let from = b.range(source_offset, count)?;
        b.range(target_offset, count)?;
        b.data.copy_within(from, target_offset);
        return Ok(Vec::new());
    }
    let src = source.borrow();
    let rest = src.len().saturating_sub(source_offset);
    let count = opt_size_arg(args, 4, "count", rest)?;
    let from = src.range(source_offset, count)?;
    let mut dst = target.borrow_mut();
    let to = dst.range(target_offset, count)?;
    dst.data[to].copy_from_slice(&src.data[from]);
    Ok(Vec::new())
}

/// `buffer.fill(b, offset, value, count?)`.
fn buf_fill(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut b = buffer_arg(args, 0, "b")?.borrow_mut();
    let offset = size_arg(args, 1, "offset")?;
    let value = bits_arg(args, 2, "value")? as u8;
    let rest = b.len().saturating_sub(offset);
    let count = opt_size_arg(args, 3, "count", rest)?;
    let range = b.range(offset, count)?;
    b.data[range].fill(value);
    Ok(Vec::new())
}

/// `buffer.slice(b, offset?, count?)`: a new buffer with a copy of part of
/// `b`.
fn buf_slice(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let data = {
        let b = buffer_arg(args, 0, "b")?.borrow();
        let offset = opt_size_arg(args, 1, "offset", 0)?;
        let count = opt_size_arg(args, 2, "count", b.len().saturating_sub(offset))?;
        let range = b.range(offset, count)?;
        b.data[range].to_vec()
    };
    new_buffer(vm, data)
}

/// `buffer.resize(b, size)`: grows (with zero bytes) or shrinks `b`.
fn buf_resize(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let b = buffer_arg(args, 0, "b")?;
    let size = size_arg(args, 1, "size")?;
    check_size(size)?;
    b.borrow_mut().data.resize(size, 0);
    Ok(vec![Value::Buffer(b.clone())])
}

/// `buffer.equal(a, b)`: whether two buffers hold the same bytes.
fn buf_equal(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let a = buffer_arg(args, 0, "a")?;
    let b = buffer_arg(args, 1, "b")?;
    Ok(vec![Value::Bool(
        Rc::ptr_eq(a, b) || a.borrow().data == b.borrow().data,
    )])
}

fn buf_tohex(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let b = buffer_arg(args, 0, "b")?.borrow();
    let mut out = String::with_capacity(b.len() * 2);
    for byte in &b.data {
        out.push_str(&format!("{:02x}", byte));
    }
    Ok(vec![Value::string(out)])
}

fn buf_fromhex(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = str_arg(args, 0, "s")?;
    if !s.is_ascii() {
        return Err("invalid hex text".to_string());
    }
    if s.len() % 2 != 0 {
        return Err("hex text must have an even length".to_string());
    }
    let data = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| "invalid hex text".to_string())?;
    new_buffer(vm, data)
}

fn buf_tobase64(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let b = buffer_arg(args, 0, "b")?.borrow();
    Ok(vec![Value::string(
        base64::engine::general_purpose::STANDARD.encode(&b.data),
    )])
}

fn buf_frombase64(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = str_arg(args, 0, "s")?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim()))
        .map_err(|_| "invalid base64 text".to_string())?;
    new_buffer(vm, data)
}

pub fn create_buffer_lib() -> Value {
    let mut table = VmTable::new();
    macro_rules! reg {
        ($($name:literal => $func:ident),* $(,)?) => {
            $(table.set_str($name, crate::native!(concat!("buffer.", $name), $func));)*
        };
    }
    reg! {
        "copy" => buf_copy,
        "create" => buf_create,
        "equal" => buf_equal,
        "fill" => buf_fill,
        "frombase64" => buf_frombase64,
        "fromhex" => buf_fromhex,
        "fromstring" => buf_fromstring,
        "len" => buf_len,
        "readbits" => buf_readbits,
        "readf32" => buf_readf32,
        "readf64" => buf_readf64,
        "readi16" => buf_readi16,
        "readi32" => buf_readi32,
        "readi64" => buf_readi64,
        "readi8" => buf_readi8,
        "readstring" => buf_readstring,
        "readu16" => buf_readu16,
        "readu32" => buf_readu32,
        "readu64" => buf_readu64,
        "readu8" => buf_readu8,
        "resize" => buf_resize,
        "slice" => buf_slice,
        "tobase64" => buf_tobase64,
        "tohex" => buf_tohex,
        "tostring" => buf_tostring,
        "writebits" => buf_writebits,
        "writef32" => buf_writef32,
        "writef64" => buf_writef64,
        "writei16" => buf_writei16,
        "writei32" => buf_writei32,
        "writei64" => buf_writei64,
        "writei8" => buf_writei8,
        "writestring" => buf_writestring,
        "writeu16" => buf_writeu16,
        "writeu32" => buf_writeu32,
        "writeu64" => buf_writeu64,
        "writeu8" => buf_writeu8,
    }
    crate::vm::libs::args::frozen(table)
}
