//! Language Server Protocol (LSP) server for `neyuki lsp`, running over stdio.

use std::collections::HashMap;
use std::io::{self, BufReader, Write};

use crate::dap_json::{self, Json};
use crate::diagnostics::severity::Severity;

pub mod analysis;
pub mod api;

use analysis::Context;
use api::MemberSource;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundSymbol {
    pub name: String,
    pub detail: String,
    pub kind: usize,
    pub line_1_based: u32,
    pub col_1_based: u32,
}

pub struct LspServer {
    pub documents: HashMap<String, String>,
    pub last_valid_symbols: HashMap<String, Vec<FoundSymbol>>,
    pub is_shutdown: bool,
    pub is_exit: bool,
    pub exit_code: Option<i32>,
    /// Whether the client accepts snippet syntax in completion items,
    /// read from the `initialize` request.
    pub snippet_support: bool,
}

impl Default for LspServer {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_text_document_position(json: &Json) -> (&str, usize, usize) {
    let uri = json
        .get("params")
        .and_then(|p| p.get("textDocument"))
        .and_then(|d| d.get("uri"))
        .and_then(|u| u.as_str())
        .unwrap_or("");
    let line = json
        .get("params")
        .and_then(|p| p.get("position"))
        .and_then(|pos| pos.get("line"))
        .and_then(|l| l.as_usize())
        .unwrap_or(0);
    let character = json
        .get("params")
        .and_then(|p| p.get("position"))
        .and_then(|pos| pos.get("character"))
        .and_then(|c| c.as_usize())
        .unwrap_or(0);
    (uri, line, character)
}

impl LspServer {
    pub fn new() -> Self {
        Self {
            documents: HashMap::new(),
            last_valid_symbols: HashMap::new(),
            is_shutdown: false,
            is_exit: false,
            exit_code: None,
            snippet_support: false,
        }
    }

    pub fn handle_message(&mut self, msg_bytes: &[u8]) -> Option<Vec<u8>> {
        let json = match dap_json::parse_json(msg_bytes) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("LSP parse error: {}", e);
                let err_resp = json_error(Json::Null, -32700, "Parse error");
                return Some(dap_json::render_json(&err_resp).into_bytes());
            }
        };

        let method = json.get("method").and_then(|m| m.as_str());
        let id = json.get("id");

