//! Audio sources for av, built on cpal so the same code runs on Linux,
//! Windows and macOS.
//!
//! - [`list_sources`] enumerates what can be captured.
//! - [`Capture`] records a source and hands out mono samples.
//! - [`Player`] plays files and hands out mono samples of what it played.

mod capture;
mod convert;
mod decode;
mod devices;
mod player;

pub use capture::Capture;
pub use devices::{SourceInfo, SourceKind, list_sources};
pub use player::{Player, collect_playlist};
