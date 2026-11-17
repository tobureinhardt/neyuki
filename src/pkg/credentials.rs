//! API tokens saved by `neyuki pkg login`, one per registry, in
//! `~/.neyuki/credentials`:
//!
//! ```toml
//! [tokens]
//! "https://neyuki.cc" = "nyk_..."
//! ```
//!
//! `NEYUKI_TOKEN` overrides the file, for CI.

use std::collections::BTreeMap;
use std::fs;

use super::paths::credentials_path;

fn read_all() -> BTreeMap<String, String> {
    let Ok(path) = credentials_path() else {
        return BTreeMap::new();
    };
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            let key = key.trim().trim_matches('"');
            let value = value.trim().trim_matches('"');
            (key.starts_with("http") && !value.is_empty())
                .then(|| (key.to_string(), value.to_string()))
        })
        .collect()
}

fn write_all(tokens: &BTreeMap<String, String>) -> Result<(), String> {
    let path = credentials_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {}", dir.display(), e))?;
    }
    let mut text =
        String::from("# Written by `neyuki pkg login`. Keep this file private.\n[tokens]\n");
    for (registry, token) in tokens {
        text.push_str(&format!("\"{}\" = \"{}\"\n", registry, token));
    }
    fs::write(&path, text).map_err(|e| format!("cannot write {}: {}", path.display(), e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn token_for(registry: &str) -> Option<String> {
    if let Ok(token) = std::env::var("NEYUKI_TOKEN")
        && !token.trim().is_empty()
    {
        return Some(token.trim().to_string());
    }
    read_all().remove(registry)
}

pub fn save(registry: &str, token: &str) -> Result<(), String> {
    let mut all = read_all();
    all.insert(registry.to_string(), token.to_string());
    write_all(&all)
}

/// Whether a token was saved (and is now removed).
pub fn remove(registry: &str) -> Result<bool, String> {
    let mut all = read_all();
    let had = all.remove(registry).is_some();
    if had {
        write_all(&all)?;
    }
    Ok(had)
}
