//! Prints IR for `neyuki ir`.

use std::fmt::Write;

use crate::compiler::ir::block::{BasicBlock, ControlFlowGraph, IrFunction, IrModule};
use crate::compiler::ir::inst::IrInst;
use crate::compiler::ir::types::{IrBinaryOp, IrConstant, IrLabel, IrUnaryOp, IrVar};

pub fn format_var(v: IrVar) -> String {
    format!("v{}", v.0)
}

pub fn format_label(l: IrLabel) -> String {
    format!("L{}", l.0)
}

pub fn print_module(module: &IrModule) -> String {
    let mut out = String::new();
    print_function(&module.main, 0, &mut out);
    out
}

pub fn print_function(func: &IrFunction, indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    let name = func.name.as_deref().unwrap_or("<anonymous>");
    let _ = writeln!(
        out,
        "{}function {}(params: {}, vararg: {}) {{",
        pad, name, func.num_params, func.is_vararg
    );

    if let Some(cfg) = &func.cfg {
        print_cfg(cfg, indent + 1, out);
    } else {
        for inst in &func.instructions {
            let _ = writeln!(out, "{}  {}", pad, format_inst(inst));
        }
    }

    for proto in &func.protos {
        let _ = writeln!(out);
        print_function(proto, indent + 1, out);
    }

    let _ = writeln!(out, "{}}}", pad);
}

pub fn print_cfg(cfg: &ControlFlowGraph, indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    let _ = writeln!(out, "{}entry: L{}", pad, cfg.entry_label.0);

    for block in &cfg.blocks {
        print_block(block, indent, out);
    }
}

pub fn print_block(block: &BasicBlock, indent: usize, out: &mut String) {
    let pad = "  ".repeat(indent);
    let preds: Vec<String> = block
        .predecessors
        .iter()
        .map(|p| format!("L{}", p.0))
        .collect();
    let succs: Vec<String> = block
        .successors
        .iter()
        .map(|s| format!("L{}", s.0))
        .collect();

    let _ = writeln!(
        out,
        "{}block L{}: [preds: {}, succs: {}]",
        pad,
        block.label.0,
        preds.join(", "),
        succs.join(", ")
    );

    for inst in &block.instructions {
        let _ = writeln!(out, "{}  {}", pad, format_inst(inst));
    }
}

