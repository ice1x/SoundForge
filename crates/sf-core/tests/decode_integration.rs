//! Integration test for task 10: exercise the public `sf-core` API the way the app will —
//! decode a real audio file into the memory-mapped PCM cache, then answer seamless
//! selection statistics per channel, as if the user were dragging a selection.
//!
//! Uses only the crate's public surface (`decode_file`, `PcmCache`, `Analyzer`,
//! `RangeStats`) plus `hound` to synthesize a deterministic WAV fixture.

use std::f32::consts::PI;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use sf_core::{decode_file, RangeStats};

/// Unique scratch path in the OS temp dir (no rand crate needed).
fn tmp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sfcore-it-{tag}-{}-{n}", std::process::id()))
}

/// Remove the given paths when the test ends.
struct Cleanup(Vec<PathBuf>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn write_stereo_wav(path: &PathBuf, left: &[f32], right: &[f32], sample_rate: u32) {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for i in 0..left.len() {
        w.write_sample(left[i]).unwrap();
        w.write_sample(right[i]).unwrap();
    }
    w.finalize().unwrap();
}

/// Decode a stereo file where the two channels carry clearly different content, then
/// verify per-channel seamless statistics over an arbitrary selection sub-range.
#[test]
fn decode_then_seamless_selection_stats_per_channel() {
    let src = tmp("stereo.wav");
    let cache = tmp("stereo.pcm");
    let _c = Cleanup(vec![src.clone(), cache.clone()]);

    let sr = 48_000u32;
    // One second of audio. Left: full-scale 1 kHz sine (whole cycles). Right: constant DC.
    let n = sr as usize;
    let left: Vec<f32> = (0..n)
        .map(|i| (2.0 * PI * 1000.0 * i as f32 / sr as f32).sin())
        .collect();
    let right: Vec<f32> = vec![0.25f32; n];
    write_stereo_wav(&src, &left, &right, sr);

    let pc = decode_file(&src, &cache).expect("decode");
    assert_eq!(pc.channels(), 2);
    assert_eq!(pc.sample_rate(), sr);
    assert_eq!(pc.frames(), n);

    let (az_l, az_r) = (pc.analyzer(0), pc.analyzer(1));

    // Whole-file stats: left is a 1 kHz sine, right is pure DC.
    let full_l = RangeStats::from_agg(&az_l.range(0, n), 0, sr);
    assert!(
        (full_l.rms - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-3,
        "left rms {}",
        full_l.rms
    );
    assert!(
        (full_l.freq_hz - 1000.0).abs() < 2.0,
        "left freq {}",
        full_l.freq_hz
    );
    assert!(full_l.dc.abs() < 1e-3, "left dc {}", full_l.dc);

    let full_r = RangeStats::from_agg(&az_r.range(0, n), 0, sr);
    assert!((full_r.dc - 0.25).abs() < 1e-4, "right dc {}", full_r.dc);
    assert_eq!(full_r.zero_crossings, 0, "DC channel has no crossings");
    assert_eq!(full_r.freq_hz, 0.0);

    // Simulate dragging a selection: several sub-ranges over the left channel. Each answer
    // must match a direct scan of the underlying samples (the "seamless" guarantee).
    let raw_left = pc.channel(0);
    for &(s, e) in &[(0usize, 100usize), (12_345, 24_000), (47_000, n), (1, 2)] {
        let st = RangeStats::from_agg(&az_l.range(s, e), s as u64, sr);
        let (mut mn, mut mx, mut sum) = (f32::INFINITY, f32::NEG_INFINITY, 0.0f64);
        for &v in &raw_left[s..e] {
            mn = mn.min(v);
            mx = mx.max(v);
            sum += v as f64;
        }
        assert_eq!(st.n, (e - s) as u64, "n for [{s},{e})");
        assert_eq!(st.min, mn, "min for [{s},{e})");
        assert_eq!(st.max, mx, "max for [{s},{e})");
        assert!(
            (st.dc - sum / (e - s) as f64).abs() < 1e-6,
            "dc for [{s},{e})"
        );
        // Peak is max |sample| and start time reflects the selection origin.
        assert!((st.peak - mn.abs().max(mx.abs())).abs() < 1e-9);
        assert!((st.start_s - s as f64 / sr as f64).abs() < 1e-12);
    }
}
