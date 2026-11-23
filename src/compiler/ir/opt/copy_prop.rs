//! Copy propagation: uses of a moved variable read the original instead.

use crate::compiler::ir::block::{ControlFlowGraph, IrModule};
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::types::IrVar;
use crate::vm::hash::{FxHashMap, FxHashSet, new_map, new_set};

pub fn copy_propagation(module: &mut IrModule) {
    let captured = new_set();
    propagate_copies_slice(&mut module.main.instructions, &captured);
}

pub fn copy_propagation_cfg(cfg: &mut ControlFlowGraph) -> bool {
    let captured = super::captured_vars(cfg);
    copy_propagation_cfg_with(cfg, &captured)
}

pub fn copy_propagation_cfg_with(cfg: &mut ControlFlowGraph, captured: &FxHashSet<IrVar>) -> bool {
    let mut changed = false;
    for block in &mut cfg.blocks {
        let block_changed = propagate_copies_slice(&mut block.instructions, captured);
        if block_changed {
            changed = true;
        }
        if block
            .instructions
            .iter()
            .any(|inst| matches!(inst, IrInst::Move { dst, src } if dst == src))
        {
            block
                .instructions
                .retain(|inst| !matches!(inst, IrInst::Move { dst, src } if dst == src));
            changed = true;
        }
    }
    changed
}

fn propagate_copies_slice(instructions: &mut [IrInst], captured: &FxHashSet<IrVar>) -> bool {
    let mut copies: FxHashMap<IrVar, IrVar> = new_map();
    let mut modified = false;

    for inst in instructions.iter_mut() {
        // Resolve uses using current copy map
        let replace_var = |v: &mut IrVar, copies: &FxHashMap<IrVar, IrVar>| -> bool {
            let mut cur = *v;
            let mut rep = false;
            while let Some(&src) = copies.get(&cur) {
                cur = src;
                rep = true;
            }
            if rep {
                *v = cur;
            }
            rep
        };

        if !copies.is_empty() {
            match inst {
                IrInst::Move { src, .. }
                | IrInst::UnOp { src, .. }
                | IrInst::AppendArray { src, .. } => {
                    if replace_var(src, &copies) {
                        modified = true;
                    }
                }
                IrInst::BinOp { lhs, rhs, .. } => {
                    if replace_var(lhs, &copies) {
                        modified = true;
                    }
                    if replace_var(rhs, &copies) {
                        modified = true;
                    }
                }
                IrInst::GetTable { table, key, .. } => {
                    if replace_var(table, &copies) {
                        modified = true;
                    }
                    if replace_var(key, &copies) {
                        modified = true;
                    }
                }
                IrInst::SetTable { table, key, val } => {
                    if replace_var(table, &copies) {
                        modified = true;
                    }
                    if replace_var(key, &copies) {
                        modified = true;
                    }
                    if replace_var(val, &copies) {
                        modified = true;
                    }
                }
                IrInst::SetGlobal { src, .. } | IrInst::SetUpval { src, .. } => {
                    if replace_var(src, &copies) {
                        modified = true;
                    }
                }
                IrInst::Call { callee, args, .. } => {
                    if replace_var(callee, &copies) {
                        modified = true;
                    }
                    for arg in args {
                        if replace_var(arg, &copies) {
                            modified = true;
                        }
                    }
                }
                IrInst::Return(vars) => {
                    for v in vars {
                        if replace_var(v, &copies) {
                            modified = true;
                        }
                    }
                }
                IrInst::JumpIfFalse { cond, .. } => {
                    if replace_var(cond, &copies) {
                        modified = true;
                    }
                }
                _ => {}
            }

            // Invalidate copies if destination or source is overwritten
            inst.for_each_def(|def| {
                copies.remove(&def);
                copies.retain(|_, src| *src != def);
            });
        }

        // Register new copy
        match inst {
            // A captured variable's register is what the closure reads, so
            // neither side of such a copy can be forwarded away.
            IrInst::Move { dst, src }
                if *dst != *src && !captured.contains(dst) && !captured.contains(src) =>
            {
                copies.insert(*dst, *src);
            }
            _ => {}
        }
    }

    modified
}
