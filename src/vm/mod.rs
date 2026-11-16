pub mod buffer;
pub mod builtins;
pub mod call;
pub mod frame;
pub mod gc;
pub mod hash;
pub mod interp;
pub mod libs;
pub mod machine;
pub mod metamethod;
pub mod ops;
pub mod table;
pub mod traceback;
pub mod upvalue;
pub mod value;

pub use machine::VM;
use std::fs;
pub use table::TableKey;
pub use value::Value;

use crate::bytecode::deserialize;

// Execute compiled bytecode buffer
pub fn execute_bytecode(bytes: &[u8]) -> Result<Value, String> {
    execute_bytecode_at(bytes, None)
}

fn execute_bytecode_at(bytes: &[u8], path: Option<&str>) -> Result<Value, String> {
    let proto = deserialize(bytes)?;
    let mut vm = VM::new();
    vm.script_dir = path.and_then(script_dir);
    vm.execute_program(proto)
}

/// The directory `require` resolves modules against for the script at
/// `path`: its parent, as written (inside an app bundle this is a path in
/// the bundle, not on disk).
fn script_dir(path: &str) -> Option<std::path::PathBuf> {
    std::path::Path::new(path)
        .parent()
        .map(|dir| dir.to_path_buf())
}

pub fn run_file_with_options(path: &str, options: crate::compiler::Options) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| format!("failed to read {}: {}", path, e))?;
    run_bytes(path, bytes, options)
}

/// Runs a script's source or bytecode that was read from `path` (a file on
/// disk or a path inside an app bundle).
pub fn run_bytes(
    path: &str,
    bytes: Vec<u8>,
    options: crate::compiler::Options,
) -> Result<(), String> {
    if bytes.starts_with(crate::bytecode::MAGIC) {
        return execute_bytecode_at(&bytes, Some(path)).map(|_| ());
    }
    let source = String::from_utf8(bytes)
        .map_err(|_| format!("failed to read {}: file is not valid UTF-8", path))?;
    let proto = crate::compiler::compile_program(&source, path, options)?;
    let mut vm = VM::new();
    vm.script_dir = script_dir(path);
    vm.execute_program(proto)
        .map(|_| ())
        .map_err(|err| vm.describe_error(&err))
}

/// Runs a `.nyk` source file (or a compiled `.nykb`) on the register VM:
/// parse, check, compile through the optimizing IR pipeline, execute.
pub fn run_file(path: &str) -> Result<(), String> {
    run_file_with_options(path, crate::compiler::Options::RUN)
}
