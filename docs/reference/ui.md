# @neyuki/ui

Windows, 2D drawing, input events and animation on Windows, macOS, X11 and Wayland from one code path. Frames are drawn on the CPU (no GPU or GL needed). The script owns the loop: `ui.poll` returns what happened, drawing calls paint a back buffer and `window:present()` shows it; `ui.run` wraps that loop.

Coordinates are logical pixels (HiDPI scaling is handled underneath). Colors are `"#rgb"`, `"#rrggbb"` or `"#rrggbbaa"` strings, `{ r, g, b, a? }` tables with channels from 0 to 255, or packed `0xRRGGBBAA` integers.

```lua
local ui = require("@neyuki/ui")
local win = ui.open({ title = "Hello", width = 480, height = 320 })
local ball = { x = 60, y = 160 }
ui.animate(ball, { x = 420 }, 1.5, { easing = "inOutSine", ["repeat"] = true, yoyo = true })

ui.run(function(events, dt)
    for _, event in events do
        if event.kind == "keydown" and event.key == "escape" then win:close() end
    end
    win:clear("#202030")
    win:circle(ball.x, ball.y, 24, "#4488ff")
    win:text(20, 20, "Press Escape to quit", { size = 18, color = "#ffffff" })
    win:present()
end)
```

An app that draws its own title bar opens the window without decorations and hands moving and resizing back to the window system, so the window still snaps and crosses monitors like a native one:

```lua
local win = ui.open({ title = "Player", width = 1280, height = 800, decorations = false,
                      minWidth = 900, minHeight = 600, icon = "icon.png" })
local query = ""

ui.run(function(events, dt)
    for _, event in events do
        if event.kind == "mousedown" and event.button == "left" and event.y < 32 then
            if event.clicks == 2 then
                if win:isMaximized() then win:restore() else win:maximize() end
            else
                win:startDrag()
            end
        elseif event.kind == "textinput" then
            query = query .. event.text
        end
    end
    -- ...draw the bar, its buttons and the rest
end)
```

See `examples/window_chrome.nyk` for a complete one with edge resizing, hover cursors and an IME-aware text field.

## ui.open(options: table?): Window

Opens a window. `options` may hold:

- `title` (default `"Neyuki"`), `width` and `height` (default 800 × 600)
- `resizable` and `visible` (default `true`)
- `decorations` (default `true`): `false` removes the native title bar and border, for an app-drawn one
- `minWidth` and `minHeight`: the smallest size the user can resize to
- `icon`: a PNG path or an `Image` for the title bar and taskbar (Windows and X11; macOS and Wayland take the icon from the app bundle or desktop file)
- `transparent` (default `false`): pixels drawn with alpha show what is behind the window, where the platform supports it (Windows and macOS; X11 and Wayland present opaque)
- `shadow` (default `true`): keep the drop shadow of a window without decorations (Windows and macOS)

The canvas starts out white, or clear when `transparent`.

Without decorations, Windows 11 does not show the Snap Layouts flyout when hovering the app's own maximize button; `Win+Z` and dragging to a screen edge still snap.

## ui.run(handler: (events: {table}, dt: number) -> any, options: table?): ()

Calls `handler(events, dt)` about 60 times a second (`options.fps`, or `"auto"`) until every window is closed, where `dt` is the seconds since the previous call. Tweens from `ui.animate` are stepped just before each call. Close requests close their window unless `options.autoClose` is `false`. An error in the handler closes the windows and is raised again.

With `fps = "auto"` the loop runs at 60 fps only while a `ui.animate` tween is running, events arrived in the last step or the handler returned `true` (return it while an animated image is visible). Otherwise it waits in the event loop for up to a second, so an idle window uses almost no CPU.

