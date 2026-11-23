//! Variables: registers split into the separate values they hold.
//!
//! The register allocator reuses a register for unrelated values, so every
//! definition is joined with the reads it reaches (through the blocks the
//! register is live into) and each resulting group becomes one variable,
//! named after the local the debug information gives it. A register that a
//! closure captures stays one variable throughout, because the closure may
//! read or write it at any time.

use std::collections::{HashMap, HashSet};

use super::flow::{Flow, Reg, access};
use super::is_name;
use crate::bytecode::instruction::Instruction;
use crate::bytecode::proto::Proto;

pub(super) type VarId = usize;

pub(super) struct Var {
    pub(super) reg: Reg,
    /// Instructions that write it.
    pub(super) defs: Vec<usize>,
    /// Instructions that read it, once per read.
    pub(super) uses: Vec<usize>,
    /// Blocks it is live into.
    pub(super) phis: Vec<usize>,
    /// The parameter it starts as, if any.
    pub(super) param: Option<usize>,
    pub(super) captured: bool,
    pub(super) debug_name: Option<String>,
}

pub(super) struct Vars {
    pub(super) list: Vec<Var>,
    reads: HashMap<(usize, Reg), VarId>,
    writes: HashMap<(usize, Reg), VarId>,
    phis: HashMap<(usize, Reg), VarId>,
    params: Vec<VarId>,
    captured: HashMap<Reg, VarId>,
}

#[derive(Clone, Copy)]
enum Node {
    Phi(usize),
    Def(usize),
    Param(usize),
}

struct UnionFind(Vec<usize>);

