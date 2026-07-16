//! The on-disk PCM cache file convention: naming, allocation, and reaping orphans.
//!
//! Naming, allocation and reaping live together in one module on purpose — the sweep has to
//! *parse* exactly what the allocator *formats*, and splitting those across modules is how
//! the two silently drift apart.
//!
//! ## Why reaping is needed
//!
//! A `Document` (see [`crate::audio`]) deletes its cache file when it is dropped, which
//! covers every normal exit. It does not cover an abnormal one: on SIGKILL, a force-quit, or
//! a panic with `panic=abort`, `Drop` never runs and the cache file is left behind. These
//! files are big — a 2-hour source is roughly a 1.2 GB planar `f32` cache — so orphans
//! accumulating across crashes is a real disk-space leak, and nothing else would ever
//! remove them.
//!
//! ## How an orphan is identified
//!
//! Each cache file carries the pid of the process that created it, so a later run can ask
//! whether that process is still alive. The sweep only deletes a file it can *prove* is
//! stale; anything uncertain is kept, because deleting a live instance's cache is far worse
//! than leaving a dead one on disk. Concretely, a second running instance's caches are
//! skipped (its pid is alive), and a recycled pid is only ever misread in the safe
//! direction — as "still alive", so the orphan is kept rather than a live file deleted.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Prefix of every PCM cache file name.
const PREFIX: &str = "pcm-";
/// Extension of every PCM cache file name.
const EXT: &str = "cache";

/// Format the cache file name for `pid` and `counter`: `pcm-<pid>-<counter>.cache`.
///
/// The single definition of the convention; [`parse_pid`] is its exact inverse.
fn file_name(pid: u32, counter: u64) -> String {
    format!("{PREFIX}{pid}-{counter}.{EXT}")
}

/// The pid embedded in a cache file name, or `None` if `name` is not one of ours.
///
/// Deliberately strict — both the pid and the counter must parse as numbers. The sweep
/// deletes what this recognises, so anything it is not certain about must not match.
fn parse_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(PREFIX)?;
    let rest = rest.strip_suffix(EXT)?.strip_suffix('.')?;
    let (pid, counter) = rest.split_once('-')?;
    // The counter must parse too, so a foreign `pcm-123-something.cache` is left alone.
    counter.parse::<u64>().ok()?;
    pid.parse::<u32>().ok()
}

/// Allocate a fresh, unique PCM cache path in `dir`.
///
/// Unique per call by design: replacing a document deletes the previous document's cache
/// file, which would clobber the new one if the paths collided. The pid separates concurrent
/// app instances; the counter separates successive opens within one instance.
pub fn next_path(dir: &Path) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    dir.join(file_name(std::process::id(), n))
}

/// What a [`sweep_at_startup`] pass did, for logging.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    /// Orphaned caches deleted.
    pub removed: usize,
    /// Total bytes freed by those deletions.
    pub bytes_freed: u64,
    /// Caches left alone because their owning process is still alive.
    pub kept: usize,
    /// Caches that looked stale but could not be deleted.
    pub failed: usize,
}

