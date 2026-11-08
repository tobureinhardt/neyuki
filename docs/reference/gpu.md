# @neyuki/gpu

GPU rendering through the platform graphics API: Direct3D 12 on Windows, Metal on macOS, Vulkan (or OpenGL ES) elsewhere, behind `wgpu`.

This is a separate path from [@neyuki/ui](ui.md), not a backend for it. `ui` keeps rendering on the CPU with `tiny-skia`, with the same guarantees as before: no GPU, no driver, works on a machine with no display driver at all. What `ui` cannot do is depth testing — it has no pixel write and no raw upload path, so everything is painter's-algorithm sorting and interpenetrating geometry always sorts wrong. That capability, plus textures, is what this module exists for.

A window opened with `gpu = true` is handed to this module. It has **no CPU back buffer**: every `ui` drawing call on it, and `window:present()`, raises an error, because softbuffer and wgpu cannot both present to one window. Everything else about the window is unchanged — `ui.poll`, `ui.run`, events, resizing, the title bar and the rest of `ui` work exactly as they do for a CPU window.

Shaders are WGSL. A pipeline's bindings are read from the shader itself — whatever it declares as `@group(0) @binding(n)` — so there is no second place to keep in step, and a pipeline's vertex layout is the one thing a script states twice. Vertex, index and uniform data come from `buffer` values, which is what makes uploading a mesh one copy with no table conversion.

Text and two-dimensional drawing come from `@neyuki/ui` itself: draw them on a canvas and upload that as a texture. `Texture:update` writes a redrawn canvas over the same allocation, which is what a heads-up display needs, and a pipeline with `depth = false` and `blend = "alpha"` draws it over depth-tested geometry in the same frame.

Colours come out as they went in. The surface is configured with a plain, non-sRGB format, so `gpu.clear(win, "#9ac2e8")` shows the same colour `win:clear("#9ac2e8")` would on a CPU window, and a texture made from a `ui` image matches what `ui` draws from it. A script that wants linear shading converts in its shader.

The module is behind the `gpu` cargo feature, which is **off** in stock builds, as it adds a large dependency tree and several MiB to the executable. A build that has it is `cargo install neyuki --features gpu`, `cargo build --release --features gpu` from a clone, or the `neyuki-gpu` executable packed beside `neyuki` in every release archive. Check `gpu.isAvailable()` first: it is `false` both on a build without the feature and on a machine with no usable adapter.

wgpu's own environment variables apply — `WGPU_BACKEND=vulkan` pins a backend, `WGPU_POWER_PREF=low` prefers the integrated GPU — and `NEYUKI_GPU_SOFTWARE=1` adds the one wgpu has no variable for: use the software adapter (WARP on Windows, lavapipe elsewhere) instead of real hardware. That is how a machine with no GPU runs the same code, slowly but correctly, and `frame:read()` makes those runs checkable with no display.

> **Status: new.** Textured, depth-tested meshes from a `buffer`, canvases as textures, and a chunked landscape as the acceptance test. Still to come: more than one vertex buffer or bind group, instance data, a first vertex and a base instance on a draw, render targets other than the window, mipmaps, and a choice of present mode.

```lua
local gpu = require("@neyuki/gpu")
local ui = require("@neyuki/ui")

if not gpu.isAvailable() then return end

local win = ui.open({ width = 960, height = 560, gpu = true })
local device = gpu.device(win)

local pipeline = device:pipeline({
    shader = device:shader(wgsl),
    vertex = { "vec3", "vec2" },   -- @location(0), @location(1)
    depth = true,
    cull = "back",
})
local mesh = device:buffer(vertexBytes, "vertex")
local matrix = device:buffer(64, "uniform")
local atlas = device:texture(ui.loadImage("atlas.png"))
local sampler = device:sampler()

ui.run(function()
    matrix:write(viewProjectionBytes)
    device:frame({ clear = "#11131a" })
        :pipeline(pipeline)
        :bind(0, matrix)
        :bind(1, atlas)
        :bind(2, sampler)
        :vertices(mesh)
        :draw(vertexCount)
        :finish()
    return true
end)
```

A full example, including the matrix arithmetic, is in [examples/gpu.nyk](../../examples/gpu.nyk). The larger one is [examples/gpu_terrain.nyk](../../examples/gpu_terrain.nyk): a chunked landscape meshed into `buffer` values, a heads-up display on a canvas, and a benchmark that will draw the same quads through `@neyuki/ui` instead for comparison.

## Conventions

Fixed now, before anything uses them, because scripts hard-code matrices against them. They are wgpu's own (Direct3D / Metal) conventions, so nothing is patched up on the way to the GPU:

