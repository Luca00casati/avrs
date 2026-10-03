//! The optional `config.toml` shared by av-server and av-viz.
//!
//! Every key is optional; command-line flags override the file. See
//! `contrib/config.toml` for a documented example.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Largest `delay_ms` adjustment accepted, either way.
pub const MAX_DELAY_MS: i32 = 2000;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Socket both programs use (a path; a pipe name on Windows).
    pub socket: Option<String>,
    pub server: ServerConfig,
    pub viz: VizConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Capture source when no files are given, by id or part of its name.
    pub source: Option<String>,
    /// Spectrum frames per second.
    pub rate: u32,
    /// Milliseconds added to the measured output latency (negative to
    /// subtract), if the visuals still run ahead of or behind the sound.
    pub delay_ms: i32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            source: None,
            rate: 60,
            delay_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VizConfig {
    /// Starting bar colour: hue in degrees.
    pub hue: f32,
    /// Starting bar colour: saturation, 0 to 1.
    pub saturation: f32,
    /// Starting bar colour: brightness, 0 to 1.
    pub brightness: f32,
    /// How fast the hue drifts, in degrees per second. 0 keeps it fixed.
    pub hue_speed: f32,
    /// Initial window size in logical pixels.
    pub width: u32,
    pub height: u32,
    /// Capture source for local analysis, by id or part of its name.
    pub source: Option<String>,
    /// Like `server.delay_ms`, for local analysis.
    pub delay_ms: i32,
}

impl Default for VizConfig {
    fn default() -> Self {
        Self {
            hue: 210.0,
            saturation: 0.7,
            brightness: 0.8,
            hue_speed: 10.0,
            width: 1024,
            height: 600,
            source: None,
            delay_ms: 0,
        }
    }
}

impl Config {
    /// Where the config file lives by default:
    /// `~/.config/avrs/config.toml` on Linux,
    /// `~/Library/Application Support/avrs/config.toml` on macOS,
    /// `%APPDATA%\avrs\config\config.toml` on Windows.
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("", "", "avrs").map(|d| d.config_dir().join("config.toml"))
    }

    /// Loads `path`, or the default location. A missing default file means
    /// defaults; a missing explicit file is an error.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let (path, explicit) = match path {
            Some(p) => (p.to_path_buf(), true),
            None => match Self::default_path() {
                Some(p) => (p, false),
                None => return Ok(Self::default()),
            },
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
                return Ok(Self::default());
            }
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("invalid config {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !(1..=240).contains(&self.server.rate) {
            bail!("server.rate must be between 1 and 240");
        }
        let v = &self.viz;
        if !(0.0..=1.0).contains(&v.saturation) || !(0.0..=1.0).contains(&v.brightness) {
            bail!("viz.saturation and viz.brightness must be between 0 and 1");
        }
        if v.width == 0 || v.height == 0 {
            bail!("viz.width and viz.height must be positive");
        }
        for (name, ms) in [("server", self.server.delay_ms), ("viz", v.delay_ms)] {
            if !(-MAX_DELAY_MS..=MAX_DELAY_MS).contains(&ms) {
                bail!("{name}.delay_ms must be between -{MAX_DELAY_MS} and {MAX_DELAY_MS}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_all_defaults() {
        let c = Config::parse("").unwrap();
        assert_eq!(c.server.rate, 60);
        assert_eq!(c.viz.hue, 210.0);
        assert!(c.socket.is_none());
    }

    #[test]
    fn partial_sections_keep_other_defaults() {
        let c = Config::parse(
            r#"
            socket = "/run/x.sock"
            [server]
            rate = 30
            [viz]
            hue = 0.0
            "#,
        )
        .unwrap();
        assert_eq!(c.socket.as_deref(), Some("/run/x.sock"));
        assert_eq!(c.server.rate, 30);
        assert_eq!(c.viz.hue, 0.0);
        assert_eq!(c.viz.saturation, 0.7);
    }

    #[test]
    fn rejects_typos_and_bad_values() {
        assert!(Config::parse("[viz]\nhue_sped = 1.0").is_err());
        assert!(Config::parse("[server]\nrate = 0").is_err());
        assert!(Config::parse("[viz]\nbrightness = 2.0").is_err());
        assert!(Config::parse("[server]\ndelay_ms = 99999").is_err());
        assert_eq!(
            Config::parse("[server]\ndelay_ms = -40")
                .unwrap()
                .server
                .delay_ms,
            -40
        );
    }

    #[test]
    fn example_config_is_valid() {
        let text = include_str!("../../../contrib/config.toml");
        Config::parse(text).unwrap();
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        assert!(Config::load(Some(Path::new("/definitely/not/here.toml"))).is_err());
    }
}
