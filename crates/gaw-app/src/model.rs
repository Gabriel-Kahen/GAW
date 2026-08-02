#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

use gaw_core::{
    AssetId, ClipId, Command, CompositionId, EditHistory, ProcessorId, ProcessorStack, Project,
    TrackId, Transaction,
};

pub const MIN_BPM: f32 = 40.0;
pub const MAX_BPM: f32 = 240.0;
pub const HIGHLIGHT_SECONDS: f64 = 2.4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    None,
    Repitch,
    Stretch,
}

fn asset_duration(asset: &gaw_core::AudioAsset) -> Option<f64> {
    match &asset.definition {
        gaw_core::AudioAssetDefinition::Imported(source) => {
            Some(source.frames.0 as f64 / f64::from(source.sample_rate.value()))
        }
        _ => asset.current_revision().map(|revision| {
            revision.frames.0 as f64 / f64::from(revision.render_context.sample_rate.value())
        }),
    }
}

fn processor_stack<'a>(
    project: &'a Project,
    stack: &ProcessorStack,
) -> Option<&'a [gaw_core::Processor]> {
    match stack {
        ProcessorStack::CompositionOutput { composition_id } => project
            .compositions
            .iter()
            .find(|composition| composition.id == *composition_id)
            .map(|composition| composition.output_effects.as_slice()),
        ProcessorStack::Track { track_id } => project
            .tracks
            .iter()
            .find(|track| track.id == *track_id)
            .map(|track| track.effects.as_slice()),
        ProcessorStack::Clip { track_id, clip_id } => project
            .tracks
            .iter()
            .find(|track| track.id == *track_id)
            .and_then(|track| track.clips.iter().find(|clip| clip.id() == *clip_id))
            .and_then(|clip| match clip {
                gaw_core::Clip::Audio(clip) => Some(clip.effects.as_slice()),
                gaw_core::Clip::Composition(_) | gaw_core::Clip::Event(_) => None,
            }),
        ProcessorStack::CompositionClip { track_id, clip_id } => project
            .tracks
            .iter()
            .find(|track| track.id == *track_id)
            .and_then(|track| track.clips.iter().find(|clip| clip.id() == *clip_id))
            .and_then(|clip| match clip {
                gaw_core::Clip::Composition(clip) => Some(clip.effects.as_slice()),
                gaw_core::Clip::Audio(_) | gaw_core::Clip::Event(_) => None,
            }),
    }
}

fn find_processor(
    project: &Project,
    stack: &ProcessorStack,
    processor_id: &ProcessorId,
) -> Option<gaw_core::Processor> {
    processor_stack(project, stack)?
        .iter()
        .find(|processor| processor.id == *processor_id)
        .cloned()
}

fn set_parameter(
    processor: &mut gaw_core::Processor,
    parameter_id: &str,
    value: serde_json::Value,
) -> bool {
    let Ok(mut encoded) = serde_json::to_value(&*processor) else {
        return false;
    };
    let Some(parameter) = encoded
        .get_mut("parameters")
        .and_then(|parameters| parameters.get_mut(parameter_id))
    else {
        return false;
    };
    *parameter = value;
    let Ok(updated) = serde_json::from_value(encoded) else {
        return false;
    };
    *processor = updated;
    true
}

fn effect_view(processor: &gaw_core::Processor) -> Effect {
    let encoded = serde_json::to_value(processor).unwrap_or_default();
    let descriptors = processor.kind.parameter_descriptors();
    let parameters = descriptors
        .iter()
        .filter_map(|descriptor| {
            let value = encoded.get("parameters")?.get(descriptor.id)?.clone();
            Some(Parameter {
                id: descriptor.id.to_owned(),
                label: descriptor.id.replace('_', " "),
                value,
                value_type: descriptor.value_type,
                range: descriptor.range.map(|range| (range.minimum, range.maximum)),
                choices: descriptor.choices.iter().map(ToString::to_string).collect(),
                unit: format!("{:?}", descriptor.unit).to_lowercase(),
                automatable: descriptor.automation == gaw_core::AutomationSupport::Continuous
                    || descriptors.iter().any(|nested| {
                        nested
                            .id
                            .strip_prefix(descriptor.id)
                            .is_some_and(|suffix| suffix.starts_with("[]."))
                            && nested.automation == gaw_core::AutomationSupport::Continuous
                    }),
                display_hint: format!("{:?}", descriptor.display_hint).to_lowercase(),
            })
        })
        .collect();
    Effect {
        id: processor.id.to_string(),
        name: processor
            .kind
            .type_id()
            .trim_start_matches("gaw.")
            .replace('_', " "),
        kind: processor.kind.type_id().to_owned(),
        enabled: processor.enabled,
        parameters,
    }
}

#[allow(clippy::too_many_lines)]
fn adapt_project(
    project: &Project,
    asset_waveforms: Option<&HashMap<String, Arc<[f32]>>>,
    clip_waveforms: Option<&HashMap<String, Arc<[f32]>>>,
) -> (Vec<Asset>, Vec<Composition>) {
    let assets = project
        .assets
        .iter()
        .map(|asset| {
            let id = asset.id.to_string();
            let revision = asset.current_revision();
            let (definition, media_path, content_hash, sample_rate, frames, channels, effects) =
                match &asset.definition {
                    gaw_core::AudioAssetDefinition::Imported(source) => (
                        "imported",
                        Some(source.media_path.as_str().to_owned()),
                        Some(source.content_hash.to_string()),
                        source.sample_rate.value(),
                        source.frames.0,
                        match source.layout {
                            gaw_core::ChannelLayout::Mono => 1,
                            gaw_core::ChannelLayout::Stereo => 2,
                        },
                        Vec::new(),
                    ),
                    gaw_core::AudioAssetDefinition::InstrumentGenerated { .. } => (
                        "instrument_generated",
                        revision.map(|value| value.media_path.as_str().to_owned()),
                        revision.map(|value| value.content_hash.to_string()),
                        revision.map_or(0, |value| value.render_context.sample_rate.value()),
                        revision.map_or(0, |value| value.frames.0),
                        revision.map_or(0, |value| match value.render_context.layout {
                            gaw_core::ChannelLayout::Mono => 1,
                            gaw_core::ChannelLayout::Stereo => 2,
                        }),
                        Vec::new(),
                    ),
                    gaw_core::AudioAssetDefinition::CompositionGenerated { .. } => (
                        "composition_generated",
                        revision.map(|value| value.media_path.as_str().to_owned()),
                        revision.map(|value| value.content_hash.to_string()),
                        revision.map_or(0, |value| value.render_context.sample_rate.value()),
                        revision.map_or(0, |value| value.frames.0),
                        2,
                        Vec::new(),
                    ),
                    gaw_core::AudioAssetDefinition::Processed { effects, .. } => (
                        "processed",
                        revision.map(|value| value.media_path.as_str().to_owned()),
                        revision.map(|value| value.content_hash.to_string()),
                        revision.map_or(0, |value| value.render_context.sample_rate.value()),
                        revision.map_or(0, |value| value.frames.0),
                        2,
                        effects.iter().map(effect_view).collect(),
                    ),
                    gaw_core::AudioAssetDefinition::Materialized { .. } => (
                        "materialized",
                        revision.map(|value| value.media_path.as_str().to_owned()),
                        revision.map(|value| value.content_hash.to_string()),
                        revision.map_or(0, |value| value.render_context.sample_rate.value()),
                        revision.map_or(0, |value| value.frames.0),
                        2,
                        Vec::new(),
                    ),
                };
            let duration = asset_duration(asset).unwrap_or(0.0) as f32;
            Asset {
                waveform: asset_waveforms
                    .and_then(|cache| cache.get(&id).cloned())
                    .unwrap_or_else(|| waveform(id_seed(&id), 256)),
                id: id.clone(),
                name: asset.name.clone(),
                duration_seconds: duration,
                channels,
                bpm: asset.tempo.map(|tempo| tempo.bpm.value() as f32),
                first_beat_seconds: asset.tempo.map(|tempo| tempo.first_beat.value() as f32),
                changed_by_agent: false,
                definition: definition.to_owned(),
                media_path,
                content_hash,
                sample_rate,
                frames,
                revision_count: asset.revisions.len(),
                current_revision: asset.current_revision_id.map(|value| value.to_string()),
                effects,
                structure_path: format!("project.assets[id={id}]"),
            }
        })
        .collect::<Vec<_>>();

    let compositions = project
        .compositions
        .iter()
        .map(|composition| {
            let composition_id = composition.id.to_string();
            let tracks = composition
                .track_ids
                .iter()
                .filter_map(|track_id| project.tracks.iter().find(|track| track.id == *track_id))
                .map(|track| {
                    let track_id = track.id.to_string();
                    let mut clips = track
                        .clips
                        .iter()
                        .map(|clip| adapt_clip(project, clip, clip_waveforms))
                        .collect::<Vec<_>>();
                    clips.sort_by(|left, right| left.start.total_cmp(&right.start));
                    let composition_clips = clips
                        .iter()
                        .any(|clip| matches!(clip.kind, ClipKind::Composition { .. }));
                    let sampler_zones = track
                        .instrument
                        .as_ref()
                        .map(|instrument| match &instrument.kind {
                            gaw_core::InstrumentKind::Sampler(sampler) => sampler
                                .zones
                                .iter()
                                .map(|zone| SamplerZone {
                                    id: zone.id.to_string(),
                                    name: zone.name.clone(),
                                    asset_id: zone.asset_id.to_string(),
                                    root_note: zone.root_note.value(),
                                    low_note: zone.note_range.low.value(),
                                    high_note: zone.note_range.high.value(),
                                    low_velocity: zone.velocity_range.low.value(),
                                    high_velocity: zone.velocity_range.high.value(),
                                    source_start_seconds: zone.source.start.value(),
                                    source_duration_seconds: zone.source.duration.value(),
                                    gain_db: zone.gain.value() as f32,
                                    velocity_sensitivity: zone.velocity_sensitivity.value() as f32,
                                    attack_ms: zone.attack.value() as f32,
                                    release_ms: zone.release.value() as f32,
                                    one_shot: zone.playback == gaw_core::SamplerPlayback::OneShot,
                                    reverse: zone.reverse,
                                    choke_group: zone.choke_group,
                                    structure_path: format!(
                                        "project.tracks[id={track_id}].instrument.zones[id={}]",
                                        zone.id
                                    ),
                                })
                                .collect(),
                        })
                        .unwrap_or_default();
                    let (sampler_polyphony, sampler_voice_stealing, sampler_output_gain_db) = track
                        .instrument
                        .as_ref()
                        .map_or((None, None, None), |instrument| match &instrument.kind {
                            gaw_core::InstrumentKind::Sampler(sampler) => (
                                Some(sampler.polyphony),
                                Some(format!("{:?}", sampler.voice_stealing).to_lowercase()),
                                Some(sampler.output_gain.value() as f32),
                            ),
                        });
                    Track {
                        id: track_id.clone(),
                        name: track.name.clone(),
                        kind: if track.kind == gaw_core::TrackKind::Event {
                            TrackKind::Event
                        } else if composition_clips {
                            TrackKind::Composition
                        } else {
                            TrackKind::Audio
                        },
                        muted: track.muted,
                        solo: track.solo,
                        level: 0.8,
                        max_visual_length: clips
                            .iter()
                            .map(|clip| {
                                clip.length
                                    + match clip.kind {
                                        ClipKind::Composition { tail_beats, .. } => tail_beats,
                                        _ => 0.0,
                                    }
                            })
                            .fold(0.0, f32::max),
                        clips,
                        effects: track.effects.iter().map(effect_view).collect(),
                        sampler_zones,
                        sampler_polyphony,
                        sampler_voice_stealing,
                        sampler_output_gain_db,
                        structure_path: format!("project.tracks[id={track_id}]"),
                    }
                })
                .collect();
            Composition {
                id: composition_id.clone(),
                name: composition.name.clone(),
                length_beats: composition.length.value() as f32,
                tracks,
                output_effects: composition.output_effects.iter().map(effect_view).collect(),
                structure_path: format!("project.compositions[id={composition_id}]"),
            }
        })
        .collect();
    (assets, compositions)
}

