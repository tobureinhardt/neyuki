//! What the language server knows about the other files of a project.
//!
//! A `require` string is resolved the way the runtime resolves it (the
//! searcher patterns and relative paths of `module_loader`), the file is read
//! and parsed, and the shape of what it returns is worked out from the syntax
//! tree: the members of the table it exports, with parameter and return
//! annotations and the comment block above each, and the types it declares.
//! Nothing is ever run, and nothing that cannot be worked out statically is
//! guessed: it is simply absent.
//!
//! `ProjectSource` is a `MemberSource` that answers for these modules and
//! hands everything else to the standard library reference.

use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use crate::ast::expr::{Expr, TableEntry};
use crate::ast::literal::Literal;
use crate::ast::pattern::AssignTarget;
use crate::ast::span::SpanPool;
use crate::ast::stmt::Stmt;
use crate::ast::ty::TypeExpr;
use crate::lsp::analysis::is_word_byte;
use crate::lsp::api::{Definition, Entry, EntryKind, Library, MemberSource, Param};
use crate::module_loader::{PathEngine, SearcherEngine};
use crate::pkg::PackageManifest;

/// Files larger than this are not read.
const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// How many `return require(...)` hops are followed.
const MAX_REEXPORT_DEPTH: usize = 4;
/// Bounds of the directory walk behind `require("` completion.
const MAX_WALK_ENTRIES: usize = 2000;
const MAX_WALK_DEPTH: usize = 6;
/// Directories the walk does not enter (hidden ones are skipped as well).
const SKIPPED_DIRS: &[&str] = &["target", "node_modules", "neyuki_packages"];
/// Entries remembered before the cache starts over.
const MAX_CACHED: usize = 1024;

/// Where a name is written: a 0-based line and UTF-16 columns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pos {
    pub line: usize,
    pub character: usize,
    pub length: usize,
}

/// One member of the table a module returns.
#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub name: String,
    /// `Function`, `Method` (written `function m:name`), `Value`, or
    /// `Namespace` for a table with members of its own.
    pub kind: EntryKind,
    /// Without the receiver of a method.
    pub params: Vec<Param>,
    /// The annotated result of a function or the type of a value; empty when
    /// the file does not say.
    pub returns: String,
    /// The `--` comment lines directly above the declaration.
    pub doc: String,
    pub pos: Option<Pos>,
    pub children: Vec<Member>,
}

/// A `type` or `export type` declaration.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeDecl {
    pub name: String,
    pub ty: String,
    pub exported: bool,
    pub doc: String,
    pub pos: Pos,
}

/// What a file exports.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shape {
    /// The name signatures are written with: the table's variable, or the
    /// file name.
    pub owner: String,
    pub members: Vec<Member>,
    pub types: Vec<TypeDecl>,
    /// The module this file returns as it is: `return require("other")`.
    pub reexport: Option<String>,
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

fn position_of(text: &str, starts: &[usize], offset: usize, length: usize) -> Pos {
    let line = starts
        .partition_point(|&start| start <= offset)
        .saturating_sub(1);
    let line_start = starts.get(line).copied().unwrap_or(0);
    let character = text
        .get(line_start..offset)
        .map_or(0, |before| before.encode_utf16().count());
    let length = text
        .get(offset..offset + length)
        .map_or(length, |name| name.encode_utf16().count());
    Pos {
        line,
        character,
        length,
    }
}

/// Reads the syntax tree of one file.
struct Extractor<'a> {
    text: &'a str,
    starts: Vec<usize>,
    spans: &'a SpanPool,
}

/// The shape of the module `text`, or `None` when it does not parse. `stem`
/// is the file name without its extension.
pub fn extract(text: &str, stem: &str) -> Option<Shape> {
    let parsed = crate::compiler::parse(text).ok()?;
    let extractor = Extractor {
        text,
        starts: line_starts(text),
        spans: &parsed.spans,
    };
    Some(extractor.shape(&parsed.stmts, stem))
}

