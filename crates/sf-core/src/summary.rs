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

/// An analyzer over a borrowed sample buffer with a precomputed summary pyramid.
///
/// Samples are borrowed, not owned, so the buffer may itself be a memory-mapped file.
pub struct Analyzer<'a> {
    samples: &'a [f32],
    /// `levels[0]` = one `Agg` per leaf block (`LEAF` samples each).
    /// `levels[k][j]` covers `FANOUT^k` leaf blocks starting at leaf `j*FANOUT^k`.
    levels: Vec<Vec<Agg>>,
}

impl<'a> Analyzer<'a> {
    /// Build the summary pyramid over `samples`. O(n) time, O(n / (FANOUT-1)) extra space.
    pub fn new(samples: &'a [f32]) -> Self {
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

        Analyzer { samples, levels }
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
        let mut i = start_leaf;
        while i < end_leaf {
            // Grow the level as long as the block stays aligned and fits within end_leaf.
            let mut level = 0usize;
            loop {
                let next = level + 1;
                let span_next = FANOUT.pow(next as u32);
                if next < self.levels.len()
                    && i.is_multiple_of(span_next)
                    && i + span_next <= end_leaf
                    && (i / span_next) < self.levels[next].len()
                {
                    level = next;
                } else {
                    break;
                }
            }
            let span = FANOUT.pow(level as u32);
            acc = acc.combine(&self.levels[level][i / span]);
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
    fn clamps_out_of_bounds() {
        let samples = vec![0.1f32, 0.2, 0.3];
        let az = Analyzer::new(&samples);
        assert_eq!(az.range(0, 999), naive(&samples, 0, 3));
        assert_eq!(az.range(2, 999).n, 1);
    }
}
