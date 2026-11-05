# Tools and runtime

## The `neyuki` command

```text
neyuki script.nyk [args...]       run a script (short for `neyuki run`)
neyuki run <file> [args...]       run a .nyk script or .nykb bytecode file
neyuki lint [--json] [--stdin] <file>
neyuki compile <file> [out]       compile to bytecode (default: <file>b)
neyuki repl                       interactive session
neyuki test                       run every .nyk file in tests/
neyuki build [project]            build a standalone app executable
neyuki pkg <command>              manage the project manifest and packages
neyuki dap                        Debug Adapter Protocol server on stdio
neyuki lsp                        Language Server Protocol server on stdio
neyuki disasm <file>              print the bytecode the compiler emits
neyuki dump-ir <file>             print the IR before and after optimization
neyuki help | --version
```

Arguments after the script path reach the script through `os.args()`. A script that fails prints the error with a stack trace and exits with status 1; `os.exit(code)` chooses the status explicitly. When the main chunk returns while tasks from the [`task` library](reference/task.md) are still waiting, the script keeps running until none is left; if any task raised an error, it then exits with status 1. The REPL keeps running tasks while it waits for input, and leaves the ones still waiting when its input ends.

### Checking without running

`neyuki lint` reports what the checker finds, the same checks `run` performs before executing: syntax errors, undefined names, assignments to `const` variables, values that do not match their type annotations, unused variables and parameters, shadowed names and unreachable code. Errors stop a script from running; warnings do not.

```text
warning[W0001]: unused variable 'total'
 --> report.nyk:4:7
  |
4 | local total = 0
  |       ^^^^^ variable declared here but never used
  |
  = help: if this is intentional, prefix with an underscore: '_total'
```

`--json` prints the diagnostics as a JSON array of `{ severity, code, message, line, column, endLine, endColumn, help? }` (1-based, end exclusive) for editors and CI, and `--stdin` reads the source from standard input, so an editor can check a buffer that has not been saved.

### Bytecode

`neyuki compile app.nyk` writes `app.nykb`, which `neyuki run` loads without parsing or checking. Bytecode is verified before it runs, so a damaged or hand-made file is rejected instead of crashing the VM.

The bytecode format changes between Neyuki versions. A `.nykb` written by another version is rejected with an error naming both formats; recompile it with `neyuki compile`. When `require` meets such a file and the `.nyk` source sits next to it, it compiles the source instead.

### Building apps

`neyuki build [project]` writes a standalone executable that holds the entry script, the modules it needs and an asset folder (default `assets`) in an embedded read-only file system, so `fs.read("assets/logo.png")` and `require` resolve inside the bundle. Options: `-o`/`--out`, `--name`, `--version`, `--icon`, `--asset` (repeatable), `--console` (keep the Windows console window) and `--runtime`. The `[app]` section of `neyuki.toml` sets `name`, `identifier`, `icon`, `assets`, `publisher` and `copyright`. A built app reads its own details with [`os.app()`](reference/os.md).

### Debugging

`neyuki dap` speaks the Debug Adapter Protocol on standard input and output, which the VS Code extension uses for breakpoints, stepping, the call stack and variables. Scripts can also inspect themselves with the [debug library](reference/debug.md) and time functions with [@neyuki/profile](reference/profile.md).

### Editor support

`neyuki lsp` speaks the Language Server Protocol on standard input and output, so any editor with an LSP client gets diagnostics (the same checks as `neyuki lint`), formatting (as `neyuki fmt`), hover, go to definition, the document outline, completion and signature help.

What the server knows about the standard library is the [library reference](reference/README.md), compiled into the `neyuki` executable. Completion after `ns.` (for the standard libraries and every bundled module, also through `local x = require("@neyuki/x")` and the names `--!importall` adds), methods after `x:` when the type of `x` is known from an annotation or from a call such as `ui.open(...)`, module names inside `require("`, hover and signature help therefore always match the installed version, and upgrading Neyuki upgrades the editor features with no editor update. The server reports its name and version in the `serverInfo` of the `initialize` result. Snippet insertion is used only for clients that advertise `snippetSupport`.

## Modules and packages

`require(name)` loads a module once and returns what it returned:

- the standard libraries by name: `require("json")`, also available as globals;
- bundled modules by `@neyuki/` name: `require("@neyuki/http")`;
- anything else as a file from the entry script's directory, then the working directory: `name.nyk`, `name/init.nyk`, then the same under `modules/`, `packages/` and `neyuki_packages/` (where `neyuki pkg` keeps packages, entered through `init.nyk` or `main.nyk`). Every name for the same file (`ui/button`, `ui.button`, `ui/button.nyk`) loads it once.

Paths in `require` may not contain `..` or be absolute.

`neyuki pkg init` creates a `neyuki.toml` manifest, `pkg add <name> [version]` records a dependency, `pkg check` validates the manifest and `pkg list` shows what is in `neyuki_packages/`. Packages are placed in that directory by hand for now; there is no registry to download from yet.

## Memory and limits

Memory is managed by reference counting with a cycle collector, so most values are freed the moment they become unreachable and cycles are reclaimed in the background. `collectgarbage` controls it, `__gc` runs finalizers and `__mode` makes tables weak.

A few limits keep one script from exhausting the machine: integers are capped at 2^20 bits, `string.rep` at 100 MiB, `json.encode` at 64 MiB of output, nesting at 120 levels, and a table passed to a native library function at a million values.

## Platform notes

The runtime is a single executable with nothing else to install, apart from OpenSSL (`libssl`) on Linux. `@neyuki/ui` and `@neyuki/hid` need a desktop session (on Linux, X11 or Wayland); everything else runs headless. On Linux, `ui.mediaSession` publishes over MPRIS and so needs a D-Bus session bus. TLS for `@neyuki/http` and `@neyuki/sql` uses the operating system's own library (SChannel, Secure Transport or OpenSSL).
