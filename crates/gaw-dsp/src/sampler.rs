//! Zone-based sampler instrument with duration-preserving pitch.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::contract::{AudioLayout, PrepareSpec, ProcessContext};

/// A decoded immutable audio asset bound to a sampler configuration.
#[derive(Clone, Debug)]
pub struct SampleAsset {
    /// Stable asset identifier referenced by zones.
    pub id: String,
    /// Original sample rate.
    pub sample_rate: f64,
    /// Planar mono or stereo sample data.
    pub channels: Vec<Vec<f32>>,
}

impl SampleAsset {
    fn is_valid(&self) -> bool {
        (self.channels.len() == 1 || self.channels.len() == 2)
            && self.sample_rate.is_finite()
            && self.sample_rate > 0.0
            && self
                .channels
                .iter()
                .all(|channel| channel.len() == self.channels[0].len())
    }
}

/// Whether a zone ignores note-off or releases with its note.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackMode {
    /// Play the complete source range after triggering.
    #[default]
    OneShot,
    /// Enter the release stage when the note is released.
    NoteGated,
}

/// One transparent sampler key/velocity zone.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SamplerZone {
    /// Stable identifier used to preserve zone identity across edits.
    #[serde(default)]
    pub id: String,
    pub asset_id: String,
    #[serde(default)]
    pub source_start_frame: usize,
    #[serde(default)]
    pub source_end_frame: Option<usize>,
    #[serde(default = "default_root_note")]
    pub root_note: u8,
    #[serde(default)]
    pub low_note: u8,
    #[serde(default = "max_note")]
    pub high_note: u8,
    #[serde(default)]
    pub low_velocity: u8,
    #[serde(default = "max_velocity")]
    pub high_velocity: u8,
    #[serde(default)]
    pub playback_mode: PlaybackMode,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default = "default_velocity_sensitivity")]
    pub velocity_sensitivity: f32,
    #[serde(default)]
    pub attack_ms: f32,
    #[serde(default = "default_release_ms")]
    pub release_ms: f32,
    #[serde(default)]
    pub reverse: bool,
    #[serde(default)]
    pub choke_group: Option<u16>,
}

const fn default_root_note() -> u8 {
    60
}
const fn max_note() -> u8 {
    127
}
const fn max_velocity() -> u8 {
    127
}
const fn default_velocity_sensitivity() -> f32 {
    1.0
}
const fn default_release_ms() -> f32 {
    20.0
}
const fn default_polyphony() -> usize {
    32
}

/// Serializable complete sampler state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SamplerConfig {
    #[serde(default = "default_polyphony")]
    pub polyphony: usize,
    #[serde(default)]
    pub zones: Vec<SamplerZone>,
}

impl Default for SamplerConfig {
    fn default() -> Self {
        Self {
            polyphony: default_polyphony(),
            zones: Vec::new(),
        }
    }
}

/// Sample-accurate musical input to an instrument.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NoteEvent {
    NoteOn {
        sample_offset: usize,
        note: u8,
        velocity: f32,
    },
    /// Per-voice tuning, limited to one semitone around the MIDI key.
    NoteOnTuned {
        sample_offset: usize,
        note: u8,
        velocity: f32,
        cents: f64,
    },
    NoteOff {
        sample_offset: usize,
        note: u8,
    },
}

impl NoteEvent {
    fn offset(self) -> usize {
        match self {
            Self::NoteOn { sample_offset, .. }
            | Self::NoteOnTuned { sample_offset, .. }
            | Self::NoteOff { sample_offset, .. } => sample_offset,
        }
    }
}

/// Instrument contract errors.
#[derive(Debug, Error, PartialEq)]
pub enum InstrumentError {
    #[error("instrument is not prepared")]
    NotPrepared,
    #[error("output channel layout does not match the prepared layout")]
    LayoutMismatch,
    #[error("block exceeds the prepared maximum")]
    BlockTooLarge,
    #[error("note events must be ordered and fall inside the block")]
    InvalidEvents,
    #[error("sample asset `{0}` is invalid")]
    InvalidAsset(String),
    #[error("sampler configuration is invalid: {0}")]
    InvalidConfiguration(&'static str),
    #[error("sampler zone `{0}` references an unavailable or invalid source range")]
    InvalidZone(String),
    #[error("too many note events in one block")]
    TooManyEvents,
}

const MAX_POLYPHONY: usize = 256;
const MAX_ZONES: usize = 1_024;
const MAX_ASSETS: usize = 1_024;
const MAX_EVENTS_PER_BLOCK: usize = 4_096;
const SINC_TAPS: usize = 65;
const SINC_PHASES: usize = 256;
const SINC_CUTOFF_LEVELS: usize = 128;
const SILENT_SINC_BIN: u8 = u8::MAX;
const MIN_SINC_CUTOFF: f64 = 1.0 / 2_048.0;
const MAX_SINC_CUTOFF: f64 = 0.95;

/// Real-time instrument contract. Preparation and asset binding happen off the audio thread.
pub trait Instrument: std::fmt::Debug + Send {
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), InstrumentError>;
    fn process(
        &mut self,
        output: &mut [&mut [f32]],
        events: &[NoteEvent],
        context: ProcessContext,
    ) -> Result<(), InstrumentError>;
    fn reset(&mut self);
    fn seek(&mut self, absolute_frame: u64);
    fn latency_frames(&self) -> usize;
    fn tail_frames(&self) -> usize;
}

#[derive(Clone, Debug)]
struct Voice {
    active: bool,
    released: bool,
    note: u8,
    zone: usize,
    asset: usize,
    position: f64,
    step: f64,
    sinc_bin: u8,
    pitch_factor: f64,
    rendered_frames: usize,
    gain: f32,
    envelope: f32,
    age: u64,
}

#[derive(Clone, Debug)]
struct PreparedZone {
    zone: SamplerZone,
    asset: usize,
    source_end_frame: usize,
    output_frames: usize,
}

#[derive(Debug)]
struct SincKernel {
    direct_integer: bool,
    coefficients: Vec<f32>,
}

type PreparedSamplerData = (Vec<PreparedZone>, Vec<Option<SincKernel>>, usize);

impl Default for Voice {
    fn default() -> Self {
        Self {
            active: false,
            released: false,
            note: 0,
            zone: 0,
            asset: 0,
            position: 0.0,
            step: 1.0,
            sinc_bin: 0,
            pitch_factor: 1.0,
            rendered_frames: 0,
            gain: 0.0,
            envelope: 0.0,
            age: 0,
        }
    }
}

