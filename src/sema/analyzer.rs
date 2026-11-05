//! Walks the syntax tree, builds symbol tables and reports diagnostics.

use std::collections::HashSet;

use crate::ast::expr::{Expr, Param};
use crate::ast::literal::Literal;
use crate::ast::op::{BinOp, UnOp};
use crate::ast::pattern::AssignTarget;
use crate::ast::span::{Span as AstSpan, SpanPool};
use crate::ast::stmt::Stmt;
use crate::ast::ty::TypeExpr;
use crate::diagnostics::diagnostic::Diagnostic;
use crate::diagnostics::error_code::ErrorCode;
use crate::diagnostics::span::Span;
use crate::sema::scope::{ScopeKind, ScopeManager, TypeAlias};
use crate::sema::symbol::{Symbol, SymbolKind};
use crate::sema::types::NeyukiType;

/// How many alias names one annotation may expand to. Aliases that nest
/// aliases can multiply (`type A = {B, B}`, `type B = {C, C}`, ...), so the
/// expansion is cut off, leaving `any`, instead of growing without bound.
const ALIAS_EXPANSION_BUDGET: usize = 512;

pub struct SemanticAnalyzer<'a> {
    pub source: &'a str,
    pub scope_mgr: ScopeManager,
    pub diagnostics: Vec<Diagnostic>,
    pub known_globals: HashSet<String>,
    pub undeclared: HashSet<String>,
    current_return_type: Option<NeyukiType>,
    current_line: u32,
    spans: Option<&'a SpanPool>,
}

impl<'a> SemanticAnalyzer<'a> {
    pub fn new(source: &'a str) -> Self {
        let mut known_globals = HashSet::new();
        let globals = [
            "print",
            "assert",
            "require",
            "type",
            "typeof",
            "checktype",
            "asserttype",
            "assert_type",
            "tostring",
            "tonumber",
            "int",
            "float",
            "pcall",
            "xpcall",
            "try",
            "error",
            "pairs",
            "ipairs",
            "next",
            "select",
            "rawget",
            "rawset",
            "rawequal",
            "rawlen",
            "setmetatable",
            "getmetatable",
            "collectgarbage",
            "math",
            "string",
            "table",
            "bit",
            "bit32",
            "buffer",
            "os",
            "coroutine",
            "task",
            "utf8",
            "debug",
            "json",
            "true",
            "false",
            "nil",
            "_G",
            "_VERSION",
            "warn",
        ];
        for g in globals {
            known_globals.insert(g.to_string());
        }

        Self {
            source,
            scope_mgr: ScopeManager::new(),
            diagnostics: Vec::new(),
            known_globals,
            undeclared: HashSet::new(),
            current_return_type: None,
            current_line: 1,
            spans: None,
        }
    }

    /// Uses the parser's node positions to place diagnostics; without them
    /// every diagnostic points at line 1.
    pub fn with_spans(mut self, spans: &'a SpanPool) -> Self {
        self.spans = Some(spans);
        self
    }

    /// Moves `current_line` to where `stmt` starts.
    fn enter_stmt(&mut self, stmt: &Stmt) {
        if let Some(span) = self.spans.and_then(|pool| pool.get(stmt.node_id()))
            && span.start.line > 0
        {
            self.current_line = span.start.line;
        }
    }

