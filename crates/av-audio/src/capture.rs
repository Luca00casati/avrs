use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use crate::SourceKind;
use crate::devices::{display_name, find};

/// Seconds of audio the ring buffer can hold before samples are dropped.
const BUFFER_SECS: u32 = 1;

/// A running capture stream that delivers mono `f32` samples.
///
/// The audio callback downmixes and pushes into a lock-free ring buffer; call
/// [`drain`](Self::drain) regularly from any thread to collect the samples.
pub struct Capture {
    _stream: cpal::Stream,
    rx: rtrb::Consumer<f32>,
    name: String,
    kind: SourceKind,
    sample_rate: u32,
    dropped: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
}

impl Capture {
    /// Starts capturing. `None` records what the default output is playing;
    /// otherwise see [`list_sources`](crate::list_sources) for valid selectors.
    pub fn open(selector: Option<&str>) -> Result<Self> {
        let (device, kind) = find(selector)?;
        let name = display_name(&device);
        let config = match kind {
            SourceKind::Input => device.default_input_config(),
            // Loopback streams take the output's format (WASAPI, CoreAudio).
            // Monitor sources are ordinary inputs.
            SourceKind::Loopback if device.supports_input() => device.default_input_config(),
            SourceKind::Loopback => device.default_output_config(),
        }
        .with_context(|| format!("no usable config for {name:?}"))?;

        let sample_rate = config.sample_rate();
        let channels = usize::from(config.channels());
        let (tx, rx) = rtrb::RingBuffer::new((sample_rate * BUFFER_SECS) as usize);
        let dropped = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicBool::new(false));

        let stream = build_stream(
            &device,
            config.sample_format(),
            config.into(),
            channels,
            tx,
            dropped.clone(),
            failed.clone(),
        )
        .with_context(|| format!("failed to open {name:?}"))?;
        stream
            .play()
            .with_context(|| format!("failed to start {name:?}"))?;

        Ok(Self {
            _stream: stream,
            rx,
            name,
            kind,
            sample_rate,
            dropped,
            failed,
        })
    }

    /// Appends every sample captured since the last call to `out`.
    pub fn drain(&mut self, out: &mut Vec<f32>) {
        let Ok(chunk) = self.rx.read_chunk(self.rx.slots()) else {
            return;
        };
        let (a, b) = chunk.as_slices();
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        chunk.commit_all();
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> SourceKind {
        self.kind
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Samples lost because [`drain`](Self::drain) was not called often enough.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Whether the stream reported an error (for example, the device went away).
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
}

fn build_stream(
    device: &cpal::Device,
    format: SampleFormat,
    config: cpal::StreamConfig,
    channels: usize,
    tx: rtrb::Producer<f32>,
    dropped: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream> {
    macro_rules! build {
        ($t:ty) => {
            build_typed::<$t>(device, config, channels, tx, dropped, failed)
        };
    }
    Ok(match format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U32 => build!(u32),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        other => anyhow::bail!("unsupported sample format {other}"),
    }?)
}

fn build_typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    mut tx: rtrb::Producer<f32>,
    dropped: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let inv = 1.0 / channels as f32;
    device.build_input_stream::<T, _, _>(
        config,
        move |data: &[T], _| {
            let mut lost = 0;
            for frame in data.chunks_exact(channels) {
                let mono = frame.iter().map(|s| s.to_sample::<f32>()).sum::<f32>() * inv;
                if tx.push(mono).is_err() {
                    lost += 1;
                }
            }
            if lost > 0 {
                dropped.fetch_add(lost, Ordering::Relaxed);
            }
        },
        move |err| {
            eprintln!("audio stream error: {err}");
            failed.store(true, Ordering::Relaxed);
        },
        None,
    )
}
