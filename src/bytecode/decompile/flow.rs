//! Control flow of one function: the registers each instruction reads and
//! writes, its basic blocks, dominators, loops and the registers live into
//! each block.

use std::collections::HashMap;

use crate::bytecode::instruction::{Instruction, MULTRET};
use crate::bytecode::proto::Proto;

/// A register index.
pub(super) type Reg = u8;

/// Marks a node that no path reaches.
const NONE: usize = usize::MAX;

/// A set of registers.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct RegSet([u64; 4]);

impl RegSet {
    pub(super) fn insert(&mut self, reg: Reg) {
        self.0[usize::from(reg >> 6)] |= 1u64 << (reg & 63);
    }

    pub(super) fn contains(&self, reg: Reg) -> bool {
        self.0[usize::from(reg >> 6)] & (1u64 << (reg & 63)) != 0
    }

    /// Adds every register of `other`; true when that changed the set.
    fn union_with(&mut self, other: &RegSet) -> bool {
        let mut changed = false;
        for (word, extra) in self.0.iter_mut().zip(other.0) {
            let merged = *word | extra;
            changed |= merged != *word;
            *word = merged;
        }
        changed
    }

    fn minus(&self, other: &RegSet) -> RegSet {
        let mut out = *self;
        for (word, removed) in out.0.iter_mut().zip(other.0) {
            *word &= !removed;
        }
        out
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = Reg> + '_ {
        (0..=u8::MAX).filter(move |reg| self.contains(*reg))
    }
}

/// Registers `start..end`, clipped to the register file.
fn regs(start: usize, end: usize) -> impl Iterator<Item = Reg> {
    (start..end).filter_map(|reg| Reg::try_from(reg).ok())
}

/// The registers an instruction reads, in the order the source evaluated
/// them, and the ones it writes.
///
/// The loop instructions are described by what the loop statement means
/// rather than what the VM does: `ForPrep` and `TForCall` consume the
/// loop's control registers, `ForLoop` and `TForCall` define its variables,
/// and the rest of the loop machinery is invisible. The loop builders in
/// `structure` check that the body leaves the control registers alone.
#[derive(Default)]
pub(super) struct Access {
    pub(super) reads: Vec<Reg>,
    pub(super) writes: Vec<Reg>,
}

