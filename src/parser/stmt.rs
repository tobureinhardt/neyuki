//! Statement parsing.

use super::Parser;
use super::action_helper::ActionHelper;
use super::expr::parse_expr;
use super::types::{read_alias_type, read_type_annotation};
use crate::ast::*;

pub fn parse_statement(parser: &mut Parser) -> Result<Stmt, String> {
    let start = parser.current_loc();
    parser.enter()?;
    let stmt = parse_statement_inner(parser);
    parser.depth -= 1;
    let stmt = stmt?;
    parser.record_span(stmt.node_id(), start);
    Ok(stmt)
}

fn is_enum_statement(parser: &Parser) -> bool {
    if !parser.check_word("enum") {
        return false;
    }
    let Some(t1) = parser.tokens.get(parser.pos + 1) else {
        return false;
    };
    if t1.kind != "name" {
        return false;
    }
    if let Some(t2) = parser.tokens.get(parser.pos + 2) {
        if t2.kind == "symbol"
            && (t2.value == "="
                || t2.value == ","
                || t2.value == "("
                || t2.value == "."
                || t2.value == "["
                || t2.value == ":"
                || t2.value == "++"
                || t2.value == "--")
        {
            return false;
        }
        if t2.kind == "symbol"
            && (ActionHelper::compound_assign_binop(&t2.value).is_some() || t2.value == "??=")
        {
            return false;
        }
    }
    true
}

fn is_match_statement(parser: &Parser) -> bool {
    if !parser.check_word("match") {
        return false;
    }
    let Some(t1) = parser.tokens.get(parser.pos + 1) else {
        return false;
    };
    if t1.kind == "symbol" {
        if matches!(t1.value.as_str(), "=" | "," | "." | "[" | ":" | "++" | "--") {
            return false;
        }
        if ActionHelper::compound_assign_binop(&t1.value).is_some() || t1.value == "??=" {
            return false;
        }
        if t1.value == "(" {
            let mut depth = 1;
            let mut i = parser.pos + 2;
            while i < parser.tokens.len() && depth > 0 {
                let tok = &parser.tokens[i];
                if tok.kind == "symbol" {
                    if tok.value == "(" {
                        depth += 1;
                    } else if tok.value == ")" {
                        depth -= 1;
                    }
                }
                i += 1;
            }
            if depth == 0 && i < parser.tokens.len() {
                let after = &parser.tokens[i];
                return (after.kind == "name" || after.kind == "keyword")
                    && matches!(after.value.as_str(), "case" | "default" | "end");
            }
            return false;
        }
    }
    true
}

/// Where a `type Name =` or `export type Name =` declaration starts: whether
/// it is exported and how many tokens come before `type`. `type` and `export`
/// stay ordinary names everywhere else (`type(x)`, `export = 1`).
fn type_decl_start(parser: &Parser) -> Option<(bool, usize)> {
    let tok = |i: usize| parser.tokens.get(parser.pos + i);
    let is_word = |t: Option<&crate::lexer::Token>, w: &str| {
        t.is_some_and(|t| (t.kind == "keyword" || t.kind == "name") && t.value == w)
    };
    let (exported, at) = if is_word(tok(0), "export") && is_word(tok(1), "type") {
        (true, 1)
    } else if is_word(tok(0), "type") {
        (false, 0)
    } else {
        return None;
    };
    let named = tok(at + 1).is_some_and(|t| t.kind == "name");
    let opens =
        tok(at + 2).is_some_and(|t| t.kind == "symbol" && matches!(t.value.as_str(), "=" | "<"));
    (named && opens).then_some((exported, at))
}

fn parse_type_alias(parser: &mut Parser, exported: bool, at: usize) -> Result<Stmt, String> {
    let line = parser.peek().line;
    if exported && !parser.top_level_stmt {
        return Err(format!(
            "[line {}] 'export type' is only allowed at the top level of a file",
            line
        ));
    }
    parser.pos += at + 1;
    let name_tok = parser.advance_token();
    let name_span = Span::new(
        SourceLocation::new(name_tok.line as u32, name_tok.col as u32),
        SourceLocation::new(
            name_tok.line as u32,
            (name_tok.col + name_tok.value.chars().count()) as u32,
        ),
    );
    if parser.check_symbol("<") {
        return Err(format!(
            "[line {}] generic type aliases are not supported: '{}' cannot take type parameters",
            name_tok.line, name_tok.value
        ));
    }
    parser.expect_symbol("=")?;
    let ty = read_alias_type(parser);
    if ty.is_empty() {
        return Err(format!(
            "[line {}] expected a type after '=' in the declaration of '{}'",
            name_tok.line, name_tok.value
        ));
    }
    Ok(Stmt::TypeAlias {
        name: name_tok.value,
        exported,
        ty,
        name_span,
        id: NodeId::next(),
    })
}

