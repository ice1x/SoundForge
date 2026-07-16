//! Integration test for reaping orphaned PCM caches.
//!
//! Drives the real startup sequence the shell performs: a previous instance died without
//! running `Drop` and left caches behind, then this instance starts, sweeps, and opens a
//! file. Covers the seam between `cache` (naming/reaping) and `audio` (which writes and
//! deletes the files), which unit tests of either module alone cannot.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use soundforge_lib::audio::AudioState;
use soundforge_lib::cache;

fn tmp_dir(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let d = std::env::temp_dir().join(format!("sf-reap-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct CleanupDir(PathBuf);
impl Drop for CleanupDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write_wav(path: &Path, samples: &[f32], sample_rate: u32) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(path, spec).unwrap();
    for &s in samples {
        w.write_sample(s).unwrap();
    }
    w.finalize().unwrap();
}

/// A pid that is guaranteed dead: spawn a child, kill it, and reap it so the pid is free.
/// (A killed-but-unwaited child is a zombie, whose pid still answers `kill(pid, 0)`.)
#[cfg(unix)]
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = child.id();
    child.kill().expect("kill");
    child.wait().expect("reap");
    pid
}

#[cfg(unix)]
#[test]
fn startup_reaps_a_crashed_instances_caches_then_opens_normally() {
    let dir = tmp_dir("startup");
    let _c = CleanupDir(dir.clone());

    // A previous instance was SIGKILLed mid-session: `Document::drop` never ran, so its PCM
    // caches are still on disk. Reproduce that state faithfully by decoding real files into
    // that directory and then leaking the documents, exactly as a killed process would.
    let crashed_pid = dead_pid();
    let src = dir.join("prev.wav");
    write_wav(&src, &vec![0.3f32; 4096], 8000);

    let orphans: Vec<PathBuf> = (0..2)
        .map(|i| {
            let p = dir.join(format!("pcm-{crashed_pid}-{i}.cache"));
            let state = AudioState::default();
            state.open(&src, &p).unwrap();
            // Leak the document: no Drop, so the cache file survives — a real orphan.
            std::mem::forget(state);
            assert!(p.exists(), "orphan should be on disk");
            p
        })
        .collect();
    let orphan_bytes: u64 = orphans.iter().map(|p| p.metadata().unwrap().len()).sum();
    assert!(orphan_bytes > 0);

    // A concurrently running second instance's cache must survive the sweep.
    let live_other = dir.join(format!("pcm-{}-0.cache", std::process::id()));
    std::fs::write(&live_other, b"other instance").unwrap();

    // --- this instance starts up ---
    let swept = cache::sweep_at_startup(&dir, 424_242, cache::pid_is_live);

    assert_eq!(swept.removed, 2, "both orphans should be reaped");
    assert_eq!(swept.failed, 0);
    assert_eq!(swept.bytes_freed, orphan_bytes);
    for p in &orphans {
        assert!(!p.exists(), "{} should have been reaped", p.display());
    }
    assert!(
        live_other.exists(),
        "a live instance's cache must not be touched"
    );
    assert!(src.exists(), "the user's source file must not be touched");

    // --- and then opens a file normally, after the sweep ---
    let state = AudioState::default();
    let path = cache::next_path(&dir);
    let info = state.open(&src, &path).unwrap();
    assert_eq!(info.frames, 4096);
    assert!(path.exists());
    // The new document still works, and closing it cleans up after itself as before.
    assert_eq!(state.stats(0, 0, 4096).unwrap().n, 4096);
    state.close();
    assert!(!path.exists());
}

#[test]
fn a_cache_written_by_open_is_recognised_by_the_sweep() {
    // The naming/parsing contract across the two modules: whatever `next_path` hands to
    // `AudioState::open` must be something the sweep can identify as reapable. If these ever
    // drift apart, orphans would silently never be collected.
    let dir = tmp_dir("contract");
    let _c = CleanupDir(dir.clone());

    let src = dir.join("s.wav");
    write_wav(&src, &vec![0.1f32; 1024], 8000);

    let state = AudioState::default();
    let path = cache::next_path(&dir);
    state.open(&src, &path).unwrap();
    assert!(path.exists());

    // Sweeping as a different process, with everything dead, must recognise and reap it.
    let swept = cache::sweep_at_startup(&dir, 424_242, |_| false);
    assert_eq!(
        swept.removed, 1,
        "a cache written via next_path must be reapable by the sweep"
    );
    assert!(!path.exists());

    // Dropping the state now hits a file that is already gone; that must not panic.
    drop(state);
}
