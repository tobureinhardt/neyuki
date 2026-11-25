//! Algebraic identities and strength reduction on CFG instructions.
//!
//! Only applies rewrites that preserve Neyuki's dynamic runtime semantics:
//! - arithmetic / bitwise identities require the non-constant operand to be
//!   statically known as an integer (or numeric constant/range) so table
//!   metamethods (`__add`, `__mul`, etc.) or float `-0.0`/`NaN` edge cases are
//!   never silently bypassed;
//! - string concatenation identities require the operand to be statically known
//!   as a `string`.

use num_bigint::BigInt;
use num_traits::{One, Zero};

use crate::compiler::ir::block::ControlFlowGraph;
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::types::{IrBinaryOp, IrConstant, IrUnaryOp, IrVar};
use crate::compiler::value_tracking::{TrackedValue, eval_binop, eval_unop};
use crate::sema::types::NeyukiType;
use crate::vm::hash::{FxHashMap, FxHashSet, new_map};

fn is_known_int(v: &TrackedValue) -> bool {
    matches!(
        v,
        TrackedValue::Const(IrConstant::Int(_))
            | TrackedValue::Range { .. }
            | TrackedValue::KnownType(NeyukiType::Int)
    )
}

fn is_known_number(v: &TrackedValue) -> bool {
    is_known_int(v)
        || matches!(
            v,
            TrackedValue::Const(IrConstant::Float(_))
                | TrackedValue::KnownType(NeyukiType::Float | NeyukiType::Number)
        )
}

fn is_known_bool(v: &TrackedValue) -> bool {
    matches!(
        v,
        TrackedValue::Const(IrConstant::Bool(_)) | TrackedValue::KnownType(NeyukiType::Boolean)
    )
}

fn is_known_string(v: &TrackedValue) -> bool {
    matches!(
        v,
        TrackedValue::Const(IrConstant::String(_)) | TrackedValue::KnownType(NeyukiType::String)
    )
}

fn as_int_const(v: &TrackedValue) -> Option<&BigInt> {
    match v {
        TrackedValue::Const(IrConstant::Int(n)) => Some(n),
        _ => None,
    }
}

fn is_zero(v: &TrackedValue) -> bool {
    as_int_const(v).is_some_and(Zero::is_zero)
}

fn is_one(v: &TrackedValue) -> bool {
    as_int_const(v).is_some_and(One::is_one)
}

fn is_neg_one(v: &TrackedValue) -> bool {
    as_int_const(v).is_some_and(|n| *n == BigInt::from(-1))
}

fn is_empty_string(v: &TrackedValue) -> bool {
    matches!(v, TrackedValue::Const(IrConstant::String(s)) if s.is_empty())
}

#[derive(Clone)]
enum UnOpOrigin {
    Unary(IrUnaryOp, IrVar),
}

