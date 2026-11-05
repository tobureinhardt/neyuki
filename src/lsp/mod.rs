//! Language Server Protocol (LSP) server for `neyuki lsp`, running over stdio.

use std::collections::HashMap;
use std::io::{self, BufReader, Write};

use crate::dap_json::{self, Json};
use crate::diagnostics::severity::Severity;

pub mod analysis;
pub mod api;
pub mod modules;
pub mod scopes;

use analysis::{Binding, Context};
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
    /// The modules the documents require, read as they are asked for.
    pub modules: modules::ModuleStore,
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
            modules: modules::ModuleStore::new(),
        }
    }

    /// What completion, hover and signature help know of the document
    /// `uri` whose text is `text`: the standard library and the modules it
    /// requires.
    fn source(&self, uri: &str, text: &str) -> modules::ProjectSource<'_> {
        modules::ProjectSource::new(api::api(), &self.modules, &self.documents, uri, text)
    }

    /// Reads the shape of a document when it changes, so that the last text
    /// that parsed is remembered while the next ones do not.
    fn track(&self, uri: &str) {
        if let Some(text) = self.documents.get(uri) {
            let _ = self
                .modules
                .shape_of_text(&modules::document_key(uri), text);
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
                                    ("referencesProvider".to_string(), Json::Bool(true)),
                                    (
                                        "renameProvider".to_string(),
                                        Json::Obj(vec![(
                                            "prepareProvider".to_string(),
                                            Json::Bool(true),
                                        )]),
                                    ),
                                    ("documentHighlightProvider".to_string(), Json::Bool(true)),
                                    ("foldingRangeProvider".to_string(), Json::Bool(true)),
                                    ("workspaceSymbolProvider".to_string(), Json::Bool(true)),
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
                        self.track(&uri);
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
                            self.track(&uri);
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
                            self.track(uri);
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
                        let src = self.source(uri, content);
                        handle_hover(content, line, character, cached, &src)
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
                        let src = self.source(uri, content);
                        handle_completion(
                            content,
                            line,
                            character,
                            cached,
                            self.snippet_support,
                            &src,
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
                        let src = self.source(uri, content);
                        analysis::signature_help(content, line, character, &src)
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
                        let src = self.source(uri, content);
                        handle_definition(uri, content, line, character, cached, &src)
                    } else {
                        Json::Null
                    };

                    let resp = json_response(req_id, result);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "textDocument/references" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let (uri, line, character) = parse_text_document_position(&json);
                    let include_declaration = json
                        .get("params")
                        .and_then(|p| p.get("context"))
                        .and_then(|c| c.get("includeDeclaration"))
                        != Some(&Json::Bool(false));
                    let result = match self.documents.get(uri) {
                        Some(content) => {
                            handle_references(uri, content, line, character, include_declaration)
                        }
                        None => Json::Null,
                    };
                    let resp = json_response(req_id, result);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "textDocument/documentHighlight" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let (uri, line, character) = parse_text_document_position(&json);
                    let result = match self.documents.get(uri) {
                        Some(content) => handle_highlight(content, line, character),
                        None => Json::Null,
                    };
                    let resp = json_response(req_id, result);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "textDocument/prepareRename" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let (uri, line, character) = parse_text_document_position(&json);
                    let result = match self.documents.get(uri) {
                        Some(content) => handle_prepare_rename(content, line, character),
                        None => Json::Null,
                    };
                    let resp = json_response(req_id, result);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "textDocument/rename" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let (uri, line, character) = parse_text_document_position(&json);
                    let new_name = json
                        .get("params")
                        .and_then(|p| p.get("newName"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("");
                    let outcome = match self.documents.get(uri) {
                        Some(content) => handle_rename(uri, content, line, character, new_name),
                        None => Ok(Json::Null),
                    };
                    let resp = match outcome {
                        Ok(edit) => json_response(req_id, edit),
                        // InvalidParams: the message is shown to the user.
                        Err(message) => json_error(req_id, -32602, &message),
                    };
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "textDocument/foldingRange" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let uri = json
                        .get("params")
                        .and_then(|p| p.get("textDocument"))
                        .and_then(|d| d.get("uri"))
                        .and_then(|u| u.as_str())
                        .unwrap_or("");
                    let result = match self.documents.get(uri) {
                        Some(content) => handle_folding_ranges(content),
                        None => Json::Arr(Vec::new()),
                    };
                    let resp = json_response(req_id, result);
                    return Some(dap_json::render_json(&resp).into_bytes());
                }
                "workspace/symbol" => {
                    let req_id = id.cloned().unwrap_or(Json::Null);
                    let query = json
                        .get("params")
                        .and_then(|p| p.get("query"))
                        .and_then(|q| q.as_str())
                        .unwrap_or("");
                    let result = handle_workspace_symbols(self, query);
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

/// `DiagnosticTag.Unnecessary`.
const TAG_UNNECESSARY: f64 = 1.0;

/// The diagnostics of `neyuki lint` for a document, as the client takes them:
/// the same checks and codes, severities mapped to LSP's four levels, the
/// help text under the message and UTF-16 columns.
fn create_publish_diagnostics(uri: &str, content: &str) -> Json {
    let diags = crate::lint::lint_source(content);
    let mut diag_items = Vec::new();
    let doc_lines: Vec<&str> = content.lines().collect();

    let line_text = |index: usize| doc_lines.get(index).copied().unwrap_or("");

    for d in diags {
        // Lines and columns are 1-based, columns count characters.
        let span = d.primary_label.as_ref().map(|l| l.span).unwrap_or_default();
        let start_line = (span.start.line.max(1) - 1) as usize;
        let end_line = (span.end.line.max(span.start.line).max(1) - 1) as usize;
        let start_col = (span.start.column.max(1) - 1) as usize;
        let end_col = (span.end.column.max(1) - 1) as usize;
        let mut start_char = char_col_to_utf16_col(line_text(start_line), start_col);
        let mut end_char = char_col_to_utf16_col(line_text(end_line), end_col);
        if end_line == start_line && end_char <= start_char {
            // A position without a length marks the code of its line.
            let text = line_text(start_line);
            start_char = text.encode_utf16().count() - text.trim_start().encode_utf16().count();
            end_char = text.trim_end().encode_utf16().count().max(start_char + 1);
        }

        // DiagnosticSeverity: 1 Error, 2 Warning, 3 Information, 4 Hint.
        let severity = match d.severity {
            Severity::Error => 1.0,
            Severity::Warning => 2.0,
            Severity::Info => 3.0,
            Severity::Help => 4.0,
        };
        let code = d.code.map(|code| code.as_str().to_string());
        let message = match &d.help {
            Some(help) => format!("{}\n{}", d.message, help),
            None => d.message.clone(),
        };

        let mut fields = vec![
            (
                "range".to_string(),
                json_obj([
                    ("start", position_json(start_line, start_char)),
                    ("end", position_json(end_line, end_char)),
                ]),
            ),
            ("severity".to_string(), Json::Num(severity)),
            ("message".to_string(), Json::Str(message)),
            ("source".to_string(), Json::Str("neyuki".to_string())),
        ];
        if let Some(code) = &code {
            fields.push(("code".to_string(), Json::Str(code.clone())));
        }
        // Unused and unreachable code is faded out, not underlined.
        if matches!(code.as_deref(), Some("W0001" | "W0002" | "W0004")) {
            fields.push((
                "tags".to_string(),
                Json::Arr(vec![Json::Num(TAG_UNNECESSARY)]),
            ));
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

fn json_obj<const N: usize>(fields: [(&str, Json); N]) -> Json {
    Json::Obj(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

fn position_json(line: usize, character: usize) -> Json {
    json_obj([
        ("line", Json::Num(line as f64)),
        ("character", Json::Num(character as f64)),
    ])
}

/// The range between two byte offsets of `content`.
fn byte_range_json(content: &str, start: usize, end: usize) -> Json {
    let (start_line, start_character) = scopes::position_at(content, start);
    let (end_line, end_character) = scopes::position_at(content, end);
    json_obj([
        ("start", position_json(start_line, start_character)),
        ("end", position_json(end_line, end_character)),
    ])
}

/// A place in a file, from `api::Definition`.
fn definition_json(definition: &api::Definition) -> Json {
    json_obj([
        ("uri", Json::Str(modules::path_to_uri(&definition.file))),
        (
            "range",
            json_obj([
                (
                    "start",
                    position_json(definition.line, definition.character),
                ),
                (
                    "end",
                    position_json(definition.line, definition.character + definition.length),
                ),
            ]),
        ),
    ])
}

/// Go to definition. A `require` string leads to the file it names, a name
/// of a required module (`k`, `k.new`, `k.Options`) to where the module
/// declares it, and any other name to its declaration in this document.
fn handle_definition(
    uri: &str,
    content: &str,
    line: usize,
    character: usize,
    cached_symbols: Option<&[FoundSymbol]>,
    src: &dyn MemberSource,
) -> Json {
    if let Some(module) = analysis::require_at(content, line, character) {
        // The libraries of the reference are inside the executable.
        let located = match src.namespace_for_module(&module) {
            Some(namespace) if !namespace.starts_with("@m") => None,
            _ => modules::uri_to_path(uri)
                .and_then(|requester| modules::resolve_module_file(&requester, &module)),
        };
        return match located {
            Some(file) => definition_json(&api::Definition {
                file,
                line: 0,
                character: 0,
                length: 0,
            }),
            None => Json::Null,
        };
    }

    if let Some(chain) = analysis::chain_at(content, line, character) {
        let offset = analysis::offset_at(content, line, character);
        let binds = analysis::bindings_at(content, offset, src);
        let head = chain.split(['.', ':']).next().unwrap_or("");
        // Only through the name of a module: a variable of a type declared
        // there goes to its own declaration.
        let through_module =
            matches!(binds.get(head), Some(Binding::Namespace(ns)) if ns.starts_with("@m"));
        if through_module
            && let Some(definition) = src.definition(&analysis::canonical(&chain, &binds))
        {
            return definition_json(&definition);
        }
    }

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

/// The name at a position and everything in the document that is the same
/// name. `None` where there is no name, or the document does not parse.
fn symbol_at_position(content: &str, line: usize, character: usize) -> Option<scopes::Symbol> {
    let offset = analysis::offset_at(content, line, character);
    scopes::analyze(content)?.symbol_at(offset)
}

fn handle_references(
    uri: &str,
    content: &str,
    line: usize,
    character: usize,
    include_declaration: bool,
) -> Json {
    let Some(symbol) = symbol_at_position(content, line, character) else {
        return Json::Null;
    };
    let length = symbol.name.len();
    Json::Arr(
        symbol
            .occurrences
            .iter()
            .filter(|occurrence| include_declaration || !occurrence.declaration)
            .map(|occurrence| {
                json_obj([
                    ("uri", Json::Str(uri.to_string())),
                    (
                        "range",
                        byte_range_json(content, occurrence.start, occurrence.start + length),
                    ),
                ])
            })
            .collect(),
    )
}

fn handle_highlight(content: &str, line: usize, character: usize) -> Json {
    let Some(symbol) = symbol_at_position(content, line, character) else {
        return Json::Null;
    };
    let length = symbol.name.len();
    Json::Arr(
        symbol
            .occurrences
            .iter()
            .map(|occurrence| {
                // DocumentHighlightKind: 2 is Read, 3 is Write.
                let kind = if occurrence.write { 3.0 } else { 2.0 };
                json_obj([
                    (
                        "range",
                        byte_range_json(content, occurrence.start, occurrence.start + length),
                    ),
                    ("kind", Json::Num(kind)),
                ])
            })
            .collect(),
    )
}

/// The range of the name that can be renamed at a position, or null.
fn handle_prepare_rename(content: &str, line: usize, character: usize) -> Json {
    let offset = analysis::offset_at(content, line, character);
    let Some(symbol) = symbol_at_position(content, line, character) else {
        return Json::Null;
    };
    if !symbol.renamable {
        return Json::Null;
    }
    let length = symbol.name.len();
    let Some(touched) = symbol
        .occurrences
        .iter()
        .find(|o| o.start <= offset && offset <= o.start + length)
    else {
        return Json::Null;
    };
    json_obj([
        (
            "range",
            byte_range_json(content, touched.start, touched.start + length),
        ),
        ("placeholder", Json::Str(symbol.name.clone())),
    ])
}

/// Renames the name at a position everywhere it refers to the same
/// declaration. The error is the message for the user.
fn handle_rename(
    uri: &str,
    content: &str,
    line: usize,
    character: usize,
    new_name: &str,
) -> Result<Json, String> {
    let valid = new_name
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && new_name.bytes().all(analysis::is_word_byte)
        && !analysis::KEYWORDS.contains(&new_name);
    if !valid {
        return Err(format!("'{new_name}' is not a valid name"));
    }
    let Some(symbol) = symbol_at_position(content, line, character) else {
        return Ok(Json::Null);
    };
    if !symbol.renamable {
        return Err(format!(
            "'{}' cannot be renamed: it is not declared in this file, or a use of it cannot be located",
            symbol.name
        ));
    }
    let length = symbol.name.len();
    let edits = symbol
        .occurrences
        .iter()
        .map(|occurrence| {
            json_obj([
                (
                    "range",
                    byte_range_json(content, occurrence.start, occurrence.start + length),
                ),
                ("newText", Json::Str(new_name.to_string())),
            ])
        })
        .collect();
    Ok(json_obj([(
        "changes",
        Json::Obj(vec![(uri.to_string(), Json::Arr(edits))]),
    )]))
}

fn handle_folding_ranges(content: &str) -> Json {
    Json::Arr(
        scopes::folding_ranges(content)
            .into_iter()
            .map(|fold| {
                let mut fields = vec![
                    ("startLine", Json::Num(fold.start_line as f64)),
                    ("endLine", Json::Num(fold.end_line as f64)),
                ];
                if fold.comment {
                    fields.push(("kind", Json::Str("comment".to_string())));
                }
                Json::Obj(
                    fields
                        .into_iter()
                        .map(|(key, value)| (key.to_string(), value))
                        .collect(),
                )
            })
            .collect(),
    )
}

/// The functions and types of the open documents whose name contains
/// `query`, ignoring case.
fn handle_workspace_symbols(server: &LspServer, query: &str) -> Json {
    const MAX_SYMBOLS: usize = 200;
    let query = query.to_lowercase();
    let mut uris: Vec<&String> = server.documents.keys().collect();
    uris.sort();
    let mut found = Vec::new();
    for uri in uris {
        let Some(content) = server.documents.get(uri) else {
            continue;
        };
        let Some(symbols) = server.last_valid_symbols.get(uri) else {
            continue;
        };
        let file = modules::uri_to_path(uri)
            .and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        for symbol in symbols {
            // Function and TypeParameter, which is how aliases are listed.
            if !matches!(symbol.kind, 12 | 26) || !symbol.name.to_lowercase().contains(&query) {
                continue;
            }
            let line_index = symbol.line_1_based.saturating_sub(1) as usize;
            let line_text = content.lines().nth(line_index).unwrap_or("");
            let start = find_ident_in_line(line_text, &symbol.name)
                .map_or(0, |col| char_col_to_utf16_col(line_text, col));
            let end = start + symbol.name.chars().map(char::len_utf16).sum::<usize>();
            found.push(json_obj([
                ("name", Json::Str(symbol.name.clone())),
                ("kind", Json::Num(symbol.kind as f64)),
                ("containerName", Json::Str(file.clone())),
                (
                    "location",
                    json_obj([
                        ("uri", Json::Str(uri.clone())),
                        (
                            "range",
                            json_obj([
                                ("start", position_json(line_index, start)),
                                ("end", position_json(line_index, end)),
                            ]),
                        ),
                    ]),
                ),
            ]));
            if found.len() >= MAX_SYMBOLS {
                return Json::Arr(found);
            }
        }
    }
    Json::Arr(found)
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
    let binds = analysis::bindings_at(content, offset, src);

    let mut items = Vec::new();
    match context {
        Context::None => {}
        Context::Module { prefix } => {
            let start = character.saturating_sub(prefix.len());
            // `./name` offers the files of the project only.
            let (dot, typed) = match prefix.strip_prefix("./") {
                Some(rest) => ("./", rest),
                None => ("", prefix.as_str()),
            };
            for lib in src.modules() {
                let Some(name) = lib.module.as_deref() else {
                    continue;
                };
                if !name.starts_with(typed) || (!dot.is_empty() && lib.file.is_none()) {
                    continue;
                }
                items.push(analysis::library_item(
                    &lib,
                    &format!("{dot}{name}"),
                    Some((line, start, character)),
                ));
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
        Context::Type {
            qualifier: Some(chain),
            prefix,
        } => {
            // `k.` in an annotation: the types the module exports.
            let namespace = analysis::canonical(&chain, &binds);
            for entry in src.type_aliases(&namespace) {
                if analysis::short_name(&entry.name).starts_with(&prefix) {
                    items.push(analysis::type_item(&entry));
                }
            }
        }
        Context::Type {
            qualifier: None,
            prefix,
        } => {
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
            // The types declared in this document.
            for entry in src.local_types() {
                if analysis::short_name(&entry.name).starts_with(&prefix) {
                    items.push(analysis::type_item(&entry));
                }
            }
            // The modules to qualify a type with.
            let mut names: Vec<&String> = binds.keys().collect();
            names.sort();
            for name in names {
                let exports_types = matches!(
                    binds.get(name),
                    Some(Binding::Namespace(ns)) if !src.type_aliases(ns).is_empty()
                );
                if exports_types && name.starts_with(&prefix) {
                    items.push(analysis::word_item(name, "module"));
                }
            }
        }
        Context::Global { prefix } => {
            // Inside the table of a typed declaration: its fields.
            if let Some((namespace, typed)) =
                analysis::constructor_context(&content[..offset], &binds)
            {
                let fields = src.namespace_members(&namespace);
                if !fields.is_empty() {
                    for entry in fields {
                        if analysis::short_name(&entry.name).starts_with(&typed) {
                            items.push(analysis::field_item(&entry));
                        }
                    }
                    return Json::Arr(items);
                }
            }

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

    // ---- modules, types and navigation across files

    /// A module with a documented function, a method, a value and two types.
    const MODULE: &str = "-- Creates a window.\nexport type Options = { name: string?, size: number, inner: Inner }\ntype Inner = { depth: int }\nlocal khione = {}\nkhione.version = \"1.0\"\n-- Opens it.\nfunction khione.new(options: Options, flag: boolean?): Inner\n    return nil\nend\nfunction khione:close(code: int)\nend\nreturn khione\n";

    /// A directory of its own under the system temp directory.
    fn cleanup(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("neyuki_lsp_server_{}_{}", name, std::process::id()));
        cleanup(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_file(path: &std::path::Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn json_string(text: &str) -> String {
        dap_json::render_json(&Json::Str(text.to_string()))
    }

    /// Opens `text` as the file `path`, which need not exist, and returns its
    /// URI.
    fn open_file(server: &mut LspServer, path: &std::path::Path, text: &str) -> String {
        let uri = modules::path_to_uri(path);
        let req = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":{},"text":{}}}}}}}"#,
            json_string(&uri),
            json_string(text)
        );
        let _ = server.handle_message(req.as_bytes());
        uri
    }

    fn change_file(server: &mut LspServer, uri: &str, text: &str) {
        let req = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didChange","params":{{"textDocument":{{"uri":{}}},"contentChanges":[{{"text":{}}}]}}}}"#,
            json_string(uri),
            json_string(text)
        );
        let _ = server.handle_message(req.as_bytes());
    }

    fn request_in(
        server: &mut LspServer,
        uri: &str,
        method: &str,
        line: usize,
        character: usize,
    ) -> String {
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":101,"method":"{}","params":{{"textDocument":{{"uri":{}}},"position":{{"line":{}, "character":{}}}}}}}"#,
            method,
            json_string(uri),
            line,
            character
        );
        let resp = server.handle_message(req.as_bytes()).unwrap();
        String::from_utf8(resp).unwrap()
    }

    /// Opens `text` as `main.nyk` of `dir` and sends `method` at its end.
    fn query_in(dir: &std::path::Path, text: &str, method: &str) -> String {
        let mut server = LspServer::new();
        let uri = open_file(&mut server, &dir.join("main.nyk"), text);
        let (line, character) = end_position(text);
        request_in(&mut server, &uri, method, line, character)
    }

    #[test]
    fn test_lsp_initialize_advertises_the_navigation_features() {
        let mut server = LspServer::new();
        let init_req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = server.handle_message(init_req.as_bytes()).unwrap();
        let resp_str = String::from_utf8(resp).unwrap();
        assert!(resp_str.contains("\"referencesProvider\":true"));
        assert!(resp_str.contains("\"renameProvider\":{\"prepareProvider\":true}"));
        assert!(resp_str.contains("\"documentHighlightProvider\":true"));
        assert!(resp_str.contains("\"foldingRangeProvider\":true"));
        assert!(resp_str.contains("\"workspaceSymbolProvider\":true"));
    }

    #[test]
    fn test_lsp_completion_lists_members_of_a_required_sibling_file() {
        let dir = scratch("members");
        write_file(&dir.join("khione.nyk"), MODULE);
        let resp = query_in(
            &dir,
            "local k = require(\"khione\")\nk.",
            "textDocument/completion",
        );
        assert!(resp.contains("\"label\":\"new\""));
        assert!(resp.contains("\"detail\":\"new(options: Options, flag: boolean?): Inner\""));
        assert!(resp.contains("Opens it."));
        assert!(resp.contains("\"label\":\"version\""));
        // The prefix filters, and the types are not values.
        let resp = query_in(
            &dir,
            "local k = require(\"khione\")\nk.ne",
            "textDocument/completion",
        );
        assert!(resp.contains("\"label\":\"new\""));
        assert!(!resp.contains("\"label\":\"version\""));
        assert!(!resp.contains("\"label\":\"Options\""));
        // After a colon: the functions declared with one.
        let resp = query_in(
            &dir,
            "local k = require(\"khione\")\nk:",
            "textDocument/completion",
        );
        assert!(resp.contains("\"label\":\"close\""));
        assert!(!resp.contains("\"label\":\"new\""));
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_completion_prefers_the_unsaved_text_of_an_open_module() {
        let dir = scratch("unsaved");
        write_file(&dir.join("khione.nyk"), MODULE);
        let mut server = LspServer::new();
        let module = open_file(
            &mut server,
            &dir.join("khione.nyk"),
            "local m = {}\nfunction m.unsaved() end\nreturn m\n",
        );
        let text = "local k = require(\"khione\")\nk.";
        let main = open_file(&mut server, &dir.join("main.nyk"), text);
        let (line, character) = end_position(text);
        let resp = request_in(
            &mut server,
            &main,
            "textDocument/completion",
            line,
            character,
        );
        assert!(resp.contains("\"label\":\"unsaved\""));
        assert!(!resp.contains("\"label\":\"new\""));
        // Typing in the module shows up in the next request.
        change_file(
            &mut server,
            &module,
            "local m = {}\nfunction m.unsaved() end\nfunction m.later() end\nreturn m\n",
        );
        let resp = request_in(
            &mut server,
            &main,
            "textDocument/completion",
            line,
            character,
        );
        assert!(resp.contains("\"label\":\"later\""));
        // Once it is closed the file on disk is read again.
        let close = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/didClose","params":{{"textDocument":{{"uri":{}}}}}}}"#,
            json_string(&module)
        );
        let _ = server.handle_message(close.as_bytes());
        let resp = request_in(
            &mut server,
            &main,
            "textDocument/completion",
            line,
            character,
        );
        assert!(resp.contains("\"label\":\"new\""));
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_missing_and_broken_modules_answer_nothing() {
        let dir = scratch("broken");
        write_file(&dir.join("broken.nyk"), "local m = {\nfunction (");
        for module in ["missing", "broken", "../escape", "@neyuki/nope"] {
            let text = format!("local m = require(\"{module}\")\nm.");
            for method in ["textDocument/completion", "textDocument/hover"] {
                let resp = query_in(&dir, &text, method);
                assert!(
                    resp.contains("\"result\":[]") || resp.contains("\"result\":null"),
                    "{resp}"
                );
            }
        }
        // The first broken line is a syntax error only the client sees.
        let resp = query_in(
            &dir,
            "local m = require(\"broken\")\nm.new(",
            "textDocument/signatureHelp",
        );
        assert!(resp.contains("\"result\":null"));
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_module_cycles_do_not_hang() {
        let dir = scratch("cycle");
        write_file(
            &dir.join("a.nyk"),
            "local b = require(\"b\")\nlocal a = {}\nfunction a.one() end\nreturn a\n",
        );
        write_file(
            &dir.join("b.nyk"),
            "local a = require(\"a\")\nlocal b = {}\nfunction b.two() end\nreturn b\n",
        );
        write_file(&dir.join("x.nyk"), "return require(\"y\")\n");
        write_file(&dir.join("y.nyk"), "return require(\"x\")\n");
        let resp = query_in(
            &dir,
            "local a = require(\"a\")\na.",
            "textDocument/completion",
        );
        assert!(resp.contains("\"label\":\"one\""));
        let resp = query_in(
            &dir,
            "local x = require(\"x\")\nx.",
            "textDocument/completion",
        );
        assert!(resp.contains("\"result\":[]"));
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_require_completion_lists_bundled_modules_packages_and_files() {
        let dir = scratch("require");
        write_file(
            &dir.join("neyuki.toml"),
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\n\n[dependencies]\ndep = \"1.0.0\"\n",
        );
        write_file(&dir.join("khione.nyk"), MODULE);
        write_file(&dir.join("sub").join("inner.nyk"), "return {}");
        write_file(&dir.join("target").join("skipped.nyk"), "return {}");
        write_file(&dir.join(".hidden").join("skipped.nyk"), "return {}");
        let resp = query_in(&dir, "local m = require(\"", "textDocument/completion");
        for label in ["@neyuki/ui", "math", "dep", "khione", "sub/inner"] {
            assert!(
                resp.contains(&format!("\"label\":\"{label}\"")),
                "{label}: {resp}"
            );
        }
        assert!(!resp.contains("skipped"));
        // A prefix narrows the list; `./` offers files only.
        let resp = query_in(&dir, "local m = require(\"kh", "textDocument/completion");
        assert!(resp.contains("\"label\":\"khione\""));
        assert!(!resp.contains("@neyuki"));
        let resp = query_in(&dir, "local m = require(\"./s", "textDocument/completion");
        assert!(resp.contains("\"newText\":\"./sub/inner\""));
        assert!(!resp.contains("\"label\":\"dep\""));
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_exported_types_complete_in_type_position() {
        let dir = scratch("types");
        write_file(&dir.join("khione.nyk"), MODULE);
        let head = "local k = require(\"khione\")\n";
        // After `k.`: the exported types, and only them.
        let resp = query_in(
            &dir,
            &format!("{head}local o: k."),
            "textDocument/completion",
        );
        assert!(resp.contains("\"label\":\"Options\""));
        assert!(!resp.contains("\"label\":\"Inner\""));
        assert!(!resp.contains("\"label\":\"new\""));
        for text in [
            format!("{head}function f(a: number, b: k.Op"),
            format!("{head}function f(): k.Op"),
            format!("{head}local list: {{k.Op"),
        ] {
            let resp = query_in(&dir, &text, "textDocument/completion");
            assert!(resp.contains("\"label\":\"Options\""), "{text}: {resp}");
        }
        // Not in a type position: `k.` is the members of the module.
        let resp = query_in(
            &dir,
            &format!("{head}local o = k."),
            "textDocument/completion",
        );
        assert!(resp.contains("\"label\":\"new\""));
        assert!(!resp.contains("\"label\":\"Options\""));

        // Without a qualifier: built-in types, the types of the document and
        // the modules that export types.
        let mut server = LspServer::new();
        let valid = format!("type Local = {{ a: number }}\n{head}local o: Local = nil\n");
        let uri = open_file(&mut server, &dir.join("main.nyk"), &valid);
        let typing = format!("type Local = {{ a: number }}\n{head}local o: ");
        change_file(&mut server, &uri, &typing);
        let (line, character) = end_position(&typing);
        let resp = request_in(
            &mut server,
            &uri,
            "textDocument/completion",
            line,
            character,
        );
        assert!(resp.contains("\"label\":\"number\""));
        assert!(resp.contains("\"label\":\"Local\""));
        assert!(resp.contains("\"label\":\"k\""));
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_record_fields_complete_for_parameters_and_constructors() {
        let dir = scratch("records");
        write_file(&dir.join("khione.nyk"), MODULE);

        // `function khione.new(options: Options)`: `options.` lists the
        // fields of the alias declared in the same file. The text does not
        // parse while it is typed, so the last good one is used.
        let mut server = LspServer::new();
        let valid = "export type Options = { name: string?, size: number }\nlocal khione = {}\nfunction khione.new(options: Options)\n    local n = options\nend\nreturn khione\n";
        let module = open_file(&mut server, &dir.join("mine.nyk"), valid);
        let typing = "export type Options = { name: string?, size: number }\nlocal khione = {}\nfunction khione.new(options: Options)\n    options.";
        change_file(&mut server, &module, typing);
        let (line, character) = end_position(typing);
        let resp = request_in(
            &mut server,
            &module,
            "textDocument/completion",
            line,
            character,
        );
        assert!(resp.contains("\"label\":\"name\""), "{resp}");
        assert!(resp.contains("\"label\":\"size\""));
        assert!(resp.contains("\"detail\":\"name: string?\""));
        // The prefix filters the fields.
        let prefix_typed = typing.to_string() + "na";
        change_file(&mut server, &module, &prefix_typed);
        let (line, character) = end_position(&prefix_typed);
        let resp = request_in(
            &mut server,
            &module,
            "textDocument/completion",
            line,
            character,
        );
        assert!(resp.contains("\"label\":\"name\""));
        assert!(!resp.contains("\"label\":\"size\""));

        // A value annotated with an exported type of a module.
        let head = "local k = require(\"khione\")\nlocal o: k.Options = nil\no.";
        let resp = query_in(&dir, head, "textDocument/completion");
        for field in ["name", "size", "inner"] {
            assert!(resp.contains(&format!("\"label\":\"{field}\"")), "{field}");
        }
        // And the fields of a field that is a record.
        let resp = query_in(&dir, &format!("{head}inner."), "textDocument/completion");
        assert!(resp.contains("\"label\":\"depth\""), "{resp}");

        // Inside the table constructor of a typed declaration.
        for text in [
            "local k = require(\"khione\")\nlocal o: k.Options = { ",
            "local k = require(\"khione\")\nlocal o: k.Options = {\n    name = \"x\",\n    si",
        ] {
            let resp = query_in(&dir, text, "textDocument/completion");
            assert!(resp.contains("\"label\":\"size\""), "{text}: {resp}");
            assert!(resp.contains("\"insertText\":\"size = \""));
            // Names in scope are not mixed in.
            assert!(!resp.contains("\"label\":\"print\""));
        }
        let resp = query_in(
            &dir,
            "local k = require(\"khione\")\nlocal o: k.Options = { inner = { ",
            "textDocument/completion",
        );
        assert!(resp.contains("\"label\":\"depth\""), "{resp}");
        // An untyped table is not a record.
        let resp = query_in(&dir, "local o = { ", "textDocument/completion");
        assert!(resp.contains("\"label\":\"print\""));
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_local_aliases_complete_fields_of_annotated_values() {
        let dir = scratch("alias");
        // The alias comes from the last text that parsed.
        let mut server = LspServer::new();
        let valid = "type Point = { x: number, y: number }\nlocal p: Point = nil\nlocal n = p\n";
        let uri = open_file(&mut server, &dir.join("main.nyk"), valid);
        let typing = "type Point = { x: number, y: number }\nlocal p: Point = nil\np.";
        change_file(&mut server, &uri, typing);
        let (line, character) = end_position(typing);
        let resp = request_in(
            &mut server,
            &uri,
            "textDocument/completion",
            line,
            character,
        );
        assert!(resp.contains("\"label\":\"x\""), "{resp}");
        assert!(resp.contains("\"label\":\"y\""));
        // The elements of a list of the bundled module's type.
        let text = "local ui = require(\"@neyuki/ui\")\nfunction frame(events: {ui.Event}, dt: number)\n    for _, e in events do\n        e.";
        let resp = query_in(&dir, text, "textDocument/completion");
        assert!(resp.contains("\"label\":\"kind\""), "{resp}");
        assert!(resp.contains("\"label\":\"key\""));
        // `--!importall` makes the types available as well.
        let text = "--!importall\nlocal e: ui.Event = nil\ne.";
        let resp = query_in(&dir, text, "textDocument/completion");
        assert!(resp.contains("\"label\":\"kind\""), "{resp}");
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_hover_and_signature_help_on_module_members_and_types() {
        let dir = scratch("hover");
        write_file(&dir.join("khione.nyk"), MODULE);
        let mut server = LspServer::new();
        let text = "local k = require(\"khione\")\nlocal o: k.Options = nil\nk.new(o)\ntype Mine = { a: number }\nlocal m: Mine = nil\nk:close(1)\n";
        let uri = open_file(&mut server, &dir.join("main.nyk"), text);

        // A function of the module, with its comment.
        let resp = request_in(&mut server, &uri, "textDocument/hover", 2, 3);
        assert!(resp.contains("khione.new(options: Options, flag: boolean?): Inner"));
        assert!(resp.contains("Opens it."));
        // An exported type through the module name, and a local alias.
        let resp = request_in(&mut server, &uri, "textDocument/hover", 1, 13);
        assert!(
            resp.contains("export type Options = { name: string?, size: number, inner: Inner }")
        );
        assert!(resp.contains("Creates a window."));
        let resp = request_in(&mut server, &uri, "textDocument/hover", 4, 10);
        assert!(resp.contains("type Mine = { a: number }"));
        // The module itself.
        let resp = request_in(&mut server, &uri, "textDocument/hover", 0, 6);
        assert!(resp.contains("khione.nyk"));

        // Signature help for a call and for a method call, with the call
        // left open as it is while it is typed.
        let typing = "local k = require(\"khione\")\nlocal o: k.Options = nil\nk.new(o, \nk:close(";
        change_file(&mut server, &uri, typing);
        let resp = request_in(&mut server, &uri, "textDocument/signatureHelp", 2, 9);
        assert!(resp.contains("\"label\":\"new(options: Options, flag: boolean?): Inner\""));
        assert!(resp.contains("\"activeParameter\":1"), "{resp}");
        let (line, character) = end_position(typing);
        let resp = request_in(
            &mut server,
            &uri,
            "textDocument/signatureHelp",
            line,
            character,
        );
        assert!(resp.contains("\"label\":\"close(code: int)\""), "{resp}");
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_the_result_type_of_a_module_function_is_known() {
        let dir = scratch("result");
        write_file(
            &dir.join("khione.nyk"),
            "export type Window = { title: string, width: number }\nlocal khione = {}\nfunction khione.open(): Window\nend\nreturn khione\n",
        );
        let text = "local k = require(\"khione\")\nlocal w = k.open()\nw.";
        let resp = query_in(&dir, text, "textDocument/completion");
        assert!(resp.contains("\"label\":\"title\""), "{resp}");
        assert!(resp.contains("\"label\":\"width\""));
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_definition_across_files() {
        let dir = scratch("definition");
        write_file(&dir.join("khione.nyk"), MODULE);
        let target = modules::path_to_uri(&dir.join("khione.nyk"));
        let mut server = LspServer::new();
        let text = "local k = require(\"khione\")\nlocal o: k.Options = nil\nk.new(o)\nlocal v = k.version\nrequire(\"@neyuki/ui\")\nrequire(\"missing\")\nlocal local_value = 1\nprint(local_value)\n";
        let uri = open_file(&mut server, &dir.join("main.nyk"), text);

        // A member: `function khione.new` is on line 6 of the module.
        let resp = request_in(&mut server, &uri, "textDocument/definition", 2, 3);
        assert!(
            resp.contains(&format!("\"uri\":{}", json_string(&target))),
            "{resp}"
        );
        assert!(resp.contains("\"line\":6"), "{resp}");
        assert!(resp.contains("\"character\":16"));
        // A value, and an exported type through the module name.
        let resp = request_in(&mut server, &uri, "textDocument/definition", 3, 14);
        assert!(resp.contains("\"line\":4"), "{resp}");
        let resp = request_in(&mut server, &uri, "textDocument/definition", 1, 13);
        assert!(resp.contains(&json_string(&target)));
        assert!(resp.contains("\"line\":1"), "{resp}");
        assert!(resp.contains("\"character\":12"));
        // The module name goes to the file, and so does the string.
        let resp = request_in(&mut server, &uri, "textDocument/definition", 0, 6);
        assert!(resp.contains(&json_string(&target)), "{resp}");
        let resp = request_in(&mut server, &uri, "textDocument/definition", 0, 22);
        assert!(resp.contains(&json_string(&target)), "{resp}");
        assert!(resp.contains("\"line\":0"));
        // Bundled modules have no file, and a missing one has none either.
        let resp = request_in(&mut server, &uri, "textDocument/definition", 4, 18);
        assert!(resp.contains("\"result\":null"), "{resp}");
        let resp = request_in(&mut server, &uri, "textDocument/definition", 5, 14);
        assert!(resp.contains("\"result\":null"), "{resp}");
        // A name of this document still goes to its declaration.
        let resp = request_in(&mut server, &uri, "textDocument/definition", 7, 8);
        assert!(resp.contains("\"line\":6"), "{resp}");
        assert!(resp.contains(&json_string(&uri)));
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_references_highlights_and_rename_follow_scopes() {
        let text = "local x = 1\nlocal function f(x)\n    return x + 1\nend\nprint(x)\n";
        let mut server = LspServer::new();
        open_document(&mut server, text);

        // The outer `x`: its declaration and the print, not the parameter.
        let resp = position_request(&mut server, "textDocument/references", 0, 6);
        assert_eq!(resp.matches("\"uri\":").count(), 2, "{resp}");
        assert!(resp.contains("\"line\":4"));
        assert!(!resp.contains("\"line\":2"));
        // Without the declaration.
        let req = r#"{"jsonrpc":"2.0","id":7,"method":"textDocument/references","params":{"textDocument":{"uri":"file:///test.nyk"},"position":{"line":0,"character":6},"context":{"includeDeclaration":false}}}"#;
        let resp = String::from_utf8(server.handle_message(req.as_bytes()).unwrap()).unwrap();
        assert_eq!(resp.matches("\"uri\":").count(), 1, "{resp}");

        // The parameter: a write at its declaration and a read.
        let resp = position_request(&mut server, "textDocument/documentHighlight", 2, 11);
        assert_eq!(resp.matches("\"range\":").count(), 2, "{resp}");
        assert!(resp.contains("\"kind\":3"));
        assert!(resp.contains("\"kind\":2"));

        // Rename the parameter: its two places, nothing else.
        let req = r#"{"jsonrpc":"2.0","id":8,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///test.nyk"},"position":{"line":2,"character":11},"newName":"count"}}"#;
        let resp = String::from_utf8(server.handle_message(req.as_bytes()).unwrap()).unwrap();
        assert_eq!(resp.matches("\"newText\":\"count\"").count(), 2, "{resp}");
        assert!(resp.contains("\"changes\":{\"file:///test.nyk\":["));
        assert!(!resp.contains("\"line\":4"));
        // And the outer one.
        let req = req.replace("\"line\":2,\"character\":11", "\"line\":0,\"character\":6");
        let resp = String::from_utf8(server.handle_message(req.as_bytes()).unwrap()).unwrap();
        assert_eq!(resp.matches("\"newText\":\"count\"").count(), 2, "{resp}");
        assert!(resp.contains("\"line\":4"));

        // Names that are not valid are refused, and so are keywords.
        for bad in ["", "1x", "a b", "end"] {
            let req = format!(
                r#"{{"jsonrpc":"2.0","id":9,"method":"textDocument/rename","params":{{"textDocument":{{"uri":"file:///test.nyk"}},"position":{{"line":0,"character":6}},"newName":{}}}}}"#,
                json_string(bad)
            );
            let resp = String::from_utf8(server.handle_message(req.as_bytes()).unwrap()).unwrap();
            assert!(resp.contains("\"code\":-32602"), "{bad}: {resp}");
        }
    }

    #[test]
    fn test_lsp_prepare_rename_rejects_names_that_cannot_be_renamed() {
        let text = "local t = {}\nt.field = print\nif t then end\n";
        let mut server = LspServer::new();
        open_document(&mut server, text);
        // A local can.
        let resp = position_request(&mut server, "textDocument/prepareRename", 0, 6);
        assert!(resp.contains("\"placeholder\":\"t\""), "{resp}");
        assert!(resp.contains("\"character\":6"));
        // A global of the standard library, a field and a keyword cannot.
        for (line, character) in [(1, 11), (1, 3), (2, 6)] {
            let resp = position_request(&mut server, "textDocument/prepareRename", line, character);
            assert!(
                resp.contains("\"result\":null"),
                "{line}:{character} {resp}"
            );
        }
        // Renaming a global is refused with a message.
        let req = r#"{"jsonrpc":"2.0","id":8,"method":"textDocument/rename","params":{"textDocument":{"uri":"file:///test.nyk"},"position":{"line":1,"character":11},"newName":"show"}}"#;
        let resp = String::from_utf8(server.handle_message(req.as_bytes()).unwrap()).unwrap();
        assert!(resp.contains("\"code\":-32602"), "{resp}");
        assert!(resp.contains("cannot be renamed"));
    }

    #[test]
    fn test_lsp_references_are_confined_to_the_document() {
        // The same names in a second document are not touched.
        let dir = scratch("confined");
        let mut server = LspServer::new();
        let one = open_file(
            &mut server,
            &dir.join("one.nyk"),
            "local value = 1\nprint(value)\n",
        );
        let _two = open_file(
            &mut server,
            &dir.join("two.nyk"),
            "local value = 2\nprint(value)\n",
        );
        let req = format!(
            r#"{{"jsonrpc":"2.0","id":8,"method":"textDocument/rename","params":{{"textDocument":{{"uri":{}}},"position":{{"line":0,"character":6}},"newName":"other"}}}}"#,
            json_string(&one)
        );
        let resp = String::from_utf8(server.handle_message(req.as_bytes()).unwrap()).unwrap();
        assert_eq!(resp.matches("\"newText\":\"other\"").count(), 2, "{resp}");
        assert!(!resp.contains("two.nyk"));
        cleanup(&dir);
    }

    #[test]
    fn test_lsp_folding_ranges_and_workspace_symbols() {
        let text = "-- a\n-- b\nfunction compute(x)\n    local t = {\n        1,\n    }\n    return x\nend\ntype Shape = { a: number }\n";
        let mut server = LspServer::new();
        open_document(&mut server, text);
        let req = r#"{"jsonrpc":"2.0","id":11,"method":"textDocument/foldingRange","params":{"textDocument":{"uri":"file:///test.nyk"}}}"#;
        let resp = String::from_utf8(server.handle_message(req.as_bytes()).unwrap()).unwrap();
        assert!(
            resp.contains("{\"startLine\":0,\"endLine\":1,\"kind\":\"comment\"}"),
            "{resp}"
        );
        assert!(resp.contains("{\"startLine\":2,\"endLine\":6}"), "{resp}");
        assert!(resp.contains("{\"startLine\":3,\"endLine\":4}"), "{resp}");

        let req =
            r#"{"jsonrpc":"2.0","id":12,"method":"workspace/symbol","params":{"query":"COMP"}}"#;
        let resp = String::from_utf8(server.handle_message(req.as_bytes()).unwrap()).unwrap();
        assert!(resp.contains("\"name\":\"compute\""), "{resp}");
        assert!(resp.contains("\"uri\":\"file:///test.nyk\""));
        assert!(!resp.contains("\"name\":\"Shape\""));
        let req =
            r#"{"jsonrpc":"2.0","id":13,"method":"workspace/symbol","params":{"query":"shape"}}"#;
        let resp = String::from_utf8(server.handle_message(req.as_bytes()).unwrap()).unwrap();
        assert!(resp.contains("\"name\":\"Shape\""), "{resp}");
        assert!(resp.contains("\"line\":8"));
    }

    #[test]
    fn test_lsp_diagnostics_carry_severity_tags_and_help() {
        let mut server = LspServer::new();
        let open = |server: &mut LspServer, text: &str| -> String {
            let req = format!(
                r#"{{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{{"textDocument":{{"uri":"file:///test.nyk","text":{}}}}}}}"#,
                json_string(text)
            );
            String::from_utf8(server.handle_message(req.as_bytes()).unwrap()).unwrap()
        };
        // Unused code is faded out.
        let resp = open(&mut server, "local unused = 10\n");
        assert!(resp.contains("\"code\":\"W0001\""), "{resp}");
        assert!(resp.contains("\"severity\":2"));
        assert!(resp.contains("\"tags\":[1]"));
        // A syntax error is an error and is not faded.
        let resp = open(&mut server, "local x = \n");
        assert!(resp.contains("\"severity\":1"), "{resp}");
        assert!(!resp.contains("\"tags\""));
        // A clean document has none.
        let resp = open(&mut server, "print(1)\n");
        assert!(resp.contains("\"diagnostics\":[]"), "{resp}");
        // Columns are UTF-16 units: the crab is two of them.
        let resp = open(&mut server, "local s = \"\u{1f980}\"; local unused = 1\n");
        let before = "local s = \"\u{1f980}\"; local ";
        let column = char_col_to_utf16_col(before, before.chars().count());
        assert!(resp.contains(&format!("\"character\":{column}")), "{resp}");
    }
}
