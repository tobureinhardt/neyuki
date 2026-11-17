use std::env;
use std::process;

#[path = "../bench/mod.rs"]
mod bench;
#[path = "../fuzz/mod.rs"]
mod fuzz;

use neyuki::*;

// Opt-in allocator; see the `mimalloc` entry in Cargo.toml.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Stack for the thread that runs every action. The parser caps nesting, but
/// the passes after it recurse once per level with large debug-build frames,
/// and the main thread only gets 1 MiB on Windows. This only reserves address
/// space; pages are committed as they are touched.
const MAIN_STACK_SIZE: usize = 256 * 1024 * 1024;

fn main() {
    let worker = std::thread::Builder::new()
        .name("main".to_string())
        .stack_size(MAIN_STACK_SIZE)
        .spawn(run_cli)
        .unwrap_or_else(|err| {
            eprintln!("failed to start the main thread: {}", err);
            process::exit(1);
        });
    if worker.join().is_err() {
        process::exit(101);
    }
}

const USAGE: &str = "Usage: neyuki <command> [arguments]

Commands:
  run [options] <file> [args...]
                          Run a .nyk script or .nykb bytecode file
    --sandbox             Deny all host capabilities unless allowed
    --allow-all, -A       Allow all host capabilities (default without --sandbox)
    --allow-read[=PATHS]  Allow filesystem read access
    --allow-write[=PATHS] Allow filesystem write access
    --allow-net[=HOSTS]   Allow network access
    --allow-run[=CMDS]    Allow subprocess execution
    --allow-env[=VARS]    Allow environment variable access
    --max-instructions=N  Stop after executing N VM instructions
    --max-memory=BYTES    Cap live GC heap (supports K, M, G suffixes)
    --timeout=DUR         Stop after duration (e.g. 500ms, 5s, 1m)
  <file> [args...]        Same as run
  fmt [--check] <path>    Format .nyk source files
  lint [--json] [--stdin] <file>
                          Report errors and warnings without running
  lsp                     Serve the Language Server Protocol on stdio
  compile <file> [out]    Compile to .nykb bytecode
  build [project]         Build a standalone app executable
  test                    Run every script in tests/
  repl                    Start an interactive session
  pkg <command>           Manage packages
  dap                     Serve the Debug Adapter Protocol on stdio
  disasm <file>           Print the bytecode the compiler emits
  dump-ir <file>          Print the IR before and after optimization
  assemble <file> [out]   Assemble bytecode from its text form
  bench                   Run the built-in benchmarks
  fuzz [target] [--seconds N]
                          Run the fuzzers' regression cases, or fuzz a
                          target (file, tree, vm, libs, memory, all)
  help, version";

