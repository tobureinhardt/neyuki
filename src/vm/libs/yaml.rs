//! Native YAML 1.2 reading and writing backing `@neyuki/yaml`.
//!
//! Parsing uses `saphyr-parser` (pure-Rust YAML 1.2 event stream) and builds
//! Neyuki VM values directly according to the YAML 1.2 Core Schema: integers
//! stay exact (promoting to `BigInt` when outside `i64`), floats include
//! `.inf` and `.nan`, and `null` becomes `nil` (or `yaml.null` / `json.null`
//! with `keepNull`). Anchors, aliases and merge keys (`<<`) are bounded by
//! strict depth, node-count and expansion caps to prevent Billion Laughs DoS.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use num_bigint::BigInt;
use num_traits::{Num, ToPrimitive};
use saphyr_parser::{Event, Parser, ScalarStyle, Span, Tag};

use crate::vm::libs::args::{array, bool_arg, opt_int_arg, str_arg};
use crate::vm::libs::json;
use crate::vm::machine::VM;
use crate::vm::value::{NativeDef, Value, VmTable};

/// Maximum container nesting depth during decode and encode.
const MAX_DEPTH: usize = 128;

/// Maximum total materialized nodes across a stream (including expanded
/// aliases and merge keys) to stop Billion Laughs memory exhaustion.
const MAX_NODES: usize = 250_000;

/// Maximum number of alias expansions allowed in a single YAML stream.
const MAX_ALIAS_EXPANSIONS: usize = 10_000;

/// Maximum output size for `yaml.encode`, matching `json` and `toml`.
const MAX_OUTPUT: usize = 64 * 1024 * 1024;

pub(crate) const NATIVES: &[NativeDef] = &[
    NativeDef {
        name: "__yaml_decode",
        func: builtin_decode,
    },
    NativeDef {
        name: "__yaml_decode_all",
        func: builtin_decode_all,
    },
    NativeDef {
        name: "__yaml_encode",
        func: builtin_encode,
    },
    NativeDef {
        name: "__yaml_null",
        func: builtin_null,
    },
];

fn builtin_null(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![json::null_sentinel()])
}

fn builtin_decode(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let text = str_arg(args, 0, "text")?;
    let keep_null = bool_arg(args, 1, "keepNull")?;
    Ok(vec![decode_single(Some(vm), text, keep_null)?])
}

fn builtin_decode_all(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let text = str_arg(args, 0, "text")?;
    let keep_null = bool_arg(args, 1, "keepNull")?;
    Ok(vec![decode_all(Some(vm), text, keep_null)?])
}

fn builtin_encode(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let value = args.first().cloned().unwrap_or(Value::NIL);
    let indent = opt_int_arg(args, 1, "indent", 2)?;
    if !(1..=8).contains(&indent) {
        return Err("indent must be an integer between 1 and 8".to_string());
    }
    let sort_keys = match args.get(2) {
        None => true,
        Some(v) if v.is_nil() => true,
        _ => bool_arg(args, 2, "sortKeys")?,
    };
    let mut encoder = Encoder {
        out: String::new(),
        indent: indent as usize,
        sort_keys,
        open: Vec::new(),
    };
    encoder.root(&value)?;
    Ok(vec![Value::string(encoder.out)])
}

/// Parses a single YAML 1.2 document (for fuzzing and internal callers).
pub fn decode_from_str(text: &str) -> Result<Value, String> {
    decode_single(None, text, false)
}

/// Parses a multi-document YAML 1.2 stream into an array value.
pub fn decode_all_from_str(text: &str) -> Result<Value, String> {
    decode_all(None, text, false)
}

