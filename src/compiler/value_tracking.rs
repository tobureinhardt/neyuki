//! Dataflow over IR tracking what each variable is known to hold.

use num_bigint::BigInt;
use num_traits::ToPrimitive;
use std::collections::VecDeque;

use crate::compiler::ir::block::ControlFlowGraph;
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::types::{IrBinaryOp, IrConstant, IrLabel, IrUnaryOp, IrVar};
use crate::sema::types::NeyukiType;
use crate::vm::hash::{FxHashMap, new_map, new_set};

/// The value lattice for dataflow analysis and abstract interpretation.
#[derive(Clone, Debug, PartialEq)]
pub enum TrackedValue {
    /// Unreached / uninitialized state.
    Top,
    /// Known exact constant.
    Const(IrConstant),
    /// Known integer range bounds [min, max].
    Range { min: i64, max: i64 },
    /// Known to be non-nil.
    NonNil,
    /// Known semantic type.
    KnownType(NeyukiType),
    /// Completely dynamic / unknown value.
    Bottom,
}

impl TrackedValue {
    pub fn is_top(&self) -> bool {
        matches!(self, Self::Top)
    }

    pub fn is_bottom(&self) -> bool {
        matches!(self, Self::Bottom)
    }

    pub fn as_const(&self) -> Option<&IrConstant> {
        match self {
            Self::Const(c) => Some(c),
            _ => None,
        }
    }

    pub fn as_range(&self) -> Option<(i64, i64)> {
        match self {
            Self::Range { min, max } => Some((*min, *max)),
            Self::Const(IrConstant::Int(b)) => b.to_i64().map(|i| (i, i)),
            _ => None,
        }
    }

    pub fn is_truthy(&self) -> Option<bool> {
        match self {
            Self::Const(IrConstant::Nil) | Self::Const(IrConstant::Bool(false)) => Some(false),
            Self::Const(IrConstant::Bool(true))
            | Self::Const(IrConstant::Int(_))
            | Self::Const(IrConstant::Float(_))
            | Self::Const(IrConstant::String(_)) => Some(true),
            Self::Range { .. } => Some(true),
            Self::KnownType(NeyukiType::Number)
            | Self::KnownType(NeyukiType::Int)
            | Self::KnownType(NeyukiType::Float)
            | Self::KnownType(NeyukiType::String)
            | Self::KnownType(NeyukiType::Table)
            | Self::KnownType(NeyukiType::Buffer)
            | Self::KnownType(NeyukiType::Thread) => Some(true),
            _ => None,
        }
    }

    pub fn is_non_nil(&self) -> bool {
        match self {
            Self::Const(IrConstant::Nil) => false,
            Self::Const(_) | Self::Range { .. } | Self::NonNil => true,
            Self::KnownType(t) => !matches!(
                t,
                NeyukiType::Nil | NeyukiType::Optional(_) | NeyukiType::Any
            ),
            _ => false,
        }
    }

    pub fn join(&self, other: &Self) -> Self {
        if self == other {
            return self.clone();
        }
        if self.is_top() {
            return other.clone();
        }
        if other.is_top() {
            return self.clone();
        }
        if self.is_bottom() || other.is_bottom() {
            return Self::Bottom;
        }

        match (self, other) {
            (Self::Const(c1), Self::Const(c2)) => {
                if c1 == c2 {
                    Self::Const(c1.clone())
                } else if let (Some(i1), Some(i2)) = (c1_to_i64(c1), c1_to_i64(c2)) {
                    Self::Range {
                        min: i1.min(i2),
                        max: i1.max(i2),
                    }
                } else {
                    let t1 = constant_type(c1);
                    let t2 = constant_type(c2);
                    Self::KnownType(t1.lub(&t2))
                }
            }
            (Self::Range { min: a1, max: b1 }, Self::Range { min: a2, max: b2 }) => Self::Range {
                min: (*a1).min(*a2),
                max: (*b1).max(*b2),
            },
            (Self::Range { min, max }, Self::Const(c))
            | (Self::Const(c), Self::Range { min, max }) => {
                if let Some(i) = c1_to_i64(c) {
                    Self::Range {
                        min: (*min).min(i),
                        max: (*max).max(i),
                    }
                } else {
                    Self::KnownType(NeyukiType::Number)
                }
            }
            (Self::NonNil, Self::NonNil) => Self::NonNil,
            (Self::NonNil, other) | (other, Self::NonNil) => {
                if other.is_non_nil() {
                    Self::NonNil
                } else {
                    Self::Bottom
                }
            }
            (Self::KnownType(t1), Self::KnownType(t2)) => Self::KnownType(t1.lub(t2)),
            (Self::KnownType(t), Self::Const(c)) | (Self::Const(c), Self::KnownType(t)) => {
                let ct = constant_type(c);
                Self::KnownType(t.lub(&ct))
            }
            _ => Self::Bottom,
        }
    }
}

