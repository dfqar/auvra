//! Opt-in transport routing diagnostics. Only fixed labels and booleans leave the provider.
use player_domain::RemoteControl;

pub(super) fn kind(control: &RemoteControl) -> Option<&'static str> {
    Some(match control {
        RemoteControl::Next => "next",
        RemoteControl::Previous => "previous",
        RemoteControl::Play => "play",
        RemoteControl::Pause => "pause",
        RemoteControl::PauseOnDevice { .. } => "pause_targeted",
        RemoteControl::Seek { .. } => "seek",
        RemoteControl::StartOnDevice { .. } => "start_targeted",
        RemoteControl::StartContext { item: Some(_), .. } => "start_context_row",
        RemoteControl::StartContext { .. } => "start_context",
        RemoteControl::ResumeOnDevice { .. } => "resume_targeted",
        RemoteControl::Transfer { .. } => "transfer",
        _ => return None,
    })
}

#[derive(Clone, Copy)]
pub(super) enum Route {
    Owned,
    Http,
    Rejected,
}

#[derive(Clone, Copy)]
pub(super) enum Reason {
    FreshClock,
    AwaitingHttp,
    ExactPrevious,
    NoClock,
    NoActions,
    NotLocal,
    OwnedRejected,
    StaleOwnedClock,
    RateLimited,
    Backoff,
    OwnedBusy,
    QueueFull,
}

#[derive(Clone, Copy)]
pub(super) enum Stage {
    Route { route: Route, reason: Reason },
    HttpDispatch,
    HttpAccepted,
    HttpFailed(u16),
    HttpReadDeferred,
    ResumeTarget(&'static str),
    PreviousObservedAfterTimeout,
    OwnedPreviousSuccessorPause,
    OwnedPreviousSuccessorAlreadyPaused,
    OwnedPreviousSuccessorPlay,
    OwnedPreviousFromStart,
    OwnedNextAfterPrevious,
    OwnedNextSuccessorPause,
}

impl Stage {
    pub(super) fn write(
        self,
        output: &mut impl std::io::Write,
        time_ms: u128,
        kind: &str,
        verified_target: bool,
    ) {
        let suffix = match self {
            Self::Route { route, reason } => {
                let route = match route {
                    Route::Owned => "owned",
                    Route::Http => "http",
                    Route::Rejected => "rejected",
                };
                let reason = match reason {
                    Reason::FreshClock => "fresh_clock",
                    Reason::AwaitingHttp => "awaiting_http",
                    Reason::ExactPrevious => "exact_previous",
                    Reason::NoClock => "no_clock",
                    Reason::NoActions => "no_actions",
                    Reason::NotLocal => "not_local",
                    Reason::OwnedRejected => "owned_rejected",
                    Reason::StaleOwnedClock => "stale_owned_clock",
                    Reason::RateLimited => "rate_limited",
                    Reason::Backoff => "backoff",
                    Reason::OwnedBusy => "owned_busy",
                    Reason::QueueFull => "queue_full",
                };
                format!("phase=route route={route} reason={reason}")
            }
            Self::HttpDispatch => "phase=http_dispatch".into(),
            Self::HttpAccepted => "phase=http_accepted accepted=true".into(),
            Self::PreviousObservedAfterTimeout => "phase=previous_observed_after_timeout".into(),
            Self::OwnedPreviousSuccessorPause => "phase=owned_previous_successor_pause".into(),
            Self::OwnedPreviousSuccessorAlreadyPaused => {
                "phase=owned_previous_successor_already_paused".into()
            }
            Self::OwnedPreviousSuccessorPlay => "phase=owned_previous_successor_play".into(),
            Self::OwnedPreviousFromStart => "phase=owned_previous_from_start".into(),
            Self::OwnedNextAfterPrevious => "phase=owned_next_after_previous".into(),
            Self::OwnedNextSuccessorPause => "phase=owned_next_successor_pause".into(),
            Self::HttpFailed(status) => format!("phase=http_failed accepted=false status={status}"),
            Self::HttpReadDeferred => "phase=http_read_deferred".into(),
            Self::ResumeTarget(mode) => format!("phase=resume_target mode={mode}"),
        };
        let _ = writeln!(
            output,
            "auvra_transport time_ms={time_ms} kind={kind} verified_target={verified_target} {suffix}"
        );
    }
}