/// `open` is where the open list of values a MULTRET operand passes on
/// starts (see [`Flow::open`]).
pub(super) fn access(inst: &Instruction, proto: &Proto, open: Option<Reg>) -> Access {
    use Instruction as I;
    let mut acc = Access::default();
    match *inst {
        I::LoadNil { dst }
        | I::LoadBool { dst, .. }
        | I::LoadInt { dst, .. }
        | I::LoadK { dst, .. }
        | I::GetGlobal { dst, .. }
        | I::GetUpval { dst, .. }
        | I::NewTable { dst, .. }
        | I::GetImport { dst, .. } => acc.writes.push(dst),
        I::Move { dst, src }
        | I::Unm { dst, src }
        | I::Not { dst, src }
        | I::Len { dst, src }
        | I::BitNot { dst, src } => {
            acc.reads.push(src);
            acc.writes.push(dst);
        }
        I::SetGlobal { src, .. } | I::SetUpval { src, .. } => acc.reads.push(src),
        I::GetTable { dst, table, key } => {
            acc.reads.extend([table, key]);
            acc.writes.push(dst);
        }
        I::SetTable { table, key, val } => acc.reads.extend([table, key, val]),
        I::GetTableK { dst, table, .. } => {
            acc.reads.push(table);
            acc.writes.push(dst);
        }
        I::SetTableK { table, val, .. } => acc.reads.extend([table, val]),
        I::AppendArray { table, src } => acc.reads.extend([table, src]),
        I::Add { dst, a, b }
        | I::Sub { dst, a, b }
        | I::Mul { dst, a, b }
        | I::Div { dst, a, b }
        | I::IDiv { dst, a, b }
        | I::Mod { dst, a, b }
        | I::Pow { dst, a, b }
        | I::BitAnd { dst, a, b }
        | I::BitOr { dst, a, b }
        | I::BitXor { dst, a, b }
        | I::Shl { dst, a, b }
        | I::Shr { dst, a, b }
        | I::LShl { dst, a, b }
        | I::LShr { dst, a, b }
        | I::Concat { dst, a, b }
        | I::Coalesce { dst, a, b } => {
            acc.reads.extend([a, b]);
            acc.writes.push(dst);
        }
        I::AddK { dst, a, .. }
        | I::SubK { dst, a, .. }
        | I::MulK { dst, a, .. }
        | I::IDivK { dst, a, .. }
        | I::ModK { dst, a, .. }
        | I::AddI { dst, a, .. }
        | I::SubI { dst, a, .. }
        | I::MulI { dst, a, .. }
        | I::ShlI { dst, a, .. }
        | I::ShrI { dst, a, .. } => {
            acc.reads.push(a);
            acc.writes.push(dst);
        }
        I::Eq { a, b, .. }
        | I::Ne { a, b, .. }
        | I::Lt { a, b, .. }
        | I::Le { a, b, .. }
        | I::Gt { a, b, .. }
        | I::Ge { a, b, .. } => acc.reads.extend([a, b]),
        I::EqK { a, .. }
        | I::NeK { a, .. }
        | I::LtK { a, .. }
        | I::LeK { a, .. }
        | I::GtK { a, .. }
        | I::GeK { a, .. }
        | I::EqI { a, .. }
        | I::NeI { a, .. }
        | I::LtI { a, .. }
        | I::LeI { a, .. }
        | I::GtI { a, .. }
        | I::GeI { a, .. } => acc.reads.push(a),
        I::Test { reg, .. } => acc.reads.push(reg),
        I::Jump { .. } | I::TForLoop { .. } | I::CloseUpval { .. } => {}
        I::Call { callee, argc, retc } => {
            let first = usize::from(callee) + 1;
            let end = if argc == MULTRET {
                open.map_or(first, usize::from)
            } else {
                first + usize::from(argc)
            };
            acc.reads.push(callee);
            acc.reads.extend(regs(first, end));
            if retc != MULTRET {
                acc.writes.extend(regs(
                    usize::from(callee),
                    usize::from(callee) + usize::from(retc),
                ));
            }
        }
        I::Return { base, count } => {
            let end = if count == MULTRET {
                open.map_or(usize::from(base), usize::from)
            } else {
                usize::from(base) + usize::from(count)
            };
            acc.reads.extend(regs(usize::from(base), end));
        }
        I::Closure { dst, proto_idx } => {
            if let Some(child) = proto.protos.get(usize::from(proto_idx)) {
                for up in &child.upvalues {
                    if up.in_stack {
                        acc.reads.push(up.index);
                    }
                }
            }
            acc.writes.push(dst);
        }
        I::Vararg { dst, count } => {
            if count != 0 && count != MULTRET {
                acc.writes.extend(regs(
                    usize::from(dst),
                    usize::from(dst) + usize::from(count),
                ));
            }
        }
        I::ForPrep { base, .. } => {
            acc.reads
                .extend(regs(usize::from(base), usize::from(base) + 3));
        }
        I::ForLoop { base, .. } => acc
            .writes
            .extend(regs(usize::from(base) + 3, usize::from(base) + 4)),
        I::SetList { table, base, count } => {
            let end = if count == MULTRET {
                open.map_or(usize::from(base), usize::from)
            } else {
                usize::from(base) + usize::from(count)
            };
            acc.reads.push(table);
            acc.reads.extend(regs(usize::from(base), end));
        }
        I::TForCall { base, retc } => {
            let base = usize::from(base);
            acc.reads.extend(regs(base, base + 3));
            acc.writes
                .extend(regs(base + 3, base + 3 + usize::from(retc)));
        }
    }
    acc
}

/// The first register of the open list of values an instruction leaves for
/// the next MULTRET operand: a call that keeps all its results, or `...`.
pub(super) fn opens(inst: &Instruction) -> Option<Reg> {
    match *inst {
        Instruction::Call { callee, retc, .. } if retc == MULTRET => Some(callee),
        Instruction::Vararg { dst, count } if count == 0 || count == MULTRET => Some(dst),
        _ => None,
    }
}

/// Whether an instruction takes an open list of values as its last operand.
pub(super) fn takes_open(inst: &Instruction) -> bool {
    matches!(
        *inst,
        Instruction::Call { argc: MULTRET, .. }
            | Instruction::Return { count: MULTRET, .. }
            | Instruction::SetList { count: MULTRET, .. }
    )
}

fn ends_block(inst: &Instruction) -> bool {
    matches!(
        inst,
        Instruction::Jump { .. }
            | Instruction::Return { .. }
            | Instruction::ForPrep { .. }
            | Instruction::ForLoop { .. }
            | Instruction::TForLoop { .. }
    ) || inst.branch_offset().is_some()
}