impl SincKernel {
    fn new(cutoff: f64) -> Self {
        let half = (SINC_TAPS / 2).cast_signed();
        let mut coefficients = vec![0.0; SINC_PHASES * SINC_TAPS];
        for phase in 0..SINC_PHASES {
            let fraction = phase as f64 / SINC_PHASES as f64;
            let row = &mut coefficients[phase * SINC_TAPS..(phase + 1) * SINC_TAPS];
            let mut sum = 0.0_f64;
            for (tap, coefficient) in row.iter_mut().enumerate() {
                let offset = tap.cast_signed() - half;
                let distance = fraction - offset as f64;
                let argument = cutoff * distance;
                let sinc = if argument.abs() < 1.0e-12 {
                    1.0
                } else {
                    (core::f64::consts::PI * argument).sin() / (core::f64::consts::PI * argument)
                };
                let normalized = distance / (half as f64 + 1.0);
                let window = if normalized.abs() >= 1.0 {
                    0.0
                } else {
                    0.358_75
                        + 0.488_29 * (core::f64::consts::PI * normalized).cos()
                        + 0.141_28 * (2.0 * core::f64::consts::PI * normalized).cos()
                        + 0.011_68 * (3.0 * core::f64::consts::PI * normalized).cos()
                };
                let value = cutoff * sinc * window;
                *coefficient = value as f32;
                sum += value;
            }
            if sum.abs() > f64::EPSILON {
                for coefficient in row {
                    *coefficient = (f64::from(*coefficient) / sum) as f32;
                }
            }
        }
        Self {
            direct_integer: cutoff >= MAX_SINC_CUTOFF,
            coefficients,
        }
    }

    fn phase(&self, phase: usize) -> &[f32] {
        &self.coefficients[phase * SINC_TAPS..(phase + 1) * SINC_TAPS]
    }
}

fn sinc_cutoff(bin: usize) -> f64 {
    let position = bin as f64 / (SINC_CUTOFF_LEVELS - 1) as f64;
    MIN_SINC_CUTOFF * (MAX_SINC_CUTOFF / MIN_SINC_CUTOFF).powf(position)
}

fn sinc_bin_for_step(step: f64) -> u8 {
    let desired = MAX_SINC_CUTOFF / step.max(1.0);
    if desired < MIN_SINC_CUTOFF {
        return SILENT_SINC_BIN;
    }
    let position = (desired.min(MAX_SINC_CUTOFF) / MIN_SINC_CUTOFF).ln()
        / (MAX_SINC_CUTOFF / MIN_SINC_CUTOFF).ln();
    (position * (SINC_CUTOFF_LEVELS - 1) as f64).floor() as u8
}

fn sinc_kernel(kernels: &[Option<SincKernel>], bin: u8) -> Option<&SincKernel> {
    if bin == SILENT_SINC_BIN {
        None
    } else {
        kernels[usize::from(bin)].as_ref()
    }
}

fn interpolate_prepared(
    source: &[f32],
    position: f64,
    start: usize,
    end: usize,
    kernel: Option<&SincKernel>,
) -> f32 {
    let Some(kernel) = kernel else {
        return 0.0;
    };
    let mut index = position.floor() as usize;
    let fraction = position - index as f64;
    let mut phase = (fraction * SINC_PHASES as f64).round() as usize;
    if phase == SINC_PHASES {
        index += 1;
        phase = 0;
    }
    let interpolation_fraction = phase as f32 / SINC_PHASES as f32;
    let half = SINC_TAPS / 2;
    if index < start + half || index + half >= end {
        let lower = index.clamp(start, end - 1);
        let upper = (lower + 1).min(end - 1);
        return source[lower] + (source[upper] - source[lower]) * interpolation_fraction;
    }
    let first = index - half;
    source[first..first + SINC_TAPS]
        .iter()
        .zip(kernel.phase(phase))
        .map(|(sample, coefficient)| sample * coefficient)
        .sum()
}

// Fixed-size per-voice storage, prepared off the audio thread. Band-limited
// resampling determines pitch; inverse-ratio stretching restores duration.
// Source lookahead removes latency without delaying the musical note.
// Signalsmith randomizes unpitched spectral phases at stretch ratios above 2,
// so notes more than one octave up may vary slightly between fresh triggers.
const PITCH_BLOCK: usize = 256;

#[derive(Debug)]
struct VoicePitch {
    stretch: gaw_stretch::TimeStretcher,
    source: Vec<f32>,
    output: Vec<f32>,
    channels: usize,
    input_frame: usize,
    output_frame: usize,
    prime_frames: usize,
    factor: f64,
    output_available: usize,
    processed_output: usize,
}

impl VoicePitch {
    fn new(sample_rate: f64, channels: usize) -> Self {
        let stretch = gaw_stretch::TimeStretcher::new(gaw_stretch::Config {
            channels: channels as u8,
            sample_rate: (sample_rate.round() as u32).max(1),
            quality: gaw_stretch::Quality::Instrument,
        })
        .expect("validated sampler output format");
        // Covers a one-frame pull at the lowest supported MIDI pitch (-128 semitones).
        let input_capacity = 2_048.max(stretch.input_latency());
        Self {
            stretch,
            source: vec![0.0; input_capacity * channels],
            output: vec![0.0; PITCH_BLOCK * channels],
            channels,
            input_frame: 0,
            output_frame: PITCH_BLOCK,
            prime_frames: 0,
            factor: 1.0,
            output_available: 0,
            processed_output: 0,
        }
    }

    fn start(&mut self, factor: f64) {
        self.stretch.reset();
        self.stretch
            .set_pitch_factor(1.0)
            .expect("finite note pitch");
        self.factor = factor;
        self.processed_output = 0;
        self.output_available = 0;
        self.input_frame = 0;
        self.output_frame = PITCH_BLOCK;
        self.prime_frames = self.stretch.output_latency();
    }

