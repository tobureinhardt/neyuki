//! Lightweight source analysis for completion, hover and signature help.
//!
//! It works on the raw text with a small scanner instead of the parser, so it
//! keeps answering while the code being typed does not parse yet, which is
//! most of the time. What the library knows comes from a `MemberSource`; the
//! symbols of the document itself stay with the parser in `mod.rs`.

use std::collections::HashMap;

use crate::dap_json::Json;
use crate::lsp::api::{Entry, EntryKind, Library, MemberSource, Param, REFERENCE_URL};

/// Words that can never be the name of a call.
pub const KEYWORDS: &[&str] = &[
    "and", "break", "const", "continue", "defer", "do", "else", "elseif", "end", "false", "for",
    "function", "global", "if", "in", "local", "nil", "not", "or", "repeat", "return", "then",
    "true", "until", "while",
];

/// Types offered where a type annotation is expected, besides object types.
pub const TYPES: &[&str] = &[
    "any", "nil", "boolean", "number", "int", "float", "string", "table", "function", "buffer",
    "thread",
];

/// LSP `CompletionItemKind` values.
const KIND_METHOD: usize = 2;
const KIND_FUNCTION: usize = 3;
const KIND_CLASS: usize = 7;
const KIND_MODULE: usize = 9;
const KIND_KEYWORD: usize = 14;
const KIND_CONSTANT: usize = 21;
const KIND_TYPE_PARAMETER: usize = 25;

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The byte length of the character starting at `at`.
fn char_len_at(text: &str, at: usize) -> usize {
    text[at..].chars().next().map_or(1, char::len_utf8)
}

/// Appends `text` with every character but newlines replaced by spaces of
/// the same byte length, so offsets stay valid.
fn push_blank(out: &mut String, text: &str) {
    for c in text.chars() {
        if c == '\n' {
            out.push('\n');
        } else {
            for _ in 0..c.len_utf8() {
                out.push(' ');
            }
        }
    }
}

/// The number of `=` of a long bracket (`[[`, `[==[`) opening `rest`.
fn long_bracket(rest: &str) -> Option<usize> {
    let inner = rest.strip_prefix('[')?;
    let level = inner.bytes().take_while(|&b| b == b'=').count();
    (inner.as_bytes().get(level) == Some(&b'[')).then_some(level)
}

/// The offset just past the bracket closing a long bracket of `level`,
/// searching from `from`.
fn long_bracket_end(text: &str, from: usize, level: usize) -> Option<usize> {
    let close = format!("]{}]", "=".repeat(level));
    text[from..].find(&close).map(|at| from + at + close.len())
}

/// Replaces string and comment contents with spaces, keeping offsets.
/// Expressions inside an interpolated string stay visible.
pub fn mask_text(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let rest = &text[i..];
        if let Some(after) = rest.strip_prefix("--")
            && let Some(level) = long_bracket(after)
        {
            let end = long_bracket_end(text, i + 4 + level, level).unwrap_or(text.len());
            push_blank(&mut out, &text[i..end]);
            i = end;
        } else if rest.starts_with("--")
            && !out
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ')' | ']'))
        {
            // After a name or a bracket `--` is a decrement, not a comment.
            let end = rest.find('\n').map_or(text.len(), |at| i + at);
            push_blank(&mut out, &text[i..end]);
            i = end;
        } else if let Some(level) = long_bracket(rest) {
            out.push('[');
            match long_bracket_end(text, i + 2 + level, level) {
                None => {
                    push_blank(&mut out, &text[i + 1..]);
                    i = text.len();
                }
                Some(end) => {
                    push_blank(&mut out, &text[i + 1..end - 1]);
                    out.push(']');
                    i = end;
                }
            }
        } else if bytes[i] == b'`' {
            out.push('`');
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'`' {
                if bytes[j] == b'\\' {
                    let end = if j + 1 < bytes.len() {
                        j + 1 + char_len_at(text, j + 1)
                    } else {
                        bytes.len()
                    };
                    push_blank(&mut out, &text[j..end]);
                    j = end;
                } else if bytes[j] == b'{' {
                    let mut depth = 0;
                    let mut k = j;
                    while k < bytes.len() {
                        if bytes[k] == b'{' {
                            depth += 1;
                        } else if bytes[k] == b'}' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        k += 1;
                    }
                    out.push_str(&mask_text(&text[j..(k + 1).min(bytes.len())]));
                    j = k + 1;
                } else {
                    let len = char_len_at(text, j);
                    push_blank(&mut out, &text[j..j + len]);
                    j += len;
                }
            }
            if j < bytes.len() {
                out.push('`');
            }
            i = j + 1;
        } else if bytes[i] == b'"' || bytes[i] == b'\'' {
            let quote = bytes[i];
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != quote && bytes[j] != b'\n' {
                if bytes[j] == b'\\' {
                    j += 1;
                }
                j += 1;
            }
            let j = j.min(bytes.len());
            let closed = j < bytes.len() && bytes[j] == quote;
            out.push(quote as char);
            push_blank(&mut out, &text[i + 1..j]);
            if closed {
                out.push(quote as char);
                i = j + 1;
            } else {
                i = j;
            }
        } else {
            let len = char_len_at(text, i);
            out.push_str(&text[i..i + len]);
            i += len;
        }
    }
    out
}

