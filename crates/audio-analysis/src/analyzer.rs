use crate::{AnalysisFrame, AudioFeatures, SPECTRUM_BANDS};
use rustfft::{Fft, FftPlanner, num_complex::Complex};
use std::{f32::consts::TAU, sync::Arc};

/// Stereo power is averaged after two FFTs, so antiphase audio never cancels out.
/// Features are linear RMS amplitudes; display gain/log scaling belongs to the host.
pub struct Analyzer {
    rate: u32,
    hop: usize,
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    window_power: f32,
    pcm: Vec<[f32; 2]>,
    cursor: usize,
    filled: usize,
    since: usize,
    generation: u64,
    last_position: Option<u64>,
    buffers: [Vec<Complex<f32>>; 2],
    scratch: Vec<Complex<f32>>,
    powers: Vec<f32>,
    previous: Vec<f32>,
    previous_ready: bool,
    transient: f32,
}
impl Analyzer {
    pub fn new(rate: u32) -> Option<Self> {
        if !(8000..=384000).contains(&rate) {
            return None;
        }
        let size = (rate as usize / 16).next_power_of_two().clamp(512, 32768);
        let fft = FftPlanner::new().plan_fft_forward(size);
        let scratch = vec![Complex::default(); fft.get_inplace_scratch_len()];
        let window: Vec<_> = (0..size)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / size as f32).cos())
            .collect();
        let window_power = window.iter().map(|v| v * v).sum();
        Some(Self {
            rate,
            hop: rate as usize / 50,
            fft,
            window,
            window_power,
            pcm: vec![[0.0; 2]; size],
            cursor: 0,
            filled: 0,
            since: 0,
            generation: 0,
            last_position: None,
            buffers: [
                vec![Complex::default(); size],
                vec![Complex::default(); size],
            ],
            scratch,
            powers: vec![0.0; size / 2 + 1],
            previous: vec![0.0; size / 2 + 1],
            previous_ready: false,
            transient: 0.0,
        })
    }
    pub fn reset(&mut self) {
        self.cursor = 0;
        self.filled = 0;
        self.since = 0;
        self.last_position = None;
        self.previous_ready = false;
        self.transient = 0.0;
    }
    pub fn window_frames(&self) -> usize {
        self.pcm.len()
    }
    pub fn hop_frames(&self) -> usize {
        self.hop
    }
    /// `position` is the exclusive end sample index actually submitted by the output callback.
    pub fn push(
        &mut self,
        samples: [f32; 2],
        position: u64,
        generation: u64,
    ) -> Option<AnalysisFrame> {
        if samples.iter().any(|v| !v.is_finite() || v.abs() > 1.0) || position == 0 {
            self.reset();
            return None;
        }
        if generation != self.generation
            || self
                .last_position
                .is_some_and(|last| last.checked_add(1) != Some(position))
        {
            self.reset();
            self.generation = generation;
        }
        self.last_position = Some(position);
        self.pcm[self.cursor] = samples;
        self.cursor = (self.cursor + 1) % self.pcm.len();
        self.filled = (self.filled + 1).min(self.pcm.len());
        self.since += 1;
        if self.filled < self.pcm.len() || (self.previous_ready && self.since < self.hop) {
            return None;
        }
        self.since = 0;
        Some(self.measure(position))
    }
    fn measure(&mut self, position: u64) -> AnalysisFrame {
        let n = self.pcm.len();
        let mut energy = 0.0_f64;
        for i in 0..n {
            let frame = self.pcm[(self.cursor + i) % n];
            for (channel, sample) in frame.into_iter().enumerate() {
                energy += f64::from(sample) * f64::from(sample);
                self.buffers[channel][i] = Complex::new(sample * self.window[i], 0.0);
            }
        }
        for buffer in &mut self.buffers {
            self.fft.process_with_scratch(buffer, &mut self.scratch);
        }
        let mut bands = [0.0_f32; 6];
        let mut spectrum = [0.0; SPECTRUM_BANDS];
        let upper = (self.rate as f32 * 0.5).min(20000.0);
        let edges = [20.0, 60.0, 200.0, 500.0, 2000.0, 6000.0, 20000.0];
        let mut total = 0.0;
        let mut weighted = 0.0;
        let mut flux = 0.0;
        for bin in 0..self.powers.len() {
            // One-sided Parseval power, compensated for Hann window energy.
            let sided = if bin == 0 || bin == n / 2 { 1.0 } else { 2.0 };
            let power =
                (self.buffers[0][bin].norm_sqr() + self.buffers[1][bin].norm_sqr()) * 0.5 * sided
                    / (n as f32 * self.window_power);
            self.powers[bin] = power;
            let hz = bin as f32 * self.rate as f32 / n as f32;
            if bin == 0 || hz > upper {
                continue;
            }
            total += power;
            weighted += power * hz;
            flux += (power - self.previous[bin]).max(0.0);
            if let Some(band) = (0..6).find(|&i| hz >= edges[i] && hz < edges[i + 1]) {
                bands[band] += power;
            }
            if hz >= 20.0 {
                let band =
                    ((hz / 20.0).ln() / (upper / 20.0).ln() * SPECTRUM_BANDS as f32) as usize;
                spectrum[band.min(SPECTRUM_BANDS - 1)] += power;
            }
        }
        let onset = if self.previous_ready && total > 1e-10 {
            (flux / total).clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.transient =
            onset.max(self.transient * (-(self.hop as f32) / (self.rate as f32 * 0.1)).exp());
        self.previous.copy_from_slice(&self.powers);
        self.previous_ready = true;
        let amplitude = |p: f32| p.max(0.0).sqrt().clamp(0.0, 1.0);
        let bands = bands.map(amplitude);
        AnalysisFrame {
            generation: self.generation,
            sample_rate: self.rate,
            window_frames: n as u32,
            hop_frames: self.hop as u32,
            window_start_frame: position - n as u64,
            window_end_frame: position,
            features: AudioFeatures {
                playback_position_ms: position.saturating_mul(1000) / u64::from(self.rate),
                rms: (energy / (2 * n) as f64).sqrt().clamp(0.0, 1.0) as f32,
                sub_bass: bands[0],
                bass: bands[1],
                low_mids: bands[2],
                mids: bands[3],
                presence: bands[4],
                highs: bands[5],
                transient: self.transient,
                spectral_centroid_hz: if total > 1e-10 { weighted / total } else { 0.0 },
            },
            spectrum: spectrum.map(amplitude),
            dropped_frames: 0,
        }
    }
}