        if let Some(method_name) = method {
            // Check for requests arriving after shutdown
            if self.is_shutdown && method_name != "exit" {
                if let Some(req_id) = id {
                    let err_resp = json_error(
                        req_id.clone(),
                        -32600,
                        "Invalid Request: server is shutting down",
                    );
                    return Some(dap_json::render_json(&err_resp).into_bytes());
                }
                return None;
            }

            match method_name {
                "initialize" => {
                    let req_id = id.cloned().unwrap_or(Json::Num(1.0));
                    self.snippet_support = matches!(
                        json.get("params")
                            .and_then(|p| p.get("capabilities"))
                            .and_then(|c| c.get("textDocument"))
                            .and_then(|t| t.get("completion"))
                            .and_then(|c| c.get("completionItem"))
                            .and_then(|i| i.get("snippetSupport")),
                        Some(Json::Bool(true))
                    );
                    let triggers = |chars: &[&str]| {
                        Json::Arr(chars.iter().map(|c| Json::Str(c.to_string())).collect())
                    };
                    let resp = json_response(
                        req_id,
                        Json::Obj(vec![
                            (
                                "capabilities".to_string(),
                                Json::Obj(vec![
                                    ("textDocumentSync".to_string(), Json::Num(1.0)),
                                    ("documentFormattingProvider".to_string(), Json::Bool(true)),
                                    ("hoverProvider".to_string(), Json::Bool(true)),
                                    (
                                        "completionProvider".to_string(),
                                        // The quotes and the slash start module names
                                        // inside `require("`.
                                        Json::Obj(vec![(
                                            "triggerCharacters".to_string(),
                                            triggers(&[".", ":", "\"", "'", "/"]),
                                        )]),
                                    ),
                                    (
                                        "signatureHelpProvider".to_string(),
                                        Json::Obj(vec![
                                            (
                                                "triggerCharacters".to_string(),
                                                triggers(&["(", ","]),
                                            ),
                                            ("retriggerCharacters".to_string(), triggers(&[","])),
                                        ]),
                                    ),
                                    ("definitionProvider".to_string(), Json::Bool(true)),
                                    ("documentSymbolProvider".to_string(), Json::Bool(true)),
                                ]),
                            ),
                            // Editors read the version to know which Neyuki they talk to.
                            (
                                "serverInfo".to_string(),
                                Json::Obj(vec![
                                    ("name".to_string(), Json::Str("neyuki".to_string())),
                                    (
                                        "version".to_string(),
                                        Json::Str(env!("CARGO_PKG_VERSION").to_string()),
                                    ),
                                ]),
                            ),
                        ]),
                    );
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "initialized" => {
                    return None;
                }
                "shutdown" => {
                    self.is_shutdown = true;
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let resp = json_response(req_id, Json::Null);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "exit" => {
                    self.is_exit = true;
                    self.exit_code = Some(if self.is_shutdown { 0 } else { 1 });
                    return None;
                }
                "textDocument/didOpen" => {
                    if let Some(params) = json.get("params")
                        && let Some(doc) = params.get("textDocument")
                    {
                        let uri = doc
                            .get("uri")
                            .and_then(|u| u.as_str())
                            .unwrap_or("")
                            .to_string();
                        let content = doc
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        self.documents.insert(uri.clone(), content.clone());
                        let symbols = extract_document_symbols(&content);
                        if !symbols.is_empty() {
                            self.last_valid_symbols.insert(uri.clone(), symbols);
                        }
                        let notif = create_publish_diagnostics(&uri, &content);
                        return Some(dap_json::render_json(&notif).into_bytes());
                    }
                    return None;
                }
                "textDocument/didChange" => {
                    if let Some(params) = json.get("params") {
                        let uri = params
                            .get("textDocument")
                            .and_then(|d| d.get("uri"))
                            .and_then(|u| u.as_str())
                            .unwrap_or("")
                            .to_string();
                        if let Some(Json::Arr(changes)) = params.get("contentChanges")
                            && let Some(first) = changes.first()
                            && let Some(new_text) = first.get("text").and_then(|t| t.as_str())
                        {
                            self.documents.insert(uri.clone(), new_text.to_string());
                            let symbols = extract_document_symbols(new_text);
                            if !symbols.is_empty() {
                                self.last_valid_symbols.insert(uri.clone(), symbols);
                            }
                            let notif = create_publish_diagnostics(&uri, new_text);
                            return Some(dap_json::render_json(&notif).into_bytes());
                        }
                    }
                    return None;
                }
                "textDocument/didClose" => {
                    if let Some(params) = json.get("params")
                        && let Some(doc) = params.get("textDocument")
                        && let Some(uri) = doc.get("uri").and_then(|u| u.as_str())
                    {
                        self.documents.remove(uri);
                        self.last_valid_symbols.remove(uri);
                        let notif = create_empty_publish_diagnostics(uri);
                        return Some(dap_json::render_json(&notif).into_bytes());
                    }
                    return None;
                }
                "textDocument/didSave" => {
                    if let Some(params) = json.get("params")
                        && let Some(doc) = params.get("textDocument")
                        && let Some(uri) = doc.get("uri").and_then(|u| u.as_str())
                    {
                        if let Some(text) = params.get("text").and_then(|t| t.as_str()) {
                            self.documents.insert(uri.to_string(), text.to_string());
                        }
                        if let Some(content) = self.documents.get(uri) {
                            let symbols = extract_document_symbols(content);
                            if !symbols.is_empty() {
                                self.last_valid_symbols.insert(uri.to_string(), symbols);
                            }
                            let notif = create_publish_diagnostics(uri, content);
                            return Some(dap_json::render_json(&notif).into_bytes());
                        }
                    }
                    return None;
                }
                "textDocument/formatting" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let uri = json
                        .get("params")
                        .and_then(|p| p.get("textDocument"))
                        .and_then(|d| d.get("uri"))
                        .and_then(|u| u.as_str())
                        .unwrap_or("");
                    let edits = if let Some(content) = self.documents.get(uri) {
                        if let Ok(formatted) = crate::fmt::format_source(content) {
                            let line_count = content.lines().count();
                            let (end_line, end_char) = if content.ends_with('\n')
                                || content.ends_with("\r\n")
                            {
                                (line_count as f64, 0.0)
                            } else {
                                let last_line = content.lines().last().unwrap_or("");
                                let last_len =
                                    last_line.chars().map(|c| c.len_utf16()).sum::<usize>() as f64;
                                (line_count.saturating_sub(1) as f64, last_len)
                            };

                            Json::Arr(vec![Json::Obj(vec![
                                (
                                    "range".to_string(),
                                    Json::Obj(vec![
                                        (
                                            "start".to_string(),
                                            Json::Obj(vec![
                                                ("line".to_string(), Json::Num(0.0)),
                                                ("character".to_string(), Json::Num(0.0)),
                                            ]),
                                        ),
                                        (
                                            "end".to_string(),
                                            Json::Obj(vec![
                                                ("line".to_string(), Json::Num(end_line)),
                                                ("character".to_string(), Json::Num(end_char)),
                                            ]),
                                        ),
                                    ]),
                                ),
                                ("newText".to_string(), Json::Str(formatted)),
                            ])])
                        } else {
                            Json::Arr(Vec::new())
                        }
                    } else {
                        Json::Arr(Vec::new())
                    };

                    let resp = json_response(req_id, edits);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "textDocument/hover" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let (uri, line, character) = parse_text_document_position(&json);
                    let cached = self.last_valid_symbols.get(uri).map(|s| s.as_slice());
                    let result = if let Some(content) = self.documents.get(uri) {
                        handle_hover(content, line, character, cached, api::api())
                    } else {
                        Json::Null
                    };

                    let resp = json_response(req_id, result);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "textDocument/completion" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let (uri, line, character) = parse_text_document_position(&json);
                    let cached = self.last_valid_symbols.get(uri).map(|s| s.as_slice());
                    let items = if let Some(content) = self.documents.get(uri) {
                        handle_completion(
                            content,
                            line,
                            character,
                            cached,
                            self.snippet_support,
                            api::api(),
                        )
                    } else {
                        Json::Arr(Vec::new())
                    };

                    let resp = json_response(req_id, items);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "textDocument/signatureHelp" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let (uri, line, character) = parse_text_document_position(&json);
                    let result = if let Some(content) = self.documents.get(uri) {
                        analysis::signature_help(content, line, character, api::api())
                    } else {
                        Json::Null
                    };

                    let resp = json_response(req_id, result);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "textDocument/definition" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let (uri, line, character) = parse_text_document_position(&json);
                    let cached = self.last_valid_symbols.get(uri).map(|s| s.as_slice());
                    let result = if let Some(content) = self.documents.get(uri) {
                        handle_definition(uri, content, line, character, cached)
                    } else {
                        Json::Null
                    };

                    let resp = json_response(req_id, result);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "textDocument/documentSymbol" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let uri = json
                        .get("params")
                        .and_then(|p| p.get("textDocument"))
                        .and_then(|d| d.get("uri"))
                        .and_then(|u| u.as_str())
                        .unwrap_or("");

                    let cached = self.last_valid_symbols.get(uri).map(|s| s.as_slice());
                    let result = if let Some(content) = self.documents.get(uri) {
                        handle_document_symbols(content, cached)
                    } else {
                        Json::Arr(Vec::new())
                    };

                    let resp = json_response(req_id, result);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                _ => {
                    if let Some(req_id) = id {
                        eprintln!("LSP unknown method: {}", method_name);
                        let resp = json_error(
                            req_id.clone(),
                            -32601,
                            &format!("Method '{}' not found", method_name),
                        );
                        return Some(dap_json::render_json(&resp).into_bytes());
                    }
                }
            }
        } else if let Some(req_id) = id {
            let resp = json_error(req_id.clone(), -32600, "Invalid Request");
            return Some(dap_json::render_json(&resp).into_bytes());
        }

        None
    }
}

fn json_response(id: Json, result: Json) -> Json {
    Json::Obj(vec![
        ("jsonrpc".to_string(), Json::Str("2.0".to_string())),
        ("id".to_string(), id),
        ("result".to_string(), result),
    ])
}

fn json_error(id: Json, code: i64, message: &str) -> Json {
    Json::Obj(vec![
        ("jsonrpc".to_string(), Json::Str("2.0".to_string())),
        ("id".to_string(), id),
        (
            "error".to_string(),
            Json::Obj(vec![
                ("code".to_string(), Json::Num(code as f64)),
                ("message".to_string(), Json::Str(message.to_string())),
            ]),
        ),
    ])
}

fn json_notification(method: &str, params: Json) -> Json {
    Json::Obj(vec![
        ("jsonrpc".to_string(), Json::Str("2.0".to_string())),
        ("method".to_string(), Json::Str(method.to_string())),
        ("params".to_string(), params),
    ])
}

/// Converts a 0-based character column to a 0-based UTF-16 code unit offset.
fn char_col_to_utf16_col(line: &str, char_col_0_based: usize) -> usize {
    line.chars()
        .take(char_col_0_based)
        .map(|c| c.len_utf16())
        .sum()
}

fn create_publish_diagnostics(uri: &str, content: &str) -> Json {
    let diags = crate::lint::lint_source(content);
    let mut diag_items = Vec::new();
    let doc_lines: Vec<&str> = content.lines().collect();

    for d in diags {
        let (start_line, start_char, end_line, end_char) = if let Some(ref l) = d.primary_label {
            let s_line = l.span.start.line.saturating_sub(1) as usize;
            let s_col = l.span.start.column.saturating_sub(1) as usize;
            let s_char = doc_lines
                .get(s_line)
                .map(|line| char_col_to_utf16_col(line, s_col))
                .unwrap_or(s_col);

            let e_line = l.span.end.line.saturating_sub(1) as usize;
            let e_col = l.span.end.column.saturating_sub(1) as usize;
            let e_char = doc_lines
                .get(e_line)
                .map(|line| char_col_to_utf16_col(line, e_col))
                .unwrap_or(e_col);

            (s_line as f64, s_char as f64, e_line as f64, e_char as f64)
        } else {
            (0.0, 0.0, 0.0, 0.0)
        };

        let severity = if d.severity == Severity::Error {
            1.0 // DiagnosticSeverity.Error
        } else {
            2.0 // DiagnosticSeverity.Warning
        };

        let mut fields = vec![
            (
                "range".to_string(),
                Json::Obj(vec![
                    (
                        "start".to_string(),
                        Json::Obj(vec![
                            ("line".to_string(), Json::Num(start_line)),
                            ("character".to_string(), Json::Num(start_char)),
                        ]),
                    ),
                    (
                        "end".to_string(),
                        Json::Obj(vec![
                            ("line".to_string(), Json::Num(end_line.max(start_line))),
                            (
                                "character".to_string(),
                                Json::Num(end_char.max(start_char + 1.0)),
                            ),
                        ]),
                    ),
                ]),
            ),
            ("severity".to_string(), Json::Num(severity)),
            ("message".to_string(), Json::Str(d.message)),
            ("source".to_string(), Json::Str("neyuki".to_string())),
        ];

        if let Some(code) = d.code {
            fields.push(("code".to_string(), Json::Str(code.as_str().to_string())));
        }

        diag_items.push(Json::Obj(fields));
    }

    json_notification(
        "textDocument/publishDiagnostics",
        Json::Obj(vec![
            ("uri".to_string(), Json::Str(uri.to_string())),
            ("diagnostics".to_string(), Json::Arr(diag_items)),
        ]),
    )
}

fn create_empty_publish_diagnostics(uri: &str) -> Json {
    json_notification(
        "textDocument/publishDiagnostics",
        Json::Obj(vec![
            ("uri".to_string(), Json::Str(uri.to_string())),
            ("diagnostics".to_string(), Json::Arr(Vec::new())),
        ]),
    )
}

fn get_word_at_position(content: &str, line_idx: usize, char_col: usize) -> Option<String> {
    let line = content.lines().nth(line_idx)?;
    let mut current_utf16_col = 0;
    let mut char_indices = Vec::new();

    for (byte_idx, ch) in line.char_indices() {
        let ch_utf16_len = ch.len_utf16();
        char_indices.push((
            byte_idx,
            ch,
            current_utf16_col,
            current_utf16_col + ch_utf16_len,
        ));
        current_utf16_col += ch_utf16_len;
    }

    let target = char_indices
        .iter()
        .find(|(_, _, start, end)| char_col >= *start && char_col <= *end)?;

    if !target.1.is_ascii_alphanumeric() && target.1 != '_' {
        return None;
    }

    let byte_pos = target.0;
    let bytes = line.as_bytes();
    let mut start = byte_pos;
    while start > 0
        && (bytes[start - 1].is_ascii_alphanumeric()
            || bytes[start - 1] == b'_'
            || bytes[start - 1] == b'.')
    {
        start -= 1;
    }
    let mut end = byte_pos + target.1.len_utf8();
    while end < bytes.len()
        && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_' || bytes[end] == b'.')
    {
        end += 1;
    }

    let word = std::str::from_utf8(&bytes[start..end])
        .ok()?
        .trim_matches('.');
    if word.is_empty() {
        None
    } else {
        Some(word.to_string())
    }
}

fn utf16_col_to_byte_offset(s: &str, utf16_col: usize) -> usize {
    let mut current_utf16 = 0;
    for (byte_offset, ch) in s.char_indices() {
        if current_utf16 >= utf16_col {
            return byte_offset;
        }
        current_utf16 += ch.len_utf16();
    }
    s.len()
}

fn get_ident_at_position(content: &str, line_idx: usize, char_col: usize) -> Option<String> {
    let line = content.lines().nth(line_idx)?;
    let mut current_utf16_col = 0;
    let mut char_indices = Vec::new();

    for (byte_idx, ch) in line.char_indices() {
        let ch_utf16_len = ch.len_utf16();
        char_indices.push((
            byte_idx,
            ch,
            current_utf16_col,
            current_utf16_col + ch_utf16_len,
        ));
        current_utf16_col += ch_utf16_len;
    }

    let target = char_indices
        .iter()
        .find(|(_, _, start, end)| char_col >= *start && char_col <= *end)?;

    if !target.1.is_ascii_alphanumeric() && target.1 != '_' {
        return None;
    }

    let byte_pos = target.0;
    let bytes = line.as_bytes();
    let mut start = byte_pos;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    let mut end = byte_pos + target.1.len_utf8();
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }

