//! HIR -> THIR lowering pass: assigns static types and lexical bindings.

use crate::ast::Param;
use crate::compiler::hir::{HirExpr, HirExprKind, HirLValue, HirModule, HirStmt, HirStmtKind};
use crate::compiler::thir::nodes::{
    BindingId, BindingKind, ThirExpr, ThirExprKind, ThirId, ThirLValue, ThirModule, ThirNumericFor,
    ThirParam, ThirStmt, ThirStmtKind, ThirTableEntry,
};
use crate::compiler::thir::typeck::{
    TypeContext, infer_binary_type, infer_body_return_type, infer_call_type, infer_index_type,
    infer_literal_type, infer_member_type, infer_table_type, infer_unary_type,
    is_module_import_member,
};
use crate::sema::types::NeyukiType;

struct ThirLowerer {
    ctx: TypeContext,
    next_id: u32,
}

impl ThirLowerer {
    fn new() -> Self {
        Self {
            ctx: TypeContext::new(),
            next_id: 0,
        }
    }

    fn alloc_id(&mut self) -> ThirId {
        let id = ThirId(self.next_id);
        self.next_id += 1;
        id
    }

    fn lower_block(&mut self, stmts: &[HirStmt]) -> Vec<ThirStmt> {
        stmts.iter().map(|s| self.lower_stmt(s)).collect()
    }

    fn parse_annotation(ann: Option<&str>) -> NeyukiType {
        ann.map(NeyukiType::parse).unwrap_or(NeyukiType::Any)
    }

