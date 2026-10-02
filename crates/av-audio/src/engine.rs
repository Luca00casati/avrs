//! A source of audio plus the analysis that turns it into band magnitudes.

use std::path::PathBuf;

use anyhow::Result;
use av_core::{Analyzer, DEFAULT_BANDS, DEFAULT_FFT_SIZE};

use crate::{Capture, Player, TestSignal, collect_playlist};

/// Where samples come from.
pub enum Source {
    Test(TestSignal),
    Capture(Capture),
    Player(Player),
}

impl Source {
    /// Opens the source a command line asked for: the test signal, files to
    /// play, or else a capture device (`None` = what the default output plays).
    pub fn open(
        test: bool,
        files: &[PathBuf],
        looping: bool,
        device: Option<&str>,
    ) -> Result<Self> {
        Ok(if test {
            Self::Test(TestSignal::new(48_000))
        } else if !files.is_empty() {
            Self::Player(Player::new(collect_playlist(files)?, looping)?)
        } else {
            Self::Capture(Capture::open(device)?)
        })
    }
}

/// Pulls samples from a [`Source`] and analyzes them.
pub struct Engine {
    source: Source,
    analyzer: Analyzer,
    samples: Vec<f32>,
    mags: Vec<f32>,
    paused: bool,
    /// Fractional test-signal samples carried between ticks.
    debt: f32,
}

impl Engine {
    pub fn new(source: Source) -> Self {
        Self {
            source,
            analyzer: Analyzer::new(DEFAULT_FFT_SIZE, DEFAULT_BANDS),
            samples: Vec::new(),
            mags: vec![0.0; DEFAULT_BANDS],
            paused: false,
            debt: 0.0,
        }
    }

    pub fn bands(&self) -> usize {
        self.mags.len()
    }

    /// Reads the samples for the last `dt` seconds and returns the band
    /// magnitudes of the most recent window.
    ///
    /// While paused the spectrum freezes; captured audio is still drained so
    /// its buffer doesn't overflow.
    pub fn tick(&mut self, dt: f32) -> &[f32] {
        self.samples.clear();
        match &mut self.source {
            Source::Test(signal) => {
                if !self.paused {
                    self.debt += dt * signal.sample_rate() as f32;
                    let count = self.debt as usize;
                    self.debt -= count as f32;
                    signal.generate(count, &mut self.samples);
                }
            }
            Source::Capture(capture) => {
                capture.drain(&mut self.samples);
                if self.paused {
                    self.samples.clear();
                }
            }
            // A paused player stops producing samples by itself.
            Source::Player(player) => player.drain(&mut self.samples),
        }
        self.analyzer.push(&self.samples);
        self.analyzer.analyze(&mut self.mags);
        &self.mags
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        if let Source::Player(player) = &self.source {
            player.set_paused(paused);
        }
    }

    pub fn has_playlist(&self) -> bool {
        matches!(self.source, Source::Player(_))
    }

    pub fn next(&mut self) {
        if let Source::Player(player) = &mut self.source {
            player.next();
        }
    }

    pub fn previous(&mut self) {
        if let Source::Player(player) = &mut self.source {
            player.previous();
        }
    }

    /// A non-looping playlist has played to the end.
    pub fn is_finished(&self) -> bool {
        matches!(&self.source, Source::Player(player) if player.is_finished())
    }

    /// What is playing or being captured, ready to display.
    pub fn title(&mut self) -> String {
        match &mut self.source {
            Source::Test(_) => "test signal".to_owned(),
            Source::Capture(capture) if capture.has_failed() => {
                format!("{}  (stream error)", capture.name())
            }
            Source::Capture(capture) => capture.name().to_owned(),
            Source::Player(player) if player.has_failed() => {
                format!("playing: {}  (output error)", player.title())
            }
            Source::Player(player) => format!("playing: {}", player.title()),
        }
    }
}
