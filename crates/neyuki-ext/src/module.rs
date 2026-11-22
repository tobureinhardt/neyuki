//! Registering functions and services, and the C trampolines behind them.

use std::any::Any;
use std::cell::RefCell;
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::sys::{self, RawValue};
use crate::value::Value;
use crate::{api, set_api};

/// What an extension function hands back: values, or `Yield` after
/// [`crate::tasks::park`] parked the running task.
#[derive(Debug, PartialEq)]
pub enum Return {
    Values(Vec<Value>),
    Yield,
}

/// Anything an extension function may return.
pub trait IntoReturn {
    fn into_return(self) -> Return;
}

impl IntoReturn for Return {
    fn into_return(self) -> Return {
        self
    }
}

impl IntoReturn for () {
    fn into_return(self) -> Return {
        Return::Values(Vec::new())
    }
}

impl IntoReturn for Vec<Value> {
    fn into_return(self) -> Return {
        Return::Values(self)
    }
}

impl<T: Into<Value>> IntoReturn for T {
    fn into_return(self) -> Return {
        Return::Values(vec![self.into()])
    }
}

impl<A: Into<Value>, B: Into<Value>> IntoReturn for (A, B) {
    fn into_return(self) -> Return {
        Return::Values(vec![self.0.into(), self.1.into()])
    }
}

impl<A: Into<Value>, B: Into<Value>, C: Into<Value>> IntoReturn for (A, B, C) {
    fn into_return(self) -> Return {
        Return::Values(vec![self.0.into(), self.1.into(), self.2.into()])
    }
}

impl From<Vec<Value>> for Return {
    fn from(values: Vec<Value>) -> Return {
        Return::Values(values)
    }
}

type Boxed = Box<dyn Fn(&[Value]) -> Result<Return, String>>;

/// Handed to the init function: registers what the extension provides.
pub struct Module {
    ctx: *mut c_void,
}

impl Module {
    /// Registers `f` as `name` (each name once; not `close`). Scripts call
    /// it as `ext.name(...)`. The function gets the arguments as values and
    /// returns anything [`IntoReturn`]; an `Err` raises a script error, and
    /// a panic is caught and raised as one.
    pub fn function<F, R>(&mut self, name: &str, f: F) -> Result<&mut Self, String>
    where
        F: Fn(&[Value]) -> Result<R, String> + 'static,
        R: IntoReturn,
    {
        let boxed: Boxed = Box::new(move |args| f(args).map(IntoReturn::into_return));
        // Leaked on purpose: the host may call it until the library unloads.
        let data = Box::into_raw(Box::new(boxed)).cast::<c_void>();
        let ok =
            unsafe { (api().register_fn)(self.ctx, name.as_ptr(), name.len(), trampoline, data) };
        if ok {
            Ok(self)
        } else {
            drop(unsafe { Box::from_raw(data.cast::<Boxed>()) });
            Err(format!(
                "cannot register '{}' (empty, 'close' or taken)",
                name
            ))
        }
    }

    /// Publishes `service` under `name` for other extensions to find with
    /// [`lookup_service`]. Put a version in the name (`"nyk.ui/1"`) and
    /// change it whenever `T` changes: the other side trusts the name to
    /// describe the layout. Use a `#[repr(C)]` struct of `extern "C"`
    /// functions, since the consumer is built separately.
    pub fn provide<T: Sync>(
        &mut self,
        name: &str,
        service: &'static T,
    ) -> Result<&mut Self, String> {
        let ptr = (service as *const T).cast::<c_void>();
        if unsafe { (api().provide_service)(self.ctx, name.as_ptr(), name.len(), ptr) } {
            Ok(self)
        } else {
            Err(format!("cannot provide service '{}'", name))
        }
    }
}

/// The service another extension published as `name`, if it is loaded.
///
/// # Safety
/// `T` must be the type the provider published under that name.
pub unsafe fn lookup_service<T>(name: &str) -> Option<&'static T> {
    let ptr = unsafe { (api().lookup_service)(name.as_ptr(), name.len()) };
    unsafe { ptr.cast::<T>().as_ref() }
}

thread_local! {
    /// What the last trampoline returned. The host copies a return value
    /// before any other extension code runs, so one slot is enough.
    static RETURNED: RefCell<(Vec<Value>, Vec<RawValue>, String)> =
        const { RefCell::new((Vec::new(), Vec::new(), String::new())) };
}

/// Stores `result` and returns the raw value pointing into the store.
pub(crate) fn hand_back(result: Result<Return, String>) -> RawValue {
    RETURNED.with(|slot| {
        let mut slot = slot.borrow_mut();
        let (values, raws, message) = &mut *slot;
        match result {
            Ok(Return::Yield) => RawValue::with(sys::TAG_YIELD, 0, 0.0, std::ptr::null(), 0),
            Ok(Return::Values(v)) => {
                *values = v;
                *raws = values.iter().map(Value::raw).collect();
                match raws.len() {
                    0 => RawValue::NIL,
                    1 => raws[0],
                    n => RawValue::with(sys::TAG_MULTI, 0, 0.0, raws.as_ptr().cast(), n),
                }
            }
            Err(msg) => {
                *message = msg;
                RawValue::bytes(sys::TAG_ERROR, message.as_bytes())
            }
        }
    })
}

pub(crate) fn panic_message(payload: Box<dyn Any + Send>) -> String {
    let what = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string());
    format!("native extension panicked: {}", what)
}

/// Copies the arguments in.
///
/// # Safety
/// `argv` must point at `argc` values from the host.
pub(crate) unsafe fn import_args(argc: usize, argv: *const RawValue) -> Result<Vec<Value>, String> {
    if argc == 0 || argv.is_null() {
        return Ok(Vec::new());
    }
    let raw = unsafe { std::slice::from_raw_parts(argv, argc) };
    raw.iter().map(|v| unsafe { Value::from_raw(v) }).collect()
}

unsafe extern "C" fn trampoline(argc: usize, argv: *const RawValue, data: *mut c_void) -> RawValue {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `data` is the `Boxed` that `Module::function` leaked.
        let f = unsafe { &*data.cast::<Boxed>() };
        let args = unsafe { import_args(argc, argv) }?;
        f(&args)
    }));
    hand_back(result.unwrap_or_else(|p| Err(panic_message(p))))
}

/// The body of `neyuki_extension_init`, which [`crate::export!`] writes.
///
/// # Safety
/// Only for the host to call, with its API and context.
#[doc(hidden)]
pub unsafe fn init(
    api: *const sys::Api,
    ctx: *mut c_void,
    f: fn(&mut Module) -> Result<(), String>,
) -> i32 {
    let Some(host) = (unsafe { api.as_ref() }) else {
        return 1;
    };
    if host.abi_version < sys::ABI_VERSION
        || (host.struct_size as usize) < std::mem::size_of::<sys::Api>()
    {
        eprintln!(
            "native extension needs Neyuki extension ABI {} (this runtime speaks {})",
            sys::ABI_VERSION,
            host.abi_version
        );
        return 1;
    }
    set_api(api);
    let mut module = Module { ctx };
    match catch_unwind(AssertUnwindSafe(|| f(&mut module))) {
        Ok(Ok(())) => 0,
        Ok(Err(msg)) => {
            eprintln!("native extension failed to initialize: {}", msg);
            2
        }
        Err(p) => {
            eprintln!("{}", panic_message(p));
            3
        }
    }
}
