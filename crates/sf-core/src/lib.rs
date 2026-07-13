//! # sf-core
//!
//! The analysis core of the SoundForge-style editor: **seamless range statistics over
//! arbitrarily large audio**. This crate is pure Rust with no GUI, audio-hardware, or
//! OS dependencies, so it is fully unit-testable on its own.
//!
//! The key idea (and the product's differentiator) lives in [`summary::Analyzer`]: a
//! summary pyramid of associative [`agg::Agg`] blocks that answers statistics for *any*
//! selection in time independent of the selection length. Dragging a selection across a
//! multi-hour file updates the Statistics panel in microseconds — no "compute & wait".
//!
//! ```
//! use sf_core::{summary::Analyzer, stats::RangeStats};
//!
//! let samples: Vec<f32> = (0..48_000)
//!     .map(|i| (i as f32 * 0.1).sin())
//!     .collect();
//! let az = Analyzer::new(&samples);
//! let stats = RangeStats::from_agg(&az.range(0, samples.len()), 0, 48_000);
//! assert!(stats.rms > 0.0);
//! ```

pub mod agg;
pub mod stats;
pub mod summary;

pub use agg::Agg;
pub use stats::{linear_to_db, RangeStats};
pub use summary::Analyzer;
