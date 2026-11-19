//! Native FFI and extension API backing `@neyuki/ffi`.
//!
//! Provides:
//! 1. Typed C-ABI dynamic library loading and invocation (`__ffi_open`, `__ffi_call`, `__ffi_close`)
//!    via `libloading`.
//! 2. Safe C-memory helpers (`__ffi_alloc`, `__ffi_free`, `__ffi_read`, `__ffi_write`,
//!    `__ffi_read_cstring`, `__ffi_sizeof`).
//! 3. Versioned Neyuki Native Extension ABI (`__ffi_load_extension`, `__ffi_ext_call`)
//!    so external `cdylib` plugins can register functions without depending on
//!    internal Rust `Value` layout.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString, c_char, c_void};
use std::rc::Rc;

use libloading::Library;

use crate::vm::libs::args::{int_arg, opt_int_arg, str_arg, table_arg};
use crate::vm::machine::VM;
use crate::vm::value::{NativeDef, Value, VmTable};

pub(crate) const NATIVES: &[NativeDef] = &[
    NativeDef {
        name: "__ffi_open",
        func: builtin_open,
    },
    NativeDef {
        name: "__ffi_call",
        func: builtin_call,
    },
    NativeDef {
        name: "__ffi_close",
        func: builtin_close,
    },
    NativeDef {
        name: "__ffi_load_extension",
        func: builtin_load_extension,
    },
    NativeDef {
        name: "__ffi_ext_call",
        func: builtin_ext_call,
    },
    NativeDef {
        name: "__ffi_alloc",
        func: builtin_alloc,
    },
    NativeDef {
        name: "__ffi_free",
        func: builtin_free,
    },
    NativeDef {
        name: "__ffi_read",
        func: builtin_read,
    },
    NativeDef {
        name: "__ffi_write",
        func: builtin_write,
    },
    NativeDef {
        name: "__ffi_read_cstring",
        func: builtin_read_cstring,
    },
    NativeDef {
        name: "__ffi_read_unsafe",
        func: builtin_read_unsafe,
    },
    NativeDef {
        name: "__ffi_write_unsafe",
        func: builtin_write_unsafe,
    },
    NativeDef {
        name: "__ffi_read_cstring_unsafe",
        func: builtin_read_cstring_unsafe,
    },
    NativeDef {
        name: "__ffi_sizeof",
        func: builtin_sizeof,
    },
];

// ---------------------------------------------------------------------------
// C-ABI Type Descriptors
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfiType {
    Void,
    Bool,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    ISize,
    USize,
    F32,
    F64,
    Ptr,
    CStr,
}

impl FfiType {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "void" => Ok(Self::Void),
            "bool" => Ok(Self::Bool),
            "i8" => Ok(Self::I8),
            "u8" => Ok(Self::U8),
            "i16" => Ok(Self::I16),
            "u16" => Ok(Self::U16),
            "i32" | "int" => Ok(Self::I32),
            "u32" | "uint" => Ok(Self::U32),
            "i64" => Ok(Self::I64),
            "u64" => Ok(Self::U64),
            "isize" => Ok(Self::ISize),
            "usize" => Ok(Self::USize),
            "f32" | "float" => Ok(Self::F32),
            "f64" | "double" => Ok(Self::F64),
            "ptr" | "pointer" | "buffer" => Ok(Self::Ptr),
            "string" | "cstr" => Ok(Self::CStr),
            other => Err(format!("unknown FFI type '{}'", other)),
        }
    }

    pub fn byte_size(self) -> Result<usize, String> {
        match self {
            Self::Void => Err("void has no byte size".to_string()),
            Self::Bool | Self::I8 | Self::U8 => Ok(1),
            Self::I16 | Self::U16 => Ok(2),
            Self::I32 | Self::U32 | Self::F32 => Ok(4),
            Self::I64 | Self::U64 | Self::F64 => Ok(8),
            Self::ISize | Self::USize | Self::Ptr | Self::CStr => Ok(std::mem::size_of::<usize>()),
        }
    }

    fn abi_class(self) -> AbiClass {
        match self {
            Self::F32 => AbiClass::F32,
            Self::F64 => AbiClass::F64,
            _ => AbiClass::Int,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AbiClass {
    Int,
    F32,
    F64,
}

#[derive(Clone, Copy, Debug)]
enum AbiArg {
    Int(u64),
    F32(f32),
    F64(f64),
}

// ---------------------------------------------------------------------------
// Native Extension Plugin ABI (v1)
// ---------------------------------------------------------------------------

pub const NEYUKI_EXT_ABI_VERSION: u32 = 1;

pub const EXT_TAG_NIL: u8 = 0;
pub const EXT_TAG_BOOL: u8 = 1;
pub const EXT_TAG_INT: u8 = 2;
pub const EXT_TAG_FLOAT: u8 = 3;
pub const EXT_TAG_STRING: u8 = 4;
pub const EXT_TAG_ERROR: u8 = 5;

/// Stable C-ABI value exchanged between Neyuki and native extension plugins.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct NeyukiExtValue {
    pub tag: u8,
    pub i: i64,
    pub f: f64,
    pub ptr: *const u8,
    pub len: usize,
}

impl NeyukiExtValue {
    pub fn nil() -> Self {
        Self {
            tag: EXT_TAG_NIL,
            i: 0,
            f: 0.0,
            ptr: std::ptr::null(),
            len: 0,
        }
    }

    pub fn from_bool(b: bool) -> Self {
        Self {
            tag: EXT_TAG_BOOL,
            i: i64::from(b),
            f: 0.0,
            ptr: std::ptr::null(),
            len: 0,
        }
    }

    pub fn from_int(i: i64) -> Self {
        Self {
            tag: EXT_TAG_INT,
            i,
            f: 0.0,
            ptr: std::ptr::null(),
            len: 0,
        }
    }

    pub fn from_float(f: f64) -> Self {
        Self {
            tag: EXT_TAG_FLOAT,
            i: 0,
            f,
            ptr: std::ptr::null(),
            len: 0,
        }
    }
}

pub type NeyukiExtFn = unsafe extern "C" fn(
    argc: usize,
    argv: *const NeyukiExtValue,
    user_data: *mut c_void,
) -> NeyukiExtValue;

pub type NeyukiExtRegisterFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    name_ptr: *const u8,
    name_len: usize,
    func: NeyukiExtFn,
    user_data: *mut c_void,
) -> bool;

#[repr(C)]
pub struct NeyukiExtApi {
    pub abi_version: u32,
    pub register_fn: NeyukiExtRegisterFn,
}

pub type NeyukiExtInitFn = unsafe extern "C" fn(api: *const NeyukiExtApi, ctx: *mut c_void) -> i32;

