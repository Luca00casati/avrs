//! wgpu renderer: instanced quads for bars and peaks, glyphon for the label.

use std::sync::Arc;

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use glyphon::{
    Attrs, Buffer, Cache, Family, FontSystem, Metrics, Resolution, Shaping, SwashCache, TextArea,
    TextAtlas, TextBounds, TextRenderer, Viewport,
};
use wgpu::util::DeviceExt;
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

/// Everything needed to draw one frame.
pub struct Scene<'a> {
    /// Bar heights, 1.0 = full window height.
    pub bars: &'a [f32],
    /// Peak marker heights, same scale as `bars`.
    pub peaks: &'a [f32],
    /// sRGB-encoded bar colour.
    pub color: [f32; 3],
    pub label: &'a str,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Rect {
    xywh: [f32; 4],
    rgba: [f32; 4],
}

impl Rect {
    const ATTRIBS: [wgpu::VertexAttribute; 2] =
        wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4];
}

pub struct Renderer {
    instance: wgpu::Instance,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,

    pipeline: wgpu::RenderPipeline,
    globals: wgpu::Buffer,
    globals_bind: wgpu::BindGroup,
    rects: Vec<Rect>,
    rect_buffer: wgpu::Buffer,

    text: Text,

    // Dropped last: the surface must not outlive the window.
    window: Arc<Window>,
}

struct Text {
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    renderer: TextRenderer,
    buffer: Buffer,
    shown: String,
    scale: f32,
}

impl Renderer {
    // Sizes in logical pixels, scaled by the window's DPI factor.
    const BAR_WIDTH: f32 = 3.0;
    const PEAK_HEIGHT: f32 = 4.0;
    const FONT_SIZE: f32 = 40.0;
    const TEXT_MARGIN: f32 = 20.0;
    /// The C version drew bars with alpha 180/255 over black.
    const BAR_ALPHA: f32 = 180.0 / 255.0;

