//! Typed High-level Intermediate Representation (THIR): carries resolved
//! lexical bindings, capture lists, and inferred static `NeyukiType`s.

pub mod lower;
pub mod nodes;
pub mod pretty;
pub mod typeck;

pub use lower::lower_hir_to_thir;
pub use nodes::{
    BindingId, BindingKind, ThirExpr, ThirExprKind, ThirId, ThirLValue, ThirModule, ThirNumericFor,
    ThirParam, ThirStmt, ThirStmtKind, ThirTableEntry, VarBinding,
};
pub use pretty::dump_thir;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::hir::lower_ast;
    use crate::compiler::{OptLevel, compile_source};
    use crate::sema::types::NeyukiType;

    #[test]
    fn test_thir_resolves_types_bindings_and_upvalues() {
        let code = "\
local function add(a: int, b: int): int
  return a + b
end
local x = add(1, 2)
local function bump(): int
  return x + 1
end
return bump()";
        let stmts = compile_source(code).expect("syntax error");
        let hir = lower_ast(&stmts, None, OptLevel::O1);
        let thir = lower_hir_to_thir(&hir);

        let x_binding = thir
            .bindings
            .iter()
            .find(|b| b.name == "x")
            .expect("missing binding for x");
        assert_eq!(x_binding.kind, BindingKind::Upvalue);
        assert_eq!(x_binding.ty, NeyukiType::Int);

        let bump_binding = thir
            .bindings
            .iter()
            .find(|b| b.name == "bump")
            .expect("missing binding for bump");
        assert_eq!(
            bump_binding.ty,
            NeyukiType::Function {
                params: vec![],
                return_type: Box::new(NeyukiType::Int),
                is_vararg: false,
            }
        );

        let ThirStmtKind::Return(vals) = &thir.items.last().unwrap().kind else {
            panic!("expected trailing return");
        };
        assert_eq!(vals[0].ty, NeyukiType::Int);
    }

    #[test]
    fn test_thir_marks_module_import_members_and_narrows_coalesce() {
        let code = "\
local s: string? = nil
local out = s ?? \"fallback\"
local root = fs.cwd()
return out, root";
        let stmts = compile_source(code).expect("syntax error");
        let hir = lower_ast(&stmts, None, OptLevel::O1);
        let thir = lower_hir_to_thir(&hir);

        let out_binding = thir
            .bindings
            .iter()
            .find(|b| b.name == "out")
            .expect("missing out binding");
        assert_eq!(out_binding.ty, NeyukiType::String);

        let ThirStmtKind::Local {
            initializer: Some(root_call),
            ..
        } = &thir.items[2].kind
        else {
            panic!("expected root initializer");
        };
        let ThirExprKind::Call { callee, .. } = &root_call.kind else {
            panic!("expected Call");
        };
        assert!(matches!(
            callee.kind,
            ThirExprKind::Member {
                is_module_import: true,
                ..
            }
        ));
    }
}
