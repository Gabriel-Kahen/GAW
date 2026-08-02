//! Allocation-free analyzer primitives. Measurements are ephemeral and audio is never modified.

use std::f32::consts::TAU;

use gaw_core::{
    AnalyzerMeasurement, LevelMeterMeasurement, LoudnessMeasurement as CoreLoudnessMeasurement,
    OscilloscopeMeasurement, SpectralPeak, SpectrumBin, SpectrumMeasurement,
    StereoMeasurement as CoreStereoMeasurement, TunerMeasurement as CoreTunerMeasurement,
};
use serde::{Deserialize, Serialize};

use crate::contract::{
    AudioLayout, MONO_AND_STEREO, PrepareSpec, ProcessContext, ProcessError, Processor,
    copy_or_map_bypass, validate_process_io,
};
use crate::parameter::{ParameterDescriptor, ParameterEvent};
use crate::parameter::{ParameterKind, ParameterUnit, ParameterValue};
use crate::true_peak::TruePeakDetector;

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// Common analyzer primitive contract.
pub trait Analyzer: std::fmt::Debug + Send {
    /// Reserve all memory needed by [`Self::analyze`].
    fn prepare(&mut self, sample_rate: f64, maximum_block_size: usize, channels: usize);
    /// Inspect one planar audio block without allocating or modifying it.
    fn analyze(&mut self, input: &[&[f32]]);
    /// Clear ephemeral measurement history.
    fn reset(&mut self);
    /// Canonical, non-automatable configuration exposed by the processor adapter.
    fn parameters(&self) -> &'static [ParameterDescriptor] {
        &[]
    }

    /// Refresh and export the latest canonical measurement off the callback.
    fn analyzer_measurement(
        &mut self,
        _sample_rate: f64,
        _channels: usize,
    ) -> Option<AnalyzerMeasurement> {
        None
    }
}

/// A pass-through processor adapter that places an analyzer in an effect stack.
#[derive(Debug)]
pub struct AnalyzerTap<A> {
    type_id: &'static str,
    analyzer: A,
    layout: AudioLayout,
    sample_rate: f64,
    maximum_block_size: usize,
    enabled: bool,
    prepared: bool,
}

impl<A: Analyzer> AnalyzerTap<A> {
    fn new(type_id: &'static str, analyzer: A) -> Self {
        Self {
            type_id,
            analyzer,
            layout: AudioLayout::Stereo,
            sample_rate: 48_000.0,
            maximum_block_size: 0,
            enabled: true,
            prepared: false,
        }
    }

    /// Access the latest ephemeral measurements.
    pub fn analyzer(&self) -> &A {
        &self.analyzer
    }

    /// Refresh heavy measurements between callbacks or edit configuration
    /// before the next [`Processor::prepare`] call.
    pub fn analyzer_mut(&mut self) -> &mut A {
        &mut self.analyzer
    }
}

impl AnalyzerTap<LevelMeter> {
    pub fn level_meter() -> Self {
        Self::new("gaw.level_meter", LevelMeter::default())
    }
}

impl AnalyzerTap<EnergyMeter> {
    pub fn energy_meter() -> Self {
        Self::new("gaw.energy_meter", EnergyMeter::default())
    }
}

impl AnalyzerTap<LoudnessMeter> {
    /// Construct the standards-based `gaw.loudness_meter` analyzer.
    pub fn loudness_meter() -> Self {
        Self::new("gaw.loudness_meter", LoudnessMeter::default())
    }
}

impl AnalyzerTap<SpectrumAnalyzer> {
    pub fn spectrum() -> Self {
        Self::new("gaw.spectrum", SpectrumAnalyzer::default())
    }
}

impl AnalyzerTap<Oscilloscope> {
    pub fn oscilloscope() -> Self {
        Self::new("gaw.oscilloscope", Oscilloscope::default())
    }
}

impl AnalyzerTap<StereoMeter> {
    pub fn stereo_meter() -> Self {
        Self::new("gaw.stereo_meter", StereoMeter::default())
    }
}

impl AnalyzerTap<Tuner> {
    pub fn tuner() -> Self {
        Self::new("gaw.tuner", Tuner::default())
    }
}

impl<A: Analyzer> Processor for AnalyzerTap<A> {
    fn type_id(&self) -> &'static str {
        self.type_id
    }

    fn input_layouts(&self) -> &'static [AudioLayout] {
        MONO_AND_STEREO
    }

    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError> {
        Ok(input)
    }

    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError> {
        spec.validate()?;
        self.layout = spec.input_layout;
        self.sample_rate = spec.sample_rate;
        self.maximum_block_size = spec.max_block_size;
        self.analyzer.prepare(
            spec.sample_rate,
            spec.max_block_size,
            spec.input_layout.channels(),
        );
        self.prepared = true;
        Ok(())
    }

    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        _: ProcessContext,
    ) -> Result<(), ProcessError> {
        if !self.prepared {
            return Err(ProcessError::NotPrepared);
        }
        validate_process_io(
            input,
            output,
            self.layout,
            self.layout,
            self.maximum_block_size,
            events,
        )?;
        if !events.is_empty() {
            return Err(ProcessError::AnalyzerEventUnsupported);
        }
        copy_or_map_bypass(input, output);
        if self.enabled {
            self.analyzer.analyze(input);
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.analyzer.reset();
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
        self.analyzer.parameters()
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    fn analyzer_measurement(&mut self) -> Option<AnalyzerMeasurement> {
        self.analyzer
            .analyzer_measurement(self.sample_rate, self.layout.channels())
    }
}

fn amplitude_dbfs(value: f32) -> f32 {
    if value <= 1.0e-6 {
        -120.0
    } else {
        20.0 * value.log10()
    }
}

/// Peak, ITU-R BS.1770 four-times true-peak, RMS, hold, and clipping
/// measurements for up to two channels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LevelMeasurement {
    pub sample_peak: [f32; 2],
    pub true_peak: [f32; 2],
    /// Compatibility alias for [`Self::true_peak`].
    pub inter_sample_peak_estimate: [f32; 2],
    pub rms: [f32; 2],
    pub peak_hold: [f32; 2],
    pub sample_clipped: [bool; 2],
}

/// Built-in `gaw.level_meter` analyzer.
#[derive(Debug, Default)]
pub struct LevelMeter {
    measurement: LevelMeasurement,
    true_peak: [TruePeakDetector; 2],
}

impl LevelMeter {
    pub fn measurement(&self) -> LevelMeasurement {
        self.measurement
    }
}

impl Analyzer for LevelMeter {
    fn prepare(&mut self, _: f64, _: usize, _: usize) {
        self.reset();
    }

