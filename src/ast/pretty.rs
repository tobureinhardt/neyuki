//! Prints a syntax tree as source or as S-expressions.

use crate::ast::expr::{Expr, InterpPart};
use crate::ast::stmt::Stmt;

pub fn pretty_print_stmt(stmt: &Stmt, indent: usize) -> String {
    let pad = "  ".repeat(indent);
    match stmt {
        Stmt::Local {
            name,
            is_const,
            is_global,
            type_name,
            initializer,
            ..
        } => {
            let kw = match (*is_global, *is_const) {
                (true, true) => "global const",
                (true, false) => "global",
                (false, true) => "const",
                (false, false) => "local",
            };
            let ty = type_name
                .as_ref()
                .map(|t| format!(": {}", t))
                .unwrap_or_default();
            if let Some(init) = initializer {
                format!("{}{} {}{} = {}", pad, kw, name, ty, pretty_print_expr(init))
            } else {
                format!("{}{} {}{}", pad, kw, name, ty)
            }
        }
        Stmt::LocalMany {
            names,
            is_const,
            is_global,
            initializers,
            ..
        } => {
            let kw = match (*is_global, *is_const) {
                (true, true) => "global const",
                (true, false) => "global",
                (false, true) => "const",
                (false, false) => "local",
            };
            let inits = initializers
                .iter()
                .map(pretty_print_expr)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}{} {} = {}", pad, kw, names.join(", "), inits)
        }
        Stmt::Assign {
            target,
            value,
            is_const,
            ..
        } => {
            let kw = if *is_const { "const " } else { "" };
            format!(
                "{}{}{} = {}",
                pad,
                kw,
                pretty_print_assign_target(target),
                pretty_print_expr(value)
            )
        }
        Stmt::AssignMany {
            targets, values, ..
        } => {
            let ts = targets
                .iter()
                .map(pretty_print_assign_target)
                .collect::<Vec<_>>()
                .join(", ");
            let vs = values
                .iter()
                .map(pretty_print_expr)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}{} = {}", pad, ts, vs)
        }
        Stmt::Increment { target, amount, .. } => {
            let op = if *amount >= 0 { "++" } else { "--" };
            format!("{}{}{}", pad, pretty_print_assign_target(target), op)
        }
        Stmt::Function {
            name, params, body, ..
        } => {
            let n = name.as_deref().unwrap_or("anonymous");
            let ps = params
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            let mut out = format!("{}function {}({})\n", pad, n, ps);
            for s in body {
                out.push_str(&pretty_print_stmt(s, indent + 1));
                out.push('\n');
            }
            out.push_str(&format!("{}end", pad));
            out
        }
        Stmt::If { data, .. } => {
            let mut out = format!("{}if {} then\n", pad, pretty_print_expr(&data.condition));
            for s in &data.then_branch {
                out.push_str(&pretty_print_stmt(s, indent + 1));
                out.push('\n');
            }
            for (cond, branch) in &data.else_if_branches {
                out.push_str(&format!("{}elseif {} then\n", pad, pretty_print_expr(cond)));
                for s in branch {
                    out.push_str(&pretty_print_stmt(s, indent + 1));
                    out.push('\n');
                }
            }
            if let Some(eb) = &data.else_branch {
                out.push_str(&format!("{}else\n", pad));
                for s in eb {
                    out.push_str(&pretty_print_stmt(s, indent + 1));
                    out.push('\n');
                }
            }
            out.push_str(&format!("{}end", pad));
            out
        }
        Stmt::While {
            condition, body, ..
        } => {
            let mut out = format!("{}while {} do\n", pad, pretty_print_expr(condition));
            for s in body {
                out.push_str(&pretty_print_stmt(s, indent + 1));
                out.push('\n');
            }
            out.push_str(&format!("{}end", pad));
            out
        }
        Stmt::Repeat {
            body, condition, ..
        } => {
            let mut out = format!("{}repeat\n", pad);
            for s in body {
                out.push_str(&pretty_print_stmt(s, indent + 1));
                out.push('\n');
            }
            out.push_str(&format!("{}until {}", pad, pretty_print_expr(condition)));
            out
        }
        Stmt::For {
            vars, source, body, ..
        } => {
            let mut out = format!(
                "{}for {} in {} do\n",
                pad,
                vars.join(", "),
                pretty_print_expr(source)
            );
            for s in body {
                out.push_str(&pretty_print_stmt(s, indent + 1));
                out.push('\n');
            }
            out.push_str(&format!("{}end", pad));
            out
        }
        Stmt::NumericFor { data, .. } => {
            let st = data
                .step
                .as_ref()
                .map(|e| format!(", {}", pretty_print_expr(e)))
                .unwrap_or_default();
            let mut out = format!(
                "{}for {} = {}, {}{} do\n",
                pad,
                data.var,
                pretty_print_expr(&data.start),
                pretty_print_expr(&data.end),
                st
            );
            for s in &data.body {
                out.push_str(&pretty_print_stmt(s, indent + 1));
                out.push('\n');
            }
            out.push_str(&format!("{}end", pad));
            out
        }
        Stmt::Return { values, .. } => {
            if values.is_empty() {
                format!("{}return", pad)
            } else {
                let es = values
                    .iter()
                    .map(pretty_print_expr)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("{}return {}", pad, es)
            }
        }
        Stmt::Break { .. } => format!("{}break", pad),
        Stmt::Continue { .. } => format!("{}continue", pad),
        Stmt::Goto { label, .. } => format!("{}goto {}", pad, label),
        Stmt::Label { name, .. } => format!("{}::{}::", pad, name),
        Stmt::TypeAlias {
            name, exported, ty, ..
        } => format!(
            "{}{}type {} = {}",
            pad,
            if *exported { "export " } else { "" },
            name,
            ty
        ),
        Stmt::Expr { expr, .. } => format!("{}{}", pad, pretty_print_expr(expr)),
    }
}

