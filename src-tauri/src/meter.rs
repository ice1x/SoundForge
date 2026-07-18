//! Level metering (task 20): the peak amplitude of what is currently being **played** or
//! **recorded**, so the UI can draw a SoundForge-style volume indicator.
//!
//! Both audio paths are realtime — the output callback in [`crate::player`] and the input
//! callback in [`crate::recorder`] must not lock, allocate or block. So the meter is just two
//! pieces, both realtime-safe:
//!
//! * [`buffer_peak`] — a plain function that reduces a buffer of samples to a single peak
//!   magnitude. Called from inside the callback on the samples it just moved.
//! * [`PeakCell`] — one lock-free `f32` cell the callback pushes that peak into with
//!   [`PeakCell::record`] and the UI polls with [`PeakCell::take`].
//!
//! ## Why "peak since last poll", not "peak of the last buffer"
//!
//! A meter is polled far more slowly than the audio callback fires — the UI reads it per
//! animation frame (~60 Hz) or, while recording, every 100 ms, whereas a callback lands every
//! few milliseconds. If the cell only held the *most recent* buffer's peak, a poll would sample
//! one callback in ten and miss the loudest transients between them — exactly the peaks a level
//! meter exists to show. So [`PeakCell::record`] keeps the **maximum** seen, and
//! [`PeakCell::take`] reads *and resets* it: every poll gets the true peak of everything that
//! played since the previous poll. The smoothing/decay that makes a meter look like a meter is
//! the UI's job (`ui/lib.js`), driven by animation frames rather than the audio clock.

use std::sync::atomic::{AtomicU32, Ordering};

/// The peak magnitude of `samples`: the largest `|sample|`, or `0.0` for an empty buffer.
///
/// This is what a level meter shows — the loudest instant in the buffer, not its average. Runs
/// inside the realtime audio callback, so it only reads: no allocation, no branching on NaN
/// beyond what `max` already does (a `NaN` sample compares false and is skipped, which is the
/// right behaviour for a meter — a stray NaN must not peg it to full scale).
pub fn buffer_peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, &s| {
        let a = s.abs();
        if a > m {
            a
        } else {
            m
        }
    })
}

/// A lock-free cell holding the peak magnitude seen since it was last read.
///
/// Single-producer (the audio callback) / single-consumer (the UI poll). The `f32` is stored as
/// its bit pattern in an [`AtomicU32`] because there is no atomic float; [`record`](Self::record)
/// keeps the running maximum with a compare-and-swap loop, and [`take`](Self::take) resets it to
/// zero as it reads. See the module docs for why the reset matters.
#[derive(Default)]
pub struct PeakCell(AtomicU32);

impl PeakCell {
    pub fn new() -> PeakCell {
        PeakCell(AtomicU32::new(0))
    }

    /// Fold `peak` (already a magnitude) into the running maximum. Realtime-safe: a short CAS
    /// loop with no allocation or blocking. A non-finite or negative input is ignored so the
    /// meter cannot be pegged by a bad sample.
    pub fn record(&self, peak: f32) {
        // Accept only a real, positive level: this rejects NaN and non-positive values, and
        // `is_finite` also rejects +∞, which would otherwise peg the meter permanently.
        if !(peak.is_finite() && peak > 0.0) {
            return;
        }
        let bits = peak.to_bits();
        let mut cur = self.0.load(Ordering::Relaxed);
        while peak > f32::from_bits(cur) {
            match self
                .0
                .compare_exchange_weak(cur, bits, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
    }

    /// The peak since the last `take`, resetting the cell to zero. This is the poll the UI does.
    pub fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Ordering::Relaxed))
    }

    /// The current peak without resetting it. For tests and diagnostics — the UI uses
    /// [`take`](Self::take).
    pub fn peek(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_peak_of_an_empty_buffer_is_zero() {
        assert_eq!(buffer_peak(&[]), 0.0);
    }

    #[test]
    fn buffer_peak_is_the_largest_magnitude_ignoring_sign() {
        assert_eq!(buffer_peak(&[0.1, -0.9, 0.3]), 0.9);
        assert_eq!(buffer_peak(&[-1.0, 0.5]), 1.0);
        assert_eq!(buffer_peak(&[0.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn buffer_peak_reports_above_full_scale_so_clipping_shows() {
        // A meter must be able to say "over 0 dBFS"; clamping to 1.0 here would hide clipping.
        assert_eq!(buffer_peak(&[0.2, 1.4, -0.3]), 1.4);
    }

    #[test]
    fn buffer_peak_skips_a_stray_nan_rather_than_pegging_full_scale() {
        assert_eq!(buffer_peak(&[0.4, f32::NAN, -0.2]), 0.4);
    }

    #[test]
    fn peak_cell_keeps_the_maximum_recorded() {
        let cell = PeakCell::new();
        cell.record(0.3);
        cell.record(0.7);
        cell.record(0.5); // lower — must not lower the reading
        assert_eq!(cell.peek(), 0.7);
    }

    #[test]
    fn take_reads_the_peak_and_resets_the_cell() {
        // "Peak since the last poll": a read empties the cell so the next poll starts fresh.
        let cell = PeakCell::new();
        cell.record(0.8);
        assert_eq!(cell.take(), 0.8);
        assert_eq!(cell.peek(), 0.0, "take must reset the cell");
        cell.record(0.2);
        assert_eq!(cell.take(), 0.2);
    }

    #[test]
    fn a_fresh_cell_reads_zero() {
        assert_eq!(PeakCell::new().take(), 0.0);
        assert_eq!(PeakCell::default().peek(), 0.0);
    }

    #[test]
    fn record_ignores_non_positive_and_non_finite_input() {
        let cell = PeakCell::new();
        cell.record(0.5);
        cell.record(-2.0); // magnitude is the caller's job; a negative must not win
        cell.record(f32::NAN); // NaN compares false against everything
        cell.record(f32::INFINITY); // > any real level, but must not peg the meter forever
        assert_eq!(cell.peek(), 0.5, "only the valid 0.5 survived");
    }
}