    fn analyze(&mut self, input: &[&[f32]]) {
        for (channel_index, channel) in input.iter().take(2).enumerate() {
            let mut peak = 0.0_f32;
            let mut true_peak = peak;
            let mut energy = 0.0_f64;
            for &sample in *channel {
                peak = peak.max(sample.abs());
                energy += f64::from(sample) * f64::from(sample);
                true_peak = true_peak.max(self.true_peak[channel_index].process([sample, 0.0], 1));
            }
            self.measurement.sample_peak[channel_index] = peak;
            self.measurement.true_peak[channel_index] = true_peak.max(peak);
            self.measurement.inter_sample_peak_estimate[channel_index] = true_peak.max(peak);
            self.measurement.rms[channel_index] = if channel.is_empty() {
                0.0
            } else {
                (energy / channel.len() as f64).sqrt() as f32
            };
            self.measurement.peak_hold[channel_index] =
                self.measurement.peak_hold[channel_index].max(peak);
            self.measurement.sample_clipped[channel_index] |= peak >= 1.0;
        }
    }

    fn reset(&mut self) {
        self.measurement = LevelMeasurement::default();
        for detector in &mut self.true_peak {
            detector.reset();
        }
    }

    fn analyzer_measurement(&mut self, _: f64, channels: usize) -> Option<AnalyzerMeasurement> {
        let measurement = self.measurement();
        let channels = channels.min(2);
        Some(AnalyzerMeasurement::LevelMeter(LevelMeterMeasurement {
            sample_peak_dbfs: measurement.sample_peak[..channels]
                .iter()
                .copied()
                .map(amplitude_dbfs)
                .collect(),
            true_peak_dbfs: measurement.true_peak[..channels]
                .iter()
                .copied()
                .map(amplitude_dbfs)
                .collect(),
            rms_dbfs: measurement.rms[..channels]
                .iter()
                .copied()
                .map(amplitude_dbfs)
                .collect(),
            peak_hold_dbfs: measurement.peak_hold[..channels]
                .iter()
                .copied()
                .map(amplitude_dbfs)
                .collect(),
            clipping: measurement.sample_clipped[..channels].to_vec(),
        }))
    }
}

/// Oscilloscope configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OscilloscopeConfig {
    pub capture_frames: usize,
}

impl Default for OscilloscopeConfig {
    fn default() -> Self {
        Self {
            capture_frames: 512,
        }
    }
}

/// Built-in `gaw.oscilloscope` rolling waveform analyzer.
#[derive(Debug, Default)]
pub struct Oscilloscope {
    pub config: OscilloscopeConfig,
    waveform: Vec<f32>,
    write: usize,
    zero_crossings: u64,
    analyzed_frames: u64,
}

impl Oscilloscope {
    pub fn waveform_ring(&self) -> &[f32] {
        &self.waveform
    }

    pub fn zero_crossings(&self) -> u64 {
        self.zero_crossings
    }
}

impl Analyzer for Oscilloscope {
    fn prepare(&mut self, _: f64, maximum_block_size: usize, _: usize) {
        let frames = self
            .config
            .capture_frames
            .clamp(1, maximum_block_size.max(1) * 16);
        self.waveform.resize(frames, 0.0);
        self.reset();
    }

    fn analyze(&mut self, input: &[&[f32]]) {
        let Some(first) = input.first() else { return };
        let mut previous = self.waveform
            [(self.write + self.waveform.len().saturating_sub(1)) % self.waveform.len()];
        for frame in 0..first.len() {
            let sample = if input.len() == 1 {
                first[frame]
            } else {
                (first[frame] + input[1][frame]) * 0.5
            };
            if (previous < 0.0 && sample >= 0.0) || (previous > 0.0 && sample <= 0.0) {
                self.zero_crossings = self.zero_crossings.saturating_add(1);
            }
            self.waveform[self.write] = sample;
            self.write = (self.write + 1) % self.waveform.len();
            previous = sample;
        }
        self.analyzed_frames = self
            .analyzed_frames
            .saturating_add(u64::try_from(first.len()).unwrap_or(u64::MAX));
    }

    fn reset(&mut self) {
        self.waveform.fill(0.0);
        self.write = 0;
        self.zero_crossings = 0;
        self.analyzed_frames = 0;
    }

    fn parameters(&self) -> &'static [ParameterDescriptor] {
        &OSCILLOSCOPE_PARAMETERS
    }

    fn analyzer_measurement(&mut self, sample_rate: f64, _: usize) -> Option<AnalyzerMeasurement> {
        let samples = self.waveform[self.write..]
            .iter()
            .chain(&self.waveform[..self.write])
            .copied()
            .collect();
        let crossing_rate = if self.analyzed_frames == 0 {
            0.0
        } else {
            (self.zero_crossings as f64 * sample_rate / self.analyzed_frames as f64) as f32
        };
        Some(AnalyzerMeasurement::Oscilloscope(OscilloscopeMeasurement {
            sample_rate_hz: sample_rate.round().clamp(0.0, f64::from(u32::MAX)) as u32,
            channel_samples: vec![samples],
            zero_crossing_rate_hz: vec![crossing_rate],
        }))
    }
}

const OSCILLOSCOPE_PARAMETERS: [ParameterDescriptor; 1] = [ParameterDescriptor {
    id: "capture_frames",
    name: "Capture Frames",
    kind: ParameterKind::Integer {
        min: 1,
        max: 1_048_576,
    },
    unit: ParameterUnit::Samples,
    default: ParameterValue::Integer(512),
    automatable: false,
    display_hint: Some("configuration; takes effect on prepare"),
}];

/// Spectrum analyzer configuration.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpectrumConfig {
    pub fft_size: usize,
    pub bins: usize,
}

impl Default for SpectrumConfig {
    fn default() -> Self {
        Self {
            fft_size: 1024,
            bins: 128,
        }
    }
}

/// Built-in `gaw.spectrum` bounded DFT analyzer.
#[derive(Debug)]
pub struct SpectrumAnalyzer {
    pub config: SpectrumConfig,
    sample_rate: f64,
    window: Vec<f32>,
    write: usize,
    magnitudes: Vec<f32>,
    centroid_hz: f32,
    peak_frequency_hz: f32,
}

impl Default for SpectrumAnalyzer {
    fn default() -> Self {
        Self {
            config: SpectrumConfig::default(),
            sample_rate: 48_000.0,
            window: Vec::new(),
            write: 0,
            magnitudes: Vec::new(),
            centroid_hz: 0.0,
            peak_frequency_hz: 0.0,
        }
    }
}

impl SpectrumAnalyzer {
    pub fn magnitudes(&self) -> &[f32] {
        &self.magnitudes
    }

    pub fn centroid_hz(&self) -> f32 {
        self.centroid_hz
    }

