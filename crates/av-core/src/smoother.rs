/// Turns raw band magnitudes into bar and peak heights for drawing.
///
/// Heights are normalized so 1.0 is the full window height. An automatic gain
/// keeps the loudest band near [`HEADROOM`](Self::HEADROOM): it rises quickly
/// on transients and falls slowly, so bars stay inside the window (heights are
/// also capped at 1.0).
///
/// The motion is "punchy": bars jump up almost at once on a hit and fall back
/// under gravity, and each peak marker hangs for a moment before dropping. All
/// rates are scaled by `dt`, so the look does not depend on the frame rate.
#[derive(Debug, Clone)]
pub struct Smoother {
    avg_max: f32,
    /// Bar heights before the neighbour blend.
    heights: Vec<f32>,
    /// Downward speed of each falling bar, in heights per second.
    fall_speed: Vec<f32>,
    bars: Vec<f32>,
    peaks: Vec<f32>,
    /// Seconds each peak marker still hangs before falling.
    peak_hold: Vec<f32>,
    peak_speed: Vec<f32>,
}

impl Smoother {
    /// How fast the auto-gain rises to a louder band.
    const GAIN_ATTACK: f32 = 0.5;
    /// How fast the auto-gain relaxes when things get quieter.
    const GAIN_RELEASE: f32 = 0.02;
    /// Height the loudest band settles at, leaving room for peaks above.
    pub const HEADROOM: f32 = 0.85;
    /// How fast a bar rises to a louder level (per 60 Hz frame).
    const RISE_RATE: f32 = 0.7;
    /// Gravity on falling bars, in heights per second squared.
    const BAR_GRAVITY: f32 = 3.4;
    /// How long a peak marker hangs before it starts to fall, in seconds.
    const PEAK_HOLD: f32 = 0.32;
    /// Gravity on falling peak markers, in heights per second squared.
    const PEAK_GRAVITY: f32 = 1.5;
    /// Weight of each neighbour in the blend that softens jagged edges.
    const NEIGHBOUR: f32 = 0.2;
    /// Floor for the auto-gain, so near-silence is not amplified to full height.
    /// Roughly the C version's floor of 1.0 in its unnormalized units.
    const MIN_LEVEL: f32 = 0.03;

    pub fn new(band_count: usize) -> Self {
        Self {
            avg_max: Self::MIN_LEVEL,
            heights: vec![0.0; band_count],
            fall_speed: vec![0.0; band_count],
            bars: vec![0.0; band_count],
            peaks: vec![0.0; band_count],
            peak_hold: vec![0.0; band_count],
            peak_speed: vec![0.0; band_count],
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
        let gain_rate = if max > self.avg_max {
            Self::GAIN_ATTACK
        } else {
            Self::GAIN_RELEASE
        };
        self.avg_max = lerp(self.avg_max, max, rate(gain_rate, dt));
        let gain = Self::HEADROOM / self.avg_max;

        // Rise fast towards louder targets; fall under gravity otherwise.
        let rise = rate(Self::RISE_RATE, dt);
        for ((h, v), &m) in self.heights.iter_mut().zip(&mut self.fall_speed).zip(mags) {
            let target = (m * gain).min(1.0);
            if target >= *h {
                *h = lerp(*h, target, rise);
                *v = 0.0;
            } else {
                *v += Self::BAR_GRAVITY * dt;
                *h = (*h - *v * dt).max(target);
            }
        }

        let side = Self::NEIGHBOUR;
        for i in 0..n {
            let left = self.heights[i.saturating_sub(1)];
            let right = self.heights[(i + 1).min(n - 1)];
            self.bars[i] = side * left + (1.0 - 2.0 * side) * self.heights[i] + side * right;
        }

        for i in 0..n {
            let bar = self.bars[i];
            if bar >= self.peaks[i] {
                self.peaks[i] = bar;
                self.peak_hold[i] = Self::PEAK_HOLD;
                self.peak_speed[i] = 0.0;
            } else if self.peak_hold[i] > 0.0 {
                self.peak_hold[i] -= dt;
            } else {
                self.peak_speed[i] += Self::PEAK_GRAVITY * dt;
                self.peaks[i] = (self.peaks[i] - self.peak_speed[i] * dt).max(bar);
            }
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
            s.bars()
                .iter()
                .all(|&b| (b - Smoother::HEADROOM).abs() < 0.05),
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
    fn transients_stay_inside_the_window() {
        let mut s = Smoother::new(4);
        for _ in 0..300 {
            s.update(&[0.05; 4], DT);
        }
        // A sudden jump to 20x louder.
        for _ in 0..60 {
            s.update(&[1.0; 4], DT);
            assert!(s.bars().iter().chain(s.peaks()).all(|&h| h <= 1.0));
        }
        // And the gain settles back to the headroom level.
        assert!((s.bars()[0] - Smoother::HEADROOM).abs() < 0.05);
    }

    #[test]
    fn hits_jump_up_and_fall_with_gravity() {
        let mut s = Smoother::new(1);
        for _ in 0..120 {
            s.update(&[0.05], DT);
        }
        // A hit gets most of the way up in two frames.
        s.update(&[0.5], DT);
        s.update(&[0.5], DT);
        let top = s.bars()[0];
        assert!(top > 0.8, "rose only to {top}");
        // Then it falls slowly at first and faster later (gravity).
        let mut heights = vec![top];
        for _ in 0..12 {
            s.update(&[0.0], DT);
            heights.push(s.bars()[0]);
        }
        let first = heights[0] - heights[1];
        let later = heights[10] - heights[11];
        assert!(later > first * 2.0, "{heights:?}");
    }

    #[test]
    fn peaks_hang_before_falling() {
        let mut s = Smoother::new(1);
        for _ in 0..60 {
            s.update(&[0.5], DT);
        }
        let peak = s.peaks()[0];
        // Within the hold time the marker stays put while the bar drops.
        for _ in 0..(Smoother::PEAK_HOLD * 60.0) as usize - 2 {
            s.update(&[0.0], DT);
        }
        assert_eq!(s.peaks()[0], peak);
        assert!(s.bars()[0] < peak - 0.1, "bar should be falling away");
        for _ in 0..30 {
            s.update(&[0.0], DT);
        }
        assert!(s.peaks()[0] < peak);
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
