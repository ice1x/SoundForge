//! WAV export (task 17): write a range of the planar PCM to a `.wav` file via [`hound`].
//!
//! Like [`crate::edit`], this module is pure and range-agnostic: it takes one already-sliced
//! `&[f32]` per channel and does nothing else — no documents, no PCM cache, no I/O beyond the
//! one file it writes. "Export the selection" is therefore just slicing each channel to
//! `[start, end)` in the caller and handing the sub-slices here, exactly as the edits do.
//!
//! ## Interleaving
//!
//! The PCM cache is **planar** (channel-major; see [`crate::decode`]), but WAV is
//! **interleaved** (frame-major: L R L R …). So export walks the channels frame by frame
//! rather than copying a channel at a time. Reading straight from the memory-mapped planar
//! slices keeps this memory-bounded: nothing larger than the `hound` write buffer is held, so
//! a two-hour file exports without ever materialising in RAM.
//!
//! ## Format
//!
//! [`WavFormat::Float32`] writes the exact `f32` samples SoundForge holds internally — a
//! lossless round-trip. [`WavFormat::Pcm16`] writes 16-bit signed PCM, the universal
//! interchange format: samples are clamped to `[-1, 1]` and quantised, so anything hotter
//! than full scale hard-clips (normalize first).

use std::fmt;
use std::io;
use std::path::Path;

use hound::{SampleFormat, WavSpec, WavWriter};

/// The sample format a WAV export is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WavFormat {
    /// 16-bit signed PCM — the universal interchange format. Samples are clamped to `[-1, 1]`
    /// and quantised; anything hotter than full scale hard-clips.
    Pcm16,
    /// 32-bit float — lossless: the exact samples SoundForge holds internally.
    Float32,
}

impl WavFormat {
    /// The `hound` spec fields (bit depth + sample format) for this choice.
    fn spec_fields(self) -> (u16, SampleFormat) {
        match self {
            WavFormat::Pcm16 => (16, SampleFormat::Int),
            WavFormat::Float32 => (32, SampleFormat::Float),
        }
    }
}

/// Anything that can go wrong writing a [`WavFormat`] file.
#[derive(Debug)]
pub enum ExportError {
    /// The `hound` encoder (or the underlying file I/O) failed.
    Encode(hound::Error),
    /// No channels were given: a WAV must have at least one.
    NoChannels,
    /// The channel slices are not all the same length, so they cannot interleave into frames.
    RaggedChannels {
        channel: usize,
        len: usize,
        expected: usize,
    },
}

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExportError::Encode(e) => write!(f, "could not write WAV: {e}"),
            ExportError::NoChannels => write!(f, "no channels to export"),
            ExportError::RaggedChannels {
                channel,
                len,
                expected,
            } => write!(
                f,
                "channel {channel} has {len} samples, expected {expected}"
            ),
        }
    }
}

impl std::error::Error for ExportError {}

impl From<hound::Error> for ExportError {
    fn from(e: hound::Error) -> Self {
        ExportError::Encode(e)
    }
}

impl From<io::Error> for ExportError {
    fn from(e: io::Error) -> Self {
        ExportError::Encode(hound::Error::IoError(e))
    }
}

