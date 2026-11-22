//! Loop invariant code motion: pure expressions that do not change inside
//! a loop move in front of it.

use crate::compiler::ir::block::ControlFlowGraph;
use crate::compiler::ir::dom::ImmediateDominators;
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::types::{IrLabel, IrVar};
use crate::vm::hash::{FxHashMap, FxHashSet, new_map, new_set, with_capacity};

#[derive(Debug, Clone)]
pub struct NaturalLoop {
    pub header: IrLabel,
    pub back_edge_source: IrLabel,
    pub body: FxHashSet<IrLabel>,
}

// Identifies all natural loops in the CFG using dominance information
pub fn find_natural_loops(cfg: &ControlFlowGraph, dom: &ImmediateDominators) -> Vec<NaturalLoop> {
    let mut loops: Vec<NaturalLoop> = Vec::new();
    let mut header_to_loop: FxHashMap<IrLabel, usize> = new_map();
    let label_to_idx: FxHashMap<IrLabel, usize> = cfg
        .blocks
        .iter()
        .enumerate()
        .map(|(i, b)| (b.label, i))
        .collect();

    for block in &cfg.blocks {
        let n = block.label;
        for &h in &block.successors {
            // If h dominates n, then (n -> h) is a back-edge
            if dom.dominates(h, n) {
                // Natural loops sharing the same header (such as a loop with
                // `continue` plus its normal back-edge) form one loop: keeping
                // them separate treats one back-edge as an outside predecessor
                // of the other's preheader and misses definitions made along
                // sibling back-edges.
                let loop_idx = match header_to_loop.get(&h).copied() {
                    Some(idx) => idx,
                    None => {
                        let idx = loops.len();
                        let mut body = new_set();
                        body.insert(h);
                        loops.push(NaturalLoop {
                            header: h,
                            back_edge_source: n,
                            body,
                        });
                        header_to_loop.insert(h, idx);
                        idx
                    }
                };
                let body = &mut loops[loop_idx].body;
                if body.insert(n) && n != h {
                    let mut stack = vec![n];
                    while let Some(current) = stack.pop() {
                        if let Some(&idx) = label_to_idx.get(&current) {
                            for &pred in &cfg.blocks[idx].predecessors {
                                if dom.dominates(h, pred) && body.insert(pred) {
                                    stack.push(pred);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    loops
}

/// Whether an instruction in a loop body may run earlier without observable effect.
fn is_hoistable_body(inst: &IrInst) -> bool {
    matches!(
        inst,
        IrInst::LoadConst { .. } | IrInst::LoadNil { .. } | IrInst::Move { .. }
    )
}

/// In the loop header, instructions before the exit test run unconditionally on loop entry.
/// Only pure, side-effect-free instructions with no potential metamethod invocations or runtime errors can be hoisted.
fn is_hoistable_header(inst: &IrInst) -> bool {
    matches!(
        inst,
        IrInst::LoadConst { .. } | IrInst::LoadNil { .. } | IrInst::Move { .. }
    )
}

/// Ensures the loop header has a dedicated pre-header block outside the loop.
/// If not already present, creates and inserts one, redirecting outside predecessors.
fn ensure_preheader(
    cfg: &mut ControlFlowGraph,
    lp: &NaturalLoop,
    next_label_id: &mut usize,
) -> (IrLabel, bool) {
    let outside_preds: Vec<IrLabel> = match cfg.find_block(lp.header) {
        Some(h) => h
            .predecessors
            .iter()
            .filter(|p| !lp.body.contains(p))
            .copied()
            .collect(),
        None => return (lp.header, false),
    };

    if outside_preds.len() == 1 {
        let p = outside_preds[0];
        if p != lp.header
            && let Some(blk) = cfg.find_block(p)
            && blk.successors.len() == 1
            && blk.successors[0] == lp.header
        {
            return (p, false);
        }
    }

    *next_label_id += 1;
    let preheader_label = IrLabel(*next_label_id);

    let mut preheader_block = crate::compiler::ir::BasicBlock::new(preheader_label);
    preheader_block.instructions.push(IrInst::Jump(lp.header));
    preheader_block.successors.push(lp.header);
    preheader_block.predecessors = outside_preds.clone();

    for &pred_lbl in &outside_preds {
        if let Some(pred_blk) = cfg.find_block_mut(pred_lbl) {
            for inst in &mut pred_blk.instructions {
                match inst {
                    IrInst::Jump(target) if *target == lp.header => {
                        *target = preheader_label;
                    }
                    IrInst::JumpIfFalse { target, .. } if *target == lp.header => {
                        *target = preheader_label;
                    }
                    IrInst::ForPrep { jump, .. }
                    | IrInst::ForLoop { jump, .. }
                    | IrInst::TForLoop { jump, .. }
                        if *jump == lp.header =>
                    {
                        *jump = preheader_label;
                    }
                    _ => {}
                }
            }
            for succ in &mut pred_blk.successors {
                if *succ == lp.header {
                    *succ = preheader_label;
                }
            }
        }
    }

    if let Some(header_blk) = cfg.find_block_mut(lp.header) {
        header_blk.predecessors.retain(|p| lp.body.contains(p));
        header_blk.predecessors.push(preheader_label);
    }

    if cfg.entry_label == lp.header {
        cfg.entry_label = preheader_label;
    }

    let header_pos = cfg
        .blocks
        .iter()
        .position(|b| b.label == lp.header)
        .unwrap_or(0);
    cfg.blocks.insert(header_pos, preheader_block);

    (preheader_label, true)
}

// Hoists loop-invariant pure instructions out of natural loops
pub fn loop_invariant_code_motion(cfg: &mut ControlFlowGraph, dom: &ImmediateDominators) -> bool {
    let loops = find_natural_loops(cfg, dom);
    if loops.is_empty() {
        return false;
    }
    let captured = super::captured_vars(cfg);
    let mut changed = false;

    let mut next_label_id = cfg.blocks.iter().map(|b| b.label.0).max().unwrap_or(0);

    // The IR is not in SSA form here: a variable assigned more than once
    // anywhere in the function is a real local whose value at a given point
    // depends on which assignment ran last, so none of its assignments can
    // move. Only single-assignment temporaries are candidates.
    let mut def_counts: FxHashMap<IrVar, usize> = new_map();
    for block in &cfg.blocks {
        for inst in &block.instructions {
            inst.for_each_def(|def| {
                *def_counts.entry(def).or_insert(0) += 1;
            });
        }
    }

    let mut label_to_idx: FxHashMap<IrLabel, usize> = with_capacity(cfg.blocks.len());
    for (i, b) in cfg.blocks.iter().enumerate() {
        label_to_idx.insert(b.label, i);
    }

    for lp in loops {
        // Collect all variables defined inside the loop
        let mut loop_defs = new_set();
        for &block_lbl in &lp.body {
            if let Some(&idx) = label_to_idx.get(&block_lbl) {
                for inst in &cfg.blocks[idx].instructions {
                    inst.for_each_def(|def| {
                        loop_defs.insert(def);
                    });
                }
            }
        }

        let mut invariant_defs = new_set();
        let mut hoisted_insts = Vec::new();

        // In layout order
        let body_in_layout_order: Vec<IrLabel> = cfg
            .blocks
            .iter()
            .map(|blk| blk.label)
            .filter(|lbl| lp.body.contains(lbl))
            .collect();

        let defs_hoistable = |inst: &IrInst| {
            let mut ok = true;
            inst.for_each_def(|d| {
                if captured.contains(&d) || def_counts.get(&d).copied().unwrap_or(0) != 1 {
                    ok = false;
                }
            });
            ok
        };

        // 1. Scan loop header for invariant instructions before the first branch/terminator
        if let Some(header_blk) = cfg.find_block_mut(lp.header) {
            let mut remaining = Vec::with_capacity(header_blk.instructions.len());
            let mut past_branch = false;

            for inst in header_blk.instructions.drain(..) {
                if !past_branch
                    && (inst.is_terminator() || matches!(inst, IrInst::JumpIfFalse { .. }))
                {
                    past_branch = true;
                }

                if !past_branch && is_hoistable_header(&inst) && defs_hoistable(&inst) {
                    let mut is_invariant = true;
                    inst.for_each_use(|u| {
                        // A variable captured by a closure may be mutated via
                        // `SETUPVAL` on any call, yield or metamethod inside the loop.
                        if captured.contains(&u)
                            || (loop_defs.contains(&u) && !invariant_defs.contains(&u))
                        {
                            is_invariant = false;
                        }
                    });

                    if is_invariant {
                        if let Some(d) = inst.def_var() {
                            invariant_defs.insert(d);
                        }
                        hoisted_insts.push(inst);
                        changed = true;
                        continue;
                    }
                }
                remaining.push(inst);
            }
            header_blk.instructions = remaining;
        }

        // 2. Scan remaining loop blocks for invariant instructions
        for block_lbl in body_in_layout_order {
            if block_lbl == lp.header {
                continue;
            }

            let Some(blk) = cfg.find_block_mut(block_lbl) else {
                continue;
            };

            let mut remaining = Vec::with_capacity(blk.instructions.len());
            for inst in blk.instructions.drain(..) {
                if is_hoistable_body(&inst) && defs_hoistable(&inst) {
                    let mut is_invariant = true;
                    inst.for_each_use(|u| {
                        if captured.contains(&u)
                            || (loop_defs.contains(&u) && !invariant_defs.contains(&u))
                        {
                            is_invariant = false;
                        }
                    });

                    if is_invariant {
                        if let Some(d) = inst.def_var() {
                            invariant_defs.insert(d);
                        }
                        hoisted_insts.push(inst);
                        changed = true;
                        continue;
                    }
                }
                remaining.push(inst);
            }
            blk.instructions = remaining;
        }

        // Insert hoisted instructions into preheader before terminator
        if hoisted_insts.is_empty() {
            continue;
        }

        let (preheader_label, created) = ensure_preheader(cfg, &lp, &mut next_label_id);
        changed = true;
        if created {
            label_to_idx.clear();
            for (i, b) in cfg.blocks.iter().enumerate() {
                label_to_idx.insert(b.label, i);
            }
        }

        if let Some(preheader) = cfg.find_block_mut(preheader_label) {
            let term_pos = preheader
                .instructions
                .iter()
                .position(|i| i.is_terminator())
                .unwrap_or(preheader.instructions.len());

            for (offset, inst) in hoisted_insts.into_iter().enumerate() {
                preheader.instructions.insert(term_pos + offset, inst);
            }
        }
    }

    changed
}
