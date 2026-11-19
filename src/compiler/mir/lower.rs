//! THIR -> MIR lowering pass: constructs typed CFG basic blocks, places,
//! explicit terminators, and closure upvalue captures.

use std::collections::HashMap;

use num_bigint::BigInt;

use crate::ast::Param;
use crate::ast::literal::Literal;
use crate::ast::op::BinOp;
use crate::compiler::mir::cfg::{
    BasicBlockData, BasicBlockId, LocalDecl, LocalId, LocalKind, MirBody, MirConst, MirProgram,
    MirSpreadSource, MirUpvalue, Operand, Place, Projection, Rvalue, Statement, StatementKind,
    TerminatorKind,
};
use crate::compiler::thir::{
    BindingId, ThirExpr, ThirExprKind, ThirLValue, ThirModule, ThirParam, ThirStmt, ThirStmtKind,
};
use crate::sema::types::NeyukiType;

struct LoopContext {
    break_bb: BasicBlockId,
    continue_bb: BasicBlockId,
}

struct MirBuilder<'a> {
    module_thir: &'a ThirModule,
    keep_locals: bool,
    current_line: u32,
    locals: Vec<LocalDecl>,
    blocks: Vec<BasicBlockData>,
    current_bb: usize,
    scopes: Vec<HashMap<String, LocalId>>,
    scope_locals: Vec<Vec<LocalId>>,
    parent_scopes: Vec<Vec<HashMap<String, LocalId>>>,
    upvalues: Vec<MirUpvalue>,
    loops: Vec<LoopContext>,
    named_labels: HashMap<String, BasicBlockId>,
    functions: Vec<MirBody>,
}

impl<'a> MirBuilder<'a> {
    fn new(module_thir: &'a ThirModule, keep_locals: bool) -> Self {
        let entry = BasicBlockData {
            id: BasicBlockId(0),
            statements: Vec::new(),
            terminator: TerminatorKind::Unreachable,
        };
        Self {
            module_thir,
            keep_locals,
            current_line: 0,
            locals: Vec::new(),
            blocks: vec![entry],
            current_bb: 0,
            scopes: vec![HashMap::new()],
            scope_locals: vec![Vec::new()],
            parent_scopes: Vec::new(),
            upvalues: Vec::new(),
            loops: Vec::new(),
            named_labels: HashMap::new(),
            functions: Vec::new(),
        }
    }

    fn alloc_local(
        &mut self,
        kind: LocalKind,
        name: Option<String>,
        ty: NeyukiType,
        binding: Option<BindingId>,
    ) -> LocalId {
        let id = LocalId(self.locals.len() as u32);
        self.locals.push(LocalDecl {
            id,
            name,
            kind,
            ty,
            binding,
        });
        id
    }

    fn alloc_temp(&mut self, ty: NeyukiType) -> LocalId {
        self.alloc_local(LocalKind::Temp, None, ty, None)
    }

    fn alloc_bb(&mut self) -> BasicBlockId {
        let id = BasicBlockId(self.blocks.len() as u32);
        self.blocks.push(BasicBlockData {
            id,
            statements: Vec::new(),
            terminator: TerminatorKind::Unreachable,
        });
        id
    }

    fn switch_to_bb(&mut self, bb: BasicBlockId) {
        self.current_bb = bb.0 as usize;
    }

    fn set_term(&mut self, term: TerminatorKind) {
        self.blocks[self.current_bb].terminator = term;
    }

    fn finish_and_switch(&mut self, term: TerminatorKind, next: BasicBlockId) {
        self.set_term(term);
        self.switch_to_bb(next);
    }

    fn push_stmt(&mut self, kind: StatementKind) {
        self.blocks[self.current_bb].statements.push(Statement {
            line: self.current_line,
            kind,
        });
    }

    fn emit_assign(&mut self, dst: LocalId, rvalue: Rvalue) {
        self.push_stmt(StatementKind::Assign(Place::local(dst), rvalue));
    }

    fn enter_scope(&mut self) {
        self.scopes.push(HashMap::new());
        self.scope_locals.push(Vec::new());
    }

    fn exit_scope(&mut self) {
        self.scopes.pop();
        let locals = self.scope_locals.pop().unwrap_or_default();
        if !locals.is_empty()
            && matches!(
                self.blocks[self.current_bb].terminator,
                TerminatorKind::Unreachable
            )
        {
            self.push_stmt(StatementKind::DebugScopeEnd {
                locals,
                keep_alive: self.keep_locals,
            });
        }
    }