// ---------------------------------------------------------------------------
// Registries for loaded libraries, bound symbols, and allocated buffers
// ---------------------------------------------------------------------------

struct BoundSymbol {
    ptr: *const c_void,
    args: Vec<FfiType>,
    ret: FfiType,
}

struct BoundExtFn {
    func: NeyukiExtFn,
    user_data: *mut c_void,
}

struct LoadedLibrary {
    _lib: Option<Library>,
    symbols: Vec<BoundSymbol>,
    ext_fns: Vec<BoundExtFn>,
}

struct AllocEntry {
    len: usize,
}

thread_local! {
    static LIBRARIES: RefCell<HashMap<u64, LoadedLibrary>> = RefCell::new(HashMap::new());
    static NEXT_LIB_ID: Cell<u64> = const { Cell::new(1) };
    static ALLOCS: RefCell<BTreeMap<usize, AllocEntry>> = const { RefCell::new(BTreeMap::new()) };
    static FFI_ALLOCATED_BYTES: Cell<usize> = const { Cell::new(0) };
}

fn next_lib_id() -> u64 {
    NEXT_LIB_ID.with(|id| {
        let v = id.get();
        id.set(v + 1);
        v
    })
}

fn open_library(path: &str) -> Result<Library, String> {
    if path.is_empty() {
        #[cfg(unix)]
        {
            let lib: libloading::os::unix::Library = libloading::os::unix::Library::this();
            return Ok(lib.into());
        }
        #[cfg(not(unix))]
        {
            return Err("empty library path is only supported on Unix hosts".to_string());
        }
    }
    // SAFETY: Caller has passed `--allow-ffi` (or runs unrestricted); loading a
    // shared library runs its platform initializers as requested by the user.
    unsafe { Library::new(path) }
        .map_err(|err| format!("failed to load library '{}': {}", path, err))
}

// ---------------------------------------------------------------------------
// FFI Open / Call / Close
// ---------------------------------------------------------------------------

fn parse_signature(tbl: &VmTable) -> Result<(Vec<FfiType>, FfiType), String> {
    let ret = match tbl.get_str("returns") {
        Value::Nil => FfiType::Void,
        Value::String(s) => FfiType::parse(&s)?,
        other => {
            return Err(format!(
                "signature 'returns' must be a string, got {}",
                other.type_name()
            ));
        }
    };
    let mut args = Vec::new();
    match tbl.get_str("args") {
        Value::Nil => {}
        Value::Table(arg_tbl) => {
            let arg_tbl = arg_tbl.borrow();
            for (idx, val) in arg_tbl.array.iter().enumerate() {
                let Value::String(s) = val else {
                    return Err(format!(
                        "signature args[{}] must be a type string, got {}",
                        idx + 1,
                        val.type_name()
                    ));
                };
                let ty = FfiType::parse(s)?;
                if ty == FfiType::Void {
                    return Err("void cannot be used as an argument type".to_string());
                }
                args.push(ty);
            }
        }
        other => {
            return Err(format!(
                "signature 'args' must be an array of type names, got {}",
                other.type_name()
            ));
        }
    }
    if args.len() > 6 {
        return Err(format!(
            "FFI supports at most 6 arguments (got {})",
            args.len()
        ));
    }
    Ok((args, ret))
}

fn builtin_open(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let path = match args.first() {
        None | Some(Value::Nil) => "",
        Some(Value::String(s)) => &**s,
        Some(other) => {
            return Err(format!(
                "path must be a string or nil, got {}",
                other.type_name()
            ));
        }
    };
    let perm_target = if path.is_empty() { "<self>" } else { path };
    vm.permissions.check_ffi(perm_target)?;

    let sigs_tbl = table_arg(args, 1, "signatures")?;
    let lib = open_library(path)?;

    let mut symbols = Vec::new();
    let mut sym_map = VmTable::new();

    {
        let sigs = sigs_tbl.borrow();
        for (name, sig_val) in sigs.fields.iter() {
            let Value::Table(sig_tbl) = sig_val else {
                return Err(format!("signature for '{}' must be a table", name.0.str));
            };
            let (arg_tys, ret_ty) = parse_signature(&sig_tbl.borrow())?;
            let c_name = CString::new(name.0.str.as_str())
                .map_err(|_| format!("symbol name '{}' contains NUL byte", name.0.str))?;
            // SAFETY: `lib` stays alive inside `LIBRARIES` for the lifetime of
            // every `BoundSymbol` until `__ffi_close` removes the entry.
            let raw_ptr: *const c_void = unsafe {
                lib.get::<*const c_void>(c_name.as_bytes_with_nul())
                    .map_err(|err| {
                        format!(
                            "symbol '{}' not found in '{}': {}",
                            name.0.str, perm_target, err
                        )
                    })?
                    .into_raw()
                    .as_raw_ptr()
            };
            let sym_idx = symbols.len();
            symbols.push(BoundSymbol {
                ptr: raw_ptr,
                args: arg_tys,
                ret: ret_ty,
            });
            sym_map.set_str(&name.0.str, Value::from_usize(sym_idx));
        }
    }

    let id = next_lib_id();
    LIBRARIES.with(|libs| {
        libs.borrow_mut().insert(
            id,
            LoadedLibrary {
                _lib: Some(lib),
                symbols,
                ext_fns: Vec::new(),
            },
        );
    });

    Ok(vec![
        Value::from_usize(id as usize),
        Value::Table(Rc::new(RefCell::new(sym_map))),
    ])
}

fn value_to_abi_arg(
    val: &Value,
    ty: FfiType,
    cstrings: &mut Vec<CString>,
) -> Result<AbiArg, String> {
    match ty {
        FfiType::Bool => Ok(AbiArg::Int(u64::from(val.is_truthy()))),
        FfiType::I8 => Ok(AbiArg::Int(val_to_i64(val, "i8")? as i8 as i64 as u64)),
        FfiType::U8 => Ok(AbiArg::Int(u64::from(val_to_i64(val, "u8")? as u8))),
        FfiType::I16 => Ok(AbiArg::Int(val_to_i64(val, "i16")? as i16 as i64 as u64)),
        FfiType::U16 => Ok(AbiArg::Int(u64::from(val_to_i64(val, "u16")? as u16))),
        FfiType::I32 => Ok(AbiArg::Int(val_to_i64(val, "i32")? as i32 as i64 as u64)),
        FfiType::U32 => Ok(AbiArg::Int(u64::from(val_to_i64(val, "u32")? as u32))),
        FfiType::I64 | FfiType::ISize => Ok(AbiArg::Int(val_to_i64(val, "i64")? as u64)),
        FfiType::U64 | FfiType::USize => Ok(AbiArg::Int(val_to_u64(val, "u64")?)),
        FfiType::F32 => Ok(AbiArg::F32(val_to_f64(val, "f32")? as f32)),
        FfiType::F64 => Ok(AbiArg::F64(val_to_f64(val, "f64")?)),
        FfiType::Ptr => match val {
            Value::Nil => Ok(AbiArg::Int(0)),
            Value::Int(i) => Ok(AbiArg::Int(*i as u64)),
            Value::Buffer(b) => {
                let mut buf = b.borrow_mut();
                Ok(AbiArg::Int(buf.data.as_mut_ptr() as usize as u64))
            }
            other => Err(format!(
                "expected pointer (int, buffer, or nil), got {}",
                other.type_name()
            )),
        },
        FfiType::CStr => match val {
            Value::Nil => Ok(AbiArg::Int(0)),
            Value::String(s) => {
                let cs = CString::new(&**s)
                    .map_err(|_| "string argument contains interior NUL byte".to_string())?;
                let ptr = cs.as_ptr() as usize as u64;
                cstrings.push(cs);
                Ok(AbiArg::Int(ptr))
            }
            other => Err(format!(
                "expected string or nil for cstr argument, got {}",
                other.type_name()
            )),
        },
        FfiType::Void => Err("void cannot be an argument".to_string()),
    }
}

