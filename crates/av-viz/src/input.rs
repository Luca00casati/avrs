//! Where the visualizer's samples come from.

use std::fmt::Write as _;

use av_audio::{Capture, Player};

use crate::signal::TestSignal;

pub enum Input {
    Test {
        signal: TestSignal,
        /// Fractional samples carried between frames.
        debt: f32,
    },
    Capture(Capture),
    Player(Player),
}

impl Input {
    pub fn test() -> Self {
        Self::Test {
            signal: TestSignal::new(48_000),
            debt: 0.0,
        }
    }

    /// Appends the mono samples for the last `dt` seconds to `out`.
    ///
    /// While paused, captured audio is still drained (so the buffer doesn't
    /// overflow) but discarded, freezing the spectrum like the C version.
    pub fn read(&mut self, dt: f32, paused: bool, out: &mut Vec<f32>) {
        match self {
            Self::Test { signal, debt } => {
                if paused {
                    return;
                }
                *debt += dt * signal.sample_rate() as f32;
                let count = *debt as usize;
                *debt -= count as f32;
                signal.generate(count, out);
            }
            Self::Capture(capture) => {
                let start = out.len();
                capture.drain(out);
                if paused {
                    out.truncate(start);
                }
            }
            // A paused player stops producing samples by itself.
            Self::Player(player) => player.drain(out),
        }
    }

    pub fn set_paused(&mut self, paused: bool) {
        if let Self::Player(player) = self {
            player.set_paused(paused);
        }
    }

    pub fn next(&mut self) {
        if let Self::Player(player) = self {
            player.next();
        }
    }

    pub fn previous(&mut self) {
        if let Self::Player(player) = self {
            player.previous();
        }
    }

    /// The playlist has ended; the visualizer should close, like the C version.
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Player(player) if player.is_finished())
    }

    pub fn describe(&mut self, out: &mut String) {
        match self {
            Self::Test { signal, .. } => {
                let _ = write!(out, "test signal: {:.0} Hz", signal.frequency());
            }
            Self::Capture(capture) => {
                out.push_str(capture.name());
                if capture.has_failed() {
                    out.push_str("  (stream error)");
                }
            }
            Self::Player(player) => {
                let _ = write!(out, "playing: {}", player.title());
                if player.has_failed() {
                    out.push_str("  (output error)");
                }
            }
        }
    }
}
