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
use av_core::{Palette, Smoother, pool_max};
use av_proto::{Config, SocketAddr, VizConfig};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, WindowEvent};
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
/// How long the label and timeline stay after the last mouse or key activity.
const UI_SHOW: std::time::Duration = std::time::Duration::from_millis(2500);
/// How quickly they fade in and out (per second).
const UI_FADE_RATE: f32 = 8.0;
/// Seconds the seek keys move through a track.
const SEEK_STEP: f64 = 5.0;
/// How long after a seek further seeks count from its target rather than
/// from the reported position (which lags while the player gets there).
const SEEK_SETTLE: std::time::Duration = std::time::Duration::from_millis(600);
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
    /// Last frame's magnitudes, to tell when a paused source has gone still.
    prev_mags: Vec<f32>,
    palette: Palette,
    /// Seconds of animation shown so far; stands still while paused.
    anim_time: f32,
    window_size: LogicalSize<f64>,
    last_frame: Instant,
    label: String,
    finished: bool,
    /// Show the sync delay in the label until then.
    show_delay_until: Option<Instant>,
    delay_adjusted: bool,
    /// Show the label and timeline until then (after mouse or key activity).
    ui_until: Instant,
    /// Current visibility of the label and timeline, 0 to 1.
    ui_alpha: f32,
    last_ui_tick: Instant,
    /// Playback progress (0 to 1) for the timeline, when playing files.
    timeline: Option<f32>,
    /// Where on the timeline the user is dragging to, while the button is held.
    dragging: Option<f32>,
    /// Mouse position in window pixels, while over the window.
    cursor: Option<(f32, f32)>,
    /// The last seek target and when it was asked for: the player takes a
    /// moment to get there, so quick repeated seeks build on the target.
    pending_seek: Option<(f64, Instant)>,
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
            prev_mags: Vec::new(),
            palette: config.palette.parse().unwrap_or_default(),
            anim_time: 0.0,
            window_size: LogicalSize::new(f64::from(config.width), f64::from(config.height)),
            last_frame: Instant::now(),
            label: String::new(),
            finished: false,
            show_delay_until: None,
            delay_adjusted: false,
            ui_until: Instant::now(),
            ui_alpha: 0.0,
            last_ui_tick: Instant::now(),
            timeline: None,
            dragging: None,
            cursor: None,
            pending_seek: None,
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
        // Paused and the audio still in flight has played out: the peak caps
        // stay put and the bars drop to the floor. (Updating on the frozen
        // spectrum would keep the auto-gain and caps drifting.)
        let frozen = self.feed.is_paused() && self.mags == self.prev_mags;
        self.prev_mags.clone_from(&self.mags);
        if frozen {
            self.smoother.fall(dt);
            return self.update_label(now);
        }
        self.anim_time += dt;

        if !self.mags.is_empty() {
            pool_max(&self.mags, &mut self.bar_mags);
        } else {
            self.bar_mags.fill(0.0);
        }
        self.smoother.update(&self.bar_mags, dt);

        self.update_label(now);
    }

    /// Refreshes the label, timeline and their fade, and whether the window
    /// should close.
    fn update_label(&mut self, now: Instant) {
        let dt = now.duration_since(self.last_ui_tick).as_secs_f32();
        self.last_ui_tick = now;
        let shown = now < self.ui_until || self.dragging.is_some();
        let target = if shown { 1.0 } else { 0.0 };
        self.ui_alpha += (target - self.ui_alpha) * (dt * UI_FADE_RATE).min(1.0);

        self.finished = self.feed.should_exit();
        let (position, duration) = (self.feed.position(), self.feed.duration());
        self.timeline = match (position, duration) {
            (Some(p), Some(d)) if d > 0.0 => Some(self.dragging.unwrap_or((p / d) as f32)),
            _ => None,
        };
        self.label.clear();
        self.feed.describe(&mut self.label);
        if let (Some(p), Some(d)) = (position, duration) {
            use std::fmt::Write as _;
            let p = self.dragging.map_or(p, |f| f64::from(f) * d);
            let _ = write!(self.label, "\n{} / {}", clock(p), clock(d));
        }
        if self.show_delay_until.is_some_and(|until| now < until)
            && let Some((total, extra)) = self.feed.delay_info()
        {
            use std::fmt::Write as _;
            let _ = write!(self.label, "\nsync delay {total} ms  (delay_ms = {extra})");
        }
    }

    /// Shows the label and timeline for a while.
    fn wake_ui(&mut self) {
        self.ui_until = Instant::now() + UI_SHOW;
    }

    /// Moves `delta` seconds through the current track.
    fn seek_by(&mut self, delta: f64) {
        let pending = self
            .pending_seek
            .filter(|(_, at)| at.elapsed() < SEEK_SETTLE)
            .map(|(target, _)| target);
        if let Some(from) = pending.or_else(|| self.feed.position()) {
            let end = self.feed.duration().unwrap_or(f64::MAX);
            self.seek_to((from + delta).clamp(0.0, end));
        }
    }

    fn seek_to(&mut self, secs: f64) {
        self.feed.seek(secs);
        self.pending_seek = Some((secs, Instant::now()));
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
                        repeat,
                        ..
                    },
                ..
            } => {
                self.wake_ui();
                match logical_key {
                    // Seeking repeats while the key is held.
                    Key::Named(NamedKey::ArrowRight) => self.seek_by(SEEK_STEP),
                    Key::Named(NamedKey::ArrowLeft) => self.seek_by(-SEEK_STEP),
                    Key::Character(c) if c.eq_ignore_ascii_case("f") => self.seek_by(SEEK_STEP),
                    Key::Character(c) if c.eq_ignore_ascii_case("b") => self.seek_by(-SEEK_STEP),
                    _ if repeat => {}
                    Key::Named(NamedKey::Escape) => event_loop.exit(),
                    Key::Named(NamedKey::Space) => self.feed.toggle_pause(),
                    Key::Named(NamedKey::ArrowDown) => self.feed.next(),
                    Key::Named(NamedKey::ArrowUp) => self.feed.previous(),
                    Key::Character(c) if c.eq_ignore_ascii_case("n") => self.feed.next(),
                    Key::Character(c) if c.eq_ignore_ascii_case("p") => self.feed.previous(),
                    Key::Character(c) if c == "[" => self.adjust_delay(-DELAY_STEP_MS),
                    Key::Character(c) if c == "]" => self.adjust_delay(DELAY_STEP_MS),
                    _ => {}
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = (position.x as f32, position.y as f32);
                self.cursor = Some((x, y));
                self.wake_ui();
                if self.dragging.is_some()
                    && let Some(r) = &self.renderer
                {
                    // Follow the pointer along the bar even when it strays off it.
                    let [tx, _, tw, _] = r.timeline_rect();
                    self.dragging = Some(((x - tx) / tw).clamp(0.0, 1.0));
                }
            }
            WindowEvent::CursorLeft { .. } => self.cursor = None,
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => {
                self.wake_ui();
                match state {
                    ElementState::Pressed => {
                        if let (Some((x, y)), Some(r), Some(_)) =
                            (self.cursor, &self.renderer, self.timeline)
                        {
                            self.dragging = r.timeline_hit(x, y);
                        }
                    }
                    ElementState::Released => {
                        if let Some(fraction) = self.dragging.take()
                            && let Some(duration) = self.feed.duration()
                        {
                            self.seek_to(f64::from(fraction) * duration);
                        }
                    }
                }
            }
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
                    palette: self.palette,
                    time: self.anim_time,
                    label: &self.label,
                    ui_alpha: self.ui_alpha,
                    timeline: self.timeline,
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

/// Formats seconds as m:ss (or h:mm:ss).
fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
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

#[cfg(test)]
mod tests {
    use super::clock;

    #[test]
    fn clock_formats_minutes_and_hours() {
        assert_eq!(clock(0.0), "0:00");
        assert_eq!(clock(65.9), "1:05");
        assert_eq!(clock(3_725.0), "1:02:05");
        assert_eq!(clock(-3.0), "0:00");
    }
}
