//! Opt-in timing and origin diagnostics for the authenticated owned queue.
use super::autoplay::Autoplay;
use player_domain::*;
use std::{
    io::Write,
    sync::OnceLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const HEAD_FRESHNESS: Duration = Duration::from_millis(1_200);
const TRANSITION_LIMIT: Duration = Duration::from_secs(6);

pub(super) fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("AUVRA_AUDIO_TRACE").is_some_and(|value| value == "1"))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    Offered,
    Acknowledged,
    NextSpotify,
    NextAuvra,
    NextUnknown,
}

pub(super) enum DiscoveryStage {
    Scheduled { explicit: bool },
    RetryScheduled { delay_ms: u128 },
    Dispatched,
    Received { status: u16, count: usize },
    Merged { count: usize },
    PlaybackReadDispatched { scoped: bool },
    PlaybackReadReceived { status: u16, has_frame: bool },
    ResumeAlbumProbe(ResumeAlbumProbe),
    ResumeAlbumCacheHit,
    ContextResumeProjected { operation: u64, generation: u64 },
    ResumeCancelledBeforeDispatch { operation: u64 },
    ResumePutClaimed { operation: u64 },
    ResumePauseNative { operation: u64, generation: u64 },
}

pub(super) struct ResumeAlbumProbe {
    pub probe: &'static str,
    pub duration_ms: u128,
    pub timeout_ms: u128,
    pub status: u16,
    pub result: &'static str,
    pub rows: usize,
}

impl DiscoveryStage {
    fn write(self, output: &mut impl Write, time_ms: u128) {
        let message = match self {
            Self::Scheduled { explicit } => format!("devices_scheduled explicit={explicit}"),
            Self::RetryScheduled { delay_ms } => {
                format!("devices_retry_scheduled delay_ms={delay_ms}")
            }
            Self::Dispatched => "devices_dispatched".into(),
            Self::Received { status, count } => {
                format!("devices_received status={status} count={count}")
            }
            Self::Merged { count } => format!("devices_merged count={count}"),
            Self::PlaybackReadDispatched { scoped } => {
                format!("playback_read_dispatched scoped={scoped}")
            }
            Self::PlaybackReadReceived { status, has_frame } => {
                format!("playback_read_received status={status} has_frame={has_frame}")
            }
            Self::ResumeAlbumProbe(ResumeAlbumProbe {
                probe,
                duration_ms,
                timeout_ms,
                status,
                result,
                rows,
            }) => {
                format!(
                    "resume_album_probe probe={probe} duration_ms={duration_ms} timeout_ms={timeout_ms} status={status} result={result} rows={rows}"
                )
            }
            Self::ResumeAlbumCacheHit => "resume_album_cache result=member".into(),
            Self::ContextResumeProjected {
                operation,
                generation,
            } => {
                format!(
                    "resume_native_proof_projected operation={operation} generation={generation}"
                )
            }
            Self::ResumeCancelledBeforeDispatch { operation } => {
                format!("resume_cancelled_before_dispatch operation={operation}")
            }
            Self::ResumePutClaimed { operation } => {
                format!("resume_put_claimed operation={operation}")
            }
            Self::ResumePauseNative {
                operation,
                generation,
            } => {
                format!("resume_pause_native operation={operation} generation={generation}")
            }
        };
        let _ = writeln!(output, "auvra_spotify time_ms={time_ms} stage={message}");
    }
}

pub(super) struct Event {
    time_ms: u128,
    kind: EventKind,
}
enum EventKind {
    Autoplay(Stage),
    Discovery(DiscoveryStage),
    Transport {
        stage: super::transport_trace::Stage,
        kind: &'static str,
        verified_target: bool,
    },
}
impl Event {
    fn new(kind: EventKind) -> Option<Self> {
        Some(Self {
            time_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()?
                .as_millis(),
            kind,
        })
    }

    pub(super) fn autoplay(stage: Stage) -> Option<Self> {
        Self::new(EventKind::Autoplay(stage))
    }

    pub(super) fn discovery(stage: DiscoveryStage) -> Option<Self> {
        if !enabled() {
            return None;
        }
        Self::new(EventKind::Discovery(stage))
    }

    pub(super) fn transport(
        control: &RemoteControl,
        stage: super::transport_trace::Stage,
        verified_target: bool,
    ) -> Option<Self> {
        if !enabled() {
            return None;
        }
        let kind = super::transport_trace::kind(control)?;
        Self::new(EventKind::Transport {
            stage,
            kind,
            verified_target,
        })
    }

