//! Where the visualizer's samples come from.

use std::fmt::Write as _;

use av_audio::Capture;

use crate::signal::TestSignal;

pub enum Input {
    Test {
        signal: TestSignal,
        /// Fractional samples carried between frames.
        debt: f32,
    },
    Capture(Capture),
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
        }
    }

    pub fn describe(&self, out: &mut String) {
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
        }
    }
}
