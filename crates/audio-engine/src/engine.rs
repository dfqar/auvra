use crate::{
    decoder::{AudioInfo, FileDecoder},
    output::{Output, OutputFrame},
    resample::PcmPipeline,
    shared::{Details, Job, OutputStatistics, Shared},
};
use player_domain::{ErrorCode, PlaybackState, PlayerError};
use rtrb::Producer;
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    thread::{self, JoinHandle, Thread},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct AudioSnapshot {
    pub generation: u64,
    pub state: PlaybackState,
    pub position_ms: u64,
    pub info: Option<AudioInfo>,
    pub error: Option<PlayerError>,
    pub device_name: String,
    pub output_rate: u32,
    pub statistics: OutputStatistics,
    pub analysis: Option<player_domain::AnalysisFrame>,
}
pub struct AudioEngine {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
    wake: Thread,
}
impl AudioEngine {
    pub fn new() -> Result<Self, PlayerError> {
        let shared = Arc::new(Shared::default());
        let worker_shared = shared.clone();
        let worker = thread::Builder::new()
            .name("auvra-audio-decode".into())
            .spawn(move || {
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(&worker_shared)));
                if outcome.is_err() {
                    worker_shared.fail(
                        worker_shared.generation.load(Ordering::Acquire),
                        PlayerError::new(
                            ErrorCode::DecodeError,
                            "Audio worker stopped unexpectedly",
                        ),
                    );
                }
            })
            .map_err(|e| PlayerError::new(ErrorCode::ProviderUnavailable, e.to_string()))?;
        let wake = worker.thread().clone();
        Ok(Self {
            shared,
            worker: Some(worker),
            wake,
        })
    }
    pub fn is_alive(&self) -> bool {
        self.worker
            .as_ref()
            .is_some_and(|worker| !worker.is_finished())
    }
    pub fn load(&self, path: PathBuf) {
        self.load_at(path, 0, true);
    }
    pub fn load_at(&self, path: PathBuf, position_ms: u64, autoplay: bool) {
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.shared.analysis_epoch.fetch_add(1, Ordering::AcqRel);
        self.shared.stopped.store(false, Ordering::Release);
        self.shared
            .wanted_playing
            .store(autoplay, Ordering::Release);
        self.shared
            .requested_ms
            .store(position_ms, Ordering::Release);
        *self
            .shared
            .details
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Details {
            generation,
            ..Default::default()
        };
        *pending = Some(Job::Load { path, generation });
        drop(pending);
        self.wake.unpark();
    }
    pub fn seek(&self, position_ms: u64) {
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let generation = self.shared.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.shared.analysis_epoch.fetch_add(1, Ordering::AcqRel);
        self.shared.stopped.store(false, Ordering::Release);
        self.shared
            .requested_ms
            .store(position_ms, Ordering::Release);
        let mut details = self
            .shared
            .details
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        details.generation = generation;
        details.error = None;
        drop(details);
        // A load in progress must not be replaced by a seek before a session exists.
        if let Some(Job::Load { path, .. }) = pending.take() {
            *pending = Some(Job::Load { path, generation });
        } else {
            *pending = Some(Job::Seek {
                position_ms,
                generation,
            });
        }
        drop(pending);
        self.wake.unpark();
    }
    pub fn pause(&self) {
        {
            // Serialize with AVFoundation's worker-side clock publication.
            // Neither side holds this lock while calling an audio API.
            let _pending = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.shared.analysis_epoch.fetch_add(1, Ordering::AcqRel);
            self.shared.wanted_playing.store(false, Ordering::Release);
        }
        self.wake.unpark();
    }
    pub fn resume(&self) {
        self.shared.analysis_epoch.fetch_add(1, Ordering::AcqRel);
        self.shared.wanted_playing.store(true, Ordering::Release);
        self.wake.unpark();
    }
    pub fn stop(&self) {
        let mut pending = self
            .shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        self.shared.analysis_epoch.fetch_add(1, Ordering::AcqRel);
        self.shared.stopped.store(true, Ordering::Release);
        self.shared.wanted_playing.store(false, Ordering::Release);
        self.shared.requested_ms.store(0, Ordering::Release);
        *pending = Some(Job::Stop);
        drop(pending);
        self.wake.unpark();
    }
    pub fn set_volume(&self, volume: f32) -> Result<(), PlayerError> {
        if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return Err(PlayerError::invalid("Volume must be between 0 and 1"));
        }
        self.shared
            .volume
            .store(volume.to_bits(), Ordering::Relaxed);
        Ok(())
    }
    pub fn set_equalizer(
        &self,
        settings: player_domain::EqualizerSettings,
    ) -> Result<(), PlayerError> {
        settings.validate()?;
        *self
            .shared
            .equalizer
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = settings;
        self.wake.unpark();
        Ok(())
    }
    /// Compact observation only; no metadata cloning or work on the realtime thread.
    pub fn output_statistics(&self) -> OutputStatistics {
        self.shared.statistics()
    }
    pub fn analysis(&self) -> Option<player_domain::AnalysisFrame> {
        let generation = self.shared.generation.load(Ordering::Acquire);
        if !self.shared.wanted_playing.load(Ordering::Acquire)
            || self.shared.stopped.load(Ordering::Acquire)
            || self.shared.device_failed()
            || self.shared.ready.load(Ordering::Acquire) != generation
            || self.shared.ended.load(Ordering::Acquire) == generation
        {
            return None;
        }
        let (clock_generation, position) = self.shared.clock.read()?;
        if clock_generation != generation {
            return None;
        }
        let rate = self.shared.rate.load(Ordering::Acquire);
        self.shared
            .analysis
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .filter(|observation| {
                let frame = &observation.frame;
                if observation.published_at.elapsed() > Duration::from_millis(250) {
                    return false;
                }
                frame.generation == self.shared.analysis_epoch.load(Ordering::Acquire)
                    && frame.sample_rate == rate
                    && frame.window_end_frame <= position
                    && position.saturating_sub(frame.window_end_frame) <= u64::from(rate) / 5
                    && frame.is_valid()
            })
            .map(|observation| observation.frame)
    }
    pub fn snapshot(&self) -> AudioSnapshot {
        let generation = self.shared.generation.load(Ordering::Acquire);
        let stopped = self.shared.stopped.load(Ordering::Acquire);
        let ready = self.shared.ready.load(Ordering::Acquire) == generation;
        let wanted = self.shared.wanted_playing.load(Ordering::Acquire);
        let ended = self.shared.ended.load(Ordering::Acquire) == generation;
        let details = self
            .shared
            .details
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let error = if details.generation == generation {
            details.error.clone()
        } else {
            None
        };
        let rate = self.shared.rate.load(Ordering::Acquire);
        let mut position = self.shared.requested_ms.load(Ordering::Acquire);
        if let Some((clock_generation, frames)) = self.shared.clock.read()
            && clock_generation == generation
            && rate > 0
        {
            position = ((u128::from(frames) * 1000) / u128::from(rate)) as u64;
        }
        if let Some(info) = &details.info {
            position = position.min(info.duration_ms);
        }
        let state = if stopped {
            PlaybackState::Stopped
        } else if error.is_some() {
            PlaybackState::Error
        } else if !ready {
            PlaybackState::Loading
        } else if !wanted {
            PlaybackState::Paused
        } else if ended {
            PlaybackState::Stopped
        } else {
            PlaybackState::Playing
        };
        let analysis = if state == PlaybackState::Playing {
            self.analysis()
        } else {
            None
        };
        AudioSnapshot {
            generation,
            state,
            position_ms: if stopped { 0 } else { position },
            info: details.info,
            error,
            device_name: details.device_name,
            output_rate: rate,
            statistics: self.shared.statistics(),
            analysis,
        }
    }
}
impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.shared.wanted_playing.store(false, Ordering::Release);
        self.shared.shutdown.store(true, Ordering::Release);
        self.wake.unpark();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
