//! The Neyuki native extension ABI (version 1).
//!
//! An extension is a shared library exporting
//! `int32_t neyuki_extension_init(const NeyukiExtApi *api, void *ctx)`. It
//! keeps `api`, calls `api->register_fn` for each function it provides and
//! returns 0. `include/neyuki_ext.h` is the C side of this file; keep the
//! two in step.
//!
//! Values cross the boundary as `NeyukiExtValue`. Scalars and strings are
//! copied. Tables, functions, buffers and big integers cross as *handles*:
//!
//! - A **local** handle is valid until the extension call that received or
//!   created it returns (like a JNI local reference). Using one afterwards is
//!   reported as an error, not undefined behaviour: each handle carries the
//!   serial of the call that made it.
//! - A **persistent** handle (`api->retain`) stays valid until
//!   `api->release`, so an extension can keep a script callback or table
//!   between calls. It keeps the value alive (the cycle collector spares
//!   values held from outside the VM).
//!
//! ABI 2 appends services (one extension handing others a vtable, as
//! `@nyk/ui` does for `@nyk/gpu` and `@nyk/webview`) and the task
//! scheduler (parking a task on the extension's own event pump, a waker,
//! the next due time and running due tasks). `NeyukiExtApi` is append-only,
//! so ABI 1 extensions keep loading.
//!
//! Host API functions may only be called on the thread running the script,
//! while an extension function (or `neyuki_extension_init`) is running.
//! Outside that they return an `ERROR` value (or false / 0).
//!
//! Ownership: strings and `MULTI` arrays the extension *returns* stay owned
//! by the extension; the host copies them before the call returns. Strings
//! and arrays the *host* hands out (arguments, `to_string`, `table_get`,
//! `call` results, error messages) stay valid until the extension call that
//! received them returns.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

use libloading::Library;

use super::{LIBRARIES, LoadedLibrary, next_lib_id};
use crate::vm::buffer::VmBuffer;
use crate::vm::libs::task;
use crate::vm::machine::VM;
use crate::vm::value::{Value, VmTable};

pub const NEYUKI_EXT_ABI_VERSION: u32 = 2;
/// The oldest ABI this runtime still loads (`NeyukiExtApi` is append-only).
pub const NEYUKI_EXT_ABI_MIN: u32 = 1;

pub const EXT_TAG_NIL: u8 = 0;
pub const EXT_TAG_BOOL: u8 = 1;
pub const EXT_TAG_INT: u8 = 2;
pub const EXT_TAG_FLOAT: u8 = 3;
pub const EXT_TAG_STRING: u8 = 4;
pub const EXT_TAG_ERROR: u8 = 5;
/// A host value (table, function, buffer, big integer); `i` holds the handle.
pub const EXT_TAG_HANDLE: u8 = 6;
/// Several values: `ptr` points at `len` `NeyukiExtValue`s. Only valid as a
/// function's return value (multiple returns) and as `api->call`'s result.
pub const EXT_TAG_MULTI: u8 = 7;
/// Returned by an extension function right after `api->task_park` parked
/// the running task: the host yields it. Valid only as that return value.
pub const EXT_TAG_YIELD: u8 = 8;

/// What a handle refers to (`api->handle_kind`). 0 means not a live handle.
pub const EXT_KIND_INVALID: u32 = 0;
pub const EXT_KIND_TABLE: u32 = 1;
pub const EXT_KIND_FUNCTION: u32 = 2;
pub const EXT_KIND_BUFFER: u32 = 3;
pub const EXT_KIND_BIGINT: u32 = 4;

/// Permission kinds for `api->check_permission`.
pub const EXT_PERM_READ: u32 = 1;
pub const EXT_PERM_WRITE: u32 = 2;
pub const EXT_PERM_NET: u32 = 3;
pub const EXT_PERM_RUN: u32 = 4;
pub const EXT_PERM_ENV: u32 = 5;
pub const EXT_PERM_FFI: u32 = 6;

/// Set in a handle id for persistent handles.
const PERSISTENT_BIT: i64 = 1 << 62;

/// Stable C-ABI value exchanged between Neyuki and native extensions.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct NeyukiExtValue {
    pub tag: u8,
    pub i: i64,
    pub f: f64,
    pub ptr: *const u8,
    pub len: usize,
}

impl NeyukiExtValue {
    const fn with(tag: u8, i: i64, f: f64, ptr: *const u8, len: usize) -> Self {
        Self {
            tag,
            i,
            f,
            ptr,
            len,
        }
    }

    pub const fn nil() -> Self {
        Self::with(EXT_TAG_NIL, 0, 0.0, std::ptr::null(), 0)
    }

    pub fn from_bool(b: bool) -> Self {
        Self::with(EXT_TAG_BOOL, i64::from(b), 0.0, std::ptr::null(), 0)
    }

    pub const fn from_int(i: i64) -> Self {
        Self::with(EXT_TAG_INT, i, 0.0, std::ptr::null(), 0)
    }

    pub const fn from_float(f: f64) -> Self {
        Self::with(EXT_TAG_FLOAT, 0, f, std::ptr::null(), 0)
    }

    /// A string pointing at `bytes`; the bytes must outlive every use.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self::with(EXT_TAG_STRING, 0, 0.0, bytes.as_ptr(), bytes.len())
    }

    /// An error pointing at `msg`; the bytes must outlive every use.
    pub fn error(msg: &[u8]) -> Self {
        Self::with(EXT_TAG_ERROR, 0, 0.0, msg.as_ptr(), msg.len())
    }

    const fn handle(id: i64) -> Self {
        Self::with(EXT_TAG_HANDLE, id, 0.0, std::ptr::null(), 0)
    }
}

pub type NeyukiExtFn = unsafe extern "C" fn(
    argc: usize,
    argv: *const NeyukiExtValue,
    user_data: *mut c_void,
) -> NeyukiExtValue;

pub type NeyukiExtRegisterFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    name_ptr: *const u8,
    name_len: usize,
    func: NeyukiExtFn,
    user_data: *mut c_void,
) -> bool;

/// An extension's event pump (`api->task_park`): waits up to `timeout_secs`
/// for events and returns them as an array table handle (or `ERROR`). The
/// host runs it inside a frame of its own, so the api can be used.
pub type NeyukiExtPumpFn =
    unsafe extern "C" fn(timeout_secs: f64, user_data: *mut c_void) -> NeyukiExtValue;

/// Wakes an extension's blocked event loop (`api->set_waker`). Called from
/// worker threads, so it must be thread-safe and must not use the api.
pub type NeyukiExtWakeFn = unsafe extern "C" fn(user_data: *mut c_void);