fn val_to_i64(val: &Value, ty: &str) -> Result<i64, String> {
    match val {
        Value::Int(i) => Ok(*i),
        Value::Float(f) if f.is_finite() => Ok(*f as i64),
        Value::BigInt(b) => num_traits::ToPrimitive::to_i64(&**b)
            .ok_or_else(|| format!("integer out of range for {}", ty)),
        other => Err(format!(
            "expected number for {}, got {}",
            ty,
            other.type_name()
        )),
    }
}

fn val_to_u64(val: &Value, ty: &str) -> Result<u64, String> {
    match val {
        Value::Int(i) if *i >= 0 => Ok(*i as u64),
        Value::Int(i) => Ok(*i as u64),
        Value::Float(f) if f.is_finite() && *f >= 0.0 => Ok(*f as u64),
        Value::BigInt(b) => num_traits::ToPrimitive::to_u64(&**b)
            .ok_or_else(|| format!("integer out of range for {}", ty)),
        other => Err(format!(
            "expected number for {}, got {}",
            ty,
            other.type_name()
        )),
    }
}

fn val_to_f64(val: &Value, ty: &str) -> Result<f64, String> {
    match val {
        Value::Int(i) => Ok(*i as f64),
        Value::Float(f) => Ok(*f),
        Value::BigInt(b) => num_traits::ToPrimitive::to_f64(&**b)
            .ok_or_else(|| format!("number out of range for {}", ty)),
        other => Err(format!(
            "expected number for {}, got {}",
            ty,
            other.type_name()
        )),
    }
}

macro_rules! invoke_c_ret {
    ($fn_ptr:expr, $ret_ty:expr, ($($aty:ty),*), ($($aval:expr),*)) => {{
        match $ret_ty {
            FfiType::Void => {
                let f: unsafe extern "C" fn($($aty),*) = std::mem::transmute($fn_ptr);
                f($($aval),*);
                Ok(Value::Nil)
            }
            FfiType::Bool => {
                let f: unsafe extern "C" fn($($aty),*) -> bool = std::mem::transmute($fn_ptr);
                Ok(Value::Bool(f($($aval),*)))
            }
            FfiType::I8 => {
                let f: unsafe extern "C" fn($($aty),*) -> i8 = std::mem::transmute($fn_ptr);
                Ok(Value::Int(i64::from(f($($aval),*))))
            }
            FfiType::U8 => {
                let f: unsafe extern "C" fn($($aty),*) -> u8 = std::mem::transmute($fn_ptr);
                Ok(Value::Int(i64::from(f($($aval),*))))
            }
            FfiType::I16 => {
                let f: unsafe extern "C" fn($($aty),*) -> i16 = std::mem::transmute($fn_ptr);
                Ok(Value::Int(i64::from(f($($aval),*))))
            }
            FfiType::U16 => {
                let f: unsafe extern "C" fn($($aty),*) -> u16 = std::mem::transmute($fn_ptr);
                Ok(Value::Int(i64::from(f($($aval),*))))
            }
            FfiType::I32 => {
                let f: unsafe extern "C" fn($($aty),*) -> i32 = std::mem::transmute($fn_ptr);
                Ok(Value::Int(i64::from(f($($aval),*))))
            }
            FfiType::U32 => {
                let f: unsafe extern "C" fn($($aty),*) -> u32 = std::mem::transmute($fn_ptr);
                Ok(Value::Int(i64::from(f($($aval),*))))
            }
            FfiType::I64 | FfiType::ISize => {
                let f: unsafe extern "C" fn($($aty),*) -> i64 = std::mem::transmute($fn_ptr);
                Ok(Value::Int(f($($aval),*)))
            }
            FfiType::U64 | FfiType::USize => {
                let f: unsafe extern "C" fn($($aty),*) -> u64 = std::mem::transmute($fn_ptr);
                let v = f($($aval),*);
                if v <= i64::MAX as u64 {
                    Ok(Value::Int(v as i64))
                } else {
                    Ok(Value::from_bigint(num_bigint::BigInt::from(v)))
                }
            }
            FfiType::F32 => {
                let f: unsafe extern "C" fn($($aty),*) -> f32 = std::mem::transmute($fn_ptr);
                Ok(Value::Float(f64::from(f($($aval),*))))
            }
            FfiType::F64 => {
                let f: unsafe extern "C" fn($($aty),*) -> f64 = std::mem::transmute($fn_ptr);
                Ok(Value::Float(f($($aval),*)))
            }
            FfiType::Ptr => {
                let f: unsafe extern "C" fn($($aty),*) -> usize = std::mem::transmute($fn_ptr);
                let addr = f($($aval),*);
                if addr == 0 {
                    Ok(Value::Nil)
                } else {
                    Ok(Value::Int(addr as i64))
                }
            }
            FfiType::CStr => {
                let f: unsafe extern "C" fn($($aty),*) -> *const c_char =
                    std::mem::transmute($fn_ptr);
                let p = f($($aval),*);
                if p.is_null() {
                    Ok(Value::Nil)
                } else {
                    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
                    Ok(Value::string(s))
                }
            }
        }
    }};
}

