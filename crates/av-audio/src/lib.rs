//! Audio sources for av, built on cpal so the same code runs on Linux,
//! Windows and macOS.
//!
//! - [`list_sources`] enumerates what can be captured.
//! - [`Capture`] records a source and hands out mono samples.

mod capture;
mod devices;

pub use capture::Capture;
pub use devices::{SourceInfo, SourceKind, list_sources};
