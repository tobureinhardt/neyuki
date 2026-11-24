//! Native primitives behind the `string` library.
//!
//! Strings are byte-oriented like Lua's: indices count bytes, and the pattern
//! matcher is a port of the one in `lstrlib.c`. Slices that cut through a
//! multi-byte character are decoded lossily.
//!
//! `pack` produces arbitrary bytes, which cannot always live inside a UTF-8
//! string, so packed data is carried as one Latin-1 character per byte
//! (`unpack` reverses that mapping).

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use num_bigint::{BigInt, Sign};
use num_traits::{FromPrimitive, One, ToPrimitive, Zero};

use crate::vm::machine::VM;
use crate::vm::value::{Value, ValueView, VmTable};

fn arg(args: &[Value], index: usize) -> Value {
    args.get(index).cloned().unwrap_or(Value::NIL)
}

fn number(value: Value) -> Result<f64, String> {
    crate::vm::libs::args::number(&value)
}

/// Strings, and numbers in their printed form, as Lua coerces them.
fn require_string(value: &Value) -> Result<Cow<'_, str>, String> {
    match value.view() {
        ValueView::String(s) => Ok(Cow::Borrowed(s.as_str())),
        ValueView::Int(_) | ValueView::BigInt(_) | ValueView::Float(_) => {
            Ok(Cow::Owned(value.to_string()))
        }
        _ => Err("expected a string or number".to_string()),
    }
}

pub(crate) fn string_arg<'a>(
    args: &'a [Value],
    index: usize,
    name: &str,
) -> Result<Cow<'a, str>, String> {
    match args.get(index) {
        Some(v) if v.is_string() => Ok(Cow::Borrowed(v.as_str().unwrap())),
        None => Err(format!("{} must be provided", name)),
        Some(v) if v.is_nil() => Err(format!("{} must be provided", name)),
        Some(_) => Err(format!("{} must be a string", name)),
    }
}

pub(crate) fn int_arg(args: &[Value], index: usize, name: &str) -> Result<i64, String> {
    match arg(args, index).view() {
        ValueView::Int(value) => Ok(value),
        ValueView::BigInt(_) => Err(format!("{} is out of range", name)),
        ValueView::Float(value) if value.is_finite() && value.fract() == 0.0 => Ok(value as i64),
        ValueView::Float(_) => Err(format!("{} must be an integer", name)),
        ValueView::Nil => Err(format!("{} must be provided", name)),
        _ => Err(format!("{} must be a number", name)),
    }
}

pub(crate) fn opt_int_arg(
    args: &[Value],
    index: usize,
    name: &str,
    default: i64,
) -> Result<i64, String> {
    if arg(args, index).is_nil() {
        Ok(default)
    } else {
        int_arg(args, index, name)
    }
}

fn bytes_to_string(bytes: &[u8]) -> Value {
    Value::string(String::from_utf8_lossy(bytes).into_owned())
}

fn integer(value: i64) -> Value {
    Value::int(value)
}

/// Translate a relative (possibly negative) 1-based start index, Lua style.
pub(crate) fn pos_relat_start(pos: i64, len: usize) -> usize {
    if pos > 0 {
        pos as usize
    } else if pos == 0 || pos < -(len as i64) {
        1
    } else {
        (len as i64 + pos + 1) as usize
    }
}

/// Translate a relative (possibly negative) 1-based end index, clipped to `len`.
pub(crate) fn pos_relat_end(pos: i64, len: usize) -> usize {
    if pos > len as i64 {
        len
    } else if pos >= 0 {
        pos as usize
    } else if pos < -(len as i64) {
        0
    } else {
        (len as i64 + pos + 1) as usize
    }
}

// ---------------------------------------------------------------------------
// simple primitives
// ---------------------------------------------------------------------------

pub(crate) fn builtin_byte(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    let bytes = s.as_bytes();
    let i = pos_relat_start(opt_int_arg(args, 1, "i", 1)?, bytes.len());
    let j = pos_relat_end(opt_int_arg(args, 2, "j", i as i64)?, bytes.len());
    if i > j {
        return Ok(Vec::new());
    }
    Ok(bytes[i - 1..j]
        .iter()
        .map(|byte| integer(*byte as i64))
        .collect())
}

pub(crate) fn builtin_char(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut bytes = Vec::with_capacity(args.len());
    for index in 0..args.len() {
        let value = int_arg(args, index, "byte")?;
        if !(0..=255).contains(&value) {
            return Err(format!("byte {} is out of range (0-255)", index + 1));
        }
        bytes.push(value as u8);
    }
    String::from_utf8(bytes)
        .map(|s| vec![Value::string(s)])
        .map_err(|_| "string.char bytes do not form valid UTF-8".to_string())
}

pub(crate) fn builtin_count(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    Ok(vec![integer(s.chars().count() as i64)])
}

pub(crate) fn builtin_sub(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    let bytes = s.as_bytes();
    let i = pos_relat_start(opt_int_arg(args, 1, "i", 1)?, bytes.len());
    let j = pos_relat_end(opt_int_arg(args, 2, "j", -1)?, bytes.len());
    if i > j {
        return Ok(vec![Value::str("")]);
    }
    Ok(vec![bytes_to_string(&bytes[i - 1..j])])
}

pub(crate) fn builtin_lower(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::string(
        string_arg(args, 0, "s")?.to_lowercase(),
    )])
}

pub(crate) fn builtin_upper(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::string(
        string_arg(args, 0, "s")?.to_uppercase(),
    )])
}

pub(crate) fn builtin_reverse(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    Ok(vec![Value::string(s.chars().rev().collect())])
}

pub(crate) fn builtin_rep(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    let n = int_arg(args, 1, "n")?;
    let sep = if arg(args, 2).is_nil() {
        Cow::Borrowed("")
    } else {
        string_arg(args, 2, "sep")?
    };
    if n <= 0 {
        return Ok(vec![Value::str("")]);
    }
    const MAX_REP_COUNT: i64 = 10_000_000;
    if n > MAX_REP_COUNT {
        return Err(format!(
            "count exceeds maximum limit ({}) in 'string.rep'",
            MAX_REP_COUNT
        ));
    }
    if s.is_empty() && sep.is_empty() {
        return Ok(vec![Value::str("")]);
    }
    let total = (s.len() + sep.len()).saturating_mul(n as usize);
    if total > i32::MAX as usize || total > 100 * 1024 * 1024 {
        return Err("resulting string is too large".to_string());
    }
    let mut out = String::with_capacity(total);
    for index in 0..n {
        if index > 0 && !sep.is_empty() {
            out.push_str(&sep);
        }
        out.push_str(&s);
    }
    Ok(vec![Value::string(out)])
}

