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

Arguments after the script path reach the script through `os.args()`. A script that fails prints the error with a stack trace and exits with status 1; `os.exit(code)` chooses the status explicitly. When the main chunk returns while tasks from the [`task` library](reference/task.md) are still waiting, the script keeps running until none is left; if any task raised an error, it then exits with status 1. The REPL keeps running tasks while it waits for input, and leaves the ones still waiting when its input ends. A task that runs `ui.run` or polls with `ui.poll(timeout)` counts as waiting until its windows are closed, so the script stays alive for it, and the window is served whenever the main program calls `task.wait` or has finished (a main program that computes without waiting freezes the window; see [`task`](reference/task.md#windows-in-a-task)).

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

The modules of the project are read as well. A `require` string is resolved the way the runtime resolves it (the search patterns of [Modules and packages](#modules-and-packages), relative to the file that requires it and then to the project root, the directory of the nearest `neyuki.toml`), the file is parsed, and nothing in it is ever run. The unsaved text of an open document is used instead of the file, and a file that does not parse keeps the last shape that did. What the server works out from a module is what it returns:

- `local m = {}` ... `return m` with `function m.name(...)`, `function m:name(...)`, `m.name = value` and the fields of the constructor, or `return { a = a, b = function() end }`, or `return require("other")`; for each member the parameter and return annotations and the `--` comment lines directly above it, which show up in completion, hover and signature help. One level of nested tables is followed.
- `type` and `export type` declarations.

Then `local k = require("khione")` followed by `k.` lists the members of the module (and `k:` its methods); `export type` names complete in annotations after `k.`, and a bare `Name`, a built-in type or a module that exports types completes there too. `local o: k.Options = { ` lists the fields of the record in the table constructor, and `o.` lists them for a value annotated with a record alias, local or exported, including a function parameter such as `options` in `function khione.new(options: Options)` and the elements of `events: {ui.Event}` in `for _, e in events do`. Hover shows the text of an alias, and go to definition leads from `k.new`, `k.Options` and `k` into the module and from the string of `require("khione")` to its file (the bundled modules have no file). `require("` offers the bundled modules, the dependencies of `neyuki.toml` and the `.nyk` files of the project (a bounded walk that skips `target`, `node_modules`, `neyuki_packages` and hidden directories; `./` narrows the list to files).

Anything that cannot be worked out statically is left out rather than guessed: members added inside functions or conditions, tables built in loops, computed field names, the members of a value returned by a call (apart from a return annotation that names a record), types that are not one name or a record (unions, functions) and the structure of a record that is only written inline. Methods of a type that is not a library type are not listed after `x:`. A document that has never parsed has no types of its own to complete; type completion for a document needs one text that parsed since it was opened.

Diagnostics are the checks of `neyuki lint`, published with the codes and severities of the linter (error, warning, information, hint), the help text under the message, and `W0001`, `W0002` and `W0004` tagged as unnecessary code so that clients fade it out. References, rename and document highlights work on the names of one document with the scopes of the parser (rename is refused for globals, for fields, and for a name with a use the server cannot place), folding ranges cover blocks, multi-line tables and runs of comments, and `workspace/symbol` searches the functions and types of the open documents.

## Modules and packages

`require(name)` loads a module once and returns what it returned:

- the standard libraries by name: `require("json")`, also available as globals;
- bundled modules by `@neyuki/` name: `require("@neyuki/http")`;
- registry packages: `require("@pkg/user/package")` from the project's `neyuki_packages/` (outside a project, from the global packages), and `require("@user/package")` from the global packages that `neyuki pkg install -g` installs. Without a module path the package's `entry` file loads; `require("@pkg/user/package/lib/util")` loads `lib/util.nyk` inside it;
- anything else as a file from the entry script's directory, then the working directory: `name.nyk`, `name/init.nyk`, then the same under `modules/`, `packages/` and `neyuki_packages/` (where `neyuki pkg` keeps packages, entered through `init.nyk` or `main.nyk`). Every name for the same file (`ui/button`, `ui.button`, `ui/button.nyk`) loads it once.

Paths in `require` may not contain `..` or be absolute.

### The package registry

Packages are published to [neyuki.cc/packages](https://neyuki.cc/packages) under their author's username, as `@user/package`. A published version never changes; its author can only yank it, which hides it from new installs while lockfiles that already name it keep working.

```text
neyuki pkg init [name]              create neyuki.toml (entry = "init.nyk")
neyuki pkg add <pkg>[@req] ...      add dependencies and install them
neyuki pkg remove <pkg> ...         remove dependencies
neyuki pkg install [--frozen]       install what neyuki.toml and neyuki.lock list
neyuki pkg update                   move every dependency to its newest matching version
neyuki pkg list                     show installed packages
neyuki pkg install -g <pkg> ...     install for your user, for require("@user/package")
neyuki pkg remove -g <pkg> | list -g
neyuki pkg search <words> | info <pkg>
neyuki pkg login [token] | logout | whoami
neyuki pkg publish [--dry-run]      upload this project as a new version
neyuki pkg yank <version> [--undo]  hide a version from new installs
```

Dependencies go in `neyuki.toml` with quoted keys, and requirements use `^`, `~`, `=`, `>`, `>=`, `<`, `<=`, `*` or a comma-separated range such as `">=1.2, <2"`. A bare version means `^`. A prerelease only matches a requirement that names a prerelease of the same version.

```toml
[package]
name = "@yuki/router"
version = "1.0.0"
description = "Small URL router for @neyuki/http"
entry = "init.nyk"
license = "MIT"
repository = "https://github.com/yuki/router"
keywords = ["http", "router"]

[dependencies]
"@yuki/colors" = "^2.1.0"
```

Every package installs once, flat, into `neyuki_packages/<user>/<package>/`, so all requirements on a package must agree on one version. `neyuki.lock` records the exact version and SHA-256 checksum of each one. Installs keep the locked versions while they still fit the manifest, download only what changed, refuse an archive whose checksum differs from the lockfile, and remove packages that are no longer needed. `--frozen` fails instead of changing the lockfile, for CI.

To publish, create an account at neyuki.cc, confirm your email, create an API token on the account page and run `neyuki pkg login`. `publish` uploads every file in the project except hidden files, `neyuki_packages/`, `target/`, `node_modules/`, `neyuki.lock` and what `.neyukiignore` lists (one glob per line), up to 10 MB compressed; a `README.md` becomes the package page.

`NEYUKI_REGISTRY` points `neyuki pkg` at another registry, `NEYUKI_TOKEN` supplies a token (for CI), `NEYUKI_PATH` moves the global packages (default `~/.neyuki/packages`) and `NEYUKI_HOME` moves `~/.neyuki`, where `neyuki pkg login` keeps its token.

Directories placed by hand in `neyuki_packages/<name>/` still load with `require("<name>")`.

## Memory and limits

Memory is managed by reference counting with a cycle collector, so most values are freed the moment they become unreachable and cycles are reclaimed in the background. `collectgarbage` controls it, `__gc` runs finalizers and `__mode` makes tables weak.

A few limits keep one script from exhausting the machine: integers are capped at 2^20 bits, `string.rep` at 100 MiB, `json.encode`, `toml.encode`, `csv.encode` and `html.render` at 64 MiB of output, nesting at 120 levels, and a table passed to a native library function at a million values.

### Sandbox permissions and limits

Running with `--sandbox` denies host capabilities by default; individual `--allow-*` flags grant scoped access:

- **`--allow-read` / `--allow-write`**: Existing directory prefixes and symlinks are canonicalized before prefix checks so symlinks inside an allow-listed directory cannot escape to paths outside it.
- **`--allow-run`**: When restricted to an allow-list, shell command strings (`os.run("...")` / `os.spawn("...")` routed through `sh -c` or `cmd /C`) are rejected to prevent shell metacharacter injection; scripts must pass the array form (`{ program, arg1, ... }`). Allow-list entries containing paths compare canonicalized full paths rather than basenames. Subprocesses spawned under `--allow-run` still inherit the host environment (plus any `options.env` overrides); `--allow-env` only gates `os.getenv`.
- **`--allow-net`**: Hostname allow-lists are checked against the unresolved hostname/URL before DNS resolution (an allow-listed hostname resolves to whatever IP address DNS returns). Non-host targets must match explicit rules when restricted: `unix:<path>` (or an explicit socket path) for Unix domain sockets, `pipe:<name>` (or pipe name) for Windows named pipes, and the bind address (such as `0.0.0.0:8080`) for local listeners.
- **`--allow-ffi`**: Controls native library loading (`ffi.open`, `ffi.loadExtension`); any non-denied FFI policy permits in-process `ffi.alloc` / `ffi.free` / `ffi.read` / `ffi.write`, and `ffi.alloc` counts toward `--max-memory`.
- **`--timeout` / `--max-instructions` / `--max-memory`**: Execution deadlines and instruction budgets are **best-effort**, polled at bytecode dispatch / trap intervals and backward jumps, and do not preempt inside long-running native calls (such as `string.rep` or blocking network/SQL calls).

## Platform notes

The runtime is a single executable with nothing else to install, apart from OpenSSL (`libssl`) on Linux. `@neyuki/ui` and `@neyuki/hid` need a desktop session (on Linux, X11 or Wayland); everything else runs headless. On Linux, `ui.mediaSession` publishes over MPRIS and so needs a D-Bus session bus. TLS for `@neyuki/http` and `@neyuki/sql` uses the operating system's own library (SChannel, Secure Transport or OpenSSL).
