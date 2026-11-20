use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use num_integer::Integer as _;

use crate::bytecode::instruction::{Instruction, MULTRET};
use crate::bytecode::proto::Proto;
use crate::dap::DapSession;
use crate::vm::frame::CallFrame;
use crate::vm::gc::GcTracker;
use crate::vm::hash::{FxHashMap, new_map, with_capacity};
use crate::vm::libs::{
    create_bit_lib, create_buffer_lib, create_coroutine_lib, create_debug_lib, create_json_lib,
    create_math_lib, create_os_lib, create_string_lib, create_table_lib, create_task_lib,
    create_utf8_lib,
};
use crate::vm::ops;
use crate::vm::table::TableKey;
use crate::vm::value::{NativeDef, StrRef, Upvalue, Value, VmClosure, VmTable};

pub const DEFAULT_MAX_CALL_DEPTH: usize = 512;

/// The interpreter loop only runs while the frame stack is deeper than its
/// target depth, so there is always a running frame inside it.
const RUNNING_FRAME: &str = "the interpreter loop has a running frame";

/// How many callable tables a call follows through `__call` before giving
/// up, like the `__index` chain limit.
const MAX_CALL_CHAIN: usize = 100;

/// A register of the running frame. The interpreter loop only calls this
/// with indices below `base + max_registers` of a verified prototype, and
/// sizes the stack to cover that window before dispatching, so the bounds
/// check is skipped.
#[inline(always)]
fn slot(stack: &[Value], idx: usize) -> &Value {
    debug_assert!(idx < stack.len());
    // SAFETY: see above; the caller upholds `idx < stack.len()`.
    unsafe { stack.get_unchecked(idx) }
}

#[inline(always)]
fn slot_mut(stack: &mut [Value], idx: usize) -> &mut Value {
    debug_assert!(idx < stack.len());
    // SAFETY: as for `slot`.
    unsafe { stack.get_unchecked_mut(idx) }
}

/// An open upvalue parked with a suspended stack: the register index it will
/// reopen at, and the upvalue itself (holding the value while parked).
pub type ParkedUpvalue = (usize, Rc<Upvalue>);

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
    fn name(self) -> &'static str {
        match self {
            HookEvent::Call => "call",
            HookEvent::TailCall => "tail call",
            HookEvent::Return => "return",
        }
    }

    fn wants(self, hook: &DebugHook) -> bool {
        match self {
            HookEvent::Call | HookEvent::TailCall => hook.on_call,
            HookEvent::Return => hook.on_return,
        }
    }
}

/// One line of a traceback: where a frame was when an error was raised.
#[derive(Clone, Debug)]
pub struct TraceFrame {
    pub function: Option<String>,
    pub source: Option<Rc<str>>,
    pub line: u32,
}

impl std::fmt::Display for TraceFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = self.source.as_deref().unwrap_or("?");
        match self.function.as_deref() {
            None | Some("main") => write!(f, "{}:{} in main chunk", source, self.line),
            Some(name) => write!(f, "{}:{} in function '{}'", source, self.line, name),
        }
    }
}