/// The host API handed to `neyuki_extension_init`.
///
/// The layout is append-only: later ABI revisions add fields at the end and
/// raise `struct_size`, so an extension can test `struct_size` before using
/// a newer field.
#[repr(C)]
pub struct NeyukiExtApi {
    pub abi_version: u32,
    pub struct_size: u32,
    pub register_fn: NeyukiExtRegisterFn,
    pub handle_kind: unsafe extern "C" fn(value: NeyukiExtValue) -> u32,
    pub new_table: unsafe extern "C" fn(array_hint: usize, field_hint: usize) -> NeyukiExtValue,
    pub table_get:
        unsafe extern "C" fn(table: NeyukiExtValue, key: NeyukiExtValue) -> NeyukiExtValue,
    pub table_set: unsafe extern "C" fn(
        table: NeyukiExtValue,
        key: NeyukiExtValue,
        value: NeyukiExtValue,
    ) -> NeyukiExtValue,
    pub table_len: unsafe extern "C" fn(table: NeyukiExtValue) -> usize,
    pub table_next: unsafe extern "C" fn(
        table: NeyukiExtValue,
        key: NeyukiExtValue,
        out_key: *mut NeyukiExtValue,
        out_value: *mut NeyukiExtValue,
    ) -> bool,
    pub call: unsafe extern "C" fn(
        func: NeyukiExtValue,
        argc: usize,
        argv: *const NeyukiExtValue,
    ) -> NeyukiExtValue,
    pub new_buffer: unsafe extern "C" fn(data: *const u8, len: usize) -> NeyukiExtValue,
    pub buffer_data: unsafe extern "C" fn(
        buffer: NeyukiExtValue,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
    ) -> bool,
    pub to_string: unsafe extern "C" fn(value: NeyukiExtValue) -> NeyukiExtValue,
    pub retain: unsafe extern "C" fn(handle: NeyukiExtValue) -> NeyukiExtValue,
    pub release: unsafe extern "C" fn(handle: NeyukiExtValue),
    pub check_permission:
        unsafe extern "C" fn(kind: u32, target: *const u8, target_len: usize) -> NeyukiExtValue,
    // ---- ABI 2 ----
    pub provide_service: unsafe extern "C" fn(
        ctx: *mut c_void,
        name_ptr: *const u8,
        name_len: usize,
        service: *const c_void,
    ) -> bool,
    pub lookup_service: unsafe extern "C" fn(name_ptr: *const u8, name_len: usize) -> *const c_void,
    pub task_park: unsafe extern "C" fn(
        timeout_secs: f64,
        pump: NeyukiExtPumpFn,
        user_data: *mut c_void,
    ) -> NeyukiExtValue,
    pub set_waker: unsafe extern "C" fn(wake: NeyukiExtWakeFn, user_data: *mut c_void) -> bool,
    pub has_waker: unsafe extern "C" fn() -> bool,
    pub next_due: unsafe extern "C" fn(out_secs: *mut f64) -> bool,
    pub run_tasks: unsafe extern "C" fn(budget_secs: f64),
}

pub type NeyukiExtInitFn = unsafe extern "C" fn(api: *const NeyukiExtApi, ctx: *mut c_void) -> i32;

static API: NeyukiExtApi = NeyukiExtApi {
    abi_version: NEYUKI_EXT_ABI_VERSION,
    struct_size: std::mem::size_of::<NeyukiExtApi>() as u32,
    register_fn: api_register,
    handle_kind: api_handle_kind,
    new_table: api_new_table,
    table_get: api_table_get,
    table_set: api_table_set,
    table_len: api_table_len,
    table_next: api_table_next,
    call: api_call,
    new_buffer: api_new_buffer,
    buffer_data: api_buffer_data,
    to_string: api_to_string,
    retain: api_retain,
    release: api_release,
    check_permission: api_check_permission,
    provide_service: api_provide_service,
    lookup_service: api_lookup_service,
    task_park: api_task_park,
    set_waker: api_set_waker,
    has_waker: api_has_waker,
    next_due: api_next_due,
    run_tasks: api_run_tasks,
};

// ---------------------------------------------------------------------------
// Call frames, handles and host-owned scratch memory
// ---------------------------------------------------------------------------

struct Frame {
    vm: *mut VM,
    serial: u32,
    handles_base: usize,
    keep_base: usize,
    scratch_base: usize,
}

#[derive(Default)]
struct ExtState {
    frames: Vec<Frame>,
    /// Local handles of every active frame: `(serial of the owning frame, value)`.
    handles: Vec<(u32, Value)>,
    /// Values whose string bytes were handed out, kept alive for the frame.
    keep: Vec<Value>,
    /// Host-owned strings and `MULTI` arrays handed out during the frame.
    scratch: Vec<Scratch>,
    persistent: HashMap<i64, Value>,
    /// Libraries closed during an extension call (see `unload_when_idle`).
    closed_while_running: Vec<LoadedLibrary>,
    next_persistent: i64,
    next_serial: u32,
    /// Set by `api->task_park` for the frame with this serial; taken by
    /// `call_ext` when the function returns `YIELD`.
    parked: Option<(u32, Duration, NeyukiExtPumpFn, *mut c_void)>,
}

/// A service vtable an extension provided, and the library that owns it.
#[derive(Clone, Copy)]
struct Service {
    lib_id: u64,
    ptr: *const c_void,
}

#[allow(dead_code)] // the boxes are only held to keep their memory alive
enum Scratch {
    Bytes(Box<[u8]>),
    Values(Box<[NeyukiExtValue]>),
}

thread_local! {
    static STATE: RefCell<ExtState> = RefCell::new(ExtState::default());
    static SERVICES: RefCell<HashMap<String, Service>> = RefCell::new(HashMap::new());
}

/// Forgets the services of a library that is being closed, so lookups
/// fail instead of handing out pointers into unloaded code.
pub(super) fn forget_services(lib_id: u64) {
    SERVICES.with(|s| s.borrow_mut().retain(|_, svc| svc.lib_id != lib_id));
}

/// Pops its frame (and everything the frame handed out) when dropped, so an
/// early return or error cannot leak handles.
struct FrameGuard;

impl FrameGuard {
    fn push(vm: &mut VM) -> Self {
        STATE.with(|s| {
            let mut s = s.borrow_mut();
            s.next_serial = s.next_serial.wrapping_add(1) & 0x3fff_ffff;
            if s.next_serial == 0 {
                s.next_serial = 1;
            }
            let frame = Frame {
                vm: vm as *mut VM,
                serial: s.next_serial,
                handles_base: s.handles.len(),
                keep_base: s.keep.len(),
                scratch_base: s.scratch.len(),
            };
            s.frames.push(frame);
        });
        FrameGuard
    }
}

impl Drop for FrameGuard {
    fn drop(&mut self) {
        // Taken out first and dropped after the borrow ends: dropping a
        // table can run arbitrary Drop code, which must not find STATE
        // borrowed.
        let (handles, keep, scratch, unload) = STATE.with(|s| {
            let mut s = s.borrow_mut();
            let frame = s.frames.pop().expect("extension frame underflow");
            let handles = s.handles.split_off(frame.handles_base);
            let keep = s.keep.split_off(frame.keep_base);
            let scratch = s.scratch.split_off(frame.scratch_base);
            let unload = if s.frames.is_empty() {
                std::mem::take(&mut s.closed_while_running)
            } else {
                Vec::new()
            };
            (handles, keep, scratch, unload)
        });
        drop((handles, keep, scratch));
        // Only now is no extension code left on the stack.
        drop(unload);
    }
}

/// Unloads a closed library, or, while any extension call is running,
/// keeps it loaded until the outermost one returns: a script callback may
/// close the very extension that called it, whose code is still running.
pub(super) fn unload_when_idle(lib: LoadedLibrary) {
    let lib = STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.frames.is_empty() {
            Some(lib)
        } else {
            s.closed_while_running.push(lib);
            None
        }
    });
    drop(lib);
}