/// How control leaves a block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Exit {
    Goto(usize),
    /// A test at the end of the block: `then` when it holds (the next
    /// instruction), `other` when it does not (the jump target).
    Branch {
        then: usize,
        other: usize,
    },
    /// A numeric `for` starts; `header` holds its `ForLoop`.
    ForPrep {
        header: usize,
    },
    ForLoop {
        body: usize,
        exit: usize,
    },
    TForLoop {
        body: usize,
        exit: usize,
    },
    Return,
    /// Runs off the end of the code.
    End,
}

pub(super) struct Block {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) exit: Exit,
    pub(super) succs: Vec<usize>,
    preds: Vec<usize>,
}

/// A natural loop: the blocks that reach a back edge to `header` without
/// passing through it.
pub(super) struct Loop {
    pub(super) header: usize,
    /// Indexed by block.
    pub(super) body: Vec<bool>,
    pub(super) latches: Vec<usize>,
    size: usize,
}

pub(super) struct Flow {
    pub(super) blocks: Vec<Block>,
    /// The block of every instruction.
    pub(super) block_of: Vec<usize>,
    /// Reachable blocks in reverse postorder.
    pub(super) rpo: Vec<usize>,
    /// Each block's position in `rpo` (`usize::MAX` when unreachable).
    pub(super) order: Vec<usize>,
    /// Immediate post-dominators in the graph without the loops' back
    /// edges; `None` is the function's exit.
    pub(super) ipdom: Vec<Option<usize>>,
    pub(super) loops: Vec<Loop>,
    /// The loop each block heads, if any.
    pub(super) header_loop: Vec<Option<usize>>,
    /// The innermost loop each block belongs to, if any.
    pub(super) innermost: Vec<Option<usize>>,
    pub(super) live_in: Vec<RegSet>,
    /// For every instruction with a MULTRET operand, where the open list of
    /// values it takes starts.
    pub(super) open: HashMap<usize, Reg>,
}