    fn lower_stmt(&mut self, stmt: &HirStmt) -> ThirStmt {
        let thir_id = self.alloc_id();
        let kind = match &stmt.kind {
            HirStmtKind::Local {
                name,
                is_const,
                is_global,
                type_name,
                initializer,
            } => {
                let lowered_init = initializer.as_ref().map(|e| self.lower_expr(e));
                let declared_ty = Self::parse_annotation(type_name.as_deref());
                let effective_ty = if declared_ty != NeyukiType::Any {
                    declared_ty
                } else if let Some(init) = &lowered_init {
                    init.ty.clone()
                } else {
                    NeyukiType::Any
                };
                let bkind = if *is_global {
                    BindingKind::Global
                } else {
                    BindingKind::Local
                };
                let binding =
                    self.ctx
                        .alloc_binding(name.clone(), bkind, *is_const, effective_ty.clone());
                ThirStmtKind::Local {
                    binding,
                    name: name.clone(),
                    is_global: *is_global,
                    ty: effective_ty,
                    initializer: lowered_init,
                }
            }
            HirStmtKind::LocalMany {
                names,
                is_const,
                is_global,
                initializers,
            } => {
                let lowered_inits: Vec<ThirExpr> =
                    initializers.iter().map(|e| self.lower_expr(e)).collect();
                let bkind = if *is_global {
                    BindingKind::Global
                } else {
                    BindingKind::Local
                };
                let mut bindings = Vec::with_capacity(names.len());
                for (idx, name) in names.iter().enumerate() {
                    let ty = lowered_inits
                        .get(idx)
                        .map(|e| e.ty.clone())
                        .unwrap_or(NeyukiType::Any);
                    bindings.push(self.ctx.alloc_binding(name.clone(), bkind, *is_const, ty));
                }
                ThirStmtKind::LocalMany {
                    bindings,
                    names: names.clone(),
                    is_global: *is_global,
                    initializers: lowered_inits,
                }
            }
            HirStmtKind::Assign { target, value, .. } => {
                let lowered_val = self.lower_expr(value);
                let lowered_target = self.lower_lvalue(target);
                if let ThirLValue::Variable {
                    binding: Some(id), ..
                } = &lowered_target
                {
                    self.ctx.refine_binding_type(*id, lowered_val.ty.clone());
                }
                ThirStmtKind::Assign {
                    target: lowered_target,
                    value: lowered_val,
                }
            }
            HirStmtKind::AssignMany { targets, values } => {
                let lowered_vals: Vec<ThirExpr> =
                    values.iter().map(|v| self.lower_expr(v)).collect();
                let lowered_targets: Vec<ThirLValue> =
                    targets.iter().map(|t| self.lower_lvalue(t)).collect();
                for (t, v) in lowered_targets.iter().zip(lowered_vals.iter()) {
                    if let ThirLValue::Variable {
                        binding: Some(id), ..
                    } = t
                    {
                        self.ctx.refine_binding_type(*id, v.ty.clone());
                    }
                }
                ThirStmtKind::AssignMany {
                    targets: lowered_targets,
                    values: lowered_vals,
                }
            }
            HirStmtKind::Increment { target, amount } => ThirStmtKind::Increment {
                target: self.lower_lvalue(target),
                amount: *amount,
            },
            HirStmtKind::Function {
                name,
                is_const,
                is_global,
                params,
                return_type,
                body,
            } => {
                let declared_ret = return_type.as_deref().map(NeyukiType::parse);
                let param_tys: Vec<NeyukiType> = params
                    .iter()
                    .filter(|p| !p.variadic)
                    .map(|p| Self::parse_annotation(p.type_name.as_deref()))
                    .collect();
                let initial_sig = NeyukiType::Function {
                    params: param_tys.clone(),
                    return_type: Box::new(declared_ret.clone().unwrap_or(NeyukiType::Any)),
                    is_vararg: params.iter().any(|p| p.variadic),
                };
                // Bind name before lowering body so recursive calls see it.
                // A `global function` has no local binding: it is assigned to the global.
                let fn_binding = name.as_ref().filter(|_| !*is_global).map(|n| {
                    if let Some((Some(existing), _)) = self
                        .ctx
                        .scopes
                        .last()
                        .map(|s| (s.locals.get(n).copied(), ()))
                    {
                        existing
                    } else {
                        self.ctx.alloc_binding(
                            n.clone(),
                            BindingKind::Local,
                            *is_const,
                            initial_sig,
                        )
                    }
                });

                let (thir_params, lowered_body, captures, effective_ret) =
                    self.lower_function_parts(params, declared_ret, body);
                let final_sig = NeyukiType::Function {
                    params: param_tys,
                    return_type: Box::new(effective_ret.clone()),
                    is_vararg: params.iter().any(|p| p.variadic),
                };
                if let Some(id) = fn_binding
                    && let Some(b) = self.ctx.bindings.get_mut(id.0 as usize)
                {
                    b.ty = final_sig;
                }
                ThirStmtKind::Function {
                    binding: fn_binding,
                    name: name.clone(),
                    is_global: *is_global,
                    params: thir_params,
                    return_type: effective_ret,
                    captures,
                    body: lowered_body,
                }
            }
            HirStmtKind::Block(body) => {
                self.ctx.enter_scope();
                let lowered = self.lower_block(body);
                self.ctx.exit_scope();
                ThirStmtKind::Block(lowered)
            }
            HirStmtKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let cond = self.lower_expr(condition);
                self.ctx.enter_scope();
                let then_b = self.lower_block(then_branch);
                self.ctx.exit_scope();
                let else_b = else_branch.as_ref().map(|b| {
                    self.ctx.enter_scope();
                    let lb = self.lower_block(b);
                    self.ctx.exit_scope();
                    lb
                });
                ThirStmtKind::If {
                    condition: cond,
                    then_branch: then_b,
                    else_branch: else_b,
                }
            }
            HirStmtKind::For { vars, source, body } => {
                let lowered_src = self.lower_expr(source);
                self.ctx.enter_scope();
                let mut bindings = Vec::with_capacity(vars.len());
                for v in vars {
                    bindings.push(self.ctx.alloc_binding(
                        v.clone(),
                        BindingKind::Local,
                        false,
                        NeyukiType::Any,
                    ));
                }
                let lowered_body = self.lower_block(body);
                self.ctx.exit_scope();
                ThirStmtKind::For {
                    bindings,
                    vars: vars.clone(),
                    source: lowered_src,
                    body: lowered_body,
                }
            }
            HirStmtKind::NumericFor {
                var,
                start,
                end,
                step,
                body,
            } => {
                let l_start = self.lower_expr(start);
                let l_end = self.lower_expr(end);
                let l_step = self.lower_expr(step);
                let loop_ty = if l_start.ty == NeyukiType::Int && l_step.ty == NeyukiType::Int {
                    NeyukiType::Int
                } else {
                    NeyukiType::Number
                };
                self.ctx.enter_scope();
                let binding =
                    self.ctx
                        .alloc_binding(var.clone(), BindingKind::Local, false, loop_ty);
                let lowered_body = self.lower_block(body);
                self.ctx.exit_scope();
                ThirStmtKind::NumericFor(Box::new(ThirNumericFor {
                    binding,
                    var: var.clone(),
                    start: l_start,
                    end: l_end,
                    step: l_step,
                    body: lowered_body,
                }))
            }
            HirStmtKind::While { condition, body } => {
                let cond = self.lower_expr(condition);
                self.ctx.enter_scope();
                let lowered_body = self.lower_block(body);
                self.ctx.exit_scope();
                ThirStmtKind::While {
                    condition: cond,
                    body: lowered_body,
                }
            }
            HirStmtKind::Repeat { body, condition } => {
                self.ctx.enter_scope();
                let lowered_body = self.lower_block(body);
                let cond = self.lower_expr(condition);
                self.ctx.exit_scope();
                ThirStmtKind::Repeat {
                    body: lowered_body,
                    condition: cond,
                }
            }
            HirStmtKind::Return(vals) => {
                ThirStmtKind::Return(vals.iter().map(|v| self.lower_expr(v)).collect())
            }
            HirStmtKind::Break => ThirStmtKind::Break,
            HirStmtKind::Continue => ThirStmtKind::Continue,
            HirStmtKind::Goto(lbl) => ThirStmtKind::Goto(lbl.clone()),
            HirStmtKind::Label(lbl) => ThirStmtKind::Label(lbl.clone()),
            HirStmtKind::Expr(e) => ThirStmtKind::Expr(self.lower_expr(e)),
        };

