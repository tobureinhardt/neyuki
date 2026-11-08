# Neyuki Development Roadmap

This document outlines the architectural milestones and engineering trajectory for the Neyuki language, compiler, runtime, and ecosystem tools towards the 1.0 release.

---

## Milestone 1: Runtime Robustness and Type Precision (v0.1.1 - Current)
- [x] Raise VM recursion ceiling from hardcoded 512 frames to 16,384 frames (`DEFAULT_MAX_CALL_DEPTH`), configurable via `NEYUKI_MAX_CALL_DEPTH`.
- [x] Full tokenizer and recursive descent parser rewrite for `TypeExpr` supporting complex nested records (e.g. `{f: (int, int) -> int}`), function signatures, unions, and optional types.
- [x] Runtime type enforcement engine (`Value::matches_type_expr`) with global builtins `checktype` and `asserttype`.
- [x] Modernized compiler and runtime diagnostic formatting conforming to compiler industry standards (error codes `E0001` - `E0201`, caret underlines, gutter line numbers, source snippets, and structured stack backtraces).
- [x] Formal Language Specification (`docs/spec.md`) and Bytecode Version 4 specification (`docs/bytecode_v4.md`).
- [x] User module documentation (`docs/modules.md`) and Semantic Versioning compatibility policy (`docs/compatibility.md`).

---

## Milestone 2: Algebraic Data Modeling and Pattern Matching (v0.2.0)
- [ ] Algebraic Data Types (`enum` / sum types):
  - [ ] Lexer and parser support for `enum <Name> ... end` declarations with payload-bearing variants.
  - [ ] Runtime representation of enum instances with immutable tags and constructor functions.
- [ ] Pattern matching (`match` construct):
  - [ ] Pattern grammar: literal matching, variant destructuring, wildcard `_`, variable binding.
  - [ ] Exhaustiveness and reachability analysis in semantic checker.
  - [ ] Bytecode lowering to efficient jump tables and conditional branch cascades.

---

## Milestone 3: Developer Tooling Completeness (v0.3.0)
- [ ] Code Formatter (`neyuki fmt`):
  - [ ] AST and token-based code formatter preserving indentation, operators, and doc comments.
  - [ ] CLI flags: `--check` for CI verification and `--write` for in-place source formatting.
- [ ] Language Server Protocol (`neyuki lsp`):
  - [ ] Stdio JSON-RPC 2.0 language server implementation.
  - [ ] Diagnostics push on document open/edit.
  - [ ] Hover documentation for variables, functions, and standard libraries.
  - [ ] Goto definition and document symbols.

---

## Milestone 4: Self-Hosted Compiler and Bootstrap (v0.4.0)
- [ ] Compiler written in Neyuki (`bootstrap/compiler.nyk`):
  - [ ] Lexer and parser written in pure Neyuki leveraging deep recursion support.
  - [ ] AST representation using Neyuki `enum` / sum types.
  - [ ] IR construction and constant folder.
  - [ ] Bytecode v4 emitter.
- [ ] Two-stage bootstrap verification:
  - [ ] Stage 1: Rust compiler compiles `bootstrap/compiler.nyk` to `compiler.nykb`.
  - [ ] Stage 2: `compiler.nykb` compiles `bootstrap/compiler.nyk` to `compiler_stage2.nykb`.
  - [ ] Binary bit-for-bit equivalence check between Stage 1 and Stage 2 bytecode.

---