fn c1_to_i64(c: &IrConstant) -> Option<i64> {
    match c {
        IrConstant::Int(b) => b.to_i64(),
        _ => None,
    }
}

fn constant_type(c: &IrConstant) -> NeyukiType {
    match c {
        IrConstant::Nil => NeyukiType::Nil,
        IrConstant::Bool(_) => NeyukiType::Boolean,
        IrConstant::Int(_) => NeyukiType::Int,
        IrConstant::Float(_) => NeyukiType::Float,
        IrConstant::String(_) => NeyukiType::String,
    }
}

/// Evaluates a binary operation on two TrackedValues.
pub fn eval_binop(op: IrBinaryOp, lhs: &TrackedValue, rhs: &TrackedValue) -> TrackedValue {
    if let (Some(c1), Some(c2)) = (lhs.as_const(), rhs.as_const())
        && let Some(res) = fold_const_binop(op, c1, c2)
    {
        return TrackedValue::Const(res);
    }

    if let (Some((min1, max1)), Some((min2, max2))) = (lhs.as_range(), rhs.as_range()) {
        match op {
            IrBinaryOp::Add => {
                if let (Some(min), Some(max)) = (min1.checked_add(min2), max1.checked_add(max2)) {
                    return TrackedValue::Range { min, max };
                }
            }
            IrBinaryOp::Sub => {
                if let (Some(min), Some(max)) = (min1.checked_sub(max2), max1.checked_sub(min2)) {
                    return TrackedValue::Range { min, max };
                }
            }
            IrBinaryOp::Mul => {
                let p1 = min1.checked_mul(min2);
                let p2 = min1.checked_mul(max2);
                let p3 = max1.checked_mul(min2);
                let p4 = max1.checked_mul(max2);
                if let (Some(a), Some(b), Some(c), Some(d)) = (p1, p2, p3, p4) {
                    let min = a.min(b).min(c).min(d);
                    let max = a.max(b).max(c).max(d);
                    return TrackedValue::Range { min, max };
                }
            }
            IrBinaryOp::Lt => {
                if max1 < min2 {
                    return TrackedValue::Const(IrConstant::Bool(true));
                }
                if min1 >= max2 {
                    return TrackedValue::Const(IrConstant::Bool(false));
                }
            }
            IrBinaryOp::Le => {
                if max1 <= min2 {
                    return TrackedValue::Const(IrConstant::Bool(true));
                }
                if min1 > max2 {
                    return TrackedValue::Const(IrConstant::Bool(false));
                }
            }
            IrBinaryOp::Gt => {
                if min1 > max2 {
                    return TrackedValue::Const(IrConstant::Bool(true));
                }
                if max1 <= min2 {
                    return TrackedValue::Const(IrConstant::Bool(false));
                }
            }
            IrBinaryOp::Ge => {
                if min1 >= max2 {
                    return TrackedValue::Const(IrConstant::Bool(true));
                }
                if max1 < min2 {
                    return TrackedValue::Const(IrConstant::Bool(false));
                }
            }
            IrBinaryOp::Eq => {
                if max1 < min2 || min1 > max2 {
                    return TrackedValue::Const(IrConstant::Bool(false));
                }
                if min1 == max1 && min2 == max2 && min1 == min2 {
                    return TrackedValue::Const(IrConstant::Bool(true));
                }
            }
            IrBinaryOp::Ne => {
                if max1 < min2 || min1 > max2 {
                    return TrackedValue::Const(IrConstant::Bool(true));
                }
                if min1 == max1 && min2 == max2 && min1 == min2 {
                    return TrackedValue::Const(IrConstant::Bool(false));
                }
            }
            _ => {}
        }
    }

    match op {
        IrBinaryOp::Eq
        | IrBinaryOp::Ne
        | IrBinaryOp::Lt
        | IrBinaryOp::Le
        | IrBinaryOp::Gt
        | IrBinaryOp::Ge => TrackedValue::KnownType(NeyukiType::Boolean),
        IrBinaryOp::Concat => TrackedValue::KnownType(NeyukiType::String),
        IrBinaryOp::BitAnd
        | IrBinaryOp::BitOr
        | IrBinaryOp::BitXor
        | IrBinaryOp::Shl
        | IrBinaryOp::Shr
        | IrBinaryOp::LShl
        | IrBinaryOp::LShr
        | IrBinaryOp::IDiv
        | IrBinaryOp::Mod => TrackedValue::KnownType(NeyukiType::Int),
        IrBinaryOp::Div => TrackedValue::KnownType(NeyukiType::Float),
        _ => TrackedValue::Bottom,
    }
}

