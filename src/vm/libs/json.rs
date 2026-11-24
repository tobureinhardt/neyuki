//! The `json` library.
//!
//! `encode` turns nil, booleans, numbers, strings and tables into JSON text:
//! a table with only an array part becomes an array, anything else an
//! object with its keys sorted. `decode` parses strict JSON (RFC 8259);
//! integers stay exact, other numbers become floats.

use std::cell::RefCell;
use std::rc::Rc;

use num_bigint::BigInt;

use crate::vm::libs::args::table_arg;
use crate::vm::machine::VM;
use crate::vm::value::{StrRef, Value, ValueView, VmTable};

const MAX_DEPTH: usize = 256;

/// The longest text `encode` produces. A table that shares a subtable under
/// several keys, level after level, encodes to exponentially much text
/// while staying under the depth limit.
const MAX_OUTPUT: usize = 64 * 1024 * 1024;

/// Marks a table in its metatable as one `encode` writes as an array.
pub(crate) const ARRAY_MARK: &str = "__jsonarray";

pub fn create_json_lib() -> Value {
    let mut table = VmTable::new();
    table.set_str("encode", crate::native!("json.encode", json_encode));
    table.set_str("decode", crate::native!("json.decode", json_decode));
    table.set_str("array", crate::native!("json.array", json_array));
    table.set_str("null", null_sentinel());
    crate::vm::libs::args::frozen(table)
}

thread_local! {
    static NULL: Rc<RefCell<VmTable>> = {
        let mut meta = VmTable::new();
        meta.set_str("__tostring", crate::native!("json.null", null_tostring));
        let mut null = VmTable::new();
        null.metatable = Some(Rc::new(RefCell::new(meta)));
        null.frozen = true;
        Rc::new(RefCell::new(null))
    };
}

/// `json.null`: a value that encodes as `null`, and what `decode` produces
/// for `null` with `keepNull`, so the key stays present.
pub(crate) fn null_sentinel() -> Value {
    NULL.with(|n| Value::table(n.clone()))
}

pub(crate) fn is_null(value: &Value) -> bool {
    value
        .table_ptr()
        .is_some_and(|p| NULL.with(|n| std::ptr::eq(p, Rc::as_ptr(n))))
}

fn null_tostring(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::str("null")])
}

/// `json.array(t?)`: marks `t` (or a new table) so it always encodes as an
/// array, even when empty.
fn json_array(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let table = match args.first() {
        None => Rc::new(RefCell::new(VmTable::new())),
        Some(v) if v.is_nil() => Rc::new(RefCell::new(VmTable::new())),
        _ => table_arg(args, 0, "t")?.to_rc(),
    };
    let meta = table.borrow().metatable.clone();
    let meta = meta.unwrap_or_else(|| {
        let meta = Rc::new(RefCell::new(VmTable::new()));
        table.borrow_mut().metatable = Some(meta.clone());
        meta
    });
    meta.borrow_mut().set_str(ARRAY_MARK, Value::TRUE);
    Ok(vec![Value::table(table)])
}

// ---------------------------------------------------------------------------
// encode
// ---------------------------------------------------------------------------

struct Encoder<'a> {
    vm: &'a mut VM,
    out: String,
    indent: String,
    sort_keys: bool,
    /// Tables being encoded, to reject cycles.
    open: Vec<*const RefCell<VmTable>>,
}

/// `json.encode(value, options?)` with `indent` (a number of spaces or a
/// string; compact by default) and `sortKeys` (true by default).
fn json_encode(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let value = args.first().cloned().unwrap_or(Value::NIL);
    let (indent, sort_keys) = match args.get(1).map(Value::view) {
        None | Some(ValueView::Nil) => (String::new(), true),
        Some(ValueView::Table(opts)) => {
            let opts = opts.borrow();
            let indent = match opts.get_str("indent").view() {
                ValueView::Nil => String::new(),
                ValueView::Int(n) if (0..=16).contains(&n) => " ".repeat(n as usize),
                ValueView::String(s) => s.to_string(),
                _ => return Err("indent must be a number from 0 to 16 or a string".to_string()),
            };
            let sort_keys = opts.get_str("sortKeys").as_bool() != Some(false);
            (indent, sort_keys)
        }
        Some(_) => {
            return Err(format!(
                "options must be a table, got {}",
                args[1].type_name()
            ));
        }
    };
    let mut encoder = Encoder {
        vm,
        out: String::new(),
        indent,
        sort_keys,
        open: Vec::new(),
    };
    encoder.value(&value, 0)?;
    Ok(vec![Value::string(encoder.out)])
}

pub(crate) fn format_float(f: f64) -> Option<String> {
    if !f.is_finite() {
        return None;
    }
    // Debug formatting is the shortest text that reads back to the same
    // float, keeps a fraction or exponent (`1.0`, `1e300`) so the value
    // stays a float, and never prints hundreds of digits.
    Some(format!("{:?}", f))
}

