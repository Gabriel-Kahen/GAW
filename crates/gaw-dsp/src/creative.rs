//! Pitch and beat-synchronous creative processors.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::float_cmp,
    clippy::needless_range_loop
)]

use serde::{Deserialize, Serialize};

use crate::contract::{MONO_AND_STEREO, copy_or_map_bypass, validate_process_io};
use crate::kernel::LinearSmoother;
use crate::{
    AudioLayout, ParameterDescriptor, ParameterEvent, ParameterKind, ParameterUnit, ParameterValue,
    PrepareSpec, ProcessContext, ProcessError, Processor,
};

/// Replaceable engine behind [`PitchShift`]. Engines must not allocate in `process`.
pub trait PitchShiftEngine: Send {
    fn prepare(&mut self, sample_rate: f64, maximum_block_size: usize, channels: usize);
    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]], semitones: f32, mix: f32);
    fn reset(&mut self);
    fn latency_frames(&self) -> u32;
    fn tail_frames(&self) -> u64;
}

/// Deterministic dual-read-head fallback suitable for live monitoring.
#[derive(Debug, Default)]
pub struct DualDelayPitchEngine {
    sample_rate: f32,
    buffers: Vec<Vec<f32>>,
    write: usize,
    phase: f64,
    window: usize,
}

impl PitchShiftEngine for DualDelayPitchEngine {
    fn prepare(&mut self, sample_rate: f64, _maximum_block_size: usize, channels: usize) {
        self.sample_rate = sample_rate as f32;
        self.window = (sample_rate * 0.05).round().max(32.0) as usize;
        self.buffers = vec![vec![0.0; self.window + 4]; channels];
        self.reset();
    }

    fn process(&mut self, input: &[&[f32]], output: &mut [&mut [f32]], semitones: f32, mix: f32) {
        let ratio = 2.0_f64.powf(semitones.clamp(-24.0, 24.0) as f64 / 12.0);
        let phase_increment = (1.0 - ratio) / self.window as f64;
        let wet_mix = mix.clamp(0.0, 1.0);
        for frame in 0..input[0].len() {
            let phase_a = self.phase.rem_euclid(1.0);
            let phase_b = (phase_a + 0.5).rem_euclid(1.0);
            let gain_a = (core::f64::consts::PI * phase_a).sin().powi(2) as f32;
            let gain_b = (core::f64::consts::PI * phase_b).sin().powi(2) as f32;
            for channel in 0..output.len() {
                let source = channel.min(input.len() - 1);
                let dry = input[source][frame];
                self.buffers[channel][self.write] = dry;
                let aligned_dry = read_fractional(
                    &self.buffers[channel],
                    self.write as f32 - self.window as f32 * 0.5,
                );
                let wet_a = read_fractional(
                    &self.buffers[channel],
                    self.write as f32 - phase_a as f32 * self.window as f32,
                );
                let wet_b = read_fractional(
                    &self.buffers[channel],
                    self.write as f32 - phase_b as f32 * self.window as f32,
                );
                let wet = wet_a * gain_a + wet_b * gain_b;
                output[channel][frame] = aligned_dry + (wet - aligned_dry) * wet_mix;
            }
            self.write = (self.write + 1) % self.buffers[0].len();
            self.phase = (self.phase + phase_increment).rem_euclid(1.0);
        }
    }

    fn reset(&mut self) {
        for buffer in &mut self.buffers {
            buffer.fill(0.0);
        }
        self.write = 0;
        self.phase = 0.0;
    }

    fn latency_frames(&self) -> u32 {
        (self.window / 2) as u32
    }

    fn tail_frames(&self) -> u64 {
        self.window as u64
    }
}

fn read_fractional(buffer: &[f32], position: f32) -> f32 {
    let position = position.rem_euclid(buffer.len() as f32);
    let lower = position.floor() as usize;
    let upper = (lower + 1) % buffer.len();
    let fraction = position - lower as f32;
    buffer[lower] + (buffer[upper] - buffer[lower]) * fraction
}

fn default_pitch_engine() -> Box<dyn PitchShiftEngine> {
    Box::<DualDelayPitchEngine>::default()
}

