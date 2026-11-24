//! Native CSV reading and writing backing `@neyuki/csv`.
//!
//! The format is RFC 4180 with the usual leniencies: LF, CRLF or CR line
//! endings, any single ASCII delimiter, a leading byte order mark, and
//! quotes inside an unquoted field kept as they are. Blank lines are
//! ignored. Fields are always strings when read; `lib/csv.nyk` checks the
//! options.

use std::collections::{BTreeSet, HashSet};

use crate::vm::libs::args::{bool_arg, str_arg, table_arg};
use crate::vm::machine::VM;
use crate::vm::value::{NativeDef, StrRef, Value, ValueView, VmTable};

/// The longest text `encode` produces, as for `json.encode`.
const MAX_OUTPUT: usize = 64 * 1024 * 1024;

pub(crate) const NATIVES: &[NativeDef] = &[
    NativeDef {
        name: "__csv_decode",
        func: builtin_decode,
    },
    NativeDef {
        name: "__csv_encode",
        func: builtin_encode,
    },
];

fn delimiter_arg(args: &[Value], index: usize) -> Result<u8, String> {
    let delimiter = str_arg(args, index, "delimiter")?;
    match delimiter.as_bytes() {
        [b] if b.is_ascii() && !matches!(*b, b'"' | b'\r' | b'\n') => Ok(*b),
        _ => Err(
            "delimiter must be a single ASCII character other than a quote or a newline"
                .to_string(),
        ),
    }
}

/// A record and the line it starts on.
struct Record {
    line: usize,
    fields: Vec<String>,
}

/// Splits `text` into records.
fn parse(text: &str, delimiter: u8) -> Result<Vec<Record>, String> {
    let bytes = text.as_bytes();
    let slice = |start: usize, end: usize| text.get(start..end).unwrap_or_default();
    let mut pos = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let mut records = Vec::new();
    let mut fields = Vec::new();
    let mut line = 1;
    let mut record_line = 1;
    // Whether the record so far is one unquoted empty field: a blank line.
    let mut blank = true;
    loop {
        if bytes.get(pos) == Some(&b'"') {
            let start_line = line;
            pos += 1;
            let mut field = String::new();
            loop {
                let start = pos;
                while pos < bytes.len() && bytes[pos] != b'"' {
                    if bytes[pos] == b'\n' {
                        line += 1;
                    }
                    pos += 1;
                }
                field.push_str(slice(start, pos));
                if pos >= bytes.len() {
                    return Err(format!(
                        "invalid CSV: the quoted field starting on line {} is never closed",
                        start_line
                    ));
                }
                if bytes.get(pos + 1) == Some(&b'"') {
                    field.push('"');
                    pos += 2;
                } else {
                    pos += 1;
                    break;
                }
            }
            match bytes.get(pos) {
                None | Some(b'\n' | b'\r') => {}
                Some(b) if *b == delimiter => {}
                Some(_) => {
                    return Err(format!(
                        "invalid CSV at line {}: expected a delimiter or a line break after a closing quote",
                        line
                    ));
                }
            }
            fields.push(field);
            blank = false;
        } else {
            let start = pos;
            while pos < bytes.len()
                && !matches!(bytes[pos], b'\n' | b'\r')
                && bytes[pos] != delimiter
            {
                pos += 1;
            }
            if pos > start || !fields.is_empty() {
                blank = false;
            }
            fields.push(slice(start, pos).to_string());
        }
        match bytes.get(pos) {
            Some(b) if *b == delimiter => {
                pos += 1;
                blank = false;
            }
            None => {
                if !blank {
                    records.push(Record {
                        line: record_line,
                        fields: std::mem::take(&mut fields),
                    });
                }
                return Ok(records);
            }
            Some(_) => {
                // A line break: CRLF, LF or a lone CR.
                if bytes[pos] == b'\r' && bytes.get(pos + 1) == Some(&b'\n') {
                    pos += 1;
                }
                pos += 1;
                line += 1;
                if blank {
                    fields.clear();
                } else {
                    records.push(Record {
                        line: record_line,
                        fields: std::mem::take(&mut fields),
                    });
                }
                blank = true;
                record_line = line;
                if pos >= bytes.len() {
                    return Ok(records);
                }
            }
        }
    }
}

/// `__csv_decode(text, delimiter, header)`: an array of records, each an
/// array of strings, or with `header` an array of tables keyed by the
/// names on the first line.
fn builtin_decode(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let text = str_arg(args, 0, "text")?;
    let delimiter = delimiter_arg(args, 1)?;
    let header = bool_arg(args, 2, "header")?;
    let records = parse(text, delimiter)?;
    let mut records = records.into_iter();
    if !header {
        let rows = records
            .map(|record| vm.alloc_array(record.fields.into_iter().map(Value::string).collect()))
            .collect();
        return Ok(vec![vm.alloc_array(rows)]);
    }
    let Some(names) = records.next() else {
        return Ok(vec![vm.alloc_array(Vec::new())]);
    };
    let mut seen = HashSet::new();
    for name in &names.fields {
        if !seen.insert(name.as_str()) {
            return Err(format!(
                "invalid CSV at line {}: the column `{}` appears twice in the header",
                names.line, name
            ));
        }
    }
    let mut rows = Vec::new();
    for record in records {
        if record.fields.len() > names.fields.len() {
            return Err(format!(
                "invalid CSV at line {}: {} fields, but the header names {} columns",
                record.line,
                record.fields.len(),
                names.fields.len()
            ));
        }
        let mut row = VmTable::new();
        for (name, field) in names.fields.iter().zip(record.fields) {
            row.fields
                .insert(StrRef::from(name.as_str()), Value::string(field));
        }
        rows.push(vm.alloc_table(row));
    }
    Ok(vec![vm.alloc_array(rows)])
}

