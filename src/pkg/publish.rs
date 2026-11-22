//! Packing a project for `neyuki pkg publish`.
//!
//! Every file under the project goes in, except hidden files and
//! directories, `neyuki_packages`, `target`, `node_modules`, the lockfile
//! and whatever `.neyukiignore` lists (one glob per line: `*` and `?` stay
//! within a path segment, `**` crosses segments, a leading `/` anchors to
//! the project root, a trailing `/` matches only directories, and a pattern
//! without `/` matches a name at any depth). `neyuki.toml` is always
//! included, since installs read the entry file from it.

use std::fs;
use std::path::Path;

use serde_json::{Map, Value as Json, json};

use super::manifest::PackageManifest;
use super::paths::{LOCKFILE, MANIFEST, PACKAGES_DIR};

/// The largest archive the registry takes from anyone: trusted publishers
/// (the official native packages, one library per target) may upload this
/// much, everyone else `STANDARD_ARCHIVE`. The registry enforces which
/// applies; the CLI only refuses what no account could publish.
pub const MAX_ARCHIVE: usize = 40 * 1024 * 1024;
/// The archive limit for most publishers.
pub const STANDARD_ARCHIVE: usize = 10 * 1024 * 1024;

fn megabytes(bytes: usize) -> f64 {
    bytes as f64 / 1024.0 / 1024.0
}

/// Checks a packed archive's size: an error past `MAX_ARCHIVE`, and a note
/// past `STANDARD_ARCHIVE`, which only trusted publishers may upload.
pub fn check_size(len: usize) -> Result<Option<String>, String> {
    if len > MAX_ARCHIVE {
        return Err(format!(
            "the package is {:.1} MB compressed; the registry takes at most {} MB. List what to leave out in .neyukiignore",
            megabytes(len),
            MAX_ARCHIVE / (1024 * 1024)
        ));
    }
    Ok((len > STANDARD_ARCHIVE).then(|| {
        format!(
            "the package is {:.1} MB compressed: over {} MB, the registry only takes it from trusted publishers",
            megabytes(len),
            STANDARD_ARCHIVE / (1024 * 1024)
        )
    }))
}
const MAX_README: usize = 200_000;
const ALWAYS_SKIPPED: &[&str] = &[PACKAGES_DIR, "target", "node_modules"];

struct Pattern {
    glob: String,
    anchored: bool,
    dir_only: bool,
}

fn parse_ignore(text: &str) -> Vec<Pattern> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let dir_only = line.ends_with('/');
            let line = line.trim_end_matches('/');
            let anchored = line.starts_with('/') || line.trim_start_matches('/').contains('/');
            Pattern {
                glob: line.trim_start_matches('/').to_string(),
                anchored,
                dir_only,
            }
        })
        .collect()
}

/// Glob matching over `/`-separated paths.
fn glob_match(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some(b'*') if pattern.get(1) == Some(&b'*') => {
            let rest = pattern[2..].strip_prefix(b"/").unwrap_or(&pattern[2..]);
            (0..=text.len())
                .any(|i| (i == 0 || text[i - 1] == b'/') && glob_match(rest, &text[i..]))
                || glob_match(rest, text)
        }
        Some(b'*') => {
            let rest = &pattern[1..];
            (0..=text.len())
                .take_while(|&i| i == 0 || text[i - 1] != b'/')
                .any(|i| glob_match(rest, &text[i..]))
        }
        Some(b'?') => !text.is_empty() && text[0] != b'/' && glob_match(&pattern[1..], &text[1..]),
        Some(&c) => !text.is_empty() && text[0] == c && glob_match(&pattern[1..], &text[1..]),
    }
}

fn ignored(patterns: &[Pattern], relative: &str, is_dir: bool) -> bool {
    patterns.iter().any(|p| {
        if p.dir_only && !is_dir {
            return false;
        }
        if p.anchored {
            glob_match(p.glob.as_bytes(), relative.as_bytes())
        } else {
            let name = relative.rsplit('/').next().unwrap_or(relative);
            glob_match(p.glob.as_bytes(), name.as_bytes())
        }
    })
}

fn walk(
    root: &Path,
    dir: &Path,
    patterns: &[Pattern],
    out: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), String> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .map_err(|e| format!("cannot list {}: {}", dir.display(), e))?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| format!("{} is outside the project", path.display()))?
            .to_string_lossy()
            .replace('\\', "/");
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
        if kind.is_symlink() {
            continue; // archives hold no links, and following them could leave the project
        }
        if kind.is_dir() {
            if dir == root && ALWAYS_SKIPPED.contains(&name.as_str()) {
                continue;
            }
            if !ignored(patterns, &relative, true) {
                walk(root, &path, patterns, out)?;
            }
        } else if kind.is_file() {
            if relative == LOCKFILE || (relative != MANIFEST && ignored(patterns, &relative, false))
            {
                continue;
            }
            let bytes =
                fs::read(&path).map_err(|e| format!("cannot read {}: {}", path.display(), e))?;
            out.push((relative, bytes));
        }
    }
    Ok(())
}

