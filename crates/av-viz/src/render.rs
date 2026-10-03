//! wgpu renderer for the "glow bars" look: rounded gradient bars with a soft
//! glow, a faint floor reflection and floating peak caps, all drawn as
//! instanced signed-distance shapes; glyphon draws the label.

use std::sync::Arc;

use anyhow::{Context, Result};
use av_core::{Ball, Hsl, Palette};
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
    /// Bar heights, 1.0 = the full bar area.
    pub bars: &'a [f32],
    /// Peak balls; heights on the same scale as `bars`.
    pub balls: &'a [Ball],
    pub palette: Palette,
    /// Seconds since start, for palettes that change over time.
    pub time: f32,
    pub label: &'a str,
}

/// One rounded, gradient-filled, optionally glowing rectangle (see bars.wgsl).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Shape {
    xywh: [f32; 4],
    /// Corner radius, glow radius, unused, unused.
    params: [f32; 4],
    top: [f32; 4],
    bottom: [f32; 4],
    glow: [f32; 4],
}

impl Shape {
    const ATTRIBS: [wgpu::VertexAttribute; 5] = wgpu::vertex_attr_array![
        0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4, 4 => Float32x4
    ];
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
    rects: Vec<Shape>,
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
    // Layout, as fractions of the window height.
    /// Where bars stand; the reflection goes below.
    const FLOOR: f32 = 0.80;
    /// Highest a bar reaches.
    const CEILING: f32 = 0.08;
    /// Share of each bar's slot that the bar fills; the rest is gap.
    const BAR_FILL: f32 = 0.62;
    /// Reflection length relative to its bar.
    const REFLECTION: f32 = 0.45;

    // Sizes in logical pixels, scaled by the window's DPI factor.
    const GLOW_RADIUS: f32 = 16.0;
    const PEAK_GAP: f32 = 4.0;
    const BALL_GLOW: f32 = 8.0;
    /// Ball diameter relative to the bar width.
    const BALL_SIZE: f32 = 0.95;
    const FONT_SIZE: f32 = 20.0;
    const TEXT_MARGIN: f32 = 18.0;

    /// Near-black stage behind the bars (sRGB).
    const BACKGROUND: [f32; 3] = [5.0 / 255.0, 6.0 / 255.0, 9.0 / 255.0];

    pub async fn new(window: Arc<Window>, event_loop: &ActiveEventLoop) -> Result<Self> {
        // WGPU_BACKEND (e.g. "gl", "vulkan") still overrides the choice below.
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle_from_env(
                Box::new(event_loop.owned_display_handle()),
            ));
        let surface = instance.create_surface(window.clone())?;
        let adapter = pick_adapter(&instance, &surface)
            .await
            .context("no graphics adapter can draw to this window")?;
        let info = adapter.get_info();
        if info.device_type == wgpu::DeviceType::Cpu {
            eprintln!(
                "av-viz: rendering in software ({}); expect high CPU use",
                info.name
            );
        }
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
                    array_stride: size_of::<Shape>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &Shape::ATTRIBS,
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
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let rect_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("rects"),
            contents: &[0; size_of::<Shape>()],
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
                        load: wgpu::LoadOp::Clear(self.clear_color()),
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

