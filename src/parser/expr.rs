//! Expression parsing.

use super::Parser;
use super::action_helper::ActionHelper;
use super::stmt::{parse_block_until, parse_param_list};
use super::types::read_type_annotation;
use crate::ast::*;

pub fn parse_expr(parser: &mut Parser) -> Result<Expr, String> {
    parse_precedence(parser, 0)
}

pub fn parse_precedence(parser: &mut Parser, min_prec: u8) -> Result<Expr, String> {
    let saved_depth = parser.depth;
    let res = parse_precedence_chain(parser, min_prec);
    parser.depth = saved_depth;
    res
}

fn parse_precedence_chain(parser: &mut Parser, min_prec: u8) -> Result<Expr, String> {
    let mut left = parse_prefix(parser)?;

    loop {
        if parser.is_eof() {
            break;
        }

        let Some((op, prec)) = peek_binary_op(parser) else {
            break;
        };
        if prec < min_prec {
            break;
        }
        parser.pos += 1;
        // Each operator adds a level to the left-deep tree; the depth is
        // restored once the whole chain is parsed.
        parser.enter()?;
        let next_min = prec + 1;
        let right = parse_precedence(parser, next_min)?;
        let bin_op = BinOp::parse_str(&op).ok_or_else(|| {
            format!(
                "[line {}] unknown binary operator '{}'",
                parser.peek().line,
                op
            )
        })?;
        let start = parser
            .span_pool
            .get(left.node_id())
            .map(|s| s.start)
            .unwrap_or_else(|| parser.current_loc());
        let bin_expr = Expr::Binary {
            id: NodeId::next(),
            left: Box::new(left),
            op: bin_op,
            right: Box::new(right),
        };
        parser.record_span(bin_expr.node_id(), start);
        left = bin_expr;
    }

    Ok(left)
}

pub fn parse_prefix(parser: &mut Parser) -> Result<Expr, String> {
    parser.enter()?;
    let res = parse_prefix_inner(parser);
    parser.depth -= 1;
    res
}

pub fn parse_prefix_inner(parser: &mut Parser) -> Result<Expr, String> {
    let start = parser.current_loc();
    if parser.check_keyword("function") {
        if !parser
            .tokens
            .get(parser.pos + 1)
            .is_some_and(|t| t.kind == "symbol" && t.value == "(")
        {
            let next_val = parser
                .tokens
                .get(parser.pos + 1)
                .map(|t| t.value.as_str())
                .unwrap_or("eof");
            return Err(format!(
                "[line {}] expected symbol \"(\" after 'function' (unexpected {:?})",
                parser.peek().line,
                next_val
            ));
        }
        parser.expect_keyword("function")?;
        parser.expect_symbol("(")?;
        let params = parse_param_list(parser)?;
        if parser.match_symbol(":") {
            read_type_annotation(parser);
        }
        let body = parse_block_until(parser, "end")?;
        parser.expect_keyword("end")?;
        let expr = Expr::Function {
            id: NodeId::next(),
            params,
            body,
        };
        parser.record_span(expr.node_id(), start);
        return Ok(expr);
    }

    if parser.check_symbol("(") {
        parser.expect_symbol("(")?;
        let inner = parse_expr(parser)?;
        parser.expect_symbol(")")?;
        // Parentheses cut a call or `...` down to its first value.
        let expr = if matches!(
            inner,
            Expr::Call { .. } | Expr::MethodCall { .. } | Expr::Vararg { .. }
        ) {
            let expr = Expr::Paren {
                id: NodeId::next(),
                expr: Box::new(inner),
            };
            parser.record_span(expr.node_id(), start);
            expr
        } else {
            inner
        };
        return parse_postfix(parser, expr);
    }

    if parser.check_symbol("{") {
        return parse_table(parser);
    }

    let tok = parser.peek();
    let maybe_unary = if matches!(tok.kind, "symbol" | "keyword") {
        ActionHelper::parse_unary_op(&tok.value)
    } else {
        None
    };
    if let Some(op) = maybe_unary {
        parser.pos += 1;
        let inner = parse_prefix(parser)?;
        let expr = Expr::Unary {
            id: NodeId::next(),
            op,
            expr: Box::new(inner),
        };
        parser.record_span(expr.node_id(), start);
        return Ok(expr);
    }

    if parser.match_symbol("...") {
        let expr = Expr::Vararg { id: NodeId::next() };
        parser.record_span(expr.node_id(), start);
        return Ok(expr);
    }

    let token = parser.peek().clone();
    match token.kind {
        "name" | "keyword" => {
            parser.pos += 1;
            let expr = match token.value.as_str() {
                "true" => Expr::Literal {
                    id: NodeId::next(),
                    value: Literal::Bool(true),
                },
                "false" => Expr::Literal {
                    id: NodeId::next(),
                    value: Literal::Bool(false),
                },
                "nil" => Expr::Literal {
                    id: NodeId::next(),
                    value: Literal::Nil,
                },
                _ => Expr::Variable {
                    id: NodeId::next(),
                    name: token.value,
                },
            };
            parser.record_span(expr.node_id(), start);
            parse_postfix(parser, expr)
        }
        "string" => {
            parser.pos += 1;
            let expr = Expr::Literal {
                id: NodeId::next(),
                value: Literal::String(token.value),
            };
            parser.record_span(expr.node_id(), start);
            Ok(expr)
        }
        "interp" => {
            parser.pos += 1;
            let parts = parse_interp_parts(&token.value)?;
            let expr = Expr::Interp {
                id: NodeId::next(),
                parts,
            };
            parser.record_span(expr.node_id(), start);
            Ok(expr)
        }
        "number" => {
            parser.pos += 1;
            let lit = Literal::parse_number(&token.value).ok_or_else(|| {
                format!(
                    "[line {}] invalid number literal '{}'",
                    token.line, token.value
                )
            })?;
            let expr = Expr::Literal {
                id: NodeId::next(),
                value: lit,
            };
            parser.record_span(expr.node_id(), start);
            Ok(expr)
        }
        _ => Err(format!(
            "[line {}] unexpected token in expression: {:?}",
            token.line, token.value
        )),
    }
}

