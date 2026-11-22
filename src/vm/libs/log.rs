//! Native structured logging, terminal detection and rotating file sinks
//! backing `@neyuki/log`.
//!
//! `lib/log.nyk` builds loggers, child loggers, level filtering and sink
//! dispatching on top of these primitives. Keeping timestamping, JSON Lines
//! encoding, ANSI detection and file rotation in Rust avoids opening and
//! closing log files per line and keeps structured serialization fast and
//! cycle-safe.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::UNIX_EPOCH;

use flate2::Compression;
use flate2::write::GzEncoder;
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::vm::libs::args::{bool_arg, opt_int_arg, str_arg};
use crate::vm::libs::json;
use crate::vm::machine::VM;
use crate::vm::value::{NativeDef, Value, VmTable};

/// Maximum nesting depth when encoding structured log fields. Deeper tables
/// become `"<max depth>"` instead of failing the log call.
const MAX_DEPTH: usize = 64;

/// Maximum encoded size of one formatted log line.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Upper bound when scanning numbered archives during unlimited rotation.
const MAX_UNLIMITED_SHIFT: usize = 10_000;

pub(crate) const NATIVES: &[NativeDef] = &[
    NativeDef {
        name: "__log_now",
        func: builtin_now,
    },
    NativeDef {
        name: "__log_color_auto",
        func: builtin_color_auto,
    },
    NativeDef {
        name: "__log_write_console",
        func: builtin_write_console,
    },
    NativeDef {
        name: "__log_json_line",
        func: builtin_json_line,
    },
    NativeDef {
        name: "__log_format_pretty",
        func: builtin_format_pretty,
    },
    NativeDef {
        name: "__log_file_open",
        func: builtin_file_open,
    },
    NativeDef {
        name: "__log_file_write",
        func: builtin_file_write,
    },
    NativeDef {
        name: "__log_file_flush",
        func: builtin_file_flush,
    },
    NativeDef {
        name: "__log_file_rotate",
        func: builtin_file_rotate,
    },
    NativeDef {
        name: "__log_file_close",
        func: builtin_file_close,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RotationMode {
    None,
    Daily,
    Hourly,
}

impl RotationMode {
    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "none" | "" => Ok(Self::None),
            "daily" => Ok(Self::Daily),
            "hourly" => Ok(Self::Hourly),
            other => Err(format!(
                "invalid rotation `{}` (expected none, daily or hourly)",
                other
            )),
        }
    }
}

struct FileSink {
    path: PathBuf,
    file: Option<File>,
    current_bytes: u64,
    max_size: u64,
    rotation: RotationMode,
    current_period: String,
    max_files: usize,
    utc: bool,
    compress: bool,
}

thread_local! {
    static FILE_SINKS: RefCell<HashMap<u64, FileSink>> = RefCell::new(HashMap::new());
    static NEXT_SINK_ID: Cell<u64> = const { Cell::new(1) };
}

fn next_sink_id() -> u64 {
    NEXT_SINK_ID.with(|cell| {
        let id = cell.get();
        cell.set(id + 1);
        id
    })
}

fn sink_id_arg(args: &[Value], index: usize) -> Result<u64, String> {
    let raw = crate::vm::libs::args::int_arg(args, index, "sink")?;
    if raw <= 0 {
        return Err("invalid file sink handle".to_string());
    }
    Ok(raw as u64)
}

fn io_error(action: &str, path: &Path, err: io::Error) -> String {
    format!("cannot {} `{}`: {}", action, path.display(), err)
}

// ---------------------------------------------------------------------------
// timestamps & terminal color detection
// ---------------------------------------------------------------------------

fn format_timestamp(ts: Timestamp, utc: bool) -> String {
    let ms = ts.subsec_nanosecond().rem_euclid(1_000_000_000) / 1_000_000;
    if utc {
        let dt = TimeZone::UTC.to_datetime(ts);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
            dt.year(),
            dt.month(),
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second(),
            ms
        )
    } else {
        let zoned = ts.to_zoned(TimeZone::system());
        let dt = zoned.datetime();
        let offset = zoned.offset().seconds();
        let sign = if offset < 0 { '-' } else { '+' };
        let abs_offset = offset.unsigned_abs();
        let off_hours = abs_offset / 3600;
        let off_mins = (abs_offset % 3600) / 60;
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{}{:02}:{:02}",
            dt.year(),
            dt.month(),
            dt.day(),
            dt.hour(),
            dt.minute(),
            dt.second(),
            ms,
            sign,
            off_hours,
            off_mins
        )
    }
}

fn period_key(ts: Timestamp, mode: RotationMode, utc: bool) -> String {
    if mode == RotationMode::None {
        return String::new();
    }
    let dt = if utc {
        TimeZone::UTC.to_datetime(ts)
    } else {
        ts.to_zoned(TimeZone::system()).datetime()
    };
    match mode {
        RotationMode::None => String::new(),
        RotationMode::Daily => format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day()),
        RotationMode::Hourly => format!(
            "{:04}-{:02}-{:02}-{:02}",
            dt.year(),
            dt.month(),
            dt.day(),
            dt.hour()
        ),
    }
}

