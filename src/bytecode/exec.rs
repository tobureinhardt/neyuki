//! Compact, zero-`RefCell` execution instructions and inline cache storage
//! materialized lazily on first execution of a [`Proto`].

use std::cell::{Cell, RefCell, UnsafeCell};
use std::rc::Rc;

use crate::bytecode::instruction::{Instruction, MULTRET};
use crate::vm::value::{Value, VmTable};

/// Inline cache entry for `GetGlobal` at a specific `GetGlobal` site.
#[derive(Clone)]
pub struct GlobalCacheSlot {
    pub epoch: u64,
    pub value: Value,
}

impl Default for GlobalCacheSlot {
    #[inline]
    fn default() -> Self {
        Self {
            epoch: u64::MAX,
            value: Value::Nil,
        }
    }
}

/// Inline cache entry for `GetTableK` / `SetTableK` at a specific site:
/// caches the table pointer, `FieldMap::shape_version`, and direct entry slot
/// index so repeated reads/writes skip hashing and probing.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct TableCacheSlot {
    pub table_ptr: usize,
    pub shape_version: u32,
    pub slot_idx: u32,
}

impl Default for TableCacheSlot {
    #[inline]
    fn default() -> Self {
        Self {
            table_ptr: 0,
            shape_version: u32::MAX,
            slot_idx: 0,
        }
    }
}

const _: () = assert!(std::mem::size_of::<TableCacheSlot>() == 16);

/// One cached `module.field` resolution for a `GetImport` site.
#[derive(Clone)]
pub struct ImportSlot {
    pub epoch: u64,
    pub module: Option<Rc<RefCell<VmTable>>>,
    pub value: Value,
}

impl Default for ImportSlot {
    #[inline]
    fn default() -> Self {
        Self {
            epoch: u64::MAX,
            module: None,
            value: Value::Nil,
        }
    }
}

/// Execution-only instruction format combining standard opcodes, fused
/// `TailCall`, dense cache site indices, and in-place quickened specializations
/// without a secondary `RefCell` lookup.
#[repr(u8)]
#[derive(Clone, Copy, Debug)]
pub enum ExecInst {
    LoadNil {
        dst: u8,
    },
    LoadBool {
        dst: u8,
        val: bool,
    },
    LoadInt {
        dst: u8,
        val: i32,
    },
    LoadK {
        dst: u8,
        k: u16,
    },
    Move {
        dst: u8,
        src: u8,
    },

    GetGlobal {
        dst: u8,
        name_k: u16,
        site: u16,
    },
    GetGlobalDynamic {
        dst: u8,
        name_k: u16,
    },
    SetGlobal {
        src: u8,
        name_k: u16,
    },

    GetUpval {
        dst: u8,
        upval_idx: u8,
    },
    SetUpval {
        src: u8,
        upval_idx: u8,
    },

    NewTable {
        dst: u8,
        array_cap: u8,
        fields_cap: u8,
    },
    GetTable {
        dst: u8,
        table: u8,
        key: u8,
    },
    SetTable {
        table: u8,
        key: u8,
        val: u8,
    },
    GetTableK {
        dst: u8,
        table: u8,
        key_k: u16,
        site: u16,
    },
    GetTableKDynamic {
        dst: u8,
        table: u8,
        key_k: u16,
    },
    SetTableK {
        table: u8,
        key_k: u16,
        val: u8,
        site: u16,
    },
    SetTableKDynamic {
        table: u8,
        key_k: u16,
        val: u8,
    },
    AppendArray {
        table: u8,
        src: u8,
    },
    GetImport {
        dst: u8,
        mod_k: u16,
        field_k: u16,
        site: u16,
    },

    Add {
        dst: u8,
        a: u8,
        b: u8,
    },
    AddInt {
        dst: u8,
        a: u8,
        b: u8,
    },
    AddFloat {
        dst: u8,
        a: u8,
        b: u8,
    },
    AddGeneric {
        dst: u8,
        a: u8,
        b: u8,
    },

