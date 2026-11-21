//! AST -> HIR lowering and syntactic desugaring.

use std::collections::HashMap;

use num_bigint::BigInt;

use crate::ast::literal::Literal;
use crate::ast::node_id::NodeId;
use crate::ast::op::BinOp;
use crate::ast::pattern::AssignTarget;
use crate::ast::{Expr, InterpPart, Stmt, TableEntry};
use crate::compiler::OptLevel;
use crate::compiler::hir::desugar::optimize_hir;
use crate::compiler::hir::nodes::{
    HirExpr, HirExprKind, HirId, HirLValue, HirModule, HirStmt, HirStmtKind, HirTableEntry,
};

struct HirLowerer<'a> {
    lines: Option<&'a HashMap<NodeId, u32>>,
    next_id: u32,
}

impl<'a> HirLowerer<'a> {
    fn new(lines: Option<&'a HashMap<NodeId, u32>>) -> Self {
        Self { lines, next_id: 0 }
    }

    fn alloc_id(&mut self) -> HirId {
        let id = HirId(self.next_id);
        self.next_id += 1;
        id
    }

    fn line_of(&self, id: NodeId) -> u32 {
        self.lines
            .and_then(|map| map.get(&id).copied())
            .unwrap_or(0)
    }

    fn lower_block(&mut self, stmts: &[Stmt]) -> Vec<HirStmt> {
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            if let Some(hir_stmt) = self.lower_stmt(stmt) {
                out.push(hir_stmt);
            }
        }
        out
    }

    fn lower_stmt(&mut self, stmt: &Stmt) -> Option<HirStmt> {
        let source_id = stmt.node_id();
        let line = self.line_of(source_id);
        let kind = match stmt {
            // Type declarations exist for sema/tooling and are erased in HIR.
            Stmt::TypeAlias { .. } => return None,
            Stmt::Local {
                name,
                is_const,
                is_global,
                type_name,
                initializer,
                ..
            } => HirStmtKind::Local {
                name: name.clone(),
                is_const: *is_const,
                is_global: *is_global,
                type_name: type_name.clone(),
                initializer: initializer.as_ref().map(|e| self.lower_expr(e)),
            },
            Stmt::LocalMany {
                names,
                is_const,
                is_global,
                initializers,
                ..
            } => HirStmtKind::LocalMany {
                names: names.clone(),
                is_const: *is_const,
                is_global: *is_global,
                initializers: initializers.iter().map(|e| self.lower_expr(e)).collect(),
            },
            Stmt::Assign {
                target,
                value,
                is_const,
                ..
            } => HirStmtKind::Assign {
                target: self.lower_lvalue(target),
                value: self.lower_expr(value),
                is_const: *is_const,
            },
            Stmt::AssignMany {
                targets, values, ..
            } => HirStmtKind::AssignMany {
                targets: targets.iter().map(|t| self.lower_lvalue(t)).collect(),
                values: values.iter().map(|v| self.lower_expr(v)).collect(),
            },
            Stmt::Increment { target, amount, .. } => match target {
                AssignTarget::Variable(name) => {
                    let var_expr = self.make_expr(source_id, HirExprKind::Variable(name.clone()));
                    let delta = self.make_expr(
                        source_id,
                        HirExprKind::Literal(Literal::Int(BigInt::from(*amount))),
                    );
                    let sum = self.make_expr(
                        source_id,
                        HirExprKind::Binary {
                            left: Box::new(var_expr),
                            op: BinOp::Add,
                            right: Box::new(delta),
                        },
                    );
                    HirStmtKind::Assign {
                        target: HirLValue::Variable(name.clone()),
                        value: sum,
                        is_const: false,
                    }
                }
                _ => HirStmtKind::Increment {
                    target: self.lower_lvalue(target),
                    amount: *amount,
                },
            },
            Stmt::Function {
                name,
                is_const,
                is_global,
                params,
                return_type,
                body,
                ..
            } => {
                if let Some(full_name) = name
                    && let Some((path, field)) = full_name.rsplit_once('.')
                {
                    let mut segments = path.split('.');
                    let root = segments
                        .next()
                        .expect("split yields at least one segment")
                        .to_string();
                    let mut obj = self.make_expr(source_id, HirExprKind::Variable(root));
                    for seg in segments {
                        obj = self.make_expr(
                            source_id,
                            HirExprKind::Member {
                                object: Box::new(obj),
                                field: seg.to_string(),
                            },
                        );
                    }
                    let lowered_body = self.lower_block(body);
                    let closure = self.make_expr(
                        source_id,
                        HirExprKind::Function {
                            params: params.clone(),
                            body: lowered_body,
                        },
                    );
                    HirStmtKind::Assign {
                        target: HirLValue::Member {
                            object: Box::new(obj),
                            field: field.to_string(),
                        },
                        value: closure,
                        is_const: *is_const,
                    }
                } else {
                    HirStmtKind::Function {
                        name: name.clone(),
                        is_const: *is_const,
                        is_global: *is_global,
                        params: params.clone(),
                        return_type: return_type.clone(),
                        body: self.lower_block(body),
                    }
                }
            }
            Stmt::If { data, .. } => {
                let mut current_else = data
                    .else_branch
                    .as_ref()
                    .map(|stmts| self.lower_block(stmts));
                for (elif_cond, elif_body) in data.else_if_branches.iter().rev() {
                    let cond_id = elif_cond.node_id();
                    let cond_line = self.line_of(cond_id);
                    let elif_stmt = HirStmt {
                        hir_id: self.alloc_id(),
                        source_id: cond_id,
                        line: cond_line,
                        kind: HirStmtKind::If {
                            condition: self.lower_expr(elif_cond),
                            then_branch: self.lower_block(elif_body),
                            else_branch: current_else,
                        },
                    };
                    current_else = Some(vec![elif_stmt]);
                }
                HirStmtKind::If {
                    condition: self.lower_expr(&data.condition),
                    then_branch: self.lower_block(&data.then_branch),
                    else_branch: current_else,
                }
            }
            Stmt::For {
                vars, source, body, ..
            } => HirStmtKind::For {
                vars: vars.clone(),
                source: self.lower_expr(source),
                body: self.lower_block(body),
            },
            Stmt::NumericFor { data, .. } => {
                let step = match &data.step {
                    Some(s) => self.lower_expr(s),
                    None => self.make_expr(
                        source_id,
                        HirExprKind::Literal(Literal::Int(BigInt::from(1))),
                    ),
                };
                HirStmtKind::NumericFor {
                    var: data.var.clone(),
                    start: self.lower_expr(&data.start),
                    end: self.lower_expr(&data.end),
                    step,
                    body: self.lower_block(&data.body),
                }
            }
            Stmt::While {
                condition, body, ..
            } => HirStmtKind::While {
                condition: self.lower_expr(condition),
                body: self.lower_block(body),
            },
            Stmt::Repeat {
                body, condition, ..
            } => HirStmtKind::Repeat {
                body: self.lower_block(body),
                condition: self.lower_expr(condition),
            },
            Stmt::Return { values, .. } => {
                HirStmtKind::Return(values.iter().map(|v| self.lower_expr(v)).collect())
            }
            Stmt::Break { .. } => HirStmtKind::Break,
            Stmt::Continue { .. } => HirStmtKind::Continue,
            Stmt::Goto { label, .. } => HirStmtKind::Goto(label.clone()),
            Stmt::Label { name, .. } => HirStmtKind::Label(name.clone()),
            Stmt::Expr { expr, .. } => HirStmtKind::Expr(self.lower_expr(expr)),
        };

        Some(HirStmt {
            hir_id: self.alloc_id(),
            source_id,
            line,
            kind,
        })
    }

    fn lower_lvalue(&mut self, target: &AssignTarget) -> HirLValue {
        match target {
            AssignTarget::Variable(name) => HirLValue::Variable(name.clone()),
            AssignTarget::Member { object, field } => HirLValue::Member {
                object: Box::new(self.lower_expr(object)),
                field: field.clone(),
            },
            AssignTarget::Index { object, index } => HirLValue::Index {
                object: Box::new(self.lower_expr(object)),
                index: Box::new(self.lower_expr(index)),
            },
        }
    }

    fn make_expr(&mut self, source_id: NodeId, kind: HirExprKind) -> HirExpr {
        HirExpr {
            hir_id: self.alloc_id(),
            source_id,
            kind,
        }
    }

    fn lower_expr(&mut self, expr: &Expr) -> HirExpr {
        let source_id = expr.node_id();
        let kind = match expr {
            Expr::Literal { value, .. } => HirExprKind::Literal(value.clone()),
            Expr::Interp { parts, .. } => return self.lower_interp(parts, source_id),
            Expr::Variable { name, .. } => HirExprKind::Variable(name.clone()),
            Expr::Vararg { .. } => HirExprKind::Vararg,
            Expr::Member { object, field, .. } => HirExprKind::Member {
                object: Box::new(self.lower_expr(object)),
                field: field.clone(),
            },
            Expr::Index { object, index, .. } => HirExprKind::Index {
                object: Box::new(self.lower_expr(object)),
                index: Box::new(self.lower_expr(index)),
            },
            Expr::Call { callee, args, .. } => HirExprKind::Call {
                callee: Box::new(self.lower_expr(callee)),
                args: args.iter().map(|a| self.lower_expr(a)).collect(),
            },
            Expr::MethodCall {
                object,
                method,
                args,
                ..
            } => HirExprKind::MethodCall {
                object: Box::new(self.lower_expr(object)),
                method: method.clone(),
                args: args.iter().map(|a| self.lower_expr(a)).collect(),
            },
            Expr::Function { params, body, .. } => HirExprKind::Function {
                params: params.clone(),
                body: self.lower_block(body),
            },
            Expr::Unary { op, expr, .. } => HirExprKind::Unary {
                op: *op,
                expr: Box::new(self.lower_expr(expr)),
            },
            Expr::Binary {
                left, op, right, ..
            } => HirExprKind::Binary {
                left: Box::new(self.lower_expr(left)),
                op: *op,
                right: Box::new(self.lower_expr(right)),
            },
            Expr::Table { entries, .. } => {
                HirExprKind::Table(entries.iter().map(|e| self.lower_table_entry(e)).collect())
            }
        };
        self.make_expr(source_id, kind)
    }

    fn lower_table_entry(&mut self, entry: &TableEntry) -> HirTableEntry {
        HirTableEntry {
            key: entry.key.clone(),
            computed: entry
                .computed
                .as_ref()
                .map(|c| Box::new(self.lower_expr(c))),
            value: self.lower_expr(&entry.value),
        }
    }

    /// Desugars string interpolation into canonical `Concat` binary chains.
    /// When the first segment is a dynamic expression, prepends `""` so a
    /// standalone `{x}` still stringifies via `..` concatenation.
    fn lower_interp(&mut self, parts: &[InterpPart], source_id: NodeId) -> HirExpr {
        if parts.is_empty() {
            return self.make_expr(
                source_id,
                HirExprKind::Literal(Literal::String(String::new())),
            );
        }

        let mut acc: Option<HirExpr> = None;
        if matches!(parts.first(), Some(InterpPart::Expr(_))) {
            acc = Some(self.make_expr(
                source_id,
                HirExprKind::Literal(Literal::String(String::new())),
            ));
        }

        for part in parts {
            let item = match part {
                InterpPart::Literal(s) => {
                    self.make_expr(source_id, HirExprKind::Literal(Literal::String(s.clone())))
                }
                InterpPart::Expr(e) => self.lower_expr(e),
            };
            acc = Some(match acc {
                Some(prev) => self.make_expr(
                    source_id,
                    HirExprKind::Binary {
                        left: Box::new(prev),
                        op: BinOp::Concat,
                        right: Box::new(item),
                    },
                ),
                None => item,
            });
        }

        acc.expect("non-empty interp parts always yield an expression")
    }
}

/// Lowers AST statements into canonical HIR and runs HIR normalization passes
/// appropriate for `opt_level`.
pub fn lower_ast(
    stmts: &[Stmt],
    lines: Option<&HashMap<NodeId, u32>>,
    opt_level: OptLevel,
) -> HirModule {
    let mut lowerer = HirLowerer::new(lines);
    let mut module = HirModule {
        items: lowerer.lower_block(stmts),
    };
    optimize_hir(&mut module, opt_level);
    module
}