impl Extractor<'_> {
    fn shape(&self, stmts: &[Stmt], stem: &str) -> Shape {
        let mut shape = Shape {
            owner: stem.to_string(),
            members: Vec::new(),
            types: self.types(stmts),
            reexport: None,
        };
        let last_return = stmts
            .iter()
            .rev()
            .find(|stmt| matches!(stmt, Stmt::Return { .. }));
        if let Some(Stmt::Return { values, .. }) = last_return
            && let Some(value) = values.first()
        {
            self.fill_from_value(&mut shape, stmts, value, 0);
        }
        shape
    }

    /// Works out the members from the expression the file returns.
    fn fill_from_value(&self, shape: &mut Shape, stmts: &[Stmt], value: &Expr, depth: usize) {
        match value {
            Expr::Variable { name, .. } => {
                if let Some(module) = Self::required_by_local(stmts, name) {
                    shape.reexport = Some(module);
                } else {
                    shape.owner = name.clone();
                    shape.members = self.table_members(stmts, name);
                }
            }
            Expr::Table { entries, .. } => {
                shape.members = self.entries_members(stmts, entries, 0);
            }
            Expr::Call { callee, args, .. } => {
                if let Some(module) = require_target(value) {
                    shape.reexport = Some(module);
                } else if let Expr::Variable { name, .. } = callee.as_ref()
                    && name == "setmetatable"
                    && depth < 2
                    && let Some(first) = args.first()
                {
                    self.fill_from_value(shape, stmts, first, depth + 1);
                }
            }
            _ => {}
        }
    }

    /// `local name = require("module")` at the top level.
    fn required_by_local(stmts: &[Stmt], variable: &str) -> Option<String> {
        stmts.iter().find_map(|stmt| match stmt {
            Stmt::Local {
                name,
                initializer: Some(init),
                ..
            } if name == variable => require_target(init),
            _ => None,
        })
    }

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

    fn start_of(&self, id: crate::ast::NodeId) -> Option<usize> {
        let span = self.spans.get(id)?;
        self.offset(span.start.line, span.start.column)
    }

    fn pos(&self, offset: usize, length: usize) -> Pos {
        position_of(self.text, &self.starts, offset, length)
    }

    /// The `--` comment lines directly above the line `offset` is on.
    fn docs_above(&self, offset: usize) -> String {
        let line = self
            .starts
            .partition_point(|&start| start <= offset)
            .saturating_sub(1);
        let mut docs: Vec<String> = Vec::new();
        let mut index = line;
        while index > 0 {
            index -= 1;
            let start = self.starts[index];
            let end = self
                .starts
                .get(index + 1)
                .copied()
                .unwrap_or(self.text.len());
            let Some(text) = self.text.get(start..end) else {
                break;
            };
            let Some(rest) = text.trim().strip_prefix("--") else {
                break;
            };
            // `--!importall` is a directive and `--[[` opens a block comment.
            if rest.starts_with('!') || rest.starts_with('[') {
                break;
            }
            docs.push(
                rest.strip_prefix(' ')
                    .unwrap_or(rest)
                    .trim_end()
                    .to_string(),
            );
        }
        docs.reverse();
        docs.join("\n").trim().to_string()
    }

    /// The offset of `name` on the line `start` is on, after a `.` or `:`
    /// when `after_separator`.
    fn find_name(&self, start: usize, name: &str, after_separator: bool) -> Option<usize> {
        let rest = self.text.get(start..)?;
        let line = rest.split('\n').next()?;
        let bytes = line.as_bytes();
        let mut from = 0;
        while let Some(found) = line[from..].find(name) {
            let at = from + found;
            let before = if at == 0 { None } else { Some(bytes[at - 1]) };
            let after = bytes.get(at + name.len()).copied();
            let separated = !after_separator || matches!(before, Some(b'.') | Some(b':'));
            let bounded = !before.is_some_and(is_word_byte) && !after.is_some_and(is_word_byte);
            if separated && bounded {
                return Some(start + at);
            }
            from = at + name.len();
        }
        None
    }

    /// The offset of the field `key` written shortly before `value`.
    fn find_key_before(&self, value: usize, key: &str) -> Option<usize> {
        let mut from = value.saturating_sub(200);
        while !self.text.is_char_boundary(from) {
            from += 1;
        }
        let head = self.text.get(from..value)?;
        let bytes = head.as_bytes();
        let mut end = head.len();
        while let Some(at) = head[..end].rfind(key) {
            let before_ok = at == 0 || !is_word_byte(bytes[at - 1]);
            let after_ok = !bytes.get(at + key.len()).copied().is_some_and(is_word_byte);
            if before_ok && after_ok {
                return Some(from + at);
            }
            end = at;
        }
        None
    }

    fn types(&self, stmts: &[Stmt]) -> Vec<TypeDecl> {
        stmts
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::TypeAlias {
                    name,
                    exported,
                    ty,
                    name_span,
                    ..
                } => {
                    let offset = self.offset(name_span.start.line, name_span.start.column)?;
                    Some(TypeDecl {
                        name: name.clone(),
                        ty: ty.clone(),
                        exported: *exported,
                        doc: self.docs_above(offset),
                        pos: self.pos(offset, name.len()),
                    })
                }
                _ => None,
            })
            .collect()
    }

    /// The members the file adds to the local table `var`: its constructor,
    /// `function var.name`, `function var:name` and `var.name = value`,
    /// at the top level.
    fn table_members(&self, stmts: &[Stmt], var: &str) -> Vec<Member> {
        let mut members: Vec<Member> = Vec::new();
        for stmt in stmts {
            match stmt {
                Stmt::Local {
                    name,
                    initializer: Some(Expr::Table { entries, .. }),
                    ..
                } if name == var => {
                    for member in self.entries_members(stmts, entries, 0) {
                        push_unique(&mut members, member);
                    }
                }
                Stmt::Function {
                    name: Some(full),
                    params,
                    return_type,
                    id,
                    ..
                } => {
                    let segments: Vec<&str> = full.split('.').collect();
                    if segments.len() < 2 || segments.len() > 3 || segments[0] != var {
                        continue;
                    }
                    let member_name = segments[segments.len() - 1];
                    let start = self.start_of(*id);
                    let at = start.and_then(|s| self.find_name(s, member_name, true));
                    let is_method = at
                        .and_then(|at| self.text.as_bytes().get(at.wrapping_sub(1)))
                        .is_some_and(|byte| *byte == b':');
                    let member = Member {
                        name: member_name.to_string(),
                        kind: if is_method {
                            EntryKind::Method
                        } else {
                            EntryKind::Function
                        },
                        params: params_of(params, is_method),
                        returns: return_type.as_deref().map(tidy_type).unwrap_or_default(),
                        doc: start.map(|s| self.docs_above(s)).unwrap_or_default(),
                        pos: at.map(|at| self.pos(at, member_name.len())),
                        children: Vec::new(),
                    };
                    if segments.len() == 3 {
                        add_nested(&mut members, segments[1], member);
                    } else {
                        push_unique(&mut members, member);
                    }
                }
                Stmt::Assign {
                    target: AssignTarget::Member { object, field },
                    value,
                    id,
                    ..
                } => {
                    let start = self.start_of(*id);
                    let at = start.and_then(|s| self.find_name(s, field, true));
                    let member = self.value_member(stmts, field, value, start, at, 0);
                    match object.as_ref() {
                        Expr::Variable { name, .. } if name == var => {
                            push_unique(&mut members, member);
                        }
                        Expr::Member {
                            object: inner,
                            field: outer,
                            ..
                        } if matches!(inner.as_ref(), Expr::Variable { name, .. } if name == var) =>
                        {
                            add_nested(&mut members, outer, member);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        members
    }

    /// The members written in a table constructor. `depth` is how many
    /// tables enclose it.
    fn entries_members(&self, stmts: &[Stmt], entries: &[TableEntry], depth: usize) -> Vec<Member> {
        let mut members: Vec<Member> = Vec::new();
        for entry in entries {
            let Some(key) = &entry.key else { continue };
            if entry.computed.is_some() {
                continue;
            }
            let value_start = self.start_of(entry.value.node_id());
            let key_at = value_start.and_then(|v| self.find_key_before(v, key));
            let doc_at = key_at.or(value_start);
            let member = self.value_member(stmts, key, &entry.value, doc_at, key_at, depth);
            push_unique(&mut members, member);
        }
        members
    }

    /// The member named `name` that holds `value`. `doc_at` is where its
    /// comment block ends and `name_at` where the name is written.
    fn value_member(
        &self,
        stmts: &[Stmt],
        name: &str,
        value: &Expr,
        doc_at: Option<usize>,
        name_at: Option<usize>,
        depth: usize,
    ) -> Member {
        let doc = doc_at.map(|at| self.docs_above(at)).unwrap_or_default();
        let pos = name_at.map(|at| self.pos(at, name.len()));
        let plain = |kind: EntryKind, returns: String| Member {
            name: name.to_string(),
            kind,
            params: Vec::new(),
            returns,
            doc: doc.clone(),
            pos,
            children: Vec::new(),
        };
        match value {
            Expr::Function { params, .. } => Member {
                params: params_of(params, false),
                ..plain(EntryKind::Function, String::new())
            },
            Expr::Table { entries, .. } if depth == 0 => Member {
                children: self.entries_members(stmts, entries, depth + 1),
                ..plain(EntryKind::Namespace, String::new())
            },
            Expr::Table { .. } => plain(EntryKind::Value, "table".to_string()),
            Expr::Variable { name: local, .. } => match self.local_function(stmts, local) {
                Some(function) => Member {
                    name: name.to_string(),
                    // The comment and the position of the function it names.
                    doc: if function.doc.is_empty() {
                        doc.clone()
                    } else {
                        function.doc.clone()
                    },
                    pos: function.pos.or(pos),
                    ..function
                },
                None => plain(EntryKind::Value, "any".to_string()),
            },
            Expr::Literal { value, .. } => plain(EntryKind::Value, literal_type(value).to_string()),
            _ => plain(EntryKind::Value, "any".to_string()),
        }
    }

    /// A function the file declares at its top level: `function name` or
    /// `local name = function`.
    fn local_function(&self, stmts: &[Stmt], name: &str) -> Option<Member> {
        stmts.iter().find_map(|stmt| match stmt {
            Stmt::Function {
                name: Some(function),
                params,
                return_type,
                id,
                ..
            } if function == name => {
                let start = self.start_of(*id);
                let at = start.and_then(|s| self.find_name(s, name, false));
                Some(Member {
                    name: name.to_string(),
                    kind: EntryKind::Function,
                    params: params_of(params, false),
                    returns: return_type.as_deref().map(tidy_type).unwrap_or_default(),
                    doc: start.map(|s| self.docs_above(s)).unwrap_or_default(),
                    pos: at.map(|at| self.pos(at, name.len())),
                    children: Vec::new(),
                })
            }
            Stmt::Local {
                name: local,
                initializer: Some(Expr::Function { params, .. }),
                id,
                ..
            } if local == name => {
                let start = self.start_of(*id);
                let at = start.and_then(|s| self.find_name(s, name, false));
                Some(Member {
                    name: name.to_string(),
                    kind: EntryKind::Function,
                    params: params_of(params, false),
                    returns: String::new(),
                    doc: start.map(|s| self.docs_above(s)).unwrap_or_default(),
                    pos: at.map(|at| self.pos(at, name.len())),
                    children: Vec::new(),
                })
            }
            _ => None,
        })
    }
}

/// `require("module")` as the string it loads.
fn require_target(expr: &Expr) -> Option<String> {
    let Expr::Call { callee, args, .. } = expr else {
        return None;
    };
    let Expr::Variable { name, .. } = callee.as_ref() else {
        return None;
    };
    if name != "require" {
        return None;
    }
    match args.first() {
        Some(Expr::Literal {
            value: Literal::String(module),
            ..
        }) => Some(module.clone()),
        _ => None,
    }
}

fn literal_type(value: &Literal) -> &'static str {
    match value {
        Literal::Nil => "nil",
        Literal::Bool(_) => "boolean",
        Literal::Int(_) => "int",
        Literal::Float(_) => "float",
        Literal::String(_) => "string",
    }
}

/// A type annotation as the parser keeps it (`boolean ?`, `{ a : number }`)
/// laid out the way it is written, as the parser does for type aliases.
fn tidy_type(ty: &str) -> String {
    let parts: Vec<&str> = ty.split_whitespace().collect();
    let mut out = String::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            let previous = parts[index - 1];
            let tight = matches!(*part, "," | ":" | ")" | "]" | "?" | ".")
                || matches!(previous, "(" | "[" | "." | "...")
                || (previous == "{" && *part == "}");
            if !tight {
                out.push(' ');
            }
        }
        out.push_str(part);
    }
    out
}

/// Parameters as the reference writes them. `skip_receiver` drops the
/// `self` the parser puts in front of a method's parameters.
fn params_of(params: &[crate::ast::expr::Param], skip_receiver: bool) -> Vec<Param> {
    params
        .iter()
        .skip(usize::from(skip_receiver))
        .map(|param| {
            let name = if param.variadic {
                "...".to_string()
            } else {
                param.name.clone()
            };
            let written = param.type_name.as_deref().map(tidy_type);
            let ty = written.clone().unwrap_or_else(|| "any".to_string());
            let optional = param.variadic || ty.ends_with('?');
            let label = match &written {
                Some(written) => format!("{name}: {written}"),
                None => name.clone(),
            };
            Param {
                name,
                ty,
                optional,
                label,
            }
        })
        .collect()
}

/// Adds a member unless one of that name exists: the first declaration wins.
fn push_unique(members: &mut Vec<Member>, member: Member) {
    if !members.iter().any(|existing| existing.name == member.name) {
        members.push(member);
    }
}

/// Adds `member` under the table member `parent`, creating the table member
/// when only its fields are declared.
fn add_nested(members: &mut Vec<Member>, parent: &str, member: Member) {
    if let Some(existing) = members.iter_mut().find(|m| m.name == parent) {
        if existing.kind == EntryKind::Value && existing.returns == "table" {
            existing.kind = EntryKind::Namespace;
        }
        if existing.kind == EntryKind::Namespace {
            push_unique(&mut existing.children, member);
        }
        return;
    }
    members.push(Member {
        name: parent.to_string(),
        kind: EntryKind::Namespace,
        params: Vec::new(),
        returns: String::new(),
        doc: String::new(),
        pos: None,
        children: vec![member],
    });
}

/// The member at `path` below `members`.
fn find_member<'a>(members: &'a [Member], path: &[&str]) -> Option<&'a Member> {
    let (first, rest) = path.split_first()?;
    let member = members.iter().find(|m| m.name == *first)?;
    if rest.is_empty() {
        Some(member)
    } else {
        find_member(&member.children, rest)
    }
}