unsafe fn dispatch_c_call(
    ptr: *const c_void,
    args: &[AbiArg],
    ret_ty: FfiType,
) -> Result<Value, String> {
    unsafe {
        match args {
            [] => invoke_c_ret!(ptr, ret_ty, (), ()),
            [AbiArg::Int(a0)] => invoke_c_ret!(ptr, ret_ty, (u64), (*a0)),
            [AbiArg::F32(a0)] => invoke_c_ret!(ptr, ret_ty, (f32), (*a0)),
            [AbiArg::F64(a0)] => invoke_c_ret!(ptr, ret_ty, (f64), (*a0)),
            [AbiArg::Int(a0), AbiArg::Int(a1)] => {
                invoke_c_ret!(ptr, ret_ty, (u64, u64), (*a0, *a1))
            }
            [AbiArg::Int(a0), AbiArg::F64(a1)] => {
                invoke_c_ret!(ptr, ret_ty, (u64, f64), (*a0, *a1))
            }
            [AbiArg::F64(a0), AbiArg::Int(a1)] => {
                invoke_c_ret!(ptr, ret_ty, (f64, u64), (*a0, *a1))
            }
            [AbiArg::F64(a0), AbiArg::F64(a1)] => {
                invoke_c_ret!(ptr, ret_ty, (f64, f64), (*a0, *a1))
            }
            [AbiArg::F32(a0), AbiArg::F32(a1)] => {
                invoke_c_ret!(ptr, ret_ty, (f32, f32), (*a0, *a1))
            }
            [AbiArg::Int(a0), AbiArg::Int(a1), AbiArg::Int(a2)] => {
                invoke_c_ret!(ptr, ret_ty, (u64, u64, u64), (*a0, *a1, *a2))
            }
            [AbiArg::Int(a0), AbiArg::Int(a1), AbiArg::F64(a2)] => {
                invoke_c_ret!(ptr, ret_ty, (u64, u64, f64), (*a0, *a1, *a2))
            }
            [AbiArg::Int(a0), AbiArg::F64(a1), AbiArg::Int(a2)] => {
                invoke_c_ret!(ptr, ret_ty, (u64, f64, u64), (*a0, *a1, *a2))
            }
            [AbiArg::F64(a0), AbiArg::Int(a1), AbiArg::Int(a2)] => {
                invoke_c_ret!(ptr, ret_ty, (f64, u64, u64), (*a0, *a1, *a2))
            }
            [AbiArg::F64(a0), AbiArg::F64(a1), AbiArg::F64(a2)] => {
                invoke_c_ret!(ptr, ret_ty, (f64, f64, f64), (*a0, *a1, *a2))
            }
            [AbiArg::F32(a0), AbiArg::F32(a1), AbiArg::F32(a2)] => {
                invoke_c_ret!(ptr, ret_ty, (f32, f32, f32), (*a0, *a1, *a2))
            }
            [
                AbiArg::Int(a0),
                AbiArg::Int(a1),
                AbiArg::Int(a2),
                AbiArg::Int(a3),
            ] => {
                invoke_c_ret!(ptr, ret_ty, (u64, u64, u64, u64), (*a0, *a1, *a2, *a3))
            }
            [
                AbiArg::F64(a0),
                AbiArg::F64(a1),
                AbiArg::F64(a2),
                AbiArg::F64(a3),
            ] => {
                invoke_c_ret!(ptr, ret_ty, (f64, f64, f64, f64), (*a0, *a1, *a2, *a3))
            }
            [
                AbiArg::Int(a0),
                AbiArg::Int(a1),
                AbiArg::Int(a2),
                AbiArg::Int(a3),
                AbiArg::Int(a4),
            ] => {
                invoke_c_ret!(
                    ptr,
                    ret_ty,
                    (u64, u64, u64, u64, u64),
                    (*a0, *a1, *a2, *a3, *a4)
                )
            }
            [
                AbiArg::Int(a0),
                AbiArg::Int(a1),
                AbiArg::Int(a2),
                AbiArg::Int(a3),
                AbiArg::Int(a4),
                AbiArg::Int(a5),
            ] => {
                invoke_c_ret!(
                    ptr,
                    ret_ty,
                    (u64, u64, u64, u64, u64, u64),
                    (*a0, *a1, *a2, *a3, *a4, *a5)
                )
            }
            _ => Err(format!(
                "unsupported FFI mixed argument combination for {} arguments",
                args.len()
            )),
        }
    }
}

fn builtin_call(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let lib_id = int_arg(args, 0, "library")? as u64;
    let sym_idx = int_arg(args, 1, "symbol")? as usize;
    let call_args = &args[2..];

    let (ptr, arg_tys, ret_ty) = LIBRARIES.with(|libs| {
        let libs = libs.borrow();
        let lib = libs
            .get(&lib_id)
            .ok_or_else(|| "FFI library is closed".to_string())?;
        let sym = lib
            .symbols
            .get(sym_idx)
            .ok_or_else(|| "invalid FFI symbol".to_string())?;
        Ok::<_, String>((sym.ptr, sym.args.clone(), sym.ret))
    })?;

    if call_args.len() != arg_tys.len() {
        return Err(format!(
            "FFI call expects {} argument(s), got {}",
            arg_tys.len(),
            call_args.len()
        ));
    }

    let mut cstrings = Vec::new();
    let mut abi_args = Vec::with_capacity(arg_tys.len());
    for (val, ty) in call_args.iter().zip(arg_tys.iter()) {
        let _ = ty.abi_class();
        abi_args.push(value_to_abi_arg(val, *ty, &mut cstrings)?);
    }

    // SAFETY: `ptr` is valid while `lib_id` is open in `LIBRARIES`, `cstrings`
    // outlives the call, and argument/return types match the declared C-ABI signature.
    let result = unsafe { dispatch_c_call(ptr, &abi_args, ret_ty)? };
    Ok(vec![result])
}

fn builtin_close(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let lib_id = int_arg(args, 0, "library")? as u64;
    let removed = LIBRARIES.with(|libs| libs.borrow_mut().remove(&lib_id).is_some());
    Ok(vec![Value::Bool(removed)])
}

// ---------------------------------------------------------------------------
// Native Extension Plugin Loader
// ---------------------------------------------------------------------------

struct ExtRegisterContext {
    exports: Vec<(String, NeyukiExtFn, *mut c_void)>,
}

unsafe extern "C" fn ext_register_callback(
    ctx: *mut c_void,
    name_ptr: *const u8,
    name_len: usize,
    func: NeyukiExtFn,
    user_data: *mut c_void,
) -> bool {
    if ctx.is_null() || name_ptr.is_null() {
        return false;
    }
    let slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len) };
    let Ok(name) = std::str::from_utf8(slice) else {
        return false;
    };
    let ctx_ref = unsafe { &mut *(ctx as *mut ExtRegisterContext) };
    ctx_ref.exports.push((name.to_string(), func, user_data));
    true
}

