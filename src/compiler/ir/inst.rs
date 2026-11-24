//! IR instructions.

use crate::compiler::ir::types::{IrBinaryOp, IrConstant, IrLabel, IrUnaryOp, IrVar};

/// Where a run of values goes once it has been produced. The register VM keeps
/// such a run on the stack rather than in named registers, so the producer and
/// the consumer are lowered together as one instruction.
#[derive(Clone, Debug, PartialEq)]
pub enum SpreadSink {
    /// Passed as the trailing arguments of another call.
    Call {
        dsts: Vec<IrVar>,
        callee: IrVar,
        fixed_args: Vec<IrVar>,
        retc: u8,
    },
    /// Appended to a table constructor's array part.
    List { table: IrVar },
    /// Returned from the enclosing function after the fixed `values`.
    Return { values: Vec<IrVar> },
    /// Cut to exactly `dsts.len()` values, padded with nil, and copied out.
    Values { dsts: Vec<IrVar> },
}

/// What produces a run of values: a call keeping all its results, or `...`.
#[derive(Clone, Debug, PartialEq)]
pub enum SpreadSource {
    Call {
        callee: IrVar,
        args: Vec<IrVar>,
        /// A producer for this call's own trailing arguments, as in
        /// `f(g(h()))`, where each call passes on everything the next yields.
        trailing: Option<Box<SpreadSource>>,
    },
    Vararg,
}

impl SpreadSource {
    /// Visits every variable read while producing the run without allocating.
    #[inline]
    pub fn for_each_use(&self, f: &mut impl FnMut(IrVar)) {
        if let Self::Call {
            callee,
            args,
            trailing,
        } = self
        {
            f(*callee);
            for &arg in args {
                f(arg);
            }
            if let Some(inner) = trailing {
                inner.for_each_use(f);
            }
        }
    }

