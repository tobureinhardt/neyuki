//! Mid-level Intermediate Representation (MIR): CFG-based place/operand IR
//! connecting THIR to both the bytecode IR pipeline and the LLVM IR backend.

pub mod cfg;
pub mod lower;
pub mod opt;
pub mod pretty;
pub mod to_ir;

pub use cfg::{
    BasicBlockData, BasicBlockId, LocalDecl, LocalId, LocalKind, MirBody, MirConst, MirProgram,
    MirSpreadSource, MirUpvalue, Operand, Place, Projection, Rvalue, Statement, StatementKind,
    TerminatorKind,
};
pub use lower::lower_thir_to_mir;
pub use opt::optimize_mir;
pub use pretty::dump_mir;
pub use to_ir::mir_to_ir;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::hir::lower_ast;
    use crate::compiler::thir::lower_hir_to_thir;
    use crate::compiler::{OptLevel, compile_source, try_compile_to_proto};
    use crate::vm::machine::VM;

    #[test]
    fn test_mir_executes_numeric_for_closures_and_spread() {
        let code = "\
local fns = {}
for i = 1, 4 do
  fns[i] = function() return i * 10 end
end
local function pair(a, b) return a, b end
local x, y = pair(fns[2](), fns[4]())
return x + y";
        let stmts = compile_source(code).expect("syntax error");
        let proto = try_compile_to_proto(&stmts).expect("compile failed");
        let res = VM::new().execute(proto).expect("exec failed");
        assert_eq!(res.to_string(), "60");
    }

    #[test]
    fn test_mir_o1_prunes_unreachable_switch_blocks() {
        let code = "\
local x = true
if x then
  return 7
else
  return 99
end";
        let stmts = compile_source(code).expect("syntax error");
        let hir = lower_ast(&stmts, None, OptLevel::O0);
        let thir = lower_hir_to_thir(&hir);
        let mut mir_o0 = lower_thir_to_mir(&thir, false);
        let blocks_before = mir_o0.main.blocks.len();
        optimize_mir(&mut mir_o0, OptLevel::O1);
        assert!(mir_o0.main.blocks.len() < blocks_before);
    }
}