/// What a name declared in the document stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Binding {
    /// A namespace such as `math` or `ui` (from a `require` or a global).
    Namespace(String),
    /// An object type such as `Window`, or `string`.
    Type(String),
}

/// A word is a run of letters, digits and underscores; every other
/// non-space character is a token of its own.
struct Token {
    start: usize,
    end: usize,
    word: bool,
}

struct Tokens<'a> {
    text: &'a str,
    list: Vec<Token>,
}

impl<'a> Tokens<'a> {
    fn new(text: &'a str) -> Self {
        let mut list = Vec::new();
        let mut chars = text.char_indices().peekable();
        while let Some((start, c)) = chars.next() {
            if c.is_whitespace() {
                continue;
            }
            let mut end = start + c.len_utf8();
            let word = c.is_ascii_alphanumeric() || c == '_';
            if word {
                while let Some(&(at, next)) = chars.peek() {
                    if !(next.is_ascii_alphanumeric() || next == '_') {
                        break;
                    }
                    end = at + next.len_utf8();
                    chars.next();
                }
            }
            list.push(Token { start, end, word });
        }
        Self { text, list }
    }

    fn len(&self) -> usize {
        self.list.len()
    }

    /// The text of token `k`, empty past the end.
    fn text(&self, k: usize) -> &'a str {
        self.list
            .get(k)
            .map_or("", |token| &self.text[token.start..token.end])
    }

    /// Whether token `k` is a name (a word that does not start with a digit).
    fn is_name(&self, k: usize) -> bool {
        self.list
            .get(k)
            .is_some_and(|token| token.word && !self.text.as_bytes()[token.start].is_ascii_digit())
    }

    /// Whether token `b` follows token `a` without anything between them.
    fn adjacent(&self, a: usize, b: usize) -> bool {
        match (self.list.get(a), self.list.get(b)) {
            (Some(x), Some(y)) => x.end == y.start,
            _ => false,
        }
    }

    /// The source from the start of token `a` to the end of token `b`.
    fn span(&self, a: usize, b: usize) -> &'a str {
        match (self.list.get(a), self.list.get(b)) {
            (Some(x), Some(y)) => &self.text[x.start..y.end],
            _ => "",
        }
    }
}

fn is_declaration_keyword(word: &str) -> bool {
    matches!(word, "local" | "const" | "global")
}

/// The trailing identifier of `text`, possibly empty.
fn trailing_ident(text: &str) -> &str {
    let start = text
        .bytes()
        .rposition(|b| !is_word_byte(b))
        .map_or(0, |at| at + 1);
    text[start..].trim_start_matches(|c: char| c.is_ascii_digit())
}

/// The base type name of a type annotation: `Window?` -> `Window`.
fn base_type(ty: &str) -> Option<String> {
    let ty = ty.trim();
    let ty = ty.strip_prefix('(').unwrap_or(ty).trim_start();
    let end = ty
        .bytes()
        .position(|b| !is_word_byte(b))
        .unwrap_or(ty.len());
    let name = &ty[..end];
    if name.is_empty() || name.as_bytes()[0].is_ascii_digit() {
        None
    } else {
        Some(name.to_string())
    }
}

