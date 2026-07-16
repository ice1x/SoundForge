//! Sample-level edit operations (task 16): normalize, fade in/out, silence.
//!
//! Every function here mutates a plain `&mut [f32]` **in place** and does nothing else — no
//! I/O, no knowledge of documents, channels or pyramids. That is deliberate: the caller
//! passes the slice for the selected range of one channel, so "apply to the selection" is
//! just slicing, and every rule below is testable against a handful of samples.
//!
//! Trimming is *not* here: it changes the buffer's length rather than its contents, so it
//! belongs to whoever owns the buffer's storage (the shell's PCM cache).
//!
//! # Warning for callers
//!
//! These change sample values without changing the length, which is exactly the case a
//! [`crate::Pyramid`] cannot detect — [`crate::Analyzer::with_pyramid`] only checks the
//! length. Rebuild the pyramid of any channel you edit, or statistics will silently be
//! computed from stale blocks.

/// The loudest absolute sample value, or `0.0` for an empty slice.
pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// The gain that lifts `peak` to `target_peak`, or `1.0` when there is no such gain.
///
/// Separated from applying it because a multi-channel normalize must compute **one** gain
/// across every channel and apply that same gain to all of them. Normalizing each channel to
/// its own peak would give a quiet channel more gain than a loud one and shift the stereo
/// image — a loud left and quiet right would come out equal.
///
/// Returns `1.0` when the range is silent (`peak == 0` has no gain that reaches the target;
/// dividing would give `inf` and fill the buffer with NaN), when `peak` is not finite, or
/// when `target_peak` is not positive.
pub fn gain_for(peak: f32, target_peak: f32) -> f32 {
    // `is_nan()` is spelled out rather than folded into a negated `>`: a NaN target must fall
    // through to "no gain" like the other degenerate cases, and relying on NaN comparisons
    // being false to get there reads like a bug even when it is not.
    if peak == 0.0 || !peak.is_finite() || target_peak.is_nan() || target_peak <= 0.0 {
        return 1.0;
    }
    target_peak / peak
}

/// Multiply every sample by `gain`.
pub fn apply_gain(samples: &mut [f32], gain: f32) {
    for s in samples.iter_mut() {
        *s *= gain;
    }
}

/// Scale a single buffer so its loudest sample reaches `target_peak`. Returns the gain.
///
/// Convenience for a lone buffer. **Do not** call this per channel to normalize a
/// multi-channel selection — see [`gain_for`] for why. Use [`peak`] across the channels,
/// then [`gain_for`], then [`apply_gain`] on each.
pub fn normalize(samples: &mut [f32], target_peak: f32) -> f32 {
    let gain = gain_for(peak(samples), target_peak);
    apply_gain(samples, gain);
    gain
}

/// Linear fade from silence to unity across the whole slice.
///
/// The first sample becomes exactly `0.0` and the last is left untouched (gain `1.0`).
/// A slice shorter than two samples is left alone: a fade needs somewhere to travel, and
/// silencing a one-sample selection is not what anyone means by "fade in".
pub fn fade_in(samples: &mut [f32]) {
    let n = samples.len();
    if n < 2 {
        return;
    }
    let last = (n - 1) as f32;
    for (i, s) in samples.iter_mut().enumerate() {
        *s *= i as f32 / last;
    }
}

/// Linear fade from unity to silence across the whole slice.
///
/// Mirror of [`fade_in`]: the first sample is untouched and the last becomes exactly `0.0`.
/// Slices shorter than two samples are left alone.
pub fn fade_out(samples: &mut [f32]) {
    let n = samples.len();
    if n < 2 {
        return;
    }
    let last = (n - 1) as f32;
    for (i, s) in samples.iter_mut().enumerate() {
        *s *= 1.0 - i as f32 / last;
    }
}

