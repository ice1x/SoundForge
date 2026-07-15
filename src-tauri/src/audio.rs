//! Document state for the shell: an open audio file, decoded to a memory-mapped PCM cache,
//! with a summary pyramid per channel — the backend behind the `stats` / `waveform` IPC
//! commands (task 11).
//!
//! ## Why the pyramid is built once and kept
//!
//! [`sf_core::Analyzer`] borrows its samples, and building its [`Pyramid`] is the only O(n)
//! step in analysis. Constructing an analyzer per IPC call would therefore re-scan the whole
//! file on every mouse-move of a selection drag — exactly the "compute & wait" behaviour
//! SoundForge exists to avoid. So [`Document`] owns the [`PcmCache`] *and* one [`Pyramid`]
//! per channel, built once at open time, and each query builds a borrowing analyzer over
//! them for free. A stats query is then O(log N): independent of how long the selection is.
//!
//! Everything here is deliberately free of Tauri types so it can be unit-tested without a
//! webview; `lib.rs` holds the thin `#[tauri::command]` wrappers.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::Serialize;
use sf_core::{decode_file, stats::RangeStats, Analyzer, DecodeError, PcmCache, Pyramid};

/// Upper bound on waveform bins per request. A bin maps to one horizontal pixel, so this is
/// far above any real window width; it exists to stop a malformed request from asking the
/// backend to allocate an unbounded vector.
pub const MAX_BINS: usize = 8192;

/// Anything an audio IPC command can reject.
#[derive(Debug)]
pub enum AudioError {
    /// A query arrived before any file was opened.
    NoDocument,
    /// The requested channel does not exist in the open document.
    BadChannel { ch: usize, channels: usize },
    /// More waveform bins were requested than [`MAX_BINS`].
    TooManyBins { bins: usize, max: usize },
    /// The source file could not be decoded into a PCM cache.
    Decode(DecodeError),
}

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AudioError::NoDocument => write!(f, "no audio file is open"),
            AudioError::BadChannel { ch, channels } => {
                write!(f, "channel {ch} out of range (document has {channels})")
            }
            AudioError::TooManyBins { bins, max } => {
                write!(f, "requested {bins} waveform bins, maximum is {max}")
            }
            AudioError::Decode(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AudioError {}

impl From<DecodeError> for AudioError {
    fn from(e: DecodeError) -> Self {
        AudioError::Decode(e)
    }
}

/// Geometry of the open document, returned to the UI on open.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioInfo {
    /// Source file path as opened.
    pub path: String,
    pub channels: usize,
    pub sample_rate: u32,
    /// Samples per channel.
    pub frames: usize,
    pub duration_s: f64,
}

/// Statistics for one selection on one channel, in linear (non-dB) units.
///
/// dB conversion stays in the UI, matching `sf_core::stats` and the `miniforge.html`
/// prototype's `db()`. That is also what keeps this struct JSON-safe: `linear_to_db(0.0)`
/// is `-inf`, which `serde_json` would emit as `null`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StatsDto {
    pub channel: usize,
    /// Samples in the selection.
    pub n: u64,
    pub start_s: f64,
    pub duration_s: f64,
    /// Max absolute sample value.
    pub peak: f32,
    pub min: f32,
    pub min_pos_s: f64,
    pub max: f32,
    pub max_pos_s: f64,
    pub rms: f64,
    /// DC offset (mean sample value).
    pub dc: f64,
    pub zero_crossings: u64,
    /// Zero-crossing frequency estimate (sine convention).
    pub freq_hz: f64,
}

impl StatsDto {
    fn new(channel: usize, s: RangeStats) -> Self {
        StatsDto {
            channel,
            n: s.n,
            start_s: s.start_s,
            duration_s: s.duration_s,
            peak: s.peak,
            min: s.min,
            min_pos_s: s.min_pos_s,
            max: s.max,
            max_pos_s: s.max_pos_s,
            rms: s.rms,
            dc: s.dc,
            zero_crossings: s.zero_crossings,
            freq_hz: s.freq_hz,
        }
    }
}

/// Min/max envelope per horizontal pixel for the waveform view.
///
/// `min` and `max` are parallel arrays of `bins` entries rather than a list of pairs: it
/// halves the JSON envelope and drops straight into typed arrays on the canvas side.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WaveformDto {
    pub channel: usize,
    /// Range actually rendered, clamped to the document.
    pub start: usize,
    pub end: usize,
    pub bins: usize,
    pub min: Vec<f32>,
    pub max: Vec<f32>,
}