/// What the names declared in `text` stand for: a namespace (the standard
/// libraries, `require`, `--!importall`), an object type (from an annotation
/// or a call whose result type is known) or `string` for string literals.
pub fn bindings(text: &str, src: &dyn MemberSource) -> HashMap<String, Binding> {
    let masked = mask_text(text);
    let bytes = masked.as_bytes();
    let mut result = HashMap::new();

    for lib in src.standard_namespaces() {
        if let Some(namespace) = lib.module {
            result.insert(namespace.clone(), Binding::Namespace(namespace));
        }
    }

    // `--!importall` declares one local per bundled module, named after it.
    if crate::compiler::has_import_all(text) {
        for (path, _) in crate::vm::libs::BUNDLED_LIBRARIES {
            if let Some(name) = path.strip_prefix("@neyuki/")
                && let Some(namespace) = src.namespace_for_module(path)
            {
                result.insert(name.to_string(), Binding::Namespace(namespace));
            }
        }
    }

    // `name = require("module")`. The call is found in the masked text, so
    // one in a comment or a string does not count; its argument is read from
    // the original, which has the same offsets.
    for (at, _) in masked.match_indices("require") {
        if at > 0 && is_word_byte(bytes[at - 1]) {
            continue;
        }
        let mut quote = skip_whitespace(&masked, at + "require".len());
        if bytes.get(quote) == Some(&b'(') {
            quote = skip_whitespace(&masked, quote + 1);
        }
        if !matches!(bytes.get(quote), Some(b'"' | b'\'')) {
            continue;
        }
        let Some(length) = text[quote + 1..].find(['"', '\'']) else {
            continue;
        };
        let module = &text[quote + 1..quote + 1 + length];
        if module.is_empty() || module.contains('\n') {
            continue;
        }
        if !matches!(bytes.get(quote + 1 + length), Some(b'"' | b'\'')) {
            continue;
        }
        let Some(target) = masked[..at].trim_end().strip_suffix('=') else {
            continue;
        };
        let name = trailing_ident(target.trim_end());
        if name.is_empty() {
            continue;
        }
        if let Some(namespace) = src.namespace_for_module(module) {
            result.insert(name.to_string(), Binding::Namespace(namespace));
        }
    }

    let tokens = Tokens::new(&masked);

    // `local name: Type`
    for k in 0..tokens.len() {
        if !is_declaration_keyword(tokens.text(k))
            || !tokens.is_name(k + 1)
            || tokens.text(k + 2) != ":"
            || !tokens.is_name(k + 3)
        {
            continue;
        }
        let mut last = k + 3;
        while tokens.text(last + 1) == "."
            && tokens.adjacent(last, last + 1)
            && tokens.is_name(last + 2)
            && tokens.adjacent(last + 1, last + 2)
        {
            last += 2;
        }
        result
            .entry(tokens.text(k + 1).to_string())
            .or_insert_with(|| Binding::Type(tokens.span(k + 3, last).to_string()));
    }

    // `local name = a.b(...)` where the entry for `a.b` says what it returns.
    for k in 0..tokens.len() {
        if !is_declaration_keyword(tokens.text(k))
            || !tokens.is_name(k + 1)
            || tokens.text(k + 2) != "="
            || !tokens.is_name(k + 3)
            || result.contains_key(tokens.text(k + 1))
        {
            continue;
        }
        let mut last = k + 3;
        while matches!(tokens.text(last + 1), "." | ":")
            && tokens.adjacent(last, last + 1)
            && tokens.is_name(last + 2)
            && tokens.adjacent(last + 1, last + 2)
        {
            last += 2;
        }
        if tokens.text(last + 1) != "(" {
            continue;
        }
        let returned = resolve_entry(tokens.span(k + 3, last), &result, src)
            .and_then(|entry| base_type(&entry.returns));
        if let Some(ty) = returned
            && ty != "any"
            && ty != "table"
        {
            result.insert(tokens.text(k + 1).to_string(), Binding::Type(ty));
        }
    }

    // `local name = "text"`
    for k in 0..tokens.len() {
        if is_declaration_keyword(tokens.text(k))
            && tokens.is_name(k + 1)
            && tokens.text(k + 2) == "="
            && matches!(tokens.text(k + 3), "\"" | "'" | "`" | "[")
        {
            result
                .entry(tokens.text(k + 1).to_string())
                .or_insert_with(|| Binding::Type("string".to_string()));
        }
    }
    result
}

/// The offset of the first non-whitespace character of `text` at or after `from`.
fn skip_whitespace(text: &str, from: usize) -> usize {
    from + text[from..].len() - text[from..].trim_start().len()
}

