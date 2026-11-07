# Neyuki Versioning and Backward Compatibility Policy

This document defines the versioning scheme, stability guarantees, and breaking change deprecation procedures for the Neyuki programming language, compiler toolchain, bytecode formats, and standard libraries.

## 1. Semantic Versioning

Neyuki strictly adheres to Semantic Versioning 2.0.0 (`MAJOR.MINOR.PATCH`):

- **MAJOR**: Incompatible language syntax alterations, opcode encoding changes, removal of deprecated standard APIs, or fundamental alterations to runtime semantics.
- **MINOR**: Backward-compatible new features (new standard library modules, additional language constructs, optimizer improvements, new tooling capabilities such as `fmt` or `lsp`).
- **PATCH**: Backward-compatible bug fixes, performance optimizations, memory safety improvements, and diagnostic message enhancements.

---

## 2. Stability Tiers

Language features and APIs are organized into three distinct stability tiers:

### Tier 1: Core Language and Runtime (Frozen)
- **Scope**: Syntax grammar defined in `docs/spec.md`, register VM execution model, call stack mechanics, lexical scoping, basic data types (nil, bool, int, float, string, table, function).
- **Guarantee**: Backwards compatible across all minor and patch versions within a major release. Syntax will not change or break existing compliant code.

### Tier 2: Standard Library (`@neyuki/*`)
- **Scope**: Every library documented under `docs/reference/`, which today means `bit`, `buffer`, `coroutine`, `crypto`, `debug`, `fs`, `hid`, `http`, `io`, `json`, `math`, `net`, `os`, `profile`, `sql`, `string`, `table`, `task`, `ui`, `utf8` and `webview`, together with the global functions in `docs/reference/globals.md`. A library gated behind a cargo feature, such as `webview`, carries the same guarantee when the feature is on; `isAvailable` reporting `false` is not a breaking change.
- **Guarantee**: APIs are strictly additive within minor versions. Any deprecated function will be marked with diagnostics warning `W0005` for at least one minor release cycle before potential removal in a major release.
- **Fields and methods**: A documented field and a documented method are equally covered, so a field cannot quietly become a method or the reverse. Note that only a method call can carry the `W0005` warning of the deprecation lifecycle below; a field read cannot, so a field named in `docs/reference/` can only be retired at a major boundary.

### Tier 3: Experimental and Preview Tooling
- **Scope**: Features designated with `--experimental` flags, unstable LSP extensions, and internal development tools.
- **Guarantee**: May be modified or refined based on community feedback without triggering a major version increment.

---

## 3. Bytecode Stability Policy

Bytecode artifacts (`.nykb`) carry an explicit version byte in the 12-byte header (`BYTECODE_VERSION`):

1. **Current Version**: Bytecode Version 4 (`0x04`).
2. **Forward Compatibility**: The VM strictly rejects bytecode with a version byte greater than its supported `BYTECODE_VERSION` with a clean diagnostic:
   ```text
   bytecode was compiled by a newer Neyuki (format X, this one reads format 6); recompile the source with `neyuki compile`
   ```
   Older bytecode is rejected the same way ("an older Neyuki"). When `require` loads such a `.nykb` and its `.nyk` source sits next to it, the source is compiled instead.
3. **Backward Compatibility**: Production releases of the VM must provide decoding pathways for the immediately preceding bytecode version or provide an automated bytecode migrator in `neyuki pkg upgrade-bytecode`.
4. **Bytecode Freeze**: Once Version 1.0 of Neyuki is tagged, Bytecode Version 4 enters Long-Term Support (LTS) status.

---

## 4. Deprecation Lifecycle

When an API or language pattern is targeted for deprecation:
1. **Phase 1 (Warning)**: The compiler and linter emit a diagnostic warning (`W0005`) with a clear explanation and recommendation for the replacement API (`= help: use new_api instead`). Code continues to execute identically.
2. **Phase 2 (Deprecation Window)**: The feature remains operational for at least one full minor release cycle (minimum 6 months).
3. **Phase 3 (Removal)**: The feature is retired upon the subsequent major version boundary (`v2.0`).
