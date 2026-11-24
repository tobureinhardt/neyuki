//! HIR structural normalization and `-O1` desugaring passes.

use crate::ast::literal::Literal;
use crate::ast::op::{BinOp, UnOp};
use crate::compiler::OptLevel;
use crate::compiler::hir::nodes::{
    HirExpr, HirExprKind, HirLValue, HirModule, HirStmt, HirStmtKind,
};

fn block_has_loop_ctrl(stmts: &[HirStmt]) -> bool {
    stmts.iter().any(|stmt| match &stmt.kind {
        HirStmtKind::Break | HirStmtKind::Continue => true,
        HirStmtKind::Block(body) => block_has_loop_ctrl(body),
        HirStmtKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            block_has_loop_ctrl(then_branch)
                || else_branch
                    .as_ref()
                    .is_some_and(|else_b| block_has_loop_ctrl(else_b))
        }
        _ => false,
    })
}

fn is_static_string(expr: &HirExpr) -> bool {
    matches!(
        expr.kind,
        HirExprKind::Literal(Literal::String(_))
            | HirExprKind::Binary {
                op: BinOp::Concat,
                ..
            }
    )
}

fn is_known_boolean(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Literal(Literal::Bool(_)) => true,
        HirExprKind::Unary { op: UnOp::Not, .. } => true,
        HirExprKind::Binary { op, .. } => matches!(
            op,
            BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge
        ),
        _ => false,
    }
}

fn block_has_label(stmts: &[HirStmt]) -> bool {
    stmts.iter().any(stmt_has_label)
}

fn stmt_has_label(stmt: &HirStmt) -> bool {
    match &stmt.kind {
        HirStmtKind::Label(_) => true,
        HirStmtKind::Block(body)
        | HirStmtKind::While { body, .. }
        | HirStmtKind::Repeat { body, .. }
        | HirStmtKind::For { body, .. }
        | HirStmtKind::NumericFor { body, .. } => block_has_label(body),
        HirStmtKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            block_has_label(then_branch)
                || else_branch
                    .as_ref()
                    .is_some_and(|else_b| block_has_label(else_b))
        }
        _ => false,
    }
}

fn eval_static_truthy(expr: &HirExpr) -> Option<bool> {
    match &expr.kind {
        HirExprKind::Literal(lit) => Some(lit.is_truthy()),
        _ => None,
    }
}

