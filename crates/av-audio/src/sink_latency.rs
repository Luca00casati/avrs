//! Tracks how far behind a sink the sound is actually heard.
//!
//! A monitor source receives audio as the sink takes it, before the sink's own
//! buffering and transport (for Bluetooth, typically 100-300 ms). Sinks may
//! report that latency, so we poll it and delay the analysis to match. Only
//! PulseAudio/PipeWire expose this; elsewhere the latency reads as zero.
//!
//! Bluetooth sinks often report almost nothing: the headset's decode delay is
//! only known when it supports A2DP delay reporting, and many (and the SBC
//! path in PipeWire) don't. For those we assume a typical delay, which users
//! can fine-tune with `delay_ms`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

/// How often the sink latency is refreshed.
#[cfg(target_os = "linux")]
const POLL: Duration = Duration::from_millis(500);

/// Assumed delay of a Bluetooth A2DP sink that reports none.
const BLUETOOTH_ESTIMATE: Duration = Duration::from_millis(200);
/// Reported latencies below this are taken as "not reported".
const UNREPORTED_BELOW: Duration = Duration::from_millis(50);

pub struct SinkLatency {
    micros: Arc<AtomicU64>,
    /// Whether the sink still exists.
    present: Arc<AtomicBool>,
    bluetooth: bool,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SinkLatency {
    /// A latency that is always zero.
    pub fn none() -> Self {
        Self {
            micros: Arc::new(AtomicU64::new(0)),
            present: Arc::new(AtomicBool::new(false)),
            bluetooth: false,
            stop: Arc::new(AtomicBool::new(true)),
            thread: None,
        }
    }

    /// Tracks the sink a monitor source (`<sink>.monitor`) belongs to.
    pub fn for_monitor(host: cpal::HostId, device_id: &str) -> Self {
        match device_id.strip_suffix(".monitor") {
            Some(sink) => Self::for_sink(host, sink),
            None => Self::none(),
        }
    }

    /// Tracks a sink by its cpal device id string (the PulseAudio sink name).
    #[cfg(target_os = "linux")]
    pub fn for_sink(host: cpal::HostId, sink: &str) -> Self {
        if host != cpal::HostId::PulseAudio {
            return Self::none();
        }
        let bluetooth = sink.starts_with("bluez_output.");
        let Ok(sink) = std::ffi::CString::new(sink) else {
            return Self::none();
        };
        let Ok(client) = pulseaudio::Client::from_env(c"avrs") else {
            return Self::none();
        };

        let micros = Arc::new(AtomicU64::new(0));
        let present = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (micros, present, stop) = (micros.clone(), present.clone(), stop.clone());
            std::thread::Builder::new()
                .name("av-sink-latency".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match pollster::block_on(client.sink_info_by_name(sink.clone())) {
                            Ok(info) => {
                                micros.store(info.actual_latency, Ordering::Relaxed);
                                present.store(true, Ordering::Relaxed);
                            }
                            // The sink went away (e.g. headphones disconnected);
                            // whatever we were moved to is not this sink.
                            Err(_) => {
                                micros.store(0, Ordering::Relaxed);
                                present.store(false, Ordering::Relaxed);
                            }
                        }
                        std::thread::park_timeout(POLL);
                    }
                })
        };
        Self {
            micros,
            present,
            bluetooth,
            stop,
            thread: thread.ok(),
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub fn for_sink(_host: cpal::HostId, _sink: &str) -> Self {
        Self::none()
    }

    fn reported(&self) -> Duration {
        Duration::from_micros(self.micros.load(Ordering::Relaxed))
    }

    /// Delay the sink itself does not report but very likely has.
    pub fn unreported(&self) -> Duration {
        if self.bluetooth
            && self.present.load(Ordering::Relaxed)
            && self.reported() < UNREPORTED_BELOW
        {
            BLUETOOTH_ESTIMATE
        } else {
            Duration::ZERO
        }
    }

    /// Total delay from the sink taking audio to it being heard.
    pub fn total(&self) -> Duration {
        self.reported() + self.unreported()
    }
}

impl Drop for SinkLatency {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
