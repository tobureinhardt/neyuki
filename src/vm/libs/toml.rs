//! Native TOML reading and writing backing `@neyuki/toml`.
//!
//! `decode` parses TOML 1.1 (and so 1.0) into tables: integers stay exact,
//! floats include `inf` and `nan`, and dates and times become RFC 3339
//! strings. `encode` writes a table back as TOML with its keys sorted:
//! plain values first, then one `[section]` per subtable and one
//! `[[section]]` per entry of an array of tables. `lib/toml.nyk` checks the
//! arguments.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::vm::libs::args::str_arg;
use crate::vm::machine::VM;
use crate::vm::value::{NativeDef, StrRef, Value, ValueView, VmTable};

/// How deeply arrays and inline tables may nest, and how many parts a
/// dotted key or a header may have, so a hostile document cannot build a
/// tree deep enough to overflow the stack.
const MAX_DEPTH: usize = 128;

/// The longest text `encode` produces, as for `json.encode`.
const MAX_OUTPUT: usize = 64 * 1024 * 1024;

pub(crate) const NATIVES: &[NativeDef] = &[
    NativeDef {
        name: "__toml_decode",
        func: builtin_decode,
    },
    NativeDef {
        name: "__toml_encode",
        func: builtin_encode,
    },
];

/// `__toml_decode(text)`: the document as a table.
fn builtin_decode(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let text = str_arg(args, 0, "text")?;
    let root = decode(text)?;
    Ok(vec![table_value(vm, root)])
}

/// `__toml_encode(value)`: a table as TOML text.
fn builtin_encode(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let _ = crate::vm::libs::args::table_arg(args, 0, "value")?;
    let mut root_val = args[0].clone();
    vm.beat_escape_if_needed(&mut root_val);
    let root = root_val
        .as_table()
        .expect("promoted TOML root must be Old")
        .to_rc();
    let mut encoder = Encoder {
        out: String::new(),
        open: Vec::new(),
    };
    encoder.section(&root, &mut Vec::new(), 0, Header::None)?;
    Ok(vec![Value::string(encoder.out)])
}

// ---------------------------------------------------------------------------
// decode
// ---------------------------------------------------------------------------

/// A parsed value, before it becomes a VM value. Tables remember how they
/// were made, which decides whether a later header or dotted key may add to
/// them.
enum Node {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    /// A date, time or date-time, normalized to RFC 3339.
    Datetime(String),
    Array(Vec<Node>),
    Table(Table),
    /// The entries of `[[name]]` sections.
    TableArray(Vec<Table>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Made on the way to a deeper header, such as `a` in `[a.b]`. Its own
    /// header may still define it once.
    Implicit,
    /// Defined by its own `[header]`, or an entry of `[[header]]`.
    Header,
    /// Made by a dotted key, such as `a` in `a.b = 1`.
    Dotted,
    /// Written inline as `{ ... }`, so closed to everything else.
    Inline,
}

struct Table {
    entries: HashMap<String, Node>,
    kind: Kind,
}

impl Table {
    fn new(kind: Kind) -> Self {
        Table {
            entries: HashMap::new(),
            kind,
        }
    }
}

/// Parses a whole TOML document.
fn decode(text: &str) -> Result<Table, String> {
    let mut parser = Parser {
        text,
        bytes: text.as_bytes(),
        pos: 0,
        depth: 0,
    };
    parser.document()
}

fn node_value(vm: &mut VM, node: Node) -> Value {
    match node {
        Node::String(s) | Node::Datetime(s) => Value::string(s),
        Node::Integer(i) => Value::int(i),
        Node::Float(f) => Value::float(f),
        Node::Boolean(b) => Value::bool(b),
        Node::Array(items) => {
            let vals: Vec<_> = items.into_iter().map(|n| node_value(vm, n)).collect();
            vm.alloc_array(vals)
        }
        Node::Table(table) => table_value(vm, table),
        Node::TableArray(tables) => {
            let vals: Vec<_> = tables.into_iter().map(|t| table_value(vm, t)).collect();
            vm.alloc_array(vals)
        }
    }
}

fn table_value(vm: &mut VM, table: Table) -> Value {
    let mut out = VmTable::new();
    for (key, node) in table.entries {
        out.fields.insert(StrRef::from(key), node_value(vm, node));
    }
    vm.alloc_table(out)
}

/// Steps from `table` into `key` for a header, making the table when it is
/// missing. An array of tables steps into its last entry.
fn header_step<'t>(table: &'t mut Table, key: &str) -> Result<&'t mut Table, String> {
    let node = table
        .entries
        .entry(key.to_string())
        .or_insert_with(|| Node::Table(Table::new(Kind::Implicit)));
    match node {
        Node::Table(t) if t.kind != Kind::Inline => Ok(t),
        Node::TableArray(tables) => tables
            .last_mut()
            .ok_or_else(|| format!("`{}` is an empty array of tables", key)),
        _ => Err(format!("`{}` is already defined as a value", key)),
    }
}

