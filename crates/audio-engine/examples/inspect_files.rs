//! Decoder-only interoperability check; all input is consumed on this process's worker thread.
use audio_engine::FileDecoder;
use std::path::Path;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    for path in std::env::args().skip(1) {
        let mut decoder = FileDecoder::open(Path::new(&path))?;
        let mut frames = 0usize;
        loop {
            let block = decoder.read_frames(1024)?;
            if block.is_empty() {
                break;
            }
            assert!(block.iter().flatten().all(|v| v.is_finite()));
            frames += block.len();
        }
        assert!(frames > 0);
        assert_eq!(frames as u64, decoder.info.total_frames);
        decoder.seek(decoder.info.total_frames / 2)?;
        assert!(!decoder.read_frames(100)?.is_empty());
        println!(
            "decoded + seek: {path}: {}Hz, {} channels, {}ms, {frames} frames",
            decoder.info.sample_rate, decoder.info.channels, decoder.info.duration_ms
        );
    }
    Ok(())
}
