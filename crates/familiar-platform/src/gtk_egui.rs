//! Headless egui renderer: renders an egui frame to an offscreen wgpu RGBA8
//! texture and downloads it to a CPU buffer each frame. The GTK backend blits
//! that buffer into the window's `Picture` via `MemoryTexture`. This is the
//! embed path — egui owns no window, takes no input; it's pure render-target +
//! (later) event consumer that we fully control, so it can't contend with the
//! pet window's transparency / click-through / compositor drag.
//!
//! See `docs/index.md` §3 (canvas-overlay) + the egui render spike. Run with
//! `VK_ICD_FILENAMES=/etc/vulkan/icd.d/nvidia_icd.json` (lavapipe fallback).

use std::cell::{Cell, RefCell};

use egui_wgpu::wgpu;

/// Owns the persistent GPU + egui state for one render surface (the pet window).
/// `new` is async (adapter/device discovery); `render` is synchronous per-frame.
///
/// GTK pointer/key events are queued via [`push_event`] and drained into the
/// next frame's `RawInput` (egui sees only what we forward — the no-contention
/// embed). [`wants_pointer_at`] exposes last frame's interactive widget rects so
/// the GTK press handler can decide: click a widget (forward) vs drag the body
/// (`begin_move`).
pub struct EguiSurface {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: egui_wgpu::Renderer,
    ctx: egui::Context,
    tex: wgpu::Texture,
    view: wgpu::TextureView,
    readback: wgpu::Buffer,
    size: (u32, u32),
    /// Padded bytes-per-row (wgpu requires a multiple of 256 for buffer copies).
    bpr: u32,
    /// GTK events awaiting the next frame's `RawInput`.
    pending: RefCell<Vec<egui::Event>>,
    /// Last frame's interactive widget rects (canvas px) for press hit-testing.
    interactive_rects: RefCell<Vec<egui::Rect>>,
    /// Demand-render flag: set on first frame + by every pushed event, cleared
    /// each render. While false the tick callback skips the GPU work entirely
    /// (idle pet → ~0% CPU instead of rendering 60 fps).
    needs_frame: Cell<bool>,
}

impl EguiSurface {
    /// Discover a Vulkan adapter + create the device, egui context/renderer, and
    /// the fixed-size offscreen render target + readback buffer. Call via
    /// `pollster::block_on` (one-time, on the first frame).
    pub async fn new(size: (u32, u32)) -> Self {
        let (w, h) = size;
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN,
            ..Default::default()
        });
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .expect("no Vulkan adapter (set VK_ICD_FILENAMES to nvidia/lvp icd)");
        eprintln!("[geek-familiar] egui adapter: {:?}", adapter.get_info());
        let (device, queue) = adapter
            .request_device(
                &wgpu::DeviceDescriptor {
                    label: None,
                    required_features: wgpu::Features::empty(),
                    required_limits: wgpu::Limits::downlevel_defaults(),
                },
                None,
            )
            .await
            .expect("egui wgpu device request failed");

        let format = wgpu::TextureFormat::Rgba8Unorm;
        let tex = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("egui-target"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let bpr = ((w * 4) + 255) & !255;
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("egui-readback"),
            size: bpr as u64 * h as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            renderer: egui_wgpu::Renderer::new(&device, format, None, 1),
            ctx: {
                let ctx = egui::Context::default();
                ctx.set_pixels_per_point(1.0);
                ctx
            },
            device,
            queue,
            tex,
            view,
            readback,
            size,
            bpr,
            pending: RefCell::new(Vec::new()),
            interactive_rects: RefCell::new(Vec::new()),
            needs_frame: Cell::new(true),
        }
    }

    /// Queue a GTK-derived egui event for the next frame.
    pub fn push_event(&self, ev: egui::Event) {
        self.pending.borrow_mut().push(ev);
        self.needs_frame.set(true);
    }

    /// Whether the next tick should render. False when idle (no pending work).
    pub fn should_render(&self) -> bool {
        self.needs_frame.get() || !self.pending.borrow().is_empty()
    }

    /// Did last frame's UI place an interactive widget at `pos` (canvas px)?
    /// Used by the GTK press handler to choose forward-to-egui vs drag-window.
    pub fn wants_pointer_at(&self, pos: egui::Pos2) -> bool {
        self.interactive_rects
            .borrow()
            .iter()
            .any(|r| r.contains(pos))
    }

    /// Run one egui frame (`build` adds the UI + reports its interactive widget
    /// rects), render it into the offscreen texture (cleared transparent), and
    /// return the packed RGBA8 pixels. Drains queued events into the frame's
    /// `RawInput`.
    pub fn render(&mut self, build: impl FnOnce(&egui::Context, &mut Vec<egui::Rect>)) -> Vec<u8> {
        let (w, h) = self.size;
        let events = self.pending.borrow_mut().drain(..).collect::<Vec<_>>();
        self.needs_frame.set(false);
        let mut rects = Vec::new();
        let full = self.ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(w as f32, h as f32),
                )),
                events,
                ..Default::default()
            },
            |ctx| build(ctx, &mut rects),
        );

        // font atlas arrives in textures_delta on the first frame.
        for (id, delta) in &full.textures_delta.set {
            self.renderer.update_texture(&self.device, &self.queue, *id, delta);
        }
        for id in &full.textures_delta.free {
            self.renderer.free_texture(id);
        }
        let paint_jobs = self.ctx.tessellate(full.shapes, full.pixels_per_point);
        let screen_desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [w, h],
            pixels_per_point: 1.0,
        };

        let mut encoder =
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        self.renderer
            .update_buffers(&self.device, &self.queue, &mut encoder, &paint_jobs, &screen_desc);
        {
            let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.renderer.render(&mut rpass, &paint_jobs, &screen_desc);
        }
        self.queue.submit(std::iter::once(encoder.finish()));

        // copy texture → buffer → map → strip row padding → packed RGBA8
        let bpr = self.bpr;
        let bpr_unpadded = w * 4;
        let mut enc = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &self.tex,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &self.readback,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(bpr),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        self.queue.submit(std::iter::once(enc.finish()));
        let slice = self.readback.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        let _ = self.device.poll(wgpu::Maintain::Wait);
        let data = { slice.get_mapped_range().to_vec() };
        self.readback.unmap();

        let mut out = Vec::with_capacity(w as usize * h as usize * 4);
        for row in data.chunks_exact(bpr as usize) {
            out.extend_from_slice(&row[..bpr_unpadded as usize]);
        }
        // remember this frame's interactive widget rects for press hit-testing
        *self.interactive_rects.borrow_mut() = rects;
        out
    }
}