| | |
| --- | --- |
| Clip space | `x`, `y` in `-1..1`, **y up**; `z` in **`0..1`** (0 near, 1 far) |
| Depth range | `0..1`, **not** OpenGL's `-1..1` |
| Framebuffer and texture coordinates | origin **top left**, y down, as in `@neyuki/ui` |
| World and view space | **right-handed**, y up, looking down `-z` |
| Matrices | **column-major**, column-vector convention: `clip = projection * view * model * position` |
| Front faces | **counter-clockwise**, which is what `cull = "back"` keeps |

A 4×4 matrix is 16 numbers written column after column, which is what a WGSL `mat4x4<f32>` reads with no transpose.

Every frame on a GPU window has a depth buffer, whether or not the pipeline uses it: `depth = false` means the pipeline never tests or writes it, not that there is none. So one frame can draw depth-tested geometry and then a flat overlay over it without a second kind of render pass, and a window that only ever clears never allocates one.

Resources are freed by `:close()`, and closing twice does nothing. The GPU memory goes when the driver has finished with it, which may be after a frame still using it has been submitted. Resources outlive the window they were made through: they belong to the device, which every GPU window shares.

## gpu.isAvailable(): boolean

Whether a window can be opened with `gpu = true` here: this build has the `gpu` feature and the machine offers an adapter. Every other function raises an error when this is `false`.

## gpu.adapter(): table

The adapter in use:

- `name`, as the driver reports it
- `backend`: `"vulkan"`, `"dx12"`, `"metal"`, `"gl"`
- `type`: `"discrete"`, `"integrated"`, `"cpu"`, `"virtual"`, `"other"`
- `driver`

The adapter is chosen for the first GPU window, so this raises an error until one is open.

## gpu.clear(window: Window, color: any): ()

Fills `window` with one colour and presents the frame — the whole of drawing it, in one call. `window` must have been opened with `gpu = true`. `color` takes the same forms as in `@neyuki/ui`: `"#rgb"`, `"#rrggbb"`, `"#rrggbbaa"`, a `{ r, g, b, a? }` table with channels 0–255, or a packed `0xRRGGBBAA` integer. Raises while a frame from `device:frame` is open.

## gpu.cancelFrame(): boolean

Discards whatever frame is open, as `Frame:cancel` does, and reports whether there was one. Nothing is presented.

This is the way out when the handle is gone. An error raised between `device:frame()` and `frame:finish()` leaves the frame open — which is what lets a mistyped argument be an ordinary error rather than the end of the window — and a draw loop that caught the error has usually lost the frame with the call that raised it. The frame holds the window's next image, so until it is let go of every later `device:frame()` is refused. A loop can call this before starting a frame, or after catching an error, and need keep no handle.

Closing a window discards its open frame too, so neither is something a script has to unwind by hand.

## gpu.device(window: Window): Device

The device for `window`, which must have been opened with `gpu = true`. The device, its adapter and its queue are shared by every GPU window; the frames this one opens are `window`'s.

Raises here, rather than from the first resource built, when the machine has no adapter or this build has no GPU support.

## Device:shader(source: string): Shader

Compiles WGSL. The shader needs one `@vertex` and one `@fragment` entry point; which they are called does not matter.

A shader that does not compile raises with the compiler's own message, which carries the line, the column and a snippet of the source:

```
cannot compile the shader: parsing error: no definition in scope for identifier: `nope`
  — wgsl:3:12
  |
3 |     return nope;
  |            ^^^^ unknown identifier
```

wgpu's framing around it — which function was called, and the label this module gave the shader module — is trimmed off, because neither is anything the script wrote.

## Device:buffer(data: buffer | number, usage: string): GpuBuffer

A buffer on the GPU, from the bytes of a `buffer` or, given a number, that many zero bytes. `usage` is `"vertex"`, `"index"` or `"uniform"`, and decides what the buffer may be bound as; a buffer used as the wrong kind is refused by name.

`size` on the result is what was actually allocated: the request rounded up to what the usage needs, so a 12-byte uniform buffer is 16 bytes. `usage` is on the result too.

## GpuBuffer:write(data: buffer, offset: number?): ()

Replaces part of the contents, which is how a uniform buffer is refreshed each frame without making a new one. `offset` and the length of `data` must both be multiples of 4, and the write must fit.

## GpuBuffer:close(): ()

Frees the buffer.

## Device:texture(source: Image | Canvas): Texture

Uploads an `@neyuki/ui` image or canvas: an image's first frame, or a canvas as it stands, RGBA and premultiplied as `@neyuki/ui` keeps it — which is why `blend = "alpha"` blends premultiplied.

`width` and `height` on the result are physical pixels, so a canvas made with `{ scale = win:scale() }` gives a texture that much larger than the size it was asked for — the same pixels `window:drawImage` would copy.

Textures are sampled with `0..1` coordinates whose origin is the top left, so `v` runs down the image as `y` does in `@neyuki/ui`.

## Texture:update(source: Image | Canvas): Texture

Writes a redrawn image or canvas over the texture, and returns it.

