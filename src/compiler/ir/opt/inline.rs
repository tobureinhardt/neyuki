//! Inlines small straight-line leaf functions.
//
// Soundness rules (all enforced below; anything else stays a call):
// - the body holds only frame-independent ops plus one trailing Return of
//   exactly one value: no calls, loops, branches, labels, varargs,
//   upvalues, spreads, or nested functions
// - the call site passes exactly `num_params` arguments and takes exactly
//   one value with a fixed `retc` (never MULTRET, which flows through the
//   stack top and cannot be spliced)
// - the callee resolves to a single Closure definition: directly, through
//   single-definition Move aliases, or through a single SetGlobal/GetGlobal
//   pair for one name

use crate::compiler::ir::block::{IrFunction, IrModule, UpvalSource};
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::types::{IrLabel, IrVar};
use crate::vm::hash::{FxHashMap, FxHashSet, new_map, new_set};

const MAX_INLINE_INSTRUCTIONS: usize = 32;

/// Frame-independent ops: relocating them into the caller preserves meaning.
fn is_inlinable_body_inst(inst: &IrInst) -> bool {
    matches!(
        inst,
        IrInst::Move { .. }
            | IrInst::BinOp { .. }
            | IrInst::UnOp { .. }
            | IrInst::LoadConst { .. }
            | IrInst::LoadNil { .. }
            | IrInst::NewTable { .. }
            | IrInst::GetTable { .. }
            | IrInst::GetImport { .. }
            | IrInst::SetTable { .. }
            | IrInst::GetGlobal { .. }
            | IrInst::SetGlobal { .. }
            | IrInst::AppendArray { .. }
            | IrInst::GetUpval { .. }
            | IrInst::SetUpval { .. }
            | IrInst::Label(_)
            | IrInst::Jump(_)
            | IrInst::JumpIfFalse { .. }
    )
}

fn build_candidates(func: &IrFunction) -> FxHashMap<usize, IrFunction> {
    func.protos
        .iter()
        .enumerate()
        .filter_map(|(idx, f)| {
            if is_inline_candidate(f) {
                Some((idx, f.clone()))
            } else {
                None
            }
        })
        .collect()
}

fn inline_func_tree(func: &mut IrFunction) -> bool {
    if func.protos.is_empty() {
        return false;
    }
    let mut changed = false;
    for proto in &mut func.protos {
        if inline_func_tree(proto) {
            changed = true;
        }
    }
    if !func
        .instructions
        .iter()
        .any(|inst| matches!(inst, IrInst::Call { .. }))
    {
        return changed;
    }
    let candidates = build_candidates(func);
    if !candidates.is_empty() && inline_in_func(func, &candidates) {
        changed = true;
    }
    changed
}

pub fn inline_functions(module: &mut IrModule) -> bool {
    let mut any_changed = false;
    const MAX_INLINE_ROUNDS: usize = 2;

    for _ in 0..MAX_INLINE_ROUNDS {
        let mut round_changed = false;
        if inline_func_tree(&mut module.main) {
            round_changed = true;
            any_changed = true;
        }

        if !round_changed {
            break;
        }
    }

    any_changed
}

