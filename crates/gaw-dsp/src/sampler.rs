//! Deterministic zone-based sampler instrument.

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
    NoteOff {
        sample_offset: usize,
        note: u8,
    },
}

impl NoteEvent {
    fn offset(self) -> usize {
        match self {
            Self::NoteOn { sample_offset, .. } | Self::NoteOff { sample_offset, .. } => {
                sample_offset
            }
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
}

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
    gain: f32,
    envelope: f32,
    age: u64,
}

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
            gain: 0.0,
            envelope: 0.0,
            age: 0,
        }
    }
}

/// Built-in `gaw.sampler` instrument.
#[derive(Debug)]
pub struct Sampler {
    pub config: SamplerConfig,
    assets: Vec<SampleAsset>,
    voices: Vec<Voice>,
    sample_rate: f64,
    max_block_size: usize,
    output_layout: AudioLayout,
    absolute_frame: u64,
    next_age: u64,
    prepared: bool,
}

impl Sampler {
    pub fn new(config: SamplerConfig, assets: Vec<SampleAsset>) -> Result<Self, InstrumentError> {
        if let Some(asset) = assets.iter().find(|asset| !asset.is_valid()) {
            return Err(InstrumentError::InvalidAsset(asset.id.clone()));
        }
        let polyphony = config.polyphony.clamp(1, 256);
        Ok(Self {
            config,
            assets,
            voices: vec![Voice::default(); polyphony],
            sample_rate: 0.0,
            max_block_size: 0,
            output_layout: AudioLayout::Stereo,
            absolute_frame: 0,
            next_age: 0,
            prepared: false,
        })
    }

    /// Replace decoded assets outside the process callback.
    pub fn set_assets(&mut self, assets: Vec<SampleAsset>) -> Result<(), InstrumentError> {
        if let Some(asset) = assets.iter().find(|asset| !asset.is_valid()) {
            return Err(InstrumentError::InvalidAsset(asset.id.clone()));
        }
        self.assets = assets;
        self.reset();
        Ok(())
    }

    fn note_on(&mut self, note: u8, velocity: f32) {
        for zone_index in 0..self.config.zones.len() {
            let zone = &self.config.zones[zone_index];
            let velocity_midi = (velocity.clamp(0.0, 1.0) * 127.0).round() as u8;
            if note < zone.low_note
                || note > zone.high_note
                || velocity_midi < zone.low_velocity
                || velocity_midi > zone.high_velocity
            {
                continue;
            }
            let Some(asset_index) = self
                .assets
                .iter()
                .position(|asset| asset.id == zone.asset_id)
            else {
                continue;
            };
            let asset = &self.assets[asset_index];
            let end = zone
                .source_end_frame
                .unwrap_or(asset.channels[0].len())
                .min(asset.channels[0].len());
            if zone.source_start_frame >= end {
                continue;
            }
            if let Some(group) = zone.choke_group {
                for voice in &mut self.voices {
                    if voice.active && self.config.zones[voice.zone].choke_group == Some(group) {
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
            let semitones = f64::from(note) - f64::from(zone.root_note);
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
                step: asset.sample_rate / self.sample_rate * 2.0_f64.powf(semitones / 12.0),
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
                && self.config.zones[voice.zone].playback_mode == PlaybackMode::NoteGated
            {
                voice.released = true;
            }
        }
    }

    fn render_frame(&mut self, output: &mut [&mut [f32]], frame: usize) {
        for voice in &mut self.voices {
            if !voice.active {
                continue;
            }
            let zone = &self.config.zones[voice.zone];
            let asset = &self.assets[voice.asset];
            let end = zone
                .source_end_frame
                .unwrap_or(asset.channels[0].len())
                .min(asset.channels[0].len());
            if voice.position < zone.source_start_frame as f64 || voice.position >= end as f64 {
                voice.active = false;
                continue;
            }
            let index = voice.position.floor() as usize;
            let fraction = (voice.position - index as f64) as f32;
            for (channel_index, channel) in output.iter_mut().enumerate() {
                let source_channel = channel_index.min(asset.channels.len() - 1);
                let source = &asset.channels[source_channel];
                let adjacent = if zone.reverse {
                    index.saturating_sub(1)
                } else {
                    (index + 1).min(end - 1)
                };
                let sample = source[index] + (source[adjacent] - source[index]) * fraction;
                channel[frame] += sample * voice.gain * voice.envelope;
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
            voice.position += if zone.reverse {
                -voice.step
            } else {
                voice.step
            };
        }
    }
}

impl Instrument for Sampler {
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), InstrumentError> {
        self.sample_rate = spec.sample_rate;
        self.max_block_size = spec.max_block_size;
        self.output_layout = spec.input_layout;
        let polyphony = self.config.polyphony.clamp(1, 256);
        self.voices.resize(polyphony, Voice::default());
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
        if events.iter().enumerate().any(|(index, event)| {
            event.offset() >= frames || (index > 0 && events[index - 1].offset() > event.offset())
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
                        self.note_on(note, velocity);
                    }
                    NoteEvent::NoteOn { note, .. } | NoteEvent::NoteOff { note, .. } => {
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
        self.config
            .zones
            .iter()
            .map(|zone| (f64::from(zone.release_ms.max(0.0)) * self.sample_rate / 1000.0) as usize)
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sampler(mode: PlaybackMode) -> Sampler {
        let config = SamplerConfig {
            polyphony: 4,
            zones: vec![SamplerZone {
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
}