pub struct VM {
    pub stack: Vec<Value>,
    pub frames: Vec<CallFrame>,
    pub globals: FxHashMap<StrRef, Value>,
    /// Bumped on every global write: `GetImport` slots resolved under an
    /// older epoch re-resolve instead of trusting a stale value.
    pub globals_epoch: u64,
    pub gc: GcTracker,
    /// Coroutine states by id, held weakly: each handle table's `co_state`
    /// field owns its state, so dropping the last handle frees a suspended
    /// coroutine by refcounting.
    pub co_states: HashMap<usize, Weak<RefCell<CoroutineState>>>,
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
    returned: Vec<Value>,
    /// The call stack where the error now unwinding was raised, innermost
    /// frame first. Cleared when `try`, `pcall` or `xpcall` catches it.
    pub error_trace: Option<Vec<TraceFrame>>,
    /// Maximum call depth allowed before raising call stack overflow.
    pub max_call_depth: usize,
    /// Tasks queued by the `task` library.
    pub tasks: crate::vm::libs::task::Scheduler,
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
    matches!(val, Value::Table(_) | Value::Closure(_) | Value::Buffer(_))
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
    match val {
        Value::Table(t) => Value::Table(deep_clone_table(&t.borrow())),
        other => {
            debug_assert!(
                !value_has_mutable_state(other),
                "unexpected mutable value in BuiltinTemplate: {}",
                other.type_name()
            );
            other.clone()
        }
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
            Some(Value::Table(t)) => t
                .borrow()
                .metatable
                .as_ref()
                .map(|m| m.borrow().clone())
                .unwrap_or_default(),
            _ => VmTable::new(),
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
            if let Value::Table(t) = val {
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
            globals: new_map(),
            globals_epoch: 0,
            gc: GcTracker::new(),
            co_states: HashMap::new(),
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
            dap: None,
            in_eval: false,
            open_upvalues: Vec::new(),
            top: 0,
            modules: HashMap::new(),
            script_dir: None,
            string_lib: Value::Nil,
            returned: Vec::new(),
            error_trace: None,
            max_call_depth: default_max_call_depth(),
            tasks: Default::default(),
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
                Value::Table(Rc::new(RefCell::new(g_table))),
            );

            for (name, key, tbl, needs_deep) in &tpl.libs {
                let lib = Value::Table(if *needs_deep {
                    deep_clone_table(tbl)
                } else {
                    Rc::new(RefCell::new(tbl.clone()))
                });
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

    pub fn set_global(&mut self, name: &str, value: Value) {
        self.globals.insert(StrRef::from(name), value);
        self.globals_epoch = self.globals_epoch.wrapping_add(1);
    }

    pub fn register_native(&mut self, def: &'static NativeDef) {
        self.set_global(def.name, Value::Native(def));
    }

    pub fn execute(&mut self, proto: Proto) -> Result<Value, String> {
        let closure = Rc::new(VmClosure {
            proto: Rc::new(proto),
            upvalues: Vec::new(),
        });
        self.stack.clear();
        self.frames.clear();
        self.open_upvalues.clear();
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
        self.stack.resize(max_reg + 1, Value::Nil);

        self.frames.push(CallFrame::new(closure, 0));
        self.catch_panics(|vm| vm.run())
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
    fn set_reg_grow(&mut self, idx: usize, val: Value) {
        if idx >= self.stack.len() {
            self.stack.resize(idx + 1, Value::Nil);
        }
        self.stack[idx] = val;
    }

    // ----- upvalues -------------------------------------------------------

    /// Moves every open upvalue at or above `from` off the stack and into
    /// its own cell. Called when the frame owning those registers ends.
    pub fn close_upvalues(&mut self, from: usize) {
        while let Some((idx, _)) = self.open_upvalues.last() {
            if *idx < from {
                break;
            }
            let (idx, uv) = self
                .open_upvalues
                .pop()
                .expect("the loop condition saw an open upvalue");
            let val = self.stack.get(idx).cloned().unwrap_or(Value::Nil);
            uv.close(val);
        }
    }

    /// Closes the open upvalue for stack slot `idx`, if there is one, so the
    /// next capture of that slot gets a fresh variable.
    fn close_upvalue_at(&mut self, idx: usize) {
        let pos = self.open_upvalues.partition_point(|(i, _)| *i < idx);
        if self.open_upvalues.get(pos).is_some_and(|(i, _)| *i == idx) {
            let (_, uv) = self.open_upvalues.remove(pos);
            let val = self.stack.get(idx).cloned().unwrap_or(Value::Nil);
            uv.close(val);
        }
    }

    /// Finds or creates the open upvalue for stack slot `idx`.
    fn capture_upvalue(&mut self, idx: usize) -> Rc<Upvalue> {
        if idx >= self.stack.len() {
            self.stack.resize(idx + 1, Value::Nil);
        }
        let pos = self.open_upvalues.partition_point(|(i, _)| *i < idx);
        if let Some((i, uv)) = self.open_upvalues.get(pos)
            && *i == idx
        {
            return uv.clone();
        }
        let uv = Upvalue::open(idx);
        self.open_upvalues.insert(pos, (idx, uv.clone()));
        uv
    }

    /// Detaches every open upvalue from the current stack so the stack can be
    /// put aside (a coroutine switch). Each keeps its value in its cell until
    /// `unpark_upvalues` reattaches it.
    pub fn park_upvalues(&mut self) -> Vec<ParkedUpvalue> {
        let parked = std::mem::take(&mut self.open_upvalues);
        for (idx, uv) in &parked {
            let val = self.stack.get(*idx).cloned().unwrap_or(Value::Nil);
            uv.close(val);
        }
        parked
    }

    /// Reattaches upvalues parked by `park_upvalues` to the (now current)
    /// stack they belong to, copying any writes made meanwhile back into
    /// their registers.
    pub fn unpark_upvalues(&mut self, parked: Vec<ParkedUpvalue>) {
        for (idx, uv) in &parked {
            let val = std::mem::replace(&mut *uv.closed.borrow_mut(), Value::Nil);
            self.set_reg_grow(*idx, val);
            uv.location.set(Some(*idx));
        }
        self.open_upvalues = parked;
    }

    #[inline]
    pub fn upvalue_get(&self, uv: &Upvalue) -> Value {
        match uv.location.get() {
            Some(idx) => self.stack.get(idx).cloned().unwrap_or(Value::Nil),
            None => uv.closed.borrow().clone(),
        }
    }

    #[inline]
    pub fn upvalue_set(&mut self, uv: &Upvalue, val: Value) {
        match uv.location.get() {
            Some(idx) => self.set_reg_grow(idx, val),
            None => *uv.closed.borrow_mut() = val,
        }
    }

    // ----- calls ----------------------------------------------------------

    /// Runs a module's top-level code and returns its value, leaving the
    /// frames and registers of whatever called `require` untouched.
    pub fn execute_module(&mut self, proto: Proto) -> Result<Value, String> {
        self.execute_module_rc(Rc::new(proto))
    }

    /// Like `execute_module`, sharing an already-compiled `Rc<Proto>`.
    pub fn execute_module_rc(&mut self, proto: Rc<Proto>) -> Result<Value, String> {
        let closure = Rc::new(VmClosure {
            proto,
            upvalues: Vec::new(),
        });
        let depth = self.frames.len();
        let prev_stack_len = self.stack.len();
        let base = prev_stack_len + 1;
        let needed = base + closure.proto.max_registers as usize + 1;
        if needed >= self.stack.len() {
            self.stack.resize(needed + 1, Value::Nil);
        }
        self.frames.push(CallFrame::new(closure, base));
        let result = self.run_to_depth(depth);
        self.frames.truncate(depth);
        // Upvalues captured by the module's closures live on in the closures
        // themselves; closing them here just moves the values across.
        self.close_upvalues(prev_stack_len);
        self.stack.truncate(prev_stack_len);
        result
    }

    /// Writes the results of a native (or `__call`) invocation back into the
    /// caller's registers, starting at the register that held the callee.
    fn store_call_results(&mut self, callee: u8, retc: u8, results: &[Value]) {
        let base = self.frames.last().map(|f| f.base).unwrap_or(0);
        let dest = base + callee as usize;
        let count = match retc {
            MULTRET => results.len(),
            0 => 1,
            n => n as usize,
        };
        if dest + count > self.stack.len() {
            self.stack.resize(dest + count, Value::Nil);
        }
        for i in 0..count {
            self.stack[dest + i] = results.get(i).cloned().unwrap_or(Value::Nil);
        }
        if retc == MULTRET {
            self.top = dest + count;
        }
    }

    pub fn call_function(&mut self, func: Value, args: &[Value]) -> Result<Vec<Value>, String> {
        self.native_calls += 1;
        let res = self.call_function_inner(func, args);
        self.native_calls -= 1;
        res
    }

    fn call_function_inner(&mut self, func: Value, args: &[Value]) -> Result<Vec<Value>, String> {
        // A native that calls a native (`__call = pcall`) nests the Rust
        // stack without pushing a frame, so re-entries are bounded too.
        if self.call_depth() >= self.max_call_depth || self.native_calls > self.max_call_depth {
            return Err(format!(
                "call stack overflow: exceeded maximum call depth of {}",
                self.max_call_depth
            ));
        }
        match func {
            Value::Native(def) => {
                self.fire_hook(HookEvent::Call)?;
                match (def.func)(self, args) {
                    Ok(results) => {
                        self.fire_hook(HookEvent::Return)?;
                        Ok(results)
                    }
                    Err(err) => Err(err),
                }
            }
            Value::Closure(c) => {
                let depth = self.frames.len();
                let prev_stack_len = self.stack.len();
                let base = prev_stack_len + 1;
                let needed = base + c.proto.max_registers as usize + args.len() + 1;
                if needed >= self.stack.len() {
                    self.stack.resize(needed + 1, Value::Nil);
                }
                for (i, arg) in args.iter().enumerate() {
                    self.stack[base + i] = arg.clone();
                }
                let num_params = c.proto.num_params as usize;
                for i in args.len()..num_params {
                    self.stack[base + i] = Value::Nil;
                }
                let varargs = if args.len() > num_params {
                    args[num_params..].to_vec()
                } else {
                    Vec::new()
                };
                self.frames
                    .push(CallFrame::with_varargs(c.clone(), base, varargs));
                if let Err(err) = self.fire_hook(HookEvent::Call) {
                    if !self.is_yielding {
                        self.frames.truncate(depth);
                        self.close_upvalues(prev_stack_len);
                        self.stack.truncate(prev_stack_len);
                    }
                    return Err(err);
                }
                if let Err(err) =
                    crate::dap::check_break(self, c.proto.name.as_deref().unwrap_or("<anonymous>"))
                {
                    if !self.is_yielding {
                        self.frames.truncate(depth);
                        self.close_upvalues(prev_stack_len);
                        self.stack.truncate(prev_stack_len);
                    }
                    return Err(err);
                }
                self.returned.clear();
                match self.run_to_depth(depth) {
                    Ok(res) => {
                        // No return hook here: the callee reports its own
                        // return through its Return instruction (or
                        // fall-off-end), so firing again would double-report.
                        if !self.is_yielding {
                            self.close_upvalues(prev_stack_len);
                            self.stack.truncate(prev_stack_len);
                        }
                        // A closure can return several values, which callers
                        // such as a generic `for` over an iterator rely on.
                        let returned = std::mem::take(&mut self.returned);
                        if returned.is_empty() {
                            Ok(vec![res])
                        } else {
                            Ok(returned)
                        }
                    }
                    Err(err) => {
                        if !self.is_yielding {
                            self.frames.truncate(depth);
                            self.close_upvalues(prev_stack_len);
                            self.stack.truncate(prev_stack_len);
                        }
                        Err(err)
                    }
                }
            }
            Value::Table(_) => {
                // A `__call` handler may itself be a table with `__call`:
                // follow the chain, each table becoming the first argument
                // of the next. A loop would otherwise recurse forever,
                // growing the arguments each time.
                let mut callee = func;
                let mut call_args = args.to_vec();
                for _ in 0..MAX_CALL_CHAIN {
                    let handler = match &callee {
                        Value::Table(t) => t.borrow().metamethod("__call"),
                        _ => return self.call_function_inner(callee, &call_args),
                    };
                    let Some(handler) = handler else {
                        return Err(
                            "attempt to call a table value without __call metamethod".to_string()
                        );
                    };
                    call_args.insert(0, callee);
                    callee = handler;
                }
                Err("loop in __call chain".to_string())
            }
            _ => Err("attempt to call a non-function value".to_string()),
        }
    }

    fn get_binop_metamethod(&self, a: &Value, b: &Value, event: &str) -> Option<Value> {
        if let Value::Table(t) = a
            && let Some(h) = t.borrow().metamethod(event)
        {
            return Some(h);
        }
        if let Value::Table(t) = b
            && let Some(h) = t.borrow().metamethod(event)
        {
            return Some(h);
        }
        None
    }

    fn get_unop_metamethod(&self, val: &Value, event: &str) -> Option<Value> {
        if let Value::Table(t) = val {
            return t.borrow().metamethod(event);
        }
        None
    }

    /// Runs the active debug hook for `event`, if one is set and listening.
    /// Hooks never trigger hooks (`in_hook` guard); a hook error propagates
    /// to the caller, which is responsible for unwinding its own frame.
    fn fire_hook(&mut self, event: HookEvent) -> Result<(), String> {
        let (func, wanted) = match &self.hook {
            Some(hook) if !self.in_hook => (hook.func.clone(), event.wants(hook)),
            _ => return Ok(()),
        };
        if !wanted {
            return Ok(());
        }
        self.in_hook = true;
        let arg = Value::str(event.name());
        let res = self.call_function(func, &[arg]);
        self.in_hook = false;
        res.map(|_| ())
    }

    /// Recomputes `trap` after a hook or debugger setting changed.
    pub fn update_trap(&mut self) {
        let hook_wants = self.hook.as_ref().is_some_and(|h| h.on_line || h.count > 0);
        let debugger_wants = self.dap.as_ref().is_some_and(|d| d.wants_lines());
        self.trap = hook_wants || debugger_wants;
    }

    /// Line and count events for the instruction at `ip` of the running
    /// frame, and the debugger's line breakpoints and stepping.
    fn instruction_hook(&mut self, ip: usize) -> Result<(), String> {
        if self.in_hook || self.in_eval {
            return Ok(());
        }
        let frame = self.frames.last_mut().expect(RUNNING_FRAME);
        let line = frame.closure.proto.lines.get(ip).copied().unwrap_or(0);
        // A new line, or a jump back into the same one (a loop), starts a
        // new line event.
        let new_line = line != 0 && (line != frame.hook_line || ip <= frame.hook_ip);
        frame.hook_line = line;
        frame.hook_ip = ip;

        if self.dap.is_some() && new_line {
            crate::dap::check_line(self, line)?;
        }
        let Some(hook) = self.hook.clone() else {
            return Ok(());
        };
        if hook.count > 0 {
            if self.hook_countdown <= 1 {
                self.hook_countdown = hook.count;
                self.call_hook(&hook.func, &[Value::str("count")])?;
            } else {
                self.hook_countdown -= 1;
            }
        }
        if hook.on_line && new_line {
            self.call_hook(&hook.func, &[Value::str("line"), Value::Int(line as i64)])?;
        }
        Ok(())
    }

    fn call_hook(&mut self, func: &Value, args: &[Value]) -> Result<(), String> {
        self.in_hook = true;
        let res = self.call_function(func.clone(), args);
        self.in_hook = false;
        res.map(|_| ())
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
        self.co_states.retain(|_, weak| weak.upgrade().is_some());
        self.co_states_pruned_at = self.co_states.len();
    }

    pub(crate) fn gc_collect(&mut self) -> Result<(), String> {
        while !self.gc.sweep_slice(usize::MAX) {}
        while !self.gc.sweep_slice(usize::MAX) {}
        let crate::vm::gc::SweepPlan {
            queue,
            internal,
            plain,
            dropped,
        } = self.gc.collect_plan(&self.stack, &self.globals);
        let (settled_cleared, settled_freed, first_err) = self.settle_queue(&queue, &internal);
        self.gc
            .sweep_finish(plain, dropped, settled_cleared, settled_freed);
        // Dead coroutine weaks need no GC decision (liveness follows the
        // handle's refcount, visible or not); drop them here every cycle.
        self.prune_co_states();
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(())
    }

    /// Runs one finalizer queue, settling each table afterwards. Shared by
    /// the draining and bounded-stepping drivers so both observe identical
    /// order and resurrection rules.
    fn settle_queue(
        &mut self,
        queue: &[(Rc<RefCell<VmTable>>, Value)],
        internal: &crate::vm::hash::FxHashMap<*const RefCell<VmTable>, usize>,
    ) -> (Vec<*const RefCell<VmTable>>, usize, Option<String>) {
        let mut settled_cleared = Vec::new();
        let mut settled_freed = 0;
        let mut first_err: Option<String> = None;
        for (rc, handler) in queue {
            let arg = Value::Table(rc.clone());
            let res = self.call_function(handler.clone(), std::slice::from_ref(&arg));
            let (cleared, freed) = self.gc.settle_table(rc, res.is_ok(), internal);
            if let Some(ptr) = cleared {
                settled_cleared.push(ptr);
            }
            settled_freed += freed;
            if let Err(e) = res {
                first_err.get_or_insert(e);
            }
        }
        (settled_cleared, settled_freed, first_err)
    }

    /// Allocation-triggered collection step: advances an in-progress sweep
    /// by one bounded slice, or starts a new cycle (plan, settle, begin)
    /// when over threshold and takes its first slice. Bounds pause times;
    /// explicit collections still drain fully. Slice size is fixed; pacing
    /// itself comes from the allocation threshold.
    pub(crate) fn gc_auto_step(&mut self) -> Result<(), String> {
        const SLICE_BUDGET: usize = 256;
        if self.gc.sweeping {
            self.gc.sweep_slice(SLICE_BUDGET);
            return Ok(());
        }
        if !self.gc.should_collect() {
            return Ok(());
        }
        // Reap dead acyclic tables from the nursery and promote survivors into
        // `tables` (safe even when `__gc` or weak tables exist, since pinned
        // and referenced entries keep `strong_count > 0`). Skip the full cycle
        // collector only while total occupancy stays comfortably below the
        // threshold and consecutive nursery-only reaps remain bounded.
        let dropped = self.gc.reap_young();
        if dropped > 0
            && self.gc.bytes_allocated <= self.gc.threshold * 3 / 4
            && self.gc.nursery_reaps_since_cycle < 4
        {
            self.gc.nursery_reaps_since_cycle += 1;
            self.gc.last_freed = dropped;
            return Ok(());
        }
        let crate::vm::gc::SweepPlan {
            queue,
            internal,
            plain,
            dropped,
        } = self.gc.collect_plan(&self.stack, &self.globals);
        let (settled_cleared, settled_freed, first_err) = self.settle_queue(&queue, &internal);
        self.gc
            .sweep_begin(plain, dropped, settled_cleared, settled_freed);
        self.gc.sweep_slice(SLICE_BUDGET);
        if let Some(e) = first_err {
            return Err(e);
        }
        Ok(())
    }

    /// Every value the last frame to finish at a `run_to_depth` boundary
    /// returned.
    pub fn take_returned(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.returned)
    }

    pub fn run(&mut self) -> Result<Value, String> {
        self.run_to_depth(0)
    }

    /// Calls a binary metamethod and stores its first result in `dst`.
    fn call_binop_mm(
        &mut self,
        mm: Value,
        va: Value,
        vb: Value,
        base: usize,
        dst: u8,
    ) -> Result<(), String> {
        let res = self.call_function(mm, &[va, vb])?;
        self.set_reg_grow(
            base + dst as usize,
            res.into_iter().next().unwrap_or(Value::Nil),
        );
        Ok(())
    }

    /// Calls a comparison metamethod and reports whether its result is truthy.
    fn call_cmp_mm(&mut self, mm: Value, va: Value, vb: Value) -> Result<bool, String> {
        let res = self.call_function(mm, &[va, vb])?;
        Ok(res.first().is_some_and(|v| v.is_truthy()))
    }

    /// The interpreter loop.
    ///
    /// The outer loop (re)loads the current frame's code and register base
    /// into locals; the inner loop dispatches instructions against them.
    /// Anything that changes the current frame breaks out to the outer loop,
    /// and anything that can run other code (natives, metamethods) writes
    /// the instruction pointer back first so that code sees a consistent
    /// frame.
    pub fn run_to_depth(&mut self, target_depth: usize) -> Result<Value, String> {
        let result = self.run_loop(target_depth);
        if result.is_err() && !self.is_yielding && self.error_trace.is_none() {
            self.error_trace = Some(self.capture_trace());
        }
        result
    }

    /// Where every live frame is, innermost first. Deep stacks keep their
    /// innermost and outermost frames.
    pub fn capture_trace(&self) -> Vec<TraceFrame> {
        const KEEP: usize = 12;
        let frame_at = |frame: &crate::vm::frame::CallFrame| {
            let proto = &frame.closure.proto;
            TraceFrame {
                function: proto.name.clone(),
                source: proto.source.clone(),
                line: proto
                    .lines
                    .get(frame.ip.saturating_sub(1))
                    .copied()
                    .unwrap_or(0),
            }
        };
        let frames: Vec<_> = self.frames.iter().rev().collect();
        if frames.len() <= KEEP * 2 {
            return frames.into_iter().map(frame_at).collect();
        }
        let mut trace: Vec<TraceFrame> = frames[..KEEP].iter().map(|f| frame_at(f)).collect();
        trace.push(TraceFrame {
            function: Some(format!("... {} more frames", frames.len() - KEEP * 2)),
            source: None,
            line: 0,
        });
        trace.extend(frames[frames.len() - KEEP..].iter().map(|f| frame_at(f)));
        trace
    }

    /// An error message followed by the traceback recorded for it, if any.
    pub fn describe_error(&self, message: &str) -> String {
        let mut out = format!(
            "runtime error[{}]: {}\n",
            crate::diagnostics::error_code::ErrorCode::E0200,
            message
        );
        if let Some(trace) = &self.error_trace {
            if let Some(top) = trace.first() {
                let src_name = top.source.as_deref().unwrap_or("<unknown>");
                let line = top.line.max(1);
                let pad = line.to_string().len().max(2);
                out.push_str(&format!(
                    "{:pad$}--> {}:{}:1\n",
                    "",
                    src_name,
                    line,
                    pad = pad
                ));
                out.push_str(&format!("{:pad$} |\n", "", pad = pad));
                let bundled = crate::app::read(src_name)
                    .and_then(Result::ok)
                    .and_then(|bytes| String::from_utf8(bytes).ok());
                if let Some(content) = bundled.or_else(|| std::fs::read_to_string(src_name).ok())
                    && let Some(line_content) = content.lines().nth((line - 1) as usize)
                {
                    out.push_str(&format!("{:pad$} | {}\n", line, line_content, pad = pad));
                    out.push_str(&format!("{:pad$} | ^ {}\n", "", message, pad = pad));
                }
                out.push_str(&format!("{:pad$} |\n", "", pad = pad));
            }
            out.push_str("  = stack backtrace:\n");
            for (idx, frame) in trace.iter().enumerate() {
                if frame.source.is_none() && frame.line == 0 {
                    out.push_str(&format!(
                        "    {:2}: {}\n",
                        idx,
                        frame.function.as_deref().unwrap_or("...")
                    ));
                } else {
                    let func = frame.function.as_deref().unwrap_or("<main>");
                    let src = frame.source.as_deref().unwrap_or("?");
                    out.push_str(&format!(
                        "    {:2}: {} at {}:{}\n",
                        idx, func, src, frame.line
                    ));
                }
            }
        }
        out
    }

    fn run_loop(&mut self, target_depth: usize) -> Result<Value, String> {
        // Hooks and the debugger need a check before every instruction, so
        // the loop is compiled twice and code without them never pays for
        // it. Either copy hands over (`None`) when `trap` changes.
        loop {
            let done = if self.trap {
                self.run_mode::<true>(target_depth)?
            } else {
                self.run_mode::<false>(target_depth)?
            };
            if let Some(value) = done {
                return Ok(value);
            }
        }
    }

    fn run_mode<const TRAP: bool>(&mut self, target_depth: usize) -> Result<Option<Value>, String> {
        if self.frames.len() <= target_depth {
            return Ok(Some(Value::Nil));
        }
        let (closure, mut base, mut ip) = {
            let f = self.frames.last().expect(RUNNING_FRAME);
            (f.closure.clone(), f.base, f.ip)
        };
        // Verified once per prototype: afterwards every register operand
        // is known to be below `max_registers`, and with the stack sized
        // to cover the frame's window the register accessors below can
        // skip bounds checks. Nothing shrinks the stack while a frame is
        // live: callers restore it to at least its earlier length.
        if !closure.proto.is_verified() {
            crate::bytecode::verify_proto(&closure.proto).map_err(|e| e.to_string())?;
            closure.proto.mark_verified();
        }
        let window_end = base + closure.proto.max_registers as usize;
        if self.stack.len() < window_end {
            self.stack.resize(window_end, Value::Nil);
        }
        let mut current_closure = closure;
        let mut code: &[Instruction] = &current_closure.proto.instructions;
        let mut consts: &[Value] = current_closure.proto.values();

        // Register access relative to this frame. `reg!` reads, `set!` writes.
        macro_rules! reg {
            ($r:expr) => {
                *slot(&self.stack, base + $r as usize)
            };
        }
        macro_rules! set {
            ($r:expr, $v:expr) => {{
                let v = $v;
                let slot = slot_mut(&mut self.stack, base + $r as usize);
                match v {
                    Value::Int(new) => match slot {
                        Value::Int(old) => *old = new,
                        s => *s = Value::Int(new),
                    },
                    Value::Float(new) => match slot {
                        Value::Float(old) => *old = new,
                        s => *s = Value::Float(new),
                    },
                    Value::Bool(new) => match slot {
                        Value::Bool(old) => *old = new,
                        s => *s = Value::Bool(new),
                    },
                    other => *slot = other,
                }
            }};
        }
        macro_rules! sync_ip {
            () => {
                self.frames.last_mut().expect(RUNNING_FRAME).ip = ip;
            };
        }
        macro_rules! jump {
            ($off:expr) => {{
                ip = (ip as isize + $off as isize) as usize;
            }};
        }
        macro_rules! throw {
                ($($arg:tt)*) => {{
                    sync_ip!();
                    return Err(format!($($arg)*));
                }};
            }
        // Runs a fallible expression, syncing the ip before propagating
        // an error so tracebacks point at the failing instruction.
        macro_rules! attempt {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(e) => {
                        sync_ip!();
                        return Err(e);
                    }
                }
            };
        }
        // Binary arithmetic: `i64` fast path, then floats, then the
        // generic evaluator (bignums, mixed types, errors); a table
        // operand goes through its metamethod.
        macro_rules! arith {
            ($dst:expr, $a:expr, $b:expr, $checked:ident, $fop:tt, $slow:path, $event:literal) => {
                arith!(@impl $dst, $a, $b, $checked, $fop, $slow, $event, {
                    let res = attempt!($slow(&reg!($a), &reg!($b)));
                    set!($dst, res);
                })
            };
            ($dst:expr, $a:expr, $b:expr, $checked:ident, $fop:tt, $slow:path, $event:literal, into $into:path) => {
                arith!(@impl $dst, $a, $b, $checked, $fop, $slow, $event, {
                    attempt!($into(
                        &mut self.stack,
                        base + $dst as usize,
                        base + $a as usize,
                        base + $b as usize,
                    ));
                })
            };
            (@impl $dst:expr, $a:expr, $b:expr, $checked:ident, $fop:tt, $slow:path, $event:literal, $fallback:block) => {{
                let ra = &reg!($a);
                let rb = &reg!($b);
                match (ra, rb) {
                    (Value::Int(x), Value::Int(y)) => match x.$checked(*y) {
                        Some(r) => {
                            let slot = slot_mut(&mut self.stack, base + $dst as usize);
                            if let Value::Int(old) = slot {
                                *old = r;
                            } else {
                                *slot = Value::Int(r);
                            }
                        }
                        None => {
                            let res = attempt!($slow(ra, rb));
                            set!($dst, res);
                        }
                    },
                    (Value::Float(x), Value::Float(y)) => {
                        let r = x $fop y;
                        let slot = slot_mut(&mut self.stack, base + $dst as usize);
                        if let Value::Float(old) = slot {
                            *old = r;
                        } else {
                            *slot = Value::Float(r);
                        }
                    }
                    (Value::Table(_), _) | (_, Value::Table(_)) => {
                        if let Some(mm) = self.get_binop_metamethod(ra, rb, $event) {
                            let (va, vb) = (ra.clone(), rb.clone());
                            sync_ip!();
                            self.call_binop_mm(mm, va, vb, base, $dst)?;
                            continue;
                        }
                        let res = attempt!($slow(ra, rb));
                        set!($dst, res);
                    }
                    _ => match (ra, rb) {
                        (Value::Int(x), Value::Float(y)) => {
                            let r = *x as f64 $fop *y;
                            let slot = slot_mut(&mut self.stack, base + $dst as usize);
                            if let Value::Float(old) = slot {
                                *old = r;
                            } else {
                                *slot = Value::Float(r);
                            }
                        }
                        (Value::Float(x), Value::Int(y)) => {
                            let r = *x $fop *y as f64;
                            let slot = slot_mut(&mut self.stack, base + $dst as usize);
                            if let Value::Float(old) = slot {
                                *old = r;
                            } else {
                                *slot = Value::Float(r);
                            }
                        }
                        _ => $fallback,
                    },
                }
            }};
        }
        // Arithmetic without an inline fast path (division, modulo,
        // power): straight to the evaluator, metamethods first.
        macro_rules! arith_slow {
            ($dst:expr, $a:expr, $b:expr, $slow:path, $event:literal) => {{
                let ra = &reg!($a);
                let rb = &reg!($b);
                if matches!(ra, Value::Table(_)) || matches!(rb, Value::Table(_)) {
                    if let Some(mm) = self.get_binop_metamethod(ra, rb, $event) {
                        let (va, vb) = (ra.clone(), rb.clone());
                        sync_ip!();
                        self.call_binop_mm(mm, va, vb, base, $dst)?;
                        continue;
                    }
                }
                let res = attempt!($slow(ra, rb));
                set!($dst, res);
            }};
        }
        // Ordered comparison with a conditional jump. `$mm_swap` is true
        // for `>`/`>=`, which are `<`/`<=` with the operands swapped.
        macro_rules! compare {
                ($a:expr, $b:expr, $jump:expr, $op:tt, $slow:path, $event:literal, $mm_swap:expr) => {{
                    let ra = &reg!($a);
                    let rb = &reg!($b);
                    let holds = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => x $op y,
                        (Value::Float(x), Value::Float(y)) => x $op y,
                        (Value::Table(_), _) | (_, Value::Table(_)) => {
                            let (l, r) = if $mm_swap { (rb, ra) } else { (ra, rb) };
                            if let Some(mm) = self.get_binop_metamethod(l, r, $event) {
                                let (vl, vr) = (l.clone(), r.clone());
                                sync_ip!();
                                self.call_cmp_mm(mm, vl, vr)?
                            } else {
                                attempt!($slow(ra, rb))
                            }
                        }
                        _ => attempt!($slow(ra, rb)),
                    };
                    if !holds {
                        jump!($jump);
                    }
                }};
            }

