//! Hashable table keys. Any value except nil and NaN can be a key.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::vm::buffer::VmBuffer;
use crate::vm::value::{NativeDef, StrRef, Value, VmClosure, VmTable};

#[derive(Clone, Debug)]
pub enum TableKey {
    Bool(bool),
    Int(i64),
    Float(u64), // normalized f64 bits
    String(StrRef),
    Table(*const RefCell<VmTable>),
    Closure(*const VmClosure),
    Native(*const NativeDef),
    Buffer(*const RefCell<VmBuffer>),
}

impl PartialEq for TableKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (TableKey::Bool(a), TableKey::Bool(b)) => a == b,
            (TableKey::Int(a), TableKey::Int(b)) => a == b,
            (TableKey::Float(a), TableKey::Float(b)) => a == b,
            (TableKey::String(a), TableKey::String(b)) => a == b,
            (TableKey::Table(a), TableKey::Table(b)) => std::ptr::eq(*a, *b),
            (TableKey::Closure(a), TableKey::Closure(b)) => std::ptr::eq(*a, *b),
            (TableKey::Native(a), TableKey::Native(b)) => std::ptr::eq(*a, *b),
            (TableKey::Buffer(a), TableKey::Buffer(b)) => std::ptr::eq(*a, *b),
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
            TableKey::Closure(p) => p.hash(state),
            TableKey::Native(p) => p.hash(state),
            TableKey::Buffer(p) => p.hash(state),
        }
    }
}

impl TableKey {
    pub fn from_value(val: &Value) -> Result<Self, &'static str> {
        match val {
            Value::Nil => Err("table index is nil"),
            Value::Bool(b) => Ok(TableKey::Bool(*b)),
            Value::Int(i) => Ok(TableKey::Int(*i)),
            Value::Float(f) => {
                if f.is_nan() {
                    return Err("table index is NaN");
                }
                // If float is an exact integer, represent as Int to unify lookup
                if f.fract() == 0.0 && *f >= (i64::MIN as f64) && *f <= (i64::MAX as f64) {
                    Ok(TableKey::Int(*f as i64))
                } else {
                    let bits = if *f == 0.0 { 0u64 } else { f.to_bits() };
                    Ok(TableKey::Float(bits))
                }
            }
            Value::BigInt(b) => {
                if let Some(i) = num_traits::ToPrimitive::to_i64(&**b) {
                    Ok(TableKey::Int(i))
                } else {
                    Err("table index too large")
                }
            }
            Value::String(s) => Ok(TableKey::String(s.clone())),
            Value::Table(t) => Ok(TableKey::Table(Rc::as_ptr(t))),
            Value::Closure(c) => Ok(TableKey::Closure(Rc::as_ptr(c))),
            Value::Native(def) => Ok(TableKey::Native(*def as *const _)),
            Value::Buffer(b) => Ok(TableKey::Buffer(Rc::as_ptr(b))),
        }
    }
}

/// Where a non-array key sorts: numbers first (ascending), then strings,
/// booleans, and everything else by identity.
fn key_rank(key: &Value) -> u8 {
    match key {
        Value::Int(_) | Value::BigInt(_) | Value::Float(_) => 0,
        Value::String(_) => 1,
        Value::Bool(_) => 2,
        _ => 3,
    }
}

fn identity(key: &Value) -> usize {
    match key {
        Value::Table(t) => Rc::as_ptr(t) as *const u8 as usize,
        Value::Closure(c) => Rc::as_ptr(c) as *const u8 as usize,
        Value::Native(d) => *d as *const NativeDef as usize,
        Value::Buffer(b) => Rc::as_ptr(b) as *const u8 as usize,
        _ => 0,
    }
}