    pub fn peak_frequency_hz(&self) -> f32 {
        self.peak_frequency_hz
    }

    /// Refresh the spectrum from the captured ring.
    ///
    /// This performs the bounded DFT and must be called off the real-time audio
    /// thread, typically through [`AnalyzerTap::analyzer_mut`].
    pub fn update_measurement(&mut self) {
        self.calculate();
    }

    fn calculate(&mut self) {
        let size = self.window.len();
        let mut weighted_sum = 0.0;
        let mut magnitude_sum = 0.0;
        let mut maximum = -1.0_f32;
        let magnitude_count = self.magnitudes.len();
        let nyquist_bin = size / 2;
        for output_bin in 0..magnitude_count {
            let dft_bin = if magnitude_count == 1 {
                0
            } else {
                output_bin * nyquist_bin / (magnitude_count - 1)
            };
            let mut real = 0.0;
            let mut imaginary = 0.0;
            for index in 0..size {
                let ring_index = (self.write + index) % size;
                let hann = 0.5 - 0.5 * (TAU * index as f32 / size as f32).cos();
                let angle = TAU * dft_bin as f32 * index as f32 / size as f32;
                let sample = self.window[ring_index] * hann;
                real += sample * angle.cos();
                imaginary -= sample * angle.sin();
            }
            let magnitude = (real.mul_add(real, imaginary * imaginary)).sqrt() / size as f32;
            self.magnitudes[output_bin] = magnitude;
            let frequency = dft_bin as f32 * self.sample_rate as f32 / size as f32;
            weighted_sum += magnitude * frequency;
            magnitude_sum += magnitude;
            if magnitude > maximum {
                maximum = magnitude;
                self.peak_frequency_hz = frequency;
            }
        }
        self.centroid_hz = if magnitude_sum > f32::EPSILON {
            weighted_sum / magnitude_sum
        } else {
            0.0
        };
    }
}

impl Analyzer for SpectrumAnalyzer {
    fn prepare(&mut self, sample_rate: f64, maximum_block_size: usize, _: usize) {
        self.sample_rate = sample_rate;
        let size = self
            .config
            .fft_size
            .clamp(32, maximum_block_size.max(32) * 16);
        self.window.resize(size, 0.0);
        self.magnitudes
            .resize(self.config.bins.clamp(2, size / 2 + 1), 0.0);
        self.reset();
    }

    fn analyze(&mut self, input: &[&[f32]]) {
        let Some(first) = input.first() else { return };
        for frame in 0..first.len() {
            self.window[self.write] = if input.len() == 1 {
                first[frame]
            } else {
                (first[frame] + input[1][frame]) * 0.5
            };
            self.write = (self.write + 1) % self.window.len();
        }
    }

    fn reset(&mut self) {
        self.window.fill(0.0);
        self.magnitudes.fill(0.0);
        self.write = 0;
        self.centroid_hz = 0.0;
        self.peak_frequency_hz = 0.0;
    }

    fn parameters(&self) -> &'static [ParameterDescriptor] {
        &SPECTRUM_PARAMETERS
    }

    fn analyzer_measurement(&mut self, _: f64, _: usize) -> Option<AnalyzerMeasurement> {
        self.update_measurement();
        let size = self.window.len();
        let magnitude_count = self.magnitudes.len();
        let nyquist_bin = size / 2;
        let frequency = |output_bin: usize| {
            let dft_bin = if magnitude_count == 1 {
                0
            } else {
                output_bin * nyquist_bin / (magnitude_count - 1)
            };
            dft_bin as f32 * self.sample_rate as f32 / size.max(1) as f32
        };
        let bins: Vec<_> = self
            .magnitudes
            .iter()
            .copied()
            .enumerate()
            .map(|(index, magnitude)| SpectrumBin {
                frequency_hz: frequency(index),
                magnitude_dbfs: amplitude_dbfs(magnitude),
            })
            .collect();
        let peaks = self
            .magnitudes
            .iter()
            .copied()
            .enumerate()
            .max_by(|left, right| left.1.total_cmp(&right.1))
            .map(|(index, magnitude)| {
                vec![SpectralPeak {
                    frequency_hz: frequency(index),
                    magnitude_dbfs: amplitude_dbfs(magnitude),
                }]
            })
            .unwrap_or_default();
        Some(AnalyzerMeasurement::Spectrum(SpectrumMeasurement {
            bins,
            peaks,
            spectral_centroid_hz: self.centroid_hz,
        }))
    }
}

const SPECTRUM_PARAMETERS: [ParameterDescriptor; 2] = [
    ParameterDescriptor {
        id: "fft_size",
        name: "DFT Size",
        kind: ParameterKind::Integer {
            min: 32,
            max: 1_048_576,
        },
        unit: ParameterUnit::Samples,
        default: ParameterValue::Integer(1024),
        automatable: false,
        display_hint: Some("configuration; takes effect on prepare"),
    },
    ParameterDescriptor {
        id: "bins",
        name: "Output Bins",
        kind: ParameterKind::Integer {
            min: 2,
            max: 524_289,
        },
        unit: ParameterUnit::None,
        default: ParameterValue::Integer(128),
        automatable: false,
        display_hint: Some("linearly spans DC through Nyquist"),
    },
];

/// Ephemeral stereo image measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StereoMeasurement {
    pub mid_rms: f32,
    pub side_rms: f32,
    pub correlation: f32,
    pub width: f32,
}

/// Built-in `gaw.stereo_meter` analyzer.
#[derive(Debug, Default)]
pub struct StereoMeter {
    measurement: StereoMeasurement,
}

impl StereoMeter {
    pub fn measurement(&self) -> StereoMeasurement {
        self.measurement
    }
}

impl Analyzer for StereoMeter {
    fn prepare(&mut self, _: f64, _: usize, _: usize) {
        self.reset();
    }

    fn analyze(&mut self, input: &[&[f32]]) {
        if input.len() < 2 || input[0].is_empty() {
            self.measurement = StereoMeasurement::default();
            return;
        }
        let mut mid_energy = 0.0_f64;
        let mut side_energy = 0.0_f64;
        let mut left_energy = 0.0_f64;
        let mut right_energy = 0.0_f64;
        let mut product = 0.0_f64;
        for (&left, &right) in input[0].iter().zip(input[1]) {
            let mid = f64::from(left + right) * 0.5;
            let side = f64::from(left - right) * 0.5;
            mid_energy += mid * mid;
            side_energy += side * side;
            left_energy += f64::from(left) * f64::from(left);
            right_energy += f64::from(right) * f64::from(right);
            product += f64::from(left) * f64::from(right);
        }
        let count = input[0].len() as f64;
        self.measurement.mid_rms = (mid_energy / count).sqrt() as f32;
        self.measurement.side_rms = (side_energy / count).sqrt() as f32;
        self.measurement.correlation =
            (product / (left_energy * right_energy).sqrt().max(f64::EPSILON)) as f32;
        self.measurement.width = (side_energy / mid_energy.max(f64::EPSILON)).sqrt() as f32;
    }