fn inline_in_func(func: &mut IrFunction, candidates: &FxHashMap<usize, IrFunction>) -> bool {
    let mut def_count: FxHashMap<IrVar, usize> = new_map();
    let mut move_alias: FxHashMap<IrVar, IrVar> = new_map();
    let mut closure_map: FxHashMap<IrVar, usize> = new_map();
    let mut getglobal_name: FxHashMap<IrVar, String> = new_map();
    let mut setglobal_srcs: FxHashMap<String, Vec<IrVar>> = new_map();
    let mut max_var_id = 0u32;
    let mut max_label_id = 0usize;
    for inst in &func.instructions {
        inst.for_each_def(|d| {
            *def_count.entry(d).or_insert(0) += 1;
            max_var_id = max_var_id.max(d.0);
        });
        inst.for_each_use(|u| {
            max_var_id = max_var_id.max(u.0);
        });
        if let Some(l) = inst.jump_target() {
            max_label_id = max_label_id.max(l.0);
        }
        match inst {
            IrInst::Closure { dst, proto_idx, .. } => {
                closure_map.insert(*dst, *proto_idx as usize);
            }
            IrInst::Move { dst, src } => {
                move_alias.insert(*dst, *src);
            }
            IrInst::GetGlobal { dst, name } => {
                getglobal_name.insert(*dst, name.clone());
            }
            IrInst::SetGlobal { name, src } => {
                setglobal_srcs.entry(name.clone()).or_default().push(*src);
            }
            IrInst::Label(l) => {
                max_label_id = max_label_id.max(l.0);
            }
            _ => {}
        }
    }

    let mut changed = false;
    let mut new_instructions = Vec::new();

    for inst in func.instructions.drain(..) {
        let inlined = match &inst {
            IrInst::Call {
                dsts,
                callee,
                args,
                retc,
            } if dsts.len() == 1 && *retc == 1 => resolve_callee(
                *callee,
                &closure_map,
                &move_alias,
                &getglobal_name,
                &setglobal_srcs,
                &def_count,
            )
            .and_then(|idx| candidates.get(&idx))
            .filter(|f| f.num_params as usize == args.len())
            .map(|target| (target, dsts[0], args.clone())),
            _ => None,
        };

        if let Some((target_func, out_dst, call_args)) = inlined
            && splice_call(
                &mut new_instructions,
                target_func,
                out_dst,
                &call_args,
                &mut max_var_id,
                &mut max_label_id,
            )
        {
            changed = true;
        } else {
            new_instructions.push(inst);
        }
    }

    func.instructions = new_instructions;
    changed
}

/// Follows a variable to the Closure definition it must hold at runtime:
/// directly, through single-definition Move aliases, or through a
/// GetGlobal fed by a single SetGlobal of the same name. `visited` guards
/// against alias cycles. Returns the child proto index on success.
fn resolve_callee(
    mut var: IrVar,
    closure_map: &FxHashMap<IrVar, usize>,
    move_alias: &FxHashMap<IrVar, IrVar>,
    getglobal_name: &FxHashMap<IrVar, String>,
    setglobal_srcs: &FxHashMap<String, Vec<IrVar>>,
    def_count: &FxHashMap<IrVar, usize>,
) -> Option<usize> {
    let mut visited: FxHashSet<IrVar> = new_set();
    loop {
        if let Some(&proto_idx) = closure_map.get(&var) {
            return Some(proto_idx);
        }
        if !visited.insert(var) {
            return None;
        }
        // Single-definition Move alias.
        if def_count.get(&var).copied().unwrap_or(0) == 1
            && let Some(&src) = move_alias.get(&var)
        {
            var = src;
            continue;
        }
        // GetGlobal fed by exactly one SetGlobal of the same name.
        if def_count.get(&var).copied().unwrap_or(0) == 1
            && let Some(name) = getglobal_name.get(&var)
            && let Some(srcs) = setglobal_srcs.get(name)
            && let [src] = srcs.as_slice()
        {
            var = *src;
            continue;
        }
        return None;
    }
}

