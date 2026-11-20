pub mod deserialize;
pub mod disasm;
pub mod format;
pub mod instruction;
pub mod parser;
pub mod proto;
pub mod serialize;
pub mod verify;

pub use deserialize::deserialize;
pub use disasm::disassemble_proto;
pub use format::{
    BYTECODE_VERSION, BytecodeHeader, MAGIC, MIN_SUPPORTED_BYTECODE_VERSION, is_supported_version,
};
pub use instruction::Instruction;
pub use parser::parse_assembly;
pub use proto::{Constant, Proto, UpvalueDesc};
pub use serialize::serialize;
pub use verify::{BytecodeVerifyError, verify_proto};