fn in_frame<R>(f: impl FnOnce(&mut ExtState) -> Result<R, String>) -> Result<R, String> {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.frames.is_empty() {
            return Err("Neyuki host API called outside an extension call".to_string());
        }
        f(&mut s)
    })
}

impl ExtState {
    fn serial(&self) -> u32 {
        self.frames.last().map_or(0, |f| f.serial)
    }

    fn new_handle(&mut self, value: Value) -> NeyukiExtValue {
        let serial = self.serial();
        let index = self.handles.len() as i64;
        self.handles.push((serial, value));
        NeyukiExtValue::handle((i64::from(serial) << 32) | (index + 1))
    }

    fn lookup(&self, id: i64) -> Result<Value, String> {
        if id & PERSISTENT_BIT != 0 {
            return self.persistent.get(&id).cloned().ok_or_else(|| {
                "invalid extension handle: persistent handle was released".to_string()
            });
        }
        let serial = (id >> 32) as u32;
        let index = (id & 0xffff_ffff) - 1;
        match usize::try_from(index).ok().and_then(|i| self.handles.get(i)) {
            Some((s, v)) if *s == serial => Ok(v.clone()),
            _ => Err(
                "invalid extension handle: local handles are only valid during the call that made them (use api->retain to keep one)"
                    .to_string(),
            ),
        }
    }

    fn scratch_bytes(&mut self, bytes: Vec<u8>) -> (*const u8, usize) {
        let boxed = bytes.into_boxed_slice();
        let out = (boxed.as_ptr(), boxed.len());
        self.scratch.push(Scratch::Bytes(boxed));
        out
    }

    fn error(&mut self, msg: impl Into<String>) -> NeyukiExtValue {
        let (ptr, len) = self.scratch_bytes(msg.into().into_bytes());
        NeyukiExtValue::with(EXT_TAG_ERROR, 0, 0.0, ptr, len)
    }

    /// Converts a host value for the extension.
    fn export(&mut self, value: Value) -> NeyukiExtValue {
        match value {
            Value::Nil => NeyukiExtValue::nil(),
            Value::Bool(b) => NeyukiExtValue::from_bool(b),
            Value::Int(i) => NeyukiExtValue::from_int(i),
            Value::Float(f) => NeyukiExtValue::from_float(f),
            Value::String(ref s) => {
                let bytes = s.as_bytes();
                let out = NeyukiExtValue::from_bytes(bytes);
                // The string's heap bytes do not move while a clone of the
                // value keeps them alive.
                self.keep.push(value);
                out
            }
            other => self.new_handle(other),
        }
    }

    /// Converts a value from the extension. `MULTI` is refused here; only a
    /// function's top-level return may carry several values.
    fn import(&self, ext: &NeyukiExtValue) -> Result<Value, String> {
        match ext.tag {
            EXT_TAG_NIL => Ok(Value::Nil),
            EXT_TAG_BOOL => Ok(Value::Bool(ext.i != 0)),
            EXT_TAG_INT => Ok(Value::Int(ext.i)),
            EXT_TAG_FLOAT => Ok(Value::Float(ext.f)),
            EXT_TAG_STRING => Ok(Value::string(unsafe { lossy(ext.ptr, ext.len) })),
            EXT_TAG_HANDLE => self.lookup(ext.i),
            EXT_TAG_ERROR => Err(error_message(ext)),
            EXT_TAG_MULTI => Err("a MULTI value is only allowed as a return value".to_string()),
            EXT_TAG_YIELD => {
                Err("YIELD is only allowed as the return value after task_park".to_string())
            }
            other => Err(format!("invalid extension value tag {}", other)),
        }
    }

    fn import_table(&self, ext: &NeyukiExtValue) -> Result<Rc<RefCell<VmTable>>, String> {
        match self.import(ext)? {
            Value::Table(t) => Ok(t),
            other => Err(format!(
                "expected a table handle, got {}",
                other.type_name()
            )),
        }
    }

    /// The VM of the innermost active extension call. Never null: a frame
    /// is only pushed from a live `&mut VM` (`FrameGuard::push`).
    fn vm(&self) -> Result<NonNull<VM>, String> {
        self.frames
            .last()
            .and_then(|f| NonNull::new(f.vm))
            .ok_or_else(|| "Neyuki host API called outside an extension call".to_string())
    }
}

/// Reads `len` bytes at `ptr` as lossy UTF-8 (empty for a null pointer).
unsafe fn lossy(ptr: *const u8, len: usize) -> String {
    if ptr.is_null() || len == 0 {
        return String::new();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

fn error_message(ext: &NeyukiExtValue) -> String {
    if ext.ptr.is_null() {
        "extension error".to_string()
    } else {
        unsafe { lossy(ext.ptr, ext.len) }
    }
}

/// Runs a host API body, turning an error into an `ERROR` value.
fn api_value(f: impl FnOnce(&mut ExtState) -> Result<NeyukiExtValue, String>) -> NeyukiExtValue {
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.frames.is_empty() {
            // No frame to own the message: a static string.
            return NeyukiExtValue::error(b"Neyuki host API called outside an extension call");
        }
        match f(&mut s) {
            Ok(v) => v,
            Err(msg) => s.error(msg),
        }
    })
}

// ---------------------------------------------------------------------------
// Host API functions
// ---------------------------------------------------------------------------

struct RegisterContext {
    exports: Vec<(String, NeyukiExtFn, *mut c_void)>,
    services: Vec<(String, *const c_void)>,
}

unsafe extern "C" fn api_register(
    ctx: *mut c_void,
    name_ptr: *const u8,
    name_len: usize,
    func: NeyukiExtFn,
    user_data: *mut c_void,
) -> bool {
    if ctx.is_null() || name_ptr.is_null() {
        return false;
    }
    let slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
    let Ok(name) = std::str::from_utf8(slice) else {
        return false;
    };
    if name.is_empty() || name == "close" {
        return false;
    }
    let ctx = unsafe { &mut *(ctx as *mut RegisterContext) };
    if ctx.exports.iter().any(|(n, _, _)| n == name) {
        return false;
    }
    ctx.exports.push((name.to_string(), func, user_data));
    true
}

unsafe extern "C" fn api_handle_kind(value: NeyukiExtValue) -> u32 {
    if value.tag != EXT_TAG_HANDLE {
        return EXT_KIND_INVALID;
    }
    in_frame(|s| s.lookup(value.i)).map_or(EXT_KIND_INVALID, |v| match v {
        Value::Table(_) => EXT_KIND_TABLE,
        Value::Closure(_) | Value::Native(_) => EXT_KIND_FUNCTION,
        Value::Buffer(_) => EXT_KIND_BUFFER,
        Value::BigInt(_) => EXT_KIND_BIGINT,
        _ => EXT_KIND_INVALID,
    })
}

unsafe extern "C" fn api_new_table(array_hint: usize, field_hint: usize) -> NeyukiExtValue {
    api_value(|s| {
        let t = VmTable::with_capacity(array_hint.min(1 << 16), field_hint.min(1 << 16));
        Ok(s.new_handle(Value::Table(Rc::new(RefCell::new(t)))))
    })
}

