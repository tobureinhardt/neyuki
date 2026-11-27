//! Hashable table keys. Any value except nil and NaN can be a key.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::vm::buffer::VmBuffer;
use crate::vm::value::{NativeDef, StrRef, Value, ValueView, VmClosure, VmString, VmTable};

#[derive(Clone, Debug)]
pub enum TableKey {
    Bool(bool),
    Int(i64),
    Float(u64), // normalized f64 bits
    String(StrRef),
    Table(*const RefCell<VmTable>),
    BeatTable(*const crate::vm::beat::BeatTable),
    Closure(*const VmClosure),
    Native(*const NativeDef),
    Buffer(*const RefCell<VmBuffer>),
    Look(crate::vm::old::SlotId),
}

impl PartialEq for TableKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (TableKey::Bool(a), TableKey::Bool(b)) => a == b,
            (TableKey::Int(a), TableKey::Int(b)) => a == b,
            (TableKey::Float(a), TableKey::Float(b)) => a == b,
            (TableKey::String(a), TableKey::String(b)) => a == b,
            (TableKey::Table(a), TableKey::Table(b)) => std::ptr::eq(*a, *b),
            (TableKey::BeatTable(a), TableKey::BeatTable(b)) => std::ptr::eq(*a, *b),
            (TableKey::Closure(a), TableKey::Closure(b)) => std::ptr::eq(*a, *b),
            (TableKey::Native(a), TableKey::Native(b)) => std::ptr::eq(*a, *b),
            (TableKey::Buffer(a), TableKey::Buffer(b)) => std::ptr::eq(*a, *b),
            (TableKey::Look(a), TableKey::Look(b)) => a == b,
            _ => false,
        }
    }
}

impl Eq for TableKey {}

impl Hash for TableKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            TableKey::Bool(b) => b.hash(state),
            TableKey::Int(i) => i.hash(state),
            TableKey::Float(bits) => bits.hash(state),
            TableKey::String(s) => s.hash(state),
            TableKey::Table(p) => p.hash(state),
            TableKey::BeatTable(p) => p.hash(state),
            TableKey::Closure(p) => p.hash(state),
            TableKey::Native(p) => p.hash(state),
            TableKey::Buffer(p) => p.hash(state),
            TableKey::Look(id) => id.hash(state),
        }
    }
}

impl TableKey {
    pub fn from_value(val: &Value) -> Result<Self, &'static str> {
        match val.view() {
            ValueView::Nil => Err("table index is nil"),
            ValueView::Bool(b) => Ok(TableKey::Bool(b)),
            ValueView::Int(i) => Ok(TableKey::Int(i)),
            ValueView::Float(f) => {
                if f.is_nan() {
                    return Err("table index is NaN");
                }
                // If float is an exact integer, represent as Int to unify lookup
                if f.fract() == 0.0 && f >= (i64::MIN as f64) && f <= (i64::MAX as f64) {
                    Ok(TableKey::Int(f as i64))
                } else {
                    let bits = if f == 0.0 { 0u64 } else { f.to_bits() };
                    Ok(TableKey::Float(bits))
                }
            }
            ValueView::BigInt(b) => {
                if let Some(i) = num_traits::ToPrimitive::to_i64(b) {
                    Ok(TableKey::Int(i))
                } else {
                    Err("table index too large")
                }
            }
            ValueView::String(s) => Ok(TableKey::String(s.to_str_ref())),
            ValueView::Table(t) => {
                if let Some(o) = t.as_old() {
                    Ok(TableKey::Table(o.as_ptr()))
                } else {
                    Ok(TableKey::BeatTable(
                        t.as_beat().expect("Beat table must have Beat pointer") as *const _,
                    ))
                }
            }
            ValueView::Closure(c) => Ok(TableKey::Closure(c.as_ptr())),
            ValueView::Native(def) => Ok(TableKey::Native(def as *const _)),
            ValueView::Buffer(b) => Ok(TableKey::Buffer(b.as_ptr())),
            ValueView::Look(id) => Ok(TableKey::Look(id)),
        }
    }
}

/// Where a non-array key sorts: numbers first (ascending), then strings,
/// booleans, and everything else by identity.
fn key_rank(key: &Value) -> u8 {
    match key.view() {
        ValueView::Int(_) | ValueView::BigInt(_) | ValueView::Float(_) => 0,
        ValueView::String(_) => 1,
        ValueView::Bool(_) => 2,
        _ => 3,
    }
}

fn identity(key: &Value) -> usize {
    match key.view() {
        ValueView::Table(t) => t.as_ptr_id(),
        ValueView::Closure(c) => c.as_ptr() as *const u8 as usize,
        ValueView::Native(d) => d as *const NativeDef as usize,
        ValueView::Buffer(b) => b.as_ptr() as *const u8 as usize,
        _ => 0,
    }
}

