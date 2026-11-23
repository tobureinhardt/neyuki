//! MIR-level optimization passes (`-O1`): constant/branch folding, dead temp
//! elimination, jump-trampoline collapsing, and unreachable block pruning.

use std::collections::VecDeque;

use num_bigint::Sign;
use num_traits::{ToPrimitive, Zero};

use crate::ast::op::{BinOp, UnOp};
use crate::compiler::OptLevel;
use crate::compiler::mir::cfg::{
    BasicBlockId, LocalKind, MirBody, MirConst, MirProgram, MirSpreadSource, Operand, Place,
    Rvalue, StatementKind, TerminatorKind,
};
use crate::vm::hash::{FxHashMap, set_with_capacity, with_capacity};
use crate::vm::ops::{MAX_INT_BITS, pow_result_bits};

fn const_truthy(c: &MirConst) -> bool {
    !matches!(c, MirConst::Nil | MirConst::Bool(false))
}

fn eval_mir_unary(op: UnOp, c: &MirConst) -> Option<MirConst> {
    match (op, c) {
        (UnOp::Not, val) => Some(MirConst::Bool(!const_truthy(val))),
        (UnOp::Neg, MirConst::Int(i)) => Some(MirConst::Int(-i)),
        (UnOp::Neg, MirConst::Float(f)) => Some(MirConst::Float(-*f)),
        (UnOp::Len, MirConst::String(s)) => Some(MirConst::Int(s.len().into())),
        (UnOp::BitNot, MirConst::Int(i)) => Some(MirConst::Int(!i)),
        _ => None,
    }
}

fn eval_mir_binary(op: BinOp, lhs: &MirConst, rhs: &MirConst) -> Option<MirConst> {
    match (lhs, rhs) {
        (MirConst::Int(a), MirConst::Int(b)) => {
            let fits = match op {
                BinOp::Add | BinOp::Sub => a.bits().max(b.bits()) < MAX_INT_BITS,
                BinOp::Mul => a.bits() + b.bits() <= MAX_INT_BITS,
                BinOp::Pow => match b.to_u64() {
                    Some(exp) => pow_result_bits(a, exp) <= MAX_INT_BITS,
                    None => false,
                },
                BinOp::Shl | BinOp::Shr => {
                    let widens = (op == BinOp::Shl) == (b.sign() != Sign::Minus);
                    if !widens || a.is_zero() {
                        true
                    } else {
                        a.bits()
                            .saturating_add(b.magnitude().to_u64().unwrap_or(u64::MAX))
                            <= MAX_INT_BITS
                    }
                }
                _ => true,
            };
            if !fits {
                return None;
            }
            match op {
                BinOp::Add => Some(MirConst::Int(a + b)),
                BinOp::Sub => Some(MirConst::Int(a - b)),
                BinOp::Mul => Some(MirConst::Int(a * b)),
                BinOp::IDiv if !b.is_zero() => {
                    Some(MirConst::Int(num_integer::Integer::div_floor(a, b)))
                }
                BinOp::Mod if !b.is_zero() => {
                    Some(MirConst::Int(num_integer::Integer::mod_floor(a, b)))
                }
                BinOp::Div if !b.is_zero() => {
                    let fa = a.to_f64()?;
                    let fb = b.to_f64()?;
                    Some(MirConst::Float(fa / fb))
                }
                BinOp::BitAnd => Some(MirConst::Int(a & b)),
                BinOp::BitOr => Some(MirConst::Int(a | b)),
                BinOp::BitXor => Some(MirConst::Int(a ^ b)),
                BinOp::Eq => Some(MirConst::Bool(a == b)),
                BinOp::Ne => Some(MirConst::Bool(a != b)),
                BinOp::Lt => Some(MirConst::Bool(a < b)),
                BinOp::Le => Some(MirConst::Bool(a <= b)),
                BinOp::Gt => Some(MirConst::Bool(a > b)),
                BinOp::Ge => Some(MirConst::Bool(a >= b)),
                _ => None,
            }
        }
        (MirConst::Float(a), MirConst::Float(b)) => match op {
            BinOp::Add => Some(MirConst::Float(a + b)),
            BinOp::Sub => Some(MirConst::Float(a - b)),
            BinOp::Mul => Some(MirConst::Float(a * b)),
            BinOp::Div if *b != 0.0 => Some(MirConst::Float(a / b)),
            BinOp::Eq => Some(MirConst::Bool(a == b)),
            BinOp::Ne => Some(MirConst::Bool(a != b)),
            BinOp::Lt => Some(MirConst::Bool(a < b)),
            BinOp::Le => Some(MirConst::Bool(a <= b)),
            BinOp::Gt => Some(MirConst::Bool(a > b)),
            BinOp::Ge => Some(MirConst::Bool(a >= b)),
            _ => None,
        },
        (MirConst::String(a), MirConst::String(b)) => match op {
            BinOp::Concat => Some(MirConst::String(format!("{}{}", a, b))),
            BinOp::Eq => Some(MirConst::Bool(a == b)),
            BinOp::Ne => Some(MirConst::Bool(a != b)),
            BinOp::Lt => Some(MirConst::Bool(a < b)),
            BinOp::Le => Some(MirConst::Bool(a <= b)),
            BinOp::Gt => Some(MirConst::Bool(a > b)),
            BinOp::Ge => Some(MirConst::Bool(a >= b)),
            _ => None,
        },
        (MirConst::Bool(a), MirConst::Bool(b)) => match op {
            BinOp::Eq => Some(MirConst::Bool(a == b)),
            BinOp::Ne => Some(MirConst::Bool(a != b)),
            _ => None,
        },
        (MirConst::Nil, MirConst::Nil) => match op {
            BinOp::Eq => Some(MirConst::Bool(true)),
            BinOp::Ne => Some(MirConst::Bool(false)),
            _ => None,
        },
        _ => None,
    }
}