/// The fields of a record type, looking through `?`, `T | nil` and the
/// aliases of `types`.
fn record_fields(
    ty: &TypeExpr,
    types: &[TypeDecl],
    depth: usize,
) -> Option<Vec<(String, TypeExpr)>> {
    if depth > 8 {
        return None;
    }
    match ty {
        TypeExpr::Record(fields) => Some(fields.clone()),
        TypeExpr::Optional(inner) => record_fields(inner, types, depth + 1),
        TypeExpr::Named(name) => {
            let decl = types.iter().find(|decl| decl.name == *name)?;
            record_fields(&TypeExpr::parse(&decl.ty), types, depth + 1)
        }
        TypeExpr::Union(variants) => {
            let mut rest = variants.iter().filter(|v| !matches!(v, TypeExpr::Nil));
            let only = rest.next()?;
            if rest.next().is_some() {
                return None;
            }
            record_fields(only, types, depth + 1)
        }
        _ => None,
    }
}

/// The fields of the type at `path` (`Options`, or `Options.inner` for the
/// type of a field) among `types`.
fn type_fields_at(types: &[TypeDecl], path: &[&str]) -> Option<Vec<(String, TypeExpr)>> {
    let (name, rest) = path.split_first()?;
    let decl = types.iter().find(|decl| decl.name == *name)?;
    let mut ty = TypeExpr::parse(&decl.ty);
    for segment in rest {
        let fields = record_fields(&ty, types, 0)?;
        ty = fields
            .into_iter()
            .find(|(field, _)| field.as_str() == *segment)?
            .1;
    }
    record_fields(&ty, types, 0)
}

fn field_entries(fields: Vec<(String, TypeExpr)>, namespace: &str) -> Vec<Entry> {
    fields
        .into_iter()
        .map(|(field, ty)| Entry {
            name: format!("{namespace}.{field}"),
            kind: EntryKind::Value,
            owner: None,
            params: Vec::new(),
            returns: ty.to_string(),
            signature: format!("{field}: {ty}"),
            doc: String::new(),
            page: String::new(),
        })
        .collect()
}

/// `prefix.Name` for every name of `names` that `ty` mentions on its own
/// (not as the member of another name).
fn qualify_names(ty: &str, names: &[&str], prefix: &str) -> String {
    let bytes = ty.as_bytes();
    let mut out = String::with_capacity(ty.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        let starts_word = (byte.is_ascii_alphabetic() || byte == b'_')
            && (i == 0 || !(is_word_byte(bytes[i - 1]) || bytes[i - 1] == b'.'));
        if starts_word {
            let mut end = i;
            while end < bytes.len() && is_word_byte(bytes[end]) {
                end += 1;
            }
            let word = &ty[i..end];
            if names.contains(&word) && bytes.get(end) != Some(&b'.') {
                out.push_str(prefix);
                out.push('.');
            }
            out.push_str(word);
            i = end;
        } else {
            let length = ty[i..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&ty[i..i + length]);
            i += length;
        }
    }
    out
}

fn member_entry(member: &Member, full_name: String, display_prefix: &str) -> Entry {
    let labels: Vec<&str> = member.params.iter().map(|p| p.label.as_str()).collect();
    let result = if member.returns.is_empty() {
        String::new()
    } else {
        format!(": {}", member.returns)
    };
    let signature = match member.kind {
        EntryKind::Function => format!(
            "{display_prefix}.{}({}){result}",
            member.name,
            labels.join(", ")
        ),
        EntryKind::Method => format!(
            "{display_prefix}:{}({}){result}",
            member.name,
            labels.join(", ")
        ),
        EntryKind::Value => format!("{display_prefix}.{}{result}", member.name),
        EntryKind::Namespace => format!("{display_prefix}.{}", member.name),
    };
    Entry {
        name: full_name,
        kind: member.kind,
        owner: None,
        params: member.params.clone(),
        returns: member.returns.clone(),
        signature,
        doc: member.doc.clone(),
        page: String::new(),
    }
}

fn type_entry(decl: &TypeDecl, full_name: String) -> Entry {
    Entry {
        name: full_name,
        kind: EntryKind::Value,
        owner: None,
        params: Vec::new(),
        returns: decl.ty.clone(),
        signature: format!(
            "{}type {} = {}",
            if decl.exported { "export " } else { "" },
            decl.name,
            decl.ty
        ),
        doc: decl.doc.clone(),
        page: String::new(),
    }
}