pub(crate) fn compare_keys(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    key_rank(a)
        .cmp(&key_rank(b))
        .then_with(|| match (a.view(), b.view()) {
            (ValueView::String(x), ValueView::String(y)) => x.as_str().cmp(y.as_str()),
            (ValueView::Bool(x), ValueView::Bool(y)) => x.cmp(&y),
            _ if key_rank(a) == 0 => {
                if crate::vm::ops::eval_lt(a, b).unwrap_or(false) {
                    Ordering::Less
                } else if crate::vm::ops::eval_lt(b, a).unwrap_or(false) {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            }
            _ => identity(a).cmp(&identity(b)),
        })
}

/// The keys outside the array part, in iteration order: numbers ascending,
/// strings sorted, then booleans and the rest. The array part always comes
/// first, so a table iterates the same way every run.
pub fn sorted_keys(table: &VmTable) -> Vec<Value> {
    let mut keys: Vec<Value> = table
        .fields
        .iter()
        .filter(|(_, v)| !v.is_nil())
        .map(|(k, _)| Value::str_ref(k.clone()))
        .collect();
    if let Some(general) = &table.general {
        keys.extend(general.values().map(|(k, _)| k.clone()));
    }
    keys.sort_by(compare_keys);
    keys
}

/// A raw lookup that ignores metatables.
pub fn raw_get(table: &VmTable, key: &Value) -> Value {
    table.raw_get(key)
}

/// The entry after `key` in iteration order, as `next` returns it.
///
/// `key` does not have to be in the table any more: assigning nil to the
/// current key while iterating is allowed, and the entry after it is still
/// the one that follows it in iteration order.
pub fn next_entry(table: &VmTable, key: &Value) -> Result<Option<(Value, Value)>, String> {
    let array_start = match key.view() {
        ValueView::Nil => Some(0),
        ValueView::Int(i) if i >= 1 && (i as u64) <= table.array.len() as u64 => Some(i as usize),
        // A key trimmed off the end of the array: the array is done.
        ValueView::Int(i)
            if i >= 1
                && (i as u64) <= table.array_peak as u64
                && table.get_general(&TableKey::Int(i)).is_none() =>
        {
            Some(table.array.len())
        }
        ValueView::Float(f) if f.is_nan() => return Err("invalid key to 'next'".to_string()),
        _ => None,
    };
    if let Some(start) = array_start {
        for (i, v) in table.array.iter().enumerate().skip(start) {
            if !v.is_nil() {
                return Ok(Some((Value::from_usize(i + 1), v.clone())));
            }
        }
        if table.fields.is_empty() && table.general.is_none() {
            return Ok(None);
        }
    }
    // The smallest hash key after `key` (or the smallest one when coming
    // from the array) is next, which a scan finds without sorting.
    let after = if array_start.is_some() {
        None
    } else {
        Some(key)
    };
    let mut best: Option<Value> = None;
    for k in table
        .fields
        .iter()
        .filter(|(_, v)| !v.is_nil())
        .map(|(k, _)| Value::str_ref(k.clone()))
        .chain(
            table
                .general
                .iter()
                .flat_map(|g| g.values().map(|(k, _)| k.clone())),
        )
    {
        if after.is_some_and(|a| compare_keys(&k, a) != std::cmp::Ordering::Greater) {
            continue;
        }
        if best
            .as_ref()
            .is_none_or(|b| compare_keys(&k, b) == std::cmp::Ordering::Less)
        {
            best = Some(k);
        }
    }
    Ok(best.map(|k| {
        let v = table.raw_get(&k);
        (k, v)
    }))
}

/// A fast string-keyed map for a table's named fields (`obj.id`, `obj[k]`).
///
/// Small tables (`<= SMALL_FIELD_LIMIT` fields) store entries inline in a
/// compact `Vec<FieldEntry>`, avoiding power-of-two slack and `None`-slot
/// zeroing on small object literals while scanning within a single cache line.
/// Larger tables promote automatically to an open-addressing power-of-two hash
/// table with Knuth backward-shift deletion (zero tombstones). Keys store their
/// precomputed 64-bit hash, eliminating re-hashing across lookups and growth.
const SMALL_FIELD_LIMIT: usize = 16;

#[derive(Clone, Debug)]
pub struct FieldEntry {
    pub hash: u64,
    pub key: StrRef,
    pub val: Value,
}

#[derive(Clone, Debug)]
struct HashFieldMap {
    entries: Vec<Option<FieldEntry>>,
    count: usize,
}

#[inline(always)]
pub(crate) fn assign_value(slot: &mut Value, val: &Value) {
    slot.assign_ref(val);
}

#[inline(always)]
fn entry_matches_strref(entry: &FieldEntry, hash: u64, key: &StrRef) -> bool {
    Rc::ptr_eq(&entry.key.0, &key.0) || (entry.hash == hash && entry.key.0.str == key.0.str)
}

#[inline(always)]
fn entry_matches_vmstr(entry: &FieldEntry, key_ptr: *const VmString, key: &VmString) -> bool {
    std::ptr::eq(Rc::as_ptr(&entry.key.0), key_ptr)
        || (entry.hash == key.hash && entry.key.0.str == key.str)
}

impl HashFieldMap {
    fn with_capacity(cap: usize) -> Self {
        let n = (cap * 4 / 3 + 1).next_power_of_two().max(4);
        let mut entries = Vec::with_capacity(n);
        entries.resize_with(n, || None);
        Self { entries, count: 0 }
    }

    fn from_small(small: Vec<FieldEntry>) -> Self {
        let cap = (small.len() * 2).next_power_of_two().max(16);
        let mut entries = Vec::with_capacity(cap);
        entries.resize_with(cap, || None);
        let mask = cap - 1;
        let count = small.len();
        for entry in small {
            let mut idx = (entry.hash as usize) & mask;
            while entries[idx].is_some() {
                idx = (idx + 1) & mask;
            }
            entries[idx] = Some(entry);
        }
        Self { entries, count }
    }

    /// Probes the open-addressed table for an entry matching `matches`,
    /// returning `Ok(index)` on hit or `Err(empty_index)` on miss.
    /// Caller must ensure `!self.entries.is_empty()` and `self.count < self.entries.len()`.
    #[inline(always)]
    fn find_slot(
        &self,
        hash: u64,
        mut matches: impl FnMut(&FieldEntry) -> bool,
    ) -> Result<usize, usize> {
        let mask = self.entries.len() - 1;
        let mut idx = (hash as usize) & mask;
        loop {
            match &self.entries[idx] {
                None => return Err(idx),
                Some(entry) if matches(entry) => return Ok(idx),
                _ => idx = (idx + 1) & mask,
            }
        }
    }

    #[inline]
    fn get_by_strref(&self, key: &StrRef) -> Option<&Value> {
        if self.count == 0 {
            return None;
        }
        let hash = key.0.hash;
        let idx = self
            .find_slot(hash, |e| entry_matches_strref(e, hash, key))
            .ok()?;
        self.entries[idx].as_ref().map(|e| &e.val)
    }

    #[inline]
    fn get_mut_by_strref(&mut self, key: &StrRef) -> Option<&mut Value> {
        if self.count == 0 {
            return None;
        }
        let hash = key.0.hash;
        let idx = self
            .find_slot(hash, |e| entry_matches_strref(e, hash, key))
            .ok()?;
        self.entries[idx].as_mut().map(|e| &mut e.val)
    }

    #[inline]
    fn get_by_vmstr(&self, key_ptr: *const VmString, key: &VmString) -> Option<&Value> {
        if self.count == 0 {
            return None;
        }
        let idx = self
            .find_slot(key.hash, |e| entry_matches_vmstr(e, key_ptr, key))
            .ok()?;
        self.entries[idx].as_ref().map(|e| &e.val)
    }

    #[inline]
    fn get_mut_by_vmstr(&mut self, key_ptr: *const VmString, key: &VmString) -> Option<&mut Value> {
        if self.count == 0 {
            return None;
        }
        let idx = self
            .find_slot(key.hash, |e| entry_matches_vmstr(e, key_ptr, key))
            .ok()?;
        self.entries[idx].as_mut().map(|e| &mut e.val)
    }

    #[inline]
    fn get(&self, key: &str) -> Option<&Value> {
        if self.count == 0 {
            return None;
        }
        let hash = crate::vm::hash::fx_hash_bytes(key.as_bytes());
        let idx = self
            .find_slot(hash, |e| e.hash == hash && e.key.0.str == key)
            .ok()?;
        self.entries[idx].as_ref().map(|e| &e.val)
    }

    #[inline]
    fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        if self.count == 0 {
            return None;
        }
        let hash = crate::vm::hash::fx_hash_bytes(key.as_bytes());
        let idx = self
            .find_slot(hash, |e| e.hash == hash && e.key.0.str == key)
            .ok()?;
        self.entries[idx].as_mut().map(|e| &mut e.val)
    }

    fn insert(&mut self, key: StrRef, val: Value) -> Option<Value> {
        let hash = key.0.hash;
        if !self.entries.is_empty() {
            match self.find_slot(hash, |e| entry_matches_strref(e, hash, &key)) {
                Ok(idx) => {
                    return self.entries[idx]
                        .as_mut()
                        .map(|e| std::mem::replace(&mut e.val, val));
                }
                Err(idx) if (self.count + 1) * 4 <= self.entries.len() * 3 => {
                    self.entries[idx] = Some(FieldEntry { hash, key, val });
                    self.count += 1;
                    return None;
                }
                Err(_) => {}
            }
        }
        self.grow();
        let Err(idx) = self.find_slot(hash, |e| entry_matches_strref(e, hash, &key)) else {
            unreachable!("missing key cannot exist after grow");
        };
        self.entries[idx] = Some(FieldEntry { hash, key, val });
        self.count += 1;
        None
    }

    #[inline]
    fn set_by_strref(&mut self, key: &StrRef, val: &Value) {
        let hash = key.0.hash;
        if !self.entries.is_empty() {
            match self.find_slot(hash, |e| entry_matches_strref(e, hash, key)) {
                Ok(idx) => {
                    if let Some(entry) = &mut self.entries[idx] {
                        assign_value(&mut entry.val, val);
                    }
                    return;
                }
                Err(idx) if (self.count + 1) * 4 <= self.entries.len() * 3 => {
                    self.entries[idx] = Some(FieldEntry {
                        hash,
                        key: key.clone(),
                        val: val.clone(),
                    });
                    self.count += 1;
                    return;
                }
                Err(_) => {}
            }
        }
        self.grow();
        let Err(idx) = self.find_slot(hash, |e| entry_matches_strref(e, hash, key)) else {
            unreachable!("missing key cannot exist after grow");
        };
        self.entries[idx] = Some(FieldEntry {
            hash,
            key: key.clone(),
            val: val.clone(),
        });
        self.count += 1;
    }

    #[inline]
    fn set_by_vmstr(&mut self, key_ptr: *const VmString, key: &VmString, val: &Value) {
        let clone_key = || {
            // SAFETY: `key_ptr` came from `Rc::into_raw` on a live `VmString`.
            unsafe {
                Rc::increment_strong_count(key_ptr);
                StrRef(Rc::from_raw(key_ptr))
            }
        };
        let hash = key.hash;
        if !self.entries.is_empty() {
            match self.find_slot(hash, |e| entry_matches_vmstr(e, key_ptr, key)) {
                Ok(idx) => {
                    if let Some(entry) = &mut self.entries[idx] {
                        assign_value(&mut entry.val, val);
                    }
                    return;
                }
                Err(idx) if (self.count + 1) * 4 <= self.entries.len() * 3 => {
                    self.entries[idx] = Some(FieldEntry {
                        hash,
                        key: clone_key(),
                        val: val.clone(),
                    });
                    self.count += 1;
                    return;
                }
                Err(_) => {}
            }
        }
        self.grow();
        let Err(idx) = self.find_slot(hash, |e| entry_matches_vmstr(e, key_ptr, key)) else {
            unreachable!("missing key cannot exist after grow");
        };
        self.entries[idx] = Some(FieldEntry {
            hash,
            key: clone_key(),
            val: val.clone(),
        });
        self.count += 1;
    }

    fn grow(&mut self) {
        let new_cap = (self.entries.len() * 2).max(4);
        let mut new_entries = Vec::with_capacity(new_cap);
        new_entries.resize_with(new_cap, || None);
        let new_mask = new_cap - 1;
        for entry in self.entries.drain(..).flatten() {
            let mut idx = (entry.hash as usize) & new_mask;
            while new_entries[idx].is_some() {
                idx = (idx + 1) & new_mask;
            }
            new_entries[idx] = Some(entry);
        }
        self.entries = new_entries;
    }

    fn remove_at(&mut self, found: usize) -> Option<Value> {
        let mask = self.entries.len() - 1;
        let removed = self.entries[found].take().map(|e| e.val);
        self.count -= 1;
        let mut i = found;
        let mut j = (found + 1) & mask;
        while let Some(entry) = &self.entries[j] {
            let nat = (entry.hash as usize) & mask;
            let dist_to_nat = (j + mask + 1 - nat) & mask;
            let dist_to_i = (j + mask + 1 - i) & mask;
            if dist_to_i <= dist_to_nat {
                self.entries[i] = self.entries[j].take();
                i = j;
            }
            j = (j + 1) & mask;
        }
        removed
    }

    fn remove(&mut self, key: &str) -> Option<Value> {
        if self.count == 0 {
            return None;
        }
        let hash = crate::vm::hash::fx_hash_bytes(key.as_bytes());
        let found = self
            .find_slot(hash, |e| e.hash == hash && e.key.0.str == key)
            .ok()?;
        self.remove_at(found)
    }

    fn remove_by_vmstr(&mut self, key_ptr: *const VmString, key: &VmString) -> Option<Value> {
        if self.count == 0 {
            return None;
        }
        let found = self
            .find_slot(key.hash, |e| entry_matches_vmstr(e, key_ptr, key))
            .ok()?;
        self.remove_at(found)
    }

    fn remove_by_strref(&mut self, key: &StrRef) -> Option<Value> {
        if self.count == 0 {
            return None;
        }
        let hash = key.0.hash;
        let found = self
            .find_slot(hash, |e| entry_matches_strref(e, hash, key))
            .ok()?;
        self.remove_at(found)
    }

    fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&StrRef, &mut Value) -> bool,
    {
        if self.count == 0 {
            return;
        }
        let old_entries = std::mem::take(&mut self.entries);
        let cap = old_entries.len();
        self.entries = Vec::with_capacity(cap);
        self.entries.resize_with(cap, || None);
        self.count = 0;
        let mask = cap - 1;
        for mut entry in old_entries.into_iter().flatten() {
            if f(&entry.key, &mut entry.val) {
                let mut idx = (entry.hash as usize) & mask;
                while self.entries[idx].is_some() {
                    idx = (idx + 1) & mask;
                }
                self.entries[idx] = Some(entry);
                self.count += 1;
            }
        }
    }
}

