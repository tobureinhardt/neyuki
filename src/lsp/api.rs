//! The library reference, embedded in the binary and read once.
//!
//! `docs/reference/*.md` is compiled into `neyuki` by `build.rs`, so what the
//! language server knows about the standard library always matches the
//! installed version. Each page is `# title`, an overview, then one
//! `## heading` per entry:
//!
//! ```text
//! ## name(param: type, other: type?): result     a function
//! ## Type:method(param: type): result            a method of an object type
//! ## name: type                                  a value
//! ```
//!
//! A page titled `@neyuki/x` is a module loaded with `require`, `globals` is
//! always in scope and every other page is a namespace such as `math`.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Every `docs/reference/*.md` page but the README, as (file name without
/// extension, text).
const PAGES: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/reference_pages.rs"));

/// Where the pages are published; an entry's page name is appended.
pub const REFERENCE_URL: &str = "https://neyuki.cc/docs/reference/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Function,
    /// A function of an object type, written `Type:method`.
    Method,
    Value,
    /// A name that only has members, such as `hid.keyboard`.
    Namespace,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub ty: String,
    pub optional: bool,
    /// The parameter as written: `init: number?`.
    pub label: String,
}

/// One documented name.
#[derive(Clone, Debug)]
pub struct Entry {
    /// The full name: `math.floor`, `print`, `Window:clear`.
    pub name: String,
    pub kind: EntryKind,
    /// The object type of a method.
    pub owner: Option<String>,
    pub params: Vec<Param>,
    /// What it returns, or its type for a value; `()` for nothing.
    pub returns: String,
    /// The heading as written.
    pub signature: String,
    /// The markdown under the heading.
    pub doc: String,
    /// The page it is documented on, such as `math`.
    pub page: String,
}

/// One reference page.
#[derive(Clone, Debug)]
pub struct Library {
    pub page: String,
    pub title: String,
    /// The name scripts reach the page by: `@neyuki/fs`, `math`, or none for
    /// the globals.
    pub module: Option<String>,
    /// The overview above the first entry.
    pub doc: String,
}

/// Where completion, hover and signature help get the members of namespaces
/// and object types. The standard library is `Api`; a source for the modules
/// of a project would implement this as well and be consulted next to it.
pub trait MemberSource {
    /// Entries in scope without a namespace: `print`, `pcall`, ...
    fn globals(&self) -> Vec<Entry>;
    /// Namespaces in scope without `require`: `math`, `string`, ...
    fn standard_namespaces(&self) -> Vec<Library>;
    /// Everything `require` accepts, with the name it is written by in
    /// `Library::module`.
    fn modules(&self) -> Vec<Library>;
    /// The namespace a `require` argument loads: `@neyuki/fs` -> `fs`.
    fn namespace_for_module(&self, module: &str) -> Option<String>;
    /// The page of a namespace, for hover.
    fn library_for_namespace(&self, namespace: &str) -> Option<Library>;
    /// An entry by full name: `math.floor`, `Window:clear`.
    fn entry(&self, name: &str) -> Option<Entry>;
    /// Functions, values and nested namespaces directly under `namespace`.
    fn namespace_members(&self, namespace: &str) -> Vec<Entry>;
    /// The methods of an object type.
    fn type_methods(&self, type_name: &str) -> Vec<Entry>;
    /// Every object type that has methods.
    fn type_names(&self) -> Vec<String>;
}

/// The parsed library reference.
pub struct Api {
    pub libraries: Vec<Library>,
    pub entries: Vec<Entry>,
    /// Headings that are not entries (prose sections), as (page, heading).
    pub skipped_headings: Vec<(String, String)>,
    by_name: HashMap<String, usize>,
    by_module: HashMap<String, usize>,
    methods: HashMap<String, Vec<usize>>,
}

/// The reference of the running version, parsed on first use.
pub fn api() -> &'static Api {
    static API: OnceLock<Api> = OnceLock::new();
    API.get_or_init(|| Api::from_pages(PAGES))
}

