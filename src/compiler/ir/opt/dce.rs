//! Dead code and unreachable block removal.

use crate::compiler::ir::block::{BasicBlock, ControlFlowGraph, IrModule};
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::liveness::LivenessInfo;
use crate::compiler::ir::types::IrVar;
use crate::vm::hash::{FxHashSet, new_set};

pub fn is_pure_instruction(inst: &IrInst) -> bool {
    matches!(
        inst,
        IrInst::LoadConst { .. }
            | IrInst::LoadNil { .. }
            | IrInst::Move { .. }
            | IrInst::BinOp { .. }
            | IrInst::UnOp { .. }
            | IrInst::NewTable { .. }
            | IrInst::Phi { .. }
    )
}

fn dce_block(
    block: &mut BasicBlock,
    mut live_now: FxHashSet<IrVar>,
    captured: &FxHashSet<IrVar>,
) -> bool {
    // Forward pass: indices of stores with statically-nil values.
    // Clearing a register is GC-observable (it releases whatever
    // reference the register may still hold), so a nil-store is
    // never dead code — even when its target is never read again
    // (the explicit-release idiom `t = nil`). Chains like
    // `v10 = nil; v1 = v10` propagate nil-ness forward.
    let mut nil_stores: Vec<bool> = Vec::new();
    if block
        .instructions
        .iter()
        .any(|inst| matches!(inst, IrInst::LoadNil { .. }))
    {
        nil_stores.resize(block.instructions.len(), false);
        let mut nil_known: FxHashSet<IrVar> = new_set();
        for (idx, inst) in block.instructions.iter().enumerate() {
            match inst {
                IrInst::LoadNil { dst } => {
                    nil_known.insert(*dst);
                    nil_stores[idx] = true;
                }
                IrInst::Move { dst, src } => {
                    nil_known.remove(dst);
                    if nil_known.contains(src) {
                        nil_known.insert(*dst);
                        nil_stores[idx] = true;
                    }
                }
                _ => {
                    inst.for_each_def(|d| {
                        nil_known.remove(&d);
                    });
                }
            }
        }
    }

    let mut changed = false;
    let mut retained = Vec::with_capacity(block.instructions.len());

    let mut pos = block.instructions.len();
    for inst in block.instructions.drain(..).rev() {
        pos -= 1;
        let is_nil_store = nil_stores.get(pos).copied().unwrap_or(false);
        let is_dead = if is_pure_instruction(&inst) && !is_nil_store {
            let mut has_def = false;
            let mut all_dead = true;
            inst.for_each_def(|dst| {
                has_def = true;
                if live_now.contains(&dst) || captured.contains(&dst) {
                    all_dead = false;
                }
            });
            has_def && all_dead
        } else {
            false
        };
        if is_dead {
            changed = true;
            continue;
        }

        // Update live_now backwards: remove defs, add uses
        inst.for_each_def(|dst| {
            live_now.remove(&dst);
        });
        inst.for_each_use(|u| {
            live_now.insert(u);
        });

        retained.push(inst);
    }

    retained.reverse();
    block.instructions = retained;
    changed
}

// CFG-aware Dead Code Elimination using backward dataflow liveness analysis
pub fn dead_code_elimination_cfg(cfg: &mut ControlFlowGraph) -> bool {
    let captured = super::captured_vars(cfg);
    dead_code_elimination_cfg_with(cfg, &captured)
}

pub fn dead_code_elimination_cfg_with(
    cfg: &mut ControlFlowGraph,
    captured: &FxHashSet<IrVar>,
) -> bool {
    // Fast path: an acyclic single-block CFG converges in one backward sweep
    // with empty live_out, without computing iterative liveness.
    if cfg.is_single_acyclic_block() {
        return dce_block(&mut cfg.blocks[0], new_set(), captured);
    }

    let mut any_changed = false;
    let mut changed = true;
    let mut iterations = 0;
    const MAX_PASSES: usize = 20;

    while changed && iterations < MAX_PASSES {
        changed = false;
        iterations += 1;

        let liveness = LivenessInfo::compute(cfg);

        for block in &mut cfg.blocks {
            let live_out = liveness
                .blocks
                .get(&block.label)
                .map(|b| b.live_out.clone())
                .unwrap_or_else(new_set);

            if dce_block(block, live_out, captured) {
                changed = true;
                any_changed = true;
            }
        }
    }

    any_changed
}

pub fn dead_code_elimination(module: &mut IrModule) {
    let mut used_vars: FxHashSet<IrVar> = new_set();

    for inst in &module.main.instructions {
        inst.for_each_use(|u| {
            used_vars.insert(u);
        });
    }

    let mut filtered_insts = Vec::new();
    let mut unreachable = false;

    for inst in module.main.instructions.drain(..) {
        if unreachable {
            if let IrInst::Label(_) = inst {
                unreachable = false;
                filtered_insts.push(inst);
            }
            continue;
        }

        match &inst {
            IrInst::LoadConst { dst, .. } | IrInst::LoadNil { dst } => {
                if used_vars.contains(dst) {
                    filtered_insts.push(inst);
                }
            }
            IrInst::BinOp { dst, .. } | IrInst::UnOp { dst, .. } => {
                if used_vars.contains(dst) {
                    filtered_insts.push(inst);
                }
            }
            IrInst::Return(_) => {
                filtered_insts.push(inst);
                unreachable = true;
            }
            IrInst::Jump(_) => {
                filtered_insts.push(inst);
                unreachable = true;
            }
            _ => {
                filtered_insts.push(inst);
            }
        }
    }

    module.main.instructions = filtered_insts;
}
