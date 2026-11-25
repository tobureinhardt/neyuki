//! Reads `.nyc` bytecode files and verifies what it loads.

use crate::bytecode::format::{
    BYTECODE_VERSION, BytecodeHeader, MAGIC, MIN_SUPPORTED_BYTECODE_VERSION, is_supported_version,
};
use crate::bytecode::instruction::Instruction;
use crate::bytecode::proto::{Constant, Proto, UpvalueDesc};
use crate::bytecode::verify::verify_proto;
use num_bigint::{BigInt, Sign};

/// The format version of bytecode written for an incompatible Neyuki, or
/// `None` when `bytes` is a supported format (or not bytecode at all).
pub fn stale_version(bytes: &[u8]) -> Option<u8> {
    if bytes.len() < BytecodeHeader::HEADER_SIZE || !bytes.starts_with(MAGIC) {
        return None;
    }
    Some(bytes[MAGIC.len()]).filter(|&v| !is_supported_version(v))
}

/// Why bytecode in format `version` cannot be loaded, and what to do about it.
pub fn version_error(version: u8) -> String {
    let made_by = if version < MIN_SUPPORTED_BYTECODE_VERSION {
        "an older"
    } else {
        "a newer"
    };
    format!(
        "bytecode was compiled by {} Neyuki (format {}, this one reads formats {}..={}); recompile the source with `neyuki compile`",
        made_by, version, MIN_SUPPORTED_BYTECODE_VERSION, BYTECODE_VERSION
    )
}

pub fn deserialize(bytes: &[u8]) -> Result<Proto, String> {
    if let Some(version) = stale_version(bytes) {
        return Err(version_error(version));
    }
    let (header, mut cursor) = BytecodeHeader::parse(bytes)?;
    let payload = &bytes[cursor..];
    let actual_checksum = crate::bytecode::format::compute_crc32(payload);
    if actual_checksum != header.checksum {
        return Err(format!(
            "bytecode integrity check failed: checksum mismatch (expected 0x{:08X}, got 0x{:08X})",
            header.checksum, actual_checksum
        ));
    }
    let proto = read_proto(bytes, &mut cursor, 0, None, header.version)?;
    // Verify bytecode integrity before allowing execution
    verify_proto(&proto).map_err(|e| e.to_string())?;
    Ok(proto)
}

fn read_u8(bytes: &[u8], cursor: &mut usize) -> Result<u8, String> {
    if *cursor >= bytes.len() {
        return Err("unexpected end of bytecode".to_string());
    }
    let val = bytes[*cursor];
    *cursor += 1;
    Ok(val)
}

fn read_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16, String> {
    if *cursor + 2 > bytes.len() {
        return Err("unexpected end of bytecode".to_string());
    }
    let val = u16::from_le_bytes([bytes[*cursor], bytes[*cursor + 1]]);
    *cursor += 2;
    Ok(val)
}

fn read_i16(bytes: &[u8], cursor: &mut usize) -> Result<i16, String> {
    if *cursor + 2 > bytes.len() {
        return Err("unexpected end of bytecode".to_string());
    }
    let val = i16::from_le_bytes([bytes[*cursor], bytes[*cursor + 1]]);
    *cursor += 2;
    Ok(val)
}

fn read_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, String> {
    if *cursor + 4 > bytes.len() {
        return Err("unexpected end of bytecode".to_string());
    }
    let val = u32::from_le_bytes([
        bytes[*cursor],
        bytes[*cursor + 1],
        bytes[*cursor + 2],
        bytes[*cursor + 3],
    ]);
    *cursor += 4;
    Ok(val)
}

fn read_i32(bytes: &[u8], cursor: &mut usize) -> Result<i32, String> {
    if *cursor + 4 > bytes.len() {
        return Err("unexpected end of bytecode".to_string());
    }
    let val = i32::from_le_bytes([
        bytes[*cursor],
        bytes[*cursor + 1],
        bytes[*cursor + 2],
        bytes[*cursor + 3],
    ]);
    *cursor += 4;
    Ok(val)
}

