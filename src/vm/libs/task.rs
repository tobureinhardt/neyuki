//! The `task` library: a cooperative scheduler on top of coroutines.
//!
//! Every task is a coroutine. `task.spawn` runs one right away,
//! `task.defer` queues it for the next scheduler pass and `task.delay` and
//! `task.wait` park it until a deadline. Natives that block (`http.request`,
//! `os.run`, ...) park a task too: they hand their work to a worker thread
//! through `background::run`, and the worker pushes the result onto a
//! completion queue that the scheduler drains on every pass.
//!
//! The scheduler runs whenever the main thread blocks in `task.wait`, on
//! every `ui.poll` that cannot yield (so `ui.run` on the main thread drives
//! it once per frame, within a time budget), and after the main chunk
//! returns, when the program keeps running until no task is left waiting.
//!
//! `ui.poll` inside a task parks the task as a *poller* instead of blocking:
//! the scheduler then pumps the OS event loop itself (through a hook the UI
//! library installs, so this module does not depend on it), hands the events
//! to the first poller and wakes the others when their timeout is up. That
//! is what lets `task.spawn(function() ui.run(...) end)` return at once while
//! the window keeps running.
//!
//! Limits: one event loop is served at a time (events go to whichever poller
//! is first in line); cooperative scheduling means a main chunk that computes
//! without calling `task.wait` leaves the window unserved; and a poll inside
//! a function a native called back (the `ui.run` handler runs under `try`)
//! still blocks, because such a call cannot yield.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::vm::libs::background::Completion;
use crate::vm::libs::coroutine;
use crate::vm::machine::VM;
use crate::vm::value::{Value, VmTable};

/// What a parked task is resumed with when it wakes up.
enum Wake {
    /// A `task.wait` call: the seconds actually waited since `since`.
    Elapsed(Instant),
    /// A `task.delay` call or finished background work: these values.
    Args(Vec<Value>),
    /// Background work that failed: this error is raised in the task.
    Fail(String),
}

/// Pumps the OS event loop for up to the given time (it may return sooner,
/// when something happens or background work wakes it) and returns the
/// events as an array table. Installed by the UI library.
pub(crate) type Pump = fn(&mut VM, Duration) -> Result<Value, String>;

/// Called from a worker thread after it queued a completion, to wake an
/// interpreter that is blocked somewhere other than the completion queue
/// (`ui.poll` waiting for window events).
pub(crate) type Waker = Box<dyn Fn() + Send>;

/// The worker side of the completion queue.
pub(crate) struct CompletionSender {
    tx: Sender<Completion>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl CompletionSender {
    /// Queues a completion and wakes the interpreter. A VM that is gone has
    /// no use for it, so a closed queue just drops it.
    pub(crate) fn send(self, completion: Completion) {
        if self.tx.send(completion).is_ok()
            && let Some(wake) = &*self.waker.lock().unwrap_or_else(|e| e.into_inner())
        {
            wake();
        }
    }
}

/// Tasks waiting for the scheduler. The values held here are coroutine
/// handles, which keeps them alive while nothing else references them.
pub struct Scheduler {
    /// Tasks for the next pass, in the order they were queued.
    deferred: VecDeque<(Value, Vec<Value>)>,
    /// Tasks waiting for a deadline. The sequence number keeps tasks with
    /// the same deadline in the order they started waiting.
    sleeping: BTreeMap<(Instant, u64), (Value, Wake)>,
    next_seq: u64,
    /// Tasks that are due but have not been resumed yet: finished
    /// background work, and whatever a pass left over when it ran out of
    /// its time budget. They go first on the next pass.
    ready: VecDeque<(Value, Wake)>,
    /// Tasks parked in `ui.poll`, in the order they started waiting, with
    /// the time their poll ends.
    pollers: VecDeque<(Value, Instant)>,
    /// How the pollers' events are pumped; set when one parks.
    pump: Option<Pump>,
    /// Tasks waiting for background work, by job id.
    waiting: HashMap<u64, Value>,
    next_job: u64,
    completions: Receiver<Completion>,
    sender: Sender<Completion>,
    waker: Arc<Mutex<Option<Waker>>>,
    /// Coroutine ids of the threads started by `spawn`, `defer` or `delay`.
    /// Only these yield inside blocking natives; any other coroutine (a
    /// generator, say) keeps the blocking behavior it always had.
    task_ids: HashSet<usize>,
    /// How many tasks ended in an error since the last `run_until_idle`.
    failed: usize,
}

impl Default for Scheduler {
    fn default() -> Self {
        let (sender, completions) = mpsc::channel();
        Scheduler {
            deferred: VecDeque::new(),
            sleeping: BTreeMap::new(),
            next_seq: 0,
            ready: VecDeque::new(),
            pollers: VecDeque::new(),
            pump: None,
            waiting: HashMap::new(),
            next_job: 0,
            completions,
            sender,
            waker: Arc::new(Mutex::new(None)),
            task_ids: HashSet::new(),
            failed: 0,
        }
    }
}

impl Scheduler {
    fn sleep_until(&mut self, at: Instant, thread: Value, wake: Wake) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.sleeping.insert((at, seq), (thread, wake));
    }

