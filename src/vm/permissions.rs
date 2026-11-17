//! Capability-based sandboxing permissions for filesystem, network,
//! subprocess execution, and environment access.

use std::path::{Component, Path, PathBuf};

/// Access policy for a capability category.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionScope {
    DenyAll,
    AllowAll,
    AllowList(Vec<String>),
}

/// Runtime permission set enforced by host natives (`fs`, `http`, `net`,
/// `sql`, `os`, `require`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permissions {
    pub read: PermissionScope,
    pub write: PermissionScope,
    pub net: PermissionScope,
    pub run: PermissionScope,
    pub env: PermissionScope,
}

impl Default for Permissions {
    fn default() -> Self {
        Self::allow_all()
    }
}

impl Permissions {
    /// Unrestricted permissions (default for standard CLI execution).
    pub fn allow_all() -> Self {
        Self {
            read: PermissionScope::AllowAll,
            write: PermissionScope::AllowAll,
            net: PermissionScope::AllowAll,
            run: PermissionScope::AllowAll,
            env: PermissionScope::AllowAll,
        }
    }

    /// Fully sandboxed permissions (all host capabilities denied).
    pub fn deny_all() -> Self {
        Self {
            read: PermissionScope::DenyAll,
            write: PermissionScope::DenyAll,
            net: PermissionScope::DenyAll,
            run: PermissionScope::DenyAll,
            env: PermissionScope::DenyAll,
        }
    }

    pub fn with_read(mut self, scope: PermissionScope) -> Self {
        self.read = scope;
        self
    }

    pub fn with_write(mut self, scope: PermissionScope) -> Self {
        self.write = scope;
        self
    }

    pub fn with_net(mut self, scope: PermissionScope) -> Self {
        self.net = scope;
        self
    }

    pub fn with_run(mut self, scope: PermissionScope) -> Self {
        self.run = scope;
        self
    }

    pub fn with_env(mut self, scope: PermissionScope) -> Self {
        self.env = scope;
        self
    }

    /// Checks whether reading `path` is permitted.
    pub fn check_read(&self, path: &str) -> Result<(), String> {
        check_path_scope(&self.read, path)
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "permission denied: read access to '{}', run with --allow-read",
                    path
                )
            })
    }

    /// Checks whether writing or modifying `path` is permitted.
    pub fn check_write(&self, path: &str) -> Result<(), String> {
        check_path_scope(&self.write, path)
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "permission denied: write access to '{}', run with --allow-write",
                    path
                )
            })
    }

    /// Checks whether network access to `target` (URL, `host:port`, or host)
    /// is permitted.
    pub fn check_net(&self, target: &str) -> Result<(), String> {
        check_net_scope(&self.net, target)
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "permission denied: network access to '{}', run with --allow-net",
                    target
                )
            })
    }

    /// Checks whether spawning subprocess `cmd` is permitted.
    pub fn check_run(&self, cmd: &str) -> Result<(), String> {
        check_run_scope(&self.run, cmd)
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "permission denied: subprocess execution of '{}', run with --allow-run",
                    cmd
                )
            })
    }

    /// Checks whether reading environment variable `var` is permitted.
    pub fn check_env(&self, var: &str) -> Result<(), String> {
        check_env_scope(&self.env, var)
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "permission denied: environment access to '{}', run with --allow-env",
                    var
                )
            })
    }
}

/// Normalizes a path lexically against `current_dir` so `..` segments cannot
/// escape an allowed directory prefix, even when the target does not exist yet.
fn normalize_abs(raw: &str) -> PathBuf {
    let p = Path::new(raw);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(p)
    } else {
        p.to_path_buf()
    };
    let mut out = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    out.components().next_back(),
                    Some(Component::RootDir | Component::Prefix(_))
                ) {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out
}

fn check_path_scope(scope: &PermissionScope, target: &str) -> bool {
    match scope {
        PermissionScope::AllowAll => true,
        PermissionScope::DenyAll => false,
        PermissionScope::AllowList(allowed) => {
            let target_norm = normalize_abs(target);
            allowed.iter().any(|prefix| {
                let prefix_norm = normalize_abs(prefix);
                target_norm.starts_with(&prefix_norm)
            })
        }
    }
}

