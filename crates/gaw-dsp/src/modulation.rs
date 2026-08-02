//! Time-stable modulation effects.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::collapsible_if,
    clippy::float_cmp,
    clippy::match_wildcard_for_single_variants,
    clippy::needless_range_loop
)]

use serde::{Deserialize, Serialize};

use crate::contract::{MONO_AND_STEREO, copy_or_map_bypass, validate_process_io};
use crate::{
    AudioLayout, ParameterDescriptor, ParameterEvent, ParameterKind, ParameterUnit, ParameterValue,
    PrepareSpec, ProcessContext, ProcessError, Processor,
};

const TAU: f64 = core::f64::consts::TAU;

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

const fn rate_parameter(default: ParameterValue) -> ParameterDescriptor {
    ParameterDescriptor {
        id: "rate",
        name: "Rate",
        kind: ParameterKind::Rate {
            hertz_min: 0.01,
            hertz_max: 20.0,
            beats_min: 1.0 / 64.0,
            beats_max: 64.0,
        },
        unit: ParameterUnit::None,
        default,
        automatable: true,
        display_hint: None,
    }
}

fn float_event(event: &ParameterEvent, min: f32, max: f32) -> Result<f32, ProcessError> {
    match event.value {
        ParameterValue::Float(value) if value.is_finite() && (min..=max).contains(&value) => {
            Ok(value)
        }
        _ => Err(ProcessError::InvalidParameterValue),
    }
}

fn rate_event(event: &ParameterEvent) -> Result<ModulationRate, ProcessError> {
    match event.value {
        ParameterValue::Hertz(rate) if rate.is_finite() && (0.01..=20.0).contains(&rate) => {
            Ok(ModulationRate::Hertz(rate))
        }
        ParameterValue::Beats(period)
            if period.is_finite() && (1.0 / 64.0..=64.0).contains(&period) =>
        {
            Ok(ModulationRate::Beats(period))
        }
        _ => Err(ProcessError::InvalidParameterValue),
    }
}

fn feedback_tail_frames(
    sample_rate: f32,
    delay_frames: f32,
    feedback: f32,
    cap_seconds: f32,
) -> u64 {
    let feedback = feedback.abs().clamp(0.0, 0.95);
    let repeats = if feedback <= 0.000_1 {
        1.0
    } else {
        (-80.0 / (20.0 * feedback.log10())).ceil()
    };
    (delay_frames * repeats)
        .min(sample_rate * cap_seconds)
        .max(0.0) as u64
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "unit", content = "value")]
pub enum ModulationRate {
    Hertz(f32),
    Beats(f32),
}

impl Default for ModulationRate {
    fn default() -> Self {
        Self::Hertz(0.8)
    }
}

impl ModulationRate {
    fn hertz(self, bpm: f64) -> f64 {
        match self {
            Self::Hertz(rate) => rate.max(0.0) as f64,
            Self::Beats(period) => bpm.max(1.0) / (60.0 * period.max(1.0 / 64.0) as f64),
        }
    }

    fn phase(self, absolute_frame: u64, sample_rate: f32, bpm: f64, offset: f32) -> f32 {
        let cycles = absolute_frame as f64 * self.hertz(bpm) / sample_rate as f64 + offset as f64;
        (TAU * cycles).sin() as f32
    }
}

#[derive(Debug, Default)]
struct ModDelay {
    buffers: Vec<Vec<f32>>,
    write: usize,
}

impl ModDelay {
    fn prepare(&mut self, channels: usize, sample_rate: f32, maximum_ms: f32) {
        let len = (sample_rate * maximum_ms * 0.001).ceil() as usize + 4;
        self.buffers = vec![vec![0.0; len]; channels];
        self.write = 0;
    }

    fn clear(&mut self) {
        for buffer in &mut self.buffers {
            buffer.fill(0.0);
        }
        self.write = 0;
    }

    fn read(&self, channel: usize, delay_frames: f32) -> f32 {
        let buffer = &self.buffers[channel];
        let position = (self.write as f32 - delay_frames).rem_euclid(buffer.len() as f32);
        let lower = position.floor() as usize;
        let upper = (lower + 1) % buffer.len();
        let fraction = position - lower as f32;
        buffer[lower] + (buffer[upper] - buffer[lower]) * fraction
    }