// ---------------------------------------------------------------------------
// decode
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum YamlNode {
    Null,
    Bool(bool),
    Int(i64),
    BigInt(BigInt),
    Float(f64),
    String(String),
    Seq(Vec<YamlNode>),
    Map(Vec<(YamlKey, YamlNode)>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum YamlKey {
    Str(String),
    Int(i64),
    BigInt(BigInt),
    Bool(bool),
    /// Non-integer finite float stored by canonical IEEE-754 bits (`-0.0` normalized to `0.0`).
    Float(u64),
}

impl YamlKey {
    fn from_node(node: YamlNode, span: Span) -> Result<Self, String> {
        match node {
            YamlNode::String(s) => Ok(YamlKey::Str(s)),
            YamlNode::Int(i) => Ok(YamlKey::Int(i)),
            YamlNode::BigInt(bi) => Ok(YamlKey::BigInt(bi)),
            YamlNode::Bool(b) => Ok(YamlKey::Bool(b)),
            YamlNode::Float(f) => {
                if f.is_nan() {
                    return Err(at(span, "YAML mapping key cannot be NaN"));
                }
                if f.is_finite() && f.fract() == 0.0 && f.abs() < i64::MAX as f64 {
                    return Ok(YamlKey::Int(f as i64));
                }
                let normalized = if f == 0.0 { 0.0 } else { f };
                Ok(YamlKey::Float(normalized.to_bits()))
            }
            YamlNode::Null => Err(at(span, "YAML mapping key cannot be null")),
            YamlNode::Seq(_) | YamlNode::Map(_) => {
                Err(at(span, "YAML mapping keys must be scalar values"))
            }
        }
    }

    fn into_value(self) -> Value {
        match self {
            YamlKey::Str(s) => Value::string(s),
            YamlKey::Int(i) => Value::int(i),
            YamlKey::BigInt(bi) => Value::from_bigint(bi),
            YamlKey::Bool(b) => Value::bool(b),
            YamlKey::Float(bits) => Value::float(f64::from_bits(bits)),
        }
    }

    fn display(&self) -> String {
        match self {
            YamlKey::Str(s) => s.clone(),
            YamlKey::Int(i) => i.to_string(),
            YamlKey::BigInt(bi) => bi.to_string(),
            YamlKey::Bool(b) => b.to_string(),
            YamlKey::Float(bits) => f64::from_bits(*bits).to_string(),
        }
    }
}

fn at(span: Span, message: impl AsRef<str>) -> String {
    format!(
        "YAML error at line {}, column {}: {}",
        span.start.line(),
        span.start.col() + 1,
        message.as_ref()
    )
}

enum MapKeySlot {
    Regular(YamlKey),
    Merge,
}

enum Frame {
    Seq {
        anchor: usize,
        items: Vec<YamlNode>,
    },
    Map {
        anchor: usize,
        entries: Vec<(YamlKey, YamlNode)>,
        seen: HashSet<YamlKey>,
        merges: Vec<(YamlNode, Span)>,
        pending_key: Option<(MapKeySlot, Span)>,
    },
}

struct NodeMetrics {
    nodes: usize,
    max_depth: usize,
}

/// Computes total node count and relative subtree depth iteratively so even
/// malformed or deep structures cannot overflow the Rust call stack.
fn measure_node(root: &YamlNode) -> NodeMetrics {
    let mut stack = vec![(root, 0usize)];
    let mut nodes = 0usize;
    let mut max_depth = 0usize;
    while let Some((node, depth)) = stack.pop() {
        nodes = nodes.saturating_add(1);
        if depth > max_depth {
            max_depth = depth;
        }
        match node {
            YamlNode::Seq(items) => {
                for item in items {
                    stack.push((item, depth + 1));
                }
            }
            YamlNode::Map(entries) => {
                for (_, val) in entries {
                    stack.push((val, depth + 1));
                }
            }
            _ => {}
        }
    }
    NodeMetrics { nodes, max_depth }
}

fn parse_integer(raw: &str, radix: u32) -> Option<YamlNode> {
    let (neg, digits) = if let Some(rest) = raw.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = raw.strip_prefix('+') {
        (false, rest)
    } else {
        (false, raw)
    };
    if digits.is_empty() {
        return None;
    }
    let valid = match radix {
        8 => digits.bytes().all(|b| matches!(b, b'0'..=b'7')),
        10 => digits.bytes().all(|b| b.is_ascii_digit()),
        16 => digits.bytes().all(|b| b.is_ascii_hexdigit()),
        _ => false,
    };
    if !valid {
        return None;
    }
    let mut bi = BigInt::from_str_radix(digits, radix).ok()?;
    if neg {
        bi = -bi;
    }
    if let Some(i) = bi.to_i64() {
        Some(YamlNode::Int(i))
    } else {
        Some(YamlNode::BigInt(bi))
    }
}

fn parse_unsigned_digits(digits: &str, radix: u32) -> Option<YamlNode> {
    if digits.is_empty() {
        return None;
    }
    let valid = match radix {
        8 => digits.bytes().all(|b| matches!(b, b'0'..=b'7')),
        16 => digits.bytes().all(|b| b.is_ascii_hexdigit()),
        _ => false,
    };
    if !valid {
        return None;
    }
    let bi = BigInt::from_str_radix(digits, radix).ok()?;
    if let Some(i) = bi.to_i64() {
        Some(YamlNode::Int(i))
    } else {
        Some(YamlNode::BigInt(bi))
    }
}

fn parse_core_int(raw: &str) -> Option<YamlNode> {
    if let Some(hex) = raw.strip_prefix("0x") {
        return parse_unsigned_digits(hex, 16);
    }
    if let Some(oct) = raw.strip_prefix("0o") {
        return parse_unsigned_digits(oct, 8);
    }
    parse_integer(raw, 10)
}

fn parse_core_float(raw: &str) -> Option<f64> {
    match raw {
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => return Some(f64::INFINITY),
        "-.inf" | "-.Inf" | "-.INF" => return Some(f64::NEG_INFINITY),
        ".nan" | ".NaN" | ".NAN" => return Some(f64::NAN),
        _ => {}
    }
    // Validate YAML 1.2 Core Schema float syntax:
    // [-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?
    let bytes = raw.as_bytes();
    let mut i = 0;
    if bytes.get(i).is_some_and(|&b| b == b'+' || b == b'-') {
        i += 1;
    }
    let mut saw_digit = false;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        saw_digit = true;
        i += 1;
    }
    let mut saw_dot_or_exp = false;
    if bytes.get(i) == Some(&b'.') {
        saw_dot_or_exp = true;
        i += 1;
        let mut frac_digit = false;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            frac_digit = true;
            i += 1;
        }
        if !saw_digit && !frac_digit {
            return None;
        }
        saw_digit = true;
    }
    if !saw_digit {
        return None;
    }
    if bytes.get(i).is_some_and(|&b| b == b'e' || b == b'E') {
        saw_dot_or_exp = true;
        i += 1;
        if bytes.get(i).is_some_and(|&b| b == b'+' || b == b'-') {
            i += 1;
        }
        let exp_start = i;
        while bytes.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == exp_start {
            return None;
        }
    }
    if i != bytes.len() || !saw_dot_or_exp {
        return None;
    }
    raw.parse::<f64>().ok()
}

