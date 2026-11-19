//! Entrypoints for lowering AST / HIR into three-address [`IrModule`] via THIR and MIR.

use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::Stmt;
use crate::ast::node_id::NodeId;
use crate::compiler::OptLevel;
use crate::compiler::hir::{HirModule, lower_ast};
use crate::compiler::ir::block::IrModule;
use crate::compiler::mir::{lower_thir_to_mir, mir_to_ir};
use crate::compiler::thir::lower_hir_to_thir;

pub fn ast_to_ir(stmts: &[Stmt]) -> IrModule {
    ast_to_ir_with(stmts, None, false)
}

/// Builds the IR via `AST -> HIR -> THIR -> MIR -> IR` with line markers from
/// `lines` and optional debug `keep_locals`.
pub fn ast_to_ir_with(
    stmts: &[Stmt],
    lines: Option<Rc<HashMap<NodeId, u32>>>,
    keep_locals: bool,
) -> IrModule {
    let hir = lower_ast(stmts, lines.as_deref(), OptLevel::O0);
    hir_to_ir_with(&hir, keep_locals)
}

/// Lowers an [`HirModule`] through THIR and MIR into a three-address [`IrModule`].
pub fn hir_to_ir_with(hir: &HirModule, keep_locals: bool) -> IrModule {
    let thir = lower_hir_to_thir(hir);
    let mir = lower_thir_to_mir(&thir, keep_locals);
    mir_to_ir(&mir)
}