#[derive(Clone, Debug)]
enum FieldStorage {
    Small(Vec<FieldEntry>),
    Hash(Box<HashFieldMap>),
}

#[derive(Clone, Debug)]
pub struct FieldMap {
    inner: FieldStorage,
    /// Bumped on every structural change (key added or removed, storage
    /// promoted or cleared) so inline caches holding a slot index can tell when
    /// it may have moved. Overwriting an existing value never bumps it.
    shape_version: u64,
}

impl Default for FieldMap {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

enum FieldEntryIter<'a> {
    Small(std::slice::Iter<'a, FieldEntry>),
    Hash(std::slice::Iter<'a, Option<FieldEntry>>),
}

impl<'a> Iterator for FieldEntryIter<'a> {
    type Item = &'a FieldEntry;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Small(it) => it.next(),
            Self::Hash(it) => it.find_map(|opt| opt.as_ref()),
        }
    }
}

enum FieldEntryIterMut<'a> {
    Small(std::slice::IterMut<'a, FieldEntry>),
    Hash(std::slice::IterMut<'a, Option<FieldEntry>>),
}

impl<'a> Iterator for FieldEntryIterMut<'a> {
    type Item = &'a mut FieldEntry;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Small(it) => it.next(),
            Self::Hash(it) => it.find_map(|opt| opt.as_mut()),
        }
    }
}

