//! The `os` library.

use num_bigint::BigInt;
use num_traits::ToPrimitive;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use jiff::fmt::strtime::{BrokenDownTime, Config, PosixCustom};
use jiff::tz::{AmbiguousOffset, TimeZone};
use jiff::{SignedDuration, Timestamp, Zoned, civil};

use crate::vm::machine::VM;
use crate::vm::value::{Value, ValueView, VmTable};

mod storage;

fn os_clock(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Ok(vec![Value::float(now.as_secs_f64())])
}

/// `time()` is the current time in whole seconds since the Unix epoch;
/// `time(t)` converts a local date table (`year`, `month`, `day`, and
/// optionally `hour` (default 12), `min` and `sec`) the same way, with
/// out-of-range fields carried over as in Lua (month 13 is next January).
fn os_time(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(fields) = args.first().and_then(Value::as_table) else {
        if let Some(other) = args.first().filter(|v| !v.is_nil()) {
            return Err(format!("t must be a table, got {}", other.type_name()));
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        return Ok(vec![Value::from_bigint(BigInt::from(now.as_secs()))]);
    };
    let fields = fields.borrow();
    let field = |name: &str, default: Option<i64>| -> Result<i64, String> {
        let v = fields.get_str(name);
        match v.view() {
            ValueView::Nil => default.ok_or_else(|| format!("field '{}' is missing", name)),
            ValueView::Int(i) => Ok(i),
            ValueView::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => Ok(f as i64),
            _ => Err(format!(
                "field '{}' must be an integer, got {}",
                name,
                v.type_name()
            )),
        }
    };
    let (year, month, day) = (
        field("year", None)?,
        field("month", None)?,
        field("day", None)?,
    );
    let (hour, min, sec) = (
        field("hour", Some(12))?,
        field("min", Some(0))?,
        field("sec", Some(0))?,
    );
    let months = year
        .checked_mul(12)
        .and_then(|m| m.checked_add(month - 1))
        .ok_or("date is out of range")?;
    let first = i16::try_from(months.div_euclid(12))
        .ok()
        .and_then(|y| civil::Date::new(y, months.rem_euclid(12) as i8 + 1, 1).ok())
        .ok_or("date is out of range")?;
    let offset = (day - 1)
        .checked_mul(86400)
        .zip(hour.checked_mul(3600))
        .zip(min.checked_mul(60))
        .and_then(|((d, h), m)| d.checked_add(h)?.checked_add(m)?.checked_add(sec));
    let naive = offset
        .and_then(|offset| {
            first
                .to_datetime(civil::Time::midnight())
                .checked_add(SignedDuration::from_secs(offset))
                .ok()
        })
        .ok_or("date is out of range")?;
    let local = TimeZone::system().to_ambiguous_timestamp(naive);
    if matches!(local.offset(), AmbiguousOffset::Gap { .. }) {
        return Err("that local time does not exist".to_string());
    }
    let local = local.earlier().map_err(|_| "date is out of range")?;
    Ok(vec![Value::int(local.as_second())])
}

/// A time as the table `date("*t")` returns.
fn date_table(time: &Zoned) -> Value {
    let mut table = VmTable::new();
    table.set_str("year", Value::int(time.year() as i64));
    table.set_str("month", Value::int(time.month() as i64));
    table.set_str("day", Value::int(time.day() as i64));
    table.set_str("hour", Value::int(time.hour() as i64));
    table.set_str("min", Value::int(time.minute() as i64));
    table.set_str("sec", Value::int(time.second() as i64));
    // Lua counts weekdays from Sunday = 1.
    table.set_str(
        "wday",
        Value::int(time.weekday().to_sunday_one_offset() as i64),
    );
    table.set_str("yday", Value::int(time.day_of_year() as i64));
    Value::table(Rc::new(RefCell::new(table)))
}

/// Formats a time with strftime codes, with `%c`, `%x` and `%X` laid out as
/// in the C locale. Unknown codes are errors.
fn format_time(time: &Zoned, format: &str) -> Result<String, String> {
    let config = Config::new().custom(PosixCustom::new());
    BrokenDownTime::from(time)
        .to_string_with_config(&config, format)
        .map_err(|_| format!("invalid date format `{}`", format))
}

/// `date(format?, time?)`: formats `time` (default now, in seconds since the
/// epoch) in local time with strftime codes (default "%c"). A leading "!"
/// uses UTC instead, and "*t" returns a table of the fields.
fn os_date(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let format = match args.first() {
        None => "%c",
        Some(v) if v.is_nil() => "%c",
        Some(v) if v.is_string() => v.as_str().unwrap(),
        Some(other) => {
            return Err(format!(
                "format must be a string, got {}",
                other.type_name()
            ));
        }
    };
    let seconds = match args.get(1) {
        None => None,
        Some(v) if v.is_nil() => None,
        Some(value) => Some(
            crate::vm::libs::args::number(value)
                .map_err(|_| format!("time must be a number, got {}", value.type_name()))?,
        ),
    };
    let timestamp = match seconds {
        None => Timestamp::now(),
        Some(seconds) => {
            let whole = seconds.floor();
            Some(seconds)
                .filter(|s| s.is_finite())
                .and_then(|_| Timestamp::new(whole as i64, ((seconds - whole) * 1e9) as i32).ok())
                .ok_or("time is out of range")?
        }
    };
    let (utc, format) = match format.strip_prefix('!') {
        Some(rest) => (true, rest),
        None => (false, format),
    };
    let zone = if utc {
        TimeZone::UTC
    } else {
        TimeZone::system()
    };
    let time = timestamp.to_zoned(zone);
    let value = match format {
        "*t" => date_table(&time),
        _ => Value::string(format_time(&time, format)?),
    };
    Ok(vec![value])
}

fn os_difftime(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let t2 = match args
        .first()
        .ok_or_else(|| "os.difftime expects 2 arguments".to_string())?
        .view()
    {
        ValueView::Int(i) => i.to_f64().unwrap_or(0.0),
        ValueView::Float(f) => f,
        _ => return Err("os.difftime expects numbers".to_string()),
    };
    let t1 = match args
        .get(1)
        .ok_or_else(|| "os.difftime expects 2 arguments".to_string())?
        .view()
    {
        ValueView::Int(i) => i.to_f64().unwrap_or(0.0),
        ValueView::Float(f) => f,
        _ => return Err("os.difftime expects numbers".to_string()),
    };
    Ok(vec![Value::float(t2 - t1)])
}

static SCRIPT_ARGS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();

/// Records the command-line arguments that follow the script path, for
/// `os.args`. Only the first call has an effect.
pub fn set_script_args(args: Vec<String>) {
    let _ = SCRIPT_ARGS.set(args);
}

/// The script's command-line arguments as a new list of strings.
fn os_args(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    let args = SCRIPT_ARGS.get().map(Vec::as_slice).unwrap_or_default();
    Ok(vec![crate::vm::libs::args::array(
        args.iter().map(|a| Value::string(a.clone())).collect(),
    )])
}

fn string_field(table: &VmTable, name: &str) -> Result<Option<String>, String> {
    let v = table.get_str(name);
    match v.view() {
        ValueView::Nil => Ok(None),
        ValueView::String(s) => Ok(Some(s.to_string())),
        _ => Err(format!("{} must be a string, got {}", name, v.type_name())),
    }
}

/// `run(command, options?)`: runs a program and waits for it. A string runs
/// through the system shell (`cmd /C` on Windows, `sh -c` elsewhere); a list
/// runs its first element with the rest as arguments, with no shell
/// involved. `options` may hold `cwd`, `env` (variables to add or override),
/// `stdin` (text to feed it) and `capture` (default true; false lets output
/// go straight to the terminal). Returns `{ ok, code, stdout, stderr }`.
fn os_run(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut command = match args.first().map(Value::view) {
        Some(ValueView::String(line)) => {
            vm.permissions.check_run_shell(&line)?;
            let mut command = if cfg!(windows) {
                Command::new("cmd")
            } else {
                Command::new("sh")
            };
            command
                .arg(if cfg!(windows) { "/C" } else { "-c" })
                .arg(line.as_str());
            command
        }
        Some(ValueView::Table(list)) => {
            let list = list.borrow();
            let mut parts = Vec::with_capacity(list.array.len());
            for (index, part) in list.array.iter().enumerate() {
                if let Some(s) = part.as_str() {
                    parts.push(s.to_string());
                } else {
                    return Err(format!(
                        "command[{}] must be a string, got {}",
                        index + 1,
                        part.type_name()
                    ));
                }
            }
            let (program, rest) = parts.split_first().ok_or("command must not be empty")?;
            let program = vm.permissions.resolve_run_program(program)?;
            let mut command = Command::new(program);
            command.args(rest);
            command
        }
        None | Some(ValueView::Nil) => return Err("command must be provided".to_string()),
        Some(_) => {
            return Err(format!(
                "command must be a string or a list of strings, got {}",
                args[0].type_name()
            ));
        }
    };

    let mut capture = true;
    let mut input = None;
    match args.get(1).map(Value::view) {
        None | Some(ValueView::Nil) => {}
        Some(ValueView::Table(options)) => {
            let options = options.borrow();
            if let Some(cwd) = string_field(&options, "cwd")? {
                command.current_dir(cwd);
            }
            input = string_field(&options, "stdin")?;
            let cap_val = options.get_str("capture");
            match cap_val.view() {
                ValueView::Nil => {}
                ValueView::Bool(b) => capture = b,
                _ => {
                    return Err(format!(
                        "capture must be a boolean, got {}",
                        cap_val.type_name()
                    ));
                }
            }
            let env_val = options.get_str("env");
            match env_val.view() {
                ValueView::Nil => {}
                ValueView::Table(env) => {
                    for (key, value) in env.borrow().fields.iter() {
                        if let Some(v) = value.as_str() {
                            command.env(&**key, v);
                        } else {
                            return Err(format!(
                                "env.{} must be a string, got {}",
                                &**key,
                                value.type_name()
                            ));
                        }
                    }
                }
                _ => {
                    return Err(format!("env must be a table, got {}", env_val.type_name()));
                }
            }
        }
        Some(_) => {
            return Err(format!(
                "options must be a table, got {}",
                args[1].type_name()
            ));
        }
    }

    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::inherit()
    });
    if capture {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    // Inside a task the program runs and is waited for on a worker thread,
    // so only that task waits; see `background::run`.
    crate::vm::libs::background::run(
        vm,
        move || {
            let mut child = command
                .spawn()
                .map_err(|err| format!("cannot run command: {}", err))?;
            if let Some(text) = input
                && let Some(mut stdin) = child.stdin.take()
            {
                // Fed from a thread so a program that writes before reading
                // all of its input cannot deadlock against us.
                let bytes = text.into_bytes();
                std::thread::spawn(move || {
                    let _ = stdin.write_all(&bytes);
                });
            }
            child
                .wait_with_output()
                .map_err(|err| format!("cannot run command: {}", err))
        },
        |_vm, output| Ok(vec![run_result(output)]),
    )
}

