//! Source to bytecode: parse, check, fold constants, lower to SSA IR,
//! optimize, allocate registers and emit a `Proto`.

pub mod builtins_folding;
pub mod constant_fold;
pub mod cost_model;
pub mod hir;
pub mod ir;
pub mod value_tracking;

use crate::ast::node_id::NodeId;
use crate::bytecode::proto::Proto;
use crate::bytecode::serialize::serialize;
use crate::parser::{Parser, Stmt};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::rc::Rc;

pub use constant_fold::fold_program;
pub use value_tracking::{TrackedValue, ValueTracker, value_tracking_cfg};

/// A parsed program together with the line each statement starts on.
pub struct Parsed {
    pub stmts: Vec<Stmt>,
    pub lines: Rc<HashMap<NodeId, u32>>,
    pub spans: crate::ast::span::SpanPool,
}

/// Compiler optimization level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OptLevel {
    /// Unoptimized: skips AST constant folding, inlining, and optimization passes.
    O0,
    /// Standard optimizations: AST folding, inlining (when enabled), and full CFG opt passes.
    #[default]
    O1,
}

/// How a program is compiled.
#[derive(Clone, Copy)]
pub struct Options {
    /// Run the semantic checks first.
    pub check: bool,
    /// Inline small functions.
    pub inline: bool,
    /// Keep locals in their registers until their scope ends, so a debugger
    /// can read them after their last use.
    pub keep_locals: bool,
    /// Optimization level (`-O0` or `-O1`).
    pub opt_level: OptLevel,
}

impl Options {
    /// For running user code.
    pub const RUN: Options = Options {
        check: true,
        inline: true,
        keep_locals: false,
        opt_level: OptLevel::O1,
    };
    /// For the debugger: every call keeps its frame, every local its value.
    pub const DEBUG: Options = Options {
        check: true,
        inline: false,
        keep_locals: true,
        opt_level: OptLevel::O0,
    };
    /// For the bundled libraries and REPL chunks, which are not checked.
    pub const UNCHECKED: Options = Options {
        check: false,
        inline: true,
        keep_locals: false,
        opt_level: OptLevel::O1,
    };

    /// Returns a copy of `self` with the requested optimization level.
    pub const fn with_opt_level(mut self, opt_level: OptLevel) -> Self {
        self.opt_level = opt_level;
        self
    }
}

/// Parses source, keeping the line of each statement.
pub fn parse(source: &str) -> Result<Parsed, String> {
    // The parser reports bad input as `Err`; the guard only keeps a parser
    // bug from taking the host down with it.
    let (mut stmts, pool) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut parser = Parser::new(source);
        parser.parse_program()
    }))
    .map_err(|payload| {
        if let Some(s) = payload.downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
        } else {
            "syntax error".to_string()
        }
    })??;

    crate::ast::visitor::check_ast_sanity(&stmts)?;
    crate::ast::visitor_mut::normalize_ast(&mut stmts);
    if has_import_all(source) {
        let undeclared = crate::sema::undeclared_names(&stmts, source, &pool);
        stmts.splice(0..0, bundled_imports(&undeclared)?);
    }
    Ok(Parsed {
        stmts,
        lines: Rc::new(pool.start_lines()),
        spans: pool,
    })
}

/// Whether a `--!importall` line comes before the first line of code.
pub(crate) fn has_import_all(source: &str) -> bool {
    source
        .lines()
        .map(str::trim)
        .take_while(|line| line.is_empty() || line.starts_with("--"))
        .any(|line| line == "--!importall")
}

/// One `local name = require("@neyuki/name")` per bundled module in `names`.
/// Modules the program never names are not loaded, so the directive costs a
/// script nothing for the ones it doesn't use.
fn bundled_imports(names: &HashSet<String>) -> Result<Vec<Stmt>, String> {
    let imports: String = crate::vm::libs::BUNDLED_LIBRARIES
        .iter()
        .map(|(path, _)| path)
        .filter_map(|path| Some((path, path.strip_prefix("@neyuki/")?)))
        .filter(|(_, name)| names.contains(*name))
        .map(|(path, name)| format!("local {} = require({:?})\n", name, path))
        .collect();
    Ok(parse(&imports)?.stmts)
}

/// Parses Neyuki source into statements.
pub fn compile_source(source: &str) -> Result<Vec<Stmt>, String> {
    parse(source).map(|parsed| parsed.stmts)
}

