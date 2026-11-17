//! `require` for registry packages.
//!
//! - `@pkg/<user>/<name>[/<module>]` loads from the project's
//!   `neyuki_packages/` (found from the entry script's directory upwards).
//!   Outside a project it falls back to the global packages, so a library
//!   that requires its own dependencies this way works in both places.
//! - `@<user>/<name>[/<module>]` loads from the global packages
//!   (`neyuki pkg install -g`).
//!
//! Without a module path the package's entry file is loaded: `entry` from
//! its `neyuki.toml`, else `init.nyk`, else `main.nyk`.

use std::path::{Path, PathBuf};

use super::Located;
use crate::pkg::paths::{self, MANIFEST, PACKAGES_DIR, PackageName};

pub struct PackageRequest {
    pub name: PackageName,
    /// The module inside the package, `/`-separated, or empty for the entry.
    pub module: String,
    /// `@pkg/...` (project) rather than `@user/...` (global).
    pub local: bool,
}

/// Recognizes a package request, or `None` when `spec` names something else.
pub fn parse(spec: &str) -> Option<Result<PackageRequest, String>> {
    let (local, rest) = match spec.strip_prefix("@pkg/") {
        Some(rest) => (true, rest),
        None if spec.starts_with('@') && !spec.starts_with("@neyuki/") => (false, &spec[1..]),
        None => return None,
    };
    let mut parts = rest.splitn(3, '/');
    let scope = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    let module = parts
        .next()
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string();
    Some(
        format!("@{}/{}", scope, name)
            .parse::<PackageName>()
            .map(|name| PackageRequest {
                name,
                module,
                local,
            })
            .map_err(|_| {
                format!(
                    "cannot find module '{}': package modules are named @pkg/<user>/<package> \
                     (project) or @<user>/<package> (global)",
                    spec
                )
            }),
    )
}

/// The entry file named in a package's `neyuki.toml`.
fn manifest_entry(text: &str) -> Option<String> {
    crate::pkg::PackageManifest::parse(text)
        .ok()
        .map(|m| m.entry.trim_start_matches("./").to_string())
        .filter(|entry| !entry.is_empty() && !entry.contains("..") && !entry.starts_with('/'))
}

/// File names to try inside a package directory, in order.
fn candidates(module: &str, entry: Option<String>) -> Vec<String> {
    if module.is_empty() {
        let mut list: Vec<String> = entry.into_iter().collect();
        list.push("init.nyk".to_string());
        list.push("main.nyk".to_string());
        return list;
    }
    if module.ends_with(".nyk") || module.ends_with(".nykb") {
        return vec![module.to_string()];
    }
    vec![
        format!("{}.nyk", module),
        format!("{}.nykb", module),
        format!("{}/init.nyk", module),
    ]
}

fn find_on_disk(dir: &Path, module: &str) -> Option<PathBuf> {
    if !dir.is_dir() {
        return None;
    }
    let entry = std::fs::read_to_string(dir.join(MANIFEST))
        .ok()
        .and_then(|t| manifest_entry(&t));
    candidates(module, entry)
        .into_iter()
        .map(|file| dir.join(file))
        .find(|path| path.is_file())
}

/// Inside a built app, project packages are in the bundle under
/// `neyuki_packages/` (see `neyuki build`).
fn find_in_bundle(name: &PackageName, module: &str) -> Option<String> {
    let bundle = crate::app::installed()?;
    let base = format!("{}/{}/{}", PACKAGES_DIR, name.scope, name.name);
    let entry = bundle
        .read(&format!("{}/{}", base, MANIFEST))
        .and_then(Result::ok)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| manifest_entry(&text));
    candidates(module, entry).into_iter().find_map(|file| {
        let path = format!("{}/{}", base, file);
        match bundle.entry(&path) {
            Some(crate::app::Entry::File { .. }) => crate::app::bundle::normalize(&path),
            _ => None,
        }
    })
}

/// Finds the file for a package request, starting the project search at
/// `base_dir` (the entry script's directory) or the working directory.
pub fn locate(request: &PackageRequest, base_dir: Option<&Path>) -> Result<Located, String> {
    let name = &request.name;
    let what = if request.module.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", name, request.module)
    };

    let mut roots: Vec<PathBuf> = Vec::new();
    if request.local {
        if let Some(found) = find_in_bundle(name, &request.module) {
            return Ok(Located::Bundled(found));
        }
        let starts = [
            base_dir.map(Path::to_path_buf),
            std::env::current_dir().ok(),
        ];
        for start in starts.into_iter().flatten() {
            if let Some(root) = paths::find_project_root(&start) {
                let dir = root.join(PACKAGES_DIR);
                if !roots.contains(&dir) {
                    roots.push(dir);
                }
            }
        }
    }
    if let Ok(global) = paths::global_packages_dir() {
        roots.push(global);
    }

    for root in &roots {
        let dir = name.dir_in(root);
        if dir.is_dir() {
            return find_on_disk(&dir, &request.module)
                .map(Located::Disk)
                .ok_or_else(|| {
                    format!(
                        "cannot find module '{}': the package has no such file",
                        what
                    )
                });
        }
    }

    Err(if request.local {
        format!(
            "cannot find module '@pkg/{}': {} is not installed; run 'neyuki pkg add {}'",
            &what[1..],
            name,
            name
        )
    } else {
        format!(
            "cannot find module '{}': {} is not installed globally; run 'neyuki pkg install -g {}' \
             (or use require(\"@pkg/{}\") for a project dependency)",
            what,
            name,
            name,
            &what[1..]
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests() {
        let req = parse("@pkg/yuki/router").unwrap().unwrap();
        assert!(req.local && req.module.is_empty() && req.name.to_string() == "@yuki/router");
        let req = parse("@yuki/router/middleware/auth").unwrap().unwrap();
        assert!(!req.local);
        assert_eq!(req.module, "middleware/auth");
        assert!(parse("@neyuki/fs").is_none());
        assert!(parse("fs").is_none());
        assert!(parse("./x").is_none());
        assert!(parse("@pkg/Yuki/x").unwrap().is_err());
        assert!(parse("@yuki").unwrap().is_err());
    }

    #[test]
    fn entry_candidates() {
        assert_eq!(
            candidates("", Some("src/lib.nyk".into())),
            vec!["src/lib.nyk", "init.nyk", "main.nyk"]
        );
        assert_eq!(
            candidates("util", None),
            vec!["util.nyk", "util.nykb", "util/init.nyk"]
        );
        assert_eq!(manifest_entry("[package]\nentry = \"../x.nyk\"\n"), None);
    }
}