#[allow(clippy::too_many_lines)]
fn adapt_clip(
    project: &Project,
    clip: &gaw_core::Clip,
    waveforms: Option<&HashMap<String, Arc<[f32]>>>,
) -> Clip {
    let (id, name, start, length, gain_db, kind, effects) = match clip {
        gaw_core::Clip::Audio(clip) => {
            let asset_index = project
                .assets
                .iter()
                .position(|asset| asset.id == clip.asset_id)
                .unwrap_or(0);
            let asset = project.assets.get(asset_index);
            (
                clip.id,
                clip.name.clone(),
                clip.start.value(),
                clip.duration.value(),
                processor_gain(&clip.effects),
                ClipKind::Audio {
                    asset: asset_index,
                    sync: match clip.tempo_sync {
                        gaw_core::TempoSync::None => SyncMode::None,
                        gaw_core::TempoSync::Repitch => SyncMode::Repitch,
                        gaw_core::TempoSync::Stretch => SyncMode::Stretch,
                    },
                    source_bpm: asset
                        .and_then(|asset| asset.tempo)
                        .map(|tempo| tempo.bpm.value() as f32),
                },
                clip.effects.iter().map(effect_view).collect(),
            )
        }
        gaw_core::Clip::Event(clip) => {
            let notes = project
                .event_data
                .iter()
                .find(|events| events.id == clip.event_data_id)
                .map(|events| {
                    events
                        .events
                        .iter()
                        .enumerate()
                        .filter_map(|(event_index, event)| match event {
                            gaw_core::Event::Note(note)
                                if note.start.value() >= clip.source_start.value()
                                    && note.start.value()
                                        < clip.source_start.value() + clip.duration.value() =>
                            {
                                Some(Note {
                                    event_index,
                                    start: (note.start.value() - clip.source_start.value()) as f32,
                                    length: note.duration.value() as f32,
                                    pitch: note.note.value(),
                                    velocity: f32::from(note.velocity.value()) / 127.0,
                                })
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (
                clip.id,
                clip.name.clone(),
                clip.start.value(),
                clip.duration.value(),
                0.0,
                ClipKind::Event {
                    notes: Arc::from(notes),
                },
                Vec::new(),
            )
        }
        gaw_core::Clip::Composition(clip) => {
            let child = project
                .compositions
                .iter()
                .position(|composition| composition.id == clip.composition_id)
                .unwrap_or(0);
            (
                clip.id,
                clip.name.clone(),
                clip.start.value(),
                clip.duration.value(),
                processor_gain(&clip.effects),
                ClipKind::Composition {
                    child,
                    render: RenderState::Fresh,
                    tail_beats: 0.0,
                },
                clip.effects.iter().map(effect_view).collect(),
            )
        }
    };
    let id = id.to_string();
    Clip {
        waveform: waveforms
            .and_then(|cache| cache.get(&id).cloned())
            .unwrap_or_else(|| waveform(id_seed(&id), 320)),
        id,
        name,
        start: start as f32,
        length: length as f32,
        gain_db,
        kind,
        effects,
    }
}

fn processor_gain(effects: &[gaw_core::Processor]) -> f32 {
    effects
        .iter()
        .find_map(|processor| match &processor.kind {
            gaw_core::ProcessorKind::Gain(parameters) => Some(parameters.gain_db),
            _ => None,
        })
        .unwrap_or(0.0)
}

fn id_seed(id: &str) -> f32 {
    let value = id.bytes().fold(17_u32, |state, byte| {
        state.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    (value % 97) as f32 / 17.0 + 0.7
}

impl SyncMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "FREE",
            Self::Repitch => "REPITCH",
            Self::Stretch => "STRETCH",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderState {
    Fresh,
    Stale,
    Rendering(u8),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Note {
    pub event_index: usize,
    pub start: f32,
    pub length: f32,
    pub pitch: u8,
    pub velocity: f32,
}

#[derive(Clone, Debug)]
pub enum ClipKind {
    Audio {
        asset: usize,
        sync: SyncMode,
        source_bpm: Option<f32>,
    },
    Event {
        notes: Arc<[Note]>,
    },
    Composition {
        child: usize,
        render: RenderState,
        tail_beats: f32,
    },
}

#[derive(Clone, Debug)]
pub struct Effect {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub parameters: Vec<Parameter>,
}

#[derive(Clone, Debug)]
pub struct Parameter {
    pub id: String,
    pub label: String,
    pub value: serde_json::Value,
    pub value_type: gaw_core::ParameterValueType,
    pub range: Option<(f64, f64)>,
    pub choices: Vec<String>,
    pub unit: String,
    pub automatable: bool,
    pub display_hint: String,
}

#[derive(Clone, Debug)]
pub struct Clip {
    pub id: String,
    pub name: String,
    pub start: f32,
    pub length: f32,
    pub gain_db: f32,
    pub waveform: Arc<[f32]>,
    pub kind: ClipKind,
    pub effects: Vec<Effect>,
}

impl Clip {
    pub fn end(&self) -> f32 {
        self.start + self.length
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Event,
    Composition,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub kind: TrackKind,
    pub muted: bool,
    pub solo: bool,
    pub level: f32,
    pub max_visual_length: f32,
    pub clips: Vec<Clip>,
    pub effects: Vec<Effect>,
    pub sampler_zones: Vec<SamplerZone>,
    pub sampler_polyphony: Option<u16>,
    pub sampler_voice_stealing: Option<String>,
    pub sampler_output_gain_db: Option<f32>,
    pub structure_path: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SamplerZone {
    pub id: String,
    pub name: String,
    pub asset_id: String,
    pub root_note: u8,
    pub low_note: u8,
    pub high_note: u8,
    pub low_velocity: u8,
    pub high_velocity: u8,
    pub source_start_seconds: f64,
    pub source_duration_seconds: f64,
    pub gain_db: f32,
    pub velocity_sensitivity: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub one_shot: bool,
    pub reverse: bool,
    pub choke_group: Option<u16>,
    pub structure_path: String,
}

#[derive(Clone, Debug)]
pub struct Composition {
    pub id: String,
    pub name: String,
    pub length_beats: f32,
    pub tracks: Vec<Track>,
    pub output_effects: Vec<Effect>,
    pub structure_path: String,
}

#[derive(Clone, Debug)]
pub struct Asset {
    pub id: String,
    pub name: String,
    pub duration_seconds: f32,
    pub channels: u8,
    pub bpm: Option<f32>,
    pub first_beat_seconds: Option<f32>,
    pub waveform: Arc<[f32]>,
    pub changed_by_agent: bool,
    pub definition: String,
    pub media_path: Option<String>,
    pub content_hash: Option<String>,
    pub sample_rate: u32,
    pub frames: u64,
    pub revision_count: usize,
    pub current_revision: Option<String>,
    pub effects: Vec<Effect>,
    pub structure_path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    None,
    Asset(usize),
    Clip {
        track: usize,
        clip: usize,
    },
    Effect {
        track: usize,
        clip: usize,
        effect: usize,
    },
    Sampler {
        track: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorKind {
    Overview,
    Waveform,
    PianoRoll,
    Sampler,
    Effect,
}

#[derive(Clone, Debug)]
pub struct Transport {
    pub playing: bool,
    pub recording: bool,
    pub loop_enabled: bool,
    pub loop_start: f32,
    pub loop_end: f32,
    pub playhead: f32,
    pub bpm: f32,
}

#[derive(Clone, Debug)]
struct Highlight {
    entity_id: String,
    changed_at: f64,
}

#[derive(Clone, Copy, Debug)]
pub enum Intent {
    TogglePlayback,
    ToggleRecording,
    Stop,
    ToggleLoop,
    Seek(f32),
    SetLoopRange {
        start: f32,
        end: f32,
    },
    EditClip {
        track: usize,
        clip: usize,
        start: f32,
        length: f32,
        target_track: usize,
    },
    DeleteClip {
        track: usize,
        clip: usize,
    },
    AddNote {
        track: usize,
        clip: usize,
        start: f32,
        length: f32,
        pitch: u8,
        velocity: u8,
    },
    EditNote {
        track: usize,
        clip: usize,
        event_index: usize,
        start: f32,
        length: f32,
        pitch: u8,
        velocity: u8,
    },
    DeleteNote {
        track: usize,
        clip: usize,
        event_index: usize,
    },
    SetBpm(f32),
    Select(Selection),
    ClearSelection,
    EnterChild {
        track: usize,
        clip: usize,
    },
    NavigateToDepth(usize),
    Back,
    ToggleMute(usize),
    ToggleSolo(usize),
    ToggleEffect {
        track: usize,
        clip: usize,
        effect: usize,
    },
    MoveEffect {
        track: usize,
        clip: usize,
        effect: usize,
        delta: isize,
    },
    AddAssetClip {
        asset: usize,
        beat: f32,
        track: Option<usize>,
    },
    ToggleStructureLens,
    SimulateAgentChange(f64),
    Undo(f64),
    Redo(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeSource {
    Ui,
    Agent,
    Undo,
    Redo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioClipEdit {
    TrimStart,
    Chop,
    ToggleFadeIn,
    ToggleFadeOut,
    ToggleReverse,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum NoteEdit {
    Add {
        start: f32,
        length: f32,
        pitch: u8,
        velocity: u8,
    },
    Update {
        event_index: usize,
        start: f32,
        length: f32,
        pitch: u8,
        velocity: u8,
    },
    Delete {
        event_index: usize,
    },
}

#[derive(Clone, Debug)]
pub struct ProjectUpdate {
    pub revision: u64,
    pub source: ChangeSource,
    pub label: String,
    pub changed_ids: Arc<[String]>,
    /// The delta-sized canonical transaction for forward edits. Undo/redo updates carry `None`.
    pub transaction: Option<Arc<Transaction>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StableSelection {
    None,
    Asset(AssetId),
    Clip {
        track_id: TrackId,
        clip_id: ClipId,
    },
    Effect {
        stack: ProcessorStack,
        processor_id: ProcessorId,
    },
    Sampler {
        track_id: TrackId,
    },
}

#[derive(Clone, Debug, Default)]
struct CommandEngine {
    history: EditHistory,
    revision: u64,
}

#[derive(Clone, Debug)]
pub struct DemoViewModel {
    project: Project,
    engine: CommandEngine,
    pub compositions: Vec<Composition>,
    pub assets: Vec<Asset>,
    pub transport: Transport,
    pub selection: Selection,
    scoped_effect: Option<(ProcessorStack, ProcessorId)>,
    pub structure_lens: bool,
    nav_path: Vec<CompositionId>,
    highlights: Vec<Highlight>,
    updates: VecDeque<ProjectUpdate>,
    last_error: Option<String>,
}

pub type ProjectViewModel = DemoViewModel;

impl Default for DemoViewModel {
    fn default() -> Self {
        Self::demo()
    }
}

impl DemoViewModel {
    pub fn demo() -> Self {
        Self::from_project(demo_project()).expect("demo project is valid")
    }

    pub fn from_project(project: Project) -> Result<Self, gaw_core::DomainError> {
        use gaw_core::Validate as _;
        project.validate()?;
        let (assets, compositions) = adapt_project(&project, None, None);
        let root = project.root_composition_id;
        Ok(Self {
            transport: Transport {
                playing: false,
                recording: false,
                loop_enabled: true,
                loop_start: 0.0,
                loop_end: project
                    .compositions
                    .iter()
                    .find(|composition| composition.id == root)
                    .map_or(4.0, |composition| composition.length.value() as f32),
                playhead: 0.0,
                bpm: project.bpm.value() as f32,
            },
            project,
            engine: CommandEngine::default(),
            compositions,
            assets,
            selection: Selection::None,
            scoped_effect: None,
            structure_lens: false,
            nav_path: vec![root],
            highlights: Vec::new(),
            updates: VecDeque::new(),
            last_error: None,
        })
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    pub fn revision(&self) -> u64 {
        self.engine.revision
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn take_updates(&mut self) -> impl Iterator<Item = ProjectUpdate> + '_ {
        self.updates.drain(..)
    }

    pub fn apply_agent_transaction(
        &mut self,
        transaction: &Transaction,
        changed_ids: impl IntoIterator<Item = String>,
        now: f64,
    ) -> Result<(), gaw_core::DomainError> {
        let changed_ids = changed_ids.into_iter().collect::<Vec<_>>();
        self.commit(transaction, ChangeSource::Agent, &changed_ids, now)
    }

    /// Atomically installs a validated canonical snapshot loaded outside the UI.
    pub fn replace_project_from_agent(
        &mut self,
        project: Project,
        changed_ids: impl IntoIterator<Item = String>,
        now: f64,
    ) -> Result<(), gaw_core::DomainError> {
        let selection = self.stable_selection();
        let changed_ids = changed_ids.into_iter().collect::<Vec<_>>();
        let asset_waveforms = self
            .assets
            .iter()
            .map(|asset| (asset.id.clone(), Arc::clone(&asset.waveform)))
            .collect::<HashMap<_, _>>();
        let clip_waveforms = self
            .compositions
            .iter()
            .flat_map(|composition| &composition.tracks)
            .flat_map(|track| &track.clips)
            .map(|clip| (clip.id.clone(), Arc::clone(&clip.waveform)))
            .collect::<HashMap<_, _>>();

        let mut replacement = Self::from_project(project)?;
        (replacement.assets, replacement.compositions) = adapt_project(
            &replacement.project,
            Some(&asset_waveforms),
            Some(&clip_waveforms),
        );
        for asset in &mut replacement.assets {
            asset.changed_by_agent = self
                .assets
                .iter()
                .any(|old| old.id == asset.id && old.changed_by_agent);
        }
        replacement.engine.revision = self.engine.revision.saturating_add(1);
        replacement.structure_lens = self.structure_lens;
        replacement.nav_path.clone_from(&self.nav_path);
        let compositions = &replacement.project.compositions;
        replacement
            .nav_path
            .retain(|id| compositions.iter().any(|composition| composition.id == *id));
        if replacement.nav_path.is_empty() {
            replacement
                .nav_path
                .push(replacement.project.root_composition_id);
        }
        replacement.restore_selection(&selection);
        replacement.transport = self.transport.clone();
        replacement.transport.bpm = replacement.project.bpm.value() as f32;
        let length = replacement.current_composition().length_beats;
        replacement.transport.playhead = replacement.transport.playhead.clamp(0.0, length);
        replacement.transport.loop_start = replacement.transport.loop_start.clamp(0.0, length);
        replacement.transport.loop_end = replacement
            .transport
            .loop_end
            .clamp(replacement.transport.loop_start, length);
        replacement.highlights.clone_from(&self.highlights);
        replacement.updates.clone_from(&self.updates);
        replacement.publish_update(
            ChangeSource::Agent,
            "External project reload",
            &changed_ids,
            now,
            None,
        );
        *self = replacement;
        Ok(())
    }

    /// Updates controller-owned render freshness for one nested composition clip.
    pub fn set_composition_clip_render_state(
        &mut self,
        clip_id: &str,
        render: RenderState,
    ) -> bool {
        for clip in self
            .compositions
            .iter_mut()
            .flat_map(|composition| &mut composition.tracks)
            .flat_map(|track| &mut track.clips)
        {
            if clip.id == clip_id
                && let ClipKind::Composition {
                    render: current, ..
                } = &mut clip.kind
            {
                *current = render;
                return true;
            }
        }
        false
    }

    pub fn stable_selection(&self) -> StableSelection {
        if let Some((stack, processor_id)) = &self.scoped_effect {
            return StableSelection::Effect {
                stack: stack.clone(),
                processor_id: processor_id.clone(),
            };
        }
        match self.selection {
            Selection::None => StableSelection::None,
            Selection::Asset(index) => self
                .project
                .assets
                .get(index)
                .map_or(StableSelection::None, |asset| {
                    StableSelection::Asset(asset.id)
                }),
            Selection::Sampler { track } => self
                .current_track_id(track)
                .map_or(StableSelection::None, |track_id| StableSelection::Sampler {
                    track_id,
                }),
            Selection::Clip { track, clip } => self
                .clip_ids(track, clip)
                .map_or(StableSelection::None, |(track_id, clip_id)| {
                    StableSelection::Clip { track_id, clip_id }
                }),
            Selection::Effect {
                track,
                clip,
                effect,
            } => self.clip_effect_ids(track, clip, effect).map_or(
                StableSelection::None,
                |(stack, processor_id)| StableSelection::Effect {
                    stack,
                    processor_id,
                },
            ),
        }
    }

    pub fn current_composition(&self) -> &Composition {
        let id = self.nav_path.last().expect("root composition exists");
        let index = self
            .project
            .compositions
            .iter()
            .position(|composition| composition.id == *id)
            .expect("navigation contains canonical composition");
        &self.compositions[index]
    }

    pub fn breadcrumbs(&self) -> impl Iterator<Item = &Composition> {
        self.nav_path.iter().filter_map(|id| {
            self.project
                .compositions
                .iter()
                .position(|composition| composition.id == *id)
                .and_then(|index| self.compositions.get(index))
        })
    }

    pub fn can_navigate_back(&self) -> bool {
        self.nav_path.len() > 1
    }

    pub fn editor_kind(&self) -> EditorKind {
        if self.scoped_effect.is_some() {
            return EditorKind::Effect;
        }
        match self.selection {
            Selection::None => EditorKind::Overview,
            Selection::Asset(_) => EditorKind::Waveform,
            Selection::Sampler { .. } => EditorKind::Sampler,
            Selection::Effect { .. } => EditorKind::Effect,
            Selection::Clip { track, clip } => self
                .current_composition()
                .tracks
                .get(track)
                .and_then(|track| track.clips.get(clip))
                .map_or(EditorKind::Overview, |clip| match clip.kind {
                    ClipKind::Audio { .. } | ClipKind::Composition { .. } => EditorKind::Waveform,
                    ClipKind::Event { .. } => EditorKind::PianoRoll,
                }),
        }
    }

    pub fn selected_clip(&self) -> Option<(usize, usize, &Clip)> {
        let (Selection::Clip {
            track: track_index,
            clip: clip_index,
        }
        | Selection::Effect {
            track: track_index,
            clip: clip_index,
            ..
        }) = self.selection
        else {
            return None;
        };
        let clip = self
            .current_composition()
            .tracks
            .get(track_index)?
            .clips
            .get(clip_index)?;
        Some((track_index, clip_index, clip))
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn highlight_alpha(&self, entity_id: &str, now: f64) -> f32 {
        self.highlights
            .iter()
            .find(|highlight| highlight.entity_id == entity_id)
            .map_or(0.0, |highlight| {
                let elapsed = now - highlight.changed_at;
                if (0.0..HIGHLIGHT_SECONDS).contains(&elapsed) {
                    (1.0 - elapsed / HIGHLIGHT_SECONDS) as f32
                } else {
                    0.0
                }
            })
    }

    pub fn has_active_highlights(&self, now: f64) -> bool {
        self.highlights
            .iter()
            .any(|highlight| now - highlight.changed_at < HIGHLIGHT_SECONDS)
    }

    pub fn advance(&mut self, seconds: f32) {
        if !self.transport.playing {
            return;
        }
        let beats_per_second = self.transport.bpm / 60.0;
        let length = self.current_composition().length_beats;
        let loop_start = self.transport.loop_start.clamp(0.0, length);
        let loop_end = self.transport.loop_end.clamp(loop_start, length);
        let next = self.transport.playhead + seconds * beats_per_second;
        if self.transport.loop_enabled && loop_end > loop_start && next >= loop_end {
            self.transport.playhead =
                loop_start + (next - loop_start).rem_euclid(loop_end - loop_start);
        } else if next >= length {
            self.transport.playhead = if self.transport.loop_enabled && length > 0.0 {
                loop_start
            } else {
                length
            };
            if !self.transport.loop_enabled {
                self.transport.playing = false;
            }
        } else {
            self.transport.playhead = next;
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn apply(&mut self, intent: Intent) {
        match intent {
            Intent::TogglePlayback => self.transport.playing = !self.transport.playing,
            Intent::ToggleRecording => self.transport.recording = !self.transport.recording,
            Intent::Stop => {
                self.transport.playing = false;
                self.transport.recording = false;
                self.transport.playhead = 0.0;
            }
            Intent::ToggleLoop => self.transport.loop_enabled = !self.transport.loop_enabled,
            Intent::Seek(beat) => {
                self.transport.playhead = beat.clamp(0.0, self.current_composition().length_beats);
            }
            Intent::SetLoopRange { start, end } => {
                let length = self.current_composition().length_beats;
                let start = start.clamp(0.0, length);
                let end = end.clamp(0.0, length);
                self.transport.loop_start = start.min(end);
                self.transport.loop_end = start.max(end).max(self.transport.loop_start + 0.25);
                self.transport.loop_enabled = true;
            }
            Intent::EditClip {
                track,
                clip,
                start,
                length,
                target_track,
            } => self.edit_clip_timing(track, clip, start, length, target_track),
            Intent::DeleteClip { track, clip } => self.delete_clip(track, clip),
            Intent::AddNote {
                track,
                clip,
                start,
                length,
                pitch,
                velocity,
            } => self.edit_note(
                track,
                clip,
                NoteEdit::Add {
                    start,
                    length,
                    pitch,
                    velocity,
                },
            ),
            Intent::EditNote {
                track,
                clip,
                event_index,
                start,
                length,
                pitch,
                velocity,
            } => self.edit_note(
                track,
                clip,
                NoteEdit::Update {
                    event_index,
                    start,
                    length,
                    pitch,
                    velocity,
                },
            ),
            Intent::DeleteNote {
                track,
                clip,
                event_index,
            } => self.edit_note(track, clip, NoteEdit::Delete { event_index }),
            Intent::SetBpm(bpm) => {
                let bpm = bpm.clamp(MIN_BPM, MAX_BPM);
                if let Ok(value) = gaw_core::Bpm::new(f64::from(bpm)) {
                    let transaction = Transaction::named(
                        "Set project tempo",
                        [Command::SetProjectTempo { bpm: value }],
                    );
                    self.commit_ui(&transaction, &[self.project.id.to_string()]);
                }
            }
            Intent::Select(selection) => {
                self.selection = selection;
                self.scoped_effect = None;
            }
            Intent::ClearSelection => {
                self.selection = Selection::None;
                self.scoped_effect = None;
            }
            Intent::EnterChild { track, clip } => {
                let child = self
                    .current_composition()
                    .tracks
                    .get(track)
                    .and_then(|track| track.clips.get(clip))
                    .and_then(|clip| match clip.kind {
                        ClipKind::Composition { child, .. } => self
                            .compositions
                            .get(child)
                            .and_then(|composition| {
                                self.project
                                    .compositions
                                    .iter()
                                    .find(|core| core.id.to_string() == composition.id)
                            })
                            .map(|composition| composition.id),
                        _ => None,
                    });
                if let Some(child) = child {
                    self.nav_path.push(child);
                    self.selection = Selection::None;
                    self.transport.playhead = 0.0;
                }
            }
            Intent::NavigateToDepth(depth) => {
                if depth < self.nav_path.len() {
                    self.nav_path.truncate(depth + 1);
                    self.selection = Selection::None;
                    self.transport.playhead = 0.0;
                }
            }
            Intent::Back => {
                if self.nav_path.len() > 1 {
                    self.nav_path.pop();
                    self.selection = Selection::None;
                    self.transport.playhead = 0.0;
                }
            }
            Intent::ToggleMute(track) => {
                if let Some(track_id) = self.current_track_id(track)
                    && let Some(mut track) = self
                        .project
                        .tracks
                        .iter()
                        .find(|candidate| candidate.id == track_id)
                        .cloned()
                {
                    track.muted = !track.muted;
                    let transaction =
                        Transaction::named("Toggle track mute", [Command::UpdateTrack { track }]);
                    self.commit_ui(&transaction, &[track_id.to_string()]);
                }
            }
            Intent::ToggleSolo(track) => {
                if let Some(track_id) = self.current_track_id(track)
                    && let Some(mut track) = self
                        .project
                        .tracks
                        .iter()
                        .find(|candidate| candidate.id == track_id)
                        .cloned()
                {
                    track.solo = !track.solo;
                    let transaction =
                        Transaction::named("Toggle track solo", [Command::UpdateTrack { track }]);
                    self.commit_ui(&transaction, &[track_id.to_string()]);
                }
            }
            Intent::ToggleEffect {
                track,
                clip,
                effect,
            } => {
                if let Some((stack, processor_id)) = self.clip_effect_ids(track, clip, effect)
                    && let Some(mut processor) =
                        find_processor(&self.project, &stack, &processor_id)
                {
                    processor.enabled = !processor.enabled;
                    let transaction = Transaction::named(
                        "Toggle processor",
                        [Command::UpdateProcessor { stack, processor }],
                    );
                    self.commit_ui(&transaction, &[processor_id.to_string()]);
                }
            }
            Intent::MoveEffect {
                track,
                clip,
                effect,
                delta,
            } => {
                if let Some(target) = effect.checked_add_signed(delta)
                    && let Some((stack, processor_id)) = self.clip_effect_ids(track, clip, effect)
                {
                    let transaction = Transaction::named(
                        "Reorder processor",
                        [Command::ReorderProcessor {
                            stack,
                            from: effect,
                            to: target,
                        }],
                    );
                    self.commit_ui(&transaction, &[processor_id.to_string()]);
                    if self.last_error.is_none() {
                        self.selection = Selection::Effect {
                            track,
                            clip,
                            effect: target,
                        };
                    }
                }
            }
            Intent::AddAssetClip { asset, beat, track } => {
                self.add_asset_clip(asset, beat, track);
            }
            Intent::ToggleStructureLens => self.structure_lens = !self.structure_lens,
            Intent::SimulateAgentChange(now) => {
                if let Some(asset) = self.project.assets.first() {
                    let id = asset.id;
                    let next = asset.tempo.map_or(120.0, |tempo| tempo.bpm.value() + 1.0);
                    if let Ok(bpm) = gaw_core::Bpm::new(next) {
                        let transaction = Transaction::named(
                            "Agent asset analysis",
                            [Command::SetAssetBpm {
                                asset_id: id,
                                bpm: Some(bpm),
                            }],
                        );
                        let _ = self.apply_agent_transaction(&transaction, [id.to_string()], now);
                    }
                }
            }
            Intent::Undo(now) => self.undo(now),
            Intent::Redo(now) => self.redo(now),
        }
    }

    pub(crate) fn current_composition_id(&self) -> CompositionId {
        *self.nav_path.last().expect("root composition exists")
    }

    pub(crate) fn current_track_id(&self, index: usize) -> Option<TrackId> {
        self.project
            .compositions
            .iter()
            .find(|composition| composition.id == self.current_composition_id())?
            .track_ids
            .get(index)
            .copied()
    }

    pub(crate) fn asset_id(&self, index: usize) -> Option<AssetId> {
        self.project.assets.get(index).map(|asset| asset.id)
    }

    pub fn set_asset_tempo(&mut self, index: usize, bpm: Option<f32>, first_beat_seconds: f32) {
        let Some(asset_id) = self.asset_id(index) else {
            return;
        };
        let tempo = bpm.and_then(|bpm| {
            Some(gaw_core::AssetTempo {
                bpm: gaw_core::Bpm::new(f64::from(bpm)).ok()?,
                first_beat: gaw_core::Seconds::new(f64::from(first_beat_seconds.max(0.0))).ok()?,
            })
        });
        if bpm.is_some() && tempo.is_none() {
            return;
        }
        let transaction = Transaction::named(
            "Set asset tempo",
            [Command::SetAssetTempo { asset_id, tempo }],
        );
        self.commit_ui(&transaction, &[asset_id.to_string()]);
    }

    pub fn accept_asset_tempo_suggestion(
        &mut self,
        index: usize,
        suggested_bpm: f32,
        first_beat_seconds: f32,
    ) {
        self.set_asset_tempo(index, Some(suggested_bpm), first_beat_seconds);
    }

    #[allow(clippy::too_many_lines)]
    pub fn edit_selected_audio_clip(&mut self, edit: AudioClipEdit) {
        let Some((track_index, clip_index, _)) = self.selected_clip() else {
            return;
        };
        let Some((track_id, clip_id)) = self.clip_ids(track_index, clip_index) else {
            return;
        };
        let Some(gaw_core::Clip::Audio(mut clip)) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.clips.iter().find(|clip| clip.id() == clip_id))
            .cloned()
        else {
            return;
        };
        let mut commands = Vec::new();
        match edit {
            AudioClipEdit::TrimStart => {
                let amount = 0.05_f64.min(clip.source.duration.value() / 2.0);
                clip.source.start = gaw_core::Seconds::new(clip.source.start.value() + amount)
                    .expect("finite trim");
                clip.source.duration =
                    gaw_core::Seconds::new(clip.source.duration.value() - amount)
                        .expect("positive trim");
                commands.push(Command::UpdateClip {
                    track_id,
                    clip: gaw_core::Clip::Audio(clip),
                });
            }
            AudioClipEdit::Chop => {
                let half = clip.duration.value() / 2.0;
                if half <= 0.0 {
                    return;
                }
                let mut right = clip.clone();
                let source_start = clip.source.start.value();
                let source_half = clip.source.duration.value() / 2.0;
                right.id = gaw_core::ClipId::new();
                right.start = gaw_core::Beats::new(clip.start.value() + half).expect("valid");
                right.duration = gaw_core::Beats::new(half).expect("valid");
                right.source.start = gaw_core::Seconds::new(if clip.reverse {
                    source_start
                } else {
                    source_start + source_half
                })
                .expect("valid");
                right.source.duration = gaw_core::Seconds::new(source_half).expect("valid");
                right.fade_in = None;
                clip.duration = gaw_core::Beats::new(half).expect("valid");
                clip.source.start = gaw_core::Seconds::new(if clip.reverse {
                    source_start + source_half
                } else {
                    source_start
                })
                .expect("valid");
                clip.source.duration = gaw_core::Seconds::new(source_half).expect("valid");
                clip.fade_out = None;
                commands.push(Command::UpdateClip {
                    track_id,
                    clip: gaw_core::Clip::Audio(clip),
                });
                commands.push(Command::AddClip {
                    track_id,
                    clip: gaw_core::Clip::Audio(right),
                });
            }
            AudioClipEdit::ToggleFadeIn => {
                clip.fade_in = clip.fade_in.map_or_else(
                    || {
                        Some(gaw_core::Fade {
                            duration: gaw_core::Seconds::new(
                                0.02_f64.min(clip.source.duration.value() / 4.0),
                            )
                            .expect("valid"),
                            curve: gaw_core::FadeCurve::EqualPower,
                        })
                    },
                    |_| None,
                );
                commands.push(Command::UpdateClip {
                    track_id,
                    clip: gaw_core::Clip::Audio(clip),
                });
            }
            AudioClipEdit::ToggleFadeOut => {
                clip.fade_out = clip.fade_out.map_or_else(
                    || {
                        Some(gaw_core::Fade {
                            duration: gaw_core::Seconds::new(
                                0.02_f64.min(clip.source.duration.value() / 4.0),
                            )
                            .expect("valid"),
                            curve: gaw_core::FadeCurve::EqualPower,
                        })
                    },
                    |_| None,
                );
                commands.push(Command::UpdateClip {
                    track_id,
                    clip: gaw_core::Clip::Audio(clip),
                });
            }
            AudioClipEdit::ToggleReverse => {
                clip.reverse = !clip.reverse;
                commands.push(Command::UpdateClip {
                    track_id,
                    clip: gaw_core::Clip::Audio(clip),
                });
            }
        }
        let transaction = Transaction::named("Edit audio clip", commands);
        self.commit_ui(&transaction, &[track_id.to_string(), clip_id.to_string()]);
    }

    pub(crate) fn selected_audio_details(&self) -> Option<(f64, f64, bool, bool, bool)> {
        let (track, clip, _) = self.selected_clip()?;
        let (track_id, clip_id) = self.clip_ids(track, clip)?;
        let gaw_core::Clip::Audio(clip) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)?
            .clips
            .iter()
            .find(|clip| clip.id() == clip_id)?
        else {
            return None;
        };
        Some((
            clip.source.start.value(),
            clip.source.duration.value(),
            clip.reverse,
            clip.fade_in.is_some(),
            clip.fade_out.is_some(),
        ))
    }

    #[allow(clippy::too_many_lines)]
    fn edit_clip_timing(
        &mut self,
        track_index: usize,
        clip_index: usize,
        start: f32,
        length: f32,
        target_track_index: usize,
    ) {
        let Some((from_track_id, clip_id)) = self.clip_ids(track_index, clip_index) else {
            return;
        };
        let Some(to_track_id) = self.current_track_id(target_track_index) else {
            return;
        };
        let Some(to_track) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == to_track_id)
        else {
            return;
        };
        let Some(mut clip) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == from_track_id)
            .and_then(|track| track.clips.iter().find(|clip| clip.id() == clip_id))
            .cloned()
        else {
            return;
        };
        let compatible = matches!(
            (&clip, to_track.kind),
            (gaw_core::Clip::Event(_), gaw_core::TrackKind::Event)
                | (
                    gaw_core::Clip::Audio(_) | gaw_core::Clip::Composition(_),
                    gaw_core::TrackKind::Audio
                )
        );
        if !compatible {
            return;
        }
        let original_start = clip.start().value();
        let original_duration = match &clip {
            gaw_core::Clip::Audio(clip) => clip.duration.value(),
            gaw_core::Clip::Event(clip) => clip.duration.value(),
            gaw_core::Clip::Composition(clip) => clip.duration.value(),
        };
        let composition_length = self.current_composition().length_beats;
        let start = start.clamp(0.0, (composition_length - 0.25).max(0.0));
        let length = length.clamp(0.25, (composition_length - start).max(0.25));
        let left_resize =
            ((f64::from(start + length) - (original_start + original_duration)).abs() < 0.001)
                && (f64::from(length) - original_duration).abs() > 0.001;
        let start_delta = f64::from(start) - original_start;
        let audio_seconds_per_beat = match &clip {
            gaw_core::Clip::Audio(audio) if audio.tempo_sync != gaw_core::TempoSync::None => self
                .project
                .assets
                .iter()
                .find(|asset| asset.id == audio.asset_id)
                .and_then(|asset| asset.tempo)
                .map_or(60.0 / self.project.bpm.value(), |tempo| {
                    60.0 / tempo.bpm.value()
                }),
            gaw_core::Clip::Audio(_) => 60.0 / self.project.bpm.value(),
            gaw_core::Clip::Event(_) | gaw_core::Clip::Composition(_) => 0.0,
        };
        let start = gaw_core::Beats::new(f64::from(start)).expect("clamped start is valid");
        let duration = gaw_core::Beats::new(f64::from(length)).expect("clamped duration is valid");
        match &mut clip {
            gaw_core::Clip::Audio(clip) => {
                if left_resize {
                    let old_source_start = clip.source.start.value();
                    let new_source_start =
                        (old_source_start + start_delta * audio_seconds_per_beat).max(0.0);
                    let applied = new_source_start - old_source_start;
                    clip.source.start =
                        gaw_core::Seconds::new(new_source_start).expect("finite source start");
                    clip.source.duration =
                        gaw_core::Seconds::new((clip.source.duration.value() - applied).max(0.001))
                            .expect("positive source duration");
                }
                clip.start = start;
                clip.duration = duration;
            }
            gaw_core::Clip::Event(clip) => {
                if left_resize {
                    clip.source_start =
                        gaw_core::Beats::new((clip.source_start.value() + start_delta).max(0.0))
                            .expect("finite event source start");
                }
                clip.start = start;
                clip.duration = duration;
            }
            gaw_core::Clip::Composition(clip) => {
                if left_resize {
                    clip.source_start =
                        gaw_core::Beats::new((clip.source_start.value() + start_delta).max(0.0))
                            .expect("finite composition source start");
                }
                clip.start = start;
                clip.duration = duration;
            }
        }
        let mut commands = Vec::with_capacity(2);
        if from_track_id != to_track_id {
            commands.push(Command::MoveClip {
                clip_id,
                from_track_id,
                to_track_id,
            });
        }
        commands.push(Command::UpdateClip {
            track_id: to_track_id,
            clip,
        });
        self.commit_ui(
            &Transaction::named("Move or resize clip", commands),
            &[
                clip_id.to_string(),
                from_track_id.to_string(),
                to_track_id.to_string(),
            ],
        );
        if self.last_error.is_none()
            && let Some(track) = self
                .current_composition()
                .tracks
                .iter()
                .position(|track| track.id == to_track_id.to_string())
            && let Some(clip) = self.current_composition().tracks[track]
                .clips
                .iter()
                .position(|clip| clip.id == clip_id.to_string())
        {
            self.selection = Selection::Clip { track, clip };
        }
    }

    fn delete_clip(&mut self, track_index: usize, clip_index: usize) {
        let Some((track_id, clip_id)) = self.clip_ids(track_index, clip_index) else {
            return;
        };
        self.commit_ui(
            &Transaction::named("Delete clip", [Command::RemoveClip { track_id, clip_id }]),
            &[track_id.to_string(), clip_id.to_string()],
        );
    }

    fn edit_note(&mut self, track_index: usize, clip_index: usize, edit: NoteEdit) {
        let Some((track_id, clip_id)) = self.clip_ids(track_index, clip_index) else {
            return;
        };
        let Some(event_clip) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.clips.iter().find(|clip| clip.id() == clip_id))
            .and_then(|clip| match clip {
                gaw_core::Clip::Event(clip) => Some(clip),
                _ => None,
            })
        else {
            return;
        };
        let event_data_id = event_clip.event_data_id;
        let source_start = event_clip.source_start.value();
        let clip_length = event_clip.duration.value();
        let Some(mut events) = self
            .project
            .event_data
            .iter()
            .find(|events| events.id == event_data_id)
            .cloned()
        else {
            return;
        };
        let make_note = |start: f32, length: f32, pitch: u8, velocity: u8| {
            let start = f64::from(start).clamp(0.0, (clip_length - 0.0625).max(0.0));
            let length = f64::from(length).clamp(0.0625, (clip_length - start).max(0.0625));
            gaw_core::NoteEvent::new(
                gaw_core::Beats::new(source_start + start).ok()?,
                gaw_core::Beats::new(length).ok()?,
                pitch.min(127),
                velocity.min(127),
            )
            .ok()
        };
        match edit {
            NoteEdit::Add {
                start,
                length,
                pitch,
                velocity,
            } => {
                let Some(note) = make_note(start, length, pitch, velocity) else {
                    return;
                };
                events.events.push(gaw_core::Event::Note(note));
            }
            NoteEdit::Update {
                event_index,
                start,
                length,
                pitch,
                velocity,
            } => {
                let release_velocity = match events.events.get(event_index) {
                    Some(gaw_core::Event::Note(note)) => note.release_velocity,
                    _ => return,
                };
                let Some(mut note) = make_note(start, length, pitch, velocity) else {
                    return;
                };
                note.release_velocity = release_velocity;
                events.events[event_index] = gaw_core::Event::Note(note);
            }
            NoteEdit::Delete { event_index } => {
                if !matches!(
                    events.events.get(event_index),
                    Some(gaw_core::Event::Note(_))
                ) {
                    return;
                }
                events.events.remove(event_index);
            }
        }
        events.sort();
        self.commit_ui(
            &Transaction::named(
                "Edit piano-roll note",
                [Command::UpdateEventData { event_data: events }],
            ),
            &[event_data_id.to_string(), clip_id.to_string()],
        );
    }

    pub fn add_note_to_selected_event_clip(&mut self) {
        let Some((track_index, clip_index, _)) = self.selected_clip() else {
            return;
        };
        self.edit_note(
            track_index,
            clip_index,
            NoteEdit::Add {
                start: 0.0,
                length: 0.25,
                pitch: 60,
                velocity: 100,
            },
        );
    }

    pub fn toggle_first_sampler_zone_reverse(&mut self, track_index: usize) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(mut instrument) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.clone())
        else {
            return;
        };
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        let Some(zone) = sampler.zones.first_mut() else {
            return;
        };
        let zone_id = zone.id;
        zone.reverse = !zone.reverse;
        let transaction = Transaction::named(
            "Edit sampler zone",
            [Command::SetTrackInstrument {
                track_id,
                instrument: Some(instrument),
            }],
        );
        self.commit_ui(&transaction, &[track_id.to_string(), zone_id.to_string()]);
    }

    pub fn update_sampler_zone(
        &mut self,
        track_index: usize,
        zone_index: usize,
        edited: &SamplerZone,
    ) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(mut instrument) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.clone())
        else {
            return;
        };
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        let Some(zone) = sampler.zones.get_mut(zone_index) else {
            return;
        };
        let Some(asset_id) = self
            .project
            .assets
            .iter()
            .find(|asset| asset.id.to_string() == edited.asset_id)
            .map(|asset| asset.id)
        else {
            return;
        };
        let Ok(source_start) = gaw_core::Seconds::new(edited.source_start_seconds.max(0.0)) else {
            return;
        };
        let Ok(source_duration) = gaw_core::Seconds::new(edited.source_duration_seconds.max(0.001))
        else {
            return;
        };
        let (
            Ok(root_note),
            Ok(note_range),
            Ok(velocity_range),
            Ok(gain),
            Ok(velocity_sensitivity),
            Ok(attack),
            Ok(release),
        ) = (
            gaw_core::MidiNote::new(edited.root_note),
            gaw_core::NoteRange::new(edited.low_note, edited.high_note),
            gaw_core::VelocityRange::new(edited.low_velocity, edited.high_velocity),
            gaw_core::Decibels::new(f64::from(edited.gain_db)),
            gaw_core::Ratio::new(f64::from(edited.velocity_sensitivity)),
            gaw_core::Milliseconds::new(f64::from(edited.attack_ms)),
            gaw_core::Milliseconds::new(f64::from(edited.release_ms)),
        )
        else {
            return;
        };
        zone.name.clone_from(&edited.name);
        zone.asset_id = asset_id;
        zone.source = gaw_core::SourceRange {
            start: source_start,
            duration: source_duration,
        };
        zone.root_note = root_note;
        zone.note_range = note_range;
        zone.velocity_range = velocity_range;
        zone.playback = if edited.one_shot {
            gaw_core::SamplerPlayback::OneShot
        } else {
            gaw_core::SamplerPlayback::NoteGated
        };
        zone.gain = gain;
        zone.velocity_sensitivity = velocity_sensitivity;
        zone.attack = attack;
        zone.release = release;
        zone.reverse = edited.reverse;
        zone.choke_group = edited.choke_group;
        let zone_id = zone.id;
        self.commit_ui(
            &Transaction::named(
                "Edit sampler zone",
                [Command::SetTrackInstrument {
                    track_id,
                    instrument: Some(instrument),
                }],
            ),
            &[track_id.to_string(), zone_id.to_string()],
        );
    }

    pub fn add_sampler_zone(&mut self, track_index: usize) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(mut instrument) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.clone())
        else {
            return;
        };
        let Some(asset) = self.project.assets.first() else {
            return;
        };
        let duration = asset_duration(asset).unwrap_or(1.0).max(0.001);
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        let zone = gaw_core::SamplerZone {
            id: gaw_core::SamplerZoneId::new(),
            name: format!("Zone {}", sampler.zones.len() + 1),
            asset_id: asset.id,
            source: gaw_core::SourceRange {
                start: gaw_core::Seconds::new(0.0).expect("zero is valid"),
                duration: gaw_core::Seconds::new(duration).expect("asset duration is valid"),
            },
            root_note: gaw_core::MidiNote::new(60).expect("valid note"),
            note_range: gaw_core::NoteRange::new(60, 60).expect("valid range"),
            velocity_range: gaw_core::VelocityRange::new(0, 127).expect("valid range"),
            playback: gaw_core::SamplerPlayback::OneShot,
            gain: gaw_core::Decibels::new(0.0).expect("valid gain"),
            velocity_sensitivity: gaw_core::Ratio::new(1.0).expect("valid ratio"),
            attack: gaw_core::Milliseconds::new(0.0).expect("valid attack"),
            release: gaw_core::Milliseconds::new(50.0).expect("valid release"),
            reverse: false,
            choke_group: None,
        };
        let zone_id = zone.id;
        sampler.zones.push(zone);
        self.commit_ui(
            &Transaction::named(
                "Add sampler zone",
                [Command::SetTrackInstrument {
                    track_id,
                    instrument: Some(instrument),
                }],
            ),
            &[track_id.to_string(), zone_id.to_string()],
        );
    }

    pub fn remove_sampler_zone(&mut self, track_index: usize, zone_index: usize) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(mut instrument) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.clone())
        else {
            return;
        };
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        if zone_index >= sampler.zones.len() {
            return;
        }
        let zone_id = sampler.zones.remove(zone_index).id;
        self.commit_ui(
            &Transaction::named(
                "Remove sampler zone",
                [Command::SetTrackInstrument {
                    track_id,
                    instrument: Some(instrument),
                }],
            ),
            &[track_id.to_string(), zone_id.to_string()],
        );
    }

    pub fn update_sampler_settings(
        &mut self,
        track_index: usize,
        polyphony: u16,
        voice_stealing: &str,
        output_gain_db: f32,
    ) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(mut instrument) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.clone())
        else {
            return;
        };
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        sampler.polyphony = polyphony.max(1);
        sampler.voice_stealing = match voice_stealing {
            "quietest" => gaw_core::VoiceStealing::Quietest,
            "lowestvelocity" | "lowest_velocity" => gaw_core::VoiceStealing::LowestVelocity,
            _ => gaw_core::VoiceStealing::Oldest,
        };
        let Ok(gain) = gaw_core::Decibels::new(f64::from(output_gain_db)) else {
            return;
        };
        sampler.output_gain = gain;
        let instrument_id = instrument.id;
        self.commit_ui(
            &Transaction::named(
                "Edit sampler settings",
                [Command::SetTrackInstrument {
                    track_id,
                    instrument: Some(instrument),
                }],
            ),
            &[track_id.to_string(), instrument_id.to_string()],
        );
    }

    fn clip_ids(&self, track: usize, clip: usize) -> Option<(TrackId, ClipId)> {
        let track_id = self.current_track_id(track)?;
        let view_clip_id = &self
            .current_composition()
            .tracks
            .get(track)?
            .clips
            .get(clip)?
            .id;
        let clip_id = self
            .project
            .tracks
            .iter()
            .find(|candidate| candidate.id == track_id)?
            .clips
            .iter()
            .find(|candidate| candidate.id().to_string() == *view_clip_id)?
            .id();
        Some((track_id, clip_id))
    }

    fn clip_effect_ids(
        &self,
        track: usize,
        clip: usize,
        effect: usize,
    ) -> Option<(ProcessorStack, ProcessorId)> {
        let stack = self.clip_stack(track, clip)?;
        let processor = processor_stack(&self.project, &stack)?.get(effect)?;
        Some((stack, processor.id.clone()))
    }

    pub(crate) fn clip_stack(&self, track: usize, clip: usize) -> Option<ProcessorStack> {
        let (track_id, clip_id) = self.clip_ids(track, clip)?;
        let clip = self
            .project
            .tracks
            .iter()
            .find(|candidate| candidate.id == track_id)?
            .clips
            .iter()
            .find(|candidate| candidate.id() == clip_id)?;
        match clip {
            gaw_core::Clip::Audio(_) => Some(ProcessorStack::Clip { track_id, clip_id }),
            gaw_core::Clip::Composition(_) => {
                Some(ProcessorStack::CompositionClip { track_id, clip_id })
            }
            gaw_core::Clip::Event(_) => None,
        }
    }

    pub(crate) fn select_processor_at(&mut self, stack: ProcessorStack, index: usize) {
        if let Some(processor) =
            processor_stack(&self.project, &stack).and_then(|stack| stack.get(index))
        {
            self.scoped_effect = Some((stack, processor.id.clone()));
            self.selection = Selection::None;
        }
    }

    pub(crate) fn toggle_processor_at(&mut self, stack: ProcessorStack, index: usize) {
        let Some(mut processor) = processor_stack(&self.project, &stack)
            .and_then(|processors| processors.get(index))
            .cloned()
        else {
            return;
        };
        let processor_id = processor.id.clone();
        processor.enabled = !processor.enabled;
        let transaction = Transaction::named(
            "Toggle processor",
            [Command::UpdateProcessor { stack, processor }],
        );
        self.commit_ui(&transaction, &[processor_id.to_string()]);
    }

    pub(crate) fn move_processor_at(&mut self, stack: ProcessorStack, index: usize, delta: isize) {
        let Some(to) = index.checked_add_signed(delta) else {
            return;
        };
        let Some(processors) = processor_stack(&self.project, &stack) else {
            return;
        };
        if index >= processors.len() || to >= processors.len() {
            return;
        }
        let id = processors[index].id.to_string();
        let transaction = Transaction::named(
            "Reorder processor",
            [Command::ReorderProcessor {
                stack,
                from: index,
                to,
            }],
        );
        self.commit_ui(&transaction, &[id]);
    }

    pub(crate) fn remove_processor_at(&mut self, stack: ProcessorStack, index: usize) {
        let Some(processor_id) = processor_stack(&self.project, &stack)
            .and_then(|processors| processors.get(index))
            .map(|processor| processor.id.clone())
        else {
            return;
        };
        let transaction = Transaction::named(
            "Remove processor",
            [Command::RemoveProcessor {
                stack,
                processor_id: processor_id.clone(),
            }],
        );
        self.commit_ui(&transaction, &[processor_id.to_string()]);
    }

    pub(crate) fn processor_catalog() -> Vec<(String, String)> {
        gaw_core::ProcessorKind::catalog_defaults()
            .into_iter()
            .map(|kind| {
                (
                    kind.type_id().to_owned(),
                    kind.type_id().trim_start_matches("gaw.").replace('_', " "),
                )
            })
            .collect()
    }

    pub(crate) fn insert_processor(&mut self, stack: ProcessorStack, catalog_index: usize) {
        let index = processor_stack(&self.project, &stack).map_or(0, <[gaw_core::Processor]>::len);
        let Some(kind) = gaw_core::ProcessorKind::catalog_defaults()
            .into_iter()
            .nth(catalog_index)
        else {
            return;
        };
        let kind_id = kind.type_id().trim_start_matches("gaw.").replace('_', "-");
        let id = ProcessorId::new(format!(
            "ui-fx-{}-{}-{kind_id}",
            self.engine.revision + 1,
            index
        ))
        .expect("generated processor id is valid");
        let processor = gaw_core::Processor::new(id.clone(), kind);
        let transaction = Transaction::named(
            "Insert processor",
            [Command::InsertProcessor {
                stack,
                index,
                processor,
            }],
        );
        self.commit_ui(&transaction, &[id.to_string()]);
    }

    pub(crate) fn selected_processor_view(&self) -> Option<Effect> {
        if let Some((stack, processor_id)) = &self.scoped_effect {
            return find_processor(&self.project, stack, processor_id)
                .map(|value| effect_view(&value));
        }
        let Selection::Effect {
            track,
            clip,
            effect,
        } = self.selection
        else {
            return None;
        };
        self.current_composition()
            .tracks
            .get(track)?
            .clips
            .get(clip)?
            .effects
            .get(effect)
            .cloned()
    }

    pub(crate) fn set_selected_processor_parameter(
        &mut self,
        parameter: usize,
        value: serde_json::Value,
    ) {
        let selected = self.stable_selection();
        let StableSelection::Effect {
            stack,
            processor_id,
        } = selected
        else {
            return;
        };
        if let Some(view) = self.selected_processor_view()
            && let Some(parameter) = view.parameters.get(parameter)
            && let Some(mut processor) = find_processor(&self.project, &stack, &processor_id)
            && set_parameter(&mut processor, &parameter.id, value)
        {
            let transaction = Transaction::named(
                "Set processor parameter",
                [Command::UpdateProcessor { stack, processor }],
            );
            self.commit_ui(&transaction, &[processor_id.to_string()]);
        }
    }

    pub(crate) fn selected_parameter_automation_lanes(&self, parameter_id: &str) -> usize {
        let StableSelection::Effect { processor_id, .. } = self.stable_selection() else {
            return 0;
        };
        self.project
            .automation
            .iter()
            .filter(|lane| match &lane.target {
                gaw_core::AutomationTarget::AudioClipProcessor {
                    processor_id: id,
                    parameter_id: parameter,
                    ..
                }
                | gaw_core::AutomationTarget::CompositionClipProcessor {
                    processor_id: id,
                    parameter_id: parameter,
                    ..
                }
                | gaw_core::AutomationTarget::TrackProcessor {
                    processor_id: id,
                    parameter_id: parameter,
                    ..
                }
                | gaw_core::AutomationTarget::CompositionOutputProcessor {
                    processor_id: id,
                    parameter_id: parameter,
                } => {
                    id == &processor_id
                        && (parameter == parameter_id
                            || parameter.strip_prefix(parameter_id).is_some_and(|suffix| {
                                suffix.starts_with('[') || suffix.starts_with("[].")
                            }))
                }
                gaw_core::AutomationTarget::Instrument { .. } => false,
            })
            .count()
    }

    fn commit_ui(&mut self, transaction: &Transaction, changed_ids: &[String]) {
        if let Err(error) = self.commit(transaction, ChangeSource::Ui, changed_ids, 0.0) {
            self.last_error = Some(error.to_string());
        }
    }

    fn commit(
        &mut self,
        transaction: &Transaction,
        source: ChangeSource,
        changed_ids: &[String],
        now: f64,
    ) -> Result<(), gaw_core::DomainError> {
        let selection = self.stable_selection();
        self.engine.history.apply(&mut self.project, transaction)?;
        self.engine.revision += 1;
        self.last_error = None;
        self.refresh_projection(&selection);
        self.publish_update(
            source,
            transaction.label.as_deref().unwrap_or("Edit"),
            changed_ids,
            now,
            Some(transaction),
        );
        Ok(())
    }

    fn undo(&mut self, now: f64) {
        let selection = self.stable_selection();
        match self.engine.history.undo(&mut self.project) {
            Ok(()) => {
                self.engine.revision += 1;
                self.last_error = None;
                self.refresh_projection(&selection);
                self.publish_update(ChangeSource::Undo, "Undo", &[], now, None);
            }
            Err(error) => self.last_error = Some(error.to_string()),
        }
    }

    fn redo(&mut self, now: f64) {
        let selection = self.stable_selection();
        match self.engine.history.redo(&mut self.project) {
            Ok(()) => {
                self.engine.revision += 1;
                self.last_error = None;
                self.refresh_projection(&selection);
                self.publish_update(ChangeSource::Redo, "Redo", &[], now, None);
            }
            Err(error) => self.last_error = Some(error.to_string()),
        }
    }

    fn publish_update(
        &mut self,
        source: ChangeSource,
        label: &str,
        changed_ids: &[String],
        now: f64,
        transaction: Option<&Transaction>,
    ) {
        if source == ChangeSource::Agent {
            for entity_id in changed_ids {
                if let Some(highlight) = self
                    .highlights
                    .iter_mut()
                    .find(|highlight| highlight.entity_id == *entity_id)
                {
                    highlight.changed_at = now;
                } else {
                    self.highlights.push(Highlight {
                        entity_id: entity_id.clone(),
                        changed_at: now,
                    });
                }
            }
            for asset in &mut self.assets {
                if changed_ids.contains(&asset.id) {
                    asset.changed_by_agent = true;
                }
            }
        }
        self.updates.push_back(ProjectUpdate {
            revision: self.engine.revision,
            source,
            label: label.to_owned(),
            changed_ids: Arc::from(changed_ids),
            transaction: transaction.cloned().map(Arc::new),
        });
        if self.updates.len() > 256 {
            self.updates.pop_front();
        }
    }

    fn refresh_projection(&mut self, selection: &StableSelection) {
        let asset_waveforms = self
            .assets
            .iter()
            .map(|asset| (asset.id.clone(), Arc::clone(&asset.waveform)))
            .collect::<HashMap<_, _>>();
        let clip_waveforms = self
            .compositions
            .iter()
            .flat_map(|composition| &composition.tracks)
            .flat_map(|track| &track.clips)
            .map(|clip| (clip.id.clone(), Arc::clone(&clip.waveform)))
            .collect::<HashMap<_, _>>();
        let (assets, compositions) =
            adapt_project(&self.project, Some(&asset_waveforms), Some(&clip_waveforms));
        self.assets = assets;
        self.compositions = compositions;
        self.transport.bpm = self.project.bpm.value() as f32;
        self.nav_path.retain(|id| {
            self.project
                .compositions
                .iter()
                .any(|composition| composition.id == *id)
        });
        if self.nav_path.is_empty() {
            self.nav_path.push(self.project.root_composition_id);
        }
        self.restore_selection(selection);
    }

    fn restore_selection(&mut self, selection: &StableSelection) {
        self.scoped_effect = None;
        self.selection = match selection {
            StableSelection::None => Selection::None,
            StableSelection::Asset(asset_id) => self
                .project
                .assets
                .iter()
                .position(|asset| asset.id == *asset_id)
                .map_or(Selection::None, Selection::Asset),
            StableSelection::Sampler { track_id } => self
                .current_composition()
                .tracks
                .iter()
                .position(|track| track.id == track_id.to_string())
                .map_or(Selection::None, |track| Selection::Sampler { track }),
            StableSelection::Clip { track_id, clip_id } => {
                self.selection_for_clip(*track_id, *clip_id, None)
            }
            StableSelection::Effect {
                stack:
                    ProcessorStack::Clip { track_id, clip_id }
                    | ProcessorStack::CompositionClip { track_id, clip_id },
                processor_id,
            } => self.selection_for_clip(*track_id, *clip_id, Some(processor_id)),
            StableSelection::Effect {
                stack,
                processor_id,
            } => {
                if find_processor(&self.project, stack, processor_id).is_some() {
                    self.scoped_effect = Some((stack.clone(), processor_id.clone()));
                }
                Selection::None
            }
        };
    }

    fn selection_for_clip(
        &self,
        track_id: TrackId,
        clip_id: ClipId,
        processor_id: Option<&ProcessorId>,
    ) -> Selection {
        let Some(track) = self
            .current_composition()
            .tracks
            .iter()
            .position(|track| track.id == track_id.to_string())
        else {
            return Selection::None;
        };
        let Some(clip) = self.current_composition().tracks[track]
            .clips
            .iter()
            .position(|clip| clip.id == clip_id.to_string())
        else {
            return Selection::None;
        };
        processor_id.map_or(Selection::Clip { track, clip }, |processor_id| {
            self.current_composition().tracks[track].clips[clip]
                .effects
                .iter()
                .position(|effect| effect.id == processor_id.to_string())
                .map_or(Selection::None, |effect| Selection::Effect {
                    track,
                    clip,
                    effect,
                })
        })
    }

    fn add_asset_clip(&mut self, asset_index: usize, beat: f32, requested_track: Option<usize>) {
        let Some(asset) = self.project.assets.get(asset_index).cloned() else {
            return;
        };
        let composition_id = self.current_composition_id();
        let composition = self
            .project
            .compositions
            .iter()
            .find(|composition| composition.id == composition_id)
            .expect("current composition exists");
        let start = f64::from(beat.max(0.0)).min(composition.length.value());
        let requested_duration: f64 = if asset.tempo.is_some() { 8.0 } else { 4.0 };
        let duration = requested_duration.min((composition.length.value() - start).max(0.0));
        if duration <= 0.0 {
            return;
        }
        let mut commands = Vec::new();
        let (track_id, track_index) = requested_track
            .filter(|index| {
                self.current_composition()
                    .tracks
                    .get(*index)
                    .is_some_and(|track| track.kind == TrackKind::Audio)
            })
            .or_else(|| {
                self.current_composition()
                    .tracks
                    .iter()
                    .position(|track| track.kind == TrackKind::Audio)
            })
            .and_then(|index| self.current_track_id(index).map(|id| (id, index)))
            .unwrap_or_else(|| {
                let track = gaw_core::Track::audio(composition_id, "DROPPED AUDIO");
                let id = track.id;
                let index = composition.track_ids.len();
                commands.push(Command::AddTrack { track, index });
                (id, index)
            });
        let source_duration = asset_duration(&asset).unwrap_or(1.0).max(0.001);
        let mut clip = gaw_core::AudioClip::new(
            asset.id,
            gaw_core::Beats::new(start).expect("finite start"),
            gaw_core::Beats::new(duration).expect("positive duration"),
            gaw_core::SourceRange {
                start: gaw_core::Seconds::new(0.0).expect("zero is valid"),
                duration: gaw_core::Seconds::new(source_duration).expect("positive duration"),
            },
        );
        clip.name.clone_from(&asset.name);
        if asset.tempo.is_some() {
            clip.tempo_sync = gaw_core::TempoSync::Stretch;
        }
        let clip_id = clip.id;
        commands.push(Command::AddClip {
            track_id,
            clip: gaw_core::Clip::Audio(clip),
        });
        let transaction = Transaction::named("Drop asset on timeline", commands);
        self.commit_ui(
            &transaction,
            &[
                asset.id.to_string(),
                track_id.to_string(),
                clip_id.to_string(),
            ],
        );
        if self.last_error.is_none() {
            let clip_index = self
                .current_composition()
                .tracks
                .get(track_index)
                .and_then(|track| {
                    track
                        .clips
                        .iter()
                        .position(|clip| clip.id == clip_id.to_string())
                })
                .unwrap_or(0);
            self.selection = Selection::Clip {
                track: track_index,
                clip: clip_index,
            };
        }
    }
}

#[allow(clippy::cast_precision_loss)]
fn waveform(seed: f32, len: usize) -> Arc<[f32]> {
    (0..len)
        .map(|index| {
            let phase = index as f32 / len as f32;
            let body = (phase * 31.0 * seed).sin() * 0.55 + (phase * 73.0).sin() * 0.22;
            let envelope = (phase * std::f32::consts::PI).sin().powf(0.35);
            (body * envelope).abs().clamp(0.03, 0.96)
        })
        .collect::<Vec<_>>()
        .into()
}

fn parameter(id: &str, label: &str, value: f32, min: f32, max: f32, unit: &str) -> Parameter {
    Parameter {
        id: id.into(),
        label: label.into(),
        value: serde_json::json!(value),
        value_type: gaw_core::ParameterValueType::Number,
        range: Some((f64::from(min), f64::from(max))),
        choices: Vec::new(),
        unit: unit.into(),
        automatable: true,
        display_hint: "linear".into(),
    }
}

fn gain_effect(id: &str) -> Effect {
    Effect {
        id: id.into(),
        name: "Gain & Pan".into(),
        kind: "gaw.gain".into(),
        enabled: true,
        parameters: vec![
            parameter("gain_db", "Gain", -1.5, -24.0, 12.0, "dB"),
            parameter("pan", "Pan", 0.0, -1.0, 1.0, ""),
        ],
    }
}

fn delay_effect(id: &str) -> Effect {
    Effect {
        id: id.into(),
        name: "Echo Space".into(),
        kind: "gaw.delay".into(),
        enabled: true,
        parameters: vec![
            parameter("time", "Time", 0.5, 0.0625, 2.0, "beats"),
            parameter("feedback", "Feedback", 0.34, 0.0, 0.92, ""),
            parameter("mix", "Mix", 0.22, 0.0, 1.0, ""),
        ],
    }
}

fn core_effect(effect: &Effect) -> gaw_core::Processor {
    let id = ProcessorId::new(effect.id.clone()).expect("demo processor id is valid");
    let value = |name: &str, fallback: f32| {
        effect
            .parameters
            .iter()
            .find(|parameter| parameter.id == name)
            .and_then(|parameter| parameter.value.as_f64())
            .map_or(fallback, |value| value as f32)
    };
    let kind = if effect.kind == "gaw.delay" {
        let parameters = gaw_core::DelayParameters {
            time: gaw_core::TimeValue::Beats(f64::from(value("time", 0.5))),
            feedback: value("feedback", 0.35),
            mix: value("mix", 0.2),
            ..gaw_core::DelayParameters::default()
        };
        gaw_core::ProcessorKind::Delay(parameters)
    } else {
        let parameters = gaw_core::GainParameters {
            gain_db: value("gain_db", 0.0),
            pan: value("pan", 0.0),
            ..gaw_core::GainParameters::default()
        };
        gaw_core::ProcessorKind::Gain(parameters)
    };
    let mut processor = gaw_core::Processor::new(id, kind);
    processor.enabled = effect.enabled;
    processor
}

#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
/// Builds the explicit polished demo/new-project fixture.
///
/// # Panics
/// Panics if a compile-time fixture constant violates a canonical model invariant.
pub fn demo_project() -> Project {
    let shell_assets = demo_assets();
    let shell_compositions = demo_compositions();
    let sample_rate = gaw_core::SampleRate::new(48_000).expect("valid sample rate");
    let mut project = Project::new(
        "Glasshouse",
        gaw_core::Bpm::new(120.0).expect("valid tempo"),
        sample_rate,
    );

    project.assets = shell_assets
        .iter()
        .enumerate()
        .map(|(index, asset)| {
            let mut core = if index + 1 == shell_assets.len() {
                gaw_core::AudioAsset {
                    id: gaw_core::AssetId::new(),
                    name: asset.name.clone(),
                    definition: gaw_core::AudioAssetDefinition::Processed {
                        source_asset_id: project
                            .assets
                            .first()
                            .map_or_else(gaw_core::AssetId::new, |source| source.id),
                        transforms: Vec::new(),
                        effects: vec![core_effect(&gain_effect("fx_asset_processed"))],
                    },
                    tempo: None,
                    revisions: Vec::new(),
                    current_revision_id: None,
                }
            } else {
                gaw_core::AudioAsset::imported(
                    asset.name.clone(),
                    gaw_core::ImportedAudio {
                        media_path: gaw_core::ProjectPath::new(format!("audio/asset_{index}.wav"))
                            .expect("valid path"),
                        original_filename: format!("asset_{index}.wav"),
                        content_hash: gaw_core::ContentHash::new(format!("{index:064x}"))
                            .expect("valid hash"),
                        sample_rate,
                        layout: if asset.channels == 1 {
                            gaw_core::ChannelLayout::Mono
                        } else {
                            gaw_core::ChannelLayout::Stereo
                        },
                        frames: gaw_core::FrameCount(
                            (asset.duration_seconds * sample_rate.value() as f32) as u64,
                        ),
                    },
                )
            };
            core.tempo = asset.bpm.map(|bpm| gaw_core::AssetTempo {
                bpm: gaw_core::Bpm::new(f64::from(bpm)).expect("valid tempo"),
                first_beat: gaw_core::Seconds::new(0.0).expect("zero is valid"),
            });
            core
        })
        .collect();
    // The processed fixture depends on the first imported asset.
    if let Some(source_id) = project.assets.first().map(|asset| asset.id)
        && let Some(last) = project.assets.last_mut()
        && let gaw_core::AudioAssetDefinition::Processed {
            source_asset_id, ..
        } = &mut last.definition
    {
        *source_asset_id = source_id;
    }

    let mut compositions = shell_compositions
        .iter()
        .map(|composition| {
            let mut core = gaw_core::Composition::new(
                composition.name.clone(),
                gaw_core::Beats::new(f64::from(composition.length_beats)).expect("valid length"),
            );
            core.output_effects = composition.output_effects.iter().map(core_effect).collect();
            core
        })
        .collect::<Vec<_>>();
    project.root_composition_id = compositions[0].id;
    let composition_ids = compositions
        .iter()
        .map(|value| value.id)
        .collect::<Vec<_>>();
    let mut tracks = Vec::new();
    let mut event_data = Vec::new();

    for (composition_index, shell_composition) in shell_compositions.iter().enumerate() {
        let mut track_ids = Vec::new();
        for shell_track in &shell_composition.tracks {
            let composition_id = composition_ids[composition_index];
            let mut core_track = if shell_track.kind == TrackKind::Event {
                let mut sampler = gaw_core::Sampler::new(12).expect("valid sampler");
                let zone_asset = &project.assets[0];
                let zone_duration = asset_duration(zone_asset).unwrap_or(0.5).min(0.5);
                sampler.zones.push(gaw_core::SamplerZone {
                    id: gaw_core::SamplerZoneId::new(),
                    name: "Demo zone".into(),
                    asset_id: zone_asset.id,
                    source: gaw_core::SourceRange {
                        start: gaw_core::Seconds::new(0.0).expect("valid"),
                        duration: gaw_core::Seconds::new(zone_duration).expect("valid"),
                    },
                    root_note: gaw_core::MidiNote::new(60).expect("valid"),
                    note_range: gaw_core::NoteRange::new(36, 84).expect("valid"),
                    velocity_range: gaw_core::VelocityRange::new(1, 127).expect("valid"),
                    playback: gaw_core::SamplerPlayback::OneShot,
                    gain: gaw_core::Decibels::new(0.0).expect("valid"),
                    velocity_sensitivity: gaw_core::Ratio::new(1.0).expect("valid"),
                    attack: gaw_core::Milliseconds::new(2.0).expect("valid"),
                    release: gaw_core::Milliseconds::new(80.0).expect("valid"),
                    reverse: false,
                    choke_group: Some(1),
                });
                gaw_core::Track::event(
                    composition_id,
                    shell_track.name.clone(),
                    gaw_core::Instrument::sampler("Slice Sampler", sampler),
                )
            } else {
                gaw_core::Track::audio(composition_id, shell_track.name.clone())
            };
            core_track.muted = shell_track.muted;
            core_track.solo = shell_track.solo;
            core_track.effects = shell_track.effects.iter().map(core_effect).collect();

            for shell_clip in &shell_track.clips {
                let core_clip = match &shell_clip.kind {
                    ClipKind::Audio { asset, sync, .. } => {
                        let asset = &project.assets[*asset];
                        let source_duration = asset_duration(asset).unwrap_or(1.0);
                        let mut clip = gaw_core::AudioClip::new(
                            asset.id,
                            gaw_core::Beats::new(f64::from(shell_clip.start)).expect("valid"),
                            gaw_core::Beats::new(f64::from(shell_clip.length)).expect("valid"),
                            gaw_core::SourceRange {
                                start: gaw_core::Seconds::new(0.0).expect("valid"),
                                duration: gaw_core::Seconds::new(source_duration).expect("valid"),
                            },
                        );
                        clip.name.clone_from(&shell_clip.name);
                        clip.tempo_sync = match sync {
                            SyncMode::None => gaw_core::TempoSync::None,
                            SyncMode::Repitch => gaw_core::TempoSync::Repitch,
                            SyncMode::Stretch => gaw_core::TempoSync::Stretch,
                        };
                        clip.effects = shell_clip.effects.iter().map(core_effect).collect();
                        gaw_core::Clip::Audio(clip)
                    }
                    ClipKind::Event { notes } => {
                        let mut events = gaw_core::EventData::new(shell_clip.name.clone());
                        events.events = notes
                            .iter()
                            .map(|note| {
                                gaw_core::NoteEvent::new(
                                    gaw_core::Beats::new(f64::from(note.start)).expect("valid"),
                                    gaw_core::Beats::new(f64::from(note.length)).expect("valid"),
                                    note.pitch,
                                    (note.velocity * 127.0).round() as u8,
                                )
                                .map(gaw_core::Event::Note)
                                .expect("valid note")
                            })
                            .collect();
                        let event_data_id = events.id;
                        event_data.push(events);
                        let mut clip = gaw_core::EventClip::new(
                            event_data_id,
                            gaw_core::Beats::new(f64::from(shell_clip.start)).expect("valid"),
                            gaw_core::Beats::new(f64::from(shell_clip.length)).expect("valid"),
                        );
                        clip.name.clone_from(&shell_clip.name);
                        gaw_core::Clip::Event(clip)
                    }
                    ClipKind::Composition { child, .. } => {
                        let mut clip = gaw_core::CompositionClip::new(
                            composition_ids[*child],
                            gaw_core::Beats::new(f64::from(shell_clip.start)).expect("valid"),
                            gaw_core::Beats::new(f64::from(shell_clip.length)).expect("valid"),
                        );
                        clip.name.clone_from(&shell_clip.name);
                        clip.effects = shell_clip.effects.iter().map(core_effect).collect();
                        gaw_core::Clip::Composition(clip)
                    }
                };
                core_track.clips.push(core_clip);
            }
            track_ids.push(core_track.id);
            tracks.push(core_track);
        }
        compositions[composition_index].track_ids = track_ids;
    }
    project.compositions = compositions;
    project.tracks = tracks;
    project.event_data = event_data;
    project
}

fn demo_assets() -> Vec<Asset> {
    vec![
        Asset {
            id: "ast_kick".into(),
            name: "Soft Kick 04".into(),
            duration_seconds: 0.72,
            channels: 1,
            bpm: None,
            first_beat_seconds: None,
            waveform: waveform(1.2, 128),
            changed_by_agent: false,
            definition: "demo".into(),
            media_path: None,
            content_hash: None,
            sample_rate: 48_000,
            frames: 0,
            revision_count: 0,
            current_revision: None,
            effects: Vec::new(),
            structure_path: String::new(),
        },
        Asset {
            id: "ast_loop".into(),
            name: "Dust Loop".into(),
            duration_seconds: 4.36,
            channels: 2,
            bpm: Some(110.0),
            first_beat_seconds: Some(0.0),
            waveform: waveform(1.8, 256),
            changed_by_agent: false,
            definition: "demo".into(),
            media_path: None,
            content_hash: None,
            sample_rate: 48_000,
            frames: 0,
            revision_count: 0,
            current_revision: None,
            effects: Vec::new(),
            structure_path: String::new(),
        },
        Asset {
            id: "ast_vocal".into(),
            name: "Vocal Air".into(),
            duration_seconds: 6.18,
            channels: 2,
            bpm: Some(120.0),
            first_beat_seconds: Some(0.0),
            waveform: waveform(2.4, 256),
            changed_by_agent: true,
            definition: "demo".into(),
            media_path: None,
            content_hash: None,
            sample_rate: 48_000,
            frames: 0,
            revision_count: 0,
            current_revision: None,
            effects: Vec::new(),
            structure_path: String::new(),
        },
        Asset {
            id: "ast_hat".into(),
            name: "Porcelain Hat".into(),
            duration_seconds: 0.31,
            channels: 1,
            bpm: None,
            first_beat_seconds: None,
            waveform: waveform(3.1, 96),
            changed_by_agent: false,
            definition: "demo".into(),
            media_path: None,
            content_hash: None,
            sample_rate: 48_000,
            frames: 0,
            revision_count: 0,
            current_revision: None,
            effects: Vec::new(),
            structure_path: String::new(),
        },
        Asset {
            id: "ast_texture".into(),
            name: "Tape Garden".into(),
            duration_seconds: 12.8,
            channels: 2,
            bpm: Some(90.0),
            first_beat_seconds: Some(0.0),
            waveform: waveform(0.8, 320),
            changed_by_agent: false,
            definition: "demo".into(),
            media_path: None,
            content_hash: None,
            sample_rate: 48_000,
            frames: 0,
            revision_count: 0,
            current_revision: None,
            effects: Vec::new(),
            structure_path: String::new(),
        },
    ]
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
fn demo_compositions() -> Vec<Composition> {
    let melody_notes: Arc<[Note]> = (0..32)
        .map(|index| Note {
            event_index: index,
            start: index as f32 * 0.5,
            length: if index % 4 == 3 { 0.42 } else { 0.28 },
            pitch: 55 + ((index * 5) % 17) as u8,
            velocity: 0.55 + (index % 4) as f32 * 0.1,
        })
        .collect::<Vec<_>>()
        .into();
    let drum_notes: Arc<[Note]> = (0..48)
        .map(|index| Note {
            event_index: index,
            start: index as f32 * 0.25,
            length: 0.12,
            pitch: [36, 42, 42, 38][index % 4],
            velocity: if index % 4 == 0 { 0.95 } else { 0.62 },
        })
        .collect::<Vec<_>>()
        .into();

    let root = Composition {
        id: "cmp_song".into(),
        name: "Glasshouse".into(),
        length_beats: 96.0,
        tracks: vec![
            Track {
                id: "trk_drums".into(),
                name: "DRUM PRINT".into(),
                kind: TrackKind::Audio,
                muted: false,
                solo: false,
                level: 0.82,
                max_visual_length: 24.0,
                clips: vec![
                    Clip {
                        id: "clip_kick".into(),
                        name: "Kick bed".into(),
                        start: 0.0,
                        length: 12.0,
                        gain_db: -2.0,
                        waveform: waveform(1.1, 320),
                        kind: ClipKind::Audio {
                            asset: 0,
                            sync: SyncMode::None,
                            source_bpm: None,
                        },
                        effects: vec![gain_effect("fx_kick_gain")],
                    },
                    Clip {
                        id: "clip_dust".into(),
                        name: "Dust Loop".into(),
                        start: 14.0,
                        length: 18.0,
                        gain_db: -3.5,
                        waveform: waveform(1.8, 360),
                        kind: ClipKind::Audio {
                            asset: 1,
                            sync: SyncMode::Repitch,
                            source_bpm: Some(110.0),
                        },
                        effects: vec![gain_effect("fx_dust_gain"), delay_effect("fx_dust_delay")],
                    },
                    Clip {
                        id: "clip_dust_b".into(),
                        name: "Dust Loop / B".into(),
                        start: 48.0,
                        length: 24.0,
                        gain_db: -4.0,
                        waveform: waveform(2.0, 420),
                        kind: ClipKind::Audio {
                            asset: 1,
                            sync: SyncMode::Stretch,
                            source_bpm: Some(110.0),
                        },
                        effects: vec![gain_effect("fx_dust_b_gain")],
                    },
                ],
                effects: vec![gain_effect("fx_drums_track")],
                sampler_zones: Vec::new(),
                sampler_polyphony: None,
                sampler_voice_stealing: None,
                sampler_output_gain_db: None,
                structure_path: String::new(),
            },
            Track {
                id: "trk_synth".into(),
                name: "GLASS KEYS".into(),
                kind: TrackKind::Event,
                muted: false,
                solo: false,
                level: 0.72,
                max_visual_length: 16.0,
                clips: vec![
                    Clip {
                        id: "clip_keys".into(),
                        name: "Folded melody".into(),
                        start: 8.0,
                        length: 16.0,
                        gain_db: 0.0,
                        waveform: Arc::from([]),
                        kind: ClipKind::Event {
                            notes: Arc::clone(&melody_notes),
                        },
                        effects: Vec::new(),
                    },
                    Clip {
                        id: "clip_keys_b".into(),
                        name: "Melody variation".into(),
                        start: 40.0,
                        length: 16.0,
                        gain_db: 0.0,
                        waveform: Arc::from([]),
                        kind: ClipKind::Event {
                            notes: melody_notes,
                        },
                        effects: Vec::new(),
                    },
                ],
                effects: vec![gain_effect("fx_keys_gain"), delay_effect("fx_keys_delay")],
                sampler_zones: Vec::new(),
                sampler_polyphony: None,
                sampler_voice_stealing: None,
                sampler_output_gain_db: None,
                structure_path: String::new(),
            },
            Track {
                id: "trk_chorus".into(),
                name: "CHORUS NEST".into(),
                kind: TrackKind::Composition,
                muted: false,
                solo: false,
                level: 0.9,
                max_visual_length: 19.0,
                clips: vec![
                    Clip {
                        id: "clip_chorus".into(),
                        name: "Chorus".into(),
                        start: 24.0,
                        length: 16.0,
                        gain_db: -0.8,
                        waveform: waveform(2.8, 360),
                        kind: ClipKind::Composition {
                            child: 1,
                            render: RenderState::Stale,
                            tail_beats: 2.5,
                        },
                        effects: vec![
                            gain_effect("fx_chorus_gain"),
                            delay_effect("fx_chorus_delay"),
                        ],
                    },
                    Clip {
                        id: "clip_chorus_render".into(),
                        name: "Chorus / lift".into(),
                        start: 64.0,
                        length: 16.0,
                        gain_db: -0.8,
                        waveform: waveform(3.3, 360),
                        kind: ClipKind::Composition {
                            child: 1,
                            render: RenderState::Rendering(67),
                            tail_beats: 3.0,
                        },
                        effects: vec![gain_effect("fx_chorus_lift_gain")],
                    },
                ],
                effects: vec![gain_effect("fx_chorus_track")],
                sampler_zones: Vec::new(),
                sampler_polyphony: None,
                sampler_voice_stealing: None,
                sampler_output_gain_db: None,
                structure_path: String::new(),
            },
            Track {
                id: "trk_vocal".into(),
                name: "VOCAL AIR".into(),
                kind: TrackKind::Audio,
                muted: false,
                solo: false,
                level: 0.66,
                max_visual_length: 11.0,
                clips: vec![Clip {
                    id: "clip_vocal".into(),
                    name: "Vocal Air / reverse".into(),
                    start: 34.0,
                    length: 11.0,
                    gain_db: -5.5,
                    waveform: waveform(2.4, 280),
                    kind: ClipKind::Audio {
                        asset: 2,
                        sync: SyncMode::None,
                        source_bpm: Some(120.0),
                    },
                    effects: vec![gain_effect("fx_vocal_gain"), delay_effect("fx_vocal_delay")],
                }],
                effects: vec![gain_effect("fx_vocal_track")],
                sampler_zones: Vec::new(),
                sampler_polyphony: None,
                sampler_voice_stealing: None,
                sampler_output_gain_db: None,
                structure_path: String::new(),
            },
        ],
        output_effects: vec![gain_effect("fx_song_output")],
        structure_path: String::new(),
    };

    let chorus = Composition {
        id: "cmp_chorus".into(),
        name: "Chorus".into(),
        length_beats: 16.0,
        tracks: vec![
            Track {
                id: "trk_chorus_drums".into(),
                name: "DRUM KIT".into(),
                kind: TrackKind::Event,
                muted: false,
                solo: false,
                level: 0.85,
                max_visual_length: 12.0,
                clips: vec![Clip {
                    id: "clip_chorus_drums".into(),
                    name: "Chorus kit".into(),
                    start: 0.0,
                    length: 12.0,
                    gain_db: 0.0,
                    waveform: Arc::from([]),
                    kind: ClipKind::Event { notes: drum_notes },
                    effects: Vec::new(),
                }],
                effects: vec![gain_effect("fx_kit_gain")],
                sampler_zones: Vec::new(),
                sampler_polyphony: None,
                sampler_voice_stealing: None,
                sampler_output_gain_db: None,
                structure_path: String::new(),
            },
            Track {
                id: "trk_texture".into(),
                name: "TEXTURE".into(),
                kind: TrackKind::Audio,
                muted: false,
                solo: false,
                level: 0.65,
                max_visual_length: 16.0,
                clips: vec![Clip {
                    id: "clip_texture".into(),
                    name: "Tape Garden".into(),
                    start: 0.0,
                    length: 16.0,
                    gain_db: -7.0,
                    waveform: waveform(0.8, 320),
                    kind: ClipKind::Audio {
                        asset: 4,
                        sync: SyncMode::Stretch,
                        source_bpm: Some(90.0),
                    },
                    effects: vec![
                        gain_effect("fx_texture_gain"),
                        delay_effect("fx_texture_delay"),
                    ],
                }],
                effects: vec![gain_effect("fx_texture_track")],
                sampler_zones: Vec::new(),
                sampler_polyphony: None,
                sampler_voice_stealing: None,
                sampler_output_gain_db: None,
                structure_path: String::new(),
            },
            Track {
                id: "trk_vocal_texture".into(),
                name: "VOCAL TEXTURE".into(),
                kind: TrackKind::Composition,
                muted: false,
                solo: false,
                level: 0.78,
                max_visual_length: 9.25,
                clips: vec![Clip {
                    id: "clip_vocal_texture".into(),
                    name: "Vocal Texture".into(),
                    start: 4.0,
                    length: 8.0,
                    gain_db: -2.0,
                    waveform: waveform(3.7, 240),
                    kind: ClipKind::Composition {
                        child: 2,
                        render: RenderState::Fresh,
                        tail_beats: 1.25,
                    },
                    effects: vec![gain_effect("fx_nested_gain")],
                }],
                effects: Vec::new(),
                sampler_zones: Vec::new(),
                sampler_polyphony: None,
                sampler_voice_stealing: None,
                sampler_output_gain_db: None,
                structure_path: String::new(),
            },
        ],
        output_effects: vec![gain_effect("fx_chorus_output")],
        structure_path: String::new(),
    };

    let vocal_texture = Composition {
        id: "cmp_vocal_texture".into(),
        name: "Vocal Texture".into(),
        length_beats: 8.0,
        tracks: vec![Track {
            id: "trk_slices".into(),
            name: "SLICE SAMPLER".into(),
            kind: TrackKind::Event,
            muted: false,
            solo: false,
            level: 0.82,
            max_visual_length: 8.0,
            clips: vec![Clip {
                id: "clip_slices".into(),
                name: "Air slices".into(),
                start: 0.0,
                length: 8.0,
                gain_db: 0.0,
                waveform: Arc::from([]),
                kind: ClipKind::Event {
                    notes: Arc::from([
                        Note {
                            event_index: 0,
                            start: 0.0,
                            length: 0.4,
                            pitch: 60,
                            velocity: 0.8,
                        },
                        Note {
                            event_index: 1,
                            start: 1.5,
                            length: 0.6,
                            pitch: 64,
                            velocity: 0.65,
                        },
                        Note {
                            event_index: 2,
                            start: 3.0,
                            length: 0.8,
                            pitch: 67,
                            velocity: 0.9,
                        },
                        Note {
                            event_index: 3,
                            start: 5.0,
                            length: 1.2,
                            pitch: 72,
                            velocity: 0.72,
                        },
                    ]),
                },
                effects: Vec::new(),
            }],
            effects: vec![
                gain_effect("fx_slices_gain"),
                delay_effect("fx_slices_delay"),
            ],
            sampler_zones: Vec::new(),
            sampler_polyphony: None,
            sampler_voice_stealing: None,
            sampler_output_gain_db: None,
            structure_path: String::new(),
        }],
        output_effects: vec![gain_effect("fx_texture_output")],
        structure_path: String::new(),
    };
    vec![root, chorus, vocal_texture]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_data_exercises_core_surfaces() {
        let vm = DemoViewModel::demo();
        let clips = vm
            .compositions
            .iter()
            .flat_map(|composition| composition.tracks.iter())
            .flat_map(|track| &track.clips);
        let mut audio = false;
        let mut event = false;
        let mut nested = false;
        for clip in clips {
            match clip.kind {
                ClipKind::Audio { .. } => audio = true,
                ClipKind::Event { .. } => event = true,
                ClipKind::Composition { .. } => nested = true,
            }
        }
        assert!(audio && event && nested);
        assert!(vm.assets.iter().any(|asset| asset.bpm.is_some()));
        assert!(vm.compositions.len() >= 3);
        assert!(
            vm.compositions
                .iter()
                .all(|composition| !composition.output_effects.is_empty())
        );
        assert!(vm.assets.iter().any(|asset| !asset.effects.is_empty()));
        assert!(
            vm.compositions
                .iter()
                .flat_map(|composition| &composition.tracks)
                .filter(|track| track.kind == TrackKind::Event)
                .all(|track| !track.sampler_zones.is_empty())
        );
        assert!(
            vm.compositions
                .iter()
                .flat_map(|composition| &composition.tracks)
                .filter(|track| track.kind == TrackKind::Event)
                .all(|track| {
                    !track.effects.is_empty()
                        && track.clips.iter().all(|clip| clip.effects.is_empty())
                })
        );
    }

    #[test]
    fn nested_navigation_and_breadcrumbs_are_bounded() {
        let mut vm = DemoViewModel::demo();
        let root_id = vm.current_composition().id.clone();
        vm.apply(Intent::EnterChild { track: 2, clip: 0 });
        assert_eq!(
            vm.breadcrumbs()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["Glasshouse", "Chorus"]
        );
        vm.apply(Intent::Back);
        vm.apply(Intent::Back);
        assert_eq!(vm.current_composition().id, root_id);
    }

    #[test]
    fn selection_derives_all_context_editors() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::Select(Selection::Asset(0)));
        assert_eq!(vm.editor_kind(), EditorKind::Waveform);
        vm.apply(Intent::Select(Selection::Clip { track: 1, clip: 0 }));
        assert_eq!(vm.editor_kind(), EditorKind::PianoRoll);
        vm.apply(Intent::Select(Selection::Sampler { track: 1 }));
        assert_eq!(vm.editor_kind(), EditorKind::Sampler);
        vm.apply(Intent::Select(Selection::Effect {
            track: 0,
            clip: 1,
            effect: 0,
        }));
        assert_eq!(vm.editor_kind(), EditorKind::Effect);
    }

    #[test]
    fn transport_and_effect_actions_clamp_and_preserve_identity() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::SetBpm(500.0));
        vm.apply(Intent::Seek(500.0));
        assert!((vm.transport.bpm - MAX_BPM).abs() < f32::EPSILON);
        assert!((vm.transport.playhead - 96.0).abs() < f32::EPSILON);
        let id = vm.compositions[0].tracks[0].clips[1].effects[0].id.clone();
        vm.apply(Intent::MoveEffect {
            track: 0,
            clip: 1,
            effect: 0,
            delta: 1,
        });
        assert_eq!(vm.compositions[0].tracks[0].clips[1].effects[1].id, id);
        assert_eq!(
            vm.selection,
            Selection::Effect {
                track: 0,
                clip: 1,
                effect: 1
            }
        );
    }

    #[test]
    fn dropping_assets_creates_and_selects_the_exact_clip() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::AddAssetClip {
            asset: 1,
            beat: 6.0,
            track: Some(0),
        });
        vm.apply(Intent::AddAssetClip {
            asset: 2,
            beat: 10.0,
            track: Some(0),
        });
        let Selection::Clip { track, clip } = vm.selection else {
            panic!("dropped clip should be selected");
        };
        let selected = &vm.current_composition().tracks[track].clips[clip];
        assert!(
            vm.project
                .tracks
                .iter()
                .flat_map(|track| &track.clips)
                .any(|clip| clip.id().to_string() == selected.id)
        );
        assert!((selected.start - 10.0).abs() < f32::EPSILON);
        assert!((selected.length - 8.0).abs() < f32::EPSILON);
    }

    #[test]
    fn audio_drop_creates_a_track_when_target_is_event_only() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::EnterChild { track: 2, clip: 0 });
        vm.apply(Intent::EnterChild { track: 2, clip: 0 });
        vm.apply(Intent::AddAssetClip {
            asset: 0,
            beat: 2.0,
            track: Some(0),
        });
        let Selection::Clip { track, clip } = vm.selection else {
            panic!("dropped clip should be selected");
        };
        assert_eq!(
            vm.current_composition().tracks[track].kind,
            TrackKind::Audio
        );
        assert!(matches!(
            vm.current_composition().tracks[track].clips[clip].kind,
            ClipKind::Audio { .. }
        ));
    }

    #[test]
    fn loop_advance_preserves_overshoot() {
        let mut vm = DemoViewModel::demo();
        vm.transport.playing = true;
        vm.transport.playhead = 95.0;
        vm.advance(1.0);
        assert!((vm.transport.playhead - 1.0).abs() < f32::EPSILON);
        vm.advance(100.0);
        assert!((vm.transport.playhead - 9.0).abs() < f32::EPSILON);
    }

    #[test]
    fn agent_highlight_fades_and_expires() {
        let mut vm = DemoViewModel::demo();
        let asset_id = vm.assets[0].id.clone();
        vm.apply(Intent::SimulateAgentChange(10.0));
        assert!((vm.highlight_alpha(&asset_id, 10.0) - 1.0).abs() < f32::EPSILON);
        assert!(vm.highlight_alpha(&asset_id, 11.2) > 0.45);
        assert!(vm.highlight_alpha(&asset_id, 13.0).abs() < f32::EPSILON);
        let update = vm.take_updates().next().expect("agent update emitted");
        assert_eq!(update.source, ChangeSource::Agent);
        assert_eq!(&*update.changed_ids, &[asset_id]);
        assert!(update.transaction.is_some());
    }

    #[test]
    fn canonical_edits_round_trip_through_undo_redo() {
        let mut vm = DemoViewModel::demo();
        let before = vm.project.clone();
        vm.apply(Intent::SetBpm(132.0));
        let after = vm.project.clone();
        assert_ne!(after, before);
        assert!((after.bpm.value() - 132.0).abs() < f64::EPSILON);
        vm.apply(Intent::Undo(1.0));
        assert_eq!(vm.project, before);
        vm.apply(Intent::Redo(2.0));
        assert_eq!(vm.project, after);
        assert_eq!(vm.revision(), 3);
        assert_eq!(
            vm.take_updates()
                .map(|update| update.source)
                .collect::<Vec<_>>(),
            [ChangeSource::Ui, ChangeSource::Undo, ChangeSource::Redo]
        );
    }

    #[test]
    fn projection_sorts_clips_but_selection_resolves_canonical_id() {
        let mut project = demo_project();
        project.tracks[0].clips.swap(0, 2);
        let mut vm = DemoViewModel::from_project(project).expect("reordered project is valid");
        vm.apply(Intent::Select(Selection::Clip { track: 0, clip: 0 }));
        let selected_view = vm.current_composition().tracks[0].clips[0].id.clone();
        let StableSelection::Clip { clip_id, .. } = vm.stable_selection() else {
            panic!("clip selection should resolve");
        };
        assert_eq!(clip_id.to_string(), selected_view);
    }

    #[test]
    fn invalid_external_transaction_is_atomic_and_silent() {
        let mut vm = DemoViewModel::demo();
        let before = vm.project.clone();
        let revision = vm.revision();
        let asset_id = vm.project.assets[0].id;
        let transaction =
            Transaction::named("invalid removal", [Command::RemoveAsset { asset_id }]);
        assert!(
            vm.apply_agent_transaction(&transaction, [asset_id.to_string()], 3.0)
                .is_err()
        );
        assert_eq!(vm.project, before);
        assert_eq!(vm.revision(), revision);
        assert_eq!(vm.take_updates().count(), 0);
    }

    #[test]
    fn external_snapshot_swap_is_atomic_and_preserves_stable_ui_state() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::Select(Selection::Asset(0)));
        vm.apply(Intent::SetBpm(132.0));
        vm.take_updates().for_each(drop);
        let selected_id = vm.assets[0].id.clone();
        let waveform = Arc::clone(&vm.assets[0].waveform);
        let previous_revision = vm.revision();
        let mut replacement = vm.project.clone();
        replacement.name = "Externally renamed".into();

        vm.replace_project_from_agent(replacement, [selected_id.clone()], 4.0)
            .expect("valid external project");

        assert_eq!(vm.project.name, "Externally renamed");
        assert_eq!(vm.revision(), previous_revision + 1);
        assert_eq!(
            vm.stable_selection(),
            StableSelection::Asset(vm.project.assets[0].id)
        );
        assert!(Arc::ptr_eq(&waveform, &vm.assets[0].waveform));
        assert!(vm.assets[0].changed_by_agent);
        assert!((vm.highlight_alpha(&selected_id, 4.0) - 1.0).abs() < f32::EPSILON);
        let installed = vm.project.clone();
        vm.apply(Intent::Undo(5.0));
        assert_eq!(vm.project, installed, "external reload clears undo history");
        let update = vm.take_updates().next().expect("reload update");
        assert_eq!(update.source, ChangeSource::Agent);
        assert!(update.transaction.is_none());
        assert_eq!(&*update.changed_ids, &[selected_id]);
    }

    #[test]
    fn invalid_external_snapshot_leaves_the_last_valid_state_untouched() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::Select(Selection::Asset(0)));
        let before = vm.project.clone();
        let selection = vm.stable_selection();
        let revision = vm.revision();
        let mut invalid = vm.project.clone();
        invalid.compositions.clear();

        assert!(
            vm.replace_project_from_agent(invalid, ["missing".into()], 1.0)
                .is_err()
        );
        assert_eq!(vm.project, before);
        assert_eq!(vm.stable_selection(), selection);
        assert_eq!(vm.revision(), revision);
        assert_eq!(vm.take_updates().count(), 0);
    }

    #[test]
    fn controller_can_set_nested_composition_render_state() {
        let mut vm = DemoViewModel::demo();
        let clip_id = vm
            .compositions
            .iter()
            .flat_map(|composition| &composition.tracks)
            .flat_map(|track| &track.clips)
            .find(|clip| matches!(clip.kind, ClipKind::Composition { .. }))
            .expect("nested composition clip")
            .id
            .clone();

        assert!(vm.set_composition_clip_render_state(&clip_id, RenderState::Rendering(42)));
        let render = vm
            .compositions
            .iter()
            .flat_map(|composition| &composition.tracks)
            .flat_map(|track| &track.clips)
            .find(|clip| clip.id == clip_id)
            .map(|clip| match clip.kind {
                ClipKind::Composition { render, .. } => render,
                _ => unreachable!(),
            });
        assert_eq!(render, Some(RenderState::Rendering(42)));
        assert!(!vm.set_composition_clip_render_state("missing", RenderState::Fresh));
    }

    #[test]
    fn cloned_project_updates_share_delta_payloads() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::SetBpm(123.0));
        let update = vm.take_updates().next().expect("UI update");
        let cloned = update.clone();

        assert!(Arc::ptr_eq(&update.changed_ids, &cloned.changed_ids));
        assert!(Arc::ptr_eq(
            update.transaction.as_ref().expect("forward delta"),
            cloned.transaction.as_ref().expect("shared forward delta")
        ));
        assert_eq!(update.source, ChangeSource::Ui);
    }

    #[test]
    fn typed_asset_note_zone_and_audio_edits_update_core() {
        let mut vm = DemoViewModel::demo();
        vm.set_asset_tempo(0, Some(98.0), 0.1);
        assert!(
            (vm.project.assets[0].tempo.expect("tempo").bpm.value() - 98.0).abs() < f64::EPSILON
        );

        vm.apply(Intent::Select(Selection::Clip { track: 0, clip: 0 }));
        let before = vm.selected_audio_details().expect("audio details");
        vm.edit_selected_audio_clip(AudioClipEdit::ToggleReverse);
        assert_ne!(
            vm.selected_audio_details().expect("audio details").2,
            before.2
        );

        vm.apply(Intent::Select(Selection::Clip { track: 1, clip: 0 }));
        let event_count = vm.project.event_data[0].events.len();
        vm.add_note_to_selected_event_clip();
        assert_eq!(vm.project.event_data[0].events.len(), event_count + 1);

        vm.apply(Intent::Select(Selection::Sampler { track: 1 }));
        let before = vm.current_composition().tracks[1].sampler_zones[0].reverse;
        vm.toggle_first_sampler_zone_reverse(1);
        assert_ne!(
            vm.current_composition().tracks[1].sampler_zones[0].reverse,
            before
        );
    }

    #[test]
    fn every_processor_scope_maps_and_uses_typed_commands() {
        let mut vm = DemoViewModel::demo();
        let composition_id = vm.project.root_composition_id;
        let track_id = vm.project.compositions[0].track_ids[0];
        let clip_id = vm
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("track")
            .clips[0]
            .id();
        let (composition_track_id, composition_clip_id) = vm
            .project
            .tracks
            .iter()
            .find_map(|track| {
                track.clips.iter().find_map(|clip| match clip {
                    gaw_core::Clip::Composition(clip) => Some((track.id, clip.id)),
                    gaw_core::Clip::Audio(_) | gaw_core::Clip::Event(_) => None,
                })
            })
            .expect("composition clip");
        let scopes = [
            ProcessorStack::Clip { track_id, clip_id },
            ProcessorStack::CompositionClip {
                track_id: composition_track_id,
                clip_id: composition_clip_id,
            },
            ProcessorStack::Track { track_id },
            ProcessorStack::CompositionOutput { composition_id },
        ];
        for stack in scopes {
            let original = vm.project.clone();
            let before = processor_stack(&vm.project, &stack).expect("mapped stack")[0].enabled;
            vm.toggle_processor_at(stack.clone(), 0);
            assert_ne!(
                processor_stack(&vm.project, &stack).expect("mapped stack")[0].enabled,
                before
            );
            vm.apply(Intent::Undo(0.0));
            assert_eq!(
                processor_stack(&vm.project, &stack).expect("mapped stack")[0].enabled,
                before
            );

            vm.select_processor_at(stack.clone(), 0);
            let parameter = vm
                .selected_processor_view()
                .expect("selected processor")
                .parameters[0]
                .clone();
            let value = parameter.value.as_f64().expect("numeric gain") + 0.5;
            vm.set_selected_processor_parameter(0, serde_json::json!(value));
            assert_ne!(vm.project, original);
            vm.apply(Intent::Undo(0.0));
            assert_eq!(vm.project, original);

            let original_len = processor_stack(&vm.project, &stack)
                .expect("mapped stack")
                .len();
            vm.insert_processor(stack.clone(), 0);
            assert_eq!(
                processor_stack(&vm.project, &stack)
                    .expect("mapped stack")
                    .len(),
                original_len + 1
            );
            vm.move_processor_at(stack.clone(), original_len, -1);
            vm.remove_processor_at(stack.clone(), original_len - 1);
            assert_eq!(
                processor_stack(&vm.project, &stack)
                    .expect("mapped stack")
                    .len(),
                original_len
            );
            vm.apply(Intent::Undo(0.0));
            vm.apply(Intent::Undo(0.0));
            vm.apply(Intent::Undo(0.0));
            assert_eq!(vm.project, original);
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn timeline_note_and_sampler_edits_are_canonical_and_undoable() {
        let mut vm = DemoViewModel::demo();

        let before_clip = vm.project.clone();
        let source_track_id = vm.current_track_id(0).expect("audio track");
        let target_track_id = vm.current_track_id(2).expect("second audio track");
        let clip_id = vm.current_composition().tracks[0].clips[0].id.clone();
        vm.apply(Intent::EditClip {
            track: 0,
            clip: 0,
            start: 2.0,
            length: 3.5,
            target_track: 2,
        });
        assert!(
            vm.project
                .tracks
                .iter()
                .find(|track| track.id == source_track_id)
                .is_some_and(|track| track
                    .clips
                    .iter()
                    .all(|clip| clip.id().to_string() != clip_id))
        );
        let moved = vm
            .project
            .tracks
            .iter()
            .find(|track| track.id == target_track_id)
            .and_then(|track| {
                track
                    .clips
                    .iter()
                    .find(|clip| clip.id().to_string() == clip_id)
            })
            .expect("moved clip");
        assert!((moved.start().value() - 2.0).abs() < f64::EPSILON);
        assert_eq!(
            vm.stable_selection(),
            StableSelection::Clip {
                track_id: target_track_id,
                clip_id: moved.id(),
            }
        );
        vm.apply(Intent::Undo(0.0));
        assert_eq!(vm.project, before_clip);

        let source_before = match &vm.project.tracks[0].clips[0] {
            gaw_core::Clip::Audio(clip) => clip.source,
            _ => panic!("audio clip"),
        };
        vm.apply(Intent::EditClip {
            track: 0,
            clip: 0,
            start: 0.5,
            length: 11.5,
            target_track: 0,
        });
        let source_after = match &vm.project.tracks[0].clips[0] {
            gaw_core::Clip::Audio(clip) => clip.source,
            _ => panic!("audio clip"),
        };
        assert!(source_after.start > source_before.start);
        assert!(source_after.duration < source_before.duration);
        vm.apply(Intent::Undo(0.0));
        assert_eq!(vm.project, before_clip);

        let note = vm.current_composition().tracks[1].clips[0].kind.clone();
        let ClipKind::Event { notes } = note else {
            panic!("event clip");
        };
        let note = notes[0];
        let before_notes = vm.project.clone();
        vm.apply(Intent::EditNote {
            track: 1,
            clip: 0,
            event_index: note.event_index,
            start: 0.75,
            length: 0.5,
            pitch: 73,
            velocity: 41,
        });
        assert!(vm.project.event_data.iter().flat_map(|data| &data.events).any(|event| {
            matches!(event, gaw_core::Event::Note(note) if note.note.value() == 73 && note.velocity.value() == 41 && (note.duration.value() - 0.5).abs() < f64::EPSILON)
        }));
        vm.apply(Intent::Undo(0.0));
        assert_eq!(vm.project, before_notes);

        let before_sampler = vm.project.clone();
        let mut zone = vm.current_composition().tracks[1].sampler_zones[0].clone();
        zone.name = "Fully edited".into();
        zone.source_start_seconds = 0.01;
        zone.source_duration_seconds = 0.2;
        zone.root_note = 64;
        zone.low_note = 48;
        zone.high_note = 72;
        zone.low_velocity = 12;
        zone.high_velocity = 111;
        zone.gain_db = -3.0;
        zone.velocity_sensitivity = 0.35;
        zone.attack_ms = 8.0;
        zone.release_ms = 240.0;
        zone.one_shot = false;
        zone.reverse = true;
        zone.choke_group = Some(7);
        vm.update_sampler_zone(1, 0, &zone);
        vm.update_sampler_settings(1, 24, "quietest", -2.0);
        let track_id = vm.current_track_id(1).expect("sampler track");
        let sampler = vm
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.as_ref())
            .map(|instrument| match &instrument.kind {
                gaw_core::InstrumentKind::Sampler(sampler) => sampler,
            })
            .expect("sampler");
        let core_zone = &sampler.zones[0];
        assert_eq!(core_zone.name, "Fully edited");
        assert_eq!(core_zone.root_note.value(), 64);
        assert_eq!(
            core_zone.note_range,
            gaw_core::NoteRange::new(48, 72).expect("range")
        );
        assert_eq!(
            core_zone.velocity_range,
            gaw_core::VelocityRange::new(12, 111).expect("range")
        );
        assert_eq!(core_zone.playback, gaw_core::SamplerPlayback::NoteGated);
        assert_eq!(core_zone.choke_group, Some(7));
        assert_eq!(sampler.polyphony, 24);
        assert_eq!(sampler.voice_stealing, gaw_core::VoiceStealing::Quietest);
        vm.apply(Intent::Undo(0.0));
        vm.apply(Intent::Undo(0.0));
        assert_eq!(vm.project, before_sampler);
    }

    #[test]
    fn full_processor_catalog_and_typed_parameters_round_trip() {
        use std::collections::HashSet;

        let catalog = DemoViewModel::processor_catalog();
        assert_eq!(catalog.len(), 27);
        assert_eq!(
            catalog
                .iter()
                .map(|(type_id, _)| type_id)
                .collect::<HashSet<_>>()
                .len(),
            catalog.len()
        );
        let cases = [
            ("gaw.filter", "cutoff_hz", serde_json::json!(4321.0)),
            (
                "gaw.beat_repeat",
                "seed",
                serde_json::json!(9_007_199_254_740_993_u64),
            ),
            ("gaw.stereo_tool", "swap_channels", serde_json::json!(true)),
            ("gaw.gain", "pan_law", serde_json::json!("minus_six_db")),
            (
                "gaw.delay",
                "time",
                serde_json::json!({"unit":"seconds","value":0.375}),
            ),
            (
                "gaw.chorus",
                "rate",
                serde_json::json!({"unit":"beats","value":0.5}),
            ),
            (
                "gaw.parametric_eq",
                "bands",
                serde_json::json!([{
                    "enabled":true,"shape":"bell","frequency_hz":1200.0,"gain_db":2.0,
                    "q":f64::from(0.8_f32),"slope_db_per_octave":"db12"
                }]),
            ),
        ];
        for (type_id, parameter_id, value) in cases {
            let mut vm = DemoViewModel::demo();
            let stack = ProcessorStack::CompositionOutput {
                composition_id: vm.current_composition_id(),
            };
            let insertion_index = processor_stack(&vm.project, &stack)
                .expect("output stack")
                .len();
            let catalog_index = catalog
                .iter()
                .position(|(candidate, _)| candidate == type_id)
                .expect("catalog entry");
            vm.insert_processor(stack.clone(), catalog_index);
            vm.select_processor_at(stack.clone(), insertion_index);
            let parameter_index = vm
                .selected_processor_view()
                .expect("processor view")
                .parameters
                .iter()
                .position(|parameter| parameter.id == parameter_id)
                .expect("root parameter");
            vm.set_selected_processor_parameter(parameter_index, value.clone());
            let processor =
                &processor_stack(&vm.project, &stack).expect("output stack")[insertion_index];
            let encoded = serde_json::to_value(processor).expect("processor json");
            assert_eq!(
                encoded["parameters"][parameter_id], value,
                "{type_id}.{parameter_id}"
            );
            assert!(vm.last_error().is_none());
        }
    }

    #[test]
    fn projection_reuses_waveforms_and_tracks_maximum_clip_duration() {
        let mut vm = DemoViewModel::demo();
        let asset_waveform = Arc::clone(&vm.assets[0].waveform);
        let clip_waveform = Arc::clone(&vm.current_composition().tracks[0].clips[0].waveform);
        vm.apply(Intent::SetBpm(121.0));
        assert!(Arc::ptr_eq(&asset_waveform, &vm.assets[0].waveform));
        assert!(Arc::ptr_eq(
            &clip_waveform,
            &vm.current_composition().tracks[0].clips[0].waveform
        ));
        for track in &vm.current_composition().tracks {
            let expected = track
                .clips
                .iter()
                .map(|clip| {
                    clip.length
                        + match clip.kind {
                            ClipKind::Composition { tail_beats, .. } => tail_beats,
                            _ => 0.0,
                        }
                })
                .fold(0.0, f32::max);
            assert!((track.max_visual_length - expected).abs() < f32::EPSILON);
        }
    }
}
