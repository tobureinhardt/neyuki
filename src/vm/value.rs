//! Runtime values and tables.

use num_bigint::BigInt;
use num_traits::ToPrimitive;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::rc::Rc;

use crate::ast::ty::TypeExpr;
use crate::bytecode::proto::Proto;
use crate::vm::buffer::VmBuffer;
use crate::vm::hash::FxHashMap;
use crate::vm::table::TableKey;

pub type NativeFn = fn(&mut crate::vm::machine::VM, &[Value]) -> Result<Vec<Value>, String>;

/// A native function together with its name. Values hold these by static
/// reference, which keeps `Value` at three words.
pub struct NativeDef {
    pub name: &'static str,
    pub func: NativeFn,
}

/// A `&'static NativeDef` for a name and function known at compile time.
#[macro_export]
macro_rules! native_def {
    ($name:expr, $func:expr) => {{
        const DEF: $crate::vm::value::NativeDef = $crate::vm::value::NativeDef {
            name: $name,
            func: $func,
        };
        &DEF
    }};
}

/// A `Value::Native` for a name and function known at compile time.
#[macro_export]
macro_rules! native {
    ($name:expr, $func:expr) => {
        $crate::vm::value::Value::Native($crate::native_def!($name, $func))
    };
}

/// Shared string handle. It wraps `Rc<VmString>` instead of `Rc<str>` so the
/// pointer stays thin (8 bytes); a fat `Rc<str>` would push every `Value`
/// past two words. Hash, equality and borrowing all follow the string
/// contents, so `&str` lookups on maps keyed by this keep working.
#[derive(Clone, Debug)]
pub struct VmString {
    pub str: String,
    /// Fast hash computed with `crate::vm::hash::fx_hash_bytes(str.as_bytes())`
    /// for `FieldMap` index lookups.
    ///
    /// NOTE: This is distinct from `<str as std::hash::Hash>::hash` from the
    /// standard library (which appends a `0xff` delimiter byte). The two hash
    /// domains are separate and must never be mixed.
    pub hash: u64,
}

#[derive(Clone, Debug)]
pub struct StrRef(pub Rc<VmString>);

impl PartialEq for StrRef {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0) || (self.0.hash == other.0.hash && self.0.str == other.0.str)
    }
}

impl Eq for StrRef {}

impl PartialOrd for StrRef {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StrRef {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.str.cmp(&other.0.str)
    }
}

impl Hash for StrRef {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.str.hash(state);
    }
}

impl std::borrow::Borrow<str> for StrRef {
    #[inline]
    fn borrow(&self) -> &str {
        &self.0.str
    }
}

impl Deref for StrRef {
    type Target = str;

    #[inline]
    fn deref(&self) -> &str {
        &self.0.str
    }
}

impl AsRef<str> for StrRef {
    #[inline]
    fn as_ref(&self) -> &str {
        &self.0.str
    }
}

impl fmt::Display for StrRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.str)
    }
}

impl From<&str> for StrRef {
    #[inline]
    fn from(s: &str) -> Self {
        let hash = crate::vm::hash::fx_hash_bytes(s.as_bytes());
        StrRef(Rc::new(VmString {
            str: s.to_owned(),
            hash,
        }))
    }
}

impl From<String> for StrRef {
    #[inline]
    fn from(s: String) -> Self {
        let hash = crate::vm::hash::fx_hash_bytes(s.as_bytes());
        StrRef(Rc::new(VmString { str: s, hash }))
    }
}

/// A runtime value. Kept small and cheap to clone: every heap-backed variant
/// is behind an `Rc`, so copying a register never copies a string or a table.
///
/// Integers are arbitrary precision as far as programs can tell, but almost
/// all of them fit an `i64`. `Int` is the fast path; arithmetic that
/// overflows promotes to `BigInt`, and every constructor normalises so that a
/// `BigInt` is never inside the `i64` range. That invariant is what lets
/// `Int == BigInt` be answered without looking at the digits.
#[derive(Clone)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Native(&'static NativeDef),
    BigInt(Rc<BigInt>),
    String(StrRef),
    Table(Rc<RefCell<VmTable>>),
    Closure(Rc<VmClosure>),
    Buffer(Rc<RefCell<VmBuffer>>),
}

