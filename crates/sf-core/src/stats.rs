//! Derived range statistics — the numbers shown in the Statistics panel.
//!
//! These mirror `computeStats` in the `miniforge.html` prototype exactly, so the native
//! app and the prototype produce identical figures for the same input:
//!   * Peak      = max |sample|
//!   * RMS       = sqrt(sum_sq / N)
//!   * DC offset = sum / N   (mean)
//!   * Frequency = zero_crossings / (2 * duration_s)   (sine convention)

use crate::agg::Agg;

/// Everything the Statistics panel needs for a selection, in raw (non-dB) units.
/// dB formatting is left to the UI, matching the prototype which formats in JS via `db()`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RangeStats {
    /// Number of samples in the selection.
    pub n: u64,
    /// Duration of the selection in seconds.
    pub duration_s: f64,
    /// Start time of the selection in seconds.
    pub start_s: f64,
    /// Peak = max absolute sample value (linear, 0..=1 typical).
    pub peak: f32,
    /// Minimum signed sample value.
    pub min: f32,
    /// Time (seconds) of the first occurrence of `min`.
    pub min_pos_s: f64,
    /// Maximum signed sample value.
    pub max: f32,
    /// Time (seconds) of the first occurrence of `max`.
    pub max_pos_s: f64,
    /// RMS level (linear).
    pub rms: f64,
    /// DC offset / mean sample value (linear, signed).
    pub dc: f64,
    /// Number of zero crossings in the selection.
    pub zero_crossings: u64,
    /// Zero-crossing frequency estimate in Hz (sine convention).
    pub freq_hz: f64,
}

impl RangeStats {
    /// Derive statistics from an [`Agg`] over a selection starting at sample `start`,
    /// at the given `sample_rate` (Hz). Returns an all-zero struct for an empty selection.
    pub fn from_agg(agg: &Agg, start: u64, sample_rate: u32) -> Self {
        if agg.n == 0 {
            return RangeStats {
                n: 0,
                duration_s: 0.0,
                start_s: start as f64 / sample_rate as f64,
                peak: 0.0,
                min: 0.0,
                min_pos_s: 0.0,
                max: 0.0,
                max_pos_s: 0.0,
                rms: 0.0,
                dc: 0.0,
                zero_crossings: 0,
                freq_hz: 0.0,
            };
        }
        let sr = sample_rate as f64;
        let n = agg.n;
        let duration_s = n as f64 / sr;
        let peak = agg.min.abs().max(agg.max.abs());
        let rms = (agg.sum_sq / n as f64).sqrt();
        let dc = agg.sum / n as f64;
        let freq_hz = if duration_s > 0.0 {
            agg.zero_crossings as f64 / (2.0 * duration_s)
        } else {
            0.0
        };
        RangeStats {
            n,
            duration_s,
            start_s: start as f64 / sr,
            peak,
            min: agg.min,
            min_pos_s: agg.min_pos as f64 / sr,
            max: agg.max,
            max_pos_s: agg.max_pos as f64 / sr,
            rms,
            dc,
            zero_crossings: agg.zero_crossings,
            freq_hz,
        }
    }
}

/// Convert a linear amplitude to decibels (`20*log10`), matching the prototype's `db()`.
/// Returns `-inf` for non-positive input.
pub fn linear_to_db(x: f64) -> f64 {
    if x <= 0.0 {
        f64::NEG_INFINITY
    } else {
        20.0 * x.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summary::Analyzer;
    use std::f32::consts::PI;

    #[test]
    fn full_scale_sine_has_expected_rms_and_dc() {
        // 1 kHz sine at 48 kHz, exactly 48 periods over 1 s -> whole number of cycles.
        let sr = 48_000u32;
        let n = sr as usize;
        let samples: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * 1000.0 * i as f32 / sr as f32).sin())
            .collect();
        let az = Analyzer::new(&samples);
        let st = RangeStats::from_agg(&az.range(0, n), 0, sr);

        assert!((st.peak - 1.0).abs() < 1e-3, "peak {}", st.peak);
        // RMS of a full-scale sine ~ 1/sqrt(2) ~ 0.7071.
        assert!(
            (st.rms - std::f64::consts::FRAC_1_SQRT_2).abs() < 1e-3,
            "rms {}",
            st.rms
        );
        // DC offset of a whole number of cycles ~ 0.
        assert!(st.dc.abs() < 1e-3, "dc {}", st.dc);
        assert!((st.duration_s - 1.0).abs() < 1e-9);
        // Zero-crossing frequency of a 1 kHz sine ~ 1000 Hz.
        assert!((st.freq_hz - 1000.0).abs() < 2.0, "freq {}", st.freq_hz);
    }

    #[test]
    fn silence_is_all_zero() {
        let samples = vec![0.0f32; 4096];
        let az = Analyzer::new(&samples);
        let st = RangeStats::from_agg(&az.range(0, samples.len()), 0, 44_100);
        assert_eq!(st.peak, 0.0);
        assert_eq!(st.rms, 0.0);
        assert_eq!(st.dc, 0.0);
        assert_eq!(st.zero_crossings, 0);
        assert_eq!(linear_to_db(st.peak as f64), f64::NEG_INFINITY);
    }

    #[test]
    fn dc_bias_detected() {
        let samples = vec![0.5f32; 1000];
        let az = Analyzer::new(&samples);
        let st = RangeStats::from_agg(&az.range(0, 1000), 0, 44_100);
        assert!((st.dc - 0.5).abs() < 1e-6);
        assert!((st.rms - 0.5).abs() < 1e-6);
        assert_eq!(st.zero_crossings, 0);
    }
}
