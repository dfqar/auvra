//! Decode-worker DSP only. RBJ peaking filters: https://www.w3.org/TR/audio-eq-cookbook/
//! Two fixed banks crossfade over 20 ms; coefficient changes never touch the callback.
use crate::{AudioProcessor, PcmBlock};
use player_domain::{EQ_FREQUENCIES, EqualizerSettings};

#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [[f64; 2]; 2],
}
impl Biquad {
    fn peaking(rate: u32, frequency: f64, gain: f32) -> Self {
        // Unsupported device bands are bypassed rather than moved into audible
        // frequencies (e.g. 12 kHz on an 8 kHz hands-free output).
        if frequency >= f64::from(rate) * 0.45 {
            return Self {
                b: [1.0, 0.0, 0.0],
                ..Default::default()
            };
        }
        let a = 10_f64.powf(f64::from(gain) / 40.0);
        let w = 2.0 * std::f64::consts::PI * frequency / f64::from(rate);
        let alpha = w.sin() / (2.0 * 0.85);
        let a0 = 1.0 + alpha / a;
        Self {
            b: [
                (1.0 + alpha * a) / a0,
                -2.0 * w.cos() / a0,
                (1.0 - alpha * a) / a0,
            ],
            a: [-2.0 * w.cos() / a0, (1.0 - alpha / a) / a0],
            z: [[0.0; 2]; 2],
        }
    }
    fn sample(&mut self, x: f64, channel: usize) -> f64 {
        let z = &mut self.z[channel];
        let y = self.b[0] * x + z[0];
        z[0] = self.b[1] * x - self.a[0] * y + z[1];
        z[1] = self.b[2] * x - self.a[1] * y;
        // Avoid denormal tails on long silence; no effect at audible levels.
        for v in z {
            if v.abs() < 1e-24 {
                *v = 0.0;
            }
        }
        y
    }
}
struct Bank {
    settings: EqualizerSettings,
    filters: [Biquad; 5],
    preamp: f64,
}
impl Bank {
    fn new(settings: EqualizerSettings, rate: u32) -> Self {
        Self {
            settings,
            filters: std::array::from_fn(|i| {
                Biquad::peaking(rate, EQ_FREQUENCIES[i], settings.bands_db[i])
            }),
            preamp: 10_f64.powf(f64::from(settings.preamp_db) / 20.0),
        }
    }
    fn frame(&mut self, frame: [f32; 2]) -> [f32; 2] {
        if !self.settings.enabled {
            return frame;
        }
        std::array::from_fn(|channel| {
            let mut y = f64::from(frame[channel]) * self.preamp;
            for filter in &mut self.filters {
                y = filter.sample(y, channel);
            }
            if !y.is_finite() {
                return 0.0;
            }
            // Continuous value and slope at −0.92 dBFS. This is a soft peak protector,
            // not a lookahead limiter; it is only active with EQ enabled.
            if self.settings.peak_protection && y.abs() > 0.9 {
                y = y.signum() * (0.9 + 0.1 * (1.0 - (-(y.abs() - 0.9) / 0.1).exp()));
            }
            y.clamp(-1.0, 1.0) as f32
        })
    }
}
pub(crate) struct Equalizer {
    rate: u32,
    current: Bank,
    next: Option<Bank>,
    target: EqualizerSettings,
    remaining: u32,
    transition: u32,
}
impl Equalizer {
    pub fn new(rate: u32, settings: EqualizerSettings) -> Self {
        Self {
            rate,
            current: Bank::new(settings, rate),
            next: None,
            target: settings,
            remaining: 0,
            transition: (rate / 50).max(1),
        }
    }
    pub fn update(&mut self, settings: EqualizerSettings) {
        self.target = settings;
    }
    pub fn reset(&mut self) {
        *self = Self::new(self.rate, self.target);
    }
    fn frame(&mut self, input: [f32; 2]) -> [f32; 2] {
        if self.next.is_none() && self.current.settings != self.target {
            self.next = Some(Bank::new(self.target, self.rate));
            self.remaining = self.transition;
        }
        let old = self.current.frame(input);
        let Some(next) = &mut self.next else {
            return old;
        };
        let new = next.frame(input);
        let t = 1.0 - self.remaining as f32 / self.transition as f32;
        let output = std::array::from_fn(|i| old[i] + (new[i] - old[i]) * t);
        self.remaining -= 1;
        if self.remaining == 0 {
            self.current = self.next.take().unwrap();
        }
        output
    }
}
impl AudioProcessor for Equalizer {
    fn process(&mut self, block: &mut PcmBlock) {
        debug_assert_eq!(block.format.channels, 2);
        debug_assert_eq!(block.format.sample_rate, self.rate);
        for frame in block.samples.as_chunks_mut::<2>().0 {
            *frame = self.frame(*frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> EqualizerSettings {
        EqualizerSettings {
            enabled: true,
            peak_protection: false,
            ..Default::default()
        }
    }
    #[test]
    fn bypass_is_exact_and_flat_is_unity_with_isolated_stereo() {
        for rate in [8000, 44100, 48000, 96000, 192000] {
            for config in [EqualizerSettings::default(), settings()] {
                let mut eq = Equalizer::new(rate, config);
                for i in 0..rate {
                    let x = (i as f32 * 0.13).sin() * 0.1;
                    let y = eq.frame([x, 0.0]);
                    assert!((y[0] - x).abs() < 1e-6);
                    assert_eq!(y[1], 0.0);
                }
            }
        }
    }
    #[test]
    fn bands_match_requested_gain_at_center_across_device_rates() {
        for rate in [44100, 48000, 96000] {
            for (band, frequency) in EQ_FREQUENCIES.into_iter().enumerate() {
                for gain in [-12.0, 6.0, 12.0] {
                    let mut config = settings();
                    config.bands_db[band] = gain;
                    let mut eq = Equalizer::new(rate, config);
                    let mut input = 0.0;
                    let mut output = 0.0;
                    for i in 0..rate {
                        let x = (2.0 * std::f64::consts::PI * frequency * f64::from(i)
                            / f64::from(rate))
                        .sin() as f32
                            * 0.01;
                        let y = eq.frame([x, x]);
                        assert_eq!(y[0], y[1]);
                        if i > rate / 2 {
                            input += f64::from(x).powi(2);
                            output += f64::from(y[0]).powi(2);
                        }
                    }
                    assert!((10.0 * (output / input).log10() - f64::from(gain)).abs() < 0.02);
                }
            }
        }
    }
    #[test]
    fn extreme_updates_are_bounded_settle_and_reset_removes_tails() {
        for rate in [8000, 44100, 48000, 192000] {
            let mut eq = Equalizer::new(rate, EqualizerSettings::default());
            for i in 0..rate {
                if i % 100 == 0 {
                    eq.update(EqualizerSettings {
                        enabled: i % 300 != 0,
                        preamp_db: 6.0,
                        bands_db: [if i % 200 == 0 { 12.0 } else { -12.0 }; 5],
                        peak_protection: true,
                    });
                }
                let y = eq.frame([0.9, -0.9]);
                assert!(y.iter().all(|v| v.is_finite() && v.abs() <= 1.0));
            }
            eq.update(EqualizerSettings::default());
            for _ in 0..rate / 10 {
                eq.frame([0.0; 2]);
            }
            assert_eq!(eq.frame([0.2, -0.3]), [0.2, -0.3]);
            eq.update(settings());
            eq.reset();
            assert_eq!(eq.frame([0.0; 2]), [0.0; 2]);
        }
    }
    #[test]
    fn preamp_and_peak_protection_have_expected_response() {
        let mut config = settings();
        config.preamp_db = -6.0;
        let mut eq = Equalizer::new(48000, config);
        assert!((eq.frame([0.2; 2])[0] - 0.10023745).abs() < 1e-6);
        config.preamp_db = 6.0;
        config.peak_protection = true;
        let mut eq = Equalizer::new(48000, config);
        let y = eq.frame([0.8, -0.8]);
        assert!(y[0] > 0.99 && y[0] <= 1.0 && y[1] == -y[0]);
    }
    #[test]
    fn bands_above_device_bandwidth_never_shift_into_an_audible_band() {
        let mut config = settings();
        config.bands_db = [0.0, 0.0, 0.0, 12.0, 12.0];
        let mut eq = Equalizer::new(8000, config);
        for i in 0..8000 {
            let x = (i as f32 * 0.13).sin() * 0.1;
            assert!((eq.frame([x, x])[0] - x).abs() < 1e-6);
        }
    }
}
