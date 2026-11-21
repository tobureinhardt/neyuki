//! Expression & function return type inference and call-site type checking.

use std::collections::HashMap;

use crate::ast::expr::{Expr, Param};
use crate::ast::literal::Literal;
use crate::ast::op::{BinOp, UnOp};
use crate::ast::stmt::{IfData, Stmt};
use crate::diagnostics::diagnostic::Diagnostic;
use crate::diagnostics::error_code::ErrorCode;
use crate::sema::analyzer::SemanticAnalyzer;
use crate::sema::generics;
use crate::sema::scope::ScopeKind;
use crate::sema::types::NeyukiType;

impl<'a> SemanticAnalyzer<'a> {
    pub(super) fn infer_expr_type(&self, expr: &Expr) -> NeyukiType {
        let env = HashMap::new();
        self.infer_expr_with_env(expr, &env)
    }

    /// Infers an expression's type while consulting temporary local/parameter bindings
    /// (used when inferring unannotated function return types before entering scope).
    fn infer_expr_with_env(&self, expr: &Expr, env: &HashMap<String, NeyukiType>) -> NeyukiType {
        match expr {
            Expr::Literal { value: val, .. } => match val {
                Literal::Nil => NeyukiType::Nil,
                Literal::Bool(_) => NeyukiType::Boolean,
                Literal::Int(_) => NeyukiType::Int,
                Literal::Float(_) => NeyukiType::Float,
                Literal::String(_) => NeyukiType::String,
            },
            Expr::Interp { .. } => NeyukiType::String,
            Expr::Table { entries, .. } => {
                if entries.is_empty() {
                    NeyukiType::Table
                } else if entries.iter().all(|e| e.is_array_element()) {
                    let elem_ty = entries.iter().fold(NeyukiType::Never, |acc, e| {
                        acc.lub(&self.infer_expr_with_env(&e.value, env))
                    });
                    NeyukiType::Array(Box::new(elem_ty))
                } else if let Some(fields) = entries
                    .iter()
                    .map(|e| {
                        e.key
                            .as_ref()
                            .map(|k| (k.clone(), self.infer_expr_with_env(&e.value, env)))
                    })
                    .collect::<Option<Vec<_>>>()
                {
                    NeyukiType::Record(fields)
                } else {
                    NeyukiType::Table
                }
            }
            Expr::Function { params, body, .. } => self.build_function_type(params, body, None),
            Expr::Variable { name, .. } => {
                if let Some(ty) = env.get(name) {
                    ty.clone()
                } else if let Some(sym) = self.scope_mgr.lookup(name) {
                    sym.inferred_type.clone()
                } else {
                    NeyukiType::Any
                }
            }
            Expr::Member { object, field, .. } => {
                if let NeyukiType::Record(fields) = self.infer_expr_with_env(object, env)
                    && let Some((_, field_ty)) = fields.iter().find(|(k, _)| k == field)
                {
                    return field_ty.clone();
                }
                NeyukiType::Any
            }
            Expr::Index { object, .. } => match self.infer_expr_with_env(object, env) {
                NeyukiType::Array(elem) => *elem,
                NeyukiType::TableMap { val, .. } => *val,
                _ => NeyukiType::Any,
            },
            Expr::Call { callee, args, .. } => {
                let callee_ty = self.infer_expr_with_env(callee, env);
                let arg_tys: Vec<NeyukiType> = args
                    .iter()
                    .map(|a| self.infer_expr_with_env(a, env))
                    .collect();
                if let Ok((ret, _)) = generics::instantiate_call(&callee_ty, &arg_tys) {
                    ret
                } else {
                    match callee_ty {
                        NeyukiType::Generic { inner, .. } => match *inner {
                            NeyukiType::Function { return_type, .. } => *return_type,
                            other => other,
                        },
                        NeyukiType::Function { return_type, .. } => *return_type,
                        _ => NeyukiType::Any,
                    }
                }
            }
            Expr::Unary {
                op, expr: inner, ..
            } => match op {
                UnOp::Not => NeyukiType::Boolean,
                UnOp::Len | UnOp::BitNot => NeyukiType::Int,
                UnOp::Neg => {
                    let inner_ty = self.infer_expr_with_env(inner, env);
                    if inner_ty == NeyukiType::Int {
                        NeyukiType::Int
                    } else if inner_ty == NeyukiType::Float {
                        NeyukiType::Float
                    } else {
                        NeyukiType::Number
                    }
                }
            },
            Expr::Binary {
                left, op, right, ..
            } => match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    NeyukiType::Boolean
                }
                BinOp::Concat => NeyukiType::String,
                BinOp::Coalesce => {
                    let l_ty = self.infer_expr_with_env(left, env).narrow_non_nil();
                    let r_ty = self.infer_expr_with_env(right, env);
                    l_ty.lub(&r_ty)
                }
                BinOp::And => {
                    let l_ty = self.infer_expr_with_env(left, env);
                    let r_ty = self.infer_expr_with_env(right, env);
                    l_ty.lub(&r_ty)
                }
                BinOp::Or => {
                    if let Expr::Binary {
                        op: BinOp::And,
                        right: then_val,
                        ..
                    } = left.as_ref()
                    {
                        let then_ty = self.infer_expr_with_env(then_val, env);
                        if !can_be_falsy(&then_ty) {
                            let else_ty = self.infer_expr_with_env(right, env);
                            return then_ty.lub(&else_ty);
                        }
                    }
                    let l_ty = self.infer_expr_with_env(left, env);
                    let r_ty = self.infer_expr_with_env(right, env);
                    l_ty.lub(&r_ty)
                }
                BinOp::BitAnd
                | BinOp::BitOr
                | BinOp::BitXor
                | BinOp::Shl
                | BinOp::Shr
                | BinOp::IDiv
                | BinOp::Mod => NeyukiType::Int,
                BinOp::Div => NeyukiType::Float,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Pow => {
                    let l_ty = self.infer_expr_with_env(left, env);
                    let r_ty = self.infer_expr_with_env(right, env);
                    // An operand of unknown type (a call, a table field)
                    // could be an integer or a float, or have metamethods.
                    if l_ty == NeyukiType::Any || r_ty == NeyukiType::Any {
                        NeyukiType::Any
                    } else if l_ty == NeyukiType::Int && r_ty == NeyukiType::Int {
                        NeyukiType::Int
                    } else if l_ty == NeyukiType::Float || r_ty == NeyukiType::Float {
                        NeyukiType::Float
                    } else {
                        NeyukiType::Number
                    }
                }
                _ => NeyukiType::Number,
            },
            Expr::MethodCall { .. } | Expr::Vararg { .. } => NeyukiType::Any,
        }
    }

    /// Builds a function's static type, inferring the return type from `body`
    /// when `explicit_ret` is `None`.
    pub(super) fn build_function_type(
        &self,
        params: &[Param],
        body: &[Stmt],
        explicit_ret: Option<NeyukiType>,
    ) -> NeyukiType {
        let mut param_types = Vec::with_capacity(params.len());
        let mut env = HashMap::new();
        for p in params {
            let ty = p
                .type_name
                .as_deref()
                .map(|t| self.parse_ty(t))
                .unwrap_or(NeyukiType::Any);
            env.insert(p.name.clone(), ty.clone());
            if !p.variadic {
                param_types.push(ty);
            }
        }
        let is_vararg = params.iter().any(|p| p.variadic);
        let ret_ty =
            explicit_ret.unwrap_or_else(|| self.infer_body_return_type_with_env(body, &env));
        generics::generalize_function(param_types, ret_ty, is_vararg)
    }

    fn infer_body_return_type_with_env(
        &self,
        body: &[Stmt],
        initial_env: &HashMap<String, NeyukiType>,
    ) -> NeyukiType {
        let mut env = initial_env.clone();
        let mut acc = NeyukiType::Never;
        let mut saw_return = false;
        self.collect_stmt_returns(body, &mut env, &mut acc, &mut saw_return);
        if !saw_return {
            NeyukiType::Nil
        } else if !definitely_returns(body) {
            acc.lub(&NeyukiType::Nil)
        } else {
            acc
        }
    }

    fn collect_stmt_returns(
        &self,
        stmts: &[Stmt],
        env: &mut HashMap<String, NeyukiType>,
        acc: &mut NeyukiType,
        saw_return: &mut bool,
    ) {
        for stmt in stmts {
            match stmt {
                Stmt::Local {
                    name,
                    type_name,
                    initializer,
                    ..
                } => {
                    let ty = type_name
                        .as_deref()
                        .map(|t| self.parse_ty(t))
                        .or_else(|| {
                            initializer
                                .as_ref()
                                .map(|e| self.infer_expr_with_env(e, env))
                        })
                        .unwrap_or(NeyukiType::Any);
                    env.insert(name.clone(), ty);
                }
                Stmt::Return { values, .. } => {
                    *saw_return = true;
                    let ty = values
                        .first()
                        .map(|e| self.infer_expr_with_env(e, env))
                        .unwrap_or(NeyukiType::Nil);
                    *acc = acc.lub(&ty);
                }
                Stmt::If { data, .. } => {
                    let mut then_env = env.clone();
                    self.collect_stmt_returns(&data.then_branch, &mut then_env, acc, saw_return);
                    for (_, branch) in &data.else_if_branches {
                        let mut br_env = env.clone();
                        self.collect_stmt_returns(branch, &mut br_env, acc, saw_return);
                    }
                    if let Some(branch) = &data.else_branch {
                        let mut else_env = env.clone();
                        self.collect_stmt_returns(branch, &mut else_env, acc, saw_return);
                    }
                }
                Stmt::While { body, .. } | Stmt::Repeat { body, .. } => {
                    let mut loop_env = env.clone();
                    self.collect_stmt_returns(body, &mut loop_env, acc, saw_return);
                }
                Stmt::NumericFor { data, .. } => {
                    let mut loop_env = env.clone();
                    loop_env.insert(data.var.clone(), NeyukiType::Number);
                    self.collect_stmt_returns(&data.body, &mut loop_env, acc, saw_return);
                }
                Stmt::For { vars, body, .. } => {
                    let mut loop_env = env.clone();
                    for v in vars {
                        loop_env.insert(v.clone(), NeyukiType::Any);
                    }
                    self.collect_stmt_returns(body, &mut loop_env, acc, saw_return);
                }
                Stmt::Function { .. } => {}
                _ => {}
            }
        }
    }

    /// Checks arity and argument compatibility at a call expression.
    pub(super) fn check_call_site(&mut self, callee: &Expr, args: &[Expr]) {
        if let Expr::Variable { name: fn_name, .. } = callee
            && let Some(sym) = self.scope_mgr.lookup(fn_name)
            && let Some(expected) = sym.num_params
            && !sym.is_vararg
            && (args.len() > expected || args.len() < sym.min_params.unwrap_or(expected))
        {
            let span = self.find_ident_span(fn_name);
            let diag = Diagnostic::warning(format!(
                "function '{}' takes {} argument(s) but {} were supplied",
                fn_name,
                expected,
                args.len()
            ))
            .with_code(ErrorCode::E0004)
            .with_label(span, format!("takes {} arguments", expected))
            .with_secondary_label(sym.span, "defined here");
            self.diagnostics.push(diag);
        }

        let callee_ty = self.infer_expr_type(callee);
        let spreads = matches!(
            args.last(),
            Some(Expr::Call { .. } | Expr::MethodCall { .. } | Expr::Vararg { .. })
        );
        let checked_count = if spreads {
            args.len().saturating_sub(1)
        } else {
            args.len()
        };

        if let NeyukiType::Generic { .. } = callee_ty {
            let arg_tys: Vec<NeyukiType> = args[..checked_count]
                .iter()
                .map(|a| self.infer_expr_type(a))
                .collect();
            if let Err(reason) = generics::instantiate_call(&callee_ty, &arg_tys) {
                let span = self.find_expr_span(callee);
                let diag = Diagnostic::error(format!("generic call type error: {}", reason))
                    .with_code(ErrorCode::E0003)
                    .with_label(span, reason);
                self.diagnostics.push(diag);
            }
        }
    }

    /// Analyzes an `if` statement with flow-sensitive condition narrowing and branch joins.
    pub(super) fn analyze_if_stmt(&mut self, data: &IfData) {
        self.analyze_expr(&data.condition);
        let simple_if = data.else_if_branches.is_empty() && data.else_branch.is_none();

        let mut constraints = Vec::new();
        collect_condition_constraints(&data.condition, &mut constraints);

        let mut saved: Vec<(String, NeyukiType, NeyukiType, NeyukiType)> = Vec::new();
        for c in constraints {
            if let Some(sym) = self.scope_mgr.lookup_mut(&c.var) {
                let pre = sym.inferred_type.clone();
                let (then_ty, else_ty) = apply_constraint(&pre, &c.kind);
                if then_ty != pre || else_ty != pre {
                    sym.inferred_type = then_ty.clone();
                    saved.push((c.var, pre, then_ty, else_ty));
                }
            }
        }

        self.scope_mgr.enter_scope(ScopeKind::Block);
        self.analyze_block(&data.then_branch);
        self.check_unused_symbols();

        if simple_if {
            let then_exits = branch_definitely_exits(&data.then_branch);
            for (var, pre, then_entry_ty, else_ty) in saved {
                if let Some(sym) = self.scope_mgr.lookup_mut(&var) {
                    let then_end_ty = sym.inferred_type.clone();
                    if then_exits {
                        sym.inferred_type = else_ty;
                    } else if then_end_ty != then_entry_ty {
                        sym.inferred_type = then_end_ty.lub(&else_ty);
                    } else {
                        sym.inferred_type = pre;
                    }
                }
            }
            return;
        }

        for (var, pre, _, _) in saved {
            if let Some(sym) = self.scope_mgr.lookup_mut(&var) {
                sym.inferred_type = pre;
            }
        }

        for (cond, branch) in &data.else_if_branches {
            self.analyze_expr(cond);
            let mut br_constraints = Vec::new();
            collect_condition_constraints(cond, &mut br_constraints);
            let mut br_saved = Vec::new();
            for c in br_constraints {
                if let Some(sym) = self.scope_mgr.lookup_mut(&c.var) {
                    let pre = sym.inferred_type.clone();
                    let (then_ty, _) = apply_constraint(&pre, &c.kind);
                    if then_ty != pre {
                        sym.inferred_type = then_ty;
                        br_saved.push((c.var, pre));
                    }
                }
            }
            self.scope_mgr.enter_scope(ScopeKind::Block);
            self.analyze_block(branch);
            self.check_unused_symbols();
            for (var, pre) in br_saved {
                if let Some(sym) = self.scope_mgr.lookup_mut(&var) {
                    sym.inferred_type = pre;
                }
            }
        }

        if let Some(branch) = &data.else_branch {
            self.scope_mgr.enter_scope(ScopeKind::Block);
            self.analyze_block(branch);
            self.check_unused_symbols();
        }
    }
}

