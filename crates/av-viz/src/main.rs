//! av-viz: spectrum visualizer window.
//!
//! Captures system audio (or a chosen source) and draws its spectrum. File
//! playback and the server connection come in later phases.

mod input;
mod render;
mod signal;

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use av_audio::Capture;
use av_core::{Analyzer, DEFAULT_BANDS, DEFAULT_FFT_SIZE, Hsv, Smoother};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use input::Input;
use render::{Renderer, Scene};

/// Spectrum visualizer.
#[derive(clap::Parser)]
#[command(version)]
struct Args {
    /// Audio source to capture, by id or part of its name.
    /// Defaults to what the default output device is playing.
    #[arg(short, long, conflicts_with = "test")]
    source: Option<String>,

    /// List capturable audio sources and exit.
    #[arg(long)]
    sources: bool,

    /// Use a built-in test sweep instead of capturing audio.
    #[arg(long)]
    test: bool,
}

/// Hue drift in degrees per second, as in the C version.
const HUE_SPEED: f32 = 10.0;
/// Longest frame step fed to the simulation, so a stall doesn't cause a jump.
const MAX_DT: f32 = 0.1;

struct App {
    renderer: Option<Renderer>,
    input: Input,
    analyzer: Analyzer,
    smoother: Smoother,
    mags: Vec<f32>,
    samples: Vec<f32>,
    color: Hsv,
    paused: bool,
    last_frame: Instant,
    label: String,
    error: Option<anyhow::Error>,
}

impl App {
    fn new(input: Input) -> Self {
        Self {
            renderer: None,
            input,
            analyzer: Analyzer::new(DEFAULT_FFT_SIZE, DEFAULT_BANDS),
            smoother: Smoother::new(DEFAULT_BANDS),
            mags: vec![0.0; DEFAULT_BANDS],
            samples: Vec::new(),
            color: Hsv::new(210.0, 0.7, 0.8),
            paused: false,
            last_frame: Instant::now(),
            label: String::new(),
            error: None,
        }
    }

    fn update(&mut self) {
        let now = Instant::now();
        let dt = now
            .duration_since(self.last_frame)
            .as_secs_f32()
            .min(MAX_DT);
        self.last_frame = now;

        self.samples.clear();
        self.input.read(dt, self.paused, &mut self.samples);
        self.analyzer.push(&self.samples);
        self.analyzer.analyze(&mut self.mags);
        self.smoother.update(&self.mags, dt);
        self.color.rotate(HUE_SPEED * dt);

        self.label.clear();
        self.input.describe(&mut self.label);
        if self.paused {
            self.label.push_str("  (paused)");
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, err: anyhow::Error) {
        self.error = Some(err);
        event_loop.exit();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("av-viz")
            .with_inner_size(LogicalSize::new(1024.0, 600.0));
        let result = event_loop
            .create_window(attrs)
            .map_err(anyhow::Error::from)
            .and_then(|w| pollster::block_on(Renderer::new(Arc::new(w), event_loop)));
        match result {
            Ok(renderer) => {
                renderer.window().request_redraw();
                self.renderer = Some(renderer);
                self.last_frame = Instant::now();
            }
            Err(err) => self.fail(event_loop, err),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key,
                        state: ElementState::Pressed,
                        repeat: false,
                        ..
                    },
                ..
            } => match logical_key {
                Key::Named(NamedKey::Escape) => event_loop.exit(),
                Key::Named(NamedKey::Space) => self.paused = !self.paused,
                _ => {}
            },
            WindowEvent::Resized(size) => {
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(r) = &mut self.renderer {
                    r.set_scale_factor(scale_factor);
                }
            }
            WindowEvent::RedrawRequested => {
                self.update();
                let Some(renderer) = &mut self.renderer else {
                    return;
                };
                let scene = Scene {
                    bars: self.smoother.bars(),
                    peaks: self.smoother.peaks(),
                    color: self.color.to_rgb(),
                    label: &self.label,
                };
                if let Err(err) = renderer.render(&scene) {
                    self.fail(event_loop, err);
                    return;
                }
                renderer.window().request_redraw();
            }
            _ => {}
        }
    }
}

fn main() -> Result<()> {
    let args: Args = clap::Parser::parse();

    if args.sources {
        for source in av_audio::list_sources()? {
            println!("{source}");
        }
        return Ok(());
    }

    let input = if args.test {
        Input::test()
    } else {
        Input::Capture(Capture::open(args.source.as_deref())?)
    };

    let event_loop = EventLoop::new()?;
    let mut app = App::new(input);
    event_loop.run_app(&mut app)?;
    match app.error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}