unsafe extern "C" fn api_table_get(table: NeyukiExtValue, key: NeyukiExtValue) -> NeyukiExtValue {
    api_value(|s| {
        let t = s.import_table(&table)?;
        let key = s.import(&key)?;
        let value = t.borrow().raw_get(&key);
        Ok(s.export(value))
    })
}

unsafe extern "C" fn api_table_set(
    table: NeyukiExtValue,
    key: NeyukiExtValue,
    value: NeyukiExtValue,
) -> NeyukiExtValue {
    api_value(|s| {
        let t = s.import_table(&table)?;
        let key = s.import(&key)?;
        let value = s.import(&value)?;
        if matches!(key, Value::Nil) {
            return Err("table key is nil".to_string());
        }
        let old = {
            let mut t = t.borrow_mut();
            if t.frozen {
                return Err("attempt to modify a frozen table".to_string());
            }
            let old = t.raw_get(&key);
            t.raw_set(key, value)?;
            old
        };
        // Dropped with STATE still borrowed is fine for plain values, but
        // keep the replaced value until here so its Drop runs after the
        // table borrow ends.
        drop(old);
        Ok(NeyukiExtValue::nil())
    })
}

unsafe extern "C" fn api_table_len(table: NeyukiExtValue) -> usize {
    in_frame(|s| s.import_table(&table)).map_or(0, |t| t.borrow().array.len())
}

unsafe extern "C" fn api_table_next(
    table: NeyukiExtValue,
    key: NeyukiExtValue,
    out_key: *mut NeyukiExtValue,
    out_value: *mut NeyukiExtValue,
) -> bool {
    let result = in_frame(|s| {
        let t = s.import_table(&table)?;
        let key = s.import(&key)?;
        let next = crate::vm::table::next_entry(&t.borrow(), &key)?;
        Ok(next.map(|(k, v)| (s.export(k), s.export(v))))
    });
    match result {
        Ok(Some((k, v))) => {
            unsafe {
                if !out_key.is_null() {
                    out_key.write(k);
                }
                if !out_value.is_null() {
                    out_value.write(v);
                }
            }
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn api_call(
    func: NeyukiExtValue,
    argc: usize,
    argv: *const NeyukiExtValue,
) -> NeyukiExtValue {
    // Resolve everything first and release STATE: the callee can run any
    // script, including other extension calls.
    let prepared = in_frame(|s| {
        let f = s.import(&func)?;
        if !matches!(f, Value::Closure(_) | Value::Native(_)) {
            return Err(format!("attempt to call a {} value", f.type_name()));
        }
        let ext_args: &[NeyukiExtValue] = if argc == 0 || argv.is_null() {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(argv, argc) }
        };
        let args = ext_args
            .iter()
            .map(|a| s.import(a))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((s.vm()?, f, args))
    });
    let (vm, f, args) = match prepared {
        Ok(p) => p,
        Err(msg) => return api_value(|_| Err(msg)),
    };
    // SAFETY: `vm` is the VM of the innermost active extension call, set
    // from its `&mut VM` by `FrameGuard::push`; that caller does not touch
    // the VM until the extension function returns.
    let result = unsafe { (*vm.as_ptr()).call_function(f, &args) };
    api_value(move |s| {
        let mut results = result?;
        match results.len() {
            0 => Ok(NeyukiExtValue::nil()),
            1 => Ok(s.export(results.pop().unwrap())),
            _ => {
                let values: Box<[NeyukiExtValue]> =
                    results.into_iter().map(|v| s.export(v)).collect();
                let out = NeyukiExtValue::with(
                    EXT_TAG_MULTI,
                    0,
                    0.0,
                    values.as_ptr().cast(),
                    values.len(),
                );
                s.scratch.push(Scratch::Values(values));
                Ok(out)
            }
        }
    })
}

unsafe extern "C" fn api_new_buffer(data: *const u8, len: usize) -> NeyukiExtValue {
    api_value(|s| {
        if len > 64 * 1024 * 1024 {
            return Err("buffer too large (at most 64 MiB)".to_string());
        }
        let buf = if data.is_null() {
            VmBuffer::new(len)
        } else {
            VmBuffer::from_bytes(unsafe { std::slice::from_raw_parts(data, len) }.to_vec())
        };
        Ok(s.new_handle(Value::Buffer(Rc::new(RefCell::new(buf)))))
    })
}

unsafe extern "C" fn api_buffer_data(
    buffer: NeyukiExtValue,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
) -> bool {
    let found = in_frame(|s| match s.import(&buffer)? {
        Value::Buffer(b) => {
            let mut b = b.borrow_mut();
            Ok((b.data.as_mut_ptr(), b.data.len()))
        }
        other => Err(format!(
            "expected a buffer handle, got {}",
            other.type_name()
        )),
    });
    match found {
        Ok((ptr, len)) => {
            unsafe {
                if !out_ptr.is_null() {
                    out_ptr.write(ptr);
                }
                if !out_len.is_null() {
                    out_len.write(len);
                }
            }
            true
        }
        Err(_) => false,
    }
}

unsafe extern "C" fn api_to_string(value: NeyukiExtValue) -> NeyukiExtValue {
    api_value(|s| {
        let v = s.import(&value)?;
        let (ptr, len) = s.scratch_bytes(v.to_string().into_bytes());
        Ok(NeyukiExtValue::with(EXT_TAG_STRING, 0, 0.0, ptr, len))
    })
}

unsafe extern "C" fn api_retain(handle: NeyukiExtValue) -> NeyukiExtValue {
    api_value(|s| {
        if handle.tag != EXT_TAG_HANDLE {
            return Err("retain expects a handle".to_string());
        }
        let v = s.lookup(handle.i)?;
        s.next_persistent += 1;
        let id = PERSISTENT_BIT | s.next_persistent;
        s.persistent.insert(id, v);
        Ok(NeyukiExtValue::handle(id))
    })
}

unsafe extern "C" fn api_release(handle: NeyukiExtValue) {
    if handle.tag != EXT_TAG_HANDLE || handle.i & PERSISTENT_BIT == 0 {
        return;
    }
    // Released from any context (an extension's own destructor included);
    // the value is dropped after STATE is no longer borrowed.
    let removed = STATE.with(|s| s.borrow_mut().persistent.remove(&handle.i));
    drop(removed);
}

unsafe extern "C" fn api_check_permission(
    kind: u32,
    target: *const u8,
    target_len: usize,
) -> NeyukiExtValue {
    api_value(|s| {
        let target = unsafe { lossy(target, target_len) };
        // SAFETY: as in `api_call`; only shared access is needed.
        let vm = s.vm()?;
        let perms = &unsafe { vm.as_ref() }.permissions;
        match kind {
            EXT_PERM_READ => perms.check_read(&target),
            EXT_PERM_WRITE => perms.check_write(&target),
            EXT_PERM_NET => perms.check_net(&target),
            EXT_PERM_RUN => perms.check_run(&target),
            EXT_PERM_ENV => perms.check_env(&target),
            EXT_PERM_FFI => perms.check_ffi(&target),
            other => Err(format!("unknown permission kind {}", other)),
        }
        .map(|()| NeyukiExtValue::nil())
    })
}

// ---------------------------------------------------------------------------
// Loading and calling extensions
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ABI 2: services and the task scheduler
// ---------------------------------------------------------------------------

unsafe fn name_arg<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    std::str::from_utf8(unsafe { std::slice::from_raw_parts(ptr, len) })
        .ok()
        .filter(|n| !n.is_empty())
}

/// Init only: makes `service` (a vtable the extension owns) findable by
/// `name` through `lookup_service`. A later provider of the same name
/// replaces it. It is forgotten when the library is closed.
unsafe extern "C" fn api_provide_service(
    ctx: *mut c_void,
    name_ptr: *const u8,
    name_len: usize,
    service: *const c_void,
) -> bool {
    let Some(name) = (unsafe { name_arg(name_ptr, name_len) }) else {
        return false;
    };
    if ctx.is_null() || service.is_null() {
        return false;
    }
    let ctx = unsafe { &mut *(ctx as *mut RegisterContext) };
    ctx.services.retain(|(n, _)| n != name);
    ctx.services.push((name.to_string(), service));
    true
}

unsafe extern "C" fn api_lookup_service(name_ptr: *const u8, name_len: usize) -> *const c_void {
    let Some(name) = (unsafe { name_arg(name_ptr, name_len) }) else {
        return std::ptr::null();
    };
    SERVICES.with(|s| s.borrow().get(name).map_or(std::ptr::null(), |svc| svc.ptr))
}

/// The VM of the innermost extension call, with STATE released: the
/// scheduler calls below run script code, which may call extensions again.
fn frame_vm() -> Option<NonNull<VM>> {
    in_frame(|s| s.vm()).ok()
}

/// Parks the running task on `pump` for up to `timeout_secs` and returns
/// `YIELD`, which the calling function must return at once. Returns `NIL`
/// (nothing parked) outside a task, under a native that called back into
/// Neyuki, or for a zero timeout: the caller then pumps in place. The task
/// resumes with the events table `pump` returns, or an empty table once
/// the timeout passes, as the result of the function that parked.
unsafe extern "C" fn api_task_park(
    timeout_secs: f64,
    pump: NeyukiExtPumpFn,
    user_data: *mut c_void,
) -> NeyukiExtValue {
    api_value(|s| {
        if timeout_secs.is_nan() || timeout_secs < 0.0 {
            return Err("task_park: the timeout must be a non-negative number".to_string());
        }
        let wait = Duration::try_from_secs_f64(timeout_secs).unwrap_or(Duration::MAX);
        let vm = s.vm()?;
        if wait.is_zero() || task::current_task(unsafe { vm.as_ref() }).is_none() {
            return Ok(NeyukiExtValue::nil());
        }
        s.parked = Some((s.serial(), wait, pump, user_data));
        Ok(NeyukiExtValue::with(
            EXT_TAG_YIELD,
            0,
            0.0,
            std::ptr::null(),
            0,
        ))
    })
}

/// A wake callback and its data, handed to worker threads.
struct ExtWaker(NeyukiExtWakeFn, *mut c_void);
// SAFETY: `NeyukiExtWakeFn` is documented as callable from any thread.
unsafe impl Send for ExtWaker {}

/// Installs `wake` as the scheduler's waker: background work that
/// completes calls it so a blocked event loop returns and lets the
/// scheduler run. Replaces any waker installed before.
unsafe extern "C" fn api_set_waker(wake: NeyukiExtWakeFn, user_data: *mut c_void) -> bool {
    let Some(vm) = frame_vm() else { return false };
    let waker = ExtWaker(wake, user_data);
    // SAFETY: see `frame_vm`; the caller is blocked in the extension.
    task::set_waker(
        unsafe { &mut *vm.as_ptr() },
        Box::new(move || {
            let ExtWaker(wake, data) = &waker;
            unsafe { wake(*data) }
        }),
    );
    true
}

unsafe extern "C" fn api_has_waker() -> bool {
    frame_vm().is_some_and(|vm| task::has_waker(unsafe { vm.as_ref() }))
}

/// Seconds until the next task is due (0 when one is ready) in
/// `*out_secs`; false when no task is waiting.
unsafe extern "C" fn api_next_due(out_secs: *mut f64) -> bool {
    let Some(vm) = frame_vm() else { return false };
    let Some(due) = task::next_due(unsafe { &mut *vm.as_ptr() }) else {
        return false;
    };
    if !out_secs.is_null() {
        let secs = due.saturating_duration_since(Instant::now()).as_secs_f64();
        unsafe { out_secs.write(secs) };
    }
    true
}

/// Resumes the tasks that are due, for about `budget_secs`.
unsafe extern "C" fn api_run_tasks(budget_secs: f64) {
    let Some(vm) = frame_vm() else { return };
    let budget = Duration::try_from_secs_f64(budget_secs.max(0.0)).unwrap_or(Duration::ZERO);
    task::run_for(unsafe { &mut *vm.as_ptr() }, budget);
}

/// The scheduler side of a parked extension pump: calls it inside a frame
/// and checks that it handed back an events table.
fn ext_pump(
    pump: NeyukiExtPumpFn,
    user_data: *mut c_void,
) -> impl Fn(&mut VM, Duration) -> Result<Value, String> + 'static {
    move |vm, wait| {
        let _frame = FrameGuard::push(vm);
        let ret = unsafe { pump(wait.as_secs_f64(), user_data) };
        let value = STATE.with(|s| s.borrow().import(&ret))?;
        match value {
            Value::Table(_) => Ok(value),
            other => Err(format!(
                "native event pump returned {} instead of an events table",
                other.type_name()
            )),
        }
    }
}

