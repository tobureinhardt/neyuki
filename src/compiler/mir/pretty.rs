//! Human-readable MIR printer (`neyuki dump-mir`).

use std::fmt::Write;

use crate::compiler::mir::cfg::{
    BasicBlockData, LocalKind, MirBody, MirConst, MirProgram, MirSpreadSource, Operand, Place,
    Projection, Rvalue, Statement, StatementKind, TerminatorKind,
};

/// Formats an entire [`MirProgram`] into indented textual form.
pub fn dump_mir(program: &MirProgram) -> String {
    let mut out = String::from("=== MIR ===\n");
    write_body(&mut out, &program.main, 0);
    out
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_body(out: &mut String, body: &MirBody, depth: usize) {
    indent(out, depth);
    let _ = writeln!(
        out,
        "fn {}(params={}, vararg={}) -> {} {{",
        body.name.as_deref().unwrap_or("<anon>"),
        body.num_params,
        body.is_vararg,
        body.return_type.display_name()
    );

    for decl in &body.locals {
        indent(out, depth + 1);
        let k = match decl.kind {
            LocalKind::Arg => "arg",
            LocalKind::Var => "var",
            LocalKind::Temp => "tmp",
        };
        let nm = decl
            .name
            .as_deref()
            .map(|n| format!(" ({})", n))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "let _{}: {} [{}{}]",
            decl.id.0,
            decl.ty.display_name(),
            k,
            nm
        );
    }

    for bb in &body.blocks {
        write_block(out, bb, depth + 1);
    }

    for child in &body.functions {
        write_body(out, child, depth + 1);
    }

    indent(out, depth);
    out.push_str("}\n");
}

fn write_block(out: &mut String, bb: &BasicBlockData, depth: usize) {
    indent(out, depth);
    let _ = writeln!(out, "bb{}:", bb.id.0);
    for stmt in &bb.statements {
        write_stmt(out, stmt, depth + 1);
    }
    indent(out, depth + 1);
    write_term(out, &bb.terminator);
    out.push('\n');
}

fn write_stmt(out: &mut String, stmt: &Statement, depth: usize) {
    indent(out, depth);
    if stmt.line > 0 {
        let _ = write!(out, "[L{}] ", stmt.line);
    }
    match &stmt.kind {
        StatementKind::Assign(place, rvalue) => {
            write_place(out, place);
            out.push_str(" = ");
            write_rvalue(out, rvalue);
            out.push('\n');
        }
        StatementKind::SetGlobal { name, value } => {
            let _ = write!(out, "global[{}] = ", name);
            write_operand(out, value);
            out.push('\n');
        }
        StatementKind::SetUpval { index, value } => {
            let _ = write!(out, "upval[{}] = ", index);
            write_operand(out, value);
            out.push('\n');
        }
        StatementKind::AppendArray { table, value } => {
            let _ = write!(out, "append _{}, ", table.0);
            write_operand(out, value);
            out.push('\n');
        }
        StatementKind::CloseUpvals(vars) => {
            let ids: Vec<String> = vars.iter().map(|v| format!("_{}", v.0)).collect();
            let _ = writeln!(out, "close_upvals [{}]", ids.join(", "));
        }
        StatementKind::DebugLocal { name, local } => {
            let _ = writeln!(out, "debug_local {} = _{}", name, local.0);
        }
        StatementKind::DebugScopeEnd { locals, keep_alive } => {
            let ids: Vec<String> = locals.iter().map(|v| format!("_{}", v.0)).collect();
            let _ = writeln!(
                out,
                "debug_scope_end [{}] keep={}",
                ids.join(", "),
                keep_alive
            );
        }
    }
}

fn write_place(out: &mut String, place: &Place) {
    let _ = write!(out, "_{}", place.local.0);
    for proj in &place.projections {
        match proj {
            Projection::Field(f) => {
                let _ = write!(out, ".{}", f);
            }
            Projection::Index(idx) => {
                out.push('[');
                write_operand(out, idx);
                out.push(']');
            }
        }
    }
}

fn write_const(out: &mut String, c: &MirConst) {
    match c {
        MirConst::Nil => out.push_str("nil"),
        MirConst::Bool(b) => {
            let _ = write!(out, "{}", b);
        }
        MirConst::Int(i) => {
            let _ = write!(out, "{}", i);
        }
        MirConst::Float(f) => {
            let _ = write!(out, "{}", f);
        }
        MirConst::String(s) => {
            let _ = write!(out, "{:?}", s);
        }
    }
}

fn write_operand(out: &mut String, op: &Operand) {
    match op {
        Operand::Copy(p) => write_place(out, p),
        Operand::Move(p) => {
            out.push_str("move ");
            write_place(out, p);
        }
        Operand::Constant(c) => write_const(out, c),
    }
}

