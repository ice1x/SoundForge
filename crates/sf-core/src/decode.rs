//! Decode any [`symphonia`]-supported audio file into an on-disk **PCM cache** and open
//! it through [`memmap2`]. This is task 10: the bridge between a compressed source file
//! (WAV/FLAC/MP3/AAC/OGG/ALAC/…) and the [`Analyzer`](crate::summary::Analyzer), which
//! needs a flat `&[f32]` per channel.
//!
//! ## Why a memory-mapped cache
//!
//! SoundForge must stay responsive on multi-hour / multi-gigabyte files without holding
//! the decoded PCM in RAM. So decoding writes the samples to a cache file on disk, and
//! reading goes through a memory map: the OS pages samples in on demand and evicts them
//! under pressure, so a 2-hour file costs almost no resident memory yet every sample is a
//! cheap `&[f32]` index away. The summary pyramid the analyzer builds over that slice is
//! only a few percent on top (see [`crate::summary`]).
//!
//! ## On-disk layout: planar f32
//!
//! The cache is raw little/native-endian `f32`, laid out **planar** (channel-major):
//! all of channel 0's samples, then all of channel 1's, etc. Planar (not interleaved) is
//! what lets each channel be handed to the analyzer as one contiguous slice with zero
//! copying — [`PcmCache::channel`] just sub-slices the map.
//!
//! Decoding is streaming and memory-bounded: each channel is spilled to its own temporary
//! file as packets arrive, then the spills are concatenated into the final cache. Nothing
//! larger than a decode packet is ever buffered, so arbitrarily long inputs are fine.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use memmap2::MmapMut;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::summary::Analyzer;

/// Anything that can go wrong turning a source file into a [`PcmCache`].
#[derive(Debug)]
pub enum DecodeError {
    /// An I/O error reading the source or writing the cache.
    Io(io::Error),
    /// The container/codec could not be probed or decoded.
    Symphonia(SymphoniaError),
    /// The container had no audio track to decode.
    NoAudioTrack,
    /// The source decoded to zero frames (an empty cache cannot be memory-mapped).
    Empty,
    /// A raw cache file's size is inconsistent with the requested channel geometry
    /// (not a whole number of `f32` samples, or not divisible by `channels`).
    Malformed(String),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Io(e) => write!(f, "i/o error: {e}"),
            DecodeError::Symphonia(e) => write!(f, "decode error: {e}"),
            DecodeError::NoAudioTrack => write!(f, "no audio track in source"),
            DecodeError::Empty => write!(f, "source decoded to zero frames"),
            DecodeError::Malformed(m) => write!(f, "malformed cache: {m}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<io::Error> for DecodeError {
    fn from(e: io::Error) -> Self {
        DecodeError::Io(e)
    }
}

impl From<SymphoniaError> for DecodeError {
    fn from(e: SymphoniaError) -> Self {
        DecodeError::Symphonia(e)
    }
}

/// A decoded, memory-mapped PCM cache: planar `f32` samples on disk, one contiguous run
/// per channel, exposed as cheap `&[f32]` slices for the [`Analyzer`].
///
/// The map borrows the OS page cache, so constructing a cache over a huge file is fast and
/// resident memory stays low regardless of file size.
pub struct PcmCache {
    mmap: MmapMut,
    channels: usize,
    sample_rate: u32,
    /// Samples per channel (a.k.a. frames).
    frames: usize,
}

impl PcmCache {
    /// Number of audio channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Sampling rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Number of samples per channel (frames).
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// True if the cache holds no samples.
    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// The whole map reinterpreted as `f32`. Length is `channels * frames`.
    fn as_f32(&self) -> &[f32] {
        // The map is page-aligned (so `f32`-aligned) and its length is a whole number of
        // `f32`s by construction, so this reinterpret cannot fail.
        bytemuck::cast_slice(&self.mmap)
    }