pub(crate) fn builtin_split(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    let sep = if arg(args, 1).is_nil() {
        Cow::Borrowed(",")
    } else {
        string_arg(args, 1, "separator")?
    };
    let parts: Vec<Value> = if sep.is_empty() {
        s.chars().map(|c| Value::string(c.to_string())).collect()
    } else {
        s.split(&*sep).map(Value::str).collect()
    };
    Ok(vec![crate::vm::libs::args::array(parts)])
}

// ---------------------------------------------------------------------------
// Lua pattern matching (port of lstrlib.c)
// ---------------------------------------------------------------------------

const MAX_CAPTURES: usize = 32;
const MAX_MATCH_DEPTH: usize = 200;
const CAP_UNFINISHED: isize = -1;
const CAP_POSITION: isize = -2;
const SPECIALS: &[u8] = b"^$*+?.([%-";

#[derive(Clone, Copy)]
struct Capture {
    init: usize,
    len: isize,
}

struct MatchState<'a> {
    src: &'a [u8],
    pat: &'a [u8],
    level: usize,
    depth: usize,
    capture: [Capture; MAX_CAPTURES],
}

impl<'a> MatchState<'a> {
    fn new(src: &'a [u8], pat: &'a [u8]) -> Self {
        Self {
            src,
            pat,
            level: 0,
            depth: 0,
            capture: [Capture { init: 0, len: 0 }; MAX_CAPTURES],
        }
    }

    fn reset(&mut self) {
        self.level = 0;
        self.depth = 0;
    }

    fn pat_at(&self, p: usize) -> u8 {
        self.pat.get(p).copied().unwrap_or(0)
    }

    fn class_end(&self, mut p: usize) -> Result<usize, String> {
        let c = self.pat[p];
        p += 1;
        if c == b'%' {
            if p >= self.pat.len() {
                return Err("malformed pattern (ends with '%')".to_string());
            }
            return Ok(p + 1);
        }
        if c == b'[' {
            if self.pat_at(p) == b'^' {
                p += 1;
            }
            loop {
                if p >= self.pat.len() {
                    return Err("malformed pattern (missing ']')".to_string());
                }
                let cc = self.pat[p];
                p += 1;
                if cc == b'%' && p < self.pat.len() {
                    p += 1;
                }
                if p >= self.pat.len() {
                    return Err("malformed pattern (missing ']')".to_string());
                }
                if self.pat[p] == b']' {
                    break;
                }
            }
            return Ok(p + 1);
        }
        Ok(p)
    }

    fn match_bracket_class(&self, c: u8, p: usize, ec: usize) -> bool {
        let mut p = p + 1;
        let mut sig = true;
        if self.pat[p] == b'^' {
            sig = false;
            p += 1;
        }
        while p < ec {
            if self.pat[p] == b'%' {
                p += 1;
                if match_class(c, self.pat[p]) {
                    return sig;
                }
                p += 1;
            } else if self.pat_at(p + 1) == b'-' && p + 2 < ec {
                if self.pat[p] <= c && c <= self.pat[p + 2] {
                    return sig;
                }
                p += 3;
            } else {
                if self.pat[p] == c {
                    return sig;
                }
                p += 1;
            }
        }
        !sig
    }

    fn single_match(&self, s: usize, p: usize, ep: usize) -> bool {
        if s >= self.src.len() {
            return false;
        }
        let c = self.src[s];
        match self.pat[p] {
            b'.' => true,
            b'%' => match_class(c, self.pat[p + 1]),
            b'[' => self.match_bracket_class(c, p, ep - 1),
            pc => pc == c,
        }
    }

    fn match_balance(&self, s: usize, p: usize) -> Result<Option<usize>, String> {
        if p + 1 >= self.pat.len() {
            return Err("malformed pattern (missing arguments to '%b')".to_string());
        }
        if s >= self.src.len() || self.src[s] != self.pat[p] {
            return Ok(None);
        }
        let open = self.pat[p];
        let close = self.pat[p + 1];
        let mut cont = 1;
        let mut s = s + 1;
        while s < self.src.len() {
            let c = self.src[s];
            if c == close {
                cont -= 1;
                if cont == 0 {
                    return Ok(Some(s + 1));
                }
            } else if c == open {
                cont += 1;
            }
            s += 1;
        }
        Ok(None)
    }

    fn max_expand(&mut self, s: usize, p: usize, ep: usize) -> Result<Option<usize>, String> {
        let mut i = 0usize;
        while self.single_match(s + i, p, ep) {
            i += 1;
        }
        loop {
            if let Some(res) = self.do_match(s + i, ep + 1)? {
                return Ok(Some(res));
            }
            if i == 0 {
                return Ok(None);
            }
            i -= 1;
        }
    }

    fn min_expand(&mut self, mut s: usize, p: usize, ep: usize) -> Result<Option<usize>, String> {
        loop {
            if let Some(res) = self.do_match(s, ep + 1)? {
                return Ok(Some(res));
            }
            if self.single_match(s, p, ep) {
                s += 1;
            } else {
                return Ok(None);
            }
        }
    }

    fn start_capture(&mut self, s: usize, p: usize, what: isize) -> Result<Option<usize>, String> {
        if self.level >= MAX_CAPTURES {
            return Err("too many captures".to_string());
        }
        self.capture[self.level] = Capture { init: s, len: what };
        self.level += 1;
        let res = self.do_match(s, p)?;
        if res.is_none() {
            self.level -= 1;
        }
        Ok(res)
    }

    fn end_capture(&mut self, s: usize, p: usize) -> Result<Option<usize>, String> {
        let l = self.capture_to_close()?;
        self.capture[l].len = (s - self.capture[l].init) as isize;
        let res = self.do_match(s, p)?;
        if res.is_none() {
            self.capture[l].len = CAP_UNFINISHED;
        }
        Ok(res)
    }

    fn capture_to_close(&self) -> Result<usize, String> {
        let mut level = self.level;
        while level > 0 {
            level -= 1;
            if self.capture[level].len == CAP_UNFINISHED {
                return Ok(level);
            }
        }
        Err("invalid pattern capture".to_string())
    }

    fn check_capture(&self, l: u8) -> Result<usize, String> {
        let index = l as isize - b'1' as isize;
        if index < 0
            || index as usize >= self.level
            || self.capture[index as usize].len == CAP_UNFINISHED
        {
            return Err(format!("invalid capture index %{}", index + 1));
        }
        Ok(index as usize)
    }

    fn match_capture(&self, s: usize, l: u8) -> Result<Option<usize>, String> {
        let l = self.check_capture(l)?;
        let len = self.capture[l].len as usize;
        let init = self.capture[l].init;
        if self.src.len() - s >= len && self.src[init..init + len] == self.src[s..s + len] {
            Ok(Some(s + len))
        } else {
            Ok(None)
        }
    }