// ---------------------------------------------------------------- paths

/// The path as the key files are remembered by: no `.` components, and
/// forward slashes and lower case on Windows.
pub fn path_key(path: &Path) -> String {
    let normal: PathBuf = path
        .components()
        .filter(|component| !matches!(component, Component::CurDir))
        .collect();
    let text = normal.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The file a `file:` URI names.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Skip the host: empty, or `localhost`.
    let rest = &rest[rest.find('/')?..];
    let decoded = percent_decode(rest)?;
    let bytes = decoded.as_bytes();
    // `/C:/dir` is `C:/dir` on Windows.
    if cfg!(windows) && bytes.len() >= 3 && bytes[0] == b'/' && bytes[2] == b':' {
        return Some(PathBuf::from(&decoded[1..]));
    }
    Some(PathBuf::from(decoded))
}

/// The `file:` URI of a path.
pub fn path_to_uri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !text.starts_with('/') {
        out.push('/');
    }
    for &byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~' | b':') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The key a document is remembered by: its path, so that it and the file on
/// disk are one module, or its URI when it is not a file.
pub fn document_key(uri: &str) -> String {
    match uri_to_path(uri) {
        Some(path) => path_key(&path),
        None => uri.to_string(),
    }
}

/// The nearest directory at or above `dir` that holds a `neyuki.toml`.
pub fn project_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|candidate| candidate.join("neyuki.toml").is_file())
        .map(Path::to_path_buf)
}

/// Whether `require` accepts the string as a path at all: the same refusals
/// as the runtime (parent directories, absolute paths, null bytes), and the
/// `@` names, which are not files.
fn is_plain_module_path(module: &str) -> bool {
    !module.is_empty()
        && !module.contains('\0')
        && !module.contains("..")
        && !module.starts_with('@')
        && !module.starts_with('/')
        && !module.starts_with('\\')
        && !Path::new(module).components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

/// The file `require(module)` loads from `requester`, searched like the
/// runtime does: next to the requesting file, then from the root of the
/// project (where `neyuki_packages` lives). The working directory, which the
/// runtime falls back to, is not searched. Only source files are returned.
pub fn resolve_module_file(requester: &Path, module: &str) -> Option<PathBuf> {
    if !is_plain_module_path(module) {
        return None;
    }
    let dir = requester.parent()?;
    let mut bases = vec![dir.to_path_buf()];
    if let Some(root) = project_root(dir)
        && root != dir
    {
        bases.push(root);
    }
    let searcher = SearcherEngine::new();
    for base in bases {
        let found = searcher
            .search(module, Some(base.as_path()))
            .or_else(|| PathEngine::resolve_candidate(module, Some(base.as_path())).ok());
        if let Some(path) = found
            && path.extension().is_some_and(|ext| ext == "nyk")
        {
            return Some(path);
        }
    }
    None
}

/// The packages the nearest `neyuki.toml` depends on.
pub fn dependency_names(requester: &Path) -> Vec<String> {
    let Some(dir) = requester.parent() else {
        return Vec::new();
    };
    let Some(root) = project_root(dir) else {
        return Vec::new();
    };
    let manifest = root.join("neyuki.toml");
    let readable = std::fs::metadata(&manifest).is_ok_and(|meta| meta.len() <= MAX_FILE_BYTES);
    let Some(text) = readable
        .then(|| std::fs::read_to_string(&manifest).ok())
        .flatten()
    else {
        return Vec::new();
    };
    match PackageManifest::parse(&text) {
        Ok(manifest) => manifest
            .dependencies
            .keys()
            .chain(manifest.dev_dependencies.keys())
            .cloned()
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// The name `require` takes for a file below `root`.
fn module_name(root: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(root).ok()?;
    let mut parts: Vec<String> = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    let last = parts.pop()?;
    let stem = last.strip_suffix(".nyk")?.to_string();
    // `dir/init.nyk` and `dir/mod.nyk` are the module `dir`.
    if matches!(stem.as_str(), "init" | "mod") && !parts.is_empty() {
        return Some(parts.join("/"));
    }
    parts.push(stem);
    Some(parts.join("/"))
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    budget: &mut usize,
    found: &mut Vec<(String, PathBuf)>,
) {
    if depth > MAX_WALK_DEPTH {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<std::fs::DirEntry> = read.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if name.starts_with('.') || SKIPPED_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, depth + 1, budget, found);
        } else if kind.is_file()
            && name.ends_with(".nyk")
            && let Some(module) = module_name(root, &path)
        {
            found.push((module, path));
        }
    }
}

/// The source files of the project of `requester` (other than itself) with
/// the names `require` takes for them, from a bounded walk of the project
/// directory that skips `target`, `node_modules`, `neyuki_packages` and
/// hidden directories.
pub fn project_module_names(requester: &Path) -> Vec<(String, PathBuf)> {
    let Some(dir) = requester.parent() else {
        return Vec::new();
    };
    let root = match project_root(dir) {
        Some(root) => root,
        // Without a manifest the directory of the file is the project, but
        // never the root of a drive.
        None if dir.parent().is_some() => dir.to_path_buf(),
        None => return Vec::new(),
    };
    let mut found = Vec::new();
    let mut budget = MAX_WALK_ENTRIES;
    walk(&root, &root, 0, &mut budget, &mut found);
    let own = path_key(requester);
    found.retain(|(_, path)| path_key(path) != own);
    found
}

// ---------------------------------------------------------------- cache

#[derive(Clone, PartialEq, Eq)]
enum Stamp {
    Disk {
        modified: Option<SystemTime>,
        length: u64,
    },
    Text {
        hash: u64,
        length: usize,
    },
    /// Never changes: the bundled libraries.
    Fixed,
}

struct Cached {
    stamp: Stamp,
    /// The last shape that parsed.
    shape: Option<Rc<Shape>>,
}

/// The shapes of the files that were read, remembered by path and the file's
/// modification time (or by the hash of the text of an open document). A file
/// that stops parsing keeps its last good shape.
#[derive(Default)]
pub struct ModuleStore {
    cache: RefCell<HashMap<String, Cached>>,
}

fn stem_of(key: &str) -> String {
    Path::new(key)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("module")
        .to_string()
}

impl ModuleStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lookup(
        &self,
        key: &str,
        stamp: Stamp,
        parse: impl FnOnce() -> Option<Shape>,
    ) -> Option<Rc<Shape>> {
        if let Some(cached) = self.cache.borrow().get(key)
            && cached.stamp == stamp
        {
            return cached.shape.clone();
        }
        let fresh = parse().map(Rc::new);
        let mut cache = self.cache.borrow_mut();
        let previous = cache.get(key).and_then(|cached| cached.shape.clone());
        let shape = fresh.or(previous);
        if cache.len() >= MAX_CACHED {
            cache.clear();
        }
        cache.insert(
            key.to_string(),
            Cached {
                stamp,
                shape: shape.clone(),
            },
        );
        shape
    }

    /// The shape of `text`, the unsaved contents of the file `key` names.
    pub fn shape_of_text(&self, key: &str, text: &str) -> Option<Rc<Shape>> {
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        let stamp = Stamp::Text {
            hash: hasher.finish(),
            length: text.len(),
        };
        self.lookup(key, stamp, || extract(text, &stem_of(key)))
    }

    /// The shape of the file at `path`; none when it is missing, too large
    /// or has never parsed.
    pub fn shape_of_file(&self, path: &Path) -> Option<Rc<Shape>> {
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            return None;
        }
        let stamp = Stamp::Disk {
            modified: meta.modified().ok(),
            length: meta.len(),
        };
        let key = path_key(path);
        self.lookup(&key, stamp, || {
            let text = std::fs::read_to_string(path).ok()?;
            extract(&text, &stem_of(&key))
        })
    }

    /// The shape of a bundled module (`ui` or `@neyuki/ui`), for its types.
    pub fn bundled_shape(&self, module: &str) -> Option<Rc<Shape>> {
        let bare = module.strip_prefix("@neyuki/").unwrap_or(module);
        let (_, source) = crate::vm::libs::BUNDLED_LIBRARIES
            .iter()
            .find(|(path, _)| path.strip_prefix("@neyuki/") == Some(bare))?;
        let key = format!("bundled:{bare}");
        self.lookup(&key, Stamp::Fixed, || extract(source, bare))
    }
}

