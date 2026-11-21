//! `neyuki test`: runs `.nyk` test scripts.

use std::collections::HashSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process;
use std::time::Instant;

use crate::vm;

#[derive(Debug)]
pub struct TestResult {
    pub path: PathBuf,
    pub duration_ms: u128,
    pub success: bool,
    pub error: Option<String>,
}

pub fn run_test_file_with_options(path: &Path, options: crate::compiler::Options) -> TestResult {
    let start = Instant::now();
    let path_str = path.to_str().expect("path should be valid UTF-8");
    let res = vm::run_file_with_options(path_str, options);
    let duration_ms = start.elapsed().as_millis();

    match res {
        Ok(()) => TestResult {
            path: path.to_path_buf(),
            duration_ms,
            success: true,
            error: None,
        },
        Err(err) => TestResult {
            path: path.to_path_buf(),
            duration_ms,
            success: false,
            error: Some(err),
        },
    }
}

pub fn run_test_file(path: &Path) -> TestResult {
    run_test_file_with_options(path, crate::compiler::Options::RUN)
}

pub fn collect_test_files(tests_dir: &Path, filter: Option<&str>) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(tests_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "nyk"))
        .filter(|path| {
            if let Some(pattern) = filter {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|s| s.contains(pattern))
            } else {
                true
            }
        })
        .collect();

    files.sort();
    files
}

fn is_nyk_file(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "nyk")
}

/// Strips redundant `.` path components so `./tests/a.nyk` and `tests/a.nyk`
/// display and compare identically.
fn normalize_test_path(path: &Path) -> PathBuf {
    let cleaned: PathBuf = path
        .components()
        .filter(|c| !matches!(c, Component::CurDir))
        .collect();
    if cleaned.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        cleaned
    }
}

fn has_path_separator(arg: &str) -> bool {
    arg.contains('/') || arg.contains('\\') || arg == "." || arg == ".."
}

fn push_path_target(arg: &str, path: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if path.is_file() {
        if !is_nyk_file(path) {
            return Err(format!("'{}' is not a .nyk test file", arg));
        }
        out.push(normalize_test_path(path));
        Ok(())
    } else if path.is_dir() {
        let dir_files = collect_test_files(path, None);
        if dir_files.is_empty() {
            return Err(format!("no .nyk test files found in '{}'", arg));
        }
        out.extend(dir_files.into_iter().map(|p| normalize_test_path(&p)));
        Ok(())
    } else {
        Err(format!("test path '{}' does not exist", arg))
    }
}

/// Resolves CLI arguments passed to `neyuki test` into a sorted, deduplicated
/// list of `.nyk` test files. With no arguments, collects `tests/*.nyk`.
///
/// Bare arguments (without path separators) match as case-sensitive substring
/// filters against file names in `tests/` first, falling back to a file or
/// directory path in the current working directory. Arguments containing a
/// path separator (or `.` / `..`) are always treated as filesystem paths.
pub fn resolve_test_targets(args: &[String]) -> Result<Vec<PathBuf>, String> {
    resolve_test_targets_in(Path::new("tests"), args)
}

fn resolve_test_targets_in(default_dir: &Path, args: &[String]) -> Result<Vec<PathBuf>, String> {
    if args.is_empty() {
        if !default_dir.is_dir() {
            return Err(format!(
                "cannot read {}/: directory not found",
                default_dir.display()
            ));
        }
        return Ok(collect_test_files(default_dir, None)
            .into_iter()
            .map(|p| normalize_test_path(&p))
            .collect());
    }

    let mut files = Vec::new();
    for arg in args {
        let path = Path::new(arg);
        if has_path_separator(arg) {
            push_path_target(arg, path, &mut files)?;
            continue;
        }

        // Bare arguments prefer filtering `tests/*.nyk` so `neyuki test json`
        // still filters `tests/json.nyk` even if the working directory has a
        // file or subdirectory named `json`.
        if default_dir.is_dir() {
            let matched = collect_test_files(default_dir, Some(arg));
            if !matched.is_empty() {
                files.extend(matched.into_iter().map(|p| normalize_test_path(&p)));
                continue;
            }
        }

        if path.exists() || arg.ends_with(".nyk") {
            push_path_target(arg, path, &mut files)?;
        } else if !default_dir.is_dir() {
            return Err(format!(
                "cannot read {}/ to match filter '{}'",
                default_dir.display(),
                arg
            ));
        } else {
            return Err(format!("no test files matched '{}'", arg));
        }
    }

    files.sort();
    let mut seen = HashSet::with_capacity(files.len());
    files.retain(|p| {
        let key = fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        seen.insert(key)
    });
    Ok(files)
}

fn format_run_header(files: &[PathBuf]) -> String {
    let default_dir = Path::new("tests");
    if files.iter().all(|p| p.parent() == Some(default_dir)) {
        format!(
            "Running {} integration test file(s) from tests/:",
            files.len()
        )
    } else {
        format!("Running {} integration test file(s):", files.len())
    }
}

