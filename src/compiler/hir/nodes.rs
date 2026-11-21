//! High-level Intermediate Representation (HIR) node definitions.
//!
//! HIR is lowered directly from the AST and normalized so later stages (THIR,
//! MIR, bytecode IR) never deal with syntactic sugar such as `elseif` chains,
//! string interpolation templates, dotted `function a.b.c()` declarations,
//! implicit numeric-for steps, local `++`/`--` increments, or erased type
//! aliases.

use crate::ast::Param;
use crate::ast::literal::Literal;
use crate::ast::node_id::NodeId;
use crate::ast::op::{BinOp, UnOp};

/// Dense identifier assigned to every HIR statement and expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HirId(pub u32);

/// Whole lowered program in HIR form.
#[derive(Clone, Debug, PartialEq)]
pub struct HirModule {
    pub items: Vec<HirStmt>,
}

/// A single HIR statement with resolved source line metadata.
#[derive(Clone, Debug, PartialEq)]
pub struct HirStmt {
    pub hir_id: HirId,
    pub source_id: NodeId,
    pub line: u32,
    pub kind: HirStmtKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HirStmtKind {
    Local {
        name: String,
        is_const: bool,
        is_global: bool,
        type_name: Option<String>,
        initializer: Option<HirExpr>,
    },
    LocalMany {
        names: Vec<String>,
        is_const: bool,
        is_global: bool,
        initializers: Vec<HirExpr>,
    },
    Assign {
        target: HirLValue,
        value: HirExpr,
        is_const: bool,
    },
    AssignMany {
        targets: Vec<HirLValue>,
        values: Vec<HirExpr>,
    },
    /// Only retained for table/index targets so the receiver/index expression
    /// is evaluated exactly once. Simple variable increments desugar to `Assign`.
    Increment {
        target: HirLValue,
        amount: i8,
    },
    Function {
        name: Option<String>,
        is_const: bool,
        is_global: bool,
        params: Vec<Param>,
        return_type: Option<String>,
        body: Vec<HirStmt>,
    },
    /// Explicit lexical scope block (e.g. from constant `if true` lowering).
    Block(Vec<HirStmt>),
    If {
        condition: HirExpr,
        then_branch: Vec<HirStmt>,
        else_branch: Option<Vec<HirStmt>>,
    },
    For {
        vars: Vec<String>,
        source: HirExpr,
        body: Vec<HirStmt>,
    },
    NumericFor {
        var: String,
        start: HirExpr,
        end: HirExpr,
        step: HirExpr,
        body: Vec<HirStmt>,
    },
    While {
        condition: HirExpr,
        body: Vec<HirStmt>,
    },
    Repeat {
        body: Vec<HirStmt>,
        condition: HirExpr,
    },
    Return(Vec<HirExpr>),
    Break,
    Continue,
    Goto(String),
    Label(String),
    Expr(HirExpr),
}

/// An HIR expression node.
#[derive(Clone, Debug, PartialEq)]
pub struct HirExpr {
    pub hir_id: HirId,
    pub source_id: NodeId,
    pub kind: HirExprKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum HirExprKind {
    Literal(Literal),
    Variable(String),
    Vararg,
    Member {
        object: Box<HirExpr>,
        field: String,
    },
    Index {
        object: Box<HirExpr>,
        index: Box<HirExpr>,
    },
    Call {
        callee: Box<HirExpr>,
        args: Vec<HirExpr>,
    },
    MethodCall {
        object: Box<HirExpr>,
        method: String,
        args: Vec<HirExpr>,
    },
    Function {
        params: Vec<Param>,
        body: Vec<HirStmt>,
    },
    Unary {
        op: UnOp,
        expr: Box<HirExpr>,
    },
    Binary {
        left: Box<HirExpr>,
        op: BinOp,
        right: Box<HirExpr>,
    },
    Table(Vec<HirTableEntry>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum HirLValue {
    Variable(String),
    Member {
        object: Box<HirExpr>,
        field: String,
    },
    Index {
        object: Box<HirExpr>,
        index: Box<HirExpr>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct HirTableEntry {
    pub key: Option<String>,
    pub computed: Option<Box<HirExpr>>,
    pub value: HirExpr,
}

impl HirTableEntry {
    pub fn is_array_element(&self) -> bool {
        self.key.is_none() && self.computed.is_none()
    }
}