/// `__log_now(utc?)`: current RFC 3339 timestamp with millisecond precision.
fn builtin_now(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let utc = match args.first() {
        None | Some(Value::Nil) => true,
        _ => bool_arg(args, 0, "utc")?,
    };
    Ok(vec![Value::string(format_timestamp(Timestamp::now(), utc))])
}

/// `__log_color_auto(stream)`: whether ANSI colors should be emitted for
/// `stdout` or `stderr`, honoring `NO_COLOR`, `FORCE_COLOR`, `TERM=dumb`
/// (when env permissions allow reading them) and `IsTerminal`.
fn builtin_color_auto(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let stream = str_arg(args, 0, "stream")?;
    let is_tty = match stream {
        "stdout" => io::stdout().is_terminal(),
        "stderr" => io::stderr().is_terminal(),
        _ => {
            return Err(format!(
                "invalid output stream `{}` (expected stdout or stderr)",
                stream
            ));
        }
    };
    if vm.permissions.check_env("NO_COLOR").is_ok()
        && std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
    {
        return Ok(vec![Value::Bool(false)]);
    }
    if vm.permissions.check_env("FORCE_COLOR").is_ok()
        && std::env::var_os("FORCE_COLOR").is_some_and(|v| !v.is_empty() && v != "0")
    {
        return Ok(vec![Value::Bool(true)]);
    }
    if vm.permissions.check_env("TERM").is_ok()
        && std::env::var_os("TERM").is_some_and(|v| v == "dumb")
    {
        return Ok(vec![Value::Bool(false)]);
    }
    Ok(vec![Value::Bool(is_tty)])
}

/// `__log_write_console(stream, line)`: writes one complete log line followed
/// by a newline to `stdout` or `stderr` under a single stream lock.
fn builtin_write_console(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let stream = str_arg(args, 0, "stream")?;
    let line = str_arg(args, 1, "line")?;
    let mut payload = String::with_capacity(line.len() + 1);
    payload.push_str(line);
    payload.push('\n');
    let result = match stream {
        "stdout" => {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(payload.as_bytes())
                .and_then(|()| lock.flush())
        }
        "stderr" => {
            let stderr = io::stderr();
            let mut lock = stderr.lock();
            lock.write_all(payload.as_bytes())
                .and_then(|()| lock.flush())
        }
        _ => {
            return Err(format!(
                "invalid output stream `{}` (expected stdout or stderr)",
                stream
            ));
        }
    };
    result.map_err(|err| format!("cannot write to {}: {}", stream, err))?;
    Ok(vec![Value::Nil])
}

// ---------------------------------------------------------------------------
// JSON Lines & pretty console formatting
// ---------------------------------------------------------------------------

struct LogJsonEncoder<'a> {
    vm: &'a mut VM,
    out: String,
    open: Vec<*const RefCell<VmTable>>,
}

impl LogJsonEncoder<'_> {
    fn check_limit(&self) -> Result<(), String> {
        if self.out.len() > MAX_LINE_BYTES {
            return Err(format!(
                "log line exceeds the {} MiB limit",
                MAX_LINE_BYTES / (1024 * 1024)
            ));
        }
        Ok(())
    }

    fn value(&mut self, value: &Value, depth: usize) -> Result<(), String> {
        self.check_limit()?;
        if depth >= MAX_DEPTH {
            json::write_string(&mut self.out, "<max depth>");
            return Ok(());
        }
        match value {
            Value::Nil => self.out.push_str("null"),
            Value::Bool(b) => self.out.push_str(if *b { "true" } else { "false" }),
            Value::Int(i) => self.out.push_str(&i.to_string()),
            Value::BigInt(i) => self.out.push_str(&i.to_string()),
            Value::Float(f) => match json::format_float(*f) {
                Some(text) => self.out.push_str(&text),
                None if f.is_nan() => json::write_string(&mut self.out, "NaN"),
                None if f.is_sign_positive() => json::write_string(&mut self.out, "Infinity"),
                None => json::write_string(&mut self.out, "-Infinity"),
            },
            Value::String(s) => json::write_string(&mut self.out, s),
            Value::Buffer(buf) => {
                let len = buf.borrow().len();
                json::write_string(&mut self.out, &format!("<buffer: {} bytes>", len));
            }
            Value::Table(_) if json::is_null(value) => self.out.push_str("null"),
            Value::Table(t) => {
                let custom = t.borrow().metamethod("__tojson");
                if let Some(handler) = custom {
                    let result = self
                        .vm
                        .call_function(handler, std::slice::from_ref(value))?;
                    let result = result.into_iter().next().unwrap_or(Value::Nil);
                    if let Value::Table(r) = &result
                        && Rc::ptr_eq(r, t)
                    {
                        json::write_string(&mut self.out, "<cycle>");
                        return Ok(());
                    }
                    return self.value(&result, depth + 1);
                }
                let ptr = Rc::as_ptr(t);
                if self.open.contains(&ptr) {
                    json::write_string(&mut self.out, "<cycle>");
                    return Ok(());
                }
                self.open.push(ptr);
                let result = self.table(t, depth);
                self.open.pop();
                result?;
            }
            other => {
                json::write_string(&mut self.out, &format!("<{}>", other.type_name()));
            }
        }
        self.check_limit()
    }

    fn table(&mut self, t: &Rc<RefCell<VmTable>>, depth: usize) -> Result<(), String> {
        let (is_array, items, entries) = {
            let tbl = t.borrow();
            let marked = tbl
                .metatable
                .as_ref()
                .is_some_and(|m| m.borrow().get_str(json::ARRAY_MARK).is_truthy());
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
                self.value(item, depth + 1)?;
            }
            self.out.push(']');
            return Ok(());
        }
        let mut pairs = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let key = key_to_string(&key);
            pairs.push((key, value));
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        self.out.push('{');
        for (i, (key, value)) in pairs.iter().enumerate() {
            if i > 0 {
                self.out.push(',');
            }
            json::write_string(&mut self.out, key);
            self.out.push(':');
            self.value(value, depth + 1)?;
        }
        self.out.push('}');
        Ok(())
    }
}