fn resolve_scalar(
    text: &str,
    style: ScalarStyle,
    tag: Option<&Tag>,
    span: Span,
) -> Result<YamlNode, String> {
    if let Some(tag) = tag {
        if tag.is_yaml_core_schema() {
            return match tag.suffix.as_str() {
                "str" => Ok(YamlNode::String(text.to_string())),
                "null" => match text {
                    "" | "null" | "Null" | "NULL" | "~" => Ok(YamlNode::Null),
                    _ => Err(at(span, format!("invalid !!null value `{}`", text))),
                },
                "bool" => match text {
                    "true" | "True" | "TRUE" => Ok(YamlNode::Bool(true)),
                    "false" | "False" | "FALSE" => Ok(YamlNode::Bool(false)),
                    _ => Err(at(span, format!("invalid !!bool value `{}`", text))),
                },
                "int" => parse_core_int(text)
                    .ok_or_else(|| at(span, format!("invalid !!int value `{}`", text))),
                "float" => parse_core_float(text)
                    .or_else(|| {
                        parse_core_int(text).and_then(|n| match n {
                            YamlNode::Int(i) => Some(i as f64),
                            YamlNode::BigInt(bi) => bi.to_f64(),
                            _ => None,
                        })
                    })
                    .map(YamlNode::Float)
                    .ok_or_else(|| at(span, format!("invalid !!float value `{}`", text))),
                _ => Ok(YamlNode::String(text.to_string())),
            };
        }
        if tag.handle == "!" && tag.suffix.is_empty() {
            // Non-specific `!` forces a string in YAML 1.2.
            return Ok(YamlNode::String(text.to_string()));
        }
    }

    if style != ScalarStyle::Plain {
        return Ok(YamlNode::String(text.to_string()));
    }

    match text {
        "" | "null" | "Null" | "NULL" | "~" => Ok(YamlNode::Null),
        "true" | "True" | "TRUE" => Ok(YamlNode::Bool(true)),
        "false" | "False" | "FALSE" => Ok(YamlNode::Bool(false)),
        _ => {
            if let Some(int_node) = parse_core_int(text) {
                return Ok(int_node);
            }
            if let Some(float_val) = parse_core_float(text) {
                return Ok(YamlNode::Float(float_val));
            }
            Ok(YamlNode::String(text.to_string()))
        }
    }
}

struct StreamBuilder {
    docs: Vec<YamlNode>,
    stack: Vec<Frame>,
    anchors: HashMap<usize, YamlNode>,
    current_doc: Option<YamlNode>,
    in_doc: bool,
    total_nodes: usize,
    alias_expansions: usize,
}

impl StreamBuilder {
    fn new() -> Self {
        Self {
            docs: Vec::new(),
            stack: Vec::new(),
            anchors: HashMap::new(),
            current_doc: None,
            in_doc: false,
            total_nodes: 0,
            alias_expansions: 0,
        }
    }

    fn count_nodes(&mut self, delta: usize, span: Span) -> Result<(), String> {
        self.total_nodes = self.total_nodes.saturating_add(delta);
        if self.total_nodes > MAX_NODES {
            return Err(at(
                span,
                format!(
                    "YAML document exceeds the {} node limit (possible alias expansion bomb)",
                    MAX_NODES
                ),
            ));
        }
        Ok(())
    }

    fn push_value(
        &mut self,
        node: YamlNode,
        raw_plain_merge: bool,
        span: Span,
    ) -> Result<(), String> {
        let Some(top) = self.stack.last_mut() else {
            self.current_doc = Some(node);
            return Ok(());
        };
        match top {
            Frame::Seq { items, .. } => {
                items.push(node);
                Ok(())
            }
            Frame::Map {
                entries,
                seen,
                merges,
                pending_key,
                ..
            } => {
                if let Some((slot, key_span)) = pending_key.take() {
                    match slot {
                        MapKeySlot::Regular(key) => {
                            if !seen.insert(key.clone()) {
                                return Err(at(
                                    key_span,
                                    format!("duplicate mapping key `{}`", key.display()),
                                ));
                            }
                            entries.push((key, node));
                        }
                        MapKeySlot::Merge => {
                            merges.push((node, span));
                        }
                    }
                    Ok(())
                } else if raw_plain_merge {
                    *pending_key = Some((MapKeySlot::Merge, span));
                    Ok(())
                } else {
                    let key = YamlKey::from_node(node, span)?;
                    *pending_key = Some((MapKeySlot::Regular(key), span));
                    Ok(())
                }
            }
        }
    }

