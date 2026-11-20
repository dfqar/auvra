//! Opt-in worker timing. Only event names and sample positions leave the relay.
use std::{
    sync::OnceLock,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("AUVRA_AUDIO_TRACE").is_some_and(|value| value == "1"))
}

pub(super) fn event(stage: &str, position: Option<u64>) {
    if enabled()
        && let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH)
    {
        eprintln!(
            "auvra_audio time_ms={} stage={stage} frame={}",
            now.as_millis(),
            position.unwrap_or_default()
        );
    }
}

pub(super) fn measure<T>(started: &str, completed: &str, operation: impl FnOnce() -> T) -> T {
    event(started, None);
    let result = operation();
    event(completed, None);
    result
}

pub(super) fn signal(samples: [f32; 2]) -> bool {
    samples.iter().any(|sample| sample.abs() >= 0.0001)
}

pub(super) fn handoff_admission(
    generation: u64,
    transition: &str,
    status: player_domain::HostSpatialStatus,
    capture_enabled: bool,
    playing: bool,
    armed: bool,
) {
    if enabled()
        && let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH)
    {
        eprintln!(
            "auvra_audio time_ms={} stage=handoff_admission generation={generation} transition={transition} status={status:?} capture_enabled={capture_enabled} playing={playing} armed={armed}",
            now.as_millis()
        );
    }
}

pub(super) fn capture_teardown(generation: u64, elapsed: Duration) {
    if enabled()
        && let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH)
    {
        eprintln!(
            "auvra_audio time_ms={} stage=capture_teardown_done generation={generation} elapsed_us={}",
            now.as_millis(),
            elapsed.as_micros()
        );
    }
}

pub(super) fn handoff_scope(
    stage: &str,
    generation: u64,
    successor: bool,
    warming: bool,
    armed: bool,
) {
    if enabled()
        && let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH)
    {
        eprintln!(
            "auvra_audio time_ms={} stage={stage} generation={generation} successor={successor} warming={warming} armed={armed}",
            now.as_millis()
        );
    }
}

pub(super) fn starting_schedule(rate: usize, lead: usize) {
    if enabled()
        && let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH)
    {
        eprintln!(
            "auvra_audio time_ms={} stage=renderer_start_schedule rate_hz={rate} lead_frames={lead}",
            now.as_millis()
        );
    }
}

pub(super) struct HandoffFinalObservation {
    pub failed: bool,
    pub flushed: bool,
    pub format_valid: bool,
    pub authority_before: Option<bool>,
    pub authority_after: Option<bool>,
    pub through: Option<u64>,
    pub origin: Option<u64>,
    pub silence_end: Option<u64>,
    pub lead: usize,
    pub rate: usize,
    pub elapsed: Option<Duration>,
    pub ready: bool,
}

pub(super) fn handoff_final(observation: HandoffFinalObservation) {
    if enabled()
        && let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH)
    {
        let HandoffFinalObservation {
            failed,
            flushed,
            format_valid,
            authority_before,
            authority_after,
            through,
            origin,
            silence_end,
            lead,
            rate,
            elapsed,
            ready,
        } = observation;
        eprintln!(
            "auvra_audio time_ms={} stage=handoff_final_observation failed={failed} flushed={flushed} format_valid={format_valid} authority_before={authority_before:?} authority_after={authority_after:?} through={through:?} origin={origin:?} silence_end={silence_end:?} lead_frames={lead} rate_hz={rate} elapsed_us={:?} ready={ready}",
            now.as_millis(),
            elapsed.map(|elapsed| elapsed.as_micros())
        );
    }
}