// --------------------------------------------------------------- source

struct Loaded {
    key: String,
    file: PathBuf,
    shape: Rc<Shape>,
}

/// The standard library plus the modules a document requires. The modules
/// load as they are named: `namespace_for_module` answers `@m0`, `@m1`, ...
/// for the files it finds, and the members of `@m0` and of the types below it
/// (`@m0.Options`) are answered from their shapes.
pub struct ProjectSource<'a> {
    api: &'a dyn MemberSource,
    store: &'a ModuleStore,
    documents: &'a HashMap<String, String>,
    /// The file of the document; none when it is not a file.
    path: Option<PathBuf>,
    /// The shape of the document itself, for the types it declares.
    current: Option<Rc<Shape>>,
    loaded: RefCell<Vec<Loaded>>,
}

/// `@m3.a.b` as the index 3 and the path `a`, `b`.
fn split_key(name: &str) -> Option<(usize, Vec<&str>)> {
    let rest = name.strip_prefix("@m")?;
    let end = rest.find(['.', ':']).unwrap_or(rest.len());
    let index = rest[..end].parse().ok()?;
    let path = rest[end..]
        .split(['.', ':'])
        .filter(|segment| !segment.is_empty())
        .collect();
    Some((index, path))
}

impl<'a> ProjectSource<'a> {
    /// A source for the document `uri` whose text is `text`. `documents` are
    /// the open documents by URI: their unsaved text wins over the files.
    pub fn new(
        api: &'a dyn MemberSource,
        store: &'a ModuleStore,
        documents: &'a HashMap<String, String>,
        uri: &str,
        text: &str,
    ) -> Self {
        let path = uri_to_path(uri);
        let current = store.shape_of_text(&document_key(uri), text);
        Self {
            api,
            store,
            documents,
            path,
            current,
            loaded: RefCell::new(Vec::new()),
        }
    }

    /// The text of the open document that is the file `file`.
    fn open_text(&self, file: &Path) -> Option<&'a String> {
        let key = path_key(file);
        let documents: &'a HashMap<String, String> = self.documents;
        documents
            .iter()
            .find(|(uri, _)| uri_to_path(uri).is_some_and(|path| path_key(&path) == key))
            .map(|(_, text)| text)
    }

    fn shape_at(&self, file: &Path) -> Option<Rc<Shape>> {
        match self.open_text(file) {
            Some(text) => self.store.shape_of_text(&path_key(file), text),
            None => self.store.shape_of_file(file),
        }
    }

    /// The file and shape a module resolves to, following
    /// `return require("other")` a few hops.
    fn resolve_shape(&self, file: &Path) -> Option<(PathBuf, Rc<Shape>)> {
        let mut file = file.to_path_buf();
        for _ in 0..=MAX_REEXPORT_DEPTH {
            let shape = self.shape_at(&file)?;
            match shape.reexport.clone() {
                None => return Some((file, shape)),
                Some(next) => file = resolve_module_file(&file, &next)?,
            }
        }
        None
    }

    /// The index in `loaded` of the module `require(module)` names.
    fn load(&self, module: &str) -> Option<usize> {
        let requester = self.path.as_ref()?;
        let file = resolve_module_file(requester, module)?;
        let (file, shape) = self.resolve_shape(&file)?;
        let key = path_key(&file);
        let mut loaded = self.loaded.borrow_mut();
        if let Some(index) = loaded.iter().position(|m| m.key == key) {
            return Some(index);
        }
        loaded.push(Loaded { key, file, shape });
        Some(loaded.len() - 1)
    }

    /// The file and shape of module `index`.
    fn module(&self, index: usize) -> Option<(PathBuf, Rc<Shape>)> {
        let loaded = self.loaded.borrow();
        let module = loaded.get(index)?;
        Some((module.file.clone(), module.shape.clone()))
    }

    /// The types in scope for `a.b.c`: those of the module `@mN`, the bundled
    /// module `a`, or the document, with the path below the type.
    fn types_for<'p>(&self, parts: &'p [&'p str]) -> Option<(Vec<TypeDecl>, &'p [&'p str])> {
        if let Some(current) = &self.current
            && parts
                .first()
                .is_some_and(|first| current.types.iter().any(|decl| decl.name == *first))
        {
            return Some((current.types.clone(), parts));
        }
        let (module, rest) = parts.split_first()?;
        let shape = self.store.bundled_shape(module)?;
        Some((shape.types.clone(), rest))
    }
}