        // Immediate binary arithmetic: `i64` fast path, then `f64`, then
        // metamethod / generic evaluator with `Value::Int(imm as i64)`.
        macro_rules! arith_imm {
            ($dst:expr, $a:expr, $imm:expr, $checked:ident, $fop:tt, $slow:path, $event:literal) => {{
                let ra = &reg!($a);
                let imm_i64 = $imm as i64;
                match ra {
                    Value::Int(x) => match x.$checked(imm_i64) {
                        Some(r) => {
                            let slot = slot_mut(&mut self.stack, base + $dst as usize);
                            if let Value::Int(old) = slot {
                                *old = r;
                            } else {
                                *slot = Value::Int(r);
                            }
                        }
                        None => {
                            let rb = Value::Int(imm_i64);
                            let res = attempt!($slow(ra, &rb));
                            set!($dst, res);
                        }
                    },
                    Value::Float(x) => {
                        let r = *x $fop (imm_i64 as f64);
                        let slot = slot_mut(&mut self.stack, base + $dst as usize);
                        if let Value::Float(old) = slot {
                            *old = r;
                        } else {
                            *slot = Value::Float(r);
                        }
                    }
                    Value::Table(_) => {
                        let rb = Value::Int(imm_i64);
                        if let Some(mm) = self.get_binop_metamethod(ra, &rb, $event) {
                            let va = ra.clone();
                            sync_ip!();
                            self.call_binop_mm(mm, va, rb, base, $dst)?;
                            continue;
                        }
                        let res = attempt!($slow(ra, &rb));
                        set!($dst, res);
                    }
                    _ => {
                        let rb = Value::Int(imm_i64);
                        let res = attempt!($slow(ra, &rb));
                        set!($dst, res);
                    }
                }
            }};
        }
        // Ordered comparison with an `i16` immediate and conditional jump.
        macro_rules! compare_imm {
            ($a:expr, $imm:expr, $jump:expr, $op:tt, $slow:path, $event:literal, $mm_swap:expr) => {{
                let ra = &reg!($a);
                let imm_i64 = $imm as i64;
                let holds = match ra {
                    Value::Int(x) => *x $op imm_i64,
                    Value::Float(x) => *x $op (imm_i64 as f64),
                    Value::Table(_) => {
                        let rb = Value::Int(imm_i64);
                        let (l, r) = if $mm_swap { (&rb, ra) } else { (ra, &rb) };
                        if let Some(mm) = self.get_binop_metamethod(l, r, $event) {
                            let (vl, vr) = (l.clone(), r.clone());
                            sync_ip!();
                            self.call_cmp_mm(mm, vl, vr)?
                        } else {
                            attempt!($slow(ra, &rb))
                        }
                    }
                    _ => {
                        let rb = Value::Int(imm_i64);
                        attempt!($slow(ra, &rb))
                    }
                };
                if !holds {
                    jump!($jump);
                }
            }};
        }

