//! Native CommonMark + GFM parsing, AST construction and safe HTML rendering
//! backing `@neyuki/markdown`.
//!
//! Parsing goes through `pulldown-cmark` (pure Rust, linear-time CommonMark
//! 0.31 + GitHub Flavored Markdown). `parse` produces plain Neyuki tables
//! mirroring `@neyuki/html`, and `toHtml` renders safe HTML by default
//! (escaping raw HTML and neutralizing `javascript:`/`vbscript:`/`data:` URLs
//! unless `unsafeHtml = true`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, CowStr, Event, HeadingLevel, Options, Parser, Tag,
    TagEnd, html as cmark_html,
};

use crate::vm::libs::args::{array, bool_arg, str_arg};
use crate::vm::machine::VM;
use crate::vm::value::{NativeDef, Value, VmTable};

/// Maximum nesting depth for AST construction and AST traversal.
const MAX_DEPTH: usize = 256;

/// Maximum AST nodes materialized for a single document.
const MAX_NODES: usize = 250_000;

/// Maximum HTML or plain-text output length, matching `json`, `toml` and `html`.
const MAX_OUTPUT: usize = 64 * 1024 * 1024;

pub(crate) const NATIVES: &[NativeDef] = &[
    NativeDef {
        name: "__markdown_to_html",
        func: builtin_to_html,
    },
    NativeDef {
        name: "__markdown_parse",
        func: builtin_parse,
    },
    NativeDef {
        name: "__markdown_frontmatter",
        func: builtin_frontmatter,
    },
    NativeDef {
        name: "__markdown_text",
        func: builtin_text,
    },
];

#[derive(Clone, Copy)]
struct MdOptions {
    gfm: bool,
    breaks: bool,
    smart: bool,
    unsafe_html: bool,
    heading_ids: bool,
    frontmatter: bool,
}

impl Default for MdOptions {
    fn default() -> Self {
        Self {
            gfm: true,
            breaks: false,
            smart: false,
            unsafe_html: false,
            heading_ids: false,
            frontmatter: true,
        }
    }
}

impl MdOptions {
    fn from_args(args: &[Value], start: usize) -> Result<Self, String> {
        let gfm = match args.get(start) {
            None | Some(Value::Nil) => true,
            _ => bool_arg(args, start, "gfm")?,
        };
        let breaks = bool_arg(args, start + 1, "breaks")?;
        let smart = bool_arg(args, start + 2, "smart")?;
        let unsafe_html = bool_arg(args, start + 3, "unsafeHtml")?;
        let heading_ids = bool_arg(args, start + 4, "headingIds")?;
        let frontmatter = match args.get(start + 5) {
            None | Some(Value::Nil) => true,
            _ => bool_arg(args, start + 5, "frontmatter")?,
        };
        Ok(Self {
            gfm,
            breaks,
            smart,
            unsafe_html,
            heading_ids,
            frontmatter,
        })
    }

    fn cmark_options(self) -> Options {
        let mut opts = Options::empty();
        if self.gfm {
            opts |= Options::ENABLE_TABLES
                | Options::ENABLE_STRIKETHROUGH
                | Options::ENABLE_TASKLISTS
                | Options::ENABLE_FOOTNOTES
                | Options::ENABLE_GFM;
        }
        if self.smart {
            opts |= Options::ENABLE_SMART_PUNCTUATION;
        }
        if self.heading_ids {
            opts |= Options::ENABLE_HEADING_ATTRIBUTES;
        }
        opts
    }
}

fn new_table(tbl: VmTable) -> Value {
    Value::Table(Rc::new(RefCell::new(tbl)))
}

/// Safe HTML rendering with default options (used by fuzz tests).
pub fn to_html_from_str(text: &str) -> Result<String, String> {
    render_html(text, MdOptions::default())
}

/// AST parsing with default options (used by fuzz tests).
pub fn parse_from_str(text: &str) -> Result<Value, String> {
    parse_ast(text, MdOptions::default())
}

// ---------------------------------------------------------------------------
// URL safety & heading slugs
// ---------------------------------------------------------------------------

/// Allows relative URLs, fragments, queries and safe schemes (`http`, `https`,
/// `mailto`, `irc`, `ircs`, `xmpp`). Blocks `javascript:`, `vbscript:`,
/// `data:` and control-character obfuscated schemes.
fn is_safe_url(url: &str) -> bool {
    let cleaned: String = url
        .chars()
        .filter(|c| !c.is_ascii_control() && !c.is_ascii_whitespace())
        .collect();
    if cleaned.is_empty() {
        return true;
    }
    for (idx, byte) in cleaned.as_bytes().iter().enumerate() {
        match *byte {
            b'/' | b'?' | b'#' => return true,
            b':' => {
                let scheme = cleaned[..idx].to_ascii_lowercase();
                return matches!(
                    scheme.as_str(),
                    "http" | "https" | "mailto" | "irc" | "ircs" | "xmpp"
                );
            }
            _ => {}
        }
    }
    true
}