/// Map a normalized `f32` sample to 16-bit signed PCM.
///
/// Clamped to `[-1, 1]` first — a hotter sample would overflow the `i16` and wrap to the
/// opposite polarity, turning a loud peak into a full-scale click. `32767` (not `32768`)
/// keeps the mapping symmetric so `+1.0` and `-1.0` land on `±32767`, and `-1.0 * 32768`
/// cannot exceed `i16::MIN`.
fn to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Write `channels` (one already-sliced `&[f32]` per channel, all the same length) to `path`
/// as a WAV file in `format` at `sample_rate` Hz.
///
/// The slices are the exact samples exported, so restricting the export to a selection is the
/// caller's job: pass `&channel[start..end]` for each channel. Every channel must be the same
/// length (they are parallel planes of one document); a mismatch is
/// [`ExportError::RaggedChannels`] rather than a truncated file.
///
/// An existing `path` is overwritten.
pub fn export_wav(
    path: impl AsRef<Path>,
    channels: &[&[f32]],
    sample_rate: u32,
    format: WavFormat,
) -> Result<(), ExportError> {
    let frames = match channels.first() {
        Some(c) => c.len(),
        None => return Err(ExportError::NoChannels),
    };
    for (ch, slice) in channels.iter().enumerate() {
        if slice.len() != frames {
            return Err(ExportError::RaggedChannels {
                channel: ch,
                len: slice.len(),
                expected: frames,
            });
        }
    }

    let (bits_per_sample, sample_format) = format.spec_fields();
    let spec = WavSpec {
        channels: channels.len() as u16,
        sample_rate,
        bits_per_sample,
        sample_format,
    };
    let mut w = WavWriter::create(path, spec)?;
    // Frame-major (interleaved), reading across the planar channels one frame at a time.
    for f in 0..frames {
        for ch in channels {
            match format {
                WavFormat::Float32 => w.write_sample(ch[f])?,
                WavFormat::Pcm16 => w.write_sample(to_i16(ch[f]))?,
            }
        }
    }
    // finalize() writes the header's final sizes; a dropped writer that skipped it would leave
    // a WAV whose length fields lie, so surface any failure here rather than swallowing it.
    w.finalize()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Unique scratch path in the OS temp dir (mirrors the helper in `decode`).
    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sfcore-export-{tag}-{}-{n}", std::process::id()))
    }

    struct Cleanup(Vec<PathBuf>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for p in &self.0 {
                let _ = std::fs::remove_file(p);
            }
        }
    }

    /// Read every sample back as `f32`, interleaved as WAV stores them. hound requires the
    /// read type to match the file: a float WAV is read as `f32`, an int WAV as `i32` and
    /// scaled back to `[-1, 1]` by the same full-scale constant `export_wav` used.
    fn read_back(path: &Path) -> (hound::WavSpec, Vec<f32>) {
        let mut r = hound::WavReader::open(path).unwrap();
        let spec = r.spec();
        let samples = match spec.sample_format {
            SampleFormat::Float => r.samples::<f32>().map(|s| s.unwrap()).collect(),
            SampleFormat::Int => r
                .samples::<i32>()
                .map(|s| s.unwrap() as f32 / 32767.0)
                .collect(),
        };
        (spec, samples)
    }

    #[test]
    fn float32_export_is_a_lossless_round_trip() {
        let path = tmp("f32.wav");
        let _c = Cleanup(vec![path.clone()]);
        let mono: Vec<f32> = (0..1000).map(|i| (i as f32 * 0.01).sin() * 0.9).collect();

        export_wav(&path, &[&mono], 48_000, WavFormat::Float32).unwrap();

        let (spec, got) = read_back(&path);
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.bits_per_sample, 32);
        assert_eq!(spec.sample_format, SampleFormat::Float);
        assert_eq!(got.len(), mono.len());
        for (i, (&g, &w)) in got.iter().zip(mono.iter()).enumerate() {
            assert_eq!(g, w, "sample {i} changed on a lossless export");
        }
    }

    #[test]
    fn stereo_export_interleaves_the_planar_channels() {
        // Distinct constant channels so an interleave bug (L/R swapped or planar-not-frame
        // order) is unmistakable in the read-back.
        let path = tmp("stereo.wav");
        let _c = Cleanup(vec![path.clone()]);
        let left = vec![0.25f32; 4];
        let right = vec![-0.5f32; 4];

        export_wav(&path, &[&left, &right], 44_100, WavFormat::Float32).unwrap();

        let (spec, got) = read_back(&path);
        assert_eq!(spec.channels, 2);
        // WAV is frame-major: L R L R …
        assert_eq!(got, vec![0.25, -0.5, 0.25, -0.5, 0.25, -0.5, 0.25, -0.5]);
    }

    #[test]
    fn pcm16_quantises_and_reads_back_close() {
        let path = tmp("pcm16.wav");
        let _c = Cleanup(vec![path.clone()]);
        let mono = vec![0.0f32, 0.5, -0.5, 1.0, -1.0];

        export_wav(&path, &[&mono], 8000, WavFormat::Pcm16).unwrap();

        let (spec, got) = read_back(&path);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, SampleFormat::Int);
        assert_eq!(got.len(), mono.len());
        // 16-bit quantisation error is at most 1 LSB ≈ 1/32767.
        for (i, (&g, &w)) in got.iter().zip(mono.iter()).enumerate() {
            assert!((g - w).abs() < 2.0 / 32767.0, "sample {i}: {g} vs {w}");
        }
    }

    #[test]
    fn pcm16_clamps_hot_samples_instead_of_wrapping() {
        // A sample above full scale must clip to the rail, never wrap to the opposite polarity
        // (which an unclamped `* 32767 as i16` on, say, 1.5 would do).
        let path = tmp("clip.wav");
        let _c = Cleanup(vec![path.clone()]);
        let mono = vec![1.5f32, -1.5, 2.0, -3.0];

        export_wav(&path, &[&mono], 8000, WavFormat::Pcm16).unwrap();

        let (_spec, got) = read_back(&path);
        assert!(
            (got[0] - 1.0).abs() < 1e-4,
            "positive over-rail: {}",
            got[0]
        );
        assert!(
            (got[1] + 1.0).abs() < 1e-4,
            "negative over-rail: {}",
            got[1]
        );
        assert!(got[2] > 0.99, "still positive: {}", got[2]);
        assert!(got[3] < -0.99, "still negative: {}", got[3]);
    }

    #[test]
    fn to_i16_is_symmetric_at_the_rails() {
        assert_eq!(to_i16(1.0), 32767);
        assert_eq!(to_i16(-1.0), -32767);
        assert_eq!(to_i16(0.0), 0);
        // Over-rail clamps rather than wrapping.
        assert_eq!(to_i16(9.9), 32767);
        assert_eq!(to_i16(-9.9), -32767);
    }

    #[test]
    fn a_sub_slice_exports_only_the_selection() {
        // "Export the selection" is slicing in the caller: the module writes exactly what it
        // is handed, so a sub-slice must round-trip as its own standalone file.
        let path = tmp("sel.wav");
        let _c = Cleanup(vec![path.clone()]);
        let full: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();

        export_wav(&path, &[&full[20..30]], 16_000, WavFormat::Float32).unwrap();

        let (_spec, got) = read_back(&path);
        assert_eq!(got.len(), 10);
        assert_eq!(got, full[20..30].to_vec());
    }

    #[test]
    fn no_channels_is_an_error_not_an_empty_file() {
        let path = tmp("none.wav");
        let empty: &[&[f32]] = &[];
        assert!(matches!(
            export_wav(&path, empty, 8000, WavFormat::Float32),
            Err(ExportError::NoChannels)
        ));
        assert!(!path.exists(), "a rejected export must not create a file");
    }

    #[test]
    fn ragged_channels_are_rejected_before_writing() {
        let path = tmp("ragged.wav");
        let _c = Cleanup(vec![path.clone()]);
        let left = vec![0.1f32; 8];
        let right = vec![0.1f32; 7];
        let err = export_wav(&path, &[&left, &right], 8000, WavFormat::Float32).unwrap_err();
        assert!(matches!(
            err,
            ExportError::RaggedChannels {
                channel: 1,
                len: 7,
                expected: 8
            }
        ));
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn a_zero_length_range_writes_a_valid_empty_wav() {
        // An empty selection is a caller concern (the shell rejects one); the encoder itself
        // must still produce a well-formed header rather than corrupt output.
        let path = tmp("emptyframes.wav");
        let _c = Cleanup(vec![path.clone()]);
        let mono: Vec<f32> = Vec::new();
        export_wav(&path, &[&mono], 8000, WavFormat::Float32).unwrap();
        let (spec, got) = read_back(&path);
        assert_eq!(spec.channels, 1);
        assert!(got.is_empty());
    }
}
