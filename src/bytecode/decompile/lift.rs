//! The straight-line code of one block as statements, with the
//! temporaries the compiler introduced folded back into expressions.
//!
//! A value computed into a register that is read once, later in the same
//! block, is held back and written where it is read. A value whose
//! evaluation may have side effects (a call, a table read through a
//! metamethod, a global) is only moved like that when it stays in its
//! original order relative to the other side effects; otherwise it is
//! written out as an assignment first. Table constructors are rebuilt from
//! the stores that fill a new table.

use super::FnCx;
use super::ast::{BinOp, Body, Expr, Func, Item, Stmt, Target, UnOp};
use super::flow::Reg;
use super::is_name;
use super::vars::VarId;
use crate::bytecode::instruction::{Instruction, MULTRET};
use crate::bytecode::proto::Constant;

const OPEN_ERR: &str = "a variable number of values is passed across other work";

/// How a block ends, beyond its statements.
pub(super) enum End {
    /// Falls through or jumps.
    None,
    /// A test: the block continues at its `then` successor when it holds.
    Cond(Expr),
    Return(Vec<Expr>),
    /// A numeric `for` with its start, limit and step.
    ForPrep(Expr, Expr, Expr),
    /// A generic `for` over the expression.
    ForIn(Expr),
}

pub(super) struct Lifted {
    pub(super) stmts: Vec<Stmt>,
    pub(super) end: End,
}

enum Kind {
    /// The value of a variable.
    Value(VarId),
    /// A table constructor still collecting entries.
    Table {
        var: VarId,
        closed: bool,
        captured: bool,
    },
    /// Several results of one call, for a generic `for`.
    Multi(Vec<VarId>),
    /// An open list of values for the next MULTRET operand.
    Open,
}

struct Pending {
    kind: Kind,
    expr: Expr,
    /// Whether evaluating it may have side effects or see others'.
    effect: bool,
    /// Whether its one read may take the expression in place.
    foldable: bool,
    /// Creation order.
    seq: u64,
}

impl Pending {
    fn var(&self) -> Option<VarId> {
        match self.kind {
            Kind::Value(v) | Kind::Table { var: v, .. } => Some(v),
            _ => None,
        }
    }

    fn touches(&self, var: VarId) -> bool {
        self.var() == Some(var)
            || matches!(&self.kind, Kind::Multi(vars) if vars.contains(&var))
            || self.expr.mentions(var)
    }
}

#[derive(Clone, Copy)]
enum Src {
    Reg(Reg),
    /// The open list of values left by the previous call or `...`.
    Open,
    /// A table constructor being extended.
    Table(VarId),
}

/// Restrictions on what an operand may take in place.
#[derive(Clone, Copy, Default)]
struct Rules {
    /// This position is evaluated only sometimes (the right side of `??`),
    /// so it takes nothing with side effects.
    pure_at: Option<usize>,
    /// The last position spreads a call or `...` to all its values.
    no_multi_last: bool,
    /// No position may take a call or `...` (table constructor entries,
    /// one of which may end up last).
    no_multi: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Plan {
    Fold(usize),
    Var(VarId),
}

struct Lifter<'c> {
    cx: &'c FnCx<'c>,
    out: Vec<Stmt>,
    pending: Vec<Pending>,
    seq: u64,
    /// The `TForCall` this block feeds, when it jumps to a generic `for`.
    tfor: Option<usize>,
}

/// Lifts block `b`. `tfor` is the `TForCall` instruction of the generic
/// `for` the block starts, if it does.
pub(super) fn lift(cx: &FnCx, b: usize, tfor: Option<usize>) -> Result<Lifted, String> {
    let block = &cx.flow.blocks[b];
    let mut l = Lifter {
        cx,
        out: Vec::new(),
        pending: Vec::new(),
        seq: 0,
        tfor,
    };
    let code = &cx.proto.instructions;
    for (pc, inst) in code.iter().enumerate().take(block.end).skip(block.start) {
        if pc + 1 == block.end
            && let Some(end) = l.terminator(pc, inst)?
        {
            return Ok(Lifted { stmts: l.out, end });
        }
        l.inst(pc, inst)?;
    }
    // A block can also run straight into the loop it starts.
    let end = match l.tfor {
        Some(tpc) => End::ForIn(l.for_source(tpc)?),
        None => {
            l.flush_all()?;
            End::None
        }
    };
    Ok(Lifted { stmts: l.out, end })
}

fn bin_op(inst: &Instruction) -> Option<BinOp> {
    use Instruction as I;
    Some(match inst {
        I::Add { .. } | I::AddK { .. } | I::AddI { .. } => BinOp::Add,
        I::Sub { .. } | I::SubK { .. } | I::SubI { .. } => BinOp::Sub,
        I::Mul { .. } | I::MulK { .. } | I::MulI { .. } => BinOp::Mul,
        I::Div { .. } => BinOp::Div,
        I::IDiv { .. } | I::IDivK { .. } => BinOp::IDiv,
        I::Mod { .. } | I::ModK { .. } => BinOp::Mod,
        I::Pow { .. } => BinOp::Pow,
        I::BitAnd { .. } => BinOp::BitAnd,
        I::BitOr { .. } => BinOp::BitOr,
        I::BitXor { .. } => BinOp::BitXor,
        I::Shl { .. } | I::ShlI { .. } => BinOp::Shl,
        I::Shr { .. } | I::ShrI { .. } => BinOp::Shr,
        I::LShl { .. } => BinOp::LShl,
        I::LShr { .. } => BinOp::LShr,
        I::Concat { .. } => BinOp::Concat,
        I::Eq { .. } | I::EqK { .. } | I::EqI { .. } => BinOp::Eq,
        I::Ne { .. } | I::NeK { .. } | I::NeI { .. } => BinOp::Ne,
        I::Lt { .. } | I::LtK { .. } | I::LtI { .. } => BinOp::Lt,
        I::Le { .. } | I::LeK { .. } | I::LeI { .. } => BinOp::Le,
        I::Gt { .. } | I::GtK { .. } | I::GtI { .. } => BinOp::Gt,
        I::Ge { .. } | I::GeK { .. } | I::GeI { .. } => BinOp::Ge,
        _ => return None,
    })
}