/// Extracts `(host, optional_port)` from a URL or `host[:port]` string.
fn extract_host_port(target: &str) -> (&str, Option<&str>) {
    let without_scheme = target
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(target);
    let authority = without_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(without_scheme);
    let authority = authority
        .rsplit_once('@')
        .map(|(_, host)| host)
        .unwrap_or(authority);
    if let Some(rest) = authority.strip_prefix('[')
        && let Some((ipv6, after)) = rest.split_once(']')
    {
        let port = after.strip_prefix(':');
        return (ipv6, port);
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h, Some(p)),
        _ => (authority, None),
    }
}

fn check_net_scope(scope: &PermissionScope, target: &str) -> bool {
    match scope {
        PermissionScope::AllowAll => true,
        PermissionScope::DenyAll => false,
        PermissionScope::AllowList(allowed) => {
            let (t_host, t_port) = extract_host_port(target);
            let t_host_lower = t_host.to_ascii_lowercase();
            allowed.iter().any(|rule| {
                let (r_host, r_port) = extract_host_port(rule);
                if let Some(rp) = r_port
                    && t_port != Some(rp)
                {
                    return false;
                }
                let r_host_lower = r_host.to_ascii_lowercase();
                if let Some(suffix) = r_host_lower.strip_prefix("*.") {
                    t_host_lower == suffix || t_host_lower.ends_with(&format!(".{}", suffix))
                } else {
                    t_host_lower == r_host_lower
                }
            })
        }
    }
}

fn check_run_scope(scope: &PermissionScope, cmd: &str) -> bool {
    match scope {
        PermissionScope::AllowAll => true,
        PermissionScope::DenyAll => false,
        PermissionScope::AllowList(allowed) => {
            // For shell command strings like `echo hi` or `/bin/ls -la`, take the executable token.
            let exe = cmd.split_whitespace().next().unwrap_or(cmd);
            let exe_name = Path::new(exe)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(exe);
            allowed.iter().any(|item| {
                item == exe
                    || item == exe_name
                    || Path::new(item).file_name().and_then(|n| n.to_str()) == Some(exe_name)
            })
        }
    }
}

fn check_env_scope(scope: &PermissionScope, var: &str) -> bool {
    match scope {
        PermissionScope::AllowAll => true,
        PermissionScope::DenyAll => false,
        PermissionScope::AllowList(allowed) => allowed.iter().any(|item| {
            if cfg!(windows) {
                item.eq_ignore_ascii_case(var)
            } else {
                item == var
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deny_all_blocks_everything() {
        let perms = Permissions::deny_all();
        assert!(perms.check_read("foo.txt").is_err());
        assert!(perms.check_write("foo.txt").is_err());
        assert!(perms.check_net("https://example.com").is_err());
        assert!(perms.check_run("ls").is_err());
        assert!(perms.check_env("HOME").is_err());
    }

    #[test]
    fn test_read_allow_list_blocks_traversal() {
        let perms =
            Permissions::deny_all().with_read(PermissionScope::AllowList(vec!["/tmp/safe".into()]));
        assert!(perms.check_read("/tmp/safe/data.txt").is_ok());
        assert!(perms.check_read("/tmp/safe/../etc/passwd").is_err());
        assert!(perms.check_read("/etc/passwd").is_err());
    }

    #[test]
    fn test_net_allow_list_matches_hosts_ports_and_wildcards() {
        let perms = Permissions::deny_all().with_net(PermissionScope::AllowList(vec![
            "api.example.com".into(),
            "localhost:8080".into(),
            "*.trusted.org".into(),
        ]));
        assert!(perms.check_net("https://api.example.com/v1/users").is_ok());
        assert!(perms.check_net("localhost:8080").is_ok());
        assert!(perms.check_net("localhost:9090").is_err());
        assert!(perms.check_net("https://sub.trusted.org/data").is_ok());
        assert!(perms.check_net("https://evil.com").is_err());
    }

    #[test]
    fn test_run_and_env_allow_lists() {
        let perms = Permissions::deny_all()
            .with_run(PermissionScope::AllowList(vec!["echo".into()]))
            .with_env(PermissionScope::AllowList(vec!["PATH".into()]));
        assert!(perms.check_run("echo hello").is_ok());
        assert!(perms.check_run("rm -rf /").is_err());
        assert!(perms.check_env("PATH").is_ok());
        assert!(perms.check_env("SECRET_KEY").is_err());
    }
}
