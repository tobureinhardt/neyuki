//! Semantic checks: scopes, `const`, types, arity and dead code.

pub mod analyzer;
pub mod scope;
pub mod symbol;
pub mod types;

pub use analyzer::SemanticAnalyzer;
pub use scope::ScopeManager;
pub use symbol::{Symbol, SymbolKind};
pub use types::NeyukiType;

use crate::ast::stmt::Stmt;
use crate::diagnostics::diagnostic::Diagnostic;

pub fn analyze(statements: &[Stmt], source: &str) -> Vec<Diagnostic> {
    let mut analyzer = SemanticAnalyzer::new(source);
    analyzer.analyze_program(statements);
    analyzer.diagnostics
}

/// Like `analyze`, placing each diagnostic with the parser's node spans.
pub fn analyze_with_spans(
    statements: &[Stmt],
    source: &str,
    spans: &crate::ast::span::SpanPool,
) -> Vec<Diagnostic> {
    let mut analyzer = SemanticAnalyzer::new(source).with_spans(spans);
    analyzer.analyze_program(statements);
    analyzer.diagnostics
}

/// The names the program uses without anything declaring them.
pub fn undeclared_names(
    statements: &[Stmt],
    source: &str,
    spans: &crate::ast::span::SpanPool,
) -> std::collections::HashSet<String> {
    let mut analyzer = SemanticAnalyzer::new(source).with_spans(spans);
    analyzer.analyze_program(statements);
    analyzer.undeclared
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::compile_source;
    use crate::diagnostics::error_code::ErrorCode;

    #[test]
    fn test_sema_undeclared_variable() {
        let code = "return unknown_var + 1";
        let stmts = compile_source(code).expect("syntax error");
        let diags = analyze(&stmts, code);
        assert!(diags.iter().any(|d| d.code == Some(ErrorCode::E0001)));
    }

    #[test]
    fn test_sema_reassign_const() {
        let code = "local const x = 10\nx = 20";
        let stmts = compile_source(code).expect("syntax error");
        let diags = analyze(&stmts, code);
        assert!(diags.iter().any(|d| d.code == Some(ErrorCode::E0002)));
    }

    #[test]
    fn test_sema_type_mismatch() {
        let code = "local x: number = \"hello\"\nreturn x";
        let stmts = compile_source(code).expect("syntax error");
        let diags = analyze(&stmts, code);
        assert!(diags.iter().any(|d| d.code == Some(ErrorCode::E0003)));
    }

    #[test]
    fn test_sema_function_arity() {
        let code = "local function add(a, b)\n  return a + b\nend\nadd(1)";
        let stmts = compile_source(code).expect("syntax error");
        let diags = analyze(&stmts, code);
        assert!(diags.iter().any(|d| d.code == Some(ErrorCode::E0004)));
    }

    #[test]
    fn test_sema_unused_variable() {
        let code = "local unused_val = 42\nreturn 0";
        let stmts = compile_source(code).expect("syntax error");
        let diags = analyze(&stmts, code);
        assert!(diags.iter().any(|d| d.code == Some(ErrorCode::W0001)));
    }

    #[test]
    fn test_sema_shadowing() {
        let code = "local x = 10\nif true then\n  local x = 20\n  print(x)\nend\nreturn x";
        let stmts = compile_source(code).expect("syntax error");
        let diags = analyze(&stmts, code);
        assert!(diags.iter().any(|d| d.code == Some(ErrorCode::W0003)));
    }

    #[test]
    fn test_sema_unreachable_code() {
        let code = "return 1\nlocal a = 2\nprint(a)";
        let stmts = compile_source(code).expect("syntax error");
        let diags = analyze(&stmts, code);
        assert!(diags.iter().any(|d| d.code == Some(ErrorCode::W0004)));
    }

    #[test]
    fn test_sema_optional_and_union_types() {
        let code_valid = "local x: string? = nil\nlocal y: int | string = 42\nreturn x, y";
        let stmts_valid = compile_source(code_valid).expect("syntax error");
        let diags_valid = analyze(&stmts_valid, code_valid);
        assert!(!diags_valid.iter().any(|d| d.code == Some(ErrorCode::E0003)));

        let code_invalid = "local x: int | string = true\nreturn x";
        let stmts_invalid = compile_source(code_invalid).expect("syntax error");
        let diags_invalid = analyze(&stmts_invalid, code_invalid);
        assert!(
            diags_invalid
                .iter()
                .any(|d| d.code == Some(ErrorCode::E0003))
        );
    }
}
