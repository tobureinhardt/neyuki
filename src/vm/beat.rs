//! Beat bump-allocation tier (`Memory Follows Work` Stage 2).
//!
//! `BeatArena` allocates short-lived tables and strings by bumping into pinned
//! heap chunks (`Box<[u8]>`). Existing chunks never relocate while a Beat is
//! active, so 47-bit `BIT_BEAT` raw pointers inside `Value(u64)` remain stable
//! across allocations until a loop safepoint rewinds `cursor` to `mark`.

use std::cell::RefCell;
use std::ptr::{NonNull, null_mut};
use std::rc::Rc;

use crate::vm::hash::fx_hash_bytes;
use crate::vm::machine::CoroutineState;
use crate::vm::table::compare_keys;
use crate::vm::value::{Value, ValueView, VmTable};

const INITIAL_CHUNK_BYTES: usize = 64 * 1024;
const MAX_CHUNK_BYTES: usize = 1024 * 1024;
/// Soft cap triggering an early Beat spill/flush inside `VM` (4 MiB).
pub const BEAT_SOFT_CAP_BYTES: usize = 4 * 1024 * 1024;
const INLINE_ARR_LIMIT: usize = 32;
const INLINE_FLD_LIMIT: usize = 8;

/// A string allocated inline in the active Beat arena (`len` UTF-8 bytes
/// stored immediately after this header in the bump chunk).
#[repr(C)]
pub struct BeatString {
    pub len: u32,
    pub hash: u64,
    pub alloc_mark: BeatMark,
    pub forwarded: u64,
}

impl BeatString {
    #[inline(always)]
    pub fn as_str(&self) -> &str {
        unsafe {
            let base = (self as *const Self).add(1) as *const u8;
            let bytes = std::slice::from_raw_parts(base, self.len as usize);
            std::str::from_utf8_unchecked(bytes)
        }
    }
}

/// One named string entry stored inside a [`BeatTable`].
#[repr(C)]
#[derive(Clone)]
pub struct BeatField {
    pub key_hash: u64,
    pub key: Value,
    pub val: Value,
}

/// Spill storage attached lazily when a [`BeatTable`] needs >32 array slots,
/// >8 string fields, a `general` map, a `metatable`, or a `co_state`.
pub struct BeatExtra {
    pub table: VmTable,
    pub taken: bool,
}

/// A table allocated inside [`BeatArena`].
///
/// The fast inline/bump state holds array values in `arr_ptr[0..arr_len]` and
/// string fields in `fld_ptr[0..fld_len]`, avoiding `Vec`/`Box`/`Drop`.
/// Rare tables exceeding those bounds attach a [`BeatExtra`] once while
/// keeping the `BeatTable` header address unchanged for the whole Beat.
#[repr(C)]
pub struct BeatTable {
    pub depth: u16,
    pub frozen: bool,
    pub has_old_refs: bool,
    pub in_rem_set: bool,
    pub shape_version: u32,
    pub alloc_mark: BeatMark,
    pub arr_len: u32,
    pub arr_cap: u32,
    pub arr_ptr: *mut Value,
    pub fld_len: u32,
    pub fld_cap: u32,
    pub fld_ptr: *mut BeatField,
    pub forwarded: u64,
    pub extra: *mut BeatExtra,
}