#[derive(Clone)]
pub struct VmTable {
    pub array: Vec<Value>,
    pub fields: crate::vm::table::FieldMap,
    /// Every other key: integers outside the array part, floats, booleans,
    /// tables, functions and buffers. Each entry keeps its key value, so
    /// iteration can hand it back and a table key stays alive.
    pub general: Option<Box<FxHashMap<TableKey, (Value, Value)>>>,
    /// The array's length before trailing nils were last trimmed off, so
    /// `next` can tell a key removed from the end of the array from a
    /// removed hash key. Zero when nothing was trimmed.
    pub array_peak: usize,
    pub frozen: bool,
    pub metatable: Option<Rc<RefCell<VmTable>>>,
    /// Strong owner of a coroutine's execution state when this table is a
    /// coroutine handle. The VM's state map keeps only a weak reference, so
    /// dropping the last handle frees a suspended coroutine immediately by
    /// plain refcounting. Cloning a handle shares the state.
    pub co_state: Option<Rc<RefCell<crate::vm::machine::CoroutineState>>>,
}

impl Default for VmTable {
    fn default() -> Self {
        Self::new()
    }
}

impl VmTable {
    pub fn new() -> Self {
        Self {
            array: Vec::new(),
            fields: crate::vm::table::FieldMap::new(),
            general: None,
            array_peak: 0,
            frozen: false,
            metatable: None,
            co_state: None,
        }
    }

    pub fn with_capacity(array: usize, fields: usize) -> Self {
        Self {
            array: if array == 0 {
                Vec::new()
            } else {
                Vec::with_capacity(array)
            },
            fields: crate::vm::table::FieldMap::with_capacity(fields),
            general: None,
            array_peak: 0,
            frozen: false,
            metatable: None,
            co_state: None,
        }
    }

    pub fn get_str(&self, key: &str) -> Value {
        self.fields.get(key).cloned().unwrap_or(Value::Nil)
    }

    pub fn set_str(&mut self, key: &str, val: Value) {
        if !self.frozen {
            self.fields.insert(StrRef::from(key), val);
        }
    }

    pub fn get_general(&self, key: &TableKey) -> Option<&Value> {
        self.general
            .as_ref()
            .and_then(|g| g.get(key))
            .map(|(_, v)| v)
    }

    /// Reads a key without metamethods. Nil and NaN keys read as nil.
    pub fn raw_get(&self, key: &Value) -> Value {
        if let Value::Float(f) = key
            && f.fract() == 0.0
            && f.abs() < i64::MAX as f64
        {
            return self.raw_get(&Value::Int(*f as i64));
        }
        match key {
            Value::String(k) => self.fields.get_by_strref(k).cloned().unwrap_or(Value::Nil),
            Value::Int(i) if *i >= 1 && (*i as u64) <= self.array.len() as u64 => {
                self.array[*i as usize - 1].clone()
            }
            _ if self.general.is_none() => Value::Nil,
            other => TableKey::from_value(other)
                .ok()
                .and_then(|k| self.get_general(&k).cloned())
                .unwrap_or(Value::Nil),
        }
    }

    /// Writes a key without metamethods or the frozen check. Assigning nil
    /// removes the entry; trailing nils are trimmed off the array part so
    /// `#t` shrinks with `t[#t] = nil`.
    pub fn raw_set(&mut self, key: Value, val: Value) -> Result<(), String> {
        match key {
            Value::String(k) => {
                if matches!(val, Value::Nil) {
                    self.fields.remove_by_strref(&k);
                } else {
                    self.fields.insert(k, val);
                }
                return Ok(());
            }
            Value::Int(i) if i >= 1 => {
                let len = self.array.len() as u64;
                let idx = i as u64;
                if idx <= len {
                    self.array[i as usize - 1] = val;
                    if idx == len {
                        self.trim_array();
                    }
                    return Ok(());
                }
                if idx == len + 1 {
                    if matches!(val, Value::Nil) {
                        self.remove_general(&TableKey::Int(i));
                    } else {
                        self.array.push(val);
                        self.absorb_general();
                    }
                    return Ok(());
                }
            }
            Value::Float(f) if f.fract() == 0.0 && f >= 1.0 && f < i64::MAX as f64 => {
                return self.raw_set(Value::Int(f as i64), val);
            }
            _ => {}
        }
        let tk = TableKey::from_value(&key).map_err(|e| e.to_string())?;
        if matches!(val, Value::Nil) {
            self.remove_general(&tk);
        } else {
            // A new key inside the trimmed range would read as an array slot.
            if matches!(tk, TableKey::Int(i) if i >= 1 && i as u64 <= self.array_peak as u64) {
                self.array_peak = 0;
            }
            self.general
                .get_or_insert_with(|| Box::new(crate::vm::hash::new_map()))
                .insert(tk, (key, val));
        }
        Ok(())
    }