/// Registers an in-process or loaded native extension via an already-resolved
/// `NeyukiExtInitFn`. Used by both `builtin_load_extension` and unit tests.
pub fn init_extension_with_fn(
    init_fn: NeyukiExtInitFn,
    lib: Option<Library>,
) -> Result<(u64, VmTable), String> {
    let api = NeyukiExtApi {
        abi_version: NEYUKI_EXT_ABI_VERSION,
        register_fn: ext_register_callback,
    };
    let mut ctx = ExtRegisterContext {
        exports: Vec::new(),
    };
    let status = unsafe { init_fn(&api, (&mut ctx as *mut ExtRegisterContext).cast()) };
    if status != 0 {
        return Err(format!(
            "native extension initialization failed with status {}",
            status
        ));
    }

    let mut ext_fns = Vec::with_capacity(ctx.exports.len());
    let mut fn_map = VmTable::new();
    for (name, func, user_data) in ctx.exports {
        let idx = ext_fns.len();
        ext_fns.push(BoundExtFn { func, user_data });
        fn_map.set_str(&name, Value::from_usize(idx));
    }

    let id = next_lib_id();
    LIBRARIES.with(|libs| {
        libs.borrow_mut().insert(
            id,
            LoadedLibrary {
                _lib: lib,
                symbols: Vec::new(),
                ext_fns,
            },
        );
    });
    Ok((id, fn_map))
}

fn builtin_load_extension(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let path = str_arg(args, 0, "path")?;
    vm.permissions.check_ffi(path)?;
    let lib = open_library(path)?;
    let init_fn: NeyukiExtInitFn = unsafe {
        *lib.get::<NeyukiExtInitFn>(b"neyuki_extension_init\0")
            .map_err(|err| format!("missing 'neyuki_extension_init' in '{}': {}", path, err))?
    };
    let (id, fn_map) = init_extension_with_fn(init_fn, Some(lib))?;
    Ok(vec![
        Value::from_usize(id as usize),
        Value::Table(Rc::new(RefCell::new(fn_map))),
    ])
}

fn value_to_ext(val: &Value, owned_strings: &mut Vec<String>) -> NeyukiExtValue {
    match val {
        Value::Nil => NeyukiExtValue::nil(),
        Value::Bool(b) => NeyukiExtValue::from_bool(*b),
        Value::Int(i) => NeyukiExtValue::from_int(*i),
        Value::Float(f) => NeyukiExtValue::from_float(*f),
        Value::String(s) => {
            let owned = s.to_string();
            let ptr = owned.as_ptr();
            let len = owned.len();
            owned_strings.push(owned);
            NeyukiExtValue {
                tag: EXT_TAG_STRING,
                i: 0,
                f: 0.0,
                ptr,
                len,
            }
        }
        other => {
            let s = other.to_string();
            let ptr = s.as_ptr();
            let len = s.len();
            owned_strings.push(s);
            NeyukiExtValue {
                tag: EXT_TAG_STRING,
                i: 0,
                f: 0.0,
                ptr,
                len,
            }
        }
    }
}

unsafe fn ext_to_value(ext: NeyukiExtValue) -> Result<Value, String> {
    match ext.tag {
        EXT_TAG_NIL => Ok(Value::Nil),
        EXT_TAG_BOOL => Ok(Value::Bool(ext.i != 0)),
        EXT_TAG_INT => Ok(Value::Int(ext.i)),
        EXT_TAG_FLOAT => Ok(Value::Float(ext.f)),
        EXT_TAG_STRING => {
            if ext.ptr.is_null() {
                return Ok(Value::string(String::new()));
            }
            let bytes = unsafe { std::slice::from_raw_parts(ext.ptr, ext.len) };
            Ok(Value::string(String::from_utf8_lossy(bytes).into_owned()))
        }
        EXT_TAG_ERROR => {
            let msg = if ext.ptr.is_null() {
                "extension error".to_string()
            } else {
                let bytes = unsafe { std::slice::from_raw_parts(ext.ptr, ext.len) };
                String::from_utf8_lossy(bytes).into_owned()
            };
            Err(msg)
        }
        other => Err(format!("invalid extension return tag {}", other)),
    }
}

fn builtin_ext_call(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let lib_id = int_arg(args, 0, "extension")? as u64;
    let fn_idx = int_arg(args, 1, "function")? as usize;
    let call_args = &args[2..];

    let (func, user_data) = LIBRARIES.with(|libs| {
        let libs = libs.borrow();
        let lib = libs
            .get(&lib_id)
            .ok_or_else(|| "native extension is closed".to_string())?;
        let bound = lib
            .ext_fns
            .get(fn_idx)
            .ok_or_else(|| "invalid extension function".to_string())?;
        Ok::<_, String>((bound.func, bound.user_data))
    })?;

    let mut owned_strings = Vec::new();
    let mut ext_args = Vec::with_capacity(call_args.len());
    for v in call_args {
        ext_args.push(value_to_ext(v, &mut owned_strings));
    }
    let ret = unsafe { func(ext_args.len(), ext_args.as_ptr(), user_data) };
    let val = unsafe { ext_to_value(ret)? };
    Ok(vec![val])
}

// ---------------------------------------------------------------------------
// Bounds-Checked C-Memory Helpers (+ explicit Unsafe raw-pointer variants)
// ---------------------------------------------------------------------------

fn ptr_arg_usize(args: &[Value], index: usize, name: &str) -> Result<usize, String> {
    let raw = int_arg(args, index, name)?;
    usize::try_from(raw).map_err(|_| format!("invalid {} pointer address {}", name, raw))
}

fn builtin_alloc(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    vm.permissions.check_ffi_memory()?;
    let size = int_arg(args, 0, "size")?;
    if size <= 0 || size > 64 * 1024 * 1024 {
        return Err("ffi.alloc size must be between 1 and 67108864 bytes".to_string());
    }
    let len = size as usize;
    if let Some(limit) = vm.limits.heap_limit_bytes {
        let ffi_used = FFI_ALLOCATED_BYTES.with(|b| b.get());
        if vm
            .gc
            .bytes_allocated
            .saturating_add(ffi_used)
            .saturating_add(len)
            > limit
        {
            return Err(format!(
                "memory limit exceeded: FFI allocation of {} bytes exceeds limit of {} bytes",
                len, limit
            ));
        }
    }
    let mut vec = vec![0u8; len].into_boxed_slice();
    let addr = vec.as_mut_ptr() as usize;
    let _ = Box::into_raw(vec);
    ALLOCS.with(|allocs| {
        allocs.borrow_mut().insert(addr, AllocEntry { len });
    });
    FFI_ALLOCATED_BYTES.with(|b| b.set(b.get().saturating_add(len)));
    Ok(vec![Value::Int(addr as i64)])
}

