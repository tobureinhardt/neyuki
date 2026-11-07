//! Source code formatter for Neyuki (`neyuki fmt`).
//!
//! # Architecture
//!
//! The formatter operates using a stateful line-and-block indentation engine:
//! - Handles nested blocks (`function`, `if`/`elseif`/`else`, `for`, `while`,
//!   `do`, `repeat`/`until`, `enum`, `match`/`case`/`default`).
//! - Tracks multi-line statement headers (`if` conditions spanning lines until `then`,
//!   `for`/`while` headers spanning lines until `do`) with continuation indentation.
//! - Tracks multi-line delimiter pairs (`( )`, `[ ]`, `{ }`).
//! - Preserves raw multiline strings (`[=*[...]=*]`) and multiline comments (`--[=*[...]=*]`) verbatim.
//! - Whitespace inside single-line expressions is preserved to protect custom tabular alignments.
//! - Every formatting operation verifies that the token stream (`kind` and `value`) and
//!   parsed syntax are 100% identical to the original source.

use std::fs;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockKind {
    Match { has_case: bool },
    Normal,
    PendingIf,
    PendingLoop,
    Paren { adds_indent: bool },
    Bracket { adds_indent: bool },
    Brace { adds_indent: bool },
}

/// Checks whether a line starts with a specific keyword at a word boundary.
fn starts_with_keyword(line: &str, kw: &str) -> bool {
    if !line.starts_with(kw) {
        return false;
    }
    let rest = &line[kw.len()..];
    rest.is_empty() || !rest.starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
}