impl Flow {
    pub(super) fn analyze(proto: &Proto) -> Result<Flow, String> {
        let code = &proto.instructions;
        let n = code.len();
        if n == 0 {
            return Err("the function has no code".to_string());
        }
        let target = |pc: usize, offset: i16| -> Result<usize, String> {
            let to = pc as isize + 1 + isize::from(offset);
            usize::try_from(to)
                .ok()
                .filter(|to| *to < n)
                .ok_or_else(|| format!("instruction {} jumps outside the function", pc))
        };
        let jump_of = |inst: &Instruction| match *inst {
            Instruction::ForPrep { jump, .. }
            | Instruction::ForLoop { jump, .. }
            | Instruction::TForLoop { jump, .. } => Some(jump),
            _ => inst.branch_offset(),
        };

        let mut leader = vec![false; n];
        leader[0] = true;
        for (pc, inst) in code.iter().enumerate() {
            if let Some(offset) = jump_of(inst) {
                leader[target(pc, offset)?] = true;
            }
            if ends_block(inst) && pc + 1 < n {
                leader[pc + 1] = true;
            }
        }
        let starts: Vec<usize> = (0..n).filter(|pc| leader[*pc]).collect();
        let mut block_of = vec![0; n];
        for (b, &start) in starts.iter().enumerate() {
            let end = starts.get(b + 1).copied().unwrap_or(n);
            block_of[start..end].fill(b);
        }
        let next = |pc: usize| -> Result<usize, String> {
            if pc + 1 < n {
                Ok(block_of[pc + 1])
            } else {
                Err(format!("instruction {} runs off the end of the code", pc))
            }
        };

        let mut blocks = Vec::with_capacity(starts.len());
        for (b, &start) in starts.iter().enumerate() {
            let end = starts.get(b + 1).copied().unwrap_or(n);
            let last = end - 1;
            let inst = &code[last];
            let exit = match *inst {
                Instruction::Jump { offset } => Exit::Goto(block_of[target(last, offset)?]),
                Instruction::Return { .. } => Exit::Return,
                Instruction::ForPrep { jump, .. } => Exit::ForPrep {
                    header: block_of[target(last, jump)?],
                },
                Instruction::ForLoop { jump, .. } => Exit::ForLoop {
                    body: block_of[target(last, jump)?],
                    exit: next(last)?,
                },
                Instruction::TForLoop { jump, .. } => Exit::TForLoop {
                    body: next(last)?,
                    exit: block_of[target(last, jump)?],
                },
                _ => match inst.branch_offset() {
                    Some(offset) => Exit::Branch {
                        then: next(last)?,
                        other: block_of[target(last, offset)?],
                    },
                    None if end == n => Exit::End,
                    None => Exit::Goto(block_of[end]),
                },
            };
            let mut succs = match exit {
                Exit::Goto(to) | Exit::ForPrep { header: to } => vec![to],
                Exit::Branch { then, other } => vec![then, other],
                Exit::ForLoop { body, exit } | Exit::TForLoop { body, exit } => vec![body, exit],
                Exit::Return | Exit::End => Vec::new(),
            };
            succs.dedup();
            blocks.push(Block {
                start,
                end,
                exit,
                succs,
                preds: Vec::new(),
            });
        }
        for b in 0..blocks.len() {
            for s in blocks[b].succs.clone() {
                blocks[s].preds.push(b);
            }
        }

        let succs: Vec<Vec<usize>> = blocks.iter().map(|b| b.succs.clone()).collect();
        let preds: Vec<Vec<usize>> = blocks.iter().map(|b| b.preds.clone()).collect();
        let rpo = reverse_postorder(0, &succs);
        let mut order = vec![NONE; blocks.len()];
        for (i, &b) in rpo.iter().enumerate() {
            order[b] = i;
        }
        let idom = dominators(0, &rpo, &order, &preds);
        let dominates = |a: usize, b: usize| {
            let mut x = b;
            loop {
                if x == a {
                    return true;
                }
                let up = idom[x];
                if up == x || up == NONE {
                    return false;
                }
                x = up;
            }
        };

        // Every edge back up the order must be a back edge of a natural
        // loop; anything else cannot be written with `while` and `for`.
        let mut back = Vec::new();
        for &u in &rpo {
            for &v in &blocks[u].succs {
                if order[v] <= order[u] {
                    if !dominates(v, u) {
                        return Err("the control flow is irreducible".to_string());
                    }
                    back.push((u, v));
                }
            }
        }

        let mut loops: Vec<Loop> = Vec::new();
        let mut header_loop = vec![None; blocks.len()];
        for &(latch, header) in &back {
            let l = match header_loop[header] {
                Some(l) => l,
                None => {
                    loops.push(Loop {
                        header,
                        body: vec![false; blocks.len()],
                        latches: Vec::new(),
                        size: 0,
                    });
                    header_loop[header] = Some(loops.len() - 1);
                    loops.len() - 1
                }
            };
            loops[l].latches.push(latch);
        }
        for lp in &mut loops {
            lp.body[lp.header] = true;
            let mut work = lp.latches.clone();
            while let Some(b) = work.pop() {
                if lp.body[b] {
                    continue;
                }
                lp.body[b] = true;
                for &p in &blocks[b].preds {
                    if order[p] != NONE && !lp.body[p] {
                        work.push(p);
                    }
                }
            }
            lp.size = lp.body.iter().filter(|x| **x).count();
        }
        let mut innermost: Vec<Option<usize>> = vec![None; blocks.len()];
        for (l, lp) in loops.iter().enumerate() {
            for (slot, &in_body) in innermost.iter_mut().zip(&lp.body) {
                if in_body && slot.is_none_or(|cur| loops[cur].size > lp.size) {
                    *slot = Some(l);
                }
            }
        }

        let ipdom = post_dominators(&blocks, &rpo, &back);
        let open = open_lists(code, &blocks, &rpo)?;
        let live_in = liveness(proto, &blocks, &rpo, &open);

        Ok(Flow {
            blocks,
            block_of,
            rpo,
            order,
            ipdom,
            loops,
            header_loop,
            innermost,
            live_in,
            open,
        })
    }

    /// Whether block `x` belongs to loop `l`.
    pub(super) fn in_loop(&self, l: usize, x: usize) -> bool {
        self.loops[l].body[x]
    }
}

fn reverse_postorder(entry: usize, succs: &[Vec<usize>]) -> Vec<usize> {
    let mut visited = vec![false; succs.len()];
    let mut post = Vec::with_capacity(succs.len());
    let mut stack = vec![(entry, 0usize)];
    visited[entry] = true;
    while let Some(top) = stack.last_mut() {
        let (b, i) = *top;
        if i < succs[b].len() {
            top.1 += 1;
            let s = succs[b][i];
            if !visited[s] {
                visited[s] = true;
                stack.push((s, 0));
            }
        } else {
            post.push(b);
            stack.pop();
        }
    }
    post.reverse();
    post
}

