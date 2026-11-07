//! `neyuki lint`: reports syntax errors and everything the checker finds
//! (undefined names, const writes, type mismatches, unused and unreachable
//! code), as a readable report or as JSON for editors.

use std::fs;
use std::io::Read;

use crate::dap_json::{Json, render_json};
use crate::diagnostics::diagnostic::Diagnostic;
use crate::diagnostics::emitter::render_report;
use crate::diagnostics::error_code::ErrorCode;
use crate::diagnostics::severity::Severity;
use crate::diagnostics::span::Span;

/// Checks source and returns its diagnostics ordered by position. A syntax
/// error stops the check and is the only diagnostic.
pub fn lint_source(source: &str) -> Vec<Diagnostic> {
    let parsed = match crate::compiler::parse(source) {
        Ok(parsed) => parsed,
        Err(err) => return vec![syntax_error(&err)],
    };
    let mut diagnostics = crate::sema::analyze_with_spans(&parsed.stmts, source, &parsed.spans);
    diagnostics.sort_by_key(|d| {
        d.primary_label
            .as_ref()
            .map(|l| (l.span.start.line, l.span.start.column))
            .unwrap_or((0, 0))
    });
    diagnostics.dedup_by(|a, b| a.code == b.code && a.message == b.message);
    // Appended after the dedup above, which collapses equal messages and
    // would drop a second identical escape on a nearby line.
    diagnostics.extend(escape_diagnostics(source));
    diagnostics.sort_by_key(|d| {
        d.primary_label
            .as_ref()
            .map(|l| (l.span.start.line, l.span.start.column))
            .unwrap_or((0, 0))
    });
    diagnostics
}

/// Warns about `\xHH` and `\ddd` escapes of 128 and above (`W0006`). Neyuki
/// strings are UTF-8 text, so such an escape names the code point of that
/// value and encodes as two bytes, where Lua would store the single byte.
fn escape_diagnostics(source: &str) -> Vec<Diagnostic> {
    let mut tokenizer = crate::parser::tokenizer::TokenizerEngine::new(source);
    if tokenizer.tokenize().is_err() {
        return Vec::new();
    }
    tokenizer
        .take_escape_warnings()
        .into_iter()
        .map(|warning| {
            let line = warning.line.min(u32::MAX as usize) as u32;
            let col = warning.col.min(u32::MAX as usize) as u32;
            let end = col.saturating_add(warning.escape.len() as u32);
            let bytes = char::from_u32(warning.value).map_or(2, char::len_utf8);
            Diagnostic::warning(format!(
                "escape `{}` is a code point, not a byte",
                warning.escape
            ))
            .with_code(ErrorCode::W0006)
            .with_label(
                Span::single_line(line.max(1), col.max(1), end.max(1)),
                format!("stores U+{:04X} as {} bytes", warning.value, bytes),
            )
            .with_help(format!(
                "write `\\u{{{:X}}}` for that character, or build raw bytes with `buffer`",
                warning.value
            ))
        })
        .collect()
}

/// Turns a parser error such as `[line 4] expected symbol ")"` into a
/// diagnostic on that line.
pub fn syntax_error(err: &str) -> Diagnostic {
    let (line, message) = err
        .strip_prefix("[line ")
        .and_then(|rest| rest.split_once("] "))
        .and_then(|(line, message)| Some((line.parse::<u32>().ok()?, message)))
        .unwrap_or((1, err));
    Diagnostic::error(message)
        .with_code(ErrorCode::E0050)
        .with_label(Span::line(line.max(1)), "syntax error")
}

/// Renders diagnostics as a JSON array of objects with `severity`, `code`,
/// `message`, `line`, `column`, `endLine`, `endColumn` (1-based, end
/// exclusive) and `help` when there is one.
pub fn diagnostics_json(diagnostics: &[Diagnostic]) -> String {
    let items = diagnostics
        .iter()
        .map(|d| {
            let span = d.primary_label.as_ref().map(|l| l.span).unwrap_or_default();
            let mut fields = vec![
                (
                    "severity".to_string(),
                    Json::Str(d.severity.as_str().to_string()),
                ),
                (
                    "code".to_string(),
                    d.code
                        .map(|c| Json::Str(c.as_str().to_string()))
                        .unwrap_or(Json::Null),
                ),
                ("message".to_string(), Json::Str(d.message.clone())),
                ("line".to_string(), Json::Num(span.start.line.max(1) as f64)),
                (
                    "column".to_string(),
                    Json::Num(span.start.column.max(1) as f64),
                ),
                (
                    "endLine".to_string(),
                    Json::Num(span.end.line.max(span.start.line).max(1) as f64),
                ),
                (
                    "endColumn".to_string(),
                    Json::Num(span.end.column.max(span.start.column).max(1) as f64),
                ),
            ];
            if let Some(help) = &d.help {
                fields.push(("help".to_string(), Json::Str(help.clone())));
            }
            Json::Obj(fields)
        })
        .collect();
    render_json(&Json::Arr(items))
}