Inside a task (`task.spawn(function() ui.run(...) end)`) the loop does not block the program: `spawn` returns at once with the task suspended, and the window runs while the main program is in `task.wait` and after it finishes, until every window is closed. See [Windows in a task](task.md#windows-in-a-task) for the limits (one event loop at a time, cooperative scheduling, and `task.wait` inside the handler blocking the loop).

## ui.poll(timeout: number?): {table}

Pumps the event loop and returns the events since the last call. Without a timeout it returns at once; with one it waits up to that many seconds for the first event. Inside a task, a poll with a timeout suspends that task instead of blocking the program, and the scheduler wakes it with the events or, when the time is up, an empty list. Each event has `kind` and `window` plus:

- `close`: the close button or shortcut was used (call `window:close()` to honour it)
- `resize`: `width`, `height`
- `focus`, `blur`, `redraw`, `mouseenter`, `mouseleave`
- `keydown`, `keyup`: `key` (`"a"`, `"escape"`, `"arrowleft"`, `"f1"`), `code` (`"KeyA"`), `repeat`, `text`, `mods` (`shift`, `ctrl`, `alt`, `meta`)
- `textinput`: `text`, what was typed or committed by an input method. Use it rather than `keydown.text` for text fields: it skips shortcuts and does not repeat what an input method is composing
- `ime`: `phase` (`"start"`, `"preedit"`, `"commit"` or `"end"`), `text` (the composition so far for `preedit`, the result for `commit`) and `cursor` (bytes of `text` before the caret, or nil when hidden). Only sent after `window:setTextInput(true)`; draw `preedit` text underlined at the caret and drop it on `commit` or `end`
- `mousemove`: `x`, `y`
- `mousedown`: `button` (`"left"`, `"right"`, `"middle"`), `x`, `y`, `clicks` (1, then 2 for a double click, 3 for a triple click: presses of the same button within 500 ms and 4 pixels)
- `mouseup`: `button`, `x`, `y`. After `startDrag` or `startResize` the window system keeps the release, so it may not arrive
- `scroll`: `dx`, `dy`, `unit` (`"lines"` or `"pixels"`), `x`, `y`
- `mediakey`: `key` (`"playpause"`, `"play"`, `"pause"`, `"next"`, `"previous"`, `"stop"`), from a media key, a headset button or the OS media controls; see `ui.mediaSession`
- `mediasession`: `action` (`"seek"` with `position` in seconds or a relative `offset`, `"raise"` or `"quit"`), from the OS media controls
- `maximize`, `minimize`, `restore`: the window changed state, by the user or by a call
- `dropfile`: `path`, a file dropped on the window (one event per file)
- `dpichange`: `scale`, the window moved to a display with another scale factor

## ui.stats(): table

Timing of the last presented frame in milliseconds, as `draw` (script and drawing since the previous present, without blur), `blur`, `present` and `frame` (the three together), plus `frames`, the number presented so far. Useful to pick a quality level.

## ui.isAvailable(): boolean

Whether a display can be reached. On a headless machine the other window functions raise an error.

## ui.mediaSession(info: table?): (boolean, string?)

Shows what is playing in the OS media controls (the volume flyout and lock screen on Windows, MPRIS applets on Linux, Control Center on macOS) and starts receiving their commands as `mediakey` and `mediasession` events. Once a session exists, the global media keys (keyboards, headsets) reach the app even while it is in the background; without one, they arrive only while a window has focus.

`info` may hold `title`, `artist`, `album`, `artwork` (an `Image`, a file path or an `http(s)` URL), `duration` and `position` in seconds, `playing` (default `false`) and the `window` to attach to (default: the first open one). Each call replaces what is shown; fields left out are cleared. `ui.mediaSession(nil)` ends the session, and closing its window does too. Returns `true`, or `false` and the reason when the OS has no media controls to offer (no D-Bus session bus, say).

```lua
ui.mediaSession({ title = "Second Wind", artist = "The Placeholders", duration = 212, position = 0, playing = true })
for _, event in ui.poll(0.1) do
    if event.kind == "mediakey" and event.key == "playpause" then togglePlayback() end
end
```
## ui.clipboard.get(): string?

The clipboard's text, or nil when it holds something else or nothing. The same clipboard as `@neyuki/hid`.

## ui.clipboard.set(text: string): ()

Replaces the clipboard contents with `text`.

## ui.openUrl(url: string): ()

Opens a link in the default browser or mail client and returns without waiting. Only `http://`, `https://` and `mailto:` links without spaces are accepted; anything else (a file path, `file:`, a custom protocol) raises an error, because the system would happily run a program for it.

## ui.color(color: any): number

Packs any color form into a `0xRRGGBBAA` integer. Drawing methods take every form directly (and remember the strings they have parsed), so packing is only needed to do arithmetic on a color.

## ui.loadFont(source: string | buffer): Font

Loads a TrueType or OpenType font for `text` and `measureText`, from a file path or from a buffer of its bytes (an app can embed its own CJK font this way). Without one, text uses the bundled Noto Sans. The `Font` has `name` and, when loaded from a path, `path`. `font:close()` frees it and drops it from the fallback list.

## ui.systemFont(family: string, options: table?): Font?

Loads the installed font whose family or PostScript name is `family` (ignoring case), or returns `nil` when there is none. `options.weight` (100 to 900, default 400) and `options.italic` pick the closest face. The installed fonts are scanned on the first call.

## ui.setFallbackFonts(fonts: {Font}?): ()

Sets the fonts every text searches, in order, for characters its own font lacks (kana, kanji, Hangul, emoji). The bundled font is always tried last. An empty list or `nil` clears them. A `fallback` option on a single call replaces this list for that call.

## ui.loadImage(path: string, options: table?): Image

Decodes a PNG, JPEG, WebP, GIF or BMP file into an `Image`. The format is detected from the file's first bytes, not its name. Animated GIF, WebP and PNG files decode every frame.

An `Image` has `width`, `height`, `frames` (1 for stills), `loopCount` (0 plays forever) and `path`. `options.maxPixels` (default 64 million) and `options.maxBytes` (memory for all decoded frames, default 512 MiB) reject oversized images with an error. `image:close()` frees it.

## ui.decodeImage(data: buffer | string, options: table?): Image

Like `loadImage`, from the bytes of an image held in memory, such as an HTTP response body.

## Image:frameDelay(i: number): number

The seconds frame `i` (from 1) stays on screen. Delays of 10 ms or less are read as 100 ms, as browsers do. Stills report 0.

## Image:resized(w: number, h: number): Image

A new image with every frame resampled to `w` x `h` pixels, for shrinking a large animation once instead of scaling it on every draw.

## Image:blur(radius: number): Image

A new image with every frame blurred by a gaussian whose standard deviation is `radius` pixels (0 to 200). Blur a wallpaper once and draw the result every frame instead of calling `blurRegion` on it.

## ui.canvas(w: number, h: number, options: table?): Canvas

Makes an off-screen surface of `w` x `h` logical pixels, transparent at first. A canvas has every drawing method of a window (`clear`, `fillRect`, `text`, `pushClip`, `getPixel` and the rest; `clear()` without a color makes it transparent again) but no `present`, and it needs no display. Draw it onto a window or another canvas with `drawImage`, which takes `x`, `y`, `width`, `height`, `opacity`, `radius` and `fit` like it does for an image. It also has `width`, `height`, `scale`, `canvas:size()` and `canvas:close()`.

`options.scale` is the canvas's physical pixels per logical pixel (default 1). Pass `win:scale()` for a canvas drawn one to one on a HiDPI display; otherwise it is resampled when drawn. Canvases are not resized for you: cache a panel's rendering in one and redraw it instead of redrawing every row each frame.

```lua
local card = ui.canvas(200, 120, { scale = win:scale() })
card:fillRect(0, 0, 200, 120, "#202030", { radius = 12 })
card:text(16, 16, "Cached", { color = "#ffffff" })
-- later, every frame:
win:drawImage(card, 40, 40, { opacity = 0.9 })
```

## ui.animation(image: Image, options: table?): ImageAnimation

Plays an animated image from elapsed time. `anim:update(dt)` advances `anim.frame` by the image's frame delays and returns it; `anim:reset()` starts over. `options.speed` scales time (default 1) and `options.loop = true` repeats forever whatever the image's `loopCount`. When the loops are used up, `anim.finished` becomes true on the last frame. `anim.playing` can be set to `false` to pause.

```lua
local anim = ui.animation(img)
anim:update(dt)
win:drawImage(img, 0, 0, { width = w, height = h, fit = "cover", frame = anim.frame })
```

## ui.measureText(s: string, options: table?): (number, number, number)

The width, height and number of lines `window:text` would cover for `s` with the same options (`size`, `font`, `fallback`, `maxWidth`, `overflow`, `lineHeight`, `letterSpacing`), truncation and wrapping included.

## Window:clear(color: any?): ()

Fills the whole canvas (default white).

## Window:fillRect(x: number, y: number, w: number, h: number, color: any, options: table?): ()

A filled rectangle; `options.radius` rounds the corners. `options.gradient` replaces the flat `color` with a gradient and `options.shadow` adds a drop shadow (both also work on `circle`, `polygon` and `fillPath`):

```lua
win:fillRect(20, 20, 200, 120, "#ffffff", {
    radius = 16,
    gradient = { from = "#ff7a59", to = "#7a5cff", angle = 45 },
    shadow = { blur = 24, offsetY = 8, color = "#00000066" },
})
```

A gradient is `{ from, to }` or `{ stops = { { offset, color }, ... } }` (offsets from 0 to 1). `kind` is `"linear"` (default) or `"radial"`. A linear gradient runs at `angle` degrees (default 0, left to right; 90 runs top to bottom) across the shape's box. A radial one is centered on `center = { x, y }` (default the middle of the box) and reaches `radius` (default the box's corners). A shadow is `{ blur, offsetX, offsetY, spread, color }`: `blur` is the CSS-style blur radius (default 0), `spread` grows the shape first, and the color defaults to a translucent black. A shadow is blurred once per shape and size and then reused, whatever its color.

