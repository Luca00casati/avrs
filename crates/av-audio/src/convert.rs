//! Channel mapping and streaming sample-rate conversion to the output format.

use anyhow::Result;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};

/// Frames per resampler chunk.
const CHUNK: usize = 1024;

/// Converts interleaved audio of one rate and channel count to another,
/// a chunk at a time.
pub struct Converter {
    in_channels: usize,
    out_channels: usize,
    resampler: Option<Fft<f32>>,
    /// Channel-mapped input waiting for a full resampler chunk.
    pending: Vec<f32>,
    /// Output frames still to drop: the resampler's start-up delay.
    skip: usize,
    scratch: Vec<f32>,
}

impl Converter {
    pub fn new(
        in_rate: u32,
        in_channels: usize,
        out_rate: u32,
        out_channels: usize,
    ) -> Result<Self> {
        let resampler = if in_rate == out_rate {
            None
        } else {
            Some(Fft::new(
                in_rate as usize,
                out_rate as usize,
                CHUNK,
                out_channels,
                FixedSync::Input,
            )?)
        };
        let skip = resampler.as_ref().map_or(0, |r| r.output_delay());
        let scratch =
            vec![0.0; resampler.as_ref().map_or(0, |r| r.output_frames_max()) * out_channels];
        Ok(Self {
            in_channels,
            out_channels,
            resampler,
            pending: Vec::new(),
            skip,
            scratch,
        })
    }

    /// Converts `input` (interleaved, `in_channels`), appending to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        if self.resampler.is_none() {
            map_channels(input, self.in_channels, self.out_channels, out);
            return Ok(());
        }
        map_channels(
            input,
            self.in_channels,
            self.out_channels,
            &mut self.pending,
        );
        self.run(out, false)
    }

    /// Flushes buffered audio at the end of a stream, appending to `out`.
    pub fn finish(&mut self, out: &mut Vec<f32>) -> Result<()> {
        if self.resampler.is_none() {
            return Ok(());
        }
        self.run(out, true)
    }

    fn run(&mut self, out: &mut Vec<f32>, flush: bool) -> Result<()> {
        let ch = self.out_channels;
        let resampler = self.resampler.as_mut().expect("checked by caller");
        let mut consumed = 0;
        loop {
            let need = resampler.input_frames_next();
            let have = self.pending.len() / ch - consumed;
            if have < need && !flush {
                break;
            }
            let input = InterleavedSlice::new(&self.pending[consumed * ch..], ch, have)?;
            let out_frames = self.scratch.len() / ch;
            let mut output = InterleavedSlice::new_mut(&mut self.scratch, ch, out_frames)?;
            let indexing = Indexing {
                input_offset: 0,
                output_offset: 0,
                partial_len: (have < need).then_some(have),
                active_channels_mask: None,
            };
            let (used, made) =
                resampler.process_into_buffer(&input, &mut output, Some(&indexing))?;
            let used = used.min(have);
            consumed += used;

            let drop = self.skip.min(made);
            self.skip -= drop;
            out.extend_from_slice(&self.scratch[drop * ch..made * ch]);

            if have < need {
                // Final partial chunk; one more pass pushes out the delay line.
                if flush && have > 0 {
                    continue;
                }
                break;
            }
        }
        self.pending.drain(..consumed * ch);
        Ok(())
    }
}

/// Maps interleaved frames between channel counts, appending to `out`.
///
/// Mono is duplicated to every output, any input folds to mono by averaging,
/// and otherwise each output takes the matching input (wrapping around).
fn map_channels(input: &[f32], from: usize, to: usize, out: &mut Vec<f32>) {
    if from == to {
        out.extend_from_slice(input);
        return;
    }
    out.reserve(input.len() / from * to);
    for frame in input.chunks_exact(from) {
        if to == 1 {
            out.push(frame.iter().sum::<f32>() / from as f32);
        } else {
            out.extend((0..to).map(|c| frame[c % from]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_channels() {
        let mut out = Vec::new();
        map_channels(&[0.1, 0.2], 1, 2, &mut out);
        assert_eq!(out, [0.1, 0.1, 0.2, 0.2]);

        out.clear();
        map_channels(&[1.0, 0.0, 0.5, 0.5], 2, 1, &mut out);
        assert_eq!(out, [0.5, 0.5]);

        out.clear();
        map_channels(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 6, 2, &mut out);
        assert_eq!(out, [1.0, 2.0]);
    }

    #[test]
    fn passthrough_at_same_rate() {
        let mut c = Converter::new(48_000, 2, 48_000, 2).unwrap();
        let mut out = Vec::new();
        c.process(&[0.1, 0.2, 0.3, 0.4], &mut out).unwrap();
        c.finish(&mut out).unwrap();
        assert_eq!(out, [0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn resamples_to_the_expected_length_and_pitch() {
        let (rate_in, rate_out) = (44_100u32, 48_000u32);
        let freq = 1000.0;
        let input: Vec<f32> = (0..rate_in)
            .map(|i| (std::f32::consts::TAU * freq * i as f32 / rate_in as f32).sin())
            .collect();

        let mut c = Converter::new(rate_in, 1, rate_out, 1).unwrap();
        let mut out = Vec::new();
        // Feed in odd-sized pieces, like decoder packets.
        for piece in input.chunks(1152) {
            c.process(piece, &mut out).unwrap();
        }
        c.finish(&mut out).unwrap();

        // One second in, about one second out (padding from the last chunk allowed).
        let len = out.len() as i64;
        assert!(
            (len - rate_out as i64).abs() <= CHUNK as i64 * 2,
            "got {len} frames"
        );

        // Same pitch: count zero crossings over the middle of the output.
        let mid = &out[4800..43200];
        let crossings = mid.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        let measured = crossings as f32 / (mid.len() as f32 / rate_out as f32);
        assert!((measured - freq).abs() < 5.0, "measured {measured} Hz");

        // Start-up delay is trimmed: the output starts near the input's start.
        let first_peak = out.iter().position(|&s| s > 0.9).unwrap();
        assert!(first_peak < 30, "first peak at frame {first_peak}");
    }
}