impl FieldMap {
    #[inline]
    pub fn new() -> Self {
        Self {
            inner: FieldStorage::Small(Vec::new()),
            shape_version: 0,
        }
    }

    #[inline]
    pub fn with_capacity(cap: usize) -> Self {
        let inner = if cap == 0 {
            FieldStorage::Small(Vec::new())
        } else if cap <= SMALL_FIELD_LIMIT {
            FieldStorage::Small(Vec::with_capacity(cap))
        } else {
            FieldStorage::Hash(Box::new(HashFieldMap::with_capacity(cap)))
        };
        Self {
            inner,
            shape_version: 0,
        }
    }

    #[inline(always)]
    pub fn shape_version(&self) -> u32 {
        self.shape_version as u32
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        match &self.inner {
            FieldStorage::Small(entries) => entries.capacity(),
            FieldStorage::Hash(map) => map.entries.len(),
        }
    }

    #[inline]
    pub fn reserve_at_least(&mut self, cap: usize) {
        if cap > self.capacity() {
            *self = Self::with_capacity(cap);
        }
    }

    #[inline(always)]
    fn bump_shape(&mut self) {
        self.shape_version = self.shape_version.wrapping_add(1);
    }

    /// Storage slot holding `key`, valid while `shape_version` is unchanged.
    #[inline]
    pub fn find_index_by_strref(&self, key: &StrRef) -> Option<usize> {
        match &self.inner {
            FieldStorage::Small(entries) => {
                let hash = key.0.hash;
                entries
                    .iter()
                    .position(|e| entry_matches_strref(e, hash, key))
            }
            FieldStorage::Hash(map) => {
                if map.count == 0 {
                    return None;
                }
                let hash = key.0.hash;
                map.find_slot(hash, |e| entry_matches_strref(e, hash, key))
                    .ok()
            }
        }
    }

    #[inline]
    pub fn find_index(&self, key: &str) -> Option<usize> {
        match &self.inner {
            FieldStorage::Small(entries) => {
                let hash = crate::vm::hash::fx_hash_bytes(key.as_bytes());
                entries
                    .iter()
                    .position(|e| e.hash == hash && e.key.0.str == key)
            }
            FieldStorage::Hash(map) => {
                if map.count == 0 {
                    return None;
                }
                let hash = crate::vm::hash::fx_hash_bytes(key.as_bytes());
                map.find_slot(hash, |e| e.hash == hash && e.key.0.str == key)
                    .ok()
            }
        }
    }

    #[inline(always)]
    pub fn get_slot(&self, idx: usize) -> Option<&Value> {
        match &self.inner {
            FieldStorage::Small(entries) => entries.get(idx).map(|e| &e.val),
            FieldStorage::Hash(map) => map
                .entries
                .get(idx)
                .and_then(|opt| opt.as_ref().map(|e| &e.val)),
        }
    }