fn builtin_free(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    vm.permissions.check_ffi_memory()?;
    let addr = ptr_arg_usize(args, 0, "ptr")?;
    if addr == 0 {
        return Ok(vec![Value::Nil]);
    }
    let entry = ALLOCS
        .with(|allocs| allocs.borrow_mut().remove(&addr))
        .ok_or_else(|| format!("invalid or already freed pointer 0x{:x}", addr))?;
    FFI_ALLOCATED_BYTES.with(|b| b.set(b.get().saturating_sub(entry.len)));
    // SAFETY: `addr` was produced by `Box::into_raw` with length `entry.len`.
    unsafe {
        let slice = std::ptr::slice_from_raw_parts_mut(addr as *mut u8, entry.len);
        drop(Box::from_raw(slice));
    }
    Ok(vec![Value::Nil])
}

/// Checks that `[addr + offset, addr + offset + size)` lies completely inside a
/// live `ffi.alloc` allocation. Rejects null pointers, arbitrary integers,
/// freed pointers, interior out-of-bounds offsets, and foreign C pointers.
fn check_ptr_range(addr: usize, offset: usize, size: usize) -> Result<*mut u8, String> {
    if addr == 0 {
        return Err("null pointer dereference".to_string());
    }
    let target = addr
        .checked_add(offset)
        .ok_or_else(|| "pointer offset overflow".to_string())?;
    let end = target
        .checked_add(size)
        .ok_or_else(|| "pointer size overflow".to_string())?;
    ALLOCS.with(|allocs| {
        let allocs = allocs.borrow();
        let Some((&base, entry)) = allocs.range(..=addr).next_back() else {
            return Err(format!(
                "FFI memory access to unowned, freed, or foreign pointer 0x{:x}",
                addr
            ));
        };
        let alloc_end = base.saturating_add(entry.len);
        if addr >= alloc_end {
            return Err(format!(
                "FFI memory access to unowned, freed, or foreign pointer 0x{:x}",
                addr
            ));
        }
        if end > alloc_end {
            return Err("FFI memory access out of bounds".to_string());
        }
        Ok(target as *mut u8)
    })
}

fn check_unchecked_ptr(addr: usize, offset: usize) -> Result<*mut u8, String> {
    if addr == 0 {
        return Err("null pointer dereference".to_string());
    }
    let target = addr
        .checked_add(offset)
        .ok_or_else(|| "pointer offset overflow".to_string())?;
    Ok(target as *mut u8)
}

unsafe fn read_typed_ptr(ptr: *const u8, ty: FfiType, checked_cstr: bool) -> Result<Value, String> {
    unsafe {
        let val = match ty {
            FfiType::Bool => Value::Bool(ptr.read_unaligned() != 0),
            FfiType::I8 => Value::Int(i64::from(ptr.cast::<i8>().read_unaligned())),
            FfiType::U8 => Value::Int(i64::from(ptr.read_unaligned())),
            FfiType::I16 => Value::Int(i64::from(ptr.cast::<i16>().read_unaligned())),
            FfiType::U16 => Value::Int(i64::from(ptr.cast::<u16>().read_unaligned())),
            FfiType::I32 => Value::Int(i64::from(ptr.cast::<i32>().read_unaligned())),
            FfiType::U32 => Value::Int(i64::from(ptr.cast::<u32>().read_unaligned())),
            FfiType::I64 | FfiType::ISize => Value::Int(ptr.cast::<i64>().read_unaligned()),
            FfiType::U64 | FfiType::USize => {
                let v = ptr.cast::<u64>().read_unaligned();
                if v <= i64::MAX as u64 {
                    Value::Int(v as i64)
                } else {
                    Value::from_bigint(num_bigint::BigInt::from(v))
                }
            }
            FfiType::F32 => Value::Float(f64::from(ptr.cast::<f32>().read_unaligned())),
            FfiType::F64 => Value::Float(ptr.cast::<f64>().read_unaligned()),
            FfiType::Ptr => {
                let p = ptr.cast::<usize>().read_unaligned();
                if p == 0 {
                    Value::Nil
                } else {
                    Value::Int(p as i64)
                }
            }
            FfiType::CStr => {
                let cptr = ptr.cast::<*const c_char>().read_unaligned();
                if cptr.is_null() {
                    Value::Nil
                } else if checked_cstr {
                    return read_cstring_checked(cptr as usize, 65536).map(Value::string);
                } else {
                    Value::string(CStr::from_ptr(cptr).to_string_lossy().into_owned())
                }
            }
            FfiType::Void => unreachable!(),
        };
        Ok(val)
    }
}

fn read_impl(vm: &mut VM, args: &[Value], checked: bool) -> Result<Vec<Value>, String> {
    vm.permissions.check_ffi_memory()?;
    let addr = ptr_arg_usize(args, 0, "ptr")?;
    let ty_str = str_arg(args, 1, "type")?;
    let ty = FfiType::parse(ty_str)?;
    let offset = opt_int_arg(args, 2, "offset", 0)?;
    if offset < 0 {
        return Err("offset must be non-negative".to_string());
    }
    let size = ty.byte_size()?;
    let ptr = if checked {
        check_ptr_range(addr, offset as usize, size)?
    } else {
        check_unchecked_ptr(addr, offset as usize)?
    };
    // SAFETY: When `checked` is true, `[ptr, ptr + size)` lies within a live
    // allocation in `ALLOCS`; `read_unsafe` requires explicit caller opt-in.
    let val = unsafe { read_typed_ptr(ptr, ty, checked)? };
    Ok(vec![val])
}

fn builtin_read(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    read_impl(vm, args, true)
}

fn builtin_read_unsafe(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    read_impl(vm, args, false)
}