    let word = std::str::from_utf8(&bytes[start..end]).ok()?;
    if word.is_empty() {
        None
    } else {
        Some(word.to_string())
    }
}

fn find_ident_in_line(line_text: &str, name: &str) -> Option<usize> {
    for (char_idx, (byte_idx, _)) in line_text.char_indices().enumerate() {
        if line_text[byte_idx..].starts_with(name) {
            let before_ok = byte_idx == 0
                || !line_text[..byte_idx]
                    .ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
            let after_idx = byte_idx + name.len();
            let after_ok = after_idx == line_text.len()
                || !line_text[after_idx..]
                    .starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
            if before_ok && after_ok {
                return Some(char_idx);
            }
        }
    }
    None
}

fn handle_hover(
    content: &str,
    line: usize,
    character: usize,
    cached_symbols: Option<&[FoundSymbol]>,
    src: &dyn MemberSource,
) -> Json {
    // The standard library first, then what the document declares.
    if let Some(hover) = analysis::hover(content, line, character, src) {
        return hover;
    }

    if let Some(ident) = get_ident_at_position(content, line, character) {
        let parsed_symbols = extract_document_symbols(content);
        let symbols = if !parsed_symbols.is_empty() {
            parsed_symbols
        } else if let Some(cached) = cached_symbols {
            cached.to_vec()
        } else {
            Vec::new()
        };

        if let Some(sym) = symbols.iter().find(|s| s.name == ident) {
            let markdown = format!("```neyuki\n{}\n```\n*Defined in this document*", sym.detail);
            return Json::Obj(vec![(
                "contents".to_string(),
                Json::Obj(vec![
                    ("kind".to_string(), Json::Str("markdown".to_string())),
                    ("value".to_string(), Json::Str(markdown)),
                ]),
            )]);
        }
    }

    Json::Null
}

fn handle_definition(
    uri: &str,
    content: &str,
    line: usize,
    character: usize,
    cached_symbols: Option<&[FoundSymbol]>,
) -> Json {
    let word = get_ident_at_position(content, line, character)
        .or_else(|| get_word_at_position(content, line, character));

    if let Some(word) = word {
        let parsed_symbols = extract_document_symbols(content);
        let symbols = if !parsed_symbols.is_empty() {
            parsed_symbols
        } else if let Some(cached) = cached_symbols {
            cached.to_vec()
        } else {
            Vec::new()
        };

        if let Some(sym) = symbols.iter().find(|s| s.name == word) {
            let line_0 = sym.line_1_based.saturating_sub(1) as f64;
            let doc_lines: Vec<&str> = content.lines().collect();
            let line_text = doc_lines
                .get(sym.line_1_based.saturating_sub(1) as usize)
                .unwrap_or(&"");

            let char_col = find_ident_in_line(line_text, &word).unwrap_or(0);
            let char_utf16 = char_col_to_utf16_col(line_text, char_col) as f64;
            let end_utf16 = char_utf16 + word.chars().map(|c| c.len_utf16()).sum::<usize>() as f64;

            return Json::Obj(vec![
                ("uri".to_string(), Json::Str(uri.to_string())),
                (
                    "range".to_string(),
                    Json::Obj(vec![
                        (
                            "start".to_string(),
                            Json::Obj(vec![
                                ("line".to_string(), Json::Num(line_0)),
                                ("character".to_string(), Json::Num(char_utf16)),
                            ]),
                        ),
                        (
                            "end".to_string(),
                            Json::Obj(vec![
                                ("line".to_string(), Json::Num(line_0)),
                                ("character".to_string(), Json::Num(end_utf16)),
                            ]),
                        ),
                    ]),
                ),
            ]);
        }
    }
    Json::Null
}