impl MemberSource for ProjectSource<'_> {
    fn globals(&self) -> Vec<Entry> {
        self.api.globals()
    }

    fn standard_namespaces(&self) -> Vec<Library> {
        self.api.standard_namespaces()
    }

    fn modules(&self) -> Vec<Library> {
        let mut libraries = self.api.modules();
        let Some(requester) = &self.path else {
            return libraries;
        };
        for name in dependency_names(requester) {
            libraries.push(Library {
                page: name.clone(),
                title: name.clone(),
                module: Some(name),
                doc: "A package of this project's `neyuki.toml`.".to_string(),
                file: None,
            });
        }
        for (name, file) in project_module_names(requester) {
            libraries.push(Library {
                page: name.clone(),
                title: name.clone(),
                module: Some(name),
                doc: String::new(),
                file: Some(file),
            });
        }
        libraries
    }

    fn namespace_for_module(&self, module: &str) -> Option<String> {
        if let Some(namespace) = self.api.namespace_for_module(module) {
            return Some(namespace);
        }
        let index = self.load(module)?;
        Some(format!("@m{index}"))
    }

    fn library_for_namespace(&self, namespace: &str) -> Option<Library> {
        if let Some(library) = self.api.library_for_namespace(namespace) {
            return Some(library);
        }
        let (index, path) = split_key(namespace)?;
        if !path.is_empty() {
            return None;
        }
        let (file, _) = self.module(index)?;
        let title = file.file_name().map_or_else(
            || "module".to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        Some(Library {
            page: title.clone(),
            title,
            module: None,
            doc: format!("`{}`", file.display()),
            file: Some(file),
        })
    }

    fn entry(&self, name: &str) -> Option<Entry> {
        if let Some(entry) = self.api.entry(name) {
            return Some(entry);
        }
        if let Some((index, path)) = split_key(name) {
            let (_, shape) = self.module(index)?;
            let mut display = vec![shape.owner.as_str()];
            if let Some(member) = find_member(&shape.members, &path) {
                display.extend(&path[..path.len() - 1]);
                return Some(member_entry(member, name.to_string(), &display.join(".")));
            }
            let (first, _) = path.split_first()?;
            if path.len() == 1
                && let Some(decl) = shape.types.iter().find(|decl| decl.name == *first)
            {
                return Some(type_entry(decl, name.to_string()));
            }
            // A field of a record type: `@m0.Options.name`.
            let (field, owner_path) = path.split_last()?;
            let fields = type_fields_at(&shape.types, owner_path)?;
            let parent = name.get(..name.len() - field.len() - 1)?;
            return field_entries(fields, parent)
                .into_iter()
                .find(|entry| entry.name == name);
        }
        // Types of the document and of bundled modules.
        let parts: Vec<&str> = name.split('.').collect();
        let (types, path) = self.types_for(&parts)?;
        if path.len() == 1 {
            let decl = types.iter().find(|decl| decl.name == path[0])?;
            return Some(type_entry(decl, name.to_string()));
        }
        let (field, owner_path) = path.split_last()?;
        let fields = type_fields_at(&types, owner_path)?;
        let parent = name.get(..name.len() - field.len() - 1)?;
        field_entries(fields, parent)
            .into_iter()
            .find(|entry| entry.name == name)
    }

    fn namespace_members(&self, namespace: &str) -> Vec<Entry> {
        let members = self.api.namespace_members(namespace);
        if !members.is_empty() {
            return members;
        }
        if let Some((index, path)) = split_key(namespace) {
            let Some((_, shape)) = self.module(index) else {
                return Vec::new();
            };
            let mut display = vec![shape.owner.as_str()];
            display.extend(&path);
            let display = display.join(".");
            let listed: Option<&[Member]> = if path.is_empty() {
                Some(&shape.members)
            } else {
                find_member(&shape.members, &path).map(|member| member.children.as_slice())
            };
            if let Some(listed) = listed {
                return listed
                    .iter()
                    .map(|member| {
                        let separator = if member.kind == EntryKind::Method {
                            ':'
                        } else {
                            '.'
                        };
                        member_entry(
                            member,
                            format!("{namespace}{separator}{}", member.name),
                            &display,
                        )
                    })
                    .collect();
            }
            return type_fields_at(&shape.types, &path)
                .map(|fields| field_entries(fields, namespace))
                .unwrap_or_default();
        }
        let parts: Vec<&str> = namespace.split('.').collect();
        match self.types_for(&parts) {
            Some((types, path)) => type_fields_at(&types, path)
                .map(|fields| field_entries(fields, namespace))
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    fn type_methods(&self, type_name: &str) -> Vec<Entry> {
        self.api.type_methods(type_name)
    }

    fn type_names(&self) -> Vec<String> {
        self.api.type_names()
    }

    fn type_aliases(&self, namespace: &str) -> Vec<Entry> {
        let shape = match split_key(namespace) {
            Some((index, path)) if path.is_empty() => self.module(index).map(|(_, shape)| shape),
            Some(_) => None,
            None => self.store.bundled_shape(namespace),
        };
        let Some(shape) = shape else {
            return Vec::new();
        };
        shape
            .types
            .iter()
            .filter(|decl| decl.exported)
            .map(|decl| type_entry(decl, format!("{namespace}.{}", decl.name)))
            .collect()
    }

    fn local_types(&self) -> Vec<Entry> {
        match &self.current {
            Some(shape) => shape
                .types
                .iter()
                .map(|decl| type_entry(decl, decl.name.clone()))
                .collect(),
            None => Vec::new(),
        }
    }

    fn qualify_type(&self, entry: &str, ty: &str) -> String {
        let Some((index, _)) = split_key(entry) else {
            return ty.to_string();
        };
        let Some((_, shape)) = self.module(index) else {
            return ty.to_string();
        };
        let names: Vec<&str> = shape.types.iter().map(|decl| decl.name.as_str()).collect();
        qualify_names(ty, &names, &format!("@m{index}"))
    }

    fn definition(&self, name: &str) -> Option<Definition> {
        let (index, path) = split_key(name)?;
        let (file, shape) = self.module(index)?;
        if path.is_empty() {
            return Some(Definition {
                file,
                line: 0,
                character: 0,
                length: 0,
            });
        }
        let member_pos = find_member(&shape.members, &path).and_then(|member| member.pos);
        let type_pos = || {
            if path.len() == 1 {
                shape
                    .types
                    .iter()
                    .find(|decl| decl.name == path[0])
                    .map(|decl| decl.pos)
            } else {
                None
            }
        };
        let pos = member_pos.or_else(type_pos)?;
        Some(Definition {
            file,
            line: pos.line,
            character: pos.character,
            length: pos.length,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::api::api;

    const KHIONE: &str = "-- Opens windows.\n\
        export type Options = { name: string?, size: number, inner: Inner }\n\
        type Inner = { depth: int }\n\
        local khione = {}\n\
        khione.version = \"1.0\"\n\
        -- Creates a window.\n\
        -- Second line.\n\
        function khione.new(options: Options, flag: boolean?): Inner\n\
        \x20   return nil\n\
        end\n\
        function khione:close(code: int)\n\
        end\n\
        khione.limits = { max = 10 }\n\
        return khione\n";

    fn shape_of(text: &str) -> Shape {
        extract(text, "mod").expect("parses")
    }

    #[test]
    fn test_extract_members_of_a_returned_table() {
        let shape = shape_of(KHIONE);
        assert_eq!(shape.owner, "khione");
        let names: Vec<&str> = shape.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["version", "new", "close", "limits"]);

        let new = &shape.members[1];
        assert_eq!(new.kind, EntryKind::Function);
        assert_eq!(new.doc, "Creates a window.\nSecond line.");
        assert_eq!(new.returns, "Inner");
        assert_eq!(new.params.len(), 2);
        assert_eq!(new.params[0].label, "options: Options");
        assert!(new.params[1].optional);
        // The name is where it is written: line 8, after `function khione.`.
        let pos = new.pos.unwrap();
        assert_eq!((pos.line, pos.character, pos.length), (7, 16, 3));

        // A method leaves out its receiver.
        let close = &shape.members[2];
        assert_eq!(close.kind, EntryKind::Method);
        assert_eq!(close.params.len(), 1);
        assert_eq!(close.params[0].name, "code");

        assert_eq!(shape.members[0].kind, EntryKind::Value);
        assert_eq!(shape.members[0].returns, "string");
        // A table assigned to a member has members of its own.
        assert_eq!(shape.members[3].kind, EntryKind::Namespace);
        assert_eq!(shape.members[3].children[0].name, "max");
    }

    #[test]
    fn test_extract_types_and_their_docs() {
        let shape = shape_of(KHIONE);
        let names: Vec<(&str, bool)> = shape
            .types
            .iter()
            .map(|t| (t.name.as_str(), t.exported))
            .collect();
        assert_eq!(names, vec![("Options", true), ("Inner", false)]);
        assert_eq!(shape.types[0].pos.line, 1);
        assert_eq!(shape.types[0].pos.character, 12);
        assert_eq!(
            shape.types[0].ty,
            "{ name: string?, size: number, inner: Inner }"
        );
    }

    #[test]
    fn test_extract_returned_table_constructor_and_local_functions() {
        let shape = shape_of(
            "-- Adds.\nlocal function add(a: number, b: number): number\n    return a + b\nend\nlocal f = function(x) return x end\nreturn { add = add, twice = function(n: int) return n end, f = f, name = \"m\" }\n",
        );
        let names: Vec<&str> = shape.members.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["add", "twice", "f", "name"]);
        assert_eq!(shape.members[0].returns, "number");
        assert_eq!(shape.members[0].doc, "Adds.");
        assert_eq!(shape.members[1].params[0].ty, "int");
        assert_eq!(shape.members[2].kind, EntryKind::Function);
        assert_eq!(shape.members[3].returns, "string");
    }

    #[test]
    fn test_extract_reexports_and_unknown_shapes() {
        let shape = shape_of("return require(\"other\")\n");
        assert_eq!(shape.reexport.as_deref(), Some("other"));
        let shape = shape_of("local m = require(\"x\")\nreturn m\n");
        assert_eq!(shape.reexport.as_deref(), Some("x"));
        // Nothing statically known: no members, no panic.
        assert!(shape_of("return f()\n").members.is_empty());
        assert!(shape_of("local x = 1\n").members.is_empty());
        assert!(extract("function (", "m").is_none());
    }

    #[test]
    fn test_record_fields_resolve_aliases_and_optionals() {
        let shape = shape_of(KHIONE);
        let fields = type_fields_at(&shape.types, &["Options"]).unwrap();
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["name", "size", "inner"]);
        // The type of a field that names another alias.
        let inner = type_fields_at(&shape.types, &["Options", "inner"]).unwrap();
        assert_eq!(inner[0].0, "depth");
        assert!(type_fields_at(&shape.types, &["Nope"]).is_none());
        assert!(type_fields_at(&shape.types, &["Options", "size"]).is_none());
    }

    #[test]
    fn test_qualify_names_only_rewrites_whole_unqualified_names() {
        let names = ["Options", "Inner"];
        assert_eq!(
            qualify_names("Options?", &names, "@m0"),
            "@m0.Options?".to_string()
        );
        assert_eq!(
            qualify_names("{Inner} | other.Options | OptionsX", &names, "@m1"),
            "{@m1.Inner} | other.Options | OptionsX".to_string()
        );
    }

    #[test]
    fn test_uri_and_path_round_trip() {
        let dir = std::env::temp_dir()
            .join("neyuki lsp uri test")
            .join("a.nyk");
        let uri = path_to_uri(&dir);
        assert!(uri.starts_with("file:///"));
        assert!(uri.contains("neyuki%20lsp%20uri%20test"));
        let back = uri_to_path(&uri).unwrap();
        assert_eq!(path_key(&back), path_key(&dir));
        assert!(uri_to_path("untitled:Untitled-1").is_none());
        assert_eq!(
            uri_to_path("file:///home/me/a%2Bb.nyk")
                .map(|p| p.to_string_lossy().replace('\\', "/")),
            Some("/home/me/a+b.nyk".to_string())
        );
    }

    #[test]
    fn test_module_paths_are_checked_like_the_runtime() {
        assert!(is_plain_module_path("khione"));
        assert!(is_plain_module_path("./sub/mod"));
        assert!(is_plain_module_path("a.b"));
        assert!(!is_plain_module_path(""));
        assert!(!is_plain_module_path("../up"));
        assert!(!is_plain_module_path("/abs"));
        assert!(!is_plain_module_path("@neyuki/ui"));
        assert!(!is_plain_module_path("a\0b"));
    }

    #[test]
    fn test_module_names_of_files() {
        let root = Path::new("proj");
        assert_eq!(module_name(root, &root.join("a.nyk")).as_deref(), Some("a"));
        assert_eq!(
            module_name(root, &root.join("sub").join("b.nyk")).as_deref(),
            Some("sub/b")
        );
        assert_eq!(
            module_name(root, &root.join("sub").join("init.nyk")).as_deref(),
            Some("sub")
        );
        assert_eq!(
            module_name(root, &root.join("init.nyk")).as_deref(),
            Some("init")
        );
    }

    /// A directory of its own under the system temp directory.
    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("neyuki_lsp_{}_{}", name, std::process::id()));
        cleanup(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn test_resolve_module_file_uses_the_searcher_patterns() {
        let dir = scratch("resolve");
        write(&dir.join("main.nyk"), "");
        write(&dir.join("sibling.nyk"), "return {}");
        write(&dir.join("pkg").join("init.nyk"), "return {}");
        write(&dir.join("modules").join("util.nyk"), "return {}");
        write(
            &dir.join("neyuki_packages").join("dep").join("main.nyk"),
            "return {}",
        );
        let main = dir.join("main.nyk");

        for module in ["sibling", "./sibling", "sibling.nyk", "pkg", "util", "dep"] {
            assert!(
                resolve_module_file(&main, module).is_some(),
                "{module} should resolve"
            );
        }
        assert!(resolve_module_file(&main, "missing").is_none());
        assert!(resolve_module_file(&main, "../main").is_none());
        assert!(resolve_module_file(&main, "@neyuki/ui").is_none());
        cleanup(&dir);
    }

    #[test]
    fn test_project_is_found_through_the_manifest() {
        let dir = scratch("root");
        write(
            &dir.join("neyuki.toml"),
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\n\n[dependencies]\nkhione = \"1.0.0\"\n",
        );
        write(&dir.join("lib").join("helper.nyk"), "return {}");
        write(&dir.join("src").join("main.nyk"), "");
        write(&dir.join("src").join("part").join("init.nyk"), "return {}");
        write(&dir.join("target").join("junk.nyk"), "return {}");
        write(&dir.join(".git").join("hook.nyk"), "return {}");
        write(&dir.join("node_modules").join("m.nyk"), "return {}");
        write(
            &dir.join("neyuki_packages").join("khione").join("init.nyk"),
            "return {}",
        );
        let main = dir.join("src").join("main.nyk");

        // From a file below the root, a module of another directory resolves
        // against the project root, as the runtime's base does.
        assert!(resolve_module_file(&main, "lib/helper").is_some());
        assert_eq!(dependency_names(&main), vec!["khione".to_string()]);

        let names: Vec<String> = project_module_names(&main)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(names.contains(&"lib/helper".to_string()), "{names:?}");
        assert!(names.contains(&"src/part".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("junk")), "{names:?}");
        assert!(!names.iter().any(|n| n.contains("hook")), "{names:?}");
        assert!(
            !names.iter().any(|n| n.contains("node_modules")),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("neyuki_packages")),
            "{names:?}"
        );
        // The requesting file is not offered to itself.
        assert!(!names.contains(&"src/main".to_string()), "{names:?}");
        cleanup(&dir);
    }

    #[test]
    fn test_store_remembers_the_last_good_shape() {
        let store = ModuleStore::new();
        let good = store
            .shape_of_text("k", "local m = {}\nfunction m.a() end\nreturn m\n")
            .unwrap();
        assert_eq!(good.members.len(), 1);
        // The text stops parsing: the last good shape stays.
        let kept = store
            .shape_of_text("k", "local m = {}\nfunction m.a(\n")
            .unwrap();
        assert_eq!(kept.members.len(), 1);
        // A different key knows nothing of it.
        assert!(store.shape_of_text("other", "function (").is_none());
    }

    #[test]
    fn test_store_rereads_a_changed_file_and_ignores_a_missing_one() {
        let dir = scratch("store");
        let file = dir.join("m.nyk");
        write(&file, "local m = {}\nfunction m.a() end\nreturn m\n");
        let store = ModuleStore::new();
        assert_eq!(store.shape_of_file(&file).unwrap().members.len(), 1);
        // A longer file has another length, whatever the clock says.
        write(
            &file,
            "local m = {}\nfunction m.a() end\nfunction m.b() end\nreturn m\n",
        );
        assert_eq!(store.shape_of_file(&file).unwrap().members.len(), 2);
        assert!(store.shape_of_file(&dir.join("missing.nyk")).is_none());
        cleanup(&dir);
    }

    fn source<'a>(
        store: &'a ModuleStore,
        documents: &'a HashMap<String, String>,
        path: &Path,
        text: &str,
    ) -> ProjectSource<'a> {
        ProjectSource::new(api(), store, documents, &path_to_uri(path), text)
    }

    #[test]
    fn test_source_lists_members_of_a_sibling_module() {
        let dir = scratch("members");
        write(&dir.join("khione.nyk"), KHIONE);
        let main = dir.join("main.nyk");
        let store = ModuleStore::new();
        let documents = HashMap::new();
        let src = source(&store, &documents, &main, "local k = require(\"khione\")\n");

        let namespace = src.namespace_for_module("khione").unwrap();
        assert_eq!(namespace, "@m0");
        let members = src.namespace_members(&namespace);
        let names: Vec<String> = members
            .iter()
            .map(|e| crate::lsp::analysis::short_name(&e.name).to_string())
            .collect();
        assert_eq!(names, vec!["version", "new", "close", "limits"]);

        let new = src.entry("@m0.new").unwrap();
        assert_eq!(
            new.signature,
            "khione.new(options: Options, flag: boolean?): Inner"
        );
        assert_eq!(new.doc, "Creates a window.\nSecond line.");
        // A method is looked up with a colon as well.
        assert_eq!(
            src.entry("@m0:close").unwrap().signature,
            "khione:close(code: int)"
        );
        // Nested tables list their members.
        assert_eq!(
            src.namespace_members("@m0.limits")[0].name,
            "@m0.limits.max"
        );

        // The return type is rewritten to read in the requesting file.
        assert_eq!(src.qualify_type("@m0.new", "Inner"), "@m0.Inner");
        // Standard library names still answer.
        assert_eq!(
            src.namespace_for_module("@neyuki/ui").as_deref(),
            Some("ui")
        );
        // A missing module is no namespace.
        assert!(src.namespace_for_module("missing").is_none());
        cleanup(&dir);
    }

    #[test]
    fn test_source_exported_types_and_record_fields() {
        let dir = scratch("types");
        write(&dir.join("khione.nyk"), KHIONE);
        let main = dir.join("main.nyk");
        let store = ModuleStore::new();
        let documents = HashMap::new();
        let src = source(&store, &documents, &main, "local k = require(\"khione\")\n");
        let namespace = src.namespace_for_module("khione").unwrap();

        // Only exported types are offered after `k.` in a type position.
        let aliases = src.type_aliases(&namespace);
        let names: Vec<&str> = aliases.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["@m0.Options"]);
        assert_eq!(
            src.entry("@m0.Options").unwrap().signature,
            "export type Options = { name: string?, size: number, inner: Inner }"
        );

        // The fields of the record, and of a field that names a type.
        let fields: Vec<String> = src
            .namespace_members("@m0.Options")
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(
            fields,
            vec!["@m0.Options.name", "@m0.Options.size", "@m0.Options.inner"]
        );
        assert_eq!(
            src.namespace_members("@m0.Options.inner")[0].name,
            "@m0.Options.inner.depth"
        );
        assert_eq!(src.entry("@m0.Options.size").unwrap().returns, "number");

        // A bundled module's types.
        let event = src.namespace_members("ui.Event");
        assert!(event.iter().any(|e| e.name == "ui.Event.kind"));
        assert!(src.type_aliases("ui").iter().any(|e| e.name == "ui.Event"));
        cleanup(&dir);
    }

    #[test]
    fn test_source_open_document_text_wins_over_the_file() {
        let dir = scratch("unsaved");
        write(
            &dir.join("khione.nyk"),
            "local m = {}\nfunction m.disk() end\nreturn m\n",
        );
        let main = dir.join("main.nyk");
        let store = ModuleStore::new();
        let mut documents = HashMap::new();
        documents.insert(
            path_to_uri(&dir.join("khione.nyk")),
            "local m = {}\nfunction m.unsaved() end\nreturn m\n".to_string(),
        );
        let src = source(&store, &documents, &main, "");
        let namespace = src.namespace_for_module("khione").unwrap();
        let names: Vec<String> = src
            .namespace_members(&namespace)
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["@m0.unsaved"]);
        cleanup(&dir);
    }

    #[test]
    fn test_source_degrades_quietly() {
        let dir = scratch("degrade");
        write(&dir.join("broken.nyk"), "local m = {\nfunction (");
        write(&dir.join("big.nyk"), &"-- filler\n".repeat(200_000));
        write(&dir.join("empty.nyk"), "");
        let main = dir.join("main.nyk");
        let store = ModuleStore::new();
        let documents = HashMap::new();
        let src = source(&store, &documents, &main, "");
        assert!(src.namespace_for_module("broken").is_none());
        assert!(src.namespace_for_module("big").is_none());
        // A module that returns nothing has no members but does exist.
        let namespace = src.namespace_for_module("empty").unwrap();
        assert!(src.namespace_members(&namespace).is_empty());
        assert!(src.entry("@m9.nothing").is_none());
        assert!(src.namespace_members("@m9").is_empty());
        cleanup(&dir);
    }

    #[test]
    fn test_source_cycles_terminate() {
        let dir = scratch("cycle");
        // Two modules that require each other, and two that re-export each
        // other: the first resolves, the second has nothing to show.
        write(
            &dir.join("a.nyk"),
            "local b = require(\"b\")\nlocal a = {}\nfunction a.one() end\nreturn a\n",
        );
        write(
            &dir.join("b.nyk"),
            "local a = require(\"a\")\nlocal b = {}\nfunction b.two() end\nreturn b\n",
        );
        write(&dir.join("x.nyk"), "return require(\"y\")\n");
        write(&dir.join("y.nyk"), "return require(\"x\")\n");
        let main = dir.join("main.nyk");
        let store = ModuleStore::new();
        let documents = HashMap::new();
        let src = source(&store, &documents, &main, "");
        let namespace = src.namespace_for_module("a").unwrap();
        assert_eq!(src.namespace_members(&namespace)[0].name, "@m0.one");
        assert!(src.namespace_for_module("x").is_none());
        cleanup(&dir);
    }

    #[test]
    fn test_source_reexport_follows_the_other_file() {
        let dir = scratch("reexport");
        write(
            &dir.join("real.nyk"),
            "local m = {}\nfunction m.f() end\nreturn m\n",
        );
        write(&dir.join("front.nyk"), "return require(\"real\")\n");
        let main = dir.join("main.nyk");
        let store = ModuleStore::new();
        let documents = HashMap::new();
        let src = source(&store, &documents, &main, "");
        let namespace = src.namespace_for_module("front").unwrap();
        assert_eq!(src.namespace_members(&namespace)[0].name, "@m0.f");
        cleanup(&dir);
    }

    #[test]
    fn test_source_definitions_point_into_the_module_file() {
        let dir = scratch("definition");
        write(&dir.join("khione.nyk"), KHIONE);
        let main = dir.join("main.nyk");
        let store = ModuleStore::new();
        let documents = HashMap::new();
        let src = source(&store, &documents, &main, "");
        let namespace = src.namespace_for_module("khione").unwrap();

        let new = src.definition(&format!("{namespace}.new")).unwrap();
        assert_eq!(path_key(&new.file), path_key(&dir.join("khione.nyk")));
        assert_eq!((new.line, new.character, new.length), (7, 16, 3));
        let options = src.definition(&format!("{namespace}.Options")).unwrap();
        assert_eq!((options.line, options.character), (1, 12));
        let module = src.definition(&namespace).unwrap();
        assert_eq!((module.line, module.character), (0, 0));
        assert!(src.definition(&format!("{namespace}.nothing")).is_none());
        cleanup(&dir);
    }

    #[test]
    fn test_source_lists_module_candidates() {
        let dir = scratch("candidates");
        write(&dir.join("sibling.nyk"), "return {}");
        let main = dir.join("main.nyk");
        let store = ModuleStore::new();
        let documents = HashMap::new();
        let src = source(&store, &documents, &main, "");
        let modules = src.modules();
        let find = |name: &str| modules.iter().find(|l| l.module.as_deref() == Some(name));
        assert!(find("@neyuki/ui").is_some_and(|l| l.file.is_none()));
        assert!(find("sibling").is_some_and(|l| l.file.is_some()));
        cleanup(&dir);
    }
}
