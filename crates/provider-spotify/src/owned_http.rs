//! Evidence for an ordered HTTP transport burst on the authenticated local output.
use crate::{
    mapping,
    output_clock::{CONTROL_FRESHNESS, QUEUE_EXPIRY},
    owned_commands::SeekBarrier,
};
use player_domain::{MediaId, PlaybackState, RemoteControl, RemoteOutputClock, RemotePlayback};
use std::time::{Duration, Instant};

const TRANSITION_LIMIT: Duration = Duration::from_secs(4);
const NATIVE_PAUSE_OFFER: Duration = Duration::from_millis(80);
const NATIVE_PAUSE_LIMIT: Duration = Duration::from_secs(2);
const NATIVE_PREVIOUS_OFFER: Duration = Duration::from_millis(80);
const NATIVE_ONLY_OFFER: Duration = Duration::from_millis(450);
const PREVIOUS_RESUME_OFFER: Duration = Duration::from_millis(450);
const PREVIOUS_RESUME_POSITION_MS: u64 = 120;

pub(super) fn previous_prefers_web(clock: &RemoteOutputClock) -> bool {
    // Soloist's relative command may restart the current track. The caller
    // already chose Previous over Seek, even if playback advanced meanwhile.
    (clock.state == PlaybackState::Paused
        || clock.state == PlaybackState::Playing && clock.rate > 0.0)
        && clock
            .actions
            .as_ref()
            .is_some_and(|actions| actions.previous)
}

pub(super) fn relative(control: &RemoteControl) -> bool {
    matches!(control, RemoteControl::Next | RemoteControl::Previous)
}

pub(super) fn follows_relative(control: &RemoteControl) -> bool {
    relative(control) || matches!(control, RemoteControl::Play | RemoteControl::Pause)
}

pub(super) fn replaces_lane(control: &RemoteControl) -> bool {
    matches!(
        control,
        RemoteControl::Start { .. }
            | RemoteControl::StartOnDevice { .. }
            | RemoteControl::StartSpokenOnDevice { .. }
            | RemoteControl::ResumeOnDevice { .. }
            | RemoteControl::StartContext { .. }
            | RemoteControl::PauseOnDevice { .. }
            | RemoteControl::Transfer { .. }
    )
}

pub(super) struct Row {
    pub media: MediaId,
    pub context: String,
}

pub(super) struct PreviousWitness {
    pub predecessor: MediaId,
    pub context: String,
    pub queue_epoch: u64,
    pub observed_at: Instant,
    pub unambiguous_context_predecessor: bool,
}

struct Sample {
    media: MediaId,
    context: Option<String>,
    previous_allowed: bool,
    next_allowed: Option<bool>,
    pause_allowed: Option<bool>,
    play_allowed: bool,
    cue_allowed: bool,
    state: PlaybackState,
    paused: bool,
    paused_at_start: bool,
    queue_epoch: Option<u64>,
    sequence: u64,
    at: Instant,
}
impl Sample {
    fn read(clock: &RemoteOutputClock, at: Instant) -> Option<Self> {
        let valid_state = match clock.state {
            PlaybackState::Playing => clock.rate > 0.0,
            PlaybackState::Paused => true,
            PlaybackState::Loading => {
                clock.rate == 0.0 && clock.actions.as_ref().is_some_and(|actions| actions.next)
            }
            _ => false,
        };
        if !clock.active || clock.control_uptime_ms.is_none() || !valid_state {
            return None;
        }
        Some(Self {
            media: clock.media_id.clone()?,
            context: clock
                .metadata
                .as_ref()
                .and_then(|item| item.context_uri.clone()),
            previous_allowed: clock
                .actions
                .as_ref()
                .is_some_and(|actions| actions.previous),
            next_allowed: clock.actions.as_ref().map(|actions| actions.next),
            pause_allowed: clock.actions.as_ref().map(|actions| actions.pause),
            play_allowed: clock.actions.as_ref().is_some_and(|actions| actions.play),
            cue_allowed: clock
                .actions
                .as_ref()
                .is_some_and(|actions| actions.seek && actions.previous)
                && (clock.state != PlaybackState::Paused || clock.rate == 0.0),
            state: clock.state,
            paused: clock.state == PlaybackState::Paused && clock.rate == 0.0,
            paused_at_start: clock.state == PlaybackState::Paused
                && clock.rate == 0.0
                && clock.position_ms <= PREVIOUS_RESUME_POSITION_MS,
            queue_epoch: clock.queue_epoch,
            sequence: clock.sequence,
            at,
        })
    }
}