pub fn parse_statement_inner(parser: &mut Parser) -> Result<Stmt, String> {
    // Only the statement the program loop just started is top level.
    let top_level = std::mem::take(&mut parser.top_level_stmt);
    if let Some((exported, at)) = type_decl_start(parser) {
        parser.top_level_stmt = top_level;
        return parse_type_alias(parser, exported, at);
    }
    if is_enum_statement(parser) {
        return parse_enum(parser);
    }
    if is_match_statement(parser) {
        return parse_match(parser);
    }
    if parser.check_keyword("do") {
        return parse_do(parser);
    }
    if parser.check_keyword("local")
        || parser.check_keyword("const")
        || parser.check_keyword("global")
    {
        return parse_binding(parser);
    }
    if parser.check_keyword("function") {
        return parse_function(parser, false);
    }
    if parser.check_keyword("if") {
        return parse_if(parser);
    }
    if parser.check_keyword("for") {
        return parse_for(parser);
    }
    if parser.check_keyword("while") {
        return parse_while(parser);
    }
    if parser.check_keyword("repeat") {
        return parse_repeat(parser);
    }
    if parser.check_keyword("return") {
        return parse_return(parser);
    }
    if parser.check_keyword("break") {
        parser.pos += 1;
        return Ok(Stmt::Break { id: NodeId::next() });
    }
    if parser.check_keyword("continue") {
        parser.pos += 1;
        return Ok(Stmt::Continue { id: NodeId::next() });
    }

    parse_expr_or_assign(parser)
}

#[inline(never)]
fn parse_return(parser: &mut Parser) -> Result<Stmt, String> {
    parser.pos += 1;
    let is_case_clause = parser.check_word("case");
    let is_default_clause = parser.check_word("default")
        && parser
            .tokens
            .get(parser.pos + 1)
            .is_some_and(|t| t.value == ":");

    let expr = if !parser.is_eof()
        && !parser.check_keyword("end")
        && !parser.check_keyword("else")
        && !parser.check_keyword("elseif")
        && !parser.check_keyword("until")
        && !is_case_clause
        && !is_default_clause
    {
        let mut values = vec![parse_expr(parser)?];
        while parser.match_symbol(",") {
            values.push(parse_expr(parser)?);
        }
        values
    } else {
        Vec::new()
    };
    Ok(Stmt::Return {
        id: NodeId::next(),
        values: expr,
    })
}

#[inline(never)]
fn parse_expr_or_assign(parser: &mut Parser) -> Result<Stmt, String> {
    let expr = parse_expr(parser)?;
    if parser.match_symbol("++") {
        let target = AssignTarget::from_expr(expr)?;
        return Ok(Stmt::Increment {
            id: NodeId::next(),
            target,
            amount: 1,
        });
    }
    if parser.match_symbol("--") {
        let target = AssignTarget::from_expr(expr)?;
        return Ok(Stmt::Increment {
            id: NodeId::next(),
            target,
            amount: -1,
        });
    }
    if parser.match_symbol("=") {
        let target = AssignTarget::from_expr(expr)?;
        let value = parse_expr(parser)?;
        return Ok(Stmt::Assign {
            id: NodeId::next(),
            target,
            value,
            is_const: false,
        });
    }
    if parser.check_symbol(",") {
        let mut targets = vec![AssignTarget::from_expr(expr)?];
        while parser.match_symbol(",") {
            let next_expr = parse_expr(parser)?;
            targets.push(AssignTarget::from_expr(next_expr)?);
        }
        parser.expect_symbol("=")?;
        let mut values = vec![parse_expr(parser)?];
        while parser.match_symbol(",") {
            values.push(parse_expr(parser)?);
        }
        return Ok(Stmt::AssignMany {
            id: NodeId::next(),
            targets,
            values,
        });
    }
    if let Some(op) = match_compound_assignment(parser) {
        let target = AssignTarget::from_expr_ref(&expr)?;
        let right = parse_expr(parser)?;
        let bin_op = BinOp::parse_str(&op).ok_or_else(|| {
            format!(
                "[line {}] unknown compound operator '{}'",
                parser.peek().line,
                op
            )
        })?;
        let bin_start = parser
            .span_pool
            .get(expr.node_id())
            .map(|s| s.start)
            .unwrap_or_else(|| parser.current_loc());
        let bin_expr = Expr::Binary {
            id: NodeId::next(),
            left: Box::new(expr),
            op: bin_op,
            right: Box::new(right),
        };
        parser.record_span(bin_expr.node_id(), bin_start);
        return Ok(Stmt::Assign {
            id: NodeId::next(),
            target,
            value: bin_expr,
            is_const: false,
        });
    }
    parser.consume_bracket_attributes();
    Ok(Stmt::Expr {
        id: NodeId::next(),
        expr,
    })
}

