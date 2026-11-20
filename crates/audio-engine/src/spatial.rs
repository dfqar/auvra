//! AVFoundation output for stereo Bluetooth routes. Each renderer has one
//! audio worker owner. The OS owns the spatialization and head tracking.
use crate::{analysis::AnalysisSample, output::OutputFrame, shared::Shared};
use block2::RcBlock;
use objc2::{rc::Retained, runtime::ProtocolObject};
use objc2_av_foundation::*;
use objc2_core_audio::*;
use objc2_core_audio_types::*;
use objc2_core_foundation::CFRetained;
use objc2_core_media::*;
use objc2_foundation::{NSNotification, NSNotificationCenter, NSObjectProtocol};
use player_domain::{ErrorCode, PlayerError};
use rtrb::{Consumer, Producer};
use std::{
    collections::VecDeque,
    ptr::{self, NonNull},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) fn bluetooth_output() -> bool {
    bluetooth_output_id().is_some()
}

pub(crate) fn bluetooth_output_id() -> Option<u32> {
    let property = |object, selector| {
        let mut address = AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let mut size = size_of::<u32>() as u32;
        let mut value = 0u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                NonNull::from(&mut address),
                0,
                ptr::null(),
                NonNull::from(&mut size),
                NonNull::from(&mut value).cast(),
            )
        };
        (status == 0 && size as usize == size_of::<u32>()).then_some(value)
    };
    let device = property(
        kAudioObjectSystemObject as u32,
        kAudioHardwarePropertyDefaultOutputDevice,
    )
    .filter(|id| *id != kAudioObjectUnknown)?;
    let transport = property(device, kAudioDevicePropertyTransportType)?;
    (transport == kAudioDeviceTransportTypeBluetooth
        || transport == kAudioDeviceTransportTypeBluetoothLE)
        .then_some(device)
}

fn error() -> PlayerError {
    PlayerError::new(
        ErrorCode::AudioDeviceLost,
        "The system spatial audio output was interrupted",
    )
}

const CHUNK: usize = 1024;
const START_TIMEOUT: Duration = Duration::from_millis(1250);

pub(crate) struct SpatialRenderer {
    renderer: Retained<AVSampleBufferAudioRenderer>,
    clock: Retained<AVSampleBufferRenderSynchronizer>,
    format: CFRetained<CMAudioFormatDescription>,
    observer: Retained<ProtocolObject<dyn NSObjectProtocol>>,
    was_flushed: Arc<AtomicBool>,
    rate: i32,
}

impl SpatialRenderer {
    pub fn open(rate: u32) -> Result<Self, PlayerError> {
        objc2::rc::autoreleasepool(|_| unsafe {
            let mut asbd = AudioStreamBasicDescription {
                mSampleRate: f64::from(rate),
                mFormatID: kAudioFormatLinearPCM,
                mFormatFlags: kAudioFormatFlagsNativeFloatPacked,
                mBytesPerPacket: 8,
                mFramesPerPacket: 1,
                mBytesPerFrame: 8,
                mChannelsPerFrame: 2,
                mBitsPerChannel: 32,
                mReserved: 0,
            };
            let mut format = ptr::null();
            if CMAudioFormatDescriptionCreate(
                None,
                NonNull::from(&mut asbd),
                0,
                ptr::null(),
                0,
                ptr::null(),
                None,
                NonNull::from(&mut format),
            ) != 0
            {
                return Err(error());
            }
            let format = CFRetained::from_raw(NonNull::new(format.cast_mut()).ok_or_else(error)?);
            let renderer = AVSampleBufferAudioRenderer::new();
            let clock = AVSampleBufferRenderSynchronizer::new();
            renderer
                .setAllowedAudioSpatializationFormats(AVAudioSpatializationFormats::MonoAndStereo);
            clock.setDelaysRateChangeUntilHasSufficientMediaData(false);
            clock.addRenderer(ProtocolObject::from_ref(&*renderer));

            let was_flushed = Arc::new(AtomicBool::new(false));
            let interrupted = was_flushed.clone();
            let notify = RcBlock::new(move |_: NonNull<NSNotification>| {
                interrupted.store(true, Ordering::Release)
            });
            let observer = NSNotificationCenter::defaultCenter()
                .addObserverForName_object_queue_usingBlock(
                    Some(AVSampleBufferAudioRendererWasFlushedAutomaticallyNotification),
                    Some(renderer.as_ref()),
                    None,
                    &notify,
                );
            Ok(Self {
                renderer,
                clock,
                format,
                observer,
                was_flushed,
                rate: rate as i32,
            })
        })
    }