    fn reset(&mut self) {
        self.measurement = StereoMeasurement::default();
    }

    fn analyzer_measurement(&mut self, _: f64, _: usize) -> Option<AnalyzerMeasurement> {
        let measurement = self.measurement();
        Some(AnalyzerMeasurement::StereoMeter(CoreStereoMeasurement {
            mid_level_dbfs: amplitude_dbfs(measurement.mid_rms),
            side_level_dbfs: amplitude_dbfs(measurement.side_rms),
            correlation: measurement.correlation,
            stereo_width: measurement.width,
        }))
    }
}

/// Unweighted energy measurements suitable for live diagnostics.
///
/// These values are dBFS-like mean-square levels. They are not K-weighted,
/// gated LUFS or standards-defined LRA and must not be presented as such.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EnergyMeasurement {
    pub block_level_dbfs: f32,
    pub smoothed_level_dbfs: f32,
    pub running_level_dbfs: f32,
    pub observed_smoothed_range_db: f32,
}

/// Built-in `gaw.energy_meter` unweighted running-energy primitive.
#[derive(Debug, Default)]
pub struct EnergyMeter {
    measurement: EnergyMeasurement,
    running_energy: f64,
    running_frames: u64,
    slow_energy: f64,
    minimum: f32,
    maximum: f32,
}

impl EnergyMeter {
    pub fn measurement(&self) -> EnergyMeasurement {
        self.measurement
    }

    fn to_dbfs(energy: f64) -> f32 {
        if energy <= 1.0e-12 {
            -120.0
        } else {
            (10.0 * energy.log10()) as f32
        }
    }
}

impl Analyzer for EnergyMeter {
    fn prepare(&mut self, _: f64, _: usize, _: usize) {
        self.reset();
    }

    fn analyze(&mut self, input: &[&[f32]]) {
        let frames = input.first().map_or(0, |channel| channel.len());
        if frames == 0 {
            return;
        }
        let mut sum = 0.0_f64;
        for frame in 0..frames {
            let mut frame_energy = 0.0;
            for channel in input.iter().take(2) {
                frame_energy += f64::from(channel[frame]) * f64::from(channel[frame]);
            }
            sum += frame_energy / input.len().clamp(1, 2) as f64;
        }
        let block_energy = sum / frames as f64;
        self.running_energy += sum;
        self.running_frames = self.running_frames.saturating_add(frames as u64);
        self.slow_energy = self.slow_energy * 0.85 + block_energy * 0.15;
        self.measurement.block_level_dbfs = Self::to_dbfs(block_energy);
        self.measurement.smoothed_level_dbfs = Self::to_dbfs(self.slow_energy);
        self.measurement.running_level_dbfs =
            Self::to_dbfs(self.running_energy / self.running_frames.max(1) as f64);
        self.minimum = self.minimum.min(self.measurement.smoothed_level_dbfs);
        self.maximum = self.maximum.max(self.measurement.smoothed_level_dbfs);
        self.measurement.observed_smoothed_range_db = (self.maximum - self.minimum).max(0.0);
    }

    fn reset(&mut self) {
        self.measurement = EnergyMeasurement {
            block_level_dbfs: -120.0,
            smoothed_level_dbfs: -120.0,
            running_level_dbfs: -120.0,
            observed_smoothed_range_db: 0.0,
        };
        self.running_energy = 0.0;
        self.running_frames = 0;
        self.slow_energy = 0.0;
        self.minimum = 0.0;
        self.maximum = -120.0;
    }
}

const LOUDNESS_FLOOR: f64 = -120.0;
const LOUDNESS_CEILING: f64 = 24.0;
const LOUDNESS_BIN_WIDTH: f64 = 0.01;
const LOUDNESS_BINS: usize =
    ((LOUDNESS_CEILING - LOUDNESS_FLOOR) / LOUDNESS_BIN_WIDTH) as usize + 1;

/// Configuration for the EBU R 128 loudness analyzer.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct LoudnessMeterConfig;

/// K-weighted EBU R 128 loudness measurements.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoudnessMeasurement {
    pub momentary_lufs: f32,
    pub short_term_lufs: f32,
    pub integrated_lufs: f32,
    pub loudness_range_lu: f32,
    pub momentary_valid: bool,
    pub short_term_valid: bool,
    pub integrated_valid: bool,
    /// EBU Tech 3341 requires LRA to be shown as unstable for the first minute.
    pub loudness_range_stable: bool,
}