pub fn parse_binding(parser: &mut Parser) -> Result<Stmt, String> {
    let mut is_const = false;
    let mut is_global = false;
    while parser.check_keyword("local")
        || parser.check_keyword("const")
        || parser.check_keyword("global")
    {
        is_const |= parser.check_keyword("const");
        is_global |= parser.check_keyword("global");
        parser.pos += 1;
    }

    if parser.check_keyword("function") {
        return parse_function(parser, is_const);
    }

    let mut name = parser.expect_name()?;
    while parser.match_symbol(".") {
        name.push('.');
        name.push_str(&parser.expect_name()?);
    }
    if name.contains('.') {
        parser.expect_symbol("=")?;
        let mut target = Expr::Variable {
            id: NodeId::next(),
            name: name
                .split('.')
                .next()
                .expect("split yields at least one piece")
                .to_string(),
        };
        for field in name.split('.').skip(1) {
            target = Expr::Member {
                id: NodeId::next(),
                object: Box::new(target),
                field: field.to_string(),
            };
        }
        let assign_target = AssignTarget::from_expr(target)?;
        return Ok(Stmt::Assign {
            id: NodeId::next(),
            target: assign_target,
            value: parse_expr(parser)?,
            is_const,
        });
    }
    if parser.check_symbol(",") {
        let mut names = vec![name];
        while parser.match_symbol(",") {
            names.push(parser.expect_name()?);
        }
        let mut initializers = Vec::new();
        if parser.match_symbol("=") {
            initializers.push(parse_expr(parser)?);
            while parser.match_symbol(",") {
                initializers.push(parse_expr(parser)?);
            }
        }
        return Ok(Stmt::LocalMany {
            id: NodeId::next(),
            names,
            is_const,
            is_global,
            initializers,
        });
    }
    let type_name = if parser.match_symbol(":") {
        Some(read_type_annotation(parser))
    } else {
        None
    };

    let initializer = if parser.match_symbol("=") {
        Some(parse_expr(parser)?)
    } else {
        None
    };

    Ok(Stmt::Local {
        id: NodeId::next(),
        name,
        is_const,
        is_global,
        type_name,
        initializer,
    })
}

pub fn parse_function(parser: &mut Parser, is_const: bool) -> Result<Stmt, String> {
    parser.expect_keyword("function")?;

    let name = if parser.peek().kind == "name" {
        let mut name = parser.expect_name()?;
        while parser.match_symbol(".") {
            name.push('.');
            name.push_str(&parser.expect_name()?);
        }
        Some(name)
    } else {
        None
    };

    // `function T:m(...)` is `function T.m(self, ...)`. A `:` here is a
    // method only when a name and `(` follow; otherwise it starts the
    // return type of a function written without parentheses.
    let is_method = name.is_some()
        && parser.check_symbol(":")
        && parser
            .tokens
            .get(parser.pos + 1)
            .is_some_and(|t| t.kind == "name")
        && parser
            .tokens
            .get(parser.pos + 2)
            .is_some_and(|t| t.kind == "symbol" && t.value == "(");
    let name = if is_method {
        parser.expect_symbol(":")?;
        let method = parser.expect_name()?;
        name.map(|n| format!("{}.{}", n, method))
    } else {
        name
    };

    let mut params = if parser.match_symbol("(") {
        parse_param_list(parser)?
    } else {
        Vec::new()
    };
    if is_method {
        params.insert(0, Param::new("self".to_string(), None, false));
    }

    let return_type = if parser.match_symbol(":") {
        Some(read_type_annotation(parser))
    } else {
        None
    };

    let body = parse_block_until(parser, "end")?;
    parser.expect_keyword("end")?;
    Ok(Stmt::Function {
        id: NodeId::next(),
        name,
        is_const,
        params,
        return_type,
        body,
    })
}

