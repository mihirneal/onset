//! macOS: drawing with wgpu, but doing the last step, "present", ourselves.
//!
//! Why: to know when each frame really reached the screen. wgpu's own
//! present gives no feedback, so the old code guessed from when the next
//! frame buffer came back, and that CPU-side time jitters by several
//! milliseconds under the macOS compositor. Metal can tell us exactly: a
//! "presented handler" on each drawable reports `presentedTime`, the host
//! time (see `hostclock.rs`) at which the frame went on screen.
//!
//! A drawable is one of the layer's frame buffers (the layer keeps two). Per
//! frame:
//!   1. `next_drawable()` waits for a free one. With vsync on this paces the
//!      loop at the refresh rate. If the window is hidden it returns at once
//!      without waiting (as wgpu does), and it gives up after 1 s if no frame
//!      buffer comes back for any other reason, instead of blocking forever.
//!   2. `wrap()` turns its Metal texture into a `wgpu::Texture` we render
//!      into with normal wgpu code, then `queue.submit()`.
//!   3. `present()` registers the presented handler and presents the
//!      drawable with our own command buffer on wgpu's own command queue,
//!      right after wgpu's. Metal runs a queue's command buffers in order, so
//!      ours comes after the frame's rendering, exactly like wgpu's present.
//!
//! The presented handler runs later on one of Metal's own threads. It only
//! sends `(frame id, presentedTime)` down a channel; the frame loop picks
//! those up with `Receiver::try_iter`.
//!
//! Rules this code keeps (from reading wgpu-hal and Apple's headers):
//! - Never also use `surface.get_current_texture()` / `queue.present()`:
//!   both take drawables from the same two-buffer layer.
//! - wgpu's `surface.configure()` turns the 1 s `nextDrawable` timeout off,
//!   so `after_configure()` must run after every configure.
//! - Check that the window is visible before `nextDrawable`, as wgpu does
//!   (gfx-rs/wgpu#8309): while it's hidden, presented frame buffers don't
//!   come back, and each wait would block the main thread (and with it key
//!   presses, scanner pulses included) for the whole 1 s timeout.
//! - Wrapped textures must be described exactly as the surface was
//!   configured; wgpu doesn't check, so a mismatch is undefined behaviour.
//!   That's why `new` and `after_configure` are `unsafe`, and why this
//!   module is private to the crate.
//! - Submit and present on one thread (the frame loop), one right after the
//!   other: wgpu can't lock out other threads for us here.
//! - Don't keep drawables around (not even inside the handler): the layer
//!   only reuses a buffer once the previous drawable has been released.
//!
//! Versions: these objc2 crates must be the ones wgpu-hal 30.0.1 uses
//! (objc2 0.6, block2 0.6, objc2-metal and objc2-quartz-core 0.3), or the
//! layer and queue types wgpu hands out won't match. See Cargo.toml.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::sync::mpsc::Sender;

use block2::RcBlock;
use objc2::Message as _;
use objc2::msg_send;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{NSObject, ProtocolObject};
use objc2_metal::{
    MTLCommandBuffer as _, MTLCommandQueue, MTLDrawable, MTLTexture, MTLTextureType,
};
use objc2_quartz_core::{CALayer, CAMetalDrawable, CAMetalLayer};
use wgpu::hal;

use crate::render::Presented;

/// The closure type inside Metal's `MTLDrawablePresentedHandler`.
type PresentedFn = dyn Fn(NonNull<ProtocolObject<dyn MTLDrawable>>);

#[derive(Debug)]
pub enum PresentError {
    /// wgpu isn't running on Metal, so there's no Metal layer or queue.
    NotMetal(&'static str),
    /// `surface.configure()` hasn't run yet (the layer has no GPU device).
    NotConfigured,
    /// The layer belongs to a different GPU device than wgpu's.
    DeviceMismatch,
    /// Metal couldn't make a command buffer to present with.
    NoCommandBuffer,
}

impl fmt::Display for PresentError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            PresentError::NotMetal(what) => write!(f, "the GPU {what} isn't using Metal"),
            PresentError::NotConfigured => {
                f.write_str("the window's layer isn't set up for drawing yet")
            }
            PresentError::DeviceMismatch => {
                f.write_str("the window's layer belongs to a different GPU")
            }
            PresentError::NoCommandBuffer => f.write_str("Metal couldn't make a command buffer"),
        }
    }
}

/// Why `MetalPresenter::next_drawable` has no frame buffer this time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoDrawable {
    /// The window isn't visible (covered, on another Space, display off).
    /// Checked before waiting, so this comes back at once.
    Hidden,
    /// No frame buffer came back within 1 s, or the layer is in a bad state.
    TimedOut,
}

