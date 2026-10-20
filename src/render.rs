//! Drawing to the screen with wgpu.
//!
//! The frame loop is: `next_frame()` (waits for a free frame buffer), decide
//! what to show, `draw()`. With vsync on and two frame buffers, `next_frame()`
//! returns about once per refresh, so the display paces the loop.
//!
//! When `next_frame()` returns is only a rough, CPU-side hint of where the
//! display is: under the macOS compositor it jitters by several
//! milliseconds. So on macOS, `draw()` presents the frame through
//! `present.rs`, and Metal later reports when it really reached the screen
//! (host seconds, see `hostclock.rs`). Collect those reports with
//! `presented()` and hand them to the `planner`, which decides which
//! stimulus frame to draw. The photodiode test is what checks those reports
//! against the light coming off the screen.
//!
//! On other systems, wgpu presents the frame itself and there are no
//! reports (`HAS_FEEDBACK` is false).

use std::sync::Arc;
use std::time::Instant;

use winit::window::Window;

#[cfg(target_os = "macos")]
use crate::present::{Drawable, MetalPresenter, NoDrawable};

/// Whether `Renderer::presented()` ever reports anything on this system.
pub const HAS_FEEDBACK: bool = cfg!(target_os = "macos");

/// One report of a frame reaching the screen: `(frame id, presented_s)`.
/// `presented_s` is host seconds, or 0.0 if Metal says the frame was never
/// shown (skipped).
pub type Presented = (u64, f64);

/// What to draw on one frame.
#[derive(Debug, Clone, Copy)]
pub struct Look {
    /// `Some(phase)` draws the checkerboard; `None` draws plain gray.
    pub checkerboard: Option<bool>,
    /// The photodiode square is white when true, black when false.
    pub diode_white: bool,
}

/// Must match `Params` in `shaders/stimulus.wgsl`, byte for byte.
/// `repr(C)` stops Rust from reordering the fields.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    size: [f32; 2],
    checker: u32,
    phase: u32,
    diode: f32,
    diode_px: f32,
    gray: f32,
    _pad: f32,
}

/// A frame buffer we're allowed to draw into, and when we got it.
#[must_use = "draw every frame you get; a dropped frame buffer is never shown"]
pub struct Frame {
    id: u64,
    /// When the frame buffer was handed over (CPU time, read right after
    /// the wait).
    pub acquired: Instant,
    #[cfg(target_os = "macos")]
    drawable: Drawable,
    #[cfg(not(target_os = "macos"))]
    texture: wgpu::SurfaceTexture,
}

impl Frame {
    /// Counts up by one per frame handed out, from 0. Presentation reports
    /// for this frame come back with the same number.
    pub fn id(&self) -> u64 {
        self.id
    }
}

pub struct Renderer {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    params: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    diode_px: f32,
    next_id: u64,
    #[cfg(target_os = "macos")]
    presenter: MetalPresenter,
    #[cfg(target_os = "macos")]
    feedback_tx: std::sync::mpsc::Sender<Presented>,
    #[cfg(target_os = "macos")]
    feedback_rx: std::sync::mpsc::Receiver<Presented>,
    #[cfg(target_os = "macos")]
    present_failed: bool,
    #[cfg(not(target_os = "macos"))]
    needs_configure: bool,
}

fn new_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        // present.rs works with wgpu's Metal backend only, so never let wgpu
        // pick another one (e.g. Vulkan through MoltenVK).
        #[cfg(target_os = "macos")]
        backends: wgpu::Backends::METAL,
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    })
}

impl Renderer {
    pub fn new(window: Arc<Window>, diode_px: u32) -> Result<Self, String> {
        let size = window.inner_size();
        let instance = new_instance();
        let surface = instance
            .create_surface(window)
            .map_err(|e| format!("can't draw to the window: {e}"))?;
        Self::with_surface(&instance, surface, size.width, size.height, diode_px)
    }