fn read_f64(bytes: &[u8], cursor: &mut usize) -> Result<f64, String> {
    if *cursor + 8 > bytes.len() {
        return Err("unexpected end of bytecode".to_string());
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&bytes[*cursor..*cursor + 8]);
    *cursor += 8;
    Ok(f64::from_le_bytes(b))
}

fn read_string(bytes: &[u8], cursor: &mut usize) -> Result<String, String> {
    let len = read_u32(bytes, cursor)? as usize;
    if *cursor + len > bytes.len() {
        return Err("unexpected end of bytecode in string".to_string());
    }
    let s = std::str::from_utf8(&bytes[*cursor..*cursor + len])
        .map_err(|e| format!("invalid utf-8: {}", e))?
        .to_string();
    *cursor += len;
    Ok(s)
}

fn read_bigint(bytes: &[u8], cursor: &mut usize) -> Result<BigInt, String> {
    let sign_byte = read_u8(bytes, cursor)?;
    let sign = match sign_byte {
        0 => Sign::Minus,
        1 => Sign::NoSign,
        2 => Sign::Plus,
        _ => return Err("invalid bigint sign".to_string()),
    };
    let len = read_u32(bytes, cursor)? as usize;
    if *cursor + len > bytes.len() {
        return Err("unexpected end of bytecode in bigint".to_string());
    }
    let b = BigInt::from_bytes_le(sign, &bytes[*cursor..*cursor + len]);
    *cursor += len;
    Ok(b)
}

fn read_constant(bytes: &[u8], cursor: &mut usize) -> Result<Constant, String> {
    let tag = read_u8(bytes, cursor)?;
    match tag {
        0 => Ok(Constant::Nil),
        1 => {
            let b = read_u8(bytes, cursor)? != 0;
            Ok(Constant::Bool(b))
        }
        2 => {
            let bi = read_bigint(bytes, cursor)?;
            Ok(Constant::Int(bi))
        }
        3 => {
            let f = read_f64(bytes, cursor)?;
            Ok(Constant::Float(f))
        }
        4 => {
            let s = read_string(bytes, cursor)?;
            Ok(Constant::String(s))
        }
        _ => Err(format!("invalid constant tag: {}", tag)),
    }
}

