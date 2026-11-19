//! Local peephole cleanups and trivial SSA Phi simplification on CFG.

use std::collections::HashSet;

use crate::compiler::ir::block::ControlFlowGraph;
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::types::IrVar;

fn simplify_trivial_phis(cfg: &mut ControlFlowGraph, captured: &HashSet<IrVar>) -> bool {
    let mut changed = false;

    for block in &mut cfg.blocks {
        for inst in &mut block.instructions {
            let IrInst::Phi { dst, incoming } = inst else {
                continue;
            };
            if incoming.is_empty() || captured.contains(dst) {
                continue;
            }
            let mut candidate: Option<IrVar> = None;
            let mut uniform = true;
            for &(_, src) in incoming.iter() {
                if src == *dst {
                    continue;
                }
                match candidate {
                    None => candidate = Some(src),
                    Some(prev) if prev == src => {}
                    Some(_) => {
                        uniform = false;
                        break;
                    }
                }
            }
            if uniform
                && let Some(src) = candidate
                && !captured.contains(&src)
            {
                *inst = IrInst::Move { dst: *dst, src };
                changed = true;
            }
        }
    }

    changed
}

fn cleanup_block_peepholes(cfg: &mut ControlFlowGraph) -> bool {
    let mut changed = false;

    for block in &mut cfg.blocks {
        // 1. Fold `JumpIfFalse { cond, target: t }` when both outgoing edges go to `t`.
        if block.successors.len() == 1 {
            let only_succ = block.successors[0];
            if let Some(IrInst::JumpIfFalse { target, .. }) = block.instructions.last().cloned()
                && target == only_succ
            {
                let last = block
                    .instructions
                    .last_mut()
                    .expect("non-empty checked above");
                *last = IrInst::Jump(only_succ);
                changed = true;
            }
        }

        // 2. Remove redundant self-moves (`Move { dst: x, src: x }`) and
        //    collapse adjacent `Line(a); Line(b)` markers with nothing between them.
        let mut out: Vec<IrInst> = Vec::with_capacity(block.instructions.len());
        for inst in block.instructions.drain(..) {
            match inst {
                IrInst::Move { dst, src } if dst == src => {
                    changed = true;
                }
                IrInst::Line(line) => {
                    if matches!(out.last(), Some(IrInst::Line(_))) {
                        out.pop();
                        changed = true;
                    }
                    out.push(IrInst::Line(line));
                }
                other => out.push(other),
            }
        }
        block.instructions = out;
    }

    changed
}

/// Runs trivial-Phi lowering, redundant self-move removal, degenerate branch
/// folding, and duplicate line-marker cleanup over `cfg`.
pub fn peephole_cfg(cfg: &mut ControlFlowGraph) -> bool {
    let captured = super::captured_vars(cfg);
    let c1 = simplify_trivial_phis(cfg, &captured);
    let c2 = cleanup_block_peepholes(cfg);
    c1 || c2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::ir::block::BasicBlock;
    use crate::compiler::ir::types::IrLabel;

    #[test]
    fn test_peephole_simplifies_trivial_phi_and_self_move() {
        let mut cfg = ControlFlowGraph::new(IrLabel(0));
        let mut b0 = BasicBlock::new(IrLabel(0));
        let v0 = IrVar(0);
        let v1 = IrVar(1);

        b0.add_instruction(IrInst::Phi {
            dst: v1,
            incoming: vec![(IrLabel(1), v0), (IrLabel(2), v0), (IrLabel(0), v1)],
        });
        b0.add_instruction(IrInst::Move { dst: v0, src: v0 });
        b0.add_instruction(IrInst::Line(3));
        b0.add_instruction(IrInst::Line(4));
        b0.add_instruction(IrInst::Return(vec![v1]));
        cfg.blocks.push(b0);

        assert!(peephole_cfg(&mut cfg));
        assert_eq!(
            cfg.blocks[0].instructions,
            vec![
                IrInst::Move { dst: v1, src: v0 },
                IrInst::Line(4),
                IrInst::Return(vec![v1]),
            ]
        );
    }
}