    fn refill(
        &mut self,
        asset: &SampleAsset,
        zone: &PreparedZone,
        step: f64,
        kernel: Option<&SincKernel>,
    ) {
        if self.input_frame == 0 {
            let frames = self.stretch.input_latency();
            self.read_source(asset, zone, step, kernel, frames);
            self.stretch
                .seek(&self.source[..frames * self.channels], 1.0 / self.factor)
                .expect("prepared sampler lookahead");
            self.input_frame = frames;
        }
        loop {
            let frames = if self.prime_frames > 0 {
                self.prime_frames.min(PITCH_BLOCK)
            } else {
                PITCH_BLOCK
            }
            .min(
                ((self.source.len() / self.channels - 1) as f64 * self.factor)
                    .floor()
                    .max(1.0) as usize,
            );
            let input_end = self.stretch.input_latency()
                + ((self.processed_output + frames) as f64 / self.factor).round() as usize;
            let input_frames = input_end.saturating_sub(self.input_frame);
            self.read_source(asset, zone, step, kernel, input_frames);
            self.stretch
                .process(
                    &self.source[..input_frames * self.channels],
                    &mut self.output[..frames * self.channels],
                )
                .expect("prepared channel-aligned sampler pitch buffers");
            self.input_frame += input_frames;
            self.processed_output += frames;
            self.output_available = frames;
            if self.prime_frames == 0 {
                break;
            }
            self.prime_frames -= frames;
        }
        self.output_frame = 0;
    }

    fn read_source(
        &mut self,
        asset: &SampleAsset,
        zone: &PreparedZone,
        step: f64,
        kernel: Option<&SincKernel>,
        frames: usize,
    ) {
        for frame in 0..frames {
            let source_frame = self.input_frame + frame;
            let position = source_position(zone, source_frame, step * self.factor);
            for channel in 0..self.channels {
                self.source[frame * self.channels + channel] =
                    if (source_frame as f64 * self.factor) < zone.output_frames as f64 {
                        source_sample(asset, zone, position, kernel, channel, self.channels)
                    } else {
                        0.0
                    };
            }
        }
    }
}

fn source_position(zone: &PreparedZone, frame: usize, step: f64) -> f64 {
    if zone.zone.reverse {
        ((zone.source_end_frame - 1) as f64 - frame as f64 * step)
            .max(zone.zone.source_start_frame as f64)
    } else {
        zone.zone.source_start_frame as f64 + frame as f64 * step
    }
}

fn source_sample(
    asset: &SampleAsset,
    zone: &PreparedZone,
    position: f64,
    kernel: Option<&SincKernel>,
    channel: usize,
    output_channels: usize,
) -> f32 {
    let read = |index: usize| {
        if kernel.is_some_and(|kernel| kernel.direct_integer) && position.fract() == 0.0 {
            asset.channels[index]
                [(position as usize).clamp(zone.zone.source_start_frame, zone.source_end_frame - 1)]
        } else {
            interpolate_prepared(
                &asset.channels[index],
                position,
                zone.zone.source_start_frame,
                zone.source_end_frame,
                kernel,
            )
        }
    };
    if output_channels == 1 && asset.channels.len() == 2 {
        0.5 * (read(0) + read(1))
    } else {
        read(channel.min(asset.channels.len() - 1))
    }
}

/// Built-in `gaw.sampler` instrument.
#[derive(Debug)]
pub struct Sampler {
    pub config: SamplerConfig,
    assets: Vec<SampleAsset>,
    prepared_zones: Vec<PreparedZone>,
    sinc_kernels: Vec<Option<SincKernel>>,
    voices: Vec<Voice>,
    voice_pitch: Vec<VoicePitch>,
    sample_rate: f64,
    max_block_size: usize,
    output_layout: AudioLayout,
    absolute_frame: u64,
    next_age: u64,
    prepared_tail_frames: usize,
    prepared: bool,
}

impl Sampler {
    pub fn new(
        mut config: SamplerConfig,
        assets: Vec<SampleAsset>,
    ) -> Result<Self, InstrumentError> {
        Self::normalize_and_validate_config(&mut config)?;
        Self::validate_assets(&assets)?;
        let polyphony = config.polyphony;
        Ok(Self {
            config,
            assets,
            prepared_zones: Vec::new(),
            sinc_kernels: (0..SINC_CUTOFF_LEVELS).map(|_| None).collect(),
            voices: vec![Voice::default(); polyphony],
            voice_pitch: Vec::new(),
            sample_rate: 0.0,
            max_block_size: 0,
            output_layout: AudioLayout::Stereo,
            absolute_frame: 0,
            next_age: 0,
            prepared_tail_frames: 0,
            prepared: false,
        })
    }

    /// Replace decoded assets outside the process callback.
    pub fn set_assets(&mut self, assets: Vec<SampleAsset>) -> Result<(), InstrumentError> {
        Self::validate_assets(&assets)?;
        if self.prepared {
            let (zones, kernels, tail) =
                Self::compile_zones(&self.config, &assets, self.sample_rate)?;
            self.prepared_zones = zones;
            self.sinc_kernels = kernels;
            self.prepared_tail_frames = tail;
        }
        self.assets = assets;
        self.reset();
        Ok(())
    }

    fn normalize_and_validate_config(config: &mut SamplerConfig) -> Result<(), InstrumentError> {
        if !(1..=MAX_POLYPHONY).contains(&config.polyphony) {
            return Err(InstrumentError::InvalidConfiguration(
                "polyphony must be between 1 and 256",
            ));
        }
        if config.zones.len() > MAX_ZONES {
            return Err(InstrumentError::InvalidConfiguration(
                "zone count exceeds the realtime bound",
            ));
        }
        for (index, zone) in config.zones.iter_mut().enumerate() {
            if zone.id.is_empty() {
                zone.id = format!("zone-{index}");
            }
            if zone.asset_id.is_empty()
                || zone.root_note > 127
                || zone.high_note > 127
                || zone.high_velocity > 127
                || zone.low_note > zone.high_note
                || zone.low_velocity > zone.high_velocity
                || !zone.gain_db.is_finite()
                || !zone.velocity_sensitivity.is_finite()
                || !(0.0..=1.0).contains(&zone.velocity_sensitivity)
                || !zone.attack_ms.is_finite()
                || zone.attack_ms < 0.0
                || !zone.release_ms.is_finite()
                || zone.release_ms < 0.0
            {
                return Err(InstrumentError::InvalidZone(zone.id.clone()));
            }
        }
        for (index, zone) in config.zones.iter().enumerate() {
            if config.zones[..index]
                .iter()
                .any(|prior| prior.id == zone.id)
            {
                return Err(InstrumentError::InvalidZone(zone.id.clone()));
            }
        }
        Ok(())
    }

