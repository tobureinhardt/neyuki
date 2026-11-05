//! Declarations: functions, locals and type aliases.

use crate::ast::expr::{Expr, Param};
use crate::ast::span::Span;
use crate::ast::stmt::Stmt;
use crate::ast::ty::TypeExpr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decl {
    Local {
        name: String,
        is_const: bool,
        ty: Option<TypeExpr>,
        initializer: Option<Expr>,
        span: Span,
    },
    Function {
        name: Option<String>,
        is_const: bool,
        params: Vec<Param>,
        return_type: Option<TypeExpr>,
        body: Vec<Stmt>,
        span: Span,
    },
}

impl Decl {
    pub fn span(&self) -> Span {
        match self {
            Self::Local { span, .. } | Self::Function { span, .. } => *span,
        }
    }
}

/// A type a file exports with `export type`, for tooling that resolves
/// qualified names such as `m.Foo`. The compiler never reads other files.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportedType {
    pub name: String,
    /// The aliased type as written in the source.
    pub ty: String,
    /// Where the name is written, for go-to-definition.
    pub span: Span,
}

/// The `export type` declarations of a program. They are only allowed at the
/// top level, so nested statements are not searched.
pub fn exported_types(stmts: &[Stmt]) -> Vec<ExportedType> {
    stmts
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::TypeAlias {
                name,
                exported: true,
                ty,
                name_span,
                ..
            } => Some(ExportedType {
                name: name.clone(),
                ty: ty.clone(),
                span: *name_span,
            }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::compile_program;

    #[test]
    fn exported_types_lists_top_level_exports_with_name_spans() {
        let source = "type Local = number\nexport type Options = { name: string? }\nexport type Id = number | string\nreturn 1";
        let parsed = crate::compiler::parse(source).expect("syntax error");
        let exported = exported_types(&parsed.stmts);
        let names: Vec<&str> = exported.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Options", "Id"]);
        assert_eq!(exported[0].ty, "{ name: string? }");
        assert_eq!(exported[1].ty, "number | string");
        // The span covers just the name, on the line it is written.
        assert_eq!(exported[0].span.start.line, 2);
        assert_eq!(exported[0].span.end.line, 2);
        assert_eq!(
            exported[0].span.end.column - exported[0].span.start.column,
            "Options".len() as u32
        );
        assert_eq!(exported[1].span.start.line, 3);
        assert!(compile_program(source, "t", crate::compiler::Options::RUN).is_ok());
    }
}