    fn do_match(&mut self, mut s: usize, mut p: usize) -> Result<Option<usize>, String> {
        self.depth += 1;
        if self.depth > MAX_MATCH_DEPTH {
            return Err("pattern too complex".to_string());
        }
        let result = loop {
            if p >= self.pat.len() {
                break Some(s);
            }
            match self.pat[p] {
                b'(' => {
                    break if self.pat_at(p + 1) == b')' {
                        self.start_capture(s, p + 2, CAP_POSITION)?
                    } else {
                        self.start_capture(s, p + 1, CAP_UNFINISHED)?
                    };
                }
                b')' => break self.end_capture(s, p + 1)?,
                b'$' if p + 1 == self.pat.len() => {
                    break if s == self.src.len() { Some(s) } else { None };
                }
                b'%' if self.pat_at(p + 1) == b'b' => match self.match_balance(s, p + 2)? {
                    Some(next) => {
                        s = next;
                        p += 4;
                        continue;
                    }
                    None => break None,
                },
                b'%' if self.pat_at(p + 1) == b'f' => {
                    p += 2;
                    if self.pat_at(p) != b'[' {
                        return Err("missing '[' after '%f' in pattern".to_string());
                    }
                    let ep = self.class_end(p)?;
                    let previous = if s == 0 { 0 } else { self.src[s - 1] };
                    let current = self.src.get(s).copied().unwrap_or(0);
                    if !self.match_bracket_class(previous, p, ep - 1)
                        && self.match_bracket_class(current, p, ep - 1)
                    {
                        p = ep;
                        continue;
                    }
                    break None;
                }
                b'%' if self.pat_at(p + 1).is_ascii_digit() => {
                    match self.match_capture(s, self.pat[p + 1])? {
                        Some(next) => {
                            s = next;
                            p += 2;
                            continue;
                        }
                        None => break None,
                    }
                }
                _ => {
                    let ep = self.class_end(p)?;
                    let epc = self.pat_at(ep);
                    if !self.single_match(s, p, ep) {
                        if epc == b'*' || epc == b'?' || epc == b'-' {
                            p = ep + 1;
                            continue;
                        }
                        break None;
                    }
                    match epc {
                        b'?' => {
                            if let Some(res) = self.do_match(s + 1, ep + 1)? {
                                break Some(res);
                            }
                            p = ep + 1;
                            continue;
                        }
                        b'+' => break self.max_expand(s + 1, p, ep)?,
                        b'*' => break self.max_expand(s, p, ep)?,
                        b'-' => break self.min_expand(s, p, ep)?,
                        _ => {
                            s += 1;
                            p = ep;
                            continue;
                        }
                    }
                }
            }
        };
        self.depth -= 1;
        Ok(result)
    }

    fn get_capture(&self, i: usize, s: usize, e: usize) -> Result<Value, String> {
        if i >= self.level {
            if i == 0 {
                return Ok(bytes_to_string(&self.src[s..e]));
            }
            return Err(format!("invalid capture index %{}", i + 1));
        }
        let capture = self.capture[i];
        match capture.len {
            CAP_UNFINISHED => Err("unfinished capture".to_string()),
            CAP_POSITION => Ok(integer(capture.init as i64 + 1)),
            len => Ok(bytes_to_string(
                &self.src[capture.init..capture.init + len as usize],
            )),
        }
    }

    fn push_captures(&self, s: usize, e: usize, whole_if_none: bool) -> Result<Vec<Value>, String> {
        let count = if self.level == 0 && whole_if_none {
            1
        } else {
            self.level
        };
        (0..count).map(|i| self.get_capture(i, s, e)).collect()
    }
}

fn match_class(c: u8, cl: u8) -> bool {
    let result = match cl.to_ascii_lowercase() {
        b'a' => c.is_ascii_alphabetic(),
        b'c' => c.is_ascii_control(),
        b'd' => c.is_ascii_digit(),
        b'g' => c.is_ascii_graphic(),
        b'l' => c.is_ascii_lowercase(),
        b'p' => c.is_ascii_punctuation(),
        b's' => c.is_ascii_whitespace() || c == 0x0b,
        b'u' => c.is_ascii_uppercase(),
        b'w' => c.is_ascii_alphanumeric(),
        b'x' => c.is_ascii_hexdigit(),
        _ => return cl == c,
    };
    if cl.is_ascii_uppercase() {
        !result
    } else {
        result
    }
}

/// Shared implementation of `find` and `match`. Returns `[nil]` when there is
/// no match; otherwise `[start, end, captures...]` for `find` and the captures
/// (or whole match) for `match`.
fn find_aux(args: &[Value], find: bool) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    let pattern = string_arg(args, 1, "pattern")?;
    let src = s.as_bytes();
    let pat = pattern.as_bytes();
    let init = pos_relat_start(opt_int_arg(args, 2, "init", 1)?, src.len());
    if init > src.len() + 1 {
        return Ok(vec![Value::NIL]);
    }
    let plain = match arg(args, 3).view() {
        ValueView::Bool(value) => value,
        ValueView::Nil => false,
        _ => return Err("plain must be a boolean".to_string()),
    };
    if find && (plain || !pat.iter().any(|c| SPECIALS.contains(c))) {
        let haystack = &src[init - 1..];
        let found = if pat.is_empty() {
            Some(0)
        } else {
            haystack.windows(pat.len()).position(|window| window == pat)
        };
        return Ok(match found {
            Some(offset) => vec![
                integer((init + offset) as i64),
                integer((init + offset + pat.len() - 1) as i64),
            ],
            None => vec![Value::NIL],
        });
    }

    let anchor = pat.first() == Some(&b'^');
    let p = if anchor { 1 } else { 0 };
    let mut ms = MatchState::new(src, pat);
    let mut s1 = init - 1;
    loop {
        ms.reset();
        if let Some(e) = ms.do_match(s1, p)? {
            if find {
                let mut values = vec![integer(s1 as i64 + 1), integer(e as i64)];
                values.extend(ms.push_captures(s1, e, false)?);
                return Ok(values);
            }
            return ms.push_captures(s1, e, true);
        }
        s1 += 1;
        if anchor || s1 > src.len() {
            return Ok(vec![Value::NIL]);
        }
    }
}

pub(crate) fn builtin_find(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    find_aux(args, true)
}

pub(crate) fn builtin_match(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    find_aux(args, false)
}

// ---------------------------------------------------------------------------
// gsub and gmatch
// ---------------------------------------------------------------------------

fn bytes_value(bytes: Vec<u8>) -> Value {
    match String::from_utf8(bytes) {
        Ok(s) => Value::string(s),
        Err(err) => Value::string(String::from_utf8_lossy(err.as_bytes()).into_owned()),
    }
}