/// The `{ ok, code, stdout, stderr }` table `run` returns.
fn run_result(output: std::process::Output) -> Value {
    let mut result = VmTable::new();
    result.set_str("ok", Value::bool(output.status.success()));
    result.set_str(
        "code",
        output
            .status
            .code()
            .map_or(Value::NIL, |c| Value::int(c as i64)),
    );
    result.set_str(
        "stdout",
        Value::string(String::from_utf8_lossy(&output.stdout).into_owned()),
    );
    result.set_str(
        "stderr",
        Value::string(String::from_utf8_lossy(&output.stderr).into_owned()),
    );
    Value::table(Rc::new(RefCell::new(result)))
}

/// Blocks the script for `seconds` (fractions allowed).
fn os_sleep(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let seconds = crate::vm::libs::args::number_arg(args, 0, "seconds")?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err("seconds must be a non-negative number".to_string());
    }
    std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    Ok(vec![])
}

/// Ends the process. `true` or nothing exits with 0, `false` with 1 and an
/// integer with that code. Standard output is flushed first.
fn os_exit(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let code = match args.first().map(Value::view) {
        None | Some(ValueView::Nil | ValueView::Bool(true)) => 0,
        Some(ValueView::Bool(false)) => 1,
        Some(ValueView::Int(i)) => i32::try_from(i).map_err(|_| "exit code is out of range")?,
        Some(_) => {
            return Err(format!(
                "code must be a boolean or an integer, got {}",
                args[0].type_name()
            ));
        }
    };
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code)
}