fn key_to_string(key: &Value) -> String {
    match key {
        Value::String(s) => s.to_string(),
        Value::Int(_) | Value::BigInt(_) | Value::Bool(_) => key.to_string(),
        Value::Float(f) => json::format_float(*f).unwrap_or_else(|| f.to_string()),
        other => format!("<{}>", other.type_name()),
    }
}

fn sorted_field_pairs(fields: &Rc<RefCell<VmTable>>) -> Vec<(String, Value)> {
    let entries = crate::vm::libs::table::entries(&fields.borrow());
    let mut pairs: Vec<(String, Value)> = entries
        .into_iter()
        .map(|(k, v)| (key_to_string(&k), v))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

/// `__log_json_line(time, level, msg, name?, fields?)`: encodes one structured
/// log record as a single compact JSON Lines string.
fn builtin_json_line(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let time = str_arg(args, 0, "time")?;
    let level = str_arg(args, 1, "level")?;
    let msg = str_arg(args, 2, "msg")?;
    let name = match args.get(3) {
        None | Some(Value::Nil) => None,
        _ => Some(str_arg(args, 3, "name")?),
    };
    let fields = match args.get(4) {
        None | Some(Value::Nil) => None,
        Some(Value::Table(t)) => Some(t.clone()),
        Some(other) => {
            return Err(format!("fields must be a table, got {}", other.type_name()));
        }
    };

    let mut encoder = LogJsonEncoder {
        vm,
        out: String::with_capacity(128),
        open: Vec::new(),
    };
    let mut emitted: HashSet<String> = HashSet::new();
    encoder.out.push('{');
    encoder.out.push_str("\"time\":");
    json::write_string(&mut encoder.out, time);
    emitted.insert("time".to_string());

    encoder.out.push_str(",\"level\":");
    json::write_string(&mut encoder.out, level);
    emitted.insert("level".to_string());

    if let Some(name) = name {
        encoder.out.push_str(",\"logger\":");
        json::write_string(&mut encoder.out, name);
        emitted.insert("logger".to_string());
    }

    encoder.out.push_str(",\"msg\":");
    json::write_string(&mut encoder.out, msg);
    emitted.insert("msg".to_string());

    if let Some(fields) = fields {
        let pairs = sorted_field_pairs(&fields);
        for (raw_key, value) in pairs {
            let mut key = raw_key;
            if emitted.contains(&key) {
                let mut candidate = format!("field_{}", key);
                let mut n = 2usize;
                while emitted.contains(&candidate) {
                    candidate = format!("field_{}_{}", key, n);
                    n += 1;
                }
                key = candidate;
            }
            emitted.insert(key.clone());
            encoder.out.push(',');
            json::write_string(&mut encoder.out, &key);
            encoder.out.push(':');
            encoder.value(&value, 0)?;
        }
    }

    encoder.out.push('}');
    encoder.check_limit()?;
    Ok(vec![Value::string(encoder.out)])
}

/// Sanitizes control characters (including ANSI ESC and CR) from log message
/// text so untrusted user values cannot inject terminal escape sequences or
/// forge extra log lines.
fn write_sanitized_message(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '\x1b' => out.push_str("\\x1b"),
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push('\t'),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
}

/// Whether `s` can appear unquoted in a `key=value` logfmt pair without
/// ambiguity or control-character injection.
fn is_bare_logfmt(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            !c.is_whitespace()
                && !c.is_control()
                && c != '"'
                && c != '='
                && c != '\\'
                && c != '\x7f'
        })
}