pub fn parse_do(parser: &mut Parser) -> Result<Stmt, String> {
    parser.expect_keyword("do")?;
    let body = parse_block_until(parser, "end")?;
    parser.expect_keyword("end")?;
    Ok(Stmt::if_stmt(
        Expr::bool(true),
        body,
        Vec::new(),
        None,
        NodeId::next(),
    ))
}

pub fn parse_if(parser: &mut Parser) -> Result<Stmt, String> {
    parser.expect_keyword("if")?;
    let condition = parse_expr(parser)?;
    parser.expect_keyword("then")?;

    let then_branch = parse_block_until_any(parser, &["else", "elseif", "end"])?;
    parse_if_rest(parser, condition, then_branch)
}

#[inline(never)]
fn parse_if_rest(
    parser: &mut Parser,
    condition: Expr,
    then_branch: Vec<Stmt>,
) -> Result<Stmt, String> {
    let mut else_if_branches = Vec::new();
    let mut else_branch = None;

    while parser.check_keyword("elseif") {
        parser.expect_keyword("elseif")?;
        let next_condition = parse_expr(parser)?;
        parser.expect_keyword("then")?;
        let next_branch = parse_block_until_any(parser, &["else", "elseif", "end"])?;
        else_if_branches.push((next_condition, next_branch));
    }

    if parser.check_keyword("else") {
        parser.expect_keyword("else")?;
        else_branch = Some(parse_block_until(parser, "end")?);
    }

    parser.expect_keyword("end")?;

    Ok(Stmt::if_stmt(
        condition,
        then_branch,
        else_if_branches,
        else_branch,
        NodeId::next(),
    ))
}

pub fn parse_for(parser: &mut Parser) -> Result<Stmt, String> {
    parser.expect_keyword("for")?;
    let first_name = parser.expect_name()?;
    if parser.match_symbol("=") {
        let start = parse_expr(parser)?;
        parse_numeric_for(parser, first_name, start)
    } else {
        parse_generic_for(parser, first_name)
    }
}

#[inline(never)]
fn parse_numeric_for(parser: &mut Parser, var: String, start: Expr) -> Result<Stmt, String> {
    parser.expect_symbol(",")?;
    let end = parse_expr(parser)?;
    let step = if parser.match_symbol(",") {
        Some(parse_expr(parser)?)
    } else {
        None
    };
    parser.expect_keyword("do")?;
    let body = parse_block_until(parser, "end")?;
    parser.expect_keyword("end")?;
    Ok(Stmt::numeric_for(
        var,
        start,
        end,
        step,
        body,
        NodeId::next(),
    ))
}

#[inline(never)]
fn parse_generic_for(parser: &mut Parser, first_name: String) -> Result<Stmt, String> {
    let mut vars = vec![first_name];
    if parser.match_symbol(",") {
        vars.push(parser.expect_name()?);
    }
    parser.expect_keyword("in")?;
    let source = parse_expr(parser)?;
    parser.expect_keyword("do")?;
    let body = parse_block_until(parser, "end")?;
    parser.expect_keyword("end")?;
    Ok(Stmt::For {
        id: NodeId::next(),
        vars,
        source,
        body,
    })
}

pub fn parse_while(parser: &mut Parser) -> Result<Stmt, String> {
    parser.expect_keyword("while")?;
    let condition = parse_expr(parser)?;
    parser.expect_keyword("do")?;
    let body = parse_block_until(parser, "end")?;
    parser.expect_keyword("end")?;
    Ok(Stmt::While {
        id: NodeId::next(),
        condition,
        body,
    })
}

pub fn parse_repeat(parser: &mut Parser) -> Result<Stmt, String> {
    parser.expect_keyword("repeat")?;
    let body = parse_block_until(parser, "until")?;
    parser.expect_keyword("until")?;
    let condition = parse_expr(parser)?;
    Ok(Stmt::Repeat {
        id: NodeId::next(),
        body,
        condition,
    })
}

pub fn parse_param_list(parser: &mut Parser) -> Result<Vec<Param>, String> {
    let mut params = Vec::new();
    if parser.check_symbol(")") {
        parser.expect_symbol(")")?;
        return Ok(params);
    }

    loop {
        if parser.match_symbol("...") {
            let type_name = if parser.match_symbol(":") {
                Some(read_type_annotation(parser))
            } else {
                None
            };
            params.push(Param {
                name: "...".to_string(),
                type_name,
                variadic: true,
            });
            parser.expect_symbol(")")?;
            break;
        }
        let name = parser.expect_name()?;
        let type_name = if parser.match_symbol(":") {
            Some(read_type_annotation(parser))
        } else {
            None
        };
        params.push(Param {
            name,
            type_name,
            variadic: false,
        });

        if parser.match_symbol(",") {
            continue;
        }
        parser.expect_symbol(")")?;
        break;
    }
    Ok(params)
}

