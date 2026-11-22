//! Driving an event loop inside the runtime's task scheduler.
//!
//! An extension that waits on the OS (window events, a device) should not
//! block other tasks. The pattern, as `@nyk/ui`'s `poll` uses it:
//!
//! ```no_run
//! use std::time::Duration;
//! use neyuki_ext::{Return, Table, Value, args, tasks};
//!
//! fn pump(wait: Duration) -> Result<Value, String> {
//!     // Wait up to `wait` for events, then return them as a list.
//!     Ok(Table::new()?.into())
//! }
//!
//! fn poll(a: &[Value]) -> Result<Return, String> {
//!     let timeout = Duration::from_secs_f64(args::opt_number_arg(a, 0, "timeout", 0.0)?);
//!     if let Some(parked) = tasks::park(timeout, pump)? {
//!         return Ok(parked); // the task resumes with what `pump` returns
//!     }
//!     // Not in a task: wait here, but no longer than the next due task.
//!     let wait = tasks::next_due().map_or(timeout, |due| timeout.min(due));
//!     let events = pump(wait)?;
//!     tasks::run_tasks(Duration::from_millis(4));
//!     Ok(vec![events].into())
//! }
//! ```

use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::Duration;

use crate::api;
use crate::module::{Return, hand_back, panic_message};
use crate::value::{Value, status};

/// An event pump: waits up to the given time and returns the events that
/// happened as a list table.
pub type Pump = fn(Duration) -> Result<Value, String>;

unsafe extern "C" fn pump_trampoline(secs: f64, data: *mut c_void) -> crate::sys::RawValue {
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `data` is the `Pump` that `park` passed.
        let pump: Pump = unsafe { std::mem::transmute::<*mut c_void, Pump>(data) };
        let wait = Duration::try_from_secs_f64(secs.max(0.0)).unwrap_or(Duration::MAX);
        pump(wait).map(|v| Return::Values(vec![v]))
    }));
    hand_back(result.unwrap_or_else(|p| Err(panic_message(p))))
}

/// Parks the running task on `pump` for up to `timeout`. On `Some`, return
/// it from your function at once: the task resumes with what `pump`
/// returns (or an empty table once `timeout` passes) as your result.
/// `None` means the task cannot be parked here (the main chunk, a plain
/// coroutine, under a native that called back into Neyuki, or a zero
/// timeout), so wait in place instead.
pub fn park(timeout: Duration, pump: Pump) -> Result<Option<Return>, String> {
    let raw =
        unsafe { (api().task_park)(timeout.as_secs_f64(), pump_trampoline, pump as *mut c_void) };
    match raw.tag {
        crate::sys::TAG_YIELD => Ok(Some(Return::Yield)),
        _ => status(raw).map(|()| None),
    }
}

/// Installs `wake` as the scheduler's waker, called (from worker threads)
/// when background work completes so a loop blocked in the OS returns.
/// Install it once: each call leaks the previous closure, since the host
/// cannot say when it is done with it. Returns false outside a call.
pub fn set_waker(wake: impl Fn() + Send + Sync + 'static) -> bool {
    unsafe extern "C" fn call(data: *mut c_void) {
        // SAFETY: `data` is the leaked closure below, never freed.
        let wake = unsafe { &*data.cast::<Box<dyn Fn() + Send + Sync>>() };
        let _ = catch_unwind(AssertUnwindSafe(wake));
    }
    let boxed: Box<dyn Fn() + Send + Sync> = Box::new(wake);
    let data = Box::into_raw(Box::new(boxed)).cast::<c_void>();
    unsafe { (api().set_waker)(call, data) }
}

/// Whether a waker is installed.
pub fn has_waker() -> bool {
    unsafe { (api().has_waker)() }
}

/// How long until the next task is due (zero when one is ready), or `None`
/// when no task is waiting. Cap a blocking wait with it.
pub fn next_due() -> Option<Duration> {
    let mut secs = 0.0;
    unsafe { (api().next_due)(&mut secs) }
        .then(|| Duration::try_from_secs_f64(secs.max(0.0)).unwrap_or(Duration::MAX))
}

/// Resumes the tasks that are due, for about `budget`.
pub fn run_tasks(budget: Duration) {
    unsafe { (api().run_tasks)(budget.as_secs_f64()) }
}
