//! Provider-independent, versioned local sound preferences. No PCM or IO.
use crate::PlayerError;
use serde::{Deserialize, Serialize};

pub const EQ_FREQUENCIES: [f64; 5] = [80.0, 300.0, 1000.0, 4000.0, 12000.0];
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EqualizerSettings {
    pub enabled: bool,
    pub preamp_db: f32,
    pub bands_db: [f32; 5],
    pub peak_protection: bool,
}
impl Default for EqualizerSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            preamp_db: 0.0,
            bands_db: [0.0; 5],
            peak_protection: true,
        }
    }
}
impl EqualizerSettings {
    pub fn validate(&self) -> Result<(), PlayerError> {
        if !self.preamp_db.is_finite()
            || !(-24.0..=6.0).contains(&self.preamp_db)
            || self
                .bands_db
                .iter()
                .any(|v| !v.is_finite() || !(-12.0..=12.0).contains(v))
        {
            return Err(PlayerError::invalid(
                "EQ requires preamp −24…+6 dB and five bands −12…+12 dB",
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EqualizerPreset {
    pub name: String,
    pub equalizer: EqualizerSettings,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoundSettings {
    pub version: u32,
    pub equalizer: EqualizerSettings,
    pub presets: Vec<EqualizerPreset>,
}
impl Default for SoundSettings {
    fn default() -> Self {
        Self {
            version: 1,
            equalizer: EqualizerSettings::default(),
            presets: vec![],
        }
    }
}
impl SoundSettings {
    pub fn validate(&self) -> Result<(), PlayerError> {
        self.equalizer.validate()?;
        if self.version != 1 || self.presets.len() > 16 {
            return Err(PlayerError::invalid(
                "Unsupported sound preferences or more than 16 presets",
            ));
        }
        let mut names = std::collections::HashSet::new();
        for preset in &self.presets {
            if preset.name.trim().is_empty()
                || preset.name.len() > 80
                || preset.name.chars().any(char::is_control)
                || !names.insert(&preset.name)
            {
                return Err(PlayerError::invalid(
                    "Preset names must be unique and contain 1…80 UTF-8 bytes",
                ));
            }
            preset.equalizer.validate()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_nonfinite_out_of_range_duplicate_and_unbounded_preferences() {
        let mut settings = SoundSettings::default();
        for invalid in [f32::NAN, f32::INFINITY, -12.01, 12.01] {
            settings.equalizer.bands_db[0] = invalid;
            assert!(settings.validate().is_err());
        }
        settings = SoundSettings::default();
        settings.equalizer.preamp_db = -24.1;
        assert!(settings.validate().is_err());
        settings = SoundSettings::default();
        settings.presets = vec![
            EqualizerPreset {
                name: "Warm".into(),
                equalizer: settings.equalizer
            };
            2
        ];
        assert!(settings.validate().is_err());
        settings.presets = vec![EqualizerPreset {
            name: "x".repeat(81),
            equalizer: settings.equalizer,
        }];
        assert!(settings.validate().is_err());
        settings.presets.clear();
        settings.version = 2;
        assert!(settings.validate().is_err());
    }
}