pub(super) struct Lane {
    pub id: u64,
    pub device: String,
    generation: u64,
    sample: Option<Sample>,
    initial_row: Option<Row>,
    initial_previous: Option<MediaId>,
    preceding_seek: Option<SeekBarrier>,
    seek_confirmed: bool,
    previous_witness: Option<PreviousWitness>,
    cued_previous: Option<PreviousWitness>,
    cue_offer_until: Option<Instant>,
    relative_may_start: bool,
    relative: Option<Transition>,
    followup_offer: Option<FollowupOffer>,
    previous_resume: Option<PreviousResume>,
    native_pause: Option<NativePause>,
    native_pause_used: bool,
    native_only: bool,
    until: Instant,
}

struct Transition {
    before: Sample,
    sent_at: Instant,
    accepted: bool,
    observing_previous: bool,
    previous_witness: Option<PreviousWitness>,
    exact_previous: bool,
    native: Option<NativeRelative>,
}

struct NativeRelative {
    command_id: u64,
    target: MediaId,
    cued_previous: bool,
    resume_request: Option<u64>,
}

#[derive(Clone)]
struct Occurrence {
    media: MediaId,
    context: String,
    epoch: u64,
}

impl Occurrence {
    fn matches(
        &self,
        media: Option<&MediaId>,
        context: Option<&String>,
        epoch: Option<u64>,
    ) -> bool {
        media == Some(&self.media) && context == Some(&self.context) && epoch == Some(self.epoch)
    }
    fn matches_sample(&self, sample: &Sample) -> bool {
        self.matches(
            Some(&sample.media),
            sample.context.as_ref(),
            sample.queue_epoch,
        )
    }
}

struct FollowupOffer {
    occurrence: Occurrence,
    deadline: Instant,
    origin: FollowupOrigin,
}

struct PreviousResume {
    occurrence: Occurrence,
    deadline: Instant,
    request_id: u64,
}

#[derive(PartialEq, Eq)]
enum FollowupOrigin {
    WebPrevious,
    CuedPrevious { outgoing: MediaId },
    NativeNext,
}

struct NativePause {
    occurrence: Occurrence,
    command_id: u64,
    sequence: u64,
    cued_previous: bool,
    acknowledged: bool,
    confirmed: bool,
    deadline: Instant,
}

impl Lane {
    pub(super) fn new(
        id: u64,
        clock: &RemoteOutputClock,
        at: Instant,
        initial_row: Option<Row>,
        preceding_seek: Option<SeekBarrier>,
        now: Instant,
    ) -> Option<Self> {
        let sample =
            Sample::read(clock, at).filter(|sample| sample.state != PlaybackState::Loading);
        if sample.is_none() && preceding_seek.is_none() {
            return None;
        }
        Some(Self {
            id,
            device: clock.device_id.clone(),
            generation: clock.generation,
            sample,
            initial_row,
            initial_previous: None,
            preceding_seek,
            seek_confirmed: false,
            previous_witness: None,
            cued_previous: None,
            cue_offer_until: None,
            relative_may_start: false,
            relative: None,
            followup_offer: None,
            previous_resume: None,
            native_pause: None,
            native_pause_used: false,
            native_only: false,
            until: now + TRANSITION_LIMIT,
        })
    }

    pub(super) fn pin_previous(
        &mut self,
        clock: &RemoteOutputClock,
        witness: Option<PreviousWitness>,
    ) {
        self.previous_witness = witness;
        self.initial_previous.clone_from(&clock.media_id);
        self.relative_may_start = clock.state == PlaybackState::Paused
            || self.preceding_seek.is_some() && clock.state == PlaybackState::Loading;
    }

