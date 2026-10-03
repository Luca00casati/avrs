//! Shared audio analysis and visual shaping for the av server and clients.
//!
//! - [`Analyzer`] turns a stream of mono samples into log-spaced band magnitudes.
//! - [`Smoother`] turns band magnitudes into normalized bar and peak heights.
//! - [`Palette`] and [`Hsl`] colour the bars.

mod analyzer;
mod color;
mod smoother;

pub use analyzer::{Analyzer, downmix, pool_max};
pub use color::{Hsl, Palette};
pub use smoother::Smoother;

/// Default FFT length, matching the original C visualizer.
pub const DEFAULT_FFT_SIZE: usize = 2048;
/// Default number of bars, matching the original C visualizer.
pub const DEFAULT_BANDS: usize = 256;
