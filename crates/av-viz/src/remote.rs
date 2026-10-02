//! Client side of the av-server connection.
//!
//! A background thread keeps the connection alive, reconnecting whenever the
//! server goes away, and stores the latest spectrum and status for the UI.

use std::io;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use av_proto::{ClientMsg, PROTOCOL_VERSION, ServerMsg, SocketAddr, Status, read_msg, write_msg};
use interprocess::local_socket::{RecvHalf, SendHalf, Stream, prelude::*};

const RETRY: Duration = Duration::from_millis(500);

pub struct Remote {
    state: Arc<Mutex<State>>,
    sender: Arc<Mutex<Option<SendHalf>>>,
}

#[derive(Default)]
struct State {
    connected: bool,
    mags: Vec<f32>,
    status: Status,
    /// Why the last connection failed, if it did.
    problem: Option<String>,
}

impl Remote {
    /// Takes over an established connection and keeps it alive.
    pub fn new(addr: SocketAddr, first: Stream) -> Self {
        Self::start(addr, Some(first))
    }

    /// Keeps trying to connect until a server appears.
    pub fn waiting(addr: SocketAddr) -> Self {
        Self::start(addr, None)
    }

    fn start(addr: SocketAddr, first: Option<Stream>) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let sender = Arc::new(Mutex::new(None));
        {
            let (state, sender) = (state.clone(), sender.clone());
            thread::Builder::new()
                .name("av-remote".into())
                .spawn(move || run(addr, first, &state, &sender))
                .expect("failed to spawn connection thread");
        }
        Self { state, sender }
    }

    /// Copies the latest band magnitudes into `out`; silence while disconnected.
    pub fn read_mags(&self, out: &mut Vec<f32>) {
        let state = self.state.lock().unwrap();
        out.clear();
        if state.connected {
            out.extend_from_slice(&state.mags);
        } else {
            out.resize(state.mags.len(), 0.0);
        }
    }

    pub fn status(&self) -> Status {
        self.state.lock().unwrap().status.clone()
    }

    /// Describes the connection for the label.
    pub fn describe(&self, out: &mut String) {
        let state = self.state.lock().unwrap();
        if state.connected {
            out.push_str(&state.status.title);
            if state.status.finished {
                out.push_str("  (finished)");
            } else if state.status.paused {
                out.push_str("  (paused)");
            }
        } else {
            out.push_str("server disconnected, reconnecting…");
            if let Some(problem) = &state.problem {
                out.push_str("  (");
                out.push_str(problem);
                out.push(')');
            }
        }
    }

    pub fn send(&self, msg: ClientMsg) {
        if let Some(send) = self.sender.lock().unwrap().as_mut() {
            // A failed write shows up as a disconnect on the reading side.
            let _ = write_msg(send, &msg);
        }
    }
}

fn run(
    addr: SocketAddr,
    mut conn: Option<Stream>,
    state: &Mutex<State>,
    sender: &Mutex<Option<SendHalf>>,
) {
    loop {
        let stream = match conn.take().map_or_else(|| addr.connect(), Ok) {
            Ok(stream) => stream,
            Err(_) => {
                thread::sleep(RETRY);
                continue;
            }
        };
        let (mut recv, send) = stream.split();
        let result = session(&mut recv, send, state, sender);

        *sender.lock().unwrap() = None;
        let mut s = state.lock().unwrap();
        s.connected = false;
        s.problem = match result {
            Err(e) if e.kind() == io::ErrorKind::Unsupported => Some(e.to_string()),
            _ => None,
        };
        drop(s);
        thread::sleep(RETRY);
    }
}

/// Handles one connection until it drops.
fn session(
    recv: &mut RecvHalf,
    send: SendHalf,
    state: &Mutex<State>,
    sender: &Mutex<Option<SendHalf>>,
) -> io::Result<()> {
    let mut buf = Vec::new();
    match read_msg(recv, &mut buf)? {
        ServerMsg::Hello { version, bands, .. } if version == PROTOCOL_VERSION => {
            let mut s = state.lock().unwrap();
            s.mags = vec![0.0; bands];
            s.connected = true;
            s.problem = None;
        }
        ServerMsg::Hello { version, .. } => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("server speaks protocol v{version}, expected v{PROTOCOL_VERSION}"),
            ));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "server did not say hello",
            ));
        }
    }
    *sender.lock().unwrap() = Some(send);

    loop {
        match read_msg(recv, &mut buf)? {
            ServerMsg::Spectrum { mags, .. } => state.lock().unwrap().mags = mags,
            ServerMsg::Status(status) => state.lock().unwrap().status = status,
            ServerMsg::Hello { .. } => {}
        }
    }
}