    fn remove_general(&mut self, key: &TableKey) {
        if let Some(general) = &mut self.general {
            general.remove(key);
            if general.is_empty() {
                self.general = None;
            }
        }
    }

    fn trim_array(&mut self) {
        self.array_peak = self.array_peak.max(self.array.len());
        while matches!(self.array.last(), Some(Value::Nil)) {
            self.array.pop();
        }
    }

    /// Moves integer keys that now continue the array part into it.
    pub fn absorb_general(&mut self) {
        let Some(general) = &mut self.general else {
            return;
        };
        loop {
            let next = TableKey::Int(self.array.len() as i64 + 1);
            match general.remove(&next) {
                Some((_, v)) => self.array.push(v),
                None => break,
            }
        }
        if general.is_empty() {
            self.general = None;
        }
    }

    /// Looks a metamethod up on this table's metatable, if it has one.
    pub fn metamethod(&self, event: &str) -> Option<Value> {
        let mt = self.metatable.as_ref()?;
        match mt.borrow().fields.get(event) {
            Some(Value::Nil) | None => None,
            Some(h) => Some(h.clone()),
        }
    }
}

/// A captured local. While the local's frame is live the upvalue is *open*:
/// the register at `location` on the current stack holds the value. When the
/// frame returns (or its stack is parked for a coroutine switch) the value
/// moves into `closed`. Keeping the register authoritative while open is what
/// lets ordinary register access skip any upvalue bookkeeping.
pub struct Upvalue {
    /// The stack index while open, `None` once closed.
    pub location: Cell<Option<usize>>,
    closed: UnsafeCell<Value>,
}

impl Upvalue {
    pub fn open(index: usize) -> Rc<Upvalue> {
        Rc::new(Upvalue {
            location: Cell::new(Some(index)),
            closed: UnsafeCell::new(Value::Nil),
        })
    }

    pub fn closed(value: Value) -> Rc<Upvalue> {
        Rc::new(Upvalue {
            location: Cell::new(None),
            closed: UnsafeCell::new(value),
        })
    }

    #[inline]
    pub fn is_open(&self) -> bool {
        self.location.get().is_some()
    }

    /// Reads the closed value without re-entering user code.
    #[inline(always)]
    pub fn get_closed(&self) -> Value {
        // SAFETY: VM execution is single-threaded (`!Sync`) and cloning a
        // `Value` never mutates or re-borrows `self.closed`.
        unsafe { &*self.closed.get() }.clone()
    }

    /// Borrows the closed value briefly for non-reentrant inspection.
    #[inline(always)]
    pub fn with_closed<R>(&self, f: impl FnOnce(&Value) -> R) -> R {
        // SAFETY: `f` must not mutate this `Upvalue`; callers only inspect GC edges.
        f(unsafe { &*self.closed.get() })
    }

    #[cfg(not(debug_assertions))]
    #[inline(always)]
    pub(crate) fn closed_ptr(&self) -> *mut Value {
        self.closed.get()
    }

    /// Overwrites the closed value, updating primitives in place to skip drops.
    #[inline(always)]
    pub fn set_closed(&self, value: Value) {
        // SAFETY: VM execution is single-threaded and old value drop happens
        // only after `&mut` is released via `mem::replace`.
        let slot = unsafe { &mut *self.closed.get() };
        match (value, &mut *slot) {
            (Value::Int(new), Value::Int(old)) => *old = new,
            (Value::Float(new), Value::Float(old)) => *old = new,
            (Value::Bool(new), Value::Bool(old)) => *old = new,
            (v, s) => {
                let _old = std::mem::replace(s, v);
            }
        }
    }

    /// Overwrites the closed value from a reference, skipping `Value::clone` on primitives.
    #[inline(always)]
    pub fn set_closed_ref(&self, value: &Value) {
        // SAFETY: VM execution is single-threaded and old value drop happens
        // only after `&mut` is released via `mem::replace`.
        let slot = unsafe { &mut *self.closed.get() };
        match (value, &mut *slot) {
            (Value::Int(new), Value::Int(old)) => *old = *new,
            (Value::Float(new), Value::Float(old)) => *old = *new,
            (Value::Bool(new), Value::Bool(old)) => *old = *new,
            (v, s) => {
                let _old = std::mem::replace(s, v.clone());
            }
        }
    }

