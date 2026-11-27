//! Runtime values and tables.
//!
//! [`Value`] uses 64-bit Negative-QNaN NaN-boxing (`size_of::<Value>() == 8`,
//! half the size of Lua 5.4's 16-byte `TValue`):
//!
//! - Non-NaN IEEE-754 `f64` values occupy `[0x0000_0000_0000_0000, TAG_BASE)`,
//!   with NaNs canonicalized to `0x7FF8_0000_0000_0000`. Checking `is_float`
//!   is a single unsigned comparison and decoding is zero-cost.
//! - `[TAG_INT, ...]` (`0xFFF8..0xFFFF`) uses the upper 16 bits as a type tag
//!   and the lower 48 bits as inline signed integer / misc tag / 48-bit pointer:
//!   - `0xFFF8`: inline 48-bit signed integer (`[-2^47, 2^47 - 1]`)
//!   - `0xFFF9`: `Nil` (`0`), `Bool(false)` (`1`), `Bool(true)` (`2`) —
//!     making `!is_truthy()` a 2-instruction `(bits | 1) == FALSE_BITS` check.
//!   - `0xFFFA`: static [`NativeDef`] reference (never refcounted).
//!   - `0xFFFB..=0xFFFF` (`bits >= TAG_RC_START`): refcounted heap pointers
//!     (`String`, `Table`, `Closure`, `Buffer`, and `BigInt` / out-of-range
//!     64-bit `HeapInt` distinguished by bit 0 of the 8-byte aligned pointer).
//!
//! Heap ownership (`Clone`/`Drop` for `bits >= TAG_RC_START`) is isolated inside
//! this module so a tracing/arena GC can replace `Rc` without touching VM
//! callers.

use num_bigint::BigInt;
use num_traits::ToPrimitive;
use std::cell::{Cell, RefCell, UnsafeCell};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::Deref;
use std::rc::Rc;

use crate::ast::ty::TypeExpr;
use crate::bytecode::proto::Proto;
use crate::vm::buffer::VmBuffer;
use crate::vm::hash::FxHashMap;
use crate::vm::table::TableKey;

const _: () = assert!(std::mem::size_of::<usize>() == 8);
const _: () = assert!(std::mem::align_of::<i64>() >= 2);
const _: () = assert!(std::mem::align_of::<BigInt>() >= 2);

pub type NativeFn = fn(&mut crate::vm::machine::VM, &[Value]) -> Result<Vec<Value>, String>;

/// A native function together with its name. Values hold these by static
/// reference, avoiding any heap allocation or refcount traffic.
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

/// A `Value::native` for a name and function known at compile time.
#[macro_export]
macro_rules! native {
    ($name:expr, $func:expr) => {
        $crate::vm::value::Value::native($crate::native_def!($name, $func))
    };
}

/// Shared string handle. It wraps `Rc<VmString>` instead of `Rc<str>` so the
/// pointer stays thin (8 bytes). Hash, equality and borrowing all follow the
/// string contents, so `&str` lookups on maps keyed by this keep working.
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

#[repr(transparent)]
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

// ---- NaN-boxing bit constants ----

const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
pub(crate) const BIT_BEAT: u64 = 1u64 << 47;
const BEAT_PAYLOAD_MASK: u64 = 0x0000_7FFF_FFFF_FFFF;
const TAG_MASK: u64 = 0xFFFF_0000_0000_0000;
const CANONICAL_NAN: u64 = 0x7FF8_0000_0000_0000;

const TAG_BASE: u64 = 0xFFF8_0000_0000_0000;
const TAG_INT: u64 = 0xFFF8_0000_0000_0000;
const TAG_MISC: u64 = 0xFFF9_0000_0000_0000;
const TAG_NATIVE: u64 = 0xFFFA_0000_0000_0000;
pub(crate) const TAG_RC_START: u64 = 0xFFFB_0000_0000_0000;
const TAG_STRING: u64 = 0xFFFB_0000_0000_0000;
const TAG_TABLE: u64 = 0xFFFC_0000_0000_0000;
const TAG_CLOSURE: u64 = 0xFFFD_0000_0000_0000;
const TAG_BUFFER: u64 = 0xFFFE_0000_0000_0000;
const TAG_BIG: u64 = 0xFFFF_0000_0000_0000;

const TAG_INT_HI: u16 = 0xFFF8;
const TAG_MISC_HI: u16 = 0xFFF9;
const TAG_NATIVE_HI: u16 = 0xFFFA;
const TAG_STRING_HI: u16 = 0xFFFB;
const TAG_TABLE_HI: u16 = 0xFFFC;
const TAG_CLOSURE_HI: u16 = 0xFFFD;
const TAG_BUFFER_HI: u16 = 0xFFFE;
const TAG_BIG_HI: u16 = 0xFFFF;

const NIL_BITS: u64 = TAG_MISC;
const FALSE_BITS: u64 = TAG_MISC | 1;
const TRUE_BITS: u64 = TAG_MISC | 2;

/// Inclusive minimum of 48-bit inline signed integers (`-2^47`).
pub const INLINE_INT_MIN: i64 = -(1i64 << 47);
/// Inclusive maximum of 48-bit inline signed integers (`2^47 - 1`).
pub const INLINE_INT_MAX: i64 = (1i64 << 47) - 1;

#[inline(always)]
fn pack_ptr<T>(ptr: *const T, tag: u64) -> u64 {
    let addr = ptr.expose_provenance() as u64;
    assert!(
        addr <= PAYLOAD_MASK && (tag < TAG_RC_START || (addr & BIT_BEAT) == 0),
        "pointer address {addr:#x} exceeds 48-bit NaN-box payload or collides with BIT_BEAT"
    );
    tag | addr
}

#[inline(always)]
fn pack_beat_ptr<T>(ptr: *const T, tag: u64) -> u64 {
    let addr = ptr.expose_provenance() as u64;
    assert!(
        addr <= BEAT_PAYLOAD_MASK,
        "beat pointer address {addr:#x} exceeds 47-bit payload"
    );
    tag | BIT_BEAT | addr
}

#[inline(always)]
fn unpack_ptr<T>(bits: u64) -> *const T {
    std::ptr::with_exposed_provenance((bits & PAYLOAD_MASK) as usize)
}

#[inline(always)]
fn unpack_beat_ptr<T>(bits: u64) -> *mut T {
    std::ptr::with_exposed_provenance_mut((bits & BEAT_PAYLOAD_MASK) as usize)
}

#[inline(always)]
fn pack_bigint(rc: Rc<BigInt>) -> u64 {
    let ptr = Rc::into_raw(rc);
    let addr = ptr.expose_provenance() as u64;
    assert!(
        addr <= BEAT_PAYLOAD_MASK && (addr & 1) == 0,
        "BigInt pointer {addr:#x} invalid for tagged NaN-box"
    );
    TAG_BIG | addr
}

#[inline(always)]
fn pack_heap_int(i: i64) -> u64 {
    let ptr = Rc::into_raw(Rc::new(i));
    let addr = ptr.expose_provenance() as u64;
    assert!(
        addr <= BEAT_PAYLOAD_MASK && (addr & 1) == 0,
        "HeapInt pointer {addr:#x} invalid for tagged NaN-box"
    );
    TAG_BIG | addr | 1
}

/// Non-owning shared borrow of an `Rc<T>` payload packed inside a [`Value`].
///
/// Dereferences to `&T` with zero refcount operations, and can promote to an
/// owned `Rc<T>` via [`BorrowedRc::to_rc`] only when shared ownership is needed.
pub struct BorrowedRc<'a, T> {
    ptr: *const T,
    _marker: PhantomData<&'a T>,
}

