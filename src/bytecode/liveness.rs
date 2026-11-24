//! Bytecode-level backward register liveness and loop allocation analysis.
//!
//! Used by [`crate::bytecode::exec::ExecProgram::compile`] to identify loops
//! that allocate heap values or invoke calls (`has_alloc == true`) and attach a
//! 256-bit `live_mask` of registers alive across each loop back-edge. Pure
//! numeric/scalar loops receive no safepoint and pay zero Beat overhead.

use crate::bytecode::instruction::{Instruction, MULTRET};
use crate::vm::hash::{FxHashMap, new_map};

/// Live-register bitmask across one allocating loop back-edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopSafepoint {
    pub loop_start: usize,
    pub live_mask: [u64; 4],
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct RegBits([u64; 4]);

impl RegBits {
    #[inline]
    fn set(&mut self, reg: u8) {
        let word = (reg as usize) >> 6;
        let bit = (reg as usize) & 63;
        self.0[word] |= 1u64 << bit;
    }

    #[inline]
    fn set_range(&mut self, start: u8, count: u8, max_regs: u8) {
        let limit = (start as usize + count as usize).min(max_regs as usize);
        for r in start as usize..limit {
            self.set(r as u8);
        }
    }

    #[inline]
    fn contains(self, reg: u8) -> bool {
        let word = (reg as usize) >> 6;
        let bit = (reg as usize) & 63;
        ((self.0[word] >> bit) & 1) != 0
    }

    #[inline]
    fn union_with(&mut self, other: Self) -> bool {
        let mut changed = false;
        for i in 0..4 {
            let next = self.0[i] | other.0[i];
            if next != self.0[i] {
                self.0[i] = next;
                changed = true;
            }
        }
        changed
    }

    #[inline]
    fn transfer(use_bits: Self, def_bits: Self, live_out: Self) -> Self {
        let mut out = RegBits([0; 4]);
        for i in 0..4 {
            out.0[i] = use_bits.0[i] | (live_out.0[i] & !def_bits.0[i]);
        }
        out
    }
}

/// Returns true if executing `inst` may allocate a Beat object or call user/native code.
#[inline]
fn inst_may_allocate_or_call(inst: Instruction) -> bool {
    matches!(
        inst,
        Instruction::NewTable { .. }
            | Instruction::Closure { .. }
            | Instruction::Concat { .. }
            | Instruction::Vararg { .. }
            | Instruction::Call { .. }
            | Instruction::TForCall { .. }
            | Instruction::SetList { .. }
            | Instruction::AppendArray { .. }
    )
}

