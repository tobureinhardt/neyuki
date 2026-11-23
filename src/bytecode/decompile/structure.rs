//! Nests a function's blocks into `if`, `while` and `for` statements.
//!
//! The walk follows the control flow from the entry. A test becomes an
//! `if` whose arms run up to the block where they meet again (the test's
//! post-dominator), a loop header becomes `while true do ... end` with
//! `break` and `continue` for the edges that leave or restart it, and the
//! loop instructions become `for` statements. A jump that none of these can
//! express (out of two loops at once, say) fails the function, which then
//! keeps its disassembly instead. A clean-up pass afterwards turns the
//! `while true` loops that test first into `while cond`, flattens `else`
//! arms after an arm that always leaves, and joins nested tests with `and`.

use super::FnCx;
use super::ast::{BinOp, Expr, Stmt, UnOp};
use super::flow::{Exit, Reg, access};
use super::lift::{End, lift};
use super::vars::VarId;
use crate::bytecode::instruction::Instruction;

struct Frame {
    header: usize,
    follow: Option<usize>,
    /// Where `follow` jumps on at once, when it is nothing but a jump: a
    /// jump there from the loop is a `break` too.
    through: Option<usize>,
    /// How many stops were active when the loop began; those belong to
    /// the code around the loop.
    stops: usize,
}

struct Walker<'c> {
    cx: &'c FnCx<'c>,
    loops: Vec<Frame>,
    /// Blocks where the arms of the `if`s being walked meet again.
    stops: Vec<usize>,
    budget: usize,
}

pub(super) fn structure(cx: &FnCx) -> Result<Vec<Stmt>, String> {
    let mut w = Walker {
        cx,
        loops: Vec::new(),
        stops: Vec::new(),
        // Blocks are walked again where arms do not meet; this bounds the
        // copies.
        budget: 8 * cx.flow.blocks.len() + 64,
    };
    let mut body = Vec::new();
    w.walk(0, &mut body)?;
    Ok(polish(body))
}

const TANGLED: &str = "a jump leaves several blocks at once";

