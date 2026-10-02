//! File decoding with symphonia.

use std::fs::File;
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;

/// An open audio file producing interleaved `f32` frames at its native rate.
pub struct Decoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    sample_rate: u32,
    channels: usize,
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

        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
        })
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
    fn rejects_non_audio() {
        let dir = temp_dir("reject");
        let path = dir.join("fake.mp3");
        std::fs::write(&path, b"not audio at all").unwrap();
        assert!(Decoder::open(&path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