fn write_rvalue(out: &mut String, rv: &Rvalue) {
    match rv {
        Rvalue::Use(op) => write_operand(out, op),
        Rvalue::BinaryOp { op, lhs, rhs } => {
            let _ = write!(out, "{:?}(", op);
            write_operand(out, lhs);
            out.push_str(", ");
            write_operand(out, rhs);
            out.push(')');
        }
        Rvalue::UnaryOp { op, src } => {
            let _ = write!(out, "{:?}(", op);
            write_operand(out, src);
            out.push(')');
        }
        Rvalue::Global(name) => {
            let _ = write!(out, "global[{}]", name);
        }
        Rvalue::Upvalue(idx) => {
            let _ = write!(out, "upval[{}]", idx);
        }
        Rvalue::ModuleImport { module, field } => {
            let _ = write!(out, "import({}.{})", module, field);
        }
        Rvalue::NewTable {
            array_cap,
            fields_cap,
        } => {
            let _ = write!(out, "new_table(arr={}, map={})", array_cap, fields_cap);
        }
        Rvalue::Closure { fn_idx, captures } => {
            let caps: Vec<String> = captures
                .iter()
                .map(|c| {
                    c.map(|l| format!("_{}", l.0))
                        .unwrap_or_else(|| "up".into())
                })
                .collect();
            let _ = write!(out, "closure(fn#{}, [{}])", fn_idx, caps.join(", "));
        }
        Rvalue::Vararg => out.push_str("vararg"),
    }
}

fn write_spread(out: &mut String, src: &MirSpreadSource) {
    match src {
        MirSpreadSource::Vararg => out.push_str("..."),
        MirSpreadSource::Call {
            callee,
            args,
            trailing,
        } => {
            write_operand(out, callee);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_operand(out, a);
            }
            if let Some(t) = trailing {
                if !args.is_empty() {
                    out.push_str(", ");
                }
                write_spread(out, t);
            }
            out.push(')');
        }
    }
}

fn write_term(out: &mut String, term: &TerminatorKind) {
    match term {
        TerminatorKind::Goto(bb) => {
            let _ = write!(out, "goto -> bb{}", bb.0);
        }
        TerminatorKind::SwitchBool {
            cond,
            true_bb,
            false_bb,
        } => {
            out.push_str("switchBool(");
            write_operand(out, cond);
            let _ = write!(out, ") -> [true: bb{}, false: bb{}]", true_bb.0, false_bb.0);
        }
        TerminatorKind::Call {
            dsts,
            callee,
            args,
            target,
            ..
        } => {
            let d: Vec<String> = dsts.iter().map(|l| format!("_{}", l.0)).collect();
            let _ = write!(out, "({}) = call ", d.join(", "));
            write_operand(out, callee);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_operand(out, a);
            }
            let _ = write!(out, ") -> bb{}", target.0);
        }
        TerminatorKind::SpreadCall {
            dsts,
            callee,
            fixed_args,
            trailing,
            target,
            ..
        } => {
            let d: Vec<String> = dsts.iter().map(|l| format!("_{}", l.0)).collect();
            let _ = write!(out, "({}) = spread_call ", d.join(", "));
            write_operand(out, callee);
            out.push('(');
            for (i, a) in fixed_args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_operand(out, a);
            }
            if !fixed_args.is_empty() {
                out.push_str(", ");
            }
            write_spread(out, trailing);
            let _ = write!(out, ") -> bb{}", target.0);
        }
        TerminatorKind::SpreadList {
            table,
            source,
            target,
        } => {
            let _ = write!(out, "spread_list _{} <- ", table.0);
            write_spread(out, source);
            let _ = write!(out, " -> bb{}", target.0);
        }
        TerminatorKind::SpreadReturn(src) => {
            out.push_str("spread_return ");
            write_spread(out, src);
        }
        TerminatorKind::ForPrep {
            base,
            loop_var,
            test_bb,
            body_bb,
            ..
        } => {
            let _ = write!(
                out,
                "for_prep(base=_{}, var=_{}) -> [test: bb{}, body: bb{}]",
                base.0, loop_var.0, test_bb.0, body_bb.0
            );
        }
        TerminatorKind::ForLoop {
            base,
            body_bb,
            exit_bb,
        } => {
            let _ = write!(
                out,
                "for_loop(base=_{}) -> [body: bb{}, exit: bb{}]",
                base.0, body_bb.0, exit_bb.0
            );
        }
        TerminatorKind::TForStep {
            base,
            vars,
            body_bb,
            exit_bb,
            ..
        } => {
            let vs: Vec<String> = vars.iter().map(|v| format!("_{}", v.0)).collect();
            let _ = write!(
                out,
                "tfor_step(base=_{}, [{}]) -> [body: bb{}, exit: bb{}]",
                base.0,
                vs.join(", "),
                body_bb.0,
                exit_bb.0
            );
        }
        TerminatorKind::Return(vals) => {
            out.push_str("return ");
            for (i, v) in vals.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_operand(out, v);
            }
        }
        TerminatorKind::Unreachable => out.push_str("unreachable"),
    }
}
