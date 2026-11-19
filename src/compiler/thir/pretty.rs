//! Human-readable THIR printer (`neyuki dump-thir`).

use std::fmt::Write;

use crate::compiler::thir::nodes::{
    BindingId, BindingKind, ThirExpr, ThirExprKind, ThirLValue, ThirModule, ThirStmt, ThirStmtKind,
};

/// Formats a [`ThirModule`] with resolved binding IDs and static types.
pub fn dump_thir(module: &ThirModule) -> String {
    let mut out = String::from("=== THIR ===\n");
    if !module.bindings.is_empty() {
        out.push_str("-- bindings --\n");
        for b in &module.bindings {
            let kind = match b.kind {
                BindingKind::Local => "local",
                BindingKind::Param => "param",
                BindingKind::Upvalue => "upval",
                BindingKind::Global => "global",
            };
            let c = if b.is_const { " const" } else { "" };
            let _ = writeln!(
                out,
                "  b#{}: {}{} {} : {}",
                b.id.0,
                kind,
                c,
                b.name,
                b.ty.display_name()
            );
        }
        out.push_str("-- body --\n");
    }
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

fn fmt_binding(b: Option<BindingId>) -> String {
    b.map(|id| format!("b#{}", id.0))
        .unwrap_or_else(|| "global".to_string())
}

fn write_stmt(out: &mut String, stmt: &ThirStmt, depth: usize) {
    indent(out, depth);
    let _ = write!(out, "[thir#{}:L{}] ", stmt.thir_id.0, stmt.line);
    match &stmt.kind {
        ThirStmtKind::Local {
            binding,
            name,
            ty,
            initializer,
            ..
        } => {
            let _ = write!(
                out,
                "let b#{} ({}) : {}",
                binding.0,
                name,
                ty.display_name()
            );
            if let Some(init) = initializer {
                out.push_str(" = ");
                write_expr(out, init);
            }
            out.push('\n');
        }
        ThirStmtKind::LocalMany {
            bindings,
            names,
            initializers,
            ..
        } => {
            let parts: Vec<String> = bindings
                .iter()
                .zip(names.iter())
                .map(|(b, n)| format!("b#{}({})", b.0, n))
                .collect();
            let _ = write!(out, "let {}", parts.join(", "));
            if !initializers.is_empty() {
                out.push_str(" = ");
                write_expr_list(out, initializers);
            }
            out.push('\n');
        }
        ThirStmtKind::Assign { target, value } => {
            write_lvalue(out, target);
            out.push_str(" = ");
            write_expr(out, value);
            out.push('\n');
        }
        ThirStmtKind::AssignMany { targets, values } => {
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
        ThirStmtKind::Increment { target, amount } => {
            write_lvalue(out, target);
            let _ = writeln!(out, " += {}", amount);
        }
        ThirStmtKind::Function {
            binding,
            name,
            params,
            return_type,
            captures,
            body,
        } => {
            let pnames: Vec<String> = params
                .iter()
                .map(|p| format!("{}:{}", p.name, p.ty.display_name()))
                .collect();
            let caps: Vec<String> = captures.iter().map(|c| format!("b#{}", c.0)).collect();
            let _ = writeln!(
                out,
                "fn {} [{}]({}) -> {} captures=[{}]",
                name.as_deref().unwrap_or("<anon>"),
                fmt_binding(*binding),
                pnames.join(", "),
                return_type.display_name(),
                caps.join(",")
            );
            for s in body {
                write_stmt(out, s, depth + 1);
            }
        }
        ThirStmtKind::Block(body) => {
            out.push_str("block\n");
            for s in body {
                write_stmt(out, s, depth + 1);
            }
        }
        ThirStmtKind::If {
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
        ThirStmtKind::For {
            vars, source, body, ..
        } => {
            let _ = write!(out, "for {} in ", vars.join(", "));
            write_expr(out, source);
            out.push('\n');
            for s in body {
                write_stmt(out, s, depth + 1);
            }
        }
        ThirStmtKind::NumericFor(data) => {
            let _ = write!(out, "for b#{}({}) = ", data.binding.0, data.var);
            write_expr(out, &data.start);
            out.push_str(", ");
            write_expr(out, &data.end);
            out.push_str(", ");
            write_expr(out, &data.step);
            out.push('\n');
            for s in &data.body {
                write_stmt(out, s, depth + 1);
            }
        }
        ThirStmtKind::While { condition, body } => {
            out.push_str("while ");
            write_expr(out, condition);
            out.push('\n');
            for s in body {
                write_stmt(out, s, depth + 1);
            }
        }
        ThirStmtKind::Repeat { body, condition } => {
            out.push_str("repeat\n");
            for s in body {
                write_stmt(out, s, depth + 1);
            }
            indent(out, depth);
            out.push_str("until ");
            write_expr(out, condition);
            out.push('\n');
        }
        ThirStmtKind::Return(vals) => {
            out.push_str("return");
            if !vals.is_empty() {
                out.push(' ');
                write_expr_list(out, vals);
            }
            out.push('\n');
        }
        ThirStmtKind::Break => out.push_str("break\n"),
        ThirStmtKind::Continue => out.push_str("continue\n"),
        ThirStmtKind::Goto(lbl) => {
            let _ = writeln!(out, "goto {}", lbl);
        }
        ThirStmtKind::Label(lbl) => {
            let _ = writeln!(out, "::{}::", lbl);
        }
        ThirStmtKind::Expr(e) => {
            write_expr(out, e);
            out.push('\n');
        }
    }
}

fn write_lvalue(out: &mut String, lv: &ThirLValue) {
    match lv {
        ThirLValue::Variable { name, binding } => {
            let _ = write!(out, "{}[{}]", name, fmt_binding(*binding));
        }
        ThirLValue::Member { object, field } => {
            write_expr(out, object);
            let _ = write!(out, ".{}", field);
        }
        ThirLValue::Index { object, index } => {
            write_expr(out, object);
            out.push('[');
            write_expr(out, index);
            out.push(']');
        }
    }
}

fn write_expr_list(out: &mut String, exprs: &[ThirExpr]) {
    for (i, e) in exprs.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_expr(out, e);
    }
}

fn write_expr(out: &mut String, expr: &ThirExpr) {
    out.push('(');
    match &expr.kind {
        ThirExprKind::Literal(lit) => {
            let _ = write!(out, "{}", lit);
        }
        ThirExprKind::Variable { name, binding } => {
            let _ = write!(out, "{}[{}]", name, fmt_binding(*binding));
        }
        ThirExprKind::Vararg => out.push_str("..."),
        ThirExprKind::Member {
            object,
            field,
            is_module_import,
        } => {
            write_expr(out, object);
            let tag = if *is_module_import { "@import" } else { "" };
            let _ = write!(out, ".{}{}", field, tag);
        }
        ThirExprKind::Index { object, index } => {
            write_expr(out, object);
            out.push('[');
            write_expr(out, index);
            out.push(']');
        }
        ThirExprKind::Call { callee, args } => {
            write_expr(out, callee);
            out.push('(');
            write_expr_list(out, args);
            out.push(')');
        }
        ThirExprKind::MethodCall {
            object,
            method,
            args,
        } => {
            write_expr(out, object);
            let _ = write!(out, ":{}(", method);
            write_expr_list(out, args);
            out.push(')');
        }
        ThirExprKind::Function {
            params,
            return_type,
            captures,
            ..
        } => {
            let _ = write!(
                out,
                "fn({} params -> {}, {} captures)",
                params.len(),
                return_type.display_name(),
                captures.len()
            );
        }
        ThirExprKind::Unary { op, expr: inner } => {
            let _ = write!(out, "{:?} ", op);
            write_expr(out, inner);
        }
        ThirExprKind::Binary { left, op, right } => {
            write_expr(out, left);
            let _ = write!(out, " {:?} ", op);
            write_expr(out, right);
        }
        ThirExprKind::Table(entries) => {
            out.push('{');
            for (i, entry) in entries.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                if let Some(k) = &entry.key {
                    let _ = write!(out, "{} = ", k);
                }
                write_expr(out, &entry.value);
            }
            out.push('}');
        }
    }
    let _ = write!(out, " : {})", expr.ty.display_name());
}
