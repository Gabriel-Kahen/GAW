//! Control-plane adaptation of canonical analyzer settings and measurements.
//!
//! These processors preserve the audio while applying the project model's
//! measurement windows, display ranges, and reset semantics.

use std::collections::VecDeque;

use gaw_dsp::{AudioLayout as DspLayout, PrepareSpec, ProcessContext, Processor as DspProcessor};

const ANALYZER_NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

#[derive(Clone, Debug)]
pub(super) enum CanonicalAnalyzerConfig {
    Level(gaw_core::LevelMeterParameters),
    Loudness(gaw_core::LoudnessMeterParameters),
    Spectrum(gaw_core::SpectrumParameters),
    Oscilloscope(gaw_core::OscilloscopeParameters),
    Stereo(gaw_core::StereoMeterParameters),
    Tuner(gaw_core::TunerParameters),
}

#[derive(Clone, Copy, Debug, Default)]
struct LoudnessEnergyBlock {
    energy: f64,
    frames: usize,
}

#[derive(Debug, Default)]
enum CanonicalAnalyzerState {
    #[default]
    Empty,
    Level {
        samples: Vec<Vec<f32>>,
        write: usize,
        filled: usize,
        held_peak: [f32; 2],
        held_until: [u64; 2],
        latest_frame: u64,
    },
    Loudness {
        blocks: VecDeque<LoudnessEnergyBlock>,
        frames: usize,
        maximum_frames: usize,
    },
    Spectrum {
        capture: Vec<f32>,
        write: usize,
        previous_dbfs: Vec<f32>,
    },
    Oscilloscope {
        capture: Vec<f32>,
        write: usize,
        filled: usize,
        crossings: u64,
        analyzed_frames: u64,
    },
    Stereo {
        left: Vec<f32>,
        right: Vec<f32>,
        write: usize,
        filled: usize,
    },
    Tuner {
        capture: Vec<f32>,
        write: usize,
        filled: usize,
    },
}

pub(super) struct CanonicalAnalyzerProcessor {
    inner: Box<dyn DspProcessor>,
    config: CanonicalAnalyzerConfig,
    state: CanonicalAnalyzerState,
    sample_rate: f64,
}

impl CanonicalAnalyzerProcessor {
    pub(super) fn new(inner: Box<dyn DspProcessor>, config: CanonicalAnalyzerConfig) -> Self {
        Self {
            inner,
            config,
            state: CanonicalAnalyzerState::Empty,
            sample_rate: 48_000.0,
        }
    }

    pub(super) fn duration_frames(sample_rate: f64, milliseconds: f32) -> usize {
        (sample_rate * f64::from(milliseconds) / 1_000.0)
            .round()
            .max(1.0) as usize
    }

    fn dbfs(amplitude: f32) -> f32 {
        if amplitude <= 1.0e-6 {
            -120.0
        } else {
            20.0 * amplitude.log10()
        }
    }

    fn initialize_state(&mut self, spec: PrepareSpec) {
        self.sample_rate = spec.sample_rate;
        self.state = match &self.config {
            CanonicalAnalyzerConfig::Level(parameters) => {
                let frames = Self::duration_frames(spec.sample_rate, parameters.window_ms);
                CanonicalAnalyzerState::Level {
                    samples: vec![vec![0.0; frames]; spec.input_layout.channels()],
                    write: 0,
                    filled: 0,
                    held_peak: [0.0; 2],
                    held_until: [0; 2],
                    latest_frame: 0,
                }
            }
            CanonicalAnalyzerConfig::Loudness(parameters) => {
                let maximum_frames = (spec.sample_rate * f64::from(parameters.integration_seconds))
                    .round()
                    .max(1.0) as usize;
                let capacity = maximum_frames
                    .div_ceil(spec.max_block_size)
                    .saturating_add(1);
                CanonicalAnalyzerState::Loudness {
                    blocks: VecDeque::with_capacity(capacity),
                    frames: 0,
                    maximum_frames,
                }
            }
            CanonicalAnalyzerConfig::Spectrum(parameters) => CanonicalAnalyzerState::Spectrum {
                capture: vec![0.0; fft_size(parameters.fft_size)],
                write: 0,
                previous_dbfs: Vec::new(),
            },
            CanonicalAnalyzerConfig::Oscilloscope(parameters) => {
                CanonicalAnalyzerState::Oscilloscope {
                    capture: vec![
                        0.0;
                        Self::duration_frames(spec.sample_rate, parameters.window_ms)
                    ],
                    write: 0,
                    filled: 0,
                    crossings: 0,
                    analyzed_frames: 0,
                }
            }
            CanonicalAnalyzerConfig::Stereo(parameters) => {
                let frames = Self::duration_frames(spec.sample_rate, parameters.window_ms);
                CanonicalAnalyzerState::Stereo {
                    left: vec![0.0; frames],
                    right: vec![0.0; frames],
                    write: 0,
                    filled: 0,
                }
            }
            CanonicalAnalyzerConfig::Tuner(parameters) => {
                let frames = (spec.sample_rate / f64::from(parameters.minimum_hz) * 2.0)
                    .ceil()
                    .max(spec.max_block_size as f64) as usize;
                CanonicalAnalyzerState::Tuner {
                    capture: vec![0.0; frames],
                    write: 0,
                    filled: 0,
                }
            }
        };
    }

