//! Resonant filtering and minimum-phase parametric equalization.

use crate::contract::{
    AudioLayout, MONO_AND_STEREO, PrepareSpec, ProcessContext, ProcessError, Processor,
    copy_or_map_bypass, validate_process_io,
};
use crate::kernel::{Biquad, BiquadCoefficients, LinearSmoother, db_to_gain};
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
            ("mode" | "slope_db_per_octave", _) => {
                return Err(ProcessError::InvalidParameterValue);
            }
            ("cutoff_hz", ParameterValue::Float(value))
                if value.is_finite() && (10.0..=24_000.0).contains(&value) =>
            {
                self.cutoff_hz = value.min(self.sample_rate as f32 * 0.499);
            }
            ("resonance_q", ParameterValue::Float(value))
                if value.is_finite() && (0.1..=24.0).contains(&value) =>
            {
                self.resonance_q = value;
            }
            ("drive_db", ParameterValue::Float(value))
                if value.is_finite() && (0.0..=36.0).contains(&value) =>
            {
                self.drive_db = value;
            }
            (id, _) if FILTER_PARAMETERS.iter().any(|p| p.id == id) => {
                return Err(ProcessError::InvalidParameterValue);
            }
            _ => return Err(ProcessError::UnknownParameter),
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
    /// Stable identity used by editors and serialized automation lanes.
    #[serde(default)]
    pub id: String,
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
            id: String::new(),
            enabled: true,
            shape: EqShape::Bell,
            frequency_hz: 1_000.0,
            gain_db: 0.0,
            q: 0.707,
            slope_db_per_octave: 12.0,
        }
    }
}
#[derive(Debug)]
struct EqRuntime {
    filters: [[Biquad; 4]; 2],
    frequency_hz: LinearSmoother,
    gain_db: LinearSmoother,
    q: LinearSmoother,
}
impl EqRuntime {
    fn new(band: &EqBand, sample_rate: f64) -> Self {
        Self {
            filters: [[Biquad::default(); 4]; 2],
            frequency_hz: LinearSmoother::new(band.frequency_hz.ln(), sample_rate, 10.0),
            gain_db: LinearSmoother::new(band.gain_db, sample_rate, 10.0),
            q: LinearSmoother::new(band.q.ln(), sample_rate, 10.0),
        }
    }

    fn reset_parameters(&mut self, band: &EqBand) {
        self.frequency_hz.jump_to(band.frequency_hz.ln());
        self.gain_db.jump_to(band.gain_db);
        self.q.jump_to(band.q.ln());
    }

    fn advance_parameters(&mut self) -> Option<(f32, f32, f32)> {
        let prior = (
            self.frequency_hz.current(),
            self.gain_db.current(),
            self.q.current(),
        );
        let next = (self.frequency_hz.next(), self.gain_db.next(), self.q.next());
        (prior.0.to_bits() != next.0.to_bits()
            || prior.1.to_bits() != next.1.to_bits()
            || prior.2.to_bits() != next.2.to_bits())
        .then(|| (next.0.exp(), next.1, next.2.exp()))
    }
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
    #[serde(skip)]
    output_gain: LinearSmoother,
}
impl Default for ParametricEq {
    fn default() -> Self {
        let band = EqBand {
            id: "band-0".into(),
            ..EqBand::default()
        };
        Self {
            enabled: true,
            bands: vec![band],
            output_gain_db: 0.0,
            sample_rate: 48_000.0,
            layout: None,
            maximum_block_size: 0,
            runtime: Vec::new(),
            output_gain: LinearSmoother::default(),
        }
    }
}

const fn eq_bool(id: &'static str, name: &'static str) -> ParameterDescriptor {
    ParameterDescriptor {
        id,
        name,
        kind: ParameterKind::Boolean,
        unit: ParameterUnit::None,
        default: ParameterValue::Bool(true),
        automatable: false,
        display_hint: None,
    }
}

