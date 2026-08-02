//! Dynamics processors.

use serde::{Deserialize, Serialize};

use crate::contract::{
    AudioLayout, MONO_AND_STEREO, PrepareSpec, ProcessContext, ProcessError, Processor,
    copy_or_map_bypass, validate_process_io,
};
use crate::kernel::LinearSmoother;
use crate::parameter::{
    ParameterDescriptor, ParameterEvent, ParameterKind, ParameterUnit, ParameterValue,
};
use crate::true_peak::{TRUE_PEAK_GROUP_DELAY, TruePeakDetector};

const SILENCE_DB: f32 = -160.0;

#[inline]
fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

#[inline]
fn gain_to_db(gain: f32) -> f32 {
    20.0 * gain.max(1.0e-8).log10()
}

#[inline]
fn coefficient(milliseconds: f32, sample_rate: f32) -> f32 {
    if milliseconds <= 0.0 {
        0.0
    } else {
        (-1.0 / (milliseconds * 0.001 * sample_rate)).exp()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorMode {
    #[default]
    Peak,
    Rms,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CompressorConfig {
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub knee_db: f32,
    pub detector: DetectorMode,
    pub lookahead_ms: f32,
    pub makeup_gain_db: f32,
    pub mix: f32,
}

impl Default for CompressorConfig {
    fn default() -> Self {
        Self {
            threshold_db: -18.0,
            ratio: 4.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            knee_db: 6.0,
            detector: DetectorMode::Peak,
            lookahead_ms: 0.0,
            makeup_gain_db: 0.0,
            mix: 1.0,
        }
    }
}

/// Feed-forward, stereo-linked compressor.
#[derive(Debug)]
pub struct Compressor {
    pub config: CompressorConfig,
    enabled: bool,
    sample_rate: f32,
    channels: usize,
    maximum_block_size: usize,
    envelope: f32,
    delay: Vec<f32>,
    delay_frames: usize,
    delay_pos: usize,
}

impl Compressor {
    pub fn new(config: CompressorConfig) -> Self {
        Self {
            config,
            enabled: true,
            sample_rate: 48_000.0,
            channels: 0,
            maximum_block_size: 0,
            envelope: 0.0,
            delay: Vec::new(),
            delay_frames: 0,
            delay_pos: 0,
        }
    }

    pub(crate) fn prepare_inner(&mut self, sample_rate: f32, channels: usize) {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.delay_frames =
            (self.config.lookahead_ms.clamp(0.0, 20.0) * 0.001 * sample_rate).round() as usize;
        self.delay.resize((self.delay_frames + 1) * channels, 0.0);
        self.reset_inner();
    }

    pub(crate) fn reset_inner(&mut self) {
        self.envelope = 0.0;
        self.delay_pos = 0;
        self.delay.fill(0.0);
    }

    #[inline]
    pub(crate) fn process_frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        let mut detected = 0.0_f32;
        for sample in &input[..self.channels] {
            detected = match self.config.detector {
                DetectorMode::Peak => detected.max(sample.abs()),
                DetectorMode::Rms => detected.max(*sample * *sample),
            };
        }
        let attack = coefficient(self.config.attack_ms.clamp(0.01, 2_000.0), self.sample_rate);
        let release = coefficient(
            self.config.release_ms.clamp(0.01, 10_000.0),
            self.sample_rate,
        );
        let coeff = if detected > self.envelope {
            attack
        } else {
            release
        };
        self.envelope = coeff * self.envelope + (1.0 - coeff) * detected;
        let level = match self.config.detector {
            DetectorMode::Peak => self.envelope,
            DetectorMode::Rms => self.envelope.sqrt(),
        };
        let gain_db = compressor_gain_db(
            gain_to_db(level),
            self.config.threshold_db,
            self.config.ratio,
            self.config.knee_db,
        ) + self.config.makeup_gain_db.clamp(-36.0, 36.0);
        let wet_gain = db_to_gain(gain_db);
        let mix = self.config.mix.clamp(0.0, 1.0);
        if self.delay_frames == 0 {
            for ch in 0..self.channels {
                output[ch] = input[ch] * ((1.0 - mix) + mix * wet_gain);
            }
            return;
        }
        let ring_frames = self.delay_frames + 1;
        let read = (self.delay_pos + 1) % ring_frames;
        for ch in 0..self.channels {
            let old = self.delay[read * self.channels + ch];
            self.delay[self.delay_pos * self.channels + ch] = input[ch];
            output[ch] = old * ((1.0 - mix) + mix * wet_gain);
        }
        self.delay_pos = read;
    }
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new(CompressorConfig::default())
    }
}

#[inline]
fn compressor_gain_db(level_db: f32, threshold: f32, ratio: f32, knee: f32) -> f32 {
    let ratio = ratio.clamp(1.0, 100.0);
    let knee = knee.clamp(0.0, 36.0);
    let over = level_db - threshold.clamp(SILENCE_DB, 12.0);
    if knee > 0.0 && over > -0.5 * knee && over < 0.5 * knee {
        (1.0 / ratio - 1.0) * (over + 0.5 * knee).powi(2) / (2.0 * knee)
    } else if over >= 0.5 * knee {
        (1.0 / ratio - 1.0) * over
    } else {
        0.0
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LimiterConfig {
    pub ceiling_db: f32,
    pub release_ms: f32,
    pub lookahead_ms: f32,
    pub input_gain_db: f32,
    /// Detect and constrain four-times oversampled peaks per ITU-R BS.1770.
    pub true_peak: bool,
}

impl Default for LimiterConfig {
    fn default() -> Self {
        Self {
            ceiling_db: -0.3,
            release_ms: 80.0,
            lookahead_ms: 3.0,
            input_gain_db: 0.0,
            true_peak: true,
        }
    }
}

#[derive(Debug)]
pub struct Limiter {
    pub config: LimiterConfig,
    enabled: bool,
    sample_rate: f32,
    channels: usize,
    maximum_block_size: usize,
    gain: f32,
    delay: Vec<f32>,
    delay_frames: usize,
    delay_pos: usize,
    true_peak: TruePeakDetector,
    input_gain_smoother: LinearSmoother,
    ceiling_smoother: LinearSmoother,
}

impl Limiter {
    pub fn new(config: LimiterConfig) -> Self {
        Self {
            config,
            enabled: true,
            sample_rate: 48_000.0,
            channels: 0,
            maximum_block_size: 0,
            gain: 1.0,
            delay: Vec::new(),
            delay_frames: 0,
            delay_pos: 0,
            true_peak: TruePeakDetector::default(),
            input_gain_smoother: LinearSmoother::default(),
            ceiling_smoother: LinearSmoother::default(),
        }
    }

    pub(crate) fn prepare_inner(&mut self, sample_rate: f32, channels: usize) {
        self.sample_rate = sample_rate;
        let lookahead =
            (self.config.lookahead_ms.clamp(0.0, 20.0) * 0.001 * sample_rate).round() as usize;
        self.delay_frames = lookahead
            + if self.config.true_peak {
                TRUE_PEAK_GROUP_DELAY
            } else {
                0
            };
        self.channels = channels;
        self.delay.resize((self.delay_frames + 1) * channels, 0.0);
        self.input_gain_smoother = LinearSmoother::new(
            db_to_gain(self.config.input_gain_db.clamp(-24.0, 36.0)),
            f64::from(sample_rate),
            5.0,
        );
        self.ceiling_smoother = LinearSmoother::new(
            db_to_gain(self.config.ceiling_db.clamp(-24.0, 0.0)),
            f64::from(sample_rate),
            5.0,
        );
        self.reset_inner();
    }

    pub(crate) fn reset_inner(&mut self) {
        self.gain = 1.0;
        self.delay_pos = 0;
        self.delay.fill(0.0);
        self.true_peak.reset();
        self.input_gain_smoother
            .jump_to(db_to_gain(self.config.input_gain_db.clamp(-24.0, 36.0)));
        self.ceiling_smoother
            .jump_to(db_to_gain(self.config.ceiling_db.clamp(-24.0, 0.0)));
    }

    #[inline]
    pub(crate) fn process_frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        let input_gain = self.input_gain_smoother.next();
        let ceiling = self.ceiling_smoother.next();
        let gained = [input[0] * input_gain, input[1] * input_gain];
        let peak = if self.config.true_peak {
            self.true_peak.process(gained, self.channels)
        } else {
            gained[..self.channels]
                .iter()
                .fold(0.0_f32, |peak, sample| peak.max(sample.abs()))
        };
        // BS.1770 permits true-peak meter under-read tolerance. This small
        // headroom also covers intersample peaks created by the base-rate gain
        // envelope itself.
        let detector_ceiling = if self.config.true_peak {
            ceiling * db_to_gain(-0.5)
        } else {
            ceiling
        };
        let target = if peak > detector_ceiling {
            detector_ceiling / peak
        } else {
            1.0
        };
        if target < self.gain {
            self.gain = target;
        } else {
            let release = coefficient(self.config.release_ms.clamp(1.0, 5_000.0), self.sample_rate);
            self.gain = release * self.gain + (1.0 - release);
        }
        if self.delay_frames == 0 {
            for ch in 0..self.channels {
                output[ch] = (gained[ch] * self.gain).clamp(-ceiling, ceiling);
            }
            return;
        }
        let ring_frames = self.delay_frames + 1;
        let read = (self.delay_pos + 1) % ring_frames;
        for ch in 0..self.channels {
            let old = self.delay[read * self.channels + ch];
            self.delay[self.delay_pos * self.channels + ch] = gained[ch];
            // The final clamp is intentional: it is the sample-peak safety net after
            // the predictive gain computer (including for zero lookahead).
            output[ch] = (old * self.gain).clamp(-ceiling, ceiling);
        }
        self.delay_pos = read;
    }
}

impl Default for Limiter {
    fn default() -> Self {
        Self::new(LimiterConfig::default())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GateConfig {
    pub threshold_db: f32,
    pub hysteresis_db: f32,
    pub attack_ms: f32,
    pub hold_ms: f32,
    pub release_ms: f32,
    pub range_db: f32,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            threshold_db: -40.0,
            hysteresis_db: 3.0,
            attack_ms: 2.0,
            hold_ms: 20.0,
            release_ms: 100.0,
            range_db: 80.0,
        }
    }
}

#[derive(Debug)]
pub struct Gate {
    pub config: GateConfig,
    enabled: bool,
    sample_rate: f32,
    channels: usize,
    maximum_block_size: usize,
    gain: f32,
    open: bool,
    hold_remaining: usize,
}

impl Gate {
    pub fn new(config: GateConfig) -> Self {
        Self {
            config,
            enabled: true,
            sample_rate: 48_000.0,
            channels: 0,
            maximum_block_size: 0,
            gain: 0.0,
            open: false,
            hold_remaining: 0,
        }
    }

    pub(crate) fn prepare_inner(&mut self, sample_rate: f32, channels: usize) {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.reset_inner();
    }

    pub(crate) fn reset_inner(&mut self) {
        self.gain = db_to_gain(-self.config.range_db.clamp(0.0, 120.0));
        self.open = false;
        self.hold_remaining = 0;
    }

    #[inline]
    pub(crate) fn process_frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        let peak = input[..self.channels]
            .iter()
            .fold(0.0_f32, |v, x| v.max(x.abs()));
        let level_db = gain_to_db(peak);
        if !self.open && level_db >= self.config.threshold_db {
            self.open = true;
        }
        if self.open {
            if level_db >= self.config.threshold_db - self.config.hysteresis_db.clamp(0.0, 24.0) {
                self.hold_remaining =
                    (self.config.hold_ms.clamp(0.0, 2_000.0) * 0.001 * self.sample_rate) as usize;
            } else if self.hold_remaining > 0 {
                self.hold_remaining -= 1;
            } else {
                self.open = false;
            }
        }
        let target = if self.open {
            1.0
        } else {
            db_to_gain(-self.config.range_db.clamp(0.0, 120.0))
        };
        let time = if target > self.gain {
            self.config.attack_ms
        } else {
            self.config.release_ms
        };
        let coeff = coefficient(time.clamp(0.01, 10_000.0), self.sample_rate);
        self.gain = coeff * self.gain + (1.0 - coeff) * target;
        for ch in 0..self.channels {
            output[ch] = input[ch] * self.gain;
        }
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::new(GateConfig::default())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ExpanderConfig {
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub knee_db: f32,
    pub range_db: f32,
}

impl Default for ExpanderConfig {
    fn default() -> Self {
        Self {
            threshold_db: -35.0,
            ratio: 2.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            knee_db: 6.0,
            range_db: 60.0,
        }
    }
}

#[derive(Debug)]
pub struct Expander {
    pub config: ExpanderConfig,
    enabled: bool,
    sample_rate: f32,
    channels: usize,
    maximum_block_size: usize,
    envelope: f32,
    gain: f32,
}

impl Expander {
    pub fn new(config: ExpanderConfig) -> Self {
        Self {
            config,
            enabled: true,
            sample_rate: 48_000.0,
            channels: 0,
            maximum_block_size: 0,
            envelope: 0.0,
            gain: 1.0,
        }
    }
    pub(crate) fn prepare_inner(&mut self, sample_rate: f32, channels: usize) {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.reset_inner();
    }
    pub(crate) fn reset_inner(&mut self) {
        self.envelope = 0.0;
        self.gain = 1.0;
    }

    #[inline]
    pub(crate) fn process_frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        let peak = input[..self.channels]
            .iter()
            .fold(0.0_f32, |v, x| v.max(x.abs()));
        let env_coeff = coefficient(
            if peak > self.envelope {
                self.config.attack_ms
            } else {
                self.config.release_ms
            }
            .clamp(0.01, 10_000.0),
            self.sample_rate,
        );
        self.envelope = env_coeff * self.envelope + (1.0 - env_coeff) * peak;
        let under = self.config.threshold_db - gain_to_db(self.envelope);
        let knee = self.config.knee_db.clamp(0.0, 36.0);
        let shaped_under = if under <= -0.5 * knee {
            0.0
        } else if knee > 0.0 && under < 0.5 * knee {
            (under + 0.5 * knee).powi(2) / (2.0 * knee)
        } else {
            under
        };
        let target_db = (-(self.config.ratio.clamp(1.0, 20.0) - 1.0) * shaped_under)
            .max(-self.config.range_db.clamp(0.0, 120.0));
        let target = db_to_gain(target_db);
        let gain_coeff = coefficient(
            if target < self.gain {
                self.config.attack_ms
            } else {
                self.config.release_ms
            }
            .clamp(0.01, 10_000.0),
            self.sample_rate,
        );
        self.gain = gain_coeff * self.gain + (1.0 - gain_coeff) * target;
        for ch in 0..self.channels {
            output[ch] = input[ch] * self.gain;
        }
    }
}

impl Default for Expander {
    fn default() -> Self {
        Self::new(ExpanderConfig::default())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TransientShaperConfig {
    pub attack_amount: f32,
    pub sustain_amount: f32,
    pub sensitivity: f32,
    pub response_ms: f32,
    pub output_gain_db: f32,
}

impl Default for TransientShaperConfig {
    fn default() -> Self {
        Self {
            attack_amount: 0.0,
            sustain_amount: 0.0,
            sensitivity: 0.5,
            response_ms: 20.0,
            output_gain_db: 0.0,
        }
    }
}

#[derive(Debug)]
pub struct TransientShaper {
    pub config: TransientShaperConfig,
    enabled: bool,
    sample_rate: f32,
    channels: usize,
    maximum_block_size: usize,
    fast: f32,
    slow: f32,
    delay: [f32; 64],
    delay_pos: usize,
}

impl TransientShaper {
    pub fn new(config: TransientShaperConfig) -> Self {
        Self {
            config,
            enabled: true,
            sample_rate: 48_000.0,
            channels: 0,
            maximum_block_size: 0,
            fast: 0.0,
            slow: 0.0,
            delay: [0.0; 64],
            delay_pos: 0,
        }
    }
    pub(crate) fn prepare_inner(&mut self, sample_rate: f32, channels: usize) {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.reset_inner();
    }
    pub(crate) fn reset_inner(&mut self) {
        self.fast = 0.0;
        self.slow = 0.0;
        self.delay = [0.0; 64];
        self.delay_pos = 0;
    }

    #[inline]
    pub(crate) fn process_frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        let peak = input[..self.channels]
            .iter()
            .fold(0.0_f32, |v, x| v.max(x.abs()));
        let response = self.config.response_ms.clamp(1.0, 200.0);
        let fast_c = coefficient(response * 0.15, self.sample_rate);
        let slow_c = coefficient(response, self.sample_rate);
        self.fast = fast_c * self.fast + (1.0 - fast_c) * peak;
        self.slow = slow_c * self.slow + (1.0 - slow_c) * peak;
        let sensitivity = self.config.sensitivity.clamp(0.0, 1.0);
        let transient =
            ((self.fast - self.slow) / (self.slow + 1.0e-4) - (1.0 - sensitivity)).max(0.0);
        let sustain = (self.slow / (self.fast + 1.0e-4)).min(2.0);
        let shape_db = self.config.attack_amount.clamp(-1.0, 1.0) * transient.min(1.0) * 18.0
            + self.config.sustain_amount.clamp(-1.0, 1.0) * sustain * 9.0
            + self.config.output_gain_db.clamp(-36.0, 36.0);
        let gain = db_to_gain(shape_db.clamp(-48.0, 24.0));
        for ch in 0..self.channels {
            let index = self.delay_pos * 2 + ch;
            output[ch] = self.delay[index];
            self.delay[index] = input[ch] * gain;
        }
        self.delay_pos = (self.delay_pos + 1) % 32;
    }
}

impl Default for TransientShaper {
    fn default() -> Self {
        Self::new(TransientShaperConfig::default())
    }
}

trait DynamicsUnit: Send {
    const TYPE_ID: &'static str;
    fn channels(&self) -> usize;
    fn maximum_block_size(&self) -> usize;
    fn prepare_unit(&mut self, spec: PrepareSpec);
    fn frame(&mut self, input: [f32; 2], output: &mut [f32; 2]);
    fn reset_unit(&mut self);
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError>;
    fn descriptors() -> &'static [ParameterDescriptor];
    fn enabled_ref(&self) -> bool;
    fn set_enabled_ref(&mut self, enabled: bool);
    fn latency(&self) -> u32 {
        0
    }
    fn tail(&self) -> u64 {
        0
    }
}

impl<T: DynamicsUnit> Processor for T {
    fn type_id(&self) -> &'static str {
        T::TYPE_ID
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(input)
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.prepare_unit(spec);
        Ok(())
    }
    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        _context: ProcessContext,
    ) -> Result<(), ProcessError> {
        let channels = self.channels();
        if channels == 0 {
            return Err(ProcessError::NotPrepared);
        }
        let layout = if channels == 1 {
            AudioLayout::Mono
        } else {
            AudioLayout::Stereo
        };
        let frames = validate_process_io(
            input,
            output,
            layout,
            layout,
            self.maximum_block_size(),
            events,
        )?;
        if !self.enabled_ref() {
            for event in events {
                self.apply_event(event)?;
            }
            copy_or_map_bypass(input, output);
            return Ok(());
        }
        let mut event_index = 0;
        for frame in 0..frames {
            while event_index < events.len() && events[event_index].sample_offset == frame {
                self.apply_event(&events[event_index])?;
                event_index += 1;
            }
            let frame_input = [
                input[0][frame],
                if channels == 2 { input[1][frame] } else { 0.0 },
            ];
            let mut frame_output = [0.0; 2];
            self.frame(frame_input, &mut frame_output);
            output[0][frame] = frame_output[0];
            if channels == 2 {
                output[1][frame] = frame_output[1];
            }
        }
        Ok(())
    }
    fn reset(&mut self) {
        self.reset_unit();
    }
    fn seek(&mut self, _absolute_frame: u64) {
        self.reset_unit();
    }
    fn latency_frames(&self) -> u32 {
        self.latency()
    }
    fn tail_frames(&self) -> u64 {
        self.tail()
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        T::descriptors()
    }
    fn enabled(&self) -> bool {
        self.enabled_ref()
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.set_enabled_ref(enabled);
    }
}

fn float_value(event: &ParameterEvent, min: f32, max: f32) -> Result<f32, ProcessError> {
    match event.value {
        ParameterValue::Float(value) if value.is_finite() && (min..=max).contains(&value) => {
            Ok(value)
        }
        _ => Err(ProcessError::InvalidParameterValue),
    }
}

const fn float_parameter(
    id: &'static str,
    name: &'static str,
    min: f32,
    max: f32,
    default: f32,
    unit: ParameterUnit,
) -> ParameterDescriptor {
    ParameterDescriptor {
        id,
        name,
        kind: ParameterKind::Float { min, max },
        unit,
        default: ParameterValue::Float(default),
        automatable: true,
        display_hint: None,
    }
}

const fn prepare_parameter(
    id: &'static str,
    name: &'static str,
    min: f32,
    max: f32,
    default: f32,
    unit: ParameterUnit,
) -> ParameterDescriptor {
    ParameterDescriptor {
        id,
        name,
        kind: ParameterKind::Float { min, max },
        unit,
        default: ParameterValue::Float(default),
        automatable: false,
        display_hint: None,
    }
}

static COMPRESSOR_PARAMETERS: &[ParameterDescriptor] = &[
    float_parameter(
        "threshold_db",
        "Threshold",
        -80.0,
        12.0,
        -18.0,
        ParameterUnit::Decibels,
    ),
    float_parameter("ratio", "Ratio", 1.0, 100.0, 4.0, ParameterUnit::Ratio),
    float_parameter(
        "attack_ms",
        "Attack",
        0.01,
        2_000.0,
        10.0,
        ParameterUnit::Milliseconds,
    ),
    float_parameter(
        "release_ms",
        "Release",
        0.01,
        10_000.0,
        100.0,
        ParameterUnit::Milliseconds,
    ),
    float_parameter("knee_db", "Knee", 0.0, 36.0, 6.0, ParameterUnit::Decibels),
    ParameterDescriptor {
        id: "detector",
        name: "Detector",
        kind: ParameterKind::Choice(&["peak", "rms"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: None,
    },
    prepare_parameter(
        "lookahead_ms",
        "Lookahead",
        0.0,
        20.0,
        0.0,
        ParameterUnit::Milliseconds,
    ),
    float_parameter(
        "makeup_gain_db",
        "Makeup Gain",
        -36.0,
        36.0,
        0.0,
        ParameterUnit::Decibels,
    ),
    float_parameter("mix", "Mix", 0.0, 1.0, 1.0, ParameterUnit::Ratio),
];

impl DynamicsUnit for Compressor {
    const TYPE_ID: &'static str = "gaw.compressor";
    fn channels(&self) -> usize {
        self.channels
    }
    fn maximum_block_size(&self) -> usize {
        self.maximum_block_size
    }
    fn prepare_unit(&mut self, spec: PrepareSpec) {
        self.maximum_block_size = spec.max_block_size;
        self.prepare_inner(spec.sample_rate as f32, spec.input_layout.channels());
    }
    fn frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        self.process_frame(input, output);
    }
    fn reset_unit(&mut self) {
        self.reset_inner();
    }
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "threshold_db" => self.config.threshold_db = float_value(event, -80.0, 12.0)?,
            "ratio" => self.config.ratio = float_value(event, 1.0, 100.0)?,
            "attack_ms" => self.config.attack_ms = float_value(event, 0.01, 2_000.0)?,
            "release_ms" => self.config.release_ms = float_value(event, 0.01, 10_000.0)?,
            "knee_db" => self.config.knee_db = float_value(event, 0.0, 36.0)?,
            "makeup_gain_db" => self.config.makeup_gain_db = float_value(event, -36.0, 36.0)?,
            "mix" => self.config.mix = float_value(event, 0.0, 1.0)?,
            // Detector mode and lookahead topology are prepare-time only.
            "detector" | "lookahead_ms" => return Err(ProcessError::InvalidParameterValue),
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
    fn descriptors() -> &'static [ParameterDescriptor] {
        COMPRESSOR_PARAMETERS
    }
    fn enabled_ref(&self) -> bool {
        self.enabled
    }
    fn set_enabled_ref(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
    fn latency(&self) -> u32 {
        u32::try_from(self.delay_frames).unwrap_or(u32::MAX)
    }
}

static LIMITER_PARAMETERS: &[ParameterDescriptor] = &[
    float_parameter(
        "ceiling_db",
        "Ceiling",
        -24.0,
        0.0,
        -0.3,
        ParameterUnit::Decibels,
    ),
    float_parameter(
        "release_ms",
        "Release",
        1.0,
        5_000.0,
        80.0,
        ParameterUnit::Milliseconds,
    ),
    prepare_parameter(
        "lookahead_ms",
        "Lookahead",
        0.0,
        20.0,
        3.0,
        ParameterUnit::Milliseconds,
    ),
    float_parameter(
        "input_gain_db",
        "Input Gain",
        -24.0,
        36.0,
        0.0,
        ParameterUnit::Decibels,
    ),
    ParameterDescriptor {
        id: "true_peak",
        name: "True Peak",
        kind: ParameterKind::Boolean,
        unit: ParameterUnit::None,
        default: ParameterValue::Bool(true),
        automatable: false,
        display_hint: Some("ITU-R BS.1770 4x; changes latency on prepare"),
    },
];

impl DynamicsUnit for Limiter {
    const TYPE_ID: &'static str = "gaw.limiter";
    fn channels(&self) -> usize {
        self.channels
    }
    fn maximum_block_size(&self) -> usize {
        self.maximum_block_size
    }
    fn prepare_unit(&mut self, spec: PrepareSpec) {
        self.maximum_block_size = spec.max_block_size;
        self.prepare_inner(spec.sample_rate as f32, spec.input_layout.channels());
    }
    fn frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        self.process_frame(input, output);
    }
    fn reset_unit(&mut self) {
        self.reset_inner();
    }
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "ceiling_db" => {
                self.config.ceiling_db = float_value(event, -24.0, 0.0)?;
                self.ceiling_smoother
                    .set_target(db_to_gain(self.config.ceiling_db));
            }
            "release_ms" => self.config.release_ms = float_value(event, 1.0, 5_000.0)?,
            "input_gain_db" => {
                self.config.input_gain_db = float_value(event, -24.0, 36.0)?;
                self.input_gain_smoother
                    .set_target(db_to_gain(self.config.input_gain_db));
            }
            "lookahead_ms" | "true_peak" => {
                return Err(ProcessError::InvalidParameterValue);
            }
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
    fn descriptors() -> &'static [ParameterDescriptor] {
        LIMITER_PARAMETERS
    }
    fn enabled_ref(&self) -> bool {
        self.enabled
    }
    fn set_enabled_ref(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
    fn latency(&self) -> u32 {
        u32::try_from(self.delay_frames).unwrap_or(u32::MAX)
    }
}

static GATE_PARAMETERS: &[ParameterDescriptor] = &[
    float_parameter(
        "threshold_db",
        "Threshold",
        -100.0,
        0.0,
        -40.0,
        ParameterUnit::Decibels,
    ),
    float_parameter(
        "hysteresis_db",
        "Hysteresis",
        0.0,
        24.0,
        3.0,
        ParameterUnit::Decibels,
    ),
    float_parameter(
        "attack_ms",
        "Attack",
        0.01,
        2_000.0,
        2.0,
        ParameterUnit::Milliseconds,
    ),
    float_parameter(
        "hold_ms",
        "Hold",
        0.0,
        2_000.0,
        20.0,
        ParameterUnit::Milliseconds,
    ),
    float_parameter(
        "release_ms",
        "Release",
        0.01,
        10_000.0,
        100.0,
        ParameterUnit::Milliseconds,
    ),
    float_parameter(
        "range_db",
        "Range",
        0.0,
        120.0,
        80.0,
        ParameterUnit::Decibels,
    ),
];

impl DynamicsUnit for Gate {
    const TYPE_ID: &'static str = "gaw.gate";
    fn channels(&self) -> usize {
        self.channels
    }
    fn maximum_block_size(&self) -> usize {
        self.maximum_block_size
    }
    fn prepare_unit(&mut self, spec: PrepareSpec) {
        self.maximum_block_size = spec.max_block_size;
        self.prepare_inner(spec.sample_rate as f32, spec.input_layout.channels());
    }
    fn frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        self.process_frame(input, output);
    }
    fn reset_unit(&mut self) {
        self.reset_inner();
    }
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "threshold_db" => float_value(event, -100.0, 0.0).map(|v| self.config.threshold_db = v),
            "hysteresis_db" => float_value(event, 0.0, 24.0).map(|v| self.config.hysteresis_db = v),
            "attack_ms" => float_value(event, 0.01, 2_000.0).map(|v| self.config.attack_ms = v),
            "hold_ms" => float_value(event, 0.0, 2_000.0).map(|v| self.config.hold_ms = v),
            "release_ms" => float_value(event, 0.01, 10_000.0).map(|v| self.config.release_ms = v),
            "range_db" => float_value(event, 0.0, 120.0).map(|v| self.config.range_db = v),
            _ => Err(ProcessError::UnknownParameter),
        }
    }
    fn descriptors() -> &'static [ParameterDescriptor] {
        GATE_PARAMETERS
    }
    fn enabled_ref(&self) -> bool {
        self.enabled
    }
    fn set_enabled_ref(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

static EXPANDER_PARAMETERS: &[ParameterDescriptor] = &[
    float_parameter(
        "threshold_db",
        "Threshold",
        -100.0,
        0.0,
        -35.0,
        ParameterUnit::Decibels,
    ),
    float_parameter("ratio", "Ratio", 1.0, 20.0, 2.0, ParameterUnit::Ratio),
    float_parameter(
        "attack_ms",
        "Attack",
        0.01,
        2_000.0,
        10.0,
        ParameterUnit::Milliseconds,
    ),
    float_parameter(
        "release_ms",
        "Release",
        0.01,
        10_000.0,
        100.0,
        ParameterUnit::Milliseconds,
    ),
    float_parameter("knee_db", "Knee", 0.0, 36.0, 6.0, ParameterUnit::Decibels),
    float_parameter(
        "range_db",
        "Range",
        0.0,
        120.0,
        60.0,
        ParameterUnit::Decibels,
    ),
];

impl DynamicsUnit for Expander {
    const TYPE_ID: &'static str = "gaw.expander";
    fn channels(&self) -> usize {
        self.channels
    }
    fn maximum_block_size(&self) -> usize {
        self.maximum_block_size
    }
    fn prepare_unit(&mut self, spec: PrepareSpec) {
        self.maximum_block_size = spec.max_block_size;
        self.prepare_inner(spec.sample_rate as f32, spec.input_layout.channels());
    }
    fn frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        self.process_frame(input, output);
    }
    fn reset_unit(&mut self) {
        self.reset_inner();
    }
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "threshold_db" => self.config.threshold_db = float_value(event, -100.0, 0.0)?,
            "ratio" => self.config.ratio = float_value(event, 1.0, 20.0)?,
            "attack_ms" => self.config.attack_ms = float_value(event, 0.01, 2_000.0)?,
            "release_ms" => self.config.release_ms = float_value(event, 0.01, 10_000.0)?,
            "knee_db" => self.config.knee_db = float_value(event, 0.0, 36.0)?,
            "range_db" => self.config.range_db = float_value(event, 0.0, 120.0)?,
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
    fn descriptors() -> &'static [ParameterDescriptor] {
        EXPANDER_PARAMETERS
    }
    fn enabled_ref(&self) -> bool {
        self.enabled
    }
    fn set_enabled_ref(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

static TRANSIENT_PARAMETERS: &[ParameterDescriptor] = &[
    float_parameter(
        "attack_amount",
        "Attack Amount",
        -1.0,
        1.0,
        0.0,
        ParameterUnit::Ratio,
    ),
    float_parameter(
        "sustain_amount",
        "Sustain Amount",
        -1.0,
        1.0,
        0.0,
        ParameterUnit::Ratio,
    ),
    float_parameter(
        "sensitivity",
        "Sensitivity",
        0.0,
        1.0,
        0.5,
        ParameterUnit::Ratio,
    ),
    float_parameter(
        "response_ms",
        "Response",
        1.0,
        200.0,
        20.0,
        ParameterUnit::Milliseconds,
    ),
    float_parameter(
        "output_gain_db",
        "Output Gain",
        -36.0,
        36.0,
        0.0,
        ParameterUnit::Decibels,
    ),
];

impl DynamicsUnit for TransientShaper {
    const TYPE_ID: &'static str = "gaw.transient_shaper";
    fn channels(&self) -> usize {
        self.channels
    }
    fn maximum_block_size(&self) -> usize {
        self.maximum_block_size
    }
    fn prepare_unit(&mut self, spec: PrepareSpec) {
        self.maximum_block_size = spec.max_block_size;
        self.prepare_inner(spec.sample_rate as f32, spec.input_layout.channels());
    }
    fn frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        self.process_frame(input, output);
    }
    fn reset_unit(&mut self) {
        self.reset_inner();
    }
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "attack_amount" => self.config.attack_amount = float_value(event, -1.0, 1.0)?,
            "sustain_amount" => self.config.sustain_amount = float_value(event, -1.0, 1.0)?,
            "sensitivity" => self.config.sensitivity = float_value(event, 0.0, 1.0)?,
            "response_ms" => self.config.response_ms = float_value(event, 1.0, 200.0)?,
            "output_gain_db" => self.config.output_gain_db = float_value(event, -36.0, 36.0)?,
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
    fn descriptors() -> &'static [ParameterDescriptor] {
        TRANSIENT_PARAMETERS
    }
    fn enabled_ref(&self) -> bool {
        self.enabled
    }
    fn set_enabled_ref(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
    fn tail(&self) -> u64 {
        32 + (self.config.response_ms.clamp(1.0, 200.0) * 0.001 * self.sample_rate * 4.0) as u64
    }
    fn latency(&self) -> u32 {
        32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> PrepareSpec {
        PrepareSpec {
            sample_rate: 48_000.0,
            max_block_size: 512,
            input_layout: AudioLayout::Mono,
            tempo_bpm: 120.0,
        }
    }

    fn render(processor: &mut dyn Processor, input: &[f32]) -> Vec<f32> {
        let mut output = vec![0.0; input.len()];
        processor
            .process(
                &[input],
                &mut [&mut output],
                &[],
                ProcessContext {
                    absolute_frame: 0,
                    tempo_bpm: 120.0,
                },
            )
            .unwrap();
        output
    }

    #[test]
    fn limiter_never_exceeds_ceiling() {
        let mut limiter = Limiter::default();
        limiter.prepare(spec()).unwrap();
        let output = render(&mut limiter, &vec![4.0; 512]);
        let ceiling = db_to_gain(limiter.config.ceiling_db);
        assert!(
            output
                .iter()
                .all(|x| x.is_finite() && x.abs() <= ceiling + 1.0e-6)
        );
    }

    #[test]
    fn true_peak_limiter_declares_fir_latency_and_controls_intersample_peaks() {
        use crate::analyzer::{Analyzer, LevelMeter};

        let mut limiter = Limiter {
            config: LimiterConfig {
                ceiling_db: -1.0,
                lookahead_ms: 1.0,
                true_peak: true,
                ..LimiterConfig::default()
            },
            ..Limiter::default()
        };
        limiter.prepare(spec()).unwrap();
        assert_eq!(limiter.latency_frames(), 48 + TRUE_PEAK_GROUP_DELAY as u32);

        let mut input = vec![0.0; 480];
        for (frame, sample) in input.iter_mut().take(300).enumerate() {
            *sample = 1.2 * (core::f32::consts::TAU * 0.24 * (frame as f32 + 0.5)).sin();
        }
        let output = render(&mut limiter, &input);
        let mut meter = LevelMeter::default();
        meter.prepare(48_000.0, output.len(), 1);
        meter.analyze(&[&output]);
        let ceiling = db_to_gain(-1.0);
        assert!(
            meter.measurement().true_peak[0] <= ceiling * 1.01,
            "true peak {} exceeded ceiling {ceiling}",
            meter.measurement().true_peak[0]
        );
    }

    #[test]
    fn limiter_latency_matches_delayed_impulse() {
        let mut limiter = Limiter {
            config: LimiterConfig {
                ceiling_db: 0.0,
                lookahead_ms: 1.0,
                true_peak: true,
                ..LimiterConfig::default()
            },
            ..Limiter::default()
        };
        limiter.prepare(spec()).unwrap();
        let mut input = vec![0.0; 128];
        input[0] = 0.5;
        let output = render(&mut limiter, &input);
        let position = output.iter().position(|sample| sample.abs() > 0.1).unwrap();
        assert_eq!(position, limiter.latency_frames() as usize);
    }

    #[test]
    fn true_peak_limiter_controls_a_wideband_reference_vector() {
        use crate::analyzer::{Analyzer, LevelMeter};

        let mut limiter = Limiter {
            config: LimiterConfig {
                ceiling_db: -1.0,
                true_peak: true,
                ..LimiterConfig::default()
            },
            ..Limiter::default()
        };
        limiter.prepare(spec()).unwrap();
        let mut state = 0x1234_5678_u32;
        let mut input = vec![0.0; 512];
        for sample in input.iter_mut().take(300) {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *sample = ((state >> 8) as f32 / 8_388_607.5 - 1.0) * 1.8;
        }
        let output = render(&mut limiter, &input);
        let mut meter = LevelMeter::default();
        meter.prepare(48_000.0, output.len(), 1);
        meter.analyze(&[&output]);
        let ceiling = db_to_gain(-1.0);
        assert!(
            meter.measurement().true_peak[0] <= ceiling * 1.01,
            "wideband true peak {} exceeded ceiling {ceiling}",
            meter.measurement().true_peak[0]
        );
    }

    #[test]
    fn compressor_reduces_steady_signal_and_reset_is_deterministic() {
        let mut compressor = Compressor::new(CompressorConfig {
            attack_ms: 0.01,
            lookahead_ms: 0.0,
            ..CompressorConfig::default()
        });
        compressor.prepare(spec()).unwrap();
        let input = vec![1.0; 512];
        let first = render(&mut compressor, &input);
        compressor.reset();
        let second = render(&mut compressor, &input);
        assert_eq!(first, second);
        assert!(first[511].abs() < 0.5);
    }

    #[test]
    fn gate_suppresses_silence_without_non_finite_samples() {
        let mut gate = Gate::default();
        gate.prepare(spec()).unwrap();
        let output = render(&mut gate, &vec![1.0e-6; 512]);
        assert!(output.iter().all(|x| x.is_finite()));
        assert!(output[511].abs() < 1.0e-6);
    }

    #[test]
    fn automation_is_sample_accurate() {
        let mut compressor = Compressor::new(CompressorConfig {
            threshold_db: 0.0,
            attack_ms: 0.01,
            ..CompressorConfig::default()
        });
        compressor.prepare(spec()).unwrap();
        let input = vec![1.0; 32];
        let mut output = vec![0.0; 32];
        let events = [ParameterEvent::new(
            16,
            "threshold_db",
            ParameterValue::Float(-40.0),
        )];
        compressor
            .process(
                &[&input],
                &mut [&mut output],
                &events,
                ProcessContext {
                    absolute_frame: 0,
                    tempo_bpm: 120.0,
                },
            )
            .unwrap();
        assert!(output[15] > output[31]);
    }
}