    /// `get_slot`, but only if the slot still holds `key`. Inline caches key on
    /// the table address and `shape_version`, and both can repeat (a freed
    /// table's address is reused by a new table whose version also starts at 0),
    /// so a cache hit must confirm the key before trusting the slot.
    #[inline(always)]
    pub fn get_slot_keyed(&self, idx: usize, key: &StrRef) -> Option<&Value> {
        let entry = match &self.inner {
            FieldStorage::Small(entries) => entries.get(idx),
            FieldStorage::Hash(map) => map.entries.get(idx).and_then(|opt| opt.as_ref()),
        }?;
        entry_matches_strref(entry, key.0.hash, key).then_some(&entry.val)
    }

    #[inline(always)]
    pub fn get_slot_keyed_ptr(&self, idx: usize, key: &StrRef) -> Option<&Value> {
        let entry = match &self.inner {
            FieldStorage::Small(entries) => entries.get(idx),
            FieldStorage::Hash(map) => map.entries.get(idx).and_then(|opt| opt.as_ref()),
        }?;
        Rc::ptr_eq(&entry.key.0, &key.0).then_some(&entry.val)
    }

    #[inline(always)]
    pub fn get_slot_keyed_vmstr(
        &self,
        idx: usize,
        key_ptr: *const VmString,
        key: &VmString,
    ) -> Option<&Value> {
        let entry = match &self.inner {
            FieldStorage::Small(entries) => entries.get(idx),
            FieldStorage::Hash(map) => map.entries.get(idx).and_then(|opt| opt.as_ref()),
        }?;
        entry_matches_vmstr(entry, key_ptr, key).then_some(&entry.val)
    }

    #[inline(always)]
    pub fn get_slot_keyed_vmstr_ptr(&self, idx: usize, key_ptr: *const VmString) -> Option<&Value> {
        let entry = match &self.inner {
            FieldStorage::Small(entries) => entries.get(idx),
            FieldStorage::Hash(map) => map.entries.get(idx).and_then(|opt| opt.as_ref()),
        }?;
        std::ptr::eq(Rc::as_ptr(&entry.key.0), key_ptr).then_some(&entry.val)
    }

    #[inline(always)]
    pub fn get_slot_keyed_mut_vmstr(
        &mut self,
        idx: usize,
        key_ptr: *const VmString,
        key: &VmString,
    ) -> Option<&mut Value> {
        let entry = match &mut self.inner {
            FieldStorage::Small(entries) => entries.get_mut(idx),
            FieldStorage::Hash(map) => map.entries.get_mut(idx).and_then(|opt| opt.as_mut()),
        }?;
        entry_matches_vmstr(entry, key_ptr, key).then_some(&mut entry.val)
    }

    #[inline(always)]
    pub fn get_slot_keyed_mut_vmstr_ptr(
        &mut self,
        idx: usize,
        key_ptr: *const VmString,
    ) -> Option<&mut Value> {
        let entry = match &mut self.inner {
            FieldStorage::Small(entries) => entries.get_mut(idx),
            FieldStorage::Hash(map) => map.entries.get_mut(idx).and_then(|opt| opt.as_mut()),
        }?;
        std::ptr::eq(Rc::as_ptr(&entry.key.0), key_ptr).then_some(&mut entry.val)
    }

    #[inline]
    pub fn find_index_by_vmstr(&self, key_ptr: *const VmString, key: &VmString) -> Option<usize> {
        match &self.inner {
            FieldStorage::Small(entries) => entries
                .iter()
                .position(|e| entry_matches_vmstr(e, key_ptr, key)),
            FieldStorage::Hash(map) => {
                if map.count == 0 {
                    return None;
                }
                map.find_slot(key.hash, |e| entry_matches_vmstr(e, key_ptr, key))
                    .ok()
            }
        }
    }

    #[inline(always)]
    pub fn get_slot_keyed_mut(&mut self, idx: usize, key: &StrRef) -> Option<&mut Value> {
        let entry = match &mut self.inner {
            FieldStorage::Small(entries) => entries.get_mut(idx),
            FieldStorage::Hash(map) => map.entries.get_mut(idx).and_then(|opt| opt.as_mut()),
        }?;
        entry_matches_strref(entry, key.0.hash, key).then_some(&mut entry.val)
    }

    #[inline(always)]
    pub fn get_slot_keyed_mut_ptr(&mut self, idx: usize, key: &StrRef) -> Option<&mut Value> {
        let entry = match &mut self.inner {
            FieldStorage::Small(entries) => entries.get_mut(idx),
            FieldStorage::Hash(map) => map.entries.get_mut(idx).and_then(|opt| opt.as_mut()),
        }?;
        Rc::ptr_eq(&entry.key.0, &key.0).then_some(&mut entry.val)
    }

    /// `set_slot`, but only if the slot still holds `key` (see `get_slot_keyed`).
    #[inline(always)]
    pub fn set_slot_keyed(&mut self, idx: usize, key: &StrRef, val: Value) -> bool {
        let entry = match &mut self.inner {
            FieldStorage::Small(entries) => entries.get_mut(idx),
            FieldStorage::Hash(map) => map.entries.get_mut(idx).and_then(|opt| opt.as_mut()),
        };
        match entry {
            Some(e) if entry_matches_strref(e, key.0.hash, key) => {
                e.val = val;
                true
            }
            _ => false,
        }
    }

