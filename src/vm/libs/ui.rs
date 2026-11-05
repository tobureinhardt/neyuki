//! Native windowing primitives backing `@neyuki/ui`.
//!
//! Windows come from `winit`, frames are drawn on the CPU with `tiny-skia`
//! and `fontdue` and blitted with `softbuffer`, so no GPU or GL
//! driver is needed and one code path serves X11, Wayland, macOS and Windows.
//!
//! Like the HTTP server, the design is pull-based because natives cannot call
//! back into Neyuki functions: `__ui_open` parks a window here under an id,
//! the drawing natives paint into that window's back buffer, `__ui_present`
//! shows it and `__ui_poll` pumps the OS event loop and hands back whatever
//! happened as a list of event tables. `lib/ui.nyk` builds the `Window`,
//! `Image` and `Font` objects and the `run` loop on top of these. Image
//! decoding, resampling and compositing live in `ui/images.rs`.
//!
//! Every coordinate crossing the boundary is in logical pixels; the back
//! buffer is kept at the window's physical size and drawing is scaled by the
//! DPI factor, so scripts never see it. Colors arrive packed as `0xRRGGBBAA`.
//!
//! winit only supports the event loop on the thread that created it (and on
//! macOS only the main thread), which is where the interpreter runs; all
//! state therefore lives in thread-locals.
//!
//! The media session (`__ui_media_session`) publishes now-playing metadata
//! to the OS through `souvlaki` and receives its transport commands, which
//! include the global media keys. Those arrive on whatever thread the OS
//! picks, so they travel through a channel and an `EventLoopProxy` wakes
//! the pump; `__ui_poll` turns them into `mediakey` / `mediasession` events.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Read;
use std::num::NonZeroU32;
use std::path::{Path as FsPath, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Mask, MaskType, Paint, Path,
    PathBuilder, Pixmap, Point, RadialGradient, Rect, Shader, SpreadMode, Stroke, Transform,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};
use winit::platform::pump_events::EventLoopExtPumpEvents;
use winit::window::{
    CursorIcon, Fullscreen, Icon, ResizeDirection, UserAttentionType, Window, WindowId,
};

use crate::vm::libs::args::array as new_table;
use crate::vm::libs::task;
use crate::vm::libs::webview;
use crate::vm::machine::VM;
use crate::vm::value::{NativeDef, StrRef, Value, VmTable};

mod clip;
mod effects;
mod images;

use clip::{Bounds, Clip, Skia};
use images::{Decoded, Fit, Limits};

pub(crate) const NATIVES: &[NativeDef] = &[
    NativeDef {
        name: "__ui_available",
        func: builtin_available,
    },
    NativeDef {
        name: "__ui_open",
        func: builtin_open,
    },
    NativeDef {
        name: "__ui_close",
        func: builtin_close,
    },
    NativeDef {
        name: "__ui_set_title",
        func: builtin_set_title,
    },
    NativeDef {
        name: "__ui_set_size",
        func: builtin_set_size,
    },
    NativeDef {
        name: "__ui_size",
        func: builtin_size,
    },
    NativeDef {
        name: "__ui_poll",
        func: builtin_poll,
    },
    NativeDef {
        name: "__ui_clear",
        func: builtin_clear,
    },
    NativeDef {
        name: "__ui_fill_rect",
        func: builtin_fill_rect,
    },
    NativeDef {
        name: "__ui_stroke_rect",
        func: builtin_stroke_rect,
    },
    NativeDef {
        name: "__ui_line",
        func: builtin_line,
    },
    NativeDef {
        name: "__ui_circle",
        func: builtin_circle,
    },
    NativeDef {
        name: "__ui_polygon",
        func: builtin_polygon,
    },
    NativeDef {
        name: "__ui_text",
        func: builtin_text,
    },
    NativeDef {
        name: "__ui_measure_text",
        func: builtin_measure_text,
    },
    NativeDef {
        name: "__ui_load_image",
        func: builtin_load_image,
    },
    NativeDef {
        name: "__ui_decode_image",
        func: builtin_decode_image,
    },
    NativeDef {
        name: "__ui_resize_image",
        func: builtin_resize_image,
    },
    NativeDef {
        name: "__ui_free_image",
        func: builtin_free_image,
    },
    NativeDef {
        name: "__ui_draw_image",
        func: builtin_draw_image,
    },
    NativeDef {
        name: "__ui_load_font",
        func: builtin_load_font,
    },
    NativeDef {
        name: "__ui_load_font_bytes",
        func: builtin_load_font_bytes,
    },
    NativeDef {
        name: "__ui_system_font",
        func: builtin_system_font,
    },
    NativeDef {
        name: "__ui_free_font",
        func: builtin_free_font,
    },
    NativeDef {
        name: "__ui_canvas",
        func: builtin_canvas,
    },
    NativeDef {
        name: "__ui_free_canvas",
        func: builtin_free_canvas,
    },
    NativeDef {
        name: "__ui_push_clip",
        func: builtin_push_clip,
    },
    NativeDef {
        name: "__ui_push_clip_path",
        func: builtin_push_clip_path,
    },
    NativeDef {
        name: "__ui_pop_clip",
        func: builtin_pop_clip,
    },
    NativeDef {
        name: "__ui_push_opacity",
        func: builtin_push_opacity,
    },
    NativeDef {
        name: "__ui_pop_opacity",
        func: builtin_pop_opacity,
    },
    NativeDef {
        name: "__ui_fill_path",
        func: builtin_fill_path,
    },
    NativeDef {
        name: "__ui_stroke_path",
        func: builtin_stroke_path,
    },
    NativeDef {
        name: "__ui_blur_region",
        func: builtin_blur_region,
    },
    NativeDef {
        name: "__ui_blur_image",
        func: builtin_blur_image,
    },
    NativeDef {
        name: "__ui_get_pixel",
        func: builtin_get_pixel,
    },
    NativeDef {
        name: "__ui_set_fallback_fonts",
        func: builtin_set_fallback_fonts,
    },
    NativeDef {
        name: "__ui_present",
        func: builtin_present,
    },
    NativeDef {
        name: "__ui_stats",
        func: builtin_stats,
    },
    NativeDef {
        name: "__ui_media_session",
        func: builtin_media_session,
    },
    NativeDef {
        name: "__ui_media_session_clear",
        func: builtin_media_session_clear,
    },
    NativeDef {
        name: "__ui_drag",
        func: builtin_drag,
    },
    NativeDef {
        name: "__ui_drag_resize",
        func: builtin_drag_resize,
    },
    NativeDef {
        name: "__ui_minimize",
        func: builtin_minimize,
    },
    NativeDef {
        name: "__ui_set_maximized",
        func: builtin_set_maximized,
    },
    NativeDef {
        name: "__ui_restore",
        func: builtin_restore,
    },
    NativeDef {
        name: "__ui_is_maximized",
        func: builtin_is_maximized,
    },
    NativeDef {
        name: "__ui_set_fullscreen",
        func: builtin_set_fullscreen,
    },
    NativeDef {
        name: "__ui_set_cursor",
        func: builtin_set_cursor,
    },
    NativeDef {
        name: "__ui_set_text_input",
        func: builtin_set_text_input,
    },
    NativeDef {
        name: "__ui_focus",
        func: builtin_focus,
    },
    NativeDef {
        name: "__ui_request_attention",
        func: builtin_request_attention,
    },
    NativeDef {
        name: "__ui_set_icon",
        func: builtin_set_icon,
    },
    NativeDef {
        name: "__ui_open_url",
        func: builtin_open_url,
    },
];

/// The font used when a script does not load its own: Noto Sans, under the
/// SIL Open Font License (see `lib/fonts/NotoSans-OFL.txt`).
const DEFAULT_FONT: &[u8] = include_bytes!("../../../lib/fonts/NotoSans-Regular.ttf");
const DEFAULT_FONT_ID: u64 = 0;

/// Font files past this size are refused: fontdue keeps every glyph outline
/// in memory, so a loaded font costs a few times its file size.
const FONT_FILE_LIMIT: usize = 64 * 1024 * 1024;

/// Rasterized glyphs are kept, across every font and size, until their
/// bitmaps add up to this many bytes; then the least recently drawn go.
const GLYPH_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// The largest text size, in logical pixels. A glyph is rasterized as one
/// bitmap, so an unbounded size could ask for gigabytes.
const MAX_TEXT_SIZE: f64 = 1000.0;

/// How many times `__ui_open` pumps the loop waiting for the window to be
/// created. macOS needs one pump to launch the application and another to
/// deliver `resumed`; everything else creates it on the first.
pub(crate) const OPEN_ATTEMPTS: usize = 8;

/// Presses of the same button count as one double (or triple) click when
/// each lands within this long and this many logical pixels of the last.
/// These are the Windows defaults; winit does not expose the user's setting.
const MULTI_CLICK_TIME: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: f64 = 4.0;

/// The cursor names `__ui_set_cursor` accepts, besides `"hidden"`.
const CURSORS: &[(&str, CursorIcon)] = &[
    ("default", CursorIcon::Default),
    ("pointer", CursorIcon::Pointer),
    ("text", CursorIcon::Text),
    ("grab", CursorIcon::Grab),
    ("grabbing", CursorIcon::Grabbing),
    ("crosshair", CursorIcon::Crosshair),
    ("move", CursorIcon::Move),
    ("notAllowed", CursorIcon::NotAllowed),
    ("wait", CursorIcon::Wait),
    ("progress", CursorIcon::Progress),
    ("help", CursorIcon::Help),
    ("copy", CursorIcon::Copy),
    ("contextMenu", CursorIcon::ContextMenu),
    ("zoomIn", CursorIcon::ZoomIn),
    ("zoomOut", CursorIcon::ZoomOut),
    ("ewResize", CursorIcon::EwResize),
    ("nsResize", CursorIcon::NsResize),
    ("neswResize", CursorIcon::NeswResize),
    ("nwseResize", CursorIcon::NwseResize),
    ("colResize", CursorIcon::ColResize),
    ("rowResize", CursorIcon::RowResize),
];

/// The edges and corners `__ui_drag_resize` accepts.
const RESIZE_DIRECTIONS: &[(&str, ResizeDirection)] = &[
    ("n", ResizeDirection::North),
    ("s", ResizeDirection::South),
    ("e", ResizeDirection::East),
    ("w", ResizeDirection::West),
    ("ne", ResizeDirection::NorthEast),
    ("nw", ResizeDirection::NorthWest),
    ("se", ResizeDirection::SouthEast),
    ("sw", ResizeDirection::SouthWest),
];

/// The only links `__ui_open_url` hands to the system: anything else (a file
/// path, `file:`, a custom protocol handler) could launch a program.
const URL_SCHEMES: &[&str] = &["http://", "https://", "mailto:"];

/// An open window: the OS handle, the surface frames are blitted to, and the
/// back buffer at physical size that the drawing natives paint into.
struct Win {
    window: Rc<Window>,
    surface: softbuffer::Surface<Rc<Window>, Rc<Window>>,
    pixmap: Pixmap,
    scale: f64,
    modifiers: ModifiersState,
    /// Last known cursor position in logical pixels, for button events,
    /// which winit reports without one.
    cursor: (f64, f64),
    /// Clips and open opacity groups of the frame being drawn.
    state: DrawState,
    /// Whether the window was opened `transparent`, so presenting keeps the
    /// alpha channel instead of dropping it.
    transparent: bool,
    /// The last maximized / minimized state reported, to turn what winit
    /// reports into `maximize`, `minimize` and `restore` events.
    maximized: bool,
    minimized: bool,
    clicks: Clicks,
    /// Whether the script enabled text input, so repeating the same call
    /// every frame does not reset the input method.
    ime_allowed: bool,
    /// An input method is composing, so the keys pressed are its business
    /// and must not also turn up as `textinput`.
    composing: bool,
}

/// Counts presses of the same button in quick succession, so `mousedown`
/// can report `clicks` = 2 for a double click.
#[derive(Default)]
struct Clicks {
    last: Option<(Instant, f64, f64, MouseButton)>,
    count: u32,
}

impl Clicks {
    fn press(&mut self, at: Instant, x: f64, y: f64, button: MouseButton) -> u32 {
        let continues = self.last.is_some_and(|(then, lx, ly, last)| {
            last == button
                && at.saturating_duration_since(then) <= MULTI_CLICK_TIME
                && (x - lx).abs() <= MULTI_CLICK_DISTANCE
                && (y - ly).abs() <= MULTI_CLICK_DISTANCE
        });
        self.count = if continues { self.count + 1 } else { 1 };
        self.last = Some((at, x, y, button));
        self.count
    }
}

/// What clipping and group opacity keep per drawing target. Clips are a
/// stack, each already intersected with the one below it, so the top one
/// is all a draw call needs.
#[derive(Default)]
struct DrawState {
    clips: Vec<Clip>,
    groups: Vec<Group>,
    /// Transparent layers left by closed groups, for the next ones to draw
    /// into instead of allocating (and zeroing) a surface every time.
    spare: Vec<Pixmap>,
}

/// An open `pushOpacity`: the pixels drawn before it, which the layer now
/// being drawn into is composited onto at `popOpacity`.
struct Group {
    saved: Pixmap,
    opacity: f32,
    /// How many clips were active when it opened; it closes back to this.
    clips: usize,
    /// The pixels of the layer drawn on so far. Only those are composited
    /// when the group closes, and cleared before the layer is reused.
    dirty: Bounds,
}

impl DrawState {
    fn clip(&self) -> Option<&Clip> {
        self.clips.last()
    }
}

/// How many spare layers a target keeps.
const SPARE_LAYERS: usize = 2;

/// An off-screen surface from `ui.canvas`, drawn into like a window.
struct Canvas {
    pixmap: Pixmap,
    scale: f64,
    state: DrawState,
    /// Whether every pixel is opaque, worked out when the canvas is drawn
    /// and forgotten whenever it is drawn on. An opaque canvas is copied
    /// instead of blended, which for a window-sized one is the difference
    /// between a memcpy and compositing a million pixels.
    opaque: Option<bool>,
}

impl Canvas {
    fn is_opaque(&mut self) -> bool {
        if self.opaque.is_none() {
            self.opaque = Some(images::is_opaque(&self.pixmap));
        }
        self.opaque == Some(true)
    }
}

/// What the drawing natives paint into, whether window or canvas.
struct Target<'a> {
    pixmap: &'a mut Pixmap,
    scale: f64,
    state: &'a mut DrawState,
}

impl Target<'_> {
    /// Notes that a draw may have changed the pixels of `area`, so the open
    /// opacity group (if any) composites them. Every drawing native calls
    /// it with an area that holds everything it painted.
    fn touch(&mut self, area: Bounds) {
        if let Some(group) = self.state.groups.last_mut() {
            let (w, h) = (self.pixmap.width(), self.pixmap.height());
            let area = clip::visible(self.state.clips.last(), area, w, h);
            group.dirty = group.dirty.union(&area);
        }
    }

    /// The pixels a path drawn through `scale` covers, grown by `pad`.
    fn path_area(&self, path: &Path, pad: f32) -> Bounds {
        let scale = self.scale as f32;
        let b = path.bounds();
        Bounds::covering(
            b.left() * scale,
            b.top() * scale,
            b.right() * scale,
            b.bottom() * scale,
            pad,
        )
    }

    /// Fills `path` (logical coordinates) through tiny-skia, clipped.
    fn fill_path(&mut self, path: &Path, paint: &Paint, rule: FillRule) {
        let area = self.path_area(path, 1.0);
        let scale = self.scale as f32;
        let (w, h) = (self.pixmap.width(), self.pixmap.height());
        if let Skia::Draw(mask) = clip::skia_clip(self.state.clips.last(), area, w, h) {
            self.pixmap
                .fill_path(path, paint, rule, Transform::from_scale(scale, scale), mask);
            self.touch(area);
        }
    }

    /// Strokes `path` (logical coordinates) through tiny-skia, clipped.
    fn stroke_path(&mut self, path: &Path, paint: &Paint, stroke: &Stroke) {
        let scale = self.scale as f32;
        // Miter joins reach out up to `miter_limit` half widths.
        let reach = stroke.width.max(1.0) * scale * stroke.miter_limit.max(1.0) / 2.0;
        let area = self.path_area(path, reach + 2.0);
        let (w, h) = (self.pixmap.width(), self.pixmap.height());
        if let Skia::Draw(mask) = clip::skia_clip(self.state.clips.last(), area, w, h) {
            self.pixmap.stroke_path(
                path,
                paint,
                stroke,
                Transform::from_scale(scale, scale),
                mask,
            );
            self.touch(area);
        }
    }
}

/// What `__ui_open` asks the event loop to create. Windows can only be made
/// from inside a winit callback, so the request is parked here and picked up
/// by `App::resumed` / `App::about_to_wait` during the next pump.
struct OpenRequest {
    title: String,
    width: f64,
    height: f64,
    resizable: bool,
    visible: bool,
    decorations: bool,
    min_width: f64,
    min_height: f64,
    transparent: bool,
    shadow: bool,
    icon: Option<Icon>,
}

/// The OS media session: the controls `souvlaki` registered for `window`,
/// the channel their commands arrive on, and the temporary PNG that holds
/// artwork drawn from an `Image` (the OS loads artwork from a file or URL).
struct MediaSession {
    controls: MediaControls,
    window: u64,
    commands: Receiver<MediaControlEvent>,
    artwork_file: Option<PathBuf>,
}

impl Drop for MediaSession {
    fn drop(&mut self) {
        if let Some(path) = self.artwork_file.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// How far a seek command without an amount (a "fast forward" button)
/// moves, in seconds.
const MEDIA_SEEK_STEP: f64 = 10.0;

thread_local! {
    static EVENT_LOOP: RefCell<Option<Result<EventLoop<()>, String>>> = const { RefCell::new(None) };
    static MEDIA: RefCell<Option<MediaSession>> = const { RefCell::new(None) };
    static WINDOWS: RefCell<HashMap<u64, Win>> = RefCell::new(HashMap::new());
    static WINDOW_IDS: RefCell<HashMap<WindowId, u64>> = RefCell::new(HashMap::new());
    static IMAGES: RefCell<HashMap<u64, ImageEntry>> = RefCell::new(HashMap::new());
    static CANVASES: RefCell<HashMap<u64, Canvas>> = RefCell::new(HashMap::new());
    static FONTS: RefCell<HashMap<u64, Rc<fontdue::Font>>> = RefCell::new(HashMap::new());
    static FALLBACKS: RefCell<Vec<u64>> = const { RefCell::new(Vec::new()) };
    static GLYPHS: RefCell<GlyphCache> = RefCell::new(GlyphCache::default());
    static LAYOUTS: RefCell<LayoutCache> = RefCell::new(LayoutCache::default());
    static SHADOWS: RefCell<ShadowCache> = RefCell::new(ShadowCache::default());
    static SYSTEM_FONTS: RefCell<Option<fontdb::Database>> = const { RefCell::new(None) };
    static EVENTS: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
    static PENDING_OPEN: RefCell<Option<OpenRequest>> = const { RefCell::new(None) };
    static OPENED: RefCell<Option<Result<(u64, f64), String>>> = const { RefCell::new(None) };
    static STATS: RefCell<Stats> = RefCell::new(Stats::default());
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

/// Frame timing for `__ui_stats`. `blur` collects the seconds spent in
/// gaussian blurs since the last present; the `_ms` fields describe the last
/// presented frame.
#[derive(Default)]
struct Stats {
    frames: u64,
    last_end: Option<Instant>,
    blur: f64,
    draw_ms: f64,
    blur_ms: f64,
    present_ms: f64,
    frame_ms: f64,
}

/// Runs a blur and charges its time to the current frame.
fn timed_blur(pixmap: &mut Pixmap, sigma: f32) {
    let start = Instant::now();
    effects::blur(pixmap, sigma);
    let spent = start.elapsed().as_secs_f64();
    STATS.with(|stats| stats.borrow_mut().blur += spent);
}

pub(crate) fn next_id() -> u64 {
    NEXT_ID.with(|id| {
        let value = id.get();
        id.set(value + 1);
        value
    })
}

// ARGUMENTS ----------------------------------------------------------------

pub(crate) fn string_arg(args: &[Value], index: usize, name: &str) -> Result<String, String> {
    match args.get(index) {
        Some(Value::String(value)) => Ok(value.to_string()),
        Some(value) => Err(format!(
            "{} must be a string, got {}",
            name,
            value.type_name()
        )),
        None => Err(format!("{} must be provided", name)),
    }
}

fn optional_string_arg(args: &[Value], index: usize, name: &str) -> Result<Option<String>, String> {
    match args.get(index) {
        None | Some(Value::Nil) => Ok(None),
        _ => string_arg(args, index, name).map(Some),
    }
}

pub(crate) fn bool_arg(
    args: &[Value],
    index: usize,
    name: &str,
    default: bool,
) -> Result<bool, String> {
    match args.get(index) {
        Some(Value::Bool(value)) => Ok(*value),
        None | Some(Value::Nil) => Ok(default),
        Some(value) => Err(format!(
            "{} must be a boolean, got {}",
            name,
            value.type_name()
        )),
    }
}

pub(crate) fn float_arg(args: &[Value], index: usize, name: &str) -> Result<f64, String> {
    let value = args.get(index).unwrap_or(&Value::Nil);
    if matches!(value, Value::Nil) {
        return Err(format!("{} must be provided", name));
    }
    let number =
        crate::vm::libs::args::number(value).map_err(|_| format!("{} must be a number", name))?;
    if !number.is_finite() {
        return Err(format!("{} must be a finite number", name));
    }
    Ok(number)
}

fn optional_float_arg(
    args: &[Value],
    index: usize,
    name: &str,
    default: f64,
) -> Result<f64, String> {
    match args.get(index) {
        None | Some(Value::Nil) => Ok(default),
        _ => float_arg(args, index, name),
    }
}

pub(crate) fn integer_arg(args: &[Value], index: usize, name: &str) -> Result<u64, String> {
    let number = float_arg(args, index, name)?;
    if number < 0.0 || number.fract() != 0.0 {
        return Err(format!("{} must be a non-negative integer", name));
    }
    Ok(number as u64)
}

/// An array of font ids.
fn font_ids_arg(args: &[Value], index: usize, name: &str) -> Result<Vec<u64>, String> {
    match args.get(index) {
        Some(Value::Table(table)) => {
            let ids = table.borrow().array.clone();
            (0..ids.len()).map(|i| integer_arg(&ids, i, name)).collect()
        }
        Some(value) => Err(format!(
            "{} must be a table, got {}",
            name,
            value.type_name()
        )),
        None => Err(format!("{} must be provided", name)),
    }
}

/// A color argument in any form `lib/ui.nyk` accepts: a packed
/// `0xRRGGBBAA` integer, a `"#rgb"`, `"#rgba"`, `"#rrggbb"` or
/// `"#rrggbbaa"` string, or an `{ r, g, b, a? }` table (also as `{ 1, 2,
/// 3, 4? }`) of 0-255 channels. Errors read as `packColor`'s do, so the
/// drawing methods can hand colors over without unpacking them first.
fn color_arg(args: &[Value], index: usize, name: &str) -> Result<(u8, u8, u8, u8), String> {
    let value = args.get(index).unwrap_or(&Value::Nil);
    match value {
        Value::Int(_) | Value::Float(_) | Value::BigInt(_) => {
            let packed = crate::vm::libs::args::number(value).unwrap_or(-1.0);
            if !(0.0..=u32::MAX as f64).contains(&packed) || packed.fract() != 0.0 {
                return Err(format!("{} must be a packed 0xRRGGBBAA integer", name));
            }
            Ok(unpack_color(packed as u32))
        }
        Value::String(text) => parse_color(text)
            .map(unpack_color)
            .ok_or_else(|| format!("{} must be a #rgb, #rrggbb or #rrggbbaa string", name)),
        Value::Table(table) => {
            let table = table.borrow();
            let channel = |key: &str, slot: usize, default: Option<f64>| {
                let named = table.get_str(key);
                let value = match named {
                    Value::Nil => table.array.get(slot).cloned().unwrap_or(Value::Nil),
                    other => other,
                };
                let number = match (&value, default) {
                    (Value::Nil, Some(default)) => Ok(default),
                    _ => crate::vm::libs::args::number(&value),
                };
                match number {
                    Ok(v) if (0.0..=255.0).contains(&v) => Ok((v + 0.5).floor() as u8),
                    _ => Err(format!("{}.{} must be a number from 0 to 255", name, key)),
                }
            };
            Ok((
                channel("r", 0, None)?,
                channel("g", 1, None)?,
                channel("b", 2, None)?,
                channel("a", 3, Some(255.0))?,
            ))
        }
        _ => Err(format!(
            "{} must be a color string or {{ r, g, b, a? }} table",
            name
        )),
    }
}

/// A `"#rgb"`, `"#rgba"`, `"#rrggbb"` or `"#rrggbbaa"` string as a packed
/// `0xRRGGBBAA` color.
fn parse_color(text: &str) -> Option<u32> {
    let hex = text.strip_prefix('#')?;
    let digit = |c: u8| (c as char).to_digit(16);
    let digits: Vec<u32> = hex.bytes().map(digit).collect::<Option<_>>()?;
    let value = |ds: &[u32]| ds.iter().fold(0u32, |acc, d| acc * 16 + d);
    match digits.len() {
        3 | 4 => {
            let doubled: Vec<u32> = digits.iter().flat_map(|&d| [d, d]).collect();
            let packed = value(&doubled);
            Some(if digits.len() == 3 {
                packed << 8 | 0xFF
            } else {
                packed
            })
        }
        6 => Some(value(&digits) << 8 | 0xFF),
        8 => Some(value(&digits)),
        _ => None,
    }
}

/// The optional line width of the stroking natives (`options.width` in
/// `lib/ui.nyk`), 1 when absent.
fn line_width_arg(args: &[Value], index: usize) -> Result<f64, String> {
    let width = optional_float_arg(args, index, "width", 1.0)?;
    if width <= 0.0 {
        return Err("width must be positive".to_string());
    }
    Ok(width)
}

/// The optional corner radius of the rectangle natives, 0 when absent.
fn corner_radius_arg(args: &[Value], index: usize) -> Result<f32, String> {
    let radius = optional_float_arg(args, index, "radius", 0.0)?;
    if radius < 0.0 {
        return Err("radius must be non-negative".to_string());
    }
    Ok(radius as f32)
}

fn unpack_color(packed: u32) -> (u8, u8, u8, u8) {
    (
        (packed >> 24) as u8,
        (packed >> 16) as u8,
        (packed >> 8) as u8,
        packed as u8,
    )
}

/// A timeout in seconds; nil, absent or zero means "do not wait".
fn timeout_arg(args: &[Value], index: usize, name: &str) -> Result<Duration, String> {
    let seconds = optional_float_arg(args, index, name, 0.0)?;
    if seconds < 0.0 {
        return Err(format!("{} must be a non-negative number of seconds", name));
    }
    Ok(Duration::from_secs_f64(seconds))
}

/// A flat `{x1, y1, x2, y2, ...}` array of coordinates.
fn points_arg(args: &[Value], index: usize, name: &str) -> Result<Vec<(f32, f32)>, String> {
    let numbers = match args.get(index) {
        Some(Value::Table(table)) => table
            .borrow()
            .array
            .iter()
            .map(|value| {
                crate::vm::libs::args::number(value)
                    .ok()
                    .filter(|number| number.is_finite())
                    .ok_or_else(|| format!("{} must hold finite numbers", name))
            })
            .collect::<Result<Vec<f64>, String>>()?,
        Some(value) => {
            return Err(format!(
                "{} must be a table, got {}",
                name,
                value.type_name()
            ));
        }
        None => return Err(format!("{} must be provided", name)),
    };
    if numbers.len() % 2 != 0 {
        return Err(format!("{} must hold x, y pairs", name));
    }
    Ok(numbers
        .chunks(2)
        .map(|pair| (pair[0] as f32, pair[1] as f32))
        .collect())
}

pub(crate) fn record(fields: Vec<(&str, Value)>) -> Value {
    let mut table = VmTable::new();
    for (name, value) in fields {
        table.fields.insert(StrRef::from(name), value);
    }
    Value::Table(Rc::new(RefCell::new(table)))
}

pub(crate) fn integer(value: u64) -> Value {
    Value::from_u64(value)
}

pub(crate) fn text(value: &str) -> Value {
    Value::str(value)
}

// EVENT LOOP ---------------------------------------------------------------

/// Creates the event loop on first use and caches the outcome, so a machine
/// without a display reports the same error from every native.
pub(crate) fn with_event_loop<T>(body: impl FnOnce(&mut EventLoop<()>) -> T) -> Result<T, String> {
    EVENT_LOOP.with(|cell| {
        let mut slot = cell.borrow_mut();
        let entry = slot.get_or_insert_with(create_event_loop);
        match entry {
            Ok(event_loop) => Ok(body(event_loop)),
            Err(err) => Err(err.clone()),
        }
    })
}

fn create_event_loop() -> Result<EventLoop<()>, String> {
    let mut builder = EventLoop::<()>::with_user_event();
    // The interpreter runs on the main thread, but `cargo test` drives the
    // `.nyk` suite from worker threads; the platforms that allow it get told
    // that is fine. macOS does not, and panics instead of returning an error.
    #[cfg(target_os = "linux")]
    {
        winit::platform::x11::EventLoopBuilderExtX11::with_any_thread(&mut builder, true);
        winit::platform::wayland::EventLoopBuilderExtWayland::with_any_thread(&mut builder, true);
    }
    #[cfg(target_os = "windows")]
    {
        winit::platform::windows::EventLoopBuilderExtWindows::with_any_thread(&mut builder, true);
    }
    let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| builder.build()));
    match built {
        Ok(Ok(event_loop)) => Ok(event_loop),
        Ok(Err(err)) => Err(format!(
            "no display available: {}",
            tidy_os_error(&err.to_string())
        )),
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown error".to_string());
            Err(format!("cannot start the UI event loop: {}", message))
        }
    }
}