impl Api {
    fn from_pages(pages: &[(&str, &str)]) -> Self {
        let mut libraries = Vec::new();
        let mut entries = Vec::new();
        let mut skipped_headings = Vec::new();
        for (page, text) in pages {
            read_page(
                page,
                text,
                &mut libraries,
                &mut entries,
                &mut skipped_headings,
            );
        }

        // The later of two entries with one name wins.
        let mut by_name = HashMap::new();
        let mut methods: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            by_name.insert(entry.name.clone(), index);
            if let (EntryKind::Method, Some(owner)) = (entry.kind, &entry.owner) {
                methods.entry(owner.clone()).or_default().push(index);
            }
        }
        let mut by_module = HashMap::new();
        for (index, lib) in libraries.iter().enumerate() {
            if let Some(module) = &lib.module {
                by_module.insert(module.clone(), index);
            }
        }

        Self {
            libraries,
            entries,
            skipped_headings,
            by_name,
            by_module,
            methods,
        }
    }
}

impl MemberSource for Api {
    fn globals(&self) -> Vec<Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.page == "globals")
            .cloned()
            .collect()
    }

    fn standard_namespaces(&self) -> Vec<Library> {
        self.libraries
            .iter()
            .filter(|lib| lib.module.as_ref().is_some_and(|m| !m.starts_with('@')))
            .cloned()
            .collect()
    }

    fn modules(&self) -> Vec<Library> {
        self.libraries
            .iter()
            .filter(|lib| lib.module.is_some())
            .cloned()
            .collect()
    }

    fn namespace_for_module(&self, module: &str) -> Option<String> {
        if let Some(root) = module.strip_prefix("@neyuki/") {
            if self.by_module.contains_key(module) || self.by_module.contains_key(root) {
                return Some(root.to_string());
            }
            return None;
        }
        self.by_module
            .contains_key(module)
            .then(|| module.to_string())
    }

    fn library_for_namespace(&self, namespace: &str) -> Option<Library> {
        let bundled = format!("@neyuki/{namespace}");
        self.libraries
            .iter()
            .find(|lib| {
                lib.module
                    .as_ref()
                    .is_some_and(|m| m == namespace || *m == bundled)
            })
            .cloned()
    }

    fn entry(&self, name: &str) -> Option<Entry> {
        self.by_name
            .get(name)
            .map(|&index| self.entries[index].clone())
    }

    fn namespace_members(&self, namespace: &str) -> Vec<Entry> {
        let prefix = format!("{namespace}.");
        let mut members = Vec::new();
        let mut nested: Vec<&str> = Vec::new();
        for entry in &self.entries {
            if entry.kind == EntryKind::Method {
                continue;
            }
            let Some(rest) = entry.name.strip_prefix(&prefix) else {
                continue;
            };
            match rest.split_once('.') {
                None => members.push(entry.clone()),
                Some((first, _)) => {
                    if !nested.contains(&first) {
                        nested.push(first);
                    }
                }
            }
        }
        for name in nested {
            let full = format!("{prefix}{name}");
            if !members.iter().any(|entry| entry.name == full) {
                members.push(Entry {
                    name: full.clone(),
                    kind: EntryKind::Namespace,
                    owner: None,
                    params: Vec::new(),
                    returns: String::new(),
                    signature: full,
                    doc: String::new(),
                    page: String::new(),
                });
            }
        }
        members
    }

    fn type_methods(&self, type_name: &str) -> Vec<Entry> {
        self.methods
            .get(type_name)
            .map(|indexes| indexes.iter().map(|&i| self.entries[i].clone()).collect())
            .unwrap_or_default()
    }

    fn type_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.methods.keys().cloned().collect();
        names.sort();
        names
    }
}

