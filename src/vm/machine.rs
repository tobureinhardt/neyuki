use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use crate::bytecode::proto::Proto;
use crate::dap::DapSession;
use crate::vm::frame::CallFrame;
use crate::vm::hash::{FxHashMap, new_map, with_capacity};
use crate::vm::libs::{
    create_bit_lib, create_buffer_lib, create_coroutine_lib, create_debug_lib, create_json_lib,
    create_math_lib, create_os_lib, create_string_lib, create_table_lib, create_task_lib,
    create_utf8_lib,
};
use crate::vm::limits::ExecutionLimits;
use crate::vm::permissions::Permissions;
use crate::vm::table::TableKey;
use crate::vm::value::{NativeDef, StrRef, Value, VmClosure, VmTable};

pub use crate::vm::traceback::TraceFrame;
pub use crate::vm::upvalue::ParkedUpvalue;

pub const DEFAULT_MAX_CALL_DEPTH: usize = 512;

/// The interpreter loop only runs while the frame stack is deeper than its
/// target depth, so there is always a running frame inside it.
pub(crate) const RUNNING_FRAME: &str = "the interpreter loop has a running frame";

pub struct CoroutineState {
    pub stack: Vec<Value>,
    pub frames: Vec<CallFrame>,
    pub status: String,
    pub func: Value,
    /// The handle `coroutine.create` returned; `resume` and `status` only
    /// accept this exact table.
    pub handle: Weak<RefCell<VmTable>>,
    pub yield_callee: u8,
    pub yield_retc: u8,
    pub yield_values: Vec<Value>,
    pub open_upvalues: Vec<ParkedUpvalue>,
    /// Set by `coroutine::resume_with_error`: the next resume raises this
    /// error where the coroutine is suspended instead of returning values
    /// there.
    pub resume_error: Option<String>,
    /// Private `Job` region id (`0` until first resumed).
    pub job_id: u32,
}

/// A `debug.sethook` hook. `None` means no hook is set, in which case event
/// checks cost a single predictable branch.
#[derive(Clone)]
pub struct DebugHook {
    pub func: Value,
    pub on_call: bool,
    pub on_return: bool,
    pub on_line: bool,
    /// Fire a "count" event every this many instructions (0: never).
    pub count: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    Call,
    TailCall,
    Return,
}

impl HookEvent {
    pub(crate) fn name(self) -> &'static str {
        match self {
            HookEvent::Call => "call",
            HookEvent::TailCall => "tail call",
            HookEvent::Return => "return",
        }
    }

    pub(crate) fn wants(self, hook: &DebugHook) -> bool {
        match self {
            HookEvent::Call | HookEvent::TailCall => hook.on_call,
            HookEvent::Return => hook.on_return,
        }
    }
}

pub struct SuspendedCaller {
    pub stack: Vec<Value>,
    pub frames: Vec<CallFrame>,
    pub open_upvalues: Vec<ParkedUpvalue>,
}

pub struct VM {
    pub stack: Vec<Value>,
    pub frames: Vec<CallFrame>,
    /// Caller execution states put aside while a resumed coroutine runs,
    /// so cycle/pending collection still sees parked caller stacks & upvalues.
    pub(crate) suspended_callers: Vec<SuspendedCaller>,
    pub globals: FxHashMap<StrRef, Value>,
    /// Bumped on every global write: `GetImport` slots resolved under an
    /// older epoch re-resolve instead of trusting a stale value.
    pub globals_epoch: u64,
    pub heap: crate::vm::heap::VmHeap,
    /// Coroutine states by id, held weakly: each handle table's `co_state`
    /// field owns its state, so dropping the last handle frees a suspended
    /// coroutine cleanly.
    pub co_states: HashMap<usize, Weak<RefCell<CoroutineState>>>,
    /// Tracks each coroutine's `Job` region id so abandoned suspended
    /// coroutines tear down their `Job` heap cleanly on prune.
    pub(crate) co_jobs: HashMap<usize, u32>,
    /// Map length at the last dead-weak prune: pruning runs when the map
    /// doubles past this, keeping cleanup amortized O(1) per coroutine.
    pub(crate) co_states_pruned_at: usize,
    pub next_co_id: usize,
    pub current_co: Option<usize>,
    pub is_yielding: bool,
    /// How many `call_function` calls are active: native code (`try`,
    /// `table.sort`, metamethods, ...) that re-entered the interpreter and
    /// is still on the Rust stack.
    pub native_calls: usize,
    /// `native_calls` when the running coroutine was resumed. A yield can only
    /// happen at that level; deeper, it would unwind native frames that
    /// cannot be saved and resumed.
    pub yield_floor: usize,
    /// Frames of the callers that resumed the running coroutine, put aside
    /// while it runs. Each resume nests the Rust stack like a call does, so
    /// they count toward `max_call_depth`.
    pub frames_below: usize,
    /// Active `debug.sethook` hook, if any.
    pub hook: Option<DebugHook>,
    /// True while a hook function itself runs: hooks never trigger hooks.
    pub in_hook: bool,
    /// Instructions left until the next count event.
    pub hook_countdown: u32,
    /// Set while anything needs to see every instruction (a line or count
    /// hook, or a debugger stepping or holding line breakpoints). The
    /// dispatch loop tests only this.
    pub trap: bool,
    /// True when either a debug hook or DAP session is attached (`Call`/`Return` fast guard).
    pub debug_active: bool,
    /// Active DAP debug session, if any (`neyuki dap`). `None` keeps
    /// breakpoint checks to a single predictable branch.
    pub dap: Option<DapSession>,
    /// True while a DAP `evaluate` call runs: breakpoints stay silent
    /// inside evaluation (it would otherwise pause on itself).
    pub in_eval: bool,
    /// Upvalues that still point at a live register of the current stack,
    /// sorted by register index. Nearly always empty or very short, which is
    /// why a plain vector beats a map here.
    pub open_upvalues: Vec<ParkedUpvalue>,
    /// Active Beat loop safepoint stack for current execution chain.
    pub(crate) beat_loops: Vec<crate::vm::beat::ActiveBeatLoop>,
    /// One past the last value produced by the most recent MULTRET `Call`,
    /// `Vararg` or returning frame. Only meaningful for the instruction that
    /// immediately consumes it.
    pub top: usize,
    /// Modules already loaded by `require`, so each one runs once. The
    /// standard libraries are registered here up front.
    pub modules: HashMap<String, Value>,
    /// The directory of the entry script, which `require` resolves module
    /// files against before the working directory. `None` outside a script.
    pub script_dir: Option<std::path::PathBuf>,
    /// The `string` library, which string values index into for methods.
    pub string_lib: Value,
    /// Every value the frame that just unwound to a `run_to_depth` boundary
    /// returned, so `call_function` can hand back more than the first.
    pub(crate) returned: Vec<Value>,
    /// The call stack where the error now unwinding was raised, innermost
    /// frame first. Cleared when `try`, `pcall` or `xpcall` catches it.
    pub error_trace: Option<Vec<TraceFrame>>,
    /// Maximum call depth allowed before raising call stack overflow.
    pub max_call_depth: usize,
    /// Tasks queued by the `task` library.
    pub tasks: crate::vm::libs::task::Scheduler,
    /// Configurable execution budgets, deadline, and interrupt handle.
    pub limits: ExecutionLimits,
    /// Capability-based sandbox permissions.
    pub permissions: Permissions,
    #[cfg(not(debug_assertions))]
    pub(crate) cur_consts: *const Value,
    #[cfg(not(debug_assertions))]
    pub(crate) cur_exec: *const crate::bytecode::exec::ExecProgram,
    #[cfg(not(debug_assertions))]
    pub(crate) cur_closure: *const VmClosure,
    #[cfg(not(debug_assertions))]
    pub(crate) cur_upvals: *const Rc<crate::vm::value::Upvalue>,
    #[cfg(not(debug_assertions))]
    pub(crate) run_target_depth: usize,
    #[cfg(not(debug_assertions))]
    pub(crate) run_result: Option<Value>,
    #[cfg(not(debug_assertions))]
    pub(crate) run_err: Option<String>,
}