/// Computes `uses` (over-approximated) and `defs` (under-approximated) for
/// sound backward register liveness on verified bytecode.
fn inst_use_def(inst: Instruction, max_regs: u8) -> (RegBits, RegBits) {
    let mut uses = RegBits::default();
    let mut defs = RegBits::default();
    match inst {
        Instruction::LoadNil { dst }
        | Instruction::LoadBool { dst, .. }
        | Instruction::LoadInt { dst, .. }
        | Instruction::LoadK { dst, .. }
        | Instruction::GetGlobal { dst, .. }
        | Instruction::GetUpval { dst, .. }
        | Instruction::GetImport { dst, .. }
        | Instruction::NewTable { dst, .. }
        | Instruction::Closure { dst, .. } => {
            defs.set(dst);
        }
        Instruction::Vararg { dst, count } => {
            if count != MULTRET {
                defs.set_range(dst, count, max_regs);
            }
        }
        Instruction::SetGlobal { src, .. } | Instruction::SetUpval { src, .. } => {
            uses.set(src);
        }
        Instruction::Move { dst, src }
        | Instruction::Unm { dst, src }
        | Instruction::Not { dst, src }
        | Instruction::Len { dst, src }
        | Instruction::BitNot { dst, src } => {
            uses.set(src);
            defs.set(dst);
        }
        Instruction::GetTable { dst, table, key } => {
            uses.set(table);
            uses.set(key);
            defs.set(dst);
        }
        Instruction::SetTable { table, key, val } => {
            uses.set(table);
            uses.set(key);
            uses.set(val);
        }
        Instruction::GetTableK { dst, table, .. } => {
            uses.set(table);
            defs.set(dst);
        }
        Instruction::SetTableK { table, val, .. }
        | Instruction::AppendArray { table, src: val } => {
            uses.set(table);
            uses.set(val);
        }
        Instruction::Add { dst, a, b }
        | Instruction::Sub { dst, a, b }
        | Instruction::Mul { dst, a, b }
        | Instruction::Div { dst, a, b }
        | Instruction::IDiv { dst, a, b }
        | Instruction::Mod { dst, a, b }
        | Instruction::Pow { dst, a, b }
        | Instruction::BitAnd { dst, a, b }
        | Instruction::BitOr { dst, a, b }
        | Instruction::BitXor { dst, a, b }
        | Instruction::Shl { dst, a, b }
        | Instruction::Shr { dst, a, b }
        | Instruction::LShl { dst, a, b }
        | Instruction::LShr { dst, a, b }
        | Instruction::Concat { dst, a, b }
        | Instruction::Coalesce { dst, a, b } => {
            uses.set(a);
            uses.set(b);
            defs.set(dst);
        }
        Instruction::AddK { dst, a, .. }
        | Instruction::SubK { dst, a, .. }
        | Instruction::MulK { dst, a, .. }
        | Instruction::IDivK { dst, a, .. }
        | Instruction::ModK { dst, a, .. }
        | Instruction::AddI { dst, a, .. }
        | Instruction::SubI { dst, a, .. }
        | Instruction::MulI { dst, a, .. }
        | Instruction::ShlI { dst, a, .. }
        | Instruction::ShrI { dst, a, .. } => {
            uses.set(a);
            defs.set(dst);
        }
        Instruction::Eq { a, b, .. }
        | Instruction::Ne { a, b, .. }
        | Instruction::Lt { a, b, .. }
        | Instruction::Le { a, b, .. }
        | Instruction::Gt { a, b, .. }
        | Instruction::Ge { a, b, .. } => {
            uses.set(a);
            uses.set(b);
        }
        Instruction::EqK { a, .. }
        | Instruction::NeK { a, .. }
        | Instruction::LtK { a, .. }
        | Instruction::LeK { a, .. }
        | Instruction::GtK { a, .. }
        | Instruction::GeK { a, .. }
        | Instruction::EqI { a, .. }
        | Instruction::NeI { a, .. }
        | Instruction::LtI { a, .. }
        | Instruction::LeI { a, .. }
        | Instruction::GtI { a, .. }
        | Instruction::GeI { a, .. }
        | Instruction::Test { reg: a, .. } => {
            uses.set(a);
        }
        Instruction::Jump { .. } | Instruction::CloseUpval { .. } => {}
        Instruction::Call { callee, argc, retc } => {
            if argc == MULTRET {
                uses.set_range(callee, max_regs.saturating_sub(callee), max_regs);
            } else {
                uses.set_range(callee, argc.saturating_add(1), max_regs);
            }
            if retc == 0 {
                defs.set(callee);
            } else if retc != MULTRET {
                defs.set_range(callee, retc, max_regs);
            }
        }
        Instruction::Return { base, count } => {
            if count == MULTRET {
                uses.set_range(base, max_regs.saturating_sub(base), max_regs);
            } else {
                uses.set_range(base, count, max_regs);
            }
        }
        Instruction::SetList { table, base, count } => {
            uses.set(table);
            if count == MULTRET {
                uses.set_range(base, max_regs.saturating_sub(base), max_regs);
            } else {
                uses.set_range(base, count, max_regs);
            }
        }
        Instruction::ForPrep { base, .. } => {
            uses.set_range(base, 3, max_regs);
            defs.set(base);
        }
        Instruction::ForLoop { base, .. } => {
            uses.set_range(base, 3, max_regs);
            defs.set(base);
            defs.set(base.saturating_add(3));
        }
        Instruction::TForCall { base, retc } => {
            uses.set_range(base, 3, max_regs);
            if retc != MULTRET {
                defs.set_range(base.saturating_add(3), retc, max_regs);
            }
        }
        Instruction::TForLoop { base, .. } => {
            if let Some(res) = base.checked_add(3) {
                uses.set(res);
            }
            if let Some(ctrl) = base.checked_add(2) {
                defs.set(ctrl);
            }
        }
    }
    (uses, defs)
}

#[inline]
fn branch_target(ip: usize, offset: i16, len: usize) -> Option<usize> {
    let t = ip as isize + 1 + offset as isize;
    (t >= 0 && (t as usize) < len).then_some(t as usize)
}

