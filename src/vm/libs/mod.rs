//! The standard libraries.
//!
//! The core libraries (`math`, `string`, `table`, ...) are native tables set
//! as globals. The rest are `lib/*.nyk` modules bundled into the executable
//! and built on the `__`-prefixed natives registered here.

pub mod args;
pub mod background;
pub mod bit;
pub mod buffer;
pub mod coroutine;
pub mod crypto;
pub mod debug;
pub mod fs;
pub mod gpu;
pub mod hid;
pub mod http;
pub mod io;
pub mod json;
pub mod math;
pub mod net;
pub mod os;
pub mod sql;
pub mod string;
pub mod strlib;
pub mod table;
pub mod task;
pub mod ui;
pub mod utf8;
pub mod webview;
#[cfg(all(
    feature = "webview",
    windows,
    target_env = "gnu",
    target_arch = "x86_64"
))]
pub mod webview2_loader;

pub use bit::create_bit_lib;
pub use buffer::create_buffer_lib;
pub use coroutine::create_coroutine_lib;
pub use debug::create_debug_lib;
pub use json::create_json_lib;
pub use math::create_math_lib;
pub use os::create_os_lib;
pub use string::create_string_lib;
pub use table::create_table_lib;
pub use task::create_task_lib;
pub use utf8::create_utf8_lib;

use crate::vm::machine::VM;

/// Every `lib/*.nyk` module, keyed by its `@neyuki/...` name.
pub(crate) const BUNDLED_LIBRARIES: &[(&str, &str)] =
    include!(concat!(env!("OUT_DIR"), "/bundled_libraries.rs"));

/// Registers the natives the bundled `lib/*.nyk` modules are built on.
pub fn register_natives(vm: &mut VM) {
    for natives in [
        crypto::NATIVES,
        fs::NATIVES,
        gpu::NATIVES,
        hid::NATIVES,
        http::NATIVES,
        io::NATIVES,
        net::NATIVES,
        sql::NATIVES,
        ui::NATIVES,
        webview::NATIVES,
    ] {
        for def in natives {
            vm.register_native(def);
        }
    }
}

/// Library functions with no side effects, which the compiler may evaluate
/// ahead of time when every argument is a constant.
const PURE: &[(&str, &[&str])] = &[
    (
        "math",
        &[
            "abs", "acos", "asin", "atan", "atan2", "cbrt", "ceil", "clamp", "cos", "cosh", "deg",
            "exp", "floor", "fmod", "isfinite", "isinf", "isnan", "ldexp", "lerp", "log", "log10",
            "log2", "map", "max", "min", "pow", "rad", "round", "sign", "sin", "sinh", "sqrt",
            "tan", "tanh", "trunc",
        ],
    ),
    (
        "string",
        &[
            "count",
            "endswith",
            "len",
            "lower",
            "ltrim",
            "rep",
            "reverse",
            "rtrim",
            "startswith",
            "sub",
            "trim",
            "upper",
        ],
    ),
];

/// Folded strings longer than this stay calls, so constants stay small.
const MAX_FOLDED_STRING: usize = 4096;

/// Evaluates `module.name(args)` with the real native when the function is
/// pure and it returns exactly one plain value, so folded and run-time
/// results can never differ. Errors are left for run time to report.
pub fn fold_call(module: &str, name: &str, args: &[crate::vm::Value]) -> Option<crate::vm::Value> {
    use crate::vm::Value;
    use std::cell::RefCell;

    PURE.iter()
        .find(|(m, _)| *m == module)
        .filter(|(_, names)| names.contains(&name))?;
    thread_local! {
        static FOLD_VM: RefCell<Option<VM>> = const { RefCell::new(None) };
    }
    FOLD_VM.with(|cell| {
        let mut slot = cell.borrow_mut();
        let vm = slot.get_or_insert_with(VM::new);
        let Some(Value::Table(lib)) = vm.modules.get(module).cloned() else {
            return None;
        };
        let Value::Native(def) = lib.borrow().get_str(name) else {
            return None;
        };
        let results = (def.func)(vm, args).ok()?;
        match results.as_slice() {
            [Value::String(s)] if s.len() > MAX_FOLDED_STRING => None,
            [
                v @ (Value::Nil
                | Value::Bool(_)
                | Value::Int(_)
                | Value::BigInt(_)
                | Value::Float(_)
                | Value::String(_)),
            ] => Some(v.clone()),
            _ => None,
        }
    })
}