fn slugify(text: &str, seen: &mut HashMap<String, usize>) -> String {
    let mut base = String::new();
    let mut prev_dash = false;
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lc in ch.to_lowercase() {
                base.push(lc);
            }
            prev_dash = false;
        } else if (ch == '-' || ch == '_' || ch.is_whitespace()) && !base.is_empty() && !prev_dash {
            base.push('-');
            prev_dash = true;
        }
    }
    while base.ends_with('-') {
        base.pop();
    }
    if base.is_empty() {
        base.push_str("section");
    }
    let mut idx = *seen.get(&base).unwrap_or(&0);
    loop {
        let candidate = if idx == 0 {
            base.clone()
        } else {
            format!("{}-{}", base, idx)
        };
        idx += 1;
        if !seen.contains_key(&candidate) {
            seen.insert(base, idx);
            seen.entry(candidate.clone()).or_insert(1);
            return candidate;
        }
    }
}

fn escape_html_text(out: &mut String, text: &str) {
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            other => out.push(other),
        }
    }
}

fn heading_level_num(level: HeadingLevel) -> i64 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn blockquote_kind_str(kind: BlockQuoteKind) -> &'static str {
    match kind {
        BlockQuoteKind::Note => "note",
        BlockQuoteKind::Tip => "tip",
        BlockQuoteKind::Important => "important",
        BlockQuoteKind::Warning => "warning",
        BlockQuoteKind::Caution => "caution",
    }
}

fn alignment_str(align: Alignment) -> &'static str {
    match align {
        Alignment::None => "none",
        Alignment::Left => "left",
        Alignment::Center => "center",
        Alignment::Right => "right",
    }
}

// ---------------------------------------------------------------------------
// frontmatter
// ---------------------------------------------------------------------------

/// Extracts a leading YAML (`---`) or TOML (`+++`) frontmatter block from
/// `text`, returning `Some((format, raw, rest))` or `None`.
fn extract_frontmatter(text: &str) -> Option<(&'static str, &str, &str)> {
    let trimmed = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (format, delim, after_open) = if let Some(rest) = trimmed.strip_prefix("---\n") {
        ("yaml", "---", rest)
    } else if let Some(rest) = trimmed.strip_prefix("---\r\n") {
        ("yaml", "---", rest)
    } else if let Some(rest) = trimmed.strip_prefix("+++\n") {
        ("toml", "+++", rest)
    } else {
        let rest = trimmed.strip_prefix("+++\r\n")?;
        ("toml", "+++", rest)
    };

    let mut pos = 0usize;
    let bytes = after_open.as_bytes();
    while pos <= bytes.len() {
        let line_start = pos;
        let line_end = after_open[pos..]
            .find('\n')
            .map_or(bytes.len(), |offset| pos + offset);
        let line = after_open[line_start..line_end].trim_end_matches('\r');
        let is_close = line == delim || (format == "yaml" && line == "...");
        if is_close {
            let raw = &after_open[..line_start];
            let next_pos = if line_end < bytes.len() {
                line_end + 1
            } else {
                bytes.len()
            };
            return Some((format, raw, &after_open[next_pos..]));
        }
        if line_end == bytes.len() {
            break;
        }
        pos = line_end + 1;
    }
    None
}

fn builtin_frontmatter(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let text = str_arg(args, 0, "text")?;
    match extract_frontmatter(text) {
        Some((format, raw, body)) => {
            let mut fm = VmTable::new();
            fm.set_str("format", Value::str(format));
            fm.set_str("raw", Value::string(raw.replace("\r\n", "\n")));
            Ok(vec![new_table(fm), Value::string(body.to_string())])
        }
        None => Ok(vec![Value::Nil, Value::string(text.to_string())]),
    }
}

// ---------------------------------------------------------------------------
// toHtml
// ---------------------------------------------------------------------------

fn builtin_to_html(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let text = str_arg(args, 0, "text")?;
    let opts = MdOptions::from_args(args, 1)?;
    Ok(vec![Value::string(render_html(text, opts)?)])
}