    fn finish_map(
        &mut self,
        entries: Vec<(YamlKey, YamlNode)>,
        mut seen: HashSet<YamlKey>,
        merges: Vec<(YamlNode, Span)>,
    ) -> Result<YamlNode, String> {
        if merges.is_empty() {
            return Ok(YamlNode::Map(entries));
        }
        // Explicit keys always win over merged keys; earlier merged maps win
        // over later ones.
        let mut merged_entries: Vec<(YamlKey, YamlNode)> = Vec::new();
        for (merge_val, span) in merges {
            match merge_val {
                YamlNode::Map(m_entries) => {
                    for (k, v) in m_entries {
                        if seen.insert(k.clone()) {
                            let metrics = measure_node(&v);
                            self.count_nodes(metrics.nodes, span)?;
                            merged_entries.push((k, v));
                        }
                    }
                }
                YamlNode::Seq(seq) => {
                    for item in seq {
                        let YamlNode::Map(m_entries) = item else {
                            return Err(at(
                                span,
                                "YAML merge key `<<` sequence must contain only mappings",
                            ));
                        };
                        for (k, v) in m_entries {
                            if seen.insert(k.clone()) {
                                let metrics = measure_node(&v);
                                self.count_nodes(metrics.nodes, span)?;
                                merged_entries.push((k, v));
                            }
                        }
                    }
                }
                _ => {
                    return Err(at(
                        span,
                        "YAML merge key `<<` requires a mapping or sequence of mappings",
                    ));
                }
            }
        }
        merged_entries.extend(entries);
        Ok(YamlNode::Map(merged_entries))
    }

    fn parse(mut self, text: &str) -> Result<Vec<YamlNode>, String> {
        for item in Parser::new_from_str(text) {
            let (event, span) = item.map_err(|err| {
                format!(
                    "YAML error at line {}, column {}: {}",
                    err.marker().line(),
                    err.marker().col() + 1,
                    err.info()
                )
            })?;
            match event {
                Event::Nothing | Event::StreamStart | Event::StreamEnd => {}
                Event::DocumentStart(_) => {
                    self.anchors.clear();
                    self.stack.clear();
                    self.current_doc = None;
                    self.in_doc = true;
                }
                Event::DocumentEnd => {
                    if self.in_doc {
                        self.docs
                            .push(self.current_doc.take().unwrap_or(YamlNode::Null));
                        self.in_doc = false;
                    }
                    self.anchors.clear();
                }
                Event::Scalar(text, style, anchor, tag) => {
                    let is_merge_key = matches!(
                        self.stack.last(),
                        Some(Frame::Map {
                            pending_key: None,
                            ..
                        })
                    ) && style == ScalarStyle::Plain
                        && tag.is_none()
                        && text == "<<";
                    let node = resolve_scalar(&text, style, tag.as_deref(), span)?;
                    if anchor != 0 {
                        self.anchors.insert(anchor, node.clone());
                    }
                    self.push_value(node, is_merge_key, span)?;
                }
                Event::Alias(anchor) => {
                    self.alias_expansions += 1;
                    if self.alias_expansions > MAX_ALIAS_EXPANSIONS {
                        return Err(at(
                            span,
                            format!(
                                "YAML stream exceeds the {} alias expansion limit",
                                MAX_ALIAS_EXPANSIONS
                            ),
                        ));
                    }
                    let Some(target) = self.anchors.get(&anchor).cloned() else {
                        return Err(at(span, "cyclic or undefined YAML alias"));
                    };
                    let metrics = measure_node(&target);
                    if self.stack.len().saturating_add(metrics.max_depth) > MAX_DEPTH {
                        return Err(at(
                            span,
                            format!("nesting is deeper than {} levels", MAX_DEPTH),
                        ));
                    }
                    self.count_nodes(metrics.nodes, span)?;
                    self.push_value(target, false, span)?;
                }
                Event::SequenceStart(anchor, _tag) => {
                    if self.stack.len() >= MAX_DEPTH {
                        return Err(at(
                            span,
                            format!("nesting is deeper than {} levels", MAX_DEPTH),
                        ));
                    }
                    self.stack.push(Frame::Seq {
                        anchor,
                        items: Vec::new(),
                    });
                }
                Event::SequenceEnd => {
                    let Some(Frame::Seq { anchor, items }) = self.stack.pop() else {
                        return Err(at(span, "unexpected sequence end"));
                    };
                    let node = YamlNode::Seq(items);
                    if anchor != 0 {
                        self.anchors.insert(anchor, node.clone());
                    }
                    self.push_value(node, false, span)?;
                }
                Event::MappingStart(anchor, _tag) => {
                    if self.stack.len() >= MAX_DEPTH {
                        return Err(at(
                            span,
                            format!("nesting is deeper than {} levels", MAX_DEPTH),
                        ));
                    }
                    self.stack.push(Frame::Map {
                        anchor,
                        entries: Vec::new(),
                        seen: HashSet::new(),
                        merges: Vec::new(),
                        pending_key: None,
                    });
                }
                Event::MappingEnd => {
                    let Some(Frame::Map {
                        anchor,
                        entries,
                        seen,
                        merges,
                        pending_key,
                    }) = self.stack.pop()
                    else {
                        return Err(at(span, "unexpected mapping end"));
                    };
                    if pending_key.is_some() {
                        return Err(at(span, "incomplete mapping entry"));
                    }
                    let node = self.finish_map(entries, seen, merges)?;
                    if anchor != 0 {
                        self.anchors.insert(anchor, node.clone());
                    }
                    self.push_value(node, false, span)?;
                }
            }
        }
        Ok(self.docs)
    }
}

