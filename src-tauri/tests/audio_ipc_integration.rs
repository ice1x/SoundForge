//! Integration test for the `stats` / `waveform` IPC backend (task 11).
//!
//! Exercises the workflow the UI actually performs: open a real encoded file, render the
//! waveform at several zoom levels, and drag a selection — issuing a stats query per
//! mouse-move, as the Statistics panel does — then verify every answer against the signal
//! that was written to the file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use soundforge_lib::audio::{AudioError, AudioState};

/// Unique scratch path in the OS temp dir.
fn tmp(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sf-ipc-{tag}-{}-{n}", std::process::id()))
}

/// Removes paths on drop so the test does not litter the temp dir.
struct Cleanup(Vec<PathBuf>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

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

#[test]
fn open_then_drag_a_selection_and_render_the_waveform() {
    use std::f32::consts::PI;

    let src = tmp("session.wav");
    let cache = tmp("session.pcm");
    let _c = Cleanup(vec![src.clone(), cache.clone()]);

    // 3 seconds of stereo at 44.1 kHz: a 440 Hz tone left, DC-biased silence right. The two
    // channels are unmistakably different, so a channel mix-up cannot pass.
    let sr = 44_100u32;
    let frames = (sr * 3) as usize;
    let left: Vec<f32> = (0..frames)
        .map(|i| (2.0 * PI * 440.0 * i as f32 / sr as f32).sin())
        .collect();
    let right: Vec<f32> = vec![0.25f32; frames];
    write_wav(&src, &[left, right], sr);

    let state = AudioState::default();
    let info = state.open(&src, &cache).unwrap();
    assert_eq!(info.channels, 2);
    assert_eq!(info.sample_rate, sr);
    assert_eq!(info.frames, frames);
    assert!((info.duration_s - 3.0).abs() < 1e-6);

    // The UI draws the whole file first, then zooms in. Every zoom level must produce a
    // full-width envelope with the tone's amplitude on the left channel.
    for &bins in &[1usize, 64, 800, 1920] {
        let wf = state.waveform(0, 0, info.frames, bins).unwrap();
        assert_eq!(wf.min.len(), bins, "bins={bins}");
        assert_eq!(wf.max.len(), bins, "bins={bins}");
        assert!(
            wf.max.iter().cloned().fold(f32::MIN, f32::max) > 0.99,
            "bins={bins}: envelope should reach the tone's peak"
        );
        for (i, (&mn, &mx)) in wf.min.iter().zip(wf.max.iter()).enumerate() {
            assert!(mn <= mx, "bins={bins} bin {i}");
        }
    }

    // The DC channel's envelope is flat at its bias everywhere.
    let wf_r = state.waveform(1, 0, info.frames, 128).unwrap();
    assert!(wf_r.min.iter().all(|&v| (v - 0.25).abs() < 1e-6));
    assert!(wf_r.max.iter().all(|&v| (v - 0.25).abs() < 1e-6));

    // Drag a selection open, one stats query per mouse-move. Each answer must describe the
    // selection at that instant — this is the seamless-statistics behaviour end to end.
    let anchor = 1000usize;
    for step in 1..=60 {
        let end = anchor + step * 2000;
        let st = state.stats(0, anchor, end).unwrap();
        assert_eq!(st.n as usize, end - anchor, "step {step}");
        assert!(
            (st.duration_s - (end - anchor) as f64 / sr as f64).abs() < 1e-9,
            "step {step}"
        );
        assert!((st.start_s - anchor as f64 / sr as f64).abs() < 1e-12);
        // A 440 Hz tone: RMS ~1/sqrt(2) and the zero-crossing estimate lands near 440 Hz
        // once the selection spans enough cycles to be meaningful.
        assert!((st.peak - 1.0).abs() < 1e-2, "step {step} peak {}", st.peak);
        assert!(
            (st.rms - std::f64::consts::FRAC_1_SQRT_2).abs() < 5e-3,
            "step {step} rms {}",
            st.rms
        );
        assert!(
            (st.freq_hz - 440.0).abs() < 5.0,
            "step {step} freq {}",
            st.freq_hz
        );
    }

    // The same drag on the DC channel reports the bias, no crossings, and no frequency.
    let st_r = state.stats(1, anchor, anchor + 60_000).unwrap();
    assert!((st_r.dc - 0.25).abs() < 1e-6, "dc {}", st_r.dc);
    assert!((st_r.rms - 0.25).abs() < 1e-6, "rms {}", st_r.rms);
    assert_eq!(st_r.zero_crossings, 0);
    assert_eq!(st_r.freq_hz, 0.0);

    // Select-all is just another range query.
    let all = state.stats(0, 0, info.frames).unwrap();
    assert_eq!(all.n as usize, frames);
    assert!((all.dc).abs() < 1e-3, "whole-file DC of a tone ~ 0");

    // Closing releases the document and its on-disk cache.
    state.close();
    assert!(state.info().is_none());
    assert!(!cache.exists());
    assert!(matches!(
        state.stats(0, 0, 100),
        Err(AudioError::NoDocument)
    ));
}

#[test]
fn stats_are_stable_across_repeated_queries_of_the_same_selection() {
    // A drag re-queries overlapping ranges constantly; the pyramid is shared mutable-free
    // state, so identical queries must keep returning identical answers.
    let src = tmp("repeat.wav");
    let cache = tmp("repeat.pcm");
    let _c = Cleanup(vec![src.clone(), cache.clone()]);

    let samples: Vec<f32> = (0..30_000)
        .map(|i| ((i as f32) * 0.01).sin() * 0.8)
        .collect();
    write_wav(&src, std::slice::from_ref(&samples), 48_000);

    let state = AudioState::default();
    state.open(&src, &cache).unwrap();

    let first = state.stats(0, 512, 29_000).unwrap();
    for _ in 0..25 {
        assert_eq!(state.stats(0, 512, 29_000).unwrap(), first);
    }
    let wf = state.waveform(0, 512, 29_000, 300).unwrap();
    for _ in 0..5 {
        assert_eq!(state.waveform(0, 512, 29_000, 300).unwrap(), wf);
    }
}
