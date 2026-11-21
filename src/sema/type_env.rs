//! Type annotation parsing, type-alias declarations, and bounded alias expansion.

use crate::ast::expr::Param;
use crate::ast::span::Span as AstSpan;
use crate::ast::stmt::Stmt;
use crate::ast::ty::TypeExpr;
use crate::diagnostics::diagnostic::Diagnostic;
use crate::diagnostics::error_code::ErrorCode;
use crate::diagnostics::span::Span;
use crate::sema::analyzer::SemanticAnalyzer;
use crate::sema::scope::TypeAlias;
use crate::sema::types::NeyukiType;

/// How many alias names one annotation may expand to. Aliases that nest
/// aliases can multiply (`type A = {B, B}`, `type B = {C, C}`, ...), so the
/// expansion is cut off, leaving `any`, instead of growing without bound.
const ALIAS_EXPANSION_BUDGET: usize = 512;

impl<'a> SemanticAnalyzer<'a> {
    pub(super) fn parse_ty(&self, text: &str) -> NeyukiType {
        if !self.scope_mgr.has_types() {
            return NeyukiType::parse(text);
        }
        let mut active = Vec::new();
        let mut budget = ALIAS_EXPANSION_BUDGET;
        let expanded = self.expand_aliases(&TypeExpr::parse(text), &mut active, &mut budget);
        NeyukiType::from_ast_type(&expanded)
    }

    /// Replaces each alias name in `ty` with the type it stands for. A
    /// reference back to an alias that is being expanded (a recursive type)
    /// becomes `any`, as does anything past the expansion budget, so
    /// expansion always ends and stays cheap.
    pub(super) fn expand_aliases(
        &self,
        ty: &TypeExpr,
        active: &mut Vec<String>,
        budget: &mut usize,
    ) -> TypeExpr {
        match ty {
            TypeExpr::Named(name) => {
                let Some(alias) = self.scope_mgr.lookup_type(name) else {
                    return ty.clone();
                };
                if active.contains(name) || *budget == 0 {
                    return TypeExpr::Any;
                }
                *budget -= 1;
                active.push(name.clone());
                let expanded = self.expand_aliases(&alias.ty, active, budget);
                active.pop();
                expanded
            }
            TypeExpr::Optional(inner) => {
                TypeExpr::Optional(Box::new(self.expand_aliases(inner, active, budget)))
            }
            TypeExpr::Array(inner) => {
                TypeExpr::Array(Box::new(self.expand_aliases(inner, active, budget)))
            }
            TypeExpr::Tuple(elems) => TypeExpr::Tuple(
                elems
                    .iter()
                    .map(|e| self.expand_aliases(e, active, budget))
                    .collect(),
            ),
            TypeExpr::Record(fields) => TypeExpr::Record(
                fields
                    .iter()
                    .map(|(k, v)| (k.clone(), self.expand_aliases(v, active, budget)))
                    .collect(),
            ),
            TypeExpr::Table { key, val } => TypeExpr::Table {
                key: Box::new(self.expand_aliases(key, active, budget)),
                val: Box::new(self.expand_aliases(val, active, budget)),
            },
            TypeExpr::Function { params, ret } => TypeExpr::Function {
                params: params
                    .iter()
                    .map(|e| self.expand_aliases(e, active, budget))
                    .collect(),
                ret: Box::new(self.expand_aliases(ret, active, budget)),
            },
            TypeExpr::Union(variants) => TypeExpr::Union(
                variants
                    .iter()
                    .map(|e| self.expand_aliases(e, active, budget))
                    .collect(),
            ),
            TypeExpr::Nil | TypeExpr::Never | TypeExpr::Any => ty.clone(),
        }
    }

    /// Records `type name = ...` in the current scope.
    pub(super) fn declare_type_alias(
        &mut self,
        stmt: &Stmt,
        name: &str,
        ty: &str,
        name_span: AstSpan,
    ) {
        self.enter_stmt(stmt);
        let span = if name_span.start.line > 0 {
            Span::single_line(
                name_span.start.line,
                name_span.start.column,
                name_span.end.column,
            )
        } else {
            self.find_ident_span(name)
        };
        if !matches!(
            NeyukiType::from_ast_type(&TypeExpr::Named(name.to_string())),
            NeyukiType::Custom(_)
        ) {
            let diag = Diagnostic::error(format!("cannot redefine the built-in type '{}'", name))
                .with_code(ErrorCode::E0005)
                .with_label(span, "this name is already a built-in type");
            self.diagnostics.push(diag);
            return;
        }
        let alias = TypeAlias {
            ty: TypeExpr::parse(ty),
            span,
        };
        if let Err(orig_span) = self.scope_mgr.define_type(name, alias) {
            let diag = Diagnostic::error(format!("duplicate type '{}' in the same scope", name))
                .with_code(ErrorCode::E0005)
                .with_label(span, "redefined here")
                .with_secondary_label(orig_span, "previous definition was here");
            self.diagnostics.push(diag);
        }
    }
}

/// How many arguments a call must pass: the fixed parameters up to the last
/// one that doesn't accept nil, since trailing optional ones may be left out.
pub(super) fn required_params(params: &[Param], parse: impl Fn(&str) -> NeyukiType) -> usize {
    let fixed: Vec<&Param> = params.iter().filter(|p| !p.variadic).collect();
    fixed
        .iter()
        .rposition(|p| match p.type_name.as_deref().map(&parse) {
            Some(NeyukiType::Any) => true,
            Some(ty) => !NeyukiType::Nil.is_assignable_to(&ty),
            None => true,
        })
        .map_or(0, |i| i + 1)
}