## Milestone: Desktop app platform
- [x] `neyuki build`: package an entry script and an asset folder into a standalone executable with an embedded read-only file system.
- [x] `[app]` manifest section: name, identifier, icon, publisher, copyright; no console window by default on Windows.
- [x] `os.app()`: read the app's name, version and identifier at runtime.
- [x] Per-user storage in `@neyuki/os`: `dataDir`, `configDir` and `cacheDir` following each platform's conventions, and `protect`/`unprotect` for secrets (DPAPI on Windows, an OS keychain key elsewhere).
- [x] `@neyuki/ui` window chrome and input: frameless windows with app-drawn title bars (`startDrag`, `startResize`, minimize/maximize/restore, fullscreen, minimum size, icon, drop shadow), pointer cursors, IME text input (`textinput` and `ime` events), click counts for double-click, window state, file drop and DPI change events, `ui.clipboard` and `ui.openUrl`.
- [x] `@neyuki/ui` text: font fallback chains for apps that ship their own CJK fonts (`ui.setFallbackFonts`, per-call `fallback`, `ui.loadFont` from a buffer, `ui.systemFont`), plus ellipsis, clip and wrap overflow, alignment, line height and letter spacing.
- [x] OS media integration for `@neyuki/ui`: now-playing metadata in the system media controls (`ui.mediaSession`) and global media keys as `mediakey` events.
- [x] `@neyuki/ui` image decoding: PNG, JPEG, WebP, GIF and BMP detected by magic bytes, from files (`ui.loadImage`) or memory (`ui.decodeImage`).
  - [x] Animated GIF/WebP/PNG frames with browser-style delay clamping, `ui.animation` playback helper and `image:resized`.
  - [x] `drawImage` options `frame`, `opacity`, `radius` and `fit` (`fill`, `contain`, `cover`) with cached smooth resampling.
  - [x] Pixel-count and decoded-memory limits.
- [x] `@neyuki/ui` canvas and effects, CPU only:
  - [x] `ui.canvas` off-screen surfaces with the full drawing API, drawable like an image, and `ui.path` with `fillPath`, `strokePath` and `getPixel`.
  - [x] Rectangle and path clipping (`pushClip`, `pushClipPath`), group opacity (`pushOpacity`), linear/radial gradients and box shadows on fills.
  - [x] Gaussian blur: `blurRegion` backdrop blur with rounded masks and `Image:blur`.
  - [x] CPU render performance: `present({ dirty })` partial present, `ui.run({ fps = "auto" })` idle throttling, `ui.stats()` frame timing and an opaque glyph fast path. A GPU backend *behind* these drawing calls is still left out: `@neyuki/gpu` sits beside them as its own module, so `ui` keeps rendering on the CPU with no driver.
- [x] Async I/O: `http.request`, `os.run` and `fs.read` suspend only the calling task and run on background threads; `ui.run` pumps the completions every frame so windows stay responsive.
- [x] `@neyuki/net`: named pipes, Unix sockets, TCP sockets and a WebSocket client with reader threads and buffered reads that suspend only the calling task, plus `os.spawn` for child processes with streamed stdin, stdout and stderr.
- [x] `@neyuki/webview`: web pages through the system web engine (WebView2, WKWebView, WebKitGTK) in the `ui` event loop, with a custom asset protocol and page/script messaging. Embedded web content is one route to GPU-accelerated drawing; `@neyuki/gpu` is the other.
- [x] `@neyuki/gpu`: GPU rendering on a `ui` window through `wgpu` (Direct3D 12, Metal, Vulkan), behind the `gpu` cargo feature, which is off in stock builds because it adds several MiB to the executable.
  - [x] WGSL shaders with errors that carry a line and a column, pipelines, and vertex, index and uniform data straight from `buffer` values.
  - [x] Textures from `@neyuki/ui` images and canvases, with `texture:update` for a canvas that is redrawn: 2D drawing and text over 3D without leaving `ui`.
  - [x] A depth buffer on every frame, back-face culling, premultiplied blending, and `frame:read()` so a frame can be checked with no display and no GPU.
- [ ] Tray icon with a menu whose items arrive as events.
- [ ] Single-instance apps: a second launch signals and focuses the first.
- [ ] Auto-update support.

---

## Milestone 5: Version 1.0 Production Freeze (v1.0.0)
- [ ] Language syntax freeze: no further breaking grammar modifications.
- [ ] Bytecode Version 4 freeze: guarantee long-term stability and binary portability across all 1.x releases.
- [ ] Performance benchmarking suite validation against standard scripting workloads.
- [ ] Full test coverage of standard libraries, security sandboxing, and memory leak regression tests.
