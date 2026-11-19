//! Control flow cleanup: drops unreachable blocks, merges straight-line
//! blocks, skips empty jumps and resolves constant branches.

use std::collections::{HashSet, VecDeque};

use crate::compiler::ir::block::ControlFlowGraph;
use crate::compiler::ir::inst::IrInst;

pub fn simplify_cfg(cfg: &mut ControlFlowGraph) -> bool {
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
    let mut reachable = HashSet::new();
    let mut queue = VecDeque::new();

    reachable.insert(cfg.entry_label);
    queue.push_back(cfg.entry_label);

    while let Some(lbl) = queue.pop_front() {
        if let Some(block) = cfg.find_block(lbl) {
            for succ in &block.successors {
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

// 2. Collapse blocks containing only an unconditional Jump
pub fn eliminate_empty_jump_blocks(cfg: &mut ControlFlowGraph) -> bool {
    let mut changed = false;
    let mut skipped = HashSet::new();

    loop {
        // Pick one fresh candidate per iteration so chained empty blocks
        // (`A -> B -> C`) never use a stale `target_label` that was already
        // deleted earlier in the same sweep.
        let candidate = cfg
            .blocks
            .iter()
            .filter(|b| b.label != cfg.entry_label && !skipped.contains(&b.label))
            .find_map(|b| {
                let mut code = b.instructions.iter().filter(|i| !i.is_debug_marker());
                match (code.next(), code.next()) {
                    (Some(IrInst::Jump(target)), None)
                        if *target != b.label && cfg.find_block(*target).is_some() =>
                    {
                        Some((b.label, *target))
                    }
                    _ => None,
                }
            });

        let Some((empty_label, target_label)) = candidate else {
            break;
        };

        let empty_preds = cfg
            .find_block(empty_label)
            .map(|b| b.predecessors.clone())
            .unwrap_or_default();

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

            for succ in &mut block.successors {
                if *succ == empty_label {
                    *succ = target_label;
                    bypassed = true;
                }
            }
        }

        if bypassed {
            // Update predecessors of target_label
            if let Some(target_blk) = cfg.find_block_mut(target_label) {
                target_blk.predecessors.retain(|&p| p != empty_label);
                for p in &empty_preds {
                    if !target_blk.predecessors.contains(p) {
                        target_blk.predecessors.push(*p);
                    }
                }
            }
            cfg.blocks.retain(|b| b.label != empty_label);
            changed = true;
        } else {
            skipped.insert(empty_label);
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
        if cfg.blocks[b_idx].predecessors.as_slice() == [a_label] {
            to_merge = Some((i, b_idx, a_label, b_label));
            break;
        }
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
                for p in &mut succ_blk.predecessors {
                    if *p == b_label {
                        *p = a_label;
                    }
                }
            }
        }

        // Remap any instructions referencing b_label across the CFG to a_label
        for blk in &mut cfg.blocks {
            for inst in &mut blk.instructions {
                match inst {
                    IrInst::Jump(j) if *j == b_label => *j = a_label,
                    IrInst::JumpIfFalse { target, .. } if *target == b_label => *target = a_label,
                    IrInst::ForPrep { jump, .. }
                    | IrInst::ForLoop { jump, .. }
                    | IrInst::TForLoop { jump, .. }
                        if *jump == b_label =>
                    {
                        *jump = a_label;
                    }
                    _ => {}
                }
            }
        }

        // Remove B
        cfg.blocks.remove(b_idx);
        return true;
    }

    false
}