## Window:strokeRect(x: number, y: number, w: number, h: number, color: any, options: table?): ()

A rectangle outline; `options.width` is the line width and `options.radius` rounds the corners.

## Window:line(x1: number, y1: number, x2: number, y2: number, color: any, options: table?): ()

A line; `options.width` is its width.

## Window:circle(cx: number, cy: number, radius: number, color: any, options: table?): ()

A filled circle, or an outline of `options.width` when `options.fill` is `false`.

## Window:polygon(points: {number}, color: any, options: table?): ()

A closed shape through the points of a flat `{ x1, y1, x2, y2, ... }` list, filled unless `options.fill` is `false`.

## Window:text(x: number, y: number, s: string, options: table?): (number, number)

Draws text with the top-left corner of its first line at `(x, y)`; `"
"` starts a new line. Returns the width and height drawn. `options` may hold:

- `size` (default 16), `color` (default black), `opacity` (0 to 1) and `font`.
- `fallback`: fonts to try for characters `font` lacks, instead of the ones from `ui.setFallbackFonts`.
- `maxWidth` with `overflow`: `"wrap"` (default) breaks lines, `"ellipsis"` shortens a line and ends it with `...`, `"clip"` cuts it at the width.
- `align`: `"left"` (default), `"center"` or `"right"`. Within `maxWidth` the text aligns inside it; without it, centered text is centered on `x` and right-aligned text ends at `x`.
- `lineHeight` (a multiple of the size) and `letterSpacing` (pixels).

