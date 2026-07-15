//! Summary pyramid: a multi-level tree of precomputed [`Agg`] blocks over the sample
//! buffer. This is the structure behind SoundForge's "seamless" statistics — the thing
//! competitors make you wait for.
//!
//! A range query `[start, end)` is answered by (1) scanning the raw samples of the
//! (short) unaligned head and tail, and (2) covering the leaf-aligned middle with
//! O(log N) precomputed pyramid blocks, then combining everything with the associative
//! [`Agg::combine`]. Cost is independent of the selection length, so dragging a
//! selection across a multi-hour file updates the stats panel in microseconds.
//!
//! Memory overhead of the pyramid is ~`1/(FANOUT-1)` of the leaf level, i.e. a few
//! percent of the sample buffer — it does not defeat the memory-mapping strategy.

use crate::agg::Agg;

/// Number of samples per leaf block. Also the max length of an unaligned head/tail scan.
pub const LEAF: usize = 1024;
/// Fan-out per pyramid level (each parent block covers `FANOUT` child blocks).
pub const FANOUT: usize = 8;

/// The precomputed block tree behind an [`Analyzer`], owned independently of it.
///
/// Building the pyramid is the only O(n) step in analysis; every query afterwards is
/// O(log N). Splitting it out lets a long-lived caller — e.g. the desktop shell holding a
/// memory-mapped file open — build one pyramid per channel at load time and then create a
/// borrowing [`Analyzer`] per query for free, via [`Analyzer::with_pyramid`]. Rebuilding it
/// per query instead would make every selection drag O(n) and defeat the whole design.
///
/// [`Analyzer::new`] builds and owns one internally, which is what short-lived, one-shot
/// analyses want.
pub struct Pyramid {
    /// `levels[0]` = one `Agg` per leaf block (`LEAF` samples each).
    /// `levels[k][j]` covers `FANOUT^k` leaf blocks starting at leaf `j*FANOUT^k`.
    levels: Vec<Vec<Agg>>,
    /// Number of samples this pyramid was built over. Kept so [`Analyzer::with_pyramid`]
    /// can reject a pyramid that does not match its buffer.
    n_samples: usize,
}

impl Pyramid {
    /// Build the summary pyramid over `samples`. O(n) time, O(n / (FANOUT-1)) extra space.
    pub fn build(samples: &[f32]) -> Self {
        let mut levels: Vec<Vec<Agg>> = Vec::new();

        // Leaf level: fold each LEAF-sized chunk.
        let n_leaves = samples.len().div_ceil(LEAF).max(1);
        let mut leaf = Vec::with_capacity(n_leaves);
        let mut pos = 0usize;
        while pos < samples.len() {
            let end = (pos + LEAF).min(samples.len());
            leaf.push(Agg::from_samples(&samples[pos..end], pos as u64));
            pos = end;
        }
        if leaf.is_empty() {
            leaf.push(Agg::identity());
        }
        levels.push(leaf);

        // Higher levels: combine FANOUT children until a level has a single block.
        while levels.last().unwrap().len() > 1 {
            let child = levels.last().unwrap();
            let mut parent = Vec::with_capacity(child.len().div_ceil(FANOUT));
            let mut i = 0;
            while i < child.len() {
                let end = (i + FANOUT).min(child.len());
                let mut acc = child[i];
                for c in &child[i + 1..end] {
                    acc = acc.combine(c);
                }
                parent.push(acc);
                i = end;
            }
            levels.push(parent);
        }

        Pyramid {
            levels,
            n_samples: samples.len(),
        }
    }

    /// Number of samples this pyramid was built over.
    pub fn len(&self) -> usize {
        self.n_samples
    }

    /// True if this pyramid was built over an empty buffer.
    pub fn is_empty(&self) -> bool {
        self.n_samples == 0
    }
}

/// A pyramid an [`Analyzer`] either owns or borrows from a longer-lived cache.
enum PyramidRef<'a> {
    Owned(Pyramid),
    Borrowed(&'a Pyramid),
}

impl PyramidRef<'_> {
    #[inline]
    fn levels(&self) -> &[Vec<Agg>] {
        match self {
            PyramidRef::Owned(p) => &p.levels,
            PyramidRef::Borrowed(p) => &p.levels,
        }
    }
}

/// An analyzer over a borrowed sample buffer with a precomputed summary pyramid.
///
/// Samples are borrowed, not owned, so the buffer may itself be a memory-mapped file.
pub struct Analyzer<'a> {
    samples: &'a [f32],
    pyramid: PyramidRef<'a>,
}

