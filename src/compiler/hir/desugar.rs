//! HIR structural normalization and `-O1` desugaring passes.

use crate::ast::literal::Literal;
use crate::compiler::OptLevel;
use crate::compiler::hir::nodes::{
    HirExpr, HirExprKind, HirLValue, HirModule, HirStmt, HirStmtKind,
};

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
        HirExprKind::Unary { expr: inner, .. } => optimize_expr(inner, opt_level),
        HirExprKind::Binary { left, right, .. } => {
            optimize_expr(left, opt_level);
            optimize_expr(right, opt_level);
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
            HirStmtKind::Function { body, .. } | HirStmtKind::Block(body) => {
                optimize_block(body, opt_level);
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