/// The value of an environment variable, or nil when it is unset or not
/// valid Unicode.
fn os_getenv(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let varname = args
        .first()
        .ok_or_else(|| "os.getenv expects variable name".to_string())?
        .as_str()
        .ok_or_else(|| "os.getenv expects string".to_string())?;
    vm.permissions.check_env(varname)?;
    match std::env::var(varname) {
        Ok(v) => Ok(vec![Value::string(v)]),
        Err(_) => Ok(vec![Value::NIL]),
    }
}

/// `app()` describes the app built with `neyuki build` that is running:
/// `{ bundled = true, name, version, identifier }`, or `{ bundled = false }`
/// under `neyuki run`.
fn os_app(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    let mut table = VmTable::new();
    let bundle = crate::app::installed();
    table.set_str("bundled", Value::bool(bundle.is_some()));
    if let Some(bundle) = bundle {
        for key in ["name", "version", "identifier"] {
            if let Some(value) = bundle.meta(key) {
                table.set_str(key, Value::string(value.to_string()));
            }
        }
    }
    Ok(vec![crate::vm::libs::args::frozen(table)])
}

pub fn create_os_lib() -> Value {
    let mut table = VmTable::new();
    table.set_str("clock", crate::native!("os.clock", os_clock));
    table.set_str("time", crate::native!("os.time", os_time));
    table.set_str("date", crate::native!("os.date", os_date));
    table.set_str("difftime", crate::native!("os.difftime", os_difftime));
    table.set_str("getenv", crate::native!("os.getenv", os_getenv));
    table.set_str("sleep", crate::native!("os.sleep", os_sleep));
    table.set_str("exit", crate::native!("os.exit", os_exit));
    table.set_str("args", crate::native!("os.args", os_args));
    table.set_str("run", crate::native!("os.run", os_run));
    table.set_str("app", crate::native!("os.app", os_app));
    table.set_str(
        "spawn",
        crate::native!("os.spawn", crate::vm::libs::net::os_spawn),
    );
    table.set_str(
        "dataDir",
        crate::native!("os.dataDir", storage::os_data_dir),
    );
    table.set_str(
        "configDir",
        crate::native!("os.configDir", storage::os_config_dir),
    );
    table.set_str(
        "cacheDir",
        crate::native!("os.cacheDir", storage::os_cache_dir),
    );
    table.set_str("protect", crate::native!("os.protect", storage::os_protect));
    table.set_str(
        "unprotect",
        crate::native!("os.unprotect", storage::os_unprotect),
    );
    table.frozen = true;
    Value::table(Rc::new(RefCell::new(table)))
}