/// Replace every sample with digital silence.
pub fn silence(samples: &mut [f32]) {
    samples.fill(0.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_scales_the_loudest_sample_to_the_target() {
        let mut s = [0.25, -0.5, 0.125];
        let gain = normalize(&mut s, 1.0);
        assert_eq!(gain, 2.0);
        assert_eq!(s, [0.5, -1.0, 0.25]);
    }

    #[test]
    fn normalize_measures_the_peak_by_absolute_value() {
        // The loudest sample is negative: normalizing on the max rather than the max ABS
        // would scale by the wrong factor and clip.
        let mut s = [0.1, -0.8];
        normalize(&mut s, 0.8);
        assert!((s[1] + 0.8).abs() < 1e-6, "{s:?}");
        assert!(s[0] <= 0.8);
    }

    #[test]
    fn normalize_preserves_the_waveform_shape() {
        // Normalizing is a gain change: every ratio between samples must survive it.
        let mut s = [0.2, -0.1, 0.4, 0.05];
        normalize(&mut s, 1.0);
        assert!((s[0] / s[2] - 0.5).abs() < 1e-6);
        assert!((s[1] / s[2] + 0.25).abs() < 1e-6);
    }

    #[test]
    fn normalize_can_attenuate_as_well_as_amplify() {
        let mut s = [1.0, -1.0];
        assert_eq!(normalize(&mut s, 0.5), 0.5);
        assert_eq!(s, [0.5, -0.5]);
    }

    #[test]
    fn normalizing_silence_is_a_no_op_not_a_division_by_zero() {
        // peak == 0 would make the gain inf and fill the buffer with NaN.
        let mut s = [0.0, 0.0, 0.0];
        assert_eq!(normalize(&mut s, 1.0), 1.0);
        assert_eq!(s, [0.0, 0.0, 0.0]);
        assert!(s.iter().all(|v| v.is_finite()));
    }

    #[test]
    fn normalize_rejects_a_non_positive_target() {
        // A zero or negative target is a caller bug; silently inverting the waveform (or
        // zeroing it) would be worse than doing nothing.
        for target in [0.0, -1.0, f32::NAN] {
            let mut s = [0.25, -0.5];
            assert_eq!(normalize(&mut s, target), 1.0, "target {target}");
            assert_eq!(s, [0.25, -0.5]);
        }
    }

    #[test]
    fn normalize_leaves_a_non_finite_buffer_alone() {
        let mut s = [f32::INFINITY, 0.5];
        assert_eq!(normalize(&mut s, 1.0), 1.0);
    }

    #[test]
    fn normalize_of_an_empty_range_does_nothing() {
        let mut s: [f32; 0] = [];
        assert_eq!(normalize(&mut s, 1.0), 1.0);
    }

    #[test]
    fn peak_reports_the_loudest_absolute_value() {
        assert_eq!(peak(&[0.1, -0.8, 0.3]), 0.8);
        assert_eq!(peak(&[]), 0.0);
        assert_eq!(peak(&[0.0, -0.0]), 0.0);
    }

    #[test]
    fn one_gain_across_channels_preserves_the_stereo_image() {
        // THE reason gain_for/apply_gain are split out of normalize. Normalizing each channel
        // to its own peak would give the quiet one more gain than the loud one, and a mix
        // panned hard left would come out centred.
        let mut left = [1.0f32, -1.0];
        let mut right = [0.25f32, -0.25];
        let g = gain_for(peak(&left).max(peak(&right)), 1.0);
        apply_gain(&mut left, g);
        apply_gain(&mut right, g);
        assert_eq!(left, [1.0, -1.0]);
        assert_eq!(
            right,
            [0.25, -0.25],
            "the quiet channel must stay 4x quieter"
        );

        // What the per-channel mistake would have produced, for contrast.
        let mut wrong = [0.25f32, -0.25];
        normalize(&mut wrong, 1.0);
        assert_eq!(
            wrong,
            [1.0, -1.0],
            "per-channel normalize equalises the channels"
        );
    }

    #[test]
    fn gain_for_refuses_the_degenerate_cases() {
        assert_eq!(gain_for(0.0, 1.0), 1.0, "silence has no gain to the target");
        assert_eq!(gain_for(f32::INFINITY, 1.0), 1.0);
        assert_eq!(gain_for(f32::NAN, 1.0), 1.0);
        assert_eq!(gain_for(0.5, 0.0), 1.0);
        assert_eq!(gain_for(0.5, -1.0), 1.0);
        assert_eq!(gain_for(0.5, 1.0), 2.0);
    }

    #[test]
    fn fade_in_runs_from_exactly_silence_to_exactly_unity() {
        let mut s = [1.0; 5];
        fade_in(&mut s);
        assert_eq!(s, [0.0, 0.25, 0.5, 0.75, 1.0]);
    }

    #[test]
    fn fade_out_runs_from_exactly_unity_to_exactly_silence() {
        let mut s = [1.0; 5];
        fade_out(&mut s);
        assert_eq!(s, [1.0, 0.75, 0.5, 0.25, 0.0]);
    }

    #[test]
    fn a_fade_scales_the_signal_rather_than_replacing_it() {
        let mut s = [0.5, 0.5, -0.5];
        fade_in(&mut s);
        assert_eq!(s, [0.0, 0.25, -0.5]);
    }

    #[test]
    fn fade_in_and_fade_out_are_mirror_images() {
        let mut a = [1.0; 9];
        let mut b = [1.0; 9];
        fade_in(&mut a);
        fade_out(&mut b);
        b.reverse();
        assert_eq!(a, b);
    }

    #[test]
    fn a_fade_too_short_to_travel_is_left_alone() {
        // Silencing a one-sample selection is not what "fade in" means.
        for f in [fade_in as fn(&mut [f32]), fade_out] {
            let mut one = [0.7];
            f(&mut one);
            assert_eq!(one, [0.7]);
            let mut none: [f32; 0] = [];
            f(&mut none);
        }
    }

    #[test]
    fn silence_zeroes_everything() {
        let mut s = [0.5, -1.0, 0.25];
        silence(&mut s);
        assert_eq!(s, [0.0, 0.0, 0.0]);
    }

    #[test]
    fn every_op_is_confined_to_the_slice_it_is_given() {
        // The shell applies these to a sub-slice of one channel, so an op that wrote outside
        // its slice would corrupt neighbouring audio (or another channel entirely).
        let mut buf = [1.0f32; 9];
        normalize(&mut buf[3..6], 0.5);
        fade_in(&mut buf[3..6]);
        assert_eq!(
            buf[..3],
            [1.0, 1.0, 1.0],
            "samples before the range changed"
        );
        assert_eq!(buf[6..], [1.0, 1.0, 1.0], "samples after the range changed");

        let mut buf2 = [1.0f32; 9];
        silence(&mut buf2[4..5]);
        assert_eq!(buf2, [1.0, 1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0, 1.0]);
    }
}