/// The table a section's keys go into.
fn section_table<'t>(root: &'t mut Table, path: &[String]) -> Result<&'t mut Table, String> {
    let mut table = root;
    for key in path {
        table = header_step(table, key)?;
    }
    Ok(table)
}

/// Defines the table of a `[path]` header, or a new entry of `[[path]]`.
fn open_section(root: &mut Table, path: &[String], array: bool) -> Result<(), String> {
    let Some((last, prefix)) = path.split_last() else {
        return Err("expected a key".to_string());
    };
    let table = section_table(root, prefix)?;
    let duplicate = || format!("`{}` is defined more than once", last);
    if array {
        match table.entries.get_mut(last) {
            None => {
                let entry = Node::TableArray(vec![Table::new(Kind::Header)]);
                table.entries.insert(last.clone(), entry);
            }
            Some(Node::TableArray(tables)) => tables.push(Table::new(Kind::Header)),
            Some(_) => return Err(duplicate()),
        }
    } else {
        match table.entries.get_mut(last) {
            None => {
                table
                    .entries
                    .insert(last.clone(), Node::Table(Table::new(Kind::Header)));
            }
            Some(Node::Table(t)) if t.kind == Kind::Implicit => t.kind = Kind::Header,
            Some(_) => return Err(duplicate()),
        }
    }
    Ok(())
}