fn push_term_successors(term: &TerminatorKind, out: &mut Vec<BasicBlockId>) {
    out.clear();
    match term {
        TerminatorKind::Goto(bb) => out.push(*bb),
        TerminatorKind::SwitchBool {
            true_bb, false_bb, ..
        } => {
            out.push(*true_bb);
            out.push(*false_bb);
        }
        TerminatorKind::Call { target, .. }
        | TerminatorKind::SpreadCall { target, .. }
        | TerminatorKind::SpreadList { target, .. } => out.push(*target),
        TerminatorKind::ForPrep {
            test_bb, body_bb, ..
        } => {
            out.push(*test_bb);
            out.push(*body_bb);
        }
        TerminatorKind::ForLoop {
            body_bb, exit_bb, ..
        }
        | TerminatorKind::TForStep {
            body_bb, exit_bb, ..
        } => {
            out.push(*body_bb);
            out.push(*exit_bb);
        }
        TerminatorKind::SpreadReturn(_)
        | TerminatorKind::Return(_)
        | TerminatorKind::Unreachable => {}
    }
}

fn remap_term_target(term: &mut TerminatorKind, from: BasicBlockId, to: BasicBlockId) -> bool {
    let mut changed = false;
    let mut replace = |bb: &mut BasicBlockId| {
        if *bb == from {
            *bb = to;
            changed = true;
        }
    };
    match term {
        TerminatorKind::Goto(bb) => replace(bb),
        TerminatorKind::SwitchBool {
            true_bb, false_bb, ..
        } => {
            replace(true_bb);
            replace(false_bb);
        }
        TerminatorKind::Call { target, .. }
        | TerminatorKind::SpreadCall { target, .. }
        | TerminatorKind::SpreadList { target, .. } => replace(target),
        TerminatorKind::ForPrep {
            test_bb, body_bb, ..
        } => {
            replace(test_bb);
            replace(body_bb);
        }
        TerminatorKind::ForLoop {
            body_bb, exit_bb, ..
        }
        | TerminatorKind::TForStep {
            body_bb, exit_bb, ..
        } => {
            replace(body_bb);
            replace(exit_bb);
        }
        TerminatorKind::SpreadReturn(_)
        | TerminatorKind::Return(_)
        | TerminatorKind::Unreachable => {}
    }
    changed
}