impl Default for LoudnessMeasurement {
    fn default() -> Self {
        Self {
            momentary_lufs: -120.0,
            short_term_lufs: -120.0,
            integrated_lufs: -120.0,
            loudness_range_lu: 0.0,
            momentary_valid: false,
            short_term_valid: false,
            integrated_valid: false,
            loudness_range_stable: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct LoudnessBiquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
    z1: f64,
    z2: f64,
}

impl LoudnessBiquad {
    fn with_coefficients(b: [f64; 3], a: [f64; 3]) -> Self {
        Self {
            b0: b[0],
            b1: b[1],
            b2: b[2],
            a1: a[1],
            a2: a[2],
            ..Self::default()
        }
    }

    #[inline]
    fn process(&mut self, input: f64) -> f64 {
        let output = self.b0.mul_add(input, self.z1);
        self.z1 = self.b1.mul_add(input, self.z2) - self.a1 * output;
        self.z2 = self.b2 * input - self.a2 * output;
        output
    }

    fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// Built-in `gaw.loudness_meter` analyzer.
///
/// The callback performs only K-weighting and bounded accumulation. Call
/// [`Self::update_measurement`] off the audio thread to refresh integrated
/// loudness and LRA from the fixed-resolution (0.01 LU) histograms.
#[derive(Debug)]
pub struct LoudnessMeter {
    pub config: LoudnessMeterConfig,
    measurement: LoudnessMeasurement,
    filters: [[LoudnessBiquad; 2]; 2],
    channels: usize,
    sample_rate: f64,
    hop_frames: usize,
    hop_position: usize,
    hop_energy: f64,
    hop_ring: [f64; 30],
    hop_count: usize,
    hop_write: usize,
    total_frames: u64,
    integrated_counts: Vec<u64>,
    integrated_energy: Vec<f64>,
    lra_counts: Vec<u64>,
    lra_energy: Vec<f64>,
}

impl Default for LoudnessMeter {
    fn default() -> Self {
        Self {
            config: LoudnessMeterConfig,
            measurement: LoudnessMeasurement::default(),
            filters: [[LoudnessBiquad::default(); 2]; 2],
            channels: 0,
            sample_rate: 48_000.0,
            hop_frames: 4_800,
            hop_position: 0,
            hop_energy: 0.0,
            hop_ring: [0.0; 30],
            hop_count: 0,
            hop_write: 0,
            total_frames: 0,
            integrated_counts: Vec::new(),
            integrated_energy: Vec::new(),
            lra_counts: Vec::new(),
            lra_energy: Vec::new(),
        }
    }
}

impl LoudnessMeter {
    pub fn measurement(&self) -> LoudnessMeasurement {
        self.measurement
    }

    /// Refresh integrated loudness and LRA. This bounded histogram scan is
    /// intentionally separated from [`Analyzer::analyze`].
    pub fn update_measurement(&mut self) {
        let (absolute_count, absolute_energy) =
            histogram_sum_above(&self.integrated_counts, &self.integrated_energy, -70.0);
        if absolute_count > 0 {
            let absolute_loudness = loudness(absolute_energy / absolute_count as f64);
            let gate = (absolute_loudness - 10.0).max(-70.0);
            let (count, energy) =
                histogram_sum_above(&self.integrated_counts, &self.integrated_energy, gate);
            if count > 0 {
                self.measurement.integrated_lufs = loudness(energy / count as f64) as f32;
                self.measurement.integrated_valid = true;
            }
        }

        let (absolute_count, absolute_energy) =
            histogram_sum_above(&self.lra_counts, &self.lra_energy, -70.0);
        if absolute_count > 0 {
            let gate = (loudness(absolute_energy / absolute_count as f64) - 20.0).max(-70.0);
            let first_bin = loudness_bin(gate);
            let gated_count: u64 = self.lra_counts[first_bin..].iter().sum();
            if gated_count > 0 {
                let low = histogram_percentile(&self.lra_counts, first_bin, gated_count, 0.10);
                let high = histogram_percentile(&self.lra_counts, first_bin, gated_count, 0.95);
                self.measurement.loudness_range_lu = (high - low).max(0.0) as f32;
            }
        }
        self.measurement.loudness_range_stable =
            self.total_frames as f64 >= self.sample_rate * 60.0;
    }

    fn configure_filters(&mut self) {
        // Parameters from the ITU reference filters, transformed to the
        // prepared sample rate by the same bilinear designs.
        let shelf_k = (core::f64::consts::PI * 1_681.974_450_955_533 / self.sample_rate).tan();
        let shelf_q = 0.707_175_236_955_419_6;
        let vh = 10.0_f64.powf(3.999_843_853_97 / 20.0);
        let vb = vh.powf(0.499_666_774_154_541_6);
        let a0 = 1.0 + shelf_k / shelf_q + shelf_k * shelf_k;
        let shelf_b = [
            (vh + vb * shelf_k / shelf_q + shelf_k * shelf_k) / a0,
            2.0 * (shelf_k * shelf_k - vh) / a0,
            (vh - vb * shelf_k / shelf_q + shelf_k * shelf_k) / a0,
        ];
        let shelf_a = [
            1.0,
            2.0 * (shelf_k * shelf_k - 1.0) / a0,
            (1.0 - shelf_k / shelf_q + shelf_k * shelf_k) / a0,
        ];

        let high_pass_k = (core::f64::consts::PI * 38.135_470_876_024_44 / self.sample_rate).tan();
        let high_pass_q = 0.500_327_037_323_877_3;
        let a0 = 1.0 + high_pass_k / high_pass_q + high_pass_k * high_pass_k;
        let high_pass_b = [1.0 / a0, -2.0 / a0, 1.0 / a0];
        let high_pass_a = [
            1.0,
            2.0 * (high_pass_k * high_pass_k - 1.0) / a0,
            (1.0 - high_pass_k / high_pass_q + high_pass_k * high_pass_k) / a0,
        ];
        for channel in 0..2 {
            self.filters[channel][0] = LoudnessBiquad::with_coefficients(shelf_b, shelf_a);
            self.filters[channel][1] = LoudnessBiquad::with_coefficients(high_pass_b, high_pass_a);
        }
    }

    fn finish_hop(&mut self) {
        let mean = self.hop_energy / self.hop_frames as f64;
        self.hop_ring[self.hop_write] = mean;
        self.hop_write = (self.hop_write + 1) % self.hop_ring.len();
        self.hop_count = self.hop_count.saturating_add(1);
        self.hop_energy = 0.0;
        self.hop_position = 0;

        if self.hop_count >= 4 {
            let energy = self.recent_hop_energy(4);
            self.measurement.momentary_lufs = loudness(energy) as f32;
            self.measurement.momentary_valid = true;
            histogram_add(
                &mut self.integrated_counts,
                &mut self.integrated_energy,
                energy,
            );
        }
        if self.hop_count >= 30 {
            let energy = self.recent_hop_energy(30);
            self.measurement.short_term_lufs = loudness(energy) as f32;
            self.measurement.short_term_valid = true;
            histogram_add(&mut self.lra_counts, &mut self.lra_energy, energy);
        }
    }

    fn recent_hop_energy(&self, count: usize) -> f64 {
        let mut sum = 0.0;
        for offset in 0..count {
            let index = (self.hop_write + self.hop_ring.len() - 1 - offset) % self.hop_ring.len();
            sum += self.hop_ring[index];
        }
        sum / count as f64
    }
}

impl Analyzer for LoudnessMeter {
    fn prepare(&mut self, sample_rate: f64, _: usize, channels: usize) {
        self.sample_rate = sample_rate;
        self.channels = channels.clamp(1, 2);
        self.hop_frames = (sample_rate * 0.1).round().max(1.0) as usize;
        self.integrated_counts.resize(LOUDNESS_BINS, 0);
        self.integrated_energy.resize(LOUDNESS_BINS, 0.0);
        self.lra_counts.resize(LOUDNESS_BINS, 0);
        self.lra_energy.resize(LOUDNESS_BINS, 0.0);
        self.configure_filters();
        self.reset();
    }

    fn analyze(&mut self, input: &[&[f32]]) {
        let frames = input.first().map_or(0, |channel| channel.len());
        for frame in 0..frames {
            let mut frame_energy = 0.0;
            for (channel, input_channel) in input.iter().take(self.channels).enumerate() {
                let shelf_output =
                    self.filters[channel][0].process(f64::from(input_channel[frame]));
                let filtered = self.filters[channel][1].process(shelf_output);
                frame_energy += filtered * filtered;
            }
            self.hop_energy += frame_energy;
            self.hop_position += 1;
            self.total_frames = self.total_frames.saturating_add(1);
            if self.hop_position == self.hop_frames {
                self.finish_hop();
            }
        }
    }

    fn reset(&mut self) {
        self.measurement = LoudnessMeasurement::default();
        for channel in &mut self.filters {
            for filter in channel {
                filter.reset();
            }
        }
        self.hop_position = 0;
        self.hop_energy = 0.0;
        self.hop_ring = [0.0; 30];
        self.hop_count = 0;
        self.hop_write = 0;
        self.total_frames = 0;
        self.integrated_counts.fill(0);
        self.integrated_energy.fill(0.0);
        self.lra_counts.fill(0);
        self.lra_energy.fill(0.0);
    }

    fn analyzer_measurement(&mut self, _: f64, _: usize) -> Option<AnalyzerMeasurement> {
        self.update_measurement();
        let measurement = self.measurement();
        Some(AnalyzerMeasurement::LoudnessMeter(
            CoreLoudnessMeasurement {
                momentary_lufs: measurement.momentary_lufs,
                short_term_lufs: measurement.short_term_lufs,
                integrated_lufs: measurement.integrated_lufs,
                loudness_range_lu: measurement.loudness_range_lu,
            },
        ))
    }
}

fn loudness(energy: f64) -> f64 {
    if energy <= 1.0e-20 {
        LOUDNESS_FLOOR
    } else {
        -0.691 + 10.0 * energy.log10()
    }
}

fn loudness_bin(level: f64) -> usize {
    (((level.clamp(LOUDNESS_FLOOR, LOUDNESS_CEILING) - LOUDNESS_FLOOR) / LOUDNESS_BIN_WIDTH).round()
        as usize)
        .min(LOUDNESS_BINS - 1)
}

fn histogram_add(counts: &mut [u64], energies: &mut [f64], energy: f64) {
    let bin = loudness_bin(loudness(energy));
    counts[bin] = counts[bin].saturating_add(1);
    energies[bin] += energy;
}

fn histogram_sum_above(counts: &[u64], energies: &[f64], gate: f64) -> (u64, f64) {
    let first = loudness_bin(gate);
    (counts[first..].iter().sum(), energies[first..].iter().sum())
}

fn histogram_percentile(counts: &[u64], first: usize, total: u64, percentile: f64) -> f64 {
    let target = ((total.saturating_sub(1)) as f64 * percentile).round() as u64;
    let mut seen = 0_u64;
    for (index, count) in counts.iter().copied().enumerate().skip(first) {
        if target < seen.saturating_add(count) {
            return LOUDNESS_FLOOR + index as f64 * LOUDNESS_BIN_WIDTH;
        }
        seen = seen.saturating_add(count);
    }
    LOUDNESS_CEILING
}

/// Fundamental pitch measurement.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TunerMeasurement {
    pub frequency_hz: f32,
    pub midi_note: i16,
    pub note_name: &'static str,
    pub cents_offset: f32,
    pub confidence: f32,
}

impl Default for TunerMeasurement {
    fn default() -> Self {
        Self {
            frequency_hz: 0.0,
            midi_note: 0,
            note_name: "--",
            cents_offset: 0.0,
            confidence: 0.0,
        }
    }
}

/// Built-in `gaw.tuner` bounded autocorrelation primitive.
#[derive(Debug)]
pub struct Tuner {
    sample_rate: f64,
    capture: Vec<f32>,
    write: usize,
    measurement: TunerMeasurement,
}

impl Default for Tuner {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            capture: Vec::new(),
            write: 0,
            measurement: TunerMeasurement::default(),
        }
    }
}