fn level_badge(level: &str, color: bool) -> (&'static str, &'static str) {
    let label = match level {
        "trace" => "TRACE",
        "debug" => "DEBUG",
        "info" => "INFO ",
        "warn" => "WARN ",
        "error" => "ERROR",
        "fatal" => "FATAL",
        _ => "LOG  ",
    };
    if !color {
        return ("", label);
    }
    let code = match level {
        "trace" => "\x1b[90m",
        "debug" => "\x1b[36m",
        "info" => "\x1b[32m",
        "warn" => "\x1b[33m",
        "error" => "\x1b[31m",
        "fatal" => "\x1b[1;91m",
        _ => "\x1b[0m",
    };
    (code, label)
}

/// `__log_format_pretty(time, level, msg, name?, fields?, color?)`: formats a
/// human-readable (and optionally ANSI-colored) log line.
fn builtin_format_pretty(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let time = str_arg(args, 0, "time")?;
    let level = str_arg(args, 1, "level")?;
    let msg = str_arg(args, 2, "msg")?;
    let name = match args.get(3) {
        None | Some(Value::Nil) => None,
        _ => Some(str_arg(args, 3, "name")?),
    };
    let fields = match args.get(4) {
        None | Some(Value::Nil) => None,
        Some(Value::Table(t)) => Some(t.clone()),
        Some(other) => {
            return Err(format!("fields must be a table, got {}", other.type_name()));
        }
    };
    let color = bool_arg(args, 5, "color")?;

    let mut out = String::with_capacity(128);
    if color {
        out.push_str("\x1b[2m");
        out.push_str(time);
        out.push_str("\x1b[0m ");
    } else {
        out.push_str(time);
        out.push(' ');
    }

    let (code, badge) = level_badge(level, color);
    if color {
        out.push_str(code);
        out.push_str(badge);
        out.push_str("\x1b[0m");
    } else {
        out.push_str(badge);
    }

    if let Some(name) = name {
        out.push(' ');
        if color {
            out.push_str("\x1b[35m[");
            write_sanitized_message(&mut out, name);
            out.push_str("]\x1b[0m");
        } else {
            out.push('[');
            write_sanitized_message(&mut out, name);
            out.push(']');
        }
    }

    if !msg.is_empty() {
        out.push(' ');
        let bold_msg = color && matches!(level, "error" | "fatal");
        if bold_msg {
            out.push_str("\x1b[1m");
        }
        write_sanitized_message(&mut out, msg);
        if bold_msg {
            out.push_str("\x1b[0m");
        }
    }

    if let Some(fields) = fields {
        let pairs = sorted_field_pairs(&fields);
        for (key, value) in pairs {
            out.push(' ');
            if color {
                out.push_str("\x1b[36m");
                write_sanitized_message(&mut out, &key);
                out.push_str("\x1b[0m\x1b[2m=\x1b[0m");
            } else {
                write_sanitized_message(&mut out, &key);
                out.push('=');
            }
            match &value {
                Value::String(s) => {
                    if is_bare_logfmt(s) {
                        out.push_str(s);
                    } else {
                        json::write_string(&mut out, s);
                    }
                }
                Value::Nil => out.push_str("null"),
                Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                Value::Int(i) => out.push_str(&i.to_string()),
                Value::BigInt(i) => out.push_str(&i.to_string()),
                Value::Float(f) => match json::format_float(*f) {
                    Some(text) => out.push_str(&text),
                    None => out.push_str(&f.to_string()),
                },
                Value::Table(_) if json::is_null(&value) => out.push_str("null"),
                _ => {
                    let mut encoder = LogJsonEncoder {
                        vm,
                        out: String::new(),
                        open: Vec::new(),
                    };
                    encoder.value(&value, 0)?;
                    out.push_str(&encoder.out);
                }
            }
            if out.len() > MAX_LINE_BYTES {
                return Err(format!(
                    "log line exceeds the {} MiB limit",
                    MAX_LINE_BYTES / (1024 * 1024)
                ));
            }
        }
    }

    Ok(vec![Value::string(out)])
}

// ---------------------------------------------------------------------------
// rotating file sinks
// ---------------------------------------------------------------------------

fn check_path_writable(vm: &VM, path: &Path) -> Result<(), String> {
    let text = path
        .to_str()
        .ok_or_else(|| "log file path is not valid UTF-8".to_string())?;
    vm.permissions.check_write(text)?;
    crate::app::check_writable(text)?;
    Ok(())
}

fn split_log_filename(path: &Path) -> Result<(PathBuf, String, String), String> {
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("invalid log file path `{}`", path.display()))?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    if let Some(dot) = file_name.rfind('.')
        && dot > 0
    {
        return Ok((
            parent,
            file_name[..dot].to_string(),
            file_name[dot..].to_string(),
        ));
    }
    Ok((parent, file_name.to_string(), String::new()))
}

fn numbered_archive_path(base: &Path, index: usize, compress: bool) -> PathBuf {
    let mut name = base.as_os_str().to_os_string();
    name.push(format!(".{}", index));
    if compress {
        name.push(".gz");
    }
    PathBuf::from(name)
}

