# Globals

Functions available everywhere without `require`. The standard libraries (`math`, `string`, `table`, `bit`, `buffer`, `coroutine`, `utf8`, `debug`, `json`, `os`) are globals too; the `@neyuki/...` modules are loaded with `require`.

## print(...: any): ()

Writes its arguments to standard output, converted with `tostring` and separated by tabs, followed by a newline.

```lua
print("total:", 42) --> total:	42
```

## assert(value: any, message: string?): ...any

Raises `message` (or "assertion failed!") when `value` is `false` or `nil`; otherwise returns all of its arguments.

```lua
local count = assert(tonumber(input), "expected a number")
```

## error(message: any): never

Raises an error with `message`. `try` and `pcall` catch it; uncaught, it ends the script with a traceback.

## try(f: function, ...: any): (boolean, ...any)

Calls `f` with the remaining arguments. Returns `true` followed by the results of `f`, or `false` and the error message if it raised one.

```lua
local ok, value = try(int, "12x")
if not ok then print("not a number: " .. value) end
```

## pcall(f: function, ...: any): (boolean, ...any)

The same as `try`, under Lua's name.

## xpcall(f: function, handler: (any) -> any, ...: any): (boolean, ...any)

Like `pcall`, but an error is passed through `handler` and its result is returned after `false`. Use `debug.traceback` as the handler to keep the stack.

## type(value: any): string

The basic type of a value: `"nil"`, `"boolean"`, `"number"`, `"string"`, `"table"`, `"function"`, `"buffer"` or `"thread"`. Integers and floats are both `"number"`.

## typeof(value: any): string

Like `type`, but tells numbers apart: integers report `"bigint"` and floating-point numbers `"float"`.

## tostring(value: any): string

Converts a value to a string, calling its `__tostring` metamethod when it has one.

## tonumber(value: any, base: number?): number?

Converts a string to a number, or returns `nil` if it is not one. With `base` (2 to 36), the string is read as an integer in that base.

```lua
print(tonumber("ff", 16)) --> 255
print(tonumber("abc"))    --> nil
```

## int(value: number | string): number

Converts to an exact integer: floats are truncated toward zero and strings must hold a decimal integer of any size. Raises an error otherwise.

## float(value: number | string): number

Converts a number or numeric string to a floating-point number. Raises an error otherwise.

## require(name: string): any

Loads a module once and returns its value. Standard libraries load by name (`"math"`), bundled modules by `"@neyuki/name"`, registry packages by `"@pkg/user/package"` (installed in the project) or `"@user/package"` (installed with `neyuki pkg install -g`), and any other name is looked up from the entry script's directory, then the working directory, as `name.nyk`, `name/init.nyk`, then under `modules/`, `packages/` and `neyuki_packages/` (where `neyuki pkg` keeps packages, entered through `init.nyk` or `main.nyk`). Paths may not leave the project (`..` and absolute paths are rejected). Every name for the same file shares one loaded value.

## pairs(t: table): (function, table, nil)

Returns an iterator over every key and value of `t`. Plain `for k, v in t do` iterates the same way.

## ipairs(t: table): (function, table, number)

Returns an iterator over `t[1]`, `t[2]`, ... stopping at the first `nil`.

## next(t: table, key: any?): (any, any)

The key and value after `key` in `t`'s iteration order, or `nil` at the end. `next(t)` returns the first entry, so `next(t) == nil` tests for an empty table.

Assigning `nil` to the current key while iterating with `next` is allowed: `next(t, k)` still returns the entry after `k`. Adding new keys during the iteration is not.

## select(index: number | string, ...: any): ...any

`select("#", ...)` returns the number of extra arguments; `select(n, ...)` returns the arguments from the `n`th on (negative `n` counts from the end).

## setmetatable(t: table, mt: table?): table

Sets or clears the metatable of `t` and returns `t`. Metatables customize indexing (`__index`, `__newindex`), operators (`__add`, `__sub`, `__mul`, `__div`, `__pow`, `__unm`, `__concat`, `__eq`, `__lt`, `__le`), calls (`__call`), `tostring` (`__tostring`), length (`__len`), iteration (`__pairs`), JSON encoding (`__tojson`), weak references (`__mode`) and finalization (`__gc`).

## getmetatable(t: any): table?

Returns the metatable of `t`, or its `__metatable` field if it has one.

## rawget(t: table, key: any): any

Reads `t[key]` without invoking `__index`.

## rawset(t: table, key: any, value: any): table

Writes `t[key] = value` without invoking `__newindex`.

## rawequal(a: any, b: any): boolean

Compares two values without invoking `__eq`.

## rawlen(value: table | string): number

The length of a table or string without invoking `__len`.

## collectgarbage(option: string?, arg: number?): any

Controls the garbage collector: `"collect"` (the default) runs a full cycle, `"count"` returns the memory in use in KiB, `"step"` runs one increment, `"stop"` and `"restart"` pause and resume automatic collection and `"isrunning"` reports whether it is running.

## warn(...: any): ()

Writes `warning: ` and its arguments to standard error.

## _G: table

The table of global variables.

## _VERSION: string

The running version, such as `"Neyuki 0.2.0"`.
