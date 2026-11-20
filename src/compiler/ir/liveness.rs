//! Backward liveness analysis over the control flow graph.

use crate::compiler::ir::block::ControlFlowGraph;
use crate::compiler::ir::types::{IrLabel, IrVar};
use crate::vm::hash::{FxHashMap, FxHashSet, new_set, set_with_capacity, with_capacity};

#[derive(Debug, Clone)]
pub struct BlockLiveness {
    pub use_vars: FxHashSet<IrVar>,
    pub def_vars: FxHashSet<IrVar>,
    pub live_in: FxHashSet<IrVar>,
    pub live_out: FxHashSet<IrVar>,
}

#[derive(Debug, Clone)]
pub struct LivenessInfo {
    pub blocks: FxHashMap<IrLabel, BlockLiveness>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveInterval {
    pub var: IrVar,
    pub start: usize,
    pub end: usize,
}

impl LivenessInfo {
    pub fn compute(cfg: &ControlFlowGraph) -> Self {
        let mut blocks = with_capacity(cfg.blocks.len());

        // Fast path: an acyclic single-block CFG has empty live_out and
        // live_in == use_vars, needing no iterative dataflow or postorder walk.
        if cfg.is_single_acyclic_block() {
            let block = &cfg.blocks[0];
            let mut block_use = new_set();
            let mut block_def = new_set();
            for inst in &block.instructions {
                inst.for_each_use(|u| {
                    if !block_def.contains(&u) {
                        block_use.insert(u);
                    }
                });
                inst.for_each_def(|d| {
                    block_def.insert(d);
                });
            }
            let live_in = block_use.clone();
            blocks.insert(
                block.label,
                BlockLiveness {
                    use_vars: block_use,
                    def_vars: block_def,
                    live_in,
                    live_out: new_set(),
                },
            );
            return Self { blocks };
        }

        let mut label_to_idx: FxHashMap<IrLabel, usize> = with_capacity(cfg.blocks.len());

        // 1. Calculate local use and def sets for each basic block
        for (idx, block) in cfg.blocks.iter().enumerate() {
            label_to_idx.insert(block.label, idx);
            let mut block_use = new_set();
            let mut block_def = new_set();

            for inst in &block.instructions {
                inst.for_each_use(|u| {
                    if !block_def.contains(&u) {
                        block_use.insert(u);
                    }
                });
                inst.for_each_def(|d| {
                    block_def.insert(d);
                });
            }

            blocks.insert(
                block.label,
                BlockLiveness {
                    use_vars: block_use,
                    def_vars: block_def,
                    live_in: new_set(),
                    live_out: new_set(),
                },
            );
        }

        // 2. Iterative backward dataflow analysis
        // out[B] = Union_{S in succ[B]} in[S]
        // in[B] = use[B] Union (out[B] \ def[B])
        //
        // Blocks are visited in postorder (successors before predecessors),
        // so straight-line dependency chains settle in one round and each
        // loop nesting level costs roughly one more round. Visiting in raw
        // layout order can propagate only one block per round and hit the
        // iteration cap on loop-heavy functions.
        let order = Self::reverse_postorder(cfg, &label_to_idx);
        let mut changed = true;
        let mut iterations = 0;
        const MAX_ITERATIONS: usize = 1000;

        while changed && iterations < MAX_ITERATIONS {
            changed = false;
            iterations += 1;

            for lbl in &order {
                // Compute out[B]
                let mut new_out = new_set();
                if let Some(&idx) = label_to_idx.get(lbl) {
                    for succ in &cfg.blocks[idx].successors {
                        if let Some(succ_info) = blocks.get(succ) {
                            new_out.extend(succ_info.live_in.iter().copied());
                        }
                    }
                }

                // Compute in[B] = use[B] Union (out[B] \ def[B]), borrowing
                // the stored sets instead of cloning them every round.
                let b = &blocks[lbl];
                let mut new_in = set_with_capacity(b.use_vars.len() + new_out.len());
                new_in.extend(b.use_vars.iter().copied());
                for v in &new_out {
                    if !b.def_vars.contains(v) {
                        new_in.insert(*v);
                    }
                }

                let cur = blocks.get_mut(lbl).expect("block exists");
                if cur.live_out != new_out || cur.live_in != new_in {
                    cur.live_out = new_out;
                    cur.live_in = new_in;
                    changed = true;
                }
            }
        }

        Self { blocks }
    }