    AddK {
        dst: u8,
        a: u8,
        k: u16,
    },
    AddIntK {
        dst: u8,
        a: u8,
        k: u16,
    },
    AddFloatK {
        dst: u8,
        a: u8,
        k: u16,
    },
    AddKGeneric {
        dst: u8,
        a: u8,
        k: u16,
    },

    Sub {
        dst: u8,
        a: u8,
        b: u8,
    },
    SubInt {
        dst: u8,
        a: u8,
        b: u8,
    },
    SubFloat {
        dst: u8,
        a: u8,
        b: u8,
    },
    SubGeneric {
        dst: u8,
        a: u8,
        b: u8,
    },

    SubK {
        dst: u8,
        a: u8,
        k: u16,
    },
    SubIntK {
        dst: u8,
        a: u8,
        k: u16,
    },
    SubFloatK {
        dst: u8,
        a: u8,
        k: u16,
    },
    SubKGeneric {
        dst: u8,
        a: u8,
        k: u16,
    },

    Mul {
        dst: u8,
        a: u8,
        b: u8,
    },
    MulInt {
        dst: u8,
        a: u8,
        b: u8,
    },
    MulFloat {
        dst: u8,
        a: u8,
        b: u8,
    },
    MulGeneric {
        dst: u8,
        a: u8,
        b: u8,
    },

    MulK {
        dst: u8,
        a: u8,
        k: u16,
    },
    MulIntK {
        dst: u8,
        a: u8,
        k: u16,
    },
    MulFloatK {
        dst: u8,
        a: u8,
        k: u16,
    },
    MulKGeneric {
        dst: u8,
        a: u8,
        k: u16,
    },

    Div {
        dst: u8,
        a: u8,
        b: u8,
    },
    IDiv {
        dst: u8,
        a: u8,
        b: u8,
    },
    IDivK {
        dst: u8,
        a: u8,
        k: u16,
    },
    IDivIntPow2 {
        dst: u8,
        a: u8,
        shift: u8,
    },
    IDivIntK {
        dst: u8,
        a: u8,
        k: u16,
    },
    IDivKGeneric {
        dst: u8,
        a: u8,
        k: u16,
    },
    Mod {
        dst: u8,
        a: u8,
        b: u8,
    },
    ModK {
        dst: u8,
        a: u8,
        k: u16,
    },
    ModIntPow2 {
        dst: u8,
        a: u8,
        shift: u8,
    },
    ModIntK {
        dst: u8,
        a: u8,
        k: u16,
    },
    ModKGeneric {
        dst: u8,
        a: u8,
        k: u16,
    },
    Pow {
        dst: u8,
        a: u8,
        b: u8,
    },

    BitAnd {
        dst: u8,
        a: u8,
        b: u8,
    },
    BitOr {
        dst: u8,
        a: u8,
        b: u8,
    },
    BitXor {
        dst: u8,
        a: u8,
        b: u8,
    },
    Shl {
        dst: u8,
        a: u8,
        b: u8,
    },
    Shr {
        dst: u8,
        a: u8,
        b: u8,
    },
    LShl {
        dst: u8,
        a: u8,
        b: u8,
    },
    LShr {
        dst: u8,
        a: u8,
        b: u8,
    },

    Concat {
        dst: u8,
        a: u8,
        b: u8,
    },

    Unm {
        dst: u8,
        src: u8,
    },
    Not {
        dst: u8,
        src: u8,
    },
    Len {
        dst: u8,
        src: u8,
    },
    BitNot {
        dst: u8,
        src: u8,
    },
    Coalesce {
        dst: u8,
        a: u8,
        b: u8,
    },

    Eq {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },
    Ne {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },
    Lt {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },
    LtInt {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },
    LtGeneric {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },
    Le {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },
    LeInt {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },
    LeGeneric {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },
    Gt {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },
    Ge {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },

    EqK {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },
    EqIntK {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },
    EqKGeneric {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },
    NeK {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },
    LtK {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },
    LtIntK {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },
    LtKGeneric {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },
    LeK {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },
    LeIntK {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },
    LeKGeneric {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },
    GtK {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },
    GeK {
        a: u8,
        k: u16,
        jump_if_false: i16,
    },