pub(super) struct BoundExtFn {
    func: NeyukiExtFn,
    user_data: *mut c_void,
}

/// Runs an extension's init function, registers what it exports and returns
/// the library id plus a `name -> function index` table.
pub fn init_extension_with_fn(
    vm: &mut VM,
    init_fn: NeyukiExtInitFn,
    lib: Option<Library>,
) -> Result<(u64, VmTable), String> {
    let mut ctx = RegisterContext {
        exports: Vec::new(),
        services: Vec::new(),
    };
    let status = {
        let _frame = FrameGuard::push(vm);
        unsafe { init_fn(&API, (&mut ctx as *mut RegisterContext).cast()) }
    };
    if status != 0 {
        return Err(format!(
            "native extension initialization failed with status {}",
            status
        ));
    }

    let mut ext_fns = Vec::with_capacity(ctx.exports.len());
    let mut fn_map = VmTable::new();
    for (name, func, user_data) in ctx.exports {
        fn_map.set_str(&name, Value::from_usize(ext_fns.len()));
        ext_fns.push(BoundExtFn { func, user_data });
    }

    // The code stays mapped until the process exits, even after `close`:
    // the extension may have left thread-exit destructors (any Rust
    // thread-local, tokio's for one) that run after the runtime drops its
    // libraries, and calling into an unloaded library crashes the process.
    std::mem::forget(lib);

    let id = next_lib_id();
    SERVICES.with(|registry| {
        let mut registry = registry.borrow_mut();
        for (name, ptr) in ctx.services {
            registry.insert(name, Service { lib_id: id, ptr });
        }
    });
    LIBRARIES.with(|libs| {
        libs.borrow_mut().insert(
            id,
            LoadedLibrary {
                _lib: None,
                symbols: Vec::new(),
                ext_fns,
            },
        );
    });
    Ok((id, fn_map))
}