/// Splices one inlined call into `out`: parameters map to the call's
/// arguments, the single returned value moves to `out_dst`. Returns false
/// if any body instruction cannot be remapped, in which case the caller
/// keeps the original call and `out` is left with a partial splice that
/// must be discarded.
fn splice_call(
    out: &mut Vec<IrInst>,
    target_func: &IrFunction,
    out_dst: IrVar,
    call_args: &[IrVar],
    max_var_id: &mut u32,
    max_label_id: &mut usize,
) -> bool {
    let checkpoint = out.len();
    *max_label_id += 1;
    let exit_label = IrLabel(*max_label_id);

    let mut label_map: FxHashMap<IrLabel, IrLabel> = new_map();
    for inst in &target_func.instructions {
        if let IrInst::Label(l) = inst {
            *max_label_id += 1;
            label_map.insert(*l, IrLabel(*max_label_id));
        }
    }

    let mut var_map: FxHashMap<IrVar, IrVar> = new_map();
    for (i, &arg) in call_args.iter().enumerate() {
        var_map.insert(IrVar(i as u32), arg);
    }

    // The callee's line and local markers would misattribute the rest of
    // the caller's statement, so they are left behind.
    for body_inst in target_func
        .instructions
        .iter()
        .filter(|inst| !inst.is_debug_marker())
    {
        match body_inst {
            IrInst::Return(ret_vars) => {
                match ret_vars.first() {
                    Some(&ret_v) => {
                        let Some(&resolved_ret) = var_map.get(&ret_v) else {
                            out.truncate(checkpoint);
                            return false;
                        };
                        out.push(IrInst::Move {
                            dst: out_dst,
                            src: resolved_ret,
                        });
                    }
                    None => {
                        out.push(IrInst::LoadNil { dst: out_dst });
                    }
                }
                out.push(IrInst::Jump(exit_label));
            }
            IrInst::Label(l) => {
                let mapped = label_map.get(l).copied().unwrap_or(*l);
                out.push(IrInst::Label(mapped));
            }
            IrInst::Jump(target) => {
                let Some(&mapped) = label_map.get(target) else {
                    out.truncate(checkpoint);
                    return false;
                };
                out.push(IrInst::Jump(mapped));
            }
            IrInst::JumpIfFalse { cond, target } => {
                let mut c = *cond;
                if !use_mapped(&mut c, &var_map) {
                    out.truncate(checkpoint);
                    return false;
                }
                let Some(&mapped) = label_map.get(target) else {
                    out.truncate(checkpoint);
                    return false;
                };
                out.push(IrInst::JumpIfFalse {
                    cond: c,
                    target: mapped,
                });
            }
            IrInst::GetUpval { dst, index } => {
                if let Some(UpvalSource::ParentLocal(parent_var)) =
                    target_func.upvalue_vars.get(*index as usize)
                {
                    *max_var_id += 1;
                    let fresh = IrVar(*max_var_id);
                    var_map.insert(*dst, fresh);
                    out.push(IrInst::Move {
                        dst: fresh,
                        src: *parent_var,
                    });
                } else {
                    out.truncate(checkpoint);
                    return false;
                }
            }
            IrInst::SetUpval { src, index } => {
                if let Some(UpvalSource::ParentLocal(parent_var)) =
                    target_func.upvalue_vars.get(*index as usize)
                {
                    let mut s = *src;
                    if !use_mapped(&mut s, &var_map) {
                        out.truncate(checkpoint);
                        return false;
                    }
                    out.push(IrInst::Move {
                        dst: *parent_var,
                        src: s,
                    });
                } else {
                    out.truncate(checkpoint);
                    return false;
                }
            }
            _ => {
                let mut cloned = body_inst.clone();
                if !remap_inline_vars(&mut cloned, &mut var_map, max_var_id) {
                    out.truncate(checkpoint);
                    return false;
                }
                out.push(cloned);
            }
        }
    }
    out.push(IrInst::Label(exit_label));
    true
}

