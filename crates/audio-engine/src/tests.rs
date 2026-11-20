use super::*;
use crate::{
    output::{OutputFrame, Renderer},
    resample::PcmPipeline,
    shared::Shared,
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
thread_local! {static COUNTING:Cell<bool>=const {Cell::new(false)}; static ALLOCATIONS:Cell<usize>=const {Cell::new(0)};}
struct CountingAllocator;
fn count() {
    if COUNTING.try_with(Cell::get).unwrap_or(false) {
        let _ = ALLOCATIONS.try_with(|v| v.set(v.get() + 1));
    }
}
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc(l) }
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        count();
        unsafe { System.alloc_zeroed(l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        count();
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        count();
        unsafe { System.dealloc(p, l) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;
struct Temp(PathBuf);
impl Temp {
    fn new(bytes: &[u8], extension: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "auvra-audio-{}-{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            extension
        ));
        fs::write(&path, bytes).unwrap();
        Self(path)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
fn decode_all(path: &std::path::Path) -> (AudioInfo, Vec<[f32; 2]>) {
    let mut decoder = FileDecoder::open(path).unwrap();
    let mut result = vec![];
    loop {
        let frames = decoder.read_frames(1000).unwrap();
        if frames.is_empty() {
            break;
        }
        result.extend(frames);
    }
    (decoder.info, result)
}
#[test]
fn wav_and_flac_decode_to_identical_normalized_pcm() {
    let wav = Temp::new(include_bytes!("../tests/fixtures/stereo-44100.wav"), "wav");
    let flac = Temp::new(
        include_bytes!("../tests/fixtures/stereo-44100.flac"),
        "flac",
    );
    let (info, a) = decode_all(&wav.0);
    let (other, b) = decode_all(&flac.0);
    assert_eq!(info.total_frames, 8820);
    assert_eq!(info.sample_rate, 44100);
    assert_eq!(info.channels, 2);
    assert_eq!(a.len(), 8820);
    assert_eq!(other.total_frames, 8820);
    assert_eq!(a, b);
    let mut decoder = FileDecoder::open(&flac.0).unwrap();
    decoder.seek(5307).unwrap();
    assert_eq!(decoder.read_frames(100).unwrap(), b[5307..5407]);
    decoder.seek(0).unwrap();
    assert_eq!(decoder.read_frames(100).unwrap(), b[..100]);
    assert!(a.iter().any(|f| f[0].abs() > 0.01));
    assert!(a.iter().flatten().all(|v| v.is_finite() && v.abs() <= 1.0));
}
#[test]
fn mono_mapping_and_sample_accurate_seek_match_reference() {
    let file = Temp::new(include_bytes!("../tests/fixtures/mono-22050.wav"), "wav");
    let (_, reference) = decode_all(&file.0);
    assert!(reference.iter().all(|f| f[0] == f[1]));
    let mut decoder = FileDecoder::open(&file.0).unwrap();
    decoder.seek(1307).unwrap();
    assert_eq!(decoder.read_frames(100).unwrap(), reference[1307..1407]);
    decoder.seek(0).unwrap();
    assert_eq!(decoder.read_frames(30).unwrap(), reference[..30]);
    decoder.seek(2205).unwrap();
    assert!(decoder.read_frames(100).unwrap().is_empty());
}
#[test]
fn resampling_trims_delay_and_tail_to_the_expected_duration() {
    let wav = Temp::new(include_bytes!("../tests/fixtures/stereo-44100.wav"), "wav");
    let mut pipeline = PcmPipeline::new(FileDecoder::open(&wav.0).unwrap(), 48000).unwrap();
    let mut output = vec![];
    while !pipeline.finished() {
        output.extend(pipeline.next_chunk().unwrap());
    }
    assert_eq!(output.len(), 9600);
    assert_eq!(output.last().unwrap().1, 9600);
    assert!(
        output
            .iter()
            .skip(100)
            .take(4000)
            .any(|f| f.0[0].abs() > 0.01)
    );
    pipeline.seek(50).unwrap();
    let mut remaining = 0;
    while !pipeline.finished() {
        remaining += pipeline.next_chunk().unwrap().len();
    }
    assert_eq!(remaining, 7200);
}
#[test]
fn decode_rejects_invalid_and_truncated_files() {
    let invalid = Temp::new(b"this is not audio", "wav");
    assert!(FileDecoder::open(&invalid.0).is_err());
    let wav = include_bytes!("../tests/fixtures/stereo-44100.wav");
    let truncated = Temp::new(&wav[..wav.len() - 1000], "wav");
    let result = FileDecoder::open(&truncated.0).and_then(|mut d| {
        while !d.read_frames(1024)?.is_empty() {}
        Ok(())
    });
    assert!(result.is_err());
}
#[test]
fn callback_is_heap_free_discards_old_generations_and_does_not_advance_on_silence() {
    let shared = Arc::new(Shared::default());
    shared.generation.store(2, Ordering::Release);
    shared.ready.store(2, Ordering::Release);
    shared.stopped.store(false, Ordering::Release);
    shared.wanted_playing.store(true, Ordering::Release);
    shared.volume.store(1.0_f32.to_bits(), Ordering::Relaxed);
    let (mut producer, consumer) = rtrb::RingBuffer::new(8);
    producer
        .push(OutputFrame {
            samples: [0.8; 2],
            generation: 1,
            position: 1,
            last: false,
        })
        .unwrap();
    producer
        .push(OutputFrame {
            samples: [0.1, -0.1],
            generation: 2,
            position: 48001,
            last: false,
        })
        .unwrap();
    let mut renderer = Renderer {
        analysis: None,
        consumer,
        shared: shared.clone(),
        channels: 2,
        stream_epoch: 0,
    };
    let mut output = [0.0_f32; 8];
    ALLOCATIONS.with(|v| v.set(0));
    COUNTING.with(|v| v.set(true));
    renderer.render(&mut output);
    COUNTING.with(|v| v.set(false));
    assert_eq!(ALLOCATIONS.with(Cell::get), 0);
    assert_eq!(&output[..2], &[0.1, -0.1]);
    assert_eq!(&output[2..], &[0.0; 6]);
    assert_eq!(shared.clock.read(), Some((2, 48001)));
    renderer.render(&mut output);
    assert_eq!(output, [0.0; 8]);
    assert_eq!(shared.clock.read(), Some((2, 48001)));
}
#[test]
fn paused_seek_flushes_only_stale_pcm_and_preserves_the_new_position() {
    let shared = Arc::new(Shared::default());
    shared.generation.store(2, Ordering::Release);
    shared.ready.store(2, Ordering::Release);
    shared.stopped.store(false, Ordering::Release);
    let (mut producer, consumer) = rtrb::RingBuffer::new(8);
    producer
        .push(OutputFrame {
            samples: [0.8; 2],
            generation: 1,
            position: 3,
            last: false,
        })
        .unwrap();
    producer
        .push(OutputFrame {
            samples: [0.2; 2],
            generation: 2,
            position: 48001,
            last: true,
        })
        .unwrap();
    let mut renderer = Renderer {
        analysis: None,
        consumer,
        shared: shared.clone(),
        channels: 2,
        stream_epoch: 0,
    };
    let mut output = [1.0_f32; 4];
    renderer.render(&mut output);
    assert_eq!(output, [0.0; 4]);
    assert_eq!(renderer.consumer.slots(), 1);
    shared.wanted_playing.store(true, Ordering::Release);
    renderer.render(&mut output);
    assert_eq!(shared.clock.read(), Some((2, 48001)));
    assert_eq!(shared.ended.load(Ordering::Acquire), 2);
}

#[test]
fn callback_leaves_future_pcm_in_the_reused_ring_until_its_generation_is_ready() {
    let shared = Arc::new(Shared::default());
    shared.generation.store(2, Ordering::Release);
    shared.ready.store(2, Ordering::Release);
    shared.stopped.store(false, Ordering::Release);
    shared.wanted_playing.store(true, Ordering::Release);
    shared.volume.store(1.0_f32.to_bits(), Ordering::Relaxed);
    let (mut producer, consumer) = rtrb::RingBuffer::new(8);
    for (generation, sample) in [(1, 0.8), (2, 0.4), (3, 0.2)] {
        producer
            .push(OutputFrame {
                samples: [sample; 2],
                generation,
                position: 1,
                last: false,
            })
            .unwrap();
    }
    let mut renderer = Renderer {
        analysis: None,
        consumer,
        shared: shared.clone(),
        channels: 2,
        stream_epoch: 0,
    };
    let mut output = [1.0_f32; 4];
    renderer.render(&mut output);
    assert_eq!(output, [0.4, 0.4, 0.0, 0.0]);
    assert_eq!(renderer.consumer.slots(), 1);
    shared.generation.store(3, Ordering::Release);
    renderer.render(&mut output);
    assert_eq!(output, [0.0; 4]);
    assert_eq!(renderer.consumer.slots(), 1);
    shared.ready.store(3, Ordering::Release);
    renderer.render(&mut output);
    assert_eq!(output, [0.2, 0.2, 0.0, 0.0]);
    assert_eq!(shared.clock.read(), Some((3, 1)));
}

#[test]
fn downsampling_removes_content_above_the_output_nyquist_limit() {
    let file = Temp::new(include_bytes!("../tests/fixtures/stereo-96000.wav"), "wav");
    let mut pipeline = PcmPipeline::new(FileDecoder::open(&file.0).unwrap(), 48000).unwrap();
    let mut output = vec![];
    while !pipeline.finished() {
        output.extend(pipeline.next_chunk().unwrap());
    }
    assert_eq!(output.len(), 4800);
    let rms = |channel: usize| {
        (output
            .iter()
            .skip(200)
            .take(4000)
            .map(|f| f64::from(f.0[channel]).powi(2))
            .sum::<f64>()
            / 4000.0)
            .sqrt()
    };
    assert!(rms(0) > 0.05, "1kHz signal lost");
    assert!(rms(1) < 0.001, "30kHz aliased into 48kHz output");
}

#[test]
fn library_codecs_decode_finite_pcm_and_seek_after_a_full_read() {
    let cases: [(&[u8], &str); 5] = [
        (include_bytes!("../tests/fixtures/stereo.mp3"), "mp3"),
        (include_bytes!("../tests/fixtures/stereo.ogg"), "ogg"),
        (include_bytes!("../tests/fixtures/stereo-aac.m4a"), "m4a"),
        (include_bytes!("../tests/fixtures/stereo-alac.m4a"), "m4a"),
        (include_bytes!("../tests/fixtures/stereo.aiff"), "aiff"),
    ];
    for (data, extension) in cases {
        let file = Temp::new(data, extension);
        let (info, frames) = decode_all(&file.0);
        assert_eq!(info.sample_rate, 44100);
        assert_eq!(info.channels, 2);
        assert_eq!(frames.len() as u64, info.total_frames);
        assert!(frames.iter().flatten().all(|v| v.is_finite()));
        assert!(frames.iter().flatten().any(|v| v.abs() > 0.005));
        let mut decoder = FileDecoder::open(&file.0).unwrap();
        decoder.seek(info.total_frames / 2).unwrap();
        assert_eq!(decoder.read_frames(100).unwrap().len(), 100);
    }
}

#[test]
fn analysis_tap_is_heap_free_bounded_post_gain_and_never_stalls_output() {
    for channels in [1, 2] {
        let shared = Arc::new(Shared::default());
        shared.generation.store(7, Ordering::Release);
        shared.ready.store(7, Ordering::Release);
        shared.analysis_epoch.store(9, Ordering::Release);
        shared.stopped.store(false, Ordering::Release);
        shared.wanted_playing.store(true, Ordering::Release);
        shared.volume.store(0.5_f32.to_bits(), Ordering::Relaxed);
        let (mut source, consumer) = rtrb::RingBuffer::new(8);
        for i in 1..=8 {
            source
                .push(OutputFrame {
                    samples: [0.2, -0.2],
                    generation: 7,
                    position: i,
                    last: false,
                })
                .unwrap();
        }
        let (tap, mut received) = rtrb::RingBuffer::new(2);
        let mut renderer = Renderer {
            consumer,
            analysis: Some(tap),
            shared: shared.clone(),
            channels,
            stream_epoch: 0,
        };
        let mut output = vec![0_f32; 8 * channels];
        ALLOCATIONS.with(|v| v.set(0));
        COUNTING.with(|v| v.set(true));
        renderer.render(&mut output);
        COUNTING.with(|v| v.set(false));
        assert_eq!(ALLOCATIONS.with(Cell::get), 0);
        assert_eq!(shared.clock.read(), Some((7, 8)));
        assert_eq!(shared.statistics().media_frames, 8);
        assert_eq!(shared.analysis_dropped.load(Ordering::Relaxed), 6);
        for position in 1..=2 {
            let frame = received.pop().unwrap();
            assert_eq!(frame.position, position);
            assert_eq!(frame.generation, 7);
            assert_eq!(frame.epoch, 9);
            assert_eq!(
                frame.samples,
                if channels == 1 { [0.0; 2] } else { [0.1, -0.1] }
            );
        }
        assert!(received.pop().is_err());
    }
}

#[test]
fn analysis_worker_clears_pause_seek_and_old_stream_results() {
    use crate::analysis::{AnalysisSample, AnalysisWorker};
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let shared = Arc::new(Shared::default());
    shared.generation.store(1, Ordering::Release);
    shared.ready.store(1, Ordering::Release);
    shared.analysis_epoch.store(1, Ordering::Release);
    shared.stream_epoch.store(1, Ordering::Release);
    shared.wanted_playing.store(true, Ordering::Release);
    shared.stopped.store(false, Ordering::Release);
    let (worker, mut tap) = AnalysisWorker::spawn(shared.clone(), 48000, 1).unwrap();
    let wait = |condition: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(2));
        }
    };
    let mut feed = |epoch, offset, silent, count| {
        for i in 1..=count {
            let v = if silent {
                0.0
            } else {
                (std::f32::consts::TAU * 1000.0 * i as f32 / 48000.0).sin() * 0.5
            };
            let sample = AnalysisSample {
                samples: [v; 2],
                generation: 1,
                epoch,
                position: offset + i as u64,
            };
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                if tap.push(sample).is_ok() {
                    break;
                }
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
        }
    };
    feed(1, 0, false, 4096);
    wait(&|| shared.analysis.lock().unwrap().is_some());
    let first = shared.analysis.lock().unwrap().unwrap();
    assert!(first.frame.features.rms > 0.35);
    assert_eq!(first.frame.window_end_frame, 4096);
    feed(1, 100000, false, 128); // A dropped PCM span clears the old observation immediately.
    wait(&|| shared.analysis.lock().unwrap().is_none());
    shared.wanted_playing.store(false, Ordering::Release);
    shared.analysis_epoch.store(2, Ordering::Release);
    wait(&|| shared.analysis.lock().unwrap().is_none());
    shared.analysis_epoch.store(3, Ordering::Release);
    shared.wanted_playing.store(true, Ordering::Release);
    feed(1, 4096, false, 4096); // Callback work from before Pause cannot warm the new epoch.
    feed(3, 480000, true, 4096);
    wait(&|| {
        shared
            .analysis
            .lock()
            .unwrap()
            .is_some_and(|f| f.frame.generation == 3)
    });
    let fresh = shared.analysis.lock().unwrap().unwrap();
    assert_eq!(fresh.frame.features.rms, 0.0);
    assert_eq!(fresh.frame.window_start_frame, 480000);
    // Replacement stream owns its result even while the old worker exits.
    shared.stream_epoch.store(2, Ordering::Release);
    *shared.analysis.lock().unwrap() = Some(first);
    drop(worker);
    assert_eq!(*shared.analysis.lock().unwrap(), Some(first));
}