fn alloc_yaml_table(vm: Option<&mut VM>, tbl: VmTable) -> Value {
    match vm {
        Some(vm) => vm.alloc_table(tbl),
        None => crate::vm::heap::VmHeap::alloc_table_untracked(tbl),
    }
}

fn alloc_yaml_array(vm: Option<&mut VM>, out: Vec<Value>) -> Value {
    match vm {
        Some(vm) => vm.alloc_array(out),
        None => array(out),
    }
}

fn node_to_value(
    mut vm: Option<&mut VM>,
    node: YamlNode,
    keep_null: bool,
) -> Result<Value, String> {
    match node {
        YamlNode::Null => Ok(if keep_null {
            json::null_sentinel()
        } else {
            Value::NIL
        }),
        YamlNode::Bool(b) => Ok(Value::bool(b)),
        YamlNode::Int(i) => Ok(Value::int(i)),
        YamlNode::BigInt(bi) => Ok(Value::from_bigint(bi)),
        YamlNode::Float(f) => Ok(Value::float(f)),
        YamlNode::String(s) => Ok(Value::string(s)),
        YamlNode::Seq(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(node_to_value(vm.as_deref_mut(), item, keep_null)?);
            }
            Ok(alloc_yaml_array(vm, out))
        }
        YamlNode::Map(entries) => {
            let mut tbl = VmTable::with_capacity(0, entries.len());
            for (k, v) in entries {
                let val = node_to_value(vm.as_deref_mut(), v, keep_null)?;
                tbl.raw_set(k.into_value(), val)?;
            }
            Ok(alloc_yaml_table(vm, tbl))
        }
    }
}

fn decode_single(vm: Option<&mut VM>, text: &str, keep_null: bool) -> Result<Value, String> {
    let docs = StreamBuilder::new().parse(text)?;
    match docs.len() {
        0 => Ok(if keep_null {
            json::null_sentinel()
        } else {
            Value::NIL
        }),
        1 => {
            let doc = docs.into_iter().next().unwrap_or(YamlNode::Null);
            node_to_value(vm, doc, keep_null)
        }
        _ => {
            Err("expected a single YAML document, found multiple (use yaml.decodeAll)".to_string())
        }
    }
}

fn decode_all(mut vm: Option<&mut VM>, text: &str, keep_null: bool) -> Result<Value, String> {
    let docs = StreamBuilder::new().parse(text)?;
    let mut out = Vec::with_capacity(docs.len());
    for doc in docs {
        out.push(node_to_value(vm.as_deref_mut(), doc, keep_null)?);
    }
    Ok(alloc_yaml_array(vm, out))
}

// ---------------------------------------------------------------------------
// encode
// ---------------------------------------------------------------------------

struct Encoder {
    out: String,
    indent: usize,
    sort_keys: bool,
    open: Vec<*const RefCell<VmTable>>,
}

/// Whether `s` can be written as an unquoted plain YAML scalar without being
/// misread by YAML 1.2 (or YAML 1.1) parsers.
fn is_safe_plain_string(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let first = s.chars().next().unwrap_or('\0');
    let last = s.chars().next_back().unwrap_or('\0');
    if first.is_whitespace() || last.is_whitespace() {
        return false;
    }
    if matches!(
        first,
        '-' | '?'
            | ':'
            | ','
            | '['
            | ']'
            | '{'
            | '}'
            | '#'
            | '&'
            | '*'
            | '!'
            | '|'
            | '>'
            | '\''
            | '"'
            | '%'
            | '@'
            | '`'
    ) {
        return false;
    }
    if s.ends_with(':') {
        return false;
    }
    if matches!(s, "<<" | "..." | "---") {
        return false;
    }
    let lower = s.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "null"
            | "~"
            | "true"
            | "false"
            | "yes"
            | "no"
            | "on"
            | "off"
            | "y"
            | "n"
            | ".inf"
            | "+.inf"
            | "-.inf"
            | ".nan"
    ) {
        return false;
    }
    if parse_core_int(s).is_some() || parse_core_float(s).is_some() {
        return false;
    }
    let bytes = s.as_bytes();
    for (i, c) in s.char_indices() {
        if c < '\x20' || c == '\x7f' || matches!(c, '\u{0085}' | '\u{2028}' | '\u{2029}') {
            return false;
        }
        if matches!(c, '[' | ']' | '{' | '}' | ',') {
            return false;
        }
        if c == ':' && bytes.get(i + 1).is_some_and(|&b| b == b' ' || b == b'\t') {
            return false;
        }
        if c == '#' && i > 0 && matches!(bytes.get(i - 1), Some(b' ' | b'\t')) {
            return false;
        }
    }
    true
}

