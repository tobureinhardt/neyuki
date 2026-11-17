//! Prints bytecode as readable assembly.

use crate::bytecode::instruction::Instruction;
use crate::bytecode::proto::{Constant, Proto};
use std::fmt::Write;

pub fn disassemble_proto(proto: &Proto, indent: usize) -> String {
    let mut out = String::new();
    let pad = "  ".repeat(indent);

    let name_str = proto.name.as_deref().unwrap_or("anonymous");
    let _ = writeln!(
        out,
        "{}.proto {} (params: {}, registers: {}, vararg: {})",
        pad, name_str, proto.num_params, proto.max_registers, proto.is_vararg
    );

    // List constant pool
    for (i, c) in proto.constants.iter().enumerate() {
        match c {
            Constant::Nil => {
                let _ = writeln!(out, "{}  .const K{} = nil", pad, i);
            }
            Constant::Bool(b) => {
                let _ = writeln!(out, "{}  .const K{} = boolean {}", pad, i, b);
            }
            Constant::Int(bi) => {
                let _ = writeln!(out, "{}  .const K{} = int {}", pad, i, bi);
            }
            Constant::Float(f) => {
                let _ = writeln!(out, "{}  .const K{} = float {}", pad, i, f);
            }
            Constant::String(s) => {
                let _ = writeln!(out, "{}  .const K{} = string {:?}", pad, i, s);
            }
        }
    }

    // List upvalues
    for (i, up) in proto.upvalues.iter().enumerate() {
        let _ = writeln!(
            out,
            "{}  .upval U{} = {}[{}]",
            pad,
            i,
            if up.in_stack { "stack" } else { "parent_upval" },
            up.index
        );
    }

    // List instructions
    for (ip, inst) in proto.instructions.iter().enumerate() {
        let line = proto.lines.get(ip).copied().unwrap_or(0);
        let _ = writeln!(
            out,
            "{}  {:04} [L{:03}] {}",
            pad,
            ip,
            line,
            format_instruction(inst)
        );
    }

    // Disassemble nested protos
    for child in &proto.protos {
        let _ = writeln!(out);
        out.push_str(&disassemble_proto(child, indent + 1));
    }

    out
}

