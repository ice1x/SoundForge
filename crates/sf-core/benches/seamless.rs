//! Task 18 — the seamless-statistics benchmark.
//!
//! The product's differentiator is that dragging a selection across a multi-hour file
//! updates the Statistics panel in ~microseconds, no "compute & wait". This benchmark
//! *proves* that on the real target: a 2-hour, ~1.2 GB single channel (44.1 kHz f32,
//! 317 M samples — the per-channel size of a 2-hour stereo take), driven through the exact
//! path a selection drag takes in the app (`Analyzer::with_pyramid(...).range(s, e)` then
//! `RangeStats::from_agg`, see `Document::stats` in `src-tauri/src/audio.rs`).
//!
//! It checks the three things task 18 asks for and exits non-zero if any fails:
//!   1. **< 5 ms per drag update** — p99 query latency across a simulated drag, at several
//!      selection lengths from 1 000 samples to the whole file.
//!   2. **Independent of selection length** — the seamless property: a whole-file query is
//!      no slower than a tiny one (median latency stays within a small factor across every
//!      selection length). An O(n) regression would blow this up by orders of magnitude.
//!   3. **RAM stable** — the drag loop performs *zero* heap allocations. This is checked
//!      directly with a counting global allocator rather than by sampling RSS: a loop that
//!      never allocates cannot grow the resident set, and the check is exact and portable
//!      instead of noisy and platform-specific.
//!
//! It is `harness = false` (a plain `fn main`) and is intentionally excluded from the
//! `cargo test` CI gate — allocating 1.2 GB is far too heavy for shared CI runners. The
//! cheap, deterministic CI guard for the same invariant is
//! `summary::tests::query_cost_is_independent_of_selection_length`, which measures the work
//! a query performs structurally, with no timing and no giant buffer.
//!
//! Tunables (env): `SF_BENCH_SECS` (default 7200), `SF_BENCH_SR` (default 44100),
//! `SF_BENCH_MOVES` (mouse-moves per drag sweep, default 400). Shrinking `SF_BENCH_SECS`
//! gives a fast smoke run on a memory-constrained machine.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use sf_core::stats::RangeStats;
use sf_core::summary::{Analyzer, Pyramid};

// --- Counting allocator: the exact, portable "RAM stable" proof ---------------------------

/// Wraps the system allocator and counts allocations so the benchmark can assert the hot
/// drag loop allocates nothing (and therefore cannot grow RSS).
struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocs() -> u64 {
    ALLOCS.load(Ordering::Relaxed)
}

// --- Deterministic signal (no external RNG, matching the crate's test convention) ---------

/// xorshift64 — the same tiny PRNG the crate's tests use, so the benchmark needs no deps.
struct Rng(u64);
impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 23) as f32 * 2.0 - 1.0
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Percentile (nearest-rank) over an already-sorted slice of nanosecond latencies.
fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// Sweep a selection of `sel_len` samples across the whole buffer in `moves` steps — one
/// stats query per simulated mouse-move — and return the sorted per-query latencies (ns).
/// The Vec is pre-reserved so the timed queries themselves never allocate.
fn drag_latencies(az: &Analyzer, sr: u32, sel_len: usize, moves: usize) -> Vec<u128> {
    let n = az.len();
    let sel_len = sel_len.min(n);
    let travel = n - sel_len; // how far the selection's start can move
    let mut lat = Vec::with_capacity(moves);
    for m in 0..moves {
        let start = if travel == 0 { 0 } else { travel * m / moves };
        let end = start + sel_len;
        let t = Instant::now();
        let agg = az.range(start, end);
        let st = RangeStats::from_agg(&agg, start as u64, sr);
        black_box(&st);
        lat.push(t.elapsed().as_nanos());
    }
    lat.sort_unstable();
    lat
}

fn fmt_ns(ns: u128) -> String {
    if ns >= 1_000_000 {
        format!("{:.3} ms", ns as f64 / 1e6)
    } else if ns >= 1_000 {
        format!("{:.3} µs", ns as f64 / 1e3)
    } else {
        format!("{ns} ns")
    }
}

