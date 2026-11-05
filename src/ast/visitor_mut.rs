//! A visitor that edits the syntax tree in place.

use crate::ast::expr::{Expr, InterpPart, TableEntry};
use crate::ast::stmt::Stmt;

pub trait AstVisitorMut {
    fn visit_stmt_mut(&mut self, stmt: &mut Stmt) {
        walk_stmt_mut(self, stmt);
    }

    fn visit_expr_mut(&mut self, expr: &mut Expr) {
        walk_expr_mut(self, expr);
    }
}

pub fn walk_stmt_mut<V: AstVisitorMut + ?Sized>(visitor: &mut V, stmt: &mut Stmt) {
    match stmt {
        Stmt::Local { initializer, .. } => {
            if let Some(init) = initializer {
                visitor.visit_expr_mut(init);
            }
        }
        Stmt::LocalMany { initializers, .. } => {
            for init in initializers {
                visitor.visit_expr_mut(init);
            }
        }
        Stmt::Assign { target, value, .. } => {
            walk_assign_target_mut(visitor, target);
            visitor.visit_expr_mut(value);
        }
        Stmt::AssignMany {
            targets, values, ..
        } => {
            for t in targets {
                walk_assign_target_mut(visitor, t);
            }
            for v in values {
                visitor.visit_expr_mut(v);
            }
        }
        Stmt::Increment { target, .. } => {
            walk_assign_target_mut(visitor, target);
        }
        Stmt::Function { body, .. } => {
            for s in body {
                visitor.visit_stmt_mut(s);
            }
        }
        Stmt::If { data, .. } => {
            visitor.visit_expr_mut(&mut data.condition);
            for s in &mut data.then_branch {
                visitor.visit_stmt_mut(s);
            }
            for (cond, branch) in &mut data.else_if_branches {
                visitor.visit_expr_mut(cond);
                for s in branch {
                    visitor.visit_stmt_mut(s);
                }
            }
            if let Some(eb) = &mut data.else_branch {
                for s in eb {
                    visitor.visit_stmt_mut(s);
                }
            }
        }
        Stmt::For { source, body, .. } => {
            visitor.visit_expr_mut(source);
            for s in body {
                visitor.visit_stmt_mut(s);
            }
        }
        Stmt::NumericFor { data, .. } => {
            visitor.visit_expr_mut(&mut data.start);
            visitor.visit_expr_mut(&mut data.end);
            if let Some(st) = &mut data.step {
                visitor.visit_expr_mut(st);
            }
            for s in &mut data.body {
                visitor.visit_stmt_mut(s);
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            visitor.visit_expr_mut(condition);
            for s in body {
                visitor.visit_stmt_mut(s);
            }
        }
        Stmt::Repeat {
            body, condition, ..
        } => {
            for s in body {
                visitor.visit_stmt_mut(s);
            }
            visitor.visit_expr_mut(condition);
        }
        Stmt::Return { values, .. } => {
            for e in values {
                visitor.visit_expr_mut(e);
            }
        }
        Stmt::Expr { expr, .. } => {
            visitor.visit_expr_mut(expr);
        }
        Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Goto { .. }
        | Stmt::Label { .. }
        | Stmt::TypeAlias { .. } => {}
    }
}

pub fn walk_assign_target_mut<V: AstVisitorMut + ?Sized>(
    visitor: &mut V,
    target: &mut crate::ast::pattern::AssignTarget,
) {
    match target {
        crate::ast::pattern::AssignTarget::Variable(_) => {}
        crate::ast::pattern::AssignTarget::Member { object, .. } => {
            visitor.visit_expr_mut(object);
        }
        crate::ast::pattern::AssignTarget::Index { object, index } => {
            visitor.visit_expr_mut(object);
            visitor.visit_expr_mut(index);
        }
    }
}

pub fn walk_expr_mut<V: AstVisitorMut + ?Sized>(visitor: &mut V, expr: &mut Expr) {
    match expr {
        Expr::Interp { parts, .. } => {
            for part in parts {
                if let InterpPart::Expr(e) = part {
                    visitor.visit_expr_mut(e);
                }
            }
        }
        Expr::Member { object, .. } => {
            visitor.visit_expr_mut(object);
        }
        Expr::Index { object, index, .. } => {
            visitor.visit_expr_mut(object);
            visitor.visit_expr_mut(index);
        }
        Expr::Call { callee, args, .. } => {
            visitor.visit_expr_mut(callee);
            for arg in args {
                visitor.visit_expr_mut(arg);
            }
        }
        Expr::MethodCall { object, args, .. } => {
            visitor.visit_expr_mut(object);
            for arg in args {
                visitor.visit_expr_mut(arg);
            }
        }
        Expr::Function { body, .. } => {
            for s in body {
                visitor.visit_stmt_mut(s);
            }
        }
        Expr::Unary { expr: inner, .. } => {
            visitor.visit_expr_mut(inner);
        }
        Expr::Binary { left, right, .. } => {
            visitor.visit_expr_mut(left);
            visitor.visit_expr_mut(right);
        }
        Expr::Table { entries, .. } => {
            for TableEntry {
                computed, value, ..
            } in entries
            {
                if let Some(key) = computed {
                    visitor.visit_expr_mut(key);
                }
                visitor.visit_expr_mut(value);
            }
        }
        Expr::Literal { .. } | Expr::Variable { .. } | Expr::Vararg { .. } => {}
    }
}

#[derive(Default, Debug, Clone)]
pub struct AstNormalizer {
    pub mutated_count: usize,
}

impl AstVisitorMut for AstNormalizer {
    fn visit_stmt_mut(&mut self, stmt: &mut Stmt) {
        if let Stmt::If { data, .. } = stmt
            && data.else_branch.as_ref().is_some_and(|eb| eb.is_empty())
        {
            data.else_branch = None;
            self.mutated_count += 1;
        }
        walk_stmt_mut(self, stmt);
    }

    fn visit_expr_mut(&mut self, expr: &mut Expr) {
        if let Expr::Interp { parts, .. } = expr {
            let mut merged: Vec<InterpPart> = Vec::new();
            for p in parts.drain(..) {
                if let (InterpPart::Literal(s), Some(InterpPart::Literal(prev))) =
                    (&p, merged.last_mut())
                {
                    prev.push_str(s);
                    self.mutated_count += 1;
                    continue;
                }
                merged.push(p);
            }
            *parts = merged;
        }
        walk_expr_mut(self, expr);
    }
}

pub fn normalize_ast(stmts: &mut [Stmt]) -> usize {
    let mut normalizer = AstNormalizer::default();
    for stmt in stmts {
        normalizer.visit_stmt_mut(stmt);
    }
    normalizer.mutated_count
}