    /// Draw into a bare Core Animation layer that isn't in any window, so
    /// nothing reaches the screen. For tests of the present path; Metal
    /// still reports every frame, with `presented_s` 0.0.
    #[cfg(target_os = "macos")]
    pub fn on_layer(
        layer: &objc2_quartz_core::CAMetalLayer,
        width: u32,
        height: u32,
        diode_px: u32,
    ) -> Result<Self, String> {
        let instance = new_instance();
        let ptr = std::ptr::from_ref(layer).cast_mut().cast();
        // SAFETY: `ptr` is a live CAMetalLayer for the length of this call,
        // and wgpu keeps its own reference (a retain) for as long as the
        // surface lives, so it can't dangle later.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(ptr))
        }
        .map_err(|e| format!("can't draw to the layer: {e}"))?;
        Self::with_surface(&instance, surface, width, height, diode_px)
    }

    fn with_surface(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
        diode_px: u32,
    ) -> Result<Self, String> {
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .map_err(|e| format!("no usable GPU: {e}"))?;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .map_err(|e| format!("couldn't open the GPU: {e}"))?;

        let mut config = surface
            .get_default_config(&adapter, width.max(1), height.max(1))
            .ok_or("the GPU can't draw to this window")?;
        // A plain (non-sRGB) format sends our values to the display as is:
        // gray 0.5 becomes pixel value 128, like E-Prime or PsychoPy.
        let caps = surface.get_capabilities(&adapter);
        if let Some(format) = caps.formats.iter().copied().find(|f| !f.is_srgb()) {
            config.format = format;
        }
        // Vsync on: frames are shown only at refreshes, never torn.
        config.present_mode = wgpu::PresentMode::Fifo;
        // Two frame buffers (one on screen, one being drawn), so
        // `next_frame` waits for every refresh.
        config.desired_maximum_frame_latency = 1;
        surface.configure(&device, &config);

        // SAFETY: `config` is what the surface was just configured with, and
        // `resize` (the only other configure) calls `after_configure` with
        // its configuration straight after.
        #[cfg(target_os = "macos")]
        let presenter = unsafe { MetalPresenter::new(&surface, &device, &queue, &config) }
            .map_err(|e| format!("can't time frames on this display: {e}"))?;

        let shader = device.create_shader_module(wgpu::include_wgsl!("shaders/stimulus.wgsl"));
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("params"),
            size: size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("params"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("params"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: params.as_entire_binding(),
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("stimulus"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("stimulus"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        // Unbounded on purpose: Metal's thread must never wait on us.
        #[cfg(target_os = "macos")]
        let (feedback_tx, feedback_rx) = std::sync::mpsc::channel();

        Ok(Renderer {
            surface,
            device,
            queue,
            config,
            pipeline,
            params,
            bind_group,
            diode_px: diode_px as f32,
            next_id: 0,
            #[cfg(target_os = "macos")]
            presenter,
            #[cfg(target_os = "macos")]
            feedback_tx,
            #[cfg(target_os = "macos")]
            feedback_rx,
            #[cfg(target_os = "macos")]
            present_failed: false,
            #[cfg(not(target_os = "macos"))]
            needs_configure: false,
        })
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        // configure() resets the layer's settings; put ours back.
        // SAFETY: `self.config` is exactly what the surface was just
        // configured with.
        #[cfg(target_os = "macos")]
        unsafe {
            self.presenter.after_configure(&self.config)
        };
    }

    /// Wait for the next free frame buffer. With vsync on, this blocks until
    /// the display has taken the previous frame, which paces the loop at the
    /// refresh rate. An `Err` says why there's no frame this time (e.g. the
    /// window is hidden, which returns at once); skip it and try again.
    #[cfg(target_os = "macos")]
    pub fn next_frame(&mut self) -> Result<Frame, &'static str> {
        let drawable = match self.presenter.next_drawable() {
            Ok(drawable) => drawable,
            Err(NoDrawable::Hidden) => return Err("the window is hidden"),
            Err(NoDrawable::TimedOut) => {
                return Err("no frame buffer came back within 1 s");
            }
        };
        Ok(Frame {
            id: self.take_id(),
            acquired: Instant::now(),
            drawable,
        })
    }

    /// Wait for the next free frame buffer. With vsync on, this blocks until
    /// the display has taken the previous frame, which paces the loop at the
    /// refresh rate. An `Err` says why there's no frame this time (e.g. the
    /// window is hidden or was just resized); skip it and try again.
    #[cfg(not(target_os = "macos"))]
    pub fn next_frame(&mut self) -> Result<Frame, &'static str> {
        use wgpu::CurrentSurfaceTexture as Current;

        if self.needs_configure {
            self.surface.configure(&self.device, &self.config);
            self.needs_configure = false;
        }
        let texture = match self.surface.get_current_texture() {
            Current::Success(t) => t,
            Current::Suboptimal(t) => {
                self.needs_configure = true;
                t
            }
            Current::Outdated => {
                self.needs_configure = true;
                return Err("the window changed size");
            }
            Current::Lost => {
                self.needs_configure = true;
                return Err("the display connection was lost");
            }
            Current::Timeout => return Err("the display didn't free a frame in time"),
            Current::Occluded => return Err("the window is hidden"),
            Current::Validation => return Err("a GPU validation error"),
        };
        Ok(Frame {
            id: self.take_id(),
            acquired: Instant::now(),
            texture,
        })
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Draw `look` into `frame` and queue it for the next refresh. On macOS,
    /// Metal reports when it reached the screen; see `presented()`.
    #[cfg(target_os = "macos")]
    pub fn draw(&mut self, frame: Frame, look: Look) {
        let texture = self.presenter.wrap(&frame.drawable);
        self.render(&texture, look);
        // Fine to drop now: wgpu keeps the texture alive until the GPU is
        // done with it.
        drop(texture);
        // Straight after the submit, on the same thread, so the present is
        // queued after the rendering.
        if let Err(e) = self
            .presenter
            .present(frame.drawable, frame.id, &self.feedback_tx)
            && !self.present_failed
        {
            eprintln!("WARNING: couldn't put a frame on screen ({e}).");
            self.present_failed = true;
        }
    }

    /// Draw `look` into `frame` and queue it for the next refresh.
    #[cfg(not(target_os = "macos"))]
    pub fn draw(&mut self, frame: Frame, look: Look) {
        self.render(&frame.texture.texture, look);
        self.queue.present(frame.texture);
    }

    /// Reports of frames that reached the screen since the last call, in
    /// whatever order Metal sent them. Never blocks.
    #[cfg(target_os = "macos")]
    pub fn presented(&self) -> impl Iterator<Item = Presented> + '_ {
        self.feedback_rx.try_iter()
    }

    /// No reports on this system.
    #[cfg(not(target_os = "macos"))]
    pub fn presented(&self) -> impl Iterator<Item = Presented> + '_ {
        std::iter::empty()
    }

    /// Render `look` into `target` and submit it to the GPU.
    fn render(&self, target: &wgpu::Texture, look: Look) {
        let params = Params {
            // The texture's own size: after a resize, frame buffers already
            // handed out keep the old size for a frame or two.
            size: [target.width() as f32, target.height() as f32],
            checker: u32::from(look.checkerboard.is_some()),
            phase: u32::from(look.checkerboard == Some(true)),
            diode: if look.diode_white { 1.0 } else { 0.0 },
            diode_px: self.diode_px,
            gray: 0.5,
            _pad: 0.0,
        };
        self.queue
            .write_buffer(&self.params, 0, bytemuck::bytes_of(&params));

        let view = target.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("stimulus"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Always clear: on macOS the frame buffer still holds
                        // an old frame.
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit([encoder.finish()]);
    }
}