fn run_cli() {
    // An executable written by `neyuki build` runs its app, never the CLI.
    match app::find_own_bundle() {
        Ok(Some(bundle)) => return run_app(bundle),
        Ok(None) => {}
        Err(err) => {
            app::report_error(&format!("error: {}", err));
            process::exit(1);
        }
    }

    let mut args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("{}", USAGE);
        process::exit(1);
    }
    // `neyuki script.nyk ...` or `neyuki --sandbox script.nyk ...` runs the script.
    if args[1].ends_with(".nyk")
        || args[1].ends_with(".nykb")
        || args[1] == "--sandbox"
        || args[1] == "-A"
        || args[1].starts_with("--allow-")
        || args[1].starts_with("--max-")
        || args[1].starts_with("--instruction-budget")
        || args[1].starts_with("--heap-limit")
        || args[1].starts_with("--timeout")
    {
        args.insert(1, "run".to_string());
    }

    let action = &args[1];

    if matches!(action.as_str(), "help" | "--help" | "-h") {
        println!("{}", USAGE);
    } else if matches!(action.as_str(), "version" | "--version" | "-V") {
        println!("neyuki {}", env!("CARGO_PKG_VERSION"));
    } else if action == "fmt" {
        if let Err(err) = fmt::run_cli(&args[2..]) {
            eprintln!("{}", err);
            process::exit(1);
        }
    } else if action == "lint" {
        if let Err(err) = lint::run_cli(&args[2..]) {
            eprintln!("{}", err);
            process::exit(1);
        }
    } else if action == "lsp" {
        if let Err(err) = lsp::run_lsp_server() {
            eprintln!("lsp error: {}", err);
            process::exit(1);
        }
    } else if action == "compile" {
        let Some(path) = args.get(2) else {
            eprintln!("Usage: {} compile <path> [output]", args[0]);
            process::exit(1);
        };
        match compiler::compile_file_to_bytecode(path) {
            Ok(bytecode) => {
                let out_path = args.get(3).cloned().unwrap_or_else(|| format!("{}b", path));
                if let Err(err) = std::fs::write(&out_path, &bytecode) {
                    eprintln!("failed to write bytecode to {}: {}", out_path, err);
                    process::exit(1);
                }
                println!(
                    "compiled {} -> {} ({} bytes)",
                    path,
                    out_path,
                    bytecode.len()
                );
            }
            Err(err) => {
                if err.starts_with("error[") || err.starts_with("runtime error") {
                    eprint!("{}", err);
                    if !err.ends_with('\n') {
                        eprintln!();
                    }
                } else {
                    eprintln!("{}: {}", path, err);
                }
                process::exit(1);
            }
        }
    } else if action == "build" {
        if let Err(err) = app::build::run_cli(&args[2..]) {
            if err.starts_with("error[") {
                eprint!("{}", err);
            } else {
                eprintln!("build error: {}", err);
            }
            process::exit(1);
        }
    } else if action == "run" {
        let (path, script_args, limits, permissions) = match parse_run_args(&args[2..]) {
            Ok(parsed) => parsed,
            Err(err) => {
                eprintln!("{}", err);
                process::exit(1);
            }
        };
        vm::libs::os::set_script_args(script_args);
        if let Err(err) =
            vm::run_file_with_config(&path, compiler::Options::RUN, limits, permissions)
        {
            if err.starts_with("error[") || err.starts_with("runtime error") {
                eprint!("{}", err);
                if !err.ends_with('\n') {
                    eprintln!();
                }
            } else {
                eprintln!("error: {}", err);
            }
            process::exit(1);
        }
    } else if action == "dump-ir" {
        // Prints the IR of every function before and after optimization.
        let Some(path) = args.get(2) else {
            eprintln!("Usage: {} dump-ir <path>", args[0]);
            process::exit(1);
        };
        match compiler::compile_file(path) {
            Ok(stmts) => print!("{}", compiler::dump_ir(&stmts)),
            Err(err) => {
                eprintln!("{}: {}", path, err);
                process::exit(1);
            }
        }
    } else if action == "disasm" {
        let Some(path) = args.get(2) else {
            eprintln!("Usage: {} disasm <path>", args[0]);
            process::exit(1);
        };
        // A debugging aid: shows what the compiler emits even for sources
        // the type checker would reject.
        let compiled = std::fs::read_to_string(path)
            .map_err(|err| format!("failed to read {}: {}", path, err))
            .and_then(|source| {
                compiler::compile_program(&source, path, compiler::Options::UNCHECKED)
            });
        match compiled {
            Ok(proto) => print!("{}", bytecode::disasm::disassemble_proto(&proto, 0)),
            Err(err) => {
                eprintln!("{}: {}", path, err);
                process::exit(1);
            }
        }
    } else if action == "assemble" {
        let Some(path) = args.get(2) else {
            eprintln!("Usage: {} assemble <assembly_file> [output]", args[0]);
            process::exit(1);
        };
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("failed to read {}: {}", path, e);
                process::exit(1);
            }
        };
        let proto = match bytecode::parse_assembly(&text) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("assembly parse error: {}", e);
                process::exit(1);
            }
        };
        let out_path = args
            .get(3)
            .cloned()
            .unwrap_or_else(|| format!("{}.bin", path));
        let serialized = bytecode::serialize(&proto);
        if let Err(e) = std::fs::write(&out_path, &serialized) {
            eprintln!("failed to write {}: {}", out_path, e);
            process::exit(1);
        }
        println!(
            "assembled {} -> {} ({} bytes)",
            path,
            out_path,
            serialized.len()
        );
    } else if action == "test" {
        tests::run_all_tests();
    } else if action == "fuzz" {
        if let Err(err) = fuzz::driver::run_cli(&args[2..]) {
            eprintln!("{}", err);
            process::exit(1);
        }
    } else if action == "bench" {
        bench::run_all_benchmarks();
    } else if action == "repl" {
        if let Err(err) = repl::run_repl() {
            eprintln!("repl error: {}", err);
            process::exit(1);
        }
    } else if action == "dap" {
        if let Err(err) = dap::run_dap_session() {
            eprintln!("dap error: {}", err);
            process::exit(1);
        }
    } else if action == "pkg" {
        if let Err(err) = pkg::run_pkg_cli(&args[2..]) {
            eprintln!("pkg error: {}", err);
            process::exit(1);
        }
    } else {
        eprintln!(
            "Unknown command: {}

{}",
            action, USAGE
        );
        process::exit(1);
    }
}

