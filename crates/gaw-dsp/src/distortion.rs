//! Waveshaping and digital-degradation processors.

use serde::{Deserialize, Serialize};

use crate::contract::{
    AudioLayout, MONO_AND_STEREO, PrepareSpec, ProcessContext, ProcessError, Processor,
    copy_or_map_bypass, validate_process_io,
};
use crate::kernel::LinearSmoother;
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
pub enum Oversampling {
    #[default]
    Off,
    #[serde(rename = "x2", alias = "linear_2x")]
    X2,
    #[serde(rename = "x4", alias = "linear_4x")]
    X4,
}

impl Oversampling {
    #[inline]
    fn factor(self) -> usize {
        match self {
            Self::Off => 1,
            Self::X2 => 2,
            Self::X4 => 4,
        }
    }
}

/// Backward-compatible type name for projects compiled against the earlier API.
pub type InterpolationQuality = Oversampling;

const HALFBAND_TAPS: usize = 65;
const HALFBAND_DELAY: u32 = 32;
const MAX_OVERSAMPLING_LATENCY: usize = 48;

// The non-zero side coefficients of a symmetric 65-tap Blackman-windowed
// halfband low-pass. Even taps other than the center are mathematically zero.
const HALFBAND_SIDE: [f32; 16] = [
    -8.937_852e-6,
    8.832_916e-5,
    -2.768_682e-4,
    6.251_808e-4,
    -1.206_746e-3,
    2.119_866e-3,
    -3.490_324e-3,
    5.477_292e-3,
    -8.287_568e-3,
    1.220_891_2e-2,
    -1.768_783_3e-2,
    2.552_079e-2,
    -3.738_347_4e-2,
    5.763_946_5e-2,
    -1.023_875_2e-1,
    3.170_515_3e-1,
];
const HALFBAND_CENTER: f32 = 4.999_958e-1;

#[derive(Clone, Copy, Debug)]
struct HalfbandFir {
    history: [f32; HALFBAND_TAPS],
    write: usize,
}

impl Default for HalfbandFir {
    fn default() -> Self {
        Self {
            history: [0.0; HALFBAND_TAPS],
            write: 0,
        }
    }
}

impl HalfbandFir {
    #[inline]
    fn process(&mut self, input: f32) -> f32 {
        self.history[self.write] = input;
        let mut output = HALFBAND_CENTER * self.delayed(32);
        for (index, coefficient) in HALFBAND_SIDE.iter().enumerate() {
            let delay = index * 2 + 1;
            output += coefficient * (self.delayed(delay) + self.delayed(64 - delay));
        }
        self.write = (self.write + 1) % HALFBAND_TAPS;
        output
    }

    #[inline]
    fn delayed(&self, delay: usize) -> f32 {
        self.history[(self.write + HALFBAND_TAPS - delay) % HALFBAND_TAPS]
    }