    /// The contiguous samples of channel `ch` (`0..channels`), in order.
    ///
    /// # Panics
    /// Panics if `ch >= channels`.
    pub fn channel(&self, ch: usize) -> &[f32] {
        assert!(ch < self.channels, "channel {ch} out of range");
        let start = ch * self.frames;
        &self.as_f32()[start..start + self.frames]
    }

    /// The samples of channel `ch`, mutably — the buffer edits are applied to.
    ///
    /// Writes land in the memory-mapped cache file, which is this document's backing store.
    ///
    /// # Warning
    /// Changing samples invalidates any [`Pyramid`] built over this channel, and a pyramid of
    /// the right length with stale contents is undetectable — see [`Analyzer::with_pyramid`].
    /// Rebuild it for every channel you touch.
    ///
    /// # Panics
    /// Panics if `ch >= channels`.
    pub fn channel_mut(&mut self, ch: usize) -> &mut [f32] {
        assert!(ch < self.channels, "channel {ch} out of range");
        let start = ch * self.frames;
        let frames = self.frames;
        &mut self.as_f32_mut()[start..start + frames]
    }

    /// The whole map reinterpreted as mutable `f32`. Length is `channels * frames`.
    fn as_f32_mut(&mut self) -> &mut [f32] {
        // Same invariant as `as_f32`: page-aligned and a whole number of f32s by construction.
        bytemuck::cast_slice_mut(&mut self.mmap)
    }

    /// Flush pending writes to the cache file.
    ///
    /// Not needed for correctness within one run — reads go through the same map — but it is
    /// what makes an edit durable if the process dies, and it surfaces a full disk as an
    /// error here rather than as a silent loss.
    pub fn flush(&self) -> Result<(), DecodeError> {
        self.mmap.flush()?;
        Ok(())
    }

    /// Build an [`Analyzer`] over channel `ch`, ready to answer seamless range queries.
    ///
    /// # Panics
    /// Panics if `ch >= channels`.
    pub fn analyzer(&self, ch: usize) -> Analyzer<'_> {
        Analyzer::new(self.channel(ch))
    }

    /// Open an existing raw planar cache file (as written by [`decode_file`]).
    ///
    /// The on-disk format carries no header, so the caller must supply the `channels` and
    /// `sample_rate` that were used to write it. Useful for reloading a cache and in tests.
    pub fn open_planar(
        path: impl AsRef<Path>,
        channels: usize,
        sample_rate: u32,
    ) -> Result<Self, DecodeError> {
        assert!(channels > 0, "channels must be non-zero");
        // Opened read+write because the map is writable: edits (task 16) change samples in
        // place, and the cache file is this document's backing store, private to the app.
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let len = file.metadata()?.len() as usize;
        if len == 0 {
            return Err(DecodeError::Empty);
        }
        // Validate the geometry up front so `as_f32`'s reinterpret can never panic and the
        // channel slicing can never misalign on a truncated or foreign file.
        let fsize = std::mem::size_of::<f32>();
        if !len.is_multiple_of(fsize) {
            return Err(DecodeError::Malformed(format!(
                "length {len} bytes is not a whole number of {fsize}-byte f32 samples"
            )));
        }
        let total = len / fsize;
        if !total.is_multiple_of(channels) {
            return Err(DecodeError::Malformed(format!(
                "{total} samples is not divisible by {channels} channels"
            )));
        }
        // SAFETY: the cache file is created by this process, is unique per open, and is
        // never touched by anything else while mapped. The map is shared (not copy-on-write)
        // on purpose: an edit belongs in the document's backing store, and the OS pages the
        // dirty parts out lazily rather than holding a whole edited file in RAM.
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        Ok(PcmCache {
            mmap,
            channels,
            sample_rate,
            frames: total / channels,
        })
    }
}

