use std::ops::Range;
use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{RealFftPlanner, RealToComplex};

/// Turns a stream of mono samples into log-spaced band magnitudes.
///
/// Samples are pushed into a sliding window of the last `fft_size` samples, so
/// [`analyze`](Self::analyze) can run at any rate independent of the FFT length.
pub struct Analyzer {
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    history: Vec<f32>,
    write_pos: usize,
    input: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    bands: Vec<Range<usize>>,
    scale: f32,
}

impl Analyzer {
    pub fn new(fft_size: usize, band_count: usize) -> Self {
        assert!(
            fft_size >= 8 && fft_size.is_power_of_two(),
            "fft_size must be a power of two >= 8"
        );
        assert!(band_count > 0, "band_count must be > 0");

        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(fft_size);
        let window = hann(fft_size);
        // Scale so a full-scale sine centred on a bin has magnitude 1.
        let scale = 2.0 / window.iter().sum::<f32>();

        Self {
            input: fft.make_input_vec(),
            spectrum: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            fft,
            window,
            history: vec![0.0; fft_size],
            write_pos: 0,
            bands: log_bands(fft_size, band_count),
            scale,
        }
    }

    pub fn fft_size(&self) -> usize {
        self.history.len()
    }

    pub fn band_count(&self) -> usize {
        self.bands.len()
    }

    /// FFT bin range covered by each band.
    pub fn band_ranges(&self) -> &[Range<usize>] {
        &self.bands
    }

    /// Appends mono samples to the sliding window.
    pub fn push(&mut self, samples: &[f32]) {
        let n = self.history.len();
        // Only the last `n` samples can matter.
        let samples = &samples[samples.len().saturating_sub(n)..];
        for &s in samples {
            self.history[self.write_pos] = s;
            self.write_pos = (self.write_pos + 1) % n;
        }
    }

    /// Clears the sliding window to silence.
    pub fn reset(&mut self) {
        self.history.fill(0.0);
        self.write_pos = 0;
    }

    /// Analyzes the current window, writing one magnitude per band into `out`.
    ///
    /// Each band is the mean of `sqrt(|X[k]|)` over its bins; the square root
    /// compresses the dynamic range the same way the original C version did.
    pub fn analyze(&mut self, out: &mut [f32]) {
        assert_eq!(
            out.len(),
            self.bands.len(),
            "output length must equal band count"
        );

        let (newer, older) = self.history.split_at(self.write_pos);
        for ((dst, &src), &w) in self
            .input
            .iter_mut()
            .zip(older.iter().chain(newer))
            .zip(&self.window)
        {
            *dst = src * w;
        }

        self.fft
            .process_with_scratch(&mut self.input, &mut self.spectrum, &mut self.scratch)
            .expect("buffer sizes come from the planner");

        for (o, range) in out.iter_mut().zip(&self.bands) {
            let sum: f32 = self.spectrum[range.clone()]
                .iter()
                .map(|c| (c.norm() * self.scale).sqrt())
                .sum();
            *o = sum / range.len() as f32;
        }
    }
}

/// Averages interleaved frames of `channels` samples into mono, appending to `out`.
pub fn downmix(interleaved: &[f32], channels: usize, out: &mut Vec<f32>) {
    assert!(channels > 0, "channels must be > 0");
    let inv = 1.0 / channels as f32;
    out.extend(
        interleaved
            .chunks_exact(channels)
            .map(|frame| frame.iter().sum::<f32>() * inv),
    );
}

fn hann(n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let x = std::f32::consts::TAU * i as f32 / n as f32;
            0.5 - 0.5 * x.cos()
        })
        .collect()
}

