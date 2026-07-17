//! Recording (task 15): a `cpal` **input** stream captured into a planar PCM cache, so a take
//! becomes a document indistinguishable from an opened file.
//!
//! This is the mirror image of [`crate::player`]. Playback pulls a range of the PCM cache
//! through a ring buffer to the output device; recording pushes the input device's frames
//! through a ring buffer into a [`sf_core::CaptureWriter`], which spills them to disk and seals
//! a planar cache on stop. It replaces the browser `MediaRecorder` path, which does not exist
//! in the macOS WKWebView the shell runs in.
//!
//! ## Why a ring buffer and a feeder thread
//!
//! The input callback is realtime: it must not lock, allocate or do I/O, or it drops samples.
//! So it only ever *pushes* the device's frames into a lock-free SPSC ring ([`ingest`]); a
//! normal writer thread ([`drain`]) pops whole frames and does the file I/O of spilling them.
//! An overflowing ring — a writer that fell behind — drops the newest frames and counts them,
//! rather than blocking the callback.
//!
//! ## Threading
//!
//! `cpal::Stream` is `!Send` on CoreAudio and [`Recorder`] is Tauri-`manage`d (`Send + Sync`),
//! exactly as in playback, so the stream lives on a thread that owns it, feeds the ring, and
//! seals the cache on stop. [`Recorder`] holds only the command channel, a [`Shared`] block of
//! atomics for the UI to poll, the join handle, and the cache path being written.
//!
//! ## Testability
//!
//! Everything that decides *what is captured* is a plain function over plain data — [`ingest`]
//! (callback → ring), [`drain`] (ring → writer) and [`pick_record_config`] (device choice) —
//! so it is unit-tested without any audio hardware. Only the thread that wires them to a real
//! input device needs a microphone, and its test is skipped where there is none (CI runners
//! have no input device).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use sf_core::{CaptureWriter, DecodeError};

use crate::player::{Chosen, ConfigRange};

/// How much audio the ring can hold before the writer must have drained it. Generous compared
/// with playback's 0.25 s because the writer does file I/O, not just a memcpy, and a recording
/// has no deadline to feel responsive against — only a duty not to drop the newest frames when
/// the writer thread is briefly descheduled.
const RING_SECONDS: f64 = 1.0;

/// How long the writer thread blocks for a command before draining the ring again. Well inside
/// [`RING_SECONDS`], so the ring never overflows between drains under normal scheduling.
const DRAIN_INTERVAL: Duration = Duration::from_millis(10);

/// Anything that can stop a recording from starting or finishing.
#[derive(Debug)]
pub enum RecordError {
    /// The host reported no default input device.
    NoDevice,
    /// The device advertises no `f32` input config. See [`pick_record_config`].
    NoF32Config,
    /// Nothing was captured — the take was empty (e.g. stop pressed immediately after start).
    Empty,
    /// A `stop`/`status` arrived with no recording in progress.
    NotRecording,
    /// Sealing the planar cache failed (spill or concat I/O).
    Cache(String),
    /// The device rejected the stream (opening, starting, or querying it), or the record
    /// thread died.
    Device(String),
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::NoDevice => write!(f, "no audio input device available"),
            RecordError::NoF32Config => {
                write!(f, "input device supports no 32-bit float configuration")
            }
            RecordError::Empty => write!(f, "nothing was recorded"),
            RecordError::NotRecording => write!(f, "not recording"),
            RecordError::Cache(e) => write!(f, "could not save the recording: {e}"),
            RecordError::Device(e) => write!(f, "audio input device error: {e}"),
        }
    }
}

impl std::error::Error for RecordError {}

impl From<DecodeError> for RecordError {
    fn from(e: DecodeError) -> Self {
        match e {
            DecodeError::Empty => RecordError::Empty,
            other => RecordError::Cache(other.to_string()),
        }
    }
}

