//! Walks the syntax tree, builds symbol tables and coordinates semantic passes.

use std::collections::HashSet;

use crate::ast::attr::{Attribute, AttributeArg, AttributePool};
use crate::ast::expr::{Expr, Param};
use crate::ast::literal::Literal;
use crate::ast::pattern::AssignTarget;
use crate::ast::span::SpanPool;
use crate::ast::stmt::Stmt;
use crate::diagnostics::diagnostic::Diagnostic;
use crate::diagnostics::error_code::ErrorCode;
use crate::diagnostics::span::Span;
use crate::sema::infer::resolve_local_many_slot;
use crate::sema::scope::{ScopeKind, ScopeManager};
use crate::sema::symbol::{Symbol, SymbolKind};
use crate::sema::type_env::required_params;
use crate::sema::types::NeyukiType;

/// Built-in globals recognized by semantic analysis without per-compile heap
/// allocations. Must stay strictly sorted for `binary_search`.
const BUILTIN_GLOBALS: &[&str] = &[
    "_G",
    "_VERSION",
    "assert",
    "assert_type",
    "asserttype",
    "bit",
    "bit32",
    "buffer",
    "checktype",
    "collectgarbage",
    "coroutine",
    "debug",
    "error",
    "false",
    "float",
    "getmetatable",
    "int",
    "ipairs",
    "json",
    "math",
    "next",
    "nil",
    "os",
    "pairs",
    "pcall",
    "print",
    "rawequal",
    "rawget",
    "rawlen",
    "rawset",
    "require",
    "select",
    "setmetatable",
    "string",
    "table",
    "task",
    "tonumber",
    "tostring",
    "true",
    "try",
    "type",
    "typeof",
    "utf8",
    "warn",
    "xpcall",
];

pub struct SemanticAnalyzer<'a> {
    pub source: &'a str,
    pub scope_mgr: ScopeManager,
    pub diagnostics: Vec<Diagnostic>,
    pub known_globals: HashSet<String>,
    pub undeclared: HashSet<String>,
    pub(super) current_return_type: Option<NeyukiType>,
    pub(super) current_line: u32,
    pub(super) spans: Option<&'a SpanPool>,
    pub(super) attributes: Option<&'a AttributePool>,
}

