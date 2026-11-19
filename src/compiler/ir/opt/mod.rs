//! The IR optimization passes and their order.

pub mod const_fold;
pub mod copy_prop;
pub mod cse;
pub mod dce;
pub mod inline;
pub mod loop_invariant;
pub mod peephole;
pub mod simplify_cfg;
pub mod strength_reduce;

use std::collections::HashSet;

use crate::compiler::OptLevel;
use crate::compiler::ir::block::ControlFlowGraph;
use crate::compiler::ir::dom::DominatorTree;
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::types::IrVar;

pub use crate::compiler::value_tracking::value_tracking_cfg;
pub use const_fold::constant_propagation;
pub use copy_prop::{copy_propagation, copy_propagation_cfg};
pub use cse::{common_subexpression_elimination, common_subexpression_elimination_cfg};
pub use dce::{dead_code_elimination, dead_code_elimination_cfg};
pub use inline::inline_functions;
pub use loop_invariant::loop_invariant_code_motion;
pub use peephole::peephole_cfg;
pub use simplify_cfg::simplify_cfg;
pub use strength_reduce::strength_reduction_cfg;

/// The variables nested functions capture. A closure holds the register of
/// each captured variable, not a snapshot of its value, so these are aliased
/// from outside the instruction stream: rewriting or dropping a write to one
/// would change what the closure sees. Passes leave them alone.
pub fn captured_vars(cfg: &ControlFlowGraph) -> HashSet<IrVar> {
    let mut captured = HashSet::new();
    for block in &cfg.blocks {
        for inst in &block.instructions {
            if let IrInst::Closure { captures, .. } = inst {
                captured.extend(captures.iter().flatten().copied());
            }
        }
    }
    captured
}

/// Fixpoint optimization driver for CFG.
/// Iteratively executes optimization passes in sequence until the CFG converges
/// to a fixed point (no further changes are made), bounded by MAX_FIXPOINT_ROUNDS.
pub fn optimize_cfg(cfg: &mut ControlFlowGraph) {
    optimize_cfg_with_level(cfg, OptLevel::O1);
}

/// Runs CFG optimization passes according to `level`:
/// - `OptLevel::O0`: structural CFG cleanup only (unreachable/empty-jump pruning)
///   so codegen receives well-formed basic blocks without folding or eliminating
///   user computations.
/// - `OptLevel::O1`: full fixpoint pass pipeline (LICM, CSE, copy propagation,
///   value tracking, strength reduction, DCE, peephole, CFG simplification).
pub fn optimize_cfg_with_level(cfg: &mut ControlFlowGraph, level: OptLevel) {
    if level == OptLevel::O0 {
        simplify_cfg(cfg);
        return;
    }

    const MAX_FIXPOINT_ROUNDS: usize = 16;

    for _ in 0..MAX_FIXPOINT_ROUNDS {
        let mut changed = false;

        // 1. Dominator-based Loop Invariant Code Motion
        let dom = DominatorTree::build(cfg);
        if loop_invariant_code_motion(cfg, &dom) {
            changed = true;
        }

        // 2. Common Subexpression Elimination
        if common_subexpression_elimination_cfg(cfg) {
            changed = true;
        }

        // 3. Copy Propagation
        if copy_propagation_cfg(cfg) {
            changed = true;
        }

        // 4. Value Tracking and Constant / Range Propagation
        if value_tracking_cfg(cfg) {
            changed = true;
        }

        // 5. Algebraic Simplification & Strength Reduction
        if strength_reduction_cfg(cfg) {
            changed = true;
        }

        // 6. Dead Code Elimination
        if dead_code_elimination_cfg(cfg) {
            changed = true;
        }

        // 7. Trivial Phi & Peephole Cleanups
        if peephole_cfg(cfg) {
            changed = true;
        }

        // 8. CFG Simplification (pruning unreachable, collapsing jumps, merging blocks)
        if simplify_cfg(cfg) {
            changed = true;
        }

        if !changed {
            break;
        }
    }
}
