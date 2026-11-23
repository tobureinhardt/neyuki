//! Mid-level Intermediate Representation (MIR) CFG and place definitions.
//!
//! MIR represents every Neyuki function as a typed control-flow graph of basic
//! blocks operating on explicit locals/temporaries (`LocalId`), projected places
//! (`Place`), operands, side-effect statements, and explicit terminators. It
//! serves as the shared backbone feeding both the bytecode IR pipeline and the
//! LLVM IR backend.

use num_bigint::BigInt;

use crate::ast::Param;
use crate::ast::op::{BinOp, UnOp};
use crate::compiler::thir::BindingId;
use crate::sema::types::NeyukiType;

/// Local slot inside a [`MirBody`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LocalId(pub u32);

/// Basic block index inside a [`MirBody`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BasicBlockId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    Arg,
    Var,
    Temp,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LocalDecl {
    pub id: LocalId,
    pub name: Option<String>,
    pub kind: LocalKind,
    pub ty: NeyukiType,
    pub binding: Option<BindingId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MirUpvalue {
    pub in_stack: bool,
    pub index: u8,
    pub levels_up: usize,
    pub parent_local: Option<LocalId>,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum MirConst {
    Nil,
    Bool(bool),
    Int(BigInt),
    Float(f64),
    String(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Projection {
    Field(String),
    Index(Operand),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub local: LocalId,
    pub projections: Vec<Projection>,
}

impl Place {
    pub fn local(local: LocalId) -> Self {
        Self {
            local,
            projections: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Operand {
    Copy(Place),
    Move(Place),
    Constant(MirConst),
}

#[derive(Clone, Debug, PartialEq)]
pub enum MirSpreadSource {
    Call {
        callee: Operand,
        args: Vec<Operand>,
        trailing: Option<Box<MirSpreadSource>>,
    },
    Vararg,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Rvalue {
    Use(Operand),
    BinaryOp {
        op: BinOp,
        lhs: Operand,
        rhs: Operand,
    },
    UnaryOp {
        op: UnOp,
        src: Operand,
    },
    Global(String),
    Upvalue(u8),
    ModuleImport {
        module: String,
        field: String,
    },
    NewTable {
        array_cap: u8,
        fields_cap: u8,
    },
    Closure {
        fn_idx: usize,
        captures: Vec<Option<LocalId>>,
    },
    Vararg,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Statement {
    pub line: u32,
    pub kind: StatementKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StatementKind {
    Assign(Place, Rvalue),
    SetGlobal {
        name: String,
        value: Operand,
    },
    SetUpval {
        index: u8,
        value: Operand,
    },
    AppendArray {
        table: LocalId,
        value: Operand,
    },
    CloseUpvals(Vec<LocalId>),
    /// Copies the first `dsts.len()` varargs into `dsts`, padding with nil.
    VarargMany(Vec<LocalId>),
    DebugLocal {
        name: String,
        local: LocalId,
    },
    DebugScopeEnd {
        locals: Vec<LocalId>,
        keep_alive: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum TerminatorKind {
    Goto(BasicBlockId),
    SwitchBool {
        cond: Operand,
        true_bb: BasicBlockId,
        false_bb: BasicBlockId,
    },
    Call {
        dsts: Vec<LocalId>,
        callee: Operand,
        args: Vec<Operand>,
        retc: u8,
        target: BasicBlockId,
    },
    SpreadCall {
        dsts: Vec<LocalId>,
        callee: Operand,
        fixed_args: Vec<Operand>,
        trailing: MirSpreadSource,
        retc: u8,
        target: BasicBlockId,
    },
    SpreadList {
        table: LocalId,
        source: MirSpreadSource,
        target: BasicBlockId,
    },
    /// Returns `values` followed by every value `source` produces.
    SpreadReturn {
        values: Vec<Operand>,
        source: MirSpreadSource,
    },
    ForPrep {
        base: LocalId,
        limit: LocalId,
        step: LocalId,
        loop_var: LocalId,
        test_bb: BasicBlockId,
        body_bb: BasicBlockId,
    },
    ForLoop {
        base: LocalId,
        body_bb: BasicBlockId,
        exit_bb: BasicBlockId,
    },
    TForStep {
        base: LocalId,
        state: LocalId,
        ctrl: LocalId,
        vars: Vec<LocalId>,
        body_bb: BasicBlockId,
        exit_bb: BasicBlockId,
    },
    Return(Vec<Operand>),
    Unreachable,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BasicBlockData {
    pub id: BasicBlockId,
    pub statements: Vec<Statement>,
    pub terminator: TerminatorKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MirBody {
    pub name: Option<String>,
    pub params: Vec<Param>,
    pub num_params: u8,
    pub is_vararg: bool,
    pub return_type: NeyukiType,
    pub locals: Vec<LocalDecl>,
    pub upvalues: Vec<MirUpvalue>,
    pub blocks: Vec<BasicBlockData>,
    pub functions: Vec<MirBody>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MirProgram {
    pub main: MirBody,
}