impl BeatTable {
    #[inline(always)]
    pub fn array_slice(&self) -> &[Value] {
        if !self.extra.is_null() {
            unsafe { &(*self.extra).table.array }
        } else if self.arr_len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.arr_ptr, self.arr_len as usize) }
        }
    }

    #[inline(always)]
    pub fn fields_slice(&self) -> &[BeatField] {
        if self.fld_len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.fld_ptr, self.fld_len as usize) }
        }
    }

    #[inline(always)]
    pub fn extra_ref(&self) -> Option<&VmTable> {
        if self.extra.is_null() {
            None
        } else {
            Some(unsafe { &(*self.extra).table })
        }
    }

    #[inline(always)]
    pub fn extra_mut(&mut self) -> Option<&mut VmTable> {
        if self.extra.is_null() {
            None
        } else {
            Some(unsafe { &mut (*self.extra).table })
        }
    }

    #[inline]
    pub fn is_frozen(&self) -> bool {
        if let Some(ext) = self.extra_ref() {
            ext.frozen
        } else {
            self.frozen
        }
    }

    #[inline]
    pub fn set_frozen(&mut self, frozen: bool) {
        self.frozen = frozen;
        if let Some(ext) = self.extra_mut() {
            ext.frozen = frozen;
        }
    }

    #[inline]
    pub fn has_co_state(&self) -> bool {
        self.extra_ref().is_some_and(|e| e.co_state.is_some())
    }

    #[inline]
    pub fn co_state(&self) -> Option<Rc<RefCell<CoroutineState>>> {
        self.extra_ref().and_then(|e| e.co_state.clone())
    }

    #[inline]
    pub fn get_str(&self, key: &str) -> Value {
        if let Some(ext) = self.extra_ref() {
            return ext.get_str(key);
        }
        let hash = fx_hash_bytes(key.as_bytes());
        for f in self.fields_slice() {
            if f.key_hash == hash && f.key.as_str() == Some(key) {
                return f.val.clone();
            }
        }
        Value::NIL
    }

    #[inline]
    pub fn raw_get(&self, key: &Value) -> Value {
        if let Some(ext) = self.extra_ref() {
            return ext.raw_get(key);
        }
        if let Some(f) = key.as_float()
            && f.fract() == 0.0
            && f.abs() < i64::MAX as f64
        {
            return self.raw_get(&Value::int(f as i64));
        }
        match key.view() {
            ValueView::String(s) => {
                let hash = s.hash_u64();
                let target = s.as_str();
                for f in self.fields_slice() {
                    if f.key_hash == hash && f.key.as_str() == Some(target) {
                        return f.val.clone();
                    }
                }
                Value::NIL
            }
            ValueView::Int(i) if i >= 1 && (i as u64) <= self.arr_len as u64 => {
                self.array_slice()[i as usize - 1].clone()
            }
            _ => Value::NIL,
        }
    }

    /// Looks up metamethod `name` if this BeatTable has spilled to `BeatExtra` with a metatable.
    #[inline]
    pub fn metamethod(&self, name: &str) -> Option<Value> {
        self.extra_ref().and_then(|ext| ext.metamethod(name))
    }

    /// Returns sorted non-array keys in standard Neyuki `pairs`/`next` order.
    pub fn sorted_keys(&self) -> Vec<Value> {
        if let Some(ext) = self.extra_ref() {
            return crate::vm::table::sorted_keys(ext);
        }
        let mut keys: Vec<Value> = self
            .fields_slice()
            .iter()
            .filter(|f| !f.val.is_nil())
            .map(|f| f.key.clone())
            .collect();
        keys.sort_by(compare_keys);
        keys
    }

    /// Iteration helper matching [`crate::vm::table::next_entry`] for Beat tables.
    pub fn next_entry(&self, key: &Value) -> Result<Option<(Value, Value)>, String> {
        if let Some(ext) = self.extra_ref() {
            return crate::vm::table::next_entry(ext, key);
        }
        let arr = self.array_slice();
        let array_start = match key.view() {
            ValueView::Nil => Some(0),
            ValueView::Int(i) if i >= 1 && (i as u64) <= arr.len() as u64 => Some(i as usize),
            ValueView::Float(f) if f.is_nan() => {
                return Err("invalid key to 'next'".to_string());
            }
            _ => None,
        };
        if let Some(start) = array_start {
            for (i, v) in arr.iter().enumerate().skip(start) {
                if !v.is_nil() {
                    return Ok(Some((Value::from_usize(i + 1), v.clone())));
                }
            }
            if self.fld_len == 0 {
                return Ok(None);
            }
        }
        let after = if array_start.is_some() {
            None
        } else {
            Some(key)
        };
        let mut best: Option<&BeatField> = None;
        for f in self.fields_slice() {
            if f.val.is_nil() {
                continue;
            }
            if after.is_some_and(|a| compare_keys(&f.key, a) != std::cmp::Ordering::Greater) {
                continue;
            }
            if best.is_none_or(|b| compare_keys(&f.key, &b.key) == std::cmp::Ordering::Less) {
                best = Some(f);
            }
        }
        Ok(best.map(|f| (f.key.clone(), f.val.clone())))
    }

    /// Clones this `BeatTable` into a freshly owned `VmTable` without
    /// modifying or consuming the Beat slot.
    pub fn to_vm_table_cloned(&self) -> VmTable {
        if let Some(ext) = self.extra_ref() {
            return ext.clone();
        }
        let mut out = VmTable::with_capacity(self.arr_len as usize, self.fld_len as usize);
        out.frozen = self.frozen;
        for v in self.array_slice() {
            out.array.push(v.clone());
        }
        for f in self.fields_slice() {
            if !f.val.is_nil()
                && let Some(s) = f.key.as_str_ref()
            {
                out.fields.insert(s.to_str_ref(), f.val.clone());
            }
        }
        out
    }

    /// Converts this `BeatTable` into a freshly owned `VmTable` (preserving any
    /// `BeatExtra` storage directly when already spilled). Values inside are moved
    /// without resetting `Rc` counts; caller must still resolve any Beat child refs.
    pub fn take_as_vm_table(&mut self) -> VmTable {
        if !self.extra.is_null() {
            unsafe {
                let ext = &mut *self.extra;
                ext.taken = true;
                return std::mem::take(&mut ext.table);
            }
        }
        let mut out = VmTable::with_capacity(self.arr_len as usize, self.fld_len as usize);
        out.frozen = self.frozen;
        for v in self.array_slice() {
            out.array.push(v.clone());
        }
        for f in self.fields_slice() {
            if !f.val.is_nil()
                && let Some(s) = f.key.as_str_ref()
            {
                out.fields.insert(s.to_str_ref(), f.val.clone());
            }
        }
        out
    }

    /// Drops any `Rc`-backed `Value`s owned by inline/bump slices if this table
    /// is discarded at Beat rewind without being evacuated.
    pub fn drop_inline_old_refs(&mut self) {
        if !self.extra.is_null() {
            return;
        }
        unsafe {
            let arr = std::slice::from_raw_parts_mut(self.arr_ptr, self.arr_len as usize);
            for slot in arr {
                let v = std::mem::replace(slot, Value::NIL);
                drop(v);
            }
            let flds = std::slice::from_raw_parts_mut(self.fld_ptr, self.fld_len as usize);
            for f in flds {
                let k = std::mem::replace(&mut f.key, Value::NIL);
                let v = std::mem::replace(&mut f.val, Value::NIL);
                drop(k);
                drop(v);
            }
            self.arr_len = 0;
            self.fld_len = 0;
        }
    }
}