    #[inline(always)]
    pub fn set_slot(&mut self, idx: usize, val: Value) -> bool {
        let slot = match &mut self.inner {
            FieldStorage::Small(entries) => entries.get_mut(idx).map(|e| &mut e.val),
            FieldStorage::Hash(map) => map
                .entries
                .get_mut(idx)
                .and_then(|opt| opt.as_mut().map(|e| &mut e.val)),
        };
        match slot {
            Some(v) => {
                *v = val;
                true
            }
            None => false,
        }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        match &self.inner {
            FieldStorage::Small(entries) => entries.len(),
            FieldStorage::Hash(map) => map.count,
        }
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    pub fn get_by_vmstr(&self, key_ptr: *const VmString, key: &VmString) -> Option<&Value> {
        match &self.inner {
            FieldStorage::Small(entries) => {
                for entry in entries {
                    if entry_matches_vmstr(entry, key_ptr, key) {
                        return Some(&entry.val);
                    }
                }
                None
            }
            FieldStorage::Hash(map) => map.get_by_vmstr(key_ptr, key),
        }
    }

    #[inline]
    pub fn get_mut_by_vmstr(
        &mut self,
        key_ptr: *const VmString,
        key: &VmString,
    ) -> Option<&mut Value> {
        match &mut self.inner {
            FieldStorage::Small(entries) => {
                for entry in entries {
                    if entry_matches_vmstr(entry, key_ptr, key) {
                        return Some(&mut entry.val);
                    }
                }
                None
            }
            FieldStorage::Hash(map) => map.get_mut_by_vmstr(key_ptr, key),
        }
    }

    #[inline]
    pub fn get_by_strref(&self, key: &StrRef) -> Option<&Value> {
        match &self.inner {
            FieldStorage::Small(entries) => {
                let hash = key.0.hash;
                for entry in entries {
                    if entry_matches_strref(entry, hash, key) {
                        return Some(&entry.val);
                    }
                }
                None
            }
            FieldStorage::Hash(map) => map.get_by_strref(key),
        }
    }

    #[inline]
    pub fn get_mut_by_strref(&mut self, key: &StrRef) -> Option<&mut Value> {
        match &mut self.inner {
            FieldStorage::Small(entries) => {
                let hash = key.0.hash;
                for entry in entries {
                    if entry_matches_strref(entry, hash, key) {
                        return Some(&mut entry.val);
                    }
                }
                None
            }
            FieldStorage::Hash(map) => map.get_mut_by_strref(key),
        }
    }

    #[inline]
    pub fn get(&self, key: &str) -> Option<&Value> {
        match &self.inner {
            FieldStorage::Small(entries) => {
                if entries.is_empty() {
                    return None;
                }
                let hash = crate::vm::hash::fx_hash_bytes(key.as_bytes());
                for entry in entries {
                    if entry.hash == hash && entry.key.0.str == key {
                        return Some(&entry.val);
                    }
                }
                None
            }
            FieldStorage::Hash(map) => map.get(key),
        }
    }

    #[inline]
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        match &mut self.inner {
            FieldStorage::Small(entries) => {
                if entries.is_empty() {
                    return None;
                }
                let hash = crate::vm::hash::fx_hash_bytes(key.as_bytes());
                for entry in entries {
                    if entry.hash == hash && entry.key.0.str == key {
                        return Some(&mut entry.val);
                    }
                }
                None
            }
            FieldStorage::Hash(map) => map.get_mut(key),
        }
    }

    #[inline]
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Promotes small linear storage to a boxed open-addressed hash map.
    fn promote_to_hash(&mut self) -> &mut HashFieldMap {
        if let FieldStorage::Small(entries) = &mut self.inner {
            let small = std::mem::take(entries);
            self.inner = FieldStorage::Hash(Box::new(HashFieldMap::from_small(small)));
        }
        match &mut self.inner {
            FieldStorage::Hash(map) => map,
            FieldStorage::Small(_) => unreachable!("just promoted to Hash"),
        }
    }

    /// Single-probe insert-or-update returning the resulting slot index,
    /// bumping `shape_version` only if the entry count grew.
    #[inline]
    pub fn insert_cached(&mut self, key: StrRef, val: Value) -> usize {
        let before = self.len();
        let slot_idx = match &mut self.inner {
            FieldStorage::Small(entries) => {
                let hash = key.0.hash;
                if let Some(pos) = entries
                    .iter()
                    .position(|e| entry_matches_strref(e, hash, &key))
                {
                    entries[pos].val = val;
                    pos
                } else if entries.len() < SMALL_FIELD_LIMIT {
                    let pos = entries.len();
                    entries.push(FieldEntry { hash, key, val });
                    pos
                } else {
                    let map = self.promote_to_hash();
                    map.insert(key.clone(), val);
                    map.find_slot(key.0.hash, |e| entry_matches_strref(e, key.0.hash, &key))
                        .unwrap_or(0)
                }
            }
            FieldStorage::Hash(map) => {
                let hash = key.0.hash;
                map.insert(key.clone(), val);
                map.find_slot(hash, |e| entry_matches_strref(e, hash, &key))
                    .unwrap_or(0)
            }
        };
        if self.len() != before {
            self.bump_shape();
        }
        slot_idx
    }

    pub fn insert(&mut self, key: StrRef, val: Value) -> Option<Value> {
        let before = self.len();
        let old = self.insert_inner(key, val);
        if self.len() != before {
            self.bump_shape();
        }
        old
    }

    fn insert_inner(&mut self, key: StrRef, val: Value) -> Option<Value> {
        match &mut self.inner {
            FieldStorage::Small(entries) => {
                let hash = key.0.hash;
                for entry in entries.iter_mut() {
                    if entry_matches_strref(entry, hash, &key) {
                        return Some(std::mem::replace(&mut entry.val, val));
                    }
                }
                if entries.len() < SMALL_FIELD_LIMIT {
                    entries.push(FieldEntry { hash, key, val });
                    return None;
                }
                self.promote_to_hash().insert(key, val)
            }
            FieldStorage::Hash(map) => map.insert(key, val),
        }
    }

    /// Single-probe update-or-insert for VM `SetTable` / `SetTableK` fast paths.
    /// Updates primitive values in place on hit and only clones `key` on miss.
    #[inline]
    pub fn set_by_strref(&mut self, key: &StrRef, val: &Value) {
        let before = self.len();
        self.set_by_strref_inner(key, val);
        if self.len() != before {
            self.bump_shape();
        }
    }

    #[inline]
    fn set_by_strref_inner(&mut self, key: &StrRef, val: &Value) {
        match &mut self.inner {
            FieldStorage::Small(entries) => {
                let hash = key.0.hash;
                for entry in entries.iter_mut() {
                    if entry_matches_strref(entry, hash, key) {
                        assign_value(&mut entry.val, val);
                        return;
                    }
                }
                if entries.len() < SMALL_FIELD_LIMIT {
                    entries.push(FieldEntry {
                        hash,
                        key: key.clone(),
                        val: val.clone(),
                    });
                    return;
                }
                self.promote_to_hash().set_by_strref(key, val);
            }
            FieldStorage::Hash(map) => map.set_by_strref(key, val),
        }
    }

