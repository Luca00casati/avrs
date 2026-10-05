//! File decoding with symphonia.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::units::{Time, TimeBase};

/// An open audio file producing interleaved `f32` frames at its native rate.
pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    sample_rate: u32,
    channels: usize,
    time_base: Option<TimeBase>,
    duration: Option<f64>,
    /// Frames still to drop after a seek landed before its target.
    skip_frames: usize,
}

impl Decoder {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(&hint, mss, Default::default(), Default::default())
            .with_context(|| format!("unsupported format: {}", path.display()))?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| anyhow!("no audio track in {}", path.display()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| anyhow!("no audio codec parameters in {}", path.display()))?;
        let sample_rate = params
            .sample_rate
            .ok_or_else(|| anyhow!("unknown sample rate in {}", path.display()))?;
        let channels = params
            .channels
            .as_ref()
            .map(|c| c.count())
            .ok_or_else(|| anyhow!("unknown channel layout in {}", path.display()))?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .with_context(|| format!("unsupported codec in {}", path.display()))?;
        let track_id = track.id;
        let time_base = track.time_base;
        // Prefer the container's duration; fall back to counting frames.
        let duration = time_base
            .zip(track.duration)
            .and_then(|(tb, d)| tb.calc_duration(d))
            .map(|t| t.as_secs_f64())
            .or_else(|| track.num_frames.map(|n| n as f64 / f64::from(sample_rate)));

        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
            time_base,
            duration,
            skip_frames: 0,
        })
    }

    /// Length of the track in seconds, if the file says.
    pub fn duration(&self) -> Option<f64> {
        self.duration
    }

    /// Jumps to `secs` from the start and returns the new position in
    /// seconds. Formats land on a packet boundary at or before the target; the
    /// samples in between are dropped, so playback starts right at `secs`.
    pub fn seek(&mut self, secs: f64) -> Result<f64> {
        let time = Time::try_from_secs_f64(secs.max(0.0))
            .ok_or_else(|| anyhow!("cannot seek to {secs} s"))?;
        let to = SeekTo::Time {
            time,
            track_id: Some(self.track_id),
        };
        let seeked = self.format.seek(SeekMode::Accurate, to)?;
        self.decoder.reset();
        let to_secs = |ts| {
            self.time_base
                .and_then(|tb| tb.calc_time(ts))
                .map(|t| t.as_secs_f64())
        };
        let (Some(required), Some(actual)) =
            (to_secs(seeked.required_ts), to_secs(seeked.actual_ts))
        else {
            self.skip_frames = 0;
            return Ok(secs);
        };
        let early = (required - actual).max(0.0);
        self.skip_frames = (early * f64::from(self.sample_rate)).round() as usize;
        Ok(required)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Replaces `out` with the next decoded packet. Returns `false` at the end.
    pub fn next_chunk(&mut self, out: &mut Vec<f32>) -> Result<bool> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => return Ok(false),
                // Chained streams (e.g. concatenated Ogg) are treated as the end.
                Err(Error::ResetRequired) => return Ok(false),
                Err(err) => return Err(err.into()),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    // Trust the buffer over the container header.
                    self.channels = buf.spec().channels().count();
                    out.clear();
                    out.resize(buf.samples_interleaved(), 0.0);
                    buf.copy_to_slice_interleaved(&mut out[..]);
                    if self.skip_frames > 0 {
                        let frames = out.len() / self.channels.max(1);
                        let skip = self.skip_frames.min(frames);
                        out.drain(..skip * self.channels);
                        self.skip_frames -= skip;
                        if out.is_empty() {
                            continue;
                        }
                    }
                    return Ok(true);
                }
                // A corrupt packet is skipped, not fatal.
                Err(Error::DecodeError(_)) | Err(Error::IoError(_)) => continue,
                Err(err) => return Err(err.into()),
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Writes a 16-bit PCM WAV of a 1 kHz sine and returns its path.
    pub(crate) fn write_wav(
        dir: &Path,
        name: &str,
        rate: u32,
        channels: u16,
        frames: u32,
    ) -> PathBuf {
        let data_len = frames * u32::from(channels) * 2;
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        bytes.extend_from_slice(&(channels * 2).to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let s = (std::f32::consts::TAU * 1000.0 * i as f32 / rate as f32).sin();
            for _ in 0..channels {
                bytes.extend_from_slice(&((s * 16_000.0) as i16).to_le_bytes());
            }
        }
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    pub(crate) fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("av-audio-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn decodes_wav() {
        let dir = temp_dir("decode");
        let path = write_wav(&dir, "tone.wav", 22_050, 2, 11_025);

        let mut decoder = Decoder::open(&path).unwrap();
        assert_eq!(decoder.sample_rate(), 22_050);
        assert_eq!(decoder.channels(), 2);

        let mut chunk = Vec::new();
        let mut samples = 0;
        let mut peak = 0f32;
        while decoder.next_chunk(&mut chunk).unwrap() {
            samples += chunk.len();
            peak = chunk.iter().fold(peak, |m, s| m.max(s.abs()));
        }
        assert_eq!(samples, 11_025 * 2);
        assert!((peak - 16_000.0 / 32_768.0).abs() < 0.01, "peak {peak}");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn knows_duration_and_seeks() {
        let dir = temp_dir("seek");
        // Two seconds at 8 kHz.
        let path = write_wav(&dir, "two.wav", 8_000, 1, 16_000);
        let mut decoder = Decoder::open(&path).unwrap();
        let duration = decoder.duration().expect("wav has a duration");
        assert!((duration - 2.0).abs() < 1e-3, "{duration}");

        let landed = decoder.seek(1.5).unwrap();
        assert!((landed - 1.5).abs() < 1e-3, "landed at {landed}");
        let mut chunk = Vec::new();
        let mut frames = 0;
        while decoder.next_chunk(&mut chunk).unwrap() {
            frames += chunk.len();
        }
        // Exactly half a second left.
        assert!(
            (3_990..=4_010).contains(&frames),
            "{frames} frames after seek"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rejects_non_audio() {
        let dir = temp_dir("reject");
        let path = dir.join("fake.mp3");
        std::fs::write(&path, b"not audio at all").unwrap();
        assert!(Decoder::open(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