/// Formant behavior supported by the built-in dual-delay pitch engine.
///
/// The engine shifts the complete spectrum and therefore cannot preserve vocal
/// formants independently of pitch.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchFormantMode {
    #[default]
    Shift,
}

/// Quality implemented by the built-in pitch engine.
///
/// More expensive quality labels are intentionally not exposed until a
/// distinct engine with measurably different behavior is available.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PitchQuality {
    #[default]
    Draft,
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct PitchShift {
    pub enabled: bool,
    pub semitones: f32,
    pub cents: f32,
    pub formant_mode: PitchFormantMode,
    pub quality: PitchQuality,
    pub mix: f32,
    #[serde(skip, default = "default_pitch_engine")]
    engine: Box<dyn PitchShiftEngine>,
    #[serde(skip)]
    layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
    #[serde(skip)]
    pitch_smoother: LinearSmoother,
    #[serde(skip)]
    mix_smoother: LinearSmoother,
}

impl core::fmt::Debug for PitchShift {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("PitchShift")
            .field("enabled", &self.enabled)
            .field("semitones", &self.semitones)
            .field("cents", &self.cents)
            .field("formant_mode", &self.formant_mode)
            .field("quality", &self.quality)
            .field("mix", &self.mix)
            .finish_non_exhaustive()
    }
}

impl Default for PitchShift {
    fn default() -> Self {
        Self {
            enabled: true,
            semitones: 0.0,
            cents: 0.0,
            formant_mode: PitchFormantMode::Shift,
            quality: PitchQuality::Draft,
            mix: 1.0,
            engine: default_pitch_engine(),
            layout: None,
            maximum_block_size: 0,
            pitch_smoother: LinearSmoother::default(),
            mix_smoother: LinearSmoother::new(1.0, 48_000.0, 5.0),
        }
    }
}

impl PitchShift {
    pub fn with_engine(engine: Box<dyn PitchShiftEngine>) -> Self {
        Self {
            engine,
            ..Self::default()
        }
    }

    pub fn set_engine(&mut self, engine: Box<dyn PitchShiftEngine>) {
        self.engine = engine;
    }
}