    /// Postorder of the CFG from the entry (successors before predecessors),
    /// with blocks unreachable from the entry appended in layout order. A
    /// backward analysis that visits blocks in this order propagates straight
    /// chains in one round and each loop level in one more, instead of one
    /// block per round.
    fn reverse_postorder(
        cfg: &ControlFlowGraph,
        label_to_idx: &FxHashMap<IrLabel, usize>,
    ) -> Vec<IrLabel> {
        let mut visited: FxHashSet<IrLabel> = set_with_capacity(cfg.blocks.len());
        let mut post: Vec<IrLabel> = Vec::with_capacity(cfg.blocks.len());
        // Seed with the entry plus every block, so unreachable blocks are covered
        // while the entry's reachable region still comes first.
        let mut seeds: Vec<IrLabel> = Vec::with_capacity(cfg.blocks.len() + 1);
        seeds.push(cfg.entry_label);
        for block in &cfg.blocks {
            seeds.push(block.label);
        }
        for seed in seeds {
            if visited.contains(&seed) {
                continue;
            }
            let mut stack: Vec<(IrLabel, bool)> = vec![(seed, false)];
            while let Some((lbl, expanded)) = stack.pop() {
                if expanded {
                    post.push(lbl);
                    continue;
                }
                if !visited.insert(lbl) {
                    continue;
                }
                let Some(&idx) = label_to_idx.get(&lbl) else {
                    continue;
                };
                stack.push((lbl, true));
                for succ in &cfg.blocks[idx].successors {
                    if !visited.contains(succ) {
                        stack.push((*succ, false));
                    }
                }
            }
        }
        post
    }

    pub fn is_live_in(&self, label: IrLabel, var: IrVar) -> bool {
        self.blocks
            .get(&label)
            .is_some_and(|b| b.live_in.contains(&var))
    }

    pub fn is_live_out(&self, label: IrLabel, var: IrVar) -> bool {
        self.blocks
            .get(&label)
            .is_some_and(|b| b.live_out.contains(&var))
    }

    pub fn compute_live_intervals(&self, cfg: &ControlFlowGraph) -> FxHashMap<IrVar, LiveInterval> {
        let mut intervals: FxHashMap<IrVar, (usize, usize)> = with_capacity(32);
        let mut pc = 0usize;

        for block in &cfg.blocks {
            // Variables live at block entry start at or before pc
            if let Some(info) = self.blocks.get(&block.label) {
                for &v in &info.live_in {
                    intervals
                        .entry(v)
                        .and_modify(|(_, end)| *end = (*end).max(pc))
                        .or_insert((pc, pc));
                }
            }

            for inst in &block.instructions {
                inst.for_each_use(|u| {
                    intervals
                        .entry(u)
                        .and_modify(|(_, end)| *end = (*end).max(pc))
                        .or_insert((pc, pc));
                });

                inst.for_each_def(|d| {
                    intervals
                        .entry(d)
                        .and_modify(|(_, end)| *end = (*end).max(pc))
                        .or_insert((pc, pc));
                });

                pc += 1;
            }

            // Variables live out of the block extend to the end of the block
            if let Some(info) = self.blocks.get(&block.label) {
                for &v in &info.live_out {
                    intervals
                        .entry(v)
                        .and_modify(|(_, end)| *end = (*end).max(pc))
                        .or_insert((pc, pc));
                }
            }
        }

        intervals
            .into_iter()
            .map(|(var, (start, end))| (var, LiveInterval { var, start, end }))
            .collect()
    }
}
