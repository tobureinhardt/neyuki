//! Splits a linear IR sequence into a control flow graph.

use crate::compiler::ir::block::{BasicBlock, ControlFlowGraph};
use crate::compiler::ir::inst::{IrInst, SpreadSink};
use crate::compiler::ir::types::IrLabel;
use crate::vm::hash::{FxHashMap, with_capacity};

pub fn build_cfg(instructions: &[IrInst]) -> ControlFlowGraph {
    if instructions.is_empty() {
        let entry = IrLabel(0);
        let mut cfg = ControlFlowGraph::new(entry);
        cfg.blocks.push(BasicBlock::new(entry));
        return cfg;
    }

    // 1. Identify block leaders (entry, label targets, instructions following jumps)
    let mut is_leader = vec![false; instructions.len()];
    is_leader[0] = true; // first instruction is always a leader

    for (idx, inst) in instructions.iter().enumerate() {
        match inst {
            IrInst::Label(_) => {
                is_leader[idx] = true;
            }
            IrInst::Jump(_)
            | IrInst::JumpIfFalse { .. }
            | IrInst::Return(_)
            | IrInst::ForPrep { .. }
            | IrInst::ForLoop { .. }
            | IrInst::TForLoop { .. }
                if idx + 1 < instructions.len() =>
            {
                is_leader[idx + 1] = true;
            }
            _ => {}
        }
    }

    // 2. Partition instructions into basic blocks
    let mut blocks: Vec<BasicBlock> = Vec::new();
    let mut current_block: Option<BasicBlock> = None;
    // Auto labels start above every label already in the stream. A CFG that
    // was flattened and rebuilt carries the previous round's auto labels as
    // real `Label` instructions, and minting from 900_000 again would hand
    // two blocks the same label: every map keyed by label, liveness above
    // all, then merges them and the allocator reuses registers that are
    // still live.
    let mut auto_label_id = 900_000usize;
    for inst in instructions {
        if let IrInst::Label(lbl) = inst {
            auto_label_id = auto_label_id.max(lbl.0);
        }
        if let Some(target) = inst.jump_target() {
            auto_label_id = auto_label_id.max(target.0);
        }
    }

    for (idx, inst) in instructions.iter().enumerate() {
        if is_leader[idx] {
            if let Some(blk) = current_block.take() {
                blocks.push(blk);
            }

            let block_label = match inst {
                IrInst::Label(lbl) => *lbl,
                _ => {
                    auto_label_id += 1;
                    IrLabel(auto_label_id)
                }
            };

            let mut new_blk = BasicBlock::new(block_label);
            if !matches!(inst, IrInst::Label(_)) {
                new_blk.add_instruction(inst.clone());
            }
            current_block = Some(new_blk);
        } else if let Some(blk) = current_block.as_mut() {
            match inst {
                IrInst::Label(_) => {}
                _ => blk.add_instruction(inst.clone()),
            }
        }
    }

    if let Some(blk) = current_block.take() {
        blocks.push(blk);
    }

    if blocks.is_empty() {
        let entry = IrLabel(0);
        let mut cfg = ControlFlowGraph::new(entry);
        cfg.blocks.push(BasicBlock::new(entry));
        return cfg;
    }

    let mut label_map: FxHashMap<IrLabel, usize> = with_capacity(blocks.len());
    for (i, b) in blocks.iter().enumerate() {
        label_map.insert(b.label, i);
    }

    // 3. Connect successors and predecessors. A block that falls through to
    // the next one also gets an explicit `Jump` to it: the optimizer deletes
    // and merges blocks freely, and an edge that only exists as "whatever
    // comes next" would be lost the moment the layout changes. Flattening
    // drops the jump again when the target does end up next.
    let num_blocks = blocks.len();
    for i in 0..num_blocks {
        let terminator = blocks[i].terminator().cloned();
        let next_label = if i + 1 < num_blocks {
            Some(blocks[i + 1].label)
        } else {
            None
        };

        let mut succs = Vec::new();
        let mut add_succ = |target: IrLabel| {
            if !succs.contains(&target) {
                succs.push(target);
            }
        };

        let mut falls_through = false;
        match terminator {
            Some(IrInst::Jump(target)) => {
                add_succ(target);
            }
            Some(IrInst::JumpIfFalse { target, .. }) => {
                add_succ(target);
                falls_through = true;
            }
            Some(IrInst::ForPrep { jump, .. })
            | Some(IrInst::ForLoop { jump, .. })
            | Some(IrInst::TForLoop { jump, .. }) => {
                add_succ(jump);
                falls_through = true;
            }
            Some(IrInst::Return(_))
            | Some(IrInst::Spread {
                sink: SpreadSink::Return { .. },
                ..
            }) => {
                // Exit block, no successors
            }
            _ => {
                falls_through = true;
            }
        }
        if falls_through && let Some(next) = next_label {
            add_succ(next);
            blocks[i].instructions.push(IrInst::Jump(next));
        }

        blocks[i].successors = succs;
    }

    // 4. Invert successors to set predecessors
    for i in 0..num_blocks {
        let from_label = blocks[i].label;
        let succs = blocks[i].successors.clone();
        for target in succs {
            if let Some(&target_idx) = label_map.get(&target) {
                let preds = &mut blocks[target_idx].predecessors;
                if !preds.contains(&from_label) {
                    preds.push(from_label);
                }
            }
        }
    }

    let entry_label = blocks[0].label;
    ControlFlowGraph {
        blocks,
        entry_label,
    }
}