/// Rewrites the first name of `chain` through the bindings: `f.read` ->
/// `fs.read` after `local f = require("@neyuki/fs")`.
pub fn canonical(chain: &str, binds: &HashMap<String, Binding>) -> String {
    let end = chain
        .bytes()
        .position(|b| !is_word_byte(b))
        .unwrap_or(chain.len());
    let head = &chain[..end];
    if head.is_empty() || head.as_bytes()[0].is_ascii_digit() {
        return chain.to_string();
    }
    match binds.get(head) {
        Some(Binding::Namespace(name)) | Some(Binding::Type(name)) => {
            format!("{name}{}", &chain[end..])
        }
        None => chain.to_string(),
    }
}

/// The entry a written expression such as `f.read` or `win:clear` names.
pub fn resolve_entry(
    chain: &str,
    binds: &HashMap<String, Binding>,
    src: &dyn MemberSource,
) -> Option<Entry> {
    src.entry(&canonical(chain, binds))
}

/// Methods available after `receiver:` given what the receiver is bound to.
/// An empty receiver stands for a string literal.
pub fn methods_for(
    receiver: &str,
    binds: &HashMap<String, Binding>,
    src: &dyn MemberSource,
) -> Vec<Entry> {
    let bound = binds.get(receiver);
    let ty = match bound {
        Some(Binding::Type(ty)) => Some(ty.as_str()),
        _ => None,
    };
    if let Some(ty) = ty {
        let methods = src.type_methods(ty);
        if !methods.is_empty() {
            return methods;
        }
    }
    if ty == Some("string") || bound.is_none() {
        // Strings carry the string library as methods; for an unknown
        // receiver it is the likeliest guess.
        return src
            .namespace_members("string")
            .into_iter()
            .filter(|entry| entry.kind == EntryKind::Function)
            .collect();
    }
    Vec::new()
}

/// The entry for a call or hover target written as `chain`, looking through
/// bindings and, for `x:method`, the receiver's type.
pub fn entry_for_chain(
    chain: &str,
    binds: &HashMap<String, Binding>,
    src: &dyn MemberSource,
) -> Option<Entry> {
    if let Some(entry) = resolve_entry(chain, binds, src) {
        return Some(entry);
    }
    let (receiver, method) = chain.rsplit_once(':')?;
    let receiver = receiver.rsplit('.').next().unwrap_or(receiver);
    let dotted = format!(".{method}");
    let coloned = format!(":{method}");
    methods_for(receiver, binds, src)
        .into_iter()
        .find(|entry| entry.name.ends_with(&coloned) || entry.name.ends_with(&dotted))
}

/// An entry's parameters, dropping the receiver for method-style calls of
/// the string library (`s:find(p)` for `string.find(s, p)`).
pub fn call_parameters(entry: &Entry, chain: &str) -> Vec<Param> {
    if chain.contains(':') && entry.kind == EntryKind::Function && entry.name.starts_with("string.")
    {
        entry.params.iter().skip(1).cloned().collect()
    } else {
        entry.params.clone()
    }
}

/// What completion should offer at the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Context {
    /// Inside a string or a comment.
    None,
    /// Inside `require("`: a module name.
    Module { prefix: String },
    /// After `a.b.`: members of the namespace `a.b`.
    Member { chain: String, prefix: String },
    /// After `x:`: methods of the receiver, which is empty when it is a
    /// string literal or a call.
    Method { receiver: String, prefix: String },
    /// Where a type annotation is written.
    Type { prefix: String },
    /// Anywhere else: names in scope.
    Global { prefix: String },
}

/// What completion should offer at the end of `before`, the text up to the
/// cursor.
pub fn completion_context(before: &str) -> Context {
    if let Some(prefix) = require_prefix(before) {
        return Context::Module {
            prefix: prefix.to_string(),
        };
    }

    // Inside a string or a comment (but not an interpolated expression)
    // masking blanks out the text before the cursor.
    let ends_visible = |text: &str| text.chars().next_back().is_some_and(|c| !c.is_whitespace());
    if ends_visible(before) && !ends_visible(&mask_text(before)) {
        return Context::None;
    }

    if let Some((chain, prefix)) = member_context(before) {
        return Context::Member { chain, prefix };
    }

    let prefix = trailing_ident(before);
    let head = before[..before.len() - prefix.len()].trim_end();
    if let Some(head) = head.strip_suffix(':') {
        let head = head.trim_end();
        // A name, or a string or a call, which is "" here.
        let receiver = match head.bytes().next_back() {
            Some(b) if is_word_byte(b) => Some(trailing_ident(head)).filter(|r| !r.is_empty()),
            Some(b')' | b']' | b'"' | b'\'' | b'`') => Some(""),
            _ => None,
        };
        if let Some(receiver) = receiver {
            // A colon after a name could also start a type annotation.
            let line = &before[before.rfind('\n').map_or(0, |at| at + 1)..];
            if is_declaration_line(line) || in_parameters(line) || in_return_type(line) {
                return Context::Type {
                    prefix: prefix.to_string(),
                };
            }
            return Context::Method {
                receiver: receiver.to_string(),
                prefix: prefix.to_string(),
            };
        }
    }
    Context::Global {
        prefix: prefix.to_string(),
    }
}