    pub async fn new(window: Arc<Window>, event_loop: &ActiveEventLoop) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(event_loop.owned_display_handle()),
        ));
        let surface = instance.create_surface(window.clone())?;
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
            .context("no suitable GPU adapter")?;
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("av-viz"),
                ..Default::default()
            })
            .await?;

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&adapter, size.width.max(1), size.height.max(1))
            .context("surface is not supported by the adapter")?;
        // Prefer an sRGB format so blending happens in linear space.
        let caps = surface.get_capabilities(&adapter);
        if let Some(&srgb) = caps.formats.iter().find(|f| f.is_srgb()) {
            config.format = srgb;
        }
        config.present_mode = wgpu::PresentMode::AutoVsync;
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::include_wgsl!("bars.wgsl"));
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
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("bars"),
            bind_group_layouts: &[Some(&globals_layout)],
            ..Default::default()
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("bars"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: size_of::<Rect>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &Rect::ATTRIBS,
                })],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: config.format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let rect_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("rects"),
            contents: &[0; size_of::<Rect>()],
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        });

        let text = Text::new(&device, &queue, config.format, window.scale_factor() as f32);

        Ok(Self {
            instance,
            surface,
            device,
            queue,
            config,
            pipeline,
            globals,
            globals_bind,
            rects: Vec::new(),
            rect_buffer,
            text,
            window,
        })
    }

    pub fn window(&self) -> &Window {
        &self.window
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    pub fn set_scale_factor(&mut self, scale: f64) {
        self.text.set_scale(scale as f32);
    }

    pub fn render(&mut self, scene: &Scene) -> Result<()> {
        // A suboptimal frame is still drawn; the surface is reconfigured only
        // after it is presented, since wgpu forbids that while it is held.
        let (frame, suboptimal) = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => (frame, false),
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => (frame, true),
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(self.window.clone())?;
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                anyhow::bail!("surface validation error");
            }
        };

        let (w, h) = (self.config.width as f32, self.config.height as f32);
        self.queue
            .write_buffer(&self.globals, 0, bytemuck::cast_slice(&[w, h, 0.0, 0.0]));
        self.build_rects(scene, w, h);
        self.upload_rects();
        self.text
            .prepare(&self.device, &self.queue, scene.label, w, h)?;

        let view = frame.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
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
            pass.set_bind_group(0, &self.globals_bind, &[]);
            pass.set_vertex_buffer(0, self.rect_buffer.slice(..));
            pass.draw(0..4, 0..self.rects.len() as u32);
            self.text.render(&mut pass)?;
        }
        self.queue.submit(Some(encoder.finish()));
        self.window.pre_present_notify();
        self.queue.present(frame);
        if suboptimal {
            self.surface.configure(&self.device, &self.config);
        }
        self.text.atlas.trim();
        Ok(())
    }

    /// Lays out bars evenly across the width, like the C version.
    fn build_rects(&mut self, scene: &Scene, w: f32, h: f32) {
        let n = scene.bars.len();
        let scale = self.window.scale_factor() as f32;
        // Shrink bars on narrow windows so they never overlap.
        let bar_w = (Self::BAR_WIDTH * scale).min(w / n.max(1) as f32 * 0.75);
        let gap = (w - n as f32 * bar_w) / (n + 1) as f32;
        let peak_h = Self::PEAK_HEIGHT * scale;

        let [r, g, b] = self.output_color(scene.color);
        let bar_rgba = [r, g, b, Self::BAR_ALPHA];
        let white = [1.0; 4];

        self.rects.clear();
        for (i, &bar) in scene.bars.iter().enumerate() {
            let x = gap + i as f32 * (bar_w + gap);
            let bh = bar * h;
            self.rects.push(Rect {
                xywh: [x, h - bh, bar_w, bh],
                rgba: bar_rgba,
            });
        }
        for (i, &peak) in scene.peaks.iter().enumerate() {
            let x = gap + i as f32 * (bar_w + gap);
            self.rects.push(Rect {
                xywh: [x, h - peak * h, bar_w, peak_h],
                rgba: white,
            });
        }
    }

    fn upload_rects(&mut self) {
        let bytes: &[u8] = bytemuck::cast_slice(&self.rects);
        if bytes.len() as u64 > self.rect_buffer.size() {
            self.rect_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rects"),
                size: bytes.len().next_power_of_two() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        self.queue.write_buffer(&self.rect_buffer, 0, bytes);
    }

    /// Converts an sRGB colour to what the surface expects.
    fn output_color(&self, srgb: [f32; 3]) -> [f32; 3] {
        if self.config.format.is_srgb() {
            srgb.map(srgb_to_linear)
        } else {
            srgb
        }
    }
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

impl Text {
    fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        scale: f32,
    ) -> Self {
        let mut font_system = FontSystem::new();
        let cache = Cache::new(device);
        let viewport = Viewport::new(device, &cache);
        let mut atlas = TextAtlas::new(device, queue, &cache, format);
        let renderer = TextRenderer::new(&mut atlas, device, Default::default(), None);
        let buffer = Buffer::new(&mut font_system, Self::metrics(scale));
        Self {
            font_system,
            swash_cache: SwashCache::new(),
            viewport,
            atlas,
            renderer,
            buffer,
            shown: String::new(),
            scale,
        }
    }

    fn metrics(scale: f32) -> Metrics {
        let size = Renderer::FONT_SIZE * scale;
        Metrics::new(size, size * 1.2)
    }

    fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
        self.buffer.set_metrics(Self::metrics(scale));
        self.shown.clear(); // force a reshape
    }

    fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        label: &str,
        w: f32,
        h: f32,
    ) -> Result<()> {
        let margin = Renderer::TEXT_MARGIN * self.scale;
        if label != self.shown {
            self.buffer.set_size(Some(w - 2.0 * margin), None);
            self.buffer.set_text(
                label,
                &Attrs::new().family(Family::SansSerif),
                Shaping::Advanced,
                None,
            );
            self.buffer.shape_until_scroll(&mut self.font_system, false);
            self.shown = label.to_owned();
        }

        self.viewport.update(
            queue,
            Resolution {
                width: w as u32,
                height: h as u32,
            },
        );
        self.renderer.prepare(
            device,
            queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            [TextArea {
                buffer: &self.buffer,
                left: margin,
                top: margin,
                scale: 1.0,
                bounds: TextBounds {
                    left: 0,
                    top: 0,
                    right: w as i32,
                    bottom: h as i32,
                },
                default_color: glyphon::Color::rgb(255, 255, 255),
                custom_glyphs: &[],
            }],
            &mut self.swash_cache,
        )?;
        Ok(())
    }

    fn render(&self, pass: &mut wgpu::RenderPass) -> Result<()> {
        self.renderer.render(&self.atlas, &self.viewport, pass)?;
        Ok(())
    }
}
