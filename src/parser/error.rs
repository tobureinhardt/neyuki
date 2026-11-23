//! Parse errors and their diagnostics.

use std::fmt;

use crate::diagnostics::diagnostic::Diagnostic;
use crate::diagnostics::error_code::ErrorCode as DiagErrorCode;
use crate::diagnostics::span::Span;
use crate::lexer::Token;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    UnexpectedToken = 1001,
    UnfinishedConstruct = 1002,
    InvalidAssignment = 1003,
    TypeSyntaxError = 1004,
    ExpectedIdentifier = 1005,
    UnmatchedDelimiter = 1006,
    GeneralSyntaxError = 1099,
}

impl ErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UnexpectedToken => "E1001",
            Self::UnfinishedConstruct => "E1002",
            Self::InvalidAssignment => "E1003",
            Self::TypeSyntaxError => "E1004",
            Self::ExpectedIdentifier => "E1005",
            Self::UnmatchedDelimiter => "E1006",
            Self::GeneralSyntaxError => "E1099",
        }
    }

    pub fn to_diag_code(self) -> DiagErrorCode {
        match self {
            Self::UnfinishedConstruct | Self::UnmatchedDelimiter => DiagErrorCode::E0050,
            Self::UnexpectedToken => DiagErrorCode::E0051,
            _ => DiagErrorCode::E0050,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticLabel {
    pub line: usize,
    pub col: usize,
    pub len: usize,
    pub message: String,
    pub is_primary: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub col: usize,
    pub code: ErrorCode,
    pub message: String,
    pub hint: Option<String>,
    pub labels: Vec<DiagnosticLabel>,
}

impl ParseError {
    pub fn new(line: usize, col: usize, message: impl Into<String>) -> Self {
        Self {
            line,
            col,
            code: ErrorCode::GeneralSyntaxError,
            message: message.into(),
            hint: None,
            labels: Vec::new(),
        }
    }

    pub fn with_code(line: usize, col: usize, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            line,
            col,
            code,
            message: message.into(),
            hint: None,
            labels: Vec::new(),
        }
    }

    pub fn with_hint(
        line: usize,
        col: usize,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            line,
            col,
            code: ErrorCode::GeneralSyntaxError,
            message: message.into(),
            hint: Some(hint.into()),
            labels: Vec::new(),
        }
    }

    pub fn add_label(
        &mut self,
        line: usize,
        col: usize,
        len: usize,
        msg: impl Into<String>,
        is_primary: bool,
    ) {
        self.labels.push(DiagnosticLabel {
            line,
            col,
            len: if len == 0 { 1 } else { len },
            message: msg.into(),
            is_primary,
        });
    }

    pub fn format_diagnostic(&self, source_line: Option<&str>) -> String {
        let mut out = format!(
            "[{}] [line {}:{}] {}",
            self.code.as_str(),
            self.line,
            self.col,
            self.message
        );
        if let Some(src) = source_line {
            out.push('\n');
            out.push_str(src);
            out.push('\n');
            let indent = " ".repeat(self.col.saturating_sub(1));
            out.push_str(&format!("{}^", indent));
        }
        for label in &self.labels {
            out.push_str(&format!(
                "\n  --> line {}:{}: {}",
                label.line, label.col, label.message
            ));
        }
        if let Some(hint) = &self.hint {
            out.push_str(&format!("\nHint: {}", hint));
        }
        out
    }

    /// Builds a structured `ParseError` from a parser message like
    /// `"[line 4] expected symbol \")\""` and the current lookahead token.
    pub fn from_parser_message(raw: &str, tok: &Token) -> Self {
        let (line, message) = raw
            .strip_prefix("[line ")
            .and_then(|rest| rest.split_once("] "))
            .and_then(|(l, m)| Some((l.parse::<usize>().ok()?, m.to_string())))
            .unwrap_or_else(|| (tok.line.max(1), raw.to_string()));

        let col = if line == tok.line && tok.col > 0 {
            tok.col
        } else {
            1
        };
        let len = if line == tok.line {
            tok.value.chars().count().max(1)
        } else {
            1
        };

        let code = if message.contains("expected name") || message.contains("expected identifier") {
            ErrorCode::ExpectedIdentifier
        } else if message.contains("unfinished")
            || message.contains("expected keyword \"end\"")
            || message.contains("expected symbol \")\"")
            || message.contains("expected symbol \"}\"")
            || message.contains("expected symbol \"]\"")
        {
            ErrorCode::UnfinishedConstruct
        } else if message.contains("unmatched") || message.contains("at top level") {
            ErrorCode::UnmatchedDelimiter
        } else if message.contains("assignment") || message.contains("assign target") {
            ErrorCode::InvalidAssignment
        } else if message.contains("type") && !message.contains("type alias") {
            ErrorCode::TypeSyntaxError
        } else if message.contains("unexpected token") {
            ErrorCode::UnexpectedToken
        } else {
            ErrorCode::GeneralSyntaxError
        };

        let hint = if message.contains("expected keyword \"end\"") {
            Some("add a matching 'end' to close this block".to_string())
        } else if message.contains("expected symbol \")\"") {
            Some("close the parenthesized expression or argument list with ')'".to_string())
        } else if message.contains("expected symbol \"}\"") {
            Some("close the table literal with '}'".to_string())
        } else if message.contains("expected symbol \"]\"") {
            Some("close the bracketed index or attribute with ']'".to_string())
        } else if message.contains("expected keyword \"then\"") {
            Some("add 'then' after the condition".to_string())
        } else if message.contains("expected keyword \"do\"") {
            Some("add 'do' before the loop body".to_string())
        } else {
            None
        };

        let mut err = Self::with_code(line, col, code, message);
        err.hint = hint;
        err.add_label(line, col, len, "syntax error", true);
        err
    }

    /// Converts this `ParseError` into a compiler/LSP `Diagnostic`.
    pub fn to_diagnostic(&self) -> Diagnostic {
        let line = self.line.max(1) as u32;
        let col = self.col.max(1) as u32;
        let len = self
            .labels
            .first()
            .map(|l| l.len.max(1) as u32)
            .unwrap_or(1);
        let span = Span::single_line(line, col, col.saturating_add(len));
        let mut diag = Diagnostic::error(self.message.clone())
            .with_code(self.code.to_diag_code())
            .with_label(span, "syntax error");
        if let Some(hint) = &self.hint {
            diag = diag.with_help(hint.clone());
        }
        diag
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[line {}] {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

impl From<ParseError> for String {
    fn from(err: ParseError) -> Self {
        err.to_string()
    }
}
