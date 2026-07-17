//! Write live-captured audio into a planar PCM cache (task 15).
//!
//! This is the recording counterpart of [`crate::decode`]: where `decode_file` turns a
//! compressed *file* into a memory-mappable planar `f32` cache, [`CaptureWriter`] turns a live
//! *stream* of interleaved `f32` frames — as delivered by a recording device — into the exact
//! same on-disk layout, so a finished recording opens as a document with no extra decode step.
//!
//! ## Why it mirrors `decode`'s spill/concat
//!
//! A recording has no known length while it runs, and — like a multi-hour source file — must
//! not be held in RAM. So each channel is spilled to its own temporary file as frames arrive,
//! and the spills are concatenated into the final planar cache when recording stops. Nothing
//! larger than one push is ever buffered, so an arbitrarily long take is fine. This reuses the
//! very same [`spill_path`](crate::decode::spill_path) /
//! [`concat_spills`](crate::decode::concat_spills) machinery the decoder uses, so both paths
//! produce a byte-identical planar layout.
//!
//! ## What this is *not*
//!
//! It knows nothing about audio hardware, `cpal`, threads or realtime callbacks — it is pure
//! file I/O over plain `&[f32]`, fully unit-testable without a sound card. The realtime input
//! wiring (ring buffer, feeder thread, device config) lives in the shell's `recorder` module.

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::decode::{concat_spills, spill_path, DecodeError, PcmCache};

/// Accumulates interleaved `f32` frames into per-channel spill files, then concatenates them
/// into a planar PCM cache — the recording analogue of the decoder's spill pipeline.
///
/// Feed it with [`push_interleaved`](CaptureWriter::push_interleaved) as frames arrive, then
/// call [`finish`](CaptureWriter::finish) to seal the planar cache. Dropping a writer that was
/// never finished (an aborted recording, or an error mid-stream) removes its spill files; it
/// never touches the final cache path, which `finish` is the only thing to create.
pub struct CaptureWriter {
    /// Final planar cache path. Created only by [`finish`](CaptureWriter::finish).
    cache: PathBuf,
    channels: usize,
    sample_rate: u32,
    /// One buffered spill writer per channel, in channel order.
    spills: Vec<BufWriter<File>>,
    /// Paths of those spills, kept so [`Drop`] can reap them on an aborted recording.
    spill_paths: Vec<PathBuf>,
    /// De-interleave scratch, one `Vec` per channel; reused across pushes to avoid allocating
    /// per callback batch.
    scratch: Vec<Vec<f32>>,
    /// Frames written so far (samples per channel).
    frames: usize,
}

impl CaptureWriter {
    /// Open a fresh writer that will spill `channels` channels at `sample_rate` and seal the
    /// planar cache at `cache`.
    ///
    /// `cache` must be unique (as [`crate::decode::decode_file`]'s is): its sibling
    /// `.chN.tmp` spill files are created immediately and would collide otherwise.
    ///
    /// # Panics
    /// Panics if `channels` is zero — a zero-channel recording is meaningless and every later
    /// index would divide by it.
    pub fn new(
        cache: impl AsRef<Path>,
        channels: usize,
        sample_rate: u32,
    ) -> Result<Self, DecodeError> {
        assert!(channels > 0, "channels must be non-zero");
        let cache = cache.as_ref().to_path_buf();
        let mut spills = Vec::with_capacity(channels);
        let mut spill_paths = Vec::with_capacity(channels);
        let mut scratch = Vec::with_capacity(channels);
        for ch in 0..channels {
            let p = spill_path(&cache, ch);
            spills.push(BufWriter::new(File::create(&p)?));
            spill_paths.push(p);
            scratch.push(Vec::new());
        }
        Ok(CaptureWriter {
            cache,
            channels,
            sample_rate,
            spills,
            spill_paths,
            scratch,
            frames: 0,
        })
    }