    /// The earliest deadline among the sleepers and the pollers.
    fn earliest_deadline(&self) -> Option<Instant> {
        let sleeper = self.sleeping.first_key_value().map(|(k, _)| k.0);
        let poller = self.pollers.iter().map(|(_, at)| *at).min();
        match (sleeper, poller) {
            (Some(s), Some(p)) => Some(s.min(p)),
            (s, p) => s.or(p),
        }
    }

    fn is_idle(&self) -> bool {
        self.deferred.is_empty()
            && self.sleeping.is_empty()
            && self.pollers.is_empty()
            && self.ready.is_empty()
            && self.waiting.is_empty()
    }

    /// Drops every queued resumption of `thread`. Background work it was
    /// waiting for still runs to the end, but its result is discarded.
    fn forget(&mut self, thread: &Rc<RefCell<VmTable>>) {
        let same = |v: &Value| matches!(v, Value::Table(t) if Rc::ptr_eq(t, thread));
        self.deferred.retain(|(t, _)| !same(t));
        self.sleeping.retain(|_, (t, _)| !same(t));
        self.pollers.retain(|(t, _)| !same(t));
        self.ready.retain(|(t, _)| !same(t));
        self.waiting.retain(|_, t| !same(t));
        if let Some(id) = coroutine_id(thread) {
            self.task_ids.remove(&id);
        }
    }
}

/// The `_id` of a coroutine handle.
fn coroutine_id(thread: &Rc<RefCell<VmTable>>) -> Option<usize> {
    thread.borrow().get_str("_id").as_usize()
}

pub fn create_task_lib() -> Value {
    let t = Rc::new(RefCell::new(VmTable::new()));
    let mut b = t.borrow_mut();
    b.set_str("spawn", crate::native!("task.spawn", task_spawn));
    b.set_str("defer", crate::native!("task.defer", task_defer));
    b.set_str("delay", crate::native!("task.delay", task_delay));
    b.set_str("wait", crate::native!("task.wait", task_wait));
    b.set_str("cancel", crate::native!("task.cancel", task_cancel));
    b.frozen = true;
    drop(b);
    Value::Table(t)
}

/// The thread for a `spawn`/`defer`/`delay` argument: a function becomes a
/// new coroutine, a thread is used as it is. Either way it is remembered as
/// a task.
fn task_thread(
    vm: &mut VM,
    arg: Option<&Value>,
    func: &str,
    index: usize,
) -> Result<Value, String> {
    let thread = match arg {
        Some(f @ (Value::Closure(_) | Value::Native(_))) => coroutine::new_coroutine(vm, f.clone()),
        Some(t @ Value::Table(_)) if t.type_name() == "thread" => t.clone(),
        _ => {
            return Err(format!(
                "bad argument #{} to '{}' (function or thread expected)",
                index, func
            ));
        }
    };
    if let Value::Table(t) = &thread
        && let Some(id) = coroutine_id(t)
    {
        vm.tasks.task_ids.insert(id);
    }
    Ok(thread)
}

/// A delay in seconds; nil means "as soon as possible".
fn seconds_arg(args: &[Value], index: usize, func: &str) -> Result<f64, String> {
    let seconds = match args.get(index) {
        None | Some(Value::Nil) => 0.0,
        Some(v) => crate::vm::libs::args::number(v).map_err(|_| {
            format!(
                "bad argument #{} to '{}' (number expected)",
                index + 1,
                func
            )
        })?,
    };
    if seconds.is_nan() || seconds < 0.0 {
        return Err(format!(
            "bad argument #{} to '{}' (seconds must be a non-negative number)",
            index + 1,
            func
        ));
    }
    Ok(seconds)
}

/// `now + seconds`, saturating far in the future for huge or infinite
/// delays instead of overflowing.
fn deadline(now: Instant, seconds: f64) -> Instant {
    match Duration::try_from_secs_f64(seconds) {
        Ok(d) => deadline_after(now, d),
        Err(_) => deadline_after(now, Duration::MAX),
    }
}

/// `now + wait`, saturating far in the future instead of overflowing.
pub(crate) fn deadline_after(now: Instant, wait: Duration) -> Instant {
    now.checked_add(wait)
        .unwrap_or_else(|| now + Duration::from_secs(60 * 60 * 24 * 365 * 100))
}

