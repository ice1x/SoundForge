//! Playback (task 14): a `cpal` output stream fed from the memory-mapped PCM cache through
//! an `rtrb` ring buffer.
//!
//! ## Why a ring buffer at all, when the samples are already in memory?
//!
//! The PCM cache is an `mmap`, so reading it can page-fault — and a page fault in the audio
//! callback is a dropout. The callback must not fault, lock, or allocate, so it only ever
//! pops from a lock-free SPSC ring that a normal feeder thread keeps full. That feeder is the
//! only thing that touches the map.
//!
//! ## Threading
//!
//! `cpal::Stream` is `!Send` on CoreAudio, and [`Player`] is `.manage`d by Tauri (`Send +
//! Sync`), so the stream cannot live in the struct. Instead each playback owns a thread that
//! builds the stream, keeps it alive, feeds the ring, and answers commands over an
//! `mpsc` channel. [`Player`] holds only the channel, a [`Shared`] block of atomics, and the
//! join handle.
//!
//! ## Testability
//!
//! Everything that decides *what is heard* is a plain function over plain data — [`Source`]
//! (channel mapping + resampling), [`pump`]/[`render`] (the two ends of the ring), and
//! [`pick_config`] (device choice) — so it is unit-tested without any audio hardware. Only
//! the thread that wires them to a real device needs a sound card, and its test is skipped
//! where there is none (CI runners have no output device).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use sf_core::PcmCache;

/// How much audio the ring holds. Long enough that a descheduled feeder thread cannot
/// starve the callback, short enough that `pause` and `stop` feel immediate — everything
/// already in the ring has to drain before a change is heard.
const RING_SECONDS: f64 = 0.25;

/// How long the playback thread blocks waiting for a command before topping the ring up.
/// Well inside [`RING_SECONDS`], so the ring never runs dry between refills.
const FEED_INTERVAL: Duration = Duration::from_millis(5);

/// Anything that can stop playback from starting.
#[derive(Debug)]
pub enum PlayError {
    /// No file is open, so there is nothing to play.
    NoDocument,
    /// The host reported no default output device.
    NoDevice,
    /// The device advertises no `f32` output config. See [`pick_config`].
    NoF32Config,
    /// The requested range contains no samples.
    EmptyRange,
    /// The device rejected the stream (opening, starting, or querying it).
    Device(String),
}

impl std::fmt::Display for PlayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlayError::NoDocument => write!(f, "no audio file is open"),
            PlayError::NoDevice => write!(f, "no audio output device available"),
            PlayError::NoF32Config => {
                write!(f, "output device supports no 32-bit float configuration")
            }
            PlayError::EmptyRange => write!(f, "nothing to play: the range is empty"),
            PlayError::Device(e) => write!(f, "audio device error: {e}"),
        }
    }
}

impl std::error::Error for PlayError {}

/// What the transport is doing. `Finished` is distinct from `Stopped` so the UI can tell
/// "ran to the end of the selection" from "the user pressed stop".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
    Finished,
}

impl PlayState {
    fn as_u8(self) -> u8 {
        match self {
            PlayState::Stopped => 0,
            PlayState::Playing => 1,
            PlayState::Paused => 2,
            PlayState::Finished => 3,
        }
    }

    fn from_u8(v: u8) -> PlayState {
        match v {
            1 => PlayState::Playing,
            2 => PlayState::Paused,
            3 => PlayState::Finished,
            _ => PlayState::Stopped,
        }
    }
}

/// Transport state for the UI: what is playing, and where the playhead is.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackDto {
    pub state: PlayState,
    /// Playhead in source frames — what has actually reached the device, not what has been
    /// fed to the ring.
    pub position_frames: u64,
    pub position_s: f64,
    /// The range being played, in source frames.
    pub start_frame: u64,
    pub end_frame: u64,
    /// Callbacks that found the ring short. Non-zero means audible dropouts.
    pub underruns: u64,
}

/// State shared between the audio callback, the feeder thread and the UI.
///
/// Only the callback writes `played`; only the feeder writes `feed_done`; both may write
/// `state`. All of it is atomic because the callback is realtime and must never block.
pub struct Shared {
    state: AtomicU8,
    /// Device frames handed to the device so far. The playhead derives from this rather
    /// than from the feeder's cursor, which runs up to [`RING_SECONDS`] ahead of the sound.
    played: AtomicU64,
    /// Set by the feeder once the source is exhausted.
    feed_done: AtomicBool,
    underruns: AtomicU64,
    /// Immutable after construction — set before the `Arc` is shared.
    dev_channels: usize,
    start_frame: u64,
    end_frame: u64,
    /// Source frames per device frame.
    ratio: f64,
    sample_rate: u32,
}

