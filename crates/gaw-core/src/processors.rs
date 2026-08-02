//! Canonical, serializable state for GAW's built-in audio processors.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::model::ChannelLayout;

/// Stable identity of one processor instance in a stack.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ProcessorId(
    #[schemars(length(min = 1, max = 128), regex(pattern = r"^[A-Za-z0-9_-]+$"))] String,
);

impl ProcessorId {
    /// Creates an ID after checking its portable canonical syntax.
    ///
    /// # Errors
    /// Returns [`ValidationError`] when the value is empty, too long, or contains unsupported bytes.
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(ValidationError::new(
                "id",
                "must be 1..=128 ASCII letters, digits, '_' or '-'",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProcessorId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ProcessorId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// A processor instance. Its kind contains its complete meaningful state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Processor {
    pub id: ProcessorId,
    pub processor_version: u32,
    pub enabled: bool,
    #[serde(flatten)]
    pub kind: ProcessorKind,
}

impl Processor {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn new(id: ProcessorId, kind: ProcessorKind) -> Self {
        Self {
            id,
            processor_version: Self::CURRENT_VERSION,
            enabled: true,
            kind,
        }
    }

    /// Checks the ID, version, and kind-specific parameter invariants.
    ///
    /// # Errors
    /// Returns [`ValidationError`] for the first invalid field.
    pub fn validate(&self) -> Result<(), ValidationError> {
        if self.processor_version != Self::CURRENT_VERSION {
            return Err(ValidationError::new(
                "processor_version",
                "unsupported processor version",
            ));
        }
        self.kind.validate()
    }

    pub fn metadata(&self) -> ProcessorMetadata {
        self.kind.metadata()
    }
}

/// All required first-party effects and analyzers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "parameters", deny_unknown_fields)]
pub enum ProcessorKind {
    #[serde(rename = "gaw.gain")]
    Gain(GainParameters),
    #[serde(rename = "gaw.stereo_tool")]
    StereoTool(StereoToolParameters),
    #[serde(rename = "gaw.filter")]
    Filter(FilterParameters),
    #[serde(rename = "gaw.parametric_eq")]
    ParametricEq(ParametricEqParameters),
    #[serde(rename = "gaw.compressor")]
    Compressor(CompressorParameters),
    #[serde(rename = "gaw.limiter")]
    Limiter(LimiterParameters),
    #[serde(rename = "gaw.gate")]
    Gate(GateParameters),
    #[serde(rename = "gaw.expander")]
    Expander(ExpanderParameters),
    #[serde(rename = "gaw.transient_shaper")]
    TransientShaper(TransientShaperParameters),
    #[serde(rename = "gaw.saturator")]
    Saturator(SaturatorParameters),
    #[serde(rename = "gaw.clipper")]
    Clipper(ClipperParameters),
    #[serde(rename = "gaw.bitcrusher")]
    Bitcrusher(BitcrusherParameters),
    #[serde(rename = "gaw.delay")]
    Delay(DelayParameters),
    #[serde(rename = "gaw.reverb")]
    Reverb(ReverbParameters),
    #[serde(rename = "gaw.chorus")]
    Chorus(ChorusParameters),
    #[serde(rename = "gaw.flanger")]
    Flanger(FlangerParameters),
    #[serde(rename = "gaw.phaser")]
    Phaser(PhaserParameters),
    #[serde(rename = "gaw.tremolo_autopan")]
    TremoloAutopan(TremoloAutopanParameters),
    #[serde(rename = "gaw.pitch_shift")]
    PitchShift(PitchShiftParameters),
    #[serde(rename = "gaw.rhythmic_gate")]
    RhythmicGate(RhythmicGateParameters),
    #[serde(rename = "gaw.beat_repeat")]
    BeatRepeat(BeatRepeatParameters),
    #[serde(rename = "gaw.level_meter")]
    LevelMeter(LevelMeterParameters),
    #[serde(rename = "gaw.loudness_meter")]
    LoudnessMeter(LoudnessMeterParameters),
    #[serde(rename = "gaw.spectrum")]
    Spectrum(SpectrumParameters),
    #[serde(rename = "gaw.oscilloscope")]
    Oscilloscope(OscilloscopeParameters),
    #[serde(rename = "gaw.stereo_meter")]
    StereoMeter(StereoMeterParameters),
    #[serde(rename = "gaw.tuner")]
    Tuner(TunerParameters),
}

macro_rules! params {
    ($name:ident { $($field:ident: $ty:ty = $default:expr),* $(,)? }) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        #[schemars(default)]
        pub struct $name { $(pub $field: $ty),* }
        impl Default for $name {
            fn default() -> Self { Self { $($field: $default),* } }
        }
    };
}

macro_rules! choice {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
    };
}

choice!(PanLaw {
    MinusThreeDb,
    MinusFourPointFiveDb,
    MinusSixDb
});
choice!(FilterMode {
    LowPass,
    HighPass,
    BandPass,
    Notch
});
choice!(FilterSlope {
    Db12,
    Db24,
    Db36,
    Db48
});
choice!(EqShape {
    Bell,
    LowShelf,
    HighShelf,
    LowPass,
    HighPass,
    BandPass,
    Notch
});
choice!(DetectorMode { Peak, Rms });
choice!(SaturationCurve {
    SoftClip,
    Tanh,
    Asymmetric,
    Fold
});
choice!(Oversampling { Off, X2, X4, X8 });
choice!(StereoDelayMode {
    Linked,
    Offset,
    PingPong
});
choice!(ReverbAlgorithm {
    RoomV1,
    HallV1,
    PlateV1,
    ChamberV1
});
choice!(LfoWaveform {
    Sine,
    Triangle,
    Saw,
    Square
});
choice!(TremoloAutopanMode { Tremolo, Autopan });
choice!(FormantMode { Shift });
choice!(PitchQuality { Draft });
choice!(FftSize {
    N256,
    N512,
    N1024,
    N2048,
    N4096,
    N8192,
    N16384
});
choice!(WindowFunction {
    Hann,
    BlackmanHarris,
    FlatTop
});
choice!(OscilloscopeTrigger {
    Free,
    RisingZero,
    FallingZero
});

/// A time value that cannot confuse beat time with wall-clock time.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "unit",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TimeValue {
    Beats(f64),
    Seconds(f64),
}

/// A modulation rate that cannot confuse beat periods with hertz.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "unit",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RateValue {
    Hertz(f64),
    Beats(f64),
}

