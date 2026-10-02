//! Where the visualizer's band magnitudes come from.

use av_audio::Engine;
use av_proto::ClientMsg;

use crate::remote::Remote;

pub enum Feed {
    /// Audio analyzed in this process.
    Local(Box<Engine>),
    /// Spectra streamed by an av-server.
    Remote(Remote),
}

impl Feed {
    /// Writes the current band magnitudes into `out`.
    pub fn tick(&mut self, dt: f32, out: &mut Vec<f32>) {
        match self {
            Self::Local(engine) => {
                let mags = engine.tick(dt);
                out.clear();
                out.extend_from_slice(mags);
            }
            Self::Remote(remote) => remote.read_mags(out),
        }
    }

    pub fn toggle_pause(&mut self) {
        match self {
            Self::Local(engine) => engine.set_paused(!engine.is_paused()),
            Self::Remote(remote) => remote.send(ClientMsg::SetPaused(!remote.status().paused)),
        }
    }

    pub fn next(&mut self) {
        match self {
            Self::Local(engine) => engine.next(),
            Self::Remote(remote) => remote.send(ClientMsg::Next),
        }
    }

    pub fn previous(&mut self) {
        match self {
            Self::Local(engine) => engine.previous(),
            Self::Remote(remote) => remote.send(ClientMsg::Previous),
        }
    }

    /// A local playlist has ended; the window closes like the C version.
    /// A server keeps running, so remote clients stay open.
    pub fn should_exit(&self) -> bool {
        matches!(self, Self::Local(engine) if engine.is_finished())
    }

    pub fn describe(&mut self, out: &mut String) {
        match self {
            Self::Local(engine) => {
                out.push_str(&engine.title());
                if engine.is_paused() {
                    out.push_str("  (paused)");
                }
            }
            Self::Remote(remote) => remote.describe(out),
        }
    }
}
