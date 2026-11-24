pub mod buffer;
pub mod builtins;
pub mod call;
pub mod frame;
pub mod gc;
pub mod hash;
pub mod heap;
pub mod interp;
pub mod libs;
pub mod limits;
pub mod machine;
pub mod metamethod;
pub mod ops;
pub mod permissions;
pub mod table;
#[cfg(not(debug_assertions))]
mod threaded;
pub mod traceback;
pub mod upvalue;
pub mod value;

pub use limits::{ExecutionLimits, LimitViolation};
pub use machine::VM;
pub use permissions::{PermissionScope, Permissions};
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
    run_file_with_config(
        path,
        options,
        ExecutionLimits::default(),
        Permissions::default(),
    )
}

/// Runs a script or bytecode file under explicit execution limits and sandbox permissions.
pub fn run_file_with_config(
    path: &str,
    options: crate::compiler::Options,
    limits: ExecutionLimits,
    permissions: Permissions,
) -> Result<(), String> {
    let bytes = fs::read(path).map_err(|e| format!("failed to read {}: {}", path, e))?;
    run_bytes_with_config(path, bytes, options, limits, permissions)
}

/// Runs a script's source or bytecode that was read from `path` (a file on
/// disk or a path inside an app bundle).
pub fn run_bytes(
    path: &str,
    bytes: Vec<u8>,
    options: crate::compiler::Options,
) -> Result<(), String> {
    run_bytes_with_config(
        path,
        bytes,
        options,
        ExecutionLimits::default(),
        Permissions::default(),
    )
}

/// Runs a script's source or bytecode with explicit execution limits and sandbox permissions.
pub fn run_bytes_with_config(
    path: &str,
    bytes: Vec<u8>,
    options: crate::compiler::Options,
    limits: ExecutionLimits,
    permissions: Permissions,
) -> Result<(), String> {
    let proto = if bytes.starts_with(crate::bytecode::MAGIC) {
        deserialize(&bytes)?
    } else {
        let source = String::from_utf8(bytes)
            .map_err(|_| format!("failed to read {}: file is not valid UTF-8", path))?;
        crate::compiler::compile_program(&source, path, options)?
    };
    let mut vm = VM::new();
    vm.script_dir = script_dir(path);
    vm.set_limits(limits);
    vm.set_permissions(permissions);
    vm.execute_program(proto)
        .map(|_| ())
        .map_err(|err| vm.describe_error(&err))
}

/// Runs a `.nyk` source file (or a compiled `.nykb`) on the register VM:
/// parse, check, compile through the optimizing IR pipeline, execute.
pub fn run_file(path: &str) -> Result<(), String> {
    run_file_with_options(path, crate::compiler::Options::RUN)
}