    #[inline]
    pub fn set_by_vmstr(&mut self, key_ptr: *const VmString, key: &VmString, val: &Value) {
        let before = self.len();
        self.set_by_vmstr_inner(key_ptr, key, val);
        if self.len() != before {
            self.bump_shape();
        }
    }

    #[inline]
    fn set_by_vmstr_inner(&mut self, key_ptr: *const VmString, key: &VmString, val: &Value) {
        let clone_key = || {
            // SAFETY: `key_ptr` came from `Rc::into_raw` on a live `VmString`.
            unsafe {
                Rc::increment_strong_count(key_ptr);
                StrRef(Rc::from_raw(key_ptr))
            }
        };
        match &mut self.inner {
            FieldStorage::Small(entries) => {
                for entry in entries.iter_mut() {
                    if entry_matches_vmstr(entry, key_ptr, key) {
                        assign_value(&mut entry.val, val);
                        return;
                    }
                }
                if entries.len() < SMALL_FIELD_LIMIT {
                    entries.push(FieldEntry {
                        hash: key.hash,
                        key: clone_key(),
                        val: val.clone(),
                    });
                    return;
                }
                self.promote_to_hash().set_by_vmstr(key_ptr, key, val);
            }
            FieldStorage::Hash(map) => map.set_by_vmstr(key_ptr, key, val),
        }
    }

    pub fn remove_by_vmstr(&mut self, key_ptr: *const VmString, key: &VmString) -> Option<Value> {
        let old = self.remove_by_vmstr_inner(key_ptr, key);
        if old.is_some() {
            self.bump_shape();
        }
        old
    }

    fn remove_by_vmstr_inner(&mut self, key_ptr: *const VmString, key: &VmString) -> Option<Value> {
        match &mut self.inner {
            FieldStorage::Small(entries) => {
                let pos = entries
                    .iter()
                    .position(|e| entry_matches_vmstr(e, key_ptr, key))?;
                Some(entries.swap_remove(pos).val)
            }
            FieldStorage::Hash(map) => map.remove_by_vmstr(key_ptr, key),
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        let old = self.remove_inner(key);
        if old.is_some() {
            self.bump_shape();
        }
        old
    }

    fn remove_inner(&mut self, key: &str) -> Option<Value> {
        match &mut self.inner {
            FieldStorage::Small(entries) => {
                if entries.is_empty() {
                    return None;
                }
                let hash = crate::vm::hash::fx_hash_bytes(key.as_bytes());
                let pos = entries
                    .iter()
                    .position(|e| e.hash == hash && e.key.0.str == key)?;
                Some(entries.swap_remove(pos).val)
            }
            FieldStorage::Hash(map) => map.remove(key),
        }
    }

    pub fn remove_by_strref(&mut self, key: &StrRef) -> Option<Value> {
        let old = self.remove_by_strref_inner(key);
        if old.is_some() {
            self.bump_shape();
        }
        old
    }

    fn remove_by_strref_inner(&mut self, key: &StrRef) -> Option<Value> {
        match &mut self.inner {
            FieldStorage::Small(entries) => {
                let hash = key.0.hash;
                let pos = entries
                    .iter()
                    .position(|e| entry_matches_strref(e, hash, key))?;
                Some(entries.swap_remove(pos).val)
            }
            FieldStorage::Hash(map) => map.remove_by_strref(key),
        }
    }

    #[inline]
    pub fn clear(&mut self) {
        self.bump_shape();
        match &mut self.inner {
            FieldStorage::Small(entries) => entries.clear(),
            FieldStorage::Hash(_) => self.inner = FieldStorage::Small(Vec::new()),
        }
    }

    #[inline]
    fn entry_iter(&self) -> FieldEntryIter<'_> {
        match &self.inner {
            FieldStorage::Small(entries) => FieldEntryIter::Small(entries.iter()),
            FieldStorage::Hash(map) => FieldEntryIter::Hash(map.entries.iter()),
        }
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (&StrRef, &Value)> {
        self.entry_iter().map(|e| (&e.key, &e.val))
    }

    #[inline]
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.entry_iter().map(|e| &e.val)
    }