    pub(crate) fn enqueue(&self, start: u64, samples: &[[f32; 2]]) -> Result<(), PlayerError> {
        let size = size_of_val(samples);
        let mut raw_block = ptr::null_mut();
        let mut raw_buffer = ptr::null_mut();
        let block = unsafe {
            if CMBlockBuffer::create_with_memory_block(
                None,
                ptr::null_mut(),
                size,
                None,
                ptr::null(),
                0,
                size,
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::from(&mut raw_block),
            ) != 0
            {
                return Err(error());
            }
            CFRetained::from_raw(NonNull::new(raw_block).ok_or_else(error)?)
        };
        // The block owns a copy. AVFoundation can retain it beyond this worker turn.
        unsafe {
            if CMBlockBuffer::replace_data_bytes(NonNull::from(&samples[0]).cast(), &block, 0, size)
                != 0
            {
                return Err(error());
            }
            let timing = CMSampleTimingInfo {
                duration: CMTime::new(1, self.rate),
                presentationTimeStamp: CMTime::new(start as i64, self.rate),
                decodeTimeStamp: kCMTimeInvalid,
            };
            if CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(&self.format),
                samples.len() as isize,
                1,
                &timing,
                1,
                &size_of::<[f32; 2]>(),
                NonNull::from(&mut raw_buffer),
            ) != 0
            {
                return Err(error());
            }
            let buffer = CFRetained::from_raw(NonNull::new(raw_buffer).ok_or_else(error)?);
            self.renderer.enqueueSampleBuffer(&buffer);
        }
        Ok(())
    }

    fn pause(&self) {
        unsafe { self.clock.setRate_time(0.0, kCMTimeInvalid) }
    }

    pub(crate) fn reset_time(&self) {
        unsafe { self.clock.setRate_time(0.0, CMTime::new(0, self.rate)) }
    }

    pub(crate) fn start_at(&self, frame: u64) {
        unsafe {
            self.clock
                .setRate_time(1.0, CMTime::new(frame as i64, self.rate))
        }
    }

    pub(crate) fn pause_preserving_time(&self) {
        self.pause();
    }

    pub(crate) fn resume_preserving_time(&self) {
        unsafe { self.clock.setRate_time(1.0, kCMTimeInvalid) }
    }

    pub(crate) fn presented_through(&self) -> Option<u64> {
        let time = unsafe { self.clock.currentTime().seconds() };
        time.is_finite()
            .then(|| (time * f64::from(self.rate)).floor() as u64)
    }

    pub(crate) fn is_ready(&self) -> bool {
        unsafe { self.renderer.isReadyForMoreMediaData() }
    }

    pub(crate) fn set_muted(&self, muted: bool) {
        unsafe { self.renderer.setMuted(muted) }
    }

    pub(crate) fn failed(&self) -> bool {
        unsafe { self.renderer.status() == AVQueuedSampleBufferRenderingStatus::Failed }
    }

    pub(crate) fn take_flush(&self) -> bool {
        self.was_flushed.swap(false, Ordering::AcqRel)
    }

    fn flush(&self) {
        self.pause();
        self.discard_enqueued();
    }

    pub(crate) fn discard_enqueued(&self) {
        unsafe { self.renderer.flush() }
    }
}

impl Drop for SpatialRenderer {
    fn drop(&mut self) {
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(self.observer.as_ref()) }
        self.flush();
    }
}

