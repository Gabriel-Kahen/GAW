//! Reusable, allocation-free DSP kernels.

use std::f32::consts::PI;

#[must_use]
pub fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db * 0.05)
}

#[must_use]
pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * gain.log10()
    }
}

#[must_use]
pub fn time_coefficient(time_ms: f32, sample_rate: f64) -> f32 {
    if time_ms <= 0.0 {
        0.0
    } else {
        (-1.0 / (time_ms * 0.001 * sample_rate as f32)).exp()
    }
}

/// A bounded linear parameter ramp.
#[derive(Debug, Clone, Copy)]
pub struct LinearSmoother {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
    sample_rate: f64,
    time_ms: f32,
}

impl LinearSmoother {
    #[must_use]
    pub fn new(value: f32, sample_rate: f64, time_ms: f32) -> Self {
        Self {
            current: value,
            target: value,
            step: 0.0,
            remaining: 0,
            sample_rate,
            time_ms,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
    }

    pub fn set_time_ms(&mut self, time_ms: f32) {
        self.time_ms = time_ms.max(0.0);
    }

    pub fn set_target(&mut self, target: f32) {
        self.target = target;
        let frames = (self.sample_rate * f64::from(self.time_ms) * 0.001).round() as u32;
        if frames == 0 {
            self.jump_to(target);
        } else {
            self.remaining = frames;
            self.step = (target - self.current) / frames as f32;
        }
    }

    pub fn jump_to(&mut self, value: f32) {
        self.current = value;
        self.target = value;
        self.step = 0.0;
        self.remaining = 0;
    }

    pub fn reset(&mut self) {
        self.jump_to(self.target);
    }

    #[must_use]
    pub const fn current(&self) -> f32 {
        self.current
    }

    #[must_use]
    pub const fn target(&self) -> f32 {
        self.target
    }

    #[must_use]
    pub fn next(&mut self) -> f32 {
        if self.remaining != 0 {
            self.current += self.step;
            self.remaining -= 1;
            if self.remaining == 0 {
                self.current = self.target;
            }
        }
        self.current
    }
}

impl Default for LinearSmoother {
    fn default() -> Self {
        Self::new(0.0, 48_000.0, 10.0)
    }
}

/// A one-pole smoother, useful for detectors and modulation.
#[derive(Debug, Clone, Copy)]
pub struct OnePoleSmoother {
    state: f32,
    coefficient: f32,
}

impl OnePoleSmoother {
    #[must_use]
    pub fn new(initial: f32, time_ms: f32, sample_rate: f64) -> Self {
        Self {
            state: initial,
            coefficient: time_coefficient(time_ms, sample_rate),
        }
    }

    pub fn configure(&mut self, time_ms: f32, sample_rate: f64) {
        self.coefficient = time_coefficient(time_ms, sample_rate);
    }

    pub fn reset(&mut self, value: f32) {
        self.state = value;
    }

    #[must_use]
    pub fn process(&mut self, input: f32) -> f32 {
        self.state = input + self.coefficient * (self.state - input);
        self.state
    }

    #[must_use]
    pub const fn state(&self) -> f32 {
        self.state
    }
}

impl Default for OnePoleSmoother {
    fn default() -> Self {
        Self::new(0.0, 10.0, 48_000.0)
    }
}

/// Normalized transposed-direct-form-II biquad coefficients.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BiquadCoefficients {
    pub b0: f32,
    pub b1: f32,
    pub b2: f32,
    pub a1: f32,
    pub a2: f32,
}

impl BiquadCoefficients {
    pub const IDENTITY: Self = Self {
        b0: 1.0,
        b1: 0.0,
        b2: 0.0,
        a1: 0.0,
        a2: 0.0,
    };

    #[must_use]
    pub fn low_pass(sample_rate: f64, frequency_hz: f32, q: f32) -> Self {
        let (cos, alpha) = cookbook(sample_rate, frequency_hz, q);
        normalize(
            (1.0 - cos) * 0.5,
            1.0 - cos,
            (1.0 - cos) * 0.5,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        )
    }

    #[must_use]
    pub fn high_pass(sample_rate: f64, frequency_hz: f32, q: f32) -> Self {
        let (cos, alpha) = cookbook(sample_rate, frequency_hz, q);
        normalize(
            (1.0 + cos) * 0.5,
            -(1.0 + cos),
            (1.0 + cos) * 0.5,
            1.0 + alpha,
            -2.0 * cos,
            1.0 - alpha,
        )
    }