        loop {
            let Some(&inst) = code.get(ip) else {
                // Fell off the end without a `Return`: treat as returning nothing.
                sync_ip!();
                if self.hook.is_some() {
                    self.fire_hook(HookEvent::Return)?;
                }
                let frame = self.frames.pop().expect(RUNNING_FRAME);
                if !self.open_upvalues.is_empty() {
                    self.close_upvalues(frame.base);
                }
                if self.frames.len() == target_depth {
                    self.returned.clear();
                    return Ok(Some(Value::Nil));
                }
                let caller_frame = self.frames.last().expect(RUNNING_FRAME);
                current_closure = caller_frame.closure.clone();
                base = caller_frame.base;
                ip = caller_frame.ip;
                if self.trap != TRAP {
                    sync_ip!();
                    return Ok(None);
                }
                code = &current_closure.proto.instructions;
                consts = current_closure.proto.values();
                continue;
            };
            if TRAP {
                // The frame's ip points past the instruction about to run,
                // as it does while that instruction executes.
                self.frames.last_mut().expect(RUNNING_FRAME).ip = ip + 1;
                self.instruction_hook(ip)?;
                if !self.trap {
                    sync_ip!();
                    return Ok(None);
                }
            }
            ip += 1;

            match inst {
                Instruction::LoadNil { dst } => set!(dst, Value::Nil),
                Instruction::LoadBool { dst, val } => set!(dst, Value::Bool(val)),
                Instruction::LoadInt { dst, val } => set!(dst, Value::Int(val as i64)),
                Instruction::LoadK { dst, k } => set!(dst, consts[k as usize].clone()),
                Instruction::Move { dst, src } => {
                    let src_slot = slot(&self.stack, base + src as usize);
                    match src_slot {
                        Value::Int(i) => {
                            let val = *i;
                            let d = slot_mut(&mut self.stack, base + dst as usize);
                            if let Value::Int(old) = d {
                                *old = val;
                            } else {
                                *d = Value::Int(val);
                            }
                        }
                        Value::Float(f) => {
                            let val = *f;
                            let d = slot_mut(&mut self.stack, base + dst as usize);
                            if let Value::Float(old) = d {
                                *old = val;
                            } else {
                                *d = Value::Float(val);
                            }
                        }
                        Value::Bool(b) => {
                            let val = *b;
                            let d = slot_mut(&mut self.stack, base + dst as usize);
                            if let Value::Bool(old) = d {
                                *old = val;
                            } else {
                                *d = Value::Bool(val);
                            }
                        }
                        _ => {
                            let v = reg!(src).clone();
                            set!(dst, v);
                        }
                    }
                }
                Instruction::GetGlobal { dst, name_k } => {
                    let Value::String(name) = &consts[name_k as usize] else {
                        throw!("global name must be string");
                    };
                    let val = self.globals.get(&**name).cloned().unwrap_or(Value::Nil);
                    set!(dst, val);
                }
                Instruction::SetGlobal { src, name_k } => {
                    let Value::String(name) = &consts[name_k as usize] else {
                        throw!("global name must be string");
                    };
                    let val = reg!(src).clone();
                    // Re-inserting an existing key would drop the old Rc for
                    // an identical one; a lookup first avoids that churn.
                    if let Some(slot) = self.globals.get_mut(&**name) {
                        *slot = val;
                    } else {
                        self.globals.insert(name.clone(), val);
                    }
                    self.globals_epoch = self.globals_epoch.wrapping_add(1);
                }
                Instruction::GetUpval { dst, upval_idx } => {
                    let uv = &current_closure.upvalues[upval_idx as usize];
                    match uv.location.get() {
                        Some(idx) => {
                            let src = &self.stack[idx];
                            match src {
                                Value::Int(i) => {
                                    let val = *i;
                                    let slot = slot_mut(&mut self.stack, base + dst as usize);
                                    if let Value::Int(old) = slot {
                                        *old = val;
                                    } else {
                                        *slot = Value::Int(val);
                                    }
                                }
                                Value::Float(f) => {
                                    let val = *f;
                                    let slot = slot_mut(&mut self.stack, base + dst as usize);
                                    if let Value::Float(old) = slot {
                                        *old = val;
                                    } else {
                                        *slot = Value::Float(val);
                                    }
                                }
                                Value::Bool(b) => {
                                    let val = *b;
                                    let slot = slot_mut(&mut self.stack, base + dst as usize);
                                    if let Value::Bool(old) = slot {
                                        *old = val;
                                    } else {
                                        *slot = Value::Bool(val);
                                    }
                                }
                                other => {
                                    let v = other.clone();
                                    set!(dst, v);
                                }
                            }
                        }
                        None => {
                            let val = uv.closed.borrow().clone();
                            set!(dst, val);
                        }
                    }
                }
                Instruction::SetUpval { src, upval_idx } => {
                    let val = reg!(src).clone();
                    self.upvalue_set(&current_closure.upvalues[upval_idx as usize], val);
                }
                Instruction::NewTable {
                    dst,
                    array_cap,
                    fields_cap,
                } => {
                    if self.gc.should_collect() || self.gc.sweeping {
                        self.gc_auto_step().inspect_err(|_| {
                            sync_ip!();
                        })?;
                    }
                    let table = if array_cap == 0 && fields_cap == 0 {
                        VmTable::new()
                    } else {
                        VmTable::with_capacity(array_cap as usize, fields_cap as usize)
                    };
                    let rc = Rc::new(RefCell::new(table));
                    let size = 64 + (array_cap as usize) * std::mem::size_of::<Value>();
                    self.gc.register_table_sized(&rc, size);
                    set!(dst, Value::Table(rc));
                }
                Instruction::GetTable { dst, table, key } => {
                    let tbl_val = &reg!(table);
                    let k = &reg!(key);
                    if let (Value::Table(t), Value::Int(i)) = (tbl_val, k)
                        && *i >= 1
                    {
                        let tbl = t.borrow();
                        let idx = *i as usize - 1;
                        if idx < tbl.array.len() {
                            let val = match &tbl.array[idx] {
                                Value::Int(n) => Value::Int(*n),
                                Value::Float(f) => Value::Float(*f),
                                Value::Bool(b) => Value::Bool(*b),
                                other => other.clone(),
                            };
                            drop(tbl);
                            set!(dst, val);
                            continue;
                        }
                    } else if let (Value::Table(t), Value::String(s)) = (tbl_val, k) {
                        let tbl = t.borrow();
                        if let Some(v) = tbl.fields.get_by_strref(s) {
                            let val = match v {
                                Value::Int(n) => Value::Int(*n),
                                Value::Float(f) => Value::Float(*f),
                                Value::Bool(b) => Value::Bool(*b),
                                other => other.clone(),
                            };
                            drop(tbl);
                            set!(dst, val);
                            continue;
                        } else if tbl.metatable.is_none() {
                            drop(tbl);
                            set!(dst, Value::Nil);
                            continue;
                        }
                    }
                    let val = match Self::table_get_fast(tbl_val, k) {
                        Some(v) => v,
                        None => {
                            let (tbl, k) = (tbl_val.clone(), k.clone());
                            sync_ip!();
                            self.table_get(&tbl, &k)?
                        }
                    };
                    set!(dst, val);
                }
                Instruction::GetTableK { dst, table, key_k } => {
                    let tbl_val = &reg!(table);
                    let k = &consts[key_k as usize];
                    if let (Value::Table(t), Value::String(s)) = (tbl_val, k) {
                        let tbl = t.borrow();
                        if let Some(v) = tbl.fields.get_by_strref(s) {
                            let val = match v {
                                Value::Int(n) => Value::Int(*n),
                                Value::Float(f) => Value::Float(*f),
                                Value::Bool(b) => Value::Bool(*b),
                                other => other.clone(),
                            };
                            drop(tbl);
                            set!(dst, val);
                            continue;
                        } else if tbl.metatable.is_none() {
                            drop(tbl);
                            set!(dst, Value::Nil);
                            continue;
                        }
                    }
                    let val = match Self::table_get_fast(tbl_val, k) {
                        Some(v) => v,
                        None => {
                            let tbl = tbl_val.clone();
                            sync_ip!();
                            self.table_get(&tbl, k)?
                        }
                    };
                    set!(dst, val);
                }
                Instruction::GetImport {
                    dst,
                    mod_k,
                    field_k,
                    site,
                } => {
                    let site = site as usize;
                    // Fast path: resolved under this epoch with the
                    // module still metatable-free. One epoch read, one
                    // pointer check, one clone instead of two hashes.
                    if let Some(cached) = current_closure
                        .proto
                        .import_cached(site, self.globals_epoch)
                    {
                        set!(dst, cached);
                    } else {
                        let (mname, fname) =
                            match (&consts[mod_k as usize], &consts[field_k as usize]) {
                                (Value::String(m), Value::String(f)) => (m.clone(), f.clone()),
                                _ => throw!("import names must be strings"),
                            };
                        // Dynamic resolution, exactly what GetGlobal
                        // plus GetTableK do: missing globals read nil.
                        let module_val = self.globals.get(&*mname).cloned().unwrap_or(Value::Nil);
                        let fkey = Value::String(fname);
                        let value = match Self::table_get_fast(&module_val, &fkey) {
                            Some(v) => v,
                            None => {
                                sync_ip!();
                                self.table_get(&module_val, &fkey)?
                            }
                        };
                        // Cache only what cannot change behind our back:
                        // a frozen, metatable-free module with a present
                        // non-nil field. Misses, user tables and anything
                        // with a metatable stay fully dynamic.
                        if let Value::Table(t) = &module_val {
                            let cacheable = {
                                let tbl = t.borrow();
                                tbl.frozen
                                    && tbl.metatable.is_none()
                                    && !matches!(value, Value::Nil)
                            };
                            if cacheable {
                                current_closure.proto.import_store(
                                    site,
                                    self.globals_epoch,
                                    t.clone(),
                                    value.clone(),
                                );
                            }
                        }
                        set!(dst, value);
                    }
                }
                Instruction::SetTable { table, key, val } => {
                    let tbl_val = &reg!(table);
                    let k = &reg!(key);
                    let v = &reg!(val);
                    if let (Value::Table(t), Value::Int(i)) = (tbl_val, k)
                        && *i >= 1
                        && !matches!(v, Value::Nil)
                    {
                        let mut tbl = t.borrow_mut();
                        if !tbl.frozen {
                            let idx = *i as usize - 1;
                            if idx < tbl.array.len() {
                                match (v, &mut tbl.array[idx]) {
                                    (Value::Int(n), Value::Int(o)) => *o = *n,
                                    (Value::Float(n), Value::Float(o)) => *o = *n,
                                    (Value::Bool(n), Value::Bool(o)) => *o = *n,
                                    (v_other, slot) => *slot = v_other.clone(),
                                }
                                continue;
                            } else if idx == tbl.array.len()
                                && tbl.metatable.is_none()
                                && tbl.general.is_none()
                            {
                                tbl.array.push(v.clone());
                                continue;
                            }
                        }
                    } else if let (Value::Table(t), Value::String(s)) = (tbl_val, k)
                        && !matches!(v, Value::Nil)
                    {
                        let mut tbl = t.borrow_mut();
                        if !tbl.frozen {
                            if tbl.metatable.is_none() {
                                tbl.fields.set_by_strref(s, v);
                                continue;
                            } else if let Some(slot) = tbl.fields.get_mut_by_strref(s) {
                                crate::vm::table::assign_value(slot, v);
                                continue;
                            }
                        }
                    }
                    if !Self::table_set_fast(tbl_val, k, v) {
                        let (tbl, k, v) = (tbl_val.clone(), k.clone(), v.clone());
                        sync_ip!();
                        self.table_set(&tbl, k, v)?;
                    }
                }
                Instruction::SetTableK { table, key_k, val } => {
                    let tbl_val = &reg!(table);
                    let k = &consts[key_k as usize];
                    let v = &reg!(val);
                    if let (Value::Table(t), Value::String(s)) = (tbl_val, k)
                        && !matches!(v, Value::Nil)
                    {
                        let mut tbl = t.borrow_mut();
                        if !tbl.frozen {
                            if tbl.metatable.is_none() {
                                tbl.fields.set_by_strref(s, v);
                                continue;
                            } else if let Some(slot) = tbl.fields.get_mut_by_strref(s) {
                                crate::vm::table::assign_value(slot, v);
                                continue;
                            }
                        }
                    }
                    if !Self::table_set_fast(tbl_val, k, v) {
                        let (tbl, k, v) = (tbl_val.clone(), k.clone(), v.clone());
                        sync_ip!();
                        self.table_set(&tbl, k, v)?;
                    }
                }
                Instruction::AppendArray { table, src } => {
                    let val = reg!(src).clone();
                    if let Value::Table(t) = &reg!(table) {
                        t.borrow_mut().array.push(val);
                    } else {
                        throw!("cannot append to non-table");
                    }
                }
                Instruction::SetList {
                    table,
                    base: list,
                    count,
                } => {
                    let start = base + list as usize;
                    let end = if count == MULTRET {
                        self.top.max(start)
                    } else {
                        start + count as usize
                    };
                    let end = end.min(self.stack.len());
                    if let Value::Table(t) = &reg!(table) {
                        let t = t.clone();
                        let mut tbl = t.borrow_mut();
                        tbl.array.reserve(end.saturating_sub(start));
                        tbl.array.extend(self.stack[start..end].iter().cloned());
                    } else {
                        throw!("cannot SetList to non-table");
                    }
                }
                Instruction::TForCall { base: b, retc } => {
                    sync_ip!();
                    self.tfor_call(base, b, retc)?;
                }
                Instruction::TForLoop { base: b, jump } => {
                    let first_var = reg!(b + 3).clone();
                    if matches!(first_var, Value::Nil) {
                        // Exit loop: jump forward past the back-jump
                        jump!(jump);
                    } else {
                        // Update ctrl to first result, continue
                        set!(b + 2, first_var);
                    }
                }
                Instruction::CloseUpval { reg } => {
                    if !self.open_upvalues.is_empty() {
                        self.close_upvalue_at(base + reg as usize);
                    }
                }
                Instruction::Add { dst, a, b } => {
                    arith!(
                        dst,
                        a,
                        b,
                        checked_add,
                        +,
                        ops::eval_add,
                        "__add",
                        into ops::eval_add_into
                    )
                }
                Instruction::AddI { dst, a, imm } => {
                    arith_imm!(dst, a, imm, checked_add, +, ops::eval_add, "__add")
                }
                Instruction::Sub { dst, a, b } => {
                    arith!(
                        dst,
                        a,
                        b,
                        checked_sub,
                        -,
                        ops::eval_sub,
                        "__sub",
                        into ops::eval_sub_into
                    )
                }
                Instruction::SubI { dst, a, imm } => {
                    arith_imm!(dst, a, imm, checked_sub, -, ops::eval_sub, "__sub")
                }
                Instruction::Mul { dst, a, b } => {
                    arith!(dst, a, b, checked_mul, *, ops::eval_mul, "__mul")
                }
                Instruction::MulI { dst, a, imm } => {
                    arith_imm!(dst, a, imm, checked_mul, *, ops::eval_mul, "__mul")
                }
                Instruction::Div { dst, a, b } => {
                    arith_slow!(dst, a, b, ops::eval_div, "__div")
                }
                Instruction::IDiv { dst, a, b } => {
                    // Fast path for small ints; the zero divisor,
                    // MIN/-1 overflow, floats, BigInts and metamethods
                    // keep going through the generic evaluator so
                    // semantics cannot drift.
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => {
                            if *y == 0 || (*x == i64::MIN && *y == -1) {
                                attempt!(ops::eval_idiv(ra, rb))
                            } else {
                                Value::Int(x.div_floor(y))
                            }
                        }
                        _ => attempt!(ops::eval_idiv(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::Mod { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => {
                            if *y == 0 {
                                attempt!(ops::eval_mod(ra, rb))
                            } else if *y == -1 {
                                Value::Int(0)
                            } else {
                                Value::Int(x.mod_floor(y))
                            }
                        }
                        (Value::BigInt(x), Value::Int(y)) => {
                            if *y == 0 {
                                attempt!(ops::eval_mod(ra, rb))
                            } else {
                                Value::Int(ops::bigint_mod_floor_i64(x, *y))
                            }
                        }
                        _ => attempt!(ops::eval_mod(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::Pow { dst, a, b } => {
                    arith_slow!(dst, a, b, ops::eval_pow, "__pow")
                }
                Instruction::BitAnd { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if let (Value::Int(x), Value::Int(y)) = (ra, rb) {
                        set!(dst, Value::Int(x & y));
                    } else {
                        attempt!(ops::eval_bitand_into(
                            &mut self.stack,
                            base + dst as usize,
                            base + a as usize,
                            base + b as usize,
                        ));
                    }
                }
                Instruction::BitOr { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if let (Value::Int(x), Value::Int(y)) = (ra, rb) {
                        set!(dst, Value::Int(x | y));
                    } else {
                        attempt!(ops::eval_bitor_into(
                            &mut self.stack,
                            base + dst as usize,
                            base + a as usize,
                            base + b as usize,
                        ));
                    }
                }
                Instruction::BitXor { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    if let (Value::Int(x), Value::Int(y)) = (ra, rb) {
                        set!(dst, Value::Int(x ^ y));
                    } else {
                        attempt!(ops::eval_bitxor_into(
                            &mut self.stack,
                            base + dst as usize,
                            base + a as usize,
                            base + b as usize,
                        ));
                    }
                }
                Instruction::Shl { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) if *x >= 0 && *y >= 0 => {
                            let res = if *x == 0 {
                                Value::Int(0)
                            } else {
                                let headroom = x.leading_zeros().saturating_sub(1) as i64;
                                let headroom128 = (64 + x.leading_zeros() as i64).saturating_sub(1);
                                if *y <= headroom {
                                    Value::Int(x << *y)
                                } else if *y <= headroom128 {
                                    Value::from_i128((*x as i128) << *y)
                                } else {
                                    let x_bits = 64 - x.leading_zeros() as u64;
                                    if (*y as u64) <= ops::MAX_INT_BITS
                                        && x_bits.saturating_add(*y as u64) <= ops::MAX_INT_BITS
                                    {
                                        Value::BigInt(std::rc::Rc::new(
                                            num_bigint::BigInt::from(*x) << (*y as usize),
                                        ))
                                    } else {
                                        attempt!(ops::eval_shl(ra, rb))
                                    }
                                }
                            };
                            set!(dst, res);
                        }
                        (Value::BigInt(_), Value::Int(y)) if *y >= 0 => {
                            let shift = *y as u64;
                            attempt!(ops::eval_shl_bigint_into(
                                &mut self.stack,
                                base + dst as usize,
                                base + a as usize,
                                shift,
                            ));
                        }
                        _ => {
                            let res = attempt!(ops::eval_shl(ra, rb));
                            set!(dst, res);
                        }
                    }
                }
                Instruction::Shr { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => {
                            if *y < 0 {
                                let res = attempt!(ops::eval_shl(ra, rb));
                                set!(dst, res);
                            } else {
                                let res = if *y >= 64 {
                                    Value::Int(if *x < 0 { -1 } else { 0 })
                                } else {
                                    Value::Int(x >> *y)
                                };
                                set!(dst, res);
                            }
                        }
                        (Value::BigInt(_), Value::Int(y)) if *y >= 0 => {
                            let shift = *y as u64;
                            ops::eval_shr_bigint_into(
                                &mut self.stack,
                                base + dst as usize,
                                base + a as usize,
                                shift,
                            );
                        }
                        _ => {
                            let res = attempt!(ops::eval_shr(ra, rb));
                            set!(dst, res);
                        }
                    }
                }

                Instruction::ShlI { dst, a, imm } => {
                    let ra = &reg!(a);
                    match ra {
                        Value::Int(x) if *x >= 0 => {
                            let res = if *x == 0 {
                                Value::Int(0)
                            } else {
                                let shift = imm as u32;
                                let headroom = x.leading_zeros().saturating_sub(1);
                                let headroom128 = (64 + x.leading_zeros()).saturating_sub(1);
                                if shift <= headroom {
                                    Value::Int(x << shift)
                                } else if shift <= headroom128 {
                                    Value::from_i128((*x as i128) << shift)
                                } else {
                                    let x_bits = 64 - x.leading_zeros() as u64;
                                    if (shift as u64) <= ops::MAX_INT_BITS
                                        && x_bits.saturating_add(shift as u64) <= ops::MAX_INT_BITS
                                    {
                                        Value::BigInt(std::rc::Rc::new(
                                            num_bigint::BigInt::from(*x) << (imm as usize),
                                        ))
                                    } else {
                                        let rb = Value::Int(imm as i64);
                                        attempt!(ops::eval_shl(ra, &rb))
                                    }
                                }
                            };
                            set!(dst, res);
                        }
                        Value::BigInt(_) => {
                            attempt!(ops::eval_shl_bigint_into(
                                &mut self.stack,
                                base + dst as usize,
                                base + a as usize,
                                imm as u64,
                            ));
                        }
                        _ => {
                            let rb = Value::Int(imm as i64);
                            let res = attempt!(ops::eval_shl(ra, &rb));
                            set!(dst, res);
                        }
                    }
                }
                Instruction::ShrI { dst, a, imm } => {
                    let ra = &reg!(a);
                    match ra {
                        Value::Int(x) => {
                            let res = if imm >= 64 {
                                Value::Int(if *x < 0 { -1 } else { 0 })
                            } else {
                                Value::Int(x >> imm)
                            };
                            set!(dst, res);
                        }
                        Value::BigInt(_) => {
                            ops::eval_shr_bigint_into(
                                &mut self.stack,
                                base + dst as usize,
                                base + a as usize,
                                imm as u64,
                            );
                        }
                        _ => {
                            let rb = Value::Int(imm as i64);
                            let res = attempt!(ops::eval_shr(ra, &rb));
                            set!(dst, res);
                        }
                    }
                }

                Instruction::LShl { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => {
                            let word = *x as u64;
                            let bits = if *y < 0 { usize::MAX } else { *y as usize };
                            let val = if bits >= 64 { 0 } else { word << bits };
                            Value::from_u64(val)
                        }
                        _ => attempt!(ops::eval_lshl(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::LShr { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::Int(x), Value::Int(y)) => {
                            let word = *x as u64;
                            let bits = if *y < 0 { usize::MAX } else { *y as usize };
                            let val = if bits >= 64 { 0 } else { word >> bits };
                            Value::from_u64(val)
                        }
                        _ => attempt!(ops::eval_lshr(ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::Concat { dst, a, b } => {
                    let ra = &reg!(a);
                    let rb = &reg!(b);
                    let res = match (ra, rb) {
                        (Value::String(x), Value::String(y)) => {
                            let mut s = String::with_capacity(x.len() + y.len());
                            s.push_str(x);
                            s.push_str(y);
                            Value::string(s)
                        }
                        (Value::Table(_), _) | (_, Value::Table(_)) => {
                            if let Some(mm) = self.get_binop_metamethod(ra, rb, "__concat") {
                                let (va, vb) = (ra.clone(), rb.clone());
                                sync_ip!();
                                self.call_binop_mm(mm, va, vb, base, dst)?;
                                continue;
                            }
                            Value::string(format!("{}{}", ra, rb))
                        }
                        _ => Value::string(format!("{}{}", ra, rb)),
                    };
                    set!(dst, res);
                }
                Instruction::Unm { dst, src } => {
                    let v = &reg!(src);
                    match v {
                        Value::Int(i) if *i != i64::MIN => set!(dst, Value::Int(-i)),
                        Value::Float(f) => set!(dst, Value::Float(-f)),
                        Value::Table(_) => {
                            if let Some(mm) = self.get_unop_metamethod(v, "__unm") {
                                let v = v.clone();
                                sync_ip!();
                                let res = self.call_function(mm, &[v])?;
                                set!(dst, res.into_iter().next().unwrap_or(Value::Nil));
                                continue;
                            }
                            throw!("unary minus expects a number");
                        }
                        _ => attempt!(ops::eval_unm_into(
                            &mut self.stack,
                            base + dst as usize,
                            base + src as usize,
                        )),
                    }
                }
                Instruction::Not { dst, src } => {
                    let truthy = reg!(src).is_truthy();
                    set!(dst, Value::Bool(!truthy));
                }
                Instruction::Len { dst, src } => {
                    let v = &reg!(src);
                    let len = match v {
                        Value::String(s) => s.len(),
                        Value::Table(t) => {
                            if let Some(mm) = self.get_unop_metamethod(v, "__len") {
                                let v = v.clone();
                                sync_ip!();
                                let res = self.call_function(mm, &[v])?;
                                set!(dst, res.into_iter().next().unwrap_or(Value::Nil));
                                continue;
                            }
                            t.borrow().array.len()
                        }
                        Value::Buffer(b) => b.borrow().len(),
                        _ => throw!("len expects string, table or buffer"),
                    };
                    set!(dst, Value::from_usize(len));
                }
                Instruction::BitNot { dst, src } => {
                    let res = attempt!(ops::eval_bitnot(&reg!(src)));
                    set!(dst, res);
                }
                Instruction::Coalesce { dst, a, b } => {
                    let va = &reg!(a);
                    let res = if matches!(va, Value::Nil) {
                        reg!(b).clone()
                    } else {
                        va.clone()
                    };
                    set!(dst, res);
                }
                Instruction::Eq {
                    a,
                    b,
                    jump_if_false,
                } => {
                    let is_eq = self.values_equal(base, a, b, ip)?;
                    if !is_eq {
                        jump!(jump_if_false);
                    }
                }
                Instruction::Ne {
                    a,
                    b,
                    jump_if_false,
                } => {
                    let is_eq = self.values_equal(base, a, b, ip)?;
                    if is_eq {
                        jump!(jump_if_false);
                    }
                }
                Instruction::Lt {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                Instruction::Le {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                Instruction::Gt {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, >, ops::eval_gt, "__lt", true)
                }
                Instruction::Ge {
                    a,
                    b,
                    jump_if_false,
                } => {
                    compare!(a, b, jump_if_false, >=, ops::eval_ge, "__le", true)
                }
                Instruction::EqI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    let is_eq = match &reg!(a) {
                        Value::Int(x) => *x == imm as i64,
                        Value::Float(x) => *x == imm as f64,
                        _ => false,
                    };
                    if !is_eq {
                        jump!(jump_if_false);
                    }
                }
                Instruction::NeI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    let is_eq = match &reg!(a) {
                        Value::Int(x) => *x == imm as i64,
                        Value::Float(x) => *x == imm as f64,
                        _ => false,
                    };
                    if is_eq {
                        jump!(jump_if_false);
                    }
                }
                Instruction::LtI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    compare_imm!(a, imm, jump_if_false, <, ops::eval_lt, "__lt", false)
                }
                Instruction::LeI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    compare_imm!(a, imm, jump_if_false, <=, ops::eval_le, "__le", false)
                }
                Instruction::GtI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    compare_imm!(a, imm, jump_if_false, >, ops::eval_gt, "__lt", true)
                }
                Instruction::GeI {
                    a,
                    imm,
                    jump_if_false,
                } => {
                    compare_imm!(a, imm, jump_if_false, >=, ops::eval_ge, "__le", true)
                }
                Instruction::Test { reg, jump_if_false } => {
                    if !reg!(reg).is_truthy() {
                        jump!(jump_if_false);
                    }
                }
                Instruction::Jump { offset } => jump!(offset),
                Instruction::ForPrep { base: b, jump } => {
                    let step = &reg!(b + 2);
                    match step {
                        Value::Int(0) => throw!("numeric for step cannot be zero"),
                        Value::Float(f) if *f == 0.0 => {
                            throw!("numeric for step cannot be zero")
                        }
                        _ => {}
                    }
                    let init = &reg!(b);
                    if let (Value::Int(i), Value::Int(s)) = (init, step)
                        && let Some(diff) = i.checked_sub(*s)
                    {
                        let slot = slot_mut(&mut self.stack, base + b as usize);
                        if let Value::Int(old) = slot {
                            *old = diff;
                        } else {
                            *slot = Value::Int(diff);
                        }
                        jump!(jump);
                        continue;
                    }
                    let init_minus_step = attempt!(ops::eval_sub(init, step));
                    set!(b, init_minus_step);
                    jump!(jump);
                }
                Instruction::ForLoop { base: b, jump } => {
                    let idx = &reg!(b);
                    let limit = &reg!(b + 1);
                    let step = &reg!(b + 2);
                    // All-integer loops are the common case and need no
                    // allocation or generic dispatch at all.
                    if let (Value::Int(i), Value::Int(lim), Value::Int(st)) = (idx, limit, step)
                        && let Some(next) = i.checked_add(*st)
                    {
                        let again = if *st >= 0 { next <= *lim } else { next >= *lim };
                        let slot_b = slot_mut(&mut self.stack, base + b as usize);
                        if let Value::Int(old) = slot_b {
                            *old = next;
                        } else {
                            *slot_b = Value::Int(next);
                        }
                        if again {
                            let slot_v = slot_mut(&mut self.stack, base + (b + 3) as usize);
                            if let Value::Int(old) = slot_v {
                                *old = next;
                            } else {
                                *slot_v = Value::Int(next);
                            }
                            jump!(jump);
                        }
                        continue;
                    }
                    match step {
                        Value::Int(0) => throw!("numeric for step cannot be zero"),
                        Value::Float(f) if *f == 0.0 => {
                            throw!("numeric for step cannot be zero")
                        }
                        _ => {}
                    }
                    let is_positive = match step {
                        Value::Int(i) => *i >= 0,
                        Value::BigInt(i) => num_traits::Signed::is_positive(&**i),
                        Value::Float(f) => *f >= 0.0,
                        _ => true,
                    };
                    let next_idx = attempt!(ops::eval_add(idx, step));
                    let loop_again = if is_positive {
                        attempt!(ops::eval_le(&next_idx, limit))
                    } else {
                        attempt!(ops::eval_ge(&next_idx, limit))
                    };
                    set!(b, next_idx.clone());
                    if loop_again {
                        set!(b + 3, next_idx);
                        jump!(jump);
                    }
                }
                Instruction::Closure { dst, proto_idx } => {
                    let child_proto = current_closure.proto.protos[proto_idx as usize].clone();
                    let mut upvalues = Vec::with_capacity(child_proto.upvalues.len());
                    for updesc in &child_proto.upvalues {
                        if updesc.in_stack {
                            upvalues.push(self.capture_upvalue(base + updesc.index as usize));
                        } else {
                            let up = current_closure
                                .upvalues
                                .get(updesc.index as usize)
                                .cloned()
                                .unwrap_or_else(|| Upvalue::closed(Value::Nil));
                            upvalues.push(up);
                        }
                    }
                    let new_closure = Value::Closure(Rc::new(VmClosure {
                        proto: child_proto,
                        upvalues,
                    }));
                    set!(dst, new_closure);
                }
                Instruction::Call { callee, argc, retc } => {
                    let args_start = base + callee as usize + 1;
                    // MULTRET args run from the first argument register up to
                    // whatever the previous instruction left on the stack.
                    let args_end = if argc == MULTRET {
                        self.top.max(args_start)
                    } else {
                        args_start + argc as usize
                    };
                    let argc = args_end - args_start;
                    if args_end > self.stack.len() {
                        self.stack.resize(args_end, Value::Nil);
                    }
                    sync_ip!();

                    match &reg!(callee) {
                        Value::Closure(callee_closure) => {
                            // Tail call: this `Call` is immediately
                            // followed by `Return` of its own results, so
                            // the current frame is replaced instead of
                            // pushing a new one, giving proper tail calls
                            // constant stack space. Only the exact shape
                            // the compilers emit for `return f(...)`
                            // qualifies (MULTRET on both sides with the
                            // results starting at the callee slot): any
                            // extra value would need an instruction
                            // between the two, which fails the adjacency
                            // check. A frame with no caller (base 0)
                            // cannot be replaced.
                            let tail = retc == MULTRET
                                && base >= 1
                                && matches!(
                                    code.get(ip),
                                    Some(Instruction::Return {
                                        base: ret,
                                        count: MULTRET,
                                    }) if *ret == callee
                                );
                            if !tail && self.call_depth() >= self.max_call_depth {
                                throw!(
                                    "call stack overflow: exceeded maximum call depth of {}",
                                    self.max_call_depth
                                );
                            }
                            let same_closure = Rc::ptr_eq(&current_closure, callee_closure);
                            let (max_registers, num_params, is_vararg) = {
                                let proto = &callee_closure.proto;
                                (
                                    proto.max_registers as usize,
                                    proto.num_params as usize,
                                    proto.is_vararg,
                                )
                            };
                            // On recursive calls (`same_closure`), `current_closure`
                            // already owns the active reference so only the frame
                            // needs a clone (1 bump instead of 2). On ordinary
                            // calls, `next_closure` is moved into `current_closure`
                            // after pushing a clone into the frame (2 bumps total,
                            // zero extra refcount churn, and DAP borrows its name
                            // without allocating).
                            let mut next_closure = if same_closure {
                                None
                            } else {
                                Some(Rc::clone(callee_closure))
                            };
                            // For a tail call the callee window slides
                            // down one slot so it sits exactly where a
                            // call from our caller would put it, and the
                            // callee inherits our return obligation.
                            let (new_base, want_ret) = if tail {
                                let cur = self.frames.pop().expect(RUNNING_FRAME);
                                if !self.open_upvalues.is_empty() {
                                    self.close_upvalues(cur.base);
                                }
                                // Relocate the whole callee window so the
                                // callee sits where our caller put us
                                // (`cur.base - 1`) and the arguments
                                // follow; the new frame keeps our base so
                                // its results land in our caller's slot.
                                let caller_dest = cur.base - 1;
                                let callee_abs = cur.base + callee as usize;
                                let shift = callee_abs - caller_dest;
                                let len = args_end - callee_abs;
                                for k in 0..len {
                                    let v = std::mem::replace(
                                        &mut self.stack[callee_abs + k],
                                        Value::Nil,
                                    );
                                    self.stack[caller_dest + k] = v;
                                }
                                if self.top > callee_abs {
                                    self.top -= shift;
                                }
                                (cur.base, cur.want_ret)
                            } else {
                                (args_start, retc)
                            };
                            let needed = new_base + max_registers;
                            if needed >= self.stack.len() {
                                self.stack.resize(needed + 1, Value::Nil);
                            }
                            for i in argc..num_params {
                                self.stack[new_base + i] = Value::Nil;
                            }
                            let frame_closure = match &next_closure {
                                Some(c) => Rc::clone(c),
                                None => Rc::clone(&current_closure),
                            };
                            let new_frame = if is_vararg && argc > num_params {
                                let varargs =
                                    self.stack[new_base + num_params..new_base + argc].to_vec();
                                CallFrame::with_varargs(frame_closure, new_base, varargs)
                            } else {
                                CallFrame::new(frame_closure, new_base)
                            }
                            .wanting(want_ret);
                            self.frames.push(new_frame);
                            if self.hook.is_some() {
                                let event = if tail {
                                    HookEvent::TailCall
                                } else {
                                    HookEvent::Call
                                };
                                if let Err(err) = self.fire_hook(event) {
                                    self.frames.pop();
                                    sync_ip!();
                                    return Err(err);
                                }
                            }
                            if self.dap.is_some() {
                                let active = next_closure.as_ref().unwrap_or(&current_closure);
                                let func_name =
                                    active.proto.name.as_deref().unwrap_or("<anonymous>");
                                if let Err(err) = crate::dap::check_break(self, func_name) {
                                    self.frames.pop();
                                    sync_ip!();
                                    return Err(err);
                                }
                            }
                            if let Some(next) = next_closure.take() {
                                if !next.proto.is_verified() {
                                    crate::bytecode::verify_proto(&next.proto)
                                        .map_err(|e| e.to_string())?;
                                    next.proto.mark_verified();
                                }
                                current_closure = next;
                                code = &current_closure.proto.instructions;
                                consts = current_closure.proto.values();
                            }
                            base = new_base;
                            ip = 0;
                            continue;
                        }
                        Value::Native(def) => {
                            let func = def.func;
                            // Natives take a slice of owned values; copy small
                            // argument lists onto the stack to avoid a heap
                            // allocation on every native call.
                            const INLINE_NATIVE_ARGS: usize = 8;
                            let mut inline_args = [const { Value::Nil }; INLINE_NATIVE_ARGS];
                            let heap_args: Vec<Value>;
                            let args: &[Value] = if argc <= INLINE_NATIVE_ARGS {
                                for (dst, src) in inline_args[..argc]
                                    .iter_mut()
                                    .zip(&self.stack[args_start..args_end])
                                {
                                    *dst = src.clone();
                                }
                                &inline_args[..argc]
                            } else {
                                heap_args = self.stack[args_start..args_end].to_vec();
                                &heap_args
                            };
                            if self.hook.is_some()
                                && let Err(err) = self.fire_hook(HookEvent::Call)
                            {
                                sync_ip!();
                                return Err(err);
                            }
                            match func(self, args) {
                                Ok(results) => {
                                    if self.hook.is_some()
                                        && let Err(err) = self.fire_hook(HookEvent::Return)
                                    {
                                        sync_ip!();
                                        return Err(err);
                                    }
                                    self.store_call_results(callee, retc, &results);
                                    if self.trap != TRAP {
                                        sync_ip!();
                                        return Ok(None);
                                    }
                                }
                                Err(err) if self.is_yielding => {
                                    if let Some(co_id) = self.current_co
                                        && let Some(co_rc) = self.co_state(co_id)
                                    {
                                        let mut cs = co_rc.borrow_mut();
                                        cs.yield_callee = callee;
                                        cs.yield_retc = retc;
                                    }
                                    return Err(err);
                                }
                                Err(err) => return Err(err),
                            }
                        }
                        Value::Table(t) => {
                            let handler = t.borrow().metamethod("__call");
                            let callee_val = reg!(callee).clone();
                            if let Some(handler) = handler {
                                let mut call_args = Vec::with_capacity(argc + 1);
                                call_args.push(callee_val);
                                call_args.extend_from_slice(&self.stack[args_start..args_end]);
                                let results = self.call_function(handler, &call_args)?;
                                self.store_call_results(callee, retc, &results);
                                if self.trap != TRAP {
                                    sync_ip!();
                                    return Ok(None);
                                }
                            } else {
                                throw!("attempted to call non-function ({:?})", callee_val);
                            }
                        }
                        other => {
                            throw!("attempted to call non-function ({:?})", other);
                        }
                    }
                }
                Instruction::Return { base: ret, count } => {
                    if self.hook.is_some() {
                        sync_ip!();
                        self.fire_hook(HookEvent::Return)?;
                    }
                    let frame = self.frames.pop().expect(RUNNING_FRAME);
                    if !self.open_upvalues.is_empty() {
                        self.close_upvalues(frame.base);
                    }
                    let ret_start = frame.base + ret as usize;
                    let ret_end = if count == MULTRET {
                        self.top.max(ret_start)
                    } else {
                        ret_start + count as usize
                    };
                    let ret_end = ret_end.min(self.stack.len());
                    let nret = ret_end.saturating_sub(ret_start);

                    if self.frames.len() == target_depth {
                        self.returned.clear();
                        self.returned
                            .extend(self.stack[ret_start..ret_end].iter().cloned());
                        let top_val = self.returned.first().cloned().unwrap_or(Value::Nil);
                        return Ok(Some(top_val));
                    }
                    // The results land where the caller's `Call` put the callee.
                    let caller_dest = frame.base - 1;
                    let wanted = match frame.want_ret {
                        MULTRET => nret,
                        0 => 1,
                        n => n as usize,
                    };
                    let needed = caller_dest + wanted;
                    if needed > self.stack.len() {
                        self.stack.resize(needed, Value::Nil);
                    }
                    // Values move down the stack in order; the source and
                    // destination windows may overlap, which is fine when
                    // copying front to back since caller_dest < ret_start.
                    if wanted == 1 && nret == 1 {
                        let src = &mut self.stack[ret_start];
                        match src {
                            Value::Int(n) => {
                                let val = *n;
                                let dest_slot = &mut self.stack[caller_dest];
                                if let Value::Int(old) = dest_slot {
                                    *old = val;
                                } else {
                                    *dest_slot = Value::Int(val);
                                }
                            }
                            Value::Float(f) => {
                                let val = *f;
                                let dest_slot = &mut self.stack[caller_dest];
                                if let Value::Float(old) = dest_slot {
                                    *old = val;
                                } else {
                                    *dest_slot = Value::Float(val);
                                }
                            }
                            _ => {
                                let v = std::mem::replace(src, Value::Nil);
                                self.stack[caller_dest] = v;
                            }
                        }
                    } else {
                        for i in 0..wanted {
                            if i < nret {
                                let src = &mut self.stack[ret_start + i];
                                match src {
                                    Value::Int(n) => {
                                        let val = *n;
                                        let dest_slot = &mut self.stack[caller_dest + i];
                                        if let Value::Int(old) = dest_slot {
                                            *old = val;
                                        } else {
                                            *dest_slot = Value::Int(val);
                                        }
                                    }
                                    Value::Float(f) => {
                                        let val = *f;
                                        let dest_slot = &mut self.stack[caller_dest + i];
                                        if let Value::Float(old) = dest_slot {
                                            *old = val;
                                        } else {
                                            *dest_slot = Value::Float(val);
                                        }
                                    }
                                    _ => {
                                        let v = std::mem::replace(src, Value::Nil);
                                        self.stack[caller_dest + i] = v;
                                    }
                                }
                            } else {
                                self.stack[caller_dest + i] = Value::Nil;
                            }
                        }
                    }
                    if frame.want_ret == MULTRET {
                        self.top = caller_dest + nret;
                    }
                    let caller_frame = self.frames.last().expect(RUNNING_FRAME);
                    if !Rc::ptr_eq(&current_closure, &caller_frame.closure) {
                        current_closure = caller_frame.closure.clone();
                        code = &current_closure.proto.instructions;
                        consts = current_closure.proto.values();
                    }
                    base = caller_frame.base;
                    ip = caller_frame.ip;
                    if self.trap != TRAP {
                        sync_ip!();
                        return Ok(None);
                    }
                    continue;
                }
                Instruction::Vararg { dst, count } => {
                    let all = count == 0 || count == MULTRET;
                    let frame = self.frames.last().expect(RUNNING_FRAME);
                    let cnt = if all {
                        frame.varargs().len()
                    } else {
                        count as usize
                    };
                    let dest = base + dst as usize;
                    if dest + cnt > self.stack.len() {
                        self.stack.resize(dest + cnt, Value::Nil);
                    }
                    let frame = self.frames.last().expect(RUNNING_FRAME);
                    let varargs = frame.varargs();
                    for i in 0..cnt {
                        let val = varargs.get(i).cloned().unwrap_or(Value::Nil);
                        self.stack[dest + i] = val;
                    }
                    if all {
                        self.top = dest + cnt;
                    }
                }
            }
        }
    }

    /// `==` with the `__eq` metamethod fallback for two tables.
    #[inline]
    fn values_equal(&mut self, base: usize, a: u8, b: u8, ip: usize) -> Result<bool, String> {
        let va = &self.stack[base + a as usize];
        let vb = &self.stack[base + b as usize];
        if va == vb {
            return Ok(true);
        }
        if let (Value::Table(_), Value::Table(_)) = (va, vb)
            && let Some(mm) = self.get_binop_metamethod(va, vb, "__eq")
        {
            let (va, vb) = (va.clone(), vb.clone());
            self.frames.last_mut().expect(RUNNING_FRAME).ip = ip;
            return self.call_cmp_mm(mm, va, vb);
        }
        Ok(false)
    }

    /// The generic-for step. `b` is the loop's base register: iterator,
    /// state, control, then the loop variables.
    fn tfor_call(&mut self, base: usize, b: u8, retc: u8) -> Result<(), String> {
        let iter_idx = base + b as usize;
        let var_base = iter_idx + 3;
        let retc_usize = retc as usize;
        if var_base + retc_usize > self.stack.len() {
            self.stack.resize(var_base + retc_usize, Value::Nil);
        }

        // Fast path: direct iteration over a plain table (`for k, v in t`) or
        // after `pairs(t)` rewrites the iterator slot to the table on step 1.
        let direct_table = match &self.stack[iter_idx] {
            Value::Table(t) => t.borrow().metamethod("__call").is_none(),
            Value::Native(def)
                if std::ptr::eq(*def, crate::vm::builtins::NEXT)
                    && matches!(self.stack[iter_idx + 2], Value::Nil)
                    && matches!(self.stack[iter_idx + 1], Value::Table(_)) =>
            {
                let tbl = std::mem::replace(&mut self.stack[iter_idx + 1], Value::Nil);
                self.stack[iter_idx] = tbl;
                true
            }
            _ => false,
        };
        if direct_table {
            let (head, tail) = self.stack.split_at_mut(iter_idx + 1);
            if let Value::Table(t) = &head[iter_idx] {
                let (state_slice, rest) = tail.split_at_mut(2);
                Self::table_for_step_into(t, &mut state_slice[0], &mut rest[..retc_usize]);
                return Ok(());
            }
        }

        let iter_fn = self.stack[iter_idx].clone();
        let state = self.stack[iter_idx + 1].clone();
        let ctrl = self.stack[iter_idx + 2].clone();
        let results = self.call_function(iter_fn, &[state, ctrl])?;
        if var_base + retc_usize > self.stack.len() {
            self.stack.resize(var_base + retc_usize, Value::Nil);
        }
        for i in 0..retc_usize {
            self.stack[var_base + i] = results.get(i).cloned().unwrap_or(Value::Nil);
        }
        Ok(())
    }

    /// One step of `for k, v in t`, writing results straight into `vars` with
    /// zero intermediate allocations. `state_slot` holds the progress: nil
    /// before the first step, an integer while walking the array part, then a
    /// snapshot of the remaining keys (stored reversed, popped from the end).
    /// Entries removed during the loop are skipped; keys added after the
    /// snapshot are not visited.
    fn table_for_step_into(t: &RefCell<VmTable>, state_slot: &mut Value, vars: &mut [Value]) {
        #[inline]
        fn write_entry(vars: &mut [Value], key: Value, value: Value) {
            if vars.len() > 1 {
                vars[0] = key;
                vars[1] = value;
                for slot in &mut vars[2..] {
                    *slot = Value::Nil;
                }
            } else if let Some(slot) = vars.first_mut() {
                *slot = value;
            }
        }

        let tbl = t.borrow();
        let mut next_index = match state_slot {
            Value::Int(i) => *i as usize,
            Value::Table(_) => usize::MAX,
            _ => 0,
        };
        while next_index < tbl.array.len() {
            let v = &tbl.array[next_index];
            next_index += 1;
            if !matches!(v, Value::Nil) {
                let v = v.clone();
                drop(tbl);
                let idx_val = Value::from_usize(next_index);
                *state_slot = idx_val.clone();
                write_entry(vars, idx_val, v);
                return;
            }
        }
        let snapshot = match state_slot {
            Value::Table(snapshot) => snapshot,
            _ => {
                if tbl.fields.is_empty() && tbl.general.is_none() {
                    drop(tbl);
                    for slot in vars {
                        *slot = Value::Nil;
                    }
                    return;
                }
                let mut keys = crate::vm::table::sorted_keys(&tbl);
                keys.reverse();
                let mut snapshot = VmTable::new();
                snapshot.array = keys;
                *state_slot = Value::Table(Rc::new(RefCell::new(snapshot)));
                match state_slot {
                    Value::Table(s) => s,
                    _ => unreachable!("just assigned Value::Table"),
                }
            }
        };
        let mut snapshot = snapshot.borrow_mut();
        while let Some(key) = snapshot.array.pop() {
            let value = tbl.raw_get(&key);
            if !matches!(value, Value::Nil) {
                drop(snapshot);
                drop(tbl);
                write_entry(vars, key, value);
                return;
            }
        }
        drop(snapshot);
        drop(tbl);
        for slot in vars {
            *slot = Value::Nil;
        }
    }

    // ----- tables ---------------------------------------------------------

    /// Raw lookup of an existing, non-nil entry. `None` means the slow path
    /// (metamethods, buffers, strings, errors) has to decide.
    #[inline]
    fn table_get_fast(table: &Value, key: &Value) -> Option<Value> {
        let Value::Table(t) = table else {
            return None;
        };
        let tbl = t.borrow();
        let found = match key {
            Value::Int(i) if *i >= 1 && (*i as u64) <= tbl.array.len() as u64 => {
                tbl.array[*i as usize - 1].clone()
            }
            Value::String(k) => tbl.fields.get_by_strref(k).cloned().unwrap_or(Value::Nil),
            Value::BigInt(_) => return None,
            other => tbl.raw_get(other),
        };
        if matches!(found, Value::Nil) && tbl.metatable.is_some() {
            return None;
        }
        Some(found)
    }

    /// Raw store into an existing array slot or field, or an append, on a
    /// table that nothing can intercept. Returns false to defer to the slow
    /// path, which also handles nil (removal) and reports errors.
    #[inline]
    fn table_set_fast(table: &Value, key: &Value, val: &Value) -> bool {
        let Value::Table(t) = table else {
            return false;
        };
        if matches!(val, Value::Nil) {
            return false;
        }
        let mut tbl = t.borrow_mut();
        if tbl.frozen {
            return false;
        }
        match key {
            Value::Int(i) if *i >= 1 => {
                let idx = *i as usize - 1;
                if idx < tbl.array.len() {
                    match (val, &mut tbl.array[idx]) {
                        (Value::Int(n), Value::Int(o)) => *o = *n,
                        (Value::Float(n), Value::Float(o)) => *o = *n,
                        (Value::Bool(n), Value::Bool(o)) => *o = *n,
                        (v, slot) => *slot = v.clone(),
                    }
                    true
                } else if idx == tbl.array.len() && tbl.metatable.is_none() && tbl.general.is_none()
                {
                    tbl.array.push(val.clone());
                    true
                } else {
                    false
                }
            }
            Value::String(k) => {
                if tbl.metatable.is_none() {
                    tbl.fields.set_by_strref(k, val);
                    true
                } else if let Some(slot) = tbl.fields.get_mut_by_strref(k) {
                    *slot = val.clone();
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    pub fn table_get(&mut self, table: &Value, key: &Value) -> Result<Value, String> {
        self.table_get_depth(table, key, 0)
    }

    fn table_get_depth(
        &mut self,
        table: &Value,
        key: &Value,
        depth: usize,
    ) -> Result<Value, String> {
        if depth > 100 {
            return Err("loop in gettable / __index chain".to_string());
        }
        match table {
            Value::Table(t) => {
                if let Value::BigInt(b) = key
                    && num_traits::Signed::is_positive(&**b)
                {
                    return Err("table index too large".to_string());
                }
                let val = t.borrow().raw_get(key);
                if !matches!(val, Value::Nil) {
                    return Ok(val);
                }

                // Check __index metamethod
                let index_handler = t.borrow().metamethod("__index");
                match index_handler {
                    Some(handler @ Value::Table(_)) => {
                        return self.table_get_depth(&handler, key, depth + 1);
                    }
                    Some(handler @ (Value::Native(_) | Value::Closure(_))) => {
                        let res = self.call_function(handler, &[table.clone(), key.clone()])?;
                        return Ok(res.into_iter().next().unwrap_or(Value::Nil));
                    }
                    _ => {}
                }
                Ok(Value::Nil)
            }
            Value::Buffer(b) => {
                let buf = b.borrow();
                match key {
                    Value::Int(idx) => {
                        let i = usize::try_from(*idx)
                            .map_err(|_| "buffer index too large".to_string())?;
                        let val = buf.read_u8(i)?;
                        Ok(Value::Int(val as i64))
                    }
                    Value::BigInt(_) => Err("buffer index too large".to_string()),
                    _ => Ok(Value::Nil),
                }
            }
            // Strings index the string library, so `s:upper()` works.
            Value::String(_) => match (&self.string_lib, key) {
                (Value::Table(lib), Value::String(name)) => Ok(lib
                    .borrow()
                    .fields
                    .get_by_strref(name)
                    .cloned()
                    .unwrap_or(Value::Nil)),
                _ => Ok(Value::Nil),
            },
            other => Err(format!("attempted to index a {} value", other.type_name())),
        }
    }

    pub fn table_set(&mut self, table: &Value, key: Value, val: Value) -> Result<(), String> {
        self.table_set_depth(table, key, val, 0)
    }

    fn table_set_depth(
        &mut self,
        table: &Value,
        key: Value,
        val: Value,
        depth: usize,
    ) -> Result<(), String> {
        if depth > 100 {
            return Err("loop in settable / __newindex chain".to_string());
        }
        self.gc.write_barrier(table, &val);
        match table {
            Value::Table(t) => {
                if t.borrow().frozen {
                    return Err("attempted to mutate a frozen table".to_string());
                }

                if let Value::BigInt(b) = &key
                    && num_traits::Signed::is_positive(&**b)
                {
                    return Err("table index too large".to_string());
                }
                let key_exists = !matches!(t.borrow().raw_get(&key), Value::Nil);
                if !key_exists {
                    let newindex_handler = t.borrow().metamethod("__newindex");
                    match newindex_handler {
                        Some(handler @ Value::Table(_)) => {
                            return self.table_set_depth(&handler, key, val, depth + 1);
                        }
                        Some(handler @ (Value::Native(_) | Value::Closure(_))) => {
                            self.call_function(handler, &[table.clone(), key, val])?;
                            return Ok(());
                        }
                        _ => {}
                    }
                }
                t.borrow_mut().raw_set(key, val)
            }
            Value::Buffer(b) => {
                let mut buf = b.borrow_mut();
                match key {
                    Value::Int(idx) => {
                        let i = usize::try_from(idx)
                            .map_err(|_| "buffer index too large".to_string())?;
                        let byte_val = match val {
                            Value::Int(v) => u8::try_from(v)
                                .map_err(|_| "value out of byte range".to_string())?,
                            Value::Float(f) => f as u8,
                            _ => return Err("buffer value expects byte".to_string()),
                        };
                        buf.write_u8(i, byte_val)?;
                        Ok(())
                    }
                    Value::BigInt(_) => Err("buffer index too large".to_string()),
                    _ => Err("invalid buffer index".to_string()),
                }
            }
            other => Err(format!(
                "attempted to set a field on a {} value",
                other.type_name()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(
            matches!(res, Value::Int(7)),
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
            &crate::vm::value::Value::Int(1),
            &crate::vm::value::Value::Int(0),
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
            &crate::vm::value::Value::Int(1),
            &crate::vm::value::Value::Int(0),
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
    fn test_deep_clone_table_isolates_nested_tables_and_metatables() {
        let mut inner = VmTable::new();
        inner.fields.insert(StrRef::from("sep"), Value::str("/"));
        let mut mt = VmTable::new();
        mt.fields.insert(StrRef::from("tag"), Value::Int(1));
        let mut root = VmTable::new();
        root.fields.insert(
            StrRef::from("path"),
            Value::Table(Rc::new(RefCell::new(inner))),
        );
        root.metatable = Some(Rc::new(RefCell::new(mt)));

        assert!(table_needs_deep_clone(&root));
        let clone1 = deep_clone_table(&root);
        let clone2 = deep_clone_table(&root);

        // Mutate nested table and metatable on clone1; clone2 and root must be untouched.
        if let Some(Value::Table(p1)) = clone1.borrow().fields.get("path") {
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
            .insert(StrRef::from("tag"), Value::Int(99));

        let sep2 = match clone2.borrow().fields.get("path") {
            Some(Value::Table(p2)) => p2.borrow().fields.get("sep").unwrap().to_string(),
            _ => panic!("missing nested table"),
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
        assert_eq!(tag2, Value::Int(1));
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
}
