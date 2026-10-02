/// Turns raw band magnitudes into bar and peak heights for drawing.
///
/// Heights are normalized so 1.0 is the full window height; they can briefly
/// exceed 1.0 on transients, as in the original C version. Smoothing rates are
/// expressed per 60 Hz frame and scaled by `dt`, so the look does not depend
/// on the frame rate.
#[derive(Debug, Clone)]
pub struct Smoother {
    avg_max: f32,
    smoothed: Vec<f32>,
    bars: Vec<f32>,
    peaks: Vec<f32>,
}

impl Smoother {
    /// How fast the auto-gain follows the loudest band.
    const GAIN_RATE: f32 = 0.1;
    /// How fast bars follow their target height.
    const BAR_RATE: f32 = 0.2;
    /// How fast peak markers fall back to the bars.
    const PEAK_FALL_RATE: f32 = 0.05;
    /// Floor for the auto-gain, so near-silence is not amplified to full height.
    /// Roughly the C version's floor of 1.0 in its unnormalized units.
    const MIN_LEVEL: f32 = 0.03;

    pub fn new(band_count: usize) -> Self {
        Self {
            avg_max: Self::MIN_LEVEL,
            smoothed: vec![0.0; band_count],
            bars: vec![0.0; band_count],
            peaks: vec![0.0; band_count],
        }
    }

    pub fn bars(&self) -> &[f32] {
        &self.bars
    }

    pub fn peaks(&self) -> &[f32] {
        &self.peaks
    }

    /// Advances by `dt` seconds towards the given band magnitudes.
    pub fn update(&mut self, mags: &[f32], dt: f32) {
        assert_eq!(
            mags.len(),
            self.bars.len(),
            "magnitude count must equal band count"
        );
        let n = mags.len();

        let max = mags.iter().copied().fold(Self::MIN_LEVEL, f32::max);
        self.avg_max = lerp(self.avg_max, max, rate(Self::GAIN_RATE, dt));

        let bar_rate = rate(Self::BAR_RATE, dt);
        for (s, &m) in self.smoothed.iter_mut().zip(mags) {
            *s = lerp(*s, m / self.avg_max, bar_rate);
        }

        // Three-tap blur across neighbouring bands.
        for i in 0..n {
            let left = self.smoothed[i.saturating_sub(1)];
            let right = self.smoothed[(i + 1).min(n - 1)];
            self.bars[i] = (left + self.smoothed[i] + right) / 3.0;
        }

        let peak_rate = rate(Self::PEAK_FALL_RATE, dt);
        for (p, &b) in self.peaks.iter_mut().zip(&self.bars) {
            *p = if b > *p { b } else { lerp(*p, b, peak_rate) };
        }
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Converts a per-60Hz-frame lerp factor into one for a step of `dt` seconds.
fn rate(per_frame: f32, dt: f32) -> f32 {
    1.0 - (1.0 - per_frame).powf(dt * 60.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    #[test]
    fn rises_towards_signal_and_peaks_fall() {
        let mut s = Smoother::new(8);
        let loud = [0.5; 8];
        for _ in 0..120 {
            s.update(&loud, DT);
        }
        assert!(
            s.bars().iter().all(|&b| (b - 1.0).abs() < 0.05),
            "{:?}",
            s.bars()
        );
        assert!(s.peaks().iter().zip(s.bars()).all(|(p, b)| p >= b));

        let quiet = [0.0; 8];
        s.update(&quiet, DT);
        let peak_after_one = s.peaks()[0];
        assert!(
            peak_after_one > s.bars()[0],
            "peak should lag behind the bar"
        );
        for _ in 0..600 {
            s.update(&quiet, DT);
        }
        assert!(s.peaks()[0] < 0.01);
    }

    #[test]
    fn frame_rate_independent() {
        let mags = [0.3, 0.1, 0.2, 0.05];
        let mut at60 = Smoother::new(4);
        let mut at120 = Smoother::new(4);
        for _ in 0..30 {
            at60.update(&mags, DT);
        }
        for _ in 0..60 {
            at120.update(&mags, DT / 2.0);
        }
        // Not identical (the blur and peak steps are per frame) but close.
        for (a, b) in at60.bars().iter().zip(at120.bars()) {
            assert!((a - b).abs() < 0.05, "{a} vs {b}");
        }
    }

    #[test]
    fn silence_stays_flat() {
        let mut s = Smoother::new(4);
        for _ in 0..60 {
            s.update(&[0.0; 4], DT);
        }
        assert!(s.bars().iter().all(|&b| b == 0.0));
    }
}
