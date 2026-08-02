//! Delay and algorithmic reverberation processors.

#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::collapsible_if,
    clippy::float_cmp,
    clippy::match_wildcard_for_single_variants
)]

use serde::{Deserialize, Serialize};

use crate::contract::{MONO_AND_STEREO, copy_or_map_bypass, validate_process_io};
use crate::{
    AudioLayout, ParameterDescriptor, ParameterEvent, ParameterKind, ParameterUnit, ParameterValue,
    PrepareSpec, ProcessContext, ProcessError, Processor,
};

const MAX_DELAY_SECONDS: f32 = 8.0;
const MAX_TAIL_SECONDS: f32 = 30.0;

const fn float_parameter(
    id: &'static str,
    name: &'static str,
    min: f32,
    max: f32,
    default: f32,
    unit: ParameterUnit,
    automatable: bool,
) -> ParameterDescriptor {
    ParameterDescriptor {
        id,
        name,
        kind: ParameterKind::Float { min, max },
        unit,
        default: ParameterValue::Float(default),
        automatable,
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

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "unit", content = "value")]
pub enum TimeValue {
    Seconds(f32),
    Beats(f32),
}

impl Default for TimeValue {
    fn default() -> Self {
        Self::Beats(0.5)
    }
}

impl TimeValue {
    pub(crate) fn seconds(self, bpm: f64) -> f32 {
        match self {
            Self::Seconds(value) => value.max(0.0),
            Self::Beats(value) => value.max(0.0) * (60.0 / bpm.max(1.0) as f32),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelayStereoMode {
    #[default]
    Linked,
    Offset,
    PingPong,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Delay {
    pub enabled: bool,
    pub time: TimeValue,
    pub feedback: f32,
    pub stereo_mode: DelayStereoMode,
    pub stereo_offset: f32,
    pub low_cut_hz: f32,
    pub high_cut_hz: f32,
    pub modulation_rate_hz: f32,
    pub modulation_depth: f32,
    pub width: f32,
    pub mix: f32,
    #[serde(skip)]
    sample_rate: f32,
    #[serde(skip)]
    ring: Vec<Vec<f32>>,
    #[serde(skip)]
    write: usize,
    #[serde(skip)]
    lp: [f32; 2],
    #[serde(skip)]
    hp_x: [f32; 2],
    #[serde(skip)]
    hp_y: [f32; 2],
    #[serde(skip)]
    input_layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
    #[serde(skip)]
    tempo_bpm: f64,
}

impl Default for Delay {
    fn default() -> Self {
        Self {
            enabled: true,
            time: TimeValue::default(),
            feedback: 0.35,
            stereo_mode: DelayStereoMode::Linked,
            stereo_offset: 0.0,
            low_cut_hz: 20.0,
            high_cut_hz: 20_000.0,
            modulation_rate_hz: 0.0,
            modulation_depth: 0.0,
            width: 1.0,
            mix: 0.2,
            sample_rate: 48_000.0,
            ring: Vec::new(),
            write: 0,
            lp: [0.0; 2],
            hp_x: [0.0; 2],
            hp_y: [0.0; 2],
            input_layout: None,
            maximum_block_size: 0,
            tempo_bpm: 120.0,
        }
    }
}

impl Delay {
    fn allocate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        let len = (sample_rate * MAX_DELAY_SECONDS).ceil() as usize + 4;
        self.ring = vec![vec![0.0; len]; 2];
        self.clear();
    }

    fn clear(&mut self) {
        for channel in &mut self.ring {
            channel.fill(0.0);
        }
        self.write = 0;
        self.lp = [0.0; 2];
        self.hp_x = [0.0; 2];
        self.hp_y = [0.0; 2];
    }

    fn read(&self, channel: usize, delay: f32) -> f32 {
        let ring = &self.ring[channel];
        let len = ring.len() as f32;
        let position = (self.write as f32 - delay).rem_euclid(len);
        let lower = position.floor() as usize;
        let fraction = position - lower as f32;
        let upper = (lower + 1) % ring.len();
        ring[lower] + (ring[upper] - ring[lower]) * fraction
    }

    fn filter_feedback(&mut self, channel: usize, input: f32) -> f32 {
        let sr = self.sample_rate;
        let high = self.high_cut_hz.clamp(20.0, sr * 0.49);
        let low_alpha = 1.0 - (-2.0 * core::f32::consts::PI * high / sr).exp();
        self.lp[channel] += low_alpha * (input - self.lp[channel]);

        let low = self.low_cut_hz.clamp(1.0, sr * 0.45);
        let high_alpha = (-2.0 * core::f32::consts::PI * low / sr).exp();
        let output = high_alpha * (self.hp_y[channel] + self.lp[channel] - self.hp_x[channel]);
        self.hp_x[channel] = self.lp[channel];
        self.hp_y[channel] = output;
        output
    }
}

const DELAY_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
        id: "time",
        name: "Time",
        kind: ParameterKind::Time {
            seconds_min: 0.001,
            seconds_max: 8.0,
            beats_min: 0.001,
            beats_max: 16.0,
        },
        unit: ParameterUnit::None,
        default: ParameterValue::Beats(0.5),
        automatable: true,
        display_hint: None,
    },
    float_parameter(
        "feedback",
        "Feedback",
        -0.98,
        0.98,
        0.35,
        ParameterUnit::Ratio,
        true,
    ),
    ParameterDescriptor {
        id: "stereo_mode",
        name: "Stereo Mode",
        kind: ParameterKind::Choice(&["linked", "offset", "ping_pong"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: None,
    },
    float_parameter(
        "stereo_offset",
        "Stereo Offset",
        -1.0,
        1.0,
        0.0,
        ParameterUnit::Ratio,
        true,
    ),
    float_parameter(
        "low_cut_hz",
        "Low Cut",
        1.0,
        20_000.0,
        20.0,
        ParameterUnit::Hertz,
        true,
    ),
    float_parameter(
        "high_cut_hz",
        "High Cut",
        20.0,
        24_000.0,
        20_000.0,
        ParameterUnit::Hertz,
        true,
    ),
    float_parameter(
        "modulation_rate_hz",
        "Modulation Rate",
        0.0,
        20.0,
        0.0,
        ParameterUnit::Hertz,
        true,
    ),
    float_parameter(
        "modulation_depth",
        "Modulation Depth",
        0.0,
        1.0,
        0.0,
        ParameterUnit::Ratio,
        true,
    ),
    float_parameter("width", "Width", 0.0, 2.0, 1.0, ParameterUnit::Ratio, true),
    float_parameter("mix", "Mix", 0.0, 1.0, 0.2, ParameterUnit::Ratio, true),
];

impl Delay {
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "time" => {
                self.time = match event.value {
                    ParameterValue::Seconds(value)
                        if value.is_finite() && (0.001..=8.0).contains(&value) =>
                    {
                        TimeValue::Seconds(value)
                    }
                    ParameterValue::Beats(value)
                        if value.is_finite() && (0.001..=16.0).contains(&value) =>
                    {
                        TimeValue::Beats(value)
                    }
                    _ => return Err(ProcessError::InvalidParameterValue),
                };
            }
            "feedback" => self.feedback = float_event(event, -0.98, 0.98)?,
            "stereo_mode" => return Err(ProcessError::InvalidParameterValue),
            "stereo_offset" => self.stereo_offset = float_event(event, -1.0, 1.0)?,
            "low_cut_hz" => self.low_cut_hz = float_event(event, 1.0, 20_000.0)?,
            "high_cut_hz" => self.high_cut_hz = float_event(event, 20.0, 24_000.0)?,
            "modulation_rate_hz" => self.modulation_rate_hz = float_event(event, 0.0, 20.0)?,
            "modulation_depth" => self.modulation_depth = float_event(event, 0.0, 1.0)?,
            "width" => self.width = float_event(event, 0.0, 2.0)?,
            "mix" => self.mix = float_event(event, 0.0, 1.0)?,
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
}

impl Processor for Delay {
    fn type_id(&self) -> &'static str {
        "gaw.delay"
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
        self.allocate(spec.sample_rate as f32);
        self.input_layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        self.tempo_bpm = spec.tempo_bpm;
        Ok(())
    }

    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        context: ProcessContext,
    ) -> Result<(), ProcessError> {
        if self.ring.is_empty() {
            return Err(ProcessError::NotPrepared);
        }
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
        self.tempo_bpm = context.tempo_bpm;
        let out_channels = output.len();
        let input_channels = input.len();
        for frame in 0..frames {
            for event in events.iter().filter(|event| event.sample_offset == frame) {
                self.apply_event(event)?;
            }
            let base = (self.time.seconds(context.tempo_bpm) * self.sample_rate)
                .clamp(1.0, self.ring[0].len() as f32 - 2.0);
            let feedback = self.feedback.clamp(-0.98, 0.98);
            let mix = self.mix.clamp(0.0, 1.0);
            let width = self.width.clamp(0.0, 2.0);
            let absolute = context.absolute_frame.saturating_add(frame as u64);
            let phase =
                absolute as f64 * self.modulation_rate_hz.max(0.0) as f64 / self.sample_rate as f64;
            let modulation = (core::f64::consts::TAU * phase).sin() as f32
                * self.modulation_depth.clamp(0.0, 1.0)
                * base.min(self.sample_rate * 0.02);
            let offset = self.stereo_offset.clamp(-1.0, 1.0) * base;
            let left_delay = (base + modulation).clamp(1.0, self.ring[0].len() as f32 - 2.0);
            let right_delay = match self.stereo_mode {
                DelayStereoMode::Linked | DelayStereoMode::PingPong => left_delay,
                DelayStereoMode::Offset => {
                    (base + offset - modulation).clamp(1.0, self.ring[0].len() as f32 - 2.0)
                }
            };
            let delayed = [self.read(0, left_delay), self.read(1, right_delay)];
            let dry = [
                input[0][frame],
                input[input_channels.saturating_sub(1).min(1)][frame],
            ];
            let feedback_source = match self.stereo_mode {
                DelayStereoMode::PingPong => [delayed[1], delayed[0]],
                _ => delayed,
            };
            for channel in 0..2 {
                let filtered = self.filter_feedback(channel, feedback_source[channel]);
                self.ring[channel][self.write] = dry[channel] + filtered * feedback;
            }
            let wet = if out_channels > 1 {
                let mid = (delayed[0] + delayed[1]) * 0.5;
                let side = (delayed[0] - delayed[1]) * 0.5 * width;
                [mid + side, mid - side]
            } else {
                [(delayed[0] + delayed[1]) * 0.5; 2]
            };
            for channel in 0..out_channels.min(2) {
                let dry_sample = dry[channel.min(input_channels - 1)];
                output[channel][frame] = dry_sample + (wet[channel] - dry_sample) * mix;
            }
            self.write = (self.write + 1) % self.ring[0].len();
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.clear();
    }

    fn seek(&mut self, _frame: u64) {
        self.clear();
    }

    fn latency_frames(&self) -> u32 {
        0
    }

    fn tail_frames(&self) -> u64 {
        let base = self.time.seconds(self.tempo_bpm) * self.sample_rate;
        let modulation = self.modulation_depth.clamp(0.0, 1.0) * base.min(self.sample_rate * 0.02);
        let offset = if self.stereo_mode == DelayStereoMode::Offset {
            self.stereo_offset.abs().clamp(0.0, 1.0) * base
        } else {
            0.0
        };
        let delay = (base + offset + modulation).min(self.sample_rate * MAX_DELAY_SECONDS);
        let feedback = self.feedback.abs().clamp(0.0, 0.98);
        let repeats = if feedback <= 0.000_1 {
            1.0
        } else {
            (-80.0_f32 / (20.0 * feedback.log10())).ceil()
        };
        (delay * repeats)
            .min(self.sample_rate * MAX_TAIL_SECONDS)
            .max(0.0) as u64
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        DELAY_PARAMETERS
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
pub enum ReverbAlgorithm {
    #[default]
    RoomV1,
    HallV1,
    PlateV1,
}

#[derive(Debug, Default)]
struct Comb {
    data: Vec<f32>,
    index: usize,
    filter: f32,
}

impl Comb {
    fn resize(&mut self, length: usize) {
        self.data.resize(length.max(1), 0.0);
        self.clear();
    }

    fn clear(&mut self) {
        self.data.fill(0.0);
        self.index = 0;
        self.filter = 0.0;
    }

    fn tick(&mut self, input: f32, feedback: f32, damping: f32) -> f32 {
        let output = self.data[self.index];
        self.filter = output * (1.0 - damping) + self.filter * damping;
        self.data[self.index] = input + self.filter * feedback;
        self.index = (self.index + 1) % self.data.len();
        output
    }
}

#[derive(Debug, Default)]
struct AllPass {
    data: Vec<f32>,
    index: usize,
}

impl AllPass {
    fn resize(&mut self, length: usize) {
        self.data.resize(length.max(1), 0.0);
        self.clear();
    }

    fn clear(&mut self) {
        self.data.fill(0.0);
        self.index = 0;
    }

    fn tick(&mut self, input: f32, feedback: f32) -> f32 {
        let delayed = self.data[self.index];
        let output = delayed - input;
        self.data[self.index] = input + delayed * feedback;
        self.index = (self.index + 1) % self.data.len();
        output
    }
}

fn default_reverb_runtime() -> Vec<ReverbChannel> {
    Vec::new()
}

#[derive(Debug, Default)]
struct ReverbChannel {
    combs: [Comb; 4],
    allpasses: [AllPass; 2],
    low_cut_x: f32,
    low_cut_y: f32,
    high_cut_y: f32,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Reverb {
    pub enabled: bool,
    pub algorithm: ReverbAlgorithm,
    pub size: f32,
    pub decay_seconds: f32,
    pub pre_delay: TimeValue,
    pub diffusion: f32,
    pub damping_hz: f32,
    pub low_cut_hz: f32,
    pub high_cut_hz: f32,
    pub width: f32,
    pub early_reflections: f32,
    pub mix: f32,
    #[serde(skip)]
    sample_rate: f32,
    #[serde(skip, default = "default_reverb_runtime")]
    channels: Vec<ReverbChannel>,
    #[serde(skip)]
    pre_delay_ring: Vec<Vec<f32>>,
    #[serde(skip)]
    pre_delay_write: usize,
    #[serde(skip)]
    input_layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
    #[serde(skip)]
    tempo_bpm: f64,
}

impl Default for Reverb {
    fn default() -> Self {
        Self {
            enabled: true,
            algorithm: ReverbAlgorithm::RoomV1,
            size: 0.5,
            decay_seconds: 1.8,
            pre_delay: TimeValue::Seconds(0.015),
            diffusion: 0.7,
            damping_hz: 7_000.0,
            low_cut_hz: 80.0,
            high_cut_hz: 16_000.0,
            width: 1.0,
            early_reflections: 0.25,
            mix: 0.2,
            sample_rate: 48_000.0,
            channels: Vec::new(),
            pre_delay_ring: Vec::new(),
            pre_delay_write: 0,
            input_layout: None,
            maximum_block_size: 0,
            tempo_bpm: 120.0,
        }
    }
}

impl Reverb {
    fn allocate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
        self.channels = (0..2).map(|_| ReverbChannel::default()).collect();
        let scale = sample_rate / 44_100.0;
        let algorithm_scale = match self.algorithm {
            ReverbAlgorithm::RoomV1 => 0.8,
            ReverbAlgorithm::HallV1 => 1.25,
            ReverbAlgorithm::PlateV1 => 0.65,
        };
        let size = 0.55 + self.size.clamp(0.0, 1.0) * 0.9;
        let comb_lengths = [1557usize, 1617, 1491, 1422];
        let allpass_lengths = [225usize, 556];
        for (channel_index, channel) in self.channels.iter_mut().enumerate() {
            for (index, comb) in channel.combs.iter_mut().enumerate() {
                let stereo_spread = channel_index * 23;
                comb.resize(
                    ((comb_lengths[index] + stereo_spread) as f32 * scale * size * algorithm_scale)
                        as usize,
                );
            }
            for (index, allpass) in channel.allpasses.iter_mut().enumerate() {
                allpass.resize((allpass_lengths[index] as f32 * scale * size) as usize);
            }
        }
        let pre_delay_len = (sample_rate * 2.0).ceil() as usize + 1;
        self.pre_delay_ring = vec![vec![0.0; pre_delay_len]; 2];
        self.clear();
    }

    fn clear(&mut self) {
        for channel in &mut self.channels {
            for comb in &mut channel.combs {
                comb.clear();
            }
            for allpass in &mut channel.allpasses {
                allpass.clear();
            }
            channel.low_cut_x = 0.0;
            channel.low_cut_y = 0.0;
            channel.high_cut_y = 0.0;
        }
        for ring in &mut self.pre_delay_ring {
            ring.fill(0.0);
        }
        self.pre_delay_write = 0;
    }
}

const REVERB_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
        id: "algorithm",
        name: "Algorithm",
        kind: ParameterKind::Choice(&["room_v1", "hall_v1", "plate_v1"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: None,
    },
    float_parameter("size", "Size", 0.0, 1.0, 0.5, ParameterUnit::Ratio, false),
    float_parameter(
        "decay_seconds",
        "Decay",
        0.05,
        30.0,
        1.8,
        ParameterUnit::Seconds,
        true,
    ),
    ParameterDescriptor {
        id: "pre_delay",
        name: "Pre-delay",
        kind: ParameterKind::Time {
            seconds_min: 0.0,
            seconds_max: 2.0,
            beats_min: 0.0,
            beats_max: 4.0,
        },
        unit: ParameterUnit::None,
        default: ParameterValue::Seconds(0.015),
        automatable: true,
        display_hint: None,
    },
    float_parameter(
        "diffusion",
        "Diffusion",
        0.0,
        1.0,
        0.7,
        ParameterUnit::Ratio,
        true,
    ),
    float_parameter(
        "damping_hz",
        "Damping",
        100.0,
        24_000.0,
        7_000.0,
        ParameterUnit::Hertz,
        true,
    ),
    float_parameter(
        "low_cut_hz",
        "Low Cut",
        1.0,
        20_000.0,
        80.0,
        ParameterUnit::Hertz,
        true,
    ),
    float_parameter(
        "high_cut_hz",
        "High Cut",
        100.0,
        24_000.0,
        16_000.0,
        ParameterUnit::Hertz,
        true,
    ),
    float_parameter("width", "Width", 0.0, 2.0, 1.0, ParameterUnit::Ratio, true),
    float_parameter(
        "early_reflections",
        "Early Reflections",
        0.0,
        1.0,
        0.25,
        ParameterUnit::Ratio,
        true,
    ),
    float_parameter("mix", "Mix", 0.0, 1.0, 0.2, ParameterUnit::Ratio, true),
];

impl Reverb {
    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match event.id.as_str() {
            "algorithm" | "size" => {
                return Err(ProcessError::InvalidParameterValue);
            }
            "decay_seconds" => self.decay_seconds = float_event(event, 0.05, 30.0)?,
            "pre_delay" => {
                self.pre_delay = match event.value {
                    ParameterValue::Seconds(value)
                        if value.is_finite() && (0.0..=2.0).contains(&value) =>
                    {
                        TimeValue::Seconds(value)
                    }
                    ParameterValue::Beats(value)
                        if value.is_finite() && (0.0..=4.0).contains(&value) =>
                    {
                        TimeValue::Beats(value)
                    }
                    _ => return Err(ProcessError::InvalidParameterValue),
                };
            }
            "diffusion" => self.diffusion = float_event(event, 0.0, 1.0)?,
            "damping_hz" => self.damping_hz = float_event(event, 100.0, 24_000.0)?,
            "low_cut_hz" => self.low_cut_hz = float_event(event, 1.0, 20_000.0)?,
            "high_cut_hz" => self.high_cut_hz = float_event(event, 100.0, 24_000.0)?,
            "width" => self.width = float_event(event, 0.0, 2.0)?,
            "early_reflections" => self.early_reflections = float_event(event, 0.0, 1.0)?,
            "mix" => self.mix = float_event(event, 0.0, 1.0)?,
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
    }
}

impl Processor for Reverb {
    fn type_id(&self) -> &'static str {
        "gaw.reverb"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, _input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(AudioLayout::Stereo)
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.allocate(spec.sample_rate as f32);
        self.input_layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        self.tempo_bpm = spec.tempo_bpm;
        Ok(())
    }

    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        context: ProcessContext,
    ) -> Result<(), ProcessError> {
        if self.channels.is_empty() {
            return Err(ProcessError::NotPrepared);
        }
        let input_layout = self.input_layout.ok_or(ProcessError::NotPrepared)?;
        let frames = validate_process_io(
            input,
            output,
            input_layout,
            AudioLayout::Stereo,
            self.maximum_block_size,
            events,
        )?;
        if !self.enabled {
            copy_or_map_bypass(input, output);
            return Ok(());
        }
        self.tempo_bpm = context.tempo_bpm;
        let input_channels = input.len();
        let output_channels = output.len();
        for frame in 0..frames {
            for event in events.iter().filter(|event| event.sample_offset == frame) {
                self.apply_event(event)?;
            }
            let pre_delay = (self.pre_delay.seconds(context.tempo_bpm) * self.sample_rate)
                .clamp(0.0, self.pre_delay_ring[0].len() as f32 - 1.0)
                as usize;
            let damping = (-2.0
                * core::f32::consts::PI
                * self.damping_hz.clamp(100.0, self.sample_rate * 0.49)
                / self.sample_rate)
                .exp();
            let diffusion = self.diffusion.clamp(0.0, 1.0) * 0.45 + 0.3;
            let early = self.early_reflections.clamp(0.0, 1.0);
            let width = self.width.clamp(0.0, 2.0);
            let high_alpha = 1.0
                - (-2.0
                    * core::f32::consts::PI
                    * self.high_cut_hz.clamp(100.0, self.sample_rate * 0.49)
                    / self.sample_rate)
                    .exp();
            let low_alpha = (-2.0
                * core::f32::consts::PI
                * self.low_cut_hz.clamp(1.0, self.sample_rate * 0.45)
                / self.sample_rate)
                .exp();
            let decay = self.decay_seconds.clamp(0.05, MAX_TAIL_SECONDS);
            let mix = self.mix.clamp(0.0, 1.0);
            let dry = [
                input[0][frame],
                input[input_channels.saturating_sub(1).min(1)][frame],
            ];
            let read = (self.pre_delay_write + self.pre_delay_ring[0].len() - pre_delay)
                % self.pre_delay_ring[0].len();
            let mut wet = [0.0; 2];
            for channel_index in 0..2 {
                self.pre_delay_ring[channel_index][self.pre_delay_write] = dry[channel_index];
                let predelayed = self.pre_delay_ring[channel_index][read];
                let channel = &mut self.channels[channel_index];
                let mut sum = 0.0;
                for comb in &mut channel.combs {
                    let loop_seconds = comb.data.len() as f32 / self.sample_rate;
                    let comb_feedback = 10.0_f32.powf(-3.0 * loop_seconds / decay).clamp(0.0, 0.97);
                    sum += comb.tick(predelayed * 0.22, comb_feedback, damping);
                }
                sum *= 0.25;
                for allpass in &mut channel.allpasses {
                    sum = allpass.tick(sum, diffusion);
                }
                channel.low_cut_y = low_alpha * (channel.low_cut_y + sum - channel.low_cut_x);
                channel.low_cut_x = sum;
                let bandpassed = channel.low_cut_y;
                channel.high_cut_y += high_alpha * (bandpassed - channel.high_cut_y);
                let late = channel.high_cut_y;
                wet[channel_index] = predelayed * early + late * (1.0 - early * 0.5);
            }
            self.pre_delay_write = (self.pre_delay_write + 1) % self.pre_delay_ring[0].len();
            let mid = (wet[0] + wet[1]) * 0.5;
            let side = (wet[0] - wet[1]) * 0.5 * width;
            wet = [mid + side, mid - side];
            for channel in 0..output_channels.min(2) {
                let dry_sample = dry[channel.min(input_channels - 1)];
                output[channel][frame] = dry_sample + (wet[channel] - dry_sample) * mix;
            }
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.clear();
    }

    fn seek(&mut self, _frame: u64) {
        self.clear();
    }

    fn latency_frames(&self) -> u32 {
        0
    }

    fn tail_frames(&self) -> u64 {
        let seconds = self.pre_delay.seconds(self.tempo_bpm)
            + self.decay_seconds.clamp(0.0, MAX_TAIL_SECONDS);
        (seconds.min(MAX_TAIL_SECONDS) * self.sample_rate) as u64
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        REVERB_PARAMETERS
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

    fn spec(layout: AudioLayout, max_block_size: usize) -> PrepareSpec {
        PrepareSpec {
            sample_rate: 1_000.0,
            max_block_size,
            input_layout: layout,
            tempo_bpm: 120.0,
        }
    }

    #[test]
    fn delay_produces_a_bounded_echo_and_stereo_from_mono() {
        let mut delay = Delay {
            time: TimeValue::Seconds(0.004),
            feedback: 0.0,
            mix: 1.0,
            ..Delay::default()
        };
        delay.prepare(spec(AudioLayout::Mono, 16)).unwrap();
        let mut source = [0.0; 16];
        source[0] = 1.0;
        let mut left = [0.0; 16];
        let mut right = [0.0; 16];
        delay
            .process(
                &[&source],
                &mut [&mut left, &mut right],
                &[],
                ProcessContext {
                    absolute_frame: 0,
                    tempo_bpm: 120.0,
                },
            )
            .unwrap();
        assert!(left[4] > 0.99 && right[4] > 0.99);
        assert!(delay.tail_frames() <= 30_000);
    }

    #[test]
    fn reverb_reset_is_repeatable_and_tail_is_finite() {
        let mut reverb = Reverb {
            mix: 1.0,
            ..Reverb::default()
        };
        reverb.prepare(spec(AudioLayout::Mono, 64)).unwrap();
        let mut source = [0.0; 64];
        source[0] = 1.0;
        let mut a_left = [0.0; 64];
        let mut a_right = [0.0; 64];
        reverb
            .process(
                &[&source],
                &mut [&mut a_left, &mut a_right],
                &[],
                ProcessContext {
                    absolute_frame: 0,
                    tempo_bpm: 120.0,
                },
            )
            .unwrap();
        reverb.reset();
        let mut b_left = [0.0; 64];
        let mut b_right = [0.0; 64];
        reverb
            .process(
                &[&source],
                &mut [&mut b_left, &mut b_right],
                &[],
                ProcessContext {
                    absolute_frame: 0,
                    tempo_bpm: 120.0,
                },
            )
            .unwrap();
        assert_eq!(a_left, b_left);
        assert_eq!(a_right, b_right);
        assert!(reverb.tail_frames() <= 30_000);
    }

    #[test]
    fn time_contracts_are_complete_and_tail_uses_stereo_modulation_and_predelay() {
        let delay = Delay {
            time: TimeValue::Seconds(1.0),
            feedback: 0.0,
            stereo_mode: DelayStereoMode::Offset,
            stereo_offset: 0.5,
            modulation_depth: 1.0,
            sample_rate: 1_000.0,
            ..Delay::default()
        };
        let delay_ids: Vec<_> = delay
            .parameters()
            .iter()
            .map(|parameter| parameter.id)
            .collect();
        assert!(delay_ids.contains(&"time"));
        assert!(delay_ids.contains(&"stereo_mode"));
        assert!(delay_ids.contains(&"high_cut_hz"));
        assert_eq!(delay.tail_frames(), 1_520);

        let reverb = Reverb {
            decay_seconds: 1.0,
            pre_delay: TimeValue::Seconds(0.5),
            sample_rate: 1_000.0,
            ..Reverb::default()
        };
        let reverb_ids: Vec<_> = reverb
            .parameters()
            .iter()
            .map(|parameter| parameter.id)
            .collect();
        assert!(reverb_ids.contains(&"algorithm"));
        assert!(reverb_ids.contains(&"pre_delay"));
        assert!(reverb_ids.contains(&"early_reflections"));
        assert_eq!(reverb.tail_frames(), 1_500);
    }
}