/// Runs the app bundled into this executable with every argument passed to
/// the script.
fn run_app(bundle: app::Bundle) {
    app::install(Some(bundle));
    vm::libs::os::set_script_args(env::args().skip(1).collect());
    if let Err(err) = app::run_installed() {
        let message = if err.starts_with("error[") || err.starts_with("runtime error") {
            err.trim_end().to_string()
        } else {
            format!("error: {}", err)
        };
        app::report_error(&message);
        process::exit(1);
    }
}

fn parse_scope(value: Option<&str>) -> vm::PermissionScope {
    match value {
        None | Some("") => vm::PermissionScope::AllowAll,
        Some(list) => vm::PermissionScope::AllowList(
            list.split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect(),
        ),
    }
}

fn parse_size(raw: &str) -> Result<usize, String> {
    let raw = raw.trim();
    let (num_part, mult) = if let Some(s) = raw.strip_suffix(['k', 'K']) {
        (s, 1024usize)
    } else if let Some(s) = raw.strip_suffix(['m', 'M']) {
        (s, 1024 * 1024)
    } else if let Some(s) = raw.strip_suffix(['g', 'G']) {
        (s, 1024 * 1024 * 1024)
    } else {
        (raw, 1)
    };
    let n: usize = num_part
        .parse()
        .map_err(|_| format!("invalid memory size '{}'", raw))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("memory size '{}' overflows usize", raw))
}

fn parse_duration(raw: &str) -> Result<std::time::Duration, String> {
    let raw = raw.trim();
    if let Some(ms) = raw.strip_suffix("ms") {
        let n: u64 = ms
            .parse()
            .map_err(|_| format!("invalid duration '{}'", raw))?;
        Ok(std::time::Duration::from_millis(n))
    } else if let Some(s) = raw.strip_suffix('s') {
        let n: f64 = s
            .parse()
            .map_err(|_| format!("invalid duration '{}'", raw))?;
        if n < 0.0 || !n.is_finite() {
            return Err(format!("invalid duration '{}'", raw));
        }
        Ok(std::time::Duration::from_secs_f64(n))
    } else if let Some(m) = raw.strip_suffix('m') {
        let n: f64 = m
            .parse()
            .map_err(|_| format!("invalid duration '{}'", raw))?;
        if n < 0.0 || !n.is_finite() {
            return Err(format!("invalid duration '{}'", raw));
        }
        Ok(std::time::Duration::from_secs_f64(n * 60.0))
    } else {
        let n: f64 = raw
            .parse()
            .map_err(|_| format!("invalid duration '{}'", raw))?;
        if n < 0.0 || !n.is_finite() {
            return Err(format!("invalid duration '{}'", raw));
        }
        Ok(std::time::Duration::from_secs_f64(n))
    }
}