struct Session {
    pipeline: PcmPipeline,
    dsp: crate::equalizer::Equalizer,
    output: Output,
    producer: Producer<OutputFrame>,
    pending: Vec<([f32; 2], u64)>,
    offset: usize,
    generation: u64,
    started: bool,
    queued: usize,
}
impl Session {
    fn open(
        path: PathBuf,
        generation: u64,
        shared: &Arc<Shared>,
        mut previous: Option<Self>,
        allow_spatial: bool,
    ) -> Result<Self, PlayerError> {
        if let Some(previous) = &mut previous {
            previous.output.reset_spatial();
        }
        let decoder = FileDecoder::open(&path)?;
        let (output, producer) = Output::open(
            shared.clone(),
            previous.map(|session| (session.output, session.producer)),
            allow_spatial,
        )?;
        let pipeline = PcmPipeline::new(decoder, output.rate)?;
        {
            let mut details = shared.details.lock().unwrap_or_else(|e| e.into_inner());
            if shared.generation.load(Ordering::Acquire) == generation {
                details.info = Some(pipeline.decoder.info.clone());
                details.generation = generation;
                details.device_name = output.device_name.clone();
            }
        }
        Ok(Self {
            pipeline,
            dsp: crate::equalizer::Equalizer::new(
                output.rate,
                *shared.equalizer.lock().unwrap_or_else(|e| e.into_inner()),
            ),
            output,
            producer,
            pending: vec![],
            offset: 0,
            generation,
            started: false,
            queued: 0,
        })
    }
    fn seek(
        &mut self,
        position_ms: u64,
        generation: u64,
        shared: &Arc<Shared>,
    ) -> Result<(), PlayerError> {
        self.output.reset_spatial();
        self.pipeline.seek(position_ms)?;
        self.dsp
            .update(*shared.equalizer.lock().unwrap_or_else(|e| e.into_inner()));
        self.dsp.reset();
        self.generation = generation;
        self.pending.clear();
        self.offset = 0;
        self.queued = 0;
        self.started = false;
        let mut details = shared.details.lock().unwrap_or_else(|e| e.into_inner());
        if shared.generation.load(Ordering::Acquire) == generation {
            details.generation = generation;
            details.info = Some(self.pipeline.decoder.info.clone());
            details.device_name = self.output.device_name.clone();
        }
        Ok(())
    }
    fn step(&mut self, shared: &Arc<Shared>) -> Result<bool, PlayerError> {
        if shared.device_failed() {
            return Err(PlayerError::new(
                ErrorCode::AudioDeviceLost,
                "The audio output device was lost",
            ));
        }
        if self.generation != shared.generation.load(Ordering::Acquire) {
            return Ok(false);
        }
        let previously_queued = self.queued;
        if self.offset == self.pending.len() && !self.pipeline.finished() {
            self.pending = self.pipeline.next_chunk()?;
            let mut block = crate::PcmBlock {
                format: crate::AudioFormat {
                    sample_rate: self.output.rate,
                    channels: 2,
                },
                samples: self
                    .pending
                    .iter()
                    .flat_map(|(samples, _)| *samples)
                    .collect(),
            };
            self.dsp
                .update(*shared.equalizer.lock().unwrap_or_else(|e| e.into_inner()));
            crate::AudioProcessor::process(&mut self.dsp, &mut block);
            for ((samples, _), processed) in self
                .pending
                .iter_mut()
                .zip(block.samples.as_chunks::<2>().0)
            {
                samples.copy_from_slice(processed);
            }
            self.offset = 0;
        }
        while self.offset < self.pending.len() && self.producer.slots() > 0 {
            if self.generation != shared.generation.load(Ordering::Acquire) {
                return Ok(false);
            }
            let (samples, position) = self.pending[self.offset];
            let last = self.pipeline.finished() && self.offset + 1 == self.pending.len();
            if self
                .producer
                .push(OutputFrame {
                    samples,
                    generation: self.generation,
                    position,
                    last,
                })
                .is_err()
            {
                break;
            }
            self.offset += 1;
            self.queued += 1;
        }
        let finished = self.pipeline.finished() && self.offset == self.pending.len();
        if !self.started && (self.queued >= (self.output.rate as usize / 10).max(512) || finished) {
            self.output.play()?;
            self.started = true;
            shared.ready.store(self.generation, Ordering::Release);
            if self.queued == 0 && finished {
                shared.ended.store(self.generation, Ordering::Release);
            }
        }
        if self.started {
            self.output.tick(self.generation)?;
        }
        // An unstarted output can fill promptly while it has room. Recheck the
        // control mailbox before every chunk, and park if the ring is blocked.
        Ok(!self.started && self.queued > previously_queued && self.producer.slots() > 0)
    }
}
fn run(shared: &Arc<Shared>) {
    let mut session: Option<Session> = None;
    let mut last_path: Option<PathBuf> = None;
    while !shared.shutdown.load(Ordering::Acquire) {
        let job = shared
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(job) = job {
            match job {
                Job::Stop => {
                    session = None;
                }
                Job::Load { path, generation } => {
                    last_path = Some(path.clone());
                    match Session::open(path, generation, shared, session.take(), true) {
                        Ok(mut created) => {
                            let position = shared.requested_ms.load(Ordering::Acquire);
                            if position > 0
                                && let Err(error) = created.seek(position, generation, shared)
                            {
                                shared.fail(generation, error);
                                continue;
                            }
                            session = Some(created);
                        }
                        Err(error) => {
                            shared.fail(generation, error);
                        }
                    }
                }
                Job::Seek {
                    position_ms,
                    generation,
                } => {
                    if let Some(session) = session.as_mut() {
                        if let Err(error) = session.seek(position_ms, generation, shared) {
                            shared.fail(generation, error);
                        }
                    } else if let Some(path) = &last_path {
                        match Session::open(path.clone(), generation, shared, None, true).and_then(
                            |mut opened| {
                                opened.seek(position_ms, generation, shared)?;
                                Ok(opened)
                            },
                        ) {
                            Ok(opened) => session = Some(opened),
                            Err(error) => shared.fail(generation, error),
                        }
                    } else {
                        shared.fail(
                            generation,
                            PlayerError::new(ErrorCode::TrackUnavailable, "No local file is open"),
                        );
                    }
                }
            }
        }
        let step = session.as_mut().map(|current| current.step(shared));
        if matches!(step, Some(Ok(true))) {
            continue;
        }
        if let Some(Err(error)) = step
            && let Some(current) = session.as_mut()
        {
            let generation = current.generation;
            let fallback = current.output.is_spatial() && error.code == ErrorCode::AudioDeviceLost;
            let resume_at = shared
                .clock
                .read()
                .filter(|(observed, _)| *observed == generation)
                .map(|(_, frames)| {
                    ((u128::from(frames) * 1000) / u128::from(current.output.rate)) as u64
                })
                .unwrap_or_else(|| shared.requested_ms.load(Ordering::Acquire));
            session = None;
            let retry_path = last_path.as_ref().filter(|_| {
                if !fallback {
                    return false;
                }
                // Reset under the same lock as load/seek so a newer requested
                // position cannot be overwritten by an older device recovery.
                let _pending = shared.pending.lock().unwrap_or_else(|e| e.into_inner());
                if shared.generation.load(Ordering::Acquire) != generation {
                    return false;
                }
                shared.requested_ms.store(resume_at, Ordering::Release);
                shared.ready.store(0, Ordering::Release);
                shared.clock.write(0, 0);
                shared.analysis_epoch.fetch_add(1, Ordering::AcqRel);
                true
            });
            if let Some(path) = retry_path {
                match Session::open(path.clone(), generation, shared, None, false).and_then(
                    |mut opened| {
                        opened.seek(resume_at, generation, shared)?;
                        Ok(opened)
                    },
                ) {
                    Ok(opened) => session = Some(opened),
                    Err(fallback_error) => shared.fail(generation, fallback_error),
                }
            } else {
                shared.fail(generation, error);
            }
        }
        let delay = if shared.wanted_playing.load(Ordering::Acquire) {
            Duration::from_millis(2)
        } else {
            Duration::from_millis(25)
        };
        thread::park_timeout(delay);
    }
}