pub fn parse_block_until(parser: &mut Parser, end_kw: &str) -> Result<Vec<Stmt>, String> {
    parse_block_until_any(parser, &[end_kw])
}

pub fn parse_block_until_any(parser: &mut Parser, end_words: &[&str]) -> Result<Vec<Stmt>, String> {
    let mut block = Vec::new();
    while !parser.is_eof() {
        if parser.check_symbol(";") {
            parser.pos += 1;
            continue;
        }
        let mut hit = false;
        for &word in end_words {
            if parser.check_keyword(word) {
                hit = true;
                break;
            }
        }
        if hit {
            break;
        }
        block.push(parse_statement(parser)?);
    }
    Ok(block)
}

pub fn match_compound_assignment(parser: &mut Parser) -> Option<String> {
    let token = parser.peek();
    if token.kind != "symbol" {
        return None;
    }
    if ActionHelper::compound_assign_binop(&token.value).is_none() && token.value != "??=" {
        return None;
    }
    let op = token.value[..token.value.len() - 1].to_string();
    parser.pos += 1;
    Some(op)
}

pub fn parse_enum(parser: &mut Parser) -> Result<Stmt, String> {
    parser.expect_word("enum")?;
    let enum_name = parser.expect_name()?;
    let mut table_entries = Vec::new();

    while !parser.check_word("end") && !parser.is_eof() {
        while parser.match_symbol(";") || parser.match_symbol(",") {}
        if parser.check_word("end") {
            break;
        }
        let variant_name = parser.expect_name()?;
        let mut fields = Vec::new();
        if parser.match_symbol("(") {
            if !parser.check_symbol(")") {
                fields.push(parser.expect_name()?);
                while parser.match_symbol(",") {
                    if parser.check_symbol(")") {
                        break;
                    }
                    fields.push(parser.expect_name()?);
                }
            }
            parser.expect_symbol(")")?;
        }

        if fields.is_empty() {
            let variant_entries = vec![
                TableEntry {
                    key: Some("__enum".to_string()),
                    computed: None,
                    value: Expr::string(enum_name.clone()),
                },
                TableEntry {
                    key: Some("__variant".to_string()),
                    computed: None,
                    value: Expr::string(variant_name.clone()),
                },
            ];
            let variant_table = Expr::Table {
                entries: variant_entries,
                id: NodeId::next(),
            };
            table_entries.push(TableEntry {
                key: Some(variant_name),
                computed: None,
                value: variant_table,
            });
        } else {
            let mut ret_entries = Vec::new();
            ret_entries.push(TableEntry {
                key: Some("__enum".to_string()),
                computed: None,
                value: Expr::string(enum_name.clone()),
            });
            ret_entries.push(TableEntry {
                key: Some("__variant".to_string()),
                computed: None,
                value: Expr::string(variant_name.clone()),
            });
            let mut params = Vec::new();
            for field in &fields {
                params.push(Param::new(field.clone(), None, false));
                ret_entries.push(TableEntry {
                    key: Some(field.clone()),
                    computed: None,
                    value: Expr::Variable {
                        name: field.clone(),
                        id: NodeId::next(),
                    },
                });
                ret_entries.push(TableEntry {
                    key: None,
                    computed: None,
                    value: Expr::Variable {
                        name: field.clone(),
                        id: NodeId::next(),
                    },
                });
            }

            let ret_stmt = Stmt::Return {
                values: vec![Expr::Table {
                    entries: ret_entries,
                    id: NodeId::next(),
                }],
                id: NodeId::next(),
            };

            let constructor = Expr::Function {
                params,
                body: vec![ret_stmt],
                id: NodeId::next(),
            };

            table_entries.push(TableEntry {
                key: Some(variant_name),
                computed: None,
                value: constructor,
            });
        }

        while parser.match_symbol(";") || parser.match_symbol(",") {}
    }

    parser.expect_keyword("end")?;

    Ok(Stmt::Local {
        name: enum_name,
        is_const: true,
        is_global: false,
        type_name: None,
        initializer: Some(Expr::Table {
            entries: table_entries,
            id: NodeId::next(),
        }),
        id: NodeId::next(),
    })
}

