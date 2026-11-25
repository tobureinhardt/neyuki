//! Function prototypes and their constants.

use crate::bytecode::exec::ExecProgram;
use crate::bytecode::instruction::Instruction;
use crate::vm::value::Value;
use num_bigint::BigInt;
use std::cell::{Cell, OnceCell};
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
    exec: OnceCell<ExecProgram>,
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

    /// Returns the materialized constant values if already initialized.
    #[inline(always)]
    pub fn values_ready(&self) -> Option<&[Value]> {
        self.cache.values.get().map(Vec::as_slice)
    }

    /// The constant pool as runtime values.
    #[inline]
    pub fn values(&self) -> &[Value] {
        self.cache.values.get_or_init(|| {
            self.constants
                .iter()
                .map(|c| match c {
                    Constant::Nil => Value::NIL,
                    Constant::Bool(b) => Value::bool(*b),
                    Constant::Int(i) => Value::from_bigint(i.clone()),
                    Constant::Float(f) => Value::float(*f),
                    Constant::String(s) => Value::str(s),
                })
                .collect()
        })
    }

    /// Pre-populates the materialized constant values across this prototype tree.
    pub fn warm_values(&self) {
        let _ = self.values();
        for child in &self.protos {
            child.warm_values();
        }
    }

    /// Clones this prototype tree with fresh, empty `GetImport` caches while
    /// preserving verification status and already-materialized constant values
    /// (which contain only immutable primitives and strings, never VM tables).
    ///
    /// Used when instantiating a cached bundled module into a VM so each VM
    /// owns its own import slots: dropping a VM releases all of its tables and
    /// concurrent VMs on the same thread never thrash each other's caches.
    pub fn clone_isolated(&self) -> Rc<Proto> {
        let cache = ProtoCache {
            values: self.cache.values.clone(),
            verified: Cell::new(self.cache.verified.get()),
            exec: OnceCell::new(),
        };
        Rc::new(Proto {
            name: self.name.clone(),
            num_params: self.num_params,
            max_registers: self.max_registers,
            is_vararg: self.is_vararg,
            constants: self.constants.clone(),
            instructions: self.instructions.clone(),
            protos: self.protos.iter().map(|p| p.clone_isolated()).collect(),
            upvalues: self.upvalues.clone(),
            lines: self.lines.clone(),
            local_names: self.local_names.clone(),
            source: self.source.clone(),
            upvalue_names: self.upvalue_names.clone(),
            cache,
        })
    }

    /// Returns the execution stream if already initialized.
    #[inline(always)]
    pub fn exec_ready(&self) -> Option<&ExecProgram> {
        self.cache.exec.get()
    }

    /// Pre-lowered execution stream and dense inline caches for this prototype.
    #[inline(always)]
    pub fn exec(&self) -> &ExecProgram {
        self.cache
            .exec
            .get_or_init(|| ExecProgram::compile(&self.instructions))
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
}