fn gzip_file(src: &Path, dst: &Path) -> Result<(), String> {
    let mut input = File::open(src).map_err(|err| io_error("read", src, err))?;
    let output = File::create(dst).map_err(|err| io_error("create", dst, err))?;
    let mut encoder = GzEncoder::new(output, Compression::default());
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = input
            .read(&mut buf)
            .map_err(|err| io_error("read", src, err))?;
        if n == 0 {
            break;
        }
        encoder
            .write_all(&buf[..n])
            .map_err(|err| io_error("write", dst, err))?;
    }
    encoder
        .finish()
        .map_err(|err| io_error("finish", dst, err))?;
    drop(input);
    fs::remove_file(src).map_err(|err| io_error("remove", src, err))?;
    Ok(())
}

fn parse_dated_stamp(stamp: &str, mode: RotationMode) -> Option<(&str, usize)> {
    let bytes = stamp.as_bytes();
    let check_date = |b: &[u8]| {
        b.len() >= 10
            && b[0..4].iter().all(u8::is_ascii_digit)
            && b[4] == b'-'
            && b[5..7].iter().all(u8::is_ascii_digit)
            && b[7] == b'-'
            && b[8..10].iter().all(u8::is_ascii_digit)
    };
    let (date_part, rest) = match mode {
        RotationMode::Daily => {
            if !check_date(bytes) {
                return None;
            }
            (&stamp[..10], &stamp[10..])
        }
        RotationMode::Hourly => {
            if bytes.len() < 13
                || !check_date(bytes)
                || bytes[10] != b'-'
                || !bytes[11..13].iter().all(u8::is_ascii_digit)
            {
                return None;
            }
            (&stamp[..13], &stamp[13..])
        }
        RotationMode::None => return None,
    };
    if rest.is_empty() {
        return Some((date_part, 0));
    }
    let seq_str = rest.strip_prefix('.')?;
    if seq_str.is_empty() || !seq_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seq = seq_str.parse::<usize>().ok()?;
    Some((date_part, seq))
}

fn rotate_files(vm: &VM, sink: &mut FileSink, next_period: &str) -> Result<(), String> {
    if sink.path.exists() {
        match sink.rotation {
            RotationMode::None => {
                if sink.max_files > 0 {
                    let oldest = numbered_archive_path(&sink.path, sink.max_files, sink.compress);
                    if oldest.exists() {
                        check_path_writable(vm, &oldest)?;
                        fs::remove_file(&oldest).map_err(|err| io_error("remove", &oldest, err))?;
                    }
                    for k in (1..sink.max_files).rev() {
                        let from = numbered_archive_path(&sink.path, k, sink.compress);
                        let to = numbered_archive_path(&sink.path, k + 1, sink.compress);
                        if from.exists() {
                            check_path_writable(vm, &from)?;
                            check_path_writable(vm, &to)?;
                            fs::rename(&from, &to).map_err(|err| io_error("rename", &from, err))?;
                        }
                    }
                } else {
                    let mut highest = 0usize;
                    for k in 1..=MAX_UNLIMITED_SHIFT {
                        if numbered_archive_path(&sink.path, k, sink.compress).exists() {
                            highest = k;
                        } else {
                            break;
                        }
                    }
                    for k in (1..=highest).rev() {
                        let from = numbered_archive_path(&sink.path, k, sink.compress);
                        let to = numbered_archive_path(&sink.path, k + 1, sink.compress);
                        check_path_writable(vm, &from)?;
                        check_path_writable(vm, &to)?;
                        fs::rename(&from, &to).map_err(|err| io_error("rename", &from, err))?;
                    }
                }
                let first = numbered_archive_path(&sink.path, 1, sink.compress);
                check_path_writable(vm, &first)?;
                if sink.compress {
                    gzip_file(&sink.path, &first)?;
                } else {
                    fs::rename(&sink.path, &first)
                        .map_err(|err| io_error("rename", &sink.path, err))?;
                }
            }
            RotationMode::Daily | RotationMode::Hourly => {
                let (parent, stem, ext) = split_log_filename(&sink.path)?;
                let gz = if sink.compress { ".gz" } else { "" };
                let period = if sink.current_period.is_empty() {
                    next_period
                } else {
                    sink.current_period.as_str()
                };
                let mut archive = parent.join(format!("{}.{}{}{}", stem, period, ext, gz));
                let mut seq = 1usize;
                while archive.exists() && seq <= MAX_UNLIMITED_SHIFT {
                    archive = parent.join(format!("{}.{}.{}{}{}", stem, period, seq, ext, gz));
                    seq += 1;
                }
                check_path_writable(vm, &archive)?;
                if sink.compress {
                    gzip_file(&sink.path, &archive)?;
                } else {
                    fs::rename(&sink.path, &archive)
                        .map_err(|err| io_error("rename", &sink.path, err))?;
                }
                if sink.max_files > 0 {
                    prune_dated_archives(vm, &parent, &stem, &ext, sink.rotation, sink.max_files)?;
                }
            }
        }
    }

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sink.path)
        .map_err(|err| io_error("open", &sink.path, err))?;
    sink.file = Some(file);
    sink.current_bytes = 0;
    sink.current_period = next_period.to_string();
    Ok(())
}