/// Evaluates a unary operation on a TrackedValue.
pub fn eval_unop(op: IrUnaryOp, src: &TrackedValue) -> TrackedValue {
    if let Some(c) = src.as_const()
        && let Some(res) = fold_const_unop(op, c)
    {
        return TrackedValue::Const(res);
    }

    match op {
        IrUnaryOp::Not => {
            if let Some(truthy) = src.is_truthy() {
                TrackedValue::Const(IrConstant::Bool(!truthy))
            } else {
                TrackedValue::KnownType(NeyukiType::Boolean)
            }
        }
        IrUnaryOp::Neg => {
            if let Some((min, max)) = src.as_range() {
                TrackedValue::Range {
                    min: -max,
                    max: -min,
                }
            } else {
                TrackedValue::KnownType(NeyukiType::Number)
            }
        }
        IrUnaryOp::Len | IrUnaryOp::BitNot => TrackedValue::KnownType(NeyukiType::Int),
    }
}

fn fold_const_binop(op: IrBinaryOp, lhs: &IrConstant, rhs: &IrConstant) -> Option<IrConstant> {
    match (lhs, rhs) {
        (IrConstant::Int(a), IrConstant::Int(b)) => match op {
            IrBinaryOp::Add => Some(IrConstant::Int(a + b)),
            IrBinaryOp::Sub => Some(IrConstant::Int(a - b)),
            IrBinaryOp::Mul => Some(IrConstant::Int(a * b)),
            IrBinaryOp::Eq => Some(IrConstant::Bool(a == b)),
            IrBinaryOp::Ne => Some(IrConstant::Bool(a != b)),
            IrBinaryOp::Lt => Some(IrConstant::Bool(a < b)),
            IrBinaryOp::Le => Some(IrConstant::Bool(a <= b)),
            IrBinaryOp::Gt => Some(IrConstant::Bool(a > b)),
            IrBinaryOp::Ge => Some(IrConstant::Bool(a >= b)),
            IrBinaryOp::BitAnd => Some(IrConstant::Int(a & b)),
            IrBinaryOp::BitOr => Some(IrConstant::Int(a | b)),
            IrBinaryOp::BitXor => Some(IrConstant::Int(a ^ b)),
            _ => None,
        },
        (IrConstant::Float(a), IrConstant::Float(b)) => match op {
            IrBinaryOp::Add => Some(IrConstant::Float(a + b)),
            IrBinaryOp::Sub => Some(IrConstant::Float(a - b)),
            IrBinaryOp::Mul => Some(IrConstant::Float(a * b)),
            IrBinaryOp::Div => Some(IrConstant::Float(a / b)),
            IrBinaryOp::Eq => Some(IrConstant::Bool(a == b)),
            IrBinaryOp::Ne => Some(IrConstant::Bool(a != b)),
            IrBinaryOp::Lt => Some(IrConstant::Bool(a < b)),
            IrBinaryOp::Le => Some(IrConstant::Bool(a <= b)),
            IrBinaryOp::Gt => Some(IrConstant::Bool(a > b)),
            IrBinaryOp::Ge => Some(IrConstant::Bool(a >= b)),
            _ => None,
        },
        (IrConstant::String(a), IrConstant::String(b)) => match op {
            IrBinaryOp::Concat => Some(IrConstant::String(format!("{}{}", a, b))),
            IrBinaryOp::Eq => Some(IrConstant::Bool(a == b)),
            IrBinaryOp::Ne => Some(IrConstant::Bool(a != b)),
            _ => None,
        },
        (IrConstant::Bool(a), IrConstant::Bool(b)) => match op {
            IrBinaryOp::Eq => Some(IrConstant::Bool(a == b)),
            IrBinaryOp::Ne => Some(IrConstant::Bool(a != b)),
            _ => None,
        },
        (IrConstant::Nil, other) => match op {
            IrBinaryOp::Coalesce => Some(other.clone()),
            IrBinaryOp::Eq => Some(IrConstant::Bool(matches!(other, IrConstant::Nil))),
            IrBinaryOp::Ne => Some(IrConstant::Bool(!matches!(other, IrConstant::Nil))),
            _ => None,
        },
        (other, _) => match op {
            IrBinaryOp::Coalesce => Some(other.clone()),
            _ => None,
        },
    }
}