    /// Number of channels this recording captures.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Sampling rate of this recording, in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Frames captured so far (samples per channel).
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Append a batch of interleaved `f32` frames (`[l, r, l, r, …]` for stereo).
    ///
    /// Only whole frames are consumed: any trailing partial frame in `interleaved` (which a
    /// device never actually delivers, but a caller could) is ignored rather than smeared
    /// across channels. De-interleaves into per-channel scratch and appends each channel to
    /// its spill file.
    pub fn push_interleaved(&mut self, interleaved: &[f32]) -> Result<(), DecodeError> {
        let nch = self.channels;
        let frames = interleaved.len() / nch;
        if frames == 0 {
            return Ok(());
        }
        for v in self.scratch.iter_mut() {
            v.clear();
        }
        for f in 0..frames {
            let base = f * nch;
            for (ch, s) in self.scratch.iter_mut().enumerate() {
                s.push(interleaved[base + ch]);
            }
        }
        for (w, s) in self.spills.iter_mut().zip(self.scratch.iter()) {
            w.write_all(bytemuck::cast_slice(s))?;
        }
        self.frames += frames;
        Ok(())
    }

    /// Seal the recording: flush the spills and concatenate them into the planar cache,
    /// returning the memory-mapped [`PcmCache`] ready for analysis and playback.
    ///
    /// A recording that captured no frames yields [`DecodeError::Empty`] (an empty cache
    /// cannot be mapped) and creates no cache file — the caller can treat "recorded nothing"
    /// as a non-event rather than a corrupt document. On any outcome the spill files are
    /// reaped, here on success and by [`Drop`] on the error paths.
    pub fn finish(mut self) -> Result<PcmCache, DecodeError> {
        // Flush and close every spill before concatenating: the mono fast path renames the
        // spill onto the cache, which needs its buffered tail on disk first.
        for w in self.spills.iter_mut() {
            w.flush()?;
        }
        self.spills.clear();

        if self.frames == 0 {
            // No cache file was created; `Drop` reaps the (empty) spills.
            return Err(DecodeError::Empty);
        }

        concat_spills(&self.cache, &self.spill_paths, self.channels)?;
        // Best-effort: the multi-channel concat copies rather than renames, so the spills
        // survive it. The mono path already renamed spill 0 away (a NotFound here is fine).
        for p in &self.spill_paths {
            let _ = fs::remove_file(p);
        }
        PcmCache::open_planar(&self.cache, self.channels, self.sample_rate)
    }
}

