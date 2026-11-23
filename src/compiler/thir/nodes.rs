//! Typed High-level Intermediate Representation (THIR) node definitions.
//!
//! THIR pairs every HIR expression and declaration with a resolved static
//! [`NeyukiType`] and lexical [`BindingId`], tracks captured upvalues per
//! function body, and classifies global `module.field` member accesses.

use crate::ast::literal::Literal;
use crate::ast::op::{BinOp, UnOp};
use crate::compiler::hir::HirId;
use crate::sema::types::NeyukiType;

/// Dense identifier for a THIR statement or expression node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ThirId(pub u32);

/// Unique identifier for a lexically bound variable or parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BindingId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BindingKind {
    Local,
    Param,
    Upvalue,
    Global,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VarBinding {
    pub id: BindingId,
    pub name: String,
    pub kind: BindingKind,
    pub is_const: bool,
    pub ty: NeyukiType,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThirModule {
    pub items: Vec<ThirStmt>,
    pub bindings: Vec<VarBinding>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThirStmt {
    pub thir_id: ThirId,
    pub hir_id: HirId,
    pub line: u32,
    pub kind: ThirStmtKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ThirStmtKind {
    Local {
        binding: BindingId,
        name: String,
        is_global: bool,
        ty: NeyukiType,
        initializer: Option<ThirExpr>,
    },
    LocalMany {
        bindings: Vec<BindingId>,
        names: Vec<String>,
        is_global: bool,
        initializers: Vec<ThirExpr>,
    },
    Assign {
        target: ThirLValue,
        value: ThirExpr,
    },
    AssignMany {
        targets: Vec<ThirLValue>,
        values: Vec<ThirExpr>,
    },
    Increment {
        target: ThirLValue,
        amount: i8,
    },
    Function {
        binding: Option<BindingId>,
        name: Option<String>,
        is_global: bool,
        params: Vec<ThirParam>,
        return_type: NeyukiType,
        captures: Vec<BindingId>,
        body: Vec<ThirStmt>,
    },
    Block(Vec<ThirStmt>),
    If {
        condition: ThirExpr,
        then_branch: Vec<ThirStmt>,
        else_branch: Option<Vec<ThirStmt>>,
    },
    For {
        bindings: Vec<BindingId>,
        vars: Vec<String>,
        source: ThirExpr,
        body: Vec<ThirStmt>,
    },
    NumericFor(Box<ThirNumericFor>),
    While {
        condition: ThirExpr,
        body: Vec<ThirStmt>,
    },
    Repeat {
        body: Vec<ThirStmt>,
        condition: ThirExpr,
    },
    Return(Vec<ThirExpr>),
    Break,
    Continue,
    Goto(String),
    Label(String),
    Expr(ThirExpr),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThirNumericFor {
    pub binding: BindingId,
    pub var: String,
    pub start: ThirExpr,
    pub end: ThirExpr,
    pub step: ThirExpr,
    pub body: Vec<ThirStmt>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThirParam {
    pub binding: Option<BindingId>,
    pub name: String,
    pub ty: NeyukiType,
    pub variadic: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThirExpr {
    pub thir_id: ThirId,
    pub hir_id: HirId,
    pub ty: NeyukiType,
    pub kind: ThirExprKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ThirExprKind {
    Literal(Literal),
    Variable {
        name: String,
        binding: Option<BindingId>,
    },
    Vararg,
    /// A call or `...` cut down to its first value.
    Paren(Box<ThirExpr>),
    Member {
        object: Box<ThirExpr>,
        field: String,
        is_module_import: bool,
    },
    Index {
        object: Box<ThirExpr>,
        index: Box<ThirExpr>,
    },
    Call {
        callee: Box<ThirExpr>,
        args: Vec<ThirExpr>,
    },
    MethodCall {
        object: Box<ThirExpr>,
        method: String,
        args: Vec<ThirExpr>,
    },
    Function {
        params: Vec<ThirParam>,
        return_type: NeyukiType,
        captures: Vec<BindingId>,
        body: Vec<ThirStmt>,
    },
    Unary {
        op: UnOp,
        expr: Box<ThirExpr>,
    },
    Binary {
        left: Box<ThirExpr>,
        op: BinOp,
        right: Box<ThirExpr>,
    },
    Table(Vec<ThirTableEntry>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ThirLValue {
    Variable {
        name: String,
        binding: Option<BindingId>,
    },
    Member {
        object: Box<ThirExpr>,
        field: String,
    },
    Index {
        object: Box<ThirExpr>,
        index: Box<ThirExpr>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThirTableEntry {
    pub key: Option<String>,
    pub computed: Option<Box<ThirExpr>>,
    pub value: ThirExpr,
}

impl ThirTableEntry {
    pub fn is_array_element(&self) -> bool {
        self.key.is_none() && self.computed.is_none()
    }
}