fn write_impl(vm: &mut VM, args: &[Value], checked: bool) -> Result<Vec<Value>, String> {
    vm.permissions.check_ffi_memory()?;
    let addr = ptr_arg_usize(args, 0, "ptr")?;
    let ty_str = str_arg(args, 1, "type")?;
    let ty = FfiType::parse(ty_str)?;
    let val = args
        .get(2)
        .ok_or_else(|| "value must be provided".to_string())?;
    let offset = opt_int_arg(args, 3, "offset", 0)?;
    if offset < 0 {
        return Err("offset must be non-negative".to_string());
    }
    let size = ty.byte_size()?;
    let ptr = if checked {
        check_ptr_range(addr, offset as usize, size)?
    } else {
        check_unchecked_ptr(addr, offset as usize)?
    };
    // SAFETY: When `checked` is true, `[ptr, ptr + size)` lies within a live
    // allocation in `ALLOCS`; `write_unsafe` requires explicit caller opt-in.
    unsafe {
        match ty {
            FfiType::Bool => ptr.write_unaligned(u8::from(val.is_truthy())),
            FfiType::I8 => ptr
                .cast::<i8>()
                .write_unaligned(val_to_i64(val, "i8")? as i8),
            FfiType::U8 => ptr.write_unaligned(val_to_i64(val, "u8")? as u8),
            FfiType::I16 => ptr
                .cast::<i16>()
                .write_unaligned(val_to_i64(val, "i16")? as i16),
            FfiType::U16 => ptr
                .cast::<u16>()
                .write_unaligned(val_to_i64(val, "u16")? as u16),
            FfiType::I32 => ptr
                .cast::<i32>()
                .write_unaligned(val_to_i64(val, "i32")? as i32),
            FfiType::U32 => ptr
                .cast::<u32>()
                .write_unaligned(val_to_i64(val, "u32")? as u32),
            FfiType::I64 | FfiType::ISize => {
                ptr.cast::<i64>().write_unaligned(val_to_i64(val, "i64")?)
            }
            FfiType::U64 | FfiType::USize => {
                ptr.cast::<u64>().write_unaligned(val_to_u64(val, "u64")?)
            }
            FfiType::F32 => ptr
                .cast::<f32>()
                .write_unaligned(val_to_f64(val, "f32")? as f32),
            FfiType::F64 => ptr.cast::<f64>().write_unaligned(val_to_f64(val, "f64")?),
            FfiType::Ptr => {
                let p = match val {
                    Value::Nil => 0usize,
                    Value::Int(i) => {
                        usize::try_from(*i).map_err(|_| format!("invalid pointer address {}", i))?
                    }
                    other => {
                        return Err(format!(
                            "expected pointer or nil, got {}",
                            other.type_name()
                        ));
                    }
                };
                ptr.cast::<usize>().write_unaligned(p);
            }
            FfiType::CStr | FfiType::Void => {
                return Err(format!("cannot write '{}' directly to raw memory", ty_str));
            }
        }
    }
    Ok(vec![Value::Nil])
}

fn builtin_write(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    write_impl(vm, args, true)
}

fn builtin_write_unsafe(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    write_impl(vm, args, false)
}

