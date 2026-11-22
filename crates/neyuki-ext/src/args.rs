//! Argument checks with the runtime's own wording (`src/vm/libs/args.rs`),
//! so a library keeps its error messages when it moves into a package.

use crate::value::{Buffer, Function, Table, Value};

/// A number as a float.
pub fn number(value: &Value) -> Result<f64, String> {
    match value {
        Value::Float(f) => Ok(*f),
        Value::Int(i) => Ok(*i as f64),
        Value::BigInt(_) => Err("integer is too large for floating-point conversion".to_string()),
        _ => Err("expected a number".to_string()),
    }
}

fn missing(name: &str) -> String {
    format!("{} must be provided", name)
}

fn wrong(name: &str, want: &str, got: &Value) -> String {
    format!("{} must be {}, got {}", name, want, got.type_name())
}

/// A named float argument.
pub fn number_arg(args: &[Value], index: usize, name: &str) -> Result<f64, String> {
    match args.get(index) {
        None | Some(Value::Nil) => Err(missing(name)),
        Some(Value::BigInt(_)) => Err(format!("{} is too large", name)),
        Some(v) => number(v).map_err(|_| wrong(name, "a number", v)),
    }
}

/// An optional float argument with a default.
pub fn opt_number_arg(
    args: &[Value],
    index: usize,
    name: &str,
    default: f64,
) -> Result<f64, String> {
    match args.get(index) {
        None | Some(Value::Nil) => Ok(default),
        _ => number_arg(args, index, name),
    }
}

/// A named string argument.
pub fn str_arg<'a>(args: &'a [Value], index: usize, name: &str) -> Result<&'a str, String> {
    match args.get(index) {
        Some(Value::String(s)) => Ok(s),
        None | Some(Value::Nil) => Err(missing(name)),
        Some(other) => Err(wrong(name, "a string", other)),
    }
}

/// An optional string argument.
pub fn opt_str_arg<'a>(
    args: &'a [Value],
    index: usize,
    name: &str,
) -> Result<Option<&'a str>, String> {
    match args.get(index) {
        None | Some(Value::Nil) => Ok(None),
        _ => str_arg(args, index, name).map(Some),
    }
}

/// An optional boolean argument, false when absent.
pub fn bool_arg(args: &[Value], index: usize, name: &str) -> Result<bool, String> {
    match args.get(index) {
        Some(Value::Bool(b)) => Ok(*b),
        None | Some(Value::Nil) => Ok(false),
        Some(other) => Err(wrong(name, "a boolean", other)),
    }
}

/// A named integer argument. Floats with no fractional part are accepted.
pub fn int_arg(args: &[Value], index: usize, name: &str) -> Result<i64, String> {
    match args.get(index) {
        Some(Value::Int(i)) => Ok(*i),
        Some(Value::BigInt(_)) => Err(format!("{} is out of range", name)),
        Some(v @ Value::Float(f)) if f.is_finite() && f.fract() == 0.0 => v
            .as_i64()
            .ok_or_else(|| format!("{} is out of range", name)),
        None | Some(Value::Nil) => Err(missing(name)),
        Some(Value::Float(_)) => Err(format!("{} must be an integer", name)),
        Some(other) => Err(wrong(name, "an integer", other)),
    }
}

/// An optional integer argument with a default.
pub fn opt_int_arg(args: &[Value], index: usize, name: &str, default: i64) -> Result<i64, String> {
    match args.get(index) {
        None | Some(Value::Nil) => Ok(default),
        _ => int_arg(args, index, name),
    }
}

/// A table argument.
pub fn table_arg(args: &[Value], index: usize, name: &str) -> Result<Table, String> {
    match args.get(index) {
        Some(Value::Table(t)) => Ok(*t),
        None | Some(Value::Nil) => Err(missing(name)),
        Some(other) => Err(wrong(name, "a table", other)),
    }
}

/// An optional table argument.
pub fn opt_table_arg(args: &[Value], index: usize, name: &str) -> Result<Option<Table>, String> {
    match args.get(index) {
        None | Some(Value::Nil) => Ok(None),
        _ => table_arg(args, index, name).map(Some),
    }
}

/// A function argument.
pub fn function_arg(args: &[Value], index: usize, name: &str) -> Result<Function, String> {
    match args.get(index) {
        Some(Value::Function(f)) => Ok(*f),
        None | Some(Value::Nil) => Err(missing(name)),
        Some(other) => Err(wrong(name, "a function", other)),
    }
}

/// A buffer argument.
pub fn buffer_arg(args: &[Value], index: usize, name: &str) -> Result<Buffer, String> {
    match args.get(index) {
        Some(Value::Buffer(b)) => Ok(*b),
        None | Some(Value::Nil) => Err(missing(name)),
        Some(other) => Err(wrong(name, "a buffer", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_the_runtime() {
        let args = [
            Value::String("x".into()),
            Value::Float(1.5),
            Value::Nil,
            Value::Float(4.0),
        ];
        assert_eq!(
            number_arg(&args, 0, "width").unwrap_err(),
            "width must be a number, got string"
        );
        assert_eq!(
            number_arg(&args, 2, "width").unwrap_err(),
            "width must be provided"
        );
        assert_eq!(
            number_arg(&args, 9, "width").unwrap_err(),
            "width must be provided"
        );
        assert_eq!(int_arg(&args, 1, "n").unwrap_err(), "n must be an integer");
        assert_eq!(
            int_arg(&args, 0, "n").unwrap_err(),
            "n must be an integer, got string"
        );
        assert_eq!(int_arg(&args, 3, "n").unwrap(), 4);
        assert_eq!(opt_int_arg(&args, 2, "n", 7).unwrap(), 7);
        assert_eq!(
            str_arg(&args, 1, "s").unwrap_err(),
            "s must be a string, got number"
        );
        assert_eq!(opt_str_arg(&args, 2, "s").unwrap(), None);
        assert!(!bool_arg(&args, 2, "b").unwrap());
        assert_eq!(
            bool_arg(&args, 0, "b").unwrap_err(),
            "b must be a boolean, got string"
        );
        assert_eq!(
            table_arg(&args, 0, "t").unwrap_err(),
            "t must be a table, got string"
        );
        assert_eq!(opt_number_arg(&args, 2, "x", 0.25).unwrap(), 0.25);
    }
}