fn handle_document_symbols(content: &str, cached_symbols: Option<&[FoundSymbol]>) -> Json {
    let parsed_symbols = extract_document_symbols(content);
    let symbols = if !parsed_symbols.is_empty() {
        parsed_symbols
    } else if let Some(cached) = cached_symbols {
        cached.to_vec()
    } else {
        Vec::new()
    };

    let doc_lines: Vec<&str> = content.lines().collect();
    let mut symbol_objs = Vec::new();

    for sym in symbols {
        let line_0 = sym.line_1_based.saturating_sub(1) as f64;
        let line_text = doc_lines
            .get(sym.line_1_based.saturating_sub(1) as usize)
            .unwrap_or(&"");
        let line_len_utf16 = line_text.chars().map(|c| c.len_utf16()).sum::<usize>() as f64;

        let char_col = find_ident_in_line(line_text, &sym.name).unwrap_or(0);
        let char_utf16 = char_col_to_utf16_col(line_text, char_col) as f64;
        let end_utf16 = char_utf16 + sym.name.chars().map(|c| c.len_utf16()).sum::<usize>() as f64;

        let range = Json::Obj(vec![
            (
                "start".to_string(),
                Json::Obj(vec![
                    ("line".to_string(), Json::Num(line_0)),
                    ("character".to_string(), Json::Num(0.0)),
                ]),
            ),
            (
                "end".to_string(),
                Json::Obj(vec![
                    ("line".to_string(), Json::Num(line_0)),
                    ("character".to_string(), Json::Num(line_len_utf16)),
                ]),
            ),
        ]);

        let sel_range = Json::Obj(vec![
            (
                "start".to_string(),
                Json::Obj(vec![
                    ("line".to_string(), Json::Num(line_0)),
                    ("character".to_string(), Json::Num(char_utf16)),
                ]),
            ),
            (
                "end".to_string(),
                Json::Obj(vec![
                    ("line".to_string(), Json::Num(line_0)),
                    ("character".to_string(), Json::Num(end_utf16)),
                ]),
            ),
        ]);

        symbol_objs.push(Json::Obj(vec![
            ("name".to_string(), Json::Str(sym.name)),
            ("detail".to_string(), Json::Str(sym.detail)),
            ("kind".to_string(), Json::Num(sym.kind as f64)),
            ("range".to_string(), range),
            ("selectionRange".to_string(), sel_range),
        ]));
    }

    Json::Arr(symbol_objs)
}

/// Completion at a position. What the cursor follows decides the answer
/// (see `analysis::Context`); the members of namespaces and types come from
/// `src`, so another source of modules can be consulted next to the
/// standard library.
fn handle_completion(
    content: &str,
    line: usize,
    character: usize,
    cached_symbols: Option<&[FoundSymbol]>,
    snippets: bool,
    src: &dyn MemberSource,
) -> Json {
    let offset = analysis::offset_at(content, line, character);
    let context = analysis::completion_context(&content[..offset]);
    let binds = analysis::bindings(content, src);

    let mut items = Vec::new();
    match context {
        Context::None => {}
        Context::Module { prefix } => {
            let start = character.saturating_sub(prefix.len());
            for lib in src.modules() {
                if let Some(name) = lib.module.as_deref()
                    && name.starts_with(&prefix)
                {
                    items.push(analysis::library_item(
                        &lib,
                        name,
                        Some((line, start, character)),
                    ));
                }
            }
        }
        Context::Member { chain, prefix } => {
            let namespace = analysis::canonical(&chain, &binds);
            for entry in src.namespace_members(&namespace) {
                if analysis::short_name(&entry.name).starts_with(&prefix) {
                    items.push(analysis::entry_item(&entry, false, snippets));
                }
            }
        }
        Context::Method { receiver, prefix } => {
            for entry in analysis::methods_for(&receiver, &binds, src) {
                if analysis::short_name(&entry.name).starts_with(&prefix) {
                    items.push(analysis::entry_item(&entry, true, snippets));
                }
            }
        }
        Context::Type { prefix } => {
            for name in analysis::TYPES {
                if name.starts_with(&prefix) {
                    items.push(analysis::word_item(name, "type"));
                }
            }
            for name in src.type_names() {
                if name.starts_with(&prefix) {
                    items.push(analysis::word_item(&name, "class"));
                }
            }
        }
        Context::Global { prefix } => {
            // Declarations of the document come first, then what is always
            // in scope: globals, standard libraries and keywords.
            let parsed_symbols = extract_document_symbols(content);
            let symbols = if !parsed_symbols.is_empty() {
                parsed_symbols
            } else if let Some(cached) = cached_symbols {
                cached.to_vec()
            } else {
                Vec::new()
            };

            let mut declared = Vec::new();
            for sym in symbols {
                if sym.name.starts_with(&prefix) {
                    items.push(with_sort_text(
                        make_completion_item(&sym.name, &sym.detail, 6),
                        format!("0{}", sym.name),
                    ));
                }
                declared.push(sym.name);
            }
            for entry in src.globals() {
                let name = analysis::short_name(&entry.name);
                if name.starts_with(&prefix) && !declared.iter().any(|d| d == name) {
                    items.push(with_sort_text(
                        analysis::entry_item(&entry, false, snippets),
                        format!("1{name}"),
                    ));
                }
            }
            for lib in src.standard_namespaces() {
                if let Some(name) = lib.module.as_deref()
                    && name.starts_with(&prefix)
                {
                    items.push(with_sort_text(
                        analysis::library_item(&lib, name, None),
                        format!("1{name}"),
                    ));
                }
            }
            for keyword in analysis::KEYWORDS {
                if keyword.starts_with(&prefix) {
                    items.push(with_sort_text(
                        analysis::word_item(keyword, "keyword"),
                        format!("2{keyword}"),
                    ));
                }
            }
        }
    }

    Json::Arr(items)
}

/// Adds the key clients order completion items by.
fn with_sort_text(item: Json, sort_text: String) -> Json {
    match item {
        Json::Obj(mut fields) => {
            fields.push(("sortText".to_string(), Json::Str(sort_text)));
            Json::Obj(fields)
        }
        other => other,
    }
}

fn make_completion_item(label: &str, detail: &str, kind: usize) -> Json {
    Json::Obj(vec![
        ("label".to_string(), Json::Str(label.to_string())),
        ("detail".to_string(), Json::Str(detail.to_string())),
        ("kind".to_string(), Json::Num(kind as f64)),
    ])
}

