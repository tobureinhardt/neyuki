//! Script values and the handles behind tables, functions and buffers.

use std::fmt;

use crate::api;
use crate::sys::{self, RawValue};

/// A Neyuki value as an extension sees it.
///
/// Scalars and strings are copied in. Tables, functions, buffers and big
/// integers are handles to host values: valid until the extension call that
/// received or made them returns, unless kept with `retain`. Using a handle
/// after that is an error from the host, never undefined behaviour.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    Table(Table),
    Function(Function),
    Buffer(Buffer),
    /// An integer outside `i64`; `to_string` gives its digits.
    BigInt(BigInt),
}

/// A handle id as the host hands it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Handle(pub(crate) i64);

impl Handle {
    pub(crate) fn raw(self) -> RawValue {
        RawValue::with(sys::TAG_HANDLE, self.0, 0.0, std::ptr::null(), 0)
    }
}

/// A table handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Table(pub(crate) Handle);
/// A function handle (a script closure or a native).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Function(pub(crate) Handle);
/// A `buffer` handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Buffer(pub(crate) Handle);
/// A big integer handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BigInt(pub(crate) Handle);

impl Value {
    /// The name `type()` gives, as the runtime's own error messages use it.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) | Value::Float(_) | Value::BigInt(_) => "number",
            Value::String(_) => "string",
            Value::Table(_) => "table",
            Value::Function(_) => "function",
            Value::Buffer(_) => "buffer",
        }
    }

    pub fn is_nil(&self) -> bool {
        matches!(self, Value::Nil)
    }

    /// Lua truthiness: everything but `nil` and `false`.
    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_table(&self) -> Option<Table> {
        match self {
            Value::Table(t) => Some(*t),
            _ => None,
        }
    }

    /// The value as a float, for any number that fits.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// The value as an integer: an `Int`, or a `Float` with no fraction.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Float(f) if f.is_finite() && f.fract() == 0.0 => {
                (*f >= i64::MIN as f64 && *f < i64::MAX as f64).then_some(*f as i64)
            }
            _ => None,
        }
    }

    /// Borrows the value as a raw value. Strings point into `self`, so the
    /// result must only be used while `self` is alive.
    pub(crate) fn raw(&self) -> RawValue {
        match self {
            Value::Nil => RawValue::NIL,
            Value::Bool(b) => {
                RawValue::with(sys::TAG_BOOL, i64::from(*b), 0.0, std::ptr::null(), 0)
            }
            Value::Int(i) => RawValue::with(sys::TAG_INT, *i, 0.0, std::ptr::null(), 0),
            Value::Float(f) => RawValue::with(sys::TAG_FLOAT, 0, *f, std::ptr::null(), 0),
            Value::String(s) => RawValue::bytes(sys::TAG_STRING, s.as_bytes()),
            Value::Table(Table(h))
            | Value::Function(Function(h))
            | Value::Buffer(Buffer(h))
            | Value::BigInt(BigInt(h)) => h.raw(),
        }
    }

    /// Copies a raw value from the host. `ERROR` becomes `Err` with its
    /// message.
    ///
    /// # Safety
    /// A `STRING` or `ERROR` must point at `len` readable bytes.
    pub(crate) unsafe fn from_raw(raw: &RawValue) -> Result<Value, String> {
        Ok(match raw.tag {
            sys::TAG_NIL => Value::Nil,
            sys::TAG_BOOL => Value::Bool(raw.i != 0),
            sys::TAG_INT => Value::Int(raw.i),
            sys::TAG_FLOAT => Value::Float(raw.f),
            sys::TAG_STRING => Value::String(unsafe { raw_string(raw) }),
            sys::TAG_ERROR => return Err(unsafe { raw_string(raw) }),
            sys::TAG_HANDLE => {
                let h = Handle(raw.i);
                match unsafe { (api().handle_kind)(*raw) } {
                    sys::KIND_TABLE => Value::Table(Table(h)),
                    sys::KIND_FUNCTION => Value::Function(Function(h)),
                    sys::KIND_BUFFER => Value::Buffer(Buffer(h)),
                    sys::KIND_BIGINT => Value::BigInt(BigInt(h)),
                    _ => return Err("invalid or expired handle".to_string()),
                }
            }
            other => return Err(format!("unexpected value tag {} from the host", other)),
        })
    }
}

