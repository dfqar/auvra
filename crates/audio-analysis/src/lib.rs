//! Windowed post-DSP analysis. Run on its worker, never in an audio callback.
pub use player_domain::{AnalysisFrame, AudioFeatures, SPECTRUM_BANDS};
mod analyzer;
pub use analyzer::Analyzer;
#[cfg(test)]
mod tests;