#[inline]
fn backedge_target(ip: usize, inst: Instruction, len: usize) -> Option<usize> {
    let offset = match inst {
        Instruction::ForLoop { jump, .. } | Instruction::TForLoop { jump, .. } if jump < 0 => jump,
        Instruction::Jump { offset } if offset < 0 => offset,
        _ => return None,
    };
    let target = branch_target(ip, offset, len)?;
    (target <= ip).then_some(target)
}

/// Analyzes `instructions` and returns a `LoopSafepoint` for every backward-branch
/// PC whose loop body may allocate or call functions.
pub fn analyze_loop_safepoints(
    instructions: &[Instruction],
    max_regs: u8,
) -> FxHashMap<usize, LoopSafepoint> {
    let n = instructions.len();
    if n == 0 {
        return new_map();
    }

    // Collect back-edges that span at least one allocating/calling op.
    let mut backedges = Vec::new();
    for (ip, &inst) in instructions.iter().enumerate() {
        if let Some(target) = backedge_target(ip, inst, n)
            && instructions[target..=ip]
                .iter()
                .copied()
                .any(inst_may_allocate_or_call)
        {
            backedges.push((ip, target));
        }
    }
    if backedges.is_empty() {
        return new_map();
    }

    // Build basic blocks over bytecode PCs.
    let mut is_leader = vec![false; n];
    is_leader[0] = true;
    for (ip, &inst) in instructions.iter().enumerate() {
        let mut mark_target = |off: i16| {
            if let Some(t) = branch_target(ip, off, n) {
                is_leader[t] = true;
            }
        };
        let is_term = match inst {
            Instruction::Jump { offset } => {
                mark_target(offset);
                true
            }
            Instruction::ForPrep { jump, .. }
            | Instruction::ForLoop { jump, .. }
            | Instruction::TForLoop { jump, .. } => {
                mark_target(jump);
                true
            }
            Instruction::Return { .. } => true,
            _ => {
                if let Some(off) = inst.branch_offset() {
                    mark_target(off);
                    true
                } else {
                    false
                }
            }
        };
        if is_term && ip + 1 < n {
            is_leader[ip + 1] = true;
        }
    }

    let mut block_starts = Vec::new();
    let mut pc_to_block = vec![0usize; n];
    for (ip, &leader) in is_leader.iter().enumerate() {
        if leader {
            block_starts.push(ip);
        }
        pc_to_block[ip] = block_starts.len() - 1;
    }

    let num_blocks = block_starts.len();
    let mut block_uses = vec![RegBits::default(); num_blocks];
    let mut block_defs = vec![RegBits::default(); num_blocks];
    let mut succs: Vec<Vec<usize>> = vec![Vec::new(); num_blocks];

    for b in 0..num_blocks {
        let start = block_starts[b];
        let end = if b + 1 < num_blocks {
            block_starts[b + 1]
        } else {
            n
        };
        let mut b_use = RegBits::default();
        let mut b_def = RegBits::default();
        for &inst in &instructions[start..end] {
            let (u, d) = inst_use_def(inst, max_regs);
            for w in 0..4 {
                b_use.0[w] |= u.0[w] & !b_def.0[w];
                b_def.0[w] |= d.0[w];
            }
        }
        block_uses[b] = b_use;
        block_defs[b] = b_def;

        let last_ip = end - 1;
        let last_inst = instructions[last_ip];
        match last_inst {
            Instruction::Return { .. } => {}
            Instruction::Jump { offset } => {
                if let Some(t) = branch_target(last_ip, offset, n) {
                    succs[b].push(pc_to_block[t]);
                }
            }
            Instruction::ForPrep { jump, .. } => {
                if let Some(t) = branch_target(last_ip, jump, n) {
                    succs[b].push(pc_to_block[t]);
                }
            }
            Instruction::ForLoop { jump, .. } | Instruction::TForLoop { jump, .. } => {
                if let Some(t) = branch_target(last_ip, jump, n) {
                    succs[b].push(pc_to_block[t]);
                }
                if end < n {
                    succs[b].push(pc_to_block[end]);
                }
            }
            _ => {
                if let Some(off) = last_inst.branch_offset()
                    && let Some(t) = branch_target(last_ip, off, n)
                {
                    succs[b].push(pc_to_block[t]);
                }
                if end < n {
                    succs[b].push(pc_to_block[end]);
                }
            }
        }
    }

    let mut live_in = vec![RegBits::default(); num_blocks];
    let mut live_out = vec![RegBits::default(); num_blocks];
    let mut changed = true;
    let mut iters = 0usize;
    while changed && iters < 512 {
        changed = false;
        iters += 1;
        for b in (0..num_blocks).rev() {
            let mut out = RegBits::default();
            for &s in &succs[b] {
                out.union_with(live_in[s]);
            }
            let inn = RegBits::transfer(block_uses[b], block_defs[b], out);
            if live_out[b] != out || live_in[b] != inn {
                live_out[b] = out;
                live_in[b] = inn;
                changed = true;
            }
        }
    }

    let mut result = new_map();
    for (back_ip, target_ip) in backedges {
        // Compute live registers right at `target_ip` (which is always a leader, so `block.live_in`),
        // plus ensure the loop control tuple of `ForLoop`/`TForLoop` stays marked live.
        let b = pc_to_block[target_ip];
        let mut mask = live_in[b];
        if let Instruction::ForLoop { base, .. } = instructions[back_ip] {
            mask.set_range(base, 3, max_regs);
        }
        // Conservative guard: if `iterations == 512` didn't converge, keep all registers live.
        if iters >= 512 {
            mask.set_range(0, max_regs, max_regs);
        }
        let _ = RegBits::contains;
        result.insert(
            back_ip,
            LoopSafepoint {
                loop_start: target_ip,
                live_mask: mask.0,
            },
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::{Options, compile_program};

    #[test]
    fn numeric_loop_without_allocs_has_zero_safepoints() {
        let proto = compile_program(
            "local sum = 0\nfor i = 1, 1000 do sum = sum + i end\nreturn sum",
            "num",
            Options::RUN,
        )
        .unwrap();
        let sp = analyze_loop_safepoints(&proto.instructions, proto.max_registers);
        assert!(
            sp.is_empty(),
            "pure numeric loop must emit zero Beat safepoints, got {:?}",
            sp
        );
    }

    #[test]
    fn allocating_for_loop_records_safepoint_with_live_mask_only() {
        let proto = compile_program(
            "local acc = 0\nfor i = 1, 10 do local t = { x = i }\nacc = acc + t.x end\nreturn acc",
            "alloc_for",
            Options::RUN,
        )
        .unwrap();
        let sp = analyze_loop_safepoints(&proto.instructions, proto.max_registers);
        assert!(
            !sp.is_empty(),
            "table-allocating for loop must produce a Beat safepoint"
        );
        let (_, info) = sp.iter().next().unwrap();
        let loop_for_ip = proto
            .instructions
            .iter()
            .position(|i| matches!(i, Instruction::ForLoop { .. }))
            .unwrap();
        let Instruction::ForLoop { base, .. } = proto.instructions[loop_for_ip] else {
            unreachable!();
        };
        let mask = RegBits(info.live_mask);
        assert!(mask.contains(base));
        assert!(mask.contains(base + 1));
        assert!(mask.contains(base + 2));
        // The temporary table created inside the loop must not appear as live
        // unless assigned to a register alive across iterations; at least one
        // register below `max_registers` (holding `{ x = i }`) is dead.
        let new_table_dst = proto.instructions[info.loop_start..=loop_for_ip]
            .iter()
            .find_map(|inst| match inst {
                Instruction::NewTable { dst, .. } => Some(*dst),
                _ => None,
            })
            .unwrap();
        assert!(
            !mask.contains(new_table_dst),
            "temporary table reg R{new_table_dst} should be dead across loop back-edge"
        );
    }

    #[test]
    fn allocating_while_loop_records_backward_jump_safepoint() {
        let proto = compile_program(
            "local i = 0\nlocal sum = 0\nwhile i < 10 do local t = { a = i }\nsum = sum + t.a\ni = i + 1 end\nreturn sum",
            "alloc_while",
            Options::RUN,
        )
        .unwrap();
        let sp = analyze_loop_safepoints(&proto.instructions, proto.max_registers);
        assert!(
            sp.keys().any(
                |&ip| matches!(proto.instructions[ip], Instruction::Jump { offset } if offset < 0)
            ),
            "allocating while loop must emit a JumpBeat safepoint"
        );
    }
}
