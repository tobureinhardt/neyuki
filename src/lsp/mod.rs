//! Language Server Protocol (LSP) server for `neyuki lsp`, running over stdio.

use std::collections::HashMap;
use std::io::{self, BufReader, Write};

use crate::dap_json::{self, Json};
use crate::diagnostics::severity::Severity;

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
                    let resp = json_response(
                        req_id,
                        Json::Obj(vec![(
                            "capabilities".to_string(),
                            Json::Obj(vec![
                                ("textDocumentSync".to_string(), Json::Num(1.0)),
                                ("documentFormattingProvider".to_string(), Json::Bool(true)),
                                ("hoverProvider".to_string(), Json::Bool(true)),
                                (
                                    "completionProvider".to_string(),
                                    Json::Obj(vec![(
                                        "triggerCharacters".to_string(),
                                        Json::Arr(vec![
                                            Json::Str(".".to_string()),
                                            Json::Str(":".to_string()),
                                        ]),
                                    )]),
                                ),
                                ("definitionProvider".to_string(), Json::Bool(true)),
                                ("documentSymbolProvider".to_string(), Json::Bool(true)),
                            ]),
                        )]),
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
                        handle_hover(content, line, character, cached)
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
                        handle_completion(content, line, character, cached)
                    } else {
                        Json::Arr(Vec::new())
                    };

                    let resp = json_response(req_id, items);
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
) -> Json {
    if let Some(word) = get_word_at_position(content, line, character)
        && let Some((sig, desc)) = lookup_docs(&word)
    {
        let markdown = format!("```neyuki\n{}\n```\n{}", sig, desc);
        return Json::Obj(vec![(
            "contents".to_string(),
            Json::Obj(vec![
                ("kind".to_string(), Json::Str("markdown".to_string())),
                ("value".to_string(), Json::Str(markdown)),
            ]),
        )]);
    }

    if let Some(ident) = get_ident_at_position(content, line, character) {
        if let Some((sig, desc)) = lookup_docs(&ident) {
            let markdown = format!("```neyuki\n{}\n```\n{}", sig, desc);
            return Json::Obj(vec![(
                "contents".to_string(),
                Json::Obj(vec![
                    ("kind".to_string(), Json::Str("markdown".to_string())),
                    ("value".to_string(), Json::Str(markdown)),
                ]),
            )]);
        }

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

pub(crate) const OS_METHODS: &[(&str, &str, &str)] = &[
    (
        "clock",
        "os.clock(): float",
        "Returns an approximation of the amount in seconds of CPU time used by the program.",
    ),
    (
        "time",
        "os.time(): int",
        "Returns the current time in seconds since the UNIX epoch.",
    ),
    (
        "date",
        "os.date(format?: string, time?: int): string | table",
        "Formats date and time string or returns date table.",
    ),
    (
        "difftime",
        "os.difftime(t2: int, t1: int): int",
        "Returns difference in seconds between two timestamps.",
    ),
    (
        "getenv",
        "os.getenv(varname: string): string | nil",
        "Returns the value of the environment variable, or nil if undefined.",
    ),
    (
        "sleep",
        "os.sleep(seconds: number): void",
        "Suspends execution for specified duration in seconds.",
    ),
    (
        "exit",
        "os.exit(code?: int): void",
        "Terminates the host process with an exit status code.",
    ),
    (
        "args",
        "os.args: {string}",
        "Command-line arguments passed to the script.",
    ),
    (
        "run",
        "os.run(command: string, options?: { stdin?: string, timeout?: int }): { stdout: string, stderr: string, code: int, success: boolean }",
        "Executes an external system command.",
    ),
    (
        "app",
        "os.app(): { bundled: boolean, name: string?, version: string?, identifier: string? }",
        "Describes the app built with `neyuki build` that is running; `bundled` is false under `neyuki run`.",
    ),
    (
        "dataDir",
        "os.dataDir(appName: string): string",
        "The per-user directory for an app's own files, created if missing.",
    ),
    (
        "configDir",
        "os.configDir(appName: string): string",
        "The per-user directory for an app's settings, created if missing.",
    ),
    (
        "cacheDir",
        "os.cacheDir(appName: string): string",
        "The per-user directory for files an app can rebuild, created if missing.",
    ),
    (
        "protect",
        "os.protect(data: string): string",
        "Seals data so only the current user on this machine can read it back.",
    ),
    (
        "unprotect",
        "os.unprotect(blob: string): string",
        "The string os.protect sealed into blob.",
    ),
];

pub(crate) const STRING_METHODS: &[(&str, &str, &str)] = &[
    (
        "byte",
        "string.byte(s: string, i?: int, j?: int): ...int",
        "Returns internal numeric codes of characters.",
    ),
    (
        "char",
        "string.char(...codes: int): string",
        "Returns string with characters of given numeric codes.",
    ),
    (
        "count",
        "string.count(s: string, pattern: string): int",
        "Counts non-overlapping occurrences of pattern.",
    ),
    (
        "endswith",
        "string.endswith(s: string, suffix: string): boolean",
        "Returns true if string ends with suffix.",
    ),
    (
        "find",
        "string.find(s: string, pattern: string, init?: int, plain?: boolean): (int, int) | nil",
        "Searches for first match of pattern.",
    ),
    (
        "format",
        "string.format(fmt: string, ...args: any): string",
        "Formats values according to format specifier.",
    ),
    (
        "gmatch",
        "string.gmatch(s: string, pattern: string): () -> ...string",
        "Returns iterator over pattern matches.",
    ),
    (
        "gsub",
        "string.gsub(s: string, pattern: string, repl: any, n?: int): (string, int)",
        "Replaces matches of pattern.",
    ),
    (
        "len",
        "string.len(s: string): int",
        "Returns length of string in bytes.",
    ),
    (
        "lower",
        "string.lower(s: string): string",
        "Converts string to lowercase.",
    ),
    (
        "ltrim",
        "string.ltrim(s: string): string",
        "Trims leading whitespace.",
    ),
    (
        "match",
        "string.match(s: string, pattern: string, init?: int): any",
        "Extracts pattern captures from string.",
    ),
    (
        "pack",
        "string.pack(fmt: string, ...values: any): string",
        "Packs values into binary string according to format.",
    ),
    (
        "packsize",
        "string.packsize(fmt: string): int",
        "Returns size of binary string for given format.",
    ),
    (
        "rep",
        "string.rep(s: string, n: int, sep?: string): string",
        "Repeats string n times.",
    ),
    (
        "reverse",
        "string.reverse(s: string): string",
        "Reverses characters in string.",
    ),
    (
        "rtrim",
        "string.rtrim(s: string): string",
        "Trims trailing whitespace.",
    ),
    (
        "split",
        "string.split(s: string, sep?: string): {string}",
        "Splits string by separator into a list of strings.",
    ),
    (
        "startswith",
        "string.startswith(s: string, prefix: string): boolean",
        "Returns true if string starts with prefix.",
    ),
    (
        "sub",
        "string.sub(s: string, i: int, j?: int): string",
        "Returns substring from index i to j.",
    ),
    (
        "trim",
        "string.trim(s: string): string",
        "Trims leading and trailing whitespace.",
    ),
    (
        "unpack",
        "string.unpack(fmt: string, s: string, pos?: int): ...any",
        "Unpacks values from binary string according to format.",
    ),
    (
        "upper",
        "string.upper(s: string): string",
        "Converts string to uppercase.",
    ),
];

pub(crate) const TABLE_METHODS: &[(&str, &str, &str)] = &[
    (
        "clear",
        "table.clear(t: table): void",
        "Clears all elements in table.",
    ),
    (
        "clone",
        "table.clone(t: table): table",
        "Creates a shallow clone of table.",
    ),
    (
        "concat",
        "table.concat(list: table, sep?: string, i?: int, j?: int): string",
        "Concatenates table elements into a string.",
    ),
    (
        "create",
        "table.create(count: int, val?: any): table",
        "Creates table preallocated with count elements.",
    ),
    (
        "find",
        "table.find(t: table, val: any, init?: int): any",
        "Finds first index of value in table.",
    ),
    (
        "freeze",
        "table.freeze(t: table): table",
        "Freezes a table, making it read-only.",
    ),
    (
        "insert",
        "table.insert(list: table, [pos: int,] val: any): void",
        "Inserts value into list at position.",
    ),
    (
        "isfrozen",
        "table.isfrozen(t: table): boolean",
        "Returns true if table is frozen.",
    ),
    (
        "keys",
        "table.keys(t: table): {any}",
        "Returns list of all keys in table.",
    ),
    (
        "length",
        "table.length(t: table): int",
        "Returns number of elements in table.",
    ),
    (
        "move",
        "table.move(a1: table, f: int, e: int, t: int, a2?: table): table",
        "Moves range of elements.",
    ),
    (
        "pack",
        "table.pack(...args: any): table",
        "Packs arguments into a table with field n.",
    ),
    (
        "remove",
        "table.remove(list: table, pos?: int): any",
        "Removes element at pos from list.",
    ),
    (
        "sort",
        "table.sort(list: table, comp?: function): void",
        "Sorts table in place.",
    ),
    (
        "unpack",
        "table.unpack(list: table, i?: int, j?: int): ...any",
        "Unpacks elements of table into multiple values.",
    ),
    (
        "values",
        "table.values(t: table): {any}",
        "Returns list of all values in table.",
    ),
];

pub(crate) const MATH_METHODS: &[(&str, &str, &str)] = &[
    (
        "abs",
        "math.abs(x: number): number",
        "Returns absolute value.",
    ),
    ("acos", "math.acos(x: number): float", "Returns arc cosine."),
    ("asin", "math.asin(x: number): float", "Returns arc sine."),
    (
        "atan",
        "math.atan(x: number): float",
        "Returns arc tangent.",
    ),
    (
        "atan2",
        "math.atan2(y: number, x: number): float",
        "Returns arc tangent of y/x.",
    ),
    ("cbrt", "math.cbrt(x: number): float", "Returns cube root."),
    (
        "ceil",
        "math.ceil(x: number): int",
        "Returns smallest integer greater than or equal to x.",
    ),
    (
        "clamp",
        "math.clamp(x: number, min: number, max: number): number",
        "Clamps value between min and max.",
    ),
    ("cos", "math.cos(x: number): float", "Returns cosine."),
    (
        "cosh",
        "math.cosh(x: number): float",
        "Returns hyperbolic cosine.",
    ),
    (
        "deg",
        "math.deg(x: number): float",
        "Converts radians to degrees.",
    ),
    (
        "exp",
        "math.exp(x: number): float",
        "Returns e raised to power x.",
    ),
    (
        "floor",
        "math.floor(x: number): int",
        "Returns largest integer less than or equal to x.",
    ),
    (
        "fmod",
        "math.fmod(x: number, y: number): number",
        "Returns floating-point remainder of x/y.",
    ),
    (
        "frexp",
        "math.frexp(x: number): (float, int)",
        "Decomposes number into mantissa and exponent.",
    ),
    (
        "isfinite",
        "math.isfinite(x: number): boolean",
        "Returns true if value is finite.",
    ),
    (
        "isinf",
        "math.isinf(x: number): boolean",
        "Returns true if value is infinite.",
    ),
    (
        "isnan",
        "math.isnan(x: number): boolean",
        "Returns true if value is NaN.",
    ),
    (
        "ldexp",
        "math.ldexp(m: number, e: int): float",
        "Returns m * 2^e.",
    ),
    (
        "lerp",
        "math.lerp(a: number, b: number, t: number): number",
        "Linear interpolation between a and b.",
    ),
    (
        "log",
        "math.log(x: number, base?: number): float",
        "Returns logarithm of x.",
    ),
    (
        "log10",
        "math.log10(x: number): float",
        "Returns base-10 logarithm of x.",
    ),
    (
        "log2",
        "math.log2(x: number): float",
        "Returns base-2 logarithm of x.",
    ),
    (
        "map",
        "math.map(x: number, in_min: number, in_max: number, out_min: number, out_max: number): number",
        "Maps number from one range to another.",
    ),
    (
        "max",
        "math.max(...x: number): number",
        "Returns maximum value.",
    ),
    (
        "min",
        "math.min(...x: number): number",
        "Returns minimum value.",
    ),
    (
        "modf",
        "math.modf(x: number): (int, float)",
        "Splits number into integral and fractional parts.",
    ),
    (
        "noise",
        "math.noise(x: number, y?: number, z?: number): float",
        "Perlin noise generator.",
    ),
    (
        "pow",
        "math.pow(x: number, y: number): number",
        "Returns x raised to power y.",
    ),
    (
        "rad",
        "math.rad(x: number): float",
        "Converts degrees to radians.",
    ),
    (
        "random",
        "math.random(m?: int, n?: int): number",
        "Generates pseudo-random number.",
    ),
    (
        "randomseed",
        "math.randomseed(x: int): void",
        "Sets random generator seed.",
    ),
    (
        "round",
        "math.round(x: number): int",
        "Rounds to nearest integer.",
    ),
    (
        "sign",
        "math.sign(x: number): int",
        "Returns sign of x (-1, 0, or 1).",
    ),
    ("sin", "math.sin(x: number): float", "Returns sine."),
    (
        "sinh",
        "math.sinh(x: number): float",
        "Returns hyperbolic cosine.",
    ),
    (
        "sqrt",
        "math.sqrt(x: number): float",
        "Returns square root.",
    ),
    ("tan", "math.tan(x: number): float", "Returns tangent."),
    (
        "tanh",
        "math.tanh(x: number): float",
        "Returns hyperbolic tangent.",
    ),
    (
        "trunc",
        "math.trunc(x: number): int",
        "Truncates toward zero.",
    ),
    ("huge", "math.huge: float", "Positive infinity."),
    ("pi", "math.pi: float", "Value of Pi."),
    ("tau", "math.tau: float", "Value of Tau (2 * Pi)."),
    ("e", "math.e: float", "Euler's number."),
    ("phi", "math.phi: float", "Golden ratio."),
    ("sqrt2", "math.sqrt2: float", "Square root of 2."),
    ("nan", "math.nan: float", "NaN value."),
];

pub(crate) const TASK_METHODS: &[(&str, &str, &str)] = &[
    (
        "spawn",
        "task.spawn(f: function | thread, ...args: any): thread",
        "Runs f as a task right away, until it first waits or yields.",
    ),
    (
        "defer",
        "task.defer(f: function | thread, ...args: any): thread",
        "Runs f as a task on the next scheduler pass.",
    ),
    (
        "delay",
        "task.delay(seconds: number, f: function | thread, ...args: any): thread",
        "Runs f as a task once seconds have passed.",
    ),
    (
        "wait",
        "task.wait(seconds?: number): number",
        "Suspends the current task for seconds and returns the time actually waited.",
    ),
    (
        "cancel",
        "task.cancel(thread: thread)",
        "Unschedules a task and closes its coroutine.",
    ),
];

pub(crate) const COROUTINE_METHODS: &[(&str, &str, &str)] = &[
    (
        "create",
        "coroutine.create(f: function): thread",
        "Creates a new coroutine.",
    ),
    (
        "resume",
        "coroutine.resume(co: thread, ...args: any): (boolean, ...any)",
        "Resumes coroutine execution.",
    ),
    (
        "yield",
        "coroutine.yield(...values: any): ...any",
        "Suspends coroutine execution.",
    ),
    (
        "status",
        "coroutine.status(co: thread): string",
        "Returns coroutine status.",
    ),
    (
        "running",
        "coroutine.running(): thread | nil",
        "Returns currently running coroutine.",
    ),
    (
        "wrap",
        "coroutine.wrap(f: function): function",
        "Creates function that resumes coroutine on each call.",
    ),
    (
        "isyieldable",
        "coroutine.isyieldable(): boolean",
        "Returns true if coroutine can yield.",
    ),
    (
        "close",
        "coroutine.close(co: thread): (boolean, string?)",
        "Closes coroutine, releasing resources.",
    ),
];

pub(crate) const JSON_METHODS: &[(&str, &str, &str)] = &[
    (
        "encode",
        "json.encode(val: any): string",
        "Encodes Neyuki value to JSON string.",
    ),
    (
        "decode",
        "json.decode(s: string): any",
        "Decodes JSON string to Neyuki value.",
    ),
    (
        "array",
        "json.array(t: table): table",
        "Marks table to serialize as JSON array.",
    ),
    ("null", "json.null: any", "JSON null sentinel value."),
];

pub(crate) const FS_METHODS: &[(&str, &str, &str)] = &[
    (
        "open",
        "fs.open(path: string, mode?: string): File",
        "Opens a file handle.",
    ),
    (
        "read",
        "fs.read(path: string): string",
        "Reads file contents as a string.",
    ),
    (
        "write",
        "fs.write(path: string, text: string): void",
        "Writes text to file.",
    ),
    (
        "append",
        "fs.append(path: string, text: string): void",
        "Appends text to file.",
    ),
    (
        "lines",
        "fs.lines(path: string): () -> string?",
        "Iterates over lines in file.",
    ),
    (
        "exists",
        "fs.exists(path: string): boolean",
        "Returns true if file exists.",
    ),
    (
        "isFile",
        "fs.isFile(path: string): boolean",
        "Returns true if path is a regular file.",
    ),
    (
        "isDir",
        "fs.isDir(path: string): boolean",
        "Returns true if path is a directory.",
    ),
    (
        "info",
        "fs.info(path: string): FileInfo?",
        "Returns file information.",
    ),
    (
        "list",
        "fs.list(path: string): {string}",
        "Lists directory contents.",
    ),
    (
        "walk",
        "fs.walk(path: string): {string}",
        "Recursively walks directory.",
    ),
    (
        "mkdir",
        "fs.mkdir(path: string, recursive?: boolean): void",
        "Creates directory.",
    ),
    (
        "remove",
        "fs.remove(path: string, recursive?: boolean): void",
        "Removes file or directory.",
    ),
    (
        "rename",
        "fs.rename(from: string, to: string): void",
        "Renames file or directory.",
    ),
    (
        "copy",
        "fs.copy(from: string, to: string): number",
        "Copies file to destination.",
    ),
    (
        "join",
        "fs.join(...parts: string): string",
        "Joins path components.",
    ),
    (
        "basename",
        "fs.basename(path: string): string",
        "Returns base name.",
    ),
    (
        "dirname",
        "fs.dirname(path: string): string",
        "Returns directory name.",
    ),
    (
        "extension",
        "fs.extension(path: string): string",
        "Returns file extension.",
    ),
    (
        "absolute",
        "fs.absolute(path: string): string",
        "Returns absolute path.",
    ),
    (
        "cwd",
        "fs.cwd(): string",
        "Returns current working directory.",
    ),
];

pub(crate) const CRYPTO_METHODS: &[(&str, &str, &str)] = &[
    (
        "hash",
        "crypto.hash(s: string, algorithm?: string, encoding?: string): string",
        "Computes hash of string.",
    ),
    (
        "hmac",
        "crypto.hmac(s: string, key: string, algorithm?: string, encoding?: string): string",
        "Computes HMAC.",
    ),
    (
        "equals",
        "crypto.equals(a: string, b: string): boolean",
        "Constant-time string comparison.",
    ),
    (
        "pbkdf2",
        "crypto.pbkdf2(password: string, salt: string, options?: table): string",
        "Derives key using PBKDF2.",
    ),
    (
        "hkdf",
        "crypto.hkdf(key: string, options?: table): string",
        "Derives key using HKDF.",
    ),
    (
        "hashPassword",
        "crypto.hashPassword(password: string, options?: table): string",
        "Hashes password securely.",
    ),
    (
        "verifyPassword",
        "crypto.verifyPassword(password: string, hash: string): boolean",
        "Verifies hashed password.",
    ),
    (
        "generateKey",
        "crypto.generateKey(algorithm?: string, options?: table): string",
        "Generates symmetric key.",
    ),
    (
        "encrypt",
        "crypto.encrypt(s: string, key: string, options?: table): string",
        "Encrypts data.",
    ),
    (
        "decrypt",
        "crypto.decrypt(s: string, key: string, options?: table): string",
        "Decrypts data.",
    ),
    (
        "generateKeyPair",
        "crypto.generateKeyPair(algorithm?: string, options?: table): KeyPair",
        "Generates asymmetric keypair.",
    ),
    (
        "publicKey",
        "crypto.publicKey(privateKey: string): string",
        "Extracts public key from private key.",
    ),
    (
        "sign",
        "crypto.sign(s: string, privateKey: string, options?: table): string",
        "Signs data using private key.",
    ),
    (
        "verify",
        "crypto.verify(s: string, signature: string, publicKey: string, options?: table): boolean",
        "Verifies signature with public key.",
    ),
    (
        "publicEncrypt",
        "crypto.publicEncrypt(s: string, publicKey: string, options?: table): string",
        "Encrypts with public key.",
    ),
    (
        "privateDecrypt",
        "crypto.privateDecrypt(s: string, privateKey: string, options?: table): string",
        "Decrypts with private key.",
    ),
    (
        "sharedSecret",
        "crypto.sharedSecret(privateKey: string, publicKey: string, options?: table): string",
        "Derives Diffie-Hellman shared secret.",
    ),
    (
        "randomBytes",
        "crypto.randomBytes(n: number, encoding?: string): string",
        "Generates CSPRNG random bytes.",
    ),
    (
        "randomInt",
        "crypto.randomInt(min: number, max?: number): number",
        "Generates CSPRNG random integer.",
    ),
    ("uuid", "crypto.uuid(): string", "Generates UUID v4."),
    (
        "encode",
        "crypto.encode(s: string, encoding: string): string",
        "Encodes string (hex, base64).",
    ),
    (
        "decode",
        "crypto.decode(s: string, encoding: string): string",
        "Decodes string (hex, base64).",
    ),
];

fn handle_completion(
    content: &str,
    line: usize,
    character: usize,
    cached_symbols: Option<&[FoundSymbol]>,
) -> Json {
    let doc_lines: Vec<&str> = content.lines().collect();
    let line_str = doc_lines.get(line).unwrap_or(&"");
    let byte_idx = utf16_col_to_byte_offset(line_str, character);
    let prefix = &line_str[..byte_idx];
    let trimmed_prefix = prefix.trim_end();

    let mut items = Vec::new();

    if let Some(dot_idx) = trimmed_prefix.rfind('.') {
        let before_dot = &trimmed_prefix[..dot_idx];
        let filter = &trimmed_prefix[dot_idx + 1..];
        let mod_name = before_dot
            .rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .next()
            .unwrap_or("");

        let method_list: Option<&[(&str, &str, &str)]> = match mod_name {
            "os" => Some(OS_METHODS),
            "string" => Some(STRING_METHODS),
            "table" => Some(TABLE_METHODS),
            "math" => Some(MATH_METHODS),
            "coroutine" => Some(COROUTINE_METHODS),
            "task" => Some(TASK_METHODS),
            "json" => Some(JSON_METHODS),
            "fs" => Some(FS_METHODS),
            "crypto" => Some(CRYPTO_METHODS),
            _ => None,
        };

        if let Some(methods) = method_list {
            for (name, detail, _) in methods {
                if filter.is_empty() || name.starts_with(filter) {
                    items.push(make_completion_item(name, detail, 3));
                }
            }
            return Json::Arr(items);
        }
    }

    let filter = prefix
        .rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()
        .unwrap_or("");

    for (name, detail) in &[
        ("print", "print(...)"),
        ("assert", "assert(cond, msg)"),
        ("type", "type(val)"),
        ("checktype", "checktype(val, ty)"),
        ("asserttype", "asserttype(val, ty)"),
        ("pcall", "pcall(fn, ...)"),
        ("xpcall", "xpcall(fn, msgh, ...)"),
        ("error", "error(msg)"),
        ("require", "require(mod)"),
        ("os", "module os"),
        ("string", "module string"),
        ("table", "module table"),
        ("math", "module math"),
        ("coroutine", "module coroutine"),
        ("task", "module task"),
        ("json", "module json"),
        ("fs", "module fs"),
        ("crypto", "module crypto"),
    ] {
        if filter.is_empty() || name.starts_with(filter) {
            items.push(make_completion_item(name, detail, 3));
        }
    }

    for kw in &[
        "function", "local", "if", "then", "else", "elseif", "for", "while", "do", "repeat",
        "until", "return", "break", "continue", "match", "case", "default", "enum", "end",
    ] {
        if filter.is_empty() || kw.starts_with(filter) {
            items.push(make_completion_item(kw, "keyword", 14));
        }
    }

    let parsed_symbols = extract_document_symbols(content);
    let symbols = if !parsed_symbols.is_empty() {
        parsed_symbols
    } else if let Some(cached) = cached_symbols {
        cached.to_vec()
    } else {
        Vec::new()
    };

    for sym in symbols {
        if filter.is_empty() || sym.name.starts_with(filter) {
            items.push(make_completion_item(&sym.name, &sym.detail, 6));
        }
    }

    Json::Arr(items)
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

fn lookup_docs(word: &str) -> Option<(&'static str, &'static str)> {
    if let Some((mod_name, func_name)) = word.split_once('.') {
        let list: Option<&[(&str, &str, &str)]> = match mod_name {
            "os" => Some(OS_METHODS),
            "string" => Some(STRING_METHODS),
            "table" => Some(TABLE_METHODS),
            "math" => Some(MATH_METHODS),
            "coroutine" => Some(COROUTINE_METHODS),
            "task" => Some(TASK_METHODS),
            "json" => Some(JSON_METHODS),
            "fs" => Some(FS_METHODS),
            "crypto" => Some(CRYPTO_METHODS),
            _ => None,
        };
        if let Some(methods) = list
            && let Some((_, sig, desc)) = methods.iter().find(|(name, _, _)| *name == func_name)
        {
            return Some((*sig, *desc));
        }
    }

    match word {
        "print" => Some((
            "print(...: any): void",
            "Prints formatted values to standard output separated by tabs and followed by a newline.",
        )),
        "assert" => Some((
            "assert(v: any, message?: string): any",
            "Asserts that `v` is truthy (not `nil` or `false`). Raises an error if assertion fails.",
        )),
        "type" => Some((
            "type(v: any): string",
            "Returns the basic type name of `v` as a string (`\"nil\"`, `\"boolean\"`, `\"number\"`, `\"string\"`, `\"table\"`, `\"function\"`, `\"thread\"`, `\"buffer\"`).",
        )),
        "checktype" => Some((
            "checktype(val: any, expected: string): boolean",
            "Returns true if `val` conforms to the specified type expression, false otherwise.",
        )),
        "asserttype" | "assert_type" => Some((
            "asserttype(val: any, expected: string): any",
            "Asserts that `val` conforms to the specified type expression. Raises error `E0210` on mismatch.",
        )),
        "pcall" => Some((
            "pcall(f: function, ...args: any): (boolean, ...any)",
            "Calls function `f` in protected mode. Catches runtime errors and returns `true` plus results, or `false` plus error message.",
        )),
        "xpcall" => Some((
            "xpcall(f: function, msgh: function, ...args: any): (boolean, ...any)",
            "Calls function `f` in protected mode with a custom error handler `msgh`.",
        )),
        "error" => Some((
            "error(message: string, level?: int): void",
            "Terminates the last protected function called and returns `message` as the error object.",
        )),
        "setmetatable" => Some((
            "setmetatable(table: table, metatable: table | nil): table",
            "Sets the metatable for the given table. If `metatable` is `nil`, removes the metatable.",
        )),
        "getmetatable" => Some((
            "getmetatable(table: table): table | nil",
            "Returns the metatable of the given table, or `nil` if it has no metatable.",
        )),
        "rawget" => Some((
            "rawget(table: table, index: any): any",
            "Gets the real value of `table[index]` without invoking `__index` metamethods.",
        )),
        "rawset" => Some((
            "rawset(table: table, index: any, value: any): table",
            "Sets the real value of `table[index]` to `value` without invoking `__newindex` metamethods.",
        )),
        "rawequal" => Some((
            "rawequal(v1: any, v2: any): boolean",
            "Checks whether `v1` is primitive-equal to `v2` without invoking `__eq` metamethods.",
        )),
        "rawlen" => Some((
            "rawlen(v: table | string): int",
            "Returns the length of `v` without invoking `__len` metamethods.",
        )),
        "select" => Some((
            "select(index: int | \"#\", ...args: any): any",
            "If index is a number, returns all arguments after argument number `index`. If `\"#\"`, returns the count of arguments.",
        )),
        "collectgarbage" => Some((
            "collectgarbage(opt?: string, arg?: int): any",
            "Performs a garbage collection cycle or queries GC statistics.",
        )),
        "require" => Some((
            "require(modname: string): any",
            "Loads and returns the given module, caching the result in `package.loaded`.",
        )),
        "os" => Some((
            "module os",
            "Operating system interaction: processes, environment, clock, and filesystem.",
        )),
        "string" => Some((
            "module string",
            "String manipulation, formatting, pattern matching, and encoding.",
        )),
        "table" => Some((
            "module table",
            "Table manipulation, sorting, packing, and freezing.",
        )),
        "math" => Some((
            "module math",
            "Standard mathematical functions, trigonometric operations, and constants.",
        )),
        "coroutine" => Some((
            "module coroutine",
            "Cooperative multithreading and fiber manipulation.",
        )),
        "task" => Some((
            "module task",
            "Task scheduler: spawn, defer, delay, wait and cancel coroutine tasks.",
        )),
        "json" => Some(("module json", "Fast JSON encoding and decoding.")),
        "fs" => Some((
            "module fs",
            "Filesystem operations: reading, writing, directories, file metadata.",
        )),
        "crypto" => Some((
            "module crypto",
            "Cryptographic hashing, HMAC, encryption, key generation, and OS CSPRNG.",
        )),
        "function" => Some(("keyword function", "Defines a named or anonymous function.")),
        "local" => Some((
            "keyword local",
            "Declares a lexical local variable or constant.",
        )),
        "if" => Some((
            "keyword if",
            "Conditional control flow: `if <cond> then <stmts> end`.",
        )),
        "then" => Some((
            "keyword then",
            "Delimits the condition in an `if` or `elseif` statement.",
        )),
        "else" => Some((
            "keyword else",
            "Specifies alternative branch in an `if` statement.",
        )),
        "elseif" => Some((
            "keyword elseif",
            "Chained conditional branch in an `if` statement.",
        )),
        "for" => Some((
            "keyword for",
            "Loop construct: numeric `for i = 1, 10 do` or generic `for k, v in iter do`.",
        )),
        "while" => Some((
            "keyword while",
            "While loop: executes body while condition is truthy.",
        )),
        "repeat" => Some((
            "keyword repeat",
            "Repeat-until loop: executes body until condition becomes truthy.",
        )),
        "until" => Some((
            "keyword until",
            "Terminates a `repeat` loop when condition is truthy.",
        )),
        "return" => Some((
            "keyword return",
            "Returns values from the enclosing function or chunk.",
        )),
        "break" => Some((
            "keyword break",
            "Terminates execution of the innermost loop.",
        )),
        "continue" => Some((
            "keyword continue",
            "Skips to the next iteration of the innermost loop.",
        )),
        "match" => Some((
            "keyword match",
            "Pattern-matching statement: `match <expr> case <pattern>: ... end`.",
        )),
        "case" => Some((
            "keyword case",
            "Pattern-matching clause in a `match` statement.",
        )),
        "default" => Some(("keyword default", "Fallback clause in a `match` statement.")),
        "enum" => Some((
            "keyword enum",
            "Defines an enumeration table: `enum <Name> ... end`.",
        )),
        "do" => Some((
            "keyword do",
            "Introduces an explicit lexical block scope or loop body.",
        )),
        "end" => Some((
            "keyword end",
            "Closes a function, block, loop, condition, match, or enum.",
        )),
        _ => None,
    }
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
        assert!(resp_str.contains("print(...: any): void"));
        assert!(resp_str.contains("Prints formatted values"));
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

    #[test]
    fn test_lsp_completion_items_match_runtime_tables() {
        let vm = crate::vm::machine::VM::new();
        for (lib_name, methods) in [
            ("os", OS_METHODS),
            ("string", STRING_METHODS),
            ("table", TABLE_METHODS),
            ("math", MATH_METHODS),
            ("coroutine", COROUTINE_METHODS),
            ("task", TASK_METHODS),
            ("json", JSON_METHODS),
        ] {
            let global_val = vm
                .globals
                .get(&crate::vm::value::StrRef::from(lib_name))
                .unwrap_or_else(|| panic!("Global {} not found in VM", lib_name));
            let Value::Table(tbl) = global_val else {
                panic!("Global {} is not a table", lib_name);
            };
            for (method, _, _) in methods {
                let val = tbl.borrow().get_str(method);
                assert!(
                    !matches!(val, Value::Nil),
                    "Method {}.{} does not exist in runtime VM table",
                    lib_name,
                    method
                );
            }
        }

        // Test bundled libraries fs and crypto
        for (module_req, methods) in [
            ("return require('@neyuki/fs')", FS_METHODS),
            ("return require('@neyuki/crypto')", CRYPTO_METHODS),
        ] {
            let proto =
                crate::compiler::compile_program(module_req, "test", crate::compiler::Options::RUN)
                    .expect("Failed to compile require script");
            let mut test_vm = crate::vm::machine::VM::new();
            let res = test_vm.execute(proto).expect("Failed to execute require");
            let Value::Table(tbl) = res else {
                panic!("Require did not return a table for {}", module_req);
            };
            for (method, _, _) in methods {
                let val = tbl.borrow().get_str(method);
                assert!(
                    !matches!(val, Value::Nil),
                    "Method {} does not exist in required module {}",
                    method,
                    module_req
                );
            }
        }
    }
}