/// Nested Beat frame pushed across `call_function` callbacks (`pcall`,
/// `table.sort`, metamethods) so objects allocated at outer depths remain
/// stationary while inner loops rewind.
#[derive(Clone, Copy, Debug)]
pub struct BeatFrame {
    pub floor: BeatMark,
    pub rewind_mark: BeatMark,
    pub depth: u16,
}

/// Packed `(chunk_idx, byte_offset)` cursor in [`BeatArena`].
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BeatMark(pub u64);

impl BeatMark {
    pub const ZERO: BeatMark = BeatMark(0);

    #[inline(always)]
    pub const fn new(chunk_idx: usize, offset: usize) -> Self {
        Self(((chunk_idx as u64) << 32) | (offset as u64))
    }

    #[inline(always)]
    pub const fn chunk_idx(self) -> usize {
        (self.0 >> 32) as usize
    }

    #[inline(always)]
    pub const fn offset(self) -> usize {
        (self.0 & 0xFFFF_FFFF) as usize
    }
}

/// Escape target recorded by write barriers when storing a young Beat value
/// into an older-depth Beat table.
#[derive(Clone, Copy)]
pub enum BeatRemTarget {
    OuterBeatTable { table: *mut BeatTable },
}

struct BeatChunk {
    data: Box<[u8]>,
}

impl BeatChunk {
    fn new(cap: usize) -> Self {
        Self {
            data: vec![0u8; cap].into_boxed_slice(),
        }
    }

