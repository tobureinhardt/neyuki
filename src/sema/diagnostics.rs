//! Span resolution and diagnostic reporting helpers for semantic analysis.

use crate::ast::expr::Expr;
use crate::ast::span::Span as AstSpan;
use crate::ast::stmt::Stmt;
use crate::diagnostics::diagnostic::Diagnostic;
use crate::diagnostics::error_code::ErrorCode;
use crate::diagnostics::span::Span;
use crate::sema::analyzer::SemanticAnalyzer;
use crate::sema::symbol::SymbolKind;

impl<'a> SemanticAnalyzer<'a> {
    /// Moves `current_line` to where `stmt` starts.
    pub(super) fn enter_stmt(&mut self, stmt: &Stmt) {
        if let Some(span) = self.spans.and_then(|pool| pool.get(stmt.node_id()))
            && span.start.line > 0
        {
            self.current_line = span.start.line;
        }
    }

    /// Converts a parser span to a diagnostic span on its first line, with
    /// trailing whitespace trimmed (a parser span ends at the next token).
    pub(super) fn diagnostic_span(&self, span: AstSpan) -> Span {
        let line = span.start.line;
        let text = self
            .source
            .lines()
            .nth(line.saturating_sub(1) as usize)
            .unwrap_or("");
        let start = span.start.column.max(1);
        let mut end = if span.end.line == line && span.end.column > start {
            span.end.column
        } else {
            text.len() as u32 + 1
        };
        while end > start + 1
            && text
                .as_bytes()
                .get(end as usize - 2)
                .is_some_and(|b| b.is_ascii_whitespace())
        {
            end -= 1;
        }
        Span::single_line(line, start, end)
    }

    /// The first whole-word occurrence of `ident` on the current line.
    pub(super) fn find_ident_span(&self, ident: &str) -> Span {
        let line = self.current_line;
        let text = self
            .source
            .lines()
            .nth(line.saturating_sub(1) as usize)
            .unwrap_or("");
        let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        let bytes = text.as_bytes();
        let mut from = 0;
        while let Some(found) = text[from..].find(ident) {
            let col = from + found;
            let after = col + ident.len();
            if (col == 0 || !is_word(bytes[col - 1]))
                && bytes.get(after).is_none_or(|b| !is_word(*b))
            {
                // Columns count characters, as the tokenizer's do; `col` and
                // `after` are byte offsets.
                let start = text[..col].chars().count() as u32 + 1;
                return Span::single_line(line, start, start + ident.chars().count() as u32);
            }
            from = col + 1;
        }
        Span::line(line)
    }

    pub(super) fn find_stmt_span(&self, stmt: &Stmt) -> Span {
        match self.spans.and_then(|pool| pool.get(stmt.node_id())) {
            Some(span) if span.start.line > 0 => self.diagnostic_span(span),
            _ => Span::line(self.current_line),
        }
    }

    pub(super) fn find_expr_span(&self, expr: &Expr) -> Span {
        match self.spans.and_then(|pool| pool.get(expr.node_id())) {
            Some(span) if span.start.line > 0 => self.diagnostic_span(span),
            _ => Span::line(self.current_line),
        }
    }

    #[inline(never)]
    pub(super) fn warn_unreachable(&mut self, stmt: &Stmt) {
        self.enter_stmt(stmt);
        let span = self.find_stmt_span(stmt);
        let diag = Diagnostic::warning("unreachable code")
            .with_code(ErrorCode::W0004)
            .with_label(span, "unreachable statement follows terminal control flow");
        self.diagnostics.push(diag);
    }

    pub(super) fn check_shadowing(&mut self, name: &str, span: Span) {
        if let Some(outer) = self.scope_mgr.find_outer(name) {
            let diag = Diagnostic::warning(format!(
                "declaration of '{}' shadows a variable in an outer scope",
                name
            ))
            .with_code(ErrorCode::W0003)
            .with_label(span, "shadows previous declaration")
            .with_secondary_label(outer.span, "outer variable was declared here");
            self.diagnostics.push(diag);
        }
    }

    pub(super) fn check_unused_symbols(&mut self) {
        let symbols = self.scope_mgr.exit_scope();
        for sym in symbols {
            // `self` is part of the method calling convention, even in a
            // method that doesn't read it.
            let is_self = sym.kind == SymbolKind::Parameter && sym.name == "self";
            if !sym.used && !sym.allow_unused && !sym.name.starts_with('_') && !is_self {
                let (code, msg, label_msg) = if sym.kind == SymbolKind::Parameter {
                    (
                        ErrorCode::W0002,
                        format!("unused parameter '{}'", sym.name),
                        "parameter never used",
                    )
                } else {
                    (
                        ErrorCode::W0001,
                        format!("unused variable '{}'", sym.name),
                        "variable never used",
                    )
                };
                let diag = Diagnostic::warning(msg)
                    .with_code(code)
                    .with_label(sym.span, label_msg)
                    .with_help(format!(
                        "if this is intentional, prefix with an underscore: '_{}'",
                        sym.name
                    ));
                self.diagnostics.push(diag);
            }
        }
    }

    pub(super) fn flush_remaining_unused(&mut self) {
        let remaining = self.scope_mgr.flush_all();
        for sym in remaining {
            if !sym.used
                && !sym.allow_unused
                && !sym.name.starts_with('_')
                && sym.kind == SymbolKind::Variable
            {
                let diag = Diagnostic::warning(format!("unused variable '{}'", sym.name))
                    .with_code(ErrorCode::W0001)
                    .with_label(sym.span, "variable declared here but never used")
                    .with_help(format!(
                        "if this is intentional, prefix with an underscore: '_{}'",
                        sym.name
                    ));
                self.diagnostics.push(diag);
            }
        }
    }
}
