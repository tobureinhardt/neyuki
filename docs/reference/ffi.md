# @neyuki/ffi

Dynamic C-ABI foreign function interface and native extension loader. Loads shared libraries (`.so`, `.dylib`, `.dll`) and binds typed C functions, allocates and inspects raw C memory, and loads versioned Neyuki native extensions. Requires `--allow-ffi` when running in sandboxed mode.

```lua
local ffi = require("@neyuki/ffi")

local ptr = ffi.alloc(8)
ffi.write(ptr, "i64", 123456)
print(ffi.read(ptr, "i64")) --> 123456
ffi.free(ptr)
```

## ffi.open(path: string?, signatures: table): table

Opens a shared library at `path` (or the current process when `path` is `nil` on Unix) and binds each named symbol in `signatures` (`{ args = { ... }, returns = "..." }`). Returns a frozen table of callable functions plus `:close()`. Under an allow-listed `--allow-ffi`, `path` must be an existing file inside the allow-list and is loaded by its resolved absolute path; bare names such as `libc.so.6` and `nil` need unrestricted `--allow-ffi`.

```lua
local m = ffi.open("libm.so.6", {
    ldexp = { args = { "f64", "i32" }, returns = "f64" },
    fmaf = { args = { "f32", "f32", "f32" }, returns = "f32" },
})
print(m.ldexp(0.75, 4)) --> 12.0
```

Types: `void` (returns only), `bool`, `i8`, `u8`, `i16`, `u16`, `i32`/`int`, `u32`/`uint`, `i64`, `u64`, `isize`, `usize`, `f32`/`float`, `f64`/`double`, `ptr` (an integer address, a `buffer`, or `nil`), and `cstr`/`string` (a NUL-terminated string; `nil` for `NULL`).

