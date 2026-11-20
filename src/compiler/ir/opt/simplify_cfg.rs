//! Control flow cleanup: drops unreachable blocks, merges straight-line
//! blocks, skips empty jumps and resolves constant branches.

use std::collections::VecDeque;

use crate::compiler::ir::block::{BasicBlock, ControlFlowGraph};
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::types::IrLabel;
use crate::vm::hash::{FxHashMap, set_with_capacity, with_capacity};

pub fn simplify_cfg(cfg: &mut ControlFlowGraph) -> bool {
    if cfg.blocks.is_empty() || cfg.is_single_acyclic_block() {
        return false;
    }

    let mut changed = false;
    let mut iterations = 0;
    const MAX_PASSES: usize = 20;

    while iterations < MAX_PASSES {
        iterations += 1;
        let c1 = remove_unreachable_blocks(cfg);
        let c2 = eliminate_empty_jump_blocks(cfg);
        let c3 = merge_single_edge_blocks(cfg);
        if !c1 && !c2 && !c3 {
            break;
        }
        changed = true;
    }

    changed
}

// 1. Remove blocks that cannot be reached from CFG entry
pub fn remove_unreachable_blocks(cfg: &mut ControlFlowGraph) -> bool {
    let mut label_to_idx: FxHashMap<IrLabel, usize> = with_capacity(cfg.blocks.len());
    for (idx, b) in cfg.blocks.iter().enumerate() {
        label_to_idx.insert(b.label, idx);
    }

    let mut reachable = set_with_capacity(cfg.blocks.len());
    let mut queue = VecDeque::with_capacity(cfg.blocks.len());

    reachable.insert(cfg.entry_label);
    queue.push_back(cfg.entry_label);

    while let Some(lbl) = queue.pop_front() {
        if let Some(&idx) = label_to_idx.get(&lbl) {
            for succ in &cfg.blocks[idx].successors {
                if reachable.insert(*succ) {
                    queue.push_back(*succ);
                }
            }
        }
    }

    let initial_len = cfg.blocks.len();
    cfg.blocks.retain(|b| reachable.contains(&b.label));

    // Clean up predecessors in remaining blocks
    for block in &mut cfg.blocks {
        block.predecessors.retain(|p| reachable.contains(p));
    }

    cfg.blocks.len() != initial_len
}

/// Returns `Some(target)` when `block`'s only non-debug instruction is a
/// non-self unconditional `Jump(target)`.
fn empty_jump_target(block: &BasicBlock) -> Option<IrLabel> {
    let mut code = block.instructions.iter().filter(|i| !i.is_debug_marker());
    match (code.next(), code.next()) {
        (Some(IrInst::Jump(target)), None) if *target != block.label => Some(*target),
        _ => None,
    }
}

/// Replaces occurrences of `old_lbl` with `new_lbl` in `labels` without
/// introducing duplicate entries when `new_lbl` is already present.
fn replace_edge_label(labels: &mut Vec<IrLabel>, old_lbl: IrLabel, new_lbl: IrLabel) -> bool {
    let mut has_new = labels.contains(&new_lbl);
    let mut replaced = false;
    labels.retain_mut(|lbl| {
        if *lbl == old_lbl {
            replaced = true;
            if has_new {
                return false;
            }
            *lbl = new_lbl;
            has_new = true;
        }
        true
    });
    replaced
}