/// The bytes of a raw `STRING`/`ERROR` as a `String` (lossy UTF-8).
///
/// # Safety
/// `raw.ptr` must point at `raw.len` readable bytes, or be null.
pub(crate) unsafe fn raw_string(raw: &RawValue) -> String {
    if raw.ptr.is_null() || raw.len == 0 {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(raw.ptr, raw.len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Checks a host result that is `NIL` or `ERROR`.
pub(crate) fn status(raw: RawValue) -> Result<(), String> {
    if raw.tag == sys::TAG_ERROR {
        Err(unsafe { raw_string(&raw) })
    } else {
        Ok(())
    }
}

/// Copies a host result that should be a value.
pub(crate) fn result(raw: RawValue) -> Result<Value, String> {
    unsafe { Value::from_raw(&raw) }
}

impl fmt::Display for Value {
    /// Scalars as `tostring` prints them; handles through the host.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Nil => f.write_str("nil"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Int(i) => write!(f, "{}", i),
            Value::String(s) => f.write_str(s),
            other => match to_string(other) {
                Ok(s) => f.write_str(&s),
                Err(_) => write!(f, "{}", other.type_name()),
            },
        }
    }
}

/// `tostring(value)`, through the host.
pub fn to_string(value: &Value) -> Result<String, String> {
    let raw = unsafe { (api().to_string)(value.raw()) };
    match result(raw)? {
        Value::String(s) => Ok(s),
        other => Err(format!("tostring returned {}", other.type_name())),
    }
}

impl Table {
    /// A new empty table.
    pub fn new() -> Result<Table, String> {
        Table::with_capacity(0, 0)
    }

    /// A new table with room for `array` list items and `fields` keys.
    pub fn with_capacity(array: usize, fields: usize) -> Result<Table, String> {
        match result(unsafe { (api().new_table)(array, fields) })? {
            Value::Table(t) => Ok(t),
            other => Err(format!("new_table returned {}", other.type_name())),
        }
    }

    /// A list of `values`.
    pub fn from_list<V: Into<Value>>(values: impl IntoIterator<Item = V>) -> Result<Table, String> {
        let values: Vec<Value> = values.into_iter().map(Into::into).collect();
        let t = Table::with_capacity(values.len(), 0)?;
        for (i, v) in values.iter().enumerate() {
            t.set(i as i64 + 1, v.clone())?;
        }
        Ok(t)
    }

    /// A record of `fields`.
    pub fn from_fields<'a>(
        fields: impl IntoIterator<Item = (&'a str, Value)>,
    ) -> Result<Table, String> {
        let fields: Vec<(&str, Value)> = fields.into_iter().collect();
        let t = Table::with_capacity(0, fields.len())?;
        for (k, v) in fields {
            t.set(k, v)?;
        }
        Ok(t)
    }

    /// `t[key]`, raw (no metamethods).
    pub fn get(&self, key: impl Into<Value>) -> Result<Value, String> {
        let key = key.into();
        result(unsafe { (api().table_get)(self.0.raw(), key.raw()) })
    }

    /// `t[key] = value`, raw. Fails on a frozen table or a nil key.
    pub fn set(&self, key: impl Into<Value>, value: impl Into<Value>) -> Result<(), String> {
        let (key, value) = (key.into(), value.into());
        status(unsafe { (api().table_set)(self.0.raw(), key.raw(), value.raw()) })
    }

    /// `#t`: the length of the list part.
    pub fn len(&self) -> usize {
        unsafe { (api().table_len)(self.0.raw()) }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends `value` to the list part.
    pub fn push(&self, value: impl Into<Value>) -> Result<(), String> {
        self.set(self.len() as i64 + 1, value)
    }

    /// The list part, `t[1]` to `t[#t]`.
    pub fn list(&self) -> Result<Vec<Value>, String> {
        (1..=self.len() as i64).map(|i| self.get(i)).collect()
    }

    /// Every key and value, in `next` order.
    pub fn pairs(&self) -> Result<Vec<(Value, Value)>, String> {
        let mut out = Vec::new();
        let mut key = Value::Nil;
        loop {
            let (mut k, mut v) = (RawValue::NIL, RawValue::NIL);
            let more = unsafe { (api().table_next)(self.0.raw(), key.raw(), &mut k, &mut v) };
            if !more {
                return Ok(out);
            }
            let k = unsafe { Value::from_raw(&k)? };
            let v = unsafe { Value::from_raw(&v)? };
            key = k.clone();
            out.push((k, v));
        }
    }

    /// Keeps the table past the current call.
    pub fn retain(&self) -> Result<Persistent, String> {
        Persistent::new(self.0)
    }
}

impl Function {
    /// Calls the function with `args` and returns its results.
    pub fn call(&self, args: &[Value]) -> Result<Vec<Value>, String> {
        let raw_args: Vec<RawValue> = args.iter().map(Value::raw).collect();
        let raw = unsafe { (api().call)(self.0.raw(), raw_args.len(), raw_args.as_ptr()) };
        if raw.tag == sys::TAG_MULTI {
            let items = if raw.len == 0 || raw.ptr.is_null() {
                &[][..]
            } else {
                unsafe { std::slice::from_raw_parts(raw.ptr.cast::<RawValue>(), raw.len) }
            };
            return items
                .iter()
                .map(|v| unsafe { Value::from_raw(v) })
                .collect();
        }
        match result(raw)? {
            Value::Nil => Ok(Vec::new()),
            v => Ok(vec![v]),
        }
    }

    /// Keeps the function past the current call, to call it later.
    pub fn retain(&self) -> Result<Persistent, String> {
        Persistent::new(self.0)
    }
}

impl Buffer {
    /// A new buffer holding a copy of `bytes`.
    pub fn new(bytes: &[u8]) -> Result<Buffer, String> {
        Buffer::make(bytes.as_ptr(), bytes.len())
    }

    /// A new zero-filled buffer of `len` bytes.
    pub fn zeroed(len: usize) -> Result<Buffer, String> {
        Buffer::make(std::ptr::null(), len)
    }

    fn make(data: *const u8, len: usize) -> Result<Buffer, String> {
        match result(unsafe { (api().new_buffer)(data, len) })? {
            Value::Buffer(b) => Ok(b),
            other => Err(format!("new_buffer returned {}", other.type_name())),
        }
    }

    /// Runs `f` on the buffer's bytes, which it may change. No script code
    /// can run meanwhile, so the bytes cannot move under it.
    pub fn with_bytes<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, String> {
        let (mut ptr, mut len) = (std::ptr::null_mut(), 0usize);
        if !unsafe { (api().buffer_data)(self.0.raw(), &mut ptr, &mut len) } {
            return Err("expected a live buffer".to_string());
        }
        let bytes = if ptr.is_null() || len == 0 {
            &mut [][..]
        } else {
            // SAFETY: the host hands out the buffer's own storage, valid
            // until script code runs again or the call returns; `f` gets
            // no way to run script code.
            unsafe { std::slice::from_raw_parts_mut(ptr, len) }
        };
        Ok(f(bytes))
    }

    /// A copy of the buffer's bytes.
    pub fn to_vec(&self) -> Result<Vec<u8>, String> {
        self.with_bytes(|b| b.to_vec())
    }
}

/// A handle kept past the call that received it (`api->retain`); released
/// when dropped.
#[derive(Debug)]
pub struct Persistent(Handle);

impl Persistent {
    fn new(h: Handle) -> Result<Persistent, String> {
        match unsafe { (api().retain)(h.raw()) } {
            raw if raw.tag == sys::TAG_HANDLE => Ok(Persistent(Handle(raw.i))),
            raw => Err(status(raw)
                .err()
                .unwrap_or_else(|| "retain failed".to_string())),
        }
    }

    /// The kept value. Only usable inside an extension call.
    pub fn value(&self) -> Result<Value, String> {
        unsafe { Value::from_raw(&self.0.raw()) }
    }
}

impl Drop for Persistent {
    fn drop(&mut self) {
        // The host accepts a release from any context.
        if let Some(api) = crate::try_api() {
            unsafe { (api.release)(self.0.raw()) };
        }
    }
}

macro_rules! from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Value {
            fn from(v: $t) -> Value {
                Value::Int(i64::from(v))
            }
        }
    )*};
}
from_int!(i8, i16, i32, i64, u8, u16, u32);

