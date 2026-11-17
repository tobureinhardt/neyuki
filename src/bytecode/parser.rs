//! Parses assembly from the disassembler back into prototypes.

use num_bigint::BigInt;
use std::str::FromStr;

use crate::bytecode::instruction::Instruction;
use crate::bytecode::proto::{Constant, Proto};

// Parses escape sequences in string literals formatted by disassembler
fn unescape_string(input: &str) -> Result<String, String> {
    let s = input.trim();
    if !s.starts_with('"') || !s.ends_with('"') || s.len() < 2 {
        return Err(format!("expected quoted string literal, got {}", input));
    }
    let inner = &s[1..s.len() - 1];
    let mut result = String::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('r') => result.push('\r'),
                Some('t') => result.push('\t'),
                Some('\\') => result.push('\\'),
                Some('"') => result.push('"'),
                Some('\'') => result.push('\''),
                Some('0') => result.push('\0'),
                Some('x') => {
                    let mut hex = String::with_capacity(2);
                    for _ in 0..2 {
                        if let Some(h) = chars.next() {
                            hex.push(h);
                        } else {
                            return Err("incomplete hex escape sequence \\x".to_string());
                        }
                    }
                    let byte = u8::from_str_radix(&hex, 16)
                        .map_err(|_| format!("invalid hex escape sequence \\x{}", hex))?;
                    result.push(byte as char);
                }
                Some('u') => {
                    if chars.next() != Some('{') {
                        return Err("expected '{' after \\u".to_string());
                    }
                    let mut hex = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some(h) if h.is_ascii_hexdigit() => hex.push(h),
                            Some(other) => {
                                return Err(format!(
                                    "invalid character '{}' in unicode escape",
                                    other
                                ));
                            }
                            None => return Err("unterminated unicode escape".to_string()),
                        }
                    }
                    let code_point = u32::from_str_radix(&hex, 16)
                        .map_err(|_| format!("invalid unicode escape \\u{{{}}}", hex))?;
                    let ch = char::from_u32(code_point)
                        .ok_or_else(|| format!("invalid unicode code point 0x{:X}", code_point))?;
                    result.push(ch);
                }
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => return Err("trailing backslash in string escape".to_string()),
            }
        } else {
            result.push(c);
        }
    }
    Ok(result)
}

pub fn parse_assembly(text: &str) -> Result<Proto, String> {
    let mut proto = Proto::new(Some("main".to_string()), 0, false);
    let mut current_line = 1u32;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("//") {
            continue;
        }

        if let Some(after_proto) = line.strip_prefix(".proto") {
            // Header information: e.g. .proto main (params: 0, registers: 4, vararg: false)
            let after_proto = after_proto.trim();
            if let Some(paren) = after_proto.find('(') {
                let name = after_proto[..paren].trim();
                if !name.is_empty() {
                    proto.name = Some(name.to_string());
                }
            }
            if let Some(params_start) = line.find("params: ") {
                let rest = &line[params_start + 8..];
                if let Some(comma) = rest.find(',') {
                    proto.num_params = rest[..comma].trim().parse().unwrap_or(0);
                }
            }
            if let Some(reg_start) = line.find("registers: ") {
                let rest = &line[reg_start + 11..];
                if let Some(comma) = rest.find(',') {
                    proto.max_registers = rest[..comma].trim().parse().unwrap_or(0);
                }
            }
            continue;
        }

        if line.starts_with(".const") {
            // E.g. .const K0 = int 10
            let parts: Vec<&str> = line.splitn(4, ' ').collect();
            if parts.len() >= 4 && parts[2] == "=" {
                let type_and_val = parts[3];
                if type_and_val == "nil" {
                    proto.constants.push(Constant::Nil);
                } else if let Some(rest) = type_and_val.strip_prefix("boolean ") {
                    let b = rest.trim() == "true";
                    proto.constants.push(Constant::Bool(b));
                } else if let Some(rest) = type_and_val.strip_prefix("int ") {
                    let s = rest.trim();
                    let bi = BigInt::from_str(s).unwrap_or_default();
                    proto.constants.push(Constant::Int(bi));
                } else if let Some(rest) = type_and_val.strip_prefix("float ") {
                    let s = rest.trim();
                    let f = s.parse::<f64>().unwrap_or(0.0);
                    proto.constants.push(Constant::Float(f));
                } else if let Some(rest) = type_and_val.strip_prefix("string ") {
                    let s = rest.trim();
                    let unescaped = unescape_string(s)?;
                    proto.constants.push(Constant::String(unescaped));
                }
            }
            continue;
        }

        if line.starts_with(".upval") {
            // E.g. .upval U0 = stack[1] or .upval U0 = parent_upval[1]
            if let Some(eq_idx) = line.find('=') {
                let rhs = line[eq_idx + 1..].trim();
                let in_stack = rhs.starts_with("stack");
                if let (Some(open), Some(close)) = (rhs.find('['), rhs.rfind(']')) {
                    let idx = rhs[open + 1..close].trim().parse::<u8>().unwrap_or(0);
                    proto.upvalues.push(crate::bytecode::proto::UpvalueDesc {
                        in_stack,
                        index: idx,
                    });
                }
            }
            continue;
        }

        // Instruction line: e.g. 0000 [L001] LOADINT R0, 10
        let inst_str = if let Some(bracket_end) = line.find(']') {
            line[bracket_end + 1..].trim()
        } else {
            line
        };

        if let Some(inst) = parse_instruction_line(inst_str)? {
            proto.emit(inst, current_line);
            current_line += 1;
        }
    }

    Ok(proto)
}