fn parse_run_args(
    args: &[String],
) -> Result<(String, Vec<String>, vm::ExecutionLimits, vm::Permissions), String> {
    let mut sandboxed = false;
    let mut allow_all = false;
    let mut read_scope: Option<vm::PermissionScope> = None;
    let mut write_scope: Option<vm::PermissionScope> = None;
    let mut net_scope: Option<vm::PermissionScope> = None;
    let mut run_scope: Option<vm::PermissionScope> = None;
    let mut env_scope: Option<vm::PermissionScope> = None;
    let mut limits = vm::ExecutionLimits::new();

    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if arg == "--" {
            idx += 1;
            break;
        }
        if !arg.starts_with('-') {
            break;
        }
        if arg == "--sandbox" {
            sandboxed = true;
        } else if arg == "--allow-all" || arg == "-A" {
            allow_all = true;
        } else if let Some(rest) = arg.strip_prefix("--allow-read") {
            let val = rest.strip_prefix('=');
            if rest.is_empty() || val.is_some() {
                sandboxed = true;
                read_scope = Some(parse_scope(val));
            } else {
                return Err(format!("unknown flag '{}'", arg));
            }
        } else if let Some(rest) = arg.strip_prefix("--allow-write") {
            let val = rest.strip_prefix('=');
            if rest.is_empty() || val.is_some() {
                sandboxed = true;
                write_scope = Some(parse_scope(val));
            } else {
                return Err(format!("unknown flag '{}'", arg));
            }
        } else if let Some(rest) = arg.strip_prefix("--allow-net") {
            let val = rest.strip_prefix('=');
            if rest.is_empty() || val.is_some() {
                sandboxed = true;
                net_scope = Some(parse_scope(val));
            } else {
                return Err(format!("unknown flag '{}'", arg));
            }
        } else if let Some(rest) = arg.strip_prefix("--allow-run") {
            let val = rest.strip_prefix('=');
            if rest.is_empty() || val.is_some() {
                sandboxed = true;
                run_scope = Some(parse_scope(val));
            } else {
                return Err(format!("unknown flag '{}'", arg));
            }
        } else if let Some(rest) = arg.strip_prefix("--allow-env") {
            let val = rest.strip_prefix('=');
            if rest.is_empty() || val.is_some() {
                sandboxed = true;
                env_scope = Some(parse_scope(val));
            } else {
                return Err(format!("unknown flag '{}'", arg));
            }
        } else if let Some(raw) = arg
            .strip_prefix("--max-instructions=")
            .or_else(|| arg.strip_prefix("--instruction-budget="))
        {
            let budget: u64 = raw
                .parse()
                .map_err(|_| format!("invalid instruction budget '{}'", raw))?;
            limits = limits.with_instruction_budget(budget);
        } else if let Some(raw) = arg
            .strip_prefix("--max-memory=")
            .or_else(|| arg.strip_prefix("--heap-limit="))
        {
            limits = limits.with_heap_limit(parse_size(raw)?);
        } else if let Some(raw) = arg.strip_prefix("--timeout=") {
            limits = limits.with_timeout(parse_duration(raw)?);
        } else {
            return Err(format!("unknown option '{}' for run", arg));
        }
        idx += 1;
    }

    let path = args
        .get(idx)
        .cloned()
        .ok_or_else(|| "Usage: neyuki run [options] <path> [args...]".to_string())?;
    let script_args = args[idx + 1..].to_vec();

    let permissions = if allow_all || !sandboxed {
        vm::Permissions::allow_all()
    } else {
        let mut perms = vm::Permissions::deny_all();
        if let Some(s) = read_scope {
            perms = perms.with_read(s);
        }
        if let Some(s) = write_scope {
            perms = perms.with_write(s);
        }
        if let Some(s) = net_scope {
            perms = perms.with_net(s);
        }
        if let Some(s) = run_scope {
            perms = perms.with_run(s);
        }
        if let Some(s) = env_scope {
            perms = perms.with_env(s);
        }
        perms
    };

    Ok((path, script_args, limits, permissions))
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn test_parse_run_args_defaults_unrestricted() {
        let args = vec!["script.nyk".into(), "arg1".into()];
        let (path, script_args, limits, perms) = parse_run_args(&args).unwrap();
        assert_eq!(path, "script.nyk");
        assert_eq!(script_args, vec!["arg1"]);
        assert!(!limits.is_active());
        assert_eq!(perms, vm::Permissions::allow_all());
    }

    #[test]
    fn test_parse_run_args_sandbox_and_limits() {
        let args = vec![
            "--sandbox".into(),
            "--allow-net=api.example.com,localhost:8080".into(),
            "--max-instructions=50000".into(),
            "--max-memory=16M".into(),
            "--timeout=250ms".into(),
            "plugin.nyk".into(),
            "--flag".into(),
        ];
        let (path, script_args, limits, perms) = parse_run_args(&args).unwrap();
        assert_eq!(path, "plugin.nyk");
        assert_eq!(script_args, vec!["--flag"]);
        assert_eq!(limits.instruction_budget, Some(50000));
        assert_eq!(limits.heap_limit_bytes, Some(16 * 1024 * 1024));
        assert!(limits.deadline.is_some());
        assert_eq!(perms.read, vm::PermissionScope::DenyAll);
        assert!(perms.check_net("https://api.example.com/v1").is_ok());
        assert!(perms.check_net("https://evil.com").is_err());
    }
}