pub fn pretty_print_assign_target(target: &crate::ast::pattern::AssignTarget) -> String {
    match target {
        crate::ast::pattern::AssignTarget::Variable(v) => v.clone(),
        crate::ast::pattern::AssignTarget::Member { object, field } => {
            format!("{}.{}", pretty_print_expr(object), field)
        }
        crate::ast::pattern::AssignTarget::Index { object, index } => {
            format!(
                "{}[{}]",
                pretty_print_expr(object),
                pretty_print_expr(index)
            )
        }
    }
}

pub fn pretty_print_expr(expr: &Expr) -> String {
    match expr {
        Expr::Literal { value: lit, .. } => lit.to_string(),
        Expr::Variable { name: v, .. } => v.clone(),
        Expr::Vararg { .. } => "...".to_string(),
        Expr::Interp { parts, .. } => {
            let mut out = String::from("`");
            for part in parts {
                match part {
                    InterpPart::Literal(s) => out.push_str(s),
                    InterpPart::Expr(e) => {
                        out.push_str(&format!("${{{}}}", pretty_print_expr(e)));
                    }
                }
            }
            out.push('`');
            out
        }
        Expr::Member { object, field, .. } => format!("{}.{}", pretty_print_expr(object), field),
        Expr::Index { object, index, .. } => {
            format!(
                "{}[{}]",
                pretty_print_expr(object),
                pretty_print_expr(index)
            )
        }
        Expr::Call { callee, args, .. } => {
            let as_str = args
                .iter()
                .map(pretty_print_expr)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({})", pretty_print_expr(callee), as_str)
        }
        Expr::MethodCall {
            object,
            method,
            args,
            ..
        } => {
            let as_str = args
                .iter()
                .map(pretty_print_expr)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}:{}({})", pretty_print_expr(object), method, as_str)
        }
        Expr::Function { params, .. } => {
            let ps = params
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            format!("function({}) ... end", ps)
        }
        Expr::Unary {
            op, expr: inner, ..
        } => {
            format!("{}{}", op, pretty_print_expr(inner))
        }
        Expr::Binary {
            left, op, right, ..
        } => {
            format!(
                "({} {} {})",
                pretty_print_expr(left),
                op,
                pretty_print_expr(right)
            )
        }
        Expr::Table { entries, .. } => {
            let es = entries
                .iter()
                .map(|e| {
                    if let Some(k) = &e.key {
                        format!("{} = {}", k, pretty_print_expr(&e.value))
                    } else if let Some(k) = &e.computed {
                        format!(
                            "[{}] = {}",
                            pretty_print_expr(k),
                            pretty_print_expr(&e.value)
                        )
                    } else {
                        pretty_print_expr(&e.value)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{{}}}", es)
        }
    }
}

// S-expression dumper for structured debugging
pub fn to_sexpr(expr: &Expr) -> String {
    match expr {
        Expr::Literal { value: lit, .. } => lit.to_string(),
        Expr::Variable { name: v, .. } => v.clone(),
        Expr::Vararg { .. } => "...".to_string(),
        Expr::Unary {
            op, expr: inner, ..
        } => format!("({} {})", op, to_sexpr(inner)),
        Expr::Binary {
            left, op, right, ..
        } => {
            format!("({} {} {})", op, to_sexpr(left), to_sexpr(right))
        }
        Expr::Call { callee, args, .. } => {
            let mut out = format!("(call {}", to_sexpr(callee));
            for a in args {
                out.push_str(&format!(" {}", to_sexpr(a)));
            }
            out.push(')');
            out
        }
        Expr::Table { entries, .. } => {
            let mut out = String::from("(table");
            for e in entries {
                if let Some(k) = &e.key {
                    out.push_str(&format!(" (:{} {})", k, to_sexpr(&e.value)));
                } else if let Some(k) = &e.computed {
                    out.push_str(&format!(" ([{}] {})", to_sexpr(k), to_sexpr(&e.value)));
                } else {
                    out.push_str(&format!(" {}", to_sexpr(&e.value)));
                }
            }
            out.push(')');
            out
        }
        _ => pretty_print_expr(expr),
    }
}
