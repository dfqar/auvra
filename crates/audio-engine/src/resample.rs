//! Bounded stereo conversion with anti-aliasing, startup-delay trimming and EOF draining.
use crate::decoder::FileDecoder;
use player_domain::{ErrorCode, PlayerError};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};
const CHUNK: usize = 1024;
pub struct PcmPipeline {
    pub decoder: FileDecoder,
    resampler: Option<SincFixedIn<f32>>,
    output: Vec<Vec<f32>>,
    skip: usize,
    produced: u64,
    expected: u64,
    origin: u64,
    eof: bool,
    flushes: usize,
    rate: u32,
}
fn error(e: impl std::fmt::Display) -> PlayerError {
    PlayerError::new(
        ErrorCode::DecodeError,
        format!("Could not resample audio: {e}"),
    )
}
impl PcmPipeline {
    pub fn new(decoder: FileDecoder, rate: u32) -> Result<Self, PlayerError> {
        let mut pipeline = Self {
            decoder,
            resampler: None,
            output: vec![],
            skip: 0,
            produced: 0,
            expected: 0,
            origin: 0,
            eof: false,
            flushes: 0,
            rate,
        };
        pipeline.reset(0)?;
        Ok(pipeline)
    }
    pub fn reset(&mut self, source_frame: u64) -> Result<(), PlayerError> {
        let input_rate = self.decoder.info.sample_rate;
        let source_frame = source_frame.min(self.decoder.info.total_frames);
        self.expected = ((u128::from(self.decoder.info.total_frames - source_frame)
            * u128::from(self.rate))
        .div_ceil(u128::from(input_rate))) as u64;
        self.origin =
            (u128::from(source_frame) * u128::from(self.rate) / u128::from(input_rate)) as u64;
        self.produced = 0;
        self.eof = false;
        self.flushes = 0;
        if input_rate == self.rate {
            self.resampler = None;
            self.output.clear();
            self.skip = 0;
        } else {
            let parameters = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: 0.95,
                interpolation: SincInterpolationType::Cubic,
                oversampling_factor: 128,
                window: WindowFunction::BlackmanHarris2,
            };
            let resampler = SincFixedIn::<f32>::new(
                f64::from(self.rate) / f64::from(input_rate),
                1.0,
                parameters,
                CHUNK,
                2,
            )
            .map_err(error)?;
            self.skip = resampler.output_delay();
            self.output = resampler.output_buffer_allocate(true);
            self.resampler = Some(resampler);
        }
        Ok(())
    }
    pub fn seek(&mut self, position_ms: u64) -> Result<(), PlayerError> {
        let frame = (u128::from(position_ms) * u128::from(self.decoder.info.sample_rate) / 1000)
            .min(u128::from(self.decoder.info.total_frames)) as u64;
        self.decoder.seek(frame)?;
        self.reset(frame)
    }
    pub fn finished(&self) -> bool {
        self.produced >= self.expected
    }
    /// Each output frame carries its source timeline position in device-rate frames.
    pub fn next_chunk(&mut self) -> Result<Vec<([f32; 2], u64)>, PlayerError> {
        if self.finished() {
            return Ok(vec![]);
        }
        if self.resampler.is_none() {
            let input = self.decoder.read_frames(CHUNK)?;
            if input.is_empty() {
                return Err(error("Unexpected empty PCM block"));
            }
            return Ok(input
                .into_iter()
                .map(|frame| {
                    self.produced += 1;
                    (
                        frame.map(|v| v.clamp(-1.0, 1.0)),
                        self.origin + self.produced,
                    )
                })
                .collect());
        }
        loop {
            let input = if self.eof {
                vec![]
            } else {
                self.decoder.read_frames(CHUNK)?
            };
            if input.len() < CHUNK {
                self.eof = true;
            }
            let planar = [
                input.iter().map(|v| v[0]).collect::<Vec<_>>(),
                input.iter().map(|v| v[1]).collect::<Vec<_>>(),
            ];
            let resampler = self.resampler.as_mut().unwrap();
            let (_, written) = if input.is_empty() {
                self.flushes += 1;
                if self.flushes > 4 {
                    return Err(error("Resampler tail did not finish"));
                }
                resampler
                    .process_partial_into_buffer::<Vec<f32>, Vec<f32>>(None, &mut self.output, None)
                    .map_err(error)?
            } else {
                resampler
                    .process_partial_into_buffer(Some(&planar), &mut self.output, None)
                    .map_err(error)?
            };
            let skip = self.skip.min(written);
            self.skip -= skip;
            let count = (written - skip).min((self.expected - self.produced) as usize);
            let mut result = Vec::with_capacity(count);
            for at in skip..skip + count {
                let sample = [self.output[0][at], self.output[1][at]];
                if sample.iter().any(|v| !v.is_finite()) {
                    return Err(error("Non-finite resampler output"));
                }
                self.produced += 1;
                result.push((
                    sample.map(|v| v.clamp(-1.0, 1.0)),
                    self.origin + self.produced,
                ));
            }
            if !result.is_empty() || self.finished() {
                return Ok(result);
            }
        }
    }
}