        ThirStmt {
            thir_id,
            hir_id: stmt.hir_id,
            line: stmt.line,
            kind,
        }
    }

    fn lower_function_parts(
        &mut self,
        params: &[Param],
        declared_ret: Option<NeyukiType>,
        body: &[HirStmt],
    ) -> (Vec<ThirParam>, Vec<ThirStmt>, Vec<BindingId>, NeyukiType) {
        self.ctx.enter_fn();
        let mut thir_params = Vec::with_capacity(params.len());
        for p in params {
            let ty = Self::parse_annotation(p.type_name.as_deref());
            let binding = if p.variadic {
                None
            } else {
                Some(
                    self.ctx
                        .alloc_binding(p.name.clone(), BindingKind::Param, false, ty.clone()),
                )
            };
            thir_params.push(ThirParam {
                binding,
                name: p.name.clone(),
                ty,
                variadic: p.variadic,
            });
        }
        let lowered_body = self.lower_block(body);
        let captures = self.ctx.exit_fn();
        let effective_ret = declared_ret.unwrap_or_else(|| infer_body_return_type(&lowered_body));
        (thir_params, lowered_body, captures, effective_ret)
    }

    fn lower_lvalue(&mut self, lv: &HirLValue) -> ThirLValue {
        match lv {
            HirLValue::Variable(name) => {
                let (binding, _) = self.ctx.resolve_var(name);
                ThirLValue::Variable {
                    name: name.clone(),
                    binding,
                }
            }
            HirLValue::Member { object, field } => ThirLValue::Member {
                object: Box::new(self.lower_expr(object)),
                field: field.clone(),
            },
            HirLValue::Index { object, index } => ThirLValue::Index {
                object: Box::new(self.lower_expr(object)),
                index: Box::new(self.lower_expr(index)),
            },
        }
    }

    fn lower_expr(&mut self, expr: &HirExpr) -> ThirExpr {
        let thir_id = self.alloc_id();
        let (ty, kind) = match &expr.kind {
            HirExprKind::Literal(lit) => {
                (infer_literal_type(lit), ThirExprKind::Literal(lit.clone()))
            }
            HirExprKind::Variable(name) => {
                let (binding, ty) = self.ctx.resolve_var(name);
                (
                    ty,
                    ThirExprKind::Variable {
                        name: name.clone(),
                        binding,
                    },
                )
            }
            HirExprKind::Vararg => (NeyukiType::Any, ThirExprKind::Vararg),
            HirExprKind::Paren(inner) => {
                let inner = self.lower_expr(inner);
                (inner.ty.first_value(), ThirExprKind::Paren(Box::new(inner)))
            }
            HirExprKind::Member { object, field } => {
                let obj = self.lower_expr(object);
                let is_module_import = is_module_import_member(&obj.kind);
                let ty = infer_member_type(&obj, field);
                (
                    ty,
                    ThirExprKind::Member {
                        object: Box::new(obj),
                        field: field.clone(),
                        is_module_import,
                    },
                )
            }
            HirExprKind::Index { object, index } => {
                let obj = self.lower_expr(object);
                let idx = self.lower_expr(index);
                let ty = infer_index_type(&obj);
                (
                    ty,
                    ThirExprKind::Index {
                        object: Box::new(obj),
                        index: Box::new(idx),
                    },
                )
            }
            HirExprKind::Call { callee, args } => {
                let c = self.lower_expr(callee);
                let a: Vec<ThirExpr> = args.iter().map(|arg| self.lower_expr(arg)).collect();
                let ty = infer_call_type(&c, &a);
                (
                    ty,
                    ThirExprKind::Call {
                        callee: Box::new(c),
                        args: a,
                    },
                )
            }
            HirExprKind::MethodCall {
                object,
                method,
                args,
            } => {
                let obj = self.lower_expr(object);
                let a: Vec<ThirExpr> = args.iter().map(|arg| self.lower_expr(arg)).collect();
                (
                    NeyukiType::Any,
                    ThirExprKind::MethodCall {
                        object: Box::new(obj),
                        method: method.clone(),
                        args: a,
                    },
                )
            }
            HirExprKind::Function { params, body } => {
                let param_tys: Vec<NeyukiType> = params
                    .iter()
                    .filter(|p| !p.variadic)
                    .map(|p| Self::parse_annotation(p.type_name.as_deref()))
                    .collect();
                let (thir_params, lowered_body, captures, ret_ty) =
                    self.lower_function_parts(params, None, body);
                let fn_ty = NeyukiType::Function {
                    params: param_tys,
                    return_type: Box::new(ret_ty.clone()),
                    is_vararg: params.iter().any(|p| p.variadic),
                };
                (
                    fn_ty,
                    ThirExprKind::Function {
                        params: thir_params,
                        return_type: ret_ty,
                        captures,
                        body: lowered_body,
                    },
                )
            }
            HirExprKind::Unary { op, expr: inner } => {
                let l_inner = self.lower_expr(inner);
                let ty = infer_unary_type(*op, &l_inner);
                (
                    ty,
                    ThirExprKind::Unary {
                        op: *op,
                        expr: Box::new(l_inner),
                    },
                )
            }
            HirExprKind::Binary { left, op, right } => {
                let l = self.lower_expr(left);
                let r = self.lower_expr(right);
                let ty = infer_binary_type(*op, &l, &r);
                (
                    ty,
                    ThirExprKind::Binary {
                        left: Box::new(l),
                        op: *op,
                        right: Box::new(r),
                    },
                )
            }
            HirExprKind::Table(entries) => {
                let lowered: Vec<ThirTableEntry> = entries
                    .iter()
                    .map(|e| ThirTableEntry {
                        key: e.key.clone(),
                        computed: e.computed.as_ref().map(|c| Box::new(self.lower_expr(c))),
                        value: self.lower_expr(&e.value),
                    })
                    .collect();
                let summary: Vec<(Option<&str>, bool, &NeyukiType)> = lowered
                    .iter()
                    .map(|e| (e.key.as_deref(), e.is_array_element(), &e.value.ty))
                    .collect();
                let ty = infer_table_type(&summary);
                (ty, ThirExprKind::Table(lowered))
            }
        };

        ThirExpr {
            thir_id,
            hir_id: expr.hir_id,
            ty,
            kind,
        }
    }
}

/// Lowers an [`HirModule`] into a typed, lexically-resolved [`ThirModule`].
pub fn lower_hir_to_thir(hir: &HirModule) -> ThirModule {
    let mut lowerer = ThirLowerer::new();
    let items = lowerer.lower_block(&hir.items);
    ThirModule {
        items,
        bindings: lowerer.ctx.bindings,
    }
}