fn find_case_colon(parser: &Parser) -> Option<usize> {
    let mut depth: usize = 0;
    let mut last_colon = None;
    let mut i = parser.pos;
    while i < parser.tokens.len() {
        let tok = &parser.tokens[i];
        if tok.kind == "symbol" {
            match tok.value.as_str() {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth = depth.saturating_sub(1),
                ":" if depth == 0 => {
                    last_colon = Some(i);
                }
                _ => {}
            }
        }
        if depth == 0
            && (tok.kind == "name" || tok.kind == "keyword")
            && (tok.value == "case" || tok.value == "default" || tok.value == "end")
        {
            break;
        }
        i += 1;
    }
    last_colon
}

pub fn parse_match(parser: &mut Parser) -> Result<Stmt, String> {
    parser.expect_word("match")?;
    let target_expr = parse_expr(parser)?;

    let match_var_id = NodeId::next();
    let match_var_name = format!("__match_val_{}", match_var_id.0);

    let mut cases: Vec<(Expr, Vec<Stmt>)> = Vec::new();
    let mut default_body: Option<Vec<Stmt>> = None;

    while !parser.check_word("end") && !parser.is_eof() {
        if parser.match_word("default") {
            parser.expect_symbol(":")?;
            let mut stmts = Vec::new();
            while !parser.check_word("case")
                && !parser.check_word("default")
                && !parser.check_word("end")
                && !parser.is_eof()
            {
                stmts.push(parse_statement(parser)?);
            }
            default_body = Some(stmts);
        } else if parser.match_word("case") {
            if parser.match_symbol("_") {
                parser.expect_symbol(":")?;
                let mut stmts = Vec::new();
                while !parser.check_word("case")
                    && !parser.check_word("default")
                    && !parser.check_word("end")
                    && !parser.is_eof()
                {
                    stmts.push(parse_statement(parser)?);
                }
                default_body = Some(stmts);
            } else {
                parser.case_colon_pos = find_case_colon(parser);
                let pattern_res = parse_pattern_test(parser, &match_var_name);
                let (condition, mut bindings) = match pattern_res {
                    Ok(res) => res,
                    Err(e) => {
                        parser.case_colon_pos = None;
                        return Err(e);
                    }
                };
                let condition = if parser.match_word("if") {
                    let guard_res = parse_expr(parser);
                    let mut guard = match guard_res {
                        Ok(g) => g,
                        Err(e) => {
                            parser.case_colon_pos = None;
                            return Err(e);
                        }
                    };
                    for b in &bindings {
                        if let Stmt::Local {
                            name,
                            initializer: Some(init),
                            ..
                        } = b
                        {
                            substitute_expr(&mut guard, name, init);
                        }
                    }
                    Expr::Binary {
                        left: Box::new(condition),
                        op: crate::ast::op::BinOp::And,
                        right: Box::new(guard),
                        id: NodeId::next(),
                    }
                } else {
                    condition
                };
                let colon_res = parser.expect_symbol(":");
                parser.case_colon_pos = None;
                colon_res?;
                let mut body = Vec::new();
                while !parser.check_word("case")
                    && !parser.check_word("default")
                    && !parser.check_word("end")
                    && !parser.is_eof()
                {
                    body.push(parse_statement(parser)?);
                }
                bindings.extend(body);
                cases.push((condition, bindings));
            }
        } else {
            return Err(format!(
                "[line {}] expected 'case', 'default', or 'end' in match statement",
                parser.peek().line
            ));
        }
    }

    parser.expect_keyword("end")?;

    let inner_if = if cases.is_empty() {
        if let Some(def) = default_body {
            Stmt::if_stmt(Expr::bool(true), def, Vec::new(), None, NodeId::next())
        } else {
            Stmt::Expr {
                expr: Expr::nil(),
                id: NodeId::next(),
            }
        }
    } else {
        let (first_cond, first_body) = cases.remove(0);
        Stmt::if_stmt(first_cond, first_body, cases, default_body, NodeId::next())
    };

    Ok(Stmt::if_stmt(
        Expr::bool(true),
        vec![
            Stmt::Local {
                name: match_var_name,
                is_const: true,
                is_global: false,
                type_name: None,
                initializer: Some(target_expr),
                id: NodeId::next(),
            },
            inner_if,
        ],
        Vec::new(),
        None,
        NodeId::next(),
    ))
}

