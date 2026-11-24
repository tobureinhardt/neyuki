//! Native terminal primitives backing `@neyuki/io`.
//!
//! These only move bytes between the standard streams and the runtime;
//! `lib/io.nyk` builds the read formats, stream objects and iterators.

use std::io::{self, BufRead, IsTerminal, Read, Write};

use num_bigint::BigInt;

use crate::vm::libs::args::{bool_arg, number, str_arg};
use crate::vm::machine::VM;
use crate::vm::value::{NativeDef, Value};

pub(crate) const NATIVES: &[NativeDef] = &[
    NativeDef {
        name: "__io_read_line",
        func: builtin_read_line,
    },
    NativeDef {
        name: "__io_read_all",
        func: builtin_read_all,
    },
    NativeDef {
        name: "__io_read_chars",
        func: builtin_read_chars,
    },
    NativeDef {
        name: "__io_read_number",
        func: builtin_read_number,
    },
    NativeDef {
        name: "__io_write",
        func: builtin_write,
    },
    NativeDef {
        name: "__io_flush",
        func: builtin_flush,
    },
    NativeDef {
        name: "__io_isatty",
        func: builtin_isatty,
    },
];

fn count_arg(args: &[Value], index: usize, name: &str) -> Result<usize, String> {
    let count = number(args.get(index).unwrap_or(&Value::NIL))
        .map_err(|_| format!("{} must be a number", name))?;
    if count < 0.0 || count.fract() != 0.0 {
        return Err(format!("{} must be a non-negative integer", name));
    }
    Ok(count as usize)
}

fn io_error(action: &str, stream: &str, err: io::Error) -> String {
    format!("cannot {} {}: {}", action, stream, err)
}

/// Resolves a stream name to a writer; only the two output streams may be
/// written to, `stdin` is read through the dedicated natives.
fn writer(stream: &str) -> Result<Box<dyn Write>, String> {
    match stream {
        "stdout" => Ok(Box::new(io::stdout())),
        "stderr" => Ok(Box::new(io::stderr())),
        _ => Err(format!(
            "invalid output stream `{}` (expected stdout or stderr)",
            stream
        )),
    }
}

/// Reads one line from stdin. Returns nil at end of input; otherwise the
/// line, with its trailing newline (and carriage return) stripped unless
/// `keep_newline` is set.
fn builtin_read_line(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let keep_newline = bool_arg(args, 0, "keepNewline")?;
    let mut line = String::new();
    let read = io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|err| io_error("read", "stdin", err))?;
    if read == 0 {
        return Ok(vec![Value::NIL]);
    }
    if !keep_newline && line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(vec![Value::string(line)])
}

/// Reads everything remaining on stdin. Returns an empty string (never nil)
/// at end of input, matching Lua's `io.read("a")`.
fn builtin_read_all(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    let mut text = String::new();
    io::stdin()
        .lock()
        .read_to_string(&mut text)
        .map_err(|err| io_error("read", "stdin", err))?;
    Ok(vec![Value::string(text)])
}

/// Reads up to `count` characters from stdin. Returns nil when nothing is
/// left; a count of zero returns an empty string unless input is exhausted.
fn builtin_read_chars(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let count = count_arg(args, 0, "count")?;
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    if count == 0 {
        let buffer = stdin
            .fill_buf()
            .map_err(|err| io_error("read", "stdin", err))?;
        return Ok(vec![if buffer.is_empty() {
            Value::NIL
        } else {
            Value::str("")
        }]);
    }
    let mut text = String::new();
    let mut read = 0usize;
    let mut pending: Vec<u8> = Vec::new();
    let mut bytes = stdin.bytes();
    while read < count {
        match bytes.next() {
            Some(Ok(byte)) => {
                pending.push(byte);
                if let Ok(chunk) = std::str::from_utf8(&pending) {
                    text.push_str(chunk);
                    pending.clear();
                    read += 1;
                } else if pending.len() >= 4 {
                    return Err("stdin is not valid UTF-8".to_string());
                }
            }
            Some(Err(err)) => return Err(io_error("read", "stdin", err)),
            None => break,
        }
    }
    if !pending.is_empty() {
        return Err("stdin is not valid UTF-8".to_string());
    }
    if text.is_empty() {
        return Ok(vec![Value::NIL]);
    }
    Ok(vec![Value::string(text)])
}

/// Reads one whitespace-delimited token from stdin and converts it to a
/// number. Returns nil at end of input or when the token is not numeric;
/// the token is consumed either way.
fn builtin_read_number(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut token: Vec<u8> = Vec::new();
    loop {
        let buffer = stdin
            .fill_buf()
            .map_err(|err| io_error("read", "stdin", err))?;
        if buffer.is_empty() {
            break;
        }
        let mut consumed = 0;
        let mut done = false;
        for &byte in buffer {
            consumed += 1;
            if byte.is_ascii_whitespace() {
                if !token.is_empty() {
                    done = true;
                    break;
                }
            } else {
                token.push(byte);
            }
        }
        stdin.consume(consumed);
        if done {
            break;
        }
    }
    if token.is_empty() {
        return Ok(vec![Value::NIL]);
    }
    let token = String::from_utf8_lossy(&token).replace('_', "");
    let integer = if let Some(hex) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        BigInt::parse_bytes(hex.as_bytes(), 16)
    } else if let Some(bin) = token
        .strip_prefix("0b")
        .or_else(|| token.strip_prefix("0B"))
    {
        BigInt::parse_bytes(bin.as_bytes(), 2)
    } else {
        BigInt::parse_bytes(token.as_bytes(), 10)
    };
    Ok(vec![if let Some(value) = integer {
        Value::from_bigint(value)
    } else if let Ok(value) = token.parse::<f64>() {
        Value::float(value)
    } else {
        Value::NIL
    }])
}

fn builtin_write(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let stream = str_arg(args, 0, "stream")?;
    let text = str_arg(args, 1, "text")?;
    if vm.dap.is_some() {
        writer(stream)?;
        crate::dap::output(vm, stream, text);
        return Ok(vec![Value::NIL]);
    }
    writer(stream)?
        .write_all(text.as_bytes())
        .map_err(|err| io_error("write to", stream, err))?;
    Ok(vec![Value::NIL])
}

fn builtin_flush(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let stream = str_arg(args, 0, "stream")?;
    writer(stream)?
        .flush()
        .map_err(|err| io_error("flush", stream, err))?;
    Ok(vec![Value::NIL])
}

fn builtin_isatty(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let stream = str_arg(args, 0, "stream")?;
    let is_terminal = match stream {
        "stdin" => io::stdin().is_terminal(),
        "stdout" => io::stdout().is_terminal(),
        "stderr" => io::stderr().is_terminal(),
        _ => {
            return Err(format!(
                "invalid stream `{}` (expected stdin, stdout or stderr)",
                stream
            ));
        }
    };
    Ok(vec![Value::bool(is_terminal)])
}
