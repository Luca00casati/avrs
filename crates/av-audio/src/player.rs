//! Playlist playback to the default output, with an analysis tap.
//!
//! A decoder thread decodes, converts and pushes interleaved frames into a
//! ring buffer. The output callback plays from it and copies a mono downmix
//! of exactly what it played into the tap, so the spectrum stays in sync with
//! the sound however far ahead the decoder runs.
//!
//! Track changes flush the ring: the decoder bumps `flush_req` and waits until
//! the callback has discarded everything queued and echoed it in `flush_ack`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample};

use crate::convert::Converter;
use crate::decode::Decoder;
use crate::devices::low_latency_configs;
use crate::sink_latency::SinkLatency;

/// Extensions treated as audio when expanding directories.
const AUDIO_EXTENSIONS: &[&str] = &[
    "aac", "aif", "aiff", "caf", "flac", "m4a", "mka", "mp3", "oga", "ogg", "wav",
];
/// Audio queued ahead of the output.
const QUEUE_SECS: f32 = 0.5;
/// Requested output callback period (the device buffer holds two). Like the
/// capture period, kept above PipeWire's default quantum so playing a file
/// doesn't shrink every other app's buffers.
const OUTPUT_PERIOD: Duration = Duration::from_millis(50);
/// How long the decoder sleeps when the queue is full.
const BACKOFF: Duration = Duration::from_millis(5);

/// Expands files and directories into a playlist. Directory entries are
/// sorted by name; non-audio files inside directories are skipped.
pub fn collect_playlist(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut playlist = Vec::new();
    for path in paths {
        if path.is_dir() {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(path)
                .with_context(|| format!("cannot read {}", path.display()))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && is_audio_file(p))
                .collect();
            entries.sort();
            playlist.extend(entries);
        } else if path.is_file() {
            playlist.push(path.clone());
        } else {
            bail!("no such file or directory: {}", path.display());
        }
    }
    if playlist.is_empty() {
        bail!("no audio files found");
    }
    Ok(playlist)
}

fn is_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.iter().any(|a| a.eq_ignore_ascii_case(e)))
}

enum Command {
    /// Jump to a track; `true` if moving backwards through the playlist.
    Play(usize, bool),
    Stop,
}

/// State shared between the player, the decoder thread and the output callback.
#[derive(Default)]
struct Shared {
    paused: AtomicBool,
    /// Frames the decoder has queued, ever.
    queued: AtomicU64,
    /// Frames the callback has taken off the queue (played or flushed), ever.
    consumed: AtomicU64,
    flush_req: AtomicU64,
    flush_ack: AtomicU64,
    /// The decoder reached the end of the playlist.
    exhausted: AtomicBool,
    failed: AtomicBool,
    /// Time from the output callback until its audio is heard, in microseconds,
    /// as reported by the backend (includes e.g. Bluetooth transport).
    latency_us: AtomicU64,
    /// `(queued frame, track)` where each track starts, oldest first.
    starts: Mutex<VecDeque<(u64, usize)>>,
}

/// Plays a playlist on the default output device.
pub struct Player {
    // Field order matters: stop the stream before the decoder thread.
    stream: cpal::Stream,
    commands: Sender<Command>,
    decoder: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    tap: rtrb::Consumer<f32>,
    playlist: Arc<[PathBuf]>,
    current: usize,
    looping: bool,
    sample_rate: u32,
    sink_latency: SinkLatency,
}