fn parse_pattern_test(
    parser: &mut Parser,
    match_var_name: &str,
) -> Result<(Expr, Vec<Stmt>), String> {
    let match_var = || Expr::Variable {
        name: match_var_name.to_string(),
        id: NodeId::next(),
    };

    if parser.match_keyword("true") {
        let cond = Expr::Binary {
            left: Box::new(match_var()),
            op: crate::ast::op::BinOp::Eq,
            right: Box::new(Expr::bool(true)),
            id: NodeId::next(),
        };
        return Ok((cond, Vec::new()));
    }
    if parser.match_keyword("false") {
        let cond = Expr::Binary {
            left: Box::new(match_var()),
            op: crate::ast::op::BinOp::Eq,
            right: Box::new(Expr::bool(false)),
            id: NodeId::next(),
        };
        return Ok((cond, Vec::new()));
    }
    if parser.match_keyword("nil") {
        let cond = Expr::Binary {
            left: Box::new(match_var()),
            op: crate::ast::op::BinOp::Eq,
            right: Box::new(Expr::nil()),
            id: NodeId::next(),
        };
        return Ok((cond, Vec::new()));
    }

    if parser.peek().kind == "number" || parser.peek().kind == "string" {
        let lit_expr = parse_expr(parser)?;
        let cond = Expr::Binary {
            left: Box::new(match_var()),
            op: crate::ast::op::BinOp::Eq,
            right: Box::new(lit_expr),
            id: NodeId::next(),
        };
        return Ok((cond, Vec::new()));
    }

    let first_name = parser.expect_name()?;
    if parser.match_symbol(".") {
        let second_name = parser.expect_name()?;
        if parser.match_symbol("(") {
            parse_variant_pattern_args(parser, match_var_name, Some(&first_name), &second_name)
        } else {
            let is_table = Expr::Binary {
                left: Box::new(Expr::Call {
                    callee: Box::new(Expr::Variable {
                        name: "type".to_string(),
                        id: NodeId::next(),
                    }),
                    args: vec![match_var()],
                    id: NodeId::next(),
                }),
                op: crate::ast::op::BinOp::Eq,
                right: Box::new(Expr::string("table")),
                id: NodeId::next(),
            };
            let check_variant = Expr::Binary {
                left: Box::new(Expr::Member {
                    object: Box::new(match_var()),
                    field: "__variant".to_string(),
                    id: NodeId::next(),
                }),
                op: crate::ast::op::BinOp::Eq,
                right: Box::new(Expr::string(second_name.clone())),
                id: NodeId::next(),
            };
            let check_enum = Expr::Binary {
                left: Box::new(Expr::Member {
                    object: Box::new(match_var()),
                    field: "__enum".to_string(),
                    id: NodeId::next(),
                }),
                op: crate::ast::op::BinOp::Eq,
                right: Box::new(Expr::string(first_name.clone())),
                id: NodeId::next(),
            };
            let table_cond = Expr::Binary {
                left: Box::new(Expr::Binary {
                    left: Box::new(is_table),
                    op: crate::ast::op::BinOp::And,
                    right: Box::new(check_variant),
                    id: NodeId::next(),
                }),
                op: crate::ast::op::BinOp::And,
                right: Box::new(check_enum),
                id: NodeId::next(),
            };
            let direct_eq = Expr::Binary {
                left: Box::new(match_var()),
                op: crate::ast::op::BinOp::Eq,
                right: Box::new(Expr::Member {
                    object: Box::new(Expr::Variable {
                        name: first_name,
                        id: NodeId::next(),
                    }),
                    field: second_name,
                    id: NodeId::next(),
                }),
                id: NodeId::next(),
            };
            let cond = Expr::Binary {
                left: Box::new(table_cond),
                op: crate::ast::op::BinOp::Or,
                right: Box::new(direct_eq),
                id: NodeId::next(),
            };
            Ok((cond, Vec::new()))
        }
    } else if parser.match_symbol("(") {
        parse_variant_pattern_args(parser, match_var_name, None, &first_name)
    } else {
        let cond = Expr::bool(true);
        let binding = Stmt::Local {
            name: first_name,
            is_const: false,
            is_global: false,
            type_name: None,
            initializer: Some(match_var()),
            id: NodeId::next(),
        };
        Ok((cond, vec![binding]))
    }
}