fn render_html(text: &str, opts: MdOptions) -> Result<String, String> {
    let input = if opts.frontmatter {
        match extract_frontmatter(text) {
            Some((_, _, rest)) => rest,
            None => text,
        }
    } else {
        text
    };

    let parser = Parser::new_ext(input, opts.cmark_options());
    let raw_events: Vec<Event<'_>> = parser.collect();

    let mut filtered: Vec<Event<'_>> = Vec::with_capacity(raw_events.len());
    let mut depth = 0usize;
    let mut event_count = 0usize;
    let mut slugs: HashMap<String, usize> = HashMap::new();

    let mut idx = 0usize;
    while idx < raw_events.len() {
        event_count += 1;
        if event_count > MAX_NODES * 4 {
            return Err(format!(
                "markdown document exceeds the {} node limit",
                MAX_NODES
            ));
        }

        let ev = raw_events[idx].clone();

        match ev {
            Event::Start(Tag::Heading {
                level,
                id,
                classes,
                attrs,
            }) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(format!("nesting is deeper than {} levels", MAX_DEPTH));
                }
                if opts.heading_ids {
                    let final_id = match id {
                        Some(explicit) if !explicit.is_empty() => {
                            *slugs.entry(explicit.to_string()).or_insert(0) += 1;
                            explicit
                        }
                        _ => {
                            let mut title = String::new();
                            let mut j = idx + 1;
                            while j < raw_events.len() {
                                match &raw_events[j] {
                                    Event::End(TagEnd::Heading(_)) => break,
                                    Event::Text(t) | Event::Code(t) => title.push_str(t),
                                    Event::SoftBreak | Event::HardBreak => title.push(' '),
                                    _ => {}
                                }
                                j += 1;
                            }
                            CowStr::Boxed(slugify(&title, &mut slugs).into_boxed_str())
                        }
                    };
                    let mut open = format!("<{} id=\"", level);
                    escape_html_text(&mut open, &final_id);
                    open.push('"');
                    if !classes.is_empty() {
                        open.push_str(" class=\"");
                        for (ci, cls) in classes.iter().enumerate() {
                            if ci > 0 {
                                open.push(' ');
                            }
                            escape_html_text(&mut open, cls);
                        }
                        open.push('"');
                    }
                    open.push('>');
                    filtered.push(Event::Html(CowStr::Boxed(open.into_boxed_str())));
                } else {
                    filtered.push(Event::Start(Tag::Heading {
                        level,
                        id,
                        classes,
                        attrs,
                    }));
                }
            }
            Event::End(TagEnd::Heading(level)) => {
                depth = depth.saturating_sub(1);
                if opts.heading_ids {
                    let close = format!("</{}>\n", level);
                    filtered.push(Event::Html(CowStr::Boxed(close.into_boxed_str())));
                } else {
                    filtered.push(Event::End(TagEnd::Heading(level)));
                }
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(format!("nesting is deeper than {} levels", MAX_DEPTH));
                }
                let safe_url = if opts.unsafe_html || is_safe_url(&dest_url) {
                    dest_url
                } else {
                    CowStr::Borrowed("")
                };
                filtered.push(Event::Start(Tag::Link {
                    link_type,
                    dest_url: safe_url,
                    title,
                    id,
                }));
            }
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(format!("nesting is deeper than {} levels", MAX_DEPTH));
                }
                let safe_url = if opts.unsafe_html || is_safe_url(&dest_url) {
                    dest_url
                } else {
                    CowStr::Borrowed("")
                };
                filtered.push(Event::Start(Tag::Image {
                    link_type,
                    dest_url: safe_url,
                    title,
                    id,
                }));
            }
            Event::Start(tag) => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return Err(format!("nesting is deeper than {} levels", MAX_DEPTH));
                }
                filtered.push(Event::Start(tag));
            }
            Event::End(tag_end) => {
                depth = depth.saturating_sub(1);
                filtered.push(Event::End(tag_end));
            }
            Event::Html(raw) | Event::InlineHtml(raw) => {
                if opts.unsafe_html {
                    filtered.push(Event::Html(raw));
                } else {
                    filtered.push(Event::Text(raw));
                }
            }
            Event::SoftBreak if opts.breaks => {
                filtered.push(Event::HardBreak);
            }
            other => filtered.push(other),
        }
        idx += 1;
    }

    let mut out = String::new();
    cmark_html::push_html(&mut out, filtered.into_iter());
    if out.len() > MAX_OUTPUT {
        return Err(format!(
            "output exceeds the {} MiB limit",
            MAX_OUTPUT / (1024 * 1024)
        ));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// parse (AST plain tables)
// ---------------------------------------------------------------------------

fn builtin_parse(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let text = str_arg(args, 0, "text")?;
    let opts = MdOptions::from_args(args, 1)?;
    Ok(vec![parse_ast(text, opts)?])
}

#[derive(Debug)]
enum AstNode {
    Text(String),
    Code(String),
    SoftBreak,
    HardBreak,
    ThematicBreak,
    HtmlBlock(String),
    HtmlInline(String),
    FootnoteRef(String),
    Paragraph(Vec<AstNode>),
    Heading {
        level: i64,
        id: Option<String>,
        children: Vec<AstNode>,
    },
    BlockQuote {
        kind: Option<&'static str>,
        children: Vec<AstNode>,
    },
    CodeBlock {
        lang: Option<String>,
        text: String,
    },
    List {
        ordered: bool,
        start: Option<u64>,
        children: Vec<AstNode>,
    },
    ListItem {
        checked: Option<bool>,
        children: Vec<AstNode>,
    },
    Table {
        align: Vec<&'static str>,
        head: Vec<Vec<AstNode>>,
        rows: Vec<Vec<Vec<AstNode>>>,
    },
    Emphasis(Vec<AstNode>),
    Strong(Vec<AstNode>),
    Strikethrough(Vec<AstNode>),
    Link {
        url: String,
        title: Option<String>,
        children: Vec<AstNode>,
    },
    Image {
        url: String,
        title: Option<String>,
        children: Vec<AstNode>,
    },
    FootnoteDef {
        label: String,
        children: Vec<AstNode>,
    },
}

enum OpenFrame {
    Document(Vec<AstNode>),
    Paragraph(Vec<AstNode>),
    Heading {
        level: i64,
        id: Option<String>,
        children: Vec<AstNode>,
    },
    BlockQuote {
        kind: Option<&'static str>,
        children: Vec<AstNode>,
    },
    CodeBlock {
        lang: Option<String>,
        text: String,
    },
    HtmlBlock(String),
    List {
        ordered: bool,
        start: Option<u64>,
        children: Vec<AstNode>,
    },
    ListItem {
        checked: Option<bool>,
        children: Vec<AstNode>,
    },
    Table {
        align: Vec<&'static str>,
        head: Vec<Vec<AstNode>>,
        rows: Vec<Vec<Vec<AstNode>>>,
    },
    TableHead(Vec<Vec<AstNode>>),
    TableRow(Vec<Vec<AstNode>>),
    TableCell(Vec<AstNode>),
    Emphasis(Vec<AstNode>),
    Strong(Vec<AstNode>),
    Strikethrough(Vec<AstNode>),
    Link {
        url: String,
        title: Option<String>,
        children: Vec<AstNode>,
    },
    Image {
        url: String,
        title: Option<String>,
        children: Vec<AstNode>,
    },
    FootnoteDef {
        label: String,
        children: Vec<AstNode>,
    },
}

fn push_child(frame: &mut OpenFrame, child: AstNode) {
    let list = match frame {
        OpenFrame::Document(c)
        | OpenFrame::Paragraph(c)
        | OpenFrame::Heading { children: c, .. }
        | OpenFrame::BlockQuote { children: c, .. }
        | OpenFrame::List { children: c, .. }
        | OpenFrame::ListItem { children: c, .. }
        | OpenFrame::TableCell(c)
        | OpenFrame::Emphasis(c)
        | OpenFrame::Strong(c)
        | OpenFrame::Strikethrough(c)
        | OpenFrame::Link { children: c, .. }
        | OpenFrame::Image { children: c, .. }
        | OpenFrame::FootnoteDef { children: c, .. } => c,
        _ => return,
    };
    if let AstNode::Text(ref new_text) = child
        && let Some(AstNode::Text(prev)) = list.last_mut()
    {
        prev.push_str(new_text);
        return;
    }
    list.push(child);
}

fn collect_inline_text(nodes: &[AstNode], out: &mut String) {
    for node in nodes {
        match node {
            AstNode::Text(s) | AstNode::Code(s) => out.push_str(s),
            AstNode::SoftBreak | AstNode::HardBreak => out.push(' '),
            AstNode::Emphasis(c)
            | AstNode::Strong(c)
            | AstNode::Strikethrough(c)
            | AstNode::Link { children: c, .. }
            | AstNode::Image { children: c, .. }
            | AstNode::Paragraph(c) => collect_inline_text(c, out),
            _ => {}
        }
    }
}

fn ast_node_to_value(
    node: AstNode,
    slugs: &mut HashMap<String, usize>,
    heading_ids: bool,
) -> Value {
    let mut tbl = VmTable::new();
    match node {
        AstNode::Text(text) => {
            tbl.set_str("type", Value::str("text"));
            tbl.set_str("text", Value::string(text));
        }
        AstNode::Code(text) => {
            tbl.set_str("type", Value::str("code"));
            tbl.set_str("text", Value::string(text));
        }
        AstNode::SoftBreak => {
            tbl.set_str("type", Value::str("softBreak"));
        }
        AstNode::HardBreak => {
            tbl.set_str("type", Value::str("hardBreak"));
        }
        AstNode::ThematicBreak => {
            tbl.set_str("type", Value::str("thematicBreak"));
        }
        AstNode::HtmlBlock(text) => {
            tbl.set_str("type", Value::str("htmlBlock"));
            tbl.set_str("text", Value::string(text));
        }
        AstNode::HtmlInline(text) => {
            tbl.set_str("type", Value::str("htmlInline"));
            tbl.set_str("text", Value::string(text));
        }
        AstNode::FootnoteRef(label) => {
            tbl.set_str("type", Value::str("footnoteReference"));
            tbl.set_str("label", Value::string(label));
        }
        AstNode::Paragraph(children) => {
            tbl.set_str("type", Value::str("paragraph"));
            tbl.set_str("children", nodes_to_array(children, slugs, heading_ids));
        }
        AstNode::Heading {
            level,
            id,
            children,
        } => {
            let resolved_id = match id {
                Some(explicit) if !explicit.is_empty() => {
                    *slugs.entry(explicit.clone()).or_insert(0) += 1;
                    Some(explicit)
                }
                _ if heading_ids => {
                    let mut plain = String::new();
                    collect_inline_text(&children, &mut plain);
                    Some(slugify(&plain, slugs))
                }
                _ => None,
            };
            tbl.set_str("type", Value::str("heading"));
            tbl.set_str("level", Value::Int(level));
            if let Some(id) = resolved_id {
                tbl.set_str("id", Value::string(id));
            }
            tbl.set_str("children", nodes_to_array(children, slugs, heading_ids));
        }
        AstNode::BlockQuote { kind, children } => {
            tbl.set_str("type", Value::str("blockquote"));
            if let Some(kind) = kind {
                tbl.set_str("kind", Value::str(kind));
            }
            tbl.set_str("children", nodes_to_array(children, slugs, heading_ids));
        }
        AstNode::CodeBlock { lang, text } => {
            tbl.set_str("type", Value::str("codeBlock"));
            if let Some(lang) = lang {
                tbl.set_str("lang", Value::string(lang));
            }
            tbl.set_str("text", Value::string(text));
        }
        AstNode::List {
            ordered,
            start,
            children,
        } => {
            tbl.set_str("type", Value::str("list"));
            tbl.set_str("ordered", Value::Bool(ordered));
            if let Some(start) = start {
                tbl.set_str("start", Value::from_u64(start));
            }
            tbl.set_str("children", nodes_to_array(children, slugs, heading_ids));
        }
        AstNode::ListItem { checked, children } => {
            tbl.set_str("type", Value::str("listItem"));
            if let Some(checked) = checked {
                tbl.set_str("checked", Value::Bool(checked));
            }
            tbl.set_str("children", nodes_to_array(children, slugs, heading_ids));
        }
        AstNode::Table { align, head, rows } => {
            tbl.set_str("type", Value::str("table"));
            tbl.set_str("align", array(align.into_iter().map(Value::str).collect()));
            let head_val = array(
                head.into_iter()
                    .map(|cell| {
                        let mut ct = VmTable::new();
                        ct.set_str("type", Value::str("tableCell"));
                        ct.set_str("children", nodes_to_array(cell, slugs, heading_ids));
                        new_table(ct)
                    })
                    .collect(),
            );
            let rows_val = array(
                rows.into_iter()
                    .map(|row| {
                        array(
                            row.into_iter()
                                .map(|cell| {
                                    let mut ct = VmTable::new();
                                    ct.set_str("type", Value::str("tableCell"));
                                    ct.set_str(
                                        "children",
                                        nodes_to_array(cell, slugs, heading_ids),
                                    );
                                    new_table(ct)
                                })
                                .collect(),
                        )
                    })
                    .collect(),
            );
            tbl.set_str("head", head_val);
            tbl.set_str("rows", rows_val);
        }
        AstNode::Emphasis(children) => {
            tbl.set_str("type", Value::str("emphasis"));
            tbl.set_str("children", nodes_to_array(children, slugs, heading_ids));
        }
        AstNode::Strong(children) => {
            tbl.set_str("type", Value::str("strong"));
            tbl.set_str("children", nodes_to_array(children, slugs, heading_ids));
        }
        AstNode::Strikethrough(children) => {
            tbl.set_str("type", Value::str("strikethrough"));
            tbl.set_str("children", nodes_to_array(children, slugs, heading_ids));
        }
        AstNode::Link {
            url,
            title,
            children,
        } => {
            tbl.set_str("type", Value::str("link"));
            tbl.set_str("url", Value::string(url));
            if let Some(title) = title {
                tbl.set_str("title", Value::string(title));
            }
            tbl.set_str("children", nodes_to_array(children, slugs, heading_ids));
        }
        AstNode::Image {
            url,
            title,
            children,
        } => {
            let mut alt = String::new();
            collect_inline_text(&children, &mut alt);
            tbl.set_str("type", Value::str("image"));
            tbl.set_str("url", Value::string(url));
            tbl.set_str("alt", Value::string(alt));
            if let Some(title) = title {
                tbl.set_str("title", Value::string(title));
            }
            tbl.set_str("children", nodes_to_array(children, slugs, heading_ids));
        }
        AstNode::FootnoteDef { label, children } => {
            tbl.set_str("type", Value::str("footnoteDefinition"));
            tbl.set_str("label", Value::string(label));
            tbl.set_str("children", nodes_to_array(children, slugs, heading_ids));
        }
    }
    new_table(tbl)
}

fn nodes_to_array(
    nodes: Vec<AstNode>,
    slugs: &mut HashMap<String, usize>,
    heading_ids: bool,
) -> Value {
    array(
        nodes
            .into_iter()
            .map(|n| ast_node_to_value(n, slugs, heading_ids))
            .collect(),
    )
}

fn parse_ast(text: &str, opts: MdOptions) -> Result<Value, String> {
    let (frontmatter, body) = if opts.frontmatter {
        match extract_frontmatter(text) {
            Some((fmt, raw, rest)) => (Some((fmt, raw.replace("\r\n", "\n"))), rest),
            None => (None, text),
        }
    } else {
        (None, text)
    };

    let mut stack: Vec<OpenFrame> = vec![OpenFrame::Document(Vec::new())];
    let mut node_count = 0usize;

    let mut bump_nodes = || -> Result<(), String> {
        node_count += 1;
        if node_count > MAX_NODES {
            return Err(format!(
                "markdown document exceeds the {} node limit",
                MAX_NODES
            ));
        }
        Ok(())
    };

    for event in Parser::new_ext(body, opts.cmark_options()) {
        match event {
            Event::Start(tag) => {
                if stack.len() >= MAX_DEPTH {
                    return Err(format!("nesting is deeper than {} levels", MAX_DEPTH));
                }
                bump_nodes()?;
                let frame = match tag {
                    Tag::Paragraph => OpenFrame::Paragraph(Vec::new()),
                    Tag::Heading { level, id, .. } => OpenFrame::Heading {
                        level: heading_level_num(level),
                        id: id.map(|s| s.into_string()),
                        children: Vec::new(),
                    },
                    Tag::BlockQuote(kind) => OpenFrame::BlockQuote {
                        kind: kind.map(blockquote_kind_str),
                        children: Vec::new(),
                    },
                    Tag::CodeBlock(kind) => {
                        let lang = match kind {
                            CodeBlockKind::Indented => None,
                            CodeBlockKind::Fenced(info) => {
                                let token = info.split_whitespace().next().unwrap_or("");
                                if token.is_empty() {
                                    None
                                } else {
                                    Some(token.to_string())
                                }
                            }
                        };
                        OpenFrame::CodeBlock {
                            lang,
                            text: String::new(),
                        }
                    }
                    Tag::HtmlBlock => OpenFrame::HtmlBlock(String::new()),
                    Tag::List(start) => OpenFrame::List {
                        ordered: start.is_some(),
                        start,
                        children: Vec::new(),
                    },
                    Tag::Item => OpenFrame::ListItem {
                        checked: None,
                        children: Vec::new(),
                    },
                    Tag::Table(align) => OpenFrame::Table {
                        align: align.into_iter().map(alignment_str).collect(),
                        head: Vec::new(),
                        rows: Vec::new(),
                    },
                    Tag::TableHead => OpenFrame::TableHead(Vec::new()),
                    Tag::TableRow => OpenFrame::TableRow(Vec::new()),
                    Tag::TableCell => OpenFrame::TableCell(Vec::new()),
                    Tag::Emphasis => OpenFrame::Emphasis(Vec::new()),
                    Tag::Strong => OpenFrame::Strong(Vec::new()),
                    Tag::Strikethrough => OpenFrame::Strikethrough(Vec::new()),
                    Tag::Link {
                        dest_url, title, ..
                    } => OpenFrame::Link {
                        url: dest_url.into_string(),
                        title: if title.is_empty() {
                            None
                        } else {
                            Some(title.into_string())
                        },
                        children: Vec::new(),
                    },
                    Tag::Image {
                        dest_url, title, ..
                    } => OpenFrame::Image {
                        url: dest_url.into_string(),
                        title: if title.is_empty() {
                            None
                        } else {
                            Some(title.into_string())
                        },
                        children: Vec::new(),
                    },
                    Tag::FootnoteDefinition(label) => OpenFrame::FootnoteDef {
                        label: label.into_string(),
                        children: Vec::new(),
                    },
                    _ => OpenFrame::Paragraph(Vec::new()),
                };
                stack.push(frame);
            }
            Event::End(_) => {
                if stack.len() <= 1 {
                    continue;
                }
                let Some(finished) = stack.pop() else {
                    continue;
                };
                match finished {
                    OpenFrame::TableCell(children) => match stack.last_mut() {
                        Some(OpenFrame::TableHead(cells)) | Some(OpenFrame::TableRow(cells)) => {
                            cells.push(children);
                        }
                        _ => {}
                    },
                    OpenFrame::TableHead(cells) => {
                        if let Some(OpenFrame::Table { head, .. }) = stack.last_mut() {
                            *head = cells;
                        }
                    }
                    OpenFrame::TableRow(cells) => {
                        if let Some(OpenFrame::Table { rows, .. }) = stack.last_mut() {
                            rows.push(cells);
                        }
                    }
                    other => {
                        let node = match other {
                            OpenFrame::Paragraph(c) => AstNode::Paragraph(c),
                            OpenFrame::Heading {
                                level,
                                id,
                                children,
                            } => AstNode::Heading {
                                level,
                                id,
                                children,
                            },
                            OpenFrame::BlockQuote { kind, children } => {
                                AstNode::BlockQuote { kind, children }
                            }
                            OpenFrame::CodeBlock { lang, text } => {
                                AstNode::CodeBlock { lang, text }
                            }
                            OpenFrame::HtmlBlock(text) => AstNode::HtmlBlock(text),
                            OpenFrame::List {
                                ordered,
                                start,
                                children,
                            } => AstNode::List {
                                ordered,
                                start,
                                children,
                            },
                            OpenFrame::ListItem { checked, children } => {
                                AstNode::ListItem { checked, children }
                            }
                            OpenFrame::Table { align, head, rows } => {
                                AstNode::Table { align, head, rows }
                            }
                            OpenFrame::Emphasis(c) => AstNode::Emphasis(c),
                            OpenFrame::Strong(c) => AstNode::Strong(c),
                            OpenFrame::Strikethrough(c) => AstNode::Strikethrough(c),
                            OpenFrame::Link {
                                url,
                                title,
                                children,
                            } => AstNode::Link {
                                url,
                                title,
                                children,
                            },
                            OpenFrame::Image {
                                url,
                                title,
                                children,
                            } => AstNode::Image {
                                url,
                                title,
                                children,
                            },
                            OpenFrame::FootnoteDef { label, children } => {
                                AstNode::FootnoteDef { label, children }
                            }
                            OpenFrame::Document(_)
                            | OpenFrame::TableHead(_)
                            | OpenFrame::TableRow(_)
                            | OpenFrame::TableCell(_) => continue,
                        };
                        if let Some(parent) = stack.last_mut() {
                            push_child(parent, node);
                        }
                    }
                }
            }
            Event::Text(s) => match stack.last_mut() {
                Some(OpenFrame::CodeBlock { text, .. }) => text.push_str(&s),
                Some(OpenFrame::HtmlBlock(text)) => text.push_str(&s),
                Some(parent) => {
                    bump_nodes()?;
                    push_child(parent, AstNode::Text(s.into_string()));
                }
                None => {}
            },
            Event::Code(s) => {
                bump_nodes()?;
                if let Some(parent) = stack.last_mut() {
                    push_child(parent, AstNode::Code(s.into_string()));
                }
            }
            Event::Html(s) => match stack.last_mut() {
                Some(OpenFrame::HtmlBlock(text)) => text.push_str(&s),
                Some(parent) => {
                    bump_nodes()?;
                    push_child(parent, AstNode::HtmlBlock(s.into_string()));
                }
                None => {}
            },
            Event::InlineHtml(s) => {
                bump_nodes()?;
                if let Some(parent) = stack.last_mut() {
                    push_child(parent, AstNode::HtmlInline(s.into_string()));
                }
            }
            Event::FootnoteReference(label) => {
                bump_nodes()?;
                if let Some(parent) = stack.last_mut() {
                    push_child(parent, AstNode::FootnoteRef(label.into_string()));
                }
            }
            Event::SoftBreak => {
                bump_nodes()?;
                if let Some(parent) = stack.last_mut() {
                    push_child(
                        parent,
                        if opts.breaks {
                            AstNode::HardBreak
                        } else {
                            AstNode::SoftBreak
                        },
                    );
                }
            }
            Event::HardBreak => {
                bump_nodes()?;
                if let Some(parent) = stack.last_mut() {
                    push_child(parent, AstNode::HardBreak);
                }
            }
            Event::Rule => {
                bump_nodes()?;
                if let Some(parent) = stack.last_mut() {
                    push_child(parent, AstNode::ThematicBreak);
                }
            }
            Event::TaskListMarker(checked) => {
                if let Some(OpenFrame::ListItem { checked: slot, .. }) = stack.last_mut() {
                    *slot = Some(checked);
                }
            }
            Event::InlineMath(s) | Event::DisplayMath(s) => {
                bump_nodes()?;
                if let Some(parent) = stack.last_mut() {
                    push_child(parent, AstNode::Code(s.into_string()));
                }
            }
        }
    }

    let children = match stack.into_iter().next() {
        Some(OpenFrame::Document(c)) => c,
        _ => Vec::new(),
    };
    let mut slugs = HashMap::new();
    let mut doc = VmTable::new();
    doc.set_str("type", Value::str("document"));
    if let Some((fmt, raw)) = frontmatter {
        let mut fm = VmTable::new();
        fm.set_str("format", Value::str(fmt));
        fm.set_str("raw", Value::string(raw));
        doc.set_str("frontmatter", new_table(fm));
    }
    doc.set_str(
        "children",
        nodes_to_array(children, &mut slugs, opts.heading_ids),
    );
    Ok(new_table(doc))
}

// ---------------------------------------------------------------------------
// plain text extraction
// ---------------------------------------------------------------------------

fn builtin_text(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    match args.first() {
        Some(Value::String(s)) => {
            let input = match extract_frontmatter(s) {
                Some((_, _, rest)) => rest,
                None => s,
            };
            let mut out = String::new();
            let mut need_sep = false;
            for event in Parser::new_ext(input, MdOptions::default().cmark_options()) {
                match event {
                    Event::Text(t) | Event::Code(t) => {
                        if need_sep && !out.is_empty() && !out.ends_with('\n') {
                            out.push('\n');
                        }
                        need_sep = false;
                        out.push_str(&t);
                    }
                    Event::SoftBreak => out.push(' '),
                    Event::HardBreak => out.push('\n'),
                    Event::End(
                        TagEnd::Paragraph
                        | TagEnd::Heading(_)
                        | TagEnd::CodeBlock
                        | TagEnd::Item
                        | TagEnd::TableRow
                        | TagEnd::TableHead,
                    ) => {
                        need_sep = true;
                    }
                    Event::End(TagEnd::TableCell)
                        if !out.ends_with(' ') && !out.ends_with('\n') && !out.is_empty() =>
                    {
                        out.push(' ');
                    }
                    _ => {}
                }
                if out.len() > MAX_OUTPUT {
                    return Err(format!(
                        "output exceeds the {} MiB limit",
                        MAX_OUTPUT / (1024 * 1024)
                    ));
                }
            }
            Ok(vec![Value::string(out.trim_end().to_string())])
        }
        Some(Value::Table(t)) => {
            let mut out = String::new();
            let mut open = Vec::new();
            collect_node_table_text(t, 0, &mut open, &mut out)?;
            Ok(vec![Value::string(out)])
        }
        None | Some(Value::Nil) => Err("input must be provided".to_string()),
        Some(other) => Err(format!(
            "input must be a string or a node table, got {}",
            other.type_name()
        )),
    }
}

fn collect_node_table_text(
    t: &Rc<RefCell<VmTable>>,
    depth: usize,
    open: &mut Vec<*const RefCell<VmTable>>,
    out: &mut String,
) -> Result<(), String> {
    if depth >= MAX_DEPTH {
        return Err(format!("nesting is deeper than {} levels", MAX_DEPTH));
    }
    if out.len() > MAX_OUTPUT {
        return Err(format!(
            "output exceeds the {} MiB limit",
            MAX_OUTPUT / (1024 * 1024)
        ));
    }
    let ptr = Rc::as_ptr(t);
    if open.contains(&ptr) {
        return Err("node table contains itself".to_string());
    }
    open.push(ptr);

    let (kind, text, children, head, rows) = {
        let tbl = t.borrow();
        let kind = match tbl.get_str("type") {
            Value::String(s) => s.to_string(),
            _ => String::new(),
        };
        let text = match tbl.get_str("text") {
            Value::String(s) => Some(s.to_string()),
            _ => None,
        };
        let children = match tbl.get_str("children") {
            Value::Table(c) => Some(c.borrow().array.clone()),
            _ => None,
        };
        let head = match tbl.get_str("head") {
            Value::Table(c) => Some(c.borrow().array.clone()),
            _ => None,
        };
        let rows = match tbl.get_str("rows") {
            Value::Table(c) => Some(c.borrow().array.clone()),
            _ => None,
        };
        (kind, text, children, head, rows)
    };

    if let Some(s) = text {
        if !matches!(kind.as_str(), "htmlBlock" | "htmlInline") {
            out.push_str(&s);
        }
    } else if kind == "softBreak" {
        out.push(' ');
    } else if kind == "hardBreak" {
        out.push('\n');
    }

    if let Some(items) = children {
        for item in items {
            match item {
                Value::String(s) => out.push_str(&s),
                Value::Table(sub) => collect_node_table_text(&sub, depth + 1, open, out)?,
                _ => {}
            }
        }
    }
    if let Some(head_cells) = head {
        for cell in head_cells {
            if let Value::Table(sub) = cell {
                collect_node_table_text(&sub, depth + 1, open, out)?;
            }
        }
    }
    if let Some(table_rows) = rows {
        for row in table_rows {
            if let Value::Table(row_tbl) = row {
                let cells = row_tbl.borrow().array.clone();
                for cell in cells {
                    if let Value::Table(sub) = cell {
                        collect_node_table_text(&sub, depth + 1, open, out)?;
                    }
                }
            }
        }
    }

    open.pop();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_html_blocks_xss_and_javascript_urls() {
        let md =
            "Hello <script>alert(1)</script> [click](javascript:alert(1)) [ok](https://neyuki.cc)";
        let safe = render_html(md, MdOptions::default()).unwrap();
        assert!(!safe.contains("<script>"));
        assert!(safe.contains("&lt;script&gt;"));
        assert!(!safe.contains("javascript:"));
        assert!(safe.contains("href=\"https://neyuki.cc\""));

        let unsafe_html = render_html(
            md,
            MdOptions {
                unsafe_html: true,
                ..MdOptions::default()
            },
        )
        .unwrap();
        assert!(unsafe_html.contains("<script>alert(1)</script>"));
    }

    #[test]
    fn test_url_safety_edge_cases() {
        assert!(is_safe_url("https://example.com/a:b"));
        assert!(is_safe_url("/local/path:1"));
        assert!(is_safe_url("#heading"));
        assert!(is_safe_url("mailto:hi@example.com"));
        assert!(!is_safe_url("javascript:alert(1)"));
        assert!(!is_safe_url("  JaVa\tScRiPt:alert(1)"));
        assert!(!is_safe_url("data:text/html,<script>alert(1)</script>"));
        assert!(!is_safe_url("vbscript:msgbox(1)"));
    }

    #[test]
    fn test_heading_ids_and_deduplication() {
        let md = "# Hello World\n\n## Hello World\n\n# Intro {#setup}\n\n# Setup\n";
        let html = render_html(
            md,
            MdOptions {
                heading_ids: true,
                ..MdOptions::default()
            },
        )
        .unwrap();
        assert!(html.contains("<h1 id=\"hello-world\">Hello World</h1>"));
        assert!(html.contains("<h2 id=\"hello-world-1\">Hello World</h2>"));
        assert!(html.contains("<h1 id=\"setup\">Intro</h1>"));
        assert!(html.contains("<h1 id=\"setup-1\">Setup</h1>"));
    }

    #[test]
    fn test_frontmatter_yaml_and_toml() {
        let yaml_doc = "---\ntitle: Demo\n---\n# Hi\n";
        let (fmt, raw, body) = extract_frontmatter(yaml_doc).unwrap();
        assert_eq!(fmt, "yaml");
        assert_eq!(raw, "title: Demo\n");
        assert_eq!(body, "# Hi\n");

        let toml_doc = "+++\ntitle = \"Demo\"\n+++\nBody";
        let (fmt, raw, body) = extract_frontmatter(toml_doc).unwrap();
        assert_eq!(fmt, "toml");
        assert_eq!(raw, "title = \"Demo\"\n");
        assert_eq!(body, "Body");

        // A thematic break immediately after frontmatter is kept as body content,
        // not stripped a second time.
        let double_dash = "---\ntitle: Post\n---\n---\nParagraph\n---\n";
        let html = render_html(double_dash, MdOptions::default()).unwrap();
        assert!(html.contains("Paragraph"));
        assert!(html.contains("<hr />"));
    }
}