impl Player {
    pub fn new(playlist: Vec<PathBuf>, looping: bool) -> Result<Self> {
        assert!(!playlist.is_empty(), "playlist must not be empty");
        let playlist: Arc<[PathBuf]> = playlist.into();

        let device = cpal::default_host()
            .default_output_device()
            .context("no default output device")?;
        let config = device
            .default_output_config()
            .context("no usable output config")?;
        let sample_rate = config.sample_rate();
        let channels = usize::from(config.channels());
        let sink_latency = match device.id() {
            Ok(id) => SinkLatency::for_sink(id.host(), id.id()),
            Err(_) => SinkLatency::none(),
        };

        let queue_len = (sample_rate as f32 * QUEUE_SECS) as usize * channels;
        let shared = Arc::new(Shared::default());

        // A short device buffer keeps pause and track changes immediate and
        // the reported latency honest; fall back to the default if refused.
        let mut attempt = Err(anyhow::anyhow!("no stream config to try"));
        for stream_config in low_latency_configs(&config, OUTPUT_PERIOD) {
            let (queue_tx, queue_rx) = rtrb::RingBuffer::new(queue_len);
            let (tap_tx, tap_rx) = rtrb::RingBuffer::new(sample_rate as usize);
            attempt = build_output(
                &device,
                config.sample_format(),
                stream_config,
                channels,
                queue_rx,
                tap_tx,
                shared.clone(),
            )
            .map(|stream| (stream, queue_tx, tap_rx));
            if attempt.is_ok() {
                break;
            }
        }
        let (stream, queue_tx, tap_rx) = attempt?;
        stream.play().context("failed to start output stream")?;

        let (commands, rx) = mpsc::channel();
        let decoder = {
            let worker = DecoderThread {
                playlist: playlist.clone(),
                looping,
                out_rate: sample_rate,
                out_channels: channels,
                queue: queue_tx,
                shared: shared.clone(),
                commands: rx,
            };
            thread::Builder::new()
                .name("av-decoder".into())
                .spawn(move || worker.run())?
        };

        Ok(Self {
            stream,
            commands,
            decoder: Some(decoder),
            shared,
            tap: tap_rx,
            playlist,
            current: 0,
            looping,
            sample_rate,
            sink_latency,
        })
    }

