//! The parser.

pub mod action_helper;
pub mod error;
pub mod expr;
pub mod readline;
pub mod stmt;
pub mod tokenizer;
pub mod types;

pub use crate::ast::*;
pub use action_helper::ActionHelper;
pub use error::ParseError;
pub use readline::ReadLineEngine;
pub use tokenizer::{DocComment, TokenizerEngine};

use crate::lexer::Token;

/// How deeply statements and expressions may nest. Every later pass (sema,
/// folding, codegen, the IR builder) recurses over the tree, so this bounds
/// their recursion too. Binary operator chains count one level per operator,
/// since `a + b + c` builds a left-deep tree even though parsing it does not
/// recurse.
pub(crate) const MAX_PARSE_DEPTH: usize = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParserSnapshot {
    pub pos: usize,
    pub depth: usize,
}

#[derive(Clone, Debug, Default)]
pub struct ParserStats {
    pub tokens_total: usize,
    pub statements_parsed: usize,
}

#[derive(Clone, Debug)]
pub struct Parser {
    pub(crate) tokens: Vec<Token>,
    pub(crate) lex_error: Option<String>,
    pub(crate) pos: usize,
    pub(crate) depth: usize,
    pub(crate) span_pool: SpanPool,
    pub(crate) statements_count: usize,
    pub(crate) case_colon_pos: Option<usize>,
    /// Set while the next statement is one of the file's own, so `export type`
    /// can tell it from one nested in a block.
    pub(crate) top_level_stmt: bool,
}

impl Parser {
    pub fn new(source: &str) -> Self {
        let mut tokenizer = TokenizerEngine::new(source);
        let (tokens, lex_error) = match tokenizer.tokenize() {
            Ok(tokens) => (tokens, None),
            Err(err) => (vec![Self::eof_token()], Some(err)),
        };
        Self {
            tokens,
            lex_error,
            pos: 0,
            depth: 0,
            span_pool: SpanPool::new(),
            statements_count: 0,
            case_colon_pos: None,
            top_level_stmt: false,
        }
    }

    pub fn with_comments(source: &str) -> (Self, Vec<DocComment>) {
        let mut tokenizer = TokenizerEngine::new(source);
        let ((tokens, comments), lex_error) = match tokenizer.tokenize_with_comments() {
            Ok(lexed) => (lexed, None),
            Err(err) => ((vec![Self::eof_token()], Vec::new()), Some(err)),
        };
        (
            Self {
                tokens,
                lex_error,
                pos: 0,
                depth: 0,
                span_pool: SpanPool::new(),
                statements_count: 0,
                case_colon_pos: None,
                top_level_stmt: false,
            },
            comments,
        )
    }

    fn eof_token() -> Token {
        Token {
            kind: "eof",
            value: String::new(),
            line: 1,
            col: 1,
        }
    }

    /// Enters one nesting level, failing once `MAX_PARSE_DEPTH` is reached.
    /// Callers restore `depth` when the nested construct is done.
    pub(crate) fn enter(&mut self) -> Result<(), String> {
        if self.depth >= MAX_PARSE_DEPTH {
            return Err(format!(
                "[line {}] parse recursion depth limit ({}) exceeded",
                self.peek().line,
                MAX_PARSE_DEPTH
            ));
        }
        self.depth += 1;
        Ok(())
    }

    pub fn snapshot(&self) -> ParserSnapshot {
        ParserSnapshot {
            pos: self.pos,
            depth: self.depth,
        }
    }

    pub fn rollback(&mut self, snapshot: ParserSnapshot) {
        self.pos = snapshot.pos;
        self.depth = snapshot.depth;
    }

    pub fn commit(&mut self, _snapshot: ParserSnapshot) {}

    pub fn synchronize(&mut self) {
        while !self.is_eof() {
            if self.check_symbol(";") {
                self.pos += 1;
                return;
            }
            if ActionHelper::is_statement_sync_token(self.peek()) {
                return;
            }
            self.pos += 1;
        }
    }

    pub fn stats(&self) -> ParserStats {
        ParserStats {
            tokens_total: self.tokens.len(),
            statements_parsed: self.statements_count,
        }
    }

