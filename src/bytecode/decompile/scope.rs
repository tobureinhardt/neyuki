//! Where each variable is declared.
//!
//! A variable whose first appearance is an assignment, with every later
//! appearance inside the same block, is declared by that assignment
//! (`local x = ...`). Any other variable is declared as `local x = nil` at
//! the top of the innermost block that holds all its appearances, moved out
//! of loops so a value carried from one iteration to the next survives.

use std::collections::{HashMap, HashSet};

use super::ast::{Expr, Stmt, Target};
use super::vars::{VarId, Vars};

struct BlockInfo {
    parent: Option<usize>,
    depth: usize,
    /// One past the position of its last statement.
    end: usize,
    in_loop: bool,
}

/// A declaration the code already has: `var` is declared for the
/// positions after `from` up to the end of `block`.
struct Cover {
    from: usize,
    block: usize,
}

struct AssignInfo {
    block: usize,
    targets: Vec<VarId>,
    reads: HashSet<VarId>,
    function: bool,
}

#[derive(Default)]
struct Scan {
    pos: usize,
    blocks: Vec<BlockInfo>,
    occurrences: HashMap<VarId, Vec<(usize, usize)>>,
    covers: HashMap<VarId, Vec<Cover>>,
    assigns: HashMap<usize, AssignInfo>,
}

impl Scan {
    fn occur(&mut self, var: VarId, pos: usize, block: usize) {
        self.occurrences.entry(var).or_default().push((pos, block));
    }

    fn expr(&mut self, e: &Expr, pos: usize, block: usize) {
        e.each_var(&mut |v| self.occur(v, pos, block));
    }

    fn block(
        &mut self,
        stmts: &[Stmt],
        parent: Option<usize>,
        in_loop: bool,
        declares: &[VarId],
    ) -> usize {
        let id = self.blocks.len();
        let depth = parent.map_or(0, |p| self.blocks[p].depth + 1);
        self.blocks.push(BlockInfo {
            parent,
            depth,
            end: 0,
            in_loop,
        });
        // Loop variables are declared by the loop for its body, which
        // starts right after the loop statement's position.
        let from = self.pos.saturating_sub(1);
        for &v in declares {
            self.covers
                .entry(v)
                .or_default()
                .push(Cover { from, block: id });
        }
        for stmt in stmts {
            let pos = self.pos;
            self.pos += 1;
            match stmt {
                Stmt::Local(vars, vals) => {
                    for v in vals {
                        self.expr(v, pos, id);
                    }
                    for &v in vars {
                        self.covers.entry(v).or_default().push(Cover {
                            from: pos,
                            block: id,
                        });
                    }
                }
                Stmt::Assign(targets, vals) => {
                    let mut vars = Vec::new();
                    let mut all_vars = true;
                    for t in targets {
                        match t {
                            Target::Var(v) => {
                                vars.push(*v);
                                self.occur(*v, pos, id);
                            }
                            Target::Index(a, b) => {
                                all_vars = false;
                                self.expr(a, pos, id);
                                self.expr(b, pos, id);
                            }
                            Target::Name(_) | Target::Global(_) => all_vars = false,
                        }
                    }
                    let mut reads = HashSet::new();
                    for v in vals {
                        self.expr(v, pos, id);
                        v.each_var(&mut |x| {
                            reads.insert(x);
                        });
                    }
                    if all_vars {
                        self.assigns.insert(
                            pos,
                            AssignInfo {
                                block: id,
                                targets: vars,
                                reads,
                                function: matches!(vals.as_slice(), [Expr::Function(_)]),
                            },
                        );
                    }
                }
                Stmt::Call(e) | Stmt::Discard(e) => self.expr(e, pos, id),
                Stmt::If(cond, then, other) => {
                    self.expr(cond, pos, id);
                    self.block(then, Some(id), in_loop, &[]);
                    self.block(other, Some(id), in_loop, &[]);
                }
                Stmt::While { cond, body } => {
                    self.expr(cond, pos, id);
                    self.block(body, Some(id), true, &[]);
                }
                Stmt::NumFor {
                    var,
                    start,
                    limit,
                    step,
                    body,
                } => {
                    self.expr(start, pos, id);
                    self.expr(limit, pos, id);
                    self.expr(step, pos, id);
                    self.block(body, Some(id), true, &[*var]);
                }
                Stmt::GenFor { vars, source, body } => {
                    self.expr(source, pos, id);
                    self.block(body, Some(id), true, vars);
                }
                Stmt::Return(vals) => {
                    for v in vals {
                        self.expr(v, pos, id);
                    }
                }
                Stmt::Break | Stmt::Continue => {}
            }
        }
        self.blocks[id].end = self.pos;
        id
    }