    /// Appends the mono samples played since the last call to `out`.
    pub fn drain(&mut self, out: &mut Vec<f32>) {
        let Ok(chunk) = self.tap.read_chunk(self.tap.slots()) else {
            return;
        };
        let (a, b) = chunk.as_slices();
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        chunk.commit_all();
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Index of the track being heard.
    pub fn current(&mut self) -> usize {
        let consumed = self.shared.consumed.load(Ordering::Acquire);
        let mut starts = self.shared.starts.lock().unwrap();
        while let Some(&(at, track)) = starts.front() {
            if at > consumed {
                break;
            }
            self.current = track;
            starts.pop_front();
        }
        self.current
    }

    pub fn playlist(&self) -> &[PathBuf] {
        &self.playlist
    }

    /// File name of the track being heard.
    pub fn title(&mut self) -> String {
        let current = self.current();
        let path = &self.playlist[current];
        path.file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }

    pub fn is_paused(&self) -> bool {
        self.shared.paused.load(Ordering::Relaxed)
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    pub fn next(&mut self) {
        let next = self.current() + 1;
        if next < self.playlist.len() {
            self.jump(next, false);
        } else if self.looping {
            self.jump(0, false);
        }
    }

    pub fn previous(&mut self) {
        let prev = match self.current() {
            0 if self.looping => self.playlist.len() - 1,
            0 => 0,
            n => n - 1,
        };
        self.jump(prev, true);
    }

    /// Jumps to a track by index.
    pub fn play(&mut self, track: usize) {
        self.jump(track, false);
    }

    fn jump(&mut self, track: usize, backward: bool) {
        let _ = self.commands.send(Command::Play(track, backward));
    }

    /// Everything has been played and the playlist does not loop.
    pub fn is_finished(&self) -> bool {
        self.shared.exhausted.load(Ordering::Acquire)
            && self.shared.consumed.load(Ordering::Acquire)
                >= self.shared.queued.load(Ordering::Acquire)
    }

    /// How long after being tapped for analysis the audio is actually heard:
    /// what the backend reports, plus any delay the sink is known to hide
    /// (e.g. Bluetooth headsets without delay reporting).
    pub fn output_latency(&self) -> Duration {
        Duration::from_micros(self.shared.latency_us.load(Ordering::Relaxed))
            + self.sink_latency.unreported()
    }

    /// Whether the output stream reported an error (for example, the device went away).
    pub fn has_failed(&self) -> bool {
        self.shared.failed.load(Ordering::Relaxed)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.stream.pause();
        let _ = self.commands.send(Command::Stop);
        if let Some(handle) = self.decoder.take() {
            let _ = handle.join();
        }
    }
}

struct DecoderThread {
    playlist: Arc<[PathBuf]>,
    looping: bool,
    out_rate: u32,
    out_channels: usize,
    queue: rtrb::Producer<f32>,
    shared: Arc<Shared>,
    commands: Receiver<Command>,
}

/// What interrupted queueing a track.
enum Interrupt {
    Play(usize, bool),
    Stop,
}

impl DecoderThread {
    fn run(mut self) {
        let len = self.playlist.len();
        let mut track = 0;
        // Direction to skip unplayable tracks in: the way the user last moved.
        let mut backward = false;
        // Consecutive tracks that failed, to stop on an all-bad playlist.
        let mut failures = 0;
        loop {
            match self.play_track(track) {
                Ok(None) => {
                    failures = 0;
                    backward = false;
                }
                Ok(Some(Interrupt::Play(next, back))) => {
                    if !self.flush() {
                        return;
                    }
                    failures = 0;
                    (track, backward) = (next.min(len - 1), back);
                    continue;
                }
                Ok(Some(Interrupt::Stop)) => return,
                Err(err) => {
                    eprintln!("skipping {}: {err:#}", self.playlist[track].display());
                    failures += 1;
                    if failures >= len {
                        self.shared.exhausted.store(true, Ordering::Release);
                        return;
                    }
                }
            }

            if let Some(next) = self.step(track, backward) {
                track = next;
            } else if backward {
                // Hit the start going backwards: search forwards instead.
                backward = false;
                match self.step(track, false) {
                    Some(next) => track = next,
                    None => {
                        self.shared.exhausted.store(true, Ordering::Release);
                        return;
                    }
                }
            } else {
                // End of a non-looping playlist. Stay responsive to "previous".
                self.shared.exhausted.store(true, Ordering::Release);
                match self.commands.recv() {
                    Ok(Command::Play(next, back)) => {
                        self.shared.exhausted.store(false, Ordering::Release);
                        if !self.flush() {
                            return;
                        }
                        failures = 0;
                        (track, backward) = (next.min(len - 1), back);
                    }
                    Ok(Command::Stop) | Err(_) => return,
                }
            }
        }
    }

    /// The track after (or before) `track`, wrapping only when looping.
    fn step(&self, track: usize, backward: bool) -> Option<usize> {
        let len = self.playlist.len();
        match (backward, self.looping) {
            (false, _) if track + 1 < len => Some(track + 1),
            (false, true) => Some(0),
            (true, _) if track > 0 => Some(track - 1),
            (true, true) => Some(len - 1),
            _ => None,
        }
    }

    /// Decodes and queues one track. Returns early if a command arrives.
    fn play_track(&mut self, index: usize) -> Result<Option<Interrupt>> {
        let mut decoder = Decoder::open(&self.playlist[index])?;
        let mut converter = Converter::new(
            decoder.sample_rate(),
            decoder.channels(),
            self.out_rate,
            self.out_channels,
        )?;

        let start = self.shared.queued.load(Ordering::Acquire);
        self.shared.starts.lock().unwrap().push_back((start, index));

        let mut decoded = Vec::new();
        let mut converted = Vec::new();
        loop {
            if let Some(stop) = self.poll_command() {
                return Ok(Some(stop));
            }
            converted.clear();
            if decoder.next_chunk(&mut decoded)? {
                converter.process(&decoded, &mut converted)?;
            } else {
                converter.finish(&mut converted)?;
                if let Some(stop) = self.enqueue(&converted) {
                    return Ok(Some(stop));
                }
                return Ok(None);
            }
            if let Some(stop) = self.enqueue(&converted) {
                return Ok(Some(stop));
            }
        }
    }

    /// Pushes frames, waiting for space. Returns early if a command arrives.
    fn enqueue(&mut self, mut samples: &[f32]) -> Option<Interrupt> {
        let ch = self.out_channels;
        while !samples.is_empty() {
            // Push whole frames only, so the callback never sees half a frame.
            let room = self.queue.slots() / ch * ch;
            if room == 0 {
                if let Some(stop) = self.wait_command(BACKOFF) {
                    return Some(stop);
                }
                continue;
            }
            let n = room.min(samples.len());
            let chunk = self
                .queue
                .write_chunk_uninit(n)
                .expect("checked free slots");
            chunk.fill_from_iter(samples[..n].iter().copied());
            self.shared
                .queued
                .fetch_add((n / ch) as u64, Ordering::Release);
            samples = &samples[n..];
        }
        None
    }

    /// Asks the callback to drop everything queued and waits until it has.
    /// Returns `false` if told to stop meanwhile.
    fn flush(&mut self) -> bool {
        let req = self.shared.flush_req.fetch_add(1, Ordering::AcqRel) + 1;
        self.shared.starts.lock().unwrap().clear();
        while self.shared.flush_ack.load(Ordering::Acquire) < req {
            if let Some(Interrupt::Stop) = self.wait_command(BACKOFF) {
                return false;
            }
        }
        true
    }

    fn poll_command(&mut self) -> Option<Interrupt> {
        self.wait_command(Duration::ZERO)
    }

    fn wait_command(&mut self, timeout: Duration) -> Option<Interrupt> {
        let cmd = if timeout.is_zero() {
            self.commands.try_recv().ok()
        } else {
            match self.commands.recv_timeout(timeout) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => Some(Command::Stop),
            }
        };
        cmd.map(|c| match c {
            Command::Play(n, back) => Interrupt::Play(n, back),
            Command::Stop => Interrupt::Stop,
        })
    }
}

fn build_output(
    device: &cpal::Device,
    format: SampleFormat,
    config: cpal::StreamConfig,
    channels: usize,
    queue: rtrb::Consumer<f32>,
    tap: rtrb::Producer<f32>,
    shared: Arc<Shared>,
) -> Result<cpal::Stream> {
    macro_rules! build {
        ($t:ty) => {
            build_typed::<$t>(device, config, channels, queue, tap, shared)
        };
    }
    let stream = match format {
        SampleFormat::I8 => build!(i8),
        SampleFormat::I16 => build!(i16),
        SampleFormat::I32 => build!(i32),
        SampleFormat::U8 => build!(u8),
        SampleFormat::U16 => build!(u16),
        SampleFormat::U32 => build!(u32),
        SampleFormat::F32 => build!(f32),
        SampleFormat::F64 => build!(f64),
        other => bail!("unsupported sample format {other}"),
    };
    stream.context("failed to open output stream")
}

fn build_typed<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    channels: usize,
    mut queue: rtrb::Consumer<f32>,
    mut tap: rtrb::Producer<f32>,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let inv = 1.0 / channels as f32;
    let error_shared = shared.clone();
    device.build_output_stream::<T, _, _>(
        config,
        move |out: &mut [T], info: &cpal::OutputCallbackInfo| {
            let ts = info.timestamp();
            let latency = ts.playback.saturating_duration_since(ts.callback);
            shared
                .latency_us
                .store(latency.as_micros() as u64, Ordering::Relaxed);

            // Discard everything queued if the decoder asked for a flush.
            let req = shared.flush_req.load(Ordering::Acquire);
            if shared.flush_ack.load(Ordering::Relaxed) != req {
                let queued = queue.slots();
                if let Ok(chunk) = queue.read_chunk(queued) {
                    chunk.commit_all();
                }
                shared
                    .consumed
                    .fetch_add((queued / channels) as u64, Ordering::Release);
                shared.flush_ack.store(req, Ordering::Release);
            }

            // `out` arrives filled with silence; leave it so when paused or starved.
            if shared.paused.load(Ordering::Relaxed) {
                return;
            }
            let frames = (out.len() / channels).min(queue.slots() / channels);
            let Ok(chunk) = queue.read_chunk(frames * channels) else {
                return;
            };
            let (a, b) = chunk.as_slices();
            let mut samples = a.iter().chain(b);
            for frame in out[..frames * channels].chunks_exact_mut(channels) {
                let mut sum = 0.0;
                for slot in frame {
                    let s = *samples.next().expect("whole frames");
                    sum += s;
                    *slot = T::from_sample(s);
                }
                // Visualization only: drop samples if nobody is reading.
                let _ = tap.push(sum * inv);
            }
            chunk.commit_all();
            shared.consumed.fetch_add(frames as u64, Ordering::Release);
        },
        move |err| {
            eprintln!("audio output error: {err}");
            error_shared.failed.store(true, Ordering::Relaxed);
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::tests::{temp_dir, write_wav};

    #[test]
    fn playlist_expands_sorted_audio_files() {
        let dir = temp_dir("playlist");
        write_wav(&dir, "b.wav", 8_000, 1, 10);
        write_wav(&dir, "A.WAV", 8_000, 1, 10);
        std::fs::write(dir.join("cover.jpg"), b"").unwrap();
        std::fs::create_dir_all(dir.join("sub.mp3")).unwrap();
        let single = write_wav(&std::env::temp_dir(), "av-audio-single.wav", 8_000, 1, 10);

        let list = collect_playlist(&[single.clone(), dir.clone()]).unwrap();
        let names: Vec<_> = list
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, ["av-audio-single.wav", "A.WAV", "b.wav"]);

        assert!(collect_playlist(&[dir.join("missing.wav")]).is_err());
        std::fs::remove_file(dir.join("A.WAV")).unwrap();
        std::fs::remove_file(dir.join("b.wav")).unwrap();
        assert!(
            collect_playlist(std::slice::from_ref(&dir)).is_err(),
            "no audio files"
        );

        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_file(single).unwrap();
    }
}