fn rotate_sink(vm: &VM, sink: &mut FileSink, next_period: &str) -> Result<(), String> {
    check_path_writable(vm, &sink.path)?;
    if let Some(mut file) = sink.file.take() {
        let _ = file.flush();
    }
    if let Err(err) = rotate_files(vm, sink, next_period) {
        if sink.file.is_none()
            && let Ok(reopened) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&sink.path)
        {
            sink.current_bytes = reopened.metadata().map(|m| m.len()).unwrap_or(0);
            sink.file = Some(reopened);
        }
        return Err(err);
    }
    Ok(())
}

fn prune_dated_archives(
    vm: &VM,
    parent: &Path,
    stem: &str,
    ext: &str,
    mode: RotationMode,
    max_files: usize,
) -> Result<(), String> {
    let prefix = format!("{}.", stem);
    let mut matches: Vec<((String, usize, String), PathBuf)> = Vec::new();
    let entries = fs::read_dir(parent).map_err(|err| io_error("list", parent, err))?;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Some(after_prefix) = name.strip_prefix(&prefix) else {
            continue;
        };
        let without_gz = after_prefix.strip_suffix(".gz").unwrap_or(after_prefix);
        let Some(stamp) = without_gz.strip_suffix(ext) else {
            continue;
        };
        if let Some((date_part, seq)) = parse_dated_stamp(stamp, mode) {
            matches.push(((date_part.to_string(), seq, name), entry.path()));
        }
    }
    matches.sort_by(|a, b| a.0.cmp(&b.0));
    if matches.len() > max_files {
        let excess = matches.len() - max_files;
        for (_, old_path) in matches.into_iter().take(excess) {
            check_path_writable(vm, &old_path)?;
            fs::remove_file(&old_path).map_err(|err| io_error("remove", &old_path, err))?;
        }
    }
    Ok(())
}

fn file_mtime_timestamp(file: &File) -> Option<Timestamp> {
    let modified = file.metadata().ok()?.modified().ok()?;
    let dur = modified.duration_since(UNIX_EPOCH).ok()?;
    Timestamp::new(dur.as_secs() as i64, dur.subsec_nanos() as i32).ok()
}

/// `__log_file_open(path, maxSize?, rotation?, maxFiles?, utc?, compress?)`:
/// opens a rotating log file sink and returns its numeric handle.
fn builtin_file_open(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let raw_path = str_arg(args, 0, "path")?;
    if raw_path.is_empty() || raw_path.contains('\0') {
        return Err("path must be a non-empty valid file path".to_string());
    }
    let max_size_raw = opt_int_arg(args, 1, "maxSize", 0)?;
    if max_size_raw < 0 {
        return Err("maxSize must be non-negative".to_string());
    }
    let rotation_str = match args.get(2) {
        None | Some(Value::Nil) => "none",
        _ => str_arg(args, 2, "rotation")?,
    };
    let rotation = RotationMode::parse(rotation_str)?;
    let max_files_raw = opt_int_arg(args, 3, "maxFiles", 5)?;
    if max_files_raw < 0 {
        return Err("maxFiles must be non-negative".to_string());
    }
    let utc = match args.get(4) {
        None | Some(Value::Nil) => true,
        _ => bool_arg(args, 4, "utc")?,
    };
    let compress = bool_arg(args, 5, "compress")?;

    let path = PathBuf::from(raw_path);
    check_path_writable(vm, &path)?;

    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|err| io_error("open", &path, err))?;
    let current_bytes = file.metadata().map(|m| m.len()).unwrap_or(0);
    let initial_ts = if current_bytes > 0 {
        file_mtime_timestamp(&file).unwrap_or_else(Timestamp::now)
    } else {
        Timestamp::now()
    };
    let current_period = period_key(initial_ts, rotation, utc);

    let id = next_sink_id();
    FILE_SINKS.with(|sinks| {
        sinks.borrow_mut().insert(
            id,
            FileSink {
                path,
                file: Some(file),
                current_bytes,
                max_size: max_size_raw as u64,
                rotation,
                current_period,
                max_files: max_files_raw as usize,
                utc,
                compress,
            },
        );
    });
    Ok(vec![Value::from_u64(id)])
}