    /// Moves the closed value out, leaving `Value::Nil` behind.
    #[inline]
    pub fn take_closed(&self) -> Value {
        // SAFETY: VM execution is single-threaded and `mem::replace` does not alias.
        std::mem::replace(unsafe { &mut *self.closed.get() }, Value::Nil)
    }

    pub fn close(&self, value: Value) {
        self.set_closed(value);
        self.location.set(None);
    }
}

pub struct VmClosure {
    pub proto: Rc<Proto>,
    pub upvalues: Vec<Rc<Upvalue>>,
}

impl Value {
    #[inline]
    pub fn is_truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    /// Builds an integer value, storing it inline when it fits an `i64`.
    #[inline]
    pub fn from_bigint(i: BigInt) -> Value {
        match i.to_i64() {
            Some(small) => Value::Int(small),
            None => Value::BigInt(Rc::new(i)),
        }
    }

    /// Builds an integer value from `i128`, storing it inline when it fits an `i64`.
    #[inline]
    pub fn from_i128(i: i128) -> Value {
        if let Ok(small) = i64::try_from(i) {
            Value::Int(small)
        } else {
            Value::BigInt(Rc::new(BigInt::from(i)))
        }
    }

    #[inline]
    pub fn from_u128(i: u128) -> Value {
        if let Ok(small) = i64::try_from(i) {
            Value::Int(small)
        } else {
            Value::BigInt(Rc::new(BigInt::from(i)))
        }
    }

    #[inline]
    pub fn from_usize(i: usize) -> Value {
        match i64::try_from(i) {
            Ok(small) => Value::Int(small),
            Err(_) => Value::BigInt(Rc::new(BigInt::from(i))),
        }
    }

    #[inline]
    pub fn from_u64(i: u64) -> Value {
        match i64::try_from(i) {
            Ok(small) => Value::Int(small),
            Err(_) => Value::BigInt(Rc::new(BigInt::from(i))),
        }
    }

    #[inline]
    pub fn str(s: &str) -> Value {
        Value::String(StrRef::from(s))
    }

    #[inline]
    pub fn string(s: String) -> Value {
        Value::String(StrRef::from(s))
    }

    #[inline]
    pub fn is_number(&self) -> bool {
        matches!(self, Value::Int(_) | Value::BigInt(_) | Value::Float(_))
    }

