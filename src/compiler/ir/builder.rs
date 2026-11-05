//! Lowers the syntax tree to IR, resolving scopes and variables.

use num_bigint::BigInt;
use std::collections::HashMap;
use std::str::FromStr;

use crate::ast::node_id::NodeId;
use crate::ast::op::{BinOp, UnOp};
use crate::ast::pattern::AssignTarget;
use crate::ast::{Expr, InterpPart, Literal, Param, Stmt};
use crate::compiler::ir::block::{IrFunction, IrModule, UpvalSource};
use crate::compiler::ir::inst::{IrInst, SpreadSink, SpreadSource};
use crate::compiler::ir::types::{IrBinaryOp, IrConstant, IrLabel, IrUnaryOp, IrVar};

pub fn parse_int_from_str(s: &str) -> Option<BigInt> {
    BigInt::from_str(s).ok()
}

struct IrLoopContext {
    break_label: IrLabel,
    continue_label: IrLabel,
}

use crate::bytecode::proto::UpvalueDesc;

pub struct IrBuilder {
    /// Source line of each statement, by node id.
    lines: Option<std::rc::Rc<HashMap<NodeId, u32>>>,
    current_line: u32,
    /// Keep locals alive until their scope ends, for the debugger.
    keep_locals: bool,
    /// The variables declared in each open scope, in declaration order.
    scope_vars: Vec<Vec<IrVar>>,
    next_var: u32,
    next_label: usize,
    instructions: Vec<IrInst>,
    scopes: Vec<HashMap<String, IrVar>>,
    loops: Vec<IrLoopContext>,
    child_protos: Vec<IrFunction>,
    named_labels: HashMap<String, IrLabel>,
    parent_scopes: Vec<Vec<HashMap<String, IrVar>>>,
    upvalues: Vec<UpvalueDesc>,
    upvalue_vars: Vec<UpvalSource>,
    upvalue_names: Vec<String>,
}

impl Default for IrBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl IrBuilder {
    pub fn new() -> Self {
        Self {
            lines: None,
            current_line: 0,
            keep_locals: false,
            scope_vars: vec![Vec::new()],
            next_var: 0,
            next_label: 0,
            instructions: Vec::new(),
            scopes: vec![HashMap::new()],
            loops: Vec::new(),
            child_protos: Vec::new(),
            named_labels: HashMap::new(),
            parent_scopes: Vec::new(),
            upvalues: Vec::new(),
            upvalue_vars: Vec::new(),
            upvalue_names: Vec::new(),
        }
    }

    pub fn get_or_alloc_label(&mut self, name: &str) -> IrLabel {
        if let Some(&lbl) = self.named_labels.get(name) {
            lbl
        } else {
            let lbl = self.alloc_label();
            self.named_labels.insert(name.to_string(), lbl);
            lbl
        }
    }

    pub fn alloc_var(&mut self) -> IrVar {
        let v = IrVar(self.next_var);
        self.next_var += 1;
        v
    }

    pub fn alloc_label(&mut self) -> IrLabel {
        let l = IrLabel(self.next_label);
        self.next_label += 1;
        l
    }

