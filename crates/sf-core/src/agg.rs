//! `Agg` — associative aggregate (a monoid) over a contiguous run of audio samples.
//!
//! Every statistic SoundForge shows for a selection is derivable from this struct,
//! and — crucially — two adjacent `Agg`s can be `combine`d into the `Agg` of their
//! concatenation in O(1). That associativity is what lets the summary pyramid answer
//! range queries by stitching together a handful of precomputed blocks instead of
//! rescanning millions of samples. See [`crate::summary`].
//!
//! The `combine` operation is *ordered*: `combine(a, b)` assumes `b`'s samples come
//! immediately after `a`'s. It is associative but NOT commutative (min/max position
//! tie-breaking and zero-crossing boundary handling both depend on order).

/// Aggregate over a run of samples. `identity()` is the neutral element (empty run).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Agg {
    /// Number of samples in the run.
    pub n: u64,
    /// Sum of sample values (f64 accumulator for precision on long runs).
    pub sum: f64,
    /// Sum of squared sample values.
    pub sum_sq: f64,
    /// Minimum (signed) sample value; `+inf` for the empty run.
    pub min: f32,
    /// Absolute sample index of the first occurrence of `min`.
    pub min_pos: u64,
    /// Maximum (signed) sample value; `-inf` for the empty run.
    pub max: f32,
    /// Absolute sample index of the first occurrence of `max`.
    pub max_pos: u64,
    /// Number of zero crossings strictly inside this run (nonzero-sign convention,
    /// matching the prototype: zero samples are skipped, a crossing is a sign flip
    /// between consecutive nonzero samples).
    pub zero_crossings: u64,
    /// Sign (-1/0/+1) of the first nonzero sample in the run (0 if the run is all zeros/empty).
    pub first_nz_sign: i8,
    /// Sign (-1/0/+1) of the last nonzero sample in the run.
    pub last_nz_sign: i8,
}

#[inline]
fn sign(v: f32) -> i8 {
    if v > 0.0 {
        1
    } else if v < 0.0 {
        -1
    } else {
        0
    }
}

impl Agg {
    /// The neutral element: an empty run. `combine(identity, x) == x == combine(x, identity)`.
    pub const fn identity() -> Self {
        Agg {
            n: 0,
            sum: 0.0,
            sum_sq: 0.0,
            min: f32::INFINITY,
            min_pos: 0,
            max: f32::NEG_INFINITY,
            max_pos: 0,
            zero_crossings: 0,
            first_nz_sign: 0,
            last_nz_sign: 0,
        }
    }

    /// Aggregate of a single sample `v` located at absolute index `pos`.
    #[inline]
    pub fn from_sample(v: f32, pos: u64) -> Self {
        let s = sign(v);
        let vd = v as f64;
        Agg {
            n: 1,
            sum: vd,
            sum_sq: vd * vd,
            min: v,
            min_pos: pos,
            max: v,
            max_pos: pos,
            zero_crossings: 0,
            first_nz_sign: s,
            last_nz_sign: s,
        }
    }

    /// Fold a contiguous slice of samples starting at absolute index `start_pos`.
    pub fn from_samples(samples: &[f32], start_pos: u64) -> Self {
        let mut acc = Agg::identity();
        for (i, &v) in samples.iter().enumerate() {
            acc = acc.combine(&Agg::from_sample(v, start_pos + i as u64));
        }
        acc
    }

    /// Combine `self` with `other`, where `other`'s samples immediately follow `self`'s.
    /// Associative; returns the aggregate of the concatenated run.
    #[inline]
    pub fn combine(&self, other: &Agg) -> Agg {
        // min: strict `<` keeps the earlier occurrence on ties (matches prototype's `if(v<minV)`).
        let (min, min_pos) = if other.min < self.min {
            (other.min, other.min_pos)
        } else {
            (self.min, self.min_pos)
        };
        let (max, max_pos) = if other.max > self.max {
            (other.max, other.max_pos)
        } else {
            (self.max, self.max_pos)
        };

        // Zero crossing spanning the boundary between the two runs: a sign flip between
        // self's last nonzero sample and other's first nonzero sample.
        let boundary = if self.last_nz_sign != 0
            && other.first_nz_sign != 0
            && self.last_nz_sign != other.first_nz_sign
        {
            1
        } else {
            0
        };

        let first_nz_sign = if self.first_nz_sign != 0 {
            self.first_nz_sign
        } else {
            other.first_nz_sign
        };
        let last_nz_sign = if other.last_nz_sign != 0 {
            other.last_nz_sign
        } else {
            self.last_nz_sign
        };

        Agg {
            n: self.n + other.n,
            sum: self.sum + other.sum,
            sum_sq: self.sum_sq + other.sum_sq,
            min,
            min_pos,
            max,
            max_pos,
            zero_crossings: self.zero_crossings + other.zero_crossings + boundary,
            first_nz_sign,
            last_nz_sign,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_neutral() {
        let a = Agg::from_samples(&[0.1, -0.2, 0.3], 0);
        assert_eq!(Agg::identity().combine(&a), a);
        assert_eq!(a.combine(&Agg::identity()), a);
    }

    #[test]
    fn folding_matches_pairwise_combine() {
        let s = [0.5f32, -0.5, 0.25, -0.75, 0.0, 0.9];
        let whole = Agg::from_samples(&s, 0);
        // Split at every point; combined halves must equal the whole.
        for split in 0..=s.len() {
            let left = Agg::from_samples(&s[..split], 0);
            let right = Agg::from_samples(&s[split..], split as u64);
            assert_eq!(left.combine(&right), whole, "split at {split}");
        }
    }

    #[test]
    fn min_max_positions_are_first_occurrence() {
        let s = [0.2f32, -0.4, 0.6, -0.4, 0.6];
        let a = Agg::from_samples(&s, 0);
        assert_eq!(a.max, 0.6);
        assert_eq!(a.max_pos, 2); // first 0.6
        assert_eq!(a.min, -0.4);
        assert_eq!(a.min_pos, 1); // first -0.4
    }

    #[test]
    fn zero_crossings_skip_zeros() {
        // +, +, 0, -, +  => flips at the -, then back at +  => 2 crossings.
        let s = [0.3f32, 0.7, 0.0, -0.2, 0.5];
        let a = Agg::from_samples(&s, 0);
        assert_eq!(a.zero_crossings, 2);
    }

    #[test]
    fn boundary_crossing_counted_once_when_combining() {
        // [+, +] then [-, -]: one crossing exactly at the boundary.
        let left = Agg::from_samples(&[0.3f32, 0.7], 0);
        let right = Agg::from_samples(&[-0.2f32, -0.4], 2);
        assert_eq!(left.zero_crossings, 0);
        assert_eq!(right.zero_crossings, 0);
        assert_eq!(left.combine(&right).zero_crossings, 1);
    }
}