/// Parses, checks and compiles a whole program with line information.
/// `chunk` names it in error messages and tracebacks, usually a file path.
pub fn compile_program(source: &str, chunk: &str, options: Options) -> Result<Proto, String> {
    let parsed = match parse(source) {
        Ok(p) => p,
        Err(err) => {
            let diag = crate::lint::syntax_error(&err);
            return Err(crate::diagnostics::render_report(chunk, source, &[diag]));
        }
    };
    if options.check {
        check(&parsed.stmts, source, chunk, Some(&parsed.spans))?;
    }
    let mut proto = lower(&parsed.stmts, Some(parsed.lines), options)?;
    proto.set_source(Rc::from(chunk));
    Ok(proto)
}

/// Reads and parses a source file.
pub fn compile_file(path: &str) -> Result<Vec<Stmt>, String> {
    let source =
        fs::read_to_string(path).map_err(|err| format!("failed to read {}: {}", path, err))?;
    compile_source(&source)
}

/// Rejects programs with semantic errors (undefined names, const writes,
/// type mismatches).
fn check(
    statements: &[Stmt],
    source: &str,
    chunk: &str,
    spans: Option<&crate::ast::span::SpanPool>,
) -> Result<(), String> {
    let diags = match spans {
        Some(spans) => crate::sema::analyze_with_spans(statements, source, spans),
        None => crate::sema::analyze(statements, source),
    };
    let errors: Vec<_> = diags
        .into_iter()
        .filter(|d| d.severity == crate::diagnostics::severity::Severity::Error)
        .collect();
    if !errors.is_empty() {
        if !source.is_empty() {
            return Err(crate::diagnostics::render_report(chunk, source, &errors));
        } else {
            return Err(format!("semantic error: {}", errors[0].message));
        }
    }
    Ok(())
}

/// Checks and compiles statements (without line information).
pub fn try_compile_to_proto(statements: &[Stmt]) -> Result<Proto, String> {
    check(statements, "", "<anon>", None)?;
    lower(statements, None, Options::RUN)
}

/// Compiles statements without checking them.
pub fn compile_bundled_to_proto(statements: &[Stmt]) -> Result<Proto, String> {
    lower(statements, None, Options::UNCHECKED)
}

fn lower(
    statements: &[Stmt],
    lines: Option<Rc<HashMap<NodeId, u32>>>,
    options: Options,
) -> Result<Proto, String> {
    let stmts = if options.opt_level == OptLevel::O1 {
        fold_program(statements.to_vec())
    } else {
        statements.to_vec()
    };
    let hir_module = hir::lower_ast(&stmts, lines.as_deref(), options.opt_level);
    let mut ir_module = ir::hir_to_ir_with(&hir_module, options.keep_locals);
    if options.inline && options.opt_level == OptLevel::O1 {
        ir::inline_functions(&mut ir_module);
    }
    optimize_ir_function(&mut ir_module.main, options.opt_level);
    ir::verify_module(&ir_module)
        .map_err(|errs| format!("IR verification error: {}", errs.join(", ")))?;
    ir::ir_to_bytecode(&ir_module)
}

/// Dumps the HIR representation of `statements` under explicit `options`.
pub fn dump_hir_with_options(statements: &[Stmt], options: Options) -> String {
    let stmts = if options.opt_level == OptLevel::O1 {
        fold_program(statements.to_vec())
    } else {
        statements.to_vec()
    };
    let hir_module = hir::lower_ast(&stmts, None, options.opt_level);
    hir::dump_hir(&hir_module)
}

/// Parses `source` and dumps its HIR with resolved source line numbers.
pub fn dump_hir_source_with_options(source: &str, options: Options) -> Result<String, String> {
    let parsed = parse(source)?;
    let stmts = if options.opt_level == OptLevel::O1 {
        fold_program(parsed.stmts)
    } else {
        parsed.stmts
    };
    let hir_module = hir::lower_ast(&stmts, Some(parsed.lines.as_ref()), options.opt_level);
    Ok(hir::dump_hir(&hir_module))
}

/// The IR of a program before and after optimization, for debugging the
/// pipeline.
pub fn dump_ir(statements: &[Stmt]) -> String {
    dump_ir_with_options(statements, Options::RUN)
}

