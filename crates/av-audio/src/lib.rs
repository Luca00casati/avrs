//! Audio sources for av, built on cpal so the same code runs on Linux,
//! Windows and macOS.
//!
//! - [`list_sources`] enumerates what can be captured.
//! - [`Capture`] records a source and hands out mono samples.
//! - [`Player`] plays files and hands out mono samples of what it played.
//! - [`TestSignal`] generates a sweep without any audio device.
//! - [`Engine`] wraps any of them and turns samples into band magnitudes.

mod capture;
mod convert;
mod decode;
mod devices;
mod engine;
mod player;
mod signal;

pub use capture::Capture;
pub use devices::{SourceInfo, SourceKind, list_sources};
pub use engine::{Engine, Source};
pub use player::{Player, collect_playlist};
pub use signal::TestSignal;
