//! Live PCM from the verified process tap to Auvra's system renderer.
//! All queue and AVFoundation work is on the owned-output worker.
use super::{capture::Sample, live_trace};
use crate::spatial::SpatialRenderer;
use objc2_core_audio::*;
use objc2_core_audio_types::{
    AudioStreamBasicDescription, kAudioFormatFlagIsFloat, kAudioFormatFlagIsPacked,
    kAudioFormatFlagIsSignedInteger, kAudioFormatLinearPCM,
};
use player_domain::HostSpatialStatus as Failure;
use std::{
    collections::VecDeque,
    ptr::{self, NonNull},
    time::{Duration, Instant},
};

const BATCH: usize = 1024;
const STALL: Duration = Duration::from_millis(400);
const COLD_START_LIMIT: Duration = Duration::from_millis(1700);
const PAUSED_ANCHOR_LIMIT: Duration = Duration::from_millis(350);
const PAUSED_ANCHOR_STABLE: Duration = Duration::from_millis(8);
const PAUSED_POSITION_TOLERANCE: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PausedRouteFormat {
    sample_rate_bits: u64,
    format_id: u32,
    flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels: u32,
    bits_per_channel: u32,
}

impl PausedRouteFormat {
    fn read(format: &AudioStreamBasicDescription) -> Option<Self> {
        (supported_stereo_format(format)
            && format.mSampleRate.is_finite()
            && format.mSampleRate > 0.0)
            .then_some(Self {
                sample_rate_bits: format.mSampleRate.to_bits(),
                format_id: format.mFormatID,
                flags: format.mFormatFlags,
                bytes_per_packet: format.mBytesPerPacket,
                frames_per_packet: format.mFramesPerPacket,
                bytes_per_frame: format.mBytesPerFrame,
                channels: format.mChannelsPerFrame,
                bits_per_channel: format.mBitsPerChannel,
            })
    }
}

fn paused_route_format(route: u32) -> Option<PausedRouteFormat> {
    if stereo_bluetooth_output() != Some(route) {
        return None;
    }
    // SAFETY: Every ASBD field accepts zero. HAL fills the whole structure.
    let initial: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
    let format = read_property(
        route,
        kAudioDevicePropertyStreamFormat,
        kAudioObjectPropertyScopeOutput,
        initial,
    )?;
    let fingerprint = PausedRouteFormat::read(&format)?;
    (probe_bluetooth_output() == Some(route)).then_some(fingerprint)
}

fn warm_position_is_current(origin: u64, position: u64, elapsed: Duration, rate: u32) -> bool {
    // A suspended clock allows one frame of quantization around its anchor.
    // None of that old interval can count as progress after restart.
    (origin == 0 || position.saturating_sub(origin) > PAUSED_POSITION_TOLERANCE)
        && crate::spatial::start_position_is_current(origin, position, elapsed, rate)
}

pub(super) fn stereo_bluetooth_output() -> Option<u32> {
    let id = probe_bluetooth_output()?;
    // SAFETY: All fields of the HAL stream description accept zero.
    let initial: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
    let format = live_trace::measure(
        "relay_route_format_start",
        "relay_route_format_done",
        || {
            read_property(
                id,
                kAudioDevicePropertyStreamFormat,
                kAudioObjectPropertyScopeOutput,
                initial,
            )
        },
    )?;
    (supported_stereo_format(&format) && probe_bluetooth_output() == Some(id)).then_some(id)
}

fn probe_bluetooth_output() -> Option<u32> {
    let id = live_trace::measure(
        "relay_route_default_start",
        "relay_route_default_done",
        default_output_id,
    )?;
    let transport = live_trace::measure(
        "relay_route_transport_start",
        "relay_route_transport_done",
        || {
            property::<u32>(
                id,
                kAudioDevicePropertyTransportType,
                kAudioObjectPropertyScopeGlobal,
            )
        },
    )?;
    (transport == kAudioDeviceTransportTypeBluetooth
        || transport == kAudioDeviceTransportTypeBluetoothLE)
        .then_some(id)
}

fn supported_stereo_format(format: &AudioStreamBasicDescription) -> bool {
    let flags = format.mFormatFlags;
    // Retain CPAL's packed F32/I16 default-config format checks.
    let supported_sample = if flags & kAudioFormatFlagIsFloat != 0 {
        format.mBitsPerChannel == 32
    } else {
        flags & kAudioFormatFlagIsSignedInteger != 0 && format.mBitsPerChannel == 16
    };
    format.mChannelsPerFrame == 2
        && format.mFormatID == kAudioFormatLinearPCM
        && flags & kAudioFormatFlagIsPacked != 0
        && supported_sample
}

fn interruption(route: u32, observed: Option<u32>, failure: Failure) -> Failure {
    if observed == Some(route) {
        failure
    } else {
        Failure::WaitingForRoute
    }
}

fn default_output_id() -> Option<u32> {
    property(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyDefaultOutputDevice,
        kAudioObjectPropertyScopeGlobal,
    )
    .filter(|id| *id != kAudioObjectUnknown)
}

fn property<T: Default>(device: u32, selector: u32, scope: u32) -> Option<T> {
    read_property(device, selector, scope, T::default())
}

fn read_property<T>(device: u32, selector: u32, scope: u32, mut value: T) -> Option<T> {
    let mut address = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size = size_of::<T>() as u32;
    let result = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&mut address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    (result == 0 && size as usize == size_of::<T>()).then_some(value)
}

fn scheduling_lead(rate: u32, device_rate: f64, latency_frames: u32) -> Result<usize, Failure> {
    let latency = if device_rate.is_finite() && device_rate > 0.0 {
        f64::from(latency_frames) / device_rate
    } else {
        0.0
    };
    // The hardware consumes packets ahead of the public media clock. The
    // floor also covers Bluetooth devices that report zero HAL latency.
    let lead = (latency + 0.04).max(0.20);
    if lead > 0.50 {
        return Err(Failure::DirectRenderer);
    }
    Ok((lead * f64::from(rate)).ceil() as usize)
}

fn route_lead(rate: u32, route: u32) -> Result<usize, Failure> {
    let device_rate = property::<f64>(
        route,
        kAudioDevicePropertyNominalSampleRate,
        kAudioObjectPropertyScopeGlobal,
    )
    .unwrap_or_default();
    let latency = property::<u32>(
        route,
        kAudioDevicePropertyLatency,
        kAudioObjectPropertyScopeOutput,
    )
    .unwrap_or_default();
    let safety = property::<u32>(
        route,
        kAudioDevicePropertySafetyOffset,
        kAudioObjectPropertyScopeOutput,
    )
    .unwrap_or_default();
    let buffer = property::<u32>(
        route,
        kAudioDevicePropertyBufferFrameSize,
        kAudioObjectPropertyScopeGlobal,
    )
    .unwrap_or_default();
    scheduling_lead(
        rate,
        device_rate,
        latency.saturating_add(safety).saturating_add(buffer),
    )
}

struct RelayQueue {
    frames: VecDeque<Sample>,
    // Prefix already queued with AVFoundation but not yet presented.
    submitted: usize,
    last_capture: Option<u64>,
    limit: usize,
}

impl RelayQueue {
    fn new(limit: usize) -> Self {
        Self {
            frames: VecDeque::with_capacity(limit),
            submitted: 0,
            last_capture: None,
            limit,
        }
    }

    fn push(&mut self, frame: Sample) -> Result<(), Failure> {
        if self.frames.len() == self.limit
            || frame.position == 0
            || self
                .last_capture
                .is_some_and(|last| last.checked_add(1) != Some(frame.position))
            || frame
                .samples
                .iter()
                .any(|sample| !sample.is_finite() || sample.abs() > 1.0)
        {
            return Err(Failure::DirectBuffer);
        }
        self.last_capture = Some(frame.position);
        self.frames.push_back(frame);
        Ok(())
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.submitted = 0;
        self.last_capture = None;
    }

    fn discard_through(&mut self, position: u64) {
        debug_assert_eq!(self.submitted, 0);
        let count = self
            .frames
            .partition_point(|frame| frame.position <= position);
        self.frames.drain(..count);
    }

    fn present(&mut self, through: u64, mut receive: impl FnMut(Sample)) -> usize {
        let count = self
            .frames
            .partition_point(|frame| frame.position <= through)
            .min(self.submitted);
        self.submitted -= count;
        for frame in self.frames.drain(..count) {
            receive(frame);
        }
        count
    }
}

pub(super) struct Relay {
    renderer: SpatialRenderer,
    queue: RelayQueue,
    scratch: Vec<[f32; 2]>,
    route: u32,
    rate: usize,
    lead: usize,
    warm_frames: u64,
    clock_map: Option<(u64, u64)>,
    presented: bool,
    submitted_through: Option<u64>,
    last_progress: Instant,
    last_input: Instant,
    last_observed_input: Option<u64>,
    resubmit: bool,
    fade_from: Option<u64>,
    muted: bool,
    needs_start: bool,
    prepared_silence_started: Option<(u64, Instant)>,
    resume_warmup: Option<ResumeWarmup>,
    paused_format: Option<PausedRouteFormat>,
    signal_trace: Option<SignalTrace>,
}

#[derive(Default)]
struct SignalTrace {
    first_input: bool,
    input: bool,
    scheduled: bool,
    presented: bool,
}

#[derive(Clone, Copy, Debug)]
enum PausedZeroClock {
    Settling {
        requested: Instant,
        sample: u64,
        unchanged_since: Instant,
    },
    Suspended {
        anchor: u64,
    },
}

impl PausedZeroClock {
    fn begin(sample: u64, now: Instant) -> Self {
        Self::Settling {
            requested: now,
            sample,
            unchanged_since: now,
        }
    }

    fn observe(
        self,
        position: Option<u64>,
        tail: u64,
        horizon: u64,
        now: Instant,
    ) -> Result<Self, Failure> {
        let position = position.ok_or(Failure::DirectOutputStalled)?;
        if position > tail || tail - position > horizon {
            return Err(Failure::DirectOutputStalled);
        }
        match self {
            Self::Settling {
                requested,
                sample,
                unchanged_since,
            } => {
                if now.saturating_duration_since(requested) >= PAUSED_ANCHOR_LIMIT
                    || position.saturating_add(PAUSED_POSITION_TOLERANCE) < sample
                {
                    return Err(Failure::DirectOutputStalled);
                }
                if position != sample {
                    return Ok(Self::Settling {
                        requested,
                        sample: position,
                        unchanged_since: now,
                    });
                }
                if now.saturating_duration_since(unchanged_since) >= PAUSED_ANCHOR_STABLE {
                    Ok(Self::Suspended { anchor: position })
                } else {
                    Ok(self)
                }
            }
            Self::Suspended { anchor }
                if position.abs_diff(anchor) <= PAUSED_POSITION_TOLERANCE =>
            {
                Ok(self)
            }
            Self::Suspended { .. } => Err(Failure::DirectOutputStalled),
        }
    }

