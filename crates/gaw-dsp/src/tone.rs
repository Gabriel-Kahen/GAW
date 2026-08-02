//! Resonant filtering and minimum-phase parametric equalization.

use crate::contract::{
    AudioLayout, MONO_AND_STEREO, PrepareSpec, ProcessContext, ProcessError, Processor,
    copy_or_map_bypass, validate_process_io,
};
use crate::kernel::{Biquad, BiquadCoefficients, db_to_gain};
use crate::parameter::{
    ParameterDescriptor, ParameterEvent, ParameterKind, ParameterUnit, ParameterValue,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterMode {
    #[default]
    LowPass,
    HighPass,
    BandPass,
    Notch,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Filter {
    pub enabled: bool,
    pub mode: FilterMode,
    pub cutoff_hz: f32,
    pub resonance_q: f32,
    pub slope_db_per_octave: u32,
    pub drive_db: f32,
    #[serde(skip)]
    sample_rate: f64,
    #[serde(skip)]
    layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
    #[serde(skip)]
    stages: [[Biquad; 4]; 2],
}
impl Default for Filter {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: FilterMode::LowPass,
            cutoff_hz: 1_000.0,
            resonance_q: 0.707,
            slope_db_per_octave: 12,
            drive_db: 0.0,
            sample_rate: 48_000.0,
            layout: None,
            maximum_block_size: 0,
            stages: [[Biquad::default(); 4]; 2],
        }
    }
}
const FILTER_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
        id: "mode",
        name: "Mode",
        kind: ParameterKind::Choice(&["low_pass", "high_pass", "band_pass", "notch"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "cutoff_hz",
        name: "Cutoff",
        kind: ParameterKind::Float {
            min: 10.0,
            max: 24_000.0,
        },
        unit: ParameterUnit::Hertz,
        default: ParameterValue::Float(1_000.0),
        automatable: true,
        display_hint: Some("logarithmic"),
    },
    ParameterDescriptor {
        id: "resonance_q",
        name: "Resonance",
        kind: ParameterKind::Float {
            min: 0.1,
            max: 24.0,
        },
        unit: ParameterUnit::Ratio,
        default: ParameterValue::Float(0.707),
        automatable: true,
        display_hint: Some("logarithmic"),
    },
    ParameterDescriptor {
        id: "slope_db_per_octave",
        name: "Slope",
        kind: ParameterKind::Choice(&["12", "24", "48"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(0),
        automatable: false,
        display_hint: None,
    },
    ParameterDescriptor {
        id: "drive_db",
        name: "Drive",
        kind: ParameterKind::Float {
            min: 0.0,
            max: 36.0,
        },
        unit: ParameterUnit::Decibels,
        default: ParameterValue::Float(0.0),
        automatable: true,
        display_hint: None,
    },
];
impl Filter {
    fn count(&self) -> usize {
        match self.slope_db_per_octave {
            0..=12 => 1,
            13..=24 => 2,
            _ => 4,
        }
    }
    fn update(&mut self) {
        let coeff = match self.mode {
            FilterMode::LowPass => {
                BiquadCoefficients::low_pass(self.sample_rate, self.cutoff_hz, self.resonance_q)
            }
            FilterMode::HighPass => {
                BiquadCoefficients::high_pass(self.sample_rate, self.cutoff_hz, self.resonance_q)
            }
            FilterMode::BandPass => {
                BiquadCoefficients::band_pass(self.sample_rate, self.cutoff_hz, self.resonance_q)
            }
            FilterMode::Notch => {
                BiquadCoefficients::notch(self.sample_rate, self.cutoff_hz, self.resonance_q)
            }
        };
        for channel in &mut self.stages {
            for stage in channel {
                stage.set_coefficients(coeff);
            }
        }
    }
    fn event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        match (event.id.as_str(), event.value) {
            ("mode", ParameterValue::Choice(v)) => {
                self.mode = match v {
                    0 => FilterMode::LowPass,
                    1 => FilterMode::HighPass,
                    2 => FilterMode::BandPass,
                    _ => FilterMode::Notch,
                }
            }
            ("cutoff_hz", ParameterValue::Float(v)) => {
                self.cutoff_hz = v.clamp(10.0, self.sample_rate as f32 * 0.499);
            }
            ("resonance_q", ParameterValue::Float(v)) => self.resonance_q = v.clamp(0.1, 24.0),
            ("slope_db_per_octave", ParameterValue::Choice(v)) => {
                self.slope_db_per_octave = match v {
                    0 => 12,
                    1 => 24,
                    _ => 48,
                }
            }
            ("drive_db", ParameterValue::Float(v)) => self.drive_db = v.clamp(0.0, 36.0),
            (id, _) if FILTER_PARAMETERS.iter().any(|p| p.id == id) => {
                return Err(ProcessError::InvalidParameterValue(event.id.clone()));
            }
            _ => return Err(ProcessError::UnknownParameter(event.id.clone())),
        }
        self.update();
        Ok(())
    }
}
impl Processor for Filter {
    fn type_id(&self) -> &'static str {
        "gaw.filter"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(input)
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.sample_rate = spec.sample_rate;
        self.layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        self.update();
        self.reset();
        Ok(())
    }
    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        _: ProcessContext,
    ) -> Result<(), ProcessError> {
        let layout = self.layout.ok_or(ProcessError::NotPrepared)?;
        let frames = validate_process_io(
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
        let mut next = 0;
        for frame in 0..frames {
            while next < events.len() && events[next].sample_offset == frame {
                self.event(&events[next])?;
                next += 1;
            }
            let drive = db_to_gain(self.drive_db);
            let normalization = drive.tanh().max(1.0e-6);
            let count = self.count();
            for channel in 0..input.len() {
                let mut sample = (input[channel][frame] * drive).tanh() / normalization;
                for stage in self.stages[channel].iter_mut().take(count) {
                    sample = stage.process(sample);
                }
                output[channel][frame] = sample;
            }
        }
        Ok(())
    }
    fn reset(&mut self) {
        for channel in &mut self.stages {
            for stage in channel {
                stage.reset();
            }
        }
    }
    fn seek(&mut self, _: u64) {
        self.reset();
    }
    fn latency_frames(&self) -> u32 {
        0
    }
    fn tail_frames(&self) -> u64 {
        0
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        FILTER_PARAMETERS
    }
    fn enabled(&self) -> bool {
        self.enabled
    }
    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EqShape {
    LowShelf,
    HighShelf,
    #[default]
    Bell,
    LowPass,
    HighPass,
    Notch,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct EqBand {
    pub enabled: bool,
    pub shape: EqShape,
    pub frequency_hz: f32,
    pub gain_db: f32,
    pub q: f32,
    pub slope_db_per_octave: f32,
}
impl Default for EqBand {
    fn default() -> Self {
        Self {
            enabled: true,
            shape: EqShape::Bell,
            frequency_hz: 1_000.0,
            gain_db: 0.0,
            q: 0.707,
            slope_db_per_octave: 12.0,
        }
    }
}
#[derive(Debug, Default)]
struct EqRuntime {
    filters: [Biquad; 2],
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ParametricEq {
    pub enabled: bool,
    pub bands: Vec<EqBand>,
    pub output_gain_db: f32,
    #[serde(skip)]
    sample_rate: f64,
    #[serde(skip)]
    layout: Option<AudioLayout>,
    #[serde(skip)]
    maximum_block_size: usize,
    #[serde(skip)]
    runtime: Vec<EqRuntime>,
}
impl Default for ParametricEq {
    fn default() -> Self {
        Self {
            enabled: true,
            bands: vec![EqBand::default()],
            output_gain_db: 0.0,
            sample_rate: 48_000.0,
            layout: None,
            maximum_block_size: 0,
            runtime: Vec::new(),
        }
    }
}
const EQ_PARAMETERS: &[ParameterDescriptor] = &[ParameterDescriptor {
    id: "output_gain_db",
    name: "Output Gain",
    kind: ParameterKind::Float {
        min: -36.0,
        max: 36.0,
    },
    unit: ParameterUnit::Decibels,
    default: ParameterValue::Float(0.0),
    automatable: true,
    display_hint: None,
}];
impl ParametricEq {
    fn coefficients(sample_rate: f64, band: &EqBand) -> BiquadCoefficients {
        match band.shape {
            EqShape::LowShelf => BiquadCoefficients::low_shelf(
                sample_rate,
                band.frequency_hz,
                band.slope_db_per_octave / 12.0,
                band.gain_db,
            ),
            EqShape::HighShelf => BiquadCoefficients::high_shelf(
                sample_rate,
                band.frequency_hz,
                band.slope_db_per_octave / 12.0,
                band.gain_db,
            ),
            EqShape::Bell => {
                BiquadCoefficients::peaking(sample_rate, band.frequency_hz, band.q, band.gain_db)
            }
            EqShape::LowPass => {
                BiquadCoefficients::low_pass(sample_rate, band.frequency_hz, band.q)
            }
            EqShape::HighPass => {
                BiquadCoefficients::high_pass(sample_rate, band.frequency_hz, band.q)
            }
            EqShape::Notch => BiquadCoefficients::notch(sample_rate, band.frequency_hz, band.q),
        }
    }
    fn update(&mut self) {
        for (index, band) in self.bands.iter().take(8).enumerate() {
            let coefficient = Self::coefficients(self.sample_rate, band);
            for filter in &mut self.runtime[index].filters {
                filter.set_coefficients(coefficient);
            }
        }
    }
}
impl Processor for ParametricEq {
    fn type_id(&self) -> &'static str {
        "gaw.parametric_eq"
    }
    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }
    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(input)
    }
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.bands.truncate(8);
        self.sample_rate = spec.sample_rate;
        self.layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        self.runtime
            .resize_with(self.bands.len(), EqRuntime::default);
        self.update();
        self.reset();
        Ok(())
    }
    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        _: ProcessContext,
    ) -> Result<(), ProcessError> {
        let layout = self.layout.ok_or(ProcessError::NotPrepared)?;
        let frames = validate_process_io(
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
        let mut next = 0;
        for frame in 0..frames {
            while next < events.len() && events[next].sample_offset == frame {
                if events[next].id == "output_gain_db" {
                    if let ParameterValue::Float(v) = events[next].value {
                        self.output_gain_db = v.clamp(-36.0, 36.0);
                    } else {
                        return Err(ProcessError::InvalidParameterValue(events[next].id.clone()));
                    }
                } else {
                    return Err(ProcessError::UnknownParameter(events[next].id.clone()));
                }
                next += 1;
            }
            let gain = db_to_gain(self.output_gain_db);
            for channel in 0..input.len() {
                let mut sample = input[channel][frame];
                for (index, band) in self.bands.iter().enumerate() {
                    if band.enabled {
                        sample = self.runtime[index].filters[channel].process(sample);
                    }
                }
                output[channel][frame] = sample * gain;
            }
        }
        Ok(())
    }
    fn reset(&mut self) {
        for runtime in &mut self.runtime {
            for filter in &mut runtime.filters {
                filter.reset();
            }
        }
    }
    fn seek(&mut self, _: u64) {
        self.reset();
    }
    fn latency_frames(&self) -> u32 {
        0
    }
    fn tail_frames(&self) -> u64 {
        0
    }
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        EQ_PARAMETERS
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
    #[test]
    fn filter_is_finite_and_reset_deterministic() {
        let mut filter = Filter::default();
        let spec = PrepareSpec {
            input_layout: AudioLayout::Mono,
            ..Default::default()
        };
        filter.prepare(spec).unwrap();
        let input = [1.0; 64];
        let mut first = [0.0; 64];
        filter
            .process(&[&input], &mut [&mut first], &[], ProcessContext::default())
            .unwrap();
        filter.reset();
        let mut second = [0.0; 64];
        filter
            .process(
                &[&input],
                &mut [&mut second],
                &[],
                ProcessContext::default(),
            )
            .unwrap();
        assert_eq!(first, second);
        assert!(first.iter().all(|x| x.is_finite()));
    }
}
