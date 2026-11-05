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

    fn diags(code: &str) -> Vec<Diagnostic> {
        let stmts = compile_source(code).expect("syntax error");
        analyze(&stmts, code)
    }

    fn has_code(diags: &[Diagnostic], code: ErrorCode) -> bool {
        diags.iter().any(|d| d.code == Some(code))
    }

    #[test]
    fn test_type_alias_is_checked() {
        let ok = diags(
            "type Point = { x: number, y: number }\nlocal p: Point = { x = 1, y = 2 }\nreturn p",
        );
        assert!(!has_code(&ok, ErrorCode::E0003));

        let bad = diags("type Point = { x: number, y: number }\nlocal p: Point = 5\nreturn p");
        assert!(has_code(&bad, ErrorCode::E0003));

        let bad_union = diags("type Id = number | string\nlocal i: Id = true\nreturn i");
        assert!(has_code(&bad_union, ErrorCode::E0003));
    }

    #[test]
    fn test_type_alias_forward_and_recursive_references() {
        let forward = diags("local p: Point = 5\ntype Point = { x: number }\nreturn p");
        assert!(has_code(&forward, ErrorCode::E0003));

        let tree = diags(
            "type Tree = { children: {Tree}, label: string }\nlocal t: Tree = { children = {}, label = \"a\" }\nreturn t",
        );
        assert!(!has_code(&tree, ErrorCode::E0003));

        let cycle =
            diags("type A = { b: B }\ntype B = { a: A }\nlocal v: A = { b = {} }\nreturn v");
        assert!(!has_code(&cycle, ErrorCode::E0003));

        let selfish = diags("type T = T\nlocal v: T = 1\nreturn v");
        assert!(!has_code(&selfish, ErrorCode::E0003));
    }

    #[test]
    fn test_type_alias_blowup_is_bounded() {
        let mut code = String::new();
        for i in 0..40 {
            code.push_str(&format!(
                "type T{} = {{ a: T{}, b: T{} }}\n",
                i,
                i + 1,
                i + 1
            ));
        }
        code.push_str("type T40 = number\nlocal v: T0 = {}\nreturn v");
        // Must finish quickly instead of expanding 2^40 times.
        let _ = diags(&code);
    }

    #[test]
    fn test_type_alias_scoping_and_duplicates() {
        let shadow = diags(
            "type P = number\ndo\n  type P = string\n  local s: P = \"x\"\n  print(s)\nend\nlocal n: P = 1\nreturn n",
        );
        assert!(!has_code(&shadow, ErrorCode::E0003));
        assert!(!has_code(&shadow, ErrorCode::E0005));

        let dup = diags("type A = number\ntype A = string\nreturn 1");
        assert!(has_code(&dup, ErrorCode::E0005));

        let builtin = diags("type string = number\nreturn 1");
        assert!(has_code(&builtin, ErrorCode::E0005));

        // Types and variables live in separate namespaces.
        let both = diags("type Point = { x: number }\nlocal Point = {}\nreturn Point");
        assert!(!has_code(&both, ErrorCode::E0005));
    }

    #[test]
    fn test_undeclared_and_qualified_names_stay_unchecked() {
        let undeclared = diags("local x: Unknown = 5\nreturn x");
        assert!(!has_code(&undeclared, ErrorCode::E0003));

        let qualified = diags("local m = {}\nlocal x: m.Foo = 5\nreturn x");
        assert!(!has_code(&qualified, ErrorCode::E0003));

        let in_function = diags(
            "local m = {}\nlocal function f(a: m.Foo, b: {m.Foo}): m.Foo?\n  return a\nend\nreturn f",
        );
        assert!(!has_code(&in_function, ErrorCode::E0003));
    }

    #[test]
    fn test_type_alias_syntax_errors() {
        let nested = compile_source("do\n  export type A = number\nend").unwrap_err();
        assert!(nested.contains("top level"), "{}", nested);

        let in_function =
            compile_source("local function f()\n  export type A = number\nend").unwrap_err();
        assert!(in_function.contains("top level"), "{}", in_function);

        let generic = compile_source("type Box<T> = { value: T }").unwrap_err();
        assert!(
            generic.contains("generic type aliases are not supported"),
            "{}",
            generic
        );

        let generic_export = compile_source("export type Box<T> = { value: T }").unwrap_err();
        assert!(
            generic_export.contains("generic type aliases are not supported"),
            "{}",
            generic_export
        );

        assert!(compile_source("type A =").is_err());
    }

    #[test]
    fn test_type_and_export_stay_ordinary_names() {
        for code in [
            "return type(1)",
            "local type = 1\nreturn type",
            "local type = 0\ntype = 1\nreturn type",
            "local export = 1\nexport = 2\nreturn export",
            "local export = { foo = function() return 1 end }\nreturn export.foo()",
            "local t = type\nreturn t(1)",
        ] {
            let stmts = compile_source(code).unwrap_or_else(|e| panic!("{}: {}", code, e));
            assert!(
                !stmts
                    .iter()
                    .any(|s| matches!(s, crate::ast::stmt::Stmt::TypeAlias { .. })),
                "{} parsed as a type declaration",
                code
            );
        }
    }

    #[test]
    fn test_type_alias_shapes_parse() {
        let code = "type A = number | string\ntype B =\n  | number\n  | string\ntype C = {\n  x: number,\n  y: number,\n}\nexport type D = ((events: {any}, dt: number) -> ())?\nlocal v = 1\nreturn v";
        let stmts = compile_source(code).expect("syntax error");
        let aliases: Vec<_> = stmts
            .iter()
            .filter_map(|s| match s {
                crate::ast::stmt::Stmt::TypeAlias {
                    name, exported, ty, ..
                } => Some((name.as_str(), *exported, ty.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(
            aliases,
            vec![
                ("A", false, "number | string"),
                ("B", false, "number | string"),
                ("C", false, "{ x: number, y: number, }"),
                ("D", true, "((events: { any }, dt: number) -> ())?"),
            ]
        );
        assert_eq!(stmts.len(), 6);
    }

    #[test]
    fn test_type_alias_is_erased_from_bytecode() {
        use crate::compiler::{Options, compile_program};
        let with = compile_program(
            "type A = number\nexport type B = { x: A }\nreturn 1",
            "t",
            Options::RUN,
        )
        .expect("compile failed");
        let without = compile_program("return 1", "t", Options::RUN).expect("compile failed");
        assert_eq!(with.instructions.len(), without.instructions.len());
    }
}