impl Default for VM {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns whether `val` carries interior-mutable state that must not be
/// shared across VM instances.
fn value_has_mutable_state(val: &Value) -> bool {
    if crate::vm::libs::json::is_null(val) {
        // `json.null` is a frozen thread-local sentinel compared by `Rc::ptr_eq`.
        return false;
    }
    val.is_table() || val.is_closure() || val.is_buffer()
}

fn table_needs_deep_clone(tbl: &VmTable) -> bool {
    tbl.metatable.is_some()
        || tbl.array.iter().any(value_has_mutable_state)
        || tbl.fields.values().any(value_has_mutable_state)
        || tbl.general.as_ref().is_some_and(|g| {
            g.values()
                .any(|(k, v)| value_has_mutable_state(k) || value_has_mutable_state(v))
        })
}

fn deep_clone_value(val: &Value) -> Value {
    if crate::vm::libs::json::is_null(val) {
        return val.clone();
    }
    if let Some(t) = val.as_table() {
        Value::table(deep_clone_table(&t.borrow()))
    } else {
        debug_assert!(
            !value_has_mutable_state(val),
            "unexpected mutable value in BuiltinTemplate: {}",
            val.type_name()
        );
        val.clone()
    }
}

fn deep_clone_table(tbl: &VmTable) -> Rc<RefCell<VmTable>> {
    let mut out = VmTable::with_capacity(tbl.array.len(), tbl.fields.len());
    out.frozen = tbl.frozen;
    out.array_peak = tbl.array_peak;
    for v in &tbl.array {
        out.array.push(deep_clone_value(v));
    }
    for (k, v) in tbl.fields.iter() {
        out.fields.insert(k.clone(), deep_clone_value(v));
    }
    if let Some(ref g) = tbl.general {
        let mut new_g = with_capacity(g.len());
        for (k, v) in g.values() {
            let ck = deep_clone_value(k);
            let cv = deep_clone_value(v);
            if let Ok(tk) = TableKey::from_value(&ck) {
                new_g.insert(tk, (ck, cv));
            }
        }
        out.general = Some(Box::new(new_g));
    }
    if let Some(ref mt) = tbl.metatable {
        out.metatable = Some(deep_clone_table(&mt.borrow()));
    }
    Rc::new(RefCell::new(out))
}

/// Pre-built per-thread template of globals and standard library tables.
///
/// Flat stdlib tables (containing only immutable natives/primitives and no
/// metatable) clone in one pass; any table containing nested tables or a
/// metatable is flagged for recursive deep cloning so no mutable table can
/// ever be shared between VMs.
struct BuiltinTemplate {
    base_globals: FxHashMap<StrRef, Value>,
    libs: Vec<(&'static str, StrRef, VmTable, bool)>,
    bit32_key: StrRef,
    g_key: StrRef,
    g_meta: VmTable,
    g_meta_needs_deep: bool,
}

impl BuiltinTemplate {
    fn build() -> Self {
        let mut scratch = VM::new_empty();
        crate::vm::builtins::register_all(&mut scratch);
        crate::vm::libs::register_natives(&mut scratch);

        // Extract `_G`'s metatable template and remove `_G` from base_globals
        // so every VM gets its own isolated `_G` table + metatable.
        let g_key = StrRef::from("_G");
        let g_meta = match scratch.globals.remove(&g_key) {
            Some(v) => v
                .as_table()
                .and_then(|t| t.borrow().metatable.as_ref().map(|m| m.borrow().clone()))
                .unwrap_or_default(),
            None => VmTable::new(),
        };
        let g_meta_needs_deep = table_needs_deep_clone(&g_meta);
        debug_assert!(
            !scratch.globals.values().any(value_has_mutable_state),
            "base_globals template must not hold shared mutable values"
        );

        let raw_libs: [(&'static str, Value); 11] = [
            ("bit", create_bit_lib()),
            ("buffer", create_buffer_lib()),
            ("coroutine", create_coroutine_lib()),
            ("debug", create_debug_lib()),
            ("json", create_json_lib()),
            ("math", create_math_lib()),
            ("os", create_os_lib()),
            ("string", create_string_lib()),
            ("table", create_table_lib()),
            ("task", create_task_lib()),
            ("utf8", create_utf8_lib()),
        ];
        let mut libs = Vec::with_capacity(raw_libs.len());
        for (name, val) in raw_libs {
            if let Some(t) = val.as_table() {
                let tbl = t.borrow().clone();
                let needs_deep = table_needs_deep_clone(&tbl);
                libs.push((name, StrRef::from(name), tbl, needs_deep));
            }
        }
        // Pre-reserve so cloning `base_globals` already has room for the 13
        // library and `_G` entries without rehashing in `VM::new()`.
        scratch.globals.reserve(libs.len() + 2);

        Self {
            base_globals: scratch.globals,
            libs,
            bit32_key: StrRef::from("bit32"),
            g_key,
            g_meta,
            g_meta_needs_deep,
        }
    }
}

fn default_max_call_depth() -> usize {
    std::env::var("NEYUKI_MAX_CALL_DEPTH")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_CALL_DEPTH)
}

impl VM {
    fn new_empty() -> Self {
        Self {
            stack: Vec::new(),
            frames: Vec::new(),
            suspended_callers: Vec::new(),
            globals: new_map(),
            globals_epoch: 0,
            heap: crate::vm::heap::VmHeap::new(),
            co_states: HashMap::new(),
            co_jobs: HashMap::new(),
            co_states_pruned_at: 0,
            next_co_id: 1,
            current_co: None,
            is_yielding: false,
            native_calls: 0,
            yield_floor: 0,
            frames_below: 0,
            hook: None,
            in_hook: false,
            hook_countdown: 0,
            trap: false,
            debug_active: false,
            dap: None,
            in_eval: false,
            open_upvalues: Vec::new(),
            beat_loops: Vec::new(),
            top: 0,
            modules: HashMap::new(),
            script_dir: None,
            string_lib: Value::NIL,
            returned: Vec::new(),
            error_trace: None,
            max_call_depth: default_max_call_depth(),
            tasks: Default::default(),
            limits: ExecutionLimits::new(),
            permissions: Permissions::allow_all(),
            #[cfg(not(debug_assertions))]
            cur_consts: std::ptr::null(),
            #[cfg(not(debug_assertions))]
            cur_exec: std::ptr::null(),
            #[cfg(not(debug_assertions))]
            cur_closure: std::ptr::null(),
            #[cfg(not(debug_assertions))]
            cur_upvals: std::ptr::null(),
            #[cfg(not(debug_assertions))]
            run_target_depth: 0,
            #[cfg(not(debug_assertions))]
            run_result: None,
            #[cfg(not(debug_assertions))]
            run_err: None,
        }
    }

    pub fn new() -> Self {
        thread_local! {
            static TEMPLATE: BuiltinTemplate = BuiltinTemplate::build();
        }
        let mut vm = Self::new_empty();
        vm.stack.reserve(256);
        vm.frames.reserve(64);
        TEMPLATE.with(|tpl| {
            let mut globals = tpl.base_globals.clone();
            let mut modules = HashMap::with_capacity(tpl.libs.len() + 4);

            let mut g_table = VmTable::new();
            g_table.metatable = Some(if tpl.g_meta_needs_deep {
                deep_clone_table(&tpl.g_meta)
            } else {
                Rc::new(RefCell::new(tpl.g_meta.clone()))
            });
            globals.insert(
                tpl.g_key.clone(),
                crate::vm::heap::VmHeap::alloc_table_untracked(g_table),
            );

            for (name, key, tbl, needs_deep) in &tpl.libs {
                let lib = if *needs_deep {
                    Value::table(deep_clone_table(tbl))
                } else {
                    crate::vm::heap::VmHeap::alloc_table_untracked(tbl.clone())
                };
                if *name == "string" {
                    vm.string_lib = lib.clone();
                } else if *name == "bit" {
                    globals.insert(tpl.bit32_key.clone(), lib.clone());
                    modules.insert("bit32".to_string(), lib.clone());
                }
                globals.insert(key.clone(), lib.clone());
                // `require("@neyuki/math")` hands back this same table even if
                // the script later reassigns the global.
                modules.insert((*name).to_string(), lib);
            }
            vm.globals = globals;
            vm.modules = modules;
        });
        vm
    }

    pub fn set_limits(&mut self, limits: ExecutionLimits) {
        self.limits = limits;
        self.update_trap();
    }

    pub fn set_permissions(&mut self, permissions: Permissions) {
        self.permissions = permissions;
    }

    pub fn max_call_depth(&self) -> usize {
        self.max_call_depth
    }

    pub fn set_max_call_depth(&mut self, depth: usize) {
        self.max_call_depth = depth;
    }

    /// Frames on the call stack, counting those of suspended coroutine
    /// callers.
    #[inline]
    pub(crate) fn call_depth(&self) -> usize {
        self.frames.len() + self.frames_below
    }

    pub fn set_global(&mut self, name: &str, mut value: Value) {
        let prev = self.globals.get(name).cloned();
        self.global_write_barrier(prev, &mut value);
        self.globals.insert(StrRef::from(name), value);
        self.globals_epoch = self.globals_epoch.wrapping_add(1);
    }

    pub fn register_native(&mut self, def: &'static NativeDef) {
        self.set_global(def.name, Value::native(def));
    }

    pub fn execute(&mut self, proto: Proto) -> Result<Value, String> {
        let closure = Rc::new(VmClosure {
            proto: Rc::new(proto),
            upvalues: Vec::new(),
        });
        self.stack.clear();
        self.frames.clear();
        self.suspended_callers.clear();
        self.open_upvalues.clear();
        self.beat_loops.clear();
        // A chunk that panicked (the REPL keeps its VM across chunks) can
        // leave these mid-call; a new top-level run starts outside any.
        self.native_calls = 0;
        self.yield_floor = 0;
        self.frames_below = 0;
        self.current_co = None;
        self.is_yielding = false;
        self.error_trace = None;

        // Ensure stack has enough capacity for main frame
        let max_reg = closure.proto.max_registers as usize;
        self.stack.resize(max_reg + 1, Value::NIL);

        self.frames.push(CallFrame::new(closure, 0));
        let res = self.catch_panics(|vm| vm.run());
        self.beat_loops.clear();
        match res {
            Ok(mut val) => {
                self.beat_escape_if_needed(&mut val);
                self.beat_flush_to_floor(crate::vm::beat::BeatMark::ZERO, true);
                self.detach_return_value(&mut val);
                Ok(val)
            }
            Err(err) => {
                self.beat_flush_to_floor(crate::vm::beat::BeatMark::ZERO, true);
                Err(err)
            }
        }
    }

