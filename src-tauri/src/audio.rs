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
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
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
    /// The samples are in use elsewhere (playback), so they cannot be edited right now.
    Busy,
    /// The edit needs a selection and was given an empty one.
    EmptyRange,
    /// Nothing left to undo.
    NothingToUndo,
    /// Writing the trimmed cache failed.
    Io(String),
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
            AudioError::Busy => write!(f, "the audio is in use (stop playback first)"),
            AudioError::EmptyRange => write!(f, "select a range first"),
            AudioError::NothingToUndo => write!(f, "nothing to undo"),
            AudioError::Io(e) => write!(f, "{e}"),
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

/// The document after an edit, plus what the Undo button should now look like.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditDto {
    /// Geometry after the edit. `frames` changes on a trim, so the UI must re-read it.
    pub info: AudioInfo,
    /// Whether the undo stack has anything in it.
    pub can_undo: bool,
    /// Whether *this* edit was recorded. False means it applied but is not reversible — its
    /// snapshot exceeded [`MAX_UNDO_BYTES`]. See there.
    pub last_undoable: bool,
}

/// Total sample data the undo stack may hold in memory.
///
/// Undo snapshots the original samples, so its cost scales with the *selection*, not the
/// file — but "Select all → Normalize" on a 2-hour stereo file would snapshot ~2.8 GB, which
/// is exactly the "behaves like a short file" promise breaking. So the stack is capped:
/// older entries are evicted to make room, and an edit whose own snapshot exceeds the cap
/// applies without being undoable rather than pretending. [`EditDto::can_undo`] tells the UI
/// which it got, so the Undo button reflects the truth.
pub const MAX_UNDO_BYTES: usize = 256 * 1024 * 1024;

/// An in-place edit: changes sample values, never the document's length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EditOp {
    /// Scale so the loudest sample in the selection reaches full scale.
    Normalize,
    FadeIn,
    FadeOut,
    Silence,
}

impl EditOp {
    fn name(self) -> &'static str {
        match self {
            EditOp::Normalize => "normalize",
            EditOp::FadeIn => "fade in",
            EditOp::FadeOut => "fade out",
            EditOp::Silence => "silence",
        }
    }
}

/// One reversible step.
enum UndoEntry {
    /// An in-place edit: the original samples of `[start, start + len)` per channel.
    Samples {
        start: usize,
        /// One vector per channel, all the same length.
        channels: Vec<Vec<f32>>,
    },
    /// A trim: the document it replaced. The old cache file is kept alive by this entry —
    /// `cache_path` is `None` once the entry has been applied and has handed it back.
    Trim {
        cache_path: Option<PathBuf>,
        info: AudioInfo,
    },
}

impl UndoEntry {
    /// Sample bytes this entry holds in memory.
    fn bytes(&self) -> usize {
        match self {
            UndoEntry::Samples { channels, .. } => channels
                .iter()
                .map(|c| c.len() * std::mem::size_of::<f32>())
                .sum(),
            // A trim's cost is a file on disk, not memory.
            UndoEntry::Trim { .. } => 0,
        }
    }
}

impl Drop for UndoEntry {
    fn drop(&mut self) {
        // A `Trim` entry owns the cache file of the document it replaced. If the entry is
        // dropped without being applied — the stack was evicted, or the document closed —
        // nothing else will ever remove that file, so it goes here. An applied entry has
        // already `take`n the path, so this cannot delete a live document's cache. Any that
        // still escape (a crash between the two) are reaped by the startup sweep.
        if let UndoEntry::Trim {
            cache_path: Some(p),
            ..
        } = self
        {
            if let Err(e) = std::fs::remove_file(&*p) {
                log::debug!("could not remove undo cache {}: {e}", p.display());
            }
        }
    }
}

/// An open audio file: the memory-mapped PCM plus its per-channel pyramids.
struct Document {
    info: AudioInfo,
    /// Shared so playback can hold the PCM for the life of a stream without ever taking the
    /// document lock on the audio path. See [`AudioState::pcm`].
    cache: Arc<PcmCache>,
    /// One pyramid per channel, built once at open time. See the module docs.
    pyramids: Vec<Pyramid>,
    cache_path: PathBuf,
    /// Most recent last.
    undo: Vec<UndoEntry>,
}