/// Loads the extension at `path` (already permission-checked).
pub fn load_extension(
    vm: &mut VM,
    path: &std::path::Path,
    shown: &str,
) -> Result<(u64, VmTable), String> {
    let lib = super::open_library(path)?;
    // SAFETY: the symbol is declared by the extension ABI with this type.
    let init_fn: NeyukiExtInitFn = unsafe {
        *lib.get::<NeyukiExtInitFn>(b"neyuki_extension_init\0")
            .map_err(|err| format!("missing 'neyuki_extension_init' in '{}': {}", shown, err))?
    };
    init_extension_with_fn(vm, init_fn, Some(lib))
}

/// Calls extension function `fn_idx` of library `lib_id`.
pub(super) fn call_ext(
    vm: &mut VM,
    lib_id: u64,
    fn_idx: usize,
    args: &[Value],
) -> Result<Vec<Value>, String> {
    let (func, user_data) = LIBRARIES.with(|libs| {
        let libs = libs.borrow();
        let lib = libs
            .get(&lib_id)
            .ok_or_else(|| "native extension is closed".to_string())?;
        let bound = lib
            .ext_fns
            .get(fn_idx)
            .ok_or_else(|| "invalid extension function".to_string())?;
        Ok::<_, String>((bound.func, bound.user_data))
    })?;

    let _frame = FrameGuard::push(vm);
    let ext_args: Vec<NeyukiExtValue> = STATE.with(|s| {
        let mut s = s.borrow_mut();
        args.iter().map(|v| s.export(v.clone())).collect()
    });
    // SAFETY: `func` was registered by the extension, whose library stays
    // loaded while `lib_id` is in LIBRARIES; `ext_args` and everything it
    // points to live until `_frame` drops.
    let ret = unsafe { func(ext_args.len(), ext_args.as_ptr(), user_data) };
    let serial = STATE.with(|s| s.borrow().serial());
    let parked = STATE.with(|s| s.borrow_mut().parked.take());
    if ret.tag == EXT_TAG_YIELD {
        let Some((_, wait, pump, data)) = parked.filter(|p| p.0 == serial) else {
            return Err("extension returned YIELD without parking the task".to_string());
        };
        // The frame goes first: the task is suspended past this return.
        drop(_frame);
        let thread =
            task::current_task(vm).ok_or_else(|| "extension parked outside a task".to_string())?;
        let deadline = task::deadline_after(Instant::now(), wait);
        return task::park_poller(vm, thread, deadline, ext_pump(pump, data));
    }
    STATE.with(|s| {
        let s = s.borrow();
        if ret.tag == EXT_TAG_MULTI {
            if ret.len > 0 && ret.ptr.is_null() {
                return Err("MULTI return with a null pointer".to_string());
            }
            if ret.len > 4096 {
                return Err("too many return values (at most 4096)".to_string());
            }
            let items: &[NeyukiExtValue] = if ret.len == 0 {
                &[]
            } else {
                unsafe { std::slice::from_raw_parts(ret.ptr.cast(), ret.len) }
            };
            items.iter().map(|v| s.import(v)).collect()
        } else {
            s.import(&ret).map(|v| vec![v])
        }
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::vm::value::StrRef;

    fn args<'a>(argc: usize, argv: *const NeyukiExtValue) -> &'a [NeyukiExtValue] {
        if argc == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(argv, argc) }
        }
    }

    unsafe fn api<'a>(user_data: *mut c_void) -> &'a NeyukiExtApi {
        unsafe { &*(user_data as *const NeyukiExtApi) }
    }

    unsafe extern "C" fn ext_add(
        argc: usize,
        argv: *const NeyukiExtValue,
        _ud: *mut c_void,
    ) -> NeyukiExtValue {
        let a = args(argc, argv);
        if a.len() != 2 || a[0].tag != EXT_TAG_INT || a[1].tag != EXT_TAG_INT {
            return NeyukiExtValue::error(b"expected two integers");
        }
        NeyukiExtValue::from_int(a[0].i + a[1].i)
    }

    /// `(t) -> sum of t's array part, #t, t.name`
    unsafe extern "C" fn ext_inspect(
        argc: usize,
        argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        let a = args(argc, argv);
        if a.len() != 1 || unsafe { (api.handle_kind)(a[0]) } != EXT_KIND_TABLE {
            return NeyukiExtValue::error(b"expected a table");
        }
        let n = unsafe { (api.table_len)(a[0]) };
        let mut sum = 0;
        for i in 1..=n {
            let v = unsafe { (api.table_get)(a[0], NeyukiExtValue::from_int(i as i64)) };
            sum += v.i;
        }
        let name = unsafe { (api.table_get)(a[0], NeyukiExtValue::from_bytes(b"name")) };
        thread_local! {
            static OUT: RefCell<[NeyukiExtValue; 3]> = const { RefCell::new([NeyukiExtValue::nil(); 3]) };
        }
        OUT.with(|o| {
            let mut o = o.borrow_mut();
            *o = [
                NeyukiExtValue::from_int(sum),
                NeyukiExtValue::from_int(n as i64),
                name,
            ];
            NeyukiExtValue::with(EXT_TAG_MULTI, 0, 0.0, o.as_ptr().cast(), 3)
        })
    }

    /// `(n) -> { 1, 2, ..., n, count = n }`
    unsafe extern "C" fn ext_range(
        argc: usize,
        argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        let n = args(argc, argv)[0].i;
        let t = unsafe { (api.new_table)(n as usize, 1) };
        for i in 1..=n {
            unsafe { (api.table_set)(t, NeyukiExtValue::from_int(i), NeyukiExtValue::from_int(i)) };
        }
        unsafe {
            (api.table_set)(
                t,
                NeyukiExtValue::from_bytes(b"count"),
                NeyukiExtValue::from_int(n),
            )
        };
        t
    }

    /// `(f, x) -> f(x) + 1`, propagating errors from `f`.
    unsafe extern "C" fn ext_apply(
        argc: usize,
        argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        let a = args(argc, argv);
        let r = unsafe { (api.call)(a[0], 1, &a[1]) };
        if r.tag == EXT_TAG_ERROR {
            return r;
        }
        NeyukiExtValue::from_int(r.i + 1)
    }

    thread_local! {
        static KEPT: std::cell::Cell<NeyukiExtValue> = const { std::cell::Cell::new(NeyukiExtValue::nil()) };
    }

    /// `(v)`: keeps `v` both as a local and a persistent handle.
    unsafe extern "C" fn ext_keep(
        argc: usize,
        argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        let a = args(argc, argv);
        KEPT.with(|k| k.set(unsafe { (api.retain)(a[0]) }));
        LOCAL.with(|k| k.set(a[0]));
        NeyukiExtValue::nil()
    }

    thread_local! {
        static LOCAL: std::cell::Cell<NeyukiExtValue> = const { std::cell::Cell::new(NeyukiExtValue::nil()) };
    }

    /// `() -> kept persistent, then the stale local (an error)`
    unsafe extern "C" fn ext_kept(
        _argc: usize,
        _argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        let kept = KEPT.with(|k| k.get());
        let stale = LOCAL.with(|k| k.get());
        if unsafe { (api.handle_kind)(stale) } != EXT_KIND_INVALID {
            return NeyukiExtValue::error(b"stale local handle still resolves");
        }
        kept
    }

    unsafe extern "C" fn ext_release(
        _argc: usize,
        _argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        unsafe { (api.release)(KEPT.with(|k| k.get())) };
        NeyukiExtValue::nil()
    }

    unsafe extern "C" fn ext_can_read(
        argc: usize,
        argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        let a = args(argc, argv);
        let r = unsafe { (api.check_permission)(EXT_PERM_READ, a[0].ptr, a[0].len) };
        if r.tag == EXT_TAG_ERROR {
            r
        } else {
            NeyukiExtValue::from_bool(true)
        }
    }

    static SERVICE: u64 = 0x5e7c;

    /// The test pump: one `"tick"` event, as an event loop would report.
    unsafe extern "C" fn tick_pump(_timeout: f64, ud: *mut c_void) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        let t = unsafe { (api.new_table)(1, 0) };
        unsafe {
            (api.table_set)(
                t,
                NeyukiExtValue::from_int(1),
                NeyukiExtValue::from_bytes(b"tick"),
            )
        };
        t
    }

    /// `park(seconds)`: parks on `tick_pump`, or pumps in place when it
    /// cannot.
    unsafe extern "C" fn ext_park(
        argc: usize,
        argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        let secs = args(argc, argv).first().map_or(0.0, |a| a.f);
        let r = unsafe { (api.task_park)(secs, tick_pump, ud) };
        if r.tag != EXT_TAG_NIL {
            return r;
        }
        unsafe { tick_pump(secs, ud) }
    }

    /// `yield_unparked()`: returns YIELD without parking, which must fail.
    unsafe extern "C" fn ext_yield_unparked(
        _argc: usize,
        _argv: *const NeyukiExtValue,
        _ud: *mut c_void,
    ) -> NeyukiExtValue {
        NeyukiExtValue::with(EXT_TAG_YIELD, 0, 0.0, std::ptr::null(), 0)
    }

    /// `service()`: whether `lookup_service` finds the one init provided.
    unsafe extern "C" fn ext_service(
        _argc: usize,
        _argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        let found = unsafe { (api.lookup_service)(b"test.service".as_ptr(), 12) };
        let missing = unsafe { (api.lookup_service)(b"test.none".as_ptr(), 9) };
        NeyukiExtValue::from_bool(
            std::ptr::eq(found, (&raw const SERVICE).cast()) && missing.is_null(),
        )
    }

    /// `due()`: seconds until the next task is due, or nil.
    unsafe extern "C" fn ext_due(
        _argc: usize,
        _argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        let mut secs = -1.0;
        if unsafe { (api.next_due)(&mut secs) } {
            NeyukiExtValue::from_float(secs)
        } else {
            NeyukiExtValue::nil()
        }
    }

    /// `run()`: runs the due tasks.
    unsafe extern "C" fn ext_run(
        _argc: usize,
        _argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        unsafe { (api(ud).run_tasks)(0.05) };
        NeyukiExtValue::nil()
    }

    static WAKES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    unsafe extern "C" fn count_wake(_ud: *mut c_void) {
        WAKES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// `waker()`: installs `count_wake`; returns has_waker before and after.
    unsafe extern "C" fn ext_waker(
        _argc: usize,
        _argv: *const NeyukiExtValue,
        ud: *mut c_void,
    ) -> NeyukiExtValue {
        let api = unsafe { api(ud) };
        let before = unsafe { (api.has_waker)() };
        let set = unsafe { (api.set_waker)(count_wake, std::ptr::null_mut()) };
        let after = unsafe { (api.has_waker)() };
        NeyukiExtValue::from_bool(!before && set && after)
    }

    pub(in crate::vm::libs::ffi) unsafe extern "C" fn test_ext_init(
        api: *const NeyukiExtApi,
        ctx: *mut c_void,
    ) -> i32 {
        let api_ref = unsafe { &*api };
        if api_ref.abi_version != NEYUKI_EXT_ABI_VERSION {
            return -1;
        }
        let provided = unsafe {
            (api_ref.provide_service)(
                ctx,
                b"test.service".as_ptr(),
                12,
                (&raw const SERVICE).cast(),
            )
        };
        if !provided {
            return -3;
        }
        let fns: [(&[u8], NeyukiExtFn); 14] = [
            (b"add_ints", ext_add),
            (b"inspect", ext_inspect),
            (b"range", ext_range),
            (b"apply", ext_apply),
            (b"keep", ext_keep),
            (b"kept", ext_kept),
            (b"release", ext_release),
            (b"can_read", ext_can_read),
            (b"park", ext_park),
            (b"yield_unparked", ext_yield_unparked),
            (b"service", ext_service),
            (b"due", ext_due),
            (b"run", ext_run),
            (b"waker", ext_waker),
        ];
        for (name, f) in fns {
            let ok = unsafe {
                (api_ref.register_fn)(ctx, name.as_ptr(), name.len(), f, api as *mut c_void)
            };
            if !ok {
                return -2;
            }
        }
        0
    }

    fn load(vm: &mut VM) -> (u64, VmTable) {
        init_extension_with_fn(vm, test_ext_init, None).unwrap()
    }

    fn idx(map: &VmTable, name: &str) -> usize {
        match map.get_str(name) {
            Value::Int(i) => i as usize,
            _ => panic!("missing {name}"),
        }
    }

    #[test]
    fn scalars_and_errors() {
        let mut vm = VM::new();
        let (id, map) = load(&mut vm);
        let f = idx(&map, "add_ints");
        let res = call_ext(&mut vm, id, f, &[Value::Int(19), Value::Int(23)]).unwrap();
        assert_eq!(res, vec![Value::Int(42)]);
        let err = call_ext(&mut vm, id, f, &[Value::Int(1)]).unwrap_err();
        assert_eq!(err, "expected two integers");
    }

    #[test]
    fn tables_in_and_out_with_multiple_returns() {
        let mut vm = VM::new();
        let (id, map) = load(&mut vm);
        let mut t = VmTable::new();
        for i in [5, 6, 7] {
            t.array.push(Value::Int(i));
        }
        t.set_str("name", Value::str("nums"));
        let t = Value::Table(Rc::new(RefCell::new(t)));
        let res = call_ext(&mut vm, id, idx(&map, "inspect"), &[t]).unwrap();
        assert_eq!(res, vec![Value::Int(18), Value::Int(3), Value::str("nums")]);

        let res = call_ext(&mut vm, id, idx(&map, "range"), &[Value::Int(4)]).unwrap();
        let Value::Table(out) = &res[0] else {
            panic!("expected a table")
        };
        let out = out.borrow();
        assert_eq!(out.array.len(), 4);
        assert_eq!(out.get_str("count"), Value::Int(4));

        let err = call_ext(&mut vm, id, idx(&map, "inspect"), &[Value::Int(1)]).unwrap_err();
        assert_eq!(err, "expected a table");
    }

    #[test]
    fn calls_back_into_the_vm() {
        let mut vm = VM::new();
        let (id, map) = load(&mut vm);
        let proto = crate::compiler::compile_program(
            "return { function(x) return x * 2 end, function() error(\"boom\") end }",
            "cb",
            crate::compiler::Options::RUN,
        )
        .unwrap();
        let fns = vm.execute(proto).unwrap();
        let Value::Table(t) = fns else {
            panic!("expected a table")
        };
        let fns = t.borrow().array.clone();
        let res = call_ext(
            &mut vm,
            id,
            idx(&map, "apply"),
            &[fns[0].clone(), Value::Int(20)],
        )
        .unwrap();
        assert_eq!(res, vec![Value::Int(41)]);
        let err = call_ext(
            &mut vm,
            id,
            idx(&map, "apply"),
            &[fns[1].clone(), Value::Nil],
        )
        .unwrap_err();
        assert!(err.contains("boom"), "{err}");
    }

    #[test]
    fn persistent_handles_outlive_the_call_and_local_ones_do_not() {
        let mut vm = VM::new();
        let (id, map) = load(&mut vm);
        let t = Value::Table(Rc::new(RefCell::new(VmTable::new())));
        call_ext(&mut vm, id, idx(&map, "keep"), std::slice::from_ref(&t)).unwrap();
        let res = call_ext(&mut vm, id, idx(&map, "kept"), &[]).unwrap();
        assert_eq!(res, vec![t]);
        call_ext(&mut vm, id, idx(&map, "release"), &[]).unwrap();
        let err = call_ext(&mut vm, id, idx(&map, "kept"), &[]).unwrap_err();
        assert!(err.contains("released"), "{err}");
    }

    #[test]
    fn permission_checks_follow_the_sandbox() {
        use crate::vm::permissions::Permissions;
        let mut vm = VM::new();
        let (id, map) = load(&mut vm);
        let f = idx(&map, "can_read");
        assert_eq!(
            call_ext(&mut vm, id, f, &[Value::str("/etc/hosts")]).unwrap(),
            vec![Value::Bool(true)]
        );
        vm.set_permissions(Permissions::deny_all());
        let err = call_ext(&mut vm, id, f, &[Value::str("/etc/hosts")]).unwrap_err();
        assert!(err.contains("permission denied"), "{err}");
    }

    /// A VM with the test extension loaded as the globals `EXT` (its id)
    /// and `F` (name -> function index), for scripts that call it through
    /// `__ffi_ext_call` the way `lib/ffi.nyk` does.
    fn script_vm() -> (VM, u64, VmTable) {
        let mut vm = VM::new();
        let (id, map) = load(&mut vm);
        vm.globals
            .insert(StrRef::from("EXT"), Value::from_usize(id as usize));
        vm.globals.insert(
            StrRef::from("F"),
            Value::Table(Rc::new(RefCell::new(map.clone()))),
        );
        (vm, id, map)
    }

    fn run(vm: &mut VM, src: &str) -> Result<Value, String> {
        let proto =
            crate::compiler::compile_program(src, "test", crate::compiler::Options::UNCHECKED)?;
        vm.execute_program(proto)
    }

    fn global(vm: &VM, name: &str) -> Value {
        vm.globals
            .get(&StrRef::from(name))
            .cloned()
            .unwrap_or(Value::Nil)
    }

    #[test]
    fn services_are_found_until_the_library_closes() {
        let mut vm = VM::new();
        let (id, map) = load(&mut vm);
        let f = idx(&map, "service");
        assert_eq!(
            call_ext(&mut vm, id, f, &[]).unwrap(),
            vec![Value::Bool(true)]
        );
        forget_services(id);
        let none = unsafe { (API.lookup_service)(b"test.service".as_ptr(), 12) };
        assert!(none.is_null());
    }

    #[test]
    fn a_task_parks_on_the_extension_pump() {
        let (mut vm, _, _) = script_vm();
        run(
            &mut vm,
            "task.spawn(function()\n\
             \u{20} local events = __ffi_ext_call(EXT, F.park, 5.0)\n\
             \u{20} _G.count = #events\n\
             \u{20} _G.first = events[1]\n\
             end)\n\
             _G.before = _G.count",
        )
        .expect("program runs");
        assert!(matches!(global(&vm, "before"), Value::Nil));
        assert_eq!(global(&vm, "count").to_string(), "1");
        assert_eq!(global(&vm, "first").to_string(), "tick");
    }

    #[test]
    fn outside_a_task_the_extension_pumps_in_place() {
        let (mut vm, id, map) = script_vm();
        let res = call_ext(&mut vm, id, idx(&map, "park"), &[Value::Float(5.0)]).unwrap();
        let Value::Table(t) = &res[0] else {
            panic!("expected the events table")
        };
        assert_eq!(t.borrow().array, vec![Value::str("tick")]);
        let err = call_ext(&mut vm, id, idx(&map, "park"), &[Value::Float(-1.0)]).unwrap_err();
        assert!(err.contains("non-negative"), "{err}");
    }

    #[test]
    fn yield_without_parking_is_an_error() {
        let (mut vm, id, map) = script_vm();
        let err = call_ext(&mut vm, id, idx(&map, "yield_unparked"), &[]).unwrap_err();
        assert!(err.contains("without parking"), "{err}");
    }

    #[test]
    fn extensions_see_and_run_due_tasks() {
        let (mut vm, _, _) = script_vm();
        run(
            &mut vm,
            "_G.idle = __ffi_ext_call(EXT, F.due)\n\
             task.delay(30, function() end)\n\
             _G.later = __ffi_ext_call(EXT, F.due)\n\
             task.defer(function() _G.ran = true end)\n\
             _G.now = __ffi_ext_call(EXT, F.due)\n\
             __ffi_ext_call(EXT, F.run)\n\
             _G.ran_inside = _G.ran\n\
             error(\"stop before the delay\")",
        )
        .expect_err("the script stops itself");
        assert!(matches!(global(&vm, "idle"), Value::Nil));
        let later = global(&vm, "later").to_string().parse::<f64>().unwrap();
        assert!(later > 25.0 && later <= 30.0, "{later}");
        let now = global(&vm, "now").to_string().parse::<f64>().unwrap();
        assert!(now < 0.01, "{now}");
        assert_eq!(global(&vm, "ran_inside").to_string(), "true");
    }

    #[test]
    fn the_extension_waker_is_installed() {
        let (mut vm, id, map) = script_vm();
        let res = call_ext(&mut vm, id, idx(&map, "waker"), &[]).unwrap();
        assert_eq!(res, vec![Value::Bool(true)]);
        let before = WAKES.load(std::sync::atomic::Ordering::SeqCst);
        let waker = task::take_waker(&mut vm).expect("a waker");
        waker();
        assert_eq!(WAKES.load(std::sync::atomic::Ordering::SeqCst), before + 1);
    }

    #[test]
    fn host_api_outside_a_call_is_an_error() {
        let r = unsafe { (API.new_table)(0, 0) };
        assert_eq!(r.tag, EXT_TAG_ERROR);
        assert_eq!(unsafe { (API.table_len)(r) }, 0);
        let r = unsafe { (API.task_park)(1.0, tick_pump, std::ptr::null_mut()) };
        assert_eq!(r.tag, EXT_TAG_ERROR);
        assert!(!unsafe { (API.has_waker)() });
        assert!(!unsafe { (API.next_due)(std::ptr::null_mut()) });
    }
}