    /// The integer as a `usize`, if it is a non-negative integer that fits.
    #[inline]
    pub fn as_usize(&self) -> Option<usize> {
        match self {
            Value::Int(i) => usize::try_from(*i).ok(),
            _ => None,
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) | Value::BigInt(_) | Value::Float(_) => "number",
            Value::String(_) => "string",
            Value::Table(t) if t.try_borrow().is_ok_and(|t| t.co_state.is_some()) => "thread",
            Value::Table(_) => "table",
            Value::Closure(_) | Value::Native(..) => "function",
            Value::Buffer(_) => "buffer",
        }
    }

    pub fn typeof_name(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) | Value::BigInt(_) => "bigint",
            Value::Float(_) => "float",
            Value::String(_) => "string",
            Value::Table(t) if t.try_borrow().is_ok_and(|t| t.co_state.is_some()) => "thread",
            Value::Table(_) => "table",
            Value::Closure(_) | Value::Native(..) => "function",
            Value::Buffer(_) => "buffer",
        }
    }

    /// Checks if this runtime value satisfies a given type expression.
    pub fn matches_type_expr(&self, ty: &TypeExpr) -> bool {
        match ty {
            TypeExpr::Any => true,
            TypeExpr::Never => false,
            TypeExpr::Nil => matches!(self, Value::Nil),
            TypeExpr::Named(name) => match name.to_lowercase().as_str() {
                "any" => true,
                "nil" => matches!(self, Value::Nil),
                "never" | "void" => false,
                "bool" | "boolean" => matches!(self, Value::Bool(_)),
                "int" | "integer" | "i32" | "i64" | "u32" | "u64" => {
                    matches!(self, Value::Int(_) | Value::BigInt(_))
                }
                "float" | "f32" | "f64" => matches!(self, Value::Float(_)),
                "num" | "number" => self.is_number(),
                "str" | "string" => matches!(self, Value::String(_)),
                "table" | "dict" => matches!(self, Value::Table(_)),
                "array" | "list" => matches!(self, Value::Table(_)),
                "buf" | "buffer" => matches!(self, Value::Buffer(_)),
                "thread" | "coroutine" => {
                    matches!(self, Value::Table(t) if t.try_borrow().is_ok_and(|t| t.co_state.is_some()))
                }
                "function" => matches!(self, Value::Closure(_) | Value::Native(..)),
                other_name => {
                    if let Value::Table(t) = self
                        && let Ok(tbl) = t.try_borrow()
                    {
                        if let Value::String(e) = tbl.get_str("__enum")
                            && e.eq_ignore_ascii_case(other_name)
                        {
                            return true;
                        }
                        if let Value::String(tp) = tbl.get_str("__type")
                            && tp.eq_ignore_ascii_case(other_name)
                        {
                            return true;
                        }
                        if let Some(meta) = &tbl.metatable
                            && let Ok(mt) = meta.try_borrow()
                            && let Value::String(n) = mt.get_str("__name")
                            && n.eq_ignore_ascii_case(other_name)
                        {
                            return true;
                        }
                    }
                    false
                }
            },
            TypeExpr::Optional(inner) => {
                matches!(self, Value::Nil) || self.matches_type_expr(inner)
            }
            TypeExpr::Union(variants) => variants.iter().any(|v| self.matches_type_expr(v)),
            TypeExpr::Array(elem_ty) => {
                if let Value::Table(t) = self {
                    if let Ok(tbl) = t.try_borrow() {
                        tbl.array.iter().all(|elem| elem.matches_type_expr(elem_ty))
                    } else {
                        true
                    }
                } else {
                    false
                }
            }
            TypeExpr::Record(fields) => {
                if let Value::Table(t) = self {
                    if let Ok(tbl) = t.try_borrow() {
                        for (fname, fty) in fields {
                            let fval = tbl.get_str(fname);
                            if !fval.matches_type_expr(fty) {
                                return false;
                            }
                        }
                        true
                    } else {
                        true
                    }
                } else {
                    false
                }
            }
            TypeExpr::Tuple(_) => matches!(self, Value::Table(_)),
            TypeExpr::Table { .. } => matches!(self, Value::Table(_)),
            TypeExpr::Function { .. } => matches!(self, Value::Closure(_) | Value::Native(..)),
        }
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Value {
        Value::Int(i)
    }
}

impl From<BigInt> for Value {
    fn from(i: BigInt) -> Value {
        Value::from_bigint(i)
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::String(StrRef::from(s))
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::String(StrRef::from(s))
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Nil => write!(f, "nil"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Int(i) => write!(f, "{}", i),
            Value::BigInt(i) => write!(f, "{}", i),
            Value::Float(n) => write!(f, "{}", n),
            Value::String(s) => write!(f, "{}", s),
            Value::Table(_) => write!(f, "table"),
            Value::Closure(c) => write!(
                f,
                "function({})",
                c.proto.name.as_deref().unwrap_or("anonymous")
            ),
            Value::Native(def) => write!(f, "native function({})", def.name),
            Value::Buffer(b) => write!(f, "buffer({})", b.borrow().len()),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self)
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Nil, Value::Nil) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::BigInt(a), Value::BigInt(b)) => Rc::ptr_eq(a, b) || a == b,
            // A normalised BigInt is outside the i64 range, so it can never
            // equal an Int.
            (Value::Int(_), Value::BigInt(_)) | (Value::BigInt(_), Value::Int(_)) => false,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Int(a), Value::Float(b)) => (*a as f64) == *b,
            (Value::Float(a), Value::Int(b)) => *a == (*b as f64),
            (Value::BigInt(a), Value::Float(b)) => a.to_f64().is_some_and(|v| v == *b),
            (Value::Float(a), Value::BigInt(b)) => b.to_f64().is_some_and(|v| *a == v),
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Table(a), Value::Table(b)) => Rc::ptr_eq(a, b),
            (Value::Closure(a), Value::Closure(b)) => Rc::ptr_eq(a, b),
            (Value::Native(a), Value::Native(b)) => std::ptr::eq(*a, *b) || a.name == b.name,
            (Value::Buffer(a), Value::Buffer(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    /// Hot-loop values are copied on every move: the enum must stay at two
    /// words so register traffic stays cheap.
    #[test]
    fn value_stays_two_words() {
        assert_eq!(size_of::<Value>(), 16);
    }
}