pub fn parse_postfix(parser: &mut Parser, mut expr: Expr) -> Result<Expr, String> {
    loop {
        let start = parser
            .span_pool
            .get(expr.node_id())
            .map(|s| s.start)
            .unwrap_or_else(|| parser.current_loc());
        if parser.match_symbol(".") {
            let field = parser.expect_name()?;
            let node = Expr::Member {
                id: NodeId::next(),
                object: Box::new(expr),
                field,
            };
            parser.record_span(node.node_id(), start);
            expr = node;
        } else if parser.match_symbol("(") {
            let args = parse_call_args(parser)?;
            let node = Expr::Call {
                id: NodeId::next(),
                callee: Box::new(expr),
                args,
            };
            parser.record_span(node.node_id(), start);
            expr = node;
        } else if check_method_call(parser) {
            parser.pos += 1;
            let method = parser.expect_name()?;
            parser.expect_symbol("(")?;
            let args = parse_call_args(parser)?;
            let node = Expr::MethodCall {
                id: NodeId::next(),
                object: Box::new(expr),
                method,
                args,
            };
            parser.record_span(node.node_id(), start);
            expr = node;
        } else if parser.match_symbol("[") {
            if parser.peek().kind == "name" && parser.peek().value == "cite" {
                while !parser.is_eof() && !parser.check_symbol("]") {
                    parser.pos += 1;
                }
                parser.expect_symbol("]")?;
                continue;
            }
            let field = parse_expr(parser)?;
            parser.expect_symbol("]")?;
            let node = Expr::Index {
                id: NodeId::next(),
                object: Box::new(expr),
                index: Box::new(field),
            };
            parser.record_span(node.node_id(), start);
            expr = node;
        } else {
            break;
        }
    }
    Ok(expr)
}

