//! Compact post-DSP observations. No PCM, FFT implementation or audio-device dependency.
use serde::{Deserialize, Serialize};

pub const SPECTRUM_BANDS: usize = 32;
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioFeatures {
    /// End of the measured window in presented output samples, converted to milliseconds.
    pub playback_position_ms: u64,
    pub rms: f32,
    pub sub_bass: f32,
    pub bass: f32,
    pub low_mids: f32,
    pub mids: f32,
    pub presence: f32,
    pub highs: f32,
    pub transient: f32,
    pub spectral_centroid_hz: f32,
}
impl AudioFeatures {
    pub fn is_valid(self) -> bool {
        [
            self.rms,
            self.sub_bass,
            self.bass,
            self.low_mids,
            self.mids,
            self.presence,
            self.highs,
            self.transient,
        ]
        .iter()
        .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
            && self.spectral_centroid_hz.is_finite()
            && self.spectral_centroid_hz >= 0.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnalysisFrame {
    /// Discontinuity epoch: changes on load/seek/pause/resume/stop, never an OS timestamp.
    pub generation: u64,
    pub sample_rate: u32,
    pub window_frames: u32,
    pub hop_frames: u32,
    /// Half-open sample interval [start, end), using the output sample clock.
    pub window_start_frame: u64,
    pub window_end_frame: u64,
    pub features: AudioFeatures,
    /// RMS energy in 32 log-spaced bands, 20 Hz to min(20 kHz, Nyquist).
    pub spectrum: [f32; SPECTRUM_BANDS],
    pub dropped_frames: u64,
}
impl AnalysisFrame {
    pub fn is_valid(&self) -> bool {
        (8000..=384000).contains(&self.sample_rate)
            && self.window_frames.is_power_of_two()
            && (512..=32768).contains(&self.window_frames)
            && self.hop_frames > 0
            && self.hop_frames <= self.window_frames
            && self.window_end_frame.checked_sub(self.window_start_frame)
                == Some(u64::from(self.window_frames))
            && self.features.playback_position_ms
                == self.window_end_frame.saturating_mul(1000) / u64::from(self.sample_rate)
            && self.features.is_valid()
            && self.features.spectral_centroid_hz <= self.sample_rate as f32 * 0.5
            && self
                .spectrum
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
    }
}
