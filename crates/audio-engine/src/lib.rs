//! M2 boundary: Symphonia decoder worker -> channel map/resample -> DSP -> SPSC -> CPAL.
//! File decoding and output live on the audio worker; the callback only consumes ready frames.
#[derive(Clone, Copy, Debug)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}
impl AudioFormat {
    pub fn is_valid(self) -> bool {
        self.sample_rate > 0 && self.channels > 0
    }
}
/// Interleaved f32 PCM, owned by the decode/DSP worker; never crosses C ABI.
pub struct PcmBlock {
    pub format: AudioFormat,
    pub samples: Vec<f32>,
}
pub trait AudioProcessor: Send {
    fn process(&mut self, block: &mut PcmBlock);
}
#[derive(Default)]
pub struct DspChain {
    processors: Vec<Box<dyn AudioProcessor>>,
}
impl DspChain {
    pub fn push(&mut self, processor: impl AudioProcessor + 'static) {
        self.processors.push(Box::new(processor));
    }
    pub fn process(&mut self, block: &mut PcmBlock) {
        for processor in &mut self.processors {
            processor.process(block);
        }
    }
}

mod analysis;
mod decoder;
mod engine;
mod equalizer;
mod output;
mod resample;
mod shared;
#[cfg(target_os = "macos")]
mod spatial;
pub use decoder::{AudioInfo, FileDecoder};
pub use engine::{AudioEngine, AudioSnapshot};
pub use shared::OutputStatistics;

#[cfg(test)]
mod tests;

#[cfg(target_os = "macos")]
mod owned_output;
#[cfg(target_os = "macos")]
pub use owned_output::OwnedOutputAnalysis;