impl<T> Copy for BorrowedRc<'_, T> {}
impl<T> Clone for BorrowedRc<'_, T> {
    #[inline(always)]
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, T> BorrowedRc<'a, T> {
    #[inline(always)]
    unsafe fn from_raw(ptr: *const T) -> Self {
        Self {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Raw pointer identical to `Rc::as_ptr`.
    #[inline(always)]
    pub fn as_ptr(self) -> *const T {
        self.ptr
    }

    /// Pointer equality identical to `Rc::ptr_eq`.
    #[inline(always)]
    pub fn ptr_eq(a: Self, b: Self) -> bool {
        std::ptr::eq(a.ptr, b.ptr)
    }

    /// Clones into an owned `Rc<T>`, incrementing the strong count.
    #[inline]
    pub fn to_rc(self) -> Rc<T> {
        // SAFETY: `self.ptr` came from `Rc::into_raw` and remains live for `'a`.
        unsafe {
            Rc::increment_strong_count(self.ptr);
            Rc::from_raw(self.ptr)
        }
    }
}

impl<T> Deref for BorrowedRc<'_, T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        // SAFETY: `self.ptr` stays alive for at least `'a`.
        unsafe { &*self.ptr }
    }
}

/// Non-owning shared borrow of a [`VmString`] or Beat-allocated
/// [`crate::vm::beat::BeatString`] packed inside a [`Value`].
#[derive(Copy, Clone)]
pub struct BorrowedStr<'a> {
    raw: usize,
    _marker: PhantomData<&'a VmString>,
}

impl<'a> BorrowedStr<'a> {
    #[inline(always)]
    unsafe fn from_raw(ptr: *const VmString) -> Self {
        Self {
            raw: ptr.expose_provenance(),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    unsafe fn from_beat_raw(ptr: *const crate::vm::beat::BeatString) -> Self {
        Self {
            raw: ptr.expose_provenance() | 1,
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub fn is_beat(self) -> bool {
        (self.raw & 1) != 0
    }

    #[inline(always)]
    pub fn as_old_vmstr(self) -> Option<(*const VmString, &'a VmString)> {
        if self.is_beat() {
            None
        } else {
            let ptr: *const VmString = std::ptr::with_exposed_provenance(self.raw);
            Some((ptr, unsafe { &*ptr }))
        }
    }

    #[inline(always)]
    pub fn as_ptr(self) -> *const VmString {
        assert!(!self.is_beat(), "BorrowedStr::as_ptr called on BeatString");
        std::ptr::with_exposed_provenance(self.raw)
    }

    #[inline(always)]
    pub fn vm_str(self) -> &'a VmString {
        assert!(!self.is_beat(), "BorrowedStr::vm_str called on BeatString");
        unsafe { &*std::ptr::with_exposed_provenance::<VmString>(self.raw) }
    }

    #[inline(always)]
    pub fn beat_str(self) -> &'a crate::vm::beat::BeatString {
        unsafe { &*std::ptr::with_exposed_provenance::<crate::vm::beat::BeatString>(self.raw & !1) }
    }

    #[inline(always)]
    pub fn as_str(self) -> &'a str {
        if self.is_beat() {
            self.beat_str().as_str()
        } else {
            &self.vm_str().str
        }
    }

    #[inline(always)]
    pub fn hash_u64(self) -> u64 {
        if self.is_beat() {
            self.beat_str().hash
        } else {
            self.vm_str().hash
        }
    }

    /// Promotes this borrow into an owned [`StrRef`], incrementing the refcount
    /// for `Old` strings or materializing an `Rc<VmString>` for Beat strings.
    #[inline]
    pub fn to_str_ref(self) -> StrRef {
        if self.is_beat() {
            StrRef::from(self.beat_str().as_str())
        } else {
            let ptr: *const VmString = std::ptr::with_exposed_provenance(self.raw);
            // SAFETY: `ptr` came from `Rc::into_raw` and remains live for `'a`.
            unsafe {
                Rc::increment_strong_count(ptr);
                StrRef(Rc::from_raw(ptr))
            }
        }
    }
}

impl Deref for BorrowedStr<'_> {
    type Target = str;

    #[inline(always)]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for BorrowedStr<'_> {
    #[inline(always)]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for BorrowedStr<'_> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
            || (self.hash_u64() == other.hash_u64() && self.as_str() == other.as_str())
    }
}

/// Uniform read borrow over either an Old `Rc<RefCell<VmTable>>` or a Beat
/// [`crate::vm::beat::BeatTable`].
pub enum TableBorrow<'a> {
    Old(std::cell::Ref<'a, VmTable>),
    Beat(&'a crate::vm::beat::BeatTable),
}

impl<'a> TableBorrow<'a> {
    #[inline(always)]
    pub fn array_slice(&self) -> &[Value] {
        match self {
            TableBorrow::Old(t) => &t.array,
            TableBorrow::Beat(t) => t.array_slice(),
        }
    }

    #[inline(always)]
    pub fn array_len(&self) -> usize {
        self.array_slice().len()
    }

    #[inline(always)]
    pub fn get_str(&self, key: &str) -> Value {
        match self {
            TableBorrow::Old(t) => t.get_str(key),
            TableBorrow::Beat(t) => t.get_str(key),
        }
    }

    #[inline(always)]
    pub fn raw_get(&self, key: &Value) -> Value {
        match self {
            TableBorrow::Old(t) => t.raw_get(key),
            TableBorrow::Beat(t) => t.raw_get(key),
        }
    }

    #[inline(always)]
    pub fn is_frozen(&self) -> bool {
        match self {
            TableBorrow::Old(t) => t.frozen,
            TableBorrow::Beat(t) => t.is_frozen(),
        }
    }

    #[inline(always)]
    pub fn has_co_state(&self) -> bool {
        match self {
            TableBorrow::Old(t) => t.co_state.is_some(),
            TableBorrow::Beat(t) => t.has_co_state(),
        }
    }

    #[inline(always)]
    pub fn co_state(&self) -> Option<Rc<RefCell<crate::vm::machine::CoroutineState>>> {
        match self {
            TableBorrow::Old(t) => t.co_state.clone(),
            TableBorrow::Beat(t) => t.co_state(),
        }
    }

    #[inline(always)]
    pub fn metatable(&self) -> Option<Rc<RefCell<VmTable>>> {
        match self {
            TableBorrow::Old(t) => t.metatable.clone(),
            TableBorrow::Beat(t) => t.extra_ref().and_then(|e| e.metatable.clone()),
        }
    }

    #[inline]
    pub fn metamethod(&self, name: &str) -> Option<Value> {
        match self {
            TableBorrow::Old(t) => t.metamethod(name),
            TableBorrow::Beat(t) => t.metamethod(name),
        }
    }

    #[inline]
    pub fn fields_len(&self) -> usize {
        match self {
            TableBorrow::Old(t) => t.fields.len(),
            TableBorrow::Beat(t) => match t.extra_ref() {
                Some(ext) => ext.fields.len(),
                None => t.fld_len as usize,
            },
        }
    }

    #[inline]
    pub fn general_len(&self) -> usize {
        match self {
            TableBorrow::Old(t) => t.general.as_ref().map_or(0, |g| g.len()),
            TableBorrow::Beat(t) => t
                .extra_ref()
                .and_then(|e| e.general.as_ref())
                .map_or(0, |g| g.len()),
        }
    }

    #[inline]
    pub fn fields_is_empty(&self) -> bool {
        match self {
            TableBorrow::Old(t) => t.fields.is_empty(),
            TableBorrow::Beat(t) => match t.extra_ref() {
                Some(ext) => ext.fields.is_empty(),
                None => t.fld_len == 0,
            },
        }
    }

    #[inline]
    pub fn has_general(&self) -> bool {
        match self {
            TableBorrow::Old(t) => t.general.is_some(),
            TableBorrow::Beat(t) => t.extra_ref().is_some_and(|e| e.general.is_some()),
        }
    }

    #[inline]
    pub fn sorted_keys(&self) -> Vec<Value> {
        match self {
            TableBorrow::Old(t) => crate::vm::table::sorted_keys(t),
            TableBorrow::Beat(t) => t.sorted_keys(),
        }
    }

    #[inline]
    pub fn next_entry(&self, key: &Value) -> Result<Option<(Value, Value)>, String> {
        match self {
            TableBorrow::Old(t) => crate::vm::table::next_entry(t, key),
            TableBorrow::Beat(t) => t.next_entry(key),
        }
    }

    pub fn for_each_field(&self, mut f: impl FnMut(&str, &Value)) {
        match self {
            TableBorrow::Old(t) => {
                for (k, v) in t.fields.iter() {
                    f(&k.0.str, v);
                }
            }
            TableBorrow::Beat(t) => {
                if let Some(ext) = t.extra_ref() {
                    for (k, v) in ext.fields.iter() {
                        f(&k.0.str, v);
                    }
                } else {
                    for entry in t.fields_slice() {
                        if let Some(ks) = entry.key.as_str() {
                            f(ks, &entry.val);
                        }
                    }
                }
            }
        }
    }

    pub fn for_each_general(&self, mut f: impl FnMut(&Value, &Value)) {
        match self {
            TableBorrow::Old(t) => {
                if let Some(gen_map) = &t.general {
                    for (k, v) in gen_map.values() {
                        f(k, v);
                    }
                }
            }
            TableBorrow::Beat(t) => {
                if let Some(ext) = t.extra_ref()
                    && let Some(gen_map) = &ext.general
                {
                    for (k, v) in gen_map.values() {
                        f(k, v);
                    }
                }
            }
        }
    }
}

/// Non-owning shared handle to either an Old `RefCell<VmTable>` or a Beat
/// [`crate::vm::beat::BeatTable`], without acquiring a `RefCell` borrow on view.
#[derive(Copy, Clone)]
pub struct BorrowedTable<'a> {
    raw: usize,
    _marker: PhantomData<&'a ()>,
}