const fn eq_shape(id: &'static str, name: &'static str) -> ParameterDescriptor {
    ParameterDescriptor {
        id,
        name,
        kind: ParameterKind::Choice(&[
            "low_shelf",
            "high_shelf",
            "bell",
            "low_pass",
            "high_pass",
            "notch",
        ]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(2),
        automatable: false,
        display_hint: None,
    }
}

const fn eq_float(
    id: &'static str,
    name: &'static str,
    min: f32,
    max: f32,
    default: f32,
    unit: ParameterUnit,
    display_hint: Option<&'static str>,
) -> ParameterDescriptor {
    ParameterDescriptor {
        id,
        name,
        kind: ParameterKind::Float { min, max },
        unit,
        default: ParameterValue::Float(default),
        automatable: true,
        display_hint,
    }
}

const fn eq_slope(id: &'static str, name: &'static str) -> ParameterDescriptor {
    ParameterDescriptor {
        id,
        name,
        kind: ParameterKind::Choice(&["6", "12", "24", "48"]),
        unit: ParameterUnit::None,
        default: ParameterValue::Choice(1),
        automatable: false,
        display_hint: None,
    }
}

const EQ_PARAMETERS: &[ParameterDescriptor] = &[
    ParameterDescriptor {
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
    },
    eq_bool("bands.0.enabled", "Band Enabled"),
    eq_shape("bands.0.shape", "Band Shape"),
    eq_float(
        "bands.0.frequency_hz",
        "Band Frequency",
        10.0,
        24_000.0,
        1_000.0,
        ParameterUnit::Hertz,
        Some("logarithmic"),
    ),
    eq_float(
        "bands.0.gain_db",
        "Band Gain",
        -36.0,
        36.0,
        0.0,
        ParameterUnit::Decibels,
        None,
    ),
    eq_float(
        "bands.0.q",
        "Band Q",
        0.1,
        24.0,
        0.707,
        ParameterUnit::Ratio,
        Some("logarithmic"),
    ),
    eq_slope("bands.0.slope_db_per_octave", "Band Slope"),
    eq_bool("bands.1.enabled", "Band Enabled"),
    eq_shape("bands.1.shape", "Band Shape"),
    eq_float(
        "bands.1.frequency_hz",
        "Band Frequency",
        10.0,
        24_000.0,
        1_000.0,
        ParameterUnit::Hertz,
        Some("logarithmic"),
    ),
    eq_float(
        "bands.1.gain_db",
        "Band Gain",
        -36.0,
        36.0,
        0.0,
        ParameterUnit::Decibels,
        None,
    ),
    eq_float(
        "bands.1.q",
        "Band Q",
        0.1,
        24.0,
        0.707,
        ParameterUnit::Ratio,
        Some("logarithmic"),
    ),
    eq_slope("bands.1.slope_db_per_octave", "Band Slope"),
    eq_bool("bands.2.enabled", "Band Enabled"),
    eq_shape("bands.2.shape", "Band Shape"),
    eq_float(
        "bands.2.frequency_hz",
        "Band Frequency",
        10.0,
        24_000.0,
        1_000.0,
        ParameterUnit::Hertz,
        Some("logarithmic"),
    ),
    eq_float(
        "bands.2.gain_db",
        "Band Gain",
        -36.0,
        36.0,
        0.0,
        ParameterUnit::Decibels,
        None,
    ),
    eq_float(
        "bands.2.q",
        "Band Q",
        0.1,
        24.0,
        0.707,
        ParameterUnit::Ratio,
        Some("logarithmic"),
    ),
    eq_slope("bands.2.slope_db_per_octave", "Band Slope"),
    eq_bool("bands.3.enabled", "Band Enabled"),
    eq_shape("bands.3.shape", "Band Shape"),
    eq_float(
        "bands.3.frequency_hz",
        "Band Frequency",
        10.0,
        24_000.0,
        1_000.0,
        ParameterUnit::Hertz,
        Some("logarithmic"),
    ),
    eq_float(
        "bands.3.gain_db",
        "Band Gain",
        -36.0,
        36.0,
        0.0,
        ParameterUnit::Decibels,
        None,
    ),
    eq_float(
        "bands.3.q",
        "Band Q",
        0.1,
        24.0,
        0.707,
        ParameterUnit::Ratio,
        Some("logarithmic"),
    ),
    eq_slope("bands.3.slope_db_per_octave", "Band Slope"),
    eq_bool("bands.4.enabled", "Band Enabled"),
    eq_shape("bands.4.shape", "Band Shape"),
    eq_float(
        "bands.4.frequency_hz",
        "Band Frequency",
        10.0,
        24_000.0,
        1_000.0,
        ParameterUnit::Hertz,
        Some("logarithmic"),
    ),
    eq_float(
        "bands.4.gain_db",
        "Band Gain",
        -36.0,
        36.0,
        0.0,
        ParameterUnit::Decibels,
        None,
    ),
    eq_float(
        "bands.4.q",
        "Band Q",
        0.1,
        24.0,
        0.707,
        ParameterUnit::Ratio,
        Some("logarithmic"),
    ),
    eq_slope("bands.4.slope_db_per_octave", "Band Slope"),
    eq_bool("bands.5.enabled", "Band Enabled"),
    eq_shape("bands.5.shape", "Band Shape"),
    eq_float(
        "bands.5.frequency_hz",
        "Band Frequency",
        10.0,
        24_000.0,
        1_000.0,
        ParameterUnit::Hertz,
        Some("logarithmic"),
    ),
    eq_float(
        "bands.5.gain_db",
        "Band Gain",
        -36.0,
        36.0,
        0.0,
        ParameterUnit::Decibels,
        None,
    ),
    eq_float(
        "bands.5.q",
        "Band Q",
        0.1,
        24.0,
        0.707,
        ParameterUnit::Ratio,
        Some("logarithmic"),
    ),
    eq_slope("bands.5.slope_db_per_octave", "Band Slope"),
    eq_bool("bands.6.enabled", "Band Enabled"),
    eq_shape("bands.6.shape", "Band Shape"),
    eq_float(
        "bands.6.frequency_hz",
        "Band Frequency",
        10.0,
        24_000.0,
        1_000.0,
        ParameterUnit::Hertz,
        Some("logarithmic"),
    ),
    eq_float(
        "bands.6.gain_db",
        "Band Gain",
        -36.0,
        36.0,
        0.0,
        ParameterUnit::Decibels,
        None,
    ),
    eq_float(
        "bands.6.q",
        "Band Q",
        0.1,
        24.0,
        0.707,
        ParameterUnit::Ratio,
        Some("logarithmic"),
    ),
    eq_slope("bands.6.slope_db_per_octave", "Band Slope"),
    eq_bool("bands.7.enabled", "Band Enabled"),
    eq_shape("bands.7.shape", "Band Shape"),
    eq_float(
        "bands.7.frequency_hz",
        "Band Frequency",
        10.0,
        24_000.0,
        1_000.0,
        ParameterUnit::Hertz,
        Some("logarithmic"),
    ),
    eq_float(
        "bands.7.gain_db",
        "Band Gain",
        -36.0,
        36.0,
        0.0,
        ParameterUnit::Decibels,
        None,
    ),
    eq_float(
        "bands.7.q",
        "Band Q",
        0.1,
        24.0,
        0.707,
        ParameterUnit::Ratio,
        Some("logarithmic"),
    ),
    eq_slope("bands.7.slope_db_per_octave", "Band Slope"),
];
impl ParametricEq {
    fn coefficients(
        sample_rate: f64,
        shape: EqShape,
        slope_db_per_octave: f32,
        frequency_hz: f32,
        gain_db: f32,
        q: f32,
    ) -> BiquadCoefficients {
        match shape {
            EqShape::LowShelf => BiquadCoefficients::low_shelf(
                sample_rate,
                frequency_hz,
                slope_db_per_octave / 12.0,
                gain_db,
            ),
            EqShape::HighShelf => BiquadCoefficients::high_shelf(
                sample_rate,
                frequency_hz,
                slope_db_per_octave / 12.0,
                gain_db,
            ),
            EqShape::Bell => BiquadCoefficients::peaking(sample_rate, frequency_hz, q, gain_db),
            EqShape::LowPass => BiquadCoefficients::low_pass(sample_rate, frequency_hz, q),
            EqShape::HighPass => BiquadCoefficients::high_pass(sample_rate, frequency_hz, q),
            EqShape::Notch => BiquadCoefficients::notch(sample_rate, frequency_hz, q),
        }
    }
    fn update(&mut self) {
        for (index, band) in self.bands.iter().take(8).enumerate() {
            let coefficient = Self::coefficients(
                self.sample_rate,
                band.shape,
                band.slope_db_per_octave,
                self.runtime[index].frequency_hz.current().exp(),
                self.runtime[index].gain_db.current(),
                self.runtime[index].q.current().exp(),
            );
            for channel in &mut self.runtime[index].filters {
                for filter in channel {
                    filter.set_coefficients(coefficient);
                }
            }
        }
    }

    fn band_stage_count(band: &EqBand) -> usize {
        match band.shape {
            EqShape::LowPass | EqShape::HighPass => {
                if band.slope_db_per_octave <= 12.0 {
                    1
                } else if band.slope_db_per_octave <= 24.0 {
                    2
                } else {
                    4
                }
            }
            _ => 1,
        }
    }

    fn apply_event(&mut self, event: &ParameterEvent) -> Result<(), ProcessError> {
        if event.id == "output_gain_db" {
            let ParameterValue::Float(value) = event.value else {
                return Err(ProcessError::InvalidParameterValue);
            };
            if !value.is_finite() || !(-36.0..=36.0).contains(&value) {
                return Err(ProcessError::InvalidParameterValue);
            }
            self.output_gain_db = value;
            self.output_gain.set_target(db_to_gain(value));
            return Ok(());
        }

        let Some(rest) = event.id.strip_prefix("bands.") else {
            return Err(ProcessError::UnknownParameter);
        };
        let Some((index, field)) = rest.split_once('.') else {
            return Err(ProcessError::UnknownParameter);
        };
        let Ok(index) = index.parse::<usize>() else {
            return Err(ProcessError::UnknownParameter);
        };
        if index >= self.bands.len() || index >= 8 {
            return Err(ProcessError::InvalidParameterValue);
        }
        let ParameterValue::Float(value) = event.value else {
            return Err(ProcessError::InvalidParameterValue);
        };
        match field {
            "frequency_hz" if value.is_finite() && (10.0..=24_000.0).contains(&value) => {
                self.bands[index].frequency_hz = value;
                self.runtime[index].frequency_hz.set_target(value.ln());
            }
            "gain_db" if value.is_finite() && (-36.0..=36.0).contains(&value) => {
                self.bands[index].gain_db = value;
                self.runtime[index].gain_db.set_target(value);
            }
            "q" if value.is_finite() && (0.1..=24.0).contains(&value) => {
                self.bands[index].q = value;
                self.runtime[index].q.set_target(value.ln());
            }
            "enabled" | "shape" | "slope_db_per_octave" | "frequency_hz" | "gain_db" | "q" => {
                return Err(ProcessError::InvalidParameterValue);
            }
            _ => return Err(ProcessError::UnknownParameter),
        }
        Ok(())
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
        if self.bands.len() > 8 {
            return Err(ProcessError::InvalidParameterValue);
        }
        for (index, band) in self.bands.iter_mut().enumerate() {
            if band.id.is_empty() {
                band.id = format!("band-{index}");
            }
            if !band.frequency_hz.is_finite()
                || !(10.0..=24_000.0).contains(&band.frequency_hz)
                || !band.gain_db.is_finite()
                || !(-36.0..=36.0).contains(&band.gain_db)
                || !band.q.is_finite()
                || !(0.1..=24.0).contains(&band.q)
                || !band.slope_db_per_octave.is_finite()
                || !(6.0..=48.0).contains(&band.slope_db_per_octave)
            {
                return Err(ProcessError::InvalidParameterValue);
            }
        }
        for index in 0..self.bands.len() {
            if self.bands[..index]
                .iter()
                .any(|prior| prior.id == self.bands[index].id)
            {
                return Err(ProcessError::InvalidParameterValue);
            }
        }
        self.sample_rate = spec.sample_rate;
        self.layout = Some(spec.input_layout);
        self.maximum_block_size = spec.max_block_size;
        self.runtime = self
            .bands
            .iter()
            .map(|band| EqRuntime::new(band, self.sample_rate))
            .collect();
        self.output_gain =
            LinearSmoother::new(db_to_gain(self.output_gain_db), self.sample_rate, 10.0);
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
                self.apply_event(&events[next])?;
                next += 1;
            }
            for index in 0..self.bands.len() {
                let Some((frequency_hz, gain_db, q)) = self.runtime[index].advance_parameters()
                else {
                    continue;
                };
                let band = &self.bands[index];
                let coefficient = Self::coefficients(
                    self.sample_rate,
                    band.shape,
                    band.slope_db_per_octave,
                    frequency_hz,
                    gain_db,
                    q,
                );
                for channel in &mut self.runtime[index].filters {
                    for filter in channel {
                        filter.set_coefficients(coefficient);
                    }
                }
            }
            let gain = self.output_gain.next();
            for channel in 0..input.len() {
                let mut sample = input[channel][frame];
                for (index, band) in self.bands.iter().enumerate() {
                    if band.enabled {
                        for filter in self.runtime[index].filters[channel]
                            .iter_mut()
                            .take(Self::band_stage_count(band))
                        {
                            sample = filter.process(sample);
                        }
                    }
                }
                output[channel][frame] = sample * gain;
            }
        }
        Ok(())
    }
    fn reset(&mut self) {
        for (runtime, band) in self.runtime.iter_mut().zip(&self.bands) {
            runtime.reset_parameters(band);
            for channel in &mut runtime.filters {
                for filter in channel {
                    filter.reset();
                }
            }
        }
        self.output_gain.jump_to(db_to_gain(self.output_gain_db));
        self.update();
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

    #[test]
    fn eq_assigns_stable_legacy_ids_and_rejects_duplicates() {
        let mut eq = ParametricEq {
            bands: vec![EqBand::default(), EqBand::default()],
            ..ParametricEq::default()
        };
        eq.prepare(PrepareSpec::default()).unwrap();
        assert_eq!(eq.bands[0].id, "band-0");
        assert_eq!(eq.bands[1].id, "band-1");

        eq.bands[1].id = "band-0".into();
        assert!(eq.prepare(PrepareSpec::default()).is_err());
    }

    #[test]
    fn eq_pass_band_slope_controls_cascade_order() {
        fn render(slope_db_per_octave: f32) -> f32 {
            let band = EqBand {
                id: "low-pass".into(),
                shape: EqShape::LowPass,
                frequency_hz: 1_000.0,
                slope_db_per_octave,
                ..EqBand::default()
            };
            let mut eq = ParametricEq {
                bands: vec![band],
                ..ParametricEq::default()
            };
            eq.prepare(PrepareSpec {
                input_layout: AudioLayout::Mono,
                max_block_size: 256,
                ..PrepareSpec::default()
            })
            .unwrap();
            let input: Vec<f32> = (0..256)
                .map(|frame| (std::f32::consts::TAU * 8_000.0 * frame as f32 / 48_000.0).sin())
                .collect();
            let mut output = [0.0; 256];
            eq.process(
                &[&input],
                &mut [&mut output],
                &[],
                ProcessContext::default(),
            )
            .unwrap();
            output[128..].iter().map(|sample| sample * sample).sum()
        }

        assert!(render(48.0) < render(12.0));
    }

    #[test]
    fn eq_exposes_core_compatible_indexed_band_parameters() {
        let eq = ParametricEq::default();
        let ids: Vec<_> = eq
            .parameters()
            .iter()
            .map(|parameter| parameter.id)
            .collect();
        assert_eq!(ids.len(), 1 + 8 * 6);
        for id in [
            "bands.0.enabled",
            "bands.0.shape",
            "bands.0.frequency_hz",
            "bands.0.gain_db",
            "bands.0.q",
            "bands.0.slope_db_per_octave",
            "bands.7.frequency_hz",
        ] {
            assert!(ids.contains(&id), "missing {id}");
        }
        assert!(
            eq.parameters()
                .iter()
                .find(|parameter| parameter.id == "bands.0.frequency_hz")
                .unwrap()
                .automatable
        );
        assert!(
            !eq.parameters()
                .iter()
                .find(|parameter| parameter.id == "bands.0.shape")
                .unwrap()
                .automatable
        );
    }

    #[test]
    fn eq_indexed_band_automation_is_smoothed() {
        let mut eq = ParametricEq::default();
        eq.prepare(PrepareSpec {
            input_layout: AudioLayout::Mono,
            max_block_size: 512,
            ..PrepareSpec::default()
        })
        .unwrap();
        let input = [0.25];
        let mut output = [0.0];
        eq.process(
            &[&input],
            &mut [&mut output],
            &[ParameterEvent::new(
                0,
                "bands.0.frequency_hz",
                ParameterValue::Float(8_000.0),
            )],
            ProcessContext::default(),
        )
        .unwrap();

        assert_eq!(eq.bands[0].frequency_hz, 8_000.0);
        let smoothed = eq.runtime[0].frequency_hz.current().exp();
        assert!(smoothed > 1_000.0 && smoothed < 8_000.0);
        assert!(output[0].is_finite());

        let result = eq.process(
            &[&input],
            &mut [&mut output],
            &[ParameterEvent::new(
                0,
                "bands.0.shape",
                ParameterValue::Choice(0),
            )],
            ProcessContext::default(),
        );
        assert_eq!(result, Err(ProcessError::InvalidParameterValue));
    }
}
