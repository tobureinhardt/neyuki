//! A read-only visitor over the syntax tree.

use crate::ast::expr::Expr;
use crate::ast::stmt::Stmt;

pub trait AstVisitor {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        walk_expr(self, expr);
    }
}

pub fn walk_stmt<V: AstVisitor + ?Sized>(visitor: &mut V, stmt: &Stmt) {
    match stmt {
        Stmt::Local { initializer, .. } => {
            if let Some(init) = initializer {
                visitor.visit_expr(init);
            }
        }
        Stmt::LocalMany { initializers, .. } => {
            for init in initializers {
                visitor.visit_expr(init);
            }
        }
        Stmt::Assign { target, value, .. } => {
            walk_assign_target(visitor, target);
            visitor.visit_expr(value);
        }
        Stmt::AssignMany {
            targets, values, ..
        } => {
            for t in targets {
                walk_assign_target(visitor, t);
            }
            for v in values {
                visitor.visit_expr(v);
            }
        }
        Stmt::Increment { target, .. } => {
            walk_assign_target(visitor, target);
        }
        Stmt::Function { body, .. } => {
            for s in body {
                visitor.visit_stmt(s);
            }
        }
        Stmt::If { data, .. } => {
            visitor.visit_expr(&data.condition);
            for s in &data.then_branch {
                visitor.visit_stmt(s);
            }
            for (c, b) in &data.else_if_branches {
                visitor.visit_expr(c);
                for s in b {
                    visitor.visit_stmt(s);
                }
            }
            if let Some(eb) = &data.else_branch {
                for s in eb {
                    visitor.visit_stmt(s);
                }
            }
        }
        Stmt::For { source, body, .. } => {
            visitor.visit_expr(source);
            for s in body {
                visitor.visit_stmt(s);
            }
        }
        Stmt::NumericFor { data, .. } => {
            visitor.visit_expr(&data.start);
            visitor.visit_expr(&data.end);
            if let Some(st) = &data.step {
                visitor.visit_expr(st);
            }
            for s in &data.body {
                visitor.visit_stmt(s);
            }
        }
        Stmt::While {
            condition, body, ..
        } => {
            visitor.visit_expr(condition);
            for s in body {
                visitor.visit_stmt(s);
            }
        }
        Stmt::Repeat {
            body, condition, ..
        } => {
            for s in body {
                visitor.visit_stmt(s);
            }
            visitor.visit_expr(condition);
        }
        Stmt::Return { values, .. } => {
            for e in values {
                visitor.visit_expr(e);
            }
        }
        Stmt::Expr { expr, .. } => {
            visitor.visit_expr(expr);
        }
        Stmt::Break { .. }
        | Stmt::Continue { .. }
        | Stmt::Goto { .. }
        | Stmt::Label { .. }
        | Stmt::TypeAlias { .. } => {}
    }
}

pub fn walk_assign_target<V: AstVisitor + ?Sized>(
    visitor: &mut V,
    target: &crate::ast::pattern::AssignTarget,
) {
    match target {
        crate::ast::pattern::AssignTarget::Variable(_) => {}
        crate::ast::pattern::AssignTarget::Member { object, .. } => {
            visitor.visit_expr(object);
        }
        crate::ast::pattern::AssignTarget::Index { object, index } => {
            visitor.visit_expr(object);
            visitor.visit_expr(index);
        }
    }
}

pub fn walk_expr<V: AstVisitor + ?Sized>(visitor: &mut V, expr: &Expr) {
    match expr {
        Expr::Literal { .. } | Expr::Variable { .. } | Expr::Vararg { .. } => {}
        Expr::Interp { parts, .. } => {
            for p in parts {
                if let crate::ast::expr::InterpPart::Expr(e) = p {
                    visitor.visit_expr(e);
                }
            }
        }
        Expr::Member { object, .. } => {
            visitor.visit_expr(object);
        }
        Expr::Index { object, index, .. } => {
            visitor.visit_expr(object);
            visitor.visit_expr(index);
        }
        Expr::Call { callee, args, .. } => {
            visitor.visit_expr(callee);
            for a in args {
                visitor.visit_expr(a);
            }
        }
        Expr::MethodCall { object, args, .. } => {
            visitor.visit_expr(object);
            for a in args {
                visitor.visit_expr(a);
            }
        }
        Expr::Function { body, .. } => {
            for s in body {
                visitor.visit_stmt(s);
            }
        }
        Expr::Unary { expr, .. } => {
            visitor.visit_expr(expr);
        }
        Expr::Binary { left, right, .. } => {
            visitor.visit_expr(left);
            visitor.visit_expr(right);
        }
        Expr::Table { entries, .. } => {
            for entry in entries {
                if let Some(key) = &entry.computed {
                    visitor.visit_expr(key);
                }
                visitor.visit_expr(&entry.value);
            }
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct AstSanityChecker {
    pub max_depth: usize,
    pub current_depth: usize,
    pub expr_count: usize,
    pub stmt_count: usize,
}

impl AstVisitor for AstSanityChecker {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        self.stmt_count += 1;
        self.current_depth += 1;
        if self.current_depth > self.max_depth {
            self.max_depth = self.current_depth;
        }
        walk_stmt(self, stmt);
        self.current_depth -= 1;
    }

    fn visit_expr(&mut self, expr: &Expr) {
        self.expr_count += 1;
        self.current_depth += 1;
        if self.current_depth > self.max_depth {
            self.max_depth = self.current_depth;
        }
        walk_expr(self, expr);
        self.current_depth -= 1;
    }
}

pub fn check_ast_sanity(stmts: &[Stmt]) -> Result<AstSanityChecker, String> {
    const MAX_AST_DEPTH: usize = 512;
    let mut checker = AstSanityChecker::default();
    for stmt in stmts {
        checker.visit_stmt(stmt);
    }
    if checker.max_depth > MAX_AST_DEPTH {
        return Err(format!("AST depth limit ({}) exceeded", MAX_AST_DEPTH));
    }
    Ok(checker)
}