impl UnionFind {
    fn add(&mut self) -> usize {
        self.0.push(self.0.len());
        self.0.len() - 1
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.0[x] != x {
            self.0[x] = self.0[self.0[x]];
            x = self.0[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.0[b] = a;
        }
    }
}

impl Vars {
    pub(super) fn build(proto: &Proto, flow: &Flow) -> Vars {
        let code = &proto.instructions;
        let mut uf = UnionFind(Vec::new());
        let mut nodes: Vec<(Reg, Node)> = Vec::new();
        let mut add = |uf: &mut UnionFind, reg: Reg, node: Node| {
            nodes.push((reg, node));
            uf.add()
        };
        let mut phi_nodes: HashMap<(usize, Reg), usize> = HashMap::new();
        let mut read_nodes: Vec<(usize, Reg, usize)> = Vec::new();
        let mut write_nodes: HashMap<(usize, Reg), usize> = HashMap::new();
        let mut param_nodes = Vec::new();
        let mut ends: HashMap<usize, Vec<Option<usize>>> = HashMap::new();

        for &b in &flow.rpo {
            let mut cur: Vec<Option<usize>> = vec![None; 256];
            for r in flow.live_in[b].iter() {
                let n = add(&mut uf, r, Node::Phi(b));
                phi_nodes.insert((b, r), n);
                cur[usize::from(r)] = Some(n);
            }
            if b == 0 {
                let params = usize::from(proto.num_params);
                for (i, slot) in cur.iter_mut().enumerate().take(params) {
                    let Ok(r) = Reg::try_from(i) else { break };
                    let n = add(&mut uf, r, Node::Param(i));
                    if let Some(phi) = *slot {
                        uf.union(n, phi);
                    }
                    *slot = Some(n);
                    param_nodes.push(n);
                }
            }
            let block = &flow.blocks[b];
            for (pc, inst) in code.iter().enumerate().take(block.end).skip(block.start) {
                let acc = access(inst, proto, flow.open.get(&pc).copied());
                for &r in &acc.reads {
                    let n = match cur[usize::from(r)] {
                        Some(n) => n,
                        None => {
                            let n = add(&mut uf, r, Node::Phi(b));
                            cur[usize::from(r)] = Some(n);
                            n
                        }
                    };
                    read_nodes.push((pc, r, n));
                }
                for &r in &acc.writes {
                    let n = add(&mut uf, r, Node::Def(pc));
                    write_nodes.insert((pc, r), n);
                    cur[usize::from(r)] = Some(n);
                }
            }
            ends.insert(b, cur);
        }
        for &b in &flow.rpo {
            for &s in &flow.blocks[b].succs {
                for r in flow.live_in[s].iter() {
                    if let (Some(x), Some(&y)) = (ends[&b][usize::from(r)], phi_nodes.get(&(s, r)))
                    {
                        uf.union(x, y);
                    }
                }
            }
        }

        let mut captured_regs = HashSet::new();
        for inst in code {
            if let Instruction::Closure { proto_idx, .. } = *inst
                && let Some(child) = proto.protos.get(usize::from(proto_idx))
            {
                for up in child.upvalues.iter().filter(|up| up.in_stack) {
                    captured_regs.insert(up.index);
                }
            }
        }
        let mut first_of_reg: HashMap<Reg, usize> = HashMap::new();
        for (n, &(reg, _)) in nodes.iter().enumerate() {
            if captured_regs.contains(&reg) {
                match first_of_reg.get(&reg) {
                    Some(&first) => uf.union(first, n),
                    None => {
                        first_of_reg.insert(reg, n);
                    }
                }
            }
        }

        let mut list: Vec<Var> = Vec::new();
        let mut var_of_root: HashMap<usize, VarId> = HashMap::new();
        let mut node_var = vec![0; nodes.len()];
        for (n, &(reg, node)) in nodes.iter().enumerate() {
            let root = uf.find(n);
            let len = list.len();
            let v = *var_of_root.entry(root).or_insert(len);
            if v == len {
                list.push(Var {
                    reg,
                    defs: Vec::new(),
                    uses: Vec::new(),
                    phis: Vec::new(),
                    param: None,
                    captured: captured_regs.contains(&reg),
                    debug_name: None,
                });
            }
            node_var[n] = v;
            match node {
                Node::Phi(b) => list[v].phis.push(b),
                Node::Def(pc) => list[v].defs.push(pc),
                Node::Param(i) => list[v].param = Some(i),
            }
        }
        let mut reads = HashMap::new();
        for &(pc, r, n) in &read_nodes {
            reads.insert((pc, r), node_var[n]);
            list[node_var[n]].uses.push(pc);
        }
        let writes = write_nodes
            .into_iter()
            .map(|(key, n)| (key, node_var[n]))
            .collect();
        let phis = phi_nodes
            .into_iter()
            .map(|(key, n)| (key, node_var[n]))
            .collect();
        let params = param_nodes.iter().map(|&n| node_var[n]).collect();
        let captured = list
            .iter()
            .enumerate()
            .filter(|(_, var)| var.captured)
            .map(|(v, var)| (var.reg, v))
            .collect();

        let mut vars = Vars {
            list,
            reads,
            writes,
            phis,
            params,
            captured,
        };
        vars.attach_debug_names(proto);
        vars
    }