/// What a recording is doing, for the UI to poll while it captures.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecordDto {
    /// True while a stream is capturing.
    pub recording: bool,
    /// Frames captured so far (samples per channel).
    pub frames: u64,
    pub duration_s: f64,
    pub channels: usize,
    pub sample_rate: u32,
    /// Frames the writer could not keep up with and dropped. Non-zero means gaps in the take.
    pub overruns: u64,
}

/// What a finished recording produced, handed to the shell so it can adopt the planar cache as
/// the open document.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordSummary {
    pub cache_path: PathBuf,
    pub channels: usize,
    pub sample_rate: u32,
    pub frames: usize,
}

/// Choose an `f32` input config for a device, preferring the device's own default geometry.
///
/// Unlike playback there is no source rate to match: a recording is captured at whatever the
/// device offers, and the file simply carries that rate. So the preference is to record at the
/// device's *default* rate and channel count (what a user expects from "record"), falling back
/// to the nearest supported rate and the channel count closest to the default. `f32`-only for
/// the same reason as playback: CoreAudio is natively `f32` and this is the Apple-Silicon
/// target, so `i16`/`u16` capture would be untested code.
pub fn pick_record_config(
    ranges: &[ConfigRange],
    preferred_rate: u32,
    preferred_channels: u16,
) -> Option<Chosen> {
    ranges
        .iter()
        .filter(|r| r.is_f32 && r.channels >= 1 && r.min_rate <= r.max_rate)
        .map(|r| {
            let sample_rate = preferred_rate.clamp(r.min_rate, r.max_rate);
            let key = (
                // Prefer a range that can capture at the device's default rate exactly.
                u8::from(sample_rate != preferred_rate),
                // Then the channel count closest to the device's default (a mic is usually
                // mono; forcing stereo would duplicate or invent a channel).
                (i32::from(r.channels) - i32::from(preferred_channels)).unsigned_abs(),
                // Then fewer channels, so ties go to the smaller recording.
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

/// Ring size in samples for a device: [`RING_SECONDS`] of audio, rounded to whole frames.
///
/// Kept a whole number of frames purely so the ring's used/free counts stay frame-aligned and
/// the arithmetic in [`ingest`]/[`drain`] is exact; correctness of the de-interleave does not
/// depend on it, because [`drain`] copies the ring's two slices into one contiguous buffer
/// before splitting them into channels.
pub fn ring_capacity(rate: u32, channels: usize) -> usize {
    let frames = (rate as f64 * RING_SECONDS).ceil() as usize;
    frames.max(1) * channels
}

/// State shared between the input callback, the writer thread and the UI. All atomic because
/// the callback is realtime and must never block.
struct Shared {
    /// 1 while capturing, 0 once stopped. Only informational for the UI.
    state: AtomicU8,
    /// Frames durably written by the writer thread (samples per channel).
    frames: AtomicU64,
    /// Frames dropped because the ring was full when the callback fired.
    overruns: AtomicU64,
    /// Immutable after construction.
    channels: usize,
    sample_rate: u32,
}

impl Shared {
    fn dto(&self, recording: bool) -> RecordDto {
        let frames = self.frames.load(Ordering::Relaxed);
        RecordDto {
            recording,
            frames,
            duration_s: frames as f64 / self.sample_rate as f64,
            channels: self.channels,
            sample_rate: self.sample_rate,
            overruns: self.overruns.load(Ordering::Relaxed),
        }
    }
}

/// Push a batch of interleaved device frames into the ring. The body of the input callback:
/// realtime-safe, so it drops rather than blocks when the writer has fallen behind.
///
/// Writes as many whole frames as fit; any that do not are dropped and counted in `overruns`.
/// Whole-frame writes are what let [`drain`] reconstruct channels unambiguously.
pub fn ingest(
    producer: &mut rtrb::Producer<f32>,
    input: &[f32],
    channels: usize,
    overruns: &AtomicU64,
) {
    let frames = input.len() / channels;
    if frames == 0 {
        return;
    }
    let free_frames = producer.slots() / channels;
    let take_frames = frames.min(free_frames);
    let take = take_frames * channels;
    if take > 0 {
        // `take <= slots`, so this cannot fail.
        let mut chunk = producer
            .write_chunk(take)
            .expect("write_chunk within available slots");
        let (a, b) = chunk.as_mut_slices();
        a.copy_from_slice(&input[..a.len()]);
        b.copy_from_slice(&input[a.len()..take]);
        chunk.commit_all();
    }
    let dropped = frames - take_frames;
    if dropped > 0 {
        overruns.fetch_add(dropped as u64, Ordering::Relaxed);
    }
}

/// Drain every whole frame currently in the ring into `writer`. Runs on the writer thread.
///
/// The ring exposes its readable region as two slices (it may have wrapped); this copies both
/// into one contiguous `scratch` buffer before handing it to the writer, so a wrap that falls
/// mid-frame cannot misalign the de-interleave. Returns the frames drained.
pub fn drain(
    consumer: &mut rtrb::Consumer<f32>,
    writer: &mut CaptureWriter,
    channels: usize,
    scratch: &mut Vec<f32>,
) -> Result<usize, DecodeError> {
    let n = (consumer.slots() / channels) * channels;
    if n == 0 {
        return Ok(0);
    }
    // `n <= slots`, so this cannot fail.
    let chunk = consumer
        .read_chunk(n)
        .expect("read_chunk within available slots");
    let (a, b) = chunk.as_slices();
    scratch.clear();
    scratch.extend_from_slice(a);
    scratch.extend_from_slice(b);
    chunk.commit_all();
    writer.push_interleaved(scratch)?;
    Ok(n / channels)
}

/// Commands to the recording thread.
enum Cmd {
    /// Stop capturing and seal the planar cache.
    Stop,
    /// Stop capturing and throw the take away (used when a recording is discarded).
    Abort,
}

/// A live recording: the thread that owns the input stream, plus what the UI can see of it.
struct Active {
    cmd: mpsc::Sender<Cmd>,
    shared: Arc<Shared>,
    /// Yields `Some(frames)` on a sealed take, `None` on an abort, or a [`RecordError`].
    thread: Option<JoinHandle<Result<Option<usize>, RecordError>>>,
    cache_path: PathBuf,
}

/// The shell's recording state, `.manage`d by Tauri. At most one recording at a time.
#[derive(Default)]
pub struct Recorder {
    active: Mutex<Option<Active>>,
}

impl Recorder {
    /// Lock the active recording, recovering from a poisoned mutex (same reasoning as
    /// [`crate::player::Player`]: the guarded value is a plain `Option` swapped wholesale).
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The status when nothing is recording.
    fn idle_dto() -> RecordDto {
        RecordDto {
            recording: false,
            frames: 0,
            duration_s: 0.0,
            channels: 0,
            sample_rate: 0,
            overruns: 0,
        }
    }

    /// Whether a recording is in progress.
    pub fn is_recording(&self) -> bool {
        self.lock().is_some()
    }

    /// Start capturing the default input device into a planar cache at `cache_path`, replacing
    /// any recording already in progress (which is discarded).
    ///
    /// `cache_path` must be unique per call — the writer creates sibling spill files next to it
    /// (see [`sf_core::CaptureWriter::new`]) — which is what [`crate::cache::next_path`]
    /// guarantees.
    pub fn start(&self, cache_path: PathBuf) -> Result<RecordDto, RecordError> {
        self.discard();

        let device = cpal::default_host()
            .default_input_device()
            .ok_or(RecordError::NoDevice)?;

        // The device's preferred geometry — what a user means by "record from the mic".
        let default = device
            .default_input_config()
            .map_err(|e| RecordError::Device(e.to_string()))?;
        let preferred_rate = default.sample_rate();
        let preferred_channels = default.channels();

        let ranges: Vec<ConfigRange> = device
            .supported_input_configs()
            .map_err(|e| RecordError::Device(e.to_string()))?
            .map(|r| ConfigRange {
                channels: r.channels(),
                min_rate: r.min_sample_rate(),
                max_rate: r.max_sample_rate(),
                is_f32: r.sample_format() == cpal::SampleFormat::F32,
            })
            .collect();
        let chosen = pick_record_config(&ranges, preferred_rate, preferred_channels)
            .ok_or(RecordError::NoF32Config)?;

        let shared = Arc::new(Shared {
            state: AtomicU8::new(1),
            frames: AtomicU64::new(0),
            overruns: AtomicU64::new(0),
            channels: chosen.channels as usize,
            sample_rate: chosen.sample_rate,
        });

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let thread_cache = cache_path.clone();
        let thread = std::thread::Builder::new()
            .name("soundforge-record".into())
            .spawn(move || {
                record_thread(
                    device,
                    chosen,
                    thread_cache,
                    thread_shared,
                    cmd_rx,
                    ready_tx,
                )
            })
            .map_err(|e| RecordError::Device(format!("could not start record thread: {e}")))?;

        // Surface a device failure to the caller instead of leaving a dead thread behind.
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = thread.join();
                return Err(e);
            }
            Err(_) => {
                let _ = thread.join();
                return Err(RecordError::Device("record thread died on startup".into()));
            }
        }

        let dto = shared.dto(true);
        *self.lock() = Some(Active {
            cmd: cmd_tx,
            shared,
            thread: Some(thread),
            cache_path,
        });
        Ok(dto)
    }

    /// Stop the recording and seal its planar cache, returning what it produced so the shell
    /// can open it as a document. Errors with [`RecordError::NotRecording`] if nothing is being
    /// recorded, or [`RecordError::Empty`] if the take captured no frames.
    pub fn stop(&self) -> Result<RecordSummary, RecordError> {
        let mut active = self.lock().take().ok_or(RecordError::NotRecording)?;
        active.shared.state.store(0, Ordering::Release);
        let _ = active.cmd.send(Cmd::Stop);
        // Join the thread to get its sealed-take result. A panicked thread is a device error
        // rather than a crash across the IPC boundary.
        let sealed = match active.thread.take() {
            Some(t) => t
                .join()
                .map_err(|_| RecordError::Device("record thread panicked".into()))?,
            None => Ok(None),
        };
        match sealed? {
            Some(frames) => Ok(RecordSummary {
                cache_path: active.cache_path.clone(),
                channels: active.shared.channels,
                sample_rate: active.shared.sample_rate,
                frames,
            }),
            // The thread only returns `None` on an abort, which `stop` never sends.
            None => Err(RecordError::Empty),
        }
    }

    /// Current recording status — the UI polls this to show the elapsed take.
    pub fn status(&self) -> RecordDto {
        match self.lock().as_ref() {
            Some(a) => a.shared.dto(true),
            None => Recorder::idle_dto(),
        }
    }

    /// Discard any in-progress recording, throwing the take away (its spill files are reaped by
    /// the writer's `Drop`). Idempotent; used on restart and on shutdown.
    pub fn discard(&self) {
        let active = self.lock().take();
        if let Some(mut a) = active {
            a.shared.state.store(0, Ordering::Release);
            let _ = a.cmd.send(Cmd::Abort);
            if let Some(t) = a.thread.take() {
                let _ = t.join();
            }
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.discard();
    }
}

/// Owns the `cpal` input stream for one recording: builds it, drains the ring into the writer,
/// and seals or discards the cache on command.
///
/// A thread of its own for the same reason as playback: `cpal::Stream` is `!Send` on CoreAudio
/// (built and dropped on one thread) while [`Recorder`] must be `Send + Sync` to be managed.
fn record_thread(
    device: cpal::Device,
    chosen: Chosen,
    cache_path: PathBuf,
    shared: Arc<Shared>,
    cmd_rx: mpsc::Receiver<Cmd>,
    ready_tx: mpsc::Sender<Result<(), RecordError>>,
) -> Result<Option<usize>, RecordError> {
    let channels = chosen.channels as usize;
    let mut writer = match CaptureWriter::new(&cache_path, channels, chosen.sample_rate) {
        Ok(w) => w,
        Err(e) => {
            let e = RecordError::from(e);
            let _ = ready_tx.send(Err(RecordError::Cache(e.to_string())));
            return Err(e);
        }
    };

    let (mut producer, mut consumer) =
        rtrb::RingBuffer::<f32>::new(ring_capacity(chosen.sample_rate, channels));

    let config = cpal::StreamConfig {
        channels: chosen.channels,
        sample_rate: chosen.sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let cb_shared = Arc::clone(&shared);
    let stream = device.build_input_stream(
        config,
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            ingest(&mut producer, data, channels, &cb_shared.overruns);
        },
        |e| log::error!("audio input stream error: {e}"),
        None,
    );
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(RecordError::Device(e.to_string())));
            return Err(RecordError::Device(e.to_string()));
        }
    };
    if let Err(e) = stream.play() {
        let _ = ready_tx.send(Err(RecordError::Device(e.to_string())));
        return Err(RecordError::Device(e.to_string()));
    }

    if ready_tx.send(Ok(())).is_err() {
        return Ok(None); // The caller gave up; nothing to record for.
    }

    let mut scratch: Vec<f32> = Vec::new();
    loop {
        let cmd = cmd_rx.recv_timeout(DRAIN_INTERVAL);
        match cmd {
            Ok(Cmd::Stop) => {
                // Stop the device on this thread first, so no more frames arrive, then drain
                // everything still in the ring before sealing the cache.
                drop(stream);
                loop {
                    let drained = drain(&mut consumer, &mut writer, channels, &mut scratch)
                        .map_err(|e| RecordError::Cache(RecordError::from(e).to_string()))?;
                    shared
                        .frames
                        .store(writer.frames() as u64, Ordering::Release);
                    if drained == 0 {
                        break;
                    }
                }
                let frames = writer.finish()?.frames();
                return Ok(Some(frames));
            }
            // A disconnected channel means `Recorder` was dropped without a command; treat it
            // like an abort so the take is thrown away rather than half-sealed.
            Ok(Cmd::Abort) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                drop(stream);
                drop(writer); // reaps the spills; no cache file was created
                return Ok(None);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                drain(&mut consumer, &mut writer, channels, &mut scratch)
                    .map_err(|e| RecordError::Cache(RecordError::from(e).to_string()))?;
                shared
                    .frames
                    .store(writer.frames() as u64, Ordering::Release);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::AtomicU64 as TestCounter;

    fn tmp(tag: &str) -> PathBuf {
        static N: TestCounter = TestCounter::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sf-record-{tag}-{}-{n}", std::process::id()))
    }

    struct Cleanup(Vec<PathBuf>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for p in &self.0 {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    fn f32_range(channels: u16, min: u32, max: u32) -> ConfigRange {
        ConfigRange {
            channels,
            min_rate: min,
            max_rate: max,
            is_f32: true,
        }
    }

    // ---------- pick_record_config ----------

    #[test]
    fn records_at_the_devices_default_rate_and_channels() {
        // A device that prefers 48 kHz mono must be recorded exactly there, not upmixed.
        let ranges = [f32_range(1, 8_000, 96_000), f32_range(2, 8_000, 96_000)];
        assert_eq!(
            pick_record_config(&ranges, 48_000, 1),
            Some(Chosen {
                channels: 1,
                sample_rate: 48_000
            })
        );
    }

    #[test]
    fn an_unsupported_default_rate_falls_back_to_the_nearest() {
        let ranges = [f32_range(2, 44_100, 44_100)];
        // The device default claims 48 kHz but only 44.1 is actually offered.
        assert_eq!(
            pick_record_config(&ranges, 48_000, 2),
            Some(Chosen {
                channels: 2,
                sample_rate: 44_100
            })
        );
    }

    #[test]
    fn the_channel_count_closest_to_the_default_wins() {
        let ranges = [
            f32_range(1, 48_000, 48_000),
            f32_range(2, 48_000, 48_000),
            f32_range(6, 48_000, 48_000),
        ];
        // Default is stereo: the stereo range must win over mono and 5.1 at the same rate.
        assert_eq!(
            pick_record_config(&ranges, 48_000, 2),
            Some(Chosen {
                channels: 2,
                sample_rate: 48_000
            })
        );
    }

    #[test]
    fn non_f32_input_configs_are_ignored() {
        let ranges = [
            ConfigRange {
                channels: 1,
                min_rate: 48_000,
                max_rate: 48_000,
                is_f32: false,
            },
            f32_range(2, 96_000, 96_000),
        ];
        assert_eq!(
            pick_record_config(&ranges, 48_000, 1),
            Some(Chosen {
                channels: 2,
                sample_rate: 96_000
            })
        );
        assert_eq!(pick_record_config(&ranges[..1], 48_000, 1), None);
        assert_eq!(pick_record_config(&[], 48_000, 1), None);
    }

    // ---------- ring_capacity ----------

    #[test]
    fn ring_capacity_is_a_whole_number_of_frames() {
        for (rate, ch) in [(48_000, 1), (44_100, 2), (96_000, 6)] {
            let cap = ring_capacity(rate, ch);
            assert_eq!(cap % ch, 0, "{rate} Hz / {ch} ch");
            assert!(cap >= ch);
        }
    }

    // ---------- ingest + drain ----------

    /// A `CaptureWriter` over a fresh temp cache, plus its cleanup guard.
    fn writer(channels: usize, rate: u32) -> (CaptureWriter, Cleanup) {
        let cache = tmp("cap.cache");
        let guard = Cleanup(vec![
            cache.clone(),
            // Spill siblings, in case a test aborts before finish.
            {
                let mut s = cache.clone().into_os_string();
                s.push(".ch0.tmp");
                PathBuf::from(s)
            },
        ]);
        (CaptureWriter::new(&cache, channels, rate).unwrap(), guard)
    }

    #[test]
    fn ingest_then_drain_reproduces_the_input_exactly() {
        let (mut w, _c) = writer(2, 48_000);
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(ring_capacity(48_000, 2));
        let overruns = AtomicU64::new(0);

        // Two interleaved stereo batches.
        ingest(&mut prod, &[1.0, -1.0, 2.0, -2.0], 2, &overruns);
        ingest(&mut prod, &[3.0, -3.0], 2, &overruns);

        let mut scratch = Vec::new();
        let drained = drain(&mut cons, &mut w, 2, &mut scratch).unwrap();
        assert_eq!(drained, 3);
        assert_eq!(overruns.load(Ordering::Relaxed), 0);

        let pcm = w.finish().unwrap();
        assert_eq!(pcm.channel(0), &[1.0, 2.0, 3.0]);
        assert_eq!(pcm.channel(1), &[-1.0, -2.0, -3.0]);
    }

    #[test]
    fn a_full_ring_drops_the_newest_frames_and_counts_them() {
        // The writer has fallen behind: the callback must not block, it drops and records.
        let (mut w, _c) = writer(1, 8_000);
        // A ring that holds only 4 frames.
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(4);
        let overruns = AtomicU64::new(0);

        ingest(&mut prod, &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0], 1, &overruns);
        assert_eq!(overruns.load(Ordering::Relaxed), 2, "two frames dropped");

        let mut scratch = Vec::new();
        drain(&mut cons, &mut w, 1, &mut scratch).unwrap();
        let pcm = w.finish().unwrap();
        // Only the frames that fit were kept, in order.
        assert_eq!(pcm.channel(0), &[0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn draining_an_empty_ring_is_a_no_op() {
        let (mut w, _c) = writer(2, 8_000);
        let (_prod, mut cons) = rtrb::RingBuffer::<f32>::new(8);
        let mut scratch = Vec::new();
        assert_eq!(drain(&mut cons, &mut w, 2, &mut scratch).unwrap(), 0);
    }

    #[test]
    fn drain_survives_a_ring_wrap_without_swapping_channels() {
        // Cycle a small ring many times with an awkward batch size: if a wrap ever split a
        // frame across the two read slices, the stereo image would swap for the rest of the
        // take. `drain` copies both slices contiguously first, so it must not.
        let (mut w, _c) = writer(2, 48_000);
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(ring_capacity(48_000, 2).min(26));
        let overruns = AtomicU64::new(0);

        let mut next = 0i32;
        // Feed 500 frames through in 7-frame batches, draining between each so the ring wraps.
        while next < 500 {
            let mut batch = Vec::new();
            for _ in 0..7 {
                if next >= 500 {
                    break;
                }
                batch.push(next as f32);
                batch.push(-(next as f32));
                next += 1;
            }
            ingest(&mut prod, &batch, 2, &overruns);
            let mut scratch = Vec::new();
            drain(&mut cons, &mut w, 2, &mut scratch).unwrap();
        }
        assert_eq!(
            overruns.load(Ordering::Relaxed),
            0,
            "the ring was drained each round"
        );

        let pcm = w.finish().unwrap();
        for f in 0..500 {
            assert_eq!(pcm.channel(0)[f], f as f32, "left at frame {f}");
            assert_eq!(pcm.channel(1)[f], -(f as f32), "right at frame {f}");
        }
    }

    // ---------- transport ----------

    #[test]
    fn an_idle_recorder_reports_not_recording_without_a_device() {
        // `status` is polled by the UI; it must never need hardware.
        let r = Recorder::default();
        assert!(!r.is_recording());
        let dto = r.status();
        assert!(!dto.recording);
        assert_eq!(dto.frames, 0);
        // Stopping when nothing is recording is an error, not a panic.
        assert!(matches!(r.stop(), Err(RecordError::NotRecording)));
        r.discard(); // idempotent, safe with nothing recording
    }

    #[test]
    fn errors_all_describe_themselves() {
        for e in [
            RecordError::NoDevice,
            RecordError::NoF32Config,
            RecordError::Empty,
            RecordError::NotRecording,
            RecordError::Cache("disk full".into()),
            RecordError::Device("boom".into()),
        ] {
            assert!(!e.to_string().is_empty());
        }
    }

    #[test]
    fn record_dto_serializes_the_keys_the_ui_expects() {
        let dto = Recorder::idle_dto();
        let v = serde_json::to_value(&dto).unwrap();
        for key in [
            "recording",
            "frames",
            "durationS",
            "channels",
            "sampleRate",
            "overruns",
        ] {
            assert!(v.get(key).is_some(), "missing key {key}");
            assert!(!v[key].is_null(), "{key} serialized as null");
        }
    }

    /// The one test that needs real hardware: it opens the default input device and captures a
    /// short take. Skipped where there is no input device (CI runners have none), which is why
    /// every decision above is tested separately against plain data.
    #[test]
    fn recording_on_a_real_device_produces_a_document() {
        if cpal::default_host().default_input_device().is_none() {
            eprintln!("skipping: no input device on this machine");
            return;
        }
        let cache = tmp("real.cache");
        let _c = Cleanup(vec![cache.clone()]);

        let r = Recorder::default();
        let started = match r.start(cache.clone()) {
            Ok(d) => d,
            // A machine whose input device will not open (headless, in use, permission denied)
            // is not a failure of this code.
            Err(e) => {
                eprintln!("skipping: input device unavailable: {e}");
                return;
            }
        };
        assert!(started.recording);
        assert!(r.is_recording());

        // Capture for a moment — long enough that at least one buffer lands.
        std::thread::sleep(Duration::from_millis(300));

        let summary = match r.stop() {
            Ok(s) => s,
            Err(RecordError::Empty) => {
                eprintln!("skipping: device delivered no frames in the window");
                return;
            }
            Err(e) => panic!("stop failed: {e}"),
        };
        assert!(!r.is_recording());
        assert!(summary.frames > 0, "a 300 ms take should hold frames");
        assert!(
            Path::new(&summary.cache_path).exists(),
            "the cache was sealed"
        );

        // It opens as a normal planar cache with the geometry we recorded at.
        let pcm = sf_core::PcmCache::open_planar(
            &summary.cache_path,
            summary.channels,
            summary.sample_rate,
        )
        .unwrap();
        assert_eq!(pcm.frames(), summary.frames);
        assert_eq!(pcm.channels(), summary.channels);
    }
}
