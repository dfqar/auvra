//! The callback owns only an SPSC consumer and atomics; no decode/IO/mutex/heap work.
use crate::{
    analysis::{AnalysisSample, AnalysisWorker},
    shared::Shared,
};
use cpal::{
    DeviceId, FromSample, Sample, SampleFormat, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use player_domain::{ErrorCode, PlayerError};
use rtrb::{Consumer, Producer, RingBuffer};
use std::sync::{Arc, atomic::Ordering};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct OutputFrame {
    pub samples: [f32; 2],
    pub generation: u64,
    pub position: u64,
    pub last: bool,
}
pub(crate) struct Renderer {
    pub consumer: Consumer<OutputFrame>,
    pub analysis: Option<Producer<AnalysisSample>>,
    pub shared: Arc<Shared>,
    pub channels: usize,
    pub stream_epoch: u64,
}
impl Renderer {
    pub fn render<T: Sample + FromSample<f32>>(&mut self, output: &mut [T]) {
        output.fill(T::EQUILIBRIUM);
        self.shared.callbacks.fetch_add(1, Ordering::Relaxed);
        let generation = self.shared.generation.load(Ordering::Acquire);
        // Clear old PCM even while paused. A newer load can start filling the
        // ring while this callback is still looking at the previous generation.
        while self
            .consumer
            .peek()
            .is_ok_and(|frame| frame.generation < generation)
        {
            let _ = self.consumer.pop();
        }
        if self.shared.ready.load(Ordering::Acquire) != generation
            || !self.shared.wanted_playing.load(Ordering::Acquire)
            || self.shared.stopped.load(Ordering::Acquire)
            || self.shared.stream_epoch.load(Ordering::Acquire) != self.stream_epoch
            || self.shared.device_failed()
        {
            return;
        }
        let analysis_epoch = self.shared.analysis_epoch.load(Ordering::Acquire);
        let gain = f32::from_bits(self.shared.volume.load(Ordering::Relaxed));
        let mut last_clock = None;
        let mut ended = None;
        let mut played = 0;
        let mut nonzero = 0;
        let mut underrun = 0;
        for destination in output.chunks_exact_mut(self.channels) {
            if !self.shared.wanted_playing.load(Ordering::Acquire)
                || self.shared.generation.load(Ordering::Acquire) != generation
            {
                break;
            }
            let Ok(front) = self.consumer.peek() else {
                if ended != Some(generation)
                    && self.shared.ended.load(Ordering::Acquire) != generation
                {
                    underrun += 1;
                }
                continue;
            };
            if front.generation != generation {
                break;
            }
            let Ok(frame) = self.consumer.pop() else {
                break;
            };
            if self.shared.generation.load(Ordering::Acquire) != generation {
                break;
            }
            if self.channels == 1 {
                destination[0] = T::from_sample((frame.samples[0] + frame.samples[1]) * 0.5 * gain);
            } else {
                destination[0] = T::from_sample(frame.samples[0] * gain);
                destination[1] = T::from_sample(frame.samples[1] * gain);
            }
            if let Some(tap) = &mut self.analysis {
                // Copy only. A full tap drops analysis samples; output and its clock continue.
                if tap
                    .push(AnalysisSample {
                        samples: if self.channels == 1 {
                            [(frame.samples[0] + frame.samples[1]) * 0.5 * gain; 2]
                        } else {
                            frame.samples.map(|v| v * gain)
                        },
                        position: frame.position,
                        generation: frame.generation,
                        epoch: analysis_epoch,
                    })
                    .is_err()
                {
                    self.shared.analysis_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            played += 1;
            if frame.samples.iter().any(|v| v.abs() > 1e-8) && gain > 0.0 {
                nonzero += 1;
            }
            last_clock = Some((frame.generation, frame.position));
            if frame.last {
                ended = Some(frame.generation);
            }
        }
        if let Some((generation, position)) = last_clock {
            self.shared.clock.write(generation, position);
        }
        if let Some(generation) = ended {
            self.shared.ended.store(generation, Ordering::Release);
        }
        self.shared
            .media_frames
            .fetch_add(played, Ordering::Relaxed);
        self.shared
            .nonzero_frames
            .fetch_add(nonzero, Ordering::Relaxed);
        self.shared
            .underrun_frames
            .fetch_add(underrun, Ordering::Relaxed);
    }
}
pub(crate) struct Output {
    backend: Backend,
    _analysis: AnalysisWorker,
    pub rate: u32,
    pub device_name: String,
    shared: Arc<Shared>,
    epoch: u64,
    device_id: Option<DeviceId>,
    config: SupportedStreamConfig,
    started: bool,
}

enum Backend {
    Cpal(cpal::Stream),
    #[cfg(target_os = "macos")]
    Spatial(Box<crate::spatial::SpatialOutput>),
}

fn error(e: impl std::fmt::Display) -> PlayerError {
    PlayerError::new(
        ErrorCode::AudioDeviceLost,
        format!("Audio output is unavailable: {e}"),
    )
}
impl Output {
    pub fn open(
        shared: Arc<Shared>,
        previous: Option<(Self, Producer<OutputFrame>)>,
        _allow_spatial: bool,
    ) -> Result<(Self, Producer<OutputFrame>), PlayerError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| error("No default output device"))?;
        let config = device.default_output_config().map_err(error)?;
        let device_id = device.id().ok();
        if let Some((output, producer)) = previous
            && output.started
            && !shared.device_failed()
            && output.epoch == shared.stream_epoch.load(Ordering::Acquire)
            && device_id.is_some()
            && device_id == output.device_id
            && config == output.config
        {
            return Ok((output, producer));
        }
        let rate = config.sample_rate();
        let channels = usize::from(config.channels());
        if !(8000..=384000).contains(&rate) || !(1..=8).contains(&channels) {
            return Err(PlayerError::new(
                ErrorCode::UnsupportedFormat,
                "Unsupported output device configuration",
            ));
        }
        let device_name = device
            .description()
            .map(|d| d.name().to_owned())
            .unwrap_or_else(|_| "Default output".into());
        let (producer, consumer) = RingBuffer::new((rate as usize / 4).max(2048));
        let epoch = shared.stream_epoch.fetch_add(1, Ordering::AcqRel) + 1;
        shared.rate.store(rate, Ordering::Release);
        let (analysis_worker, analysis_tap) =
            AnalysisWorker::spawn(shared.clone(), rate, epoch).map_err(error)?;
        let cpal_output = |consumer, analysis_tap| {
            let renderer = Renderer {
                analysis: Some(analysis_tap),
                consumer,
                shared: shared.clone(),
                channels,
                stream_epoch: epoch,
            };
            macro_rules! build {
                ($sample:ty) => {{
                    let errors = shared.clone();
                    let mut renderer = renderer;
                    device
                        .build_output_stream::<$sample, _, _>(
                            config.config(),
                            move |data, _| renderer.render(data),
                            move |_| {
                                errors
                                    .failed_stream_epoch
                                    .fetch_max(epoch, Ordering::AcqRel);
                            },
                            None,
                        )
                        .map_err(error)?
                }};
            }
            Ok(Backend::Cpal(match config.sample_format() {
                SampleFormat::F32 => build!(f32),
                SampleFormat::F64 => build!(f64),
                SampleFormat::I8 => build!(i8),
                SampleFormat::I16 => build!(i16),
                SampleFormat::I32 => build!(i32),
                SampleFormat::I64 => build!(i64),
                SampleFormat::U8 => build!(u8),
                SampleFormat::U16 => build!(u16),
                SampleFormat::U32 => build!(u32),
                SampleFormat::U64 => build!(u64),
                SampleFormat::I24 => build!(cpal::I24),
                SampleFormat::U24 => build!(cpal::U24),
                _ => {
                    return Err(PlayerError::new(
                        ErrorCode::UnsupportedFormat,
                        "Unsupported device sample format",
                    ));
                }
            }))
        };
        #[cfg(target_os = "macos")]
        let backend = if _allow_spatial
            && channels == 2
            && crate::spatial::bluetooth_output()
            && let Ok(renderer) = crate::spatial::SpatialRenderer::open(rate)
        {
            Backend::Spatial(Box::new(crate::spatial::SpatialOutput::new(
                renderer,
                consumer,
                analysis_tap,
            )))
        } else {
            cpal_output(consumer, analysis_tap)?
        };
        #[cfg(not(target_os = "macos"))]
        let backend = cpal_output(consumer, analysis_tap)?;
        Ok((
            Self {
                backend,
                _analysis: analysis_worker,
                rate,
                device_name,
                shared,
                epoch,
                device_id,
                config,
                started: false,
            },
            producer,
        ))
    }
    pub fn play(&mut self) -> Result<(), PlayerError> {
        match &self.backend {
            Backend::Cpal(stream) => stream.play().map_err(error)?,
            #[cfg(target_os = "macos")]
            Backend::Spatial(_) => {}
        }
        self.started = true;
        Ok(())
    }

    pub fn reset_spatial(&mut self) {
        #[cfg(target_os = "macos")]
        if let Backend::Spatial(output) = &mut self.backend {
            output.reset();
        }
    }

    pub fn tick(&mut self, _generation: u64) -> Result<(), PlayerError> {
        #[cfg(target_os = "macos")]
        if let Backend::Spatial(output) = &mut self.backend {
            output.tick(&self.shared, _generation)?;
        }
        Ok(())
    }

    pub fn is_spatial(&self) -> bool {
        match self.backend {
            Backend::Cpal(_) => false,
            #[cfg(target_os = "macos")]
            Backend::Spatial(_) => true,
        }
    }

    #[cfg(all(test, target_os = "macos"))]
    pub fn configured_for_stereo(&self) -> bool {
        self.config.channels() == 2
    }
}
impl Drop for Output {
    fn drop(&mut self) {
        let _ = self.shared.stream_epoch.compare_exchange(
            self.epoch,
            self.epoch + 1,
            Ordering::AcqRel,
            Ordering::Relaxed,
        );
    }
}