impl Drop for CaptureWriter {
    fn drop(&mut self) {
        // Reap the per-channel spill files of an aborted or failed recording. `finish` already
        // removes them on success, so this only bites on the paths it did not reach; the final
        // cache file is never touched here (only `finish` creates it). Best-effort and silent,
        // as in `decode`: a destructor must not panic, and a leaked spill is harmless — it is
        // scratch, not the document, and cannot be mistaken for a PCM cache by the reaper.
        for p in &self.spill_paths {
            let _ = fs::remove_file(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique scratch path in the OS temp dir (mirrors `decode`'s test helper).
    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sfcap-{tag}-{}-{n}", std::process::id()))
    }

    struct Cleanup(Vec<PathBuf>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for p in &self.0 {
                let _ = fs::remove_file(p);
            }
        }
    }

    #[test]
    fn mono_capture_round_trips_through_the_planar_cache() {
        let cache = tmp("mono.cache");
        let _c = Cleanup(vec![cache.clone()]);
        let mut w = CaptureWriter::new(&cache, 1, 48_000).unwrap();
        // Two separate batches, to prove appends across pushes are contiguous.
        w.push_interleaved(&[0.0, 0.1, 0.2]).unwrap();
        w.push_interleaved(&[0.3, 0.4]).unwrap();
        assert_eq!(w.frames(), 5);

        let pcm = w.finish().unwrap();
        assert_eq!(pcm.channels(), 1);
        assert_eq!(pcm.sample_rate(), 48_000);
        assert_eq!(pcm.frames(), 5);
        assert_eq!(pcm.channel(0), &[0.0, 0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn stereo_capture_deinterleaves_into_planar_channels() {
        let cache = tmp("stereo.cache");
        let _c = Cleanup(vec![cache.clone()]);
        let mut w = CaptureWriter::new(&cache, 2, 44_100).unwrap();
        // Interleaved L/R; L ascends, R descends, so a de-interleave slip is obvious.
        w.push_interleaved(&[1.0, -1.0, 2.0, -2.0]).unwrap();
        w.push_interleaved(&[3.0, -3.0]).unwrap();
        assert_eq!(w.frames(), 3);

        let pcm = w.finish().unwrap();
        assert_eq!(pcm.channels(), 2);
        assert_eq!(pcm.channel(0), &[1.0, 2.0, 3.0]);
        assert_eq!(pcm.channel(1), &[-1.0, -2.0, -3.0]);
    }

    #[test]
    fn a_trailing_partial_frame_is_ignored_not_smeared() {
        // A device always delivers whole frames, but if a caller passes a ragged length the
        // dangling sample must not become channel 0 of a phantom frame.
        let cache = tmp("ragged.cache");
        let _c = Cleanup(vec![cache.clone()]);
        let mut w = CaptureWriter::new(&cache, 2, 8_000).unwrap();
        w.push_interleaved(&[1.0, 2.0, 3.0]).unwrap(); // 1.5 frames -> one frame
        assert_eq!(w.frames(), 1);
        let pcm = w.finish().unwrap();
        assert_eq!(pcm.channel(0), &[1.0]);
        assert_eq!(pcm.channel(1), &[2.0]);
    }

    #[test]
    fn finishing_an_empty_recording_is_empty_and_leaves_no_cache() {
        let cache = tmp("empty.cache");
        let _c = Cleanup(vec![cache.clone()]);
        let w = CaptureWriter::new(&cache, 1, 48_000).unwrap();
        assert!(matches!(w.finish(), Err(DecodeError::Empty)));
        assert!(
            !cache.exists(),
            "an empty recording must not create a cache file"
        );
    }

    #[test]
    fn a_push_of_only_a_partial_frame_records_nothing() {
        let cache = tmp("partial.cache");
        let _c = Cleanup(vec![cache.clone()]);
        let mut w = CaptureWriter::new(&cache, 2, 8_000).unwrap();
        w.push_interleaved(&[0.5]).unwrap(); // half a stereo frame
        assert_eq!(w.frames(), 0);
        assert!(matches!(w.finish(), Err(DecodeError::Empty)));
    }

    #[test]
    fn dropping_an_unfinished_writer_reaps_its_spills() {
        let cache = tmp("aborted.cache");
        let _c = Cleanup(vec![cache.clone()]);
        let mut w = CaptureWriter::new(&cache, 2, 8_000).unwrap();
        w.push_interleaved(&[1.0, 2.0]).unwrap();
        let spills = [spill_path(&cache, 0), spill_path(&cache, 1)];
        assert!(
            spills.iter().all(|p| p.exists()),
            "spills should exist mid-recording"
        );

        drop(w); // aborted: never finished
        assert!(
            spills.iter().all(|p| !p.exists()),
            "an aborted recording must not leak its spill files"
        );
        assert!(!cache.exists(), "and must never create the final cache");
    }

    #[test]
    fn finishing_reaps_the_spills_too() {
        // A successful stereo finish copies (not renames) the spills, so it must clean them up.
        let cache = tmp("cleanup.cache");
        let _c = Cleanup(vec![cache.clone()]);
        let mut w = CaptureWriter::new(&cache, 2, 8_000).unwrap();
        w.push_interleaved(&[1.0, 2.0, 3.0, 4.0]).unwrap();
        let spills = [spill_path(&cache, 0), spill_path(&cache, 1)];
        w.finish().unwrap();
        assert!(
            spills.iter().all(|p| !p.exists()),
            "finish must not leave spill files behind"
        );
        assert!(cache.exists());
    }

    #[test]
    fn a_finished_cache_reopens_with_the_recorded_geometry() {
        // The whole point: a recording is indistinguishable from a decoded file on disk.
        let cache = tmp("reopen.cache");
        let _c = Cleanup(vec![cache.clone()]);
        let mut w = CaptureWriter::new(&cache, 2, 22_050).unwrap();
        for f in 0..500i32 {
            w.push_interleaved(&[f as f32, -(f as f32)]).unwrap();
        }
        w.finish().unwrap();

        let pcm = PcmCache::open_planar(&cache, 2, 22_050).unwrap();
        assert_eq!(pcm.frames(), 500);
        assert_eq!(pcm.channel(0)[499], 499.0);
        assert_eq!(pcm.channel(1)[499], -499.0);
    }

    #[test]
    #[should_panic(expected = "channels must be non-zero")]
    fn zero_channels_is_rejected() {
        let _ = CaptureWriter::new(tmp("zero.cache"), 0, 48_000);
    }
}