    fn reset(&mut self) {
        self.history.fill(0.0);
        self.write = 0;
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct OversamplingChannel {
    up_2x: HalfbandFir,
    down_2x: HalfbandFir,
    up_4x: HalfbandFir,
    down_4x: HalfbandFir,
}

#[derive(Debug)]
struct Oversampler {
    mode: Oversampling,
    channels: [OversamplingChannel; 2],
}

impl Default for Oversampler {
    fn default() -> Self {
        Self {
            mode: Oversampling::Off,
            channels: [OversamplingChannel::default(); 2],
        }
    }
}

impl Oversampler {
    fn prepare(&mut self, mode: Oversampling) {
        self.mode = mode;
        self.reset();
    }

    fn reset(&mut self) {
        for channel in &mut self.channels {
            channel.up_2x.reset();
            channel.down_2x.reset();
            channel.up_4x.reset();
            channel.down_4x.reset();
        }
    }

    #[inline]
    fn upsample(&mut self, channel: usize, input: f32, output: &mut [f32; 4]) -> usize {
        match self.mode {
            Oversampling::Off => {
                output[0] = input;
                1
            }
            Oversampling::X2 => {
                output[0] = self.channels[channel].up_2x.process(input * 2.0);
                output[1] = self.channels[channel].up_2x.process(0.0);
                2
            }
            Oversampling::X4 => {
                let first = self.channels[channel].up_2x.process(input * 2.0);
                let second = self.channels[channel].up_2x.process(0.0);
                output[0] = self.channels[channel].up_4x.process(first * 2.0);
                output[1] = self.channels[channel].up_4x.process(0.0);
                output[2] = self.channels[channel].up_4x.process(second * 2.0);
                output[3] = self.channels[channel].up_4x.process(0.0);
                4
            }
        }
    }

    #[inline]
    fn downsample(&mut self, channel: usize, input: &[f32; 4]) -> f32 {
        match self.mode {
            Oversampling::Off => input[0],
            Oversampling::X2 => {
                let output = self.channels[channel].down_2x.process(input[0]);
                let _ = self.channels[channel].down_2x.process(input[1]);
                output
            }
            Oversampling::X4 => {
                let first = self.channels[channel].down_4x.process(input[0]);
                let _ = self.channels[channel].down_4x.process(input[1]);
                let second = self.channels[channel].down_4x.process(input[2]);
                let _ = self.channels[channel].down_4x.process(input[3]);
                let output = self.channels[channel].down_2x.process(first);
                let _ = self.channels[channel].down_2x.process(second);
                output
            }
        }
    }

    const fn latency_frames(&self) -> u32 {
        match self.mode {
            Oversampling::Off => 0,
            Oversampling::X2 => HALFBAND_DELAY,
            Oversampling::X4 => HALFBAND_DELAY + HALFBAND_DELAY / 2,
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
    #[serde(alias = "interpolation_quality")]
    pub oversampling: Oversampling,
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
            oversampling: Oversampling::Off,
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
    oversampler: Oversampler,
    tone_state: [f32; 2],
    dry_delay: [[f32; MAX_OVERSAMPLING_LATENCY]; 2],
    dry_delay_position: usize,
    drive_gain: LinearSmoother,
    bias: LinearSmoother,
    tone_log_hz: LinearSmoother,
    output_gain: LinearSmoother,
    mix: LinearSmoother,
}

impl Saturator {
    pub fn new(config: SaturatorConfig) -> Self {
        let drive_gain = db_to_gain(config.drive_db.clamp(-24.0, 48.0));
        let bias = config.bias.clamp(-1.0, 1.0);
        let tone_log_hz = config.tone_hz.clamp(20.0, 24_000.0).ln();
        let output_gain = db_to_gain(config.output_gain_db.clamp(-36.0, 24.0));
        let mix = config.mix.clamp(0.0, 1.0);
        Self {
            config,
            enabled: true,
            sample_rate: 48_000.0,
            channels: 0,
            maximum_block_size: 0,
            oversampler: Oversampler::default(),
            tone_state: [0.0; 2],
            dry_delay: [[0.0; MAX_OVERSAMPLING_LATENCY]; 2],
            dry_delay_position: 0,
            drive_gain: LinearSmoother::new(drive_gain, 48_000.0, 5.0),
            bias: LinearSmoother::new(bias, 48_000.0, 5.0),
            tone_log_hz: LinearSmoother::new(tone_log_hz, 48_000.0, 10.0),
            output_gain: LinearSmoother::new(output_gain, 48_000.0, 5.0),
            mix: LinearSmoother::new(mix, 48_000.0, 5.0),
        }
    }
    pub(crate) fn prepare_inner(&mut self, sample_rate: f32, channels: usize) {
        self.sample_rate = sample_rate;
        self.channels = channels;
        self.oversampler.prepare(self.config.oversampling);
        self.drive_gain = LinearSmoother::new(
            db_to_gain(self.config.drive_db.clamp(-24.0, 48.0)),
            f64::from(sample_rate),
            5.0,
        );
        self.bias = LinearSmoother::new(
            self.config.bias.clamp(-1.0, 1.0),
            f64::from(sample_rate),
            5.0,
        );
        self.tone_log_hz = LinearSmoother::new(
            self.config.tone_hz.clamp(20.0, 24_000.0).ln(),
            f64::from(sample_rate),
            10.0,
        );
        self.output_gain = LinearSmoother::new(
            db_to_gain(self.config.output_gain_db.clamp(-36.0, 24.0)),
            f64::from(sample_rate),
            5.0,
        );
        self.mix =
            LinearSmoother::new(self.config.mix.clamp(0.0, 1.0), f64::from(sample_rate), 5.0);
        self.reset_inner();
    }
    pub(crate) fn reset_inner(&mut self) {
        self.oversampler.reset();
        self.tone_state = [0.0; 2];
        self.dry_delay = [[0.0; MAX_OVERSAMPLING_LATENCY]; 2];
        self.dry_delay_position = 0;
        self.drive_gain
            .jump_to(db_to_gain(self.config.drive_db.clamp(-24.0, 48.0)));
        self.bias.jump_to(self.config.bias.clamp(-1.0, 1.0));
        self.tone_log_hz
            .jump_to(self.config.tone_hz.clamp(20.0, 24_000.0).ln());
        self.output_gain
            .jump_to(db_to_gain(self.config.output_gain_db.clamp(-36.0, 24.0)));
        self.mix.jump_to(self.config.mix.clamp(0.0, 1.0));
    }

    #[inline]
    pub(crate) fn process_frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        let factor = self.oversampler.mode.factor();
        let drive = self.drive_gain.next();
        let output_gain = self.output_gain.next();
        let mix = self.mix.next();
        let bias = self.bias.next();
        let cutoff = self
            .tone_log_hz
            .next()
            .exp()
            .clamp(20.0, self.sample_rate * 0.49);
        let tone_coefficient =
            (-std::f32::consts::TAU * cutoff / (self.sample_rate * factor as f32)).exp();
        let latency = self.oversampler.latency_frames() as usize;
        for ch in 0..self.channels {
            let dry = if latency == 0 {
                input[ch]
            } else {
                let delayed = self.dry_delay[ch][self.dry_delay_position];
                self.dry_delay[ch][self.dry_delay_position] = input[ch];
                delayed
            };
            let mut high_rate = [0.0; 4];
            let count = self.oversampler.upsample(ch, input[ch], &mut high_rate);
            for sample in &mut high_rate[..count] {
                let driven = (*sample * drive + bias).clamp(-1.0e6, 1.0e6);
                let shaped =
                    waveshape(driven, self.config.curve) - waveshape(bias, self.config.curve);
                self.tone_state[ch] =
                    tone_coefficient * self.tone_state[ch] + (1.0 - tone_coefficient) * shaped;
                *sample = self.tone_state[ch];
            }
            let wet = self.oversampler.downsample(ch, &high_rate) * output_gain;
            output[ch] = dry * (1.0 - mix) + wet * mix;
        }
        if latency != 0 {
            self.dry_delay_position = (self.dry_delay_position + 1) % latency;
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
    #[serde(alias = "interpolation_quality")]
    pub oversampling: Oversampling,
}

impl Default for ClipperConfig {
    fn default() -> Self {
        Self {
            threshold_db: -3.0,
            softness: 0.0,
            output_ceiling_db: -0.1,
            oversampling: Oversampling::Off,
        }
    }
}

#[derive(Debug)]
pub struct Clipper {
    pub config: ClipperConfig,
    enabled: bool,
    channels: usize,
    maximum_block_size: usize,
    oversampler: Oversampler,
    threshold_gain: LinearSmoother,
    softness: LinearSmoother,
    ceiling_gain: LinearSmoother,
}

impl Clipper {
    pub fn new(config: ClipperConfig) -> Self {
        let threshold_gain = db_to_gain(config.threshold_db.clamp(-36.0, 0.0));
        let softness = config.softness.clamp(0.0, 1.0);
        let ceiling_gain = db_to_gain(config.output_ceiling_db.clamp(-36.0, 0.0));
        Self {
            config,
            enabled: true,
            channels: 0,
            maximum_block_size: 0,
            oversampler: Oversampler::default(),
            threshold_gain: LinearSmoother::new(threshold_gain, 48_000.0, 5.0),
            softness: LinearSmoother::new(softness, 48_000.0, 5.0),
            ceiling_gain: LinearSmoother::new(ceiling_gain, 48_000.0, 5.0),
        }
    }
    pub(crate) fn prepare_inner(&mut self, sample_rate: f32, channels: usize) {
        self.channels = channels;
        self.oversampler.prepare(self.config.oversampling);
        self.threshold_gain = LinearSmoother::new(
            db_to_gain(self.config.threshold_db.clamp(-36.0, 0.0)),
            f64::from(sample_rate),
            5.0,
        );
        self.softness = LinearSmoother::new(
            self.config.softness.clamp(0.0, 1.0),
            f64::from(sample_rate),
            5.0,
        );
        self.ceiling_gain = LinearSmoother::new(
            db_to_gain(self.config.output_ceiling_db.clamp(-36.0, 0.0)),
            f64::from(sample_rate),
            5.0,
        );
        self.reset_inner();
    }
    pub(crate) fn reset_inner(&mut self) {
        self.oversampler.reset();
        self.threshold_gain
            .jump_to(db_to_gain(self.config.threshold_db.clamp(-36.0, 0.0)));
        self.softness.jump_to(self.config.softness.clamp(0.0, 1.0));
        self.ceiling_gain
            .jump_to(db_to_gain(self.config.output_ceiling_db.clamp(-36.0, 0.0)));
    }

    #[inline]
    pub(crate) fn process_frame(&mut self, input: [f32; 2], output: &mut [f32; 2]) {
        let threshold = self.threshold_gain.next();
        let configured_ceiling = db_to_gain(self.config.output_ceiling_db.clamp(-36.0, 0.0));
        let ceiling = self.ceiling_gain.next().min(configured_ceiling);
        let softness = self.softness.next();
        for ch in 0..self.channels {
            let mut high_rate = [0.0; 4];
            let count = self.oversampler.upsample(ch, input[ch], &mut high_rate);
            for x in &mut high_rate[..count] {
                let magnitude = x.abs();
                let shaped = if softness <= 1.0e-6 {
                    x.clamp(-threshold, threshold)
                } else {
                    let knee_start = threshold * (1.0 - softness);
                    if magnitude <= knee_start {
                        *x
                    } else {
                        let knee = (threshold - knee_start).max(1.0e-6);
                        let normalized = (magnitude - knee_start) / knee;
                        x.signum() * (knee_start + knee * normalized.tanh())
                    }
                };
                *x = shaped;
            }
            output[ch] = (self.oversampler.downsample(ch, &high_rate) * (ceiling / threshold))
                .clamp(-ceiling, ceiling);
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
    fn latency(&self) -> u32 {
        0
    }
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
                if self.enabled_ref() {
                    self.latency()
                } else {
                    0
                }
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

const OVERSAMPLING: &[&str] = &["off", "x2", "x4"];

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
        id: "oversampling",
        name: "Oversampling",
        kind: ParameterKind::Choice(OVERSAMPLING),
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
            "drive_db" => {
                self.config.drive_db = float_value(event, -24.0, 48.0)?;
                self.drive_gain.set_target(db_to_gain(self.config.drive_db));
            }
            "bias" => {
                self.config.bias = float_value(event, -1.0, 1.0)?;
                self.bias.set_target(self.config.bias);
            }
            "tone_hz" => {
                self.config.tone_hz = float_value(event, 20.0, 24_000.0)?;
                self.tone_log_hz.set_target(self.config.tone_hz.ln());
            }
            "output_gain_db" => {
                self.config.output_gain_db = float_value(event, -36.0, 24.0)?;
                self.output_gain
                    .set_target(db_to_gain(self.config.output_gain_db));
            }
            "mix" => {
                self.config.mix = float_value(event, 0.0, 1.0)?;
                self.mix.set_target(self.config.mix);
            }
            "curve" | "oversampling" => {
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
    fn latency(&self) -> u32 {
        self.oversampler.latency_frames()
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
        id: "oversampling",
        name: "Oversampling",
        kind: ParameterKind::Choice(OVERSAMPLING),
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
            "threshold_db" => {
                self.config.threshold_db = float_value(event, -36.0, 0.0)?;
                self.threshold_gain
                    .set_target(db_to_gain(self.config.threshold_db));
            }
            "softness" => {
                self.config.softness = float_value(event, 0.0, 1.0)?;
                self.softness.set_target(self.config.softness);
            }
            "output_ceiling_db" => {
                self.config.output_ceiling_db = float_value(event, -36.0, 0.0)?;
                self.ceiling_gain
                    .set_target(db_to_gain(self.config.output_ceiling_db));
            }
            "oversampling" => {
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
    fn latency(&self) -> u32 {
        self.oversampler.latency_frames()
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

    #[test]
    fn oversampling_descriptor_is_truthful_and_prepare_time_only() {
        let descriptor = Saturator::default()
            .parameters()
            .iter()
            .find(|descriptor| descriptor.id == "oversampling")
            .unwrap();
        assert_eq!(descriptor.kind, ParameterKind::Choice(&["off", "x2", "x4"]));
        assert!(!descriptor.automatable);
        assert!(
            Saturator::default()
                .parameters()
                .iter()
                .all(|descriptor| descriptor.id != "interpolation_quality")
        );
    }

    #[test]
    fn oversampling_latency_matches_the_aligned_dry_path() {
        for (oversampling, expected_latency) in
            [(Oversampling::X2, 32usize), (Oversampling::X4, 48usize)]
        {
            let mut saturator = Saturator::new(SaturatorConfig {
                mix: 0.0,
                oversampling,
                ..SaturatorConfig::default()
            });
            saturator.prepare(spec()).unwrap();
            assert_eq!(saturator.latency_frames(), expected_latency as u32);

            let mut input = [0.0; 128];
            input[0] = 1.0;
            let output = render(&mut saturator, &input);
            assert!(
                output[..expected_latency]
                    .iter()
                    .all(|sample| *sample == 0.0)
            );
            assert_eq!(output[expected_latency], 1.0);
        }
    }

    fn sinusoid_amplitude(signal: &[f32], frequency_hz: f32, sample_rate: f32) -> f32 {
        let mut sine = 0.0_f64;
        let mut cosine = 0.0_f64;
        for (frame, sample) in signal.iter().enumerate() {
            let phase = std::f64::consts::TAU * f64::from(frequency_hz) * frame as f64
                / f64::from(sample_rate);
            sine += f64::from(*sample) * phase.sin();
            cosine += f64::from(*sample) * phase.cos();
        }
        (2.0 * sine.hypot(cosine) / signal.len() as f64) as f32
    }

    fn render_saturator_alias(oversampling: Oversampling) -> f32 {
        const SAMPLE_RATE: f32 = 48_000.0;
        const FRAMES: usize = 16_384;
        const DISCARD: usize = 4_096;
        let input: Vec<_> = (0..FRAMES)
            .map(|frame| {
                (std::f32::consts::TAU * 15_000.0 * frame as f32 / SAMPLE_RATE).sin() * 0.9
            })
            .collect();
        let mut processor = Saturator::new(SaturatorConfig {
            drive_db: 18.0,
            tone_hz: 24_000.0,
            mix: 1.0,
            oversampling,
            ..SaturatorConfig::default()
        });
        processor
            .prepare(PrepareSpec {
                sample_rate: f64::from(SAMPLE_RATE),
                max_block_size: FRAMES,
                input_layout: AudioLayout::Mono,
                tempo_bpm: 120.0,
            })
            .unwrap();
        let mut output = vec![0.0; FRAMES];
        processor
            .process(
                &[&input],
                &mut [&mut output],
                &[],
                ProcessContext::default(),
            )
            .unwrap();
        sinusoid_amplitude(&output[DISCARD..], 3_000.0, SAMPLE_RATE)
    }

    #[test]
    fn four_times_oversampling_rejects_the_third_harmonic_alias() {
        let unfiltered_alias = render_saturator_alias(Oversampling::Off);
        let oversampled_alias = render_saturator_alias(Oversampling::X4);
        assert!(
            oversampled_alias < unfiltered_alias * 0.1,
            "4x alias {oversampled_alias} was not 20 dB below off alias {unfiltered_alias}"
        );
    }
}