/// The module name typed so far inside `require("`.
fn require_prefix(before: &str) -> Option<&str> {
    let start = before
        .bytes()
        .rposition(|b| !(is_word_byte(b) || b == b'@' || b == b'/'))
        .map_or(0, |at| at + 1);
    let head = before[..start].strip_suffix(['"', '\''])?.trim_end();
    let head = match head.strip_suffix('(') {
        Some(head) => head.trim_end(),
        None => head,
    };
    let word = head.strip_suffix("require")?;
    if word.bytes().next_back().is_some_and(is_word_byte) {
        return None;
    }
    Some(&before[start..])
}

/// `a.b.` with the member prefix typed so far, as (`a.b`, prefix).
fn member_context(before: &str) -> Option<(String, String)> {
    let prefix = trailing_ident(before);
    let head = before[..before.len() - prefix.len()]
        .trim_end()
        .strip_suffix('.')?
        .trim_end();
    let mut segments = Vec::new();
    let mut rest = head;
    let mut chain_start = head.len();
    loop {
        let ident = trailing_ident(rest);
        if ident.is_empty() {
            break;
        }
        segments.push(ident);
        chain_start = rest.len() - ident.len();
        let Some(earlier) = rest[..chain_start].trim_end().strip_suffix('.') else {
            break;
        };
        rest = earlier.trim_end();
    }
    if segments.is_empty() || before[..chain_start].ends_with('.') {
        return None;
    }
    segments.reverse();
    Some((segments.join("."), prefix.to_string()))
}

/// Whether `line` starts a declaration: `local x: ...`.
fn is_declaration_line(line: &str) -> bool {
    let line = line.trim_start();
    ["local", "const", "global"].iter().any(|keyword| {
        line.strip_prefix(keyword)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    })
}

/// The offsets just past each whole-word `function` in `line`.
fn function_ends(line: &str) -> Vec<usize> {
    line.match_indices("function")
        .filter(|&(at, _)| {
            !line[..at].bytes().next_back().is_some_and(is_word_byte)
                && !line[at + 8..].bytes().next().is_some_and(is_word_byte)
        })
        .map(|(at, _)| at + 8)
        .collect()
}

/// Whether the cursor is in the parameter list of a `function` on `line`.
fn in_parameters(line: &str) -> bool {
    function_ends(line)
        .last()
        .is_some_and(|&end| !line[end..].contains(')'))
}

/// Whether the cursor is at the return type of a `function` on `line`:
/// `function f(a): Ty`.
fn in_return_type(line: &str) -> bool {
    function_ends(line).into_iter().any(|end| {
        let rest = &line[end..];
        let Some(open) = rest.find('(') else {
            return false;
        };
        let Some(close) = rest[open..].find(')') else {
            return false;
        };
        rest[open + close + 1..]
            .trim_start()
            .strip_prefix(':')
            .is_some_and(|ty| ty.trim_start().bytes().all(is_word_byte))
    })
}

/// The call the cursor is inside.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallContext {
    /// The callee as written, whitespace removed: `ui.open`, `w:fillRect`.
    pub callee: String,
    /// How many arguments come before the cursor, ignoring commas inside
    /// brackets and strings.
    pub active_parameter: usize,
}

/// The call the cursor is inside, given the text up to it.
pub fn call_context(before: &str) -> Option<CallContext> {
    let masked = mask_text(before);
    let bytes = masked.as_bytes();
    let mut depth = 0usize;
    let mut commas = 0;
    for i in (0..bytes.len()).rev() {
        match bytes[i] {
            b')' | b']' | b'}' => depth += 1,
            b'(' | b'[' | b'{' => {
                if depth == 0 {
                    if bytes[i] != b'(' {
                        return None;
                    }
                    let callee = chain_ending(&masked[..i])?;
                    if KEYWORDS.contains(&callee.as_str()) {
                        return None;
                    }
                    return Some(CallContext {
                        callee,
                        active_parameter: commas,
                    });
                }
                depth -= 1;
            }
            b',' if depth == 0 => commas += 1,
            // A new statement started; the cursor is not inside a call.
            b'\n' if depth == 0 && starts_statement(&masked[i + 1..]) => return None,
            _ => {}
        }
    }
    None
}