fn append_replacement(
    vm: &mut VM,
    ms: &MatchState,
    out: &mut Vec<u8>,
    s: usize,
    e: usize,
    replacement: &Value,
) -> Result<(), String> {
    let whole = &ms.src[s..e];
    let value = match replacement.view() {
        ValueView::String(template) => {
            let t = template.as_bytes();
            let mut i = 0;
            while i < t.len() {
                if t[i] != b'%' {
                    out.push(t[i]);
                    i += 1;
                    continue;
                }
                i += 1;
                match t.get(i) {
                    Some(b'%') => out.push(b'%'),
                    Some(b'0') => out.extend_from_slice(whole),
                    Some(d @ b'1'..=b'9') => {
                        let capture = ms.get_capture((d - b'1') as usize, s, e)?;
                        out.extend_from_slice(capture.to_string().as_bytes());
                    }
                    _ => return Err("invalid use of '%' in replacement string".to_string()),
                }
                i += 1;
            }
            return Ok(());
        }
        ValueView::Table(_) => {
            let key = ms.get_capture(0, s, e)?;
            vm.table_get(replacement, &key)?
        }
        _ => {
            let captures = ms.push_captures(s, e, true)?;
            vm.call_function(replacement.clone(), &captures)?
                .into_iter()
                .next()
                .unwrap_or(Value::NIL)
        }
    };
    match value.view() {
        ValueView::Nil | ValueView::Bool(false) => out.extend_from_slice(whole),
        ValueView::String(text) => out.extend_from_slice(text.as_bytes()),
        ValueView::Int(_) | ValueView::BigInt(_) | ValueView::Float(_) => {
            out.extend_from_slice(value.to_string().as_bytes())
        }
        _ => {
            return Err(format!(
                "invalid replacement value (a {})",
                value.type_name()
            ));
        }
    }
    Ok(())
}

/// `gsub(s, pattern, replacement, n?)`: every match (or the first `n`)
/// replaced by a template string, a table lookup or a function call.
pub(crate) fn builtin_gsub(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    let pattern = string_arg(args, 1, "pattern")?;
    let replacement = match args.get(2) {
        Some(v) if v.is_string() || v.is_table() || v.is_callable() => v.clone(),
        None => return Err("replacement must be provided".to_string()),
        Some(v) if v.is_nil() => return Err("replacement must be provided".to_string()),
        Some(_) => return Err("replacement must be a string, table or function".to_string()),
    };
    let max = match args.get(3) {
        None => i64::MAX,
        Some(v) if v.is_nil() => i64::MAX,
        _ => int_arg(args, 3, "n")?,
    };
    let src = s.as_bytes();
    let pat = pattern.as_bytes();
    let anchor = pat.first() == Some(&b'^');
    let p = usize::from(anchor);
    let mut ms = MatchState::new(src, pat);
    let mut out = Vec::with_capacity(src.len());
    let mut pos = 0usize;
    let mut last_match: Option<usize> = None;
    let mut count = 0i64;
    while count < max {
        ms.reset();
        match ms.do_match(pos, p)? {
            Some(e) if Some(e) != last_match => {
                count += 1;
                append_replacement(vm, &ms, &mut out, pos, e, &replacement)?;
                pos = e;
                last_match = Some(e);
            }
            _ if pos < src.len() => {
                out.push(src[pos]);
                pos += 1;
            }
            _ => break,
        }
        if anchor {
            break;
        }
    }
    out.extend_from_slice(&src[pos.min(src.len())..]);
    Ok(vec![bytes_value(out), Value::int(count)])
}

/// One step of a `gmatch` iterator. The iterator is a table whose fields hold
/// the subject, the pattern and how far matching has got.
fn gmatch_step(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(state) = args.first().and_then(Value::as_table) else {
        return Err("gmatch iterator called without its state".to_string());
    };
    let (subject, pattern, pos, last) = {
        let st = state.borrow();
        let s_val = st.get_str("s");
        let p_val = st.get_str("p");
        let pos_val = st.get_str("pos");
        let (Some(subject), Some(pattern), Some(pos)) =
            (s_val.to_str_ref(), p_val.to_str_ref(), pos_val.as_int())
        else {
            return Err("gmatch iterator state is corrupt".to_string());
        };
        let last = st.get_str("last").as_int().map(|i| i as usize);
        (subject, pattern, pos as usize, last)
    };
    let src = subject.as_bytes();
    let pat = pattern.as_bytes();
    let anchor = pat.first() == Some(&b'^');
    let p = usize::from(anchor);
    let mut ms = MatchState::new(src, pat);
    let mut start = pos;
    while start <= src.len() {
        ms.reset();
        if let Some(e) = ms.do_match(start, p)?
            && Some(e) != last
        {
            let next = if anchor { src.len() + 1 } else { e };
            let mut st = state.borrow_mut();
            st.set_str("pos", Value::from_usize(next));
            st.set_str("last", Value::from_usize(e));
            drop(st);
            return ms.push_captures(start, e, true);
        }
        if anchor {
            break;
        }
        start += 1;
    }
    state
        .borrow_mut()
        .set_str("pos", Value::from_usize(src.len() + 1));
    Ok(vec![Value::NIL])
}

/// `gmatch(s, pattern)`: an iterator over every match, yielding its captures
/// (or the whole match). Works in a generic `for` and when called directly.
pub(crate) fn builtin_gmatch(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    let pattern = string_arg(args, 1, "pattern")?;
    let mut meta = VmTable::new();
    meta.set_str("__call", crate::native!("gmatch iterator", gmatch_step));
    let mut state = VmTable::new();
    state.set_str("s", Value::string(s.into_owned()));
    state.set_str("p", Value::string(pattern.into_owned()));
    state.set_str("pos", Value::int(0));
    state.metatable = Some(Rc::new(RefCell::new(meta)));
    Ok(vec![Value::table(Rc::new(RefCell::new(state)))])
}

// ---------------------------------------------------------------------------
// small conveniences
// ---------------------------------------------------------------------------

/// `trim(s, chars?)`: `s` without leading and trailing whitespace, or without
/// any of the characters in `chars`.
pub(crate) fn builtin_trim(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    trim_with(args, true, true)
}

pub(crate) fn builtin_ltrim(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    trim_with(args, true, false)
}

pub(crate) fn builtin_rtrim(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    trim_with(args, false, true)
}

fn trim_with(args: &[Value], left: bool, right: bool) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    let chars = if arg(args, 1).is_nil() {
        None
    } else {
        Some(string_arg(args, 1, "chars")?)
    };
    let matches = |c: char| match &chars {
        Some(set) => set.contains(c),
        None => c.is_whitespace(),
    };
    let mut out: &str = &s;
    if left {
        out = out.trim_start_matches(matches);
    }
    if right {
        out = out.trim_end_matches(matches);
    }
    Ok(vec![Value::str(out)])
}

