//! Write Neyuki native extensions in safe Rust.
//!
//! An extension is a `cdylib` that the runtime loads with
//! `ffi.loadExtension(path)`, or through `require` from a package that lists
//! it under `[native]` in `neyuki.toml`. This crate wraps the C ABI of
//! `include/neyuki_ext.h` (ABI 2):
//!
//! ```no_run
//! use neyuki_ext::{Module, Table, Value, args};
//!
//! fn sum(a: &[Value]) -> Result<(i64, usize), String> {
//!     let list = args::table_arg(a, 0, "list")?;
//!     let mut total = 0;
//!     for v in list.list()? {
//!         total += v.as_i64().ok_or("list items must be integers")?;
//!     }
//!     Ok((total, list.len()))
//! }
//!
//! fn init(m: &mut Module) -> Result<(), String> {
//!     m.function("sum", sum)?;
//!     m.function("pair", |_| Table::from_list([1, 2]))?;
//!     Ok(())
//! }
//!
//! neyuki_ext::export!(init);
//! ```
//!
//! ```lua
//! local ext = ffi.loadExtension("./libsum.so")
//! local total, count = ext.sum({ 1, 2, 3 }) --> 6, 3
//! ```
//!
//! Functions take the arguments as [`Value`]s and return `Result<_,
//! String>`: an `Err` raises a script error, and a panic is caught and
//! raised as one. [`args`] checks arguments with the runtime's own error
//! messages. Tables, functions and buffers are handles, valid until the call
//! that received them returns unless kept with `retain`. Every call into
//! the runtime has to happen on the script's thread, inside an extension
//! call.

use std::sync::atomic::{AtomicPtr, Ordering};

pub mod args;
mod module;
pub mod sys;
pub mod tasks;
mod value;

pub use module::{IntoReturn, Module, Return, lookup_service};
pub use value::{BigInt, Buffer, Function, Handle, Persistent, Table, Value, to_string};

#[doc(hidden)]
pub use module::init as __init;

static API: AtomicPtr<sys::Api> = AtomicPtr::new(std::ptr::null_mut());

fn set_api(api: *const sys::Api) {
    API.store(api.cast_mut(), Ordering::Release);
}

fn try_api() -> Option<&'static sys::Api> {
    // SAFETY: the host's API table is a static that outlives every library.
    unsafe { API.load(Ordering::Acquire).as_ref() }
}

/// The host API. Panics if the extension was never initialized, which only
/// happens when its functions are called outside the runtime.
pub(crate) fn api() -> &'static sys::Api {
    try_api().expect("neyuki-ext used before neyuki_extension_init")
}

/// What the sandbox can be asked about ([`check_permission`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    /// A file path to read.
    Read,
    /// A file path to write.
    Write,
    /// A `host:port` to connect to or listen on.
    Net,
    /// A program to run.
    Run,
    /// An environment variable name.
    Env,
    /// A native library path.
    Ffi,
}

/// Asks the sandbox (`--sandbox`, `--allow-*`) whether the script may
/// touch `target`. Native code is not sandboxed by itself: check before
/// doing I/O on a script's behalf, and return the `Err` as is.
pub fn check_permission(kind: Permission, target: &str) -> Result<(), String> {
    let kind = match kind {
        Permission::Read => sys::PERM_READ,
        Permission::Write => sys::PERM_WRITE,
        Permission::Net => sys::PERM_NET,
        Permission::Run => sys::PERM_RUN,
        Permission::Env => sys::PERM_ENV,
        Permission::Ffi => sys::PERM_FFI,
    };
    value::status(unsafe { (api().check_permission)(kind, target.as_ptr(), target.len()) })
}

/// Writes the library's `neyuki_extension_init`, which checks the ABI and
/// runs `init` to register the extension's functions and services.
#[macro_export]
macro_rules! export {
    ($init:path) => {
        /// The entry point the Neyuki runtime calls when it loads the library.
        ///
        /// # Safety
        /// Only for the runtime to call.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn neyuki_extension_init(
            api: *const $crate::sys::Api,
            ctx: *mut ::std::ffi::c_void,
        ) -> i32 {
            unsafe { $crate::__init(api, ctx, $init) }
        }
    };
}
