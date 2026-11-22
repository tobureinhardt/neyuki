# neyuki-ext

Write [Neyuki](https://neyuki.cc) native extensions and native packages in safe Rust.

The crate wraps the C ABI of [`include/neyuki_ext.h`](../../include/neyuki_ext.h) (ABI 2) and has no dependencies. An extension built with it is a `cdylib` that scripts load with `ffi.loadExtension(path)`, or with `require` from a package that lists it under `[native]` in `neyuki.toml`.

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
neyuki-ext = "0.1"
```

```rust
use neyuki_ext::{Module, Table, Value, args};

fn sum(a: &[Value]) -> Result<(i64, usize), String> {
    let list = args::table_arg(a, 0, "list")?;
    let mut total = 0;
    for v in list.list()? {
        total += v.as_i64().ok_or("list items must be integers")?;
    }
    Ok((total, list.len()))
}

fn init(m: &mut Module) -> Result<(), String> {
    m.function("sum", sum)?;
    m.function("pair", |_| Table::from_list([1, 2]))?;
    Ok(())
}

neyuki_ext::export!(init);
```

```lua
local ext = ffi.loadExtension("./libsum.so")
local total, count = ext.sum({ 1, 2, 3 }) --> 6, 3
```

## What it covers

- **Functions** take the arguments as `&[Value]` and return `Result<R, String>`. `R` can be `()`, anything that converts into a `Value`, a tuple of up to three of those, a `Vec<Value>`, or a `Return`. An `Err` raises a script error, and a panic is caught and raised as one, so it never crosses into the runtime.
- **`args`** checks arguments with the runtime's own wording (`width must be provided`, `n must be an integer, got string`). A library keeps its error messages when it moves from the runtime into a package.
- **Values.** `Value` holds nil, booleans, integers, floats and strings (copied), plus `Table`, `Function`, `Buffer` and `BigInt` handles. A handle lasts until the call that received it returns. `retain()` gives a `Persistent` that lasts until it is dropped. A stale handle is an error from the host, never undefined behaviour.
- **`check_permission`** asks the sandbox before the extension does I/O on a script's behalf.
- **Services.** `Module::provide` publishes a `#[repr(C)]` vtable under a versioned name, and `lookup_service` finds one that another extension published. This is how `@nyk/gpu` and `@nyk/webview` reach `@nyk/ui`'s windows.
- **`tasks`** runs an event loop inside the task scheduler. `park` suspends the running task on your pump, and `set_waker`, `next_due` and `run_tasks` cover a loop that waits in place.

Call into the runtime only from the script's thread, while one of your functions (or `init`) is running.

The crate lives in the Neyuki repository next to the runtime, and `src/vm/libs/ffi/sdk_tests.rs` there checks it against the host side of the ABI.