fn resolve_const<'a>(op: &'a Operand, consts: &'a [Option<MirConst>]) -> Option<&'a MirConst> {
    match op {
        Operand::Constant(c) => Some(c),
        Operand::Copy(p) | Operand::Move(p) if p.projections.is_empty() => {
            consts.get(p.local.0 as usize).and_then(|c| c.as_ref())
        }
        _ => None,
    }
}

fn fold_constants_and_branches(body: &mut MirBody) -> bool {
    let mut changed = false;
    let num_locals = body.locals.len();
    let mut consts: Vec<Option<MirConst>> = vec![None; num_locals];

    for bb in &mut body.blocks {
        consts.fill(None);
        for stmt in &mut bb.statements {
            let StatementKind::Assign(place, rvalue) = &mut stmt.kind else {
                continue;
            };
            if !place.projections.is_empty() {
                continue;
            }
            let dst_idx = place.local.0 as usize;
            let folded = match rvalue {
                Rvalue::Use(Operand::Constant(c)) => Some(c.clone()),
                Rvalue::Use(Operand::Copy(src) | Operand::Move(src))
                    if src.projections.is_empty() =>
                {
                    consts.get(src.local.0 as usize).and_then(|c| c.clone())
                }
                Rvalue::UnaryOp { op, src } => {
                    if let Some(c) = resolve_const(src, &consts) {
                        eval_mir_unary(*op, c)
                    } else {
                        None
                    }
                }
                Rvalue::BinaryOp { op, lhs, rhs } => {
                    if let (Some(lc), Some(rc)) =
                        (resolve_const(lhs, &consts), resolve_const(rhs, &consts))
                    {
                        eval_mir_binary(*op, lc, rc)
                    } else {
                        None
                    }
                }
                _ => None,
            };

            if let Some(c) = folded {
                if !matches!(rvalue, Rvalue::Use(Operand::Constant(_))) {
                    *rvalue = Rvalue::Use(Operand::Constant(c.clone()));
                    changed = true;
                }
                if dst_idx < consts.len() {
                    consts[dst_idx] = Some(c);
                }
            } else if dst_idx < consts.len() {
                consts[dst_idx] = None;
            }
        }

        if let TerminatorKind::SwitchBool {
            cond,
            true_bb,
            false_bb,
        } = &bb.terminator
            && let Some(c) = resolve_const(cond, &consts)
        {
            let target = if const_truthy(c) { *true_bb } else { *false_bb };
            bb.terminator = TerminatorKind::Goto(target);
            changed = true;
        }
    }

    changed
}

fn mark_place_reads(place: &Place, counts: &mut [u32], include_base: bool) {
    if include_base && let Some(slot) = counts.get_mut(place.local.0 as usize) {
        *slot = slot.saturating_add(1);
    }
    for proj in &place.projections {
        if let crate::compiler::mir::cfg::Projection::Index(op) = proj {
            mark_operand_reads(op, counts);
        }
    }
}

fn mark_operand_reads(op: &Operand, counts: &mut [u32]) {
    if let Operand::Copy(p) | Operand::Move(p) = op {
        mark_place_reads(p, counts, true);
    }
}

fn mark_spread_reads(src: &MirSpreadSource, counts: &mut [u32]) {
    if let MirSpreadSource::Call {
        callee,
        args,
        trailing,
    } = src
    {
        mark_operand_reads(callee, counts);
        for a in args {
            mark_operand_reads(a, counts);
        }
        if let Some(inner) = trailing {
            mark_spread_reads(inner, counts);
        }
    }
}

fn is_pure_dead_temp_rvalue(rvalue: &Rvalue) -> bool {
    match rvalue {
        Rvalue::Use(Operand::Constant(_)) | Rvalue::NewTable { .. } | Rvalue::Upvalue(_) => true,
        Rvalue::Use(Operand::Copy(p) | Operand::Move(p)) => p.projections.is_empty(),
        _ => false,
    }
}

