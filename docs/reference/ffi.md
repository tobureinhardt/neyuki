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

### Extension ABI (`NeyukiExtValue`)

- **Arguments (`value_to_ext`)**: Neyuki keeps ownership of string buffers for the duration of the extension call; extensions must not retain argument `ptr` after returning. Non-string values passed to extension arguments are converted with `to_string()`.
- **Return strings & errors (`EXT_TAG_STRING`, `EXT_TAG_ERROR`)**: The extension retains ownership of returned buffers (static data or extension-managed storage kept alive long enough for the host to copy immediately on return). The host never frees returned extension pointers and decodes returned bytes with `from_utf8_lossy` (non-UTF-8 binary data is replaced with U+FFFD).

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

Unchecked variant of `ffi.read` that accepts raw foreign C pointers without `ffi.alloc` bounds checks. Requires non-denied `--allow-ffi`.

## ffi.writeUnsafe(ptr: int, type: string, value: any, offset: int?): void

Unchecked variant of `ffi.write` that accepts raw foreign C pointers without `ffi.alloc` bounds checks. Requires non-denied `--allow-ffi`.

## ffi.readCStringUnsafe(ptr: int?, maxLen: int?): string?

Unchecked variant of `ffi.readCString` that reads a NUL-terminated C string from a raw foreign pointer. Requires non-denied `--allow-ffi`.

## ffi.sizeof(type: string): int

Returns the byte size of C `type`.