## Window:drawImage(image: Image, x: number, y: number, options: table?): ()

Draws an image (or a `ui.canvas`) at its own size, or into the box given by `options.width` and/or `options.height`. More options: `frame` (which frame of an animation, from 1), `opacity` (0 to 1), `radius` (rounds the corners) and `fit`: `"fill"` (default) stretches the image to the box, `"contain"` shows all of it centered, `"cover"` fills the box and crops the overflow around the center. Scaling is smooth, and the scaled copy of a frame is cached, so redrawing it at the same size is cheap.

## ui.path(): Path

Starts an empty path to fill, stroke or clip with. Every method returns the path, so calls chain: `ui.path():moveTo(0, 0):lineTo(40, 0):lineTo(20, 30):close()`. Coordinates are logical pixels.

- `path:moveTo(x, y)`, `path:lineTo(x, y)` and `path:close()` build straight sub-paths.
- `path:quadTo(cx, cy, x, y)` and `path:cubicTo(c1x, c1y, c2x, c2y, x, y)` add curves.
- `path:rect(x, y, w, h)`, `path:roundRect(x, y, w, h, r)`, `path:circle(cx, cy, r)` and `path:ellipse(cx, cy, rx, ry)` add closed shapes.
- `path:bounds()` returns `x, y, width, height` around every point and control point added so far (all 0 for an empty path).