impl<'a> BorrowedTable<'a> {
    #[inline(always)]
    unsafe fn from_old_raw(ptr: *const RefCell<VmTable>) -> Self {
        Self {
            raw: ptr.expose_provenance(),
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    unsafe fn from_beat_raw(ptr: *const crate::vm::beat::BeatTable) -> Self {
        Self {
            raw: ptr.expose_provenance() | 1,
            _marker: PhantomData,
        }
    }

    #[inline(always)]
    pub fn is_beat(self) -> bool {
        (self.raw & 1) != 0
    }

    #[inline(always)]
    pub fn as_ptr_id(self) -> usize {
        self.raw & !1
    }

    #[inline(always)]
    pub fn ptr_eq(a: Self, b: Self) -> bool {
        a.raw == b.raw
    }

    #[inline(always)]
    pub fn as_old(self) -> Option<BorrowedRc<'a, RefCell<VmTable>>> {
        if self.is_beat() {
            None
        } else {
            let ptr: *const RefCell<VmTable> = std::ptr::with_exposed_provenance(self.raw);
            Some(unsafe { BorrowedRc::from_raw(ptr) })
        }
    }

    #[inline(always)]
    pub fn as_beat(self) -> Option<&'a crate::vm::beat::BeatTable> {
        if self.is_beat() {
            let ptr: *const crate::vm::beat::BeatTable =
                std::ptr::with_exposed_provenance(self.raw & !1);
            Some(unsafe { &*ptr })
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn borrow(self) -> TableBorrow<'a> {
        if let Some(bt) = self.as_beat() {
            TableBorrow::Beat(bt)
        } else {
            let ptr: *const RefCell<VmTable> = std::ptr::with_exposed_provenance(self.raw);
            TableBorrow::Old(unsafe { &*ptr }.borrow())
        }
    }

    #[inline(always)]
    pub fn try_borrow(self) -> Result<TableBorrow<'a>, std::cell::BorrowError> {
        if let Some(bt) = self.as_beat() {
            Ok(TableBorrow::Beat(bt))
        } else {
            let ptr: *const RefCell<VmTable> = std::ptr::with_exposed_provenance(self.raw);
            unsafe { &*ptr }.try_borrow().map(TableBorrow::Old)
        }
    }
}

impl Eq for BorrowedStr<'_> {}

impl fmt::Display for BorrowedStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for BorrowedStr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.as_str())
    }
}

