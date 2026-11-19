//! Algebraic identities and strength reduction on CFG instructions.
//!
//! Only applies rewrites that preserve Neyuki's dynamic runtime semantics:
//! - arithmetic / bitwise identities require the non-constant operand to be
//!   statically known as an integer (or numeric constant/range) so table
//!   metamethods (`__add`, `__mul`, etc.) or float `-0.0`/`NaN` edge cases are
//!   never silently bypassed;
//! - string concatenation identities require the operand to be statically known
//!   as a `string`.

use std::collections::HashMap;

use num_bigint::BigInt;
use num_traits::{One, Zero};

use crate::compiler::ir::block::ControlFlowGraph;
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::types::{IrBinaryOp, IrConstant, IrUnaryOp, IrVar};
use crate::compiler::value_tracking::{TrackedValue, eval_binop, eval_unop};
use crate::sema::types::NeyukiType;

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
    let mut changed = false;

    for block in &mut cfg.blocks {
        let mut state: HashMap<IrVar, TrackedValue> = HashMap::new();
        let mut origins: HashMap<IrVar, UnOpOrigin> = HashMap::new();
        let mut out = Vec::with_capacity(block.instructions.len());

        for inst in block.instructions.drain(..) {
            match inst {
                IrInst::LoadConst { dst, ref val } => {
                    state.insert(dst, TrackedValue::Const(val.clone()));
                    origins.remove(&dst);
                    out.push(inst);
                }
                IrInst::LoadNil { dst } => {
                    state.insert(dst, TrackedValue::Const(IrConstant::Nil));
                    origins.remove(&dst);
                    out.push(inst);
                }
                IrInst::Move { dst, src } => {
                    let val = state.get(&src).cloned().unwrap_or(TrackedValue::Bottom);
                    state.insert(dst, val);
                    if let Some(orig) = origins.get(&src).cloned() {
                        origins.insert(dst, orig);
                    } else {
                        origins.remove(&dst);
                    }
                    out.push(inst);
                }
                IrInst::BinOp { dst, op, lhs, rhs } => {
                    let l_val = state.get(&lhs).cloned().unwrap_or(TrackedValue::Bottom);
                    let r_val = state.get(&rhs).cloned().unwrap_or(TrackedValue::Bottom);
                    let res_val = eval_binop(op, &l_val, &r_val);

                    if !captured.contains(&dst)
                        && !captured.contains(&lhs)
                        && !captured.contains(&rhs)
                        && let Some(rewritten) = rewrite_binop(dst, op, lhs, rhs, &l_val, &r_val)
                    {
                        changed = true;
                        match &rewritten {
                            IrInst::Move { src, .. } => {
                                let v = state.get(src).cloned().unwrap_or(res_val);
                                state.insert(dst, v);
                            }
                            IrInst::LoadConst { val, .. } => {
                                state.insert(dst, TrackedValue::Const(val.clone()));
                            }
                            _ => {
                                state.insert(dst, res_val);
                            }
                        }
                        origins.remove(&dst);
                        out.push(rewritten);
                    } else {
                        state.insert(dst, res_val);
                        origins.remove(&dst);
                        out.push(IrInst::BinOp { dst, op, lhs, rhs });
                    }
                }
                IrInst::UnOp { dst, op, src } => {
                    let s_val = state.get(&src).cloned().unwrap_or(TrackedValue::Bottom);
                    let res_val = eval_unop(op, &s_val);
                    let double_cancel = if !captured.contains(&dst) && !captured.contains(&src) {
                        match origins.get(&src) {
                            Some(UnOpOrigin::Unary(IrUnaryOp::Not, inner))
                                if op == IrUnaryOp::Not
                                    && !captured.contains(inner)
                                    && is_known_bool(
                                        state.get(inner).unwrap_or(&TrackedValue::Bottom),
                                    ) =>
                            {
                                Some(*inner)
                            }
                            Some(UnOpOrigin::Unary(IrUnaryOp::Neg, inner))
                                if op == IrUnaryOp::Neg
                                    && !captured.contains(inner)
                                    && is_known_number(
                                        state.get(inner).unwrap_or(&TrackedValue::Bottom),
                                    ) =>
                            {
                                Some(*inner)
                            }
                            Some(UnOpOrigin::Unary(IrUnaryOp::BitNot, inner))
                                if op == IrUnaryOp::BitNot
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
                        state.insert(dst, v);
                        origins.remove(&dst);
                        out.push(IrInst::Move { dst, src: inner });
                    } else {
                        state.insert(dst, res_val);
                        origins.insert(dst, UnOpOrigin::Unary(op, src));
                        out.push(IrInst::UnOp { dst, op, src });
                    }
                }
                IrInst::Call { .. } | IrInst::Spread { .. } => {
                    state.retain(|var, _| !captured.contains(var));
                    origins.retain(|var, _| !captured.contains(var));
                    for d in inst.def_vars() {
                        state.remove(&d);
                        origins.remove(&d);
                        origins.retain(|_, orig| match orig {
                            UnOpOrigin::Unary(_, inner) => *inner != d,
                        });
                    }
                    out.push(inst);
                }
                other => {
                    for d in other.def_vars() {
                        state.remove(&d);
                        origins.remove(&d);
                        origins.retain(|_, orig| match orig {
                            UnOpOrigin::Unary(_, inner) => *inner != d,
                        });
                    }
                    out.push(other);
                }
            }
        }

        block.instructions = out;
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