    fn anchor(self) -> Option<u64> {
        match self {
            Self::Suspended { anchor } => Some(anchor),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum ResumeWarmup {
    Resetting,
    Started { origin: u64, started: Instant },
    Retained(PausedZeroClock),
}

impl ResumeWarmup {
    fn position(&self, through: Option<u64>, now: Instant, rate: u32) -> Option<u64> {
        match self {
            Self::Resetting => through.filter(|&position| position == 0),
            Self::Started { origin, started } => through.filter(|&position| {
                warm_position_is_current(
                    *origin,
                    position,
                    now.saturating_duration_since(*started),
                    rate,
                )
            }),
            Self::Retained(_) => None,
        }
    }

    fn is_retained(self) -> bool {
        matches!(self, Self::Retained(_) | Self::Started { origin: 1.., .. })
    }
}

pub(super) struct StandbyRenderer {
    renderer: SpatialRenderer,
    pub(super) route: u32,
    rate: usize,
    lead: usize,
    warming: Option<StandbyWarmup>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum OpenAuthorization {
    Playing,
    PausedSameOutput,
    Revoked,
}

pub(super) enum OpenResult {
    Ready(Relay),
    Dormant(StandbyRenderer),
    Cancelled,
}

fn paused_preroll_authorized(
    authorization: OpenAuthorization,
    now: Instant,
    deadline: Instant,
) -> bool {
    authorization == OpenAuthorization::PausedSameOutput && now < deadline
}

struct StandbyWarmup {
    origin: u64,
    started: Instant,
    silence_end: u64,
    scratch: Vec<[f32; 2]>,
    paused_deadline: Option<Instant>,
    running_handoff_deadline: Option<Instant>,
    retained: Option<PausedZeroClock>,
    retained_pause: bool,
    paused_format: Option<PausedRouteFormat>,
}

impl StandbyWarmup {
    fn requires_expiry_suspend(&self, now: Instant) -> bool {
        !self.retained_pause && self.paused_deadline.is_some_and(|deadline| now >= deadline)
    }
    fn is_current(&self) -> bool {
        let now = Instant::now();
        self.running_handoff_deadline
            .is_none_or(|deadline| now < deadline)
            && (self.retained_pause || self.paused_deadline.is_none_or(|deadline| now < deadline))
    }

    fn position(&self, through: Option<u64>, now: Instant, rate: u32) -> Option<u64> {
        through.filter(|&position| {
            self.is_current()
                && self.retained.is_none()
                && warm_position_is_current(
                    self.origin,
                    position,
                    now.saturating_duration_since(self.started),
                    rate,
                )
        })
    }
}

// AirPods can hold the new clock at zero for about 190 ms after the native
// queue starts. Count work already done since requesting that startup.
const NEXT_PREROLL_LIMIT: Duration = Duration::from_millis(250);
const NEXT_PREROLL_MAX_WAIT: Duration = Duration::from_millis(210);
const PAUSED_NEXT_PREROLL_WAIT: Duration = Duration::from_millis(170);
const RUNNING_NEXT_CLOCK_WAIT: Duration = Duration::from_millis(90);
// A promptly confirmed Seek is faster if it opens the fresh tap immediately.
// Only an already-running silent queue can justify a short wait.
const SEEK_PREROLL_MIN_AGE: Duration = Duration::from_millis(150);
const SEEK_PREROLL_MAX_WAIT: Duration = Duration::from_millis(70);
const SEEK_PREROLL_AFTER_RETIREMENT: Duration = Duration::from_millis(250);

fn next_preroll_deadline(now: Instant, started: Instant) -> Instant {
    (started + NEXT_PREROLL_LIMIT).min(now + NEXT_PREROLL_MAX_WAIT)
}

fn observe_authorized_preroll_tail<T>(
    authorized: &impl Fn() -> bool,
    observe: impl FnOnce() -> Option<T>,
) -> Option<T> {
    if !authorized() {
        return None;
    }
    let tail = observe();
    if authorized() { tail } else { None }
}

fn seek_preroll_deadline(now: Instant, released_at: Instant, started: Instant) -> Option<Instant> {
    let deadline = (released_at + SEEK_PREROLL_AFTER_RETIREMENT).min(now + SEEK_PREROLL_MAX_WAIT);
    (now.saturating_duration_since(started) >= SEEK_PREROLL_MIN_AGE && now < deadline)
        .then_some(deadline)
}

impl StandbyRenderer {
    pub(super) fn has_preroll(&self) -> bool {
        self.warming.is_some()
    }

    fn keeps_running_handoff(&self) -> bool {
        self.warming
            .as_ref()
            .is_some_and(|warming| warming.running_handoff_deadline.is_some())
    }

    fn begin_cold(
        rate: u32,
        route: u32,
        lead: usize,
        early: bool,
        can_start: impl FnOnce() -> bool,
    ) -> Result<Option<Self>, Failure> {
        live_trace::event(
            if early {
                "renderer_early_cold_prepare"
            } else {
                "renderer_cold_prepare"
            },
            None,
        );
        let renderer = live_trace::measure(
            "renderer_cold_open_start",
            "renderer_cold_open_done",
            || SpatialRenderer::open(rate),
        )
        .map_err(|_| Failure::DirectRenderer)?;
        let silence = vec![[0.0; 2]; lead + rate as usize / 12];
        live_trace::measure(
            "renderer_cold_enqueue_start",
            "renderer_cold_enqueue_done",
            || renderer.enqueue(0, &silence),
        )
        .map_err(|_| Failure::DirectRenderer)?;
        if !live_trace::measure(
            "renderer_cold_launch_check_start",
            "renderer_cold_launch_check_done",
            can_start,
        ) {
            return Ok(None);
        }
        let started = Instant::now();
        let (start_event, returned_event) = if early {
            (
                "renderer_early_cold_start",
                "renderer_early_cold_start_returned",
            )
        } else {
            ("renderer_cold_start", "renderer_cold_start_returned")
        };
        live_trace::event(start_event, None);
        renderer.start_at(0);
        live_trace::event(returned_event, None);
        Ok(Some(Self {
            renderer,
            route,
            rate: rate as usize,
            lead,
            warming: Some(StandbyWarmup {
                origin: 0,
                started,
                silence_end: silence.len() as u64,
                scratch: silence,
                paused_deadline: None,
                running_handoff_deadline: None,
                retained: None,
                retained_pause: false,
                paused_format: None,
            }),
        }))
    }

    pub(super) fn keep_paused_until(&mut self, deadline: Instant) {
        if let Some(warming) = &mut self.warming {
            warming.paused_deadline = Some(deadline);
        }
    }

    pub(super) fn paused_ready(&self) -> bool {
        self.warm_tail(self.lead).is_some()
    }

    pub(super) fn has_retained_pause(&self) -> bool {
        self.warming
            .as_ref()
            .is_some_and(|warmup| warmup.retained_pause)
    }

    pub(super) fn invalidate_retained_pause(&mut self) -> bool {
        if !self.has_retained_pause() {
            return false;
        }
        self.cancel_prewarm();
        live_trace::event("renderer_retained_pause_invalidated", None);
        true
    }

    pub(super) fn suspend_paused_silence(
        &mut self,
        authorized: impl Fn() -> bool,
    ) -> Result<(), Failure> {
        objc2::rc::autoreleasepool(|_| {
            if !authorized() || self.renderer.failed() || self.renderer.take_flush() {
                return Err(Failure::DirectRendererInterrupted);
            }
            let horizon = (self.lead + self.rate / 12) as u64;
            let warmup = self.warming.as_mut().ok_or(Failure::DirectOutputStalled)?;
            if let Some(retained) = warmup.retained {
                warmup.retained = Some(retained.observe(
                    self.renderer.presented_through(),
                    warmup.silence_end,
                    horizon,
                    Instant::now(),
                )?);
                return if authorized() {
                    Ok(())
                } else {
                    Err(Failure::DirectRendererInterrupted)
                };
            }
            let format = paused_route_format(self.route).ok_or(Failure::WaitingForRoute)?;
            if warmup
                .paused_format
                .is_some_and(|previous| previous != format)
            {
                return Err(Failure::WaitingForRoute);
            }
            let through = self
                .renderer
                .presented_through()
                .filter(|&through| {
                    crate::spatial::start_position_is_current(
                        warmup.origin,
                        through,
                        warmup.started.elapsed(),
                        self.rate as u32,
                    )
                })
                .ok_or(Failure::DirectOutputStalled)?;
            if through >= warmup.silence_end || warmup.silence_end - through > horizon {
                return Err(Failure::DirectOutputStalled);
            }
            if stereo_bluetooth_output() != Some(self.route) {
                return Err(Failure::WaitingForRoute);
            }
            self.renderer.set_muted(true);
            self.renderer.pause_preserving_time();
            if !authorized() || self.renderer.failed() || self.renderer.take_flush() {
                return Err(Failure::DirectRendererInterrupted);
            }
            if paused_route_format(self.route) != Some(format) {
                return Err(Failure::WaitingForRoute);
            }
            let sample = self
                .renderer
                .presented_through()
                .ok_or(Failure::DirectOutputStalled)?;
            warmup.retained = Some(PausedZeroClock::begin(sample, Instant::now()));
            warmup.retained_pause = true;
            warmup.paused_format = Some(format);
            live_trace::event("renderer_standby_silence_suspending", None);
            Ok(())
        })
    }

    pub(super) fn resume_paused_silence(
        &mut self,
        authorized: impl Fn() -> bool,
    ) -> Result<bool, Failure> {
        objc2::rc::autoreleasepool(|_| {
            if !authorized() {
                return Err(Failure::DirectRendererInterrupted);
            }
            if self
                .warming
                .as_ref()
                .is_some_and(|warmup| warmup.requires_expiry_suspend(Instant::now()))
            {
                // Resume can be the first worker turn after the old deadline.
                // Retire no queue and accept no old clock in that ordering.
                self.suspend_paused_silence(&authorized)?;
                // Observe the stopped anchor on a later worker turn.
                return Ok(false);
            }
            if self.renderer.failed() || self.renderer.take_flush() {
                return Err(Failure::DirectRendererInterrupted);
            }
            if stereo_bluetooth_output() != Some(self.route) {
                return Err(Failure::WaitingForRoute);
            }
            let horizon = (self.lead + self.rate / 12) as u64;
            let warmup = self.warming.as_mut().ok_or(Failure::DirectOutputStalled)?;
            if let Some(retained) = warmup.retained {
                if warmup.paused_format.is_none()
                    || paused_route_format(self.route) != warmup.paused_format
                {
                    return Err(Failure::WaitingForRoute);
                }
                let retained = retained.observe(
                    self.renderer.presented_through(),
                    warmup.silence_end,
                    horizon,
                    Instant::now(),
                )?;
                warmup.retained = Some(retained);
                let Some(anchor) = retained.anchor() else {
                    return Ok(false);
                };
                let target = anchor.checked_add(horizon).ok_or(Failure::DirectBuffer)?;
                if target > warmup.silence_end && self.renderer.is_ready() {
                    enqueue_silence(
                        &self.renderer,
                        &mut warmup.scratch,
                        &mut warmup.silence_end,
                        target,
                    )?;
                }
                self.renderer.set_muted(false);
                if !authorized() || paused_route_format(self.route) != warmup.paused_format {
                    return Err(Failure::DirectRendererInterrupted);
                }
                warmup.origin = anchor;
                warmup.started = Instant::now();
                self.renderer.resume_preserving_time();
                warmup.retained = None;
                live_trace::event("renderer_standby_retained_resume_started", None);
                return Ok(false);
            }
            if warmup.retained_pause
                && warmup.paused_format.is_some()
                && paused_route_format(self.route) != warmup.paused_format
            {
                return Err(Failure::WaitingForRoute);
            }
            let ready = self.keep_paused_warm()?;
            if !authorized() {
                return Err(Failure::DirectRendererInterrupted);
            }
            Ok(ready)
        })
    }

    pub(super) fn keep_paused_warm(&mut self) -> Result<bool, Failure> {
        if self.warming.as_ref().is_some_and(|warming| {
            warming.retained_pause
                && warming.retained.is_none()
                && warming.started.elapsed() >= COLD_START_LIMIT
        }) && !self.paused_ready()
        {
            return Err(Failure::DirectOutputStalled);
        }
        if self.warming.as_ref().is_some_and(|warming| {
            warming.started.elapsed() >= COLD_START_LIMIT
                && self.renderer.presented_through().unwrap_or_default() == 0
        }) {
            return Err(Failure::DirectOutputStalled);
        }
        self.warm_next()?;
        Ok(self.paused_ready())
    }

    pub(super) fn warm_next(&mut self) -> Result<(), Failure> {
        objc2::rc::autoreleasepool(|_| {
            if self.renderer.failed() || self.renderer.take_flush() {
                return Err(Failure::DirectRendererInterrupted);
            }
            let horizon = (self.lead + self.rate / 12) as u64;
            let through = self.renderer.presented_through();
            if let Some(warmup) = &mut self.warming {
                if !warmup.is_current() || warmup.retained.is_some() {
                    return Err(Failure::DirectOutputStalled);
                }
                let Some(through) = warmup.position(through, Instant::now(), self.rate as u32)
                else {
                    return Ok(());
                };
                if through >= warmup.silence_end {
                    return Err(Failure::DirectOutputStalled);
                }
                let target = through + horizon;
                if target.saturating_sub(warmup.silence_end) >= (self.rate / 25) as u64
                    && self.renderer.is_ready()
                {
                    enqueue_silence(
                        &self.renderer,
                        &mut warmup.scratch,
                        &mut warmup.silence_end,
                        target,
                    )?;
                }
            } else if through == Some(0) {
                // Reset is asynchronous. Never accept a positive time from
                // the outgoing track as proof of a warmed new epoch.
                let format = paused_route_format(self.route).ok_or(Failure::WaitingForRoute)?;
                let warmup = self.warming.insert(StandbyWarmup {
                    origin: 0,
                    started: Instant::now(),
                    silence_end: 0,
                    scratch: Vec::new(),
                    paused_deadline: None,
                    running_handoff_deadline: None,
                    retained: None,
                    retained_pause: false,
                    paused_format: Some(format),
                });
                self.renderer.set_muted(false);
                enqueue_silence(
                    &self.renderer,
                    &mut warmup.scratch,
                    &mut warmup.silence_end,
                    horizon,
                )?;
                if paused_route_format(self.route) != Some(format) {
                    return Err(Failure::WaitingForRoute);
                }
                warmup.started = Instant::now();
                self.renderer.start_at(0);
                live_trace::event("renderer_next_warm_started", None);
            }
            Ok(())
        })
    }

    pub(super) fn finish_requested_handoff(
        &mut self,
        rate: u32,
        route: u32,
        released_at: Instant,
        authorized: impl Fn() -> bool,
    ) -> Result<(), Failure> {
        if self.warming.is_none() {
            return self.finish_coalesced_handoff(rate, route, released_at, authorized);
        }
        if !authorized() {
            self.cancel_prewarm();
            return Ok(());
        }
        let format = paused_route_format(route).ok_or(Failure::WaitingForRoute)?;
        if self.warming.as_ref().is_some_and(|warming| {
            warming
                .paused_format
                .is_some_and(|previous| previous != format)
        }) {
            self.cancel_prewarm();
            return Err(Failure::WaitingForRoute);
        }
        // A natural successor can arrive a few milliseconds before the silent
        // clock starts advancing. Use the same original-start deadline as an
        // ordinary Next, rather than discarding a healthy starting queue.
        live_trace::event("renderer_existing_handoff_preroll_check", None);
        self.finish_warmup(rate, route, None, &authorized);
        if self.warming.is_some() && paused_route_format(route) != Some(format) {
            self.cancel_prewarm();
            return Err(Failure::WaitingForRoute);
        }
        // Exactly one wait path runs: a coalesced initializer never falls
        // through into a second, newly measured preroll wait.
        Ok(())
    }

    pub(super) fn finish_coalesced_handoff(
        &mut self,
        rate: u32,
        route: u32,
        released_at: Instant,
        authorized: impl Fn() -> bool,
    ) -> Result<(), Failure> {
        if self.warming.is_some() || self.route != route || self.rate != rate as usize {
            return Ok(());
        }
        let deadline = next_preroll_deadline(Instant::now(), released_at);
        if Instant::now() >= deadline || !authorized() {
            return Ok(());
        }
        let format = paused_route_format(route).ok_or(Failure::WaitingForRoute)?;
        if !authorized() {
            return Ok(());
        }
        live_trace::event("renderer_coalesced_preroll_started", None);
        while authorized() && Instant::now() < deadline {
            if let Err(error) = self.warm_next() {
                self.cancel_prewarm();
                return Err(error);
            }
            if self.warm_tail(self.lead).is_some() {
                if paused_route_format(route) != Some(format) {
                    self.cancel_prewarm();
                    return Err(Failure::WaitingForRoute);
                }
                if authorized() {
                    live_trace::event("renderer_coalesced_preroll_ready", None);
                    return Ok(());
                }
                break;
            }
            std::thread::park_timeout(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(4)),
            );
        }
        // Route checks in the last authorization poll can cross the wait
        // deadline just as the native clock begins advancing. Observe the
        // already queued tail once more, as finish_warmup does; never enqueue
        // or extend either deadline here. Request authority must still hold.
        use std::cell::Cell;
        let failed = self.renderer.failed();
        let flushed = self.renderer.take_flush();
        let format_valid = paused_route_format(route) == Some(format);
        let authority_before = Cell::new(None);
        let authority_after = Cell::new(None);
        let through = Cell::new(None);
        let elapsed = Cell::new(None);
        let final_authority = || {
            let current = authorized();
            if authority_before.get().is_none() {
                authority_before.set(Some(current));
            } else {
                authority_after.set(Some(current));
            }
            current
        };
        let tail = (!failed && !flushed && format_valid)
            .then(|| {
                observe_authorized_preroll_tail(&final_authority, || {
                    let position = self.renderer.presented_through();
                    let observed_at = Instant::now();
                    through.set(position);
                    elapsed.set(
                        self.warming
                            .as_ref()
                            .map(|warming| observed_at.saturating_duration_since(warming.started)),
                    );
                    self.warm_tail_at(self.lead, position, observed_at)
                })
            })
            .flatten();
        live_trace::handoff_final(live_trace::HandoffFinalObservation {
            failed,
            flushed,
            format_valid,
            authority_before: authority_before.get(),
            authority_after: authority_after.get(),
            through: through.get(),
            origin: self.warming.as_ref().map(|warming| warming.origin),
            silence_end: self.warming.as_ref().map(|warming| warming.silence_end),
            lead: self.lead,
            rate: self.rate,
            elapsed: elapsed.get(),
            ready: tail.is_some(),
        });
        if tail.is_some() {
            live_trace::event("renderer_coalesced_preroll_ready_at_deadline", None);
            return Ok(());
        }
        // No positive outgoing clock is accepted while the asynchronous reset
        // settles. Expiry/revocation returns to the ordinary cleared cold path.
        self.cancel_prewarm();
        live_trace::event("renderer_coalesced_preroll_abandoned", None);
        Ok(())
    }

    fn warm_tail(&self, lead: usize) -> Option<u64> {
        self.warm_tail_at(lead, self.renderer.presented_through(), Instant::now())
    }

    fn warm_tail_at(&self, lead: usize, through: Option<u64>, now: Instant) -> Option<u64> {
        let warming = self
            .warming
            .as_ref()
            .filter(|warmup| warmup.is_current() && warmup.retained.is_none())?;
        let through = warming.position(through, now, self.rate as u32)?;
        (through.saturating_add(lead as u64) <= warming.silence_end).then_some(warming.silence_end)
    }

    fn natural_warm_tail(&mut self, lead: usize, authorized: impl Fn() -> bool) -> Option<u64> {
        if !authorized() {
            live_trace::event("renderer_natural_tail_revoked", None);
            return None;
        }
        let Some(warming) = self
            .warming
            .as_mut()
            .filter(|warming| warming.is_current() && !warming.retained_pause)
        else {
            live_trace::event("renderer_natural_tail_missing", None);
            return None;
        };
        let Some(through) = warming.position(
            self.renderer.presented_through(),
            Instant::now(),
            self.rate as u32,
        ) else {
            live_trace::event("renderer_natural_tail_clock", None);
            return None;
        };
        let remaining = warming.silence_end.saturating_sub(through);
        let lead = lead as u64;
        live_trace::event("renderer_natural_tail_remaining", Some(remaining));
        live_trace::event("renderer_natural_tail_lead", Some(lead));
        if remaining >= lead {
            return Some(warming.silence_end);
        }
        if remaining == 0 || !self.renderer.is_ready() {
            live_trace::event("renderer_natural_tail_unavailable", None);
            return None;
        }
        if self.renderer.failed() || self.renderer.take_flush() || !authorized() {
            live_trace::event("renderer_natural_tail_revoked", None);
            return None;
        }
        // The tap was prepared while the renderer consumed its reserve. Extend
        // the same silence epoch once, up to the ordinary standby horizon.
        let target = through.saturating_add(lead + (self.rate / 12) as u64);
        let added = target - warming.silence_end;
        live_trace::event("renderer_natural_tail_top_up_start", Some(added));
        if enqueue_silence(
            &self.renderer,
            &mut warming.scratch,
            &mut warming.silence_end,
            target,
        )
        .is_err()
        {
            live_trace::event("renderer_natural_tail_top_up_failed", None);
            return None;
        }
        live_trace::event("renderer_natural_tail_top_up_done", Some(added));
        if !authorized() || self.renderer.failed() || self.renderer.take_flush() {
            return None;
        }
        self.warm_tail(lead as usize)
    }

    pub(super) fn finish_warmup(
        &mut self,
        rate: u32,
        route: u32,
        seek_released_at: Option<Instant>,
        authorized: impl Fn() -> bool,
    ) {
        let Some(warmup) = self.warming.as_ref() else {
            if seek_released_at.is_some() {
                live_trace::event("renderer_seek_prefers_cold", None);
            }
            return;
        };
        if self.route != route || self.rate != rate as usize {
            self.cancel_prewarm();
            return;
        }
        let warm_started = warmup.started;
        let ordinary_next = seek_released_at.is_none() && warmup.paused_deadline.is_none();
        let deadline = if let Some(released_at) = seek_released_at {
            if !authorized() || self.renderer.failed() || self.renderer.take_flush() {
                self.cancel_prewarm();
                return;
            }
            // A healthy current tail is usable even when its preroll budget
            // has elapsed. The handoff authorization still has its own limit.
            if self.warm_tail(self.lead).is_some() && authorized() {
                return;
            }
            let Some(deadline) = seek_preroll_deadline(Instant::now(), released_at, warm_started)
            else {
                live_trace::event("renderer_seek_prefers_cold", None);
                self.cancel_prewarm();
                return;
            };
            deadline
        } else if ordinary_next {
            next_preroll_deadline(Instant::now(), warm_started)
        } else {
            Instant::now() + PAUSED_NEXT_PREROLL_WAIT
        };
        let mut waited = false;
        while authorized() {
            if ordinary_next && Instant::now() >= deadline {
                if !self.renderer.failed()
                    && !self.renderer.take_flush()
                    && self.warm_tail(self.lead).is_some()
                    && authorized()
                {
                    return;
                }
                break;
            }
            if self.warm_next().is_err() {
                break;
            }
            if self.warm_tail(self.lead).is_some() && authorized() {
                return;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            if !waited {
                live_trace::event("renderer_next_waiting_for_preroll", None);
                waited = true;
            }
            std::thread::park_timeout(remaining.min(Duration::from_millis(4)));
        }
        self.cancel_prewarm();
    }

    pub(super) fn cancel_prewarm(&mut self) {
        if self.warming.take().is_some() {
            objc2::rc::autoreleasepool(|_| {
                self.renderer.reset_time();
                self.renderer.discard_enqueued();
                self.renderer.set_muted(true);
                self.renderer.take_flush();
            });
        }
    }
}

fn enqueue_silence(
    renderer: &SpatialRenderer,
    scratch: &mut Vec<[f32; 2]>,
    from: &mut u64,
    through: u64,
) -> Result<(), Failure> {
    scratch.clear();
    scratch.resize((through - *from) as usize, [0.0; 2]);
    renderer
        .enqueue(*from, scratch)
        .map_err(|_| Failure::DirectRendererInterrupted)?;
    *from = through;
    Ok(())
}

fn faded(mut frame: Sample, start: Option<u64>, fade_frames: usize) -> Sample {
    if let Some(start) = start {
        let elapsed = frame.position.saturating_sub(start);
        if elapsed < fade_frames as u64 {
            let t = elapsed as f32 / fade_frames as f32;
            let gain = t * t * (3.0 - 2.0 * t);
            frame.samples = frame.samples.map(|value| value * gain);
        }
    }
    frame
}

impl Relay {
    #[cfg(test)]
    pub(super) fn open(
        rate: u32,
        route: u32,
        standby: Option<StandbyRenderer>,
        authorized: impl Fn() -> bool,
    ) -> Result<Option<Self>, Failure> {
        Self::open_for_session(rate, route, standby, None, None, None, || {
            if authorized() {
                OpenAuthorization::Playing
            } else {
                OpenAuthorization::Revoked
            }
        })
        .map(|result| match result {
            OpenResult::Ready(relay) => Some(relay),
            OpenResult::Cancelled => None,
            OpenResult::Dormant(_) => unreachable!("test open cannot retain a paused renderer"),
        })
    }

    pub(super) fn start_paused_preroll(
        rate: u32,
        route: u32,
        deadline: Instant,
        authorization: impl Fn() -> OpenAuthorization,
    ) -> Result<Option<StandbyRenderer>, Failure> {
        let authorized = || paused_preroll_authorized(authorization(), Instant::now(), deadline);
        if !authorized() {
            return Ok(None);
        }
        let format = paused_route_format(route).ok_or(Failure::WaitingForRoute)?;
        let lead = route_lead(rate, route)?;
        if !authorized() {
            return Ok(None);
        }
        let Some(mut standby) = StandbyRenderer::begin_cold(rate, route, lead, true, || {
            authorized() && paused_route_format(route) == Some(format) && authorized()
        })?
        else {
            return Ok(None);
        };
        if !authorized() || paused_route_format(route) != Some(format) {
            return Ok(None);
        }
        standby.keep_paused_until(deadline);
        if let Some(warmup) = &mut standby.warming {
            warmup.paused_format = Some(format);
        }
        live_trace::event("renderer_confirmed_pause_started", None);
        Ok(Some(standby))
    }

    pub(super) fn start_cold_preroll(
        rate: u32,
        route: u32,
        authorization: impl Fn() -> OpenAuthorization,
    ) -> Result<Option<StandbyRenderer>, Failure> {
        let playing = || {
            live_trace::measure(
                "renderer_early_lease_check_start",
                "renderer_early_lease_check_done",
                &authorization,
            ) == OpenAuthorization::Playing
        };
        let route_matches = || {
            live_trace::measure(
                "renderer_early_route_check_start",
                "renderer_early_route_check_done",
                crate::spatial::bluetooth_output_id,
            ) == Some(route)
        };
        if !playing() {
            return Ok(None);
        }
        if !route_matches() {
            return Err(Failure::WaitingForRoute);
        }
        let lead = live_trace::measure(
            "renderer_early_route_lead_start",
            "renderer_early_route_lead_done",
            || route_lead(rate, route),
        )?;
        if !playing() {
            return Ok(None);
        }
        // The device continues rendering silence while this worker prepares
        // the private aggregate. The tap has no registered IOProc yet.
        StandbyRenderer::begin_cold(rate, route, lead, true, || {
            playing() && route_matches() && playing()
        })
    }

    pub(super) fn open_for_session(
        rate: u32,
        route: u32,
        standby: Option<StandbyRenderer>,
        early_preroll: Option<StandbyRenderer>,
        natural_handoff: Option<&dyn Fn() -> bool>,
        running_handoff: Option<&dyn Fn() -> bool>,
        authorization: impl Fn() -> OpenAuthorization,
    ) -> Result<OpenResult, Failure> {
        let authorized = authorization();
        if authorized != OpenAuthorization::Playing
            && (authorized != OpenAuthorization::PausedSameOutput || early_preroll.is_none())
        {
            return Ok(OpenResult::Cancelled);
        }
        if live_trace::measure(
            "renderer_route_check_start",
            "renderer_route_check_done",
            default_output_id,
        ) != Some(route)
        {
            return Err(Failure::WaitingForRoute);
        }
        let lead = live_trace::measure(
            "renderer_route_lead_start",
            "renderer_route_lead_done",
            || route_lead(rate, route),
        )?;
        if let Some(mut early) = early_preroll.filter(|early| {
            standby.is_none() && early.route == route && early.rate == rate as usize
        }) {
            early.lead = lead;
            if let Some(result) = Self::finish_early_cold(early, &authorization)? {
                return Ok(result);
            }
            if stereo_bluetooth_output() != Some(route) {
                return Err(Failure::WaitingForRoute);
            }
        }
        if authorization() != OpenAuthorization::Playing {
            return Ok(OpenResult::Cancelled);
        }
        let (renderer, warm_frames, muted, prepared_silence_started) = if let Some(mut standby) =
            standby.filter(|standby| {
                standby.route == route
                    && standby.rate == rate as usize
                    && !standby.renderer.failed()
                    && standby.warming.as_ref().is_none_or(|warmup| {
                        warmup
                            .paused_format
                            .is_none_or(|format| paused_route_format(route) == Some(format))
                    })
            }) {
            live_trace::event("renderer_reused", None);
            let running = standby.keeps_running_handoff();
            let permits_running = || {
                authorization() == OpenAuthorization::Playing
                    && running_handoff.is_some_and(|current| current())
            };
            let mut warm_tail = if running && !permits_running() {
                None
            } else if standby.renderer.take_flush() {
                if natural_handoff.is_some() {
                    live_trace::event("renderer_natural_tail_flushed", None);
                }
                None
            } else if let Some(handoff_is_current) = natural_handoff {
                standby.natural_warm_tail(lead, || {
                    default_output_id() == Some(route)
                        && authorization() == OpenAuthorization::Playing
                        && handoff_is_current()
                })
            } else {
                standby.warm_tail(lead)
            };
            if running {
                warm_tail = warm_tail.filter(|_| {
                    paused_route_format(route)
                        == standby
                            .warming
                            .as_ref()
                            .and_then(|warming| warming.paused_format)
                        && !standby.renderer.failed()
                        && !standby.renderer.take_flush()
                        && permits_running()
                });
                if warm_tail.is_some() {
                    live_trace::event("renderer_running_next_reused", None);
                }
            }
            if warm_tail.is_some() {
                live_trace::event("renderer_next_warm_reused", None);
            } else {
                standby.cancel_prewarm();
            }
            let prepared_silence_started = if running {
                // Pause can still arrive between readiness and capture IO.
                // Do not carry a running track handoff across that Pause.
                None
            } else {
                warm_tail.and_then(|_| {
                    standby
                        .warming
                        .as_ref()
                        .map(|warmup| (warmup.origin, warmup.started))
                })
            };
            (
                standby.renderer,
                warm_tail.unwrap_or_default(),
                warm_tail.is_none(),
                prepared_silence_started,
            )
        } else {
            // The first AVFoundation start can block while Bluetooth activates.
            // Prime with silence while Soloist still plays directly.
            let cold = StandbyRenderer::begin_cold(rate, route, lead, false, || true)?
                .expect("ordinary cold start permits silence");
            let warming = cold.warming.as_ref().expect("cold start created silence");
            loop {
                match authorization() {
                    OpenAuthorization::Playing => {}
                    OpenAuthorization::PausedSameOutput
                        if !cold.renderer.failed()
                            && !cold.renderer.take_flush()
                            && default_output_id() == Some(route) =>
                    {
                        return Ok(OpenResult::Dormant(cold));
                    }
                    _ => {
                        live_trace::event("renderer_cold_cancelled", None);
                        return Ok(OpenResult::Cancelled);
                    }
                }
                if cold.renderer.presented_through().unwrap_or_default() > 0 {
                    break;
                }
                // On idle AirPods AudioQueue took 1.125 s just to start I/O.
                // Its media clock can stay negative during additional preroll.
                let failed = cold.renderer.failed();
                if failed || warming.started.elapsed() >= COLD_START_LIMIT {
                    live_trace::event(
                        if failed {
                            "renderer_cold_failed"
                        } else {
                            "renderer_cold_timeout"
                        },
                        None,
                    );
                    return Err(interruption(
                        route,
                        default_output_id(),
                        Failure::DirectRenderer,
                    ));
                }
                std::thread::sleep(Duration::from_millis(4));
            }
            live_trace::event("renderer_cold_ready", None);
            (
                cold.renderer,
                warming.silence_end,
                false,
                Some((0, warming.started)),
            )
        };
        Ok(if authorization() == OpenAuthorization::Playing {
            OpenResult::Ready(Self::new(
                renderer,
                route,
                rate as usize,
                lead,
                warm_frames,
                muted,
                prepared_silence_started,
            ))
        } else {
            OpenResult::Cancelled
        })
    }

    fn finish_early_cold(
        mut early: StandbyRenderer,
        authorization: impl Fn() -> OpenAuthorization,
    ) -> Result<Option<OpenResult>, Failure> {
        let started = early
            .warming
            .as_ref()
            .expect("early cold started silence")
            .started;
        loop {
            let authorized = authorization();
            if authorized == OpenAuthorization::Revoked {
                live_trace::event("renderer_early_cold_cancelled", None);
                return Ok(Some(OpenResult::Cancelled));
            }
            if authorized == OpenAuthorization::PausedSameOutput {
                // Any extension through Pause requires the worker's separate
                // confirmed-pause fence. Do not append zeros here.
                if stereo_bluetooth_output() == Some(early.route)
                    && authorization() == OpenAuthorization::PausedSameOutput
                    && !early.renderer.failed()
                    && !early.renderer.take_flush()
                {
                    return Ok(Some(OpenResult::Dormant(early)));
                }
                return Ok(Some(OpenResult::Cancelled));
            }
            if early.renderer.failed() || early.renderer.take_flush() {
                live_trace::event("renderer_early_cold_abandoned", None);
                return Ok(None);
            }
            let mut tail = early.warm_tail(early.lead);
            if tail.is_none() {
                if early.warm_next().is_err() {
                    live_trace::event("renderer_early_cold_abandoned", None);
                    return Ok(None);
                }
                tail = early.warm_tail(early.lead);
            }
            if let Some(tail) = tail {
                if stereo_bluetooth_output() != Some(early.route) {
                    return Err(Failure::WaitingForRoute);
                }
                // HAL may block. Recheck the advancing tail and lease after
                // revalidating the physical route.
                if early.renderer.failed() || early.renderer.take_flush() {
                    live_trace::event("renderer_early_cold_abandoned", None);
                    return Ok(None);
                }
                if authorization() == OpenAuthorization::Playing
                    && early.warm_tail(early.lead) == Some(tail)
                {
                    live_trace::event("renderer_early_cold_ready", None);
                    return Ok(Some(OpenResult::Ready(Self::new(
                        early.renderer,
                        early.route,
                        early.rate,
                        early.lead,
                        tail,
                        false,
                        Some((0, started)),
                    ))));
                }
            }
            if started.elapsed() >= COLD_START_LIMIT {
                live_trace::event("renderer_early_cold_timeout", None);
                return Err(interruption(
                    early.route,
                    default_output_id(),
                    Failure::DirectRenderer,
                ));
            }
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    fn new(
        renderer: SpatialRenderer,
        route: u32,
        rate: usize,
        lead: usize,
        warm_frames: u64,
        muted: bool,
        prepared_silence_started: Option<(u64, Instant)>,
    ) -> Self {
        Self {
            renderer,
            queue: RelayQueue::new(lead + rate / 5),
            scratch: Vec::with_capacity(BATCH),
            route,
            rate,
            lead,
            warm_frames,
            clock_map: None,
            presented: false,
            submitted_through: None,
            last_progress: Instant::now(),
            last_input: Instant::now(),
            last_observed_input: None,
            resubmit: false,
            fade_from: None,
            muted,
            needs_start: muted,
            prepared_silence_started,
            resume_warmup: None,
            paused_format: None,
            signal_trace: live_trace::enabled().then(SignalTrace::default),
        }
    }

    pub(super) fn push(&mut self, sample: Sample) -> Result<(), Failure> {
        self.queue.push(sample)?;
        if let Some(trace) = &mut self.signal_trace {
            if !trace.first_input {
                trace.first_input = true;
                live_trace::event("tap_first_input", Some(sample.position));
                live_trace::event("tap_input_rate_hz", Some(self.rate as u64));
            }
            if !trace.input && live_trace::signal(sample.samples) {
                trace.input = true;
                live_trace::event("tap_signal", Some(sample.position));
            }
        }
        Ok(())
    }

    pub(super) fn can_accept(&self) -> bool {
        self.queue.frames.len() < self.queue.limit
    }

    pub(super) fn capture_started(&mut self) {
        self.prepared_silence_started = None;
        self.last_progress = Instant::now();
        self.last_input = self.last_progress;
    }

    pub(super) fn has_retained_pause(&self) -> bool {
        self.resume_warmup.is_some_and(ResumeWarmup::is_retained)
    }

    pub(super) fn invalidate_retained_pause(&mut self) -> bool {
        if !self.has_retained_pause() {
            return false;
        }
        self.pause();
        live_trace::event("renderer_retained_pause_invalidated", None);
        true
    }

    fn owns_only_native_silence(&self) -> bool {
        !self.needs_start
            && self.clock_map.is_none()
            && self.submitted_through.is_none()
            && !self.presented
            && !self.resubmit
            && self.queue.submitted == 0
    }

    pub(super) fn suspend_paused_silence(
        &mut self,
        authorized: impl Fn() -> bool,
    ) -> Result<(), Failure> {
        objc2::rc::autoreleasepool(|_| {
            if !authorized()
                || !self.owns_only_native_silence()
                || !self.queue.frames.is_empty()
                || self.queue.last_capture.is_some()
                || self.last_observed_input.is_some()
                || self.renderer.failed()
                || self.renderer.take_flush()
            {
                return Err(Failure::DirectRendererInterrupted);
            }
            if matches!(self.resume_warmup, Some(ResumeWarmup::Retained(_))) {
                self.observe_suspended_silence()?;
                return if authorized() {
                    Ok(())
                } else {
                    Err(Failure::DirectRendererInterrupted)
                };
            }
            let format = paused_route_format(self.route).ok_or(Failure::WaitingForRoute)?;
            if self
                .paused_format
                .is_some_and(|previous| previous != format)
            {
                return Err(Failure::WaitingForRoute);
            }
            let through = self
                .resume_preroll_position(self.renderer.presented_through())
                .ok_or(Failure::DirectOutputStalled)?;
            let horizon = (self.lead + self.rate / 12) as u64;
            if through >= self.warm_frames || self.warm_frames - through > horizon {
                return Err(Failure::DirectOutputStalled);
            }
            if stereo_bluetooth_output() != Some(self.route) {
                return Err(Failure::WaitingForRoute);
            }
            self.silence();
            self.renderer.pause_preserving_time();
            if !authorized() || self.renderer.failed() || self.renderer.take_flush() {
                return Err(Failure::DirectRendererInterrupted);
            }
            if paused_route_format(self.route) != Some(format) {
                return Err(Failure::WaitingForRoute);
            }
            let sample = self
                .renderer
                .presented_through()
                .ok_or(Failure::DirectOutputStalled)?;
            self.resume_warmup = Some(ResumeWarmup::Retained(PausedZeroClock::begin(
                sample,
                Instant::now(),
            )));
            self.paused_format = Some(format);
            live_trace::event("renderer_pause_silence_suspending", None);
            Ok(())
        })
    }

    fn observe_suspended_silence(&mut self) -> Result<Option<u64>, Failure> {
        let Some(ResumeWarmup::Retained(retained)) = self.resume_warmup else {
            return Ok(None);
        };
        let observed = retained.observe(
            self.renderer.presented_through(),
            self.warm_frames,
            (self.lead + self.rate / 12) as u64,
            Instant::now(),
        )?;
        if retained.anchor().is_none() && observed.anchor().is_some() {
            live_trace::event("renderer_pause_silence_suspended", None);
        }
        self.resume_warmup = Some(ResumeWarmup::Retained(observed));
        Ok(observed.anchor())
    }

    fn restart_retained_silence(&mut self) -> Result<bool, Failure> {
        if !matches!(self.resume_warmup, Some(ResumeWarmup::Retained(_))) {
            return Ok(false);
        }
        if !self.owns_only_native_silence() || self.renderer.failed() || self.renderer.take_flush()
        {
            return Err(Failure::DirectRendererInterrupted);
        }
        if stereo_bluetooth_output() != Some(self.route) {
            return Err(Failure::WaitingForRoute);
        }
        if self.paused_format.is_none() || paused_route_format(self.route) != self.paused_format {
            return Err(Failure::WaitingForRoute);
        }
        let Some(anchor) = self.observe_suspended_silence()? else {
            return Ok(true);
        };
        let target = anchor
            .checked_add((self.lead + self.rate / 12) as u64)
            .ok_or(Failure::DirectBuffer)?;
        if target > self.warm_frames && self.renderer.is_ready() {
            self.enqueue_silence(target)?;
        }
        self.renderer.set_muted(false);
        self.muted = false;
        if paused_route_format(self.route) != self.paused_format {
            return Err(Failure::WaitingForRoute);
        }
        self.resume_warmup = Some(ResumeWarmup::Started {
            origin: anchor,
            started: Instant::now(),
        });
        self.renderer.resume_preserving_time();
        self.last_progress = Instant::now();
        live_trace::event("renderer_retained_resume_started", None);
        Ok(true)
    }

    pub(super) fn into_prepared_pause(self) -> Option<StandbyRenderer> {
        let (origin, started) = self.prepared_silence_started?;
        if self.muted
            || self.needs_start
            || self.clock_map.is_some()
            || self.submitted_through.is_some()
            || !self.queue.frames.is_empty()
            || self.queue.submitted != 0
            || self.queue.last_capture.is_some()
            || stereo_bluetooth_output() != Some(self.route)
            || self.renderer.failed()
            || self.renderer.take_flush()
        {
            return None;
        }
        let through = self.renderer.presented_through()?;
        if through >= self.warm_frames
            || !crate::spatial::start_position_is_current(
                origin,
                through,
                started.elapsed(),
                self.rate as u32,
            )
        {
            return None;
        }
        Some(StandbyRenderer {
            renderer: self.renderer,
            route: self.route,
            rate: self.rate,
            lead: self.lead,
            warming: Some(StandbyWarmup {
                origin,
                started,
                silence_end: self.warm_frames,
                scratch: self.scratch,
                paused_deadline: None,
                running_handoff_deadline: None,
                retained: None,
                retained_pause: origin > 0,
                paused_format: None,
            }),
        })
    }

    pub(super) fn silence(&mut self) {
        if !self.muted {
            self.renderer.set_muted(true);
            self.muted = true;
            live_trace::event("renderer_muted", None);
        }
    }

    pub(super) fn into_running_next_standby(
        mut self,
        deadline: Instant,
        authorized: impl Fn() -> bool,
    ) -> Option<StandbyRenderer> {
        let Some(warming) = self.prepare_running_next(deadline, authorized) else {
            return self.into_standby(None);
        };
        Some(StandbyRenderer {
            renderer: self.renderer,
            route: self.route,
            rate: self.rate,
            lead: self.lead,
            warming: Some(warming),
        })
    }

    fn prepare_running_next(
        &mut self,
        deadline: Instant,
        authorized: impl Fn() -> bool,
    ) -> Option<StandbyWarmup> {
        let current = || Instant::now() < deadline && authorized();
        if !self.presented || self.clock_map.is_none() || !current() {
            return None;
        }
        let format = paused_route_format(self.route)?;
        let outgoing_cursor = self.renderer.presented_through().filter(|&at| at > 0)?;
        let retired_at = Instant::now();
        self.silence();
        if self.renderer.failed() || self.renderer.take_flush() || !current() {
            return None;
        }
        // Flush all outgoing PCM while muted. Keep only the device clock;
        // the old Relay queue and capture never enter the new generation.
        self.renderer.discard_enqueued();
        if paused_route_format(self.route) != Some(format)
            || self.renderer.failed()
            || self.renderer.take_flush()
            || !current()
        {
            return None;
        }
        let anchor = self.renderer.presented_through().filter(|&at| {
            at == outgoing_cursor
                || warm_position_is_current(
                    outgoing_cursor,
                    at,
                    retired_at.elapsed(),
                    self.rate as u32,
                )
        })?;
        let started = Instant::now();
        let mut end = anchor.checked_add(self.lead as u64)?;
        let target = end.checked_add((self.rate / 25) as u64)?;
        enqueue_silence(&self.renderer, &mut self.scratch, &mut end, target).ok()?;
        let advancing =
            |at| warm_position_is_current(anchor, at, started.elapsed(), self.rate as u32);
        let wait_until = (started + RUNNING_NEXT_CLOCK_WAIT).min(deadline);
        while Instant::now() < wait_until && current() {
            if self.renderer.failed() || self.renderer.take_flush() {
                return None;
            }
            if let Some(before_unmute) = self
                .renderer
                .presented_through()
                .filter(|&at| advancing(at) && at.saturating_add(self.lead as u64) <= end)
            {
                self.renderer.set_muted(false);
                self.muted = false;
                let validated_cursor = |previous| {
                    if paused_route_format(self.route) != Some(format) {
                        live_trace::event("renderer_running_next_rejected_route", None);
                        return None;
                    }
                    if self.renderer.failed() || self.renderer.take_flush() {
                        live_trace::event("renderer_running_next_rejected_flush", None);
                        return None;
                    }
                    if !current() {
                        live_trace::event("renderer_running_next_rejected_token", None);
                        return None;
                    }
                    let cursor = self.renderer.presented_through();
                    cursor
                        .filter(|&at| at >= previous && advancing(at))
                        .or_else(|| {
                            live_trace::event("renderer_running_next_rejected_cursor", cursor);
                            live_trace::event("renderer_running_next_cursor_anchor", Some(anchor));
                            live_trace::event("renderer_running_next_cursor_prior", Some(previous));
                            live_trace::event(
                                "renderer_running_next_cursor_elapsed_us",
                                Some(started.elapsed().as_micros() as u64),
                            );
                            None
                        })
                };
                let cursor = validated_cursor(before_unmute)?;
                let needed_end = cursor.checked_add(self.lead as u64)?;
                if needed_end > end {
                    // Unmute can block while the device clock consumes the
                    // reserve. Extend the existing silence once if it still
                    // runs ahead of the cursor and the same route can accept it.
                    if cursor >= end || !self.renderer.is_ready() || !current() {
                        live_trace::event("renderer_running_next_rejected_tail", Some(end));
                        return None;
                    }
                    let extra = needed_end - end;
                    let target = needed_end.checked_add((self.rate / 200) as u64)?;
                    enqueue_silence(&self.renderer, &mut self.scratch, &mut end, target).ok()?;
                    if validated_cursor(cursor)?.saturating_add(self.lead as u64) > end {
                        live_trace::event("renderer_running_next_rejected_top_up", Some(end));
                        return None;
                    }
                    live_trace::event("renderer_running_next_unmute_top_up", Some(extra));
                }
                live_trace::event("renderer_running_next_ready", None);
                return Some(StandbyWarmup {
                    origin: anchor,
                    started,
                    silence_end: end,
                    scratch: std::mem::take(&mut self.scratch),
                    paused_deadline: None,
                    running_handoff_deadline: Some(deadline),
                    retained: None,
                    retained_pause: false,
                    paused_format: Some(format),
                });
            }
            std::thread::park_timeout(Duration::from_millis(2));
        }
        None
    }

    pub(super) fn into_standby(
        mut self,
        keep_paused_until: Option<Instant>,
    ) -> Option<StandbyRenderer> {
        let warming = if let Some(deadline) = keep_paused_until
            && Instant::now() < deadline
            && let Some(ResumeWarmup::Started { origin: 0, started }) = self.resume_warmup
            && !self.muted
            && !self.needs_start
            && self.clock_map.is_none()
            && self.submitted_through.is_none()
            && self.queue.frames.is_empty()
            && self.queue.submitted == 0
            && self.queue.last_capture.is_none()
            && default_output_id() == Some(self.route)
            && !self.renderer.failed()
            && !self.renderer.take_flush()
            && self
                .resume_preroll_position(self.renderer.presented_through())
                .is_some_and(|through| through < self.warm_frames)
        {
            // The paused renderer is already running with synthetic silence.
            // A fresh capture will join this clock after the successor is verified.
            live_trace::event("renderer_next_kept_pause_silence", None);
            Some(StandbyWarmup {
                origin: 0,
                started,
                silence_end: self.warm_frames,
                scratch: std::mem::take(&mut self.scratch),
                paused_deadline: Some(deadline),
                running_handoff_deadline: None,
                retained: None,
                retained_pause: false,
                paused_format: None,
            })
        } else {
            // The outgoing capture has gone. Discard its PCM before warming
            // the platform renderer at a new anchor.
            self.silence();
            self.renderer.reset_time();
            self.renderer.discard_enqueued();
            self.renderer.take_flush();
            live_trace::event("renderer_retired", None);
            None
        };
        (!self.renderer.failed()).then_some(StandbyRenderer {
            renderer: self.renderer,
            route: self.route,
            rate: self.rate,
            lead: self.lead,
            warming,
        })
    }

    pub(super) fn pause(&mut self) {
        self.silence();
        self.renderer.reset_time();
        self.renderer.discard_enqueued();
        self.renderer.take_flush();
        self.queue.clear();
        self.clock_map = None;
        self.submitted_through = None;
        self.presented = false;
        self.last_observed_input = None;
        self.resubmit = false;
        self.fade_from = None;
        self.warm_frames = 0;
        self.needs_start = true;
        self.resume_warmup = None;
        self.paused_format = None;
        self.signal_trace = live_trace::enabled().then(SignalTrace::default);
        live_trace::event("pause_reset", None);
    }

    pub(super) fn prewarm_paused(&mut self) -> Result<bool, Failure> {
        if self.needs_start && default_output_id() != Some(self.route) {
            return Err(Failure::WaitingForRoute);
        }
        objc2::rc::autoreleasepool(|_| self.warm_paused_renderer())
            .map_err(|failure| interruption(self.route, default_output_id(), failure))
    }

    fn warm_paused_renderer(&mut self) -> Result<bool, Failure> {
        let flushed = self.renderer.take_flush();
        if self.renderer.failed() || (flushed && !self.needs_start) {
            return Err(Failure::DirectRendererInterrupted);
        }
        if self.restart_retained_silence()? {
            return Ok(false);
        }
        let through = self.renderer.presented_through();
        let horizon = (self.lead + self.rate / 12) as u64;
        if self.needs_start {
            self.resume_warmup = Some(ResumeWarmup::Resetting);
            // Reset is asynchronous. A positive clock may still describe the
            // outgoing epoch, so wait until the zero anchor is visible.
            if through != Some(0) {
                return Ok(false);
            }
            debug_assert!(self.queue.frames.is_empty());
            self.renderer.set_muted(false);
            self.muted = false;
            self.enqueue_silence(horizon)?;
            self.resume_warmup = Some(ResumeWarmup::Started {
                origin: 0,
                started: Instant::now(),
            });
            self.renderer.start_at(0);
            self.needs_start = false;
            live_trace::event("renderer_prewarm_started", None);
            return Ok(false);
        }
        let Some(through) = self.resume_preroll_position(through) else {
            return Ok(false);
        };
        // Keep a bounded wall of synthetic silence until actual playback
        // is confirmed. The first source batch joins this same clock.
        let target = through + horizon;
        if target.saturating_sub(self.warm_frames) >= (self.rate / 25) as u64
            && self.renderer.is_ready()
        {
            self.enqueue_silence(target.min(self.warm_frames + horizon))?;
        }
        let Some(through) = self.resume_preroll_position(self.renderer.presented_through()) else {
            return Ok(false);
        };
        Ok(through.saturating_add(self.lead as u64) <= self.warm_frames)
    }

    fn resume_preroll_position(&self, through: Option<u64>) -> Option<u64> {
        self.resume_warmup
            .as_ref()?
            .position(through, Instant::now(), self.rate as u32)
    }

    pub(super) fn cancel_prewarm(&mut self) {
        self.resume_warmup = None;
        if !self.needs_start {
            self.pause();
            live_trace::event("renderer_prewarm_cancelled", None);
        }
    }

    fn enqueue_silence(&mut self, through: u64) -> Result<(), Failure> {
        enqueue_silence(
            &self.renderer,
            &mut self.scratch,
            &mut self.warm_frames,
            through,
        )
    }

    pub(super) fn tick(
        &mut self,
        receive: impl FnMut(Sample),
        playing: impl Fn() -> bool,
    ) -> Result<bool, Failure> {
        objc2::rc::autoreleasepool(|_| self.pump(receive, playing))
    }

    fn reset_after_flush(&mut self) {
        // AVFoundation leaves the timebase running after an automatic flush.
        // Preserve an established media mapping, but restart an unmapped
        // clock: flushing removed its synthetic scheduling lead as well.
        if self.clock_map.is_none() {
            self.silence();
            self.renderer.reset_time();
            self.warm_frames = 0;
            self.needs_start = true;
            self.resume_warmup = Some(ResumeWarmup::Resetting);
        }
        self.renderer.discard_enqueued();
        self.queue.submitted = 0;
        self.submitted_through = None;
        self.resubmit = self.clock_map.is_some();
        if self.presented {
            self.last_progress = Instant::now();
        }
        self.presented = false;
    }

    fn pump(
        &mut self,
        mut receive: impl FnMut(Sample),
        playing: impl Fn() -> bool,
    ) -> Result<bool, Failure> {
        let failed = self.renderer.failed();
        let flushed = self.renderer.take_flush();
        if failed || flushed {
            let status = interruption(
                self.route,
                default_output_id(),
                Failure::DirectRendererInterrupted,
            );
            if failed || status == Failure::WaitingForRoute {
                return Err(status);
            }
            self.reset_after_flush();
            live_trace::event("system_flush", None);
        }
        // An external controller can resume this owned Soloist before the
        // coordinator's deferred Play. Restart only the retained zero clock;
        // capture continues to drain into its existing bounded PCM queue.
        if matches!(self.resume_warmup, Some(ResumeWarmup::Retained(_)))
            && playing()
            && self.restart_retained_silence()?
        {
            return Ok(false);
        }
        let now = Instant::now();
        let preroll = self.rate / 25;

        if self.last_observed_input != self.queue.last_capture {
            self.last_observed_input = self.queue.last_capture;
            self.last_input = now;
        }
        if self.last_input.elapsed() >= STALL {
            return Err(Failure::DirectInputStalled);
        }

        let startup_slack = if self.presented {
            Duration::ZERO
        } else {
            Duration::from_secs_f64((self.lead + self.rate / 12) as f64 / self.rate as f64)
        };
        let output_stall = if self.resume_warmup.is_some_and(ResumeWarmup::is_retained) {
            COLD_START_LIMIT
        } else {
            STALL + startup_slack
        };
        let through = self.renderer.presented_through();
        let through = if let Some(warmup) = &self.resume_warmup {
            // Playback may resume outside the deferred command while the
            // synthetic warmup is in flight. Keep the captured PCM in the
            // bounded queue until the new timebase is ready.
            let Some(through) = warmup.position(through, now, self.rate as u32) else {
                if self.last_progress.elapsed() >= output_stall {
                    return Err(Failure::DirectOutputStalled);
                }
                return Ok(false);
            };
            through
        } else {
            through.ok_or(Failure::DirectOutputStalled)?
        };
        let fade_frames = self.rate / 100;
        if let Some((capture, output)) = self.clock_map
            && let Some(elapsed) = through.checked_sub(output)
        {
            let played = capture + elapsed;
            if self
                .submitted_through
                .is_some_and(|tail| played.saturating_sub(tail) > (self.rate / 40) as u64)
            {
                // If the renderer ran out, continuing would skip capture
                // timestamps. Releasing the tap restores direct sound.
                return Err(Failure::DirectOutputStalled);
            }
            let signal_trace = &mut self.signal_trace;
            if self.queue.present(played, |frame| {
                let frame = faded(frame, self.fade_from, fade_frames);
                if let Some(trace) = signal_trace
                    && !trace.presented
                    && live_trace::signal(frame.samples)
                {
                    trace.presented = true;
                    live_trace::event("presented_signal", Some(frame.position));
                }
                receive(frame);
            }) > 0
            {
                self.presented = true;
                self.last_progress = Instant::now();
            }
        }
        if self.last_progress.elapsed() >= output_stall {
            return Err(Failure::DirectOutputStalled);
        }

        if self.clock_map.is_none() && self.queue.frames.len() < preroll {
            return Ok(false);
        }
        if self.resubmit {
            let (capture, output) = self.clock_map.expect("resubmission keeps the clock map");
            // The hardware consumes ahead of the public clock. Reuse PCM
            // whose original timestamp is still schedulable, with no replay
            // of elapsed sound and no change in playback rate.
            if let Some(elapsed) = (through + self.lead as u64).checked_sub(output) {
                self.queue.discard_through(capture + elapsed);
            }
        }
        let horizon = self.lead + self.rate / 12;
        while playing() && self.renderer.is_ready() {
            let offset = self.queue.submitted;
            let count = (self.queue.frames.len() - offset)
                .min(horizon - offset)
                .min(BATCH);
            if count == 0 || (self.clock_map.is_some() && count < self.rate / 250) {
                break;
            }
            if self.needs_start && self.muted {
                // The clock is stopped and its old queue is empty. Changing
                // mute after start can synchronously wait for Bluetooth I/O
                // and overrun the process tap while the worker is blocked.
                self.renderer.set_muted(false);
                self.muted = false;
                live_trace::event("renderer_unmuted", None);
                if !playing() {
                    self.silence();
                    return Ok(false);
                }
            }
            let batch = &self.queue.frames.make_contiguous()[offset..offset + count];
            let fade_from = if self.clock_map.is_none() || self.resubmit {
                Some(batch[0].position)
            } else {
                self.fade_from
            };
            self.scratch.clear();
            let needs_start = self.needs_start;
            if needs_start {
                // Start the flushed AudioQueue at a new media origin. Put the
                // hardware lead and first fresh PCM in the same buffer so
                // readiness cannot stall a paused renderer between them.
                live_trace::starting_schedule(self.rate, self.lead);
                self.scratch.resize(self.lead, [0.0; 2]);
            }
            self.scratch.extend(
                batch
                    .iter()
                    .map(|frame| faded(*frame, fade_from, fade_frames).samples),
            );
            let start = batch[0].position - 1;
            // Keep the warmed clock running. The lead also covers the device
            // scheduling clock advancing ahead of this media timebase.
            let media_start = if needs_start {
                self.lead as u64
            } else {
                self.warm_frames.max(through + self.lead as u64)
            };
            let (capture, output) = self.clock_map.unwrap_or((start, media_start));
            let timestamp = if needs_start {
                0
            } else {
                output + (start - capture)
            };
            // The lease can change while AVFoundation reports readiness or
            // the worker assembles the faded batch. Commit only accepted PCM.
            if !playing() {
                if needs_start {
                    self.silence();
                }
                return Ok(false);
            }
            self.renderer
                .enqueue(timestamp, &self.scratch)
                .map_err(|_| Failure::DirectRendererInterrupted)?;
            if needs_start {
                if !playing() {
                    // The clock is still stopped. Clear the native batch so
                    // another start cannot pick up PCM from this turn.
                    self.renderer.discard_enqueued();
                    self.silence();
                    return Ok(false);
                }
                self.renderer.start_at(0);
                self.needs_start = false;
            }
            self.clock_map = Some((capture, output));
            self.resume_warmup = None;
            self.fade_from = fade_from;
            if let Some(trace) = &mut self.signal_trace
                && !trace.scheduled
                && let Some(index) = self.scratch[self.scratch.len() - count..]
                    .iter()
                    .position(|samples| live_trace::signal(*samples))
            {
                trace.scheduled = true;
                live_trace::event("scheduled_signal", Some(batch[index].position));
            }
            self.submitted_through = batch.last().map(|frame| frame.position);
            self.queue.submitted += count;
            self.resubmit = false;
            if needs_start {
                live_trace::event("renderer_epoch_started", None);
            }
        }
        Ok(self.presented)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paused_preroll_requires_the_distinct_paused_authority_and_original_deadline() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(45);
        assert!(paused_preroll_authorized(
            OpenAuthorization::PausedSameOutput,
            now,
            deadline
        ));
        for authority in [OpenAuthorization::Playing, OpenAuthorization::Revoked] {
            assert!(!paused_preroll_authorized(authority, now, deadline));
            assert!(
                Relay::start_paused_preroll(48_000, 0, deadline, || authority)
                    .unwrap()
                    .is_none()
            );
        }
        assert!(!paused_preroll_authorized(
            OpenAuthorization::PausedSameOutput,
            deadline,
            deadline
        ));
        assert!(
            Relay::start_paused_preroll(48_000, 0, now, || OpenAuthorization::PausedSameOutput)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[ignore = "starts only synthetic zeros on the current AirPods route; no tap or capture IO"]
    fn live_airpods_confirmed_paused_zero_renderer_before_any_capture_io() {
        let route = stereo_bluetooth_output().expect("AirPods stereo route is required");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut standby = Relay::start_paused_preroll(48_000, route, deadline, || {
            OpenAuthorization::PausedSameOutput
        })
        .unwrap()
        .expect("confirmed pause starts zeros");
        let began = Instant::now();
        while !standby.keep_paused_warm().unwrap() {
            assert!(began.elapsed() < COLD_START_LIMIT);
            std::thread::sleep(Duration::from_millis(4));
        }
        let warmup = standby.warming.as_ref().unwrap();
        assert_eq!(warmup.paused_deadline, Some(deadline));
        assert!(warmup.paused_format.is_some());
        assert!(warmup.scratch.iter().all(|sample| *sample == [0.0; 2]));
        let through = standby.renderer.presented_through().unwrap();
        assert!(through > 0 && through < warmup.silence_end);
        assert!(standby.paused_ready());
        assert!(standby.resume_paused_silence(|| false).is_err());
        standby.cancel_prewarm();
        assert!(standby.warming.is_none());
        eprintln!(
            "confirmed_pause_zero_constructor ready_in_ms={} no_capture_io=true",
            began.elapsed().as_millis()
        );
    }

    #[test]
    fn route_format_requires_packed_stereo_f32_or_i16_linear_pcm() {
        let mut format = AudioStreamBasicDescription {
            mSampleRate: 48_000.0,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        assert!(supported_stereo_format(&format));
        for channels in [0, 1, 3, 6] {
            format.mChannelsPerFrame = channels;
            assert!(!supported_stereo_format(&format));
        }
        format.mChannelsPerFrame = 2;
        format.mBitsPerChannel = 16;
        assert!(!supported_stereo_format(&format));
        format.mFormatFlags = kAudioFormatFlagIsSignedInteger | kAudioFormatFlagIsPacked;
        format.mBytesPerPacket = 4;
        format.mBytesPerFrame = 4;
        assert!(supported_stereo_format(&format));
        assert!(!supported_stereo_format(&AudioStreamBasicDescription {
            mFormatFlags: kAudioFormatFlagIsSignedInteger,
            ..format
        }));
        assert!(!supported_stereo_format(&AudioStreamBasicDescription {
            mFormatID: 0,
            ..format
        }));
        assert!(!supported_stereo_format(&AudioStreamBasicDescription {
            mBitsPerChannel: 24,
            ..format
        }));
    }

    #[test]
    fn ordinary_next_preroll_caps_both_remaining_wait_and_total_startup() {
        let started = Instant::now();
        assert_eq!(
            next_preroll_deadline(started, started),
            started + Duration::from_millis(210)
        );
        let capture_prepared = started + Duration::from_millis(44);
        let deadline = next_preroll_deadline(capture_prepared, started);
        assert_eq!(deadline - capture_prepared, Duration::from_millis(206));
        assert_eq!(deadline, started + Duration::from_millis(250));

        let expired = deadline + Duration::from_millis(1);
        assert!(next_preroll_deadline(expired, started) < expired);
    }

    #[test]
    fn natural_handoff_can_observe_late_clock_progress_without_renewing_startup() {
        let started = Instant::now();
        let handoff = started + Duration::from_millis(204);
        let deadline = next_preroll_deadline(handoff, started);
        assert_eq!(deadline, started + NEXT_PREROLL_LIMIT);
        assert!(!warm_position_is_current(0, 0, handoff - started, 48_000));
        let ready = started + Duration::from_millis(218);
        assert!(ready < deadline);
        assert!(warm_position_is_current(0, 2, ready - started, 48_000));
        let late = started + NEXT_PREROLL_LIMIT + Duration::from_millis(1);
        assert_eq!(next_preroll_deadline(late, started), deadline);
        assert!(
            deadline < late,
            "A later handoff must not acquire another 210 ms wait"
        );
    }

    #[test]
    fn final_preroll_observation_accepts_fresh_progress_after_wait_but_not_expired_authority() {
        use std::cell::Cell;
        let started = Instant::now();
        let wait_deadline = next_preroll_deadline(started, started);
        let authority_deadline = started + Duration::from_millis(1250);
        let now = Cell::new(wait_deadline + Duration::from_millis(1));
        let observed = Cell::new(0);
        let authorized = || now.get() < authority_deadline;
        let tail = observe_authorized_preroll_tail(&authorized, || {
            observed.set(observed.get() + 1);
            // The timebase crossed zero during the final route check. Its
            // position is still bounded by this startup and existing zeros.
            warm_position_is_current(0, 2, now.get() - started, 48_000).then_some(14_112)
        });
        assert_eq!(tail, Some(14_112));
        assert_eq!(observed.get(), 1);
        assert_eq!(now.get(), wait_deadline + Duration::from_millis(1));

        let tail = observe_authorized_preroll_tail(&authorized, || {
            observed.set(observed.get() + 1);
            // A native observation cannot extend a request which expired
            // while that read was in progress.
            now.set(authority_deadline);
            Some(14_112)
        });
        assert_eq!(tail, None);
        assert_eq!(observed.get(), 2);
        assert!(
            observe_authorized_preroll_tail(&authorized, || {
                observed.set(observed.get() + 1);
                Some(14_112)
            })
            .is_none()
        );
        assert_eq!(
            observed.get(),
            2,
            "Expired authority must not even observe a reusable tail"
        );
    }

    #[test]
    fn final_preroll_observation_rejects_revocation_during_the_native_read() {
        use std::cell::Cell;
        let current = Cell::new(true);
        assert!(
            observe_authorized_preroll_tail(&|| current.get(), || {
                current.set(false);
                Some(14_112)
            })
            .is_none()
        );
    }

    #[test]
    fn seek_preroll_waits_only_for_an_aged_queue_and_obeys_both_deadlines() {
        let retired_at = Instant::now();
        let started = retired_at + Duration::from_millis(17);
        assert_eq!(
            seek_preroll_deadline(started + Duration::from_millis(149), retired_at, started),
            None
        );
        let eligible = started + Duration::from_millis(150);
        assert_eq!(
            seek_preroll_deadline(eligible, retired_at, started),
            Some(eligible + Duration::from_millis(70))
        );
        let expires = retired_at + Duration::from_millis(250);
        assert_eq!(
            seek_preroll_deadline(started + Duration::from_millis(179), retired_at, started),
            Some(expires)
        );
        assert_eq!(seek_preroll_deadline(expires, retired_at, started), None);
    }

    #[test]
    fn interruption_requires_a_route_mismatch_to_wait_for_a_route() {
        for failure in [Failure::DirectRenderer, Failure::DirectRendererInterrupted] {
            assert_eq!(interruption(7, Some(7), failure), failure);
            assert_eq!(interruption(7, None, failure), Failure::WaitingForRoute);
            assert_eq!(interruption(7, Some(8), failure), Failure::WaitingForRoute);
        }
    }

    fn sample(position: u64) -> Sample {
        Sample {
            samples: [0.01, -0.02],
            position,
        }
    }

    #[test]
    fn relay_accepts_only_contiguous_bounded_stereo_and_retires_only_submitted_frames() {
        let mut queue = RelayQueue::new(3);
        assert!(queue.push(sample(0)).is_err());
        assert!(queue.push(sample(99)).is_ok());
        assert!(queue.push(sample(101)).is_err());
        assert!(
            queue
                .push(Sample {
                    samples: [f32::NAN, 0.0],
                    position: 100
                })
                .is_err()
        );
        assert!(queue.push(sample(100)).is_ok());
        assert!(queue.push(sample(101)).is_ok());
        assert!(queue.push(sample(102)).is_err());
        queue.submitted = 2;
        let mut retired = Vec::new();
        assert_eq!(queue.present(120, |frame| retired.push(frame.position)), 2);
        assert_eq!(retired, [99, 100]);
        assert_eq!(queue.frames[0].position, 101);
        assert_eq!(queue.submitted, 0);
        assert!(queue.push(sample(102)).is_ok());
    }

    #[test]
    fn lead_covers_the_observed_airpods_device_delay_and_scales_with_input_rate() {
        let lead = scheduling_lead(44_100, 48_000.0, 7_680 + 512).unwrap();
        assert!((9_290..9_300).contains(&lead), "{lead}");
        assert_eq!(scheduling_lead(48_000, 48_000.0, 0), Ok(9_600));
        assert_eq!(
            scheduling_lead(48_000, 48_000.0, 30_000),
            Err(Failure::DirectRenderer)
        );
    }

    #[test]
    fn flush_drops_only_unschedulable_frames_and_pause_starts_a_fresh_epoch() {
        let mut queue = RelayQueue::new(8);
        for position in 1..=8 {
            queue.push(sample(position)).unwrap();
        }
        queue.discard_through(5);
        assert_eq!(
            queue
                .frames
                .iter()
                .map(|frame| frame.position)
                .collect::<Vec<_>>(),
            [6, 7, 8]
        );
        assert!(queue.push(sample(9)).is_ok());
        assert_eq!(queue.present(20, |_| unreachable!()), 0);
        queue.submitted = 2;
        queue.clear();
        assert!(queue.frames.is_empty());
        assert_eq!(queue.submitted, 0);
        assert!(queue.push(sample(100)).is_ok());
    }

    #[test]
    fn restart_envelope_keeps_both_channels_and_full_gain_after_ten_milliseconds() {
        assert_eq!(faded(sample(10), Some(10), 480).samples, [0.0; 2]);
        assert_eq!(faded(sample(250), Some(10), 480).samples, [0.005, -0.01]);
        assert_eq!(
            faded(sample(490), Some(10), 480).samples,
            sample(490).samples
        );
    }

    #[test]
    fn late_revocation_does_not_commit_or_start_a_prepared_batch() {
        use std::cell::Cell;

        let rate = 48_000;
        let lead = rate as usize / 5;
        // AVFoundation's synchronizer stays stopped. Both revocation points
        // exercise the production pump without starting an audio device.
        for revoke_at in [3, 4] {
            let renderer = SpatialRenderer::open(rate).unwrap();
            renderer.set_muted(true);
            let mut relay = Relay::new(renderer, 0, rate as usize, lead, 0, true, None);
            let first = 4_001;
            for position in first..first + u64::from(rate / 25) {
                relay.push(sample(position)).unwrap();
            }
            let checked = Cell::new(0);
            assert_eq!(
                relay.tick(
                    |_| panic!("the stopped clock cannot present PCM"),
                    || {
                        let call = checked.get() + 1;
                        checked.set(call);
                        // Check at loop entry, after unmute, before enqueue,
                        // then before starting the renderer.
                        call < revoke_at
                    },
                ),
                Ok(false),
            );
            assert_eq!(checked.get(), revoke_at);
            assert_eq!(relay.scratch.len(), lead + BATCH);
            assert_eq!(relay.queue.frames.len(), rate as usize / 25);
            assert_eq!(relay.queue.frames.front().unwrap().position, first);
            assert_eq!(relay.queue.submitted, 0);
            assert!(relay.submitted_through.is_none());
            assert!(relay.clock_map.is_none());
            assert!(relay.fade_from.is_none());
            assert!(relay.muted && relay.needs_start && !relay.resubmit);
        }
    }

    #[test]
    fn resume_requires_the_reset_anchor_and_the_new_clock() {
        let rate = 48_000;
        let now = Instant::now();
        for (position, resetting, started) in [
            (None, None, None),
            (Some(0), Some(0), None),
            (Some(240), None, Some(240)),
            (Some(47 * rate as u64), None, None),
        ] {
            assert_eq!(
                ResumeWarmup::Resetting.position(position, now, rate),
                resetting
            );
            assert_eq!(
                ResumeWarmup::Started {
                    origin: 0,
                    started: now
                }
                .position(position, now + Duration::from_millis(5), rate),
                started,
            );
        }
    }

    #[test]
    fn retained_resume_requires_a_stationary_anchor_then_new_relative_advancement() {
        let rate = 48_000;
        let anchor = 45 * u64::from(rate);
        let horizon = 13_600;
        let now = Instant::now();
        let settling = PausedZeroClock::begin(anchor, now);
        let settling = settling
            .observe(
                Some(anchor),
                anchor + horizon,
                horizon,
                now + PAUSED_ANCHOR_STABLE / 2,
            )
            .unwrap();
        assert!(settling.anchor().is_none());
        let suspended = settling
            .observe(
                Some(anchor),
                anchor + horizon,
                horizon,
                now + PAUSED_ANCHOR_STABLE,
            )
            .unwrap();
        assert_eq!(suspended.anchor(), Some(anchor));
        let stopped = ResumeWarmup::Retained(suspended);
        assert!(stopped.is_retained());
        for position in [None, Some(0), Some(anchor), Some(anchor + 240)] {
            assert_eq!(
                stopped.position(position, now + Duration::from_secs(60), rate),
                None
            );
        }
        let resumed = ResumeWarmup::Started {
            origin: anchor,
            started: now,
        };
        for previous in [anchor - 1, anchor, anchor + 1] {
            assert_eq!(
                resumed.position(Some(previous), now + Duration::from_millis(700), rate),
                None,
                "stationary tolerance cannot prove a restarted clock"
            );
        }
        assert_eq!(
            resumed.position(Some(anchor + 2), now, rate),
            Some(anchor + 2)
        );
        assert_eq!(
            resumed.position(Some(anchor), now + Duration::from_millis(700), rate),
            None
        );
        assert_eq!(
            resumed.position(Some(anchor - 1), now + Duration::from_millis(5), rate),
            None
        );
        assert_eq!(
            resumed.position(Some(anchor + 240), now + Duration::from_millis(5), rate),
            Some(anchor + 240)
        );
        assert_eq!(
            resumed.position(
                Some(anchor + u64::from(rate)),
                now + Duration::from_millis(5),
                rate
            ),
            None
        );
        assert!(
            !ResumeWarmup::Started {
                origin: 0,
                started: now
            }
            .is_retained()
        );
    }

    #[test]
    fn paused_anchor_rejects_drift_exhausted_tails_and_unsettled_timebases() {
        let now = Instant::now();
        let anchor = 96_000;
        let horizon = 13_600;
        let settling = PausedZeroClock::begin(anchor, now);
        let changed = settling
            .observe(
                Some(anchor + 32),
                anchor + horizon,
                horizon,
                now + Duration::from_millis(4),
            )
            .unwrap();
        assert!(
            changed
                .observe(
                    Some(anchor + 32),
                    anchor + horizon,
                    horizon,
                    now + Duration::from_millis(8)
                )
                .unwrap()
                .anchor()
                .is_none()
        );
        let settled = changed
            .observe(
                Some(anchor + 32),
                anchor + horizon,
                horizon,
                now + Duration::from_millis(12),
            )
            .unwrap();
        assert_eq!(settled.anchor(), Some(anchor + 32));
        assert!(
            settled
                .observe(
                    Some(anchor + 34),
                    anchor + horizon,
                    horizon,
                    now + Duration::from_secs(1)
                )
                .is_err()
        );
        assert!(
            settled
                .observe(Some(anchor + horizon + 1), anchor + horizon, horizon, now)
                .is_err()
        );
        assert!(
            settling
                .observe(Some(anchor), anchor + horizon + 1, horizon, now)
                .is_err()
        );
        assert!(
            settling
                .observe(
                    Some(anchor),
                    anchor + horizon,
                    horizon,
                    now + PAUSED_ANCHOR_LIMIT
                )
                .is_err()
        );
        assert!(
            settling
                .observe(None, anchor + horizon, horizon, now)
                .is_err()
        );
    }

    #[test]
    fn dormant_retained_clock_never_uses_the_old_positive_zero_origin_proof() {
        let now = Instant::now();
        let anchor = 2_160_000;
        let mut warmup = StandbyWarmup {
            origin: 0,
            started: now - Duration::from_secs(45),
            silence_end: anchor + 13_600,
            scratch: Vec::new(),
            paused_deadline: Some(now - Duration::from_secs(1)),
            running_handoff_deadline: None,
            retained: Some(PausedZeroClock::Suspended { anchor }),
            retained_pause: true,
            paused_format: None,
        };
        assert!(warmup.is_current());
        assert_eq!(warmup.position(Some(anchor), now, 48_000), None);
        warmup.origin = anchor;
        warmup.started = now;
        warmup.retained = None;
        for previous in [anchor - 1, anchor, anchor + 1] {
            assert_eq!(
                warmup.position(Some(previous), now + Duration::from_millis(700), 48_000),
                None
            );
        }
        assert_eq!(
            warmup.position(Some(anchor + 2), now, 48_000),
            Some(anchor + 2)
        );
        assert_eq!(
            warmup.position(Some(anchor), now + Duration::from_millis(700), 48_000),
            None
        );
        assert_eq!(
            warmup.position(Some(anchor + 240), now + Duration::from_millis(5), 48_000),
            Some(anchor + 240)
        );
        assert_eq!(
            warmup.position(
                Some(anchor + 48_000),
                now + Duration::from_millis(5),
                48_000
            ),
            None
        );
        warmup.retained_pause = false;
        assert_eq!(
            warmup.position(Some(anchor + 240), now + Duration::from_millis(5), 48_000),
            None
        );
    }

    #[test]
    fn first_dormant_resume_after_expiry_must_suspend_before_accepting_the_clock() {
        let now = Instant::now();
        let mut warmup = StandbyWarmup {
            origin: 0,
            started: now - Duration::from_secs(45),
            silence_end: 2_173_600,
            scratch: Vec::new(),
            paused_deadline: Some(now),
            running_handoff_deadline: None,
            retained: None,
            retained_pause: false,
            paused_format: None,
        };
        assert!(!warmup.requires_expiry_suspend(now - Duration::from_millis(1)));
        assert!(warmup.requires_expiry_suspend(now));
        assert_eq!(warmup.position(Some(2_160_000), now, 48_000), None);
        warmup.retained = Some(PausedZeroClock::begin(2_160_000, now));
        warmup.retained_pause = true;
        assert!(!warmup.requires_expiry_suspend(now));
        assert_eq!(
            warmup.position(Some(2_160_001), now + Duration::from_secs(1), 48_000),
            None
        );
    }

    #[test]
    fn retained_route_fingerprint_rejects_same_device_format_changes() {
        let format = AudioStreamBasicDescription {
            mSampleRate: 48_000.0,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let original = PausedRouteFormat::read(&format);
        assert!(original.is_some());
        for changed in [
            AudioStreamBasicDescription {
                mSampleRate: 44_100.0,
                ..format
            },
            AudioStreamBasicDescription {
                mChannelsPerFrame: 1,
                ..format
            },
            AudioStreamBasicDescription {
                mFormatFlags: format.mFormatFlags | kAudioFormatFlagIsSignedInteger,
                ..format
            },
            AudioStreamBasicDescription {
                mBytesPerFrame: 16,
                ..format
            },
            AudioStreamBasicDescription {
                mBitsPerChannel: 16,
                ..format
            },
            AudioStreamBasicDescription {
                mSampleRate: f64::NAN,
                ..format
            },
        ] {
            assert_ne!(PausedRouteFormat::read(&changed), original);
        }
    }

    #[test]
    fn resume_falls_back_when_a_started_clock_never_advances() {
        let rate = 48_000;
        let renderer = SpatialRenderer::open(rate).unwrap();
        renderer.set_muted(true);
        let mut relay = Relay::new(renderer, 0, rate as usize, rate as usize / 5, 0, true, None);
        let expired = Instant::now() - Duration::from_secs(1);
        relay.resume_warmup = Some(ResumeWarmup::Started {
            origin: 0,
            started: expired,
        });
        relay.last_progress = expired;
        for position in 1..=u64::from(rate / 25) {
            relay.push(sample(position)).unwrap();
        }
        assert_eq!(
            relay.tick(|_| panic!("the stopped clock cannot present PCM"), || true),
            Err(Failure::DirectOutputStalled),
        );
        assert_eq!(relay.queue.submitted, 0);
        assert!(relay.clock_map.is_none());
    }

    fn play_quiet_epoch(relay: &mut Relay, first: u64, frequency: f32) -> Duration {
        let start = Instant::now();
        let rate = relay.rate as u64;
        let mut submitted = 0;
        let mut presented = 0;
        let mut first_signal = None;
        relay.capture_started();
        while presented < rate / 10 {
            assert!(start.elapsed() < Duration::from_secs(2));
            let available = (start.elapsed().as_secs_f64() * rate as f64) as u64;
            while submitted < available && relay.can_accept() {
                let phase = submitted as f32 * std::f32::consts::TAU * frequency / rate as f32;
                relay
                    .push(Sample {
                        samples: [0.004 * phase.sin(), 0.004 * phase.cos()],
                        position: first + submitted,
                    })
                    .unwrap();
                submitted += 1;
            }
            relay
                .tick(
                    |frame| {
                        assert_eq!(
                            frame.position,
                            first + presented,
                            "previous epoch was replayed or a new frame was skipped"
                        );
                        presented += 1;
                        if live_trace::signal(frame.samples) {
                            first_signal.get_or_insert_with(|| start.elapsed());
                        }
                    },
                    || true,
                )
                .unwrap();
            std::thread::sleep(Duration::from_millis(4));
        }
        first_signal.expect("the renderer clock must present a new non-silent frame")
    }

    #[test]
    #[ignore = "plays original quiet tones on the selected stereo Bluetooth output for a hardware timing comparison"]
    fn live_airpods_resume_after_silent_pause() {
        let route = stereo_bluetooth_output().expect("select AirPods as the default stereo output");
        let rate = 48_000;
        let pause_duration = Duration::from_secs(35);
        let drain = Duration::from_millis(600);
        for keep_warm in [false, true] {
            let mut relay = Relay::open(rate, route, None, || true).unwrap().unwrap();
            play_quiet_epoch(&mut relay, 1, 330.0);
            assert!(
                relay.queue.submitted > 0,
                "pause must discard queued source audio"
            );

            let paused = Instant::now();
            relay.pause();
            assert!(relay.muted && relay.needs_start);
            std::thread::sleep(drain);
            assert_eq!(relay.renderer.presented_through(), Some(0));
            let mut silent_ready = None;
            let mut next_route_check = Instant::now();
            while paused.elapsed() < pause_duration {
                let now = Instant::now();
                if now >= next_route_check {
                    assert_eq!(
                        default_output_id(),
                        Some(route),
                        "output changed during the experiment"
                    );
                    next_route_check = now + Duration::from_millis(200);
                }
                if keep_warm && relay.prewarm_paused().unwrap() {
                    silent_ready.get_or_insert_with(|| paused.elapsed());
                    assert!(relay.scratch.iter().all(|samples| *samples == [0.0; 2]));
                }
                assert!(relay.queue.frames.is_empty() && relay.queue.submitted == 0);
                assert!(relay.clock_map.is_none());
                std::thread::sleep(Duration::from_millis(10));
            }
            if keep_warm {
                assert!(
                    silent_ready.expect("silence must reach the renderer during the pause")
                        < pause_duration / 2
                );
                assert!(
                    relay.renderer.presented_through().unwrap_or_default() > u64::from(rate) * 30
                );
            } else {
                assert!(relay.muted && relay.needs_start);
                assert_eq!(relay.renderer.presented_through(), Some(0));
            }
            assert_eq!(stereo_bluetooth_output(), Some(route));

            let play = Instant::now();
            while !relay.prewarm_paused().unwrap() {
                assert!(
                    play.elapsed() < super::super::RESUME_PREWARM_LIMIT,
                    "renderer readiness timed out after Play"
                );
                std::thread::sleep(Duration::from_millis(4));
            }
            let ready = play.elapsed();
            let first_signal = play_quiet_epoch(&mut relay, 1_000_001, 550.0);
            assert_eq!(stereo_bluetooth_output(), Some(route));
            relay.pause();
            eprintln!(
                "airpods_35s_resume keep_warm={keep_warm} silent_ready_during_pause={silent_ready:?} play_to_ready={ready:?} ready_to_first_signal={first_signal:?} play_to_first_signal={:?}",
                ready + first_signal,
            );
        }
    }

    #[test]
    #[ignore = "plays original quiet epochs around a real 45-second zero window and 30-second stopped AirPods pause"]
    fn live_airpods_retained_resume_after_expired_zero_window() {
        let route = stereo_bluetooth_output().expect("select the stereo AirPods output");
        let route_current = || crate::spatial::bluetooth_output_id() == Some(route);
        let rate = 48_000;
        let mut relay = Relay::open(rate, route, None, || true).unwrap().unwrap();
        play_quiet_epoch(&mut relay, 1, 330.0);
        relay.pause();
        let paused = Instant::now();
        while paused.elapsed() < super::super::PAUSED_SILENCE_AFTER {
            assert!(route_current());
            std::thread::sleep(Duration::from_millis(12));
        }
        let mut was_ready = false;
        while paused.elapsed() < super::super::PAUSED_SILENCE_LIMIT {
            assert!(route_current());
            was_ready |= relay.prewarm_paused().unwrap();
            assert!(relay.queue.frames.is_empty() && relay.clock_map.is_none());
            std::thread::sleep(Duration::from_millis(8));
        }
        assert!(was_ready, "the original zero window never became ready");
        let stopped = Instant::now();
        let tail = relay.warm_frames;
        let mut anchor = None;
        while stopped.elapsed() < Duration::from_secs(30) {
            assert!(route_current());
            relay.suspend_paused_silence(route_current).unwrap();
            assert_eq!(
                relay.warm_frames, tail,
                "stopped retention appended synthetic buffers"
            );
            assert!(
                relay.queue.frames.is_empty()
                    && relay.clock_map.is_none()
                    && relay.queue.last_capture.is_none()
            );
            if let Some(ResumeWarmup::Retained(clock)) = relay.resume_warmup
                && let Some(observed) = clock.anchor()
            {
                assert_eq!(*anchor.get_or_insert(observed), observed);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let anchor = anchor.expect("the stopped clock never settled");
        assert!(anchor >= u64::from(rate) * 40);
        assert_eq!(relay.resume_preroll_position(Some(anchor)), None);
        let resume = Instant::now();
        while !relay.prewarm_paused().unwrap() {
            assert!(route_current());
            assert!(resume.elapsed() < COLD_START_LIMIT);
            std::thread::sleep(Duration::from_millis(4));
        }
        let ready = resume.elapsed();
        assert!(
            matches!(relay.resume_warmup, Some(ResumeWarmup::Started { origin, .. }) if origin == anchor)
        );
        let signal = play_quiet_epoch(&mut relay, 1_000_001, 550.0);
        assert!(route_current());
        relay.pause();
        assert!(relay.muted && relay.needs_start && relay.resume_warmup.is_none());
        assert!(relay.queue.frames.is_empty() && relay.clock_map.is_none());
        eprintln!(
            "airpods_retained_resume path=session zero_window={:?} stopped={:?} anchor={anchor} resume_to_ready={ready:?} ready_to_first_signal={signal:?}",
            super::super::PAUSED_SILENCE_LIMIT,
            stopped.elapsed()
        );
    }

    #[test]
    #[ignore = "runs the production dormant zero-clock path through its real 45-second cutoff and a 30-second stopped AirPods pause"]
    fn live_airpods_dormant_retained_resume_after_expired_zero_window() {
        let route = stereo_bluetooth_output().expect("select the stereo AirPods output");
        let route_current = || crate::spatial::bluetooth_output_id() == Some(route);
        let rate = 48_000;
        let lead = route_lead(rate, route).unwrap();
        let mut standby = StandbyRenderer::begin_cold(rate, route, lead, true, route_current)
            .unwrap()
            .unwrap();
        let paused = Instant::now();
        standby.keep_paused_until(paused + super::super::PAUSED_SILENCE_LIMIT);
        let mut was_ready = false;
        while paused.elapsed() + Duration::from_millis(12) < super::super::PAUSED_SILENCE_LIMIT {
            assert!(route_current());
            was_ready |= standby.keep_paused_warm().unwrap();
            std::thread::sleep(Duration::from_millis(8));
        }
        while paused.elapsed() < super::super::PAUSED_SILENCE_LIMIT {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(was_ready);
        let stopped = Instant::now();
        let tail = standby.warming.as_ref().unwrap().silence_end;
        // The first expiry turn can already carry the user's Resume. It must
        // enter settling retention instead of the expired ordinary warm path.
        assert!(!standby.resume_paused_silence(route_current).unwrap());
        assert!(standby.warming.as_ref().unwrap().retained.is_some());
        let mut anchor = None;
        while stopped.elapsed() < Duration::from_secs(30) {
            assert!(route_current());
            standby.suspend_paused_silence(route_current).unwrap();
            let warmup = standby.warming.as_ref().unwrap();
            assert_eq!(
                warmup.silence_end, tail,
                "dormant retention appended synthetic buffers"
            );
            assert_eq!(
                warmup.position(standby.renderer.presented_through(), Instant::now(), rate),
                None
            );
            if let Some(clock) = warmup.retained
                && let Some(observed) = clock.anchor()
            {
                assert_eq!(*anchor.get_or_insert(observed), observed);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let anchor = anchor.expect("the dormant stopped clock never settled");
        let resume = Instant::now();
        while !standby.resume_paused_silence(route_current).unwrap() {
            assert!(route_current());
            assert!(resume.elapsed() < COLD_START_LIMIT);
            std::thread::sleep(Duration::from_millis(4));
        }
        let ready = resume.elapsed();
        assert_eq!(standby.warming.as_ref().unwrap().origin, anchor);
        let mut relay = Relay::open(rate, route, Some(standby), || true)
            .unwrap()
            .unwrap();
        assert_eq!(
            relay.prepared_silence_started.map(|(origin, _)| origin),
            Some(anchor),
            "the dormant resume must reuse its retained clock rather than open a cold epoch"
        );
        let signal = play_quiet_epoch(&mut relay, 2_000_001, 440.0);
        relay.pause();
        assert!(relay.queue.frames.is_empty() && relay.clock_map.is_none());
        eprintln!(
            "airpods_retained_resume path=dormant zero_window={:?} stopped={:?} anchor={anchor} resume_to_ready={ready:?} ready_to_first_signal={signal:?}",
            super::super::PAUSED_SILENCE_LIMIT,
            stopped.elapsed()
        );
    }

    #[test]
    #[ignore = "runs synthetic silence on the selected stereo Bluetooth output"]
    fn live_airpods_natural_handoff_refills_short_silence_without_resetting_clock() {
        objc2::rc::autoreleasepool(|_| {
            let rate = 48_000;
            let route = stereo_bluetooth_output().expect("a stereo Bluetooth output is required");
            let lead = route_lead(rate, route).unwrap();
            let horizon = (lead + rate as usize / 12) as u64;
            let route_current = || crate::spatial::bluetooth_output_id() == Some(route);
            let mut standby = StandbyRenderer::begin_cold(rate, route, lead, true, route_current)
                .expect("open a synthetic standby renderer")
                .expect("the Bluetooth route must remain current");
            let warming = standby.warming.as_ref().unwrap();
            let started = warming.started;
            let original_end = warming.silence_end;
            assert_eq!(original_end, horizon);

            let (before, remaining) = loop {
                assert!(
                    route_current(),
                    "the Bluetooth route changed during preroll"
                );
                assert!(
                    started.elapsed() < COLD_START_LIMIT,
                    "native clock did not advance into the short tail"
                );
                if let Some(through) = standby.renderer.presented_through().filter(|through| {
                    crate::spatial::start_position_is_current(0, *through, started.elapsed(), rate)
                }) {
                    let remaining = original_end.saturating_sub(through);
                    assert!(
                        remaining > 0,
                        "the native renderer exhausted its initial silence"
                    );
                    if remaining < lead as u64 {
                        break (through, remaining);
                    }
                }
                std::thread::sleep(Duration::from_millis(2));
            };
            assert!(standby.warm_tail(lead).is_none());
            assert!(
                !standby.renderer.take_flush(),
                "the native queue flushed before refill"
            );

            let refill = Instant::now();
            let new_end = standby
                .natural_warm_tail(lead, route_current)
                .expect("one refill should provide sufficient native silence");
            let refill_elapsed = refill.elapsed();
            let after = standby.renderer.presented_through().unwrap();
            assert_eq!(standby.warming.as_ref().unwrap().started, started);
            assert!(after >= before);
            assert!(new_end > original_end);
            assert!(new_end >= before + horizon && new_end <= after + horizon);
            assert!(new_end - after >= lead as u64);

            let advance = Instant::now();
            while standby.renderer.presented_through().unwrap_or_default() <= after {
                assert!(
                    advance.elapsed() < Duration::from_millis(50),
                    "native clock stopped after refill"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(route_current());
            eprintln!(
                "airpods_natural_tail rate={rate} lead={lead} horizon={horizon} before={before} remaining={remaining} original_end={original_end} new_end={new_end} added={} after={after} refill={refill_elapsed:?}",
                new_end - original_end,
            );
        });
    }

    #[test]
    #[ignore = "plays two original quiet epochs and synthetic successor preroll on the selected stereo Bluetooth output"]
    fn live_airpods_coalesced_successor_initializes_preroll_after_retirement() {
        use std::cell::Cell;
        objc2::rc::autoreleasepool(|_| {
            let rate = 48_000;
            let route = stereo_bluetooth_output().expect("a stereo Bluetooth output is required");
            let route_current = || stereo_bluetooth_output() == Some(route);
            let mut relay = Relay::open(rate, route, None, route_current)
                .unwrap()
                .unwrap();
            play_quiet_epoch(&mut relay, 1, 330.0);
            let mut standby = relay
                .into_standby(None)
                .expect("the outgoing renderer remains healthy");
            let released_at = Instant::now();
            assert!(
                !standby.has_preroll(),
                "No idle worker turn may warm this coalesced fixture"
            );
            let checks = Cell::new(0);
            let successor_is_current = || {
                checks.set(checks.get() + 1);
                route_current()
            };
            standby
                .finish_requested_handoff(rate, route, released_at, successor_is_current)
                .unwrap();
            assert!(
                checks.get() > 1,
                "A None-warming successor bypassed scoped preroll admission"
            );
            let warm = standby.has_preroll();
            if let Some(warming) = &standby.warming {
                assert_eq!(warming.origin, 0);
                assert!(warming.paused_format.is_some());
                assert!(warming.scratch.iter().all(|samples| *samples == [0.0; 2]));
                assert!(
                    warming.scratch.len() <= standby.lead + rate as usize / 12 + rate as usize / 25
                );
                assert!(standby.warm_tail(standby.lead).is_some());
            } else {
                // Native readiness can miss the existing 250/210 ms budget.
                // That is a bounded, emptied cold fallback, not old-clock proof.
                assert!(released_at.elapsed() >= NEXT_PREROLL_MAX_WAIT);
            }
            let ready_at = released_at.elapsed();
            let Some(mut next) = Relay::open(rate, route, Some(standby), route_current).unwrap()
            else {
                panic!("the same scoped route must open its successor");
            };
            assert!(next.queue.frames.is_empty() && next.queue.submitted == 0);
            assert!(next.clock_map.is_none() && next.submitted_through.is_none());
            let first_signal = play_quiet_epoch(&mut next, 1_000_001, 550.0);
            next.pause();
            assert!(route_current());
            eprintln!(
                "airpods_coalesced_next warm={warm} admission_checks={} retire_to_ready={ready_at:?} ready_to_signal={first_signal:?}",
                checks.get()
            );
        });
    }

    #[test]
    #[ignore = "plays two original quiet epochs per comparison on the selected stereo Bluetooth output"]
    fn live_airpods_compare_running_clock_handoff_after_full_flush() {
        objc2::rc::autoreleasepool(|_| {
            let rate = 48_000;
            let route = stereo_bluetooth_output().expect("a stereo Bluetooth output is required");
            let format = paused_route_format(route).expect("a stable stereo format is required");
            let route_current = || paused_route_format(route) == Some(format);

            for trial in 1..=3 {
                for retain_clock in [trial % 2 == 0, trial % 2 != 0] {
                    let mut outgoing = Relay::open(rate, route, None, route_current)
                        .unwrap()
                        .unwrap();
                    play_quiet_epoch(&mut outgoing, 1, 330.0);
                    assert!(outgoing.queue.submitted > 0 && outgoing.clock_map.is_some());
                    assert!(route_current());
                    let retired_at = Instant::now();
                    let mut standby = if retain_clock {
                        outgoing.into_running_next_standby(
                            retired_at + Duration::from_millis(1_250),
                            route_current,
                        )
                    } else {
                        outgoing.into_standby(None)
                    }
                    .expect("retire the outgoing renderer");
                    assert_eq!(standby.keeps_running_handoff(), retain_clock);
                    assert!(standby.warming.as_ref().is_none_or(|warming| {
                        warming.scratch.iter().all(|&sample| sample == [0.0; 2])
                    }));
                    let released_at = Instant::now();
                    // Simulate the observed preparation of a new tap, without
                    // registering or opening a process tap in this fixture.
                    while released_at.elapsed() < Duration::from_millis(45) {
                        assert!(route_current());
                        standby.warm_next().unwrap();
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    standby
                        .finish_requested_handoff(rate, route, released_at, route_current)
                        .unwrap();
                    assert_eq!(standby.keeps_running_handoff(), retain_clock);
                    let OpenResult::Ready(mut successor) = Relay::open_for_session(
                        rate,
                        route,
                        Some(standby),
                        None,
                        None,
                        retain_clock.then_some(&route_current as &dyn Fn() -> bool),
                        || {
                            if route_current() {
                                OpenAuthorization::Playing
                            } else {
                                OpenAuthorization::Revoked
                            }
                        },
                    )
                    .unwrap() else {
                        panic!("the same authorized route must open its successor");
                    };
                    let to_reuse = retired_at.elapsed();
                    assert!(successor.queue.frames.is_empty());
                    assert!(successor.queue.submitted == 0);
                    assert!(successor.clock_map.is_none());
                    let reused_silence = successor.warm_frames > 0;
                    if retain_clock {
                        assert!(reused_silence);
                        assert!(successor.prepared_silence_started.is_none());
                    }
                    let reuse_to_signal = play_quiet_epoch(&mut successor, 1_000_001, 550.0);
                    successor.pause();
                    assert!(route_current());
                    eprintln!(
                        "airpods_running_handoff trial={trial} retained={retain_clock} warmth={reused_silence} retire_to_ready={to_reuse:?} ready_to_signal={reuse_to_signal:?} retire_to_signal={:?}",
                        to_reuse + reuse_to_signal,
                    );
                }
            }
        });
    }

    #[test]
    #[ignore = "plays two original quiet epochs around an already-started AirPods natural handoff"]
    fn live_airpods_natural_handoff_waits_for_the_existing_silent_clock() {
        objc2::rc::autoreleasepool(|_| {
            let rate = 48_000;
            let route = stereo_bluetooth_output().expect("a stereo Bluetooth output is required");
            let route_current = || stereo_bluetooth_output() == Some(route);
            let mut relay = Relay::open(rate, route, None, route_current)
                .unwrap()
                .unwrap();
            play_quiet_epoch(&mut relay, 1, 330.0);
            let mut standby = relay
                .into_standby(None)
                .expect("retain the flushed renderer");
            let released_at = Instant::now();
            while !standby.has_preroll() {
                assert!(released_at.elapsed() < NEXT_PREROLL_LIMIT);
                assert!(route_current());
                standby.warm_next().unwrap();
                std::thread::sleep(Duration::from_millis(2));
            }
            let started = standby.warming.as_ref().unwrap().started;
            while started.elapsed() < Duration::from_millis(190) {
                assert!(route_current());
                std::thread::sleep(Duration::from_millis(2));
            }
            let needed_wait = standby.warm_tail(standby.lead).is_none();
            standby
                .finish_requested_handoff(rate, route, released_at, route_current)
                .unwrap();
            assert_eq!(
                standby
                    .warming
                    .as_ref()
                    .expect("the original startup must remain live")
                    .started,
                started
            );
            assert!(
                standby.warm_tail(standby.lead).is_some(),
                "A starting natural handoff was reset instead of using the original bounded wait"
            );
            let ready = started.elapsed();
            let OpenResult::Ready(mut next) = Relay::open_for_session(
                rate,
                route,
                Some(standby),
                None,
                Some(&route_current),
                None,
                || {
                    if route_current() {
                        OpenAuthorization::Playing
                    } else {
                        OpenAuthorization::Revoked
                    }
                },
            )
            .unwrap() else {
                panic!("the same natural handoff must remain authorized");
            };
            assert!(!next.needs_start && next.warm_frames > 0);
            assert!(next.queue.frames.is_empty() && next.clock_map.is_none());
            let signal = play_quiet_epoch(&mut next, 1_000_001, 550.0);
            next.pause();
            assert!(route_current());
            eprintln!(
                "airpods_existing_natural needed_wait={needed_wait} original_start_to_ready={ready:?} ready_to_signal={signal:?}"
            );
        });
    }

    #[test]
    #[ignore = "plays an original quiet stereo signal through the system renderer"]
    fn live_renderer_presents_a_paced_pcm_stream() {
        let rate = 48_000;
        let route = default_output_id().expect("a system output is required");
        let mut relay = Relay::open(rate, route, None, || true).unwrap().unwrap();
        let mut position = 0;
        let mut presented = 0;
        let mut flushed_at = None;
        let mut paused_at = None;
        let mut resumed_at = None;
        let mut first_resumed = None;
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(3000) {
            if paused_at.is_none() && start.elapsed() >= Duration::from_millis(1300) {
                assert!(
                    presented > flushed_at.unwrap() + u64::from(rate / 4),
                    "only presented {presented}"
                );
                relay.pause();
                assert!(relay.muted && relay.queue.frames.is_empty());
                paused_at = Some(presented);
            }
            let ready = (start.elapsed().as_secs_f64() * f64::from(rate)) as u64;
            if paused_at.is_some() && resumed_at.is_none() {
                if start.elapsed() < Duration::from_millis(1700) {
                    position = ready;
                    std::thread::sleep(Duration::from_millis(3));
                    continue;
                }
                resumed_at = Some(position);
                relay.capture_started();
            }
            while position < ready && relay.can_accept() {
                position += 1;
                let phase = position as f32 * std::f32::consts::TAU * 300.0 / rate as f32;
                relay
                    .push(Sample {
                        samples: [0.004 * phase.sin(), 0.004 * phase.cos()],
                        position,
                    })
                    .unwrap_or_else(|_| panic!("at {:?}, received {position}, presented {presented}, queued {}, submitted {}", start.elapsed(), relay.queue.frames.len(), relay.queue.submitted));
            }
            assert!(
                ready - position <= super::super::capture::CAPACITY as u64,
                "source would overflow the bounded capture ring"
            );
            if flushed_at.is_none() && start.elapsed() >= Duration::from_millis(700) {
                assert!(presented > u64::from(rate / 4));
                let clock = relay.clock_map;
                relay.reset_after_flush();
                assert_eq!(relay.clock_map, clock);
                flushed_at = Some(position);
            }
            let before = Instant::now();
            relay
                .tick(
                    |frame| {
                        if resumed_at.is_some() && first_resumed.is_none() {
                            first_resumed = Some(frame.position);
                        }
                        presented = frame.position;
                    },
                    || true,
                )
                .unwrap_or_else(|failure| {
                    panic!(
                        "{failure:?} at {:?}, queued {}, submitted {}, presenting {presented}",
                        start.elapsed(),
                        relay.queue.frames.len(),
                        relay.queue.submitted
                    )
                });
            if before.elapsed() > Duration::from_millis(30) {
                eprintln!(
                    "slow renderer turn {:?} at {:?}",
                    before.elapsed(),
                    start.elapsed()
                );
            }
            std::thread::sleep(Duration::from_millis(3));
        }
        assert_eq!(first_resumed, Some(resumed_at.unwrap() + 1));
        assert!(!relay.muted);
        assert!(position - presented <= relay.queue.limit as u64);

        // Use the native renderer on the existing device without requiring a
        // Bluetooth route. Cancellation must return to muted, empty time zero.
        relay.pause();
        let warming = Instant::now();
        loop {
            assert!(warming.elapsed() < COLD_START_LIMIT);
            if objc2::rc::autoreleasepool(|_| relay.warm_paused_renderer()).unwrap() {
                break;
            }
            std::thread::sleep(Duration::from_millis(4));
        }
        assert!(relay.queue.frames.is_empty() && relay.clock_map.is_none());
        assert!(!relay.muted && relay.warm_frames >= relay.lead as u64);
        let cancelled = Instant::now();
        relay.cancel_prewarm();
        while relay.renderer.presented_through() != Some(0) {
            assert!(cancelled.elapsed() < Duration::from_millis(500));
            std::thread::sleep(Duration::from_millis(4));
        }
        assert!(relay.muted && relay.needs_start && relay.resume_warmup.is_none());
        assert_eq!(relay.warm_frames, 0);
        eprintln!(
            "resume renderer, silence warmed and cancelled at {:?}",
            warming.elapsed()
        );

        let warming = Instant::now();
        while !objc2::rc::autoreleasepool(|_| relay.warm_paused_renderer()).unwrap() {
            assert!(warming.elapsed() < COLD_START_LIMIT);
            std::thread::sleep(Duration::from_millis(4));
        }
        let discarded_silence_end = relay.warm_frames;
        let restart = Instant::now();
        relay.capture_started();
        let mut position = 0;
        let mut next_frame = || {
            position += 1;
            let phase = position as f32 * std::f32::consts::TAU * 380.0 / rate as f32;
            Sample {
                samples: [0.004 * phase.sin(), 0.004 * phase.cos()],
                position,
            }
        };
        let initial_frames = u64::from(rate / 25);
        for _ in 0..initial_frames {
            relay.push(next_frame()).unwrap();
        }
        assert!(discarded_silence_end > relay.lead as u64);
        assert!(relay.clock_map.is_none() && relay.queue.submitted == 0);
        relay.reset_after_flush();
        assert_eq!(relay.warm_frames, 0);
        assert!(relay.muted && relay.needs_start);
        assert_eq!(relay.queue.frames.len(), initial_frames as usize);
        let mut first_presented = None;
        let mut captured = initial_frames;
        while first_presented.is_none() && restart.elapsed() < Duration::from_millis(800) {
            let ready = initial_frames + (restart.elapsed().as_secs_f64() * f64::from(rate)) as u64;
            while captured < ready && relay.can_accept() {
                relay.push(next_frame()).unwrap();
                captured += 1;
            }
            relay
                .tick(
                    |sample| {
                        first_presented.get_or_insert(sample.position);
                    },
                    || true,
                )
                .unwrap();
            std::thread::sleep(Duration::from_millis(3));
        }
        assert_eq!(first_presented, Some(1));
        assert_eq!(relay.clock_map, Some((0, relay.lead as u64)));
        eprintln!(
            "resume renderer, flush before first PCM discarded the old {discarded_silence_end}-frame silence tail and presented frame 1 in {:?}",
            restart.elapsed()
        );

        for (proof_ms, warm) in [(337, false), (337, true), (150, true)] {
            let skip = Instant::now();
            relay.silence();
            let mut standby = relay.into_standby(None).expect("healthy renderer");
            while skip.elapsed() < Duration::from_millis(proof_ms) {
                if warm {
                    standby.warm_next().expect("warm the same renderer");
                }
                std::thread::sleep(Duration::from_millis(3));
            }
            standby.finish_warmup(rate, route, None, || true);
            let mut replacement = Relay::open(rate, route, Some(standby), || true)
                .unwrap()
                .unwrap();
            assert_eq!(replacement.muted, !warm);
            assert_eq!(replacement.warm_frames > 0, warm);
            assert!(replacement.clock_map.is_none());
            assert!(replacement.queue.frames.is_empty());
            replacement.capture_started();
            let restarted = Instant::now();
            let mut position = 0;
            let mut first_presented = None;
            while restarted.elapsed() < Duration::from_millis(800) && first_presented.is_none() {
                let ready = (restarted.elapsed().as_secs_f64() * f64::from(rate)) as u64;
                while position < ready && replacement.can_accept() {
                    position += 1;
                    let phase = position as f32 * std::f32::consts::TAU * 440.0 / rate as f32;
                    replacement
                        .push(Sample {
                            samples: [0.004 * phase.sin(), 0.004 * phase.cos()],
                            position,
                        })
                        .unwrap();
                }
                replacement
                    .tick(
                        |frame| {
                            first_presented.get_or_insert(frame.position);
                        },
                        || true,
                    )
                    .unwrap();
                std::thread::sleep(Duration::from_millis(3));
            }
            assert_eq!(first_presented, Some(1));
            assert!(!replacement.muted);
            eprintln!(
                "next renderer, proof_ms={proof_ms}, warm={warm}, first fresh frame in {:?}",
                skip.elapsed()
            );
            relay = replacement;
        }

        relay.silence();
        let mut standby = relay
            .into_standby(None)
            .expect("healthy renderer before cancellation");
        let started = Instant::now();
        while standby.warm_tail(standby.lead).is_none() {
            assert!(started.elapsed() < Duration::from_millis(1250));
            standby.warm_next().expect("warm before an interruption");
            std::thread::sleep(Duration::from_millis(3));
        }
        let cancelled = Instant::now();
        standby.cancel_prewarm();
        assert!(standby.warm_tail(standby.lead).is_none());
        while standby.renderer.presented_through() != Some(0) {
            assert!(cancelled.elapsed() < Duration::from_millis(500));
            std::thread::sleep(Duration::from_millis(3));
        }
        eprintln!(
            "next renderer, cancelled, stopped in {:?}",
            cancelled.elapsed()
        );
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(standby.renderer.presented_through(), Some(0));
        let after_cancel = Relay::open(rate, route, Some(standby), || true)
            .unwrap()
            .unwrap();
        assert!(after_cancel.muted && after_cancel.needs_start);
        assert_eq!(after_cancel.warm_frames, 0);
        assert!(after_cancel.queue.frames.is_empty());
    }
}