fn rewrite_binop(
    dst: IrVar,
    op: IrBinaryOp,
    lhs: IrVar,
    rhs: IrVar,
    l_val: &TrackedValue,
    r_val: &TrackedValue,
) -> Option<IrInst> {
    match op {
        IrBinaryOp::Add => {
            if is_zero(r_val) && is_known_int(l_val) {
                return Some(IrInst::Move { dst, src: lhs });
            }
            if is_zero(l_val) && is_known_int(r_val) {
                return Some(IrInst::Move { dst, src: rhs });
            }
        }
        IrBinaryOp::Sub => {
            if is_zero(r_val) && is_known_int(l_val) {
                return Some(IrInst::Move { dst, src: lhs });
            }
            if is_zero(l_val) && is_known_int(r_val) {
                return Some(IrInst::UnOp {
                    dst,
                    op: IrUnaryOp::Neg,
                    src: rhs,
                });
            }
            if lhs == rhs && is_known_int(l_val) {
                return Some(IrInst::LoadConst {
                    dst,
                    val: IrConstant::Int(BigInt::zero()),
                });
            }
        }
        IrBinaryOp::Mul => {
            if (is_zero(r_val) && is_known_int(l_val)) || (is_zero(l_val) && is_known_int(r_val)) {
                return Some(IrInst::LoadConst {
                    dst,
                    val: IrConstant::Int(BigInt::zero()),
                });
            }
            if is_one(r_val) && is_known_int(l_val) {
                return Some(IrInst::Move { dst, src: lhs });
            }
            if is_one(l_val) && is_known_int(r_val) {
                return Some(IrInst::Move { dst, src: rhs });
            }
            if is_neg_one(r_val) && is_known_int(l_val) {
                return Some(IrInst::UnOp {
                    dst,
                    op: IrUnaryOp::Neg,
                    src: lhs,
                });
            }
            if is_neg_one(l_val) && is_known_int(r_val) {
                return Some(IrInst::UnOp {
                    dst,
                    op: IrUnaryOp::Neg,
                    src: rhs,
                });
            }
        }
        IrBinaryOp::IDiv => {
            if is_one(r_val) && is_known_int(l_val) {
                return Some(IrInst::Move { dst, src: lhs });
            }
            if is_neg_one(r_val) && is_known_int(l_val) {
                return Some(IrInst::UnOp {
                    dst,
                    op: IrUnaryOp::Neg,
                    src: lhs,
                });
            }
        }
        IrBinaryOp::Mod => {
            if (is_one(r_val) || is_neg_one(r_val)) && is_known_int(l_val) {
                return Some(IrInst::LoadConst {
                    dst,
                    val: IrConstant::Int(BigInt::zero()),
                });
            }
        }
        IrBinaryOp::BitAnd => {
            if (is_zero(r_val) && is_known_int(l_val)) || (is_zero(l_val) && is_known_int(r_val)) {
                return Some(IrInst::LoadConst {
                    dst,
                    val: IrConstant::Int(BigInt::zero()),
                });
            }
            if is_neg_one(r_val) && is_known_int(l_val) {
                return Some(IrInst::Move { dst, src: lhs });
            }
            if is_neg_one(l_val) && is_known_int(r_val) {
                return Some(IrInst::Move { dst, src: rhs });
            }
            if lhs == rhs && is_known_int(l_val) {
                return Some(IrInst::Move { dst, src: lhs });
            }
        }
        IrBinaryOp::BitOr => {
            if is_zero(r_val) && is_known_int(l_val) {
                return Some(IrInst::Move { dst, src: lhs });
            }
            if is_zero(l_val) && is_known_int(r_val) {
                return Some(IrInst::Move { dst, src: rhs });
            }
            if lhs == rhs && is_known_int(l_val) {
                return Some(IrInst::Move { dst, src: lhs });
            }
        }
        IrBinaryOp::BitXor => {
            if is_zero(r_val) && is_known_int(l_val) {
                return Some(IrInst::Move { dst, src: lhs });
            }
            if is_zero(l_val) && is_known_int(r_val) {
                return Some(IrInst::Move { dst, src: rhs });
            }
            if lhs == rhs && is_known_int(l_val) {
                return Some(IrInst::LoadConst {
                    dst,
                    val: IrConstant::Int(BigInt::zero()),
                });
            }
        }
        IrBinaryOp::Shl | IrBinaryOp::Shr | IrBinaryOp::LShl | IrBinaryOp::LShr => {
            if is_zero(r_val) && is_known_int(l_val) {
                return Some(IrInst::Move { dst, src: lhs });
            }
        }
        IrBinaryOp::Concat => {
            if is_empty_string(r_val) && is_known_string(l_val) {
                return Some(IrInst::Move { dst, src: lhs });
            }
            if is_empty_string(l_val) && is_known_string(r_val) {
                return Some(IrInst::Move { dst, src: rhs });
            }
        }
        IrBinaryOp::Eq
            if lhs == rhs
                && (is_known_int(l_val) || is_known_string(l_val) || is_known_bool(l_val)) =>
        {
            return Some(IrInst::LoadConst {
                dst,
                val: IrConstant::Bool(true),
            });
        }
        IrBinaryOp::Ne
            if lhs == rhs
                && (is_known_int(l_val) || is_known_string(l_val) || is_known_bool(l_val)) =>
        {
            return Some(IrInst::LoadConst {
                dst,
                val: IrConstant::Bool(false),
            });
        }
        IrBinaryOp::Lt | IrBinaryOp::Gt if lhs == rhs && is_known_int(l_val) => {
            return Some(IrInst::LoadConst {
                dst,
                val: IrConstant::Bool(false),
            });
        }
        IrBinaryOp::Le | IrBinaryOp::Ge if lhs == rhs && is_known_int(l_val) => {
            return Some(IrInst::LoadConst {
                dst,
                val: IrConstant::Bool(true),
            });
        }
        _ => {}
    }
    None
}

