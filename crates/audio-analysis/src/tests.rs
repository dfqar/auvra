use super::*;
use std::f32::consts::TAU;
fn collect(rate: u32, seconds: f32, signal: impl Fn(usize) -> [f32; 2]) -> Vec<AnalysisFrame> {
    let mut analyzer = Analyzer::new(rate).unwrap();
    (0..(rate as f32 * seconds) as usize)
        .filter_map(|i| analyzer.push(signal(i), i as u64 + 1, 1))
        .collect()
}
fn tone(rate: u32, hz: f32, amplitude: f32, phase_flip: bool) -> Vec<AnalysisFrame> {
    collect(rate, 0.5, |i| {
        let v = (TAU * hz * i as f32 / rate as f32).sin() * amplitude;
        [v, if phase_flip { -v } else { v }]
    })
}
#[test]
fn calibrated_tones_at_all_output_rates_preserve_energy_centroid_and_cadence() {
    for rate in [8000, 22050, 44100, 48000, 96000, 192000, 384000] {
        let frames = tone(rate, 1000.0, 0.5, false);
        let last = frames.last().unwrap();
        assert!(last.is_valid(), "{last:?}");
        assert!(
            (last.features.rms - 0.5 / 2_f32.sqrt()).abs() < 0.002,
            "{rate}: {last:?}"
        );
        assert!((last.features.mids - last.features.rms).abs() < 0.003);
        assert!((last.features.spectral_centroid_hz - 1000.0).abs() < 2.0);
        assert!(last.features.bass < 0.001 && last.features.highs < 0.001);
        assert!(
            (18..=23).contains(&frames.len()),
            "{rate}: {}",
            frames.len()
        );
        for pair in frames.windows(2) {
            assert_eq!(
                pair[1].window_end_frame - pair[0].window_end_frame,
                u64::from(rate / 50)
            );
            assert!(pair[1].features.playback_position_ms > pair[0].features.playback_position_ms);
        }
    }
}
#[test]
fn six_bands_and_log_spectrum_locate_original_tones_without_antiphase_cancellation() {
    for (band, hz) in [40.0, 120.0, 350.0, 1000.0, 4000.0, 10000.0]
        .into_iter()
        .enumerate()
    {
        let normal = tone(48000, hz, 0.4, false);
        let antiphase = tone(48000, hz, 0.4, true);
        let last = normal.last().unwrap();
        assert_eq!(last, antiphase.last().unwrap());
        let f = last.features;
        let bands = [f.sub_bass, f.bass, f.low_mids, f.mids, f.presence, f.highs];
        assert_eq!(
            bands
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.total_cmp(b.1))
                .unwrap()
                .0,
            band,
            "{hz}: {bands:?}"
        );
        let dominant = last
            .spectrum
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        let expected = ((hz / 20.0).ln() / 1000_f32.ln() * SPECTRUM_BANDS as f32) as usize;
        assert!(
            dominant.abs_diff(expected) <= 1,
            "{hz}: spectrum {dominant}, expected {expected}"
        );
    }
}
#[test]
fn silence_and_transient_burst_are_finite_and_decay() {
    let silence = collect(48000, 0.3, |_| [0.0; 2]);
    for frame in silence {
        assert!(frame.is_valid());
        assert_eq!(
            frame.features,
            AudioFeatures {
                playback_position_ms: frame.features.playback_position_ms,
                ..Default::default()
            }
        );
        assert_eq!(frame.spectrum, [0.0; SPECTRUM_BANDS]);
    }
    let burst = collect(48000, 0.8, |i| {
        if (12000..18000).contains(&i) {
            [(TAU * 1000.0 * i as f32 / 48000.0).sin() * 0.7; 2]
        } else {
            [0.0; 2]
        }
    });
    assert!(burst.iter().any(|f| f.features.transient > 0.8));
    assert!(burst.last().unwrap().features.transient < 0.05);
    assert_eq!(burst.last().unwrap().features.rms, 0.0);
}
#[test]
fn stereo_power_is_averaged_and_master_gain_is_linear() {
    let left = collect(48000, 0.3, |i| {
        [(TAU * 1000.0 * i as f32 / 48000.0).sin() * 0.5, 0.0]
    });
    let f = left.last().unwrap().features;
    assert!((f.rms - 0.25).abs() < 0.002);
    assert!((f.mids - 0.25).abs() < 0.002);
    let quiet = tone(48000, 1000.0, 0.05, false);
    assert!(
        (quiet.last().unwrap().features.mids * 10.0
            - tone(48000, 1000.0, 0.5, false)
                .last()
                .unwrap()
                .features
                .mids)
            .abs()
            < 0.001
    );
}
#[test]
fn epochs_gaps_seeks_and_invalid_samples_require_a_new_full_window() {
    let mut a = Analyzer::new(48000).unwrap();
    let n = a.window_frames();
    for i in 1..=n {
        let f = a.push([0.1; 2], i as u64, 1);
        assert_eq!(f.is_some(), i == n);
    }
    assert!(a.push([0.1; 2], n as u64 + 1, 2).is_none());
    for i in 2..n {
        assert!(a.push([0.1; 2], n as u64 + i as u64, 2).is_none());
    }
    assert!(a.push([0.1; 2], (n * 2) as u64, 2).is_some());
    assert!(a.push([0.1; 2], 300000, 2).is_none());
    assert!(a.push([f32::NAN; 2], 300001, 2).is_none());
    for i in 1..n {
        assert!(a.push([0.0; 2], i as u64, 3).is_none());
    }
    let frame = a.push([0.0; 2], n as u64, 3).unwrap();
    assert_eq!(frame.generation, 3);
    assert_eq!(frame.window_start_frame, 0);
    assert_eq!(frame.features.rms, 0.0);
    assert!(Analyzer::new(0).is_none());
    assert!(Analyzer::new(768000).is_none());
}