impl Walker<'_> {
    fn walk(&mut self, mut b: usize, out: &mut Vec<Stmt>) -> Result<(), String> {
        loop {
            let barrier = self.loops.last().map_or(0, |f| f.stops);
            if self.stops.len() > barrier && self.stops.last() == Some(&b) {
                return Ok(());
            }
            if let Some(frame) = self.loops.last() {
                if frame.header == b {
                    out.push(Stmt::Continue);
                    return Ok(());
                }
                if frame.follow == Some(b) || frame.through == Some(b) {
                    out.push(Stmt::Break);
                    return Ok(());
                }
            }
            if self.stops.contains(&b)
                || self
                    .loops
                    .iter()
                    .any(|f| f.header == b || f.follow == Some(b))
            {
                return Err(TANGLED.to_string());
            }
            if self.budget == 0 {
                return Err("the control flow does not nest into blocks".to_string());
            }
            self.budget -= 1;
            let next = match self.cx.flow.header_loop[b] {
                Some(l) => self.while_loop(l, out)?,
                None => self.block(b, out)?,
            };
            match next {
                Some(n) => b = n,
                None => return Ok(()),
            }
        }
    }

    /// Emits block `b` and says where the code after it continues.
    fn block(&mut self, b: usize, out: &mut Vec<Stmt>) -> Result<Option<usize>, String> {
        let flow = &self.cx.flow;
        let exit = flow.blocks[b].exit;
        // Only an edge from outside the loop enters it; the body's own jump
        // back to the header is the next iteration.
        let tfor = match exit {
            Exit::Goto(to) if flow.header_loop[to].is_some_and(|l| !flow.in_loop(l, b)) => {
                self.tfor_call(to)
            }
            _ => None,
        };
        let lifted = lift(self.cx, b, tfor)?;
        out.extend(lifted.stmts);
        match (exit, lifted.end) {
            (Exit::Return, End::Return(vals)) => {
                out.push(Stmt::Return(vals));
                Ok(None)
            }
            (Exit::End, _) => {
                out.push(Stmt::Return(Vec::new()));
                Ok(None)
            }
            (Exit::Goto(to), End::ForIn(source)) => self.generic_for(to, source, out),
            (Exit::Goto(to), End::None) => Ok(Some(to)),
            (Exit::Branch { then, other }, End::Cond(cond)) => {
                self.branch(b, cond, then, other, out)
            }
            (Exit::ForPrep { header }, End::ForPrep(start, limit, step)) => {
                self.numeric_for(header, start, limit, step, out)
            }
            _ => Err("a block ends in an unexpected way".to_string()),
        }
    }

    fn branch(
        &mut self,
        b: usize,
        cond: Expr,
        then: usize,
        other: usize,
        out: &mut Vec<Stmt>,
    ) -> Result<Option<usize>, String> {
        let merge = self.cx.flow.ipdom[b];
        if let Some(m) = merge {
            self.stops.push(m);
        }
        let mut t = Vec::new();
        let mut e = Vec::new();
        let walked = self
            .walk(then, &mut t)
            .and_then(|()| self.walk(other, &mut e));
        if merge.is_some() {
            self.stops.pop();
        }
        walked?;
        out.push(Stmt::If(cond, t, e));
        Ok(merge)
    }

    /// Where a loop goes on when it ends: the target of its exit edges that
    /// best looks like the code after it.
    fn follow(&self, l: usize) -> Option<usize> {
        let flow = &self.cx.flow;
        let lp = &flow.loops[l];
        let mut exits: Vec<(usize, usize)> = Vec::new();
        for &u in &flow.rpo {
            if !lp.body[u] {
                continue;
            }
            for &v in &flow.blocks[u].succs {
                if !lp.body[v] {
                    exits.push((u, v));
                }
            }
        }
        // An exit to an enclosing loop's restart or end cannot be written
        // inside this loop, so it has to be this loop's end.
        if let Some(&(_, v)) = exits.iter().find(|(_, v)| {
            self.loops
                .iter()
                .any(|f| f.header == *v || f.follow == Some(*v))
        }) {
            return Some(v);
        }
        let mut best: Option<((bool, bool, usize, bool), usize)> = None;
        let mut targets: Vec<usize> = exits.iter().map(|(_, v)| *v).collect();
        targets.dedup();
        for v in targets {
            let from = |pred: &dyn Fn(usize) -> bool| exits.iter().any(|&(u, t)| t == v && pred(u));
            let score = (
                from(&|u| u == lp.header),
                from(&|u| lp.latches.contains(&u)),
                exits.iter().filter(|(_, t)| *t == v).count(),
                !matches!(flow.blocks[v].exit, Exit::Return | Exit::End),
            );
            let better = match &best {
                None => true,
                Some((s, cur)) => score > *s || (score == *s && flow.order[v] < flow.order[*cur]),
            };
            if better {
                best = Some((score, v));
            }
        }
        best.map(|(_, v)| v)
    }

    fn enter(&mut self, header: usize, follow: Option<usize>) {
        let flow = &self.cx.flow;
        let through = follow.and_then(|f| {
            let block = &flow.blocks[f];
            let only_jump = block.end == block.start + 1
                && matches!(
                    self.cx.proto.instructions[block.start],
                    Instruction::Jump { .. }
                );
            match block.exit {
                Exit::Goto(to) if only_jump && to != header => Some(to),
                _ => None,
            }
        });
        self.loops.push(Frame {
            header,
            follow,
            through,
            stops: self.stops.len(),
        });
    }

    fn while_loop(&mut self, l: usize, out: &mut Vec<Stmt>) -> Result<Option<usize>, String> {
        let header = self.cx.flow.loops[l].header;
        let follow = self.follow(l);
        let mut body = self.fresh_locals(l, &[], follow);
        self.enter(header, follow);
        let walked = match self.block(header, &mut body) {
            Ok(Some(next)) => self.walk(next, &mut body),
            Ok(None) => Ok(()),
            Err(err) => Err(err),
        };
        self.loops.pop();
        walked?;
        out.push(Stmt::While {
            cond: Expr::Bool(true),
            body,
        });
        Ok(follow)
    }

    /// The `TForCall` at the start of block `b`, when `b` is the header of a
    /// generic `for`.
    fn tfor_call(&self, b: usize) -> Option<usize> {
        let flow = &self.cx.flow;
        let code = &self.cx.proto.instructions;
        let block = &flow.blocks[b];
        if !matches!(block.exit, Exit::TForLoop { .. }) {
            return None;
        }
        (block.start..block.end)
            .find(|&pc| !matches!(code[pc], Instruction::CloseUpval { .. }))
            .filter(|&pc| matches!(code[pc], Instruction::TForCall { .. }))
    }

    /// Fails unless the blocks of loop `l` other than its header leave the
    /// loop's control registers `base..base + 3` alone.
    fn check_control(&self, l: usize, base: Reg) -> Result<(), String> {
        let flow = &self.cx.flow;
        let proto = self.cx.proto;
        let control = |r: Reg| r >= base && usize::from(r) < usize::from(base) + 3;
        for (b, block) in flow.blocks.iter().enumerate() {
            if b == flow.loops[l].header || !flow.in_loop(l, b) {
                continue;
            }
            for pc in block.start..block.end {
                let acc = access(&proto.instructions[pc], proto, flow.open.get(&pc).copied());
                if acc.reads.iter().chain(&acc.writes).any(|r| control(*r)) {
                    return Err("a for loop's body uses the loop's own registers".to_string());
                }
            }
        }
        Ok(())
    }

    fn numeric_for(
        &mut self,
        header: usize,
        start: Expr,
        limit: Expr,
        step: Expr,
        out: &mut Vec<Stmt>,
    ) -> Result<Option<usize>, String> {
        let flow = &self.cx.flow;
        let code = &self.cx.proto.instructions;
        let block = &flow.blocks[header];
        let shape = || "a numeric for loop has an unexpected shape".to_string();
        let last = block.end - 1;
        let Instruction::ForLoop { base, .. } = code[last] else {
            return Err(shape());
        };
        if (block.start..last).any(|pc| !matches!(code[pc], Instruction::CloseUpval { .. })) {
            return Err(shape());
        }
        let (Exit::ForLoop { body: first, exit }, Some(l)) = (block.exit, flow.header_loop[header])
        else {
            return Err(shape());
        };
        self.check_control(l, base)?;
        let var_reg = base.checked_add(3).ok_or_else(shape)?;
        let var = self.cx.vars.write(last, var_reg)?;
        let mut body = self.fresh_locals(l, &[var], Some(exit));
        self.enter(header, Some(exit));
        let walked = self.walk(first, &mut body);
        self.loops.pop();
        walked?;
        out.push(Stmt::NumFor {
            var,
            start,
            limit,
            step,
            body,
        });
        Ok(Some(exit))
    }

    fn generic_for(
        &mut self,
        header: usize,
        source: Expr,
        out: &mut Vec<Stmt>,
    ) -> Result<Option<usize>, String> {
        let flow = &self.cx.flow;
        let code = &self.cx.proto.instructions;
        let block = &flow.blocks[header];
        let shape = || "a generic for loop has an unexpected shape".to_string();
        let tpc = self.tfor_call(header).ok_or_else(shape)?;
        let Instruction::TForCall { base, retc } = code[tpc] else {
            return Err(shape());
        };
        if tpc + 2 != block.end || !(1..=2).contains(&retc) {
            return Err(shape());
        }
        let (Exit::TForLoop { body: first, exit }, Some(l)) =
            (block.exit, flow.header_loop[header])
        else {
            return Err(shape());
        };
        self.check_control(l, base)?;
        let mut vars = Vec::new();
        for i in 0..retc {
            let reg = base.checked_add(3 + i).ok_or_else(shape)?;
            vars.push(self.cx.vars.write(tpc, reg)?);
        }
        let mut body = self.fresh_locals(l, &vars, Some(exit));
        self.enter(header, Some(exit));
        let walked = self.walk(first, &mut body);
        self.loops.pop();
        walked?;
        out.push(Stmt::GenFor { vars, source, body });
        Ok(Some(exit))
    }

    /// `local x = nil` at the top of loop `l` for every captured variable
    /// the loop closes (`CloseUpval`) after each iteration, so closures made
    /// in different iterations get different variables, as in the source.
    /// `skip` are the loop's own variables, which the loop already declares.
    fn fresh_locals(&self, l: usize, skip: &[VarId], follow: Option<usize>) -> Vec<Stmt> {
        let flow = &self.cx.flow;
        let code = &self.cx.proto.instructions;
        let header = flow.loops[l].header;
        let mut regs = Vec::new();
        for (b, block) in flow.blocks.iter().enumerate() {
            if flow.innermost[b] != Some(l) {
                continue;
            }
            for inst in &code[block.start..block.end] {
                if let Instruction::CloseUpval { reg } = *inst
                    && !regs.contains(&reg)
                {
                    regs.push(reg);
                }
            }
        }
        regs.sort_unstable();
        let mut out = Vec::new();
        for reg in regs {
            let Some(var) = self.cx.vars.captured(reg) else {
                continue;
            };
            let carried = self.cx.vars.phi(header, reg) == Some(var);
            let used_after = follow.is_some_and(|f| flow.live_in[f].contains(reg));
            if !skip.contains(&var) && !carried && !used_after {
                out.push(Stmt::Local(vec![var], vec![Expr::Nil]));
            }
        }
        out
    }
}