fn is_inline_candidate(func: &IrFunction) -> bool {
    let body: Vec<&IrInst> = func
        .instructions
        .iter()
        .filter(|inst| !inst.is_debug_marker())
        .collect();
    if body.len() > MAX_INLINE_INSTRUCTIONS {
        return false;
    }
    if !crate::compiler::cost_model::CostModel::default().should_inline_ir(func) {
        return false;
    }
    if func.is_vararg || !func.protos.is_empty() {
        return false;
    }
    for uv in &func.upvalue_vars {
        if !matches!(uv, UpvalSource::ParentLocal(_)) {
            return false;
        }
    }
    for inst in &body {
        match inst {
            IrInst::Return(_) => {}
            _ if is_inlinable_body_inst(inst) => {}
            _ => return false,
        }
    }
    if !body.iter().any(|inst| matches!(inst, IrInst::Return(_))) {
        return false;
    }

    // Reject backward jumps (loops): linear splice cannot reconstruct loop SSA.
    let mut label_indices: FxHashMap<IrLabel, usize> = new_map();
    for (idx, inst) in body.iter().enumerate() {
        if let IrInst::Label(l) = inst {
            label_indices.insert(*l, idx);
        }
    }
    for (idx, inst) in body.iter().enumerate() {
        if let Some(target) = inst.jump_target() {
            if let Some(&target_idx) = label_indices.get(&target) {
                if target_idx <= idx {
                    return false;
                }
            } else {
                return false;
            }
        }
    }

    // If branches/labels exist, mutable variables (assigned > 1 time) cannot be reconciled
    // across divergent paths by linear variable remapping without phi-nodes.
    let has_branches = body.iter().any(|inst| {
        matches!(
            inst,
            IrInst::Jump(_) | IrInst::JumpIfFalse { .. } | IrInst::Label(_)
        )
    });
    if has_branches {
        let mut def_counts: FxHashMap<IrVar, usize> = new_map();
        for i in 0..func.num_params {
            *def_counts.entry(IrVar(i as u32)).or_insert(0) += 1;
        }
        for inst in &body {
            inst.for_each_def(|d| {
                *def_counts.entry(d).or_insert(0) += 1;
            });
        }
        if def_counts.values().any(|&c| c > 1) {
            return false;
        }
    }

    true
}

/// Maps a use to its replacement; bails on uses that are neither parameters
/// nor remapped definitions (the body would not be self-contained).
fn use_mapped(v: &mut IrVar, var_map: &FxHashMap<IrVar, IrVar>) -> bool {
    match var_map.get(v).copied() {
        Some(mapped) => {
            *v = mapped;
            true
        }
        None => false,
    }
}

/// Mints a fresh variable for a definition so spliced code cannot collide
/// with caller variables.
fn fresh_def(v: IrVar, var_map: &mut FxHashMap<IrVar, IrVar>, max_id: &mut u32) -> IrVar {
    *max_id += 1;
    let fresh = IrVar(*max_id);
    var_map.insert(v, fresh);
    fresh
}

fn remap_inline_vars(
    inst: &mut IrInst,
    var_map: &mut FxHashMap<IrVar, IrVar>,
    max_id: &mut u32,
) -> bool {
    match inst {
        IrInst::Move { dst, src } => {
            if !use_mapped(src, var_map) {
                return false;
            }
            *dst = fresh_def(*dst, var_map, max_id);
        }
        IrInst::BinOp { dst, lhs, rhs, .. } => {
            if !use_mapped(lhs, var_map) || !use_mapped(rhs, var_map) {
                return false;
            }
            *dst = fresh_def(*dst, var_map, max_id);
        }
        IrInst::UnOp { dst, src, .. } => {
            if !use_mapped(src, var_map) {
                return false;
            }
            *dst = fresh_def(*dst, var_map, max_id);
        }
        IrInst::LoadConst { dst, .. } | IrInst::LoadNil { dst } => {
            *dst = fresh_def(*dst, var_map, max_id);
        }
        IrInst::NewTable { dst, .. } => {
            *dst = fresh_def(*dst, var_map, max_id);
        }
        IrInst::GetTable { dst, table, key } => {
            if !use_mapped(table, var_map) || !use_mapped(key, var_map) {
                return false;
            }
            *dst = fresh_def(*dst, var_map, max_id);
        }
        IrInst::SetTable { table, key, val } => {
            if !use_mapped(table, var_map) || !use_mapped(key, var_map) || !use_mapped(val, var_map)
            {
                return false;
            }
        }
        IrInst::GetGlobal { dst, .. } => {
            *dst = fresh_def(*dst, var_map, max_id);
        }
        IrInst::SetGlobal { src, .. } => {
            if !use_mapped(src, var_map) {
                return false;
            }
        }
        IrInst::AppendArray { table, src } => {
            if !use_mapped(table, var_map) || !use_mapped(src, var_map) {
                return false;
            }
        }
        _ => return false,
    }
    true
}
