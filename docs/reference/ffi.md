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

Opens a shared library at `path` (or the current process when `path` is `nil` on Unix) and binds each named symbol in `signatures` (`{ args = { ... }, returns = "..." }`). Returns a frozen table of callable functions plus `:close()`.

## ffi.loadExtension(path: string): table

Loads a Neyuki native extension (`cdylib` exporting `neyuki_extension_init`) and returns a frozen table of the functions it registered plus `:close()`.

## ffi.alloc(size: int): int

Allocates `size` zeroed bytes of C memory and returns its address.

## ffi.free(ptr: int): void

Frees a pointer previously returned by `ffi.alloc`.

## ffi.read(ptr: int, type: string, offset: int?): any

Reads a C value of `type` (`bool`, `i8`, `u8`, `i16`, `u16`, `i32`, `u32`, `i64`, `u64`, `f32`, `f64`, `ptr`, `cstr`) from `ptr + offset`.

## ffi.write(ptr: int, type: string, value: any, offset: int?): void

Writes `value` as C `type` to `ptr + offset`.

## ffi.readCString(ptr: int?, maxLen: int?): string?

Reads a NUL-terminated C string from `ptr`, or returns `nil` when `ptr` is `nil` or `0`.

## ffi.sizeof(type: string): int

Returns the byte size of C `type`.