params!(GainParameters {
    gain_db: f32 = 0.0,
    pan: f32 = 0.0,
    pan_law: PanLaw = PanLaw::MinusThreeDb
});
params!(StereoToolParameters {
    balance: f32 = 0.0,
    width: f32 = 1.0,
    mid_gain_db: f32 = 0.0,
    side_gain_db: f32 = 0.0,
    swap_channels: bool = false,
    invert_left: bool = false,
    invert_right: bool = false,
    output_layout: ChannelLayout = ChannelLayout::Stereo
});
params!(FilterParameters {
    mode: FilterMode = FilterMode::LowPass,
    cutoff_hz: f32 = 1_000.0,
    resonance_q: f32 = 0.707,
    slope_db_per_octave: FilterSlope = FilterSlope::Db12,
    drive_db: f32 = 0.0
});

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(default)]
pub struct EqBand {
    pub enabled: bool,
    pub shape: EqShape,
    pub frequency_hz: f32,
    pub gain_db: f32,
    pub q: f32,
    pub slope_db_per_octave: FilterSlope,
}
impl Default for EqBand {
    fn default() -> Self {
        Self {
            enabled: true,
            shape: EqShape::Bell,
            frequency_hz: 1_000.0,
            gain_db: 0.0,
            q: 0.707,
            slope_db_per_octave: FilterSlope::Db12,
        }
    }
}
params!(ParametricEqParameters { bands: Vec<EqBand> = Vec::new(), output_gain_db: f32 = 0.0 });
params!(CompressorParameters {
    threshold_db: f32 = -18.0,
    ratio: f32 = 4.0,
    attack_ms: f32 = 10.0,
    release_ms: f32 = 100.0,
    knee_db: f32 = 6.0,
    detector: DetectorMode = DetectorMode::Rms,
    lookahead_ms: f32 = 0.0,
    makeup_gain_db: f32 = 0.0,
    mix: f32 = 1.0
});
params!(LimiterParameters {
    ceiling_db: f32 = -1.0,
    release_ms: f32 = 100.0,
    lookahead_ms: f32 = 1.0,
    true_peak: bool = true,
    input_gain_db: f32 = 0.0
});
params!(GateParameters {
    threshold_db: f32 = -40.0,
    hysteresis_db: f32 = 3.0,
    attack_ms: f32 = 1.0,
    hold_ms: f32 = 20.0,
    release_ms: f32 = 100.0,
    range_db: f32 = 80.0
});
params!(ExpanderParameters {
    threshold_db: f32 = -40.0,
    ratio: f32 = 2.0,
    attack_ms: f32 = 10.0,
    release_ms: f32 = 100.0,
    knee_db: f32 = 6.0,
    range_db: f32 = 40.0
});
params!(TransientShaperParameters {
    attack_amount: f32 = 0.0,
    sustain_amount: f32 = 0.0,
    sensitivity: f32 = 0.5,
    response_ms: f32 = 20.0,
    output_gain_db: f32 = 0.0
});
params!(SaturatorParameters {
    curve: SaturationCurve = SaturationCurve::Tanh,
    drive_db: f32 = 6.0,
    bias: f32 = 0.0,
    tone_hz: f32 = 8_000.0,
    output_gain_db: f32 = 0.0,
    mix: f32 = 1.0,
    oversampling: Oversampling = Oversampling::X2
});
params!(ClipperParameters {
    threshold_db: f32 = -3.0,
    softness: f32 = 0.0,
    output_ceiling_db: f32 = -1.0,
    oversampling: Oversampling = Oversampling::X4
});
params!(BitcrusherParameters {
    bit_depth: u8 = 12,
    sample_rate_ratio: f32 = 0.5,
    dither: bool = false,
    jitter: f32 = 0.0,
    mix: f32 = 1.0
});
params!(DelayParameters {
    time: TimeValue = TimeValue::Beats(0.5),
    feedback: f32 = 0.35,
    stereo_mode: StereoDelayMode = StereoDelayMode::Linked,
    stereo_offset: TimeValue = TimeValue::Beats(0.0),
    low_cut_hz: f32 = 20.0,
    high_cut_hz: f32 = 20_000.0,
    modulation_rate_hz: f32 = 0.25,
    modulation_depth: f32 = 0.0,
    width: f32 = 1.0,
    mix: f32 = 0.2
});
params!(ReverbParameters {
    algorithm: ReverbAlgorithm = ReverbAlgorithm::RoomV1,
    size: f32 = 0.5,
    decay_seconds: f32 = 1.5,
    pre_delay: TimeValue = TimeValue::Seconds(0.01),
    diffusion: f32 = 0.7,
    damping_hz: f32 = 8_000.0,
    low_cut_hz: f32 = 20.0,
    high_cut_hz: f32 = 20_000.0,
    width: f32 = 1.0,
    early_reflections: f32 = 0.5,
    mix: f32 = 0.2
});
params!(ChorusParameters {
    rate: RateValue = RateValue::Hertz(0.8),
    depth: f32 = 0.5,
    base_delay_ms: f32 = 15.0,
    voices: u8 = 3,
    stereo_phase: f32 = 0.25,
    feedback: f32 = 0.1,
    width: f32 = 1.0,
    mix: f32 = 0.5
});
params!(FlangerParameters {
    rate: RateValue = RateValue::Hertz(0.25),
    depth: f32 = 0.5,
    base_delay_ms: f32 = 2.0,
    feedback: f32 = 0.25,
    stereo_phase: f32 = 0.25,
    mix: f32 = 0.5
});
params!(PhaserParameters {
    rate: RateValue = RateValue::Hertz(0.25),
    depth: f32 = 0.5,
    center_frequency_hz: f32 = 1_000.0,
    frequency_span: f32 = 0.5,
    stages: u8 = 6,
    feedback: f32 = 0.25,
    stereo_phase: f32 = 0.25,
    mix: f32 = 0.5
});
params!(TremoloAutopanParameters {
    mode: TremoloAutopanMode = TremoloAutopanMode::Tremolo,
    rate: RateValue = RateValue::Beats(0.5),
    depth: f32 = 0.5,
    waveform: LfoWaveform = LfoWaveform::Sine,
    phase: f32 = 0.0,
    stereo_phase: f32 = 0.5,
    smoothing: f32 = 0.05
});
params!(PitchShiftParameters {
    semitones: i8 = 0,
    cents: i16 = 0,
    formant_mode: FormantMode = FormantMode::Shift,
    quality: PitchQuality = PitchQuality::Draft,
    mix: f32 = 1.0
});

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(default)]
pub struct GateStep {
    pub level: f32,
}
impl Default for GateStep {
    fn default() -> Self {
        Self { level: 1.0 }
    }
}
params!(RhythmicGateParameters { steps: Vec<GateStep> = vec![GateStep::default(); 16], step_length_beats: f64 = 0.25, attack_ms: f32 = 2.0, release_ms: f32 = 10.0, phase_offset_beats: f64 = 0.0, mix: f32 = 1.0 });
params!(BeatRepeatParameters {
    interval_beats: f64 = 1.0,
    slice_length_beats: f64 = 0.25,
    repeat_count: u16 = 4,
    gate: f32 = 1.0,
    decay: f32 = 0.0,
    pitch_step_semitones: f32 = 0.0,
    reverse_probability: f32 = 0.0,
    mix: f32 = 1.0,
    seed: u64 = 0
});

