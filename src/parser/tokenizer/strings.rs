//! String literals and interpolation.

use super::scanner::Scanner;

pub struct StringEngine;

impl StringEngine {
    #[inline(always)]
    pub fn is_string_start(ch: char) -> bool {
        ch == '"' || ch == '\''
    }

    #[inline(always)]
    pub fn is_long_bracket_start(scanner: &Scanner<'_>) -> bool {
        scanner.peek() == '[' && scanner.peek_next().is_some_and(|c| c == '[' || c == '=')
    }

    #[inline(always)]
    pub fn is_interp_start(ch: char) -> bool {
        ch == '`'
    }

    pub fn scan_quoted_string(scanner: &mut Scanner<'_>, quote: char) -> Result<String, String> {
        let start_line = scanner.line();
        scanner.advance(); // consume opening quote
        let mut out = String::new();

        while !scanner.is_at_end() {
            let ch = scanner.advance();
            if ch == quote {
                return Ok(out);
            }
            if ch == '\\' {
                Self::scan_escape(scanner, &mut out)?;
            } else {
                out.push(ch);
            }
        }

        Err(format!("[line {}] unfinished string literal", start_line))
    }

    /// Decodes the escape after a backslash into `out`: the C escapes,
    /// `\ddd` (decimal, up to 255), `\xHH`, `\u{H...}`, `\z` (skips the
    /// whitespace that follows) and a backslash before a line break (keeps
    /// the break). Any other character stands for itself, so `\"`, `\{` and
    /// `` \` `` work.
    fn scan_escape(scanner: &mut Scanner<'_>, out: &mut String) -> Result<(), String> {
        if scanner.is_at_end() {
            return Err(format!(
                "[line {}] unfinished string escape at EOF",
                scanner.line()
            ));
        }
        // The caller has already consumed the backslash, so it sits one
        // column back; `esc_start` is where the escape's own text begins.
        let esc_line = scanner.line();
        let esc_col = scanner.col().saturating_sub(1).max(1);
        let esc_start = scanner.pos();
        let esc = scanner.advance();
        match esc {
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0C'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\x0B'),
            '0'..='9' => {
                let mut value = esc.to_digit(10).unwrap_or(0);
                for _ in 0..2 {
                    if scanner.is_at_end() {
                        break;
                    }
                    let Some(digit) = scanner.peek().to_digit(10) else {
                        break;
                    };
                    value = value * 10 + digit;
                    scanner.advance();
                }
                if value > 255 {
                    return Err(format!(
                        "[line {}] invalid escape \\{}: decimal escapes go up to 255",
                        scanner.line(),
                        value
                    ));
                }
                if value >= 0x80 {
                    let text = format!("{}{}", '\\', scanner.slice_from(esc_start));
                    scanner.warn_escape(esc_line, esc_col, text, value);
                }
                out.push(char::from(value as u8));
            }
            'x' => {
                let hi = scanner.advance();
                let lo = scanner.advance();
                let hex = format!("{}{}", hi, lo);
                let value = u8::from_str_radix(&hex, 16).map_err(|_| {
                    format!(
                        "[line {}] invalid escape \\x{}: expected two hex digits",
                        scanner.line(),
                        hex
                    )
                })?;
                if value >= 0x80 {
                    let text = format!("{}{}", '\\', scanner.slice_from(esc_start));
                    scanner.warn_escape(esc_line, esc_col, text, u32::from(value));
                }
                out.push(value as char);
            }
            'u' if scanner.peek() == '{' => {
                scanner.advance();
                let mut digits = String::new();
                while !scanner.is_at_end() && scanner.peek() != '}' {
                    digits.push(scanner.advance());
                }
                if scanner.peek() != '}' {
                    return Err(format!(
                        "[line {}] unfinished escape \\u{{{}",
                        scanner.line(),
                        digits
                    ));
                }
                scanner.advance();
                let c = u32::from_str_radix(&digits, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| {
                        format!(
                            "[line {}] invalid escape \\u{{{}}}: not a Unicode scalar value",
                            scanner.line(),
                            digits
                        )
                    })?;
                out.push(c);
            }
            'z' => {
                while !scanner.is_at_end() && matches!(scanner.peek(), ' ' | '\t' | '\r' | '\n') {
                    scanner.advance();
                }
            }
            '\r' => {
                if scanner.peek() == '\n' {
                    scanner.advance();
                }
                out.push('\n');
            }
            _ => out.push(esc),
        }
        Ok(())
    }

    pub fn scan_long_bracket(scanner: &mut Scanner<'_>) -> Result<String, String> {
        let start_pos = scanner.current_pos();
        let start_line = scanner.line();
        if scanner.peek() != '[' {
            return Err(format!("[line {}] long bracket expected", start_line));
        }
        scanner.advance();

        let mut level = 0;
        while !scanner.is_at_end() && scanner.peek() == '=' {
            level += 1;
            scanner.advance();
        }

        if scanner.peek() != '[' {
            scanner.restore(start_pos);
            return Err(format!("[line {}] long bracket expected", start_line));
        }
        scanner.advance();

        // Skip immediately following newline if any
        if scanner.peek() == '\r' {
            scanner.advance();
        }
        if scanner.peek() == '\n' {
            scanner.advance();
        }

        let closer = format!("]{}]", "=".repeat(level));
        let content_start = scanner.pos();
        let mut found = None;

        while !scanner.is_at_end() {
            if scanner.starts_with(&closer) {
                found = Some(scanner.pos());
                break;
            }
            scanner.advance();
        }

        let Some(index) = found else {
            scanner.restore(start_pos);
            return Err(format!("[line {}] unfinished long bracket", start_line));
        };

        let content = scanner.slice_from(content_start)[..index - content_start].to_owned();
        scanner.advance_bytes(closer.len());
        Ok(content)
    }

    /// Scans a backtick string into its parts, encoded for the parser as
    /// `L<len>:<text>` for literal text (escapes already decoded) and
    /// `E<len>:<source>` for an expression in braces, with lengths in bytes,
    /// so no character in the text can be mistaken for a delimiter.
    pub fn scan_interpolated_string(scanner: &mut Scanner<'_>) -> Result<String, String> {
        let start_line = scanner.line();
        scanner.advance(); // consume opening backtick
        let mut encoded = String::new();
        let mut literal = String::new();

        while !scanner.is_at_end() {
            let ch = scanner.advance();
            match ch {
                '`' => {
                    push_part(&mut encoded, 'L', &literal);
                    return Ok(encoded);
                }
                '\\' => Self::scan_escape(scanner, &mut literal)?,
                '{' => {
                    push_part(&mut encoded, 'L', &literal);
                    literal.clear();
                    let expression = Self::scan_interp_expression(scanner, start_line)?;
                    push_part(&mut encoded, 'E', &expression);
                }
                _ => literal.push(ch),
            }
        }

        Err(format!(
            "[line {}] unfinished interpolated string",
            start_line
        ))
    }

    /// The source of an interpolated expression up to its closing brace,
    /// stepping over braces in nested tables and in string literals.
    fn scan_interp_expression(
        scanner: &mut Scanner<'_>,
        start_line: usize,
    ) -> Result<String, String> {
        let mut depth = 1;
        let mut source = String::new();
        while !scanner.is_at_end() {
            let c = scanner.advance();
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(source);
                    }
                }
                '"' | '\'' => {
                    source.push(c);
                    while !scanner.is_at_end() {
                        let inner = scanner.advance();
                        source.push(inner);
                        if inner == '\\' && !scanner.is_at_end() {
                            source.push(scanner.advance());
                        } else if inner == c {
                            break;
                        }
                    }
                    continue;
                }
                _ => {}
            }
            source.push(c);
        }
        Err(format!(
            "[line {}] unfinished expression in interpolated string",
            start_line
        ))
    }
}

fn push_part(encoded: &mut String, tag: char, text: &str) {
    if tag == 'L' && text.is_empty() {
        return;
    }
    encoded.push(tag);
    encoded.push_str(&text.len().to_string());
    encoded.push(':');
    encoded.push_str(text);
}