/// The IR of a program before and after optimization under explicit `options`.
pub fn dump_ir_with_options(statements: &[Stmt], options: Options) -> String {
    let stmts = if options.opt_level == OptLevel::O1 {
        fold_program(statements.to_vec())
    } else {
        statements.to_vec()
    };
    let hir_module = hir::lower_ast(&stmts, None, options.opt_level);
    let mut ir_module = ir::hir_to_ir_with(&hir_module, options.keep_locals);
    let mut out = String::from("=== before optimization ===\n");
    out.push_str(&ir::pretty::print_module(&ir_module));
    if let Err(errs) = ir::verify_module(&ir_module) {
        out.push_str(&format!("\n[verification warnings: {}]\n", errs.join(", ")));
    }
    if options.inline && options.opt_level == OptLevel::O1 {
        ir::inline_functions(&mut ir_module);
    }
    optimize_ir_function(&mut ir_module.main, options.opt_level);
    out.push_str("\n=== after optimization ===\n");
    out.push_str(&ir::pretty::print_module(&ir_module));
    if let Err(errs) = ir::verify_module(&ir_module) {
        out.push_str(&format!("\n[verification warnings: {}]\n", errs.join(", ")));
    }
    out
}

/// Runs the CFG optimizer over a function and every function nested in it.
fn optimize_ir_function(func: &mut ir::IrFunction, opt_level: OptLevel) {
    let mut cfg = ir::build_cfg(&func.instructions);
    ir::optimize_cfg_with_level(&mut cfg, opt_level);
    func.instructions = cfg.to_flat_instructions();
    func.cfg = Some(cfg);
    for child in &mut func.protos {
        optimize_ir_function(child, opt_level);
    }
}

/// Compiles statements, panicking on errors. For tests, benchmarks and
/// fuzzing, where the source is known to be valid.
pub fn compile_to_proto(statements: &[Stmt]) -> Proto {
    try_compile_to_proto(statements).unwrap_or_else(|err| panic!("{}", err))
}

/// Compiles source to serialized bytecode, which starts with `MAGIC`.
pub fn compile_source_to_bytecode(source: &str, chunk: &str) -> Result<Vec<u8>, String> {
    Ok(serialize(&compile_program(source, chunk, Options::RUN)?))
}