pub(crate) fn builtin_startswith(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    let prefix = string_arg(args, 1, "prefix")?;
    Ok(vec![Value::bool(s.starts_with(&*prefix))])
}

pub(crate) fn builtin_endswith(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = string_arg(args, 0, "s")?;
    let suffix = string_arg(args, 1, "suffix")?;
    Ok(vec![Value::bool(s.ends_with(&*suffix))])
}

pub(crate) fn builtin_len(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::from_usize(string_arg(args, 0, "s")?.len())])
}

// ---------------------------------------------------------------------------
// string.format
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FormatSpec {
    left: bool,
    plus: bool,
    space: bool,
    alt: bool,
    zero: bool,
    width: usize,
    precision: Option<usize>,
}

impl FormatSpec {
    /// Pad `body` (already carrying its own sign/prefix of `prefix_len` bytes)
    /// to the requested width.
    fn pad(&self, body: String, prefix_len: usize, numeric: bool) -> String {
        if body.chars().count() >= self.width {
            return body;
        }
        let fill = self.width - body.chars().count();
        if self.left {
            format!("{}{}", body, " ".repeat(fill))
        } else if self.zero && numeric {
            let (prefix, digits) = body.split_at(prefix_len);
            format!("{}{}{}", prefix, "0".repeat(fill), digits)
        } else {
            format!("{}{}", " ".repeat(fill), body)
        }
    }

    fn sign(&self, negative: bool) -> &'static str {
        if negative {
            "-"
        } else if self.plus {
            "+"
        } else if self.space {
            " "
        } else {
            ""
        }
    }
}

fn format_integer_arg(value: Value) -> Result<BigInt, String> {
    match value.view() {
        ValueView::Int(value) => Ok(BigInt::from(value)),
        ValueView::BigInt(value) => Ok(value.clone()),
        ValueView::Float(value) if value.is_finite() && value.fract() == 0.0 => {
            BigInt::from_f64(value)
                .ok_or_else(|| "number has no integer representation".to_string())
        }
        ValueView::Float(_) => Err("number has no integer representation".to_string()),
        _ => Err(format!("expected a number, got {}", value.type_name())),
    }
}

fn format_decimal(spec: &FormatSpec, value: BigInt) -> String {
    let negative = value.sign() == Sign::Minus;
    let mut digits = value.magnitude().to_string();
    if let Some(precision) = spec.precision {
        if digits.len() < precision {
            digits = format!("{}{}", "0".repeat(precision - digits.len()), digits);
        }
        if precision == 0 && value.is_zero() {
            digits.clear();
        }
    }
    let sign = spec.sign(negative);
    let zero = spec.zero && spec.precision.is_none();
    FormatSpec { zero, ..*spec }.pad(format!("{}{}", sign, digits), sign.len(), true)
}

fn format_unsigned(spec: &FormatSpec, value: BigInt, radix: u32, upper: bool) -> String {
    // Negative values wrap like a 64-bit unsigned integer, as in C/Lua.
    let value = if value.sign() == Sign::Minus {
        (BigInt::one() << 64u32) + value
    } else {
        value
    };
    let mut digits = value.to_str_radix(radix);
    if upper {
        digits = digits.to_uppercase();
    }
    if let Some(precision) = spec.precision {
        if digits.len() < precision {
            digits = format!("{}{}", "0".repeat(precision - digits.len()), digits);
        }
        if precision == 0 && value.is_zero() {
            digits.clear();
        }
    }
    let prefix = if spec.alt && !value.is_zero() {
        match (radix, upper) {
            (16, false) => "0x",
            (16, true) => "0X",
            (8, _) if !digits.starts_with('0') => "0",
            _ => "",
        }
    } else {
        ""
    };
    let zero = spec.zero && spec.precision.is_none();
    FormatSpec { zero, ..*spec }.pad(format!("{}{}", prefix, digits), prefix.len(), true)
}

fn exponent_form(value: f64, precision: usize, upper: bool) -> String {
    let formatted = format!("{:.*e}", precision, value);
    let (mantissa, exponent) = formatted
        .split_once('e')
        .expect("`{:e}` output has an exponent");
    let exponent: i32 = exponent.parse().expect("`{:e}` exponent is an integer");
    let e = if upper { 'E' } else { 'e' };
    format!(
        "{}{}{}{:02}",
        mantissa,
        e,
        if exponent < 0 { '-' } else { '+' },
        exponent.abs()
    )
}

fn strip_trailing_zeros(text: String) -> String {
    if let Some((mantissa, exponent)) = text.split_once(['e', 'E']) {
        let e = if text.contains('E') { "E" } else { "e" };
        return format!(
            "{}{}{}",
            strip_trailing_zeros(mantissa.to_string()),
            e,
            exponent
        );
    }
    if !text.contains('.') {
        return text;
    }
    let trimmed = text.trim_end_matches('0');
    trimmed.trim_end_matches('.').to_string()
}

fn hex_float(value: f64, precision: Option<usize>, upper: bool) -> String {
    let bits = value.to_bits();
    let exponent_bits = ((bits >> 52) & 0x7ff) as i32;
    let mut fraction = bits & ((1u64 << 52) - 1);
    let (lead, mut exponent) = if exponent_bits == 0 {
        if fraction == 0 {
            (0u64, 0)
        } else {
            (0u64, -1022)
        }
    } else {
        (1u64, exponent_bits - 1023)
    };
    let mut lead = lead;
    let mut hex = if let Some(precision) = precision.filter(|p| *p < 13) {
        // Round the 52-bit fraction to `precision` hex digits (half away from zero).
        let drop = 52 - precision * 4;
        let half = 1u64 << (drop - 1);
        fraction += half;
        if fraction >> 52 != 0 {
            lead += 1;
            fraction &= (1u64 << 52) - 1;
        }
        fraction >>= drop;
        if precision == 0 {
            String::new()
        } else {
            format!("{:0width$x}", fraction, width = precision)
        }
    } else {
        let mut text = format!("{:013x}", fraction);
        match precision {
            Some(precision) => text.push_str(&"0".repeat(precision - 13)),
            None => text = text.trim_end_matches('0').to_string(),
        }
        text
    };
    if lead == 2 {
        lead = 1;
        exponent += 1;
    }
    if upper {
        hex = hex.to_uppercase();
    }
    let dot = if hex.is_empty() { "" } else { "." };
    let (x, p) = if upper { ('X', 'P') } else { ('x', 'p') };
    format!("0{}{}{}{}{}{:+}", x, lead, dot, hex, p, exponent)
}

