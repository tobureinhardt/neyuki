//! The tokenizer.

pub mod comments;
pub mod numbers;
pub mod scanner;
pub mod strings;
pub mod words;

pub use comments::{CommentEngine, DocComment};
pub use numbers::{NumberEngine, NumberFormat, NumberResult, NumberType};
pub use scanner::{EscapeWarning, Scanner, SourcePos};
pub use strings::StringEngine;
pub use words::WordEngine;

use crate::lexer::Token;

pub struct TokenizerEngine<'a> {
    scanner: Scanner<'a>,
    can_start_comment: bool,
}

impl<'a> TokenizerEngine<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            scanner: Scanner::new(source),
            can_start_comment: true,
        }
    }

    pub fn tokenize(&mut self) -> Result<Vec<Token>, String> {
        let (tokens, _) = self.tokenize_with_comments()?;
        Ok(tokens)
    }

    /// Takes the `\xHH` and `\ddd` escapes above ASCII seen so far. Call
    /// after tokenizing; see `W0006`.
    pub fn take_escape_warnings(&mut self) -> Vec<EscapeWarning> {
        self.scanner.take_escape_warnings()
    }

    pub fn tokenize_with_comments(&mut self) -> Result<(Vec<Token>, Vec<DocComment>), String> {
        let mut tokens = Vec::new();
        let mut doc_comments = Vec::new();

        while !self.scanner.is_at_end() {
            self.skip_whitespace_and_comments(&mut doc_comments);
            if self.scanner.is_at_end() {
                break;
            }

            let start_line = self.scanner.line();
            let start_col = self.scanner.col();
            let ch = self.scanner.peek();

            if WordEngine::is_identifier_start(ch) {
                let word = WordEngine::scan_word(&mut self.scanner);
                let kind = WordEngine::classify_word(word);
                tokens.push(Token {
                    kind,
                    value: word.to_string(),
                    line: start_line,
                    col: start_col,
                });
                self.can_start_comment = false;
            } else if NumberEngine::is_number_start(&self.scanner) {
                let num = NumberEngine::scan_number(&mut self.scanner);
                tokens.push(Token {
                    kind: "number",
                    value: num.raw.to_string(),
                    line: start_line,
                    col: start_col,
                });
                self.can_start_comment = false;
            } else if StringEngine::is_string_start(ch) {
                let s = StringEngine::scan_quoted_string(&mut self.scanner, ch)?;
                tokens.push(Token {
                    kind: "string",
                    value: s,
                    line: start_line,
                    col: start_col,
                });
                self.can_start_comment = false;
            } else if StringEngine::is_long_bracket_start(&self.scanner) {
                let s = StringEngine::scan_long_bracket(&mut self.scanner)?;
                tokens.push(Token {
                    kind: "string",
                    value: s,
                    line: start_line,
                    col: start_col,
                });
                self.can_start_comment = false;
            } else if StringEngine::is_interp_start(ch) {
                let s = StringEngine::scan_interpolated_string(&mut self.scanner)?;
                tokens.push(Token {
                    kind: "interp",
                    value: s,
                    line: start_line,
                    col: start_col,
                });
                self.can_start_comment = false;
            } else {
                if ch == '\0' {
                    return Err(format!("[line {}] unexpected null byte", start_line));
                }
                let sym = self.read_fast_symbol();
                tokens.push(Token {
                    kind: "symbol",
                    value: sym,
                    line: start_line,
                    col: start_col,
                });
                self.can_start_comment = false;
            }
        }

        tokens.push(Token {
            kind: "eof",
            value: String::new(),
            line: self.scanner.line(),
            col: self.scanner.col(),
        });

        Ok((tokens, doc_comments))
    }

    fn skip_whitespace_and_comments(&mut self, comments: &mut Vec<DocComment>) {
        while !self.scanner.is_at_end() {
            let ch = self.scanner.peek();
            match ch {
                ' ' | '\t' | '\r' | '\n' => {
                    self.scanner.advance();
                    self.can_start_comment = true;
                }
                '-' if self.scanner.starts_with("--") && self.can_start_comment => {
                    if let Some(comment) = CommentEngine::scan_comment(&mut self.scanner) {
                        comments.push(comment);
                    }
                    self.can_start_comment = true;
                }
                _ => break,
            }
        }
    }

    fn read_fast_symbol(&mut self) -> String {
        let ch = self.scanner.peek();

        // Optimized tiered lookup based on initial character
        match ch {
            '<' => {
                if self.scanner.starts_with("<<<=") {
                    self.scanner.advance_bytes(4);
                    return "<<<=".to_string();
                }
                if self.scanner.starts_with("<<<") {
                    self.scanner.advance_bytes(3);
                    return "<<<".to_string();
                }
                if self.scanner.starts_with("<<=") {
                    self.scanner.advance_bytes(3);
                    return "<<=".to_string();
                }
                if self.scanner.starts_with("<<") {
                    self.scanner.advance_bytes(2);
                    return "<<".to_string();
                }
                if self.scanner.starts_with("<=") {
                    self.scanner.advance_bytes(2);
                    return "<=".to_string();
                }
                self.scanner.advance();
                "<".to_string()
            }
            '>' => {
                if self.scanner.starts_with(">>>=") {
                    self.scanner.advance_bytes(4);
                    return ">>>=".to_string();
                }
                if self.scanner.starts_with(">>>") {
                    self.scanner.advance_bytes(3);
                    return ">>>".to_string();
                }
                if self.scanner.starts_with(">>=") {
                    self.scanner.advance_bytes(3);
                    return ">>=".to_string();
                }
                if self.scanner.starts_with(">>") {
                    self.scanner.advance_bytes(2);
                    return ">>".to_string();
                }
                if self.scanner.starts_with(">=") {
                    self.scanner.advance_bytes(2);
                    return ">=".to_string();
                }
                self.scanner.advance();
                ">".to_string()
            }
            '=' => {
                if self.scanner.starts_with("==") {
                    self.scanner.advance_bytes(2);
                    "==".to_string()
                } else {
                    self.scanner.advance();
                    "=".to_string()
                }
            }
            '!' => {
                if self.scanner.starts_with("!=") {
                    self.scanner.advance_bytes(2);
                    "!=".to_string()
                } else {
                    self.scanner.advance();
                    "!".to_string()
                }
            }
            '~' => {
                if self.scanner.starts_with("~=") {
                    self.scanner.advance_bytes(2);
                    "~=".to_string()
                } else {
                    self.scanner.advance();
                    "~".to_string()
                }
            }
            '?' => {
                if self.scanner.starts_with("??=") {
                    self.scanner.advance_bytes(3);
                    "??=".to_string()
                } else if self.scanner.starts_with("??") {
                    self.scanner.advance_bytes(2);
                    "??".to_string()
                } else {
                    self.scanner.advance();
                    "?".to_string()
                }
            }
            '.' => {
                if self.scanner.starts_with("...") {
                    self.scanner.advance_bytes(3);
                    "...".to_string()
                } else if self.scanner.starts_with("..=") {
                    self.scanner.advance_bytes(3);
                    "..=".to_string()
                } else if self.scanner.starts_with("..") {
                    self.scanner.advance_bytes(2);
                    "..".to_string()
                } else {
                    self.scanner.advance();
                    ".".to_string()
                }
            }
            '+' => {
                if self.scanner.starts_with("++") {
                    self.scanner.advance_bytes(2);
                    "++".to_string()
                } else if self.scanner.starts_with("+=") {
                    self.scanner.advance_bytes(2);
                    "+=".to_string()
                } else {
                    self.scanner.advance();
                    "+".to_string()
                }
            }
            '-' => {
                if self.scanner.starts_with("--") {
                    self.scanner.advance_bytes(2);
                    "--".to_string()
                } else if self.scanner.starts_with("-=") {
                    self.scanner.advance_bytes(2);
                    "-=".to_string()
                } else if self.scanner.starts_with("->") {
                    self.scanner.advance_bytes(2);
                    "->".to_string()
                } else {
                    self.scanner.advance();
                    "-".to_string()
                }
            }
            '*' => {
                if self.scanner.starts_with("*=") {
                    self.scanner.advance_bytes(2);
                    "*=".to_string()
                } else {
                    self.scanner.advance();
                    "*".to_string()
                }
            }
            '/' => {
                if self.scanner.starts_with("//=") {
                    self.scanner.advance_bytes(3);
                    "//=".to_string()
                } else if self.scanner.starts_with("//") {
                    self.scanner.advance_bytes(2);
                    "//".to_string()
                } else if self.scanner.starts_with("/=") {
                    self.scanner.advance_bytes(2);
                    "/=".to_string()
                } else {
                    self.scanner.advance();
                    "/".to_string()
                }
            }
            '%' => {
                if self.scanner.starts_with("%=") {
                    self.scanner.advance_bytes(2);
                    "%=".to_string()
                } else {
                    self.scanner.advance();
                    "%".to_string()
                }
            }
            '^' => {
                if self.scanner.starts_with("^=") {
                    self.scanner.advance_bytes(2);
                    "^=".to_string()
                } else {
                    self.scanner.advance();
                    "^".to_string()
                }
            }
            '&' => {
                if self.scanner.starts_with("&=") {
                    self.scanner.advance_bytes(2);
                    "&=".to_string()
                } else {
                    self.scanner.advance();
                    "&".to_string()
                }
            }
            '|' => {
                if self.scanner.starts_with("|=") {
                    self.scanner.advance_bytes(2);
                    "|=".to_string()
                } else {
                    self.scanner.advance();
                    "|".to_string()
                }
            }
            ':' => {
                if self.scanner.starts_with("::") {
                    self.scanner.advance_bytes(2);
                    "::".to_string()
                } else {
                    self.scanner.advance();
                    ":".to_string()
                }
            }
            '(' | ')' | '[' | ']' | '{' | '}' | ';' | ',' | '#' => {
                let advanced = self.scanner.advance();
                advanced.to_string()
            }
            _ => {
                let advanced = self.scanner.advance();
                if advanced == '\0' {
                    unreachable!("null bytes are rejected before symbol scanning");
                }
                advanced.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenizer_numbers_and_symbols() {
        let mut tokenizer = TokenizerEngine::new("0x1A + 0b101 * 3.14e-2");
        let tokens = tokenizer.tokenize().unwrap();
        assert_eq!(tokens[0].kind, "number");
        assert_eq!(tokens[0].value, "0x1A");
        assert_eq!(tokens[1].kind, "symbol");
        assert_eq!(tokens[1].value, "+");
        assert_eq!(tokens[2].kind, "number");
        assert_eq!(tokens[2].value, "0b101");
        assert_eq!(tokens[3].kind, "symbol");
        assert_eq!(tokens[3].value, "*");
        assert_eq!(tokens[4].kind, "number");
        assert_eq!(tokens[4].value, "3.14e-2");
    }

    #[test]
    fn test_tokenizer_doc_comments() {
        let source = "---@param name string\nlocal name = \"neyuki\"";
        let mut tokenizer = TokenizerEngine::new(source);
        let (tokens, docs) = tokenizer.tokenize_with_comments().unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].tag.as_deref(), Some("@param"));
        assert_eq!(tokens[0].kind, "keyword");
        assert_eq!(tokens[0].value, "local");
    }

    #[test]
    fn test_tokenizer_interpolated_string() {
        let mut tokenizer = TokenizerEngine::new("`hello {user}!`");
        let tokens = tokenizer.tokenize().unwrap();
        assert_eq!(tokens[0].kind, "interp");
        assert_eq!(tokens[0].value, "L6:hello E4:userL1:!");
    }
}
