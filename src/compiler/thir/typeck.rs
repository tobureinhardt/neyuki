//! Type inference and lexical binding resolution for THIR lowering.

use std::collections::{HashMap, HashSet};

use crate::ast::literal::Literal;
use crate::ast::op::{BinOp, UnOp};
use crate::compiler::thir::nodes::{BindingId, BindingKind, ThirExpr, ThirExprKind, VarBinding};
use crate::sema::types::NeyukiType;

pub(crate) struct ScopeFrame {
    pub(crate) locals: HashMap<String, BindingId>,
}

pub(crate) struct FnCaptureFrame {
    pub(crate) scope_depth: usize,
    pub(crate) captures: Vec<BindingId>,
    pub(crate) capture_set: HashSet<BindingId>,
}

pub(crate) struct TypeContext {
    pub(crate) bindings: Vec<VarBinding>,
    pub(crate) scopes: Vec<ScopeFrame>,
    pub(crate) fn_frames: Vec<FnCaptureFrame>,
}

impl TypeContext {
    pub(crate) fn new() -> Self {
        Self {
            bindings: Vec::new(),
            scopes: vec![ScopeFrame {
                locals: HashMap::new(),
            }],
            fn_frames: Vec::new(),
        }
    }

    pub(crate) fn enter_scope(&mut self) {
        self.scopes.push(ScopeFrame {
            locals: HashMap::new(),
        });
    }

    pub(crate) fn exit_scope(&mut self) {
        if self.scopes.len() > 1 {
            self.scopes.pop();
        }
    }

    pub(crate) fn enter_fn(&mut self) {
        self.fn_frames.push(FnCaptureFrame {
            scope_depth: self.scopes.len(),
            captures: Vec::new(),
            capture_set: HashSet::new(),
        });
        self.enter_scope();
    }

    pub(crate) fn exit_fn(&mut self) -> Vec<BindingId> {
        self.exit_scope();
        self.fn_frames.pop().map(|f| f.captures).unwrap_or_default()
    }

    pub(crate) fn alloc_binding(
        &mut self,
        name: String,
        kind: BindingKind,
        is_const: bool,
        ty: NeyukiType,
    ) -> BindingId {
        let id = BindingId(self.bindings.len() as u32);
        self.bindings.push(VarBinding {
            id,
            name: name.clone(),
            kind,
            is_const,
            ty,
        });
        if kind != BindingKind::Global
            && let Some(scope) = self.scopes.last_mut()
        {
            scope.locals.insert(name, id);
        }
        id
    }

    pub(crate) fn resolve_var(&mut self, name: &str) -> (Option<BindingId>, NeyukiType) {
        let min_local_depth = self.fn_frames.last().map(|f| f.scope_depth).unwrap_or(0);

        for idx in (0..self.scopes.len()).rev() {
            if let Some(&id) = self.scopes[idx].locals.get(name) {
                if idx < min_local_depth {
                    for frame in self.fn_frames.iter_mut().rev() {
                        if idx < frame.scope_depth && frame.capture_set.insert(id) {
                            frame.captures.push(id);
                        }
                    }
                    if let Some(binding) = self.bindings.get_mut(id.0 as usize)
                        && binding.kind == BindingKind::Local
                    {
                        binding.kind = BindingKind::Upvalue;
                    }
                }
                let ty = self.bindings[id.0 as usize].ty.clone();
                return (Some(id), ty);
            }
        }
        (None, NeyukiType::Any)
    }

    pub(crate) fn refine_binding_type(&mut self, id: BindingId, ty: NeyukiType) {
        if let Some(binding) = self.bindings.get_mut(id.0 as usize)
            && binding.ty == NeyukiType::Any
            && ty != NeyukiType::Any
        {
            binding.ty = ty;
        }
    }
}

pub(crate) fn infer_literal_type(lit: &Literal) -> NeyukiType {
    match lit {
        Literal::Nil => NeyukiType::Nil,
        Literal::Bool(_) => NeyukiType::Boolean,
        Literal::Int(_) => NeyukiType::Int,
        Literal::Float(_) => NeyukiType::Float,
        Literal::String(_) => NeyukiType::String,
    }
}

pub(crate) fn infer_unary_type(op: UnOp, inner: &ThirExpr) -> NeyukiType {
    match op {
        UnOp::Not => NeyukiType::Boolean,
        UnOp::Len | UnOp::BitNot => NeyukiType::Int,
        UnOp::Neg => match inner.ty {
            NeyukiType::Int => NeyukiType::Int,
            NeyukiType::Float => NeyukiType::Float,
            NeyukiType::Number => NeyukiType::Number,
            _ => NeyukiType::Number,
        },
    }
}