    #[inline(always)]
    fn base_ptr(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    #[inline(always)]
    fn cap(&self) -> usize {
        self.data.len()
    }
}

/// Non-moving bump arena backing active Beat allocations.
pub struct BeatArena {
    chunks: Vec<BeatChunk>,
    cur_chunk: usize,
    cur_off: usize,
    pub depth: u16,
    pub floor: BeatMark,
    pub rewind_mark: BeatMark,
    frames: Vec<BeatFrame>,
    pub rem_set: Vec<BeatRemTarget>,
    pub extras: Vec<(BeatMark, NonNull<BeatExtra>)>,
    pub old_ref_holders: Vec<(BeatMark, *mut BeatTable)>,
    pub finalizers: Vec<(BeatMark, *mut BeatTable)>,
    pub active_bytes: usize,
}

impl Default for BeatArena {
    fn default() -> Self {
        Self::new()
    }
}

impl BeatArena {
    pub fn new() -> Self {
        Self {
            chunks: Vec::new(),
            cur_chunk: 0,
            cur_off: 0,
            depth: 1,
            floor: BeatMark::ZERO,
            rewind_mark: BeatMark::ZERO,
            frames: Vec::with_capacity(16),
            rem_set: Vec::new(),
            extras: Vec::new(),
            old_ref_holders: Vec::new(),
            finalizers: Vec::new(),
            active_bytes: 0,
        }
    }

    /// Packed cursor for O(1) equality check `arena.cursor() == mark`.
    #[inline(always)]
    pub fn cursor(&self) -> BeatMark {
        BeatMark::new(self.cur_chunk, self.cur_off)
    }

    #[inline(always)]
    pub fn needs_spill(&self) -> bool {
        self.active_bytes >= BEAT_SOFT_CAP_BYTES
    }

    #[inline(always)]
    pub fn enter_nested(&mut self) -> BeatMark {
        let prev_floor = self.floor;
        self.frames.push(BeatFrame {
            floor: prev_floor,
            rewind_mark: self.rewind_mark,
            depth: self.depth,
        });
        let cur = self.cursor();
        self.floor = cur;
        self.rewind_mark = cur;
        self.depth = self.depth.saturating_add(1);
        cur
    }

    #[inline(always)]
    pub fn exit_nested(&mut self) {
        if let Some(prev) = self.frames.pop() {
            self.floor = prev.floor;
            self.rewind_mark = prev.rewind_mark;
            self.depth = prev.depth;
        }
    }

    fn ensure_chunk(&mut self, bytes: usize, align: usize) {
        if self.chunks.is_empty() {
            let cap = INITIAL_CHUNK_BYTES.max(bytes + align);
            self.chunks.push(BeatChunk::new(cap));
            self.cur_chunk = 0;
            self.cur_off = 0;
            return;
        }
        let aligned = (self.cur_off + (align - 1)) & !(align - 1);
        if aligned + bytes <= self.chunks[self.cur_chunk].cap() {
            return;
        }
        if self.cur_chunk + 1 < self.chunks.len()
            && self.chunks[self.cur_chunk + 1].cap() >= bytes + align
        {
            self.cur_chunk += 1;
            self.cur_off = 0;
            return;
        }
        let next_cap = (self.chunks[self.cur_chunk].cap() * 2)
            .min(MAX_CHUNK_BYTES)
            .max(bytes + align);
        self.chunks.push(BeatChunk::new(next_cap));
        self.cur_chunk = self.chunks.len() - 1;
        self.cur_off = 0;
    }

    #[inline]
    pub fn alloc_raw(&mut self, bytes: usize, align: usize) -> *mut u8 {
        debug_assert!(align.is_power_of_two());
        let aligned = (self.cur_off + (align - 1)) & !(align - 1);
        if self.chunks.is_empty() || aligned + bytes > self.chunks[self.cur_chunk].cap() {
            self.ensure_chunk(bytes, align);
        }
        let aligned = (self.cur_off + (align - 1)) & !(align - 1);
        let chunk = &mut self.chunks[self.cur_chunk];
        let ptr = unsafe { chunk.base_ptr().add(aligned) };
        self.cur_off = aligned + bytes;
        self.active_bytes += bytes;
        ptr
    }