fn find_keyword_pos(s: &str, kw: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(idx) = s[start..].find(kw) {
        let abs_idx = start + idx;
        let before_ok = abs_idx == 0
            || !s[..abs_idx].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
        let after_idx = abs_idx + kw.len();
        let after_ok = after_idx == s.len()
            || !s[after_idx..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
        if before_ok && after_ok {
            return Some(abs_idx);
        }
        start = abs_idx + kw.len();
    }
    None
}

fn has_word_after(s: &str, pos: usize, word: &str) -> bool {
    if pos >= s.len() {
        false
    } else {
        has_word(&s[pos..], word)
    }
}

fn count_unmatched(s: &str, open: char, close: char) -> usize {
    let mut depth: usize = 0;
    for c in s.chars() {
        if c == open {
            depth += 1;
        } else if c == close {
            depth = depth.saturating_sub(1);
        }
    }
    depth
}

/// Formats a Neyuki source string.
pub fn format_source(source: &str) -> Result<String, String> {
    // 1. Verify syntax before attempting to format
    crate::compiler::parse(source)
        .map_err(|err| format!("cannot format invalid source: {}", err))?;

    let newline = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut out = String::new();
    let mut prev_was_empty = false;
    let mut multiline_closer: Option<String> = None;
    let mut block_stack: Vec<BlockKind> = Vec::new();
    let mut indent_level: usize = 0;

    for line in source.lines() {
        // If inside a multiline raw string or long comment, preserve verbatim
        if let Some(ref closer) = multiline_closer {
            out.push_str(line);
            out.push_str(newline);
            if let Some(pos) = line.find(closer) {
                let after = &line[pos + closer.len()..];
                multiline_closer = find_unclosed_multiline(after);
            }
            continue;
        }

        let trimmed = line.trim();

        if trimmed.is_empty() {
            if !prev_was_empty && !out.is_empty() {
                out.push_str(newline);
                prev_was_empty = true;
            }
            continue;
        }
        prev_was_empty = false;

        let stripped = strip_strings_and_comments(trimmed);

        let is_case_or_default = (starts_with_keyword(trimmed, "case")
            || starts_with_keyword(trimmed, "default"))
            && trimmed.ends_with(':');

        let starts_with_closer = starts_with_keyword(trimmed, "end")
            || starts_with_keyword(trimmed, "until")
            || trimmed.starts_with('}')
            || trimmed.starts_with(')')
            || trimmed.starts_with(']');

        let is_else_or_elseif =
            starts_with_keyword(trimmed, "else") || starts_with_keyword(trimmed, "elseif");

        // Calculate current line indentation:
        let current_indent = if is_case_or_default {
            if let Some(BlockKind::Match { has_case: true }) = block_stack.last() {
                indent_level.saturating_sub(1)
            } else {
                indent_level
            }
        } else if starts_with_closer {
            match block_stack.last() {
                Some(BlockKind::Match { has_case: true })
                    if starts_with_keyword(trimmed, "end") =>
                {
                    indent_level.saturating_sub(2)
                }
                Some(BlockKind::Paren { adds_indent: false }) if trimmed.starts_with(')') => {
                    indent_level
                }
                Some(BlockKind::Brace { adds_indent: false }) if trimmed.starts_with('}') => {
                    indent_level
                }
                Some(BlockKind::Bracket { adds_indent: false }) if trimmed.starts_with(']') => {
                    indent_level
                }
                _ => indent_level.saturating_sub(1),
            }
        } else if is_else_or_elseif {
            indent_level.saturating_sub(1)
        } else {
            indent_level
        };

        // Emit indentation (4 spaces per level)
        let spaces = "    ".repeat(current_indent);
        out.push_str(&spaces);

        // Normalize spacing on the line
        let normalized_line = normalize_line_spacing(trimmed);
        out.push_str(&normalized_line);
        out.push_str(newline);

        // Check if a multiline raw string or comment started on this line
        if let Some(closer) = find_unclosed_multiline(trimmed) {
            multiline_closer = Some(closer);
        }

        // Update block stack and indentation for subsequent lines
        if is_case_or_default {
            if let Some(BlockKind::Match { has_case }) = block_stack.last_mut()
                && !*has_case
            {
                *has_case = true;
                indent_level += 1;
            }
        } else {
            // 1. Check if this line satisfies any pending headers (then / do)
            if has_word(&stripped, "then")
                && let Some(BlockKind::PendingIf) = block_stack.last()
            {
                block_stack.pop();
                if !has_word(&stripped, "end") {
                    block_stack.push(BlockKind::Normal);
                } else {
                    indent_level = indent_level.saturating_sub(1);
                }
            }
            if has_word(&stripped, "do")
                && let Some(BlockKind::PendingLoop) = block_stack.last()
            {
                block_stack.pop();
                if !has_word(&stripped, "end") {
                    block_stack.push(BlockKind::Normal);
                } else {
                    indent_level = indent_level.saturating_sub(1);
                }
            }

            // 2. Check closing keywords
            if starts_with_keyword(trimmed, "end") {
                if let Some(top) = block_stack.pop() {
                    match top {
                        BlockKind::Match { has_case: true } => {
                            indent_level = indent_level.saturating_sub(2);
                        }
                        BlockKind::Match { has_case: false } | BlockKind::Normal => {
                            indent_level = indent_level.saturating_sub(1);
                        }
                        BlockKind::Paren { adds_indent }
                        | BlockKind::Brace { adds_indent }
                        | BlockKind::Bracket { adds_indent } => {
                            if adds_indent {
                                indent_level = indent_level.saturating_sub(1);
                            }
                        }
                        _ => {
                            indent_level = indent_level.saturating_sub(1);
                        }
                    }
                }
            } else if starts_with_keyword(trimmed, "until") {
                block_stack.pop();
                indent_level = indent_level.saturating_sub(1);
            }

            // 3. Delimiters closed on this line
            let open_braces = stripped.chars().filter(|&c| c == '{').count();
            let close_braces = stripped.chars().filter(|&c| c == '}').count();
            if close_braces > open_braces {
                for _ in 0..(close_braces - open_braces) {
                    if let Some(BlockKind::Brace { adds_indent }) = block_stack.last() {
                        if *adds_indent {
                            indent_level = indent_level.saturating_sub(1);
                        }
                        block_stack.pop();
                    }
                }
            }

            let open_parens = stripped.chars().filter(|&c| c == '(').count();
            let close_parens = stripped.chars().filter(|&c| c == ')').count();
            if close_parens > open_parens {
                for _ in 0..(close_parens - open_parens) {
                    if let Some(BlockKind::Paren { adds_indent }) = block_stack.last() {
                        if *adds_indent {
                            indent_level = indent_level.saturating_sub(1);
                        }
                        block_stack.pop();
                    }
                }
            }

            let open_brackets = stripped.chars().filter(|&c| c == '[').count();
            let close_brackets = stripped.chars().filter(|&c| c == ']').count();
            if close_brackets > open_brackets {
                for _ in 0..(close_brackets - open_brackets) {
                    if let Some(BlockKind::Bracket { adds_indent }) = block_stack.last() {
                        if *adds_indent {
                            indent_level = indent_level.saturating_sub(1);
                        }
                        block_stack.pop();
                    }
                }
            }

            // 4. Check for starting multi-line headers or blocks
            let is_if_start = starts_with_keyword(trimmed, "if")
                || (starts_with_keyword(trimmed, "elseif") && !has_word(&stripped, "then"));
            let is_loop_start = (starts_with_keyword(trimmed, "for")
                || starts_with_keyword(trimmed, "while"))
                && !has_word(&stripped, "do");

            if is_if_start && !has_word(&stripped, "then") {
                if starts_with_keyword(trimmed, "elseif")
                    && let Some(BlockKind::Normal) = block_stack.last()
                {
                    block_stack.pop();
                    indent_level = indent_level.saturating_sub(1);
                }
                block_stack.push(BlockKind::PendingIf);
                indent_level += 1;
            } else if is_loop_start {
                block_stack.push(BlockKind::PendingLoop);
                indent_level += 1;
            } else {
                let fn_opt = find_keyword_pos(&stripped, "function");
                let opens_fn = fn_opt.is_some_and(|pos| !has_word_after(&stripped, pos + 8, "end"));

                if opens_fn {
                    let fn_pos = fn_opt.expect("opens_fn implies a `function` keyword");
                    let before_fn = &stripped[..fn_pos];
                    let parens_before = count_unmatched(before_fn, '(', ')');
                    let braces_before = count_unmatched(before_fn, '{', '}');
                    let brackets_before = count_unmatched(before_fn, '[', ']');

                    // Push enclosing delimiters before callback (no extra indent)
                    for _ in 0..braces_before {
                        block_stack.push(BlockKind::Brace { adds_indent: false });
                    }
                    for _ in 0..parens_before {
                        block_stack.push(BlockKind::Paren { adds_indent: false });
                    }
                    for _ in 0..brackets_before {
                        block_stack.push(BlockKind::Bracket { adds_indent: false });
                    }

                    // Push function block
                    block_stack.push(BlockKind::Normal);
                    indent_level += 1;

                    // Delimiters opened after function
                    let after_fn = &stripped[fn_pos + 8..];
                    let parens_after = count_unmatched(after_fn, '(', ')');
                    let braces_after = count_unmatched(after_fn, '{', '}');
                    let brackets_after = count_unmatched(after_fn, '[', ']');

                    for _ in 0..braces_after {
                        block_stack.push(BlockKind::Brace { adds_indent: true });
                        indent_level += 1;
                    }
                    for _ in 0..parens_after {
                        block_stack.push(BlockKind::Paren { adds_indent: true });
                        indent_level += 1;
                    }
                    for _ in 0..brackets_after {
                        block_stack.push(BlockKind::Bracket { adds_indent: true });
                        indent_level += 1;
                    }
                } else {
                    let opens_normal_block = (has_word(&stripped, "if")
                        && !has_word(&stripped, "elseif")
                        && has_word(&stripped, "then")
                        && !has_word(&stripped, "end"))
                        || (has_word(&stripped, "for")
                            && has_word(&stripped, "do")
                            && !has_word(&stripped, "end"))
                        || (has_word(&stripped, "while")
                            && has_word(&stripped, "do")
                            && !has_word(&stripped, "end"))
                        || (starts_with_keyword(trimmed, "do") && !has_word(&stripped, "end"))
                        || (starts_with_keyword(trimmed, "repeat")
                            && !has_word(&stripped, "until"))
                        || (starts_with_keyword(trimmed, "enum") && !has_word(&stripped, "end"));

                    let is_match_block = starts_with_keyword(trimmed, "match")
                        && !trimmed["match".len()..].trim_start().starts_with('=')
                        && !has_word(&stripped, "end");

                    if is_match_block {
                        block_stack.push(BlockKind::Match { has_case: false });
                        indent_level += 1;
                    } else if opens_normal_block {
                        block_stack.push(BlockKind::Normal);
                        indent_level += 1;
                    }

                    if open_braces > close_braces {
                        for _ in 0..(open_braces - close_braces) {
                            block_stack.push(BlockKind::Brace { adds_indent: true });
                            indent_level += 1;
                        }
                    }
                    if open_parens > close_parens {
                        for _ in 0..(open_parens - close_parens) {
                            block_stack.push(BlockKind::Paren { adds_indent: true });
                            indent_level += 1;
                        }
                    }
                    if open_brackets > close_brackets {
                        for _ in 0..(open_brackets - close_brackets) {
                            block_stack.push(BlockKind::Bracket { adds_indent: true });
                            indent_level += 1;
                        }
                    }
                }
            }
        }
    }

    // Safety check: verify formatted output preserves identical tokens and valid syntax
    crate::compiler::parse(&out).map_err(|err| format!("formatting corrupted syntax: {}", err))?;

    let tokens_before: Vec<(&'static str, String)> = crate::lexer::Lexer::new(source)
        .tokenize()
        .map_err(|err| format!("cannot tokenize source: {}", err))?
        .into_iter()
        .map(|t| (t.kind, t.value))
        .collect();

    let tokens_after: Vec<(&'static str, String)> = crate::lexer::Lexer::new(&out)
        .tokenize()
        .map_err(|err| format!("formatting corrupted tokens: {}", err))?
        .into_iter()
        .map(|t| (t.kind, t.value))
        .collect();

    if tokens_before != tokens_after {
        return Err("formatting altered program tokens".to_string());
    }

    Ok(out)
}

fn find_unclosed_multiline(line: &str) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"' || chars[i] == '\'' {
            let quote = chars[i];
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    i += 2;
                } else if chars[i] == quote {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if chars[i] == '-' && i + 1 < chars.len() && chars[i + 1] == '-' {
            let j = i + 2;
            if j < chars.len() && chars[j] == '[' {
                let eq_start = j + 1;
                let mut eq_end = eq_start;
                while eq_end < chars.len() && chars[eq_end] == '=' {
                    eq_end += 1;
                }
                if eq_end < chars.len() && chars[eq_end] == '[' {
                    let eq_count = eq_end - eq_start;
                    let closer = format!("]{}]", "=".repeat(eq_count));
                    let rest: String = chars[eq_end + 1..].iter().collect();
                    if let Some(pos) = rest.find(&closer) {
                        i = eq_end + 1 + pos + closer.len();
                        continue;
                    } else {
                        return Some(closer);
                    }
                }
            }
            return None;
        }
        if chars[i] == '[' && i + 1 < chars.len() {
            let eq_start = i + 1;
            let mut eq_end = eq_start;
            while eq_end < chars.len() && chars[eq_end] == '=' {
                eq_end += 1;
            }
            if eq_end < chars.len() && chars[eq_end] == '[' {
                let eq_count = eq_end - eq_start;
                let closer = format!("]{}]", "=".repeat(eq_count));
                let rest: String = chars[eq_end + 1..].iter().collect();
                if let Some(pos) = rest.find(&closer) {
                    i = eq_end + 1 + pos + closer.len();
                    continue;
                } else {
                    return Some(closer);
                }
            }
        }
        i += 1;
    }
    None
}

fn strip_strings_and_comments(line: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '-' && i + 1 < chars.len() && chars[i + 1] == '-' {
            break;
        }
        if chars[i] == '"' || chars[i] == '\'' {
            let quote = chars[i];
            i += 1;
            while i < chars.len() {
                if chars[i] == '\\' {
                    i += 2;
                } else if chars[i] == quote {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if chars[i] == '[' && i + 1 < chars.len() {
            let mut j = i + 1;
            while j < chars.len() && chars[j] == '=' {
                j += 1;
            }
            if j < chars.len() && chars[j] == '[' {
                let eq_count = j - (i + 1);
                let closer = format!("]{}]", "=".repeat(eq_count));
                let rest: String = chars[j + 1..].iter().collect();
                if let Some(pos) = rest.find(&closer) {
                    i = j + 1 + pos + closer.len();
                    continue;
                } else {
                    break;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn has_word(s: &str, word: &str) -> bool {
    let bytes = s.as_bytes();
    let word_bytes = word.as_bytes();
    let mut pos = 0;
    while pos + word_bytes.len() <= bytes.len() {
        if let Some(idx) = s[pos..].find(word) {
            let start = pos + idx;
            let end = start + word_bytes.len();
            let before_ok = start == 0
                || (!bytes[start - 1].is_ascii_alphanumeric() && bytes[start - 1] != b'_');
            let after_ok =
                end == bytes.len() || (!bytes[end].is_ascii_alphanumeric() && bytes[end] != b'_');
            if before_ok && after_ok {
                return true;
            }
            pos = start + 1;
        } else {
            break;
        }
    }
    false
}

fn normalize_line_spacing(line: &str) -> String {
    // If it's a comment, keep as is
    if line.starts_with("--") {
        return line.to_string();
    }
    line.to_string()
}

/// Formats a file or directory recursively.
pub fn run_cli(args: &[String]) -> Result<(), String> {
    let check = args.iter().any(|a| a == "--check");
    let targets: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();

    if targets.is_empty() {
        return Err("Usage: neyuki fmt [--check] <file|dir>".to_string());
    }

    let mut unformatted_count = 0;
    let mut files = Vec::new();

    for target in targets {
        let p = Path::new(target);
        if p.is_file() {
            files.push(p.to_path_buf());
        } else if p.is_dir() {
            collect_nyk_files(p, &mut files);
        } else {
            return Err(format!("path '{}' does not exist", target));
        }
    }

    for file in files {
        let content = fs::read_to_string(&file)
            .map_err(|e| format!("failed to read {}: {}", file.display(), e))?;
        match format_source(&content) {
            Ok(formatted) => {
                if formatted != content {
                    unformatted_count += 1;
                    if check {
                        eprintln!("Diff in {}", file.display());
                    } else {
                        fs::write(&file, formatted)
                            .map_err(|e| format!("failed to write {}: {}", file.display(), e))?;
                        println!("Formatted {}", file.display());
                    }
                }
            }
            Err(e) => {
                eprintln!("Warning: skipping {}: {}", file.display(), e);
            }
        }
    }

    if check && unformatted_count > 0 {
        Err(format!(
            "{} file(s) need formatting. Run `neyuki fmt <path>` to fix.",
            unformatted_count
        ))
    } else {
        Ok(())
    }
}

fn collect_nyk_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_nyk_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "nyk") {
                out.push(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_source_preserves_valid_code() {
        let input = "local x = 1\nlocal y = 2\nreturn x + y\n";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "local x = 1\nlocal y = 2\nreturn x + y\n");
    }

    #[test]
    fn test_format_source_indents_functions() {
        let input = "function add(a, b)\nreturn a + b\nend\n";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, "function add(a, b)\n    return a + b\nend\n");
    }

    #[test]
    fn test_format_source_indents_local_functions_and_assigned_functions() {
        let input = "local function add(a, b)\nreturn a + b\nend\nlocal f = function(x)\nreturn x * 2\nend\n";
        let formatted = format_source(input).unwrap();
        assert_eq!(
            formatted,
            "local function add(a, b)\n    return a + b\nend\nlocal f = function(x)\n    return x * 2\nend\n"
        );
    }

    #[test]
    fn test_format_source_keeps_names_without_initializers() {
        let input = "local a, b, c\nconst d, e\nprint(a)\n";
        assert_eq!(format_source(input).unwrap(), input);
    }

    #[test]
    fn test_format_source_preserves_multiline_raw_strings() {
        let input = "local s = [[\nfirst line\n    indented line\nlast line\n]]\nlocal x = 1\n";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, input);
    }

    #[test]
    fn test_format_source_ignores_braces_in_strings_and_comments() {
        let input = "local s = \"{foo}\"\n-- comment with {bar}\nlocal t = {\na = 1,\n}\n";
        let formatted = format_source(input).unwrap();
        assert_eq!(
            formatted,
            "local s = \"{foo}\"\n-- comment with {bar}\nlocal t = {\n    a = 1,\n}\n"
        );
    }

    #[test]
    fn test_format_source_match_case_no_staircase() {
        let input = "match c\ncase 1:\nname = \"one\"\ncase 2:\nname = \"two\"\ndefault:\nname = \"other\"\nend\nlocal done = true\n";
        let formatted = format_source(input).unwrap();
        let expected = "match c\n    case 1:\n        name = \"one\"\n    case 2:\n        name = \"two\"\n    default:\n        name = \"other\"\nend\nlocal done = true\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_source_idempotent() {
        let input = "local x = 10\nif x > 5 then\n    print(x)\nend\n";
        let formatted1 = format_source(input).unwrap();
        let formatted2 = format_source(&formatted1).unwrap();
        assert_eq!(formatted1, formatted2);
    }

    #[test]
    fn test_format_source_identifiers_with_keyword_prefixes() {
        let input = "function test()\nendpoint = 1\nending = 2\nelsewhere = 3\nuntil_done = 4\nreturn endpoint\nend\n";
        let formatted = format_source(input).unwrap();
        let expected = "function test()\n    endpoint = 1\n    ending = 2\n    elsewhere = 3\n    until_done = 4\n    return endpoint\nend\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_source_local_match_and_assignment_not_block() {
        let input =
            "function test()\nlocal match = \"regex\"\nmatch = \"updated\"\nreturn match\nend\n";
        let formatted = format_source(input).unwrap();
        let expected = "function test()\n    local match = \"regex\"\n    match = \"updated\"\n    return match\nend\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_source_multiline_comments_midline_and_level_matching() {
        let input = "local x = 1 --[[\n    first comment line\n    second comment line\n]]\nlocal y = 2 --[==[\n    comment with ]] inside\n]==]\nlocal z = 3\n";
        let formatted = format_source(input).unwrap();
        assert_eq!(formatted, input);
    }

    #[test]
    fn test_format_source_multiline_if_header() {
        let input = "if cond1\nand cond2 then\nprint(\"ok\")\nend\n";
        let formatted = format_source(input).unwrap();
        let expected = "if cond1\n    and cond2 then\n    print(\"ok\")\nend\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_source_multiline_for_header() {
        let input = "for k, v in\npairs(t) do\nprint(k, v)\nend\n";
        let formatted = format_source(input).unwrap();
        let expected = "for k, v in\n    pairs(t) do\n    print(k, v)\nend\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_source_multiline_parens() {
        let input = "local res = call(\narg1,\narg2\n)\n";
        let formatted = format_source(input).unwrap();
        let expected = "local res = call(\n    arg1,\n    arg2\n)\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_source_callback_followed_by_statements() {
        let input = "foo(function()\nprint(\"inside\")\nend)\nlocal after = 1\n";
        let formatted = format_source(input).unwrap();
        let expected = "foo(function()\n    print(\"inside\")\nend)\nlocal after = 1\n";
        assert_eq!(formatted, expected);

        let round_trip = format_source(&formatted).unwrap();
        assert_eq!(round_trip, formatted);
    }

    #[test]
    fn test_format_source_callback_in_table() {
        let input = "local t = {\nf = function()\nreturn 1\nend,\n}\nlocal after = 2\n";
        let formatted = format_source(input).unwrap();
        let expected =
            "local t = {\n    f = function()\n        return 1\n    end,\n}\nlocal after = 2\n";
        assert_eq!(formatted, expected);

        let round_trip = format_source(&formatted).unwrap();
        assert_eq!(round_trip, formatted);
    }

    #[test]
    fn test_format_source_pcall_callback() {
        let input = "pcall(function()\nerror(\"err\")\nend)\nlocal after = 3\n";
        let formatted = format_source(input).unwrap();
        let expected = "pcall(function()\n    error(\"err\")\nend)\nlocal after = 3\n";
        assert_eq!(formatted, expected);

        let round_trip = format_source(&formatted).unwrap();
        assert_eq!(round_trip, formatted);
    }

    #[test]
    fn test_format_source_match_inside_function() {
        let input = "function test(x)\nmatch x\ncase 1:\nreturn \"one\"\ncase 2:\nreturn \"two\"\ndefault:\nreturn \"other\"\nend\nend\n";
        let formatted = format_source(input).unwrap();
        let expected = "function test(x)\n    match x\n        case 1:\n            return \"one\"\n        case 2:\n            return \"two\"\n        default:\n            return \"other\"\n    end\nend\n";
        assert_eq!(formatted, expected);

        let round_trip = format_source(&formatted).unwrap();
        assert_eq!(round_trip, formatted);
    }

    #[test]
    fn test_format_source_repeat_until_multiline_condition() {
        let input = "repeat\nwork()\nuntil (\ncond1\nand cond2\n)\n";
        let formatted = format_source(input).unwrap();
        let expected = "repeat\n    work()\nuntil (\n    cond1\n    and cond2\n)\n";
        assert_eq!(formatted, expected);

        let round_trip = format_source(&formatted).unwrap();
        assert_eq!(round_trip, formatted);
    }

    #[test]
    fn test_format_source_else_elseif_in_block() {
        let input = "function test()\nif a then\nb()\nelseif c then\nd()\nelse\ne()\nend\nend\n";
        let formatted = format_source(input).unwrap();
        let expected = "function test()\n    if a then\n        b()\n    elseif c then\n        d()\n    else\n        e()\n    end\nend\n";
        assert_eq!(formatted, expected);

        let round_trip = format_source(&formatted).unwrap();
        assert_eq!(round_trip, formatted);
    }

    #[test]
    fn test_format_source_multiline_elseif_header() {
        let input = "function test()\nif a then\nb()\nelseif c\nand d then\ne()\nend\nend\n";
        let formatted = format_source(input).unwrap();
        let expected = "function test()\n    if a then\n        b()\n    elseif c\n        and d then\n        e()\n    end\nend\n";
        assert_eq!(formatted, expected);

        let round_trip = format_source(&formatted).unwrap();
        assert_eq!(round_trip, formatted);
    }

    #[test]
    fn test_format_source_crlf_preservation() {
        let input = "local x = 1\r\nif x then\r\nprint(x)\r\nend\r\n";
        let formatted = format_source(input).unwrap();
        let expected = "local x = 1\r\nif x then\r\n    print(x)\r\nend\r\n";
        assert_eq!(formatted, expected);
        assert!(formatted.contains("\r\n"));

        let round_trip = format_source(&formatted).unwrap();
        assert_eq!(round_trip, formatted);
    }

    #[test]
    fn test_format_source_idempotency_all_files() {
        let mut files = Vec::new();
        collect_nyk_files(Path::new("tests"), &mut files);
        collect_nyk_files(Path::new("examples"), &mut files);

        assert!(
            !files.is_empty(),
            "Should find .nyk files in tests and examples"
        );

        for file in files {
            let content = match fs::read_to_string(&file) {
                Ok(c) => c,
                Err(_) => continue,
            };

            if let Ok(first_format) = format_source(&content) {
                let second_format = format_source(&first_format).unwrap_or_else(|err| {
                    panic!("Second format failed on {}: {}", file.display(), err)
                });
                assert_eq!(
                    first_format,
                    second_format,
                    "Formatting is not idempotent for file {}",
                    file.display()
                );
            }
        }
    }

    #[test]
    fn test_format_source_type_declarations() {
        let src = "export type Options = { name: string?, frame: ((events: {any}, dt: number) -> ())? }\ntype Point = {\n        x: number,\n  y: number,\n    }\nlocal p: Point = { x = 1, y = 2 }\nif true then\ntype Inner = number\nend\n";
        let formatted = format_source(src).unwrap();
        assert_eq!(
            formatted,
            "export type Options = { name: string?, frame: ((events: {any}, dt: number) -> ())? }\ntype Point = {\n    x: number,\n    y: number,\n}\nlocal p: Point = { x = 1, y = 2 }\nif true then\n    type Inner = number\nend\n"
        );
        assert_eq!(format_source(&formatted).unwrap(), formatted);
    }
}