impl Shared {
    fn state(&self) -> PlayState {
        PlayState::from_u8(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, s: PlayState) {
        self.state.store(s.as_u8(), Ordering::Release);
    }

    /// The playhead, in source frames, clamped to the range being played.
    pub fn position_frames(&self) -> u64 {
        let advanced = (self.played.load(Ordering::Relaxed) as f64 * self.ratio) as u64;
        self.start_frame
            .saturating_add(advanced)
            .min(self.end_frame)
    }

    fn dto(&self) -> PlaybackDto {
        let position_frames = self.position_frames();
        PlaybackDto {
            state: self.state(),
            position_frames,
            position_s: position_frames as f64 / self.sample_rate as f64,
            start_frame: self.start_frame,
            end_frame: self.end_frame,
            underruns: self.underruns.load(Ordering::Relaxed),
        }
    }
}

/// Pulls interleaved device frames out of the PCM cache: the whole of "what is heard".
///
/// Two conversions happen here, both needed because the device rarely matches the file:
/// channel mapping (a mono file on a stereo device, a 5.1 file on a stereo device) and
/// resampling (a 44.1 kHz file on a 48 kHz device — the common case on macOS).
pub struct Source {
    pcm: Arc<PcmCache>,
    /// Exclusive end of the range, in source frames.
    end: usize,
    dev_channels: usize,
    /// Fractional read cursor in source frames. Fractional because of resampling.
    pos: f64,
    /// Source frames per device frame. 1.0 when the rates match, and then every read lands
    /// exactly on a sample (`frac == 0`), so a matched-rate file is bit-exact.
    ratio: f64,
}

impl Source {
    /// Play `[start, end)` of `pcm` on a device with `dev_channels` channels at `dev_rate`.
    /// The range is assumed already clamped to the document; see [`Player::play`].
    pub fn new(
        pcm: Arc<PcmCache>,
        start: usize,
        end: usize,
        dev_channels: usize,
        dev_rate: u32,
    ) -> Source {
        debug_assert!(dev_channels > 0 && dev_rate > 0);
        debug_assert!(end <= pcm.frames() && start <= end);
        let ratio = pcm.sample_rate() as f64 / dev_rate as f64;
        Source {
            pcm,
            end,
            dev_channels,
            pos: start as f64,
            ratio,
        }
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// True once the range has been fully read.
    pub fn finished(&self) -> bool {
        self.pos >= self.end as f64
    }

    /// Fill `out` with interleaved device frames, returning how many *frames* were written.
    ///
    /// A short return means the range ran out; `out` beyond that is untouched. Any trailing
    /// partial frame in `out` is ignored (cpal always asks for whole frames).
    ///
    /// Called only from the feeder thread, never the audio callback — which is why it is
    /// allowed the small per-call `Vec` of channel slices below.
    pub fn fill(&mut self, out: &mut [f32]) -> usize {
        let file_ch = self.pcm.channels();
        // `PcmCache::channel` re-slices the map each call; hoist it out of the sample loop.
        // One tiny allocation per ~5 ms refill, on a thread where that is fine.
        let chans: Vec<&[f32]> = (0..file_ch).map(|c| self.pcm.channel(c)).collect();
        // Downmix only when the device is mono and the file is not: dropping every channel
        // but the first would silently lose half a stereo mix.
        let downmix = self.dev_channels == 1 && file_ch > 1;

        let mut frames = 0;
        for frame in out.chunks_exact_mut(self.dev_channels) {
            if self.pos >= self.end as f64 {
                break;
            }
            let i0 = self.pos as usize;
            let frac = (self.pos - i0 as f64) as f32;
            // Hold the last sample rather than reading past the range: `i0` can be the final
            // frame, and interpolating into the next one would read outside `[start, end)`.
            let i1 = (i0 + 1).min(self.end - 1);

            if downmix {
                let mut acc = 0.0;
                for s in &chans {
                    acc += lerp(s[i0], s[i1], frac);
                }
                frame[0] = acc / file_ch as f32;
            } else {
                for (c, o) in frame.iter_mut().enumerate() {
                    // Mono file on a stereo device duplicates; a file with more channels
                    // than the device keeps the first `dev_channels` of them.
                    let s = chans[c % file_ch];
                    *o = lerp(s[i0], s[i1], frac);
                }
            }
            self.pos += self.ratio;
            frames += 1;
        }
        frames
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Top the ring up from `source`. Runs on the feeder thread.
///
/// Only ever writes whole device frames, which is what keeps the ring frame-aligned for
/// [`render`] — see [`ring_capacity`].
pub fn pump(producer: &mut rtrb::Producer<f32>, source: &mut Source, shared: &Shared) {
    let ch = shared.dev_channels;
    let free = (producer.slots() / ch) * ch;
    if free == 0 {
        return;
    }
    // `free <= slots`, so this cannot fail.
    let mut chunk = producer
        .write_chunk(free)
        .expect("write_chunk within available slots");
    let (a, b) = chunk.as_mut_slices();
    debug_assert!(a.len() % ch == 0, "ring wrap must land on a frame boundary");
    let mut n = source.fill(a) * ch;
    // Only continue into the wrapped half if the first was filled completely; a short fill
    // means the source ran out.
    if n == a.len() {
        n += source.fill(b) * ch;
    }
    chunk.commit(n);
    if source.finished() {
        shared.feed_done.store(true, Ordering::Release);
    }
}

/// The audio callback body: move whole frames from the ring into the device buffer.
///
/// Realtime-safe — no locks, no allocation, no I/O. Anything it cannot fill becomes silence
/// rather than stale audio.
pub fn render(consumer: &mut rtrb::Consumer<f32>, out: &mut [f32], shared: &Shared) {
    let ch = shared.dev_channels;
    let want = out.len();
    let got = (consumer.slots().min(want) / ch) * ch;

    if got > 0 {
        let chunk = consumer
            .read_chunk(got)
            .expect("read_chunk within available slots");
        let (a, b) = chunk.as_slices();
        out[..a.len()].copy_from_slice(a);
        out[a.len()..got].copy_from_slice(&b[..got - a.len()]);
        chunk.commit_all();
        shared
            .played
            .fetch_add((got / ch) as u64, Ordering::Relaxed);
    }

    if got < want {
        out[got..].fill(0.0);
        // A short read is only an underrun if there was supposed to be more. Once the feeder
        // is done and the ring has drained, the same condition means the range finished.
        if shared.feed_done.load(Ordering::Acquire) && consumer.is_empty() {
            shared.set_state(PlayState::Finished);
        } else {
            shared.underruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Ring size in samples for a device: [`RING_SECONDS`] of audio, rounded to whole frames.
///
/// The frame-alignment is load-bearing, not tidiness. `rtrb` splits its slices at the
/// capacity boundary, so a capacity that is a whole number of frames — combined with a
/// producer and consumer that only ever commit whole frames — guarantees the split itself
/// lands on a frame boundary. Otherwise a wrap would rotate the channels and swap the stereo
/// image for the rest of the stream.
pub fn ring_capacity(dev_rate: u32, dev_channels: usize) -> usize {
    let frames = (dev_rate as f64 * RING_SECONDS).ceil() as usize;
    frames.max(1) * dev_channels
}

/// What [`pick_config`] needs to know about one of a device's supported output configs.
///
/// A plain mirror of `cpal::SupportedStreamConfigRange` so the choice can be tested against
/// synthetic device descriptions rather than whatever sound card the test machine has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigRange {
    pub channels: u16,
    pub min_rate: u32,
    pub max_rate: u32,
    /// Whether this range delivers `f32` samples.
    pub is_f32: bool,
}

/// A chosen output config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chosen {
    pub channels: u16,
    pub sample_rate: u32,
}

/// Choose an output config for a source at `src_rate`.
///
/// Preference order: play at the file's own rate if the device allows it (that makes
/// [`Source`] a straight copy — no interpolation, no quality loss), then a channel count
/// closest to stereo, then fewer channels. When no range covers the file's rate, the nearest
/// supported rate is used and [`Source`] resamples.
///
/// `f32`-only is deliberate: CoreAudio is natively `f32`, and this project is
/// Apple-Silicon-first, so supporting `i16`/`u16` would be untested code. Elsewhere a device
/// with no `f32` range reports [`PlayError::NoF32Config`] rather than playing noise.
pub fn pick_config(ranges: &[ConfigRange], src_rate: u32) -> Option<Chosen> {
    ranges
        .iter()
        .filter(|r| r.is_f32 && r.channels >= 1 && r.min_rate <= r.max_rate)
        .map(|r| {
            let sample_rate = src_rate.clamp(r.min_rate, r.max_rate);
            let key = (
                u8::from(sample_rate != src_rate),
                (i32::from(r.channels) - 2).unsigned_abs(),
                r.channels,
            );
            (
                key,
                Chosen {
                    channels: r.channels,
                    sample_rate,
                },
            )
        })
        .min_by_key(|(k, _)| *k)
        .map(|(_, c)| c)
}

/// Commands to the playback thread.
enum Cmd {
    Pause,
    Resume,
    Stop,
}

/// A live playback: the thread that owns the stream, plus what the UI can see of it.
struct Active {
    cmd: mpsc::Sender<Cmd>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

/// The shell's playback state, `.manage`d by Tauri. At most one playback at a time.
#[derive(Default)]
pub struct Player {
    active: Mutex<Option<Active>>,
}

impl Player {
    /// Lock the active playback, recovering from a poisoned mutex.
    ///
    /// Same reasoning as `AudioState::lock`: the guarded value is a plain `Option` only ever
    /// swapped wholesale, so a panic elsewhere cannot leave it half-updated, and refusing
    /// every later command would break the transport for no safety gain.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The transport state when nothing is playing.
    fn idle_dto() -> PlaybackDto {
        PlaybackDto {
            state: PlayState::Stopped,
            position_frames: 0,
            position_s: 0.0,
            start_frame: 0,
            end_frame: 0,
            underruns: 0,
        }
    }

    /// Start playing `[start, end)` of `pcm`, replacing any current playback.
    ///
    /// `pcm` is an `Arc` clone of the open document's cache, taken once here: the audio path
    /// then never touches the document lock, and the map stays valid even if the document is
    /// closed mid-playback — the cache file is unlinked, but POSIX keeps a mapped file alive
    /// until it is unmapped.
    ///
    /// The range is clamped to the document; an empty one is [`PlayError::EmptyRange`].
    pub fn play(
        &self,
        pcm: Arc<PcmCache>,
        start: usize,
        end: usize,
    ) -> Result<PlaybackDto, PlayError> {
        self.stop();

        let frames = pcm.frames();
        let start = start.min(frames);
        let end = end.min(frames);
        if start >= end {
            return Err(PlayError::EmptyRange);
        }

        let device = cpal::default_host()
            .default_output_device()
            .ok_or(PlayError::NoDevice)?;
        let ranges: Vec<ConfigRange> = device
            .supported_output_configs()
            .map_err(|e| PlayError::Device(e.to_string()))?
            .map(|r| ConfigRange {
                channels: r.channels(),
                min_rate: r.min_sample_rate(),
                max_rate: r.max_sample_rate(),
                is_f32: r.sample_format() == cpal::SampleFormat::F32,
            })
            .collect();
        let chosen = pick_config(&ranges, pcm.sample_rate()).ok_or(PlayError::NoF32Config)?;

        let source = Source::new(
            Arc::clone(&pcm),
            start,
            end,
            chosen.channels as usize,
            chosen.sample_rate,
        );
        let shared = Arc::new(Shared {
            state: AtomicU8::new(PlayState::Stopped.as_u8()),
            played: AtomicU64::new(0),
            feed_done: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            dev_channels: chosen.channels as usize,
            start_frame: start as u64,
            end_frame: end as u64,
            ratio: source.ratio(),
            sample_rate: pcm.sample_rate(),
        });

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("soundforge-playback".into())
            .spawn(move || {
                playback_thread(device, chosen, source, thread_shared, cmd_rx, ready_tx);
            })
            .map_err(|e| PlayError::Device(format!("could not start playback thread: {e}")))?;

        // Surface a device failure to the caller instead of leaving a dead thread behind.
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(PlayError::Device("playback thread died on startup".into()));
            }
        }

        let dto = shared.dto();
        *self.lock() = Some(Active {
            cmd: cmd_tx,
            shared,
            thread: Some(thread),
        });
        Ok(dto)
    }

    /// Send `cmd` and report the resulting state. A no-op when nothing is playing.
    fn command(&self, cmd: Cmd, state: PlayState) -> PlaybackDto {
        let guard = self.lock();
        match guard.as_ref() {
            Some(a) => {
                // The thread sets the state too, but doing it here as well means `status`
                // reflects a pause immediately rather than after the thread wakes up.
                if a.cmd.send(cmd).is_ok() && a.shared.state() != PlayState::Finished {
                    a.shared.set_state(state);
                }
                a.shared.dto()
            }
            None => Player::idle_dto(),
        }
    }

    /// Pause at the current position. The device keeps the stream open.
    pub fn pause(&self) -> PlaybackDto {
        self.command(Cmd::Pause, PlayState::Paused)
    }

    /// Resume a paused playback.
    pub fn resume(&self) -> PlaybackDto {
        self.command(Cmd::Resume, PlayState::Playing)
    }

    /// Stop playback and release the device. Idempotent.
    pub fn stop(&self) {
        let active = self.lock().take();
        if let Some(mut a) = active {
            // A closed channel also means "stop", so the thread exits even if this fails.
            let _ = a.cmd.send(Cmd::Stop);
            if let Some(t) = a.thread.take() {
                // Join so the device is released before any new stream opens on it.
                let _ = t.join();
            }
            a.shared.set_state(PlayState::Stopped);
        }
    }

    /// Current transport state — the UI polls this to move the playhead.
    pub fn status(&self) -> PlaybackDto {
        match self.lock().as_ref() {
            Some(a) => a.shared.dto(),
            None => Player::idle_dto(),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Owns the `cpal` stream for one playback: builds it, keeps the ring fed, and answers
/// commands until stopped or finished.
///
/// This has to be a thread of its own because `cpal::Stream` is `!Send` on CoreAudio — it
/// must be built and dropped on the same thread — while [`Player`] must be `Send + Sync` to
/// be Tauri-managed state.
fn playback_thread(
    device: cpal::Device,
    chosen: Chosen,
    mut source: Source,
    shared: Arc<Shared>,
    cmd_rx: mpsc::Receiver<Cmd>,
    ready_tx: mpsc::Sender<Result<(), PlayError>>,
) {
    let (mut producer, mut consumer) =
        rtrb::RingBuffer::<f32>::new(ring_capacity(chosen.sample_rate, chosen.channels as usize));

    // Fill the ring before the device starts, or the first callbacks are guaranteed
    // underruns.
    pump(&mut producer, &mut source, &shared);

    let config = cpal::StreamConfig {
        channels: chosen.channels,
        sample_rate: chosen.sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let cb_shared = Arc::clone(&shared);
    let stream = device.build_output_stream(
        config,
        move |out: &mut [f32], _: &cpal::OutputCallbackInfo| render(&mut consumer, out, &cb_shared),
        |e| log::error!("audio output stream error: {e}"),
        None,
    );
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(PlayError::Device(e.to_string())));
            return;
        }
    };
    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(PlayError::Device(e.to_string())));
        return;
    }

    shared.set_state(PlayState::Playing);
    if ready_tx.send(Ok(())).is_err() {
        return; // The caller gave up; nothing to play for.
    }

    loop {
        match cmd_rx.recv_timeout(FEED_INTERVAL) {
            // A disconnected channel means `Player` was dropped without stopping us.
            Ok(Cmd::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Cmd::Pause) => {
                if let Err(e) = stream.pause() {
                    log::warn!("could not pause output stream: {e}");
                }
                shared.set_state(PlayState::Paused);
            }
            Ok(Cmd::Resume) => {
                if let Err(e) = stream.play() {
                    log::warn!("could not resume output stream: {e}");
                }
                shared.set_state(PlayState::Playing);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        if !shared.feed_done.load(Ordering::Acquire) {
            pump(&mut producer, &mut source, &shared);
        }
        // Set by the callback once the ring has drained: the range has been heard in full.
        if shared.state() == PlayState::Finished {
            break;
        }
    }
    // Explicit for emphasis: dropping the stream here, on the thread that built it, is what
    // releases the device.
    drop(stream);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64 as TestCounter;

    /// Unique scratch path in the OS temp dir (mirrors the helper in `audio.rs`).
    fn tmp(tag: &str) -> PathBuf {
        static N: TestCounter = TestCounter::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sf-player-{tag}-{}-{n}", std::process::id()))
    }

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A `PcmCache` over planar channel data, written straight to disk — no decode needed.
    fn pcm(channels: &[Vec<f32>], sample_rate: u32) -> (Arc<PcmCache>, Cleanup) {
        let path = tmp("planar.pcm");
        let guard = Cleanup(path.clone());
        let mut bytes = Vec::new();
        for ch in channels {
            for s in ch {
                bytes.extend_from_slice(&s.to_ne_bytes());
            }
        }
        std::fs::write(&path, &bytes).unwrap();
        let cache = PcmCache::open_planar(&path, channels.len(), sample_rate).unwrap();
        (Arc::new(cache), guard)
    }

    /// `0, 1, 2, ...` — every frame identifiable, so a mapping or ordering slip is obvious.
    fn ramp(n: usize) -> Vec<f32> {
        (0..n).map(|i| i as f32).collect()
    }

    fn shared_for(dev_channels: usize, start: u64, end: u64, ratio: f64, rate: u32) -> Arc<Shared> {
        Arc::new(Shared {
            state: AtomicU8::new(PlayState::Playing.as_u8()),
            played: AtomicU64::new(0),
            feed_done: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            dev_channels,
            start_frame: start,
            end_frame: end,
            ratio,
            sample_rate: rate,
        })
    }

    // ---------- Source: channel mapping ----------

    #[test]
    fn matched_stereo_is_copied_frame_for_frame() {
        let (cache, _c) = pcm(&[ramp(4), vec![-1.0, -2.0, -3.0, -4.0]], 48_000);
        let mut src = Source::new(cache, 0, 4, 2, 48_000);
        let mut out = [0.0f32; 8];
        assert_eq!(src.fill(&mut out), 4);
        assert_eq!(out, [0.0, -1.0, 1.0, -2.0, 2.0, -3.0, 3.0, -4.0]);
        assert!(src.finished());
    }

    #[test]
    fn mono_file_is_duplicated_to_every_device_channel() {
        // Playing a mono file on a stereo device must be heard from both speakers, not one.
        let (cache, _c) = pcm(&[vec![0.5, -0.5]], 48_000);
        let mut src = Source::new(cache, 0, 2, 2, 48_000);
        let mut out = [0.0f32; 4];
        assert_eq!(src.fill(&mut out), 2);
        assert_eq!(out, [0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn stereo_on_a_mono_device_is_downmixed_not_truncated() {
        // Dropping the right channel would silently lose half the mix.
        let (cache, _c) = pcm(&[vec![1.0, 0.0], vec![0.0, 0.5]], 48_000);
        let mut src = Source::new(cache, 0, 2, 1, 48_000);
        let mut out = [0.0f32; 2];
        assert_eq!(src.fill(&mut out), 2);
        assert_eq!(out, [0.5, 0.25]);
    }

    #[test]
    fn a_file_with_more_channels_than_the_device_keeps_the_first_ones() {
        let (cache, _c) = pcm(&[vec![1.0], vec![2.0], vec![3.0], vec![4.0]], 48_000);
        let mut src = Source::new(cache, 0, 1, 2, 48_000);
        let mut out = [0.0f32; 2];
        assert_eq!(src.fill(&mut out), 1);
        assert_eq!(out, [1.0, 2.0]);
    }

    // ---------- Source: range ----------

    #[test]
    fn only_the_requested_range_is_played() {
        let (cache, _c) = pcm(&[ramp(100)], 48_000);
        let mut src = Source::new(cache, 10, 14, 1, 48_000);
        let mut out = [0.0f32; 16];
        assert_eq!(src.fill(&mut out), 4, "must stop at the end of the range");
        assert_eq!(out[..4], [10.0, 11.0, 12.0, 13.0]);
        assert!(src.finished());
        // Past the end there is nothing more, and nothing is overwritten.
        assert_eq!(src.fill(&mut out), 0);
    }

    #[test]
    fn a_short_fill_leaves_the_rest_of_the_buffer_untouched() {
        // `pump` relies on this to detect that the source ran out mid-chunk.
        let (cache, _c) = pcm(&[ramp(3)], 48_000);
        let mut src = Source::new(cache, 0, 3, 1, 48_000);
        let mut out = [-99.0f32; 6];
        assert_eq!(src.fill(&mut out), 3);
        assert_eq!(out[3..], [-99.0, -99.0, -99.0]);
    }

    #[test]
    fn fill_across_several_calls_is_continuous() {
        let (cache, _c) = pcm(&[ramp(6)], 48_000);
        let mut src = Source::new(cache, 0, 6, 1, 48_000);
        let mut a = [0.0f32; 4];
        let mut b = [0.0f32; 4];
        assert_eq!(src.fill(&mut a), 4);
        assert_eq!(src.fill(&mut b), 2);
        assert_eq!(a, [0.0, 1.0, 2.0, 3.0]);
        assert_eq!(b[..2], [4.0, 5.0]);
    }

    // ---------- Source: resampling ----------

    #[test]
    fn a_matched_rate_needs_no_interpolation() {
        let (cache, _c) = pcm(&[ramp(8)], 44_100);
        let src = Source::new(cache, 0, 8, 2, 44_100);
        assert_eq!(src.ratio(), 1.0);
    }

    #[test]
    fn downsampling_a_ramp_interpolates_linearly() {
        // 4 kHz source on an 8 kHz device: ratio 0.5, so every other frame is a midpoint.
        let (cache, _c) = pcm(&[ramp(4)], 4_000);
        let mut src = Source::new(cache, 0, 4, 1, 8_000);
        let mut out = [0.0f32; 8];
        assert_eq!(src.fill(&mut out), 8);
        assert_eq!(out, [0.0, 0.5, 1.0, 1.5, 2.0, 2.5, 3.0, 3.0]);
        // The last frame holds rather than interpolating past the range.
    }

    #[test]
    fn resampling_preserves_duration_and_frequency() {
        // The real risk in resampling is playing at the wrong speed. A 100 Hz sine at 8 kHz
        // played on a 48 kHz device must still be 100 Hz and still last 1 s.
        let sr = 8_000;
        let sine: Vec<f32> = (0..sr)
            .map(|i| (2.0 * PI * 100.0 * i as f32 / sr as f32).sin())
            .collect();
        let (cache, _c) = pcm(&[sine], sr as u32);
        let mut src = Source::new(cache, 0, sr, 1, 48_000);
        let mut out = vec![0.0f32; 48_000 * 2];
        let frames = src.fill(&mut out);

        // 1 s at 48 kHz, within a frame of rounding.
        assert!(
            (frames as i64 - 48_000).abs() <= 1,
            "expected ~48000 device frames, got {frames}"
        );
        // Count zero crossings: 100 Hz for 1 s is 200, less the one at the buffer's end.
        let zc = out[..frames]
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        assert!(
            (zc as i64 - 199).abs() <= 2,
            "frequency drifted: {zc} crossings"
        );
        // Interpolation must not manufacture gain.
        let peak = out[..frames].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 1.0).abs() < 0.01, "peak {peak}");
    }

    #[test]
    fn resampling_starts_at_the_right_offset() {
        let (cache, _c) = pcm(&[ramp(100)], 4_000);
        let mut src = Source::new(cache, 20, 24, 1, 8_000);
        let mut out = [0.0f32; 8];
        assert_eq!(src.fill(&mut out), 8);
        assert_eq!(out[0], 20.0, "playback must begin at the range start");
    }

    // ---------- ring: pump + render ----------

    #[test]
    fn ring_capacity_is_a_whole_number_of_frames() {
        // Load-bearing: rtrb splits its slices at the capacity boundary, so an unaligned
        // capacity would rotate the channels on every wrap.
        for (rate, ch) in [(48_000, 2), (44_100, 2), (44_100, 1), (96_000, 6)] {
            let cap = ring_capacity(rate, ch);
            assert_eq!(cap % ch, 0, "{rate} Hz / {ch} ch");
            assert!(cap >= ch);
        }
    }

    #[test]
    fn pump_then_render_reproduces_the_source_exactly() {
        let (cache, _c) = pcm(&[ramp(64), ramp(64)], 48_000);
        let mut src = Source::new(Arc::clone(&cache), 0, 64, 2, 48_000);
        let shared = shared_for(2, 0, 64, 1.0, 48_000);
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(32);

        let mut heard = Vec::new();
        let mut out = [0.0f32; 6];
        for _ in 0..40 {
            pump(&mut prod, &mut src, &shared);
            render(&mut cons, &mut out, &shared);
            heard.extend_from_slice(&out);
        }
        // Everything the source held, in order, interleaved.
        let want: Vec<f32> = (0..64).flat_map(|i| [i as f32, i as f32]).collect();
        assert_eq!(&heard[..want.len()], &want[..]);
        assert_eq!(shared.state(), PlayState::Finished);
    }

    #[test]
    fn the_ring_stays_frame_aligned_across_many_wraps() {
        // A ring smaller than the data, cycled many times with an awkward callback size: if
        // a wrap ever split a frame, the two channels would swap for the rest of the stream.
        let left: Vec<f32> = (0..500).map(|i| i as f32).collect();
        let right: Vec<f32> = (0..500).map(|i| -(i as f32)).collect();
        let (cache, _c) = pcm(&[left, right], 48_000);
        let mut src = Source::new(cache, 0, 500, 2, 48_000);
        let shared = shared_for(2, 0, 500, 1.0, 48_000);
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(ring_capacity(48_000, 2).min(26));

        let mut heard = Vec::new();
        let mut out = [0.0f32; 14]; // 7 frames: not a divisor of the ring
        while shared.state() != PlayState::Finished {
            pump(&mut prod, &mut src, &shared);
            render(&mut cons, &mut out, &shared);
            heard.extend_from_slice(&out);
        }
        for f in 0..500 {
            assert_eq!(heard[f * 2], f as f32, "left channel at frame {f}");
            assert_eq!(heard[f * 2 + 1], -(f as f32), "right channel at frame {f}");
        }
    }

    #[test]
    fn render_reports_the_playhead_in_source_frames() {
        let (cache, _c) = pcm(&[ramp(100)], 48_000);
        let mut src = Source::new(cache, 10, 90, 1, 48_000);
        let shared = shared_for(1, 10, 90, 1.0, 48_000);
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(64);

        assert_eq!(
            shared.position_frames(),
            10,
            "playhead starts at the range start"
        );
        pump(&mut prod, &mut src, &shared);
        let mut out = [0.0f32; 20];
        render(&mut cons, &mut out, &shared);
        // The playhead follows what reached the device, not what the feeder has queued.
        assert_eq!(shared.position_frames(), 30);
        assert_eq!(shared.dto().position_s, 30.0 / 48_000.0);
    }

    #[test]
    fn the_playhead_tracks_the_source_rate_not_the_device_rate() {
        // 4 kHz file on an 8 kHz device: 100 device frames is 50 source frames.
        let (cache, _c) = pcm(&[ramp(200)], 4_000);
        let mut src = Source::new(cache, 0, 200, 1, 8_000);
        let shared = shared_for(1, 0, 200, 0.5, 4_000);
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(256);
        pump(&mut prod, &mut src, &shared);
        let mut out = [0.0f32; 100];
        render(&mut cons, &mut out, &shared);
        assert_eq!(shared.position_frames(), 50);
    }

    #[test]
    fn the_playhead_never_runs_past_the_end_of_the_range() {
        let (cache, _c) = pcm(&[ramp(10)], 48_000);
        let mut src = Source::new(cache, 0, 10, 1, 48_000);
        let shared = shared_for(1, 0, 10, 1.0, 48_000);
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(64);
        pump(&mut prod, &mut src, &shared);
        let mut out = [0.0f32; 40];
        render(&mut cons, &mut out, &shared);
        assert_eq!(shared.position_frames(), 10);
        assert_eq!(shared.dto().end_frame, 10);
    }

    #[test]
    fn an_underrun_yields_silence_and_is_counted() {
        // A starved callback must play silence, not stale audio, and must not claim the
        // playhead advanced through sound nobody heard.
        let (cache, _c) = pcm(&[ramp(100)], 48_000);
        let mut src = Source::new(cache, 0, 100, 1, 48_000);
        let shared = shared_for(1, 0, 100, 1.0, 48_000);
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(8);
        pump(&mut prod, &mut src, &shared); // only 8 samples available

        let mut out = [-99.0f32; 12];
        render(&mut cons, &mut out, &shared);
        assert_eq!(out[..8], ramp(8)[..]);
        assert_eq!(out[8..], [0.0; 4], "the gap must be silence");
        assert_eq!(shared.underruns.load(Ordering::Relaxed), 1);
        assert_eq!(
            shared.position_frames(),
            8,
            "silence must not move the playhead"
        );
        assert_eq!(
            shared.state(),
            PlayState::Playing,
            "an underrun is not the end"
        );
    }

    #[test]
    fn finishing_is_not_reported_as_an_underrun() {
        let (cache, _c) = pcm(&[ramp(4)], 48_000);
        let mut src = Source::new(cache, 0, 4, 1, 48_000);
        let shared = shared_for(1, 0, 4, 1.0, 48_000);
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(16);
        pump(&mut prod, &mut src, &shared);
        assert!(shared.feed_done.load(Ordering::Acquire));

        let mut out = [0.0f32; 8];
        render(&mut cons, &mut out, &shared);
        assert_eq!(shared.state(), PlayState::Finished);
        assert_eq!(
            shared.underruns.load(Ordering::Relaxed),
            0,
            "running out of range is not a dropout"
        );
    }

    #[test]
    fn a_full_ring_is_not_overfilled() {
        let (cache, _c) = pcm(&[ramp(1000)], 48_000);
        let mut src = Source::new(cache, 0, 1000, 1, 48_000);
        let shared = shared_for(1, 0, 1000, 1.0, 48_000);
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(16);
        for _ in 0..5 {
            pump(&mut prod, &mut src, &shared);
        }
        assert_eq!(prod.slots(), 0);
        assert!(
            !shared.feed_done.load(Ordering::Acquire),
            "the source is not exhausted"
        );
        // Nothing was lost: the ring holds the first 16 samples in order.
        let mut out = [0.0f32; 16];
        render(&mut cons, &mut out, &shared);
        assert_eq!(out, ramp(16)[..]);
    }

    // ---------- device config choice ----------

    #[test]
    fn the_files_own_rate_is_preferred_so_no_resampling_happens() {
        let ranges = [ConfigRange {
            channels: 2,
            min_rate: 44_100,
            max_rate: 96_000,
            is_f32: true,
        }];
        assert_eq!(
            pick_config(&ranges, 44_100),
            Some(Chosen {
                channels: 2,
                sample_rate: 44_100
            })
        );
    }

    #[test]
    fn an_unsupported_rate_falls_back_to_the_nearest_supported_one() {
        let ranges = [ConfigRange {
            channels: 2,
            min_rate: 48_000,
            max_rate: 48_000,
            is_f32: true,
        }];
        // 8 kHz cannot be played natively; the device runs at 48 kHz and Source resamples.
        assert_eq!(
            pick_config(&ranges, 8_000),
            Some(Chosen {
                channels: 2,
                sample_rate: 48_000
            })
        );
    }

    #[test]
    fn an_exact_rate_beats_a_nicer_channel_count() {
        let ranges = [
            ConfigRange {
                channels: 2,
                min_rate: 48_000,
                max_rate: 48_000,
                is_f32: true,
            },
            ConfigRange {
                channels: 8,
                min_rate: 44_100,
                max_rate: 44_100,
                is_f32: true,
            },
        ];
        assert_eq!(
            pick_config(&ranges, 44_100),
            Some(Chosen {
                channels: 8,
                sample_rate: 44_100
            })
        );
    }

    #[test]
    fn stereo_is_preferred_among_equally_good_rates() {
        let ranges = [
            ConfigRange {
                channels: 8,
                min_rate: 44_100,
                max_rate: 48_000,
                is_f32: true,
            },
            ConfigRange {
                channels: 2,
                min_rate: 44_100,
                max_rate: 48_000,
                is_f32: true,
            },
            ConfigRange {
                channels: 1,
                min_rate: 44_100,
                max_rate: 48_000,
                is_f32: true,
            },
        ];
        assert_eq!(
            pick_config(&ranges, 48_000),
            Some(Chosen {
                channels: 2,
                sample_rate: 48_000
            })
        );
    }

    #[test]
    fn non_f32_configs_are_ignored() {
        let ranges = [
            ConfigRange {
                channels: 2,
                min_rate: 44_100,
                max_rate: 44_100,
                is_f32: false,
            },
            ConfigRange {
                channels: 6,
                min_rate: 96_000,
                max_rate: 96_000,
                is_f32: true,
            },
        ];
        // The i16 stereo range at the exact rate must not win: we only render f32.
        assert_eq!(
            pick_config(&ranges, 44_100),
            Some(Chosen {
                channels: 6,
                sample_rate: 96_000
            })
        );
        assert_eq!(
            pick_config(&ranges[..1], 44_100),
            None,
            "no f32 range at all"
        );
        assert_eq!(pick_config(&[], 44_100), None);
    }

    // ---------- transport ----------

    #[test]
    fn an_idle_player_reports_stopped_without_a_device() {
        // `status` is polled by the UI on every animation frame; it must never need hardware.
        let p = Player::default();
        let dto = p.status();
        assert_eq!(dto.state, PlayState::Stopped);
        assert_eq!(dto.position_frames, 0);
        p.stop(); // idempotent, and safe with nothing playing
        assert_eq!(p.pause().state, PlayState::Stopped);
        assert_eq!(p.resume().state, PlayState::Stopped);
    }

    #[test]
    fn playing_an_empty_range_is_an_error_not_a_silent_stream() {
        let (cache, _c) = pcm(&[ramp(10)], 48_000);
        let p = Player::default();
        // Checked before any device is touched, so this holds on a machine with no sound card.
        assert!(matches!(
            p.play(Arc::clone(&cache), 5, 5),
            Err(PlayError::EmptyRange)
        ));
        assert!(matches!(
            p.play(Arc::clone(&cache), 9, 3),
            Err(PlayError::EmptyRange)
        ));
        // A range entirely past the end clamps to empty rather than reading out of bounds.
        assert!(matches!(
            p.play(cache, 100, 200),
            Err(PlayError::EmptyRange)
        ));
    }

    #[test]
    fn play_states_serialize_as_the_lowercase_names_the_ui_expects() {
        for (s, want) in [
            (PlayState::Stopped, "stopped"),
            (PlayState::Playing, "playing"),
            (PlayState::Paused, "paused"),
            (PlayState::Finished, "finished"),
        ] {
            assert_eq!(serde_json::to_value(s).unwrap(), want);
        }
        let dto = serde_json::to_value(Player::idle_dto()).unwrap();
        for key in [
            "state",
            "positionFrames",
            "positionS",
            "startFrame",
            "endFrame",
            "underruns",
        ] {
            assert!(dto.get(key).is_some(), "missing key {key}");
            assert!(!dto[key].is_null(), "{key} serialized as null");
        }
    }

    #[test]
    fn play_state_round_trips_through_its_atomic_encoding() {
        for s in [
            PlayState::Stopped,
            PlayState::Playing,
            PlayState::Paused,
            PlayState::Finished,
        ] {
            assert_eq!(PlayState::from_u8(s.as_u8()), s);
        }
    }

    #[test]
    fn errors_all_describe_themselves() {
        for e in [
            PlayError::NoDocument,
            PlayError::NoDevice,
            PlayError::NoF32Config,
            PlayError::EmptyRange,
            PlayError::Device("boom".into()),
        ] {
            assert!(!e.to_string().is_empty());
        }
    }

    /// The one test that needs real hardware: it opens the default output device and plays a
    /// short, quiet tone. Skipped where there is no device (CI runners have none), which is
    /// why every decision above is tested separately against plain data.
    #[test]
    fn playback_on_a_real_device_runs_to_the_end() {
        if cpal::default_host().default_output_device().is_none() {
            eprintln!("skipping: no output device on this machine");
            return;
        }
        // 200 ms of a quiet 440 Hz tone.
        let sr = 48_000;
        let n = sr / 5;
        let tone: Vec<f32> = (0..n)
            .map(|i| 0.05 * (2.0 * PI * 440.0 * i as f32 / sr as f32).sin())
            .collect();
        let (cache, _c) = pcm(&[tone.clone(), tone], sr as u32);

        let p = Player::default();
        let started = match p.play(Arc::clone(&cache), 0, n) {
            Ok(d) => d,
            // A machine with a device that will not open (headless, in use) is not a failure
            // of this code.
            Err(e) => {
                eprintln!("skipping: output device unavailable: {e}");
                return;
            }
        };
        assert_eq!(started.state, PlayState::Playing);
        assert_eq!(started.end_frame, n as u64);

        // Wait for it to finish, generously — this is real time on real hardware.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while p.status().state == PlayState::Playing && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let end = p.status();
        assert_eq!(end.state, PlayState::Finished, "playback did not finish");
        assert_eq!(
            end.position_frames, n as u64,
            "playhead did not reach the end"
        );
        p.stop();
        assert_eq!(p.status().state, PlayState::Stopped);
    }

    #[test]
    fn pausing_a_real_device_holds_the_playhead() {
        if cpal::default_host().default_output_device().is_none() {
            eprintln!("skipping: no output device on this machine");
            return;
        }
        let sr = 48_000;
        let n = sr * 2; // long enough that it cannot finish during the test
        let (cache, _c) = pcm(&[vec![0.0f32; n]], sr as u32);

        let p = Player::default();
        if p.play(Arc::clone(&cache), 0, n).is_err() {
            eprintln!("skipping: output device unavailable");
            return;
        }
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(p.pause().state, PlayState::Paused);
        std::thread::sleep(Duration::from_millis(60));
        let held = p.status().position_frames;
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            p.status().position_frames,
            held,
            "the playhead moved while paused"
        );

        assert_eq!(p.resume().state, PlayState::Playing);
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            p.status().position_frames > held,
            "the playhead did not resume"
        );
        p.stop();
    }

    #[test]
    fn a_second_play_replaces_the_first() {
        if cpal::default_host().default_output_device().is_none() {
            eprintln!("skipping: no output device on this machine");
            return;
        }
        let sr = 48_000;
        let (cache, _c) = pcm(&[vec![0.0f32; sr * 2]], sr as u32);
        let p = Player::default();
        if p.play(Arc::clone(&cache), 0, sr * 2).is_err() {
            eprintln!("skipping: output device unavailable");
            return;
        }
        // Restarting must release the first stream and re-arm at the new range.
        let second = match p.play(Arc::clone(&cache), 100, 200) {
            Ok(d) => d,
            Err(e) => panic!("restart failed: {e}"),
        };
        assert_eq!(second.start_frame, 100);
        assert_eq!(second.end_frame, 200);
        p.stop();
    }
}