/// Whether a row is keyed by column names rather than an array.
fn is_keyed(row: &VmTable) -> bool {
    !row.fields.is_empty() || row.general.is_some()
}

fn write_record(out: &mut String, cells: &[String], delimiter: u8, newline: &str) {
    let delimiter = delimiter as char;
    for (i, cell) in cells.iter().enumerate() {
        if i > 0 {
            out.push(delimiter);
        }
        // A record of one empty field would be a blank line, which reads
        // back as no record at all.
        let quote =
            cell.contains(['"', '\r', '\n', delimiter]) || (cells.len() == 1 && cell.is_empty());
        if quote {
            out.push('"');
            out.push_str(&cell.replace('"', "\"\""));
            out.push('"');
        } else {
            out.push_str(cell);
        }
    }
    out.push_str(newline);
}

fn cell_text(vm: &mut VM, value: &Value, row: usize) -> Result<String, String> {
    match value.view() {
        ValueView::Nil => Ok(String::new()),
        ValueView::String(s) => Ok(s.to_string()),
        ValueView::Bool(_) | ValueView::Int(_) | ValueView::BigInt(_) | ValueView::Float(_) => {
            crate::vm::builtins::global::tostring(vm, value)
        }
        _ => Err(format!(
            "row {}: cannot write a {} as a CSV field",
            row,
            value.type_name()
        )),
    }
}

/// `__csv_encode(rows, delimiter, columns, header, newline)`. Rows are
/// arrays of fields or tables keyed by column name. `columns` orders the
/// keyed rows' fields; without it the keys of every keyed row are used,
/// sorted. A header line naming the columns is written unless `header` is
/// false.
fn builtin_encode(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let rows = table_arg(args, 0, "rows")?.borrow().array.clone();
    let delimiter = delimiter_arg(args, 1)?;
    let columns = match args.get(2) {
        None => None,
        Some(v) if v.is_nil() => None,
        Some(v) if v.is_table() => {
            let t = v.as_table().unwrap();
            let mut names = Vec::new();
            for value in t.borrow().array.iter() {
                match value.as_str() {
                    Some(s) => names.push(s.to_string()),
                    None => return Err("columns must be an array of strings".to_string()),
                }
            }
            Some(names)
        }
        Some(other) => {
            return Err(format!(
                "columns must be an array of strings, got {}",
                other.type_name()
            ));
        }
    };
    let header = args.get(3).and_then(|v| v.as_bool()) != Some(false);
    let newline = match args.get(4) {
        None => "\n".to_string(),
        Some(v) if v.is_nil() => "\n".to_string(),
        Some(v) if matches!(v.as_str(), Some("\n" | "\r\n")) => v.as_str().unwrap().to_string(),
        _ => return Err("newline must be \"\\n\" or \"\\r\\n\"".to_string()),
    };

    let mut keyed = false;
    for (i, row) in rows.iter().enumerate() {
        match row.as_table() {
            Some(t) => keyed |= is_keyed(&t.borrow()),
            None => {
                return Err(format!(
                    "row {} must be a table, got {}",
                    i + 1,
                    row.type_name()
                ));
            }
        }
    }
    let columns = match columns {
        Some(columns) => Some(columns),
        None if keyed => {
            let mut names = BTreeSet::new();
            for row in &rows {
                if let Some(t) = row.as_table() {
                    for (key, _) in t.borrow().fields.iter() {
                        names.insert(key.to_string());
                    }
                }
            }
            Some(names.into_iter().collect())
        }
        None => None,
    };

    let mut out = String::new();
    if header && let Some(columns) = &columns {
        write_record(&mut out, columns, delimiter, &newline);
    }
    for (i, row) in rows.iter().enumerate() {
        let Some(t) = row.as_table() else {
            continue;
        };
        let values: Vec<Value> = {
            let t = t.borrow();
            match &columns {
                Some(columns) if is_keyed(&t) => {
                    columns.iter().map(|name| t.get_str(name)).collect()
                }
                _ => t.array.clone(),
            }
        };
        let mut cells = Vec::with_capacity(values.len());
        for value in &values {
            cells.push(cell_text(vm, value, i + 1)?);
        }
        write_record(&mut out, &cells, delimiter, &newline);
        if out.len() > MAX_OUTPUT {
            return Err(format!(
                "output exceeds the {} MiB limit",
                MAX_OUTPUT / (1024 * 1024)
            ));
        }
    }
    Ok(vec![Value::string(out)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(text: &str) -> Vec<Vec<String>> {
        parse(text, b',')
            .unwrap()
            .into_iter()
            .map(|record| record.fields)
            .collect()
    }

    #[test]
    fn quoted_fields_and_line_endings() {
        assert_eq!(
            fields("a,\"b,\"\"c\"\"\"\r\n\"x\ny\",\n\nlast"),
            vec![
                vec!["a".to_string(), "b,\"c\"".to_string()],
                vec!["x\ny".to_string(), String::new()],
                vec!["last".to_string()],
            ]
        );
        assert!(fields("").is_empty());
        assert_eq!(
            fields("\"\"\n").len(),
            1,
            "a quoted empty field is a record"
        );
    }

    #[test]
    fn records_know_their_line() {
        let records = parse("a\n\"b\nc\"\nd\n", b',').unwrap();
        let lines: Vec<usize> = records.iter().map(|record| record.line).collect();
        assert_eq!(lines, vec![1, 2, 4]);
    }

    #[test]
    fn rejects_malformed_quotes() {
        assert!(parse("\"open", b',').is_err());
        assert!(parse("\"a\"b,c", b',').is_err());
    }
}