pub(crate) fn infer_binary_type(op: BinOp, left: &ThirExpr, right: &ThirExpr) -> NeyukiType {
    match op {
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            NeyukiType::Boolean
        }
        BinOp::Concat => NeyukiType::String,
        BinOp::Coalesce => left.ty.narrow_non_nil().lub(&right.ty),
        BinOp::And => {
            if !can_be_falsy(&left.ty) {
                right.ty.clone()
            } else {
                left.ty.lub(&right.ty)
            }
        }
        BinOp::Or => left.ty.lub(&right.ty),
        BinOp::BitAnd
        | BinOp::BitOr
        | BinOp::BitXor
        | BinOp::Shl
        | BinOp::Shr
        | BinOp::LShl
        | BinOp::LShr
        | BinOp::IDiv
        | BinOp::Mod => NeyukiType::Int,
        BinOp::Div => NeyukiType::Float,
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Pow => {
            if left.ty == NeyukiType::Any || right.ty == NeyukiType::Any {
                NeyukiType::Any
            } else if left.ty == NeyukiType::Int && right.ty == NeyukiType::Int {
                NeyukiType::Int
            } else if left.ty == NeyukiType::Float || right.ty == NeyukiType::Float {
                NeyukiType::Float
            } else {
                NeyukiType::Number
            }
        }
    }
}

fn can_be_falsy(ty: &NeyukiType) -> bool {
    match ty {
        NeyukiType::Nil | NeyukiType::Boolean | NeyukiType::Any | NeyukiType::Optional(_) => true,
        NeyukiType::Union(parts) => parts.iter().any(can_be_falsy),
        _ => false,
    }
}

pub(crate) fn infer_for_var_types(source_ty: &NeyukiType, count: usize) -> Vec<NeyukiType> {
    if count == 0 {
        return Vec::new();
    }
    let mut out = vec![NeyukiType::Any; count];
    match source_ty {
        NeyukiType::Array(elem) => {
            out[0] = NeyukiType::Int;
            if count > 1 {
                out[1] = (**elem).clone();
            }
        }
        NeyukiType::TableMap { key, val } => {
            out[0] = (**key).clone();
            if count > 1 {
                out[1] = (**val).clone();
            }
        }
        NeyukiType::Record(fields) if !fields.is_empty() => {
            out[0] = NeyukiType::String;
            if count > 1 {
                out[1] = fields
                    .iter()
                    .fold(NeyukiType::Never, |acc, (_, t)| acc.lub(t));
            }
        }
        _ => {}
    }
    out
}

pub(crate) struct IfNarrowing {
    pub(crate) then_branch: Option<(BindingId, NeyukiType)>,
    pub(crate) else_branch: Option<(BindingId, NeyukiType)>,
}