pub(crate) struct SpatialOutput {
    native: SpatialRenderer,
    input: Consumer<OutputFrame>,
    tap: Producer<AnalysisSample>,
    scheduled: VecDeque<OutputFrame>,
    batch: Vec<OutputFrame>,
    samples: Vec<[f32; 2]>,
    limit: usize,
    source_at_zero: Option<u64>,
    state: RenderState,
    volume: f32,
}

enum RenderState {
    Stopped,
    Starting { frame: u64, requested_at: Instant },
    Playing,
}

pub(crate) fn start_position_is_current(
    frame: u64,
    position: u64,
    elapsed: Duration,
    rate: u32,
) -> bool {
    // AVFoundation applies timebase changes asynchronously. An old position
    // can still be readable after start, but cannot follow the new anchor
    // farther than its elapsed time plus a worker scheduling margin.
    let elapsed_frames = (elapsed + Duration::from_millis(5)).as_secs_f64() * f64::from(rate);
    position > frame && position - frame <= elapsed_frames.ceil() as u64
}

impl SpatialOutput {
    pub fn new(
        native: SpatialRenderer,
        input: Consumer<OutputFrame>,
        tap: Producer<AnalysisSample>,
    ) -> Self {
        let limit = (native.rate as usize / 4).max(CHUNK);
        Self {
            native,
            input,
            tap,
            scheduled: VecDeque::with_capacity(limit),
            batch: Vec::with_capacity(CHUNK),
            samples: Vec::with_capacity(CHUNK),
            limit,
            source_at_zero: None,
            state: RenderState::Stopped,
            volume: -1.0,
        }
    }

    pub fn reset(&mut self) {
        objc2::rc::autoreleasepool(|_| self.reset_renderer());
        self.scheduled.clear();
        while self.input.pop().is_ok() {}
    }

    fn reset_renderer(&mut self) {
        self.native.reset_time();
        self.native.discard_enqueued();
        self.source_at_zero = None;
        self.state = RenderState::Stopped;
    }

    pub fn tick(&mut self, shared: &Shared, generation: u64) -> Result<(), PlayerError> {
        objc2::rc::autoreleasepool(|_| self.pump(shared, generation))
    }

    fn pump(&mut self, shared: &Shared, generation: u64) -> Result<(), PlayerError> {
        if self.native.failed() {
            return Err(error());
        }
        let gain = f32::from_bits(shared.volume.load(Ordering::Relaxed));
        if gain != self.volume {
            unsafe { self.native.renderer.setVolume(gain) }
            self.volume = gain;
        }
        let wants_playback = shared.wanted_playing.load(Ordering::Acquire);
        if self.native.take_flush() {
            // Re-submit only the bounded, unpresented tail after a route flush.
            shared.analysis_epoch.fetch_add(1, Ordering::AcqRel);
            self.reset_renderer();
            for chunk in self.scheduled.make_contiguous().chunks(CHUNK) {
                let position = chunk[0].position - 1;
                let origin = *self.source_at_zero.get_or_insert(position);
                self.samples.clear();
                self.samples.extend(chunk.iter().map(|frame| frame.samples));
                self.native.enqueue(position - origin, &self.samples)?;
            }
        }
        if !matches!(self.state, RenderState::Stopped) {
            if wants_playback {
                self.present(shared, generation, gain)?;
                if self.scheduled.is_empty() {
                    if shared.ended.load(Ordering::Acquire) == generation {
                        self.native.pause();
                        self.state = RenderState::Stopped;
                    } else {
                        self.reset_renderer();
                    }
                }
            } else {
                self.native.pause();
                self.state = RenderState::Stopped;
            }
        }
        if !wants_playback || shared.ended.load(Ordering::Acquire) == generation {
            return Ok(());
        }
        while self.scheduled.len() < self.limit && self.native.is_ready() {
            let remaining = self.limit - self.scheduled.len();
            let size = remaining.min(CHUNK).min(self.input.slots());
            if size == 0 || (!matches!(self.state, RenderState::Stopped) && remaining < 256) {
                break;
            }
            self.batch.clear();
            for _ in 0..size {
                if let Ok(frame) = self.input.pop()
                    && frame.generation == generation
                {
                    self.batch.push(frame);
                }
            }
            if self.batch.is_empty() {
                break;
            }
            self.samples.clear();
            self.samples
                .extend(self.batch.iter().map(|frame| frame.samples));
            let position = self.batch[0].position - 1;
            let origin = *self.source_at_zero.get_or_insert(position);
            self.native.enqueue(position - origin, &self.samples)?;
            self.scheduled.extend(self.batch.iter().copied());
        }
        // Bluetooth may consume data ahead of the public clock. Prime at
        // least 200 ms when available, while allowing short tails to finish.
        if matches!(self.state, RenderState::Stopped)
            && let Some(first) = self.scheduled.front()
            && let Some(origin) = self.source_at_zero
            && (self.scheduled.len() >= self.native.rate as usize / 5
                || self.scheduled.back().is_some_and(|frame| frame.last)
                || !self.native.is_ready())
            && shared.wanted_playing.load(Ordering::Acquire)
            && shared.generation.load(Ordering::Acquire) == generation
        {
            let frame = first.position - 1 - origin;
            self.state = RenderState::Starting {
                frame,
                requested_at: Instant::now(),
            };
            self.native.start_at(frame);
        }
        Ok(())
    }

