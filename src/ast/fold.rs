//! A folder that rebuilds the tree, for pure AST transformations.

use crate::ast::expr::{Expr, InterpPart, TableEntry};
use crate::ast::pattern::AssignTarget;
use crate::ast::stmt::Stmt;

pub trait AstFolder {
    fn fold_stmt(&mut self, stmt: Stmt) -> Stmt {
        walk_fold_stmt(self, stmt)
    }

    fn fold_expr(&mut self, expr: Expr) -> Expr {
        walk_fold_expr(self, expr)
    }
}

pub fn walk_fold_stmt<F: AstFolder + ?Sized>(folder: &mut F, stmt: Stmt) -> Stmt {
    match stmt {
        Stmt::Local {
            name,
            is_const,
            is_global,
            type_name,
            initializer,
            id,
        } => Stmt::Local {
            name,
            is_const,
            is_global,
            type_name,
            initializer: initializer.map(|e| folder.fold_expr(e)),
            id,
        },
        Stmt::LocalMany {
            names,
            is_const,
            is_global,
            initializers,
            id,
        } => Stmt::LocalMany {
            names,
            is_const,
            is_global,
            initializers: initializers
                .into_iter()
                .map(|e| folder.fold_expr(e))
                .collect(),
            id,
        },
        Stmt::Assign {
            target,
            value,
            is_const,
            id,
        } => Stmt::Assign {
            target: fold_assign_target(folder, target),
            value: folder.fold_expr(value),
            is_const,
            id,
        },
        Stmt::AssignMany {
            targets,
            values,
            id,
        } => Stmt::AssignMany {
            targets: targets
                .into_iter()
                .map(|t| fold_assign_target(folder, t))
                .collect(),
            values: values.into_iter().map(|e| folder.fold_expr(e)).collect(),
            id,
        },
        Stmt::Increment { target, amount, id } => Stmt::Increment {
            target: fold_assign_target(folder, target),
            amount,
            id,
        },
        Stmt::Function {
            name,
            is_const,
            params,
            return_type,
            body,
            id,
        } => Stmt::Function {
            name,
            is_const,
            params,
            return_type,
            body: body.into_iter().map(|s| folder.fold_stmt(s)).collect(),
            id,
        },
        Stmt::If { data, id } => Stmt::if_stmt(
            folder.fold_expr(data.condition),
            data.then_branch
                .into_iter()
                .map(|s| folder.fold_stmt(s))
                .collect(),
            data.else_if_branches
                .into_iter()
                .map(|(cond, branch)| {
                    (
                        folder.fold_expr(cond),
                        branch.into_iter().map(|s| folder.fold_stmt(s)).collect(),
                    )
                })
                .collect(),
            data.else_branch
                .map(|eb| eb.into_iter().map(|s| folder.fold_stmt(s)).collect()),
            id,
        ),
        Stmt::For {
            vars,
            source,
            body,
            id,
        } => Stmt::For {
            vars,
            source: folder.fold_expr(source),
            body: body.into_iter().map(|s| folder.fold_stmt(s)).collect(),
            id,
        },
        Stmt::NumericFor { data, id } => Stmt::numeric_for(
            data.var,
            folder.fold_expr(data.start),
            folder.fold_expr(data.end),
            data.step.map(|s| folder.fold_expr(s)),
            data.body.into_iter().map(|s| folder.fold_stmt(s)).collect(),
            id,
        ),
        Stmt::While {
            condition,
            body,
            id,
        } => Stmt::While {
            condition: folder.fold_expr(condition),
            body: body.into_iter().map(|s| folder.fold_stmt(s)).collect(),
            id,
        },
        Stmt::Repeat {
            body,
            condition,
            id,
        } => Stmt::Repeat {
            body: body.into_iter().map(|s| folder.fold_stmt(s)).collect(),
            condition: folder.fold_expr(condition),
            id,
        },
        Stmt::Return { values, id } => Stmt::Return {
            values: values.into_iter().map(|e| folder.fold_expr(e)).collect(),
            id,
        },
        Stmt::Expr { expr, id } => Stmt::Expr {
            expr: folder.fold_expr(expr),
            id,
        },
        Stmt::Break { id } => Stmt::Break { id },
        Stmt::Continue { id } => Stmt::Continue { id },
        Stmt::Goto { label, id } => Stmt::Goto { label, id },
        Stmt::Label { name, id } => Stmt::Label { name, id },
        alias @ Stmt::TypeAlias { .. } => alias,
    }
}

pub fn walk_fold_expr<F: AstFolder + ?Sized>(folder: &mut F, expr: Expr) -> Expr {
    match expr {
        Expr::Interp { parts, id } => Expr::Interp {
            parts: parts
                .into_iter()
                .map(|part| match part {
                    InterpPart::Expr(e) => InterpPart::Expr(folder.fold_expr(e)),
                    literal => literal,
                })
                .collect(),
            id,
        },
        Expr::Member { object, field, id } => Expr::Member {
            object: Box::new(folder.fold_expr(*object)),
            field,
            id,
        },
        Expr::Index { object, index, id } => Expr::Index {
            object: Box::new(folder.fold_expr(*object)),
            index: Box::new(folder.fold_expr(*index)),
            id,
        },
        Expr::Call { callee, args, id } => Expr::Call {
            callee: Box::new(folder.fold_expr(*callee)),
            args: args.into_iter().map(|a| folder.fold_expr(a)).collect(),
            id,
        },
        Expr::MethodCall {
            object,
            method,
            args,
            id,
        } => Expr::MethodCall {
            object: Box::new(folder.fold_expr(*object)),
            method,
            args: args.into_iter().map(|a| folder.fold_expr(a)).collect(),
            id,
        },
        Expr::Function { params, body, id } => Expr::Function {
            params,
            body: body.into_iter().map(|s| folder.fold_stmt(s)).collect(),
            id,
        },
        Expr::Unary {
            op,
            expr: inner,
            id,
        } => Expr::Unary {
            op,
            expr: Box::new(folder.fold_expr(*inner)),
            id,
        },
        Expr::Binary {
            left,
            op,
            right,
            id,
        } => Expr::Binary {
            left: Box::new(folder.fold_expr(*left)),
            op,
            right: Box::new(folder.fold_expr(*right)),
            id,
        },
        Expr::Table { entries, id } => Expr::Table {
            entries: entries
                .into_iter()
                .map(|entry| TableEntry {
                    key: entry.key,
                    computed: entry.computed.map(|k| Box::new(folder.fold_expr(*k))),
                    value: folder.fold_expr(entry.value),
                })
                .collect(),
            id,
        },
        leaf => leaf,
    }
}

pub fn fold_assign_target<F: AstFolder + ?Sized>(
    folder: &mut F,
    target: AssignTarget,
) -> AssignTarget {
    match target {
        AssignTarget::Variable(name) => AssignTarget::Variable(name),
        AssignTarget::Member { object, field } => AssignTarget::Member {
            object: Box::new(folder.fold_expr(*object)),
            field,
        },
        AssignTarget::Index { object, index } => AssignTarget::Index {
            object: Box::new(folder.fold_expr(*object)),
            index: Box::new(folder.fold_expr(*index)),
        },
    }
}