impl Encoder<'_> {
    fn newline(&mut self, depth: usize) {
        if !self.indent.is_empty() {
            self.out.push('\n');
            for _ in 0..depth {
                self.out.push_str(&self.indent);
            }
        }
    }

    fn value(&mut self, value: &Value, depth: usize) -> Result<(), String> {
        if depth >= MAX_DEPTH {
            return Err(format!("nesting is deeper than {} levels", MAX_DEPTH));
        }
        if self.out.len() > MAX_OUTPUT {
            return Err(format!(
                "output exceeds the {} MiB limit",
                MAX_OUTPUT / (1024 * 1024)
            ));
        }
        match value.view() {
            ValueView::Nil => self.out.push_str("null"),
            ValueView::Bool(b) => self.out.push_str(if b { "true" } else { "false" }),
            ValueView::Int(i) => self.out.push_str(&i.to_string()),
            ValueView::BigInt(i) => self.out.push_str(&i.to_string()),
            ValueView::Float(f) => match format_float(f) {
                Some(text) => self.out.push_str(&text),
                None => return Err(format!("cannot encode {} as JSON", f)),
            },
            ValueView::String(s) => write_string(&mut self.out, &s),
            ValueView::Table(_) if is_null(value) => self.out.push_str("null"),
            ValueView::Table(t) => {
                let custom = t.borrow().metamethod("__tojson");
                if let Some(handler) = custom {
                    let result = self
                        .vm
                        .call_function(handler, std::slice::from_ref(value))?;
                    let result = result.into_iter().next().unwrap_or(Value::NIL);
                    if result
                        .table_ptr()
                        .is_some_and(|p| std::ptr::eq(p, t.as_ptr()))
                    {
                        return Err("__tojson returned its own table".to_string());
                    }
                    return self.value(&result, depth + 1);
                }
                let ptr = t.as_ptr();
                if self.open.contains(&ptr) {
                    return Err("cannot encode a table that contains itself".to_string());
                }
                self.open.push(ptr);
                let result = self.table(&t, depth);
                self.open.pop();
                result?;
            }
            _ => {
                return Err(format!("cannot encode a {} as JSON", value.type_name()));
            }
        }
        Ok(())
    }

    fn table(&mut self, t: &RefCell<VmTable>, depth: usize) -> Result<(), String> {
        let (is_array, items, entries) = {
            let tbl = t.borrow();
            let marked = tbl
                .metatable
                .as_ref()
                .is_some_and(|m| m.borrow().get_str(ARRAY_MARK).is_truthy());
            let has_keys = !tbl.fields.is_empty() || tbl.general.is_some();
            if (marked || !tbl.array.is_empty()) && !has_keys {
                (true, tbl.array.clone(), Vec::new())
            } else {
                let entries = crate::vm::libs::table::entries(&tbl);
                (false, Vec::new(), entries)
            }
        };
        if is_array {
            self.out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    self.out.push(',');
                }
                self.newline(depth + 1);
                self.value(item, depth + 1)?;
            }
            if !items.is_empty() {
                self.newline(depth);
            }
            self.out.push(']');
            return Ok(());
        }
        let mut pairs = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let key = match key.view() {
                ValueView::String(s) => s.to_string(),
                ValueView::Int(_) | ValueView::BigInt(_) => key.to_string(),
                ValueView::Float(f) => format_float(f).unwrap_or_default(),
                _ => {
                    return Err(format!(
                        "cannot encode a table key of type {} as JSON",
                        key.type_name()
                    ));
                }
            };
            pairs.push((key, value));
        }
        if self.sort_keys {
            pairs.sort_by(|a, b| a.0.cmp(&b.0));
        }
        self.out.push('{');
        let separator = if self.indent.is_empty() { ":" } else { ": " };
        for (i, (key, value)) in pairs.iter().enumerate() {
            if i > 0 {
                self.out.push(',');
            }
            self.newline(depth + 1);
            write_string(&mut self.out, key);
            self.out.push_str(separator);
            self.value(value, depth + 1)?;
        }
        if !pairs.is_empty() {
            self.newline(depth);
        }
        self.out.push('}');
        Ok(())
    }
}

pub(crate) fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

// ---------------------------------------------------------------------------
// decode
// ---------------------------------------------------------------------------

/// `json.decode(text, options?)`. With `keepNull`, `null` becomes
/// `json.null` instead of nil, so object keys holding it stay present.
fn json_decode(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let text = match args.first() {
        Some(v) if v.is_string() => v.as_str().unwrap(),
        Some(other) => {
            return Err(format!("text must be a string, got {}", other.type_name()));
        }
        None => return Err("text must be provided".to_string()),
    };
    let keep_null = match args.get(1).and_then(Value::as_table) {
        Some(opts) => opts.borrow().get_str("keepNull").is_truthy(),
        _ => false,
    };
    Ok(vec![decode_with(text, keep_null)?])
}

/// Parses a whole JSON text.
pub fn decode_from_str(text: &str) -> Result<Value, String> {
    decode_with(text, false)
}

