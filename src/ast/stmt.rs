//! Statement nodes.

use crate::ast::expr::{Expr, Param};
use crate::ast::node_id::NodeId;
use crate::ast::pattern::AssignTarget;
use crate::ast::span::Span;

#[derive(Clone, Debug, Eq)]
pub enum Stmt {
    Local {
        name: String,
        is_const: bool,
        is_global: bool,
        type_name: Option<String>,
        initializer: Option<Expr>,
        id: NodeId,
    },
    LocalMany {
        names: Vec<String>,
        is_const: bool,
        is_global: bool,
        initializers: Vec<Expr>,
        id: NodeId,
    },
    Assign {
        target: AssignTarget,
        value: Expr,
        is_const: bool,
        id: NodeId,
    },
    AssignMany {
        targets: Vec<AssignTarget>,
        values: Vec<Expr>,
        id: NodeId,
    },
    Increment {
        target: AssignTarget,
        amount: i8,
        id: NodeId,
    },
    Function {
        name: Option<String>,
        is_const: bool,
        params: Vec<Param>,
        return_type: Option<String>,
        body: Vec<Stmt>,
        id: NodeId,
    },
    If {
        data: Box<IfData>,
        id: NodeId,
    },
    For {
        vars: Vec<String>,
        source: Expr,
        body: Vec<Stmt>,
        id: NodeId,
    },
    NumericFor {
        data: Box<NumericForData>,
        id: NodeId,
    },
    While {
        condition: Expr,
        body: Vec<Stmt>,
        id: NodeId,
    },
    Repeat {
        body: Vec<Stmt>,
        condition: Expr,
        id: NodeId,
    },
    Return {
        values: Vec<Expr>,
        id: NodeId,
    },
    Break {
        id: NodeId,
    },
    Continue {
        id: NodeId,
    },
    Goto {
        label: String,
        id: NodeId,
    },
    Label {
        name: String,
        id: NodeId,
    },
    Expr {
        expr: Expr,
        id: NodeId,
    },
    /// `type Name = ...` or `export type Name = ...`. Erased before code
    /// generation: it exists for the checker and for tooling.
    TypeAlias {
        name: String,
        exported: bool,
        /// The aliased type as source text, like every other annotation.
        ty: String,
        /// Where `name` is written, for go-to-definition and diagnostics.
        name_span: Span,
        id: NodeId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IfData {
    pub condition: Expr,
    pub then_branch: Vec<Stmt>,
    pub else_if_branches: Vec<(Expr, Vec<Stmt>)>,
    pub else_branch: Option<Vec<Stmt>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumericForData {
    pub var: String,
    pub start: Expr,
    pub end: Expr,
    pub step: Option<Expr>,
    pub body: Vec<Stmt>,
}

impl Stmt {
    pub fn if_stmt(
        condition: Expr,
        then_branch: Vec<Stmt>,
        else_if_branches: Vec<(Expr, Vec<Stmt>)>,
        else_branch: Option<Vec<Stmt>>,
        id: NodeId,
    ) -> Self {
        Stmt::If {
            data: Box::new(IfData {
                condition,
                then_branch,
                else_if_branches,
                else_branch,
            }),
            id,
        }
    }

    pub fn numeric_for(
        var: String,
        start: Expr,
        end: Expr,
        step: Option<Expr>,
        body: Vec<Stmt>,
        id: NodeId,
    ) -> Self {
        Stmt::NumericFor {
            data: Box::new(NumericForData {
                var,
                start,
                end,
                step,
                body,
            }),
            id,
        }
    }
}

impl PartialEq for Stmt {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Stmt::Local {
                    name: na,
                    is_const: ca,
                    is_global: ga,
                    type_name: ta,
                    initializer: ia,
                    ..
                },
                Stmt::Local {
                    name: nb,
                    is_const: cb,
                    is_global: gb,
                    type_name: tb,
                    initializer: ib,
                    ..
                },
            ) => na == nb && ca == cb && ga == gb && ta == tb && ia == ib,
            (
                Stmt::LocalMany {
                    names: na,
                    is_const: ca,
                    is_global: ga,
                    initializers: ia,
                    ..
                },
                Stmt::LocalMany {
                    names: nb,
                    is_const: cb,
                    is_global: gb,
                    initializers: ib,
                    ..
                },
            ) => na == nb && ca == cb && ga == gb && ia == ib,
            (
                Stmt::Assign {
                    target: ta,
                    value: va,
                    is_const: ca,
                    ..
                },
                Stmt::Assign {
                    target: tb,
                    value: vb,
                    is_const: cb,
                    ..
                },
            ) => ta == tb && va == vb && ca == cb,
            (
                Stmt::AssignMany {
                    targets: ta,
                    values: va,
                    ..
                },
                Stmt::AssignMany {
                    targets: tb,
                    values: vb,
                    ..
                },
            ) => ta == tb && va == vb,
            (
                Stmt::Increment {
                    target: ta,
                    amount: aa,
                    ..
                },
                Stmt::Increment {
                    target: tb,
                    amount: ab,
                    ..
                },
            ) => ta == tb && aa == ab,
            (
                Stmt::Function {
                    name: na,
                    is_const: ca,
                    params: pa,
                    return_type: ra,
                    body: ba,
                    ..
                },
                Stmt::Function {
                    name: nb,
                    is_const: cb,
                    params: pb,
                    return_type: rb,
                    body: bb,
                    ..
                },
            ) => na == nb && ca == cb && pa == pb && ra == rb && ba == bb,
            (Stmt::If { data: da, .. }, Stmt::If { data: db, .. }) => da == db,
            (
                Stmt::For {
                    vars: va,
                    source: sa,
                    body: ba,
                    ..
                },
                Stmt::For {
                    vars: vb,
                    source: sb,
                    body: bb,
                    ..
                },
            ) => va == vb && sa == sb && ba == bb,
            (Stmt::NumericFor { data: da, .. }, Stmt::NumericFor { data: db, .. }) => da == db,
            (
                Stmt::While {
                    condition: ca,
                    body: ba,
                    ..
                },
                Stmt::While {
                    condition: cb,
                    body: bb,
                    ..
                },
            ) => ca == cb && ba == bb,
            (
                Stmt::Repeat {
                    body: ba,
                    condition: ca,
                    ..
                },
                Stmt::Repeat {
                    body: bb,
                    condition: cb,
                    ..
                },
            ) => ba == bb && ca == cb,
            (Stmt::Return { values: va, .. }, Stmt::Return { values: vb, .. }) => va == vb,
            (Stmt::Break { .. }, Stmt::Break { .. }) => true,
            (Stmt::Continue { .. }, Stmt::Continue { .. }) => true,
            (Stmt::Goto { label: la, .. }, Stmt::Goto { label: lb, .. }) => la == lb,
            (Stmt::Label { name: na, .. }, Stmt::Label { name: nb, .. }) => na == nb,
            (Stmt::Expr { expr: ea, .. }, Stmt::Expr { expr: eb, .. }) => ea == eb,
            (
                Stmt::TypeAlias {
                    name: na,
                    exported: xa,
                    ty: ta,
                    ..
                },
                Stmt::TypeAlias {
                    name: nb,
                    exported: xb,
                    ty: tb,
                    ..
                },
            ) => na == nb && xa == xb && ta == tb,
            _ => false,
        }
    }
}

impl Stmt {
    pub fn node_id(&self) -> NodeId {
        match self {
            Stmt::Local { id, .. }
            | Stmt::LocalMany { id, .. }
            | Stmt::Assign { id, .. }
            | Stmt::AssignMany { id, .. }
            | Stmt::Increment { id, .. }
            | Stmt::Function { id, .. }
            | Stmt::If { id, .. }
            | Stmt::For { id, .. }
            | Stmt::NumericFor { id, .. }
            | Stmt::While { id, .. }
            | Stmt::Repeat { id, .. }
            | Stmt::Return { id, .. }
            | Stmt::Break { id }
            | Stmt::Continue { id }
            | Stmt::Goto { id, .. }
            | Stmt::Label { id, .. }
            | Stmt::TypeAlias { id, .. }
            | Stmt::Expr { id, .. } => *id,
        }
    }

    pub fn is_terminator(&self) -> bool {
        matches!(
            self,
            Self::Return { .. } | Self::Break { .. } | Self::Continue { .. } | Self::Goto { .. }
        )
    }
}