    pub fn current_loc(&self) -> SourceLocation {
        let tok = &self.tokens[self.pos.min(self.tokens.len().saturating_sub(1))];
        SourceLocation::new(tok.line as u32, tok.col as u32)
    }

    pub fn record_span(&mut self, id: NodeId, start: SourceLocation) {
        let end = self.current_loc();
        self.span_pool.insert(id, Span::new(start, end));
    }

    pub fn span_pool(&self) -> &SpanPool {
        &self.span_pool
    }

    pub fn span_pool_mut(&mut self) -> &mut SpanPool {
        &mut self.span_pool
    }

    pub fn parse_program(&mut self) -> Result<(Vec<Stmt>, SpanPool), String> {
        if let Some(err) = self.lex_error.take() {
            return Err(err);
        }
        let mut program = Vec::new();
        while !self.is_eof() {
            if self.check_keyword("end")
                || self.check_keyword("else")
                || self.check_keyword("elseif")
                || self.check_keyword("until")
            {
                break;
            }

            if self.check_symbol(";") {
                self.pos += 1;
                continue;
            }

            self.top_level_stmt = true;
            program.push(self.parse_statement()?);
            self.consume_semicolon_if_any();
        }
        Ok((program, std::mem::take(&mut self.span_pool)))
    }

    pub fn parse_statement(&mut self) -> Result<Stmt, String> {
        let stmt = stmt::parse_statement(self)?;
        self.statements_count += 1;
        Ok(stmt)
    }

    pub fn parse_expr(&mut self) -> Result<Expr, String> {
        expr::parse_expr(self)
    }

    pub fn parse_type_expr(&mut self) -> Result<crate::ast::ty::TypeExpr, String> {
        types::parse_type_expr(self)
    }

    pub fn advance_token(&mut self) -> Token {
        let token = self.peek().clone();
        self.pos += 1;
        token
    }

    pub fn consume_bracket_attributes(&mut self) {
        while self.match_symbol("[") {
            while !self.is_eof() && !self.check_symbol("]") {
                if self.check_symbol("[") {
                    self.pos += 1;
                    continue;
                }
                self.pos += 1;
            }
            let _ = self.match_symbol("]");
        }
    }

    pub fn consume_semicolon_if_any(&mut self) {
        while self.match_symbol(";") {}
    }