/// One of the layer's frame buffers, ready to draw into. Only
/// `MetalPresenter::next_drawable` makes these, so its texture is known to
/// belong to wgpu's device.
pub struct Drawable {
    raw: Retained<ProtocolObject<dyn CAMetalDrawable>>,
}

/// Our handles on the Metal objects behind wgpu's surface, device and
/// queue. They're our own references (`Retained`, an Objective-C retain),
/// so using them never takes wgpu's locks.
///
/// Not `Send`: the layer must stay on the thread that runs the frame loop.
pub struct MetalPresenter {
    layer: Retained<CAMetalLayer>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    device: wgpu::Device,
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
}

impl MetalPresenter {
    /// Take the layer and command queue out of wgpu. Call this after
    /// `surface.configure(device, config)`.
    ///
    /// # Safety
    ///
    /// `config` must be exactly the configuration `surface` was last
    /// configured with, and every later `surface.configure()` must be
    /// followed by `after_configure` with its configuration. `wrap()`
    /// describes the drawables with `config`'s format and usage, and wgpu
    /// doesn't check that description: e.g. a `COPY_SRC` usage on a
    /// framebuffer-only drawable would let wgpu read from a texture Metal
    /// forbids reading.
    pub unsafe fn new(
        surface: &wgpu::Surface<'_>,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        config: &wgpu::SurfaceConfiguration,
    ) -> Result<Self, PresentError> {
        // SAFETY (all three `as_hal`s): we only copy handles out and never
        // destroy them. The guards just hold a reference count on wgpu's
        // objects (no lock), and are dropped at the end of each block.
        let layer = {
            let hal_surface = unsafe { surface.as_hal::<hal::api::Metal>() }
                .ok_or(PresentError::NotMetal("surface"))?;
            // A mutex around the layer; clone (retain) it and let go of the
            // lock right away, as wgpu itself does.
            let guard = hal_surface.render_layer().lock();
            Retained::clone(&*guard)
        };
        let device_raw = {
            let hal_device = unsafe { device.as_hal::<hal::api::Metal>() }
                .ok_or(PresentError::NotMetal("device"))?;
            Retained::clone(hal_device.raw_device())
        };
        let queue_raw = {
            let hal_queue = unsafe { queue.as_hal::<hal::api::Metal>() }
                .ok_or(PresentError::NotMetal("queue"))?;
            hal_queue.as_raw().retain()
        };

        // configure() points the layer at wgpu's device. Without that, the
        // drawables wouldn't belong to the device we render with.
        let layer_device = layer.device().ok_or(PresentError::NotConfigured)?;
        if Retained::as_ptr(&layer_device) != Retained::as_ptr(&device_raw) {
            return Err(PresentError::DeviceMismatch);
        }

        let mut presenter = MetalPresenter {
            layer,
            queue: queue_raw,
            device: device.clone(),
            format: config.format,
            usage: config.usage,
        };
        // SAFETY: passed on from our caller.
        unsafe { presenter.after_configure(config) };
        Ok(presenter)
    }

    /// Call after EVERY `surface.configure()` (e.g. on resize), because
    /// configure resets these layer settings.
    ///
    /// - wgpu sets `allowsNextDrawableTimeout = NO`, so `nextDrawable` would
    ///   block forever if no frame buffer came back. We turn the 1 s timeout
    ///   back on as a backstop (the window check in `next_drawable` handles
    ///   the usual cause, a hidden window).
    /// - `presentsWithTransaction` must stay NO (its default), because
    ///   `present()` presents from the command buffer.
    ///
    /// # Safety
    ///
    /// `config` must be exactly the configuration just passed to
    /// `surface.configure()` (see `new`).
    pub unsafe fn after_configure(&mut self, config: &wgpu::SurfaceConfiguration) {
        self.layer.setAllowsNextDrawableTimeout(true);
        if self.layer.presentsWithTransaction() {
            self.layer.setPresentsWithTransaction(false);
        }
        // The layer's pixel format and "framebuffer only" flag come from
        // these, and the wrapped textures must say exactly the same.
        self.format = config.format;
        self.usage = config.usage;
    }

    /// Wait for a free frame buffer. With vsync on and two buffers, this
    /// returns about once per refresh.
    ///
    /// `Err(Hidden)` straight away, without waiting, if the window isn't
    /// visible. `Err(TimedOut)` after 1 s without a frame buffer, which
    /// blocks the calling thread (winit's main thread) for that second.
    ///
    /// Call on the main thread: the visibility check asks AppKit.
    pub fn next_drawable(&self) -> Result<Drawable, NoDrawable> {
        if self.window_hidden() {
            return Err(NoDrawable::Hidden);
        }
        // Without the pool, an autoreleased reference could keep the
        // drawable out of the layer's two-buffer pool for a frame or more.
        autoreleasepool(|_| self.layer.nextDrawable())
            .map(|raw| Drawable { raw })
            .ok_or(NoDrawable::TimedOut)
    }