    fn validate_assets(assets: &[SampleAsset]) -> Result<(), InstrumentError> {
        if assets.len() > MAX_ASSETS {
            return Err(InstrumentError::InvalidConfiguration(
                "asset count exceeds the realtime bound",
            ));
        }
        for (index, asset) in assets.iter().enumerate() {
            if asset.id.is_empty()
                || !asset.is_valid()
                || assets[..index].iter().any(|prior| prior.id == asset.id)
            {
                return Err(InstrumentError::InvalidAsset(asset.id.clone()));
            }
        }
        Ok(())
    }

    fn compile_zones(
        config: &SamplerConfig,
        assets: &[SampleAsset],
        output_sample_rate: f64,
    ) -> Result<PreparedSamplerData, InstrumentError> {
        let mut prepared = Vec::with_capacity(config.zones.len());
        let mut used_sinc_bins = [false; SINC_CUTOFF_LEVELS];
        let mut tail = 0;
        for zone in &config.zones {
            let Some(asset) = assets.iter().position(|asset| asset.id == zone.asset_id) else {
                return Err(InstrumentError::InvalidZone(zone.id.clone()));
            };
            let asset_frames = assets[asset].channels[0].len();
            let end = zone.source_end_frame.unwrap_or(asset_frames);
            if zone.source_start_frame >= end || end > asset_frames {
                return Err(InstrumentError::InvalidZone(zone.id.clone()));
            }
            let step = assets[asset].sample_rate / output_sample_rate;
            let output_frames = ((end - zone.source_start_frame) as f64 / step).ceil() as usize;
            let zone_tail = match zone.playback_mode {
                PlaybackMode::OneShot => output_frames,
                PlaybackMode::NoteGated => {
                    (f64::from(zone.release_ms) * output_sample_rate / 1000.0).ceil() as usize
                }
            };
            tail = tail.max(zone_tail);
            let sinc_bin = sinc_bin_for_step(step);
            if sinc_bin != SILENT_SINC_BIN {
                used_sinc_bins[usize::from(sinc_bin)] = true;
                for note in zone.low_note..=zone.high_note {
                    let pitch = 2.0_f64.powf((f64::from(note) - f64::from(zone.root_note)) / 12.0);
                    let low = sinc_bin_for_step(step * pitch * 2.0_f64.powf(1.0 / 12.0));
                    let high = sinc_bin_for_step(step * pitch * 2.0_f64.powf(-1.0 / 12.0));
                    if high != SILENT_SINC_BIN {
                        used_sinc_bins[usize::from(if low == SILENT_SINC_BIN { 0 } else { low })
                            ..=usize::from(high)]
                            .fill(true);
                    }
                }
            }
            prepared.push(PreparedZone {
                zone: zone.clone(),
                asset,
                source_end_frame: end,
                output_frames,
            });
        }
        let sinc_kernels = used_sinc_bins
            .into_iter()
            .enumerate()
            .map(|(bin, used)| used.then(|| SincKernel::new(sinc_cutoff(bin))))
            .collect();
        Ok((prepared, sinc_kernels, tail))
    }

    fn note_on(&mut self, note: u8, velocity: f32, cents: f64) {
        let event_age_floor = self.next_age;
        for zone_index in 0..self.prepared_zones.len() {
            let prepared_zone = &self.prepared_zones[zone_index];
            let zone = &prepared_zone.zone;
            let velocity_midi = (velocity.clamp(0.0, 1.0) * 127.0).round() as u8;
            if note < zone.low_note
                || note > zone.high_note
                || velocity_midi < zone.low_velocity
                || velocity_midi > zone.high_velocity
            {
                continue;
            }
            let asset_index = prepared_zone.asset;
            let asset = &self.assets[asset_index];
            let end = prepared_zone.source_end_frame;
            if let Some(group) = zone.choke_group {
                for voice in &mut self.voices {
                    if voice.active
                        && voice.age < event_age_floor
                        && self.prepared_zones[voice.zone].zone.choke_group == Some(group)
                    {
                        voice.active = false;
                    }
                }
            }
            let slot = self
                .voices
                .iter()
                .position(|voice| !voice.active)
                .unwrap_or_else(|| {
                    self.voices
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, voice)| voice.age)
                        .map_or(0, |(index, _)| index)
                });
            let velocity_gain = 1.0 - zone.velocity_sensitivity.clamp(0.0, 1.0)
                + zone.velocity_sensitivity.clamp(0.0, 1.0) * velocity.clamp(0.0, 1.0);
            let semitones = f64::from(note) - f64::from(zone.root_note) + cents / 100.0;
            let step = asset.sample_rate / self.sample_rate;
            let pitch_factor = 2.0_f64.powf(semitones / 12.0);
            if (pitch_factor - 1.0).abs() > f64::EPSILON {
                self.voice_pitch[slot].start(pitch_factor);
            }
            self.voices[slot] = Voice {
                active: true,
                released: false,
                note,
                zone: zone_index,
                asset: asset_index,
                position: if zone.reverse {
                    (end - 1) as f64
                } else {
                    zone.source_start_frame as f64
                },
                step,
                sinc_bin: sinc_bin_for_step(step * pitch_factor),
                pitch_factor,
                rendered_frames: 0,
                gain: 10.0_f32.powf(zone.gain_db / 20.0) * velocity_gain,
                envelope: if zone.attack_ms <= 0.0 { 1.0 } else { 0.0 },
                age: self.next_age,
            };
            self.next_age = self.next_age.wrapping_add(1);
        }
    }

    fn note_off(&mut self, note: u8) {
        for voice in &mut self.voices {
            if voice.active
                && voice.note == note
                && self.prepared_zones[voice.zone].zone.playback_mode == PlaybackMode::NoteGated
            {
                if self.prepared_zones[voice.zone].zone.release_ms == 0.0 {
                    voice.active = false;
                } else {
                    voice.released = true;
                }
            }
        }
    }

    fn render_frame(&mut self, output: &mut [&mut [f32]], frame: usize) {
        for (voice, pitch) in self.voices.iter_mut().zip(&mut self.voice_pitch) {
            if !voice.active {
                continue;
            }
            let prepared_zone = &self.prepared_zones[voice.zone];
            let zone = &prepared_zone.zone;
            let asset = &self.assets[voice.asset];
            if voice.rendered_frames >= prepared_zone.output_frames {
                voice.active = false;
                continue;
            }
            let kernel = sinc_kernel(&self.sinc_kernels, voice.sinc_bin);
            let shifted = (voice.pitch_factor - 1.0).abs() > f64::EPSILON;
            if shifted && pitch.output_frame >= pitch.output_available {
                pitch.refill(asset, prepared_zone, voice.step, kernel);
            }
            let channels = output.len();
            for (channel_index, channel) in output.iter_mut().enumerate() {
                let sample = if shifted {
                    pitch.output[pitch.output_frame * channels + channel_index]
                } else {
                    source_sample(
                        asset,
                        prepared_zone,
                        voice.position,
                        kernel,
                        channel_index,
                        channels,
                    )
                };
                channel[frame] += sample * voice.gain * voice.envelope;
            }
            if shifted {
                pitch.output_frame += 1;
            }

            if voice.released {
                let release_frames = (f64::from(zone.release_ms.max(0.01)) * self.sample_rate
                    / 1000.0)
                    .max(1.0) as f32;
                voice.envelope = (voice.envelope - 1.0 / release_frames).max(0.0);
                if voice.envelope <= 0.0 {
                    voice.active = false;
                }
            } else if voice.envelope < 1.0 {
                let attack_frames = (f64::from(zone.attack_ms.max(0.01)) * self.sample_rate
                    / 1000.0)
                    .max(1.0) as f32;
                voice.envelope = (voice.envelope + 1.0 / attack_frames).min(1.0);
            }
            voice.rendered_frames += 1;
            voice.position = source_position(prepared_zone, voice.rendered_frames, voice.step);
        }
    }
}

