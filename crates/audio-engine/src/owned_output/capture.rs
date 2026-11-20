use super::{identity::Identity, live_trace};
use objc2::{
    AnyThread, msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject},
    sel,
};
use objc2_core_audio::*;
use objc2_core_audio_types::*;
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};
use player_domain::{HostAnalysisLease, HostAnalysisStatus as Issue};
use rtrb::{Consumer, Producer, RingBuffer};
use std::{
    ffi::{CStr, c_void},
    ptr::NonNull,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

const MAX_CALLBACK: usize = 4096;
pub(super) const CAPACITY: usize = 16384;
type CreateTap = unsafe extern "C-unwind" fn(*const CATapDescription, *mut u32) -> i32;
type DestroyTap = unsafe extern "C-unwind" fn(u32) -> i32;
#[derive(Clone, Copy)]
pub(super) struct Sample {
    pub samples: [f32; 2],
    pub position: u64,
}
struct Callback {
    producer: Producer<Sample>,
    planar: bool,
    enabled: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
    generation: u64,
    position: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}
pub(super) struct Capture {
    aggregate: u32,
    tap: u32,
    io: AudioDeviceIOProcID,
    started: bool,
    destroy: DestroyTap,
    context: NonNull<Callback>,
    position: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    consumer: Consumer<Sample>,
    sample_rate: u32,
    planar: bool,
    identity: Identity,
}
impl Capture {
    pub(super) fn prepare(
        lease: &HostAnalysisLease,
        enabled: Arc<AtomicU64>,
        playing: Arc<AtomicBool>,
        relay: bool,
        prime_renderer: Option<impl FnOnce(u32, Identity)>,
    ) -> Result<Self, Issue> {
        let identity =
            live_trace::measure("capture_identity_start", "capture_identity_done", || {
                Identity::verify(lease.process_id)
            })
            .ok_or(Issue::UnverifiedOutput)?;
        // Resolve only the 14.2 exports dynamically. They must not become strong
        // imports in a binary that still launches on macOS 14.0/14.1.
        let create = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                c"AudioHardwareCreateProcessTap".as_ptr(),
            )
        };
        let destroy = unsafe {
            libc::dlsym(
                libc::RTLD_DEFAULT,
                c"AudioHardwareDestroyProcessTap".as_ptr(),
            )
        };
        if create.is_null() || destroy.is_null() || AnyClass::get(c"CATapDescription").is_none() {
            return Err(Issue::UnsupportedSystem);
        }
        let create: CreateTap = unsafe { std::mem::transmute(create) };
        let destroy: DestroyTap = unsafe { std::mem::transmute(destroy) };
        let mut process = 0u32;
        let mut size = size_of::<u32>() as u32;
        let mut property = AudioObjectPropertyAddress {
            mSelector: kAudioHardwarePropertyTranslatePIDToProcessObject,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };
        let result =
            live_trace::measure("capture_process_start", "capture_process_done", || unsafe {
                AudioObjectGetPropertyData(
                    kAudioObjectSystemObject as u32,
                    NonNull::from(&mut property),
                    size_of::<i32>() as u32,
                    (&lease.process_id as *const i32).cast(),
                    NonNull::from(&mut size),
                    NonNull::from(&mut process).cast(),
                )
            });
        if result != 0 || process == 0 {
            return Err(Issue::ProcessUnavailable);
        }
        let processes = NSArray::from_slice(&[&*NSNumber::new_u32(process)]);
        let description = unsafe {
            CATapDescription::initStereoMixdownOfProcesses(CATapDescription::alloc(), &processes)
        };
        unsafe {
            description.setName(&NSString::from_str(if relay {
                "Auvra spatial stereo"
            } else {
                "Auvra music light"
            }));
            description.setPrivate(true);
            description.setExclusive(false);
            description.setMuteBehavior(if relay {
                CATapMuteBehavior::MutedWhenTapped
            } else {
                CATapMuteBehavior::Unmuted
            });
            let can_disable_restore: bool =
                msg_send![&*description, respondsToSelector: sel!(setProcessRestoreEnabled:)];
            if can_disable_restore {
                description.setProcessRestoreEnabled(false);
            }
        }
        let mut tap = 0;
        let created = live_trace::measure(
            "capture_create_tap_start",
            "capture_create_tap_done",
            || unsafe { create(&*description, &mut tap) },
        );
        if created != 0 || tap == 0 {
            return Err(Issue::TapUnavailable);
        }
        let (producer, consumer) = RingBuffer::new(CAPACITY);
        let position = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let callback = Box::new(Callback {
            producer,
            planar: false,
            enabled,
            playing,
            generation: lease.generation,
            position: position.clone(),
            dropped: dropped.clone(),
        });
        // A raw, stable context avoids reborrowing callback-owned memory while
        // the worker drains the separate consumer and shared atomic counters.
        let context = NonNull::new(Box::into_raw(callback)).expect("Box pointer is nonnull");
        let mut capture = Self {
            aggregate: 0,
            tap,
            io: None,
            started: false,
            destroy,
            consumer,
            sample_rate: 0,
            planar: false,
            identity,
            context,
            position,
            dropped,
        };
        if let Some(prime_renderer) = prime_renderer {
            let format = live_trace::measure(
                "capture_tap_format_start",
                "capture_tap_format_done",
                || {
                    read_format(
                        tap,
                        kAudioTapPropertyFormat,
                        kAudioObjectPropertyScopeGlobal,
                    )
                },
            );
            if let Some(format) = format.filter(|format| supported(*format)) {
                // Let native output start on silence while HAL creates the device.
                // Its actual input format is independently validated below.
                prime_renderer(format.mSampleRate as u32, identity);
            }
        }
        let name = NSString::from_str("Auvra private Soloist audio");
        let uid = NSUUID::new().UUIDString();
        let tap_uid = unsafe { description.UUID().UUIDString() };
        let yes = NSNumber::new_bool(true);
        let no = NSNumber::new_bool(false);
        let sub = dictionary(&[
            (kAudioSubTapUIDKey, &*tap_uid),
            (kAudioSubTapDriftCompensationKey, &*yes),
        ]);
        let taps = NSArray::from_slice(&[&*sub]);
        let settings = dictionary(&[
            (kAudioAggregateDeviceNameKey, &*name),
            (kAudioAggregateDeviceUIDKey, &*uid),
            (kAudioAggregateDeviceIsPrivateKey, &*yes),
            (kAudioAggregateDeviceIsStackedKey, &*no),
            // Relay owns the startup watchdog. Waiting for the source inside
            // AudioDeviceStart would bypass it if playback just paused.
            (
                kAudioAggregateDeviceTapAutoStartKey,
                if relay { &*no } else { &*yes },
            ),
            (kAudioAggregateDeviceTapListKey, &*taps),
        ]);
        // NSDictionary and CFDictionary are toll-free bridged. The SDK owns a
        // copy; temporary Foundation objects are released by the worker pool.
        let settings = unsafe {
            &*((&*settings as *const NSDictionary<NSString, AnyObject>).cast::<CFDictionary>())
        };
        let created = live_trace::measure(
            "capture_aggregate_start",
            "capture_aggregate_done",
            || unsafe {
                AudioHardwareCreateAggregateDevice(settings, NonNull::from(&mut capture.aggregate))
            },
        );
        if created != 0 {
            return Err(Issue::DeviceUnavailable);
        }
        let format = live_trace::measure("capture_format_start", "capture_format_done", || {
            read_format(
                capture.aggregate,
                kAudioDevicePropertyStreamFormat,
                kAudioObjectPropertyScopeInput,
            )
        })
        .filter(|format| supported(*format))
        .ok_or(Issue::UnsupportedFormat)?;
        capture.sample_rate = format.mSampleRate as u32;
        capture.planar = format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
        unsafe {
            capture.context.as_mut().planar = capture.planar;
        }
        Ok(capture)
    }
    pub(super) fn start(
        &mut self,
        validate_lease_and_route: impl FnOnce() -> Result<(), Issue>,
    ) -> Result<(), Issue> {
        // Register only after the renderer is prepared and the worker
        // revalidates its lease. Discarding a stale, prepared tap then has
        // no registered IOProc to tear down through HAL.
        let context = self.context.as_ptr().cast();
        if unsafe {
            AudioDeviceCreateIOProcID(
                self.aggregate,
                Some(copy_audio),
                context,
                NonNull::from(&mut self.io),
            )
        } != 0
            || self.io.is_none()
        {
            return Err(Issue::StartUnavailable);
        }
        live_trace::event("capture_io_registered", None);
        if !self.identity.alive() {
            return Err(Issue::ProcessUnavailable);
        }
        validate_lease_and_route()?;
        let started = live_trace::measure(
            "capture_hal_start_begin",
            "capture_hal_start_end",
            || unsafe { AudioDeviceStart(self.aggregate, self.io) },
        );
        if started != 0 {
            return Err(Issue::StartUnavailable);
        }
        self.started = true;
        Ok(())
    }
    pub(super) fn into_prepared_pause(mut self) -> Option<Self> {
        if self.started {
            return None;
        }
        if self.io.is_some() {
            let destroyed = unsafe { AudioDeviceDestroyIOProcID(self.aggregate, self.io) };
            if destroyed != 0 {
                return None;
            }
            self.io = None;
        }
        Some(self)
    }
    pub(super) fn prepared_is_current(&self) -> bool {
        if self.started || self.io.is_some() || !self.identity.alive() {
            return false;
        }
        live_trace::measure(
            "capture_prepared_format_start",
            "capture_prepared_format_done",
            || {
                read_format(
                    self.aggregate,
                    kAudioDevicePropertyStreamFormat,
                    kAudioObjectPropertyScopeInput,
                )
            },
        )
        .is_some_and(|format| {
            supported(format)
                && format.mSampleRate == f64::from(self.sample_rate)
                && (format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0) == self.planar
                && self.identity.alive()
        })
    }
    pub(super) fn rate(&self) -> u32 {
        self.sample_rate
    }
    pub(super) fn process_is_alive(&self) -> bool {
        self.identity.alive()
    }
    pub(super) fn identity(&self) -> Identity {
        self.identity
    }
    pub(super) fn pop(&mut self) -> Option<Sample> {
        self.consumer.pop().ok()
    }
    pub(super) fn discard_pending(&mut self) {
        for _ in 0..CAPACITY {
            if self.pop().is_none() {
                break;
            }
        }
    }
    pub(super) fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
    pub(super) fn position(&self) -> u64 {
        self.position.load(Ordering::Acquire)
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        unsafe {
            if self.started {
                AudioDeviceStop(self.aggregate, self.io);
            }
            if self.io.is_some() {
                AudioDeviceDestroyIOProcID(self.aggregate, self.io);
            }
            if self.aggregate != 0 {
                AudioHardwareDestroyAggregateDevice(self.aggregate);
            }
            if self.tap != 0 {
                (self.destroy)(self.tap);
            }
            drop(Box::from_raw(self.context.as_ptr()));
        }
        // IO is stopped and unregistered before callback/ring memory is freed.
    }
}
fn dictionary(entries: &[(&CStr, &AnyObject)]) -> Retained<NSDictionary<NSString, AnyObject>> {
    let keys: Vec<_> = entries
        .iter()
        .map(|(key, _)| NSString::from_str(key.to_str().expect("SDK ASCII key")))
        .collect();
    NSDictionary::from_slices(
        &keys.iter().map(|key| &**key).collect::<Vec<_>>(),
        &entries.iter().map(|(_, value)| *value).collect::<Vec<_>>(),
    )
}
fn read_format(object: u32, selector: u32, scope: u32) -> Option<AudioStreamBasicDescription> {
    let mut format: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
    let mut size = size_of::<AudioStreamBasicDescription>() as u32;
    let mut property = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: scope,
        mElement: kAudioObjectPropertyElementMain,
    };
    let result = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&mut property),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut format).cast(),
        )
    };
    (result == 0 && size as usize == size_of::<AudioStreamBasicDescription>()).then_some(format)
}
fn supported(format: AudioStreamBasicDescription) -> bool {
    format.mFormatID == kAudioFormatLinearPCM
        && format.mFormatFlags & kAudioFormatFlagIsFloat != 0
        && format.mFormatFlags & kAudioFormatFlagIsBigEndian == 0
        && format.mBitsPerChannel == 32
        && format.mChannelsPerFrame == 2
        && format.mSampleRate.is_finite()
        && format.mSampleRate.fract() == 0.0
        && (8000.0..=384000.0).contains(&format.mSampleRate)
        && format.mBytesPerFrame
            == if format.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0 {
                4
            } else {
                8
            }
}
// CoreAudio serializes this IOProc. It only performs bounded copies and lock-free
// atomics. No allocations, logging, FFT, IO, Objective-C or Swift calls here.
unsafe extern "C-unwind" fn copy_audio(
    _: u32,
    _: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _: NonNull<AudioTimeStamp>,
    _: NonNull<AudioBufferList>,
    _: NonNull<AudioTimeStamp>,
    context: *mut c_void,
) -> i32 {
    let state = unsafe { &mut *context.cast::<Callback>() };
    if state.enabled.load(Ordering::Acquire) != state.generation
        || !state.playing.load(Ordering::Acquire)
    {
        return 0;
    }
    let input = unsafe { input.as_ref() };
    if input.mNumberBuffers != if state.planar { 2 } else { 1 } {
        return 0;
    }
    let first = &input.mBuffers[0];
    if first.mData.is_null() || first.mNumberChannels != if state.planar { 1 } else { 2 } {
        return 0;
    }
    let left = first.mData.cast::<f32>();
    let mut right = left;
    let mut frames = first.mDataByteSize as usize / if state.planar { 4 } else { 8 };
    if state.planar {
        let second = unsafe { &*input.mBuffers.as_ptr().add(1) };
        if second.mData.is_null() || second.mNumberChannels != 1 {
            return 0;
        }
        right = second.mData.cast();
        frames = frames.min(second.mDataByteSize as usize / 4);
    }
    let start = state.position.load(Ordering::Relaxed);
    let bounded = frames.min(MAX_CALLBACK);
    let mut copied = 0;
    for index in 0..bounded {
        let samples = unsafe {
            if state.planar {
                [*left.add(index), *right.add(index)]
            } else {
                [*left.add(index * 2), *left.add(index * 2 + 1)]
            }
        };
        if state
            .producer
            .push(Sample {
                samples,
                position: start.saturating_add(index as u64 + 1),
            })
            .is_err()
        {
            break;
        }
        copied += 1;
    }
    state
        .dropped
        .fetch_add((frames - copied) as u64, Ordering::Relaxed);
    state
        .position
        .store(start.saturating_add(frames as u64), Ordering::Release);
    0
}
#[cfg(test)]
mod tests {
    use super::*;
    fn callback(producer: Producer<Sample>, planar: bool) -> Callback {
        Callback {
            producer,
            planar,
            enabled: Arc::new(AtomicU64::new(1)),
            playing: Arc::new(AtomicBool::new(true)),
            generation: 1,
            position: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }
    fn copy(state: &mut Callback, input: &mut AudioBufferList) {
        unsafe {
            copy_audio(
                0,
                NonNull::dangling(),
                NonNull::from(input),
                NonNull::dangling(),
                NonNull::dangling(),
                NonNull::dangling(),
                (state as *mut Callback).cast(),
            );
        }
    }
    #[test]
    fn callback_is_bounded_accounts_overflow_and_does_not_copy_a_revoked_lease() {
        let (producer, mut consumer) = RingBuffer::new(CAPACITY);
        let mut state = callback(producer, false);
        let mut pcm = vec![0.02f32; 10000];
        let mut input = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: (pcm.len() * 4) as u32,
                mData: pcm.as_mut_ptr().cast(),
            }],
        };
        copy(&mut state, &mut input);
        let first = consumer.pop().unwrap();
        assert_eq!(first.samples, [0.02; 2]);
        assert_eq!(first.position, 1);
        let mut count = 1;
        while consumer.pop().is_ok() {
            count += 1;
        }
        assert_eq!(count, MAX_CALLBACK);
        assert_eq!(state.dropped.load(Ordering::Relaxed), 904);
        for _ in 0..5 {
            copy(&mut state, &mut input);
        }
        let mut count = 0;
        while consumer.pop().is_ok() {
            count += 1;
        }
        assert_eq!(count, CAPACITY);
        assert_eq!(
            state.dropped.load(Ordering::Relaxed),
            30000 - MAX_CALLBACK as u64 - CAPACITY as u64
        );
        state.playing.store(false, Ordering::Release);
        copy(&mut state, &mut input);
        assert!(consumer.pop().is_err());
        assert_eq!(state.position.load(Ordering::Acquire), 30000);
        state.playing.store(true, Ordering::Release);
        state.enabled.store(0, Ordering::Release);
        copy(&mut state, &mut input);
        assert!(consumer.pop().is_err());
        assert_eq!(state.position.load(Ordering::Acquire), 30000);
    }
    #[test]
    fn planar_copy_uses_both_channels_and_the_shorter_buffer() {
        #[repr(C)]
        struct StereoList {
            count: u32,
            buffers: [AudioBuffer; 2],
        }
        let (producer, mut consumer) = RingBuffer::new(8);
        let mut state = callback(producer, true);
        let (mut left, mut right) = ([0.1f32, 0.2, 0.3], [0.4f32, 0.5]);
        let mut input = StereoList {
            count: 2,
            buffers: [
                AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: 12,
                    mData: left.as_mut_ptr().cast(),
                },
                AudioBuffer {
                    mNumberChannels: 1,
                    mDataByteSize: 8,
                    mData: right.as_mut_ptr().cast(),
                },
            ],
        };
        copy(&mut state, unsafe {
            &mut *(&mut input as *mut StereoList).cast::<AudioBufferList>()
        });
        assert_eq!(consumer.pop().unwrap().samples, [0.1, 0.4]);
        assert_eq!(consumer.pop().unwrap().samples, [0.2, 0.5]);
        assert!(consumer.pop().is_err());
    }
    #[test]
    fn format_rejects_integer_multichannel_nonfinite_and_bad_stride() {
        let mut f = AudioStreamBasicDescription {
            mSampleRate: 48000.0,
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        assert!(supported(f));
        f.mFormatFlags = 0;
        assert!(!supported(f));
        f.mFormatFlags = kAudioFormatFlagIsFloat;
        f.mChannelsPerFrame = 6;
        assert!(!supported(f));
        f.mChannelsPerFrame = 2;
        f.mSampleRate = f64::NAN;
        assert!(!supported(f));
        f.mSampleRate = 48000.0;
        f.mBytesPerFrame = 4;
        assert!(!supported(f));
        f.mFormatFlags |= kAudioFormatFlagIsNonInterleaved;
        assert!(supported(f));
    }
}
