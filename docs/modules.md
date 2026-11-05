# Neyuki Module System Specification

This document details the architecture, resolution algorithm, caching semantics, and export conventions of the Neyuki module system.

## 1. Module Definition and Execution

A module in Neyuki is a self-contained `.nyk` source file or precompiled `.nykb` bytecode artifact.

### 1.1 Scope and Isolation
- Each module executes within its own private top-level chunk.
- Local variables (`local`, `const`) defined in a module are private to that module chunk and are never accessible from importing files.
- The return value of the module chunk (via `return <value>`) is the exported interface of the module.
- If a module terminates without an explicit return statement, it evaluates to `nil`.

### 1.2 Export Pattern
The standard convention is to export a table containing functions, constants, or types:

```lua
local math_utils = {}

const function math_utils.add(a: int, b: int): int
    return a + b
end

const function math_utils.clamp(val: number, min: number, max: number): number
    if val < min then return min end
    if val > max then return max end
    return val
end

return math_utils
```

### 1.3 Exporting Types
`export type Name = ...` makes a type usable by other files as `module.Name`. It is only allowed at the top level of a file (a nested one is a compile error); a plain `type` stays private to its file.

```lua
-- khione.nyk
export type Options = { name: string?, frame: ((events: {any}, dt: number) -> ())? }
type Internal = number        -- not exported

local khione = {}
function khione.open(options: Options?) end
return khione

-- main.nyk
local khione = require("khione")
local o: khione.Options = { name = "x" }
```

Type declarations are erased at compile time, so they do not change what a module returns and the module table gets no `Options` field. A qualified name such as `khione.Options` is accepted in any annotation but is not checked: the compiler never reads the required file. Editor tooling uses the file's `export type` declarations to resolve it.

The bundled `@neyuki/ui` module exports `Event`, `Window`, `Canvas`, `Image`, `Font`, `Path`, `WindowOptions` and `RunOptions` this way (`ui.Event`, ...). The handle types are loose tables.

---

## 2. Module Resolution (`require`)

Modules are loaded at runtime using the global `require(module_path)` function.

### 2.1 Resolution Hierarchy
When `require(path)` is invoked, the runtime resolves the target in the following order:

1. **Module Cache Check**:
   If the canonical name has already been resolved and executed, the cached exported value is returned immediately without re-executing the module code.

2. **Standard and Bundled Libraries (`@neyuki/`)**:
   Paths matching `@neyuki/<name>` or `<name>` (where `<name>` is one of the built-in libraries: `bit`, `bit32`, `buffer`, `coroutine`, `crypto`, `debug`, `fs`, `hid`, `http`, `io`, `json`, `math`, `net`, `os`, `sql`, `string`, `table`, `task`, `ui`, `utf8`) are served directly from the embedded standard library distribution.

3. **Package Manager Dependencies (`@pkg/`)**:
   Paths starting with `@pkg/<package_name>` resolve against packages installed in the local `neyuki_packages/` directory or global cache configured via `NEYUKI_PATH`.

4. **Relative and Filesystem Paths**:
   Paths ending in `.nyk` or `.nykb`, or relative module identifiers (e.g., `./utils`, `components/parser`, `components.parser`) resolve against the directory of the entry script (the file passed to `neyuki run`, or the entry of a built app), whichever directory the process runs in. Modules requiring other modules use the same base, so `require("kit/input")` names the same file from the entry script and from `app/rail.nyk`. If nothing matches there, the working directory is searched next (inside a built app: the bundle root).

### 2.2 Security and Path Traversal Restrictions
To ensure sandboxing integrity and prevent arbitrary filesystem escape, `require` rejects:
- Null bytes (`\0`).
- Relative parent path traversal (`..`).
- Absolute root paths (`/` or Windows drive prefixes `C:\`).

Path traversal attempts trigger an immediate security exception:
```text
runtime error[E0200]: security error: path traversal forbidden in require: '../sensitive'
```

### 2.3 Importing Every Bundled Module (`--!importall`)
A `--!importall` comment on its own line, before the first line of code (only blank lines and other comments may precede it), binds each bundled `lib/*.nyk` module to a local of the same name at the top of the file, as if the file began with `local fs = require("@neyuki/fs")` and so on.

Only names the file uses without declaring are bound, so unused modules are never loaded, and a file's own `local ui = ...` is left alone. The directive applies per file; other undeclared names remain errors.

---

## 3. Caching and Lifecycle

### 3.1 Idempotent Execution
Modules execute once per VM instance. The returned value is cached in `vm.modules[canonical_name]`: for module files the canonical name is the resolved file's canonical path, so every spelling that reaches the same file (`app/rail`, `./app/rail`, `app/rail.nyk`, `app.rail`) receives the same cached instance. A module that raises an error while loading is not cached; the next `require` runs it again.

### 3.2 Circular Dependencies
If module `A` requires module `B`, and module `B` requires module `A`:
1. While `A` is executing, its entry in `vm.modules` is unresolved: `require("A")` from `B` returns an empty placeholder table instead of running `A` again.
2. If `B` immediately accesses fields on `A` during top-level load, `nil` is observed. Once `A` returns, `require("A")` returns its real value.
3. Modules intended for mutual recursion must defer cross-module function calls until execution time inside function bodies rather than during top-level module initialization.

### 3.3 Dynamic Reloading
In standard execution, modules remain cached for the lifetime of the `VM` instance. Development environments (such as the REPL or interactive runner) can invalidate cache keys directly via `package.loaded[name] = nil`.