    /// Converts a parser span to a diagnostic span on its first line, with
    /// trailing whitespace trimmed (a parser span ends at the next token).
    fn diagnostic_span(&self, span: crate::ast::span::Span) -> Span {
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

    fn is_known_global(&self, name: &str) -> bool {
        self.known_globals.contains(name) || name.starts_with("__")
    }

    pub fn analyze_program(&mut self, statements: &[Stmt]) {
        self.analyze_block(statements);
        // Flush all scope symbols
        let remaining = self.scope_mgr.flush_all();
        for sym in remaining {
            if !sym.used && !sym.name.starts_with('_') && sym.kind == SymbolKind::Variable {
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

    fn analyze_block(&mut self, statements: &[Stmt]) {
        // Pre-pass: declare type aliases first, so annotations may name an
        // alias declared further down (and aliases may refer to each other).
        for stmt in statements {
            if let Stmt::TypeAlias {
                name,
                ty,
                name_span,
                ..
            } = stmt
            {
                self.declare_type_alias(stmt, name, ty, *name_span);
            }
        }

        // Pre-pass: hoist function declarations in current scope for mutual recursion & forward references
        for stmt in statements {
            if let Stmt::Function {
                name: Some(fn_name),
                is_const,
                params,
                return_type,
                ..
            } = stmt
                && !fn_name.contains('.')
            {
                self.hoist_function(stmt, fn_name, *is_const, params, return_type.as_deref());
            }
        }

        let mut unreachable = false;
        for stmt in statements {
            if unreachable {
                self.warn_unreachable(stmt);
                continue;
            }

            self.analyze_stmt(stmt);

            if matches!(
                stmt,
                Stmt::Return { .. } | Stmt::Break { .. } | Stmt::Continue { .. }
            ) {
                unreachable = true;
            }
        }
    }

    /// Resolves an annotation to a type, expanding the local type aliases it
    /// names. Names no alias declares stay unchecked `Custom` types.
    fn parse_ty(&self, text: &str) -> NeyukiType {
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
    fn expand_aliases(
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
    fn declare_type_alias(&mut self, stmt: &Stmt, name: &str, ty: &str, name_span: AstSpan) {
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

    #[inline(never)]
    fn hoist_function(
        &mut self,
        stmt: &Stmt,
        fn_name: &str,
        is_const: bool,
        params: &[Param],
        return_type: Option<&str>,
    ) {
        self.enter_stmt(stmt);
        let span = self.find_ident_span(fn_name);
        let param_types: Vec<NeyukiType> = params
            .iter()
            .map(|p| {
                p.type_name
                    .as_deref()
                    .map(|t| self.parse_ty(t))
                    .unwrap_or(NeyukiType::Any)
            })
            .collect();
        let is_vararg = params.iter().any(|p| p.variadic);
        let ret_ty = return_type.map(|t| self.parse_ty(t));
        let fn_type = NeyukiType::Function {
            params: param_types,
            return_type: Box::new(ret_ty.unwrap_or(NeyukiType::Any)),
            is_vararg,
        };
        let mut sym = Symbol::new(
            fn_name.to_string(),
            SymbolKind::Function,
            is_const,
            Some(fn_type),
            span,
        );
        sym.num_params = Some(params.iter().filter(|p| !p.variadic).count());
        sym.min_params = Some(required_params(params, |t| self.parse_ty(t)));
        sym.is_vararg = is_vararg;
        let _ = self.scope_mgr.define(sym);
    }

    #[inline(never)]
    fn warn_unreachable(&mut self, stmt: &Stmt) {
        self.enter_stmt(stmt);
        let span = self.find_stmt_span(stmt);
        let diag = Diagnostic::warning("unreachable code")
            .with_code(ErrorCode::W0004)
            .with_label(span, "unreachable statement follows terminal control flow");
        self.diagnostics.push(diag);
    }

    fn analyze_stmt(&mut self, stmt: &Stmt) {
        self.enter_stmt(stmt);
        match stmt {
            Stmt::Local {
                name,
                is_const,
                is_global,
                type_name,
                initializer,
                ..
            } => {
                self.analyze_local(
                    name,
                    *is_const,
                    *is_global,
                    type_name.as_deref(),
                    initializer.as_ref(),
                );
            }
            Stmt::LocalMany {
                names,
                is_const,
                is_global,
                initializers,
                ..
            } => {
                self.analyze_local_many(names, *is_const, *is_global, initializers);
            }
            Stmt::Assign { target, value, .. } => {
                self.analyze_expr(value);
                self.check_assign_target(target, value);
            }
            Stmt::AssignMany {
                targets, values, ..
            } => {
                self.analyze_assign_many(targets, values);
            }
            Stmt::Increment { target, .. } => {
                self.analyze_increment(target);
            }
            Stmt::Function {
                name,
                is_const,
                params,
                return_type,
                body,
                ..
            } => {
                let ret_ty = return_type.as_deref().map(|t| self.parse_ty(t));
                if let Some((object, _)) = name.as_deref().and_then(|n| n.split_once('.')) {
                    // `function t.f()` and `function t:m()` store into a
                    // field of `t`; they read `t` and declare nothing.
                    self.read_variable(object);
                } else if let Some(fn_name) = name {
                    let span = self.find_ident_span(fn_name);
                    self.check_shadowing(fn_name, span);

                    let param_types: Vec<NeyukiType> = params
                        .iter()
                        .map(|p| {
                            p.type_name
                                .as_deref()
                                .map(|t| self.parse_ty(t))
                                .unwrap_or(NeyukiType::Any)
                        })
                        .collect();

                    let is_vararg = params.iter().any(|p| p.variadic);
                    let fn_type = NeyukiType::Function {
                        params: param_types,
                        return_type: Box::new(ret_ty.clone().unwrap_or(NeyukiType::Any)),
                        is_vararg,
                    };

                    let mut sym = Symbol::new(
                        fn_name.clone(),
                        SymbolKind::Function,
                        *is_const,
                        Some(fn_type),
                        span,
                    );
                    sym.num_params = Some(params.iter().filter(|p| !p.variadic).count());
                    sym.min_params = Some(required_params(params, |t| self.parse_ty(t)));
                    sym.is_vararg = is_vararg;
                    let _ = self.scope_mgr.define(sym);
                }

                // Analyze function body with its own scope
                self.scope_mgr.enter_scope(ScopeKind::Function);
                let prev_ret = self.current_return_type.clone();
                self.current_return_type = ret_ty;

                for param in params {
                    let span = self.find_ident_span(&param.name);
                    let decl_ty = param.type_name.as_deref().map(|t| self.parse_ty(t));
                    let sym = Symbol::new(
                        param.name.clone(),
                        SymbolKind::Parameter,
                        false,
                        decl_ty,
                        span,
                    );
                    let _ = self.scope_mgr.define(sym);
                }

                self.analyze_block(body);

                self.check_unused_symbols();
                self.current_return_type = prev_ret;
            }
            Stmt::If { data, .. } => {
                self.analyze_expr(&data.condition);
                self.scope_mgr.enter_scope(ScopeKind::Block);
                self.analyze_block(&data.then_branch);
                self.check_unused_symbols();

                for (cond, branch) in &data.else_if_branches {
                    self.analyze_expr(cond);
                    self.scope_mgr.enter_scope(ScopeKind::Block);
                    self.analyze_block(branch);
                    self.check_unused_symbols();
                }

                if let Some(branch) = &data.else_branch {
                    self.scope_mgr.enter_scope(ScopeKind::Block);
                    self.analyze_block(branch);
                    self.check_unused_symbols();
                }
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.analyze_expr(condition);
                self.scope_mgr.enter_scope(ScopeKind::Loop);
                self.analyze_block(body);
                self.check_unused_symbols();
            }
            Stmt::Repeat {
                body, condition, ..
            } => {
                self.scope_mgr.enter_scope(ScopeKind::Loop);
                self.analyze_block(body);
                self.analyze_expr(condition);
                self.check_unused_symbols();
            }
            Stmt::NumericFor { data, .. } => {
                self.analyze_expr(&data.start);
                self.analyze_expr(&data.end);
                if let Some(s) = &data.step {
                    self.analyze_expr(s);
                }
                self.scope_mgr.enter_scope(ScopeKind::Loop);
                let span = self.find_ident_span(&data.var);
                let mut sym = Symbol::new(
                    data.var.clone(),
                    SymbolKind::Variable,
                    false,
                    Some(NeyukiType::Number),
                    span,
                );
                sym.used = true; // loop variable is automatically considered used
                let _ = self.scope_mgr.define(sym);
                self.analyze_block(&data.body);
                self.check_unused_symbols();
            }
            Stmt::For {
                vars, source, body, ..
            } => {
                self.analyze_expr(source);
                self.scope_mgr.enter_scope(ScopeKind::Loop);
                for v in vars {
                    let span = self.find_ident_span(v);
                    let mut sym = Symbol::new(v.clone(), SymbolKind::Variable, false, None, span);
                    sym.used = true;
                    let _ = self.scope_mgr.define(sym);
                }
                self.analyze_block(body);
                self.check_unused_symbols();
            }
            Stmt::Return { values: exprs, .. } => {
                self.analyze_return(exprs);
            }
            // Declared by the pre-pass of the block that holds it.
            Stmt::TypeAlias { .. } => {}
            Stmt::Break { .. } | Stmt::Continue { .. } | Stmt::Goto { .. } | Stmt::Label { .. } => {
            }
            Stmt::Expr { expr: e, .. } => {
                self.analyze_expr(e);
            }
        }
    }

    fn check_assign_target(&mut self, target: &AssignTarget, value: &Expr) {
        match target {
            AssignTarget::Variable(name) => {
                let span = self.find_ident_span(name);
                let val_type = self.infer_expr_type(value);

                if let Some(sym) = self.scope_mgr.lookup_mut(name) {
                    sym.assigned_count += 1;
                    sym.used = true;
                    if sym.is_const {
                        let diag = Diagnostic::error(format!(
                            "cannot reassign to const variable '{}'",
                            name
                        ))
                        .with_code(ErrorCode::E0002)
                        .with_label(span, "cannot reassign to a const variable")
                        .with_secondary_label(sym.span, "defined as const here");
                        self.diagnostics.push(diag);
                    } else if let Some(decl_type) = &sym.declared_type
                        && val_type != NeyukiType::Any
                        && !val_type.is_assignable_to(decl_type)
                    {
                        let diag = Diagnostic::error(format!(
                            "type mismatch in assignment to '{}': expected '{}', found '{}'",
                            name,
                            decl_type.display_name(),
                            val_type.display_name()
                        ))
                        .with_code(ErrorCode::E0003)
                        .with_label(
                            span,
                            format!(
                                "expected '{}', found '{}'",
                                decl_type.display_name(),
                                val_type.display_name()
                            ),
                        );
                        self.diagnostics.push(diag);
                    } else if let Some(decl_type) = &sym.declared_type {
                        // Assigning a value that cannot be nil narrows an
                        // optional variable (`x = x ?? default`) until the
                        // next assignment. Branches are not merged, so this
                        // only ever removes diagnostics.
                        let non_nil = decl_type.narrow_non_nil();
                        sym.inferred_type =
                            if val_type != NeyukiType::Any && val_type.is_assignable_to(&non_nil) {
                                non_nil
                            } else {
                                decl_type.clone()
                            };
                    }
                } else if !self.is_known_global(name) {
                    self.undeclared.insert(name.to_string());
                    let diag = Diagnostic::error(format!(
                        "variable '{}' is used before declaration",
                        name
                    ))
                    .with_code(ErrorCode::E0001)
                    .with_label(span, "not found in this scope")
                    .with_help(format!("declare 'local {} = ...' before using it", name));
                    self.diagnostics.push(diag);
                }
            }
            AssignTarget::Member { object, .. } => {
                self.analyze_expr(object);
            }
            AssignTarget::Index { object, index } => {
                self.analyze_expr(object);
                self.analyze_expr(index);
            }
        }
    }

    /// Marks `name` used, or reports it when nothing by that name exists.
    fn read_variable(&mut self, name: &str) {
        let span = self.find_ident_span(name);
        if !self.scope_mgr.mark_used(name) && !self.is_known_global(name) {
            self.undeclared.insert(name.to_string());
            let diag = Diagnostic::error(format!("cannot find variable '{}' in this scope", name))
                .with_code(ErrorCode::E0001)
                .with_label(span, "not found in this scope")
                .with_help(format!(
                    "declare 'local {} = ...' before accessing it",
                    name
                ));
            self.diagnostics.push(diag);
        }
    }

    fn analyze_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Variable { name, .. } => self.read_variable(name),
            Expr::Call { callee, args, .. } => {
                self.analyze_expr(callee);
                for a in args {
                    self.analyze_expr(a);
                }
                // Arity check if callee is a known local function
                if let Expr::Variable { name: fn_name, .. } = callee.as_ref()
                    && let Some(sym) = self.scope_mgr.lookup(fn_name)
                    && let Some(expected) = sym.num_params
                    && !sym.is_vararg
                    && (args.len() > expected || args.len() < sym.min_params.unwrap_or(expected))
                {
                    let span = self.find_ident_span(fn_name);
                    let diag = Diagnostic::warning(format!(
                        "function '{}' takes {} argument(s) but {} were supplied",
                        fn_name,
                        expected,
                        args.len()
                    ))
                    .with_code(ErrorCode::E0004)
                    .with_label(span, format!("takes {} arguments", expected))
                    .with_secondary_label(sym.span, "defined here");
                    self.diagnostics.push(diag);
                }
            }
            Expr::MethodCall { object, args, .. } => {
                self.analyze_expr(object);
                for a in args {
                    self.analyze_expr(a);
                }
            }
            Expr::Member { object, .. } => {
                self.analyze_expr(object);
            }
            Expr::Index { object, index, .. } => {
                self.analyze_expr(object);
                self.analyze_expr(index);
            }
            Expr::Unary { expr, .. } => {
                self.analyze_expr(expr);
            }
            Expr::Binary { left, right, .. } => {
                self.analyze_expr(left);
                self.analyze_expr(right);
            }
            Expr::Table { entries, .. } => {
                for entry in entries {
                    if let Some(key) = &entry.computed {
                        self.analyze_expr(key);
                    }
                    self.analyze_expr(&entry.value);
                }
            }
            Expr::Function { params, body, .. } => {
                // A function expression keeps no return annotation, so its
                // returns are not checked against the enclosing function's.
                let prev_ret = self.current_return_type.take();
                self.scope_mgr.enter_scope(ScopeKind::Function);
                for param in params {
                    let span = self.find_ident_span(&param.name);
                    let decl_ty = param.type_name.as_deref().map(|t| self.parse_ty(t));
                    let sym = Symbol::new(
                        param.name.clone(),
                        SymbolKind::Parameter,
                        false,
                        decl_ty,
                        span,
                    );
                    let _ = self.scope_mgr.define(sym);
                }
                self.analyze_block(body);
                self.check_unused_symbols();
                self.current_return_type = prev_ret;
            }
            Expr::Interp { parts, .. } => {
                for part in parts {
                    if let crate::ast::expr::InterpPart::Expr(e) = part {
                        self.analyze_expr(e);
                    }
                }
            }
            Expr::Vararg { .. } => {
                self.scope_mgr.mark_used("...");
            }
            Expr::Literal { .. } => {}
        }
    }

    fn infer_expr_type(&self, expr: &Expr) -> NeyukiType {
        match expr {
            Expr::Literal { value: val, .. } => match val {
                Literal::Nil => NeyukiType::Nil,
                Literal::Bool(_) => NeyukiType::Boolean,
                Literal::Int(_) => NeyukiType::Int,
                Literal::Float(_) => NeyukiType::Float,
                Literal::String(_) => NeyukiType::String,
            },
            Expr::Table { entries, .. } => {
                if entries.is_empty() {
                    NeyukiType::Table
                } else if entries.iter().all(|e| e.is_array_element()) {
                    let elem_ty = entries.iter().fold(NeyukiType::Never, |acc, e| {
                        acc.lub(&self.infer_expr_type(&e.value))
                    });
                    NeyukiType::Array(Box::new(elem_ty))
                } else {
                    NeyukiType::Table
                }
            }
            Expr::Function { params, .. } => NeyukiType::Function {
                params: params
                    .iter()
                    .filter(|p| !p.variadic)
                    .map(|p| {
                        p.type_name
                            .as_deref()
                            .map(|t| self.parse_ty(t))
                            .unwrap_or(NeyukiType::Any)
                    })
                    .collect(),
                return_type: Box::new(NeyukiType::Any),
                is_vararg: params.iter().any(|p| p.variadic),
            },
            Expr::Variable { name, .. } => {
                if let Some(sym) = self.scope_mgr.lookup(name) {
                    sym.inferred_type.clone()
                } else {
                    NeyukiType::Any
                }
            }
            Expr::Unary {
                op, expr: inner, ..
            } => match op {
                UnOp::Not => NeyukiType::Boolean,
                UnOp::Len => NeyukiType::Int,
                UnOp::BitNot => NeyukiType::Int,
                UnOp::Neg => {
                    let inner_ty = self.infer_expr_type(inner);
                    if inner_ty == NeyukiType::Int {
                        NeyukiType::Int
                    } else if inner_ty == NeyukiType::Float {
                        NeyukiType::Float
                    } else {
                        NeyukiType::Number
                    }
                }
            },
            Expr::Binary {
                left, op, right, ..
            } => match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    NeyukiType::Boolean
                }
                BinOp::Concat => NeyukiType::String,
                BinOp::Coalesce => {
                    let l_ty = self.infer_expr_type(left).narrow_non_nil();
                    let r_ty = self.infer_expr_type(right);
                    l_ty.lub(&r_ty)
                }
                BinOp::And | BinOp::Or => {
                    let r_ty = self.infer_expr_type(right);
                    let l_ty = self.infer_expr_type(left);
                    l_ty.lub(&r_ty)
                }
                BinOp::BitAnd
                | BinOp::BitOr
                | BinOp::BitXor
                | BinOp::Shl
                | BinOp::Shr
                | BinOp::IDiv
                | BinOp::Mod => NeyukiType::Int,
                BinOp::Div => NeyukiType::Float,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Pow => {
                    let l_ty = self.infer_expr_type(left);
                    let r_ty = self.infer_expr_type(right);
                    // An operand of unknown type (a call, a table field)
                    // could be an integer or a float, or have metamethods.
                    if l_ty == NeyukiType::Any || r_ty == NeyukiType::Any {
                        NeyukiType::Any
                    } else if l_ty == NeyukiType::Int && r_ty == NeyukiType::Int {
                        NeyukiType::Int
                    } else if l_ty == NeyukiType::Float || r_ty == NeyukiType::Float {
                        NeyukiType::Float
                    } else {
                        NeyukiType::Number
                    }
                }
                _ => NeyukiType::Number,
            },
            _ => NeyukiType::Any,
        }
    }

    fn check_shadowing(&mut self, name: &str, span: Span) {
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

    fn check_unused_symbols(&mut self) {
        let symbols = self.scope_mgr.exit_scope();
        for sym in symbols {
            // `self` is part of the method calling convention, even in a
            // method that doesn't read it.
            let is_self = sym.kind == SymbolKind::Parameter && sym.name == "self";
            if !sym.used && !sym.name.starts_with('_') && !is_self {
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

    /// The first whole-word occurrence of `ident` on the current line.
    fn find_ident_span(&self, ident: &str) -> Span {
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
                return Span::single_line(line, col as u32 + 1, after as u32 + 1);
            }
            from = col + 1;
        }
        Span::line(line)
    }

    fn find_stmt_span(&self, stmt: &Stmt) -> Span {
        match self.spans.and_then(|pool| pool.get(stmt.node_id())) {
            Some(span) if span.start.line > 0 => self.diagnostic_span(span),
            _ => Span::line(self.current_line),
        }
    }

    fn find_expr_span(&self, expr: &Expr) -> Span {
        match self.spans.and_then(|pool| pool.get(expr.node_id())) {
            Some(span) if span.start.line > 0 => self.diagnostic_span(span),
            _ => Span::line(self.current_line),
        }
    }

    #[inline(never)]
    fn analyze_local(
        &mut self,
        name: &str,
        is_const: bool,
        is_global: bool,
        type_name: Option<&str>,
        initializer: Option<&Expr>,
    ) {
        let span = self.find_ident_span(name);
        if !is_global {
            self.check_shadowing(name, span);
        }

        let declared_type = type_name.map(|t| self.parse_ty(t));

        let mut sym = Symbol::new(
            name.to_string(),
            SymbolKind::Variable,
            is_const,
            declared_type.clone(),
            span,
        );
        if let Some(init) = initializer {
            sym.inferred_type = self.infer_expr_type(init);
        }
        if is_global {
            if let Err(orig_span) = self.scope_mgr.define_global(sym) {
                let diag = Diagnostic::error(format!("duplicate global variable '{}'", name))
                    .with_code(ErrorCode::E0005)
                    .with_label(span, "redefined here")
                    .with_secondary_label(orig_span, "previous definition was here");
                self.diagnostics.push(diag);
            }
            self.known_globals.insert(name.to_string());
        } else if let Err(orig_span) = self.scope_mgr.define(sym) {
            let diag = Diagnostic::error(format!(
                "duplicate local variable '{}' in the same scope",
                name
            ))
            .with_code(ErrorCode::E0005)
            .with_label(span, "redefined here")
            .with_secondary_label(orig_span, "previous definition was here");
            self.diagnostics.push(diag);
        }

        if let Some(init) = initializer {
            self.analyze_expr(init);
            let init_type = self.infer_expr_type(init);
            if let Some(decl_type) = &declared_type
                && init_type != NeyukiType::Any
                && !init_type.is_assignable_to(decl_type)
            {
                let diag = Diagnostic::error(format!(
                    "type mismatch: variable '{}' declared as '{}' but initialized with '{}'",
                    name,
                    decl_type.display_name(),
                    init_type.display_name()
                ))
                .with_code(ErrorCode::E0003)
                .with_label(
                    span,
                    format!(
                        "expected '{}', found '{}'",
                        decl_type.display_name(),
                        init_type.display_name()
                    ),
                )
                .with_help(format!(
                    "ensure the assigned value matches '{}'",
                    decl_type.display_name()
                ));
                self.diagnostics.push(diag);
            }
        }
    }

    #[inline(never)]
    fn analyze_local_many(
        &mut self,
        names: &[String],
        is_const: bool,
        is_global: bool,
        initializers: &[Expr],
    ) {
        for (i, name) in names.iter().enumerate() {
            let span = self.find_ident_span(name);
            if !is_global {
                self.check_shadowing(name, span);
            }

            let mut sym = Symbol::new(name.clone(), SymbolKind::Variable, is_const, None, span);
            if let Some(init) = initializers.get(i) {
                sym.inferred_type = self.infer_expr_type(init);
            }
            if is_global {
                if let Err(orig_span) = self.scope_mgr.define_global(sym) {
                    let diag = Diagnostic::error(format!("duplicate global variable '{}'", name))
                        .with_code(ErrorCode::E0005)
                        .with_label(span, "redefined here")
                        .with_secondary_label(orig_span, "previous definition was here");
                    self.diagnostics.push(diag);
                }
                self.known_globals.insert(name.clone());
            } else if let Err(orig_span) = self.scope_mgr.define(sym) {
                let diag = Diagnostic::error(format!(
                    "duplicate local variable '{}' in the same scope",
                    name
                ))
                .with_code(ErrorCode::E0005)
                .with_label(span, "redefined here")
                .with_secondary_label(orig_span, "previous definition was here");
                self.diagnostics.push(diag);
            }
        }
        for init in initializers {
            self.analyze_expr(init);
        }
    }

    #[inline(never)]
    fn analyze_assign_many(&mut self, targets: &[AssignTarget], values: &[Expr]) {
        for v in values {
            self.analyze_expr(v);
        }
        for (i, t) in targets.iter().enumerate() {
            if let Some(v) = values.get(i) {
                self.check_assign_target(t, v);
            } else {
                self.check_assign_target(t, &Expr::nil());
            }
        }
    }

    #[inline(never)]
    fn analyze_increment(&mut self, target: &AssignTarget) {
        match target {
            AssignTarget::Variable(name) => {
                let span = self.find_ident_span(name);
                if let Some(sym) = self.scope_mgr.lookup_mut(name) {
                    sym.used = true;
                    if sym.is_const {
                        let diag = Diagnostic::error(format!(
                            "cannot increment const variable '{}'",
                            name
                        ))
                        .with_code(ErrorCode::E0002)
                        .with_label(span, "cannot mutate a const variable")
                        .with_secondary_label(sym.span, "defined as const here");
                        self.diagnostics.push(diag);
                    }
                } else if !self.is_known_global(name) {
                    self.undeclared.insert(name.to_string());
                    let diag = Diagnostic::error(format!("variable '{}' is not declared", name))
                        .with_code(ErrorCode::E0001)
                        .with_label(span, "cannot increment undeclared variable");
                    self.diagnostics.push(diag);
                }
            }
            AssignTarget::Member { object, .. } => {
                self.analyze_expr(object);
            }
            AssignTarget::Index { object, index } => {
                self.analyze_expr(object);
                self.analyze_expr(index);
            }
        }
    }

    #[inline(never)]
    fn analyze_return(&mut self, exprs: &[Expr]) {
        for e in exprs {
            self.analyze_expr(e);
        }
        if let Some(expected_ret) = &self.current_return_type {
            if exprs.is_empty() {
                if *expected_ret != NeyukiType::Nil && *expected_ret != NeyukiType::Any {
                    let span = Span::line(self.current_line);
                    let diag = Diagnostic::error(format!(
                        "empty return in function declared to return '{}'",
                        expected_ret.display_name()
                    ))
                    .with_code(ErrorCode::E0003)
                    .with_label(
                        span,
                        format!("expected return of type '{}'", expected_ret.display_name()),
                    );
                    self.diagnostics.push(diag);
                }
            } else {
                let expected: Vec<NeyukiType> = match expected_ret {
                    NeyukiType::Record(fields)
                        if fields.first().is_some_and(|(name, _)| name == "_0") =>
                    {
                        fields.iter().map(|(_, ty)| ty.clone()).collect()
                    }
                    other => vec![other.clone()],
                };
                let spreads = matches!(
                    exprs.last(),
                    Some(Expr::Call { .. } | Expr::MethodCall { .. } | Expr::Vararg { .. })
                );
                for (index, want) in expected.iter().enumerate() {
                    if spreads && index + 1 >= exprs.len() {
                        break;
                    }
                    let (actual_ret, span) = match exprs.get(index) {
                        Some(expr) => (self.infer_expr_type(expr), self.find_expr_span(expr)),
                        None => (
                            NeyukiType::Nil,
                            self.find_expr_span(&exprs[exprs.len() - 1]),
                        ),
                    };
                    if actual_ret != NeyukiType::Any && !actual_ret.is_assignable_to(want) {
                        let diag = Diagnostic::error(format!(
                            "type mismatch: function declared with return type '{}' returns '{}'",
                            want.display_name(),
                            actual_ret.display_name()
                        ))
                        .with_code(ErrorCode::E0003)
                        .with_label(
                            span,
                            format!(
                                "expected '{}', found '{}'",
                                want.display_name(),
                                actual_ret.display_name()
                            ),
                        );
                        self.diagnostics.push(diag);
                        break;
                    }
                }
            }
        }
    }
}

/// How many arguments a call must pass: the fixed parameters up to the last
/// one that doesn't accept nil, since trailing optional ones may be left out.
fn required_params(params: &[Param], parse: impl Fn(&str) -> NeyukiType) -> usize {
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
