#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    sync::{Arc, OnceLock},
};

use gaw_core::{
    AssetFolder, AssetFolderId, AssetId, ClipId, Command, CompositionId, EditHistory, EventDataId,
    ProcessorId, ProcessorStack, Project, TrackGroup, TrackGroupId, TrackId, Transaction,
};

// UI state is a projection; all musical edits commit canonical transactions.
mod assets;
mod clips;
mod demo;
mod equalizer;
mod midi_recording;
mod projection;
mod sampler;

pub use demo::demo_project;
pub(crate) use midi_recording::{MidiRecordingTarget, RecordedMidiNote};
use projection::audio_clip_waveform;
pub(crate) use projection::effect_view;
use projection::{adapt_midi_assets, adapt_project};

pub const MIN_BPM: f32 = 40.0;
pub const MAX_BPM: f32 = 240.0;
pub const HIGHLIGHT_SECONDS: f64 = 2.4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    None,
    Repitch,
    Stretch,
}

fn processor_name(type_id: &str) -> String {
    match type_id {
        "gaw.parametric_eq" => "Parametric EQ".into(),
        "gaw.pitch_shift" => "Pitch Shift".into(),
        "gaw.saturator" => "Distortion".into(),
        "gaw.bitcrusher" => "Bitcrusher".into(),
        _ => type_id.trim_start_matches("gaw.").replace('_', " "),
    }
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

fn asset_timeline_duration(
    asset: &gaw_core::AudioAsset,
    project: &Project,
    tempo_sync: gaw_core::TempoSync,
) -> f64 {
    let source_seconds = asset_duration(asset).unwrap_or(1.0).max(0.001);
    let timeline_bpm = if tempo_sync == gaw_core::TempoSync::None {
        project.bpm.value()
    } else {
        asset
            .tempo
            .map_or(project.bpm.value(), |tempo| tempo.bpm.value())
    };
    (source_seconds * timeline_bpm / 60.0).max(0.001)
}

fn extend_composition_for_drop(
    composition: &gaw_core::Composition,
    start: f64,
    duration: f64,
    bar_length: f64,
    commands: &mut Vec<Command>,
) {
    let required_end = start + duration;
    let extended_length = if composition.length.value() <= f64::EPSILON {
        required_end.max(gaw_core::DEFAULT_COMPOSITION_LENGTH_BEATS)
    } else {
        required_end.max(composition.length.value())
    };
    let extended_length = (extended_length / bar_length).ceil() * bar_length;
    if extended_length > composition.length.value() {
        let mut extended = composition.clone();
        extended.length = gaw_core::Beats::new(extended_length).expect("finite positive length");
        commands.push(Command::UpdateComposition {
            composition: extended,
        });
    }
}

fn clip_duration(clip: &gaw_core::Clip) -> f64 {
    match clip {
        gaw_core::Clip::Audio(clip) => clip.duration.value(),
        gaw_core::Clip::Event(clip) => clip.duration.value(),
        gaw_core::Clip::Composition(clip) => clip.duration.value(),
    }
}

fn set_clip_start(clip: &mut gaw_core::Clip, start: f64) {
    let start = gaw_core::Beats::new(start).expect("packed clip start is valid");
    match clip {
        gaw_core::Clip::Audio(clip) => clip.start = start,
        gaw_core::Clip::Event(clip) => clip.start = start,
        gaw_core::Clip::Composition(clip) => clip.start = start,
    }
}

fn clip_is_compatible_with_track(clip: &gaw_core::Clip, track_kind: gaw_core::TrackKind) -> bool {
    matches!(
        (clip, track_kind),
        (gaw_core::Clip::Event(_), gaw_core::TrackKind::Event)
            | (
                gaw_core::Clip::Audio(_) | gaw_core::Clip::Composition(_),
                gaw_core::TrackKind::Audio
            )
    )
}

fn clip_dependencies_exist(project: &Project, clip: &gaw_core::Clip) -> bool {
    match clip {
        gaw_core::Clip::Audio(clip) => project.assets.iter().any(|asset| asset.id == clip.asset_id),
        gaw_core::Clip::Event(clip) => project
            .event_data
            .iter()
            .any(|data| data.id == clip.event_data_id),
        gaw_core::Clip::Composition(clip) => project
            .compositions
            .iter()
            .any(|composition| composition.id == clip.composition_id),
    }
}

fn is_clip_automation_target(
    target: &gaw_core::AutomationTarget,
    track_id: TrackId,
    clip_id: ClipId,
) -> bool {
    matches!(
        target,
        gaw_core::AutomationTarget::AudioClipProcessor {
            track_id: target_track,
            clip_id: target_clip,
            ..
        } | gaw_core::AutomationTarget::CompositionClipProcessor {
            track_id: target_track,
            clip_id: target_clip,
            ..
        } if *target_track == track_id && *target_clip == clip_id
    )
}

fn fresh_clip_identity(
    mut clip: gaw_core::Clip,
) -> (gaw_core::Clip, HashMap<ProcessorId, ProcessorId>) {
    let processors = match &mut clip {
        gaw_core::Clip::Audio(clip) => {
            clip.id = ClipId::new();
            clip.effects.as_mut_slice()
        }
        gaw_core::Clip::Event(clip) => {
            clip.id = ClipId::new();
            clip.effects.as_mut_slice()
        }
        gaw_core::Clip::Composition(clip) => {
            clip.id = ClipId::new();
            clip.effects.as_mut_slice()
        }
    };
    let processor_ids = processors
        .iter_mut()
        .map(|processor| {
            let old = processor.id.clone();
            processor.id = ProcessorId::new(format!("clip-fx-{}", ClipId::new()))
                .expect("UUID-backed processor ID is valid");
            (old, processor.id.clone())
        })
        .collect();
    (clip, processor_ids)
}

fn clone_clip_automation(
    automation: Vec<gaw_core::AutomationLane>,
    processor_ids: &HashMap<ProcessorId, ProcessorId>,
    composition_id: CompositionId,
    track_id: TrackId,
    clip_id: ClipId,
    time_delta: f64,
) -> Option<Vec<gaw_core::AutomationLane>> {
    automation
        .into_iter()
        .map(|mut lane| {
            lane.id = gaw_core::AutomationLaneId::new();
            lane.composition_id = composition_id;
            match &mut lane.target {
                gaw_core::AutomationTarget::AudioClipProcessor {
                    track_id: target_track,
                    clip_id: target_clip,
                    processor_id,
                    ..
                }
                | gaw_core::AutomationTarget::CompositionClipProcessor {
                    track_id: target_track,
                    clip_id: target_clip,
                    processor_id,
                    ..
                } => {
                    *target_track = track_id;
                    *target_clip = clip_id;
                    *processor_id = processor_ids.get(processor_id)?.clone();
                }
                gaw_core::AutomationTarget::TrackProcessor { .. }
                | gaw_core::AutomationTarget::CompositionOutputProcessor { .. }
                | gaw_core::AutomationTarget::Instrument { .. } => return None,
            }
            lane.points = shifted_automation_points(&lane, time_delta)?;
            Some(lane)
        })
        .collect()
}

fn shifted_automation_points(
    lane: &gaw_core::AutomationLane,
    time_delta: f64,
) -> Option<Vec<gaw_core::AutomationPoint>> {
    let mut shifted = Vec::with_capacity(lane.points.len() + 1);
    let mut cropped = false;
    let mut has_zero = false;
    for point in &lane.points {
        let time = point.time.value() + time_delta;
        if time < -f64::EPSILON {
            cropped = true;
            continue;
        }
        let time = if time.abs() <= f64::EPSILON {
            has_zero = true;
            0.0
        } else {
            time
        };
        let mut point = *point;
        point.time = gaw_core::Beats::new(time).ok()?;
        shifted.push(point);
    }
    if cropped && !has_zero {
        let source_time = gaw_core::Beats::new((-time_delta).max(0.0)).ok()?;
        let value = lane.value_at(source_time)?;
        let curve = lane
            .points
            .iter()
            .rev()
            .find(|point| point.time <= source_time)
            .or_else(|| lane.points.first())?
            .curve;
        shifted.insert(
            0,
            gaw_core::AutomationPoint {
                time: gaw_core::Beats::new(0.0).expect("zero is valid"),
                value,
                curve,
            },
        );
    }
    (!shifted.is_empty()).then_some(shifted)
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
                gaw_core::Clip::Event(clip) => Some(clip.effects.as_slice()),
                gaw_core::Clip::Composition(_) => None,
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

pub(crate) fn set_parameter(
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
    pub cents: f64,
    pub event_index: usize,
    pub start: f32,
    pub length: f32,
    pub pitch: u8,
    pub velocity: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteInsert {
    pub cents: f64,
    pub start: f32,
    pub length: f32,
    pub pitch: u8,
    pub velocity: u8,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteUpdate {
    pub event_index: usize,
    pub start: f32,
    pub length: f32,
    pub pitch: u8,
    pub velocity: u8,
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

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WaveformPoint {
    pub minimum: f32,
    pub maximum: f32,
}

#[derive(Clone, Debug)]
pub struct Clip {
    pub id: String,
    pub name: String,
    pub start: f32,
    pub length: f32,
    pub gain_db: f32,
    pub waveform: Arc<[WaveformPoint]>,
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
    pub volume_db: f32,
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
    pub track_groups: Vec<TrackGroup>,
    pub bar_timeline_gaps: Vec<BarTimelineGap>,
    pub output_effects: Vec<Effect>,
    pub structure_path: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BarTimelineGap {
    pub start: f32,
    pub duration: f32,
}

impl BarTimelineGap {
    pub fn end(self) -> f32 {
        self.start + self.duration
    }
}

impl Composition {
    /// Musical beat position after removing elapsed portions of counting gaps.
    pub fn counted_beat_at(&self, timeline_beat: f32) -> f32 {
        timeline_beat
            - self
                .bar_timeline_gaps
                .iter()
                .map(|gap| (timeline_beat - gap.start).clamp(0.0, gap.duration))
                .sum::<f32>()
    }
}

fn selected_clip_move_delta(
    tracks: &[Track],
    selected_ids: &BTreeSet<String>,
    composition_length: f32,
    requested_delta: f32,
) -> f32 {
    let selected = tracks
        .iter()
        .enumerate()
        .flat_map(|(track_index, track)| {
            track.clips.iter().filter_map(move |clip| {
                selected_ids
                    .contains(&clip.id)
                    .then_some((track_index, clip.start, clip.end()))
            })
        })
        .collect::<Vec<_>>();
    let Some(min_start) = selected.iter().map(|(_, start, _)| *start).reduce(f32::min) else {
        return 0.0;
    };
    let max_end = selected
        .iter()
        .map(|(_, _, end)| *end)
        .reduce(f32::max)
        .unwrap_or(min_start);
    let minimum = -min_start;
    let maximum = (composition_length - max_end).max(minimum);
    let requested = requested_delta.clamp(minimum, maximum);
    let forbidden = selected
        .iter()
        .flat_map(|(track_index, selected_start, selected_end)| {
            tracks[*track_index]
                .clips
                .iter()
                .filter(|clip| !selected_ids.contains(&clip.id))
                .map(move |clip| (clip.start - selected_end, clip.end() - selected_start))
        })
        .filter(|(start, end)| *start < maximum && *end > minimum)
        .collect::<Vec<_>>();
    let allowed = |delta: f32| {
        !forbidden
            .iter()
            .any(|(start, end)| delta > *start && delta < *end)
    };
    if allowed(requested) {
        return requested;
    }

    std::iter::once(minimum)
        .chain(std::iter::once(maximum))
        .chain(
            forbidden
                .iter()
                .flat_map(|(start, end)| [*start, *end])
                .map(|delta| delta.clamp(minimum, maximum)),
        )
        .filter(|delta| allowed(*delta))
        .min_by(|left, right| {
            (left - requested)
                .abs()
                .total_cmp(&(right - requested).abs())
                .then_with(|| left.total_cmp(right))
        })
        .unwrap_or(0.0)
}

#[derive(Clone, Debug)]
pub struct Asset {
    pub id: String,
    pub name: String,
    pub duration_seconds: f32,
    pub channels: u8,
    pub bpm: Option<f32>,
    pub first_beat_seconds: Option<f32>,
    pub waveform: Arc<[WaveformPoint]>,
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

#[derive(Clone, Debug)]
pub struct MidiAsset {
    pub id: String,
    pub name: String,
    pub note_count: usize,
    pub duration_beats: f32,
    pub structure_path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    None,
    Asset(usize),
    MidiAsset(usize),
    Track {
        track: usize,
    },
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
#[allow(clippy::struct_excessive_bools)]
pub struct Transport {
    pub playing: bool,
    pub recording: bool,
    pub loop_enabled: bool,
    pub loop_start: f32,
    pub loop_end: f32,
    pub playhead: f32,
    pub bpm: f32,
    pub time_signature: gaw_core::TimeSignature,
    pub metronome_enabled: bool,
    pub metronome_gain: f32,
    pub master_volume_db: f32,
    /// Smoothed, post-master output peak in the normalized range 0..=1.
    pub master_level: f32,
}

#[derive(Clone, Debug)]
struct Highlight {
    entity_id: String,
    changed_at: f64,
}

#[derive(Clone, Debug)]
pub enum Intent {
    TogglePlayback,
    ToggleRecording,
    Stop,
    DeleteLoop,
    Seek(f32),
    SetLoopRange {
        start: f32,
        end: f32,
    },
    AddBarTimelineGap {
        start: f32,
        duration: f32,
    },
    UpdateBarTimelineGap {
        index: usize,
        start: f32,
        duration: f32,
    },
    DeleteBarTimelineGap {
        index: usize,
    },
    EditClip {
        track: usize,
        clip: usize,
        start: f32,
        length: f32,
        target_track: usize,
    },
    MoveSelectedClips {
        delta: f32,
    },
    DeleteSelectedClips,
    DeleteClip {
        track: usize,
        clip: usize,
    },
    CopyClip {
        track: usize,
        clip: usize,
    },
    CutClip {
        track: usize,
        clip: usize,
    },
    DuplicateClip {
        track: usize,
        clip: usize,
    },
    PasteClip {
        track: Option<usize>,
        beat: f32,
    },
    RenameClip {
        track: usize,
        clip: usize,
        name: String,
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
    AddNotes {
        track: usize,
        clip: usize,
        notes: Vec<NoteInsert>,
    },
    EditNotes {
        track: usize,
        clip: usize,
        notes: Vec<NoteUpdate>,
    },
    DeleteNotes {
        track: usize,
        clip: usize,
        event_indices: Vec<usize>,
    },
    SetBpm(f32),
    SetProjectSampleRate(u32),
    SetTimeSignature {
        numerator: u8,
        denominator: u8,
    },
    SetMetronomeGain(f32),
    SetMasterVolume(f32),
    ToggleMetronome,
    Select(Selection),
    SelectClips(Vec<String>),
    ToggleClipSelection {
        track: usize,
        clip: usize,
    },
    ToggleAssetSelection(Selection),
    ClearSelection,
    EnterChild {
        track: usize,
        clip: usize,
    },
    NavigateToDepth(usize),
    Back,
    ToggleMute(usize),
    ToggleSolo(usize),
    SetTrackVolume {
        track: usize,
        volume_db: f32,
    },
    RenameTrack {
        track: usize,
        name: String,
    },
    DeleteTrack {
        track: usize,
    },
    ReorderTrack {
        from: usize,
        to: usize,
    },
    CreateTrackGroup {
        track: Option<usize>,
        name: String,
    },
    ToggleTrackGroup {
        group_id: TrackGroupId,
    },
    DeleteTrackGroup {
        group_id: TrackGroupId,
    },
    MoveTrackToGroup {
        track: usize,
        group_id: Option<TrackGroupId>,
    },
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
        asset_id: AssetId,
        beat: f32,
        track: Option<usize>,
        tempo_sync: Option<gaw_core::TempoSync>,
    },
    AddEventDataClip {
        event_data_id: EventDataId,
        beat: f32,
        track: Option<usize>,
    },
    CreateMidiAsset,
    CreateMidiClip {
        beat: f32,
        track: usize,
    },
    CreateMidiTrack {
        beat: f32,
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
        cents: f64,
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
    pub audio_render_changed: bool,
    /// The delta-sized canonical transaction for forward edits. Undo/redo updates carry `None`.
    pub transaction: Option<Arc<Transaction>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StableSelection {
    None,
    Asset(AssetId),
    MidiAsset(EventDataId),
    Track(TrackId),
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
struct ClipClipboard {
    clip: gaw_core::Clip,
    automation: Vec<gaw_core::AutomationLane>,
    source_composition_id: CompositionId,
    source_track_id: TrackId,
}

#[derive(Clone, Debug)]
pub struct ProjectViewModel {
    project: Project,
    shared_project: OnceLock<Arc<Project>>,
    engine: CommandEngine,
    pub compositions: Vec<Composition>,
    pub assets: Vec<Asset>,
    pub midi_assets: Vec<MidiAsset>,
    pub transport: Transport,
    pub selection: Selection,
    selected_clip_ids: BTreeSet<String>,
    selected_asset_ids: BTreeSet<String>,
    clip_clipboard: Option<ClipClipboard>,
    scoped_effect: Option<(ProcessorStack, ProcessorId)>,
    signal_scope: Option<ProcessorStack>,
    signal_context: Option<StableSelection>,
    eq_edit_gesture: Option<equalizer::EqEditGesture>,
    pub structure_lens: bool,
    nav_path: Vec<CompositionId>,
    highlights: Vec<Highlight>,
    updates: VecDeque<ProjectUpdate>,
    last_error: Option<String>,
}

impl Default for ProjectViewModel {
    fn default() -> Self {
        Self::demo()
    }
}

impl ProjectViewModel {
    /// Creates the non-persistent demo projection.
    ///
    /// # Panics
    /// Panics if the bundled fixture violates the canonical model.
    pub fn demo() -> Self {
        let mut vm = Self::from_project(demo_project()).expect("demo project is valid");
        vm.initialize_demo_waveforms();
        vm
    }

    /// Projects a validated canonical snapshot into UI state.
    ///
    /// # Errors
    /// Returns a domain error without creating a view model if validation fails.
    pub fn from_project(project: Project) -> Result<Self, gaw_core::DomainError> {
        use gaw_core::Validate as _;
        project.validate()?;
        let (assets, compositions) = adapt_project(&project, None, None);
        let midi_assets = adapt_midi_assets(&project);
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
                time_signature: project.time_signature,
                metronome_enabled: project.settings.metronome_enabled,
                metronome_gain: project.settings.metronome_gain.value() as f32,
                master_volume_db: project.settings.master_volume.value() as f32,
                master_level: 0.0,
            },
            project,
            shared_project: OnceLock::new(),
            engine: CommandEngine::default(),
            compositions,
            assets,
            midi_assets,
            selection: Selection::None,
            selected_clip_ids: BTreeSet::new(),
            selected_asset_ids: BTreeSet::new(),
            clip_clipboard: None,
            scoped_effect: None,
            signal_scope: None,
            signal_context: None,
            eq_edit_gesture: None,
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

    /// Share one immutable canonical snapshot across background requests until the next edit.
    pub(crate) fn project_snapshot(&self) -> Arc<Project> {
        Arc::clone(
            self.shared_project
                .get_or_init(|| Arc::new(self.project.clone())),
        )
    }

    pub fn revision(&self) -> u64 {
        self.engine.revision
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub(crate) fn install_asset_waveform(
        &mut self,
        asset_id: &str,
        content_hash: &str,
        waveform: Arc<[WaveformPoint]>,
    ) {
        let Some(asset) = self.assets.iter_mut().find(|asset| {
            asset.id == asset_id && asset.content_hash.as_deref() == Some(content_hash)
        }) else {
            return;
        };
        asset.waveform = waveform;
        let source_waveform = &asset.waveform;
        let Ok(source_id) = asset_id.parse::<AssetId>() else {
            return;
        };
        let Some(source) = self
            .project
            .assets
            .iter()
            .find(|asset| asset.id == source_id)
        else {
            return;
        };
        // A waveform completion changes display data only. Rebuild the placements
        // of this source without reprojecting notes, processors, or unrelated clips.
        let mut updated = self
            .project
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .filter_map(|clip| match clip {
                gaw_core::Clip::Audio(clip) if clip.asset_id == source_id => Some((
                    clip.id.to_string(),
                    audio_clip_waveform(&self.project, source, clip, source_waveform),
                )),
                _ => None,
            })
            .collect::<HashMap<_, _>>();
        for clip in self
            .compositions
            .iter_mut()
            .flat_map(|composition| &mut composition.tracks)
            .flat_map(|track| &mut track.clips)
        {
            if let Some(waveform) = updated.remove(&clip.id) {
                clip.waveform = waveform;
            }
        }
    }

    pub fn take_updates(&mut self) -> impl Iterator<Item = ProjectUpdate> + '_ {
        self.updates.drain(..)
    }

    /// Applies one atomic, undoable agent edit through the canonical command engine.
    ///
    /// # Errors
    /// Returns a command or validation error without changing the project.
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
    /// # Errors
    /// Returns a validation error without replacing the current project.
    pub fn replace_project_from_agent(
        &mut self,
        project: Project,
        changed_ids: impl IntoIterator<Item = String>,
        now: f64,
    ) -> Result<(), gaw_core::DomainError> {
        let selection = self.stable_selection();
        let changed_ids = changed_ids.into_iter().collect::<Vec<_>>();
        let asset_waveforms = self.cached_asset_waveforms(&project);
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
        replacement.signal_scope.clone_from(&self.signal_scope);
        replacement.signal_context.clone_from(&self.signal_context);
        replacement.restore_selection(&selection);
        replacement
            .selected_clip_ids
            .clone_from(&self.selected_clip_ids);
        replacement
            .selected_asset_ids
            .clone_from(&self.selected_asset_ids);
        replacement.retain_valid_asset_selections();
        replacement.clip_clipboard.clone_from(&self.clip_clipboard);
        replacement.retain_valid_clip_selections();
        replacement.transport = self.transport.clone();
        replacement.sync_project_transport();
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
            true,
        );
        *self = replacement;
        Ok(())
    }

    /// Records a transaction that the project worker has already committed.
    ///
    /// This keeps GUI undo/redo exact without publishing the same transaction
    /// back to persistence a second time.
    pub(crate) fn accept_persisted_transaction(
        &mut self,
        transaction: &Transaction,
        expected_project: &Project,
        selected_asset: AssetId,
    ) -> Result<(), gaw_core::DomainError> {
        let mut preview = self.project.clone();
        transaction.apply(&mut preview)?;
        if preview != *expected_project {
            return Err(gaw_core::DomainError::Invalid {
                field: "persisted_transaction",
                message: "committed project does not match the expected GUI transition".into(),
            });
        }
        self.engine.history.apply(&mut self.project, transaction)?;
        self.engine.revision = self.engine.revision.saturating_add(1);
        self.last_error = None;
        self.refresh_projection(&StableSelection::Asset(selected_asset));
        Ok(())
    }

    /// Merges an already-persisted stem group into a GUI project that may have
    /// accumulated newer, unrelated edits while inference was running.
    pub(crate) fn accept_persisted_stem_split(
        &mut self,
        persisted_transaction: &Transaction,
        persisted_project: &Project,
        asset_ids: &[AssetId],
        selected_asset: AssetId,
    ) -> Result<(), gaw_core::DomainError> {
        let assets = asset_ids
            .iter()
            .map(|asset_id| {
                persisted_project
                    .assets
                    .iter()
                    .find(|asset| asset.id == *asset_id)
                    .cloned()
                    .ok_or_else(|| gaw_core::DomainError::NotFound {
                        entity: "persisted stem asset",
                        id: asset_id.to_string(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let folder = persisted_project
            .asset_folders
            .iter()
            .find(|folder| asset_ids.iter().all(|id| folder.asset_ids.contains(id)))
            .cloned()
            .ok_or_else(|| gaw_core::DomainError::Invalid {
                field: "persisted_stem_folder",
                message: "committed stems are not grouped in one folder".into(),
            })?;
        let mut folders = self.project.asset_folders.clone();
        folders.push(folder);
        let mut commands = assets
            .into_iter()
            .map(|asset| Command::AddAsset { asset })
            .collect::<Vec<_>>();
        commands.push(Command::SetAssetFolders { folders });
        let transaction = Transaction {
            label: persisted_transaction.label.clone(),
            commands,
        };
        self.engine.history.apply(&mut self.project, &transaction)?;
        self.engine.revision = self.engine.revision.saturating_add(1);
        self.last_error = None;
        self.refresh_projection(&StableSelection::Asset(selected_asset));
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
            Selection::MidiAsset(index) => self
                .project
                .event_data
                .get(index)
                .map_or(StableSelection::None, |data| {
                    StableSelection::MidiAsset(data.id)
                }),
            Selection::Track { track } => self
                .current_track_id(track)
                .map_or(StableSelection::None, StableSelection::Track),
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

    pub fn is_clip_selected(&self, clip_id: &str) -> bool {
        self.selected_clip_ids.contains(clip_id)
    }

    pub fn selected_clip_count(&self) -> usize {
        self.selected_clip_ids.len()
    }

    pub fn is_audio_asset_selected(&self, index: usize) -> bool {
        self.assets.get(index).is_some_and(|asset| {
            self.selected_asset_ids
                .contains(&format!("audio:{}", asset.id))
        })
    }

    pub fn is_midi_asset_selected(&self, index: usize) -> bool {
        self.midi_assets.get(index).is_some_and(|asset| {
            self.selected_asset_ids
                .contains(&format!("midi:{}", asset.id))
        })
    }

    pub fn asset_action_indices(&self, clicked: Selection) -> (Vec<usize>, Vec<usize>) {
        let clicked_is_selected = self
            .asset_selection_key(clicked)
            .is_some_and(|key| self.selected_asset_ids.contains(&key))
            || (self.selected_asset_ids.is_empty() && self.selection == clicked);
        if clicked_is_selected {
            return self.selected_asset_indices();
        }
        match clicked {
            Selection::Asset(index) if index < self.assets.len() => (vec![index], Vec::new()),
            Selection::MidiAsset(index) if index < self.midi_assets.len() => {
                (Vec::new(), vec![index])
            }
            _ => (Vec::new(), Vec::new()),
        }
    }

    fn selected_asset_indices(&self) -> (Vec<usize>, Vec<usize>) {
        if self.selected_asset_ids.is_empty() {
            return match self.selection {
                Selection::Asset(index) if index < self.assets.len() => (vec![index], Vec::new()),
                Selection::MidiAsset(index) if index < self.midi_assets.len() => {
                    (Vec::new(), vec![index])
                }
                _ => (Vec::new(), Vec::new()),
            };
        }
        let audio = self
            .assets
            .iter()
            .enumerate()
            .filter_map(|(index, asset)| {
                self.selected_asset_ids
                    .contains(&format!("audio:{}", asset.id))
                    .then_some(index)
            })
            .collect();
        let midi = self
            .midi_assets
            .iter()
            .enumerate()
            .filter_map(|(index, asset)| {
                self.selected_asset_ids
                    .contains(&format!("midi:{}", asset.id))
                    .then_some(index)
            })
            .collect();
        (audio, midi)
    }

    pub fn has_clip_clipboard(&self) -> bool {
        self.clip_clipboard.is_some()
    }

    pub fn can_paste_clip_to(&self, track: usize) -> bool {
        let Some(clipboard) = &self.clip_clipboard else {
            return false;
        };
        let Some(track_id) = self.current_track_id(track) else {
            return false;
        };
        let Some(track) = self
            .project
            .tracks
            .iter()
            .find(|candidate| candidate.id == track_id)
        else {
            return false;
        };
        clip_is_compatible_with_track(&clipboard.clip, track.kind)
            && clip_dependencies_exist(&self.project, &clipboard.clip)
            && (!matches!(&clipboard.clip, gaw_core::Clip::Composition(_))
                || clipboard.source_composition_id == self.current_composition_id())
    }

    pub fn selected_clip_move_delta(&self, requested_delta: f32) -> f32 {
        selected_clip_move_delta(
            &self.current_composition().tracks,
            &self.selected_clip_ids,
            self.current_composition().length_beats,
            requested_delta,
        )
    }

    fn select_clips(&mut self, clip_ids: Vec<String>) {
        let requested = clip_ids.into_iter().collect::<BTreeSet<_>>();
        let composition = self.current_composition();
        let selected = composition
            .tracks
            .iter()
            .enumerate()
            .flat_map(|(track_index, track)| {
                track
                    .clips
                    .iter()
                    .enumerate()
                    .map(move |(clip_index, clip)| (track_index, clip_index, clip))
            })
            .filter(|(_, _, clip)| requested.contains(&clip.id))
            .map(|(track, clip, value)| (value.id.clone(), Selection::Clip { track, clip }))
            .collect::<Vec<_>>();
        self.selected_clip_ids = selected.iter().map(|(id, _)| id.clone()).collect();
        self.selection = selected
            .first()
            .map_or(Selection::None, |(_, selection)| *selection);
        self.selected_asset_ids.clear();
        self.reset_signal_scope();
    }

    fn toggle_clip_selection(&mut self, track: usize, clip: usize) {
        let Some(clicked_id) = self
            .current_composition()
            .tracks
            .get(track)
            .and_then(|track| track.clips.get(clip))
            .map(|clip| clip.id.clone())
        else {
            return;
        };
        if self.selected_clip_ids.is_empty()
            && let Selection::Clip {
                track: selected_track,
                clip: selected_clip,
            }
            | Selection::Effect {
                track: selected_track,
                clip: selected_clip,
                ..
            } = self.selection
            && let Some(selected_id) = self
                .current_composition()
                .tracks
                .get(selected_track)
                .and_then(|track| track.clips.get(selected_clip))
                .map(|clip| clip.id.clone())
        {
            self.selected_clip_ids.insert(selected_id);
        }
        if !self.selected_clip_ids.insert(clicked_id.clone()) {
            self.selected_clip_ids.remove(&clicked_id);
        }
        let selected_ids = self.selected_clip_ids.iter().cloned().collect();
        self.select_clips(selected_ids);
        if self.selected_clip_ids.contains(&clicked_id) {
            self.selection = Selection::Clip { track, clip };
        }
    }

    fn asset_selection_key(&self, selection: Selection) -> Option<String> {
        match selection {
            Selection::Asset(index) => self
                .assets
                .get(index)
                .map(|asset| format!("audio:{}", asset.id)),
            Selection::MidiAsset(index) => self
                .midi_assets
                .get(index)
                .map(|asset| format!("midi:{}", asset.id)),
            _ => None,
        }
    }

    fn toggle_asset_selection(&mut self, selection: Selection) {
        let Some(clicked_key) = self.asset_selection_key(selection) else {
            return;
        };
        if self.selected_asset_ids.is_empty()
            && let Some(selected_key) = self.asset_selection_key(self.selection)
        {
            self.selected_asset_ids.insert(selected_key);
        }
        if !self.selected_asset_ids.insert(clicked_key.clone()) {
            self.selected_asset_ids.remove(&clicked_key);
        }
        if self.selected_asset_ids.contains(&clicked_key) {
            self.selection = selection;
        } else {
            self.selection = self
                .selected_asset_ids
                .iter()
                .find_map(|key| {
                    key.strip_prefix("audio:")
                        .and_then(|id| self.assets.iter().position(|asset| asset.id == id))
                        .map(Selection::Asset)
                        .or_else(|| {
                            key.strip_prefix("midi:")
                                .and_then(|id| {
                                    self.midi_assets.iter().position(|asset| asset.id == id)
                                })
                                .map(Selection::MidiAsset)
                        })
                })
                .unwrap_or(Selection::None);
        }
        self.selected_clip_ids.clear();
        self.reset_signal_scope();
    }

    fn retain_valid_asset_selections(&mut self) {
        let valid = self
            .assets
            .iter()
            .map(|asset| format!("audio:{}", asset.id))
            .chain(
                self.midi_assets
                    .iter()
                    .map(|asset| format!("midi:{}", asset.id)),
            )
            .collect::<BTreeSet<_>>();
        self.selected_asset_ids.retain(|key| valid.contains(key));
    }

    /// Returns the currently visible canonical composition projection.
    ///
    /// # Panics
    /// Panics if internal navigation references a missing composition.
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
        if let StableSelection::Effect {
            stack,
            processor_id,
        } = self.stable_selection()
            && find_processor(&self.project, &stack, &processor_id).is_some_and(|processor| {
                matches!(processor.kind, gaw_core::ProcessorKind::ParametricEq(_))
            })
        {
            return match stack {
                ProcessorStack::Clip { track_id, clip_id }
                | ProcessorStack::CompositionClip { track_id, clip_id } => self
                    .project
                    .tracks
                    .iter()
                    .find(|track| track.id == track_id)
                    .and_then(|track| track.clips.iter().find(|clip| clip.id() == clip_id))
                    .map_or(EditorKind::Overview, |clip| match clip {
                        gaw_core::Clip::Event(_) => EditorKind::PianoRoll,
                        gaw_core::Clip::Audio(_) | gaw_core::Clip::Composition(_) => {
                            EditorKind::Waveform
                        }
                    }),
                ProcessorStack::Track { .. } | ProcessorStack::CompositionOutput { .. } => {
                    EditorKind::Overview
                }
            };
        }
        if self.scoped_effect.is_some() {
            return EditorKind::Effect;
        }
        match self.selection {
            Selection::None | Selection::Track { .. } | Selection::MidiAsset(_) => {
                EditorKind::Overview
            }
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
    /// Dispatches a human editor gesture; musical edits use canonical transactions.
    ///
    /// Numeric gesture inputs must be finite. Programmatic project edits should
    /// use `apply_agent_transaction` with validated domain quantities.
    ///
    /// # Panics
    /// May panic on non-finite gesture inputs or inconsistent internal selection state.
    pub fn apply(&mut self, intent: Intent) {
        match intent {
            Intent::TogglePlayback => self.transport.playing = !self.transport.playing,
            Intent::ToggleRecording => self.transport.recording = !self.transport.recording,
            Intent::Stop => {
                self.transport.playing = false;
                self.transport.recording = false;
                self.transport.playhead = 0.0;
            }
            Intent::DeleteLoop => self.transport.loop_enabled = false,
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
            Intent::AddBarTimelineGap { start, duration } => {
                self.add_bar_timeline_gap(start, duration);
            }
            Intent::UpdateBarTimelineGap {
                index,
                start,
                duration,
            } => self.update_bar_timeline_gap(index, start, duration),
            Intent::DeleteBarTimelineGap { index } => self.delete_bar_timeline_gap(index),
            Intent::EditClip {
                track,
                clip,
                start,
                length,
                target_track,
            } => self.edit_clip_timing(track, clip, start, length, target_track),
            Intent::MoveSelectedClips { delta } => {
                self.move_selected_clips(delta);
            }
            Intent::DeleteSelectedClips => self.delete_selected_clips(),
            Intent::DeleteClip { track, clip } => self.delete_clip(track, clip),
            Intent::CopyClip { track, clip } => self.copy_clip(track, clip),
            Intent::CutClip { track, clip } => self.cut_clip(track, clip),
            Intent::DuplicateClip { track, clip } => self.duplicate_clip(track, clip),
            Intent::PasteClip { track, beat } => self.paste_clip(track, beat),
            Intent::RenameClip { track, clip, name } => self.rename_clip(track, clip, &name),
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
                    cents: 0.0,
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
            Intent::AddNotes { track, clip, notes } => self.edit_notes(
                track,
                clip,
                notes.into_iter().map(|note| NoteEdit::Add {
                    cents: note.cents,
                    start: note.start,
                    length: note.length,
                    pitch: note.pitch,
                    velocity: note.velocity,
                }),
            ),
            Intent::EditNotes { track, clip, notes } => self.edit_notes(
                track,
                clip,
                notes.into_iter().map(|note| NoteEdit::Update {
                    event_index: note.event_index,
                    start: note.start,
                    length: note.length,
                    pitch: note.pitch,
                    velocity: note.velocity,
                }),
            ),
            Intent::DeleteNotes {
                track,
                clip,
                event_indices,
            } => self.edit_notes(
                track,
                clip,
                event_indices
                    .into_iter()
                    .map(|event_index| NoteEdit::Delete { event_index }),
            ),
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
            Intent::SetProjectSampleRate(sample_rate) => {
                if let Ok(sample_rate) = gaw_core::SampleRate::new(sample_rate) {
                    let transaction = Transaction::named(
                        "Set project sample rate",
                        [Command::SetProjectSampleRate { sample_rate }],
                    );
                    self.commit_ui(&transaction, &[self.project.id.to_string()]);
                }
            }
            Intent::SetTimeSignature {
                numerator,
                denominator,
            } => {
                if let Ok(time_signature) = gaw_core::TimeSignature::new(numerator, denominator) {
                    let transaction = Transaction::named(
                        "Set project time signature",
                        [Command::SetProjectTimeSignature { time_signature }],
                    );
                    self.commit_ui(&transaction, &[self.project.id.to_string()]);
                }
            }
            Intent::SetMetronomeGain(gain) => {
                let gain = gain.clamp(0.0, 1.0);
                let mut settings = self.project.settings.clone();
                settings.metronome_gain =
                    gaw_core::Ratio::new(f64::from(gain)).expect("clamped metronome gain is valid");
                let transaction = Transaction::named(
                    "Set metronome volume",
                    [Command::SetProjectSettings { settings }],
                );
                self.commit_ui(&transaction, &[self.project.id.to_string()]);
            }
            Intent::SetMasterVolume(volume_db) => {
                let volume = gaw_core::Decibels::new(f64::from(volume_db.clamp(-120.0, 24.0)))
                    .expect("clamped master volume is valid");
                let transaction = Transaction::named(
                    "Set master volume",
                    [Command::SetProjectMasterVolume { volume }],
                );
                self.commit_ui(&transaction, &[self.project.id.to_string()]);
            }
            Intent::ToggleMetronome => {
                let transaction = Transaction::named(
                    "Toggle project metronome",
                    [Command::SetProjectMetronome {
                        enabled: !self.project.settings.metronome_enabled,
                    }],
                );
                self.commit_ui(&transaction, &[self.project.id.to_string()]);
            }
            Intent::Select(selection) => {
                self.selection = selection;
                self.selected_clip_ids.clear();
                self.selected_asset_ids.clear();
                self.reset_signal_scope();
            }
            Intent::SelectClips(clip_ids) => self.select_clips(clip_ids),
            Intent::ToggleClipSelection { track, clip } => {
                self.toggle_clip_selection(track, clip);
            }
            Intent::ToggleAssetSelection(selection) => self.toggle_asset_selection(selection),
            Intent::ClearSelection => {
                self.selection = Selection::None;
                self.selected_clip_ids.clear();
                self.selected_asset_ids.clear();
                self.reset_signal_scope();
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
                    self.reset_signal_scope();
                    self.selection = Selection::None;
                    self.selected_clip_ids.clear();
                    self.selected_asset_ids.clear();
                    self.transport.playhead = 0.0;
                }
            }
            Intent::NavigateToDepth(depth) => {
                if depth < self.nav_path.len() {
                    self.nav_path.truncate(depth + 1);
                    self.reset_signal_scope();
                    self.selection = Selection::None;
                    self.selected_clip_ids.clear();
                    self.selected_asset_ids.clear();
                    self.transport.playhead = 0.0;
                }
            }
            Intent::Back => {
                if self.nav_path.len() > 1 {
                    self.nav_path.pop();
                    self.reset_signal_scope();
                    self.selection = Selection::None;
                    self.selected_clip_ids.clear();
                    self.selected_asset_ids.clear();
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
            Intent::SetTrackVolume { track, volume_db } => {
                if let Some(track_id) = self.current_track_id(track) {
                    let transaction = Transaction::named(
                        "Set track volume",
                        [Command::SetTrackVolume {
                            track_id,
                            volume_db: volume_db.clamp(-120.0, 24.0),
                        }],
                    );
                    self.commit_ui(&transaction, &[track_id.to_string()]);
                }
            }
            Intent::RenameTrack { track, name } => {
                let name = name.trim();
                if !name.is_empty()
                    && let Some(track_id) = self.current_track_id(track)
                    && let Some(mut track) = self
                        .project
                        .tracks
                        .iter()
                        .find(|candidate| candidate.id == track_id)
                        .cloned()
                    && track.name != name
                {
                    name.clone_into(&mut track.name);
                    let transaction =
                        Transaction::named("Rename track", [Command::UpdateTrack { track }]);
                    self.commit_ui(&transaction, &[track_id.to_string()]);
                }
            }
            Intent::DeleteTrack { track } => {
                if let Some(track_id) = self.current_track_id(track)
                    && let Some(mut composition) = self
                        .project
                        .compositions
                        .iter()
                        .find(|composition| composition.id == self.current_composition_id())
                        .cloned()
                {
                    let mut commands = self
                        .project
                        .automation
                        .iter()
                        .filter(|lane| match lane.target {
                            gaw_core::AutomationTarget::AudioClipProcessor {
                                track_id: target,
                                ..
                            }
                            | gaw_core::AutomationTarget::CompositionClipProcessor {
                                track_id: target,
                                ..
                            }
                            | gaw_core::AutomationTarget::TrackProcessor {
                                track_id: target, ..
                            }
                            | gaw_core::AutomationTarget::Instrument {
                                track_id: target, ..
                            } => target == track_id,
                            gaw_core::AutomationTarget::CompositionOutputProcessor { .. } => false,
                        })
                        .map(|lane| Command::RemoveAutomation { lane_id: lane.id })
                        .collect::<Vec<_>>();
                    let old_groups = composition.track_groups.clone();
                    for group in &mut composition.track_groups {
                        group.track_ids.retain(|candidate| *candidate != track_id);
                    }
                    if composition.track_groups != old_groups {
                        commands.push(Command::UpdateComposition { composition });
                    }
                    commands.push(Command::RemoveTrack { track_id });
                    let transaction = Transaction::named("Delete track", commands);
                    self.commit_ui(&transaction, &[track_id.to_string()]);
                }
            }
            Intent::ReorderTrack { from, to } => {
                let track_count = self.current_composition().tracks.len();
                if from < track_count && to < track_count && from != to {
                    let composition_id = self.current_composition_id();
                    let changed_ids = [self.current_composition().tracks[from].id.clone()];
                    let transaction = Transaction::named(
                        "Reorder track",
                        [Command::ReorderCompositionTracks {
                            composition_id,
                            from,
                            to,
                        }],
                    );
                    self.commit_ui(&transaction, &changed_ids);
                }
            }
            Intent::CreateTrackGroup { track, name } => {
                let name = name.trim();
                let track_id = track.and_then(|track| self.current_track_id(track));
                if track.is_some() && track_id.is_none() {
                    return;
                }
                if !name.is_empty()
                    && let Some(mut composition) = self
                        .project
                        .compositions
                        .iter()
                        .find(|composition| composition.id == self.current_composition_id())
                        .cloned()
                {
                    if let Some(track_id) = track_id {
                        for group in &mut composition.track_groups {
                            group.track_ids.retain(|candidate| *candidate != track_id);
                        }
                    }
                    let group = TrackGroup {
                        id: TrackGroupId::new(),
                        name: name.to_owned(),
                        track_ids: track_id.into_iter().collect(),
                        collapsed: false,
                    };
                    let group_id = group.id;
                    composition.track_groups.push(group);
                    let transaction = Transaction::named(
                        "Create track group",
                        [Command::UpdateComposition { composition }],
                    );
                    let mut changed_ids = vec![group_id.to_string()];
                    changed_ids.extend(track_id.map(|track_id| track_id.to_string()));
                    self.commit_ui(&transaction, &changed_ids);
                }
            }
            Intent::ToggleTrackGroup { group_id } => {
                if let Some(mut composition) = self
                    .project
                    .compositions
                    .iter()
                    .find(|composition| composition.id == self.current_composition_id())
                    .cloned()
                    && let Some(group) = composition
                        .track_groups
                        .iter_mut()
                        .find(|group| group.id == group_id)
                {
                    group.collapsed = !group.collapsed;
                    let transaction = Transaction::named(
                        "Toggle track group",
                        [Command::UpdateComposition { composition }],
                    );
                    self.commit_ui(&transaction, &[group_id.to_string()]);
                }
            }
            Intent::DeleteTrackGroup { group_id } => {
                if let Some(mut composition) = self
                    .project
                    .compositions
                    .iter()
                    .find(|composition| composition.id == self.current_composition_id())
                    .cloned()
                {
                    let old_len = composition.track_groups.len();
                    composition
                        .track_groups
                        .retain(|group| group.id != group_id);
                    if composition.track_groups.len() != old_len {
                        let transaction = Transaction::named(
                            "Delete track group",
                            [Command::UpdateComposition { composition }],
                        );
                        self.commit_ui(&transaction, &[group_id.to_string()]);
                    }
                }
            }
            Intent::MoveTrackToGroup { track, group_id } => {
                if let Some(track_id) = self.current_track_id(track)
                    && let Some(mut composition) = self
                        .project
                        .compositions
                        .iter()
                        .find(|composition| composition.id == self.current_composition_id())
                        .cloned()
                    && group_id.is_none_or(|group_id| {
                        composition
                            .track_groups
                            .iter()
                            .any(|group| group.id == group_id)
                    })
                {
                    let old_groups = composition.track_groups.clone();
                    for group in &mut composition.track_groups {
                        group.track_ids.retain(|candidate| *candidate != track_id);
                    }
                    if let Some(group_id) = group_id
                        && let Some(group) = composition
                            .track_groups
                            .iter_mut()
                            .find(|group| group.id == group_id)
                    {
                        group.track_ids.push(track_id);
                    }
                    if composition.track_groups != old_groups {
                        let mut changed_ids = vec![track_id.to_string()];
                        if let Some(group_id) = group_id {
                            changed_ids.push(group_id.to_string());
                        }
                        let transaction = Transaction::named(
                            "Move track to group",
                            [Command::UpdateComposition { composition }],
                        );
                        self.commit_ui(&transaction, &changed_ids);
                    }
                }
            }
            Intent::ToggleEffect {
                track,
                clip,
                effect,
            } => {
                if let Some(stack) = self.clip_stack(track, clip) {
                    self.toggle_processor_at(stack, effect);
                }
            }
            Intent::MoveEffect {
                track,
                clip,
                effect,
                delta,
            } => {
                if let Some(target) = effect.checked_add_signed(delta)
                    && let Some(stack) = self.clip_stack(track, clip)
                {
                    let revision = self.revision();
                    self.move_processor_at(stack.clone(), effect, delta);
                    if self.revision() != revision {
                        self.select_processor_at(stack, target);
                    }
                }
            }
            Intent::AddAssetClip {
                asset_id,
                beat,
                track,
                tempo_sync,
            } => {
                self.add_asset_clip(asset_id, beat, track, tempo_sync);
            }
            Intent::AddEventDataClip {
                event_data_id,
                beat,
                track,
            } => self.add_event_data_clip(event_data_id, beat, track),
            Intent::CreateMidiAsset => self.create_midi_asset(),
            Intent::CreateMidiClip { beat, track } => {
                self.create_midi_clip(beat, Some(track));
            }
            Intent::CreateMidiTrack { beat } => self.create_midi_clip(beat, None),
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

    pub(crate) fn midi_asset_id(&self, index: usize) -> Option<EventDataId> {
        self.project.event_data.get(index).map(|data| data.id)
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
            gaw_core::Clip::Audio(_) | gaw_core::Clip::Event(_) => {
                Some(ProcessorStack::Clip { track_id, clip_id })
            }
            gaw_core::Clip::Composition(_) => {
                Some(ProcessorStack::CompositionClip { track_id, clip_id })
            }
        }
    }

    pub(crate) fn select_processor_at(&mut self, stack: ProcessorStack, index: usize) {
        if let Some(processor) =
            processor_stack(&self.project, &stack).and_then(|stack| stack.get(index))
        {
            let processor_id = processor.id.clone();
            self.end_selected_eq_edit();
            if let ProcessorStack::Clip { track_id, clip_id }
            | ProcessorStack::CompositionClip { track_id, clip_id } = &stack
            {
                self.signal_context = Some(StableSelection::Clip {
                    track_id: *track_id,
                    clip_id: *clip_id,
                });
            } else if self.signal_context.is_none() {
                self.signal_context = Some(self.arrangement_context());
            }
            self.signal_scope = Some(stack.clone());
            let selection = StableSelection::Effect {
                stack,
                processor_id,
            };
            self.selected_asset_ids.clear();
            self.restore_selection(&selection);
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
            .map(|kind| (kind.type_id().to_owned(), processor_name(kind.type_id())))
            .collect()
    }

    pub(crate) fn insert_processor(&mut self, stack: ProcessorStack, catalog_index: usize) {
        let index = processor_stack(&self.project, &stack).map_or(0, <[gaw_core::Processor]>::len);
        let Some(mut kind) = gaw_core::ProcessorKind::catalog_defaults()
            .into_iter()
            .nth(catalog_index)
        else {
            return;
        };
        if let gaw_core::ProcessorKind::PitchShift(parameters) = &mut kind {
            parameters.quality = gaw_core::PitchQuality::Signalsmith;
        }
        if let gaw_core::ProcessorKind::ParametricEq(parameters) = &mut kind {
            *parameters = self.default_eq_parameters_for_project();
        }
        let id = ProcessorId::new(format!("fx-{}", ClipId::new()))
            .expect("UUID-backed processor id is valid");
        let processor = gaw_core::Processor::new(id.clone(), kind);
        let transaction = Transaction::named(
            "Insert processor",
            [Command::InsertProcessor {
                stack: stack.clone(),
                index,
                processor,
            }],
        );
        let revision = self.revision();
        self.commit_ui(&transaction, &[id.to_string()]);
        if self.revision() != revision {
            self.select_processor_at(stack, index);
        }
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
                                suffix.starts_with('[') || suffix.starts_with('.')
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

    fn add_bar_timeline_gap(&mut self, start: f32, duration: f32) {
        let Some(composition_id) = self.nav_path.last().copied() else {
            return;
        };
        let Some(mut composition) = self
            .project
            .compositions
            .iter()
            .find(|composition| composition.id == composition_id)
            .cloned()
        else {
            return;
        };
        let Ok(start) = gaw_core::Beats::new(f64::from(start)) else {
            return;
        };
        let Ok(duration) = gaw_core::Beats::new(f64::from(duration)) else {
            return;
        };
        composition
            .bar_timeline_gaps
            .push(gaw_core::BarTimelineGap { start, duration });
        composition
            .bar_timeline_gaps
            .sort_by(|left, right| left.start.value().total_cmp(&right.start.value()));
        let changed_id = composition.id.to_string();
        self.commit_ui(
            &Transaction::named(
                "Add bar timeline gap",
                [Command::UpdateComposition { composition }],
            ),
            &[changed_id],
        );
    }

    fn update_bar_timeline_gap(&mut self, index: usize, start: f32, duration: f32) {
        let Some(composition_id) = self.nav_path.last().copied() else {
            return;
        };
        let Some(mut composition) = self
            .project
            .compositions
            .iter()
            .find(|composition| composition.id == composition_id)
            .cloned()
        else {
            return;
        };
        let Some(gap) = composition.bar_timeline_gaps.get_mut(index) else {
            return;
        };
        let (Ok(start), Ok(duration)) = (
            gaw_core::Beats::new(f64::from(start)),
            gaw_core::Beats::new(f64::from(duration)),
        ) else {
            return;
        };
        *gap = gaw_core::BarTimelineGap { start, duration };
        composition
            .bar_timeline_gaps
            .sort_by(|left, right| left.start.value().total_cmp(&right.start.value()));
        let changed_id = composition.id.to_string();
        self.commit_ui(
            &Transaction::named(
                "Resize bar timeline gap",
                [Command::UpdateComposition { composition }],
            ),
            &[changed_id],
        );
    }

    fn delete_bar_timeline_gap(&mut self, index: usize) {
        let Some(composition_id) = self.nav_path.last().copied() else {
            return;
        };
        let Some(mut composition) = self
            .project
            .compositions
            .iter()
            .find(|composition| composition.id == composition_id)
            .cloned()
        else {
            return;
        };
        if index >= composition.bar_timeline_gaps.len() {
            return;
        }
        composition.bar_timeline_gaps.remove(index);
        let changed_id = composition.id.to_string();
        self.commit_ui(
            &Transaction::named(
                "Delete bar timeline gap",
                [Command::UpdateComposition { composition }],
            ),
            &[changed_id],
        );
    }

    fn commit(
        &mut self,
        transaction: &Transaction,
        source: ChangeSource,
        changed_ids: &[String],
        now: f64,
    ) -> Result<(), gaw_core::DomainError> {
        let selection = self.stable_selection();
        self.apply_transaction_history(transaction, source)?;
        self.engine.revision += 1;
        self.last_error = None;
        self.refresh_projection(&selection);
        self.publish_update(
            source,
            transaction.label.as_deref().unwrap_or("Edit"),
            changed_ids,
            now,
            Some(transaction),
            transaction.affects_render(),
        );
        Ok(())
    }

    fn undo(&mut self, now: f64) {
        self.end_selected_eq_edit();
        let selection = self.stable_selection();
        let audio_render_changed = self.engine.history.undo_affects_render().unwrap_or(true);
        match self.engine.history.undo(&mut self.project) {
            Ok(()) => {
                self.engine.revision += 1;
                self.last_error = None;
                self.refresh_projection(&selection);
                self.publish_update(
                    ChangeSource::Undo,
                    "Undo",
                    &[],
                    now,
                    None,
                    audio_render_changed,
                );
            }
            Err(error) => self.last_error = Some(error.to_string()),
        }
    }

    fn redo(&mut self, now: f64) {
        self.end_selected_eq_edit();
        let selection = self.stable_selection();
        let audio_render_changed = self.engine.history.redo_affects_render().unwrap_or(true);
        match self.engine.history.redo(&mut self.project) {
            Ok(()) => {
                self.engine.revision += 1;
                self.last_error = None;
                self.refresh_projection(&selection);
                self.publish_update(
                    ChangeSource::Redo,
                    "Redo",
                    &[],
                    now,
                    None,
                    audio_render_changed,
                );
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
        audio_render_changed: bool,
    ) {
        if source == ChangeSource::Agent {
            self.update_agent_highlights(changed_ids, now);
        }
        self.updates.push_back(ProjectUpdate {
            revision: self.engine.revision,
            source,
            label: label.to_owned(),
            changed_ids: Arc::from(changed_ids),
            audio_render_changed,
            transaction: transaction.cloned().map(Arc::new),
        });
        if self.updates.len() > 256 {
            self.updates.pop_front();
        }
    }

    fn update_agent_highlights(&mut self, changed_ids: &[String], now: f64) {
        if changed_ids.is_empty() {
            return;
        }
        if changed_ids.len() <= 16 {
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
            return;
        }
        let mut handled: HashMap<_, _> =
            changed_ids.iter().map(|id| (id.as_str(), false)).collect();
        let mut remaining = handled.len();
        for highlight in &mut self.highlights {
            if let Some(seen) = handled.get_mut(highlight.entity_id.as_str())
                && !*seen
            {
                highlight.changed_at = now;
                *seen = true;
                remaining -= 1;
                if remaining == 0 {
                    break;
                }
            }
        }
        if remaining > 0 {
            // Append in input order, and preserve first-match behavior for duplicate IDs.
            for entity_id in changed_ids {
                let seen = handled
                    .get_mut(entity_id.as_str())
                    .expect("indexed changed ID");
                if !*seen {
                    self.highlights.push(Highlight {
                        entity_id: entity_id.clone(),
                        changed_at: now,
                    });
                    *seen = true;
                }
            }
        }
        for asset in &mut self.assets {
            if handled.contains_key(asset.id.as_str()) {
                asset.changed_by_agent = true;
            }
        }
    }

    fn cached_asset_waveforms(&self, project: &Project) -> HashMap<String, Arc<[WaveformPoint]>> {
        let previous = self
            .assets
            .iter()
            .map(|asset| (asset.id.as_str(), asset))
            .collect::<HashMap<_, _>>();
        project
            .assets
            .iter()
            .filter_map(|asset| {
                let id = asset.id.to_string();
                let hash = match &asset.definition {
                    gaw_core::AudioAssetDefinition::Imported(source) => Some(&source.content_hash),
                    _ => asset
                        .current_revision()
                        .map(|revision| &revision.content_hash),
                }
                .map(gaw_core::ContentHash::as_str);
                previous
                    .get(id.as_str())
                    .filter(|old| old.content_hash.as_deref() == hash)
                    .map(|old| (id, Arc::clone(&old.waveform)))
            })
            .collect()
    }

    fn sync_project_transport(&mut self) {
        self.transport.bpm = self.project.bpm.value() as f32;
        self.transport.time_signature = self.project.time_signature;
        self.transport.metronome_enabled = self.project.settings.metronome_enabled;
        self.transport.metronome_gain = self.project.settings.metronome_gain.value() as f32;
        self.transport.master_volume_db = self.project.settings.master_volume.value() as f32;
    }

    fn refresh_projection(&mut self, selection: &StableSelection) {
        // Every accepted canonical edit refreshes the projection. Retire its shared
        // snapshot here; jobs already holding it continue to see their original data.
        self.shared_project.take();
        let asset_waveforms = self.cached_asset_waveforms(&self.project);
        let clip_waveforms = self
            .compositions
            .iter()
            .flat_map(|composition| &composition.tracks)
            .flat_map(|track| &track.clips)
            .map(|clip| (clip.id.clone(), Arc::clone(&clip.waveform)))
            .collect::<HashMap<_, _>>();
        let track_levels = self
            .compositions
            .iter()
            .flat_map(|composition| &composition.tracks)
            .map(|track| (track.id.clone(), track.level))
            .collect::<HashMap<_, _>>();
        let (assets, mut compositions) =
            adapt_project(&self.project, Some(&asset_waveforms), Some(&clip_waveforms));
        for track in compositions
            .iter_mut()
            .flat_map(|composition| &mut composition.tracks)
        {
            if let Some(level) = track_levels.get(&track.id) {
                track.level = *level;
            }
        }
        self.assets = assets;
        self.midi_assets = adapt_midi_assets(&self.project);
        self.retain_valid_asset_selections();
        self.compositions = compositions;
        self.sync_project_transport();
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
        self.retain_valid_clip_selections();
    }

    fn retain_valid_clip_selections(&mut self) {
        if self.selected_clip_ids.is_empty() {
            return;
        }
        let valid = self
            .current_composition()
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .map(|clip| clip.id.clone())
            .collect::<BTreeSet<_>>();
        self.selected_clip_ids.retain(|id| valid.contains(id));
        if self.selection == Selection::None && self.scoped_effect.is_none() {
            self.selection = self
                .current_composition()
                .tracks
                .iter()
                .enumerate()
                .find_map(|(track, value)| {
                    value
                        .clips
                        .iter()
                        .position(|clip| self.selected_clip_ids.contains(&clip.id))
                        .map(|clip| Selection::Clip { track, clip })
                })
                .unwrap_or(Selection::None);
        }
    }

    fn restore_selection(&mut self, selection: &StableSelection) {
        self.scoped_effect = None;
        if self
            .signal_scope
            .as_ref()
            .is_some_and(|stack| processor_stack(&self.project, stack).is_none())
        {
            self.signal_scope = None;
        }
        self.selection = match selection {
            StableSelection::None => Selection::None,
            StableSelection::Asset(asset_id) => self
                .project
                .assets
                .iter()
                .position(|asset| asset.id == *asset_id)
                .map_or(Selection::None, Selection::Asset),
            StableSelection::MidiAsset(event_data_id) => self
                .project
                .event_data
                .iter()
                .position(|data| data.id == *event_data_id)
                .map_or(Selection::None, Selection::MidiAsset),
            StableSelection::Track(track_id) => {
                let id = track_id.to_string();
                self.current_composition()
                    .tracks
                    .iter()
                    .position(|track| track.id == id)
                    .map_or(Selection::None, |track| Selection::Track { track })
            }
            StableSelection::Sampler { track_id } => {
                let id = track_id.to_string();
                self.current_composition()
                    .tracks
                    .iter()
                    .position(|track| track.id == id)
                    .map_or(Selection::None, |track| Selection::Sampler { track })
            }
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
                self.signal_context
                    .as_ref()
                    .map_or(Selection::None, |context| {
                        self.selection_for_context(context)
                    })
            }
        };
    }

    fn selection_for_clip(
        &self,
        track_id: TrackId,
        clip_id: ClipId,
        processor_id: Option<&ProcessorId>,
    ) -> Selection {
        let composition = self.current_composition();
        let track_id = track_id.to_string();
        let Some(track) = composition
            .tracks
            .iter()
            .position(|track| track.id == track_id)
        else {
            return Selection::None;
        };
        let clip_id = clip_id.to_string();
        let Some(clip) = composition.tracks[track]
            .clips
            .iter()
            .position(|clip| clip.id == clip_id)
        else {
            return Selection::None;
        };
        processor_id.map_or(Selection::Clip { track, clip }, |processor_id| {
            let processor_id = processor_id.to_string();
            composition.tracks[track].clips[clip]
                .effects
                .iter()
                .position(|effect| effect.id == processor_id)
                .map_or(Selection::Clip { track, clip }, |effect| {
                    Selection::Effect {
                        track,
                        clip,
                        effect,
                    }
                })
        })
    }

    fn add_asset_clip(
        &mut self,
        asset_id: AssetId,
        beat: f32,
        requested_track: Option<usize>,
        requested_tempo_sync: Option<gaw_core::TempoSync>,
    ) {
        let Some(asset) = self
            .project
            .assets
            .iter()
            .find(|asset| asset.id == asset_id)
            .cloned()
        else {
            return;
        };
        let tempo_sync = requested_tempo_sync.unwrap_or_else(|| {
            if asset.tempo.is_some() {
                gaw_core::TempoSync::Stretch
            } else {
                gaw_core::TempoSync::None
            }
        });
        let composition_id = self.current_composition_id();
        let composition = self
            .project
            .compositions
            .iter()
            .find(|composition| composition.id == composition_id)
            .expect("current composition exists")
            .clone();
        let start = f64::from(beat.max(0.0));
        let requested_duration = asset_timeline_duration(&asset, &self.project, tempo_sync);
        let mut commands = Vec::new();
        extend_composition_for_drop(
            &composition,
            start,
            requested_duration,
            self.project.time_signature.quarter_notes_per_bar(),
            &mut commands,
        );
        let (track_id, track_index) = requested_track
            .filter(|index| {
                self.current_composition()
                    .tracks
                    .get(*index)
                    .is_some_and(|track| track.kind == TrackKind::Audio)
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
            gaw_core::Beats::new(requested_duration).expect("positive duration"),
            gaw_core::SourceRange {
                start: gaw_core::Seconds::new(0.0).expect("zero is valid"),
                duration: gaw_core::Seconds::new(source_duration).expect("positive duration"),
            },
        );
        clip.name.clone_from(&asset.name);
        clip.tempo_sync = tempo_sync;
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
            self.apply(Intent::Select(Selection::Clip {
                track: track_index,
                clip: clip_index,
            }));
        }
    }

    fn add_event_data_clip(
        &mut self,
        event_data_id: EventDataId,
        beat: f32,
        requested_track: Option<usize>,
    ) {
        let Some(event_data) = self
            .project
            .event_data
            .iter()
            .find(|data| data.id == event_data_id)
            .cloned()
        else {
            return;
        };
        let composition_id = self.current_composition_id();
        let composition = self
            .project
            .compositions
            .iter()
            .find(|composition| composition.id == composition_id)
            .expect("current composition exists")
            .clone();
        let start = f64::from(beat.max(0.0));
        let duration = event_data
            .events
            .iter()
            .map(|event| match event {
                gaw_core::Event::Note(note) => note.start.value() + note.duration.value(),
                gaw_core::Event::Control(control) => control.time.value(),
                gaw_core::Event::PitchBend(bend) => bend.time.value(),
            })
            .fold(0.0_f64, f64::max)
            .max(0.25);
        let mut commands = Vec::new();
        extend_composition_for_drop(
            &composition,
            start,
            duration,
            self.project.time_signature.quarter_notes_per_bar(),
            &mut commands,
        );
        let (track_id, track_index) = requested_track
            .filter(|index| {
                self.current_composition()
                    .tracks
                    .get(*index)
                    .is_some_and(|track| track.kind == TrackKind::Event)
            })
            .and_then(|index| self.current_track_id(index).map(|id| (id, index)))
            .unwrap_or_else(|| {
                let sampler = gaw_core::Sampler::new(32).expect("valid sampler polyphony");
                let track = gaw_core::Track::event(
                    composition_id,
                    "DROPPED MIDI",
                    gaw_core::Instrument::sampler("Sampler", sampler),
                );
                let id = track.id;
                let index = composition.track_ids.len();
                commands.push(Command::AddTrack { track, index });
                (id, index)
            });
        let mut clip = gaw_core::EventClip::new(
            event_data.id,
            gaw_core::Beats::new(start).expect("finite start"),
            gaw_core::Beats::new(duration).expect("positive duration"),
        );
        clip.name.clone_from(&event_data.name);
        let clip_id = clip.id;
        commands.push(Command::AddClip {
            track_id,
            clip: gaw_core::Clip::Event(clip),
        });
        let transaction = Transaction::named("Drop MIDI asset on timeline", commands);
        self.commit_ui(
            &transaction,
            &[
                event_data.id.to_string(),
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
            self.apply(Intent::Select(Selection::Clip {
                track: track_index,
                clip: clip_index,
            }));
        }
    }

    fn next_midi_asset_name(&self) -> String {
        (1..=self.project.event_data.len() + 1)
            .map(|number| format!("MIDI {number}"))
            .find(|candidate| {
                self.project
                    .event_data
                    .iter()
                    .all(|data| data.name != *candidate)
            })
            .expect("a unique MIDI asset name exists")
    }

    fn create_midi_asset(&mut self) {
        let event_data = gaw_core::EventData::new(self.next_midi_asset_name());
        let event_data_id = event_data.id;
        let transaction =
            Transaction::named("Create MIDI asset", [Command::AddEventData { event_data }]);
        self.commit_ui(&transaction, &[event_data_id.to_string()]);
        if self.last_error.is_none()
            && let Some(index) = self
                .project
                .event_data
                .iter()
                .position(|data| data.id == event_data_id)
        {
            self.apply(Intent::Select(Selection::MidiAsset(index)));
        }
    }

    fn create_midi_clip(&mut self, beat: f32, requested_track: Option<usize>) {
        let composition_id = self.current_composition_id();
        let composition = self
            .project
            .compositions
            .iter()
            .find(|composition| composition.id == composition_id)
            .expect("current composition exists")
            .clone();
        let start = f64::from(beat.max(0.0));
        let duration = self.project.time_signature.quarter_notes_per_bar();
        let name = self.next_midi_asset_name();
        let event_data = gaw_core::EventData::new(name.clone());
        let event_data_id = event_data.id;
        let mut commands = Vec::new();
        extend_composition_for_drop(&composition, start, duration, duration, &mut commands);
        commands.push(Command::AddEventData { event_data });

        let existing_track = requested_track
            .filter(|index| {
                self.current_composition()
                    .tracks
                    .get(*index)
                    .is_some_and(|track| track.kind == TrackKind::Event)
            })
            .and_then(|index| self.current_track_id(index).map(|id| (id, index)));
        let creates_track = existing_track.is_none();
        let (track_id, track_index) = existing_track.unwrap_or_else(|| {
            let sampler = gaw_core::Sampler::new(32).expect("valid sampler polyphony");
            let track = gaw_core::Track::event(
                composition_id,
                name.clone(),
                gaw_core::Instrument::sampler("Sampler", sampler),
            );
            let id = track.id;
            let index = composition.track_ids.len();
            commands.push(Command::AddTrack { track, index });
            (id, index)
        });

        let mut clip = gaw_core::EventClip::new(
            event_data_id,
            gaw_core::Beats::new(start).expect("finite start"),
            gaw_core::Beats::new(duration).expect("positive duration"),
        );
        clip.name = name;
        let clip_id = clip.id;
        commands.push(Command::AddClip {
            track_id,
            clip: gaw_core::Clip::Event(clip),
        });
        let transaction = Transaction::named(
            if creates_track {
                "Create MIDI track"
            } else {
                "Create MIDI clip"
            },
            commands,
        );
        self.commit_ui(
            &transaction,
            &[
                event_data_id.to_string(),
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
            self.apply(Intent::Select(Selection::Clip {
                track: track_index,
                clip: clip_index,
            }));
        }
    }

    pub(crate) fn add_transcribed_event_data(
        &mut self,
        mut event_data: gaw_core::EventData,
    ) -> Result<String, String> {
        let requested = event_data.name.clone();
        if self
            .project
            .event_data
            .iter()
            .any(|data| data.name == event_data.name)
        {
            let stem = requested.strip_suffix(" (MIDI)").unwrap_or(&requested);
            let mut number = 2;
            loop {
                let candidate = format!("{stem} (MIDI {number})");
                if self
                    .project
                    .event_data
                    .iter()
                    .all(|data| data.name != candidate)
                {
                    event_data.name = candidate;
                    break;
                }
                number += 1;
            }
        }
        let id = event_data.id;
        let name = event_data.name.clone();
        let transaction = Transaction::named(
            format!("Create {name}"),
            [Command::AddEventData { event_data }],
        );
        if let Err(error) = self.commit(&transaction, ChangeSource::Ui, &[id.to_string()], 0.0) {
            let error = error.to_string();
            self.last_error = Some(error.clone());
            return Err(error);
        }
        if let Some(index) = self
            .project
            .event_data
            .iter()
            .position(|data| data.id == id)
        {
            self.apply(Intent::Select(Selection::MidiAsset(index)));
        }
        Ok(name)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod performance;

#[cfg(test)]
mod snapshots;

#[cfg(test)]
mod update_tests;