/// winit prefixes OS errors with the source location they were raised at
/// ("os error at .../mod.rs:765: ..."), which means nothing to a script.
pub(crate) fn tidy_os_error(message: &str) -> String {
    match message.strip_prefix("os error at ") {
        Some(rest) => match rest.split_once(": ") {
            Some((_, detail)) => detail.to_string(),
            None => rest.to_string(),
        },
        None => message.to_string(),
    }
}

/// Pumps the OS loop once, waiting up to `timeout` for the first event.
pub(crate) fn pump(timeout: Duration) -> Result<(), String> {
    with_event_loop(|event_loop| {
        event_loop.pump_app_events(Some(timeout), &mut App);
    })
}

struct App;

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        create_pending(event_loop);
        webview::create_pending(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        create_pending(event_loop);
        webview::create_pending(event_loop);
    }

    fn window_event(&mut self, _: &ActiveEventLoop, window_id: WindowId, event: WindowEvent) {
        if webview::window_event(window_id, &event) {
            return;
        }
        let Some(id) = WINDOW_IDS.with(|ids| ids.borrow().get(&window_id).copied()) else {
            return;
        };
        WINDOWS.with(|windows| {
            let mut windows = windows.borrow_mut();
            let Some(win) = windows.get_mut(&id) else {
                return;
            };
            // A media key pressed while the window has focus. Once a media
            // session exists the OS routes the keys through it instead, so
            // only report them here when there is none, or a press would
            // arrive twice.
            let media_key = match &event {
                WindowEvent::KeyboardInput { event: key, .. }
                    if key.state == ElementState::Pressed
                        && !key.repeat
                        && MEDIA.with(|media| media.borrow().is_none()) =>
                {
                    media_key_name(&key.logical_key)
                }
                _ => None,
            };
            let mut out = Vec::new();
            translate_event(id, win, event, &mut out);
            if let Some(key) = media_key {
                out.push(media_key_event(id, key));
            }
            EVENTS.with(|events| events.borrow_mut().extend(out));
        });
    }
}

// MEDIA --------------------------------------------------------------------

/// The `key` of a `mediakey` event for a keyboard media key.
fn media_key_name(key: &Key) -> Option<&'static str> {
    match key {
        Key::Named(NamedKey::MediaPlayPause) => Some("playpause"),
        Key::Named(NamedKey::MediaPlay) => Some("play"),
        Key::Named(NamedKey::MediaPause) => Some("pause"),
        Key::Named(NamedKey::MediaTrackNext) => Some("next"),
        Key::Named(NamedKey::MediaTrackPrevious) => Some("previous"),
        Key::Named(NamedKey::MediaStop) => Some("stop"),
        _ => None,
    }
}

/// What a media session command means to a script.
#[derive(Debug, PartialEq)]
enum MediaCommand {
    /// A transport key or button: a `mediakey` event.
    Key(&'static str),
    /// Jump to a position in seconds: `mediasession` `seek` with `position`.
    SeekTo(f64),
    /// Move by seconds (negative is back): `mediasession` `seek` with `offset`.
    SeekBy(f64),
    /// Anything else the session asks for: `raise` or `quit`.
    Action(&'static str),
}

/// Sorts a session command into the event it becomes. Volume and open-URI
/// requests have no counterpart and are dropped.
fn media_command(event: &MediaControlEvent) -> Option<MediaCommand> {
    let signed = |direction: &SeekDirection, seconds: f64| match direction {
        SeekDirection::Forward => seconds,
        SeekDirection::Backward => -seconds,
    };
    Some(match event {
        MediaControlEvent::Play => MediaCommand::Key("play"),
        MediaControlEvent::Pause => MediaCommand::Key("pause"),
        MediaControlEvent::Toggle => MediaCommand::Key("playpause"),
        MediaControlEvent::Next => MediaCommand::Key("next"),
        MediaControlEvent::Previous => MediaCommand::Key("previous"),
        MediaControlEvent::Stop => MediaCommand::Key("stop"),
        MediaControlEvent::Seek(direction) => {
            MediaCommand::SeekBy(signed(direction, MEDIA_SEEK_STEP))
        }
        MediaControlEvent::SeekBy(direction, amount) => {
            MediaCommand::SeekBy(signed(direction, amount.as_secs_f64()))
        }
        MediaControlEvent::SetPosition(MediaPosition(position)) => {
            MediaCommand::SeekTo(position.as_secs_f64())
        }
        MediaControlEvent::Raise => MediaCommand::Action("raise"),
        MediaControlEvent::Quit => MediaCommand::Action("quit"),
        MediaControlEvent::SetVolume(_) | MediaControlEvent::OpenUri(_) => return None,
    })
}

fn media_key_event(window: u64, key: &str) -> Value {
    record(vec![
        ("kind", text("mediakey")),
        ("window", integer(window)),
        ("key", text(key)),
    ])
}

fn media_command_event(window: u64, command: MediaCommand) -> Value {
    let session = |action: &str, field: Option<(&'static str, f64)>| {
        let mut fields = vec![
            ("kind", text("mediasession")),
            ("window", integer(window)),
            ("action", text(action)),
        ];
        if let Some((name, value)) = field {
            fields.push((name, Value::Float(value)));
        }
        record(fields)
    };
    match command {
        MediaCommand::Key(key) => media_key_event(window, key),
        MediaCommand::SeekTo(position) => session("seek", Some(("position", position))),
        MediaCommand::SeekBy(offset) => session("seek", Some(("offset", offset))),
        MediaCommand::Action(action) => session(action, None),
    }
}

/// Moves the commands the session received since the last poll into the
/// event queue.
fn drain_media_commands() {
    MEDIA.with(|media| {
        let media = media.borrow();
        let Some(session) = media.as_ref() else {
            return;
        };
        for command in session.commands.try_iter() {
            if let Some(command) = media_command(&command) {
                let event = media_command_event(session.window, command);
                EVENTS.with(|events| events.borrow_mut().push(event));
            }
        }
    });
}

/// The name MPRIS registers the player under (`org.mpris.MediaPlayer2.` is
/// prepended): bus name elements allow `[A-Za-z0-9_]` and may not start
/// with a digit, and the `instance<pid>` suffix the spec suggests lets two
/// copies of one script run side by side.
fn dbus_name(title: &str, pid: u32) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let mut name = match cleaned.trim_matches('_') {
        "" => "neyuki".to_string(),
        trimmed => trimmed.to_string(),
    };
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert(0, '_');
    }
    format!("{}.instance{}", name, pid)
}

/// Whether an artwork string is a URL to hand over as is rather than a path.
fn is_artwork_url(artwork: &str) -> bool {
    let lower = artwork.to_ascii_lowercase();
    ["http://", "https://", "file://"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
}

/// A `file://` URL for an absolute path, in the form each backend reads:
/// souvlaki strips the scheme on Windows and opens the rest as a path, while
/// MPRIS and `NSURL` want a percent-encoded URI.
fn file_url(path: &str, windows: bool) -> String {
    if windows {
        return format!("file://{}", path);
    }
    let mut url = String::from("file://");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~".contains(&byte) {
            url.push(byte as char);
        } else {
            url.push_str(&format!("%{:02X}", byte));
        }
    }
    url
}

fn path_url(path: &FsPath) -> Result<String, String> {
    let absolute = std::path::absolute(path)
        .map_err(|err| format!("cannot read artwork {}: {}", path.display(), err))?;
    if !absolute.is_file() {
        return Err(format!(
            "cannot read artwork {}: no such file",
            path.display()
        ));
    }
    Ok(file_url(&absolute.to_string_lossy(), cfg!(windows)))
}

/// The native handle souvlaki needs to find a window's media controls; only
/// Windows asks for one.
#[cfg(target_os = "windows")]
fn native_window_handle(window: &Window) -> Option<*mut std::ffi::c_void> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as *mut std::ffi::c_void),
        _ => None,
    }
}

#[cfg(not(target_os = "windows"))]
fn native_window_handle(_window: &Window) -> Option<*mut std::ffi::c_void> {
    None
}

/// Registers media controls for window `id` and starts listening to them.
fn create_media_session(id: u64) -> Result<MediaSession, String> {
    let (title, hwnd) = with_window(id, |win| {
        Ok((win.window.title(), native_window_handle(&win.window)))
    })?;
    if cfg!(target_os = "windows") && hwnd.is_none() {
        return Err("cannot start media session: no native window handle".to_string());
    }
    let display_name = if title.is_empty() { "Neyuki" } else { &title };
    let bus_name = dbus_name(display_name, std::process::id());
    let mut controls = MediaControls::new(PlatformConfig {
        display_name,
        dbus_name: &bus_name,
        hwnd,
    })
    .map_err(|err| format!("cannot start media session: {:?}", err))?;
    let proxy = with_event_loop(|event_loop| event_loop.create_proxy())?;
    let (sender, commands) = mpsc::channel();
    controls
        .attach(move |command| {
            // Wake a `poll` that is waiting so the command arrives promptly.
            if sender.send(command).is_ok() {
                let _ = proxy.send_event(());
            }
        })
        .map_err(|err| format!("cannot start media session: {:?}", err))?;
    Ok(MediaSession {
        controls,
        window: id,
        commands,
        artwork_file: None,
    })
}

/// Writes an image to a fresh temporary PNG for the OS to load as artwork.
fn write_artwork_image(image: u64) -> Result<PathBuf, String> {
    let bytes = IMAGES.with(|images| {
        let images = images.borrow();
        let pixmap = &images
            .get(&image)
            .ok_or_else(|| "image was freed".to_string())?
            .frames[0]
            .pixmap;
        pixmap
            .encode_png()
            .map_err(|err| format!("cannot encode artwork: {}", err))
    })?;
    let path = std::env::temp_dir().join(format!(
        "neyuki-artwork-{}-{}.png",
        std::process::id(),
        next_id()
    ));
    std::fs::write(&path, bytes).map_err(|err| format!("cannot write artwork: {}", err))?;
    Ok(path)
}

/// What a script asked `ui.mediaSession` to show.
struct NowPlaying {
    title: String,
    artist: String,
    album: String,
    artwork: Option<String>,
    artwork_image: Option<u64>,
    duration: Option<f64>,
    position: Option<f64>,
    playing: bool,
}

/// The artwork URL to publish, plus the temporary file behind it when it
/// was drawn from an `Image`.
fn prepare_artwork(info: &NowPlaying) -> Result<(Option<String>, Option<PathBuf>), String> {
    Ok(match (&info.artwork_image, &info.artwork) {
        (Some(image), _) => {
            let path = write_artwork_image(*image)?;
            let url = file_url(&path.to_string_lossy(), cfg!(windows));
            (Some(url), Some(path))
        }
        (None, Some(artwork)) if is_artwork_url(artwork) => (Some(artwork.clone()), None),
        (None, Some(artwork)) => (Some(path_url(FsPath::new(artwork))?), None),
        (None, None) => (None, None),
    })
}

/// Publishes `info`. Errors here come from the OS (no D-Bus session, say)
/// rather than from the script.
fn update_media_session(
    session: &mut MediaSession,
    info: &NowPlaying,
    cover_url: Option<String>,
    new_file: Option<PathBuf>,
) -> Result<(), String> {
    // Empty strings rather than `None`, which souvlaki takes as "keep what
    // was there", so a field left out of an update really is cleared.
    let metadata = MediaMetadata {
        title: Some(info.title.as_str()),
        artist: Some(info.artist.as_str()),
        album: Some(info.album.as_str()),
        cover_url: cover_url.as_deref(),
        duration: info.duration.map(Duration::from_secs_f64),
    };
    let result = session
        .controls
        .set_metadata(metadata)
        .and_then(|_| {
            let progress = info
                .position
                .map(|seconds| MediaPosition(Duration::from_secs_f64(seconds)));
            session.controls.set_playback(if info.playing {
                MediaPlayback::Playing { progress }
            } else {
                MediaPlayback::Paused { progress }
            })
        })
        .map_err(|err| format!("cannot update media session: {:?}", err));
    // The previous artwork file has been replaced (or the update failed and
    // the new one is not needed); either way one of them can go.
    let stale = match new_file {
        Some(file) if result.is_ok() => session.artwork_file.replace(file),
        unused => unused,
    };
    if let Some(stale) = stale {
        let _ = std::fs::remove_file(stale);
    }
    result
}

/// Fulfils a parked `__ui_open` request, if there is one.
fn create_pending(event_loop: &ActiveEventLoop) {
    let Some(request) = PENDING_OPEN.with(|pending| pending.borrow_mut().take()) else {
        return;
    };
    let result = create_window(event_loop, request);
    OPENED.with(|opened| *opened.borrow_mut() = Some(result));
}

fn create_window(event_loop: &ActiveEventLoop, request: OpenRequest) -> Result<(u64, f64), String> {
    let icon = request.icon.or_else(executable_icon);
    #[allow(unused_mut)]
    let mut attributes = Window::default_attributes()
        .with_title(&request.title)
        .with_inner_size(LogicalSize::new(request.width, request.height))
        .with_resizable(request.resizable)
        .with_visible(request.visible)
        .with_decorations(request.decorations)
        .with_transparent(request.transparent)
        .with_window_icon(icon.clone());
    #[cfg(windows)]
    {
        use winit::platform::windows::WindowAttributesExtWindows;
        attributes = attributes.with_taskbar_icon(icon);
    }
    if request.min_width > 0.0 || request.min_height > 0.0 {
        attributes =
            attributes.with_min_inner_size(LogicalSize::new(request.min_width, request.min_height));
    }
    // A window without decorations loses its drop shadow on Windows unless
    // asked to keep it; macOS keeps one by default.
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::WindowAttributesExtWindows;
        attributes = attributes.with_undecorated_shadow(request.shadow);
    }
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::WindowAttributesExtMacOS;
        attributes = attributes.with_has_shadow(request.shadow);
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = request.shadow;
    let window = Rc::new(
        event_loop
            .create_window(attributes)
            .map_err(|err| format!("cannot create window: {}", tidy_os_error(&err.to_string())))?,
    );
    let context = softbuffer::Context::new(window.clone())
        .map_err(|err| format!("cannot create window: {}", err))?;
    let surface = softbuffer::Surface::new(&context, window.clone())
        .map_err(|err| format!("cannot create window: {}", err))?;
    let scale = window.scale_factor();
    let size = window.inner_size();
    let mut pixmap = Pixmap::new(size.width.max(1), size.height.max(1))
        .ok_or_else(|| "cannot create window: back buffer too large".to_string())?;
    pixmap.fill(blank(request.transparent));

    let id = next_id();
    let mut win = Win {
        window: window.clone(),
        surface,
        pixmap,
        scale,
        modifiers: ModifiersState::empty(),
        cursor: (0.0, 0.0),
        state: DrawState::default(),
        transparent: request.transparent,
        maximized: window.is_maximized(),
        minimized: false,
        clicks: Clicks::default(),
        ime_allowed: false,
        composing: false,
    };
    // Wayland only maps a window once it has a frame, so show the blank
    // canvas straight away; a failure here just means the first real
    // `present` does it.
    let _ = blit(&mut win, None);
    WINDOW_IDS.with(|ids| ids.borrow_mut().insert(window.id(), id));
    WINDOWS.with(|windows| windows.borrow_mut().insert(id, win));
    Ok((id, scale))
}

/// Turns a winit event into the tables `__ui_poll` returns (none, one or a
/// few), updating the window's bookkeeping (size, DPI, modifiers, cursor,
/// clicks, input method) along the way.
fn translate_event(id: u64, win: &mut Win, event: WindowEvent, out: &mut Vec<Value>) {
    let mut push =
        |kind: &str, fields: Vec<(&str, Value)>| out.push(event_record(id, kind, fields));
    match event {
        WindowEvent::CloseRequested => push("close", vec![]),
        WindowEvent::Resized(size) => {
            if let Some(pixmap) = Pixmap::new(size.width.max(1), size.height.max(1)) {
                let mut pixmap = pixmap;
                pixmap.fill(blank(win.transparent));
                win.pixmap = pixmap;
                win.state = DrawState::default();
            }
            let size = win.window.inner_size().to_logical::<f64>(win.scale);
            push(
                "resize",
                vec![
                    ("width", Value::Float(size.width)),
                    ("height", Value::Float(size.height)),
                ],
            );
            if let Some(kind) = take_state_change(win) {
                push(kind, vec![]);
            }
        }
        WindowEvent::Moved(_) | WindowEvent::Occluded(_) => {
            if let Some(kind) = take_state_change(win) {
                push(kind, vec![]);
            }
        }
        WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
            win.scale = scale_factor;
            push("dpichange", vec![("scale", Value::Float(scale_factor))]);
        }
        WindowEvent::Focused(focused) => {
            push(if focused { "focus" } else { "blur" }, vec![]);
            if let Some(kind) = take_state_change(win) {
                push(kind, vec![]);
            }
        }
        WindowEvent::RedrawRequested => {
            let _ = blit(win, None);
            push("redraw", vec![]);
        }
        WindowEvent::ModifiersChanged(modifiers) => {
            win.modifiers = modifiers.state();
        }
        WindowEvent::KeyboardInput { event, .. } => {
            let name = match &event.logical_key {
                Key::Named(named) => format!("{:?}", named).to_lowercase(),
                Key::Character(s) => s.to_lowercase(),
                Key::Unidentified(_) => "unidentified".to_string(),
                Key::Dead(_) => "dead".to_string(),
            };
            let code = match event.physical_key {
                PhysicalKey::Code(code) => format!("{:?}", code),
                PhysicalKey::Unidentified(_) => "Unidentified".to_string(),
            };
            let typed = event
                .text
                .as_ref()
                .map(|s| s.to_string())
                .filter(|s| !s.chars().all(char::is_control));
            let mods = win.modifiers;
            let pressed = event.state == ElementState::Pressed;
            push(
                if pressed { "keydown" } else { "keyup" },
                vec![
                    ("key", Value::string(name)),
                    ("code", Value::string(code)),
                    ("repeat", Value::Bool(event.repeat)),
                    (
                        "text",
                        typed.clone().map(Value::string).unwrap_or(Value::Nil),
                    ),
                    (
                        "mods",
                        record(vec![
                            ("shift", Value::Bool(mods.shift_key())),
                            ("ctrl", Value::Bool(mods.control_key())),
                            ("alt", Value::Bool(mods.alt_key())),
                            ("meta", Value::Bool(mods.super_key())),
                        ]),
                    ),
                ],
            );
            // Shortcuts are not typing, even where the layout gives them text.
            let shortcut = mods.control_key() || mods.super_key();
            if let Some(typed) = typed.filter(|_| pressed && !shortcut && !win.composing) {
                push("textinput", vec![("text", Value::string(typed))]);
            }
        }
        WindowEvent::Ime(ime) => {
            let (phase, typed, cursor) = ime_fields(&ime);
            win.composing = phase == "preedit" && !typed.is_empty();
            let committed = if phase == "commit" && !typed.is_empty() {
                Some(typed.clone())
            } else {
                None
            };
            push(
                "ime",
                vec![
                    ("phase", text(phase)),
                    ("text", Value::string(typed)),
                    (
                        "cursor",
                        cursor.map(|c| integer(c as u64)).unwrap_or(Value::Nil),
                    ),
                ],
            );
            if let Some(committed) = committed {
                push("textinput", vec![("text", Value::string(committed))]);
            }
        }
        WindowEvent::DroppedFile(path) => {
            push(
                "dropfile",
                vec![("path", Value::string(path.to_string_lossy().into_owned()))],
            );
        }
        WindowEvent::CursorMoved { position, .. } => {
            let position = position.to_logical::<f64>(win.scale);
            win.cursor = (position.x, position.y);
            push(
                "mousemove",
                vec![
                    ("x", Value::Float(position.x)),
                    ("y", Value::Float(position.y)),
                ],
            );
        }
        WindowEvent::CursorEntered { .. } => push("mouseenter", vec![]),
        WindowEvent::CursorLeft { .. } => push("mouseleave", vec![]),
        WindowEvent::MouseInput { state, button, .. } => {
            let name = match button {
                MouseButton::Left => "left".to_string(),
                MouseButton::Right => "right".to_string(),
                MouseButton::Middle => "middle".to_string(),
                MouseButton::Back => "back".to_string(),
                MouseButton::Forward => "forward".to_string(),
                MouseButton::Other(n) => format!("button{}", n),
            };
            let mut fields = vec![
                ("button", Value::string(name)),
                ("x", Value::Float(win.cursor.0)),
                ("y", Value::Float(win.cursor.1)),
            ];
            let kind = match state {
                ElementState::Pressed => {
                    let (x, y) = win.cursor;
                    let clicks = win.clicks.press(Instant::now(), x, y, button);
                    fields.push(("clicks", integer(clicks as u64)));
                    "mousedown"
                }
                ElementState::Released => "mouseup",
            };
            push(kind, fields);
        }
        WindowEvent::MouseWheel { delta, .. } => {
            let (dx, dy, unit) = match delta {
                MouseScrollDelta::LineDelta(x, y) => (x as f64, y as f64, "lines"),
                MouseScrollDelta::PixelDelta(position) => {
                    let position = position.to_logical::<f64>(win.scale);
                    (position.x, position.y, "pixels")
                }
            };
            push(
                "scroll",
                vec![
                    ("dx", Value::Float(dx)),
                    ("dy", Value::Float(dy)),
                    ("unit", text(unit)),
                    ("x", Value::Float(win.cursor.0)),
                    ("y", Value::Float(win.cursor.1)),
                ],
            );
        }
        _ => {}
    }
}