fn compare_keys(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    key_rank(a).cmp(&key_rank(b)).then_with(|| match (a, b) {
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
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
        .filter(|(_, v)| !matches!(v, Value::Nil))
        .map(|(k, _)| Value::String(k.clone()))
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
    let array_start = match key {
        Value::Nil => Some(0),
        Value::Int(i) if *i >= 1 && (*i as u64) <= table.array.len() as u64 => Some(*i as usize),
        // A key trimmed off the end of the array: the array is done.
        Value::Int(i)
            if *i >= 1
                && (*i as u64) <= table.array_peak as u64
                && table.get_general(&TableKey::Int(*i)).is_none() =>
        {
            Some(table.array.len())
        }
        Value::Float(f) if f.is_nan() => return Err("invalid key to 'next'".to_string()),
        _ => None,
    };
    if let Some(start) = array_start {
        for (i, v) in table.array.iter().enumerate().skip(start) {
            if !matches!(v, Value::Nil) {
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
        .filter(|(_, v)| !matches!(v, Value::Nil))
        .map(|(k, _)| Value::String(k.clone()))
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
/// Implemented as an open-addressing hash table with power-of-two capacity and
/// Knuth backward-shift deletion (zero tombstones). Keys store their precomputed
/// 64-bit hash, eliminating re-hashing across all lookups, mutations, and table growths.
#[derive(Clone, Debug)]
pub struct FieldEntry {
    pub hash: u64,
    pub key: StrRef,
    pub val: Value,
}

#[derive(Clone, Debug, Default)]
pub struct FieldMap {
    entries: Vec<Option<FieldEntry>>,
    count: usize,
    shape_version: u64,
}

impl FieldMap {
    #[inline]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            count: 0,
            shape_version: 0,
        }
    }

    #[inline]
    pub fn with_capacity(cap: usize) -> Self {
        if cap == 0 {
            Self::new()
        } else {
            let n = (cap * 4 / 3 + 1).next_power_of_two().max(4);
            let mut entries = Vec::with_capacity(n);
            entries.resize_with(n, || None);
            Self {
                entries,
                count: 0,
                shape_version: 0,
            }
        }
    }

    #[inline(always)]
    pub fn shape_version(&self) -> u64 {
        self.shape_version
    }

    #[inline(always)]
    pub fn get_slot(&self, idx: usize) -> Option<&Value> {
        self.entries
            .get(idx)
            .and_then(|opt| opt.as_ref().map(|e| &e.val))
    }

    #[inline(always)]
    pub fn set_slot(&mut self, idx: usize, val: Value) -> bool {
        if let Some(Some(entry)) = self.entries.get_mut(idx) {
            entry.val = val;
            true
        } else {
            false
        }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.count
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[inline]
    pub fn find_index_by_strref(&self, key: &StrRef) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let mask = self.entries.len() - 1;
        let hash = key.0.hash;
        let mut idx = (hash as usize) & mask;
        loop {
            let entry = self.entries[idx].as_ref()?;
            if entry.hash == hash
                && (Rc::ptr_eq(&entry.key.0, &key.0) || entry.key.0.str == key.0.str)
            {
                return Some(idx);
            }
            idx = (idx + 1) & mask;
        }
    }

    #[inline]
    pub fn find_index(&self, key: &str) -> Option<usize> {
        if self.count == 0 {
            return None;
        }
        let mask = self.entries.len() - 1;
        let hash = crate::vm::hash::fx_hash_bytes(key.as_bytes());
        let mut idx = (hash as usize) & mask;
        loop {
            let entry = self.entries[idx].as_ref()?;
            if entry.hash == hash && entry.key.0.str == key {
                return Some(idx);
            }
            idx = (idx + 1) & mask;
        }
    }

    #[inline]
    pub fn get_by_strref(&self, key: &StrRef) -> Option<&Value> {
        let idx = self.find_index_by_strref(key)?;
        self.entries[idx].as_ref().map(|entry| &entry.val)
    }

    #[inline]
    pub fn get_mut_by_strref(&mut self, key: &StrRef) -> Option<&mut Value> {
        let idx = self.find_index_by_strref(key)?;
        self.entries[idx].as_mut().map(|entry| &mut entry.val)
    }

    #[inline]
    pub fn get(&self, key: &str) -> Option<&Value> {
        let idx = self.find_index(key)?;
        self.entries[idx].as_ref().map(|entry| &entry.val)
    }

    #[inline]
    pub fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        let idx = self.find_index(key)?;
        self.entries[idx].as_mut().map(|entry| &mut entry.val)
    }

    #[inline]
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn insert(&mut self, key: StrRef, val: Value) -> Option<Value> {
        if self.entries.is_empty() || (self.count + 1) * 4 > self.entries.len() * 3 {
            self.grow();
        }
        let mask = self.entries.len() - 1;
        let hash = key.0.hash;
        let mut idx = (hash as usize) & mask;
        loop {
            match &mut self.entries[idx] {
                None => {
                    self.entries[idx] = Some(FieldEntry { hash, key, val });
                    self.count += 1;
                    self.shape_version = self.shape_version.wrapping_add(1);
                    return None;
                }
                Some(entry) => {
                    if entry.hash == hash
                        && (Rc::ptr_eq(&entry.key.0, &key.0) || entry.key.0.str == key.0.str)
                    {
                        return Some(std::mem::replace(&mut entry.val, val));
                    }
                    idx = (idx + 1) & mask;
                }
            }
        }
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
        self.shape_version = self.shape_version.wrapping_add(1);
    }

    pub fn remove(&mut self, key: &str) -> Option<Value> {
        if self.count == 0 {
            return None;
        }
        let mask = self.entries.len() - 1;
        let hash = crate::vm::hash::fx_hash_bytes(key.as_bytes());
        let mut idx = (hash as usize) & mask;
        let found = loop {
            match &self.entries[idx] {
                None => return None,
                Some(entry) if entry.hash == hash && entry.key.0.str == key => {
                    break idx;
                }
                _ => {
                    idx = (idx + 1) & mask;
                }
            }
        };
        let removed = self.entries[found].take().map(|e| e.val);
        self.count -= 1;
        self.shape_version = self.shape_version.wrapping_add(1);
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

    pub fn remove_by_strref(&mut self, key: &StrRef) -> Option<Value> {
        if self.count == 0 {
            return None;
        }
        let mask = self.entries.len() - 1;
        let hash = key.0.hash;
        let mut idx = (hash as usize) & mask;
        let found = loop {
            match &self.entries[idx] {
                None => return None,
                Some(entry)
                    if entry.hash == hash
                        && (Rc::ptr_eq(&entry.key.0, &key.0) || entry.key.0.str == key.0.str) =>
                {
                    break idx;
                }
                _ => {
                    idx = (idx + 1) & mask;
                }
            }
        };
        let removed = self.entries[found].take().map(|e| e.val);
        self.count -= 1;
        self.shape_version = self.shape_version.wrapping_add(1);
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

    #[inline]
    pub fn clear(&mut self) {
        self.entries.clear();
        self.count = 0;
        self.shape_version = self.shape_version.wrapping_add(1);
    }

    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = (&StrRef, &Value)> {
        self.entries
            .iter()
            .filter_map(|opt| opt.as_ref().map(|e| (&e.key, &e.val)))
    }

    #[inline]
    pub fn values(&self) -> impl Iterator<Item = &Value> {
        self.entries
            .iter()
            .filter_map(|opt| opt.as_ref().map(|e| &e.val))
    }

    #[inline]
    pub fn values_mut(&mut self) -> impl Iterator<Item = &mut Value> {
        self.entries
            .iter_mut()
            .filter_map(|opt| opt.as_mut().map(|e| &mut e.val))
    }

    pub fn retain<F>(&mut self, mut f: F)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_table_key_conversions() {
        assert_eq!(
            TableKey::from_value(&Value::Bool(true)),
            Ok(TableKey::Bool(true))
        );
        assert_eq!(
            TableKey::from_value(&Value::Int(-42)),
            Ok(TableKey::Int(-42))
        );
        assert_eq!(
            TableKey::from_value(&Value::Float(10.0)),
            Ok(TableKey::Int(10))
        );
        assert!(TableKey::from_value(&Value::Nil).is_err());
        assert!(TableKey::from_value(&Value::Float(f64::NAN)).is_err());
    }

    #[test]
    fn test_field_map_crud_and_backward_shift() {
        let mut map = FieldMap::new();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);

        // Insert keys
        for i in 0..50 {
            let key = StrRef::from(format!("field_{i}"));
            assert!(map.insert(key, Value::Int(i * 10)).is_none());
        }
        assert_eq!(map.len(), 50);

        // Read back
        for i in 0..50 {
            let key_str = format!("field_{i}");
            let key = StrRef::from(key_str.clone());
            assert_eq!(map.get_by_strref(&key), Some(&Value::Int(i * 10)));
            assert_eq!(map.get(&key_str), Some(&Value::Int(i * 10)));
        }

        // Overwrite
        let k0 = StrRef::from("field_0");
        assert_eq!(map.insert(k0.clone(), Value::Int(999)), Some(Value::Int(0)));
        assert_eq!(map.get_by_strref(&k0), Some(&Value::Int(999)));

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
            assert_eq!(map.get_by_strref(&key), Some(&Value::Int(i * 10)));
        }

        // Clear
        map.clear();
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
    }
}