/// `__log_file_write(id, line)`: writes one line to a rotating file sink,
/// rotating beforehand if the time period rolled over or the next write would
/// exceed `maxSize`.
fn builtin_file_write(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = sink_id_arg(args, 0)?;
    let line = str_arg(args, 1, "line")?;
    let line_len = (line.len() as u64).saturating_add(1);

    FILE_SINKS.with(|sinks| {
        let mut map = sinks.borrow_mut();
        let sink = map
            .get_mut(&id)
            .ok_or_else(|| "log file sink is closed".to_string())?;
        let now_period = period_key(Timestamp::now(), sink.rotation, sink.utc);
        let period_changed = sink.rotation != RotationMode::None
            && sink.current_bytes > 0
            && now_period != sink.current_period;
        let size_exceeded = sink.max_size > 0
            && sink.current_bytes > 0
            && sink.current_bytes.saturating_add(line_len) > sink.max_size;
        if period_changed || size_exceeded {
            rotate_sink(vm, sink, &now_period)?;
        } else if sink.rotation != RotationMode::None && sink.current_bytes == 0 {
            sink.current_period = now_period;
        }
        let file = sink
            .file
            .as_mut()
            .ok_or_else(|| "log file sink is not open".to_string())?;
        let mut payload = Vec::with_capacity(line.len() + 1);
        payload.extend_from_slice(line.as_bytes());
        payload.push(b'\n');
        file.write_all(&payload)
            .map_err(|err| io_error("write", &sink.path, err))?;
        sink.current_bytes = sink.current_bytes.saturating_add(line_len);
        Ok(vec![Value::Nil])
    })
}

/// `__log_file_flush(id)`: flushes OS buffers for a file sink.
fn builtin_file_flush(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = sink_id_arg(args, 0)?;
    FILE_SINKS.with(|sinks| {
        let mut map = sinks.borrow_mut();
        let sink = map
            .get_mut(&id)
            .ok_or_else(|| "log file sink is closed".to_string())?;
        if let Some(file) = sink.file.as_mut() {
            file.flush()
                .map_err(|err| io_error("flush", &sink.path, err))?;
        }
        Ok(vec![Value::Nil])
    })
}

/// `__log_file_rotate(id)`: rotates a file sink immediately.
fn builtin_file_rotate(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = sink_id_arg(args, 0)?;
    FILE_SINKS.with(|sinks| {
        let mut map = sinks.borrow_mut();
        let sink = map
            .get_mut(&id)
            .ok_or_else(|| "log file sink is closed".to_string())?;
        let now_period = period_key(Timestamp::now(), sink.rotation, sink.utc);
        rotate_sink(vm, sink, &now_period)?;
        Ok(vec![Value::Nil])
    })
}