    fn covered(&self, var: VarId, pos: usize) -> bool {
        self.covers.get(&var).is_some_and(|covers| {
            covers
                .iter()
                .any(|c| pos > c.from && pos < self.blocks[c.block].end)
        })
    }

    fn common(&self, mut a: usize, mut b: usize) -> usize {
        while self.blocks[a].depth > self.blocks[b].depth {
            a = self.blocks[a].parent.unwrap_or(0);
        }
        while self.blocks[b].depth > self.blocks[a].depth {
            b = self.blocks[b].parent.unwrap_or(0);
        }
        while a != b {
            a = self.blocks[a].parent.unwrap_or(0);
            b = self.blocks[b].parent.unwrap_or(0);
        }
        a
    }
}

/// Adds the declarations to a function body. Parameters are declared by
/// the function itself.
pub(super) fn declare(body: &mut Vec<Stmt>, vars: &Vars) {
    let mut scan = Scan::default();
    scan.block(body, None, false, &[]);

    let mut candidates: HashMap<usize, Vec<VarId>> = HashMap::new();
    let mut undeclared: Vec<(VarId, Vec<(usize, usize)>)> = Vec::new();
    let mut ids: Vec<VarId> = scan.occurrences.keys().copied().collect();
    ids.sort_unstable();
    for v in ids {
        if vars.list[v].param.is_some() {
            continue;
        }
        let occurrences: Vec<(usize, usize)> = scan.occurrences[&v]
            .iter()
            .copied()
            .filter(|&(pos, _)| !scan.covered(v, pos))
            .collect();
        let Some(&(first, _)) = occurrences.iter().min_by_key(|(pos, _)| *pos) else {
            continue;
        };
        let eligible = scan.assigns.get(&first).is_some_and(|info| {
            let end = scan.blocks[info.block].end;
            info.targets.contains(&v)
                && (!info.reads.contains(&v) || (info.targets.len() == 1 && info.function))
                && occurrences.iter().all(|&(pos, _)| pos < end)
                && !(vars.list[v].captured && scan.blocks[info.block].in_loop)
        });
        if eligible {
            candidates.entry(first).or_default().push(v);
        }
        undeclared.push((v, occurrences));
    }

    // An assignment declares its targets only when it can declare all of
    // them.
    let mut converted: HashSet<usize> = HashSet::new();
    let mut declared: HashSet<VarId> = HashSet::new();
    for (pos, vs) in &candidates {
        let info = &scan.assigns[pos];
        if info.targets.iter().all(|t| vs.contains(t)) {
            converted.insert(*pos);
            declared.extend(info.targets.iter().copied());
        }
    }

    let mut decls: HashMap<usize, Vec<VarId>> = HashMap::new();
    for (v, occurrences) in undeclared {
        if declared.contains(&v) {
            continue;
        }
        let mut block = occurrences[0].1;
        for &(_, b) in &occurrences[1..] {
            block = scan.common(block, b);
        }
        while scan.blocks[block].in_loop {
            match scan.blocks[block].parent {
                Some(p) => block = p,
                None => break,
            }
        }
        decls.entry(block).or_default().push(v);
    }

    let mut pos = 0;
    let mut next_block = 0;
    apply(body, &converted, &decls, &mut pos, &mut next_block);
}

fn apply(
    stmts: &mut Vec<Stmt>,
    converted: &HashSet<usize>,
    decls: &HashMap<usize, Vec<VarId>>,
    pos: &mut usize,
    next_block: &mut usize,
) {
    let id = *next_block;
    *next_block += 1;
    for stmt in stmts.iter_mut() {
        let here = *pos;
        *pos += 1;
        if converted.contains(&here)
            && let Stmt::Assign(targets, vals) = stmt
        {
            let vars = targets
                .iter()
                .filter_map(|t| match t {
                    Target::Var(v) => Some(*v),
                    _ => None,
                })
                .collect();
            *stmt = Stmt::Local(vars, std::mem::take(vals));
        }
        match stmt {
            Stmt::If(_, then, other) => {
                apply(then, converted, decls, pos, next_block);
                apply(other, converted, decls, pos, next_block);
            }
            Stmt::While { body, .. } | Stmt::NumFor { body, .. } | Stmt::GenFor { body, .. } => {
                apply(body, converted, decls, pos, next_block);
            }
            _ => {}
        }
    }
    if let Some(vars) = decls.get(&id) {
        stmts.insert(0, Stmt::Local(vars.clone(), vec![Expr::Nil; vars.len()]));
    }
}