fn parse_reg(token: &str) -> Result<u8, String> {
    let clean = token.trim().trim_end_matches(',');
    if let Some(reg_num) = clean.strip_prefix('R') {
        reg_num
            .parse::<u8>()
            .map_err(|_| format!("invalid register {}", clean))
    } else {
        Err(format!("expected register prefix 'R', got {}", clean))
    }
}

fn parse_const_idx(token: &str) -> Result<u16, String> {
    let clean = token.trim().trim_end_matches(',');
    if let Some(idx_str) = clean.strip_prefix('K') {
        idx_str
            .parse::<u16>()
            .map_err(|_| format!("invalid constant index {}", clean))
    } else {
        Err(format!("expected constant prefix 'K', got {}", clean))
    }
}

fn parse_upval_idx(token: &str) -> Result<u8, String> {
    let clean = token.trim().trim_end_matches(',');
    if let Some(idx_str) = clean.strip_prefix('U') {
        idx_str
            .parse::<u8>()
            .map_err(|_| format!("invalid upvalue index {}", clean))
    } else {
        Err(format!("expected upvalue prefix 'U', got {}", clean))
    }
}

fn parse_proto_idx(token: &str) -> Result<u16, String> {
    let clean = token.trim().trim_end_matches(',');
    if let Some(idx_str) = clean.strip_prefix('P') {
        idx_str
            .parse::<u16>()
            .map_err(|_| format!("invalid prototype index {}", clean))
    } else {
        Err(format!("expected prototype prefix 'P', got {}", clean))
    }
}

// Parses "R1[R2]" into (1, 2)
fn parse_table_and_key(token: &str) -> Result<(u8, u8), String> {
    let clean = token.trim().trim_end_matches(',');
    let open = clean
        .find('[')
        .ok_or_else(|| format!("expected '[' in table access, got {}", clean))?;
    let close = clean
        .rfind(']')
        .ok_or_else(|| format!("expected ']' in table access, got {}", clean))?;
    let table = parse_reg(&clean[..open])?;
    let key = parse_reg(&clean[open + 1..close])?;
    Ok((table, key))
}

// Parses "R1[K2]" into (1, 2)
fn parse_table_and_const_key(token: &str) -> Result<(u8, u16), String> {
    let clean = token.trim().trim_end_matches(',');
    let open = clean
        .find('[')
        .ok_or_else(|| format!("expected '[' in table access, got {}", clean))?;
    let close = clean
        .rfind(']')
        .ok_or_else(|| format!("expected ']' in table access, got {}", clean))?;
    let table = parse_reg(&clean[..open])?;
    let key_k = parse_const_idx(&clean[open + 1..close])?;
    Ok((table, key_k))
}