fn collect_symbols_from_stmts(
    stmts: &[crate::ast::Stmt],
    spans: &crate::ast::span::SpanPool,
    out: &mut Vec<FoundSymbol>,
) {
    for stmt in stmts {
        match stmt {
            crate::ast::Stmt::Function {
                name: Some(name),
                params,
                body,
                id,
                ..
            } => {
                let span = spans.get(*id);
                let (line_1, col_1) = if let Some(s) = span {
                    (s.start.line, s.start.column)
                } else {
                    (1, 1)
                };
                let param_str = params
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push(FoundSymbol {
                    name: name.clone(),
                    detail: format!("function {}({})", name, param_str),
                    kind: 12, // Function
                    line_1_based: line_1,
                    col_1_based: col_1,
                });
                collect_symbols_from_stmts(body, spans, out);
            }
            crate::ast::Stmt::Function {
                name: None, body, ..
            } => {
                collect_symbols_from_stmts(body, spans, out);
            }
            crate::ast::Stmt::Local {
                name,
                type_name,
                id,
                initializer,
                ..
            } => {
                // No span: a binding `--!importall` added, not one in the file.
                let Some(s) = spans.get(*id) else { continue };
                let (line_1, col_1) = (s.start.line, s.start.column);
                let ty = type_name.as_deref().unwrap_or("any");
                out.push(FoundSymbol {
                    name: name.clone(),
                    detail: format!("local {}: {}", name, ty),
                    kind: 13, // Variable
                    line_1_based: line_1,
                    col_1_based: col_1,
                });
                if let Some(init) = initializer {
                    collect_symbols_from_expr(init, spans, out);
                }
            }
            crate::ast::Stmt::LocalMany {
                names,
                id,
                initializers,
                ..
            } => {
                let span = spans.get(*id);
                let (line_1, col_1) = if let Some(s) = span {
                    (s.start.line, s.start.column)
                } else {
                    (1, 1)
                };
                for name in names {
                    out.push(FoundSymbol {
                        name: name.clone(),
                        detail: format!("local {}", name),
                        kind: 13,
                        line_1_based: line_1,
                        col_1_based: col_1,
                    });
                }
                for init in initializers {
                    collect_symbols_from_expr(init, spans, out);
                }
            }
            crate::ast::Stmt::If { data, .. } => {
                collect_symbols_from_stmts(&data.then_branch, spans, out);
                for (_, branch) in &data.else_if_branches {
                    collect_symbols_from_stmts(branch, spans, out);
                }
                if let Some(ref else_branch) = data.else_branch {
                    collect_symbols_from_stmts(else_branch, spans, out);
                }
            }
            crate::ast::Stmt::For { body, .. } => {
                collect_symbols_from_stmts(body, spans, out);
            }
            crate::ast::Stmt::NumericFor { data, .. } => {
                collect_symbols_from_stmts(&data.body, spans, out);
            }
            crate::ast::Stmt::While { body, .. } => {
                collect_symbols_from_stmts(body, spans, out);
            }
            crate::ast::Stmt::Repeat { body, .. } => {
                collect_symbols_from_stmts(body, spans, out);
            }
            crate::ast::Stmt::Expr { expr, .. } => {
                collect_symbols_from_expr(expr, spans, out);
            }
            crate::ast::Stmt::Assign { value, .. } => {
                collect_symbols_from_expr(value, spans, out);
            }
            crate::ast::Stmt::AssignMany { values, .. } => {
                for v in values {
                    collect_symbols_from_expr(v, spans, out);
                }
            }
            crate::ast::Stmt::TypeAlias {
                name,
                exported,
                ty,
                name_span,
                ..
            } => {
                out.push(FoundSymbol {
                    name: name.clone(),
                    detail: format!(
                        "{}type {} = {}",
                        if *exported { "export " } else { "" },
                        name,
                        ty
                    ),
                    kind: 26, // TypeParameter, the closest kind to a type alias
                    line_1_based: name_span.start.line,
                    col_1_based: name_span.start.column,
                });
            }
            _ => {}
        }
    }
}

fn collect_symbols_from_expr(
    expr: &crate::ast::Expr,
    spans: &crate::ast::span::SpanPool,
    out: &mut Vec<FoundSymbol>,
) {
    if let crate::ast::Expr::Function { body, .. } = expr {
        collect_symbols_from_stmts(body, spans, out);
    }
}

fn extract_document_symbols(content: &str) -> Vec<FoundSymbol> {
    let mut symbols = Vec::new();
    if let Ok(parsed) = crate::compiler::parse(content) {
        collect_symbols_from_stmts(&parsed.stmts, &parsed.spans, &mut symbols);
    }
    symbols
}