/// Stores `keys = value` in `table`, making the tables of a dotted key.
fn insert_value(table: &mut Table, keys: &[String], value: Node) -> Result<(), String> {
    let Some((last, prefix)) = keys.split_last() else {
        return Err("expected a key".to_string());
    };
    let mut table = table;
    for key in prefix {
        table = match table
            .entries
            .entry(key.clone())
            .or_insert_with(|| Node::Table(Table::new(Kind::Dotted)))
        {
            Node::Table(t) if t.kind == Kind::Dotted => t,
            _ => return Err(format!("`{}` cannot be extended with a dotted key", key)),
        };
    }
    if table.entries.contains_key(last) {
        return Err(format!("`{}` is defined more than once", last));
    }
    table.entries.insert(last.clone(), value);
    Ok(())
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Whether `s` is digits with single underscores between them.
fn valid_digits(s: &str, digit: DigitTest) -> bool {
    let bytes = s.as_bytes();
    !bytes.is_empty()
        && bytes[0] != b'_'
        && bytes[bytes.len() - 1] != b'_'
        && !s.contains("__")
        && bytes.iter().all(|b| *b == b'_' || digit(*b))
}

/// Whether a byte is a digit of some base.
type DigitTest = fn(u8) -> bool;

fn is_decimal(b: u8) -> bool {
    b.is_ascii_digit()
}

fn is_hex(b: u8) -> bool {
    b.is_ascii_hexdigit()
}

fn is_octal(b: u8) -> bool {
    (b'0'..=b'7').contains(&b)
}

fn is_binary(b: u8) -> bool {
    b == b'0' || b == b'1'
}

fn without_underscores(s: &str) -> String {
    s.chars().filter(|c| *c != '_').collect()
}

/// An integer or float token.
fn parse_number(token: &str) -> Result<Node, &'static str> {
    let (negative, signed, body) = if let Some(rest) = token.strip_prefix('+') {
        (false, true, rest)
    } else if let Some(rest) = token.strip_prefix('-') {
        (true, true, rest)
    } else {
        (false, false, token)
    };
    match body {
        "inf" if negative => return Ok(Node::Float(f64::NEG_INFINITY)),
        "inf" => return Ok(Node::Float(f64::INFINITY)),
        "nan" if negative => return Ok(Node::Float(-f64::NAN)),
        "nan" => return Ok(Node::Float(f64::NAN)),
        _ => {}
    }
    let radix: Option<(u32, DigitTest)> = match body.get(..2) {
        Some("0x") => Some((16, is_hex)),
        Some("0o") => Some((8, is_octal)),
        Some("0b") => Some((2, is_binary)),
        _ => None,
    };
    if let Some((radix, digit)) = radix {
        let digits = &body[2..];
        if signed {
            return Err("a hexadecimal, octal or binary integer cannot have a sign");
        }
        if !valid_digits(digits, digit) {
            return Err("invalid number");
        }
        return i64::from_str_radix(&without_underscores(digits), radix)
            .map(Node::Integer)
            .map_err(|_| "integer is out of the 64-bit range");
    }
    let split = body.find(['.', 'e', 'E']).unwrap_or(body.len());
    let (int_part, mut rest) = body.split_at(split);
    if !valid_digits(int_part, is_decimal) {
        return Err("invalid number");
    }
    if int_part.len() > 1 && int_part.starts_with('0') {
        return Err("leading zeros are not allowed");
    }
    if rest.is_empty() {
        let digits = without_underscores(int_part);
        let text = if negative {
            format!("-{}", digits)
        } else {
            digits
        };
        return text
            .parse::<i64>()
            .map(Node::Integer)
            .map_err(|_| "integer is out of the 64-bit range");
    }
    if let Some(fraction) = rest.strip_prefix('.') {
        let end = fraction.find(['e', 'E']).unwrap_or(fraction.len());
        if !valid_digits(&fraction[..end], is_decimal) {
            return Err("expected digits after '.'");
        }
        rest = &fraction[end..];
    }
    if !rest.is_empty() {
        let exponent = &rest[1..];
        let exponent = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if !valid_digits(exponent, is_decimal) {
            return Err("expected digits in the exponent");
        }
    }
    without_underscores(token)
        .parse::<f64>()
        .map(Node::Float)
        .map_err(|_| "invalid number")
}

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn error_at(&self, pos: usize, message: &str) -> String {
        let before = &self.bytes[..pos.min(self.bytes.len())];
        let line = before.iter().filter(|b| **b == b'\n').count() + 1;
        let column = before.iter().rev().take_while(|b| **b != b'\n').count() + 1;
        format!(
            "invalid TOML at line {}, column {}: {}",
            line, column, message
        )
    }

    fn error(&self, message: &str) -> String {
        self.error_at(self.pos, message)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.pos + offset).copied()
    }

    fn eat(&mut self, b: u8) -> bool {
        if self.peek() == Some(b) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn starts_with(&self, s: &str) -> bool {
        self.bytes[self.pos..].starts_with(s.as_bytes())
    }

    /// The source between two positions that sit on ASCII delimiters.
    fn slice(&self, start: usize, end: usize) -> &str {
        self.text.get(start..end).unwrap_or_default()
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    /// Skips a `#` comment up to the end of its line.
    fn comment(&mut self) -> Result<(), String> {
        self.pos += 1;
        while let Some(b) = self.peek() {
            match b {
                b'\n' | b'\r' => break,
                b'\t' => {}
                0x00..=0x1f | 0x7f => return Err(self.error("control character in a comment")),
                _ => {}
            }
            self.pos += 1;
        }
        Ok(())
    }

    /// Consumes a newline (LF or CRLF), if there is one.
    fn newline(&mut self) -> Result<bool, String> {
        match self.peek() {
            Some(b'\n') => {
                self.pos += 1;
                Ok(true)
            }
            Some(b'\r') if self.peek_at(1) == Some(b'\n') => {
                self.pos += 2;
                Ok(true)
            }
            Some(b'\r') => Err(self.error("carriage return without a line feed")),
            _ => Ok(false),
        }
    }

    /// Skips whitespace, comments and newlines, as allowed inside arrays and
    /// inline tables.
    fn skip_trivia(&mut self) -> Result<(), String> {
        loop {
            self.skip_whitespace();
            if self.peek() == Some(b'#') {
                self.comment()?;
            }
            if !self.newline()? {
                return Ok(());
            }
        }
    }

    /// Requires the rest of the line to be blank or a comment.
    fn end_of_line(&mut self) -> Result<(), String> {
        self.skip_whitespace();
        if self.peek() == Some(b'#') {
            self.comment()?;
        }
        if self.peek().is_none() || self.newline()? {
            Ok(())
        } else {
            Err(self.error("expected the end of the line"))
        }
    }

    fn document(&mut self) -> Result<Table, String> {
        let mut root = Table::new(Kind::Header);
        let mut section: Vec<String> = Vec::new();
        if self.text.starts_with('\u{feff}') {
            self.pos = '\u{feff}'.len_utf8();
        }
        loop {
            self.skip_whitespace();
            let start = self.pos;
            match self.peek() {
                None => break,
                Some(b'#' | b'\n' | b'\r') => {}
                Some(b'[') => {
                    let array = self.peek_at(1) == Some(b'[');
                    self.pos += if array { 2 } else { 1 };
                    self.skip_whitespace();
                    let path = self.key()?;
                    self.skip_whitespace();
                    if !self.eat(b']') || (array && !self.eat(b']')) {
                        return Err(self.error(if array {
                            "expected ']]'"
                        } else {
                            "expected ']'"
                        }));
                    }
                    open_section(&mut root, &path, array).map_err(|m| self.error_at(start, &m))?;
                    section = path;
                }
                Some(_) => {
                    let (keys, value) = self.key_value()?;
                    if section.len() + keys.len() > MAX_DEPTH {
                        return Err(self.error_at(start, "keys are nested too deeply"));
                    }
                    section_table(&mut root, &section)
                        .and_then(|table| insert_value(table, &keys, value))
                        .map_err(|m| self.error_at(start, &m))?;
                }
            }
            self.end_of_line()?;
        }
        Ok(root)
    }

    /// A dotted key: `a`, `a.b`, `"quoted".'literal'`.
    fn key(&mut self) -> Result<Vec<String>, String> {
        let mut parts = Vec::new();
        loop {
            self.skip_whitespace();
            parts.push(self.simple_key()?);
            if parts.len() > MAX_DEPTH {
                return Err(self.error("keys are nested too deeply"));
            }
            self.skip_whitespace();
            if !self.eat(b'.') {
                return Ok(parts);
            }
        }
    }

    fn simple_key(&mut self) -> Result<String, String> {
        match self.peek() {
            Some(b'"') => {
                if self.starts_with("\"\"\"") {
                    return Err(self.error("a key cannot be a multi-line string"));
                }
                self.basic_string()
            }
            Some(b'\'') => {
                if self.starts_with("'''") {
                    return Err(self.error("a key cannot be a multi-line string"));
                }
                self.literal_string()
            }
            _ => {
                let start = self.pos;
                while matches!(self.peek(), Some(b) if b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
                {
                    self.pos += 1;
                }
                if self.pos == start {
                    return Err(self.error("expected a key"));
                }
                Ok(self.slice(start, self.pos).to_string())
            }
        }
    }

    fn key_value(&mut self) -> Result<(Vec<String>, Node), String> {
        let keys = self.key()?;
        self.skip_whitespace();
        if !self.eat(b'=') {
            return Err(self.error("expected '=' after the key"));
        }
        self.skip_whitespace();
        let value = self.value()?;
        Ok((keys, value))
    }

    fn value(&mut self) -> Result<Node, String> {
        match self.peek() {
            None => Err(self.error("expected a value")),
            Some(b'"') if self.starts_with("\"\"\"") => {
                self.multiline_string(b'"').map(Node::String)
            }
            Some(b'"') => self.basic_string().map(Node::String),
            Some(b'\'') if self.starts_with("'''") => {
                self.multiline_string(b'\'').map(Node::String)
            }
            Some(b'\'') => self.literal_string().map(Node::String),
            Some(b't') if self.starts_with("true") => {
                self.pos += 4;
                Ok(Node::Boolean(true))
            }
            Some(b'f') if self.starts_with("false") => {
                self.pos += 5;
                Ok(Node::Boolean(false))
            }
            Some(b'[') => self.nested(Self::array),
            Some(b'{') => self.nested(Self::inline_table),
            Some(b'0'..=b'9' | b'+' | b'-' | b'i' | b'n') => self.number_or_datetime(),
            Some(_) => Err(self.error("expected a value")),
        }
    }

    fn nested(&mut self, parse: fn(&mut Self) -> Result<Node, String>) -> Result<Node, String> {
        if self.depth >= MAX_DEPTH {
            return Err(self.error("nesting is too deep"));
        }
        self.depth += 1;
        let result = parse(self);
        self.depth -= 1;
        result
    }

    fn array(&mut self) -> Result<Node, String> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_trivia()?;
            if self.eat(b']') {
                break;
            }
            items.push(self.value()?);
            self.skip_trivia()?;
            if self.eat(b',') {
                continue;
            }
            if self.eat(b']') {
                break;
            }
            return Err(self.error("expected ',' or ']'"));
        }
        Ok(Node::Array(items))
    }

    /// `{ key = value, ... }`. TOML 1.1 allows newlines, comments and a
    /// trailing comma inside.
    fn inline_table(&mut self) -> Result<Node, String> {
        self.pos += 1;
        let mut table = Table::new(Kind::Inline);
        loop {
            self.skip_trivia()?;
            if self.eat(b'}') {
                break;
            }
            let start = self.pos;
            let (keys, value) = self.key_value()?;
            insert_value(&mut table, &keys, value).map_err(|m| self.error_at(start, &m))?;
            self.skip_trivia()?;
            if self.eat(b',') {
                continue;
            }
            if self.eat(b'}') {
                break;
            }
            return Err(self.error("expected ',' or '}'"));
        }
        Ok(Node::Table(table))
    }

    fn is_digit_at(&self, offset: usize) -> bool {
        self.peek_at(offset).is_some_and(|b| b.is_ascii_digit())
    }

    fn number_or_datetime(&mut self) -> Result<Node, String> {
        let date = (0..4).all(|i| self.is_digit_at(i)) && self.peek_at(4) == Some(b'-');
        let time = self.is_digit_at(0) && self.is_digit_at(1) && self.peek_at(2) == Some(b':');
        if date || time {
            return self.datetime(date);
        }
        let start = self.pos;
        while matches!(self.peek(), Some(b) if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'-' | b'.'))
        {
            self.pos += 1;
        }
        let token = self.slice(start, self.pos);
        parse_number(token)
            .map_err(|message| self.error_at(start, &format!("{} (`{}`)", message, token)))
    }

    /// Exactly `count` digits.
    fn fixed_digits(&mut self, count: usize) -> Result<u32, String> {
        let mut n = 0;
        for _ in 0..count {
            match self.peek() {
                Some(b) if b.is_ascii_digit() => {
                    n = n * 10 + u32::from(b - b'0');
                    self.pos += 1;
                }
                _ => return Err(self.error("invalid date or time")),
            }
        }
        Ok(n)
    }

    fn expect(&mut self, b: u8, message: &str) -> Result<(), String> {
        if self.eat(b) {
            Ok(())
        } else {
            Err(self.error(message))
        }
    }

    /// A date, a time, or a date and time with an optional offset, written
    /// back in RFC 3339 form: `T` and `Z` upper case, seconds always given.
    fn datetime(&mut self, has_date: bool) -> Result<Node, String> {
        let start = self.pos;
        let mut out = String::new();
        if has_date {
            let year = self.fixed_digits(4)?;
            self.expect(b'-', "invalid date")?;
            let month = self.fixed_digits(2)?;
            self.expect(b'-', "invalid date")?;
            let day = self.fixed_digits(2)?;
            if !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
                return Err(self.error_at(start, "invalid date"));
            }
            out.push_str(&format!("{:04}-{:02}-{:02}", year, month, day));
            let time_follows = match self.peek() {
                Some(b'T' | b't') => true,
                Some(b' ') => {
                    self.is_digit_at(1) && self.is_digit_at(2) && self.peek_at(3) == Some(b':')
                }
                _ => false,
            };
            if !time_follows {
                return Ok(Node::Datetime(out));
            }
            self.pos += 1;
            out.push('T');
        }
        let hour = self.fixed_digits(2)?;
        self.expect(b':', "invalid time")?;
        let minute = self.fixed_digits(2)?;
        // Seconds are optional since TOML 1.1.
        let has_seconds = self.eat(b':');
        let second = if has_seconds {
            self.fixed_digits(2)?
        } else {
            0
        };
        // 60 is a leap second.
        if hour > 23 || minute > 59 || second > 60 {
            return Err(self.error_at(start, "invalid time"));
        }
        out.push_str(&format!("{:02}:{:02}:{:02}", hour, minute, second));
        if has_seconds && self.eat(b'.') {
            let digits = self.pos;
            while self.is_digit_at(0) {
                self.pos += 1;
            }
            if self.pos == digits {
                return Err(self.error("expected digits after '.'"));
            }
            out.push('.');
            out.push_str(self.slice(digits, self.pos));
        }
        if has_date {
            match self.peek() {
                Some(b'Z' | b'z') => {
                    self.pos += 1;
                    out.push('Z');
                }
                Some(sign @ (b'+' | b'-')) => {
                    self.pos += 1;
                    let hours = self.fixed_digits(2)?;
                    self.expect(b':', "invalid offset")?;
                    let minutes = self.fixed_digits(2)?;
                    if hours > 23 || minutes > 59 {
                        return Err(self.error_at(start, "invalid offset"));
                    }
                    out.push(sign as char);
                    out.push_str(&format!("{:02}:{:02}", hours, minutes));
                }
                _ => {}
            }
        }
        Ok(Node::Datetime(out))
    }

    fn hex_escape(&mut self, count: usize) -> Result<char, String> {
        let digits = self
            .bytes
            .get(self.pos..self.pos + count)
            .filter(|digits| digits.iter().all(u8::is_ascii_hexdigit))
            .ok_or_else(|| self.error("invalid unicode escape"))?;
        let code = std::str::from_utf8(digits)
            .ok()
            .and_then(|text| u32::from_str_radix(text, 16).ok())
            .and_then(char::from_u32)
            .ok_or_else(|| self.error("invalid unicode escape"))?;
        self.pos += count;
        Ok(code)
    }

    /// An escape sequence of a basic string; `pos` is on the backslash.
    fn escape(&mut self, out: &mut String) -> Result<(), String> {
        self.pos += 1;
        let Some(b) = self.peek() else {
            return Err(self.error("unfinished escape"));
        };
        self.pos += 1;
        match b {
            b'b' => out.push('\u{8}'),
            b't' => out.push('\t'),
            b'n' => out.push('\n'),
            b'f' => out.push('\u{c}'),
            b'r' => out.push('\r'),
            b'e' => out.push('\u{1b}'),
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'x' => out.push(self.hex_escape(2)?),
            b'u' => out.push(self.hex_escape(4)?),
            b'U' => out.push(self.hex_escape(8)?),
            _ => {
                self.pos -= 2;
                return Err(self.error("invalid escape"));
            }
        }
        Ok(())
    }

    /// Whether `b` may appear as is in a string: tab, printable ASCII and
    /// the bytes of non-ASCII characters.
    fn plain(b: u8) -> bool {
        b == b'\t' || (b >= 0x20 && b != 0x7f)
    }

    fn string_error(&self) -> String {
        match self.peek() {
            None => self.error("unfinished string"),
            Some(b'\n' | b'\r') => self.error("newline in a single-line string"),
            Some(_) => self.error("control character in a string"),
        }
    }

    fn basic_string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while matches!(self.peek(), Some(b) if Self::plain(b) && b != b'"' && b != b'\\') {
                self.pos += 1;
            }
            out.push_str(self.slice(start, self.pos));
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    return Ok(out);
                }
                Some(b'\\') => self.escape(&mut out)?,
                _ => return Err(self.string_error()),
            }
        }
    }

    fn literal_string(&mut self) -> Result<String, String> {
        self.pos += 1;
        let start = self.pos;
        while matches!(self.peek(), Some(b) if Self::plain(b) && b != b'\'') {
            self.pos += 1;
        }
        let out = self.slice(start, self.pos).to_string();
        if self.eat(b'\'') {
            Ok(out)
        } else {
            Err(self.string_error())
        }
    }

    /// `"""..."""` or `'''...'''`. A newline right after the opening quotes
    /// is dropped, and in the basic form a backslash at the end of a line
    /// joins it to the next non-blank text.
    fn multiline_string(&mut self, quote: u8) -> Result<String, String> {
        let basic = quote == b'"';
        self.pos += 3;
        self.newline()?;
        let mut out = String::new();
        loop {
            let start = self.pos;
            while matches!(self.peek(), Some(b) if (Self::plain(b) || b == b'\n') && b != quote && !(basic && b == b'\\'))
            {
                self.pos += 1;
            }
            out.push_str(self.slice(start, self.pos));
            match self.peek() {
                Some(b) if b == quote => {
                    let mut count = 0;
                    while self.peek_at(count) == Some(quote) {
                        count += 1;
                    }
                    if count >= 3 {
                        // Up to two quotes may sit right before the closing three.
                        if count > 5 {
                            return Err(self.error("too many quotes"));
                        }
                        for _ in 3..count {
                            out.push(quote as char);
                        }
                        self.pos += count;
                        return Ok(out);
                    }
                    for _ in 0..count {
                        out.push(quote as char);
                    }
                    self.pos += count;
                }
                Some(b'\\') => {
                    let mut ahead = self.pos + 1;
                    while matches!(self.bytes.get(ahead), Some(b' ' | b'\t')) {
                        ahead += 1;
                    }
                    let line_end = match self.bytes.get(ahead) {
                        Some(b'\n') => true,
                        Some(b'\r') => self.bytes.get(ahead + 1) == Some(&b'\n'),
                        _ => false,
                    };
                    if line_end {
                        self.pos = ahead;
                        loop {
                            self.skip_whitespace();
                            if !self.newline()? {
                                break;
                            }
                        }
                    } else {
                        self.escape(&mut out)?;
                    }
                }
                Some(b'\r') => {
                    if self.peek_at(1) != Some(b'\n') {
                        return Err(self.error("carriage return without a line feed"));
                    }
                    // Line breaks inside the string become "\n", so a
                    // document reads the same with CRLF line endings.
                    out.push('\n');
                    self.pos += 2;
                }
                None => return Err(self.error("unfinished string")),
                Some(_) => return Err(self.error("control character in a string")),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// encode
// ---------------------------------------------------------------------------

/// What a table holds: a non-empty array part only, or keys.
enum Shape {
    Array(Vec<Value>),
    Map(Vec<(String, Value)>),
}

fn shape(table: &Rc<RefCell<VmTable>>) -> Result<Shape, String> {
    let tbl = table.borrow();
    let has_keys = !tbl.fields.is_empty() || tbl.general.is_some();
    if !tbl.array.is_empty() && !has_keys {
        return Ok(Shape::Array(tbl.array.clone()));
    }
    let mut pairs = Vec::new();
    for (key, value) in crate::vm::libs::table::entries(&tbl) {
        let key_str = match key.view() {
            ValueView::String(s) => s.to_string(),
            ValueView::Int(_) | ValueView::BigInt(_) => key.to_string(),
            _ => {
                return Err(format!(
                    "cannot encode a table key of type {} as TOML",
                    key.type_name()
                ));
            }
        };
        pairs.push((key_str, value));
    }
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Shape::Map(pairs))
}

/// How a value of a section is written.
enum Class {
    /// `key = value` in the section itself.
    Inline,
    /// A `[section]` of its own.
    Table(Rc<RefCell<VmTable>>),
    /// One `[[section]]` per entry.
    TableArray(Vec<Rc<RefCell<VmTable>>>),
}

fn is_map(table: &Rc<RefCell<VmTable>>) -> bool {
    matches!(shape(table), Ok(Shape::Map(_)))
}

fn classify(value: &Value) -> Result<Class, String> {
    let Some(table) = value.as_table() else {
        return Ok(Class::Inline);
    };
    let table = table.to_rc();
    match shape(&table)? {
        Shape::Map(_) => Ok(Class::Table(table)),
        Shape::Array(items) => {
            let tables: Option<Vec<_>> = items
                .iter()
                .map(|item| item.as_table().map(|t| t.to_rc()).filter(is_map))
                .collect();
            Ok(tables.map_or(Class::Inline, Class::TableArray))
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Header {
    /// The top level, which has no header.
    None,
    /// `[path]`, left out when the table only holds other sections.
    Table,
    /// `[[path]]`.
    ArrayEntry,
}

struct Encoder {
    out: String,
    /// Tables being encoded, to reject cycles.
    open: Vec<*const RefCell<VmTable>>,
}

fn format_float(f: f64) -> String {
    if f.is_nan() {
        "nan".to_string()
    } else if f.is_infinite() && f > 0.0 {
        "inf".to_string()
    } else if f.is_infinite() {
        "-inf".to_string()
    } else {
        // Debug formatting keeps a fraction or an exponent (`1.0`, `1e300`),
        // which TOML reads back as a float.
        format!("{:?}", f)
    }
}

fn write_string(out: &mut String, s: &str) {
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
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                out.push_str(&format!("\\u{:04X}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn write_key(out: &mut String, key: &str) {
    let bare = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if bare {
        out.push_str(key);
    } else {
        write_string(out, key);
    }
}

impl Encoder {
    /// Marks `table` as being encoded, rejecting cycles, deep nesting and
    /// oversized output.
    fn enter(&mut self, table: &Rc<RefCell<VmTable>>, depth: usize) -> Result<(), String> {
        if depth >= MAX_DEPTH {
            return Err(format!("nesting is deeper than {} levels", MAX_DEPTH));
        }
        if self.out.len() > MAX_OUTPUT {
            return Err(format!(
                "output exceeds the {} MiB limit",
                MAX_OUTPUT / (1024 * 1024)
            ));
        }
        let ptr = Rc::as_ptr(table);
        if self.open.contains(&ptr) {
            return Err("cannot encode a table that contains itself".to_string());
        }
        self.open.push(ptr);
        Ok(())
    }

    fn header(&mut self, path: &[String], array: bool) {
        if !self.out.is_empty() {
            self.out.push('\n');
        }
        self.out.push_str(if array { "[[" } else { "[" });
        for (i, key) in path.iter().enumerate() {
            if i > 0 {
                self.out.push('.');
            }
            write_key(&mut self.out, key);
        }
        self.out.push_str(if array { "]]\n" } else { "]\n" });
    }

    fn section(
        &mut self,
        table: &Rc<RefCell<VmTable>>,
        path: &mut Vec<String>,
        depth: usize,
        header: Header,
    ) -> Result<(), String> {
        self.enter(table, depth)?;
        let result = self.section_body(table, path, depth, header);
        self.open.pop();
        result
    }

    fn section_body(
        &mut self,
        table: &Rc<RefCell<VmTable>>,
        path: &mut Vec<String>,
        depth: usize,
        header: Header,
    ) -> Result<(), String> {
        let pairs = match shape(table)? {
            Shape::Map(pairs) => pairs,
            Shape::Array(_) => {
                return Err("the top level must be a table with keys, not an array".to_string());
            }
        };
        let mut values = Vec::new();
        let mut tables = Vec::new();
        let mut table_arrays = Vec::new();
        for (key, value) in pairs {
            match classify(&value)? {
                Class::Inline => values.push((key, value)),
                Class::Table(t) => tables.push((key, t)),
                Class::TableArray(items) => table_arrays.push((key, items)),
            }
        }
        match header {
            Header::None => {}
            Header::ArrayEntry => self.header(path, true),
            Header::Table => {
                if !values.is_empty() || (tables.is_empty() && table_arrays.is_empty()) {
                    self.header(path, false);
                }
            }
        }
        for (key, value) in &values {
            write_key(&mut self.out, key);
            self.out.push_str(" = ");
            self.inline(value, depth + 1)?;
            self.out.push('\n');
        }
        for (key, t) in tables {
            path.push(key);
            let result = self.section(&t, path, depth + 1, Header::Table);
            path.pop();
            result?;
        }
        for (key, items) in table_arrays {
            path.push(key);
            let mut result = Ok(());
            for item in &items {
                result = self.section(item, path, depth + 1, Header::ArrayEntry);
                if result.is_err() {
                    break;
                }
            }
            path.pop();
            result?;
        }
        Ok(())
    }

    fn inline(&mut self, value: &Value, depth: usize) -> Result<(), String> {
        match value.view() {
            ValueView::Bool(b) => self.out.push_str(if b { "true" } else { "false" }),
            ValueView::Int(i) => self.out.push_str(&i.to_string()),
            ValueView::BigInt(_) => {
                return Err("integer is out of TOML's 64-bit range".to_string());
            }
            ValueView::Float(f) => self.out.push_str(&format_float(f)),
            ValueView::String(s) => write_string(&mut self.out, &s),
            ValueView::Table(t) => {
                let t = t
                    .as_old()
                    .expect("toml encoder promotes root Beat tables upfront")
                    .to_rc();
                self.enter(&t, depth)?;
                let result = self.inline_table(&t, depth);
                self.open.pop();
                result?;
            }
            ValueView::Nil => return Err("cannot encode nil as TOML".to_string()),
            _ => {
                return Err(format!("cannot encode a {} as TOML", value.type_name()));
            }
        }
        Ok(())
    }

    fn inline_table(&mut self, table: &Rc<RefCell<VmTable>>, depth: usize) -> Result<(), String> {
        match shape(table)? {
            Shape::Array(items) => {
                self.out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    self.inline(item, depth + 1)?;
                }
                self.out.push(']');
            }
            Shape::Map(pairs) if pairs.is_empty() => self.out.push_str("{}"),
            Shape::Map(pairs) => {
                self.out.push_str("{ ");
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(", ");
                    }
                    write_key(&mut self.out, key);
                    self.out.push_str(" = ");
                    self.inline(value, depth + 1)?;
                }
                self.out.push_str(" }");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'t>(table: &'t Table, key: &str) -> &'t Node {
        table.entries.get(key).expect("key present")
    }

    #[test]
    fn headers_dotted_keys_and_arrays_of_tables() {
        let doc = decode(
            "title = \"x\"\n[server]\nhost.name = 'h'\n[[item]]\nn = 1\n[[item]]\nn = 2\n[item.sub]\nk = true\n",
        )
        .unwrap();
        assert!(matches!(get(&doc, "title"), Node::String(s) if s == "x"));
        let Node::Table(server) = get(&doc, "server") else {
            panic!("server is a table")
        };
        let Node::Table(host) = get(server, "host") else {
            panic!("host is a table")
        };
        assert!(matches!(get(host, "name"), Node::String(s) if s == "h"));
        let Node::TableArray(items) = get(&doc, "item") else {
            panic!("item is an array of tables")
        };
        assert_eq!(items.len(), 2);
        assert!(items[1].entries.contains_key("sub"));
    }

    #[test]
    fn numbers_and_datetimes() {
        let doc = decode(
            "a = 0xff\nb = -1_000\nc = 6.5e-1\nd = -inf\ne = 1979-05-27 07:32Z\nf = 07:32:00.5\n",
        )
        .unwrap();
        assert!(matches!(get(&doc, "a"), Node::Integer(255)));
        assert!(matches!(get(&doc, "b"), Node::Integer(-1000)));
        assert!(matches!(get(&doc, "c"), Node::Float(f) if (*f - 0.65).abs() < 1e-12));
        assert!(matches!(get(&doc, "d"), Node::Float(f) if *f == f64::NEG_INFINITY));
        assert!(matches!(get(&doc, "e"), Node::Datetime(s) if s == "1979-05-27T07:32:00Z"));
        assert!(matches!(get(&doc, "f"), Node::Datetime(s) if s == "07:32:00.5"));
    }

    #[test]
    fn multiline_strings() {
        let doc = decode("a = \"\"\"\none \\\n   two\"\"\"\"\nb = '''x''y'''\n").unwrap();
        assert!(matches!(get(&doc, "a"), Node::String(s) if s == "one two\""));
        assert!(matches!(get(&doc, "b"), Node::String(s) if s == "x''y"));
    }

    #[test]
    fn rejects_invalid_documents() {
        for bad in [
            "a = 1\na = 2",
            "[a]\n[a]",
            "a = {}\n[a]",
            "a.b = 1\n[a]",
            "a = 01",
            "a = 1__0",
            "a = +0x1",
            "a = 9223372036854775808",
            "a = 1979-02-30",
            "a = \"\\q\"",
            "a = [1,,2]",
            "a = 1 b = 2",
            "x = [[1]]\n[[x]]",
        ] {
            assert!(decode(bad).is_err(), "accepted {:?}", bad);
        }
    }

    #[test]
    fn deep_nesting_is_an_error() {
        let text = format!("a = {}{}", "[".repeat(1000), "]".repeat(1000));
        let Err(err) = decode(&text) else {
            panic!("deep nesting was accepted")
        };
        assert!(err.contains("too deep"), "{}", err);
    }
}
