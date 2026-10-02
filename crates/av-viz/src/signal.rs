//! Synthetic audio for exercising the visualizer without any audio backend.

use std::f32::consts::TAU;

/// A logarithmic sine sweep plus a quieter octave-up harmonic, looping forever.
pub struct TestSignal {
    sample_rate: f32,
    phase: f32,
    harmonic_phase: f32,
    t: f32,
}

impl TestSignal {
    const LOW_HZ: f32 = 40.0;
    const HIGH_HZ: f32 = 16_000.0;
    const SWEEP_SECS: f32 = 12.0;

    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate as f32,
            phase: 0.0,
            harmonic_phase: 0.0,
            t: 0.0,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate as u32
    }

    /// Current fundamental frequency in Hz.
    pub fn frequency(&self) -> f32 {
        let pos = (self.t / Self::SWEEP_SECS).fract();
        Self::LOW_HZ * (Self::HIGH_HZ / Self::LOW_HZ).powf(pos)
    }

    /// Appends `count` mono samples to `out`.
    pub fn generate(&mut self, count: usize, out: &mut Vec<f32>) {
        let dt = 1.0 / self.sample_rate;
        out.extend((0..count).map(|_| {
            let f = self.frequency();
            self.phase = (self.phase + TAU * f * dt) % TAU;
            self.harmonic_phase = (self.harmonic_phase + TAU * 2.0 * f * dt) % TAU;
            self.t += dt;
            0.6 * self.phase.sin() + 0.2 * self.harmonic_phase.sin()
        }));
    }
}