fn read_cstring_checked(addr: usize, max_len: usize) -> Result<String, String> {
    let remaining = ALLOCS.with(|allocs| {
        let allocs = allocs.borrow();
        let Some((&base, entry)) = allocs.range(..=addr).next_back() else {
            return Err(format!(
                "FFI readCString on unowned, freed, or foreign pointer 0x{:x}",
                addr
            ));
        };
        let alloc_end = base.saturating_add(entry.len);
        if addr >= alloc_end {
            return Err(format!(
                "FFI readCString on unowned, freed, or foreign pointer 0x{:x}",
                addr
            ));
        }
        Ok(alloc_end - addr)
    })?;

    let limit = max_len.min(remaining);
    let ptr = addr as *const u8;
    let mut bytes = Vec::new();
    let mut found_nul = false;
    for i in 0..limit {
        // SAFETY: `i < remaining <= alloc_end - addr` inside a live `ALLOCS` entry.
        let b = unsafe { *ptr.add(i) };
        if b == 0 {
            found_nul = true;
            break;
        }
        bytes.push(b);
    }
    if !found_nul && limit == remaining && remaining < max_len {
        return Err("FFI C string read out of bounds (missing NUL terminator)".to_string());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_cstring_impl(vm: &mut VM, args: &[Value], checked: bool) -> Result<Vec<Value>, String> {
    vm.permissions.check_ffi_memory()?;
    let addr = match args.first() {
        None | Some(Value::Nil) => return Ok(vec![Value::Nil]),
        Some(Value::Int(0)) => return Ok(vec![Value::Nil]),
        Some(Value::Int(i)) => {
            usize::try_from(*i).map_err(|_| format!("invalid ptr address {}", i))?
        }
        Some(other) => {
            return Err(format!(
                "ptr must be an integer or nil, got {}",
                other.type_name()
            ));
        }
    };
    let max_len = opt_int_arg(args, 1, "maxLen", 65536)?.max(0) as usize;
    if checked {
        Ok(vec![Value::string(read_cstring_checked(addr, max_len)?)])
    } else {
        let ptr = addr as *const u8;
        let mut bytes = Vec::new();
        for i in 0..max_len {
            let b = unsafe { *ptr.add(i) };
            if b == 0 {
                break;
            }
            bytes.push(b);
        }
        Ok(vec![Value::string(
            String::from_utf8_lossy(&bytes).into_owned(),
        )])
    }
}

fn builtin_read_cstring(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    read_cstring_impl(vm, args, true)
}

fn builtin_read_cstring_unsafe(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    read_cstring_impl(vm, args, false)
}

fn builtin_sizeof(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let ty_str = str_arg(args, 0, "type")?;
    let size = FfiType::parse(ty_str)?.byte_size()?;
    Ok(vec![Value::Int(size as i64)])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use crate::vm::libs::args::array;

    unsafe extern "C" fn test_ext_add(
        argc: usize,
        argv: *const NeyukiExtValue,
        _user_data: *mut c_void,
    ) -> NeyukiExtValue {
        let args = unsafe { std::slice::from_raw_parts(argv, argc) };
        if args.len() != 2 || args[0].tag != EXT_TAG_INT || args[1].tag != EXT_TAG_INT {
            let msg = b"expected two integers";
            return NeyukiExtValue {
                tag: EXT_TAG_ERROR,
                i: 0,
                f: 0.0,
                ptr: msg.as_ptr(),
                len: msg.len(),
            };
        }
        NeyukiExtValue::from_int(args[0].i + args[1].i)
    }

    unsafe extern "C" fn test_ext_init(api: *const NeyukiExtApi, ctx: *mut c_void) -> i32 {
        let api = unsafe { &*api };
        if api.abi_version != NEYUKI_EXT_ABI_VERSION {
            return -1;
        }
        let name = b"add_ints";
        let ok = unsafe {
            (api.register_fn)(
                ctx,
                name.as_ptr(),
                name.len(),
                test_ext_add,
                std::ptr::null_mut(),
            )
        };
        if ok { 0 } else { -2 }
    }

    #[test]
    fn test_extension_api_roundtrip() {
        let (lib_id, fn_map) = init_extension_with_fn(test_ext_init, None).unwrap();
        let Value::Int(fn_idx) = fn_map.get_str("add_ints") else {
            panic!("missing add_ints in extension table");
        };
        let mut vm = VM::new();
        let res = builtin_ext_call(
            &mut vm,
            &[
                Value::Int(lib_id as i64),
                Value::Int(fn_idx),
                Value::Int(19),
                Value::Int(23),
            ],
        )
        .unwrap();
        assert_eq!(res[0].to_string(), "42");

        let err = builtin_ext_call(
            &mut vm,
            &[Value::Int(lib_id as i64), Value::Int(fn_idx), Value::Int(1)],
        )
        .unwrap_err();
        assert_eq!(err, "expected two integers");

        assert_eq!(
            builtin_close(&mut vm, &[Value::Int(lib_id as i64)]).unwrap()[0],
            Value::Bool(true)
        );
    }

    #[test]
    fn test_ffi_alloc_read_write_bounds_and_double_free() {
        let mut vm = VM::new();
        let ptr = builtin_alloc(&mut vm, &[Value::Int(16)]).unwrap()[0].clone();
        builtin_write(
            &mut vm,
            &[
                ptr.clone(),
                Value::str("i32"),
                Value::Int(-12345),
                Value::Int(4),
            ],
        )
        .unwrap();
        let val = builtin_read(&mut vm, &[ptr.clone(), Value::str("i32"), Value::Int(4)]).unwrap();
        assert_eq!(val[0].to_string(), "-12345");

        // Out-of-bounds read rejected cleanly
        let oob = builtin_read(&mut vm, &[ptr.clone(), Value::str("i64"), Value::Int(12)]);
        assert!(oob.unwrap_err().contains("out of bounds"));

        // Free and double-free check
        builtin_free(&mut vm, std::slice::from_ref(&ptr)).unwrap();
        let df = builtin_free(&mut vm, &[ptr]);
        assert!(df.unwrap_err().contains("already freed"));
    }

    #[cfg(unix)]
    #[test]
    fn test_ffi_open_and_call_libc_strlen_and_abs() {
        let mut vm = VM::new();
        let mut strlen_sig = VmTable::new();
        strlen_sig.set_str("args", array(vec![Value::str("string")]));
        strlen_sig.set_str("returns", Value::str("usize"));

        let mut abs_sig = VmTable::new();
        abs_sig.set_str("args", array(vec![Value::str("i32")]));
        abs_sig.set_str("returns", Value::str("i32"));

        let mut sigs = VmTable::new();
        sigs.set_str("strlen", Value::Table(Rc::new(RefCell::new(strlen_sig))));
        sigs.set_str("abs", Value::Table(Rc::new(RefCell::new(abs_sig))));

        let opened = builtin_open(
            &mut vm,
            &[Value::Nil, Value::Table(Rc::new(RefCell::new(sigs)))],
        )
        .unwrap();
        let lib_id = opened[0].clone();
        let Value::Table(sym_map) = &opened[1] else {
            panic!("expected symbol map");
        };
        let strlen_idx = sym_map.borrow().get_str("strlen");
        let abs_idx = sym_map.borrow().get_str("abs");

        let len_res = builtin_call(
            &mut vm,
            &[lib_id.clone(), strlen_idx, Value::str("hello ffi")],
        )
        .unwrap();
        assert_eq!(len_res[0].to_string(), "9");

        let abs_res = builtin_call(&mut vm, &[lib_id.clone(), abs_idx, Value::Int(-99)]).unwrap();
        assert_eq!(abs_res[0].to_string(), "99");

        builtin_close(&mut vm, &[lib_id]).unwrap();
    }

    #[test]
    fn test_ffi_bounds_interior_post_free_readcstring_and_limits() {
        use crate::vm::limits::ExecutionLimits;
        use crate::vm::permissions::{PermissionScope, Permissions};

        // `--allow-ffi=/opt/lib/foo.so` should still allow in-process memory helpers,
        // while `DenyAll` rejects them.
        let mut vm_denied = VM::new();
        vm_denied.set_permissions(Permissions::deny_all());
        assert!(
            builtin_alloc(&mut vm_denied, &[Value::Int(8)])
                .unwrap_err()
                .contains("permission denied")
        );

        let mut vm = VM::new();
        vm.set_permissions(
            Permissions::deny_all()
                .with_ffi(PermissionScope::AllowList(vec!["/opt/lib/foo.so".into()])),
        );
        vm.set_limits(ExecutionLimits::new().with_heap_limit(1024));

        // Respect `--max-memory` on `ffi.alloc`
        let too_big = builtin_alloc(&mut vm, &[Value::Int(2048)]).unwrap_err();
        assert!(too_big.contains("memory limit exceeded"), "got: {too_big}");

        let ptr = builtin_alloc(&mut vm, &[Value::Int(16)]).unwrap()[0].clone();
        let Value::Int(base_addr) = ptr else {
            panic!("expected integer pointer");
        };

        // Valid interior pointer read/write
        let interior = Value::Int(base_addr + 8);
        builtin_write(
            &mut vm,
            &[
                interior.clone(),
                Value::str("i32"),
                Value::Int(77),
                Value::Int(0),
            ],
        )
        .unwrap();
        assert_eq!(
            builtin_read(
                &mut vm,
                &[interior.clone(), Value::str("i32"), Value::Int(0)]
            )
            .unwrap()[0]
                .to_string(),
            "77"
        );

        // Interior pointer reading past end of allocation must fail
        let interior_oob = builtin_read(&mut vm, &[interior, Value::str("i64"), Value::Int(4)]);
        assert!(interior_oob.unwrap_err().contains("out of bounds"));

        // One-past-the-end pointer must fail
        let past_end = Value::Int(base_addr + 16);
        assert!(builtin_read(&mut vm, &[past_end, Value::str("u8"), Value::Int(0)]).is_err());

        // `readCString` without NUL before end of allocation must fail cleanly
        builtin_write(
            &mut vm,
            &[
                ptr.clone(),
                Value::str("u8"),
                Value::Int(65),
                Value::Int(14),
            ],
        )
        .unwrap();
        builtin_write(
            &mut vm,
            &[
                ptr.clone(),
                Value::str("u8"),
                Value::Int(66),
                Value::Int(15),
            ],
        )
        .unwrap();
        let cstr_oob = builtin_read_cstring(&mut vm, &[Value::Int(base_addr + 14), Value::Int(64)]);
        assert!(cstr_oob.unwrap_err().contains("out of bounds"));

        // Arbitrary unowned integer must fail safe `read`/`write`/`readCString`
        assert!(
            builtin_read(
                &mut vm,
                &[Value::Int(0x1234), Value::str("u8"), Value::Int(0)]
            )
            .is_err()
        );
        assert!(builtin_read_cstring(&mut vm, &[Value::Int(0x1234)]).is_err());

        // Free and verify post-free access (use-after-free) fails
        builtin_free(&mut vm, std::slice::from_ref(&ptr)).unwrap();
        assert!(builtin_read(&mut vm, &[ptr.clone(), Value::str("u8"), Value::Int(0)]).is_err());
        assert!(
            builtin_write(
                &mut vm,
                &[ptr.clone(), Value::str("u8"), Value::Int(1), Value::Int(0)]
            )
            .is_err()
        );
        assert!(builtin_read_cstring(&mut vm, &[ptr]).is_err());
    }
}