    pub(super) fn waiting_for_seek(&self) -> Option<SeekBarrier> {
        self.preceding_seek.filter(|_| !self.seek_confirmed)
    }

    pub(super) fn has_preceding_seek(&self) -> bool {
        self.preceding_seek.is_some()
    }

    pub(super) fn confirm_seek(&mut self, now: Instant) {
        self.seek_confirmed = true;
        self.cue_offer_until = self
            .cued_previous
            .as_ref()
            .map(|_| now + self.cue_offer_duration());
    }

    pub(super) fn offer_cued_previous(&mut self, witness: PreviousWitness, now: Instant) {
        self.cued_previous = Some(witness);
        if self.waiting_for_seek().is_none() {
            self.cue_offer_until = Some(now + self.cue_offer_duration());
        }
    }

    fn cue_offer_duration(&self) -> Duration {
        if self.native_only {
            NATIVE_ONLY_OFFER
        } else {
            NATIVE_PREVIOUS_OFFER
        }
    }

    pub(super) fn native_only(&self) -> bool {
        self.native_only
    }

    pub(super) fn restrict_to_native_previous(&mut self) {
        if self.native_only {
            return;
        }
        self.native_only = true;
        if let Some(until) = &mut self.cue_offer_until {
            *until += NATIVE_ONLY_OFFER - NATIVE_PREVIOUS_OFFER;
        }
        if let Some(offer) = &mut self.followup_offer
            && matches!(offer.origin, FollowupOrigin::CuedPrevious { .. })
        {
            offer.deadline += NATIVE_ONLY_OFFER - NATIVE_PAUSE_OFFER;
        }
    }

    pub(super) fn has_cued_previous(&self, now: Instant) -> bool {
        !self.expired(now)
            && (self.cued_previous.is_some()
                || self.relative.as_ref().is_some_and(|relative| {
                    relative
                        .native
                        .as_ref()
                        .is_some_and(|native| native.cued_previous)
                })
                || self.followup_offer.as_ref().is_some_and(|offer| {
                    matches!(offer.origin, FollowupOrigin::CuedPrevious { .. })
                }))
    }

    pub(super) fn can_keep_native_previous(&self, now: Instant) -> bool {
        !self.expired(now)
            && (self.has_cued_previous(now)
                && self
                    .cued_previous
                    .as_ref()
                    .is_none_or(|witness| witness.unambiguous_context_predecessor)
                || self
                    .previous_resume
                    .as_ref()
                    .is_some_and(|resume| now < resume.deadline)
                || self
                    .native_pause
                    .as_ref()
                    .is_some_and(|pause| pause.cued_previous && !pause.confirmed))
    }

    pub(super) fn waiting_to_send_previous(&self) -> bool {
        self.initial_previous.is_some()
    }

    pub(super) fn relative_start_pending(&self, now: Instant) -> bool {
        self.relative_may_start && !self.expired(now)
    }

