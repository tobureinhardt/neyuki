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
  run <file> [args...]    Run a .nyk script or .nykb bytecode file
  <file> [args...]        Same as run
  fmt [--check] <path>    Format .nyk source files
  lint [--json] [--stdin] <file>
                          Report errors and warnings without running
  lsp                     Serve the Language Server Protocol on stdio
  compile <file> [out]    Compile to .nykb bytecode
  build [project]         Build a standalone app executable
  test [filter|path...]   Run .nyk test scripts (default: tests/)
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
    // `neyuki script.nyk ...` runs the script.
    if args[1].ends_with(".nyk") || args[1].ends_with(".nykb") {
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
        let Some(path) = args.get(2) else {
            eprintln!("Usage: {} run <path> [args...]", args[0]);
            process::exit(1);
        };
        vm::libs::os::set_script_args(args[3..].to_vec());
        if let Err(err) = vm::run_file(path) {
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
        if let Err(err) = tests::run_cli(&args[2..]) {
            eprintln!("{}", err);
            process::exit(1);
        }
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
