//! Human-readable HIR printer (`neyuki dump-hir`).

use std::fmt::Write;

use crate::compiler::hir::nodes::{
    HirExpr, HirExprKind, HirLValue, HirModule, HirStmt, HirStmtKind,
};

/// Formats an [`HirModule`] into readable indented text.
pub fn dump_hir(module: &HirModule) -> String {
    let mut out = String::from("=== HIR ===\n");
    for stmt in &module.items {
        write_stmt(&mut out, stmt, 0);
    }
    out
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_stmt(out: &mut String, stmt: &HirStmt, depth: usize) {
    indent(out, depth);
    let _ = write!(out, "[hir#{}:L{}] ", stmt.hir_id.0, stmt.line);
    match &stmt.kind {
        HirStmtKind::Local {
            name,
            is_const,
            is_global,
            type_name,
            initializer,
        } => {
            let kw = if *is_global { "global" } else { "local" };
            let ckw = if *is_const { " const" } else { "" };
            let ty = type_name
                .as_deref()
                .map(|t| format!(": {}", t))
                .unwrap_or_default();
            let _ = write!(out, "{}{} {}{}", kw, ckw, name, ty);
            if let Some(init) = initializer {
                out.push_str(" = ");
                write_expr(out, init);
            }
            out.push('\n');
        }
        HirStmtKind::LocalMany {
            names,
            is_const,
            is_global,
            initializers,
        } => {
            let kw = if *is_global { "global" } else { "local" };
            let ckw = if *is_const { " const" } else { "" };
            let _ = write!(out, "{}{} {}", kw, ckw, names.join(", "));
            if !initializers.is_empty() {
                out.push_str(" = ");
                write_expr_list(out, initializers);
            }
            out.push('\n');
        }
        HirStmtKind::Assign { target, value, .. } => {
            write_lvalue(out, target);
            out.push_str(" = ");
            write_expr(out, value);
            out.push('\n');
        }
        HirStmtKind::AssignMany { targets, values } => {
            for (i, t) in targets.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_lvalue(out, t);
            }
            out.push_str(" = ");
            write_expr_list(out, values);
            out.push('\n');
        }
        HirStmtKind::Increment { target, amount } => {
            write_lvalue(out, target);
            let _ = writeln!(out, " += {}", amount);
        }
        HirStmtKind::Function {
            name,
            params,
            return_type,
            body,
            ..
        } => {
            let pnames: Vec<String> = params
                .iter()
                .map(|p| {
                    if p.variadic {
                        "...".to_string()
                    } else if let Some(t) = &p.type_name {
                        format!("{}: {}", p.name, t)
                    } else {
                        p.name.clone()
                    }
                })
                .collect();
            let ret = return_type
                .as_deref()
                .map(|r| format!(" -> {}", r))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "fn {}({}){}",
                name.as_deref().unwrap_or("<anon>"),
                pnames.join(", "),
                ret
            );
            for s in body {
                write_stmt(out, s, depth + 1);
            }
        }
        HirStmtKind::Block(body) => {
            out.push_str("block\n");
            for s in body {
                write_stmt(out, s, depth + 1);
            }
        }
        HirStmtKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            out.push_str("if ");
            write_expr(out, condition);
            out.push('\n');
            for s in then_branch {
                write_stmt(out, s, depth + 1);
            }
            if let Some(else_b) = else_branch {
                indent(out, depth);
                out.push_str("else\n");
                for s in else_b {
                    write_stmt(out, s, depth + 1);
                }
            }
        }
        HirStmtKind::For { vars, source, body } => {
            let _ = write!(out, "for {} in ", vars.join(", "));
            write_expr(out, source);
            out.push('\n');
            for s in body {
                write_stmt(out, s, depth + 1);
            }
        }
        HirStmtKind::NumericFor {
            var,
            start,
            end,
            step,
            body,
        } => {
            let _ = write!(out, "for {} = ", var);
            write_expr(out, start);
            out.push_str(", ");
            write_expr(out, end);
            out.push_str(", ");
            write_expr(out, step);
            out.push('\n');
            for s in body {
                write_stmt(out, s, depth + 1);
            }
        }
        HirStmtKind::While { condition, body } => {
            out.push_str("while ");
            write_expr(out, condition);
            out.push('\n');
            for s in body {
                write_stmt(out, s, depth + 1);
            }
        }
        HirStmtKind::Repeat { body, condition } => {
            out.push_str("repeat\n");
            for s in body {
                write_stmt(out, s, depth + 1);
            }
            indent(out, depth);
            out.push_str("until ");
            write_expr(out, condition);
            out.push('\n');
        }
        HirStmtKind::Return(vals) => {
            out.push_str("return");
            if !vals.is_empty() {
                out.push(' ');
                write_expr_list(out, vals);
            }
            out.push('\n');
        }
        HirStmtKind::Break => out.push_str("break\n"),
        HirStmtKind::Continue => out.push_str("continue\n"),
        HirStmtKind::Goto(lbl) => {
            let _ = writeln!(out, "goto {}", lbl);
        }
        HirStmtKind::Label(lbl) => {
            let _ = writeln!(out, "::{}::", lbl);
        }
        HirStmtKind::Expr(expr) => {
            write_expr(out, expr);
            out.push('\n');
        }
    }
}

