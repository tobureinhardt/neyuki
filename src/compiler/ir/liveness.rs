//! Backward liveness analysis over the control flow graph.

use crate::compiler::ir::block::ControlFlowGraph;
use crate::compiler::ir::types::{IrLabel, IrVar};
use crate::vm::hash::{FxHashMap, FxHashSet, new_set, with_capacity};

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

        let num_blocks = cfg.blocks.len();
        let mut label_to_idx: FxHashMap<IrLabel, usize> = with_capacity(num_blocks);
        let mut block_infos: Vec<BlockLiveness> = Vec::with_capacity(num_blocks);

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

            block_infos.push(BlockLiveness {
                use_vars: block_use,
                def_vars: block_def,
                live_in: new_set(),
                live_out: new_set(),
            });
        }

        let succ_indices: Vec<Vec<usize>> = cfg
            .blocks
            .iter()
            .map(|b| {
                b.successors
                    .iter()
                    .filter_map(|s| label_to_idx.get(s).copied())
                    .collect()
            })
            .collect();

        // 2. Iterative backward dataflow analysis over dense block indices
        let order = Self::postorder_indices(cfg, &label_to_idx, &succ_indices);
        let mut scratch_out = new_set();
        let mut scratch_in = new_set();
        let mut changed = true;
        let mut iterations = 0;
        const MAX_ITERATIONS: usize = 1000;

        while changed && iterations < MAX_ITERATIONS {
            changed = false;
            iterations += 1;

            for &idx in &order {
                scratch_out.clear();
                for &succ_idx in &succ_indices[idx] {
                    scratch_out.extend(block_infos[succ_idx].live_in.iter().copied());
                }

                scratch_in.clear();
                {
                    let b = &block_infos[idx];
                    scratch_in.extend(b.use_vars.iter().copied());
                    for v in &scratch_out {
                        if !b.def_vars.contains(v) {
                            scratch_in.insert(*v);
                        }
                    }
                }

                let cur = &mut block_infos[idx];
                if cur.live_out != scratch_out {
                    std::mem::swap(&mut cur.live_out, &mut scratch_out);
                    changed = true;
                }
                if cur.live_in != scratch_in {
                    std::mem::swap(&mut cur.live_in, &mut scratch_in);
                    changed = true;
                }
            }
        }

        for (block, info) in cfg.blocks.iter().zip(block_infos) {
            blocks.insert(block.label, info);
        }

        Self { blocks }
    }

    fn postorder_indices(
        cfg: &ControlFlowGraph,
        label_to_idx: &FxHashMap<IrLabel, usize>,
        succ_indices: &[Vec<usize>],
    ) -> Vec<usize> {
        let n = cfg.blocks.len();
        let mut visited = vec![false; n];
        let mut post: Vec<usize> = Vec::with_capacity(n);
        let mut seeds: Vec<usize> = Vec::with_capacity(n + 1);
        if let Some(&entry_idx) = label_to_idx.get(&cfg.entry_label) {
            seeds.push(entry_idx);
        }
        seeds.extend(0..n);

        for seed in seeds {
            if visited[seed] {
                continue;
            }
            let mut stack: Vec<(usize, bool)> = vec![(seed, false)];
            while let Some((idx, expanded)) = stack.pop() {
                if expanded {
                    post.push(idx);
                    continue;
                }
                if visited[idx] {
                    continue;
                }
                visited[idx] = true;
                stack.push((idx, true));
                for &succ_idx in &succ_indices[idx] {
                    if !visited[succ_idx] {
                        stack.push((succ_idx, false));
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