- **Argument mixes.** Integer and float arguments can be mixed in any order. The limits are 6 integer/pointer plus 8 float arguments on x86-64 Linux and macOS, 8 plus 8 on ARM64, and 8 arguments of any kind on x64 Windows.
- **Return values.** Narrow integer returns are sign- or zero-extended from their declared width. A `u64` above the `int` range comes back as a big integer, and a `NULL` `ptr` or `cstr` comes back as `nil`.
- **Not supported.** Variadic functions (`printf`), structs passed by value and C callbacks. Write a [native extension](#native-extensions) for those.

## ffi.loadExtension(path: string): table

Loads a Neyuki native extension (a shared library exporting `neyuki_extension_init`) and returns a frozen table of the functions it registered plus `:close()`. Path rules under an allow-list are the same as for `ffi.open`. Packages can ship extensions for every platform and load them with `require`; see [native packages](../runtime.md#native-packages).

## ffi.target(): string

The target this runtime runs on, as native packages name it: `x86_64-linux`, `aarch64-macos`, `x86_64-windows`, and so on (`-musl` is appended on musl Linux).

## ffi.abiVersion: int

The native extension ABI version this runtime speaks (`2`). It also loads extensions and [native packages](../runtime.md#native-packages) built for ABI `1`.

## Native extensions

An extension is a C-ABI shared library written against [`include/neyuki_ext.h`](../../include/neyuki_ext.h). It exports `int32_t neyuki_extension_init(const NeyukiExtApi *api, void *ctx)`, which keeps `api`, calls `api->register_fn` for each function and returns `0`. Each function has the signature `NeyukiExtValue fn(size_t argc, const NeyukiExtValue *argv, void *user_data)`.

```c
#include "neyuki_ext.h"

static const NeyukiExtApi *api;

/* sum(list) -> total, count */
static NeyukiExtValue sum(size_t argc, const NeyukiExtValue *argv, void *ud) {
    static NeyukiExtValue out[2];
    if (argc != 1 || api->handle_kind(argv[0]) != NEYUKI_KIND_TABLE)
        return neyuki_error("sum expects a list", 18);
    size_t n = api->table_len(argv[0]);
    int64_t total = 0;
    for (size_t i = 1; i <= n; i++)
        total += api->table_get(argv[0], neyuki_int((int64_t)i)).i;
    out[0] = neyuki_int(total);
    out[1] = neyuki_int((int64_t)n);
    return neyuki_multi(out, 2);
}

NEYUKI_EXPORT int32_t neyuki_extension_init(const NeyukiExtApi *host, void *ctx) {
    if (host->abi_version < NEYUKI_EXT_ABI_VERSION) return 1; /* runtime too old */
    api = host;
    return api->register_fn(ctx, (const uint8_t *)"sum", 3, sum, NULL) ? 0 : 2;
}
```

```lua
local ext = ffi.loadExtension("./libsum.so")
local total, count = ext.sum({ 1, 2, 3 }) --> 6, 3
```

### Writing extensions in Rust

The [`neyuki-ext`](../../crates/neyuki-ext/README.md) crate wraps this ABI in safe Rust: build a `cdylib`, register functions that take `&[Value]` and return `Result<_, String>`, and `neyuki_ext::export!` writes `neyuki_extension_init`. Its `args` helpers raise the same errors as the standard libraries (`width must be a number, got string`), panics are raised as script errors, and it covers services and the task scheduler below.

```rust
use neyuki_ext::{Module, Value, args};

fn init(m: &mut Module) -> Result<(), String> {
    m.function("hello", |a: &[Value]| Ok(format!("hello, {}", args::str_arg(a, 0, "name")?)))?;
    Ok(())
}

neyuki_ext::export!(init);
```

### Values

`NeyukiExtValue` is `{ uint8_t tag; int64_t i; double f; const uint8_t *ptr; size_t len; }`.

| Tag | Neyuki value | Fields |
| --- | --- | --- |
| `NEYUKI_NIL` | `nil` | |
| `NEYUKI_BOOL` | `boolean` | `i` is 0 or 1 |
| `NEYUKI_INT` | `int` | `i` |
| `NEYUKI_FLOAT` | `float` | `f` |
| `NEYUKI_STRING` | `string` | `ptr`, `len` (UTF-8; invalid bytes become U+FFFD on the way in) |
| `NEYUKI_HANDLE` | table, function, buffer or big integer | `i` is the handle |
| `NEYUKI_ERROR` | raises an error | `ptr`, `len` hold the message |
| `NEYUKI_MULTI` | several return values | `ptr` points at `len` values |
| `NEYUKI_YIELD` | suspends the running task | only right after `task_park` (ABI 2) |

Returning `NEYUKI_MULTI` gives the script several values; returning `NEYUKI_ERROR` raises an error it can catch with `pcall`.

### Host API

`NeyukiExtApi` gives extensions access to script values. Its layout is append-only: later versions add fields at the end and raise `struct_size`, so an extension built for an older ABI keeps loading.

- `handle_kind(v)`: `NEYUKI_KIND_TABLE`, `_FUNCTION`, `_BUFFER`, `_BIGINT`, or `_INVALID` for a value that is not a live handle.
- `new_table(array_hint, field_hint)`, `table_get(t, key)`, `table_set(t, key, value)` (returns `NIL`, or an `ERROR` for a frozen table or a nil key), `table_len(t)` (`#t`), and `table_next(t, key, &out_key, &out_value)` (start with `NIL`; returns false at the end). These are raw accesses that skip metamethods.
- `call(fn, argc, argv)` calls a script function and returns its result: `NIL` for none, `MULTI` for several, or `ERROR` when it raised one. Return that `ERROR` to pass the error on.
- `new_buffer(data, len)` copies bytes into a new `buffer` (zero-filled when `data` is `NULL`). `buffer_data(b, &ptr, &len)` exposes a buffer's bytes for reading and writing until the next `call` or until your function returns.
- `to_string(v)` is `tostring(v)`.
- `retain(handle)` returns a persistent handle and `release(handle)` drops it (see below).
- `check_permission(kind, target, len)` asks the sandbox about `NEYUKI_PERM_READ`, `_WRITE`, `_NET`, `_RUN`, `_ENV` or `_FFI`. It returns `NIL` when the access is allowed, or an `ERROR` to return. Native code is not sandboxed by itself, so an extension that does I/O on a script's behalf should check first.

### Services (ABI 2)

One extension can hand others a table of C functions, as `@nyk/ui` does for the packages that draw into its windows. In `neyuki_extension_init`, `provide_service(ctx, name, len, ptr)` publishes `ptr` (memory the extension owns, usually a `static` struct of function pointers) under `name`; any extension then finds it with `lookup_service(name, len)`, which returns `NULL` when nothing provides it. Put a version in the name (`"nyk.ui/1"`) and change it when the struct changes. A service is forgotten when the library that provides it is closed, so look it up when you need it rather than once at load. Require the providing package first: a service exists only once its library has loaded.

### Tasks and event loops (ABI 2)

An extension that runs its own event loop (windows, audio devices) can let the [task scheduler](task.md) keep running while it waits:

- `task_park(timeout_secs, pump, user_data)` parks the running task and returns `NEYUKI_YIELD`, which your function must return at once. While other tasks run, the scheduler calls `pump(wait_secs, user_data)`, which waits up to `wait_secs` and returns the events that happened as an array table (the API is usable inside it). The task resumes with that table as the result of your function, or with an empty table once `timeout_secs` passes. `task_park` returns `NIL` instead when it cannot park: outside a task, under a native that called back into Neyuki, or for a zero timeout. Then call your pump yourself and return what it returns.
- `set_waker(wake, user_data)` installs the function background work (`fs`, `http`, `net`, `os.spawn` callbacks) calls when it completes, so a loop blocked in the OS returns and lets the scheduler run. It is called from worker threads: it must be thread-safe and must not use the API. `has_waker()` says whether one is installed.
- `next_due(&secs)` stores the seconds until the next task is due (`0` when one is ready) and returns false when no task is waiting; cap your wait with it. `run_tasks(budget_secs)` resumes the due tasks for about that long.

Check `api->abi_version >= 2` (or `struct_size`) before using these fields.

### Handles and ownership

- **Local handles.** Arguments and API results are valid until the extension call that received them returns. **Persistent handles** from `retain` are valid until `release`, so an extension can keep a callback or a table between calls. A handle used after it expired is reported as an error, not undefined behaviour.
- **Host memory.** Strings and arrays that the host hands out (argument strings, `to_string`, `table_get`, `call` results, error messages) are valid until the current extension call returns.
- **Extension memory.** Strings, errors and `MULTI` arrays that the extension returns stay owned by the extension. The host copies them before the call returns, so static or per-call storage is fine and the host never frees them.
- **Threads.** Call the API only from the thread that called you, and only while one of your functions (or `neyuki_extension_init`) is running. Otherwise it returns an `ERROR`, `false` or `0`.

## ffi.alloc(size: int): int

Allocates `size` zeroed bytes of C memory (up to 64 MiB per allocation and subject to `--max-memory`) and returns its address.

## ffi.free(ptr: int): void

Frees a pointer previously returned by `ffi.alloc`.

## ffi.read(ptr: int, type: string, offset: int?): any

Bounds-checked read of C `type` (`bool`, `i8`, `u8`, `i16`, `u16`, `i32`, `u32`, `i64`, `u64`, `f32`, `f64`, `ptr`, `cstr`) from `ptr + offset`. Only pointers inside live `ffi.alloc` allocations are allowed (including valid interior offsets); freed, out-of-bounds, arbitrary integer, and foreign C pointers are rejected.

## ffi.write(ptr: int, type: string, value: any, offset: int?): void

Bounds-checked write of `value` as C `type` to `ptr + offset` inside a live `ffi.alloc` allocation.

## ffi.readCString(ptr: int?, maxLen: int?): string?

Bounds-checked read of a NUL-terminated C string from a live `ffi.alloc` allocation (up to `maxLen`, default 65536), or `nil` when `ptr` is `nil` or `0`. Bytes are decoded with lossy UTF-8 replacement (`from_utf8_lossy`).

## ffi.readUnsafe(ptr: int, type: string, offset: int?): any

Unchecked variant of `ffi.read` that accepts raw foreign C pointers without `ffi.alloc` bounds checks. Requires unrestricted `--allow-ffi` (an allow-list is refused, since a raw pointer can reach any process memory).

## ffi.writeUnsafe(ptr: int, type: string, value: any, offset: int?): void

Unchecked variant of `ffi.write` that accepts raw foreign C pointers without `ffi.alloc` bounds checks. Requires unrestricted `--allow-ffi` (an allow-list is refused, since a raw pointer can reach any process memory).

## ffi.readCStringUnsafe(ptr: int?, maxLen: int?): string?

Unchecked variant of `ffi.readCString` that reads a NUL-terminated C string from a raw foreign pointer. Requires unrestricted `--allow-ffi` (an allow-list is refused, since a raw pointer can reach any process memory).

## ffi.sizeof(type: string): int

Returns the byte size of C `type`.
