//! The `utf8` library. Neyuki strings are always valid UTF-8; these
//! functions work in characters (code points) where `string` works in
//! bytes. Byte positions are 1-based like everywhere else.

use std::cell::RefCell;
use std::rc::Rc;

use crate::vm::libs::args::{int_arg, opt_int_arg, str_arg};
use crate::vm::libs::strlib::{pos_relat_end, pos_relat_start};
use crate::vm::machine::VM;
use crate::vm::value::{Value, VmTable};

pub fn create_utf8_lib() -> Value {
    let mut table = VmTable::new();
    macro_rules! reg {
        ($($name:literal => $func:ident),* $(,)?) => {
            $(table.set_str($name, crate::native!(concat!("utf8.", $name), $func));)*
        };
    }
    reg! {
        "char" => utf8_char,
        "chars" => utf8_chars,
        "codepoint" => utf8_codepoint,
        "codes" => utf8_codes,
        "len" => utf8_len,
        "offset" => utf8_offset,
        "sub" => utf8_sub,
    }
    crate::vm::libs::args::frozen(table)
}

/// `utf8.char(...)`: the string made of the given code points.
fn utf8_char(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut out = String::with_capacity(args.len());
    for index in 0..args.len() {
        let code = int_arg(args, index, &format!("code point {}", index + 1))?;
        let c = u32::try_from(code)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(|| format!("code point {} is not a valid character", code))?;
        out.push(c);
    }
    Ok(vec![Value::string(out)])
}

/// The byte range `i..=j` (1-based, negative from the end) as a 0-based
/// half-open range that starts on a character boundary.
fn byte_range(s: &str, i: i64, j: i64) -> Result<(usize, usize), String> {
    let start = pos_relat_start(i, s.len()) - 1;
    let end = pos_relat_end(j, s.len());
    if start > s.len() {
        return Err(format!("byte position {} is out of bounds", i));
    }
    if !s.is_char_boundary(start) {
        return Err(format!("byte position {} is inside a character", start + 1));
    }
    Ok((start, end.max(start)))
}

/// `utf8.codepoint(s, i?, j?)`: the code points of the characters that
/// start between byte positions `i` and `j` (both default to `i`).
fn utf8_codepoint(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = str_arg(args, 0, "s")?;
    let i = opt_int_arg(args, 1, "i", 1)?;
    let j = opt_int_arg(args, 2, "j", i)?;
    let (start, end) = byte_range(s, i, j)?;
    Ok(s[start..]
        .char_indices()
        .take_while(|(offset, _)| start + offset < end)
        .map(|(_, c)| Value::int(c as i64))
        .collect())
}

/// `utf8.len(s, i?, j?)`: how many characters start between byte positions
/// `i` and `j`.
fn utf8_len(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = str_arg(args, 0, "s")?;
    let i = opt_int_arg(args, 1, "i", 1)?;
    let j = opt_int_arg(args, 2, "j", -1)?;
    let (start, end) = byte_range(s, i, j)?;
    let count = s[start..]
        .char_indices()
        .take_while(|(offset, _)| start + offset < end)
        .count();
    Ok(vec![Value::from_usize(count)])
}

/// `utf8.offset(s, n, i?)`: the byte position where the `n`th character
/// counting from byte position `i` starts (backwards for a negative `n`;
/// `n = 0` finds the start of the character containing `i`), or nil.
fn utf8_offset(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = str_arg(args, 0, "s")?;
    let n = int_arg(args, 1, "n")?;
    let default_i = if n >= 0 { 1 } else { s.len() as i64 + 1 };
    let i = opt_int_arg(args, 2, "i", default_i)?;
    let mut pos = pos_relat_start(i, s.len()) - 1;
    if pos > s.len() {
        return Ok(vec![Value::NIL]);
    }
    if n == 0 {
        while pos > 0 && !s.is_char_boundary(pos) {
            pos -= 1;
        }
        return Ok(vec![Value::from_usize(pos + 1)]);
    }
    if !s.is_char_boundary(pos) {
        return Err(format!("byte position {} is inside a character", pos + 1));
    }
    let found = if n > 0 {
        let skip = (n - 1) as usize;
        s[pos..]
            .char_indices()
            .map(|(offset, _)| pos + offset)
            .chain(std::iter::once(s.len()))
            .nth(skip)
    } else {
        let back = n.unsigned_abs() as usize;
        s[..pos]
            .char_indices()
            .rev()
            .map(|(offset, _)| offset)
            .nth(back - 1)
    };
    Ok(vec![match found {
        Some(p) if p <= s.len() => Value::from_usize(p + 1),
        _ => Value::NIL,
    }])
}