fn can_use_block_literal(s: &str) -> bool {
    s.contains('\n')
        && !s.ends_with("\n\n")
        && s.chars().any(|c| c != '\n')
        && s.chars().all(|c| {
            c == '\n'
                || (c >= '\x20'
                    && c != '\x7f'
                    && !matches!(c, '\u{0085}' | '\u{2028}' | '\u{2029}'))
        })
}

fn write_double_quoted(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\u{0085}' => out.push_str("\\N"),
            '\u{00a0}' => out.push_str("\\_"),
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn classify_table(t: &RefCell<VmTable>) -> (bool, Vec<Value>, Vec<(Value, Value)>) {
    let tbl = t.borrow();
    let marked = tbl
        .metatable
        .as_ref()
        .is_some_and(|m| m.borrow().get_str(json::ARRAY_MARK).is_truthy());
    let has_keys = !tbl.fields.is_empty() || tbl.general.is_some();
    if (marked || !tbl.array.is_empty()) && !has_keys {
        (true, tbl.array.clone(), Vec::new())
    } else {
        let entries = crate::vm::libs::table::entries(&tbl);
        (false, Vec::new(), entries)
    }
}

impl Encoder {
    fn check_limit(&self) -> Result<(), String> {
        if self.out.len() > MAX_OUTPUT {
            return Err(format!(
                "output exceeds the {} MiB limit",
                MAX_OUTPUT / (1024 * 1024)
            ));
        }
        Ok(())
    }

    fn push_indent(&mut self, level: usize) {
        for _ in 0..(level * self.indent) {
            self.out.push(' ');
        }
    }

    fn write_scalar(&mut self, value: &Value) -> Result<(), String> {
        match value.view() {
            crate::vm::value::ValueView::Nil => self.out.push_str("null"),
            crate::vm::value::ValueView::Table(_) if json::is_null(value) => {
                self.out.push_str("null")
            }
            crate::vm::value::ValueView::Bool(b) => {
                self.out.push_str(if b { "true" } else { "false" })
            }
            crate::vm::value::ValueView::Int(i) => self.out.push_str(&i.to_string()),
            crate::vm::value::ValueView::BigInt(bi) => self.out.push_str(&bi.to_string()),
            crate::vm::value::ValueView::Float(f) => match json::format_float(f) {
                Some(text) => self.out.push_str(&text),
                None if f.is_nan() => self.out.push_str(".nan"),
                None if f.is_sign_positive() => self.out.push_str(".inf"),
                None => self.out.push_str("-.inf"),
            },
            crate::vm::value::ValueView::String(s) => {
                if is_safe_plain_string(&s) {
                    self.out.push_str(&s);
                } else {
                    write_double_quoted(&mut self.out, &s);
                }
            }
            _ => {
                return Err(format!("cannot encode a {} as YAML", value.type_name()));
            }
        }
        self.check_limit()
    }

    fn write_block_literal(&mut self, s: &str, content_level: usize) -> Result<(), String> {
        let trailing_newlines = s.bytes().rev().take_while(|&b| b == b'\n').count();
        let chomp = match trailing_newlines {
            0 => "-",
            1 => "",
            _ => "+",
        };
        let body = if trailing_newlines == 1 {
            &s[..s.len() - 1]
        } else {
            s
        };
        let first_non_empty_starts_with_space = body
            .split('\n')
            .find(|line| !line.is_empty())
            .is_some_and(|line| line.starts_with(' '));
        self.out.push('|');
        if first_non_empty_starts_with_space {
            self.out.push_str(&self.indent.to_string());
        }
        self.out.push_str(chomp);
        for line in body.split('\n') {
            self.out.push('\n');
            if !line.is_empty() {
                self.push_indent(content_level);
                self.out.push_str(line);
            }
        }
        self.check_limit()
    }

    fn write_key(&mut self, key: &Value) -> Result<(), String> {
        match key.view() {
            crate::vm::value::ValueView::String(s) => {
                if is_safe_plain_string(&s) {
                    self.out.push_str(&s);
                } else {
                    write_double_quoted(&mut self.out, &s);
                }
            }
            crate::vm::value::ValueView::Int(_)
            | crate::vm::value::ValueView::BigInt(_)
            | crate::vm::value::ValueView::Bool(_) => self.write_scalar(key)?,
            crate::vm::value::ValueView::Float(f) if !f.is_nan() => self.write_scalar(key)?,
            _ => {
                return Err(format!(
                    "cannot encode a table key of type {} as YAML",
                    key.type_name()
                ));
            }
        }
        Ok(())
    }

    fn sort_entries(&self, entries: &mut [(Value, Value)]) {
        if self.sort_keys {
            entries.sort_by_key(|(a, _)| key_sort_repr(a));
        }
    }

    fn root(&mut self, value: &Value) -> Result<(), String> {
        match value.view() {
            crate::vm::value::ValueView::Table(t) if !json::is_null(value) => {
                let (is_array, items, mut entries) = classify_table(&t);
                if is_array && items.is_empty() {
                    self.out.push_str("[]");
                    return Ok(());
                }
                if !is_array && entries.is_empty() {
                    self.out.push_str("{}");
                    return Ok(());
                }
                let ptr = t.as_ptr();
                self.open.push(ptr);
                let result = if is_array {
                    self.write_seq(&items, 0, false)
                } else {
                    self.sort_entries(&mut entries);
                    self.write_map(&entries, 0, false)
                };
                self.open.pop();
                result?;
                self.out.push('\n');
                self.check_limit()
            }
            crate::vm::value::ValueView::String(s) if can_use_block_literal(&s) => {
                self.write_block_literal(&s, 1)?;
                self.out.push('\n');
                self.check_limit()
            }
            _ => {
                self.write_scalar(value)?;
                Ok(())
            }
        }
    }

    fn write_value_inline_or_block(
        &mut self,
        value: &Value,
        child_level: usize,
    ) -> Result<(), String> {
        if child_level >= MAX_DEPTH {
            return Err(format!("nesting is deeper than {} levels", MAX_DEPTH));
        }
        self.check_limit()?;
        match value.view() {
            crate::vm::value::ValueView::String(s) if can_use_block_literal(&s) => {
                self.out.push(' ');
                self.write_block_literal(&s, child_level)
            }
            crate::vm::value::ValueView::Table(t) if !json::is_null(value) => {
                let ptr = t.as_ptr();
                if self.open.contains(&ptr) {
                    return Err("cannot encode a table that contains itself".to_string());
                }
                let (is_array, items, mut entries) = classify_table(&t);
                if is_array && items.is_empty() {
                    self.out.push_str(" []");
                    return Ok(());
                }
                if !is_array && entries.is_empty() {
                    self.out.push_str(" {}");
                    return Ok(());
                }
                self.open.push(ptr);
                self.out.push('\n');
                let result = if is_array {
                    self.write_seq(&items, child_level, false)
                } else {
                    self.sort_entries(&mut entries);
                    self.write_map(&entries, child_level, false)
                };
                self.open.pop();
                result
            }
            _ => {
                self.out.push(' ');
                self.write_scalar(value)
            }
        }
    }

    fn write_map(
        &mut self,
        entries: &[(Value, Value)],
        level: usize,
        skip_first_indent: bool,
    ) -> Result<(), String> {
        for (i, (key, val)) in entries.iter().enumerate() {
            if i > 0 {
                self.out.push('\n');
            }
            if i > 0 || !skip_first_indent {
                self.push_indent(level);
            }
            self.write_key(key)?;
            self.out.push(':');
            self.write_value_inline_or_block(val, level + 1)?;
        }
        self.check_limit()
    }

    fn write_seq(
        &mut self,
        items: &[Value],
        level: usize,
        skip_first_indent: bool,
    ) -> Result<(), String> {
        if level >= MAX_DEPTH {
            return Err(format!("nesting is deeper than {} levels", MAX_DEPTH));
        }
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                self.out.push('\n');
            }
            if i > 0 || !skip_first_indent {
                self.push_indent(level);
            }
            self.out.push('-');
            match item.view() {
                crate::vm::value::ValueView::String(s) if can_use_block_literal(&s) => {
                    self.out.push(' ');
                    self.write_block_literal(&s, level + 1)?;
                }
                crate::vm::value::ValueView::Table(t) if !json::is_null(item) => {
                    let ptr = t.as_ptr();
                    if self.open.contains(&ptr) {
                        return Err("cannot encode a table that contains itself".to_string());
                    }
                    let (is_array, sub_items, mut sub_entries) = classify_table(&t);
                    if is_array && sub_items.is_empty() {
                        self.out.push_str(" []");
                    } else if !is_array && sub_entries.is_empty() {
                        self.out.push_str(" {}");
                    } else {
                        self.open.push(ptr);
                        let res = if is_array {
                            self.out.push('\n');
                            self.write_seq(&sub_items, level + 1, false)
                        } else {
                            self.sort_entries(&mut sub_entries);
                            self.out.push('\n');
                            self.write_map(&sub_entries, level + 1, false)
                        };
                        self.open.pop();
                        res?;
                    }
                }
                _ => {
                    self.out.push(' ');
                    self.write_scalar(item)?;
                }
            }
        }
        self.check_limit()
    }
}

