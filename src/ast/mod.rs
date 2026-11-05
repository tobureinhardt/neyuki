//! The syntax tree and its traversals.

pub mod decl;
pub mod expr;
pub mod fold;
pub mod literal;
pub mod module;
pub mod node_id;
pub mod op;
pub mod pattern;
pub mod pretty;
pub mod span;
pub mod stmt;
pub mod ty;
pub mod visitor;
pub mod visitor_mut;

pub use decl::{Decl, ExportedType, exported_types};
pub use expr::{Expr, InterpPart, Param, TableEntry};
pub use fold::{AstFolder, walk_fold_expr, walk_fold_stmt};
pub use literal::Literal;
pub use module::Module;
pub use node_id::{NodeId, NodeIdGenerator};
pub use op::{AssignOp, BinOp, CompoundOp, UnOp};
pub use pattern::{AssignTarget, SpannedAssignTarget};
pub use pretty::{pretty_print_expr, pretty_print_stmt, to_sexpr};
pub use span::{SourceLocation, Span, SpanPool};
pub use stmt::{IfData, NumericForData, Stmt};
pub use ty::TypeExpr;
pub use visitor::{
    AstSanityChecker, AstVisitor, check_ast_sanity, walk_assign_target, walk_expr, walk_stmt,
};
pub use visitor_mut::{
    AstNormalizer, AstVisitorMut, normalize_ast, walk_assign_target_mut, walk_expr_mut,
    walk_stmt_mut,
};
