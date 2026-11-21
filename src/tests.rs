//! `neyuki test`: runs `.nyk` test scripts.

use std::fs;
use std::path::{Path, PathBuf};
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

/// Resolves CLI arguments passed to `neyuki test` into a sorted, deduplicated
/// list of `.nyk` test files. With no arguments, collects `tests/*.nyk`. Each
/// argument may be a `.nyk` file path, a directory containing `.nyk` files, or
/// a substring filter matched against `tests/*.nyk`.
pub fn resolve_test_targets(args: &[String]) -> Result<Vec<PathBuf>, String> {
    let default_dir = Path::new("tests");
    if args.is_empty() {
        if !default_dir.exists() {
            return Err("cannot read tests/: directory not found".to_string());
        }
        return Ok(collect_test_files(default_dir, None));
    }

    let mut files = Vec::new();
    for arg in args {
        let path = Path::new(arg);
        if path.is_file() {
            files.push(path.to_path_buf());
        } else if path.is_dir() {
            let dir_files = collect_test_files(path, None);
            if dir_files.is_empty() {
                return Err(format!("no .nyk test files found in '{}'", arg));
            }
            files.extend(dir_files);
        } else {
            if !default_dir.exists() {
                return Err(format!("cannot read tests/ to match filter '{}'", arg));
            }
            let matched = collect_test_files(default_dir, Some(arg));
            if matched.is_empty() {
                return Err(format!("no test files matched '{}'", arg));
            }
            files.extend(matched);
        }
    }

    files.sort();
    files.dedup();
    Ok(files)
}

pub fn run_cli(args: &[String]) -> Result<(), String> {
    let files = resolve_test_targets(args)?;

    if files.is_empty() {
        println!("No .nyk test files found in tests/");
        return Ok(());
    }

    println!(
        "Running {} integration test file(s) from tests/:",
        files.len()
    );
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

        // Filter by substring.
        let filtered = resolve_test_targets(&["crypto".to_string()]).expect("filter match");
        assert_eq!(filtered, vec![PathBuf::from("tests/crypto.nyk")]);

        // Explicit file path.
        let by_file = resolve_test_targets(&["tests/json.nyk".to_string()]).expect("file target");
        assert_eq!(by_file, vec![PathBuf::from("tests/json.nyk")]);

        // Unmatched filter returns a clean error.
        let err = resolve_test_targets(&["no_such_test_pattern_xyz".to_string()]).unwrap_err();
        assert!(err.contains("no test files matched"));
    }
}
