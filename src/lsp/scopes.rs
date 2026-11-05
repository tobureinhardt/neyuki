//! Names and their scopes within one document, and the layout of its code:
//! references, rename, highlights and folding ranges.
//!
//! The scopes come from the parser's syntax tree, so shadowing, loops and
//! nested functions resolve the way the compiler resolves them. The tree
//! does not keep the position of every name (assignment targets, parameters
//! and the names a declaration introduces are only known by the statement
//! they belong to), so those are found in the text next to the statement. A
//! name whose uses cannot all be placed is not offered for rename.

use std::collections::HashMap;

use crate::ast::NodeId;
use crate::ast::expr::{Expr, InterpPart, Param};
use crate::ast::pattern::AssignTarget;
use crate::ast::span::SpanPool;
use crate::ast::stmt::Stmt;
use crate::ast::visitor::{AstVisitor, walk_expr, walk_stmt};
use crate::lsp::analysis::{KEYWORDS, is_word_byte, mask_text};

/// One place a name is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Occurrence {
    /// The byte offset of the name.
    pub start: usize,
    /// A declaration or an assignment, rather than a read.
    pub write: bool,
    pub declaration: bool,
}

/// A name and everything that refers to the same declaration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    /// In document order.
    pub occurrences: Vec<Occurrence>,
    /// Whether every use was placed and the name is a declared one, so that
    /// renaming it is safe.
    pub renamable: bool,
}

struct Decl {
    name: String,
    uses: Vec<Occurrence>,
    incomplete: bool,
    /// A global function or variable, which code above it can call.
    hoisted: bool,
}

struct Free {
    name: String,
    at: usize,
    write: bool,
}

/// A name used inside an interpolated string, whose position the syntax tree
/// does not keep.
struct InterpUse {
    decl: Option<usize>,
    name: String,
    start: usize,
    end: usize,
}

enum Located {
    Found(usize),
    /// The syntax tree has no position for the node (it was made up by the
    /// parser, or is inside an interpolated string).
    NoSpan,
    /// The position does not hold the name.
    Mismatch,
}

/// The names of a document, resolved.
pub struct Analysis {
    decls: Vec<Decl>,
    free: Vec<Free>,
}

struct Resolver<'a> {
    text: &'a str,
    masked: String,
    starts: Vec<usize>,
    spans: &'a SpanPool,
    scopes: Vec<HashMap<String, usize>>,
    decls: Vec<Decl>,
    free: Vec<Free>,
    interps: Vec<InterpUse>,
    interp: Option<(usize, usize)>,
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// The line and the UTF-16 column of a byte offset.
pub fn position_at(text: &str, offset: usize) -> (usize, usize) {
    let head = text.get(..offset).unwrap_or(text);
    let line = head.matches('\n').count();
    let line_start = head.rfind('\n').map_or(0, |at| at + 1);
    (line, head[line_start..].encode_utf16().count())
}

/// Resolves the names of `text`, or `None` when it does not parse.
pub fn analyze(text: &str) -> Option<Analysis> {
    let parsed = crate::compiler::parse(text).ok()?;
    let mut resolver = Resolver {
        text,
        masked: mask_text(text),
        starts: line_starts(text),
        spans: &parsed.spans,
        scopes: vec![HashMap::new()],
        decls: Vec::new(),
        free: Vec::new(),
        interps: Vec::new(),
        interp: None,
    };
    resolver.stmts(&parsed.stmts);
    Some(resolver.finish())
}

impl Analysis {
    /// The name the cursor is on, with every place it is written.
    pub fn symbol_at(&self, offset: usize) -> Option<Symbol> {
        let touches = |start: usize, name: &str| start <= offset && offset <= start + name.len();
        for decl in &self.decls {
            if decl.uses.iter().any(|u| touches(u.start, &decl.name)) {
                return Some(Symbol {
                    name: decl.name.clone(),
                    occurrences: decl.uses.clone(),
                    renamable: !decl.incomplete && !KEYWORDS.contains(&decl.name.as_str()),
                });
            }
        }
        let name = self
            .free
            .iter()
            .find(|free| touches(free.at, &free.name))
            .map(|free| free.name.clone())?;
        let mut occurrences: Vec<Occurrence> = self
            .free
            .iter()
            .filter(|free| free.name == name)
            .map(|free| Occurrence {
                start: free.at,
                write: free.write,
                declaration: false,
            })
            .collect();
        occurrences.sort_by_key(|o| o.start);
        occurrences.dedup_by_key(|o| o.start);
        Some(Symbol {
            name,
            occurrences,
            renamable: false,
        })
    }
}