/// `neyuki lint [--json] [--stdin] <path>`. With `--stdin` the source is read
/// from standard input and `path` only names it. Fails when there are errors.
pub fn run_cli(args: &[String]) -> Result<(), String> {
    let json = args.iter().any(|a| a == "--json");
    let stdin = args.iter().any(|a| a == "--stdin");
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or("Usage: neyuki lint [--json] [--stdin] <path>")?;
    let source = if stdin {
        let mut source = String::new();
        std::io::stdin()
            .read_to_string(&mut source)
            .map_err(|err| format!("failed to read stdin: {}", err))?;
        source
    } else {
        fs::read_to_string(path).map_err(|err| format!("failed to read '{}': {}", path, err))?
    };

    let diagnostics = lint_source(&source);
    let has_errors = diagnostics.iter().any(|d| d.severity == Severity::Error);
    if json {
        println!("{}", diagnostics_json(&diagnostics));
    } else if diagnostics.is_empty() {
        println!("No issues found in '{}'.", path);
    } else {
        eprintln!("{}", render_report(path, &source, &diagnostics));
    }
    if has_errors && !json {
        Err(format!("lint found errors in '{}'", path))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_on_byte_escapes_above_ascii() {
        let diags = lint_source(r#"local a = "\x89""#);
        let warning = diags
            .iter()
            .find(|d| d.code == Some(ErrorCode::W0006))
            .expect("W0006 warning");
        assert!(warning.message.contains(r"\x89"));
        let span = warning.primary_label.as_ref().unwrap().span;
        assert_eq!(span.start.line, 1);
        // The backslash sits at column 12 of `local a = "\x89"`.
        assert_eq!(span.start.column, 12);
        assert_eq!(span.end.column, 16);
    }

    #[test]
    fn warns_on_decimal_escapes_above_ascii() {
        let diags = lint_source(r#"local a = "\137""#);
        let warning = diags
            .iter()
            .find(|d| d.code == Some(ErrorCode::W0006))
            .expect("W0006 warning");
        assert!(warning.message.contains(r"\137"));
    }

    #[test]
    fn ascii_and_unicode_escapes_are_quiet() {
        for source in [
            r#"local a = "\x41\65\n\t""#,
            r#"local a = "\u{89}""#,
            r#"local a = [[\x89]]"#,
        ] {
            let diags = lint_source(source);
            assert!(
                !diags.iter().any(|d| d.code == Some(ErrorCode::W0006)),
                "unexpected W0006 for {source}"
            );
        }
    }

    #[test]
    fn repeated_identical_escapes_are_all_reported() {
        let diags = lint_source(
            r#"local a = "\x89"
local b = "\x89""#,
        );
        let count = diags
            .iter()
            .filter(|d| d.code == Some(ErrorCode::W0006))
            .count();
        assert_eq!(count, 2, "both escapes should survive the dedup");
    }

    #[test]
    fn clean_source_has_no_diagnostics() {
        assert!(lint_source("local x = 10\nreturn x + 1").is_empty());
    }

    #[test]
    fn reports_unused_locals_where_they_are() {
        let diags = lint_source("local a = 1\nlocal unused = 42\nreturn a");
        let unused = diags
            .iter()
            .find(|d| d.code == Some(ErrorCode::W0001))
            .expect("unused warning");
        let span = unused.primary_label.as_ref().unwrap().span;
        assert_eq!((span.start.line, span.start.column), (2, 7));
    }

    #[test]
    fn reports_undefined_names_and_type_errors_on_their_lines() {
        let diags = lint_source(
            "local x = 1\nprint(y + x)\nlocal function f(): number\n    return \"s\"\nend\nf()",
        );
        let lines: Vec<(Option<ErrorCode>, u32)> = diags
            .iter()
            .map(|d| (d.code, d.primary_label.as_ref().unwrap().span.start.line))
            .collect();
        assert!(lines.contains(&(Some(ErrorCode::E0001), 2)), "{:?}", lines);
        assert!(lines.contains(&(Some(ErrorCode::E0003), 4)), "{:?}", lines);
    }

    #[test]
    fn syntax_errors_keep_their_line() {
        let diags = lint_source("local x = 1\nprint(x\n");
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code, Some(ErrorCode::E0050));
        assert!(diags[0].primary_label.as_ref().unwrap().span.start.line >= 2);
    }

    fn codes(source: &str) -> Vec<ErrorCode> {
        lint_source(source).iter().filter_map(|d| d.code).collect()
    }

    #[test]
    fn methods_and_fields_declare_no_locals() {
        let source = "local t = {}\n\
            function t.f(self: any): number return 1 end\n\
            function t:g() return 2 end\n\
            return t";
        assert!(codes(source).is_empty(), "{:?}", lint_source(source));
        // The table a method is stored into must still exist.
        assert!(codes("function missing.f() end").contains(&ErrorCode::E0001));
    }

    #[test]
    fn varargs_read_in_an_expression_are_used() {
        assert!(codes("local function f(...: string) return {...} end\nf()").is_empty());
        assert!(codes("local function f(...: any) return 1 end\nf()").contains(&ErrorCode::W0002));
    }

    #[test]
    fn empty_tuple_return_type_allows_bare_return() {
        assert!(codes("local function f(): () return end\nf()").is_empty());
    }

    #[test]
    fn named_types_accept_tables_and_defaults() {
        let source = "local function f(options: Options?): Options\n\
            options = options ?? {}\n\
            return options\n\
            end\nf()";
        assert!(codes(source).is_empty(), "{:?}", lint_source(source));
        // Two different names still don't match.
        let source = "local function f(r: Request): Response return r end\nf({})";
        assert!(codes(source).contains(&ErrorCode::E0003));
        // Nor does a possibly-nil value where nil isn't accepted.
        let source = "local function f(o: Options?): number return o end\nf()";
        assert!(codes(source).contains(&ErrorCode::E0003));
    }

    #[test]
    fn nested_unions_are_flattened_for_assignment() {
        let source = "local function f(x: string | number | nil): string | number\n\
            x = x ?? \"l\"\n\
            return x\n\
            end\nf()";
        assert!(codes(source).is_empty(), "{:?}", lint_source(source));
        let source = "local function f(x: string | number | nil): string | number\n\
            return x\n\
            end\nf()";
        assert!(codes(source).contains(&ErrorCode::E0003));
    }

    #[test]
    fn function_expressions_have_their_own_returns() {
        let source = "local function f(): string\n\
            local cmp = function(a: string, b: string): boolean return a < b end\n\
            cmp(\"a\", \"b\")\n\
            return \"s\"\n\
            end\nf()";
        assert!(codes(source).is_empty(), "{:?}", lint_source(source));
    }

    #[test]
    fn function_expressions_have_their_parameter_types() {
        let ok = "local function f(): (number) -> number\n\
            return function(n: number): number return n end\n\
            end\nf()";
        assert!(codes(ok).is_empty(), "{:?}", lint_source(ok));
        let variadic = "local function f(): (...any) -> number\n\
            return function(...: any): number return select(\"#\", ...) end\n\
            end\nf()";
        assert!(codes(variadic).is_empty(), "{:?}", lint_source(variadic));
        let wrong = "local function f(): (number) -> number\n\
            return function(a: number, b: number): number return a + b end\n\
            end\nf()";
        assert!(codes(wrong).contains(&ErrorCode::E0003));
    }

    #[test]
    fn trailing_optional_arguments_may_be_left_out() {
        let decl = "local function f(a: string, b: string?) return a .. (b ?? \"\") end\n";
        assert!(codes(&format!("{}f(\"x\")", decl)).is_empty());
        assert!(codes(&format!("{}f(\"x\", \"y\")", decl)).is_empty());
        assert!(codes(&format!("{}f()", decl)).contains(&ErrorCode::E0004));
        assert!(codes(&format!("{}f(\"x\", \"y\", \"z\")", decl)).contains(&ErrorCode::E0004));
    }

    #[test]
    fn bundled_libraries_lint_clean() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/lib");
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "nyk") {
                let source = std::fs::read_to_string(&path).unwrap();
                let diags = lint_source(&source);
                assert!(
                    diags.is_empty(),
                    "{}",
                    render_report(&path.display().to_string(), &source, &diags)
                );
                checked += 1;
            }
        }
        assert!(checked > 0);
    }

    #[test]
    fn json_output_is_an_array_of_positions() {
        let json = diagnostics_json(&lint_source("local unused = 1"));
        assert!(
            json.starts_with('[') && json.contains("\"code\":\"W0001\""),
            "{}",
            json
        );
        assert!(json.contains("\"line\":1"), "{}", json);
    }
}