pub(crate) fn extract_if_narrowing(cond: &ThirExpr) -> IfNarrowing {
    let none = || IfNarrowing {
        then_branch: None,
        else_branch: None,
    };
    match &cond.kind {
        ThirExprKind::Variable {
            binding: Some(id), ..
        } => IfNarrowing {
            then_branch: Some((*id, cond.ty.narrow_non_nil())),
            else_branch: None,
        },
        ThirExprKind::Unary {
            op: UnOp::Not,
            expr,
        } => {
            if let ThirExprKind::Variable {
                binding: Some(id), ..
            } = &expr.kind
            {
                IfNarrowing {
                    then_branch: None,
                    else_branch: Some((*id, expr.ty.narrow_non_nil())),
                }
            } else {
                none()
            }
        }
        ThirExprKind::Binary {
            left,
            op: op @ (BinOp::Eq | BinOp::Ne),
            right,
        } => {
            let nil_var = match (&left.kind, &right.kind) {
                (
                    ThirExprKind::Variable {
                        binding: Some(id), ..
                    },
                    ThirExprKind::Literal(Literal::Nil),
                ) => Some((*id, left.ty.narrow_non_nil())),
                (
                    ThirExprKind::Literal(Literal::Nil),
                    ThirExprKind::Variable {
                        binding: Some(id), ..
                    },
                ) => Some((*id, right.ty.narrow_non_nil())),
                _ => None,
            };
            if let Some(pair) = nil_var {
                return if *op == BinOp::Ne {
                    IfNarrowing {
                        then_branch: Some(pair),
                        else_branch: None,
                    }
                } else {
                    IfNarrowing {
                        then_branch: None,
                        else_branch: Some(pair),
                    }
                };
            }
            if *op == BinOp::Eq {
                let type_call = match (&left.kind, &right.kind) {
                    (
                        ThirExprKind::Call { callee, args },
                        ThirExprKind::Literal(Literal::String(s)),
                    )
                    | (
                        ThirExprKind::Literal(Literal::String(s)),
                        ThirExprKind::Call { callee, args },
                    ) if matches!(
                        &callee.kind,
                        ThirExprKind::Variable {
                            name,
                            binding: None
                        } if name == "type"
                    ) && args.len() == 1 =>
                    {
                        if let ThirExprKind::Variable {
                            binding: Some(id), ..
                        } = &args[0].kind
                        {
                            let narrowed = match s.as_str() {
                                "number" => Some(NeyukiType::Number),
                                "string" => Some(NeyukiType::String),
                                "boolean" => Some(NeyukiType::Boolean),
                                "table" => Some(NeyukiType::Table),
                                "function" => Some(NeyukiType::Function {
                                    params: Vec::new(),
                                    return_type: Box::new(NeyukiType::Any),
                                    is_vararg: true,
                                }),
                                "thread" => Some(NeyukiType::Thread),
                                "buffer" => Some(NeyukiType::Buffer),
                                "nil" => Some(NeyukiType::Nil),
                                _ => None,
                            };
                            narrowed.map(|ty| (*id, ty))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                if let Some(pair) = type_call {
                    return IfNarrowing {
                        then_branch: Some(pair),
                        else_branch: None,
                    };
                }
            }
            none()
        }
        _ => none(),
    }
}

pub(crate) fn infer_member_type(object: &ThirExpr, field: &str) -> NeyukiType {
    if let NeyukiType::Record(fields) = &object.ty
        && let Some((_, field_ty)) = fields.iter().find(|(k, _)| k == field)
    {
        return field_ty.clone();
    }
    NeyukiType::Any
}

pub(crate) fn infer_index_type(object: &ThirExpr) -> NeyukiType {
    match &object.ty {
        NeyukiType::Array(elem) => (**elem).clone(),
        NeyukiType::TableMap { val, .. } => (**val).clone(),
        _ => NeyukiType::Any,
    }
}

pub(crate) fn infer_call_type(callee: &ThirExpr, args: &[ThirExpr]) -> NeyukiType {
    match &callee.ty {
        NeyukiType::Generic { .. } => {
            let arg_tys: Vec<NeyukiType> = args.iter().map(|a| a.ty.clone()).collect();
            crate::sema::generics::instantiate_call(&callee.ty, &arg_tys)
                .map(|(ret, _)| ret)
                .unwrap_or(NeyukiType::Any)
        }
        NeyukiType::Function { return_type, .. } => (**return_type).clone(),
        _ => NeyukiType::Any,
    }
}

pub(crate) fn infer_table_type(entries: &[(Option<&str>, bool, &NeyukiType)]) -> NeyukiType {
    if entries.is_empty() {
        return NeyukiType::Table;
    }
    if entries.iter().all(|(_, is_arr, _)| *is_arr) {
        let elem = entries
            .iter()
            .fold(NeyukiType::Never, |acc, (_, _, ty)| acc.lub(ty));
        return NeyukiType::Array(Box::new(elem));
    }
    let mut fields = Vec::new();
    for (key, _, ty) in entries {
        let Some(k) = key else {
            return NeyukiType::Table;
        };
        fields.push(((*k).to_string(), (*ty).clone()));
    }
    NeyukiType::Record(fields)
}

pub(crate) fn infer_body_return_type(
    stmts: &[crate::compiler::thir::nodes::ThirStmt],
) -> NeyukiType {
    let mut acc = NeyukiType::Never;
    let mut saw_return = false;
    for stmt in stmts {
        collect_return_type(stmt, &mut acc, &mut saw_return);
    }
    if saw_return { acc } else { NeyukiType::Nil }
}

fn collect_return_type(
    stmt: &crate::compiler::thir::nodes::ThirStmt,
    acc: &mut NeyukiType,
    saw_return: &mut bool,
) {
    use crate::compiler::thir::nodes::ThirStmtKind;
    match &stmt.kind {
        ThirStmtKind::Return(vals) => {
            *saw_return = true;
            let ret_ty = vals
                .first()
                .map(|e| e.ty.clone())
                .unwrap_or(NeyukiType::Nil);
            *acc = acc.lub(&ret_ty);
        }
        ThirStmtKind::Block(body)
        | ThirStmtKind::While { body, .. }
        | ThirStmtKind::Repeat { body, .. }
        | ThirStmtKind::For { body, .. } => {
            for s in body {
                collect_return_type(s, acc, saw_return);
            }
        }
        ThirStmtKind::NumericFor(data) => {
            for s in &data.body {
                collect_return_type(s, acc, saw_return);
            }
        }
        ThirStmtKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            for s in then_branch {
                collect_return_type(s, acc, saw_return);
            }
            if let Some(else_b) = else_branch {
                for s in else_b {
                    collect_return_type(s, acc, saw_return);
                }
            }
        }
        _ => {}
    }
}

pub(crate) fn is_module_import_member(expr: &ThirExprKind) -> bool {
    matches!(expr, ThirExprKind::Variable { binding: None, .. })
}