    fn bind_local(&mut self, name: String, local: LocalId) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.clone(), local);
        }
        if let Some(vars) = self.scope_locals.last_mut() {
            vars.push(local);
        }
        self.push_stmt(StatementKind::DebugLocal { name, local });
    }

    fn resolve_local(&self, name: &str) -> Option<LocalId> {
        for scope in self.scopes.iter().rev() {
            if let Some(&l) = scope.get(name) {
                return Some(l);
            }
        }
        None
    }

    fn resolve_upvalue(&mut self, name: &str) -> Option<u8> {
        let num_parents = self.parent_scopes.len();
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

    fn resolve_var(&mut self, name: &str) -> (bool, Option<u8>, Option<LocalId>) {
        if let Some(l) = self.resolve_local(name) {
            return (true, None, Some(l));
        }
        if let Some(up) = self.resolve_upvalue(name) {
            return (false, Some(up), None);
        }
        (false, None, None)
    }

    fn capture(&mut self, var: LocalId, levels_up: usize, name: &str) -> u8 {
        if let Some(pos) = self.upvalues.iter().position(|u| {
            u.levels_up == levels_up
                && ((levels_up == 0 && u.parent_local == Some(var))
                    || (levels_up > 0 && u.name == name && u.parent_local == Some(var)))
        }) {
            return pos as u8;
        }
        let idx = self.upvalues.len() as u8;
        self.upvalues.push(MirUpvalue {
            in_stack: levels_up == 0,
            index: 0,
            levels_up,
            parent_local: Some(var),
            name: name.to_string(),
        });
        idx
    }

    fn get_or_alloc_label(&mut self, name: &str) -> BasicBlockId {
        if let Some(&bb) = self.named_labels.get(name) {
            bb
        } else {
            let bb = self.alloc_bb();
            self.named_labels.insert(name.to_string(), bb);
            bb
        }
    }

    fn binding_type(&self, binding: Option<BindingId>, fallback: &NeyukiType) -> NeyukiType {
        binding
            .and_then(|id| self.module_thir.bindings.get(id.0 as usize))
            .map(|b| b.ty.clone())
            .unwrap_or_else(|| fallback.clone())
    }

    fn is_multi_value(expr: &ThirExpr) -> bool {
        matches!(
            expr.kind,
            ThirExprKind::Call { .. } | ThirExprKind::MethodCall { .. } | ThirExprKind::Vararg
        )
    }

    fn last_spreads(args: &[ThirExpr]) -> bool {
        args.last().is_some_and(Self::is_multi_value)
    }

    fn lower_spread_source(&mut self, expr: &ThirExpr) -> MirSpreadSource {
        match &expr.kind {
            ThirExprKind::Call { callee, args } => {
                let callee_op = Operand::Copy(Place::local(self.lower_expr(callee, None)));
                let spreads = Self::last_spreads(args);
                let fixed = if spreads {
                    &args[..args.len() - 1]
                } else {
                    &args[..]
                };
                let arg_ops = fixed
                    .iter()
                    .map(|a| Operand::Copy(Place::local(self.lower_expr(a, None))))
                    .collect();
                let trailing =
                    spreads.then(|| Box::new(self.lower_spread_source(&args[args.len() - 1])));
                MirSpreadSource::Call {
                    callee: callee_op,
                    args: arg_ops,
                    trailing,
                }
            }
            ThirExprKind::MethodCall {
                object,
                method,
                args,
            } => {
                let tbl = self.lower_expr(object, None);
                let key = self.alloc_temp(NeyukiType::String);
                self.emit_assign(
                    key,
                    Rvalue::Use(Operand::Constant(MirConst::String(method.clone()))),
                );
                let callee = self.alloc_temp(NeyukiType::Any);
                self.emit_assign(
                    callee,
                    Rvalue::Use(Operand::Copy(Place {
                        local: tbl,
                        projections: vec![Projection::Index(Operand::Copy(Place::local(key)))],
                    })),
                );
                let spreads = Self::last_spreads(args);
                let fixed = if spreads {
                    &args[..args.len() - 1]
                } else {
                    &args[..]
                };
                let mut arg_ops = vec![Operand::Copy(Place::local(tbl))];
                for a in fixed {
                    arg_ops.push(Operand::Copy(Place::local(self.lower_expr(a, None))));
                }
                let trailing =
                    spreads.then(|| Box::new(self.lower_spread_source(&args[args.len() - 1])));
                MirSpreadSource::Call {
                    callee: Operand::Copy(Place::local(callee)),
                    args: arg_ops,
                    trailing,
                }
            }
            _ => MirSpreadSource::Vararg,
        }
    }

    fn lower_call_results(&mut self, expr: &ThirExpr, count: usize) -> Option<Vec<LocalId>> {
        let (callee_local, receiver, args) = match &expr.kind {
            ThirExprKind::Call { callee, args } => (self.lower_expr(callee, None), None, args),
            ThirExprKind::MethodCall {
                object,
                method,
                args,
            } => {
                let tbl = self.lower_expr(object, None);
                let key = self.alloc_temp(NeyukiType::String);
                self.emit_assign(
                    key,
                    Rvalue::Use(Operand::Constant(MirConst::String(method.clone()))),
                );
                let callee = self.alloc_temp(NeyukiType::Any);
                self.emit_assign(
                    callee,
                    Rvalue::Use(Operand::Copy(Place {
                        local: tbl,
                        projections: vec![Projection::Index(Operand::Copy(Place::local(key)))],
                    })),
                );
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
        let mut arg_ops: Vec<Operand> = receiver
            .into_iter()
            .map(|r| Operand::Copy(Place::local(r)))
            .collect();
        for a in fixed {
            arg_ops.push(Operand::Copy(Place::local(self.lower_expr(a, None))));
        }
        let dsts: Vec<LocalId> = (0..count)
            .map(|_| self.alloc_temp(NeyukiType::Any))
            .collect();
        let next_bb = self.alloc_bb();
        if spreads {
            let trailing = self.lower_spread_source(&args[args.len() - 1]);
            self.finish_and_switch(
                TerminatorKind::SpreadCall {
                    dsts: dsts.clone(),
                    callee: Operand::Copy(Place::local(callee_local)),
                    fixed_args: arg_ops,
                    trailing,
                    retc: count as u8,
                    target: next_bb,
                },
                next_bb,
            );
        } else {
            self.finish_and_switch(
                TerminatorKind::Call {
                    dsts: dsts.clone(),
                    callee: Operand::Copy(Place::local(callee_local)),
                    args: arg_ops,
                    retc: count as u8,
                    target: next_bb,
                },
                next_bb,
            );
        }
        Some(dsts)
    }

    fn captures_of(&self, fn_idx: usize) -> Vec<Option<LocalId>> {
        self.functions
            .get(fn_idx)
            .map(|child| {
                child
                    .upvalues
                    .iter()
                    .map(|u| {
                        if u.levels_up == 0 {
                            u.parent_local
                        } else {
                            None
                        }
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn collect_loop_closures(
        &self,
        from_bb: usize,
        from_stmt: usize,
        first_local: u32,
    ) -> Vec<LocalId> {
        let mut vars = Vec::new();
        for (bb_idx, bb) in self.blocks.iter().enumerate().skip(from_bb) {
            let start = if bb_idx == from_bb { from_stmt } else { 0 };
            for stmt in bb.statements.iter().skip(start) {
                if let StatementKind::Assign(_, Rvalue::Closure { captures, .. }) = &stmt.kind {
                    for cap in captures.iter().flatten() {
                        if cap.0 >= first_local {
                            vars.push(*cap);
                        }
                    }
                }
            }
        }
        vars.sort_by_key(|v| v.0);
        vars.dedup();
        vars
    }

    fn insert_loop_close(&mut self, at_bb: usize, at_stmt: usize, first_local: u32) {
        let vars = self.collect_loop_closures(at_bb, at_stmt, first_local);
        if !vars.is_empty() {
            self.blocks[at_bb].statements.insert(
                at_stmt,
                Statement {
                    line: self.current_line,
                    kind: StatementKind::CloseUpvals(vars),
                },
            );
        }
    }

    fn lower_literal(lit: &Literal) -> MirConst {
        match lit {
            Literal::Nil => MirConst::Nil,
            Literal::Bool(b) => MirConst::Bool(*b),
            Literal::Int(i) => MirConst::Int(i.clone()),
            Literal::Float(f) => MirConst::Float(*f),
            Literal::String(s) => MirConst::String(s.clone()),
        }
    }

    fn lower_expr(&mut self, expr: &ThirExpr, target: Option<LocalId>) -> LocalId {
        let dst = target.unwrap_or_else(|| self.alloc_temp(expr.ty.clone()));
        match &expr.kind {
            ThirExprKind::Literal(lit) => {
                self.emit_assign(
                    dst,
                    Rvalue::Use(Operand::Constant(Self::lower_literal(lit))),
                );
            }
            ThirExprKind::Vararg => {
                self.emit_assign(dst, Rvalue::Vararg);
            }
            ThirExprKind::Variable { name, .. } => {
                let (_, upval, local) = self.resolve_var(name);
                if let Some(src) = local {
                    self.emit_assign(dst, Rvalue::Use(Operand::Copy(Place::local(src))));
                } else if let Some(idx) = upval {
                    self.emit_assign(dst, Rvalue::Upvalue(idx));
                } else {
                    self.emit_assign(dst, Rvalue::Global(name.clone()));
                }
            }
            ThirExprKind::Binary {
                left,
                op: BinOp::And,
                right,
            } => {
                let lv = self.lower_expr(left, Some(dst));
                if lv != dst {
                    self.emit_assign(dst, Rvalue::Use(Operand::Copy(Place::local(lv))));
                }
                let rhs_bb = self.alloc_bb();
                let join_bb = self.alloc_bb();
                self.finish_and_switch(
                    TerminatorKind::SwitchBool {
                        cond: Operand::Copy(Place::local(dst)),
                        true_bb: rhs_bb,
                        false_bb: join_bb,
                    },
                    rhs_bb,
                );
                let rv = self.lower_expr(right, Some(dst));
                if rv != dst {
                    self.emit_assign(dst, Rvalue::Use(Operand::Copy(Place::local(rv))));
                }
                self.finish_and_switch(TerminatorKind::Goto(join_bb), join_bb);
            }
            ThirExprKind::Binary {
                left,
                op: BinOp::Or,
                right,
            } => {
                let lv = self.lower_expr(left, Some(dst));
                if lv != dst {
                    self.emit_assign(dst, Rvalue::Use(Operand::Copy(Place::local(lv))));
                }
                let rhs_bb = self.alloc_bb();
                let join_bb = self.alloc_bb();
                self.finish_and_switch(
                    TerminatorKind::SwitchBool {
                        cond: Operand::Copy(Place::local(dst)),
                        true_bb: join_bb,
                        false_bb: rhs_bb,
                    },
                    rhs_bb,
                );
                let rv = self.lower_expr(right, Some(dst));
                if rv != dst {
                    self.emit_assign(dst, Rvalue::Use(Operand::Copy(Place::local(rv))));
                }
                self.finish_and_switch(TerminatorKind::Goto(join_bb), join_bb);
            }
            ThirExprKind::Binary {
                left,
                op: BinOp::Coalesce,
                right,
            } => {
                let lv = self.lower_expr(left, Some(dst));
                if lv != dst {
                    self.emit_assign(dst, Rvalue::Use(Operand::Copy(Place::local(lv))));
                }
                let is_nil = self.alloc_temp(NeyukiType::Boolean);
                self.emit_assign(
                    is_nil,
                    Rvalue::BinaryOp {
                        op: BinOp::Eq,
                        lhs: Operand::Copy(Place::local(dst)),
                        rhs: Operand::Constant(MirConst::Nil),
                    },
                );
                let rhs_bb = self.alloc_bb();
                let join_bb = self.alloc_bb();
                self.finish_and_switch(
                    TerminatorKind::SwitchBool {
                        cond: Operand::Copy(Place::local(is_nil)),
                        true_bb: rhs_bb,
                        false_bb: join_bb,
                    },
                    rhs_bb,
                );
                let rv = self.lower_expr(right, Some(dst));
                if rv != dst {
                    self.emit_assign(dst, Rvalue::Use(Operand::Copy(Place::local(rv))));
                }
                self.finish_and_switch(TerminatorKind::Goto(join_bb), join_bb);
            }
            ThirExprKind::Binary { left, op, right } => {
                let l = self.lower_expr(left, None);
                let r = self.lower_expr(right, None);
                self.emit_assign(
                    dst,
                    Rvalue::BinaryOp {
                        op: *op,
                        lhs: Operand::Copy(Place::local(l)),
                        rhs: Operand::Copy(Place::local(r)),
                    },
                );
            }
            ThirExprKind::Unary { op, expr: inner } => {
                let s = self.lower_expr(inner, None);
                self.emit_assign(
                    dst,
                    Rvalue::UnaryOp {
                        op: *op,
                        src: Operand::Copy(Place::local(s)),
                    },
                );
            }
            ThirExprKind::Call { callee, args } => {
                let callee_local = self.lower_expr(callee, None);
                let next_bb = self.alloc_bb();
                if Self::last_spreads(args) {
                    let fixed_ops = args[..args.len() - 1]
                        .iter()
                        .map(|a| Operand::Copy(Place::local(self.lower_expr(a, None))))
                        .collect();
                    let trailing = self.lower_spread_source(&args[args.len() - 1]);
                    self.finish_and_switch(
                        TerminatorKind::SpreadCall {
                            dsts: vec![dst],
                            callee: Operand::Copy(Place::local(callee_local)),
                            fixed_args: fixed_ops,
                            trailing,
                            retc: 1,
                            target: next_bb,
                        },
                        next_bb,
                    );
                } else {
                    let arg_ops = args
                        .iter()
                        .map(|a| Operand::Copy(Place::local(self.lower_expr(a, None))))
                        .collect();
                    self.finish_and_switch(
                        TerminatorKind::Call {
                            dsts: vec![dst],
                            callee: Operand::Copy(Place::local(callee_local)),
                            args: arg_ops,
                            retc: 1,
                            target: next_bb,
                        },
                        next_bb,
                    );
                }
            }
            ThirExprKind::MethodCall {
                object,
                method,
                args,
            } => {
                let tbl = self.lower_expr(object, None);
                let key = self.alloc_temp(NeyukiType::String);
                self.emit_assign(
                    key,
                    Rvalue::Use(Operand::Constant(MirConst::String(method.clone()))),
                );
                let callee_local = self.alloc_temp(NeyukiType::Any);
                self.emit_assign(
                    callee_local,
                    Rvalue::Use(Operand::Copy(Place {
                        local: tbl,
                        projections: vec![Projection::Index(Operand::Copy(Place::local(key)))],
                    })),
                );
                let next_bb = self.alloc_bb();
                if Self::last_spreads(args) {
                    let mut fixed_ops = vec![Operand::Copy(Place::local(tbl))];
                    for a in &args[..args.len() - 1] {
                        fixed_ops.push(Operand::Copy(Place::local(self.lower_expr(a, None))));
                    }
                    let trailing = self.lower_spread_source(&args[args.len() - 1]);
                    self.finish_and_switch(
                        TerminatorKind::SpreadCall {
                            dsts: vec![dst],
                            callee: Operand::Copy(Place::local(callee_local)),
                            fixed_args: fixed_ops,
                            trailing,
                            retc: 1,
                            target: next_bb,
                        },
                        next_bb,
                    );
                } else {
                    let mut arg_ops = vec![Operand::Copy(Place::local(tbl))];
                    for a in args {
                        arg_ops.push(Operand::Copy(Place::local(self.lower_expr(a, None))));
                    }
                    self.finish_and_switch(
                        TerminatorKind::Call {
                            dsts: vec![dst],
                            callee: Operand::Copy(Place::local(callee_local)),
                            args: arg_ops,
                            retc: 1,
                            target: next_bb,
                        },
                        next_bb,
                    );
                }
            }
            ThirExprKind::Table(entries) => {
                let mut array_cap: usize = 0;
                let mut fields_cap: usize = 0;
                for entry in entries {
                    if entry.is_array_element() {
                        array_cap += 1;
                    } else if entry.key.is_some() {
                        fields_cap += 1;
                    }
                }
                self.emit_assign(
                    dst,
                    Rvalue::NewTable {
                        array_cap: array_cap.min(255) as u8,
                        fields_cap: fields_cap.min(255) as u8,
                    },
                );
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
                        let key_local = self.lower_expr(key_expr, None);
                        let val_local = self.lower_expr(&entry.value, None);
                        self.push_stmt(StatementKind::Assign(
                            Place {
                                local: dst,
                                projections: vec![Projection::Index(Operand::Copy(Place::local(
                                    key_local,
                                )))],
                            },
                            Rvalue::Use(Operand::Copy(Place::local(val_local))),
                        ));
                    } else if let Some(k) = &entry.key {
                        let key_local = self.alloc_temp(NeyukiType::String);
                        self.emit_assign(
                            key_local,
                            Rvalue::Use(Operand::Constant(MirConst::String(k.clone()))),
                        );
                        let val_local = self.lower_expr(&entry.value, None);
                        self.push_stmt(StatementKind::Assign(
                            Place {
                                local: dst,
                                projections: vec![Projection::Index(Operand::Copy(Place::local(
                                    key_local,
                                )))],
                            },
                            Rvalue::Use(Operand::Copy(Place::local(val_local))),
                        ));
                    } else if has_computed {
                        position += 1;
                        let key_local = self.alloc_temp(NeyukiType::Int);
                        self.emit_assign(
                            key_local,
                            Rvalue::Use(Operand::Constant(MirConst::Int(BigInt::from(position)))),
                        );
                        let val_local = self.lower_expr(&entry.value, None);
                        self.push_stmt(StatementKind::Assign(
                            Place {
                                local: dst,
                                projections: vec![Projection::Index(Operand::Copy(Place::local(
                                    key_local,
                                )))],
                            },
                            Rvalue::Use(Operand::Copy(Place::local(val_local))),
                        ));
                    } else {
                        let elem = self.lower_expr(&entry.value, None);
                        self.push_stmt(StatementKind::AppendArray {
                            table: dst,
                            value: Operand::Copy(Place::local(elem)),
                        });
                    }
                }
                if let Some(val) = trailing {
                    let source = self.lower_spread_source(&val);
                    let next_bb = self.alloc_bb();
                    self.finish_and_switch(
                        TerminatorKind::SpreadList {
                            table: dst,
                            source,
                            target: next_bb,
                        },
                        next_bb,
                    );
                }
            }
            ThirExprKind::Index { object, index } => {
                let tbl = self.lower_expr(object, None);
                let idx = self.lower_expr(index, None);
                self.emit_assign(
                    dst,
                    Rvalue::Use(Operand::Copy(Place {
                        local: tbl,
                        projections: vec![Projection::Index(Operand::Copy(Place::local(idx)))],
                    })),
                );
            }
            ThirExprKind::Member {
                object,
                field,
                is_module_import,
            } => {
                if *is_module_import && let ThirExprKind::Variable { name, .. } = &object.kind {
                    let (is_local, upval, _) = self.resolve_var(name);
                    if !is_local && upval.is_none() {
                        self.emit_assign(
                            dst,
                            Rvalue::ModuleImport {
                                module: name.clone(),
                                field: field.clone(),
                            },
                        );
                        return dst;
                    }
                }
                let tbl = self.lower_expr(object, None);
                let key = self.alloc_temp(NeyukiType::String);
                self.emit_assign(
                    key,
                    Rvalue::Use(Operand::Constant(MirConst::String(field.clone()))),
                );
                self.emit_assign(
                    dst,
                    Rvalue::Use(Operand::Copy(Place {
                        local: tbl,
                        projections: vec![Projection::Index(Operand::Copy(Place::local(key)))],
                    })),
                );
            }
            ThirExprKind::Function {
                params,
                return_type,
                body,
                ..
            } => {
                let fn_idx = self.lower_sub_function(None, params, return_type.clone(), body);
                let captures = self.captures_of(fn_idx);
                self.emit_assign(dst, Rvalue::Closure { fn_idx, captures });
            }
        }
        dst
    }

    fn lower_sub_function(
        &mut self,
        name: Option<String>,
        params: &[ThirParam],
        return_type: NeyukiType,
        body: &[ThirStmt],
    ) -> usize {
        let is_vararg = params.iter().any(|p| p.variadic);
        let num_params = params.iter().filter(|p| !p.variadic).count() as u8;
        let ast_params: Vec<Param> = params
            .iter()
            .map(|p| Param::new(p.name.clone(), None, p.variadic))
            .collect();

        let mut sub = MirBuilder::new(self.module_thir, self.keep_locals);
        sub.current_line = self.current_line;
        sub.parent_scopes = self.parent_scopes.clone();
        sub.parent_scopes.push(self.scopes.clone());
        sub.enter_scope();
        for p in params {
            if !p.variadic {
                let ty = self.binding_type(p.binding, &p.ty);
                let lid = sub.alloc_local(LocalKind::Arg, Some(p.name.clone()), ty, p.binding);
                sub.bind_local(p.name.clone(), lid);
            }
        }
        for s in body {
            sub.lower_stmt(s);
        }
        sub.exit_scope();

        // Propagate transitive captures through enclosing functions.
        for slot in 0..sub.upvalues.len() {
            if sub.upvalues[slot].levels_up > 0
                && let Some(parent_var) = sub.upvalues[slot].parent_local
            {
                let up_name = sub.upvalues[slot].name.clone();
                let levels = sub.upvalues[slot].levels_up - 1;
                sub.upvalues[slot].index = self.capture(parent_var, levels, &up_name);
                sub.upvalues[slot].in_stack = false;
                sub.upvalues[slot].parent_local = None;
            }
        }

        if matches!(
            sub.blocks[sub.current_bb].terminator,
            TerminatorKind::Unreachable
        ) {
            sub.set_term(TerminatorKind::Return(vec![Operand::Constant(
                MirConst::Nil,
            )]));
        }

        let fn_idx = self.functions.len();
        self.functions.push(MirBody {
            name,
            params: ast_params,
            num_params,
            is_vararg,
            return_type,
            locals: sub.locals,
            upvalues: sub.upvalues,
            blocks: sub.blocks,
            functions: sub.functions,
        });
        fn_idx
    }

    fn lower_assign(&mut self, target: &ThirLValue, val: LocalId) {
        match target {
            ThirLValue::Variable { name, .. } => {
                let (_, upval, local) = self.resolve_var(name);
                if let Some(dst) = local {
                    self.emit_assign(dst, Rvalue::Use(Operand::Copy(Place::local(val))));
                } else if let Some(idx) = upval {
                    self.push_stmt(StatementKind::SetUpval {
                        index: idx,
                        value: Operand::Copy(Place::local(val)),
                    });
                } else {
                    self.push_stmt(StatementKind::SetGlobal {
                        name: name.clone(),
                        value: Operand::Copy(Place::local(val)),
                    });
                }
            }
            ThirLValue::Member { object, field } => {
                let tbl = self.lower_expr(object, None);
                let key = self.alloc_temp(NeyukiType::String);
                self.emit_assign(
                    key,
                    Rvalue::Use(Operand::Constant(MirConst::String(field.clone()))),
                );
                self.push_stmt(StatementKind::Assign(
                    Place {
                        local: tbl,
                        projections: vec![Projection::Index(Operand::Copy(Place::local(key)))],
                    },
                    Rvalue::Use(Operand::Copy(Place::local(val))),
                ));
            }
            ThirLValue::Index { object, index } => {
                let tbl = self.lower_expr(object, None);
                let idx = self.lower_expr(index, None);
                self.push_stmt(StatementKind::Assign(
                    Place {
                        local: tbl,
                        projections: vec![Projection::Index(Operand::Copy(Place::local(idx)))],
                    },
                    Rvalue::Use(Operand::Copy(Place::local(val))),
                ));
            }
        }
    }

    fn lower_stmt(&mut self, stmt: &ThirStmt) {
        if stmt.line > 0 {
            self.current_line = stmt.line;
        }
        match &stmt.kind {
            ThirStmtKind::Local {
                binding,
                name,
                is_global,
                ty,
                initializer,
            } => {
                let effective_ty = self.binding_type(Some(*binding), ty);
                let local = self.alloc_local(
                    LocalKind::Var,
                    Some(name.clone()),
                    effective_ty,
                    Some(*binding),
                );
                if let Some(init) = initializer {
                    self.lower_expr(init, Some(local));
                } else {
                    self.emit_assign(local, Rvalue::Use(Operand::Constant(MirConst::Nil)));
                }
                if *is_global {
                    self.push_stmt(StatementKind::SetGlobal {
                        name: name.clone(),
                        value: Operand::Copy(Place::local(local)),
                    });
                } else {
                    self.bind_local(name.clone(), local);
                }
            }
            ThirStmtKind::LocalMany {
                bindings,
                names,
                is_global,
                initializers,
            } => {
                if initializers.len() == 1
                    && names.len() > 1
                    && let Some(vars) = self.lower_call_results(&initializers[0], names.len())
                {
                    for ((name, &binding), var) in names.iter().zip(bindings.iter()).zip(vars) {
                        let ty = self.binding_type(Some(binding), &NeyukiType::Any);
                        if let Some(decl) = self.locals.get_mut(var.0 as usize) {
                            decl.name = Some(name.clone());
                            decl.kind = LocalKind::Var;
                            decl.ty = ty;
                            decl.binding = Some(binding);
                        }
                        if *is_global {
                            self.push_stmt(StatementKind::SetGlobal {
                                name: name.clone(),
                                value: Operand::Copy(Place::local(var)),
                            });
                        } else {
                            self.bind_local(name.clone(), var);
                        }
                    }
                    return;
                }
                for (i, name) in names.iter().enumerate() {
                    let binding = bindings.get(i).copied();
                    let ty = self.binding_type(binding, &NeyukiType::Any);
                    let local = self.alloc_local(LocalKind::Var, Some(name.clone()), ty, binding);
                    if let Some(init) = initializers.get(i) {
                        self.lower_expr(init, Some(local));
                    } else {
                        self.emit_assign(local, Rvalue::Use(Operand::Constant(MirConst::Nil)));
                    }
                    if *is_global {
                        self.push_stmt(StatementKind::SetGlobal {
                            name: name.clone(),
                            value: Operand::Copy(Place::local(local)),
                        });
                    } else {
                        self.bind_local(name.clone(), local);
                    }
                }
            }
            ThirStmtKind::Assign { target, value } => {
                let v = self.lower_expr(value, None);
                self.lower_assign(target, v);
            }
            ThirStmtKind::AssignMany { targets, values } => {
                if values.len() == 1
                    && targets.len() > 1
                    && let Some(vars) = self.lower_call_results(&values[0], targets.len())
                {
                    for (t, v) in targets.iter().zip(vars) {
                        self.lower_assign(t, v);
                    }
                    return;
                }
                let mut val_locals: Vec<LocalId> =
                    values.iter().map(|v| self.lower_expr(v, None)).collect();
                while val_locals.len() < targets.len() {
                    let nil_local = self.alloc_temp(NeyukiType::Nil);
                    self.emit_assign(nil_local, Rvalue::Use(Operand::Constant(MirConst::Nil)));
                    val_locals.push(nil_local);
                }
                for (t, v) in targets.iter().zip(val_locals) {
                    self.lower_assign(t, v);
                }
            }
            ThirStmtKind::Increment { target, amount } => {
                let delta = Operand::Constant(MirConst::Int(BigInt::from(*amount)));
                match target {
                    ThirLValue::Variable { name, .. } => {
                        let (_, upval, local) = self.resolve_var(name);
                        let cur = self.alloc_temp(NeyukiType::Number);
                        if let Some(src) = local {
                            self.emit_assign(cur, Rvalue::Use(Operand::Copy(Place::local(src))));
                        } else if let Some(idx) = upval {
                            self.emit_assign(cur, Rvalue::Upvalue(idx));
                        } else {
                            self.emit_assign(cur, Rvalue::Global(name.clone()));
                        }
                        let res = self.alloc_temp(NeyukiType::Number);
                        self.emit_assign(
                            res,
                            Rvalue::BinaryOp {
                                op: BinOp::Add,
                                lhs: Operand::Copy(Place::local(cur)),
                                rhs: delta,
                            },
                        );
                        self.lower_assign(target, res);
                    }
                    ThirLValue::Member { object, field } => {
                        let tbl = self.lower_expr(object, None);
                        let key = self.alloc_temp(NeyukiType::String);
                        self.emit_assign(
                            key,
                            Rvalue::Use(Operand::Constant(MirConst::String(field.clone()))),
                        );
                        let cur = self.alloc_temp(NeyukiType::Number);
                        self.emit_assign(
                            cur,
                            Rvalue::Use(Operand::Copy(Place {
                                local: tbl,
                                projections: vec![Projection::Index(Operand::Copy(Place::local(
                                    key,
                                )))],
                            })),
                        );
                        let res = self.alloc_temp(NeyukiType::Number);
                        self.emit_assign(
                            res,
                            Rvalue::BinaryOp {
                                op: BinOp::Add,
                                lhs: Operand::Copy(Place::local(cur)),
                                rhs: delta,
                            },
                        );
                        self.push_stmt(StatementKind::Assign(
                            Place {
                                local: tbl,
                                projections: vec![Projection::Index(Operand::Copy(Place::local(
                                    key,
                                )))],
                            },
                            Rvalue::Use(Operand::Copy(Place::local(res))),
                        ));
                    }
                    ThirLValue::Index { object, index } => {
                        let tbl = self.lower_expr(object, None);
                        let key = self.lower_expr(index, None);
                        let cur = self.alloc_temp(NeyukiType::Number);
                        self.emit_assign(
                            cur,
                            Rvalue::Use(Operand::Copy(Place {
                                local: tbl,
                                projections: vec![Projection::Index(Operand::Copy(Place::local(
                                    key,
                                )))],
                            })),
                        );
                        let res = self.alloc_temp(NeyukiType::Number);
                        self.emit_assign(
                            res,
                            Rvalue::BinaryOp {
                                op: BinOp::Add,
                                lhs: Operand::Copy(Place::local(cur)),
                                rhs: delta,
                            },
                        );
                        self.push_stmt(StatementKind::Assign(
                            Place {
                                local: tbl,
                                projections: vec![Projection::Index(Operand::Copy(Place::local(
                                    key,
                                )))],
                            },
                            Rvalue::Use(Operand::Copy(Place::local(res))),
                        ));
                    }
                }
            }
            ThirStmtKind::Function {
                binding,
                name,
                params,
                return_type,
                body,
                ..
            } => {
                let local_var = name.as_ref().map(|func_name| {
                    if let Some(lv) = self.resolve_local(func_name) {
                        lv
                    } else {
                        let ty = self.binding_type(*binding, return_type);
                        let lv =
                            self.alloc_local(LocalKind::Var, Some(func_name.clone()), ty, *binding);
                        self.bind_local(func_name.clone(), lv);
                        lv
                    }
                });
                let fn_idx =
                    self.lower_sub_function(name.clone(), params, return_type.clone(), body);
                let captures = self.captures_of(fn_idx);
                let closure_local = self.alloc_temp(NeyukiType::Any);
                self.emit_assign(closure_local, Rvalue::Closure { fn_idx, captures });
                if let Some(lv) = local_var {
                    self.emit_assign(lv, Rvalue::Use(Operand::Copy(Place::local(closure_local))));
                } else if let Some(func_name) = name {
                    self.push_stmt(StatementKind::SetGlobal {
                        name: func_name.clone(),
                        value: Operand::Copy(Place::local(closure_local)),
                    });
                }
            }
            ThirStmtKind::Block(body) => {
                self.enter_scope();
                for s in body {
                    self.lower_stmt(s);
                }
                self.exit_scope();
            }
            ThirStmtKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond_local = self.lower_expr(condition, None);
                let then_bb = self.alloc_bb();
                let else_bb = self.alloc_bb();
                let join_bb = self.alloc_bb();

                self.finish_and_switch(
                    TerminatorKind::SwitchBool {
                        cond: Operand::Copy(Place::local(cond_local)),
                        true_bb: then_bb,
                        false_bb: else_bb,
                    },
                    then_bb,
                );

                self.enter_scope();
                for s in then_branch {
                    self.lower_stmt(s);
                }
                self.exit_scope();
                if matches!(
                    self.blocks[self.current_bb].terminator,
                    TerminatorKind::Unreachable
                ) {
                    self.set_term(TerminatorKind::Goto(join_bb));
                }

                self.switch_to_bb(else_bb);
                if let Some(else_stmts) = else_branch {
                    self.enter_scope();
                    for s in else_stmts {
                        self.lower_stmt(s);
                    }
                    self.exit_scope();
                }
                if matches!(
                    self.blocks[self.current_bb].terminator,
                    TerminatorKind::Unreachable
                ) {
                    self.set_term(TerminatorKind::Goto(join_bb));
                }
                self.switch_to_bb(join_bb);
            }
            ThirStmtKind::While { condition, body } => {
                let head_bb = self.alloc_bb();
                let body_bb = self.alloc_bb();
                let exit_bb = self.alloc_bb();

                self.finish_and_switch(TerminatorKind::Goto(head_bb), head_bb);
                let first_local = self.locals.len() as u32;
                let head_stmt_idx = self.blocks[self.current_bb].statements.len();
                let head_bb_idx = self.current_bb;

                self.loops.push(LoopContext {
                    break_bb: exit_bb,
                    continue_bb: head_bb,
                });

                let cond_local = self.lower_expr(condition, None);
                self.finish_and_switch(
                    TerminatorKind::SwitchBool {
                        cond: Operand::Copy(Place::local(cond_local)),
                        true_bb: body_bb,
                        false_bb: exit_bb,
                    },
                    body_bb,
                );

                self.enter_scope();
                for s in body {
                    self.lower_stmt(s);
                }
                self.exit_scope();
                if matches!(
                    self.blocks[self.current_bb].terminator,
                    TerminatorKind::Unreachable
                ) {
                    self.set_term(TerminatorKind::Goto(head_bb));
                }
                self.insert_loop_close(head_bb_idx, head_stmt_idx, first_local);
                self.loops.pop();
                self.switch_to_bb(exit_bb);
            }
            ThirStmtKind::Repeat { body, condition } => {
                let start_bb = self.alloc_bb();
                let cond_bb = self.alloc_bb();
                let exit_bb = self.alloc_bb();

                self.finish_and_switch(TerminatorKind::Goto(start_bb), start_bb);
                let first_local = self.locals.len() as u32;
                let start_stmt_idx = self.blocks[self.current_bb].statements.len();
                let start_bb_idx = self.current_bb;

                self.loops.push(LoopContext {
                    break_bb: exit_bb,
                    continue_bb: cond_bb,
                });

                self.enter_scope();
                for s in body {
                    self.lower_stmt(s);
                }
                if matches!(
                    self.blocks[self.current_bb].terminator,
                    TerminatorKind::Unreachable
                ) {
                    self.set_term(TerminatorKind::Goto(cond_bb));
                }
                self.switch_to_bb(cond_bb);
                let cond_local = self.lower_expr(condition, None);
                self.insert_loop_close(start_bb_idx, start_stmt_idx, first_local);
                self.exit_scope();
                self.finish_and_switch(
                    TerminatorKind::SwitchBool {
                        cond: Operand::Copy(Place::local(cond_local)),
                        true_bb: exit_bb,
                        false_bb: start_bb,
                    },
                    exit_bb,
                );
                self.loops.pop();
            }
            ThirStmtKind::NumericFor(data) => {
                self.enter_scope();
                let first_local = self.locals.len() as u32;
                let base = self.alloc_temp(data.start.ty.clone());
                self.lower_expr(&data.start, Some(base));
                let limit = self.alloc_temp(data.end.ty.clone());
                self.lower_expr(&data.end, Some(limit));
                let step = self.alloc_temp(data.step.ty.clone());
                self.lower_expr(&data.step, Some(step));

                let loop_ty = self.binding_type(Some(data.binding), &NeyukiType::Number);
                let loop_var = self.alloc_local(
                    LocalKind::Var,
                    Some(data.var.clone()),
                    loop_ty,
                    Some(data.binding),
                );
                self.bind_local(data.var.clone(), loop_var);

                let body_bb = self.alloc_bb();
                let test_bb = self.alloc_bb();
                let exit_bb = self.alloc_bb();

                self.loops.push(LoopContext {
                    break_bb: exit_bb,
                    continue_bb: test_bb,
                });

                self.finish_and_switch(
                    TerminatorKind::ForPrep {
                        base,
                        limit,
                        step,
                        loop_var,
                        test_bb,
                        body_bb,
                    },
                    body_bb,
                );

                let body_bb_idx = self.current_bb;
                let body_stmt_idx = self.blocks[self.current_bb].statements.len();
                self.enter_scope();
                for s in &data.body {
                    self.lower_stmt(s);
                }
                self.exit_scope();
                if matches!(
                    self.blocks[self.current_bb].terminator,
                    TerminatorKind::Unreachable
                ) {
                    self.set_term(TerminatorKind::Goto(test_bb));
                }

                self.switch_to_bb(test_bb);
                let vars = self.collect_loop_closures(body_bb_idx, body_stmt_idx, first_local);
                if !vars.is_empty() {
                    self.push_stmt(StatementKind::CloseUpvals(vars));
                }
                self.finish_and_switch(
                    TerminatorKind::ForLoop {
                        base,
                        body_bb,
                        exit_bb,
                    },
                    exit_bb,
                );
                self.loops.pop();
                self.exit_scope();
            }
            ThirStmtKind::For {
                bindings,
                vars,
                source,
                body,
            } => {
                self.enter_scope();
                let first_local = self.locals.len() as u32;
                let base = self.alloc_temp(NeyukiType::Any);
                let state = self.alloc_temp(NeyukiType::Any);
                let ctrl = self.alloc_temp(NeyukiType::Any);

                match &source.kind {
                    ThirExprKind::Call { callee, args } => {
                        let callee_local = self.lower_expr(callee, Some(base));
                        let arg_ops: Vec<Operand> = args
                            .iter()
                            .map(|a| Operand::Copy(Place::local(self.lower_expr(a, None))))
                            .collect();
                        if callee_local != base {
                            self.emit_assign(
                                base,
                                Rvalue::Use(Operand::Copy(Place::local(callee_local))),
                            );
                        }
                        let next_bb = self.alloc_bb();
                        self.finish_and_switch(
                            TerminatorKind::Call {
                                dsts: vec![base, state, ctrl],
                                callee: Operand::Copy(Place::local(base)),
                                args: arg_ops,
                                retc: 3,
                                target: next_bb,
                            },
                            next_bb,
                        );
                    }
                    _ => {
                        self.lower_expr(source, Some(base));
                        self.emit_assign(state, Rvalue::Use(Operand::Constant(MirConst::Nil)));
                        self.emit_assign(ctrl, Rvalue::Use(Operand::Constant(MirConst::Nil)));
                    }
                }

                let mut iter_vars = Vec::with_capacity(vars.len());
                for (vname, &binding) in vars.iter().zip(bindings.iter()) {
                    let ty = self.binding_type(Some(binding), &NeyukiType::Any);
                    let iv =
                        self.alloc_local(LocalKind::Var, Some(vname.clone()), ty, Some(binding));
                    self.bind_local(vname.clone(), iv);
                    iter_vars.push(iv);
                }

                let call_bb = self.alloc_bb();
                let body_bb = self.alloc_bb();
                let exit_bb = self.alloc_bb();

                self.loops.push(LoopContext {
                    break_bb: exit_bb,
                    continue_bb: call_bb,
                });

                self.finish_and_switch(TerminatorKind::Goto(call_bb), call_bb);
                let call_bb_idx = self.current_bb;
                let call_stmt_idx = self.blocks[self.current_bb].statements.len();
                self.finish_and_switch(
                    TerminatorKind::TForStep {
                        base,
                        state,
                        ctrl,
                        vars: iter_vars,
                        body_bb,
                        exit_bb,
                    },
                    body_bb,
                );

                self.enter_scope();
                for s in body {
                    self.lower_stmt(s);
                }
                self.exit_scope();
                if matches!(
                    self.blocks[self.current_bb].terminator,
                    TerminatorKind::Unreachable
                ) {
                    self.set_term(TerminatorKind::Goto(call_bb));
                }

                self.insert_loop_close(call_bb_idx, call_stmt_idx, first_local);
                self.loops.pop();
                self.switch_to_bb(exit_bb);
                self.exit_scope();
            }
            ThirStmtKind::Break => {
                if let Some(target) = self.loops.last().map(|lp| lp.break_bb) {
                    let dead = self.alloc_bb();
                    self.finish_and_switch(TerminatorKind::Goto(target), dead);
                }
            }
            ThirStmtKind::Continue => {
                if let Some(target) = self.loops.last().map(|lp| lp.continue_bb) {
                    let dead = self.alloc_bb();
                    self.finish_and_switch(TerminatorKind::Goto(target), dead);
                }
            }
            ThirStmtKind::Return(exprs) => {
                if exprs.len() == 1 && Self::is_multi_value(&exprs[0]) {
                    let src = self.lower_spread_source(&exprs[0]);
                    let dead = self.alloc_bb();
                    self.finish_and_switch(TerminatorKind::SpreadReturn(src), dead);
                    return;
                }
                let ret_ops: Vec<Operand> = exprs
                    .iter()
                    .map(|e| Operand::Copy(Place::local(self.lower_expr(e, None))))
                    .collect();
                let dead = self.alloc_bb();
                self.finish_and_switch(TerminatorKind::Return(ret_ops), dead);
            }
            ThirStmtKind::Expr(e) => {
                self.lower_expr(e, None);
            }
            ThirStmtKind::Goto(lbl) => {
                let target = self.get_or_alloc_label(lbl);
                let dead = self.alloc_bb();
                self.finish_and_switch(TerminatorKind::Goto(target), dead);
            }
            ThirStmtKind::Label(lbl) => {
                let target = self.get_or_alloc_label(lbl);
                if matches!(
                    self.blocks[self.current_bb].terminator,
                    TerminatorKind::Unreachable
                ) {
                    self.set_term(TerminatorKind::Goto(target));
                }
                self.switch_to_bb(target);
            }
        }
    }
}

/// Lowers a typed [`ThirModule`] into a CFG-based [`MirProgram`].
pub fn lower_thir_to_mir(thir: &ThirModule, keep_locals: bool) -> MirProgram {
    let mut builder = MirBuilder::new(thir, keep_locals);
    for stmt in &thir.items {
        builder.lower_stmt(stmt);
    }

    if matches!(
        builder.blocks[builder.current_bb].terminator,
        TerminatorKind::Unreachable
    ) {
        let locals = std::mem::take(&mut builder.scope_locals[0]);
        if !locals.is_empty() {
            builder.push_stmt(StatementKind::DebugScopeEnd {
                locals,
                keep_alive: builder.keep_locals,
            });
        }
        builder.set_term(TerminatorKind::Return(vec![Operand::Constant(
            MirConst::Nil,
        )]));
    }

    MirProgram {
        main: MirBody {
            name: Some("main".to_string()),
            params: Vec::new(),
            num_params: 0,
            is_vararg: false,
            return_type: NeyukiType::Any,
            locals: builder.locals,
            upvalues: Vec::new(),
            blocks: builder.blocks,
            functions: builder.functions,
        },
    }
}