    #[inline]
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Value> {
        let iter = match &mut self.inner {
            FieldStorage::Small(entries) => FieldEntryIterMut::Small(entries.iter_mut()),
            FieldStorage::Hash(map) => FieldEntryIterMut::Hash(map.entries.iter_mut()),
        };
        iter.map(|e| &mut e.val)
    }

    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&StrRef, &mut Value) -> bool,
    {
        let before = self.len();
        match &mut self.inner {
            FieldStorage::Small(entries) => entries.retain_mut(|e| f(&e.key, &mut e.val)),
            FieldStorage::Hash(map) => map.retain(f),
        }
        if self.len() != before {
            self.bump_shape();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyed_slot_access_rejects_a_different_key_at_the_same_shape() {
        // Two tables with the same shape_version but different key layouts, as
        // when a freed table's address is reused: a cached slot index from one
        // must not read or write the other's field.
        let (a, b) = (StrRef::from("a"), StrRef::from("b"));
        let mut first = FieldMap::new();
        first.insert(a.clone(), Value::int(1));
        first.insert(b.clone(), Value::int(2));
        let mut second = FieldMap::new();
        second.insert(b.clone(), Value::int(20));
        second.insert(a.clone(), Value::int(10));
        assert_eq!(first.shape_version(), second.shape_version());
        let idx = first.find_index_by_strref(&a).unwrap();
        assert_eq!(first.get_slot_keyed(idx, &a), Some(&Value::int(1)));
        assert!(second.get_slot_keyed(idx, &a).is_none());
        assert!(!second.set_slot_keyed(idx, &a, Value::int(99)));
        assert_eq!(second.get_slot(idx), Some(&Value::int(20)));
    }

    #[test]
    fn test_table_key_conversions() {
        assert_eq!(
            TableKey::from_value(&Value::bool(true)),
            Ok(TableKey::Bool(true))
        );
        assert_eq!(
            TableKey::from_value(&Value::int(-42)),
            Ok(TableKey::Int(-42))
        );
        assert_eq!(
            TableKey::from_value(&Value::float(10.0)),
            Ok(TableKey::Int(10))
        );
        assert!(TableKey::from_value(&Value::NIL).is_err());
        assert!(TableKey::from_value(&Value::float(f64::NAN)).is_err());
    }

    #[test]
    fn test_field_map_crud_and_backward_shift() {
        let mut map = FieldMap::new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);

        // Insert keys
        for i in 0..50 {
            let key = StrRef::from(format!("field_{i}"));
            assert!(map.insert(key, Value::int(i * 10)).is_none());
        }
        assert_eq!(map.len(), 50);

        // Read back
        for i in 0..50 {
            let key_str = format!("field_{i}");
            let key = StrRef::from(key_str.clone());
            assert_eq!(map.get_by_strref(&key), Some(&Value::int(i * 10)));
            assert_eq!(map.get(&key_str), Some(&Value::int(i * 10)));
        }

        // Overwrite
        let k0 = StrRef::from("field_0");
        assert_eq!(map.insert(k0.clone(), Value::int(999)), Some(Value::int(0)));
        assert_eq!(map.get_by_strref(&k0), Some(&Value::int(999)));

        // Remove every second key
        for i in (0..50).step_by(2) {
            let key_str = format!("field_{i}");
            assert!(map.remove(&key_str).is_some());
            assert_eq!(map.get(&key_str), None);
        }
        assert_eq!(map.len(), 25);

        // Verify remaining odd keys are still reachable
        for i in (1..50).step_by(2) {
            let key_str = format!("field_{i}");
            let key = StrRef::from(key_str.clone());
            assert_eq!(map.get_by_strref(&key), Some(&Value::int(i * 10)));
        }

        // Clear
        map.clear();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn test_field_map_small_to_hash_promotion_and_set_by_strref() {
        assert!(std::mem::size_of::<FieldMap>() <= 32);

        let mut map = FieldMap::with_capacity(3);
        assert!(matches!(map.inner, FieldStorage::Small(_)));

        // Fill up to the small limit (8 entries) via set_by_strref.
        for i in 0..SMALL_FIELD_LIMIT {
            let k = StrRef::from(format!("k{i}"));
            map.set_by_strref(&k, &Value::int(i as i64));
        }
        assert!(matches!(map.inner, FieldStorage::Small(_)));
        assert_eq!(map.len(), SMALL_FIELD_LIMIT);

        // In-place primitive overwrite in Small mode keeps Small representation.
        let k2 = StrRef::from("k2");
        map.set_by_strref(&k2, &Value::int(200));
        assert!(matches!(map.inner, FieldStorage::Small(_)));
        assert_eq!(map.get("k2"), Some(&Value::int(200)));

        // Remove and re-insert in Small mode.
        assert_eq!(map.remove_by_strref(&k2), Some(Value::int(200)));
        assert_eq!(map.len(), SMALL_FIELD_LIMIT - 1);
        map.set_by_strref(&k2, &Value::int(202));
        assert!(matches!(map.inner, FieldStorage::Small(_)));

        // `(SMALL_FIELD_LIMIT + 1)`-th distinct key promotes seamlessly to Hash mode.
        let k_promote = StrRef::from(format!("k{SMALL_FIELD_LIMIT}"));
        map.set_by_strref(&k_promote, &Value::int(800));
        assert!(matches!(map.inner, FieldStorage::Hash(_)));
        assert_eq!(map.len(), SMALL_FIELD_LIMIT + 1);
        assert_eq!(map.get_by_strref(&k_promote), Some(&Value::int(800)));
        assert_eq!(map.get("k2"), Some(&Value::int(202)));
        for i in 0..SMALL_FIELD_LIMIT {
            let expected = if i == 2 { 202 } else { i as i64 };
            assert_eq!(map.get(&format!("k{i}")), Some(&Value::int(expected)));
        }

        // Overwrite in Hash mode via set_by_strref.
        map.set_by_strref(&k_promote, &Value::float(8.5));
        assert_eq!(map.get(&k_promote), Some(&Value::float(8.5)));

        // Retain in both Small and Hash modes.
        map.retain(|k, _| **k == *k_promote || &**k == "k2");
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("k2"), Some(&Value::int(202)));
        assert_eq!(map.get(&k_promote), Some(&Value::float(8.5)));

        let mut small = FieldMap::with_capacity(4);
        small.insert(StrRef::from("a"), Value::int(1));
        small.insert(StrRef::from("b"), Value::int(2));
        small.retain(|k, _| &**k == "b");
        assert!(matches!(small.inner, FieldStorage::Small(_)));
        assert_eq!(small.len(), 1);
        assert_eq!(small.get("b"), Some(&Value::int(2)));
        assert_eq!(small.get("a"), None);
    }

    #[test]
    fn test_field_map_update_at_load_threshold_does_not_rehash() {
        // 24 keys in a 32-slot hash map sits right at the 75% load limit
        // (`(24 + 1) * 4 > 32 * 3`). Updating existing keys must not rehash.
        let mut map = FieldMap::new();
        for i in 0..24 {
            map.insert(StrRef::from(format!("k{i}")), Value::int(i));
        }
        let cap_before = match &map.inner {
            FieldStorage::Hash(h) => h.entries.len(),
            FieldStorage::Small(_) => panic!("expected Hash storage"),
        };
        assert_eq!(cap_before, 32);

        let k0 = StrRef::from("k0");
        map.set_by_strref(&k0, &Value::int(999));
        map.insert(StrRef::from("k1"), Value::int(888));

        let cap_after = match &map.inner {
            FieldStorage::Hash(h) => h.entries.len(),
            FieldStorage::Small(_) => panic!("expected Hash storage"),
        };
        assert_eq!(cap_after, cap_before);
        assert_eq!(map.get("k0"), Some(&Value::int(999)));
        assert_eq!(map.get("k1"), Some(&Value::int(888)));
    }
}