    pub(super) fn write(self) {
        let mut output = std::io::stderr().lock();
        let stage = match self.kind {
            EventKind::Autoplay(stage) => stage,
            EventKind::Discovery(stage) => {
                stage.write(&mut output, self.time_ms);
                return;
            }
            EventKind::Transport {
                stage,
                kind,
                verified_target,
            } => {
                stage.write(&mut output, self.time_ms, kind, verified_target);
                return;
            }
        };
        let stage = match stage {
            Stage::Offered => "autoplay_candidate_offered",
            Stage::Acknowledged => "autoplay_candidate_acknowledged",
            Stage::NextSpotify => "autoplay_next_spotify",
            Stage::NextAuvra => "autoplay_next_auvra",
            Stage::NextUnknown => "autoplay_next_unknown",
        };
        let _ = writeln!(
            output,
            "auvra_spotify time_ms={} stage={stage}",
            self.time_ms
        );
    }
}

#[derive(Default)]
pub(super) struct Trace {
    current: Option<Observed>,
    explicit_selection: bool,
}

struct Observed {
    generation: u64,
    device: String,
    sequence: u64,
    epoch: Option<u64>,
    media: MediaId,
    pending: Option<(Stage, Instant)>,
}
impl Observed {
    fn same_output(&self, clock: &RemoteOutputClock) -> bool {
        self.generation == clock.generation && self.device == clock.device_id
    }
}

impl Trace {
    pub(super) fn clear(&mut self) {
        self.current = None;
        self.explicit_selection = false;
    }

    pub(super) fn control(&mut self, control: &RemoteControl) {
        if matches!(
            control,
            RemoteControl::Start { .. }
                | RemoteControl::StartOnDevice { .. }
                | RemoteControl::StartSpokenOnDevice { .. }
                | RemoteControl::ResumeOnDevice { .. }
                | RemoteControl::StartContext { .. }
                | RemoteControl::Transfer { .. }
                | RemoteControl::Stop
        ) {
            self.explicit_selection = self.current.is_some();
        }
    }

    // The old queue is invalidated when the new media is ingested. Read its
    // unmodified head here, before the UI relabels accepted Auvra suggestions.
    pub(super) fn origin(
        &self,
        next: Option<&RemoteOutputClock>,
        queue: Option<(&RemoteOutputQueue, Duration)>,
        autoplay: &Autoplay,
    ) -> Option<Stage> {
        let previous = self.current.as_ref()?;
        let next = next?;
        let media = next.media_id.as_ref()?;
        let (queue, age) = queue?;
        if self.explicit_selection
            || !next.active
            || !previous.same_output(next)
            || next.sequence <= previous.sequence
            || &previous.media == media
            || age > HEAD_FRESHNESS
            || queue.generation != previous.generation
            || queue.device_id != previous.device
            || queue.media_id != previous.media
            || Some(queue.epoch) != previous.epoch
        {
            return None;
        }
        let (head, rest) = queue.upcoming.split_first()?;
        if &head.media_id != media || rest.iter().any(|row| &row.media_id == media) {
            return None;
        }
        match (head.source, autoplay.is_offered(media)) {
            (Some(QueueEntrySource::Autoplay), false) => Some(Stage::NextSpotify),
            (Some(QueueEntrySource::Queue), true) => Some(Stage::NextAuvra),
            _ => None,
        }
    }

    pub(super) fn observe(
        &mut self,
        clock: Option<&RemoteOutputClock>,
        origin: Option<Stage>,
        now: Instant,
    ) -> Option<Stage> {
        let Some((clock, media)) = clock
            .filter(|clock| clock.active)
            .and_then(|clock| clock.media_id.as_ref().map(|media| (clock, media)))
        else {
            self.clear();
            return None;
        };
        let Some(previous) = self.current.as_mut().filter(|old| old.same_output(clock)) else {
            self.explicit_selection = false;
            self.current = Some(Observed {
                generation: clock.generation,
                device: clock.device_id.clone(),
                sequence: clock.sequence,
                epoch: clock.queue_epoch,
                media: media.clone(),
                pending: None,
            });
            return None;
        };
        if clock.sequence <= previous.sequence {
            return None;
        }
        previous.sequence = clock.sequence;
        previous.epoch = clock.queue_epoch;
        if &previous.media != media {
            self.explicit_selection = false;
            previous.media = media.clone();
            previous.pending = Some((origin.unwrap_or(Stage::NextUnknown), now));
        }
        if clock.state == PlaybackState::Playing
            && clock.rate > 0.0
            && let Some((origin, began)) = previous.pending.take()
        {
            let unambiguous = !std::mem::take(&mut self.explicit_selection)
                && now.saturating_duration_since(began) <= TRANSITION_LIMIT;
            return Some(if unambiguous {
                origin
            } else {
                Stage::NextUnknown
            });
        }
        None
    }
}