/// Sibling temp path for channel `ch`'s spill file next to the final `cache` path.
///
/// Shared with [`crate::capture`], which spills a live recording the same way a decode does.
pub(crate) fn spill_path(cache: &Path, ch: usize) -> PathBuf {
    let mut s = cache.as_os_str().to_owned();
    s.push(format!(".ch{ch}.tmp"));
    PathBuf::from(s)
}

/// Decode `src` (any symphonia-supported format) into a planar `f32` PCM cache written to
/// `cache`, and return it memory-mapped and ready for analysis.
///
/// `cache` is overwritten if it exists. Decoding streams packet-by-packet and never holds
/// the whole file in memory, so multi-hour inputs are fine. Returns [`DecodeError::Empty`]
/// if the source has no samples (an empty file cannot be mapped).
///
/// Only the first logical stream of the first audio track is decoded. Chained streams
/// (e.g. several concatenated OGG logical bitstreams, as in some internet-radio captures)
/// stop at the chain boundary — see the `ResetRequired` handling in the decode loop.
pub fn decode_file(
    src: impl AsRef<Path>,
    cache: impl AsRef<Path>,
) -> Result<PcmCache, DecodeError> {
    let src = src.as_ref();
    let cache = cache.as_ref();

    // Probe the container/codec.
    let file = File::open(src)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = src.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let mut format = probed.format;

    // Pick the first decodable audio track.
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or(DecodeError::NoAudioTrack)?;
    let track_id = track.id;
    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    // Discovered from the first decoded packet's signal spec.
    let mut channels = 0usize;
    let mut sample_rate = 0u32;
    let mut spills: Vec<BufWriter<File>> = Vec::new();
    let mut spill_paths: Vec<PathBuf> = Vec::new();
    // Reused per packet: interleaved f32 from the decoder, then per-channel scratch.
    let mut sample_buf: Option<SampleBuffer<f32>> = None;
    let mut scratch: Vec<Vec<f32>> = Vec::new();

    let result = decode_loop(
        &mut *format,
        &mut *decoder,
        track_id,
        cache,
        &mut channels,
        &mut sample_rate,
        &mut spills,
        &mut spill_paths,
        &mut sample_buf,
        &mut scratch,
    );

    // Whatever happened, flush and drop the spill writers before we concat or clean up.
    let flush_result = spills
        .iter_mut()
        .try_for_each(|w| w.flush())
        .map_err(DecodeError::from);
    drop(spills);

    let finish = result.and(flush_result).and_then(|()| {
        if channels == 0 || sample_rate == 0 {
            return Err(DecodeError::Empty);
        }
        concat_spills(cache, &spill_paths, channels)?;
        PcmCache::open_planar(cache, channels, sample_rate)
    });

    // Best-effort cleanup of the per-channel spill files (the concat consumed them, but a
    // failure path may leave them behind).
    for p in &spill_paths {
        let _ = fs::remove_file(p);
    }

    finish
}

/// The streaming decode loop, factored out so the caller can guarantee spill cleanup on
/// every exit path.
#[allow(clippy::too_many_arguments)]
fn decode_loop(
    format: &mut dyn symphonia::core::formats::FormatReader,
    decoder: &mut dyn symphonia::core::codecs::Decoder,
    track_id: u32,
    cache: &Path,
    channels: &mut usize,
    sample_rate: &mut u32,
    spills: &mut Vec<BufWriter<File>>,
    spill_paths: &mut Vec<PathBuf>,
    sample_buf: &mut Option<SampleBuffer<f32>>,
    scratch: &mut Vec<Vec<f32>>,
) -> Result<(), DecodeError> {
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // Clean end-of-stream.
            Err(SymphoniaError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            // A new chained logical stream begins here; we stop at the boundary rather than
            // re-examining tracks and rebuilding the decoder. Documented on `decode_file`.
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // A single corrupt packet is skippable; keep going.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(e.into()),
        };

        // Lazily discover the channel layout / rate and open the spill writers.
        if spills.is_empty() {
            let spec = *decoded.spec();
            *channels = spec.channels.count();
            *sample_rate = spec.rate;
            if *channels == 0 {
                return Err(DecodeError::NoAudioTrack);
            }
            for ch in 0..*channels {
                let p = spill_path(cache, ch);
                spills.push(BufWriter::new(File::create(&p)?));
                spill_paths.push(p);
                scratch.push(Vec::new());
            }
        }

        // Copy the decoder's native samples into an interleaved f32 buffer.
        let sb = sample_buf.get_or_insert_with(|| {
            SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec())
        });
        sb.copy_interleaved_ref(decoded);
        let inter = sb.samples();
        let nch = *channels;
        let frames = inter.len() / nch;

        // De-interleave into per-channel scratch, then append each to its spill file.
        for v in scratch.iter_mut() {
            v.clear();
        }
        for f in 0..frames {
            let base = f * nch;
            for (ch, s) in scratch.iter_mut().enumerate() {
                s.push(inter[base + ch]);
            }
        }
        for (w, s) in spills.iter_mut().zip(scratch.iter()) {
            w.write_all(bytemuck::cast_slice(s))?;
        }
    }
    Ok(())
}