/// An event table: `kind` and `window` first, then the kind's own fields.
fn event_record(id: u64, kind: &str, mut fields: Vec<(&str, Value)>) -> Value {
    fields.insert(0, ("kind", text(kind)));
    fields.insert(1, ("window", integer(id)));
    record(fields)
}

/// Compares the window's maximized / minimized state with the last one
/// reported and returns the kind of event that describes the change. winit
/// has no event of its own for these; they show up as resizes and moves.
fn take_state_change(win: &mut Win) -> Option<&'static str> {
    let now = (
        win.window.is_maximized(),
        win.window.is_minimized().unwrap_or(false),
    );
    let kind = state_change((win.maximized, win.minimized), now);
    (win.maximized, win.minimized) = now;
    kind
}

/// The event for going from one `(maximized, minimized)` state to another.
fn state_change(was: (bool, bool), now: (bool, bool)) -> Option<&'static str> {
    let (was_max, was_min) = was;
    let (max, min) = now;
    if min && !was_min {
        Some("minimize")
    } else if max && !was_max && !min {
        Some("maximize")
    } else if (was_min && !min) || (was_max && !max) {
        Some("restore")
    } else {
        None
    }
}

/// The `phase`, `text` and `cursor` of an `ime` event. The cursor is the
/// byte offset into the preedit text, absent when the input method hides it.
fn ime_fields(ime: &Ime) -> (&'static str, String, Option<usize>) {
    match ime {
        Ime::Enabled => ("start", String::new(), None),
        Ime::Preedit(text, cursor) => ("preedit", text.clone(), cursor.map(|(start, _)| start)),
        Ime::Commit(text) => ("commit", text.clone(), None),
        Ime::Disabled => ("end", String::new(), None),
    }
}

/// What a fresh or resized back buffer is filled with: white, or nothing
/// at all for a transparent window.
fn blank(transparent: bool) -> Color {
    if transparent {
        Color::TRANSPARENT
    } else {
        Color::WHITE
    }
}

// WINDOWS ------------------------------------------------------------------

/// Runs `body` on window `id`. Natives cannot call back into Neyuki, so
/// nothing can reach for the map while the window is borrowed.
fn with_window<T>(id: u64, body: impl FnOnce(&mut Win) -> Result<T, String>) -> Result<T, String> {
    WINDOWS.with(|windows| {
        let mut windows = windows.borrow_mut();
        let win = windows
            .get_mut(&id)
            .ok_or_else(|| "window is closed".to_string())?;
        body(win)
    })
}

/// Runs `body` on the window or canvas `id`, whichever it names.
fn with_target<T>(
    id: u64,
    body: impl FnOnce(&mut Target) -> Result<T, String>,
) -> Result<T, String> {
    if WINDOWS.with(|windows| windows.borrow().contains_key(&id)) {
        return with_window(id, |win| {
            body(&mut Target {
                pixmap: &mut win.pixmap,
                scale: win.scale,
                state: &mut win.state,
            })
        });
    }
    CANVASES.with(|canvases| {
        let mut canvases = canvases.borrow_mut();
        let canvas = canvases
            .get_mut(&id)
            .ok_or_else(|| "window is closed".to_string())?;
        canvas.opaque = None;
        body(&mut Target {
            pixmap: &mut canvas.pixmap,
            scale: canvas.scale,
            state: &mut canvas.state,
        })
    })
}

/// Copies the back buffer to the screen. softbuffer wants `0RGB` words;
/// the pixmap is premultiplied, which over an opaque window is exactly the
/// composited color. A transparent window also gets the alpha in the top
/// byte, premultiplied as compositors expect; backends that read only
/// `0RGB` ignore it.
fn blit(win: &mut Win, dirty: Option<&[(i32, i32, i32, i32)]>) -> Result<(), String> {
    let (width, height) = (win.pixmap.width(), win.pixmap.height());
    let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) else {
        return Ok(());
    };
    win.surface
        .resize(w, h)
        .map_err(|err| format!("cannot present window: {}", err))?;
    let mut buffer = win
        .surface
        .buffer_mut()
        .map_err(|err| format!("cannot present window: {}", err))?;
    // Only a buffer that still holds the previous frame can be updated in
    // part; any other age is stale or new and needs every pixel.
    let rects = match dirty {
        Some(rects) if buffer.age() == 1 => clamp_rects(rects, width, height),
        _ => None,
    };
    match rects {
        Some(rects) => {
            let stride = width as usize;
            let pixels = win.pixmap.data();
            let mut damage = Vec::with_capacity(rects.len());
            for (x, y, rw, rh) in rects {
                for row in y..y + rh {
                    let from = row as usize * stride + x as usize;
                    let to = from + rw as usize;
                    pack_pixels(
                        &pixels[from * 4..to * 4],
                        &mut buffer[from..to],
                        win.transparent,
                    );
                }
                if let (Some(rw), Some(rh)) = (NonZeroU32::new(rw), NonZeroU32::new(rh)) {
                    damage.push(softbuffer::Rect {
                        x,
                        y,
                        width: rw,
                        height: rh,
                    });
                }
            }
            buffer
                .present_with_damage(&damage)
                .map_err(|err| format!("cannot present window: {}", err))
        }
        None => {
            pack_pixels(win.pixmap.data(), &mut buffer, win.transparent);
            buffer
                .present()
                .map_err(|err| format!("cannot present window: {}", err))
        }
    }
}

/// Clips physical `(x, y, w, h)` rectangles to a `width` x `height` surface,
/// dropping empty ones. `None` when nothing is left, which presents the
/// whole frame instead.
fn clamp_rects(
    rects: &[(i32, i32, i32, i32)],
    width: u32,
    height: u32,
) -> Option<Vec<(u32, u32, u32, u32)>> {
    let mut out = Vec::with_capacity(rects.len());
    for &(x, y, w, h) in rects {
        let x0 = (x as i64).max(0);
        let y0 = (y as i64).max(0);
        let x1 = (x as i64 + w as i64).min(width as i64);
        let y1 = (y as i64 + h as i64).min(height as i64);
        if x1 > x0 && y1 > y0 {
            out.push((x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32));
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Turns RGBA bytes into the `ARGB` (or `0RGB`) words softbuffer takes:
/// one swap of red and blue per word, which the compiler vectorizes.
fn pack_pixels(pixels: &[u8], out: &mut [u32], alpha: bool) {
    let (pixels, _) = pixels.as_chunks::<4>();
    let keep = if alpha { 0xFF00_FF00 } else { 0x0000_FF00 };
    for (pixel, word) in pixels.iter().zip(out.iter_mut()) {
        let v = u32::from_le_bytes(*pixel);
        *word = (v & keep) | ((v & 0xFF) << 16) | ((v >> 16) & 0xFF);
    }
}

/// A window icon from a decoded image; winit wants straight (not
/// premultiplied) RGBA.
fn icon_from_pixmap(pixmap: &Pixmap) -> Result<Icon, String> {
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|pixel| {
            let color = pixel.demultiply();
            [color.red(), color.green(), color.blue(), color.alpha()]
        })
        .collect();
    Icon::from_rgba(rgba, pixmap.width(), pixmap.height())
        .map_err(|err| format!("cannot use the image as an icon: {}", err))
}

/// The icon for image `id`, or nil for none.
fn icon_arg(args: &[Value], index: usize) -> Result<Option<Icon>, String> {
    if matches!(args.get(index), None | Some(Value::Nil)) {
        return Ok(None);
    }
    let image = integer_arg(args, index, "icon")?;
    IMAGES.with(|images| {
        let images = images.borrow();
        let pixmap = &images
            .get(&image)
            .ok_or_else(|| "image was freed".to_string())?
            .frames[0]
            .pixmap;
        icon_from_pixmap(pixmap).map(Some)
    })
}

/// Maps a `setCursor` name to a winit cursor; `None` means hide it.
fn cursor_icon(name: &str) -> Result<Option<CursorIcon>, String> {
    if name == "hidden" {
        return Ok(None);
    }
    CURSORS
        .iter()
        .find(|(known, _)| *known == name)
        .map(|(_, icon)| Some(*icon))
        .ok_or_else(|| {
            let names: Vec<&str> = CURSORS.iter().map(|(known, _)| *known).collect();
            format!(
                "unknown cursor '{}' (expected {} or hidden)",
                name,
                names.join(", ")
            )
        })
}

/// Maps `"n"`, `"se"` and so on to the edge or corner to resize from.
fn resize_direction(name: &str) -> Result<ResizeDirection, String> {
    RESIZE_DIRECTIONS
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(name))
        .map(|(_, direction)| *direction)
        .ok_or_else(|| {
            format!(
                "unknown resize direction '{}' (expected n, s, e, w, ne, nw, se or sw)",
                name
            )
        })
}

/// Checks that `url` is a web or mail link before it reaches the system's
/// URL handler, which would otherwise happily run a program or open a file.
fn check_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    let scheme = URL_SCHEMES
        .iter()
        .find(|scheme| lower.starts_with(**scheme))
        .ok_or_else(|| "url must start with http://, https:// or mailto:".to_string())?;
    if url.len() == scheme.len() {
        return Err("url has nothing after the scheme".to_string());
    }
    if url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("url must not contain spaces or control characters".to_string());
    }
    Ok(())
}

fn paint(color: (u8, u8, u8, u8)) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.0, color.1, color.2, color.3);
    paint.anti_alias = true;
    paint
}

fn stroke(width: f64) -> Stroke {
    Stroke {
        width: width as f32,
        ..Stroke::default()
    }
}

/// A stroke with the given line cap (`"butt"`, `"round"` or `"square"`) and
/// join (`"miter"`, `"round"` or `"bevel"`).
fn styled_stroke(width: f64, cap: &str, join: &str) -> Result<Stroke, String> {
    let line_cap = match cap {
        "butt" => LineCap::Butt,
        "round" => LineCap::Round,
        "square" => LineCap::Square,
        _ => return Err("cap must be \"butt\", \"round\" or \"square\"".to_string()),
    };
    let line_join = match join {
        "miter" => LineJoin::Miter,
        "round" => LineJoin::Round,
        "bevel" => LineJoin::Bevel,
        _ => return Err("join must be \"miter\", \"round\" or \"bevel\"".to_string()),
    };
    Ok(Stroke {
        width: width as f32,
        line_cap,
        line_join,
        ..Stroke::default()
    })
}

/// An optional table of finite numbers, such as a gradient or shadow spec.
fn numbers_arg(args: &[Value], index: usize, name: &str) -> Result<Option<Vec<f64>>, String> {
    match args.get(index) {
        None | Some(Value::Nil) => Ok(None),
        Some(Value::Table(table)) => table
            .borrow()
            .array
            .iter()
            .map(|value| {
                crate::vm::libs::args::number(value)
                    .ok()
                    .filter(|number| number.is_finite())
                    .ok_or_else(|| format!("{} must hold finite numbers", name))
            })
            .collect::<Result<Vec<f64>, String>>()
            .map(Some),
        Some(value) => Err(format!(
            "{} must be a table, got {}",
            name,
            value.type_name()
        )),
    }
}

/// A path from the flat command list `lib/ui.nyk` builds: `0 x y` moves,
/// `1 x y` draws a line, `2 cx cy x y` a quadratic curve, `3 c1x c1y c2x
/// c2y x y` a cubic curve and `4` closes the sub-path.
fn path_from_commands(commands: &[f64]) -> Result<Path, String> {
    let mut pb = PathBuilder::new();
    let mut i = 0;
    while i < commands.len() {
        let count = match commands[i] as i32 {
            0 | 1 => 2,
            2 => 4,
            3 => 6,
            4 => 0,
            _ => return Err("path holds an unknown command".to_string()),
        };
        let Some(a) = commands.get(i + 1..i + 1 + count) else {
            return Err("path ends in the middle of a command".to_string());
        };
        let f = |k: usize| a[k] as f32;
        match commands[i] as i32 {
            0 => pb.move_to(f(0), f(1)),
            1 => pb.line_to(f(0), f(1)),
            2 => pb.quad_to(f(0), f(1), f(2), f(3)),
            3 => pb.cubic_to(f(0), f(1), f(2), f(3), f(4), f(5)),
            _ => pb.close(),
        }
        i += 1 + count;
    }
    pb.finish().ok_or_else(|| "path is empty".to_string())
}

fn path_arg(args: &[Value], index: usize) -> Result<Path, String> {
    let commands = numbers_arg(args, index, "path")?.ok_or("path must be provided")?;
    path_from_commands(&commands)
}

/// The shader of a gradient spec: `kind` (0 linear, 1 radial), four numbers
/// (`x1, y1, x2, y2` of a linear gradient; `cx, cy, radius` and a spare of a
/// radial one), then `offset, color` pairs. Coordinates are logical.
fn gradient_shader(spec: &[f64]) -> Result<Shader<'static>, String> {
    if spec.len() < 9 || !(spec.len() - 5).is_multiple_of(2) {
        return Err("gradient needs at least two stops".to_string());
    }
    let stops = spec[5..]
        .chunks(2)
        .map(|pair| {
            let (r, g, b, a) = unpack_color(pair[1] as u32);
            GradientStop::new(pair[0] as f32, Color::from_rgba8(r, g, b, a))
        })
        .collect();
    let point = |x: f64, y: f64| Point::from_xy(x as f32, y as f32);
    let shader = if spec[0] == 0.0 {
        LinearGradient::new(
            point(spec[1], spec[2]),
            point(spec[3], spec[4]),
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        )
    } else {
        let center = point(spec[1], spec[2]);
        RadialGradient::new(
            center,
            center,
            spec[3] as f32,
            stops,
            SpreadMode::Pad,
            Transform::identity(),
        )
    };
    shader.ok_or_else(|| "gradient is degenerate (zero length or radius)".to_string())
}

/// Fills `path` (logical coordinates) with `color` or the `gradient`, after
/// painting the `shadow` it casts.
fn fill_shape(
    target: &mut Target,
    path: &Path,
    color: (u8, u8, u8, u8),
    gradient: Option<&[f64]>,
    shadow: Option<&[f64]>,
) -> Result<(), String> {
    if let Some(shadow) = shadow {
        draw_shadow(target, path, shadow)?;
    }
    let mut paint = paint(color);
    if let Some(gradient) = gradient {
        paint.shader = gradient_shader(gradient)?;
    }
    target.fill_path(path, &paint, FillRule::Winding);
    Ok(())
}

/// What a cached shadow is keyed by: the shape relative to its own bounds,
/// where those bounds fall inside a pixel (in eighths), and the blur,
/// spread and scale. The color is applied when it is drawn, so a shadow
/// fading in or out still hits the cache.
#[derive(PartialEq, Eq, Hash)]
struct ShadowKey {
    verbs: Vec<u8>,
    points: Vec<u32>,
    offset: (u32, u32),
    sigma: u32,
    spread: u32,
    scale: u32,
}

/// Blurred shadow coverage masks, kept because the same cards, popovers and
/// knobs cast the same shadows every frame. Bounded by bytes; when full it
/// starts over.
#[derive(Default)]
struct ShadowCache {
    masks: HashMap<ShadowKey, Rc<Mask>>,
    bytes: usize,
}

const SHADOW_CACHE_BYTES: usize = 32 * 1024 * 1024;

/// Paints the blurred shadow of `path`. `spec` is `blur, offsetX, offsetY,
/// spread, color`: `blur` is the CSS-style blur radius (the gaussian's
/// standard deviation is half of it), `spread` grows the shape first.
///
/// The blurred coverage is cached per shape and drawn in the shadow's
/// color, so a shadow costs a blur only the first time it is cast.
fn draw_shadow(target: &mut Target, path: &Path, spec: &[f64]) -> Result<(), String> {
    if spec.len() < 5 {
        return Err("shadow needs blur, offsetX, offsetY, spread and color".to_string());
    }
    let blur = spec[0].max(0.0) as f32;
    let (dx, dy) = (spec[1] as f32, spec[2] as f32);
    let spread = spec[3].max(0.0) as f32;
    let (r, g, b, a) = unpack_color(spec[4] as u32);
    if a == 0 {
        return Ok(());
    }
    let scale = target.scale as f32;
    let sigma = blur * scale / 2.0;
    let pad = (sigma * 3.0).ceil() + 1.0 + spread * scale;
    let bounds = path.bounds();
    // The mask's top-left pixel, and where the shape's bounds start in it.
    let (ox, oy) = (
        (bounds.left() + dx) * scale - pad,
        (bounds.top() + dy) * scale - pad,
    );
    if !ox.is_finite() || !oy.is_finite() {
        return Ok(());
    }
    let (left, top) = (ox.floor(), oy.floor());
    let eighths = |v: f32| ((v * 8.0).round() as u32).min(7);
    let offset = (eighths(ox - left), eighths(oy - top));
    let width = (bounds.width() * scale + pad * 2.0 + 1.0).ceil().max(1.0);
    let height = (bounds.height() * scale + pad * 2.0 + 1.0).ceil().max(1.0);
    if width > u32::MAX as f32 || height > u32::MAX as f32 {
        return Err("shadow is too large".to_string());
    }
    let (width, height) = (width as u32, height as u32);
    let (left, top) = (
        left.clamp(-(1 << 30) as f32, (1 << 30) as f32) as i32,
        top.clamp(-(1 << 30) as f32, (1 << 30) as f32) as i32,
    );
    let area = Bounds::from_xywh(left, top, width, height);
    let (pw, ph) = (target.pixmap.width(), target.pixmap.height());
    if clip::visible(target.state.clip(), area, pw, ph).is_empty() {
        return Ok(());
    }
    if images::check_size(width, height, Limits::default()).is_err() {
        return Err("shadow is too large".to_string());
    }
    let key = ShadowKey {
        verbs: path.verbs().iter().map(|verb| *verb as u8).collect(),
        points: path
            .points()
            .iter()
            .flat_map(|p| {
                [
                    (p.x - bounds.left()).to_bits(),
                    (p.y - bounds.top()).to_bits(),
                ]
            })
            .collect(),
        offset,
        sigma: sigma.to_bits(),
        spread: spread.to_bits(),
        scale: scale.to_bits(),
    };
    let cached = SHADOWS.with(|cache| cache.borrow().masks.get(&key).cloned());
    let mask = match cached {
        Some(mask) => mask,
        None => {
            let mut layer =
                Pixmap::new(width, height).ok_or_else(|| "cannot allocate shadow".to_string())?;
            let transform = Transform::from_row(
                scale,
                0.0,
                0.0,
                scale,
                pad + offset.0 as f32 / 8.0 - bounds.left() * scale,
                pad + offset.1 as f32 / 8.0 - bounds.top() * scale,
            );
            let solid = paint((0, 0, 0, 255));
            layer.fill_path(path, &solid, FillRule::Winding, transform, None);
            if spread > 0.0 {
                layer.stroke_path(path, &solid, &stroke(spread as f64 * 2.0), transform, None);
            }
            timed_blur(&mut layer, sigma);
            let mask = Rc::new(Mask::from_pixmap(layer.as_ref(), MaskType::Alpha));
            SHADOWS.with(|cache| {
                let mut cache = cache.borrow_mut();
                let cost = width as usize * height as usize + 64;
                if cache.bytes + cost > SHADOW_CACHE_BYTES {
                    cache.masks.clear();
                    cache.bytes = 0;
                }
                cache.bytes += cost;
                cache.masks.insert(key, mask.clone());
            });
            mask
        }
    };
    let premultiply = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
    images::fill_solid(
        target.pixmap,
        [premultiply(r), premultiply(g), premultiply(b), a],
        left,
        top,
        width,
        height,
        Some(&mask),
        target.state.clip(),
    );
    target.touch(area);
    Ok(())
}

/// The deepest clip and opacity nesting a frame may use.
const MAX_CLIPS: usize = 64;
const MAX_GROUPS: usize = 16;

/// Pushes a clip: the area of `path` (logical coordinates; none clips
/// everything away) intersected with the clip below it.
fn push_clip(target: &mut Target, path: Option<&Path>) -> Result<(), String> {
    if target.state.clips.len() >= MAX_CLIPS {
        return Err(format!("clips nest at most {} deep", MAX_CLIPS));
    }
    let scale = target.scale as f32;
    let (w, h) = (target.pixmap.width(), target.pixmap.height());
    let clip = Clip::path(
        w,
        h,
        target.state.clip(),
        path,
        Transform::from_scale(scale, scale),
    )
    .ok_or_else(|| "cannot allocate clip".to_string())?;
    target.state.clips.push(clip);
    Ok(())
}

/// Pushes a rectangular clip (logical coordinates). It needs no mask: its
/// edges are rounded to whole pixels.
fn push_clip_rect(target: &mut Target, x: f32, y: f32, w: f32, h: f32) -> Result<(), String> {
    if target.state.clips.len() >= MAX_CLIPS {
        return Err(format!("clips nest at most {} deep", MAX_CLIPS));
    }
    let scale = target.scale as f32;
    let (pw, ph) = (target.pixmap.width(), target.pixmap.height());
    let edges = (x * scale, y * scale, (x + w) * scale, (y + h) * scale);
    let clip = Clip::rect(pw, ph, target.state.clip(), edges);
    target.state.clips.push(clip);
    Ok(())
}

fn pop_clip(target: &mut Target) -> Result<(), String> {
    let floor = target.state.groups.last().map_or(0, |group| group.clips);
    if target.state.clips.len() <= floor {
        return Err("popClip without a matching pushClip".to_string());
    }
    target.state.clips.pop();
    Ok(())
}

/// Starts a group: what is drawn next goes to a transparent layer that
/// `pop_opacity` composites over the pixels drawn so far. Layers are reused
/// from groups closed earlier, so a fade allocates nothing per frame.
fn push_opacity(target: &mut Target, opacity: f32) -> Result<(), String> {
    if target.state.groups.len() >= MAX_GROUPS {
        return Err(format!("opacity groups nest at most {} deep", MAX_GROUPS));
    }
    let (w, h) = (target.pixmap.width(), target.pixmap.height());
    let layer = match target.state.spare.pop() {
        Some(layer) if layer.width() == w && layer.height() == h => layer,
        _ => Pixmap::new(w, h).ok_or_else(|| "cannot allocate layer".to_string())?,
    };
    let saved = std::mem::replace(target.pixmap, layer);
    let clips = target.state.clips.len();
    target.state.groups.push(Group {
        saved,
        opacity,
        clips,
        dirty: Bounds::EMPTY,
    });
    Ok(())
}

fn pop_opacity(target: &mut Target) -> Result<(), String> {
    match target.state.groups.last() {
        None => return Err("popOpacity without a matching pushOpacity".to_string()),
        Some(group) if group.clips != target.state.clips.len() => {
            return Err("popClip is missing before popOpacity".to_string());
        }
        Some(_) => {}
    }
    let Some(group) = target.state.groups.pop() else {
        return Ok(());
    };
    let mut layer = std::mem::replace(target.pixmap, group.saved);
    // Only what was drawn into the layer is composited, and then cleared so
    // the layer can serve the next group.
    images::composite_area(
        target.pixmap,
        layer.as_ref(),
        0,
        0,
        group.dirty,
        group.opacity,
        None,
        None,
    );
    images::clear_area(&mut layer, group.dirty);
    if group.opacity > 0.0 {
        target.touch(group.dirty);
    }
    if target.state.spare.len() < SPARE_LAYERS {
        target.state.spare.push(layer);
    }
    Ok(())
}