/// Resolves the inferred type for identifier `idx` in `local a, b, ... = e1, ...`,
/// unpacking trailing multi-return tuple types (`_0`, `_1`, ...).
pub(super) fn resolve_local_many_slot(inits: &[NeyukiType], idx: usize) -> Option<NeyukiType> {
    if inits.is_empty() {
        return None;
    }
    if idx + 1 < inits.len() {
        return Some(inits[idx].clone());
    }
    let last_pos = inits.len() - 1;
    let last_ty = &inits[last_pos];
    if let NeyukiType::Record(fields) = last_ty
        && fields.first().is_some_and(|(k, _)| k == "_0")
    {
        let slot = idx - last_pos;
        return Some(
            fields
                .get(slot)
                .map(|(_, ty)| ty.clone())
                .unwrap_or(NeyukiType::Nil),
        );
    }
    if idx == last_pos {
        Some(last_ty.clone())
    } else {
        None
    }
}

enum ConstraintKind {
    Truthy,
    NonNil,
    IsNil,
    TypeEq(String),
    TypeNe(String),
}

struct ConditionConstraint {
    var: String,
    kind: ConstraintKind,
}

fn push_constraint(out: &mut Vec<ConditionConstraint>, var: &str, kind: ConstraintKind) {
    if !out.iter().any(|c| c.var == var) {
        out.push(ConditionConstraint {
            var: var.to_string(),
            kind,
        });
    }
}