    pub fn match_symbol(&mut self, value: &str) -> bool {
        if self.peek().kind == "symbol" && self.peek().value == value {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub fn expect_symbol(&mut self, value: &str) -> Result<(), String> {
        if !self.match_symbol(value) {
            Err(format!(
                "[line {}] expected symbol {:?}",
                self.peek().line,
                value
            ))
        } else {
            Ok(())
        }
    }

    pub fn expect_keyword(&mut self, value: &str) -> Result<(), String> {
        if !self.match_keyword(value) {
            Err(format!(
                "[line {}] expected keyword {:?}",
                self.peek().line,
                value
            ))
        } else {
            Ok(())
        }
    }

    pub fn match_keyword(&mut self, value: &str) -> bool {
        if self.peek().kind == "keyword" && self.peek().value == value {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub fn check_keyword(&self, value: &str) -> bool {
        self.peek().kind == "keyword" && self.peek().value == value
    }

    pub fn check_word(&self, value: &str) -> bool {
        (self.peek().kind == "keyword" || self.peek().kind == "name") && self.peek().value == value
    }

    pub fn match_word(&mut self, value: &str) -> bool {
        if self.check_word(value) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub fn expect_word(&mut self, value: &str) -> Result<(), String> {
        if !self.match_word(value) {
            Err(format!(
                "[line {}] expected '{}', got {:?}",
                self.peek().line,
                value,
                self.peek().value
            ))
        } else {
            Ok(())
        }
    }

    pub fn expect_name(&mut self) -> Result<String, String> {
        let token = self.peek().clone();
        if token.kind == "name" || token.kind == "keyword" {
            self.pos += 1;
            Ok(token.value)
        } else {
            Err(format!(
                "[line {}] expected name, got {:?}",
                token.line, token.value
            ))
        }
    }

    pub fn check_symbol(&self, value: &str) -> bool {
        self.peek().kind == "symbol" && self.peek().value == value
    }

    pub fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    pub fn is_eof(&self) -> bool {
        self.peek().kind == "eof"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_assignment_and_call() {
        let mut parser = Parser::new("local msg = \"hello\"\nprint(msg)");
        let (program, _pool) = parser.parse_program().expect("failed to parse");

        assert_eq!(
            program,
            vec![
                Stmt::Local {
                    id: NodeId::next(),
                    name: "msg".to_string(),
                    is_const: false,
                    is_global: false,
                    type_name: None,
                    initializer: Some(Expr::string("hello")),
                },
                Stmt::Expr {
                    id: NodeId::next(),
                    expr: Expr::Call {
                        id: NodeId::next(),
                        callee: Box::new(Expr::Variable {
                            id: NodeId::next(),
                            name: "print".to_string(),
                        }),
                        args: vec![Expr::Variable {
                            id: NodeId::next(),
                            name: "msg".to_string(),
                        }],
                    },
                },
            ]
        );
    }

    #[test]
    fn parses_typed_local_and_function_bindings() {
        let mut parser = Parser::new(
            "local small: int = 2\nconst function divide(a: int, b: int): int\n    return a // b\nend",
        );
        let (program, _pool) = parser.parse_program().expect("failed to parse");

        assert_eq!(
            program[0],
            Stmt::Local {
                id: NodeId::next(),
                name: "small".to_string(),
                is_const: false,
                is_global: false,
                type_name: Some("int".to_string()),
                initializer: Some(Expr::int(2)),
            }
        );

        assert!(matches!(program[1], Stmt::Function { .. }));
    }

    #[test]
    fn parses_dotted_const_binding_as_assignment() {
        let mut parser = Parser::new("const math.e = 1");
        let (program, _pool) = parser.parse_program().expect("failed to parse");

        assert_eq!(
            program,
            vec![Stmt::Assign {
                id: NodeId::next(),
                target: AssignTarget::Member {
                    object: Box::new(Expr::Variable {
                        id: NodeId::next(),
                        name: "math".to_string(),
                    }),
                    field: "e".to_string(),
                },
                value: Expr::int(1),
                is_const: true,
            }]
        );
    }

    #[test]
    fn parses_immediately_invoked_function_expression() {
        let mut parser = Parser::new("local value = (function() return 1 end)()");
        let (program, _pool) = parser.parse_program().expect("failed to parse");

        assert!(matches!(
            program.first(),
            Some(Stmt::Local {
                initializer: Some(Expr::Call { .. }),
                ..
            })
        ));
    }

    #[test]
    fn parses_compound_and_multiple_assignment() {
        let mut parser = Parser::new("n += 2\na, b = b, a");
        let (program, _pool) = parser.parse_program().expect("failed to parse");

        assert_eq!(
            program[0],
            Stmt::Assign {
                id: NodeId::next(),
                target: AssignTarget::Variable("n".to_string()),
                value: Expr::Binary {
                    id: NodeId::next(),
                    left: Box::new(Expr::Variable {
                        id: NodeId::next(),
                        name: "n".to_string(),
                    }),
                    op: BinOp::Add,
                    right: Box::new(Expr::int(2)),
                },
                is_const: false,
            }
        );
        assert_eq!(
            program[1],
            Stmt::AssignMany {
                id: NodeId::next(),
                targets: vec![
                    AssignTarget::Variable("a".to_string()),
                    AssignTarget::Variable("b".to_string()),
                ],
                values: vec![
                    Expr::Variable {
                        id: NodeId::next(),
                        name: "b".to_string(),
                    },
                    Expr::Variable {
                        id: NodeId::next(),
                        name: "a".to_string(),
                    },
                ],
            }
        );
    }

    #[test]
    fn parses_dotted_typed_function_bindings() {
        let mut parser = Parser::new(
            "function math.random(min: number, max: number): number\n    return 1\nend",
        );
        let (program, _pool) = parser.parse_program().expect("failed to parse");

        assert_eq!(
            program[0],
            Stmt::Function {
                id: NodeId::next(),
                name: Some("math.random".to_string()),
                is_const: false,
                params: vec![
                    Param {
                        name: "min".to_string(),
                        type_name: Some("number".to_string()),
                        variadic: false,
                    },
                    Param {
                        name: "max".to_string(),
                        type_name: Some("number".to_string()),
                        variadic: false,
                    },
                ],
                return_type: Some("number".to_string()),
                body: vec![Stmt::Return {
                    id: NodeId::next(),
                    values: vec![Expr::int(1)],
                }],
            }
        );
    }

    #[test]
    fn parses_typed_variadic_parameter_and_increment() {
        let mut parser = Parser::new(
            "function collect(first: number, ...: number): number\n    local total = first\n    total++\n    total--\n    return total\nend",
        );
        let (program, _pool) = parser.parse_program().expect("failed to parse");

        let Stmt::Function { params, body, .. } = &program[0] else {
            panic!("expected function");
        };
        assert_eq!(
            params[1],
            Param {
                name: "...".to_string(),
                type_name: Some("number".to_string()),
                variadic: true,
            }
        );
        assert!(matches!(body[1], Stmt::Increment { amount: 1, .. }));
        assert!(matches!(body[2], Stmt::Increment { amount: -1, .. }));
    }

    #[test]
    fn parses_numeric_for_loop() {
        let mut parser = Parser::new("for i = 1, 10, 2 do\n    print(i)\nend");
        let (program, _pool) = parser.parse_program().expect("failed to parse");

        assert!(matches!(
            &program[0],
            Stmt::NumericFor {
                data,
                ..
            } if data.var == "i" && data.step.is_some() && data.body.len() == 1
        ));
    }

    #[test]
    fn parses_anonymous_function_expression() {
        let mut parser = Parser::new("table.sort(numbers, function(a, b) return a < b end)");
        let (program, _pool) = parser.parse_program().expect("failed to parse");

        assert!(matches!(
            &program[0],
            Stmt::Expr {
                expr: Expr::Call { args, .. },
                ..
            } if matches!(args.get(1), Some(Expr::Function { params, body, .. })
                if params.len() == 2 && body.len() == 1)
        ));
    }

    #[test]
    fn test_parser_returns_err_on_depth_limit() {
        // Deeply nested parentheses past the limit should return Err, not panic
        let mut expr = "1".to_string();
        for _ in 0..super::MAX_PARSE_DEPTH + 10 {
            expr = format!("({})", expr);
        }
        let mut parser = Parser::new(&expr);
        let res = parser.parse_program();
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("parse recursion depth limit"));
    }

    #[test]
    fn test_parser_limits_nested_blocks_and_operator_chains() {
        let depth = super::MAX_PARSE_DEPTH + 10;
        let blocks = format!(
            "{}print(1){}",
            "if true then\n".repeat(depth),
            "\nend".repeat(depth)
        );
        let chain = format!("local x = 1{}", " + 1".repeat(depth));
        for source in [blocks, chain] {
            let err = Parser::new(&source).parse_program().unwrap_err();
            assert!(err.contains("parse recursion depth limit"), "{}", err);
        }
        // Chains well inside the limit still parse.
        let fine = format!("local x = 1{}", " + 1".repeat(100));
        assert!(Parser::new(&fine).parse_program().is_ok());
    }

    #[test]
    fn test_parser_reports_lexer_errors() {
        let err = Parser::new("print(\"abc").parse_program().unwrap_err();
        assert!(err.contains("unfinished string literal"), "{}", err);
    }

    #[test]
    fn test_parser_returns_err_on_syntax_errors() {
        let mut p1 = Parser::new("local x =");
        assert!(p1.parse_program().is_err());

        let mut p2 = Parser::new("if x then");
        assert!(p2.parse_program().is_err());

        let mut p3 = Parser::new("for i in do end");
        assert!(p3.parse_program().is_err());
    }

    #[test]
    fn parse_program_produces_real_spans() {
        let mut parser = Parser::new("local x = 42");
        let (program, pool) = parser.parse_program().expect("parse ok");
        let span = pool.get_or_dummy(program[0].node_id());
        assert_eq!(span.start.line, 1);
        assert_eq!(span.start.column, 1);
    }

    #[test]
    fn test_parse_error_diagnostic() {
        let err = ParseError::with_hint(10, 5, "unexpected identifier", "consider adding a comma");
        let diag = err.format_diagnostic(Some("local a b = 1, 2"));
        assert!(diag.contains("[line 10:5] unexpected identifier"));
        assert!(diag.contains("consider adding a comma"));
    }
}
