//! Argument checks and result builders shared by the native libraries.

use num_traits::ToPrimitive;

use crate::vm::heap::VmHeap;
use crate::vm::value::{BorrowedTable, Value, ValueView, VmTable};

/// A number as a float. Integers too large for an `f64` are an error rather
/// than silently becoming infinity.
pub(crate) fn number(value: &Value) -> Result<f64, String> {
    match value.view() {
        ValueView::Float(f) => Ok(f),
        ValueView::Int(i) => Ok(i as f64),
        ValueView::BigInt(i) => i
            .to_f64()
            .filter(|f| f.is_finite())
            .ok_or_else(|| "integer is too large for floating-point conversion".to_string()),
        _ => Err("expected a number".to_string()),
    }
}

/// A named float argument.
pub(crate) fn number_arg(args: &[Value], index: usize, name: &str) -> Result<f64, String> {
    number(args.get(index).unwrap_or(&Value::NIL)).map_err(|_| match args.get(index) {
        None => format!("{} must be provided", name),
        Some(v) if v.is_nil() => format!("{} must be provided", name),
        Some(v) if v.is_bigint() => format!("{} is too large", name),
        Some(other) => format!("{} must be a number, got {}", name, other.type_name()),
    })
}

/// A named string argument.
pub(crate) fn str_arg<'a>(args: &'a [Value], index: usize, name: &str) -> Result<&'a str, String> {
    match args.get(index) {
        Some(v) if v.is_string() => Ok(v.as_str().unwrap()),
        None => Err(format!("{} must be provided", name)),
        Some(v) if v.is_nil() => Err(format!("{} must be provided", name)),
        Some(other) => Err(format!(
            "{} must be a string, got {}",
            name,
            other.type_name()
        )),
    }
}

/// An optional boolean argument, false when absent.
pub(crate) fn bool_arg(args: &[Value], index: usize, name: &str) -> Result<bool, String> {
    match args.get(index) {
        Some(v) if v.is_bool() => Ok(v.as_bool().unwrap()),
        None => Ok(false),
        Some(v) if v.is_nil() => Ok(false),
        Some(other) => Err(format!(
            "{} must be a boolean, got {}",
            name,
            other.type_name()
        )),
    }
}

/// A named integer argument. Floats with no fractional part are accepted.
pub(crate) fn int_arg(args: &[Value], index: usize, name: &str) -> Result<i64, String> {
    let Some(val) = args.get(index) else {
        return Err(format!("{} must be provided", name));
    };
    match val.view() {
        ValueView::Int(i) => Ok(i),
        ValueView::BigInt(_) => Err(format!("{} is out of range", name)),
        ValueView::Float(f) if f.is_finite() && f.fract() == 0.0 => {
            if f >= i64::MIN as f64 && f < i64::MAX as f64 {
                Ok(f as i64)
            } else {
                Err(format!("{} is out of range", name))
            }
        }
        ValueView::Nil => Err(format!("{} must be provided", name)),
        ValueView::Float(_) => Err(format!("{} must be an integer", name)),
        _ => Err(format!(
            "{} must be an integer, got {}",
            name,
            val.type_name()
        )),
    }
}

/// An optional integer argument with a default.
pub(crate) fn opt_int_arg(
    args: &[Value],
    index: usize,
    name: &str,
    default: i64,
) -> Result<i64, String> {
    match args.get(index) {
        None => Ok(default),
        Some(v) if v.is_nil() => Ok(default),
        _ => int_arg(args, index, name),
    }
}

/// A table argument (Old or Beat).
pub(crate) fn table_arg<'a>(
    args: &'a [Value],
    index: usize,
    name: &str,
) -> Result<BorrowedTable<'a>, String> {
    match args.get(index).map(Value::view) {
        Some(ValueView::Table(t)) => Ok(t),
        None | Some(ValueView::Nil) => Err(format!("{} must be provided", name)),
        Some(_) => Err(format!(
            "{} must be a table, got {}",
            name,
            args[index].type_name()
        )),
    }
}

/// A sequence of values as a new array table.
pub(crate) fn array(values: Vec<Value>) -> Value {
    let mut table = VmTable::new();
    table.array = values;
    VmHeap::alloc_table_untracked(table)
}

/// Wraps a frozen library table.
pub(crate) fn frozen(table: VmTable) -> Value {
    VmHeap::alloc_frozen(table)
}