    /// Allocates a UTF-8 [`BeatString`] directly inside the Beat bump arena.
    pub fn alloc_string(&mut self, s: &str) -> *mut BeatString {
        let alloc_mark = self.cursor();
        let total = std::mem::size_of::<BeatString>() + s.len();
        let raw = self.alloc_raw(total, std::mem::align_of::<BeatString>());
        let hdr = raw as *mut BeatString;
        let hash = fx_hash_bytes(s.as_bytes());
        unsafe {
            std::ptr::write(
                hdr,
                BeatString {
                    len: s.len() as u32,
                    hash,
                    alloc_mark,
                    forwarded: 0,
                },
            );
            let dst = hdr.add(1) as *mut u8;
            std::ptr::copy_nonoverlapping(s.as_ptr(), dst, s.len());
        }
        hdr
    }

    /// Allocates a [`BeatTable`] with compact inline bump slices (`arr_cap`, `fld_cap`).
    pub fn alloc_table(&mut self, array_cap: usize, fields_cap: usize) -> *mut BeatTable {
        let alloc_mark = self.cursor();
        let arr_cap = array_cap.clamp(4, INLINE_ARR_LIMIT);
        let fld_cap = fields_cap.clamp(4, INLINE_FLD_LIMIT);
        let hdr_size = std::mem::size_of::<BeatTable>();
        let arr_size = arr_cap * std::mem::size_of::<Value>();
        let fld_size = fld_cap * std::mem::size_of::<BeatField>();
        let raw = self.alloc_raw(hdr_size + arr_size + fld_size, 8);
        let hdr = raw as *mut BeatTable;
        unsafe {
            let arr_ptr = raw.add(hdr_size) as *mut Value;
            let fld_ptr = raw.add(hdr_size + arr_size) as *mut BeatField;
            std::ptr::write(
                hdr,
                BeatTable {
                    depth: self.depth,
                    frozen: false,
                    has_old_refs: false,
                    in_rem_set: false,
                    shape_version: 0,
                    alloc_mark,
                    arr_len: 0,
                    arr_cap: arr_cap as u32,
                    arr_ptr,
                    fld_len: 0,
                    fld_cap: fld_cap as u32,
                    fld_ptr,
                    forwarded: 0,
                    extra: null_mut(),
                },
            );
            hdr
        }
    }

    /// Ensures `tbl` has a [`BeatExtra`] spill block registered under `tbl.alloc_mark`,
    /// moving any existing inline array/fields once into `BeatExtra::table`.
    pub fn ensure_extra(&mut self, tbl: &mut BeatTable) -> &mut VmTable {
        if !tbl.extra.is_null() {
            return unsafe { &mut (*tbl.extra).table };
        }
        let vm_tbl = tbl.take_as_vm_table();
        let boxed = Box::new(BeatExtra {
            table: vm_tbl,
            taken: false,
        });
        let raw = NonNull::from(Box::leak(boxed));
        self.extras.push((tbl.alloc_mark, raw));
        tbl.arr_len = 0;
        tbl.arr_cap = 0;
        tbl.fld_len = 0;
        tbl.fld_cap = 0;
        tbl.extra = raw.as_ptr();
        unsafe { &mut (*tbl.extra).table }
    }

    /// # Safety
    ///
    /// `tbl` must be a valid, live `*mut BeatTable` allocated from `self`.
    #[inline]
    pub unsafe fn mark_old_ref_holder(&mut self, tbl: *mut BeatTable) {
        unsafe {
            if !(*tbl).has_old_refs {
                (*tbl).has_old_refs = true;
                self.old_ref_holders.push(((*tbl).alloc_mark, tbl));
            }
        }
    }