    Test {
        reg: u8,
        jump_if_false: i16,
    },
    Jump {
        offset: i16,
    },

    Call {
        callee: u8,
        argc: u8,
        retc: u8,
    },
    TailCall {
        callee: u8,
        argc: u8,
    },
    Return {
        base: u8,
        count: u8,
    },
    Closure {
        dst: u8,
        proto_idx: u16,
    },
    Vararg {
        dst: u8,
        count: u8,
    },

    ForPrep {
        base: u8,
        jump: i16,
    },
    ForLoop {
        base: u8,
        jump: i16,
    },
    ForLoopInt {
        base: u8,
        jump: i16,
    },
    ForLoopGeneric {
        base: u8,
        jump: i16,
    },

    SetList {
        table: u8,
        base: u8,
        count: u8,
    },
    TForCall {
        base: u8,
        retc: u8,
    },
    TForLoop {
        base: u8,
        jump: i16,
    },
    CloseUpval {
        reg: u8,
    },

    AddI {
        dst: u8,
        a: u8,
        imm: i16,
    },
    AddIntI {
        dst: u8,
        a: u8,
        imm: i16,
    },
    AddIGeneric {
        dst: u8,
        a: u8,
        imm: i16,
    },
    SubI {
        dst: u8,
        a: u8,
        imm: i16,
    },
    SubIntI {
        dst: u8,
        a: u8,
        imm: i16,
    },
    SubIGeneric {
        dst: u8,
        a: u8,
        imm: i16,
    },
    MulI {
        dst: u8,
        a: u8,
        imm: i16,
    },
    MulIntI {
        dst: u8,
        a: u8,
        imm: i16,
    },
    MulIGeneric {
        dst: u8,
        a: u8,
        imm: i16,
    },
    ShlI {
        dst: u8,
        a: u8,
        imm: u8,
    },
    ShrI {
        dst: u8,
        a: u8,
        imm: u8,
    },

    EqI {
        a: u8,
        imm: i16,
        jump_if_false: i16,
    },
    NeI {
        a: u8,
        imm: i16,
        jump_if_false: i16,
    },
    LtI {
        a: u8,
        imm: i16,
        jump_if_false: i16,
    },
    LtIntI {
        a: u8,
        imm: i16,
        jump_if_false: i16,
    },
    LtIGeneric {
        a: u8,
        imm: i16,
        jump_if_false: i16,
    },
    LeI {
        a: u8,
        imm: i16,
        jump_if_false: i16,
    },
    LeIntI {
        a: u8,
        imm: i16,
        jump_if_false: i16,
    },
    LeIGeneric {
        a: u8,
        imm: i16,
        jump_if_false: i16,
    },
    GtI {
        a: u8,
        imm: i16,
        jump_if_false: i16,
    },
    GeI {
        a: u8,
        imm: i16,
        jump_if_false: i16,
    },

    /// Sentinel at `ops[instructions.len()]` so verified jumps to `len` terminate cleanly.
    EndSentinel,

    /// Countdown-specialized integer `ForLoop` where `R[base+1]` stores remaining iterations.
    ForLoopCount {
        base: u8,
        jump: i16,
    },

    /// Countdown-specialized integer `ForLoop` with unit step (`step == 1`).
    ForLoopCount1 {
        base: u8,
        jump: i16,
    },
}

const _: () = assert!(std::mem::size_of::<ExecInst>() == 8);

pub const NUM_EXEC_OPS: usize = 129;

