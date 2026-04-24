//! Automated task-segment resolution scoring.

pub mod continuation_signal;
pub mod scorer;
pub mod self_assessment_signal;
pub mod structural_signal;
pub mod tool_signal;
pub mod verification_signal;

pub use scorer::{ResolutionOverride, ResolutionScorer};