#[cfg(all(test, unix))]
mod run_sandbox_tests {
    use super::*;
    use crate::vm::permissions::{PermissionScope, Permissions};
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn script(path: &Path, marker: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn call(vm: &mut VM, cmd: &[&str], opts: &[(&str, Value)]) -> Result<Vec<Value>, String> {
        let mut list = VmTable::new();
        for c in cmd {
            list.array.push(Value::str(c));
        }
        let mut o = VmTable::new();
        for (k, v) in opts {
            o.set_str(k, v.clone());
        }
        let wrap = |t: VmTable| Value::table(std::rc::Rc::new(std::cell::RefCell::new(t)));
        os_run(vm, &[wrap(list), wrap(o)])
    }

    fn table(pairs: &[(&str, &str)]) -> Value {
        let mut t = VmTable::new();
        for (k, v) in pairs {
            t.set_str(k, Value::str(v));
        }
        Value::table(std::rc::Rc::new(std::cell::RefCell::new(t)))
    }

    fn vm_with_run(list: &[&str]) -> VM {
        let mut vm = VM::new();
        vm.set_permissions(
            Permissions::allow_all().with_run(PermissionScope::AllowList(
                list.iter().map(|s| s.to_string()).collect(),
            )),
        );
        vm
    }

    #[test]
    fn allow_list_bare_name_ignores_child_path_override() {
        let base = std::env::temp_dir().join(format!("neyuki_run_path_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let marker = base.join("evil_ran");
        script(&base.join("bin/true"), &marker);
        let mut vm = vm_with_run(&["true"]);
        let env = table(&[("PATH", &base.join("bin").to_string_lossy())]);
        let _ = call(&mut vm, &["true"], &[("env", env)]);
        assert!(
            !marker.exists(),
            "child PATH override ran an unlisted binary"
        );
        // The legitimate binary still runs.
        let res = call(&mut vm, &["true"], &[]).unwrap();
        assert!(res[0].is_table());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn allow_list_relative_path_ignores_child_cwd() {
        let id = std::process::id();
        let rel = format!("target/neyuki_run_cwd_{id}/prog");
        let good_marker = std::env::temp_dir().join(format!("neyuki_good_{id}"));
        let evil_root = std::env::temp_dir().join(format!("neyuki_evil_{id}"));
        let evil_marker = std::env::temp_dir().join(format!("neyuki_evil_ran_{id}"));
        let _ = std::fs::remove_file(&good_marker);
        let _ = std::fs::remove_file(&evil_marker);
        script(Path::new(&rel), &good_marker);
        script(&evil_root.join(&rel), &evil_marker);
        let mut vm = vm_with_run(&[&format!("./{rel}")]);
        let cwd = Value::str(&evil_root.to_string_lossy());
        let _ = call(&mut vm, &[&format!("./{rel}")], &[("cwd", cwd)]);
        assert!(!evil_marker.exists(), "child cwd redirected the program");
        assert!(good_marker.exists(), "allowed program did not run");
        let _ = std::fs::remove_dir_all(format!("target/neyuki_run_cwd_{id}"));
        let _ = std::fs::remove_dir_all(&evil_root);
        let _ = std::fs::remove_file(&good_marker);
    }
}