impl ExecInst {
    #[inline(always)]
    pub fn tag(self) -> usize {
        match self {
            ExecInst::LoadNil { .. } => 0,
            ExecInst::LoadBool { .. } => 1,
            ExecInst::LoadInt { .. } => 2,
            ExecInst::LoadK { .. } => 3,
            ExecInst::Move { .. } => 4,
            ExecInst::GetGlobal { .. } => 5,
            ExecInst::GetGlobalDynamic { .. } => 6,
            ExecInst::SetGlobal { .. } => 7,
            ExecInst::GetUpval { .. } => 8,
            ExecInst::SetUpval { .. } => 9,
            ExecInst::NewTable { .. } => 10,
            ExecInst::GetTable { .. } => 11,
            ExecInst::SetTable { .. } => 12,
            ExecInst::GetTableK { .. } => 13,
            ExecInst::GetTableKDynamic { .. } => 14,
            ExecInst::SetTableK { .. } => 15,
            ExecInst::SetTableKDynamic { .. } => 16,
            ExecInst::AppendArray { .. } => 17,
            ExecInst::GetImport { .. } => 18,
            ExecInst::Add { .. } => 19,
            ExecInst::AddInt { .. } => 20,
            ExecInst::AddFloat { .. } => 21,
            ExecInst::AddGeneric { .. } => 22,
            ExecInst::AddK { .. } => 23,
            ExecInst::AddIntK { .. } => 24,
            ExecInst::AddFloatK { .. } => 25,
            ExecInst::AddKGeneric { .. } => 26,
            ExecInst::Sub { .. } => 27,
            ExecInst::SubInt { .. } => 28,
            ExecInst::SubFloat { .. } => 29,
            ExecInst::SubGeneric { .. } => 30,
            ExecInst::SubK { .. } => 31,
            ExecInst::SubIntK { .. } => 32,
            ExecInst::SubFloatK { .. } => 33,
            ExecInst::SubKGeneric { .. } => 34,
            ExecInst::Mul { .. } => 35,
            ExecInst::MulInt { .. } => 36,
            ExecInst::MulFloat { .. } => 37,
            ExecInst::MulGeneric { .. } => 38,
            ExecInst::MulK { .. } => 39,
            ExecInst::MulIntK { .. } => 40,
            ExecInst::MulFloatK { .. } => 41,
            ExecInst::MulKGeneric { .. } => 42,
            ExecInst::Div { .. } => 43,
            ExecInst::IDiv { .. } => 44,
            ExecInst::IDivK { .. } => 45,
            ExecInst::IDivIntPow2 { .. } => 46,
            ExecInst::IDivIntK { .. } => 47,
            ExecInst::IDivKGeneric { .. } => 48,
            ExecInst::Mod { .. } => 49,
            ExecInst::ModK { .. } => 50,
            ExecInst::ModIntPow2 { .. } => 51,
            ExecInst::ModIntK { .. } => 52,
            ExecInst::ModKGeneric { .. } => 53,
            ExecInst::Pow { .. } => 54,
            ExecInst::BitAnd { .. } => 55,
            ExecInst::BitOr { .. } => 56,
            ExecInst::BitXor { .. } => 57,
            ExecInst::Shl { .. } => 58,
            ExecInst::Shr { .. } => 59,
            ExecInst::LShl { .. } => 60,
            ExecInst::LShr { .. } => 61,
            ExecInst::Concat { .. } => 62,
            ExecInst::Unm { .. } => 63,
            ExecInst::Not { .. } => 64,
            ExecInst::Len { .. } => 65,
            ExecInst::BitNot { .. } => 66,
            ExecInst::Coalesce { .. } => 67,
            ExecInst::Eq { .. } => 68,
            ExecInst::Ne { .. } => 69,
            ExecInst::Lt { .. } => 70,
            ExecInst::LtInt { .. } => 71,
            ExecInst::LtGeneric { .. } => 72,
            ExecInst::Le { .. } => 73,
            ExecInst::LeInt { .. } => 74,
            ExecInst::LeGeneric { .. } => 75,
            ExecInst::Gt { .. } => 76,
            ExecInst::Ge { .. } => 77,
            ExecInst::EqK { .. } => 78,
            ExecInst::EqIntK { .. } => 79,
            ExecInst::EqKGeneric { .. } => 80,
            ExecInst::NeK { .. } => 81,
            ExecInst::LtK { .. } => 82,
            ExecInst::LtIntK { .. } => 83,
            ExecInst::LtKGeneric { .. } => 84,
            ExecInst::LeK { .. } => 85,
            ExecInst::LeIntK { .. } => 86,
            ExecInst::LeKGeneric { .. } => 87,
            ExecInst::GtK { .. } => 88,
            ExecInst::GeK { .. } => 89,
            ExecInst::Test { .. } => 90,
            ExecInst::Jump { .. } => 91,
            ExecInst::Call { .. } => 92,
            ExecInst::TailCall { .. } => 93,
            ExecInst::Return { .. } => 94,
            ExecInst::Closure { .. } => 95,
            ExecInst::Vararg { .. } => 96,
            ExecInst::ForPrep { .. } => 97,
            ExecInst::ForLoop { .. } => 98,
            ExecInst::ForLoopInt { .. } => 99,
            ExecInst::ForLoopGeneric { .. } => 100,
            ExecInst::SetList { .. } => 101,
            ExecInst::TForCall { .. } => 102,
            ExecInst::TForLoop { .. } => 103,
            ExecInst::CloseUpval { .. } => 104,
            ExecInst::AddI { .. } => 105,
            ExecInst::AddIntI { .. } => 106,
            ExecInst::AddIGeneric { .. } => 107,
            ExecInst::SubI { .. } => 108,
            ExecInst::SubIntI { .. } => 109,
            ExecInst::SubIGeneric { .. } => 110,
            ExecInst::MulI { .. } => 111,
            ExecInst::MulIntI { .. } => 112,
            ExecInst::MulIGeneric { .. } => 113,
            ExecInst::ShlI { .. } => 114,
            ExecInst::ShrI { .. } => 115,
            ExecInst::EqI { .. } => 116,
            ExecInst::NeI { .. } => 117,
            ExecInst::LtI { .. } => 118,
            ExecInst::LtIntI { .. } => 119,
            ExecInst::LtIGeneric { .. } => 120,
            ExecInst::LeI { .. } => 121,
            ExecInst::LeIntI { .. } => 122,
            ExecInst::LeIGeneric { .. } => 123,
            ExecInst::GtI { .. } => 124,
            ExecInst::GeI { .. } => 125,
            ExecInst::EndSentinel => 126,
            ExecInst::ForLoopCount { .. } => 127,
            ExecInst::ForLoopCount1 { .. } => 128,
        }
    }
}