fn decode_with(text: &str, keep_null: bool) -> Result<Value, String> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
        depth: 0,
        keep_null,
    };
    parser.skip_whitespace();
    let value = parser.value()?;
    parser.skip_whitespace();
    if parser.pos < parser.bytes.len() {
        return Err(parser.error("unexpected data after the value"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
    keep_null: bool,
}

impl Parser<'_> {
    fn error(&self, message: &str) -> String {
        let before = &self.bytes[..self.pos.min(self.bytes.len())];
        let line = before.iter().filter(|b| **b == b'\n').count() + 1;
        let column = before.iter().rev().take_while(|b| **b != b'\n').count() + 1;
        format!(
            "invalid JSON at line {}, column {}: {}",
            line, column, message
        )
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect_literal(&mut self, word: &str, value: Value) -> Result<Value, String> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.error("unknown literal"))
        }
    }

    fn value(&mut self) -> Result<Value, String> {
        match self.peek() {
            None => Err(self.error("unexpected end of input")),
            Some(b'n') => {
                let null = if self.keep_null {
                    null_sentinel()
                } else {
                    Value::NIL
                };
                self.expect_literal("null", null)
            }
            Some(b't') => self.expect_literal("true", Value::TRUE),
            Some(b'f') => self.expect_literal("false", Value::FALSE),
            Some(b'"') => self.string().map(Value::string),
            Some(b'[') => self.nested(Self::array),
            Some(b'{') => self.nested(Self::object),
            Some(b'-' | b'0'..=b'9') => self.number(),
            Some(_) => Err(self.error("expected a value")),
        }
    }

    fn nested(&mut self, parse: fn(&mut Self) -> Result<Value, String>) -> Result<Value, String> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error("nesting is too deep"));
        }
        self.depth += 1;
        let result = parse(self);
        self.depth -= 1;
        result
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Value, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => self.pos += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.error("expected a digit")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            is_float = true;
            if self.digits() == 0 {
                return Err(self.error("expected a digit after '.'"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            is_float = true;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.error("expected a digit in the exponent"));
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or("0");
        if is_float {
            text.parse::<f64>()
                .map(Value::float)
                .map_err(|_| self.error("invalid number"))
        } else if let Ok(i) = text.parse::<i64>() {
            Ok(Value::int(i))
        } else {
            text.parse::<BigInt>()
                .map(Value::from_bigint)
                .map_err(|_| self.error("invalid number"))
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or_else(|| self.error("unfinished \\u escape"))?;
        let text = std::str::from_utf8(digits).map_err(|_| self.error("invalid \\u escape"))?;
        let code = u32::from_str_radix(text, 16).map_err(|_| self.error("invalid \\u escape"))?;
        self.pos += 4;
        Ok(code)
    }

    fn string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while let Some(b) = self.peek() {
                if b == b'"' || b == b'\\' || b < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            // The input is a Rust string, so any run between ASCII
            // delimiters is valid UTF-8.
            out.push_str(std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or_default());
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.pos += 1;
                    let escape = self.peek().ok_or_else(|| self.error("unfinished escape"))?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let high = self.hex4()?;
                            let code = if (0xD800..0xDC00).contains(&high) {
                                if !self.bytes[self.pos..].starts_with(b"\\u") {
                                    return Err(self.error("unpaired surrogate"));
                                }
                                self.pos += 2;
                                let low = self.hex4()?;
                                if !(0xDC00..0xE000).contains(&low) {
                                    return Err(self.error("unpaired surrogate"));
                                }
                                0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00)
                            } else {
                                high
                            };
                            out.push(
                                char::from_u32(code)
                                    .ok_or_else(|| self.error("unpaired surrogate"))?,
                            );
                        }
                        _ => return Err(self.error("invalid escape")),
                    }
                }
                Some(_) => return Err(self.error("control character in string")),
                None => return Err(self.error("unfinished string")),
            }
        }
    }

    fn array(&mut self) -> Result<Value, String> {
        self.pos += 1;
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
        } else {
            loop {
                self.skip_whitespace();
                items.push(self.value()?);
                self.skip_whitespace();
                match self.peek() {
                    Some(b',') => self.pos += 1,
                    Some(b']') => {
                        self.pos += 1;
                        break;
                    }
                    _ => return Err(self.error("expected ',' or ']'")),
                }
            }
        }
        // Trailing nulls would leave the array with a nil end.
        while items.last().is_some_and(Value::is_nil) {
            items.pop();
        }
        let mut table = VmTable::new();
        table.array = items;
        Ok(Value::table(Rc::new(RefCell::new(table))))
    }

    fn object(&mut self) -> Result<Value, String> {
        self.pos += 1;
        let mut table = VmTable::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::table(Rc::new(RefCell::new(table))));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a string key"));
            }
            let key = self.string()?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("expected ':'"));
            }
            self.pos += 1;
            self.skip_whitespace();
            let value = self.value()?;
            if !value.is_nil() {
                table.fields.insert(StrRef::from(key), value);
            }
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
        Ok(Value::table(Rc::new(RefCell::new(table))))
    }
}
