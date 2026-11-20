//! Explicit real-output analysis check using only our original quiet segmented fixture.
use audio_engine::{AudioEngine, AudioSnapshot};
use player_domain::PlaybackState;
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
        let s = engine.snapshot();
        assert!(s.error.is_none(), "{label}: {s:?}");
        if check(&s) {
            return s;
        }
        assert!(Instant::now() < until, "{label}: {s:?}");
        thread::sleep(Duration::from_millis(5));
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = PathBuf::from(std::env::args().nth(1).expect("fixture WAV"));
    let engine = AudioEngine::new()?;
    engine.set_volume(0.25)?;
    engine.load(path.clone());
    let initial = wait(&engine, "1kHz output", |s| {
        s.analysis
            .is_some_and(|f| f.is_valid() && (f.features.spectral_centroid_hz - 1000.0).abs() < 3.0)
    });
    let first = initial.analysis.unwrap();
    assert!((first.features.rms - 0.04 * 0.25 / 2_f32.sqrt()).abs() < 0.0005);
    assert!(first.window_end_frame * 1000 / u64::from(first.sample_rate) <= initial.position_ms);
    assert!(initial.statistics.nonzero_frames > 0);
    let mut observations = 0;
    let mut previous = 0;
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        if let Some(f) = engine.analysis() {
            assert!(f.is_valid());
            if f.window_end_frame != previous {
                observations += 1;
                previous = f.window_end_frame;
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
    assert!(
        (35..=60).contains(&observations),
        "Analysis cadence: {observations}"
    );
    engine.set_volume(0.5)?;
    wait(&engine, "post-volume RMS", |s| {
        s.analysis
            .is_some_and(|f| (f.features.rms - first.features.rms * 2.0).abs() < 0.0005)
    });
    engine.pause();
    assert!(engine.analysis().is_none());
    let paused = engine.snapshot().position_ms;
    thread::sleep(Duration::from_millis(160));
    assert_eq!(engine.snapshot().position_ms, paused);
    assert!(engine.analysis().is_none());
    engine.seek(4500);
    wait(&engine, "paused seek", |s| {
        s.state == PlaybackState::Paused && s.position_ms == 4500
    });
    assert!(engine.analysis().is_none());
    engine.resume();
    let silence = wait(&engine, "seek into silence", |s| {
        s.analysis
            .is_some_and(|f| f.features.playback_position_ms >= 4600 && f.features.rms < 1e-6)
    });
    assert_ne!(silence.analysis.unwrap().generation, first.generation);
    engine.seek(8500);
    let bass = wait(&engine, "120Hz band", |s| {
        s.analysis.is_some_and(|f| {
            f.features.playback_position_ms >= 8600
                && (f.features.spectral_centroid_hz - 120.0).abs() < 3.0
                && f.features.bass > 0.01
        })
    });
    engine.seek(12500);
    let high = wait(&engine, "4kHz band", |s| {
        s.analysis.is_some_and(|f| {
            f.features.playback_position_ms >= 12600
                && (f.features.spectral_centroid_hz - 4000.0).abs() < 3.0
                && f.features.presence > 0.01
        })
    });
    engine.set_volume(0.0)?;
    wait(&engine, "mute", |s| {
        s.analysis.is_some_and(|f| f.features.rms == 0.0)
    });
    engine.set_volume(0.25)?;
    engine.seek(15700);
    wait(&engine, "EOF", |s| s.state == PlaybackState::Stopped);
    assert!(engine.analysis().is_none());
    engine.stop();
    assert!(engine.analysis().is_none());
    engine.load(path.with_file_name("missing-analysis.wav"));
    let until = Instant::now() + Duration::from_secs(3);
    while engine.snapshot().error.is_none() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(engine.analysis().is_none());
    println!(
        "Analysis hardware: device={:?}, output={}Hz, source=44100Hz stereo, {} observations/s, FFT={}, hop={}, RMS={:.6}, centroid={:.2}Hz, drops={}; {:?}",
        initial.device_name,
        initial.output_rate,
        observations,
        first.window_frames,
        first.hop_frames,
        first.features.rms,
        first.features.spectral_centroid_hz,
        high.analysis.unwrap().dropped_frames,
        high.statistics
    );
    println!(
        "Confirmed post-gain RMS, output sample timestamps, Pause/Resume/seek generation, silence, 120Hz bass={:.6}, 4kHz presence={:.6}, mute, stop/EOF/error cleanup",
        bass.analysis.unwrap().features.bass,
        high.analysis.unwrap().features.presence
    );
    drop(engine);
    Ok(())
}
