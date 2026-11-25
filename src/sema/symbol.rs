//! Symbols and symbol tables.

use crate::diagnostics::span::Span;
use crate::sema::types::NeyukiType;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SymbolKind {
    Variable,
    Parameter,
    Function,
}

#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub is_const: bool,
    pub declared_type: Option<NeyukiType>,
    pub inferred_type: NeyukiType,
    pub span: Span,
    pub used: bool,
    pub assigned_count: usize,
    pub num_params: Option<usize>,
    /// Arguments a call must pass; trailing optional parameters may be left
    /// out.
    pub min_params: Option<usize>,
    pub is_vararg: bool,
    pub allow_unused: bool,
    pub deprecated: Option<String>,
}

impl Symbol {
    pub fn new(
        name: String,
        kind: SymbolKind,
        is_const: bool,
        declared_type: Option<NeyukiType>,
        span: Span,
    ) -> Self {
        let inferred_type = declared_type.clone().unwrap_or(NeyukiType::Any);
        Self {
            name,
            kind,
            is_const,
            declared_type,
            inferred_type,
            span,
            used: false,
            assigned_count: 0,
            num_params: None,
            min_params: None,
            is_vararg: false,
            allow_unused: false,
            deprecated: None,
        }
    }
}
