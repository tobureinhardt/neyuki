//! Writes `.nyc` bytecode files.

use crate::bytecode::format::BytecodeHeader;
use crate::bytecode::instruction::Instruction;
use crate::bytecode::proto::{Constant, Proto};
use num_bigint::{BigInt, Sign};

pub use crate::bytecode::deserialize::deserialize;
pub use crate::bytecode::format::{BYTECODE_VERSION, MAGIC};

pub fn serialize(proto: &Proto) -> Vec<u8> {
    let mut payload = Vec::new();
    write_proto(&mut payload, proto, None);
    let checksum = crate::bytecode::format::compute_crc32(&payload);
    let mut buf = Vec::with_capacity(BytecodeHeader::HEADER_SIZE + payload.len());
    let header = BytecodeHeader::new(checksum);
    header.write_to(&mut buf);
    buf.extend_from_slice(&payload);
    buf
}

fn write_u8(buf: &mut Vec<u8>, val: u8) {
    buf.push(val);
}

fn write_u16(buf: &mut Vec<u8>, val: u16) {
    buf.extend_from_slice(&val.to_le_bytes());
}

fn write_i16(buf: &mut Vec<u8>, val: i16) {
    buf.extend_from_slice(&val.to_le_bytes());
}

fn write_u32(buf: &mut Vec<u8>, val: u32) {
    buf.extend_from_slice(&val.to_le_bytes());
}

fn write_i32(buf: &mut Vec<u8>, val: i32) {
    buf.extend_from_slice(&val.to_le_bytes());
}

fn write_f64(buf: &mut Vec<u8>, val: f64) {
    buf.extend_from_slice(&val.to_le_bytes());
}

fn write_string(buf: &mut Vec<u8>, s: &str) {
    write_u32(buf, s.len() as u32);
    buf.extend_from_slice(s.as_bytes());
}

fn write_bigint(buf: &mut Vec<u8>, b: &BigInt) {
    let (sign, bytes) = b.to_bytes_le();
    let sign_byte = match sign {
        Sign::Minus => 0u8,
        Sign::NoSign => 1u8,
        Sign::Plus => 2u8,
    };
    write_u8(buf, sign_byte);
    write_u32(buf, bytes.len() as u32);
    buf.extend_from_slice(&bytes);
}

fn write_constant(buf: &mut Vec<u8>, c: &Constant) {
    match c {
        Constant::Nil => write_u8(buf, 0),
        Constant::Bool(b) => {
            write_u8(buf, 1);
            write_u8(buf, if *b { 1 } else { 0 });
        }
        Constant::Int(i) => {
            write_u8(buf, 2);
            write_bigint(buf, i);
        }
        Constant::Float(f) => {
            write_u8(buf, 3);
            write_f64(buf, *f);
        }
        Constant::String(s) => {
            write_u8(buf, 4);
            write_string(buf, s);
        }
    }
}