impl Resolver<'_> {
    fn offset(&self, line: u32, column: u32) -> Option<usize> {
        let index = (line as usize).checked_sub(1)?;
        let start = *self.starts.get(index)?;
        let end = self
            .starts
            .get(index + 1)
            .copied()
            .unwrap_or(self.text.len());
        let line_text = self.text.get(start..end)?;
        let skip = (column as usize).checked_sub(1)?;
        let within = line_text
            .char_indices()
            .nth(skip)
            .map_or(line_text.len(), |(at, _)| at);
        Some(start + within)
    }

    fn start_of(&self, id: NodeId) -> Option<usize> {
        let span = self.spans.get(id)?;
        self.offset(span.start.line, span.start.column)
    }

    fn end_of(&self, id: NodeId) -> Option<usize> {
        let span = self.spans.get(id)?;
        self.offset(span.end.line, span.end.column)
    }

    /// Whether `name` is written as a whole word at byte `at`.
    fn word_at(&self, at: usize, name: &str) -> bool {
        let bytes = self.masked.as_bytes();
        self.masked
            .get(at..)
            .is_some_and(|rest| rest.starts_with(name))
            && !bytes
                .get(at + name.len())
                .copied()
                .is_some_and(is_word_byte)
            && (at == 0 || !is_word_byte(bytes[at - 1]))
    }

    /// The first whole-word `name` at or after `from`, in code (not in
    /// strings or comments).
    fn find_word(&self, from: usize, name: &str) -> Option<usize> {
        let rest = self.masked.get(from..)?;
        rest.match_indices(name)
            .map(|(at, _)| from + at)
            .find(|&at| self.word_at(at, name))
    }

    fn locate(&self, id: NodeId, name: &str) -> Located {
        let Some(span) = self.spans.get(id) else {
            return Located::NoSpan;
        };
        match self.offset(span.start.line, span.start.column) {
            Some(at) if self.word_at(at, name) => Located::Found(at),
            _ => Located::Mismatch,
        }
    }

    fn located(&self, at: Option<usize>, name: &str) -> Located {
        match at {
            Some(at) if self.word_at(at, name) => Located::Found(at),
            _ => Located::Mismatch,
        }
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn declare(&mut self, name: &str, at: Option<usize>, hoisted: bool, root: bool) -> usize {
        let index = self.decls.len();
        let mut decl = Decl {
            name: name.to_string(),
            uses: Vec::new(),
            incomplete: at.is_none(),
            hoisted,
        };
        if let Some(at) = at {
            decl.uses.push(Occurrence {
                start: at,
                write: true,
                declaration: true,
            });
        }
        self.decls.push(decl);
        let scope = if root { 0 } else { self.scopes.len() - 1 };
        self.scopes[scope].insert(name.to_string(), index);
        index
    }

    fn use_name(&mut self, name: &str, located: Located, write: bool) {
        let decl = self.lookup(name);
        match (decl, located) {
            (Some(index), Located::Found(at)) => self.decls[index].uses.push(Occurrence {
                start: at,
                write,
                declaration: false,
            }),
            (Some(index), Located::Mismatch) => self.decls[index].incomplete = true,
            (None, Located::Found(at)) => self.free.push(Free {
                name: name.to_string(),
                at,
                write,
            }),
            (decl, Located::NoSpan) => {
                if let Some((start, end)) = self.interp {
                    self.interps.push(InterpUse {
                        decl,
                        name: name.to_string(),
                        start,
                        end,
                    });
                }
            }
            (None, Located::Mismatch) => {}
        }
    }

    /// Where each parameter is written: the text of the parameter list is
    /// split at the commas outside brackets.
    fn param_positions(&self, start: usize, params: &[Param]) -> Vec<Option<usize>> {
        let mut found = vec![None; params.len()];
        let Some(open) = self
            .masked
            .get(start..)
            .and_then(|rest| rest.find('('))
            .map(|at| start + at)
        else {
            return found;
        };
        let bytes = self.masked.as_bytes();
        let mut depth = 0usize;
        let mut index = 0;
        let mut segment = open + 1;
        let mut i = open + 1;
        while i <= bytes.len() && index < params.len() {
            let byte = if i == bytes.len() { b')' } else { bytes[i] };
            match byte {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' if depth > 0 => depth -= 1,
                b',' | b')' if depth == 0 => {
                    let name = params[index].name.as_str();
                    found[index] = self
                        .masked
                        .get(segment..i)
                        .and_then(|part| {
                            if name == "..." {
                                part.find("...")
                            } else {
                                part.match_indices(name)
                                    .map(|(at, _)| at)
                                    .find(|&at| self.word_at(segment + at, name))
                            }
                        })
                        .map(|at| segment + at);
                    index += 1;
                    segment = i + 1;
                    if byte == b')' {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        found
    }

    /// Declares the parameters of a function that starts at `start` in a new
    /// scope, which the caller pops.
    fn enter_function(&mut self, start: Option<usize>, params: &[Param], is_method: bool) {
        let positions = match start {
            // The receiver of a method is not in the parameter list.
            Some(start) if is_method && !params.is_empty() => {
                let mut positions = vec![None];
                positions.extend(self.param_positions(start, &params[1..]));
                positions
            }
            Some(start) => self.param_positions(start, params),
            None => vec![None; params.len()],
        };
        self.scopes.push(HashMap::new());
        for (index, (param, at)) in params.iter().zip(positions).enumerate() {
            if param.variadic {
                continue;
            }
            // The `self` of `function a:b()` is not written anywhere.
            let at = if is_method && index == 0 { None } else { at };
            self.declare(&param.name, at, false, false);
        }
    }

    fn stmts(&mut self, list: &[Stmt]) {
        for stmt in list {
            self.stmt(stmt);
        }
    }

    fn block(&mut self, list: &[Stmt]) {
        self.scopes.push(HashMap::new());
        self.stmts(list);
        self.scopes.pop();
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Local {
                name,
                is_global,
                initializer,
                id,
                ..
            } => {
                if let Some(init) = initializer {
                    self.expr(init);
                }
                // A binding with no position is one `--!importall` added.
                if let Some(start) = self.start_of(*id) {
                    let at = self.find_word(start, name);
                    self.declare(name, at, *is_global, *is_global);
                }
            }
            Stmt::LocalMany {
                names,
                initializers,
                id,
                ..
            } => {
                for init in initializers {
                    self.expr(init);
                }
                if let Some(start) = self.start_of(*id) {
                    let mut cursor = start;
                    for name in names {
                        let at = self.find_word(cursor, name);
                        if let Some(at) = at {
                            cursor = at + name.len();
                        }
                        self.declare(name, at, false, false);
                    }
                }
            }
            Stmt::Assign {
                target, value, id, ..
            } => {
                self.assign_target(target, self.start_of(*id), true);
                self.expr(value);
            }
            Stmt::AssignMany {
                targets,
                values,
                id,
                ..
            } => {
                let start = self.start_of(*id);
                for (index, target) in targets.iter().enumerate() {
                    // Only the first target is known to start the statement.
                    self.assign_target(target, if index == 0 { start } else { None }, index == 0);
                }
                for value in values {
                    self.expr(value);
                }
            }
            Stmt::Increment { target, id, .. } => {
                self.assign_target(target, self.start_of(*id), true);
            }
            Stmt::Function {
                name,
                params,
                body,
                id,
                ..
            } => {
                let start = self.start_of(*id);
                let mut is_method = false;
                if let Some(start) = start {
                    // `function a:b(` is a method.
                    let open = self.masked[start..]
                        .find('(')
                        .map_or(start, |at| start + at);
                    is_method = self.masked[start..open].contains(':');
                }
                if let (Some(full), Some(start)) = (name, start) {
                    let first = full.split('.').next().unwrap_or(full);
                    let at = self.find_word(start, first);
                    let is_local = at.is_some_and(|at| self.masked[start..at].contains("local"));
                    if full.contains('.') {
                        let located = self.located(at, first);
                        self.use_name(first, located, false);
                    } else if !is_local && self.lookup(first).is_some() {
                        let located = self.located(at, first);
                        self.use_name(first, located, true);
                    } else {
                        // Declared before the body so that it can call itself.
                        let root = self.scopes.len() == 1 && !is_local;
                        self.declare(first, at, root, false);
                    }
                }
                self.enter_function(start, params, is_method);
                self.stmts(body);
                self.scopes.pop();
            }
            Stmt::If { data, .. } => {
                self.expr(&data.condition);
                self.block(&data.then_branch);
                for (condition, branch) in &data.else_if_branches {
                    self.expr(condition);
                    self.block(branch);
                }
                if let Some(branch) = &data.else_branch {
                    self.block(branch);
                }
            }
            Stmt::For {
                vars,
                source,
                body,
                id,
            } => {
                self.expr(source);
                let start = self.start_of(*id);
                self.scopes.push(HashMap::new());
                // The names follow the `for`.
                let mut cursor = start.map(|s| s + "for".len());
                for var in vars {
                    let at = cursor.and_then(|from| self.find_word(from, var));
                    if let Some(at) = at {
                        cursor = Some(at + var.len());
                    }
                    self.declare(var, at, false, false);
                }
                self.stmts(body);
                self.scopes.pop();
            }
            Stmt::NumericFor { data, id } => {
                self.expr(&data.start);
                self.expr(&data.end);
                if let Some(step) = &data.step {
                    self.expr(step);
                }
                let at = self
                    .start_of(*id)
                    .and_then(|start| self.find_word(start + "for".len(), &data.var));
                self.scopes.push(HashMap::new());
                self.declare(&data.var, at, false, false);
                self.stmts(&data.body);
                self.scopes.pop();
            }
            Stmt::While {
                condition, body, ..
            } => {
                self.expr(condition);
                self.block(body);
            }
            Stmt::Repeat {
                body, condition, ..
            } => {
                // The condition sees the locals of the body.
                self.scopes.push(HashMap::new());
                self.stmts(body);
                self.expr(condition);
                self.scopes.pop();
            }
            Stmt::Return { values, .. } => {
                for value in values {
                    self.expr(value);
                }
            }
            Stmt::Expr { expr, .. } => self.expr(expr),
            Stmt::Break { .. }
            | Stmt::Continue { .. }
            | Stmt::Goto { .. }
            | Stmt::Label { .. }
            | Stmt::TypeAlias { .. } => {}
        }
    }

    /// `at` is where the target starts, when that is known.
    fn assign_target(&mut self, target: &AssignTarget, at: Option<usize>, placed: bool) {
        match target {
            AssignTarget::Variable(name) => {
                let located = if placed {
                    self.located(at, name)
                } else {
                    Located::Mismatch
                };
                self.use_name(name, located, true);
            }
            AssignTarget::Member { object, .. } => self.expr(object),
            AssignTarget::Index { object, index } => {
                self.expr(object);
                self.expr(index);
            }
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Literal { .. } | Expr::Vararg { .. } => {}
            Expr::Variable { name, id } => {
                let located = self.locate(*id, name);
                self.use_name(name, located, false);
            }
            Expr::Interp { parts, id } => {
                let range = self.start_of(*id).zip(self.end_of(*id));
                let outer = self.interp;
                if range.is_some() {
                    self.interp = range;
                }
                for part in parts {
                    if let InterpPart::Expr(inner) = part {
                        self.expr(inner);
                    }
                }
                self.interp = outer;
            }
            Expr::Member { object, .. } => self.expr(object),
            Expr::Index { object, index, .. } => {
                self.expr(object);
                self.expr(index);
            }
            Expr::Call { callee, args, .. } => {
                self.expr(callee);
                for arg in args {
                    self.expr(arg);
                }
            }
            Expr::MethodCall { object, args, .. } => {
                self.expr(object);
                for arg in args {
                    self.expr(arg);
                }
            }
            Expr::Function { params, body, id } => {
                let start = self.start_of(*id);
                self.enter_function(start, params, false);
                self.stmts(body);
                self.scopes.pop();
            }
            Expr::Unary { expr, .. } => self.expr(expr),
            Expr::Binary { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            Expr::Table { entries, .. } => {
                for entry in entries {
                    if let Some(key) = &entry.computed {
                        self.expr(key);
                    }
                    self.expr(&entry.value);
                }
            }
        }
    }

    fn finish(mut self) -> Analysis {
        // Global functions can be called above their declaration, from the
        // body of another function.
        let hoisted: HashMap<String, usize> = self.scopes[0]
            .iter()
            .filter(|entry| self.decls[*entry.1].hoisted)
            .map(|(name, index)| (name.clone(), *index))
            .collect();
        let free = std::mem::take(&mut self.free);
        for item in free {
            match hoisted.get(&item.name) {
                Some(&index) => self.decls[index].uses.push(Occurrence {
                    start: item.at,
                    write: item.write,
                    declaration: false,
                }),
                None => self.free.push(item),
            }
        }

        // Names inside interpolated strings are found in the text of the
        // string, outside the literal parts and not after a `.` or `:`.
        let interps = std::mem::take(&mut self.interps);
        for used in interps {
            let Some(literal) = self.text.get(used.start..used.end) else {
                continue;
            };
            let masked = mask_text(literal);
            let bytes = masked.as_bytes();
            for (at, _) in masked.match_indices(used.name.as_str()) {
                let before = if at == 0 { None } else { Some(bytes[at - 1]) };
                let after = bytes.get(at + used.name.len()).copied();
                if before.is_some_and(|b| is_word_byte(b) || b == b'.' || b == b':')
                    || after.is_some_and(is_word_byte)
                {
                    continue;
                }
                let occurrence = Occurrence {
                    start: used.start + at,
                    write: false,
                    declaration: false,
                };
                match used.decl {
                    Some(index) => self.decls[index].uses.push(occurrence),
                    None => self.free.push(Free {
                        name: used.name.clone(),
                        at: occurrence.start,
                        write: false,
                    }),
                }
            }
        }

        for decl in &mut self.decls {
            decl.uses.sort_by_key(|o| o.start);
            decl.uses.dedup_by_key(|o| o.start);
        }
        Analysis {
            decls: self.decls,
            free: self.free,
        }
    }
}

/// A range of lines to fold, 0-based and inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    pub start_line: usize,
    pub end_line: usize,
    /// A block of comment lines rather than code.
    pub comment: bool,
}

struct FoldCollector<'a> {
    text: &'a str,
    masked: String,
    starts: Vec<usize>,
    spans: &'a SpanPool,
    folds: Vec<Fold>,
}

impl FoldCollector<'_> {
    fn offset(&self, line: u32, column: u32) -> Option<usize> {
        let index = (line as usize).checked_sub(1)?;
        let start = *self.starts.get(index)?;
        let end = self
            .starts
            .get(index + 1)
            .copied()
            .unwrap_or(self.text.len());
        let line_text = self.text.get(start..end)?;
        let skip = (column as usize).checked_sub(1)?;
        let within = line_text
            .char_indices()
            .nth(skip)
            .map_or(line_text.len(), |(at, _)| at);
        Some(start + within)
    }

    fn line_of(&self, offset: usize) -> usize {
        self.starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1)
    }

    /// Folds a node that starts at its span and ends with `closer` (`end` or
    /// `}`): the closing line stays visible.
    fn fold(&mut self, id: NodeId, closer: &str) {
        let Some(span) = self.spans.get(id) else {
            return;
        };
        let (Some(start), Some(stop)) = (
            self.offset(span.start.line, span.start.column),
            self.offset(span.end.line, span.end.column),
        ) else {
            return;
        };
        // The span ends where the next token starts; the closer is the last
        // word before it.
        let Some(head) = self.masked.get(..stop.min(self.masked.len())) else {
            return;
        };
        let head = head.trim_end();
        let Some(before) = head.strip_suffix(closer) else {
            return;
        };
        if closer.as_bytes()[0].is_ascii_alphabetic()
            && before.bytes().next_back().is_some_and(is_word_byte)
        {
            return;
        }
        let first = self.line_of(start);
        let last = self.line_of(before.len());
        if last > first + 1 {
            self.folds.push(Fold {
                start_line: first,
                end_line: last - 1,
                comment: false,
            });
        }
    }
}