fn key_sort_repr(v: &Value) -> (u8, String) {
    match v.view() {
        crate::vm::value::ValueView::String(s) => (0, s.to_string()),
        crate::vm::value::ValueView::Int(i) => (1, format!("{:020}", i)),
        crate::vm::value::ValueView::BigInt(bi) => (1, bi.to_string()),
        crate::vm::value::ValueView::Float(f) => (2, f.to_string()),
        crate::vm::value::ValueView::Bool(b) => (3, b.to_string()),
        _ => (4, v.type_name().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_core_schema_scalars_and_bigint() {
        let doc = r#"
null_val: ~
empty_val:
bool_true: true
bool_false: FALSE
norway: NO
yes_str: yes
int_dec: 42
int_hex: 0xFF
int_oct: 0o755
big: 123456789012345678901234567890
float_val: 3.14
pos_inf: .inf
neg_inf: -.inf
nan_val: .nan
forced_str: !!str 123
"#;
        let val = decode_single(None, doc, true).unwrap();
        let Some(t) = val.as_table() else {
            panic!("expected table");
        };
        let tbl = t.borrow();
        assert!(json::is_null(&tbl.get_str("null_val")));
        assert!(json::is_null(&tbl.get_str("empty_val")));
        assert_eq!(tbl.get_str("bool_true"), Value::TRUE);
        assert_eq!(tbl.get_str("bool_false"), Value::FALSE);
        assert_eq!(tbl.get_str("norway"), Value::str("NO"));
        assert_eq!(tbl.get_str("yes_str"), Value::str("yes"));
        assert_eq!(tbl.get_str("int_dec"), Value::int(42));
        assert_eq!(tbl.get_str("int_hex"), Value::int(255));
        assert_eq!(tbl.get_str("int_oct"), Value::int(493));
        assert!(parse_core_int("0x-1A").is_none());
        assert!(parse_core_int("0o+7").is_none());
        assert!(!is_safe_plain_string("<<"));
        assert!(!is_safe_plain_string("..."));
        assert!(!is_safe_plain_string("---"));
        assert_eq!(
            tbl.get_str("big"),
            Value::from_bigint("123456789012345678901234567890".parse().unwrap())
        );
        assert_eq!(tbl.get_str("forced_str"), Value::str("123"));
        assert!(matches!(
            tbl.get_str("pos_inf").as_float(),
            Some(f) if f.is_infinite() && f > 0.0
        ));
        assert!(matches!(
            tbl.get_str("neg_inf").as_float(),
            Some(f) if f.is_infinite() && f < 0.0
        ));
        assert!(matches!(
            tbl.get_str("nan_val").as_float(),
            Some(f) if f.is_nan()
        ));
    }

    #[test]
    fn test_billion_laughs_alias_bomb_rejected() {
        let bomb = r#"
a: &a ["lol", "lol", "lol", "lol", "lol", "lol", "lol", "lol", "lol"]
b: &b [*a, *a, *a, *a, *a, *a, *a, *a, *a]
c: &c [*b, *b, *b, *b, *b, *b, *b, *b, *b]
d: &d [*c, *c, *c, *c, *c, *c, *c, *c, *c]
e: &e [*d, *d, *d, *d, *d, *d, *d, *d, *d]
f: &f [*e, *e, *e, *e, *e, *e, *e, *e, *e]
"#;
        let err = decode_single(None, bomb, false).unwrap_err();
        assert!(
            err.contains("limit"),
            "expected node/alias limit error, got: {}",
            err
        );
    }

    #[test]
    fn test_deep_nesting_rejected() {
        let mut yaml = String::new();
        for _ in 0..140 {
            yaml.push('[');
        }
        yaml.push('1');
        for _ in 0..140 {
            yaml.push(']');
        }
        let err = decode_single(None, &yaml, false).unwrap_err();
        assert!(err.contains("nesting"), "unexpected error: {}", err);
    }

    #[test]
    fn test_merge_keys_and_duplicate_rejection() {
        let doc = r#"
defaults: &def
  host: localhost
  port: 5432
service:
  <<: *def
  port: 6432
"#;
        let val = decode_single(None, doc, false).unwrap();
        let Some(root) = val.as_table() else {
            panic!("expected table");
        };
        let svc_val = root.borrow().get_str("service");
        let Some(svc) = svc_val.as_table() else {
            panic!("expected service table");
        };
        assert_eq!(svc.borrow().get_str("port"), Value::int(6432));
        assert_eq!(svc.borrow().get_str("host"), Value::str("localhost"));

        let dup = "a: 1\na: 2\n";
        assert!(
            decode_single(None, dup, false)
                .unwrap_err()
                .contains("duplicate")
        );
    }

    #[test]
    fn test_multi_doc_handling() {
        let stream = "---\na: 1\n---\nb: 2\n";
        assert!(decode_single(None, stream, false).is_err());
        let all = decode_all(None, stream, false).unwrap();
        let Some(arr) = all.as_table() else {
            panic!("expected array");
        };
        assert_eq!(arr.borrow().array.len(), 2);
    }
}