fn extract_typeof_var(expr: &Expr) -> Option<&str> {
    if let Expr::Call { callee, args, .. } = expr
        && let Expr::Variable { name, .. } = callee.as_ref()
        && (name == "type" || name == "typeof")
        && args.len() == 1
        && let Expr::Variable { name: var, .. } = &args[0]
    {
        return Some(var.as_str());
    }
    None
}

fn collect_condition_constraints(cond: &Expr, out: &mut Vec<ConditionConstraint>) {
    match cond {
        Expr::Variable { name, .. } => push_constraint(out, name, ConstraintKind::Truthy),
        Expr::Binary {
            left,
            op: BinOp::Ne,
            right,
            ..
        } => match (left.as_ref(), right.as_ref()) {
            (
                Expr::Variable { name, .. },
                Expr::Literal {
                    value: Literal::Nil,
                    ..
                },
            )
            | (
                Expr::Literal {
                    value: Literal::Nil,
                    ..
                },
                Expr::Variable { name, .. },
            ) => push_constraint(out, name, ConstraintKind::NonNil),
            (
                call_expr,
                Expr::Literal {
                    value: Literal::String(tag),
                    ..
                },
            )
            | (
                Expr::Literal {
                    value: Literal::String(tag),
                    ..
                },
                call_expr,
            ) => {
                if let Some(var) = extract_typeof_var(call_expr) {
                    push_constraint(out, var, ConstraintKind::TypeNe(tag.clone()));
                }
            }
            _ => {}
        },
        Expr::Binary {
            left,
            op: BinOp::Eq,
            right,
            ..
        } => match (left.as_ref(), right.as_ref()) {
            (
                Expr::Variable { name, .. },
                Expr::Literal {
                    value: Literal::Nil,
                    ..
                },
            )
            | (
                Expr::Literal {
                    value: Literal::Nil,
                    ..
                },
                Expr::Variable { name, .. },
            ) => push_constraint(out, name, ConstraintKind::IsNil),
            (
                call_expr,
                Expr::Literal {
                    value: Literal::String(tag),
                    ..
                },
            )
            | (
                Expr::Literal {
                    value: Literal::String(tag),
                    ..
                },
                call_expr,
            ) => {
                if let Some(var) = extract_typeof_var(call_expr) {
                    push_constraint(out, var, ConstraintKind::TypeEq(tag.clone()));
                }
            }
            _ => {}
        },
        Expr::Binary {
            left,
            op: BinOp::And,
            right,
            ..
        } => {
            collect_condition_constraints(left, out);
            collect_condition_constraints(right, out);
        }
        _ => {}
    }
}

