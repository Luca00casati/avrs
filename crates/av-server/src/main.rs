//! av-server: plays or captures audio, analyzes it, and streams spectra to
//! any number of clients over a local socket.

use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use av_audio::{Engine, Source};
use av_proto::{ClientMsg, PROTOCOL_VERSION, ServerMsg, SocketAddr, Status, encode, read_msg};
use interprocess::local_socket::{ListenerOptions, Stream, prelude::*};

/// Spectrum analysis server.
#[derive(clap::Parser)]
#[command(version)]
struct Args {
    /// Audio files or directories to play. Without any, captures audio instead.
    #[arg(conflicts_with_all = ["source", "test"])]
    files: Vec<PathBuf>,

    /// Repeat the playlist forever.
    #[arg(short, long = "loop", requires = "files")]
    looping: bool,

    /// Audio source to capture, by id or part of its name.
    /// Defaults to what the default output device is playing.
    #[arg(short, long, conflicts_with = "test")]
    source: Option<String>,

    /// List capturable audio sources and exit.
    #[arg(long)]
    sources: bool,

    /// Use a built-in test sweep instead of real audio.
    #[arg(long)]
    test: bool,

    /// Socket to listen on (a path; a pipe name on Windows).
    /// Defaults to $AVRS_SOCKET, else a per-user location.
    #[arg(long)]
    socket: Option<String>,

    /// Spectrum frames per second.
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u32).range(1..=240))]
    rate: u32,
}

/// Frames queued per client before new ones are dropped for it.
const CLIENT_QUEUE: usize = 8;

type Frame = Arc<[u8]>;

enum Event {
    Joined(SyncSender<Frame>),
    Command(ClientMsg),
}

fn main() -> Result<()> {
    let args: Args = clap::Parser::parse();
    if args.sources {
        for source in av_audio::list_sources()? {
            println!("{source}");
        }
        return Ok(());
    }

    let addr = SocketAddr::resolve(args.socket.as_deref());
    let listener = listen(&addr)?;
    let source = Source::open(args.test, &args.files, args.looping, args.source.as_deref())?;
    let mut engine = Engine::new(source);

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || running.store(false, Ordering::Relaxed))
            .context("failed to install signal handler")?;
    }

    let (events_tx, events) = mpsc::channel();
    thread::Builder::new()
        .name("av-accept".into())
        .spawn(move || accept_loop(listener, events_tx))?;

    eprintln!("av-server listening on {addr}");
    let result = serve(&mut engine, &events, args.rate, &running);
    addr.remove();
    result
}

/// Binds the socket, replacing a stale one left by a crashed server.
fn listen(addr: &SocketAddr) -> Result<interprocess::local_socket::Listener> {
    let create = |overwrite: bool| {
        ListenerOptions::new()
            .name(addr.name()?)
            .reclaim_name(false)
            .try_overwrite(overwrite)
            .create_sync()
    };
    match create(false) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if addr.connect().is_ok() {
                bail!("another av-server is already running on {addr}");
            }
            create(true).with_context(|| format!("cannot listen on {addr}"))
        }
        Err(e) => Err(e).with_context(|| format!("cannot listen on {addr}")),
    }
}

fn accept_loop(listener: interprocess::local_socket::Listener, events: Sender<Event>) {
    for conn in listener.incoming() {
        match conn {
            Ok(conn) => {
                if spawn_client(conn, &events).is_err() {
                    return; // server is shutting down
                }
            }
            Err(e) => eprintln!("connection failed: {e}"),
        }
    }
}

/// Starts the reader and writer threads for one client.
fn spawn_client(conn: Stream, events: &Sender<Event>) -> Result<(), mpsc::SendError<Event>> {
    let (mut recv, mut send) = conn.split();
    let (frames_tx, frames) = mpsc::sync_channel::<Frame>(CLIENT_QUEUE);

    // Writer: exits when the client goes away or the server drops its queue.
    let _ = thread::Builder::new()
        .name("av-client-tx".into())
        .spawn(move || {
            for frame in frames {
                if send.write_all(&frame).and_then(|()| send.flush()).is_err() {
                    break;
                }
            }
        });

    // Reader: forwards commands until the client disconnects.
    let commands = events.clone();
    let _ = thread::Builder::new()
        .name("av-client-rx".into())
        .spawn(move || {
            let mut buf = Vec::new();
            while let Ok(msg) = read_msg::<_, ClientMsg>(&mut recv, &mut buf) {
                if commands.send(Event::Command(msg)).is_err() {
                    break;
                }
            }
        });

    events.send(Event::Joined(frames_tx))
}

/// Runs the analysis loop until interrupted.
fn serve(
    engine: &mut Engine,
    events: &Receiver<Event>,
    rate: u32,
    running: &AtomicBool,
) -> Result<()> {
    let period = Duration::from_secs_f64(1.0 / f64::from(rate));
    let hello: Frame = encode(&ServerMsg::Hello {
        version: PROTOCOL_VERSION,
        bands: engine.bands(),
        rate,
    })?
    .into();

    let mut clients: Vec<SyncSender<Frame>> = Vec::new();
    let mut status = current_status(engine);
    let mut status_frame: Frame = encode(&ServerMsg::Status(status.clone()))?.into();
    let mut seq = 0u64;
    let mut last = Instant::now();
    let mut next_tick = last + period;

    while running.load(Ordering::Relaxed) {
        // Handle events until the next tick is due.
        loop {
            let wait = next_tick.saturating_duration_since(Instant::now());
            match events.recv_timeout(wait) {
                Ok(Event::Joined(client)) => {
                    // Hello and the current status go out before any spectrum.
                    if client.try_send(hello.clone()).is_ok()
                        && client.try_send(status_frame.clone()).is_ok()
                    {
                        clients.push(client);
                        eprintln!("client connected ({} total)", clients.len());
                    }
                }
                Ok(Event::Command(cmd)) => match cmd {
                    ClientMsg::SetPaused(paused) => engine.set_paused(paused),
                    ClientMsg::Next => engine.next(),
                    ClientMsg::Previous => engine.previous(),
                },
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => bail!("accept thread stopped"),
            }
        }

        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f32();
        last = now;
        // Don't try to catch up after a stall; just keep the cadence.
        next_tick = (next_tick + period).max(now);

        let mags = engine.tick(dt);
        seq += 1;
        let spectrum: Frame = encode(&ServerMsg::Spectrum {
            seq,
            mags: mags.to_vec(),
        })?
        .into();

        let new_status = current_status(engine);
        // Also resend once a second, in case a full queue dropped a change.
        let send_status = new_status != status || seq % u64::from(rate) == 0;
        if new_status != status {
            status = new_status;
            status_frame = encode(&ServerMsg::Status(status.clone()))?.into();
        }

        let before = clients.len();
        clients.retain(|client| {
            if send_status && !deliver(client, &status_frame) {
                return false;
            }
            deliver(client, &spectrum)
        });
        if clients.len() < before {
            eprintln!("client disconnected ({} total)", clients.len());
        }
    }
    eprintln!("av-server shutting down");
    Ok(())
}

/// Queues a frame for a client. A full queue drops the frame (the client is
/// slow); returns `false` only if the client is gone.
fn deliver(client: &SyncSender<Frame>, frame: &Frame) -> bool {
    !matches!(
        client.try_send(frame.clone()),
        Err(TrySendError::Disconnected(_))
    )
}

fn current_status(engine: &mut Engine) -> Status {
    Status {
        title: engine.title(),
        paused: engine.is_paused(),
        has_playlist: engine.has_playlist(),
        finished: engine.is_finished(),
    }
}