fn main() {
    let secs = env_usize("SF_BENCH_SECS", 7200);
    let sr = env_usize("SF_BENCH_SR", 44_100) as u32;
    let moves = env_usize("SF_BENCH_MOVES", 400);
    let n = secs * sr as usize;

    println!("SoundForge seamless-statistics benchmark (task 18)");
    println!(
        "  buffer: {secs}s @ {sr} Hz = {n} samples ({:.2} GB f32), {moves} moves/drag\n",
        (n * std::mem::size_of::<f32>()) as f64 / 1e9
    );

    // Build the signal: a swept-ish sine plus a little noise, so stats are non-trivial.
    let t_fill = Instant::now();
    let mut rng = Rng(0x5F3D_C0DE_1234_ABCD);
    let mut samples = vec![0.0f32; n];
    for (i, s) in samples.iter_mut().enumerate() {
        let phase = (i as f64) * 0.02;
        *s = (phase.sin() as f32) * 0.9 + rng.unit() * 0.05;
    }
    println!("  filled buffer in {:.2?}", t_fill.elapsed());

    // Build the pyramid once — the only O(n) step, exactly as `open_file` does at load time.
    let t_build = Instant::now();
    let pyramid = Pyramid::build(&samples);
    let build = t_build.elapsed();
    println!(
        "  built pyramid in {:.2?}  ({:.1} ns/sample)\n",
        build,
        build.as_nanos() as f64 / n as f64
    );

    let az = Analyzer::with_pyramid(&samples, &pyramid);

    // Warm up caches/branch predictors before timing.
    for _ in 0..64 {
        black_box(az.range(0, n));
    }

    // A drag at each selection length, from a sliver to the whole file.
    let lengths = [1_000usize, 100_000, 10_000_000, 100_000_000, n];
    let threshold_ns = 5_000_000u128; // 5 ms/drag update, as task 18 requires
    let mut medians: Vec<u128> = Vec::new();
    let mut worst_p99 = 0u128;
    let mut failures: Vec<String> = Vec::new();

    println!(
        "  {:<14} {:>12} {:>12} {:>12}",
        "selection", "p50", "p99", "max"
    );
    for &len in &lengths {
        if len > n {
            continue;
        }
        let lat = drag_latencies(&az, sr, len, moves);
        let (p50, p99, max) = (
            percentile(&lat, 50.0),
            percentile(&lat, 99.0),
            lat[lat.len() - 1],
        );
        medians.push(p50);
        worst_p99 = worst_p99.max(p99);
        let label = if len == n {
            "whole file".to_string()
        } else {
            format!("{len}")
        };
        println!(
            "  {:<14} {:>12} {:>12} {:>12}",
            label,
            fmt_ns(p50),
            fmt_ns(p99),
            fmt_ns(max)
        );
        if p99 > threshold_ns {
            failures.push(format!(
                "selection {label}: p99 {} exceeds the 5 ms/drag budget",
                fmt_ns(p99)
            ));
        }
    }

    // Seamless property: median latency must be independent of selection length. Allow a
    // generous 25x spread to absorb timer noise on sub-microsecond queries — an O(n)
    // regression would be ~5 orders of magnitude, not 25x.
    let (min_med, max_med) = (
        medians.iter().copied().min().unwrap_or(0).max(1),
        medians.iter().copied().max().unwrap_or(0),
    );
    let spread = max_med as f64 / min_med as f64;
    println!(
        "\n  length-independence: median spans {} … {} (×{:.1} across {}× the selection length)",
        fmt_ns(min_med),
        fmt_ns(max_med),
        spread,
        n / 1_000
    );
    if spread > 25.0 {
        failures.push(format!(
            "median latency spread ×{spread:.1} across selection lengths — not seamless"
        ));
    }

    // RAM stable: the drag loop allocates nothing, so RSS cannot grow. Measured exactly.
    let before = allocs();
    let mut sink = RangeStats::from_agg(&az.range(0, 0), 0, sr);
    for m in 0..100_000u64 {
        let start = (m as usize * 7919) % (n - 1);
        let end = (start + 1 + (m as usize * 104_729) % (n - start)).min(n);
        let agg = az.range(start, end);
        sink = RangeStats::from_agg(&agg, start as u64, sr);
        black_box(&sink);
    }
    black_box(&sink);
    let leaked = allocs() - before;
    println!("\n  RAM stable: {leaked} heap allocations during 100 000 queries");
    if leaked != 0 {
        failures.push(format!(
            "{leaked} heap allocations during the drag loop — the hot path must not allocate"
        ));
    }

    if failures.is_empty() {
        println!(
            "\n✓ seamless benchmark passed (p99 {} < 5 ms, length-independent, allocation-free)",
            fmt_ns(worst_p99)
        );
    } else {
        eprintln!("\n✗ seamless benchmark FAILED:");
        for f in &failures {
            eprintln!("    - {f}");
        }
        std::process::exit(1);
    }
}
