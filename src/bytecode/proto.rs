//! Function prototypes and their constants.

use crate::bytecode::instruction::Instruction;
use crate::vm::value::{Value, VmTable};
use num_bigint::BigInt;
use std::cell::{Cell, OnceCell, RefCell};
use std::fmt;
use std::rc::Rc;

#[derive(Clone, Debug, PartialEq)]
pub enum Constant {
    Nil,
    Bool(bool),
    Int(BigInt),
    Float(f64),
    String(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpvalueDesc {
    // True if captures local variable in immediate parent, false if captures parent upvalue
    pub in_stack: bool,
    pub index: u8,
}

// Debug info: maps a local variable name to its allocated register and active instruction range
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalVarInfo {
    pub name: String,
    pub reg: u8,
    pub from_pc: u32,
    pub to_pc: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Proto {
    pub name: Option<String>,
    pub num_params: u8,
    pub max_registers: u8,
    pub is_vararg: bool,
    pub constants: Vec<Constant>,
    pub instructions: Vec<Instruction>,
    // Shared with every closure instantiated from the child, so creating a
    // closure never copies its code.
    pub protos: Vec<Rc<Proto>>,
    pub upvalues: Vec<UpvalueDesc>,
    pub lines: Vec<u32>,
    // Local variable debug info for traceback
    pub local_names: Vec<LocalVarInfo>,
    /// The chunk this function was compiled from, such as a file path.
    pub source: Option<Rc<str>>,
    /// Parallel to `upvalues`: the captured variables' names.
    pub upvalue_names: Vec<String>,
    // Runtime-derived state the VM attaches on first execution.
    pub cache: ProtoCache,
}

/// What the VM works out about a prototype the first time it runs it: the
/// constant pool as runtime values (so a constant loads with a refcount bump
/// instead of allocating a fresh string or bignum every time) and whether
/// the bytecode has passed verification. Derived data: it never takes part
/// in equality, and a copied proto rebuilds its own.
#[derive(Default)]
pub struct ProtoCache {
    values: OnceCell<Vec<Value>>,
    verified: Cell<bool>,
    imports: RefCell<Vec<ImportSlot>>,
    globals: RefCell<Vec<GlobalCacheSlot>>,
    tables: RefCell<Vec<TableCacheSlot>>,
    quick_ops: RefCell<Vec<QuickOp>>,
}

/// Inline cache entry for `GetGlobal` at a specific instruction index.
#[derive(Clone)]
pub struct GlobalCacheSlot {
    pub epoch: u64,
    pub value: Value,
}

impl Default for GlobalCacheSlot {
    fn default() -> Self {
        Self {
            epoch: u64::MAX,
            value: Value::Nil,
        }
    }
}

/// Inline cache entry for `GetTableK` / `SetTableK` at a specific instruction
/// index: caches the table pointer, `FieldMap::shape_version`, and the direct
/// entry slot index so repeated reads/writes skip hashing and probing.
#[derive(Clone, Copy)]
pub struct TableCacheSlot {
    pub table_ptr: usize,
    pub shape_version: u64,
    pub slot_idx: usize,
}

impl Default for TableCacheSlot {
    fn default() -> Self {
        Self {
            table_ptr: 0,
            shape_version: u64::MAX,
            slot_idx: 0,
        }
    }
}

/// Type-specialized fast-path opcode populated lazily by the VM on first
/// execution. Falls back to `QuickOp::None` (deoptimization) if a guard fails.
#[derive(Clone, Copy, Debug, Default)]
pub enum QuickOp {
    #[default]
    None,
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
    AddIntK {
        dst: u8,
        a: u8,
        imm: i64,
    },
    AddFloatK {
        dst: u8,
        a: u8,
        imm: f64,
    },
    SubIntK {
        dst: u8,
        a: u8,
        imm: i64,
    },
    SubFloatK {
        dst: u8,
        a: u8,
        imm: f64,
    },
    MulIntK {
        dst: u8,
        a: u8,
        imm: i64,
    },
    MulFloatK {
        dst: u8,
        a: u8,
        imm: f64,
    },
    LtInt {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },
    LeInt {
        a: u8,
        b: u8,
        jump_if_false: i16,
    },
    LtIntK {
        a: u8,
        imm: i64,
        jump_if_false: i16,
    },
    LeIntK {
        a: u8,
        imm: i64,
        jump_if_false: i16,
    },
    EqIntK {
        a: u8,
        imm: i64,
        jump_if_false: i16,
    },
}

/// One cached `module.field` resolution for a `GetImport` site: the globals
/// epoch it was resolved under, the module table (whose lack of a metatable
/// is rechecked on every hit, so a later `setmetatable` can never go stale),
/// and the resolved field value. Uninitialized slots carry `u64::MAX`, which
/// no real epoch ever reaches.
#[derive(Clone)]
pub struct ImportSlot {
    pub epoch: u64,
    pub module: Option<Rc<RefCell<VmTable>>>,
    pub value: Value,
}

impl Default for ImportSlot {
    fn default() -> Self {
        Self {
            epoch: u64::MAX,
            module: None,
            value: Value::Nil,
        }
    }
}

impl PartialEq for ProtoCache {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Clone for ProtoCache {
    fn clone(&self) -> Self {
        ProtoCache::default()
    }
}

impl fmt::Debug for ProtoCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ProtoCache")
    }
}

impl Proto {
    pub fn new(name: Option<String>, num_params: u8, is_vararg: bool) -> Self {
        Self {
            name,
            num_params,
            max_registers: 0,
            is_vararg,
            constants: Vec::new(),
            instructions: Vec::new(),
            protos: Vec::new(),
            upvalues: Vec::new(),
            lines: Vec::new(),
            local_names: Vec::new(),
            source: None,
            upvalue_names: Vec::new(),
            cache: ProtoCache::default(),
        }
    }

    /// Whether this prototype (and so every prototype nested in it) has
    /// passed bytecode verification. The VM relies on a verified prototype
    /// never naming a register at or beyond `max_registers`.
    #[inline]
    pub fn is_verified(&self) -> bool {
        self.cache.verified.get()
    }

    /// Records that verification of this prototype tree succeeded.
    pub fn mark_verified(&self) {
        self.cache.verified.set(true);
        for child in &self.protos {
            child.mark_verified();
        }
    }

    /// The constant pool as runtime values.
    #[inline]
    pub fn values(&self) -> &[Value] {
        self.cache.values.get_or_init(|| {
            self.constants
                .iter()
                .map(|c| match c {
                    Constant::Nil => Value::Nil,
                    Constant::Bool(b) => Value::Bool(*b),
                    Constant::Int(i) => Value::from_bigint(i.clone()),
                    Constant::Float(f) => Value::Float(*f),
                    Constant::String(s) => Value::str(s),
                })
                .collect()
        })
    }

    /// Cached `module.field` value for a `GetImport` site, if the slot was
    /// resolved under the current globals epoch and the module table still
    /// has no metatable (so no `__index`/`__mode` game can have intervened).
    /// Returns a clone; the borrow is not held.
    pub fn import_cached(&self, site: usize, epoch: u64) -> Option<Value> {
        let slots = self.cache.imports.borrow();
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

    /// Records a fresh resolution. Only frozen, metatable-free modules with
    /// a present non-nil field are stored; everything else stays dynamic.
    pub fn import_store(
        &self,
        site: usize,
        epoch: u64,
        module: Rc<RefCell<VmTable>>,
        value: Value,
    ) {
        let mut slots = self.cache.imports.borrow_mut();
        if slots.len() <= site {
            slots.resize(site + 1, ImportSlot::default());
        }
        slots[site] = ImportSlot {
            epoch,
            module: Some(module),
            value,
        };
    }

    // Add a constant, deduplicating if identical constant already exists
    pub fn add_constant(&mut self, c: Constant) -> u16 {
        for (i, existing) in self.constants.iter().enumerate() {
            if *existing == c {
                return i as u16;
            }
        }
        let idx = self.constants.len() as u16;
        self.constants.push(c);
        idx
    }

    // Emit instruction with line number
    pub fn emit(&mut self, inst: Instruction, line: u32) -> usize {
        let idx = self.instructions.len();
        self.instructions.push(inst);
        self.lines.push(line);
        idx
    }

    /// Names the chunk this function and every function nested in it came
    /// from.
    pub fn set_source(&mut self, source: Rc<str>) {
        for child in &mut self.protos {
            Rc::make_mut(child).set_source(source.clone());
        }
        self.source = Some(source);
    }

    // Register a local variable in debug info
    pub fn push_local(&mut self, name: String, reg: u8, from_pc: u32) {
        self.local_names.push(LocalVarInfo {
            name,
            reg,
            from_pc,
            to_pc: u32::MAX,
        });
    }

    // Close a local variable's scope at the current pc
    pub fn close_local(&mut self, name: &str, to_pc: u32) {
        for info in self.local_names.iter_mut().rev() {
            if info.name == name && info.to_pc == u32::MAX {
                info.to_pc = to_pc;
                return;
            }
        }
    }

    #[inline]
    pub fn quick_op(&self, ip: usize) -> QuickOp {
        let ops = self.cache.quick_ops.borrow();
        ops.get(ip).copied().unwrap_or(QuickOp::None)
    }

    #[inline]
    pub fn set_quick_op(&self, ip: usize, op: QuickOp) {
        let mut ops = self.cache.quick_ops.borrow_mut();
        if ops.len() < self.instructions.len() {
            ops.resize(self.instructions.len(), QuickOp::None);
        }
        if let Some(slot) = ops.get_mut(ip) {
            *slot = op;
        }
    }

    #[inline]
    pub fn global_cache_get(&self, ip: usize, epoch: u64) -> Option<Value> {
        let cache = self.cache.globals.borrow();
        let slot = cache.get(ip)?;
        if slot.epoch == epoch {
            Some(slot.value.clone())
        } else {
            None
        }
    }

    #[inline]
    pub fn global_cache_set(&self, ip: usize, epoch: u64, value: Value) {
        let mut cache = self.cache.globals.borrow_mut();
        if cache.len() <= ip {
            cache.resize_with(
                self.instructions.len().max(ip + 1),
                GlobalCacheSlot::default,
            );
        }
        cache[ip] = GlobalCacheSlot { epoch, value };
    }

    #[inline]
    pub fn table_cache_get(&self, ip: usize) -> Option<TableCacheSlot> {
        let cache = self.cache.tables.borrow();
        let slot = cache.get(ip).copied()?;
        if slot.table_ptr != 0 {
            Some(slot)
        } else {
            None
        }
    }

    #[inline]
    pub fn table_cache_set(&self, ip: usize, slot: TableCacheSlot) {
        let mut cache = self.cache.tables.borrow_mut();
        if cache.len() <= ip {
            cache.resize_with(self.instructions.len().max(ip + 1), TableCacheSlot::default);
        }
        cache[ip] = slot;
    }
}