/// Runs the LSP server listening on stdio.
pub fn run_lsp_server() -> io::Result<()> {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let mut stdout = io::stdout().lock();
    let mut server = LspServer::new();

    loop {
        match dap_json::read_message(&mut reader) {
            Ok(Some(msg_bytes)) => {
                if let Some(resp_bytes) = server.handle_message(&msg_bytes) {
                    dap_json::write_message(&mut stdout, &resp_bytes)?;
                    stdout.flush()?;
                }
                if server.is_exit {
                    let code = server.exit_code.unwrap_or(0);
                    drop(stdout);
                    if code != 0 {
                        std::process::exit(code);
                    }
                    return Ok(());
                }
            }
            Ok(None) => break,
            Err(e) => {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    break;
                }
                eprintln!("LSP IO error: {}", e);
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::Value;

    #[test]
    fn test_lsp_initialize_capabilities() {
        let mut server = LspServer::new();
        let init_req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = server.handle_message(init_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"capabilities\""));
        assert!(resp_str.contains("\"documentFormattingProvider\":true"));
        assert!(resp_str.contains("\"hoverProvider\":true"));
        assert!(resp_str.contains("\"completionProvider\""));
        assert!(resp_str.contains("\"definitionProvider\":true"));
        assert!(resp_str.contains("\"documentSymbolProvider\":true"));
        assert!(resp_str.contains("\"signatureHelpProvider\""));
    }

    #[test]
    fn test_lsp_initialize_reports_server_info() {
        let mut server = LspServer::new();
        let init_req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = server.handle_message(init_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains(&format!(
            "\"serverInfo\":{{\"name\":\"neyuki\",\"version\":\"{}\"}}",
            env!("CARGO_PKG_VERSION")
        )));
    }

    #[test]
    fn test_lsp_shutdown_and_exit_sequence() {
        let mut server = LspServer::new();
        let shutdown_req = r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#;
        let resp = server.handle_message(shutdown_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"result\":null"));
        assert!(server.is_shutdown);

        // Requests after shutdown should return error
        let invalid_req =
            r#"{"jsonrpc":"2.0","id":3,"method":"textDocument/formatting","params":{}}"#;
        let err_resp = server.handle_message(invalid_req.as_bytes()).unwrap();
        let err_str = String::from_utf8(err_resp).unwrap();
        assert!(err_str.contains("\"code\":-32600"));

        // Exit notification after shutdown
        let exit_notif = r#"{"jsonrpc":"2.0","method":"exit"}"#;
        let exit_resp = server.handle_message(exit_notif.as_bytes());
        assert!(exit_resp.is_none());
        assert!(server.is_exit);
        assert_eq!(server.exit_code, Some(0));
    }

    #[test]
    fn test_lsp_exit_without_shutdown_sets_exit_code_1() {
        let mut server = LspServer::new();
        let exit_notif = r#"{"jsonrpc":"2.0","method":"exit"}"#;
        let exit_resp = server.handle_message(exit_notif.as_bytes());
        assert!(exit_resp.is_none());
        assert!(server.is_exit);
        assert_eq!(server.exit_code, Some(1));
    }

    #[test]
    fn test_lsp_publish_diagnostics_on_open() {
        let mut server = LspServer::new();
        let open_req = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.nyk","text":"local unused = 10\n"}}}"#;
        let resp = server.handle_message(open_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("textDocument/publishDiagnostics"));
        assert!(resp_str.contains("W0001")); // unused variable warning
        assert!(server.documents.contains_key("file:///test.nyk"));
    }

    #[test]
    fn test_lsp_did_change() {
        let mut server = LspServer::new();
        let open_req = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.nyk","text":"local a = 1\n"}}}"#;
        let _ = server.handle_message(open_req.as_bytes());

        let change_req = r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///test.nyk"},"contentChanges":[{"text":"local b = 2\n"}]}}"#;
        let resp = server.handle_message(change_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("textDocument/publishDiagnostics"));
        assert_eq!(
            server.documents.get("file:///test.nyk"),
            Some(&"local b = 2\n".to_string())
        );
    }

    #[test]
    fn test_lsp_did_close_cleans_document_and_diagnostics() {
        let mut server = LspServer::new();
        let open_req = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.nyk","text":"local a = 1\n"}}}"#;
        let _ = server.handle_message(open_req.as_bytes());
        assert!(server.documents.contains_key("file:///test.nyk"));

        let close_req = r#"{"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":"file:///test.nyk"}}}"#;
        let resp = server.handle_message(close_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"diagnostics\":[]"));
        assert!(!server.documents.contains_key("file:///test.nyk"));
    }

    #[test]
    fn test_lsp_formatting() {
        let mut server = LspServer::new();
        let open_req = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.nyk","text":"function foo()\nreturn 1\nend\n"}}}"#;
        let _ = server.handle_message(open_req.as_bytes());

        let format_req = r#"{"jsonrpc":"2.0","id":10,"method":"textDocument/formatting","params":{"textDocument":{"uri":"file:///test.nyk"}}}"#;
        let resp = server.handle_message(format_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"newText\":\"function foo()\\n    return 1\\nend\\n\""));
        assert!(!resp_str.contains("100000"));
    }

    #[test]
    fn test_lsp_hover_standard_library_and_locals() {
        let mut server = LspServer::new();
        let open_req = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.nyk","text":"local x = 1\nprint(x)\n"}}}"#;
        let _ = server.handle_message(open_req.as_bytes());

        // Hover on print (line 1, col 2)
        let hover_req = r#"{"jsonrpc":"2.0","id":20,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///test.nyk"},"position":{"line":1,"character":2}}}"#;
        let resp = server.handle_message(hover_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("print(...: any): ()"));
        assert!(resp_str.contains("Writes its arguments to standard output"));
    }

    #[test]
    fn test_lsp_completion_globals_and_modules() {
        let mut server = LspServer::new();
        let open_req = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.nyk","text":"os.\n"}}}"#;
        let _ = server.handle_message(open_req.as_bytes());

        let comp_req = r#"{"jsonrpc":"2.0","id":30,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///test.nyk"},"position":{"line":0,"character":3}}}"#;
        let resp = server.handle_message(comp_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"label\":\"run\""));
        assert!(resp_str.contains("\"label\":\"exit\""));
    }

    #[test]
    fn test_lsp_document_symbols() {
        let mut server = LspServer::new();
        let open_req = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.nyk","text":"local a = 1\nfunction compute(x)\nreturn x * 2\nend\n"}}}"#;
        let _ = server.handle_message(open_req.as_bytes());

        let sym_req = r#"{"jsonrpc":"2.0","id":40,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":"file:///test.nyk"}}}"#;
        let resp = server.handle_message(sym_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"name\":\"compute\""));
        assert!(resp_str.contains("\"name\":\"a\""));
    }

    #[test]
    fn test_lsp_unknown_request_returns_error() {
        let mut server = LspServer::new();
        let req = r#"{"jsonrpc":"2.0","id":42,"method":"custom/unknownMethod","params":{}}"#;
        let resp = server.handle_message(req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"code\":-32601"));
        assert!(resp_str.contains("Method 'custom/unknownMethod' not found"));
    }

    #[test]
    fn test_lsp_malformed_json_returns_error() {
        let mut server = LspServer::new();
        let bad_bytes = b"not a json";
        let resp = server.handle_message(bad_bytes).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"code\":-32700"));
        assert!(resp_str.contains("Parse error"));
    }

    #[test]
    fn test_lsp_diagnostics_utf16_positions() {
        let s = "local 🦀_unused = 1\n";
        let col_chars = 7;
        let col_utf16 = char_col_to_utf16_col(s, col_chars);
        assert_eq!(col_utf16, 8);
    }

    #[test]
    fn test_lsp_completion_non_ascii() {
        let mut server = LspServer::new();
        let content = "local 🦀_unused = 1\nlocal é_var = 2\n🦀";
        let open_req = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///test.nyk","text":{}}}}}}}"#,
            dap_json::render_json(&Json::Str(content.to_string()))
        );
        let _ = server.handle_message(open_req.as_bytes());

        // Offset right after the crab emoji (line 0, UTF-16 character offset 8)
        let comp_req = r#"{"jsonrpc":"2.0","id":50,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///test.nyk"},"position":{"line":0,"character":8}}}"#;
        let resp = server.handle_message(comp_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"result\""));
    }

    #[test]
    fn test_lsp_completion_prefix_filtering() {
        let mut server = LspServer::new();
        let open_req = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///test.nyk","text":"fs.re\n"}}}"#;
        let _ = server.handle_message(open_req.as_bytes());

        // Position at line 0, character 5 (after "fs.re")
        let comp_req = r#"{"jsonrpc":"2.0","id":51,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///test.nyk"},"position":{"line":0,"character":5}}}"#;
        let resp = server.handle_message(comp_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"label\":\"read\""));
        assert!(resp_str.contains("\"label\":\"remove\""));
        assert!(resp_str.contains("\"label\":\"rename\""));
        assert!(!resp_str.contains("\"label\":\"write\""));
        assert!(!resp_str.contains("\"label\":\"mkdir\""));
    }

    #[test]
    fn test_lsp_definition_nested_and_exact_column() {
        let mut server = LspServer::new();
        let text = "function compute()\n    local secret_val = 42\n    if secret_val > 0 then\n        print(secret_val)\n    end\nend\n";
        let open_req = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///test.nyk","text":{}}}}}}}"#,
            dap_json::render_json(&Json::Str(text.to_string()))
        );
        let _ = server.handle_message(open_req.as_bytes());

        // Position on secret_val inside print(secret_val) -> line 3, character 16
        let def_req = r#"{"jsonrpc":"2.0","id":52,"method":"textDocument/definition","params":{"textDocument":{"uri":"file:///test.nyk"},"position":{"line":3,"character":16}}}"#;
        let resp = server.handle_message(def_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"line\":1"));
        // "    local secret_val" has column 10
        assert!(resp_str.contains("\"character\":10"));
    }

    #[test]
    fn test_lsp_hover_nested_function_and_user_symbols() {
        let mut server = LspServer::new();
        let text = "function calculate_sum(a, b)\n    return a + b\nend\nlocal total = calculate_sum(1, 2)\n";
        let open_req = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///test.nyk","text":{}}}}}}}"#,
            dap_json::render_json(&Json::Str(text.to_string()))
        );
        let _ = server.handle_message(open_req.as_bytes());

        // Hover over calculate_sum at line 3, col 16
        let hover_req = r#"{"jsonrpc":"2.0","id":53,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///test.nyk"},"position":{"line":3,"character":16}}}"#;
        let resp = server.handle_message(hover_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("calculate_sum"));
        assert!(resp_str.contains("function calculate_sum(a, b)"));
    }

    #[test]
    fn test_lsp_symbols_retained_on_syntax_error() {
        let mut server = LspServer::new();
        let valid_text = "function solve(x)\n    return x * 2\nend\n";
        let open_req = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///test.nyk","text":{}}}}}}}"#,
            dap_json::render_json(&Json::Str(valid_text.to_string()))
        );
        let _ = server.handle_message(open_req.as_bytes());

        // Now introduce a syntax error via didChange
        let broken_text = "function solve(x)\n    return x *\n";
        let change_req = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":"file:///test.nyk"}},"contentChanges":[{{"text":{}}}]}}}}"#,
            dap_json::render_json(&Json::Str(broken_text.to_string()))
        );
        let _ = server.handle_message(change_req.as_bytes());

        // Document symbols should still return cached symbols from the last valid parse
        let sym_req = r#"{"jsonrpc":"2.0","id":54,"method":"textDocument/documentSymbol","params":{"textDocument":{"uri":"file:///test.nyk"}}}"#;
        let resp = server.handle_message(sym_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"name\":\"solve\""));
    }

    /// Opens `text` as `file:///test.nyk`.
    fn open_document(server: &mut LspServer, text: &str) {
        let open_req = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///test.nyk","text":{}}}}}}}"#,
            dap_json::render_json(&Json::Str(text.to_string()))
        );
        let _ = server.handle_message(open_req.as_bytes());
    }

    /// Sends a request that takes a position and returns the response.
    fn position_request(
        server: &mut LspServer,
        method: &str,
        line: usize,
        character: usize,
    ) -> String {
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":100,"method":"{}","params":{{"textDocument":{{"uri":"file:///test.nyk"}},"position":{{"line":{},"character":{}}}}}}}"#,
            method, line, character
        );
        let resp = server.handle_message(req.as_bytes()).unwrap();
        String::from_utf8(resp).unwrap()
    }

    /// The line and UTF-16 column at the end of `text`.
    fn end_position(text: &str) -> (usize, usize) {
        let line = text.split('\n').count() - 1;
        let last = text.split('\n').next_back().unwrap_or("");
        (line, char_col_to_utf16_col(last, last.chars().count()))
    }

    /// Opens `text` and sends `method` with the cursor at its end.
    fn query_at_end(text: &str, method: &str) -> String {
        let mut server = LspServer::new();
        open_document(&mut server, text);
        let (line, character) = end_position(text);
        position_request(&mut server, method, line, character)
    }

    fn complete_at_end(text: &str) -> String {
        query_at_end(text, "textDocument/completion")
    }

    #[test]
    fn test_lsp_completion_through_require_binding() {
        let resp = complete_at_end("local g = require(\"@neyuki/ui\")\ng.");
        assert!(resp.contains("\"label\":\"open\""));
        assert!(resp.contains("\"label\":\"isAvailable\""));
        // Methods of Window belong after `w:`, not after `ui.`.
        assert!(!resp.contains("\"label\":\"fillRect\""));

        let resp = complete_at_end("local m = require(\"@neyuki/math\")\nm.fl");
        assert!(resp.contains("\"label\":\"floor\""));
        assert!(!resp.contains("\"label\":\"ceil\""));
    }

    #[test]
    fn test_lsp_completion_of_nested_namespace() {
        let resp = complete_at_end("local hid = require(\"@neyuki/hid\")\nhid.");
        assert!(resp.contains("\"label\":\"keyboard\""));
        let resp = complete_at_end("hid.keyboard.pr");
        assert!(resp.contains("\"label\":\"press\""));
        assert!(!resp.contains("\"label\":\"tap\""));
    }

    #[test]
    fn test_lsp_completion_importall_binds_bundled_modules() {
        let resp = complete_at_end("--!importall\nlocal w = ui.open({})\nw:");
        assert!(resp.contains("\"label\":\"fillRect\""));
        assert!(resp.contains("\"label\":\"close\""));
        // The type of `w` is known, so strings are not guessed.
        assert!(!resp.contains("\"label\":\"upper\""));
    }

    #[test]
    fn test_lsp_method_completion_from_known_types() {
        // The receiver is the result of a known call through a require alias.
        let resp = complete_at_end("local g = require(\"@neyuki/ui\")\nlocal w = g.open({})\nw:cl");
        assert!(resp.contains("\"label\":\"clear\""));
        assert!(resp.contains("\"label\":\"close\""));
        assert!(!resp.contains("\"label\":\"fillRect\""));

        // The receiver is annotated.
        let resp = complete_at_end("local win: Window? = nil\nwin:");
        assert!(resp.contains("\"label\":\"fillRect\""));
        assert!(!resp.contains("\"label\":\"upper\""));

        // A string literal, directly or through a variable: the string
        // functions without their first parameter.
        let resp = complete_at_end("local s = \"abc\"\ns:");
        assert!(resp.contains("\"detail\":\"sub(i: number, j: number?): string\""));
        let resp = complete_at_end("(\"abc\"):up");
        assert!(resp.contains("\"label\":\"upper\""));
        assert!(!resp.contains("\"label\":\"lower\""));
    }

    #[test]
    fn test_lsp_completion_inside_strings_and_comments_is_empty() {
        assert!(complete_at_end("print(\"fs.").contains("\"result\":[]"));
        assert!(complete_at_end("-- math.").contains("\"result\":[]"));
        assert!(complete_at_end("local s = [[\nmath.").contains("\"result\":[]"));
        // Inside an interpolated expression it is code again.
        assert!(complete_at_end("local s = `{math.fl").contains("\"label\":\"floor\""));
    }

    #[test]
    fn test_lsp_completion_of_module_names_and_types() {
        let resp = complete_at_end("local x = require(\"@neyuki/f");
        assert!(resp.contains("\"label\":\"@neyuki/fs\""));
        assert!(resp.contains("\"newText\":\"@neyuki/fs\""));
        assert!(!resp.contains("\"label\":\"@neyuki/ui\""));

        let resp = complete_at_end("local x = require(\"ma");
        assert!(resp.contains("\"label\":\"math\""));

        let resp = complete_at_end("local w: Wi");
        assert!(resp.contains("\"label\":\"Window\""));
        assert!(!resp.contains("\"label\":\"number\""));
    }

    #[test]
    fn test_lsp_completion_of_names_in_scope() {
        // Declarations of the document come from the parser, so the text
        // has to parse: the cursor is after the `co` of a call.
        let mut server = LspServer::new();
        open_document(&mut server, "local counter = 1\nco()\n");
        let resp = position_request(&mut server, "textDocument/completion", 1, 2);
        assert!(resp.contains("\"label\":\"counter\""));
        assert!(resp.contains("\"label\":\"coroutine\""));
        let resp = complete_at_end("loc");
        assert!(resp.contains("\"label\":\"local\""));
        let resp = complete_at_end("pri");
        assert!(resp.contains("\"label\":\"print\""));
    }

    #[test]
    fn test_lsp_completion_items_carry_documentation_and_snippets() {
        let resp = complete_at_end("math.fl");
        assert!(resp.contains("\"detail\":\"floor(n: number): number\""));
        assert!(resp.contains("\"documentation\":{\"kind\":\"markdown\",\"value\":\"```neyuki"));
        // Snippets only when the client says it supports them.
        assert!(!resp.contains("insertText"));

        let mut server = LspServer::new();
        let init_req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{"textDocument":{"completion":{"completionItem":{"snippetSupport":true}}}}}}"#;
        let _ = server.handle_message(init_req.as_bytes());
        assert!(server.snippet_support);
        open_document(&mut server, "math.fl\nos.cl");
        let resp = position_request(&mut server, "textDocument/completion", 0, 7);
        assert!(resp.contains("\"insertText\":\"floor($0)\""));
        assert!(resp.contains("\"insertTextFormat\":2"));
        // A function without parameters puts the cursor after the parentheses.
        let resp = position_request(&mut server, "textDocument/completion", 1, 5);
        assert!(resp.contains("\"insertText\":\"clock()$0\""));
    }

    #[test]
    fn test_lsp_signature_help_active_parameter_with_nested_calls() {
        let text = "local ui = require(\"@neyuki/ui\")\nlocal w = ui.open({})\nw:fillRect(1, math.max(2, 3), ";
        let resp = query_at_end(text, "textDocument/signatureHelp");
        assert!(resp.contains(
            "\"label\":\"fillRect(x: number, y: number, w: number, h: number, color: any, options: table?)\""
        ));
        assert!(resp.contains("\"activeParameter\":2"));

        // Commas in strings and in brackets do not count.
        let resp = query_at_end(
            "string.find(\"a,b\", {1, 2}, ",
            "textDocument/signatureHelp",
        );
        assert!(resp.contains("\"activeParameter\":2"));

        // The innermost call is the one being typed.
        let resp = query_at_end("string.find(math.max(1, ", "textDocument/signatureHelp");
        assert!(resp.contains("\"label\":\"max(...: number): number\""));
        assert!(resp.contains("\"activeParameter\":0"));
    }

    #[test]
    fn test_lsp_signature_help_variadic_methods_and_no_call() {
        // A variadic parameter stays active.
        let resp = query_at_end("print(1, 2, 3, ", "textDocument/signatureHelp");
        assert!(resp.contains("\"label\":\"print(...: any)\""));
        assert!(resp.contains("\"activeParameter\":0"));

        // `s:sub(...)` leaves out the receiver.
        let resp = query_at_end("local s = \"abc\"\ns:sub(1, ", "textDocument/signatureHelp");
        assert!(resp.contains("\"label\":\"sub(i: number, j: number?): string\""));
        assert!(resp.contains("\"activeParameter\":1"));

        assert!(
            query_at_end("local x = 1", "textDocument/signatureHelp").contains("\"result\":null")
        );
        assert!(
            query_at_end("unknown(1, ", "textDocument/signatureHelp").contains("\"result\":null")
        );
    }

    #[test]
    fn test_lsp_hover_through_bindings() {
        let text = "local ui = require(\"@neyuki/ui\")\nlocal w = ui.open({})\nw:clear()\nprint(\"math.floor\")\n";
        let mut server = LspServer::new();
        open_document(&mut server, text);

        // A method of a known type.
        let resp = position_request(&mut server, "textDocument/hover", 2, 3);
        assert!(resp.contains("Window:clear(color: any?): ()"));
        // A bundled module.
        let resp = position_request(&mut server, "textDocument/hover", 1, 11);
        assert!(resp.contains("**@neyuki/ui**"));
        // Inside a string the words are not names.
        let resp = position_request(&mut server, "textDocument/hover", 3, 10);
        assert!(resp.contains("\"result\":null"));
    }

    #[test]
    fn test_lsp_utf16_positions_after_non_ascii_text() {
        // The crab is two UTF-16 units and four bytes, the accent one unit
        // and two bytes, so the columns differ from both counts.
        let before_member = "local s = \"\u{e9}\u{1f980}\"; ui.";
        let line = format!("{before_member}open({{}}) -- \u{65e5}\u{672c}");
        let text = format!("local ui = require(\"@neyuki/ui\")\n{line}\n");
        let column = char_col_to_utf16_col(before_member, before_member.chars().count());
        assert_ne!(column, before_member.chars().count());

        let mut server = LspServer::new();
        open_document(&mut server, &text);
        // Completion with the cursor right after `ui.`.
        let resp = position_request(&mut server, "textDocument/completion", 1, column);
        assert!(resp.contains("\"label\":\"open\""));
        // Hover inside `open`.
        let resp = position_request(&mut server, "textDocument/hover", 1, column + 2);
        assert!(resp.contains("ui.open(options: table?): Window"));
        // The hover range is in UTF-16 units as well.
        assert!(resp.contains(&format!("\"character\":{}", column)));
        assert!(resp.contains(&format!("\"character\":{}", column + 4)));
        // Signature help inside the parentheses.
        let resp = position_request(&mut server, "textDocument/signatureHelp", 1, column + 5);
        assert!(resp.contains("\"label\":\"open(options: table?): Window\""));
    }

    /// The value `root` holds at `path` (`read`, `keyboard.press`), nil if absent.
    fn exists_in(root: &Value, path: &str) -> bool {
        let mut current = root.clone();
        for part in path.split('.') {
            let next = match &current {
                Value::Table(table) => table.borrow().get_str(part),
                _ => return false,
            };
            current = next;
        }
        !matches!(current, Value::Nil)
    }

    /// What `require(module)` returns in a fresh VM.
    fn load_module(module: &str) -> Value {
        let source = format!("return require('{module}')");
        let proto =
            crate::compiler::compile_program(&source, "test", crate::compiler::Options::RUN)
                .expect("Failed to compile require script");
        let mut vm = crate::vm::machine::VM::new();
        vm.execute(proto).expect("Failed to execute require")
    }

    #[test]
    fn test_lsp_completion_items_match_runtime_tables() {
        let api = api::api();
        let vm = crate::vm::machine::VM::new();
        let standard: Vec<String> = api
            .standard_namespaces()
            .into_iter()
            .filter_map(|lib| lib.module)
            .collect();
        // The bundled modules that load without a desktop session or a web
        // engine. `ui`, `hid` and `webview` need one, so what their pages
        // document is not compared with the runtime here.
        let headless = ["crypto", "fs", "http", "io", "net", "profile", "sql"];
        let modules: HashMap<&str, Value> = headless
            .iter()
            .map(|name| (*name, load_module(&format!("@neyuki/{name}"))))
            .collect();

        let mut missing = Vec::new();
        for entry in &api.entries {
            // Methods belong to values a script gets back, not to a table.
            if entry.kind == api::EntryKind::Method {
                continue;
            }
            let page = entry.page.as_str();
            let path = entry.name.as_str();
            let present = if page == "globals" || standard.iter().any(|name| name == page) {
                // `math.floor` is the field `floor` of the global `math`.
                let (first, rest) = match path.split_once('.') {
                    Some((first, rest)) => (first, Some(rest)),
                    None => (path, None),
                };
                let root = vm
                    .globals
                    .get(&crate::vm::value::StrRef::from(first))
                    .cloned()
                    .unwrap_or(Value::Nil);
                match rest {
                    Some(rest) => exists_in(&root, rest),
                    None => !matches!(root, Value::Nil),
                }
            } else if let Some(module) = modules.get(page) {
                // `fs.read` is the field `read` of what `require` returns.
                let rest = path.split_once('.').map_or(path, |(_, rest)| rest);
                exists_in(module, rest)
            } else {
                continue;
            };
            if !present {
                missing.push(entry.name.clone());
            }
        }
        assert!(
            missing.is_empty(),
            "documented in docs/reference but absent at runtime: {missing:?}"
        );
    }
}