/// A rectangle with rounded corners as a path; a zero radius is a plain
/// rectangle, and larger radii are clamped to what the sides allow.
fn rounded_rect(x: f32, y: f32, w: f32, h: f32, radius: f32) -> Option<Path> {
    let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    if r == 0.0 {
        return PathBuilder::from_rect(Rect::from_xywh(x, y, w, h)?).into();
    }
    // Circular arcs as cubic Béziers, the usual 0.5523 control distance.
    let k = r * 0.552_284_8;
    let (right, bottom) = (x + w, y + h);
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(right - r, y);
    pb.cubic_to(right - r + k, y, right, y + r - k, right, y + r);
    pb.line_to(right, bottom - r);
    pb.cubic_to(
        right,
        bottom - r + k,
        right - r + k,
        bottom,
        right - r,
        bottom,
    );
    pb.line_to(x + r, bottom);
    pb.cubic_to(x + r - k, bottom, x, bottom - r + k, x, bottom - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn polygon_path(points: &[(f32, f32)]) -> Option<Path> {
    let mut pb = PathBuilder::new();
    let (first, rest) = points.split_first()?;
    pb.move_to(first.0, first.1);
    for (x, y) in rest {
        pb.line_to(*x, *y);
    }
    pb.close();
    pb.finish()
}

/// Reads the window id plus the `x, y, w, h` that every rectangle native
/// starts with.
fn rect_args(args: &[Value]) -> Result<(u64, f32, f32, f32, f32), String> {
    let id = integer_arg(args, 0, "window")?;
    let x = float_arg(args, 1, "x")? as f32;
    let y = float_arg(args, 2, "y")? as f32;
    let w = float_arg(args, 3, "width")? as f32;
    let h = float_arg(args, 4, "height")? as f32;
    if w < 0.0 || h < 0.0 {
        return Err("width and height must be non-negative".to_string());
    }
    Ok((id, x, y, w, h))
}

// FONTS --------------------------------------------------------------------

fn load_font(bytes: Vec<u8>, index: u32) -> Result<fontdue::Font, String> {
    if bytes.len() > FONT_FILE_LIMIT {
        return Err(format!(
            "cannot load font: larger than {} MiB",
            FONT_FILE_LIMIT / 1024 / 1024
        ));
    }
    let settings = fontdue::FontSettings {
        collection_index: index,
        ..fontdue::FontSettings::default()
    };
    fontdue::Font::from_bytes(bytes, settings).map_err(|err| format!("cannot load font: {}", err))
}

/// Parks a freshly loaded font under a new id and returns what the
/// `__ui_load_font*` natives hand back: the id and the font's full name.
fn register_font(font: fontdue::Font) -> Vec<Value> {
    let name = font.name().map(text).unwrap_or(Value::Nil);
    let id = next_id();
    FONTS.with(|fonts| fonts.borrow_mut().insert(id, Rc::new(font)));
    vec![integer(id), name]
}

/// Font `id`, loading the bundled font on first use.
fn font(id: u64) -> Result<Rc<fontdue::Font>, String> {
    FONTS.with(|fonts| {
        let mut fonts = fonts.borrow_mut();
        if id == DEFAULT_FONT_ID && !fonts.contains_key(&id) {
            fonts.insert(id, Rc::new(load_font(DEFAULT_FONT.to_vec(), 0)?));
        }
        fonts
            .get(&id)
            .cloned()
            .ok_or_else(|| "font was freed".to_string())
    })
}

/// The fonts a piece of text looks its glyphs up in, in order: the font it
/// asked for, then its own fallbacks or else the global ones, then the
/// bundled font, so Latin text still shows when a fallback lacks it.
fn font_chain(
    primary: u64,
    fallback: Option<&[u64]>,
) -> Result<Vec<(u64, Rc<fontdue::Font>)>, String> {
    let global;
    let fallback = match fallback {
        Some(ids) => ids,
        None => {
            global = FALLBACKS.with(|fallbacks| fallbacks.borrow().clone());
            &global
        }
    };
    let mut ids = vec![primary];
    for &id in fallback.iter().chain(&[DEFAULT_FONT_ID]) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.into_iter().map(|id| Ok((id, font(id)?))).collect()
}

/// Finds an installed font by family name (or PostScript name), ignoring
/// case, and returns its file and the face index inside it. The system's
/// fonts are scanned once, on first use.
fn find_system_font(family: &str, weight: u16, italic: bool) -> Option<(std::path::PathBuf, u32)> {
    SYSTEM_FONTS.with(|cell| {
        let mut cell = cell.borrow_mut();
        let db = cell.get_or_insert_with(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            db
        });
        let canonical = db
            .faces()
            .flat_map(|face| face.families.iter())
            .find(|(name, _)| name.eq_ignore_ascii_case(family))
            .map(|(name, _)| name.clone());
        let id = match canonical {
            Some(name) => db.query(&fontdb::Query {
                families: &[fontdb::Family::Name(&name)],
                weight: fontdb::Weight(weight),
                stretch: fontdb::Stretch::Normal,
                style: if italic {
                    fontdb::Style::Italic
                } else {
                    fontdb::Style::Normal
                },
            })?,
            None => {
                db.faces()
                    .find(|face| face.post_script_name.eq_ignore_ascii_case(family))?
                    .id
            }
        };
        match db.face_source(id)? {
            (fontdb::Source::File(path), index) => Some((path, index)),
            (fontdb::Source::SharedFile(path, _), index) => Some((path, index)),
            (fontdb::Source::Binary(_), _) => None,
        }
    })
}

// GLYPH CACHE --------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphKey {
    font: u64,
    index: u16,
    /// The pixel size's bits: sizes are compared exactly.
    px: u32,
}

type Bitmap = Rc<(fontdue::Metrics, Vec<u8>)>;

/// Rasterized glyphs, kept because text is redrawn every frame. Bounded by
/// the bytes its bitmaps take; when they pass the limit, the least recently
/// drawn are dropped until a quarter of the room is free again.
#[derive(Default)]
struct GlyphCache {
    entries: HashMap<GlyphKey, (Bitmap, u64)>,
    bytes: usize,
    clock: u64,
}

impl GlyphCache {
    fn get(
        &mut self,
        key: GlyphKey,
        limit: usize,
        rasterize: impl FnOnce() -> (fontdue::Metrics, Vec<u8>),
    ) -> Bitmap {
        self.clock += 1;
        if let Some((bitmap, used)) = self.entries.get_mut(&key) {
            *used = self.clock;
            return bitmap.clone();
        }
        let bitmap = Rc::new(rasterize());
        self.bytes += glyph_cost(&bitmap);
        self.entries.insert(key, (bitmap.clone(), self.clock));
        if self.bytes > limit {
            self.trim(limit / 4 * 3);
        }
        bitmap
    }

    fn trim(&mut self, target: usize) {
        let mut ages: Vec<(u64, GlyphKey)> = self
            .entries
            .iter()
            .map(|(key, (_, used))| (*used, *key))
            .collect();
        ages.sort_unstable_by_key(|(used, _)| *used);
        for (_, key) in ages {
            if self.bytes <= target {
                break;
            }
            if let Some((bitmap, _)) = self.entries.remove(&key) {
                self.bytes -= glyph_cost(&bitmap);
            }
        }
    }

    fn forget_font(&mut self, font: u64) {
        self.entries.retain(|key, _| key.font != font);
        self.bytes = self
            .entries
            .values()
            .map(|(bitmap, _)| glyph_cost(bitmap))
            .sum();
    }
}

/// What a cached glyph counts against the limit: its bitmap plus a rough
/// allowance for the entry itself.
fn glyph_cost(bitmap: &Bitmap) -> usize {
    bitmap.1.len() + 64
}

// TEXT LAYOUT --------------------------------------------------------------
//
// Text is laid out here rather than with fontdue's `Layout` so that each
// character can come from a different font (fallbacks), and so `text` and
// `measureText` share one function: they cannot disagree. Layout works in
// logical pixels at the requested size; drawing only rasterizes the placed
// glyphs at the window's scale, so the measurements do not depend on DPI.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Overflow {
    Clip,
    Ellipsis,
    Wrap,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Align {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, Debug)]
struct TextOptions {
    size: f32,
    max_width: Option<f32>,
    overflow: Overflow,
    align: Align,
    /// A multiple of `size`; `None` keeps the font's own line spacing.
    line_height: Option<f32>,
    letter_spacing: f32,
}

#[cfg(test)]
impl TextOptions {
    /// Plain text at `size`, the defaults the natives start from.
    fn sized(size: f32) -> TextOptions {
        TextOptions {
            size,
            max_width: None,
            overflow: Overflow::Wrap,
            align: Align::Left,
            line_height: None,
            letter_spacing: 0.0,
        }
    }
}

/// A character resolved against the font chain: which font draws it, its
/// glyph there, how far it advances the pen and the kerning against the
/// character before it.
#[derive(Clone, Copy, Debug)]
struct Cluster {
    font: usize,
    index: u16,
    advance: f32,
    kern: f32,
    ch: char,
}

impl Cluster {
    fn is_space(&self) -> bool {
        self.ch.is_whitespace()
    }
}

/// A glyph ready to draw: the font (as a position in the chain), its index
/// and its pen position and baseline relative to the text's origin.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Placed {
    font: usize,
    index: u16,
    x: f32,
    baseline: f32,
}

#[derive(Debug, Default)]
struct TextLayout {
    /// The size it was laid out at, which drawing rasterizes at (scaled).
    size: f32,
    glyphs: Vec<Placed>,
    width: f32,
    height: f32,
    lines: usize,
    /// The horizontal range glyphs are clipped to, for `overflow = "clip"`.
    clip: Option<(f32, f32)>,
}

/// Characters that take no room and draw nothing: control characters,
/// zero-width spaces and joiners, bidi marks, variation selectors (as in
/// emoji presentation sequences) and tags.
fn is_ignorable(ch: char) -> bool {
    (ch.is_control() && ch != '\t')
        || matches!(ch,
            '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{FEFF}'
            | '\u{E0000}'..='\u{E0FFF}')
}

/// The first font in the chain with a glyph for `ch`, falling back to the
/// primary font's "missing glyph" box when none has one.
fn resolve_glyph(chain: &[&fontdue::Font], ch: char) -> (usize, u16) {
    chain
        .iter()
        .enumerate()
        .find_map(|(slot, font)| match font.lookup_glyph_index(ch) {
            0 => None,
            index => Some((slot, index)),
        })
        .unwrap_or((0, 0))
}

/// Resolves one line of text (no newlines) to clusters.
fn clusters(chain: &[&fontdue::Font], text: &str, size: f32) -> Vec<Cluster> {
    let mut out: Vec<Cluster> = Vec::with_capacity(text.len());
    for ch in text.chars() {
        if is_ignorable(ch) {
            continue;
        }
        // A tab is four spaces wide.
        let (lookup, repeat) = if ch == '\t' { (' ', 4.0) } else { (ch, 1.0) };
        let (font, index) = resolve_glyph(chain, lookup);
        let advance = chain[font].metrics_indexed(index, size).advance_width * repeat;
        let kern = match out.last() {
            Some(prev) if prev.font == font => chain[font]
                .horizontal_kern_indexed(prev.index, index, size)
                .unwrap_or(0.0),
            _ => 0.0,
        };
        out.push(Cluster {
            font,
            index,
            advance,
            kern,
            ch,
        });
    }
    out
}

/// The width of a run of clusters laid side by side.
fn run_width(run: &[Cluster], spacing: f32) -> f32 {
    let mut pen = 0.0;
    for (i, cluster) in run.iter().enumerate() {
        if i > 0 {
            pen += spacing + cluster.kern;
        }
        pen += cluster.advance;
    }
    pen
}

/// Scripts written without spaces between words, where a line may break
/// between any two characters: CJK ideographs, kana, Hangul and their
/// punctuation and full-width forms.
fn breaks_anywhere(ch: char) -> bool {
    matches!(ch,
        '\u{1100}'..='\u{11FF}'
        | '\u{2E80}'..='\u{9FFF}'
        | '\u{AC00}'..='\u{D7AF}'
        | '\u{F900}'..='\u{FAFF}'
        | '\u{FF00}'..='\u{FFEF}'
        | '\u{20000}'..='\u{3FFFF}')
}

/// Characters a line may not start with (closing punctuation, small kana,
/// the prolonged sound mark), the usual "kinsoku" rule.
fn cannot_start_line(ch: char) -> bool {
    matches!(
        ch,
        ')' | ']'
            | '}'
            | ','
            | '.'
            | ':'
            | ';'
            | '!'
            | '?'
            | '、'
            | '。'
            | '，'
            | '．'
            | '・'
            | '：'
            | '；'
            | '？'
            | '！'
            | 'ー'
            | '～'
            | '）'
            | '」'
            | '』'
            | '】'
            | '〉'
            | '》'
            | '〕'
            | '］'
            | '｝'
            | 'ぁ'
            | 'ぃ'
            | 'ぅ'
            | 'ぇ'
            | 'ぉ'
            | 'っ'
            | 'ゃ'
            | 'ゅ'
            | 'ょ'
            | 'ゎ'
            | 'ァ'
            | 'ィ'
            | 'ゥ'
            | 'ェ'
            | 'ォ'
            | 'ッ'
            | 'ャ'
            | 'ュ'
            | 'ョ'
            | 'ヮ'
            | 'ヵ'
            | 'ヶ'
            | 'ゝ'
            | 'ゞ'
            | 'ヽ'
            | 'ヾ'
            | '々'
    )
}

/// Characters a line may not end with: opening brackets.
fn cannot_end_line(ch: char) -> bool {
    matches!(
        ch,
        '(' | '[' | '{' | '（' | '「' | '『' | '【' | '〈' | '《' | '〔' | '［' | '｛'
    )
}

/// Whether a line may break between `before` and `after`: after a space,
/// after a hyphen inside a word, or around CJK characters.
fn can_break(before: &Cluster, after: &Cluster) -> bool {
    if after.is_space() {
        return false;
    }
    if before.is_space() {
        return true;
    }
    if cannot_start_line(after.ch) || cannot_end_line(before.ch) {
        return false;
    }
    before.ch == '-' || breaks_anywhere(before.ch) || breaks_anywhere(after.ch)
}

/// Splits a line into ranges no wider than `max` where it can, breaking at
/// the last opportunity that fits, or mid-word when a word is wider than a
/// whole line. Spaces at a break are dropped.
fn wrap_lines(run: &[Cluster], max: f32, spacing: f32) -> Vec<(usize, usize)> {
    if run.is_empty() {
        return vec![(0, 0)];
    }
    let mut lines = Vec::new();
    let mut start = 0;
    while start < run.len() {
        let mut pen = 0.0;
        let mut opportunity = None;
        let mut end = run.len();
        for i in start..run.len() {
            let cluster = &run[i];
            if i > start && can_break(&run[i - 1], cluster) {
                opportunity = Some(i);
            }
            let next = if i == start {
                cluster.advance
            } else {
                pen + spacing + cluster.kern + cluster.advance
            };
            // Spaces may hang past the edge; they are trimmed below.
            if i > start && !cluster.is_space() && next > max {
                end = opportunity.unwrap_or(i);
                break;
            }
            pen = next;
        }
        let mut line_end = end;
        if end < run.len() {
            while line_end > start && run[line_end - 1].is_space() {
                line_end -= 1;
            }
        }
        lines.push((start, line_end));
        start = end;
        if end < run.len() {
            while start < run.len() && run[start].is_space() {
                start += 1;
            }
        }
    }
    lines
}

/// Shortens a line wider than `max` to the longest start that still fits
/// with `ellipsis` after it, dropping spaces before the ellipsis.
fn ellipsize(run: &[Cluster], ellipsis: &[Cluster], max: f32, spacing: f32) -> Vec<Cluster> {
    if run_width(run, spacing) <= max {
        return run.to_vec();
    }
    let room = max - spacing - run_width(ellipsis, spacing);
    let mut pen = 0.0;
    let mut keep = 0;
    for (i, cluster) in run.iter().enumerate() {
        let next = if i == 0 {
            cluster.advance
        } else {
            pen + spacing + cluster.kern + cluster.advance
        };
        if next > room {
            break;
        }
        pen = next;
        keep = i + 1;
    }
    while keep > 0 && run[keep - 1].is_space() {
        keep -= 1;
    }
    let mut out = run[..keep].to_vec();
    for (i, cluster) in ellipsis.iter().enumerate() {
        out.push(Cluster {
            kern: if i == 0 { 0.0 } else { cluster.kern },
            ..*cluster
        });
    }
    out
}

/// "…" from the first font that has it, or three full stops.
fn ellipsis_clusters(chain: &[&fontdue::Font], size: f32) -> Vec<Cluster> {
    let ellipsis = clusters(chain, "\u{2026}", size);
    if ellipsis.iter().all(|cluster| cluster.index != 0) {
        ellipsis
    } else {
        clusters(chain, "...", size)
    }
}

/// Lays `text` out with its block's top-left corner at the origin (or, with
/// no `max_width`, its top-center or top-right for centered and
/// right-aligned text). Line boxes use the primary font's metrics, so a
/// line keeps its height whichever fallbacks its characters come from.
fn layout_text(chain: &[&fontdue::Font], text: &str, options: &TextOptions) -> TextLayout {
    let size = options.size;
    let spacing = options.letter_spacing;
    let mut lines: Vec<Vec<Cluster>> = Vec::new();
    if !text.is_empty() {
        let mut ellipsis = None;
        for paragraph in text.split('\n') {
            let run = clusters(chain, paragraph, size);
            match (options.max_width, options.overflow) {
                (Some(max), Overflow::Wrap) => {
                    for (start, end) in wrap_lines(&run, max, spacing) {
                        lines.push(run[start..end].to_vec());
                    }
                }
                (Some(max), Overflow::Ellipsis) => {
                    let ellipsis = ellipsis.get_or_insert_with(|| ellipsis_clusters(chain, size));
                    lines.push(ellipsize(&run, ellipsis, max, spacing));
                }
                _ => lines.push(run),
            }
        }
    }

    let (ascent, descent, natural) = match chain[0].horizontal_line_metrics(size) {
        Some(metrics) => (metrics.ascent, metrics.descent, metrics.new_line_size),
        None => (size * 0.8, -size * 0.2, size),
    };
    // An explicit line height spreads the extra room evenly above and
    // below the glyphs, as CSS does.
    let (advance, top) = match options.line_height {
        Some(multiple) => (
            multiple * size,
            (multiple * size - (ascent - descent)) / 2.0,
        ),
        None => (natural, 0.0),
    };

    let widths: Vec<f32> = lines.iter().map(|line| run_width(line, spacing)).collect();
    let widest = widths.iter().copied().fold(0.0_f32, f32::max);
    let clipping = options.max_width.is_some() && options.overflow == Overflow::Clip;
    let (box_width, origin) = match options.max_width {
        Some(max) => (max, 0.0),
        None => (
            widest,
            match options.align {
                Align::Left => 0.0,
                Align::Center => -widest / 2.0,
                Align::Right => -widest,
            },
        ),
    };

    let mut glyphs = Vec::new();
    for (row, line) in lines.iter().enumerate() {
        let mut free = box_width - widths[row];
        if clipping {
            // A clipped line that overflows starts at the left edge.
            free = free.max(0.0);
        }
        let mut pen = origin
            + match options.align {
                Align::Left => 0.0,
                Align::Center => free / 2.0,
                Align::Right => free,
            };
        let baseline = top + ascent + row as f32 * advance;
        for (i, cluster) in line.iter().enumerate() {
            if i > 0 {
                pen += spacing + cluster.kern;
            }
            if !cluster.is_space() {
                glyphs.push(Placed {
                    font: cluster.font,
                    index: cluster.index,
                    x: pen,
                    baseline,
                });
            }
            pen += cluster.advance;
        }
    }

    TextLayout {
        size,
        glyphs,
        width: match options.max_width {
            Some(max) if clipping => widest.min(max),
            _ => widest,
        },
        height: lines.len() as f32 * advance,
        lines: lines.len(),
        clip: if clipping {
            Some((origin, origin + box_width))
        } else {
            None
        },
    }
}

/// Composites an 8-bit coverage mask at `(x0, y0)` in `color` over the
/// pixmap, clipping to its bounds, to the columns `columns.0..columns.1` and
/// to the active clip. Returns the pixels it may have changed.
fn blend_mask(
    pixmap: &mut Pixmap,
    (x0, y0): (i32, i32),
    (width, height): (usize, usize),
    mask: &[u8],
    color: (u8, u8, u8, u8),
    columns: (i32, i32),
    clip: Option<&Clip>,
) -> Bounds {
    let glyph = Bounds::from_xywh(x0, y0, width as u32, height as u32);
    let allowed = Bounds {
        x0: columns.0,
        y0: i32::MIN,
        x1: columns.1,
        y1: i32::MAX,
    };
    let (pw, ph) = (pixmap.width(), pixmap.height());
    let b = clip::visible(clip, glyph.intersect(&allowed), pw, ph);
    if b.is_empty() || color.3 == 0 {
        return Bounds::EMPTY;
    }
    let origin = ((b.x0 - x0) as usize, (b.y0 - y0) as usize);
    images::blend_coverage(pixmap, color, mask, width, origin, b, clip);
    b
}

/// Draws a laid-out text with its origin at logical `(x, y)`, rasterizing
/// each glyph at the window's `scale` (through the glyph cache). Returns
/// the pixels it may have changed.
#[allow(clippy::too_many_arguments)]
fn draw_layout(
    pixmap: &mut Pixmap,
    cache: &mut GlyphCache,
    chain: &[(u64, Rc<fontdue::Font>)],
    layout: &TextLayout,
    (x, y): (f32, f32),
    scale: f32,
    color: (u8, u8, u8, u8),
    clip: Option<&Clip>,
) -> Bounds {
    let px = layout.size * scale;
    let columns = match layout.clip {
        Some((left, right)) => (
            ((x + left) * scale).floor() as i32,
            ((x + right) * scale).ceil() as i32,
        ),
        None => (i32::MIN, i32::MAX),
    };
    let mut touched = Bounds::EMPTY;
    for glyph in &layout.glyphs {
        let (id, font) = &chain[glyph.font];
        let key = GlyphKey {
            font: *id,
            index: glyph.index,
            px: px.to_bits(),
        };
        let bitmap = cache.get(key, GLYPH_CACHE_BYTES, || {
            font.rasterize_indexed(glyph.index, px)
        });
        let (metrics, mask) = &*bitmap;
        if metrics.width == 0 || metrics.height == 0 {
            continue;
        }
        let left = ((x + glyph.x) * scale).round() as i32 + metrics.xmin;
        let top = ((y + glyph.baseline) * scale).round() as i32
            + (-metrics.bounds.height - metrics.bounds.ymin).floor() as i32;
        let drawn = blend_mask(
            pixmap,
            (left, top),
            (metrics.width, metrics.height),
            mask,
            color,
            columns,
            clip,
        );
        touched = touched.union(&drawn);
    }
    touched
}

/// What a laid-out text depends on: the text, the fonts of its chain and
/// every option, sizes by their bits.
#[derive(PartialEq, Eq, Hash)]
struct LayoutKey {
    text: String,
    fonts: Vec<u64>,
    options: [u32; 7],
}

impl LayoutKey {
    fn new(text: String, chain: &[(u64, Rc<fontdue::Font>)], options: &TextOptions) -> LayoutKey {
        let bits = |value: Option<f32>| value.map_or(u32::MAX, f32::to_bits);
        LayoutKey {
            text,
            fonts: chain.iter().map(|(id, _)| *id).collect(),
            options: [
                options.size.to_bits(),
                bits(options.max_width),
                options.overflow as u32,
                options.align as u32,
                bits(options.line_height),
                options.letter_spacing.to_bits(),
                0,
            ],
        }
    }
}

/// Laid-out texts, kept because a UI draws (and measures) the same labels
/// every frame. Two generations: lookups move entries into the current
/// one, and when it fills up the previous one is dropped, which keeps what
/// was used recently without tracking every entry's age.
#[derive(Default)]
struct LayoutCache {
    current: HashMap<LayoutKey, Rc<TextLayout>>,
    previous: HashMap<LayoutKey, Rc<TextLayout>>,
}

/// How many layouts a generation holds.
const LAYOUT_CACHE: usize = 4096;
/// Texts longer than this are laid out every time rather than kept.
const LAYOUT_CACHE_TEXT: usize = 4096;

