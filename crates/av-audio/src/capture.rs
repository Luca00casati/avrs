use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use crate::SourceKind;
use crate::devices::{default_loopback_id, display_name, find, low_latency_configs};
use crate::sink_latency::SinkLatency;

/// Seconds of audio the ring buffer can hold before samples are dropped.
const BUFFER_SECS: u32 = 1;
/// Requested capture fragment length.
const CAPTURE_PERIOD: Duration = Duration::from_millis(10);

/// A capture that delivers mono `f32` samples.
///
/// The audio callback downmixes and pushes into a lock-free ring buffer; call
/// [`drain`](Self::drain) regularly from any thread to collect the samples.
///
/// Opened without a selector, it follows the default output: when that
/// changes (headphones connected or removed, a different device chosen), it
/// switches to the new output's loopback. A stream that fails is reopened.
/// The checking and reopening happen on a background thread, since opening a
/// device can block for a long time; the switch happens on the next `drain`.
pub struct Capture {
    /// Always `Some` until dropped; see the `Drop` impl.
    live: Option<Live>,
    replacements: mpsc::Receiver<Live>,
    stop_watcher: Arc<AtomicBool>,
    watcher: std::thread::Thread,
}

impl Capture {
    /// How often the watcher looks for a new default output or a failed stream.
    const CHECK_EVERY: Duration = Duration::from_secs(1);

    /// Starts capturing. `None` records what the default output is playing;
    /// otherwise see [`list_sources`](crate::list_sources) for valid selectors.
    pub fn open(selector: Option<&str>) -> Result<Self> {
        let host = cpal::default_host();
        let live = Live::open(&host, selector)?;
        let (tx, replacements) = mpsc::channel();
        let stop_watcher = Arc::new(AtomicBool::new(false));
        let watcher = {
            let selector = selector.map(str::to_owned);
            let (mut current_id, mut current_failed) =
                (live.device_id.clone(), live.failed.clone());
            let stop = stop_watcher.clone();
            std::thread::Builder::new()
                .name("av-capture-watch".into())
                .spawn(move || {
                    // A connection of our own for the checks, and a fresh one
                    // per reopen: with cpal's PulseAudio host, queries on a
                    // connection that another thread opened streams on can
                    // block indefinitely.
                    let host = cpal::default_host();
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::park_timeout(Self::CHECK_EVERY);
                        let moved = selector.is_none()
                            && default_loopback_id(&host).is_some_and(|id| id != current_id);
                        if !(moved || current_failed.load(Ordering::Relaxed)) {
                            continue;
                        }
                        match Live::open(&cpal::default_host(), selector.as_deref()) {
                            Ok(live) => {
                                current_id = live.device_id.clone();
                                current_failed = live.failed.clone();
                                if tx.send(live).is_err() {
                                    return;
                                }
                            }
                            // Try again on the next check.
                            Err(e) => eprintln!("cannot reopen capture: {e:#}"),
                        }
                    }
                })?
                .thread()
                .clone()
        };
        Ok(Self {
            live: Some(live),
            replacements,
            stop_watcher,
            watcher,
        })
    }

    /// Appends every sample captured since the last call to `out`.
    pub fn drain(&mut self, out: &mut Vec<f32>) {
        if let Some(live) = self.replacements.try_iter().last() {
            eprintln!("capturing {}", live.name);
            if let Some(old) = self.live.replace(live) {
                close_in_background(old);
            }
        }
        let Some(live) = &mut self.live else {
            return;
        };
        let rx = &mut live.rx;
        let Ok(chunk) = rx.read_chunk(rx.slots()) else {
            return;
        };
        let (a, b) = chunk.as_slices();
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        chunk.commit_all();
    }

    fn live(&self) -> &Live {
        self.live.as_ref().expect("present until dropped")
    }

    pub fn name(&self) -> &str {
        &self.live().name
    }

    pub fn kind(&self) -> SourceKind {
        self.live().kind
    }

    pub fn sample_rate(&self) -> u32 {
        self.live().sample_rate
    }

    /// Samples lost because [`drain`](Self::drain) was not called often enough.
    pub fn dropped(&self) -> u64 {
        self.live().dropped.load(Ordering::Relaxed)
    }

    /// How long after capture the audio is heard. Nonzero only for
    /// PulseAudio/PipeWire monitor sources: the sink's reported latency, or an
    /// estimate for Bluetooth sinks that report none.
    pub fn output_latency(&self) -> Duration {
        self.live().sink_latency.total()
    }

    /// Whether the stream reported an error (for example, the device went away).
    pub fn has_failed(&self) -> bool {
        self.live().failed.load(Ordering::Relaxed)
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Not joined: the watcher may be stuck opening a device; it exits on
        // its own once that returns.
        self.stop_watcher.store(true, Ordering::Relaxed);
        self.watcher.unpark();
        if let Some(live) = self.live.take() {
            close_in_background(live);
        }
    }
}

/// Closes a stream on a throwaway thread. Closing can block (cpal's
/// PulseAudio host waits on a pending server query), which must not stall the
/// analysis loop or keep the process from exiting.
fn close_in_background(live: Live) {
    let _ = std::thread::Builder::new()
        .name("av-capture-close".into())
        .spawn(move || drop(live));
}

/// One open capture stream on one device.
struct Live {
    _stream: cpal::Stream,
    rx: rtrb::Consumer<f32>,
    device_id: String,
    name: String,
    kind: SourceKind,
    sample_rate: u32,
    dropped: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    sink_latency: SinkLatency,
}

impl Live {
    fn open(host: &cpal::Host, selector: Option<&str>) -> Result<Self> {
        let (device, kind) = find(host, selector)?;
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
        let dropped = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicBool::new(false));

        // Small fragments so audio arrives steadily; fall back to the default.
        let mut attempt = Err(anyhow::anyhow!("no stream config to try"));
        for stream_config in low_latency_configs(&config, CAPTURE_PERIOD) {
            let (tx, rx) = rtrb::RingBuffer::new((sample_rate * BUFFER_SECS) as usize);
            attempt = build_stream(
                &device,
                config.sample_format(),
                stream_config,
                channels,
                tx,
                dropped.clone(),
                failed.clone(),
            )
            .map(|stream| (stream, rx));
            if attempt.is_ok() {
                break;
            }
        }
        let (stream, rx) = attempt.with_context(|| format!("failed to open {name:?}"))?;
        stream
            .play()
            .with_context(|| format!("failed to start {name:?}"))?;

        let id = device.id().ok();
        let sink_latency = match &id {
            Some(id) => SinkLatency::for_monitor(id.host(), id.id()),
            None => SinkLatency::none(),
        };

        Ok(Self {
            _stream: stream,
            rx,
            device_id: id.map(|id| id.to_string()).unwrap_or_default(),
            name,
            kind,
            sample_rate,
            dropped,
            failed,
            sink_latency,
        })
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