impl Tuner {
    pub fn measurement(&self) -> TunerMeasurement {
        self.measurement
    }

    /// Refresh the pitch estimate from the captured ring.
    ///
    /// This performs bounded autocorrelation and must be called off the
    /// real-time audio thread, typically through [`AnalyzerTap::analyzer_mut`].
    pub fn update_measurement(&mut self) {
        self.calculate();
    }

    fn calculate(&mut self) {
        let min_lag = (self.sample_rate / 1_200.0).max(1.0) as usize;
        let max_lag = (self.sample_rate / 40.0) as usize;
        let max_lag = max_lag.min(self.capture.len() / 2);
        let mut best_lag = 0;
        let mut best = 0.0_f64;
        let mut previous_two = -1.0_f64;
        let mut previous = -1.0_f64;
        for lag in min_lag..=max_lag {
            let mut correlation = 0.0_f64;
            let mut first_energy = 0.0_f64;
            let mut second_energy = 0.0_f64;
            for index in 0..self.capture.len() / 2 {
                let first = f64::from(self.capture[(self.write + index) % self.capture.len()]);
                let second =
                    f64::from(self.capture[(self.write + index + lag) % self.capture.len()]);
                correlation += first * second;
                first_energy += first * first;
                second_energy += second * second;
            }
            let normalized = correlation / (first_energy * second_energy).sqrt().max(1.0e-12);
            if normalized > best {
                best = normalized;
                best_lag = lag;
            }
            // The first strong local maximum is the fundamental; selecting the
            // global maximum would commonly lock to an integer subharmonic.
            if lag > min_lag + 1
                && previous > previous_two
                && previous >= normalized
                && previous > 0.65
            {
                best = previous;
                best_lag = lag - 1;
                break;
            }
            previous_two = previous;
            previous = normalized;
        }
        if best_lag == 0 || best <= 0.0 {
            self.measurement = TunerMeasurement::default();
            return;
        }
        let frequency = self.sample_rate / best_lag as f64;
        let midi = 69.0 + 12.0 * (frequency / 440.0).log2();
        let rounded = midi.round();
        let midi_note = rounded as i16;
        self.measurement = TunerMeasurement {
            frequency_hz: frequency as f32,
            midi_note,
            note_name: NOTE_NAMES[usize::from(midi_note.rem_euclid(12) as u8)],
            cents_offset: ((midi - rounded) * 100.0) as f32,
            confidence: best.clamp(0.0, 1.0) as f32,
        };
    }
}

impl Analyzer for Tuner {
    fn prepare(&mut self, sample_rate: f64, maximum_block_size: usize, _: usize) {
        self.sample_rate = sample_rate;
        self.capture.resize(
            maximum_block_size.max((sample_rate / 40.0) as usize * 2),
            0.0,
        );
        self.reset();
    }

    fn analyze(&mut self, input: &[&[f32]]) {
        let Some(first) = input.first() else { return };
        for frame in 0..first.len() {
            self.capture[self.write] = if input.len() == 1 {
                first[frame]
            } else {
                (first[frame] + input[1][frame]) * 0.5
            };
            self.write = (self.write + 1) % self.capture.len();
        }
    }