    /// Raw set on a `BeatTable`, upgrading slices in Beat or spilling to `BeatExtra`
    /// if capacities or key kinds exceed inline bump storage.
    ///
    /// # Safety
    ///
    /// `tbl_ptr` must point to a valid, live `BeatTable` allocated from `self`.
    pub unsafe fn table_raw_set(
        &mut self,
        tbl_ptr: *mut BeatTable,
        key: Value,
        val: Value,
    ) -> Result<(), String> {
        let tbl = unsafe { &mut *tbl_ptr };
        if tbl.is_frozen() {
            return Err("attempted to mutate a frozen table".to_string());
        }
        let key_old = key.bits() >= crate::vm::value::TAG_RC_START && !key.is_beat();
        let val_old = val.bits() >= crate::vm::value::TAG_RC_START && !val.is_beat();
        if key_old || val_old {
            unsafe { self.mark_old_ref_holder(tbl_ptr) };
        }
        if tbl.alloc_mark < self.rewind_mark && !tbl.in_rem_set {
            let young_key = key.beat_alloc_mark().is_some_and(|m| m >= self.rewind_mark);
            let young_val = val.beat_alloc_mark().is_some_and(|m| m >= self.rewind_mark);
            if young_key || young_val {
                tbl.in_rem_set = true;
                self.rem_set
                    .push(BeatRemTarget::OuterBeatTable { table: tbl_ptr });
            }
        }

        if !tbl.extra.is_null() {
            return unsafe { (*tbl.extra).table.raw_set(key, val) };
        }

        if let Some(s) = key.as_str_ref() {
            let hash = s.hash_u64();
            let target = s.as_str();
            let flds = unsafe { std::slice::from_raw_parts_mut(tbl.fld_ptr, tbl.fld_len as usize) };
            for i in 0..flds.len() {
                if flds[i].key_hash == hash && flds[i].key.as_str() == Some(target) {
                    if val.is_nil() {
                        tbl.shape_version = tbl.shape_version.wrapping_add(1);
                        let last = flds.len() - 1;
                        if i != last {
                            flds.swap(i, last);
                        }
                        let old_k = std::mem::replace(&mut flds[last].key, Value::NIL);
                        let old_v = std::mem::replace(&mut flds[last].val, Value::NIL);
                        drop(old_k);
                        drop(old_v);
                        tbl.fld_len -= 1;
                    } else {
                        flds[i].val = val;
                    }
                    return Ok(());
                }
            }
            if val.is_nil() {
                return Ok(());
            }
            if (tbl.fld_len as usize) < tbl.fld_cap as usize {
                let idx = tbl.fld_len as usize;
                unsafe {
                    std::ptr::write(
                        tbl.fld_ptr.add(idx),
                        BeatField {
                            key_hash: hash,
                            key,
                            val,
                        },
                    );
                }
                tbl.fld_len += 1;
                tbl.shape_version = tbl.shape_version.wrapping_add(1);
                return Ok(());
            }
            return self.ensure_extra(tbl).raw_set(key, val);
        }

        if let Some(i) = key.as_int()
            && i >= 1
        {
            let idx_u = (i - 1) as usize;
            let len = tbl.arr_len as usize;
            if idx_u < len {
                unsafe {
                    *tbl.arr_ptr.add(idx_u) = val;
                }
                if idx_u + 1 == len {
                    while tbl.arr_len > 0 {
                        let last = unsafe { &*tbl.arr_ptr.add(tbl.arr_len as usize - 1) };
                        if last.is_nil() {
                            tbl.arr_len -= 1;
                        } else {
                            break;
                        }
                    }
                }
                return Ok(());
            }
            if idx_u == len && !val.is_nil() {
                if len < tbl.arr_cap as usize {
                    unsafe {
                        std::ptr::write(tbl.arr_ptr.add(len), val);
                    }
                    tbl.arr_len += 1;
                    return Ok(());
                }
                return self.ensure_extra(tbl).raw_set(key, val);
            } else if idx_u == len && val.is_nil() {
                return Ok(());
            }
        }

        if let Some(f) = key.as_float()
            && f.fract() == 0.0
            && f >= 1.0
            && f < i64::MAX as f64
        {
            return unsafe { self.table_raw_set(tbl_ptr, Value::int(f as i64), val) };
        }

        self.ensure_extra(tbl).raw_set(key, val)
    }