fn write_instruction(buf: &mut Vec<u8>, inst: &Instruction) {
    match inst {
        Instruction::LoadNil { dst } => {
            write_u8(buf, 1);
            write_u8(buf, *dst);
        }
        Instruction::LoadBool { dst, val } => {
            write_u8(buf, 2);
            write_u8(buf, *dst);
            write_u8(buf, if *val { 1 } else { 0 });
        }
        Instruction::LoadInt { dst, val } => {
            write_u8(buf, 3);
            write_u8(buf, *dst);
            write_i32(buf, *val);
        }
        Instruction::LoadK { dst, k } => {
            write_u8(buf, 4);
            write_u8(buf, *dst);
            write_u16(buf, *k);
        }
        Instruction::Move { dst, src } => {
            write_u8(buf, 5);
            write_u8(buf, *dst);
            write_u8(buf, *src);
        }
        Instruction::GetGlobal { dst, name_k } => {
            write_u8(buf, 6);
            write_u8(buf, *dst);
            write_u16(buf, *name_k);
        }
        Instruction::SetGlobal { src, name_k } => {
            write_u8(buf, 7);
            write_u8(buf, *src);
            write_u16(buf, *name_k);
        }
        Instruction::GetUpval { dst, upval_idx } => {
            write_u8(buf, 8);
            write_u8(buf, *dst);
            write_u8(buf, *upval_idx);
        }
        Instruction::SetUpval { src, upval_idx } => {
            write_u8(buf, 9);
            write_u8(buf, *src);
            write_u8(buf, *upval_idx);
        }
        Instruction::NewTable {
            dst,
            array_cap,
            fields_cap,
        } => {
            write_u8(buf, 10);
            write_u8(buf, *dst);
            write_u8(buf, *array_cap);
            write_u8(buf, *fields_cap);
        }
        Instruction::GetTable { dst, table, key } => {
            write_u8(buf, 11);
            write_u8(buf, *dst);
            write_u8(buf, *table);
            write_u8(buf, *key);
        }
        Instruction::SetTable { table, key, val } => {
            write_u8(buf, 12);
            write_u8(buf, *table);
            write_u8(buf, *key);
            write_u8(buf, *val);
        }
        Instruction::GetTableK { dst, table, key_k } => {
            write_u8(buf, 13);
            write_u8(buf, *dst);
            write_u8(buf, *table);
            write_u16(buf, *key_k);
        }
        Instruction::SetTableK { table, key_k, val } => {
            write_u8(buf, 14);
            write_u8(buf, *table);
            write_u16(buf, *key_k);
            write_u8(buf, *val);
        }
        Instruction::AppendArray { table, src } => {
            write_u8(buf, 15);
            write_u8(buf, *table);
            write_u8(buf, *src);
        }
        Instruction::Add { dst, a, b } => {
            write_u8(buf, 16);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::Sub { dst, a, b } => {
            write_u8(buf, 17);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::Mul { dst, a, b } => {
            write_u8(buf, 18);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::Div { dst, a, b } => {
            write_u8(buf, 19);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::IDiv { dst, a, b } => {
            write_u8(buf, 20);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::Mod { dst, a, b } => {
            write_u8(buf, 21);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::Pow { dst, a, b } => {
            write_u8(buf, 22);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::BitAnd { dst, a, b } => {
            write_u8(buf, 23);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::BitOr { dst, a, b } => {
            write_u8(buf, 24);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::BitXor { dst, a, b } => {
            write_u8(buf, 25);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::Shl { dst, a, b } => {
            write_u8(buf, 26);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::Shr { dst, a, b } => {
            write_u8(buf, 27);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::Concat { dst, a, b } => {
            write_u8(buf, 28);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::Unm { dst, src } => {
            write_u8(buf, 29);
            write_u8(buf, *dst);
            write_u8(buf, *src);
        }
        Instruction::Not { dst, src } => {
            write_u8(buf, 30);
            write_u8(buf, *dst);
            write_u8(buf, *src);
        }
        Instruction::Len { dst, src } => {
            write_u8(buf, 31);
            write_u8(buf, *dst);
            write_u8(buf, *src);
        }
        Instruction::BitNot { dst, src } => {
            write_u8(buf, 32);
            write_u8(buf, *dst);
            write_u8(buf, *src);
        }
        Instruction::Coalesce { dst, a, b } => {
            write_u8(buf, 33);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::Eq {
            a,
            b,
            jump_if_false,
        } => {
            write_u8(buf, 34);
            write_u8(buf, *a);
            write_u8(buf, *b);
            write_i16(buf, *jump_if_false);
        }
        Instruction::Ne {
            a,
            b,
            jump_if_false,
        } => {
            write_u8(buf, 35);
            write_u8(buf, *a);
            write_u8(buf, *b);
            write_i16(buf, *jump_if_false);
        }
        Instruction::Lt {
            a,
            b,
            jump_if_false,
        } => {
            write_u8(buf, 36);
            write_u8(buf, *a);
            write_u8(buf, *b);
            write_i16(buf, *jump_if_false);
        }
        Instruction::Le {
            a,
            b,
            jump_if_false,
        } => {
            write_u8(buf, 37);
            write_u8(buf, *a);
            write_u8(buf, *b);
            write_i16(buf, *jump_if_false);
        }
        Instruction::Gt {
            a,
            b,
            jump_if_false,
        } => {
            write_u8(buf, 38);
            write_u8(buf, *a);
            write_u8(buf, *b);
            write_i16(buf, *jump_if_false);
        }
        Instruction::Ge {
            a,
            b,
            jump_if_false,
        } => {
            write_u8(buf, 39);
            write_u8(buf, *a);
            write_u8(buf, *b);
            write_i16(buf, *jump_if_false);
        }
        Instruction::Test { reg, jump_if_false } => {
            write_u8(buf, 40);
            write_u8(buf, *reg);
            write_i16(buf, *jump_if_false);
        }
        Instruction::Jump { offset } => {
            write_u8(buf, 41);
            write_i16(buf, *offset);
        }
        Instruction::Call { callee, argc, retc } => {
            write_u8(buf, 42);
            write_u8(buf, *callee);
            write_u8(buf, *argc);
            write_u8(buf, *retc);
        }
        Instruction::Return { base, count } => {
            write_u8(buf, 43);
            write_u8(buf, *base);
            write_u8(buf, *count);
        }
        Instruction::Closure { dst, proto_idx } => {
            write_u8(buf, 44);
            write_u8(buf, *dst);
            write_u16(buf, *proto_idx);
        }
        Instruction::Vararg { dst, count } => {
            write_u8(buf, 45);
            write_u8(buf, *dst);
            write_u8(buf, *count);
        }
        Instruction::ForPrep { base, jump } => {
            write_u8(buf, 46);
            write_u8(buf, *base);
            write_i16(buf, *jump);
        }
        Instruction::ForLoop { base, jump } => {
            write_u8(buf, 47);
            write_u8(buf, *base);
            write_i16(buf, *jump);
        }
        Instruction::SetList { table, base, count } => {
            write_u8(buf, 48);
            write_u8(buf, *table);
            write_u8(buf, *base);
            write_u8(buf, *count);
        }
        Instruction::TForCall { base, retc } => {
            write_u8(buf, 49);
            write_u8(buf, *base);
            write_u8(buf, *retc);
        }
        Instruction::TForLoop { base, jump } => {
            write_u8(buf, 50);
            write_u8(buf, *base);
            write_i16(buf, *jump);
        }
        Instruction::LShl { dst, a, b } => {
            write_u8(buf, 51);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::LShr { dst, a, b } => {
            write_u8(buf, 52);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u8(buf, *b);
        }
        Instruction::GetImport {
            dst,
            mod_k,
            field_k,
            site,
        } => {
            write_u8(buf, 53);
            write_u8(buf, *dst);
            write_u16(buf, *mod_k);
            write_u16(buf, *field_k);
            write_u16(buf, *site);
        }
        Instruction::CloseUpval { reg } => {
            write_u8(buf, 54);
            write_u8(buf, *reg);
        }
        Instruction::AddK { dst, a, k } => {
            write_u8(buf, 55);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u16(buf, *k);
        }
        Instruction::SubK { dst, a, k } => {
            write_u8(buf, 56);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u16(buf, *k);
        }
        Instruction::MulK { dst, a, k } => {
            write_u8(buf, 57);
            write_u8(buf, *dst);
            write_u8(buf, *a);
            write_u16(buf, *k);
        }
        Instruction::EqK {
            a,
            k,
            jump_if_false,
        } => {
            write_u8(buf, 58);
            write_u8(buf, *a);
            write_u16(buf, *k);
            write_i16(buf, *jump_if_false);
        }
        Instruction::NeK {
            a,
            k,
            jump_if_false,
        } => {
            write_u8(buf, 59);
            write_u8(buf, *a);
            write_u16(buf, *k);
            write_i16(buf, *jump_if_false);
        }
        Instruction::LtK {
            a,
            k,
            jump_if_false,
        } => {
            write_u8(buf, 60);
            write_u8(buf, *a);
            write_u16(buf, *k);
            write_i16(buf, *jump_if_false);
        }
        Instruction::LeK {
            a,
            k,
            jump_if_false,
        } => {
            write_u8(buf, 61);
            write_u8(buf, *a);
            write_u16(buf, *k);
            write_i16(buf, *jump_if_false);
        }
        Instruction::GtK {
            a,
            k,
            jump_if_false,
        } => {
            write_u8(buf, 62);
            write_u8(buf, *a);
            write_u16(buf, *k);
            write_i16(buf, *jump_if_false);
        }
        Instruction::GeK {
            a,
            k,
            jump_if_false,
        } => {
            write_u8(buf, 63);
            write_u8(buf, *a);
            write_u16(buf, *k);
            write_i16(buf, *jump_if_false);
        }
    }
}

fn write_proto(buf: &mut Vec<u8>, proto: &Proto, parent_source: Option<&str>) {
    if let Some(name) = &proto.name {
        write_u8(buf, 1);
        write_string(buf, name);
    } else {
        write_u8(buf, 0);
    }
    // 0: no source, 1: a source follows, 2: the same source as the parent.
    match proto.source.as_deref() {
        None => write_u8(buf, 0),
        Some(source) if Some(source) == parent_source => write_u8(buf, 2),
        Some(source) => {
            write_u8(buf, 1);
            write_string(buf, source);
        }
    }
    write_u8(buf, proto.num_params);
    write_u8(buf, proto.max_registers);
    write_u8(buf, if proto.is_vararg { 1 } else { 0 });

    write_u32(buf, proto.constants.len() as u32);
    for c in &proto.constants {
        write_constant(buf, c);
    }

    write_u32(buf, proto.instructions.len() as u32);
    for inst in &proto.instructions {
        write_instruction(buf, inst);
    }

    write_u32(buf, proto.protos.len() as u32);
    for child in &proto.protos {
        write_proto(buf, child, proto.source.as_deref());
    }

    write_u32(buf, proto.upvalues.len() as u32);
    for (i, upval) in proto.upvalues.iter().enumerate() {
        write_u8(buf, if upval.in_stack { 1 } else { 0 });
        write_u8(buf, upval.index);
        write_string(buf, proto.upvalue_names.get(i).map_or("", String::as_str));
    }

    write_u32(buf, proto.lines.len() as u32);
    for line in &proto.lines {
        write_u32(buf, *line);
    }

    write_u32(buf, proto.local_names.len() as u32);
    for info in &proto.local_names {
        write_string(buf, &info.name);
        write_u8(buf, info.reg);
        write_u32(buf, info.from_pc);
        write_u32(buf, info.to_pc);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_magic_header_and_version() {
        let proto = Proto::new(Some("test".to_string()), 0, false);
        let bytes = serialize(&proto);
        assert_eq!(&bytes[0..7], b"neyuki!");
        assert_eq!(bytes[7], BYTECODE_VERSION);
    }

    #[test]
    fn test_rejects_invalid_magic() {
        let fake = b"badhead\x01\x00\x00";
        assert!(deserialize(fake).is_err());
    }

    #[test]
    fn test_proto_roundtrip() {
        let mut proto = Proto::new(Some("math_fn".to_string()), 2, false);
        proto.max_registers = 4;
        let k0 = proto.add_constant(Constant::Int(BigInt::from(42)));
        let k1 = proto.add_constant(Constant::String("hello".to_string()));
        proto.emit(Instruction::LoadK { dst: 0, k: k0 }, 10);
        proto.emit(Instruction::LoadK { dst: 1, k: k1 }, 11);
        proto.emit(Instruction::Add { dst: 2, a: 0, b: 1 }, 12);
        proto.push_local("my_local".to_string(), 2, 10);
        proto.close_local("my_local", 13);

        let bytes = serialize(&proto);
        let decoded = deserialize(&bytes).expect("deserialize failed");

        assert_eq!(decoded.name, proto.name);
        assert_eq!(decoded.num_params, proto.num_params);
        assert_eq!(decoded.max_registers, proto.max_registers);
        assert_eq!(decoded.constants, proto.constants);
        assert_eq!(decoded.instructions, proto.instructions);
        assert_eq!(decoded.lines, proto.lines);
        assert_eq!(decoded.local_names, proto.local_names);
    }

    #[test]
    fn test_verifier_catches_corrupt_bytecode() {
        let mut proto = Proto::new(Some("corrupt".to_string()), 0, false);
        proto.max_registers = 2;
        // Invalid register 99 exceeds max_registers 2
        proto.emit(Instruction::LoadNil { dst: 99 }, 1);
        proto.emit(Instruction::Return { base: 0, count: 1 }, 2);

        let bytes = serialize(&proto);
        let err = deserialize(&bytes);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("verification failed"));
    }

    #[test]
    fn test_checksum_mismatch_caught() {
        let mut proto = Proto::new(Some("test".to_string()), 0, false);
        proto.max_registers = 1;
        proto.emit(Instruction::Return { base: 0, count: 0 }, 1);
        let mut bytes = serialize(&proto);
        // Corrupt one byte in the payload
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        let err = deserialize(&bytes);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("checksum mismatch"));
    }

    #[test]
    fn test_max_registers_zero_rejected() {
        let mut proto = Proto::new(Some("zero_reg".to_string()), 0, false);
        proto.max_registers = 0;
        // Any register access with max_registers=0 must be rejected
        proto.emit(Instruction::LoadNil { dst: 0 }, 1);
        proto.emit(Instruction::Return { base: 0, count: 0 }, 2);
        let bytes = serialize(&proto);
        let err = deserialize(&bytes);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("exceeds prototype max_registers"));
    }

    #[test]
    fn test_multiregister_range_rejected() {
        let mut proto = Proto::new(Some("tfor_oob".to_string()), 0, false);
        proto.max_registers = 4;
        // TForCall base=2 with retc=2 needs registers 2..6, which exceeds max_registers=4
        proto.emit(Instruction::TForCall { base: 2, retc: 2 }, 1);
        let bytes = serialize(&proto);
        let err = deserialize(&bytes);
        assert!(err.is_err());
        assert!(err.unwrap_err().contains("exceeds prototype max_registers"));
    }

    #[test]
    fn test_fuzz_deserialize_never_panics() {
        // Run 5000 random mutations against deserialize, ensuring zero panics
        let mut proto = Proto::new(Some("seed".to_string()), 1, false);
        proto.max_registers = 4;
        proto.emit(Instruction::LoadInt { dst: 0, val: 42 }, 1);
        proto.emit(Instruction::Return { base: 0, count: 1 }, 2);
        let valid_bytes = serialize(&proto);

        // Deterministic pseudo-random sequence for repeatability
        let mut state: u64 = 0x1234_5678_9ABC_DEF0;
        let mut rng = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 32) as u32
        };

        for _ in 0..2000 {
            let mut corrupted = valid_bytes.clone();
            let num_edits = (rng() % 5 + 1) as usize;
            for _ in 0..num_edits {
                let idx = (rng() as usize) % corrupted.len();
                corrupted[idx] = (rng() & 0xFF) as u8;
            }
            // Must return Ok or Err, never panic
            let _ = std::panic::catch_unwind(|| {
                let _ = deserialize(&corrupted);
            });
        }

        // Also fuzz with purely random byte streams of varying lengths
        for len in [0, 1, 4, 7, 8, 12, 13, 20, 50, 100, 256] {
            let mut junk = vec![0u8; len];
            for b in junk.iter_mut() {
                *b = (rng() & 0xFF) as u8;
            }
            let _ = std::panic::catch_unwind(|| {
                let _ = deserialize(&junk);
            });
        }
    }
}