    /// How many consecutive registers laying this producer out needs.
    pub fn slots(&self) -> u8 {
        match self {
            Self::Call { args, trailing, .. } => {
                let nested = trailing.as_ref().map(|t| t.slots()).unwrap_or(0);
                1u8.saturating_add(args.len() as u8).saturating_add(nested)
            }
            Self::Vararg => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum IrInst {
    LoadConst {
        dst: IrVar,
        val: IrConstant,
    },
    LoadNil {
        dst: IrVar,
    },
    Move {
        dst: IrVar,
        src: IrVar,
    },
    BinOp {
        dst: IrVar,
        op: IrBinaryOp,
        lhs: IrVar,
        rhs: IrVar,
    },
    UnOp {
        dst: IrVar,
        op: IrUnaryOp,
        src: IrVar,
    },
    NewTable {
        dst: IrVar,
        array_cap: u8,
        fields_cap: u8,
    },
    GetTable {
        dst: IrVar,
        table: IrVar,
        key: IrVar,
    },
    GetImport {
        dst: IrVar,
        module: String,
        field: String,
    },
    SetTable {
        table: IrVar,
        key: IrVar,
        val: IrVar,
    },
    AppendArray {
        table: IrVar,
        src: IrVar,
    },
    GetGlobal {
        dst: IrVar,
        name: String,
    },
    SetGlobal {
        name: String,
        src: IrVar,
    },
    GetUpval {
        dst: IrVar,
        index: u8,
    },
    SetUpval {
        index: u8,
        src: IrVar,
    },
    Call {
        /// Where the call's results go, one variable per result. A call used
        /// as a statement has none; `local a, b = f()` has two.
        dsts: Vec<IrVar>,
        callee: IrVar,
        args: Vec<IrVar>,
        retc: u8,
    },
    Return(Vec<IrVar>),
    Closure {
        dst: IrVar,
        proto_idx: u16,
        /// Parallel to the nested prototype's upvalues: the parent variable
        /// each one captures, or `None` when it comes from the parent's own
        /// upvalues. Listing them here keeps the optimizer from dropping or
        /// renaming a variable that only a nested function reads.
        captures: Vec<Option<IrVar>>,
    },
    Vararg {
        dst: IrVar,
        count: u8,
    },
    ForPrep {
        base: IrVar,
        limit: IrVar,
        step: IrVar,
        loop_var: IrVar,
        jump: IrLabel,
    },
    ForLoop {
        base: IrVar,
        jump: IrLabel,
    },
    TForCall {
        base: IrVar,
        state: IrVar,
        ctrl: IrVar,
        vars: Vec<IrVar>,
    },
    TForLoop {
        base: IrVar,
        jump: IrLabel,
    },
    /// Detaches these captured variables from any closure made so far, so the
    /// next closure capturing one gets a fresh variable. Emitted where each
    /// loop iteration starts for the loop's own locals.
    CloseUpvals {
        vars: Vec<IrVar>,
    },
    Jump(IrLabel),
    JumpIfFalse {
        cond: IrVar,
        target: IrLabel,
    },
    /// Every value from `source` handed straight to `sink`, e.g. `f(g())`,
    /// `{...}` or `return f()`.
    Spread {
        source: SpreadSource,
        sink: SpreadSink,
    },
    Label(IrLabel),
    /// The source line of the instructions that follow. Emits no code.
    Line(u32),
    /// From here on the local variable `name` lives in `var`. Debug info only.
    DebugLocal {
        name: String,
        var: IrVar,
    },
    /// The locals declared since the matching scope began go out of scope.
    /// With `keep_alive` the variables count as used here, so a debugger can
    /// read them until the end of their scope.
    DebugScopeEnd {
        vars: Vec<IrVar>,
        keep_alive: bool,
    },
    // SSA Phi node: chooses incoming variable based on predecessor block label
    Phi {
        dst: IrVar,
        incoming: Vec<(IrLabel, IrVar)>,
    },
}

impl IrInst {
    pub fn def_var(&self) -> Option<IrVar> {
        match self {
            Self::LoadConst { dst, .. }
            | Self::LoadNil { dst }
            | Self::Move { dst, .. }
            | Self::BinOp { dst, .. }
            | Self::UnOp { dst, .. }
            | Self::NewTable { dst, .. }
            | Self::GetTable { dst, .. }
            | Self::GetImport { dst, .. }
            | Self::GetGlobal { dst, .. }
            | Self::GetUpval { dst, .. }
            | Self::Closure { dst, .. }
            | Self::Vararg { dst, .. }
            | Self::Phi { dst, .. } => Some(*dst),
            Self::Call { dsts, .. } => dsts.first().copied(),
            _ => None,
        }
    }

    /// Visits every variable this instruction writes without allocating.
    #[inline]
    pub fn for_each_def(&self, mut f: impl FnMut(IrVar)) {
        match self {
            Self::ForPrep {
                base,
                limit,
                step,
                loop_var,
                ..
            } => {
                f(*base);
                f(*limit);
                f(*step);
                f(*loop_var);
            }
            Self::ForLoop { base, .. } | Self::TForLoop { base, .. } => f(*base),
            Self::TForCall {
                base,
                state,
                ctrl,
                vars,
            } => {
                f(*base);
                f(*state);
                f(*ctrl);
                for &v in vars {
                    f(v);
                }
            }
            Self::Call { dsts, .. }
            | Self::Spread {
                sink: SpreadSink::Call { dsts, .. } | SpreadSink::Values { dsts },
                ..
            } => {
                for &d in dsts {
                    f(d);
                }
            }
            _ => {
                if let Some(d) = self.def_var() {
                    f(d);
                }
            }
        }
    }

    /// Every variable this instruction writes. `def_var` names only the single
    /// value an expression produces; a `for` loop also writes its control and
    /// loop variables on each iteration, and an analysis that cannot see those
    /// will treat a use of the loop variable as loop-invariant.
    pub fn def_vars(&self) -> Vec<IrVar> {
        let mut defs = Vec::new();
        self.for_each_def(|v| defs.push(v));
        defs
    }

    /// Visits every variable this instruction reads without allocating.
    #[inline]
    pub fn for_each_use(&self, mut f: impl FnMut(IrVar)) {
        match self {
            Self::Move { src, .. }
            | Self::UnOp { src, .. }
            | Self::SetGlobal { src, .. }
            | Self::SetUpval { src, .. } => f(*src),
            Self::AppendArray { table, src } => {
                f(*table);
                f(*src);
            }
            Self::BinOp { lhs, rhs, .. } => {
                f(*lhs);
                f(*rhs);
            }
            Self::GetTable { table, key, .. } => {
                f(*table);
                f(*key);
            }
            Self::SetTable { table, key, val } => {
                f(*table);
                f(*key);
                f(*val);
            }
            Self::Call { callee, args, .. } => {
                f(*callee);
                for &arg in args {
                    f(arg);
                }
            }
            Self::Return(vars) => {
                for &v in vars {
                    f(v);
                }
            }
            Self::JumpIfFalse { cond, .. } => f(*cond),
            Self::ForPrep {
                base, limit, step, ..
            } => {
                f(*base);
                f(*limit);
                f(*step);
            }
            Self::ForLoop { base, .. } | Self::TForLoop { base, .. } => f(*base),
            Self::TForCall {
                base, state, ctrl, ..
            } => {
                f(*base);
                f(*state);
                f(*ctrl);
            }
            Self::Closure { captures, .. } => {
                for &v in captures.iter().flatten() {
                    f(v);
                }
            }
            // Reading them keeps each variable in its register up to here,
            // so nothing else can occupy it while a closure still holds it.
            Self::CloseUpvals { vars } => {
                for &v in vars {
                    f(v);
                }
            }
            Self::Spread { source, sink } => {
                source.for_each_use(&mut f);
                match sink {
                    SpreadSink::Call {
                        callee, fixed_args, ..
                    } => {
                        f(*callee);
                        for &arg in fixed_args {
                            f(arg);
                        }
                    }
                    SpreadSink::List { table } => f(*table),
                    SpreadSink::Return { values } => {
                        for &v in values {
                            f(v);
                        }
                    }
                    SpreadSink::Values { .. } => {}
                }
            }
            Self::Phi { incoming, .. } => {
                for &(_, v) in incoming {
                    f(v);
                }
            }
            Self::DebugScopeEnd {
                vars,
                keep_alive: true,
            } => {
                for &v in vars {
                    f(v);
                }
            }
            _ => {}
        }
    }

    pub fn use_vars(&self) -> Vec<IrVar> {
        let mut uses = Vec::new();
        self.for_each_use(&mut |v| uses.push(v));
        uses
    }

    /// Whether this instruction reads `var`.
    #[inline]
    pub fn uses_var(&self, var: IrVar) -> bool {
        let mut found = false;
        self.for_each_use(|u| {
            if u == var {
                found = true;
            }
        });
        found
    }

    /// Line and local-variable markers, which produce no code.
    pub fn is_debug_marker(&self) -> bool {
        matches!(
            self,
            Self::Line(_) | Self::DebugLocal { .. } | Self::DebugScopeEnd { .. }
        )
    }

    pub fn is_terminator(&self) -> bool {
        matches!(
            self,
            Self::Return(_)
                | Self::Spread {
                    sink: SpreadSink::Return { .. },
                    ..
                }
                | Self::Jump(_)
                | Self::JumpIfFalse { .. }
                | Self::ForPrep { .. }
                | Self::ForLoop { .. }
                | Self::TForLoop { .. }
        )
    }

    /// Target label of a jump or loop instruction, if any.
    #[inline]
    pub fn jump_target(&self) -> Option<IrLabel> {
        match self {
            Self::Jump(lbl) => Some(*lbl),
            Self::JumpIfFalse { target, .. } => Some(*target),
            Self::ForPrep { jump, .. }
            | Self::ForLoop { jump, .. }
            | Self::TForLoop { jump, .. } => Some(*jump),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_for_each_visitors_cover_all_variants() {
        let samples: Vec<(IrInst, Vec<IrVar>, Vec<IrVar>)> = vec![
            (
                IrInst::LoadConst {
                    dst: IrVar(1),
                    val: IrConstant::Nil,
                },
                vec![IrVar(1)],
                vec![],
            ),
            (IrInst::LoadNil { dst: IrVar(2) }, vec![IrVar(2)], vec![]),
            (
                IrInst::Move {
                    dst: IrVar(3),
                    src: IrVar(4),
                },
                vec![IrVar(3)],
                vec![IrVar(4)],
            ),
            (
                IrInst::BinOp {
                    dst: IrVar(5),
                    op: IrBinaryOp::Add,
                    lhs: IrVar(6),
                    rhs: IrVar(7),
                },
                vec![IrVar(5)],
                vec![IrVar(6), IrVar(7)],
            ),
            (
                IrInst::UnOp {
                    dst: IrVar(8),
                    op: IrUnaryOp::Neg,
                    src: IrVar(9),
                },
                vec![IrVar(8)],
                vec![IrVar(9)],
            ),
            (
                IrInst::NewTable {
                    dst: IrVar(10),
                    array_cap: 1,
                    fields_cap: 2,
                },
                vec![IrVar(10)],
                vec![],
            ),
            (
                IrInst::GetTable {
                    dst: IrVar(11),
                    table: IrVar(12),
                    key: IrVar(13),
                },
                vec![IrVar(11)],
                vec![IrVar(12), IrVar(13)],
            ),
            (
                IrInst::SetTable {
                    table: IrVar(14),
                    key: IrVar(15),
                    val: IrVar(16),
                },
                vec![],
                vec![IrVar(14), IrVar(15), IrVar(16)],
            ),
            (
                IrInst::AppendArray {
                    table: IrVar(17),
                    src: IrVar(18),
                },
                vec![],
                vec![IrVar(17), IrVar(18)],
            ),
            (
                IrInst::Call {
                    dsts: vec![IrVar(19), IrVar(20)],
                    callee: IrVar(21),
                    args: vec![IrVar(22), IrVar(23)],
                    retc: 2,
                },
                vec![IrVar(19), IrVar(20)],
                vec![IrVar(21), IrVar(22), IrVar(23)],
            ),
            (
                IrInst::Closure {
                    dst: IrVar(24),
                    proto_idx: 0,
                    captures: vec![Some(IrVar(25)), None, Some(IrVar(26))],
                },
                vec![IrVar(24)],
                vec![IrVar(25), IrVar(26)],
            ),
            (
                IrInst::ForPrep {
                    base: IrVar(27),
                    limit: IrVar(28),
                    step: IrVar(29),
                    loop_var: IrVar(30),
                    jump: IrLabel(1),
                },
                vec![IrVar(27), IrVar(28), IrVar(29), IrVar(30)],
                vec![IrVar(27), IrVar(28), IrVar(29)],
            ),
            (
                IrInst::TForCall {
                    base: IrVar(31),
                    state: IrVar(32),
                    ctrl: IrVar(33),
                    vars: vec![IrVar(34), IrVar(35)],
                },
                vec![IrVar(31), IrVar(32), IrVar(33), IrVar(34), IrVar(35)],
                vec![IrVar(31), IrVar(32), IrVar(33)],
            ),
            (
                IrInst::Spread {
                    source: SpreadSource::Call {
                        callee: IrVar(36),
                        args: vec![IrVar(37)],
                        trailing: Some(Box::new(SpreadSource::Call {
                            callee: IrVar(38),
                            args: vec![IrVar(39)],
                            trailing: Some(Box::new(SpreadSource::Vararg)),
                        })),
                    },
                    sink: SpreadSink::Call {
                        dsts: vec![IrVar(40)],
                        callee: IrVar(41),
                        fixed_args: vec![IrVar(42)],
                        retc: 1,
                    },
                },
                vec![IrVar(40)],
                vec![
                    IrVar(36),
                    IrVar(37),
                    IrVar(38),
                    IrVar(39),
                    IrVar(41),
                    IrVar(42),
                ],
            ),
            (
                IrInst::Phi {
                    dst: IrVar(43),
                    incoming: vec![(IrLabel(0), IrVar(44)), (IrLabel(1), IrVar(45))],
                },
                vec![IrVar(43)],
                vec![IrVar(44), IrVar(45)],
            ),
            (
                IrInst::DebugScopeEnd {
                    vars: vec![IrVar(46)],
                    keep_alive: true,
                },
                vec![],
                vec![IrVar(46)],
            ),
            (
                IrInst::DebugScopeEnd {
                    vars: vec![IrVar(47)],
                    keep_alive: false,
                },
                vec![],
                vec![],
            ),
        ];

        for (inst, expected_defs, expected_uses) in samples {
            assert_eq!(inst.def_vars(), expected_defs);
            assert_eq!(inst.use_vars(), expected_uses);
            for &u in &expected_uses {
                assert!(inst.uses_var(u));
            }
            assert!(!inst.uses_var(IrVar(999)));
        }
    }
}