params!(LevelMeterParameters {
    window_ms: f32 = 300.0,
    peak_hold_ms: f32 = 1_000.0,
    true_peak: bool = true
});
params!(LoudnessMeterParameters {
    integration_seconds: f32 = 3.0,
    absolute_gate_lufs: f32 = -70.0
});
params!(SpectrumParameters {
    fft_size: FftSize = FftSize::N2048,
    window: WindowFunction = WindowFunction::Hann,
    smoothing: f32 = 0.5,
    minimum_hz: f32 = 20.0,
    maximum_hz: f32 = 20_000.0
});
params!(OscilloscopeParameters {
    window_ms: f32 = 20.0,
    trigger: OscilloscopeTrigger = OscilloscopeTrigger::RisingZero
});
params!(StereoMeterParameters {
    window_ms: f32 = 300.0
});
params!(TunerParameters {
    minimum_hz: f32 = 27.5,
    maximum_hz: f32 = 4_186.01,
    reference_pitch_hz: f32 = 440.0
});

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationError {
    pub parameter: &'static str,
    pub reason: &'static str,
}
impl ValidationError {
    const fn new(parameter: &'static str, reason: &'static str) -> Self {
        Self { parameter, reason }
    }
}
impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.parameter, self.reason)
    }
}
impl std::error::Error for ValidationError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LayoutBehavior {
    Preserve,
    MayProduceStereo,
    ExplicitOutput,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LatencyKind {
    None,
    Lookahead,
    Oversampling,
    Analysis,
    Algorithmic,
    CaptureBuffer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TailKind {
    None,
    Negligible,
    ShortFinite,
    FeedbackCapped,
    DecayCapped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessorMetadata {
    pub type_id: &'static str,
    pub analyzer: bool,
    pub accepts_mono: bool,
    pub accepts_stereo: bool,
    pub layout_behavior: LayoutBehavior,
    pub latency: LatencyKind,
    pub tail: TailKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParameterValueType {
    Number,
    Integer,
    Boolean,
    Choice,
    Time,
    Rate,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ParameterUnit {
    Unitless,
    Decibels,
    Lufs,
    Hertz,
    Milliseconds,
    Seconds,
    Beats,
    Normalized,
    Bipolar,
    Ratio,
    Semitones,
    Cents,
    PhaseCycles,
    Bits,
    Count,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AutomationSupport {
    None,
    Continuous,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DisplayHint {
    Linear,
    Logarithmic,
    Decibels,
    Percentage,
    Bipolar,
    Time,
    Frequency,
    Count,
    Toggle,
    Choice,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ParameterRange {
    pub minimum: f64,
    pub maximum: f64,
}

/// Static agent/UI-facing contract for a stable processor parameter path.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct ParameterDescriptor {
    pub id: &'static str,
    pub value_type: ParameterValueType,
    pub unit: ParameterUnit,
    /// Canonical JSON encoding of the catalog default.
    pub default_json: &'static str,
    pub range: Option<ParameterRange>,
    pub choices: &'static [&'static str],
    pub automation: AutomationSupport,
    pub display_hint: DisplayHint,
}

macro_rules! num {
    ($id:literal, $unit:ident, $default:literal, $min:expr, $max:expr, $automation:ident, $hint:ident) => {
        ParameterDescriptor {
            id: $id,
            value_type: ParameterValueType::Number,
            unit: ParameterUnit::$unit,
            default_json: $default,
            range: Some(ParameterRange {
                minimum: $min,
                maximum: $max,
            }),
            choices: &[],
            automation: AutomationSupport::$automation,
            display_hint: DisplayHint::$hint,
        }
    };
}
macro_rules! int {
    ($id:literal, $unit:ident, $default:literal, $min:expr, $max:expr) => {
        ParameterDescriptor {
            id: $id,
            value_type: ParameterValueType::Integer,
            unit: ParameterUnit::$unit,
            default_json: $default,
            range: Some(ParameterRange {
                minimum: $min,
                maximum: $max,
            }),
            choices: &[],
            automation: AutomationSupport::None,
            display_hint: DisplayHint::Count,
        }
    };
}
macro_rules! boolean {
    ($id:literal, $default:literal) => {
        ParameterDescriptor {
            id: $id,
            value_type: ParameterValueType::Boolean,
            unit: ParameterUnit::Unitless,
            default_json: $default,
            range: None,
            choices: &[],
            automation: AutomationSupport::None,
            display_hint: DisplayHint::Toggle,
        }
    };
}
macro_rules! choice_desc {
    ($id:literal, $default:literal, [$($choice:literal),+ $(,)?]) => {
        ParameterDescriptor { id: $id, value_type: ParameterValueType::Choice, unit: ParameterUnit::Unitless,
            default_json: $default, range: None, choices: &[$($choice),+], automation: AutomationSupport::None,
            display_hint: DisplayHint::Choice }
    };
}
macro_rules! compound {
    ($kind:ident, $id:literal, $unit:ident, $default:literal, $min:expr, $max:expr) => {
        ParameterDescriptor {
            id: $id,
            value_type: ParameterValueType::$kind,
            unit: ParameterUnit::$unit,
            default_json: $default,
            range: Some(ParameterRange {
                minimum: $min,
                maximum: $max,
            }),
            choices: &[],
            automation: AutomationSupport::Continuous,
            display_hint: DisplayHint::Time,
        }
    };
}
macro_rules! list {
    ($id:literal, $default:literal) => {
        ParameterDescriptor {
            id: $id,
            value_type: ParameterValueType::List,
            unit: ParameterUnit::Unitless,
            default_json: $default,
            range: None,
            choices: &[],
            automation: AutomationSupport::None,
            display_hint: DisplayHint::Linear,
        }
    };
}

fn number(
    value: impl Into<f64>,
    parameter: &'static str,
    min: f64,
    max: f64,
) -> Result<(), ValidationError> {
    let value = value.into();
    if value.is_finite() && (min..=max).contains(&value) {
        Ok(())
    } else {
        Err(ValidationError::new(
            parameter,
            "is outside its finite valid range",
        ))
    }
}

fn ordered(low: f32, high: f32, low_name: &'static str) -> Result<(), ValidationError> {
    if low < high {
        Ok(())
    } else {
        Err(ValidationError::new(
            low_name,
            "must be lower than the paired upper frequency",
        ))
    }
}

fn time(
    value: TimeValue,
    parameter: &'static str,
    allow_zero: bool,
) -> Result<(), ValidationError> {
    let value = match value {
        TimeValue::Beats(v) | TimeValue::Seconds(v) => v,
    };
    let min = if allow_zero { 0.0 } else { f64::EPSILON };
    number(value, parameter, min, 64.0)
}

fn rate(value: RateValue, parameter: &'static str) -> Result<(), ValidationError> {
    match value {
        RateValue::Hertz(v) => number(v, parameter, 0.01, 40.0),
        RateValue::Beats(v) => number(v, parameter, 1.0 / 64.0, 64.0),
    }
}

macro_rules! checks {
    ($($value:expr, $name:literal, $min:expr, $max:expr);+ $(;)?) => {{
        $(number($value, $name, $min, $max)?;)+
        Ok(())
    }};
}

impl ProcessorKind {
    /// One valid, version-1 default for every built-in catalog entry.
    pub fn catalog_defaults() -> Vec<Self> {
        vec![
            Self::Gain(GainParameters::default()),
            Self::StereoTool(StereoToolParameters::default()),
            Self::Filter(FilterParameters::default()),
            Self::ParametricEq(ParametricEqParameters::default()),
            Self::Compressor(CompressorParameters::default()),
            Self::Limiter(LimiterParameters::default()),
            Self::Gate(GateParameters::default()),
            Self::Expander(ExpanderParameters::default()),
            Self::TransientShaper(TransientShaperParameters::default()),
            Self::Saturator(SaturatorParameters::default()),
            Self::Clipper(ClipperParameters::default()),
            Self::Bitcrusher(BitcrusherParameters::default()),
            Self::Delay(DelayParameters::default()),
            Self::Reverb(ReverbParameters::default()),
            Self::Chorus(ChorusParameters::default()),
            Self::Flanger(FlangerParameters::default()),
            Self::Phaser(PhaserParameters::default()),
            Self::TremoloAutopan(TremoloAutopanParameters::default()),
            Self::PitchShift(PitchShiftParameters::default()),
            Self::RhythmicGate(RhythmicGateParameters::default()),
            Self::BeatRepeat(BeatRepeatParameters::default()),
            Self::LevelMeter(LevelMeterParameters::default()),
            Self::LoudnessMeter(LoudnessMeterParameters::default()),
            Self::Spectrum(SpectrumParameters::default()),
            Self::Oscilloscope(OscilloscopeParameters::default()),
            Self::StereoMeter(StereoMeterParameters::default()),
            Self::Tuner(TunerParameters::default()),
        ]
    }

    /// Complete static parameter contract for this processor type.
    #[allow(clippy::too_many_lines)]
    pub fn parameter_descriptors(&self) -> &'static [ParameterDescriptor] {
        match self {
            Self::Gain(_) => &[
                num!(
                    "gain_db", Decibels, "0.0", -120.0, 24.0, Continuous, Decibels
                ),
                num!("pan", Bipolar, "0.0", -1.0, 1.0, Continuous, Bipolar),
                choice_desc!(
                    "pan_law",
                    "\"minus_three_db\"",
                    ["minus_three_db", "minus_four_point_five_db", "minus_six_db"]
                ),
            ],
            Self::StereoTool(_) => &[
                num!("balance", Bipolar, "0.0", -1.0, 1.0, Continuous, Bipolar),
                num!("width", Ratio, "1.0", 0.0, 2.0, Continuous, Percentage),
                num!(
                    "mid_gain_db",
                    Decibels,
                    "0.0",
                    -120.0,
                    24.0,
                    Continuous,
                    Decibels
                ),
                num!(
                    "side_gain_db",
                    Decibels,
                    "0.0",
                    -120.0,
                    24.0,
                    Continuous,
                    Decibels
                ),
                boolean!("swap_channels", "false"),
                boolean!("invert_left", "false"),
                boolean!("invert_right", "false"),
                choice_desc!("output_layout", "\"stereo\"", ["mono", "stereo"]),
            ],
            Self::Filter(_) => &[
                choice_desc!(
                    "mode",
                    "\"low_pass\"",
                    ["low_pass", "high_pass", "band_pass", "notch"]
                ),
                num!(
                    "cutoff_hz",
                    Hertz,
                    "1000.0",
                    10.0,
                    24_000.0,
                    Continuous,
                    Frequency
                ),
                num!(
                    "resonance_q",
                    Ratio,
                    "0.707",
                    0.1,
                    30.0,
                    Continuous,
                    Logarithmic
                ),
                choice_desc!(
                    "slope_db_per_octave",
                    "\"db12\"",
                    ["db12", "db24", "db36", "db48"]
                ),
                num!("drive_db", Decibels, "0.0", 0.0, 36.0, Continuous, Decibels),
            ],
            Self::ParametricEq(_) => &[
                list!("bands", "[]"),
                boolean!("bands[].enabled", "true"),
                choice_desc!(
                    "bands[].shape",
                    "\"bell\"",
                    [
                        "bell",
                        "low_shelf",
                        "high_shelf",
                        "low_pass",
                        "high_pass",
                        "band_pass",
                        "notch"
                    ]
                ),
                num!(
                    "bands[].frequency_hz",
                    Hertz,
                    "1000.0",
                    10.0,
                    24_000.0,
                    Continuous,
                    Frequency
                ),
                num!(
                    "bands[].gain_db",
                    Decibels,
                    "0.0",
                    -24.0,
                    24.0,
                    Continuous,
                    Decibels
                ),
                num!(
                    "bands[].q",
                    Ratio,
                    "0.707",
                    0.1,
                    30.0,
                    Continuous,
                    Logarithmic
                ),
                choice_desc!(
                    "bands[].slope_db_per_octave",
                    "\"db12\"",
                    ["db12", "db24", "db36", "db48"]
                ),
                num!(
                    "output_gain_db",
                    Decibels,
                    "0.0",
                    -24.0,
                    24.0,
                    Continuous,
                    Decibels
                ),
            ],
            Self::Compressor(_) => &[
                num!(
                    "threshold_db",
                    Decibels,
                    "-18.0",
                    -120.0,
                    0.0,
                    Continuous,
                    Decibels
                ),
                num!("ratio", Ratio, "4.0", 1.0, 100.0, Continuous, Logarithmic),
                num!(
                    "attack_ms",
                    Milliseconds,
                    "10.0",
                    0.01,
                    2_000.0,
                    Continuous,
                    Time
                ),
                num!(
                    "release_ms",
                    Milliseconds,
                    "100.0",
                    1.0,
                    10_000.0,
                    Continuous,
                    Time
                ),
                num!("knee_db", Decibels, "6.0", 0.0, 24.0, Continuous, Decibels),
                choice_desc!("detector", "\"rms\"", ["peak", "rms"]),
                num!(
                    "lookahead_ms",
                    Milliseconds,
                    "0.0",
                    0.0,
                    100.0,
                    Continuous,
                    Time
                ),
                num!(
                    "makeup_gain_db",
                    Decibels,
                    "0.0",
                    -24.0,
                    36.0,
                    Continuous,
                    Decibels
                ),
                num!("mix", Normalized, "1.0", 0.0, 1.0, Continuous, Percentage),
            ],
            Self::Limiter(_) => &[
                num!(
                    "ceiling_db",
                    Decibels,
                    "-1.0",
                    -24.0,
                    0.0,
                    Continuous,
                    Decibels
                ),
                num!(
                    "release_ms",
                    Milliseconds,
                    "100.0",
                    1.0,
                    10_000.0,
                    Continuous,
                    Time
                ),
                num!(
                    "lookahead_ms",
                    Milliseconds,
                    "1.0",
                    0.0,
                    100.0,
                    Continuous,
                    Time
                ),
                boolean!("true_peak", "true"),
                num!(
                    "input_gain_db",
                    Decibels,
                    "0.0",
                    -24.0,
                    36.0,
                    Continuous,
                    Decibels
                ),
            ],
            Self::Gate(_) => &[
                num!(
                    "threshold_db",
                    Decibels,
                    "-40.0",
                    -120.0,
                    0.0,
                    Continuous,
                    Decibels
                ),
                num!(
                    "hysteresis_db",
                    Decibels,
                    "3.0",
                    0.0,
                    24.0,
                    Continuous,
                    Decibels
                ),
                num!(
                    "attack_ms",
                    Milliseconds,
                    "1.0",
                    0.01,
                    2_000.0,
                    Continuous,
                    Time
                ),
                num!(
                    "hold_ms",
                    Milliseconds,
                    "20.0",
                    0.0,
                    10_000.0,
                    Continuous,
                    Time
                ),
                num!(
                    "release_ms",
                    Milliseconds,
                    "100.0",
                    1.0,
                    10_000.0,
                    Continuous,
                    Time
                ),
                num!(
                    "range_db", Decibels, "80.0", 0.0, 120.0, Continuous, Decibels
                ),
            ],
            Self::Expander(_) => &[
                num!(
                    "threshold_db",
                    Decibels,
                    "-40.0",
                    -120.0,
                    0.0,
                    Continuous,
                    Decibels
                ),
                num!("ratio", Ratio, "2.0", 1.0, 100.0, Continuous, Logarithmic),
                num!(
                    "attack_ms",
                    Milliseconds,
                    "10.0",
                    0.01,
                    2_000.0,
                    Continuous,
                    Time
                ),
                num!(
                    "release_ms",
                    Milliseconds,
                    "100.0",
                    1.0,
                    10_000.0,
                    Continuous,
                    Time
                ),
                num!("knee_db", Decibels, "6.0", 0.0, 24.0, Continuous, Decibels),
                num!(
                    "range_db", Decibels, "40.0", 0.0, 120.0, Continuous, Decibels
                ),
            ],
            Self::TransientShaper(_) => &[
                num!(
                    "attack_amount",
                    Bipolar,
                    "0.0",
                    -1.0,
                    1.0,
                    Continuous,
                    Bipolar
                ),
                num!(
                    "sustain_amount",
                    Bipolar,
                    "0.0",
                    -1.0,
                    1.0,
                    Continuous,
                    Bipolar
                ),
                num!(
                    "sensitivity",
                    Normalized,
                    "0.5",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
                num!(
                    "response_ms",
                    Milliseconds,
                    "20.0",
                    0.1,
                    500.0,
                    Continuous,
                    Time
                ),
                num!(
                    "output_gain_db",
                    Decibels,
                    "0.0",
                    -24.0,
                    24.0,
                    Continuous,
                    Decibels
                ),
            ],
            Self::Saturator(_) => &[
                choice_desc!(
                    "curve",
                    "\"tanh\"",
                    ["soft_clip", "tanh", "asymmetric", "fold"]
                ),
                num!("drive_db", Decibels, "6.0", 0.0, 48.0, Continuous, Decibels),
                num!("bias", Bipolar, "0.0", -1.0, 1.0, Continuous, Bipolar),
                num!(
                    "tone_hz", Hertz, "8000.0", 20.0, 20_000.0, Continuous, Frequency
                ),
                num!(
                    "output_gain_db",
                    Decibels,
                    "0.0",
                    -36.0,
                    24.0,
                    Continuous,
                    Decibels
                ),
                num!("mix", Normalized, "1.0", 0.0, 1.0, Continuous, Percentage),
                choice_desc!("oversampling", "\"x2\"", ["off", "x2", "x4", "x8"]),
            ],
            Self::Clipper(_) => &[
                num!(
                    "threshold_db",
                    Decibels,
                    "-3.0",
                    -48.0,
                    0.0,
                    Continuous,
                    Decibels
                ),
                num!(
                    "softness", Normalized, "0.0", 0.0, 1.0, Continuous, Percentage
                ),
                num!(
                    "output_ceiling_db",
                    Decibels,
                    "-1.0",
                    -48.0,
                    0.0,
                    Continuous,
                    Decibels
                ),
                choice_desc!("oversampling", "\"x4\"", ["off", "x2", "x4", "x8"]),
            ],
            Self::Bitcrusher(_) => &[
                int!("bit_depth", Bits, "12", 1.0, 32.0),
                num!(
                    "sample_rate_ratio",
                    Normalized,
                    "0.5",
                    0.001,
                    1.0,
                    Continuous,
                    Percentage
                ),
                boolean!("dither", "false"),
                num!(
                    "jitter", Normalized, "0.0", 0.0, 1.0, Continuous, Percentage
                ),
                num!("mix", Normalized, "1.0", 0.0, 1.0, Continuous, Percentage),
            ],
            Self::Delay(_) => &[
                compound!(
                    Time,
                    "time",
                    Beats,
                    r#"{"unit":"beats","value":0.5}"#,
                    0.0,
                    64.0
                ),
                num!(
                    "feedback", Normalized, "0.35", 0.0, 0.98, Continuous, Percentage
                ),
                choice_desc!(
                    "stereo_mode",
                    "\"linked\"",
                    ["linked", "offset", "ping_pong"]
                ),
                compound!(
                    Time,
                    "stereo_offset",
                    Beats,
                    r#"{"unit":"beats","value":0.0}"#,
                    0.0,
                    64.0
                ),
                num!(
                    "low_cut_hz",
                    Hertz,
                    "20.0",
                    10.0,
                    24_000.0,
                    Continuous,
                    Frequency
                ),
                num!(
                    "high_cut_hz",
                    Hertz,
                    "20000.0",
                    10.0,
                    24_000.0,
                    Continuous,
                    Frequency
                ),
                num!(
                    "modulation_rate_hz",
                    Hertz,
                    "0.25",
                    0.0,
                    20.0,
                    Continuous,
                    Frequency
                ),
                num!(
                    "modulation_depth",
                    Normalized,
                    "0.0",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
                num!("width", Ratio, "1.0", 0.0, 2.0, Continuous, Percentage),
                num!("mix", Normalized, "0.2", 0.0, 1.0, Continuous, Percentage),
            ],
            Self::Reverb(_) => &[
                choice_desc!(
                    "algorithm",
                    "\"room_v1\"",
                    ["room_v1", "hall_v1", "plate_v1", "chamber_v1"]
                ),
                num!("size", Normalized, "0.5", 0.0, 1.0, Continuous, Percentage),
                num!(
                    "decay_seconds",
                    Seconds,
                    "1.5",
                    0.05,
                    60.0,
                    Continuous,
                    Time
                ),
                compound!(
                    Time,
                    "pre_delay",
                    Seconds,
                    r#"{"unit":"seconds","value":0.01}"#,
                    0.0,
                    64.0
                ),
                num!(
                    "diffusion",
                    Normalized,
                    "0.7",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
                num!(
                    "damping_hz",
                    Hertz,
                    "8000.0",
                    20.0,
                    20_000.0,
                    Continuous,
                    Frequency
                ),
                num!(
                    "low_cut_hz",
                    Hertz,
                    "20.0",
                    10.0,
                    24_000.0,
                    Continuous,
                    Frequency
                ),
                num!(
                    "high_cut_hz",
                    Hertz,
                    "20000.0",
                    10.0,
                    24_000.0,
                    Continuous,
                    Frequency
                ),
                num!("width", Ratio, "1.0", 0.0, 2.0, Continuous, Percentage),
                num!(
                    "early_reflections",
                    Normalized,
                    "0.5",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
                num!("mix", Normalized, "0.2", 0.0, 1.0, Continuous, Percentage),
            ],
            Self::Chorus(_) => &[
                compound!(
                    Rate,
                    "rate",
                    Hertz,
                    r#"{"unit":"hertz","value":0.8}"#,
                    0.01,
                    64.0
                ),
                num!("depth", Normalized, "0.5", 0.0, 1.0, Continuous, Percentage),
                num!(
                    "base_delay_ms",
                    Milliseconds,
                    "15.0",
                    0.1,
                    100.0,
                    Continuous,
                    Time
                ),
                int!("voices", Count, "3", 1.0, 16.0),
                num!(
                    "stereo_phase",
                    PhaseCycles,
                    "0.25",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
                num!("feedback", Bipolar, "0.1", -0.98, 0.98, Continuous, Bipolar),
                num!("width", Ratio, "1.0", 0.0, 2.0, Continuous, Percentage),
                num!("mix", Normalized, "0.5", 0.0, 1.0, Continuous, Percentage),
            ],
            Self::Flanger(_) => &[
                compound!(
                    Rate,
                    "rate",
                    Hertz,
                    r#"{"unit":"hertz","value":0.25}"#,
                    0.01,
                    64.0
                ),
                num!("depth", Normalized, "0.5", 0.0, 1.0, Continuous, Percentage),
                num!(
                    "base_delay_ms",
                    Milliseconds,
                    "2.0",
                    0.01,
                    20.0,
                    Continuous,
                    Time
                ),
                num!(
                    "feedback", Bipolar, "0.25", -0.98, 0.98, Continuous, Bipolar
                ),
                num!(
                    "stereo_phase",
                    PhaseCycles,
                    "0.25",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
                num!("mix", Normalized, "0.5", 0.0, 1.0, Continuous, Percentage),
            ],
            Self::Phaser(_) => &[
                compound!(
                    Rate,
                    "rate",
                    Hertz,
                    r#"{"unit":"hertz","value":0.25}"#,
                    0.01,
                    64.0
                ),
                num!("depth", Normalized, "0.5", 0.0, 1.0, Continuous, Percentage),
                num!(
                    "center_frequency_hz",
                    Hertz,
                    "1000.0",
                    20.0,
                    20_000.0,
                    Continuous,
                    Frequency
                ),
                num!(
                    "frequency_span",
                    Normalized,
                    "0.5",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
                int!("stages", Count, "6", 2.0, 24.0),
                num!(
                    "feedback", Bipolar, "0.25", -0.98, 0.98, Continuous, Bipolar
                ),
                num!(
                    "stereo_phase",
                    PhaseCycles,
                    "0.25",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
                num!("mix", Normalized, "0.5", 0.0, 1.0, Continuous, Percentage),
            ],
            Self::TremoloAutopan(_) => &[
                choice_desc!("mode", "\"tremolo\"", ["tremolo", "autopan"]),
                compound!(
                    Rate,
                    "rate",
                    Beats,
                    r#"{"unit":"beats","value":0.5}"#,
                    0.01,
                    64.0
                ),
                num!("depth", Normalized, "0.5", 0.0, 1.0, Continuous, Percentage),
                choice_desc!(
                    "waveform",
                    "\"sine\"",
                    ["sine", "triangle", "saw", "square"]
                ),
                num!(
                    "phase",
                    PhaseCycles,
                    "0.0",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
                num!(
                    "stereo_phase",
                    PhaseCycles,
                    "0.5",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
                num!(
                    "smoothing",
                    Normalized,
                    "0.05",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
            ],
            Self::PitchShift(_) => &[
                int!("semitones", Semitones, "0", -24.0, 24.0),
                int!("cents", Cents, "0", -100.0, 100.0),
                choice_desc!("formant_mode", "\"shift\"", ["shift"]),
                choice_desc!("quality", "\"draft\"", ["draft"]),
                num!("mix", Normalized, "1.0", 0.0, 1.0, Continuous, Percentage),
            ],
            Self::RhythmicGate(_) => &[
                list!(
                    "steps",
                    "[{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0},{\"level\":1.0}]"
                ),
                num!(
                    "steps[].level",
                    Normalized,
                    "1.0",
                    0.0,
                    1.0,
                    None,
                    Percentage
                ),
                num!(
                    "step_length_beats",
                    Beats,
                    "0.25",
                    1.0 / 64.0,
                    64.0,
                    Continuous,
                    Time
                ),
                num!(
                    "attack_ms",
                    Milliseconds,
                    "2.0",
                    0.0,
                    1_000.0,
                    Continuous,
                    Time
                ),
                num!(
                    "release_ms",
                    Milliseconds,
                    "10.0",
                    0.0,
                    1_000.0,
                    Continuous,
                    Time
                ),
                num!(
                    "phase_offset_beats",
                    Beats,
                    "0.0",
                    0.0,
                    64.0,
                    Continuous,
                    Time
                ),
                num!("mix", Normalized, "1.0", 0.0, 1.0, Continuous, Percentage),
            ],
            Self::BeatRepeat(_) => &[
                num!(
                    "interval_beats",
                    Beats,
                    "1.0",
                    1.0 / 64.0,
                    64.0,
                    Continuous,
                    Time
                ),
                num!(
                    "slice_length_beats",
                    Beats,
                    "0.25",
                    1.0 / 128.0,
                    64.0,
                    Continuous,
                    Time
                ),
                int!("repeat_count", Count, "4", 1.0, 128.0),
                num!("gate", Normalized, "1.0", 0.0, 1.0, Continuous, Percentage),
                num!("decay", Normalized, "0.0", 0.0, 1.0, Continuous, Percentage),
                num!(
                    "pitch_step_semitones",
                    Semitones,
                    "0.0",
                    -24.0,
                    24.0,
                    Continuous,
                    Linear
                ),
                num!(
                    "reverse_probability",
                    Normalized,
                    "0.0",
                    0.0,
                    1.0,
                    Continuous,
                    Percentage
                ),
                num!("mix", Normalized, "1.0", 0.0, 1.0, Continuous, Percentage),
                int!("seed", Count, "0", 0.0, 18_446_744_073_709_551_615.0),
            ],
            Self::LevelMeter(_) => &[
                num!(
                    "window_ms",
                    Milliseconds,
                    "300.0",
                    1.0,
                    10_000.0,
                    None,
                    Time
                ),
                num!(
                    "peak_hold_ms",
                    Milliseconds,
                    "1000.0",
                    0.0,
                    60_000.0,
                    None,
                    Time
                ),
                boolean!("true_peak", "true"),
            ],
            Self::LoudnessMeter(_) => &[
                num!(
                    "integration_seconds",
                    Seconds,
                    "3.0",
                    0.1,
                    600.0,
                    None,
                    Time
                ),
                num!(
                    "absolute_gate_lufs",
                    Lufs,
                    "-70.0",
                    -100.0,
                    0.0,
                    None,
                    Decibels
                ),
            ],
            Self::Spectrum(_) => &[
                choice_desc!(
                    "fft_size",
                    "\"n2048\"",
                    ["n256", "n512", "n1024", "n2048", "n4096", "n8192", "n16384"]
                ),
                choice_desc!(
                    "window",
                    "\"hann\"",
                    ["hann", "blackman_harris", "flat_top"]
                ),
                num!("smoothing", Normalized, "0.5", 0.0, 1.0, None, Percentage),
                num!("minimum_hz", Hertz, "20.0", 1.0, 24_000.0, None, Frequency),
                num!(
                    "maximum_hz",
                    Hertz,
                    "20000.0",
                    1.0,
                    24_000.0,
                    None,
                    Frequency
                ),
            ],
            Self::Oscilloscope(_) => &[
                num!("window_ms", Milliseconds, "20.0", 0.1, 10_000.0, None, Time),
                choice_desc!(
                    "trigger",
                    "\"rising_zero\"",
                    ["free", "rising_zero", "falling_zero"]
                ),
            ],
            Self::StereoMeter(_) => &[num!(
                "window_ms",
                Milliseconds,
                "300.0",
                1.0,
                10_000.0,
                None,
                Time
            )],
            Self::Tuner(_) => &[
                num!("minimum_hz", Hertz, "27.5", 1.0, 24_000.0, None, Frequency),
                num!(
                    "maximum_hz",
                    Hertz,
                    "4186.01",
                    1.0,
                    24_000.0,
                    None,
                    Frequency
                ),
                num!(
                    "reference_pitch_hz",
                    Hertz,
                    "440.0",
                    400.0,
                    480.0,
                    None,
                    Frequency
                ),
            ],
        }
    }

    pub fn type_id(&self) -> &'static str {
        match self {
            Self::Gain(_) => "gaw.gain",
            Self::StereoTool(_) => "gaw.stereo_tool",
            Self::Filter(_) => "gaw.filter",
            Self::ParametricEq(_) => "gaw.parametric_eq",
            Self::Compressor(_) => "gaw.compressor",
            Self::Limiter(_) => "gaw.limiter",
            Self::Gate(_) => "gaw.gate",
            Self::Expander(_) => "gaw.expander",
            Self::TransientShaper(_) => "gaw.transient_shaper",
            Self::Saturator(_) => "gaw.saturator",
            Self::Clipper(_) => "gaw.clipper",
            Self::Bitcrusher(_) => "gaw.bitcrusher",
            Self::Delay(_) => "gaw.delay",
            Self::Reverb(_) => "gaw.reverb",
            Self::Chorus(_) => "gaw.chorus",
            Self::Flanger(_) => "gaw.flanger",
            Self::Phaser(_) => "gaw.phaser",
            Self::TremoloAutopan(_) => "gaw.tremolo_autopan",
            Self::PitchShift(_) => "gaw.pitch_shift",
            Self::RhythmicGate(_) => "gaw.rhythmic_gate",
            Self::BeatRepeat(_) => "gaw.beat_repeat",
            Self::LevelMeter(_) => "gaw.level_meter",
            Self::LoudnessMeter(_) => "gaw.loudness_meter",
            Self::Spectrum(_) => "gaw.spectrum",
            Self::Oscilloscope(_) => "gaw.oscilloscope",
            Self::StereoMeter(_) => "gaw.stereo_meter",
            Self::Tuner(_) => "gaw.tuner",
        }
    }

    pub fn is_analyzer(&self) -> bool {
        matches!(
            self,
            Self::LevelMeter(_)
                | Self::LoudnessMeter(_)
                | Self::Spectrum(_)
                | Self::Oscilloscope(_)
                | Self::StereoMeter(_)
                | Self::Tuner(_)
        )
    }

    pub fn metadata(&self) -> ProcessorMetadata {
        let (layout_behavior, latency, tail) = match self {
            Self::StereoTool(_) => (
                LayoutBehavior::ExplicitOutput,
                LatencyKind::None,
                TailKind::None,
            ),
            Self::Gain(_) => (
                LayoutBehavior::MayProduceStereo,
                LatencyKind::None,
                TailKind::None,
            ),
            Self::Delay(_) => (
                LayoutBehavior::MayProduceStereo,
                LatencyKind::None,
                TailKind::FeedbackCapped,
            ),
            Self::Reverb(_) => (
                LayoutBehavior::MayProduceStereo,
                LatencyKind::Algorithmic,
                TailKind::DecayCapped,
            ),
            Self::Chorus(_) | Self::Flanger(_) => (
                LayoutBehavior::MayProduceStereo,
                LatencyKind::Algorithmic,
                TailKind::FeedbackCapped,
            ),
            Self::Phaser(_) => (
                LayoutBehavior::Preserve,
                LatencyKind::None,
                TailKind::ShortFinite,
            ),
            Self::TremoloAutopan(p) if p.mode == TremoloAutopanMode::Autopan => (
                LayoutBehavior::MayProduceStereo,
                LatencyKind::None,
                TailKind::None,
            ),
            Self::Compressor(_) | Self::Limiter(_) => (
                LayoutBehavior::Preserve,
                LatencyKind::Lookahead,
                TailKind::None,
            ),
            Self::TransientShaper(_) => (
                LayoutBehavior::Preserve,
                LatencyKind::Analysis,
                TailKind::ShortFinite,
            ),
            Self::Saturator(_) | Self::Clipper(_) => (
                LayoutBehavior::Preserve,
                LatencyKind::Oversampling,
                TailKind::None,
            ),
            Self::PitchShift(_) => (
                LayoutBehavior::Preserve,
                LatencyKind::Algorithmic,
                TailKind::ShortFinite,
            ),
            Self::BeatRepeat(_) => (
                LayoutBehavior::Preserve,
                LatencyKind::CaptureBuffer,
                TailKind::ShortFinite,
            ),
            _ => (LayoutBehavior::Preserve, LatencyKind::None, TailKind::None),
        };
        ProcessorMetadata {
            type_id: self.type_id(),
            analyzer: self.is_analyzer(),
            accepts_mono: true,
            accepts_stereo: true,
            layout_behavior,
            latency,
            tail,
        }
    }

    /// Conservative latency declaration for alignment by the engine.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn latency_frames(&self, sample_rate: u32) -> u32 {
        let milliseconds = match self {
            Self::Compressor(p) => p.lookahead_ms,
            Self::Limiter(p) => p.lookahead_ms + if p.true_peak { 1.0 } else { 0.0 },
            Self::TransientShaper(p) => p.response_ms.min(20.0),
            Self::Saturator(p) => oversampling_latency_ms(p.oversampling),
            Self::Clipper(p) => oversampling_latency_ms(p.oversampling),
            Self::Reverb(_) => 1.0,
            Self::Chorus(p) => p.base_delay_ms,
            Self::Flanger(p) => p.base_delay_ms,
            Self::PitchShift(_) => 10.0,
            _ => 0.0,
        };
        ((f64::from(milliseconds) * f64::from(sample_rate) / 1_000.0).ceil() as u64)
            .min(u64::from(u32::MAX)) as u32
    }

    /// Finite upper bound used when calculating composition render tails.
    pub fn tail_seconds_cap(&self) -> f32 {
        match self {
            Self::Delay(_) | Self::Reverb(_) => 60.0,
            Self::Chorus(_) | Self::Flanger(_) | Self::Phaser(_) => 10.0,
            Self::TransientShaper(p) => p.response_ms / 1_000.0,
            Self::PitchShift(_) => 1.0,
            Self::BeatRepeat(_) => 32.0,
            _ => 0.0,
        }
    }

    /// Required finite tail at the given project tempo, capped by the catalog declaration.
    #[allow(clippy::cast_possible_truncation)]
    pub fn tail_seconds(&self, bpm: f64) -> f32 {
        let beat_seconds = if bpm.is_finite() && bpm > 0.0 {
            60.0 / bpm
        } else {
            0.5
        };
        let feedback_tail = |delay_seconds: f64, feedback: f32| {
            if feedback.abs() <= f32::EPSILON {
                0.0
            } else {
                delay_seconds
                    * (0.000_1_f64.ln() / f64::from(feedback.abs()).ln())
                        .ceil()
                        .max(1.0)
            }
        };
        let seconds = match self {
            Self::Delay(p) => feedback_tail(time_seconds(p.time, beat_seconds), p.feedback),
            Self::Reverb(p) => f64::from(p.decay_seconds) + time_seconds(p.pre_delay, beat_seconds),
            Self::Chorus(p) => feedback_tail(f64::from(p.base_delay_ms) / 1_000.0, p.feedback),
            Self::Flanger(p) => feedback_tail(f64::from(p.base_delay_ms) / 1_000.0, p.feedback),
            Self::Phaser(p) => feedback_tail(0.02, p.feedback),
            Self::TransientShaper(p) => f64::from(p.response_ms) / 1_000.0,
            Self::PitchShift(_) => 1.0,
            Self::BeatRepeat(p) => p.slice_length_beats * f64::from(p.repeat_count) * beat_seconds,
            _ => 0.0,
        };
        seconds.clamp(0.0, f64::from(self.tail_seconds_cap())) as f32
    }

    /// Output layout declaration; downmixing is possible only through `gaw.stereo_tool`.
    pub fn output_layout(&self, input: ChannelLayout) -> ChannelLayout {
        match self {
            Self::StereoTool(p) => p.output_layout,
            Self::Gain(p) if input == ChannelLayout::Mono && p.pan != 0.0 => ChannelLayout::Stereo,
            Self::Delay(_) | Self::Reverb(_) | Self::Chorus(_) => ChannelLayout::Stereo,
            Self::TremoloAutopan(p) if p.mode == TremoloAutopanMode::Autopan => {
                ChannelLayout::Stereo
            }
            _ => input,
        }
    }

    /// Validates all parameter ranges and cross-parameter constraints.
    ///
    /// # Errors
    /// Returns [`ValidationError`] for the first invalid parameter.
    #[allow(clippy::too_many_lines)]
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::Gain(p) => checks!(p.gain_db, "gain_db", -120.0, 24.0; p.pan, "pan", -1.0, 1.0),
            Self::StereoTool(p) => {
                checks!(p.balance, "balance", -1.0, 1.0; p.width, "width", 0.0, 2.0; p.mid_gain_db, "mid_gain_db", -120.0, 24.0; p.side_gain_db, "side_gain_db", -120.0, 24.0)
            }
            Self::Filter(p) => {
                checks!(p.cutoff_hz, "cutoff_hz", 10.0, 24_000.0; p.resonance_q, "resonance_q", 0.1, 30.0; p.drive_db, "drive_db", 0.0, 36.0)
            }
            Self::ParametricEq(p) => {
                if p.bands.len() > 8 {
                    return Err(ValidationError::new(
                        "bands",
                        "may contain at most eight bands",
                    ));
                }
                number(p.output_gain_db, "output_gain_db", -24.0, 24.0)?;
                for b in &p.bands {
                    checks!(b.frequency_hz, "bands.frequency_hz", 10.0, 24_000.0; b.gain_db, "bands.gain_db", -24.0, 24.0; b.q, "bands.q", 0.1, 30.0)?;
                }
                Ok(())
            }
            Self::Compressor(p) => {
                checks!(p.threshold_db, "threshold_db", -120.0, 0.0; p.ratio, "ratio", 1.0, 100.0; p.attack_ms, "attack_ms", 0.01, 2_000.0; p.release_ms, "release_ms", 1.0, 10_000.0; p.knee_db, "knee_db", 0.0, 24.0; p.lookahead_ms, "lookahead_ms", 0.0, 100.0; p.makeup_gain_db, "makeup_gain_db", -24.0, 36.0; p.mix, "mix", 0.0, 1.0)
            }
            Self::Limiter(p) => {
                checks!(p.ceiling_db, "ceiling_db", -24.0, 0.0; p.release_ms, "release_ms", 1.0, 10_000.0; p.lookahead_ms, "lookahead_ms", 0.0, 100.0; p.input_gain_db, "input_gain_db", -24.0, 36.0)
            }
            Self::Gate(p) => {
                checks!(p.threshold_db, "threshold_db", -120.0, 0.0; p.hysteresis_db, "hysteresis_db", 0.0, 24.0; p.attack_ms, "attack_ms", 0.01, 2_000.0; p.hold_ms, "hold_ms", 0.0, 10_000.0; p.release_ms, "release_ms", 1.0, 10_000.0; p.range_db, "range_db", 0.0, 120.0)
            }
            Self::Expander(p) => {
                checks!(p.threshold_db, "threshold_db", -120.0, 0.0; p.ratio, "ratio", 1.0, 100.0; p.attack_ms, "attack_ms", 0.01, 2_000.0; p.release_ms, "release_ms", 1.0, 10_000.0; p.knee_db, "knee_db", 0.0, 24.0; p.range_db, "range_db", 0.0, 120.0)
            }
            Self::TransientShaper(p) => {
                checks!(p.attack_amount, "attack_amount", -1.0, 1.0; p.sustain_amount, "sustain_amount", -1.0, 1.0; p.sensitivity, "sensitivity", 0.0, 1.0; p.response_ms, "response_ms", 0.1, 500.0; p.output_gain_db, "output_gain_db", -24.0, 24.0)
            }
            Self::Saturator(p) => {
                checks!(p.drive_db, "drive_db", 0.0, 48.0; p.bias, "bias", -1.0, 1.0; p.tone_hz, "tone_hz", 20.0, 20_000.0; p.output_gain_db, "output_gain_db", -36.0, 24.0; p.mix, "mix", 0.0, 1.0)
            }
            Self::Clipper(p) => {
                checks!(p.threshold_db, "threshold_db", -48.0, 0.0; p.softness, "softness", 0.0, 1.0; p.output_ceiling_db, "output_ceiling_db", -48.0, 0.0)
            }
            Self::Bitcrusher(p) => {
                checks!(p.bit_depth, "bit_depth", 1.0, 32.0; p.sample_rate_ratio, "sample_rate_ratio", 0.001, 1.0; p.jitter, "jitter", 0.0, 1.0; p.mix, "mix", 0.0, 1.0)
            }
            Self::Delay(p) => {
                time(p.time, "time", false)?;
                time(p.stereo_offset, "stereo_offset", true)?;
                checks!(p.feedback, "feedback", 0.0, 0.98; p.low_cut_hz, "low_cut_hz", 10.0, 24_000.0; p.high_cut_hz, "high_cut_hz", 10.0, 24_000.0; p.modulation_rate_hz, "modulation_rate_hz", 0.0, 20.0; p.modulation_depth, "modulation_depth", 0.0, 1.0; p.width, "width", 0.0, 2.0; p.mix, "mix", 0.0, 1.0)?;
                ordered(p.low_cut_hz, p.high_cut_hz, "low_cut_hz")
            }
            Self::Reverb(p) => {
                time(p.pre_delay, "pre_delay", true)?;
                checks!(p.size, "size", 0.0, 1.0; p.decay_seconds, "decay_seconds", 0.05, 60.0; p.diffusion, "diffusion", 0.0, 1.0; p.damping_hz, "damping_hz", 20.0, 20_000.0; p.low_cut_hz, "low_cut_hz", 10.0, 24_000.0; p.high_cut_hz, "high_cut_hz", 10.0, 24_000.0; p.width, "width", 0.0, 2.0; p.early_reflections, "early_reflections", 0.0, 1.0; p.mix, "mix", 0.0, 1.0)?;
                ordered(p.low_cut_hz, p.high_cut_hz, "low_cut_hz")
            }
            Self::Chorus(p) => {
                rate(p.rate, "rate")?;
                checks!(p.depth, "depth", 0.0, 1.0; p.base_delay_ms, "base_delay_ms", 0.1, 100.0; p.voices, "voices", 1.0, 16.0; p.stereo_phase, "stereo_phase", 0.0, 1.0; p.feedback, "feedback", -0.98, 0.98; p.width, "width", 0.0, 2.0; p.mix, "mix", 0.0, 1.0)
            }
            Self::Flanger(p) => {
                rate(p.rate, "rate")?;
                checks!(p.depth, "depth", 0.0, 1.0; p.base_delay_ms, "base_delay_ms", 0.01, 20.0; p.feedback, "feedback", -0.98, 0.98; p.stereo_phase, "stereo_phase", 0.0, 1.0; p.mix, "mix", 0.0, 1.0)
            }
            Self::Phaser(p) => {
                rate(p.rate, "rate")?;
                checks!(p.depth, "depth", 0.0, 1.0; p.center_frequency_hz, "center_frequency_hz", 20.0, 20_000.0; p.frequency_span, "frequency_span", 0.0, 1.0; p.stages, "stages", 2.0, 24.0; p.feedback, "feedback", -0.98, 0.98; p.stereo_phase, "stereo_phase", 0.0, 1.0; p.mix, "mix", 0.0, 1.0)
            }
            Self::TremoloAutopan(p) => {
                rate(p.rate, "rate")?;
                checks!(p.depth, "depth", 0.0, 1.0; p.phase, "phase", 0.0, 1.0; p.stereo_phase, "stereo_phase", 0.0, 1.0; p.smoothing, "smoothing", 0.0, 1.0)
            }
            Self::PitchShift(p) => {
                checks!(p.semitones, "semitones", -24.0, 24.0; p.cents, "cents", -100.0, 100.0; p.mix, "mix", 0.0, 1.0)
            }
            Self::RhythmicGate(p) => {
                if p.steps.is_empty() || p.steps.len() > 64 {
                    return Err(ValidationError::new("steps", "must contain 1..=64 steps"));
                }
                for s in &p.steps {
                    number(s.level, "steps.level", 0.0, 1.0)?;
                }
                checks!(p.step_length_beats, "step_length_beats", 1.0 / 64.0, 64.0; p.attack_ms, "attack_ms", 0.0, 1_000.0; p.release_ms, "release_ms", 0.0, 1_000.0; p.phase_offset_beats, "phase_offset_beats", 0.0, 64.0; p.mix, "mix", 0.0, 1.0)
            }
            Self::BeatRepeat(p) => {
                checks!(p.interval_beats, "interval_beats", 1.0 / 64.0, 64.0; p.slice_length_beats, "slice_length_beats", 1.0 / 128.0, 64.0; p.repeat_count, "repeat_count", 1.0, 128.0; p.gate, "gate", 0.0, 1.0; p.decay, "decay", 0.0, 1.0; p.pitch_step_semitones, "pitch_step_semitones", -24.0, 24.0; p.reverse_probability, "reverse_probability", 0.0, 1.0; p.mix, "mix", 0.0, 1.0)
            }
            Self::LevelMeter(p) => {
                checks!(p.window_ms, "window_ms", 1.0, 10_000.0; p.peak_hold_ms, "peak_hold_ms", 0.0, 60_000.0)
            }
            Self::LoudnessMeter(p) => {
                checks!(p.integration_seconds, "integration_seconds", 0.1, 600.0; p.absolute_gate_lufs, "absolute_gate_lufs", -100.0, 0.0)
            }
            Self::Spectrum(p) => {
                checks!(p.smoothing, "smoothing", 0.0, 1.0; p.minimum_hz, "minimum_hz", 1.0, 24_000.0; p.maximum_hz, "maximum_hz", 1.0, 24_000.0)?;
                ordered(p.minimum_hz, p.maximum_hz, "minimum_hz")
            }
            Self::Oscilloscope(p) => checks!(p.window_ms, "window_ms", 0.1, 10_000.0),
            Self::StereoMeter(p) => checks!(p.window_ms, "window_ms", 1.0, 10_000.0),
            Self::Tuner(p) => {
                checks!(p.minimum_hz, "minimum_hz", 1.0, 24_000.0; p.maximum_hz, "maximum_hz", 1.0, 24_000.0; p.reference_pitch_hz, "reference_pitch_hz", 400.0, 480.0)?;
                ordered(p.minimum_hz, p.maximum_hz, "minimum_hz")
            }
        }
    }
}

const fn oversampling_latency_ms(value: Oversampling) -> f32 {
    match value {
        Oversampling::Off => 0.0,
        Oversampling::X2 => 0.1,
        Oversampling::X4 => 0.2,
        Oversampling::X8 => 0.4,
    }
}

fn time_seconds(value: TimeValue, beat_seconds: f64) -> f64 {
    match value {
        TimeValue::Beats(value) => value * beat_seconds,
        TimeValue::Seconds(value) => value,
    }
}

/// Ephemeral analyzer output. It is structured for UI and agent consumers but is not persisted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", content = "measurement", deny_unknown_fields)]
pub enum AnalyzerMeasurement {
    #[serde(rename = "gaw.level_meter")]
    LevelMeter(LevelMeterMeasurement),
    #[serde(rename = "gaw.loudness_meter")]
    LoudnessMeter(LoudnessMeasurement),
    #[serde(rename = "gaw.spectrum")]
    Spectrum(SpectrumMeasurement),
    #[serde(rename = "gaw.oscilloscope")]
    Oscilloscope(OscilloscopeMeasurement),
    #[serde(rename = "gaw.stereo_meter")]
    StereoMeter(StereoMeasurement),
    #[serde(rename = "gaw.tuner")]
    Tuner(TunerMeasurement),
}

macro_rules! measurement {
    ($name:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name { $(pub $field: $ty),* }
    };
}

measurement!(LevelMeterMeasurement {
    sample_peak_dbfs: Vec<f32>, true_peak_dbfs: Vec<f32>, rms_dbfs: Vec<f32>,
    peak_hold_dbfs: Vec<f32>, clipping: Vec<bool>,
});
measurement!(LoudnessMeasurement {
    momentary_lufs: f32,
    short_term_lufs: f32,
    integrated_lufs: f32,
    loudness_range_lu: f32,
});
measurement!(SpectrumBin {
    frequency_hz: f32,
    magnitude_dbfs: f32
});
measurement!(SpectralPeak {
    frequency_hz: f32,
    magnitude_dbfs: f32
});
measurement!(SpectrumMeasurement {
    bins: Vec<SpectrumBin>, peaks: Vec<SpectralPeak>, spectral_centroid_hz: f32,
});
measurement!(OscilloscopeMeasurement {
    sample_rate_hz: u32, channel_samples: Vec<Vec<f32>>, zero_crossing_rate_hz: Vec<f32>,
});
measurement!(StereoMeasurement {
    mid_level_dbfs: f32,
    side_level_dbfs: f32,
    correlation: f32,
    stereo_width: f32,
});
measurement!(TunerMeasurement {
    fundamental_hz: f32,
    note_name: String,
    cents_offset: f32,
    confidence: f32,
});

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn complete_catalog_is_valid_unique_and_round_trips() {
        let catalog = ProcessorKind::catalog_defaults();
        assert_eq!(catalog.len(), 27);
        let mut ids = HashSet::new();
        for (index, kind) in catalog.into_iter().enumerate() {
            assert!(ids.insert(kind.type_id()), "duplicate type id");
            kind.validate().expect("catalog defaults must validate");
            let processor = Processor::new(ProcessorId::new(format!("fx_{index}")).unwrap(), kind);
            let json = serde_json::to_string(&processor).unwrap();
            assert!(json.contains(processor.kind.type_id()));
            let decoded: Processor = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, processor);
        }
        assert_eq!(ids.len(), 27);
    }

    #[test]
    fn canonical_json_is_strict() {
        let top_level = r#"{"id":"fx_1","processor_version":1,"enabled":true,"type":"gaw.gain","parameters":{"gain_db":0.0,"pan":0.0,"pan_law":"minus_three_db"},"mystery":1}"#;
        assert!(serde_json::from_str::<Processor>(top_level).is_err());
        let parameter = r#"{"id":"fx_1","processor_version":1,"enabled":true,"type":"gaw.gain","parameters":{"gain_db":0.0,"pan":0.0,"pan_law":"minus_three_db","mystery":1}}"#;
        assert!(serde_json::from_str::<Processor>(parameter).is_err());
        let unknown_type = r#"{"id":"fx_1","processor_version":1,"enabled":true,"type":"vendor.opaque","parameters":{}}"#;
        assert!(serde_json::from_str::<Processor>(unknown_type).is_err());
    }

    #[test]
    fn explicit_time_units_have_canonical_shape() {
        let beats = serde_json::to_value(TimeValue::Beats(0.5)).unwrap();
        assert_eq!(beats, serde_json::json!({"unit":"beats", "value":0.5}));
        let hertz = serde_json::to_value(RateValue::Hertz(2.0)).unwrap();
        assert_eq!(hertz, serde_json::json!({"unit":"hertz", "value":2.0}));
        assert!(
            serde_json::from_str::<TimeValue>(r#"{"unit":"beats","value":0.5,"extra":true}"#)
                .is_err()
        );
    }

    #[test]
    fn validation_rejects_boundaries_and_non_finite_numbers() {
        for bad in [-1.0, 1.000_001, f32::INFINITY, f32::NAN] {
            let p = CompressorParameters {
                mix: bad,
                ..CompressorParameters::default()
            };
            assert!(ProcessorKind::Compressor(p).validate().is_err());
        }
        let eq = ParametricEqParameters {
            bands: vec![EqBand::default(); 9],
            ..ParametricEqParameters::default()
        };
        assert!(ProcessorKind::ParametricEq(eq).validate().is_err());
        let delay = DelayParameters {
            feedback: 1.0,
            ..DelayParameters::default()
        };
        assert!(ProcessorKind::Delay(delay).validate().is_err());
        let spectrum = SpectrumParameters {
            minimum_hz: SpectrumParameters::default().maximum_hz,
            ..SpectrumParameters::default()
        };
        assert!(ProcessorKind::Spectrum(spectrum).validate().is_err());
    }

    #[test]
    fn stable_ids_and_versions_are_validated() {
        for bad in ["", "has spaces", "slash/no", "💥"] {
            assert!(ProcessorId::new(bad).is_err());
        }
        let mut processor = Processor::new(
            ProcessorId::new("fx-ok_1").unwrap(),
            ProcessorKind::Gain(GainParameters::default()),
        );
        assert!(processor.validate().is_ok());
        processor.processor_version += 1;
        assert!(processor.validate().is_err());
    }

    #[test]
    fn analyzers_are_pass_through_metadata_with_no_tail() {
        for kind in ProcessorKind::catalog_defaults() {
            let metadata = kind.metadata();
            assert!(metadata.accepts_mono && metadata.accepts_stereo);
            if kind.is_analyzer() {
                assert_eq!(metadata.tail, TailKind::None);
                assert_eq!(metadata.latency, LatencyKind::None);
                assert_eq!(metadata.layout_behavior, LayoutBehavior::Preserve);
                assert!(kind.tail_seconds_cap().abs() < f32::EPSILON);
                assert_eq!(kind.latency_frames(48_000), 0);
            }
        }
    }

    #[test]
    fn schema_contains_every_stable_type_id() {
        let schema = schemars::schema_for!(Processor);
        let json = serde_json::to_string(&schema).unwrap();
        for kind in ProcessorKind::catalog_defaults() {
            assert!(json.contains(kind.type_id()), "missing {}", kind.type_id());
        }
        assert!(json.contains("additionalProperties"));
    }

    #[test]
    fn descriptors_cover_every_canonical_parameter_and_default() {
        for kind in ProcessorKind::catalog_defaults() {
            let encoded = serde_json::to_value(&kind).unwrap();
            let parameters = encoded["parameters"].as_object().unwrap();
            let descriptors = kind.parameter_descriptors();
            let roots: HashSet<_> = descriptors
                .iter()
                .map(|descriptor| descriptor.id.split(['.', '[']).next().unwrap())
                .collect();
            assert_eq!(roots.len(), parameters.len(), "{}", kind.type_id());
            for key in parameters.keys() {
                assert!(
                    roots.contains(key.as_str()),
                    "{} missing {key}",
                    kind.type_id()
                );
            }
            let mut ids = HashSet::new();
            for descriptor in descriptors {
                assert!(
                    ids.insert(descriptor.id),
                    "duplicate {} {}",
                    kind.type_id(),
                    descriptor.id
                );
                let default: serde_json::Value =
                    serde_json::from_str(descriptor.default_json).unwrap();
                if !descriptor.id.contains(['.', '[']) {
                    let actual = &parameters[descriptor.id];
                    if let (Some(expected), Some(actual)) = (default.as_f64(), actual.as_f64()) {
                        let tolerance = 1.0e-6 * expected.abs().max(1.0);
                        assert!(
                            (expected - actual).abs() <= tolerance,
                            "{}.{} default",
                            kind.type_id(),
                            descriptor.id
                        );
                    } else {
                        assert_eq!(
                            &default,
                            actual,
                            "{}.{} default",
                            kind.type_id(),
                            descriptor.id
                        );
                    }
                }
                if let Some(range) = descriptor.range {
                    assert!(range.minimum.is_finite() && range.maximum.is_finite());
                    assert!(range.minimum <= range.maximum);
                }
                if descriptor.value_type == ParameterValueType::Choice {
                    assert!(!descriptor.choices.is_empty());
                    assert!(descriptor.choices.contains(&default.as_str().unwrap()));
                }
            }
        }
    }

    #[test]
    fn analyzer_measurement_catalog_round_trips_strictly() {
        let measurements = vec![
            AnalyzerMeasurement::LevelMeter(LevelMeterMeasurement {
                sample_peak_dbfs: vec![-1.0, -2.0],
                true_peak_dbfs: vec![-0.8, -1.8],
                rms_dbfs: vec![-12.0, -13.0],
                peak_hold_dbfs: vec![-1.0, -2.0],
                clipping: vec![false, false],
            }),
            AnalyzerMeasurement::LoudnessMeter(LoudnessMeasurement {
                momentary_lufs: -14.0,
                short_term_lufs: -15.0,
                integrated_lufs: -16.0,
                loudness_range_lu: 6.0,
            }),
            AnalyzerMeasurement::Spectrum(SpectrumMeasurement {
                bins: vec![SpectrumBin {
                    frequency_hz: 440.0,
                    magnitude_dbfs: -12.0,
                }],
                peaks: vec![SpectralPeak {
                    frequency_hz: 440.0,
                    magnitude_dbfs: -12.0,
                }],
                spectral_centroid_hz: 1_200.0,
            }),
            AnalyzerMeasurement::Oscilloscope(OscilloscopeMeasurement {
                sample_rate_hz: 48_000,
                channel_samples: vec![vec![0.0, 1.0]],
                zero_crossing_rate_hz: vec![440.0],
            }),
            AnalyzerMeasurement::StereoMeter(StereoMeasurement {
                mid_level_dbfs: -6.0,
                side_level_dbfs: -12.0,
                correlation: 0.8,
                stereo_width: 0.5,
            }),
            AnalyzerMeasurement::Tuner(TunerMeasurement {
                fundamental_hz: 440.0,
                note_name: "A4".into(),
                cents_offset: 0.0,
                confidence: 0.99,
            }),
        ];
        for measurement in measurements {
            let json = serde_json::to_string(&measurement).unwrap();
            assert_eq!(
                serde_json::from_str::<AnalyzerMeasurement>(&json).unwrap(),
                measurement
            );
        }
        let unknown = r#"{"type":"gaw.tuner","measurement":{"fundamental_hz":440.0,"note_name":"A4","cents_offset":0.0,"confidence":1.0,"opaque":true}}"#;
        assert!(serde_json::from_str::<AnalyzerMeasurement>(unknown).is_err());
    }

    #[test]
    fn latency_and_tail_declarations_are_finite_and_bounded() {
        for kind in ProcessorKind::catalog_defaults() {
            let latency = kind.latency_frames(384_000);
            let tail = kind.tail_seconds_cap();
            let required_tail = kind.tail_seconds(120.0);
            assert!(latency < u32::MAX);
            assert!(tail.is_finite() && (0.0..=60.0).contains(&tail));
            assert!(required_tail.is_finite() && (0.0..=tail).contains(&required_tail));
            if kind.output_layout(ChannelLayout::Stereo) == ChannelLayout::Mono {
                assert!(matches!(kind, ProcessorKind::StereoTool(_)));
            }
        }
    }
}
