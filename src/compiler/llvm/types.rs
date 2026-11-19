//! LLVM IR type mapping, tagged value layout, and identifier/literal escaping.

use std::fmt::Write;

use crate::sema::types::NeyukiType;

/// Runtime type tags stored in field 0 (`i8`) of `%NykVal = type { i8, i64 }`.
pub const TAG_NIL: u8 = 0;
pub const TAG_BOOL: u8 = 1;
pub const TAG_INT: u8 = 2;
pub const TAG_FLOAT: u8 = 3;
pub const TAG_STRING: u8 = 4;
pub const TAG_TABLE: u8 = 5;
pub const TAG_FN: u8 = 6;
pub const TAG_BUILTIN_PRINT: u8 = 7;

/// Describes the LLVM IR representation of a static [`NeyukiType`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LlvmTypeHint {
    Void,
    Bool,
    Int,
    Float,
    DynamicVal,
}

impl LlvmTypeHint {
    pub fn from_neyuki(ty: &NeyukiType) -> Self {
        match ty {
            NeyukiType::Nil => Self::Void,
            NeyukiType::Boolean => Self::Bool,
            NeyukiType::Int => Self::Int,
            NeyukiType::Float => Self::Float,
            _ => Self::DynamicVal,
        }
    }

    pub fn ir_comment_name(self) -> &'static str {
        match self {
            Self::Void => "nil",
            Self::Bool => "bool",
            Self::Int => "int",
            Self::Float => "float",
            Self::DynamicVal => "dyn",
        }
    }
}

/// Sanitizes an arbitrary Neyuki identifier into a valid LLVM IR symbol segment.
pub fn sanitize_ident(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            let _ = write!(out, "_{:x}", ch as u32);
        }
    }
    if out.is_empty() {
        out.push_str("anon");
    }
    out
}

/// Formats a Rust string into an LLVM IR NUL-terminated byte literal (`c"...\00"`)
/// alongside its exact byte length including the trailing NUL.
pub fn format_llvm_cstring(s: &str) -> (String, usize) {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(bytes.len() + 8);
    out.push_str("c\"");
    for &b in bytes {
        match b {
            b'\\' => out.push_str("\\5C"),
            b'"' => out.push_str("\\22"),
            b'\n' => out.push_str("\\0A"),
            b'\r' => out.push_str("\\0D"),
            b'\t' => out.push_str("\\09"),
            0x20..=0x7E => out.push(b as char),
            other => {
                let _ = write!(out, "\\{:02X}", other);
            }
        }
    }
    out.push_str("\\00\"");
    (out, bytes.len() + 1)
}