impl<'a> Analyzer<'a> {
    /// Build the summary pyramid over `samples` and own it. O(n) time.
    ///
    /// Use [`Self::with_pyramid`] instead when the same buffer is queried repeatedly, so the
    /// O(n) build happens once rather than per query.
    pub fn new(samples: &'a [f32]) -> Self {
        Analyzer {
            samples,
            pyramid: PyramidRef::Owned(Pyramid::build(samples)),
        }
    }

    /// Analyze `samples` using a pyramid already built over *those same samples*.
    /// O(1) — no pyramid work at all, so this is cheap enough to call per query.
    ///
    /// # Panics
    /// Panics if `pyramid` was not built over a buffer of the same length as `samples`.
    /// Checking here is what turns a stale pyramid — e.g. one kept across an edit that
    /// resized the buffer — into an immediate, named error instead of an opaque
    /// out-of-bounds index deep inside a range query. A pyramid of the right length but
    /// stale *contents* cannot be detected and will silently return wrong statistics, so
    /// rebuild it whenever the samples change.
    pub fn with_pyramid(samples: &'a [f32], pyramid: &'a Pyramid) -> Self {
        assert_eq!(
            pyramid.n_samples,
            samples.len(),
            "pyramid was built over {} samples but the buffer has {} — rebuild it after \
             the buffer is resized",
            pyramid.n_samples,
            samples.len()
        );
        Analyzer {
            samples,
            pyramid: PyramidRef::Borrowed(pyramid),
        }
    }

    /// Total number of samples.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True if there are no samples.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Aggregate over the half-open sample range `[start, end)`.
    ///
    /// `start`/`end` are clamped to the valid range; `start >= end` yields the identity.
    pub fn range(&self, start: usize, end: usize) -> Agg {
        let start = start.min(self.samples.len());
        let end = end.min(self.samples.len());
        if start >= end {
            return Agg::identity();
        }

        // Leaf-aligned middle boundaries.
        let hs = start.div_ceil(LEAF) * LEAF; // first leaf boundary >= start
        let te = (end / LEAF) * LEAF; // last leaf boundary <= end

        // No full leaf inside the range: just scan it directly (at most 2*LEAF samples).
        if hs >= te {
            return Agg::from_samples(&self.samples[start..end], start as u64);
        }

        // Head + aligned middle + tail.
        let mut acc = Agg::from_samples(&self.samples[start..hs], start as u64);
        acc = self.combine_aligned(acc, hs / LEAF, te / LEAF);
        acc.combine(&Agg::from_samples(&self.samples[te..end], te as u64))
    }

    /// Min/max amplitude per horizontal pixel for the range `[start, end)`, split into
    /// `bins` buckets. This is what the waveform view draws — and, like [`Self::range`],
    /// it reads from the pyramid, so redrawing while zooming a multi-hour file stays cheap
    /// (each bin costs O(log N), independent of how many samples it spans).
    ///
    /// Returns `bins` pairs `(min, max)`; empty buckets report `(0.0, 0.0)`.
    pub fn waveform(&self, start: usize, end: usize, bins: usize) -> Vec<(f32, f32)> {
        let start = start.min(self.samples.len());
        let end = end.min(self.samples.len());
        if bins == 0 || start >= end {
            return vec![(0.0, 0.0); bins];
        }
        let span = end - start;
        let mut out = Vec::with_capacity(bins);
        for b in 0..bins {
            let bs = start + span * b / bins;
            let be = start + span * (b + 1) / bins;
            if bs >= be {
                out.push((0.0, 0.0));
                continue;
            }
            let a = self.range(bs, be);
            out.push((a.min, a.max));
        }
        out
    }