## Window:fillPath(path: Path, color: any, options: table?): ()

Fills a path. Options: `gradient` and `shadow` as for `fillRect`, and `evenOdd` (default `false`) to fill by the even-odd rule instead of the nonzero rule, so that nested shapes leave holes (it cannot be combined with a gradient or shadow).

## Window:strokePath(path: Path, color: any, options: table?): ()

Strokes a path. Options: `width` (default 1), `cap` (`"butt"` default, `"round"` or `"square"`) and `join` (`"miter"` default, `"round"` or `"bevel"`).

## Window:pushClip(x: number, y: number, w: number, h: number, options: table?): ()

Limits later drawing (shapes, text and images) to a rectangle, rounded by `options.radius`, until the matching `popClip`. Clips nest: each one intersects the clips pushed before it. A clip without rounded corners has its edges rounded to whole pixels and costs next to nothing to push or draw in; a rounded one has anti-aliased edges. At most 64 clips can be open at once.

```lua
win:pushClip(0, 40, 300, 200, { radius = 12 })
for i = 1, 500 do
    win:text(12, 40 + i * 24 - scroll, "Row " .. i)
end
win:popClip()
```

## Window:pushClipPath(path: Path): ()

Like `pushClip`, limited to the inside of a path.

## Window:popClip(): ()

Ends the latest `pushClip` or `pushClipPath`. It is an error to pop more clips than were pushed.

## Window:pushOpacity(opacity: number): ()

Starts a group: everything drawn until the matching `popOpacity` is composited as one at `opacity` (0 to 1), so overlapping shapes inside it do not show through each other. This is what a fade or slide-in of a whole panel needs. Groups nest (up to 16). A group draws into an off-screen layer the size of the surface, which is kept for the next group, and only the part that was drawn on is composited, so a fade costs about as much as what it fades. Close the clips opened inside a group before popping it. Inside a group, `blurRegion` sees only the group's own layer.

## Window:popOpacity(): ()

Ends the latest `pushOpacity` and composites the group.

## Window:blurRegion(x: number, y: number, w: number, h: number, radius: number, options: table?): ()

Blurs what has been drawn so far under a rectangle, like a CSS `backdrop-filter`: draw the wallpaper, call `blurRegion`, then draw a translucent panel on top for frosted glass. `radius` is the standard deviation of the gaussian in logical pixels (0 to 200), and `options.radius` rounds the rectangle's corners. The cost does not grow with the radius, and blurring the same pixels the same way as a recent call (a panel over a still background, frame after frame) reuses that result.

```lua
win:drawImage(wallpaper, 0, 0, { width = w, height = h, fit = "cover" })
win:blurRegion(16, 16, 240, h - 32, 24, { radius = 16 })
win:fillRect(16, 16, 240, h - 32, "#ffffff30", { radius = 16 })
```

## Window:getPixel(x: number, y: number): (number, number, number, number)

The color of the pixel under `(x, y)` as `r, g, b, a`, each from 0 to 255 (not premultiplied). It reads the back buffer, so it sees what has been drawn but not necessarily presented.

## Window:present(options: table?): ()

Shows everything drawn since the last call. `options.dirty` may list the rectangles that changed as `{x, y, w, h}` in logical pixels, for example `win:present({ dirty = { { 0, 0, 200, 40 } } })`; only those regions are copied to the screen (where the platform can reuse the previous frame, otherwise the whole frame is shown). The back buffer is never cleared by `present`, so drawing only the changed parts works. Every `pushClip` and `pushOpacity` must be closed first; otherwise the unfinished ones are dropped and an error is raised.

## Window:size(): (number, number)

The current logical width and height.

## Window:setSize(w: number, h: number): ()

Resizes the window.

## Window:scale(): number

Physical pixels per logical pixel (2 on a typical HiDPI display).

## Window:title(): string

The window title.

## Window:setTitle(title: string): ()

Changes the window title.

## Window:isOpen(): boolean

Whether the window has not been closed.

## Window:close(): ()

Closes the window. Calling it again does nothing.

## Window:startDrag(): boolean