/// Whether `pid` names a process that currently exists.
///
/// On Unix this is `kill(pid, 0)`, which sends no signal and only performs the error
/// checking. Errors are read in the safe direction: `EPERM` means the process exists but
/// belongs to another user, so it counts as alive; only `ESRCH` ("no such process") is
/// treated as dead.
#[cfg(unix)]
pub fn pid_is_live(pid: u32) -> bool {
    // `kill` treats 0 and negative pids as "signal a whole process group", which is not a
    // question we are asking. A pid that cannot be a real process id counts as alive so the
    // sweep leaves the file alone.
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    if pid <= 0 {
        return true;
    }
    // SAFETY: `kill` with signal 0 performs error checking only and sends no signal; `pid`
    // is a positive `pid_t`, so this cannot address a process group.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Whether `pid` names a process that currently exists.
///
/// Non-Unix targets have no liveness probe wired up yet (the project is Apple-Silicon
/// first), so every pid counts as alive and the sweep becomes a no-op — orphans are kept
/// rather than risking a live instance's cache.
#[cfg(not(unix))]
pub fn pid_is_live(_pid: u32) -> bool {
    true
}

/// Delete orphaned PCM caches in `dir` left behind by processes that are gone.
///
/// `is_live` is injected so the decision can be tested without spawning processes; production
/// callers pass [`pid_is_live`].
///
/// # Preconditions
/// Must run **before the first document is opened** — it deletes caches bearing the current
/// pid, which is only sound while this process has not yet written any. That rule is what
/// reclaims a cache orphaned by an earlier crashed instance whose pid the OS later recycled
/// onto us; without it such a file would look "alive" forever and never be reaped.
///
/// A missing `dir` is not an error (nothing has ever been cached). Individual failures are
/// counted and logged rather than propagated: a startup sweep must never stop the app from
/// launching.
pub fn sweep_at_startup(dir: &Path, current_pid: u32, is_live: impl Fn(u32) -> bool) -> Swept {
    let mut swept = Swept::default();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // Nothing cached yet, or the directory is unreadable — either way there is nothing
        // to reap and nothing worth failing startup over.
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                log::debug!("cache sweep skipped {}: {e}", dir.display());
            }
            return swept;
        }
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(pid) = parse_pid(name) else { continue };

        // Only ever unlink a regular file. A directory or symlink wearing a cache-shaped name
        // is not something we wrote, and following a symlink here would delete its target.
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }

        // Our own pid predates us (see the precondition): reap it. Otherwise only reap what
        // is provably dead.
        if pid != current_pid && is_live(pid) {
            swept.kept += 1;
            continue;
        }

        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        match std::fs::remove_file(entry.path()) {
            Ok(()) => {
                swept.removed += 1;
                swept.bytes_freed += size;
                log::debug!("reaped orphaned PCM cache {name} ({size} bytes, pid {pid})");
            }
            // Another instance sweeping concurrently got there first. Not a failure.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                swept.failed += 1;
                log::warn!("could not reap orphaned PCM cache {name}: {e}");
            }
        }
    }
    swept
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    /// A unique scratch *directory* for a sweep test.
    fn tmp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("sf-cache-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Removes a directory tree on drop.
    struct CleanupDir(PathBuf);
    impl Drop for CleanupDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Write a cache file of `size` bytes for `pid`.
    fn write_cache(dir: &Path, pid: u32, counter: u64, size: usize) -> PathBuf {
        let p = dir.join(file_name(pid, counter));
        std::fs::write(&p, vec![0u8; size]).unwrap();
        p
    }

    #[test]
    fn file_name_round_trips_through_parse() {
        for &(pid, n) in &[(1u32, 0u64), (12_345, 7), (u32::MAX, u64::MAX)] {
            let name = file_name(pid, n);
            assert_eq!(parse_pid(&name), Some(pid), "name {name}");
        }
    }

    #[test]
    fn parse_pid_rejects_foreign_names() {
        // The sweep deletes whatever this matches, so it must not match anything else.
        for name in [
            "unrelated.txt",
            "important-user-data.cache",
            "pcm.cache",
            "pcm-.cache",
            "pcm-123.cache",       // no counter
            "pcm-abc-1.cache",     // pid not numeric
            "pcm-123-abc.cache",   // counter not numeric
            "pcm-123-1.txt",       // wrong extension
            "pcm-123-1.cache.bak", // wrong extension
            "notpcm-123-1.cache",
            "pcm--1.cache", // empty pid
        ] {
            assert_eq!(parse_pid(name), None, "should not match: {name}");
        }
    }

    #[test]
    fn parse_pid_accepts_a_negative_looking_pid_as_no_match() {
        // A pid is unsigned in our names; `pcm--5-0.cache` must not parse to something we
        // would then hand to kill().
        assert_eq!(parse_pid("pcm--5-0.cache"), None);
    }

    #[test]
    fn next_path_is_unique_per_call_and_carries_our_pid() {
        let dir = tmp_dir("next");
        let _c = CleanupDir(dir.clone());
        let a = next_path(&dir);
        let b = next_path(&dir);
        assert_ne!(a, b, "each open must get its own cache path");
        for p in [&a, &b] {
            let name = p.file_name().unwrap().to_str().unwrap();
            assert_eq!(parse_pid(name), Some(std::process::id()));
        }
    }

    #[test]
    fn sweep_removes_caches_of_dead_processes() {
        let dir = tmp_dir("dead");
        let _c = CleanupDir(dir.clone());
        let dead = write_cache(&dir, 4242, 0, 1024);
        let also_dead = write_cache(&dir, 4243, 1, 512);

        let swept = sweep_at_startup(&dir, 999, |_| false);
        assert_eq!(swept.removed, 2);
        assert_eq!(swept.kept, 0);
        assert_eq!(swept.failed, 0);
        assert_eq!(swept.bytes_freed, 1024 + 512);
        assert!(!dead.exists());
        assert!(!also_dead.exists());
    }

    #[test]
    fn sweep_keeps_caches_of_a_live_second_instance() {
        // The dangerous mistake: deleting a concurrently running instance's cache.
        let dir = tmp_dir("live");
        let _c = CleanupDir(dir.clone());
        let other = write_cache(&dir, 4242, 0, 64);
        let dead = write_cache(&dir, 4243, 0, 64);

        let swept = sweep_at_startup(&dir, 999, |pid| pid == 4242);
        assert_eq!(swept.kept, 1);
        assert_eq!(swept.removed, 1);
        assert!(other.exists(), "a live instance's cache must survive");
        assert!(!dead.exists());
    }

    #[test]
    fn sweep_removes_our_own_pid_caches_even_though_we_are_live() {
        // A crashed earlier instance whose pid the OS recycled onto us. `is_live` says yes
        // (it is us), so only the own-pid rule can reclaim this; otherwise it leaks forever.
        let dir = tmp_dir("recycled");
        let _c = CleanupDir(dir.clone());
        let ours = write_cache(&dir, 777, 0, 2048);

        let swept = sweep_at_startup(&dir, 777, |_| true);
        assert_eq!(swept.removed, 1);
        assert_eq!(swept.kept, 0);
        assert_eq!(swept.bytes_freed, 2048);
        assert!(!ours.exists());
    }

    #[test]
    fn sweep_ignores_files_that_are_not_ours() {
        let dir = tmp_dir("foreign");
        let _c = CleanupDir(dir.clone());
        let keep = [
            dir.join("unrelated.txt"),
            dir.join("important-user-data.cache"),
            dir.join("pcm-abc-1.cache"),
            dir.join("notpcm-1-1.cache"),
        ];
        for p in &keep {
            std::fs::write(p, b"precious").unwrap();
        }
        // A subdirectory that happens to match the pattern must not trip it up either.
        let dir_shaped_like_a_cache = dir.join(file_name(4242, 9));
        std::fs::create_dir(&dir_shaped_like_a_cache).unwrap();

        let swept = sweep_at_startup(&dir, 999, |_| false);
        assert_eq!(swept.removed, 0, "no foreign file may be deleted");
        assert_eq!(swept.failed, 0, "a non-file is skipped, not a failure");
        for p in &keep {
            assert!(p.exists(), "{} was deleted", p.display());
        }
        assert!(dir_shaped_like_a_cache.exists(), "directory was removed");
    }

    #[cfg(unix)]
    #[test]
    fn sweep_never_follows_a_cache_shaped_symlink() {
        // A symlink wearing a cache name must not get its target deleted.
        let dir = tmp_dir("symlink");
        let _c = CleanupDir(dir.clone());
        let precious = dir.join("precious.wav");
        std::fs::write(&precious, b"user data").unwrap();
        let link = dir.join(file_name(4242, 0));
        std::os::unix::fs::symlink(&precious, &link).unwrap();

        let swept = sweep_at_startup(&dir, 999, |_| false);
        assert_eq!(swept.removed, 0);
        assert!(precious.exists(), "symlink target must survive");
        assert!(link.exists(), "the symlink itself is not ours to remove");
    }

    #[test]
    fn sweep_of_a_missing_directory_is_not_an_error() {
        let dir = std::env::temp_dir().join("sf-cache-does-not-exist-xyz");
        assert_eq!(sweep_at_startup(&dir, 1, |_| false), Swept::default());
    }

    #[test]
    fn sweep_of_an_empty_directory_does_nothing() {
        let dir = tmp_dir("empty");
        let _c = CleanupDir(dir.clone());
        assert_eq!(sweep_at_startup(&dir, 1, |_| false), Swept::default());
    }

    #[test]
    fn pid_is_live_is_true_for_this_process() {
        assert!(pid_is_live(std::process::id()));
    }

    #[test]
    fn pid_is_live_rejects_unusable_pids_conservatively() {
        // 0 and out-of-range values must never be handed to kill() as process groups; they
        // report "live" so the sweep keeps the file.
        assert!(pid_is_live(0));
        assert!(pid_is_live(u32::MAX));
    }

    #[cfg(unix)]
    #[test]
    fn pid_is_live_follows_a_real_child_process() {
        // The real probe against the OS: alive while running, dead once reaped.
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(pid_is_live(pid), "child should be alive while running");

        child.kill().expect("kill child");
        // Must reap it: a killed-but-unwaited child is a zombie, and kill(pid, 0) still
        // succeeds for a zombie, so without this the pid would still look alive.
        child.wait().expect("reap child");
        assert!(!pid_is_live(pid), "reaped child's pid should be dead");
    }

    #[cfg(unix)]
    #[test]
    fn sweep_reaps_a_real_dead_process_cache_end_to_end() {
        // Same as above, but through the sweep with the production liveness probe.
        let dir = tmp_dir("realpid");
        let _c = CleanupDir(dir.clone());

        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let dead_pid = child.id();
        child.kill().unwrap();
        child.wait().unwrap();

        let orphan = write_cache(&dir, dead_pid, 0, 128);
        let live = write_cache(&dir, std::process::id(), 0, 128);
        // Sweep as if we were some other process, so the own-pid rule is not what removes it.
        let swept = sweep_at_startup(&dir, 424_242, pid_is_live);

        assert!(!orphan.exists(), "dead process's cache should be reaped");
        assert!(live.exists(), "this live process's cache should be kept");
        assert_eq!(swept.removed, 1);
        assert_eq!(swept.kept, 1);
    }
}
