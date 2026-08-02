//! Real-time-safe instruments, processors, effects, and analyzers.
//!
//! Processor configuration and preparation happen off the audio thread. After
//! [`Processor::prepare`], processing is bounded by the declared maximum block
//! size and uses only preallocated state.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::missing_errors_doc,
    clippy::should_implement_trait,
    clippy::struct_excessive_bools,
    clippy::struct_field_names
)]
#![cfg_attr(test, allow(clippy::float_cmp))]

pub mod analyzer;
pub mod contract;
pub mod creative;
pub mod distortion;
pub mod dynamics;
pub mod kernel;
pub mod modulation;
pub mod parameter;
pub mod resample;
pub mod sampler;
pub mod time;
pub mod tone;
pub mod utility;

pub use analyzer::{
    Analyzer, AnalyzerTap, EnergyMeasurement, EnergyMeter, LevelMeasurement, LevelMeter,
    Oscilloscope, OscilloscopeConfig, SpectrumAnalyzer, SpectrumConfig, StereoMeasurement,
    StereoMeter, Tuner, TunerMeasurement,
};
pub use contract::{AudioLayout, PrepareSpec, ProcessContext, ProcessError, Processor};
pub use creative::{BeatRepeat, PitchShift, RhythmicGate};
pub use distortion::{Bitcrusher, Clipper, Saturator};
pub use dynamics::{Compressor, Expander, Gate, Limiter, TransientShaper};
pub use modulation::{Chorus, Flanger, Phaser, TremoloAutopan};
pub use parameter::{
    ParameterDescriptor, ParameterEvent, ParameterKind, ParameterUnit, ParameterValue,
};
pub use resample::{ResampleError, repitch_planar};
pub use sampler::{
    Instrument, InstrumentError, NoteEvent, PlaybackMode, SampleAsset, Sampler, SamplerConfig,
    SamplerZone,
};
pub use time::{Delay, Reverb};
pub use tone::{Filter, ParametricEq};
pub use utility::{Gain, StereoTool};