/// 8-byte aligned execution instruction cell so indirect dispatch fetches with
/// a single aligned 64-bit load across all variants.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug)]
pub struct AlignedExec(pub ExecInst);

const _: () = assert!(std::mem::size_of::<AlignedExec>() == 8);
const _: () = assert!(std::mem::align_of::<AlignedExec>() == 8);

/// Pre-lowered execution stream and inline cache arrays for a `Proto`.
pub struct ExecProgram {
    pub ops: Box<[Cell<AlignedExec>]>,
    pub table_caches: Box<[Cell<TableCacheSlot>]>,
    global_caches: UnsafeCell<Box<[GlobalCacheSlot]>>,
    import_caches: UnsafeCell<Box<[ImportSlot]>>,
}

impl ExecProgram {
    /// Builds the execution stream and dense cache tables from verified bytecode.
    pub fn compile(instructions: &[Instruction]) -> Self {
        let mut ops = Vec::with_capacity(instructions.len() + 1);
        let mut table_sites: u16 = 0;
        let mut global_sites: u16 = 0;
        let mut import_len: usize = 0;

        for (ip, &inst) in instructions.iter().enumerate() {
            let exec = match inst {
                Instruction::LoadNil { dst } => ExecInst::LoadNil { dst },
                Instruction::LoadBool { dst, val } => ExecInst::LoadBool { dst, val },
                Instruction::LoadInt { dst, val } => ExecInst::LoadInt { dst, val },
                Instruction::LoadK { dst, k } => ExecInst::LoadK { dst, k },
                Instruction::Move { dst, src } => ExecInst::Move { dst, src },
                Instruction::GetGlobal { dst, name_k } => {
                    if global_sites < u16::MAX {
                        let site = global_sites;
                        global_sites += 1;
                        ExecInst::GetGlobal { dst, name_k, site }
                    } else {
                        ExecInst::GetGlobalDynamic { dst, name_k }
                    }
                }
                Instruction::SetGlobal { src, name_k } => ExecInst::SetGlobal { src, name_k },
                Instruction::GetUpval { dst, upval_idx } => ExecInst::GetUpval { dst, upval_idx },
                Instruction::SetUpval { src, upval_idx } => ExecInst::SetUpval { src, upval_idx },
                Instruction::NewTable {
                    dst,
                    array_cap,
                    fields_cap,
                } => ExecInst::NewTable {
                    dst,
                    array_cap,
                    fields_cap,
                },
                Instruction::GetTable { dst, table, key } => ExecInst::GetTable { dst, table, key },
                Instruction::SetTable { table, key, val } => ExecInst::SetTable { table, key, val },
                Instruction::GetTableK { dst, table, key_k } => {
                    if table_sites < u16::MAX {
                        let site = table_sites;
                        table_sites += 1;
                        ExecInst::GetTableK {
                            dst,
                            table,
                            key_k,
                            site,
                        }
                    } else {
                        ExecInst::GetTableKDynamic { dst, table, key_k }
                    }
                }
                Instruction::SetTableK { table, key_k, val } => {
                    if table_sites < u16::MAX {
                        let site = table_sites;
                        table_sites += 1;
                        ExecInst::SetTableK {
                            table,
                            key_k,
                            val,
                            site,
                        }
                    } else {
                        ExecInst::SetTableKDynamic { table, key_k, val }
                    }
                }
                Instruction::AppendArray { table, src } => ExecInst::AppendArray { table, src },
                Instruction::GetImport {
                    dst,
                    mod_k,
                    field_k,
                    site,
                } => {
                    import_len = import_len.max(site as usize + 1);
                    ExecInst::GetImport {
                        dst,
                        mod_k,
                        field_k,
                        site,
                    }
                }
                Instruction::Add { dst, a, b } => ExecInst::Add { dst, a, b },
                Instruction::AddK { dst, a, k } => ExecInst::AddK { dst, a, k },
                Instruction::Sub { dst, a, b } => ExecInst::Sub { dst, a, b },
                Instruction::SubK { dst, a, k } => ExecInst::SubK { dst, a, k },
                Instruction::Mul { dst, a, b } => ExecInst::Mul { dst, a, b },
                Instruction::MulK { dst, a, k } => ExecInst::MulK { dst, a, k },
                Instruction::Div { dst, a, b } => ExecInst::Div { dst, a, b },
                Instruction::IDiv { dst, a, b } => ExecInst::IDiv { dst, a, b },
                Instruction::IDivK { dst, a, k } => ExecInst::IDivK { dst, a, k },
                Instruction::Mod { dst, a, b } => ExecInst::Mod { dst, a, b },
                Instruction::ModK { dst, a, k } => ExecInst::ModK { dst, a, k },
                Instruction::Pow { dst, a, b } => ExecInst::Pow { dst, a, b },
                Instruction::BitAnd { dst, a, b } => ExecInst::BitAnd { dst, a, b },
                Instruction::BitOr { dst, a, b } => ExecInst::BitOr { dst, a, b },
                Instruction::BitXor { dst, a, b } => ExecInst::BitXor { dst, a, b },
                Instruction::Shl { dst, a, b } => ExecInst::Shl { dst, a, b },
                Instruction::Shr { dst, a, b } => ExecInst::Shr { dst, a, b },
                Instruction::LShl { dst, a, b } => ExecInst::LShl { dst, a, b },
                Instruction::LShr { dst, a, b } => ExecInst::LShr { dst, a, b },
                Instruction::Concat { dst, a, b } => ExecInst::Concat { dst, a, b },
                Instruction::Unm { dst, src } => ExecInst::Unm { dst, src },
                Instruction::Not { dst, src } => ExecInst::Not { dst, src },
                Instruction::Len { dst, src } => ExecInst::Len { dst, src },
                Instruction::BitNot { dst, src } => ExecInst::BitNot { dst, src },
                Instruction::Coalesce { dst, a, b } => ExecInst::Coalesce { dst, a, b },
                Instruction::Eq {
                    a,
                    b,
                    jump_if_false,
                } => ExecInst::Eq {
                    a,
                    b,
                    jump_if_false,
                },
                Instruction::Ne {
                    a,
                    b,
                    jump_if_false,
                } => ExecInst::Ne {
                    a,
                    b,
                    jump_if_false,
                },
                Instruction::Lt {
                    a,
                    b,
                    jump_if_false,
                } => ExecInst::Lt {
                    a,
                    b,
                    jump_if_false,
                },
                Instruction::Le {
                    a,
                    b,
                    jump_if_false,
                } => ExecInst::Le {
                    a,
                    b,
                    jump_if_false,
                },
                Instruction::Gt {
                    a,
                    b,
                    jump_if_false,
                } => ExecInst::Gt {
                    a,
                    b,
                    jump_if_false,
                },
                Instruction::Ge {
                    a,
                    b,
                    jump_if_false,
                } => ExecInst::Ge {
                    a,
                    b,
                    jump_if_false,
                },
                Instruction::EqK {
                    a,
                    k,
                    jump_if_false,
                } => ExecInst::EqK {
                    a,
                    k,
                    jump_if_false,
                },
                Instruction::NeK {
                    a,
                    k,
                    jump_if_false,
                } => ExecInst::NeK {
                    a,
                    k,
                    jump_if_false,
                },
                Instruction::LtK {
                    a,
                    k,
                    jump_if_false,
                } => ExecInst::LtK {
                    a,
                    k,
                    jump_if_false,
                },
                Instruction::LeK {
                    a,
                    k,
                    jump_if_false,
                } => ExecInst::LeK {
                    a,
                    k,
                    jump_if_false,
                },
                Instruction::GtK {
                    a,
                    k,
                    jump_if_false,
                } => ExecInst::GtK {
                    a,
                    k,
                    jump_if_false,
                },
                Instruction::GeK {
                    a,
                    k,
                    jump_if_false,
                } => ExecInst::GeK {
                    a,
                    k,
                    jump_if_false,
                },
                Instruction::Test { reg, jump_if_false } => ExecInst::Test { reg, jump_if_false },
                Instruction::Jump { offset } => ExecInst::Jump { offset },
                Instruction::Call {
                    callee,
                    argc,
                    retc: MULTRET,
                } if matches!(
                    instructions.get(ip + 1),
                    Some(Instruction::Return {
                        base,
                        count: MULTRET,
                    }) if *base == callee
                ) =>
                {
                    ExecInst::TailCall { callee, argc }
                }
                Instruction::Call { callee, argc, retc } => ExecInst::Call { callee, argc, retc },
                Instruction::Return { base, count } => ExecInst::Return { base, count },
                Instruction::Closure { dst, proto_idx } => ExecInst::Closure { dst, proto_idx },
                Instruction::Vararg { dst, count } => ExecInst::Vararg { dst, count },
                Instruction::ForPrep { base, jump } => ExecInst::ForPrep { base, jump },
                Instruction::ForLoop { base, jump } => ExecInst::ForLoop { base, jump },
                Instruction::SetList { table, base, count } => {
                    ExecInst::SetList { table, base, count }
                }
                Instruction::TForCall { base, retc } => ExecInst::TForCall { base, retc },
                Instruction::TForLoop { base, jump } => ExecInst::TForLoop { base, jump },
                Instruction::CloseUpval { reg } => ExecInst::CloseUpval { reg },
                Instruction::AddI { dst, a, imm } => ExecInst::AddI { dst, a, imm },
                Instruction::SubI { dst, a, imm } => ExecInst::SubI { dst, a, imm },
                Instruction::MulI { dst, a, imm } => ExecInst::MulI { dst, a, imm },
                Instruction::ShlI { dst, a, imm } => ExecInst::ShlI { dst, a, imm },
                Instruction::ShrI { dst, a, imm } => ExecInst::ShrI { dst, a, imm },
                Instruction::EqI {
                    a,
                    imm,
                    jump_if_false,
                } => ExecInst::EqI {
                    a,
                    imm,
                    jump_if_false,
                },
                Instruction::NeI {
                    a,
                    imm,
                    jump_if_false,
                } => ExecInst::NeI {
                    a,
                    imm,
                    jump_if_false,
                },
                Instruction::LtI {
                    a,
                    imm,
                    jump_if_false,
                } => ExecInst::LtI {
                    a,
                    imm,
                    jump_if_false,
                },
                Instruction::LeI {
                    a,
                    imm,
                    jump_if_false,
                } => ExecInst::LeI {
                    a,
                    imm,
                    jump_if_false,
                },
                Instruction::GtI {
                    a,
                    imm,
                    jump_if_false,
                } => ExecInst::GtI {
                    a,
                    imm,
                    jump_if_false,
                },
                Instruction::GeI {
                    a,
                    imm,
                    jump_if_false,
                } => ExecInst::GeI {
                    a,
                    imm,
                    jump_if_false,
                },
            };
            ops.push(Cell::new(AlignedExec(exec)));
        }
        ops.push(Cell::new(AlignedExec(ExecInst::EndSentinel)));

        let table_caches =
            vec![Cell::new(TableCacheSlot::default()); table_sites as usize].into_boxed_slice();
        let global_caches = UnsafeCell::new(
            vec![GlobalCacheSlot::default(); global_sites as usize].into_boxed_slice(),
        );
        let import_caches =
            UnsafeCell::new(vec![ImportSlot::default(); import_len].into_boxed_slice());

        Self {
            ops: ops.into_boxed_slice(),
            table_caches,
            global_caches,
            import_caches,
        }
    }