fn apply_constraint(ty: &NeyukiType, kind: &ConstraintKind) -> (NeyukiType, NeyukiType) {
    match kind {
        ConstraintKind::Truthy => (ty.narrow_truthy(), ty.clone()),
        ConstraintKind::NonNil => (ty.narrow_non_nil(), NeyukiType::Nil),
        ConstraintKind::IsNil => (NeyukiType::Nil, ty.narrow_non_nil()),
        ConstraintKind::TypeEq(tag) => (narrow_to_tag(ty, tag), exclude_tag(ty, tag)),
        ConstraintKind::TypeNe(tag) => (exclude_tag(ty, tag), narrow_to_tag(ty, tag)),
    }
}

fn tag_default_type(tag: &str) -> NeyukiType {
    match tag {
        "nil" => NeyukiType::Nil,
        "boolean" => NeyukiType::Boolean,
        "number" => NeyukiType::Number,
        "string" => NeyukiType::String,
        "table" => NeyukiType::Table,
        "function" => NeyukiType::Function {
            params: vec![],
            return_type: Box::new(NeyukiType::Any),
            is_vararg: true,
        },
        "thread" => NeyukiType::Thread,
        "buffer" => NeyukiType::Buffer,
        _ => NeyukiType::Any,
    }
}

fn variant_definitely_matches_tag(ty: &NeyukiType, tag: &str) -> bool {
    match tag {
        "nil" => *ty == NeyukiType::Nil,
        "boolean" => *ty == NeyukiType::Boolean,
        "number" => matches!(ty, NeyukiType::Int | NeyukiType::Float | NeyukiType::Number),
        "string" => *ty == NeyukiType::String,
        "table" => matches!(
            ty,
            NeyukiType::Table
                | NeyukiType::Array(_)
                | NeyukiType::Record(_)
                | NeyukiType::TableMap { .. }
        ),
        "function" => matches!(ty, NeyukiType::Function { .. } | NeyukiType::Generic { .. }),
        "thread" => *ty == NeyukiType::Thread,
        "buffer" => *ty == NeyukiType::Buffer,
        _ => false,
    }
}