Moves the window with the mouse as if its title bar were being dragged. Call it on `mousedown` in a custom title bar, while the button is still held; returns whether the window system took over.

## Window:startResize(direction: string): boolean

Resizes the window with the mouse from an edge or corner: `"n"`, `"s"`, `"e"`, `"w"`, `"ne"`, `"nw"`, `"se"` or `"sw"`. Call it on `mousedown` near the edge, like `startDrag`. A window without decorations has no resize border of its own, so this is how one gets resized.

## Window:minimize(): ()

Minimizes the window to the taskbar or dock.

## Window:maximize(): ()

Maximizes the window.

## Window:restore(): ()

Brings a minimized window back, or else returns a maximized one to its normal size.

## Window:isMaximized(): boolean

Whether the window is maximized.

## Window:setFullscreen(fullscreen: boolean): ()

Switches to borderless fullscreen on the window's current monitor, or back.

## Window:setCursor(cursor: string): ()

The pointer shown over the window: `"default"`, `"pointer"` (the hand for links and buttons), `"text"`, `"grab"`, `"grabbing"`, `"crosshair"`, `"move"`, `"notAllowed"`, `"wait"`, `"progress"`, `"help"`, `"copy"`, `"contextMenu"`, `"zoomIn"`, `"zoomOut"`, `"ewResize"`, `"nsResize"`, `"neswResize"`, `"nwseResize"`, `"colResize"`, `"rowResize"`, or `"hidden"`.

## Window:setTextInput(enabled: boolean, area: table?): ()

Turns text input through the input method (for Chinese, Japanese, Korean and other composed text) on or off; `ime` events are only sent while it is on. `area` is the `{ x, y, w, h }` of the text being edited, so the candidate window opens next to it; call it again as the caret moves. Turn it off when the text field loses focus.

## Window:focus(): ()

Brings the window to the front and gives it the keyboard, as far as the window system allows.

## Window:requestAttention(): ()

Flashes the taskbar entry or bounces the dock icon until the window is focused.

## Window:setIcon(icon: any): ()

Sets the title bar and taskbar icon from an `Image` or a PNG path, or clears it with nil. Like the `icon` option, it has no effect on macOS and Wayland.

## ui.tween(target: table, goal: table, duration: number, options: table?): Tween

Animates the fields of `target` toward the values in `goal` over `duration` seconds. Number fields move numerically; any other goal is a color, and the field ends up a packed color. Starting values are read when the tween starts, so a tween chained from `onComplete` continues where the previous one ended.

`options` may hold `easing` (a function or the name of a `ui.easing` curve, default `"outQuad"`), `delay` (seconds), `repeat` (extra runs, or `true` for forever), `yoyo` (alternate runs play backwards), `onUpdate(target)` and `onComplete(target)`.

A `Tween` does nothing until `tween:update(dt)` is called, which returns whether it has ended. It also has `pause()`, `resume()`, `cancel()` (stop in place), `finish()` (jump to the goal and call `onComplete`), `isDone()`, `isPaused()` and `progress()` (0 to 1).

## ui.animate(target: table, goal: table, duration: number, options: table?): Tween

Like `tween`, but registered so `ui.run` (or `ui.update`) steps it until it ends.

```lua
local panel = { y = -200, fill = "#334455" }
ui.animate(panel, { y = 0 }, 0.4, { easing = "outBack" })
ui.animate(panel, { fill = "#5577aa" }, 0.8, { delay = 0.4 })
```

## ui.update(dt: number): number

Steps every tween started with `animate` by `dt` seconds and returns how many are still running. Only needed in a loop written with `poll` instead of `run`.

## ui.easing: table

Easing curves that map progress from 0 to 1: `linear`, `inQuad`, `outQuad`, `inOutQuad`, `inCubic`, `outCubic`, `inOutCubic`, `inSine`, `outSine`, `inOutSine`, `inExpo`, `outExpo`, `inBack`, `outBack`, `outElastic` and `outBounce`.

## ui.lerp(a: number, b: number, t: number): number

The value `t` of the way from `a` to `b`.

## ui.mixColor(from: any, to: any, t: number): number

Mixes two colors channel by channel, alpha included, and returns the packed result.