impl Instrument for Sampler {
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), InstrumentError> {
        if !spec.sample_rate.is_finite() || spec.sample_rate <= 0.0 {
            return Err(InstrumentError::InvalidConfiguration(
                "sample rate must be finite and positive",
            ));
        }
        if spec.max_block_size == 0 {
            return Err(InstrumentError::InvalidConfiguration(
                "maximum block size must be non-zero",
            ));
        }
        if !spec.tempo_bpm.is_finite() || spec.tempo_bpm <= 0.0 {
            return Err(InstrumentError::InvalidConfiguration(
                "tempo must be finite and positive",
            ));
        }
        Self::normalize_and_validate_config(&mut self.config)?;
        Self::validate_assets(&self.assets)?;
        let (prepared_zones, sinc_kernels, tail_frames) =
            Self::compile_zones(&self.config, &self.assets, spec.sample_rate)?;
        self.prepared = false;
        self.sample_rate = spec.sample_rate;
        self.max_block_size = spec.max_block_size;
        self.output_layout = spec.input_layout;
        let polyphony = self.config.polyphony;
        self.voices.resize(polyphony, Voice::default());
        self.voice_pitch = (0..polyphony)
            .map(|_| VoicePitch::new(spec.sample_rate, spec.input_layout.channels()))
            .collect();
        self.prepared_zones = prepared_zones;
        self.sinc_kernels = sinc_kernels;
        self.prepared_tail_frames = tail_frames;
        self.prepared = true;
        self.reset();
        Ok(())
    }

    fn process(
        &mut self,
        output: &mut [&mut [f32]],
        events: &[NoteEvent],
        context: ProcessContext,
    ) -> Result<(), InstrumentError> {
        if !self.prepared {
            return Err(InstrumentError::NotPrepared);
        }
        let channels = self.output_layout.channels();
        if output.len() != channels || output.windows(2).any(|pair| pair[0].len() != pair[1].len())
        {
            return Err(InstrumentError::LayoutMismatch);
        }
        let frames = output.first().map_or(0, |channel| channel.len());
        if frames > self.max_block_size {
            return Err(InstrumentError::BlockTooLarge);
        }
        if events.len() > MAX_EVENTS_PER_BLOCK {
            return Err(InstrumentError::TooManyEvents);
        }
        if events.iter().enumerate().any(|(index, event)| {
            event.offset() >= frames
                || (index > 0 && events[index - 1].offset() > event.offset())
                || matches!(event, NoteEvent::NoteOn { note, .. } | NoteEvent::NoteOnTuned { note, .. } | NoteEvent::NoteOff { note, .. } if *note > 127)
                || matches!(event, NoteEvent::NoteOn { velocity, .. } | NoteEvent::NoteOnTuned { velocity, .. } if !velocity.is_finite() || !(0.0..=1.0).contains(velocity))
                || matches!(event, NoteEvent::NoteOnTuned { cents, .. } if !cents.is_finite() || cents.abs() > 100.0)
        }) {
            return Err(InstrumentError::InvalidEvents);
        }
        for channel in output.iter_mut() {
            channel.fill(0.0);
        }
        self.absolute_frame = context.absolute_frame;
        let mut event_index = 0;
        for frame in 0..frames {
            while event_index < events.len() && events[event_index].offset() == frame {
                match events[event_index] {
                    NoteEvent::NoteOn { note, velocity, .. } if velocity > 0.0 => {
                        self.note_on(note, velocity, 0.0);
                    }
                    NoteEvent::NoteOnTuned {
                        note,
                        velocity,
                        cents,
                        ..
                    } if velocity > 0.0 => {
                        self.note_on(note, velocity, cents);
                    }
                    NoteEvent::NoteOn { note, .. }
                    | NoteEvent::NoteOnTuned { note, .. }
                    | NoteEvent::NoteOff { note, .. } => {
                        self.note_off(note);
                    }
                }
                event_index += 1;
            }
            self.render_frame(output, frame);
        }
        self.absolute_frame = self.absolute_frame.saturating_add(frames as u64);
        Ok(())
    }

    fn reset(&mut self) {
        for voice in &mut self.voices {
            *voice = Voice::default();
        }
        self.absolute_frame = 0;
        self.next_age = 0;
    }

    fn seek(&mut self, absolute_frame: u64) {
        self.reset();
        self.absolute_frame = absolute_frame;
    }

    fn latency_frames(&self) -> usize {
        0
    }

    fn tail_frames(&self) -> usize {
        self.prepared_tail_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sampler(mode: PlaybackMode) -> Sampler {
        let config = SamplerConfig {
            polyphony: 4,
            zones: vec![SamplerZone {
                id: "tone-zone".into(),
                asset_id: "tone".into(),
                source_start_frame: 0,
                source_end_frame: None,
                root_note: 60,
                low_note: 60,
                high_note: 60,
                low_velocity: 0,
                high_velocity: 127,
                playback_mode: mode,
                gain_db: 0.0,
                velocity_sensitivity: 1.0,
                attack_ms: 0.0,
                release_ms: 1.0,
                reverse: false,
                choke_group: None,
            }],
        };
        Sampler::new(
            config,
            vec![SampleAsset {
                id: "tone".into(),
                sample_rate: 1_000.0,
                channels: vec![vec![1.0; 64]],
            }],
        )
        .unwrap()
    }

    #[test]
    fn note_gated_voice_releases() {
        let mut sampler = sampler(PlaybackMode::NoteGated);
        sampler
            .prepare(PrepareSpec {
                sample_rate: 1_000.0,
                max_block_size: 8,
                input_layout: AudioLayout::Mono,
                tempo_bpm: 120.0,
            })
            .unwrap();
        let mut output = [0.0; 8];
        sampler
            .process(
                &mut [&mut output],
                &[
                    NoteEvent::NoteOn {
                        sample_offset: 0,
                        note: 60,
                        velocity: 1.0,
                    },
                    NoteEvent::NoteOff {
                        sample_offset: 3,
                        note: 60,
                    },
                ],
                ProcessContext::default(),
            )
            .unwrap();
        assert_eq!(output[0], 1.0);
        assert_eq!(output[3], 1.0);
        assert_eq!(output[4], 0.0);
    }

    #[test]
    fn tuned_voices_have_independent_equal_division_pitch_and_release() {
        let mut sampler = sampler(PlaybackMode::NoteGated);
        sampler.config.zones[0].high_note = 72;
        sampler
            .prepare(PrepareSpec {
                sample_rate: 1_000.0,
                max_block_size: 8,
                input_layout: AudioLayout::Mono,
                tempo_bpm: 120.0,
            })
            .unwrap();
        let mut output = [0.0; 8];
        sampler
            .process(
                &mut [&mut output],
                &[
                    NoteEvent::NoteOnTuned {
                        sample_offset: 0,
                        note: 62,
                        velocity: 1.0,
                        cents: -200.0 / 7.0,
                    },
                    NoteEvent::NoteOnTuned {
                        sample_offset: 0,
                        note: 63,
                        velocity: 1.0,
                        cents: 300.0 / 7.0,
                    },
                ],
                ProcessContext::default(),
            )
            .unwrap();
        assert!((sampler.voices[0].pitch_factor - 2.0_f64.powf(1.0 / 7.0)).abs() < 1e-12);
        assert!((sampler.voices[1].pitch_factor - 2.0_f64.powf(2.0 / 7.0)).abs() < 1e-12);
        sampler
            .process(
                &mut [&mut output],
                &[NoteEvent::NoteOff {
                    sample_offset: 0,
                    note: 62,
                }],
                ProcessContext::default(),
            )
            .unwrap();
        assert!(!sampler.voices[0].active);
        assert!(sampler.voices[1].active);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn tuned_events_reject_nonfinite_or_out_of_range_offsets() {
        let mut sampler = sampler(PlaybackMode::NoteGated);
        sampler
            .prepare(PrepareSpec {
                sample_rate: 1_000.0,
                max_block_size: 8,
                input_layout: AudioLayout::Mono,
                tempo_bpm: 120.0,
            })
            .unwrap();
        for cents in [f64::NAN, f64::INFINITY, -101.0, 101.0] {
            let mut output = [0.0; 8];
            assert_eq!(
                sampler.process(
                    &mut [&mut output],
                    &[NoteEvent::NoteOnTuned {
                        sample_offset: 0,
                        note: 60,
                        velocity: 1.0,
                        cents
                    }],
                    ProcessContext::default()
                ),
                Err(InstrumentError::InvalidEvents)
            );
        }
    }

    #[test]
    fn reset_replays_identically() {
        let mut sampler = sampler(PlaybackMode::OneShot);
        sampler
            .prepare(PrepareSpec {
                sample_rate: 1_000.0,
                max_block_size: 8,
                input_layout: AudioLayout::Mono,
                tempo_bpm: 120.0,
            })
            .unwrap();
        let events = [NoteEvent::NoteOn {
            sample_offset: 0,
            note: 60,
            velocity: 0.5,
        }];
        let mut first = [0.0; 8];
        sampler
            .process(&mut [&mut first], &events, ProcessContext::default())
            .unwrap();
        sampler.reset();
        let mut second = [0.0; 8];
        sampler
            .process(&mut [&mut second], &events, ProcessContext::default())
            .unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn one_shot_tail_preserves_source_duration_across_notes() {
        let mut sampler = sampler(PlaybackMode::OneShot);
        sampler.config.zones[0].low_note = 48;
        sampler.config.zones[0].high_note = 60;
        sampler
            .prepare(PrepareSpec {
                sample_rate: 2_000.0,
                max_block_size: 8,
                input_layout: AudioLayout::Mono,
                tempo_bpm: 120.0,
            })
            .unwrap();

        assert_eq!(sampler.tail_frames(), 128);
    }

    #[test]
    fn reverse_stereo_source_is_interpolated_then_downmixed_for_mono() {
        let config = SamplerConfig {
            polyphony: 1,
            zones: vec![SamplerZone {
                id: "reverse".into(),
                asset_id: "stereo".into(),
                source_start_frame: 0,
                source_end_frame: Some(4),
                root_note: 60,
                low_note: 60,
                high_note: 60,
                low_velocity: 0,
                high_velocity: 127,
                playback_mode: PlaybackMode::OneShot,
                gain_db: 0.0,
                velocity_sensitivity: 0.0,
                attack_ms: 0.0,
                release_ms: 0.0,
                reverse: true,
                choke_group: None,
            }],
        };
        let mut sampler = Sampler::new(
            config,
            vec![SampleAsset {
                id: "stereo".into(),
                sample_rate: 500.0,
                channels: vec![vec![0.0, 2.0, 4.0, 6.0], vec![2.0, 4.0, 6.0, 8.0]],
            }],
        )
        .unwrap();
        sampler
            .prepare(PrepareSpec {
                sample_rate: 1_000.0,
                max_block_size: 4,
                input_layout: AudioLayout::Mono,
                tempo_bpm: 120.0,
            })
            .unwrap();
        let mut output = [0.0; 4];
        sampler
            .process(
                &mut [&mut output],
                &[NoteEvent::NoteOn {
                    sample_offset: 0,
                    note: 60,
                    velocity: 1.0,
                }],
                ProcessContext::default(),
            )
            .unwrap();
        assert_eq!(output, [7.0, 6.0, 5.0, 4.0]);
    }

    #[test]
    fn prepare_rejects_missing_assets_and_invalid_runtime_configuration() {
        let mut missing = sampler(PlaybackMode::OneShot);
        missing.assets.clear();
        assert!(matches!(
            missing.prepare(PrepareSpec::default()),
            Err(InstrumentError::InvalidZone(_))
        ));

        let mut invalid = sampler(PlaybackMode::OneShot);
        invalid.config.zones[0].release_ms = f32::NAN;
        assert!(matches!(
            invalid.prepare(PrepareSpec::default()),
            Err(InstrumentError::InvalidZone(_))
        ));
    }

    #[test]
    fn absent_zone_ids_are_filled_deterministically() {
        let config = SamplerConfig {
            zones: vec![SamplerZone {
                id: String::new(),
                asset_id: "asset".into(),
                source_start_frame: 0,
                source_end_frame: None,
                root_note: 60,
                low_note: 0,
                high_note: 127,
                low_velocity: 0,
                high_velocity: 127,
                playback_mode: PlaybackMode::OneShot,
                gain_db: 0.0,
                velocity_sensitivity: 1.0,
                attack_ms: 0.0,
                release_ms: 20.0,
                reverse: false,
                choke_group: None,
            }],
            ..SamplerConfig::default()
        };
        let sampler = Sampler::new(config, Vec::new()).unwrap();
        assert_eq!(sampler.config.zones[0].id, "zone-0");
    }

    fn pitched_sine_sampler(source_frequency: f64, note: u8) -> Sampler {
        let frames = 8_192;
        let source = (0..frames)
            .map(|frame| {
                (2.0 * core::f64::consts::PI * source_frequency * frame as f64).sin() as f32
            })
            .collect();
        Sampler::new(
            SamplerConfig {
                polyphony: 1,
                zones: vec![SamplerZone {
                    id: "pitched".into(),
                    asset_id: "sine".into(),
                    source_start_frame: 0,
                    source_end_frame: Some(frames),
                    root_note: 60,
                    low_note: note,
                    high_note: note,
                    low_velocity: 0,
                    high_velocity: 127,
                    playback_mode: PlaybackMode::OneShot,
                    gain_db: 0.0,
                    velocity_sensitivity: 0.0,
                    attack_ms: 0.0,
                    release_ms: 0.0,
                    reverse: false,
                    choke_group: None,
                }],
            },
            vec![SampleAsset {
                id: "sine".into(),
                sample_rate: 48_000.0,
                channels: vec![source],
            }],
        )
        .unwrap()
    }

    fn render_test_note(sampler: &mut Sampler, note: u8, frames: usize) -> Vec<f32> {
        sampler
            .prepare(PrepareSpec {
                sample_rate: 48_000.0,
                max_block_size: frames,
                input_layout: AudioLayout::Mono,
                tempo_bpm: 120.0,
            })
            .unwrap();
        let mut output = vec![0.0; frames];
        sampler
            .process(
                &mut [&mut output],
                &[NoteEvent::NoteOn {
                    sample_offset: 0,
                    note,
                    velocity: 1.0,
                }],
                ProcessContext::default(),
            )
            .unwrap();
        output
    }

    #[test]
    fn prepared_sinc_suppresses_an_upshifted_alias() {
        let mut sampler = pitched_sine_sampler(0.30, 72);
        let output = render_test_note(&mut sampler, 72, 2_048);
        let rms = (output[128..1_920]
            .iter()
            .map(|sample| sample * sample)
            .sum::<f32>()
            / 1_792.0)
            .sqrt();
        assert!(rms < 0.01, "aliased stop-band RMS was {rms}");
    }

    #[test]
    fn prepared_sinc_tracks_a_passband_reference() {
        let step = 2.0_f64.powf(7.0 / 12.0);
        let sampler = pitched_sine_sampler(0.04, 67);
        let source = &sampler.assets[0].channels[0];
        let kernel = SincKernel::new(sinc_cutoff(usize::from(sinc_bin_for_step(step))));
        let error_rms = ((128_usize..1_920)
            .map(|frame| {
                let sample = interpolate_prepared(
                    source,
                    frame as f64 * step,
                    0,
                    source.len(),
                    Some(&kernel),
                );
                let expected = (core::f64::consts::TAU * 0.04 * step * frame as f64).sin() as f32;
                (sample - expected).powi(2)
            })
            .sum::<f32>()
            / 1_792.0)
            .sqrt();
        assert!(error_rms < 0.002, "pass-band RMS error was {error_rms}");
    }

    fn spectral_amplitude(samples: &[f32], frequency: f64) -> f64 {
        let (real, imaginary) =
            samples
                .iter()
                .enumerate()
                .fold((0.0, 0.0), |(re, im), (frame, &sample)| {
                    let phase = core::f64::consts::TAU * frequency * frame as f64;
                    (
                        re + f64::from(sample) * phase.cos(),
                        im + f64::from(sample) * phase.sin(),
                    )
                });
        2.0 * real.hypot(imaginary) / samples.len() as f64
    }

    #[test]
    fn transposed_notes_keep_pitch_and_complete_source_duration() {
        for (note, cents) in [(48, 0.0), (60, 0.0), (72, 0.0), (62, -200.0 / 7.0)] {
            for reverse in [false, true] {
                let mut sampler = pitched_sine_sampler(0.02, note);
                sampler.config.zones[0].source_start_frame = 128;
                sampler.config.zones[0].source_end_frame = Some(8_128);
                sampler.config.zones[0].reverse = reverse;
                // Source conversion doubles length; tuning must not change it.
                sampler.assets[0].sample_rate = 24_000.0;
                sampler
                    .prepare(PrepareSpec {
                        sample_rate: 48_000.0,
                        max_block_size: 16_016,
                        input_layout: AudioLayout::Mono,
                        tempo_bpm: 120.0,
                    })
                    .unwrap();
                let mut output = vec![0.0; 16_016];
                sampler
                    .process(
                        &mut [&mut output],
                        &[NoteEvent::NoteOnTuned {
                            sample_offset: 7,
                            note,
                            velocity: 1.0,
                            cents,
                        }],
                        ProcessContext::default(),
                    )
                    .unwrap();
                assert_eq!(sampler.tail_frames(), 16_000);
                assert!(output[..7].iter().all(|&sample| sample == 0.0));
                assert!(output[16_007..].iter().all(|&sample| sample == 0.0));
                assert!(!sampler.voices[0].active);
                assert!(
                    output[15_000..16_000]
                        .iter()
                        .any(|sample| sample.abs() > 0.1),
                    "note {note} reverse {reverse} ended early"
                );
                let pitch = 2.0_f64.powf((f64::from(note) - 60.0 + cents / 100.0) / 12.0);
                let amplitude = spectral_amplitude(&output[2_000..14_000], 0.01 * pitch);
                assert!(
                    amplitude > 0.8,
                    "note {note}, cents {cents}, reverse {reverse}: expected pitch amplitude {amplitude}"
                );
            }
        }
    }

    #[test]
    fn transposed_short_samples_are_audible_without_leading_processor_latency() {
        for note in [48, 72] {
            let mut sampler = pitched_sine_sampler(0.02, note);
            sampler.config.zones[0].source_end_frame = Some(256);
            let output = render_test_note(&mut sampler, note, 512);
            assert!(output[..256].iter().any(|sample| sample.abs() > 0.05));
            assert!(output[256..].iter().all(|&sample| sample == 0.0));
        }
    }
    #[test]
    fn transposed_bass_and_microtonal_pitch_stay_in_tune() {
        for frequency in [55.0, 130.81, 261.625_565, 523.25] {
            for (note, cents) in [(48, 0.0), (72, 0.0), (62, -200.0 / 7.0), (84, 0.0)] {
                let mut sampler = pitched_sine_sampler(frequency / 48_000.0, note);
                sampler.assets[0].channels[0] = (0..65536)
                    .map(|i| {
                        (core::f64::consts::TAU * frequency * f64::from(i) / 48_000.0).sin() as f32
                    })
                    .collect();
                sampler.config.zones[0].source_end_frame = Some(65536);
                sampler
                    .prepare(PrepareSpec {
                        sample_rate: 48_000.0,
                        max_block_size: 65536,
                        input_layout: AudioLayout::Mono,
                        tempo_bpm: 120.0,
                    })
                    .unwrap();
                let mut output = vec![0.0; 65536];
                sampler
                    .process(
                        &mut [&mut output],
                        &[NoteEvent::NoteOnTuned {
                            sample_offset: 0,
                            note,
                            velocity: 1.0,
                            cents,
                        }],
                        ProcessContext::default(),
                    )
                    .unwrap();
                let crossings: Vec<f64> = output[8192..60000]
                    .windows(2)
                    .enumerate()
                    .filter(|(_, p)| p[0] < 0.0 && p[1] >= 0.0)
                    .map(|(i, p)| i as f64 + f64::from(-p[0] / (p[1] - p[0])))
                    .collect();
                let actual = 48000.0 * (crossings.len() - 1) as f64
                    / (crossings.last().unwrap() - crossings[0]);
                let expected =
                    frequency * 2.0_f64.powf((f64::from(note) - 60.0 + cents / 100.0) / 12.0);
                if note == 84 {
                    // >2x stretching decorrelates some bins. Check the actual
                    // tone, since extra crossings in that residual are misleading.
                    // Average short-window magnitudes rather than canceling
                    // randomized phases across the whole render.
                    let amplitude_at = |hz| {
                        output[8_192..59_392]
                            .chunks_exact(1_024)
                            .map(|chunk| spectral_amplitude(chunk, hz / 48_000.0))
                            .sum::<f64>()
                            / 50.0
                    };
                    let amplitude = amplitude_at(expected);
                    let original = amplitude_at(frequency);
                    assert!(
                        amplitude > 0.5 && amplitude > original * 10.0,
                        "{frequency}Hz -> C6: expected pitch amplitude {amplitude}, original {original}"
                    );
                } else {
                    let error_cents = 1200.0 * (actual / expected).log2();
                    assert!(
                        error_cents.abs() < 1.0,
                        "{frequency}Hz -> {note}, {cents} cents: pitch error {error_cents} cents"
                    );
                }
            }
        }
    }

    #[test]
    fn shifted_reset_and_block_partitions_replay_identically() {
        for (note, cents) in [(48, 0.0), (72, 0.0), (62, -200.0 / 7.0)] {
            let mut sampler = pitched_sine_sampler(0.02, note);
            sampler
                .prepare(PrepareSpec {
                    sample_rate: 48_000.0,
                    max_block_size: 8_192,
                    input_layout: AudioLayout::Mono,
                    tempo_bpm: 120.0,
                })
                .unwrap();
            let event = [NoteEvent::NoteOnTuned {
                sample_offset: 0,
                note,
                velocity: 1.0,
                cents,
            }];
            let mut whole = vec![0.0; 8_192];
            sampler
                .process(&mut [&mut whole], &event, ProcessContext::default())
                .unwrap();
            sampler.reset();
            let mut split = vec![0.0; 8_192];
            for (index, block) in split.chunks_mut(127).enumerate() {
                sampler
                    .process(
                        &mut [block],
                        if index == 0 { &event } else { &[] },
                        ProcessContext::default(),
                    )
                    .unwrap();
            }
            assert_eq!(whole, split, "note {note}, cents {cents}");
        }
    }

    #[test]
    fn extreme_midi_tuning_uses_bounded_buffers_at_low_sample_rates() {
        for (root, note, cents) in [(127, 0, -100.0), (0, 127, 100.0)] {
            let mut sampler = sampler(PlaybackMode::OneShot);
            sampler.config.zones[0].root_note = root;
            sampler.config.zones[0].low_note = 0;
            sampler.config.zones[0].high_note = 127;
            sampler
                .prepare(PrepareSpec {
                    sample_rate: 1_000.0,
                    max_block_size: 128,
                    input_layout: AudioLayout::Mono,
                    tempo_bpm: 120.0,
                })
                .unwrap();
            let mut output = [0.0; 128];
            sampler
                .process(
                    &mut [&mut output],
                    &[NoteEvent::NoteOnTuned {
                        sample_offset: 0,
                        note,
                        velocity: 1.0,
                        cents,
                    }],
                    ProcessContext::default(),
                )
                .unwrap();
            assert!(output.iter().all(|sample| sample.is_finite()));
            assert!(output[64..].iter().all(|&sample| sample == 0.0));
        }
    }
}