pub fn run_cli(args: &[String]) -> Result<(), String> {
    let files = resolve_test_targets(args)?;

    if files.is_empty() {
        println!("No .nyk test files found in tests/");
        return Ok(());
    }

    println!("{}", format_run_header(&files));
    let suite_start = Instant::now();
    let mut passed = 0;
    let mut failed = 0;

    for path in &files {
        let result = run_test_file(path);
        let name = path.display();
        if result.success {
            passed += 1;
            println!("  test {:<40} ... ok ({}ms)", name, result.duration_ms);
        } else {
            failed += 1;
            println!("  test {:<40} ... FAILED", name);
            if let Some(err) = &result.error {
                eprintln!("    --> error: {}", err);
            }
        }
    }

    let elapsed = suite_start.elapsed().as_secs_f64();
    println!(
        "\nTest result: {}. {} passed; {} failed; finished in {:.2}s\n",
        if failed == 0 { "ok" } else { "FAILED" },
        passed,
        failed,
        elapsed
    );

    if failed > 0 {
        process::exit(1);
    }
    Ok(())
}

pub fn run_all_tests() {
    if let Err(err) = run_cli(&[]) {
        eprintln!("{}", err);
        process::exit(1);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;

    #[test]
    fn test_all_nyk_integration_files() {
        let tests_dir = Path::new("tests");
        let files = collect_test_files(tests_dir, None);
        assert!(!files.is_empty(), "expected tests/*.nyk files to exist");

        for path in &files {
            let result = run_test_file(path);
            assert!(
                result.success,
                "Test script '{}' failed: {:?}",
                path.display(),
                result.error
            );
        }

        for path in &files {
            let result = run_test_file_with_options(path, crate::compiler::Options::DEBUG);
            assert!(
                result.success,
                "Test script '{}' (unoptimized) failed: {:?}",
                path.display(),
                result.error
            );
        }
    }

    #[test]
    fn test_cargo_fmt_check() {
        let output = std::process::Command::new("cargo")
            .args(["fmt", "--all", "--", "--check"])
            .output();
        if let Ok(out) = output {
            assert!(
                out.status.success(),
                "cargo fmt --all -- --check failed:\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    #[test]
    fn test_collect_and_resolve_test_targets() {
        // Missing directory returns empty Vec instead of panicking.
        assert!(collect_test_files(Path::new("nonexistent_tests_dir_xyz"), None).is_empty());

        // Filter by substring takes precedence over a same-named directory/file
        // when no path separator is given (e.g. `src` exists as a directory,
        // while `json` matches `tests/json.nyk`).
        let filtered = resolve_test_targets(&["crypto".to_string()]).expect("filter match");
        assert_eq!(filtered, vec![PathBuf::from("tests/crypto.nyk")]);

        // Explicit file path and `./` prefix deduplicate to a single run.
        let by_file =
            resolve_test_targets(&["tests/json.nyk".to_string(), "./tests/json.nyk".to_string()])
                .expect("file target");
        assert_eq!(by_file, vec![PathBuf::from("tests/json.nyk")]);
        assert_eq!(
            format_run_header(&by_file),
            "Running 1 integration test file(s) from tests/:"
        );

        // Targets outside `tests/` omit "from tests/:" in the run header.
        let outside = vec![PathBuf::from("examples/hello.nyk")];
        assert_eq!(
            format_run_header(&outside),
            "Running 1 integration test file(s):"
        );

        // Non-.nyk file is rejected even when passed explicitly.
        let non_nyk = resolve_test_targets(&["Cargo.toml".to_string()]).unwrap_err();
        assert!(
            non_nyk.contains("is not a .nyk test file"),
            "unexpected error: {non_nyk}"
        );

        // Typo'd path with `/` or `.nyk` reports that the path does not exist
        // rather than falling through to filter matching.
        let bad_path = resolve_test_targets(&["tests/typo_missing.nyk".to_string()]).unwrap_err();
        assert!(
            bad_path.contains("does not exist"),
            "unexpected error: {bad_path}"
        );
        let bad_nyk = resolve_test_targets(&["typo_missing.nyk".to_string()]).unwrap_err();
        assert!(
            bad_nyk.contains("does not exist"),
            "unexpected error: {bad_nyk}"
        );

        // If a bare name matches both a filter in `tests/` and an existing file
        // or directory in CWD, filter matching wins unless a path separator is used.
        let tmp = std::env::temp_dir().join(format!("neyuki_test_cli_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let tmp_tests = tmp.join("tests");
        let colliding_dir = tmp.join("json");
        fs::create_dir_all(&tmp_tests).unwrap();
        fs::create_dir_all(&colliding_dir).unwrap();
        let test_src = tmp_tests.join("src.nyk");
        let dir_other = colliding_dir.join("other.nyk");
        fs::write(&test_src, "return 1\n").unwrap();
        fs::write(&dir_other, "return 2\n").unwrap();

        // Even though `src` exists as a directory in the working directory,
        // a bare `"src"` argument matches `src.nyk` in `tests/` first.
        let filter_wins =
            resolve_test_targets_in(&tmp_tests, &["src".to_string()]).expect("filter wins");
        assert_eq!(filter_wins, vec![normalize_test_path(&test_src)]);

        let explicit_dir =
            resolve_test_targets_in(&tmp_tests, &[colliding_dir.to_string_lossy().into()])
                .expect("explicit path with separator runs dir");
        assert_eq!(explicit_dir, vec![normalize_test_path(&dir_other)]);

        // Unmatched filter returns a clean error.
        let err = resolve_test_targets(&["no_such_test_pattern_xyz".to_string()]).unwrap_err();
        assert!(err.contains("no test files matched"));

        let _ = fs::remove_dir_all(&tmp);
    }
}
