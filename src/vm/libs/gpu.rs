//! The GPU surface backing `@neyuki/gpu`.
//!
//! `@neyuki/ui` renders on the CPU with `tiny-skia` and that does not change.
//! This module is a second, separate way to fill a window: a `wgpu` surface
//! on a window opened with `gpu = true`, which `@neyuki/ui` then leaves
//! alone. The two cannot share a window — softbuffer and wgpu would both
//! present to it — so a GPU window has no back buffer and the `ui` drawing
//! calls refuse to touch it.
//!
//! The adapter, device and queue are shared by every GPU window, as surfaces
//! from one instance must be. They live in thread locals, like the windows
//! in `ui.rs`, because the whole UI runs on the event loop thread.
//!
//! The module is behind the `gpu` cargo feature, which is **off** by default
//! (unlike `webview`). Without it `gpu.isAvailable()` returns `false` and
//! every other native fails with a clear error, so a default build links no
//! graphics driver loader at all.
//!
//! wgpu's own environment variables work (`WGPU_BACKEND`, `WGPU_POWER_PREF`,
//! the validation flags), and `NEYUKI_GPU_SOFTWARE=1` adds the one it has
//! none for: take the software adapter (WARP on Windows, lavapipe
//! elsewhere), which is how a machine with no GPU runs the same checks.
//!
//! ## Conventions
//!
//! Fixed now, because scripts will hard-code matrices against them. These
//! are wgpu's own (Direct3D / Metal) conventions, so nothing has to be
//! patched up on the way to the GPU:
//!
//! - **Clip space** (what a vertex shader returns): `x` and `y` in `-1..1`
//!   with **y up**, `z` in **`0..1`** — 0 at the near plane, 1 at the far
//!   plane. Not OpenGL's `-1..1` depth range.
//! - **Framebuffer and texture coordinates**: origin at the **top left**,
//!   y down, so `(0, 0)` is the top-left pixel as in `@neyuki/ui`.
//! - **World and view space**: **right-handed**, y up, looking down `-z`.
//!   The projection matrix does the handedness flip into clip space.
//! - **Matrices**: **column-major**, column-vector convention
//!   (`clip = projection * view * model * position`). A `mat4x4<f32>`
//!   uniform is 16 floats, column after column, which is what WGSL reads
//!   without a transpose.

#[cfg(feature = "gpu")]
pub(crate) use real::{Surface, create_surface, discard_window_frame, resize};
#[cfg(feature = "gpu")]
pub(crate) const NATIVES: &[crate::vm::value::NativeDef] = real::NATIVES;

#[cfg(not(feature = "gpu"))]
pub(crate) use stub::{Surface, create_surface, discard_window_frame, resize};
#[cfg(not(feature = "gpu"))]
pub(crate) const NATIVES: &[crate::vm::value::NativeDef] = stub::NATIVES;

/// The error `ui.open` reports for `gpu = true` on a build without the
/// feature, and every native but `__gpu_available` reports always.
#[cfg(not(feature = "gpu"))]
const UNSUPPORTED: &str =
    "this build of Neyuki has no GPU support (built without the `gpu` feature)";

#[cfg(not(feature = "gpu"))]
mod stub {
    use std::sync::Arc;

    use winit::window::Window;

    use crate::vm::machine::VM;
    use crate::vm::value::{NativeDef, Value};

    /// No surface can exist, so this type has no values: the
    /// `Option<Surface>` in `ui::Win` is always `None` and takes no space.
    pub(crate) enum Surface {}

    pub(crate) fn create_surface(_: &Arc<Window>) -> Result<Surface, String> {
        Err(super::UNSUPPORTED.to_string())
    }

    pub(crate) fn resize(surface: &mut Surface, _: u32, _: u32) {
        match *surface {}
    }

    /// No frame can be open, so a window being closed has none to discard.
    pub(crate) fn discard_window_frame(_: u64) {}

    fn available(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
        Ok(vec![Value::Bool(false)])
    }

    fn unsupported(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
        Err(super::UNSUPPORTED.to_string())
    }

    pub(crate) const NATIVES: &[NativeDef] = &[
        NativeDef {
            name: "__gpu_available",
            func: available,
        },
        NativeDef {
            name: "__gpu_adapter",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_clear",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_device",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_shader",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_buffer",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_buffer_write",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_texture",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_texture_update",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_sampler",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_pipeline",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_free",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_frame",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_frame_pipeline",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_frame_bind",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_frame_vertices",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_frame_indices",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_frame_draw",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_frame_finish",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_frame_read",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_frame_cancel",
            func: unsupported,
        },
        NativeDef {
            name: "__gpu_cancel_frame",
            func: unsupported,
        },
    ];
}