    #[must_use]
    pub fn band_pass(sample_rate: f64, frequency_hz: f32, q: f32) -> Self {
        let (cos, alpha) = cookbook(sample_rate, frequency_hz, q);
        normalize(alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    #[must_use]
    pub fn notch(sample_rate: f64, frequency_hz: f32, q: f32) -> Self {
        let (cos, alpha) = cookbook(sample_rate, frequency_hz, q);
        normalize(1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    #[must_use]
    pub fn peaking(sample_rate: f64, frequency_hz: f32, q: f32, gain_db: f32) -> Self {
        let (cos, alpha) = cookbook(sample_rate, frequency_hz, q);
        let a = 10.0_f32.powf(gain_db / 40.0);
        normalize(
            1.0 + alpha * a,
            -2.0 * cos,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * cos,
            1.0 - alpha / a,
        )
    }

    #[must_use]
    pub fn low_shelf(sample_rate: f64, frequency_hz: f32, slope: f32, gain_db: f32) -> Self {
        shelf(sample_rate, frequency_hz, slope, gain_db, false)
    }

    #[must_use]
    pub fn high_shelf(sample_rate: f64, frequency_hz: f32, slope: f32, gain_db: f32) -> Self {
        shelf(sample_rate, frequency_hz, slope, gain_db, true)
    }
}

impl Default for BiquadCoefficients {
    fn default() -> Self {
        Self::IDENTITY
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct BiquadState {
    z1: f32,
    z2: f32,
}

impl BiquadState {
    #[must_use]
    pub fn process(&mut self, input: f32, coefficients: BiquadCoefficients) -> f32 {
        let output = coefficients.b0.mul_add(input, self.z1);
        self.z1 = coefficients.b1.mul_add(input, self.z2) - coefficients.a1 * output;
        self.z2 = coefficients.b2 * input - coefficients.a2 * output;
        output
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Biquad {
    pub coefficients: BiquadCoefficients,
    pub state: BiquadState,
}

impl Biquad {
    #[must_use]
    pub const fn new(coefficients: BiquadCoefficients) -> Self {
        Self {
            coefficients,
            state: BiquadState { z1: 0.0, z2: 0.0 },
        }
    }

    pub fn set_coefficients(&mut self, coefficients: BiquadCoefficients) {
        self.coefficients = coefficients;
    }

    #[must_use]
    pub fn process(&mut self, input: f32) -> f32 {
        self.state.process(input, self.coefficients)
    }

    pub fn reset(&mut self) {
        self.state.reset();
    }
}

/// A fixed-capacity circular delay line. Allocation occurs only in `new`/`resize`.
#[derive(Debug, Clone)]
pub struct DelayLine {
    buffer: Vec<f32>,
    write: usize,
}

impl DelayLine {
    #[must_use]
    pub fn new(maximum_delay_frames: usize) -> Self {
        Self {
            buffer: vec![0.0; maximum_delay_frames.saturating_add(2)],
            write: 0,
        }
    }

    pub fn resize(&mut self, maximum_delay_frames: usize) {
        self.buffer
            .resize(maximum_delay_frames.saturating_add(2), 0.0);
        self.reset();
    }

    #[must_use]
    pub fn read(&self, delay_frames: f32) -> f32 {
        let maximum = self.buffer.len().saturating_sub(2) as f32;
        let delay = delay_frames.clamp(0.0, maximum);
        let integer = delay.floor() as usize;
        let fraction = delay - integer as f32;
        let newer = (self.write + self.buffer.len() - integer - 1) % self.buffer.len();
        let older = (newer + self.buffer.len() - 1) % self.buffer.len();
        self.buffer[newer] + (self.buffer[older] - self.buffer[newer]) * fraction
    }

    pub fn push(&mut self, sample: f32) {
        self.buffer[self.write] = sample;
        self.write = (self.write + 1) % self.buffer.len();
    }

    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write = 0;
    }
}

fn cookbook(sample_rate: f64, frequency_hz: f32, q: f32) -> (f32, f32) {
    let frequency = frequency_hz.clamp(1.0, sample_rate as f32 * 0.499);
    let omega = 2.0 * PI * frequency / sample_rate as f32;
    (omega.cos(), omega.sin() / (2.0 * q.max(0.01)))
}

fn normalize(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> BiquadCoefficients {
    let inverse = a0.recip();
    BiquadCoefficients {
        b0: b0 * inverse,
        b1: b1 * inverse,
        b2: b2 * inverse,
        a1: a1 * inverse,
        a2: a2 * inverse,
    }
}

fn shelf(
    sample_rate: f64,
    frequency_hz: f32,
    slope: f32,
    gain_db: f32,
    high: bool,
) -> BiquadCoefficients {
    let frequency = frequency_hz.clamp(1.0, sample_rate as f32 * 0.499);
    let omega = 2.0 * PI * frequency / sample_rate as f32;
    let cos = omega.cos();
    let sin = omega.sin();
    let a = 10.0_f32.powf(gain_db / 40.0);
    let slope = slope.clamp(0.1, 2.0);
    let alpha = sin * 0.5 * ((a + a.recip()) * (slope.recip() - 1.0) + 2.0).sqrt();
    let beta = 2.0 * a.sqrt() * alpha;
    if high {
        normalize(
            a * ((a + 1.0) + (a - 1.0) * cos + beta),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
            a * ((a + 1.0) + (a - 1.0) * cos - beta),
            (a + 1.0) - (a - 1.0) * cos + beta,
            2.0 * ((a - 1.0) - (a + 1.0) * cos),
            (a + 1.0) - (a - 1.0) * cos - beta,
        )
    } else {
        normalize(
            a * ((a + 1.0) - (a - 1.0) * cos + beta),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
            a * ((a + 1.0) - (a - 1.0) * cos - beta),
            (a + 1.0) + (a - 1.0) * cos + beta,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos),
            (a + 1.0) + (a - 1.0) * cos - beta,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decibel_conversion_round_trips() {
        for value in [-60.0, -12.0, 0.0, 6.0, 24.0] {
            assert!((gain_to_db(db_to_gain(value)) - value).abs() < 1.0e-4);
        }
    }

    #[test]
    fn linear_smoother_reaches_target_exactly() {
        let mut smoother = LinearSmoother::new(0.0, 1_000.0, 10.0);
        smoother.set_target(1.0);
        for _ in 0..10 {
            let _ = smoother.next();
        }
        assert_eq!(smoother.current(), 1.0);
    }

    #[test]
    fn stable_low_pass_has_finite_impulse_response() {
        let mut filter = Biquad::new(BiquadCoefficients::low_pass(48_000.0, 1_000.0, 0.707));
        for index in 0..48_000 {
            let output = filter.process(if index == 0 { 1.0 } else { 0.0 });
            assert!(output.is_finite());
        }
    }
}