    fn analyze_configuration(&mut self, input: &[&[f32]], context: ProcessContext) {
        match (&self.config, &mut self.state) {
            (
                CanonicalAnalyzerConfig::Level(parameters),
                CanonicalAnalyzerState::Level {
                    samples,
                    write,
                    filled,
                    held_peak,
                    held_until,
                    latest_frame,
                },
            ) => {
                let hold_frames = Self::duration_frames(self.sample_rate, parameters.peak_hold_ms)
                    .saturating_sub(1) as u64;
                let frames = input.first().map_or(0, |channel| channel.len());
                for frame in 0..frames {
                    let absolute_frame = context.absolute_frame.saturating_add(frame as u64);
                    for (channel_index, channel) in input.iter().take(2).enumerate() {
                        let amplitude = channel[frame].abs();
                        if absolute_frame > held_until[channel_index]
                            || amplitude >= held_peak[channel_index]
                        {
                            held_peak[channel_index] = amplitude;
                            held_until[channel_index] = absolute_frame.saturating_add(hold_frames);
                        }
                        samples[channel_index][*write] = channel[frame];
                    }
                    *write = (*write + 1) % samples[0].len();
                    *filled = (*filled + 1).min(samples[0].len());
                }
                *latest_frame = context.absolute_frame.saturating_add(frames as u64);
            }
            (
                CanonicalAnalyzerConfig::Loudness(_),
                CanonicalAnalyzerState::Loudness {
                    blocks,
                    frames,
                    maximum_frames,
                },
            ) => {
                let block_frames = input.first().map_or(0, |channel| channel.len());
                if block_frames == 0 {
                    return;
                }
                let channels = input.len().clamp(1, 2);
                let mut energy = 0.0_f64;
                for frame in 0..block_frames {
                    for channel in input.iter().take(2) {
                        energy += f64::from(channel[frame]) * f64::from(channel[frame]);
                    }
                }
                energy /= (block_frames * channels) as f64;
                while *frames > 0 && frames.saturating_add(block_frames) > *maximum_frames {
                    if let Some(discarded) = blocks.pop_front() {
                        *frames = frames.saturating_sub(discarded.frames);
                    }
                }
                blocks.push_back(LoudnessEnergyBlock {
                    energy,
                    frames: block_frames,
                });
                *frames = frames.saturating_add(block_frames);
            }
            (
                CanonicalAnalyzerConfig::Stereo(_),
                CanonicalAnalyzerState::Stereo {
                    left,
                    right,
                    write,
                    filled,
                },
            ) => {
                let frames = input.first().map_or(0, |channel| channel.len());
                for frame in 0..frames {
                    left[*write] = input[0][frame];
                    right[*write] = input.get(1).map_or(0.0, |channel| channel[frame]);
                    *write = (*write + 1) % left.len();
                    *filled = (*filled + 1).min(left.len());
                }
            }
            (
                CanonicalAnalyzerConfig::Spectrum(_),
                CanonicalAnalyzerState::Spectrum { capture, write, .. },
            ) => {
                let Some(first) = input.first() else { return };
                for frame in 0..first.len() {
                    capture[*write] = if input.len() == 1 {
                        first[frame]
                    } else {
                        (first[frame] + input[1][frame]) * 0.5
                    };
                    *write = (*write + 1) % capture.len();
                }
            }
            (
                CanonicalAnalyzerConfig::Oscilloscope(_),
                CanonicalAnalyzerState::Oscilloscope {
                    capture,
                    write,
                    filled,
                    crossings,
                    analyzed_frames,
                },
            ) => {
                let Some(first) = input.first() else { return };
                let mut previous = if *filled == 0 {
                    0.0
                } else {
                    capture[(*write + capture.len() - 1) % capture.len()]
                };
                for frame in 0..first.len() {
                    let sample = if input.len() == 1 {
                        first[frame]
                    } else {
                        (first[frame] + input[1][frame]) * 0.5
                    };
                    if (previous < 0.0 && sample >= 0.0) || (previous > 0.0 && sample <= 0.0) {
                        *crossings = crossings.saturating_add(1);
                    }
                    capture[*write] = sample;
                    *write = (*write + 1) % capture.len();
                    *filled = (*filled + 1).min(capture.len());
                    previous = sample;
                }
                *analyzed_frames = analyzed_frames.saturating_add(first.len() as u64);
            }
            (
                CanonicalAnalyzerConfig::Tuner(_),
                CanonicalAnalyzerState::Tuner {
                    capture,
                    write,
                    filled,
                },
            ) => {
                let Some(first) = input.first() else { return };
                for frame in 0..first.len() {
                    capture[*write] = if input.len() == 1 {
                        first[frame]
                    } else {
                        (first[frame] + input[1][frame]) * 0.5
                    };
                    *write = (*write + 1) % capture.len();
                    *filled = (*filled + 1).min(capture.len());
                }
            }
            _ => {}
        }
    }

