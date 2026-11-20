//! Player-owned mapping from measured audio to generic transient visual controls.
//! No GPU, device, provider-name policy, clock timer or FFT work lives here.
use player_domain::{AnalysisFrame, Capabilities, MediaId};
use serde::Serialize;
/// Player-side wire DTO; the native adapter maps it into the independent Still ABI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct VisualControls {
    pub time_seconds: f64,
    pub motion: f32,
    pub turbulence: f32,
    pub light: f32,
}
impl VisualControls {
    pub fn is_valid(self) -> bool {
        self.time_seconds.is_finite()
            && self.time_seconds >= 0.0
            && [self.motion, self.turbulence, self.light]
                .iter()
                .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct VisualModulation {
    pub generation: u64,
    pub sample_rate: u32,
    pub window_end_frame: u64,
    pub controls: VisualControls,
}

#[derive(Default)]
pub struct ModulationBus {
    binding: Option<(MediaId, u64, u32)>,
    last_end: u64,
    smoothed: [f32; 3],
    latest: Option<VisualModulation>,
}
impl ModulationBus {
    pub fn clear(&mut self) {
        self.binding = None;
        self.last_end = 0;
        self.smoothed = [0.0; 3];
        self.latest = None;
    }
    /// Caller supplies the effective capabilities and the accepted playing observation.
    /// None releases immediately; smoothing never carries across media/epoch/gaps.
    pub fn update_audio(
        &mut self,
        capabilities: Capabilities,
        media: Option<&MediaId>,
        frame: Option<&AnalysisFrame>,
    ) -> Option<VisualModulation> {
        let accepted = media
            .zip(frame)
            .filter(|(_, f)| capabilities.pcm_analysis && f.is_valid());
        let Some((media, frame)) = accepted else {
            self.clear();
            return None;
        };
        let same = self.binding.as_ref().is_some_and(|(id, epoch, rate)| {
            id == media && *epoch == frame.generation && *rate == frame.sample_rate
        });
        if same && frame.window_end_frame <= self.last_end {
            return self.latest;
        }
        let gap = frame.window_end_frame.saturating_sub(self.last_end);
        let reset = !same || gap > u64::from(frame.sample_rate) / 4;
        if reset {
            self.binding = Some((media.clone(), frame.generation, frame.sample_rate));
            self.smoothed = [0.0; 3];
        }
        let dt = if reset {
            frame.hop_frames as f32
        } else {
            gap as f32
        } / frame.sample_rate as f32;
        self.last_end = frame.window_end_frame;
        let f = frame.features;
        let energy = level(f.rms);
        let low = level(f.sub_bass.hypot(f.bass));
        // Power-weighted position in the compact log spectrum; no raw FFT or PCM.
        let mut power = 0.0;
        let mut moment = 0.0;
        for (i, amplitude) in frame.spectrum.iter().enumerate() {
            let p = amplitude * amplitude;
            power += p;
            moment += p * i as f32 / (frame.spectrum.len() - 1) as f32;
        }
        let shape = if power > 1e-12 { moment / power } else { 0.0 };
        let middle = level(f.low_mids.hypot(f.mids).hypot(f.presence));
        let targets = [
            low,
            (middle * 0.7 + energy * shape * 0.3).clamp(0.0, 1.0),
            (f.transient * 0.8 + energy * 0.2).clamp(0.0, 1.0) * (energy * 4.0).min(1.0),
        ];
        for (value, target) in self.smoothed.iter_mut().zip(targets) {
            let seconds = if target > *value { 0.02 } else { 0.22 };
            *value += (target - *value) * (1.0 - (-dt / seconds).exp());
        }
        let controls = VisualControls {
            time_seconds: frame.window_end_frame as f64 / f64::from(frame.sample_rate),
            motion: self.smoothed[0],
            turbulence: self.smoothed[1],
            light: self.smoothed[2],
        };
        debug_assert!(controls.is_valid());
        let result = VisualModulation {
            generation: frame.generation,
            sample_rate: frame.sample_rate,
            window_end_frame: frame.window_end_frame,
            controls,
        };
        self.latest = Some(result);
        Some(result)
    }
}

/// Fixed dBFS response; silence stays neutral. No per-track automatic gain normalization.
fn level(rms: f32) -> f32 {
    ((20.0 * rms.max(1e-8).log10() + 66.0) / 48.0).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use player_domain::AudioFeatures;
    fn frame(end: u64) -> AnalysisFrame {
        AnalysisFrame {
            generation: 1,
            sample_rate: 48000,
            window_frames: 4096,
            hop_frames: 960,
            window_start_frame: end - 4096,
            window_end_frame: end,
            dropped_frames: 0,
            spectrum: [0.0; 32],
            features: AudioFeatures {
                playback_position_ms: end * 1000 / 48000,
                ..Default::default()
            },
        }
    }
    fn media() -> MediaId {
        MediaId::parse("local:track:fixture").unwrap()
    }
    #[test]
    fn bass_transient_and_spectral_distribution_have_distinct_bounded_effects() {
        let id = media();
        let mut bus = ModulationBus::default();
        let mut f = frame(4800);
        f.features.rms = 0.05;
        f.features.bass = 0.05;
        f.spectrum[4] = 0.05;
        let bass = bus
            .update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
            .unwrap();
        assert!(bass.controls.motion > 0.4 && bass.controls.turbulence < 0.1);
        bus.clear();
        f.features.bass = 0.0;
        f.features.mids = 0.05;
        f.spectrum = [0.0; 32];
        f.spectrum[25] = 0.05;
        let high = bus
            .update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
            .unwrap();
        assert_eq!(high.controls.motion, 0.0);
        assert!(high.controls.turbulence > bass.controls.turbulence + 0.3);
        bus.clear();
        f.features.transient = 1.0;
        let onset = bus
            .update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
            .unwrap();
        assert!(onset.controls.light > high.controls.light + 0.4);
        assert_eq!(onset.controls.time_seconds, 0.1);
        assert!(onset.controls.is_valid());
    }
    #[test]
    fn reset_policy_rejects_remote_invalid_missing_and_old_samples() {
        let id = media();
        let mut bus = ModulationBus::default();
        let mut f = frame(4800);
        f.features.rms = 0.1;
        f.features.bass = 0.1;
        let first = bus
            .update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
            .unwrap();
        let duplicate = bus
            .update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
            .unwrap();
        assert_eq!(first, duplicate);
        assert!(
            bus.update_audio(Capabilities::REMOTE, Some(&id), Some(&f))
                .is_none()
        );
        assert!(
            bus.update_audio(Capabilities::LOCAL, Some(&id), None)
                .is_none()
        );
        f.features.rms = f32::NAN;
        assert!(
            bus.update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
                .is_none()
        );
        f.features.rms = 0.1;
        bus.update_audio(Capabilities::LOCAL, Some(&id), Some(&f));
        f.generation += 1;
        f.features = AudioFeatures {
            playback_position_ms: 100,
            ..Default::default()
        };
        f.spectrum = [0.0; 32];
        let reset = bus
            .update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
            .unwrap();
        assert_eq!(reset.controls.motion, 0.0);
        assert_eq!(reset.controls.light, 0.0);
    }
    #[test]
    fn attack_release_uses_sample_time_and_a_gap_cannot_carry_old_energy() {
        let id = media();
        let mut bus = ModulationBus::default();
        let mut f = frame(4800);
        f.features.rms = 0.05;
        f.features.bass = 0.05;
        let peak = bus
            .update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
            .unwrap()
            .controls
            .motion;
        f = frame(5760);
        let release = bus
            .update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
            .unwrap()
            .controls
            .motion;
        assert!(release < peak && release > peak * 0.9);
        f = frame(24000);
        let gap = bus
            .update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
            .unwrap();
        assert_eq!(gap.controls.motion, 0.0);
    }

    #[test]
    fn envelope_is_cadence_independent_and_media_binding_does_not_inherit_energy() {
        fn simulate(hz: u32) -> f32 {
            let id = media();
            let mut bus = ModulationBus::default();
            let hop = 48000 / hz;
            let mut value = 0.0;
            for i in 0..hz + hz / 5 {
                let mut f = frame(4800 + u64::from(i * hop));
                f.hop_frames = hop;
                if i < hz {
                    f.features.rms = 0.05;
                    f.features.bass = 0.05;
                }
                value = bus
                    .update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
                    .unwrap()
                    .controls
                    .motion;
            }
            value
        }
        assert!((simulate(50) - simulate(100)).abs() < 0.00001);
        let id = media();
        let other = MediaId::parse("local:track:second").unwrap();
        let mut bus = ModulationBus::default();
        let mut f = frame(4800);
        f.features.rms = 0.05;
        f.features.bass = 0.05;
        let first = bus
            .update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
            .unwrap();
        f = AnalysisFrame {
            window_start_frame: 5760 - 4096,
            window_end_frame: 5760,
            features: player_domain::AudioFeatures {
                playback_position_ms: 120,
                ..f.features
            },
            ..f
        };
        assert!(
            bus.update_audio(Capabilities::LOCAL, Some(&id), Some(&f))
                .unwrap()
                .controls
                .motion
                > first.controls.motion
        );
        let replacement = bus
            .update_audio(Capabilities::LOCAL, Some(&other), Some(&f))
            .unwrap();
        assert!((replacement.controls.motion - first.controls.motion).abs() < 0.00001);
        let old = frame(4800);
        assert_eq!(
            bus.update_audio(Capabilities::LOCAL, Some(&other), Some(&old))
                .unwrap()
                .window_end_frame,
            5760
        );
    }
}