/// Log-spaced bin ranges from bin 2 to Nyquist, as in the original C version.
/// Low bands narrower than one bin are widened to one bin, so neighbours may repeat.
fn log_bands(fft_size: usize, band_count: usize) -> Vec<Range<usize>> {
    let nyquist = fft_size / 2;
    let log_min = 2f32.log10();
    let log_max = (nyquist as f32).log10();
    let edge = |i: usize| {
        if i == band_count {
            return nyquist; // avoid float error truncating the last edge
        }
        let t = i as f32 / band_count as f32;
        10f32.powf(log_min + (log_max - log_min) * t) as usize
    };
    (0..band_count)
        .map(|i| {
            let start = edge(i).min(nyquist - 1);
            let end = edge(i + 1).min(nyquist).max(start + 1);
            start..end
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = 2048;
    const BANDS: usize = 256;

    fn sine(bin: f32, amp: f32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| amp * (std::f32::consts::TAU * bin * i as f32 / N as f32).sin())
            .collect()
    }

    #[test]
    fn bands_are_ordered_and_in_bounds() {
        let a = Analyzer::new(N, BANDS);
        let bands = a.band_ranges();
        assert_eq!(bands.len(), BANDS);
        assert_eq!(bands[0].start, 2);
        assert_eq!(bands[BANDS - 1].end, N / 2);
        for w in bands.windows(2) {
            assert!(!w[0].is_empty());
            assert!(w[0].start <= w[1].start);
            assert!(w[0].end <= w[1].end);
        }
    }

    #[test]
    fn silence_is_zero() {
        let mut a = Analyzer::new(N, BANDS);
        let mut out = vec![1.0; BANDS];
        a.push(&vec![0.0; N]);
        a.analyze(&mut out);
        assert!(out.iter().all(|&m| m == 0.0));
    }

    #[test]
    fn sine_lands_in_its_band() {
        for bin in [20usize, 100, 400, 900] {
            let mut a = Analyzer::new(N, BANDS);
            let mut out = vec![0.0; BANDS];
            a.push(&sine(bin as f32, 1.0, N));
            a.analyze(&mut out);

            let loudest = out
                .iter()
                .enumerate()
                .max_by(|x, y| x.1.total_cmp(y.1))
                .unwrap()
                .0;
            assert!(
                a.band_ranges()[loudest].contains(&bin),
                "bin {bin} peaked in band {loudest} {:?}",
                a.band_ranges()[loudest]
            );
        }
    }

    #[test]
    fn full_scale_sine_has_unit_magnitude() {
        // Low bands are one bin wide, so a bin-centred sine reads sqrt(1) = 1.
        let bin = 20;
        let mut a = Analyzer::new(N, BANDS);
        let mut out = vec![0.0; BANDS];
        a.push(&sine(bin as f32, 1.0, N));
        a.analyze(&mut out);
        let band = a
            .band_ranges()
            .iter()
            .position(|r| *r == (bin..bin + 1))
            .expect("a single-bin band at a low bin");
        assert!((out[band] - 1.0).abs() < 1e-3, "got {}", out[band]);
    }

    #[test]
    fn push_keeps_only_latest_window() {
        let mut a = Analyzer::new(N, BANDS);
        let mut out = vec![0.0; BANDS];
        // Loud noise followed by a full window of silence must read as silence.
        a.push(&sine(50.0, 1.0, N / 2));
        a.push(&vec![0.0; N + 17]);
        a.analyze(&mut out);
        assert!(out.iter().all(|&m| m == 0.0));
    }

    #[test]
    fn push_in_chunks_matches_single_push() {
        let signal = sine(123.0, 0.5, N + 300);
        let mut whole = Analyzer::new(N, BANDS);
        let mut chunked = Analyzer::new(N, BANDS);
        whole.push(&signal);
        for chunk in signal.chunks(97) {
            chunked.push(chunk);
        }
        let (mut a, mut b) = (vec![0.0; BANDS], vec![0.0; BANDS]);
        whole.analyze(&mut a);
        chunked.analyze(&mut b);
        assert_eq!(a, b);
    }

    #[test]
    fn downmix_averages_channels() {
        let mut out = Vec::new();
        downmix(&[1.0, 0.0, 0.5, 0.5, -1.0, 1.0], 2, &mut out);
        assert_eq!(out, [0.5, 0.5, 0.0]);
    }
}