This is how a heads-up display reaches the GPU: draw it on a canvas with `@neyuki/ui`, upload it once with `device:texture`, then call this whenever it changes. The texture, its view and every bind group holding it stay as they are, so a frame costs one copy and nothing is rebuilt. Redraw a display when it changes rather than every frame: `text` costs far more than the copy does.

The source has to be the size the texture already is. A texture cannot be resized, so a canvas that grew needs a new one.

## Texture:close(): ()

Frees the texture.

## Device:sampler(options: table?): Sampler

How a shader reads a texture.

| option | |
| --- | --- |
| `filter` | `"linear"` (the default) or `"nearest"` |
| `wrap` | `"clamp"` (the default), `"repeat"` or `"mirror"` |

## Sampler:close(): ()

Frees the sampler.

## Device:pipeline(options: table): Pipeline

One shader, one vertex layout, and the fixed state around them.

| option | |
| --- | --- |
| `shader` | the compiled WGSL |
| `vertex` | the attribute list, packed in order from the start of a vertex and numbered from `@location(0)` |
| `depth` | test and write the depth buffer (`false` by default) |
| `cull` | `"none"` (the default), `"back"` or `"front"` |
| `topology` | `"triangles"` (the default), `"triangleStrip"`, `"lines"`, `"lineStrip"` or `"points"` |
| `blend` | `"none"` (the default) or `"alpha"`, premultiplied |

An attribute is one of `"float"`, `"vec2"`, `"vec3"`, `"vec4"`, `"half2"`, `"half4"`, `"int"`, `"uint"`, `"ubyte4"` or `"unorm4"`. They are packed tightly: `{ "vec3", "unorm4" }` is a 16-byte vertex of a position and a colour, written straight into a `buffer` with no padding.

Bindings are not declared here. The shader's own `@group(0) @binding(n)` lines are the layout, and `frame:bind` fills them; a binding the shader does not declare, or one of the wrong kind, is refused by the next `draw`. Only group 0 is bound.

## Pipeline:close(): ()

Frees the pipeline.

## Device:frame(options: table?): Frame

Starts a frame on this device's window: takes the window's next image, clears the colour and depth buffers, and opens a render pass. `options.clear` is the colour (black by default, in the same forms as `@neyuki/ui`) and `options.depth` the depth to clear to (1, the far plane, by default).

Only one frame can be open at a time, because it holds the window's next image. `frame:finish()` presents it; a script that stops short leaves the window's image held until it starts another frame.

A frame whose window cannot be drawn — minimized, hidden, or the compositor was busy — is opened anyway and **skipped**: every call on it still checks its arguments, nothing is drawn, and `finish` reports `false`. A draw loop needs no special case for a window nobody can see.

## Frame:pipeline(pipeline: Pipeline): Frame

Draws with `pipeline` from here on, and **clears the bindings**: a layout comes from one shader's own `@group(0)` declarations, and the next shader's are not the same. So bind after setting the pipeline, and bind again after changing it.

That is what lets one frame hold a depth-tested landscape and then a flat, blended overlay. Each pipeline is given its own resources, and a sampler the first one wanted does not make the second one's bind group the wrong size. Forgetting to bind after a change is refused by the next draw, naming the binding, rather than quietly drawing with the previous pipeline's resources.

## Frame:bind(index: number, resource: GpuBuffer | Texture | Sampler): Frame

Points `@group(0) @binding(index)` at a uniform buffer, a texture or a sampler. The bind group is built once before the next draw, from everything bound by then, so binding once and drawing many times costs one.

## Frame:vertices(buffer: GpuBuffer): Frame

The vertex buffer to read attributes from. It must have been made with usage `"vertex"`.

## Frame:indices(buffer: GpuBuffer, format: string?): Frame

Makes the following draws indexed. `format` is `"u32"` (the default) or `"u16"`, and the buffer must have been made with usage `"index"`.

## Frame:draw(count: number, instances: number?): Frame

Draws `count` vertices, or `count` indices once `indices` has been called, `instances` times (1 by default). The count is measured against the bound buffers first, so a wrong number is a script error naming it rather than a driver error on a later call.

## Frame:finish(): boolean

Submits the frame and presents the window. Returns `false` when the frame was skipped, which is not an error. The frame cannot be used again.

## Frame:cancel(): ()

Throws the frame away without presenting it: the window keeps whatever it last showed. The frame cannot be used again.

This is how a draw loop recovers from an error raised mid-frame. Use `gpu.cancelFrame()` where the handle is out of reach, which in a loop it usually is.

## Frame:read(): (boolean, buffer?, number?, number?)

Finishes the frame as `finish` does, and also reads the image back: `presented, pixels, width, height`, where `pixels` is tightly packed RGBA and the size is in physical pixels — the window's logical size times whatever the display scales by. `pixels` is nil when the frame was skipped.

This waits for the GPU, so it is for tests and screenshots, not for a draw loop. It is what lets a script check that what was drawn is what was meant, on a hidden window and with no GPU at all.