fn optimize_expr(expr: &mut HirExpr, opt_level: OptLevel) {
    match &mut expr.kind {
        HirExprKind::Member { object, .. } => optimize_expr(object, opt_level),
        HirExprKind::Index { object, index } => {
            optimize_expr(object, opt_level);
            optimize_expr(index, opt_level);
        }
        HirExprKind::Call { callee, args } => {
            optimize_expr(callee, opt_level);
            for a in args {
                optimize_expr(a, opt_level);
            }
        }
        HirExprKind::MethodCall { object, args, .. } => {
            optimize_expr(object, opt_level);
            for a in args {
                optimize_expr(a, opt_level);
            }
        }
        HirExprKind::Function { body, .. } => {
            optimize_block(body, opt_level);
        }
        HirExprKind::Paren(inner) => optimize_expr(inner, opt_level),
        HirExprKind::Unary { op, expr: inner } => {
            optimize_expr(inner, opt_level);
            if opt_level != OptLevel::O1 {
                return;
            }
            let op_val = *op;
            match op_val {
                UnOp::Not => {
                    if let HirExprKind::Literal(lit) = &inner.kind {
                        expr.kind = HirExprKind::Literal(Literal::Bool(!lit.is_truthy()));
                    } else if let HirExprKind::Unary {
                        op: UnOp::Not,
                        expr: double_inner,
                    } = &mut inner.kind
                        && is_known_boolean(double_inner)
                    {
                        let replacement = std::mem::replace(
                            &mut double_inner.kind,
                            HirExprKind::Literal(Literal::Nil),
                        );
                        expr.kind = replacement;
                    }
                }
                UnOp::Len => {
                    if let HirExprKind::Literal(Literal::String(s)) = &inner.kind {
                        expr.kind = HirExprKind::Literal(Literal::Int(s.len().into()));
                    }
                }
                UnOp::Neg => match &inner.kind {
                    HirExprKind::Literal(Literal::Int(i)) => {
                        expr.kind = HirExprKind::Literal(Literal::Int(-i.clone()));
                    }
                    HirExprKind::Literal(Literal::Float(f)) => {
                        expr.kind = HirExprKind::Literal(Literal::Float(-*f));
                    }
                    _ => {}
                },
                UnOp::BitNot => {
                    if let HirExprKind::Literal(Literal::Int(i)) = &inner.kind {
                        expr.kind = HirExprKind::Literal(Literal::Int(!i.clone()));
                    }
                }
            }
        }
        HirExprKind::Binary { left, op, right } => {
            optimize_expr(left, opt_level);
            optimize_expr(right, opt_level);
            if opt_level != OptLevel::O1 {
                return;
            }
            match *op {
                BinOp::And => {
                    if let HirExprKind::Literal(lit) = &left.kind {
                        if !lit.is_truthy() {
                            expr.kind = HirExprKind::Literal(lit.clone());
                        } else {
                            let rhs = std::mem::replace(
                                &mut right.kind,
                                HirExprKind::Literal(Literal::Nil),
                            );
                            expr.kind = rhs;
                        }
                    }
                }
                BinOp::Or => {
                    if let HirExprKind::Literal(lit) = &left.kind {
                        if lit.is_truthy() {
                            expr.kind = HirExprKind::Literal(lit.clone());
                        } else {
                            let rhs = std::mem::replace(
                                &mut right.kind,
                                HirExprKind::Literal(Literal::Nil),
                            );
                            expr.kind = rhs;
                        }
                    }
                }
                BinOp::Coalesce => {
                    if let HirExprKind::Literal(lit) = &left.kind {
                        if matches!(lit, Literal::Nil) {
                            let rhs = std::mem::replace(
                                &mut right.kind,
                                HirExprKind::Literal(Literal::Nil),
                            );
                            expr.kind = rhs;
                        } else {
                            expr.kind = HirExprKind::Literal(lit.clone());
                        }
                    }
                }
                BinOp::Concat => {
                    if let (
                        HirExprKind::Literal(Literal::String(a)),
                        HirExprKind::Literal(Literal::String(b)),
                    ) = (&left.kind, &right.kind)
                    {
                        let mut joined = String::with_capacity(a.len() + b.len());
                        joined.push_str(a);
                        joined.push_str(b);
                        expr.kind = HirExprKind::Literal(Literal::String(joined));
                    } else if matches!(&left.kind, HirExprKind::Literal(Literal::String(s)) if s.is_empty())
                        && is_static_string(right)
                    {
                        let rhs =
                            std::mem::replace(&mut right.kind, HirExprKind::Literal(Literal::Nil));
                        expr.kind = rhs;
                    } else if matches!(&right.kind, HirExprKind::Literal(Literal::String(s)) if s.is_empty())
                        && is_static_string(left)
                    {
                        let lhs =
                            std::mem::replace(&mut left.kind, HirExprKind::Literal(Literal::Nil));
                        expr.kind = lhs;
                    }
                }
                _ => {}
            }
        }
        HirExprKind::Table(entries) => {
            for entry in entries {
                if let Some(comp) = &mut entry.computed {
                    optimize_expr(comp, opt_level);
                }
                optimize_expr(&mut entry.value, opt_level);
            }
        }
        HirExprKind::Literal(_) | HirExprKind::Variable(_) | HirExprKind::Vararg => {}
    }
}

fn optimize_lvalue(lv: &mut HirLValue, opt_level: OptLevel) {
    match lv {
        HirLValue::Variable(_) => {}
        HirLValue::Member { object, .. } => optimize_expr(object, opt_level),
        HirLValue::Index { object, index } => {
            optimize_expr(object, opt_level);
            optimize_expr(index, opt_level);
        }
    }
}

fn is_unconditional_terminator(kind: &HirStmtKind) -> bool {
    matches!(
        kind,
        HirStmtKind::Return(_) | HirStmtKind::Break | HirStmtKind::Continue | HirStmtKind::Goto(_)
    )
}

