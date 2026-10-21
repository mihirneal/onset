//! Drawing scenes with wgpu.
//!
//! Frame pacing lives in the caller: `acquire()` blocks until the display is
//! ready for a new frame (vsync), the caller timestamps that moment, decides
//! what to show, then calls `draw()` which submits and presents.
//!
//! `Renderer::headless` draws without a window, for snapshots in tests.

use crate::brain;
use crate::color::Color;
use crate::rig::Corner;
use crate::scene::{Item, Scene, WelcomeScreen};
use anyhow::{Context, Result, anyhow};
use glyphon::cosmic_text::Weight;
use glyphon::{
    Attrs, Buffer as TextBuffer, Cache, ColorMode, Family, FontSystem, Metrics, Resolution, Shaping,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use winit::window::Window;

const MAX_SHAPES: usize = 4096;
const MAX_IMAGES: usize = 256;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ShapeInstance {
    center: [f32; 2],
    half: [f32; 2],
    color_a: [f32; 4],
    color_b: [f32; 4],
    params: [f32; 4],
    kind: u32,
    phase: u32,
    _pad: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ImageInstance {
    center: [f32; 2],
    half: [f32; 2],
}

/// Uniforms for `shaders/welcome.wgsl`, in physical pixels from the top-left.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct WelcomeUniforms {
    size: [f32; 2],
    time: f32,
    scale: f32,
    /// Dot-matrix brain: center x, y, half height, strength (0 = none).
    brain: [f32; 4],
    /// Hairline rule: x0, y, x1, on.
    rule: [f32; 4],
    /// Standby dot: x, y, radius (logical px), on.
    dot: [f32; 4],
}

/// How to shape a text, beyond its size and font.
#[derive(Clone, Copy)]
struct TextStyle {
    weight: Weight,
    /// Letter spacing, in em.
    spacing: f32,
    /// Wrap width in pixels.
    wrap: f32,
    /// Left-aligned (otherwise centered).
    left: bool,
    /// Line height, as a multiple of the size.
    leading: f32,
}

/// A shaped text: its cache key, the width of its longest line, its height
/// and its wrap width.
struct Shaped {
    key: String,
    w: f32,
    h: f32,
    wrap: f32,
}

/// Fonts that ship inside Onset for the welcome screen: Space Grotesk and
/// Space Mono (SIL Open Font License, `src/fonts/OFL-*.txt`).
const BUNDLED_FONTS: [&[u8]; 2] =
    [include_bytes!("fonts/SpaceGrotesk-Variable.ttf"), include_bytes!("fonts/SpaceMono-Regular.ttf")];

/// Logo height, as a fraction of the screen height.
const LOGO_HEIGHT: f32 = 0.085;

struct GpuImage {
    bind_group: wgpu::BindGroup,
    width: f32,
    height: f32,
}

enum Draw {
    Shapes(std::ops::Range<u32>),
    Image(PathBuf, u32),
}

pub struct PhotodiodePatch {
    pub corner: Corner,
    /// Logical pixels.
    pub size: f32,
}

pub struct Renderer {
    // Field order matters: the surface must drop before the window.
    // Both are None for a headless renderer.
    surface: Option<wgpu::Surface<'static>>,
    instance: wgpu::Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    pub adapter_info: wgpu::AdapterInfo,

    globals: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    shape_pipeline: wgpu::RenderPipeline,
    shape_buf: wgpu::Buffer,
    image_pipeline: wgpu::RenderPipeline,
    image_layout: wgpu::BindGroupLayout,
    image_buf: wgpu::Buffer,
    sampler: wgpu::Sampler,
    images: HashMap<PathBuf, GpuImage>,
    welcome_pipeline: wgpu::RenderPipeline,
    welcome_buf: wgpu::Buffer,
    welcome_bind: wgpu::BindGroup,
    brain_pipeline: wgpu::RenderPipeline,
    /// The brain's surface points, made the first time a welcome screen is drawn.
    brain_points: Option<(wgpu::Buffer, u32)>,

    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    text_cache: HashMap<String, TextBuffer>,

    pub scale: f32,
    pub photodiode: Option<PhotodiodePatch>,
    /// Shown in a top corner of the welcome screen (`set_logo`).
    logo: Option<PathBuf>,
    window: Option<Arc<Window>>,
}

impl Renderer {
    pub fn new(window: Arc<Window>, display: Box<dyn wgpu::wgt::WgpuHasDisplayHandle>) -> Result<Renderer> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(display));
        let surface = instance.create_surface(window.clone()).context("create window surface")?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .context("no GPU adapter can draw to this window")?;
        let adapter_info = adapter.get_info();
        let (device, queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor { label: Some("onset"), ..Default::default() }),
        )
        .context("create GPU device")?;

        let caps = surface.get_capabilities(&adapter);
        // A non-sRGB format sends color values to the display unmodified,
        // like other stimulus software: [128,128,128] means pixel value 128.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .or_else(|| caps.formats.first().copied())
            .ok_or_else(|| anyhow!("the window surface supports no formats"))?;
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            caps.alpha_modes[0]
        };
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            // Fifo = wait for vblank; never tear, never drop a queued frame.
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            // One frame in flight: acquire blocks until the previous frame
            // is on screen, which is what makes our flip timestamps work.
            desired_maximum_frame_latency: 1,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);
        let scale = window.scale_factor() as f32;
        Ok(Renderer::build(instance, Some(surface), Some(window), adapter_info, device, queue, config, scale))
    }

    /// A renderer with no window that only draws offscreen (`snapshot`), at
    /// `width` × `height` physical pixels in the format a Mac display uses.
    /// `acquire` always returns None.
    pub fn headless(width: u32, height: u32, scale: f32) -> Result<Renderer> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            ..Default::default()
        }))
        .context("no GPU adapter")?;
        let adapter_info = adapter.get_info();
        let (device, queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor { label: Some("onset"), ..Default::default() }),
        )
        .context("create GPU device")?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: wgpu::TextureFormat::Bgra8Unorm,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: vec![],
            desired_maximum_frame_latency: 1,
            color_space: wgpu::SurfaceColorSpace::Auto,
        };
        Ok(Renderer::build(instance, None, None, adapter_info, device, queue, config, scale))
    }

    /// Pipelines, buffers and text setup, shared by both constructors.
    #[allow(clippy::too_many_arguments)]
    fn build(
        instance: wgpu::Instance,
        surface: Option<wgpu::Surface<'static>>,
        window: Option<Arc<Window>>,
        adapter_info: wgpu::AdapterInfo,
        device: wgpu::Device,
        queue: wgpu::Queue,
        config: wgpu::SurfaceConfiguration,
        scale: f32,
    ) -> Renderer {
        let format = config.format;
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("globals"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let globals_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("globals"),
            layout: &globals_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() }],
        });

        let blend_target = [Some(wgpu::ColorTargetState {
            format,
            blend: Some(wgpu::BlendState::ALPHA_BLENDING),
            write_mask: wgpu::ColorWrites::ALL,
        })];
        let strip =
            wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() };

        // Shapes
        let shape_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shapes"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/shapes.wgsl").into()),
        });
        let shape_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("shapes"),
            bind_group_layouts: &[Some(&globals_layout)],
            ..Default::default()
        });
        let shape_attrs = wgpu::vertex_attr_array![
            0 => Float32x2, 1 => Float32x2, 2 => Float32x4, 3 => Float32x4,
            4 => Float32x4, 5 => Uint32, 6 => Uint32
        ];
        let shape_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("shapes"),
            layout: Some(&shape_layout),
            vertex: wgpu::VertexState {
                module: &shape_shader,
                entry_point: Some("vs_main"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ShapeInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &shape_attrs,
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shape_shader,
                entry_point: Some("fs_main"),
                targets: &blend_target,
                compilation_options: Default::default(),
            }),
            primitive: strip,
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: None,
        });
        let shape_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shape instances"),
            size: (MAX_SHAPES * std::mem::size_of::<ShapeInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // Images
        let image_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        multisampled: false,
                        view_dimension: wgpu::TextureViewDimension::D2,
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let image_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("image"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/image.wgsl").into()),
        });
        let image_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("image"),
            bind_group_layouts: &[Some(&globals_layout), Some(&image_layout)],
            ..Default::default()
        });
        let image_attrs = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2];
        let image_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("image"),
            layout: Some(&image_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &image_shader,
                entry_point: Some("vs_main"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<ImageInstance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &image_attrs,
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &image_shader,
                entry_point: Some("fs_main"),
                targets: &blend_target,
                compilation_options: Default::default(),
            }),
            primitive: strip,
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: None,
        });
        let image_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image instances"),
            size: (MAX_IMAGES * std::mem::size_of::<ImageInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        // Welcome background: one full-screen triangle, opaque.
        let welcome_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("welcome"),
            size: std::mem::size_of::<WelcomeUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let welcome_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("welcome"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let welcome_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("welcome"),
            layout: &welcome_layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: welcome_buf.as_entire_binding() }],
        });
        let welcome_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("welcome"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/welcome.wgsl").into()),
        });
        let welcome_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("welcome"),
            bind_group_layouts: &[Some(&welcome_layout)],
            ..Default::default()
        });
        let welcome_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("welcome"),
            layout: Some(&welcome_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &welcome_shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &welcome_shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: None,
        });
        // The welcome brain: one quad per surface point, light added up.
        let brain_attrs = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4];
        let additive = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::One,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent::OVER,
        };
        let brain_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("welcome brain"),
            layout: Some(&welcome_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &welcome_shader,
                entry_point: Some("vs_brain"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 32,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &brain_attrs,
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &welcome_shader,
                entry_point: Some("fs_brain"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(additive),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: strip,
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            cache: None,
            multiview_mask: None,
        });

        // Text
        let mut font_system = FontSystem::new();
        for font in BUNDLED_FONTS {
            font_system.db_mut().load_font_data(font.to_vec());
        }
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let color_mode = if format.is_srgb() { ColorMode::Accurate } else { ColorMode::Web };
        let mut atlas = TextAtlas::with_color_mode(&device, &queue, &cache, format, color_mode);
        let text_renderer = TextRenderer::new(&mut atlas, &device, wgpu::MultisampleState::default(), None);

        Renderer {
            surface,
            instance,
            device,
            queue,
            config,
            adapter_info,
            globals,
            globals_bind,
            shape_pipeline,
            shape_buf,
            image_pipeline,
            image_layout,
            image_buf,
            sampler,
            images: HashMap::new(),
            welcome_pipeline,
            welcome_buf,
            welcome_bind,
            brain_pipeline,
            brain_points: None,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            text_cache: HashMap::new(),
            scale,
            photodiode: None,
            logo: None,
            window,
        }
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    pub fn resize(&mut self, width: u32, height: u32, scale: f32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.scale = scale;
        self.text_cache.clear();
        if let Some(surface) = &self.surface {
            surface.configure(&self.device, &self.config);
        }
        if let Err(e) = self.upload_logo() {
            eprintln!("onset: {e:#}");
        }
    }

    /// Show this image (PNG or JPEG) in a top corner of the welcome screen.
    pub fn set_logo(&mut self, path: &Path) -> Result<()> {
        self.logo = Some(path.to_path_buf());
        self.upload_logo().inspect_err(|_| self.logo = None)
    }

    /// Upload the logo resized to the size it's drawn at, so it stays crisp
    /// (images are drawn without mipmaps).
    fn upload_logo(&mut self) -> Result<()> {
        let Some(path) = self.logo.clone() else { return Ok(()) };
        let img =
            image::open(&path).with_context(|| format!("can't load logo {}", path.display()))?.to_rgba8();
        let th = (LOGO_HEIGHT * self.config.height as f32).round().max(1.0) as u32;
        let tw = (th as f32 * img.width() as f32 / img.height() as f32).round().max(1.0) as u32;
        let img = image::imageops::resize(&img, tw, th, image::imageops::FilterType::Lanczos3);
        self.upload_image(&path, &img);
        Ok(())
    }

    /// Load and upload images before the run so nothing is decoded mid-run.
    pub fn preload_images<'a>(&mut self, paths: impl IntoIterator<Item = &'a PathBuf>) -> Result<()> {
        for path in paths {
            self.load_image(path)?;
        }
        Ok(())
    }

    fn load_image(&mut self, path: &Path) -> Result<()> {
        if self.images.contains_key(path) {
            return Ok(());
        }
        let img =
            image::open(path).with_context(|| format!("can't load image {}", path.display()))?.to_rgba8();
        self.upload_image(path, &img);
        Ok(())
    }

    fn upload_image(&mut self, path: &Path, img: &image::RgbaImage) {
        let (w, h) = img.dimensions();
        let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            img,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * w), rows_per_image: None },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("image"),
            layout: &self.image_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        self.images.insert(path.to_path_buf(), GpuImage { bind_group, width: w as f32, height: h as f32 });
    }

    /// Shape and rasterize every text in `scenes` now, so the first frame
    /// that shows a word doesn't pay for font loading and glyph rendering.
    pub fn prewarm(&mut self, scenes: &[Scene]) {
        let w = self.config.width as f32;
        let mut keys = Vec::new();
        for scene in scenes {
            for item in &scene.items {
                match item {
                    Item::Text { text, size, font, .. } => {
                        keys.push(self.ensure_text(text, size * self.scale, font.as_deref(), w))
                    }
                    Item::Welcome(wl) => {
                        self.ensure_brain();
                        let mut placed = Vec::new();
                        self.layout_welcome(wl, &mut placed);
                        keys.extend(placed.into_iter().map(|p| p.0));
                    }
                    _ => {}
                }
            }
        }
        keys.sort();
        keys.dedup();
        self.viewport
            .update(&self.queue, Resolution { width: self.config.width, height: self.config.height });
        // Prepare in batches; this uploads each glyph into the atlas.
        for chunk in keys.chunks(64) {
            let areas: Vec<TextArea> = chunk
                .iter()
                .filter_map(|k| self.text_cache.get(k))
                .map(|buffer| TextArea {
                    buffer,
                    left: 0.0,
                    top: 0.0,
                    scale: 1.0,
                    bounds: TextBounds {
                        left: 0,
                        top: 0,
                        right: w as i32,
                        bottom: self.config.height as i32,
                    },
                    default_color: glyphon::Color::rgb(255, 255, 255),
                    custom_glyphs: &[],
                })
                .collect();
            let _ = self.text_renderer.prepare(
                &self.device,
                &self.queue,
                &mut self.font_system,
                &mut self.atlas,
                &self.viewport,
                areas,
                &mut self.swash_cache,
            );
            self.atlas.trim();
        }
    }

    /// Block until the display can take a new frame. Returns None if the
    /// surface needed reconfiguring (just call again next loop).
    pub fn acquire(&mut self) -> Option<wgpu::SurfaceTexture> {
        let (surface, window) = (self.surface.as_mut()?, self.window.as_ref()?);
        match surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => Some(frame),
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => Some(frame),
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => None,
            wgpu::CurrentSurfaceTexture::Outdated => {
                surface.configure(&self.device, &self.config);
                None
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                if let Ok(s) = self.instance.create_surface(window.clone()) {
                    *surface = s;
                }
                surface.configure(&self.device, &self.config);
                None
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                eprintln!("onset: GPU validation error acquiring a frame");
                None
            }
        }
    }

    /// Draw `scene` into `frame` and present it.
    pub fn draw(&mut self, frame: wgpu::SurfaceTexture, scene: &Scene, photodiode_on: Option<bool>) {
        let view = frame.texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.render_to(&view, scene, photodiode_on);
        if let Some(window) = &self.window {
            window.pre_present_notify();
        }
        self.queue.present(frame);
        self.atlas.trim();
    }

    /// Render `scene` offscreen at the window's size and return it as an image.
    /// Slow (waits for the GPU); for documentation and debugging, not mid-run.
    pub fn snapshot(&mut self, scene: &Scene, photodiode_on: Option<bool>) -> Result<image::RgbaImage> {
        let (w, h) = (self.config.width, self.config.height);
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("snapshot"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        self.render_to(&view, scene, photodiode_on);
        self.atlas.trim();

        let row = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("snapshot readback"),
            size: (row * h) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: None,
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(encoder.finish()));
        buffer.map_async(wgpu::MapMode::Read, .., |_| {});
        self.device.poll(wgpu::PollType::wait_indefinitely()).map_err(|e| anyhow!("GPU wait failed: {e}"))?;
        let data = buffer.get_mapped_range(..).map_err(|e| anyhow!("snapshot readback failed: {e:?}"))?;
        let bgra = matches!(
            self.config.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let mut img = image::RgbaImage::new(w, h);
        for y in 0..h {
            let src = &data[(y * row) as usize..(y * row + w * 4) as usize];
            for (x, px) in src.chunks_exact(4).enumerate() {
                let p = if bgra { [px[2], px[1], px[0], 255] } else { [px[0], px[1], px[2], 255] };
                img.put_pixel(x as u32, y, image::Rgba(p));
            }
        }
        Ok(img)
    }

    fn render_to(&mut self, view: &wgpu::TextureView, scene: &Scene, photodiode_on: Option<bool>) {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        let s = self.scale;
        self.queue.write_buffer(&self.globals, 0, bytemuck::cast_slice(&[w, h, 0.0, 0.0]));

        let mut shapes: Vec<ShapeInstance> = Vec::new();
        let mut image_instances: Vec<ImageInstance> = Vec::new();
        let mut draws: Vec<Draw> = Vec::new();
        let mut texts: Vec<(String, f32, f32, Color)> = Vec::new();
        let mut welcome: Option<&WelcomeScreen> = None;

        let push_shape = |shapes: &mut Vec<ShapeInstance>, draws: &mut Vec<Draw>, inst: ShapeInstance| {
            if shapes.len() >= MAX_SHAPES {
                return;
            }
            let i = shapes.len() as u32;
            shapes.push(inst);
            match draws.last_mut() {
                Some(Draw::Shapes(r)) if r.end == i => r.end = i + 1,
                _ => draws.push(Draw::Shapes(i..i + 1)),
            }
        };
        let shape = |center: [f32; 2], half: [f32; 2], a: Color, kind: u32| ShapeInstance {
            center,
            half,
            color_a: a.to_array(),
            color_b: a.to_array(),
            params: [0.0; 4],
            kind,
            phase: 0,
            _pad: [0; 2],
        };

        for item in &scene.items {
            match item {
                Item::Rect { x, y, w: rw, h: rh, color } => push_shape(
                    &mut shapes,
                    &mut draws,
                    shape([x * s, y * s], [rw * s / 2.0, rh * s / 2.0], *color, 0),
                ),
                Item::Circle { x, y, r, color, border, border_color } => {
                    let mut inst = shape([x * s, y * s], [r * s, r * s], *color, 1);
                    inst.color_b = border_color.to_array();
                    inst.params[0] = border * s;
                    push_shape(&mut shapes, &mut draws, inst)
                }
                Item::Checker { x, y, inner, outer, rings, wedges, phase, a, b, .. } => {
                    let mut inst = shape([x * s, y * s], [outer * s, outer * s], *a, 2);
                    inst.color_b = b.to_array();
                    inst.params = [inner * s, outer * s, *rings as f32, *wedges as f32];
                    inst.phase = *phase as u32;
                    push_shape(&mut shapes, &mut draws, inst);
                }
                Item::Image { path, x, y, w: iw, h: ih } => {
                    if !self.images.contains_key(path) {
                        if let Err(e) = self.load_image(path) {
                            eprintln!("onset: {e:#}");
                            continue;
                        }
                    }
                    let img = &self.images[path];
                    let (dw, dh) = match (iw, ih) {
                        (Some(a), Some(b)) => (*a, *b),
                        (Some(a), None) => (*a, a * img.height / img.width),
                        (None, Some(b)) => (b * img.width / img.height, *b),
                        (None, None) => (img.width, img.height),
                    };
                    if image_instances.len() < MAX_IMAGES {
                        let i = image_instances.len() as u32;
                        image_instances.push(ImageInstance {
                            center: [x * s, y * s],
                            half: [dw * s / 2.0, dh * s / 2.0],
                        });
                        draws.push(Draw::Image(path.clone(), i));
                    }
                }
                Item::Text { text, x, y, size, color, font } => {
                    let key = self.ensure_text(text, size * s, font.as_deref(), w);
                    texts.push((key, x * s, y * s, *color));
                }
                Item::Welcome(wl) => welcome = Some(wl),
            }
        }
        // Only the screens before t0 have a welcome item; without one this
        // costs nothing (no uniform upload, no draw call).
        let welcome = welcome.map(|wl| self.layout_welcome(wl, &mut texts));
        let brain = welcome.as_ref().is_some_and(|(u, _)| u.brain[3] > 0.0);
        if brain {
            self.ensure_brain();
        }
        if let Some((u, logo)) = &welcome {
            self.queue.write_buffer(&self.welcome_buf, 0, bytemuck::bytes_of(u));
            if let (Some(logo), Some(path)) = (logo, &self.logo) {
                draws.push(Draw::Image(path.clone(), image_instances.len() as u32));
                image_instances.push(*logo);
            }
        }

        // Photodiode patch goes on top of everything.
        let photodiode_range = if let (Some(on), Some(p)) = (photodiode_on, &self.photodiode) {
            let side = p.size * s;
            let (sx, sy) = match p.corner {
                Corner::BottomLeft => (-1.0, -1.0),
                Corner::BottomRight => (1.0, -1.0),
                Corner::TopLeft => (-1.0, 1.0),
                Corner::TopRight => (1.0, 1.0),
            };
            let center = [sx * (w - side) / 2.0, sy * (h - side) / 2.0];
            let i = shapes.len() as u32;
            shapes.push(shape(
                center,
                [side / 2.0, side / 2.0],
                if on { Color::WHITE } else { Color::BLACK },
                0,
            ));
            Some(i..i + 1)
        } else {
            None
        };

        if !shapes.is_empty() {
            self.queue.write_buffer(&self.shape_buf, 0, bytemuck::cast_slice(&shapes));
        }
        if !image_instances.is_empty() {
            self.queue.write_buffer(&self.image_buf, 0, bytemuck::cast_slice(&image_instances));
        }

        // Prepare text.
        self.viewport
            .update(&self.queue, Resolution { width: self.config.width, height: self.config.height });
        let areas: Vec<TextArea> = texts
            .iter()
            .filter_map(|(key, x, y, color)| {
                let buf = self.text_cache.get(key)?;
                let (bw, bh) = text_extent(buf);
                let [r, g, b, a] = color.to_u8();
                Some(TextArea {
                    buffer: buf,
                    left: (w / 2.0 + x - bw / 2.0).round(),
                    top: (h / 2.0 - y - bh / 2.0).round(),
                    scale: 1.0,
                    bounds: TextBounds { left: 0, top: 0, right: w as i32, bottom: h as i32 },
                    default_color: glyphon::Color::rgba(r, g, b, a),
                    custom_glyphs: &[],
                })
            })
            .collect();
        if let Err(e) = self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            areas,
            &mut self.swash_cache,
        ) {
            eprintln!("onset: text error: {e}");
        }

        let mut encoder =
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        {
            let bg = scene.background;
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg.r as f64,
                            g: bg.g as f64,
                            b: bg.b as f64,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if welcome.is_some() {
                pass.set_pipeline(&self.welcome_pipeline);
                pass.set_bind_group(0, &self.welcome_bind, &[]);
                pass.draw(0..3, 0..1);
                if let (true, Some((points, n))) = (brain, &self.brain_points) {
                    pass.set_pipeline(&self.brain_pipeline);
                    pass.set_vertex_buffer(0, points.slice(..));
                    pass.draw(0..4, 0..*n);
                }
            }
            pass.set_bind_group(0, &self.globals_bind, &[]);
            for d in &draws {
                match d {
                    Draw::Shapes(range) => {
                        pass.set_pipeline(&self.shape_pipeline);
                        pass.set_vertex_buffer(0, self.shape_buf.slice(..));
                        pass.draw(0..4, range.clone());
                    }
                    Draw::Image(path, i) => {
                        pass.set_pipeline(&self.image_pipeline);
                        pass.set_bind_group(1, &self.images[path].bind_group, &[]);
                        pass.set_vertex_buffer(0, self.image_buf.slice(..));
                        pass.draw(0..4, *i..*i + 1);
                    }
                }
            }
            if let Err(e) = self.text_renderer.render(&self.atlas, &self.viewport, &mut pass) {
                eprintln!("onset: text error: {e}");
            }
            if let Some(range) = photodiode_range {
                pass.set_pipeline(&self.shape_pipeline);
                pass.set_bind_group(0, &self.globals_bind, &[]);
                pass.set_vertex_buffer(0, self.shape_buf.slice(..));
                pass.draw(0..4, range);
            }
        }
        self.queue.submit(Some(encoder.finish()));
    }

    /// Lay out the welcome screen: its text goes into `texts`; returns the
    /// shader's uniforms and where the logo goes. Sizes follow the screen
    /// height, so it looks the same on any display.
    ///
    /// Everything hangs off one left margin, like a spec sheet: a small mono
    /// label, the name large and light, a hairline rule, then label/value
    /// rows, with the brain on the right. With instructions the name moves to
    /// the top and the instructions go under the rule; they get smaller, and
    /// then the name goes, if they don't fit.
    fn layout_welcome(
        &mut self,
        wl: &WelcomeScreen,
        texts: &mut Vec<(String, f32, f32, Color)>,
    ) -> (WelcomeUniforms, Option<ImageInstance>) {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        let s = self.scale;
        let (display, mono) = (Some(wl.font.as_str()), Some(wl.mono_font.as_str()));
        let tone = |a: f32| Color { a: wl.color.a * a, ..wl.color };
        let (x0, margin) = (0.075 * w, 0.06 * h);
        let line = |weight: Weight, spacing: f32| TextStyle {
            weight,
            spacing,
            wrap: 0.9 * w,
            left: true,
            leading: 1.25,
        };
        let (label_px, label_style) = (0.0165 * h, line(Weight::NORMAL, 0.16));
        // Shaped text with its left edge at x and its top at y.
        let put = |out: &mut Vec<(String, f32, f32, Color)>, t: &Shaped, x: f32, y: f32, color: Color| {
            out.push((t.key.clone(), x - w / 2.0 + t.wrap / 2.0, h / 2.0 - y - t.h / 2.0, color))
        };
        let mut out = Vec::new();
        let greeting = self.shape(&wl.greeting.to_uppercase(), label_px, mono, label_style);
        let hero_line = wl.name.as_deref().unwrap_or(&wl.title);
        let (brain, rule, dot);

        if wl.waiting {
            let hero = self.fit_line(hero_line, 0.13 * h, display, 0.48 * w);
            let (value_px, value_style) = (0.03 * h, line(Weight::NORMAL, 0.0));
            // Label/value rows; the status row gets the standby dot.
            let mut rows = Vec::new();
            if wl.name.is_some() {
                let label = self.shape("STUDY", label_px, mono, label_style);
                rows.push((label, self.shape(&wl.title, value_px, display, value_style), false));
            }
            if let Some(text) = &wl.text {
                let label = self.shape("STATUS", label_px, mono, label_style);
                rows.push((label, self.shape(text, value_px, display, value_style), true));
            }
            let row_gap = 0.016 * h;
            let rows_h: f32 = rows.iter().map(|(_, v, _)| v.h + row_gap).sum();
            let mut y = 0.5 * h - (greeting.h + hero.h + 0.05 * h + rows_h) / 2.0;
            put(&mut out, &greeting, x0, y, tone(0.5));
            y += greeting.h;
            put(&mut out, &hero, x0, y, tone(1.0));
            y += hero.h + 0.02 * h;
            rule = [x0, y, x0 + 0.42 * w, 1.0];
            y += 0.03 * h;
            let value_x = x0 + 0.095 * w;
            let mut standby = [0.0; 4];
            for (label, value, has_dot) in &rows {
                put(&mut out, label, x0, y + (value.h - label.h) / 2.0, tone(0.45));
                let mut x = value_x;
                if *has_dot {
                    standby = [value_x + 4.0 * s, y + value.h / 2.0, 3.5, 1.0];
                    x += 0.02 * h;
                }
                put(&mut out, value, x, y, tone(0.88));
                y += value.h + row_gap;
            }
            dot = standby;
            brain = [0.74 * w, 0.52 * h, 0.29 * h, 1.0];
        } else {
            let top = if self.logo.is_some() { margin + LOGO_HEIGHT * h + 0.05 * h } else { margin };
            let bottom = h - margin - 0.05 * h;
            let hero = self.fit_line(hero_line, 0.075 * h, display, 0.55 * w);
            let study = wl.name.as_ref().map(|_| {
                let label = self.shape("STUDY", label_px, mono, label_style);
                (label, self.shape(&wl.title, 0.026 * h, display, line(Weight::NORMAL, 0.0)))
            });
            let label = self.shape("INSTRUCTIONS", label_px, mono, label_style);
            let header_h = greeting.h + hero.h + 0.055 * h;
            // The instructions at `font_size`, but at most 4.4% of the screen
            // height, made smaller until they fit; without the name if need be.
            let text_font = wl.text_font.as_deref().or(display);
            let style =
                TextStyle { weight: Weight::NORMAL, spacing: 0.0, wrap: 0.58 * w, left: true, leading: 1.4 };
            let size = (wl.text_size * s).min(0.044 * h);
            let mut layout = None;
            'fit: for with_header in [true, false] {
                for scale in [1.0, 0.9, 0.8, 0.7] {
                    let body = wl.text.as_deref().map(|t| self.shape(t, size * scale, text_font, style));
                    let start = top + if with_header { header_h } else { 0.0 } + label.h + 0.012 * h;
                    if start + body.as_ref().map_or(0.0, |b| b.h) <= bottom || (!with_header && scale == 0.7)
                    {
                        layout = Some((with_header, body));
                        break 'fit;
                    }
                }
            }
            let (with_header, body) = layout.expect("the last try always fits");
            let mut y = top;
            if with_header {
                put(&mut out, &greeting, x0, y, tone(0.5));
                y += greeting.h;
                put(&mut out, &hero, x0, y, tone(1.0));
                if let Some((study_label, title)) = &study {
                    // On the right, level with the bottom of the name.
                    let tx = w - x0 - title.w;
                    let ty = y + hero.h - title.h;
                    put(&mut out, title, tx, ty, tone(0.88));
                    put(
                        &mut out,
                        study_label,
                        tx - 0.012 * w - study_label.w,
                        ty + (title.h - study_label.h) / 2.0,
                        tone(0.45),
                    );
                }
                y += hero.h + 0.02 * h;
                rule = [x0, y, w - x0, 1.0];
                y += 0.035 * h;
            } else {
                rule = [0.0; 4];
            }
            put(&mut out, &label, x0, y, tone(0.45));
            y += label.h + 0.012 * h;
            if let Some(body) = &body {
                put(&mut out, body, x0, y, tone(0.92));
            }
            dot = [0.0; 4];
            brain = [0.8 * w, (y + bottom) / 2.0, 0.2 * h, 0.6];
        }

        // Corners: the logo top left and "POWERED BY ONSET" bottom right,
        // each moving out of the photodiode patch's way.
        let patch = |corner: Corner| match &self.photodiode {
            Some(p) if p.corner == corner => p.size * s + margin,
            _ => 0.0,
        };
        let footer_left = patch(Corner::BottomRight);
        let logo_right = patch(Corner::TopLeft) > 0.0;
        if wl.powered_by {
            let px = 0.0145 * h;
            let by = self.shape("POWERED BY", px, mono, line(Weight::NORMAL, 0.16));
            let onset = self.shape("ONSET", px, mono, line(Weight::NORMAL, 0.16));
            let right = w - x0 - footer_left;
            let top = h - margin - by.h;
            put(&mut out, &onset, right - onset.w, top, tone(0.9));
            put(&mut out, &by, right - onset.w - 0.6 * px - by.w, top, tone(0.38));
        }
        let logo = self.logo.as_ref().and_then(|p| self.images.get(p)).map(|img| {
            let lh = LOGO_HEIGHT * h;
            let lw = lh * img.width / img.height;
            let left = if logo_right { w - x0 - lw } else { x0 };
            ImageInstance {
                center: [left + lw / 2.0 - w / 2.0, h / 2.0 - margin - lh / 2.0],
                half: [lw / 2.0, lh / 2.0],
            }
        });
        texts.extend(out);
        (WelcomeUniforms { size: [w, h], time: wl.time, scale: s, brain, rule, dot }, logo)
    }

    /// Make the brain's surface points and upload them, once.
    fn ensure_brain(&mut self) {
        if self.brain_points.is_some() {
            return;
        }
        // The same for every renderer, so make them once per process.
        static POINTS: std::sync::OnceLock<Vec<[f32; 8]>> = std::sync::OnceLock::new();
        let points = POINTS.get_or_init(brain::surface_points);
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("welcome brain"),
            size: std::mem::size_of_val(points.as_slice()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buffer, 0, bytemuck::cast_slice(points));
        self.brain_points = Some((buffer, points.len() as u32));
    }

    /// `text` in a light weight at `px`, or smaller until it fits on one
    /// line no wider than `max_w`.
    fn fit_line(&mut self, text: &str, mut px: f32, font: Option<&str>, max_w: f32) -> Shaped {
        let wrap = 0.9 * self.config.width as f32;
        loop {
            let style = TextStyle { weight: Weight::LIGHT, spacing: -0.025, wrap, left: true, leading: 1.15 };
            let shaped = self.shape(text, px, font, style);
            let lines = self.text_cache[&shaped.key].layout_runs().count();
            if (lines <= 1 && shaped.w <= max_w) || px < 20.0 {
                return shaped;
            }
            px *= 0.9;
        }
    }

    /// Shape `text` (cached) and measure it.
    fn shape(&mut self, text: &str, px: f32, font: Option<&str>, style: TextStyle) -> Shaped {
        let key = self.ensure_styled_text(text, px, font, style);
        let (w, h) = line_extent(&self.text_cache[&key]);
        Shaped { key, w, h, wrap: style.wrap }
    }

    /// Shape `text` once and cache it; returns the cache key.
    fn ensure_text(&mut self, text: &str, px: f32, font: Option<&str>, screen_w: f32) -> String {
        let style = TextStyle {
            weight: Weight::NORMAL,
            spacing: 0.0,
            wrap: 0.9 * screen_w,
            left: false,
            leading: 1.3,
        };
        self.ensure_styled_text(text, px, font, style)
    }

    /// `ensure_text` with a weight, letter spacing, wrap width, alignment and line height.
    fn ensure_styled_text(&mut self, text: &str, px: f32, font: Option<&str>, style: TextStyle) -> String {
        let TextStyle { weight, spacing, wrap, left, leading } = style;
        let key = format!("{px}|{font:?}|{}|{spacing}|{wrap}|{left}|{leading}|{text}", weight.0);
        if !self.text_cache.contains_key(&key) {
            let mut buf = TextBuffer::new(&mut self.font_system, Metrics::new(px, px * leading));
            buf.set_size(Some(wrap), None);
            let family = match font {
                Some(name) => Family::Name(name),
                None => Family::SansSerif,
            };
            let mut attrs = Attrs::new().family(family).weight(weight);
            if spacing != 0.0 {
                attrs = attrs.letter_spacing(spacing);
            }
            let align =
                if left { glyphon::cosmic_text::Align::Left } else { glyphon::cosmic_text::Align::Center };
            buf.set_text(text, &attrs, Shaping::Advanced, Some(align));
            buf.shape_until_scroll(&mut self.font_system, false);
            self.text_cache.insert(key.clone(), buf);
        }
        key
    }
}

/// Width (the wrap box) and height of shaped text.
fn text_extent(buf: &TextBuffer) -> (f32, f32) {
    let width = buf.size().0.unwrap_or(0.0);
    let height = buf.layout_runs().map(|r| r.line_top + r.line_height).fold(0.0, f32::max);
    (width, height)
}

/// Width of the longest line and height of shaped text.
fn line_extent(buf: &TextBuffer) -> (f32, f32) {
    let width = buf.layout_runs().map(|r| r.line_w).fold(0.0, f32::max);
    (width, text_extent(buf).1)
}
