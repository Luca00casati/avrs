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
use av_core::{Balls, Palette, Smoother, pool_max};
use av_proto::{Config, SocketAddr, VizConfig};
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

    /// Config file [default: the per-user avrs/config.toml].
    #[arg(long)]
    config: Option<PathBuf>,

    /// Milliseconds to add to the measured output latency (negative to
    /// subtract) if the visuals run ahead of or behind the sound.
    #[arg(long, conflicts_with = "connect", allow_negative_numbers = true,
          value_parser = clap::value_parser!(i32).range(-(av_proto::MAX_DELAY_MS as i64)..=av_proto::MAX_DELAY_MS as i64))]
    delay_ms: Option<i32>,
}

/// Longest frame step fed to the simulation, so a stall doesn't cause a jump.
const MAX_DT: f32 = 0.1;
/// After this long without sound, the bars drop and the balls drift off.
const SILENCE_SECS: f32 = 1.0;
/// Levels below this count as silence.
const SILENCE_LEVEL: f32 = 1e-3;
/// Step for the `[` and `]` sync keys, in ms.
const DELAY_STEP_MS: i32 = 10;
/// How long the delay stays in the label after adjusting it.
const DELAY_LABEL_SECS: u64 = 4;

struct App {
    renderer: Option<Renderer>,
    feed: Feed,
    smoother: Smoother,
    mags: Vec<f32>,
    /// Band magnitudes reduced to one per bar.
    bar_mags: Vec<f32>,
    balls: Balls,
    /// How long the input has been silent, in seconds.
    silent_for: f32,
    palette: Palette,
    started: Instant,
    window_size: LogicalSize<f64>,
    last_frame: Instant,
    label: String,
    finished: bool,
    /// Show the sync delay in the label until then.
    show_delay_until: Option<Instant>,
    delay_adjusted: bool,
    error: Option<anyhow::Error>,
}

impl App {
    fn new(feed: Feed, config: &VizConfig) -> Self {
        Self {
            renderer: None,
            feed,
            smoother: Smoother::new(config.bars),
            mags: Vec::new(),
            bar_mags: vec![0.0; config.bars],
            balls: Balls::new(config.bars, seed()),
            silent_for: 0.0,
            palette: config.palette.parse().unwrap_or_default(),
            started: Instant::now(),
            window_size: LogicalSize::new(f64::from(config.width), f64::from(config.height)),
            last_frame: Instant::now(),
            label: String::new(),
            finished: false,
            show_delay_until: None,
            delay_adjusted: false,
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
        if !self.mags.is_empty() {
            pool_max(&self.mags, &mut self.bar_mags);
        } else {
            self.bar_mags.fill(0.0);
        }

        let loud = self.bar_mags.iter().any(|&m| m > SILENCE_LEVEL);
        self.silent_for = if loud { 0.0 } else { self.silent_for + dt };
        let idle = self.feed.is_stopped() || self.silent_for > SILENCE_SECS;
        if idle {
            // Let the bars fall instead of freezing them.
            self.bar_mags.fill(0.0);
        }
        self.smoother.update(&self.bar_mags, dt);
        let aspect = self.renderer.as_ref().map_or(2.0, Renderer::bar_aspect);
        self.balls.update(self.smoother.peaks(), idle, aspect, dt);

        self.finished = self.feed.should_exit();
        self.label.clear();
        self.feed.describe(&mut self.label);
        if self.show_delay_until.is_some_and(|until| now < until)
            && let Some((total, extra)) = self.feed.delay_info()
        {
            use std::fmt::Write as _;
            let _ = write!(self.label, "\nsync delay {total} ms  (delay_ms = {extra})");
        }
    }

    fn adjust_delay(&mut self, delta_ms: i32) {
        self.feed.adjust_delay(delta_ms);
        self.delay_adjusted = true;
        self.show_delay_until =
            Some(Instant::now() + std::time::Duration::from_secs(DELAY_LABEL_SECS));
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
            .with_inner_size(self.window_size);
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
                Key::Character(c) if c == "[" => self.adjust_delay(-DELAY_STEP_MS),
                Key::Character(c) if c == "]" => self.adjust_delay(DELAY_STEP_MS),
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
                    balls: self.balls.balls(),
                    palette: self.palette,
                    time: self.started.elapsed().as_secs_f32(),
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

    let config = Config::load(args.config.as_deref())?;
    let feed = open_feed(&args, &config)?;
    let event_loop = EventLoop::new()?;
    let mut app = App::new(feed, &config.viz);
    event_loop.run_app(&mut app)?;
    if app.delay_adjusted
        && let Some((_, extra)) = app.feed.delay_info()
    {
        eprintln!(
            "sync delay adjusted; to keep it, put `delay_ms = {extra}` under {} in {}",
            app.feed.delay_config_section(),
            args.config
                .clone()
                .or_else(Config::default_path)
                .map_or("config.toml".into(), |p| p.display().to_string())
        );
    }
    match app.error {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// A seed for the balls' drift, different on every run.
fn seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64)
}

fn open_feed(args: &Args, config: &Config) -> Result<Feed> {
    let explicit_source = args.test || !args.files.is_empty() || args.source.is_some();
    if !(explicit_source || args.local) {
        let addr = SocketAddr::resolve(args.socket.as_deref(), config.socket.as_deref());
        match addr.connect() {
            Ok(conn) => return Ok(Feed::Remote(Remote::new(addr, conn))),
            Err(_) if args.connect => {
                eprintln!("waiting for av-server on {addr}");
                return Ok(Feed::Remote(Remote::waiting(addr)));
            }
            Err(_) => eprintln!("no av-server on {addr}, analyzing locally"),
        }
    }
    let device = args.source.as_deref().or(config.viz.source.as_deref());
    let source = Source::open(args.test, &args.files, args.looping, device)?;
    let mut engine = Engine::new(source);
    engine.set_extra_delay_ms(args.delay_ms.unwrap_or(config.viz.delay_ms));
    Ok(Feed::Local(Box::new(engine)))
}