/// Immediate dominators by Cooper, Harvey and Kennedy's iterative
/// algorithm; unreachable nodes get `NONE`.
fn dominators(entry: usize, rpo: &[usize], order: &[usize], preds: &[Vec<usize>]) -> Vec<usize> {
    let mut idom = vec![NONE; preds.len()];
    idom[entry] = entry;
    let mut changed = true;
    while changed {
        changed = false;
        for &b in rpo.iter().skip(1) {
            let mut new = NONE;
            for &p in &preds[b] {
                if idom[p] == NONE {
                    continue;
                }
                new = if new == NONE {
                    p
                } else {
                    intersect(&idom, order, p, new)
                };
            }
            if new != NONE && idom[b] != new {
                idom[b] = new;
                changed = true;
            }
        }
    }
    idom
}

fn intersect(idom: &[usize], order: &[usize], mut a: usize, mut b: usize) -> usize {
    while a != b {
        while order[a] > order[b] {
            a = idom[a];
        }
        while order[b] > order[a] {
            b = idom[b];
        }
    }
    a
}

/// Post-dominators over the reachable blocks plus a virtual exit, with
/// every back edge redirected to the exit so loops do not hide the point
/// where the two arms of an `if` meet again.
fn post_dominators(blocks: &[Block], rpo: &[usize], back: &[(usize, usize)]) -> Vec<Option<usize>> {
    let exit = blocks.len();
    let mut fwd: Vec<Vec<usize>> = vec![Vec::new(); exit + 1];
    for &u in rpo {
        for &v in &blocks[u].succs {
            let to = if back.contains(&(u, v)) { exit } else { v };
            if !fwd[u].contains(&to) {
                fwd[u].push(to);
            }
        }
        if fwd[u].is_empty() {
            fwd[u].push(exit);
        }
    }
    // The reverse graph: its successors are the forward predecessors.
    let mut rev: Vec<Vec<usize>> = vec![Vec::new(); exit + 1];
    for (u, outs) in fwd.iter().enumerate() {
        for &v in outs {
            rev[v].push(u);
        }
    }
    let rrpo = reverse_postorder(exit, &rev);
    let mut rorder = vec![NONE; exit + 1];
    for (i, &b) in rrpo.iter().enumerate() {
        rorder[b] = i;
    }
    let pdom = dominators(exit, &rrpo, &rorder, &fwd);
    (0..blocks.len())
        .map(|b| match pdom[b] {
            p if p == exit || p == NONE => None,
            p => Some(p),
        })
        .collect()
}

/// Pairs every MULTRET operand with the open list of values before it in
/// its block.
fn open_lists(
    code: &[Instruction],
    blocks: &[Block],
    rpo: &[usize],
) -> Result<HashMap<usize, Reg>, String> {
    let mut map = HashMap::new();
    for &b in rpo {
        let mut open = None;
        for (pc, inst) in code
            .iter()
            .enumerate()
            .take(blocks[b].end)
            .skip(blocks[b].start)
        {
            if takes_open(inst) {
                let start = open.take().ok_or_else(|| {
                    format!(
                        "instruction {} passes on values that nothing before it produced",
                        pc
                    )
                })?;
                map.insert(pc, start);
            }
            if let Some(start) = opens(inst) {
                open = Some(start);
            }
        }
    }
    Ok(map)
}

fn liveness(
    proto: &Proto,
    blocks: &[Block],
    rpo: &[usize],
    open: &HashMap<usize, Reg>,
) -> Vec<RegSet> {
    let code = &proto.instructions;
    let mut uses = vec![RegSet::default(); blocks.len()];
    let mut defs = vec![RegSet::default(); blocks.len()];
    for &b in rpo {
        let block = &blocks[b];
        for (pc, inst) in code.iter().enumerate().take(block.end).skip(block.start) {
            let acc = access(inst, proto, open.get(&pc).copied());
            for &r in &acc.reads {
                if !defs[b].contains(r) {
                    uses[b].insert(r);
                }
            }
            for &r in &acc.writes {
                defs[b].insert(r);
            }
        }
    }
    let mut live_in = uses.clone();
    let mut changed = true;
    while changed {
        changed = false;
        for &b in rpo.iter().rev() {
            let mut out = RegSet::default();
            for &s in &blocks[b].succs {
                out.union_with(&live_in[s]);
            }
            let mut new = uses[b];
            new.union_with(&out.minus(&defs[b]));
            if new != live_in[b] {
                live_in[b] = new;
                changed = true;
            }
        }
    }
    live_in
}