    /// Gives each variable the local name whose range covers most of its
    /// reads and writes.
    fn attach_debug_names(&mut self, proto: &Proto) {
        let mut by_reg: HashMap<Reg, Vec<VarId>> = HashMap::new();
        for (v, var) in self.list.iter().enumerate() {
            by_reg.entry(var.reg).or_default().push(v);
        }
        let mut votes: Vec<Vec<(String, usize)>> = vec![Vec::new(); self.list.len()];
        for local in &proto.local_names {
            if !is_name(&local.name) || local.to_pc <= local.from_pc {
                continue;
            }
            let (from, to) = (local.from_pc as usize, local.to_pc as usize);
            // The register may be reused once the local is dead, so the
            // name goes to the one variable that matches the local best: the
            // one it starts with, then the one with most reads and writes
            // inside its range.
            let mut best: Option<((bool, usize), VarId)> = None;
            for &v in by_reg.get(&local.reg).into_iter().flatten() {
                let var = &self.list[v];
                let count = var
                    .defs
                    .iter()
                    .filter(|&&d| from <= d + 1 && d < to)
                    .count()
                    + var.uses.iter().filter(|&&u| from <= u && u < to).count();
                if count == 0 {
                    continue;
                }
                let starts =
                    (var.param.is_some() && from == 0) || var.defs.iter().any(|&d| d + 1 == from);
                let score = (starts, count);
                if best.as_ref().is_none_or(|(s, _)| score > *s) {
                    best = Some((score, v));
                }
            }
            if let Some(((_, count), v)) = best {
                match votes[v].iter_mut().find(|(name, _)| *name == local.name) {
                    Some(entry) => entry.1 += count,
                    None => votes[v].push((local.name.clone(), count)),
                }
            }
        }
        for (v, mut candidates) in votes.into_iter().enumerate() {
            // Highest count wins; among equals, the first one seen.
            candidates.reverse();
            self.list[v].debug_name = candidates
                .into_iter()
                .max_by_key(|(_, count)| *count)
                .map(|(name, _)| name);
        }
    }

    /// Unique names for every variable, avoiding `taken`: parameters first,
    /// then the rest in order of appearance. Variables without a debug name
    /// become `t1`, `t2`, ... and unnamed parameters `arg1`, `arg2`, ...
    pub(super) fn names(&self, taken: &HashSet<String>) -> Vec<String> {
        let mut used: HashSet<String> = HashSet::new();
        let mut names = vec![String::new(); self.list.len()];
        let first_pc = |var: &Var| {
            var.defs
                .iter()
                .chain(&var.uses)
                .min()
                .copied()
                .unwrap_or(usize::MAX)
        };
        let mut order: Vec<VarId> = (0..self.list.len()).collect();
        order.sort_by_key(|&v| {
            let var = &self.list[v];
            (
                var.param.is_none(),
                var.param.unwrap_or(0),
                first_pc(var),
                v,
            )
        });
        let mut temps = 0;
        for v in order {
            let var = &self.list[v];
            let free =
                |name: &str, used: &HashSet<String>| !taken.contains(name) && !used.contains(name);
            let name = match (&var.debug_name, var.param) {
                (Some(name), _) => unique(name, |n| free(n, &used)),
                (None, Some(i)) => unique(&format!("arg{}", i + 1), |n| free(n, &used)),
                (None, None) => loop {
                    temps += 1;
                    let name = format!("t{}", temps);
                    if free(&name, &used) {
                        break name;
                    }
                },
            };
            used.insert(name.clone());
            names[v] = name;
        }
        names
    }

    pub(super) fn read(&self, pc: usize, reg: Reg) -> Result<VarId, String> {
        self.reads.get(&(pc, reg)).copied().ok_or_else(|| {
            format!(
                "instruction {} reads register {} that was never set",
                pc, reg
            )
        })
    }

    pub(super) fn write(&self, pc: usize, reg: Reg) -> Result<VarId, String> {
        self.writes
            .get(&(pc, reg))
            .copied()
            .ok_or_else(|| format!("instruction {} has no result in register {}", pc, reg))
    }

    /// The variable `reg` holds on entry to `block`, if it is live there.
    pub(super) fn phi(&self, block: usize, reg: Reg) -> Option<VarId> {
        self.phis.get(&(block, reg)).copied()
    }

    pub(super) fn param(&self, i: usize) -> Option<VarId> {
        self.params.get(i).copied()
    }

    /// The variable of a register that closures capture.
    pub(super) fn captured(&self, reg: Reg) -> Option<VarId> {
        self.captured.get(&reg).copied()
    }
}

/// `base`, or `base_2`, `base_3`, ... when it is not free.
pub(super) fn unique(base: &str, free: impl Fn(&str) -> bool) -> String {
    if free(base) {
        return base.to_string();
    }
    (2..)
        .map(|k| format!("{}_{}", base, k))
        .find(|name| free(name))
        .unwrap_or_else(|| base.to_string())
}
