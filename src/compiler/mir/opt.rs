//! MIR-level optimization passes (`-O1`): branch folding and dead block pruning.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::compiler::OptLevel;
use crate::compiler::mir::cfg::{
    BasicBlockId, MirBody, MirConst, MirProgram, Operand, Rvalue, StatementKind, TerminatorKind,
};

fn const_truthy(c: &MirConst) -> bool {
    !matches!(c, MirConst::Nil | MirConst::Bool(false))
}

fn term_successors(term: &TerminatorKind) -> Vec<BasicBlockId> {
    match term {
        TerminatorKind::Goto(bb) => vec![*bb],
        TerminatorKind::SwitchBool {
            true_bb, false_bb, ..
        } => vec![*true_bb, *false_bb],
        TerminatorKind::Call { target, .. }
        | TerminatorKind::SpreadCall { target, .. }
        | TerminatorKind::SpreadList { target, .. } => vec![*target],
        TerminatorKind::ForPrep {
            test_bb, body_bb, ..
        } => vec![*test_bb, *body_bb],
        TerminatorKind::ForLoop {
            body_bb, exit_bb, ..
        }
        | TerminatorKind::TForStep {
            body_bb, exit_bb, ..
        } => vec![*body_bb, *exit_bb],
        TerminatorKind::SpreadReturn { .. }
        | TerminatorKind::Return(_)
        | TerminatorKind::Unreachable => Vec::new(),
    }
}

fn fold_constant_branches(body: &mut MirBody) -> bool {
    let mut changed = false;

    for bb in &mut body.blocks {
        let mut consts: HashMap<crate::compiler::mir::cfg::LocalId, MirConst> = HashMap::new();
        for stmt in &bb.statements {
            if let StatementKind::Assign(place, rvalue) = &stmt.kind
                && place.projections.is_empty()
            {
                match rvalue {
                    Rvalue::Use(Operand::Constant(c)) => {
                        consts.insert(place.local, c.clone());
                    }
                    Rvalue::Use(Operand::Copy(src) | Operand::Move(src))
                        if src.projections.is_empty() =>
                    {
                        if let Some(c) = consts.get(&src.local).cloned() {
                            consts.insert(place.local, c);
                        } else {
                            consts.remove(&place.local);
                        }
                    }
                    _ => {
                        consts.remove(&place.local);
                    }
                }
            }
        }

        if let TerminatorKind::SwitchBool {
            cond,
            true_bb,
            false_bb,
        } = &bb.terminator
        {
            let known_const = match cond {
                Operand::Constant(c) => Some(c),
                Operand::Copy(p) | Operand::Move(p) if p.projections.is_empty() => {
                    consts.get(&p.local)
                }
                _ => None,
            };
            if let Some(c) = known_const {
                let target = if const_truthy(c) { *true_bb } else { *false_bb };
                bb.terminator = TerminatorKind::Goto(target);
                changed = true;
            }
        }
    }

    changed
}

fn prune_unreachable_blocks(body: &mut MirBody) -> bool {
    if body.blocks.is_empty() {
        return false;
    }
    let block_map: HashMap<BasicBlockId, usize> = body
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.id, i))
        .collect();
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    let entry = body.blocks[0].id;
    visited.insert(entry);
    queue.push_back(entry);

    while let Some(id) = queue.pop_front() {
        if let Some(&idx) = block_map.get(&id) {
            for succ in term_successors(&body.blocks[idx].terminator) {
                if visited.insert(succ) {
                    queue.push_back(succ);
                }
            }
        }
    }

    let before = body.blocks.len();
    body.blocks.retain(|b| visited.contains(&b.id));
    body.blocks.len() != before
}

fn optimize_body(body: &mut MirBody) {
    for _ in 0..8 {
        let c1 = fold_constant_branches(body);
        let c2 = prune_unreachable_blocks(body);
        if !c1 && !c2 {
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