/// Reads one page into `libraries` and `entries`. A heading that is not an
/// entry (a section of prose) is recorded in `skipped` and otherwise ignored.
fn read_page(
    page: &str,
    text: &str,
    libraries: &mut Vec<Library>,
    entries: &mut Vec<Entry>,
    skipped: &mut Vec<(String, String)>,
) {
    let text = text.replace("\r\n", "\n");
    let title = text
        .lines()
        .find_map(|line| line.strip_prefix("# ").filter(|t| !t.is_empty()))
        .unwrap_or(page)
        .to_string();

    // Everything before the first `## ` is the overview; each later section
    // is a heading and the lines under it.
    let mut intro: Vec<&str> = Vec::new();
    let mut sections: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in text.split('\n') {
        if let Some(heading) = line.strip_prefix("## ") {
            sections.push((heading.trim(), Vec::new()));
        } else if let Some((_, body)) = sections.last_mut() {
            body.push(line);
        } else {
            intro.push(line);
        }
    }
    if intro
        .first()
        .is_some_and(|line| line.starts_with("# ") && line.len() > 2)
    {
        intro.remove(0);
    }

    let module = if title.starts_with("@neyuki/") {
        Some(title.clone())
    } else if page == "globals" {
        None
    } else {
        Some(page.to_string())
    };
    libraries.push(Library {
        page: page.to_string(),
        title,
        module,
        doc: intro.join("\n").trim().to_string(),
    });

    for (heading, body) in sections {
        match parse_heading(heading) {
            Some(mut entry) => {
                entry.doc = body.join("\n").trim().to_string();
                entry.page = page.to_string();
                entries.push(entry);
            }
            None => skipped.push((page.to_string(), heading.to_string())),
        }
    }
}

/// Splits `text` at commas that are not nested in brackets.
fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (index, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&text[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    parts.push(&text[start..]);
    parts
        .into_iter()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

/// The index of the bracket closing the one at `open`.
fn matching_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    for (index, c) in text[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + index);
                }
            }
            _ => {}
        }
    }
    None
}