/// Compiles a source file to serialized bytecode.
pub fn compile_file_to_bytecode(path: &str) -> Result<Vec<u8>, String> {
    let source =
        fs::read_to_string(path).map_err(|err| format!("failed to read {}: {}", path, err))?;
    compile_source_to_bytecode(&source, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::MAGIC;

    #[test]
    fn test_compile_source_to_bytecode_magic() {
        let code = "local a = 10\nlocal b = 20\nreturn a + b";
        let bc = compile_source_to_bytecode(code, "test").expect("compilation failed");
        assert!(bc.starts_with(MAGIC));
    }

    #[test]
    fn import_all_binds_only_the_undeclared_modules_used() {
        let parsed = parse("--!importall\nprint(fs, http)\nlocal ui = 1\nprint(ui, x.sql)\n");
        let names: Vec<_> = parsed
            .unwrap()
            .stmts
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::Local { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["fs", "http", "ui"]);
    }

    #[test]
    fn import_all_must_come_before_code() {
        let run = |code: &str| compile_program(code, "test", Options::RUN);
        assert!(run("return fs").is_err());
        assert!(run("print(1)\n--!importall\nreturn fs").is_err());
        assert!(run("--!importall now\nreturn fs").is_err());
        assert!(run("-- about\n\n--!importall\nreturn fs").is_ok());
    }

    #[test]
    fn import_all_lints_clean_and_keeps_undefined_name_errors() {
        assert!(crate::lint::lint_source("--!importall\nprint(fs, net)\n").is_empty());
        assert!(compile_program("--!importall\nreturn nope", "test", Options::RUN).is_err());
    }

    #[test]
    fn test_compile_registers_and_proto() {
        let code = "local x = 5\nreturn x";
        let stmts = compile_source(code).expect("syntax error");
        let proto = compile_to_proto(&stmts);
        assert!(proto.max_registers >= 1);
        assert!(!proto.instructions.is_empty());
    }

    #[test]
    fn test_compile_error_on_invalid_interpolation() {
        // Unfinished interpolation must fail compilation
        let code = "local s = `hello {name`";
        let res = compile_source(code);
        assert!(res.is_err());
        assert!(res.unwrap_err().to_lowercase().contains("unfinished"));

        // Multi-statement in interpolation must fail compilation
        let code2 = "local s = `hello {a; b}`";
        let res2 = compile_source(code2);
        assert!(res2.is_err());
        assert!(res2.unwrap_err().contains("exactly one expression"));
    }

    #[test]
    fn test_ir_pipeline_inlines_small_leaf_call() {
        let code = "local function add(x, y) return x + y end\nlocal s = 0\nfor i = 1, 5 do s = s + add(i, i) end\nreturn s";
        let stmts = compile_source(code).expect("syntax error");
        let proto = try_compile_to_proto(&stmts).expect("IR compilation failed");
        // The two-instruction leaf must be inlined: no Call left in main.
        assert!(
            !proto
                .instructions
                .iter()
                .any(|i| matches!(i, crate::bytecode::instruction::Instruction::Call { .. })),
            "expected inlined call, got:\n{}",
            crate::bytecode::disasm::disassemble_proto(&proto, 0)
        );

        let mut vm = crate::vm::machine::VM::new();
        let res = vm.execute(proto).expect("exec failed");
        assert_eq!(res.to_string(), "30");
    }

    #[test]
    fn test_opt_levels_o0_and_o1_produce_same_result_and_respect_inlining() {
        let code =
            "local function add(x, y) return x + y end\nlocal a = 2 + 3\nreturn a + add(4, 5)";
        let proto_o0 = compile_program(code, "test", Options::RUN.with_opt_level(OptLevel::O0))
            .expect("O0 compile failed");
        let proto_o1 = compile_program(code, "test", Options::RUN.with_opt_level(OptLevel::O1))
            .expect("O1 compile failed");

        assert!(
            proto_o0
                .instructions
                .iter()
                .any(|i| matches!(i, crate::bytecode::instruction::Instruction::Call { .. })),
            "O0 must not inline calls"
        );
        assert!(
            !proto_o1
                .instructions
                .iter()
                .any(|i| matches!(i, crate::bytecode::instruction::Instruction::Call { .. })),
            "O1 should inline leaf calls"
        );

        let r0 = crate::vm::machine::VM::new()
            .execute(proto_o0)
            .expect("O0 exec failed");
        let r1 = crate::vm::machine::VM::new()
            .execute(proto_o1)
            .expect("O1 exec failed");
        assert_eq!(r0.to_string(), "14");
        assert_eq!(r1.to_string(), "14");
    }

    #[test]
    fn test_multiple_returns_and_generic_for_state() {
        let code = "local function rotate(a, b, c) return b, c, a end
                    local x, y, z = rotate(1, 2, 3)
                    local function step(t, i) i = i + 1 if t[i] == nil then return nil end return i, t[i] end
                    local function each(t) return step, t, 0 end
                    local sum = 0
                    for i, v in each({10, 20}) do sum = sum + i * v end
                    return x * 100 + y * 10 + z + sum";
        let stmts = compile_source(code).expect("syntax error");
        let proto = try_compile_to_proto(&stmts).expect("IR compilation failed");
        let res = crate::vm::machine::VM::new()
            .execute(proto)
            .expect("exec failed");
        assert_eq!(res.to_string(), "281");
    }

    /// The parser's nesting limit has to keep every later pass inside an
    /// ordinary thread's stack: library users and `cargo test` do not get the
    /// CLI's large one. The deepest programs it accepts, in each shape that
    /// recurses, must compile and run on a 2 MiB thread in a debug build.
    #[test]
    fn test_deepest_accepted_programs_fit_a_default_stack() {
        type Shape = fn(usize) -> String;
        let shapes: [(&str, Shape); 6] = [
            ("blocks", |n| {
                format!("{}x = 1{}", "if true then\n".repeat(n), "\nend".repeat(n))
            }),
            ("loops", |n| {
                format!(
                    "{}x = 1{}",
                    "while x == nil do\n".repeat(n),
                    "\nend".repeat(n)
                )
            }),
            ("parentheses", |n| {
                format!("x = {}1{}", "(".repeat(n), ")".repeat(n))
            }),
            ("operator chain", |n| format!("x = 1{}", " + 1".repeat(n))),
            ("tables", |n| {
                format!("x = {}{}", "{".repeat(n), "}".repeat(n))
            }),
            ("calls", |n| {
                format!("x = {}1{}", "tostring(".repeat(n), ")".repeat(n))
            }),
        ];
        let worker = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                for (name, shape) in shapes {
                    let deepest = (1..=crate::parser::MAX_PARSE_DEPTH)
                        .rev()
                        .find(|&n| compile_source(&shape(n)).is_ok())
                        .unwrap_or_else(|| panic!("no {} program parses", name));
                    assert!(deepest >= 60, "{} only nest {} deep", name, deepest);
                    let source = format!("local x\n{}", shape(deepest));
                    let stmts = compile_source(&source).expect(name);
                    let proto = try_compile_to_proto(&stmts).expect(name);
                    crate::vm::machine::VM::new().execute(proto).expect(name);
                }
            })
            .unwrap();
        worker
            .join()
            .expect("a pass overflowed or failed on the deepest program");
    }
}
