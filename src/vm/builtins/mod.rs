//! Registers the global functions.

pub mod gc;
pub mod global;
pub mod iter;
pub mod mem;
pub mod meta;

pub use gc::builtin_collectgarbage;
pub use global::{
    builtin_assert, builtin_asserttype, builtin_checktype, builtin_error, builtin_float,
    builtin_int, builtin_pcall, builtin_print, builtin_require, builtin_tonumber, builtin_tostring,
    builtin_try, builtin_type, builtin_typeof, builtin_xpcall,
};
pub use iter::NEXT;
pub use meta::{
    builtin_getmetatable, builtin_rawequal, builtin_rawget, builtin_rawlen, builtin_rawset,
    builtin_setmetatable,
};

use crate::vm::machine::VM;

pub fn register_all(vm: &mut VM) {
    // Globals
    vm.register_native(crate::native_def!("print", builtin_print));
    vm.register_native(crate::native_def!("assert", builtin_assert));
    vm.register_native(crate::native_def!("type", builtin_type));
    vm.register_native(crate::native_def!("typeof", builtin_typeof));
    vm.register_native(crate::native_def!("checktype", builtin_checktype));
    vm.register_native(crate::native_def!("asserttype", builtin_asserttype));
    vm.register_native(crate::native_def!("assert_type", builtin_asserttype));
    vm.register_native(crate::native_def!("tostring", builtin_tostring));
    vm.register_native(crate::native_def!("tonumber", builtin_tonumber));
    vm.register_native(crate::native_def!("int", builtin_int));
    vm.register_native(crate::native_def!("float", builtin_float));
    vm.register_native(crate::native_def!("error", builtin_error));
    vm.register_native(crate::native_def!("pcall", builtin_pcall));
    vm.register_native(crate::native_def!("xpcall", builtin_xpcall));
    vm.register_native(crate::native_def!("try", builtin_try));
    vm.register_native(crate::native_def!("require", builtin_require));
    vm.register_native(NEXT);
    vm.register_native(crate::native_def!("pairs", iter::builtin_pairs));
    vm.register_native(crate::native_def!("ipairs", iter::builtin_ipairs));
    vm.register_native(crate::native_def!("select", iter::builtin_select));
    vm.register_native(crate::native_def!("warn", iter::builtin_warn));
    vm.set_global("_G", iter::globals_table());
    vm.set_global(
        "_VERSION",
        crate::vm::Value::str(concat!("Neyuki ", env!("CARGO_PKG_VERSION"))),
    );

    // Garbage collector and memory regions
    vm.register_native(crate::native_def!("collectgarbage", builtin_collectgarbage));
    vm.register_native(crate::native_def!("job", mem::builtin_job));
    vm.register_native(crate::native_def!("share", mem::builtin_share));
    vm.register_native(crate::native_def!("look", mem::builtin_look));

    // Metatable and raw functions
    vm.register_native(crate::native_def!("setmetatable", builtin_setmetatable));
    vm.register_native(crate::native_def!("getmetatable", builtin_getmetatable));
    vm.register_native(crate::native_def!("rawget", builtin_rawget));
    vm.register_native(crate::native_def!("rawset", builtin_rawset));
    vm.register_native(crate::native_def!("rawequal", builtin_rawequal));
    vm.register_native(crate::native_def!("rawlen", builtin_rawlen));
}