/// `utf8.sub(s, i, j?)`: the characters from the `i`th to the `j`th,
/// counted like `string.sub` counts bytes (negative from the end).
fn utf8_sub(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = str_arg(args, 0, "s")?;
    let i = opt_int_arg(args, 1, "i", 1)?;
    let j = opt_int_arg(args, 2, "j", -1)?;
    let count = s.chars().count();
    let first = pos_relat_start(i, count);
    let last = pos_relat_end(j, count);
    if first > last {
        return Ok(vec![Value::str("")]);
    }
    let out: String = s.chars().skip(first - 1).take(last + 1 - first).collect();
    Ok(vec![Value::string(out)])
}

/// An iterator over `s`: a table holding the string and the next byte
/// offset, called through `__call`.
fn iterator(vm: &mut VM, s: &str, step: Value) -> Result<Value, String> {
    let mut meta = VmTable::new();
    meta.set_str("__call", step);
    let meta_val = vm.alloc_table(meta);
    let mut state = VmTable::new();
    state.set_str("s", Value::str(s));
    state.set_str("pos", Value::small_int(0));
    let state_val = vm.alloc_table(state);
    vm.table_set_metatable(&state_val, Some(meta_val))?;
    Ok(state_val)
}

/// The string and next byte offset an iterator table carries.
fn iterator_state(
    args: &[Value],
) -> Option<(Rc<RefCell<VmTable>>, crate::vm::value::StrRef, usize)> {
    let state = args.first()?.as_table()?;
    let st = state.borrow();
    let s_val = st.get_str("s");
    let pos_val = st.get_str("pos");
    let s = s_val.to_str_ref()?;
    let pos = pos_val.as_int()?;
    drop(st);
    Some((state.to_rc(), s, pos as usize))
}

fn codes_step(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some((state, s, pos)) = iterator_state(args) else {
        return Ok(vec![Value::NIL]);
    };
    match s.get(pos..).and_then(|rest| rest.chars().next()) {
        Some(c) => {
            state
                .borrow_mut()
                .set_str("pos", Value::from_usize(pos + c.len_utf8()));
            Ok(vec![Value::from_usize(pos + 1), Value::int(c as i64)])
        }
        None => Ok(vec![Value::NIL]),
    }
}

fn chars_step(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some((state, s, pos)) = iterator_state(args) else {
        return Ok(vec![Value::NIL]);
    };
    match s.get(pos..).and_then(|rest| rest.chars().next()) {
        Some(c) => {
            state
                .borrow_mut()
                .set_str("pos", Value::from_usize(pos + c.len_utf8()));
            Ok(vec![Value::string(c.to_string())])
        }
        None => Ok(vec![Value::NIL]),
    }
}

/// `utf8.codes(s)`: iterates `position, code point` for each character.
fn utf8_codes(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = str_arg(args, 0, "s")?;
    Ok(vec![iterator(
        vm,
        s,
        crate::native!("utf8.codes iterator", codes_step),
    )?])
}

/// `utf8.chars(s)`: iterates each character as a string.
fn utf8_chars(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = str_arg(args, 0, "s")?;
    Ok(vec![iterator(
        vm,
        s,
        crate::native!("utf8.chars iterator", chars_step),
    )?])
}
