//! Explicit real-output regression for original short codec fixtures; no external music.
use audio_engine::AudioEngine;
use player_domain::PlaybackState;
use std::{
    thread,
    time::{Duration, Instant},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let engine = AudioEngine::new()?;
    engine.set_volume(0.3)?;
    for path in std::env::args().skip(1) {
        let before = engine.snapshot().statistics;
        engine.load(path.clone().into());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state = engine.snapshot();
            assert!(state.error.is_none(), "{path}: {:?}", state.error);
            if state.state == PlaybackState::Stopped && state.position_ms > 0 {
                assert!(state.statistics.nonzero_frames > before.nonzero_frames);
                println!(
                    "codec output: {path}: device={:?} rate={} duration={}ms nonzero_frames={}",
                    state.device_name,
                    state.output_rate,
                    state.position_ms,
                    state.statistics.nonzero_frames - before.nonzero_frames
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Output timed out: {path}: {state:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    engine.stop();
    Ok(())
}