/// An open audio file: the memory-mapped PCM plus its per-channel pyramids.
struct Document {
    info: AudioInfo,
    cache: PcmCache,
    /// One pyramid per channel, built once at open time. See the module docs.
    pyramids: Vec<Pyramid>,
    cache_path: PathBuf,
}

impl Document {
    /// A borrowing analyzer over channel `ch`. O(1): the pyramid is already built.
    fn analyzer(&self, ch: usize) -> Analyzer<'_> {
        Analyzer::with_pyramid(self.cache.channel(ch), &self.pyramids[ch])
    }

    /// Reject an out-of-range channel. `PcmCache::channel` panics on one, and a panic
    /// across the IPC boundary would take down the command, so validate first.
    fn check_channel(&self, ch: usize) -> Result<(), AudioError> {
        if ch >= self.info.channels {
            return Err(AudioError::BadChannel {
                ch,
                channels: self.info.channels,
            });
        }
        Ok(())
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        // Drop the PCM cache file with the document that owns it, so repeatedly opening files
        // does not fill the app cache directory. The `Mmap` in `self.cache` is still alive at
        // this point, but on POSIX unlinking a mapped file keeps the mapping valid until it is
        // unmapped, so this is safe on the Apple-Silicon target. Best-effort: a failure here
        // (e.g. the file is already gone) must not panic in a destructor.
        if let Err(e) = std::fs::remove_file(&self.cache_path) {
            log::debug!(
                "could not remove PCM cache {}: {e}",
                self.cache_path.display()
            );
        }
    }
}

/// The shell's audio state: at most one open document, guarded for IPC access.
#[derive(Default)]
pub struct AudioState {
    doc: Mutex<Option<Document>>,
}

impl AudioState {
    /// Lock the document, recovering from a poisoned mutex.
    ///
    /// The guarded value is a plain `Option<Document>` that is only ever swapped wholesale, so
    /// a panic elsewhere cannot leave it half-updated; treating the lock as poisoned forever
    /// would break every later query for no safety gain.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Document>> {
        self.doc.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Decode `src` into a PCM cache at `cache_path`, build the pyramids, and make it the
    /// open document (replacing and cleaning up any previous one).
    ///
    /// `cache_path` must be unique per call: the previous document's cache file is removed
    /// when it is dropped here, which would delete this one if the paths collided.
    pub fn open(&self, src: &Path, cache_path: &Path) -> Result<AudioInfo, AudioError> {
        let cache = decode_file(src, cache_path)?;
        // The only O(n) work in the whole pipeline, done once per file.
        let pyramids: Vec<Pyramid> = (0..cache.channels())
            .map(|ch| Pyramid::build(cache.channel(ch)))
            .collect();

        // decode_file rejects a zero sample rate (DecodeError::Empty), so this cannot divide by zero.
        let info = AudioInfo {
            path: src.display().to_string(),
            channels: cache.channels(),
            sample_rate: cache.sample_rate(),
            frames: cache.frames(),
            duration_s: cache.frames() as f64 / cache.sample_rate() as f64,
        };
        let doc = Document {
            info: info.clone(),
            cache,
            pyramids,
            cache_path: cache_path.to_path_buf(),
        };
        *self.lock() = Some(doc);
        Ok(info)
    }

    /// Geometry of the open document, or `None` if nothing is open.
    pub fn info(&self) -> Option<AudioInfo> {
        self.lock().as_ref().map(|d| d.info.clone())
    }

    /// Close the open document (and delete its cache file). No-op if nothing is open.
    pub fn close(&self) {
        *self.lock() = None;
    }

    /// Statistics for the half-open selection `[start, end)` on channel `ch`.
    ///
    /// Bounds are clamped to the document; an empty or reversed selection yields zeroed
    /// stats rather than an error, so the UI can query freely while a drag is in progress.
    pub fn stats(&self, ch: usize, start: usize, end: usize) -> Result<StatsDto, AudioError> {
        let guard = self.lock();
        let doc = guard.as_ref().ok_or(AudioError::NoDocument)?;
        doc.check_channel(ch)?;
        let agg = doc.analyzer(ch).range(start, end);
        let start_s = start.min(doc.info.frames) as u64;
        Ok(StatsDto::new(
            ch,
            RangeStats::from_agg(&agg, start_s, doc.info.sample_rate),
        ))
    }