fn fold_const_unop(op: IrUnaryOp, src: &IrConstant) -> Option<IrConstant> {
    match op {
        IrUnaryOp::Not => match src {
            IrConstant::Nil | IrConstant::Bool(false) => Some(IrConstant::Bool(true)),
            _ => Some(IrConstant::Bool(false)),
        },
        IrUnaryOp::Neg => match src {
            IrConstant::Int(i) => Some(IrConstant::Int(-i)),
            IrConstant::Float(f) => Some(IrConstant::Float(-f)),
            _ => None,
        },
        IrUnaryOp::Len => match src {
            IrConstant::String(s) => Some(IrConstant::Int(BigInt::from(s.len()))),
            _ => None,
        },
        IrUnaryOp::BitNot => match src {
            IrConstant::Int(i) => Some(IrConstant::Int(!i)),
            _ => None,
        },
    }
}

/// Abstract state tracking variables throughout basic blocks.
pub struct ValueTracker {
    pub block_in: FxHashMap<IrLabel, FxHashMap<IrVar, TrackedValue>>,
    pub block_out: FxHashMap<IrLabel, FxHashMap<IrVar, TrackedValue>>,
}

impl Default for ValueTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ValueTracker {
    pub fn new() -> Self {
        Self {
            block_in: new_map(),
            block_out: new_map(),
        }
    }

    /// Runs fixed-point dataflow analysis over the ControlFlowGraph.
    pub fn analyze(&mut self, cfg: &ControlFlowGraph) {
        let captured = crate::compiler::ir::opt::captured_vars(cfg);
        let mut worklist = VecDeque::new();
        let mut in_worklist = new_set();

        if let Some(entry) = cfg.entry_block() {
            worklist.push_back(entry.label);
            in_worklist.insert(entry.label);
        }

        while let Some(label) = worklist.pop_front() {
            in_worklist.remove(&label);

            let Some(block) = cfg.find_block(label) else {
                continue;
            };

            // Compute block_in from all predecessors
            let mut state: FxHashMap<IrVar, TrackedValue> = new_map();
            let mut first = true;
            for pred_label in &block.predecessors {
                if let Some(pred_out) = self.block_out.get(pred_label) {
                    if first {
                        state = pred_out.clone();
                        first = false;
                    } else {
                        for (var, val) in pred_out {
                            let curr = state.entry(*var).or_insert(TrackedValue::Top);
                            *curr = curr.join(val);
                        }
                    }
                }
            }

            self.block_in.insert(label, state.clone());

            // Transfer functions for each instruction in block
            for inst in &block.instructions {
                match inst {
                    IrInst::LoadConst { dst, val } => {
                        state.insert(*dst, TrackedValue::Const(val.clone()));
                    }
                    IrInst::LoadNil { dst } => {
                        state.insert(*dst, TrackedValue::Const(IrConstant::Nil));
                    }
                    IrInst::Move { dst, src } => {
                        let val = if captured.contains(src) {
                            TrackedValue::Bottom
                        } else {
                            state.get(src).cloned().unwrap_or(TrackedValue::Bottom)
                        };
                        state.insert(*dst, val);
                    }
                    IrInst::BinOp { dst, op, lhs, rhs } => {
                        let l_val = if captured.contains(lhs) {
                            TrackedValue::Bottom
                        } else {
                            state.get(lhs).cloned().unwrap_or(TrackedValue::Bottom)
                        };
                        let r_val = if captured.contains(rhs) {
                            TrackedValue::Bottom
                        } else {
                            state.get(rhs).cloned().unwrap_or(TrackedValue::Bottom)
                        };
                        let res = eval_binop(*op, &l_val, &r_val);
                        if !matches!(res, TrackedValue::Const(_)) && !captured.is_empty() {
                            state.retain(|var, _| !captured.contains(var));
                        }
                        state.insert(*dst, res);
                    }
                    IrInst::UnOp { dst, op, src } => {
                        let s_val = if captured.contains(src) {
                            TrackedValue::Bottom
                        } else {
                            state.get(src).cloned().unwrap_or(TrackedValue::Bottom)
                        };
                        let res = eval_unop(*op, &s_val);
                        if !matches!(res, TrackedValue::Const(_))
                            && *op != IrUnaryOp::Not
                            && !captured.is_empty()
                        {
                            state.retain(|var, _| !captured.contains(var));
                        }
                        state.insert(*dst, res);
                    }
                    IrInst::NewTable { dst, .. } => {
                        state.insert(*dst, TrackedValue::NonNil);
                    }
                    IrInst::Phi { dst, incoming } => {
                        let mut phi_val = TrackedValue::Top;
                        for (from_label, from_var) in incoming {
                            if let Some(pred_out) = self.block_out.get(from_label)
                                && let Some(v) = pred_out.get(from_var)
                            {
                                phi_val = phi_val.join(v);
                            }
                        }
                        state.insert(*dst, phi_val);
                    }
                    IrInst::Call { dsts, .. } => {
                        if !captured.is_empty() {
                            state.retain(|var, _| !captured.contains(var));
                        }
                        for dst in dsts {
                            state.insert(*dst, TrackedValue::Bottom);
                        }
                    }
                    IrInst::Spread { .. }
                    | IrInst::TForCall { .. }
                    | IrInst::GetTable { .. }
                    | IrInst::SetTable { .. }
                    | IrInst::AppendArray { .. } => {
                        if !captured.is_empty() {
                            state.retain(|var, _| !captured.contains(var));
                        }
                        inst.for_each_def(|dst| {
                            state.insert(dst, TrackedValue::Bottom);
                        });
                    }
                    other => {
                        if let Some(dst) = other.def_var() {
                            state.insert(dst, TrackedValue::Bottom);
                        }
                    }
                }
            }

            let changed = match self.block_out.get(&label) {
                Some(prev) => *prev != state,
                None => true,
            };

            if changed {
                self.block_out.insert(label, state);
                for succ in &block.successors {
                    if in_worklist.insert(*succ) {
                        worklist.push_back(*succ);
                    }
                }
            }
        }
    }
}

