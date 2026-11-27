//! Function invocation, module execution, call hooks, and return value passing.

use std::rc::Rc;

use crate::bytecode::instruction::MULTRET;
use crate::bytecode::proto::Proto;
use crate::vm::frame::CallFrame;
use crate::vm::machine::{HookEvent, VM};
use crate::vm::value::{Value, ValueView, VmClosure};

/// How many callable tables a call follows through `__call` before giving
/// up, like the `__index` chain limit.
const MAX_CALL_CHAIN: usize = 100;

impl VM {
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
            self.stack.resize(needed + 1, Value::NIL);
        }
        let beat_floor = self.beat_enter_nested();
        self.frames.push(CallFrame::new(closure, base));
        let mut result = self.run_to_depth(depth);
        self.beat_unwind_frames(depth + 1);
        self.frames.truncate(depth);
        // Upvalues captured by the module's closures live on in the closures
        // themselves; closing them here just moves the values across.
        self.close_upvalues(prev_stack_len);
        self.stack.truncate(prev_stack_len);
        if let Ok(ref mut val) = result {
            self.beat_escape_if_needed(val);
        }
        self.beat_flush_to_floor(beat_floor, false);
        self.beat_exit_nested();
        result
    }

    /// Writes the results of a native (or `__call`) invocation back into the
    /// caller's registers, starting at the register that held the callee.
    pub(crate) fn store_call_results(&mut self, callee: u8, retc: u8, results: Vec<Value>) {
        let base = self.frames.last().map(|f| f.base).unwrap_or(0);
        let dest = base + callee as usize;
        let count = match retc {
            MULTRET => results.len(),
            0 => 1,
            n => n as usize,
        };
        if dest + count > self.stack.len() {
            self.stack.resize(dest + count, Value::NIL);
        }
        let mut iter = results.into_iter();
        for i in 0..count {
            self.stack[dest + i] = iter.next().unwrap_or(Value::NIL);
        }
        if retc == MULTRET {
            self.top = dest + count;
        }
    }

    pub fn call_function(&mut self, func: Value, args: &[Value]) -> Result<Vec<Value>, String> {
        self.native_calls += 1;
        let _beat_floor = self.beat_enter_nested();
        let res = self.call_function_inner(func, args);
        self.beat_exit_nested();
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
        match func.view() {
            ValueView::Native(def) => {
                self.fire_hook(HookEvent::Call)?;
                match (def.func)(self, args) {
                    Ok(results) => {
                        self.fire_hook(HookEvent::Return)?;
                        Ok(results)
                    }
                    Err(err) => Err(err),
                }
            }
            ValueView::Closure(c) => {
                let c = c.to_rc();
                let depth = self.frames.len();
                let prev_stack_len = self.stack.len();
                let base = prev_stack_len + 1;
                let needed = base + c.proto.max_registers as usize + args.len() + 1;
                if needed >= self.stack.len() {
                    self.stack.resize(needed + 1, Value::NIL);
                }
                for (i, arg) in args.iter().enumerate() {
                    self.stack[base + i] = arg.clone();
                }
                let num_params = c.proto.num_params as usize;
                for i in args.len()..num_params {
                    self.stack[base + i] = Value::NIL;
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
                        self.beat_unwind_frames(depth + 1);
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
                        self.beat_unwind_frames(depth + 1);
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
                            self.beat_unwind_frames(depth + 1);
                            self.frames.truncate(depth);
                            self.close_upvalues(prev_stack_len);
                            self.stack.truncate(prev_stack_len);
                        }
                        Err(err)
                    }
                }
            }
            ValueView::Table(_) => {
                // A `__call` handler may itself be a table with `__call`:
                // follow the chain, each table becoming the first argument
                // of the next. A loop would otherwise recurse forever,
                // growing the arguments each time.
                let mut callee = func;
                let mut call_args = args.to_vec();
                for _ in 0..MAX_CALL_CHAIN {
                    let Some(handler) = callee.table_borrow().map(|t| t.metamethod("__call"))
                    else {
                        return self.call_function_inner(callee, &call_args);
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
            ValueView::Look(_) => Ok(vec![self.resolve_look_value(&func)]),
            _ => Err("attempt to call a non-function value".to_string()),
        }
    }

    /// Runs the active debug hook for `event`, if one is set and listening.
    /// Hooks never trigger hooks (`in_hook` guard); a hook error propagates
    /// to the caller, which is responsible for unwinding its own frame.
    pub(crate) fn fire_hook(&mut self, event: HookEvent) -> Result<(), String> {
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

    pub(crate) fn call_hook(&mut self, func: &Value, args: &[Value]) -> Result<(), String> {
        self.in_hook = true;
        let res = self.call_function(func.clone(), args);
        self.in_hook = false;
        res.map(|_| ())
    }

    /// Every value the last frame to finish at a `run_to_depth` boundary
    /// returned.
    pub fn take_returned(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.returned)
    }
}
