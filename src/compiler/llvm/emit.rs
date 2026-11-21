//! LLVM IR module assembly and C `main` entrypoint generation.

use crate::compiler::OptLevel;
use crate::compiler::llvm::codegen::{
    FunctionEmitter, LlvmModuleCollector, emit_string_constants, emit_user_globals,
};
use crate::compiler::llvm::runtime::emit_runtime_prelude;
use crate::compiler::mir::MirProgram;

/// Emits standalone textual LLVM IR (`.ll`) for a compiled [`MirProgram`].
pub fn emit_llvm_ir(program: &MirProgram, opt_level: OptLevel) -> String {
    let mut collector = LlvmModuleCollector::new();
    collector.collect_program(program);

    let mut fn_ir = String::new();
    let emitter = FunctionEmitter::new(&mut collector, 0);
    fn_ir.push_str(&emitter.emit_body(&program.main, "nyk_main"));

    let opt_note = match opt_level {
        OptLevel::O0 => "-O0",
        OptLevel::O1 => "-O1",
    };

    let mut out = String::new();
    emit_runtime_prelude(&mut out, opt_note);
    out.push_str(&emit_string_constants(&collector.string_literals));
    out.push_str(&emit_user_globals(&collector.user_globals));
    out.push_str(&fn_ir);
    out.push_str(
        r#"
define i32 @main() {
entry:
  %res = call %NykVal @nyk_main()
  ret i32 0
}
"#,
    );
    out
}