fn format_float(spec: &FormatSpec, value: f64, conv: u8) -> String {
    let negative = value.is_sign_negative() && !value.is_nan();
    let magnitude = value.abs();
    let upper = conv.is_ascii_uppercase();
    let sign = spec.sign(negative);
    if !value.is_finite() {
        let body = if value.is_nan() { "nan" } else { "inf" };
        let body = if upper {
            body.to_uppercase()
        } else {
            body.to_string()
        };
        return FormatSpec {
            zero: false,
            ..*spec
        }
        .pad(format!("{}{}", sign, body), sign.len(), false);
    }
    let body = match conv.to_ascii_lowercase() {
        b'f' => {
            let mut text = format!("{:.*}", spec.precision.unwrap_or(6), magnitude);
            if spec.alt && !text.contains('.') {
                text.push('.');
            }
            text
        }
        b'e' => exponent_form(magnitude, spec.precision.unwrap_or(6), upper),
        b'g' => {
            let precision = match spec.precision {
                Some(0) => 1,
                Some(p) => p,
                None => 6,
            };
            let exponent = if magnitude == 0.0 {
                0
            } else {
                exponent_form(magnitude, precision - 1, false)
                    .split_once('e')
                    .and_then(|(_, e)| e.parse::<i32>().ok())
                    .unwrap_or(0)
            };
            let text = if exponent < -4 || exponent >= precision as i32 {
                exponent_form(magnitude, precision - 1, upper)
            } else {
                format!(
                    "{:.*}",
                    (precision as i32 - 1 - exponent) as usize,
                    magnitude
                )
            };
            if spec.alt {
                text
            } else {
                strip_trailing_zeros(text)
            }
        }
        b'a' => hex_float(magnitude, spec.precision, upper),
        _ => unreachable!("only float conversions reach format_float"),
    };
    spec.pad(format!("{}{}", sign, body), sign.len(), true)
}

fn quote_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let chars: Vec<char> = s.chars().collect();
    for (index, c) in chars.iter().enumerate() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\0' => out.push_str("\\0"),
            c if c.is_ascii_control() => {
                // Use three digits when a digit follows, so the escape stays unambiguous.
                if chars
                    .get(index + 1)
                    .is_some_and(|next| next.is_ascii_digit())
                {
                    out.push_str(&format!("\\{:03}", *c as u32));
                } else {
                    out.push_str(&format!("\\{}", *c as u32));
                }
            }
            c => out.push(*c),
        }
    }
    out.push('"');
    out
}

fn format_quoted(value: Value) -> Result<String, String> {
    Ok(match value.view() {
        ValueView::String(s) => quote_string(&s),
        ValueView::Int(i) => i.to_string(),
        ValueView::BigInt(bi) => bi.to_string(),
        ValueView::Float(value) => {
            if value.is_infinite() {
                if value > 0.0 {
                    "1e9999".to_string()
                } else {
                    "-1e9999".to_string()
                }
            } else if value.is_nan() {
                "(0/0)".to_string()
            } else if value.fract() == 0.0 && value.abs() < 1e15 {
                format!("{}.0", value as i64)
            } else {
                let sign = if value < 0.0 { "-" } else { "" };
                format!("{}{}", sign, hex_float(value.abs(), None, false))
            }
        }
        ValueView::Nil => "nil".to_string(),
        ValueView::Bool(b) => b.to_string(),
        _ => return Err(format!("value has no literal form ({})", value.type_name())),
    })
}

pub(crate) fn builtin_format(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let formatstring = string_arg(args, 0, "formatstring")?;
    let fmt = formatstring.as_bytes();
    let mut out = String::with_capacity(fmt.len());
    let mut arg_index = 1usize;
    let mut i = 0usize;
    let mut literal_start = 0usize;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            i += 1;
            continue;
        }
        out.push_str(&String::from_utf8_lossy(&fmt[literal_start..i]));
        i += 1;
        if i >= fmt.len() {
            return Err("invalid conversion '%' to 'format'".to_string());
        }
        if fmt[i] == b'%' {
            out.push('%');
            i += 1;
            literal_start = i;
            continue;
        }
        let spec_start = i;
        let mut spec = FormatSpec::default();
        while i < fmt.len() && b"-+ #0".contains(&fmt[i]) {
            match fmt[i] {
                b'-' => spec.left = true,
                b'+' => spec.plus = true,
                b' ' => spec.space = true,
                b'#' => spec.alt = true,
                _ => spec.zero = true,
            }
            i += 1;
        }
        let mut width_digits = 0;
        while i < fmt.len() && fmt[i].is_ascii_digit() {
            spec.width = spec.width * 10 + (fmt[i] - b'0') as usize;
            width_digits += 1;
            i += 1;
        }
        if width_digits > 2 {
            return Err("invalid conversion (width too long)".to_string());
        }
        if i < fmt.len() && fmt[i] == b'.' {
            i += 1;
            let mut precision = 0usize;
            let mut precision_digits = 0;
            while i < fmt.len() && fmt[i].is_ascii_digit() {
                precision = precision * 10 + (fmt[i] - b'0') as usize;
                precision_digits += 1;
                i += 1;
            }
            if precision_digits > 2 {
                return Err("invalid conversion (precision too long)".to_string());
            }
            spec.precision = Some(precision);
        }
        if i >= fmt.len() {
            return Err(format!(
                "invalid conversion '%{}' to 'format'",
                String::from_utf8_lossy(&fmt[spec_start..])
            ));
        }
        let conv = fmt[i];
        i += 1;
        literal_start = i;

        let next_arg = |index: &mut usize| -> Result<Value, String> {
            let value = args
                .get(*index)
                .cloned()
                .ok_or_else(|| format!("bad argument #{} to 'format' (no value)", *index))?;
            *index += 1;
            Ok(value)
        };

        let piece = match conv {
            b'd' | b'i' => format_decimal(&spec, format_integer_arg(next_arg(&mut arg_index)?)?),
            b'u' => format_unsigned(
                &spec,
                format_integer_arg(next_arg(&mut arg_index)?)?,
                10,
                false,
            ),
            b'x' => format_unsigned(
                &spec,
                format_integer_arg(next_arg(&mut arg_index)?)?,
                16,
                false,
            ),
            b'X' => format_unsigned(
                &spec,
                format_integer_arg(next_arg(&mut arg_index)?)?,
                16,
                true,
            ),
            b'o' => format_unsigned(
                &spec,
                format_integer_arg(next_arg(&mut arg_index)?)?,
                8,
                false,
            ),
            b'c' => {
                let code = format_integer_arg(next_arg(&mut arg_index)?)?
                    .to_u32()
                    .and_then(char::from_u32)
                    .ok_or_else(|| "bad argument to '%c' (invalid character code)".to_string())?;
                spec.pad(code.to_string(), 0, false)
            }
            b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'a' | b'A' => {
                format_float(&spec, number(next_arg(&mut arg_index)?)?, conv)
            }
            b's' => {
                let value = next_arg(&mut arg_index)?;
                let text = crate::vm::builtins::global::tostring(vm, &value)?;
                let text = match spec.precision {
                    Some(precision) => text.chars().take(precision).collect(),
                    None => text,
                };
                spec.pad(text, 0, false)
            }
            b'q' => {
                if spec.width != 0 || spec.precision.is_some() || spec.left || spec.zero {
                    return Err("specifier '%q' cannot have modifiers".to_string());
                }
                format_quoted(next_arg(&mut arg_index)?)?
            }
            other => {
                return Err(format!(
                    "invalid conversion '%{}{}' to 'format'",
                    String::from_utf8_lossy(&fmt[spec_start..i - 1]),
                    other as char
                ));
            }
        };
        out.push_str(&piece);
    }
    out.push_str(&String::from_utf8_lossy(&fmt[literal_start..]));
    Ok(vec![Value::string(out)])
}