    /// Whether the window showing the layer is hidden. wgpu-hal does the
    /// same check before every `nextDrawable` (metal/surface.rs, for
    /// gfx-rs/wgpu#8309): while the window is hidden, frames that were
    /// presented never hand their buffers back, so `nextDrawable` would sit
    /// out its whole 1 s timeout every time. False for a layer that isn't
    /// in a window (the headless test).
    fn window_hidden(&self) -> bool {
        /// `NSWindowOcclusionStateVisible` (NSWindow.h).
        const VISIBLE: usize = 1 << 1;
        autoreleasepool(|_| {
            let Some(window) = hosting_window(&self.layer) else {
                return false;
            };
            // SAFETY: `occlusionState` is an NSWindow property of type
            // NSWindowOcclusionState, an NSUInteger, and `window` is an
            // NSWindow (see `hosting_window`).
            let state: usize = unsafe { msg_send![&*window, occlusionState] };
            state & VISIBLE == 0
        })
    }

    /// The drawable's texture as a `wgpu::Texture` to render into. Make one
    /// per frame and drop it after `queue.submit()`; wgpu keeps the texture
    /// alive until the GPU is done with it.
    ///
    /// Always clear it when rendering (`LoadOp::Clear`): wgpu treats it as
    /// already initialised, so `LoadOp::Load` would read an old frame.
    pub fn wrap(&self, drawable: &Drawable) -> wgpu::Texture {
        // Cheap checks of what the SAFETY comments below rely on, in debug
        // builds (the tests): the drawable is from our layer, and its
        // "framebuffer only" flag matches the usage we're about to claim
        // (wgpu-hal sets it exactly when usage is RENDER_ATTACHMENT alone).
        debug_assert!(
            Retained::as_ptr(&drawable.raw.layer()) == Retained::as_ptr(&self.layer),
            "a drawable from another layer"
        );
        // In a pool for the same reason as `next_drawable`.
        let texture = autoreleasepool(|_| drawable.raw.texture());
        debug_assert_eq!(
            texture.isFramebufferOnly(),
            self.usage == wgpu::TextureUsages::RENDER_ATTACHMENT,
            "the wrapped texture's usage doesn't match the layer's configuration"
        );
        // The size comes from the texture itself, not from our config:
        // after a resize, drawables already in flight have the old size.
        let width = texture.width() as u32;
        let height = texture.height() as u32;
        debug_assert_eq!(texture.textureType(), MTLTextureType::Type2D);
        debug_assert_eq!(texture.sampleCount(), 1);
        debug_assert_eq!(texture.mipmapLevelCount(), 1);
        debug_assert_eq!(texture.arrayLength(), 1);

        // SAFETY: the texture is a drawable's (2D, 1 layer, 1 mip), described
        // the same way wgpu-hal describes its own surface textures, with no
        // drop callback.
        let hal_texture = unsafe {
            hal::metal::Device::texture_from_raw(
                texture,
                self.format,
                MTLTextureType::Type2D,
                1,
                1,
                hal::CopyExtent {
                    width,
                    height,
                    depth: 1,
                },
                None,
            )
        };
        let desc = wgpu::TextureDescriptor {
            label: Some("drawable"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: self.usage,
            // "Framebuffer only" drawables can't be viewed as another format.
            view_formats: &[],
        };
        // SAFETY: wgpu doesn't check any of this, so a mismatch would be
        // undefined behaviour rather than an error. It holds because the
        // drawable came from our layer, whose device is wgpu's (checked in
        // `new`); the size is the texture's own; format and usage are exactly
        // the ones the layer was configured with (the contract of `new` and
        // `after_configure`). UNINITIALIZED says the old contents don't
        // matter (every frame is cleared).
        unsafe {
            self.device.create_texture_from_hal::<hal::api::Metal>(
                hal_texture,
                &desc,
                wgpu::TextureUses::UNINITIALIZED,
            )
        }
    }

    /// Put `drawable` on screen at the next refresh, and have Metal send
    /// `(frame_id, presentedTime)` to `feedback` once it's there.
    ///
    /// Call right after the `queue.submit()` that rendered into it, on the
    /// same thread. Takes the drawable by value and releases it on return;
    /// Metal keeps its own reference until the frame is shown.
    pub fn present(
        &self,
        drawable: Drawable,
        frame_id: u64,
        feedback: &Sender<Presented>,
    ) -> Result<(), PresentError> {
        debug_assert!(!self.layer.presentsWithTransaction());
        autoreleasepool(|_| {
            // `commandBuffer` (not the "unretained references" kind), so the
            // command buffer keeps the drawable alive until it's presented.
            let command_buffer = self
                .queue
                .commandBuffer()
                .ok_or(PresentError::NoCommandBuffer)?;

            // The handler gets only a channel end and a number. It reads the
            // time from the drawable Metal passes in, never from ours.
            let tx = feedback.clone();
            let handler = presented_block(move |shown| {
                // If the receiver is gone (Onset is quitting), nobody cares.
                let _ = tx.send((frame_id, shown.presentedTime()));
            });
            // SAFETY: `handler` is a heap block with exactly the signature
            // Metal expects, and its closure is `Send + 'static` (checked by
            // `presented_block`). Metal copies it, so dropping ours at the end
            // of this scope is fine. Apple's docs say to add it before the
            // present is scheduled, so this comes first.
            unsafe { drawable.raw.addPresentedHandler(RcBlock::as_ptr(&handler)) };

            let mtl_drawable: &ProtocolObject<dyn MTLDrawable> =
                ProtocolObject::from_ref(&*drawable.raw);
            command_buffer.presentDrawable(mtl_drawable);
            command_buffer.commit();
            Ok(())
        })
    }
}

/// The NSWindow showing `layer`, if it's in one. The same walk as wgpu-hal's
/// `hosting_window`: wgpu puts its Metal layer inside the NSView's own
/// layer, and the first layer up the tree with a delegate is that view's,
/// whose delegate is the NSView.
fn hosting_window(layer: &CAMetalLayer) -> Option<Retained<NSObject>> {
    let mut current: Option<Retained<CALayer>> = Some(Retained::into_super(layer.retain()));
    while let Some(l) = current {
        if let Some(delegate) = l.delegate() {
            // SAFETY: that delegate is the NSView (see above), and NSView's
            // `window` returns its NSWindow or nil. wgpu-hal sends exactly
            // this message to the same layer tree on every frame it draws.
            return unsafe { msg_send![&*delegate, window] };
        }
        current = l.superlayer();
    }
    None
}

/// Build the block Metal calls when a drawable is presented.
///
/// Metal calls it on one of its own threads, but the block2 crate has no
/// way to say "this block must be safe to send to another thread", so a
/// closure holding, say, an `Rc` would compile and then break at run time.
/// The `Send + 'static` bound here makes the compiler check it before the
/// closure's type is erased.
fn presented_block<F>(f: F) -> RcBlock<PresentedFn>
where
    F: Fn(&ProtocolObject<dyn MTLDrawable>) + Send + 'static,
{
    RcBlock::new(move |drawable: NonNull<ProtocolObject<dyn MTLDrawable>>| {
        // SAFETY: Metal passes the drawable that was presented, valid for
        // the length of the call.
        let drawable = unsafe { drawable.as_ref() };
        // A panic must not unwind into Metal's code.
        let _ = catch_unwind(AssertUnwindSafe(|| f(drawable)));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reviewers' case: the layer is configured RENDER_ATTACHMENT-only
    /// (framebuffer-only drawables), but the presenter is told COPY_SRC as
    /// well. `wrap` must refuse in debug builds before it hands wgpu a
    /// texture description that would let it read from the drawable. This
    /// breaks `new`'s safety contract on purpose; the check fires before
    /// anything unsafe uses the wrong description. No window: a bare layer.
    #[test]
    #[should_panic(expected = "usage doesn't match")]
    fn a_usage_the_layer_wasnt_configured_with_is_caught() {
        let layer = CAMetalLayer::layer();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        let ptr = std::ptr::from_ref(&*layer).cast_mut().cast();
        // SAFETY: a live layer, which wgpu retains for the surface's life.
        let surface = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(ptr))
        }
        .unwrap();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            ..Default::default()
        }))
        .unwrap();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let config = surface.get_default_config(&adapter, 16, 16).unwrap();
        assert_eq!(config.usage, wgpu::TextureUsages::RENDER_ATTACHMENT);
        surface.configure(&device, &config);

        let mut wrong = config.clone();
        wrong.usage |= wgpu::TextureUsages::COPY_SRC;
        // SAFETY: deliberately not met (see above); `wrap` panics first.
        let presenter = unsafe { MetalPresenter::new(&surface, &device, &queue, &wrong) }.unwrap();
        let drawable = presenter.next_drawable().unwrap();
        let _texture = presenter.wrap(&drawable);
    }
}
