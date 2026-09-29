//! Narrowband P25 channel selection and differential demodulation.
//!
//! Both C4FM and CQPSK carry dibits as phase/frequency transitions. This chain
//! first reduces 240 kS/s complex IQ to the 48 kS/s sample stream required by
//! `p25`, then emits normalized discriminator samples. A fractionally-spaced
//! equalizer can be inserted before the discriminator for difficult simulcast
//! paths without changing the protocol decoder.

use std::f32::consts::TAU;

use num_complex::Complex32;
use p25_filts::{BandpassFir, DecimFir};
use static_fir::FirFilter;

pub const SDR_SAMPLE_RATE_HZ: u32 = 240_000;
pub const BASEBAND_SAMPLE_RATE_HZ: u32 = 48_000;
pub const DECIMATION: usize = (SDR_SAMPLE_RATE_HZ / BASEBAND_SAMPLE_RATE_HZ) as usize;
const P25_PEAK_DEVIATION_HZ: f32 = 5_000.0;
const SMOOTHING_SAMPLES: usize = 10;

#[derive(Debug, Clone, Copy, Default)]
pub struct DspMetrics {
    pub input_samples: usize,
    pub output_samples: usize,
    pub power_dbfs: f32,
}

pub struct P25Channel {
    decimator: FirDecimator,
    channel_filter: FirFilter<BandpassFir>,
    discriminator: PhaseDiscriminator,
    smoother: MovingAverage<SMOOTHING_SAMPLES>,
}

impl P25Channel {
    pub fn new() -> Self {
        Self {
            decimator: FirDecimator::new(),
            channel_filter: FirFilter::new(),
            discriminator: PhaseDiscriminator::new(
                BASEBAND_SAMPLE_RATE_HZ as f32,
                P25_PEAK_DEVIATION_HZ,
            ),
            smoother: MovingAverage::new(),
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Convert interleaved unsigned CU8 IQ into 48 kHz P25 discriminator samples.
    pub fn process_cu8(&mut self, bytes: &[u8], output: &mut Vec<f32>) -> DspMetrics {
        output.clear();
        output.reserve(bytes.len() / (2 * DECIMATION));

        let mut power_sum = 0.0_f32;
        let mut power_count = 0_usize;

        for pair in bytes.chunks_exact(2) {
            let sample = cu8_pair(pair[0], pair[1]);
            let Some(decimated) = self.decimator.feed(sample) else {
                continue;
            };

            let selected = self.channel_filter.feed(decimated);
            power_sum += selected.norm_sqr();
            power_count += 1;

            if let Some(baseband) = self.discriminator.feed(selected) {
                output.push(self.smoother.feed(baseband));
            }
        }

        DspMetrics {
            input_samples: bytes.len() / 2,
            output_samples: output.len(),
            power_dbfs: power_dbfs(power_sum, power_count),
        }
    }
}

impl Default for P25Channel {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
pub fn cu8_pair(i: u8, q: u8) -> Complex32 {
    const SCALE: f32 = 1.0 / 127.5;
    Complex32::new((i as f32 - 127.5) * SCALE, (q as f32 - 127.5) * SCALE)
}

struct FirDecimator {
    filter: FirFilter<DecimFir>,
    phase: usize,
}

impl FirDecimator {
    fn new() -> Self {
        Self {
            filter: FirFilter::new(),
            phase: 0,
        }
    }

    fn feed(&mut self, sample: Complex32) -> Option<Complex32> {
        let filtered = self.filter.feed(sample);
        self.phase += 1;
        if self.phase == DECIMATION {
            self.phase = 0;
            Some(filtered)
        } else {
            None
        }
    }
}

struct PhaseDiscriminator {
    previous: Option<Complex32>,
    scale: f32,
}

impl PhaseDiscriminator {
    fn new(sample_rate_hz: f32, peak_deviation_hz: f32) -> Self {
        Self {
            previous: None,
            scale: sample_rate_hz / (TAU * peak_deviation_hz),
        }
    }

    fn feed(&mut self, sample: Complex32) -> Option<f32> {
        let previous = self.previous.replace(sample)?;
        let delta = sample * previous.conj();
        Some(delta.im.atan2(delta.re) * self.scale)
    }
}

struct MovingAverage<const N: usize> {
    history: [f32; N],
    sum: f32,
    cursor: usize,
}

impl<const N: usize> MovingAverage<N> {
    fn new() -> Self {
        Self {
            history: [0.0; N],
            sum: 0.0,
            cursor: 0,
        }
    }

    fn feed(&mut self, sample: f32) -> f32 {
        self.sum -= self.history[self.cursor];
        self.history[self.cursor] = sample;
        self.sum += sample;
        self.cursor = (self.cursor + 1) % N;
        self.sum / N as f32
    }
}

fn power_dbfs(sum: f32, count: usize) -> f32 {
    if count == 0 || sum <= f32::EPSILON {
        -120.0
    } else {
        10.0 * (sum / count as f32).log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_unsigned_iq_without_unsafe_code() {
        let zero = cu8_pair(128, 128);
        assert!(zero.re.abs() < 0.004);
        assert!(zero.im.abs() < 0.004);

        let corner = cu8_pair(255, 0);
        assert!((corner.re - 1.0).abs() < 0.001);
        assert!((corner.im + 1.0).abs() < 0.001);
    }

    #[test]
    fn discriminator_recovers_frequency_deviation() {
        let mut discriminator =
            PhaseDiscriminator::new(BASEBAND_SAMPLE_RATE_HZ as f32, P25_PEAK_DEVIATION_HZ);
        let frequency_hz = 1_000.0_f32;
        let step = TAU * frequency_hz / BASEBAND_SAMPLE_RATE_HZ as f32;
        let mut last = None;

        for index in 0..100 {
            let phase = index as f32 * step;
            last = discriminator.feed(Complex32::from_polar(1.0, phase));
        }

        assert!((last.unwrap() - frequency_hz / P25_PEAK_DEVIATION_HZ).abs() < 1e-4);
    }

    #[test]
    fn cu8_chain_has_expected_rate_ratio() {
        let mut chain = P25Channel::new();
        let bytes = vec![128_u8; 20_000];
        let mut output = Vec::new();
        let metrics = chain.process_cu8(&bytes, &mut output);

        assert_eq!(metrics.input_samples, 10_000);
        assert!(metrics.output_samples >= 1_998);
        assert!(metrics.output_samples <= 2_000);
    }
}