impl AstVisitor for FoldCollector<'_> {
    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let Stmt::Function { id, .. }
        | Stmt::If { id, .. }
        | Stmt::For { id, .. }
        | Stmt::NumericFor { id, .. }
        | Stmt::While { id, .. } = stmt
        {
            self.fold(*id, "end");
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Function { id, .. } => self.fold(*id, "end"),
            Expr::Table { id, .. } => self.fold(*id, "}"),
            _ => {}
        }
        walk_expr(self, expr);
    }
}

/// The folding ranges of a document: its blocks and multi-line tables (when
/// it parses) and runs of comment lines.
pub fn folding_ranges(text: &str) -> Vec<Fold> {
    let mut folds = Vec::new();
    if let Ok(parsed) = crate::compiler::parse(text) {
        let mut collector = FoldCollector {
            text,
            masked: mask_text(text),
            starts: line_starts(text),
            spans: &parsed.spans,
            folds: Vec::new(),
        };
        for stmt in &parsed.stmts {
            collector.visit_stmt(stmt);
        }
        folds = collector.folds;
    }

    // Two or more `--` lines in a row, other than directives.
    let mut run: Option<usize> = None;
    let lines: Vec<&str> = text.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let comment = trimmed.starts_with("--") && !trimmed.starts_with("--!");
        match (comment, run) {
            (true, None) => run = Some(index),
            (false, Some(first)) => {
                if index - 1 > first {
                    folds.push(Fold {
                        start_line: first,
                        end_line: index - 1,
                        comment: true,
                    });
                }
                run = None;
            }
            _ => {}
        }
    }
    if let Some(first) = run
        && lines.len() - 1 > first
    {
        folds.push(Fold {
            start_line: first,
            end_line: lines.len() - 1,
            comment: true,
        });
    }

    folds.sort_by_key(|fold| (fold.start_line, fold.end_line));
    folds.dedup();
    folds
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The offset of the `n`th (0-based) whole-word `name` in `text`.
    fn at(text: &str, name: &str, n: usize) -> usize {
        text.match_indices(name)
            .map(|(at, _)| at)
            .filter(|&at| {
                !text[..at].bytes().next_back().is_some_and(is_word_byte)
                    && !text[at + name.len()..]
                        .bytes()
                        .next()
                        .is_some_and(is_word_byte)
            })
            .nth(n)
            .expect("name is in the text")
    }

    fn symbol(text: &str, name: &str, n: usize) -> Symbol {
        let analysis = analyze(text).expect("parses");
        analysis
            .symbol_at(at(text, name, n))
            .expect("a symbol is there")
    }

    fn starts(symbol: &Symbol) -> Vec<usize> {
        symbol.occurrences.iter().map(|o| o.start).collect()
    }

    #[test]
    fn test_references_follow_shadowing() {
        let text = "local x = 1\nlocal function f(x)\n    return x + 1\nend\nprint(x)\n";
        // The outer `x`: its declaration and the use after the function.
        let outer = symbol(text, "x", 0);
        assert_eq!(starts(&outer), vec![at(text, "x", 0), at(text, "x", 3)]);
        assert!(outer.renamable);
        // The parameter and its use are another name.
        let inner = symbol(text, "x", 2);
        assert_eq!(starts(&inner), vec![at(text, "x", 1), at(text, "x", 2)]);
        assert!(inner.occurrences[0].declaration);
        assert!(!inner.occurrences[1].write);
    }

    #[test]
    fn test_references_of_loops_blocks_and_assignments() {
        let text = "local total = 0\nfor i = 1, 3 do\n    total = total + i\nend\nfor _, v in {1, 2} do\n    total += v\nend\nif total > 1 then\n    local total = 5\n    print(total)\nend\nprint(total)\n";
        let outer = symbol(text, "total", 0);
        // Declaration, `total =`, `+ total`, `total +=`, `if total`, the last print.
        assert_eq!(outer.occurrences.len(), 6, "{:?}", outer.occurrences);
        assert!(outer.occurrences[1].write);
        assert!(!outer.occurrences[2].write);
        let shadow = symbol(text, "total", 6);
        assert_eq!(shadow.occurrences.len(), 2);
        let var = symbol(text, "i", 0);
        assert_eq!(var.occurrences.len(), 2);
    }

    #[test]
    fn test_references_in_interpolated_strings_and_forward_calls() {
        let text = "local n = 1\nprint(`n is {n} {n + 1}`)\nfunction a()\n    b()\nend\nfunction b()\nend\n";
        let n = symbol(text, "n", 0);
        // The declaration and the two uses in the template, not the word in
        // its literal text.
        assert_eq!(n.occurrences.len(), 3, "{:?}", n.occurrences);
        assert!(n.renamable);
        // `b` is called from `a` above its declaration.
        let b = symbol(text, "b", 1);
        assert_eq!(b.occurrences.len(), 2);
        assert!(b.renamable);
    }

    #[test]
    fn test_free_names_and_keywords_are_not_renamable() {
        let text = "print(1)\nprint(2)\nlocal s = 1\n";
        let print = symbol(text, "print", 0);
        assert!(!print.renamable);
        assert_eq!(print.occurrences.len(), 2);
        // The cursor on a keyword or a field is no symbol at all.
        let analysis = analyze("local t = {}\nt.field = 1\nif t then end\n").unwrap();
        let text = "local t = {}\nt.field = 1\nif t then end\n";
        assert!(analysis.symbol_at(at(text, "field", 0)).is_none());
        assert!(analysis.symbol_at(at(text, "then", 0)).is_none());
        // Text that does not parse has no analysis.
        assert!(analyze("local = = =").is_none());
    }

    #[test]
    fn test_methods_and_parameters() {
        let text = "local obj = {}\nfunction obj:greet(name: string, ...)\n    return self.x .. name\nend\nobj:greet(\"a\")\n";
        let name = symbol(text, "name", 0);
        assert_eq!(name.occurrences.len(), 2);
        assert!(name.renamable);
        // `self` is declared by the colon and is not written there.
        let this = symbol(text, "self", 0);
        assert!(!this.renamable);
        let obj = symbol(text, "obj", 0);
        assert_eq!(obj.occurrences.len(), 3);
    }

    #[test]
    fn test_position_at_counts_utf16() {
        let text = "a\n\u{1f980}b";
        assert_eq!(position_at(text, 0), (0, 0));
        assert_eq!(position_at(text, 2), (1, 0));
        assert_eq!(position_at(text, text.len()), (1, 3));
    }

    #[test]
    fn test_folding_ranges_of_blocks_tables_and_comments() {
        let text = "-- one\n-- two\nlocal function f()\n    local t = {\n        a = 1,\n        b = 2,\n    }\n    if t then\n        print(1)\n    end\nend\nprint(f)\n";
        let folds = folding_ranges(text);
        // The comment lines, the function without its `end`, the table and
        // the `if`.
        assert!(folds.contains(&Fold {
            start_line: 0,
            end_line: 1,
            comment: true
        }));
        assert!(folds.contains(&Fold {
            start_line: 2,
            end_line: 9,
            comment: false
        }));
        assert!(folds.contains(&Fold {
            start_line: 3,
            end_line: 5,
            comment: false
        }));
        assert!(folds.contains(&Fold {
            start_line: 7,
            end_line: 8,
            comment: false
        }));
        // A block of one line folds nothing, and broken code still folds
        // its comments.
        assert!(folding_ranges("if x then end\n").is_empty());
        let broken = folding_ranges("-- a\n-- b\nlocal = =\n");
        assert_eq!(broken.len(), 1);
    }
}