fn eliminate_dead_temps(body: &mut MirBody) -> bool {
    if body.locals.is_empty() {
        return false;
    }
    let mut reads = vec![0u32; body.locals.len()];
    for bb in &body.blocks {
        for stmt in &bb.statements {
            match &stmt.kind {
                StatementKind::Assign(place, rvalue) => {
                    // Projections on LHS read the base table and any index operands.
                    if !place.projections.is_empty() {
                        mark_place_reads(place, &mut reads, true);
                    }
                    match rvalue {
                        Rvalue::Use(op) => mark_operand_reads(op, &mut reads),
                        Rvalue::BinaryOp { lhs, rhs, .. } => {
                            mark_operand_reads(lhs, &mut reads);
                            mark_operand_reads(rhs, &mut reads);
                        }
                        Rvalue::UnaryOp { src, .. } => mark_operand_reads(src, &mut reads),
                        Rvalue::Closure { captures, .. } => {
                            for cap in captures.iter().flatten() {
                                if let Some(slot) = reads.get_mut(cap.0 as usize) {
                                    *slot = slot.saturating_add(1);
                                }
                            }
                        }
                        Rvalue::Global(_)
                        | Rvalue::Upvalue(_)
                        | Rvalue::ModuleImport { .. }
                        | Rvalue::NewTable { .. }
                        | Rvalue::Vararg => {}
                    }
                }
                StatementKind::SetGlobal { value, .. } | StatementKind::SetUpval { value, .. } => {
                    mark_operand_reads(value, &mut reads);
                }
                StatementKind::AppendArray { table, value } => {
                    if let Some(slot) = reads.get_mut(table.0 as usize) {
                        *slot = slot.saturating_add(1);
                    }
                    mark_operand_reads(value, &mut reads);
                }
                StatementKind::CloseUpvals(vars) => {
                    for v in vars {
                        if let Some(slot) = reads.get_mut(v.0 as usize) {
                            *slot = slot.saturating_add(1);
                        }
                    }
                }
                StatementKind::DebugLocal { .. } | StatementKind::DebugScopeEnd { .. } => {}
            }
        }
        match &bb.terminator {
            TerminatorKind::Goto(_) | TerminatorKind::Unreachable => {}
            TerminatorKind::SwitchBool { cond, .. } => mark_operand_reads(cond, &mut reads),
            TerminatorKind::Call { callee, args, .. } => {
                mark_operand_reads(callee, &mut reads);
                for a in args {
                    mark_operand_reads(a, &mut reads);
                }
            }
            TerminatorKind::SpreadCall {
                callee,
                fixed_args,
                trailing,
                ..
            } => {
                mark_operand_reads(callee, &mut reads);
                for a in fixed_args {
                    mark_operand_reads(a, &mut reads);
                }
                mark_spread_reads(trailing, &mut reads);
            }
            TerminatorKind::SpreadList { table, source, .. } => {
                if let Some(slot) = reads.get_mut(table.0 as usize) {
                    *slot = slot.saturating_add(1);
                }
                mark_spread_reads(source, &mut reads);
            }
            TerminatorKind::SpreadReturn(src) => mark_spread_reads(src, &mut reads),
            TerminatorKind::ForPrep {
                base,
                limit,
                step,
                loop_var,
                ..
            } => {
                for id in [base, limit, step, loop_var] {
                    if let Some(slot) = reads.get_mut(id.0 as usize) {
                        *slot = slot.saturating_add(1);
                    }
                }
            }
            TerminatorKind::ForLoop { base, .. } => {
                if let Some(slot) = reads.get_mut(base.0 as usize) {
                    *slot = slot.saturating_add(1);
                }
            }
            TerminatorKind::TForStep {
                base,
                state,
                ctrl,
                vars,
                ..
            } => {
                for id in [base, state, ctrl].into_iter().chain(vars.iter()) {
                    if let Some(slot) = reads.get_mut(id.0 as usize) {
                        *slot = slot.saturating_add(1);
                    }
                }
            }
            TerminatorKind::Return(ops) => {
                for op in ops {
                    mark_operand_reads(op, &mut reads);
                }
            }
        }
    }

    let mut changed = false;
    for bb in &mut body.blocks {
        let before = bb.statements.len();
        bb.statements.retain(|stmt| {
            if let StatementKind::Assign(place, rvalue) = &stmt.kind
                && place.projections.is_empty()
                && let Some(decl) = body.locals.get(place.local.0 as usize)
                && decl.kind == LocalKind::Temp
                && reads[place.local.0 as usize] == 0
                && is_pure_dead_temp_rvalue(rvalue)
            {
                return false;
            }
            true
        });
        if bb.statements.len() != before {
            changed = true;
        }
    }
    changed
}