/// `__log_file_close(id)`: flushes and closes a file sink. Closing an already
/// closed sink returns `false`.
fn builtin_file_close(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = sink_id_arg(args, 0)?;
    let removed = FILE_SINKS.with(|sinks| sinks.borrow_mut().remove(&id));
    if let Some(mut sink) = removed {
        if let Some(mut file) = sink.file.take() {
            let _ = file.flush();
        }
        return Ok(vec![Value::Bool(true)]);
    }
    Ok(vec![Value::Bool(false)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "neyuki_log_test_{}_{}_{}",
            name,
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_format_timestamp_utc_rfc3339() {
        let ts = Timestamp::new(1_700_000_000, 42_000_000).unwrap();
        assert_eq!(format_timestamp(ts, true), "2023-11-14T22:13:20.042Z");
    }

    #[test]
    fn test_json_line_reserved_key_collision_and_cycle() {
        let mut vm = VM::new();
        let fields = Rc::new(RefCell::new(VmTable::new()));
        fields
            .borrow_mut()
            .set_str("msg", Value::str("user override"));
        fields.borrow_mut().set_str("port", Value::Int(8080));
        fields
            .borrow_mut()
            .set_str("self", Value::Table(fields.clone()));
        fields.borrow_mut().set_str("nan", Value::Float(f64::NAN));

        let out = builtin_json_line(
            &mut vm,
            &[
                Value::str("2026-11-22T09:00:00.000Z"),
                Value::str("info"),
                Value::str("hello\nworld"),
                Value::str("api"),
                Value::Table(fields),
            ],
        )
        .unwrap();
        let Value::String(line) = &out[0] else {
            panic!("expected string");
        };
        assert!(!line.contains('\n'), "JSON line must stay single-line");
        let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(parsed["level"], "info");
        assert_eq!(parsed["logger"], "api");
        assert_eq!(parsed["msg"], "hello\nworld");
        assert_eq!(parsed["field_msg"], "user override");
        assert_eq!(parsed["port"], 8080);
        assert_eq!(parsed["nan"], "NaN");
        assert_eq!(parsed["self"]["self"], "<cycle>");
    }

    #[test]
    fn test_pretty_sanitizes_ansi_escapes() {
        let mut vm = VM::new();
        let out = builtin_format_pretty(
            &mut vm,
            &[
                Value::str("2026-11-22T09:00:00.000Z"),
                Value::str("warn"),
                Value::str("bad \x1b[31minput\r\nforged"),
                Value::Nil,
                Value::Nil,
                Value::Bool(false),
            ],
        )
        .unwrap();
        let Value::String(line) = &out[0] else {
            panic!("expected string");
        };
        assert!(!line.contains('\x1b'));
        assert!(!line.contains('\n'));
        assert!(line.contains("\\x1b[31minput\\r\\nforged"));
    }

    #[test]
    fn test_size_rotation_and_max_files_with_gzip() {
        let dir = temp_dir("size_rot");
        let path = dir.join("app.log");
        let mut vm = VM::new();
        let opened = builtin_file_open(
            &mut vm,
            &[
                Value::string(path.to_string_lossy().into_owned()),
                Value::Int(20),
                Value::str("none"),
                Value::Int(2),
                Value::Bool(true),
                Value::Bool(true),
            ],
        )
        .unwrap();
        let id = opened[0].clone();
        for i in 1..=5 {
            builtin_file_write(
                &mut vm,
                &[id.clone(), Value::string(format!("line-number-{}", i))],
            )
            .unwrap();
        }
        builtin_file_close(&mut vm, &[id]).unwrap();

        assert!(path.exists());
        assert!(dir.join("app.log.1.gz").exists());
        assert!(dir.join("app.log.2.gz").exists());
        assert!(
            !dir.join("app.log.3.gz").exists(),
            "maxFiles=2 must prune older rotated archives"
        );

        let mut gz = GzDecoder::new(File::open(dir.join("app.log.1.gz")).unwrap());
        let mut content = String::new();
        gz.read_to_string(&mut content).unwrap();
        assert!(content.contains("line-number-4"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dated_rotation_pruning() {
        let dir = temp_dir("dated_rot");
        let path = dir.join("svc.jsonl");
        let mut vm = VM::new();
        let opened = builtin_file_open(
            &mut vm,
            &[
                Value::string(path.to_string_lossy().into_owned()),
                Value::Int(0),
                Value::str("daily"),
                Value::Int(2),
                Value::Bool(true),
                Value::Bool(false),
            ],
        )
        .unwrap();
        let id = opened[0].clone();

        // Simulate 4 daily period rollovers deterministically.
        for day in ["2026-11-18", "2026-11-19", "2026-11-20", "2026-11-21"] {
            builtin_file_write(
                &mut vm,
                &[id.clone(), Value::string(format!("day {}", day))],
            )
            .unwrap();
            FILE_SINKS.with(|sinks| {
                let mut map = sinks.borrow_mut();
                let sink = map.get_mut(&(id_from_val(&id))).unwrap();
                sink.current_period = day.to_string();
            });
            builtin_file_rotate(&mut vm, std::slice::from_ref(&id)).unwrap();
        }
        builtin_file_close(&mut vm, &[id]).unwrap();

        assert!(!dir.join("svc.2026-11-18.jsonl").exists());
        assert!(!dir.join("svc.2026-11-19.jsonl").exists());
        assert!(dir.join("svc.2026-11-20.jsonl").exists());
        assert!(dir.join("svc.2026-11-21.jsonl").exists());

        // Dated archives with sequence suffixes sort by (date, seq), so .log (0)
        // and .1.log (1) are older than .2.log (2) and .10.log (10).
        for name in [
            "svc.2026-11-22.jsonl",
            "svc.2026-11-22.1.jsonl",
            "svc.2026-11-22.2.jsonl",
            "svc.2026-11-22.10.jsonl",
        ] {
            fs::write(dir.join(name), "x\n").unwrap();
        }
        prune_dated_archives(&vm, &dir, "svc", ".jsonl", RotationMode::Daily, 2).unwrap();
        assert!(!dir.join("svc.2026-11-20.jsonl").exists());
        assert!(!dir.join("svc.2026-11-21.jsonl").exists());
        assert!(!dir.join("svc.2026-11-22.jsonl").exists());
        assert!(!dir.join("svc.2026-11-22.1.jsonl").exists());
        assert!(dir.join("svc.2026-11-22.2.jsonl").exists());
        assert!(dir.join("svc.2026-11-22.10.jsonl").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_failed_rotation_keeps_sink_open() {
        let dir = temp_dir("rot_recover");
        let path = dir.join("app.log");
        let mut vm = VM::new();
        let opened = builtin_file_open(
            &mut vm,
            &[
                Value::string(path.to_string_lossy().into_owned()),
                Value::Int(0),
                Value::str("none"),
                Value::Int(2),
                Value::Bool(true),
                Value::Bool(false),
            ],
        )
        .unwrap();
        let id = opened[0].clone();
        builtin_file_write(&mut vm, &[id.clone(), Value::str("before")]).unwrap();

        // Create a non-empty directory at app.log.2 so shifting app.log.1 -> app.log.2 fails.
        let blocker1 = dir.join("app.log.1");
        fs::write(&blocker1, "old").unwrap();
        let blocker2 = dir.join("app.log.2");
        fs::create_dir_all(&blocker2).unwrap();
        fs::write(blocker2.join("inside"), "x").unwrap();

        assert!(builtin_file_rotate(&mut vm, std::slice::from_ref(&id)).is_err());

        // Subsequent writes still succeed on the recovered file handle.
        builtin_file_write(&mut vm, &[id.clone(), Value::str("after")]).unwrap();
        builtin_file_close(&mut vm, &[id]).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("before"));
        assert!(text.contains("after"));
        let _ = fs::remove_dir_all(&dir);
    }

    fn id_from_val(v: &Value) -> u64 {
        match v {
            Value::Int(i) => *i as u64,
            _ => panic!("expected int handle"),
        }
    }
}
