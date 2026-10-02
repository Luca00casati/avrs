//! av-viz: spectrum visualizer window.
//!
//! By default it shows what a running `av-server` streams, falling back to
//! analyzing audio in-process when no server is up. Giving files, `--source`
//! or `--test` always runs locally.

mod feed;
mod remote;
mod render;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use av_audio::{Engine, Source};
use av_core::{Hsv, Smoother};
use av_proto::SocketAddr;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

use feed::Feed;
use remote::Remote;
use render::{Renderer, Scene};

/// Spectrum visualizer.
#[derive(clap::Parser)]
#[command(version)]
struct Args {
    /// Audio files or directories to play locally.
    #[arg(conflicts_with_all = ["source", "test", "connect"])]
    files: Vec<PathBuf>,

    /// Repeat the playlist forever.
    #[arg(short, long = "loop", requires = "files")]
    looping: bool,

    /// Audio source to capture locally, by id or part of its name.
    #[arg(short, long, conflicts_with_all = ["test", "connect"])]
    source: Option<String>,

    /// List capturable audio sources and exit.
    #[arg(long)]
    sources: bool,

    /// Use a built-in test sweep instead of real audio.
    #[arg(long, conflicts_with = "connect")]
    test: bool,

    /// Analyze audio in this process, even if a server is running.
    #[arg(long, conflicts_with = "connect")]
    local: bool,

    /// Require an av-server; wait for one instead of falling back to local.
    #[arg(long)]
    connect: bool,

    /// Server socket (a path; a pipe name on Windows).
    /// Defaults to $AVRS_SOCKET, else a per-user location.
    #[arg(long)]
    socket: Option<String>,
}

/// Hue drift in degrees per second, as in the C version.
const HUE_SPEED: f32 = 10.0;
/// Longest frame step fed to the simulation, so a stall doesn't cause a jump.
const MAX_DT: f32 = 0.1;

struct App {
    renderer: Option<Renderer>,
    feed: Feed,
    smoother: Smoother,
    mags: Vec<f32>,
    color: Hsv,
    last_frame: Instant,
    label: String,
    finished: bool,
    error: Option<anyhow::Error>,
}

impl App {
    fn new(feed: Feed) -> Self {
        Self {
            renderer: None,
            feed,
            smoother: Smoother::new(0),
            mags: Vec::new(),
            color: Hsv::new(210.0, 0.7, 0.8),
            last_frame: Instant::now(),
            label: String::new(),
            finished: false,
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

        self.feed.tick(dt, &mut self.mags);
        if self.smoother.bars().len() != self.mags.len() {
            // First frame, or a server with a different band count.
            self.smoother = Smoother::new(self.mags.len());
        }
        self.smoother.update(&self.mags, dt);
        self.color.rotate(HUE_SPEED * dt);

        self.finished = self.feed.should_exit();
        self.label.clear();
        self.feed.describe(&mut self.label);
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
                Key::Named(NamedKey::Space) => self.feed.toggle_pause(),
                Key::Named(NamedKey::ArrowRight) => self.feed.next(),
                Key::Named(NamedKey::ArrowLeft) => self.feed.previous(),
                Key::Character(c) if c.eq_ignore_ascii_case("n") => self.feed.next(),
                Key::Character(c) if c.eq_ignore_ascii_case("p") => self.feed.previous(),
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
                if self.finished {
                    event_loop.exit();
                    return;
                }
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

    let feed = open_feed(&args)?;
    let event_loop = EventLoop::new()?;
    let mut app = App::new(feed);
    event_loop.run_app(&mut app)?;
    match app.error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

fn open_feed(args: &Args) -> Result<Feed> {
    let explicit_source = args.test || !args.files.is_empty() || args.source.is_some();
    if !(explicit_source || args.local) {
        let addr = SocketAddr::resolve(args.socket.as_deref());
        match addr.connect() {
            Ok(conn) => return Ok(Feed::Remote(Remote::new(addr, conn))),
            Err(_) if args.connect => {
                eprintln!("waiting for av-server on {addr}");
                return Ok(Feed::Remote(Remote::waiting(addr)));
            }
            Err(_) => eprintln!("no av-server on {addr}, analyzing locally"),
        }
    }
    let source = Source::open(args.test, &args.files, args.looping, args.source.as_deref())?;
    Ok(Feed::Local(Box::new(Engine::new(source))))
}