fn collapse_trampoline_blocks(body: &mut MirBody) -> bool {
    if body.blocks.len() <= 1 {
        return false;
    }
    let entry_id = body.blocks[0].id;
    let mut redirects: FxHashMap<BasicBlockId, BasicBlockId> = with_capacity(4);
    for bb in &body.blocks {
        if bb.id != entry_id
            && bb.statements.is_empty()
            && let TerminatorKind::Goto(target) = bb.terminator
            && target != bb.id
        {
            redirects.insert(bb.id, target);
        }
    }
    if redirects.is_empty() {
        return false;
    }
    // Resolve chains A -> B -> C safely (bounded for cycles).
    let keys: Vec<BasicBlockId> = redirects.keys().copied().collect();
    for k in keys {
        let mut cur = redirects[&k];
        for _ in 0..redirects.len() {
            if let Some(&next) = redirects.get(&cur)
                && next != k
            {
                cur = next;
            } else {
                break;
            }
        }
        redirects.insert(k, cur);
    }

    let mut changed = false;
    for bb in &mut body.blocks {
        for (&from, &to) in &redirects {
            if remap_term_target(&mut bb.terminator, from, to) {
                changed = true;
            }
        }
    }
    changed
}

fn prune_unreachable_blocks(body: &mut MirBody) -> bool {
    if body.blocks.len() <= 1 {
        return false;
    }
    let mut block_map: FxHashMap<BasicBlockId, usize> = with_capacity(body.blocks.len());
    for (i, b) in body.blocks.iter().enumerate() {
        block_map.insert(b.id, i);
    }
    let mut visited = set_with_capacity(body.blocks.len());
    let mut queue = VecDeque::with_capacity(body.blocks.len());
    let mut succs = Vec::with_capacity(4);
    let entry = body.blocks[0].id;
    visited.insert(entry);
    queue.push_back(entry);

    while let Some(id) = queue.pop_front() {
        if let Some(&idx) = block_map.get(&id) {
            push_term_successors(&body.blocks[idx].terminator, &mut succs);
            for &succ in &succs {
                if visited.insert(succ) {
                    queue.push_back(succ);
                }
            }
        }
    }

    if visited.len() == body.blocks.len() {
        return false;
    }
    body.blocks.retain(|b| visited.contains(&b.id));
    true
}

fn optimize_body(body: &mut MirBody) {
    for _ in 0..8 {
        let c1 = fold_constants_and_branches(body);
        let c2 = collapse_trampoline_blocks(body);
        let c3 = prune_unreachable_blocks(body);
        let c4 = eliminate_dead_temps(body);
        if !c1 && !c2 && !c3 && !c4 {
            break;
        }
    }
    for child in &mut body.functions {
        optimize_body(child);
    }
}

/// Applies MIR-level passes when `opt_level` is [`OptLevel::O1`].
pub fn optimize_mir(program: &mut MirProgram, opt_level: OptLevel) {
    if opt_level == OptLevel::O0 {
        return;
    }
    optimize_body(&mut program.main);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::{OptLevel, compile_source, hir, mir, thir};

    #[test]
    fn test_mir_opt_folds_constants_and_collapses_trampolines() {
        let code = "\
local x = 10 + 20 * 2
if x > 40 then
  return x - 5
else
  return 0
end";
        let stmts = compile_source(code).expect("syntax error");
        let hir_mod = hir::lower_ast(&stmts, None, OptLevel::O1);
        let thir_mod = thir::lower_hir_to_thir(&hir_mod);
        let mut mir_prog = mir::lower_thir_to_mir(&thir_mod, false);
        optimize_mir(&mut mir_prog, OptLevel::O1);

        // No dead unreachable `else` return 0 block should survive once folded.
        let has_zero_ret = mir_prog.main.blocks.iter().any(|b| {
            matches!(
                &b.terminator,
                TerminatorKind::Return(ops)
                    if ops.first().is_some_and(|o| matches!(o, Operand::Constant(MirConst::Int(n)) if *n == 0.into()))
            )
        });
        assert!(!has_zero_ret, "expected dead branch pruned in MIR");
    }
}