    /// Cover the leaf range `[start_leaf, end_leaf)` (both leaf-aligned) with the coarsest
    /// pyramid blocks that fit, greedily. Every block used is composed entirely of full
    /// leaves, so its precomputed `Agg` is exact for the samples it represents.
    fn combine_aligned(&self, mut acc: Agg, start_leaf: usize, end_leaf: usize) -> Agg {
        let levels = self.pyramid.levels();
        let mut i = start_leaf;
        while i < end_leaf {
            // Grow the level as long as the block stays aligned and fits within end_leaf.
            let mut level = 0usize;
            loop {
                let next = level + 1;
                let span_next = FANOUT.pow(next as u32);
                if next < levels.len()
                    && i.is_multiple_of(span_next)
                    && i + span_next <= end_leaf
                    && (i / span_next) < levels[next].len()
                {
                    level = next;
                } else {
                    break;
                }
            }
            let span = FANOUT.pow(level as u32);
            acc = acc.combine(&levels[level][i / span]);
            i += span;
        }
        acc
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny deterministic PRNG (xorshift64) so tests need no external crates and are reproducible.
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
        /// Float in [-1, 1).
        fn next_f32(&mut self) -> f32 {
            (self.next_u64() >> 40) as f32 / (1u64 << 23) as f32 * 2.0 - 1.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
    }

    /// Naive O(N) reference over a slice — the ground truth the pyramid must match.
    fn naive(samples: &[f32], start: usize, end: usize) -> Agg {
        Agg::from_samples(&samples[start..end], start as u64)
    }

    /// Assert two aggregates are equal, allowing a tiny tolerance on the fp sums (the
    /// pyramid folds in a different order than a left-to-right scan, so `sum`/`sum_sq`
    /// differ in the last ULPs — everything else must match exactly).
    fn assert_agg_eq(got: &Agg, want: &Agg) {
        assert_eq!(got.n, want.n, "n");
        assert_eq!(got.min, want.min, "min");
        assert_eq!(got.min_pos, want.min_pos, "min_pos");
        assert_eq!(got.max, want.max, "max");
        assert_eq!(got.max_pos, want.max_pos, "max_pos");
        assert_eq!(got.zero_crossings, want.zero_crossings, "zero_crossings");
        assert_eq!(got.first_nz_sign, want.first_nz_sign, "first_nz_sign");
        assert_eq!(got.last_nz_sign, want.last_nz_sign, "last_nz_sign");
        let tol = 1e-6 * want.sum_sq.abs().max(1.0);
        assert!(
            (got.sum - want.sum).abs() < tol,
            "sum {} vs {}",
            got.sum,
            want.sum
        );
        assert!(
            (got.sum_sq - want.sum_sq).abs() < tol,
            "sum_sq {} vs {}",
            got.sum_sq,
            want.sum_sq
        );
    }

    #[test]
    fn pyramid_matches_naive_on_random_ranges() {
        let mut rng = Rng(0x9E3779B97F4A7C15);
        // A few odd sizes to exercise partial leaves and multi-level coverage.
        for &len in &[1usize, LEAF - 1, LEAF, LEAF + 1, 5000, 40_000, 123_457] {
            let samples: Vec<f32> = (0..len)
                .map(|_| {
                    let v = rng.next_f32();
                    // Sprinkle exact zeros to exercise the nonzero-sign zero-crossing logic.
                    if rng.below(11) == 0 {
                        0.0
                    } else {
                        v
                    }
                })
                .collect();
            let az = Analyzer::new(&samples);

            for _ in 0..200 {
                let a = rng.below(len);
                let b = rng.below(len);
                let (s, e) = (a.min(b), a.max(b) + 1);
                let got = az.range(s, e);
                let want = naive(&samples, s, e);
                assert_eq!(got.n, want.n, "n len={len} [{s},{e})");
                assert_eq!(got.min, want.min, "min len={len} [{s},{e})");
                assert_eq!(got.min_pos, want.min_pos, "min_pos len={len} [{s},{e})");
                assert_eq!(got.max, want.max, "max len={len} [{s},{e})");
                assert_eq!(got.max_pos, want.max_pos, "max_pos len={len} [{s},{e})");
                assert_eq!(
                    got.zero_crossings, want.zero_crossings,
                    "zc len={len} [{s},{e})"
                );
                // Sums are order-dependent in fp; the pyramid folds in a different order
                // than the naive left-to-right scan, so allow a tiny tolerance.
                assert!(
                    (got.sum - want.sum).abs() < 1e-3,
                    "sum len={len} [{s},{e}) got={} want={}",
                    got.sum,
                    want.sum
                );
                assert!(
                    (got.sum_sq - want.sum_sq).abs() < 1e-3,
                    "sum_sq len={len} [{s},{e})"
                );
            }
        }
    }

    #[test]
    fn full_range_and_edges() {
        let samples: Vec<f32> = (0..10_000).map(|i| ((i as f32) * 0.01).sin()).collect();
        let az = Analyzer::new(&samples);
        assert_agg_eq(
            &az.range(0, samples.len()),
            &naive(&samples, 0, samples.len()),
        );
        assert_agg_eq(&az.range(0, 1), &naive(&samples, 0, 1));
        assert_agg_eq(&az.range(9999, 10_000), &naive(&samples, 9999, 10_000));
        assert_eq!(az.range(500, 500), Agg::identity()); // empty
        assert_eq!(az.range(5000, 4000), Agg::identity()); // reversed -> empty
    }

    #[test]
    fn waveform_bins_match_naive_minmax() {
        let mut rng = Rng(0xDEADBEEFCAFEF00D);
        let samples: Vec<f32> = (0..50_000).map(|_| rng.next_f32()).collect();
        let az = Analyzer::new(&samples);
        let (start, end, bins) = (137usize, 48_913usize, 800usize);
        let got = az.waveform(start, end, bins);
        assert_eq!(got.len(), bins);
        let span = end - start;
        for (b, &(gmin, gmax)) in got.iter().enumerate() {
            let bs = start + span * b / bins;
            let be = start + span * (b + 1) / bins;
            let (mut wmin, mut wmax) = (f32::INFINITY, f32::NEG_INFINITY);
            for &v in &samples[bs..be] {
                wmin = wmin.min(v);
                wmax = wmax.max(v);
            }
            assert_eq!(gmin, wmin, "bin {b} min");
            assert_eq!(gmax, wmax, "bin {b} max");
        }
    }

    #[test]
    fn waveform_handles_more_bins_than_samples() {
        let samples = vec![0.1f32, -0.2, 0.3];
        let az = Analyzer::new(&samples);
        let got = az.waveform(0, 3, 10);
        assert_eq!(got.len(), 10);
        // Each of the 3 samples lands in its own bin; the rest are empty (0,0).
        assert!(
            got.iter()
                .filter(|&&(mn, mx)| mn != 0.0 || mx != 0.0)
                .count()
                <= 3
        );
    }

    #[test]
    fn with_pyramid_matches_owned_analyzer() {
        let mut rng = Rng(0x5EED_1234_ABCD_9876);
        // Sizes spanning partial leaves, exact leaves, and several pyramid levels.
        for &len in &[1usize, LEAF - 1, LEAF, LEAF + 1, 9_999, 60_000] {
            let samples: Vec<f32> = (0..len).map(|_| rng.next_f32()).collect();
            let owned = Analyzer::new(&samples);
            let pyramid = Pyramid::build(&samples);
            let borrowed = Analyzer::with_pyramid(&samples, &pyramid);

            for _ in 0..50 {
                let a = rng.below(len);
                let b = rng.below(len);
                let (s, e) = (a.min(b), a.max(b) + 1);
                assert_agg_eq(&borrowed.range(s, e), &owned.range(s, e));
            }
            assert_eq!(borrowed.waveform(0, len, 64), owned.waveform(0, len, 64));
            assert_eq!(borrowed.len(), owned.len());
        }
    }

    #[test]
    fn mismatched_pyramid_is_rejected_at_construction() {
        // A stale pyramid (e.g. kept across an edit that resized the buffer) used to index
        // out of bounds deep inside `range`; it must fail loudly and immediately instead.
        let short = vec![0.5f32; 100];
        let long = vec![0.5f32; 10_000];
        let pyramid = Pyramid::build(&short);
        assert_eq!(pyramid.len(), 100);

        let err = std::panic::catch_unwind(|| Analyzer::with_pyramid(&long, &pyramid))
            .err()
            .expect("a pyramid of the wrong length must be rejected");
        let msg = err
            .downcast_ref::<String>()
            .expect("panic payload should be a string");
        assert!(
            msg.contains("100") && msg.contains("10000"),
            "message: {msg}"
        );

        // The matching buffer is of course still accepted.
        assert_eq!(Analyzer::with_pyramid(&short, &pyramid).len(), 100);
    }

    #[test]
    fn one_pyramid_serves_many_analyzers() {
        // The point of the split: build once, query through many short-lived analyzers.
        let samples: Vec<f32> = (0..20_000).map(|i| ((i as f32) * 0.003).sin()).collect();
        let pyramid = Pyramid::build(&samples);
        let want = naive(&samples, 100, 19_000);
        for _ in 0..5 {
            let az = Analyzer::with_pyramid(&samples, &pyramid);
            assert_agg_eq(&az.range(100, 19_000), &want);
        }
    }

    #[test]
    fn clamps_out_of_bounds() {
        let samples = vec![0.1f32, 0.2, 0.3];
        let az = Analyzer::new(&samples);
        assert_eq!(az.range(0, 999), naive(&samples, 0, 3));
        assert_eq!(az.range(2, 999).n, 1);
    }
}
