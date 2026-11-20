use crate::decoder::AudioInfo;
use player_domain::{AnalysisFrame, PlayerError};
use std::{
    path::PathBuf,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
    time::Instant,
};

pub use player_domain::OutputStatistics;
#[derive(Default)]
pub(crate) struct Clock {
    version: AtomicU64,
    generation: AtomicU64,
    frames: AtomicU64,
}
impl Clock {
    // The active output has one writer. Readers never spin on the audio thread.
    pub fn write(&self, generation: u64, frames: u64) {
        self.version.fetch_add(1, Ordering::AcqRel);
        self.generation.store(generation, Ordering::Relaxed);
        self.frames.store(frames, Ordering::Relaxed);
        self.version.fetch_add(1, Ordering::Release);
    }
    pub fn read(&self) -> Option<(u64, u64)> {
        for _ in 0..4 {
            let before = self.version.load(Ordering::Acquire);
            if before & 1 != 0 {
                continue;
            }
            let result = (
                self.generation.load(Ordering::Relaxed),
                self.frames.load(Ordering::Relaxed),
            );
            if before == self.version.load(Ordering::Acquire) {
                return Some(result);
            }
        }
        None
    }
}
#[derive(Clone, Debug)]
pub(crate) enum Job {
    Load { path: PathBuf, generation: u64 },
    Seek { position_ms: u64, generation: u64 },
    Stop,
}
#[derive(Clone, Default)]
pub(crate) struct Details {
    pub generation: u64,
    pub info: Option<AudioInfo>,
    pub error: Option<PlayerError>,
    pub device_name: String,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PublishedAnalysis {
    pub frame: AnalysisFrame,
    pub published_at: Instant,
}
pub(crate) struct Shared {
    pub analysis_epoch: AtomicU64,
    pub analysis_dropped: AtomicU64,
    pub analysis: Mutex<Option<PublishedAnalysis>>,
    pub generation: AtomicU64,
    pub ready: AtomicU64,
    pub ended: AtomicU64,
    pub wanted_playing: AtomicBool,
    pub stopped: AtomicBool,
    pub shutdown: AtomicBool,
    pub requested_ms: AtomicU64,
    pub rate: AtomicU32,
    pub volume: AtomicU32,
    pub failed_stream_epoch: AtomicU64,
    pub stream_epoch: AtomicU64,
    pub clock: Clock,
    pub pending: Mutex<Option<Job>>,
    pub equalizer: Mutex<player_domain::EqualizerSettings>,
    pub details: Mutex<Details>,
    pub callbacks: AtomicU64,
    pub media_frames: AtomicU64,
    pub nonzero_frames: AtomicU64,
    pub underrun_frames: AtomicU64,
}
impl Default for Shared {
    fn default() -> Self {
        Self {
            analysis_epoch: AtomicU64::new(0),
            analysis_dropped: AtomicU64::new(0),
            analysis: Mutex::new(None),
            generation: AtomicU64::new(0),
            ready: AtomicU64::new(0),
            ended: AtomicU64::new(0),
            wanted_playing: AtomicBool::new(false),
            stopped: AtomicBool::new(true),
            shutdown: AtomicBool::new(false),
            requested_ms: AtomicU64::new(0),
            rate: AtomicU32::new(0),
            volume: AtomicU32::new(0.7_f32.to_bits()),
            failed_stream_epoch: AtomicU64::new(0),
            stream_epoch: AtomicU64::new(0),
            clock: Clock::default(),
            pending: Mutex::new(None),
            equalizer: Mutex::new(player_domain::EqualizerSettings::default()),
            details: Mutex::new(Details::default()),
            callbacks: AtomicU64::new(0),
            media_frames: AtomicU64::new(0),
            nonzero_frames: AtomicU64::new(0),
            underrun_frames: AtomicU64::new(0),
        }
    }
}
impl Shared {
    pub fn device_failed(&self) -> bool {
        let failed = self.failed_stream_epoch.load(Ordering::Acquire);
        failed != 0 && failed == self.stream_epoch.load(Ordering::Acquire)
    }
    pub fn fail(&self, generation: u64, error: PlayerError) {
        let mut details = self.details.lock().unwrap_or_else(|e| e.into_inner());
        if self.generation.load(Ordering::Acquire) == generation {
            details.error = Some(error);
            details.generation = generation;
            self.wanted_playing.store(false, Ordering::Release);
            self.analysis_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }
    pub fn statistics(&self) -> OutputStatistics {
        OutputStatistics {
            analysis_dropped_frames: self.analysis_dropped.load(Ordering::Relaxed),
            callbacks: self.callbacks.load(Ordering::Relaxed),
            media_frames: self.media_frames.load(Ordering::Relaxed),
            nonzero_frames: self.nonzero_frames.load(Ordering::Relaxed),
            underrun_frames: self.underrun_frames.load(Ordering::Relaxed),
        }
    }
}