    /// Min/max envelope of `[start, end)` on channel `ch`, bucketed into `bins` pixels.
    pub fn waveform(
        &self,
        ch: usize,
        start: usize,
        end: usize,
        bins: usize,
    ) -> Result<WaveformDto, AudioError> {
        if bins > MAX_BINS {
            return Err(AudioError::TooManyBins {
                bins,
                max: MAX_BINS,
            });
        }
        let guard = self.lock();
        let doc = guard.as_ref().ok_or(AudioError::NoDocument)?;
        doc.check_channel(ch)?;

        let start = start.min(doc.info.frames);
        let end = end.min(doc.info.frames);
        let pairs = doc.analyzer(ch).waveform(start, end, bins);
        let (min, max) = pairs.into_iter().unzip();
        Ok(WaveformDto {
            channel: ch,
            start,
            end,
            bins,
            min,
            max,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique scratch path in the OS temp dir (mirrors the helper in `sf_core::decode`).
    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sf-audio-{tag}-{}-{n}", std::process::id()))
    }

    /// Removes paths on drop so tests do not litter the temp dir.
    struct Cleanup(Vec<PathBuf>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for p in &self.0 {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// Write a 32-bit float WAV from planar channel vectors.
    fn write_wav(path: &Path, channels: &[Vec<f32>], sample_rate: u32) {
        let spec = hound::WavSpec {
            channels: channels.len() as u16,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for f in 0..channels[0].len() {
            for ch in channels {
                w.write_sample(ch[f]).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    /// A 1 kHz sine at 48 kHz lasting exactly 1 s (whole cycles), as in the sf-core tests.
    fn sine_1k(sr: u32) -> Vec<f32> {
        (0..sr)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / sr as f32).sin())
            .collect()
    }

    /// Open a fresh state on a mono 1 kHz sine. Returns the state and its cleanup guard.
    fn open_sine() -> (AudioState, Cleanup) {
        let src = tmp("sine.wav");
        let cache = tmp("sine.pcm");
        let guard = Cleanup(vec![src.clone(), cache.clone()]);
        write_wav(&src, std::slice::from_ref(&sine_1k(48_000)), 48_000);
        let state = AudioState::default();
        state.open(&src, &cache).unwrap();
        (state, guard)
    }

    #[test]
    fn open_reports_geometry() {
        let src = tmp("geom.wav");
        let cache = tmp("geom.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);
        let left: Vec<f32> = (0..3000).map(|i| (i as f32 * 0.001).sin()).collect();
        let right: Vec<f32> = (0..3000).map(|i| -(i as f32 * 0.002).cos()).collect();
        write_wav(&src, &[left, right], 44_100);

        let state = AudioState::default();
        let info = state.open(&src, &cache).unwrap();
        assert_eq!(info.channels, 2);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.frames, 3000);
        assert!((info.duration_s - 3000.0 / 44_100.0).abs() < 1e-9);
        assert_eq!(state.info().unwrap(), info);
    }

    #[test]
    fn stats_over_full_selection_match_the_signal() {
        let (state, _c) = open_sine();
        let st = state.stats(0, 0, 48_000).unwrap();
        assert_eq!(st.channel, 0);
        assert_eq!(st.n, 48_000);
        assert!((st.duration_s - 1.0).abs() < 1e-9);
        assert!((st.peak - 1.0).abs() < 1e-3, "peak {}", st.peak);
        // RMS of a full-scale sine is 1/sqrt(2).
        assert!(
            (st.rms - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-3,
            "rms {}",
            st.rms
        );
        assert!(st.dc.abs() < 1e-3, "dc {}", st.dc);
        assert!((st.freq_hz - 1000.0).abs() < 2.0, "freq {}", st.freq_hz);
    }

    #[test]
    fn stats_of_subrange_agree_with_a_direct_core_analyzer() {
        // The IPC layer must not distort what sf-core computes: compare against the core
        // API over the same samples.
        let (state, _c) = open_sine();
        let samples = sine_1k(48_000);
        let az = Analyzer::new(&samples);
        for &(s, e) in &[
            (0usize, 1usize),
            (137, 20_011),
            (47_000, 48_000),
            (0, 48_000),
        ] {
            let got = state.stats(0, s, e).unwrap();
            let want = RangeStats::from_agg(&az.range(s, e), s as u64, 48_000);
            assert_eq!(got.n, want.n, "n [{s},{e})");
            assert_eq!(got.min, want.min, "min [{s},{e})");
            assert_eq!(got.max, want.max, "max [{s},{e})");
            assert_eq!(got.zero_crossings, want.zero_crossings, "zc [{s},{e})");
            assert!((got.rms - want.rms).abs() < 1e-9, "rms [{s},{e})");
            assert!(
                (got.start_s - want.start_s).abs() < 1e-12,
                "start [{s},{e})"
            );
        }
    }

    #[test]
    fn stats_are_per_channel() {
        let src = tmp("perch.wav");
        let cache = tmp("perch.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);
        // Channel 0 is silent, channel 1 has a constant DC bias — impossible to confuse.
        write_wav(&src, &[vec![0.0f32; 2048], vec![0.5f32; 2048]], 8000);

        let state = AudioState::default();
        state.open(&src, &cache).unwrap();
        let l = state.stats(0, 0, 2048).unwrap();
        let r = state.stats(1, 0, 2048).unwrap();
        assert_eq!(l.channel, 0);
        assert_eq!(l.peak, 0.0);
        assert_eq!(l.rms, 0.0);
        assert_eq!(r.channel, 1);
        assert!((r.dc - 0.5).abs() < 1e-6, "dc {}", r.dc);
        assert!((r.rms - 0.5).abs() < 1e-6, "rms {}", r.rms);
    }

    #[test]
    fn empty_and_reversed_selections_are_zeroed_not_errors() {
        let (state, _c) = open_sine();
        for &(s, e) in &[(500usize, 500usize), (900, 400)] {
            let st = state.stats(0, s, e).unwrap();
            assert_eq!(st.n, 0, "[{s},{e})");
            assert_eq!(st.peak, 0.0);
            assert_eq!(st.duration_s, 0.0);
            assert_eq!(st.zero_crossings, 0);
        }
    }

    #[test]
    fn out_of_bounds_selection_clamps_to_the_document() {
        let (state, _c) = open_sine();
        let st = state.stats(0, 0, 999_999).unwrap();
        assert_eq!(st.n, 48_000);
        // Starting past the end is empty, not a panic.
        assert_eq!(state.stats(0, 999_999, 1_000_000).unwrap().n, 0);
    }

    #[test]
    fn queries_without_an_open_document_error() {
        let state = AudioState::default();
        assert!(matches!(state.stats(0, 0, 1), Err(AudioError::NoDocument)));
        assert!(matches!(
            state.waveform(0, 0, 1, 8),
            Err(AudioError::NoDocument)
        ));
        assert!(state.info().is_none());
    }

    #[test]
    fn bad_channel_is_an_error_not_a_panic() {
        // PcmCache::channel panics out of range; the state layer must catch it first.
        let (state, _c) = open_sine();
        assert!(matches!(
            state.stats(7, 0, 100),
            Err(AudioError::BadChannel { ch: 7, channels: 1 })
        ));
        assert!(matches!(
            state.waveform(7, 0, 100, 8),
            Err(AudioError::BadChannel { .. })
        ));
    }

    #[test]
    fn waveform_returns_parallel_bounded_arrays() {
        let (state, _c) = open_sine();
        let wf = state.waveform(0, 0, 48_000, 800).unwrap();
        assert_eq!(wf.channel, 0);
        assert_eq!(wf.bins, 800);
        assert_eq!(wf.min.len(), 800);
        assert_eq!(wf.max.len(), 800);
        for (i, (&mn, &mx)) in wf.min.iter().zip(wf.max.iter()).enumerate() {
            assert!(mn <= mx, "bin {i}: min {mn} > max {mx}");
            assert!(mn.is_finite() && mx.is_finite(), "bin {i} not finite");
            assert!(
                (-1.0..=1.0).contains(&mn) && (-1.0..=1.0).contains(&mx),
                "bin {i}"
            );
        }
        // A full-scale sine must reach near ±1 somewhere in the envelope.
        assert!(wf.max.iter().cloned().fold(f32::MIN, f32::max) > 0.99);
        assert!(wf.min.iter().cloned().fold(f32::MAX, f32::min) < -0.99);
    }

    #[test]
    fn waveform_matches_a_direct_core_analyzer() {
        let (state, _c) = open_sine();
        let samples = sine_1k(48_000);
        let az = Analyzer::new(&samples);
        let want = az.waveform(137, 40_000, 256);
        let got = state.waveform(0, 137, 40_000, 256).unwrap();
        for (b, &(wmin, wmax)) in want.iter().enumerate() {
            assert_eq!(got.min[b], wmin, "bin {b} min");
            assert_eq!(got.max[b], wmax, "bin {b} max");
        }
    }

    #[test]
    fn waveform_clamps_range_and_reports_what_it_drew() {
        let (state, _c) = open_sine();
        let wf = state.waveform(0, 0, 999_999, 16).unwrap();
        assert_eq!(wf.start, 0);
        assert_eq!(wf.end, 48_000, "end must be clamped to the document");
        assert_eq!(wf.min.len(), 16);
    }

    #[test]
    fn waveform_rejects_absurd_bin_counts() {
        let (state, _c) = open_sine();
        assert!(matches!(
            state.waveform(0, 0, 48_000, MAX_BINS + 1),
            Err(AudioError::TooManyBins { max: MAX_BINS, .. })
        ));
        // The limit itself is allowed.
        assert_eq!(
            state.waveform(0, 0, 48_000, MAX_BINS).unwrap().bins,
            MAX_BINS
        );
    }

    #[test]
    fn zero_bins_yields_empty_arrays() {
        let (state, _c) = open_sine();
        let wf = state.waveform(0, 0, 48_000, 0).unwrap();
        assert!(wf.min.is_empty() && wf.max.is_empty());
    }

    #[test]
    fn open_replaces_the_previous_document_and_removes_its_cache() {
        let src_a = tmp("a.wav");
        let cache_a = tmp("a.pcm");
        let src_b = tmp("b.wav");
        let cache_b = tmp("b.pcm");
        let _c = Cleanup(vec![
            src_a.clone(),
            cache_a.clone(),
            src_b.clone(),
            cache_b.clone(),
        ]);
        write_wav(&src_a, std::slice::from_ref(&vec![0.25f32; 1024]), 8000);
        write_wav(&src_b, &[vec![0.5f32; 2048], vec![0.5f32; 2048]], 16_000);

        let state = AudioState::default();
        state.open(&src_a, &cache_a).unwrap();
        assert!(cache_a.exists());

        let info = state.open(&src_b, &cache_b).unwrap();
        assert_eq!(info.channels, 2);
        assert_eq!(info.frames, 2048);
        assert_eq!(state.info().unwrap().sample_rate, 16_000);
        // The replaced document dropped, taking its cache file with it.
        assert!(!cache_a.exists(), "previous PCM cache should be cleaned up");
        assert!(cache_b.exists());
    }

    #[test]
    fn close_clears_the_document_and_its_cache() {
        let src = tmp("close.wav");
        let cache = tmp("close.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);
        write_wav(&src, std::slice::from_ref(&vec![0.1f32; 512]), 8000);

        let state = AudioState::default();
        state.open(&src, &cache).unwrap();
        assert!(cache.exists());
        state.close();
        assert!(state.info().is_none());
        assert!(!cache.exists());
        assert!(matches!(state.stats(0, 0, 1), Err(AudioError::NoDocument)));
        state.close(); // idempotent
    }

    #[test]
    fn opening_a_non_audio_file_is_a_decode_error() {
        let src = tmp("garbage.wav");
        let cache = tmp("garbage.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);
        std::fs::write(&src, b"this is not audio").unwrap();

        let state = AudioState::default();
        let err = state.open(&src, &cache).unwrap_err();
        assert!(matches!(err, AudioError::Decode(_)));
        // A failed open must not leave a half-open document behind.
        assert!(state.info().is_none());
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn dtos_serialize_to_finite_camel_case_json() {
        // The UI consumes these directly; non-finite floats would serialize as `null`.
        let (state, _c) = open_sine();
        let st = serde_json::to_value(state.stats(0, 0, 48_000).unwrap()).unwrap();
        for key in [
            "startS",
            "durationS",
            "peak",
            "minPosS",
            "rms",
            "dc",
            "freqHz",
        ] {
            assert!(st.get(key).is_some(), "missing key {key}");
            assert!(!st[key].is_null(), "{key} serialized as null (non-finite?)");
        }
        // 1000 cycles at 2 crossings each, less the one at the very end of the buffer.
        assert_eq!(st["zeroCrossings"], 1999);

        // Silence has peak/rms 0 — the dB(-inf) hazard lives in the UI, not the DTO.
        let src = tmp("silence.wav");
        let cache = tmp("silence.pcm");
        let _c2 = Cleanup(vec![src.clone(), cache.clone()]);
        write_wav(&src, std::slice::from_ref(&vec![0.0f32; 1024]), 8000);
        let s2 = AudioState::default();
        s2.open(&src, &cache).unwrap();
        let js = serde_json::to_value(s2.stats(0, 0, 1024).unwrap()).unwrap();
        assert_eq!(js["peak"], 0.0);
        assert_eq!(js["rms"], 0.0);

        let wf = serde_json::to_value(state.waveform(0, 0, 48_000, 4).unwrap()).unwrap();
        assert_eq!(wf["bins"], 4);
        assert_eq!(wf["min"].as_array().unwrap().len(), 4);
        assert!(wf["max"].as_array().unwrap().iter().all(|v| !v.is_null()));

        let info = serde_json::to_value(state.info().unwrap()).unwrap();
        assert_eq!(info["sampleRate"], 48_000);
        assert_eq!(info["frames"], 48_000);
    }
}