    /// Lays out the bars, their reflections, peak caps and the floor line.
    fn build_rects(&mut self, scene: &Scene, w: f32, h: f32) {
        let n = scene.bars.len();
        self.rects.clear();
        if n == 0 {
            return;
        }
        let px = self.window.scale_factor() as f32;
        let floor = h * Self::FLOOR;
        let span = floor - h * Self::CEILING;
        let slot = w / n as f32;
        let bar_w = slot * Self::BAR_FILL;
        let radius = bar_w / 2.0;
        let glow = Self::GLOW_RADIUS * px;
        let color_at = |i: usize| {
            scene
                .palette
                .color(i as f32 / (n - 1).max(1) as f32, scene.time)
        };

        // Reflections first, so the bars' glow lies over them.
        for (i, &bar) in scene.bars.iter().enumerate() {
            let c = color_at(i);
            let bar_h = (bar * span).max(bar_w);
            self.rects.push(Shape {
                xywh: [
                    i as f32 * slot + (slot - bar_w) / 2.0,
                    floor + 3.0 * px,
                    bar_w,
                    bar_h * Self::REFLECTION,
                ],
                params: [radius, 0.0, 0.0, 0.0],
                top: self.rgba(c, 0.22),
                bottom: self.rgba(c, 0.0),
                glow: [0.0; 4],
            });
        }
        self.rects.push(Shape {
            xywh: [0.0, floor + px, w, px],
            params: [0.0; 4],
            top: [0.06, 0.06, 0.06, 0.06],
            bottom: [0.06, 0.06, 0.06, 0.06],
            glow: [0.0; 4],
        });
        for (i, &bar) in scene.bars.iter().enumerate() {
            let c = color_at(i);
            let bar_h = (bar * span).max(bar_w);
            self.rects.push(Shape {
                xywh: [
                    i as f32 * slot + (slot - bar_w) / 2.0,
                    floor - bar_h,
                    bar_w,
                    bar_h,
                ],
                params: [radius, glow, 0.0, 0.0],
                top: self.rgba(c.lighten(0.08), 1.0),
                bottom: self.rgba(c.lighten(-0.28), 0.35),
                glow: self.rgba(c, 0.75),
            });
        }
        // Peak balls, keeping their bar's colour wherever they drift.
        let d = bar_w * Self::BALL_SIZE;
        for (i, ball) in scene.balls.iter().enumerate() {
            let c = color_at(i).lighten(0.22);
            let bottom = floor - (ball.y * span).max(bar_w) - Self::PEAK_GAP * px;
            self.rects.push(Shape {
                xywh: [ball.x * w - d / 2.0, bottom - d, d, d],
                params: [d / 2.0, Self::BALL_GLOW * px, 0.0, 0.0],
                top: self.rgba(c, 0.95),
                bottom: self.rgba(c.lighten(-0.12), 0.95),
                glow: self.rgba(c, 0.5),
            });
        }
    }

    /// Width over height of the area bars grow in, for isotropic ball drift.
    pub fn bar_aspect(&self) -> f32 {
        let (w, h) = (self.config.width as f32, self.config.height as f32);
        w / (h * (Self::FLOOR - Self::CEILING))
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

    /// A palette colour with straight alpha, ready for the shader.
    fn rgba(&self, color: Hsl, alpha: f32) -> [f32; 4] {
        let [r, g, b] = self.output_color(color.to_rgb());
        [r, g, b, alpha]
    }

    fn clear_color(&self) -> wgpu::Color {
        let [r, g, b] = self.output_color(Self::BACKGROUND);
        wgpu::Color {
            r: f64::from(r),
            g: f64::from(g),
            b: f64::from(b),
            a: 1.0,
        }
    }
}

/// Picks the adapter to draw with: any real GPU before a software renderer,
/// then Vulkan/Metal/DX12 before OpenGL.
///
/// wgpu's own choice can land on a software rasterizer (Mesa's llvmpipe) when
/// the GPU's Vulkan driver is missing or incomplete, as on Intel Haswell,
/// even though OpenGL would use the GPU. Drawing every frame on the CPU costs
/// a whole core, heats the machine and starves the audio threads.
async fn pick_adapter(
    instance: &wgpu::Instance,
    surface: &wgpu::Surface<'_>,
) -> Option<wgpu::Adapter> {
    let rank = |info: &wgpu::AdapterInfo| {
        let device = match info.device_type {
            wgpu::DeviceType::DiscreteGpu => 0,
            wgpu::DeviceType::IntegratedGpu => 1,
            wgpu::DeviceType::VirtualGpu => 2,
            wgpu::DeviceType::Other => 3,
            wgpu::DeviceType::Cpu => 4,
        };
        let backend = match info.backend {
            wgpu::Backend::Vulkan | wgpu::Backend::Metal | wgpu::Backend::Dx12 => 0,
            _ => 1,
        };
        (device, backend)
    };
    instance
        .enumerate_adapters(wgpu::Backends::all())
        .await
        .into_iter()
        .filter(|a| a.is_surface_supported(surface))
        .min_by_key(|a| rank(&a.get_info()))
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
                default_color: glyphon::Color::rgba(232, 228, 222, 190),
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