fn read_instruction(bytes: &[u8], cursor: &mut usize, version: u8) -> Result<Instruction, String> {
    let tag = read_u8(bytes, cursor)?;
    if tag >= 75 && version < 9 {
        return Err(format!(
            "instruction tag {} requires bytecode format 9 (file is format {})",
            tag, version
        ));
    }
    if tag >= 66 && version < 8 {
        return Err(format!(
            "instruction tag {} requires bytecode format 8 (file is format {})",
            tag, version
        ));
    }
    if tag >= 55 && version < 7 {
        return Err(format!(
            "instruction tag {} requires bytecode format 7 (file is format {})",
            tag, version
        ));
    }
    match tag {
        1 => Ok(Instruction::LoadNil {
            dst: read_u8(bytes, cursor)?,
        }),
        2 => Ok(Instruction::LoadBool {
            dst: read_u8(bytes, cursor)?,
            val: read_u8(bytes, cursor)? != 0,
        }),
        3 => Ok(Instruction::LoadInt {
            dst: read_u8(bytes, cursor)?,
            val: read_i32(bytes, cursor)?,
        }),
        4 => Ok(Instruction::LoadK {
            dst: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
        }),
        5 => Ok(Instruction::Move {
            dst: read_u8(bytes, cursor)?,
            src: read_u8(bytes, cursor)?,
        }),
        6 => Ok(Instruction::GetGlobal {
            dst: read_u8(bytes, cursor)?,
            name_k: read_u16(bytes, cursor)?,
        }),
        7 => Ok(Instruction::SetGlobal {
            src: read_u8(bytes, cursor)?,
            name_k: read_u16(bytes, cursor)?,
        }),
        8 => Ok(Instruction::GetUpval {
            dst: read_u8(bytes, cursor)?,
            upval_idx: read_u8(bytes, cursor)?,
        }),
        9 => Ok(Instruction::SetUpval {
            src: read_u8(bytes, cursor)?,
            upval_idx: read_u8(bytes, cursor)?,
        }),
        10 => Ok(Instruction::NewTable {
            dst: read_u8(bytes, cursor)?,
            array_cap: read_u8(bytes, cursor)?,
            fields_cap: read_u8(bytes, cursor)?,
        }),
        11 => Ok(Instruction::GetTable {
            dst: read_u8(bytes, cursor)?,
            table: read_u8(bytes, cursor)?,
            key: read_u8(bytes, cursor)?,
        }),
        12 => Ok(Instruction::SetTable {
            table: read_u8(bytes, cursor)?,
            key: read_u8(bytes, cursor)?,
            val: read_u8(bytes, cursor)?,
        }),
        13 => Ok(Instruction::GetTableK {
            dst: read_u8(bytes, cursor)?,
            table: read_u8(bytes, cursor)?,
            key_k: read_u16(bytes, cursor)?,
        }),
        14 => Ok(Instruction::SetTableK {
            table: read_u8(bytes, cursor)?,
            key_k: read_u16(bytes, cursor)?,
            val: read_u8(bytes, cursor)?,
        }),
        15 => Ok(Instruction::AppendArray {
            table: read_u8(bytes, cursor)?,
            src: read_u8(bytes, cursor)?,
        }),
        16 => Ok(Instruction::Add {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        17 => Ok(Instruction::Sub {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        18 => Ok(Instruction::Mul {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        19 => Ok(Instruction::Div {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        20 => Ok(Instruction::IDiv {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        21 => Ok(Instruction::Mod {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        22 => Ok(Instruction::Pow {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        23 => Ok(Instruction::BitAnd {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        24 => Ok(Instruction::BitOr {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        25 => Ok(Instruction::BitXor {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        26 => Ok(Instruction::Shl {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        27 => Ok(Instruction::Shr {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        28 => Ok(Instruction::Concat {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        29 => Ok(Instruction::Unm {
            dst: read_u8(bytes, cursor)?,
            src: read_u8(bytes, cursor)?,
        }),
        30 => Ok(Instruction::Not {
            dst: read_u8(bytes, cursor)?,
            src: read_u8(bytes, cursor)?,
        }),
        31 => Ok(Instruction::Len {
            dst: read_u8(bytes, cursor)?,
            src: read_u8(bytes, cursor)?,
        }),
        32 => Ok(Instruction::BitNot {
            dst: read_u8(bytes, cursor)?,
            src: read_u8(bytes, cursor)?,
        }),
        33 => Ok(Instruction::Coalesce {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        34 => Ok(Instruction::Eq {
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        35 => Ok(Instruction::Ne {
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        36 => Ok(Instruction::Lt {
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        37 => Ok(Instruction::Le {
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        38 => Ok(Instruction::Gt {
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        39 => Ok(Instruction::Ge {
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        40 => Ok(Instruction::Test {
            reg: read_u8(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        41 => Ok(Instruction::Jump {
            offset: read_i16(bytes, cursor)?,
        }),
        42 => Ok(Instruction::Call {
            callee: read_u8(bytes, cursor)?,
            argc: read_u8(bytes, cursor)?,
            retc: read_u8(bytes, cursor)?,
        }),
        43 => Ok(Instruction::Return {
            base: read_u8(bytes, cursor)?,
            count: read_u8(bytes, cursor)?,
        }),
        44 => Ok(Instruction::Closure {
            dst: read_u8(bytes, cursor)?,
            proto_idx: read_u16(bytes, cursor)?,
        }),
        45 => Ok(Instruction::Vararg {
            dst: read_u8(bytes, cursor)?,
            count: read_u8(bytes, cursor)?,
        }),
        46 => Ok(Instruction::ForPrep {
            base: read_u8(bytes, cursor)?,
            jump: read_i16(bytes, cursor)?,
        }),
        47 => Ok(Instruction::ForLoop {
            base: read_u8(bytes, cursor)?,
            jump: read_i16(bytes, cursor)?,
        }),
        48 => Ok(Instruction::SetList {
            table: read_u8(bytes, cursor)?,
            base: read_u8(bytes, cursor)?,
            count: read_u8(bytes, cursor)?,
        }),
        49 => Ok(Instruction::TForCall {
            base: read_u8(bytes, cursor)?,
            retc: read_u8(bytes, cursor)?,
        }),
        50 => Ok(Instruction::TForLoop {
            base: read_u8(bytes, cursor)?,
            jump: read_i16(bytes, cursor)?,
        }),
        51 => Ok(Instruction::LShl {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        52 => Ok(Instruction::LShr {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            b: read_u8(bytes, cursor)?,
        }),
        53 => Ok(Instruction::GetImport {
            dst: read_u8(bytes, cursor)?,
            mod_k: read_u16(bytes, cursor)?,
            field_k: read_u16(bytes, cursor)?,
            site: read_u16(bytes, cursor)?,
        }),
        54 => Ok(Instruction::CloseUpval {
            reg: read_u8(bytes, cursor)?,
        }),
        55 => Ok(Instruction::AddI {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            imm: read_i16(bytes, cursor)?,
        }),
        56 => Ok(Instruction::SubI {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            imm: read_i16(bytes, cursor)?,
        }),
        57 => Ok(Instruction::MulI {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            imm: read_i16(bytes, cursor)?,
        }),
        58 => Ok(Instruction::ShlI {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            imm: read_u8(bytes, cursor)?,
        }),
        59 => Ok(Instruction::ShrI {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            imm: read_u8(bytes, cursor)?,
        }),
        60 => Ok(Instruction::EqI {
            a: read_u8(bytes, cursor)?,
            imm: read_i16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        61 => Ok(Instruction::NeI {
            a: read_u8(bytes, cursor)?,
            imm: read_i16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        62 => Ok(Instruction::LtI {
            a: read_u8(bytes, cursor)?,
            imm: read_i16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        63 => Ok(Instruction::LeI {
            a: read_u8(bytes, cursor)?,
            imm: read_i16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        64 => Ok(Instruction::GtI {
            a: read_u8(bytes, cursor)?,
            imm: read_i16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        65 => Ok(Instruction::GeI {
            a: read_u8(bytes, cursor)?,
            imm: read_i16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        66 => Ok(Instruction::AddK {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
        }),
        67 => Ok(Instruction::SubK {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
        }),
        68 => Ok(Instruction::MulK {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
        }),
        69 => Ok(Instruction::EqK {
            a: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        70 => Ok(Instruction::NeK {
            a: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        71 => Ok(Instruction::LtK {
            a: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        72 => Ok(Instruction::LeK {
            a: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        73 => Ok(Instruction::GtK {
            a: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        74 => Ok(Instruction::GeK {
            a: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
            jump_if_false: read_i16(bytes, cursor)?,
        }),
        75 => Ok(Instruction::IDivK {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
        }),
        76 => Ok(Instruction::ModK {
            dst: read_u8(bytes, cursor)?,
            a: read_u8(bytes, cursor)?,
            k: read_u16(bytes, cursor)?,
        }),
        _ => Err(format!("unknown instruction tag: {}", tag)),
    }
}

fn read_proto(
    bytes: &[u8],
    cursor: &mut usize,
    depth: usize,
    parent_source: Option<std::rc::Rc<str>>,
    version: u8,
) -> Result<Proto, String> {
    const MAX_PROTO_DEPTH: usize = 64;
    if depth > MAX_PROTO_DEPTH {
        return Err(format!(
            "bytecode nested prototype depth limit ({}) exceeded",
            MAX_PROTO_DEPTH
        ));
    }

    let has_name = read_u8(bytes, cursor)? != 0;
    let name = if has_name {
        Some(read_string(bytes, cursor)?)
    } else {
        None
    };
    let source: Option<std::rc::Rc<str>> = match read_u8(bytes, cursor)? {
        0 => None,
        1 => Some(read_string(bytes, cursor)?.into()),
        2 => parent_source,
        tag => return Err(format!("invalid source tag {}", tag)),
    };
    let num_params = read_u8(bytes, cursor)?;
    let max_registers = read_u8(bytes, cursor)?;
    let is_vararg = read_u8(bytes, cursor)? != 0;

    let num_constants = read_u32(bytes, cursor)? as usize;
    if num_constants > 65535 {
        return Err(format!(
            "proto constant pool too large: {} (max 65535)",
            num_constants
        ));
    }
    let mut constants = Vec::with_capacity(num_constants);
    for _ in 0..num_constants {
        constants.push(read_constant(bytes, cursor)?);
    }

    let num_instructions = read_u32(bytes, cursor)? as usize;
    if num_instructions > 0x10_0000 {
        return Err(format!(
            "proto instruction count too large: {} (max 1048576)",
            num_instructions
        ));
    }
    let mut instructions = Vec::with_capacity(num_instructions);
    for _ in 0..num_instructions {
        instructions.push(read_instruction(bytes, cursor, version)?);
    }

    let num_protos = read_u32(bytes, cursor)? as usize;
    if num_protos > 4096 {
        return Err(format!(
            "proto nested prototype count too large: {} (max 4096)",
            num_protos
        ));
    }
    let mut protos = Vec::with_capacity(num_protos);
    for _ in 0..num_protos {
        protos.push(std::rc::Rc::new(read_proto(
            bytes,
            cursor,
            depth + 1,
            source.clone(),
            version,
        )?));
    }

    let num_upvalues = read_u32(bytes, cursor)? as usize;
    if num_upvalues > 255 {
        return Err(format!(
            "proto upvalue count too large: {} (max 255)",
            num_upvalues
        ));
    }
    let mut upvalues = Vec::with_capacity(num_upvalues);
    let mut upvalue_names = Vec::with_capacity(num_upvalues);
    for _ in 0..num_upvalues {
        let in_stack = read_u8(bytes, cursor)? != 0;
        let index = read_u8(bytes, cursor)?;
        upvalues.push(UpvalueDesc { in_stack, index });
        upvalue_names.push(read_string(bytes, cursor)?);
    }

    let num_lines = read_u32(bytes, cursor)? as usize;
    if num_lines > 0x10_0000 {
        return Err(format!(
            "proto line info too large: {} (max 1048576)",
            num_lines
        ));
    }
    let mut lines = Vec::with_capacity(num_lines);
    for _ in 0..num_lines {
        lines.push(read_u32(bytes, cursor)?);
    }

    let num_locals = read_u32(bytes, cursor)? as usize;
    if num_locals > 65535 {
        return Err(format!(
            "proto local variable count too large: {} (max 65535)",
            num_locals
        ));
    }
    let mut local_names = Vec::with_capacity(num_locals);
    for _ in 0..num_locals {
        let name = read_string(bytes, cursor)?;
        let reg = read_u8(bytes, cursor)?;
        let from_pc = read_u32(bytes, cursor)?;
        let to_pc = read_u32(bytes, cursor)?;
        local_names.push(crate::bytecode::proto::LocalVarInfo {
            name,
            reg,
            from_pc,
            to_pc,
        });
    }

    Ok(Proto {
        name,
        num_params,
        max_registers,
        is_vararg,
        constants,
        instructions,
        protos,
        upvalues,
        lines,
        local_names,
        source,
        upvalue_names,
        cache: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::serialize::serialize;

    fn with_version(version: u8) -> Vec<u8> {
        let mut proto = Proto::new(Some("old".to_string()), 0, false);
        proto.max_registers = 1;
        proto.emit(Instruction::Return { base: 0, count: 0 }, 1);
        let mut bytes = serialize(&proto);
        bytes[MAGIC.len()] = version;
        bytes
    }

    #[test]
    fn test_current_and_previous_bytecode_are_not_stale() {
        assert_eq!(stale_version(&with_version(BYTECODE_VERSION)), None);
        assert_eq!(
            stale_version(&with_version(MIN_SUPPORTED_BYTECODE_VERSION)),
            None
        );
        assert_eq!(stale_version(b"local x = 1"), None);
        assert_eq!(stale_version(MAGIC), None);
    }

    #[test]
    fn test_v6_bytecode_deserializes_and_runs_but_rejects_v7_opcodes() {
        let mut proto = Proto::new(Some("v6_compat".to_string()), 0, false);
        proto.max_registers = 2;
        proto.emit(Instruction::LoadInt { dst: 0, val: 40 }, 1);
        proto.emit(Instruction::LoadInt { dst: 1, val: 2 }, 1);
        proto.emit(Instruction::Add { dst: 0, a: 0, b: 1 }, 1);
        proto.emit(Instruction::Return { base: 0, count: 1 }, 1);

        let mut bytes = serialize(&proto);
        bytes[MAGIC.len()] = 6;
        let loaded = deserialize(&bytes).expect("v6 bytecode must still load");
        let mut vm = crate::vm::machine::VM::new();
        assert_eq!(vm.execute(loaded).expect("exec v6").to_string(), "42");

        // A v6 header claiming a v7-only opcode tag (AddI) must be rejected.
        let mut v7_only = Proto::new(Some("v7_only".to_string()), 0, false);
        v7_only.max_registers = 1;
        v7_only.emit(
            Instruction::AddI {
                dst: 0,
                a: 0,
                imm: 1,
            },
            1,
        );
        v7_only.emit(Instruction::Return { base: 0, count: 1 }, 1);
        let mut bad_v6 = serialize(&v7_only);
        bad_v6[MAGIC.len()] = 6;
        let err = deserialize(&bad_v6).unwrap_err();
        assert!(err.contains("requires bytecode format 7"), "{}", err);
    }

    #[test]
    fn test_older_bytecode_names_both_versions() {
        let old = MIN_SUPPORTED_BYTECODE_VERSION - 1;
        let bytes = with_version(old);
        assert_eq!(stale_version(&bytes), Some(old));
        let err = deserialize(&bytes).unwrap_err();
        assert!(err.contains("an older Neyuki"), "{}", err);
        assert!(
            err.contains(&format!(
                "format {}, this one reads formats {}..={}",
                old, MIN_SUPPORTED_BYTECODE_VERSION, BYTECODE_VERSION
            )),
            "{}",
            err
        );
        assert!(err.contains("neyuki compile"), "{}", err);
    }

    #[test]
    fn test_newer_bytecode_is_reported_as_newer() {
        let err = deserialize(&with_version(BYTECODE_VERSION + 1)).unwrap_err();
        assert!(err.contains("a newer Neyuki"), "{}", err);
    }

    #[test]
    fn test_version_is_checked_before_the_checksum() {
        // Older formats may checksum differently; the version is what to report.
        let mut bytes = with_version(MIN_SUPPORTED_BYTECODE_VERSION - 1);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let err = deserialize(&bytes).unwrap_err();
        assert!(err.contains("an older Neyuki"), "{}", err);
    }

    #[test]
    fn test_parameters_outside_the_register_window_are_rejected() {
        // Calling a function clears the parameters the caller did not pass;
        // with more parameters than registers that wrote past the stack.
        let mut proto = Proto::new(Some("params".to_string()), 0, false);
        proto.max_registers = 1;
        proto.emit(Instruction::Return { base: 0, count: 0 }, 1);
        proto.num_params = 200;
        let err = deserialize(&serialize(&proto)).unwrap_err();
        assert!(err.contains("parameter count 200 exceeds"), "{}", err);
    }
}
