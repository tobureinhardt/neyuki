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
| [@neyuki/http](http.md) | HTTP client and server, port forwarding |
| [@neyuki/net](net.md) | Pipes, Unix sockets, TCP sockets and WebSocket clients |
| [@neyuki/sql](sql.md) | PostgreSQL, MySQL and MariaDB |
| [@neyuki/crypto](crypto.md) | Hashes, passwords, encryption, signatures, secure random values |
| [@neyuki/ui](ui.md) | Windows, drawing, input events and animation |
| [@neyuki/webview](webview.md) | Web pages in a window: load a URL, HTML or app files, and exchange messages with the page |
| [@neyuki/gpu](gpu.md) | GPU rendering on a `ui` window, behind the `gpu` cargo feature: textured, depth-tested meshes and canvases as textures |
| [@neyuki/hid](hid.md) | Keyboard and mouse automation, screen size, clipboard |
| [@neyuki/profile](profile.md) | Finding slow functions |