    fn reset_configuration(&mut self) {
        match &mut self.state {
            CanonicalAnalyzerState::Empty => {}
            CanonicalAnalyzerState::Level {
                samples,
                write,
                filled,
                held_peak,
                held_until,
                latest_frame,
            } => {
                for channel in samples {
                    channel.fill(0.0);
                }
                *write = 0;
                *filled = 0;
                *held_peak = [0.0; 2];
                *held_until = [0; 2];
                *latest_frame = 0;
            }
            CanonicalAnalyzerState::Loudness { blocks, frames, .. } => {
                blocks.clear();
                *frames = 0;
            }
            CanonicalAnalyzerState::Spectrum {
                capture,
                write,
                previous_dbfs,
            } => {
                capture.fill(0.0);
                *write = 0;
                previous_dbfs.clear();
            }
            CanonicalAnalyzerState::Oscilloscope {
                capture,
                write,
                filled,
                crossings,
                analyzed_frames,
            } => {
                capture.fill(0.0);
                *write = 0;
                *filled = 0;
                *crossings = 0;
                *analyzed_frames = 0;
            }
            CanonicalAnalyzerState::Stereo {
                left,
                right,
                write,
                filled,
            } => {
                left.fill(0.0);
                right.fill(0.0);
                *write = 0;
                *filled = 0;
            }
            CanonicalAnalyzerState::Tuner {
                capture,
                write,
                filled,
            } => {
                capture.fill(0.0);
                *write = 0;
                *filled = 0;
            }
        }
    }