pub fn format_inst(inst: &IrInst) -> String {
    match inst {
        IrInst::LoadConst { dst, val } => format!("v{} = const {}", dst.0, format_const(val)),
        IrInst::LoadNil { dst } => format!("v{} = nil", dst.0),
        IrInst::Move { dst, src } => format!("v{} = v{}", dst.0, src.0),
        IrInst::BinOp { dst, op, lhs, rhs } => {
            format!("v{} = v{} {} v{}", dst.0, lhs.0, format_binop(*op), rhs.0)
        }
        IrInst::UnOp { dst, op, src } => {
            format!("v{} = {}v{}", dst.0, format_unop(*op), src.0)
        }
        IrInst::NewTable {
            dst,
            array_cap,
            fields_cap,
        } => {
            if *array_cap == 0 && *fields_cap == 0 {
                format!("v{} = new_table()", dst.0)
            } else {
                format!(
                    "v{} = new_table(arr={}, fields={})",
                    dst.0, array_cap, fields_cap
                )
            }
        }
        IrInst::Line(n) => format!("; line {}", n),
        IrInst::DebugLocal { name, var } => format!("; local {} = v{}", name, var.0),
        IrInst::DebugScopeEnd { vars, .. } => format!(
            "; end of scope {}",
            vars.iter()
                .map(|v| format!("v{}", v.0))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        IrInst::GetTable { dst, table, key } => {
            format!("v{} = v{}[v{}]", dst.0, table.0, key.0)
        }
        IrInst::SetTable { table, key, val } => {
            format!("v{}[v{}] = v{}", table.0, key.0, val.0)
        }
        IrInst::AppendArray { table, src } => {
            format!("append(v{}, v{})", table.0, src.0)
        }
        IrInst::GetGlobal { dst, name } => format!("v{} = global {}", dst.0, name),
        IrInst::GetImport { dst, module, field } => {
            format!("v{} = import {}.{}", dst.0, module, field)
        }
        IrInst::SetGlobal { name, src } => format!("global {} = v{}", name, src.0),
        IrInst::GetUpval { dst, index } => format!("v{} = upval[{}]", dst.0, index),
        IrInst::SetUpval { index, src } => format!("upval[{}] = v{}", index, src.0),
        IrInst::Call {
            dsts,
            callee,
            args,
            retc,
        } => {
            let args_str: Vec<String> = args.iter().map(|a| format!("v{}", a.0)).collect();
            let call = format!(
                "call v{}({}) [retc: {}]",
                callee.0,
                args_str.join(", "),
                retc
            );
            if dsts.is_empty() {
                call
            } else {
                let dsts_str: Vec<String> = dsts.iter().map(|d| format!("v{}", d.0)).collect();
                format!("{} = {}", dsts_str.join(", "), call)
            }
        }
        IrInst::Spread { source, sink } => {
            let src = match source {
                crate::compiler::ir::inst::SpreadSource::Call { callee, args, .. } => {
                    let args_str: Vec<String> = args.iter().map(|a| format!("v{}", a.0)).collect();
                    format!("v{}({})...", callee.0, args_str.join(", "))
                }
                crate::compiler::ir::inst::SpreadSource::Vararg => "...".to_string(),
            };
            match sink {
                crate::compiler::ir::inst::SpreadSink::Call {
                    dsts,
                    callee,
                    fixed_args,
                    retc,
                } => {
                    let mut all: Vec<String> =
                        fixed_args.iter().map(|a| format!("v{}", a.0)).collect();
                    all.push(src);
                    let call = format!("call v{}({}) [retc: {}]", callee.0, all.join(", "), retc);
                    if dsts.is_empty() {
                        call
                    } else {
                        let dsts_str: Vec<String> =
                            dsts.iter().map(|d| format!("v{}", d.0)).collect();
                        format!("{} = {}", dsts_str.join(", "), call)
                    }
                }
                crate::compiler::ir::inst::SpreadSink::List { table } => {
                    format!("append_all(v{}, {})", table.0, src)
                }
                crate::compiler::ir::inst::SpreadSink::Return { values } => {
                    let mut all: Vec<String> = values.iter().map(|v| format!("v{}", v.0)).collect();
                    all.push(src);
                    format!("return {}", all.join(", "))
                }
                crate::compiler::ir::inst::SpreadSink::Values { dsts } => {
                    let dsts_str: Vec<String> = dsts.iter().map(|d| format!("v{}", d.0)).collect();
                    format!("{} = {}", dsts_str.join(", "), src)
                }
            }
        }
        IrInst::Return(vars) => {
            let vars_str: Vec<String> = vars.iter().map(|v| format!("v{}", v.0)).collect();
            format!("return {}", vars_str.join(", "))
        }
        IrInst::Closure { dst, proto_idx, .. } => {
            format!("v{} = closure(proto #{})", dst.0, proto_idx)
        }
        IrInst::Vararg { dst, count } => format!("v{} = vararg({})", dst.0, count),
        IrInst::ForPrep {
            base,
            limit,
            step,
            loop_var,
            jump,
        } => {
            format!(
                "for_prep v{}, v{}, v{}, loop_var: v{}, jump: L{}",
                base.0, limit.0, step.0, loop_var.0, jump.0
            )
        }
        IrInst::ForLoop { base, jump } => format!("for_loop v{}, jump: L{}", base.0, jump.0),
        IrInst::TForCall {
            base,
            state,
            ctrl,
            vars,
        } => {
            let vars_str: Vec<String> = vars.iter().map(|v| format!("v{}", v.0)).collect();
            format!(
                "tfor_call v{}, v{}, v{}, vars: [{}]",
                base.0,
                state.0,
                ctrl.0,
                vars_str.join(", ")
            )
        }
        IrInst::TForLoop { base, jump } => format!("tfor_loop v{}, jump: L{}", base.0, jump.0),
        IrInst::CloseUpvals { vars } => {
            let vars_str: Vec<String> = vars.iter().map(|v| format!("v{}", v.0)).collect();
            format!("close_upvals [{}]", vars_str.join(", "))
        }
        IrInst::Jump(lbl) => format!("jump L{}", lbl.0),
        IrInst::JumpIfFalse { cond, target } => format!("jump_if_false v{}, L{}", cond.0, target.0),
        IrInst::Label(lbl) => format!("label L{}:", lbl.0),
        IrInst::Phi { dst, incoming } => {
            let inc_str: Vec<String> = incoming
                .iter()
                .map(|(lbl, v)| format!("[L{}: v{}]", lbl.0, v.0))
                .collect();
            format!("v{} = phi({})", dst.0, inc_str.join(", "))
        }
    }
}

fn format_const(c: &IrConstant) -> String {
    match c {
        IrConstant::Nil => "nil".to_string(),
        IrConstant::Bool(b) => b.to_string(),
        IrConstant::Int(i) => i.to_string(),
        IrConstant::Float(f) => f.to_string(),
        IrConstant::String(s) => format!("{:?}", s),
    }
}

fn format_binop(op: IrBinaryOp) -> &'static str {
    match op {
        IrBinaryOp::Add => "+",
        IrBinaryOp::Sub => "-",
        IrBinaryOp::Mul => "*",
        IrBinaryOp::Div => "/",
        IrBinaryOp::IDiv => "//",
        IrBinaryOp::Mod => "%",
        IrBinaryOp::Pow => "^",
        IrBinaryOp::BitAnd => "&",
        IrBinaryOp::BitOr => "|",
        IrBinaryOp::BitXor => "~",
        IrBinaryOp::Shl => "<<",
        IrBinaryOp::Shr => ">>",
        IrBinaryOp::LShl => "<<<",
        IrBinaryOp::LShr => ">>>",
        IrBinaryOp::Concat => "..",
        IrBinaryOp::Eq => "==",
        IrBinaryOp::Ne => "!=",
        IrBinaryOp::Lt => "<",
        IrBinaryOp::Le => "<=",
        IrBinaryOp::Gt => ">",
        IrBinaryOp::Ge => ">=",
        IrBinaryOp::Coalesce => "??",
    }
}

fn format_unop(op: IrUnaryOp) -> &'static str {
    match op {
        IrUnaryOp::Neg => "-",
        IrUnaryOp::Not => "not ",
        IrUnaryOp::Len => "#",
        IrUnaryOp::BitNot => "~",
    }
}
