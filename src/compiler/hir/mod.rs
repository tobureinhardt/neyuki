//! High-level Intermediate Representation (HIR): desugared AST ready for
//! typed lowering (THIR) and CFG/bytecode IR lowering.

pub mod desugar;
pub mod lower;
pub mod nodes;
pub mod pretty;

pub use desugar::optimize_hir;
pub use lower::lower_ast;
pub use nodes::{
    HirExpr, HirExprKind, HirId, HirLValue, HirModule, HirStmt, HirStmtKind, HirTableEntry,
};
pub use pretty::dump_hir;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::literal::Literal;
    use crate::ast::op::BinOp;
    use crate::compiler::{OptLevel, compile_source};

    #[test]
    fn test_hir_erases_type_alias_and_flattens_elseif() {
        let code = "\
type User = { name: string }
local x = 2
if x == 1 then
  return 10
elseif x == 2 then
  return 20
else
  return 30
end";
        let stmts = compile_source(code).expect("syntax error");
        let hir = lower_ast(&stmts, None, OptLevel::O1);

        assert_eq!(hir.items.len(), 2);
        let HirStmtKind::If {
            else_branch: Some(else_items),
            ..
        } = &hir.items[1].kind
        else {
            panic!(
                "expected If with flattened else_branch, got {:?}",
                hir.items[1]
            );
        };
        assert_eq!(else_items.len(), 1);
        assert!(matches!(else_items[0].kind, HirStmtKind::If { .. }));
    }

    #[test]
    fn test_hir_desugars_interp_dotted_fn_and_local_increment() {
        let code = "\
local t = {}
function t.greet(who)
  return `hi {who}!`
end
local x = 1
x++";
        let stmts = compile_source(code).expect("syntax error");
        let hir = lower_ast(&stmts, None, OptLevel::O1);

        // `function t.greet` desugared into `t.greet = fn(who) ...`
        assert!(matches!(
            hir.items[1].kind,
            HirStmtKind::Assign {
                target: HirLValue::Member { .. },
                ..
            }
        ));
        if let HirStmtKind::Assign { value, .. } = &hir.items[1].kind {
            if let HirExprKind::Function { body, .. } = &value.kind {
                if let HirStmtKind::Return(vals) = &body[0].kind {
                    assert!(matches!(
                        vals[0].kind,
                        HirExprKind::Binary {
                            op: BinOp::Concat,
                            ..
                        }
                    ));
                } else {
                    panic!("expected return");
                }
            } else {
                panic!("expected function expr");
            }
        }

        // `x++` desugared into `x = x + 1`
        assert!(matches!(
            hir.items[3].kind,
            HirStmtKind::Assign {
                target: HirLValue::Variable(ref name),
                value: HirExpr {
                    kind: HirExprKind::Binary {
                        op: BinOp::Add,
                        ..
                    },
                    ..
                },
                ..
            } if name == "x"
        ));
    }

    #[test]
    fn test_hir_o1_prunes_dead_branches_while_o0_preserves_them() {
        let code = "\
if false then
  local dead = 1
end
if true then
  return 42
end";
        let stmts = compile_source(code).expect("syntax error");
        let hir_o0 = lower_ast(&stmts, None, OptLevel::O0);
        let hir_o1 = lower_ast(&stmts, None, OptLevel::O1);

        assert_eq!(hir_o0.items.len(), 2);
        assert!(matches!(hir_o0.items[0].kind, HirStmtKind::If { .. }));

        assert_eq!(hir_o1.items.len(), 1);
        assert!(matches!(
            hir_o1.items[0].kind,
            HirStmtKind::Block(ref inner)
            if matches!(inner.as_slice(), [HirStmt { kind: HirStmtKind::Return(vals), .. }]
                if matches!(vals[0].kind, HirExprKind::Literal(Literal::Int(_))))
        ));
    }

    #[test]
    fn test_hir_o1_folds_interp_concat_and_short_circuits() {
        let code = "\
local function f(x: string): string
  local a = nil ?? x
  local b = true and a
  local c = false or `hi {b}`
  return c
end";
        let stmts = compile_source(code).expect("syntax error");
        let hir = lower_ast(&stmts, None, OptLevel::O1);
        let HirStmtKind::Function { body, .. } = &hir.items[0].kind else {
            panic!("expected function");
        };
        // `nil ?? x` -> `x`
        assert!(matches!(
            body[0].kind,
            HirStmtKind::Local {
                initializer: Some(HirExpr {
                    kind: HirExprKind::Variable(ref v),
                    ..
                }),
                ..
            } if v == "x"
        ));
        // `true and a` -> `a`
        assert!(matches!(
            body[1].kind,
            HirStmtKind::Local {
                initializer: Some(HirExpr {
                    kind: HirExprKind::Variable(ref v),
                    ..
                }),
                ..
            } if v == "a"
        ));
    }
}
