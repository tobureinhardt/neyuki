//! Type annotation parsing.

use super::Parser;
use crate::ast::ty::TypeExpr;

pub struct TypeParserEngine;

impl TypeParserEngine {
    /// Reads raw string type annotation tokens until the next statement delimiter.
    pub fn read_type_annotation(parser: &mut Parser) -> String {
        join_type_tokens(&Self::read_type_tokens(parser, false))
    }

    /// Like `read_type_annotation`, for the right side of a `type` alias:
    /// the type may continue on the next line after an operator or before
    /// one, and the text is laid out as it is usually written.
    pub fn read_alias_type(parser: &mut Parser) -> String {
        let mut parts = Self::read_type_tokens(parser, true);
        // `type T =` may put a `|` before the first variant.
        if parts.first().is_some_and(|p| p == "|") {
            parts.remove(0);
        }
        pretty_type_tokens(&parts)
    }

    fn read_type_tokens(parser: &mut Parser, continue_lines: bool) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        let mut braces = 0usize;
        let mut brackets = 0usize;
        let mut parens = 0usize;
        let mut last_line = None;
        while !parser.is_eof() {
            let token = parser.peek().clone();
            if token.kind == "eof" {
                break;
            }
            if braces == 0
                && brackets == 0
                && parens == 0
                && last_line.is_some_and(|line| line != token.line)
            {
                let joins = continue_lines
                    && (parts
                        .last()
                        .is_some_and(|p| matches!(p.as_str(), "|" | "->"))
                        || (token.kind == "symbol" && matches!(token.value.as_str(), "|" | "->")));
                if !joins {
                    break;
                }
            }
            // Types never put two names side by side, so a name right after a
            // complete type starts the body: `function f(): () x = 1 end`.
            if braces == 0
                && brackets == 0
                && parens == 0
                && token.kind == "name"
                && parts.last().is_some_and(|prev: &String| {
                    prev == ")"
                        || prev == "}"
                        || prev == "]"
                        || prev == "?"
                        || prev.starts_with(|c: char| c.is_alphanumeric() || c == '_')
                })
            {
                break;
            }
            last_line = Some(token.line);
            if token.kind == "symbol" {
                if braces == 0
                    && brackets == 0
                    && parens == 0
                    && matches!(token.value.as_str(), "=" | "," | ")" | ";")
                {
                    break;
                }
                match token.value.as_str() {
                    "{" => braces += 1,
                    "}" if braces > 0 => braces -= 1,
                    "[" => brackets += 1,
                    "]" if brackets > 0 => brackets -= 1,
                    "(" => parens += 1,
                    ")" if parens > 0 => parens -= 1,
                    _ => {}
                }
            }
            if token.kind == "keyword"
                && braces == 0
                && brackets == 0
                && parens == 0
                && matches!(
                    token.value.as_str(),
                    "end"
                        | "do"
                        | "then"
                        | "else"
                        | "elseif"
                        | "until"
                        | "return"
                        | "local"
                        | "const"
                        | "global"
                        | "if"
                        | "for"
                        | "while"
                        | "repeat"
                )
            {
                break;
            }
            parts.push(parser.advance_token().value);
        }
        parts
    }

    /// Parses a structured `TypeExpr` from the current token stream.
    pub fn parse_type_expr(parser: &mut Parser) -> Result<TypeExpr, String> {
        let raw = Self::read_type_annotation(parser);
        if raw.trim().is_empty() {
            return Ok(TypeExpr::Any);
        }
        Ok(TypeExpr::parse(&raw))
    }
}

/// Joins type tokens with spaces, keeping `m.Foo` together.
fn join_type_tokens(parts: &[String]) -> String {
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        let glued = part == "." || (i > 0 && parts[i - 1] == ".");
        if i > 0 && !glued {
            out.push(' ');
        }
        out.push_str(part);
    }
    out
}

/// Lays type tokens out the way a type is written: `{ x: number, y: number }`,
/// `(a: number) -> string?`.
fn pretty_type_tokens(parts: &[String]) -> String {
    let mut out = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            let prev = parts[i - 1].as_str();
            let tight = matches!(part.as_str(), "," | ":" | ")" | "]" | "?" | ".")
                || matches!(prev, "(" | "[" | "." | "...")
                || (prev == "{" && part == "}");
            if !tight {
                out.push(' ');
            }
        }
        out.push_str(part);
    }
    out
}

pub fn read_type_annotation(parser: &mut Parser) -> String {
    TypeParserEngine::read_type_annotation(parser)
}

pub fn read_alias_type(parser: &mut Parser) -> String {
    TypeParserEngine::read_alias_type(parser)
}

pub fn parse_type_expr(parser: &mut Parser) -> Result<TypeExpr, String> {
    TypeParserEngine::parse_type_expr(parser)
}