    /// Rewinds bump cursor and frees any `BeatExtra` / `old_ref_holders`
    /// belonging to Beat allocations at or above `mark` (respecting `self.floor`).
    /// Outer Beat tables modified inside inner loops retain their resources.
    pub fn rewind(&mut self, mark: BeatMark) {
        let target = if mark < self.floor { self.floor } else { mark };
        if !self.extras.is_empty() {
            self.extras.retain(|&(m, raw)| {
                if m >= target {
                    unsafe {
                        drop(Box::from_raw(raw.as_ptr()));
                    }
                    false
                } else {
                    true
                }
            });
        }
        if !self.old_ref_holders.is_empty() {
            self.old_ref_holders.retain(|&(m, ptr)| {
                if m >= target {
                    unsafe {
                        if (*ptr).forwarded == 0 {
                            (*ptr).drop_inline_old_refs();
                        }
                    }
                    false
                } else {
                    true
                }
            });
        }
        if !self.finalizers.is_empty() {
            self.finalizers.retain(|&(m, _)| m < target);
        }
        self.cur_chunk = target.chunk_idx();
        self.cur_off = target.offset();
        if self.cur_chunk == 0 && self.cur_off == 0 {
            self.active_bytes = 0;
        }
    }
}

impl Drop for BeatArena {
    fn drop(&mut self) {
        self.rewind(BeatMark::ZERO);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beat_arena_alloc_rewind_keeps_headers_stationary() {
        let mut beat = BeatArena::new();
        let t_outer = beat.alloc_table(4, 4);
        unsafe { beat.table_raw_set(t_outer, Value::str("k"), Value::int(7)) }.unwrap();
        let floor = beat.enter_nested();
        assert_eq!(floor, beat.cursor());

        for i in 0..1000 {
            let mark = beat.cursor();
            let tmp = beat.alloc_table(4, 4);
            unsafe { beat.table_raw_set(tmp, Value::int(1), Value::int(i)) }.unwrap();
            unsafe { beat.table_raw_set(tmp, Value::str("inner_old"), Value::str("tmp")) }.unwrap();
            assert_eq!(unsafe { &*tmp }.raw_get(&Value::int(1)), Value::int(i));
            beat.rewind(mark);
        }
        beat.exit_nested();
        assert_eq!(
            unsafe { &*t_outer }.raw_get(&Value::str("k")),
            Value::int(7)
        );
    }

    #[test]
    fn outer_beat_table_mutated_inside_inner_loop_survives_rewind() {
        let mut beat = BeatArena::new();
        let t_outer = beat.alloc_table(4, 4);
        let loop_mark = beat.cursor();

        for i in 1..=50 {
            let iter_mark = beat.cursor();
            let tmp = beat.alloc_table(4, 4);
            unsafe {
                beat.table_raw_set(tmp, Value::int(1), Value::int(i))
                    .unwrap();
                beat.table_raw_set(t_outer, Value::int(i), Value::int(i * 3))
                    .unwrap();
                beat.table_raw_set(t_outer, Value::str("old_tag"), Value::str("alive"))
                    .unwrap();
            }
            beat.rewind(iter_mark);
        }

        assert!(beat.cursor() >= loop_mark);
        assert_eq!(
            unsafe { &*t_outer }.raw_get(&Value::int(50)),
            Value::int(150)
        );
        assert_eq!(
            unsafe { &*t_outer }
                .raw_get(&Value::str("old_tag"))
                .as_str(),
            Some("alive")
        );
    }

    #[test]
    fn beat_table_spills_to_extra_and_rewinds_cleanly() {
        let mut beat = BeatArena::new();
        let mark = beat.cursor();
        let t = beat.alloc_table(4, 4);
        for i in 1..=50 {
            unsafe { beat.table_raw_set(t, Value::int(i), Value::int(i * 10)) }.unwrap();
        }
        let tbl = unsafe { &*t };
        assert!(!tbl.extra.is_null());
        assert_eq!(tbl.raw_get(&Value::int(50)), Value::int(500));
        assert_eq!(beat.extras.len(), 1);

        beat.rewind(mark);
        assert!(beat.extras.is_empty());
        assert_eq!(beat.cursor(), BeatMark::ZERO);
    }

    #[test]
    fn beat_string_stores_utf8_inline_with_hash() {
        let mut beat = BeatArena::new();
        let s = beat.alloc_string("xin chao");
        unsafe {
            assert_eq!((*s).as_str(), "xin chao");
            assert_eq!((*s).hash, fx_hash_bytes(b"xin chao"));
        }
    }
}