    #[inline(always)]
    pub fn table_cache_get(&self, site: u16) -> Option<TableCacheSlot> {
        let slot = self.table_caches.get(site as usize)?.get();
        (slot.table_ptr != 0).then_some(slot)
    }

    #[inline(always)]
    pub fn table_cache_set(&self, site: u16, slot: TableCacheSlot) {
        if let Some(cell) = self.table_caches.get(site as usize) {
            cell.set(slot);
        }
    }

    #[inline(always)]
    pub fn global_cache_get(&self, site: u16, epoch: u64) -> Option<Value> {
        self.global_cache_ref(site, epoch).cloned()
    }

    #[inline(always)]
    pub fn global_cache_ref(&self, site: u16, epoch: u64) -> Option<&Value> {
        // SAFETY: VM is single-threaded (`!Sync`); slice bounds stay fixed after init
        // and the returned reference is only used before any mutation of `global_caches`.
        let slots = unsafe { &*self.global_caches.get() };
        let slot = slots.get(site as usize)?;
        if slot.epoch == epoch {
            Some(&slot.value)
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn global_cache_set(&self, site: u16, epoch: u64, value: Value) {
        // SAFETY: VM is single-threaded; dropping the old `Value` happens on overwrite
        // outside any active borrow of `global_caches`.
        let slots = unsafe { &mut *self.global_caches.get() };
        if let Some(slot) = slots.get_mut(site as usize) {
            slot.epoch = epoch;
            slot.value = value;
        }
    }

    #[inline(always)]
    pub fn import_cached(&self, site: usize, epoch: u64) -> Option<Value> {
        // SAFETY: VM is single-threaded and module metatable check + clone do not
        // re-enter `import_caches`.
        let slots = unsafe { &*self.import_caches.get() };
        let slot = slots.get(site)?;
        if slot.epoch != epoch {
            return None;
        }
        let module = slot.module.as_ref()?;
        if module.borrow().metatable.is_some() {
            return None;
        }
        Some(slot.value.clone())
    }

    #[inline(always)]
    pub fn import_store(
        &self,
        site: usize,
        epoch: u64,
        module: Rc<RefCell<VmTable>>,
        value: Value,
    ) {
        // SAFETY: VM is single-threaded; slot write does not alias other callers.
        let slots = unsafe { &mut *self.import_caches.get() };
        if let Some(slot) = slots.get_mut(site) {
            slot.epoch = epoch;
            slot.module = Some(module);
            slot.value = value;
        }
    }
}