fn format_instruction(inst: &Instruction) -> String {
    match inst {
        Instruction::LoadNil { dst } => format!("LOADNIL      R{}", dst),
        Instruction::LoadBool { dst, val } => format!("LOADBOOL     R{}, {}", dst, val),
        Instruction::LoadInt { dst, val } => format!("LOADINT      R{}, {}", dst, val),
        Instruction::LoadK { dst, k } => format!("LOADK        R{}, K{}", dst, k),
        Instruction::Move { dst, src } => format!("MOVE         R{}, R{}", dst, src),
        Instruction::GetGlobal { dst, name_k } => format!("GETGLOBAL    R{}, K{}", dst, name_k),
        Instruction::SetGlobal { src, name_k } => format!("SETGLOBAL    K{}, R{}", name_k, src),
        Instruction::GetUpval { dst, upval_idx } => {
            format!("GETUPVAL     R{}, U{}", dst, upval_idx)
        }
        Instruction::SetUpval { src, upval_idx } => {
            format!("SETUPVAL     U{}, R{}", upval_idx, src)
        }
        Instruction::NewTable {
            dst,
            array_cap,
            fields_cap,
        } => {
            if *array_cap == 0 && *fields_cap == 0 {
                format!("NEWTABLE     R{}", dst)
            } else {
                format!(
                    "NEWTABLE     R{} (arr={}, fields={})",
                    dst, array_cap, fields_cap
                )
            }
        }
        Instruction::GetTable { dst, table, key } => {
            format!("GETTABLE     R{}, R{}[R{}]", dst, table, key)
        }
        Instruction::SetTable { table, key, val } => {
            format!("SETTABLE     R{}[R{}], R{}", table, key, val)
        }
        Instruction::GetTableK { dst, table, key_k } => {
            format!("GETTABLEK    R{}, R{}[K{}]", dst, table, key_k)
        }
        Instruction::GetImport {
            dst,
            mod_k,
            field_k,
            site,
        } => {
            format!("GETIMPORT    R{}, K{}, K{}, {}", dst, mod_k, field_k, site)
        }
        Instruction::SetTableK { table, key_k, val } => {
            format!("SETTABLEK    R{}[K{}], R{}", table, key_k, val)
        }
        Instruction::AppendArray { table, src } => format!("APPEND       R{}, R{}", table, src),
        Instruction::Add { dst, a, b } => format!("ADD          R{}, R{}, R{}", dst, a, b),
        Instruction::Sub { dst, a, b } => format!("SUB          R{}, R{}, R{}", dst, a, b),
        Instruction::Mul { dst, a, b } => format!("MUL          R{}, R{}, R{}", dst, a, b),
        Instruction::Div { dst, a, b } => format!("DIV          R{}, R{}, R{}", dst, a, b),
        Instruction::IDiv { dst, a, b } => format!("IDIV         R{}, R{}, R{}", dst, a, b),
        Instruction::Mod { dst, a, b } => format!("MOD          R{}, R{}, R{}", dst, a, b),
        Instruction::Pow { dst, a, b } => format!("POW          R{}, R{}, R{}", dst, a, b),
        Instruction::BitAnd { dst, a, b } => format!("BITAND       R{}, R{}, R{}", dst, a, b),
        Instruction::BitOr { dst, a, b } => format!("BITOR        R{}, R{}, R{}", dst, a, b),
        Instruction::BitXor { dst, a, b } => format!("BITXOR       R{}, R{}, R{}", dst, a, b),
        Instruction::Shl { dst, a, b } => format!("SHL          R{}, R{}, R{}", dst, a, b),
        Instruction::Shr { dst, a, b } => format!("SHR          R{}, R{}, R{}", dst, a, b),
        Instruction::LShl { dst, a, b } => format!("LSHL         R{}, R{}, R{}", dst, a, b),
        Instruction::LShr { dst, a, b } => format!("LSHR         R{}, R{}, R{}", dst, a, b),
        Instruction::Concat { dst, a, b } => format!("CONCAT       R{}, R{}, R{}", dst, a, b),
        Instruction::Unm { dst, src } => format!("UNM          R{}, R{}", dst, src),
        Instruction::Not { dst, src } => format!("NOT          R{}, R{}", dst, src),
        Instruction::Len { dst, src } => format!("LEN          R{}, R{}", dst, src),
        Instruction::BitNot { dst, src } => format!("BITNOT       R{}, R{}", dst, src),
        Instruction::Coalesce { dst, a, b } => format!("COALESCE     R{}, R{}, R{}", dst, a, b),
        Instruction::Eq {
            a,
            b,
            jump_if_false,
        } => format!("EQ           R{}, R{}, offset {}", a, b, jump_if_false),
        Instruction::Ne {
            a,
            b,
            jump_if_false,
        } => format!("NE           R{}, R{}, offset {}", a, b, jump_if_false),
        Instruction::Lt {
            a,
            b,
            jump_if_false,
        } => format!("LT           R{}, R{}, offset {}", a, b, jump_if_false),
        Instruction::Le {
            a,
            b,
            jump_if_false,
        } => format!("LE           R{}, R{}, offset {}", a, b, jump_if_false),
        Instruction::Gt {
            a,
            b,
            jump_if_false,
        } => format!("GT           R{}, R{}, offset {}", a, b, jump_if_false),
        Instruction::Ge {
            a,
            b,
            jump_if_false,
        } => format!("GE           R{}, R{}, offset {}", a, b, jump_if_false),
        Instruction::Test { reg, jump_if_false } => {
            format!("TEST         R{}, offset {}", reg, jump_if_false)
        }
        Instruction::Jump { offset } => format!("JUMP         offset {}", offset),
        Instruction::Call { callee, argc, retc } => {
            format!("CALL         R{}, argc {}, retc {}", callee, argc, retc)
        }
        Instruction::Return { base, count } => format!("RETURN       R{}, count {}", base, count),
        Instruction::Closure { dst, proto_idx } => {
            format!("CLOSURE      R{}, proto P{}", dst, proto_idx)
        }
        Instruction::Vararg { dst, count } => format!("VARARG       R{}, count {}", dst, count),
        Instruction::ForPrep { base, jump } => format!("FORPREP      R{}, offset {}", base, jump),
        Instruction::ForLoop { base, jump } => format!("FORLOOP      R{}, offset {}", base, jump),
        Instruction::SetList { table, base, count } => {
            format!("SETLIST      R{}, R{}, count {}", table, base, count)
        }
        Instruction::TForCall { base, retc } => format!("TFORCALL     R{}, {}", base, retc),
        Instruction::TForLoop { base, jump } => format!("TFORLOOP     R{}, {}", base, jump),
        Instruction::CloseUpval { reg } => format!("CLOSEUPVAL   R{}", reg),
        Instruction::AddK { dst, a, k } => format!("ADDK         R{}, R{}, K{}", dst, a, k),
        Instruction::SubK { dst, a, k } => format!("SUBK         R{}, R{}, K{}", dst, a, k),
        Instruction::MulK { dst, a, k } => format!("MULK         R{}, R{}, K{}", dst, a, k),
        Instruction::EqK {
            a,
            k,
            jump_if_false,
        } => format!("EQK          R{}, K{}, {:+}", a, k, jump_if_false),
        Instruction::NeK {
            a,
            k,
            jump_if_false,
        } => format!("NEK          R{}, K{}, {:+}", a, k, jump_if_false),
        Instruction::LtK {
            a,
            k,
            jump_if_false,
        } => format!("LTK          R{}, K{}, {:+}", a, k, jump_if_false),
        Instruction::LeK {
            a,
            k,
            jump_if_false,
        } => format!("LEK          R{}, K{}, {:+}", a, k, jump_if_false),
        Instruction::GtK {
            a,
            k,
            jump_if_false,
        } => format!("GTK          R{}, K{}, {:+}", a, k, jump_if_false),
        Instruction::GeK {
            a,
            k,
            jump_if_false,
        } => format!("GEK          R{}, K{}, {:+}", a, k, jump_if_false),
    }
}
