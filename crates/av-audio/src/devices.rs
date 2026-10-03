//! Finding capturable sources across hosts.
//!
//! "Loopback" means recording what the system is playing:
//! - Linux (PulseAudio/PipeWire): the sink's `.monitor` source.
//! - Windows (WASAPI) and macOS 14.6+ (CoreAudio): an output device opened as
//!   an input, which cpal turns into a loopback stream.

use std::fmt;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, HostTrait};

/// Whether capturing an output device as input yields loopback on this platform.
const OUTPUT_LOOPBACK: bool = cfg!(any(target_os = "windows", target_os = "macos"));
const MONITOR_SUFFIX: &str = ".monitor";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// A microphone or line input.
    Input,
    /// What an output device is playing.
    Loopback,
}

#[derive(Debug, Clone)]
pub struct SourceInfo {
    /// Stable identifier, as `host:device`. Accepted by [`Capture::open`](crate::Capture::open).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    pub kind: SourceKind,
    /// Whether this is what [`Capture::open`](crate::Capture::open) picks with no selector.
    pub is_default: bool,
}

impl fmt::Display for SourceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            SourceKind::Input => "input",
            SourceKind::Loopback => "loopback",
        };
        let default = if self.is_default { " (default)" } else { "" };
        write!(f, "[{kind}] {}{default}\n    {}", self.name, self.id)
    }
}

/// Lists every source that can be captured, loopback sources first.
pub fn list_sources() -> Result<Vec<SourceInfo>> {
    let host = cpal::default_host();
    let default_id = default_loopback(&host)
        .ok()
        .and_then(|d| d.id().ok())
        .map(|id| id.to_string());

    let mut sources: Vec<SourceInfo> = host
        .devices()
        .context("failed to enumerate audio devices")?
        .filter_map(|device| {
            let kind = kind_of(&device)?;
            let id = device.id().ok()?.to_string();
            Some(SourceInfo {
                name: display_name(&device),
                is_default: default_id.as_deref() == Some(id.as_str()),
                id,
                kind,
            })
        })
        .collect();
    sources.sort_by_key(|s| (s.kind != SourceKind::Loopback, !s.is_default));
    Ok(sources)
}

/// Finds the device to capture and whether it is loopback.
///
/// With no selector, picks the default output's loopback. Otherwise matches an
/// exact id (with or without the `host:` prefix), then an exact name, then a
/// unique substring of the name or id, ignoring case.
pub(crate) fn find(
    host: &cpal::Host,
    selector: Option<&str>,
) -> Result<(cpal::Device, SourceKind)> {
    let Some(selector) = selector else {
        let device = default_loopback(host)?;
        return Ok((device, SourceKind::Loopback));
    };

    let candidates: Vec<(cpal::Device, SourceKind)> = host
        .devices()
        .context("failed to enumerate audio devices")?
        .filter_map(|d| kind_of(&d).map(|k| (d, k)))
        .collect();

    let id_matches = |d: &cpal::Device| {
        d.id()
            .is_ok_and(|id| id.to_string() == selector || id.id() == selector)
    };
    if let Some(found) = candidates.iter().find(|(d, _)| id_matches(d)) {
        return Ok(found.clone());
    }

    let name_matches = |d: &cpal::Device| display_name(d).eq_ignore_ascii_case(selector);
    if let Some(found) = candidates.iter().find(|(d, _)| name_matches(d)) {
        return Ok(found.clone());
    }

    let needle = selector.to_lowercase();
    let mut matches: Vec<_> = candidates
        .into_iter()
        .filter(|(d, _)| {
            display_name(d).to_lowercase().contains(&needle)
                || d.id()
                    .is_ok_and(|id| id.to_string().to_lowercase().contains(&needle))
        })
        .collect();
    match matches.len() {
        0 => bail!("no audio source matches {selector:?}; list them with --sources"),
        1 => Ok(matches.remove(0)),
        _ => {
            let names: Vec<String> = matches
                .iter()
                .map(|(d, _)| format!("  {}", display_name(d)))
                .collect();
            bail!(
                "{selector:?} matches several sources, be more specific:\n{}",
                names.join("\n")
            )
        }
    }
}

pub(crate) fn display_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.to_string())
        .or_else(|_| device.id().map(|id| id.id().to_owned()))
        .unwrap_or_else(|_| "unknown device".to_owned())
}

fn kind_of(device: &cpal::Device) -> Option<SourceKind> {
    if device.supports_input() {
        let is_monitor = device
            .id()
            .is_ok_and(|id| id.id().ends_with(MONITOR_SUFFIX));
        Some(if is_monitor {
            SourceKind::Loopback
        } else {
            SourceKind::Input
        })
    } else if OUTPUT_LOOPBACK && device.supports_output() {
        Some(SourceKind::Loopback)
    } else {
        None
    }
}

/// Id (`host:device`) of what [`find`] picks with no selector, if anything.
pub(crate) fn default_loopback_id(host: &cpal::Host) -> Option<String> {
    default_loopback(host)
        .ok()?
        .id()
        .ok()
        .map(|id| id.to_string())
}

fn default_loopback(host: &cpal::Host) -> Result<cpal::Device> {
    let output = host
        .default_output_device()
        .context("no default output device")?;
    if OUTPUT_LOOPBACK {
        return Ok(output);
    }
    // PulseAudio/PipeWire name the monitor of sink `X` as `X.monitor`.
    let sink = output.id().context("default output has no id")?;
    let monitor = cpal::DeviceId::new(sink.host(), format!("{}{MONITOR_SUFFIX}", sink.id()));
    host.device_by_id(&monitor).ok_or_else(|| {
        anyhow!(
            "no monitor source for the default output {:?} (host {}); \
             loopback needs PulseAudio or PipeWire, pick an input with --source",
            sink.id(),
            sink.host()
        )
    })
}

/// Stream configs to try, in order: `config` with a fixed buffer of about
/// `period` (clamped to what the device supports), then the device default.
///
/// The default can be very large: PulseAudio's is about two seconds, which
/// delays playback controls and makes captured audio arrive in late bursts.
/// Too small is also harmful: on PipeWire the smallest request sets the
/// buffer size for every app, so callers ask for at least ~25 ms.
pub(crate) fn low_latency_configs(
    config: &cpal::SupportedStreamConfig,
    period: std::time::Duration,
) -> Vec<cpal::StreamConfig> {
    let default: cpal::StreamConfig = config.config();
    let frames = (f64::from(config.sample_rate()) * period.as_secs_f64()) as u32;
    match *config.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } if min <= max => {
            let fixed = cpal::StreamConfig {
                buffer_size: cpal::BufferSize::Fixed(frames.clamp(min, max)),
                ..default
            };
            vec![fixed, default]
        }
        _ => vec![default],
    }
}