impl<'a> SemanticAnalyzer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            scope_mgr: ScopeManager::new(),
            diagnostics: Vec::new(),
            known_globals: HashSet::new(),
            undeclared: HashSet::new(),
            current_return_type: None,
            current_line: 1,
            spans: None,
            attributes: None,
        }
    }

    /// Uses the parser's node positions to place diagnostics; without them
    /// every diagnostic points at line 1.
    pub fn with_spans(mut self, spans: &'a SpanPool) -> Self {
        self.spans = Some(spans);
        self
    }

    /// Attaches parsed statement/declaration attributes (`@allow`, `@deprecated`, etc.).
    pub fn with_attributes(mut self, attributes: &'a AttributePool) -> Self {
        self.attributes = Some(attributes);
        self
    }

    fn stmt_attrs(&self, stmt: &Stmt) -> &[Attribute] {
        self.attributes
            .map(|pool| pool.get(stmt.node_id()))
            .unwrap_or(&[])
    }

    fn allows_rule(&self, stmt: &Stmt, rule: &str) -> bool {
        self.stmt_attrs(stmt)
            .iter()
            .any(|attr| attr.name == "allow" && attr.has_ident_arg(rule))
    }

    fn deprecated_reason(&self, stmt: &Stmt) -> Option<String> {
        self.stmt_attrs(stmt)
            .iter()
            .find(|attr| attr.name == "deprecated")
            .map(|attr| attr.first_string_arg().unwrap_or("").to_string())
    }

    fn validate_stmt_attributes(&mut self, stmt: &Stmt) {
        let attrs = self.stmt_attrs(stmt).to_vec();
        for attr in attrs {
            let span = if attr.span.start.line > 0 {
                self.diagnostic_span(attr.span)
            } else {
                Span::line(self.current_line)
            };
            if attr.name == "allow" && attr.args.is_empty() {
                let diag = Diagnostic::warning("attribute '@allow' expects at least one rule name")
                    .with_code(ErrorCode::W0007)
                    .with_label(span, "missing rule argument, e.g. '@allow(unused)'");
                self.diagnostics.push(diag);
            } else if attr.name == "deprecated"
                && !attr.args.is_empty()
                && !matches!(
                    attr.args.first(),
                    Some(AttributeArg::Literal(Literal::String(_)))
                )
            {
                let diag = Diagnostic::warning(
                    "attribute '@deprecated' expects an optional string message",
                )
                .with_code(ErrorCode::W0007)
                .with_label(span, "expected string literal reason");
                self.diagnostics.push(diag);
            }
        }
    }

    fn is_known_global(&self, name: &str) -> bool {
        BUILTIN_GLOBALS.binary_search(&name).is_ok()
            || self.known_globals.contains(name)
            || name.starts_with("__")
    }

    pub fn analyze_program(&mut self, statements: &[Stmt]) {
        self.analyze_block(statements);
        self.flush_remaining_unused();
    }

    pub(super) fn analyze_block(&mut self, statements: &[Stmt]) {
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
                is_global,
                params,
                return_type,
                body,
                ..
            } = stmt
                && !fn_name.contains('.')
            {
                self.hoist_function(
                    stmt,
                    fn_name,
                    *is_const,
                    *is_global,
                    params,
                    return_type.as_deref(),
                    body,
                );
            }
        }

        let mut unreachable = false;
        for stmt in statements {
            if unreachable {
                if !self.allows_rule(stmt, "unreachable") {
                    self.warn_unreachable(stmt);
                }
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

    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn hoist_function(
        &mut self,
        stmt: &Stmt,
        fn_name: &str,
        is_const: bool,
        is_global: bool,
        params: &[Param],
        return_type: Option<&str>,
        body: &[Stmt],
    ) {
        self.enter_stmt(stmt);
        let span = self.find_ident_span(fn_name);
        let ret_ty = return_type.map(|t| self.parse_ty(t));
        let fn_type = self.build_function_type(params, body, ret_ty);
        let mut sym = Symbol::new(
            fn_name.to_string(),
            SymbolKind::Function,
            is_const,
            Some(fn_type),
            span,
        );
        sym.num_params = Some(params.iter().filter(|p| !p.variadic).count());
        sym.min_params = Some(required_params(params, |t| self.parse_ty(t)));
        sym.is_vararg = params.iter().any(|p| p.variadic);
        sym.allow_unused = self.allows_rule(stmt, "unused");
        sym.deprecated = self.deprecated_reason(stmt);
        if is_global {
            let _ = self.scope_mgr.define_global(sym);
            self.known_globals.insert(fn_name.to_string());
        } else {
            let _ = self.scope_mgr.define(sym);
        }
    }

    fn analyze_stmt(&mut self, stmt: &Stmt) {
        self.enter_stmt(stmt);
        self.validate_stmt_attributes(stmt);
        let allow_unused = self.allows_rule(stmt, "unused");
        let allow_shadow = self.allows_rule(stmt, "shadow");
        let deprecated = self.deprecated_reason(stmt);
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
                    allow_unused,
                    allow_shadow,
                    deprecated,
                );
            }
            Stmt::LocalMany {
                names,
                is_const,
                is_global,
                initializers,
                ..
            } => {
                self.analyze_local_many(
                    names,
                    *is_const,
                    *is_global,
                    initializers,
                    allow_unused,
                    allow_shadow,
                );
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
                is_global,
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
                    if !*is_global && !allow_shadow {
                        self.check_shadowing(fn_name, span);
                    }

                    let fn_type = self.build_function_type(params, body, ret_ty.clone());
                    let mut sym = Symbol::new(
                        fn_name.clone(),
                        SymbolKind::Function,
                        *is_const,
                        Some(fn_type),
                        span,
                    );
                    sym.num_params = Some(params.iter().filter(|p| !p.variadic).count());
                    sym.min_params = Some(required_params(params, |t| self.parse_ty(t)));
                    sym.is_vararg = params.iter().any(|p| p.variadic);
                    sym.allow_unused = allow_unused;
                    sym.deprecated = deprecated;
                    if *is_global {
                        let _ = self.scope_mgr.define_global(sym);
                        self.known_globals.insert(fn_name.clone());
                    } else {
                        let _ = self.scope_mgr.define(sym);
                    }
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
                self.analyze_if_stmt(data);
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
                        sym.inferred_type = if val_type != NeyukiType::Any
                            && matches!(non_nil, NeyukiType::Union(_))
                            && val_type.is_assignable_to(&non_nil)
                        {
                            val_type
                        } else if val_type != NeyukiType::Any && val_type.is_assignable_to(&non_nil)
                        {
                            non_nil
                        } else {
                            decl_type.clone()
                        };
                    } else {
                        sym.inferred_type = val_type;
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
        if let Some(sym) = self.scope_mgr.lookup_mut(name) {
            sym.used = true;
            if let Some(reason) = sym.deprecated.clone() {
                let decl_span = sym.span;
                let msg = if reason.is_empty() {
                    format!("use of deprecated '{}'", name)
                } else {
                    format!("use of deprecated '{}': {}", name, reason)
                };
                let diag = Diagnostic::warning(msg)
                    .with_code(ErrorCode::W0008)
                    .with_label(span, "deprecated symbol referenced here")
                    .with_secondary_label(decl_span, "marked '@deprecated' here");
                self.diagnostics.push(diag);
            }
        } else if !self.is_known_global(name) {
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

    pub(super) fn analyze_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Variable { name, .. } => self.read_variable(name),
            Expr::Call { callee, args, .. } => {
                self.analyze_expr(callee);
                for a in args {
                    self.analyze_expr(a);
                }
                self.check_call_site(callee, args);
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
            Expr::Unary { expr, .. } | Expr::Paren { expr, .. } => {
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

    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn analyze_local(
        &mut self,
        name: &str,
        is_const: bool,
        is_global: bool,
        type_name: Option<&str>,
        initializer: Option<&Expr>,
        allow_unused: bool,
        allow_shadow: bool,
        deprecated: Option<String>,
    ) {
        let span = self.find_ident_span(name);
        if !is_global && !allow_shadow {
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
        sym.allow_unused = allow_unused;
        sym.deprecated = deprecated;
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
        allow_unused: bool,
        allow_shadow: bool,
    ) {
        let inferred_inits: Vec<NeyukiType> = initializers
            .iter()
            .map(|e| self.infer_expr_type(e))
            .collect();
        for (i, name) in names.iter().enumerate() {
            let span = self.find_ident_span(name);
            if !is_global && !allow_shadow {
                self.check_shadowing(name, span);
            }

            let mut sym = Symbol::new(name.clone(), SymbolKind::Variable, is_const, None, span);
            sym.allow_unused = allow_unused;
            if let Some(init_ty) = resolve_local_many_slot(&inferred_inits, i) {
                sym.inferred_type = init_ty;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_globals_sorted_and_recognized() {
        assert!(
            BUILTIN_GLOBALS.windows(2).all(|w| w[0] < w[1]),
            "BUILTIN_GLOBALS must be strictly sorted for binary_search"
        );
        let analyzer = SemanticAnalyzer::new("");
        for &name in BUILTIN_GLOBALS {
            assert!(
                analyzer.is_known_global(name),
                "expected builtin global '{name}' to be recognized"
            );
        }
        assert!(analyzer.is_known_global("__custom_internal"));
        assert!(!analyzer.is_known_global("definitely_not_a_global"));
    }
}