/// Resumes `thread` with `args` if it is still suspended. A task that ends
/// in an error is reported on stderr and counted; the other tasks go on.
fn resume_task(vm: &mut VM, thread: Value, args: Vec<Value>) {
    resume_with(vm, thread, Wake::Args(args));
}

/// Resumes `thread` the way `wake` says (with values, or by raising an
/// error where it is suspended) if it is still suspended.
fn resume_with(vm: &mut VM, thread: Value, wake: Wake) {
    if coroutine::is_suspended(vm, &thread) {
        let outcome = match wake {
            Wake::Elapsed(since) => coroutine::coroutine_resume(
                vm,
                &[thread.clone(), Value::Float(since.elapsed().as_secs_f64())],
            ),
            Wake::Args(args) => {
                let mut resume_args = Vec::with_capacity(args.len() + 1);
                resume_args.push(thread.clone());
                resume_args.extend(args);
                coroutine::coroutine_resume(vm, &resume_args)
            }
            Wake::Fail(message) => coroutine::resume_with_error(vm, &thread, message),
        };
        let failure = match outcome {
            Ok(res) => match res.first() {
                Some(Value::Bool(false)) => {
                    Some(res.get(1).map(|v| v.to_string()).unwrap_or_default())
                }
                _ => None,
            },
            Err(err) => Some(err),
        };
        if let Some(message) = failure {
            eprintln!("task error: {}", message);
            vm.tasks.failed += 1;
        }
    }
    // A finished task is no longer one; dropping it here keeps the set from
    // growing with every task a long-running program starts.
    if !coroutine::is_suspended(vm, &thread)
        && let Value::Table(t) = &thread
        && let Some(id) = coroutine_id(t)
    {
        vm.tasks.task_ids.remove(&id);
    }
}

/// `task.spawn(f, ...)`: runs `f` (a function or thread) as a task right
/// away, until it first waits or yields, and returns its thread.
fn task_spawn(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let thread = task_thread(vm, args.first(), "task.spawn", 1)?;
    resume_task(vm, thread.clone(), args[1..].to_vec());
    Ok(vec![thread])
}

/// `task.defer(f, ...)`: runs `f` on the next scheduler pass.
fn task_defer(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let thread = task_thread(vm, args.first(), "task.defer", 1)?;
    vm.tasks
        .deferred
        .push_back((thread.clone(), args[1..].to_vec()));
    Ok(vec![thread])
}

/// `task.delay(seconds, f, ...)`: runs `f` once `seconds` have passed.
fn task_delay(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let seconds = seconds_arg(args, 0, "task.delay")?;
    let thread = task_thread(vm, args.get(1), "task.delay", 2)?;
    let rest = args.get(2..).unwrap_or_default().to_vec();
    let at = deadline(Instant::now(), seconds);
    vm.tasks.sleep_until(at, thread.clone(), Wake::Args(rest));
    Ok(vec![thread])
}

/// `task.wait(seconds?)`: suspends the calling task for `seconds` (by
/// default until the next scheduler pass) and returns the seconds actually
/// waited. Outside a yieldable coroutine, such as in the main chunk, it
/// blocks and runs the other tasks while it waits.
fn task_wait(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let seconds = seconds_arg(args, 0, "task.wait")?;
    let start = Instant::now();
    let at = deadline(start, seconds);
    if let Some(handle) = coroutine::yieldable_handle(vm) {
        vm.tasks
            .sleep_until(at, Value::Table(handle), Wake::Elapsed(start));
        return coroutine::coroutine_yield(vm, &[]);
    }
    run_until(vm, Some(at));
    Ok(vec![Value::Float(start.elapsed().as_secs_f64())])
}

/// `task.cancel(thread)`: unschedules a task and closes its coroutine. If
/// it was waiting for background work, that work's result is dropped.
fn task_cancel(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(Value::Table(thread)) = args.first().filter(|v| v.type_name() == "thread") else {
        return Err("bad argument #1 to 'task.cancel' (thread expected)".to_string());
    };
    vm.tasks.forget(thread);
    match coroutine::coroutine_close(vm, &args[..1])?.as_slice() {
        [Value::Bool(false), message, ..] => Err(message.to_string()),
        _ => Ok(vec![]),
    }
}

// BACKGROUND WORK ------------------------------------------------------------

/// Installs the function worker threads call after queuing a completion,
/// replacing any earlier one. `ui.poll` uses it to wake its event loop.
pub(crate) fn set_waker(vm: &mut VM, waker: Waker) {
    *vm.tasks.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(waker);
}