#[cfg(feature = "gpu")]
mod real {
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, Mutex};

    use winit::window::Window;

    use crate::vm::libs::args::{bool_arg, number_arg, str_arg};
    use crate::vm::libs::ui;
    use crate::vm::machine::VM;
    use crate::vm::value::{NativeDef, Value};

    /// The adapter, device and queue every GPU window shares. Created from
    /// the first `gpu = true` window's surface, so the adapter chosen can
    /// actually present to a window, and then kept for the process.
    struct Context {
        adapter: wgpu::Adapter,
        device: wgpu::Device,
        queue: wgpu::Queue,
    }

    thread_local! {
        /// Surfaces are only valid on the instance that created them, so
        /// there is exactly one, made on first use.
        static INSTANCE: RefCell<Option<wgpu::Instance>> = const { RefCell::new(None) };
        /// `Ok` once a device was created, `Err` once that failed. The error
        /// is cached so a script that retries every frame does not probe the
        /// drivers again each time.
        static CONTEXT: RefCell<Option<Result<Context, String>>> = const { RefCell::new(None) };
    }

    /// A window's swap chain: the surface, the size it is configured for
    /// (so `resize` can skip reconfiguring when nothing moved), and the
    /// depth buffer every frame on this window attaches.
    pub(crate) struct Surface {
        surface: wgpu::Surface<'static>,
        config: wgpu::SurfaceConfiguration,
        /// Made on the first frame and dropped on resize, so a window that
        /// is only ever cleared never allocates one. Every frame attaches
        /// it, and a pipeline chooses whether to test or write it.
        depth: Option<wgpu::TextureView>,
    }

    /// The one depth format this module uses. `Depth32Float` is a core
    /// WebGPU format, so it is available on every adapter wgpu supports,
    /// and 32 bits of float depth is what a z range of `0..1` wants.
    const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

    /// `display: None` leaves the display handle to `create_surface`, which
    /// takes it from the window. That is the only arrangement that fits:
    /// `@neyuki/ui` owns the event loop and hands out windows, and a GPU
    /// window may be opened long after the loop started.
    ///
    /// Reading the environment keeps wgpu's own variables working, so
    /// `WGPU_BACKEND=vulkan` (and the debug and validation flags) can pin a
    /// backend without a rebuild. That is how a headless or CI machine
    /// chooses a software driver, and how a bug report can be narrowed to
    /// one backend.
    fn instance_descriptor() -> wgpu::InstanceDescriptor {
        wgpu::InstanceDescriptor::new_without_display_handle_from_env()
    }

    /// The adapter to prefer, overridable with `WGPU_POWER_PREF`. A 3D
    /// workload is what this module is for, so the default is the fastest
    /// adapter rather than the one that saves battery.
    fn power_preference() -> wgpu::PowerPreference {
        wgpu::PowerPreference::from_env().unwrap_or(wgpu::PowerPreference::HighPerformance)
    }

    /// `NEYUKI_GPU_SOFTWARE=1` asks for the software adapter instead of real
    /// hardware: WARP ("Microsoft Basic Render Driver") on Windows, lavapipe
    /// where it is installed. wgpu has no environment variable of its own
    /// for this, and it is what makes a continuous-integration machine with
    /// no GPU able to run the same checks. Anything but `0` turns it on, to
    /// match how wgpu reads its own variables.
    fn force_software_adapter() -> bool {
        std::env::var("NEYUKI_GPU_SOFTWARE").is_ok_and(|value| value != "0")
    }

    /// Runs `body` on the one instance, creating it on first use.
    fn with_instance<T>(body: impl FnOnce(&wgpu::Instance) -> T) -> T {
        INSTANCE.with(|cell| {
            let mut cell = cell.borrow_mut();
            let instance = cell.get_or_insert_with(|| wgpu::Instance::new(instance_descriptor()));
            body(instance)
        })
    }

    /// Picks the adapter and creates the device, blocking until the driver
    /// answers. `compatible` is the surface it has to be able to present to.
    ///
    /// Blocking is deliberate. Both requests are futures, but on every
    /// native backend they resolve on the calling thread without waiting on
    /// I/O, and this runs once, from `ui.open`, before any frame. Making it
    /// task-aware would mean handing the work to a `background` worker,
    /// while the device, queue and surface all belong to the event loop
    /// thread (Metal requires it), so the task would hand the result back
    /// and wait anyway. Nothing called per frame blocks, so `ui.run` pacing
    /// is untouched.
    fn create_context(compatible: &wgpu::Surface<'static>) -> Result<Context, String> {
        let adapter = with_instance(|instance| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: power_preference(),
                force_fallback_adapter: force_software_adapter(),
                compatible_surface: Some(compatible),
                ..Default::default()
            }))
        })
        .map_err(|err| format!("no graphics adapter is available: {err}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("neyuki"),
            ..Default::default()
        }))
        .map_err(|err| format!("cannot create a graphics device: {err}"))?;
        // wgpu's default handler for an error raised outside an error scope
        // logs it and panics, which is the one thing a script must never
        // get. Collecting them instead lets the next native call raise the
        // message as an ordinary Neyuki error.
        device.on_uncaptured_error(Arc::new(record_device_error));
        Ok(Context {
            adapter,
            device,
            queue,
        })
    }

    /// Errors the graphics driver reported outside an error scope: a bad
    /// draw call, a lost device, an allocation that failed. The handler has
    /// to be `Send + Sync + 'static`, so these cannot live in a thread
    /// local like everything else here.
    static DEVICE_ERRORS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    /// How many unreported driver errors to keep. One broken draw call in a
    /// loop would otherwise grow this without end, and only the first is
    /// worth reporting anyway.
    const MAX_DEVICE_ERRORS: usize = 4;

    fn record_device_error(err: wgpu::Error) {
        let mut errors = DEVICE_ERRORS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if errors.len() < MAX_DEVICE_ERRORS {
            errors.push(err.to_string());
        }
    }

    /// Takes the first driver error since the last check and drops the
    /// rest. A later error is usually a consequence of the first.
    fn take_device_error() -> Option<String> {
        let mut errors = DEVICE_ERRORS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let first = errors.first().cloned();
        errors.clear();
        first
    }

    /// Turns a driver error into a script error, prefixed with what was
    /// being attempted. Called after the operations that are not worth an
    /// error scope of their own -- a scope's `pop` has to block, which is
    /// acceptable when building resources but not once per frame.
    fn check_device_error(what: &str) -> Result<(), String> {
        match take_device_error() {
            Some(err) => Err(format!("{what}: {}", err.trim())),
            None => Ok(()),
        }
    }

    /// Runs `body` with wgpu's validation errors captured, so a shader that
    /// does not compile or a pipeline that does not fit its shader becomes
    /// a script error instead of reaching the uncaptured handler.
    ///
    /// `pop` is a future and this blocks on it. That is on purpose and the
    /// same reasoning as `create_context`: these calls build resources, and
    /// a script builds them before it draws, not inside a frame.
    fn capturing<T>(
        device: &wgpu::Device,
        what: &str,
        body: impl FnOnce() -> T,
    ) -> Result<T, String> {
        // Anything already pending would otherwise be blamed on `body`.
        let _ = take_device_error();
        let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let value = body();
        match pollster::block_on(scope.pop()) {
            // wgpu's message for a WGSL error carries the line and column
            // and a snippet of the source, so it is passed through whole.
            Some(err) => Err(format!("{what}: {}", err.to_string().trim())),
            None => Ok(value),
        }
    }

    /// The label every shader module here is given. wgpu repeats it in an
    /// error, where it says nothing to a script: there is one shader module
    /// per `device:shader` call and the script did not name it.
    const SHADER_LABEL: &str = "neyuki";

    /// Trims wgpu's framing off a shader error. A WGSL mistake arrives
    /// wrapped in two layers naming `Device::create_shader_module` and the
    /// label above, with the part a script can act on last: the message,
    /// the line and column, and a snippet of its own source. That part is
    /// kept whole. An error not in that shape is passed through untouched,
    /// because a wordy error beats a lost diagnostic.
    fn tidy_shader_error(err: String) -> String {
        let marker = format!(
            "
Shader '{SHADER_LABEL}' "
        );
        match err.find(&marker) {
            Some(offset) => format!(
                "cannot compile the shader: {}",
                err[offset + marker.len()..].trim_end()
            ),
            None => err,
        }
    }

    /// Runs `body` on the shared context. `probe` is a surface the adapter
    /// must be able to present to, needed only the first time.
    fn with_context<T>(
        probe: Option<&wgpu::Surface<'static>>,
        body: impl FnOnce(&Context) -> Result<T, String>,
    ) -> Result<T, String> {
        CONTEXT.with(|cell| {
            let mut cell = cell.borrow_mut();
            if cell.is_none() {
                let Some(probe) = probe else {
                    return Err("no GPU window is open".to_string());
                };
                *cell = Some(create_context(probe));
            }
            match cell.as_ref().expect("the context was just created") {
                Ok(context) => body(context),
                Err(err) => Err(err.clone()),
            }
        })
    }

    /// Creates the surface for a window opened with `gpu = true`.
    ///
    /// The window arrives as `Arc<Window>`, not by reference: wgpu's safe
    /// `create_surface` wants a `Send + Sync` handle it can own, and owning
    /// a clone of the `Arc` is what makes the surface sound with no unsafe
    /// lifetime argument. `ui::Win` keeps the same `Arc` and drops the
    /// surface before the window.
    pub(crate) fn create_surface(window: &Arc<Window>) -> Result<Surface, String> {
        let surface = with_instance(|instance| instance.create_surface(Arc::clone(window)))
            .map_err(|err| format!("cannot create a GPU surface for this window: {err}"))?;
        let size = window.inner_size();
        let (width, height) = (size.width.max(1), size.height.max(1));
        let config = with_context(Some(&surface), |context| {
            let mut config = surface
                .get_default_config(&context.adapter, width, height)
                .ok_or_else(|| "the graphics adapter cannot present to this window".to_string())?;
            let capabilities = surface.get_capabilities(&context.adapter);
            // `frame:read()` copies the image out of the surface, so the
            // surface has to allow that. Every desktop backend does; where
            // one does not, the capability is simply left out and only
            // `read` fails, with the driver saying why.
            config.usage |= capabilities.usages & wgpu::TextureUsages::COPY_SRC;
            // The adapter usually prefers an sRGB format, which would have
            // the driver encode everything written to it: a clear of
            // "#204060" would reach the screen as #638aa5 and an uploaded
            // `@neyuki/ui` image would not match what `ui` draws from it.
            // `@neyuki/ui` holds 8-bit non-linear channels throughout, so
            // the plain variant of the same format is the one that keeps
            // the two paths showing the same colours. A script that wants
            // linear shading can convert in its shader, which is where it
            // would have to anyway.
            let plain = config.format.remove_srgb_suffix();
            if capabilities.formats.contains(&plain) {
                config.format = plain;
            }
            Ok(config)
        })?;
        // Pipelines have to name the format they render to, and they are
        // built before any particular window is drawn, so the first
        // surface's format is the one they all use. Every GPU window on one
        // adapter is given the same preferred format.
        TARGET_FORMAT.with(|format| {
            if format.get().is_none() {
                format.set(Some(config.format));
            }
        });
        let mut surface = Surface {
            surface,
            config,
            depth: None,
        };
        configure(&mut surface)?;
        Ok(surface)
    }

    fn configure(target: &mut Surface) -> Result<(), String> {
        let Surface {
            surface, config, ..
        } = target;
        with_context(None, |context| {
            surface.configure(&context.device, config);
            Ok(())
        })
    }

    /// The window's depth buffer, made on first use and after a resize.
    /// Returned by clone: the handle is reference counted, so a frame can
    /// hold it without keeping the window borrowed.
    fn depth_view(target: &mut Surface) -> Result<wgpu::TextureView, String> {
        if let Some(view) = target.depth.as_ref() {
            return Ok(view.clone());
        }
        let (width, height) = (target.config.width, target.config.height);
        let view = with_context(None, |context| {
            let texture = context.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("neyuki depth"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: DEPTH_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            Ok(texture.create_view(&wgpu::TextureViewDescriptor::default()))
        })?;
        check_device_error("cannot create a depth buffer")?;
        Ok(target.depth.insert(view).clone())
    }

    /// Reconfigures the swap chain after the window changed size. A failure
    /// is dropped: a resize is not a script call, and the next `clear`
    /// reports whatever is still wrong.
    pub(crate) fn resize(surface: &mut Surface, width: u32, height: u32) {
        let (width, height) = (width.max(1), height.max(1));
        if surface.config.width == width && surface.config.height == height {
            return;
        }
        surface.config.width = width;
        surface.config.height = height;
        // The depth buffer has to match the surface, so it is dropped and
        // the next frame makes one at the new size.
        surface.depth = None;
        let _ = configure(surface);
    }

    /// Why a frame could not be acquired, for the states that are real
    /// failures rather than "try again next frame".
    fn acquire_error(acquired: &wgpu::CurrentSurfaceTexture) -> &'static str {
        use wgpu::CurrentSurfaceTexture as Acquired;
        match acquired {
            Acquired::Validation => "the graphics driver rejected the frame",
            Acquired::Outdated => "the surface is outdated",
            Acquired::Lost => "the surface was lost",
            Acquired::Timeout => "the graphics driver timed out",
            Acquired::Occluded => "the window is not visible",
            Acquired::Success(_) | Acquired::Suboptimal(_) => "no error",
        }
    }

    /// Fills the window with `color` (components in `0..1`) and presents it.
    ///
    /// This is the whole of the spike's drawing: one render pass whose only
    /// operation is the clear. Pipelines, meshes and depth come later.
    fn present(target: &mut Surface, color: [f64; 4]) -> Result<(), String> {
        use wgpu::CurrentSurfaceTexture as Acquired;

        let frame = match target.surface.get_current_texture() {
            Acquired::Success(frame) | Acquired::Suboptimal(frame) => frame,
            // The surface went stale (resized, the display changed, the
            // compositor took it away). Reconfiguring and asking once more
            // is the documented recovery; a second failure is reported.
            Acquired::Outdated | Acquired::Lost => {
                configure(target)?;
                match target.surface.get_current_texture() {
                    Acquired::Success(frame) | Acquired::Suboptimal(frame) => frame,
                    other => {
                        return Err(format!("cannot present window: {}", acquire_error(&other)));
                    }
                }
            }
            // Nothing is wrong and nothing can be drawn: the window is
            // minimized or hidden, or the compositor was busy. Skipping the
            // frame is what wgpu asks for, and a script that clears every
            // frame will simply try again.
            Acquired::Occluded | Acquired::Timeout => return Ok(()),
            other => return Err(format!("cannot present window: {}", acquire_error(&other))),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        with_context(None, |context| {
            let mut encoder =
                context
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("clear"),
                    });
            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: color[0],
                            g: color[1],
                            b: color[2],
                            a: color[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            context.queue.submit([encoder.finish()]);
            // In wgpu 30 the queue presents the frame, not the frame itself.
            // A `SurfaceTexture` that is dropped unpresented discards itself,
            // so this has to happen while the queue is still in hand.
            context.queue.present(frame);
            Ok(())
        })
    }

    /// `__gpu_available()` — whether a window can be opened with
    /// `gpu = true` here: this build has the feature, and the system offers
    /// an adapter. Answered without a window, so before any GPU window
    /// exists it asks for an adapter with no compatible surface.
    fn available(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
        if CONTEXT.with(|cell| matches!(*cell.borrow(), Some(Ok(_)))) {
            return Ok(vec![Value::Bool(true)]);
        }
        let found = with_instance(|instance| {
            pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: power_preference(),
                force_fallback_adapter: force_software_adapter(),
                ..Default::default()
            }))
        })
        .is_ok();
        Ok(vec![Value::Bool(found)])
    }

    /// `__gpu_adapter()` describes the adapter in use: `name`, `backend`
    /// (`"vulkan"`, `"dx12"`, `"metal"`, `"gl"`), `type` (`"discrete"`,
    /// `"integrated"`, `"cpu"`, ...) and `driver`. Only once a GPU window
    /// exists, because the adapter is chosen for a window.
    fn adapter(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
        with_context(None, |context| {
            let info = context.adapter.get_info();
            Ok(vec![ui::record(vec![
                ("name", ui::text(&info.name)),
                (
                    "backend",
                    ui::text(match info.backend {
                        wgpu::Backend::Vulkan => "vulkan",
                        wgpu::Backend::Dx12 => "dx12",
                        wgpu::Backend::Metal => "metal",
                        wgpu::Backend::Gl => "gl",
                        wgpu::Backend::Noop => "none",
                        _ => "other",
                    }),
                ),
                (
                    "type",
                    ui::text(match info.device_type {
                        wgpu::DeviceType::DiscreteGpu => "discrete",
                        wgpu::DeviceType::IntegratedGpu => "integrated",
                        wgpu::DeviceType::VirtualGpu => "virtual",
                        wgpu::DeviceType::Cpu => "cpu",
                        wgpu::DeviceType::Other => "other",
                    }),
                ),
                ("driver", ui::text(&info.driver)),
            ])])
        })
    }

    /// `__gpu_clear(window, color)` fills the window with one colour and
    /// presents it. `color` takes the same forms as in `@neyuki/ui`, parsed
    /// by the same code.
    fn clear(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let id = ui::integer_arg(args, 0, "window")?;
        let (r, g, b, a) = ui::color_arg(args, 1, "color")?;
        if FRAME.with(|frame| frame.borrow().is_some()) {
            return Err(
                "a frame is open: finish it, or discard it with gpu.cancelFrame()".to_string(),
            );
        }
        // The surface is configured with a plain (non-sRGB) format, so the
        // channels reach the screen as the script wrote them, exactly as
        // `@neyuki/ui` would have drawn the same colour.
        let color = [
            r as f64 / 255.0,
            g as f64 / 255.0,
            b as f64 / 255.0,
            a as f64 / 255.0,
        ];
        ui::with_window_gpu(id, |surface| present(surface, color))?;
        Ok(vec![])
    }

    // RESOURCES ------------------------------------------------------------

    /// What a script holds as a shader, buffer, texture, sampler or
    /// pipeline: an id into these maps. The GPU memory behind them is
    /// wgpu's, released when the handle is dropped by `:close()` or when
    /// the process ends, the same arrangement as `@neyuki/ui` images.
    ///
    /// One counter serves every kind, so an id used as the wrong kind is
    /// simply not found rather than quietly naming another resource.
    #[derive(Default)]
    struct Resources {
        shaders: HashMap<u64, wgpu::ShaderModule>,
        buffers: HashMap<u64, BufferEntry>,
        textures: HashMap<u64, TextureEntry>,
        samplers: HashMap<u64, wgpu::Sampler>,
        pipelines: HashMap<u64, PipelineEntry>,
    }

    thread_local! {
        static RESOURCES: RefCell<Resources> = RefCell::new(Resources::default());
        static NEXT_ID: Cell<u64> = const { Cell::new(1) };
        /// The format pipelines render to: the first surface's, which is
        /// the adapter's preferred one. Every GPU window on one adapter is
        /// given the same, so a pipeline works on all of them.
        static TARGET_FORMAT: Cell<Option<wgpu::TextureFormat>> = const { Cell::new(None) };
        /// The frame between `device:frame()` and `frame:finish()`, if one
        /// is open. Only one can be, as it holds the window's next image.
        static FRAME: RefCell<Option<Frame>> = const { RefCell::new(None) };
    }

    fn next_id() -> u64 {
        NEXT_ID.with(|id| {
            let next = id.get();
            id.set(next + 1);
            next
        })
    }

    /// What a buffer may be used for. Scripts say which, because the usage
    /// decides both the wgpu flags and how the size has to be rounded.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Usage {
        Vertex,
        Index,
        Uniform,
    }

    impl Usage {
        fn parse(name: &str) -> Result<Usage, String> {
            match name {
                "vertex" => Ok(Usage::Vertex),
                "index" => Ok(Usage::Index),
                "uniform" => Ok(Usage::Uniform),
                other => Err(format!(
                    "usage must be \"vertex\", \"index\" or \"uniform\", got \"{other}\""
                )),
            }
        }

        fn flags(self) -> wgpu::BufferUsages {
            let kind = match self {
                Usage::Vertex => wgpu::BufferUsages::VERTEX,
                Usage::Index => wgpu::BufferUsages::INDEX,
                Usage::Uniform => wgpu::BufferUsages::UNIFORM,
            };
            // Every buffer here can be rewritten, which is what makes a
            // uniform buffer worth keeping between frames.
            kind | wgpu::BufferUsages::COPY_DST
        }

        /// What the size is rounded up to. A uniform binding is read in
        /// 16-byte units, and anything written to has to be a multiple of
        /// `COPY_BUFFER_ALIGNMENT`, so rounding here saves every script
        /// from padding its own data.
        fn alignment(self) -> u64 {
            match self {
                Usage::Uniform => 16,
                Usage::Vertex | Usage::Index => wgpu::COPY_BUFFER_ALIGNMENT,
            }
        }

        fn label(self) -> &'static str {
            match self {
                Usage::Vertex => "vertex",
                Usage::Index => "index",
                Usage::Uniform => "uniform",
            }
        }
    }

    /// A buffer, with its size beside it so a draw call can be measured
    /// against it here instead of by the driver.
    struct BufferEntry {
        buffer: wgpu::Buffer,
        size: u64,
        usage: Usage,
    }

    /// The texture is kept beside its view so that a canvas redrawn every
    /// frame can be written over the same allocation, and its size so that
    /// a write of the wrong size is refused by name rather than by the
    /// driver. Nothing here reads a texture back.
    struct TextureEntry {
        texture: wgpu::Texture,
        view: wgpu::TextureView,
        width: u32,
        height: u32,
    }

    /// A pipeline and what a draw call needs to know about it: the layout
    /// of group 0, derived from the shader's own declarations, and the
    /// vertex stride its attribute list implies.
    #[derive(Clone)]
    struct PipelineEntry {
        pipeline: wgpu::RenderPipeline,
        bindings: wgpu::BindGroupLayout,
        stride: u64,
    }

    /// A vertex attribute as scripts name it, and the wgpu format it means.
    /// Attributes are packed from offset 0 in the order given and numbered
    /// from `@location(0)`, which is the layout bytes written into a
    /// `buffer` already have.
    fn vertex_format(name: &str) -> Result<wgpu::VertexFormat, String> {
        use wgpu::VertexFormat as Format;
        match name {
            "float" => Ok(Format::Float32),
            "vec2" => Ok(Format::Float32x2),
            "vec3" => Ok(Format::Float32x3),
            "vec4" => Ok(Format::Float32x4),
            "half2" => Ok(Format::Float16x2),
            "half4" => Ok(Format::Float16x4),
            "int" => Ok(Format::Sint32),
            "uint" => Ok(Format::Uint32),
            "ubyte4" => Ok(Format::Uint8x4),
            "unorm4" => Ok(Format::Unorm8x4),
            other => Err(format!(
                "vertex must hold \"float\", \"vec2\", \"vec3\", \"vec4\", \"half2\", \
                 \"half4\", \"int\", \"uint\", \"ubyte4\" or \"unorm4\", got \"{other}\""
            )),
        }
    }

    /// The vertex layout for a comma-separated list of attribute names.
    fn vertex_layout(spec: &str) -> Result<(Vec<wgpu::VertexAttribute>, u64), String> {
        let mut attributes = Vec::new();
        let mut offset = 0;
        for (index, name) in spec.split(',').enumerate() {
            let format = vertex_format(name.trim())?;
            attributes.push(wgpu::VertexAttribute {
                format,
                offset,
                shader_location: index as u32,
            });
            offset += format.size();
        }
        Ok((attributes, offset))
    }

    fn topology(name: &str) -> Result<wgpu::PrimitiveTopology, String> {
        use wgpu::PrimitiveTopology as Topology;
        match name {
            "triangles" => Ok(Topology::TriangleList),
            "triangleStrip" => Ok(Topology::TriangleStrip),
            "lines" => Ok(Topology::LineList),
            "lineStrip" => Ok(Topology::LineStrip),
            "points" => Ok(Topology::PointList),
            other => Err(format!(
                "topology must be \"triangles\", \"triangleStrip\", \"lines\", \
                 \"lineStrip\" or \"points\", got \"{other}\""
            )),
        }
    }

    /// Which faces to drop. Front faces are counter-clockwise, wgpu's
    /// default and the usual pairing with a right-handed world and a y-up
    /// clip space.
    fn cull_mode(name: &str) -> Result<Option<wgpu::Face>, String> {
        match name {
            "none" => Ok(None),
            "back" => Ok(Some(wgpu::Face::Back)),
            "front" => Ok(Some(wgpu::Face::Front)),
            other => Err(format!(
                "cull must be \"none\", \"back\" or \"front\", got \"{other}\""
            )),
        }
    }

    /// Premultiplied alpha, because that is how `@neyuki/ui` keeps pixels
    /// and so how a texture made from one of its images arrives.
    fn blend_state(name: &str) -> Result<Option<wgpu::BlendState>, String> {
        match name {
            "none" => Ok(None),
            "alpha" => Ok(Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING)),
            other => Err(format!(
                "blend must be \"none\" or \"alpha\", got \"{other}\""
            )),
        }
    }

    /// The depth state of a pipeline that does or does not use depth.
    ///
    /// Both have one, because every frame on a GPU window attaches that
    /// window's depth buffer. A pipeline with `depth = false` never writes
    /// it and always passes the test, which lets one frame mix
    /// depth-tested geometry with a flat overlay without a second kind of
    /// render pass, and costs a script that wants no depth nothing but the
    /// buffer itself.
    fn depth_state(depth: bool) -> wgpu::DepthStencilState {
        wgpu::DepthStencilState {
            format: DEPTH_FORMAT,
            depth_write_enabled: Some(depth),
            // `z` runs 0 at the near plane to 1 at the far one, so nearer
            // is less.
            depth_compare: Some(match depth {
                true => wgpu::CompareFunction::Less,
                false => wgpu::CompareFunction::Always,
            }),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }
    }

    /// Reads a `buffer` argument's bytes.
    fn with_bytes<T>(
        args: &[Value],
        index: usize,
        name: &str,
        body: impl FnOnce(&[u8]) -> Result<T, String>,
    ) -> Result<T, String> {
        match args.get(index) {
            Some(Value::Buffer(buffer)) => body(&buffer.borrow().data),
            None | Some(Value::Nil) => Err(format!("{name} must be provided")),
            Some(other) => Err(format!(
                "{} must be a buffer, got {}",
                name,
                other.type_name()
            )),
        }
    }

    fn with_resources<T>(
        body: impl FnOnce(&mut Resources) -> Result<T, String>,
    ) -> Result<T, String> {
        RESOURCES.with(|resources| body(&mut resources.borrow_mut()))
    }

    /// `__gpu_device(window)` checks that `window` is a GPU window and that
    /// the device exists, so `gpu.device` reports a missing adapter once,
    /// where the script asked for it, rather than from the first resource
    /// it tries to build.
    fn device(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let id = ui::integer_arg(args, 0, "window")?;
        ui::with_window_gpu(id, |_| Ok(()))?;
        with_context(None, |_| Ok(vec![]))
    }

    /// `__gpu_shader(source)` compiles WGSL.
    ///
    /// Errors come back through an error scope, and wgpu's message for a
    /// WGSL error already carries the line, the column and a snippet of
    /// the source, so it is passed to the script unchanged.
    fn shader(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let source = str_arg(args, 0, "source")?.to_string();
        let module = with_context(None, |context| {
            capturing(&context.device, "cannot compile the shader", || {
                context
                    .device
                    .create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some(SHADER_LABEL),
                        source: wgpu::ShaderSource::Wgsl(source.into()),
                    })
            })
        })
        .map_err(tidy_shader_error)?;
        let id = next_id();
        with_resources(|resources| {
            resources.shaders.insert(id, module);
            Ok(())
        })?;
        Ok(vec![ui::integer(id)])
    }

    /// `__gpu_buffer(data, usage)` makes a buffer from the bytes of a
    /// `buffer`, or `size` zero bytes when given a number. Returns the id
    /// and the size actually allocated, which is the requested size rounded
    /// up for the usage.
    fn buffer(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let usage = Usage::parse(str_arg(args, 1, "usage")?)?;
        let align = usage.alignment();
        let (requested, data) = match args.first() {
            Some(Value::Buffer(buffer)) => {
                let bytes = buffer.borrow().data.clone();
                (bytes.len() as u64, Some(bytes))
            }
            _ => (ui::integer_arg(args, 0, "data")?, None),
        };
        if requested == 0 {
            return Err("data must not be empty".to_string());
        }
        let size = requested.div_ceil(align) * align;
        // The write has to be as long as the allocation is rounded to, so
        // the padding is zeroed rather than left out of the copy.
        let data = data.map(|mut data| {
            data.resize(size as usize, 0);
            data
        });
        let id = next_id();
        let entry = with_context(None, |context| {
            let buffer = context.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(usage.label()),
                size,
                usage: usage.flags(),
                mapped_at_creation: false,
            });
            if let Some(data) = &data {
                context.queue.write_buffer(&buffer, 0, data);
            }
            Ok(BufferEntry {
                buffer,
                size,
                usage,
            })
        })?;
        check_device_error("cannot create a buffer")?;
        with_resources(|resources| {
            resources.buffers.insert(id, entry);
            Ok(())
        })?;
        Ok(vec![ui::integer(id), ui::integer(size)])
    }

    /// `__gpu_buffer_write(buffer, data, offset)` replaces part of a
    /// buffer's contents. This is how a uniform buffer is refreshed each
    /// frame without making a new one.
    fn buffer_write(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let id = ui::integer_arg(args, 0, "buffer")?;
        let offset = ui::integer_arg(args, 2, "offset")?;
        with_bytes(args, 1, "data", |data| {
            if !offset.is_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT) {
                return Err(format!(
                    "offset must be a multiple of {}",
                    wgpu::COPY_BUFFER_ALIGNMENT
                ));
            }
            // A partial write cannot be padded without overwriting bytes
            // the script may still want, so the length has to be right.
            if !(data.len() as u64).is_multiple_of(wgpu::COPY_BUFFER_ALIGNMENT) {
                return Err(format!(
                    "data must be a multiple of {} bytes long, got {}",
                    wgpu::COPY_BUFFER_ALIGNMENT,
                    data.len()
                ));
            }
            with_resources(|resources| {
                let entry = resources
                    .buffers
                    .get(&id)
                    .ok_or_else(|| "buffer is closed".to_string())?;
                let end = offset + data.len() as u64;
                if end > entry.size {
                    return Err(format!(
                        "writing {} bytes at offset {} runs past the end of a {}-byte buffer",
                        data.len(),
                        offset,
                        entry.size
                    ));
                }
                with_context(None, |context| {
                    context.queue.write_buffer(&entry.buffer, offset, data);
                    Ok(())
                })
            })
        })?;
        check_device_error("cannot write to a buffer")?;
        Ok(vec![])
    }

    /// Writes tightly packed premultiplied RGBA over the whole of a
    /// texture, which is what `@neyuki/ui` hands out for both an image and
    /// a canvas.
    fn upload(
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                // `tiny-skia` packs rows tightly.
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// `__gpu_texture(source)` uploads an `@neyuki/ui` image or canvas.
    ///
    /// The pixels arrive premultiplied, which is how `tiny-skia` keeps
    /// them, so `blend = "alpha"` uses premultiplied blending to match.
    fn texture(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let source = ui::integer_arg(args, 0, "source")?;
        let (id, width, height) = ui::with_pixels_rgba(source, |width, height, pixels| {
            let size = wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            };
            let entry = with_context(None, |context| {
                let texture = context.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("neyuki texture"),
                    size,
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    // Not the sRGB format: `@neyuki/ui` holds plain 8-bit
                    // channels and the surface is a plain format too, so
                    // nothing is converted on the way to the screen.
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                upload(&context.queue, &texture, width, height, pixels);
                Ok(TextureEntry {
                    view: texture.create_view(&wgpu::TextureViewDescriptor::default()),
                    texture,
                    width,
                    height,
                })
            })?;
            check_device_error("cannot create a texture")?;
            let id = next_id();
            with_resources(|resources| {
                resources.textures.insert(id, entry);
                Ok(())
            })?;
            Ok((id, width, height))
        })?;
        Ok(vec![
            ui::integer(id),
            ui::integer(width as u64),
            ui::integer(height as u64),
        ])
    }

    /// `__gpu_texture_update(texture, source)` writes an image or canvas
    /// over a texture that is already on the GPU.
    ///
    /// This is the path a heads-up display needs: a canvas is redrawn with
    /// `@neyuki/ui` every frame and arrives here as one copy into the
    /// allocation the first upload made, with no new texture, no new view
    /// and so no new bind group. The size has to match, because a texture
    /// cannot be resized -- a canvas that grew needs a new one.
    fn texture_update(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let id = ui::integer_arg(args, 0, "texture")?;
        let source = ui::integer_arg(args, 1, "source")?;
        // Lifted out before the pixels are borrowed: the two live in
        // different thread locals, and nothing is held across the write.
        let (texture, width, height) = with_resources(|resources| {
            let entry = resources
                .textures
                .get(&id)
                .ok_or_else(|| "texture is closed".to_string())?;
            Ok((entry.texture.clone(), entry.width, entry.height))
        })?;
        ui::with_pixels_rgba(source, |source_width, source_height, pixels| {
            if (source_width, source_height) != (width, height) {
                return Err(format!(
                    "the source is {source_width}x{source_height} and the texture is {width}x{height}: a texture cannot be resized, so make a new one"
                ));
            }
            with_context(None, |context| {
                upload(&context.queue, &texture, width, height, pixels);
                Ok(())
            })
        })?;
        check_device_error("cannot write to a texture")?;
        Ok(vec![])
    }

    /// `__gpu_sampler(filter, wrap)` makes a sampler. `filter` is
    /// `"linear"` or `"nearest"`; `wrap` is `"clamp"`, `"repeat"` or
    /// `"mirror"`.
    fn sampler(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let filter = match str_arg(args, 0, "filter")? {
            "linear" => wgpu::FilterMode::Linear,
            "nearest" => wgpu::FilterMode::Nearest,
            other => {
                return Err(format!(
                    "filter must be \"linear\" or \"nearest\", got \"{other}\""
                ));
            }
        };
        let wrap = match str_arg(args, 1, "wrap")? {
            "clamp" => wgpu::AddressMode::ClampToEdge,
            "repeat" => wgpu::AddressMode::Repeat,
            "mirror" => wgpu::AddressMode::MirrorRepeat,
            other => {
                return Err(format!(
                    "wrap must be \"clamp\", \"repeat\" or \"mirror\", got \"{other}\""
                ));
            }
        };
        let entry = with_context(None, |context| {
            Ok(context.device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("neyuki sampler"),
                address_mode_u: wrap,
                address_mode_v: wrap,
                address_mode_w: wrap,
                mag_filter: filter,
                min_filter: filter,
                ..Default::default()
            }))
        })?;
        check_device_error("cannot create a sampler")?;
        let id = next_id();
        with_resources(|resources| {
            resources.samplers.insert(id, entry);
            Ok(())
        })?;
        Ok(vec![ui::integer(id)])
    }

    /// `__gpu_pipeline(shader, vertex, depth, cull, topology, blend)`.
    ///
    /// `vertex` is the attribute list as a comma-separated string, packed
    /// from offset 0 and numbered from `@location(0)`. The bind group
    /// layout is taken from the shader rather than declared here: a script
    /// writes `@group(0) @binding(n)` in its WGSL and wgpu derives the rest,
    /// so there is nothing to keep in step by hand.
    fn pipeline(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let shader = ui::integer_arg(args, 0, "shader")?;
        let (attributes, stride) = vertex_layout(str_arg(args, 1, "vertex")?)?;
        let depth = bool_arg(args, 2, "depth")?;
        let cull = cull_mode(str_arg(args, 3, "cull")?)?;
        let topology = topology(str_arg(args, 4, "topology")?)?;
        let blend = blend_state(str_arg(args, 5, "blend")?)?;
        let format = TARGET_FORMAT
            .with(|format| format.get())
            .ok_or_else(|| "no GPU window is open".to_string())?;
        let entry = with_resources(|resources| {
            let module = resources
                .shaders
                .get(&shader)
                .ok_or_else(|| "shader is closed".to_string())?;
            with_context(None, |context| {
                capturing(&context.device, "cannot create the pipeline", || {
                    context
                        .device
                        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                            label: Some("neyuki"),
                            // Derived from the shader's own bindings.
                            layout: None,
                            vertex: wgpu::VertexState {
                                module,
                                entry_point: None,
                                compilation_options: Default::default(),
                                buffers: &[Some(wgpu::VertexBufferLayout {
                                    array_stride: stride,
                                    step_mode: wgpu::VertexStepMode::Vertex,
                                    attributes: &attributes,
                                })],
                            },
                            primitive: wgpu::PrimitiveState {
                                topology,
                                cull_mode: cull,
                                ..Default::default()
                            },
                            depth_stencil: Some(depth_state(depth)),
                            multisample: wgpu::MultisampleState::default(),
                            fragment: Some(wgpu::FragmentState {
                                module,
                                entry_point: None,
                                compilation_options: Default::default(),
                                targets: &[Some(wgpu::ColorTargetState {
                                    format,
                                    blend,
                                    write_mask: wgpu::ColorWrites::ALL,
                                })],
                            }),
                            multiview_mask: None,
                            cache: None,
                        })
                })
            })
        })?;
        // Derived from the shader: wgpu gives an empty layout when it
        // declares nothing in group 0, so binding to such a pipeline is
        // refused by `create_bind_group` with the counts it saw.
        let bindings = with_context(None, |context| {
            capturing(
                &context.device,
                "cannot read the pipeline's bindings",
                || entry.get_bind_group_layout(0),
            )
        })?;
        let id = next_id();
        with_resources(|resources| {
            resources.pipelines.insert(
                id,
                PipelineEntry {
                    pipeline: entry,
                    bindings,
                    stride,
                },
            );
            Ok(())
        })?;
        Ok(vec![ui::integer(id)])
    }

    /// `__gpu_free(kind, id)` drops a resource. The GPU memory goes when
    /// wgpu's last reference does, which may be after a frame still using
    /// it has been submitted.
    fn free(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let kind = str_arg(args, 0, "kind")?;
        let id = ui::integer_arg(args, 1, "id")?;
        with_resources(|resources| {
            match kind {
                "shader" => drop(resources.shaders.remove(&id)),
                "buffer" => drop(resources.buffers.remove(&id)),
                "texture" => drop(resources.textures.remove(&id)),
                "sampler" => drop(resources.samplers.remove(&id)),
                "pipeline" => drop(resources.pipelines.remove(&id)),
                other => return Err(format!("unknown resource kind \"{other}\"")),
            }
            Ok(())
        })?;
        Ok(vec![])
    }

    // FRAMES ---------------------------------------------------------------

    /// What a binding index holds for the next draw call.
    enum Binding {
        Buffer(wgpu::Buffer),
        Texture(wgpu::TextureView),
        Sampler(wgpu::Sampler),
    }

    /// The GPU side of an open frame: the window's next image, the encoder
    /// recording into it, and the render pass.
    ///
    /// The pass borrows the encoder, which a struct cannot express, so the
    /// lifetime is erased with wgpu's own `forget_lifetime`. That is what
    /// it exists for, and the borrow is honoured by construction here: the
    /// encoder is only touched again in `finish_frame`, after the pass has
    /// been dropped.
    struct Target {
        pass: wgpu::RenderPass<'static>,
        encoder: wgpu::CommandEncoder,
        texture: wgpu::SurfaceTexture,
    }

    /// A frame between `device:frame()` and `frame:finish()`.
    struct Frame {
        /// Tells a live frame from a finished one whose handle a script
        /// kept, so using the stale one is an error and not someone else's
        /// frame.
        id: u64,
        window: u64,
        /// Absent when the surface had no image to give and the frame is
        /// being skipped: arguments are still checked, nothing is drawn.
        target: Option<Target>,
        pipeline: Option<PipelineEntry>,
        bindings: BTreeMap<u32, Binding>,
        /// Set when a binding changed, so the bind group is rebuilt once
        /// before the next draw rather than for every draw.
        rebind: bool,
        /// The size of the bound vertex and index buffers, to measure a
        /// draw call against.
        vertices: Option<u64>,
        indices: Option<(u64, wgpu::IndexFormat)>,
    }

    /// Runs `body` on the open frame `id`.
    fn with_frame<T>(
        id: u64,
        body: impl FnOnce(&mut Frame) -> Result<T, String>,
    ) -> Result<T, String> {
        FRAME.with(|frame| {
            let mut frame = frame.borrow_mut();
            let frame = frame
                .as_mut()
                .filter(|frame| frame.id == id)
                .ok_or_else(|| "frame is finished".to_string())?;
            body(frame)
        })
    }

    /// Takes the open frame out of the thread local if it is frame `id`.
    ///
    /// Finishing, reading and cancelling all start this way: the frame is
    /// gone whatever happens next. An error after this point must not leave
    /// a frame open, or the window's image would be held until the script
    /// happened to start another one.
    fn take_frame(id: u64) -> Option<Frame> {
        FRAME.with(|frame| {
            let mut frame = frame.borrow_mut();
            match frame.as_ref().is_some_and(|open| open.id == id) {
                true => frame.take(),
                false => None,
            }
        })
    }

    /// Throws a frame away without presenting it. An unpresented
    /// `SurfaceTexture` discards itself, so the window keeps whatever it
    /// last showed.
    ///
    /// The order matters: the render pass records into the encoder and the
    /// encoder into the window's image, so they go in that order.
    fn discard(frame: Option<Frame>) {
        let Some(Frame {
            target: Some(target),
            ..
        }) = frame
        else {
            return;
        };
        let Target {
            pass,
            encoder,
            texture,
        } = target;
        drop(pass);
        drop(encoder);
        drop(texture);
    }

    /// Discards the open frame if it belongs to `window`, for a window that
    /// is being closed.
    ///
    /// A frame holds the window's next image, and that image cannot outlive
    /// the surface it came from: dropping it afterwards panics inside wgpu,
    /// and the panic lands in a thread local's destructor, which aborts the
    /// process rather than reaching the script. So the frame goes while the
    /// surface is still there.
    pub(crate) fn discard_window_frame(window: u64) {
        let frame = FRAME.with(|frame| {
            let mut frame = frame.borrow_mut();
            match frame.as_ref().is_some_and(|open| open.window == window) {
                true => frame.take(),
                false => None,
            }
        });
        discard(frame);
    }

    /// `__gpu_frame(window, color, depth)` starts a frame on a GPU window:
    /// acquires the window's next image, clears the colour and depth
    /// buffers, and opens a render pass.
    ///
    /// When the surface has no image to give for a reason that is not a
    /// failure -- the window is minimized, or the compositor was busy --
    /// the frame is opened anyway and skipped: every call on it still
    /// checks its arguments, nothing is drawn, and `finish` reports
    /// `false`. A script's draw loop then needs no special case for a
    /// window that cannot be seen.
    fn frame(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        use wgpu::CurrentSurfaceTexture as Acquired;

        let window = ui::integer_arg(args, 0, "window")?;
        let (r, g, b, a) = ui::color_arg(args, 1, "clear")?;
        let depth = number_arg(args, 2, "depth")?;
        if !(0.0..=1.0).contains(&depth) {
            return Err("depth must be between 0 and 1".to_string());
        }
        if FRAME.with(|frame| frame.borrow().is_some()) {
            return Err(
                "a frame is already open: finish it, or discard it with gpu.cancelFrame()"
                    .to_string(),
            );
        }
        let acquired = ui::with_window_gpu(window, |surface| {
            let acquired = match surface.surface.get_current_texture() {
                Acquired::Success(texture) | Acquired::Suboptimal(texture) => Some(texture),
                // Stale: the window resized, the display changed, or the
                // compositor took the surface away. Reconfiguring and
                // asking once more is the documented recovery.
                Acquired::Outdated | Acquired::Lost => {
                    configure(surface)?;
                    match surface.surface.get_current_texture() {
                        Acquired::Success(texture) | Acquired::Suboptimal(texture) => Some(texture),
                        Acquired::Occluded | Acquired::Timeout => None,
                        other => {
                            return Err(format!("cannot start a frame: {}", acquire_error(&other)));
                        }
                    }
                }
                Acquired::Occluded | Acquired::Timeout => None,
                other => return Err(format!("cannot start a frame: {}", acquire_error(&other))),
            };
            match acquired {
                Some(texture) => Ok(Some((texture, depth_view(surface)?))),
                None => Ok(None),
            }
        })?;
        let target = match acquired {
            Some((texture, depth_view)) => Some(with_context(None, |context| {
                let view = texture
                    .texture
                    .create_view(&wgpu::TextureViewDescriptor::default());
                let mut encoder =
                    context
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("frame"),
                        });
                let pass = encoder
                    .begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("frame"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color {
                                    r: f64::from(r) / 255.0,
                                    g: f64::from(g) / 255.0,
                                    b: f64::from(b) / 255.0,
                                    a: f64::from(a) / 255.0,
                                }),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                            view: &depth_view,
                            depth_ops: Some(wgpu::Operations {
                                load: wgpu::LoadOp::Clear(depth as f32),
                                store: wgpu::StoreOp::Store,
                            }),
                            stencil_ops: None,
                        }),
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    })
                    // The pass holds the colour and depth views for as long
                    // as it records; both outlive it here, the depth view
                    // because the frame keeps a clone and the colour view
                    // because the pass is dropped before the texture.
                    .forget_lifetime();
                Ok(Target {
                    pass,
                    encoder,
                    texture,
                })
            })?),
            None => None,
        };
        let id = next_id();
        FRAME.with(|frame| {
            *frame.borrow_mut() = Some(Frame {
                id,
                window,
                target,
                pipeline: None,
                bindings: BTreeMap::new(),
                rebind: false,
                vertices: None,
                indices: None,
            });
        });
        Ok(vec![ui::integer(id)])
    }

    /// `__gpu_frame_pipeline(frame, pipeline)`.
    fn frame_pipeline(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let frame = ui::integer_arg(args, 0, "frame")?;
        let id = ui::integer_arg(args, 1, "pipeline")?;
        let entry = with_resources(|resources| {
            resources
                .pipelines
                .get(&id)
                .cloned()
                .ok_or_else(|| "pipeline is closed".to_string())
        })?;
        with_frame(frame, |frame| {
            if let Some(target) = frame.target.as_mut() {
                target.pass.set_pipeline(&entry.pipeline);
            }
            // The bind group layout belongs to the pipeline, and a layout
            // is derived from one shader's own declarations: what the last
            // pipeline wanted bound is not what this one wants. So the
            // bindings go with it, and a frame that draws a landscape and
            // then a flat overlay binds each pipeline's own resources
            // without the first pipeline's extra sampler making the
            // second's bind group the wrong size.
            frame.bindings.clear();
            // And the next draw builds a group against this pipeline's own
            // layout, rather than leaving the one the last pipeline set on
            // the pass. Where the two layouts happen to agree that group
            // would still be usable, so a script that changed pipeline and
            // forgot to bind would draw with the resources of the one
            // before it; now it is told which binding is missing.
            frame.rebind = true;
            frame.pipeline = Some(entry);
            Ok(())
        })?;
        Ok(vec![])
    }

    /// `__gpu_frame_bind(frame, index, kind, id)` points one
    /// `@group(0) @binding(index)` at a buffer, texture or sampler. The
    /// bind group itself is built at the next draw, once, from everything
    /// bound by then.
    fn frame_bind(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let frame = ui::integer_arg(args, 0, "frame")?;
        let index = ui::integer_arg(args, 1, "index")?;
        let kind = str_arg(args, 2, "kind")?;
        let id = ui::integer_arg(args, 3, "resource")?;
        let index = u32::try_from(index).map_err(|_| "index is too large".to_string())?;
        let binding = with_resources(|resources| match kind {
            "buffer" => {
                let entry = resources
                    .buffers
                    .get(&id)
                    .ok_or_else(|| "buffer is closed".to_string())?;
                if entry.usage != Usage::Uniform {
                    return Err(format!(
                        "only a uniform buffer can be bound, not a {} one",
                        entry.usage.label()
                    ));
                }
                Ok(Binding::Buffer(entry.buffer.clone()))
            }
            "texture" => resources
                .textures
                .get(&id)
                .map(|entry| Binding::Texture(entry.view.clone()))
                .ok_or_else(|| "texture is closed".to_string()),
            "sampler" => resources
                .samplers
                .get(&id)
                .map(|sampler| Binding::Sampler(sampler.clone()))
                .ok_or_else(|| "sampler is closed".to_string()),
            other => Err(format!("cannot bind a {other}")),
        })?;
        with_frame(frame, |frame| {
            frame.bindings.insert(index, binding);
            frame.rebind = true;
            Ok(())
        })?;
        Ok(vec![])
    }

    /// `__gpu_frame_vertices(frame, buffer)`.
    fn frame_vertices(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let frame = ui::integer_arg(args, 0, "frame")?;
        let id = ui::integer_arg(args, 1, "buffer")?;
        let (buffer, size) = with_resources(|resources| {
            let entry = resources
                .buffers
                .get(&id)
                .ok_or_else(|| "buffer is closed".to_string())?;
            if entry.usage != Usage::Vertex {
                return Err(format!(
                    "vertices needs a buffer made with usage \"vertex\", not \"{}\"",
                    entry.usage.label()
                ));
            }
            Ok((entry.buffer.clone(), entry.size))
        })?;
        with_frame(frame, |frame| {
            if let Some(target) = frame.target.as_mut() {
                target.pass.set_vertex_buffer(0, buffer.slice(..));
            }
            frame.vertices = Some(size);
            Ok(())
        })?;
        Ok(vec![])
    }

    /// `__gpu_frame_indices(frame, buffer, format)` makes the next draw an
    /// indexed one. `format` is `"u16"` or `"u32"`.
    fn frame_indices(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let frame = ui::integer_arg(args, 0, "frame")?;
        let id = ui::integer_arg(args, 1, "buffer")?;
        let format = match str_arg(args, 2, "format")? {
            "u16" => wgpu::IndexFormat::Uint16,
            "u32" => wgpu::IndexFormat::Uint32,
            other => {
                return Err(format!(
                    "format must be \"u16\" or \"u32\", got \"{other}\""
                ));
            }
        };
        let (buffer, size) = with_resources(|resources| {
            let entry = resources
                .buffers
                .get(&id)
                .ok_or_else(|| "buffer is closed".to_string())?;
            if entry.usage != Usage::Index {
                return Err(format!(
                    "indices needs a buffer made with usage \"index\", not \"{}\"",
                    entry.usage.label()
                ));
            }
            Ok((entry.buffer.clone(), entry.size))
        })?;
        with_frame(frame, |frame| {
            if let Some(target) = frame.target.as_mut() {
                target.pass.set_index_buffer(buffer.slice(..), format);
            }
            frame.indices = Some((size, format));
            Ok(())
        })?;
        Ok(vec![])
    }

    /// Builds and sets the bind group when a binding changed since the last
    /// draw. Scripts that bind once and draw many times pay for this once.
    fn apply_bindings(frame: &mut Frame) -> Result<(), String> {
        if !frame.rebind {
            return Ok(());
        }
        let pipeline = frame
            .pipeline
            .as_ref()
            .ok_or_else(|| "no pipeline is set".to_string())?;
        let layout = &pipeline.bindings;
        let entries: Vec<wgpu::BindGroupEntry> = frame
            .bindings
            .iter()
            .map(|(index, binding)| wgpu::BindGroupEntry {
                binding: *index,
                resource: match binding {
                    Binding::Buffer(buffer) => buffer.as_entire_binding(),
                    Binding::Texture(view) => wgpu::BindingResource::TextureView(view),
                    Binding::Sampler(sampler) => wgpu::BindingResource::Sampler(sampler),
                },
            })
            .collect();
        let group = with_context(None, |context| {
            // A bind group has to satisfy the whole layout at once, so a
            // missing or mistyped binding is a validation error naming the
            // binding -- better than anything this could say about it.
            capturing(&context.device, "cannot bind", || {
                context
                    .device
                    .create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("frame"),
                        layout,
                        entries: &entries,
                    })
            })
        })?;
        if let Some(target) = frame.target.as_mut() {
            target.pass.set_bind_group(0, &group, &[]);
        }
        frame.rebind = false;
        Ok(())
    }

    /// `__gpu_frame_draw(frame, count, instances)` draws `count` vertices,
    /// or `count` indices when `indices` was called.
    ///
    /// The count is measured against the bound buffers here. The driver
    /// would catch it too, but a validation error from a draw call arrives
    /// through the uncaptured handler, a call later and without saying
    /// which number was wrong.
    fn frame_draw(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let frame = ui::integer_arg(args, 0, "frame")?;
        let count = ui::integer_arg(args, 1, "count")?;
        let instances = ui::integer_arg(args, 2, "instances")?;
        let count = u32::try_from(count).map_err(|_| "count is too large".to_string())?;
        let instances =
            u32::try_from(instances).map_err(|_| "instances is too large".to_string())?;
        with_frame(frame, |frame| {
            let stride = frame
                .pipeline
                .as_ref()
                .ok_or_else(|| "no pipeline is set: call frame:pipeline first".to_string())?
                .stride;
            let vertices = frame
                .vertices
                .ok_or_else(|| "no vertex buffer is set: call frame:vertices first".to_string())?;
            match frame.indices {
                Some((size, format)) => {
                    let width = match format {
                        wgpu::IndexFormat::Uint16 => 2,
                        wgpu::IndexFormat::Uint32 => 4,
                    };
                    if u64::from(count) * width > size {
                        return Err(format!(
                            "{count} indices need {} bytes, but the index buffer holds {size}",
                            u64::from(count) * width
                        ));
                    }
                }
                None => {
                    if stride > 0 && u64::from(count) * stride > vertices {
                        return Err(format!(
                            "{count} vertices need {} bytes, but the vertex buffer holds {vertices}",
                            u64::from(count) * stride
                        ));
                    }
                }
            }
            apply_bindings(frame)?;
            if let Some(target) = frame.target.as_mut() {
                match frame.indices {
                    Some(_) => target.pass.draw_indexed(0..count, 0, 0..instances),
                    None => target.pass.draw(0..count, 0..instances),
                }
            }
            Ok(())
        })?;
        Ok(vec![])
    }

    /// `__gpu_frame_finish(frame)` ends the render pass, submits it and
    /// presents the window. Returns whether anything was presented: a
    /// skipped frame reports `false`.
    ///
    /// The frame is taken out of the thread local first, so it is gone
    /// whatever happens next. An error here must not leave a frame open:
    /// the window's image would be held until the script happened to start
    /// another one.
    fn frame_finish(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let id = ui::integer_arg(args, 0, "frame")?;
        let frame = take_frame(id).ok_or_else(|| "frame is finished".to_string())?;
        let Some(target) = frame.target else {
            return Ok(vec![Value::Bool(false)]);
        };
        // The window may have been closed between the first call on this
        // frame and this one. Presenting to a surface that is gone is not
        // worth finding out the hard way.
        ui::with_window_gpu(frame.window, |_| Ok(()))?;
        let Target {
            pass,
            encoder,
            texture,
        } = target;
        // The encoder cannot be finished while the pass is recording.
        drop(pass);
        with_context(None, |context| {
            context.queue.submit([encoder.finish()]);
            context.queue.present(texture);
            Ok(())
        })?;
        check_device_error("the frame could not be drawn")?;
        Ok(vec![Value::Bool(true)])
    }

    /// `__gpu_frame_cancel(frame)` throws the frame away without presenting
    /// it. The window keeps whatever it last showed, and the frame cannot
    /// be used again.
    fn frame_cancel(_vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let id = ui::integer_arg(args, 0, "frame")?;
        let frame = take_frame(id).ok_or_else(|| "frame is finished".to_string())?;
        discard(Some(frame));
        Ok(vec![])
    }

    /// `__gpu_cancel_frame()` discards whatever frame is open and reports
    /// whether there was one.
    ///
    /// This is the way out of a frame whose handle a script no longer has.
    /// An error raised between `device:frame()` and `frame:finish()` leaves
    /// the frame open -- that is what keeps the checks on a frame from
    /// having to be fatal -- and a draw loop that caught the error has
    /// usually lost the handle with the call it was raised in. Nothing else
    /// would let the next frame start.
    fn cancel_frame(_vm: &mut VM, _args: &[Value]) -> Result<Vec<Value>, String> {
        let frame = FRAME.with(|frame| frame.borrow_mut().take());
        let open = frame.is_some();
        discard(frame);
        Ok(vec![Value::Bool(open)])
    }

    /// Rows of a texture-to-buffer copy start on a multiple of this, so a
    /// readback is padded and then packed tightly again for the script.
    const COPY_ROW_ALIGNMENT: u32 = 256;

    /// `__gpu_frame_read(frame)` finishes the frame, presents it, and reads
    /// the pixels back as tightly packed RGBA, with the size they came at.
    /// That size is in physical pixels, which is what indexing the bytes
    /// needs and is not the window's logical size on a scaled display.
    ///
    /// This waits for the GPU, so it belongs in a test or a screenshot and
    /// not in a draw loop. It is here because it is the only way a script
    /// can check that what was drawn is what was meant -- that depth
    /// testing hid the far triangle, that a texture arrived the right way
    /// up -- and those checks then run on a hidden window and a software
    /// adapter, with no display and no GPU.
    fn frame_read(vm: &mut VM, args: &[Value]) -> Result<Vec<Value>, String> {
        let id = ui::integer_arg(args, 0, "frame")?;
        let frame = take_frame(id).ok_or_else(|| "frame is finished".to_string())?;
        let Some(target) = frame.target else {
            return Ok(vec![Value::Bool(false), Value::Nil, Value::Nil, Value::Nil]);
        };
        ui::with_window_gpu(frame.window, |_| Ok(()))?;
        let Target {
            pass,
            mut encoder,
            texture,
        } = target;
        drop(pass);
        // The surface format is the adapter's preference, which is a BGRA
        // one on Windows and an RGBA one elsewhere. Scripts are handed RGBA
        // either way so a check written on one machine holds on another.
        let format = texture.texture.format();
        let swizzle = match format.remove_srgb_suffix() {
            wgpu::TextureFormat::Rgba8Unorm => false,
            wgpu::TextureFormat::Bgra8Unorm => true,
            other => return Err(format!("cannot read back a {other:?} surface")),
        };
        let (width, height) = (texture.texture.width(), texture.texture.height());
        let padded = (width * 4).div_ceil(COPY_ROW_ALIGNMENT) * COPY_ROW_ALIGNMENT;
        let pixels = with_context(None, |context| {
            let staging = context.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("readback"),
                size: u64::from(padded) * u64::from(height),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &staging,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(padded),
                        rows_per_image: Some(height),
                    },
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
            context.queue.submit([encoder.finish()]);
            context.queue.present(texture);
            let (sender, receiver) = std::sync::mpsc::channel();
            staging.map_async(wgpu::MapMode::Read, .., move |result| {
                let _ = sender.send(result);
            });
            // The map finishes when the copy has, which is why this waits
            // on the queue rather than spinning.
            context
                .device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .map_err(|err| format!("cannot read the frame back: {err}"))?;
            receiver
                .recv()
                .map_err(|_| "the graphics driver did not answer".to_string())?
                .map_err(|err| format!("cannot read the frame back: {err}"))?;
            let view = staging
                .slice(..)
                .get_mapped_range()
                .map_err(|err| format!("cannot read the frame back: {err}"))?;
            let mut pixels = Vec::with_capacity((width * height * 4) as usize);
            for row in view.chunks_exact(padded as usize) {
                let (row, _) = row[..(width * 4) as usize].as_chunks::<4>();
                match swizzle {
                    true => {
                        for pixel in row {
                            pixels.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
                        }
                    }
                    false => pixels.extend_from_slice(row.as_flattened()),
                }
            }
            drop(view);
            staging.unmap();
            Ok(pixels)
        })?;
        check_device_error("the frame could not be read back")?;
        let mut values = crate::vm::libs::buffer::new_buffer(vm, pixels)?;
        values.insert(0, Value::Bool(true));
        values.push(ui::integer(u64::from(width)));
        values.push(ui::integer(u64::from(height)));
        Ok(values)
    }

    pub(crate) const NATIVES: &[NativeDef] = &[
        NativeDef {
            name: "__gpu_available",
            func: available,
        },
        NativeDef {
            name: "__gpu_adapter",
            func: adapter,
        },
        NativeDef {
            name: "__gpu_clear",
            func: clear,
        },
        NativeDef {
            name: "__gpu_device",
            func: device,
        },
        NativeDef {
            name: "__gpu_shader",
            func: shader,
        },
        NativeDef {
            name: "__gpu_buffer",
            func: buffer,
        },
        NativeDef {
            name: "__gpu_buffer_write",
            func: buffer_write,
        },
        NativeDef {
            name: "__gpu_texture",
            func: texture,
        },
        NativeDef {
            name: "__gpu_texture_update",
            func: texture_update,
        },
        NativeDef {
            name: "__gpu_sampler",
            func: sampler,
        },
        NativeDef {
            name: "__gpu_pipeline",
            func: pipeline,
        },
        NativeDef {
            name: "__gpu_free",
            func: free,
        },
        NativeDef {
            name: "__gpu_frame",
            func: frame,
        },
        NativeDef {
            name: "__gpu_frame_pipeline",
            func: frame_pipeline,
        },
        NativeDef {
            name: "__gpu_frame_bind",
            func: frame_bind,
        },
        NativeDef {
            name: "__gpu_frame_vertices",
            func: frame_vertices,
        },
        NativeDef {
            name: "__gpu_frame_indices",
            func: frame_indices,
        },
        NativeDef {
            name: "__gpu_frame_draw",
            func: frame_draw,
        },
        NativeDef {
            name: "__gpu_frame_finish",
            func: frame_finish,
        },
        NativeDef {
            name: "__gpu_frame_read",
            func: frame_read,
        },
        NativeDef {
            name: "__gpu_frame_cancel",
            func: frame_cancel,
        },
        NativeDef {
            name: "__gpu_cancel_frame",
            func: cancel_frame,
        },
    ];
}