// ---- clean-up -------------------------------------------------------------

fn negate(cond: Expr) -> Expr {
    match cond {
        Expr::Bin(BinOp::Eq, a, b) => Expr::Bin(BinOp::Ne, a, b),
        Expr::Bin(BinOp::Ne, a, b) => Expr::Bin(BinOp::Eq, a, b),
        // Only ever applied to conditions, where `not not x` tests the
        // same as `x`.
        Expr::Un(UnOp::Not, inner) => *inner,
        other => Expr::Un(UnOp::Not, Box::new(other)),
    }
}

/// Whether control never runs past the end of `stmts`.
fn leaves(stmts: &[Stmt]) -> bool {
    match stmts.last() {
        Some(Stmt::Return(_) | Stmt::Break | Stmt::Continue) => true,
        Some(Stmt::If(_, t, e)) => !e.is_empty() && leaves(t) && leaves(e),
        _ => false,
    }
}

/// Drops `continue` statements that end a loop's body.
fn strip_continue(body: &mut Vec<Stmt>) {
    match body.last_mut() {
        Some(Stmt::Continue) => {
            body.pop();
        }
        Some(Stmt::If(_, t, e)) => {
            strip_continue(t);
            strip_continue(e);
        }
        _ => {}
    }
}

/// Tidies the statements the walk produced.
fn polish(stmts: Vec<Stmt>) -> Vec<Stmt> {
    let mut out = Vec::with_capacity(stmts.len());
    let mut queue: std::collections::VecDeque<Stmt> = stmts.into();
    while let Some(stmt) = queue.pop_front() {
        match stmt {
            Stmt::If(cond, t, e) => {
                let mut cond = cond;
                let mut t = polish(t);
                let mut e = polish(e);
                if t.is_empty() && !e.is_empty() {
                    cond = negate(cond);
                    std::mem::swap(&mut t, &mut e);
                }
                if !e.is_empty() && leaves(&t) {
                    // `if c then leave else rest end` is `if c then leave end rest`.
                    for s in e.into_iter().rev() {
                        queue.push_front(s);
                    }
                    e = Vec::new();
                } else if !e.is_empty() && leaves(&e) {
                    cond = negate(cond);
                    for s in t.into_iter().rev() {
                        queue.push_front(s);
                    }
                    t = e;
                    e = Vec::new();
                }
                while e.is_empty()
                    && t.len() == 1
                    && matches!(&t[0], Stmt::If(_, _, inner) if inner.is_empty())
                {
                    let Some(Stmt::If(inner_cond, inner_then, _)) = t.pop() else {
                        break;
                    };
                    cond = Expr::bin(BinOp::And, cond, inner_cond);
                    t = inner_then;
                }
                out.push(Stmt::If(cond, t, e));
            }
            Stmt::While { cond, body } => {
                let mut cond = cond;
                let mut body = polish(body);
                strip_continue(&mut body);
                if cond == Expr::Bool(true)
                    && let Some(Stmt::If(_, t, e)) = body.first()
                    && e.is_empty()
                    && matches!(t.as_slice(), [Stmt::Break])
                    && let Stmt::If(test, _, _) = body.remove(0)
                {
                    cond = negate(test);
                }
                out.push(Stmt::While { cond, body });
            }
            Stmt::NumFor {
                var,
                start,
                limit,
                step,
                body,
            } => {
                let mut body = polish(body);
                strip_continue(&mut body);
                out.push(Stmt::NumFor {
                    var,
                    start,
                    limit,
                    step,
                    body,
                });
            }
            Stmt::GenFor { vars, source, body } => {
                let mut body = polish(body);
                strip_continue(&mut body);
                out.push(Stmt::GenFor { vars, source, body });
            }
            other => out.push(other),
        }
    }
    out
}
