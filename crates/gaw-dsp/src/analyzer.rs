//! Allocation-free analyzer primitives. Measurements are ephemeral and audio is never modified.

use std::f32::consts::TAU;

use serde::{Deserialize, Serialize};

use crate::contract::{
    AudioLayout, MONO_AND_STEREO, PrepareSpec, ProcessContext, ProcessError, Processor,
    copy_or_map_bypass, validate_process_io,
};
use crate::parameter::{ParameterDescriptor, ParameterEvent};
use crate::parameter::{ParameterKind, ParameterUnit, ParameterValue};

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
}

/// A pass-through processor adapter that places an analyzer in an effect stack.
#[derive(Debug)]
pub struct AnalyzerTap<A> {
    type_id: &'static str,
    analyzer: A,
    layout: AudioLayout,
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
}

/// Peak, RMS, hold, and clipping measurements for up to two channels.
///
/// `inter_sample_peak_estimate` uses four-point cubic interpolation. It can
/// reveal likely inter-sample overs, but is intentionally not labelled true
/// peak: it is not the oversampling filter specified by ITU-R BS.1770.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LevelMeasurement {
    pub sample_peak: [f32; 2],
    pub inter_sample_peak_estimate: [f32; 2],
    pub rms: [f32; 2],
    pub peak_hold: [f32; 2],
    pub sample_clipped: [bool; 2],
}

/// Built-in `gaw.level_meter` analyzer.
#[derive(Debug, Default)]
pub struct LevelMeter {
    measurement: LevelMeasurement,
    history: [[f32; 3]; 2],
    history_len: [usize; 2],
}

impl LevelMeter {
    pub fn measurement(&self) -> LevelMeasurement {
        self.measurement
    }

    fn cubic(p0: f32, p1: f32, p2: f32, p3: f32, position: f32) -> f32 {
        let a = 2.0 * p1;
        let b = -p0 + p2;
        let c = 2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3;
        let d = -p0 + 3.0 * p1 - 3.0 * p2 + p3;
        0.5 * (a + position * (b + position * (c + position * d)))
    }
}

impl Analyzer for LevelMeter {
    fn prepare(&mut self, _: f64, _: usize, _: usize) {
        self.reset();
    }

    fn analyze(&mut self, input: &[&[f32]]) {
        for (channel_index, channel) in input.iter().take(2).enumerate() {
            let mut peak = 0.0_f32;
            let mut inter_sample_peak = peak;
            let mut energy = 0.0_f64;
            for &sample in *channel {
                peak = peak.max(sample.abs());
                energy += f64::from(sample) * f64::from(sample);
                if self.history_len[channel_index] == 3 {
                    let [p0, p1, p2] = self.history[channel_index];
                    for phase in 0..=4 {
                        let position = phase as f32 * 0.25;
                        inter_sample_peak =
                            inter_sample_peak.max(Self::cubic(p0, p1, p2, sample, position).abs());
                    }
                }
                self.history[channel_index].rotate_left(1);
                self.history[channel_index][2] = sample;
                self.history_len[channel_index] = (self.history_len[channel_index] + 1).min(3);
            }
            self.measurement.sample_peak[channel_index] = peak;
            self.measurement.inter_sample_peak_estimate[channel_index] =
                inter_sample_peak.max(peak);
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
        self.history = [[0.0; 3]; 2];
        self.history_len = [0; 2];
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
    }

    fn reset(&mut self) {
        self.waveform.fill(0.0);
        self.write = 0;
        self.zero_crossings = 0;
    }

    fn parameters(&self) -> &'static [ParameterDescriptor] {
        &OSCILLOSCOPE_PARAMETERS
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
    fn level_meter_labels_cubic_intersample_result_as_estimate() {
        let mut meter = LevelMeter::default();
        meter.prepare(48_000.0, 4, 1);
        meter.analyze(&[&[0.0, 1.0, 1.0, 0.0]]);
        let result = meter.measurement();
        assert_eq!(result.sample_peak[0], 1.0);
        assert!(result.inter_sample_peak_estimate[0] > result.sample_peak[0]);
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
}