fn narrow_to_tag(ty: &NeyukiType, tag: &str) -> NeyukiType {
    match ty {
        NeyukiType::Any => tag_default_type(tag),
        NeyukiType::Optional(inner) => {
            if tag == "nil" {
                NeyukiType::Nil
            } else {
                narrow_to_tag(inner, tag)
            }
        }
        NeyukiType::Union(variants) => {
            let matching: Vec<NeyukiType> = variants
                .iter()
                .filter(|v| variant_definitely_matches_tag(v, tag))
                .cloned()
                .collect();
            match matching.len() {
                0 => tag_default_type(tag),
                1 => matching.into_iter().next().expect("checked: one variant"),
                _ => NeyukiType::Union(matching),
            }
        }
        other => {
            if variant_definitely_matches_tag(other, tag) {
                other.clone()
            } else {
                tag_default_type(tag)
            }
        }
    }
}

fn exclude_tag(ty: &NeyukiType, tag: &str) -> NeyukiType {
    match ty {
        NeyukiType::Optional(inner) => {
            if tag == "nil" {
                (**inner).clone()
            } else {
                let inner_excluded = exclude_tag(inner, tag);
                if inner_excluded == NeyukiType::Never {
                    NeyukiType::Nil
                } else {
                    NeyukiType::Optional(Box::new(inner_excluded))
                }
            }
        }
        NeyukiType::Union(variants) => {
            let kept: Vec<NeyukiType> = variants
                .iter()
                .filter(|v| !variant_definitely_matches_tag(v, tag))
                .cloned()
                .collect();
            match kept.len() {
                0 => NeyukiType::Never,
                1 => kept.into_iter().next().expect("checked: one variant"),
                _ => NeyukiType::Union(kept),
            }
        }
        other => {
            if variant_definitely_matches_tag(other, tag) {
                NeyukiType::Never
            } else {
                other.clone()
            }
        }
    }
}

fn branch_definitely_exits(stmts: &[Stmt]) -> bool {
    for stmt in stmts {
        match stmt {
            Stmt::Return { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => return true,
            Stmt::Expr { expr, .. } => {
                if let Expr::Call { callee, .. } = expr
                    && let Expr::Variable { name, .. } = callee.as_ref()
                    && name == "error"
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn can_be_falsy(ty: &NeyukiType) -> bool {
    match ty {
        NeyukiType::Any
        | NeyukiType::Boolean
        | NeyukiType::Nil
        | NeyukiType::Optional(_)
        | NeyukiType::Custom(_)
        | NeyukiType::TypeVar(_) => true,
        NeyukiType::Union(variants) => variants.iter().any(can_be_falsy),
        _ => false,
    }
}

fn definitely_returns(stmts: &[Stmt]) -> bool {
    for stmt in stmts {
        match stmt {
            Stmt::Return { .. } => return true,
            Stmt::If { data, .. } => {
                let all_branches = definitely_returns(&data.then_branch)
                    && data
                        .else_if_branches
                        .iter()
                        .all(|(_, b)| definitely_returns(b))
                    && data.else_branch.as_deref().is_some_and(definitely_returns);
                if all_branches {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}