    fn configured_measurement(
        &mut self,
        measurement: gaw_core::AnalyzerMeasurement,
    ) -> gaw_core::AnalyzerMeasurement {
        match (&self.config, &mut self.state, measurement) {
            (
                CanonicalAnalyzerConfig::Level(parameters),
                CanonicalAnalyzerState::Level {
                    samples,
                    filled,
                    held_peak,
                    held_until,
                    latest_frame,
                    ..
                },
                gaw_core::AnalyzerMeasurement::LevelMeter(mut measurement),
            ) => {
                for (channel_index, channel) in samples.iter().enumerate() {
                    let active = &channel[..*filled];
                    let peak = active
                        .iter()
                        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
                    let rms = if active.is_empty() {
                        0.0
                    } else {
                        (active.iter().map(|sample| sample * sample).sum::<f32>()
                            / active.len() as f32)
                            .sqrt()
                    };
                    measurement.sample_peak_dbfs[channel_index] = Self::dbfs(peak);
                    measurement.rms_dbfs[channel_index] = Self::dbfs(rms);
                    if parameters.true_peak {
                        measurement.true_peak_dbfs[channel_index] =
                            measurement.true_peak_dbfs[channel_index].max(Self::dbfs(peak));
                    } else {
                        measurement.true_peak_dbfs[channel_index] = Self::dbfs(peak);
                    }
                    if *latest_frame > held_until[channel_index] {
                        held_peak[channel_index] = peak;
                    }
                    measurement.peak_hold_dbfs[channel_index] =
                        Self::dbfs(held_peak[channel_index].max(peak));
                    measurement.clipping[channel_index] = peak >= 1.0;
                }
                gaw_core::AnalyzerMeasurement::LevelMeter(measurement)
            }
            (
                CanonicalAnalyzerConfig::Loudness(parameters),
                CanonicalAnalyzerState::Loudness { blocks, .. },
                gaw_core::AnalyzerMeasurement::LoudnessMeter(mut measurement),
            ) => {
                let mut weighted_energy = 0.0;
                let mut gated_frames = 0_usize;
                for block in blocks {
                    let lufs = if block.energy <= 1.0e-12 {
                        -120.0
                    } else {
                        -0.691 + 10.0 * block.energy.log10()
                    };
                    if lufs >= f64::from(parameters.absolute_gate_lufs) {
                        weighted_energy += block.energy * block.frames as f64;
                        gated_frames = gated_frames.saturating_add(block.frames);
                    }
                }
                measurement.integrated_lufs = if gated_frames == 0 {
                    -120.0
                } else {
                    (-0.691 + 10.0 * (weighted_energy / gated_frames as f64).log10()) as f32
                };
                gaw_core::AnalyzerMeasurement::LoudnessMeter(measurement)
            }
            (
                CanonicalAnalyzerConfig::Spectrum(parameters),
                CanonicalAnalyzerState::Spectrum {
                    capture,
                    write,
                    previous_dbfs,
                },
                gaw_core::AnalyzerMeasurement::Spectrum(mut measurement),
            ) => {
                let size = capture.len();
                let bin_count = (size / 2 + 1).min(512);
                measurement.bins.clear();
                for output_bin in 0..bin_count {
                    let dft_bin = if bin_count == 1 {
                        0
                    } else {
                        output_bin * (size / 2) / (bin_count - 1)
                    };
                    let frequency_hz = dft_bin as f32 * self.sample_rate as f32 / size as f32;
                    if frequency_hz < parameters.minimum_hz || frequency_hz > parameters.maximum_hz
                    {
                        continue;
                    }
                    let mut real = 0.0_f32;
                    let mut imaginary = 0.0_f32;
                    for index in 0..size {
                        let phase = std::f32::consts::TAU * index as f32 / size as f32;
                        let window = match parameters.window {
                            gaw_core::WindowFunction::Hann => 0.5 - 0.5 * phase.cos(),
                            gaw_core::WindowFunction::BlackmanHarris => {
                                0.358_75 - 0.488_29 * phase.cos() + 0.141_28 * (2.0 * phase).cos()
                                    - 0.011_68 * (3.0 * phase).cos()
                            }
                            gaw_core::WindowFunction::FlatTop => {
                                0.215_578_95 - 0.416_631_58 * phase.cos()
                                    + 0.277_263_16 * (2.0 * phase).cos()
                                    - 0.083_578_944 * (3.0 * phase).cos()
                                    + 0.006_947_368 * (4.0 * phase).cos()
                            }
                        };
                        let angle =
                            std::f32::consts::TAU * dft_bin as f32 * index as f32 / size as f32;
                        let sample = capture[(*write + index) % size] * window;
                        real += sample * angle.cos();
                        imaginary -= sample * angle.sin();
                    }
                    let magnitude =
                        (real.mul_add(real, imaginary * imaginary)).sqrt() / size as f32;
                    measurement.bins.push(gaw_core::SpectrumBin {
                        frequency_hz,
                        magnitude_dbfs: Self::dbfs(magnitude),
                    });
                }
                if previous_dbfs.len() != measurement.bins.len() {
                    previous_dbfs.resize(measurement.bins.len(), -120.0);
                }
                for (bin, previous) in measurement.bins.iter_mut().zip(previous_dbfs.iter_mut()) {
                    bin.magnitude_dbfs = parameters
                        .smoothing
                        .mul_add(*previous, (1.0 - parameters.smoothing) * bin.magnitude_dbfs);
                    *previous = bin.magnitude_dbfs;
                }
                measurement.peaks = measurement
                    .bins
                    .iter()
                    .max_by(|left, right| left.magnitude_dbfs.total_cmp(&right.magnitude_dbfs))
                    .map(|bin| gaw_core::SpectralPeak {
                        frequency_hz: bin.frequency_hz,
                        magnitude_dbfs: bin.magnitude_dbfs,
                    })
                    .into_iter()
                    .collect();
                let (weighted, magnitude) =
                    measurement
                        .bins
                        .iter()
                        .fold((0.0_f32, 0.0_f32), |(weighted, total), bin| {
                            let amplitude = 10.0_f32.powf(bin.magnitude_dbfs / 20.0);
                            (weighted + amplitude * bin.frequency_hz, total + amplitude)
                        });
                measurement.spectral_centroid_hz = if magnitude <= f32::EPSILON {
                    0.0
                } else {
                    weighted / magnitude
                };
                gaw_core::AnalyzerMeasurement::Spectrum(measurement)
            }
            (
                CanonicalAnalyzerConfig::Oscilloscope(parameters),
                CanonicalAnalyzerState::Oscilloscope {
                    capture,
                    write,
                    filled,
                    crossings,
                    analyzed_frames,
                },
                gaw_core::AnalyzerMeasurement::Oscilloscope(mut measurement),
            ) => {
                let samples: Vec<_> = if *filled == capture.len() {
                    capture[*write..]
                        .iter()
                        .chain(&capture[..*write])
                        .copied()
                        .collect()
                } else {
                    capture[..*filled].to_vec()
                };
                measurement.channel_samples = vec![samples];
                measurement.zero_crossing_rate_hz = vec![if *analyzed_frames == 0 {
                    0.0
                } else {
                    (*crossings as f64 * self.sample_rate / *analyzed_frames as f64) as f32
                }];
                for samples in &mut measurement.channel_samples {
                    let crossing = match parameters.trigger {
                        gaw_core::OscilloscopeTrigger::Free => None,
                        gaw_core::OscilloscopeTrigger::RisingZero => samples
                            .windows(2)
                            .position(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
                            .map(|index| index + 1),
                        gaw_core::OscilloscopeTrigger::FallingZero => samples
                            .windows(2)
                            .position(|pair| pair[0] > 0.0 && pair[1] <= 0.0)
                            .map(|index| index + 1),
                    };
                    if let Some(crossing) = crossing {
                        samples.rotate_left(crossing);
                    }
                }
                gaw_core::AnalyzerMeasurement::Oscilloscope(measurement)
            }
            (
                CanonicalAnalyzerConfig::Stereo(_),
                CanonicalAnalyzerState::Stereo {
                    left,
                    right,
                    filled,
                    ..
                },
                gaw_core::AnalyzerMeasurement::StereoMeter(mut measurement),
            ) => {
                let mut mid_energy = 0.0_f64;
                let mut side_energy = 0.0_f64;
                let mut left_energy = 0.0_f64;
                let mut right_energy = 0.0_f64;
                let mut product = 0.0_f64;
                for (&left, &right) in left[..*filled].iter().zip(&right[..*filled]) {
                    let mid = f64::from(left + right) * 0.5;
                    let side = f64::from(left - right) * 0.5;
                    mid_energy += mid * mid;
                    side_energy += side * side;
                    left_energy += f64::from(left) * f64::from(left);
                    right_energy += f64::from(right) * f64::from(right);
                    product += f64::from(left) * f64::from(right);
                }
                let frames = (*filled).max(1) as f64;
                measurement.mid_level_dbfs = Self::dbfs((mid_energy / frames).sqrt() as f32);
                measurement.side_level_dbfs = Self::dbfs((side_energy / frames).sqrt() as f32);
                measurement.correlation =
                    (product / (left_energy * right_energy).sqrt().max(f64::EPSILON)) as f32;
                measurement.stereo_width =
                    (side_energy / mid_energy.max(f64::EPSILON)).sqrt() as f32;
                gaw_core::AnalyzerMeasurement::StereoMeter(measurement)
            }
            (
                CanonicalAnalyzerConfig::Tuner(parameters),
                CanonicalAnalyzerState::Tuner {
                    capture,
                    write,
                    filled,
                },
                gaw_core::AnalyzerMeasurement::Tuner(mut measurement),
            ) => {
                let samples: Vec<_> = if *filled == capture.len() {
                    capture[*write..]
                        .iter()
                        .chain(&capture[..*write])
                        .copied()
                        .collect()
                } else {
                    capture[..*filled].to_vec()
                };
                let crossings: Vec<_> = samples
                    .windows(2)
                    .enumerate()
                    .filter_map(|(index, pair)| {
                        (pair[0] < 0.0 && pair[1] >= 0.0).then_some(index + 1)
                    })
                    .collect();
                let average_period = if crossings.len() < 2 {
                    None
                } else {
                    Some(
                        crossings
                            .windows(2)
                            .map(|pair| (pair[1] - pair[0]) as f64)
                            .sum::<f64>()
                            / (crossings.len() - 1) as f64,
                    )
                };
                measurement.fundamental_hz =
                    average_period.map_or(0.0, |period| (self.sample_rate / period) as f32);
                if measurement.fundamental_hz < parameters.minimum_hz
                    || measurement.fundamental_hz > parameters.maximum_hz
                    || measurement.fundamental_hz == 0.0
                {
                    measurement.fundamental_hz = 0.0;
                    "--".clone_into(&mut measurement.note_name);
                    measurement.cents_offset = 0.0;
                    measurement.confidence = 0.0;
                } else {
                    let midi = 69.0
                        + 12.0
                            * (measurement.fundamental_hz / parameters.reference_pitch_hz).log2();
                    let rounded = midi.round();
                    ANALYZER_NOTE_NAMES[usize::from((rounded as i16).rem_euclid(12) as u8)]
                        .clone_into(&mut measurement.note_name);
                    measurement.cents_offset = (midi - rounded) * 100.0;
                    measurement.confidence = 1.0;
                }
                gaw_core::AnalyzerMeasurement::Tuner(measurement)
            }
            (_, _, measurement) => measurement,
        }
    }
}

impl DspProcessor for CanonicalAnalyzerProcessor {
    fn type_id(&self) -> &'static str {
        self.inner.type_id()
    }

