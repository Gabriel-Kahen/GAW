//! Canonical, strict, unit-safe GAW project model.

#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

use std::{fmt, str::FromStr};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::{
    SCHEMA_VERSION,
    processors::{Processor, ProcessorId},
};

macro_rules! id_type {
    ($($name:ident),+ $(,)?) => {$ (
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name { pub fn new() -> Self { Self(Uuid::new_v4()) } }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl fmt::Display for $name { fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) } }
        impl FromStr for $name { type Err = uuid::Error; fn from_str(value: &str) -> Result<Self, Self::Err> { Uuid::parse_str(value).map(Self) } }
        impl From<Uuid> for $name { fn from(value: Uuid) -> Self { Self(value) } }
        impl From<$name> for Uuid { fn from(value: $name) -> Self { value.0 } }
    )+};
}

id_type!(
    ProjectId,
    AssetId,
    AssetRevisionId,
    EventDataId,
    ClipId,
    CompositionId,
    TrackId,
    InstrumentId,
    AutomationLaneId,
    SamplerZoneId
);

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ModelError {
    #[error("{0} must be finite and in {1}")]
    Range(&'static str, &'static str),
    #[error("{0} must not be empty")]
    Empty(&'static str),
    #[error("project path must be normalized, relative, and portable")]
    InvalidProjectPath,
    #[error("invalid inclusive range")]
    InvalidRange,
    #[error("sampler polyphony must be greater than zero")]
    InvalidPolyphony,
    #[error("automation point times must be strictly increasing")]
    UnorderedAutomation,
}

macro_rules! finite_unit {
    ($name:ident, $label:literal, $range:literal, $valid:expr $(, $schema:meta)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, JsonSchema)]
        #[serde(transparent)]
        pub struct $name($(#[$schema])? f64);
        impl $name {
            pub fn new(value: f64) -> Result<Self, ModelError> {
                if value.is_finite() && ($valid)(value) {
                    Ok(Self(value))
                } else {
                    Err(ModelError::Range($label, $range))
                }
            }
            pub const fn value(self) -> f64 {
                self.0
            }
        }
        impl Eq for $name {}
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                Self::new(f64::deserialize(deserializer)?).map_err(serde::de::Error::custom)
            }
        }
        impl TryFrom<f64> for $name {
            type Error = ModelError;
            fn try_from(v: f64) -> Result<Self, Self::Error> {
                Self::new(v)
            }
        }
        impl From<$name> for f64 {
            fn from(v: $name) -> Self {
                v.0
            }
        }
    };
}