    fn present(&mut self, shared: &Shared, generation: u64, gain: f32) -> Result<(), PlayerError> {
        let position = self.native.presented_through();
        if let RenderState::Starting {
            frame,
            requested_at,
        } = self.state
        {
            let elapsed = requested_at.elapsed();
            if !position.is_some_and(|position| {
                start_position_is_current(frame, position, elapsed, self.native.rate as u32)
            }) {
                return if elapsed >= START_TIMEOUT {
                    Err(error())
                } else {
                    Ok(())
                };
            }
            self.state = RenderState::Playing;
        }
        let Some(position) = self
            .source_at_zero
            .zip(position)
            .map(|(origin, position)| origin.saturating_add(position))
        else {
            return Ok(());
        };
        if !shared.wanted_playing.load(Ordering::Acquire)
            || shared.generation.load(Ordering::Acquire) != generation
        {
            return Ok(());
        }
        let played = self
            .scheduled
            .partition_point(|frame| frame.position <= position);
        let Some(last) = played
            .checked_sub(1)
            .and_then(|index| self.scheduled.get(index))
        else {
            return Ok(());
        };
        let epoch = {
            let _pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
            if !shared.wanted_playing.load(Ordering::Acquire)
                || shared.generation.load(Ordering::Acquire) != generation
            {
                return Ok(());
            }
            shared.clock.write(generation, last.position);
            if last.last {
                shared.ended.store(generation, Ordering::Release);
            }
            shared.analysis_epoch.load(Ordering::Acquire)
        };
        let mut nonzero = 0;
        for frame in self.scheduled.drain(..played) {
            if self
                .tap
                .push(AnalysisSample {
                    samples: frame.samples.map(|sample| sample * gain),
                    position: frame.position,
                    generation,
                    epoch,
                })
                .is_err()
            {
                shared.analysis_dropped.fetch_add(1, Ordering::Relaxed);
            }
            nonzero +=
                u64::from(gain > 0.0 && frame.samples.iter().any(|sample| sample.abs() > 1e-8));
        }
        shared.callbacks.fetch_add(1, Ordering::Relaxed);
        shared
            .media_frames
            .fetch_add(played as u64, Ordering::Relaxed);
        shared.nonzero_frames.fetch_add(nonzero, Ordering::Relaxed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_rejects_old_timebase_and_accepts_only_plausible_progress() {
        let rate = 48_000;
        let elapsed = Duration::from_millis(12);
        assert!(!start_position_is_current(0, 57 * 48_000, elapsed, rate));
        assert!(!start_position_is_current(0, 4_800, elapsed, rate));
        assert!(!start_position_is_current(48_000, 0, elapsed, rate));
        assert!(!start_position_is_current(48_000, 48_000, elapsed, rate));
        assert!(start_position_is_current(0, 96, elapsed, rate));
        assert!(start_position_is_current(48_000, 48_096, elapsed, rate));
    }
}
