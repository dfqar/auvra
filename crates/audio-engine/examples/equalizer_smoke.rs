//! Original quiet fixture only; actual post-EQ/CoreAudio analysis, never captures device audio.
use audio_engine::{AudioEngine, AudioSnapshot};
use player_domain::{EqualizerSettings, PlaybackState};
use std::{
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
fn wait(
    engine: &AudioEngine,
    label: &str,
    check: impl Fn(&AudioSnapshot) -> bool,
) -> AudioSnapshot {
    let until = Instant::now() + Duration::from_secs(8);
    loop {
        let snapshot = engine.snapshot();
        assert!(snapshot.error.is_none(), "{label}: {snapshot:?}");
        if check(&snapshot) {
            return snapshot;
        }
        assert!(Instant::now() < until, "{label}: {snapshot:?}");
        thread::sleep(Duration::from_millis(5));
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(std::env::args().nth(1).expect("original analysis fixture"));
    let engine = AudioEngine::new()?;
    engine.set_volume(0.25)?;
    engine.load(path);
    let initial = wait(&engine, "bypass", |s| {
        s.analysis.is_some_and(|f| {
            f.features.playback_position_ms >= 180
                && (f.features.rms - 0.04 * 0.25 / 2_f32.sqrt()).abs() < 0.00005
                && (f.features.spectral_centroid_hz - 1000.0).abs() < 3.0
        })
    });
    let rms = initial.analysis.unwrap().features.rms;
    let mut eq = EqualizerSettings {
        enabled: true,
        ..Default::default()
    };
    eq.bands_db[2] = 6.0;
    engine.set_equalizer(eq)?;
    let boosted = wait(&engine, "post-EQ +6 dB", |s| {
        s.analysis
            .is_some_and(|f| (f.features.rms / rms - 1.99526).abs() < 0.03)
    });
    eq.preamp_db = -6.0;
    engine.set_equalizer(eq)?;
    wait(&engine, "preamp cancels boost", |s| {
        s.analysis
            .is_some_and(|f| (f.features.rms / rms - 1.0).abs() < 0.02)
    });
    eq.enabled = false;
    engine.set_equalizer(eq)?;
    wait(&engine, "bypass restored", |s| {
        s.analysis
            .is_some_and(|f| (f.features.rms / rms - 1.0).abs() < 0.02)
    });
    engine.pause();
    engine.seek(8500);
    wait(&engine, "paused seek resets filter tails", |s| {
        s.state == PlaybackState::Paused && s.position_ms == 8500
    });
    eq.enabled = true;
    eq.preamp_db = -3.0;
    eq.bands_db = [6.0, 0.0, 0.0, 0.0, 0.0];
    engine.set_equalizer(eq)?;
    engine.resume();
    wait(&engine, "new epoch with bass EQ", |s| {
        s.analysis.is_some_and(|f| {
            f.generation != initial.analysis.unwrap().generation
                && (f.features.spectral_centroid_hz - 120.0).abs() < 3.0
        })
    });
    let stats = engine.output_statistics();
    assert!(stats.media_frames > 0 && stats.nonzero_frames > 0);
    assert_eq!(stats.analysis_dropped_frames, 0);
    assert_eq!(stats.underrun_frames, 0);
    println!(
        "EQ CoreAudio passed: device={:?}, rate={}, baseline RMS={rms:.7}, +6 dB ratio={:.5}, statistics={stats:?}",
        initial.device_name,
        initial.output_rate,
        boosted.analysis.unwrap().features.rms / rms
    );
    engine.stop();
    Ok(())
}
