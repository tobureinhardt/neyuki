//! Package names, and where packages, credentials and the registry live.
//!
//! - Project packages: `<project>/neyuki_packages/<scope>/<name>/`, loaded
//!   with `require("@pkg/<scope>/<name>")`.
//! - Global packages: `$NEYUKI_PATH`, else `~/.neyuki/packages`, as
//!   `<scope>/<name>/`, loaded with `require("@<scope>/<name>")`.
//! - Credentials: `~/.neyuki/credentials` (or `$NEYUKI_HOME`).
//! - Registry: `$NEYUKI_REGISTRY`, else `https://neyuki.cc`.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub const DEFAULT_REGISTRY: &str = "https://neyuki.cc";
pub const PACKAGES_DIR: &str = "neyuki_packages";
pub const MANIFEST: &str = "neyuki.toml";
pub const LOCKFILE: &str = "neyuki.lock";

/// `@scope/name`: the scope is the publisher's username.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackageName {
    pub scope: String,
    pub name: String,
}

fn valid_scope(scope: &str) -> bool {
    let bytes = scope.as_bytes();
    (2..=39).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
}

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
}

impl FromStr for PackageName {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        let bad = || {
            format!(
                "'{}' is not a package name; names look like @user/package \
                 (lowercase letters, digits, - and _)",
                s
            )
        };
        let rest = s.strip_prefix('@').ok_or_else(bad)?;
        let (scope, name) = rest.split_once('/').ok_or_else(bad)?;
        if !valid_scope(scope) || !valid_name(name) {
            return Err(bad());
        }
        Ok(Self {
            scope: scope.to_string(),
            name: name.to_string(),
        })
    }
}

impl fmt::Display for PackageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}/{}", self.scope, self.name)
    }
}

impl PackageName {
    /// The package's directory below a packages root.
    pub fn dir_in(&self, root: &Path) -> PathBuf {
        root.join(&self.scope).join(&self.name)
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// `$NEYUKI_HOME`, else `~/.neyuki`.
pub fn neyuki_home() -> Result<PathBuf, String> {
    if let Some(home) = env_path("NEYUKI_HOME") {
        return Ok(home);
    }
    let home = if cfg!(windows) {
        env_path("USERPROFILE").or_else(|| env_path("HOME"))
    } else {
        env_path("HOME")
    };
    home.map(|home| home.join(".neyuki"))
        .ok_or_else(|| "cannot find your home directory; set NEYUKI_HOME".to_string())
}

/// Where `neyuki pkg install -g` puts packages.
pub fn global_packages_dir() -> Result<PathBuf, String> {
    match env_path("NEYUKI_PATH") {
        Some(path) => Ok(path),
        None => Ok(neyuki_home()?.join("packages")),
    }
}

pub fn credentials_path() -> Result<PathBuf, String> {
    Ok(neyuki_home()?.join("credentials"))
}

pub fn registry_url() -> String {
    std::env::var("NEYUKI_REGISTRY")
        .ok()
        .map(|url| url.trim_end_matches('/').to_string())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string())
}

/// The nearest directory at or above `start` with a `neyuki.toml` or a
/// `neyuki_packages` directory.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let start = if start.as_os_str().is_empty() {
        Path::new(".")
    } else {
        start
    };
    let absolute = std::fs::canonicalize(start).ok()?;
    absolute
        .ancestors()
        .find(|dir| dir.join(MANIFEST).is_file() || dir.join(PACKAGES_DIR).is_dir())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names() {
        let name: PackageName = "@yuki/http-router_2".parse().unwrap();
        assert_eq!(name.scope, "yuki");
        assert_eq!(name.name, "http-router_2");
        assert_eq!(name.to_string(), "@yuki/http-router_2");
        for bad in [
            "yuki/router",
            "@yuki",
            "@Yuki/router",
            "@y/router",
            "@yuki/Router",
            "@yuki/../x",
            "@-yuki/x",
            "@yuki/_x",
            "@yuki/a/b",
        ] {
            assert!(
                bad.parse::<PackageName>().is_err(),
                "{} should be rejected",
                bad
            );
        }
    }
}