/// `name: type`, `name: type?`, `...: type` or a bare name as a parameter.
fn parse_param(text: &str) -> Param {
    let (name, ty) = match text.split_once(':') {
        Some((name, ty)) => (name.trim(), ty.trim()),
        None => (text.trim(), "any"),
    };
    Param {
        name: name.to_string(),
        ty: ty.to_string(),
        optional: ty.ends_with('?') || name == "...",
        label: text.to_string(),
    }
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Reads a reference heading into an entry (without its `doc` and `page`),
/// or `None` if it is not one.
fn parse_heading(heading: &str) -> Option<Entry> {
    let bytes = heading.as_bytes();
    let first = *bytes.first()?;
    if !(first.is_ascii_alphabetic() || first == b'_' || first == b'@') {
        return None;
    }
    let mut end = 1;
    while end < bytes.len() && (is_name_byte(bytes[end]) || bytes[end] == b'.') {
        end += 1;
    }
    let head = &heading[..end];

    // `Type:method`: a colon straight after the name and a name after it.
    let mut name_end = end;
    let mut method = None;
    if bytes.get(end) == Some(&b':')
        && let Some(&c) = bytes.get(end + 1)
        && (c.is_ascii_alphabetic() || c == b'_')
    {
        let mut stop = end + 2;
        while stop < bytes.len() && is_name_byte(bytes[stop]) {
            stop += 1;
        }
        method = Some(&heading[end + 1..stop]);
        name_end = stop;
    }

    let owner = method.map(|_| head.to_string());
    let name = match method {
        Some(method) => format!("{head}:{method}"),
        None => head.to_string(),
    };
    let rest = &heading[name_end..];

    let mut entry = Entry {
        name,
        kind: EntryKind::Value,
        owner,
        params: Vec::new(),
        returns: String::new(),
        signature: heading.to_string(),
        doc: String::new(),
        page: String::new(),
    };
    if rest.starts_with('(') {
        let close = matching_paren(heading, name_end)?;
        let inner = &heading[name_end + 1..close];
        let after = heading[close + 1..].trim();
        entry.kind = if entry.owner.is_some() {
            EntryKind::Method
        } else {
            EntryKind::Function
        };
        entry.params = split_top_level(inner)
            .iter()
            .map(|param| parse_param(param))
            .collect();
        entry.returns = match after.strip_prefix(':') {
            Some(returns) => returns.trim().to_string(),
            None => "()".to_string(),
        };
        Some(entry)
    } else if let Some(returns) = rest.strip_prefix(':') {
        entry.returns = returns.trim().to_string();
        Some(entry)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_heading_forms() {
        let find = parse_heading(
            "string.find(s: string, pattern: string, init: number?): (number?, number?)",
        )
        .unwrap();
        assert_eq!(find.name, "string.find");
        assert_eq!(find.kind, EntryKind::Function);
        assert_eq!(find.params.len(), 3);
        assert_eq!(find.params[2].name, "init");
        assert_eq!(find.params[2].ty, "number?");
        assert!(find.params[2].optional);
        assert!(!find.params[0].optional);
        assert_eq!(find.returns, "(number?, number?)");

        let clear = parse_heading("Window:clear(color: any?): ()").unwrap();
        assert_eq!(clear.name, "Window:clear");
        assert_eq!(clear.kind, EntryKind::Method);
        assert_eq!(clear.owner.as_deref(), Some("Window"));

        let print = parse_heading("print(...: any): ()").unwrap();
        assert!(print.params[0].optional);

        let version = parse_heading("_VERSION: string").unwrap();
        assert_eq!(version.kind, EntryKind::Value);
        assert_eq!(version.returns, "string");

        // No result means nothing is returned; brackets nest in parameters.
        let cancel = parse_heading("task.cancel(thread: thread)").unwrap();
        assert_eq!(cancel.returns, "()");
        let run = parse_heading(
            "ui.run(handler: (events: {table}, dt: number) -> any, options: table?): ()",
        )
        .unwrap();
        assert_eq!(run.params.len(), 2);
        assert_eq!(run.params[0].name, "handler");
    }

    #[test]
    fn test_parse_heading_rejects_prose_without_panicking() {
        for heading in [
            "",
            "Blocking calls",
            "Two words (not a signature)",
            "1abc()",
            "broken(a: (b",
            "\u{1f980}()",
            "name\u{e9}(x)",
        ] {
            let _ = parse_heading(heading);
        }
        assert!(parse_heading("Blocking calls").is_none());
        assert!(parse_heading("broken(a: (b").is_none());
    }

    #[test]
    fn test_embedded_headings_all_parse() {
        let api = api();
        assert!(api.entries.len() > 300, "{} entries", api.entries.len());
        // Prose sections are the only headings that are not entries. Naming
        // them here means a mistyped heading in the docs fails this test
        // instead of silently dropping out of completion and hover.
        assert_eq!(
            api.skipped_headings,
            vec![("task".to_string(), "Blocking calls".to_string())]
        );
        for lib in &api.libraries {
            assert!(!lib.doc.is_empty(), "page {} has no overview", lib.page);
        }
    }

    #[test]
    fn test_modules_namespaces_and_types() {
        let api = api();
        assert_eq!(
            api.namespace_for_module("@neyuki/ui").as_deref(),
            Some("ui")
        );
        assert_eq!(
            api.namespace_for_module("@neyuki/math").as_deref(),
            Some("math")
        );
        assert_eq!(api.namespace_for_module("math").as_deref(), Some("math"));
        assert_eq!(api.namespace_for_module("@neyuki/nope"), None);
        assert_eq!(api.namespace_for_module("some/file"), None);

        let standard: Vec<String> = api
            .standard_namespaces()
            .into_iter()
            .filter_map(|lib| lib.module)
            .collect();
        assert!(standard.contains(&"math".to_string()));
        assert!(!standard.contains(&"@neyuki/fs".to_string()));

        // Nested namespaces show up as members of their parent.
        let hid = api.namespace_members("hid");
        assert!(
            hid.iter()
                .any(|e| e.name == "hid.keyboard" && e.kind == EntryKind::Namespace)
        );
        assert!(
            api.namespace_members("ui.clipboard")
                .iter()
                .any(|e| e.name == "ui.clipboard.get")
        );

        assert!(
            api.type_methods("Window")
                .iter()
                .any(|e| e.name == "Window:clear")
        );
        assert!(api.type_names().contains(&"Window".to_string()));
        assert_eq!(api.entry("ui.open").unwrap().returns, "Window");
    }
}
