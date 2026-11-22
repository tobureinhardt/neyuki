//! The raw C ABI, mirroring `include/neyuki_ext.h` (ABI 2).
//!
//! Extensions normally use the safe wrappers in the rest of this crate;
//! these are here for the rare case they do not cover.

use std::ffi::c_void;

pub const ABI_VERSION: u32 = 2;

pub const TAG_NIL: u8 = 0;
pub const TAG_BOOL: u8 = 1;
pub const TAG_INT: u8 = 2;
pub const TAG_FLOAT: u8 = 3;
pub const TAG_STRING: u8 = 4;
pub const TAG_ERROR: u8 = 5;
pub const TAG_HANDLE: u8 = 6;
pub const TAG_MULTI: u8 = 7;
pub const TAG_YIELD: u8 = 8;

pub const KIND_INVALID: u32 = 0;
pub const KIND_TABLE: u32 = 1;
pub const KIND_FUNCTION: u32 = 2;
pub const KIND_BUFFER: u32 = 3;
pub const KIND_BIGINT: u32 = 4;

pub const PERM_READ: u32 = 1;
pub const PERM_WRITE: u32 = 2;
pub const PERM_NET: u32 = 3;
pub const PERM_RUN: u32 = 4;
pub const PERM_ENV: u32 = 5;
pub const PERM_FFI: u32 = 6;

/// `NeyukiExtValue`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RawValue {
    pub tag: u8,
    pub i: i64,
    pub f: f64,
    pub ptr: *const u8,
    pub len: usize,
}

impl RawValue {
    pub const NIL: RawValue = RawValue::with(TAG_NIL, 0, 0.0, std::ptr::null(), 0);

    pub const fn with(tag: u8, i: i64, f: f64, ptr: *const u8, len: usize) -> Self {
        Self {
            tag,
            i,
            f,
            ptr,
            len,
        }
    }

    pub fn bytes(tag: u8, bytes: &[u8]) -> Self {
        Self::with(tag, 0, 0.0, bytes.as_ptr(), bytes.len())
    }
}

pub type ExtFn =
    unsafe extern "C" fn(argc: usize, argv: *const RawValue, user_data: *mut c_void) -> RawValue;
pub type PumpFn = unsafe extern "C" fn(timeout_secs: f64, user_data: *mut c_void) -> RawValue;
pub type WakeFn = unsafe extern "C" fn(user_data: *mut c_void);

/// `NeyukiExtApi`. Append-only: fields after `check_permission` exist only
/// when `abi_version >= 2`.
#[repr(C)]
pub struct Api {
    pub abi_version: u32,
    pub struct_size: u32,
    pub register_fn: unsafe extern "C" fn(
        ctx: *mut c_void,
        name: *const u8,
        name_len: usize,
        func: ExtFn,
        user_data: *mut c_void,
    ) -> bool,
    pub handle_kind: unsafe extern "C" fn(value: RawValue) -> u32,
    pub new_table: unsafe extern "C" fn(array_hint: usize, field_hint: usize) -> RawValue,
    pub table_get: unsafe extern "C" fn(table: RawValue, key: RawValue) -> RawValue,
    pub table_set:
        unsafe extern "C" fn(table: RawValue, key: RawValue, value: RawValue) -> RawValue,
    pub table_len: unsafe extern "C" fn(table: RawValue) -> usize,
    pub table_next: unsafe extern "C" fn(
        table: RawValue,
        key: RawValue,
        out_key: *mut RawValue,
        out_value: *mut RawValue,
    ) -> bool,
    pub call: unsafe extern "C" fn(func: RawValue, argc: usize, argv: *const RawValue) -> RawValue,
    pub new_buffer: unsafe extern "C" fn(data: *const u8, len: usize) -> RawValue,
    pub buffer_data:
        unsafe extern "C" fn(buffer: RawValue, out_ptr: *mut *mut u8, out_len: *mut usize) -> bool,
    pub to_string: unsafe extern "C" fn(value: RawValue) -> RawValue,
    pub retain: unsafe extern "C" fn(handle: RawValue) -> RawValue,
    pub release: unsafe extern "C" fn(handle: RawValue),
    pub check_permission:
        unsafe extern "C" fn(kind: u32, target: *const u8, target_len: usize) -> RawValue,
    // ---- ABI 2 ----
    pub provide_service: unsafe extern "C" fn(
        ctx: *mut c_void,
        name: *const u8,
        name_len: usize,
        service: *const c_void,
    ) -> bool,
    pub lookup_service: unsafe extern "C" fn(name: *const u8, name_len: usize) -> *const c_void,
    pub task_park:
        unsafe extern "C" fn(timeout_secs: f64, pump: PumpFn, user_data: *mut c_void) -> RawValue,
    pub set_waker: unsafe extern "C" fn(wake: WakeFn, user_data: *mut c_void) -> bool,
    pub has_waker: unsafe extern "C" fn() -> bool,
    pub next_due: unsafe extern "C" fn(out_secs: *mut f64) -> bool,
    pub run_tasks: unsafe extern "C" fn(budget_secs: f64),
}

pub type InitFn = unsafe extern "C" fn(api: *const Api, ctx: *mut c_void) -> i32;

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn layout_matches_the_c_header() {
        assert_eq!(size_of::<RawValue>(), 40);
        assert_eq!(offset_of!(RawValue, i), 8);
        assert_eq!(offset_of!(RawValue, ptr), 24);
        assert_eq!(offset_of!(Api, register_fn), 8);
        assert_eq!(
            offset_of!(Api, provide_service),
            8 + 14 * size_of::<usize>()
        );
        assert_eq!(size_of::<Api>(), 8 + 21 * size_of::<usize>());
    }
}