/// Concatenate the per-channel spill files into the final planar `cache` file (channel 0,
/// then channel 1, …). For a single channel this is just a rename, avoiding a full copy.
///
/// Shared with [`crate::capture`], whose recording writer produces the same per-channel spills.
pub(crate) fn concat_spills(
    cache: &Path,
    spill_paths: &[PathBuf],
    channels: usize,
) -> Result<(), DecodeError> {
    if channels == 1 {
        // Fast path: the lone spill already *is* the planar layout.
        fs::rename(&spill_paths[0], cache)?;
        return Ok(());
    }
    let mut out = BufWriter::new(File::create(cache)?);
    for p in spill_paths {
        let mut r = File::open(p)?;
        io::copy(&mut r, &mut out)?;
    }
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::PI;
    use std::path::PathBuf;

    /// A unique-ish scratch path inside the OS temp dir (no external rand crate: fold the
    /// test name and a per-call counter — enough to keep concurrent tests from colliding).
    fn tmp(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("sfcore-{tag}-{pid}-{n}"))
    }

    /// Guard that removes a set of paths on drop, so tests don't litter temp files.
    struct Cleanup(Vec<PathBuf>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for p in &self.0 {
                let _ = fs::remove_file(p);
            }
        }
    }

    /// Write a 32-bit float WAV fixture with `channels` planar sample vectors.
    fn write_wav(path: &Path, channels: &[Vec<f32>], sample_rate: u32) {
        let spec = hound::WavSpec {
            channels: channels.len() as u16,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        let frames = channels[0].len();
        for f in 0..frames {
            for ch in channels {
                w.write_sample(ch[f]).unwrap();
            }
        }
        w.finalize().unwrap();
    }

    #[test]
    fn decode_mono_wav_roundtrip() {
        let src = tmp("mono-src.wav");
        let cache = tmp("mono.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);

        let sr = 48_000u32;
        let mono: Vec<f32> = (0..4096)
            .map(|i| (2.0 * PI * 440.0 * i as f32 / sr as f32).sin())
            .collect();
        write_wav(&src, std::slice::from_ref(&mono), sr);

        let pc = decode_file(&src, &cache).unwrap();
        assert_eq!(pc.channels(), 1);
        assert_eq!(pc.sample_rate(), sr);
        assert_eq!(pc.frames(), mono.len());
        assert!(!pc.is_empty());

        let got = pc.channel(0);
        assert_eq!(got.len(), mono.len());
        for (i, (&g, &w)) in got.iter().zip(mono.iter()).enumerate() {
            assert!((g - w).abs() < 1e-6, "sample {i}: {g} vs {w}");
        }
    }

    #[test]
    fn decode_stereo_wav_deinterleaves() {
        let src = tmp("stereo-src.wav");
        let cache = tmp("stereo.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);

        let sr = 44_100u32;
        // Two clearly distinct channels so a de-interleave bug is obvious.
        let left: Vec<f32> = (0..3000).map(|i| (i as f32 * 0.001).sin()).collect();
        let right: Vec<f32> = (0..3000).map(|i| -(i as f32 * 0.002).cos()).collect();
        write_wav(&src, &[left.clone(), right.clone()], sr);

        let pc = decode_file(&src, &cache).unwrap();
        assert_eq!(pc.channels(), 2);
        assert_eq!(pc.sample_rate(), sr);
        assert_eq!(pc.frames(), 3000);

        let (gl, gr) = (pc.channel(0), pc.channel(1));
        for i in 0..3000 {
            assert!((gl[i] - left[i]).abs() < 1e-6, "L[{i}]");
            assert!((gr[i] - right[i]).abs() < 1e-6, "R[{i}]");
        }
    }

    #[test]
    fn decode_feeds_analyzer() {
        use crate::stats::RangeStats;

        let src = tmp("sine-src.wav");
        let cache = tmp("sine.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);

        // Exactly 1000 Hz over 1 s at 48 kHz -> whole cycles, like the stats unit test.
        let sr = 48_000u32;
        let mono: Vec<f32> = (0..sr)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / sr as f32).sin())
            .collect();
        write_wav(&src, std::slice::from_ref(&mono), sr);

        let pc = decode_file(&src, &cache).unwrap();
        let az = pc.analyzer(0);
        let st = RangeStats::from_agg(&az.range(0, pc.frames()), 0, pc.sample_rate());
        assert!(
            (st.rms - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-3,
            "rms {}",
            st.rms
        );
        assert!((st.freq_hz - 1000.0).abs() < 2.0, "freq {}", st.freq_hz);
    }

    #[test]
    fn open_planar_reopens_written_cache() {
        let src = tmp("reopen-src.wav");
        let cache = tmp("reopen.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);

        let sr = 22_050u32;
        let a: Vec<f32> = (0..500).map(|i| i as f32 / 500.0).collect();
        let b: Vec<f32> = (0..500).map(|i| -(i as f32) / 500.0).collect();
        write_wav(&src, &[a.clone(), b.clone()], sr);
        decode_file(&src, &cache).unwrap();

        // Reopen the raw cache with the same geometry and confirm the planar contents.
        let pc = PcmCache::open_planar(&cache, 2, sr).unwrap();
        assert_eq!(pc.frames(), 500);
        assert_eq!(pc.channel(0)[499], a[499]);
        assert_eq!(pc.channel(1)[499], b[499]);
    }

    #[test]
    fn open_planar_rejects_malformed_geometry() {
        use std::io::Write;
        // A file whose byte length is not a whole number of f32 samples.
        let ragged = tmp("ragged.pcm");
        let _c1 = Cleanup(vec![ragged.clone()]);
        File::create(&ragged).unwrap().write_all(&[0u8; 6]).unwrap();
        assert!(matches!(
            PcmCache::open_planar(&ragged, 1, 8000),
            Err(DecodeError::Malformed(_))
        ));

        // A whole number of samples, but not divisible by the channel count (3 f32 / 2ch).
        let odd = tmp("odd.pcm");
        let _c2 = Cleanup(vec![odd.clone()]);
        File::create(&odd).unwrap().write_all(&[0u8; 12]).unwrap();
        assert!(matches!(
            PcmCache::open_planar(&odd, 2, 8000),
            Err(DecodeError::Malformed(_))
        ));
    }

    #[test]
    fn channel_out_of_range_panics() {
        let src = tmp("oob-src.wav");
        let cache = tmp("oob.pcm");
        let _c = Cleanup(vec![src.clone(), cache.clone()]);
        write_wav(&src, std::slice::from_ref(&vec![0.1f32; 128]), 8000);
        let pc = decode_file(&src, &cache).unwrap();
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pc.channel(1)));
        assert!(r.is_err());
    }
}