pub fn parse_table(parser: &mut Parser) -> Result<Expr, String> {
    let start = parser.current_loc();
    parser.expect_symbol("{")?;
    let mut entries = Vec::new();
    if !parser.check_symbol("}") {
        loop {
            if (parser.peek().kind == "name" || parser.peek().kind == "keyword")
                && parser
                    .tokens
                    .get(parser.pos + 1)
                    .is_some_and(|next| next.kind == "symbol" && next.value == "=")
            {
                let key = parser.expect_name()?;
                parser.expect_symbol("=")?;
                let value = parse_expr(parser)?;
                entries.push(TableEntry {
                    key: Some(key),
                    computed: None,
                    value,
                });
            } else if parser.check_symbol("[")
                && parser
                    .tokens
                    .get(parser.pos + 1)
                    .is_some_and(|next| next.kind == "string")
                && parser
                    .tokens
                    .get(parser.pos + 2)
                    .is_some_and(|next| next.kind == "symbol" && next.value == "]")
            {
                parser.expect_symbol("[")?;
                let key = parser.advance_token().value;
                parser.expect_symbol("]")?;
                parser.expect_symbol("=")?;
                let value = parse_expr(parser)?;
                entries.push(TableEntry {
                    key: Some(key),
                    computed: None,
                    value,
                });
            } else if parser.match_symbol("[") {
                let key = parse_expr(parser)?;
                parser.expect_symbol("]")?;
                parser.expect_symbol("=")?;
                let value = parse_expr(parser)?;
                entries.push(TableEntry {
                    key: None,
                    computed: Some(Box::new(key)),
                    value,
                });
            } else {
                let value = parse_expr(parser)?;
                entries.push(TableEntry::positional(value));
            }

            // Entries are separated by `,` or `;`, and one may trail.
            if !(parser.match_symbol(",") || parser.match_symbol(";")) || parser.check_symbol("}") {
                break;
            }
        }
    }
    parser.expect_symbol("}")?;
    let expr = Expr::Table {
        id: NodeId::next(),
        entries,
    };
    parser.record_span(expr.node_id(), start);
    Ok(expr)
}

pub fn parse_call_args(parser: &mut Parser) -> Result<Vec<Expr>, String> {
    let mut args = Vec::new();
    if !parser.check_symbol(")") {
        loop {
            args.push(parse_expr(parser)?);
            if !parser.match_symbol(",") {
                break;
            }
        }
    }
    parser.expect_symbol(")")?;
    Ok(args)
}

pub fn peek_binary_op(parser: &Parser) -> Option<(String, u8)> {
    if parser.peek().kind != "symbol" && parser.peek().kind != "keyword" {
        return None;
    }

    let value = parser.peek().value.clone();
    let prec = ActionHelper::binary_precedence(&value)?;
    Some((value, prec))
}

pub fn check_method_call(parser: &Parser) -> bool {
    if parser.case_colon_pos == Some(parser.pos) {
        return false;
    }
    parser.check_symbol(":")
        && parser
            .tokens
            .get(parser.pos + 1)
            .is_some_and(|t| t.kind == "name")
        && parser
            .tokens
            .get(parser.pos + 2)
            .is_some_and(|t| t.kind == "symbol" && t.value == "(")
}

pub fn parse_interp_parts(value: &str) -> Result<Vec<InterpPart>, String> {
    // The tokenizer encodes the parts as `L<len>:<text>` and `E<len>:<source>`.
    let malformed = || "syntax error: malformed interpolated string".to_string();
    let mut parts = Vec::new();
    let mut rest = value;
    while !rest.is_empty() {
        let tag = rest.as_bytes()[0];
        let colon = rest.find(':').ok_or_else(malformed)?;
        let len: usize = rest[1..colon].parse().map_err(|_| malformed())?;
        let text = rest.get(colon + 1..colon + 1 + len).ok_or_else(malformed)?;
        rest = &rest[colon + 1 + len..];
        if tag == b'L' {
            parts.push(InterpPart::Literal(text.to_string()));
            continue;
        }
        let expression = text;
        if expression.trim().is_empty() {
            return Err("syntax error: empty expression in interpolated string".to_string());
        }
        let mut inner_parser = Parser::new(expression);
        let (statements, _pool) = inner_parser.parse_program()?;
        if statements.len() != 1 {
            return Err(format!(
                "syntax error: string interpolation must contain exactly one expression: {:?}",
                expression
            ));
        }
        match statements
            .into_iter()
            .next()
            .expect("checked above: exactly one statement")
        {
            Stmt::Expr { expr, .. } => {
                parts.push(InterpPart::Expr(expr));
            }
            _ => {
                return Err(format!(
                    "syntax error: string interpolation must contain an expression: {:?}",
                    expression
                ));
            }
        }
    }
    Ok(parts)
}
