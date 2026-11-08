# Neyuki
*/neˈjuːki/ — `neh-yoo-kee`. From 根雪, the base layer of snow that settles early and stays all winter.*

### NEYUKI IS STILL IN BETA
Please do not use this in professional use, but only for hobby projects. It is unknown if we will get so far for Neyuki to become usable for production use-cases. If you find any bugs please report them in the Issues tab! We are actively looking at them!

Neyuki is a small scripting language for everyday programs: automation, command-line tools, little servers, quick windows and games. It reads like Lua, ships as one executable with no dependencies, and comes with the libraries those jobs need.

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
- **Batteries for daily work.** Files and paths, JSON, dates, running programs, HTTP client and server, PostgreSQL and MySQL, hashing and encryption, windows with 2D drawing and animation, and keyboard and mouse automation.
- **One executable.** No runtime to install; on Linux, TLS uses the system's OpenSSL. Scripts can also be compiled to verified bytecode.

## Install

With a recent stable Rust and a C compiler (on Linux also `pkg-config` and the OpenSSL headers; see [CONTRIBUTING.md](CONTRIBUTING.md#building) for each OS):

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

[@neyuki/gpu](docs/reference/gpu.md) is the one module a stock build leaves out, because `wgpu` adds several MiB to the executable. Add `--features gpu` to either command for a build that has it — `cargo install neyuki --features gpu` — or take the `neyuki-gpu` that sits beside `neyuki` in every release archive. The two are otherwise the same build, and `gpu.isAvailable()` tells a script which one it is running on.

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

MIT. See [LICENSE](LICENSE).
