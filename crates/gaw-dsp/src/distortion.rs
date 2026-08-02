//! Waveshaping and digital-degradation processors.

use serde::{Deserialize, Serialize};

use crate::contract::{
    AudioLayout, MONO_AND_STEREO, PrepareSpec, ProcessContext, ProcessError, Processor,
    copy_or_map_bypass, validate_process_io,
};
use crate::parameter::{
    ParameterDescriptor, ParameterEvent, ParameterKind, ParameterUnit, ParameterValue,
};

#[inline]
fn db_to_gain(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaturationCurve {
    SoftClip,
    #[default]
    Tanh,
    Asymmetric,
    Fold,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterpolationQuality {
    #[default]
    Off,
    #[serde(rename = "linear_2x")]
    Linear2x,
    #[serde(rename = "linear_4x")]
    Linear4x,
}

impl InterpolationQuality {
    #[inline]
    fn factor(self) -> usize {
        match self {
            Self::Off => 1,
            Self::Linear2x => 2,
            Self::Linear4x => 4,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SaturatorConfig {
    pub curve: SaturationCurve,
    pub drive_db: f32,
    pub bias: f32,
    pub tone_hz: f32,
    pub output_gain_db: f32,
    pub mix: f32,
    /// Linear sub-sample interpolation quality. This is not band-limited oversampling.
    pub interpolation_quality: InterpolationQuality,
}

impl Default for SaturatorConfig {
    fn default() -> Self {
        Self {
            curve: SaturationCurve::Tanh,
            drive_db: 6.0,
            bias: 0.0,
            tone_hz: 18_000.0,
            output_gain_db: 0.0,
            mix: 1.0,
            interpolation_quality: InterpolationQuality::Off,
        }
    }
}

#[derive(Debug)]
pub struct Saturator {
    pub config: SaturatorConfig,
    enabled: bool,
    sample_rate: f32,
    channels: usize,
    maximum_block_size: usize,
    previous: [f32; 2],
    tone_state: [f32; 2],
}

impl Saturator {
    pub fn new(config: SaturatorConfig) -> Self {
        Self {
            config,
            enabled: true,
            sample_rate: 48_000.0,
            channels: 0,
            maximum_block_size: 0,
            previous: [0.0; 2],
            tone_state: [0.0; 2],
        }
    }
    pub(crate) fn prepare_inner(&mut self, sample_rate: f32, channels: usize) {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.reset_inner();
    }
    pub(crate) fn reset_inner(&mut self) {
        self.previous = [0.0; 2];
        self.tone_state = [0.0; 2];
    }

    #[inline]
    pub(crate) fn process_frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        let factor = self.config.interpolation_quality.factor();
        let drive = db_to_gain(self.config.drive_db.clamp(-24.0, 48.0));
        let output_gain = db_to_gain(self.config.output_gain_db.clamp(-36.0, 24.0));
        let mix = self.config.mix.clamp(0.0, 1.0);
        let bias = self.config.bias.clamp(-1.0, 1.0);
        let cutoff = self.config.tone_hz.clamp(20.0, self.sample_rate * 0.49);
        let tone_coefficient =
            (-std::f32::consts::TAU * cutoff / (self.sample_rate * factor as f32)).exp();
        for ch in 0..self.channels {
            let mut accumulated = 0.0;
            for phase in 1..=factor {
                let t = phase as f32 / factor as f32;
                let interpolated = self.previous[ch] + (input[ch] - self.previous[ch]) * t;
                let driven = (interpolated * drive + bias).clamp(-1.0e6, 1.0e6);
                let shaped =
                    waveshape(driven, self.config.curve) - waveshape(bias, self.config.curve);
                self.tone_state[ch] =
                    tone_coefficient * self.tone_state[ch] + (1.0 - tone_coefficient) * shaped;
                accumulated += self.tone_state[ch];
            }
            self.previous[ch] = input[ch];
            let wet = accumulated / factor as f32 * output_gain;
            output[ch] = input[ch] * (1.0 - mix) + wet * mix;
        }
    }
}

impl Default for Saturator {
    fn default() -> Self {
        Self::new(SaturatorConfig::default())
    }
}

#[inline]
fn waveshape(x: f32, curve: SaturationCurve) -> f32 {
    match curve {
        SaturationCurve::SoftClip => x / (1.0 + x.abs()),
        SaturationCurve::Tanh => x.tanh(),
        SaturationCurve::Asymmetric => {
            if x >= 0.0 {
                x.tanh()
            } else {
                0.7 * (x / 0.7).tanh()
            }
        }
        SaturationCurve::Fold => {
            let wrapped = (x + 1.0).rem_euclid(4.0);
            if wrapped <= 2.0 {
                wrapped - 1.0
            } else {
                3.0 - wrapped
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipperConfig {
    pub threshold_db: f32,
    pub softness: f32,
    pub output_ceiling_db: f32,
    /// Linear sub-sample interpolation quality. This is not band-limited oversampling.
    pub interpolation_quality: InterpolationQuality,
}

impl Default for ClipperConfig {
    fn default() -> Self {
        Self {
            threshold_db: -3.0,
            softness: 0.0,
            output_ceiling_db: -0.1,
            interpolation_quality: InterpolationQuality::Off,
        }
    }
}

#[derive(Debug)]
pub struct Clipper {
    pub config: ClipperConfig,
    enabled: bool,
    channels: usize,
    maximum_block_size: usize,
    previous: [f32; 2],
}

impl Clipper {
    pub fn new(config: ClipperConfig) -> Self {
        Self {
            config,
            enabled: true,
            channels: 0,
            maximum_block_size: 0,
            previous: [0.0; 2],
        }
    }
    pub(crate) fn prepare_inner(&mut self, channels: usize) {
        self.channels = channels;
        self.reset_inner();
    }
    pub(crate) fn reset_inner(&mut self) {
        self.previous = [0.0; 2];
    }

    #[inline]
    pub(crate) fn process_frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        let factor = self.config.interpolation_quality.factor();
        let threshold = db_to_gain(self.config.threshold_db.clamp(-36.0, 0.0));
        let ceiling = db_to_gain(self.config.output_ceiling_db.clamp(-36.0, 0.0));
        let softness = self.config.softness.clamp(0.0, 1.0);
        for ch in 0..self.channels {
            let mut accumulated = 0.0;
            for phase in 1..=factor {
                let t = phase as f32 / factor as f32;
                let x = self.previous[ch] + (input[ch] - self.previous[ch]) * t;
                let magnitude = x.abs();
                let shaped = if softness <= 1.0e-6 {
                    x.clamp(-threshold, threshold)
                } else {
                    let knee_start = threshold * (1.0 - softness);
                    if magnitude <= knee_start {
                        x
                    } else {
                        let knee = (threshold - knee_start).max(1.0e-6);
                        let normalized = (magnitude - knee_start) / knee;
                        x.signum() * (knee_start + knee * normalized.tanh())
                    }
                };
                accumulated += shaped;
            }
            self.previous[ch] = input[ch];
            output[ch] =
                (accumulated / factor as f32 * (ceiling / threshold)).clamp(-ceiling, ceiling);
        }
    }
}

impl Default for Clipper {
    fn default() -> Self {
        Self::new(ClipperConfig::default())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BitcrusherConfig {
    pub bit_depth: u8,
    pub sample_rate_ratio: f32,
    pub dither: bool,
    pub jitter: f32,
    pub mix: f32,
    pub seed: u64,
}

impl Default for BitcrusherConfig {
    fn default() -> Self {
        Self {
            bit_depth: 8,
            sample_rate_ratio: 0.5,
            dither: false,
            jitter: 0.0,
            mix: 1.0,
            seed: 0,
        }
    }
}

#[derive(Debug)]
pub struct Bitcrusher {
    pub config: BitcrusherConfig,
    enabled: bool,
    channels: usize,
    maximum_block_size: usize,
    phase: f32,
    held: [f32; 2],
    frame: u64,
}

impl Bitcrusher {
    pub fn new(config: BitcrusherConfig) -> Self {
        Self {
            config,
            enabled: true,
            channels: 0,
            maximum_block_size: 0,
            phase: 1.0,
            held: [0.0; 2],
            frame: 0,
        }
    }
    pub(crate) fn prepare_inner(&mut self, channels: usize) {
        self.channels = channels;
        self.reset_inner(0);
    }
    pub(crate) fn reset_inner(&mut self, absolute_frame: u64) {
        self.phase = 1.0;
        self.held = [0.0; 2];
        self.frame = absolute_frame;
    }

    #[inline]
    pub(crate) fn process_frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        let ratio = self.config.sample_rate_ratio.clamp(0.001, 1.0);
        let jitter = self.config.jitter.clamp(0.0, 1.0);
        let random = noise(self.config.seed, self.frame, 7);
        self.phase += ratio * (1.0 + jitter * random * 0.9);
        if self.phase >= 1.0 {
            self.phase -= 1.0;
            let levels = 2.0_f32.powi(i32::from(self.config.bit_depth.clamp(1, 24)) - 1);
            let dither = if self.config.dither {
                1.0 / levels
            } else {
                0.0
            };
            for (ch, sample) in input[..self.channels].iter().enumerate() {
                let triangular = noise(self.config.seed, self.frame, ch as u64 * 2)
                    - noise(self.config.seed, self.frame, ch as u64 * 2 + 1);
                self.held[ch] = ((*sample + triangular * dither) * levels).round() / levels;
            }
        }
        let mix = self.config.mix.clamp(0.0, 1.0);
        for ch in 0..self.channels {
            output[ch] = input[ch] * (1.0 - mix) + self.held[ch] * mix;
        }
        self.frame = self.frame.wrapping_add(1);
    }
}

impl Default for Bitcrusher {
    fn default() -> Self {
        Self::new(BitcrusherConfig::default())
    }
}

#[inline]
fn noise(seed: u64, frame: u64, stream: u64) -> f32 {
    let mut x = seed
        ^ frame.wrapping_mul(0x9e37_79b9_7f4a_7c15)
        ^ stream.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^= x >> 31;
    let upper = u16::try_from(x >> 48).unwrap_or(0);
    f32::from(upper) / f32::from(u16::MAX) * 2.0 - 1.0
}

trait DistortionUnit {
    const TYPE_ID: &'static str;
    fn channels(&self) -> usize;
    fn maximum_block_size(&self) -> usize;
    fn prepare_unit(&mut self, spec: PrepareSpec);
    fn frame(&mut self, input: [f32; 2], output: &mut [f32; 2]);
    fn reset_unit(&mut self, absolute_frame: u64);
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError>;
    fn descriptors() -> &'static [ParameterDescriptor];
    fn enabled_ref(&self) -> bool;
    fn set_enabled_ref(&mut self, enabled: bool);
}

macro_rules! impl_processor {
    ($processor:ty) => {
        impl Processor for $processor {
            fn type_id(&self) -> &'static str {
                <Self as DistortionUnit>::TYPE_ID
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
                context: ProcessContext,
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
                let _ = context;
                Ok(())
            }
            fn reset(&mut self) {
                self.reset_unit(0);
            }
            fn seek(&mut self, absolute_frame: u64) {
                self.reset_unit(absolute_frame);
            }
            fn latency_frames(&self) -> u32 {
                0
            }
            fn tail_frames(&self) -> u64 {
                0
            }
            fn parameters(&self) -> &'static [ParameterDescriptor] {
                <Self as DistortionUnit>::descriptors()
            }
            fn enabled(&self) -> bool {
                self.enabled_ref()
            }
            fn set_enabled(&mut self, enabled: bool) {
                self.set_enabled_ref(enabled);
            }
        }
    };
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

const INTERPOLATION_QUALITY: &[&str] = &["off", "linear_2x", "linear_4x"];

static SATURATOR_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
        id: "curve",
        name: "Curve",
        kind: ParameterKind::Choice(&["soft_clip", "tanh", "asymmetric", "fold"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(1),
        automatable: false,
        display_hint: None,
    },
    float_parameter(
        "drive_db",
        "Drive",
        -24.0,
        48.0,
        6.0,
        ParameterUnit::Decibels,
    ),
    float_parameter("bias", "Bias", -1.0, 1.0, 0.0, ParameterUnit::Ratio),
    float_parameter(
        "tone_hz",
        "Tone",
        20.0,
        24_000.0,
        18_000.0,
        ParameterUnit::Hertz,
    ),
    float_parameter(
        "output_gain_db",
        "Output Gain",
        -36.0,
        24.0,
        0.0,
        ParameterUnit::Decibels,
    ),
    float_parameter("mix", "Mix", 0.0, 1.0, 1.0, ParameterUnit::Ratio),
    ParameterDescriptor {
        id: "interpolation_quality",
        name: "Interpolation Quality",
        kind: ParameterKind::Choice(INTERPOLATION_QUALITY),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: None,
    },
];

impl DistortionUnit for Saturator {
    const TYPE_ID: &'static str = "gaw.saturator";
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
    fn reset_unit(&mut self, _: u64) {
        self.reset_inner();
    }
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "drive_db" => self.config.drive_db = float_value(event, -24.0, 48.0)?,
            "bias" => self.config.bias = float_value(event, -1.0, 1.0)?,
            "tone_hz" => self.config.tone_hz = float_value(event, 20.0, 24_000.0)?,
            "output_gain_db" => self.config.output_gain_db = float_value(event, -36.0, 24.0)?,
            "mix" => self.config.mix = float_value(event, 0.0, 1.0)?,
            "curve" | "interpolation_quality" => {
                return Err(ProcessError::InvalidParameterValue);
            }
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
    fn descriptors() -> &'static [ParameterDescriptor] {
        SATURATOR_PARAMETERS
    }
    fn enabled_ref(&self) -> bool {
        self.enabled
    }
    fn set_enabled_ref(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl_processor!(Saturator);

static CLIPPER_PARAMETERS: &[ParameterDescriptor] = &[
    float_parameter(
        "threshold_db",
        "Threshold",
        -36.0,
        0.0,
        -3.0,
        ParameterUnit::Decibels,
    ),
    float_parameter("softness", "Softness", 0.0, 1.0, 0.0, ParameterUnit::Ratio),
    float_parameter(
        "output_ceiling_db",
        "Output Ceiling",
        -36.0,
        0.0,
        -0.1,
        ParameterUnit::Decibels,
    ),
    ParameterDescriptor {
        id: "interpolation_quality",
        name: "Interpolation Quality",
        kind: ParameterKind::Choice(INTERPOLATION_QUALITY),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: None,
    },
];

impl DistortionUnit for Clipper {
    const TYPE_ID: &'static str = "gaw.clipper";
    fn channels(&self) -> usize {
        self.channels
    }
    fn maximum_block_size(&self) -> usize {
        self.maximum_block_size
    }
    fn prepare_unit(&mut self, spec: PrepareSpec) {
        self.maximum_block_size = spec.max_block_size;
        self.prepare_inner(spec.input_layout.channels());
    }
    fn frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        self.process_frame(input, output);
    }
    fn reset_unit(&mut self, _: u64) {
        self.reset_inner();
    }
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "threshold_db" => self.config.threshold_db = float_value(event, -36.0, 0.0)?,
            "softness" => self.config.softness = float_value(event, 0.0, 1.0)?,
            "output_ceiling_db" => self.config.output_ceiling_db = float_value(event, -36.0, 0.0)?,
            "interpolation_quality" => {
                return Err(ProcessError::InvalidParameterValue);
            }
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
    fn descriptors() -> &'static [ParameterDescriptor] {
        CLIPPER_PARAMETERS
    }
    fn enabled_ref(&self) -> bool {
        self.enabled
    }
    fn set_enabled_ref(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl_processor!(Clipper);

static BITCRUSHER_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
        id: "bit_depth",
        name: "Bit Depth",
        kind: ParameterKind::Integer { min: 1, max: 24 },
        unit: ParameterUnit::None,
        default: ParameterValue::Integer(8),
        automatable: true,
        display_hint: None,
    },
    float_parameter(
        "sample_rate_ratio",
        "Sample Rate Ratio",
        0.001,
        1.0,
        0.5,
        ParameterUnit::Ratio,
    ),
    ParameterDescriptor {
        id: "dither",
        name: "Dither",
        kind: ParameterKind::Boolean,
        unit: ParameterUnit::None,
        default: ParameterValue::Bool(false),
        automatable: true,
        display_hint: None,
    },
    float_parameter("jitter", "Jitter", 0.0, 1.0, 0.0, ParameterUnit::Ratio),
    float_parameter("mix", "Mix", 0.0, 1.0, 1.0, ParameterUnit::Ratio),
    ParameterDescriptor {
        id: "seed",
        name: "Seed",
        kind: ParameterKind::UnsignedInteger {
            min: u64::MIN,
            max: u64::MAX,
        },
        unit: ParameterUnit::None,
        default: ParameterValue::UnsignedInteger(0),
        automatable: false,
        display_hint: None,
    },
];

impl DistortionUnit for Bitcrusher {
    const TYPE_ID: &'static str = "gaw.bitcrusher";
    fn channels(&self) -> usize {
        self.channels
    }
    fn maximum_block_size(&self) -> usize {
        self.maximum_block_size
    }
    fn prepare_unit(&mut self, spec: PrepareSpec) {
        self.maximum_block_size = spec.max_block_size;
        self.prepare_inner(spec.input_layout.channels());
    }
    fn frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        self.process_frame(input, output);
    }
    fn reset_unit(&mut self, absolute_frame: u64) {
        self.reset_inner(absolute_frame);
    }
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "bit_depth" => {
                self.config.bit_depth = match event.value {
                    ParameterValue::Integer(value @ 1..=24) => {
                        u8::try_from(value).map_err(|_| ProcessError::InvalidParameterValue)?
                    }
                    _ => return Err(ProcessError::InvalidParameterValue),
                }
            }
            "sample_rate_ratio" => self.config.sample_rate_ratio = float_value(event, 0.001, 1.0)?,
            "dither" => {
                self.config.dither = match event.value {
                    ParameterValue::Bool(value) => value,
                    _ => return Err(ProcessError::InvalidParameterValue),
                }
            }
            "jitter" => self.config.jitter = float_value(event, 0.0, 1.0)?,
            "mix" => self.config.mix = float_value(event, 0.0, 1.0)?,
            "seed" => return Err(ProcessError::InvalidParameterValue),
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
    fn descriptors() -> &'static [ParameterDescriptor] {
        BITCRUSHER_PARAMETERS
    }
    fn enabled_ref(&self) -> bool {
        self.enabled
    }
    fn set_enabled_ref(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl_processor!(Bitcrusher);

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> PrepareSpec {
        PrepareSpec {
            sample_rate: 48_000.0,
            max_block_size: 256,
            input_layout: AudioLayout::Stereo,
            tempo_bpm: 120.0,
        }
    }

    fn render(processor: &mut dyn Processor, input: &[f32]) -> Vec<f32> {
        let mut left = vec![0.0; input.len()];
        let mut right = vec![0.0; input.len()];
        processor
            .process(
                &[input, input],
                &mut [&mut left, &mut right],
                &[],
                ProcessContext {
                    absolute_frame: 0,
                    tempo_bpm: 120.0,
                },
            )
            .unwrap();
        left
    }

    #[test]
    fn clipper_obeys_its_output_ceiling() {
        let mut clipper = Clipper::default();
        clipper.prepare(spec()).unwrap();
        let output = render(&mut clipper, &vec![10.0; 256]);
        let ceiling = db_to_gain(clipper.config.output_ceiling_db);
        assert!(
            output
                .iter()
                .all(|x| x.is_finite() && x.abs() <= ceiling + 1.0e-6)
        );
    }

    #[test]
    fn saturator_is_finite_for_extreme_input() {
        let mut saturator = Saturator::default();
        saturator.prepare(spec()).unwrap();
        let output = render(&mut saturator, &vec![f32::MAX; 256]);
        assert!(output.iter().all(|x| x.is_finite()));
    }

    #[test]
    fn bitcrusher_seek_is_deterministic() {
        let mut crusher = Bitcrusher::new(BitcrusherConfig {
            dither: true,
            jitter: 1.0,
            ..BitcrusherConfig::default()
        });
        crusher.prepare(spec()).unwrap();
        crusher.seek(41);
        let first = render(&mut crusher, &vec![0.123; 256]);
        crusher.seek(41);
        let second = render(&mut crusher, &vec![0.123; 256]);
        assert_eq!(first, second);
    }

    #[test]
    fn bitcrusher_quantizes() {
        let mut crusher = Bitcrusher::new(BitcrusherConfig {
            bit_depth: 2,
            sample_rate_ratio: 1.0,
            ..BitcrusherConfig::default()
        });
        crusher.prepare(spec()).unwrap();
        let output = render(&mut crusher, &vec![0.3; 256]);
        assert!(output.iter().all(|sample| (*sample * 2.0).fract() == 0.0));
    }
}