#[cfg(test)]
mod analysis_health_tests {
    use super::*;
    use crate::shared::PublishedAnalysis;
    use player_domain::{AnalysisFrame, AudioFeatures, SPECTRUM_BANDS};
    use std::time::Instant;

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "plays the original quiet fixtures on a stable CoreAudio output"]
    fn compatible_real_output_stays_open_for_playing_and_paused_loads() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/audio-fixtures");
        if crate::spatial::bluetooth_output() {
            let (output, _) = Output::open(Arc::new(Shared::default()), None, true).unwrap();
            if output.configured_for_stereo() {
                assert!(
                    output.is_spatial(),
                    "Stereo Bluetooth must opt in to AVFoundation"
                );
                println!("AVFoundation spatial stereo backend selected");
            }
        }
        let engine = AudioEngine::new().unwrap();
        engine.set_volume(0.2).unwrap();
        let wait = |condition: &dyn Fn(&AudioSnapshot) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let state = engine.snapshot();
                assert!(state.error.is_none(), "{state:?}");
                if condition(&state) {
                    return state;
                }
                assert!(Instant::now() < deadline, "{state:?}");
                thread::sleep(Duration::from_millis(2));
            }
        };
        let has_pcm =
            |state: &AudioSnapshot| state.state == PlaybackState::Playing && state.position_ms > 0;
        engine.load(root.join("Auvra Quiet Check.wav"));
        let first = wait(&has_pcm);
        let output_epoch = engine.shared.stream_epoch.load(Ordering::Acquire);
        engine.load(root.join("Auvra Mono Check.wav"));
        let second = wait(&has_pcm);
        assert!(second.generation > first.generation);
        assert_eq!(second.info.unwrap().channels, 1);
        assert_eq!(
            engine.shared.stream_epoch.load(Ordering::Acquire),
            output_epoch
        );

        engine.load_at(root.join("Auvra Quiet Check.flac"), 2000, false);
        let paused =
            wait(&|state| state.state == PlaybackState::Paused && state.position_ms == 2000);
        thread::sleep(Duration::from_millis(70));
        assert_eq!(
            engine.snapshot().statistics.media_frames,
            paused.statistics.media_frames
        );
        assert_eq!(
            engine.shared.stream_epoch.load(Ordering::Acquire),
            output_epoch
        );
        engine.resume();
        wait(&|state| has_pcm(state) && state.position_ms > 2000);
        engine.stop();
    }

    #[test]
    fn stalled_analysis_expires_even_when_output_sample_clock_stops() {
        let shared = Arc::new(Shared::default());
        shared.generation.store(1, Ordering::Release);
        shared.ready.store(1, Ordering::Release);
        shared.analysis_epoch.store(3, Ordering::Release);
        shared.rate.store(48000, Ordering::Release);
        shared.stopped.store(false, Ordering::Release);
        shared.wanted_playing.store(true, Ordering::Release);
        shared.clock.write(1, 4096);
        let frame = AnalysisFrame {
            generation: 3,
            sample_rate: 48000,
            window_frames: 4096,
            hop_frames: 960,
            window_start_frame: 0,
            window_end_frame: 4096,
            features: AudioFeatures {
                playback_position_ms: 85,
                ..Default::default()
            },
            spectrum: [0.0; SPECTRUM_BANDS],
            dropped_frames: 0,
        };
        *shared.analysis.lock().unwrap() = Some(PublishedAnalysis {
            frame,
            published_at: Instant::now(),
        });
        let engine = AudioEngine {
            shared: shared.clone(),
            worker: None,
            wake: thread::current(),
        };
        assert!(engine.analysis().is_some());
        shared
            .analysis
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .published_at = Instant::now() - Duration::from_millis(300);
        assert!(engine.analysis().is_none());
        shared
            .analysis
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .published_at = Instant::now();
        shared.analysis_epoch.fetch_add(1, Ordering::AcqRel);
        assert!(engine.analysis().is_none());
    }
}