/// Whether `text` begins (after whitespace) with a statement keyword.
fn starts_statement(text: &str) -> bool {
    let text = text.trim_start();
    ["local", "function", "if", "for", "while", "return"]
        .iter()
        .any(|keyword| {
            text.strip_prefix(keyword)
                .is_some_and(|rest| !rest.bytes().next().is_some_and(is_word_byte))
        })
}

/// The dotted or colon-joined name that ends `head`, whitespace removed:
/// `ui.open`, `w:fillRect`.
fn chain_ending(head: &str) -> Option<String> {
    let head = head.trim_end();
    let ident = trailing_ident(head);
    if ident.is_empty() {
        return None;
    }
    let mut chain_start = head.len() - ident.len();
    let mut rest = &head[..chain_start];
    while let Some(earlier) = rest.trim_end().strip_suffix(['.', ':']) {
        let earlier = earlier.trim_end();
        let ident = trailing_ident(earlier);
        if ident.is_empty() {
            break;
        }
        chain_start = earlier.len() - ident.len();
        rest = &earlier[..chain_start];
    }
    Some(
        head[chain_start..]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect(),
    )
}

/// The byte offset of an LSP position (UTF-16 column) in `content`.
pub fn offset_at(content: &str, line: usize, character: usize) -> usize {
    let mut offset = 0;
    for (index, text) in content.split('\n').enumerate() {
        if index == line {
            return offset + super::utf16_col_to_byte_offset(text, character);
        }
        offset += text.len() + 1;
    }
    content.len()
}

/// The identifier that touches the byte `at` of `line`, as a byte range.
fn ident_range_at(line: &str, at: usize) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut start = at.min(bytes.len());
    let mut end = start;
    while start > 0 && is_word_byte(bytes[start - 1]) {
        start -= 1;
    }
    while end < bytes.len() && is_word_byte(bytes[end]) {
        end += 1;
    }
    (start < end && !bytes[start].is_ascii_digit()).then_some((start, end))
}

/// The last part of an entry name: `math.floor` -> `floor`, `Window:clear`
/// -> `clear`.
pub fn short_name(name: &str) -> &str {
    name.rsplit(['.', ':']).next().unwrap_or(name)
}

