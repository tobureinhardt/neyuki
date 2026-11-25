//! Common subexpression elimination for pure instructions.

use crate::compiler::ir::block::{ControlFlowGraph, IrModule};
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::types::{IrBinaryOp, IrUnaryOp, IrVar};
use crate::vm::hash::{FxHashMap, new_map};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ExpressionKey {
    BinOp(IrBinaryOp, IrVar, IrVar),
    UnOp(IrUnaryOp, IrVar),
    Upval(u8),
}

fn canonical_binop_key(op: IrBinaryOp, lhs: IrVar, rhs: IrVar) -> ExpressionKey {
    match op {
        IrBinaryOp::Add
        | IrBinaryOp::Mul
        | IrBinaryOp::Eq
        | IrBinaryOp::Ne
        | IrBinaryOp::BitAnd
        | IrBinaryOp::BitOr
        | IrBinaryOp::BitXor => {
            if lhs.0 <= rhs.0 {
                ExpressionKey::BinOp(op, lhs, rhs)
            } else {
                ExpressionKey::BinOp(op, rhs, lhs)
            }
        }
        _ => ExpressionKey::BinOp(op, lhs, rhs),
    }
}

pub fn common_subexpression_elimination(module: &mut IrModule) {
    cse_slice(&mut module.main.instructions);
}

pub fn common_subexpression_elimination_cfg(cfg: &mut ControlFlowGraph) -> bool {
    let mut changed = false;
    for block in &mut cfg.blocks {
        if cse_slice(&mut block.instructions) {
            changed = true;
        }
    }
    changed
}

/// Operators whose result depends only on their operand values. `a .. b` on
/// a table consults state that a store or a call can change while the operand
/// variables stay the same, so it is never shared.
fn is_cse_candidate(op: IrBinaryOp) -> bool {
    !matches!(op, IrBinaryOp::Concat | IrBinaryOp::Coalesce)
}

/// Whether an instruction may run arbitrary code or change memory that an
/// earlier expression could have depended on (through a metamethod).
fn has_side_effects(inst: &IrInst) -> bool {
    matches!(
        inst,
        IrInst::Call { .. }
            | IrInst::Spread { .. }
            | IrInst::TForCall { .. }
            | IrInst::SetTable { .. }
            | IrInst::GetTable { .. }
            | IrInst::AppendArray { .. }
            | IrInst::SetGlobal { .. }
    )
}

fn cse_slice(instructions: &mut [IrInst]) -> bool {
    let mut expr_map: FxHashMap<ExpressionKey, IrVar> = new_map();
    let mut modified = false;

    for inst in instructions.iter_mut() {
        if has_side_effects(inst) {
            expr_map.clear();
        }
        match inst {
            IrInst::SetUpval { index, src } => {
                expr_map.insert(ExpressionKey::Upval(*index), *src);
            }
            IrInst::GetUpval { dst, index } => {
                let key = ExpressionKey::Upval(*index);
                if let Some(&existing_var) = expr_map.get(&key) {
                    if existing_var != *dst {
                        *inst = IrInst::Move {
                            dst: *dst,
                            src: existing_var,
                        };
                        modified = true;
                    }
                } else {
                    let d = *dst;
                    invalidate_expressions(&mut expr_map, d);
                    expr_map.insert(key, d);
                    continue;
                }
            }
            IrInst::BinOp { dst, op, lhs, rhs } if is_cse_candidate(*op) => {
                let key = canonical_binop_key(*op, *lhs, *rhs);
                if let Some(&existing_var) = expr_map.get(&key) {
                    if existing_var != *dst {
                        *inst = IrInst::Move {
                            dst: *dst,
                            src: existing_var,
                        };
                        modified = true;
                    }
                } else {
                    let d = *dst;
                    invalidate_expressions(&mut expr_map, d);
                    expr_map.insert(key, d);
                    continue;
                }
            }
            // `#t` reads the table, which anything in between may have grown.
            IrInst::UnOp { dst, op, src } if *op != IrUnaryOp::Len => {
                let key = ExpressionKey::UnOp(*op, *src);
                if let Some(&existing_var) = expr_map.get(&key) {
                    if existing_var != *dst {
                        *inst = IrInst::Move {
                            dst: *dst,
                            src: existing_var,
                        };
                        modified = true;
                    }
                } else {
                    let d = *dst;
                    invalidate_expressions(&mut expr_map, d);
                    expr_map.insert(key, d);
                    continue;
                }
            }
            _ => {}
        }

        // Any instruction that modifies a variable invalidates expressions using or defining it
        if !expr_map.is_empty() {
            inst.for_each_def(|def| {
                invalidate_expressions(&mut expr_map, def);
            });
        }
    }

    modified
}

fn invalidate_expressions(map: &mut FxHashMap<ExpressionKey, IrVar>, modified_var: IrVar) {
    map.retain(|key, &mut target_var| {
        if target_var == modified_var {
            return false;
        }
        match key {
            ExpressionKey::BinOp(_, lhs, rhs) => *lhs != modified_var && *rhs != modified_var,
            ExpressionKey::UnOp(_, src) => *src != modified_var,
            ExpressionKey::Upval(_) => true,
        }
    });
}
