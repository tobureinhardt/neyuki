# Library reference

Every function Neyuki ships with. Each page starts with an overview and an example, followed by one section per function, written as `name(parameter: type, ...): result`. A `?` marks an optional parameter.

## Always available

These are globals; no `require` is needed.

| Page | What it covers |
| --- | --- |
| [Globals](globals.md) | `print`, `type`, `tostring`, `try`, `require`, `pairs`, metatables and the rest of the base functions |
| [math](math.md) | Rounding, trigonometry, `clamp`, `lerp`, random numbers and noise |
| [string](string.md) | Searching, slicing, splitting, trimming, patterns and `format` |
| [table](table.md) | Inserting, removing, sorting, joining, copying and freezing |
| [utf8](utf8.md) | Working with text by character instead of byte |
| [json](json.md) | Encoding and decoding JSON |
| [os](os.md) | Time and dates, environment variables, arguments, running and spawning programs, sleeping and exiting |
| [buffer](buffer.md) | Mutable blocks of bytes for binary formats |
| [bit](bit.md) | 32-bit bitwise operations compatible with Lua's `bit32` |
| [coroutine](coroutine.md) | Functions that pause and resume |
| [task](task.md) | Running tasks side by side: `spawn`, `defer`, `delay`, `wait` and `cancel` |
| [debug](debug.md) | Stack traces, introspection and hooks |

## Bundled modules

Load these with `require("@neyuki/name")`.

| Page | What it covers |
| --- | --- |
| [@neyuki/fs](fs.md) | Files, directories and paths |
| [@neyuki/io](io.md) | Reading from and writing to the terminal |
| [@neyuki/log](log.md) | Structured logging with levels, colored console output, JSON Lines and rotating file sinks |
| [@neyuki/http](http.md) | HTTP client and server, router, middleware, static files, cookies, sessions, multipart and port forwarding |
| [@neyuki/net](net.md) | Pipes, Unix sockets, TCP sockets, WebSocket clients and servers, UDP sockets |
| [@neyuki/sql](sql.md) | PostgreSQL, MySQL, MariaDB and SQLite |
| [@neyuki/crypto](crypto.md) | Hashes, passwords, encryption, signatures, secure random values |
| [@neyuki/toml](toml.md) | Reading and writing TOML configuration files |
| [@neyuki/yaml](yaml.md) | Reading and writing YAML 1.2 configuration files and multi-document streams |
| [@neyuki/csv](csv.md) | Reading and writing CSV, with or without a header line |
| [@neyuki/html](html.md) | Parsing HTML like a browser, CSS selectors, text and rendering |
| [@neyuki/ui](ui.md) | Windows, drawing, input events and animation |
| [@neyuki/webview](webview.md) | Web pages in a window: load a URL, HTML or app files, and exchange messages with the page |
| [@neyuki/gpu](gpu.md) | GPU rendering on a `ui` window, behind the `gpu` cargo feature: textured, depth-tested meshes and canvases as textures |
| [@neyuki/audio](audio.md) | Playing sound files, tones and samples: volume, panning, speed, looping, fades and seeking |
| [@neyuki/hid](hid.md) | Keyboard and mouse automation, screen size, clipboard |
| [@neyuki/profile](profile.md) | Finding slow functions |
| [@neyuki/test](test.md) | Tests with `describe`/`it`, hooks, expectations with diffs, snapshots and a summary line |

## Official packages

Some libraries live in packages rather than in the executable, so a build only carries what a project uses. Install one with `neyuki pkg add @nyk/name` and load it with `require("@pkg/nyk/name")`. Each package's page on the registry is its reference.

| Package | What it covers |
| --- | --- |
| [@nyk/markdown](https://neyuki.cc/packages/@nyk/markdown) | CommonMark and GFM parsing, plain-table ASTs, frontmatter and safe HTML rendering (formerly `@neyuki/markdown`) |
