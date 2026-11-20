//! Bounded tap of presented post-DSP/post-volume PCM. FFT and publication are worker-only.
use crate::shared::{PublishedAnalysis, Shared};
use audio_analysis::Analyzer;
use rtrb::{Producer, RingBuffer};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
pub(crate) struct AnalysisSample {
    pub samples: [f32; 2],
    pub position: u64,
    pub generation: u64,
    pub epoch: u64,
}
pub(crate) struct AnalysisWorker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl AnalysisWorker {
    pub fn spawn(
        shared: Arc<Shared>,
        rate: u32,
        stream_epoch: u64,
    ) -> std::io::Result<(Self, Producer<AnalysisSample>)> {
        let (producer, mut consumer) =
            RingBuffer::<AnalysisSample>::new((rate as usize / 8).clamp(2048, 65536));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::Builder::new()
            .name("auvra-audio-analysis".into())
            .spawn(move || {
                let Some(mut analyzer) = Analyzer::new(rate) else {
                    return;
                };
                let mut epoch = shared.analysis_epoch.load(Ordering::Acquire);
                let mut last_position: Option<u64> = None;
                while !stopping.load(Ordering::Acquire) && !shared.shutdown.load(Ordering::Acquire)
                {
                    let current_epoch = shared.analysis_epoch.load(Ordering::Acquire);
                    let active = shared.wanted_playing.load(Ordering::Acquire)
                        && !shared.stopped.load(Ordering::Acquire)
                        && !shared.device_failed()
                        && shared.ended.load(Ordering::Acquire)
                            != shared.generation.load(Ordering::Acquire)
                        && shared.stream_epoch.load(Ordering::Acquire) == stream_epoch;
                    if current_epoch != epoch || !active {
                        analyzer.reset();
                        epoch = current_epoch;
                        last_position = None;
                        // A stale output worker must never clear its replacement's observation.
                        clear(&shared, stream_epoch);
                    }
                    let mut latest = None;
                    let mut consumed = 0;
                    for _ in 0..4096 {
                        let Ok(sample) = consumer.pop() else {
                            break;
                        };
                        consumed += 1;
                        if !active
                            || sample.epoch != epoch
                            || sample.generation != shared.generation.load(Ordering::Acquire)
                        {
                            continue;
                        }
                        if last_position
                            .is_some_and(|last| last.checked_add(1) != Some(sample.position))
                            || sample
                                .samples
                                .iter()
                                .any(|v| !v.is_finite() || v.abs() > 1.0)
                        {
                            analyzer.reset();
                            latest = None;
                            clear(&shared, stream_epoch);
                        }
                        last_position = Some(sample.position);
                        if let Some(mut frame) =
                            analyzer.push(sample.samples, sample.position, sample.epoch)
                        {
                            frame.dropped_frames = shared.analysis_dropped.load(Ordering::Relaxed);
                            latest = Some(frame);
                        }
                    }
                    if let Some(frame) = latest {
                        let mut slot = shared.analysis.lock().unwrap_or_else(|e| e.into_inner());
                        if shared.analysis_epoch.load(Ordering::Acquire) == epoch
                            && shared.wanted_playing.load(Ordering::Acquire)
                            && shared.stream_epoch.load(Ordering::Acquire) == stream_epoch
                        {
                            *slot = Some(PublishedAnalysis {
                                frame,
                                published_at: Instant::now(),
                            });
                        }
                    }
                    if consumed < 4096 {
                        thread::park_timeout(Duration::from_millis(if active { 2 } else { 25 }));
                    }
                }
            })?;
        Ok((
            Self {
                stop,
                thread: Some(thread),
            },
            producer,
        ))
    }
}
impl Drop for AnalysisWorker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.thread.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

fn clear(shared: &Shared, stream_epoch: u64) {
    let mut slot = shared.analysis.lock().unwrap_or_else(|e| e.into_inner());
    if shared.stream_epoch.load(Ordering::Acquire) == stream_epoch {
        *slot = None;
    }
}