    pub fn enter_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.scope_vars.push(Vec::new());
    }

    pub fn exit_scope(&mut self) {
        self.scopes.pop();
        let vars = self.scope_vars.pop().unwrap_or_default();
        let ended = self
            .instructions
            .last()
            .is_some_and(|inst| inst.is_terminator());
        if !vars.is_empty() && !ended {
            self.emit(IrInst::DebugScopeEnd {
                vars,
                keep_alive: self.keep_locals,
            });
        }
    }

    pub fn add_local(&mut self, name: String, var: IrVar) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.clone(), var);
        }
        if let Some(vars) = self.scope_vars.last_mut() {
            vars.push(var);
        }
        self.emit(IrInst::DebugLocal { name, var });
    }

    /// Emits a line marker for `stmt` when it starts on a new line.
    fn mark_line(&mut self, id: NodeId) {
        let Some(lines) = &self.lines else {
            return;
        };
        if let Some(&line) = lines.get(&id)
            && line != self.current_line
        {
            self.current_line = line;
            self.emit(IrInst::Line(line));
        }
    }

    pub fn resolve_local(&self, name: &str) -> Option<IrVar> {
        for scope in self.scopes.iter().rev() {
            if let Some(&var) = scope.get(name) {
                return Some(var);
            }
        }
        None
    }

    pub fn resolve_variable(&mut self, name: &str) -> (bool, Option<u8>, Option<IrVar>) {
        if let Some(var) = self.resolve_local(name) {
            return (true, None, Some(var));
        }
        if let Some(upval_idx) = self.resolve_upvalue(name) {
            return (false, Some(upval_idx), None);
        }
        (false, None, None)
    }

    fn resolve_upvalue(&mut self, name: &str) -> Option<u8> {
        let num_parents = self.parent_scopes.len();
        // Innermost enclosing function first; `p` counts down through the
        // ancestors, so the variable's distance is how far below the top it is.
        for p in (0..num_parents).rev() {
            for scope in self.parent_scopes[p].iter().rev() {
                if let Some(&var) = scope.get(name) {
                    let levels_up = num_parents - 1 - p;
                    return Some(self.capture(var, levels_up, name));
                }
            }
        }
        None
    }

    /// Records that this function captures `var`, which lives `levels_up`
    /// functions above the immediately enclosing one (0 means in it), and
    /// returns its upvalue slot. A variable further out has to be captured by
    /// the enclosing function too, which is settled once this one is attached
    /// to it. Each descriptor's `index` is filled in then, or at lowering for
    /// a parent's register.
    fn capture(&mut self, var: IrVar, levels_up: usize, name: &str) -> u8 {
        let wanted = if levels_up == 0 {
            UpvalSource::ParentLocal(var)
        } else {
            UpvalSource::Pending { var, levels_up }
        };
        if let Some(i) = self.upvalue_vars.iter().position(|s| *s == wanted) {
            return i as u8;
        }
        let idx = self.upvalues.len() as u8;
        self.upvalues.push(UpvalueDesc {
            in_stack: levels_up == 0,
            index: 0,
        });
        self.upvalue_vars.push(wanted);
        self.upvalue_names.push(name.to_string());
        idx
    }

    /// True when `expr` can yield more than one value at runtime.
    fn is_multi_value(expr: &Expr) -> bool {
        matches!(
            expr,
            Expr::Call { .. } | Expr::MethodCall { .. } | Expr::Vararg { .. }
        )
    }

    fn last_spreads(args: &[Expr]) -> bool {
        args.last().is_some_and(Self::is_multi_value)
    }

    /// Compiles `expr` as the producer of a run of values. Only called for an
    /// expression `is_multi_value` accepted.
    fn spread_source(&mut self, expr: &Expr) -> SpreadSource {
        match expr {
            Expr::Call { callee, args, .. } => {
                let callee_var = self.compile_expr(callee, None);
                let spreads = Self::last_spreads(args);
                let fixed = if spreads {
                    &args[..args.len() - 1]
                } else {
                    &args[..]
                };
                let arg_vars = fixed
                    .iter()
                    .map(|arg| self.compile_expr(arg, None))
                    .collect();
                let trailing = spreads.then(|| Box::new(self.spread_source(&args[args.len() - 1])));
                SpreadSource::Call {
                    callee: callee_var,
                    args: arg_vars,
                    trailing,
                }
            }
            Expr::MethodCall {
                object,
                method,
                args,
                ..
            } => {
                let tbl = self.compile_expr(object, None);
                let key = self.alloc_var();
                self.emit(IrInst::LoadConst {
                    dst: key,
                    val: IrConstant::String(method.clone()),
                });
                let callee = self.alloc_var();
                self.emit(IrInst::GetTable {
                    dst: callee,
                    table: tbl,
                    key,
                });
                let spreads = Self::last_spreads(args);
                let fixed = if spreads {
                    &args[..args.len() - 1]
                } else {
                    &args[..]
                };
                let mut arg_vars = vec![tbl];
                for arg in fixed {
                    arg_vars.push(self.compile_expr(arg, None));
                }
                let trailing = spreads.then(|| Box::new(self.spread_source(&args[args.len() - 1])));
                SpreadSource::Call {
                    callee,
                    args: arg_vars,
                    trailing,
                }
            }
            _ => SpreadSource::Vararg,
        }
    }

    /// Compiles `expr` as a call asked for `count` results, one variable each.
    /// Returns `None` when `expr` is not a call, so the caller falls back to
    /// evaluating it as a single value.
    fn compile_call_results(&mut self, expr: &Expr, count: usize) -> Option<Vec<IrVar>> {
        let (callee, receiver, args) = match expr {
            Expr::Call { callee, args, .. } => (self.compile_expr(callee, None), None, args),
            Expr::MethodCall {
                object,
                method,
                args,
                ..
            } => {
                let tbl = self.compile_expr(object, None);
                let key = self.alloc_var();
                self.emit(IrInst::LoadConst {
                    dst: key,
                    val: IrConstant::String(method.clone()),
                });
                let callee = self.alloc_var();
                self.emit(IrInst::GetTable {
                    dst: callee,
                    table: tbl,
                    key,
                });
                (callee, Some(tbl), args)
            }
            _ => return None,
        };

        let spreads = Self::last_spreads(args);
        let fixed = if spreads {
            &args[..args.len() - 1]
        } else {
            &args[..]
        };
        let mut arg_vars: Vec<IrVar> = receiver.into_iter().collect();
        for arg in fixed {
            arg_vars.push(self.compile_expr(arg, None));
        }
        let dsts: Vec<IrVar> = (0..count).map(|_| self.alloc_var()).collect();
        if spreads {
            let source = self.spread_source(&args[args.len() - 1]);
            self.emit(IrInst::Spread {
                source,
                sink: SpreadSink::Call {
                    dsts: dsts.clone(),
                    callee,
                    fixed_args: arg_vars,
                    retc: count as u8,
                },
            });
        } else {
            self.emit(IrInst::Call {
                dsts: dsts.clone(),
                callee,
                args: arg_vars,
                retc: count as u8,
            });
        }
        Some(dsts)
    }

    /// The parent variables a freshly compiled nested prototype captures.
    fn captures_of(&self, proto_idx: u16) -> Vec<Option<IrVar>> {
        self.child_protos
            .get(proto_idx as usize)
            .map(|child| {
                child
                    .upvalue_vars
                    .iter()
                    .map(|source| match source {
                        UpvalSource::ParentLocal(var) => Some(*var),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn emit(&mut self, inst: IrInst) {
        self.instructions.push(inst);
    }

    /// A close of every variable created since `first_var` that a closure
    /// emitted since instruction `from` captures, if there are any. Run where
    /// each loop iteration starts, it gives every iteration fresh copies of
    /// the loop's locals, so closures made in different iterations don't
    /// share them.
    fn loop_close(&self, from: usize, first_var: u32) -> Option<IrInst> {
        let mut vars: Vec<IrVar> = self.instructions[from..]
            .iter()
            .filter_map(|inst| match inst {
                IrInst::Closure { captures, .. } => Some(captures),
                _ => None,
            })
            .flatten()
            .flatten()
            .copied()
            .filter(|var| var.0 >= first_var)
            .collect();
        if vars.is_empty() {
            return None;
        }
        vars.sort_by_key(|var| var.0);
        vars.dedup();
        Some(IrInst::CloseUpvals { vars })
    }

    /// Inserts `loop_close(at, first_var)` at instruction `at`.
    fn insert_loop_close(&mut self, at: usize, first_var: u32) {
        if let Some(close) = self.loop_close(at, first_var) {
            self.instructions.insert(at, close);
        }
    }

    pub fn compile_expr(&mut self, expr: &Expr, target: Option<IrVar>) -> IrVar {
        let dst = target.unwrap_or_else(|| self.alloc_var());

        match expr {
            Expr::Literal { value: lit, .. } => match lit {
                crate::ast::Literal::Nil => {
                    self.emit(IrInst::LoadNil { dst });
                }
                crate::ast::Literal::Bool(b) => {
                    self.emit(IrInst::LoadConst {
                        dst,
                        val: IrConstant::Bool(*b),
                    });
                }
                crate::ast::Literal::Int(bi) => {
                    self.emit(IrInst::LoadConst {
                        dst,
                        val: IrConstant::Int(bi.clone()),
                    });
                }
                crate::ast::Literal::Float(f) => {
                    self.emit(IrInst::LoadConst {
                        dst,
                        val: IrConstant::Float(*f),
                    });
                }
                crate::ast::Literal::String(s) => {
                    self.emit(IrInst::LoadConst {
                        dst,
                        val: IrConstant::String(s.clone()),
                    });
                }
            },
            Expr::Interp { parts, .. } => {
                if parts.is_empty() {
                    self.emit(IrInst::LoadConst {
                        dst,
                        val: IrConstant::String(String::new()),
                    });
                } else {
                    let mut prev_var: Option<IrVar> = None;
                    // Concatenating onto "" makes `{n}` alone a string too.
                    if matches!(parts.first(), Some(InterpPart::Expr(_))) {
                        let empty = self.alloc_var();
                        self.emit(IrInst::LoadConst {
                            dst: empty,
                            val: IrConstant::String(String::new()),
                        });
                        prev_var = Some(empty);
                    }
                    for part in parts {
                        let part_var = match part {
                            InterpPart::Literal(lit) => {
                                let v = self.alloc_var();
                                self.emit(IrInst::LoadConst {
                                    dst: v,
                                    val: IrConstant::String(lit.clone()),
                                });
                                v
                            }
                            InterpPart::Expr(e) => self.compile_expr(e, None),
                        };
                        if let Some(prev) = prev_var {
                            let next = self.alloc_var();
                            self.emit(IrInst::BinOp {
                                dst: next,
                                op: IrBinaryOp::Concat,
                                lhs: prev,
                                rhs: part_var,
                            });
                            prev_var = Some(next);
                        } else {
                            prev_var = Some(part_var);
                        }
                    }
                    if let Some(res) = prev_var {
                        self.emit(IrInst::Move { dst, src: res });
                    }
                }
            }
            Expr::Vararg { .. } => {
                self.emit(IrInst::Vararg { dst, count: 1 });
            }
            Expr::Variable { name, .. } => {
                let (_, upval, local_var) = self.resolve_variable(name);
                if let Some(src) = local_var {
                    self.emit(IrInst::Move { dst, src });
                } else if let Some(upval_idx) = upval {
                    self.emit(IrInst::GetUpval {
                        dst,
                        index: upval_idx,
                    });
                } else {
                    self.emit(IrInst::GetGlobal {
                        dst,
                        name: name.clone(),
                    });
                }
            }
            Expr::Binary {
                left, op, right, ..
            } if *op == BinOp::And => {
                let l_var = self.compile_expr(left, Some(dst));
                if l_var != dst {
                    self.emit(IrInst::Move { dst, src: l_var });
                }
                let end_label = self.alloc_label();
                self.emit(IrInst::JumpIfFalse {
                    cond: dst,
                    target: end_label,
                });
                let r_var = self.compile_expr(right, Some(dst));
                if r_var != dst {
                    self.emit(IrInst::Move { dst, src: r_var });
                }
                self.emit(IrInst::Label(end_label));
            }
            Expr::Binary {
                left, op, right, ..
            } if *op == BinOp::Or => {
                let l_var = self.compile_expr(left, Some(dst));
                if l_var != dst {
                    self.emit(IrInst::Move { dst, src: l_var });
                }
                let else_label = self.alloc_label();
                let end_label = self.alloc_label();
                self.emit(IrInst::JumpIfFalse {
                    cond: dst,
                    target: else_label,
                });
                self.emit(IrInst::Jump(end_label));
                self.emit(IrInst::Label(else_label));
                let r_var = self.compile_expr(right, Some(dst));
                if r_var != dst {
                    self.emit(IrInst::Move { dst, src: r_var });
                }
                self.emit(IrInst::Label(end_label));
            }
            Expr::Binary {
                left, op, right, ..
            } if *op == BinOp::Coalesce => {
                // Short-circuits like `or`, but tests for nil rather than
                // falsiness: the right side runs only when the left is nil.
                // The Eq feeding the branch fuses into one compare-and-jump.
                let l_var = self.compile_expr(left, Some(dst));
                if l_var != dst {
                    self.emit(IrInst::Move { dst, src: l_var });
                }
                let nil = self.alloc_var();
                self.emit(IrInst::LoadNil { dst: nil });
                let is_nil = self.alloc_var();
                self.emit(IrInst::BinOp {
                    dst: is_nil,
                    op: IrBinaryOp::Eq,
                    lhs: dst,
                    rhs: nil,
                });
                let end_label = self.alloc_label();
                self.emit(IrInst::JumpIfFalse {
                    cond: is_nil,
                    target: end_label,
                });
                let r_var = self.compile_expr(right, Some(dst));
                if r_var != dst {
                    self.emit(IrInst::Move { dst, src: r_var });
                }
                self.emit(IrInst::Label(end_label));
            }
            Expr::Binary {
                left, op, right, ..
            } => {
                let lhs = self.compile_expr(left, None);
                let rhs = self.compile_expr(right, None);
                let ir_op = match op {
                    BinOp::Add => IrBinaryOp::Add,
                    BinOp::Sub => IrBinaryOp::Sub,
                    BinOp::Mul => IrBinaryOp::Mul,
                    BinOp::Div => IrBinaryOp::Div,
                    BinOp::IDiv => IrBinaryOp::IDiv,
                    BinOp::Mod => IrBinaryOp::Mod,
                    BinOp::Pow => IrBinaryOp::Pow,
                    BinOp::BitAnd => IrBinaryOp::BitAnd,
                    BinOp::BitOr => IrBinaryOp::BitOr,
                    BinOp::BitXor => IrBinaryOp::BitXor,
                    BinOp::Shl => IrBinaryOp::Shl,
                    BinOp::Shr => IrBinaryOp::Shr,
                    BinOp::LShl => IrBinaryOp::LShl,
                    BinOp::LShr => IrBinaryOp::LShr,
                    BinOp::Concat => IrBinaryOp::Concat,
                    BinOp::Eq => IrBinaryOp::Eq,
                    BinOp::Ne => IrBinaryOp::Ne,
                    BinOp::Lt => IrBinaryOp::Lt,
                    BinOp::Le => IrBinaryOp::Le,
                    BinOp::Gt => IrBinaryOp::Gt,
                    BinOp::Ge => IrBinaryOp::Ge,
                    _ => IrBinaryOp::Add,
                };
                self.emit(IrInst::BinOp {
                    dst,
                    op: ir_op,
                    lhs,
                    rhs,
                });
            }
            Expr::Unary { op, expr, .. } => {
                let src = self.compile_expr(expr, None);
                let ir_op = match op {
                    UnOp::Neg => IrUnaryOp::Neg,
                    UnOp::Not => IrUnaryOp::Not,
                    UnOp::Len => IrUnaryOp::Len,
                    UnOp::BitNot => IrUnaryOp::BitNot,
                };
                self.emit(IrInst::UnOp {
                    dst,
                    op: ir_op,
                    src,
                });
            }
            Expr::Call { callee, args, .. } => {
                let callee_var = self.compile_expr(callee, None);
                if Self::last_spreads(args) {
                    let fixed = args[..args.len() - 1]
                        .iter()
                        .map(|arg| self.compile_expr(arg, None))
                        .collect();
                    let source = self.spread_source(&args[args.len() - 1]);
                    self.emit(IrInst::Spread {
                        source,
                        sink: SpreadSink::Call {
                            dsts: vec![dst],
                            callee: callee_var,
                            fixed_args: fixed,
                            retc: 1,
                        },
                    });
                    return dst;
                }
                let mut arg_vars = Vec::new();
                for arg in args {
                    arg_vars.push(self.compile_expr(arg, None));
                }
                self.emit(IrInst::Call {
                    dsts: vec![dst],
                    callee: callee_var,
                    args: arg_vars,
                    retc: 1,
                });
            }
            Expr::MethodCall {
                object,
                method,
                args,
                ..
            } => {
                let tbl = self.compile_expr(object, None);
                let key = self.alloc_var();
                self.emit(IrInst::LoadConst {
                    dst: key,
                    val: IrConstant::String(method.clone()),
                });
                let callee = self.alloc_var();
                self.emit(IrInst::GetTable {
                    dst: callee,
                    table: tbl,
                    key,
                });

                if Self::last_spreads(args) {
                    let mut fixed = vec![tbl];
                    for arg in &args[..args.len() - 1] {
                        fixed.push(self.compile_expr(arg, None));
                    }
                    let source = self.spread_source(&args[args.len() - 1]);
                    self.emit(IrInst::Spread {
                        source,
                        sink: SpreadSink::Call {
                            dsts: vec![dst],
                            callee,
                            fixed_args: fixed,
                            retc: 1,
                        },
                    });
                    return dst;
                }
                let mut arg_vars = vec![tbl];
                for arg in args {
                    arg_vars.push(self.compile_expr(arg, None));
                }
                self.emit(IrInst::Call {
                    dsts: vec![dst],
                    callee,
                    args: arg_vars,
                    retc: 1,
                });
            }
            Expr::Table { entries, .. } => {
                let mut array_cap: usize = 0;
                let mut fields_cap: usize = 0;
                for entry in entries {
                    if entry.is_array_element() {
                        array_cap += 1;
                    } else if entry.key.is_some() {
                        fields_cap += 1;
                    }
                }
                let array_cap = array_cap.min(255) as u8;
                let fields_cap = fields_cap.min(255) as u8;
                self.emit(IrInst::NewTable {
                    dst,
                    array_cap,
                    fields_cap,
                });
                // A trailing call or `...` contributes every value it yields,
                // so it is appended as a run rather than as one element.
                let trailing = entries
                    .last()
                    .filter(|e| e.is_array_element())
                    .filter(|e| Self::is_multi_value(&e.value))
                    .map(|e| e.value.clone());
                let fixed_entries = if trailing.is_some() {
                    &entries[..entries.len() - 1]
                } else {
                    &entries[..]
                };
                let has_computed = entries.iter().any(|e| e.computed.is_some());
                let mut position: u64 = 0;
                for entry in fixed_entries {
                    if let Some(key_expr) = &entry.computed {
                        let key_var = self.compile_expr(key_expr, None);
                        let val_var = self.compile_expr(&entry.value, None);
                        self.emit(IrInst::SetTable {
                            table: dst,
                            key: key_var,
                            val: val_var,
                        });
                    } else if let Some(ref k) = entry.key {
                        let key_var = self.alloc_var();
                        self.emit(IrInst::LoadConst {
                            dst: key_var,
                            val: IrConstant::String(k.clone()),
                        });
                        let val_var = self.compile_expr(&entry.value, None);
                        self.emit(IrInst::SetTable {
                            table: dst,
                            key: key_var,
                            val: val_var,
                        });
                    } else if has_computed {
                        // A computed key may already have claimed this
                        // index, so positional entries say theirs.
                        position += 1;
                        let key_var = self.alloc_var();
                        self.emit(IrInst::LoadConst {
                            dst: key_var,
                            val: IrConstant::Int(BigInt::from(position)),
                        });
                        let elem_var = self.compile_expr(&entry.value, None);
                        self.emit(IrInst::SetTable {
                            table: dst,
                            key: key_var,
                            val: elem_var,
                        });
                    } else {
                        let elem_var = self.compile_expr(&entry.value, None);
                        self.emit(IrInst::AppendArray {
                            table: dst,
                            src: elem_var,
                        });
                    }
                }
                if let Some(value) = trailing {
                    let source = self.spread_source(&value);
                    self.emit(IrInst::Spread {
                        source,
                        sink: SpreadSink::List { table: dst },
                    });
                }
            }
            Expr::Index { object, index, .. } => {
                let tbl = self.compile_expr(object, None);
                let key = self.compile_expr(index, None);
                self.emit(IrInst::GetTable {
                    dst,
                    table: tbl,
                    key,
                });
            }
            Expr::Member { object, field, .. } => {
                // `module.field` on a global resolves through the import
                // cache; anything else keeps the dynamic table read.
                if let Expr::Variable { name, .. } = object.as_ref() {
                    let (is_local, upval, _) = self.resolve_variable(name);
                    if !is_local && upval.is_none() {
                        self.emit(IrInst::GetImport {
                            dst,
                            module: name.clone(),
                            field: field.clone(),
                        });
                        return dst;
                    }
                }
                let tbl = self.compile_expr(object, None);
                let key = self.alloc_var();
                self.emit(IrInst::LoadConst {
                    dst: key,
                    val: IrConstant::String(field.clone()),
                });
                self.emit(IrInst::GetTable {
                    dst,
                    table: tbl,
                    key,
                });
            }
            Expr::Function { params, body, .. } => {
                let proto_idx = self.compile_sub_function(None, params, body);
                let captures = self.captures_of(proto_idx);
                self.emit(IrInst::Closure {
                    dst,
                    proto_idx,
                    captures,
                });
            }
        }
        dst
    }

    pub fn compile_sub_function(
        &mut self,
        name: Option<String>,
        params: &[Param],
        body: &[Stmt],
    ) -> u16 {
        let is_vararg = params.iter().any(|p| p.variadic);
        let num_params = params.iter().filter(|p| !p.variadic).count() as u8;

        let mut sub_builder = IrBuilder::new();
        sub_builder.lines = self.lines.clone();
        sub_builder.keep_locals = self.keep_locals;
        if self.current_line > 0 {
            sub_builder.current_line = self.current_line;
            sub_builder.emit(IrInst::Line(self.current_line));
        }
        sub_builder.parent_scopes = self.parent_scopes.clone();
        sub_builder.parent_scopes.push(self.scopes.clone());
        sub_builder.enter_scope();
        for param in params {
            if !param.variadic {
                let v = sub_builder.alloc_var();
                sub_builder.add_local(param.name.clone(), v);
            }
        }
        for stmt in body {
            sub_builder.compile_stmt(stmt);
        }
        sub_builder.exit_scope();

        // A variable the nested function reaches past this one for has to be
        // captured here as well, so it can read it from this function's
        // upvalues rather than from a register that does not hold it.
        for slot in 0..sub_builder.upvalue_vars.len() {
            if let UpvalSource::Pending { var, levels_up } = sub_builder.upvalue_vars[slot] {
                // One level closer from here: the child's grandparent is this
                // function's parent.
                let name = sub_builder.upvalue_names[slot].clone();
                sub_builder.upvalues[slot].index = self.capture(var, levels_up - 1, &name);
                sub_builder.upvalue_vars[slot] = UpvalSource::ParentUpvalue;
            }
        }

        let has_terminal = matches!(sub_builder.instructions.last(), Some(IrInst::Return(_)));
        if !has_terminal {
            let nil_var = sub_builder.alloc_var();
            sub_builder.emit(IrInst::LoadNil { dst: nil_var });
            sub_builder.emit(IrInst::Return(vec![nil_var]));
        }

        let proto_idx = self.child_protos.len() as u16;
        self.child_protos.push(IrFunction {
            name,
            params: params.to_vec(),
            num_params,
            is_vararg,
            instructions: sub_builder.instructions,
            protos: sub_builder.child_protos,
            upvalues: sub_builder.upvalues,
            upvalue_vars: sub_builder.upvalue_vars,
            upvalue_names: sub_builder.upvalue_names,
            cfg: None,
        });
        proto_idx
    }

    pub fn compile_stmt(&mut self, stmt: &Stmt) {
        self.mark_line(stmt.node_id());
        match stmt {
            Stmt::Local {
                name,
                is_global,
                initializer,
                ..
            } => {
                let var = self.alloc_var();
                if let Some(init) = initializer {
                    self.compile_expr(init, Some(var));
                } else {
                    self.emit(IrInst::LoadNil { dst: var });
                }
                if *is_global {
                    self.emit(IrInst::SetGlobal {
                        name: name.clone(),
                        src: var,
                    });
                } else {
                    self.add_local(name.clone(), var);
                }
            }
            Stmt::LocalMany {
                names,
                is_global,
                initializers,
                ..
            } => {
                // `local a, b = f()` takes one value per name from the call
                // rather than giving only the first name a value.
                if initializers.len() == 1
                    && names.len() > 1
                    && let Some(vars) = self.compile_call_results(&initializers[0], names.len())
                {
                    for (name, var) in names.iter().zip(vars) {
                        if *is_global {
                            self.emit(IrInst::SetGlobal {
                                name: name.clone(),
                                src: var,
                            });
                        } else {
                            self.add_local(name.clone(), var);
                        }
                    }
                    return;
                }
                for (i, name) in names.iter().enumerate() {
                    let var = self.alloc_var();
                    if let Some(init) = initializers.get(i) {
                        self.compile_expr(init, Some(var));
                    } else {
                        self.emit(IrInst::LoadNil { dst: var });
                    }
                    if *is_global {
                        self.emit(IrInst::SetGlobal {
                            name: name.clone(),
                            src: var,
                        });
                    } else {
                        self.add_local(name.clone(), var);
                    }
                }
            }
            Stmt::Assign { target, value, .. } => {
                let val_var = self.compile_expr(value, None);
                self.compile_assign(target, val_var);
            }
            Stmt::AssignMany {
                targets, values, ..
            } => {
                if values.len() == 1
                    && targets.len() > 1
                    && let Some(vars) = self.compile_call_results(&values[0], targets.len())
                {
                    for (target, var) in targets.iter().zip(vars) {
                        self.compile_assign(target, var);
                    }
                    return;
                }
                let mut val_vars = Vec::new();
                for val in values {
                    val_vars.push(self.compile_expr(val, None));
                }
                while val_vars.len() < targets.len() {
                    let nil_var = self.alloc_var();
                    self.emit(IrInst::LoadNil { dst: nil_var });
                    val_vars.push(nil_var);
                }
                for (target, val_var) in targets.iter().zip(val_vars.iter()) {
                    self.compile_assign(target, *val_var);
                }
            }
            Stmt::Increment { target, amount, .. } => {
                let target_expr = target.to_expr();
                let current = self.compile_expr(&target_expr, None);
                let amount_var = self.alloc_var();
                self.emit(IrInst::LoadConst {
                    dst: amount_var,
                    val: IrConstant::Int(BigInt::from(*amount)),
                });
                let result = self.alloc_var();
                self.emit(IrInst::BinOp {
                    dst: result,
                    op: IrBinaryOp::Add,
                    lhs: current,
                    rhs: amount_var,
                });
                self.compile_assign(target, result);
            }
            Stmt::Function {
                name, params, body, ..
            } => {
                let local_var = if let Some(func_name) = name {
                    if !func_name.contains('.') {
                        if let Some(lv) = self.resolve_local(func_name) {
                            Some(lv)
                        } else {
                            let lv = self.alloc_var();
                            self.add_local(func_name.clone(), lv);
                            Some(lv)
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                let proto_idx = self.compile_sub_function(name.clone(), params, body);
                let closure_var = self.alloc_var();
                let captures = self.captures_of(proto_idx);
                self.emit(IrInst::Closure {
                    dst: closure_var,
                    proto_idx,
                    captures,
                });
                if let Some(lv) = local_var {
                    self.emit(IrInst::Move {
                        dst: lv,
                        src: closure_var,
                    });
                } else if let Some(func_name) = name {
                    if let Some((path, field)) = func_name.rsplit_once('.') {
                        // `function a.b.c()` stores the closure in field `c`
                        // of the table `a.b` evaluates to.
                        let mut table = self.compile_expr(
                            &Expr::Variable {
                                id: NodeId::next(),
                                name: path
                                    .split('.')
                                    .next()
                                    .expect("split yields at least one piece")
                                    .to_string(),
                            },
                            None,
                        );
                        for segment in path.split('.').skip(1) {
                            let key = self.alloc_var();
                            self.emit(IrInst::LoadConst {
                                dst: key,
                                val: IrConstant::String(segment.to_string()),
                            });
                            let next = self.alloc_var();
                            self.emit(IrInst::GetTable {
                                dst: next,
                                table,
                                key,
                            });
                            table = next;
                        }
                        let key = self.alloc_var();
                        self.emit(IrInst::LoadConst {
                            dst: key,
                            val: IrConstant::String(field.to_string()),
                        });
                        self.emit(IrInst::SetTable {
                            table,
                            key,
                            val: closure_var,
                        });
                    } else {
                        self.emit(IrInst::SetGlobal {
                            name: func_name.clone(),
                            src: closure_var,
                        });
                    }
                }
            }
            Stmt::If { data, .. } => {
                if data.else_if_branches.is_empty()
                    && data.else_branch.is_none()
                    && matches!(
                        data.condition,
                        Expr::Literal {
                            value: Literal::Bool(true),
                            ..
                        }
                    )
                {
                    self.enter_scope();
                    for s in &data.then_branch {
                        self.compile_stmt(s);
                    }
                    self.exit_scope();
                    return;
                }

                let cond_var = self.compile_expr(&data.condition, None);
                let else_label = self.alloc_label();
                let end_label = self.alloc_label();

                self.emit(IrInst::JumpIfFalse {
                    cond: cond_var,
                    target: else_label,
                });

                self.enter_scope();
                for s in &data.then_branch {
                    self.compile_stmt(s);
                }
                self.exit_scope();
                self.emit(IrInst::Jump(end_label));

                self.emit(IrInst::Label(else_label));
                for (elif_cond, elif_body) in &data.else_if_branches {
                    let next_elif_label = self.alloc_label();
                    let elif_var = self.compile_expr(elif_cond, None);
                    self.emit(IrInst::JumpIfFalse {
                        cond: elif_var,
                        target: next_elif_label,
                    });
                    self.enter_scope();
                    for s in elif_body {
                        self.compile_stmt(s);
                    }
                    self.exit_scope();
                    self.emit(IrInst::Jump(end_label));
                    self.emit(IrInst::Label(next_elif_label));
                }

                if let Some(else_stmts) = &data.else_branch {
                    self.enter_scope();
                    for s in else_stmts {
                        self.compile_stmt(s);
                    }
                    self.exit_scope();
                }

                self.emit(IrInst::Label(end_label));
            }
            Stmt::While {
                condition, body, ..
            } => {
                let start_label = self.alloc_label();
                let exit_label = self.alloc_label();

                self.loops.push(IrLoopContext {
                    break_label: exit_label,
                    continue_label: start_label,
                });

                let first_var = self.next_var;
                self.emit(IrInst::Label(start_label));
                let iteration_start = self.instructions.len();
                let cond_var = self.compile_expr(condition, None);
                self.emit(IrInst::JumpIfFalse {
                    cond: cond_var,
                    target: exit_label,
                });

                self.enter_scope();
                for s in body {
                    self.compile_stmt(s);
                }
                self.exit_scope();

                self.insert_loop_close(iteration_start, first_var);
                self.emit(IrInst::Jump(start_label));
                self.emit(IrInst::Label(exit_label));

                self.loops.pop();
            }
            Stmt::Repeat {
                body, condition, ..
            } => {
                let start_label = self.alloc_label();
                let exit_label = self.alloc_label();
                let cond_label = self.alloc_label();

                self.loops.push(IrLoopContext {
                    break_label: exit_label,
                    continue_label: cond_label,
                });

                let first_var = self.next_var;
                self.emit(IrInst::Label(start_label));
                let iteration_start = self.instructions.len();

                self.enter_scope();
                for s in body {
                    self.compile_stmt(s);
                }

                // The condition can see the body's locals, as in Lua.
                self.emit(IrInst::Label(cond_label));
                let cond_var = self.compile_expr(condition, None);
                self.emit(IrInst::JumpIfFalse {
                    cond: cond_var,
                    target: start_label,
                });
                self.insert_loop_close(iteration_start, first_var);
                self.exit_scope();
                self.emit(IrInst::Label(exit_label));

                self.loops.pop();
            }
            Stmt::NumericFor { data, .. } => {
                self.enter_scope();
                let first_var = self.next_var;
                let base = self.alloc_var();
                self.compile_expr(&data.start, Some(base));
                let limit = self.alloc_var();
                self.compile_expr(&data.end, Some(limit));
                let step_var = self.alloc_var();
                if let Some(s) = &data.step {
                    self.compile_expr(s, Some(step_var));
                } else {
                    self.emit(IrInst::LoadConst {
                        dst: step_var,
                        val: IrConstant::Int(BigInt::from(1)),
                    });
                };

                let loop_var = self.alloc_var();
                self.add_local(data.var.clone(), loop_var);

                let body_label = self.alloc_label();
                let test_label = self.alloc_label();
                let exit_label = self.alloc_label();

                self.loops.push(IrLoopContext {
                    break_label: exit_label,
                    continue_label: test_label,
                });

                self.emit(IrInst::ForPrep {
                    base,
                    limit,
                    step: step_var,
                    loop_var,
                    jump: test_label,
                });

                self.emit(IrInst::Label(body_label));
                let body_start = self.instructions.len();
                self.enter_scope();
                for s in &data.body {
                    self.compile_stmt(s);
                }
                self.exit_scope();

                self.emit(IrInst::Label(test_label));
                // ForLoop writes the next value into the loop variable, so
                // whatever captured this iteration's has to let go first.
                if let Some(close) = self.loop_close(body_start, first_var) {
                    self.emit(close);
                }
                self.emit(IrInst::ForLoop {
                    base,
                    jump: body_label,
                });
                self.emit(IrInst::Label(exit_label));

                self.loops.pop();
                self.exit_scope();
            }
            Stmt::For {
                vars, source, body, ..
            } => {
                self.enter_scope();
                let first_var = self.next_var;
                let base = self.alloc_var();
                let state_var = self.alloc_var();
                let ctrl_var = self.alloc_var();

                match source {
                    Expr::Call { callee, args, .. } => {
                        let callee_var = self.compile_expr(callee, Some(base));
                        let mut arg_vars = Vec::new();
                        for arg in args {
                            arg_vars.push(self.compile_expr(arg, None));
                        }
                        if callee_var != base {
                            self.emit(IrInst::Move {
                                dst: base,
                                src: callee_var,
                            });
                        }
                        self.emit(IrInst::Call {
                            dsts: vec![base, state_var, ctrl_var],
                            callee: base,
                            args: arg_vars,
                            retc: 3,
                        });
                    }
                    _ => {
                        self.compile_expr(source, Some(base));
                        self.emit(IrInst::LoadNil { dst: state_var });
                        self.emit(IrInst::LoadNil { dst: ctrl_var });
                    }
                }

                let mut iter_vars = Vec::new();
                for vname in vars {
                    let iv = self.alloc_var();
                    self.add_local(vname.clone(), iv);
                    iter_vars.push(iv);
                }

                let call_label = self.alloc_label();
                let exit_label = self.alloc_label();

                self.loops.push(IrLoopContext {
                    break_label: exit_label,
                    continue_label: call_label,
                });

                self.emit(IrInst::Label(call_label));
                let iteration_start = self.instructions.len();
                self.emit(IrInst::TForCall {
                    base,
                    state: state_var,
                    ctrl: ctrl_var,
                    vars: iter_vars,
                });
                self.emit(IrInst::TForLoop {
                    base,
                    jump: exit_label,
                });

                self.enter_scope();
                for s in body {
                    self.compile_stmt(s);
                }
                self.exit_scope();

                // TForCall writes the loop variables, so close them before it.
                self.insert_loop_close(iteration_start, first_var);
                self.emit(IrInst::Jump(call_label));
                self.emit(IrInst::Label(exit_label));

                self.loops.pop();
                self.exit_scope();
            }
            Stmt::Break { .. } => {
                if let Some(lp) = self.loops.last() {
                    self.emit(IrInst::Jump(lp.break_label));
                }
            }
            Stmt::Continue { .. } => {
                if let Some(lp) = self.loops.last() {
                    self.emit(IrInst::Jump(lp.continue_label));
                }
            }
            Stmt::Return { values: exprs, .. } => {
                // `return f()` hands back every value f produced.
                if exprs.len() == 1 && Self::is_multi_value(&exprs[0]) {
                    let source = self.spread_source(&exprs[0]);
                    self.emit(IrInst::Spread {
                        source,
                        sink: SpreadSink::Return,
                    });
                    return;
                }
                let mut ret_vars = Vec::new();
                for e in exprs {
                    ret_vars.push(self.compile_expr(e, None));
                }
                self.emit(IrInst::Return(ret_vars));
            }
            Stmt::Expr { expr, .. } => {
                self.compile_expr(expr, None);
            }
            Stmt::Goto { label: lbl, .. } => {
                let target = self.get_or_alloc_label(lbl);
                self.emit(IrInst::Jump(target));
            }
            Stmt::Label { name: lbl, .. } => {
                let target = self.get_or_alloc_label(lbl);
                self.emit(IrInst::Label(target));
            }
            // Type declarations are erased: they produce no code.
            Stmt::TypeAlias { .. } => {}
        }
    }

    fn compile_assign(&mut self, target: &AssignTarget, val_var: IrVar) {
        match target {
            AssignTarget::Variable(name) => {
                let (_, upval, local_var) = self.resolve_variable(name);
                if let Some(dst) = local_var {
                    self.emit(IrInst::Move { dst, src: val_var });
                } else if let Some(upval_idx) = upval {
                    self.emit(IrInst::SetUpval {
                        index: upval_idx,
                        src: val_var,
                    });
                } else {
                    self.emit(IrInst::SetGlobal {
                        name: name.clone(),
                        src: val_var,
                    });
                }
            }
            AssignTarget::Member { object, field } => {
                let tbl = self.compile_expr(object, None);
                let key = self.alloc_var();
                self.emit(IrInst::LoadConst {
                    dst: key,
                    val: IrConstant::String(field.clone()),
                });
                self.emit(IrInst::SetTable {
                    table: tbl,
                    key,
                    val: val_var,
                });
            }
            AssignTarget::Index { object, index } => {
                let tbl = self.compile_expr(object, None);
                let key = self.compile_expr(index, None);
                self.emit(IrInst::SetTable {
                    table: tbl,
                    key,
                    val: val_var,
                });
            }
        }
    }
}

pub fn ast_to_ir(stmts: &[Stmt]) -> IrModule {
    ast_to_ir_with(stmts, None, false)
}

/// Builds the IR with line markers from `lines` (statement id to line) and,
/// with `keep_locals`, locals kept alive to the end of their scope.
pub fn ast_to_ir_with(
    stmts: &[Stmt],
    lines: Option<std::rc::Rc<HashMap<NodeId, u32>>>,
    keep_locals: bool,
) -> IrModule {
    let mut builder = IrBuilder::new();
    builder.lines = lines;
    builder.keep_locals = keep_locals;
    for stmt in stmts {
        builder.compile_stmt(stmt);
    }
    let has_terminal = matches!(builder.instructions.last(), Some(IrInst::Return(_)));
    if !has_terminal {
        // The chunk's own locals end with it.
        let vars = std::mem::take(&mut builder.scope_vars[0]);
        if !vars.is_empty() {
            builder.emit(IrInst::DebugScopeEnd {
                vars,
                keep_alive: builder.keep_locals,
            });
        }
        let nil_var = builder.alloc_var();
        builder.emit(IrInst::LoadNil { dst: nil_var });
        builder.emit(IrInst::Return(vec![nil_var]));
    }

    IrModule {
        main: IrFunction {
            name: Some("main".to_string()),
            params: Vec::new(),
            num_params: 0,
            is_vararg: false,
            instructions: builder.instructions,
            protos: builder.child_protos,
            upvalues: Vec::new(),
            upvalue_vars: Vec::new(),
            upvalue_names: Vec::new(),
            cfg: None,
        },
    }
}