fn optimize_block(stmts: &mut Vec<HirStmt>, opt_level: OptLevel) {
    let mut out = Vec::with_capacity(stmts.len());
    let mut dead_after_term = false;

    for mut stmt in stmts.drain(..) {
        if dead_after_term {
            if matches!(stmt.kind, HirStmtKind::Label(_)) {
                dead_after_term = false;
            } else if !stmt_has_label(&stmt) {
                continue;
            } else {
                dead_after_term = false;
            }
        }

        match &mut stmt.kind {
            HirStmtKind::Local {
                initializer: Some(init),
                ..
            } => optimize_expr(init, opt_level),
            HirStmtKind::LocalMany { initializers, .. } => {
                for init in initializers {
                    optimize_expr(init, opt_level);
                }
            }
            HirStmtKind::Assign { target, value, .. } => {
                optimize_lvalue(target, opt_level);
                optimize_expr(value, opt_level);
            }
            HirStmtKind::AssignMany { targets, values } => {
                for t in targets {
                    optimize_lvalue(t, opt_level);
                }
                for v in values {
                    optimize_expr(v, opt_level);
                }
            }
            HirStmtKind::Increment { target, .. } => optimize_lvalue(target, opt_level),
            HirStmtKind::Function { body, .. } => {
                optimize_block(body, opt_level);
            }
            HirStmtKind::Block(body) => {
                optimize_block(body, opt_level);
                if opt_level == OptLevel::O1 {
                    if body.is_empty() {
                        continue;
                    }
                    if body.len() == 1 && matches!(body[0].kind, HirStmtKind::Block(_)) {
                        let inner = body.remove(0);
                        stmt.kind = inner.kind;
                    }
                }
            }
            HirStmtKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                optimize_expr(condition, opt_level);
                optimize_block(then_branch, opt_level);
                if let Some(else_b) = else_branch {
                    optimize_block(else_b, opt_level);
                }

                if opt_level == OptLevel::O1
                    && let Some(cond_val) = eval_static_truthy(condition)
                {
                    if cond_val {
                        let drop_safe = else_branch.as_ref().is_none_or(|b| !block_has_label(b));
                        if drop_safe {
                            let body = std::mem::take(then_branch);
                            stmt.kind = HirStmtKind::Block(body);
                        }
                    } else if !block_has_label(then_branch) {
                        if let Some(else_b) = else_branch.take() {
                            stmt.kind = HirStmtKind::Block(else_b);
                        } else {
                            continue;
                        }
                    }
                }
            }
            HirStmtKind::For { source, body, .. } => {
                optimize_expr(source, opt_level);
                optimize_block(body, opt_level);
            }
            HirStmtKind::NumericFor {
                start,
                end,
                step,
                body,
                ..
            } => {
                optimize_expr(start, opt_level);
                optimize_expr(end, opt_level);
                optimize_expr(step, opt_level);
                optimize_block(body, opt_level);
            }
            HirStmtKind::While { condition, body } => {
                optimize_expr(condition, opt_level);
                optimize_block(body, opt_level);
                if opt_level == OptLevel::O1
                    && matches!(
                        condition.kind,
                        HirExprKind::Literal(Literal::Bool(false) | Literal::Nil)
                    )
                    && !block_has_label(body)
                {
                    continue;
                }
            }
            HirStmtKind::Repeat { body, condition } => {
                optimize_block(body, opt_level);
                optimize_expr(condition, opt_level);
                if opt_level == OptLevel::O1
                    && eval_static_truthy(condition) == Some(true)
                    && !block_has_label(body)
                    && !block_has_loop_ctrl(body)
                {
                    let inner = std::mem::take(body);
                    if inner.is_empty() {
                        continue;
                    }
                    stmt.kind = HirStmtKind::Block(inner);
                }
            }
            HirStmtKind::Return(vals) => {
                for v in vals {
                    optimize_expr(v, opt_level);
                }
            }
            HirStmtKind::Expr(e) => optimize_expr(e, opt_level),
            HirStmtKind::Local {
                initializer: None, ..
            }
            | HirStmtKind::Break
            | HirStmtKind::Continue
            | HirStmtKind::Goto(_)
            | HirStmtKind::Label(_) => {}
        }

        if opt_level == OptLevel::O1 && is_unconditional_terminator(&stmt.kind) {
            dead_after_term = true;
        }
        out.push(stmt);
    }

    *stmts = out;
}

/// Applies HIR normalization passes according to `opt_level`.
pub fn optimize_hir(module: &mut HirModule, opt_level: OptLevel) {
    optimize_block(&mut module.items, opt_level);
}