fn parse_instruction_line(line: &str) -> Result<Option<Instruction>, String> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(None);
    }

    let op = parts[0];
    match op {
        "LOADNIL" => {
            let dst = parse_reg(parts.get(1).ok_or("LOADNIL missing dst")?)?;
            Ok(Some(Instruction::LoadNil { dst }))
        }
        "LOADBOOL" => {
            let dst = parse_reg(parts.get(1).ok_or("LOADBOOL missing dst")?)?;
            let val = parts
                .get(2)
                .ok_or("LOADBOOL missing val")?
                .trim_end_matches(',')
                == "true";
            Ok(Some(Instruction::LoadBool { dst, val }))
        }
        "LOADINT" => {
            let dst = parse_reg(parts.get(1).ok_or("LOADINT missing dst")?)?;
            let val = parts
                .get(2)
                .ok_or("LOADINT missing val")?
                .trim_end_matches(',')
                .parse::<i32>()
                .map_err(|e| e.to_string())?;
            Ok(Some(Instruction::LoadInt { dst, val }))
        }
        "LOADK" => {
            let dst = parse_reg(parts.get(1).ok_or("LOADK missing dst")?)?;
            let k = parse_const_idx(parts.get(2).ok_or("LOADK missing const index")?)?;
            Ok(Some(Instruction::LoadK { dst, k }))
        }
        "MOVE" => {
            let dst = parse_reg(parts.get(1).ok_or("MOVE missing dst")?)?;
            let src = parse_reg(parts.get(2).ok_or("MOVE missing src")?)?;
            Ok(Some(Instruction::Move { dst, src }))
        }
        "GETGLOBAL" => {
            let dst = parse_reg(parts.get(1).ok_or("GETGLOBAL missing dst")?)?;
            let name_k = parse_const_idx(parts.get(2).ok_or("GETGLOBAL missing const index")?)?;
            Ok(Some(Instruction::GetGlobal { dst, name_k }))
        }
        "SETGLOBAL" => {
            let name_k = parse_const_idx(parts.get(1).ok_or("SETGLOBAL missing const index")?)?;
            let src = parse_reg(parts.get(2).ok_or("SETGLOBAL missing src")?)?;
            Ok(Some(Instruction::SetGlobal { src, name_k }))
        }
        "GETUPVAL" => {
            let dst = parse_reg(parts.get(1).ok_or("GETUPVAL missing dst")?)?;
            let upval_idx = parse_upval_idx(parts.get(2).ok_or("GETUPVAL missing upval index")?)?;
            Ok(Some(Instruction::GetUpval { dst, upval_idx }))
        }
        "SETUPVAL" => {
            let upval_idx = parse_upval_idx(parts.get(1).ok_or("SETUPVAL missing upval index")?)?;
            let src = parse_reg(parts.get(2).ok_or("SETUPVAL missing src")?)?;
            Ok(Some(Instruction::SetUpval { src, upval_idx }))
        }
        "NEWTABLE" => {
            let dst = parse_reg(parts.get(1).ok_or("NEWTABLE missing dst")?)?;
            let array_cap = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
            let fields_cap = parts.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
            Ok(Some(Instruction::NewTable {
                dst,
                array_cap,
                fields_cap,
            }))
        }
        "GETTABLE" => {
            let dst = parse_reg(parts.get(1).ok_or("GETTABLE missing dst")?)?;
            let (table, key) =
                parse_table_and_key(parts.get(2).ok_or("GETTABLE missing table[key]")?)?;
            Ok(Some(Instruction::GetTable { dst, table, key }))
        }
        "SETTABLE" => {
            let (table, key) =
                parse_table_and_key(parts.get(1).ok_or("SETTABLE missing table[key]")?)?;
            let val = parse_reg(parts.get(2).ok_or("SETTABLE missing val")?)?;
            Ok(Some(Instruction::SetTable { table, key, val }))
        }
        "GETTABLEK" => {
            let dst = parse_reg(parts.get(1).ok_or("GETTABLEK missing dst")?)?;
            let (table, key_k) =
                parse_table_and_const_key(parts.get(2).ok_or("GETTABLEK missing table[key_k]")?)?;
            Ok(Some(Instruction::GetTableK { dst, table, key_k }))
        }
        "SETTABLEK" => {
            let (table, key_k) =
                parse_table_and_const_key(parts.get(1).ok_or("SETTABLEK missing table[key_k]")?)?;
            let val = parse_reg(parts.get(2).ok_or("SETTABLEK missing val")?)?;
            Ok(Some(Instruction::SetTableK { table, key_k, val }))
        }
        "GETIMPORT" => {
            let dst = parse_reg(parts.get(1).ok_or("GETIMPORT missing dst")?)?;
            let mod_k = parse_const_idx(parts.get(2).ok_or("GETIMPORT missing mod_k")?)?;
            let field_k = parse_const_idx(parts.get(3).ok_or("GETIMPORT missing field_k")?)?;
            let site = parts
                .get(4)
                .ok_or("GETIMPORT missing site")?
                .trim()
                .trim_end_matches(',')
                .parse::<u16>()
                .map_err(|_| "invalid import site".to_string())?;
            Ok(Some(Instruction::GetImport {
                dst,
                mod_k,
                field_k,
                site,
            }))
        }
        "APPEND" => {
            let table = parse_reg(parts.get(1).ok_or("APPEND missing table")?)?;
            let src = parse_reg(parts.get(2).ok_or("APPEND missing src")?)?;
            Ok(Some(Instruction::AppendArray { table, src }))
        }
        "ADD" => {
            let dst = parse_reg(parts.get(1).ok_or("ADD missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("ADD missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("ADD missing b")?)?;
            Ok(Some(Instruction::Add { dst, a, b }))
        }
        "SUB" => {
            let dst = parse_reg(parts.get(1).ok_or("SUB missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("SUB missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("SUB missing b")?)?;
            Ok(Some(Instruction::Sub { dst, a, b }))
        }
        "MUL" => {
            let dst = parse_reg(parts.get(1).ok_or("MUL missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("MUL missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("MUL missing b")?)?;
            Ok(Some(Instruction::Mul { dst, a, b }))
        }
        "DIV" => {
            let dst = parse_reg(parts.get(1).ok_or("DIV missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("DIV missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("DIV missing b")?)?;
            Ok(Some(Instruction::Div { dst, a, b }))
        }
        "IDIV" => {
            let dst = parse_reg(parts.get(1).ok_or("IDIV missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("IDIV missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("IDIV missing b")?)?;
            Ok(Some(Instruction::IDiv { dst, a, b }))
        }
        "MOD" => {
            let dst = parse_reg(parts.get(1).ok_or("MOD missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("MOD missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("MOD missing b")?)?;
            Ok(Some(Instruction::Mod { dst, a, b }))
        }
        "POW" => {
            let dst = parse_reg(parts.get(1).ok_or("POW missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("POW missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("POW missing b")?)?;
            Ok(Some(Instruction::Pow { dst, a, b }))
        }
        "BITAND" => {
            let dst = parse_reg(parts.get(1).ok_or("BITAND missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("BITAND missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("BITAND missing b")?)?;
            Ok(Some(Instruction::BitAnd { dst, a, b }))
        }
        "BITOR" => {
            let dst = parse_reg(parts.get(1).ok_or("BITOR missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("BITOR missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("BITOR missing b")?)?;
            Ok(Some(Instruction::BitOr { dst, a, b }))
        }
        "BITXOR" => {
            let dst = parse_reg(parts.get(1).ok_or("BITXOR missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("BITXOR missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("BITXOR missing b")?)?;
            Ok(Some(Instruction::BitXor { dst, a, b }))
        }
        "SHL" => {
            let dst = parse_reg(parts.get(1).ok_or("SHL missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("SHL missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("SHL missing b")?)?;
            Ok(Some(Instruction::Shl { dst, a, b }))
        }
        "SHR" => {
            let dst = parse_reg(parts.get(1).ok_or("SHR missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("SHR missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("SHR missing b")?)?;
            Ok(Some(Instruction::Shr { dst, a, b }))
        }
        "LSHL" => {
            let dst = parse_reg(parts.get(1).ok_or("LSHL missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("LSHL missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("LSHL missing b")?)?;
            Ok(Some(Instruction::LShl { dst, a, b }))
        }
        "LSHR" => {
            let dst = parse_reg(parts.get(1).ok_or("LSHR missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("LSHR missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("LSHR missing b")?)?;
            Ok(Some(Instruction::LShr { dst, a, b }))
        }
        "CONCAT" => {
            let dst = parse_reg(parts.get(1).ok_or("CONCAT missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("CONCAT missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("CONCAT missing b")?)?;
            Ok(Some(Instruction::Concat { dst, a, b }))
        }
        "COALESCE" => {
            let dst = parse_reg(parts.get(1).ok_or("COALESCE missing dst")?)?;
            let a = parse_reg(parts.get(2).ok_or("COALESCE missing a")?)?;
            let b = parse_reg(parts.get(3).ok_or("COALESCE missing b")?)?;
            Ok(Some(Instruction::Coalesce { dst, a, b }))
        }
        "UNM" => {
            let dst = parse_reg(parts.get(1).ok_or("UNM missing dst")?)?;
            let src = parse_reg(parts.get(2).ok_or("UNM missing src")?)?;
            Ok(Some(Instruction::Unm { dst, src }))
        }
        "NOT" => {
            let dst = parse_reg(parts.get(1).ok_or("NOT missing dst")?)?;
            let src = parse_reg(parts.get(2).ok_or("NOT missing src")?)?;
            Ok(Some(Instruction::Not { dst, src }))
        }
        "LEN" => {
            let dst = parse_reg(parts.get(1).ok_or("LEN missing dst")?)?;
            let src = parse_reg(parts.get(2).ok_or("LEN missing src")?)?;
            Ok(Some(Instruction::Len { dst, src }))
        }
        "BITNOT" => {
            let dst = parse_reg(parts.get(1).ok_or("BITNOT missing dst")?)?;
            let src = parse_reg(parts.get(2).ok_or("BITNOT missing src")?)?;
            Ok(Some(Instruction::BitNot { dst, src }))
        }
        "EQ" | "NE" | "LT" | "LE" | "GT" | "GE" => {
            let a = parse_reg(parts.get(1).ok_or_else(|| format!("{} missing a", op))?)?;
            let b = parse_reg(parts.get(2).ok_or_else(|| format!("{} missing b", op))?)?;
            let offset_token = if parts.len() >= 5 && parts[3] == "offset" {
                parts[4]
            } else {
                parts
                    .get(3)
                    .ok_or_else(|| format!("{} missing offset", op))?
            };
            let jump_if_false = offset_token
                .trim_end_matches(',')
                .parse::<i16>()
                .map_err(|e| e.to_string())?;
            let inst = match op {
                "EQ" => Instruction::Eq {
                    a,
                    b,
                    jump_if_false,
                },
                "NE" => Instruction::Ne {
                    a,
                    b,
                    jump_if_false,
                },
                "LT" => Instruction::Lt {
                    a,
                    b,
                    jump_if_false,
                },
                "LE" => Instruction::Le {
                    a,
                    b,
                    jump_if_false,
                },
                "GT" => Instruction::Gt {
                    a,
                    b,
                    jump_if_false,
                },
                "GE" => Instruction::Ge {
                    a,
                    b,
                    jump_if_false,
                },
                _ => unreachable!("the outer match only lets comparison names through"),
            };
            Ok(Some(inst))
        }
        "TEST" => {
            let reg = parse_reg(parts.get(1).ok_or("TEST missing reg")?)?;
            let offset_token = if parts.len() >= 4 && parts[2] == "offset" {
                parts[3]
            } else {
                parts.get(2).ok_or("TEST missing offset")?
            };
            let jump_if_false = offset_token
                .trim_end_matches(',')
                .parse::<i16>()
                .map_err(|e| e.to_string())?;
            Ok(Some(Instruction::Test { reg, jump_if_false }))
        }
        "JUMP" => {
            let offset_token = if parts.len() >= 3 && parts[1] == "offset" {
                parts[2]
            } else {
                parts.get(1).ok_or("JUMP missing offset")?
            };
            let offset = offset_token
                .trim_end_matches(',')
                .parse::<i16>()
                .map_err(|e| e.to_string())?;
            Ok(Some(Instruction::Jump { offset }))
        }
        "CALL" => {
            let callee = parse_reg(parts.get(1).ok_or("CALL missing callee")?)?;
            let argc = if parts.len() >= 4 && parts[2] == "argc" {
                parts[3]
                    .trim_end_matches(',')
                    .parse::<u8>()
                    .map_err(|e| e.to_string())?
            } else {
                parts
                    .get(2)
                    .ok_or("CALL missing argc")?
                    .trim_end_matches(',')
                    .parse::<u8>()
                    .map_err(|e| e.to_string())?
            };
            let retc = if parts.len() >= 6 && parts[4] == "retc" {
                parts[5]
                    .trim_end_matches(',')
                    .parse::<u8>()
                    .map_err(|e| e.to_string())?
            } else {
                parts
                    .get(3)
                    .ok_or("CALL missing retc")?
                    .trim_end_matches(',')
                    .parse::<u8>()
                    .map_err(|e| e.to_string())?
            };
            Ok(Some(Instruction::Call { callee, argc, retc }))
        }
        "RETURN" => {
            let base = parse_reg(parts.get(1).ok_or("RETURN missing base")?)?;
            let count = if parts.len() >= 4 && parts[2] == "count" {
                parts[3].trim_end_matches(',').parse::<u8>().unwrap_or(1)
            } else if parts.len() >= 3 {
                parts[2].trim_end_matches(',').parse::<u8>().unwrap_or(1)
            } else {
                1
            };
            Ok(Some(Instruction::Return { base, count }))
        }
        "CLOSURE" => {
            let dst = parse_reg(parts.get(1).ok_or("CLOSURE missing dst")?)?;
            let proto_token = if parts.len() >= 4 && parts[2] == "proto" {
                parts[3]
            } else {
                parts.get(2).ok_or("CLOSURE missing proto index")?
            };
            let proto_idx = parse_proto_idx(proto_token)?;
            Ok(Some(Instruction::Closure { dst, proto_idx }))
        }
        "VARARG" => {
            let dst = parse_reg(parts.get(1).ok_or("VARARG missing dst")?)?;
            let count = if parts.len() >= 4 && parts[2] == "count" {
                parts[3]
                    .trim_end_matches(',')
                    .parse::<u8>()
                    .map_err(|e| e.to_string())?
            } else {
                parts
                    .get(2)
                    .ok_or("VARARG missing count")?
                    .trim_end_matches(',')
                    .parse::<u8>()
                    .map_err(|e| e.to_string())?
            };
            Ok(Some(Instruction::Vararg { dst, count }))
        }
        "FORPREP" => {
            let base = parse_reg(parts.get(1).ok_or("FORPREP missing base")?)?;
            let offset_token = if parts.len() >= 4 && parts[2] == "offset" {
                parts[3]
            } else {
                parts.get(2).ok_or("FORPREP missing offset")?
            };
            let jump = offset_token
                .trim_end_matches(',')
                .parse::<i16>()
                .map_err(|e| e.to_string())?;
            Ok(Some(Instruction::ForPrep { base, jump }))
        }
        "FORLOOP" => {
            let base = parse_reg(parts.get(1).ok_or("FORLOOP missing base")?)?;
            let offset_token = if parts.len() >= 4 && parts[2] == "offset" {
                parts[3]
            } else {
                parts.get(2).ok_or("FORLOOP missing offset")?
            };
            let jump = offset_token
                .trim_end_matches(',')
                .parse::<i16>()
                .map_err(|e| e.to_string())?;
            Ok(Some(Instruction::ForLoop { base, jump }))
        }
        "SETLIST" => {
            let table = parse_reg(parts.get(1).ok_or("SETLIST missing table")?)?;
            let base = parse_reg(parts.get(2).ok_or("SETLIST missing base")?)?;
            let count = if parts.len() >= 5 && parts[3] == "count" {
                parts[4]
                    .trim_end_matches(',')
                    .parse::<u8>()
                    .map_err(|e| e.to_string())?
            } else {
                parts
                    .get(3)
                    .ok_or("SETLIST missing count")?
                    .trim_end_matches(',')
                    .parse::<u8>()
                    .map_err(|e| e.to_string())?
            };
            Ok(Some(Instruction::SetList { table, base, count }))
        }
        "TFORCALL" => {
            let base = parse_reg(parts.get(1).ok_or("TFORCALL missing base")?)?;
            let retc = parts
                .get(2)
                .ok_or("TFORCALL missing retc")?
                .trim_end_matches(',')
                .parse::<u8>()
                .map_err(|e| e.to_string())?;
            Ok(Some(Instruction::TForCall { base, retc }))
        }
        "TFORLOOP" => {
            let base = parse_reg(parts.get(1).ok_or("TFORLOOP missing base")?)?;
            let jump = parts
                .get(2)
                .ok_or("TFORLOOP missing jump")?
                .trim_end_matches(',')
                .parse::<i16>()
                .map_err(|e| e.to_string())?;
            Ok(Some(Instruction::TForLoop { base, jump }))
        }
        "CLOSEUPVAL" => {
            let reg = parse_reg(parts.get(1).ok_or("CLOSEUPVAL missing reg")?)?;
            Ok(Some(Instruction::CloseUpval { reg }))
        }
        "ADDK" | "SUBK" | "MULK" => {
            let dst = parse_reg(parts.get(1).ok_or_else(|| format!("{} missing dst", op))?)?;
            let a = parse_reg(parts.get(2).ok_or_else(|| format!("{} missing a", op))?)?;
            let k = parse_const_idx(parts.get(3).ok_or_else(|| format!("{} missing k", op))?)?;
            let inst = match op {
                "ADDK" => Instruction::AddK { dst, a, k },
                "SUBK" => Instruction::SubK { dst, a, k },
                "MULK" => Instruction::MulK { dst, a, k },
                _ => unreachable!(),
            };
            Ok(Some(inst))
        }
        "EQK" | "NEK" | "LTK" | "LEK" | "GTK" | "GEK" => {
            let a = parse_reg(parts.get(1).ok_or_else(|| format!("{} missing a", op))?)?;
            let k = parse_const_idx(parts.get(2).ok_or_else(|| format!("{} missing k", op))?)?;
            let offset_token = if parts.len() >= 5 && parts[3] == "offset" {
                parts[4]
            } else {
                parts
                    .get(3)
                    .ok_or_else(|| format!("{} missing jump offset", op))?
            };
            let jump_if_false = offset_token
                .trim_end_matches(',')
                .parse::<i16>()
                .map_err(|e| e.to_string())?;
            let inst = match op {
                "EQK" => Instruction::EqK {
                    a,
                    k,
                    jump_if_false,
                },
                "NEK" => Instruction::NeK {
                    a,
                    k,
                    jump_if_false,
                },
                "LTK" => Instruction::LtK {
                    a,
                    k,
                    jump_if_false,
                },
                "LEK" => Instruction::LeK {
                    a,
                    k,
                    jump_if_false,
                },
                "GTK" => Instruction::GtK {
                    a,
                    k,
                    jump_if_false,
                },
                "GEK" => Instruction::GeK {
                    a,
                    k,
                    jump_if_false,
                },
                _ => unreachable!(),
            };
            Ok(Some(inst))
        }
        _ => Err(format!(
            "unknown or unsupported instruction mnemonic: '{}'",
            op
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::disasm::disassemble_proto;

    #[test]
    fn test_disasm_and_parse_assembly() {
        let mut proto = Proto::new(Some("test_func".to_string()), 2, false);
        proto.max_registers = 8;
        proto
            .instructions
            .push(Instruction::LoadInt { dst: 0, val: 42 });
        proto
            .instructions
            .push(Instruction::Return { base: 0, count: 1 });

        let asm = disassemble_proto(&proto, 0);
        let parsed = parse_assembly(&asm).expect("failed to parse assembly");
        assert_eq!(parsed.name, proto.name);
        assert_eq!(parsed.max_registers, proto.max_registers);
        assert_eq!(parsed.instructions.len(), 2);
    }

    #[test]
    fn test_unescape_and_string_constants_roundtrip() {
        let mut proto = Proto::new(Some("string_test".to_string()), 0, false);
        proto.constants.push(Constant::String(
            "hello \"world\"\nnewline\\slash\ttab".to_string(),
        ));
        proto.instructions.push(Instruction::LoadK { dst: 0, k: 0 });
        proto
            .instructions
            .push(Instruction::Return { base: 0, count: 1 });

        let asm = disassemble_proto(&proto, 0);
        let parsed = parse_assembly(&asm).expect("failed to parse assembly with escaped string");
        assert_eq!(parsed.constants.len(), 1);
        assert_eq!(
            parsed.constants[0],
            Constant::String("hello \"world\"\nnewline\\slash\ttab".to_string())
        );
    }

    #[test]
    fn test_unknown_instruction_errors() {
        let bad_asm = "0000 [L001] INVALID_OP R0, R1\n";
        let err = parse_assembly(bad_asm);
        assert!(err.is_err());
        assert!(
            err.unwrap_err()
                .contains("unknown or unsupported instruction mnemonic")
        );
    }

    #[test]
    fn test_all_instruction_types_roundtrip() {
        let mut proto = Proto::new(Some("all_ops".to_string()), 3, true);
        proto.max_registers = 16;
        proto
            .constants
            .push(Constant::String("test_str".to_string()));

        proto.instructions.push(Instruction::LoadNil { dst: 0 });
        proto
            .instructions
            .push(Instruction::LoadBool { dst: 1, val: true });
        proto
            .instructions
            .push(Instruction::LoadInt { dst: 2, val: -100 });
        proto.instructions.push(Instruction::LoadK { dst: 3, k: 0 });
        proto
            .instructions
            .push(Instruction::Move { dst: 4, src: 0 });
        proto
            .instructions
            .push(Instruction::GetGlobal { dst: 5, name_k: 0 });
        proto
            .instructions
            .push(Instruction::SetGlobal { src: 5, name_k: 0 });
        proto.instructions.push(Instruction::GetUpval {
            dst: 6,
            upval_idx: 1,
        });
        proto.instructions.push(Instruction::SetUpval {
            src: 6,
            upval_idx: 1,
        });
        proto.instructions.push(Instruction::NewTable {
            dst: 7,
            array_cap: 0,
            fields_cap: 0,
        });
        proto.instructions.push(Instruction::GetTable {
            dst: 8,
            table: 7,
            key: 0,
        });
        proto.instructions.push(Instruction::SetTable {
            table: 7,
            key: 0,
            val: 1,
        });
        proto.instructions.push(Instruction::GetTableK {
            dst: 8,
            table: 7,
            key_k: 0,
        });
        proto.instructions.push(Instruction::GetImport {
            dst: 8,
            mod_k: 0,
            field_k: 0,
            site: 0,
        });
        proto.instructions.push(Instruction::SetTableK {
            table: 7,
            key_k: 0,
            val: 1,
        });
        proto
            .instructions
            .push(Instruction::AppendArray { table: 7, src: 1 });
        proto
            .instructions
            .push(Instruction::Add { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::Sub { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::Mul { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::Div { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::IDiv { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::Mod { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::Pow { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::BitAnd { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::BitOr { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::BitXor { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::Shl { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::Shr { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::LShl { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::LShr { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::Concat { dst: 0, a: 1, b: 2 });
        proto
            .instructions
            .push(Instruction::Coalesce { dst: 0, a: 1, b: 2 });
        proto.instructions.push(Instruction::Unm { dst: 0, src: 1 });
        proto.instructions.push(Instruction::Not { dst: 0, src: 1 });
        proto.instructions.push(Instruction::Len { dst: 0, src: 1 });
        proto
            .instructions
            .push(Instruction::BitNot { dst: 0, src: 1 });
        proto.instructions.push(Instruction::Eq {
            a: 0,
            b: 1,
            jump_if_false: 3,
        });
        proto.instructions.push(Instruction::Ne {
            a: 0,
            b: 1,
            jump_if_false: -2,
        });
        proto.instructions.push(Instruction::Lt {
            a: 0,
            b: 1,
            jump_if_false: 1,
        });
        proto.instructions.push(Instruction::Le {
            a: 0,
            b: 1,
            jump_if_false: 1,
        });
        proto.instructions.push(Instruction::Gt {
            a: 0,
            b: 1,
            jump_if_false: 1,
        });
        proto.instructions.push(Instruction::Ge {
            a: 0,
            b: 1,
            jump_if_false: 1,
        });
        proto.instructions.push(Instruction::Test {
            reg: 0,
            jump_if_false: 2,
        });
        proto.instructions.push(Instruction::Jump { offset: -5 });
        proto.instructions.push(Instruction::Call {
            callee: 0,
            argc: 2,
            retc: 1,
        });
        proto.instructions.push(Instruction::Closure {
            dst: 0,
            proto_idx: 1,
        });
        proto
            .instructions
            .push(Instruction::Vararg { dst: 0, count: 2 });
        proto
            .instructions
            .push(Instruction::ForPrep { base: 0, jump: 4 });
        proto
            .instructions
            .push(Instruction::ForLoop { base: 0, jump: -3 });
        proto.instructions.push(Instruction::SetList {
            table: 7,
            base: 0,
            count: 3,
        });
        proto
            .instructions
            .push(Instruction::TForCall { base: 0, retc: 2 });
        proto
            .instructions
            .push(Instruction::TForLoop { base: 0, jump: 5 });
        proto.instructions.push(Instruction::CloseUpval { reg: 4 });
        proto
            .instructions
            .push(Instruction::Return { base: 0, count: 1 });

        let asm = disassemble_proto(&proto, 0);
        let parsed = parse_assembly(&asm).expect("failed to parse all instructions");
        assert_eq!(parsed.instructions.len(), proto.instructions.len());
        assert_eq!(parsed.instructions, proto.instructions);
    }
}