finite_unit!(
    Beats,
    "beats",
    "[0, infinity)",
    |v: f64| v >= 0.0,
    schemars(range(min = 0.0))
);
finite_unit!(
    Seconds,
    "seconds",
    "[0, infinity)",
    |v: f64| v >= 0.0,
    schemars(range(min = 0.0))
);
finite_unit!(
    Milliseconds,
    "milliseconds",
    "[0, infinity)",
    |v: f64| v >= 0.0,
    schemars(range(min = 0.0))
);
finite_unit!(
    Hertz,
    "hertz",
    "(0, infinity)",
    |v: f64| v > 0.0,
    schemars(range(min = 0.0))
);
finite_unit!(Decibels, "decibels", "(-infinity, infinity)", |_v: f64| {
    true
});
finite_unit!(
    Ratio,
    "ratio",
    "[0, 1]",
    |v: f64| (0.0..=1.0).contains(&v),
    schemars(range(min = 0.0, max = 1.0))
);
finite_unit!(
    Bipolar,
    "bipolar value",
    "[-1, 1]",
    |v: f64| (-1.0..=1.0).contains(&v),
    schemars(range(min = -1.0, max = 1.0))
);
finite_unit!(
    Bpm,
    "bpm",
    "(0, infinity)",
    |v: f64| v > 0.0,
    schemars(range(min = 0.0))
);
finite_unit!(
    PlaybackRatio,
    "playback ratio",
    "(0, infinity)",
    |v: f64| v > 0.0,
    schemars(range(min = 0.0))
);
finite_unit!(
    Semitones,
    "semitones",
    "(-infinity, infinity)",
    |_v: f64| true
);
finite_unit!(Cents, "cents", "(-infinity, infinity)", |_v: f64| true);
finite_unit!(Scalar, "number", "(-infinity, infinity)", |_v: f64| true);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct SampleRate(#[schemars(range(min = 1))] u32);
impl SampleRate {
    pub fn new(value: u32) -> Result<Self, ModelError> {
        (value > 0)
            .then_some(Self(value))
            .ok_or(ModelError::Range("sample rate", "(0, infinity)"))
    }
    pub const fn value(self) -> u32 {
        self.0
    }
}
impl<'de> Deserialize<'de> for SampleRate {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(u32::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct FrameCount(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct FramePosition(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FrameRange {
    pub start: FramePosition,
    pub length: FrameCount,
}

macro_rules! midi_u7 {
    ($name:ident, $label:literal) => {
        #[derive(
            Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, JsonSchema,
        )]
        #[serde(transparent)]
        pub struct $name(#[schemars(range(max = 127))] u8);
        impl $name {
            pub fn new(value: u8) -> Result<Self, ModelError> {
                (value <= 127)
                    .then_some(Self(value))
                    .ok_or(ModelError::Range($label, "[0, 127]"))
            }
            pub const fn value(self) -> u8 {
                self.0
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                Self::new(u8::deserialize(d)?).map_err(serde::de::Error::custom)
            }
        }
        impl TryFrom<u8> for $name {
            type Error = ModelError;
            fn try_from(v: u8) -> Result<Self, Self::Error> {
                Self::new(v)
            }
        }
        impl From<$name> for u8 {
            fn from(v: $name) -> Self {
                v.0
            }
        }
    };
}
midi_u7!(MidiNote, "MIDI note");
midi_u7!(MidiVelocity, "MIDI velocity");

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ProjectPath(#[schemars(length(min = 1))] String);
impl ProjectPath {
    pub fn new(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = value.into();
        if value.is_empty()
            || value.starts_with('/')
            || value.starts_with('\\')
            || value.contains(':')
            || value
                .split(['/', '\\'])
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
        {
            return Err(ModelError::InvalidProjectPath);
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for ProjectPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// A lowercase hexadecimal SHA-256 digest.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct ContentHash(#[schemars(length(equal = 64), regex(pattern = r"^[0-9a-f]{64}$"))] String);
impl ContentHash {
    pub fn new(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = value.into();
        if value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            Ok(Self(value))
        } else {
            Err(ModelError::Range(
                "content hash",
                "64 lowercase hexadecimal characters",
            ))
        }
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for ContentHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for ContentHash {
    type Err = ModelError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}
impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLayout {
    Mono,
    Stereo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceRange {
    pub start: Seconds,
    pub duration: Seconds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteRange {
    pub low: MidiNote,
    pub high: MidiNote,
}
impl NoteRange {
    pub fn new(low: u8, high: u8) -> Result<Self, ModelError> {
        let (low, high) = (MidiNote::new(low)?, MidiNote::new(high)?);
        (low <= high)
            .then_some(Self { low, high })
            .ok_or(ModelError::InvalidRange)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VelocityRange {
    pub low: MidiVelocity,
    pub high: MidiVelocity,
}
impl VelocityRange {
    pub fn new(low: u8, high: u8) -> Result<Self, ModelError> {
        let (low, high) = (MidiVelocity::new(low)?, MidiVelocity::new(high)?);
        (low <= high)
            .then_some(Self { low, high })
            .ok_or(ModelError::InvalidRange)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub schema_version: u32,
    pub id: ProjectId,
    pub name: String,
    pub root_composition_id: CompositionId,
    pub bpm: Bpm,
    pub sample_rate: SampleRate,
    pub settings: ProjectSettings,
    pub assets: Vec<AudioAsset>,
    pub event_data: Vec<EventData>,
    pub compositions: Vec<Composition>,
    pub tracks: Vec<Track>,
    pub automation: Vec<AutomationLane>,
}

impl Project {
    pub fn new(name: impl Into<String>, bpm: Bpm, sample_rate: SampleRate) -> Self {
        let root = Composition::new("Song", Beats::new(0.0).expect("zero is valid"));
        Self {
            schema_version: SCHEMA_VERSION,
            id: ProjectId::new(),
            name: name.into(),
            root_composition_id: root.id,
            bpm,
            sample_rate,
            settings: ProjectSettings::default(),
            assets: vec![],
            event_data: vec![],
            compositions: vec![root],
            tracks: vec![],
            automation: vec![],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectSettings {
    pub maximum_tail: Seconds,
    pub random_seed: u64,
    pub cache_budget_bytes: Option<u64>,
}
impl Default for ProjectSettings {
    fn default() -> Self {
        Self {
            maximum_tail: Seconds::new(60.0).expect("valid"),
            random_seed: 0,
            cache_budget_bytes: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Composition {
    pub id: CompositionId,
    pub name: String,
    pub length: Beats,
    pub output_layout: ChannelLayout,
    pub track_ids: Vec<TrackId>,
    pub output_effects: Vec<Processor>,
}
impl Composition {
    pub fn new(name: impl Into<String>, length: Beats) -> Self {
        Self {
            id: CompositionId::new(),
            name: name.into(),
            length,
            output_layout: ChannelLayout::Stereo,
            track_ids: vec![],
            output_effects: vec![],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Track {
    pub id: TrackId,
    pub composition_id: CompositionId,
    pub name: String,
    pub kind: TrackKind,
    pub muted: bool,
    pub solo: bool,
    pub clips: Vec<Clip>,
    pub instrument: Option<Instrument>,
    pub effects: Vec<Processor>,
}
impl Track {
    pub fn new(composition_id: CompositionId, name: impl Into<String>) -> Self {
        Self::audio(composition_id, name)
    }
    pub fn audio(composition_id: CompositionId, name: impl Into<String>) -> Self {
        Self {
            id: TrackId::new(),
            composition_id,
            name: name.into(),
            kind: TrackKind::Audio,
            muted: false,
            solo: false,
            clips: vec![],
            instrument: None,
            effects: vec![],
        }
    }
    pub fn event(
        composition_id: CompositionId,
        name: impl Into<String>,
        instrument: Instrument,
    ) -> Self {
        Self {
            id: TrackId::new(),
            composition_id,
            name: name.into(),
            kind: TrackKind::Event,
            muted: false,
            solo: false,
            clips: vec![],
            instrument: Some(instrument),
            effects: vec![],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrackKind {
    Audio,
    Event,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Clip {
    Audio(AudioClip),
    Event(EventClip),
    Composition(CompositionClip),
}
impl Clip {
    pub const fn id(&self) -> ClipId {
        match self {
            Self::Audio(v) => v.id,
            Self::Event(v) => v.id,
            Self::Composition(v) => v.id,
        }
    }
    pub const fn start(&self) -> Beats {
        match self {
            Self::Audio(v) => v.start,
            Self::Event(v) => v.start,
            Self::Composition(v) => v.start,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TempoSync {
    None,
    Repitch,
    Stretch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FadeCurve {
    Linear,
    EqualPower,
    Exponential,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Fade {
    pub duration: Seconds,
    pub curve: FadeCurve,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioClip {
    pub id: ClipId,
    pub name: String,
    pub start: Beats,
    pub duration: Beats,
    pub muted: bool,
    pub asset_id: AssetId,
    pub source: SourceRange,
    pub fade_in: Option<Fade>,
    pub fade_out: Option<Fade>,
    pub reverse: bool,
    pub tempo_sync: TempoSync,
    pub effects: Vec<Processor>,
}
impl AudioClip {
    pub fn new(asset_id: AssetId, start: Beats, duration: Beats, source: SourceRange) -> Self {
        Self {
            id: ClipId::new(),
            name: String::new(),
            start,
            duration,
            muted: false,
            asset_id,
            source,
            fade_in: None,
            fade_out: None,
            reverse: false,
            tempo_sync: TempoSync::None,
            effects: vec![],
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventClip {
    pub id: ClipId,
    pub name: String,
    pub start: Beats,
    pub duration: Beats,
    pub muted: bool,
    pub event_data_id: EventDataId,
    pub source_start: Beats,
}
impl EventClip {
    pub fn new(event_data_id: EventDataId, start: Beats, duration: Beats) -> Self {
        Self {
            id: ClipId::new(),
            name: String::new(),
            start,
            duration,
            muted: false,
            event_data_id,
            source_start: Beats::new(0.0).expect("valid"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompositionClip {
    pub id: ClipId,
    pub name: String,
    pub start: Beats,
    pub duration: Beats,
    pub muted: bool,
    pub composition_id: CompositionId,
    pub source_start: Beats,
    pub effects: Vec<Processor>,
}
impl CompositionClip {
    pub fn new(composition_id: CompositionId, start: Beats, duration: Beats) -> Self {
        Self {
            id: ClipId::new(),
            name: String::new(),
            start,
            duration,
            muted: false,
            composition_id,
            source_start: Beats::new(0.0).expect("valid"),
            effects: vec![],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioAsset {
    pub id: AssetId,
    pub name: String,
    pub definition: AudioAssetDefinition,
    pub tempo: Option<AssetTempo>,
    pub revisions: Vec<AudioAssetRevision>,
    pub current_revision_id: Option<AssetRevisionId>,
}
impl AudioAsset {
    pub fn imported(name: impl Into<String>, source: ImportedAudio) -> Self {
        Self {
            id: AssetId::new(),
            name: name.into(),
            definition: AudioAssetDefinition::Imported(source),
            tempo: None,
            revisions: vec![],
            current_revision_id: None,
        }
    }
    /// Appends an immutable render and atomically makes it current.
    pub fn publish_revision(&mut self, revision: AudioAssetRevision) -> Result<(), ModelError> {
        if self.revisions.iter().any(|old| old.id == revision.id) {
            return Err(ModelError::Range("revision id", "unique within asset"));
        }
        self.current_revision_id = Some(revision.id);
        self.revisions.push(revision);
        Ok(())
    }
    pub fn current_revision(&self) -> Option<&AudioAssetRevision> {
        let id = self.current_revision_id?;
        self.revisions.iter().find(|revision| revision.id == id)
    }
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.name.trim().is_empty() {
            return Err(ModelError::Empty("asset name"));
        }
        if self.current_revision_id.is_some() && self.current_revision().is_none() {
            return Err(ModelError::Range("current revision", "asset revisions"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AudioAssetDefinition {
    Imported(ImportedAudio),
    InstrumentGenerated {
        instrument_id: InstrumentId,
        event_data_id: EventDataId,
    },
    CompositionGenerated {
        composition_id: CompositionId,
    },
    Processed {
        source_asset_id: AssetId,
        transforms: Vec<AudioTransform>,
        effects: Vec<Processor>,
    },
    Materialized {
        revision_id: AssetRevisionId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportedAudio {
    pub media_path: ProjectPath,
    pub original_filename: String,
    pub content_hash: ContentHash,
    pub sample_rate: SampleRate,
    pub layout: ChannelLayout,
    pub frames: FrameCount,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AudioTransform {
    Trim(SourceRange),
    Reverse,
    Repitch { ratio: PlaybackRatio },
    Stretch { ratio: PlaybackRatio },
    FadeIn(Fade),
    FadeOut(Fade),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AudioAssetRevision {
    pub id: AssetRevisionId,
    pub content_hash: ContentHash,
    pub definition_hash: ContentHash,
    pub dependency_revision_ids: Vec<AssetRevisionId>,
    pub render_context: RenderContext,
    pub media_path: ProjectPath,
    pub frames: FrameCount,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenderContext {
    pub sample_rate: SampleRate,
    pub layout: ChannelLayout,
    pub bpm: Bpm,
    /// `None` means the complete logical asset; reads may request a bounded range.
    pub requested_range: Option<FrameRange>,
    pub engine_version: String,
    pub random_seed: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssetTempo {
    pub bpm: Bpm,
    pub first_beat: Seconds,
}

impl AssetTempo {
    pub fn playback_ratio(self, project_bpm: Bpm) -> Result<PlaybackRatio, ModelError> {
        PlaybackRatio::new(project_bpm.value() / self.bpm.value())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventData {
    pub id: EventDataId,
    pub name: String,
    pub events: Vec<Event>,
}
impl EventData {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: EventDataId::new(),
            name: name.into(),
            events: vec![],
        }
    }
    pub fn sort(&mut self) {
        self.events
            .sort_by(|a, b| a.time().value().total_cmp(&b.time().value()));
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Event {
    Note(NoteEvent),
    Control(ControlEvent),
    PitchBend(PitchBendEvent),
}
impl Event {
    pub const fn time(&self) -> Beats {
        match self {
            Self::Note(v) => v.start,
            Self::Control(v) => v.time,
            Self::PitchBend(v) => v.time,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteEvent {
    pub start: Beats,
    pub duration: Beats,
    pub note: MidiNote,
    pub velocity: MidiVelocity,
    pub release_velocity: MidiVelocity,
}
impl NoteEvent {
    pub fn new(start: Beats, duration: Beats, note: u8, velocity: u8) -> Result<Self, ModelError> {
        Ok(Self {
            start,
            duration,
            note: MidiNote::new(note)?,
            velocity: MidiVelocity::new(velocity)?,
            release_velocity: MidiVelocity::new(64).expect("valid"),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ControlEvent {
    pub time: Beats,
    pub controller: String,
    pub value: Ratio,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PitchBendEvent {
    pub time: Beats,
    pub value: Bipolar,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Instrument {
    pub id: InstrumentId,
    pub name: String,
    pub kind: InstrumentKind,
}
impl Instrument {
    pub fn sampler(name: impl Into<String>, sampler: Sampler) -> Self {
        Self {
            id: InstrumentId::new(),
            name: name.into(),
            kind: InstrumentKind::Sampler(sampler),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    content = "data",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum InstrumentKind {
    Sampler(Sampler),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Sampler {
    pub zones: Vec<SamplerZone>,
    pub polyphony: u16,
    pub voice_stealing: VoiceStealing,
    pub output_gain: Decibels,
}
impl Sampler {
    pub fn new(polyphony: u16) -> Result<Self, ModelError> {
        if polyphony == 0 {
            Err(ModelError::InvalidPolyphony)
        } else {
            Ok(Self {
                zones: vec![],
                polyphony,
                voice_stealing: VoiceStealing::Oldest,
                output_gain: Decibels::new(0.0).expect("valid"),
            })
        }
    }
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.polyphony == 0 {
            return Err(ModelError::InvalidPolyphony);
        }
        for zone in &self.zones {
            zone.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum VoiceStealing {
    Oldest,
    Quietest,
    LowestVelocity,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SamplerZone {
    pub id: SamplerZoneId,
    pub name: String,
    pub asset_id: AssetId,
    pub source: SourceRange,
    pub root_note: MidiNote,
    pub note_range: NoteRange,
    pub velocity_range: VelocityRange,
    pub playback: SamplerPlayback,
    pub gain: Decibels,
    pub velocity_sensitivity: Ratio,
    pub attack: Milliseconds,
    pub release: Milliseconds,
    pub reverse: bool,
    pub choke_group: Option<u16>,
}
impl SamplerZone {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.name.trim().is_empty() {
            Err(ModelError::Empty("sampler zone name"))
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SamplerPlayback {
    OneShot,
    NoteGated,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AutomationLane {
    pub id: AutomationLaneId,
    pub composition_id: CompositionId,
    pub name: String,
    pub target: AutomationTarget,
    pub points: Vec<AutomationPoint>,
}
impl AutomationLane {
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.points.is_empty() {
            Err(ModelError::Empty("automation points"))
        } else if self
            .points
            .windows(2)
            .all(|pair| pair[0].time < pair[1].time)
        {
            Ok(())
        } else {
            Err(ModelError::UnorderedAutomation)
        }
    }
    pub fn value_at(&self, time: Beats) -> Option<AutomationValue> {
        let first = self.points.first()?;
        if time <= first.time {
            return Some(first.value);
        }
        for pair in self.points.windows(2) {
            if time <= pair[1].time {
                return Some(pair[0].interpolate(pair[1], time));
            }
        }
        self.points.last().map(|point| point.value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum AutomationTarget {
    AudioClipProcessor {
        track_id: TrackId,
        clip_id: ClipId,
        processor_id: ProcessorId,
        parameter_id: String,
    },
    CompositionClipProcessor {
        track_id: TrackId,
        clip_id: ClipId,
        processor_id: ProcessorId,
        parameter_id: String,
    },
    TrackProcessor {
        track_id: TrackId,
        processor_id: ProcessorId,
        parameter_id: String,
    },
    CompositionOutputProcessor {
        processor_id: ProcessorId,
        parameter_id: String,
    },
    Instrument {
        track_id: TrackId,
        instrument_id: InstrumentId,
        parameter_id: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AutomationPoint {
    pub time: Beats,
    pub value: AutomationValue,
    pub curve: AutomationCurve,
}
impl AutomationPoint {
    fn interpolate(self, next: Self, time: Beats) -> AutomationValue {
        if self.curve == AutomationCurve::Step || self.value.unit() != next.value.unit() {
            return self.value;
        }
        let span = next.time.value() - self.time.value();
        if span <= 0.0 {
            return next.value;
        }
        let mut t = (time.value() - self.time.value()) / span;
        if self.curve == AutomationCurve::Smooth {
            t = t * t * (3.0 - 2.0 * t);
        }
        AutomationValue::from_unit(
            self.value.unit(),
            self.value.number() + (next.value.number() - self.value.number()) * t,
        )
        .unwrap_or(self.value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AutomationCurve {
    Step,
    Linear,
    Smooth,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "unit",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum AutomationValue {
    Number(Scalar),
    Decibels(Decibels),
    Hertz(Hertz),
    Seconds(Seconds),
    Milliseconds(Milliseconds),
    Beats(Beats),
    Ratio(Ratio),
    Bipolar(Bipolar),
    Semitones(Semitones),
    Cents(Cents),
}
impl AutomationValue {
    pub const fn unit(self) -> AutomationUnit {
        match self {
            Self::Number(_) => AutomationUnit::Number,
            Self::Decibels(_) => AutomationUnit::Decibels,
            Self::Hertz(_) => AutomationUnit::Hertz,
            Self::Seconds(_) => AutomationUnit::Seconds,
            Self::Milliseconds(_) => AutomationUnit::Milliseconds,
            Self::Beats(_) => AutomationUnit::Beats,
            Self::Ratio(_) => AutomationUnit::Ratio,
            Self::Bipolar(_) => AutomationUnit::Bipolar,
            Self::Semitones(_) => AutomationUnit::Semitones,
            Self::Cents(_) => AutomationUnit::Cents,
        }
    }
    pub const fn number(self) -> f64 {
        match self {
            Self::Number(v) => v.value(),
            Self::Decibels(v) => v.value(),
            Self::Hertz(v) => v.value(),
            Self::Seconds(v) => v.value(),
            Self::Milliseconds(v) => v.value(),
            Self::Beats(v) => v.value(),
            Self::Ratio(v) => v.value(),
            Self::Bipolar(v) => v.value(),
            Self::Semitones(v) => v.value(),
            Self::Cents(v) => v.value(),
        }
    }
    pub fn from_unit(unit: AutomationUnit, value: f64) -> Result<Self, ModelError> {
        Ok(match unit {
            AutomationUnit::Number => Self::Number(Scalar::new(value)?),
            AutomationUnit::Decibels => Self::Decibels(Decibels::new(value)?),
            AutomationUnit::Hertz => Self::Hertz(Hertz::new(value)?),
            AutomationUnit::Seconds => Self::Seconds(Seconds::new(value)?),
            AutomationUnit::Milliseconds => Self::Milliseconds(Milliseconds::new(value)?),
            AutomationUnit::Beats => Self::Beats(Beats::new(value)?),
            AutomationUnit::Ratio => Self::Ratio(Ratio::new(value)?),
            AutomationUnit::Bipolar => Self::Bipolar(Bipolar::new(value)?),
            AutomationUnit::Semitones => Self::Semitones(Semitones::new(value)?),
            AutomationUnit::Cents => Self::Cents(Cents::new(value)?),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AutomationUnit {
    Number,
    Decibels,
    Hertz,
    Seconds,
    Milliseconds,
    Beats,
    Ratio,
    Bipolar,
    Semitones,
    Cents,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beats(value: f64) -> Beats {
        Beats::new(value).expect("test beat value is valid")
    }

    #[test]
    fn ids_are_unique_and_round_trip() {
        let ids: Vec<_> = (0..128).map(|_| AssetId::new()).collect();
        for (index, id) in ids.iter().enumerate() {
            assert_eq!(id.to_string().parse::<AssetId>().unwrap(), *id);
            assert!(!ids[index + 1..].contains(id));
        }
    }

    #[test]
    fn finite_units_reject_invalid_json() {
        for json in ["-0.01", "null", "\"NaN\""] {
            assert!(
                serde_json::from_str::<Seconds>(json).is_err(),
                "accepted {json}"
            );
        }
        for value in [-0.1, 1.1, f64::INFINITY, f64::NAN] {
            assert!(Ratio::new(value).is_err(), "accepted {value}");
        }
        for value in [0.0, 0.25, 1.0] {
            let decoded = serde_json::from_str::<Ratio>(
                &serde_json::to_string(&Ratio::new(value).unwrap()).unwrap(),
            )
            .unwrap()
            .value();
            assert!((decoded - value).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn midi_and_ranges_are_checked() {
        assert!(MidiNote::new(128).is_err());
        assert!(NoteRange::new(60, 59).is_err());
        assert!(VelocityRange::new(1, 127).is_ok());
        assert!(serde_json::from_str::<MidiVelocity>("255").is_err());
    }

    #[test]
    fn canonical_json_is_strict() {
        let json = serde_json::to_value(ProjectSettings::default()).unwrap();
        let mut object = json.as_object().unwrap().clone();
        object.insert("mystery".into(), true.into());
        assert!(serde_json::from_value::<ProjectSettings>(object.into()).is_err());
        assert!(serde_json::from_str::<TempoSync>("\"stretch\"").is_ok());
        assert!(serde_json::from_str::<TempoSync>("\"warped\"").is_err());
    }

    #[test]
    fn paths_and_hashes_are_portable_and_validated() {
        assert!(ProjectPath::new("assets/media/kick.wav").is_ok());
        for path in [
            "",
            "/tmp/a.wav",
            "../a.wav",
            "assets/../../a.wav",
            r"C:\audio\a.wav",
            "assets/./a.wav",
            "assets//a.wav",
        ] {
            assert!(ProjectPath::new(path).is_err());
        }
        let hash = "ab".repeat(32);
        assert_eq!(ContentHash::new(&hash).unwrap().as_str(), hash);
        assert!(ContentHash::new("AB".repeat(32)).is_err());
        assert!(ContentHash::new("0".repeat(63)).is_err());
    }

    #[test]
    fn sampler_schema_covers_ranges_and_playback() {
        assert!(Sampler::new(0).is_err());
        let mut sampler = Sampler::new(32).unwrap();
        sampler.zones.push(SamplerZone {
            id: SamplerZoneId::new(),
            name: "snare".into(),
            asset_id: AssetId::new(),
            source: SourceRange {
                start: Seconds::new(0.1).unwrap(),
                duration: Seconds::new(0.5).unwrap(),
            },
            root_note: MidiNote::new(38).unwrap(),
            note_range: NoteRange::new(38, 38).unwrap(),
            velocity_range: VelocityRange::new(1, 127).unwrap(),
            playback: SamplerPlayback::OneShot,
            gain: Decibels::new(-3.0).unwrap(),
            velocity_sensitivity: Ratio::new(0.8).unwrap(),
            attack: Milliseconds::new(2.0).unwrap(),
            release: Milliseconds::new(80.0).unwrap(),
            reverse: false,
            choke_group: Some(1),
        });
        assert!(sampler.validate().is_ok());
        let round_trip: Sampler =
            serde_json::from_value(serde_json::to_value(&sampler).unwrap()).unwrap();
        assert_eq!(round_trip, sampler);
    }

    #[test]
    fn automation_requires_order_and_interpolates() {
        let target = AutomationTarget::CompositionOutputProcessor {
            processor_id: ProcessorId::new("gain").unwrap(),
            parameter_id: "gain_db".into(),
        };
        let mut lane = AutomationLane {
            id: AutomationLaneId::new(),
            composition_id: CompositionId::new(),
            name: "fade".into(),
            target,
            points: vec![
                AutomationPoint {
                    time: beats(0.0),
                    value: AutomationValue::Decibels(Decibels::new(-12.0).unwrap()),
                    curve: AutomationCurve::Linear,
                },
                AutomationPoint {
                    time: beats(4.0),
                    value: AutomationValue::Decibels(Decibels::new(0.0).unwrap()),
                    curve: AutomationCurve::Linear,
                },
            ],
        };
        assert!(lane.validate().is_ok());
        assert!((lane.value_at(beats(2.0)).unwrap().number() + 6.0).abs() < f64::EPSILON);
        lane.points.push(lane.points[1]);
        assert_eq!(lane.validate(), Err(ModelError::UnorderedAutomation));
        lane.points.clear();
        assert_eq!(lane.validate(), Err(ModelError::Empty("automation points")));
    }

    #[test]
    fn project_constructor_creates_a_valid_root() {
        let project = Project::new(
            "Test",
            Bpm::new(120.0).unwrap(),
            SampleRate::new(48_000).unwrap(),
        );
        assert_eq!(project.schema_version, SCHEMA_VERSION);
        assert_eq!(project.compositions.len(), 1);
        assert_eq!(project.compositions[0].id, project.root_composition_id);
    }

    #[test]
    fn asset_tempo_uses_the_canonical_playback_ratio() {
        let tempo = AssetTempo {
            bpm: Bpm::new(110.0).unwrap(),
            first_beat: Seconds::new(0.0).unwrap(),
        };
        let ratio = tempo
            .playback_ratio(Bpm::new(120.0).unwrap())
            .unwrap()
            .value();
        assert!((ratio - 120.0 / 110.0).abs() < f64::EPSILON);
    }
}
