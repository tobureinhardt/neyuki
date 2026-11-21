//! The three-address IR between the syntax tree and bytecode.

pub mod block;
pub mod builder;
pub mod cfg_builder;
pub mod codegen;
pub mod dom;
pub mod inst;
pub mod liveness;
pub mod opt;
pub mod pretty;
pub mod regalloc;
pub mod ssa;
pub mod types;
pub mod verify;

pub use block::{BasicBlock, ControlFlowGraph, IrFunction, IrModule, UpvalSource};
pub use builder::{ast_to_ir, ast_to_ir_with, hir_to_ir_with};
pub use cfg_builder::build_cfg;
pub use codegen::ir_to_bytecode;
pub use dom::{DominatorTree, ImmediateDominators};
pub use inst::{IrInst, SpreadSink, SpreadSource};
pub use liveness::{LiveInterval, LivenessInfo};
pub use opt::{
    common_subexpression_elimination, common_subexpression_elimination_cfg, constant_propagation,
    copy_propagation, copy_propagation_cfg, dead_code_elimination, dead_code_elimination_cfg,
    inline_functions, loop_invariant_code_motion, optimize_cfg, optimize_cfg_with_level,
    peephole_cfg, simplify_cfg, strength_reduction_cfg,
};
pub use pretty::{format_inst, print_cfg, print_function, print_module};
pub use regalloc::{RegisterAllocation, allocate_registers};
pub use ssa::{construct_ssa, destruct_ssa, ssa_constant_propagation};
pub use types::{IrBinaryOp, IrConstant, IrLabel, IrUnaryOp, IrVar};
pub use verify::{verify_cfg, verify_function, verify_module};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::compile_source;
    use crate::vm::machine::VM;
    use num_bigint::BigInt;

    /// Compiles and runs `code` through the full IR pipeline, the way
    /// `--opt-ir` does.
    fn run_via_ir(code: &str) -> String {
        let stmts = compile_source(code).expect("syntax error");
        let proto = crate::compiler::try_compile_to_proto(&stmts).expect("ir lowering failed");
        let mut vm = VM::new();
        vm.execute(proto).expect("execution failed").to_string()
    }

    #[test]
    fn test_ir_call_does_not_clobber_live_registers() {
        // The call window has to sit above every live variable, or the
        // callee's frame overwrites them while it runs.
        let code = "local function add(a, b)\n  return a + b\nend\n\
local keep = 7\n\
local sum = add(1, 2)\n\
return keep * 100 + sum";
        assert_eq!(run_via_ir(code), "703");
    }

    #[test]
    fn test_ir_recursive_global_function() {
        let code = "function fib(n)\n\
  if n <= 1 then\n    return n\n  end\n\
  return fib(n - 1) + fib(n - 2)\n\
end\n\
return fib(10)";
        assert_eq!(run_via_ir(code), "55");
    }

    #[test]
    fn test_ir_generic_for_terminates() {
        // Staging the iterator inside the loop would reset the control
        // variable on every pass and never finish.
        let code = "local t = { 10, 20, 30 }\n\
local sum = 0\n\
for _, v in t do\n  sum = sum + v\nend\n\
return sum";
        assert_eq!(run_via_ir(code), "60");
    }

    #[test]
    fn test_ir_capture_across_two_function_levels() {
        // The middle function has to capture `secret` too, so the innermost
        // one can read it from an upvalue rather than a register.
        let code = "local function outer()
  local secret = 42
  local function middle()
    local function inner() return secret end
    return inner()
  end
  return middle()
end
return outer()";
        assert_eq!(run_via_ir(code), "42");
    }

    #[test]
    fn test_ir_closure_capture_survives_register_reuse() {
        let code = "local function make()\n\
  local hidden = 11\n\
  local unrelated = 1\n\
  return function() return hidden end\n\
end\n\
return make()()";
        assert_eq!(run_via_ir(code), "11");
    }

    #[test]
    fn test_ir_multiple_assignment_from_call() {
        let code = "local function pair()\n  return 1, 2\nend\n\
local a, b = pair()\n\
return a * 10 + b";
        assert_eq!(run_via_ir(code), "12");
    }

    #[test]
    fn test_ir_spreads_call_results() {
        let code = "local table = require(\"@neyuki/table\")\n\
local function three()\n  return 1, 2, 3\nend\n\
local collected = { three() }\n\
local function total(a, b, c)\n  return a + b + c\nend\n\
return #collected * 100 + total(table.unpack(collected))";
        assert_eq!(run_via_ir(code), "306");
    }

    #[test]
    fn test_ir_reuses_registers_across_many_locals() {
        // One register per variable would run past the 255-register limit.
        let mut code = String::from("local total = 0\n");
        for i in 0..300 {
            code.push_str(&format!("total = total + {}\n", i));
        }
        code.push_str("return total");
        assert_eq!(run_via_ir(&code), "44850");
    }

    #[test]
    fn test_ir_basic() {
        let stmts = vec![];
        let mut module = ast_to_ir(&stmts);
        dead_code_elimination(&mut module);
        constant_propagation(&mut module);
        let proto = ir_to_bytecode(&module).expect("ir lowering failed");
        assert_eq!(proto.name, Some("main".to_string()));
    }

    #[test]
    fn test_ast_to_ir_and_constant_folding() {
        let code = "local a = 10 + 20\nreturn a";
        let stmts = compile_source(code).expect("syntax error");
        let mut ir_module = ast_to_ir(&stmts);

        let has_binop_before = ir_module
            .main
            .instructions
            .iter()
            .any(|i| matches!(i, IrInst::BinOp { .. }));
        assert!(has_binop_before);

        constant_propagation(&mut ir_module);

        let has_folded_30 = ir_module.main.instructions.iter().any(|i| {
            if let IrInst::LoadConst {
                val: IrConstant::Int(bi),
                ..
            } = i
            {
                bi == &BigInt::from(30)
            } else {
                false
            }
        });
        assert!(has_folded_30);

        dead_code_elimination(&mut ir_module);

        let proto = ir_to_bytecode(&ir_module).expect("ir lowering failed");
        let mut vm = VM::new();
        let res = vm.execute(proto).expect("execution failed");
        assert_eq!(res.to_string(), "30");
    }

    #[test]
    fn test_ir_if_control_flow() {
        let code =
            "local x = 5\nlocal res = 0\nif x == 5 then res = 100 else res = 200 end\nreturn res";
        let stmts = compile_source(code).expect("syntax error");
        let mut ir_module = ast_to_ir(&stmts);
        constant_propagation(&mut ir_module);
        dead_code_elimination(&mut ir_module);
        let proto = ir_to_bytecode(&ir_module).expect("ir lowering failed");

        let mut vm = VM::new();
        let val = vm.execute(proto).expect("exec failed");
        assert_eq!(val.to_string(), "100");
    }

    #[test]
    fn test_ir_while_loop() {
        let code =
            "local sum = 0\nlocal i = 1\nwhile i <= 4 do sum = sum + i; i = i + 1 end\nreturn sum";
        let stmts = compile_source(code).expect("syntax error");
        let ir_module = ast_to_ir(&stmts);
        let proto = ir_to_bytecode(&ir_module).expect("ir lowering failed");

        let mut vm = VM::new();
        let val = vm.execute(proto).expect("exec failed");
        assert_eq!(val.to_string(), "10");
    }

    #[test]
    fn test_ir_repeat_and_interpolation() {
        let code = "local a = 0\nrepeat a += 1 until a == 3\nlocal msg = `val: {a}`\nreturn msg";
        let stmts = compile_source(code).expect("syntax error");
        let ir_module = ast_to_ir(&stmts);
        let proto = ir_to_bytecode(&ir_module).expect("ir lowering failed");

        let mut vm = VM::new();
        let val = vm.execute(proto).expect("exec failed");
        assert_eq!(val.to_string(), "val: 3");
    }

    #[test]
    fn test_ir_closure_and_sub_function() {
        let code = "local function make_adder(x)\n  return x + 5\nend\nreturn make_adder(10)";
        let stmts = compile_source(code).expect("syntax error");
        let ir_module = ast_to_ir(&stmts);
        let proto = ir_to_bytecode(&ir_module).expect("ir lowering failed");

        let mut vm = VM::new();
        let val = vm.execute(proto).expect("exec failed");
        assert_eq!(val.to_string(), "15");
    }

    #[test]
    fn test_ir_depth_limit_rejected() {
        let mut curr = IrFunction::new(Some("level_65".to_string()), 0, false);
        for lvl in (0..65).rev() {
            let mut parent = IrFunction::new(Some(format!("level_{}", lvl)), 0, false);
            parent.protos.push(curr);
            curr = parent;
        }
        let module = IrModule { main: curr };
        let result = ir_to_bytecode(&module);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("depth limit (64) exceeded"));
    }

    #[test]
    fn test_ir_and_or_short_circuit() {
        let code = "local a = false and 10 or 20\nreturn a";
        let stmts = compile_source(code).expect("syntax error");
        let ir_module = ast_to_ir(&stmts);
        let proto = ir_to_bytecode(&ir_module).expect("ir lowering failed");

        let mut vm = VM::new();
        let val = vm.execute(proto).expect("exec failed");
        assert_eq!(val.to_string(), "20");
    }

    #[test]
    fn test_ir_numeric_for() {
        let code = "local sum = 0\nfor i = 1, 5 do\n  sum = sum + i\nend\nreturn sum";
        let stmts = compile_source(code).expect("syntax error");
        let ir_module = ast_to_ir(&stmts);
        let proto = ir_to_bytecode(&ir_module).expect("ir lowering failed");

        let mut vm = VM::new();
        let val = vm.execute(proto).expect("exec failed");
        assert_eq!(val.to_string(), "15");
    }

    #[test]
    fn test_cfg_construction_and_dom_tree() {
        let code = "local x = 10\nif x > 5 then x = 20 else x = 30 end\nreturn x";
        let stmts = compile_source(code).expect("syntax error");
        let ir_module = ast_to_ir(&stmts);
        let cfg = build_cfg(&ir_module.main.instructions);

        assert!(cfg.blocks.len() >= 3);
        let dom = DominatorTree::build(&cfg);
        // Entry dominates entry
        assert!(dom.dominates(cfg.entry_label, cfg.entry_label));

        let liveness = LivenessInfo::compute(&cfg);
        let intervals = liveness.compute_live_intervals(&cfg);
        assert!(!intervals.is_empty());

        let reg_alloc = allocate_registers(&cfg, 0).expect("regalloc should succeed");
        assert!(reg_alloc.max_registers < 255);
    }

    #[test]
    fn test_ssa_construction_and_destruction() {
        let code = "local x = 1\nif x == 1 then x = 2 else x = 3 end\nreturn x";
        let stmts = compile_source(code).expect("syntax error");
        let ir_module = ast_to_ir(&stmts);
        let mut cfg = build_cfg(&ir_module.main.instructions);
        let dom = DominatorTree::build(&cfg);

        construct_ssa(&mut cfg, &dom);
        destruct_ssa(&mut cfg);
        let verify_result = verify_cfg(&cfg);
        assert!(
            verify_result.is_ok(),
            "CFG verification failed: {:?}",
            verify_result
        );
    }

    #[test]
    fn test_cfg_optimizations() {
        let code = "local a = 10\nlocal b = 20\nlocal c = a + b\nlocal d = a + b\nreturn c";
        let stmts = compile_source(code).expect("syntax error");
        let ir_module = ast_to_ir(&stmts);
        let mut cfg = build_cfg(&ir_module.main.instructions);

        common_subexpression_elimination_cfg(&mut cfg);
        copy_propagation_cfg(&mut cfg);
        dead_code_elimination_cfg(&mut cfg);
        simplify_cfg(&mut cfg);

        let pretty_out = {
            let mut out = String::new();
            print_cfg(&cfg, 0, &mut out);
            out
        };
        assert!(!pretty_out.is_empty());
    }

    #[test]
    fn test_ssa_constant_propagation() {
        let code = "local x = 1\nif x == 1 then x = 42 else x = 42 end\nreturn x";
        let stmts = compile_source(code).expect("syntax error");
        let ir_module = ast_to_ir(&stmts);
        let mut cfg = build_cfg(&ir_module.main.instructions);
        let dom = DominatorTree::build(&cfg);

        construct_ssa(&mut cfg, &dom);
        let folded = ssa_constant_propagation(&mut cfg);
        assert!(
            folded,
            "SSA constant propagation should have folded the phi"
        );
        destruct_ssa(&mut cfg);

        let proto = ir_to_bytecode(&IrModule {
            main: IrFunction {
                name: Some("main".to_string()),
                params: Vec::new(),
                num_params: 0,
                is_vararg: false,
                instructions: cfg.to_flat_instructions(),
                protos: Vec::new(),
                upvalues: Vec::new(),
                upvalue_vars: Vec::new(),
                upvalue_names: Vec::new(),
                cfg: Some(cfg),
            },
        })
        .expect("codegen failed");

        let mut vm = VM::new();
        let val = vm.execute(proto).expect("execution failed");
        assert_eq!(val.to_string(), "42");
    }

    #[test]
    fn test_cfg_fixpoint_optimization_driver() {
        let code = "local a = 10\nlocal b = 20\nlocal c = a + b\nlocal d = a + b\nlocal e = d\nreturn c + e";
        let stmts = compile_source(code).expect("syntax error");
        let ir_module = ast_to_ir(&stmts);
        let mut cfg = build_cfg(&ir_module.main.instructions);

        optimize_cfg(&mut cfg);
        let verify_result = verify_cfg(&cfg);
        assert!(verify_result.is_ok());
    }

    #[test]
    fn test_complex_control_flow_3_level_loops_and_closures() {
        let code = r#"
            local total = 0
            for i = 1, 3 do
                for j = 1, 3 do
                    local k = 1
                    while k <= 2 do
                        if k == 2 then
                            break
                        end
                        local function add_k(val)
                            return val + k + i + j
                        end
                        total = add_k(total)
                        k = k + 1
                    end
                end
            end
            return total
        "#;
        let stmts = compile_source(code).expect("syntax error");
        let ir_module = ast_to_ir(&stmts);
        let mut cfg = build_cfg(&ir_module.main.instructions);

        optimize_cfg(&mut cfg);
        let verify_result = verify_cfg(&cfg);
        assert!(
            verify_result.is_ok(),
            "CFG verification failed: {:?}",
            verify_result.err()
        );

        let proto = ir_to_bytecode(&ir_module).expect("ir codegen failed");
        let mut vm = VM::new();
        let val = vm.execute(proto).expect("execution failed");
        // i: 1..3, j: 1..3 -> 9 iterations. In each: k=1, add_k(val) = val + 1 + i + j.
        // sum(1 + i + j) for i in 1..3, j in 1..3 = 9*1 + 3*(1+2+3) + 3*(1+2+3) = 9 + 18 + 18 = 45.
        assert_eq!(val.to_string(), "45");
    }

    /// The CFG is flattened after optimization and rebuilt for codegen, so
    /// the second round sees the first round's auto labels as real `Label`
    /// instructions. Minting fresh ones from the same base handed two blocks
    /// the same label, and every map keyed by label — liveness above all —
    /// then merged them.
    #[test]
    fn test_cfg_rebuild_keeps_labels_unique() {
        let code = r##"
            local win = { w = 640, h = 400 }
            function win.size(self) return self.w, self.h end
            function win.clear(self, color) self.last = color end
            function win.text(self, x, y, s) self.label = s end
            function win.present(self) end
            local ball = { x = 320, y = 200, vx = 180, vy = 120, r = 24 }
            local clicks = 0
            local handler = function(events, dt)
                for _, event in events do
                    if event.kind == "keydown" and event.key == "escape" then
                        return "quit"
                    elseif event.kind == "mousedown" and event.button == "left" then
                        ball.x, ball.y = event.x, event.y
                        clicks = clicks + 1
                    end
                end
                local w, h = win:size()
                ball.x = ball.x + ball.vx * dt
                ball.y = ball.y + ball.vy * dt
                if ball.x < ball.r or ball.x > w - ball.r then ball.vx = -ball.vx end
                if ball.y < ball.r or ball.y > h - ball.r then ball.vy = -ball.vy end
                win:clear("#1e1e2e")
                win:text(24, 22, "clicks: " .. clicks)
                win:present()
                return "ran"
            end
            local verdict = handler({
                { kind = "focus" },
                { kind = "mousedown", button = "left", x = 7, y = 9 },
                { kind = "mousemove", x = 1, y = 2 }
            }, 0)
            return verdict .. ":" .. clicks .. ":" .. ball.x .. ":" .. ball.y .. ":" .. win.label
        "##;
        fn check(func: &IrFunction) {
            let mut cfg = build_cfg(&func.instructions);
            optimize_cfg(&mut cfg);
            let rebuilt = build_cfg(&cfg.to_flat_instructions());

            let mut seen = std::collections::HashSet::new();
            for block in &rebuilt.blocks {
                assert!(
                    seen.insert(block.label),
                    "two blocks of {:?} share {:?} after the rebuild",
                    func.name,
                    block.label
                );
            }
            for child in &func.protos {
                check(child);
            }
        }

        let stmts = compile_source(code).expect("syntax error");
        check(&ast_to_ir(&stmts).main);
    }

    /// The field names the loop conditions read are loop invariant and get
    /// hoisted above the loop. Merged liveness let the allocator hand their
    /// registers to the branch temporaries in the body, so from the second
    /// iteration on the event was indexed with a leftover boolean.
    #[test]
    fn test_ir_hoisted_constants_survive_a_loop_iteration() {
        let code = r##"
            local win = { w = 640, h = 400 }
            function win.size(self) return self.w, self.h end
            function win.clear(self, color) self.last = color end
            function win.text(self, x, y, s) self.label = s end
            function win.present(self) end
            local ball = { x = 320, y = 200, vx = 180, vy = 120, r = 24 }
            local clicks = 0
            local handler = function(events, dt)
                for _, event in events do
                    if event.kind == "keydown" and event.key == "escape" then
                        return "quit"
                    elseif event.kind == "mousedown" and event.button == "left" then
                        ball.x, ball.y = event.x, event.y
                        clicks = clicks + 1
                    end
                end
                local w, h = win:size()
                ball.x = ball.x + ball.vx * dt
                ball.y = ball.y + ball.vy * dt
                if ball.x < ball.r or ball.x > w - ball.r then ball.vx = -ball.vx end
                if ball.y < ball.r or ball.y > h - ball.r then ball.vy = -ball.vy end
                win:clear("#1e1e2e")
                win:text(24, 22, "clicks: " .. clicks)
                win:present()
                return "ran"
            end
            local verdict = handler({
                { kind = "focus" },
                { kind = "mousedown", button = "left", x = 7, y = 9 },
                { kind = "mousemove", x = 1, y = 2 }
            }, 0)
            return verdict .. ":" .. clicks .. ":" .. ball.x .. ":" .. ball.y .. ":" .. win.label
        "##;
        assert_eq!(run_via_ir(code), "ran:1:7:9:clicks: 1");
    }

    /// The same source has to compile to the same bytecode every run: the
    /// passes may not iterate hash sets where the order reaches the output.
    #[test]
    fn test_ir_codegen_is_deterministic() {
        let code = r#"
            local function handle(events, acc)
                for _, event in events do
                    if event.kind == "keydown" and event.key == "escape" then
                        acc = acc .. "q"
                    elseif event.kind == "mousedown" and event.button == "left" then
                        acc = acc .. "c"
                    end
                end
                return acc
            end
            return handle({ { kind = "focus" } }, "")
        "#;
        let stmts = compile_source(code).expect("syntax error");
        let first = format!(
            "{:?}",
            crate::compiler::try_compile_to_proto(&stmts).expect("ir lowering failed")
        );
        for _ in 0..8 {
            let again = format!(
                "{:?}",
                crate::compiler::try_compile_to_proto(&stmts).expect("ir lowering failed")
            );
            assert_eq!(first, again, "codegen differs between runs");
        }
    }

    #[test]
    fn test_ir_register_coalescing_eliminates_redundant_moves() {
        let code = r#"
            local x = 100
            local y = x
            local z = y
            return z
        "#;
        let stmts = compile_source(code).expect("syntax error");
        let proto = crate::compiler::try_compile_to_proto(&stmts).expect("lowering failed");
        let move_count = proto
            .instructions
            .iter()
            .filter(|i| matches!(i, crate::bytecode::instruction::Instruction::Move { .. }))
            .count();
        assert_eq!(
            move_count, 0,
            "All redundant moves should be coalesced away"
        );
        assert_eq!(run_via_ir(code), "100");
    }

    #[test]
    fn test_ir_loop_invariant_code_motion_hoisting() {
        let code = r#"
            local sum = 0
            for i = 1, 10 do
                local c = 42
                sum = sum + c
            end
            return sum
        "#;
        assert_eq!(run_via_ir(code), "420");
    }

    #[test]
    fn test_ir_inlining_small_function_with_branches() {
        let code = r#"
            local function max(a, b)
                if a > b then
                    return a
                else
                    return b
                end
            end
            local x = max(10, 20)
            local y = max(50, 5)
            return x + y
        "#;
        let stmts = compile_source(code).expect("syntax error");
        let proto = crate::compiler::try_compile_to_proto(&stmts).expect("lowering failed");
        let call_count = proto
            .instructions
            .iter()
            .filter(|i| matches!(i, crate::bytecode::instruction::Instruction::Call { .. }))
            .count();
        assert_eq!(call_count, 0, "Calls to max should be fully inlined");
        assert_eq!(run_via_ir(code), "70");
    }

    #[test]
    fn test_ir_inline_function_with_while_loop_not_inlined_into_infinite_loop() {
        let code = r#"
            local function loop_fn(n)
                local i = 0
                while i < n do
                    i = i + 1
                end
                return i
            end
            local res = loop_fn(3)
            return res
        "#;
        assert_eq!(run_via_ir(code), "3");
    }

    #[test]
    fn test_ir_inline_function_with_mutable_vars_across_branches_correctness() {
        let code = r#"
            local function f(cond)
                local x = 1
                if cond then
                    x = 2
                else
                    x = 3
                end
                return x
            end
            local a = f(true)
            local b = f(false)
            return a .. ":" .. b
        "#;
        assert_eq!(run_via_ir(code), "2:3");
    }

    #[test]
    fn test_ir_jump_threading_chained_jumps() {
        let code = r#"
            local x = 10
            local y = 20
            if x < 5 then
                y = 100
            elseif x < 8 then
                y = 200
            else
                y = 300
            end
            return y
        "#;
        assert_eq!(run_via_ir(code), "300");
    }

    #[test]
    fn test_single_acyclic_block_rejects_self_loop_with_stale_edges() {
        let mut cfg = ControlFlowGraph::new(IrLabel(0));
        let mut blk = BasicBlock::new(IrLabel(0));
        blk.add_instruction(IrInst::LoadConst {
            dst: IrVar(1),
            val: IrConstant::Int(BigInt::from(1)),
        });
        // Intentionally leave `successors` empty (stale) even though the block
        // contains a self-loop jump.
        blk.add_instruction(IrInst::Jump(IrLabel(0)));
        cfg.blocks.push(blk);

        assert!(
            !cfg.is_single_acyclic_block(),
            "a single block with a jump instruction must not pass is_single_acyclic_block"
        );
    }

    #[test]
    fn test_codegen_ignores_stale_cached_cfg_when_instructions_change() {
        let code = "local a = 10\nlocal b = 20\nreturn a + b";
        let stmts = compile_source(code).expect("syntax error");
        let mut ir_module = ast_to_ir(&stmts);
        let stale_cfg = build_cfg(&ir_module.main.instructions);
        ir_module.main.cfg = Some(stale_cfg);

        // Mutate `instructions` directly after storing `cfg`: codegen must
        // notice the mismatch and rebuild the CFG for register allocation.
        ir_module.main.instructions = vec![
            IrInst::Label(IrLabel(0)),
            IrInst::LoadConst {
                dst: IrVar(10),
                val: IrConstant::Int(BigInt::from(99)),
            },
            IrInst::LoadConst {
                dst: IrVar(11),
                val: IrConstant::Int(BigInt::from(1)),
            },
            IrInst::BinOp {
                dst: IrVar(12),
                op: IrBinaryOp::Add,
                lhs: IrVar(10),
                rhs: IrVar(11),
            },
            IrInst::Return(vec![IrVar(12)]),
        ];

        assert!(
            !ir_module
                .main
                .cfg
                .as_ref()
                .unwrap()
                .matches_flat_instructions(&ir_module.main.instructions)
        );
        let proto = ir_to_bytecode(&ir_module).expect("codegen failed");
        let mut vm = VM::new();
        let val = vm.execute(proto).expect("exec failed");
        assert_eq!(val.to_string(), "100");
    }

    #[test]
    fn test_simplify_cfg_fuzz_repro_seed_698491900() {
        // Reproduces the `fuzz tree --seed 698491900` crash where LICM + DCE
        // left a chain of empty jump blocks whose later target sat earlier in
        // `cfg.blocks`, causing `eliminate_empty_jump_blocks` to rewrite a
        // predecessor to a deleted block and panic in `LivenessInfo::compute`.
        let code = "local i = 0\n\
while true do\n\
  i += 1\n\
  if i > 5 then break end\n\
  if i % 2 == 0 then continue end\n\
1e-400end\n\
repeat i -= 1 until i <= 0\n\
return i\n";
        let debug = crate::compiler::Options {
            check: false,
            ..crate::compiler::Options::DEBUG
        };
        for options in [crate::compiler::Options::UNCHECKED, debug] {
            let proto =
                crate::compiler::compile_program(code, "fuzz_repro", options).expect("compile");
            let bytes = crate::bytecode::serialize(&proto);
            let loaded = crate::bytecode::deserialize(&bytes).expect("valid bytecode");
            let mut vm = VM::new();
            assert_eq!(vm.execute(loaded).expect("run").to_string(), "0");
        }
    }

    #[test]
    fn test_simplify_cfg_chained_empty_jump_blocks_reverse_order() {
        // `L2 -> L3` sits before `L1 -> L2` in `cfg.blocks`. Eliminating `L2`
        // first updates `L1` to jump to `L3` and deletes `L2`; eliminating `L1`
        // next must follow `L1`'s updated target `L3` instead of the deleted `L2`.
        let mut cfg = ControlFlowGraph::new(IrLabel(0));

        let mut b0 = BasicBlock::new(IrLabel(0));
        b0.instructions.push(IrInst::LoadConst {
            dst: IrVar(0),
            val: IrConstant::Int(BigInt::from(7)),
        });
        b0.instructions.push(IrInst::Jump(IrLabel(1)));
        b0.successors = vec![IrLabel(1)];

        let mut b2 = BasicBlock::new(IrLabel(2));
        b2.instructions.push(IrInst::Jump(IrLabel(3)));
        b2.predecessors = vec![IrLabel(1)];
        b2.successors = vec![IrLabel(3)];

        let mut b1 = BasicBlock::new(IrLabel(1));
        b1.instructions.push(IrInst::Jump(IrLabel(2)));
        b1.predecessors = vec![IrLabel(0)];
        b1.successors = vec![IrLabel(2)];

        let mut b3 = BasicBlock::new(IrLabel(3));
        b3.instructions.push(IrInst::Return(vec![IrVar(0)]));
        b3.predecessors = vec![IrLabel(2)];

        cfg.blocks = vec![b0, b2, b1, b3];
        assert!(verify_cfg(&cfg).is_ok());

        simplify_cfg(&mut cfg);
        assert_eq!(verify_cfg(&cfg), Ok(()));
        assert!(cfg.find_block(IrLabel(1)).is_none());
        assert!(cfg.find_block(IrLabel(2)).is_none());
        assert_eq!(cfg.blocks.len(), 1);
    }

    #[test]
    fn test_simplify_cfg_empty_jump_cycle_preserved() {
        // A 2-block empty cycle `L1 -> L2 -> L1` collapses to a single
        // self-loop block; the second block must not bypass itself to deleted `L1`.
        let mut cfg = ControlFlowGraph::new(IrLabel(0));

        let mut b0 = BasicBlock::new(IrLabel(0));
        b0.instructions.push(IrInst::Jump(IrLabel(1)));
        b0.successors = vec![IrLabel(1)];

        let mut b1 = BasicBlock::new(IrLabel(1));
        b1.instructions.push(IrInst::Jump(IrLabel(2)));
        b1.predecessors = vec![IrLabel(0), IrLabel(2)];
        b1.successors = vec![IrLabel(2)];

        let mut b2 = BasicBlock::new(IrLabel(2));
        b2.instructions.push(IrInst::Jump(IrLabel(1)));
        b2.predecessors = vec![IrLabel(1)];
        b2.successors = vec![IrLabel(1)];

        cfg.blocks = vec![b0, b1, b2];
        assert!(verify_cfg(&cfg).is_ok());

        simplify_cfg(&mut cfg);
        assert_eq!(verify_cfg(&cfg), Ok(()));
        assert!(
            cfg.blocks.iter().any(|b| b.successors.contains(&b.label)),
            "cycle must remain a valid self-loop"
        );
    }

    #[test]
    fn test_simplify_cfg_conditional_both_branches_same_target_does_not_mismerge() {
        // Bypassing an empty `then` block leaves `JumpIfFalse(cond, L2); Jump(L2)`.
        // Deduplicating successors must not cause `merge_single_edge_blocks` to
        // merge `L0` and `L2` and turn `JumpIfFalse` into a loop back to `L0`.
        let mut cfg = ControlFlowGraph::new(IrLabel(0));

        let mut b0 = BasicBlock::new(IrLabel(0));
        b0.instructions.push(IrInst::JumpIfFalse {
            cond: IrVar(0),
            target: IrLabel(2),
        });
        b0.instructions.push(IrInst::Jump(IrLabel(1)));
        b0.successors = vec![IrLabel(2), IrLabel(1)];

        let mut b1 = BasicBlock::new(IrLabel(1));
        b1.instructions.push(IrInst::Jump(IrLabel(2)));
        b1.predecessors = vec![IrLabel(0)];
        b1.successors = vec![IrLabel(2)];

        let mut b2 = BasicBlock::new(IrLabel(2));
        b2.instructions.push(IrInst::Return(vec![IrVar(0)]));
        b2.predecessors = vec![IrLabel(0), IrLabel(1)];

        cfg.blocks = vec![b0, b1, b2];
        assert!(verify_cfg(&cfg).is_ok());

        simplify_cfg(&mut cfg);
        assert_eq!(verify_cfg(&cfg), Ok(()));
        let entry = cfg.find_block(IrLabel(0)).expect("entry exists");
        assert_eq!(entry.successors, vec![IrLabel(2)]);

        // End-to-end execution must terminate and return the right value for both arms.
        let code = "local function f(x)\n  local n = 0\n  if x then end\n  n += 1\n  return n\nend\nreturn f(false) + f(true)";
        assert_eq!(run_via_ir(code), "2");
    }

    #[test]
    fn test_liveness_empty_cfg_does_not_panic() {
        let cfg = ControlFlowGraph::new(IrLabel(0));
        let liveness = LivenessInfo::compute(&cfg);
        assert!(liveness.blocks.is_empty());
    }

    #[test]
    fn test_licm_merges_shared_header_loops_with_continue() {
        // A loop with `continue` has multiple back-edges to the same header.
        // `find_natural_loops` must merge them into a single `NaturalLoop` so:
        // 1. Constants hoisted to the preheader stay outside the loop rather
        //    than being absorbed back into the header and ping-ponging across
        //    all 16 fixpoint rounds.
        // 2. Variables modified after a `continue` branch are included in the
        //    merged loop's `loop_defs` and are never mis-hoisted.
        let code = r#"
            local x = 10
            local i = 0
            local sum = 0
            while i < 4 do
                i += 1
                local y = x + 777
                sum += y
                if i % 2 == 0 then
                    continue
                end
                x += 100
            end
            return sum
        "#;
        let stmts = compile_source(code).expect("parse");
        let ir_module = ast_to_ir(&stmts);
        let mut cfg = build_cfg(&ir_module.main.instructions);
        let dom = ImmediateDominators::build(&cfg);
        let initial_loops =
            crate::compiler::ir::opt::loop_invariant::find_natural_loops(&cfg, &dom);
        assert_eq!(
            initial_loops.len(),
            1,
            "multiple back-edges to the same header must be merged into one NaturalLoop"
        );

        optimize_cfg(&mut cfg);
        assert_eq!(verify_cfg(&cfg), Ok(()));

        // After `optimize_cfg`, LICM must have truly converged (no further
        // hoisting possible) and all loop-invariant `LoadConst` instructions
        // must live outside the loop body.
        let dom_after = ImmediateDominators::build(&cfg);
        let loops_after =
            crate::compiler::ir::opt::loop_invariant::find_natural_loops(&cfg, &dom_after);
        assert_eq!(loops_after.len(), 1);
        for lbl in &loops_after[0].body {
            let blk = cfg.find_block(*lbl).expect("loop block exists");
            assert!(
                !blk.instructions
                    .iter()
                    .any(|i| matches!(i, IrInst::LoadConst { .. })),
                "loop-invariant LoadConst remained inside loop block {:?}: {:?}",
                lbl,
                blk.instructions
            );
        }
        assert!(
            !loop_invariant_code_motion(&mut cfg, &dom_after),
            "LICM must be at a fixpoint after optimize_cfg"
        );

        // Iter 1 (i=1): y = 10 + 777 = 787, sum = 787, i%2!=0 -> x = 110
        // Iter 2 (i=2): y = 110 + 777 = 887, sum = 1674, i%2==0 -> continue (x stays 110)
        // Iter 3 (i=3): y = 110 + 777 = 887, sum = 2561, i%2!=0 -> x = 210
        // Iter 4 (i=4): y = 210 + 777 = 987, sum = 3548, i%2==0 -> continue
        assert_eq!(run_via_ir(code), "3548");
    }
}
