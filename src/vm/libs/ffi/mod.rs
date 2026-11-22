//! Native FFI and extension API backing `@neyuki/ffi`.
//!
//! Provides:
//! 1. Typed C-ABI dynamic library loading and invocation (`__ffi_open`,
//!    `__ffi_call`, `__ffi_close`) via `libloading`; `call.rs` places the
//!    arguments in registers.
//! 2. Bounds-checked C-memory helpers (`__ffi_alloc`, `__ffi_free`,
//!    `__ffi_read`, `__ffi_write`, `__ffi_read_cstring`, `__ffi_sizeof`).
//! 3. The versioned Neyuki native extension ABI (`ext.rs`:
//!    `__ffi_load_extension`, `__ffi_ext_call`), which native packages
//!    (`[native]` in `neyuki.toml`) are loaded through as well.

mod call;
pub mod ext;
#[cfg(test)]
mod sdk_tests;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString, c_char, c_void};
use std::rc::Rc;

use libloading::Library;

use self::call::AbiArg;
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
    NativeDef {
        name: "__ffi_target",
        func: builtin_target,
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
}

// ---------------------------------------------------------------------------
// Registries for loaded libraries, bound symbols, and allocated buffers
// ---------------------------------------------------------------------------

struct BoundSymbol {
    ptr: *const c_void,
    args: Vec<FfiType>,
    ret: FfiType,
}

pub(crate) struct LoadedLibrary {
    _lib: Option<Library>,
    symbols: Vec<BoundSymbol>,
    ext_fns: Vec<ext::BoundExtFn>,
}

struct AllocEntry {
    len: usize,
}