    /// Runs a whole program: the main chunk, then every task it left
    /// scheduled until none is waiting any more.
    pub fn execute_program(&mut self, proto: Proto) -> Result<Value, String> {
        let value = self.execute(proto)?;
        self.catch_panics(crate::vm::libs::task::run_until_idle)?;
        Ok(value)
    }

    /// Turns a panic inside `f` into a runtime error. This is a last resort:
    /// script and bytecode input reach errors through `Result`, so a panic
    /// here is an interpreter bug, and the VM may be left mid-instruction.
    fn catch_panics<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, String>,
    ) -> Result<T, String> {
        let run_res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self)));
        match run_res {
            Ok(res) => res,
            Err(panic_payload) => {
                let msg = if let Some(s) = panic_payload.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "unexpected panic during VM execution".to_string()
                };
                Err(format!("VM runtime panic caught: {}", msg))
            }
        }
    }

    /// Writes a register, growing the stack if the frame's declared register
    /// window was exceeded (variadic results can spill past it).
    #[inline]
    pub(crate) fn set_reg_grow(&mut self, idx: usize, val: Value) {
        if idx >= self.stack.len() {
            self.stack.resize(idx + 1, Value::NIL);
        }
        self.stack[idx] = val;
    }

    /// Recomputes `trap` and `debug_active` after a hook, debugger, or limit setting changed.
    pub fn update_trap(&mut self) {
        let hook_wants = self.hook.as_ref().is_some_and(|h| h.on_line || h.count > 0);
        let debugger_wants = self.dap.as_ref().is_some_and(|d| d.wants_lines());
        self.trap = hook_wants || debugger_wants || self.limits.is_active();
        self.debug_active = self.hook.is_some() || self.dap.is_some();
    }

    /// Full collection with finalizers: mark, extract the finalizer queue,
    /// run each handler newest-first, settle resurrections, then finish the
    /// sweep. A handler error is recorded and propagated after the whole
    /// queue ran; its table is still collected (run-once guarantee).
    /// Drains any sweep left staged by an earlier bounded step first, so
    /// explicit collections always complete fully.
    /// Drops state-map entries whose coroutine is gone (last handle dropped).
    /// Weak liveness needs no GC visibility: an entry upgrades exactly while
    /// some handle — on a visible stack or a put-aside one — still owns it.
    /// The live coroutine state with this id, if its handle still exists.
    pub(crate) fn co_state(&self, co_id: usize) -> Option<Rc<RefCell<CoroutineState>>> {
        self.co_states.get(&co_id).and_then(|weak| weak.upgrade())
    }

    pub(crate) fn prune_co_states(&mut self) {
        let mut dead_jobs = Vec::new();
        self.co_states.retain(|id, weak| {
            let alive = weak.upgrade().is_some();
            if !alive && let Some(jid) = self.co_jobs.remove(id) {
                dead_jobs.push(jid);
            }
            alive
        });
        for jid in dead_jobs {
            if jid != 0 {
                let _ = self.teardown_job_and_finalize(jid);
            }
        }
        self.co_states_pruned_at = self.co_states.len();
    }

    /// Full collection with finalizers: flushes `Beat`, `pending_free` Old subtrees,
    /// local Bacon-Rajan cycle collection on `purple_buffer` + `suspect_roots`,
    /// weak-value pruning, and dead coroutine teardown.
    pub(crate) fn gc_collect(&mut self) -> Result<(), String> {
        let floor = self.heap.beat.floor;
        self.beat_flush_to_floor(floor, false);

        let pins = self.collect_root_pins();
        let plan = self.heap.old.collect_cycles(&pins);
        let first_err = self.run_old_reclaim_plan(plan);
        // Second pass settles any children released by finalized destructors
        // and any abandoned coroutine handles/jobs.
        let pins2 = self.collect_root_pins();
        let plan2 = self.heap.old.flush_pending(&pins2, true);
        let err2 = self.run_old_reclaim_plan(plan2);
        self.prune_co_states();
        let pins3 = self.collect_root_pins();
        let _ = self.heap.old.flush_pending(&pins3, true);
        self.heap.old.update_threshold();

        if let Some(e) = first_err.or(err2) {
            return Err(e);
        }
        Ok(())
    }

    /// Bounded collection step triggered when allocations cross `threshold`.
    pub(crate) fn gc_auto_step(&mut self) -> Result<(), String> {
        if !self.heap.old.pending_free.is_empty()
            && let Some(e) = self.flush_pending_old(false)
        {
            return Err(e);
        }
        if !self.heap.old.should_collect() {
            if let Some(limit) = self.limits.heap_limit_bytes
                && self.heap.old.bytes_allocated > limit
            {
                self.gc_collect()?;
                self.limits.check_heap(self.heap.old.bytes_allocated)?;
            }
            return Ok(());
        }
        let pins = self.collect_root_pins();
        let plan = self.heap.old.collect_cycles(&pins);
        let first_err = self.run_old_reclaim_plan(plan);
        self.prune_co_states();
        self.heap.old.update_threshold();
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Instruction;
    use crate::compiler::compile_to_proto;
    use crate::parser::Parser;

    fn run_code(src: &str) -> Value {
        let mut parser = Parser::new(src);
        let (stmts, _pool) = parser.parse_program().expect("syntax error");
        let proto = compile_to_proto(&stmts);
        let mut vm = VM::new();
        vm.execute(proto).expect("execution error")
    }

    /// Compiled like a debug session: no inlining, so hooks see every call.
    fn run_debug(src: &str) -> Value {
        let proto = crate::compiler::compile_program(src, "test", crate::compiler::Options::DEBUG)
            .expect("compile error");
        VM::new().execute(proto).expect("execution error")
    }

    fn run_err(src: &str) -> String {
        let mut parser = Parser::new(src);
        let (stmts, _pool) = parser.parse_program().expect("syntax error");
        let proto = compile_to_proto(&stmts);
        let mut vm = VM::new();
        vm.execute(proto).expect_err("expected an error")
    }

    #[test]
    fn test_abandoned_suspended_coroutines_are_collected() {
        // Abandoned suspended coroutines must die with their last reference
        // (Lua semantics): after a full collection only the live survivor
        // keeps a map entry, and it still resumes intact afterwards.
        let mut vm = VM::new();
        let mut parser = Parser::new(
            "local co = coroutine.create(function() coroutine.yield(42) return 7 end)\n\
             local _, v = coroutine.resume(co)\n\
             for i = 1, 100 do\n\
             \u{20} local tmp = coroutine.create(function() coroutine.yield(1) end)\n\
             \u{20} coroutine.resume(tmp)\n\
             \u{20} tmp = nil\n\
             end\n\
             collectgarbage(\"collect\")\n\
             local ok, w = coroutine.resume(co)\n\
             return w",
        );
        let (stmts, _pool) = parser.parse_program().expect("syntax error");
        let proto = compile_to_proto(&stmts);
        let res = vm.execute(proto).expect("execution error");
        assert_eq!(
            res.as_int(),
            Some(7),
            "survivor must resume intact through gc, got {}",
            res
        );
        // A fresh `execute` clears dead interpreter temps (the last
        // iteration's create-temp still holds its handle), then a second
        // collection finalizes whatever that released.
        let mut parser2 = Parser::new("return 1");
        let (stmts2, _pool2) = parser2.parse_program().expect("syntax error");
        let proto2 = compile_to_proto(&stmts2);
        vm.execute(proto2).expect("execution error");
        vm.gc_collect().expect("gc");
        assert!(
            vm.co_states.is_empty(),
            "abandoned suspended coroutines must be collected"
        );
    }

    #[test]
    fn test_vm_hook_yield_errors() {
        // Yielding from inside a hook with no coroutine active is an
        // error (Lua: "attempt to yield across a C-call boundary"),
        // never a hang.
        let code = "debug.sethook(function() return coroutine.yield(99) end, \"c\")\nlocal function foo() return 1 end\nreturn foo()";
        let err = run_err(code);
        assert!(
            err.contains("yield"),
            "expected a yield error, got: {}",
            err
        );
    }

    #[test]
    fn test_vm_arithmetic() {
        let res = run_code("local a = 10\nlocal b = 20\nreturn a + b * 2");
        assert_eq!(res.to_string(), "50");
    }

    #[test]
    fn test_vm_bitwise_operations() {
        let res =
            run_code("local a = 12 & 10\nlocal b = 12 | 10\nlocal c = 12 ~ 10\nreturn a + b + c");
        // 8 + 14 + 6 = 28
        assert_eq!(res.to_string(), "28");
    }

    #[test]
    fn test_vm_bitwise_not_and_shifts() {
        let res = run_code("local a = ~0\nlocal b = 1 << 4\nlocal c = 16 >> 2\nreturn a + b + c");
        // -1 + 16 + 4 = 19
        assert_eq!(res.to_string(), "19");
    }

    #[test]
    fn test_vm_logical_shifts() {
        let res = run_code("local a = -1 >>> 60\nlocal b = 1 <<< 4\nreturn a + b");
        // 15 + 16 = 31
        assert_eq!(res.to_string(), "31");
    }

    #[test]
    fn test_vm_shr_edge_cases() {
        // Negative value, oversized shift, and negative shift amount.
        let res = run_code("return (16 >> 2) + (1 >> 100) + (-8 >> 2) + (4 >> -1)");
        // 4 + 0 + -2 + 8 = 10
        assert_eq!(res.to_string(), "10");
    }

    #[test]
    fn test_vm_shr_negative_runtime() {
        let res = run_code("local x = 4\nlocal s = -2\nreturn x >> s");
        assert_eq!(res.to_string(), "16");
    }

    #[test]
    fn test_vm_mod_edge_cases() {
        let res = run_code("return (17 % 5) + (-17 % 5) + (7.5 % 2)");
        // Floored modulo: 2 + 3 + 1.5 = 6.5 (verified against Lua 5.4).
        assert_eq!(res.to_string(), "6.5");
    }

    #[test]
    fn test_vm_mod_min_zero() {
        // i64::MIN % -1 is 0 mathematically, not an overflow panic.
        let res = run_code("return -9223372036854775808 % -1");
        assert_eq!(res.to_string(), "0");
    }

    #[test]
    fn test_vm_mod_zero_message() {
        let err = crate::vm::ops::eval_mod(
            &crate::vm::value::Value::int(1),
            &crate::vm::value::Value::int(0),
        )
        .unwrap_err();
        assert_eq!(err, "modulo by zero");
    }

    #[test]
    fn test_vm_idiv_edge_cases() {
        let res = run_code("return (17 // 5) + (-17 // 5) + (7.5 // 2)");
        // Floored division: 3 + -4 + 3.0 = 2.0, displayed as "2".
        assert_eq!(res.to_string(), "2");
    }

    #[test]
    fn test_vm_idiv_min_neg1_widens() {
        // i64::MIN // -1 overflows i64: must widen to BigInt, not wrap.
        let res = run_code("return -9223372036854775808 // -1");
        assert_eq!(res.to_string(), "9223372036854775808");
    }

    #[test]
    fn test_vm_idiv_zero_message() {
        let err = crate::vm::ops::eval_idiv(
            &crate::vm::value::Value::int(1),
            &crate::vm::value::Value::int(0),
        )
        .unwrap_err();
        assert_eq!(err, "division by zero");
    }

    #[test]
    fn test_vm_bitops_int_paths() {
        let res = run_code("return (12 & 10) + (12 | 10) + (12 ~ 10) + (1 << 4)");
        // 8 + 14 + 6 + 16 = 44
        assert_eq!(res.to_string(), "44");
    }

    #[test]
    fn test_vm_shl_widening_fallback() {
        // 1 << 100 widens past i64: must equal 2^100, not wrap.
        // 3 << 62 also widens (headroom rule); both go through fallback.
        let res = run_code(
            "return ((1 << 100) == 1267650600228229401496703205376) and ((3 << 62) == 13835058055282163712)",
        );
        assert_eq!(res.to_string(), "true");
    }

    #[test]
    fn test_vm_functions_and_calls() {
        let code = "function add(x, y) return x + y end\nreturn add(15, 27)";
        let res = run_code(code);
        assert_eq!(res.to_string(), "42");
    }

    #[test]
    fn test_vm_tail_call_deep_recursion() {
        let code = "local function loop(n, acc)\nif n == 0 then return acc end\nreturn loop(n - 1, acc + n)\nend\nreturn loop(100000, 0)";
        let res = run_code(code);
        assert_eq!(res.to_string(), "5000050000");
    }

    #[test]
    fn test_vm_if_and_loops() {
        let code =
            "local sum = 0\nlocal i = 1\nwhile i <= 5 do\n  sum = sum + i\n  i++\nend\nreturn sum";
        let res = run_code(code);
        assert_eq!(res.to_string(), "15");
    }

    #[test]
    fn test_vm_tables() {
        let code = "local t = {10, 20, 30}\nt[2] = 99\nreturn t[2]";
        let res = run_code(code);
        assert_eq!(res.to_string(), "99");
    }

    #[test]
    fn test_vm_bit_library() {
        let code = "local band = bit.band(15, 7)\nlocal bor = bit.bor(8, 2)\nlocal bxor = bit.bxor(10, 6)\nreturn band + bor + bxor";
        // 7 + 10 + 12 = 29
        let res = run_code(code);
        assert_eq!(res.to_string(), "29");
    }

    #[test]
    fn test_vm_buffer_library() {
        let code = "local b = buffer.create(16)\nbuffer.writeu8(b, 0, 42)\nbuffer.writei32(b, 4, 123456)\nlocal u = buffer.readu8(b, 0)\nlocal i = buffer.readi32(b, 4)\nreturn u + i";
        // 42 + 123456 = 123498
        let res = run_code(code);
        assert_eq!(res.to_string(), "123498");
    }

    #[test]
    fn test_vm_gc_library() {
        let code = "local kb = collectgarbage(\"count\")\nassert(kb >= 0)\nlocal running = collectgarbage(\"isrunning\")\nassert(running == true)\nreturn 1";
        let res = run_code(code);
        assert_eq!(res.to_string(), "1");
    }

    #[test]
    fn test_vm_math_library() {
        let code = "local a = math.abs(-15)\nlocal b = math.floor(3.9)\nlocal c = math.max(10, 25, 5)\nreturn a + b + c";
        // 15 + 3 + 25 = 43
        let res = run_code(code);
        assert_eq!(res.to_string(), "43");
    }

    #[test]
    fn test_vm_string_library() {
        let code = "local s = \"hello\"\nlocal upper = string.upper(s)\nlocal sub = string.sub(s, 1, 4)\nreturn upper .. \"_\" .. sub";
        let res = run_code(code);
        assert_eq!(res.to_string(), "HELLO_hell");
    }

    #[test]
    fn test_vm_table_library() {
        let code =
            "local t = {10, 20}\ntable.insert(t, 30)\nlocal s = table.concat(t, \",\")\nreturn s";
        let res = run_code(code);
        assert_eq!(res.to_string(), "10,20,30");
    }

    #[test]
    fn test_vm_pcall_and_error() {
        let code = "local ok, err = pcall(error, \"boom\")\nassert(ok == false)\nassert(err == \"boom\")\nreturn 1";
        let res = run_code(code);
        assert_eq!(res.to_string(), "1");
    }

    #[test]
    fn test_vm_and_or_short_circuit() {
        let code = "local a = false and 10\nlocal b = 5 or 20\nlocal c = false or 30\nlocal d = true and 40\nreturn b + c + d";
        // 5 + 30 + 40 = 75
        let res = run_code(code);
        assert_eq!(res.to_string(), "75");
    }

    #[test]
    fn test_vm_for_prep_and_for_loop() {
        let code = "local sum = 0\nfor i = 1, 5 do\n  sum = sum + i\nend\nreturn sum";
        let res = run_code(code);
        assert_eq!(res.to_string(), "15");

        let code_step = "local sum = 0\nfor i = 10, 2, -2 do\n  sum = sum + i\nend\nreturn sum";
        // 10 + 8 + 6 + 4 + 2 = 30
        let res_step = run_code(code_step);
        assert_eq!(res_step.to_string(), "30");
    }

    #[test]
    fn test_vm_setlist_table_construct() {
        let code = "local t = {10, 20, 30, 40, 50}\nreturn t[1] + t[3] + t[5]";
        // 10 + 30 + 50 = 90
        let res = run_code(code);
        assert_eq!(res.to_string(), "90");
    }

    #[test]
    fn test_vm_metatables_index_newindex() {
        let code = "local proto = { answer = 42 }\nlocal t = {}\nsetmetatable(t, { __index = proto })\nreturn t.answer";
        let res = run_code(code);
        assert_eq!(res.to_string(), "42");

        let code_fn = "local t = {}\nsetmetatable(t, { __index = function(tbl, key) return key .. \"_handled\" end })\nreturn t.foo";
        let res_fn = run_code(code_fn);
        assert_eq!(res_fn.to_string(), "foo_handled");

        let code_newindex = "local sink = {}\nlocal t = {}\nsetmetatable(t, { __newindex = sink })\nt.val = 123\nreturn sink.val";
        let res_newindex = run_code(code_newindex);
        assert_eq!(res_newindex.to_string(), "123");
    }

    #[test]
    fn test_vm_metatables_operators_and_call() {
        let code_add = "local mt = { __add = function(a, b) return a.val + b.val end }\nlocal a = { val = 15 }\nlocal b = { val = 27 }\nsetmetatable(a, mt)\nreturn a + b";
        let res_add = run_code(code_add);
        assert_eq!(res_add.to_string(), "42");

        let code_call = "local t = {}\nsetmetatable(t, { __call = function(tbl, x, y) return x * y end })\nreturn t(6, 7)";
        let res_call = run_code(code_call);
        assert_eq!(res_call.to_string(), "42");

        let code_tostring = "local t = { name = \"Neyuki\" }\nsetmetatable(t, { __tostring = function(x) return \"Hello \" .. x.name end })\nreturn tostring(t)";
        let res_tostring = run_code(code_tostring);
        assert_eq!(res_tostring.to_string(), "Hello Neyuki");
    }

    #[test]
    fn test_vm_raw_builtins() {
        let code = "local t = {}\nrawset(t, \"k\", 99)\nlocal a = rawget(t, \"k\")\nlocal eq = rawequal(t, t)\nassert(eq == true)\nreturn a";
        let res = run_code(code);
        assert_eq!(res.to_string(), "99");
    }

    #[test]
    fn test_vm_debug_hook_call_return() {
        // Verified against Lua 5.4: the sethook call itself reports its
        // return (hook is active by then), then add's pair, then the
        // clearing sethook reports its call (but no return: hook is off).
        let code = "local events = {}\nlocal function tracer(ev)\n  events[#events + 1] = ev\nend\ndebug.sethook(tracer, \"cr\")\nlocal function add(x, y)\n  return x + y\nend\nlocal s = add(1, 2)\ndebug.sethook(nil)\nreturn table.concat(events, \",\")";
        let res = run_debug(code);
        assert_eq!(res.to_string(), "return,call,return,call");
    }

    #[test]
    fn test_vm_debug_gethook_roundtrip() {
        let code = "local function tracer(ev) end\ndebug.sethook(tracer, \"cr\")\nlocal h, m = debug.gethook()\nassert(h == tracer)\nassert(m == \"cr\")\ndebug.sethook(nil)\nlocal h2 = debug.gethook()\nassert(h2 == nil)\nreturn 1";
        let res = run_code(code);
        assert_eq!(res.to_string(), "1");
    }

    #[test]
    fn test_vm_debug_hook_line_events() {
        let code = "local lines = {}\nlocal function tracer(ev)\n  if ev == \"line\" then\n    local info = debug.getinfo(2)\n    lines[#lines + 1] = info.currentline\n  end\nend\ndebug.sethook(tracer, \"l\")\nlocal a = 1\nlocal b = 2\nlocal c = a + b\ndebug.sethook(nil)\nreturn table.concat(lines, \",\")";
        let res = run_debug(code);
        assert_eq!(res.to_string(), "9,10,11,12");
    }

    #[test]
    fn test_vm_debug_hook_native_call() {
        // `tostring(i)` over a loop variable cannot be constant-folded, so
        // the native call really executes. Lua-consistent sequence:
        // sethook-return, two tostring pairs, clearing call.
        let code = "local events = {}\nlocal function tracer(ev)\n  events[#events + 1] = ev\nend\ndebug.sethook(tracer, \"cr\")\nlocal x = \"\"\nfor i = 1, 2 do x = tostring(i) end\ndebug.sethook(nil)\nassert(x == \"2\")\nreturn table.concat(events, \",\")";
        let res = run_code(code);
        assert_eq!(res.to_string(), "return,call,return,call,return,call");
    }

    #[test]
    fn test_vm_debug_hook_pcall_path() {
        // As in Lua 5.4: sethook-return, pcall-call, add-call,
        // add-return, pcall-return, clearing call.
        let code = "local events = {}\nlocal function tracer(ev)\n  events[#events + 1] = ev\nend\nlocal function add(x, y)\n  return x + y\nend\ndebug.sethook(tracer, \"cr\")\nlocal ok, v = pcall(add, 1, 2)\ndebug.sethook(nil)\nassert(ok == true)\nassert(v == 3)\nreturn table.concat(events, \",\")";
        let res = run_code(code);
        assert_eq!(res.to_string(), "return,call,call,return,return,call");
    }

    #[test]
    fn test_vm_debug_hook_tail_call_event() {
        // As in Lua 5.4: sethook-return, initial call, one "tail
        // call" per replaced frame, final return, clearing call.
        let code = "local events = {}\nlocal function tracer(ev)\n  events[#events + 1] = ev\nend\nlocal function loop(n, acc)\n  if n == 0 then return acc end\n  return loop(n - 1, acc + n)\nend\ndebug.sethook(tracer, \"cr\")\nlocal r = loop(3, 0)\ndebug.sethook(nil)\nassert(r == 6)\nreturn table.concat(events, \",\")";
        let res = run_code(code);
        assert_eq!(
            res.to_string(),
            "return,call,tail call,tail call,tail call,return,call"
        );
    }

    #[test]
    fn test_vm_require_bundled_libs() {
        let code_bit = "local bit = require(\"@neyuki/bit\")\nreturn bit.band(7, 3)";
        let res_bit = run_code(code_bit);
        assert_eq!(res_bit.to_string(), "3");

        let code_buf = "local buffer = require(\"@neyuki/buffer\")\nlocal b = buffer.create(8)\nbuffer.writeu8(b, 0, 99)\nreturn buffer.readu8(b, 0)";
        let res_buf = run_code(code_buf);
        assert_eq!(res_buf.to_string(), "99");

        let code_os =
            "local os = require(\"@neyuki/os\")\nlocal t = os.clock()\nassert(t >= 0)\nreturn 1";
        let res_os = run_code(code_os);
        assert_eq!(res_os.to_string(), "1");
    }

    #[test]
    fn test_vm_utf8_library() {
        let code = "local u = require(\"@neyuki/utf8\")\nlocal s = u.char(65, 66, 67)\nassert(u.len(s) == 3)\nassert(u.codepoint(s, 1, 1) == 65)\nreturn s";
        let res = run_code(code);
        assert_eq!(res.to_string(), "ABC");
    }

    #[test]
    fn test_vm_json_library() {
        let code = "local j = require(\"@neyuki/json\")\nlocal s = j.encode({ name = \"Neyuki\", version = 1 })\nlocal obj = j.decode(s)\nreturn obj.name";
        let res = run_code(code);
        assert_eq!(res.to_string(), "Neyuki");
    }

    #[test]
    fn test_vm_debug_library() {
        let code = "local d = require(\"@neyuki/debug\")\nlocal trace = d.traceback(\"test trace\")\nassert(string.find(trace, \"stack traceback\") != nil)\nreturn 1";
        let res = run_code(code);
        assert_eq!(res.to_string(), "1");
    }

    #[test]
    fn test_vm_coroutine_library() {
        let code = "local co = require(\"@neyuki/coroutine\")\nlocal f = function(x) return x * 10 end\nlocal t = co.create(f)\nlocal ok, val = co.resume(t, 5)\nassert(ok == true)\nassert(val == 50)\nassert(co.status(t) == \"dead\")\nreturn val";
        let res = run_code(code);
        assert_eq!(res.to_string(), "50");
    }

    #[test]
    fn test_vm_call_stack_overflow_caught() {
        // Infinite NON-tail recursion via Instruction::Call must be caught by
        // max_call_depth (configurable, default 16,384).
        let code = "local function f()\n  return f() + 1\nend\nreturn f()";
        let stmts = crate::compiler::compile_source(code).expect("syntax error");
        let proto = crate::compiler::compile_to_proto(&stmts);
        let mut vm = VM::new();
        vm.set_max_call_depth(128);
        let res = vm.execute(proto);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("call stack overflow"));
    }

    #[test]
    fn test_vm_recursion_limits_and_configurable_depth() {
        let code = "local function rec(n)\n  if n <= 1 then return 1 end\n  return rec(n - 1) + 1\nend\nreturn rec(1000)";
        let stmts = crate::compiler::compile_source(code).expect("syntax error");
        let proto = crate::compiler::compile_to_proto(&stmts);
        let mut vm = VM::new();
        assert_eq!(vm.max_call_depth(), DEFAULT_MAX_CALL_DEPTH);
        assert_eq!(DEFAULT_MAX_CALL_DEPTH, 512);
        let err = vm.execute(proto.clone());
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("call stack overflow"));

        vm.set_max_call_depth(2000);
        let res = vm
            .execute(proto)
            .expect("recursion up to 1000 frames must succeed with depth 2000");
        assert_eq!(res.to_string(), "1000");
    }

    #[test]
    fn test_nested_coroutine_resumes_count_toward_the_call_depth() {
        // Each coroutine runs on its own frame stack, so a coroutine that
        // resumes a new one forever overflowed the Rust stack and aborted.
        // Every resume nests the interpreter, whose debug-build frames are
        // large, so this runs on a stack as big as the CLI's.
        let run = || {
            for code in [
                "local function f() return select(2, coroutine.resume(coroutine.create(f))) end\nreturn f()",
                "local function f() return coroutine.wrap(f)() end\nreturn f()",
            ] {
                let stmts = crate::compiler::compile_source(code).expect("syntax error");
                let proto = crate::compiler::compile_to_proto(&stmts);
                let mut vm = VM::new();
                let text = match vm.execute(proto) {
                    Ok(v) => v.to_string(),
                    Err(e) => e,
                };
                assert!(
                    text.contains("call stack overflow: exceeded maximum call depth of 512"),
                    "{}",
                    text
                );
            }
        };
        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(run)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_natives_calling_natives_count_toward_the_call_depth() {
        // `__call = pcall` re-enters the interpreter from native to native
        // without pushing a frame; it recursed until the Rust stack
        // overflowed and the process aborted.
        let run = || {
            let code =
                "local t = setmetatable({}, {__call = pcall})\nreturn select(-1, pcall(t, t))";
            let stmts = crate::compiler::compile_source(code).expect("syntax error");
            let proto = crate::compiler::compile_to_proto(&stmts);
            let mut vm = VM::new();
            let text = match vm.execute(proto) {
                Ok(v) => v.to_string(),
                Err(e) => e,
            };
            assert!(text.contains("call stack overflow"), "{}", text);
        };
        std::thread::Builder::new()
            .stack_size(256 * 1024 * 1024)
            .spawn(run)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_vm_string_rep_limits() {
        // Empty string with giant n should be caught without allocating GBs of memory
        let code = "return string.rep(\"\", 2000000000)";
        let stmts = crate::compiler::compile_source(code).expect("syntax error");
        let proto = crate::compiler::compile_to_proto(&stmts);
        let mut vm = VM::new();
        let res = vm.execute(proto);
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("exceeds maximum limit"));
    }

    #[test]
    fn test_vm_crypto_library() {
        let code = "local c = require(\"@neyuki/crypto\")\nlocal h = c.hash(\"hello world\", \"sha256\")\nreturn h";
        let res = run_code(code);
        // sha256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
        assert_eq!(
            res.to_string(),
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    #[test]
    fn test_vm_coroutine_real_yield_resume() {
        let code = "local co = require(\"@neyuki/coroutine\")\n\
local f = function(x)\n\
  local y = co.yield(x + 10)\n\
  return y * 2\n\
end\n\
local t = co.create(f)\n\
assert(co.status(t) == \"suspended\")\n\
local ok1, val1 = co.resume(t, 5)\n\
assert(ok1 == true)\n\
assert(val1 == 15)\n\
assert(co.status(t) == \"suspended\")\n\
local ok2, val2 = co.resume(t, 20)\n\
assert(ok2 == true)\n\
assert(val2 == 40)\n\
assert(co.status(t) == \"dead\")\n\
return val2";
        let res = run_code(code);
        assert_eq!(res.to_string(), "40");
    }

    #[test]
    fn test_vm_debug_getinfo_and_traceback() {
        let code = "local d = require(\"@neyuki/debug\")\n\
local info = d.getinfo(d.traceback)\n\
assert(info.what == \"native\")\n\
assert(info.name == \"debug.traceback\")\n\
local tb = d.traceback(\"error message\")\n\
assert(string.len(tb) > 10)\n\
return 1";
        let res = run_code(code);
        assert_eq!(res.to_string(), "1");
    }

    #[test]
    fn test_vm_varargs() {
        let code = "local function sum(first, ...)\n\
  local second = ...\n\
  return first + second\n\
end\n\
return sum(10, 25, 99)";
        let res = run_code(code);
        assert_eq!(res.to_string(), "35");
    }

    #[test]
    fn test_vm_xpcall_with_args_and_custom_handler() {
        let code = "local function failing(a, b)\n\
  error(tostring(a + b))\n\
end\n\
local function handler(err)\n\
  return \"caught: \" .. tostring(err)\n\
end\n\
local ok, msg = xpcall(failing, handler, 20, 30)\n\
assert(ok == false)\n\
assert(msg == \"caught: 50\")\n\
return msg";
        let res = run_code(code);
        assert_eq!(res.to_string(), "caught: 50");
    }

    #[test]
    fn test_vm_table_sort_custom_comparator() {
        let code = "local t = { 3, 1, 4, 1, 5, 9 }\n\
table.sort(t, function(a, b) return a > b end)\n\
assert(t[1] == 9)\n\
assert(t[2] == 5)\n\
assert(t[6] == 1)\n\
return t[1]";
        let res = run_code(code);
        assert_eq!(res.to_string(), "9");
    }

    #[test]
    fn test_vm_string_format() {
        let code = "local s = string.format(\"hello %s, number %d, hex %x\", \"world\", 42, 255)\n\
return s";
        let res = run_code(code);
        assert_eq!(res.to_string(), "hello world, number 42, hex ff");
    }

    #[test]
    fn test_vm_string_sub_utf8_safe() {
        let code = "local s = \"xin chào thế giới\"\n\
local sub1 = string.sub(s, 1, 6)\n\
return sub1";
        let res = run_code(code);
        assert!(!res.to_string().is_empty());
    }

    #[test]
    fn test_vm_tonumber_bases() {
        let code = "assert(tonumber(\"1010\", 2) == 10)\n\
assert(tonumber(\"ff\", 16) == 255)\n\
assert(tonumber(\"0xFF\") == 255)\n\
assert(tonumber(\"  42  \") == 42)\n\
assert(tonumber(\"-0x10\") == -16)\n\
assert(tonumber(\"z\", 36) == 35)\n\
return tonumber(\"1010\", 2)";
        let res = run_code(code);
        assert_eq!(res.to_string(), "10");
    }

    #[test]
    fn test_vm_coroutine_no_magic_yield_collision() {
        let code = "local co = coroutine.create(function()\n\
    error(\"__NEYUKI_COROUTINE_YIELD__\")\n\
end)\n\
local ok, err = coroutine.resume(co)\n\
assert(ok == false)\n\
assert(coroutine.status(co) == \"dead\")\n\
return coroutine.status(co)";
        let res = run_code(code);
        assert_eq!(res.to_string(), "dead");
    }

    #[test]
    fn test_vm_open_upvalue_mutation() {
        let code = "local x = 1\n\
local f = function() return x end\n\
x = 2\n\
return f()";
        let res = run_code(code);
        assert_eq!(res.to_string(), "2");
    }

    #[test]
    fn test_vm_numeric_for_zero_step_rejected() {
        let code = "for i = 1, 10, 0 do end";
        let res = std::panic::catch_unwind(|| {
            run_code(code);
        });
        assert!(res.is_err());
    }

    #[test]
    fn test_vm_missing_param_nil_initialization() {
        let code = "local function f(a, b, c)\n\
  return c == nil\n\
end\n\
return f(1)";
        let res = run_code(code);
        assert_eq!(res.to_string(), "true");
    }

    #[test]
    fn test_vm_direct_table_generic_for() {
        let code = "local t = { 10, 20, 30 }\n\
local sum = 0\n\
for i, v in t do\n\
  sum = sum + v\n\
end\n\
return sum";
        let res = run_code(code);
        assert_eq!(res.to_string(), "60");
    }

    #[test]
    fn test_vm_require_path_traversal_blocked() {
        let code = "return require(\"../secret.nyk\")";
        let res = std::panic::catch_unwind(|| {
            run_code(code);
        });
        assert!(res.is_err());
    }

    #[test]
    fn test_vm_return_forwards_every_value() {
        let code = "local function pair()
  return 1, 2
end
local function forward()
  return pair()
end
local a, b = forward()
return a + b";
        assert_eq!(run_code(code).to_string(), "3");
    }

    #[test]
    fn test_vm_missing_returns_pad_with_nil() {
        // The callee returns one value where two were asked for; the second
        // must be nil rather than whatever the register held.
        let code = "local function one()
  return 7
end
local used = 99
local a, b = one()
return b == nil";
        assert_eq!(run_code(code).to_string(), "true");
    }

    #[test]
    fn test_vm_varargs_spread_into_call_and_table() {
        let code = "local table = require(\"@neyuki/table\")
local function count(...)
  local args = {...}
  return #args
end
local function relay(...)
  return count(...)
end
return relay(1, 2, 3) + #{table.unpack({4, 5})}";
        assert_eq!(run_code(code).to_string(), "5");
    }

    #[test]
    fn test_vm_block_local_captured_by_closure() {
        // The closure's upvalue must survive the register being reused after
        // the block it was declared in ends.
        let code = "local function make()
  local t = {}
  if (true) then
    local hidden = 11
    t.get = function() return hidden end
  end
  return t
end
return make().get()";
        assert_eq!(run_code(code).to_string(), "11");
    }

    #[test]
    fn test_vm_string_gmatch() {
        let code = "local string = require(\"@neyuki/string\")
local parts = {}
for piece in string.gmatch(\"a=1&b=2\", \"[^&]+\") do
  parts[#parts + 1] = piece
end
return parts[1] .. \",\" .. parts[2]";
        assert_eq!(run_code(code).to_string(), "a=1,b=2");
    }

    #[test]
    fn test_vm_requires_bundled_http_module() {
        let code = "local http = require(\"@neyuki/http\")
local path, params = http.parseQuery(\"/hello?name=you\")
return path .. \" \" .. params.name .. \" \" .. http.encode(\"a b\")";
        assert_eq!(run_code(code).to_string(), "/hello you a%20b");
    }

    #[test]
    fn test_vm_require_caches_modules() {
        let code = "return require(\"@neyuki/io\") == require(\"@neyuki/io\")";
        assert_eq!(run_code(code).to_string(), "true");
    }

    #[test]
    fn test_inliner_param_reassignment_with_branches() {
        let code = "local function abs(x)
            if x < 0 then
                x = -x
            end
            return x
        end
        return abs(5) + abs(-5) + abs(0)";
        assert_eq!(run_code(code).to_string(), "10");
    }

    #[test]
    fn test_inliner_nested_helper_resolution() {
        let code = "local function outer(n)
            local function helper(x)
                return x * 2 + 1
            end
            return helper(n) + helper(n + 1)
        end
        return outer(3)";
        assert_eq!(run_code(code).to_string(), "16");
    }

    #[test]
    fn test_vm_shl_bit_overflow_past_i128() {
        let code = "return ((1 << 62) << 70) == (1 << 132)";
        assert_eq!(run_code(code).to_string(), "true");
    }

    #[test]
    fn test_vm_bigint_mul_neg1_normalization() {
        let code = "local prod = (1 << 63) * -1
        local min_int = -9223372036854775807 - 1
        local t = {}
        t[prod] = \"found\"
        return (prod == min_int) and (t[min_int] == \"found\")";
        assert_eq!(run_code(code).to_string(), "true");
    }

    #[test]
    fn test_vm_shl_huge_shift_does_not_panic() {
        let code = "local x = 1
        local y = 4294967296
        local ok, err = pcall(function() return x << y end)
        return not ok";
        assert_eq!(run_code(code).to_string(), "true");
    }

    #[test]
    fn test_licm_metamethod_call_count_preserved() {
        let code = "local count = 0
        local mt = {
            __add = function(a, b)
                count += 1
                return a.val + b.val
            end
        }
        local a = { val = 10 }
        setmetatable(a, mt)
        local b = { val = 20 }
        setmetatable(b, mt)
        local sum = 0
        for i = 1, 5 do
            local x = a + b
            sum += x
        end
        return sum == 150 and count == 5";
        assert_eq!(run_code(code).to_string(), "true");
    }

    #[test]
    fn test_licm_does_not_hoist_reads_of_captured_locals() {
        // Without guarding captured uses in LICM, reading `done` in the loop
        // header is hoisted into a preheader Move copy and the loop ignores
        // updates written via SETUPVAL inside `setDone`, a metamethod, or a task.
        let code = r#"
        local function loopHeaderCapture()
            local done = false
            local function setDone()
                done = true
            end
            local iters = 0
            while not done and iters < 10 do
                iters += 1
                if iters == 2 then setDone() end
            end
            return iters == 2 and done == true
        end

        local function loopBodyCapture()
            local val = 1
            local function bump()
                val += 10
            end
            local sum = 0
            for i = 1, 3 do
                local snapshot = val
                sum += snapshot
                bump()
            end
            return sum == (1 + 11 + 21)
        end

        local function metamethodCapture()
            local flag = false
            local obj = setmetatable({}, {
                __index = function()
                    flag = true
                    return 1
                end
            })
            local steps = 0
            while not flag and steps < 5 do
                local _ = obj.x
                steps += 1
            end
            return steps == 1 and flag == true
        end

        return loopHeaderCapture() and loopBodyCapture() and metamethodCapture()
        "#;
        assert_eq!(run_code(code).to_string(), "true");
    }

    fn compile_src(src: &str) -> Proto {
        let mut parser = Parser::new(src);
        let (stmts, _pool) = parser.parse_program().expect("parse failed");
        compile_to_proto(&stmts)
    }

    #[test]
    fn test_vm_limits_instruction_budget_survives_pcall() {
        let proto = compile_src("pcall(function() while true do end end)");
        let mut vm = VM::new();
        vm.set_limits(ExecutionLimits::new().with_instruction_budget(500));
        let err = vm.execute(proto).unwrap_err();
        assert!(
            err.contains("instruction budget of 500 exhausted"),
            "got: {err}"
        );
    }

    #[test]
    fn test_vm_limits_heap_limit_trips_on_allocation() {
        let proto = compile_src(
            "local t = {}
            for i = 1, 5000 do
                t[i] = { a = i, b = i + 1, c = i + 2 }
            end",
        );
        let mut vm = VM::new();
        vm.set_limits(ExecutionLimits::new().with_heap_limit(8192));
        let err = vm.execute(proto).unwrap_err();
        assert!(
            err.contains("heap limit of 8192 bytes exceeded"),
            "got: {err}"
        );
    }

    #[test]
    fn test_vm_limits_deadline_terminates_loop() {
        let proto = compile_src("while true do end");
        let mut vm = VM::new();
        vm.set_limits(ExecutionLimits::new().with_timeout(std::time::Duration::from_millis(20)));
        let err = vm.execute(proto).unwrap_err();
        assert!(err.contains("deadline exceeded"), "got: {err}");
    }

    #[test]
    fn test_vm_limits_interrupt_from_background_thread() {
        let proto = compile_src("while true do end");
        let mut vm = VM::new();
        let handle = vm.limits.interrupt_handle();
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            handle.store(true, std::sync::atomic::Ordering::Release);
        });
        let err = vm.execute(proto).unwrap_err();
        t.join().unwrap();
        assert_eq!(err, "execution interrupted");
    }

    #[test]
    fn test_vm_permissions_sandbox_blocks_privileged_calls() {
        let mut vm = VM::new();
        vm.set_permissions(Permissions::deny_all());

        let env_proto = compile_src("return os.getenv('PATH')");
        let err = vm.execute(env_proto).unwrap_err();
        assert!(
            err.contains("permission denied: environment access"),
            "got: {err}"
        );

        let run_proto = compile_src("return os.run('echo hi')");
        let err = vm.execute(run_proto).unwrap_err();
        assert!(
            err.contains("permission denied: subprocess execution"),
            "got: {err}"
        );

        let fs_proto =
            compile_src("local fs = require('@neyuki/fs'); return fs.exists('Cargo.toml')");
        let err = vm.execute(fs_proto).unwrap_err();
        assert!(err.contains("permission denied: read access"), "got: {err}");

        let net_proto = compile_src(
            "local http = require('@neyuki/http'); return http.get('http://example.com')",
        );
        let err = vm.execute(net_proto).unwrap_err();
        assert!(
            err.contains("permission denied: network access"),
            "got: {err}"
        );

        let ffi_proto = compile_src("local ffi = require('@neyuki/ffi'); return ffi.alloc(8)");
        let err = vm.execute(ffi_proto).unwrap_err();
        assert!(err.contains("permission denied: FFI"), "got: {err}");

        // Granting specific env var succeeds while others remain blocked
        vm.set_permissions(Permissions::deny_all().with_env(
            crate::vm::permissions::PermissionScope::AllowList(vec!["PATH".into()]),
        ));
        let allowed_env = compile_src("return type(os.getenv('PATH'))");
        assert!(vm.execute(allowed_env).is_ok());
    }

    #[test]
    fn test_super_instructions_addk_and_compare_jump_emitted() {
        // Constants outside the i16 immediate range use the constant-pool forms.
        let proto = compile_src(
            "local function f(x)
                if x < 100000 then
                    return x + 100000
                end
                return x * 200000 - 300000
            end
            return f(4) + f(10)",
        );
        let child = &proto.protos[0];
        let has_ltk = child
            .instructions
            .iter()
            .any(|i| matches!(i, crate::bytecode::instruction::Instruction::LtK { .. }));
        let has_addk = child
            .instructions
            .iter()
            .any(|i| matches!(i, crate::bytecode::instruction::Instruction::AddK { .. }));
        let has_subk = child
            .instructions
            .iter()
            .any(|i| matches!(i, crate::bytecode::instruction::Instruction::SubK { .. }));
        let has_mulk = child
            .instructions
            .iter()
            .any(|i| matches!(i, crate::bytecode::instruction::Instruction::MulK { .. }));
        assert!(has_ltk && has_addk && has_subk && has_mulk);

        let mut vm = VM::new();
        let res = vm.execute(proto).unwrap();
        assert_eq!(res.to_string(), "200014");
    }

    #[test]
    fn test_quickening_deopts_on_type_change_and_overflow() {
        let code = "local function step(x)
            if x < 100 then
                return x + 1
            end
            return x + 1
        end
        local a = step(5)
        local b = step(2.5)
        local c = step(9223372036854775807)
        return a == 6 and b == 3.5 and c == 9223372036854775808";
        assert_eq!(run_code(code).to_string(), "true");
    }

    #[test]
    fn test_inline_cache_globals_and_table_fields() {
        let code = "global g = 10
        local t = { x = 1, y = 2 }
        local sum = 0
        for i = 1, 10 do
            if i == 5 then
                g = 20
                t.z = 3
                t.w = 4
                t.u = 5
            end
            sum += g + t.x + t.y
            t.x += 1
        end
        return sum == 235 and t.x == 11";
        assert_eq!(run_code(code).to_string(), "true");
    }

    #[test]
    fn test_super_instructions_serialize_disasm_roundtrip() {
        let proto = compile_src(
            "local function cmp(x)
                if x == 5 then return x + 2 end
                if x != 1 then return x - 1 end
                return x * 3
            end
            return cmp(5) + cmp(2) + cmp(1)",
        );
        let bytes = crate::bytecode::serialize(&proto);
        let decoded = crate::bytecode::deserialize(&bytes).expect("deserialize failed");
        let mut vm = VM::new();
        let res = vm.execute(decoded).expect("execute failed");
        assert_eq!(res.to_string(), "11");
    }

    #[test]
    fn test_vm_limits_deadline_terminates_call_heavy_loop() {
        let proto = compile_src(
            "local function spin(n)
                return spin(n + 1)
            end
            return spin(0)",
        );
        let mut vm = VM::new();
        vm.set_limits(ExecutionLimits::new().with_timeout(std::time::Duration::from_millis(20)));
        let err = vm.execute(proto).unwrap_err();
        assert!(err.contains("deadline exceeded"), "got: {err}");
    }

    #[test]
    fn test_inline_cache_table_deopt_and_global_redefine() {
        let code = "
        global gval = 10
        local function read_field(tbl)
            return tbl.x
        end
        local function read_global()
            return gval
        end

        local a = { x = 1, y = 2 }
        local b = { y = 9, x = 42 }
        assert(read_field(a) == 1)
        assert(read_field(a) == 1)
        -- Mutate shape: delete cached field, then attach __index metatable while IC is hot
        a.x = nil
        assert(read_field(a) == nil)
        setmetatable(a, { __index = { x = 77 } })
        assert(read_field(a) == 77)
        -- Different shape / different table instance at same callsite
        assert(read_field(b) == 42)

        -- Redefine global while GetGlobal inline cache is hot
        assert(read_global() == 10)
        assert(read_global() == 10)
        gval = 'redefined'
        assert(read_global() == 'redefined')
        gval = nil
        assert(read_global() == nil)
        return true";
        assert_eq!(run_code(code).to_string(), "true");
    }

    #[test]
    fn test_deep_clone_table_isolates_nested_tables_and_metatables() {
        let mut inner = VmTable::new();
        inner.fields.insert(StrRef::from("sep"), Value::str("/"));
        let mut mt = VmTable::new();
        mt.fields.insert(StrRef::from("tag"), Value::int(1));
        let mut root = VmTable::new();
        root.fields.insert(
            StrRef::from("path"),
            crate::vm::heap::VmHeap::alloc_table_untracked(inner),
        );
        root.metatable = Some(Rc::new(RefCell::new(mt)));

        assert!(table_needs_deep_clone(&root));
        let clone1 = deep_clone_table(&root);
        let clone2 = deep_clone_table(&root);

        // Mutate nested table and metatable on clone1; clone2 and root must be untouched.
        if let Some(p1) = clone1
            .borrow()
            .fields
            .get("path")
            .and_then(|v| v.as_table())
        {
            p1.borrow_mut()
                .fields
                .insert(StrRef::from("sep"), Value::str("\\"));
        }
        clone1
            .borrow()
            .metatable
            .as_ref()
            .unwrap()
            .borrow_mut()
            .fields
            .insert(StrRef::from("tag"), Value::int(99));

        let sep2 = match clone2
            .borrow()
            .fields
            .get("path")
            .and_then(|v| v.as_table())
        {
            Some(p2) => p2.borrow().fields.get("sep").unwrap().to_string(),
            None => panic!("missing nested table"),
        };
        let tag2 = clone2
            .borrow()
            .metatable
            .as_ref()
            .unwrap()
            .borrow()
            .fields
            .get("tag")
            .unwrap()
            .clone();
        assert_eq!(sep2, "/");
        assert_eq!(tag2, Value::int(1));
    }

    #[test]
    fn test_immediate_ops_overflow_floats_and_metamethods() {
        let code = r#"
            -- Overflow to BigInt via AddI, SubI, MulI, ShlI
            local max64 = 9223372036854775807
            local min64 = -9223372036854775807 - 1
            assert(max64 + 1 == 9223372036854775808)
            assert(min64 - 1 == -9223372036854775809)
            assert(max64 * 2 == 18446744073709551614)
            assert((max64 << 2) == max64 * 4)
            assert(((1 << 62) << 4) == (1 << 66))
            assert(((-8) >> 1) == -4)
            assert(((1 << 70) >> 60) == 1024)

            -- Float operands with immediate opcodes
            local f = 2.5
            assert(f + 2 == 4.5)
            assert(f - 1 == 1.5)
            assert(f * 4 == 10.0)
            local two_f = 2.0
            assert(two_f == 2 and not (two_f != 2))
            assert(two_f < 3 and two_f <= 2 and two_f > 1 and two_f >= 2)
            local nan = 0.0 / 0.0
            assert(not (nan == 0) and (nan != 0))
            assert(not (nan < 0) and not (nan <= 0) and not (nan > 0) and not (nan >= 0))

            -- Flipped comparisons with LHS immediate
            local n = 5
            assert(3 < n and 5 <= n and 7 > n and 5 >= n and 5 == n and 4 != n)
            assert(not (6 < n) and not (6 <= n) and not (4 > n) and not (4 >= n))

            -- Metamethods preserve exact operand order (including 1 + tbl and 10 < tbl)
            local order = {}
            local mt = {
                __add = function(a, b)
                    if type(a) == "table" then
                        order[#order + 1] = "add:t," .. tostring(b)
                        return a.v + b
                    else
                        order[#order + 1] = "add:" .. tostring(a) .. ",t"
                        return a - b.v
                    end
                end,
                __sub = function(a, b) return a.v - b end,
                __mul = function(a, b) return a.v * b end,
                __lt = function(a, b)
                    if type(a) == "table" then
                        order[#order + 1] = "lt:t," .. tostring(b)
                        return a.v < b
                    else
                        order[#order + 1] = "lt:" .. tostring(a) .. ",t"
                        return a < b.v
                    end
                end,
                __le = function(a, b)
                    if type(a) == "table" then
                        order[#order + 1] = "le:t," .. tostring(b)
                        return a.v <= b
                    else
                        order[#order + 1] = "le:" .. tostring(a) .. ",t"
                        return a <= b.v
                    end
                end,
            }
            local t = setmetatable({ v = 10 }, mt)
            assert(t + 5 == 15)
            assert(5 + t == -5)
            assert(t - 3 == 7)
            assert(t * 4 == 40)
            assert(t < 20 and not (t < 5))
            assert(5 < t and not (20 < t))
            assert(t <= 10 and 10 <= t)
            assert(order[1] == "add:t,5")
            assert(order[2] == "add:5,t")
            assert(order[3] == "lt:t,20")
            assert(order[5] == "lt:5,t")
            return true
        "#;
        assert_eq!(run_code(code).to_string(), "true");
    }

    #[test]
    fn test_codegen_immediate_ops_and_coalescing() {
        let loop_code = r#"
            local sum = 0
            local i = 0
            while i < 10000 do
                sum = sum + (i * 3 - (i >> 1))
                i = i + 1
            end
            return sum
        "#;
        let stmts = crate::compiler::compile_source(loop_code).expect("parse");
        let proto = crate::compiler::try_compile_to_proto(&stmts).expect("compile");
        assert!(
            proto
                .instructions
                .iter()
                .any(|i| matches!(i, Instruction::LtI { imm: 10000, .. }))
        );
        assert!(
            proto
                .instructions
                .iter()
                .any(|i| matches!(i, Instruction::MulI { imm: 3, .. }))
        );
        assert!(
            proto
                .instructions
                .iter()
                .any(|i| matches!(i, Instruction::ShrI { imm: 1, .. }))
        );
        assert!(
            proto
                .instructions
                .iter()
                .any(|i| matches!(i, Instruction::AddI { imm: 1, .. }))
        );
        assert!(
            !proto
                .instructions
                .iter()
                .any(|i| matches!(i, Instruction::Move { .. })),
            "expected self-update Move elimination in loop, got:\n{}",
            crate::bytecode::disasm::disassemble_proto(&proto, 0)
        );
        let mut vm = VM::new();
        assert_eq!(vm.execute(proto).expect("exec").to_string(), "124990000");

        let fib_code = r#"
            function fib(n)
                if n <= 1 then return n end
                return fib(n - 1) + fib(n - 2)
            end
            return fib(18)
        "#;
        let stmts = crate::compiler::compile_source(fib_code).expect("parse");
        let proto = crate::compiler::try_compile_to_proto(&stmts).expect("compile");
        let fib_proto = &proto.protos[0];
        assert!(
            fib_proto
                .instructions
                .iter()
                .any(|i| matches!(i, Instruction::LeI { imm: 1, .. }))
        );
        assert!(
            fib_proto
                .instructions
                .iter()
                .any(|i| matches!(i, Instruction::SubI { imm: 1, .. }))
        );
        assert!(
            fib_proto
                .instructions
                .iter()
                .any(|i| matches!(i, Instruction::SubI { imm: 2, .. }))
        );
        // Only the 1st recursive call needs to save its result out of the call window.
        let move_count = fib_proto
            .instructions
            .iter()
            .filter(|i| matches!(i, Instruction::Move { .. }))
            .count();
        assert_eq!(
            move_count,
            1,
            "expected 2nd call result coalesced without MOVE, got:\n{}",
            crate::bytecode::disasm::disassemble_proto(fib_proto, 0)
        );
        let mut vm = VM::new();
        assert_eq!(vm.execute(proto).expect("exec").to_string(), "2584");
    }

    #[test]
    fn test_captured_local_mutated_by_metamethod_and_loop() {
        let code = r#"
            local x = 1
            local t = setmetatable({}, {
                __add = function(_, _)
                    x = 10
                    return 0
                end,
            })
            local side = t + 1
            local after_add = x + 1 + side

            local done = false
            local flips = 0
            local step = function()
                flips = flips + 1
                if flips >= 3 then
                    done = true
                end
            end
            while not done do
                step()
            end

            return tostring(after_add) .. ":" .. tostring(flips)
        "#;
        assert_eq!(run_code(code).to_string(), "11:3");
    }
}