impl LayoutCache {
    fn get(&mut self, key: LayoutKey, layout: impl FnOnce(&str) -> TextLayout) -> Rc<TextLayout> {
        if let Some(found) = self.current.get(&key) {
            return found.clone();
        }
        let found = match self.previous.remove(&key) {
            Some(found) => found,
            None => Rc::new(layout(&key.text)),
        };
        if self.current.len() >= LAYOUT_CACHE {
            self.previous = std::mem::take(&mut self.current);
        }
        self.current.insert(key, found.clone());
        found
    }

    fn clear(&mut self) {
        self.current.clear();
        self.previous.clear();
    }
}

/// Lays `text` out through the layout cache.
fn cached_layout(
    text: String,
    chain: &[(u64, Rc<fontdue::Font>)],
    options: &TextOptions,
) -> Rc<TextLayout> {
    let fonts: Vec<&fontdue::Font> = chain.iter().map(|(_, font)| font.as_ref()).collect();
    if text.len() > LAYOUT_CACHE_TEXT {
        return Rc::new(layout_text(&fonts, &text, options));
    }
    let key = LayoutKey::new(text, chain, options);
    LAYOUTS.with(|cache| {
        cache
            .borrow_mut()
            .get(key, |text| layout_text(&fonts, text, options))
    })
}

// IMAGES -------------------------------------------------------------------

/// The bytes of a font or image: from the app bundle when the path is
/// bundled (see `crate::app`), otherwise from disk.
fn read_asset(path: &str) -> Result<Vec<u8>, String> {
    crate::app::read(path)
        .unwrap_or_else(|| std::fs::read(path))
        .map_err(|err| format!("cannot open {}: {}", path, err))
}

/// Scaled copies an image may keep for redrawing at the same size. A full
/// HD animation scaled to a window keeps one copy per frame until this is
/// reached, then starts over, so it rescales each frame at most once per
/// loop instead of on every draw.
const SCALED_CACHE_BYTES: usize = 128 * 1024 * 1024;

/// A decoded image parked under an id. Drawing only reads `frames`, so an
/// off-screen canvas can later be stored and drawn the same way.
struct ImageEntry {
    frames: Vec<ImageFrame>,
    delays: Vec<f64>,
    loop_count: u32,
    /// The limits it was decoded under, which `resized` copies keep.
    limits: Limits,
    /// The last prepared (scaled or cropped) copy of each frame.
    scaled: HashMap<usize, Scaled>,
    scaled_bytes: usize,
}

struct ImageFrame {
    pixmap: Pixmap,
    opaque: bool,
}

/// What a prepared copy was made from: the source rectangle and the size
/// in physical pixels.
#[derive(Clone, Copy, PartialEq)]
struct ScaleKey {
    src: images::RectF,
    width: u32,
    height: u32,
}

struct Scaled {
    key: ScaleKey,
    frame: ImageFrame,
}

impl ImageEntry {
    fn new(decoded: Decoded, limits: Limits) -> ImageEntry {
        let frames = decoded
            .frames
            .into_iter()
            .map(|pixmap| ImageFrame {
                opaque: images::is_opaque(&pixmap),
                pixmap,
            })
            .collect();
        ImageEntry {
            frames,
            delays: decoded.delays,
            loop_count: decoded.loop_count,
            limits,
            scaled: HashMap::new(),
            scaled_bytes: 0,
        }
    }

    fn width(&self) -> u32 {
        self.frames[0].pixmap.width()
    }

    fn height(&self) -> u32 {
        self.frames[0].pixmap.height()
    }

    /// Frame `index` prepared at `key`'s crop and size, from the cache when
    /// the last draw of this frame used the same ones.
    fn prepared(&mut self, index: usize, key: ScaleKey) -> Option<&ImageFrame> {
        let hit = self.scaled.get(&index).is_some_and(|s| s.key == key);
        if !hit {
            let pixmap =
                images::resample(&self.frames[index].pixmap, key.src, key.width, key.height)?;
            let bytes = pixmap.data().len();
            if let Some(old) = self.scaled.remove(&index) {
                self.scaled_bytes -= old.frame.pixmap.data().len();
            }
            if self.scaled_bytes + bytes > SCALED_CACHE_BYTES {
                self.scaled.clear();
                self.scaled_bytes = 0;
            }
            self.scaled_bytes += bytes;
            let frame = ImageFrame {
                opaque: images::is_opaque(&pixmap),
                pixmap,
            };
            self.scaled.insert(index, Scaled { key, frame });
        }
        self.scaled.get(&index).map(|s| &s.frame)
    }
}

/// The optional `maxPixels` and `maxBytes` limits at `index` and after.
fn limits_arg(args: &[Value], index: usize) -> Result<Limits, String> {
    let defaults = Limits::default();
    let limit = |offset: usize, name: &str, default: u64| match args.get(index + offset) {
        None | Some(Value::Nil) => Ok(default),
        _ => match integer_arg(args, index + offset, name)? {
            0 => Err(format!("{} must be positive", name)),
            value => Ok(value),
        },
    };
    Ok(Limits {
        max_pixels: limit(0, "maxPixels", defaults.max_pixels)?,
        max_bytes: limit(1, "maxBytes", defaults.max_bytes)?,
    })
}

/// Parks a decoded image and returns what `lib/ui.nyk` builds the `Image`
/// from: id, width, height, frame count, loop count and frame delays.
fn register_image(entry: ImageEntry) -> Vec<Value> {
    let (width, height) = (entry.width(), entry.height());
    let frames = entry.frames.len();
    let loop_count = entry.loop_count;
    let delays = entry.delays.iter().map(|d| Value::Float(*d)).collect();
    let id = next_id();
    IMAGES.with(|images| images.borrow_mut().insert(id, entry));
    vec![
        integer(id),
        integer(width as u64),
        integer(height as u64),
        integer(frames as u64),
        integer(loop_count as u64),
        new_table(delays),
    ]
}

/// The icon `neyuki build` embedded in this executable, so an app's windows
/// match its file icon without asking for one. Other platforms take the
/// app icon from the bundle or desktop file instead.
#[cfg(windows)]
fn executable_icon() -> Option<Icon> {
    use winit::platform::windows::IconExtWindows;
    Icon::from_resource_name("MAINICON", None).ok()
}

#[cfg(not(windows))]
fn executable_icon() -> Option<Icon> {
    None
}

// NATIVES ------------------------------------------------------------------

/// `__ui_available()` reports whether a display can be reached.
fn builtin_available(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::Bool(with_event_loop(|_| ()).is_ok())])
}

/// `__ui_open(title, width, height, resizable, visible, decorations?,
/// minWidth?, minHeight?, transparent?, shadow?, icon?)` creates a window
/// and returns its id and DPI scale factor. `icon` is an image id.
fn builtin_open(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let request = OpenRequest {
        title: string_arg(args, 0, "title")?,
        width: float_arg(args, 1, "width")?,
        height: float_arg(args, 2, "height")?,
        resizable: bool_arg(args, 3, "resizable", true)?,
        visible: bool_arg(args, 4, "visible", true)?,
        decorations: bool_arg(args, 5, "decorations", true)?,
        min_width: optional_float_arg(args, 6, "minWidth", 0.0)?,
        min_height: optional_float_arg(args, 7, "minHeight", 0.0)?,
        transparent: bool_arg(args, 8, "transparent", false)?,
        shadow: bool_arg(args, 9, "shadow", true)?,
        icon: icon_arg(args, 10)?,
    };
    if request.width <= 0.0 || request.height <= 0.0 {
        return Err("width and height must be positive".to_string());
    }
    if request.min_width < 0.0 || request.min_height < 0.0 {
        return Err("minWidth and minHeight must be non-negative".to_string());
    }
    PENDING_OPEN.with(|pending| *pending.borrow_mut() = Some(request));
    OPENED.with(|opened| *opened.borrow_mut() = None);
    for _ in 0..OPEN_ATTEMPTS {
        if let Err(err) = pump(Duration::ZERO) {
            PENDING_OPEN.with(|pending| *pending.borrow_mut() = None);
            return Err(err);
        }
        if let Some(result) = OPENED.with(|opened| opened.borrow_mut().take()) {
            let (id, scale) = result?;
            return Ok(vec![integer(id), Value::Float(scale)]);
        }
    }
    PENDING_OPEN.with(|pending| *pending.borrow_mut() = None);
    Err("cannot create window: the event loop did not respond".to_string())
}

/// `__ui_close(window)` destroys the window; later calls on the id fail.
fn builtin_close(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let removed = WINDOWS.with(|windows| windows.borrow_mut().remove(&id));
    if let Some(win) = removed {
        // The OS controls belong to the window, so the session ends with it.
        MEDIA.with(|media| {
            let mut media = media.borrow_mut();
            if media.as_ref().is_some_and(|session| session.window == id) {
                *media = None;
            }
        });
        WINDOW_IDS.with(|ids| ids.borrow_mut().remove(&win.window.id()));
        drop(win);
        // Let the window system process the teardown so the window really
        // disappears even if the script never polls again.
        let _ = pump(Duration::ZERO);
    }
    Ok(vec![])
}