    fn version(&self) -> u32 {
        self.inner.version()
    }

    fn input_layouts(&self) -> &'static [DspLayout] {
        self.inner.input_layouts()
    }

    fn output_layout(&self, input: DspLayout) -> Result<DspLayout, gaw_dsp::ProcessError> {
        self.inner.output_layout(input)
    }

    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), gaw_dsp::ProcessError> {
        self.inner.prepare(spec)?;
        self.initialize_state(spec);
        Ok(())
    }

    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[gaw_dsp::ParameterEvent],
        context: ProcessContext,
    ) -> Result<(), gaw_dsp::ProcessError> {
        self.inner.process(input, output, events, context)?;
        if self.inner.enabled() {
            self.analyze_configuration(input, context);
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.reset_configuration();
    }

    fn seek(&mut self, absolute_frame: u64) {
        self.inner.seek(absolute_frame);
        self.reset_configuration();
    }

    fn latency_frames(&self) -> u32 {
        self.inner.latency_frames()
    }

    fn tail_frames(&self) -> u64 {
        self.inner.tail_frames()
    }

    fn parameters(&self) -> &'static [gaw_dsp::ParameterDescriptor] {
        self.inner.parameters()
    }

    fn enabled(&self) -> bool {
        self.inner.enabled()
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.inner.set_enabled(enabled);
    }

    fn analyzer_measurement(&mut self) -> Option<gaw_core::AnalyzerMeasurement> {
        let measurement = self.inner.analyzer_measurement()?;
        Some(self.configured_measurement(measurement))
    }
}

pub(super) const fn fft_size(value: gaw_core::FftSize) -> usize {
    match value {
        gaw_core::FftSize::N256 => 256,
        gaw_core::FftSize::N512 => 512,
        gaw_core::FftSize::N1024 => 1_024,
        gaw_core::FftSize::N2048 => 2_048,
        gaw_core::FftSize::N4096 => 4_096,
        gaw_core::FftSize::N8192 => 8_192,
        gaw_core::FftSize::N16384 => 16_384,
    }
}