thread_local! {
    pub(crate) static LIBRARIES: RefCell<HashMap<u64, LoadedLibrary>> = RefCell::new(HashMap::new());
    static NEXT_LIB_ID: Cell<u64> = const { Cell::new(1) };
    static ALLOCS: RefCell<BTreeMap<usize, AllocEntry>> = const { RefCell::new(BTreeMap::new()) };
    static FFI_ALLOCATED_BYTES: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn next_lib_id() -> u64 {
    NEXT_LIB_ID.with(|id| {
        let v = id.get();
        id.set(v + 1);
        v
    })
}

pub(crate) fn open_library(path: &std::path::Path) -> Result<Library, String> {
    if path.as_os_str().is_empty() {
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
        .map_err(|err| format!("failed to load library '{}': {}", path.display(), err))
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
    call::check_signature(&args)?;
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
    let resolved = vm.permissions.resolve_ffi_library(path)?;

    let sigs_tbl = table_arg(args, 1, "signatures")?;
    let lib = open_library(&resolved)?;

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
        abi_args.push(value_to_abi_arg(val, *ty, &mut cstrings)?);
    }

    // SAFETY: `ptr` is valid while `lib_id` is open in `LIBRARIES`, `cstrings`
    // outlives the call, and argument/return types match the declared C-ABI
    // signature, which `check_signature` accepted when it was bound.
    let result = unsafe { call::call(ptr, &abi_args, ret_ty)? };
    Ok(vec![result])
}

fn builtin_close(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let lib_id = int_arg(args, 0, "library")? as u64;
    let removed = LIBRARIES.with(|libs| libs.borrow_mut().remove(&lib_id));
    let closed = removed.is_some();
    if let Some(lib) = removed {
        ext::forget_services(lib_id);
        ext::unload_when_idle(lib);
    }
    Ok(vec![Value::Bool(closed)])
}

// ---------------------------------------------------------------------------
// Native extensions (see ext.rs)
// ---------------------------------------------------------------------------

fn builtin_load_extension(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let path = str_arg(args, 0, "path")?;
    let resolved = vm.permissions.resolve_ffi_library(path)?;
    let (id, fn_map) = ext::load_extension(vm, &resolved, path)?;
    Ok(vec![
        Value::from_usize(id as usize),
        Value::Table(Rc::new(RefCell::new(fn_map))),
    ])
}

fn builtin_ext_call(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let lib_id = int_arg(args, 0, "extension")? as u64;
    let fn_idx = int_arg(args, 1, "function")? as usize;
    ext::call_ext(vm, lib_id, fn_idx, &args[2..])
}

fn builtin_target(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::str(&crate::pkg::native::current_target())])
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
    if checked {
        vm.permissions.check_ffi_memory()?;
    } else {
        vm.permissions.check_ffi_unsafe_memory()?;
    }
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
    if checked {
        vm.permissions.check_ffi_memory()?;
    } else {
        vm.permissions.check_ffi_unsafe_memory()?;
    }
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
    if checked {
        vm.permissions.check_ffi_memory()?;
    } else {
        vm.permissions.check_ffi_unsafe_memory()?;
    }
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

    fn allow_list_vm(entries: &[&str]) -> VM {
        use crate::vm::permissions::{PermissionScope, Permissions};
        let mut vm = VM::new();
        vm.set_permissions(
            Permissions::allow_all().with_ffi(PermissionScope::AllowList(
                entries.iter().map(|s| s.to_string()).collect(),
            )),
        );
        vm
    }

    fn empty_sigs() -> Value {
        Value::Table(Rc::new(RefCell::new(VmTable::new())))
    }

    #[cfg(unix)]
    #[test]
    fn allow_list_rejects_search_path_and_self_loading() {
        let mut vm = allow_list_vm(&["."]);
        // Resolves to ./libc.so.6 (inside "."), which does not exist; dlopen
        // must not fall back to the system search path.
        for name in ["libc.so.6", "libm.so.6", "libdl.so.2"] {
            let err = builtin_open(&mut vm, &[Value::str(name), empty_sigs()]).err();
            assert!(err.is_some(), "{name} must not load by search path");
        }
        let err = builtin_open(&mut vm, &[Value::Nil, empty_sigs()])
            .err()
            .unwrap();
        assert!(err.contains("permission denied"), "{err}");
        let err = builtin_load_extension(&mut vm, &[Value::str("libc.so.6")])
            .err()
            .unwrap();
        assert!(err.contains("permission denied"), "{err}");
    }

    #[test]
    fn allow_list_blocks_unchecked_memory_ops() {
        let mut vm = allow_list_vm(&["/opt/lib/demo.so"]);
        let target = Value::Int(0x1000);
        assert!(
            builtin_read_unsafe(&mut vm, &[target.clone(), Value::str("i32")])
                .unwrap_err()
                .contains("unrestricted")
        );
        assert!(
            builtin_write_unsafe(&mut vm, &[target.clone(), Value::str("i32"), Value::Int(1)])
                .unwrap_err()
                .contains("unrestricted")
        );
        assert!(
            builtin_read_cstring_unsafe(&mut vm, &[target])
                .unwrap_err()
                .contains("unrestricted")
        );
        // Checked operations on ffi.alloc memory keep working.
        let ptr = builtin_alloc(&mut vm, &[Value::Int(8)]).unwrap()[0].clone();
        builtin_write(&mut vm, &[ptr.clone(), Value::str("i32"), Value::Int(7)]).unwrap();
        assert_eq!(
            builtin_read(&mut vm, &[ptr.clone(), Value::str("i32")]).unwrap()[0].to_string(),
            "7"
        );
        builtin_free(&mut vm, &[ptr]).unwrap();
    }

    /// Compiles `tests/fixtures/native/<name>.c` into a shared library in
    /// `dir`, or `None` (the test is skipped) when no C compiler is around.
    #[cfg(unix)]
    fn build_fixture(name: &str, dir: &std::path::Path) -> Option<std::path::PathBuf> {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/native")
            .join(format!("{name}.c"));
        let ext = if cfg!(target_os = "macos") {
            "dylib"
        } else {
            "so"
        };
        let out = dir.join(format!("lib{name}.{ext}"));
        let status =
            std::process::Command::new(std::env::var("CC").unwrap_or_else(|_| "cc".into()))
                .args(["-shared", "-fPIC", "-O1", "-o"])
                .arg(&out)
                .arg(&src)
                .status()
                .ok()?;
        if !status.success() {
            eprintln!("skipping: could not compile {}", src.display());
            return None;
        }
        Some(out)
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("neyuki_ffi_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn run_script(vm: &mut VM, src: &str) -> Result<Value, String> {
        let proto =
            crate::compiler::compile_program(src, "ffi_test", crate::compiler::Options::RUN)
                .map_err(|e| e.to_string())?;
        vm.execute(proto)
    }

    #[cfg(unix)]
    #[test]
    fn open_calls_mixed_int_and_float_signatures() {
        let dir = scratch("open_mixed");
        let Some(lib) = build_fixture("demo_c", &dir) else {
            return;
        };
        let mut vm = VM::new();
        let src = format!(
            r#"
            local ffi = require("@neyuki/ffi")
            local c = ffi.open("{}", {{
                mix = {{ args = {{ "i32", "f64", "i64", "f32", "u8", "f64" }}, returns = "f64" }},
                scale = {{ args = {{ "f32", "i32" }}, returns = "f32" }},
                sum8 = {{ args = {{ "i64", "i64", "i64", "i64", "i64", "i64" }}, returns = "i64" }},
                fsum8 = {{ args = {{ "f64", "f64", "f64", "f64", "f64", "f64", "f64", "f64" }}, returns = "f64" }},
                neg8 = {{ args = {{ "i8" }}, returns = "i8" }},
                wrap16 = {{ args = {{ "u32" }}, returns = "u16" }},
                minus_one = {{ returns = "i32" }},
                is_even = {{ args = {{ "i32" }}, returns = "bool" }},
            }})
            assert(c.mix(1, 2.0, 3, 4.0, 5, 6.0) == 1 + 20 + 300 + 4000 + 50000 + 600000)
            assert(c.scale(1.5, 4) == 6.0)
            assert(c.sum8(1, 2, 3, 4, 5, 6) == 21)
            assert(c.fsum8(1, 2, 3, 4, 5, 6, 7, 8) == 36)
            assert(c.neg8(5) == -5)
            assert(c.wrap16(65537) == 1)
            assert(c.minus_one() == -1)
            assert(c.is_even(4) == true and c.is_even(3) == false)
            return true
            "#,
            lib.display()
        );
        assert_eq!(run_script(&mut vm, &src).unwrap(), Value::Bool(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn c_extension_round_trips_tables_callbacks_and_buffers() {
        let dir = scratch("c_ext");
        let Some(lib) = build_fixture("demo_ext", &dir) else {
            return;
        };
        let mut vm = VM::new();
        let src = format!(
            r#"
            local ffi = require("@neyuki/ffi")
            local ext = ffi.loadExtension("{}")
            assert(ext.greet("yuki") == "hello, yuki")
            local lo, hi, sum = ext.stats({{ 4, 1.5, 9 }})
            assert(lo == 1.5 and hi == 9 and sum == 14.5)
            local doubled = ext.map({{ 1, 2, 3 }}, function(x) return x * 2 end)
            assert(#doubled == 3 and doubled[3] == 6)
            local ok, err = pcall(ext.map, {{ 1 }}, function() error("nope") end)
            assert(not ok and string.find(tostring(err), "nope") != nil)
            local buf = buffer.fromstring("abc")
            assert(ext.reverse(buf) == buf and buffer.tostring(buf) == "cba")
            assert(ext.readable("/etc/hosts") == true)
            local ok2, err2 = pcall(ext.greet, 42)
            assert(not ok2 and string.find(tostring(err2), "short string") != nil)
            -- Closing the extension from a callback it is running must not
            -- unload its code under it; later calls fail cleanly.
            local again = ext.map({{ 5 }}, function(x) ext:close() return x end)
            assert(again[1] == 5)
            assert(not pcall(ext.greet, "late"))
            return true
            "#,
            lib.display()
        );
        assert_eq!(run_script(&mut vm, &src).unwrap(), Value::Bool(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An extension's code has to outlive the thread that loaded it: its
    /// thread-exit destructors (every Rust thread-local has one) run after
    /// the runtime's own, which drop the loaded libraries.
    #[cfg(unix)]
    #[test]
    fn extensions_stay_loaded_for_their_thread_exit_destructors() {
        let dir = scratch("thread_local_ext");
        let Some(lib) = build_fixture("thread_local_ext", &dir) else {
            return;
        };
        let src = format!(
            "local ffi = require(\"@neyuki/ffi\")\nlocal ext = ffi.loadExtension({:?})\nreturn ext.touch()",
            lib.display().to_string()
        );
        let finished = std::thread::spawn(move || {
            let mut vm = VM::new();
            run_script(&mut vm, &src).map(|value| value.to_string())
        })
        .join()
        .expect("the script thread exits cleanly");
        assert_eq!(finished.unwrap(), "true");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn native_package_loads_through_require_and_respects_the_sandbox() {
        use crate::vm::permissions::{PermissionScope, Permissions};
        let dir = scratch("native_pkg");
        let Some(lib) = build_fixture("demo_ext", &dir) else {
            return;
        };
        let target = crate::pkg::native::current_target();
        let pkg = dir.join("neyuki_packages/demo/stats");
        std::fs::create_dir_all(pkg.join("native")).unwrap();
        std::fs::copy(&lib, pkg.join("native/libdemo.so")).unwrap();
        std::fs::write(
            pkg.join("neyuki.toml"),
            format!(
                "[package]\nname = \"@demo/stats\"\nversion = \"1.0.0\"\n\n[native]\nabi = 1\n{target} = \"native/libdemo.so\"\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("neyuki.toml"),
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let script = r#"
            local a = require("@pkg/demo/stats")
            local b = require("@pkg/demo/stats/native")
            assert(a == b, "both names load the same cached module")
            return a.greet("pkg")
        "#;
        let mut vm = VM::new();
        vm.script_dir = Some(dir.clone());
        assert_eq!(
            run_script(&mut vm, script).unwrap(),
            Value::str("hello, pkg")
        );

        // Native code is unsandboxed, so a sandbox without --allow-ffi refuses it...
        let mut vm = VM::new();
        vm.script_dir = Some(dir.clone());
        vm.set_permissions(Permissions::allow_all().with_ffi(PermissionScope::DenyAll));
        let err = run_script(&mut vm, script).unwrap_err();
        assert!(err.contains("permission denied"), "{err}");

        // ...and an allow-list must contain the library.
        let mut vm = VM::new();
        vm.script_dir = Some(dir.clone());
        vm.set_permissions(
            Permissions::allow_all().with_ffi(PermissionScope::AllowList(vec![
                pkg.to_string_lossy().into_owned(),
            ])),
        );
        assert_eq!(
            run_script(&mut vm, script).unwrap(),
            Value::str("hello, pkg")
        );

        // An ABI the runtime does not speak is refused with a clear message.
        std::fs::write(
            pkg.join("neyuki.toml"),
            format!("[package]\nname = \"@demo/stats\"\nversion = \"1.0.0\"\n\n[native]\nabi = 99\n{target} = \"native/libdemo.so\"\n"),
        )
        .unwrap();
        let mut vm = VM::new();
        vm.script_dir = Some(dir.clone());
        let err = run_script(&mut vm, script).unwrap_err();
        assert!(err.contains("ABI 99"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn abi_layout_matches_the_c_header() {
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<ext::NeyukiExtValue>(), 40);
        assert_eq!(offset_of!(ext::NeyukiExtValue, i), 8);
        assert_eq!(offset_of!(ext::NeyukiExtValue, ptr), 24);
        assert_eq!(offset_of!(ext::NeyukiExtApi, register_fn), 8);
        // ABI 1 ends after 14 pointers; ABI 2 appends 7 more.
        assert_eq!(
            offset_of!(ext::NeyukiExtApi, provide_service),
            8 + 14 * size_of::<usize>()
        );
        assert_eq!(size_of::<ext::NeyukiExtApi>(), 8 + 21 * size_of::<usize>());
    }
}