impl Document {
    /// A borrowing analyzer over channel `ch`. O(1): the pyramid is already built.
    fn analyzer(&self, ch: usize) -> Analyzer<'_> {
        Analyzer::with_pyramid(self.cache.channel(ch), &self.pyramids[ch])
    }

    /// Exclusive access to the samples.
    ///
    /// Fails with [`AudioError::Busy`] while playback holds its own handle: the audio path
    /// reads these samples from another thread, and editing under it would be a data race
    /// (and, audibly, a click). Callers stop playback first — see the `edit` command.
    fn cache_mut(&mut self) -> Result<&mut PcmCache, AudioError> {
        Arc::get_mut(&mut self.cache).ok_or(AudioError::Busy)
    }

    /// Rebuild the summary pyramids for `channels`.
    ///
    /// **Every** path that changes samples must end here. A pyramid whose length still
    /// matches but whose contents are stale is undetectable — `Analyzer::with_pyramid` only
    /// asserts the length — and would silently answer every later query from pre-edit blocks.
    /// This is the single reason edits go through `Document` rather than touching the cache
    /// directly: it is the only place that cannot forget.
    fn rebuild_pyramids(&mut self, channels: impl IntoIterator<Item = usize>) {
        for ch in channels {
            self.pyramids[ch] = Pyramid::build(self.cache.channel(ch));
        }
    }

    /// Push an undo entry, evicting oldest-first to stay under [`MAX_UNDO_BYTES`].
    ///
    /// An entry too big to fit on its own is not recorded at all and clears the stack: the
    /// alternative is holding gigabytes of snapshot for one keystroke. Returns whether the
    /// edit ended up undoable.
    fn push_undo(&mut self, entry: UndoEntry) -> bool {
        if entry.bytes() > MAX_UNDO_BYTES {
            // Later entries describe a document this one no longer matches, so keeping them
            // would let Undo restore samples into the wrong place.
            self.undo.clear();
            log::info!(
                "edit is too large to undo ({} MB > {} MB cap); undo history cleared",
                entry.bytes() / 1024 / 1024,
                MAX_UNDO_BYTES / 1024 / 1024
            );
            return false;
        }
        self.undo.push(entry);
        let mut total: usize = self.undo.iter().map(|e| e.bytes()).sum();
        while total > MAX_UNDO_BYTES && self.undo.len() > 1 {
            let dropped = self.undo.remove(0);
            total -= dropped.bytes();
        }
        true
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
            cache: Arc::new(cache),
            pyramids,
            cache_path: cache_path.to_path_buf(),
            undo: Vec::new(),
        };
        *self.lock() = Some(doc);
        Ok(info)
    }

    /// Geometry of the open document, or `None` if nothing is open.
    pub fn info(&self) -> Option<AudioInfo> {
        self.lock().as_ref().map(|d| d.info.clone())
    }

    /// A shared handle on the open document's PCM, for playback (task 14).
    ///
    /// Taken once when a stream starts, so the audio path never touches this lock — a
    /// selection drag holds it thousands of times a minute, and blocking the feeder thread
    /// behind one would be an audible dropout.
    ///
    /// Holding this `Arc` also keeps playback safe across a `close`: the document drops and
    /// unlinks its cache file, but the `Mmap` inside the `PcmCache` lives until the last
    /// handle goes, and on POSIX an unlinked file stays readable while mapped.
    pub fn pcm(&self) -> Result<Arc<PcmCache>, AudioError> {
        let guard = self.lock();
        let doc = guard.as_ref().ok_or(AudioError::NoDocument)?;
        Ok(Arc::clone(&doc.cache))
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

    /// Apply `op` to `[start, end)` across **every** channel, and record it for undo.
    ///
    /// The range is clamped to the document; an empty one is [`AudioError::EmptyRange`] —
    /// unlike `stats`, which zeroes an empty selection so a drag can query freely. An edit is
    /// a deliberate act on a chosen range, and silently editing nothing would be worse than
    /// saying so.
    ///
    /// Applies to all channels, not one: the Statistics panel's channel selector chooses what
    /// you *look at*, and using it to decide what gets *edited* would silence one side of a
    /// stereo file because the panel happened to be showing channel 0.
    pub fn edit(&self, op: EditOp, start: usize, end: usize) -> Result<EditDto, AudioError> {
        let mut guard = self.lock();
        let doc = guard.as_mut().ok_or(AudioError::NoDocument)?;
        let frames = doc.info.frames;
        let channels = doc.info.channels;
        let start = start.min(frames);
        let end = end.min(frames);
        if start >= end {
            return Err(AudioError::EmptyRange);
        }

        let cache = doc.cache_mut()?;
        // Snapshot before touching anything: on the error paths below the document must be
        // exactly as it was.
        let original: Vec<Vec<f32>> = (0..channels)
            .map(|ch| cache.channel(ch)[start..end].to_vec())
            .collect();

        match op {
            // One gain across every channel, computed from the loudest of them. Per-channel
            // gains would equalise the channels and shift the stereo image.
            EditOp::Normalize => {
                let peak = (0..channels)
                    .map(|ch| sf_core::peak(&cache.channel(ch)[start..end]))
                    .fold(0.0f32, f32::max);
                let gain = sf_core::gain_for(peak, 1.0);
                for ch in 0..channels {
                    sf_core::apply_gain(&mut cache.channel_mut(ch)[start..end], gain);
                }
                log::info!("normalize [{start}, {end}): peak {peak:.4} -> gain {gain:.4}");
            }
            EditOp::FadeIn => {
                for ch in 0..channels {
                    sf_core::fade_in(&mut cache.channel_mut(ch)[start..end]);
                }
            }
            EditOp::FadeOut => {
                for ch in 0..channels {
                    sf_core::fade_out(&mut cache.channel_mut(ch)[start..end]);
                }
            }
            EditOp::Silence => {
                for ch in 0..channels {
                    sf_core::silence(&mut cache.channel_mut(ch)[start..end]);
                }
            }
        }

        doc.rebuild_pyramids(0..channels);
        let can_undo = doc.push_undo(UndoEntry::Samples {
            start,
            channels: original,
        });
        log::info!("applied {} to [{start}, {end})", op.name());
        Ok(EditDto {
            info: doc.info.clone(),
            can_undo: !doc.undo.is_empty(),
            last_undoable: can_undo,
        })
    }

    /// Discard everything outside `[start, end)`, making the selection the whole document.
    ///
    /// Unlike the in-place edits this changes the document's *length*, so it cannot write
    /// through the existing map: it writes a fresh planar cache at `new_cache_path` (which
    /// must be unique — see [`crate::cache::next_path`]) and swaps the document onto it. The
    /// previous cache file is handed to the undo stack rather than deleted, which is what
    /// makes this reversible without copying the samples into memory.
    pub fn trim(
        &self,
        start: usize,
        end: usize,
        new_cache_path: &Path,
    ) -> Result<EditDto, AudioError> {
        let mut guard = self.lock();
        let doc = guard.as_mut().ok_or(AudioError::NoDocument)?;
        let frames = doc.info.frames;
        let channels = doc.info.channels;
        let sample_rate = doc.info.sample_rate;
        let start = start.min(frames);
        let end = end.min(frames);
        if start >= end {
            return Err(AudioError::EmptyRange);
        }
        if start == 0 && end == frames {
            // Trimming to the whole file would otherwise burn a full copy to say "nothing
            // changed", and push an undo entry that restores an identical document.
            return Ok(EditDto {
                info: doc.info.clone(),
                can_undo: !doc.undo.is_empty(),
                last_undoable: false,
            });
        }
        // Reject before writing anything: a trim under playback would swap the samples out
        // from under the audio thread.
        doc.cache_mut()?;

        // Write the kept range as a new planar cache, channel by channel, straight from the
        // old map — never materialising the document in memory.
        let mut out = std::fs::File::create(new_cache_path).map_err(|e| {
            AudioError::Io(format!(
                "could not create {}: {e}",
                new_cache_path.display()
            ))
        })?;
        {
            use std::io::Write;
            for ch in 0..channels {
                let slice = &doc.cache.channel(ch)[start..end];
                out.write_all(bytemuck::cast_slice(slice)).map_err(|e| {
                    AudioError::Io(format!("could not write the trimmed cache: {e}"))
                })?;
            }
            out.flush()
                .map_err(|e| AudioError::Io(format!("could not flush the trimmed cache: {e}")))?;
        }
        drop(out);

        let new_cache = PcmCache::open_planar(new_cache_path, channels, sample_rate)?;
        let new_frames = new_cache.frames();
        let pyramids: Vec<Pyramid> = (0..channels)
            .map(|ch| Pyramid::build(new_cache.channel(ch)))
            .collect();

        let old_info = doc.info.clone();
        let old_path = std::mem::replace(&mut doc.cache_path, new_cache_path.to_path_buf());
        doc.cache = Arc::new(new_cache);
        doc.pyramids = pyramids;
        doc.info.frames = new_frames;
        doc.info.duration_s = new_frames as f64 / sample_rate as f64;

        // Every earlier entry's `start` indexes the untrimmed document, so applying one after
        // this trim would write samples at the wrong offset. They go.
        doc.undo.clear();
        doc.undo.push(UndoEntry::Trim {
            cache_path: Some(old_path),
            info: old_info,
        });
        log::info!("trimmed to [{start}, {end}): {new_frames} frames remain");
        Ok(EditDto {
            info: doc.info.clone(),
            can_undo: true,
            last_undoable: true,
        })
    }

    /// Reverse the most recent edit.
    pub fn undo(&self) -> Result<EditDto, AudioError> {
        let mut guard = self.lock();
        let doc = guard.as_mut().ok_or(AudioError::NoDocument)?;
        if doc.undo.is_empty() {
            return Err(AudioError::NothingToUndo);
        }
        // Check before popping, so a rejected undo leaves the stack intact.
        doc.cache_mut()?;
        let mut entry = doc.undo.pop().expect("checked non-empty");

        match &mut entry {
            UndoEntry::Samples { start, channels } => {
                let start = *start;
                let cache = doc.cache_mut()?;
                for (ch, original) in channels.iter().enumerate() {
                    cache.channel_mut(ch)[start..start + original.len()].copy_from_slice(original);
                }
                let n = channels.len();
                doc.rebuild_pyramids(0..n);
                log::info!("undid an edit at [{start}, {})", start + channels[0].len());
            }
            UndoEntry::Trim { cache_path, info } => {
                let path = cache_path
                    .take()
                    .expect("an unapplied Trim entry owns its path");
                let restored = PcmCache::open_planar(&path, info.channels, info.sample_rate)?;
                let pyramids: Vec<Pyramid> = (0..info.channels)
                    .map(|ch| Pyramid::build(restored.channel(ch)))
                    .collect();
                // Dropping the Document would take the *trimmed* cache with it, so remove it
                // explicitly here: we are replacing the file, not the document.
                let trimmed_path = std::mem::replace(&mut doc.cache_path, path);
                doc.cache = Arc::new(restored);
                doc.pyramids = pyramids;
                doc.info = info.clone();
                if let Err(e) = std::fs::remove_file(&trimmed_path) {
                    log::debug!(
                        "could not remove trimmed cache {}: {e}",
                        trimmed_path.display()
                    );
                }
                log::info!("undid a trim: {} frames restored", doc.info.frames);
            }
        }
        Ok(EditDto {
            info: doc.info.clone(),
            can_undo: !doc.undo.is_empty(),
            last_undoable: true,
        })
    }

    /// Whether there is anything to undo.
    pub fn can_undo(&self) -> bool {
        self.lock().as_ref().is_some_and(|d| !d.undo.is_empty())
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
    fn pcm_hands_out_the_open_documents_samples() {
        let (state, _c) = open_sine();
        let pcm = state.pcm().unwrap();
        assert_eq!(pcm.channels(), 1);
        assert_eq!(pcm.frames(), 48_000);
        assert_eq!(pcm.sample_rate(), 48_000);
        assert_eq!(pcm.channel(0), &sine_1k(48_000)[..]);
        assert!(matches!(
            AudioState::default().pcm().map(|_| ()),
            Err(AudioError::NoDocument)
        ));
    }

    #[test]
    fn a_pcm_handle_outlives_the_document_it_came_from() {
        // This is what lets playback survive the user closing the file mid-stream: the cache
        // file is unlinked with the document, but a POSIX mapping stays valid until unmapped.
        let src = tmp("outlive.wav");
        let cache = tmp("outlive.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);
        write_wav(&src, std::slice::from_ref(&vec![0.75f32; 4096]), 8000);

        let state = AudioState::default();
        state.open(&src, &cache).unwrap();
        let pcm = state.pcm().unwrap();

        state.close();
        assert!(!cache.exists(), "the cache file should have been unlinked");
        // The samples are still readable through the handle taken before the close.
        assert_eq!(pcm.frames(), 4096);
        assert!(pcm.channel(0).iter().all(|&s| s == 0.75));
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

    // ---------- edits ----------

    /// Frames in the edit fixture.
    ///
    /// Deliberately several `sf_core::summary::LEAF` blocks (LEAF is 1024): a range query
    /// shorter than one leaf is served by scanning raw samples and never consults the
    /// pyramid, so a fixture under 1024 frames cannot detect a stale pyramid at all — the
    /// exact failure these tests exist to catch. Verified by mutation: with 1000 frames,
    /// deleting the rebuild from `undo` killed nothing.
    const FIXTURE_FRAMES: usize = 8192;

    /// A stereo document: channel 0 loud, channel 1 four times quieter. Any edit that treats
    /// the channels independently shows up as the two drifting apart.
    fn open_stereo() -> (AudioState, Cleanup, PathBuf) {
        let src = tmp("edit.wav");
        let cache = tmp("edit.pcm");
        let guard = Cleanup(vec![src.clone(), cache.clone()]);
        let loud: Vec<f32> = (0..FIXTURE_FRAMES)
            .map(|i| 0.5 * (i as f32 * 0.05).sin())
            .collect();
        let quiet: Vec<f32> = loud.iter().map(|s| s * 0.25).collect();
        write_wav(&src, &[loud, quiet], 8000);
        let state = AudioState::default();
        state.open(&src, &cache).unwrap();
        (state, guard, cache)
    }

    #[test]
    fn normalize_lifts_the_selection_to_full_scale() {
        let (state, _c, _p) = open_stereo();
        state.edit(EditOp::Normalize, 0, FIXTURE_FRAMES).unwrap();
        let l = state.stats(0, 0, FIXTURE_FRAMES).unwrap();
        assert!((l.peak - 1.0).abs() < 1e-4, "peak {}", l.peak);
    }

    #[test]
    fn normalize_uses_one_gain_across_channels_and_keeps_the_balance() {
        // The channels start 4x apart. Normalizing each to its own peak would make them
        // equal — audibly, a hard-panned mix would jump to the centre.
        let (state, _c, _p) = open_stereo();
        let before = (
            state.stats(0, 0, FIXTURE_FRAMES).unwrap().peak,
            state.stats(1, 0, FIXTURE_FRAMES).unwrap().peak,
        );
        assert!(
            (before.0 / before.1 - 4.0).abs() < 1e-3,
            "fixture: {before:?}"
        );

        state.edit(EditOp::Normalize, 0, FIXTURE_FRAMES).unwrap();
        let after = (
            state.stats(0, 0, FIXTURE_FRAMES).unwrap().peak,
            state.stats(1, 0, FIXTURE_FRAMES).unwrap().peak,
        );
        assert!((after.0 - 1.0).abs() < 1e-4, "loud channel {}", after.0);
        assert!(
            (after.1 - 0.25).abs() < 1e-4,
            "quiet channel must stay 4x quieter, got {}",
            after.1
        );
    }

    #[test]
    fn an_edit_rebuilds_the_pyramid_so_stats_reflect_it() {
        // THE trap this module exists to close: an edit changes sample values without
        // changing the length, and `Analyzer::with_pyramid` only asserts the length. A
        // forgotten rebuild is invisible — every later query silently answers from pre-edit
        // blocks. `stats` goes through the pyramid, so this catches it.
        let (state, _c, _p) = open_stereo();
        assert!(state.stats(0, 0, FIXTURE_FRAMES).unwrap().peak > 0.4);

        state.edit(EditOp::Silence, 0, FIXTURE_FRAMES).unwrap();

        let st = state.stats(0, 0, FIXTURE_FRAMES).unwrap();
        assert_eq!(st.peak, 0.0, "stats came from a stale pyramid");
        assert_eq!(st.rms, 0.0);
        assert_eq!(st.zero_crossings, 0);
        // The waveform reads the pyramid too.
        let wf = state.waveform(0, 0, FIXTURE_FRAMES, 8).unwrap();
        assert!(wf.max.iter().all(|&v| v == 0.0), "envelope is stale");
    }

    #[test]
    fn an_edit_touches_only_the_selected_range() {
        let (state, _c, _p) = open_stereo();
        let half = FIXTURE_FRAMES / 2;
        let before_tail = state.stats(0, half, FIXTURE_FRAMES).unwrap().peak;
        state.edit(EditOp::Silence, 0, half).unwrap();
        assert_eq!(state.stats(0, 0, half).unwrap().peak, 0.0);
        assert_eq!(
            state.stats(0, half, FIXTURE_FRAMES).unwrap().peak,
            before_tail,
            "audio outside the selection changed"
        );
    }

    #[test]
    fn fades_run_the_right_way_round() {
        let (state, _c, _p) = open_stereo();
        let tail = FIXTURE_FRAMES - 50;
        state.edit(EditOp::FadeIn, 0, FIXTURE_FRAMES).unwrap();
        // A fade in leaves the start near silence and the end untouched.
        assert!(state.stats(0, 0, 50).unwrap().peak < 0.05, "fade in start");
        assert!(
            state.stats(0, tail, FIXTURE_FRAMES).unwrap().peak > 0.4,
            "fade in end"
        );

        let (state2, _c2, _p2) = open_stereo();
        state2.edit(EditOp::FadeOut, 0, FIXTURE_FRAMES).unwrap();
        assert!(
            state2.stats(0, tail, FIXTURE_FRAMES).unwrap().peak < 0.05,
            "fade out end"
        );
        assert!(state2.stats(0, 0, 50).unwrap().peak > 0.4, "fade out start");
    }

    #[test]
    fn an_empty_selection_is_rejected_rather_than_silently_editing_nothing() {
        // Unlike `stats`, which zeroes an empty range so a drag can query freely.
        let (state, _c, _p) = open_stereo();
        for &(s, e) in &[(500usize, 500usize), (900, 400), (99_000, 100_000)] {
            assert!(
                matches!(
                    state.edit(EditOp::Silence, s, e),
                    Err(AudioError::EmptyRange)
                ),
                "[{s},{e})"
            );
        }
        assert!(matches!(
            AudioState::default().edit(EditOp::Silence, 0, 1),
            Err(AudioError::NoDocument)
        ));
    }

    #[test]
    fn an_edit_cannot_run_while_playback_holds_the_samples() {
        // Editing under the audio thread would be a data race on the mmap.
        let (state, _c, _p) = open_stereo();
        let held = state.pcm().unwrap();
        assert!(matches!(
            state.edit(EditOp::Silence, 0, 100),
            Err(AudioError::Busy)
        ));
        // And the document is untouched by the rejection.
        assert!(state.stats(0, 0, 100).unwrap().peak > 0.0);
        drop(held);
        assert!(state.edit(EditOp::Silence, 0, 100).is_ok(), "freed again");
    }

    // ---------- undo ----------

    #[test]
    fn undo_restores_the_samples_exactly() {
        let (state, _c, _p) = open_stereo();
        let before: Vec<StatsDto> = (0..2)
            .map(|ch| state.stats(ch, 0, FIXTURE_FRAMES).unwrap())
            .collect();
        assert!(!state.can_undo());

        state.edit(EditOp::Normalize, 100, 5000).unwrap();
        assert!(state.can_undo());
        assert_ne!(state.stats(0, 100, 5000).unwrap().peak, before[0].peak);

        state.undo().unwrap();
        for (ch, want) in before.iter().enumerate() {
            assert_eq!(
                &state.stats(ch, 0, FIXTURE_FRAMES).unwrap(),
                want,
                "channel {ch} not restored"
            );
        }
        assert!(!state.can_undo());
        assert!(matches!(state.undo(), Err(AudioError::NothingToUndo)));
    }

    #[test]
    fn undo_rebuilds_the_pyramid_too() {
        // Restoring the samples but not the pyramid is the same trap in reverse.
        let (state, _c, _p) = open_stereo();
        let before = state.waveform(0, 0, FIXTURE_FRAMES, 8).unwrap();
        let stats_before = state.stats(0, 0, FIXTURE_FRAMES).unwrap();
        state.edit(EditOp::Silence, 0, FIXTURE_FRAMES).unwrap();
        state.undo().unwrap();
        assert_eq!(state.waveform(0, 0, FIXTURE_FRAMES, 8).unwrap(), before);
        // The range query stitches whole leaf blocks out of the pyramid, so a rebuild the
        // undo forgot shows up right here.
        assert_eq!(state.stats(0, 0, FIXTURE_FRAMES).unwrap(), stats_before);
    }

    #[test]
    fn undo_unwinds_several_edits_in_reverse_order() {
        let (state, _c, _p) = open_stereo();
        let before = state.stats(0, 0, FIXTURE_FRAMES).unwrap();
        state.edit(EditOp::Silence, 0, 2000).unwrap();
        let after_first = state.stats(0, 0, FIXTURE_FRAMES).unwrap();
        state.edit(EditOp::Silence, 6000, FIXTURE_FRAMES).unwrap();

        state.undo().unwrap();
        assert_eq!(
            state.stats(0, 0, FIXTURE_FRAMES).unwrap(),
            after_first,
            "second undone"
        );
        state.undo().unwrap();
        assert_eq!(
            state.stats(0, 0, FIXTURE_FRAMES).unwrap(),
            before,
            "first undone"
        );
    }

    #[test]
    fn an_undo_too_large_to_record_is_reported_not_hidden() {
        // The cap exists so "Select all -> Normalize" on a multi-hour file cannot snapshot
        // gigabytes. When it bites, the UI must be told, or Undo lies.
        let src = tmp("big.wav");
        let cache = tmp("big.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);
        // One channel of > MAX_UNDO_BYTES / 4 samples.
        let n = MAX_UNDO_BYTES / std::mem::size_of::<f32>() + 1024;
        write_wav(&src, std::slice::from_ref(&vec![0.5f32; n]), 8000);
        let state = AudioState::default();
        state.open(&src, &cache).unwrap();

        let dto = state.edit(EditOp::Silence, 0, n).unwrap();
        assert!(!dto.last_undoable, "an oversized edit must report itself");
        assert!(!dto.can_undo, "and must not leave a bogus undo entry");
        // The edit itself still happened.
        assert_eq!(state.stats(0, 0, n).unwrap().peak, 0.0);
        assert!(matches!(state.undo(), Err(AudioError::NothingToUndo)));
    }

    #[test]
    fn the_undo_stack_stays_under_its_memory_cap() {
        let src = tmp("cap.wav");
        let cache = tmp("cap.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);
        // Each edit below snapshots ~40% of the cap, so the third must evict the first.
        let n = (MAX_UNDO_BYTES / std::mem::size_of::<f32>()) * 2 / 5;
        write_wav(&src, std::slice::from_ref(&vec![0.5f32; n]), 8000);
        let state = AudioState::default();
        state.open(&src, &cache).unwrap();

        for _ in 0..3 {
            assert!(state.edit(EditOp::Silence, 0, n).unwrap().last_undoable);
        }
        let guard = state.lock();
        let doc = guard.as_ref().unwrap();
        let total: usize = doc.undo.iter().map(|e| e.bytes()).sum();
        assert!(total <= MAX_UNDO_BYTES, "{total} bytes over the cap");
        assert!(doc.undo.len() < 3, "oldest entry should have been evicted");
    }

    // ---------- trim ----------

    #[test]
    fn trim_keeps_only_the_selection_and_reports_the_new_geometry() {
        let (state, _c, _old) = open_stereo();
        let kept = state.stats(0, 2000, 7000).unwrap();
        let new_cache = tmp("trimmed.pcm");
        let _c2 = Cleanup(vec![new_cache.clone()]);

        let dto = state.trim(2000, 7000, &new_cache).unwrap();
        assert_eq!(dto.info.frames, 5000);
        assert!((dto.info.duration_s - 5000.0 / 8000.0).abs() < 1e-9);
        assert_eq!(state.info().unwrap().frames, 5000);

        // The kept audio is the same audio, now at the front.
        let now = state.stats(0, 0, 5000).unwrap();
        assert_eq!(now.n, kept.n);
        assert_eq!(now.min, kept.min);
        assert_eq!(now.max, kept.max);
        assert!((now.rms - kept.rms).abs() < 1e-9);
    }

    #[test]
    fn trim_replaces_the_cache_file_and_removes_the_old_one_only_on_undo() {
        let (state, _c, old_cache) = open_stereo();
        let new_cache = tmp("trim2.pcm");
        let _c2 = Cleanup(vec![new_cache.clone()]);

        state.trim(1000, 6000, &new_cache).unwrap();
        assert!(new_cache.exists(), "the trimmed cache must exist");
        assert!(
            old_cache.exists(),
            "the old cache is the undo record — it must survive the trim"
        );

        state.undo().unwrap();
        assert_eq!(
            state.info().unwrap().frames,
            FIXTURE_FRAMES,
            "geometry restored"
        );
        assert!(old_cache.exists(), "restored document uses the old cache");
        assert!(
            !new_cache.exists(),
            "the trimmed cache should be cleaned up"
        );
    }

    #[test]
    fn an_undone_trim_restores_the_audio_itself() {
        let (state, _c, _old) = open_stereo();
        let before: Vec<StatsDto> = (0..2)
            .map(|ch| state.stats(ch, 0, FIXTURE_FRAMES).unwrap())
            .collect();
        let new_cache = tmp("trim3.pcm");
        let _c2 = Cleanup(vec![new_cache.clone()]);

        state.trim(3000, 4000, &new_cache).unwrap();
        state.undo().unwrap();
        for (ch, want) in before.iter().enumerate() {
            assert_eq!(
                &state.stats(ch, 0, FIXTURE_FRAMES).unwrap(),
                want,
                "channel {ch}"
            );
        }
    }

    #[test]
    fn a_dropped_trim_undo_entry_takes_its_cache_file_with_it() {
        // The old cache outlives the document that owned it, so nothing else would ever
        // remove it. Closing the document must not leak a gigabyte.
        let (state, _c, old_cache) = open_stereo();
        let new_cache = tmp("trim4.pcm");
        let _c2 = Cleanup(vec![new_cache.clone()]);
        state.trim(0, 5000, &new_cache).unwrap();
        assert!(old_cache.exists());

        state.close();
        assert!(!old_cache.exists(), "undo's cache file leaked on close");
        assert!(
            !new_cache.exists(),
            "the document's own cache leaked on close"
        );
    }

    #[test]
    fn trimming_to_the_whole_file_is_a_no_op() {
        let (state, _c, _old) = open_stereo();
        let new_cache = tmp("trim5.pcm");
        let dto = state.trim(0, FIXTURE_FRAMES, &new_cache).unwrap();
        assert_eq!(dto.info.frames, FIXTURE_FRAMES);
        assert!(!dto.last_undoable);
        assert!(!new_cache.exists(), "a no-op trim must not burn a copy");
    }

    #[test]
    fn a_trim_discards_earlier_undo_entries() {
        // Their `start` indexes the untrimmed document; applying one afterwards would write
        // samples at the wrong offset.
        let (state, _c, _old) = open_stereo();
        let new_cache = tmp("trim6.pcm");
        let _c2 = Cleanup(vec![new_cache.clone()]);
        state.edit(EditOp::Silence, 7000, FIXTURE_FRAMES).unwrap();
        state.trim(0, 5000, &new_cache).unwrap();

        state.undo().unwrap(); // undoes the trim
        assert_eq!(state.info().unwrap().frames, FIXTURE_FRAMES);
        assert!(!state.can_undo(), "pre-trim entries must not survive");
    }

    #[test]
    fn trim_rejects_an_empty_selection_and_a_busy_document() {
        let (state, _c, _old) = open_stereo();
        let new_cache = tmp("trim7.pcm");
        assert!(matches!(
            state.trim(400, 400, &new_cache),
            Err(AudioError::EmptyRange)
        ));
        let held = state.pcm().unwrap();
        assert!(matches!(
            state.trim(0, 5000, &new_cache),
            Err(AudioError::Busy)
        ));
        drop(held);
        assert!(!new_cache.exists(), "a rejected trim must not leave a file");
    }

    #[test]
    fn edit_dtos_serialize_as_the_ui_expects() {
        let (state, _c, _p) = open_stereo();
        let dto = serde_json::to_value(state.edit(EditOp::Normalize, 0, 100).unwrap()).unwrap();
        assert_eq!(dto["canUndo"], true);
        assert_eq!(dto["lastUndoable"], true);
        assert_eq!(dto["info"]["frames"], FIXTURE_FRAMES);
        // EditOp arrives from JS as a camelCase string.
        assert_eq!(
            serde_json::from_str::<EditOp>("\"fadeIn\"").unwrap(),
            EditOp::FadeIn
        );
        assert_eq!(
            serde_json::from_str::<EditOp>("\"normalize\"").unwrap(),
            EditOp::Normalize
        );
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