/// Unsigned sizes beyond `i64::MAX` become floats, as the runtime does.
impl From<usize> for Value {
    fn from(v: usize) -> Value {
        i64::try_from(v).map_or(Value::Float(v as f64), Value::Int)
    }
}

impl From<u64> for Value {
    fn from(v: u64) -> Value {
        i64::try_from(v).map_or(Value::Float(v as f64), Value::Int)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Value {
        Value::Float(v)
    }
}

impl From<f32> for Value {
    fn from(v: f32) -> Value {
        Value::Float(f64::from(v))
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Value {
        Value::Bool(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Value {
        Value::String(v.to_string())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Value {
        Value::String(v)
    }
}

impl From<&String> for Value {
    fn from(v: &String) -> Value {
        Value::String(v.clone())
    }
}

impl From<Table> for Value {
    fn from(v: Table) -> Value {
        Value::Table(v)
    }
}

impl From<Function> for Value {
    fn from(v: Function) -> Value {
        Value::Function(v)
    }
}

impl From<Buffer> for Value {
    fn from(v: Buffer) -> Value {
        Value::Buffer(v)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Value {
        v.map_or(Value::Nil, Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_follow_the_runtime() {
        assert_eq!(Value::from(3u8), Value::Int(3));
        assert_eq!(Value::from(usize::MAX), Value::Float(usize::MAX as f64));
        assert_eq!(Value::from(Some("a")), Value::String("a".into()));
        assert_eq!(Value::from(None::<i64>), Value::Nil);
        assert_eq!(Value::Float(2.0).as_i64(), Some(2));
        assert_eq!(Value::Float(2.5).as_i64(), None);
        assert_eq!(Value::Float(f64::INFINITY).as_i64(), None);
        assert!(Value::Int(0).truthy());
        assert!(!Value::Bool(false).truthy());
        assert_eq!(Value::Float(1.0).type_name(), "number");
    }

    #[test]
    fn scalars_round_trip_through_raw() {
        for v in [
            Value::Nil,
            Value::Bool(true),
            Value::Int(-7),
            Value::Float(0.5),
            Value::String("héllo".into()),
        ] {
            let back = unsafe { Value::from_raw(&v.raw()) }.unwrap();
            assert_eq!(back, v);
        }
        let err = RawValue::bytes(sys::TAG_ERROR, b"boom");
        assert_eq!(unsafe { Value::from_raw(&err) }.unwrap_err(), "boom");
    }
}