// 2. Collapse blocks containing only an unconditional Jump
pub fn eliminate_empty_jump_blocks(cfg: &mut ControlFlowGraph) -> bool {
    let mut changed = false;

    // Collect candidate labels only; each block's current target is re-read
    // below because earlier eliminations in a chain (`A -> B -> C`) update
    // `A`'s jump target and delete `B`, and collapsing a 2-block empty cycle
    // (`A -> B -> A`) turns `B` into a self-loop that must remain.
    let candidates: Vec<IrLabel> = cfg
        .blocks
        .iter()
        .filter(|b| b.label != cfg.entry_label && empty_jump_target(b).is_some())
        .map(|b| b.label)
        .collect();

    for empty_label in candidates {
        let Some(empty_blk) = cfg.find_block(empty_label) else {
            continue;
        };
        if empty_blk.label == cfg.entry_label {
            continue;
        }
        let Some(target_label) = empty_jump_target(empty_blk) else {
            continue;
        };
        if cfg.find_block(target_label).is_none() {
            continue;
        }
        let empty_preds = empty_blk.predecessors.clone();

        // Bypass empty_label in all other blocks
        let mut bypassed = false;

        for block in &mut cfg.blocks {
            if block.label == empty_label {
                continue;
            }

            for inst in &mut block.instructions {
                match inst {
                    IrInst::Jump(j) if *j == empty_label => {
                        *j = target_label;
                        bypassed = true;
                    }
                    IrInst::JumpIfFalse { target, .. } if *target == empty_label => {
                        *target = target_label;
                        bypassed = true;
                    }
                    IrInst::ForPrep { jump, .. }
                    | IrInst::ForLoop { jump, .. }
                    | IrInst::TForLoop { jump, .. }
                        if *jump == empty_label =>
                    {
                        *jump = target_label;
                        bypassed = true;
                    }
                    _ => {}
                }
            }

            if replace_edge_label(&mut block.successors, empty_label, target_label) {
                bypassed = true;
            }
        }

        if bypassed {
            // Update predecessors of target_label
            if let Some(target_blk) = cfg.find_block_mut(target_label) {
                target_blk.predecessors.retain(|&p| p != empty_label);
                for p in &empty_preds {
                    if *p != empty_label && !target_blk.predecessors.contains(p) {
                        target_blk.predecessors.push(*p);
                    }
                }
            }
            cfg.blocks.retain(|b| b.label != empty_label);
            changed = true;
        }
    }

    changed
}

// 3. Merge block A and block B if A has only successor B, and B has only predecessor A
pub fn merge_single_edge_blocks(cfg: &mut ControlFlowGraph) -> bool {
    let mut to_merge = None;

    for i in 0..cfg.blocks.len() {
        let a_label = cfg.blocks[i].label;
        let [b_label] = cfg.blocks[i].successors.as_slice() else {
            continue;
        };
        let b_label = *b_label;
        if b_label == a_label || b_label == cfg.entry_label {
            continue;
        }
        let Some(b_idx) = cfg.block_index(b_label) else {
            continue;
        };
        if cfg.blocks[b_idx].predecessors.as_slice() != [a_label] {
            continue;
        }
        // Only merge when control transfers from A to B solely via a trailing
        // unconditional jump (or fall-through). If an earlier branch in A
        // (such as `JumpIfFalse { target: B }` or `ForLoop { jump: B }`) also
        // targets B, removing B would leave that jump dangling (or turn it
        // into a loop back to A if remapped).
        let a_insts = &cfg.blocks[i].instructions;
        let prefix = if matches!(a_insts.last(), Some(IrInst::Jump(j)) if *j == b_label) {
            &a_insts[..a_insts.len() - 1]
        } else {
            a_insts.as_slice()
        };
        if prefix
            .iter()
            .any(|inst| inst.jump_target() == Some(b_label))
            || cfg.blocks[b_idx]
                .instructions
                .iter()
                .any(|inst| inst.jump_target() == Some(b_label))
        {
            continue;
        }
        to_merge = Some((i, b_idx, a_label, b_label));
        break;
    }

    if let Some((a_idx, b_idx, a_label, b_label)) = to_merge {
        let b_succs = cfg.blocks[b_idx].successors.clone();
        let mut b_insts = cfg.blocks[b_idx].instructions.clone();

        // Remove unconditional jump to B from A
        if matches!(cfg.blocks[a_idx].instructions.last(), Some(IrInst::Jump(j)) if *j == b_label) {
            cfg.blocks[a_idx].instructions.pop();
        }

        cfg.blocks[a_idx].instructions.append(&mut b_insts);
        cfg.blocks[a_idx].successors = b_succs.clone();

        // Update successors of B: replace predecessor B with A
        for succ_lbl in b_succs {
            if let Some(succ_blk) = cfg.find_block_mut(succ_lbl) {
                replace_edge_label(&mut succ_blk.predecessors, b_label, a_label);
            }
        }

        // Remove B
        cfg.blocks.remove(b_idx);
        return true;
    }

    false
}
