//! `neyuki decompile`: turns bytecode back into Neyuki source.
//!
//! Bytecode keeps the names of locals, parameters and upvalues as debug
//! information, but not comments, formatting, type annotations or the
//! temporaries the compiler introduced, so the output is equivalent source
//! rather than the original text. Each function goes through these steps:
//!
//! 1. [`flow`] splits the code into basic blocks and finds the loops,
//!    dominators and the registers live into each block.
//! 2. [`vars`] splits every register into the separate variables it holds
//!    and names them.
//! 3. [`lift`] turns each block into statements, folding temporaries back
//!    into expressions, while [`structure`] nests the blocks into `if`,
//!    `while` and `for` statements.
//! 4. [`scope`] places the `local` declarations.
//!
//! A function whose code does not fit those shapes (hand-written or damaged
//! bytecode, say) keeps its signature, and its body becomes its disassembly
//! in a comment followed by a call to `error`, so the rest of the chunk
//! still decompiles.

mod ast;
mod flow;
mod lift;
mod scope;
mod structure;
mod vars;

#[cfg(test)]
mod tests;

use std::collections::HashSet;

use ast::{Body, Expr, Func, Printer, Stmt};
use flow::Flow;
use vars::{Vars, unique};

use crate::bytecode::instruction::Instruction;
use crate::bytecode::proto::{Constant, Proto};

/// Words that cannot name a variable. (`type` and `export` are keywords
/// only at the start of a declaration, so they can.)
const KEYWORDS: &[&str] = &[
    "and", "break", "const", "continue", "defer", "do", "else", "elseif", "end", "false", "for",
    "function", "global", "if", "in", "local", "nil", "not", "or", "repeat", "return", "then",
    "true", "until", "while",
];

/// Globals the output itself may use.
const USED_GLOBALS: &[&str] = &["_", "error", "math", "table"];

/// Whether `s` can be written as a plain name.
fn is_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !KEYWORDS.contains(&s)
}

/// What decompiling one function needs to know.
struct FnCx<'a> {
    proto: &'a Proto,
    flow: Flow,
    vars: Vars,
    /// Names of this function's variables, by id.
    names: Vec<String>,
    /// Names of this function's upvalues, by index.
    upvalues: Vec<String>,
    /// Names nested functions must not reuse: the enclosing functions'
    /// variables, this function's, and every global the chunk uses.
    child_taken: HashSet<String>,
}

/// Decompiles a chunk (the main function of a compiled file) to source.
pub fn decompile(proto: &Proto) -> String {
    let mut taken: HashSet<String> = KEYWORDS
        .iter()
        .chain(USED_GLOBALS)
        .map(|s| s.to_string())
        .collect();
    collect_globals(proto, &mut taken);
    let upvalues: Vec<String> = (0..proto.upvalues.len())
        .map(|i| {
            proto
                .upvalue_names
                .get(i)
                .filter(|n| is_name(n))
                .cloned()
                .unwrap_or_else(|| format!("upvalue{}", i + 1))
        })
        .collect();
    let main = match try_decompile(proto, upvalues, &taken, true) {
        Ok(func) => func,
        Err(reason) => lost(proto, &taken, reason),
    };

    let mut out = format!(
        "-- Decompiled by neyuki {} from {}.\n\
         -- Comments, formatting, type annotations and the names of temporaries are not\n\
         -- kept in bytecode, so this is equivalent source rather than the original.\n\n",
        env!("CARGO_PKG_VERSION"),
        proto.source.as_deref().unwrap_or("bytecode"),
    );
    let mut printer = Printer::default();
    match &main.body {
        Body::Code(stmts) => printer.block(&mut out, stmts, &main.names, 0),
        Body::Lost { .. } => {
            let text = printer.expr(&Expr::Function(Box::new(main.clone())), &main.names, 0);
            out.push_str(&format!("local main = {}\nreturn main(...)\n", text));
        }
    }
    out
}

/// Every global name the chunk reads or writes, so no variable shadows one.
fn collect_globals(proto: &Proto, names: &mut HashSet<String>) {
    for inst in &proto.instructions {
        let k = match *inst {
            Instruction::GetGlobal { name_k, .. } | Instruction::SetGlobal { name_k, .. } => name_k,
            Instruction::GetImport { mod_k, .. } => mod_k,
            _ => continue,
        };
        if let Some(Constant::String(name)) = proto.constants.get(usize::from(k)) {
            names.insert(name.clone());
        }
    }
    for child in &proto.protos {
        collect_globals(child, names);
    }
}

/// Decompiles one function. `upvalues` names its upvalues, and `taken` are
/// the names it must not give its own variables.
fn decompile_function(proto: &Proto, upvalues: Vec<String>, taken: &HashSet<String>) -> Func {
    match try_decompile(proto, upvalues, taken, false) {
        Ok(func) => func,
        Err(reason) => lost(proto, taken, reason),
    }
}

fn try_decompile(
    proto: &Proto,
    upvalues: Vec<String>,
    taken: &HashSet<String>,
    main: bool,
) -> Result<Func, String> {
    let flow = Flow::analyze(proto)?;
    let vars = Vars::build(proto, &flow);
    let names = vars.names(taken);
    let mut child_taken = taken.clone();
    child_taken.extend(names.iter().cloned());
    let params: Vec<String> = (0..usize::from(proto.num_params))
        .map(|i| {
            vars.param(i)
                .map(|v| names[v].clone())
                .ok_or_else(|| format!("parameter {} has no variable", i + 1))
        })
        .collect::<Result<_, _>>()?;
    let cx = FnCx {
        proto,
        flow,
        vars,
        names,
        upvalues,
        child_taken,
    };
    let mut body = structure::structure(&cx)?;
    scope::declare(&mut body, &cx.vars);
    // A function ends with a bare `return`, and the main chunk with
    // `return nil`; neither needs writing.
    if let Some(Stmt::Return(vals)) = body.last()
        && vals.iter().all(|v| *v == Expr::Nil)
        && (vals.is_empty() || main)
    {
        body.pop();
    }
    Ok(Func {
        params,
        vararg: proto.is_vararg,
        body: Body::Code(body),
        names: cx.names,
        captures: Vec::new(),
    })
}

/// A function whose body could not be recovered.
fn lost(proto: &Proto, taken: &HashSet<String>, reason: String) -> Func {
    let mut used: HashSet<String> = HashSet::new();
    let params = (0..proto.num_params)
        .map(|i| {
            let debug = proto
                .local_names
                .iter()
                .find(|l| l.reg == i && l.from_pc == 0 && is_name(&l.name))
                .map(|l| l.name.clone())
                .unwrap_or_else(|| format!("arg{}", i + 1));
            let name = unique(&debug, |n| !taken.contains(n) && !used.contains(n));
            used.insert(name.clone());
            name
        })
        .collect();
    Func {
        params,
        vararg: proto.is_vararg,
        body: Body::Lost {
            reason,
            listing: crate::bytecode::disasm::disassemble_proto(proto, 0),
        },
        names: Vec::new(),
        captures: Vec::new(),
    }
}
