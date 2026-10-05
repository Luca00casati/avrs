//! Wire protocol between `av-server` and its clients.
//!
//! Messages are postcard-encoded and framed with a little-endian `u32` length
//! prefix. On connect the server sends [`ServerMsg::Hello`]; clients must
//! check [`PROTOCOL_VERSION`] before using anything else.

mod config;

pub use config::{Config, MAX_DELAY_MS, ServerConfig, VizConfig};

use std::io::{self, Read, Write};
use std::path::PathBuf;

use interprocess::local_socket::{GenericFilePath, GenericNamespaced, Name, prelude::*};
use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to the messages below.
pub const PROTOCOL_VERSION: u32 = 3;

/// Upper bound on a frame, so a bad peer can't make us allocate gigabytes.
const MAX_FRAME: usize = 1 << 20;

/// Environment variable that overrides the socket location.
pub const SOCKET_ENV: &str = "AVRS_SOCKET";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerMsg {
    Hello {
        version: u32,
        /// Number of magnitudes in every [`ServerMsg::Spectrum`].
        bands: usize,
        /// Spectrum frames per second.
        rate: u32,
    },
    /// Band magnitudes, as produced by `av_core::Analyzer`, and how far into
    /// the current track playback is (files only), in seconds.
    Spectrum {
        seq: u64,
        mags: Vec<f32>,
        position: Option<f32>,
    },
    /// Sent on connect and whenever it changes.
    Status(Status),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// What is playing or being captured, ready to display.
    pub title: String,
    pub paused: bool,
    /// Whether next/previous do anything.
    pub has_playlist: bool,
    /// A non-looping playlist has ended.
    pub finished: bool,
    /// Total delay applied to sync the visuals with the sound, in ms.
    pub delay_ms: u32,
    /// The user's part of that delay (`delay_ms` in the config), in ms.
    pub extra_delay_ms: i32,
    /// Length of the current track in seconds (files only, if known).
    pub duration: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ClientMsg {
    SetPaused(bool),
    Next,
    Previous,
    /// Shift the sync delay by this many milliseconds.
    AdjustDelay(i32),
    /// Jump to this many seconds into the current track.
    Seek(f32),
}

/// Encodes one message as a complete frame, ready to write.
pub fn encode<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let body = postcard::to_stdvec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(body.len()).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Writes one framed message.
pub fn write_msg<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    w.write_all(&encode(msg)?)?;
    w.flush()
}

/// Reads one framed message. `buf` is reused between calls.
pub fn read_msg<R: Read, T: for<'de> Deserialize<'de>>(
    r: &mut R,
    buf: &mut Vec<u8>,
) -> io::Result<T> {
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds limit"),
        ));
    }
    buf.resize(len, 0);
    r.read_exact(buf)?;
    postcard::from_bytes(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Where the server listens.
#[derive(Debug, Clone)]
pub struct SocketAddr {
    kind: AddrKind,
}

#[derive(Debug, Clone)]
enum AddrKind {
    Path(PathBuf),
    Namespaced(String),
}

impl SocketAddr {
    /// The socket location, from the first of: `cli` (a `--socket` flag),
    /// `$AVRS_SOCKET`, `configured` (the config file), or the default:
    ///
    /// - Unix: `$XDG_RUNTIME_DIR/avrs.sock` (private to the user), falling back
    ///   to `avrs-$USER.sock` in the temp directory.
    /// - Windows: the named pipe `\\.\pipe\avrs-%USERNAME%`.
    pub fn resolve(cli: Option<&str>, configured: Option<&str>) -> Self {
        match cli
            .map(str::to_owned)
            .or_else(|| std::env::var(SOCKET_ENV).ok())
            .or_else(|| configured.map(str::to_owned))
        {
            Some(s) if cfg!(windows) => Self::namespaced(s.trim_start_matches(r"\\.\pipe\")),
            Some(s) => Self::path(PathBuf::from(s)),
            None => Self::default_addr(),
        }
    }

    #[cfg(windows)]
    fn default_addr() -> Self {
        let user = std::env::var("USERNAME").unwrap_or_default();
        Self::namespaced(&format!("avrs-{user}"))
    }

    #[cfg(not(windows))]
    fn default_addr() -> Self {
        match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(dir) if !dir.is_empty() => Self::path(PathBuf::from(dir).join("avrs.sock")),
            _ => {
                let user = std::env::var("USER").unwrap_or_default();
                Self::path(std::env::temp_dir().join(format!("avrs-{user}.sock")))
            }
        }
    }

    fn path(path: PathBuf) -> Self {
        Self {
            kind: AddrKind::Path(path),
        }
    }

    fn namespaced(name: &str) -> Self {
        Self {
            kind: AddrKind::Namespaced(name.to_owned()),
        }
    }

    pub fn name(&self) -> io::Result<Name<'_>> {
        match &self.kind {
            AddrKind::Path(p) => p.as_path().to_fs_name::<GenericFilePath>(),
            AddrKind::Namespaced(n) => n.as_str().to_ns_name::<GenericNamespaced>(),
        }
    }

    /// Removes a leftover socket file. Named pipes need no cleanup.
    pub fn remove(&self) {
        if let AddrKind::Path(p) = &self.kind {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Connects as a client.
    pub fn connect(&self) -> io::Result<interprocess::local_socket::Stream> {
        interprocess::local_socket::Stream::connect(self.name()?)
    }
}

impl std::fmt::Display for SocketAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            AddrKind::Path(p) => write!(f, "{}", p.display()),
            AddrKind::Namespaced(n) => write!(f, r"\\.\pipe\{n}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_messages() {
        let msgs = [
            ServerMsg::Hello {
                version: PROTOCOL_VERSION,
                bands: 256,
                rate: 60,
            },
            ServerMsg::Spectrum {
                seq: 7,
                mags: vec![0.0, 0.5, 1.0],
                position: Some(12.5),
            },
            ServerMsg::Status(Status {
                title: "playing: x.mp3".into(),
                paused: true,
                has_playlist: true,
                finished: false,
                delay_ms: 230,
                extra_delay_ms: 30,
                duration: Some(181.0),
            }),
        ];
        let mut wire = Vec::new();
        for m in &msgs {
            write_msg(&mut wire, m).unwrap();
        }
        write_msg(&mut wire, &ClientMsg::Seek(42.0)).unwrap();
        write_msg(&mut wire, &ClientMsg::Next).unwrap();

        let mut r = wire.as_slice();
        let mut buf = Vec::new();
        for m in &msgs {
            assert_eq!(&read_msg::<_, ServerMsg>(&mut r, &mut buf).unwrap(), m);
        }
        assert_eq!(
            read_msg::<_, ClientMsg>(&mut r, &mut buf).unwrap(),
            ClientMsg::Seek(42.0)
        );
        assert_eq!(
            read_msg::<_, ClientMsg>(&mut r, &mut buf).unwrap(),
            ClientMsg::Next
        );
        assert!(read_msg::<_, ClientMsg>(&mut r, &mut buf).is_err(), "eof");
    }

    #[test]
    fn rejects_oversized_frames() {
        let wire = u32::MAX.to_le_bytes();
        let err = read_msg::<_, ClientMsg>(&mut wire.as_slice(), &mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn explicit_socket_wins() {
        let addr = SocketAddr::resolve(Some("x.sock"), Some("ignored.sock"));
        if cfg!(windows) {
            assert_eq!(addr.to_string(), r"\\.\pipe\x.sock");
        } else {
            assert_eq!(addr.to_string(), "x.sock");
        }
    }
}