    fn reset(&mut self) {
        self.capture.fill(0.0);
        self.write = 0;
        self.measurement = TunerMeasurement::default();
    }

    fn analyzer_measurement(&mut self, _: f64, _: usize) -> Option<AnalyzerMeasurement> {
        self.update_measurement();
        let measurement = self.measurement();
        Some(AnalyzerMeasurement::Tuner(CoreTunerMeasurement {
            fundamental_hz: measurement.frequency_hz,
            note_name: measurement.note_name.to_owned(),
            cents_offset: measurement.cents_offset,
            confidence: measurement.confidence,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepare_spec(layout: AudioLayout, maximum_block_size: usize) -> PrepareSpec {
        PrepareSpec {
            sample_rate: 48_000.0,
            max_block_size: maximum_block_size,
            input_layout: layout,
            tempo_bpm: 120.0,
        }
    }

    #[test]
    fn level_meter_reports_peak_and_rms() {
        let mut meter = LevelMeter::default();
        meter.prepare(48_000.0, 4, 1);
        meter.analyze(&[&[1.0, -1.0, 0.0, 0.0]]);
        let result = meter.measurement();
        assert_eq!(result.sample_peak[0], 1.0);
        assert!((result.rms[0] - std::f32::consts::FRAC_1_SQRT_2).abs() < 1.0e-6);
        assert!(result.sample_clipped[0]);
    }

    #[test]
    fn level_meter_reports_bs1770_intersample_peak() {
        let mut meter = LevelMeter::default();
        meter.prepare(48_000.0, 256, 1);
        let input: Vec<f32> = (0..256)
            .map(|frame| (TAU * 0.24 * (frame as f32 + 0.5)).sin())
            .collect();
        meter.analyze(&[&input]);
        let result = meter.measurement();
        assert!(result.true_peak[0] > result.sample_peak[0]);
        assert_eq!(result.inter_sample_peak_estimate, result.true_peak);
    }

    #[test]
    fn energy_meter_reports_unweighted_dbfs_without_lufs_claims() {
        let mut meter = EnergyMeter::default();
        meter.prepare(48_000.0, 16, 1);
        meter.analyze(&[&[0.5; 16]]);
        let result = meter.measurement();
        assert!((result.block_level_dbfs + 6.020_6).abs() < 1.0e-3);
        assert!((result.running_level_dbfs + 6.020_6).abs() < 1.0e-3);
    }

    #[test]
    fn loudness_meter_matches_stereo_reference_tone() {
        let sample_rate = 48_000.0;
        let amplitude = 10.0_f32.powf(-23.0 / 20.0);
        let input: Vec<f32> = (0..sample_rate as usize * 5)
            .map(|frame| (TAU * 997.0 * frame as f32 / sample_rate as f32).sin() * amplitude)
            .collect();
        let mut meter = LoudnessMeter::default();
        meter.prepare(sample_rate, 1_024, 2);
        for block in input.chunks(257) {
            meter.analyze(&[block, block]);
        }
        meter.update_measurement();
        let result = meter.measurement();
        assert!(result.momentary_valid && result.short_term_valid && result.integrated_valid);
        assert!((result.momentary_lufs + 23.0).abs() < 0.1, "{result:?}");
        assert!((result.short_term_lufs + 23.0).abs() < 0.1, "{result:?}");
        assert!((result.integrated_lufs + 23.0).abs() < 0.1, "{result:?}");
    }

    #[test]
    fn loudness_range_uses_ebu_gating_and_percentiles() {
        let sample_rate = 8_000.0;
        let segment_frames = sample_rate as usize * 20;
        let mut first = Vec::with_capacity(segment_frames);
        let mut second = Vec::with_capacity(segment_frames);
        for frame in 0..segment_frames {
            let sine = (TAU * 997.0 * frame as f32 / sample_rate as f32).sin();
            first.push(sine * 10.0_f32.powf(-20.0 / 20.0));
            second.push(sine * 10.0_f32.powf(-30.0 / 20.0));
        }
        let mut meter = LoudnessMeter::default();
        meter.prepare(sample_rate, 512, 2);
        for block in first.chunks(251) {
            meter.analyze(&[block, block]);
        }
        for block in second.chunks(251) {
            meter.analyze(&[block, block]);
        }
        meter.update_measurement();
        let result = meter.measurement();
        assert!((result.loudness_range_lu - 10.0).abs() <= 1.0, "{result:?}");
        assert!(!result.loudness_range_stable);
    }

    #[test]
    fn integrated_loudness_applies_absolute_and_relative_gates() {
        let sample_rate = 48_000.0;
        let mut meter = LoudnessMeter::default();
        meter.prepare(sample_rate, 512, 1);
        let mut frame = 0_u64;
        for (seconds, target_lufs) in [(10, -36.0_f32), (60, -23.0), (10, -36.0)] {
            let amplitude = 10.0_f32.powf((target_lufs + 3.01) / 20.0);
            for _ in 0..seconds * sample_rate as usize / 1_200 {
                let mut block = [0.0; 1_200];
                for sample in &mut block {
                    *sample = (TAU * 997.0 * frame as f32 / sample_rate as f32).sin() * amplitude;
                    frame += 1;
                }
                meter.analyze(&[&block]);
            }
        }
        meter.update_measurement();
        let result = meter.measurement();
        assert!(result.integrated_valid);
        assert!((result.integrated_lufs + 23.0).abs() < 0.1, "{result:?}");
        assert!(result.loudness_range_stable);
    }

    #[test]
    fn spectrum_spans_dc_through_nyquist_and_updates_off_callback() {
        let mut spectrum = SpectrumAnalyzer {
            config: SpectrumConfig {
                fft_size: 64,
                bins: 5,
            },
            ..SpectrumAnalyzer::default()
        };
        spectrum.prepare(48_000.0, 64, 1);
        let nyquist: Vec<f32> = (0..64)
            .map(|frame| if frame % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        spectrum.analyze(&[&nyquist]);
        assert_eq!(spectrum.peak_frequency_hz(), 0.0);
        spectrum.update_measurement();
        assert!((spectrum.peak_frequency_hz() - 24_000.0).abs() < 1.0);
    }

    #[test]
    fn stereo_meter_identifies_mono_and_sides() {
        let mut meter = StereoMeter::default();
        let left = [0.5; 16];
        let right = [0.5; 16];
        meter.analyze(&[&left, &right]);
        assert!(meter.measurement().correlation > 0.99);
        assert_eq!(meter.measurement().side_rms, 0.0);
        let inverted = [-0.5; 16];
        meter.analyze(&[&left, &inverted]);
        assert!(meter.measurement().correlation < -0.99);
    }

    #[test]
    fn tuner_finds_a440() {
        let sample_rate = 8_000.0;
        let input: Vec<f32> = (0..800)
            .map(|frame| (TAU * 440.0 * frame as f32 / sample_rate as f32).sin())
            .collect();
        let mut tuner = Tuner::default();
        tuner.prepare(sample_rate, 800, 1);
        tuner.analyze(&[&input]);
        assert_eq!(tuner.measurement().frequency_hz, 0.0);
        tuner.update_measurement();
        assert!((tuner.measurement().frequency_hz - 440.0).abs() < 10.0);
        assert_eq!(tuner.measurement().midi_note, 69);
        assert_eq!(tuner.measurement().note_name, "A");
    }

    #[test]
    fn analyzer_tap_is_transparent_in_mono_and_rejects_events_without_allocating_state() {
        let mut tap = AnalyzerTap::<Oscilloscope>::oscilloscope();
        tap.prepare(prepare_spec(AudioLayout::Mono, 4)).unwrap();
        assert_eq!(tap.latency_frames(), 0);
        assert_eq!(tap.tail_frames(), 0);
        assert_eq!(tap.parameters(), &OSCILLOSCOPE_PARAMETERS);

        let input = [0.25, -0.5, 0.75, -1.0];
        let mut output = [0.0; 4];
        tap.process(
            &[&input],
            &mut [&mut output],
            &[],
            ProcessContext::default(),
        )
        .unwrap();
        assert_eq!(input, output);
        assert!(tap.analyzer().zero_crossings() > 0);

        let event = ParameterEvent::new(0, "capture_frames", ParameterValue::Integer(128));
        assert!(matches!(
            tap.process(
                &[&input],
                &mut [&mut output],
                &[event],
                ProcessContext::default(),
            ),
            Err(ProcessError::AnalyzerEventUnsupported)
        ));
        tap.seek(10_000);
        assert_eq!(tap.analyzer().zero_crossings(), 0);
    }

    #[test]
    fn processor_measurement_accessor_reports_actual_level_without_changing_audio() {
        let mut tap = AnalyzerTap::<LevelMeter>::level_meter();
        tap.prepare(prepare_spec(AudioLayout::Mono, 4)).unwrap();
        let input = [0.5, -0.5, 0.5, -0.5];
        let mut output = [0.0; 4];
        tap.process(
            &[&input],
            &mut [&mut output],
            &[],
            ProcessContext::default(),
        )
        .unwrap();
        let Some(AnalyzerMeasurement::LevelMeter(measurement)) =
            Processor::analyzer_measurement(&mut tap)
        else {
            panic!("level meter measurement was not exported");
        };
        assert_eq!(output, input);
        assert_eq!(measurement.sample_peak_dbfs.len(), 1);
        assert!((measurement.sample_peak_dbfs[0] + 6.020_6).abs() < 1.0e-3);
        assert!((measurement.rms_dbfs[0] + 6.020_6).abs() < 1.0e-3);
        assert_eq!(measurement.clipping, [false]);
    }

    #[test]
    fn heavy_measurements_are_refreshed_when_exported() {
        let mut spectrum = SpectrumAnalyzer {
            config: SpectrumConfig {
                fft_size: 64,
                bins: 5,
            },
            ..SpectrumAnalyzer::default()
        };
        spectrum.prepare(48_000.0, 64, 1);
        let nyquist: Vec<f32> = (0..64)
            .map(|frame| if frame % 2 == 0 { 1.0 } else { -1.0 })
            .collect();
        spectrum.analyze(&[&nyquist]);
        let Some(AnalyzerMeasurement::Spectrum(spectrum)) =
            Analyzer::analyzer_measurement(&mut spectrum, 48_000.0, 1)
        else {
            panic!("spectrum measurement was not exported");
        };
        assert_eq!(spectrum.bins.len(), 5);
        assert!((spectrum.peaks[0].frequency_hz - 24_000.0).abs() < 1.0);

        let mut tuner = Tuner::default();
        tuner.prepare(8_000.0, 800, 1);
        let tone: Vec<f32> = (0..800)
            .map(|frame| (TAU * 440.0 * frame as f32 / 8_000.0).sin())
            .collect();
        tuner.analyze(&[&tone]);
        let Some(AnalyzerMeasurement::Tuner(tuner)) =
            Analyzer::analyzer_measurement(&mut tuner, 8_000.0, 1)
        else {
            panic!("tuner measurement was not exported");
        };
        assert!((tuner.fundamental_hz - 440.0).abs() < 10.0);
        assert_eq!(tuner.note_name, "A");

        let mut loudness = LoudnessMeter::default();
        loudness.prepare(8_000.0, 512, 1);
        let tone: Vec<f32> = (0..8_000 * 5)
            .map(|frame| {
                (TAU * 997.0 * frame as f32 / 8_000.0).sin() * 10.0_f32.powf((-23.0 + 3.01) / 20.0)
            })
            .collect();
        for block in tone.chunks(251) {
            loudness.analyze(&[block]);
        }
        let Some(AnalyzerMeasurement::LoudnessMeter(loudness)) =
            Analyzer::analyzer_measurement(&mut loudness, 8_000.0, 1)
        else {
            panic!("loudness measurement was not exported");
        };
        assert!(loudness.integrated_lufs > -60.0);
    }

    #[test]
    fn oscilloscope_and_stereo_measurements_export_canonical_values() {
        let mut scope = Oscilloscope::default();
        scope.prepare(48_000.0, 4, 1);
        let waveform = [-0.5, 0.5, -0.25, 0.25];
        scope.analyze(&[&waveform]);
        let Some(AnalyzerMeasurement::Oscilloscope(scope)) =
            Analyzer::analyzer_measurement(&mut scope, 48_000.0, 1)
        else {
            panic!("oscilloscope measurement was not exported");
        };
        assert_eq!(scope.sample_rate_hz, 48_000);
        assert!(scope.channel_samples[0].ends_with(&waveform));
        assert!(scope.zero_crossing_rate_hz[0] > 0.0);

        let mut stereo = StereoMeter::default();
        stereo.prepare(48_000.0, 16, 2);
        stereo.analyze(&[&[0.5; 16], &[0.5; 16]]);
        let Some(AnalyzerMeasurement::StereoMeter(stereo)) =
            Analyzer::analyzer_measurement(&mut stereo, 48_000.0, 2)
        else {
            panic!("stereo measurement was not exported");
        };
        assert!((stereo.mid_level_dbfs + 6.020_6).abs() < 1.0e-3);
        assert_eq!(stereo.side_level_dbfs, -120.0);
        assert!(stereo.correlation > 0.99);
        assert_eq!(stereo.stereo_width, 0.0);
    }
}