// ---------------------------------------------------------------------------
// string.pack / string.unpack / string.packsize
// ---------------------------------------------------------------------------

const NATIVE_INT_SIZE: usize = 4;
const NATIVE_LONG_SIZE: usize = 8;
const NATIVE_MAX_ALIGN: usize = 8;
const MAX_INT_SIZE: usize = 16;
/// The largest string `string.pack` builds.
const MAX_PACK_OUTPUT: usize = 100 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum PackKind {
    Int,
    Uint,
    Float,
    Double,
    Char,
    String,
    Zstr,
    Padding,
    PaddAlign,
    NoOp,
}

struct PackState {
    little: bool,
    max_align: usize,
}

impl PackState {
    fn new() -> Self {
        Self {
            little: true,
            max_align: 1,
        }
    }
}

fn read_size(fmt: &[u8], i: &mut usize, default: usize) -> usize {
    let mut size = 0usize;
    let mut digits = 0;
    while *i < fmt.len() && fmt[*i].is_ascii_digit() {
        size = size * 10 + (fmt[*i] - b'0') as usize;
        digits += 1;
        *i += 1;
    }
    if digits == 0 { default } else { size }
}

fn int_size(fmt: &[u8], i: &mut usize, default: usize) -> Result<usize, String> {
    let size = read_size(fmt, i, default);
    if !(1..=MAX_INT_SIZE).contains(&size) {
        return Err(format!(
            "integral size ({}) out of limits [1,{}]",
            size, MAX_INT_SIZE
        ));
    }
    Ok(size)
}

/// Read one option, returning its kind and size (0 for variable-size options).
fn read_option(
    state: &mut PackState,
    fmt: &[u8],
    i: &mut usize,
) -> Result<(PackKind, usize), String> {
    let opt = fmt[*i];
    *i += 1;
    Ok(match opt {
        b'b' => (PackKind::Int, 1),
        b'B' => (PackKind::Uint, 1),
        b'h' => (PackKind::Int, 2),
        b'H' => (PackKind::Uint, 2),
        b'l' => (PackKind::Int, NATIVE_LONG_SIZE),
        b'L' => (PackKind::Uint, NATIVE_LONG_SIZE),
        b'j' => (PackKind::Int, 8),
        b'J' => (PackKind::Uint, 8),
        b'T' => (PackKind::Uint, 8),
        b'f' => (PackKind::Float, 4),
        b'd' | b'n' => (PackKind::Double, 8),
        b'i' => (PackKind::Int, int_size(fmt, i, NATIVE_INT_SIZE)?),
        b'I' => (PackKind::Uint, int_size(fmt, i, NATIVE_INT_SIZE)?),
        b's' => (PackKind::String, int_size(fmt, i, 8)?),
        b'c' => {
            let size = read_size(fmt, i, usize::MAX);
            if size == usize::MAX {
                return Err("missing size for format option 'c'".to_string());
            }
            (PackKind::Char, size)
        }
        b'z' => (PackKind::Zstr, 0),
        b'x' => (PackKind::Padding, 1),
        b'X' => (PackKind::PaddAlign, 0),
        b' ' => (PackKind::NoOp, 0),
        b'<' => {
            state.little = true;
            (PackKind::NoOp, 0)
        }
        b'>' => {
            state.little = false;
            (PackKind::NoOp, 0)
        }
        b'=' => {
            state.little = true;
            (PackKind::NoOp, 0)
        }
        b'!' => {
            state.max_align = int_size(fmt, i, NATIVE_MAX_ALIGN)?;
            (PackKind::NoOp, 0)
        }
        other => return Err(format!("invalid format option '{}'", other as char)),
    })
}

/// Read an option plus the padding needed to align it at `total`.
fn read_option_aligned(
    state: &mut PackState,
    fmt: &[u8],
    i: &mut usize,
    total: usize,
) -> Result<(PackKind, usize, usize), String> {
    let (mut kind, mut size) = read_option(state, fmt, i)?;
    let mut align = size;
    if kind == PackKind::PaddAlign {
        if *i >= fmt.len() {
            return Err("invalid next option for option 'X'".to_string());
        }
        let (next_kind, next_size) = read_option(state, fmt, i)?;
        align = next_size;
        if next_kind == PackKind::Char || align == 0 {
            return Err("invalid next option for option 'X'".to_string());
        }
        kind = PackKind::PaddAlign;
        size = 0;
    }
    let padding = if align <= 1
        || matches!(
            kind,
            PackKind::Char | PackKind::String | PackKind::Zstr | PackKind::Padding | PackKind::NoOp
        ) {
        0
    } else {
        let align = align.min(state.max_align);
        if !align.is_power_of_two() {
            return Err("format asks for alignment not power of 2".to_string());
        }
        (align - (total & (align - 1))) & (align - 1)
    };
    Ok((kind, size, padding))
}

fn pack_int(
    out: &mut Vec<u8>,
    value: &BigInt,
    size: usize,
    little: bool,
    signed: bool,
) -> Result<(), String> {
    let bytes = value.to_signed_bytes_le();
    let negative = value.sign() == Sign::Minus;
    if signed {
        let bits = size * 8;
        let min = -(BigInt::one() << (bits - 1));
        let max = (BigInt::one() << (bits - 1)) - 1;
        if *value < min || *value > max {
            return Err(format!(
                "integer overflow packing {} into {} byte(s)",
                value, size
            ));
        }
    } else {
        let max = (BigInt::one() << (size * 8)) - 1;
        if (negative && *value < -(BigInt::one() << (size * 8 - 1))) || *value > max {
            return Err(format!(
                "unsigned overflow packing {} into {} byte(s)",
                value, size
            ));
        }
    }
    let fill = if negative { 0xff } else { 0x00 };
    let mut buffer: Vec<u8> = bytes.iter().copied().take(size).collect();
    while buffer.len() < size {
        buffer.push(fill);
    }
    if !little {
        buffer.reverse();
    }
    out.extend(buffer);
    Ok(())
}