/// The files to publish from the project at `root`, as (path, contents).
pub fn collect(root: &Path) -> Result<Vec<(String, Vec<u8>)>, String> {
    let patterns = match fs::read_to_string(root.join(".neyukiignore")) {
        Ok(text) => parse_ignore(&text),
        Err(_) => Vec::new(),
    };
    let mut files = Vec::new();
    walk(root, root, &patterns, &mut files)?;
    Ok(files)
}

/// Checks the manifest is publishable and builds the metadata the registry
/// stores next to the archive.
pub fn metadata(manifest: &PackageManifest, files: &[(String, Vec<u8>)]) -> Result<Json, String> {
    let name: super::PackageName = manifest.name.parse().map_err(|e: String| {
        format!(
            "{}. Set name in {} to @<your username>/<package>",
            e, MANIFEST
        )
    })?;
    let entry = manifest.entry.trim_start_matches("./").to_string();
    if let Some(native) = &manifest.native {
        super::native::check_publish(native, files)?;
    }
    // A package with native libraries may have no Neyuki code at all:
    // requiring it then loads the library.
    if manifest.native.is_none() && !files.iter().any(|(path, _)| *path == entry) {
        return Err(format!(
            "the entry file '{}' is not in the package; set entry in {} or add the file",
            entry, MANIFEST
        ));
    }
    let readme = files
        .iter()
        .find(|(path, _)| path.eq_ignore_ascii_case("readme.md"))
        .map(|(_, bytes)| {
            let text = String::from_utf8_lossy(bytes).to_string();
            if text.len() > MAX_README {
                let mut cut = MAX_README;
                while !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                text[..cut].to_string()
            } else {
                text
            }
        })
        .unwrap_or_default();

    let mut dependencies = Map::new();
    for (dep, req) in &manifest.dependencies {
        let dep_name: super::PackageName = dep
            .parse()
            .map_err(|e: String| format!("dependency {}: {}", dep, e))?;
        if dep_name == name {
            return Err("a package cannot depend on itself".to_string());
        }
        dependencies.insert(dep.clone(), Json::String(req.to_string()));
    }

    Ok(json!({
        "name": name.to_string(),
        "version": manifest.version.to_string(),
        "description": manifest.description.clone().unwrap_or_default(),
        "entry": entry,
        "license": manifest.license,
        "repository": manifest.repository,
        "keywords": manifest.keywords,
        "dependencies": dependencies,
        "readme": readme,
        "native": manifest.native.as_ref().map(|n| json!({
            "abi": n.abi,
            "targets": n.targets.keys().collect::<Vec<_>>(),
        })),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_size_limits() {
        assert_eq!(check_size(STANDARD_ARCHIVE).unwrap(), None);
        let note = check_size(STANDARD_ARCHIVE + 1).unwrap().unwrap();
        assert!(note.contains("trusted publishers"), "{note}");
        assert!(check_size(MAX_ARCHIVE).is_ok());
        let err = check_size(MAX_ARCHIVE + 1).unwrap_err();
        assert!(
            err.contains("at most 40 MB") && err.contains(".neyukiignore"),
            "{err}"
        );
    }

    #[test]
    fn ignore_globs() {
        let patterns = parse_ignore("# comment\n*.log\n/build/\ndocs/**/*.png\ntests/\n");
        assert!(ignored(&patterns, "debug.log", false));
        assert!(ignored(&patterns, "deep/dir/x.log", false));
        assert!(ignored(&patterns, "build", true));
        assert!(
            !ignored(&patterns, "src/build", true),
            "anchored to the root"
        );
        assert!(!ignored(&patterns, "build", false), "directories only");
        assert!(ignored(&patterns, "docs/a/b/c.png", false));
        assert!(ignored(&patterns, "docs/c.png", false));
        assert!(!ignored(&patterns, "docs/c.jpg", false));
        assert!(ignored(&patterns, "lib/tests", true));
        assert!(!ignored(&patterns, "init.nyk", false));
    }

    #[test]
    fn collects_project_files() {
        let dir = std::env::temp_dir().join(format!("neyuki-publish-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for (path, text) in [
            (
                "neyuki.toml",
                "[package]\nname = \"@yuki/demo\"\nversion = \"1.0.0\"\nentry = \"init.nyk\"\n",
            ),
            ("init.nyk", "return {}"),
            ("lib/util.nyk", "return 1"),
            ("notes.log", "x"),
            (".git/config", "x"),
            ("neyuki_packages/a/b/init.nyk", "x"),
            ("neyuki.lock", "x"),
            ("README.md", "# Demo"),
            (".neyukiignore", "*.log\n"),
        ] {
            let target = dir.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, text).unwrap();
        }
        let files = collect(&dir).unwrap();
        let names: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            names,
            vec!["README.md", "init.nyk", "lib/util.nyk", "neyuki.toml"]
        );

        let manifest =
            PackageManifest::parse(&fs::read_to_string(dir.join("neyuki.toml")).unwrap()).unwrap();
        let meta = metadata(&manifest, &files).unwrap();
        assert_eq!(meta["name"], "@yuki/demo");
        assert_eq!(meta["readme"], "# Demo");
        let mut bad = manifest.clone();
        bad.entry = "missing.nyk".into();
        assert!(metadata(&bad, &files).unwrap_err().contains("entry file"));
        bad.name = "demo".into();
        assert!(metadata(&bad, &files).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