    fn write(&mut self, channel: usize, sample: f32) {
        self.buffers[channel][self.write] = sample;
    }

    fn advance(&mut self) {
        self.write = (self.write + 1) % self.buffers[0].len();
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Chorus {
    pub enabled: bool,
    pub rate: ModulationRate,
    pub depth: f32,
    pub base_delay_ms: f32,
    pub voices: u8,
    pub stereo_phase: f32,
    pub feedback: f32,
    pub width: f32,
    pub mix: f32,
    #[serde(skip)]
    sample_rate: f32,
    #[serde(skip)]
    delay: ModDelay,
    #[serde(skip)]
    input_layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
}

impl Default for Chorus {
    fn default() -> Self {
        Self {
            enabled: true,
            rate: ModulationRate::Hertz(0.8),
            depth: 0.5,
            base_delay_ms: 12.0,
            voices: 3,
            stereo_phase: 0.25,
            feedback: 0.05,
            width: 1.0,
            mix: 0.35,
            sample_rate: 48_000.0,
            delay: ModDelay::default(),
            input_layout: None,
            maximum_block_size: 0,
        }
    }
}

const CHORUS_PARAMETERS: &[ParameterDescriptor] = &[
    rate_parameter(ParameterValue::Hertz(0.8)),
    float_parameter("depth", "Depth", 0.0, 1.0, 0.5, ParameterUnit::Ratio),
    float_parameter(
        "base_delay_ms",
        "Base Delay",
        1.0,
        40.0,
        12.0,
        ParameterUnit::Milliseconds,
    ),
    ParameterDescriptor {
        id: "voices",
        name: "Voices",
        kind: ParameterKind::Integer { min: 1, max: 8 },
        unit: ParameterUnit::None,
        default: ParameterValue::Integer(3),
        automatable: false,
        display_hint: None,
    },
    float_parameter(
        "stereo_phase",
        "Stereo Phase",
        0.0,
        1.0,
        0.25,
        ParameterUnit::Ratio,
    ),
    float_parameter(
        "feedback",
        "Feedback",
        -0.95,
        0.95,
        0.05,
        ParameterUnit::Ratio,
    ),
    float_parameter("width", "Width", 0.0, 2.0, 1.0, ParameterUnit::Ratio),
    float_parameter("mix", "Mix", 0.0, 1.0, 0.35, ParameterUnit::Ratio),
];

impl Chorus {
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "rate" => self.rate = rate_event(event)?,
            "depth" => self.depth = float_event(event, 0.0, 1.0)?,
            "base_delay_ms" => self.base_delay_ms = float_event(event, 1.0, 40.0)?,
            "voices" => return Err(ProcessError::InvalidParameterValue),
            "stereo_phase" => self.stereo_phase = float_event(event, 0.0, 1.0)?,
            "feedback" => self.feedback = float_event(event, -0.95, 0.95)?,
            "width" => self.width = float_event(event, 0.0, 2.0)?,
            "mix" => self.mix = float_event(event, 0.0, 1.0)?,
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
}

impl Processor for Chorus {
    fn type_id(&self) -> &'static str {
        "gaw.chorus"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, _input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(AudioLayout::Stereo)
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.sample_rate = spec.sample_rate as f32;
        self.delay.prepare(2, spec.sample_rate as f32, 80.0);
        self.input_layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        Ok(())
    }

    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        context: ProcessContext,
    ) -> Result<(), ProcessError> {
        if self.delay.buffers.is_empty() {
            return Err(ProcessError::NotPrepared);
        }
        let layout = self.input_layout.ok_or(ProcessError::NotPrepared)?;
        let frames = validate_process_io(
            input,
            output,
            layout,
            AudioLayout::Stereo,
            self.maximum_block_size,
            events,
        )?;
        if !self.enabled {
            copy_or_map_bypass(input, output);
            return Ok(());
        }
        let channels = input.len();
        let output_channels = output.len();
        let voices = self.voices.clamp(1, 8) as usize;
        for frame in 0..frames {
            for event in events.iter().filter(|event| event.sample_offset == frame) {
                self.apply_event(event)?;
            }
            let absolute = context.absolute_frame.saturating_add(frame as u64);
            let mix = self.mix.clamp(0.0, 1.0);
            let feedback = self.feedback.clamp(-0.95, 0.95);
            let dry = [
                input[0][frame],
                input[channels.saturating_sub(1).min(1)][frame],
            ];
            let mut wet = [0.0; 2];
            for channel in 0..2 {
                for voice in 0..voices {
                    let voice_phase = voice as f32 / voices as f32;
                    let stereo_phase = if channel == 0 { 0.0 } else { self.stereo_phase };
                    let lfo = self.rate.phase(
                        absolute,
                        self.sample_rate,
                        context.tempo_bpm,
                        voice_phase + stereo_phase,
                    );
                    let delay_ms = self.base_delay_ms.clamp(1.0, 40.0)
                        * (1.0 + lfo * self.depth.clamp(0.0, 1.0) * 0.9);
                    wet[channel] += self
                        .delay
                        .read(channel, (delay_ms * self.sample_rate * 0.001).max(1.0));
                }
                wet[channel] /= voices as f32;
                self.delay
                    .write(channel, dry[channel] + wet[channel] * feedback);
            }
            let mid = (wet[0] + wet[1]) * 0.5;
            let side = (wet[0] - wet[1]) * 0.5 * self.width.clamp(0.0, 2.0);
            wet = [mid + side, mid - side];
            for channel in 0..output_channels.min(2) {
                let dry_sample = dry[channel.min(channels - 1)];
                output[channel][frame] = dry_sample + (wet[channel] - dry_sample) * mix;
            }
            self.delay.advance();
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.delay.clear();
    }

    fn seek(&mut self, _frame: u64) {
        self.delay.clear();
    }

    fn latency_frames(&self) -> u32 {
        0
    }

    fn tail_frames(&self) -> u64 {
        let maximum_delay = self.base_delay_ms.clamp(1.0, 40.0) * 1.9 * self.sample_rate * 0.001;
        feedback_tail_frames(self.sample_rate, maximum_delay, self.feedback, 10.0)
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        CHORUS_PARAMETERS
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Flanger {
    pub enabled: bool,
    pub rate: ModulationRate,
    pub depth: f32,
    pub base_delay_ms: f32,
    pub feedback: f32,
    pub stereo_phase: f32,
    pub mix: f32,
    #[serde(skip)]
    sample_rate: f32,
    #[serde(skip)]
    delay: ModDelay,
    #[serde(skip)]
    input_layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
}

impl Default for Flanger {
    fn default() -> Self {
        Self {
            enabled: true,
            rate: ModulationRate::Hertz(0.25),
            depth: 0.7,
            base_delay_ms: 2.0,
            feedback: 0.45,
            stereo_phase: 0.25,
            mix: 0.5,
            sample_rate: 48_000.0,
            delay: ModDelay::default(),
            input_layout: None,
            maximum_block_size: 0,
        }
    }
}

const FLANGER_PARAMETERS: &[ParameterDescriptor] = &[
    rate_parameter(ParameterValue::Hertz(0.25)),
    float_parameter("depth", "Depth", 0.0, 1.0, 0.7, ParameterUnit::Ratio),
    float_parameter(
        "base_delay_ms",
        "Base Delay",
        0.1,
        10.0,
        2.0,
        ParameterUnit::Milliseconds,
    ),
    float_parameter(
        "feedback",
        "Feedback",
        -0.95,
        0.95,
        0.45,
        ParameterUnit::Ratio,
    ),
    float_parameter(
        "stereo_phase",
        "Stereo Phase",
        0.0,
        1.0,
        0.25,
        ParameterUnit::Ratio,
    ),
    float_parameter("mix", "Mix", 0.0, 1.0, 0.5, ParameterUnit::Ratio),
];

impl Flanger {
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "rate" => self.rate = rate_event(event)?,
            "depth" => self.depth = float_event(event, 0.0, 1.0)?,
            "base_delay_ms" => self.base_delay_ms = float_event(event, 0.1, 10.0)?,
            "feedback" => self.feedback = float_event(event, -0.95, 0.95)?,
            "stereo_phase" => self.stereo_phase = float_event(event, 0.0, 1.0)?,
            "mix" => self.mix = float_event(event, 0.0, 1.0)?,
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
}

impl Processor for Flanger {
    fn type_id(&self) -> &'static str {
        "gaw.flanger"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, _input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(AudioLayout::Stereo)
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.sample_rate = spec.sample_rate as f32;
        self.delay.prepare(2, spec.sample_rate as f32, 20.0);
        self.input_layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        Ok(())
    }

    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        context: ProcessContext,
    ) -> Result<(), ProcessError> {
        if self.delay.buffers.is_empty() {
            return Err(ProcessError::NotPrepared);
        }
        let layout = self.input_layout.ok_or(ProcessError::NotPrepared)?;
        let frames = validate_process_io(
            input,
            output,
            layout,
            AudioLayout::Stereo,
            self.maximum_block_size,
            events,
        )?;
        if !self.enabled {
            copy_or_map_bypass(input, output);
            return Ok(());
        }
        let input_channels = input.len();
        let output_channels = output.len();
        for frame in 0..frames {
            for event in events.iter().filter(|event| event.sample_offset == frame) {
                self.apply_event(event)?;
            }
            let absolute = context.absolute_frame.saturating_add(frame as u64);
            let mix = self.mix.clamp(0.0, 1.0);
            for channel in 0..output_channels.min(2) {
                let source_channel = channel.min(input_channels - 1);
                let dry = input[source_channel][frame];
                let phase = if channel == 0 { 0.0 } else { self.stereo_phase };
                let lfo = self
                    .rate
                    .phase(absolute, self.sample_rate, context.tempo_bpm, phase);
                let delay_ms = self.base_delay_ms.clamp(0.1, 10.0)
                    * (1.0 + self.depth.clamp(0.0, 1.0) * lfo * 0.95);
                let wet = self
                    .delay
                    .read(channel, (delay_ms * self.sample_rate * 0.001).max(1.0));
                self.delay
                    .write(channel, dry + wet * self.feedback.clamp(-0.95, 0.95));
                output[channel][frame] = dry + (wet - dry) * mix;
            }
            self.delay.advance();
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.delay.clear();
    }

    fn seek(&mut self, _frame: u64) {
        self.delay.clear();
    }

    fn latency_frames(&self) -> u32 {
        0
    }

    fn tail_frames(&self) -> u64 {
        let maximum_delay = self.base_delay_ms.clamp(0.1, 10.0) * 1.95 * self.sample_rate * 0.001;
        feedback_tail_frames(self.sample_rate, maximum_delay, self.feedback, 10.0)
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        FLANGER_PARAMETERS
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

const MAX_PHASER_STAGES: usize = 12;

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Phaser {
    pub enabled: bool,
    pub rate: ModulationRate,
    pub depth: f32,
    pub center_frequency_hz: f32,
    pub frequency_span: f32,
    pub stages: u8,
    pub feedback: f32,
    pub stereo_phase: f32,
    pub mix: f32,
    #[serde(skip)]
    sample_rate: f32,
    #[serde(skip)]
    states: [[f32; MAX_PHASER_STAGES]; 2],
    #[serde(skip)]
    feedback_state: [f32; 2],
    #[serde(skip)]
    input_layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
}

impl Default for Phaser {
    fn default() -> Self {
        Self {
            enabled: true,
            rate: ModulationRate::Hertz(0.35),
            depth: 0.7,
            center_frequency_hz: 900.0,
            frequency_span: 0.75,
            stages: 6,
            feedback: 0.25,
            stereo_phase: 0.25,
            mix: 0.5,
            sample_rate: 48_000.0,
            states: [[0.0; MAX_PHASER_STAGES]; 2],
            feedback_state: [0.0; 2],
            input_layout: None,
            maximum_block_size: 0,
        }
    }
}

const PHASER_PARAMETERS: &[ParameterDescriptor] = &[
    rate_parameter(ParameterValue::Hertz(0.35)),
    float_parameter("depth", "Depth", 0.0, 1.0, 0.7, ParameterUnit::Ratio),
    float_parameter(
        "center_frequency_hz",
        "Center Frequency",
        20.0,
        20_000.0,
        900.0,
        ParameterUnit::Hertz,
    ),
    float_parameter(
        "frequency_span",
        "Frequency Span",
        0.0,
        4.0,
        0.75,
        ParameterUnit::Ratio,
    ),
    ParameterDescriptor {
        id: "stages",
        name: "Stages",
        kind: ParameterKind::Integer { min: 2, max: 12 },
        unit: ParameterUnit::None,
        default: ParameterValue::Integer(6),
        automatable: false,
        display_hint: None,
    },
    float_parameter(
        "feedback",
        "Feedback",
        -0.95,
        0.95,
        0.25,
        ParameterUnit::Ratio,
    ),
    float_parameter(
        "stereo_phase",
        "Stereo Phase",
        0.0,
        1.0,
        0.25,
        ParameterUnit::Ratio,
    ),
    float_parameter("mix", "Mix", 0.0, 1.0, 0.5, ParameterUnit::Ratio),
];

impl Phaser {
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "rate" => self.rate = rate_event(event)?,
            "depth" => self.depth = float_event(event, 0.0, 1.0)?,
            "center_frequency_hz" => self.center_frequency_hz = float_event(event, 20.0, 20_000.0)?,
            "frequency_span" => self.frequency_span = float_event(event, 0.0, 4.0)?,
            "stages" => return Err(ProcessError::InvalidParameterValue),
            "feedback" => self.feedback = float_event(event, -0.95, 0.95)?,
            "stereo_phase" => self.stereo_phase = float_event(event, 0.0, 1.0)?,
            "mix" => self.mix = float_event(event, 0.0, 1.0)?,
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
}

impl Processor for Phaser {
    fn type_id(&self) -> &'static str {
        "gaw.phaser"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(match input {
            AudioLayout::Mono => AudioLayout::Stereo,
            layout => layout,
        })
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.sample_rate = spec.sample_rate as f32;
        self.input_layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        self.reset();
        Ok(())
    }

    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        context: ProcessContext,
    ) -> Result<(), ProcessError> {
        let input_layout = self.input_layout.ok_or(ProcessError::NotPrepared)?;
        let output_layout = self.output_layout(input_layout)?;
        let frames = validate_process_io(
            input,
            output,
            input_layout,
            output_layout,
            self.maximum_block_size,
            events,
        )?;
        if !self.enabled {
            copy_or_map_bypass(input, output);
            return Ok(());
        }
        let channels = input.len();
        let stages = self.stages.clamp(2, MAX_PHASER_STAGES as u8) as usize;
        for frame in 0..frames {
            for event in events.iter().filter(|event| event.sample_offset == frame) {
                self.apply_event(event)?;
            }
            let absolute = context.absolute_frame.saturating_add(frame as u64);
            let mix = self.mix.clamp(0.0, 1.0);
            for channel in 0..output.len().min(2) {
                let dry = input[channel.min(channels - 1)][frame];
                let phase = if channel == 0 { 0.0 } else { self.stereo_phase };
                let lfo = self
                    .rate
                    .phase(absolute, self.sample_rate, context.tempo_bpm, phase)
                    * self.depth.clamp(0.0, 1.0);
                let octaves = self.frequency_span.clamp(0.0, 4.0) * lfo;
                let frequency = (self.center_frequency_hz * 2.0_f32.powf(octaves))
                    .clamp(20.0, self.sample_rate * 0.45);
                let tangent = (core::f32::consts::PI * frequency / self.sample_rate).tan();
                let coefficient = (1.0 - tangent) / (1.0 + tangent);
                let mut wet = dry + self.feedback_state[channel] * self.feedback.clamp(-0.95, 0.95);
                for stage in 0..stages {
                    let output = -coefficient * wet + self.states[channel][stage];
                    self.states[channel][stage] = wet + coefficient * output;
                    wet = output;
                }
                self.feedback_state[channel] = wet;
                output[channel][frame] = dry + (wet - dry) * mix;
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.states = [[0.0; MAX_PHASER_STAGES]; 2];
        self.feedback_state = [0.0; 2];
    }

    fn seek(&mut self, _frame: u64) {
        self.reset();
    }

    fn latency_frames(&self) -> u32 {
        0
    }

    fn tail_frames(&self) -> u64 {
        let minimum_frequency = (self.center_frequency_hz
            * 2.0_f32.powf(-self.frequency_span.clamp(0.0, 4.0)))
        .clamp(20.0, self.sample_rate * 0.45);
        let tangent = (core::f32::consts::PI * minimum_frequency / self.sample_rate).tan();
        let pole = ((1.0 - tangent) / (1.0 + tangent)).abs();
        let decay = pole.max(self.feedback.abs().clamp(0.0, 0.95));
        if decay <= 0.000_1 {
            0
        } else {
            ((1.0e-4_f32.ln() / decay.ln()).ceil()
                * self.stages.clamp(2, MAX_PHASER_STAGES as u8) as f32)
                .min(self.sample_rate * 10.0) as u64
        }
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        PHASER_PARAMETERS
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TremoloMode {
    #[default]
    Tremolo,
    Autopan,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Waveform {
    #[default]
    Sine,
    Triangle,
    Square,
}

fn oscillator(waveform: Waveform, phase: f64) -> f32 {
    let phase = phase.rem_euclid(1.0);
    match waveform {
        Waveform::Sine => (TAU * phase).sin() as f32,
        Waveform::Triangle => (1.0 - 4.0 * (phase - 0.5).abs()) as f32,
        Waveform::Square => {
            if phase < 0.5 {
                1.0
            } else {
                -1.0
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TremoloAutopan {
    pub enabled: bool,
    pub mode: TremoloMode,
    pub rate: ModulationRate,
    pub depth: f32,
    pub waveform: Waveform,
    pub phase: f32,
    pub stereo_phase: f32,
    pub smoothing: f32,
    #[serde(skip)]
    sample_rate: f32,
    #[serde(skip)]
    smoothed: [f32; 2],
    #[serde(skip)]
    input_layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
}

impl Default for TremoloAutopan {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: TremoloMode::Tremolo,
            rate: ModulationRate::Beats(1.0),
            depth: 0.6,
            waveform: Waveform::Sine,
            phase: 0.0,
            stereo_phase: 0.5,
            smoothing: 0.2,
            sample_rate: 48_000.0,
            smoothed: [1.0; 2],
            input_layout: None,
            maximum_block_size: 0,
        }
    }
}

const TREMOLO_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
        id: "mode",
        name: "Mode",
        kind: ParameterKind::Choice(&["tremolo", "autopan"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: None,
    },
    rate_parameter(ParameterValue::Beats(1.0)),
    float_parameter("depth", "Depth", 0.0, 1.0, 0.6, ParameterUnit::Ratio),
    ParameterDescriptor {
        id: "waveform",
        name: "Waveform",
        kind: ParameterKind::Choice(&["sine", "triangle", "square"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: None,
    },
    float_parameter("phase", "Phase", 0.0, 1.0, 0.0, ParameterUnit::Ratio),
    float_parameter(
        "stereo_phase",
        "Stereo Phase",
        0.0,
        1.0,
        0.5,
        ParameterUnit::Ratio,
    ),
    float_parameter(
        "smoothing",
        "Smoothing",
        0.0,
        1.0,
        0.2,
        ParameterUnit::Ratio,
    ),
];

impl TremoloAutopan {
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "mode" | "waveform" => {
                return Err(ProcessError::InvalidParameterValue);
            }
            "rate" => self.rate = rate_event(event)?,
            "depth" => self.depth = float_event(event, 0.0, 1.0)?,
            "phase" => self.phase = float_event(event, 0.0, 1.0)?,
            "stereo_phase" => self.stereo_phase = float_event(event, 0.0, 1.0)?,
            "smoothing" => self.smoothing = float_event(event, 0.0, 1.0)?,
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
}

impl Processor for TremoloAutopan {
    fn type_id(&self) -> &'static str {
        "gaw.tremolo_autopan"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(match (self.mode, input) {
            (TremoloMode::Autopan, AudioLayout::Mono) => AudioLayout::Stereo,
            (_, layout) => layout,
        })
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.sample_rate = spec.sample_rate as f32;
        self.input_layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        self.reset();
        Ok(())
    }

    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        context: ProcessContext,
    ) -> Result<(), ProcessError> {
        let input_layout = self.input_layout.ok_or(ProcessError::NotPrepared)?;
        let output_layout = self.output_layout(input_layout)?;
        let frames = validate_process_io(
            input,
            output,
            input_layout,
            output_layout,
            self.maximum_block_size,
            events,
        )?;
        if !self.enabled {
            copy_or_map_bypass(input, output);
            return Ok(());
        }
        for frame in 0..frames {
            for event in events.iter().filter(|event| event.sample_offset == frame) {
                self.apply_event(event)?;
            }
            let smoothing_ms = self.smoothing.clamp(0.0, 1.0) * 20.0;
            let coefficient = if smoothing_ms <= 0.0 {
                0.0
            } else {
                (-1.0 / (self.sample_rate * smoothing_ms * 0.001)).exp()
            };
            let depth = self.depth.clamp(0.0, 1.0);
            let absolute = context.absolute_frame.saturating_add(frame as u64);
            let cycles = absolute as f64 * self.rate.hertz(context.tempo_bpm)
                / self.sample_rate as f64
                + self.phase as f64;
            for channel in 0..output.len().min(2) {
                let lfo = match self.mode {
                    TremoloMode::Tremolo => oscillator(
                        self.waveform,
                        cycles + channel as f64 * self.stereo_phase as f64,
                    ),
                    TremoloMode::Autopan => {
                        let pan = oscillator(self.waveform, cycles) * depth;
                        if channel == 0 { -pan } else { pan }
                    }
                };
                let target = match self.mode {
                    TremoloMode::Tremolo => 1.0 - depth * (lfo + 1.0) * 0.5,
                    TremoloMode::Autopan => ((1.0 + lfo) * 0.5).sqrt(),
                };
                self.smoothed[channel] = target + coefficient * (self.smoothed[channel] - target);
                let source = channel.min(input.len() - 1);
                output[channel][frame] = input[source][frame] * self.smoothed[channel];
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.smoothed = [1.0; 2];
    }

    fn seek(&mut self, _frame: u64) {
        self.reset();
    }

    fn latency_frames(&self) -> u32 {
        0
    }

    fn tail_frames(&self) -> u64 {
        0
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        TREMOLO_PARAMETERS
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(layout: AudioLayout) -> PrepareSpec {
        PrepareSpec {
            sample_rate: 48_000.0,
            max_block_size: 128,
            input_layout: layout,
            tempo_bpm: 120.0,
        }
    }

    #[test]
    fn absolute_time_oscillator_repeats_after_seek() {
        let mut effect = TremoloAutopan {
            smoothing: 0.0,
            ..TremoloAutopan::default()
        };
        effect.prepare(spec(AudioLayout::Mono)).unwrap();
        let source = [1.0; 128];
        let mut first = [0.0; 128];
        effect
            .process(
                &[&source],
                &mut [&mut first],
                &[],
                ProcessContext {
                    absolute_frame: 91_337,
                    tempo_bpm: 123.0,
                },
            )
            .unwrap();
        effect.seek(91_337);
        let mut second = [0.0; 128];
        effect
            .process(
                &[&source],
                &mut [&mut second],
                &[],
                ProcessContext {
                    absolute_frame: 91_337,
                    tempo_bpm: 123.0,
                },
            )
            .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn spatial_modulators_explicitly_expand_mono() {
        assert_eq!(
            Chorus::default().output_layout(AudioLayout::Mono).unwrap(),
            AudioLayout::Stereo
        );
        assert_eq!(
            Flanger::default().output_layout(AudioLayout::Mono).unwrap(),
            AudioLayout::Stereo
        );
        let autopan = TremoloAutopan {
            mode: TremoloMode::Autopan,
            ..TremoloAutopan::default()
        };
        assert_eq!(
            autopan.output_layout(AudioLayout::Mono).unwrap(),
            AudioLayout::Stereo
        );
    }

    #[test]
    fn modulation_contracts_expose_rate_and_feedback_tails() {
        let mut chorus = Chorus {
            base_delay_ms: 40.0,
            feedback: 0.0,
            ..Chorus::default()
        };
        chorus.prepare(spec(AudioLayout::Mono)).unwrap();
        let ids: Vec<_> = chorus
            .parameters()
            .iter()
            .map(|parameter| parameter.id)
            .collect();
        assert!(ids.contains(&"rate"));
        assert!(ids.contains(&"voices"));
        assert!(ids.contains(&"feedback"));
        let no_feedback = chorus.tail_frames();
        chorus.feedback = 0.9;
        assert!(chorus.tail_frames() > no_feedback);

        let tremolo = TremoloAutopan::default();
        let ids: Vec<_> = tremolo
            .parameters()
            .iter()
            .map(|parameter| parameter.id)
            .collect();
        assert!(ids.contains(&"mode"));
        assert!(ids.contains(&"waveform"));
        assert!(ids.contains(&"smoothing"));
    }
}