/// Borrowed discriminated view of a [`Value`] for clean pattern matching.
pub enum ValueView<'a> {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Native(&'static NativeDef),
    BigInt(&'a BigInt),
    String(BorrowedStr<'a>),
    Table(BorrowedTable<'a>),
    Closure(BorrowedRc<'a, VmClosure>),
    Buffer(BorrowedRc<'a, RefCell<VmBuffer>>),
}

/// Owned discriminated representation extracted without extra refcount bumps.
pub enum OwnedValue {
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

/// A compact 8-byte runtime value encoded via 64-bit NaN-boxing.
///
/// Integers are arbitrary precision as far as programs can tell: values within
/// `[-2^47, 2^47 - 1]` live completely inline inside the 8-byte word, values
/// inside `i64` outside 48-bit box as `Rc<i64>`, and larger integers normalize to
/// `Rc<BigInt>` (which never holds values in `i64` range).
#[repr(transparent)]
pub struct Value(u64);

#[cold]
#[inline(never)]
unsafe fn clone_rc_slow(bits: u64) {
    let tag_hi = (bits >> 48) as u16;
    unsafe {
        match tag_hi {
            TAG_STRING_HI => Rc::increment_strong_count(unpack_ptr::<VmString>(bits)),
            TAG_TABLE_HI => Rc::increment_strong_count(unpack_ptr::<RefCell<VmTable>>(bits)),
            TAG_CLOSURE_HI => Rc::increment_strong_count(unpack_ptr::<VmClosure>(bits)),
            TAG_BUFFER_HI => Rc::increment_strong_count(unpack_ptr::<RefCell<VmBuffer>>(bits)),
            TAG_BIG_HI => {
                if (bits & 1) == 0 {
                    Rc::increment_strong_count(unpack_ptr::<BigInt>(bits));
                } else {
                    Rc::increment_strong_count(unpack_ptr::<i64>(bits & !1));
                }
            }
            _ => std::hint::unreachable_unchecked(),
        }
    }
}

#[cold]
#[inline(never)]
unsafe fn drop_rc_slow(bits: u64) {
    let tag_hi = (bits >> 48) as u16;
    unsafe {
        match tag_hi {
            TAG_STRING_HI => drop(Rc::from_raw(unpack_ptr::<VmString>(bits))),
            TAG_TABLE_HI => drop(Rc::from_raw(unpack_ptr::<RefCell<VmTable>>(bits))),
            TAG_CLOSURE_HI => drop(Rc::from_raw(unpack_ptr::<VmClosure>(bits))),
            TAG_BUFFER_HI => drop(Rc::from_raw(unpack_ptr::<RefCell<VmBuffer>>(bits))),
            TAG_BIG_HI => {
                if (bits & 1) == 0 {
                    drop(Rc::from_raw(unpack_ptr::<BigInt>(bits)));
                } else {
                    drop(Rc::from_raw(unpack_ptr::<i64>(bits & !1)));
                }
            }
            _ => std::hint::unreachable_unchecked(),
        }
    }
}

impl Clone for Value {
    #[inline(always)]
    fn clone(&self) -> Self {
        if self.0 >= TAG_RC_START && (self.0 & BIT_BEAT) == 0 {
            // SAFETY: `self.0 >= TAG_RC_START && (self.0 & BIT_BEAT) == 0`
            // guarantees a valid Old `Rc::into_raw` payload.
            unsafe { clone_rc_slow(self.0) };
        }
        Value(self.0)
    }
}

impl Drop for Value {
    #[inline(always)]
    fn drop(&mut self) {
        if self.0 >= TAG_RC_START && (self.0 & BIT_BEAT) == 0 {
            // SAFETY: `self.0 >= TAG_RC_START && (self.0 & BIT_BEAT) == 0`
            // guarantees this Value owns one strong Old `Rc` ref.
            unsafe { drop_rc_slow(self.0) };
        }
    }
}

impl Value {
    pub const NIL: Value = Value(NIL_BITS);
    pub const FALSE: Value = Value(FALSE_BITS);
    pub const TRUE: Value = Value(TRUE_BITS);

    #[inline(always)]
    pub const fn nil() -> Value {
        Value(NIL_BITS)
    }

    #[inline(always)]
    pub const fn bool(b: bool) -> Value {
        Value(if b { TRUE_BITS } else { FALSE_BITS })
    }

    /// Encodes a 32-bit integer directly inline without heap fallback.
    #[inline(always)]
    pub const fn small_int(i: i32) -> Value {
        Value(TAG_INT | ((i as i64 as u64) & PAYLOAD_MASK))
    }

    /// Encodes an `i64`, storing 48-bit signed values inline and boxing larger
    /// 64-bit values only when outside `[-2^47, 2^47 - 1]`.
    #[inline(always)]
    pub fn int(i: i64) -> Value {
        if (INLINE_INT_MIN..=INLINE_INT_MAX).contains(&i) {
            Value(TAG_INT | ((i as u64) & PAYLOAD_MASK))
        } else {
            Value(pack_heap_int(i))
        }
    }

    #[inline(always)]
    pub const fn float(f: f64) -> Value {
        if f.is_nan() {
            Value(CANONICAL_NAN)
        } else {
            Value(f.to_bits())
        }
    }

    #[inline(always)]
    pub fn native(def: &'static NativeDef) -> Value {
        Value(pack_ptr(def as *const NativeDef, TAG_NATIVE))
    }

    #[inline(always)]
    pub fn str_ref(s: StrRef) -> Value {
        Value(pack_ptr(Rc::into_raw(s.0), TAG_STRING))
    }

    #[inline(always)]
    pub fn beat_string(ptr: *const crate::vm::beat::BeatString) -> Value {
        Value(pack_beat_ptr(ptr, TAG_STRING))
    }

    #[inline(always)]
    pub fn table(t: Rc<RefCell<VmTable>>) -> Value {
        Value(pack_ptr(Rc::into_raw(t), TAG_TABLE))
    }

    #[inline(always)]
    pub fn beat_table(ptr: *const crate::vm::beat::BeatTable) -> Value {
        Value(pack_beat_ptr(ptr, TAG_TABLE))
    }

    #[inline(always)]
    pub const fn is_beat(&self) -> bool {
        self.0 >= TAG_RC_START && (self.0 & BIT_BEAT) != 0
    }

    #[inline(always)]
    pub const fn is_beat_string(&self) -> bool {
        (self.0 & (TAG_MASK | BIT_BEAT)) == (TAG_STRING | BIT_BEAT)
    }

    #[inline(always)]
    pub const fn is_beat_table(&self) -> bool {
        (self.0 & (TAG_MASK | BIT_BEAT)) == (TAG_TABLE | BIT_BEAT)
    }

    #[inline(always)]
    pub const fn is_old_table(&self) -> bool {
        (self.0 & (TAG_MASK | BIT_BEAT)) == TAG_TABLE
    }

    #[inline(always)]
    pub fn closure(c: Rc<VmClosure>) -> Value {
        Value(pack_ptr(Rc::into_raw(c), TAG_CLOSURE))
    }

    #[inline(always)]
    pub fn buffer(b: Rc<RefCell<VmBuffer>>) -> Value {
        Value(pack_ptr(Rc::into_raw(b), TAG_BUFFER))
    }

    /// Raw NaN-boxed 64-bit representation.
    #[inline(always)]
    pub const fn bits(&self) -> u64 {
        self.0
    }

    /// Constructs a `Value` from a trivial (non-RC) raw bit pattern.
    ///
    /// # Safety
    /// Caller must ensure `bits < TAG_RC_START` and represents a valid encoded scalar.
    #[inline(always)]
    pub(crate) const unsafe fn from_trivial_bits(bits: u64) -> Value {
        debug_assert!(bits < TAG_RC_START);
        Value(bits)
    }

    /// Reconstructs a cloned owned `Value` from a stored raw NaN-boxed bit word
    /// (incrementing strong count if it represents an Old `Rc`-backed object).
    ///
    /// # Safety
    /// Caller must ensure `bits` represents a live valid `Value` encoding.
    #[inline(always)]
    pub(crate) unsafe fn clone_from_bits(bits: u64) -> Value {
        if bits >= TAG_RC_START && (bits & BIT_BEAT) == 0 {
            unsafe { clone_rc_slow(bits) };
        }
        Value(bits)
    }

    /// Returns the raw Beat allocation cursor for a Beat-resident value (`BeatTable`
    /// or `BeatString`), even after forwarding, or `None` for scalars and Old values.
    #[inline(always)]
    pub fn beat_alloc_mark(&self) -> Option<crate::vm::beat::BeatMark> {
        if let Some(bt) = self.raw_beat_table_ptr() {
            Some(unsafe { (*bt).alloc_mark })
        } else {
            self.raw_beat_str_ptr()
                .map(|bs| unsafe { (*bs).alloc_mark })
        }
    }

    #[inline(always)]
    pub(crate) fn raw_beat_str_ptr(&self) -> Option<*mut crate::vm::beat::BeatString> {
        if self.is_beat_string() {
            Some(unpack_beat_ptr::<crate::vm::beat::BeatString>(self.0))
        } else {
            None
        }
    }

    #[inline(always)]
    pub(crate) fn raw_beat_table_ptr(&self) -> Option<*mut crate::vm::beat::BeatTable> {
        if self.is_beat_table() {
            Some(unpack_beat_ptr::<crate::vm::beat::BeatTable>(self.0))
        } else {
            None
        }
    }

    /// Overwrites `*self` with `val`, bypassing `Drop` when both old and new are trivial scalars.
    #[inline(always)]
    pub fn set_inplace(&mut self, val: Value) {
        if self.0 < TAG_RC_START && val.0 < TAG_RC_START {
            self.0 = val.0;
            std::mem::forget(val);
        } else {
            *self = val;
        }
    }

    /// Overwrites `*self` from `&Value`, bypassing both `Drop` and `Clone` for scalars
    /// and identical `Rc` handles.
    #[inline(always)]
    pub fn assign_ref(&mut self, val: &Value) {
        if self.0 < TAG_RC_START && val.0 < TAG_RC_START {
            self.0 = val.0;
        } else if self.0 != val.0 {
            *self = val.clone();
        }
    }

    #[inline(always)]
    pub const fn is_trivial(&self) -> bool {
        self.0 < TAG_RC_START
    }

    #[inline(always)]
    pub const fn is_nil(&self) -> bool {
        self.0 == NIL_BITS
    }

    #[inline(always)]
    pub const fn is_truthy(&self) -> bool {
        (self.0 | 1) != FALSE_BITS
    }

    #[inline(always)]
    pub const fn is_bool(&self) -> bool {
        self.0 == FALSE_BITS || self.0 == TRUE_BITS
    }

    #[inline(always)]
    pub const fn as_bool(&self) -> Option<bool> {
        if self.0 == TRUE_BITS {
            Some(true)
        } else if self.0 == FALSE_BITS {
            Some(false)
        } else {
            None
        }
    }

    #[inline(always)]
    pub const fn is_float(&self) -> bool {
        self.0 < TAG_BASE
    }

    #[inline(always)]
    pub const fn as_float(&self) -> Option<f64> {
        if self.is_float() {
            Some(f64::from_bits(self.0))
        } else {
            None
        }
    }

    /// Unchecked float extraction when `self.is_float()` is already known.
    #[inline(always)]
    pub const fn as_float_unchecked(&self) -> f64 {
        debug_assert!(self.is_float());
        f64::from_bits(self.0)
    }

    #[inline(always)]
    pub const fn is_inline_int(&self) -> bool {
        (self.0 & TAG_MASK) == TAG_INT
    }

    /// Unchecked inline 48-bit integer extraction when `self.is_inline_int()` is already known.
    #[inline(always)]
    pub const fn as_inline_int_unchecked(&self) -> i64 {
        debug_assert!(self.is_inline_int());
        ((self.0 as i64) << 16) >> 16
    }

    #[inline(always)]
    pub fn is_int(&self) -> bool {
        self.is_inline_int() || (self.0 & (TAG_MASK | 1)) == (TAG_BIG | 1)
    }

    #[inline(always)]
    pub fn as_int(&self) -> Option<i64> {
        if self.is_inline_int() {
            Some(self.as_inline_int_unchecked())
        } else if (self.0 & (TAG_MASK | 1)) == (TAG_BIG | 1) {
            // SAFETY: `TAG_BIG | 1` stores an exposed `Rc::into_raw` for `i64`.
            Some(unsafe { *unpack_ptr::<i64>(self.0 & !1) })
        } else {
            None
        }
    }

    #[inline(always)]
    pub const fn is_bigint(&self) -> bool {
        (self.0 & (TAG_MASK | 1)) == TAG_BIG
    }

    #[inline(always)]
    pub fn as_bigint(&self) -> Option<&BigInt> {
        if self.is_bigint() {
            // SAFETY: `TAG_BIG` with bit 0 == 0 stores an exposed `Rc::into_raw` for `BigInt`.
            Some(unsafe { &*unpack_ptr::<BigInt>(self.0) })
        } else {
            None
        }
    }

    #[inline]
    pub fn to_bigint_rc(&self) -> Option<Rc<BigInt>> {
        if self.is_bigint() {
            let ptr = unpack_ptr::<BigInt>(self.0);
            unsafe {
                Rc::increment_strong_count(ptr);
                Some(Rc::from_raw(ptr))
            }
        } else {
            None
        }
    }

    /// Mutates a uniquely-owned `BigInt` (`strong_count == 1`) in place and demotes
    /// it back to an inline/boxed `i64` if the result shrunk into `i64` range.
    #[inline]
    pub fn with_unique_bigint_mut(&mut self, f: impl FnOnce(&mut BigInt)) -> bool {
        if !self.is_bigint() {
            return false;
        }
        let ptr = unpack_ptr::<BigInt>(self.0);
        // Quick read check before temporarily taking ownership out of `self.0`.
        let prev_bits = std::mem::replace(&mut self.0, NIL_BITS);
        let mut rc = unsafe { Rc::from_raw(ptr) };
        if let Some(big) = Rc::get_mut(&mut rc) {
            f(big);
            *self = Value::from_bigint_rc(rc);
            true
        } else {
            std::mem::forget(rc);
            self.0 = prev_bits;
            false
        }
    }

    /// Mutates a uniquely-owned boxed `i64` (`strong_count == 1`) in place when
    /// `next` stays outside inline range, or demotes back to inline `Int`.
    #[inline(always)]
    pub(crate) fn try_set_unique_heap_int(&mut self, next: i64) -> bool {
        if (self.0 & (TAG_MASK | 1)) != (TAG_BIG | 1) {
            return false;
        }
        let ptr = std::ptr::with_exposed_provenance::<i64>((self.0 & (PAYLOAD_MASK & !1)) as usize);
        let prev_bits = std::mem::replace(&mut self.0, NIL_BITS);
        let mut rc = unsafe { Rc::from_raw(ptr) };
        if let Some(slot) = Rc::get_mut(&mut rc) {
            if (INLINE_INT_MIN..=INLINE_INT_MAX).contains(&next) {
                drop(rc);
                self.0 = TAG_INT | ((next as u64) & PAYLOAD_MASK);
            } else {
                *slot = next;
                let _ = Rc::into_raw(rc);
                self.0 = prev_bits;
            }
            true
        } else {
            std::mem::forget(rc);
            self.0 = prev_bits;
            false
        }
    }

    #[inline(always)]
    pub fn is_number(&self) -> bool {
        self.is_float() || self.is_inline_int() || (self.0 & TAG_MASK) == TAG_BIG
    }

    #[inline(always)]
    pub const fn is_native(&self) -> bool {
        (self.0 & TAG_MASK) == TAG_NATIVE
    }

    #[inline(always)]
    pub fn as_native(&self) -> Option<&'static NativeDef> {
        if self.is_native() {
            // SAFETY: `TAG_NATIVE` stores `&'static NativeDef`.
            Some(unsafe { &*unpack_ptr::<NativeDef>(self.0) })
        } else {
            None
        }
    }

    #[inline(always)]
    pub const fn is_string(&self) -> bool {
        (self.0 & TAG_MASK) == TAG_STRING
    }

    #[inline(always)]
    pub fn as_str_ref(&self) -> Option<BorrowedStr<'_>> {
        if self.is_string() {
            if (self.0 & BIT_BEAT) != 0 {
                let ptr = unpack_beat_ptr::<crate::vm::beat::BeatString>(self.0);
                let fwd = unsafe { (*ptr).forwarded };
                if fwd != 0 {
                    Some(unsafe { BorrowedStr::from_raw(unpack_ptr::<VmString>(fwd)) })
                } else {
                    Some(unsafe { BorrowedStr::from_beat_raw(ptr) })
                }
            } else {
                Some(unsafe { BorrowedStr::from_raw(unpack_ptr::<VmString>(self.0)) })
            }
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_str(&self) -> Option<&str> {
        self.as_str_ref().map(|s| s.as_str())
    }

    #[inline]
    pub fn to_str_ref(&self) -> Option<StrRef> {
        self.as_str_ref().map(|s| s.to_str_ref())
    }

    #[inline(always)]
    pub fn beat_str_ptr(&self) -> Option<*mut crate::vm::beat::BeatString> {
        if self.is_beat_string() {
            let ptr = unpack_beat_ptr::<crate::vm::beat::BeatString>(self.0);
            if unsafe { (*ptr).forwarded } == 0 {
                Some(ptr)
            } else {
                None
            }
        } else {
            None
        }
    }

    #[inline(always)]
    pub const fn is_table(&self) -> bool {
        (self.0 & TAG_MASK) == TAG_TABLE
    }

    #[inline(always)]
    pub fn as_table(&self) -> Option<BorrowedRc<'_, RefCell<VmTable>>> {
        if self.is_old_table() {
            Some(unsafe { BorrowedRc::from_raw(unpack_ptr::<RefCell<VmTable>>(self.0)) })
        } else if self.is_beat_table() {
            let bt = unsafe { &*unpack_beat_ptr::<crate::vm::beat::BeatTable>(self.0) };
            if bt.forwarded != 0 {
                Some(unsafe { BorrowedRc::from_raw(unpack_ptr::<RefCell<VmTable>>(bt.forwarded)) })
            } else {
                None
            }
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn table_ptr(&self) -> Option<*const RefCell<VmTable>> {
        if self.is_old_table() {
            Some(unpack_ptr::<RefCell<VmTable>>(self.0))
        } else if self.is_beat_table() {
            let bt = unsafe { &*unpack_beat_ptr::<crate::vm::beat::BeatTable>(self.0) };
            if bt.forwarded != 0 {
                Some(unpack_ptr::<RefCell<VmTable>>(bt.forwarded))
            } else {
                None
            }
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn beat_table_ptr(&self) -> Option<*mut crate::vm::beat::BeatTable> {
        if self.is_beat_table() {
            let ptr = unpack_beat_ptr::<crate::vm::beat::BeatTable>(self.0);
            if unsafe { (*ptr).forwarded } == 0 {
                Some(ptr)
            } else {
                None
            }
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn table_borrow(&self) -> Option<TableBorrow<'_>> {
        if self.is_old_table() {
            let tbl = unsafe { &*unpack_ptr::<RefCell<VmTable>>(self.0) };
            Some(TableBorrow::Old(tbl.borrow()))
        } else if self.is_beat_table() {
            let tbl = unsafe { &*unpack_beat_ptr::<crate::vm::beat::BeatTable>(self.0) };
            if tbl.forwarded != 0 {
                let old_tbl = unsafe { &*unpack_ptr::<RefCell<VmTable>>(tbl.forwarded) };
                Some(TableBorrow::Old(old_tbl.borrow()))
            } else {
                Some(TableBorrow::Beat(tbl))
            }
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn try_table_borrow(&self) -> Option<Result<TableBorrow<'_>, std::cell::BorrowError>> {
        if self.is_old_table() {
            let tbl = unsafe { &*unpack_ptr::<RefCell<VmTable>>(self.0) };
            Some(tbl.try_borrow().map(TableBorrow::Old))
        } else if self.is_beat_table() {
            let tbl = unsafe { &*unpack_beat_ptr::<crate::vm::beat::BeatTable>(self.0) };
            if tbl.forwarded != 0 {
                let old_tbl = unsafe { &*unpack_ptr::<RefCell<VmTable>>(tbl.forwarded) };
                Some(old_tbl.try_borrow().map(TableBorrow::Old))
            } else {
                Some(Ok(TableBorrow::Beat(tbl)))
            }
        } else {
            None
        }
    }

    #[inline]
    pub fn to_table(&self) -> Option<Rc<RefCell<VmTable>>> {
        self.as_table().map(|t| t.to_rc())
    }

    #[inline(always)]
    pub const fn is_closure(&self) -> bool {
        (self.0 & TAG_MASK) == TAG_CLOSURE
    }

    #[inline(always)]
    pub fn as_closure(&self) -> Option<BorrowedRc<'_, VmClosure>> {
        if self.is_closure() {
            Some(unsafe { BorrowedRc::from_raw(unpack_ptr::<VmClosure>(self.0)) })
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn closure_ptr(&self) -> Option<*const VmClosure> {
        if self.is_closure() {
            Some(unpack_ptr::<VmClosure>(self.0))
        } else {
            None
        }
    }

    #[inline]
    pub fn to_closure(&self) -> Option<Rc<VmClosure>> {
        self.as_closure().map(|c| c.to_rc())
    }

    #[inline(always)]
    pub const fn is_buffer(&self) -> bool {
        (self.0 & TAG_MASK) == TAG_BUFFER
    }

    #[inline(always)]
    pub fn as_buffer(&self) -> Option<BorrowedRc<'_, RefCell<VmBuffer>>> {
        if self.is_buffer() {
            Some(unsafe { BorrowedRc::from_raw(unpack_ptr::<RefCell<VmBuffer>>(self.0)) })
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn buffer_ptr(&self) -> Option<*const RefCell<VmBuffer>> {
        if self.is_buffer() {
            Some(unpack_ptr::<RefCell<VmBuffer>>(self.0))
        } else {
            None
        }
    }

    #[inline]
    pub fn to_buffer(&self) -> Option<Rc<RefCell<VmBuffer>>> {
        self.as_buffer().map(|b| b.to_rc())
    }

    #[inline(always)]
    pub const fn is_callable(&self) -> bool {
        let hi = (self.0 >> 48) as u16;
        hi == TAG_NATIVE_HI || hi == TAG_CLOSURE_HI
    }

    /// Borrows this value as a pattern-matchable [`ValueView`] with zero refcount overhead.
    #[inline(always)]
    pub fn view(&self) -> ValueView<'_> {
        let bits = self.0;
        if bits < TAG_BASE {
            return ValueView::Float(f64::from_bits(bits));
        }
        match (bits >> 48) as u16 {
            TAG_INT_HI => ValueView::Int(((bits as i64) << 16) >> 16),
            TAG_MISC_HI => match bits {
                NIL_BITS => ValueView::Nil,
                FALSE_BITS => ValueView::Bool(false),
                _ => ValueView::Bool(true),
            },
            TAG_NATIVE_HI => ValueView::Native(unsafe { &*unpack_ptr::<NativeDef>(bits) }),
            TAG_STRING_HI => {
                if (bits & BIT_BEAT) != 0 {
                    let ptr = unpack_beat_ptr::<crate::vm::beat::BeatString>(bits);
                    let fwd = unsafe { (*ptr).forwarded };
                    if fwd != 0 {
                        ValueView::String(unsafe {
                            BorrowedStr::from_raw(unpack_ptr::<VmString>(fwd))
                        })
                    } else {
                        ValueView::String(unsafe { BorrowedStr::from_beat_raw(ptr) })
                    }
                } else {
                    ValueView::String(unsafe {
                        BorrowedStr::from_raw(unpack_ptr::<VmString>(bits))
                    })
                }
            }
            TAG_TABLE_HI => {
                if (bits & BIT_BEAT) != 0 {
                    let ptr = unpack_beat_ptr::<crate::vm::beat::BeatTable>(bits);
                    let fwd = unsafe { (*ptr).forwarded };
                    if fwd != 0 {
                        ValueView::Table(unsafe {
                            BorrowedTable::from_old_raw(unpack_ptr::<RefCell<VmTable>>(fwd))
                        })
                    } else {
                        ValueView::Table(unsafe { BorrowedTable::from_beat_raw(ptr) })
                    }
                } else {
                    ValueView::Table(unsafe {
                        BorrowedTable::from_old_raw(unpack_ptr::<RefCell<VmTable>>(bits))
                    })
                }
            }
            TAG_CLOSURE_HI => {
                ValueView::Closure(unsafe { BorrowedRc::from_raw(unpack_ptr::<VmClosure>(bits)) })
            }
            TAG_BUFFER_HI => ValueView::Buffer(unsafe {
                BorrowedRc::from_raw(unpack_ptr::<RefCell<VmBuffer>>(bits))
            }),
            _ => {
                if (bits & 1) == 0 {
                    ValueView::BigInt(unsafe { &*unpack_ptr::<BigInt>(bits) })
                } else {
                    ValueView::Int(unsafe { *unpack_ptr::<i64>(bits & !1) })
                }
            }
        }
    }

    /// Consumes `self`, moving out any `Rc` handle without changing refcounts.
    #[inline]
    pub fn into_view(self) -> OwnedValue {
        let bits = self.0;
        if bits < TAG_BASE {
            return OwnedValue::Float(f64::from_bits(bits));
        }
        std::mem::forget(self);
        unsafe {
            match (bits >> 48) as u16 {
                TAG_INT_HI => OwnedValue::Int(((bits as i64) << 16) >> 16),
                TAG_MISC_HI => match bits {
                    NIL_BITS => OwnedValue::Nil,
                    FALSE_BITS => OwnedValue::Bool(false),
                    _ => OwnedValue::Bool(true),
                },
                TAG_NATIVE_HI => OwnedValue::Native(&*unpack_ptr::<NativeDef>(bits)),
                TAG_STRING_HI => {
                    if (bits & BIT_BEAT) != 0 {
                        let bs = &*unpack_beat_ptr::<crate::vm::beat::BeatString>(bits);
                        OwnedValue::String(StrRef::from(bs.as_str()))
                    } else {
                        OwnedValue::String(StrRef(Rc::from_raw(unpack_ptr(bits))))
                    }
                }
                TAG_TABLE_HI => {
                    if (bits & BIT_BEAT) != 0 {
                        let bt = &*unpack_beat_ptr::<crate::vm::beat::BeatTable>(bits);
                        OwnedValue::Table(Rc::new(RefCell::new(bt.to_vm_table_cloned())))
                    } else {
                        OwnedValue::Table(Rc::from_raw(unpack_ptr(bits)))
                    }
                }
                TAG_CLOSURE_HI => OwnedValue::Closure(Rc::from_raw(unpack_ptr(bits))),
                TAG_BUFFER_HI => OwnedValue::Buffer(Rc::from_raw(unpack_ptr(bits))),
                _ => {
                    if (bits & 1) == 0 {
                        OwnedValue::BigInt(Rc::from_raw(unpack_ptr(bits)))
                    } else {
                        let rc = Rc::from_raw(unpack_ptr::<i64>(bits & !1));
                        OwnedValue::Int(*rc)
                    }
                }
            }
        }
    }

    #[inline]
    pub fn into_table(self) -> Result<Rc<RefCell<VmTable>>, Value> {
        if self.is_old_table() {
            let bits = self.0;
            std::mem::forget(self);
            Ok(unsafe { Rc::from_raw(unpack_ptr(bits)) })
        } else if self.is_beat_table() {
            let bt = unsafe { &*unpack_beat_ptr::<crate::vm::beat::BeatTable>(self.0) };
            let fwd = bt.forwarded;
            if fwd != 0 {
                std::mem::forget(self);
                Ok(unsafe { Value::clone_from_bits(fwd).into_table().unwrap() })
            } else {
                let vm_tbl = bt.to_vm_table_cloned();
                std::mem::forget(self);
                Ok(Rc::new(RefCell::new(vm_tbl)))
            }
        } else {
            Err(self)
        }
    }

    #[inline]
    pub fn into_str_ref(self) -> Result<StrRef, Value> {
        if self.is_string() {
            if (self.0 & BIT_BEAT) != 0 {
                let s = unsafe { &*unpack_beat_ptr::<crate::vm::beat::BeatString>(self.0) };
                if s.forwarded != 0 {
                    let fwd = s.forwarded;
                    std::mem::forget(self);
                    unsafe { Value::clone_from_bits(fwd) }.into_str_ref()
                } else {
                    let owned = StrRef::from(s.as_str());
                    std::mem::forget(self);
                    Ok(owned)
                }
            } else {
                let bits = self.0;
                std::mem::forget(self);
                Ok(StrRef(unsafe { Rc::from_raw(unpack_ptr(bits)) }))
            }
        } else {
            Err(self)
        }
    }

    #[inline]
    pub fn into_closure(self) -> Result<Rc<VmClosure>, Value> {
        if self.is_closure() {
            let bits = self.0;
            std::mem::forget(self);
            Ok(unsafe { Rc::from_raw(unpack_ptr(bits)) })
        } else {
            Err(self)
        }
    }

    /// Builds an integer value, storing it inline/i64 when it fits `i64`.
    #[inline]
    pub fn from_bigint(i: BigInt) -> Value {
        match i.to_i64() {
            Some(small) => Value::int(small),
            None => Value(pack_bigint(Rc::new(i))),
        }
    }

    #[inline]
    pub fn from_bigint_rc(i: Rc<BigInt>) -> Value {
        match i.to_i64() {
            Some(small) => Value::int(small),
            None => Value(pack_bigint(i)),
        }
    }

    /// Builds an integer value from `i128`, storing it inline when it fits.
    #[inline]
    pub fn from_i128(i: i128) -> Value {
        if let Ok(small) = i64::try_from(i) {
            Value::int(small)
        } else {
            Value(pack_bigint(Rc::new(BigInt::from(i))))
        }
    }

    #[inline]
    pub fn from_u128(i: u128) -> Value {
        if let Ok(small) = i64::try_from(i) {
            Value::int(small)
        } else {
            Value(pack_bigint(Rc::new(BigInt::from(i))))
        }
    }

    #[inline]
    pub fn from_usize(i: usize) -> Value {
        match i64::try_from(i) {
            Ok(small) => Value::int(small),
            Err(_) => Value(pack_bigint(Rc::new(BigInt::from(i)))),
        }
    }

    #[inline]
    pub fn from_u64(i: u64) -> Value {
        match i64::try_from(i) {
            Ok(small) => Value::int(small),
            Err(_) => Value(pack_bigint(Rc::new(BigInt::from(i)))),
        }
    }

    #[inline]
    pub fn str(s: &str) -> Value {
        Value::str_ref(StrRef::from(s))
    }

    #[inline]
    pub fn string(s: String) -> Value {
        Value::str_ref(StrRef::from(s))
    }

    /// The integer as a `usize`, if it is a non-negative integer that fits.
    #[inline]
    pub fn as_usize(&self) -> Option<usize> {
        self.as_int().and_then(|i| usize::try_from(i).ok())
    }

    pub fn type_name(&self) -> &'static str {
        match self.view() {
            ValueView::Nil => "nil",
            ValueView::Bool(_) => "boolean",
            ValueView::Int(_) | ValueView::BigInt(_) | ValueView::Float(_) => "number",
            ValueView::String(_) => "string",
            ValueView::Table(t) if t.try_borrow().is_ok_and(|t| t.has_co_state()) => "thread",
            ValueView::Table(_) => "table",
            ValueView::Closure(_) | ValueView::Native(..) => "function",
            ValueView::Buffer(_) => "buffer",
        }
    }

    pub fn typeof_name(&self) -> &'static str {
        match self.view() {
            ValueView::Nil => "nil",
            ValueView::Bool(_) => "boolean",
            ValueView::Int(_) | ValueView::BigInt(_) => "bigint",
            ValueView::Float(_) => "float",
            ValueView::String(_) => "string",
            ValueView::Table(t) if t.try_borrow().is_ok_and(|t| t.has_co_state()) => "thread",
            ValueView::Table(_) => "table",
            ValueView::Closure(_) | ValueView::Native(..) => "function",
            ValueView::Buffer(_) => "buffer",
        }
    }

    /// Checks if this runtime value satisfies a given type expression.
    pub fn matches_type_expr(&self, ty: &TypeExpr) -> bool {
        match ty {
            TypeExpr::Any => true,
            TypeExpr::Never => false,
            TypeExpr::Nil => self.is_nil(),
            TypeExpr::Named(name) => match name.to_lowercase().as_str() {
                "any" => true,
                "nil" => self.is_nil(),
                "never" | "void" => false,
                "bool" | "boolean" => self.is_bool(),
                "int" | "integer" | "i32" | "i64" | "u32" | "u64" => {
                    self.is_int() || self.is_bigint()
                }
                "float" | "f32" | "f64" => self.is_float(),
                "num" | "number" => self.is_number(),
                "str" | "string" => self.is_string(),
                "table" | "dict" => self.is_table(),
                "array" | "list" => self.is_table(),
                "buf" | "buffer" => self.is_buffer(),
                "thread" | "coroutine" => self
                    .try_table_borrow()
                    .is_some_and(|t| t.is_ok_and(|t| t.has_co_state())),
                "function" => self.is_callable(),
                other_name => {
                    if let Some(Ok(tbl)) = self.try_table_borrow() {
                        if let Some(e) = tbl.get_str("__enum").as_str()
                            && e.eq_ignore_ascii_case(other_name)
                        {
                            return true;
                        }
                        if let Some(tp) = tbl.get_str("__type").as_str()
                            && tp.eq_ignore_ascii_case(other_name)
                        {
                            return true;
                        }
                        if let Some(meta) = tbl.metatable()
                            && let Ok(mt) = meta.try_borrow()
                            && let Some(n) = mt.get_str("__name").as_str()
                            && n.eq_ignore_ascii_case(other_name)
                        {
                            return true;
                        }
                    }
                    false
                }
            },
            TypeExpr::Optional(inner) => self.is_nil() || self.matches_type_expr(inner),
            TypeExpr::Union(variants) => variants.iter().any(|v| self.matches_type_expr(v)),
            TypeExpr::Array(elem_ty) => {
                if let Some(t) = self.try_table_borrow() {
                    if let Ok(tbl) = t {
                        tbl.array_slice()
                            .iter()
                            .all(|elem| elem.matches_type_expr(elem_ty))
                    } else {
                        true
                    }
                } else {
                    false
                }
            }
            TypeExpr::Record(fields) => {
                if let Some(t) = self.try_table_borrow() {
                    if let Ok(tbl) = t {
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
            TypeExpr::Tuple(_) => self.is_table(),
            TypeExpr::Table { .. } => self.is_table(),
            TypeExpr::Function { .. } => self.is_callable(),
        }
    }

    #[cold]
    #[inline(never)]
    fn eq_slow(&self, other: &Self) -> bool {
        if self.is_table() || other.is_table() {
            if let (Some(a), Some(b)) = (self.table_ptr(), other.table_ptr()) {
                return std::ptr::eq(a, b);
            }
            return false;
        }
        match (self.view(), other.view()) {
            (ValueView::Float(a), ValueView::Float(b)) => a == b,
            (ValueView::Int(a), ValueView::Int(b)) => a == b,
            (ValueView::BigInt(a), ValueView::BigInt(b)) => a == b,
            (ValueView::Int(_), ValueView::BigInt(_))
            | (ValueView::BigInt(_), ValueView::Int(_)) => false,
            (ValueView::Int(a), ValueView::Float(b)) => (a as f64) == b,
            (ValueView::Float(a), ValueView::Int(b)) => a == (b as f64),
            (ValueView::BigInt(a), ValueView::Float(b)) => a.to_f64().is_some_and(|v| v == b),
            (ValueView::Float(a), ValueView::BigInt(b)) => b.to_f64().is_some_and(|v| a == v),
            (ValueView::String(a), ValueView::String(b)) => a == b,
            (ValueView::Native(a), ValueView::Native(b)) => std::ptr::eq(a, b) || a.name == b.name,
            _ => false,
        }
    }
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
        self.fields.get(key).cloned().unwrap_or(Value::NIL)
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
        if let Some(f) = key.as_float()
            && f.fract() == 0.0
            && f.abs() < i64::MAX as f64
        {
            return self.raw_get(&Value::int(f as i64));
        }
        match key.view() {
            ValueView::String(k) => {
                if let Some((k_ptr, k_vms)) = k.as_old_vmstr() {
                    self.fields
                        .get_by_vmstr(k_ptr, k_vms)
                        .cloned()
                        .unwrap_or(Value::NIL)
                } else {
                    self.fields.get(k.as_str()).cloned().unwrap_or(Value::NIL)
                }
            }
            ValueView::Int(i) if i >= 1 && (i as u64) <= self.array.len() as u64 => {
                self.array[i as usize - 1].clone()
            }
            _ if self.general.is_none() => Value::NIL,
            _ => TableKey::from_value(key)
                .ok()
                .and_then(|k| self.get_general(&k).cloned())
                .unwrap_or(Value::NIL),
        }
    }

    /// Writes a key without metamethods or the frozen check. Assigning nil
    /// removes the entry; trailing nils are trimmed off the array part so
    /// `#t` shrinks with `t[#t] = nil`.
    pub fn raw_set(&mut self, key: Value, val: Value) -> Result<(), String> {
        if let Some(s) = key.as_str_ref() {
            if val.is_nil() {
                if let Some((s_ptr, s_vms)) = s.as_old_vmstr() {
                    self.fields.remove_by_vmstr(s_ptr, s_vms);
                } else {
                    self.fields.remove(s.as_str());
                }
            } else {
                let owned = key.into_str_ref().unwrap_or_else(|_| unreachable!());
                self.fields.insert(owned, val);
            }
            return Ok(());
        }
        if let Some(i) = key.as_int()
            && i >= 1
        {
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
                if val.is_nil() {
                    self.remove_general(&TableKey::Int(i));
                } else {
                    self.array.push(val);
                    self.absorb_general();
                }
                return Ok(());
            }
        }
        if let Some(f) = key.as_float()
            && f.fract() == 0.0
            && f >= 1.0
            && f < i64::MAX as f64
        {
            return self.raw_set(Value::int(f as i64), val);
        }
        let tk = TableKey::from_value(&key).map_err(|e| e.to_string())?;
        if val.is_nil() {
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
        while self.array.last().is_some_and(|v| v.is_nil()) {
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
            Some(v) if !v.is_nil() => Some(v.clone()),
            _ => None,
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
            closed: UnsafeCell::new(Value::NIL),
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
        // only after `&mut` is released via `set_inplace`.
        unsafe { &mut *self.closed.get() }.set_inplace(value);
    }

    /// Overwrites the closed value from a reference, skipping `Value::clone` on primitives.
    #[inline(always)]
    pub fn set_closed_ref(&self, value: &Value) {
        // SAFETY: VM execution is single-threaded.
        unsafe { &mut *self.closed.get() }.assign_ref(value);
    }

    /// Moves the closed value out, leaving `Value::NIL` behind.
    #[inline]
    pub fn take_closed(&self) -> Value {
        // SAFETY: VM execution is single-threaded and `mem::replace` does not alias.
        std::mem::replace(unsafe { &mut *self.closed.get() }, Value::NIL)
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

impl From<i64> for Value {
    #[inline]
    fn from(i: i64) -> Value {
        Value::int(i)
    }
}

impl From<BigInt> for Value {
    fn from(i: BigInt) -> Value {
        Value::from_bigint(i)
    }
}

impl From<String> for Value {
    fn from(s: String) -> Value {
        Value::str_ref(StrRef::from(s))
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::str_ref(StrRef::from(s))
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.view() {
            ValueView::Nil => write!(f, "nil"),
            ValueView::Bool(b) => write!(f, "{}", b),
            ValueView::Int(i) => write!(f, "{}", i),
            ValueView::BigInt(i) => write!(f, "{}", i),
            ValueView::Float(n) => write!(f, "{}", n),
            ValueView::String(s) => write!(f, "{}", s),
            ValueView::Table(_) => write!(f, "table"),
            ValueView::Closure(c) => write!(
                f,
                "function({})",
                c.proto.name.as_deref().unwrap_or("anonymous")
            ),
            ValueView::Native(def) => write!(f, "native function({})", def.name),
            ValueView::Buffer(b) => write!(f, "buffer({})", b.borrow().len()),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self)
    }
}

impl PartialEq for Value {
    #[inline(always)]
    fn eq(&self, other: &Self) -> bool {
        if self.0 == other.0 {
            return self.0 != CANONICAL_NAN;
        }
        self.eq_slow(other)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::size_of;

    /// Hot-loop values are packed into a single 64-bit word (half of Lua 5.4's
    /// 16-byte `TValue`) so register traffic and table arrays stay dense.
    #[test]
    fn value_is_one_word() {
        assert_eq!(size_of::<Value>(), 8);
    }

    #[test]
    fn nan_box_primitives_and_boundaries() {
        assert!(Value::NIL.is_nil());
        assert!(!Value::NIL.is_truthy());
        assert!(!Value::bool(false).is_truthy());
        assert!(Value::bool(true).is_truthy());
        assert_eq!(Value::bool(false).as_bool(), Some(false));
        assert_eq!(Value::bool(true).as_bool(), Some(true));
        assert!(Value::int(0).is_truthy());
        assert!(Value::float(0.0).is_truthy());

        for &n in &[
            0i64,
            1,
            -1,
            INLINE_INT_MIN,
            INLINE_INT_MAX,
            INLINE_INT_MIN - 1,
            INLINE_INT_MAX + 1,
            i64::MIN,
            i64::MAX,
        ] {
            let v = Value::int(n);
            assert_eq!(v.as_int(), Some(n));
            assert!(v.is_int());
            assert!(v.is_number());
            assert_eq!(v, Value::int(n));
        }

        let pos_zero = Value::float(0.0);
        let neg_zero = Value::float(-0.0);
        assert_eq!(pos_zero, neg_zero);
        assert_eq!(pos_zero, Value::int(0));

        let nan = Value::float(f64::NAN);
        assert!(nan.is_float());
        assert!(nan.as_float().unwrap().is_nan());
        assert_ne!(nan, nan);

        for &f in &[f64::INFINITY, f64::NEG_INFINITY, -1.5, 42.25] {
            let v = Value::float(f);
            assert_eq!(v.as_float(), Some(f));
        }
    }

    #[test]
    fn nan_box_rc_refcounts_cleanly() {
        let tbl = Rc::new(RefCell::new(VmTable::new()));
        let s = StrRef::from("hello");
        assert_eq!(Rc::strong_count(&tbl), 1);
        assert_eq!(Rc::strong_count(&s.0), 1);

        let vt = Value::table(Rc::clone(&tbl));
        let vs = Value::str_ref(s.clone());
        assert_eq!(Rc::strong_count(&tbl), 2);
        assert_eq!(Rc::strong_count(&s.0), 2);

        let vt2 = vt.clone();
        let vs2 = vs.clone();
        assert_eq!(Rc::strong_count(&tbl), 3);
        assert_eq!(Rc::strong_count(&s.0), 3);
        assert_eq!(vt, vt2);
        assert_eq!(vs, vs2);
        assert_eq!(vs.as_str(), Some("hello"));

        drop(vt);
        drop(vt2);
        drop(vs);
        drop(vs2);
        assert_eq!(Rc::strong_count(&tbl), 1);
        assert_eq!(Rc::strong_count(&s.0), 1);
    }

    #[test]
    fn beat_value_tags_skip_rc_and_borrow_uniformly() {
        let mut beat = crate::vm::beat::BeatArena::new();
        let bs = beat.alloc_string("beat_key");
        let bt = beat.alloc_table(2, 2);
        let vs_beat = Value::beat_string(bs);
        let vt_beat = Value::beat_table(bt);

        assert!(vs_beat.is_beat());
        assert!(vs_beat.is_beat_string());
        assert!(vs_beat.is_string());
        assert!(!vs_beat.is_table());
        assert_eq!(vs_beat.as_str(), Some("beat_key"));

        assert!(vt_beat.is_beat());
        assert!(vt_beat.is_beat_table());
        assert!(vt_beat.is_table());
        assert!(!vt_beat.is_old_table());
        assert!(vt_beat.as_table().is_none());
        assert!(vt_beat.table_ptr().is_none());
        assert_eq!(vt_beat.beat_table_ptr(), Some(bt));
        assert_eq!(vt_beat.type_name(), "table");
        assert_eq!(vt_beat.typeof_name(), "table");
        assert_eq!(format!("{vt_beat}"), "table");

        // Compare BeatString equality against an Old VmString with identical content.
        let vs_old = Value::str("beat_key");
        assert_eq!(vs_beat, vs_old);
        assert_eq!(vs_old, vs_beat);

        // Clone and drop of Beat values must not touch any Rc or mutate arena state.
        let vt_clone = vt_beat.clone();
        let vs_clone = vs_beat.clone();
        assert_eq!(vt_clone, vt_beat);
        assert_eq!(vs_clone, vs_beat);
        assert_ne!(vt_beat, Value::NIL);

        unsafe {
            beat.table_raw_set(bt, vs_beat.clone(), Value::int(77))
                .unwrap();
            beat.table_raw_set(bt, Value::int(1), Value::int(11))
                .unwrap();
        }
        {
            let tb = vt_beat.table_borrow().unwrap();
            assert_eq!(tb.array_len(), 1);
            assert_eq!(tb.get_str("beat_key"), Value::int(77));
            assert_eq!(tb.raw_get(&vs_old), Value::int(77));
            assert_eq!(tb.raw_get(&Value::int(1)), Value::int(11));
        }

        // Convert BeatTable into Old via into_table without mutating the Beat slot.
        let owned_rc = vt_clone.into_table().unwrap();
        assert_eq!(owned_rc.borrow().get_str("beat_key"), Value::int(77));
        assert_eq!(owned_rc.borrow().array, vec![Value::int(11)]);
    }
}