fn unpack_int(bytes: &[u8], little: bool, signed: bool) -> BigInt {
    let mut buffer = bytes.to_vec();
    if !little {
        buffer.reverse();
    }
    if signed {
        BigInt::from_signed_bytes_le(&buffer)
    } else {
        BigInt::from_bytes_le(Sign::Plus, &buffer)
    }
}

fn bytes_to_latin1(bytes: &[u8]) -> Value {
    Value::string(bytes.iter().map(|b| *b as char).collect())
}

fn latin1_to_bytes(s: &str) -> Result<Vec<u8>, String> {
    s.chars()
        .map(|c| {
            u8::try_from(c as u32)
                .map_err(|_| "packed data must only contain byte characters (0-255)".to_string())
        })
        .collect()
}

pub(crate) fn builtin_pack(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let format = string_arg(args, 0, "format")?;
    let fmt = format.as_bytes();
    let mut state = PackState::new();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut arg_index = 1usize;
    while i < fmt.len() {
        let (kind, size, padding) = read_option_aligned(&mut state, fmt, &mut i, out.len())?;
        out.extend(std::iter::repeat_n(0u8, padding));
        let next = |index: &mut usize| -> Result<Value, String> {
            let value = args
                .get(*index)
                .cloned()
                .ok_or_else(|| format!("bad argument #{} to 'pack' (no value)", *index))?;
            *index += 1;
            Ok(value)
        };
        match kind {
            PackKind::Int | PackKind::Uint => {
                let value = format_integer_arg(next(&mut arg_index)?)?;
                pack_int(&mut out, &value, size, state.little, kind == PackKind::Int)?;
            }
            PackKind::Float => {
                let value = number(next(&mut arg_index)?)? as f32;
                let bytes = if state.little {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                };
                out.extend(bytes);
            }
            PackKind::Double => {
                let value = number(next(&mut arg_index)?)?;
                let bytes = if state.little {
                    value.to_le_bytes()
                } else {
                    value.to_be_bytes()
                };
                out.extend(bytes);
            }
            PackKind::Char => {
                let value = next(&mut arg_index)?;
                let value = require_string(&value)?;
                if value.len() > size {
                    return Err("string longer than given size".to_string());
                }
                // `c` takes any size, so cap what the zero padding can
                // allocate (the same limit as `string.rep`).
                if out.len().saturating_add(size) > MAX_PACK_OUTPUT {
                    return Err("packed result is too large".to_string());
                }
                out.extend(value.as_bytes());
                out.extend(std::iter::repeat_n(0u8, size - value.len()));
            }
            PackKind::String => {
                let value = next(&mut arg_index)?;
                let value = require_string(&value)?;
                if size < 8 && value.len() >= 1usize << (size * 8) {
                    return Err("string length does not fit in given size".to_string());
                }
                pack_int(
                    &mut out,
                    &BigInt::from(value.len()),
                    size,
                    state.little,
                    false,
                )?;
                out.extend(value.as_bytes());
            }
            PackKind::Zstr => {
                let value = next(&mut arg_index)?;
                let value = require_string(&value)?;
                if value.as_bytes().contains(&0) {
                    return Err("string contains zeros".to_string());
                }
                out.extend(value.as_bytes());
                out.push(0);
            }
            PackKind::Padding => out.push(0),
            PackKind::PaddAlign | PackKind::NoOp => {}
        }
    }
    Ok(vec![bytes_to_latin1(&out)])
}

pub(crate) fn builtin_packsize(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let format = string_arg(args, 0, "format")?;
    let fmt = format.as_bytes();
    let mut state = PackState::new();
    let mut total = 0usize;
    let mut i = 0usize;
    while i < fmt.len() {
        let (kind, size, padding) = read_option_aligned(&mut state, fmt, &mut i, total)?;
        if matches!(kind, PackKind::String | PackKind::Zstr) {
            return Err("variable-length format".to_string());
        }
        total += padding + size;
    }
    Ok(vec![integer(total as i64)])
}

pub(crate) fn builtin_unpack(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let format = string_arg(args, 0, "format")?;
    let data = latin1_to_bytes(&string_arg(args, 1, "data")?)?;
    let fmt = format.as_bytes();
    let mut pos = pos_relat_start(opt_int_arg(args, 2, "readStart", 1)?, data.len()) - 1;
    if pos > data.len() {
        return Err("initial position out of string".to_string());
    }
    let mut state = PackState::new();
    let mut values = Vec::new();
    let mut i = 0usize;
    while i < fmt.len() {
        let (kind, size, padding) = read_option_aligned(&mut state, fmt, &mut i, pos)?;
        if padding + size > data.len() - pos {
            return Err("data string too short".to_string());
        }
        pos += padding;
        match kind {
            PackKind::Int | PackKind::Uint => {
                values.push(Value::from_bigint(unpack_int(
                    &data[pos..pos + size],
                    state.little,
                    kind == PackKind::Int,
                )));
            }
            PackKind::Float => {
                let bytes: [u8; 4] = data[pos..pos + 4].try_into().expect("slice is 4 bytes");
                let value = if state.little {
                    f32::from_le_bytes(bytes)
                } else {
                    f32::from_be_bytes(bytes)
                };
                values.push(Value::float(value as f64));
            }
            PackKind::Double => {
                let bytes: [u8; 8] = data[pos..pos + 8].try_into().expect("slice is 8 bytes");
                let value = if state.little {
                    f64::from_le_bytes(bytes)
                } else {
                    f64::from_be_bytes(bytes)
                };
                values.push(Value::float(value));
            }
            PackKind::Char => values.push(bytes_to_string(&data[pos..pos + size])),
            PackKind::String => {
                let len = unpack_int(&data[pos..pos + size], state.little, false)
                    .to_usize()
                    .ok_or_else(|| "data string too short".to_string())?;
                if len > data.len() - pos - size {
                    return Err("data string too short".to_string());
                }
                values.push(bytes_to_string(&data[pos + size..pos + size + len]));
                pos += len;
            }
            PackKind::Zstr => {
                let len = data[pos..]
                    .iter()
                    .position(|b| *b == 0)
                    .ok_or_else(|| "unfinished string for format 'z'".to_string())?;
                values.push(bytes_to_string(&data[pos..pos + len]));
                pos += len + 1;
            }
            PackKind::Padding | PackKind::PaddAlign | PackKind::NoOp => {}
        }
        pos += size;
    }
    values.push(integer(pos as i64 + 1));
    Ok(values)
}