fn write_lvalue(out: &mut String, lv: &HirLValue) {
    match lv {
        HirLValue::Variable(name) => out.push_str(name),
        HirLValue::Member { object, field } => {
            write_expr(out, object);
            let _ = write!(out, ".{}", field);
        }
        HirLValue::Index { object, index } => {
            write_expr(out, object);
            out.push('[');
            write_expr(out, index);
            out.push(']');
        }
    }
}

fn write_expr_list(out: &mut String, exprs: &[HirExpr]) {
    for (i, e) in exprs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_expr(out, e);
    }
}

fn write_expr(out: &mut String, expr: &HirExpr) {
    match &expr.kind {
        HirExprKind::Literal(lit) => {
            let _ = write!(out, "{}", lit);
        }
        HirExprKind::Variable(name) => out.push_str(name),
        HirExprKind::Vararg => out.push_str("..."),
        HirExprKind::Member { object, field } => {
            write_expr(out, object);
            let _ = write!(out, ".{}", field);
        }
        HirExprKind::Index { object, index } => {
            write_expr(out, object);
            out.push('[');
            write_expr(out, index);
            out.push(']');
        }
        HirExprKind::Call { callee, args } => {
            write_expr(out, callee);
            out.push('(');
            write_expr_list(out, args);
            out.push(')');
        }
        HirExprKind::MethodCall {
            object,
            method,
            args,
        } => {
            write_expr(out, object);
            let _ = write!(out, ":{}(", method);
            write_expr_list(out, args);
            out.push(')');
        }
        HirExprKind::Function { params, .. } => {
            let pnames: Vec<_> = params
                .iter()
                .map(|p| if p.variadic { "..." } else { &p.name })
                .collect();
            let _ = write!(out, "fn({}) {{ ... }}", pnames.join(", "));
        }
        HirExprKind::Unary { op, expr } => {
            let _ = write!(out, "({:?} ", op);
            write_expr(out, expr);
            out.push(')');
        }
        HirExprKind::Binary { left, op, right } => {
            out.push('(');
            write_expr(out, left);
            let _ = write!(out, " {:?} ", op);
            write_expr(out, right);
            out.push(')');
        }
        HirExprKind::Table(entries) => {
            out.push('{');
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                if let Some(comp) = &entry.computed {
                    out.push('[');
                    write_expr(out, comp);
                    out.push_str("] = ");
                } else if let Some(k) = &entry.key {
                    let _ = write!(out, "{} = ", k);
                }
                write_expr(out, &entry.value);
            }
            out.push('}');
        }
    }
}
