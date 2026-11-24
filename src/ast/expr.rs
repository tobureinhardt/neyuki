//! Expression nodes.

use num_bigint::BigInt;

use crate::ast::literal::Literal;
use crate::ast::node_id::NodeId;
use crate::ast::op::{BinOp, UnOp};
use crate::ast::stmt::Stmt;
use crate::ast::ty::TypeExpr;

#[derive(Clone, Debug, Eq)]
pub enum Expr {
    Literal {
        value: Literal,
        id: NodeId,
    },
    Interp {
        parts: Vec<InterpPart>,
        id: NodeId,
    },
    Variable {
        name: String,
        id: NodeId,
    },
    Vararg {
        id: NodeId,
    },
    Member {
        object: Box<Expr>,
        field: String,
        id: NodeId,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
        id: NodeId,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        id: NodeId,
    },
    MethodCall {
        object: Box<Expr>,
        method: String,
        args: Vec<Expr>,
        id: NodeId,
    },
    Function {
        params: Vec<Param>,
        body: Vec<Stmt>,
        id: NodeId,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
        id: NodeId,
    },
    Binary {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
        id: NodeId,
    },
    Table {
        entries: Vec<TableEntry>,
        id: NodeId,
    },
    /// A call or `...` in parentheses, which yields exactly one value.
    /// Other parenthesized expressions need no node of their own.
    Paren {
        expr: Box<Expr>,
        id: NodeId,
    },
}

impl PartialEq for Expr {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Expr::Literal { value: a, .. }, Expr::Literal { value: b, .. }) => a == b,
            (Expr::Interp { parts: a, .. }, Expr::Interp { parts: b, .. }) => a == b,
            (Expr::Variable { name: a, .. }, Expr::Variable { name: b, .. }) => a == b,
            (Expr::Vararg { .. }, Expr::Vararg { .. }) => true,
            (
                Expr::Member {
                    object: oa,
                    field: fa,
                    ..
                },
                Expr::Member {
                    object: ob,
                    field: fb,
                    ..
                },
            ) => oa == ob && fa == fb,
            (
                Expr::Index {
                    object: oa,
                    index: ia,
                    ..
                },
                Expr::Index {
                    object: ob,
                    index: ib,
                    ..
                },
            ) => oa == ob && ia == ib,
            (
                Expr::Call {
                    callee: ca,
                    args: aa,
                    ..
                },
                Expr::Call {
                    callee: cb,
                    args: ab,
                    ..
                },
            ) => ca == cb && aa == ab,
            (
                Expr::MethodCall {
                    object: oa,
                    method: ma,
                    args: aa,
                    ..
                },
                Expr::MethodCall {
                    object: ob,
                    method: mb,
                    args: ab,
                    ..
                },
            ) => oa == ob && ma == mb && aa == ab,
            (
                Expr::Function {
                    params: pa,
                    body: ba,
                    ..
                },
                Expr::Function {
                    params: pb,
                    body: bb,
                    ..
                },
            ) => pa == pb && ba == bb,
            (
                Expr::Unary {
                    op: oa, expr: ea, ..
                },
                Expr::Unary {
                    op: ob, expr: eb, ..
                },
            ) => oa == ob && ea == eb,
            (
                Expr::Binary {
                    left: la,
                    op: oa,
                    right: ra,
                    ..
                },
                Expr::Binary {
                    left: lb,
                    op: ob,
                    right: rb,
                    ..
                },
            ) => la == lb && oa == ob && ra == rb,
            (Expr::Table { entries: a, .. }, Expr::Table { entries: b, .. }) => a == b,
            (Expr::Paren { expr: a, .. }, Expr::Paren { expr: b, .. }) => a == b,
            _ => false,
        }
    }
}

impl Expr {
    pub fn node_id(&self) -> NodeId {
        match self {
            Expr::Literal { id, .. }
            | Expr::Interp { id, .. }
            | Expr::Variable { id, .. }
            | Expr::Vararg { id }
            | Expr::Member { id, .. }
            | Expr::Index { id, .. }
            | Expr::Call { id, .. }
            | Expr::MethodCall { id, .. }
            | Expr::Function { id, .. }
            | Expr::Unary { id, .. }
            | Expr::Binary { id, .. }
            | Expr::Table { id, .. }
            | Expr::Paren { id, .. } => *id,
        }
    }

    pub fn bin_op(&self) -> Option<BinOp> {
        match self {
            Self::Binary { op, .. } => Some(*op),
            _ => None,
        }
    }

    pub fn un_op(&self) -> Option<UnOp> {
        match self {
            Self::Unary { op, .. } => Some(*op),
            _ => None,
        }
    }

    pub fn literal_value(&self) -> Option<Literal> {
        match self {
            Self::Literal { value, .. } => Some(value.clone()),
            _ => None,
        }
    }

    pub fn is_literal(&self) -> bool {
        matches!(self, Self::Literal { .. })
    }

    // Builder helpers
    pub fn int(val: impl Into<BigInt>) -> Self {
        Self::Literal {
            value: Literal::Int(val.into()),
            id: NodeId::next(),
        }
    }

    pub fn float(val: f64) -> Self {
        Self::Literal {
            value: Literal::Float(val),
            id: NodeId::next(),
        }
    }

    pub fn bool(val: bool) -> Self {
        Self::Literal {
            value: Literal::Bool(val),
            id: NodeId::next(),
        }
    }

    pub fn nil() -> Self {
        Self::Literal {
            value: Literal::Nil,
            id: NodeId::next(),
        }
    }

    pub fn string(val: impl Into<String>) -> Self {
        Self::Literal {
            value: Literal::String(val.into()),
            id: NodeId::next(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InterpPart {
    Literal(String),
    Expr(Expr),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableEntry {
    /// `name = value` and `["name"] = value`.
    pub key: Option<String>,
    /// `[expr] = value` for any other key.
    pub computed: Option<Box<Expr>>,
    pub value: Expr,
}

impl TableEntry {
    pub fn positional(value: Expr) -> Self {
        Self {
            key: None,
            computed: None,
            value,
        }
    }

    pub fn is_array_element(&self) -> bool {
        self.key.is_none() && self.computed.is_none()
    }

    pub fn is_record_field(&self) -> bool {
        !self.is_array_element()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub type_name: Option<String>,
    pub variadic: bool,
}

impl Param {
    pub fn new(name: String, type_name: Option<String>, variadic: bool) -> Self {
        Self {
            name,
            type_name,
            variadic,
        }
    }

    pub fn type_expr(&self) -> Option<TypeExpr> {
        self.type_name.as_deref().map(TypeExpr::parse)
    }
}