/// Whether a waker is installed.
pub(crate) fn has_waker(vm: &VM) -> bool {
    vm.tasks
        .waker
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// The running task's thread, if a blocking native called right here should
/// yield: inside a coroutine started by the task library, and not under a
/// native that called back into Neyuki.
pub(crate) fn current_task(vm: &VM) -> Option<Value> {
    let id = vm.current_co?;
    if !vm.tasks.task_ids.contains(&id) {
        return None;
    }
    coroutine::yieldable_handle(vm).map(Value::Table)
}

/// Parks the running task (`thread`, from `current_task`) as a poller until
/// `deadline` and yields. The task resumes with the events `pump` returns,
/// or with an empty array once the deadline passes; those become the values
/// of the native that called this.
pub(crate) fn park_poller(
    vm: &mut VM,
    thread: Value,
    deadline: Instant,
    pump: Pump,
) -> Result<Vec<Value>, String> {
    vm.tasks.pump = Some(pump);
    vm.tasks.pollers.push_back((thread, deadline));
    coroutine::coroutine_yield(vm, &[])
}

/// Parks `thread` until a new background job completes, returning the
/// job's id and the sender its worker reports through.
pub(crate) fn expect_completion(vm: &mut VM, thread: Value) -> (u64, CompletionSender) {
    let id = vm.tasks.next_job;
    vm.tasks.next_job += 1;
    vm.tasks.waiting.insert(id, thread);
    let sender = CompletionSender {
        tx: vm.tasks.sender.clone(),
        waker: vm.tasks.waker.clone(),
    };
    (id, sender)
}

/// Makes a completed job's task ready, running the job's `finish` here on
/// the interpreter thread. The completion of a cancelled task is dropped
/// without running `finish`.
fn file_completion(vm: &mut VM, completion: Completion) {
    let Some(thread) = vm.tasks.waiting.remove(&completion.id) else {
        return;
    };
    let wake = match (completion.finish)(vm) {
        Ok(values) => Wake::Args(values),
        Err(err) => Wake::Fail(err),
    };
    vm.tasks.ready.push_back((thread, wake));
}

/// Files every completion that has arrived so far.
fn drain_completions(vm: &mut VM) {
    while let Ok(completion) = vm.tasks.completions.try_recv() {
        file_completion(vm, completion);
    }
}

// SCHEDULING -----------------------------------------------------------------

fn empty_events() -> Value {
    Value::Table(Rc::new(RefCell::new(VmTable::new())))
}

/// Serves the parked pollers: pumps the event loop for up to `wait`, wakes
/// the first poller with the events if there are any, then wakes every
/// poller whose deadline has passed with no events. A failing pump fails
/// them all. Does nothing while no poller is parked.
fn service_pollers(vm: &mut VM, wait: Duration) {
    if vm.tasks.pollers.is_empty() {
        return;
    }
    let pumped = match vm.tasks.pump {
        Some(pump) => pump(vm, wait),
        None => {
            std::thread::sleep(wait);
            Ok(empty_events())
        }
    };
    match pumped {
        Ok(events) => {
            let any = matches!(&events, Value::Table(t) if !t.borrow().array.is_empty());
            if any && let Some((thread, _)) = vm.tasks.pollers.pop_front() {
                vm.tasks.ready.push_back((thread, Wake::Args(vec![events])));
            }
            let now = Instant::now();
            for (thread, at) in std::mem::take(&mut vm.tasks.pollers) {
                if at <= now {
                    let wake = Wake::Args(vec![empty_events()]);
                    vm.tasks.ready.push_back((thread, wake));
                } else {
                    vm.tasks.pollers.push_back((thread, at));
                }
            }
        }
        Err(message) => {
            for (thread, _) in std::mem::take(&mut vm.tasks.pollers) {
                vm.tasks
                    .ready
                    .push_back((thread, Wake::Fail(message.clone())));
            }
        }
    }
}

/// Runs every task that is due now: finished background work, the
/// deferred queue and the sleepers whose deadline has passed. Tasks queued
/// while the pass runs wait for the next one, so `task.wait()` in a loop
/// cannot starve the scheduler. With a `budget`, the pass stops once that
/// much time has gone by and leaves the rest for the next pass (one task
/// still runs until it yields, so the budget is not a hard limit). Returns
/// whether anything was resumed.
fn run_pass(vm: &mut VM, budget: Option<Duration>) -> bool {
    let start = Instant::now();
    service_pollers(vm, Duration::ZERO);
    drain_completions(vm);
    let deferred = std::mem::take(&mut vm.tasks.deferred);
    vm.tasks.ready.extend(
        deferred
            .into_iter()
            .map(|(thread, args)| (thread, Wake::Args(args))),
    );
    while let Some(entry) = vm.tasks.sleeping.first_entry() {
        if entry.key().0 > start {
            break;
        }
        let due = entry.remove();
        vm.tasks.ready.push_back(due);
    }
    let due = vm.tasks.ready.len();
    let mut ran = false;
    for _ in 0..due {
        if ran && budget.is_some_and(|b| start.elapsed() >= b) {
            break;
        }
        let Some((thread, wake)) = vm.tasks.ready.pop_front() else {
            break;
        };
        resume_with(vm, thread, wake);
        ran = true;
    }
    ran
}

/// Runs tasks until `until` passes, or with `None` until no task is left
/// waiting, sleeping the thread while nothing is due. While background work
/// is outstanding it sleeps on the completion queue instead, so a finished
/// job wakes it at once. While a task is parked in `ui.poll` it waits in the
/// event loop instead, so input is handled as it arrives and the waker (see
/// `set_waker`) ends the wait when background work finishes.
fn run_until(vm: &mut VM, until: Option<Instant>) {
    loop {
        let ran = run_pass(vm, None);
        if until.is_some_and(|u| u <= Instant::now()) {
            return;
        }
        if ran {
            continue;
        }
        let next = vm.tasks.earliest_deadline();
        let wake = match (next, until) {
            (Some(n), Some(u)) => Some(n.min(u)),
            (Some(n), None) => Some(n),
            (None, Some(u)) => Some(u),
            // A poller has a deadline, so this is unreachable while one is
            // parked.
            (None, None) if vm.tasks.waiting.is_empty() => return,
            (None, None) => None,
        };
        if !vm.tasks.pollers.is_empty()
            && let Some(wake) = wake
        {
            service_pollers(vm, wake.saturating_duration_since(Instant::now()));
            continue;
        }
        if vm.tasks.waiting.is_empty() {
            let now = Instant::now();
            if let Some(wake) = wake
                && wake > now
            {
                std::thread::sleep(wake - now);
            }
            continue;
        }
        let received = match wake {
            Some(wake) => vm
                .tasks
                .completions
                .recv_timeout(wake.saturating_duration_since(Instant::now())),
            // The scheduler holds a sender itself, so this cannot fail.
            None => vm
                .tasks
                .completions
                .recv()
                .map_err(|_| RecvTimeoutError::Disconnected),
        };
        if let Ok(completion) = received {
            file_completion(vm, completion);
        }
    }
}

/// Runs the scheduler until every task has finished or is suspended
/// without a wake-up. Called once the main chunk has returned. Fails if
/// any task raised an error, after the others have run.
pub fn run_until_idle(vm: &mut VM) -> Result<(), String> {
    run_until(vm, None);
    debug_assert!(vm.tasks.is_idle());
    match std::mem::take(&mut vm.tasks.failed) {
        0 => Ok(()),
        1 => Err("a task raised an error".to_string()),
        n => Err(format!("{} tasks raised an error", n)),
    }
}

/// Runs the tasks that are due right now without blocking (the REPL calls
/// this after each chunk and whenever a task wakes up between lines).
pub fn run_due(vm: &mut VM) {
    run_pass(vm, None);
    vm.tasks.failed = 0;
}

/// Runs the tasks that are due right now for about `budget` at most and
/// leaves the rest for the next call (`ui.poll` calls this every frame).
/// Task errors are reported and counted as always, so a script that ends
/// afterwards still exits with a failure.
pub(crate) fn run_for(vm: &mut VM, budget: Duration) {
    run_pass(vm, Some(budget));
}

/// How often a caller that cannot sleep on the completion queue and has no
/// waker installed (the REPL) should look for finished background work.
const COMPLETION_POLL: Duration = Duration::from_millis(20);

/// When the next task is due: now if one is ready or deferred, otherwise
/// the earliest sleeper or poller deadline, or `None` when no task is
/// waiting. While
/// background work is outstanding and no waker is installed, it is at most
/// `COMPLETION_POLL` away.
pub fn next_due(vm: &mut VM) -> Option<Instant> {
    drain_completions(vm);
    let now = Instant::now();
    if !vm.tasks.deferred.is_empty() || !vm.tasks.ready.is_empty() {
        return Some(now);
    }
    let sleeper = vm.tasks.earliest_deadline();
    if vm.tasks.waiting.is_empty() || has_waker(vm) {
        return sleeper;
    }
    let poll = now + COMPLETION_POLL;
    Some(sleeper.map_or(poll, |s| s.min(poll)))
}

#[cfg(test)]
mod tests {
    use crate::vm::machine::VM;
    use crate::vm::value::Value;

    fn run_program(vm: &mut VM, src: &str) -> Result<Value, String> {
        let proto = crate::compiler::compile_program(src, "test", crate::compiler::Options::RUN)?;
        vm.execute_program(proto)
    }

    /// Like `run_program`, without the checker, which does not know about
    /// natives a test registers itself.
    fn run_native_program(vm: &mut VM, src: &str) -> Result<Value, String> {
        let proto =
            crate::compiler::compile_program(src, "test", crate::compiler::Options::UNCHECKED)?;
        vm.execute_program(proto)
    }

    fn global(vm: &VM, name: &str) -> Value {
        vm.globals
            .get(&crate::vm::value::StrRef::from(name))
            .cloned()
            .unwrap_or(Value::Nil)
    }

    #[test]
    fn test_tasks_run_after_the_main_chunk() {
        let mut vm = VM::new();
        run_program(
            &mut vm,
            "_G.ticks = 0\n\
             task.spawn(function()\n\
             \u{20} while task.wait(0.001) do\n\
             \u{20}   _G.ticks += 1\n\
             \u{20}   if _G.ticks == 3 then break end\n\
             \u{20} end\n\
             end)\n\
             task.delay(0.002, function(v: string) _G.delayed = v end, \"done\")\n\
             _G.main_saw = _G.ticks",
        )
        .expect("program failed");
        assert_eq!(global(&vm, "main_saw").to_string(), "0");
        assert_eq!(global(&vm, "ticks").to_string(), "3");
        assert_eq!(global(&vm, "delayed").to_string(), "done");
        assert!(vm.tasks.is_idle());
    }

    #[test]
    fn test_task_errors_fail_the_program_after_the_rest_ran() {
        let mut vm = VM::new();
        let err = run_program(
            &mut vm,
            "task.spawn(function() error(\"boom\") end)\n\
             task.delay(0.001, function() error(\"again\") end)\n\
             task.delay(0.002, function() _G.ran = true end)\n\
             _G.main_done = true",
        )
        .expect_err("a failing task must fail the program");
        assert_eq!(err, "2 tasks raised an error");
        assert!(matches!(global(&vm, "main_done"), Value::Bool(true)));
        assert!(matches!(global(&vm, "ran"), Value::Bool(true)));
    }

    #[test]
    fn test_budgeted_passes_leave_the_rest_for_later() {
        let mut vm = VM::new();
        vm.execute(
            crate::compiler::compile_program(
                "_G.count = 0\n\
                 for i = 1, 5 do\n\
                 \u{20} task.defer(function()\n\
                 \u{20}   local t = os.clock()\n\
                 \u{20}   while os.clock() - t < 0.01 do end\n\
                 \u{20}   _G.count += 1\n\
                 \u{20} end)\n\
                 end",
                "test",
                crate::compiler::Options::RUN,
            )
            .expect("compiles"),
        )
        .expect("runs");
        super::run_for(&mut vm, std::time::Duration::from_millis(1));
        assert_eq!(global(&vm, "count").to_string(), "1");
        super::run_until_idle(&mut vm).expect("the rest run");
        assert_eq!(global(&vm, "count").to_string(), "5");
    }

    /// A task can use `background::run` and get the result back, and a
    /// failure is raised inside the task, not in the main program.
    #[test]
    fn test_background_work_resumes_tasks() {
        fn slow_double(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
            let n = crate::vm::libs::args::number_arg(args, 0, "n")?;
            crate::vm::libs::background::run(
                vm,
                move || {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if n < 0.0 {
                        Err("negative".to_string())
                    } else {
                        Ok(n * 2.0)
                    }
                },
                |_vm, doubled| Ok(vec![Value::Float(doubled)]),
            )
        }
        let mut vm = VM::new();
        vm.register_native(crate::native_def!("slow_double", slow_double));
        let start = std::time::Instant::now();
        let err = run_native_program(
            &mut vm,
            "_G.sum = 0\n\
             for i = 1, 5 do\n\
             \u{20} task.spawn(function() local d = slow_double(i) _G.sum += d end)\n\
             end\n\
             task.spawn(function() slow_double(-1) _G.after = true end)\n\
             _G.direct = slow_double(21)",
        )
        .expect_err("the failing task fails the program");
        assert_eq!(err, "a task raised an error");
        assert_eq!(global(&vm, "direct").to_string(), "42");
        assert_eq!(global(&vm, "sum").to_string(), "30");
        assert!(matches!(global(&vm, "after"), Value::Nil));
        // Six jobs of 100 ms plus one in place: well under the 700 ms they
        // would take one after another.
        assert!(start.elapsed() < std::time::Duration::from_millis(500));
        assert!(vm.tasks.is_idle());
    }

    #[test]
    fn test_cancel_drops_background_results() {
        fn slow(vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
            crate::vm::libs::background::run(
                vm,
                || {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    Ok(())
                },
                |_vm, ()| Ok(vec![Value::Bool(true)]),
            )
        }
        let mut vm = VM::new();
        vm.register_native(crate::native_def!("slow", slow));
        run_native_program(
            &mut vm,
            "local t = task.spawn(function() _G.got = slow() end)\n\
             task.cancel(t)\n\
             task.wait(0.15)",
        )
        .expect("program runs");
        assert!(matches!(global(&vm, "got"), Value::Nil));
        assert!(vm.tasks.is_idle());
    }

    // POLLERS ------------------------------------------------------------

    use std::cell::{Cell, RefCell};
    use std::time::{Duration, Instant};

    thread_local! {
        /// The events the fake event loop hands out, and how often it was
        /// pumped.
        static FAKE_EVENTS: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
        static PUMPS: Cell<usize> = const { Cell::new(0) };
    }

    fn event_table(events: Vec<Value>) -> Value {
        crate::vm::libs::args::array(events)
    }

    /// Stands in for the OS event loop: returns the queued events at once,
    /// or waits out `wait` (at most 5 ms, so tests stay quick and the
    /// scheduler loops instead of sleeping long) and returns none.
    fn fake_pump(_vm: &mut VM, wait: Duration) -> Result<Value, String> {
        PUMPS.with(|p| p.set(p.get() + 1));
        let events = FAKE_EVENTS.with(|e| std::mem::take(&mut *e.borrow_mut()));
        if events.is_empty() {
            std::thread::sleep(wait.min(Duration::from_millis(5)));
        }
        Ok(event_table(events))
    }

    fn failing_pump(_vm: &mut VM, _wait: Duration) -> Result<Value, String> {
        Err("event loop is gone".to_string())
    }

    /// `fake_poll(seconds, fail?)` is `ui.poll` on the fake event loop: a
    /// task with a timeout parks, anything else pumps in place.
    fn fake_poll(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let seconds = crate::vm::libs::args::number_arg(args, 0, "seconds")?;
        let pump: super::Pump = if matches!(args.get(1), Some(Value::Bool(true))) {
            failing_pump
        } else {
            fake_pump
        };
        let wait = Duration::from_secs_f64(seconds);
        if !wait.is_zero()
            && let Some(thread) = super::current_task(vm)
        {
            let at = super::deadline_after(Instant::now(), wait);
            return super::park_poller(vm, thread, at, pump);
        }
        Ok(vec![pump(vm, wait)?])
    }

    fn poll_vm() -> VM {
        FAKE_EVENTS.with(|e| e.borrow_mut().clear());
        PUMPS.with(|p| p.set(0));
        let mut vm = VM::new();
        vm.register_native(crate::native_def!("fake_poll", fake_poll));
        vm
    }

    fn queue_event(name: &str) {
        FAKE_EVENTS.with(|e| e.borrow_mut().push(Value::string(name.to_string())));
    }

    /// The bug: a task that polled blocked `task.spawn`, so the main chunk
    /// only went on once the "window" was gone. Now `spawn` returns at once
    /// with the task suspended, and the poll is served afterwards.
    #[test]
    fn test_spawned_poller_does_not_block_the_main_chunk() {
        let mut vm = poll_vm();
        run_native_program(
            &mut vm,
            "_G.order = \"\"\n\
             local t = task.spawn(function()\n\
             \u{20} _G.order = _G.order .. \"a\"\n\
             \u{20} fake_poll(0.01)\n\
             \u{20} _G.order = _G.order .. \"c\"\n\
             end)\n\
             _G.status = coroutine.status(t)\n\
             _G.order = _G.order .. \"b\"",
        )
        .expect("program runs");
        assert_eq!(global(&vm, "status").to_string(), "suspended");
        assert_eq!(global(&vm, "order").to_string(), "abc");
        assert!(vm.tasks.is_idle());
    }

    #[test]
    fn test_parked_poller_receives_events() {
        let mut vm = poll_vm();
        queue_event("keydown");
        run_native_program(
            &mut vm,
            "task.spawn(function()\n\
             \u{20} local events = fake_poll(5)\n\
             \u{20} _G.count = #events\n\
             \u{20} _G.first = events[1]\n\
             end)\n\
             _G.before = _G.count",
        )
        .expect("program runs");
        assert!(matches!(global(&vm, "before"), Value::Nil));
        assert_eq!(global(&vm, "count").to_string(), "1");
        assert_eq!(global(&vm, "first").to_string(), "keydown");
    }

    /// Events go to the first poller in line; the next one only gets an
    /// empty table once its own timeout is up.
    #[test]
    fn test_events_go_to_the_first_poller_only() {
        let mut vm = poll_vm();
        queue_event("keydown");
        run_native_program(
            &mut vm,
            "task.spawn(function() _G.one = #fake_poll(5) end)\n\
             task.spawn(function() _G.two = #fake_poll(0.02) end)",
        )
        .expect("program runs");
        assert_eq!(global(&vm, "one").to_string(), "1");
        assert_eq!(global(&vm, "two").to_string(), "0");
    }

    #[test]
    fn test_poller_deadline_expiry_gives_an_empty_table() {
        let mut vm = poll_vm();
        let start = Instant::now();
        run_native_program(
            &mut vm,
            "task.spawn(function()\n\
             \u{20} local events = fake_poll(0.05)\n\
             \u{20} _G.kind = type(events)\n\
             \u{20} _G.count = #events\n\
             end)",
        )
        .expect("program runs");
        assert!(start.elapsed() >= Duration::from_millis(50));
        assert_eq!(global(&vm, "kind").to_string(), "table");
        assert_eq!(global(&vm, "count").to_string(), "0");
    }

    #[test]
    fn test_cancel_removes_a_poller() {
        let mut vm = poll_vm();
        let start = Instant::now();
        run_native_program(
            &mut vm,
            "local t = task.spawn(function() fake_poll(30) _G.ran = true end)\n\
             _G.status = coroutine.status(t)\n\
             task.cancel(t)",
        )
        .expect("program runs");
        assert_eq!(global(&vm, "status").to_string(), "suspended");
        assert!(matches!(global(&vm, "ran"), Value::Nil));
        assert!(vm.tasks.pollers.is_empty());
        assert!(vm.tasks.is_idle());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// `run_until_idle` keeps going while a poller is parked, through any
    /// number of polls, and does not spin: the pump is called about once
    /// per wait, not once per CPU cycle.
    #[test]
    fn test_run_until_idle_waits_for_parked_pollers() {
        let mut vm = poll_vm();
        let start = Instant::now();
        run_native_program(
            &mut vm,
            "task.spawn(function()\n\
             \u{20} for i = 1, 3 do fake_poll(0.04) _G.polls = i end\n\
             end)",
        )
        .expect("program runs");
        assert_eq!(global(&vm, "polls").to_string(), "3");
        assert!(start.elapsed() >= Duration::from_millis(120));
        assert!(PUMPS.with(|p| p.get()) < 200, "the scheduler spun");
    }

    /// Other tasks keep their timing while a poller is parked, and a
    /// `task.wait` in the main chunk serves the poller.
    #[test]
    fn test_main_wait_serves_pollers_and_sleepers() {
        let mut vm = poll_vm();
        queue_event("tick");
        run_native_program(
            &mut vm,
            "task.spawn(function() _G.got = #fake_poll(5) end)\n\
             task.delay(0.01, function() _G.slept = true end)\n\
             task.wait(0.05)\n\
             _G.seen = _G.got",
        )
        .expect("program runs");
        assert_eq!(global(&vm, "seen").to_string(), "1");
        assert!(matches!(global(&vm, "slept"), Value::Bool(true)));
    }

    #[test]
    fn test_failing_pump_raises_in_the_pollers() {
        let mut vm = poll_vm();
        let err = run_native_program(
            &mut vm,
            "task.spawn(function()\n\
             \u{20} _G.failed = fake_poll(5, true)\n\
             end)",
        );
        assert!(err.is_err(), "the task error fails the program");
        assert!(matches!(global(&vm, "failed"), Value::Nil));
        assert!(vm.tasks.is_idle());
    }

    /// A caller that cannot yield keeps polling in place: the main chunk,
    /// a plain coroutine and a function under `try`.
    #[test]
    fn test_polls_that_cannot_yield_run_inline() {
        let mut vm = poll_vm();
        queue_event("inline");
        run_native_program(
            &mut vm,
            "_G.main = fake_poll(0.01)[1]\n\
             local gen = coroutine.wrap(function() coroutine.yield(#fake_poll(0.01)) end)\n\
             _G.plain = gen()\n\
             task.spawn(function()\n\
             \u{20} local ok, v = try(function() return #fake_poll(0.01) end)\n\
             \u{20} _G.guarded = v\n\
             \u{20} _G.zero = #fake_poll(0)\n\
             end)\n\
             _G.after_spawn = _G.guarded",
        )
        .expect("program runs");
        assert_eq!(global(&vm, "main").to_string(), "inline");
        assert_eq!(global(&vm, "plain").to_string(), "0");
        // The task polled inline, so `spawn` only returned once it was done.
        assert_eq!(global(&vm, "after_spawn").to_string(), "0");
        assert_eq!(global(&vm, "zero").to_string(), "0");
    }

    /// Plain coroutines are not tasks: blocking natives inside them block
    /// instead of yielding, so generators keep working.
    #[test]
    fn test_plain_coroutines_do_not_yield_for_background_work() {
        fn slow(vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
            crate::vm::libs::background::run(vm, || Ok(7), |_vm, n| Ok(vec![Value::Int(n)]))
        }
        let mut vm = VM::new();
        vm.register_native(crate::native_def!("slow", slow));
        run_native_program(
            &mut vm,
            "local gen = coroutine.wrap(function() coroutine.yield(slow()) end)\n\
             _G.got = gen()",
        )
        .expect("program runs");
        assert_eq!(global(&vm, "got").to_string(), "7");
    }
}
