# Neyuki
*/neˈjuːki/ — `neh-yoo-kee`. From 根雪, the base layer of snow that settles early and stays all winter.*

### NEYUKI IS STILL IN BETA
Please do not use this in professional use, but only for hobby projects. It is unknown if we will get so far for Neyuki to become usable for production use-cases. If you find any bugs please report them in the Issues tab! We are actively looking at them!

Neyuki is a small scripting language for everyday programs: automation, command-line tools, little servers, quick windows and games. It reads like Lua and runs from one small executable with no dependencies. What most scripts need is built in. The heavier libraries (windows and drawing, the GPU, web views, databases, sound) are official packages a project adds when it uses them, so neither the runtime nor an app built with it carries code it does not need.

```lua
local fs = require("@neyuki/fs")

-- count the lines in every Neyuki file under src/
local total = 0
for _, path in fs.walk("src") do
    if fs.extension(path) == ".nyk" then
        local lines = 0
        for _ in fs.lines(fs.join("src", path)) do lines += 1 end
        print(`{path}: {lines}`)
        total += lines
    end
end
print(`total: {total}`)
```

## What it gives you

- **A familiar language.** Lua's tables, closures, metatables and coroutines, a Luau-style `task` scheduler, plus `!=`, `??`, `+=`, `continue`, `` `{interpolation}` `` and `function T:method()`.
- **Exact integers.** Integers never overflow or lose precision; `2 ^ 100` is exact.
- **Optional types, checked before running.** `local n: number = "x"` is reported with its line before anything executes, and `neyuki lint` shows the same diagnostics in your editor.
- **Tests built in.** `@neyuki/test` gives `describe`/`it`, hooks, expectations that show a diff, snapshots and a summary line; `neyuki test` runs them.
- **Batteries for daily work.** Files and paths, JSON, TOML, YAML and CSV, dates, running programs, structured logging, HTTP client and server, and hashing and encryption.
- **Official packages for the rest.** Libraries a project may not need install on demand, such as `neyuki pkg add @nyk/html` for HTML. `@nyk/markdown`, `@nyk/sql` (PostgreSQL, MySQL and SQLite), `@nyk/ui` (windows with 2D drawing and animation), `@nyk/gpu` (GPU rendering on them), `@nyk/webview` (web pages in a window), `@nyk/hid` (keyboard and mouse automation) and `@nyk/audio` (sound) work the same way. See [the library reference](docs/reference/README.md#official-packages).
- **One executable.** No runtime to install; on Linux, TLS uses the system's OpenSSL, and nothing else is linked. Packages with native code ship a prebuilt library for every platform, and `neyuki build` bundles the one the app's platform needs. Scripts can also be compiled to verified bytecode.

## Install

On Linux and macOS:

```bash
curl -fsSL https://neyuki.cc/install.sh | sh
```

On Windows, in PowerShell:

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://neyuki.cc/install.ps1 | iex"
```

Both download the latest [release](https://github.com/neyuki-lang/neyuki/releases) for your platform, check it against the release's `SHA256SUMS` (releases after v0.12.0), put `neyuki` in `~/.neyuki/bin` (`%USERPROFILE%\.neyuki\bin`; `$NEYUKI_HOME/bin` when that is set) and add that directory to your `PATH`. Run the command again to upgrade. To pick a release, pass `sh -s -- --version v0.12.0` to the shell script or set `NEYUKI_VERSION` for either; `--no-modify-path` (or `NEYUKI_NO_MODIFY_PATH=1`) leaves your `PATH` alone. The scripts are [`install/`](install) in this repository.

To build it yourself instead, with a recent stable Rust and a C compiler (on Linux also `pkg-config` and the OpenSSL headers; see [CONTRIBUTING.md](CONTRIBUTING.md#building) for each OS):

```bash
cargo install neyuki
```

Or from a clone, which is what you want to work on Neyuki itself:

```bash
git clone https://github.com/neyuki-lang/neyuki
cd neyuki
cargo build --release
```

The executable is `target/release/neyuki` (`neyuki.exe` on Windows); put it on your `PATH`.

## Use

```bash
neyuki hello.nyk            # run a script
neyuki lint hello.nyk       # check it without running
neyuki repl                 # try things interactively
neyuki help                 # every command
```

## Documentation

- [The language](docs/language.md): syntax, values, tables, functions, objects and modules
- [Library reference](docs/reference/README.md): every function, with examples
- [Tools and runtime](docs/runtime.md): the CLI, lint, bytecode, packages and limits
- [Examples](examples/)

The VS Code extension in the sibling `editor-extensions/vscode` folder adds highlighting, completion with documentation, live diagnostics, snippets and debugging.

## Project layout

```text
src/
├── main.rs            the command-line interface
├── parser/, lexer.rs  tokenizer and parser
├── ast/               syntax tree, spans and visitors
├── sema/              scopes, const and type checks
├── lint/              `neyuki lint`
├── compiler/          constant folding, SSA IR, optimization passes, register allocation, codegen
├── bytecode/          instruction format, serializer, verifier, disassembler
├── vm/                register VM, garbage collector
│   └── libs/          standard libraries and the natives behind lib/*.nyk
├── repl.rs, dap.rs    REPL and debug adapter
└── diagnostics/
lib/                   bundled @neyuki/* modules written in Neyuki
include/neyuki_ext.h   the C ABI for native extensions
crates/neyuki-ext/     the Rust SDK for native extensions and packages
tests/                 .nyk test scripts (`neyuki test`)
bench/, fuzz/          `neyuki bench` and `neyuki fuzz`
docs/
```

`cargo test` runs the Rust tests and every script in `tests/`.

## Community

- Discord: https://discord.gg/mvCB3KnAT4
- GitHub: https://github.com/neyuki-lang

Contributions are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). To report a security vulnerability, follow [SECURITY.md](SECURITY.md) instead of opening a public issue.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.