fn obj<const N: usize>(fields: [(&str, Json); N]) -> Json {
    Json::Obj(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

fn markup(value: String) -> Json {
    obj([
        ("kind", Json::Str("markdown".to_string())),
        ("value", Json::Str(value)),
    ])
}

fn position_json(line: usize, character: usize) -> Json {
    obj([
        ("line", Json::Num(line as f64)),
        ("character", Json::Num(character as f64)),
    ])
}

fn range_json(line: usize, start: usize, end: usize) -> Json {
    obj([
        ("start", position_json(line, start)),
        ("end", position_json(line, end)),
    ])
}

/// Markdown for an entry: its signature, what the reference says and a link.
pub fn entry_markdown(entry: &Entry) -> String {
    let mut text = String::new();
    if entry.kind != EntryKind::Namespace {
        text.push_str(&format!("```neyuki\n{}\n```\n", entry.signature));
    }
    if !entry.doc.is_empty() {
        text.push('\n');
        text.push_str(&entry.doc.replace("```lua", "```neyuki"));
    }
    if !entry.page.is_empty() {
        text.push_str(&format!("\n\n[Reference]({REFERENCE_URL}{})", entry.page));
    }
    text
}

fn is_callable(entry: &Entry) -> bool {
    matches!(entry.kind, EntryKind::Function | EntryKind::Method)
}

/// `name(a: type, b: type): result`, the call as it is documented.
fn call_label(entry: &Entry, params: &[Param]) -> String {
    let labels: Vec<&str> = params.iter().map(|param| param.label.as_str()).collect();
    let returns = if entry.returns.is_empty() || entry.returns == "()" {
        String::new()
    } else {
        format!(": {}", entry.returns)
    };
    format!(
        "{}({}){returns}",
        short_name(&entry.name),
        labels.join(", ")
    )
}

/// A completion item for an entry. `drop_receiver` is for string functions
/// completed after `x:`, which take the string as the receiver.
pub fn entry_item(entry: &Entry, drop_receiver: bool, snippets: bool) -> Json {
    let label = short_name(&entry.name);
    let params: &[Param] = if drop_receiver && entry.name.starts_with("string.") {
        entry.params.get(1..).unwrap_or(&[])
    } else {
        &entry.params
    };
    let kind = match entry.kind {
        EntryKind::Function => KIND_FUNCTION,
        EntryKind::Method => KIND_METHOD,
        EntryKind::Value => KIND_CONSTANT,
        EntryKind::Namespace => KIND_MODULE,
    };
    let detail = if is_callable(entry) {
        call_label(entry, params)
    } else {
        entry.signature.clone()
    };
    let mut item = vec![
        ("label".to_string(), Json::Str(label.to_string())),
        ("kind".to_string(), Json::Num(kind as f64)),
        ("detail".to_string(), Json::Str(detail)),
        ("documentation".to_string(), markup(entry_markdown(entry))),
    ];
    if snippets && is_callable(entry) {
        let text = if params.is_empty() {
            format!("{label}()$0")
        } else {
            format!("{label}($0)")
        };
        item.push(("insertText".to_string(), Json::Str(text)));
        item.push(("insertTextFormat".to_string(), Json::Num(2.0)));
    }
    Json::Obj(item)
}

/// A completion item for a library: a namespace in scope, or with `edit` a
/// module name replacing the `edit.1`..`edit.2` columns of line `edit.0`.
pub fn library_item(lib: &Library, name: &str, edit: Option<(usize, usize, usize)>) -> Json {
    let mut item = vec![
        ("label".to_string(), Json::Str(name.to_string())),
        ("kind".to_string(), Json::Num(KIND_MODULE as f64)),
        (
            "documentation".to_string(),
            markup(lib.doc.replace("```lua", "```neyuki")),
        ),
    ];
    match edit {
        // Module names contain `@` and `/`, which clients do not treat as
        // part of a word, so say what is being replaced.
        Some((line, start, end)) => item.push((
            "textEdit".to_string(),
            obj([
                ("range", range_json(line, start, end)),
                ("newText", Json::Str(name.to_string())),
            ]),
        )),
        None => item.push(("detail".to_string(), Json::Str(format!("module {name}")))),
    }
    Json::Obj(item)
}

/// A completion item for a plain word: a type name or a keyword.
pub fn word_item(label: &str, kind: &str) -> Json {
    let (number, detail) = match kind {
        "keyword" => (KIND_KEYWORD, "keyword"),
        "class" => (KIND_CLASS, "type"),
        _ => (KIND_TYPE_PARAMETER, "type"),
    };
    obj([
        ("label", Json::Str(label.to_string())),
        ("kind", Json::Num(number as f64)),
        ("detail", Json::Str(detail.to_string())),
    ])
}

/// Hover for the library name under the cursor: `Some(Json::Null)` inside a
/// string or a comment, where the answer is no hover, and `None` when the
/// word is not a library name and the caller may look further.
pub fn hover(content: &str, line: usize, character: usize, src: &dyn MemberSource) -> Option<Json> {
    let line_text = content.lines().nth(line)?;
    let at = super::utf16_col_to_byte_offset(line_text, character);
    let (start, end) = ident_range_at(line_text, at)?;

    let masked = mask_text(content);
    let word_offset = offset_at(content, line, 0) + start;
    if masked.as_bytes().get(word_offset) == Some(&b' ') {
        return Some(Json::Null);
    }

    let chain = chain_ending(&line_text[..end])?;
    let binds = bindings(content, src);
    let value = match entry_for_chain(&chain, &binds, src) {
        Some(entry) => entry_markdown(&entry),
        None => {
            let lib = src.library_for_namespace(&canonical(&chain, &binds))?;
            format!(
                "**{}**\n\n{}",
                lib.title,
                lib.doc.replace("```lua", "```neyuki")
            )
        }
    };

    let start_col = line_text[..start].encode_utf16().count();
    let end_col = line_text[..end].encode_utf16().count();
    Some(obj([
        ("contents", markup(value)),
        ("range", range_json(line, start_col, end_col)),
    ]))
}

/// Signature help for the call the cursor is inside, or null.
pub fn signature_help(
    content: &str,
    line: usize,
    character: usize,
    src: &dyn MemberSource,
) -> Json {
    let offset = offset_at(content, line, character);
    let Some(call) = call_context(&content[..offset]) else {
        return Json::Null;
    };
    let binds = bindings(content, src);
    let Some(entry) = entry_for_chain(&call.callee, &binds, src) else {
        return Json::Null;
    };
    if !is_callable(&entry) {
        return Json::Null;
    }

    let params = call_parameters(&entry, &call.callee);
    let label = call_label(&entry, &params);
    let documentation = markup(entry_markdown(&Entry {
        signature: label.clone(),
        ..entry
    }));
    let variadic = params.last().is_some_and(|param| param.name == "...");
    let active = if variadic {
        call.active_parameter.min(params.len() - 1)
    } else {
        call.active_parameter
    };
    let parameters = params
        .iter()
        .map(|param| obj([("label", Json::Str(param.label.clone()))]))
        .collect();
    obj([
        (
            "signatures",
            Json::Arr(vec![obj([
                ("label", Json::Str(label)),
                ("documentation", documentation),
                ("parameters", Json::Arr(parameters)),
            ])]),
        ),
        ("activeSignature", Json::Num(0.0)),
        ("activeParameter", Json::Num(active as f64)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mask_text_keeps_offsets_and_blanks_strings_and_comments() {
        let text = "print(\"a--b\") -- note \u{1f980}\nx = [[long\ntext]] y = `v{n}\u{e9}`";
        let masked = mask_text(text);
        assert_eq!(masked.len(), text.len());
        assert_eq!(masked.matches('\n').count(), text.matches('\n').count());
        assert!(masked.starts_with("print(\""));
        assert!(!masked.contains("a--b"));
        assert!(!masked.contains("note"));
        assert!(!masked.contains("long"));
        // The expression inside an interpolated string stays visible.
        assert!(masked.contains("{n}"));
        assert!(masked.ends_with('`'));
    }

    #[test]
    fn test_mask_text_decrement_is_not_a_comment() {
        let masked = mask_text("x-- y");
        assert_eq!(masked, "x-- y");
    }

    #[test]
    fn test_completion_context_kinds() {
        assert_eq!(
            completion_context("local m = require(\"@neyuki/"),
            Context::Module {
                prefix: "@neyuki/".to_string()
            }
        );
        assert_eq!(
            completion_context("local x = ui.op"),
            Context::Member {
                chain: "ui".to_string(),
                prefix: "op".to_string()
            }
        );
        assert_eq!(
            completion_context("hid.keyboard."),
            Context::Member {
                chain: "hid.keyboard".to_string(),
                prefix: String::new()
            }
        );
        assert_eq!(
            completion_context("w:cl"),
            Context::Method {
                receiver: "w".to_string(),
                prefix: "cl".to_string()
            }
        );
        assert_eq!(
            completion_context("(\"a\"):up"),
            Context::Method {
                receiver: String::new(),
                prefix: "up".to_string()
            }
        );
        assert_eq!(
            completion_context("local w: Wi"),
            Context::Type {
                prefix: "Wi".to_string()
            }
        );
        assert_eq!(
            completion_context("function f(a: Nu"),
            Context::Type {
                prefix: "Nu".to_string()
            }
        );
        assert_eq!(
            completion_context("pri"),
            Context::Global {
                prefix: "pri".to_string()
            }
        );
        assert_eq!(completion_context("print(\"fs."), Context::None);
        assert_eq!(completion_context("-- fs."), Context::None);
        // A call's result is not a namespace.
        assert!(!matches!(
            completion_context("a().b."),
            Context::Member { .. }
        ));
    }

    #[test]
    fn test_call_context_counts_top_level_commas_only() {
        let call = call_context("string.find(s, \"a,b\", math.max(1, 2), ").unwrap();
        assert_eq!(call.callee, "string.find");
        assert_eq!(call.active_parameter, 3);

        let call = call_context("w:fillRect(").unwrap();
        assert_eq!(call.callee, "w:fillRect");
        assert_eq!(call.active_parameter, 0);

        // A table constructor or a finished call is not a call being typed.
        assert_eq!(call_context("f({1, 2, "), None);
        assert_eq!(call_context("f(1, 2)"), None);
        assert_eq!(call_context("if (a and "), None);
        assert_eq!(call_context("x = 1\nlocal y = "), None);
    }
}