const PITCH_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
        id: "semitones",
        name: "Semitones",
        kind: ParameterKind::Float {
            min: -24.0,
            max: 24.0,
        },
        unit: ParameterUnit::Semitones,
        default: ParameterValue::Float(0.0),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "cents",
        name: "Cents",
        kind: ParameterKind::Float {
            min: -100.0,
            max: 100.0,
        },
        unit: ParameterUnit::Cents,
        default: ParameterValue::Float(0.0),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "formant_mode",
        name: "Formant Mode",
        kind: ParameterKind::Choice(&["shift"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: Some("dual-delay engine shifts formants"),
    },
    ParameterDescriptor {
        id: "quality",
        name: "Quality",
        kind: ParameterKind::Choice(&["draft"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: Some("realtime dual-delay"),
    },
    ParameterDescriptor {
        id: "mix",
        name: "Mix",
        kind: ParameterKind::Float { min: 0.0, max: 1.0 },
        unit: ParameterUnit::Ratio,
        default: ParameterValue::Float(1.0),
        automatable: true,
        display_hint: None,
    },
];

impl Processor for PitchShift {
    fn type_id(&self) -> &'static str {
        "gaw.pitch_shift"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(input)
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.engine.prepare(
            spec.sample_rate,
            spec.max_block_size,
            spec.input_layout.channels(),
        );
        self.pitch_smoother =
            LinearSmoother::new(self.semitones + self.cents / 100.0, spec.sample_rate, 5.0);
        self.mix_smoother = LinearSmoother::new(self.mix, spec.sample_rate, 5.0);
        self.layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        Ok(())
    }
    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        _context: ProcessContext,
    ) -> Result<(), ProcessError> {
        let layout = self.layout.ok_or(ProcessError::NotPrepared)?;
        validate_process_io(
            input,
            output,
            layout,
            layout,
            self.maximum_block_size,
            events,
        )?;
        if !self.enabled {
            copy_or_map_bypass(input, output);
            return Ok(());
        }
        let mut cursor = 0;
        for event in events {
            self.process_pitch_segment(input, output, cursor, event.sample_offset);
            match (event.id.as_str(), event.value) {
                ("semitones", ParameterValue::Float(value))
                    if value.is_finite() && (-24.0..=24.0).contains(&value) =>
                {
                    self.semitones = value;
                    self.pitch_smoother
                        .set_target(self.semitones + self.cents / 100.0);
                }
                ("cents", ParameterValue::Float(value))
                    if value.is_finite() && (-100.0..=100.0).contains(&value) =>
                {
                    self.cents = value;
                    self.pitch_smoother
                        .set_target(self.semitones + self.cents / 100.0);
                }
                ("mix", ParameterValue::Float(value))
                    if value.is_finite() && (0.0..=1.0).contains(&value) =>
                {
                    self.mix = value;
                    self.mix_smoother.set_target(value);
                }
                ("formant_mode" | "quality", _) => {
                    return Err(ProcessError::InvalidParameterValue);
                }
                (id, _) if PITCH_PARAMETERS.iter().any(|parameter| parameter.id == id) => {
                    return Err(ProcessError::InvalidParameterValue);
                }
                _ => return Err(ProcessError::UnknownParameter),
            }
            cursor = event.sample_offset;
        }
        self.process_pitch_segment(input, output, cursor, input[0].len());
        Ok(())
    }
    fn reset(&mut self) {
        self.engine.reset();
        self.pitch_smoother
            .jump_to(self.semitones + self.cents / 100.0);
        self.mix_smoother.jump_to(self.mix);
    }
    fn seek(&mut self, _absolute_frame: u64) {
        self.reset();
    }
    fn latency_frames(&self) -> u32 {
        self.engine.latency_frames()
    }
    fn tail_frames(&self) -> u64 {
        self.engine.tail_frames()
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        PITCH_PARAMETERS
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

impl PitchShift {
    fn process_pitch_segment(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        start: usize,
        end: usize,
    ) {
        if start == end {
            return;
        }
        for frame in start..end {
            let pitch = self.pitch_smoother.next();
            let mix = self.mix_smoother.next();
            match (input, &mut *output) {
                ([mono], [out]) => self.engine.process(
                    &[&mono[frame..=frame]],
                    &mut [&mut out[frame..=frame]],
                    pitch,
                    mix,
                ),
                ([left, right], [out_left, out_right]) => self.engine.process(
                    &[&left[frame..=frame], &right[frame..=frame]],
                    &mut [&mut out_left[frame..=frame], &mut out_right[frame..=frame]],
                    pitch,
                    mix,
                ),
                _ => unreachable!("validated layouts are mono or stereo"),
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RhythmicGate {
    pub enabled: bool,
    pub steps: Vec<f32>,
    pub step_length_beats: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub phase_offset_beats: f32,
    pub mix: f32,
    #[serde(skip)]
    sample_rate: f64,
    #[serde(skip)]
    envelope: f32,
    #[serde(skip)]
    layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
    #[serde(skip)]
    mix_smoother: LinearSmoother,
}

impl Default for RhythmicGate {
    fn default() -> Self {
        Self {
            enabled: true,
            steps: vec![1.0, 0.0, 1.0, 0.0],
            step_length_beats: 0.25,
            attack_ms: 2.0,
            release_ms: 8.0,
            phase_offset_beats: 0.0,
            mix: 1.0,
            sample_rate: 48_000.0,
            envelope: 0.0,
            layout: None,
            maximum_block_size: 0,
            mix_smoother: LinearSmoother::new(1.0, 48_000.0, 5.0),
        }
    }
}

const RHYTHMIC_GATE_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
        id: "step_length_beats",
        name: "Step Length",
        kind: ParameterKind::Float {
            min: 0.03125,
            max: 8.0,
        },
        unit: ParameterUnit::Beats,
        default: ParameterValue::Float(0.25),
        automatable: false,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "phase_offset_beats",
        name: "Phase Offset",
        kind: ParameterKind::Float {
            min: -64.0,
            max: 64.0,
        },
        unit: ParameterUnit::Beats,
        default: ParameterValue::Float(0.0),
        automatable: true,
        display_hint: Some("bipolar"),
    },
    ParameterDescriptor {
        id: "attack_ms",
        name: "Attack",
        kind: ParameterKind::Float {
            min: 0.0,
            max: 100.0,
        },
        unit: ParameterUnit::Milliseconds,
        default: ParameterValue::Float(2.0),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "release_ms",
        name: "Release",
        kind: ParameterKind::Float {
            min: 0.0,
            max: 500.0,
        },
        unit: ParameterUnit::Milliseconds,
        default: ParameterValue::Float(8.0),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "mix",
        name: "Mix",
        kind: ParameterKind::Float { min: 0.0, max: 1.0 },
        unit: ParameterUnit::Ratio,
        default: ParameterValue::Float(1.0),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "steps[].level",
        name: "Step Level",
        kind: ParameterKind::Float { min: 0.0, max: 1.0 },
        unit: ParameterUnit::Ratio,
        default: ParameterValue::Float(1.0),
        automatable: true,
        display_hint: Some("collection schema; events use steps.N.level"),
    },
];

impl Processor for RhythmicGate {
    fn type_id(&self) -> &'static str {
        "gaw.rhythmic_gate"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(input)
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        if self.steps.is_empty()
            || self.steps.len() > 64
            || self
                .steps
                .iter()
                .any(|level| !level.is_finite() || !(0.0..=1.0).contains(level))
        {
            return Err(ProcessError::InvalidParameterValue);
        }
        self.sample_rate = spec.sample_rate;
        self.layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        self.mix_smoother = LinearSmoother::new(self.mix.clamp(0.0, 1.0), spec.sample_rate, 5.0);
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
        let layout = self.layout.ok_or(ProcessError::NotPrepared)?;
        validate_process_io(
            input,
            output,
            layout,
            layout,
            self.maximum_block_size,
            events,
        )?;
        if !self.enabled {
            copy_or_map_bypass(input, output);
            return Ok(());
        }
        let frames_per_beat = self.sample_rate * 60.0 / context.tempo_bpm.max(1.0);
        let step_frames =
            (frames_per_beat * self.step_length_beats.max(1.0 / 64.0) as f64).max(1.0);
        let mut event_index = 0;
        for frame in 0..input[0].len() {
            while event_index < events.len() && events[event_index].sample_offset == frame {
                apply_gate_event(self, &events[event_index])?;
                event_index += 1;
            }
            let attack = coefficient(self.attack_ms, self.sample_rate);
            let release = coefficient(self.release_ms, self.sample_rate);
            let mix = self.mix_smoother.next();
            let position = context.absolute_frame as f64
                + frame as f64
                + self.phase_offset_beats as f64 * frames_per_beat;
            let step = ((position / step_frames).floor() as usize) % self.steps.len().max(1);
            let target = self.steps.get(step).copied().unwrap_or(1.0).clamp(0.0, 1.0);
            let c = if target > self.envelope {
                attack
            } else {
                release
            };
            self.envelope = target + c * (self.envelope - target);
            let gain = 1.0 + (self.envelope - 1.0) * mix;
            for channel in 0..output.len() {
                output[channel][frame] = input[channel.min(input.len() - 1)][frame] * gain;
            }
        }
        Ok(())
    }
    fn reset(&mut self) {
        self.envelope = 0.0;
        self.mix_smoother.jump_to(self.mix.clamp(0.0, 1.0));
    }
    fn seek(&mut self, _absolute_frame: u64) {
        self.reset();
    }
    fn latency_frames(&self) -> u32 {
        0
    }
    fn tail_frames(&self) -> u64 {
        0
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        RHYTHMIC_GATE_PARAMETERS
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

fn apply_gate_event(gate: &mut RhythmicGate, event: &ParameterEvent) -> Result<(), ProcessError> {
    match (event.id.as_str(), event.value) {
        ("attack_ms", ParameterValue::Float(value))
            if value.is_finite() && (0.0..=100.0).contains(&value) =>
        {
            gate.attack_ms = value;
        }
        ("release_ms", ParameterValue::Float(value))
            if value.is_finite() && (0.0..=500.0).contains(&value) =>
        {
            gate.release_ms = value;
        }
        ("phase_offset_beats", ParameterValue::Float(value))
            if value.is_finite() && (-64.0..=64.0).contains(&value) =>
        {
            gate.phase_offset_beats = value;
        }
        ("mix", ParameterValue::Float(value))
            if value.is_finite() && (0.0..=1.0).contains(&value) =>
        {
            gate.mix = value;
            gate.mix_smoother.set_target(value);
        }
        ("step_length_beats", _) => {
            return Err(ProcessError::InvalidParameterValue);
        }
        (id, ParameterValue::Float(value))
            if value.is_finite()
                && (0.0..=1.0).contains(&value)
                && parse_step_parameter(id).is_some() =>
        {
            let index = parse_step_parameter(id).unwrap();
            let Some(level) = gate.steps.get_mut(index) else {
                return Err(ProcessError::InvalidParameterValue);
            };
            *level = value;
        }
        (id, _)
            if RHYTHMIC_GATE_PARAMETERS
                .iter()
                .any(|parameter| parameter.id == id) =>
        {
            return Err(ProcessError::InvalidParameterValue);
        }
        _ => return Err(ProcessError::UnknownParameter),
    }
    Ok(())
}

fn parse_step_parameter(id: &str) -> Option<usize> {
    id.strip_prefix("steps.")?
        .strip_suffix(".level")?
        .parse()
        .ok()
}

fn coefficient(milliseconds: f32, sample_rate: f64) -> f32 {
    if milliseconds <= 0.0 {
        0.0
    } else {
        (-1.0 / (sample_rate as f32 * milliseconds * 0.001)).exp()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BeatRepeat {
    pub enabled: bool,
    pub interval_beats: f32,
    pub slice_length_beats: f32,
    pub repeat_count: u32,
    pub gate: f32,
    pub decay: f32,
    pub pitch_step_semitones: f32,
    pub reverse_probability: f32,
    pub mix: f32,
    pub seed: u64,
    #[serde(skip)]
    sample_rate: f64,
    #[serde(skip)]
    tempo_bpm: f64,
    #[serde(skip)]
    capture: Vec<Vec<f32>>,
    #[serde(skip)]
    write: usize,
    #[serde(skip)]
    layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
}

impl Default for BeatRepeat {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_beats: 1.0,
            slice_length_beats: 0.25,
            repeat_count: 3,
            gate: 0.9,
            decay: 0.85,
            pitch_step_semitones: 0.0,
            reverse_probability: 0.0,
            mix: 1.0,
            seed: 0,
            sample_rate: 48_000.0,
            tempo_bpm: 120.0,
            capture: Vec::new(),
            write: 0,
            layout: None,
            maximum_block_size: 0,
        }
    }
}

const BEAT_REPEAT_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
        id: "interval_beats",
        name: "Interval",
        kind: ParameterKind::Float {
            min: 0.125,
            max: 16.0,
        },
        unit: ParameterUnit::Beats,
        default: ParameterValue::Float(1.0),
        automatable: false,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "gate",
        name: "Gate",
        kind: ParameterKind::Float { min: 0.0, max: 1.0 },
        unit: ParameterUnit::Ratio,
        default: ParameterValue::Float(0.9),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "decay",
        name: "Decay",
        kind: ParameterKind::Float { min: 0.0, max: 1.0 },
        unit: ParameterUnit::Ratio,
        default: ParameterValue::Float(0.85),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "pitch_step_semitones",
        name: "Pitch Step",
        kind: ParameterKind::Float {
            min: -24.0,
            max: 24.0,
        },
        unit: ParameterUnit::Semitones,
        default: ParameterValue::Float(0.0),
        automatable: true,
        display_hint: Some("bipolar"),
    },
    ParameterDescriptor {
        id: "reverse_probability",
        name: "Reverse Probability",
        kind: ParameterKind::Float { min: 0.0, max: 1.0 },
        unit: ParameterUnit::Ratio,
        default: ParameterValue::Float(0.0),
        automatable: true,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "slice_length_beats",
        name: "Slice Length",
        kind: ParameterKind::Float {
            min: 0.03125,
            max: 4.0,
        },
        unit: ParameterUnit::Beats,
        default: ParameterValue::Float(0.25),
        automatable: false,
        display_hint: None,
    },
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
    ParameterDescriptor {
        id: "repeat_count",
        name: "Repeat Count",
        kind: ParameterKind::Integer { min: 1, max: 32 },
        unit: ParameterUnit::None,
        default: ParameterValue::Integer(3),
        automatable: false,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "mix",
        name: "Mix",
        kind: ParameterKind::Float { min: 0.0, max: 1.0 },
        unit: ParameterUnit::Ratio,
        default: ParameterValue::Float(1.0),
        automatable: true,
        display_hint: None,
    },
];

impl Processor for BeatRepeat {
    fn type_id(&self) -> &'static str {
        "gaw.beat_repeat"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(input)
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        if spec.tempo_bpm < 32.0 {
            return Err(ProcessError::InvalidTempo(spec.tempo_bpm));
        }
        self.sample_rate = spec.sample_rate;
        self.tempo_bpm = spec.tempo_bpm;
        let maximum_interval_seconds =
            f64::from(self.interval_beats.clamp(0.125, 16.0)) * 60.0 / 32.0;
        let frames = (spec.sample_rate * maximum_interval_seconds).ceil() as usize + 4;
        self.capture = vec![vec![0.0; frames]; spec.input_layout.channels()];
        self.layout = Some(spec.input_layout);
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
        let layout = self.layout.ok_or(ProcessError::NotPrepared)?;
        validate_process_io(
            input,
            output,
            layout,
            layout,
            self.maximum_block_size,
            events,
        )?;
        if self.capture.is_empty() {
            return Err(ProcessError::NotPrepared);
        }
        if !self.enabled {
            copy_or_map_bypass(input, output);
            return Ok(());
        }
        if context.tempo_bpm < 32.0 || !context.tempo_bpm.is_finite() {
            return Err(ProcessError::InvalidTempo(context.tempo_bpm));
        }
        self.tempo_bpm = context.tempo_bpm;
        let frames_per_beat = self.sample_rate * 60.0 / context.tempo_bpm.max(1.0);
        let interval = (frames_per_beat * self.interval_beats.clamp(0.125, 16.0) as f64)
            .round()
            .max(1.0) as u64;
        let slice = (frames_per_beat * self.slice_length_beats.clamp(1.0 / 32.0, 4.0) as f64)
            .round()
            .max(1.0) as usize;
        for frame in 0..input[0].len() {
            for event in events.iter().filter(|event| event.sample_offset == frame) {
                match (event.id.as_str(), event.value) {
                    ("gate", ParameterValue::Float(value))
                        if value.is_finite() && (0.0..=1.0).contains(&value) =>
                    {
                        self.gate = value;
                    }
                    ("decay", ParameterValue::Float(value))
                        if value.is_finite() && (0.0..=1.0).contains(&value) =>
                    {
                        self.decay = value;
                    }
                    ("pitch_step_semitones", ParameterValue::Float(value))
                        if value.is_finite() && (-24.0..=24.0).contains(&value) =>
                    {
                        self.pitch_step_semitones = value;
                    }
                    ("reverse_probability", ParameterValue::Float(value))
                        if value.is_finite() && (0.0..=1.0).contains(&value) =>
                    {
                        self.reverse_probability = value;
                    }
                    ("mix", ParameterValue::Float(value))
                        if value.is_finite() && (0.0..=1.0).contains(&value) =>
                    {
                        self.mix = value;
                    }
                    (id, _)
                        if BEAT_REPEAT_PARAMETERS
                            .iter()
                            .any(|parameter| parameter.id == id) =>
                    {
                        return Err(ProcessError::InvalidParameterValue);
                    }
                    _ => return Err(ProcessError::UnknownParameter),
                }
            }
            let absolute = context.absolute_frame.saturating_add(frame as u64);
            let within = (absolute % interval) as usize;
            let repeat = within / slice;
            let active = repeat > 0 && repeat <= self.repeat_count as usize;
            let reverse = random_unit(self.seed, absolute / interval)
                < self.reverse_probability.clamp(0.0, 1.0);
            let pitched_ratio = 2.0_f32.powf(self.pitch_step_semitones * repeat as f32 / 12.0);
            for channel in 0..output.len() {
                let source = channel.min(input.len() - 1);
                let dry = input[source][frame];
                let wet = if active {
                    let local = within % slice;
                    let local = if reverse {
                        slice.saturating_sub(1 + local)
                    } else {
                        local
                    };
                    let offset = (local as f32 * pitched_ratio).rem_euclid(slice as f32);
                    let start = self.write as f32 - within as f32;
                    read_fractional(&self.capture[source], start + offset)
                        * self.decay.clamp(0.0, 1.0).powi(repeat as i32)
                        * if local as f32 / slice as f32 <= self.gate.clamp(0.0, 1.0) {
                            1.0
                        } else {
                            0.0
                        }
                } else {
                    dry
                };
                output[channel][frame] = dry + (wet - dry) * self.mix.clamp(0.0, 1.0);
                self.capture[source][self.write] = dry;
            }
            self.write = (self.write + 1) % self.capture[0].len();
        }
        Ok(())
    }
    fn reset(&mut self) {
        for channel in &mut self.capture {
            channel.fill(0.0);
        }
        self.write = 0;
    }
    fn seek(&mut self, _absolute_frame: u64) {
        self.reset();
    }
    fn latency_frames(&self) -> u32 {
        0
    }
    fn tail_frames(&self) -> u64 {
        let interval = self.interval_beats.clamp(0.125, 16.0) as f64 * 60.0
            / self.tempo_bpm.max(32.0)
            * self.sample_rate;
        interval.min(self.capture.first().map_or(0, Vec::len) as f64) as u64
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        BEAT_REPEAT_PARAMETERS
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

fn random_unit(seed: u64, interval: u64) -> f32 {
    let mut value = seed ^ interval.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    ((value ^ (value >> 31)) >> 40) as f32 / (1_u32 << 24) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(max_block_size: usize) -> PrepareSpec {
        PrepareSpec {
            sample_rate: 1_000.0,
            max_block_size,
            input_layout: AudioLayout::Mono,
            tempo_bpm: 60.0,
        }
    }

    #[test]
    fn fallback_pitch_engine_is_deterministic_after_reset() {
        let mut pitch = PitchShift {
            semitones: 7.0,
            ..PitchShift::default()
        };
        pitch.prepare(spec(128)).unwrap();
        let source = core::array::from_fn::<_, 128, _>(|index| (index as f32 * 0.1).sin());
        let mut first = [0.0; 128];
        pitch
            .process(
                &[&source],
                &mut [&mut first],
                &[],
                ProcessContext {
                    absolute_frame: 0,
                    tempo_bpm: 60.0,
                },
            )
            .unwrap();
        pitch.reset();
        let mut second = [0.0; 128];
        pitch
            .process(
                &[&source],
                &mut [&mut second],
                &[],
                ProcessContext {
                    absolute_frame: 0,
                    tempo_bpm: 60.0,
                },
            )
            .unwrap();
        assert_eq!(first, second);
        assert!(first.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn pitch_descriptors_only_claim_the_builtin_engine_modes() {
        let pitch = PitchShift::default();
        let formant = pitch
            .parameters()
            .iter()
            .find(|parameter| parameter.id == "formant_mode")
            .unwrap();
        let quality = pitch
            .parameters()
            .iter()
            .find(|parameter| parameter.id == "quality")
            .unwrap();
        assert_eq!(formant.kind, ParameterKind::Choice(&["shift"]));
        assert_eq!(quality.kind, ParameterKind::Choice(&["draft"]));
        assert!(!formant.automatable);
        assert!(!quality.automatable);
    }

    #[test]
    fn pitch_automation_ramps_instead_of_stepping() {
        let mut pitch = PitchShift::default();
        pitch.prepare(spec(8)).unwrap();
        let source = [0.0; 8];
        let mut output = [0.0; 8];
        pitch
            .process(
                &[&source],
                &mut [&mut output],
                &[ParameterEvent::new(
                    0,
                    "semitones",
                    ParameterValue::Float(12.0),
                )],
                ProcessContext::default(),
            )
            .unwrap();
        assert_eq!(pitch.pitch_smoother.current(), 12.0);

        pitch.reset();
        pitch
            .process(
                &[&source[..2]],
                &mut [&mut output[..2]],
                &[ParameterEvent::new(
                    0,
                    "semitones",
                    ParameterValue::Float(-12.0),
                )],
                ProcessContext::default(),
            )
            .unwrap();
        assert!((-12.0..12.0).contains(&pitch.pitch_smoother.current()));
    }

    #[test]
    fn dry_pitch_path_matches_declared_latency() {
        let mut pitch = PitchShift {
            mix: 0.0,
            ..PitchShift::default()
        };
        pitch.prepare(spec(64)).unwrap();
        let mut source = [0.0; 64];
        source[0] = 1.0;
        let mut output = [0.0; 64];
        pitch
            .process(
                &[&source],
                &mut [&mut output],
                &[],
                ProcessContext::default(),
            )
            .unwrap();
        let latency = pitch.latency_frames() as usize;
        assert_eq!(output[latency], 1.0);
        assert!(output[..latency].iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn seeded_beat_repeat_is_reproducible() {
        let source = core::array::from_fn::<_, 1_500, _>(|index| (index % 37) as f32 / 37.0);
        let mut a = BeatRepeat {
            reverse_probability: 0.5,
            seed: 42,
            ..BeatRepeat::default()
        };
        let mut b = BeatRepeat {
            reverse_probability: 0.5,
            seed: 42,
            ..BeatRepeat::default()
        };
        a.prepare(spec(source.len())).unwrap();
        b.prepare(spec(source.len())).unwrap();
        let mut output_a = [0.0; 1_500];
        let mut output_b = [0.0; 1_500];
        let context = ProcessContext {
            absolute_frame: 0,
            tempo_bpm: 60.0,
        };
        a.process(&[&source], &mut [&mut output_a], &[], context)
            .unwrap();
        b.process(&[&source], &mut [&mut output_b], &[], context)
            .unwrap();
        assert_eq!(output_a, output_b);
    }

    #[test]
    fn rhythmic_gate_uses_absolute_musical_time() {
        let source = [1.0; 64];
        let mut gate = RhythmicGate {
            attack_ms: 0.0,
            release_ms: 0.0,
            ..RhythmicGate::default()
        };
        gate.prepare(spec(64)).unwrap();
        let mut first = [0.0; 64];
        gate.process(
            &[&source],
            &mut [&mut first],
            &[],
            ProcessContext {
                absolute_frame: 300,
                tempo_bpm: 60.0,
            },
        )
        .unwrap();
        gate.seek(300);
        let mut second = [0.0; 64];
        gate.process(
            &[&source],
            &mut [&mut second],
            &[],
            ProcessContext {
                absolute_frame: 300,
                tempo_bpm: 60.0,
            },
        )
        .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn rhythmic_steps_are_validated_and_sample_accurately_automatable() {
        let mut invalid = RhythmicGate {
            steps: Vec::new(),
            ..RhythmicGate::default()
        };
        assert_eq!(
            invalid.prepare(spec(16)).unwrap_err(),
            ProcessError::InvalidParameterValue
        );

        let mut gate = RhythmicGate {
            steps: vec![1.0],
            attack_ms: 0.0,
            release_ms: 0.0,
            ..RhythmicGate::default()
        };
        gate.prepare(spec(8)).unwrap();
        let input = [1.0; 8];
        let mut output = [0.0; 8];
        gate.process(
            &[&input],
            &mut [&mut output],
            &[ParameterEvent::new(
                4,
                "steps.0.level",
                ParameterValue::Float(0.0),
            )],
            ProcessContext::default(),
        )
        .unwrap();
        assert_eq!(&output[..4], &[1.0; 4]);
        assert_eq!(&output[4..], &[0.0; 4]);
        assert!(
            gate.parameters()
                .iter()
                .any(|parameter| parameter.id == "steps[].level" && parameter.automatable)
        );
    }

    #[test]
    fn beat_repeat_reports_low_tempo_interval_tail_and_rejects_unbounded_tempos() {
        let mut repeat = BeatRepeat {
            interval_beats: 16.0,
            ..BeatRepeat::default()
        };
        repeat
            .prepare(PrepareSpec {
                sample_rate: 1_000.0,
                max_block_size: 16,
                input_layout: AudioLayout::Mono,
                tempo_bpm: 32.0,
            })
            .unwrap();
        assert_eq!(repeat.tail_frames(), 30_000);

        let error = BeatRepeat::default()
            .prepare(PrepareSpec {
                tempo_bpm: 31.0,
                ..spec(16)
            })
            .unwrap_err();
        assert_eq!(error, ProcessError::InvalidTempo(31.0));
    }
}