/// Performs intra-block algebraic simplification and strength reduction on `cfg`.
pub fn strength_reduction_cfg(cfg: &mut ControlFlowGraph) -> bool {
    let captured = super::captured_vars(cfg);
    strength_reduction_cfg_with(cfg, &captured)
}

pub fn strength_reduction_cfg_with(
    cfg: &mut ControlFlowGraph,
    captured: &FxHashSet<IrVar>,
) -> bool {
    let mut changed = false;
    let mut state: FxHashMap<IrVar, TrackedValue> = new_map();
    let mut origins: FxHashMap<IrVar, UnOpOrigin> = new_map();

    for block in &mut cfg.blocks {
        state.clear();
        origins.clear();

        for inst in &mut block.instructions {
            match inst {
                IrInst::LoadConst { dst, val } => {
                    state.insert(*dst, TrackedValue::Const(val.clone()));
                    if !origins.is_empty() {
                        origins.remove(dst);
                    }
                }
                IrInst::LoadNil { dst } => {
                    state.insert(*dst, TrackedValue::Const(IrConstant::Nil));
                    if !origins.is_empty() {
                        origins.remove(dst);
                    }
                }
                IrInst::Move { dst, src } => {
                    let dst_v = *dst;
                    let src_v = *src;
                    let val = state.get(&src_v).cloned().unwrap_or(TrackedValue::Bottom);
                    state.insert(dst_v, val);
                    if let Some(orig) = origins.get(&src_v).cloned() {
                        origins.insert(dst_v, orig);
                    } else if !origins.is_empty() {
                        origins.remove(&dst_v);
                    }
                }
                IrInst::BinOp { dst, op, lhs, rhs } => {
                    let dst_v = *dst;
                    let op_v = *op;
                    let lhs_v = *lhs;
                    let rhs_v = *rhs;
                    let l_val = if captured.contains(&lhs_v) {
                        &TrackedValue::Bottom
                    } else {
                        state.get(&lhs_v).unwrap_or(&TrackedValue::Bottom)
                    };
                    let r_val = if captured.contains(&rhs_v) {
                        &TrackedValue::Bottom
                    } else {
                        state.get(&rhs_v).unwrap_or(&TrackedValue::Bottom)
                    };
                    let res_val = eval_binop(op_v, l_val, r_val);

                    if !captured.contains(&dst_v)
                        && !captured.contains(&lhs_v)
                        && !captured.contains(&rhs_v)
                        && let Some(rewritten) =
                            rewrite_binop(dst_v, op_v, lhs_v, rhs_v, l_val, r_val)
                    {
                        changed = true;
                        match &rewritten {
                            IrInst::Move { src, .. } => {
                                let v = state.get(src).cloned().unwrap_or(res_val);
                                state.insert(dst_v, v);
                            }
                            IrInst::LoadConst { val, .. } => {
                                state.insert(dst_v, TrackedValue::Const(val.clone()));
                            }
                            _ => {
                                state.insert(dst_v, res_val);
                            }
                        }
                        if !origins.is_empty() {
                            origins.remove(&dst_v);
                        }
                        *inst = rewritten;
                    } else {
                        if !captured.is_empty() {
                            state.retain(|var, _| !captured.contains(var));
                            if !origins.is_empty() {
                                origins.retain(|var, _| !captured.contains(var));
                            }
                        }
                        state.insert(dst_v, res_val);
                        if !origins.is_empty() {
                            origins.remove(&dst_v);
                        }
                    }
                }
                IrInst::UnOp { dst, op, src } => {
                    let dst_v = *dst;
                    let op_v = *op;
                    let src_v = *src;
                    let s_val = if captured.contains(&src_v) {
                        &TrackedValue::Bottom
                    } else {
                        state.get(&src_v).unwrap_or(&TrackedValue::Bottom)
                    };
                    let res_val = eval_unop(op_v, s_val);
                    let double_cancel = if !captured.contains(&dst_v) && !captured.contains(&src_v)
                    {
                        match origins.get(&src_v) {
                            Some(UnOpOrigin::Unary(IrUnaryOp::Not, inner))
                                if op_v == IrUnaryOp::Not
                                    && !captured.contains(inner)
                                    && is_known_bool(
                                        state.get(inner).unwrap_or(&TrackedValue::Bottom),
                                    ) =>
                            {
                                Some(*inner)
                            }
                            Some(UnOpOrigin::Unary(IrUnaryOp::Neg, inner))
                                if op_v == IrUnaryOp::Neg
                                    && !captured.contains(inner)
                                    && is_known_number(
                                        state.get(inner).unwrap_or(&TrackedValue::Bottom),
                                    ) =>
                            {
                                Some(*inner)
                            }
                            Some(UnOpOrigin::Unary(IrUnaryOp::BitNot, inner))
                                if op_v == IrUnaryOp::BitNot
                                    && !captured.contains(inner)
                                    && is_known_int(
                                        state.get(inner).unwrap_or(&TrackedValue::Bottom),
                                    ) =>
                            {
                                Some(*inner)
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };

                    if let Some(inner) = double_cancel {
                        changed = true;
                        let v = state.get(&inner).cloned().unwrap_or(res_val);
                        state.insert(dst_v, v);
                        origins.remove(&dst_v);
                        *inst = IrInst::Move {
                            dst: dst_v,
                            src: inner,
                        };
                    } else {
                        if op_v != IrUnaryOp::Not && !captured.is_empty() {
                            state.retain(|var, _| !captured.contains(var));
                            if !origins.is_empty() {
                                origins.retain(|var, _| !captured.contains(var));
                            }
                        }
                        state.insert(dst_v, res_val);
                        origins.insert(dst_v, UnOpOrigin::Unary(op_v, src_v));
                    }
                }
                IrInst::Call { .. }
                | IrInst::Spread { .. }
                | IrInst::TForCall { .. }
                | IrInst::GetTable { .. }
                | IrInst::SetTable { .. }
                | IrInst::AppendArray { .. } => {
                    if !captured.is_empty() {
                        state.retain(|var, _| !captured.contains(var));
                        if !origins.is_empty() {
                            origins.retain(|var, _| !captured.contains(var));
                        }
                    }
                    inst.for_each_def(|d| {
                        state.remove(&d);
                        if !origins.is_empty() {
                            origins.remove(&d);
                            origins.retain(|_, orig| match orig {
                                UnOpOrigin::Unary(_, inner) => *inner != d,
                            });
                        }
                    });
                }
                other => {
                    other.for_each_def(|d| {
                        state.remove(&d);
                        if !origins.is_empty() {
                            origins.remove(&d);
                            origins.retain(|_, orig| match orig {
                                UnOpOrigin::Unary(_, inner) => *inner != d,
                            });
                        }
                    });
                }
            }
        }
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::ir::block::BasicBlock;
    use crate::compiler::ir::types::IrLabel;

    #[test]
    fn test_strength_reduce_algebraic_int_identities() {
        let mut cfg = ControlFlowGraph::new(IrLabel(0));
        let mut b0 = BasicBlock::new(IrLabel(0));

        let v0 = IrVar(0);
        let v1 = IrVar(1);
        let v2 = IrVar(2);
        let v3 = IrVar(3);
        let v4 = IrVar(4);

        b0.add_instruction(IrInst::LoadConst {
            dst: v0,
            val: IrConstant::Int(BigInt::from(7)),
        });
        b0.add_instruction(IrInst::LoadConst {
            dst: v1,
            val: IrConstant::Int(BigInt::from(0)),
        });
        // v2 = v0 & v1 -> v2 = 0
        b0.add_instruction(IrInst::BinOp {
            dst: v2,
            op: IrBinaryOp::BitAnd,
            lhs: v0,
            rhs: v1,
        });
        // v3 = v2 + v0 -> 0 + v0 -> Move(v0)
        b0.add_instruction(IrInst::BinOp {
            dst: v3,
            op: IrBinaryOp::Add,
            lhs: v2,
            rhs: v0,
        });
        // v4 = v3 - v3 -> 0
        b0.add_instruction(IrInst::BinOp {
            dst: v4,
            op: IrBinaryOp::Sub,
            lhs: v3,
            rhs: v3,
        });
        b0.add_instruction(IrInst::Return(vec![v4]));
        cfg.blocks.push(b0);

        assert!(strength_reduction_cfg(&mut cfg));
        assert!(matches!(
            cfg.blocks[0].instructions[2],
            IrInst::LoadConst {
                dst,
                val: IrConstant::Int(ref n)
            } if dst == v2 && n.is_zero()
        ));
        assert!(matches!(
            cfg.blocks[0].instructions[3],
            IrInst::Move { dst, src } if dst == v3 && src == v0
        ));
        assert!(matches!(
            cfg.blocks[0].instructions[4],
            IrInst::LoadConst {
                dst,
                val: IrConstant::Int(ref n)
            } if dst == v4 && n.is_zero()
        ));
    }
}
