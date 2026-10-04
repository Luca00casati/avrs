//! A source of audio plus the analysis that turns it into band magnitudes.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

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
    /// Holds samples back until they are actually heard.
    delay_line: DelayLine,
    /// User adjustment on top of the measured latency, in seconds.
    extra_delay: f64,
    /// Smoothed total delay currently applied, in seconds.
    delay: f64,
    /// Apply the next target delay immediately instead of smoothing.
    snap_delay: bool,
    /// The capture's switch count last seen, to snap the delay on a change.
    seen_switches: u64,
}

impl Engine {
    /// Longest delay applied, whatever the latency reports say.
    const MAX_DELAY: f64 = 2.0;
    /// Largest user adjustment either way, in ms.
    pub const MAX_EXTRA_MS: i32 = 2000;
    /// Per-tick smoothing of the measured latency, which jitters.
    const DELAY_SMOOTHING: f64 = 0.05;

    pub fn new(source: Source) -> Self {
        Self {
            source,
            analyzer: Analyzer::new(DEFAULT_FFT_SIZE, DEFAULT_BANDS),
            samples: Vec::new(),
            mags: vec![0.0; DEFAULT_BANDS],
            paused: false,
            debt: 0.0,
            delay_line: DelayLine::default(),
            extra_delay: 0.0,
            delay: 0.0,
            snap_delay: true,
            seen_switches: 0,
        }
    }

    /// Adds `ms` milliseconds (negative to subtract) to the automatically
    /// measured output latency, for outputs that misreport it.
    pub fn set_extra_delay_ms(&mut self, ms: i32) {
        let ms = ms.clamp(-Self::MAX_EXTRA_MS, Self::MAX_EXTRA_MS);
        self.extra_delay = f64::from(ms) / 1000.0;
    }

    /// The user's adjustment, as set by [`set_extra_delay_ms`](Self::set_extra_delay_ms).
    pub fn extra_delay_ms(&self) -> i32 {
        (self.extra_delay * 1000.0).round() as i32
    }

    /// Shifts the user's adjustment by `delta_ms`. The applied delay follows
    /// right away rather than gliding there.
    pub fn adjust_extra_delay_ms(&mut self, delta_ms: i32) {
        self.set_extra_delay_ms(self.extra_delay_ms().saturating_add(delta_ms));
        self.snap_delay = true;
    }

    /// The delay currently applied between capturing audio and showing it.
    pub fn delay(&self) -> Duration {
        Duration::from_secs_f64(self.delay)
    }

    /// The source's own estimate of how late its audio is heard.
    fn source_latency(&self) -> f64 {
        match &self.source {
            Source::Test(_) => 0.0,
            Source::Capture(capture) => capture.output_latency().as_secs_f64(),
            Source::Player(player) => player.output_latency().as_secs_f64(),
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

        if let Source::Capture(capture) = &self.source
            && capture.switches() != self.seen_switches
        {
            // A different device: its latency applies at once.
            self.seen_switches = capture.switches();
            self.snap_delay = true;
        }
        let target = (self.source_latency() + self.extra_delay).clamp(0.0, Self::MAX_DELAY);
        if self.snap_delay || (target - self.delay).abs() > 0.5 {
            // First reading, a user adjustment, or a device change: jump.
            self.delay = target;
            self.snap_delay = false;
        } else {
            self.delay += (target - self.delay) * Self::DELAY_SMOOTHING;
        }
        self.delay_line.push(f64::from(dt), &self.samples);
        self.samples.clear();
        self.delay_line.release(self.delay, &mut self.samples);

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

/// Releases each chunk of samples `delay` seconds after it arrived.
///
/// Timing by arrival (not by count) keeps pauses right: after the source
/// stops, what is still in flight keeps draining for `delay`, just as the
/// listener keeps hearing it.
#[derive(Default)]
struct DelayLine {
    chunks: VecDeque<(f64, Vec<f32>)>,
    clock: f64,
}

impl DelayLine {
    /// Advances the clock by `dt` seconds and records `samples` as arriving now.
    fn push(&mut self, dt: f64, samples: &[f32]) {
        self.clock += dt;
        if !samples.is_empty() {
            self.chunks.push_back((self.clock, samples.to_vec()));
        }
    }

    /// Appends every chunk that arrived at least `delay` seconds ago.
    fn release(&mut self, delay: f64, out: &mut Vec<f32>) {
        while let Some((arrived, _)) = self.chunks.front() {
            if arrived + delay > self.clock + 1e-9 {
                break;
            }
            let (_, chunk) = self.chunks.pop_front().expect("checked front");
            out.extend_from_slice(&chunk);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f64 = 1.0 / 60.0;

    fn tick(line: &mut DelayLine, input: &[f32], delay: f64) -> Vec<f32> {
        let mut out = Vec::new();
        line.push(DT, input);
        line.release(delay, &mut out);
        out
    }

    #[test]
    fn zero_delay_passes_straight_through() {
        let mut line = DelayLine::default();
        assert_eq!(tick(&mut line, &[1.0, 2.0], 0.0), [1.0, 2.0]);
        assert_eq!(tick(&mut line, &[3.0], 0.0), [3.0]);
    }

    #[test]
    fn holds_samples_for_the_delay() {
        let mut line = DelayLine::default();
        let delay = 10.0 * DT;
        let mut released = Vec::new();
        for i in 0..30 {
            let out = tick(&mut line, &[i as f32], delay);
            released.push((i, out));
        }
        // Nothing for the first 10 ticks, then each sample 10 ticks late.
        for (i, out) in &released {
            if *i < 10 {
                assert!(out.is_empty(), "tick {i} released {out:?}");
            } else {
                assert_eq!(out, &[(*i - 10) as f32], "tick {i}");
            }
        }
    }

    #[test]
    fn keeps_draining_after_input_stops() {
        let mut line = DelayLine::default();
        let delay = 5.0 * DT;
        for i in 0..20 {
            tick(&mut line, &[i as f32], delay);
        }
        // Source paused: the last 5 chunks still come out, one per tick.
        let tail: Vec<f32> = (0..8).flat_map(|_| tick(&mut line, &[], delay)).collect();
        assert_eq!(tail, [15.0, 16.0, 17.0, 18.0, 19.0]);
    }

    #[test]
    fn shorter_delay_releases_the_backlog() {
        let mut line = DelayLine::default();
        for i in 0..10 {
            tick(&mut line, &[i as f32], 1.0);
        }
        assert_eq!(tick(&mut line, &[10.0], 0.0).len(), 11);
    }
}