    /// A gap never grants control. An explicitly different or inactive runtime revokes it.
    pub(super) fn observe(&mut self, clock: Option<(&RemoteOutputClock, Instant)>) -> bool {
        let Some((clock, at)) = clock else {
            self.cancel_previous_resume();
            self.sample = None;
            return !self.native_only;
        };
        if clock.generation != self.generation
            || clock.device_id != self.device
            || !clock.active
            || clock.control_uptime_ms.is_none()
            || self.preceding_seek.is_some_and(|seek| {
                clock.queue_epoch != seek.queue_epoch || clock.media_id.is_none()
            })
            || self.initial_previous.as_ref().is_some_and(|expected| {
                clock
                    .media_id
                    .as_ref()
                    .is_some_and(|media| !mapping::same_playback_id(media, expected))
            })
            || self.native_only
                && self.cued_previous.as_ref().is_some_and(|witness| {
                    clock.queue_epoch != Some(witness.queue_epoch)
                        || clock
                            .metadata
                            .as_ref()
                            .and_then(|metadata| metadata.context_uri.as_ref())
                            .is_some_and(|context| context != &witness.context)
                })
        {
            return false;
        }
        if let Some(relative) = &self.relative
            && let Some(native) = &relative.native
            && !(clock
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.context_uri.as_ref())
                .is_none_or(|context| relative.before.context.as_ref() == Some(context))
                && clock.media_id.as_ref().is_some_and(|media| {
                    if mapping::same_playback_id(media, &relative.before.media) {
                        clock.queue_epoch == relative.before.queue_epoch
                            || !native.cued_previous
                                && clock.state == PlaybackState::Loading
                                && clock.rate == 0.0
                                && clock.actions.is_none()
                                && clock
                                    .queue_epoch
                                    .zip(relative.before.queue_epoch)
                                    .is_some_and(|(now, before)| now > before)
                    } else {
                        mapping::same_playback_id(media, &native.target)
                            && clock
                                .queue_epoch
                                .zip(relative.before.queue_epoch)
                                .is_some_and(|(now, before)| now > before)
                    }
                }))
        {
            return false;
        }
        if let Some(occurrence) = self
            .native_pause
            .as_ref()
            .map(|pause| &pause.occurrence)
            .or_else(|| self.followup_offer.as_ref().map(|offer| &offer.occurrence))
            && !occurrence.matches(
                clock.media_id.as_ref(),
                clock
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.context_uri.as_ref()),
                clock.queue_epoch,
            )
        {
            return false;
        }
        if self.relative.as_ref().is_some_and(|relative| {
            relative.previous_witness.as_ref().is_some_and(|expected| {
                clock.media_id.as_ref().is_some_and(|media| {
                    !mapping::same_playback_id(media, &relative.before.media)
                        && !mapping::same_playback_id(media, &expected.predecessor)
                }) || clock
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.context_uri.as_ref())
                    .is_some_and(|context| context != &expected.context)
            })
        }) && let Some(relative) = &mut self.relative
        {
            relative.previous_witness = None;
        }
        self.sample = Sample::read(clock, at).filter(|sample| {
            sample.state != PlaybackState::Loading
                || self.relative.as_ref().is_some_and(|relative| {
                    relative
                        .native
                        .as_ref()
                        .is_some_and(|native| native.cued_previous)
                        && Self::successor(relative, sample).is_some()
                })
        });
        if let Some((relative, sample)) = self.relative.as_mut().zip(self.sample.as_ref())
            && let Some(native) = &mut relative.native
            && native.resume_request.is_some()
            && sample.state != PlaybackState::Loading
            && !sample.paused_at_start
            && sample.at > relative.sent_at
            && sample.sequence > relative.before.sequence
            && sample
                .queue_epoch
                .zip(relative.before.queue_epoch)
                .is_some_and(|(now, before)| now > before)
            && mapping::same_playback_id(&sample.media, &native.target)
        {
            // A receipt may arrive after several clocks. An earlier moving
            // successor (even before its context metadata) cannot authorize
            // resume from a subsequent independent Pause near zero.
            native.resume_request = None;
        }
        if self.previous_resume.as_ref().is_some_and(|resume| {
            self.sample.as_ref().is_none_or(|sample| {
                !sample.paused_at_start || !resume.occurrence.matches_sample(sample)
            })
        }) {
            self.previous_resume = None;
        }
        self.settle(at);
        true
    }

    pub(super) fn conflicts_with(&self, frame: Option<&RemotePlayback>) -> bool {
        frame
            .and_then(|frame| frame.device.as_ref())
            .is_some_and(|device| {
                device.id == self.device && device.is_restricted
                    || device.id != self.device && device.is_active
            })
    }

    pub(super) fn expired(&self, now: Instant) -> bool {
        now >= self.until
            || self.native_only && self.cue_offer_until.is_some_and(|until| now >= until)
            || self
                .native_pause
                .as_ref()
                .is_some_and(|pause| !pause.confirmed && now >= pause.deadline)
    }

    pub(super) fn ready(&self, now: Instant) -> bool {
        self.relative.is_none()
            && !self.native_pause_pending()
            && self.waiting_for_seek().is_none()
            && !self.expired(now)
            && self.sample.as_ref().is_some_and(|sample| {
                now.saturating_duration_since(sample.at) <= CONTROL_FRESHNESS
                    && self.initial_row.as_ref().is_none_or(|row| {
                        sample.media == row.media && sample.context.as_ref() == Some(&row.context)
                    })
                    && self.initial_previous.as_ref().is_none_or(|media| {
                        sample.previous_allowed && mapping::same_playback_id(&sample.media, media)
                    })
            })
    }

    pub(super) fn navigating(&self) -> bool {
        self.relative.is_some() || self.native_pause_pending()
    }

    pub(super) fn awaiting_initial_row(&self) -> bool {
        self.initial_row.is_some()
    }

    pub(super) fn begin_relative(&mut self, now: Instant) {
        if let Some(before) = self.sample.take() {
            let exact_previous = self.waiting_to_send_previous();
            self.initial_row = None;
            self.initial_previous = None;
            self.preceding_seek = None;
            let previous_witness = self.previous_witness.take().filter(|expected| {
                before.queue_epoch == Some(expected.queue_epoch)
                    && before.context.as_ref() == Some(&expected.context)
                    && now.saturating_duration_since(expected.observed_at) <= CONTROL_FRESHNESS
            });
            self.relative = Some(Transition {
                before,
                sent_at: now,
                accepted: false,
                observing_previous: false,
                previous_witness,
                exact_previous,
                native: None,
            });
            self.native_pause_used = false;
            self.until = now + TRANSITION_LIMIT;
        }
    }

    pub(super) fn accepted(&mut self, now: Instant) {
        if let Some(relative) = &mut self.relative {
            relative.accepted = true;
            self.until = now + TRANSITION_LIMIT;
            self.settle(now);
        }
    }

    pub(super) fn start_previous_observation(&mut self, now: Instant, wait: Duration) {
        if let Some(relative) = &mut self.relative {
            relative.observing_previous = true;
            self.until = now + wait;
        }
    }

    pub(super) fn observing_previous(&self) -> bool {
        self.relative
            .as_ref()
            .is_some_and(|relative| relative.observing_previous)
    }

    pub(super) fn can_reconcile_previous(&self) -> bool {
        self.relative
            .as_ref()
            .is_some_and(|relative| relative.previous_witness.is_some())
    }

    pub(super) fn previous_observed(&self, now: Instant) -> bool {
        self.relative
            .as_ref()
            .zip(self.sample.as_ref())
            .is_some_and(|(relative, sample)| {
                relative.previous_witness.as_ref().is_some_and(|expected| {
                    (sample.state == PlaybackState::Playing || self.native_pause_used)
                        && sample.sequence > relative.before.sequence
                        && sample.at > relative.sent_at
                        && now.saturating_duration_since(sample.at) <= CONTROL_FRESHNESS
                        && mapping::same_playback_id(&sample.media, &expected.predecessor)
                        && sample.context.as_ref() == Some(&expected.context)
                })
            })
    }

    pub(super) fn offers_cued_previous(&self, now: Instant) -> bool {
        self.ready(now)
            && self.cue_offer_until.is_some_and(|until| now < until)
            && self
                .sample
                .as_ref()
                .zip(self.cued_previous.as_ref())
                .is_some_and(|(sample, witness)| {
                    sample.cue_allowed
                        && sample.queue_epoch == Some(witness.queue_epoch)
                        && sample.context.as_ref() == Some(&witness.context)
                        && now.saturating_duration_since(witness.observed_at) <= QUEUE_EXPIRY
                })
    }

    pub(super) fn cued_previous_candidate(
        &self,
        clock: &RemoteOutputClock,
        latest: &PreviousWitness,
        now: Instant,
    ) -> bool {
        clock.generation == self.generation
            && clock.device_id == self.device
            && self
                .sample
                .as_ref()
                .is_some_and(|sample| sample.sequence == clock.sequence)
            && self.cued_previous.as_ref().is_some_and(|pinned| {
                pinned.predecessor == latest.predecessor
                    && pinned.context == latest.context
                    && pinned.queue_epoch == latest.queue_epoch
                    && (!self.native_only || latest.unambiguous_context_predecessor)
            })
            && self.offers_cued_previous(now)
    }

    pub(super) fn begin_cued_previous(
        &mut self,
        command_id: u64,
        resume_request: Option<u64>,
        now: Instant,
    ) {
        let witness = self.cued_previous.take();
        self.cue_offer_until = None;
        self.begin_relative(now);
        if let (Some(relative), Some(witness)) = (&mut self.relative, witness) {
            relative.native = Some(NativeRelative {
                command_id,
                target: witness.predecessor,
                cued_previous: true,
                resume_request: resume_request.filter(|request| {
                    *request > 0
                        && witness.unambiguous_context_predecessor
                        && relative.before.state == PlaybackState::Playing
                }),
            });
        }
    }

    pub(super) fn offers_native_next(&self, now: Instant) -> bool {
        self.ready(now)
            && self.sample.as_ref().is_some_and(|sample| {
                sample.next_allowed != Some(false)
                    && self.followup_offer.as_ref().is_some_and(|offer| {
                        matches!(offer.origin, FollowupOrigin::CuedPrevious { .. })
                            && now < offer.deadline
                            && offer.occurrence.matches_sample(sample)
                    })
            })
    }

    pub(super) fn native_next_candidate(
        &self,
        clock: &RemoteOutputClock,
        head: &MediaId,
        now: Instant,
    ) -> bool {
        let Some(sample) = self.sample.as_ref().filter(|sample| {
            clock.generation == self.generation
                && clock.device_id == self.device
                && sample.sequence == clock.sequence
                && sample.next_allowed == Some(true)
        }) else {
            return false;
        };
        let outgoing = if sample.state == PlaybackState::Loading {
            // Next can leave this identified predecessor before it plays. Keep
            // the relative transition pending so the HTTP lane still waits.
            self.relative
                .as_ref()
                .filter(|relative| {
                    relative.accepted
                        && !self.expired(now)
                        && now.saturating_duration_since(sample.at) <= CONTROL_FRESHNESS
                })
                .map(|relative| &relative.before.media)
        } else {
            self.followup_offer
                .as_ref()
                .filter(|_| self.offers_native_next(now))
                .and_then(|offer| {
                    if let FollowupOrigin::CuedPrevious { outgoing } = &offer.origin {
                        Some(outgoing)
                    } else {
                        None
                    }
                })
        };
        // The host can tag a late queue event with the new occurrence. Only
        // return to the item that this Previous actually left.
        outgoing.is_some_and(|outgoing| mapping::same_playback_id(outgoing, head))
    }

    pub(super) fn begin_native_next(&mut self, command_id: u64, target: MediaId, now: Instant) {
        self.followup_offer = None;
        self.relative_may_start = self
            .sample
            .as_ref()
            .is_some_and(|sample| sample.state != PlaybackState::Playing);
        self.begin_relative(now);
        if let Some(relative) = &mut self.relative {
            relative.native = Some(NativeRelative {
                command_id,
                target,
                cued_previous: false,
                resume_request: None,
            });
        }
    }

    pub(super) fn native_relative_pending(&self) -> bool {
        self.relative
            .as_ref()
            .is_some_and(|relative| relative.native.is_some())
    }

    pub(super) fn owns_native_relative(&self, id: u64, generation: u64) -> bool {
        generation == self.generation
            && self
                .relative
                .as_ref()
                .and_then(|relative| relative.native.as_ref())
                .is_some_and(|native| native.command_id == id)
    }

    pub(super) fn acknowledge_native_relative(&mut self, id: u64, generation: u64, now: Instant) {
        if self.owns_native_relative(id, generation)
            && let Some(relative) = &mut self.relative
        {
            relative.accepted = true;
            self.settle(now);
        }
    }

    fn successor(relative: &Transition, sample: &Sample) -> Option<Occurrence> {
        let epoch = sample.queue_epoch?;
        let context = sample.context.as_ref()?;
        ((relative.exact_previous || relative.native.is_some())
            && sample.sequence > relative.before.sequence
            && sample.at > relative.sent_at
            && relative
                .before
                .queue_epoch
                .is_some_and(|before| epoch > before)
            && relative.before.context.as_ref() == Some(context)
            && !mapping::same_playback_id(&sample.media, &relative.before.media)
            && relative
                .native
                .as_ref()
                .is_none_or(|native| mapping::same_playback_id(&sample.media, &native.target)))
        .then(|| Occurrence {
            media: sample.media.clone(),
            context: context.clone(),
            epoch,
        })
    }

    pub(super) fn offers_native_pause(&self, now: Instant) -> bool {
        !self.expired(now)
            && !self.native_pause_used
            && self.sample.as_ref().is_some_and(|sample| {
                sample.state == PlaybackState::Playing
                    && now.saturating_duration_since(sample.at) <= CONTROL_FRESHNESS
                    && self.followup_offer.as_ref().is_some_and(|offer| {
                        now < offer.deadline
                            && offer.occurrence.matches_sample(sample)
                            && (sample.pause_allowed == Some(true)
                                || sample.pause_allowed.is_none()
                                    && offer.origin == FollowupOrigin::NativeNext)
                    })
            })
    }

    pub(super) fn native_pause_candidate(&self, clock: &RemoteOutputClock, now: Instant) -> bool {
        !self.expired(now)
            && !self.native_pause_used
            && clock.generation == self.generation
            && clock.device_id == self.device
            && self.sample.as_ref().is_some_and(|sample| {
                sample.sequence == clock.sequence
                    && sample.state == PlaybackState::Playing
                    && sample.pause_allowed == Some(true)
                    && (self.offers_native_pause(now)
                        || self.relative.as_ref().is_some_and(|relative| {
                            relative.previous_witness.as_ref().is_some_and(|witness| {
                                witness.unambiguous_context_predecessor
                                    && now.saturating_duration_since(witness.observed_at)
                                        <= CONTROL_FRESHNESS
                            }) && relative.native.is_none()
                                && !relative.accepted
                                && !relative.observing_previous
                                && self.previous_observed(now)
                                && Self::successor(relative, sample).is_some()
                        }))
            })
    }

    pub(super) fn satisfied_cued_pause(&self, now: Instant) -> bool {
        !self.expired(now)
            && !self.native_pause_used
            && self
                .followup_offer
                .as_ref()
                .zip(self.sample.as_ref())
                .is_some_and(|(offer, sample)| {
                    matches!(offer.origin, FollowupOrigin::CuedPrevious { .. })
                        && sample.paused
                        && now.saturating_duration_since(sample.at) <= CONTROL_FRESHNESS
                        && offer.occurrence.matches_sample(sample)
                })
    }

    pub(super) fn satisfies_cued_pause_on(&self, clock: &RemoteOutputClock, now: Instant) -> bool {
        clock.generation == self.generation
            && clock.device_id == self.device
            && self
                .sample
                .as_ref()
                .is_some_and(|sample| sample.sequence == clock.sequence)
            && self.satisfied_cued_pause(now)
    }

    pub(super) fn pause_follows_native_next(&self) -> bool {
        self.followup_offer
            .as_ref()
            .is_some_and(|offer| offer.origin == FollowupOrigin::NativeNext)
    }

    pub(super) fn begin_native_pause(&mut self, command_id: u64, now: Instant) {
        let Some(sample) = &self.sample else { return };
        let cued_previous = self
            .followup_offer
            .as_ref()
            .is_some_and(|offer| matches!(offer.origin, FollowupOrigin::CuedPrevious { .. }));
        let occurrence = self
            .followup_offer
            .take()
            .map(|offer| offer.occurrence)
            .or_else(|| Self::successor(self.relative.as_ref()?, sample));
        if let Some(occurrence) = occurrence {
            self.native_pause = Some(NativePause {
                occurrence,
                command_id,
                sequence: sample.sequence,
                cued_previous,
                acknowledged: false,
                confirmed: false,
                deadline: now + NATIVE_PAUSE_LIMIT,
            });
            self.native_pause_used = true;
        }
    }

    pub(super) fn native_pause_pending(&self) -> bool {
        self.native_pause
            .as_ref()
            .is_some_and(|pause| !pause.confirmed)
    }

    pub(super) fn strict_followup_scope(&self) -> bool {
        self.native_pause.is_some() || self.followup_offer.is_some()
    }

    pub(super) fn owns_native_pause(&self, id: u64, generation: u64) -> bool {
        generation == self.generation
            && self
                .native_pause
                .as_ref()
                .is_some_and(|pause| pause.command_id == id)
    }

    pub(super) fn acknowledges_native_pause(
        &mut self,
        id: u64,
        generation: u64,
        now: Instant,
    ) -> bool {
        if !self.owns_native_pause(id, generation) {
            return false;
        }
        self.native_pause.as_mut().unwrap().acknowledged = true;
        self.settle(now);
        true
    }

    pub(super) fn begin_http_followup(&mut self) {
        self.followup_offer = None;
        self.native_pause = None;
        self.cued_previous = None;
        self.cue_offer_until = None;
        self.previous_resume = None;
    }

    pub(super) fn cancel_previous_resume(&mut self) {
        self.previous_resume = None;
        if let Some(native) = self
            .relative
            .as_mut()
            .and_then(|relative| relative.native.as_mut())
        {
            native.resume_request = None;
        }
    }

    pub(super) fn previous_resume_request(
        &self,
        clock: &RemoteOutputClock,
        now: Instant,
    ) -> Option<u64> {
        let resume = self.previous_resume.as_ref()?;
        let sample = self.sample.as_ref()?;
        (!self.expired(now)
            && clock.generation == self.generation
            && clock.device_id == self.device
            && now < resume.deadline
            && sample.sequence == clock.sequence
            && now.saturating_duration_since(sample.at) <= CONTROL_FRESHNESS
            && sample.paused_at_start
            && sample.play_allowed
            && resume.occurrence.matches_sample(sample))
        .then_some(resume.request_id)
    }

    pub(super) fn discard_followup_offer(&mut self, next_pending: bool) {
        if !next_pending
            || !self
                .followup_offer
                .as_ref()
                .is_some_and(|offer| matches!(offer.origin, FollowupOrigin::CuedPrevious { .. }))
        {
            self.followup_offer = None;
        }
    }

    fn settle(&mut self, now: Instant) {
        if let Some((pause, sample)) = self.native_pause.as_mut().zip(self.sample.as_ref())
            && pause.acknowledged
            && sample.state == PlaybackState::Paused
            && sample.sequence > pause.sequence
            && pause.occurrence.matches_sample(sample)
            && now < pause.deadline
        {
            pause.confirmed = true;
        }
        if let Some((relative, sample)) = self.relative.as_ref().zip(self.sample.as_ref())
            && relative.accepted
            && sample.state != PlaybackState::Loading
            && sample.sequence > relative.before.sequence
            && sample.at > relative.sent_at
            && !mapping::same_playback_id(&sample.media, &relative.before.media)
            && (relative.native.is_none()
                || now < self.until && Self::successor(relative, sample).is_some())
        {
            if sample.paused_at_start
                && let Some(request_id) = relative
                    .native
                    .as_ref()
                    .and_then(|native| native.resume_request)
                && let Some(occurrence) = Self::successor(relative, sample)
            {
                // Soloist can select the exact predecessor and leave it paused
                // at zero even though the compound command left a moving item.
                // The first complete successor alone can authorize one repair.
                self.previous_resume = Some(PreviousResume {
                    occurrence,
                    deadline: sample.at + PREVIOUS_RESUME_OFFER,
                    request_id,
                });
            }
            self.followup_offer = if self.native_pause_used {
                None
            } else {
                Self::successor(relative, sample).map(|occurrence| FollowupOffer {
                    occurrence,
                    deadline: now
                        + if self.native_only {
                            NATIVE_ONLY_OFFER
                        } else {
                            NATIVE_PAUSE_OFFER
                        },
                    origin: match &relative.native {
                        Some(native) if native.cued_previous => FollowupOrigin::CuedPrevious {
                            outgoing: relative.before.media.clone(),
                        },
                        Some(_) => FollowupOrigin::NativeNext,
                        None => FollowupOrigin::WebPrevious,
                    },
                })
            };
            self.relative = None;
            self.relative_may_start = false;
        }
    }
}