fn parse_variant_pattern_args(
    parser: &mut Parser,
    match_var_name: &str,
    enum_name: Option<&str>,
    variant_name: &str,
) -> Result<(Expr, Vec<Stmt>), String> {
    let match_var = || Expr::Variable {
        name: match_var_name.to_string(),
        id: NodeId::next(),
    };

    let is_table = Expr::Binary {
        left: Box::new(Expr::Call {
            callee: Box::new(Expr::Variable {
                name: "type".to_string(),
                id: NodeId::next(),
            }),
            args: vec![match_var()],
            id: NodeId::next(),
        }),
        op: crate::ast::op::BinOp::Eq,
        right: Box::new(Expr::string("table")),
        id: NodeId::next(),
    };

    let check_variant = Expr::Binary {
        left: Box::new(Expr::Member {
            object: Box::new(match_var()),
            field: "__variant".to_string(),
            id: NodeId::next(),
        }),
        op: crate::ast::op::BinOp::Eq,
        right: Box::new(Expr::string(variant_name)),
        id: NodeId::next(),
    };

    let mut condition = Expr::Binary {
        left: Box::new(is_table),
        op: crate::ast::op::BinOp::And,
        right: Box::new(check_variant),
        id: NodeId::next(),
    };

    if let Some(ename) = enum_name {
        let check_enum = Expr::Binary {
            left: Box::new(Expr::Member {
                object: Box::new(match_var()),
                field: "__enum".to_string(),
                id: NodeId::next(),
            }),
            op: crate::ast::op::BinOp::Eq,
            right: Box::new(Expr::string(ename)),
            id: NodeId::next(),
        };
        condition = Expr::Binary {
            left: Box::new(condition),
            op: crate::ast::op::BinOp::And,
            right: Box::new(check_enum),
            id: NodeId::next(),
        };
    }

    let mut bindings = Vec::new();
    let mut arg_idx = 1;

    if !parser.check_symbol(")") {
        loop {
            if parser.match_symbol("_") {
                // Wildcard
            } else if parser.peek().kind == "number"
                || parser.peek().kind == "string"
                || parser.check_keyword("true")
                || parser.check_keyword("false")
                || parser.check_keyword("nil")
            {
                let lit = parse_expr(parser)?;
                let field_access = Expr::Index {
                    object: Box::new(match_var()),
                    index: Box::new(Expr::int(arg_idx as i64)),
                    id: NodeId::next(),
                };
                let check_lit = Expr::Binary {
                    left: Box::new(field_access),
                    op: crate::ast::op::BinOp::Eq,
                    right: Box::new(lit),
                    id: NodeId::next(),
                };
                condition = Expr::Binary {
                    left: Box::new(condition),
                    op: crate::ast::op::BinOp::And,
                    right: Box::new(check_lit),
                    id: NodeId::next(),
                };
            } else {
                let var_name = parser.expect_name()?;
                let field_access = Expr::Index {
                    object: Box::new(match_var()),
                    index: Box::new(Expr::int(arg_idx as i64)),
                    id: NodeId::next(),
                };
                bindings.push(Stmt::Local {
                    name: var_name,
                    is_const: false,
                    is_global: false,
                    type_name: None,
                    initializer: Some(field_access),
                    id: NodeId::next(),
                });
            }
            arg_idx += 1;
            if !parser.match_symbol(",") {
                break;
            }
        }
    }
    parser.expect_symbol(")")?;

    Ok((condition, bindings))
}

fn substitute_expr(expr: &mut Expr, from: &str, to: &Expr) {
    if let Expr::Variable { name, .. } = expr
        && name == from
    {
        *expr = to.clone();
        return;
    }
    match expr {
        Expr::Unary { expr: inner, .. } => substitute_expr(inner, from, to),
        Expr::Binary { left, right, .. } => {
            substitute_expr(left, from, to);
            substitute_expr(right, from, to);
        }
        Expr::Call { callee, args, .. } => {
            substitute_expr(callee, from, to);
            for arg in args {
                substitute_expr(arg, from, to);
            }
        }
        Expr::MethodCall { object, args, .. } => {
            substitute_expr(object, from, to);
            for arg in args {
                substitute_expr(arg, from, to);
            }
        }
        Expr::Member { object, .. } => substitute_expr(object, from, to),
        Expr::Index { object, index, .. } => {
            substitute_expr(object, from, to);
            substitute_expr(index, from, to);
        }
        Expr::Table { entries, .. } => {
            for entry in entries {
                if let Some(comp) = &mut entry.computed {
                    substitute_expr(comp, from, to);
                }
                substitute_expr(&mut entry.value, from, to);
            }
        }
        _ => {}
    }
}