/// Whether `inst` adds an entry to the table in `reg` without reading it
/// anywhere else.
fn fills_table(inst: &Instruction, reg: Reg) -> bool {
    match *inst {
        Instruction::SetTableK { table, val, .. } => table == reg && val != reg,
        Instruction::SetTable { table, key, val } => table == reg && key != reg && val != reg,
        Instruction::AppendArray { table, src } => table == reg && src != reg,
        Instruction::SetList { table, base, .. } => table == reg && base > reg,
        _ => false,
    }
}

impl Lifter<'_> {
    fn constant(&self, k: u16) -> Result<Expr, String> {
        Ok(match self.cx.proto.constants.get(usize::from(k)) {
            Some(Constant::Nil) => Expr::Nil,
            Some(Constant::Bool(b)) => Expr::Bool(*b),
            Some(Constant::Int(n)) => Expr::Int(n.clone()),
            Some(Constant::Float(x)) => Expr::Float(*x),
            Some(Constant::String(s)) => Expr::Str(s.clone()),
            None => return Err(format!("constant {} does not exist", k)),
        })
    }

    fn global(&self, k: u16) -> Result<String, String> {
        match self.cx.proto.constants.get(usize::from(k)) {
            Some(Constant::String(s)) if is_name(s) => Ok(s.clone()),
            _ => Err(format!("constant {} is not a global's name", k)),
        }
    }

    fn upvalue(&self, idx: u8) -> Result<String, String> {
        self.cx
            .upvalues
            .get(usize::from(idx))
            .cloned()
            .ok_or_else(|| format!("upvalue {} does not exist", idx))
    }

    fn captured(&self, var: VarId) -> bool {
        self.cx.vars.list[var].captured
    }

    // ---- the pending values -------------------------------------------

    fn push(&mut self, kind: Kind, expr: Expr, effect: bool, foldable: bool) {
        self.seq += 1;
        self.pending.push(Pending {
            kind,
            expr,
            effect,
            foldable,
            seq: self.seq,
        });
    }

    fn index_of(&self, var: VarId) -> Option<usize> {
        self.pending.iter().position(|p| p.var() == Some(var))
    }

    fn materialize(&mut self, p: Pending) -> Result<(), String> {
        match p.kind {
            Kind::Value(v) | Kind::Table { var: v, .. } => {
                self.out
                    .push(Stmt::Assign(vec![Target::Var(v)], vec![p.expr]));
            }
            Kind::Multi(vars) => {
                let targets = vars.into_iter().map(Target::Var).collect();
                self.out.push(Stmt::Assign(targets, vec![p.expr]));
            }
            Kind::Open => return Err(OPEN_ERR.to_string()),
        }
        Ok(())
    }

    /// Writes out the entries for which `emit` holds, in order, and keeps
    /// the rest.
    fn flush_where(&mut self, emit: impl Fn(usize, &Pending) -> bool) -> Result<(), String> {
        let mut keep = Vec::new();
        let mut out = Vec::new();
        for (i, p) in std::mem::take(&mut self.pending).into_iter().enumerate() {
            if emit(i, &p) {
                out.push(p);
            } else {
                keep.push(p);
            }
        }
        self.pending = keep;
        for p in out {
            self.materialize(p)?;
        }
        Ok(())
    }

    fn flush_all(&mut self) -> Result<(), String> {
        self.flush_where(|_, _| true)
    }

    /// Before a statement with side effects: every pending side effect
    /// happened before it.
    fn flush_effects(&mut self) -> Result<(), String> {
        self.flush_where(|_, p| p.effect)
    }

    /// Writes out entry `idx`, after the side effects that came before it.
    fn flush_through(&mut self, idx: usize) -> Result<(), String> {
        let seq = self.pending[idx].seq;
        self.flush_where(|i, p| i == idx || (p.effect && p.seq < seq))
    }

    /// Before `var` changes: writes out whatever still reads its old value.
    fn flush_for_write(&mut self, var: VarId) -> Result<(), String> {
        while let Some(idx) = self.pending.iter().position(|p| p.touches(var)) {
            self.flush_through(idx)?;
        }
        Ok(())
    }

    /// Whether the value written to `var` at `pc` can wait for its one read.
    fn foldable(&self, var: VarId, pc: usize) -> bool {
        let v = &self.cx.vars.list[var];
        if v.debug_name.is_some()
            || v.captured
            || v.param.is_some()
            || v.defs.len() != 1
            || v.uses.len() != 1
        {
            return false;
        }
        let u = v.uses[0];
        let flow = &self.cx.flow;
        (v.phis.is_empty() && u > pc && flow.block_of[u] == flow.block_of[pc])
            || self.tfor == Some(u)
    }

    /// Whether the new table in `var`, made at `pc`, is a temporary that is
    /// only filled and then read once, all in this block.
    fn table_foldable(&self, var: VarId, pc: usize) -> bool {
        let v = &self.cx.vars.list[var];
        if v.debug_name.is_some()
            || v.captured
            || v.param.is_some()
            || v.defs.len() != 1
            || !v.phis.is_empty()
        {
            return false;
        }
        let flow = &self.cx.flow;
        let code = &self.cx.proto.instructions;
        let Some((_, filling)) = v.uses.split_last() else {
            return false;
        };
        v.uses
            .iter()
            .all(|&u| u > pc && flow.block_of[u] == flow.block_of[pc])
            && filling.iter().all(|&u| fills_table(&code[u], v.reg))
    }

    /// The operands `srcs` read at `pc`, in evaluation order, with whether
    /// each may have side effects. Takes pending values in place where the
    /// rules and the order of side effects allow, and writes the others out
    /// first.
    fn take(&mut self, pc: usize, srcs: &[Src], rules: Rules) -> Result<Vec<(Expr, bool)>, String> {
        let mut vars = Vec::with_capacity(srcs.len());
        for src in srcs {
            vars.push(match *src {
                Src::Reg(r) => Some(self.cx.vars.read(pc, r)?),
                Src::Table(v) => Some(v),
                Src::Open => None,
            });
        }
        loop {
            let mut plan = Vec::with_capacity(srcs.len());
            let mut blocked = None;
            for (i, src) in srcs.iter().enumerate() {
                let step = match (*src, vars[i]) {
                    (Src::Open, _) => {
                        match self
                            .pending
                            .iter()
                            .rposition(|p| matches!(p.kind, Kind::Open))
                        {
                            Some(idx) => Ok(Plan::Fold(idx)),
                            None => return Err(OPEN_ERR.to_string()),
                        }
                    }
                    (Src::Table(var), _) => match self.index_of(var) {
                        Some(idx)
                            if matches!(
                                self.pending[idx].kind,
                                Kind::Table { closed: false, .. }
                            ) =>
                        {
                            Ok(Plan::Fold(idx))
                        }
                        Some(idx) => Err(idx),
                        None => Ok(Plan::Var(var)),
                    },
                    (Src::Reg(_), Some(var)) => match self.index_of(var) {
                        None => Ok(Plan::Var(var)),
                        Some(idx) => {
                            let p = &self.pending[idx];
                            let repeated = vars.iter().filter(|v| **v == Some(var)).count() > 1;
                            let multi = p.expr.is_multi()
                                && (rules.no_multi || (rules.no_multi_last && i + 1 == srcs.len()));
                            if p.foldable
                                && !repeated
                                && !multi
                                && !(rules.pure_at == Some(i) && p.effect)
                            {
                                Ok(Plan::Fold(idx))
                            } else {
                                Err(idx)
                            }
                        }
                    },
                    (Src::Reg(r), None) => return Err(format!("register {} has no variable", r)),
                };
                match step {
                    Ok(p) => plan.push(p),
                    Err(idx) => {
                        blocked = Some(idx);
                        break;
                    }
                }
            }
            if let Some(idx) = blocked {
                self.flush_through(idx)?;
                continue;
            }
            if !self.in_order(&plan) {
                self.flush_all()?;
                continue;
            }

            let mut folds: Vec<usize> = plan
                .iter()
                .filter_map(|p| match p {
                    Plan::Fold(i) => Some(*i),
                    Plan::Var(_) => None,
                })
                .collect();
            folds.sort_unstable_by(|a, b| b.cmp(a));
            let mut removed = Vec::with_capacity(folds.len());
            for i in folds {
                removed.push((i, self.pending.remove(i)));
            }
            let mut result = Vec::with_capacity(plan.len());
            for p in plan {
                match p {
                    Plan::Var(v) => result.push((Expr::Var(v), self.captured(v))),
                    Plan::Fold(i) => {
                        let pos = removed
                            .iter()
                            .position(|(j, _)| *j == i)
                            .ok_or_else(|| "an operand was used twice".to_string())?;
                        let (_, p) = removed.swap_remove(pos);
                        result.push((p.expr, p.effect));
                    }
                }
            }
            return Ok(result);
        }
    }

    /// Whether taking the planned values keeps every side effect in order:
    /// the ones taken must be the newest pending ones, read in the order
    /// they were made, and no captured variable may be read before one of
    /// them (it would be read before a call that might change it).
    fn in_order(&self, plan: &[Plan]) -> bool {
        let mut last = None;
        let mut oldest = None;
        let mut read_captured = false;
        for p in plan {
            match *p {
                Plan::Fold(i) if self.pending[i].effect => {
                    let seq = self.pending[i].seq;
                    if read_captured || last.is_some_and(|l| seq <= l) {
                        return false;
                    }
                    last = Some(seq);
                    oldest.get_or_insert(seq);
                }
                Plan::Fold(_) => {}
                Plan::Var(v) => read_captured |= self.captured(v),
            }
        }
        match oldest {
            Some(oldest) => self
                .pending
                .iter()
                .enumerate()
                .all(|(i, p)| !p.effect || p.seq < oldest || plan.contains(&Plan::Fold(i))),
            None => true,
        }
    }

    // ---- results ------------------------------------------------------

    /// Stores `expr` in register `reg` at `pc`.
    fn define(&mut self, pc: usize, reg: Reg, expr: Expr, effect: bool) -> Result<(), String> {
        let var = self.cx.vars.write(pc, reg)?;
        self.flush_for_write(var)?;
        if self.foldable(var, pc) {
            self.push(Kind::Value(var), expr, effect, true);
            return Ok(());
        }
        let cx = self.cx;
        let v = &cx.vars.list[var];
        if v.uses.is_empty() && v.debug_name.is_none() && !v.captured && v.param.is_none() {
            if effect {
                self.flush_effects()?;
                self.out.push(effect_stmt(expr));
            }
            return Ok(());
        }
        if effect || v.captured {
            self.flush_effects()?;
        }
        self.out
            .push(Stmt::Assign(vec![Target::Var(var)], vec![expr]));
        Ok(())
    }

    /// Stores the `n` values of `expr` in the registers from `base`.
    fn define_many(
        &mut self,
        pc: usize,
        base: Reg,
        n: u8,
        expr: Expr,
        effect: bool,
    ) -> Result<(), String> {
        let mut vars = Vec::with_capacity(usize::from(n));
        for i in 0..n {
            let reg = base
                .checked_add(i)
                .ok_or_else(|| "a result lands past the last register".to_string())?;
            vars.push(self.cx.vars.write(pc, reg)?);
        }
        for &v in &vars {
            self.flush_for_write(v)?;
        }
        // The source of the generic for this block feeds: its three results
        // go to the loop's registers, directly or through one copy each.
        let code = &self.cx.proto.instructions;
        let feeds = |tpc: usize, i: usize, var: &super::vars::Var| match var.uses.as_slice() {
            [u] if *u == tpc => true,
            [u] => {
                let block = self.cx.flow.block_of[pc];
                matches!((code[*u], code[tpc]), (Instruction::Move { dst, .. }, Instruction::TForCall { base, .. })
                    if usize::from(dst) == usize::from(base) + i && *u > pc && self.cx.flow.block_of[*u] == block)
            }
            _ => false,
        };
        if let Some(tpc) = self.tfor
            && n == 3
            && vars.iter().enumerate().all(|(i, &v)| {
                let var = &self.cx.vars.list[v];
                feeds(tpc, i, var)
                    && var.defs.len() == 1
                    && var.debug_name.is_none()
                    && !var.captured
            })
        {
            self.push(Kind::Multi(vars), expr, effect, true);
            return Ok(());
        }
        self.flush_effects()?;
        let targets = vars.into_iter().map(Target::Var).collect();
        self.out.push(Stmt::Assign(targets, vec![expr]));
        Ok(())
    }

    fn new_table(&mut self, pc: usize, dst: Reg) -> Result<(), String> {
        let var = self.cx.vars.write(pc, dst)?;
        self.flush_for_write(var)?;
        let cx = self.cx;
        let v = &cx.vars.list[var];
        if v.uses.is_empty() && v.debug_name.is_none() && !v.captured && v.param.is_none() {
            return Ok(());
        }
        let captured = v.captured;
        let foldable = self.table_foldable(var, pc);
        // A closure could look at a captured table while it is being
        // filled, so that one counts as a side effect.
        self.push(
            Kind::Table {
                var,
                closed: false,
                captured,
            },
            Expr::Table(Vec::new()),
            captured,
            foldable,
        );
        Ok(())
    }

    /// Adds the entries `make` builds from the operands `srcs` to the
    /// constructor of the table in `table`. Returns the operands instead,
    /// with the table first, when the table is not being constructed.
    fn fill(
        &mut self,
        pc: usize,
        table: Reg,
        srcs: &[Src],
        rules: Rules,
        close: bool,
        make: impl FnOnce(Vec<Expr>) -> Vec<Item>,
    ) -> Result<Option<Vec<(Expr, bool)>>, String> {
        let var = self.cx.vars.read(pc, table)?;
        let mut all = vec![Src::Table(var)];
        all.extend_from_slice(srcs);
        let shifted = Rules {
            pure_at: rules.pure_at.map(|i| i + 1),
            ..rules
        };
        if let Some(idx) = self.index_of(var)
            && let Kind::Table { captured: true, .. } = self.pending[idx].kind
        {
            // Nothing that could run a closure may happen while a captured
            // table is still unassigned.
            let mut risky = false;
            for src in srcs {
                match *src {
                    Src::Reg(r) => {
                        let v = self.cx.vars.read(pc, r)?;
                        risky |= self.captured(v)
                            || self.index_of(v).is_some_and(|i| self.pending[i].effect);
                    }
                    Src::Open | Src::Table(_) => risky = true,
                }
            }
            if risky {
                self.flush_through(idx)?;
            }
        }
        let foldable = self.index_of(var).map(|i| self.pending[i].foldable);
        let mut got = self.take(pc, &all, shifted)?;
        let (table_expr, table_effect) = got.remove(0);
        match (table_expr, foldable) {
            (Expr::Table(mut items), Some(foldable)) => {
                let effect = table_effect || got.iter().any(|(_, e)| *e);
                items.extend(make(got.into_iter().map(|(e, _)| e).collect()));
                let captured = self.captured(var);
                self.push(
                    Kind::Table {
                        var,
                        closed: close,
                        captured,
                    },
                    Expr::Table(items),
                    effect || captured,
                    foldable,
                );
                Ok(None)
            }
            (table_expr, _) => {
                got.insert(0, (table_expr, table_effect));
                Ok(Some(got))
            }
        }
    }

    // ---- instructions -------------------------------------------------

    fn inst(&mut self, pc: usize, inst: &Instruction) -> Result<(), String> {
        use Instruction as I;
        let none = Rules::default();
        match *inst {
            I::LoadNil { dst } => self.define(pc, dst, Expr::Nil, false),
            I::LoadBool { dst, val } => self.define(pc, dst, Expr::Bool(val), false),
            I::LoadInt { dst, val } => self.define(pc, dst, Expr::Int(val.into()), false),
            I::LoadK { dst, k } => {
                let value = self.constant(k)?;
                self.define(pc, dst, value, false)
            }
            I::Move { dst, src } => {
                // A copy of a held-back value into another temporary keeps
                // the value's place among the side effects.
                let from = self.cx.vars.read(pc, src)?;
                let to = self.cx.vars.write(pc, dst)?;
                if from != to
                    && self.foldable(to, pc)
                    && !self.pending.iter().any(|p| p.touches(to))
                    && let Some(idx) = self.index_of(from)
                    && self.pending[idx].foldable
                {
                    self.pending[idx].kind = Kind::Value(to);
                    return Ok(());
                }
                let (value, effect) = self.take(pc, &[Src::Reg(src)], none)?.remove(0);
                self.define(pc, dst, value, effect)
            }
            I::GetGlobal { dst, name_k } => {
                let name = self.global(name_k)?;
                self.define(pc, dst, Expr::Name(name), true)
            }
            I::SetGlobal { src, name_k } => {
                let name = self.global(name_k)?;
                let (value, _) = self.take(pc, &[Src::Reg(src)], none)?.remove(0);
                self.flush_effects()?;
                self.out
                    .push(Stmt::Assign(vec![Target::Global(name)], vec![value]));
                Ok(())
            }
            I::GetUpval { dst, upval_idx } => {
                let name = self.upvalue(upval_idx)?;
                self.define(pc, dst, Expr::Name(name), true)
            }
            I::SetUpval { src, upval_idx } => {
                let name = self.upvalue(upval_idx)?;
                let (value, _) = self.take(pc, &[Src::Reg(src)], none)?.remove(0);
                self.flush_effects()?;
                self.out
                    .push(Stmt::Assign(vec![Target::Name(name)], vec![value]));
                Ok(())
            }
            I::NewTable { dst, .. } => self.new_table(pc, dst),
            I::GetTable { dst, table, key } => {
                let mut got = self.take(pc, &[Src::Reg(table), Src::Reg(key)], none)?;
                let (k, _) = got.remove(1);
                let (t, _) = got.remove(0);
                self.define(pc, dst, Expr::index(t, k), true)
            }
            I::GetTableK { dst, table, key_k } => {
                let key = self.constant(key_k)?;
                let (t, _) = self.take(pc, &[Src::Reg(table)], none)?.remove(0);
                self.define(pc, dst, Expr::index(t, key), true)
            }
            I::GetImport {
                dst,
                mod_k,
                field_k,
                ..
            } => {
                let module = self.global(mod_k)?;
                let field = self.constant(field_k)?;
                self.define(pc, dst, Expr::index(Expr::Name(module), field), true)
            }
            I::SetTable { table, key, val } => {
                let srcs = [Src::Reg(key), Src::Reg(val)];
                let filled = self.fill(pc, table, &srcs, none, false, |mut vals| {
                    let v = vals.pop().unwrap_or(Expr::Nil);
                    let k = vals.pop().unwrap_or(Expr::Nil);
                    vec![Item::Keyed(k, v)]
                })?;
                if let Some(mut got) = filled {
                    let (v, _) = got.remove(2);
                    let (k, _) = got.remove(1);
                    let (t, _) = got.remove(0);
                    self.flush_effects()?;
                    self.out
                        .push(Stmt::Assign(vec![Target::Index(t, k)], vec![v]));
                }
                Ok(())
            }
            I::SetTableK { table, key_k, val } => {
                let key = self.constant(key_k)?;
                let item_key = key.clone();
                let filled = self.fill(pc, table, &[Src::Reg(val)], none, false, |mut vals| {
                    vec![Item::Keyed(item_key, vals.pop().unwrap_or(Expr::Nil))]
                })?;
                if let Some(mut got) = filled {
                    let (v, _) = got.remove(1);
                    let (t, _) = got.remove(0);
                    self.flush_effects()?;
                    self.out
                        .push(Stmt::Assign(vec![Target::Index(t, key)], vec![v]));
                }
                Ok(())
            }
            I::AppendArray { table, src } => {
                let rules = Rules {
                    no_multi: true,
                    ..none
                };
                let filled = self.fill(pc, table, &[Src::Reg(src)], rules, false, |vals| {
                    vals.into_iter().map(Item::Pos).collect()
                })?;
                if let Some(got) = filled {
                    self.insert_each(got)?;
                }
                Ok(())
            }
            I::SetList { table, base, count } => {
                let end = if count == MULTRET {
                    self.open_start(pc)?
                } else {
                    usize::from(base) + usize::from(count)
                };
                let mut srcs: Vec<Src> = (usize::from(base)..end)
                    .filter_map(|r| Reg::try_from(r).ok())
                    .map(Src::Reg)
                    .collect();
                if count == MULTRET {
                    srcs.push(Src::Open);
                }
                let rules = Rules {
                    no_multi: count != MULTRET,
                    no_multi_last: true,
                    ..none
                };
                let close = count == MULTRET;
                let filled = self.fill(pc, table, &srcs, rules, close, |vals| {
                    vals.into_iter().map(Item::Pos).collect()
                })?;
                if let Some(got) = filled {
                    if count == MULTRET {
                        return Err(
                            "a variable number of values is added to a finished table".to_string()
                        );
                    }
                    self.insert_each(got)?;
                }
                Ok(())
            }
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
            | I::Concat { dst, a, b } => {
                let op = bin_op(inst).ok_or("unknown operator")?;
                let mut got = self.take(pc, &[Src::Reg(a), Src::Reg(b)], none)?;
                let (y, _) = got.remove(1);
                let (x, _) = got.remove(0);
                self.define(pc, dst, Expr::bin(op, x, y), true)
            }
            I::AddK { dst, a, k }
            | I::SubK { dst, a, k }
            | I::MulK { dst, a, k }
            | I::IDivK { dst, a, k }
            | I::ModK { dst, a, k } => {
                let op = bin_op(inst).ok_or("unknown operator")?;
                let y = self.constant(k)?;
                let (x, _) = self.take(pc, &[Src::Reg(a)], none)?.remove(0);
                self.define(pc, dst, Expr::bin(op, x, y), true)
            }
            I::AddI { dst, a, imm } | I::SubI { dst, a, imm } | I::MulI { dst, a, imm } => {
                let op = bin_op(inst).ok_or("unknown operator")?;
                let (x, _) = self.take(pc, &[Src::Reg(a)], none)?.remove(0);
                self.define(pc, dst, Expr::bin(op, x, Expr::Int(imm.into())), true)
            }
            I::ShlI { dst, a, imm } | I::ShrI { dst, a, imm } => {
                let op = bin_op(inst).ok_or("unknown operator")?;
                let (x, _) = self.take(pc, &[Src::Reg(a)], none)?.remove(0);
                self.define(pc, dst, Expr::bin(op, x, Expr::Int(imm.into())), true)
            }
            I::Coalesce { dst, a, b } => {
                // `??` evaluates its right side only for nil, but the
                // instruction has both values ready: only a value without
                // side effects may move into the right side.
                let rules = Rules {
                    pure_at: Some(1),
                    ..none
                };
                let mut got = self.take(pc, &[Src::Reg(a), Src::Reg(b)], rules)?;
                let (y, ey) = got.remove(1);
                let (x, ex) = got.remove(0);
                self.define(pc, dst, Expr::bin(BinOp::Coalesce, x, y), ex || ey)
            }
            I::Unm { dst, src }
            | I::Not { dst, src }
            | I::Len { dst, src }
            | I::BitNot { dst, src } => {
                let (op, own_effect) = match *inst {
                    I::Unm { .. } => (UnOp::Neg, true),
                    I::Not { .. } => (UnOp::Not, false),
                    I::Len { .. } => (UnOp::Len, true),
                    _ => (UnOp::BitNot, true),
                };
                let (x, effect) = self.take(pc, &[Src::Reg(src)], none)?.remove(0);
                self.define(pc, dst, Expr::Un(op, Box::new(x)), effect || own_effect)
            }
            I::Call { callee, argc, retc } => {
                let first = usize::from(callee) + 1;
                let end = if argc == MULTRET {
                    self.open_start(pc)?
                } else {
                    first + usize::from(argc)
                };
                let mut srcs = vec![Src::Reg(callee)];
                srcs.extend(
                    (first..end)
                        .filter_map(|r| Reg::try_from(r).ok())
                        .map(Src::Reg),
                );
                if argc == MULTRET {
                    srcs.push(Src::Open);
                }
                let rules = Rules {
                    no_multi_last: argc != MULTRET && argc > 0,
                    ..none
                };
                let mut got = self.take(pc, &srcs, rules)?;
                let (f, _) = got.remove(0);
                let call = self.make_call(f, got.into_iter().map(|(e, _)| e).collect());
                match retc {
                    0 => {
                        self.flush_effects()?;
                        self.out.push(Stmt::Call(call));
                        Ok(())
                    }
                    1 => self.define(pc, callee, call, true),
                    MULTRET => {
                        self.push(Kind::Open, call, true, true);
                        Ok(())
                    }
                    n => self.define_many(pc, callee, n, call, true),
                }
            }
            I::Closure { dst, proto_idx } => {
                let cx = self.cx;
                let child = cx
                    .proto
                    .protos
                    .get(usize::from(proto_idx))
                    .ok_or_else(|| format!("function {} does not exist", proto_idx))?;
                let mut captures = Vec::new();
                let mut upvalues = Vec::new();
                for up in &child.upvalues {
                    if up.in_stack {
                        let v = self.cx.vars.read(pc, up.index)?;
                        captures.push(v);
                        upvalues.push(self.cx.names[v].clone());
                    } else {
                        upvalues.push(self.upvalue(up.index)?);
                    }
                }
                let mut func = super::decompile_function(child, upvalues, &cx.child_taken);
                func.captures = captures;
                self.define(pc, dst, Expr::Function(Box::new(func)), false)
            }
            I::Vararg { dst, count } => match count {
                0 | MULTRET => {
                    self.push(Kind::Open, Expr::Vararg, false, true);
                    Ok(())
                }
                1 => self.define(pc, dst, Expr::Vararg, false),
                n => self.define_many(pc, dst, n, Expr::Vararg, false),
            },
            I::CloseUpval { .. } => Ok(()),
            I::TForCall { .. } | I::ForLoop { .. } | I::TForLoop { .. } | I::ForPrep { .. } => {
                Err("a loop instruction is in an unexpected place".to_string())
            }
            _ => Err(format!("instruction {} ends its block too early", pc)),
        }
    }

    fn open_start(&self, pc: usize) -> Result<usize, String> {
        self.cx
            .flow
            .open
            .get(&pc)
            .map(|r| usize::from(*r))
            .ok_or_else(|| OPEN_ERR.to_string())
    }

    /// `table.insert(t, v)` for each value after the table, for entries
    /// added to a table that is no longer being constructed.
    fn insert_each(&mut self, mut got: Vec<(Expr, bool)>) -> Result<(), String> {
        let (t, _) = got.remove(0);
        self.flush_effects()?;
        for (v, _) in got {
            let insert = Expr::index(
                Expr::Name("table".to_string()),
                Expr::Str("insert".to_string()),
            );
            self.out
                .push(Stmt::Call(Expr::Call(Box::new(insert), vec![t.clone(), v])));
        }
        Ok(())
    }

    /// `obj.name(obj, ...)` becomes `obj:name(...)` when `obj` is a plain
    /// variable, which reads the same either way.
    fn make_call(&self, f: Expr, mut args: Vec<Expr>) -> Expr {
        let method = match &f {
            Expr::Index(obj, key) => match (obj.as_ref(), key.as_ref()) {
                (Expr::Var(v), Expr::Str(name))
                    if is_name(name)
                        && !self.captured(*v)
                        && args.first() == Some(obj.as_ref()) =>
                {
                    Some(name.clone())
                }
                _ => None,
            },
            _ => None,
        };
        match (method, f) {
            (Some(name), Expr::Index(obj, _)) => {
                args.remove(0);
                Expr::Method(obj, name, args)
            }
            (_, f) => Expr::Call(Box::new(f), args),
        }
    }

    // ---- the end of the block ------------------------------------------

    fn terminator(&mut self, pc: usize, inst: &Instruction) -> Result<Option<End>, String> {
        use Instruction as I;
        let none = Rules::default();
        let end = match *inst {
            I::Jump { .. } => match self.tfor {
                Some(tpc) => End::ForIn(self.for_source(tpc)?),
                None => {
                    self.flush_all()?;
                    End::None
                }
            },
            I::Return { base, count } => {
                let end = if count == MULTRET {
                    self.open_start(pc)?
                } else {
                    usize::from(base) + usize::from(count)
                };
                let mut srcs: Vec<Src> = (usize::from(base)..end)
                    .filter_map(|r| Reg::try_from(r).ok())
                    .map(Src::Reg)
                    .collect();
                if count == MULTRET {
                    srcs.push(Src::Open);
                }
                let rules = Rules {
                    no_multi_last: count != MULTRET && count > 0,
                    ..none
                };
                let vals = self.take(pc, &srcs, rules)?;
                self.flush_effects()?;
                End::Return(vals.into_iter().map(|(e, _)| e).collect())
            }
            I::ForPrep { base, .. } => {
                let srcs: Vec<Src> = (0..3)
                    .map(|i| base.checked_add(i).map(Src::Reg))
                    .collect::<Option<_>>()
                    .ok_or("a for loop's registers run past the last register")?;
                let mut got = self.take(pc, &srcs, none)?;
                self.flush_all()?;
                let (step, _) = got.remove(2);
                let (limit, _) = got.remove(1);
                let (start, _) = got.remove(0);
                End::ForPrep(start, limit, step)
            }
            I::Test { reg, .. } => {
                let (cond, _) = self.take(pc, &[Src::Reg(reg)], none)?.remove(0);
                self.flush_all()?;
                End::Cond(cond)
            }
            I::Eq { a, b, .. }
            | I::Ne { a, b, .. }
            | I::Lt { a, b, .. }
            | I::Le { a, b, .. }
            | I::Gt { a, b, .. }
            | I::Ge { a, b, .. } => {
                let op = bin_op(inst).ok_or("unknown comparison")?;
                let mut got = self.take(pc, &[Src::Reg(a), Src::Reg(b)], none)?;
                self.flush_all()?;
                let (y, _) = got.remove(1);
                let (x, _) = got.remove(0);
                End::Cond(Expr::bin(op, x, y))
            }
            I::EqK { a, k, .. }
            | I::NeK { a, k, .. }
            | I::LtK { a, k, .. }
            | I::LeK { a, k, .. }
            | I::GtK { a, k, .. }
            | I::GeK { a, k, .. } => {
                let op = bin_op(inst).ok_or("unknown comparison")?;
                let y = self.constant(k)?;
                let (x, _) = self.take(pc, &[Src::Reg(a)], none)?.remove(0);
                self.flush_all()?;
                End::Cond(Expr::bin(op, x, y))
            }
            I::EqI { a, imm, .. }
            | I::NeI { a, imm, .. }
            | I::LtI { a, imm, .. }
            | I::LeI { a, imm, .. }
            | I::GtI { a, imm, .. }
            | I::GeI { a, imm, .. } => {
                let op = bin_op(inst).ok_or("unknown comparison")?;
                let (x, _) = self.take(pc, &[Src::Reg(a)], none)?.remove(0);
                self.flush_all()?;
                End::Cond(Expr::bin(op, x, Expr::Int(imm.into())))
            }
            I::ForLoop { .. } | I::TForLoop { .. } => {
                return Err("a loop instruction is in an unexpected place".to_string());
            }
            _ => return Ok(None),
        };
        Ok(Some(end))
    }

    /// The expression a generic `for` iterates over: the call whose three
    /// results fill its control registers, or a single value followed by
    /// two nils (how `for k, v in t` compiles).
    fn for_source(&mut self, tpc: usize) -> Result<Expr, String> {
        const SHAPE: &str = "a generic for loop starts from values that are not one expression";
        let Instruction::TForCall { base, .. } = self.cx.proto.instructions[tpc] else {
            return Err(SHAPE.to_string());
        };
        let mut vars = Vec::with_capacity(3);
        for i in 0..3 {
            let reg = base.checked_add(i).ok_or(SHAPE)?;
            vars.push(self.cx.vars.read(tpc, reg)?);
        }
        if let Some(idx) = self
            .pending
            .iter()
            .position(|p| matches!(&p.kind, Kind::Multi(vs) if *vs == vars))
        {
            let p = self.pending.remove(idx);
            return self.finish_source(p.expr, p.seq, p.effect);
        }
        if let Some(source) = self.copied_call(&vars)? {
            return Ok(source);
        }
        // The state and control slots must hold nil: a pending nil, or a
        // variable whose only value is a `LoadNil` and that only the loop
        // reads.
        let is_nil = |l: &Self, v: VarId| match l.index_of(v) {
            Some(i) => {
                l.pending[i].expr == Expr::Nil && matches!(l.pending[i].kind, Kind::Value(_))
            }
            None => {
                let var = &l.cx.vars.list[v];
                var.uses == [tpc]
                    && matches!(var.defs.as_slice(), [d] if matches!(l.cx.proto.instructions[*d], Instruction::LoadNil { .. }))
            }
        };
        if !vars[1..].iter().all(|&v| is_nil(self, v)) {
            // `for` takes one expression, so the three values the loop
            // starts from go through a call that returns them unchanged:
            // `(function(...) return ... end)(f, s, c)`.
            let srcs: Vec<Src> = (0..3)
                .map(|i| base.checked_add(i).map(Src::Reg).ok_or(SHAPE))
                .collect::<Result<_, _>>()?;
            let args = self
                .take(tpc, &srcs, Rules::default())?
                .into_iter()
                .map(|(e, _)| e)
                .collect();
            self.flush_all()?;
            let pass = Func {
                params: Vec::new(),
                vararg: true,
                body: Body::Code(vec![Stmt::Return(vec![Expr::Vararg])]),
                names: Vec::new(),
                captures: Vec::new(),
            };
            return Ok(Expr::Call(Box::new(Expr::Function(Box::new(pass))), args));
        }
        for &v in &vars[1..] {
            if let Some(i) = self.index_of(v) {
                self.pending.remove(i);
            }
        }
        match self.index_of(vars[0]) {
            Some(i)
                if self.pending[i].foldable && matches!(self.pending[i].kind, Kind::Value(_)) =>
            {
                let p = self.pending.remove(i);
                self.finish_source(p.expr, p.seq, p.effect)
            }
            Some(i) => {
                self.flush_through(i)?;
                self.flush_all()?;
                Ok(Expr::Var(vars[0]))
            }
            None => {
                self.flush_all()?;
                Ok(Expr::Var(vars[0]))
            }
        }
    }

    /// The call of `a, b, c = f()` just before, when the three loop
    /// registers are copies of `a`, `b` and `c` and nothing else reads them.
    fn copied_call(&mut self, vars: &[VarId]) -> Result<Option<Expr>, String> {
        let mut copies = Vec::with_capacity(vars.len());
        for &v in vars {
            match self
                .index_of(v)
                .map(|i| (&self.pending[i].kind, &self.pending[i].expr))
            {
                Some((Kind::Value(_), Expr::Var(x))) => copies.push(*x),
                _ => return Ok(None),
            }
        }
        let only_copied = copies.iter().all(|&x| {
            let var = &self.cx.vars.list[x];
            var.uses.len() == 1 && var.debug_name.is_none() && !var.captured
        });
        if !only_copied {
            return Ok(None);
        }
        // The call may still be held back, or already written out.
        let held = |l: &Self| {
            l.pending
                .iter()
                .position(|p| matches!(&p.kind, Kind::Multi(vs) if *vs == copies))
        };
        if held(self).is_some() {
            for &v in vars {
                if let Some(i) = self.index_of(v) {
                    self.pending.remove(i);
                }
            }
            let Some(idx) = held(self) else {
                return Ok(None);
            };
            let p = self.pending.remove(idx);
            return self.finish_source(p.expr, p.seq, p.effect).map(Some);
        }
        let targets: Vec<Target> = copies.iter().map(|x| Target::Var(*x)).collect();
        let fits = matches!(self.out.last(), Some(Stmt::Assign(t, vals)) if *t == targets && vals.len() == 1)
            && !self.pending.iter().any(|p| p.effect);
        if !fits {
            return Ok(None);
        }
        for &v in vars {
            if let Some(i) = self.index_of(v) {
                self.pending.remove(i);
            }
        }
        let Some(Stmt::Assign(_, mut vals)) = self.out.pop() else {
            return Ok(None);
        };
        self.flush_all()?;
        Ok(vals.pop())
    }

    fn finish_source(&mut self, expr: Expr, seq: u64, effect: bool) -> Result<Expr, String> {
        if effect && self.pending.iter().any(|p| p.effect && p.seq > seq) {
            return Err("a generic for loop's source is evaluated out of order".to_string());
        }
        self.flush_all()?;
        Ok(expr)
    }
}

/// A statement that evaluates `expr` for its side effects.
fn effect_stmt(expr: Expr) -> Stmt {
    match expr {
        Expr::Call(..) | Expr::Method(..) => Stmt::Call(expr),
        other => Stmt::Discard(other),
    }
}