/// `__ui_set_title(window, title)`.
fn builtin_set_title(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let title = string_arg(args, 1, "title")?;
    with_window(id, |win| {
        win.window.set_title(&title);
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_set_size(window, width, height)` asks for a new logical size; the
/// `resize` event confirms what the window system granted.
fn builtin_set_size(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let width = float_arg(args, 1, "width")?;
    let height = float_arg(args, 2, "height")?;
    if width <= 0.0 || height <= 0.0 {
        return Err("width and height must be positive".to_string());
    }
    with_window(id, |win| {
        let _ = win
            .window
            .request_inner_size(LogicalSize::new(width, height));
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_size(window)` returns the logical width, height and scale factor.
fn builtin_size(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    with_window(id, |win| {
        let size = win.window.inner_size().to_logical::<f64>(win.scale);
        Ok(vec![
            Value::Float(size.width),
            Value::Float(size.height),
            Value::Float(win.scale),
        ])
    })
}

/// How long `__ui_poll` may spend resuming tasks after pumping events, so
/// a busy scheduler cannot starve rendering. It is checked between tasks,
/// so one task that runs long without yielding still delays the frame.
const TASK_BUDGET: Duration = Duration::from_millis(4);

/// Pumps the event loop, waiting up to `timeout` for something to happen,
/// and takes the events that arrived.
fn pump_events(timeout: Duration) -> Result<Vec<Value>, String> {
    pump(timeout)?;
    webview::pump_toolkit();
    drain_media_commands();
    let mut events = EVENTS.with(|events| std::mem::take(&mut *events.borrow_mut()));
    events.extend(webview::take_events());
    Ok(events)
}

/// The hook the task scheduler pumps the event loop through for the tasks
/// parked in `__ui_poll`.
fn scheduler_pump(_vm: &mut VM, timeout: Duration) -> Result<Value, String> {
    pump_events(timeout).map(new_table)
}

/// `__ui_poll(timeout?)` pumps the event loop, waiting up to `timeout`
/// seconds for something to happen, then runs the tasks that are due
/// (within `TASK_BUDGET`) and returns the events as an array. Background
/// work that finishes during the wait wakes it early through an event loop
/// proxy, so an idle app can wait long without missing a completion.
///
/// A task that polls with a timeout does not block: it is parked and
/// yields, and the scheduler pumps the event loop for it (see
/// `task::service_pollers`), so the rest of the program keeps running. The
/// main chunk, a plain coroutine, a call under `try` and a zero timeout
/// cannot or need not yield and block as described above.
fn builtin_poll(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let timeout = timeout_arg(args, 0, "timeout")?;
    if !task::has_waker(vm) {
        let proxy = with_event_loop(|event_loop| event_loop.create_proxy())?;
        task::set_waker(
            vm,
            Box::new(move || {
                // The wake-up event itself carries nothing: returning from
                // the pump is all it is for.
                let _ = proxy.send_event(());
            }),
        );
    }
    if !timeout.is_zero()
        && let Some(thread) = task::current_task(vm)
    {
        let deadline = task::deadline_after(Instant::now(), timeout);
        return task::park_poller(vm, thread, deadline, scheduler_pump);
    }
    // A sleeping task's deadline also ends the wait, so `task.wait` keeps
    // its timing while `fps = "auto"` idles for up to a second.
    let timeout = match task::next_due(vm) {
        Some(due) => timeout.min(due.saturating_duration_since(Instant::now())),
        None => timeout,
    };
    let events = pump_events(timeout)?;
    task::run_for(vm, TASK_BUDGET);
    Ok(vec![new_table(events)])
}

/// `__ui_clear(window, color)` fills the whole back buffer.
fn builtin_clear(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let (r, g, b, a) = color_arg(args, 1, "color")?;
    with_target(id, |win| {
        win.pixmap.fill(Color::from_rgba8(r, g, b, a));
        win.touch(Bounds::surface(win.pixmap.width(), win.pixmap.height()));
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_fill_rect(window, x, y, width, height, color, radius?, gradient?,
/// shadow?)`.
fn builtin_fill_rect(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let (id, x, y, w, h) = rect_args(args)?;
    let color = color_arg(args, 5, "color")?;
    let radius = corner_radius_arg(args, 6)?;
    let gradient = numbers_arg(args, 7, "gradient")?;
    let shadow = numbers_arg(args, 8, "shadow")?;
    with_target(id, |target| {
        if let Some(path) = rounded_rect(x, y, w, h, radius) {
            if gradient.is_none()
                && fill_flat(target, x, y, w, h, radius, color, shadow.as_deref(), &path)?
            {
                return Ok(());
            }
            fill_shape(target, &path, color, gradient.as_deref(), shadow.as_deref())?;
        }
        Ok(())
    })?;
    Ok(vec![])
}

/// A flat-colored rectangle is filled by `images` instead of the path
/// rasterizer, which is several times slower for the cards, panels and
/// highlights a UI fills every frame: a plain one with the coverage of its
/// edges worked out exactly, a rounded one through a cached coverage mask
/// placed to a quarter of a pixel. Returns false (nothing drawn) for a
/// shape too large to cache, which the rasterizer fills instead.
#[allow(clippy::too_many_arguments)]
fn fill_flat(
    target: &mut Target,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
    color: (u8, u8, u8, u8),
    shadow: Option<&[f64]>,
    path: &Path,
) -> Result<bool, String> {
    let scale = target.scale as f32;
    let (left, top) = (x * scale, y * scale);
    let (right, bottom) = ((x + w) * scale, (y + h) * scale);
    if !(left.is_finite() && top.is_finite() && right.is_finite() && bottom.is_finite()) {
        return Ok(false);
    }
    let (pw, ph) = (target.pixmap.width(), target.pixmap.height());
    let area = Bounds::covering(left, top, right, bottom, 0.0);
    let visible = clip::visible(target.state.clip(), area, pw, ph);
    let (r, g, b, a) = color;
    let premultiply = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
    let fill = [premultiply(r), premultiply(g), premultiply(b), a];
    let radius = radius.min(w / 2.0).min(h / 2.0) * scale;
    if radius <= 0.0 {
        if let Some(shadow) = shadow {
            draw_shadow(target, path, shadow)?;
        }
        if !visible.is_empty() {
            images::fill_rect_aa(
                target.pixmap,
                fill,
                (left, top, right, bottom),
                target.state.clip(),
            );
            target.touch(area);
        }
        return Ok(true);
    }
    let (x0, y0) = (left.floor(), top.floor());
    let (mw, mh) = ((right.ceil() - x0) as u64, (bottom.ceil() - y0) as u64);
    // Masks are cached by size: one larger than a screen goes through the
    // rasterizer, which only touches what is visible.
    if mw * mh > 16_000_000 {
        return Ok(false);
    }
    if let Some(shadow) = shadow {
        draw_shadow(target, path, shadow)?;
    }
    if visible.is_empty() {
        return Ok(true);
    }
    let Some(shape) =
        images::rounded_mask_at(left - x0, top - y0, right - left, bottom - top, radius)
    else {
        return Ok(true);
    };
    let (x0, y0) = (x0 as i32, y0 as i32);
    images::fill_solid(
        target.pixmap,
        fill,
        x0,
        y0,
        shape.width(),
        shape.height(),
        Some(&shape),
        target.state.clip(),
    );
    target.touch(Bounds::from_xywh(x0, y0, shape.width(), shape.height()));
    Ok(true)
}

/// `__ui_stroke_rect(window, x, y, width, height, color, radius?, lineWidth?)`.
fn builtin_stroke_rect(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let (id, x, y, w, h) = rect_args(args)?;
    let color = color_arg(args, 5, "color")?;
    let radius = corner_radius_arg(args, 6)?;
    let width = line_width_arg(args, 7)?;
    with_target(id, |win| {
        if let Some(path) = rounded_rect(x, y, w, h, radius) {
            win.stroke_path(&path, &paint(color), &stroke(width));
        }
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_line(window, x1, y1, x2, y2, color, lineWidth?)`.
fn builtin_line(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let x1 = float_arg(args, 1, "x1")? as f32;
    let y1 = float_arg(args, 2, "y1")? as f32;
    let x2 = float_arg(args, 3, "x2")? as f32;
    let y2 = float_arg(args, 4, "y2")? as f32;
    let color = color_arg(args, 5, "color")?;
    let width = line_width_arg(args, 6)?;
    with_target(id, |win| {
        let mut pb = PathBuilder::new();
        pb.move_to(x1, y1);
        pb.line_to(x2, y2);
        if let Some(path) = pb.finish() {
            win.stroke_path(&path, &paint(color), &stroke(width));
        }
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_circle(window, cx, cy, radius, color, fill, lineWidth?, gradient?,
/// shadow?)`.
fn builtin_circle(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let cx = float_arg(args, 1, "x")? as f32;
    let cy = float_arg(args, 2, "y")? as f32;
    let radius = float_arg(args, 3, "radius")? as f32;
    let color = color_arg(args, 4, "color")?;
    let fill = bool_arg(args, 5, "fill", true)?;
    let width = line_width_arg(args, 6)?;
    let gradient = numbers_arg(args, 7, "gradient")?;
    let shadow = numbers_arg(args, 8, "shadow")?;
    if radius < 0.0 {
        return Err("radius must be non-negative".to_string());
    }
    with_target(id, |win| {
        if let Some(path) = PathBuilder::from_circle(cx, cy, radius) {
            if fill {
                fill_shape(win, &path, color, gradient.as_deref(), shadow.as_deref())?;
            } else {
                win.stroke_path(&path, &paint(color), &stroke(width));
            }
        }
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_polygon(window, points, color, fill, lineWidth?, gradient?, shadow?)`
/// where `points`
/// is a flat `{x1, y1, x2, y2, ...}` array.
fn builtin_polygon(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let points = points_arg(args, 1, "points")?;
    let color = color_arg(args, 2, "color")?;
    let fill = bool_arg(args, 3, "fill", true)?;
    let width = line_width_arg(args, 4)?;
    let gradient = numbers_arg(args, 5, "gradient")?;
    let shadow = numbers_arg(args, 6, "shadow")?;
    if points.len() < 2 {
        return Err("points must hold at least two points".to_string());
    }
    with_target(id, |win| {
        if let Some(path) = polygon_path(&points) {
            if fill {
                fill_shape(win, &path, color, gradient.as_deref(), shadow.as_deref())?;
            } else {
                win.stroke_path(&path, &paint(color), &stroke(width));
            }
        }
        Ok(())
    })?;
    Ok(vec![])
}

type FontChain = Vec<(u64, Rc<fontdue::Font>)>;

/// Reads the text arguments `__ui_text` and `__ui_measure_text` share: the
/// size at `size_index`, then from `font_index` on `font?, fallback?,
/// maxWidth?, overflow?, align?, lineHeight?, letterSpacing?`. Returns the
/// options and the font chain.
fn text_args(
    args: &[Value],
    size_index: usize,
    font_index: usize,
) -> Result<(TextOptions, FontChain), String> {
    let size = float_arg(args, size_index, "size")?;
    if size <= 0.0 {
        return Err("size must be positive".to_string());
    }
    if size > MAX_TEXT_SIZE {
        return Err(format!("size must be at most {}", MAX_TEXT_SIZE));
    }
    let primary = match args.get(font_index) {
        None | Some(Value::Nil) => DEFAULT_FONT_ID,
        _ => integer_arg(args, font_index, "font")?,
    };
    let fallback = match args.get(font_index + 1) {
        None | Some(Value::Nil) => None,
        _ => Some(font_ids_arg(args, font_index + 1, "fallback")?),
    };
    let max_width = match args.get(font_index + 2) {
        None | Some(Value::Nil) => None,
        _ => {
            let max = float_arg(args, font_index + 2, "maxWidth")?;
            if max <= 0.0 {
                return Err("maxWidth must be positive".to_string());
            }
            Some(max as f32)
        }
    };
    let overflow = match optional_string_arg(args, font_index + 3, "overflow")?.as_deref() {
        None | Some("wrap") => Overflow::Wrap,
        Some("clip") => Overflow::Clip,
        Some("ellipsis") => Overflow::Ellipsis,
        Some(_) => return Err("overflow must be \"clip\", \"ellipsis\" or \"wrap\"".to_string()),
    };
    let align = match optional_string_arg(args, font_index + 4, "align")?.as_deref() {
        None | Some("left") => Align::Left,
        Some("center") => Align::Center,
        Some("right") => Align::Right,
        Some(_) => return Err("align must be \"left\", \"center\" or \"right\"".to_string()),
    };
    let line_height = match args.get(font_index + 5) {
        None | Some(Value::Nil) => None,
        _ => {
            let multiple = float_arg(args, font_index + 5, "lineHeight")?;
            if multiple <= 0.0 {
                return Err("lineHeight must be positive".to_string());
            }
            Some(multiple as f32)
        }
    };
    let letter_spacing = optional_float_arg(args, font_index + 6, "letterSpacing", 0.0)? as f32;
    let options = TextOptions {
        size: size as f32,
        max_width,
        overflow,
        align,
        line_height,
        letter_spacing,
    };
    Ok((options, font_chain(primary, fallback.as_deref())?))
}

/// `__ui_text(window, x, y, text, size, color, font?, fallback?, maxWidth?,
/// overflow?, align?, lineHeight?, letterSpacing?)` draws `text` with the
/// top-left of its block at `(x, y)` and returns the width and height it
/// covers.
fn builtin_text(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let x = float_arg(args, 1, "x")? as f32;
    let y = float_arg(args, 2, "y")? as f32;
    let text = string_arg(args, 3, "text")?;
    let color = color_arg(args, 5, "color")?;
    let (options, chain) = text_args(args, 4, 6)?;
    let layout = cached_layout(text, &chain, &options);
    with_target(id, |target| {
        let touched = GLYPHS.with(|cache| {
            draw_layout(
                target.pixmap,
                &mut cache.borrow_mut(),
                &chain,
                &layout,
                (x, y),
                target.scale as f32,
                color,
                target.state.clip(),
            )
        });
        target.touch(touched);
        Ok(())
    })?;
    Ok(vec![
        Value::Float(layout.width as f64),
        Value::Float(layout.height as f64),
    ])
}

/// `__ui_measure_text(text, size, font?, fallback?, maxWidth?, overflow?,
/// align?, lineHeight?, letterSpacing?)` returns the logical width and
/// height `__ui_text` would cover, and the number of lines.
fn builtin_measure_text(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let text = string_arg(args, 0, "text")?;
    let (options, chain) = text_args(args, 1, 2)?;
    let layout = cached_layout(text, &chain, &options);
    Ok(vec![
        Value::Float(layout.width as f64),
        Value::Float(layout.height as f64),
        integer(layout.lines as u64),
    ])
}

/// `__ui_load_image(path, maxPixels?, maxBytes?)` decodes a PNG, JPEG,
/// WebP, GIF or BMP file and returns what `register_image` describes.
fn builtin_load_image(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let path = string_arg(args, 0, "path")?;
    let limits = limits_arg(args, 1)?;
    // No image format compresses worse than raw pixels by much, so a file
    // over the memory limit could not decode within it anyway.
    let bytes = match crate::app::read(&path) {
        Some(bundled) => bundled.map_err(|err| format!("cannot open {}: {}", path, err))?,
        None => {
            let file = std::fs::File::open(&path)
                .map_err(|err| format!("cannot open {}: {}", path, err))?;
            let mut bytes = Vec::new();
            file.take(limits.max_bytes + 1)
                .read_to_end(&mut bytes)
                .map_err(|err| format!("cannot read {}: {}", path, err))?;
            bytes
        }
    };
    if bytes.len() as u64 > limits.max_bytes {
        return Err(format!(
            "cannot decode {}: file is over {} bytes (maxBytes)",
            path, limits.max_bytes
        ));
    }
    let decoded =
        images::decode(&bytes, limits).map_err(|err| format!("cannot decode {}: {}", path, err))?;
    Ok(register_image(ImageEntry::new(decoded, limits)))
}

/// `__ui_decode_image(data, maxPixels?, maxBytes?)` decodes an image held
/// in a buffer or string, like `__ui_load_image`.
fn builtin_decode_image(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let limits = limits_arg(args, 1)?;
    let decoded = match args.first() {
        Some(Value::Buffer(buffer)) => images::decode(&buffer.borrow().data, limits),
        Some(Value::String(s)) => images::decode(s.0.str.as_bytes(), limits),
        Some(value) => {
            return Err(format!(
                "data must be a buffer or string, got {}",
                value.type_name()
            ));
        }
        None => return Err("data must be provided".to_string()),
    }
    .map_err(|err| format!("cannot decode image: {}", err))?;
    Ok(register_image(ImageEntry::new(decoded, limits)))
}

/// `__ui_resize_image(image, width, height)` makes a new image with every
/// frame resampled to `width` × `height` pixels.
fn builtin_resize_image(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "image")?;
    let width = integer_arg(args, 1, "width")?;
    let height = integer_arg(args, 2, "height")?;
    if width == 0 || height == 0 || width > u32::MAX as u64 || height > u32::MAX as u64 {
        return Err("width and height must be positive whole numbers".to_string());
    }
    let (width, height) = (width as u32, height as u32);
    let entry = IMAGES.with(|images| {
        let images = images.borrow();
        let source = images
            .get(&id)
            .ok_or_else(|| "image was freed".to_string())?;
        let frame_bytes = images::check_size(width, height, source.limits)?;
        if frame_bytes * source.frames.len() as u64 > source.limits.max_bytes {
            return Err(format!(
                "resized image needs more than {} bytes (maxBytes)",
                source.limits.max_bytes
            ));
        }
        let full = (0.0, 0.0, source.width() as f32, source.height() as f32);
        let frames = source
            .frames
            .iter()
            .map(|frame| images::resample(&frame.pixmap, full, width, height))
            .collect::<Option<Vec<Pixmap>>>()
            .ok_or_else(|| "cannot resize image".to_string())?;
        let decoded = Decoded {
            frames,
            delays: source.delays.clone(),
            loop_count: source.loop_count,
        };
        Ok(ImageEntry::new(decoded, source.limits))
    })?;
    Ok(register_image(entry))
}

/// `__ui_free_image(image)`.
fn builtin_free_image(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "image")?;
    IMAGES.with(|images| images.borrow_mut().remove(&id));
    Ok(vec![])
}

/// `__ui_draw_image(window, image, x, y, width?, height?, frame?, opacity?,
/// radius?, fit?)` draws frame `frame` (from 1) of an image into the box at
/// `(x, y)`, which is the image's own size unless `width` and/or `height`
/// say otherwise, fitted by `fit` ("fill", "contain" or "cover").
///
/// The frame is resampled once to the physical size it covers and that copy
/// is cached, so redrawing it at the same size is a plain blit.
fn builtin_draw_image(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let image = integer_arg(args, 1, "image")?;
    let x = float_arg(args, 2, "x")? as f32;
    let y = float_arg(args, 3, "y")? as f32;
    let width = optional_float_arg(args, 4, "width", 0.0)? as f32;
    let height = optional_float_arg(args, 5, "height", 0.0)? as f32;
    let frame = match args.get(6) {
        None | Some(Value::Nil) => 1,
        _ => integer_arg(args, 6, "frame")?,
    };
    let opacity = optional_float_arg(args, 7, "opacity", 1.0)? as f32;
    let radius = optional_float_arg(args, 8, "radius", 0.0)? as f32;
    let fit = match args.get(9) {
        None | Some(Value::Nil) => Fit::Fill,
        _ => Fit::parse(&string_arg(args, 9, "fit")?)?,
    };
    if width < 0.0 || height < 0.0 {
        return Err("width and height must be non-negative".to_string());
    }
    if !(0.0..=1.0).contains(&opacity) {
        return Err("opacity must be from 0 to 1".to_string());
    }
    if radius < 0.0 {
        return Err("radius must be non-negative".to_string());
    }
    if image == id {
        return Err("a canvas cannot be drawn onto itself".to_string());
    }
    // A canvas is lifted out while it is drawn, so drawing it onto another
    // canvas does not borrow the table twice.
    if let Some(mut canvas) = CANVASES.with(|canvases| canvases.borrow_mut().remove(&image)) {
        let result = draw_canvas(id, &mut canvas, x, y, width, height, opacity, radius, fit);
        CANVASES.with(|canvases| canvases.borrow_mut().insert(image, canvas));
        result?;
        return Ok(vec![]);
    }
    IMAGES.with(|images| {
        let mut images = images.borrow_mut();
        let entry = images
            .get_mut(&image)
            .ok_or_else(|| "image was freed".to_string())?;
        let count = entry.frames.len() as u64;
        if frame == 0 || frame > count {
            return Err(format!("frame must be from 1 to {}", count));
        }
        let index = (frame - 1) as usize;
        let (image_w, image_h) = (entry.width() as f32, entry.height() as f32);
        let target = (
            x,
            y,
            if width > 0.0 { width } else { image_w },
            if height > 0.0 { height } else { image_h },
        );
        let (src, dest) = images::place(image_w, image_h, target, fit);
        with_target(id, |win| {
            let scale = win.scale as f32;
            let (left, top, w, h) = images::snap((
                dest.0 * scale,
                dest.1 * scale,
                dest.2 * scale,
                dest.3 * scale,
            ));
            if w == 0 || h == 0 || opacity == 0.0 {
                return Ok(());
            }
            let unscaled =
                src == (0.0, 0.0, image_w, image_h) && (w, h) == (entry.width(), entry.height());
            let prepared = if unscaled {
                &entry.frames[index]
            } else {
                let key = ScaleKey {
                    src,
                    width: w,
                    height: h,
                };
                match entry.prepared(index, key) {
                    Some(frame) => frame,
                    None => return Ok(()),
                }
            };
            images::draw(
                win.pixmap,
                prepared.pixmap.as_ref(),
                prepared.opaque,
                left,
                top,
                opacity,
                radius * scale,
                win.state.clip(),
            );
            win.touch(Bounds::from_xywh(left, top, w, h));
            Ok(())
        })
    })?;
    Ok(vec![])
}

/// `__ui_canvas(width, height, scale?)` makes an off-screen surface of
/// `width` × `height` logical pixels at `scale` physical pixels per logical
/// one (default 1), transparent at first, and returns its id. Drawing
/// natives take the id like a window's, and `__ui_draw_image` draws it.
fn builtin_canvas(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let width = float_arg(args, 0, "width")?;
    let height = float_arg(args, 1, "height")?;
    let scale = optional_float_arg(args, 2, "scale", 1.0)?;
    if width <= 0.0 || height <= 0.0 || scale <= 0.0 {
        return Err("width, height and scale must be positive".to_string());
    }
    let (pw, ph) = ((width * scale).ceil(), (height * scale).ceil());
    if pw > u32::MAX as f64 || ph > u32::MAX as f64 {
        return Err("canvas is too large".to_string());
    }
    let (pw, ph) = (pw as u32, ph as u32);
    images::check_size(pw, ph, Limits::default())?;
    let pixmap = Pixmap::new(pw, ph).ok_or_else(|| "canvas is too large".to_string())?;
    let id = next_id();
    CANVASES.with(|canvases| {
        canvases.borrow_mut().insert(
            id,
            Canvas {
                pixmap,
                scale,
                state: DrawState::default(),
                opaque: None,
            },
        )
    });
    Ok(vec![integer(id)])
}

/// `__ui_free_canvas(canvas)`.
fn builtin_free_canvas(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "canvas")?;
    CANVASES.with(|canvases| canvases.borrow_mut().remove(&id));
    Ok(vec![])
}

/// `__ui_push_clip(target, x, y, width, height, radius?)` limits drawing to
/// a (rounded) rectangle inside the current clip.
fn builtin_push_clip(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let (id, x, y, w, h) = rect_args(args)?;
    let radius = optional_float_arg(args, 5, "radius", 0.0)? as f32;
    if radius < 0.0 {
        return Err("radius must be non-negative".to_string());
    }
    with_target(id, |target| {
        if radius == 0.0 {
            return push_clip_rect(target, x, y, w, h);
        }
        let path = rounded_rect(x, y, w, h, radius);
        push_clip(target, path.as_ref())
    })?;
    Ok(vec![])
}

/// `__ui_push_clip_path(target, path)` limits drawing to the inside of a
/// path.
fn builtin_push_clip_path(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "target")?;
    let path = path_arg(args, 1)?;
    with_target(id, |target| push_clip(target, Some(&path)))?;
    Ok(vec![])
}

/// `__ui_pop_clip(target)`.
fn builtin_pop_clip(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "target")?;
    with_target(id, pop_clip)?;
    Ok(vec![])
}

/// `__ui_push_opacity(target, opacity)`.
fn builtin_push_opacity(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "target")?;
    let opacity = float_arg(args, 1, "opacity")? as f32;
    if !(0.0..=1.0).contains(&opacity) {
        return Err("opacity must be from 0 to 1".to_string());
    }
    with_target(id, |target| push_opacity(target, opacity))?;
    Ok(vec![])
}

/// `__ui_pop_opacity(target)`.
fn builtin_pop_opacity(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "target")?;
    with_target(id, pop_opacity)?;
    Ok(vec![])
}

/// `__ui_fill_path(target, path, color, gradient?, shadow?, evenOdd?)`.
fn builtin_fill_path(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "target")?;
    let path = path_arg(args, 1)?;
    let color = color_arg(args, 2, "color")?;
    let gradient = numbers_arg(args, 3, "gradient")?;
    let shadow = numbers_arg(args, 4, "shadow")?;
    let even_odd = bool_arg(args, 5, "evenOdd", false)?;
    with_target(id, |target| {
        if even_odd {
            // Shadows and gradients go through `fill_shape`, which fills
            // with the winding rule; even-odd is the plain-color case.
            if gradient.is_some() || shadow.is_some() {
                return Err("evenOdd cannot be combined with a gradient or shadow".to_string());
            }
            target.fill_path(&path, &paint(color), FillRule::EvenOdd);
            return Ok(());
        }
        fill_shape(target, &path, color, gradient.as_deref(), shadow.as_deref())
    })?;
    Ok(vec![])
}

/// `__ui_stroke_path(target, path, color, lineWidth?, cap?, join?)`.
fn builtin_stroke_path(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "target")?;
    let path = path_arg(args, 1)?;
    let color = color_arg(args, 2, "color")?;
    let width = line_width_arg(args, 3)?;
    let cap = match args.get(4) {
        None | Some(Value::Nil) => "butt".to_string(),
        _ => string_arg(args, 4, "cap")?,
    };
    let join = match args.get(5) {
        None | Some(Value::Nil) => "miter".to_string(),
        _ => string_arg(args, 5, "join")?,
    };
    let stroke = styled_stroke(width, &cap, &join)?;
    with_target(id, |target| {
        target.stroke_path(&path, &paint(color), &stroke);
        Ok(())
    })?;
    Ok(vec![])
}

/// The largest blur radius, in logical pixels, a script may ask for.
const MAX_BLUR: f64 = 200.0;

/// `__ui_blur_region(target, x, y, width, height, radius, cornerRadius?)`
/// blurs what is already drawn under a rectangle, the way a CSS
/// `backdrop-filter` does. `radius` is the gaussian's standard deviation.
fn builtin_blur_region(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let (id, x, y, w, h) = rect_args(args)?;
    let sigma = float_arg(args, 5, "radius")?;
    let corner = optional_float_arg(args, 6, "cornerRadius", 0.0)? as f32;
    if !(0.0..=MAX_BLUR).contains(&sigma) {
        return Err(format!("radius must be from 0 to {}", MAX_BLUR));
    }
    if corner < 0.0 {
        return Err("cornerRadius must be non-negative".to_string());
    }
    if sigma == 0.0 {
        return Ok(vec![]);
    }
    with_target(id, |target| {
        let scale = target.scale as f32;
        let (left, top, width, height) = images::snap((x * scale, y * scale, w * scale, h * scale));
        let (pw, ph) = (target.pixmap.width() as i64, target.pixmap.height() as i64);
        // Blur a margin around the rectangle too, so its edges see their
        // surroundings instead of repeating their own border.
        let pad = (sigma as f32 * scale * 3.0).ceil() as i64;
        let x0 = (left as i64 - pad).max(0);
        let y0 = (top as i64 - pad).max(0);
        let x1 = (left as i64 + width as i64 + pad).min(pw);
        let y1 = (top as i64 + height as i64 + pad).min(ph);
        if x1 <= x0 || y1 <= y0 || width == 0 || height == 0 {
            return Ok(());
        }
        let region = Bounds {
            x0: x0 as i32,
            y0: y0 as i32,
            x1: x1 as i32,
            y1: y1 as i32,
        };
        let inner = Bounds::from_xywh(left, top, width, height);
        let cover = if corner > 0.0 {
            Some(
                images::rounded_mask(width, height, corner * scale)
                    .ok_or_else(|| "cannot allocate blur".to_string())?,
            )
        } else {
            None
        };
        let start = Instant::now();
        effects::blur_behind(
            target.pixmap,
            region,
            inner,
            sigma as f32 * scale,
            cover
                .as_deref()
                .map(|mask| (mask, (corner * scale).to_bits())),
            target.state.clip(),
        );
        let spent = start.elapsed().as_secs_f64();
        STATS.with(|stats| stats.borrow_mut().blur += spent);
        target.touch(inner);
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_blur_image(image, radius)` makes a new image with every frame
/// blurred by a gaussian of standard deviation `radius` pixels.
fn builtin_blur_image(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "image")?;
    let sigma = float_arg(args, 1, "radius")?;
    if !(0.0..=MAX_BLUR).contains(&sigma) {
        return Err(format!("radius must be from 0 to {}", MAX_BLUR));
    }
    let entry = IMAGES.with(|images| {
        let images = images.borrow();
        let source = images
            .get(&id)
            .ok_or_else(|| "image was freed".to_string())?;
        let frames = source
            .frames
            .iter()
            .map(|frame| {
                let mut pixmap = frame.pixmap.clone();
                timed_blur(&mut pixmap, sigma as f32);
                pixmap
            })
            .collect();
        let decoded = Decoded {
            frames,
            delays: source.delays.clone(),
            loop_count: source.loop_count,
        };
        Ok::<ImageEntry, String>(ImageEntry::new(decoded, source.limits))
    })?;
    Ok(register_image(entry))
}

/// `__ui_get_pixel(target, x, y)` returns the red, green, blue and alpha
/// (0 to 255, not premultiplied) of the pixel under the logical point.
fn builtin_get_pixel(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "target")?;
    let x = float_arg(args, 1, "x")?;
    let y = float_arg(args, 2, "y")?;
    let pixel = with_target(id, |target| {
        let (px, py) = ((x * target.scale).floor(), (y * target.scale).floor());
        let (w, h) = (target.pixmap.width() as f64, target.pixmap.height() as f64);
        if px < 0.0 || py < 0.0 || px >= w || py >= h {
            return Err("pixel is outside the surface".to_string());
        }
        let pixel = target.pixmap.pixels()[(py as usize) * (w as usize) + px as usize];
        Ok(pixel.demultiply())
    })?;
    Ok(vec![
        integer(pixel.red() as u64),
        integer(pixel.green() as u64),
        integer(pixel.blue() as u64),
        integer(pixel.alpha() as u64),
    ])
}

/// Draws a canvas like an image: `__ui_draw_image` with a canvas id lands
/// here. The canvas is resampled to the size it covers unless it is drawn
/// one to one.
#[allow(clippy::too_many_arguments)]
fn draw_canvas(
    id: u64,
    canvas: &mut Canvas,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    opacity: f32,
    radius: f32,
    fit: Fit,
) -> Result<(), String> {
    let (pixel_w, pixel_h) = (canvas.pixmap.width(), canvas.pixmap.height());
    let scale = canvas.scale as f32;
    let target = (
        x,
        y,
        if width > 0.0 {
            width
        } else {
            pixel_w as f32 / scale
        },
        if height > 0.0 {
            height
        } else {
            pixel_h as f32 / scale
        },
    );
    let (src, dest) = images::place(pixel_w as f32, pixel_h as f32, target, fit);
    let opaque = canvas.is_opaque();
    with_target(id, |dst| {
        let scale = dst.scale as f32;
        let (left, top, w, h) = images::snap((
            dest.0 * scale,
            dest.1 * scale,
            dest.2 * scale,
            dest.3 * scale,
        ));
        if w == 0 || h == 0 || opacity == 0.0 {
            return Ok(());
        }
        let resampled;
        let source =
            if src == (0.0, 0.0, pixel_w as f32, pixel_h as f32) && (w, h) == (pixel_w, pixel_h) {
                &canvas.pixmap
            } else {
                resampled = images::resample(&canvas.pixmap, src, w, h)
                    .ok_or_else(|| "cannot scale canvas".to_string())?;
                &resampled
            };
        images::draw(
            dst.pixmap,
            source.as_ref(),
            opaque,
            left,
            top,
            opacity,
            radius * scale,
            dst.state.clip(),
        );
        dst.touch(Bounds::from_xywh(left, top, w, h));
        Ok(())
    })
}

/// `__ui_load_font(path)` reads a TrueType or OpenType font and returns its
/// id and full name.
fn builtin_load_font(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let path = string_arg(args, 0, "path")?;
    Ok(register_font(load_font(read_asset(&path)?, 0)?))
}

/// `__ui_load_font_bytes(buffer)` is `__ui_load_font` for a font already in
/// memory.
fn builtin_load_font_bytes(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let bytes = match args.first() {
        Some(Value::Buffer(buffer)) => buffer.borrow().data.clone(),
        Some(value) => {
            return Err(format!(
                "font must be a path or a buffer, got {}",
                value.type_name()
            ));
        }
        None => return Err("font must be provided".to_string()),
    };
    Ok(register_font(load_font(bytes, 0)?))
}

/// `__ui_system_font(family, weight?, italic?)` loads the installed font
/// that best matches and returns its id, full name and path, or nothing
/// when no installed font has that family.
fn builtin_system_font(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let family = string_arg(args, 0, "family")?;
    let weight = optional_float_arg(args, 1, "weight", 400.0)?;
    if !(1.0..=1000.0).contains(&weight) {
        return Err("weight must be from 1 to 1000".to_string());
    }
    let italic = bool_arg(args, 2, "italic", false)?;
    let Some((path, index)) = find_system_font(&family, weight as u16, italic) else {
        return Ok(vec![Value::Nil]);
    };
    let shown = path.display().to_string();
    let bytes = std::fs::read(&path).map_err(|err| format!("cannot open {}: {}", shown, err))?;
    let mut result = register_font(load_font(bytes, index)?);
    result.push(Value::string(shown));
    Ok(result)
}

/// `__ui_free_font(font)`.
fn builtin_free_font(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "font")?;
    if id == DEFAULT_FONT_ID {
        return Err("the default font cannot be freed".to_string());
    }
    FONTS.with(|fonts| fonts.borrow_mut().remove(&id));
    FALLBACKS.with(|fallbacks| fallbacks.borrow_mut().retain(|&other| other != id));
    GLYPHS.with(|cache| cache.borrow_mut().forget_font(id));
    LAYOUTS.with(|cache| cache.borrow_mut().clear());
    Ok(vec![])
}

/// `__ui_set_fallback_fonts(fonts)` sets the fonts every text looks in, in
/// order, for characters its own font lacks.
fn builtin_set_fallback_fonts(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let ids = font_ids_arg(args, 0, "fonts")?;
    for &id in &ids {
        font(id)?;
    }
    FALLBACKS.with(|fallbacks| *fallbacks.borrow_mut() = ids);
    Ok(vec![])
}

/// `__ui_present(window, dirty?)` shows what has been drawn since the last
/// call. `dirty` is a flat `{x, y, w, h, ...}` array of logical rectangles
/// that changed; when given, only those are copied to the screen.
fn builtin_present(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let dirty = match args.get(1) {
        None | Some(Value::Nil) => None,
        _ => Some(points_arg(args, 1, "dirty")?),
    };
    let started = Instant::now();
    with_window(id, |win| {
        if !win.state.groups.is_empty() || !win.state.clips.is_empty() {
            // Drop the unfinished layers so the next frame starts clean.
            let state = std::mem::take(&mut win.state);
            if let Some(first) = state.groups.into_iter().next() {
                win.pixmap = first.saved;
            }
            return Err("popClip or popOpacity is missing before present".to_string());
        }
        let rects = dirty.as_ref().map(|points| {
            let scale = win.scale as f32;
            points
                .as_chunks::<2>()
                .0
                .iter()
                .map(|&[(x, y), (w, h)]| {
                    let (x0, y0) = ((x * scale).floor(), (y * scale).floor());
                    let (x1, y1) = (((x + w) * scale).ceil(), ((y + h) * scale).ceil());
                    (x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32)
                })
                .collect::<Vec<_>>()
        });
        blit(win, rects.as_deref())
    })?;
    let now = Instant::now();
    STATS.with(|stats| {
        let mut stats = stats.borrow_mut();
        let present = now.duration_since(started).as_secs_f64();
        let since = stats
            .last_end
            .map_or(0.0, |end| started.duration_since(end).as_secs_f64());
        stats.draw_ms = (since - stats.blur).max(0.0) * 1000.0;
        stats.blur_ms = stats.blur * 1000.0;
        stats.present_ms = present * 1000.0;
        stats.frame_ms = (since + present) * 1000.0;
        stats.blur = 0.0;
        stats.frames += 1;
        stats.last_end = Some(now);
    });
    Ok(vec![])
}

/// `__ui_stats()` returns the timing of the last presented frame in
/// milliseconds: `draw` (everything between presents except blur), `blur`,
/// `present` and `frame` (all three together), plus the `frames` presented so
/// far.
fn builtin_stats(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    let stats = STATS.with(|stats| {
        let stats = stats.borrow();
        record(vec![
            ("frames", integer(stats.frames)),
            ("draw", Value::Float(stats.draw_ms)),
            ("blur", Value::Float(stats.blur_ms)),
            ("present", Value::Float(stats.present_ms)),
            ("frame", Value::Float(stats.frame_ms)),
        ])
    });
    Ok(vec![stats])
}

/// A non-negative number of seconds small enough for every backend's clock.
fn optional_seconds_arg(args: &[Value], index: usize, name: &str) -> Result<Option<f64>, String> {
    match args.get(index) {
        None | Some(Value::Nil) => Ok(None),
        _ => {
            let seconds = float_arg(args, index, name)?;
            if !(0.0..=1e9).contains(&seconds) {
                return Err(format!("{} must be a number of seconds from 0", name));
            }
            Ok(Some(seconds))
        }
    }
}

/// `__ui_media_session(window, title?, artist?, album?, artwork?, image?,
/// duration?, position?, playing)` publishes now-playing metadata through
/// window `window`, starting the session on first use (or when the window
/// changes). Returns true, or false and the reason when the OS refused.
fn builtin_media_session(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let info = NowPlaying {
        title: optional_string_arg(args, 1, "title")?.unwrap_or_default(),
        artist: optional_string_arg(args, 2, "artist")?.unwrap_or_default(),
        album: optional_string_arg(args, 3, "album")?.unwrap_or_default(),
        artwork: optional_string_arg(args, 4, "artwork")?,
        artwork_image: match args.get(5) {
            None | Some(Value::Nil) => None,
            _ => Some(integer_arg(args, 5, "artwork")?),
        },
        duration: optional_seconds_arg(args, 6, "duration")?,
        position: optional_seconds_arg(args, 7, "position")?,
        playing: bool_arg(args, 8, "playing", false)?,
    };
    with_window(id, |_| Ok(()))?;
    let (cover_url, new_file) = prepare_artwork(&info)?;

    let outcome = MEDIA.with(|media| {
        let mut media = media.borrow_mut();
        if media.as_ref().is_some_and(|session| session.window != id) {
            *media = None;
        }
        if media.is_none() {
            match create_media_session(id) {
                Ok(session) => *media = Some(session),
                Err(err) => {
                    if let Some(unused) = &new_file {
                        let _ = std::fs::remove_file(unused);
                    }
                    return Err(err);
                }
            }
        }
        match media.as_mut() {
            Some(session) => update_media_session(session, &info, cover_url, new_file),
            None => Ok(()),
        }
    });
    Ok(match outcome {
        Ok(()) => vec![Value::Bool(true)],
        Err(err) => {
            // A session that failed to publish is not worth keeping: it
            // would also swallow the focused-window media keys.
            MEDIA.with(|media| *media.borrow_mut() = None);
            vec![Value::Bool(false), Value::string(err)]
        }
    })
}

/// `__ui_media_session_clear()` ends the media session, removing the app
/// from the OS media controls.
fn builtin_media_session_clear(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
    MEDIA.with(|media| *media.borrow_mut() = None);
    Ok(vec![])
}

/// `__ui_drag(window)` lets the window system move the window with the
/// mouse, as if its title bar were being dragged. Only works while a button
/// is held; returns whether the system took over.
fn builtin_drag(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let started = with_window(id, |win| Ok(win.window.drag_window().is_ok()))?;
    Ok(vec![Value::Bool(started)])
}

/// `__ui_drag_resize(window, direction)` lets the window system resize the
/// window from an edge or corner with the mouse; returns whether it did.
fn builtin_drag_resize(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let direction = resize_direction(&string_arg(args, 1, "direction")?)?;
    let started = with_window(id, |win| {
        Ok(win.window.drag_resize_window(direction).is_ok())
    })?;
    Ok(vec![Value::Bool(started)])
}

/// `__ui_minimize(window)`.
fn builtin_minimize(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    with_window(id, |win| {
        win.window.set_minimized(true);
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_set_maximized(window, maximized)`.
fn builtin_set_maximized(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let maximized = bool_arg(args, 1, "maximized", true)?;
    with_window(id, |win| {
        win.window.set_maximized(maximized);
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_restore(window)` brings a minimized window back, or else returns a
/// maximized one to its normal size.
fn builtin_restore(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    with_window(id, |win| {
        if win.window.is_minimized().unwrap_or(win.minimized) {
            win.window.set_minimized(false);
        } else {
            win.window.set_maximized(false);
        }
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_is_maximized(window)`.
fn builtin_is_maximized(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let maximized = with_window(id, |win| Ok(win.window.is_maximized()))?;
    Ok(vec![Value::Bool(maximized)])
}

/// `__ui_set_fullscreen(window, fullscreen)` switches to borderless
/// fullscreen on the window's current monitor and back.
fn builtin_set_fullscreen(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let fullscreen = bool_arg(args, 1, "fullscreen", true)?;
    with_window(id, |win| {
        win.window
            .set_fullscreen(fullscreen.then_some(Fullscreen::Borderless(None)));
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_set_cursor(window, name)` changes the pointer shown over the window.
fn builtin_set_cursor(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let icon = cursor_icon(&string_arg(args, 1, "cursor")?)?;
    with_window(id, |win| {
        match icon {
            Some(icon) => {
                win.window.set_cursor(icon);
                win.window.set_cursor_visible(true);
            }
            None => win.window.set_cursor_visible(false),
        }
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_set_text_input(window, enabled, x?, y?, width?, height?)` turns the
/// input method on or off and places its candidate window next to the
/// logical rectangle of the text being edited.
fn builtin_set_text_input(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let enabled = bool_arg(args, 1, "enabled", true)?;
    let area = match args.get(2) {
        None | Some(Value::Nil) => None,
        _ => {
            let x = float_arg(args, 2, "x")?;
            let y = float_arg(args, 3, "y")?;
            let w = optional_float_arg(args, 4, "width", 0.0)?;
            let h = optional_float_arg(args, 5, "height", 0.0)?;
            if w < 0.0 || h < 0.0 {
                return Err("width and height must be non-negative".to_string());
            }
            Some((x, y, w, h))
        }
    };
    with_window(id, |win| {
        if win.ime_allowed != enabled {
            win.ime_allowed = enabled;
            win.window.set_ime_allowed(enabled);
            if !enabled {
                win.composing = false;
            }
        }
        if let (true, Some((x, y, w, h))) = (enabled, area) {
            win.window
                .set_ime_cursor_area(LogicalPosition::new(x, y), LogicalSize::new(w, h));
        }
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_focus(window)` brings the window to the front and gives it the
/// keyboard, as far as the window system allows.
fn builtin_focus(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    with_window(id, |win| {
        win.window.focus_window();
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_request_attention(window)` flashes the taskbar entry or bounces the
/// dock icon until the window is focused.
fn builtin_request_attention(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    with_window(id, |win| {
        win.window
            .request_user_attention(Some(UserAttentionType::Informational));
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_set_icon(window, image?)` sets the title bar and taskbar icon, or
/// clears it when `image` is nil.
fn builtin_set_icon(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let id = integer_arg(args, 0, "window")?;
    let icon = icon_arg(args, 1)?;
    with_window(id, |win| {
        win.window.set_window_icon(icon);
        Ok(())
    })?;
    Ok(vec![])
}

/// `__ui_open_url(url)` opens an http, https or mailto link in the default
/// browser or mail client without waiting for it.
fn builtin_open_url(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
    let url = string_arg(args, 0, "url")?;
    check_url(&url)?;
    open::that_detached(&url).map_err(|err| format!("cannot open {}: {}", url, err))?;
    Ok(vec![])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_unpack_as_rgba() {
        assert_eq!(unpack_color(0x11223344), (0x11, 0x22, 0x33, 0x44));
        assert_eq!(unpack_color(0xff0000ff), (255, 0, 0, 255));
    }

    #[test]
    fn color_arg_rejects_out_of_range_values() {
        let err = color_arg(&[Value::Float(1e12)], 0, "color").unwrap_err();
        assert!(err.contains("packed 0xRRGGBBAA"), "{}", err);
        let err = color_arg(&[Value::Float(-1.0)], 0, "color").unwrap_err();
        assert!(err.contains("packed 0xRRGGBBAA"), "{}", err);
        let err = color_arg(&[Value::Float(1.5)], 0, "color").unwrap_err();
        assert!(err.contains("packed 0xRRGGBBAA"), "{}", err);
        let err = color_arg(&[Value::Bool(true)], 0, "color").unwrap_err();
        assert!(err.contains("color string"), "{}", err);
        let err = color_arg(&[Value::Nil], 0, "color").unwrap_err();
        assert!(err.contains("color string"), "{}", err);
    }

    #[test]
    fn colors_are_read_from_strings_and_tables_as_packcolor_does() {
        let string = |text: &str| color_arg(&[Value::String(text.into())], 0, "color");
        assert_eq!(string("#ff8000").unwrap(), (255, 128, 0, 255));
        assert_eq!(string("#FF800080").unwrap(), (255, 128, 0, 128));
        assert_eq!(string("#f80").unwrap(), (255, 136, 0, 255));
        assert_eq!(string("#f808").unwrap(), (255, 136, 0, 136));
        for bad in ["ff0000", "#12345", "#gg0000", "#", "#ff00001"] {
            let err = string(bad).unwrap_err();
            assert!(err.contains("#rgb"), "{}: {}", bad, err);
        }
        let table = |array: Vec<Value>| {
            let t = Rc::new(RefCell::new(VmTable::new()));
            t.borrow_mut().array = array;
            color_arg(&[Value::Table(t)], 0, "color")
        };
        let n = |v: f64| Value::Float(v);
        assert_eq!(table(vec![n(1.0), n(2.4), n(2.6)]).unwrap(), (1, 2, 3, 255));
        assert_eq!(
            table(vec![n(1.0), n(2.0), n(3.0), n(4.0)]).unwrap(),
            (1, 2, 3, 4)
        );
        let err = table(vec![n(256.0), n(0.0), n(0.0)]).unwrap_err();
        assert!(
            err.contains("color.r must be a number from 0 to 255"),
            "{}",
            err
        );
        let err = table(vec![n(1.0), Value::String("1".into()), n(0.0)]).unwrap_err();
        assert!(err.contains("color.g must be"), "{}", err);
        let err = table(vec![n(1.0), n(2.0)]).unwrap_err();
        assert!(err.contains("color.b must be"), "{}", err);
    }

    #[test]
    fn packing_drops_alpha_and_orders_channels() {
        let pixels = [10, 20, 30, 255];
        let mut out = [0u32; 1];
        pack_pixels(&pixels, &mut out, false);
        assert_eq!(out[0], 0x000a141e);
    }

    #[test]
    fn transparent_windows_keep_alpha() {
        let pixels = [10, 20, 30, 128];
        let mut out = [0u32; 1];
        pack_pixels(&pixels, &mut out, true);
        assert_eq!(out[0], 0x800a141e);
    }

    #[test]
    fn resize_directions_parse() {
        assert_eq!(resize_direction("se").unwrap(), ResizeDirection::SouthEast);
        assert_eq!(resize_direction("N").unwrap(), ResizeDirection::North);
        assert_eq!(resize_direction("w").unwrap(), ResizeDirection::West);
        let err = resize_direction("up").unwrap_err();
        assert!(err.contains("ne, nw, se or sw"), "{}", err);
    }

    #[test]
    fn cursor_names_map_to_icons() {
        assert_eq!(cursor_icon("pointer").unwrap(), Some(CursorIcon::Pointer));
        assert_eq!(cursor_icon("ewResize").unwrap(), Some(CursorIcon::EwResize));
        assert_eq!(cursor_icon("hidden").unwrap(), None);
        let err = cursor_icon("hand").unwrap_err();
        assert!(err.contains("pointer") && err.contains("hidden"), "{}", err);
        // Names are camelCase, like the rest of the library.
        assert!(cursor_icon("ew-resize").is_err());
    }

    #[test]
    fn only_web_and_mail_links_open() {
        assert!(check_url("https://example.com/a?b=c").is_ok());
        assert!(check_url("HTTP://example.com").is_ok());
        assert!(check_url("mailto:someone@example.com").is_ok());
        for bad in [
            "file:///etc/passwd",
            "C:\\Windows\\System32\\calc.exe",
            "javascript:alert(1)",
            "ms-settings:",
            "/usr/bin/xterm",
            "",
        ] {
            assert!(check_url(bad).is_err(), "{} should be refused", bad);
        }
        assert!(check_url("https://").is_err());
        assert!(check_url("https://example.com/a b").is_err());
        assert!(check_url("https://example.com/\n--flag").is_err());
    }

    #[test]
    fn clicks_count_up_when_quick_and_close() {
        let mut clicks = Clicks::default();
        let start = Instant::now();
        assert_eq!(clicks.press(start, 10.0, 10.0, MouseButton::Left), 1);
        let soon = start + Duration::from_millis(200);
        assert_eq!(clicks.press(soon, 11.0, 12.0, MouseButton::Left), 2);
        let sooner = soon + Duration::from_millis(200);
        assert_eq!(clicks.press(sooner, 11.0, 12.0, MouseButton::Left), 3);
        // Too late, too far or another button starts over.
        let late = sooner + Duration::from_millis(900);
        assert_eq!(clicks.press(late, 11.0, 12.0, MouseButton::Left), 1);
        let far = late + Duration::from_millis(100);
        assert_eq!(clicks.press(far, 40.0, 12.0, MouseButton::Left), 1);
        let other = far + Duration::from_millis(100);
        assert_eq!(clicks.press(other, 40.0, 12.0, MouseButton::Right), 1);
    }

    #[test]
    fn window_state_changes_name_their_event() {
        // (maximized, minimized)
        assert_eq!(
            state_change((false, false), (true, false)),
            Some("maximize")
        );
        assert_eq!(
            state_change((false, false), (false, true)),
            Some("minimize")
        );
        assert_eq!(state_change((true, false), (true, true)), Some("minimize"));
        assert_eq!(state_change((true, false), (false, false)), Some("restore"));
        assert_eq!(state_change((false, true), (false, false)), Some("restore"));
        assert_eq!(state_change((true, true), (true, false)), Some("restore"));
        assert_eq!(state_change((true, false), (true, false)), None);
        assert_eq!(state_change((false, false), (false, false)), None);
    }

    #[test]
    fn ime_events_carry_phase_text_and_cursor() {
        assert_eq!(ime_fields(&Ime::Enabled), ("start", String::new(), None));
        assert_eq!(
            ime_fields(&Ime::Preedit("啊b".to_string(), Some((3, 3)))),
            ("preedit", "啊b".to_string(), Some(3))
        );
        assert_eq!(
            ime_fields(&Ime::Preedit(String::new(), None)),
            ("preedit", String::new(), None)
        );
        assert_eq!(
            ime_fields(&Ime::Commit("啊不".to_string())),
            ("commit", "啊不".to_string(), None)
        );
        assert_eq!(ime_fields(&Ime::Disabled), ("end", String::new(), None));
    }

    #[test]
    fn event_tables_start_with_kind_and_window() {
        let Value::Table(table) = event_record(7, "dropfile", vec![("path", text("a.txt"))]) else {
            panic!("an event is a table");
        };
        let table = table.borrow();
        let field = |name: &str| table.fields.get(name).cloned();
        assert!(matches!(field("kind"), Some(Value::String(s)) if s.to_string() == "dropfile"));
        assert!(matches!(field("path"), Some(Value::String(s)) if s.to_string() == "a.txt"));
        assert!(field("window").is_some());
    }

    #[test]
    fn icons_are_demultiplied() {
        let pixmap = images::rgba_to_pixmap(vec![255, 0, 0, 128, 0, 0, 255, 255], 2, 1).unwrap();
        assert!(icon_from_pixmap(&pixmap).is_ok());
    }

    #[test]
    fn mask_blending_covers_and_clips() {
        let mut pixmap = Pixmap::new(4, 4).unwrap();
        pixmap.fill(Color::WHITE);
        // A 2×2 fully covered mask drawn half outside the pixmap.
        let open = (i32::MIN, i32::MAX);
        blend_mask(
            &mut pixmap,
            (3, 3),
            (2, 2),
            &[255; 4],
            (255, 0, 0, 255),
            open,
            None,
        );
        let pixels = pixmap.pixels();
        assert_eq!(
            (pixels[15].red(), pixels[15].green(), pixels[15].blue()),
            (255, 0, 0)
        );
        assert_eq!(
            (pixels[0].red(), pixels[0].green(), pixels[0].blue()),
            (255, 255, 255)
        );
        // Half coverage of an opaque color leaves a blend.
        blend_mask(
            &mut pixmap,
            (0, 0),
            (1, 1),
            &[128],
            (0, 0, 255, 255),
            open,
            None,
        );
        let blended = pixmap.pixels()[0];
        assert!(blended.red() < 255 && blended.red() > 0);
        assert_eq!(blended.alpha(), 255);
        // Columns outside the clip range are left alone.
        blend_mask(
            &mut pixmap,
            (0, 1),
            (4, 1),
            &[255; 4],
            (0, 0, 0, 255),
            (1, 3),
            None,
        );
        let row: Vec<u8> = pixmap.pixels()[4..8].iter().map(|p| p.red()).collect();
        assert_eq!(row, vec![255, 0, 0, 255]);
    }

    // TEXT -----------------------------------------------------------------

    /// The text from the fallback acceptance case: kana and kanji, an em
    /// dash and an emoji around Latin text.
    const MIXED: &str = "ループザルーム (feat. 初音ミク) — rusino 🎵";

    /// Builds a minimal TrueType font in which each of `chars` is a filled
    /// box `advance` units wide (1000 units per em, ascender 800, descender
    /// -200), with an empty "missing glyph". Small, made from nothing, and
    /// enough to test fallback without shipping a real CJK font.
    fn box_font(chars: &[char], advance: u16) -> Vec<u8> {
        fn u16s(out: &mut Vec<u8>, values: &[u16]) {
            for value in values {
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
        fn u32s(out: &mut Vec<u8>, values: &[u32]) {
            for value in values {
                out.extend_from_slice(&value.to_be_bytes());
            }
        }
        let mut chars = chars.to_vec();
        chars.sort_unstable();
        chars.dedup();
        let glyphs = chars.len() as u16 + 1;
        let (left, right, top) = (100u16, advance - 100, 700u16);

        // cmap: one format 12 subtable (Windows, full Unicode).
        let mut cmap = Vec::new();
        u16s(&mut cmap, &[0, 1, 3, 10]);
        u32s(&mut cmap, &[12]);
        u16s(&mut cmap, &[12, 0]);
        u32s(
            &mut cmap,
            &[16 + 12 * chars.len() as u32, 0, chars.len() as u32],
        );
        for (i, &ch) in chars.iter().enumerate() {
            u32s(&mut cmap, &[ch as u32, ch as u32, i as u32 + 1]);
        }

        // glyf and loca: glyph 0 is empty, the rest are the same box,
        // drawn clockwise from its bottom-left corner.
        let mut glyph = Vec::new();
        u16s(&mut glyph, &[1, left, 0, right, top, 3, 0]);
        glyph.extend_from_slice(&[1, 1, 1, 1]);
        u16s(&mut glyph, &[left, 0, right - left, 0]);
        u16s(&mut glyph, &[0, top, 0, (-(top as i16)) as u16]);
        glyph.resize(glyph.len().next_multiple_of(4), 0);
        let mut glyf = Vec::new();
        let mut loca = Vec::new();
        u32s(&mut loca, &[0, 0]);
        for _ in &chars {
            glyf.extend_from_slice(&glyph);
            u32s(&mut loca, &[glyf.len() as u32]);
        }

        let mut head = Vec::new();
        u32s(&mut head, &[0x0001_0000, 0x0001_0000, 0, 0x5F0F_3CF5]);
        u16s(&mut head, &[0x000B, 1000]);
        head.extend_from_slice(&[0; 16]);
        u16s(&mut head, &[left, 0, right, top, 0, 8, 2, 1, 0]);

        let mut hhea = Vec::new();
        u32s(&mut hhea, &[0x0001_0000]);
        u16s(
            &mut hhea,
            &[800, (-200i16) as u16, 0, advance, left, 100, right, 1],
        );
        u16s(&mut hhea, &[0, 0, 0, 0, 0, 0, 0, glyphs]);

        let mut hmtx = Vec::new();
        for _ in 0..glyphs {
            u16s(&mut hmtx, &[advance, left]);
        }

        let mut maxp = Vec::new();
        u32s(&mut maxp, &[0x0000_5000]);
        u16s(&mut maxp, &[glyphs]);

        let tables: [(&[u8; 4], Vec<u8>); 7] = [
            (b"cmap", cmap),
            (b"glyf", glyf),
            (b"head", head),
            (b"hhea", hhea),
            (b"hmtx", hmtx),
            (b"loca", loca),
            (b"maxp", maxp),
        ];
        let mut font = Vec::new();
        u32s(&mut font, &[0x0001_0000]);
        u16s(&mut font, &[tables.len() as u16, 64, 2, 48]);
        let mut offset = 12 + 16 * tables.len();
        for (tag, data) in &tables {
            let padded = {
                let mut padded = data.clone();
                padded.resize(data.len().next_multiple_of(4), 0);
                padded
            };
            let checksum = padded
                .chunks(4)
                .map(|word| u32::from_be_bytes([word[0], word[1], word[2], word[3]]))
                .fold(0u32, u32::wrapping_add);
            font.extend_from_slice(*tag);
            u32s(&mut font, &[checksum, offset as u32, data.len() as u32]);
            offset += padded.len();
        }
        for (_, data) in &tables {
            font.extend_from_slice(data);
            font.resize(font.len().next_multiple_of(4), 0);
        }
        font
    }

    /// The CJK fixture: the kana and kanji of `MIXED` plus a few for the
    /// wrapping tests, full-width.
    fn cjk_fixture() -> Vec<u8> {
        let chars: Vec<char> = "ループザルーム初音ミク、。漢字日本語のテキスト"
            .chars()
            .collect();
        box_font(&chars, 1000)
    }

    /// The symbol fixture: just the emoji of `MIXED`, a little wider.
    fn symbol_fixture() -> Vec<u8> {
        box_font(&['🎵'], 1200)
    }

    fn fonts() -> (fontdue::Font, fontdue::Font, fontdue::Font) {
        (
            load_font(DEFAULT_FONT.to_vec(), 0).unwrap(),
            load_font(cjk_fixture(), 0).unwrap(),
            load_font(symbol_fixture(), 0).unwrap(),
        )
    }

    #[test]
    fn fixture_fonts_cover_their_characters() {
        // The .nyk suite loads the same box fonts from disk.
        for (path, covered) in [
            ("tests/fixtures/fallback-cjk.ttf", "ループザルーム初音ミク"),
            ("tests/fixtures/fallback-symbols.ttf", "🎵"),
        ] {
            let font = load_font(std::fs::read(path).unwrap(), 0).unwrap();
            for ch in covered.chars() {
                assert_ne!(font.lookup_glyph_index(ch), 0, "{} lacks {:?}", path, ch);
            }
            assert_eq!(
                font.lookup_glyph_index('a'),
                0,
                "{} should lack Latin",
                path
            );
        }
    }

    #[test]
    fn fallback_fonts_cover_missing_glyphs() {
        let (noto, cjk, symbols) = fonts();
        // Alone, the bundled font has no kana, kanji or emoji.
        let alone = clusters(&[&noto], MIXED, 16.0);
        assert!(alone.iter().any(|c| c.index == 0 && c.ch == 'ル'));
        assert!(alone.iter().any(|c| c.index == 0 && c.ch == '🎵'));

        // With fallbacks, every character finds a real glyph, each in the
        // first font of the chain that has it.
        let chain = [&noto, &cjk, &symbols];
        for cluster in clusters(&chain, MIXED, 16.0) {
            assert_ne!(cluster.index, 0, "{:?} fell through to tofu", cluster.ch);
            let expected = match cluster.ch {
                '🎵' => 2,
                ch if breaks_anywhere(ch) => 1,
                _ => 0,
            };
            assert_eq!(
                cluster.font, expected,
                "{:?} came from the wrong font",
                cluster.ch
            );
        }
        // Whichever font comes first, a character it lacks comes from the
        // next one that has it.
        let latin = clusters(&[&cjk, &noto], "a", 16.0);
        assert_eq!(latin[0].font, 1);
    }

    #[test]
    fn ellipsis_truncates_to_the_width() {
        let (noto, cjk, symbols) = fonts();
        let chain = [&noto, &cjk, &symbols];
        let full = layout_text(&chain, MIXED, &TextOptions::sized(14.0));
        let options = TextOptions {
            max_width: Some(160.0),
            overflow: Overflow::Ellipsis,
            ..TextOptions::sized(14.0)
        };
        let short = layout_text(&chain, MIXED, &options);
        assert!(
            full.width > 160.0,
            "the fixture should overflow: {}",
            full.width
        );
        assert!(
            short.width <= 160.0 && short.width > 120.0,
            "{}",
            short.width
        );
        assert_eq!(short.lines, 1);
        let last = short.glyphs.last().unwrap();
        assert_eq!((last.font, last.index), (0, noto.lookup_glyph_index('…')));
        assert!(short.glyphs.len() < full.glyphs.len());

        // Text that fits is left alone.
        let fits = layout_text(&chain, "ループ", &options);
        assert_eq!(fits.glyphs.len(), 3);
        assert!(fits.glyphs.iter().all(|g| g.font == 1));
    }

    #[test]
    fn ellipsis_falls_back_to_full_stops() {
        let cjk = load_font(cjk_fixture(), 0).unwrap();
        let noto = load_font(DEFAULT_FONT.to_vec(), 0).unwrap();
        let options = TextOptions {
            max_width: Some(40.0),
            overflow: Overflow::Ellipsis,
            ..TextOptions::sized(10.0)
        };
        // The fixture has no "…" and no "."; with the bundled font after it
        // the "…" comes from there.
        let layout = layout_text(&[&cjk, &noto], "ループザルーム初音ミク", &options);
        assert_eq!(layout.glyphs.last().unwrap().font, 1);
        let ellipsis = ellipsis_clusters(&[&cjk], 10.0);
        assert_eq!(ellipsis.len(), 3, "no font has \"…\", so three dots");
    }

    #[test]
    fn measuring_agrees_with_drawing() {
        let (noto, cjk, symbols) = fonts();
        let chain = vec![
            (DEFAULT_FONT_ID, Rc::new(noto)),
            (101, Rc::new(cjk)),
            (102, Rc::new(symbols)),
        ];
        let fonts: Vec<&fontdue::Font> = chain.iter().map(|(_, f)| f.as_ref()).collect();
        let options = TextOptions {
            max_width: Some(200.0),
            overflow: Overflow::Ellipsis,
            ..TextOptions::sized(16.0)
        };
        for scale in [1.0_f32, 2.0] {
            let layout = layout_text(&fonts, MIXED, &options);
            let mut pixmap = Pixmap::new((260.0 * scale) as u32, (40.0 * scale) as u32).unwrap();
            pixmap.fill(Color::WHITE);
            let mut cache = GlyphCache::default();
            let color = (0, 0, 0, 255);
            draw_layout(
                &mut pixmap,
                &mut cache,
                &chain,
                &layout,
                (10.0, 5.0),
                scale,
                color,
                None,
            );
            let width = pixmap.width() as usize;
            let inked: Vec<usize> = pixmap
                .pixels()
                .iter()
                .enumerate()
                .filter(|(_, p)| p.red() < 200)
                .map(|(i, _)| i % width)
                .collect();
            let first = *inked.iter().min().unwrap() as f32 / scale;
            let last = (*inked.iter().max().unwrap() + 1) as f32 / scale;
            // Ink sits inside the measured box, give or take side bearings
            // and rounding to whole pixels.
            assert!((first - 10.0).abs() <= 2.0, "ink starts at {}", first);
            assert!(
                (last - (10.0 + layout.width)).abs() <= 2.0,
                "ink ends at {} but the text measures {}",
                last,
                layout.width
            );
            assert!(!cache.entries.is_empty(), "glyphs should be cached");
        }
    }

    #[test]
    fn layout_does_not_depend_on_scale() {
        // Drawing scales the layout rather than redoing it, so a HiDPI
        // window measures exactly what measureText reports.
        let (noto, cjk, _) = fonts();
        let options = TextOptions {
            max_width: Some(90.0),
            ..TextOptions::sized(13.0)
        };
        let a = layout_text(&[&noto, &cjk], "wrap these words 初音ミク", &options);
        let b = layout_text(&[&noto, &cjk], "wrap these words 初音ミク", &options);
        assert_eq!(a.glyphs, b.glyphs);
        assert_eq!((a.width, a.height, a.lines), (b.width, b.height, b.lines));
    }

    #[test]
    fn wrapping_breaks_at_spaces_and_between_cjk_characters() {
        let (noto, cjk, _) = fonts();
        let hello = layout_text(&[&noto], "hello", &TextOptions::sized(16.0));
        let world = layout_text(&[&noto], "world", &TextOptions::sized(16.0));
        let one_width = hello.width.max(world.width);
        let options = TextOptions {
            max_width: Some(one_width + 4.0),
            ..TextOptions::sized(16.0)
        };
        let wrapped = layout_text(&[&noto], "hello world", &options);
        assert_eq!(wrapped.lines, 2);
        assert!(wrapped.width <= one_width + 4.0);
        // The second line starts with "w" at the left edge.
        let second: Vec<&Placed> = wrapped
            .glyphs
            .iter()
            .filter(|g| g.baseline > wrapped.glyphs[0].baseline)
            .collect();
        assert_eq!(second[0].index, noto.lookup_glyph_index('w'));
        assert_eq!(second[0].x, 0.0);

        // Full-width characters are 16 px at 16 px, so 50 px holds three.
        let options = TextOptions {
            max_width: Some(50.0),
            ..TextOptions::sized(16.0)
        };
        let cjk_wrapped = layout_text(&[&cjk, &noto], "漢字日本語のテキスト", &options);
        assert_eq!(cjk_wrapped.lines, 4);
        assert!(cjk_wrapped.width <= 50.0);

        // A word wider than the line is broken where it overflows.
        let long = layout_text(&[&noto], "abcdefghijklmnopqrstuvwxyz", &options);
        assert!(long.lines > 1 && long.width <= 50.0);
    }

    #[test]
    fn wrapping_keeps_closing_marks_off_the_line_start() {
        let (noto, cjk, _) = fonts();
        let chain = [&cjk, &noto];
        let run = clusters(&chain, "テルーム", 16.0);
        // Two characters fit on a line; "ー" may not start one, so "ル"
        // moves down with it.
        let lines = wrap_lines(&run, 40.0, 0.0);
        assert_eq!(lines, vec![(0, 1), (1, 3), (3, 4)]);
        assert!(!can_break(&run[1], &run[2]));
        assert!(can_break(&run[2], &run[3]));
    }

    #[test]
    fn alignment_moves_lines_inside_the_box() {
        let noto = load_font(DEFAULT_FONT.to_vec(), 0).unwrap();
        let chain = [&noto];
        let natural = layout_text(&chain, "Hi", &TextOptions::sized(16.0));
        let center = TextOptions {
            align: Align::Center,
            ..TextOptions::sized(16.0)
        };
        let right = TextOptions {
            align: Align::Right,
            ..TextOptions::sized(16.0)
        };
        // Without a width, x is the left edge, center or right edge.
        assert_eq!(
            layout_text(&chain, "Hi", &center).glyphs[0].x,
            -natural.width / 2.0
        );
        assert_eq!(
            layout_text(&chain, "Hi", &right).glyphs[0].x,
            -natural.width
        );
        // With one, lines are placed inside [0, maxWidth].
        let boxed = TextOptions {
            max_width: Some(100.0),
            ..right
        };
        let layout = layout_text(&chain, "Hi", &boxed);
        assert!((layout.glyphs[0].x - (100.0 - natural.width)).abs() < 1e-4);
        assert_eq!(layout.width, natural.width);
    }

    #[test]
    fn clipping_limits_the_width() {
        let noto = load_font(DEFAULT_FONT.to_vec(), 0).unwrap();
        let options = TextOptions {
            max_width: Some(30.0),
            overflow: Overflow::Clip,
            align: Align::Center,
            ..TextOptions::sized(16.0)
        };
        let layout = layout_text(&[&noto], "a long line of text", &options);
        assert_eq!(layout.width, 30.0);
        assert_eq!(layout.clip, Some((0.0, 30.0)));
        // An overflowing clipped line starts at the left edge.
        assert_eq!(layout.glyphs[0].x, 0.0);
        assert_eq!(layout.lines, 1);
    }

    #[test]
    fn spacing_options_change_the_metrics() {
        let noto = load_font(DEFAULT_FONT.to_vec(), 0).unwrap();
        let chain = [&noto];
        let plain = layout_text(&chain, "abcd", &TextOptions::sized(20.0));
        let spaced = TextOptions {
            letter_spacing: 2.0,
            ..TextOptions::sized(20.0)
        };
        let wide = layout_text(&chain, "abcd", &spaced);
        assert!(
            (wide.width - plain.width - 6.0).abs() < 1e-3,
            "three gaps of 2 px"
        );

        let tall = TextOptions {
            line_height: Some(1.5),
            ..TextOptions::sized(20.0)
        };
        let lines = layout_text(&chain, "a\nb\nc", &tall);
        assert_eq!((lines.lines, lines.height), (3, 90.0));

        let natural = layout_text(&chain, "a\nb", &TextOptions::sized(20.0));
        assert!(natural.height > 40.0, "{}", natural.height);
        assert_eq!(
            layout_text(&chain, "", &TextOptions::sized(20.0)).height,
            0.0
        );
    }

    #[test]
    fn spaces_count_but_invisible_characters_do_not() {
        let noto = load_font(DEFAULT_FONT.to_vec(), 0).unwrap();
        let chain = [&noto];
        let options = TextOptions::sized(16.0);
        let a = layout_text(&chain, "a", &options).width;
        // A caret after a trailing space needs the space measured.
        assert!(layout_text(&chain, "a ", &options).width > a);
        let ab = layout_text(&chain, "ab", &options);
        let joined = layout_text(&chain, "a\u{200D}\u{FE0F}b\r", &options);
        assert_eq!(ab.width, joined.width);
        assert_eq!(ab.glyphs.len(), joined.glyphs.len());
        let tab = layout_text(&chain, "\t", &options).width;
        let spaces = layout_text(&chain, "    ", &options).width;
        assert!((tab - spaces).abs() < 1e-3);
    }

    #[test]
    fn glyph_cache_drops_the_least_recently_used() {
        let mut cache = GlyphCache::default();
        let bitmap = |bytes: usize| (fontdue::Metrics::default(), vec![0u8; bytes]);
        let key = |font: u64, index: u16| GlyphKey {
            font,
            index,
            px: 16f32.to_bits(),
        };
        // Each entry costs 936 + 64 = 1000 bytes against a 4000 byte limit.
        for index in 0..4 {
            cache.get(key(1, index), 4000, || bitmap(936));
        }
        assert_eq!(cache.bytes, 4000);
        // Touch glyph 0 so glyph 1 is the oldest, then overflow.
        let mut rasterized = false;
        cache.get(key(1, 0), 4000, || {
            rasterized = true;
            bitmap(936)
        });
        assert!(!rasterized, "a cached glyph is not rasterized again");
        cache.get(key(2, 0), 4000, || bitmap(936));
        assert!(
            cache.bytes <= 3000,
            "trimmed to three quarters: {}",
            cache.bytes
        );
        assert!(cache.entries.contains_key(&key(1, 0)));
        assert!(cache.entries.contains_key(&key(2, 0)));
        assert!(!cache.entries.contains_key(&key(1, 1)));

        cache.forget_font(1);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.bytes, 1000);
    }

    #[test]
    fn fonts_are_size_checked() {
        let err = load_font(vec![0; FONT_FILE_LIMIT + 1], 0).unwrap_err();
        assert!(err.contains("larger than"), "{}", err);
        let err = load_font(vec![1, 2, 3], 0).unwrap_err();
        assert!(err.contains("cannot load font"), "{}", err);
    }

    #[test]
    fn os_errors_lose_their_source_location() {
        assert_eq!(
            tidy_os_error("os error at /x/mod.rs:765: DISPLAY is not set."),
            "DISPLAY is not set."
        );
        assert_eq!(tidy_os_error("plain message"), "plain message");
    }

    #[test]
    fn rounded_rect_paths_are_built() {
        assert!(rounded_rect(0.0, 0.0, 10.0, 10.0, 0.0).is_some());
        assert!(rounded_rect(0.0, 0.0, 10.0, 10.0, 50.0).is_some());
        assert!(rounded_rect(0.0, 0.0, -1.0, 10.0, 0.0).is_none());
    }

    #[test]
    fn image_limits_are_read_and_checked() {
        let limits = limits_arg(&[Value::Nil], 1).unwrap();
        assert_eq!(limits.max_pixels, images::DEFAULT_MAX_PIXELS);
        assert_eq!(limits.max_bytes, images::DEFAULT_MAX_BYTES);
        let limits = limits_arg(&[Value::Nil, Value::Int(10), Value::Int(20)], 1).unwrap();
        assert_eq!((limits.max_pixels, limits.max_bytes), (10, 20));
        let err = limits_arg(&[Value::Nil, Value::Int(0)], 1).unwrap_err();
        assert!(err.contains("maxPixels must be positive"), "{}", err);
    }

    #[test]
    fn scaled_frames_are_cached_per_size() {
        let mut pixmap = Pixmap::new(8, 8).unwrap();
        pixmap.fill(Color::BLACK);
        let decoded = Decoded {
            frames: vec![pixmap.clone(), pixmap],
            delays: vec![0.1, 0.1],
            loop_count: 0,
        };
        let mut entry = ImageEntry::new(decoded, Limits::default());
        assert!(entry.frames[0].opaque);
        let key = ScaleKey {
            src: (0.0, 0.0, 8.0, 8.0),
            width: 4,
            height: 4,
        };
        let first = entry.prepared(0, key).unwrap().pixmap.data().as_ptr();
        let again = entry.prepared(0, key).unwrap().pixmap.data().as_ptr();
        assert_eq!(first, again, "the same size should reuse the cached copy");
        entry.prepared(1, key).unwrap();
        assert_eq!(entry.scaled.len(), 2, "each frame keeps its own copy");
        assert_eq!(entry.scaled_bytes, 2 * 4 * 4 * 4);
        let bigger = ScaleKey { width: 6, ..key };
        assert_eq!(entry.prepared(0, bigger).unwrap().pixmap.width(), 6);
        assert_eq!(entry.scaled_bytes, 4 * 4 * 4 + 6 * 4 * 4);
    }

    #[test]
    fn damage_rects_are_clamped_to_the_surface() {
        let rects = [
            (-5, -5, 10, 10),
            (95, 95, 20, 20),
            (200, 0, 5, 5),
            (3, 3, 0, 4),
        ];
        assert_eq!(
            clamp_rects(&rects, 100, 100),
            Some(vec![(0, 0, 5, 5), (95, 95, 5, 5)])
        );
        assert_eq!(clamp_rects(&[(200, 0, 5, 5)], 100, 100), None);
    }

    #[test]
    fn media_commands_become_keys_and_session_actions() {
        let key = |event| media_command(&event);
        assert_eq!(
            key(MediaControlEvent::Toggle),
            Some(MediaCommand::Key("playpause"))
        );
        assert_eq!(
            key(MediaControlEvent::Play),
            Some(MediaCommand::Key("play"))
        );
        assert_eq!(
            key(MediaControlEvent::Pause),
            Some(MediaCommand::Key("pause"))
        );
        assert_eq!(
            key(MediaControlEvent::Next),
            Some(MediaCommand::Key("next"))
        );
        assert_eq!(
            key(MediaControlEvent::Previous),
            Some(MediaCommand::Key("previous"))
        );
        assert_eq!(
            key(MediaControlEvent::Stop),
            Some(MediaCommand::Key("stop"))
        );
        assert_eq!(
            key(MediaControlEvent::SetPosition(MediaPosition(
                Duration::from_millis(61_500)
            ))),
            Some(MediaCommand::SeekTo(61.5))
        );
        assert_eq!(
            key(MediaControlEvent::SeekBy(
                SeekDirection::Backward,
                Duration::from_secs(5)
            )),
            Some(MediaCommand::SeekBy(-5.0))
        );
        assert_eq!(
            key(MediaControlEvent::Seek(SeekDirection::Forward)),
            Some(MediaCommand::SeekBy(MEDIA_SEEK_STEP))
        );
        assert_eq!(
            key(MediaControlEvent::Raise),
            Some(MediaCommand::Action("raise"))
        );
        assert_eq!(key(MediaControlEvent::SetVolume(0.5)), None);
        assert_eq!(key(MediaControlEvent::OpenUri("x".to_string())), None);
    }

    #[test]
    fn keyboard_media_keys_are_named() {
        assert_eq!(
            media_key_name(&Key::Named(NamedKey::MediaPlayPause)),
            Some("playpause")
        );
        assert_eq!(
            media_key_name(&Key::Named(NamedKey::MediaTrackNext)),
            Some("next")
        );
        assert_eq!(
            media_key_name(&Key::Named(NamedKey::MediaTrackPrevious)),
            Some("previous")
        );
        assert_eq!(media_key_name(&Key::Named(NamedKey::Escape)), None);
        assert_eq!(media_key_name(&Key::Character("p".into())), None);
    }

    #[test]
    fn media_events_carry_their_fields() {
        let event = media_command_event(3, MediaCommand::SeekBy(-10.0));
        let Value::Table(table) = event else {
            panic!("events are tables");
        };
        let table = table.borrow();
        let field = |name: &str| table.fields.get(name).cloned();
        assert!(matches!(field("kind"), Some(Value::String(s)) if s.to_string() == "mediasession"));
        assert!(matches!(field("action"), Some(Value::String(s)) if s.to_string() == "seek"));
        assert!(matches!(field("offset"), Some(Value::Float(f)) if f == -10.0));
        assert!(field("position").is_none());
    }

    #[test]
    fn dbus_names_are_valid_bus_name_elements() {
        assert_eq!(dbus_name("Neyuki Player", 42), "neyuki_player.instance42");
        assert_eq!(dbus_name("", 1), "neyuki.instance1");
        assert_eq!(dbus_name("  ♫♫ ", 1), "neyuki.instance1");
        assert_eq!(dbus_name("3D Tunes", 7), "_3d_tunes.instance7");
        assert!(dbus_name(&"x".repeat(500), 1).len() < 100);
    }

    #[test]
    fn artwork_urls_are_told_from_paths() {
        assert!(is_artwork_url("https://example.com/cover.png"));
        assert!(is_artwork_url("HTTP://example.com/cover.png"));
        assert!(is_artwork_url("file:///tmp/cover.png"));
        assert!(!is_artwork_url("covers/cover.png"));
        assert!(!is_artwork_url("C:\\music\\cover.png"));
    }

    #[test]
    fn file_urls_match_each_backend() {
        assert_eq!(
            file_url("C:\\Music\\My Cover.png", true),
            "file://C:\\Music\\My Cover.png"
        );
        assert_eq!(
            file_url("/home/me/My Cover#1.png", false),
            "file:///home/me/My%20Cover%231.png"
        );
        assert_eq!(file_url("/tmp/é.png", false), "file:///tmp/%C3%A9.png");
    }

    #[test]
    fn points_need_pairs() {
        let table = new_table(vec![
            Value::Float(1.0),
            Value::Float(2.0),
            Value::Float(3.0),
        ]);
        let err = points_arg(&[table], 0, "points").unwrap_err();
        assert!(err.contains("pairs"), "{}", err);
    }

    fn red_at(pixmap: &Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
        let p = pixmap.pixels()[(y * pixmap.width() + x) as usize];
        (p.red(), p.green(), p.blue(), p.alpha())
    }

    #[test]
    fn paths_are_built_from_commands() {
        let square = [0.0, 0.0, 0.0, 1.0, 10.0, 0.0, 1.0, 10.0, 10.0, 4.0];
        assert!(path_from_commands(&square).is_ok());
        assert!(path_from_commands(&[]).is_err());
        assert!(path_from_commands(&[9.0]).unwrap_err().contains("unknown"));
        assert!(
            path_from_commands(&[1.0, 5.0])
                .unwrap_err()
                .contains("middle")
        );
    }

    #[test]
    fn gradients_need_stops_and_extent() {
        let linear = [0.0, 0.0, 0.0, 10.0, 0.0, 0.0, 4278190335.0, 1.0, 65535.0];
        assert!(gradient_shader(&linear).is_ok());
        assert!(gradient_shader(&linear[..7]).is_err());
        let radial = [1.0, 5.0, 5.0, 0.0, 0.0, 0.0, 4278190335.0, 1.0, 65535.0];
        assert!(gradient_shader(&radial).unwrap_err().contains("degenerate"));
    }

    #[test]
    fn clips_and_opacity_groups_shape_drawing() {
        let mut pixmap = Pixmap::new(10, 10).unwrap();
        pixmap.fill(Color::WHITE);
        let mut state = DrawState::default();
        let mut target = Target {
            pixmap: &mut pixmap,
            scale: 1.0,
            state: &mut state,
        };
        let whole = rounded_rect(0.0, 0.0, 10.0, 10.0, 0.0).unwrap();
        let left = rounded_rect(0.0, 0.0, 5.0, 10.0, 0.0);
        push_clip(&mut target, left.as_ref()).unwrap();
        fill_shape(&mut target, &whole, (255, 0, 0, 255), None, None).unwrap();
        pop_clip(&mut target).unwrap();
        assert!(pop_clip(&mut target).is_err());
        // An empty clip draws nothing.
        push_clip(&mut target, None).unwrap();
        fill_shape(&mut target, &whole, (0, 0, 255, 255), None, None).unwrap();
        pop_clip(&mut target).unwrap();

        push_opacity(&mut target, 0.5).unwrap();
        push_clip(&mut target, left.as_ref()).unwrap();
        assert!(pop_opacity(&mut target).is_err());
        pop_clip(&mut target).unwrap();
        fill_shape(&mut target, &whole, (0, 0, 0, 255), None, None).unwrap();
        pop_opacity(&mut target).unwrap();
        assert!(pop_opacity(&mut target).is_err());

        let (r, _, _, _) = red_at(&pixmap, 2, 5);
        assert!(r > 100 && r < 160, "red under half black: {}", r);
        let (r, g, _, a) = red_at(&pixmap, 7, 5);
        assert!(
            r > 100 && r < 160 && g == r && a == 255,
            "{} {} {}",
            r,
            g,
            a
        );
    }

    #[test]
    fn gradients_and_shadows_paint() {
        let mut pixmap = Pixmap::new(40, 40).unwrap();
        pixmap.fill(Color::WHITE);
        let mut state = DrawState::default();
        let mut target = Target {
            pixmap: &mut pixmap,
            scale: 1.0,
            state: &mut state,
        };
        let rect = rounded_rect(10.0, 10.0, 20.0, 20.0, 0.0).unwrap();
        // Black to white across the rectangle, with a black shadow.
        let gradient = [0.0, 10.0, 0.0, 30.0, 0.0, 0.0, 255.0, 1.0, 4294967295.0];
        let shadow = [8.0, 0.0, 0.0, 0.0, 255.0];
        fill_shape(
            &mut target,
            &rect,
            (0, 0, 0, 255),
            Some(&gradient),
            Some(&shadow),
        )
        .unwrap();
        let (left, ..) = red_at(&pixmap, 11, 20);
        let (right, ..) = red_at(&pixmap, 28, 20);
        assert!(left < 60 && right > 190, "{} {}", left, right);
        let (halo, ..) = red_at(&pixmap, 8, 20);
        assert!(halo < 250, "shadow reaches outside the shape: {}", halo);
        let (far, ..) = red_at(&pixmap, 0, 0);
        assert_eq!(far, 255);
    }

    #[test]
    fn opacity_layers_are_reused_clean() {
        let mut pixmap = Pixmap::new(40, 20).unwrap();
        pixmap.fill(Color::WHITE);
        let mut state = DrawState::default();
        let mut target = Target {
            pixmap: &mut pixmap,
            scale: 1.0,
            state: &mut state,
        };
        let black = (0, 0, 0, 255);
        // Every kind of draw inside a group shows up once it closes.
        push_opacity(&mut target, 1.0).unwrap();
        let square = rounded_rect(0.0, 0.0, 5.0, 5.0, 0.0).unwrap();
        fill_flat(&mut target, 0.0, 0.0, 5.0, 5.0, 0.0, black, None, &square).unwrap();
        let card = rounded_rect(6.5, 0.5, 5.0, 5.0, 2.0).unwrap();
        fill_flat(&mut target, 6.5, 0.5, 5.0, 5.0, 2.0, black, None, &card).unwrap();
        let circle = PathBuilder::from_circle(16.0, 3.0, 2.5).unwrap();
        fill_shape(&mut target, &circle, black, None, None).unwrap();
        let outline = rounded_rect(20.5, 0.5, 4.0, 4.0, 0.0).unwrap();
        target.stroke_path(&outline, &paint(black), &stroke(1.0));
        pop_opacity(&mut target).unwrap();
        for (x, y) in [(2, 2), (9, 3), (16, 3), (20, 2)] {
            assert_eq!(red_at(&pixmap, x, y).0, 0, "at {} {}", x, y);
        }

        // The next group gets the same layer back, with nothing left on it.
        let mut target = Target {
            pixmap: &mut pixmap,
            scale: 1.0,
            state: &mut state,
        };
        push_opacity(&mut target, 1.0).unwrap();
        assert!(target.state.spare.is_empty());
        assert!(target.pixmap.pixels().iter().all(|p| p.alpha() == 0));
        pop_opacity(&mut target).unwrap();
        assert_eq!(state.spare.len(), 1);
    }

    #[test]
    fn rectangle_clips_need_no_mask_and_clip_rasterized_shapes() {
        let mut pixmap = Pixmap::new(20, 20).unwrap();
        pixmap.fill(Color::WHITE);
        let mut state = DrawState::default();
        let mut target = Target {
            pixmap: &mut pixmap,
            scale: 1.0,
            state: &mut state,
        };
        push_clip_rect(&mut target, 5.0, 5.0, 10.0, 10.0).unwrap();
        assert!(target.state.clip().unwrap().mask.is_none());
        // A circle crossing the clip edge is cut along it.
        let circle = PathBuilder::from_circle(5.0, 10.0, 4.0).unwrap();
        fill_shape(&mut target, &circle, (0, 0, 0, 255), None, None).unwrap();
        pop_clip(&mut target).unwrap();
        assert_eq!(red_at(&pixmap, 4, 10).0, 255);
        assert_eq!(red_at(&pixmap, 6, 10).0, 0);
    }

    #[test]
    fn shadows_are_cached_by_shape_but_drawn_in_their_color() {
        let draw = |color: f64| {
            let mut pixmap = Pixmap::new(40, 40).unwrap();
            pixmap.fill(Color::WHITE);
            let mut state = DrawState::default();
            let mut target = Target {
                pixmap: &mut pixmap,
                scale: 1.0,
                state: &mut state,
            };
            let rect = rounded_rect(10.0, 10.0, 20.0, 20.0, 4.0).unwrap();
            draw_shadow(&mut target, &rect, &[8.0, 0.0, 2.0, 0.0, color]).unwrap();
            pixmap
        };
        let black = draw(255.0);
        let again = draw(255.0);
        assert_eq!(black.data(), again.data());
        assert!(red_at(&black, 20, 31).0 < 200);
        let red = draw(4278190335.0);
        let (r, g, ..) = red_at(&red, 20, 31);
        assert!(r == 255 && g < 200, "{} {}", r, g);
    }

    #[test]
    fn layouts_are_cached_and_agree_with_fresh_ones() {
        let chain = font_chain(DEFAULT_FONT_ID, Some(&[])).unwrap();
        let options = TextOptions::sized(14.0);
        let fonts: Vec<&fontdue::Font> = chain.iter().map(|(_, f)| f.as_ref()).collect();
        let fresh = layout_text(&fonts, "Cached text", &options);
        let first = cached_layout("Cached text".to_string(), &chain, &options);
        let second = cached_layout("Cached text".to_string(), &chain, &options);
        assert!(Rc::ptr_eq(&first, &second));
        assert_eq!(first.glyphs, fresh.glyphs);
        let wider = TextOptions {
            letter_spacing: 2.0,
            ..options
        };
        let spaced = cached_layout("Cached text".to_string(), &chain, &wider);
        assert!(spaced.width > first.width);
    }
}