/// CFG Optimization pass driven by Value Tracking.
/// Propagates constants, performs range-based comparison folding, and optimizes expressions.
pub fn value_tracking_cfg(cfg: &mut ControlFlowGraph) -> bool {
    let captured = crate::compiler::ir::opt::captured_vars(cfg);
    value_tracking_cfg_with(cfg, &captured)
}

pub fn value_tracking_cfg_with(
    cfg: &mut ControlFlowGraph,
    captured: &crate::vm::hash::FxHashSet<IrVar>,
) -> bool {
    let mut changed = false;
    let mut state: FxHashMap<IrVar, TrackedValue> = new_map();

    for block in &mut cfg.blocks {
        state.clear();

        for inst in &mut block.instructions {
            match inst {
                IrInst::LoadConst { dst, val } => {
                    state.insert(*dst, TrackedValue::Const(val.clone()));
                }
                IrInst::LoadNil { dst } => {
                    state.insert(*dst, TrackedValue::Const(IrConstant::Nil));
                }
                IrInst::Move { dst, src } => {
                    let dst_v = *dst;
                    let src_v = *src;
                    let const_opt = if !captured.contains(&src_v)
                        && let Some(TrackedValue::Const(c)) = state.get(&src_v)
                    {
                        Some(c.clone())
                    } else {
                        None
                    };
                    if !captured.contains(&dst_v)
                        && !captured.contains(&src_v)
                        && let Some(c) = const_opt
                    {
                        changed = true;
                        state.insert(dst_v, TrackedValue::Const(c.clone()));
                        *inst = IrInst::LoadConst { dst: dst_v, val: c };
                    } else {
                        let val = if captured.contains(&src_v) {
                            TrackedValue::Bottom
                        } else {
                            state.get(&src_v).cloned().unwrap_or(TrackedValue::Bottom)
                        };
                        state.insert(dst_v, val);
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
                    let res = eval_binop(op_v, l_val, r_val);
                    let const_opt = if let TrackedValue::Const(ref c) = res {
                        Some(c.clone())
                    } else {
                        None
                    };

                    if !captured.contains(&dst_v)
                        && !captured.contains(&lhs_v)
                        && !captured.contains(&rhs_v)
                        && let Some(c) = const_opt
                    {
                        changed = true;
                        state.insert(dst_v, res);
                        *inst = IrInst::LoadConst { dst: dst_v, val: c };
                    } else {
                        if !captured.is_empty() {
                            state.retain(|var, _| !captured.contains(var));
                        }
                        state.insert(dst_v, res);
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
                    let res = eval_unop(op_v, s_val);
                    let const_opt = if let TrackedValue::Const(ref c) = res {
                        Some(c.clone())
                    } else {
                        None
                    };

                    if !captured.contains(&dst_v)
                        && !captured.contains(&src_v)
                        && let Some(c) = const_opt
                    {
                        changed = true;
                        state.insert(dst_v, res);
                        *inst = IrInst::LoadConst { dst: dst_v, val: c };
                    } else {
                        if op_v != IrUnaryOp::Not && !captured.is_empty() {
                            state.retain(|var, _| !captured.contains(var));
                        }
                        state.insert(dst_v, res);
                    }
                }
                IrInst::Call { .. }
                | IrInst::Spread { .. }
                | IrInst::TForCall { .. }
                | IrInst::GetTable { .. }
                | IrInst::SetTable { .. }
                | IrInst::AppendArray { .. } => {
                    // Calls, iterators and table operations (via metamethods) may
                    // execute closures and mutate captured variables.
                    if !captured.is_empty() {
                        state.retain(|var, _| !captured.contains(var));
                    }
                    inst.for_each_def(|d| {
                        state.remove(&d);
                    });
                }
                other => {
                    other.for_each_def(|d| {
                        state.remove(&d);
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
    use crate::compiler::ir::BasicBlock;

    #[test]
    fn test_tracked_value_lattice_join() {
        let v1 = TrackedValue::Const(IrConstant::Int(BigInt::from(10)));
        let v2 = TrackedValue::Const(IrConstant::Int(BigInt::from(20)));
        let joined = v1.join(&v2);
        assert_eq!(joined, TrackedValue::Range { min: 10, max: 20 });

        let v3 = TrackedValue::Const(IrConstant::Int(BigInt::from(15)));
        assert_eq!(joined.join(&v3), TrackedValue::Range { min: 10, max: 20 });

        let v4 = TrackedValue::Const(IrConstant::Int(BigInt::from(25)));
        assert_eq!(joined.join(&v4), TrackedValue::Range { min: 10, max: 25 });
    }

    #[test]
    fn test_eval_binop_range_folding() {
        let r1 = TrackedValue::Range { min: 10, max: 20 };
        let r2 = TrackedValue::Range { min: 30, max: 40 };

        // r1 < r2 is strictly true (20 < 30)
        let lt = eval_binop(IrBinaryOp::Lt, &r1, &r2);
        assert_eq!(lt, TrackedValue::Const(IrConstant::Bool(true)));

        // r1 > r2 is strictly false
        let gt = eval_binop(IrBinaryOp::Gt, &r1, &r2);
        assert_eq!(gt, TrackedValue::Const(IrConstant::Bool(false)));

        // Range arithmetic
        let add = eval_binop(IrBinaryOp::Add, &r1, &r2);
        assert_eq!(add, TrackedValue::Range { min: 40, max: 60 });
    }

    #[test]
    fn test_value_tracking_optimization_cfg() {
        let mut cfg = ControlFlowGraph::new(IrLabel(0));
        let mut b0 = BasicBlock::new(IrLabel(0));

        let v1 = IrVar(1);
        let v2 = IrVar(2);
        let v3 = IrVar(3);
        let v_cond = IrVar(4);

        b0.add_instruction(IrInst::LoadConst {
            dst: v1,
            val: IrConstant::Int(BigInt::from(5)),
        });
        b0.add_instruction(IrInst::LoadConst {
            dst: v2,
            val: IrConstant::Int(BigInt::from(10)),
        });
        b0.add_instruction(IrInst::BinOp {
            dst: v3,
            op: IrBinaryOp::Add,
            lhs: v1,
            rhs: v2,
        });
        b0.add_instruction(IrInst::BinOp {
            dst: v_cond,
            op: IrBinaryOp::Lt,
            lhs: v1,
            rhs: v2,
        });

        cfg.blocks.push(b0);

        let changed = value_tracking_cfg(&mut cfg);
        assert!(changed);

        // v3 was folded to LoadConst(15)
        let b0_opt = cfg.find_block(IrLabel(0)).unwrap();
        assert_eq!(
            b0_opt.instructions[2],
            IrInst::LoadConst {
                dst: v3,
                val: IrConstant::Int(BigInt::from(15)),
            }
        );

        // v_cond was folded to LoadConst(true)
        assert_eq!(
            b0_opt.instructions[3],
            IrInst::LoadConst {
                dst: v_cond,
                val: IrConstant::Bool(true),
            }
        );
    }
}
