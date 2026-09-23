//! Typed, atomic edits and project-wide integrity validation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Display;
use std::num::NonZeroUsize;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod history;
mod validation;

pub use history::EditHistory;
use history::apply_transaction;

use crate::model::{
    AssetFolder, AssetId, AssetRevisionId, AssetTempo, AudioAsset, AudioAssetDefinition,
    AudioAssetRevision, AudioTransform, AutomationLane, AutomationLaneId, AutomationTarget,
    AutomationUnit, Beats, Bpm, Clip, ClipId, Composition, CompositionId, Decibels, EffectPreset,
    Event, EventData, EventDataId, Instrument, InstrumentId, InstrumentKind, ModelError, Project,
    ProjectSettings, SampleRate, SamplerPreset, Seconds, SourceRange, TempoSync, TimeSignature,
    Track, TrackId, TrackKind,
};
use crate::processors::{
    AutomationSupport, ParameterDescriptor, ParameterRange, ParameterUnit, ParameterValueType,
    Processor, ProcessorId, ProcessorKind,
};

/// A domain invariant or command precondition failure.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum DomainError {
    #[error("{entity} {id} does not exist")]
    NotFound { entity: &'static str, id: String },
    #[error("{entity} {id} already exists")]
    AlreadyExists { entity: &'static str, id: String },
    #[error("invalid {field}: {message}")]
    Invalid {
        field: &'static str,
        message: String,
    },
    #[error("reference from {from} to missing {to}")]
    DanglingReference { from: String, to: String },
    #[error("dependency cycle: {path}")]
    DependencyCycle { path: String },
    #[error("cross-composition access from {from} to {to} is forbidden")]
    CrossBoundary { from: String, to: String },
    #[error("cannot remove {entity} {id}; it is referenced by {referenced_by}")]
    InUse {
        entity: &'static str,
        id: String,
        referenced_by: String,
    },
    #[error("index {index} is outside 0..={len}")]
    IndexOutOfBounds { index: usize, len: usize },
    #[error("transaction must contain at least one command")]
    EmptyTransaction,
    #[error("nothing to undo")]
    NothingToUndo,
    #[error("nothing to redo")]
    NothingToRedo,
}

/// Validates a value independently of serialization.
pub trait Validate {
    /// Checks all invariants reachable from this value.
    ///
    /// # Errors
    /// Returns the first invariant violation in deterministic traversal order.
    fn validate(&self) -> Result<(), DomainError>;
}

fn invalid(field: &'static str, message: impl Display) -> DomainError {
    DomainError::Invalid {
        field,
        message: message.to_string(),
    }
}

fn not_found(entity: &'static str, id: impl Display) -> DomainError {
    DomainError::NotFound {
        entity,
        id: id.to_string(),
    }
}

fn already_exists(entity: &'static str, id: impl Display) -> DomainError {
    DomainError::AlreadyExists {
        entity,
        id: id.to_string(),
    }
}

fn checked_insert<T>(index: usize, values: &mut Vec<T>, value: T) -> Result<(), DomainError> {
    if index > values.len() {
        return Err(DomainError::IndexOutOfBounds {
            index,
            len: values.len(),
        });
    }
    values.insert(index, value);
    Ok(())
}

fn checked_move<T>(values: &mut Vec<T>, from: usize, to: usize) -> Result<(), DomainError> {
    let len = values.len();
    if from >= len {
        return Err(DomainError::IndexOutOfBounds { index: from, len });
    }
    if to >= len {
        return Err(DomainError::IndexOutOfBounds { index: to, len });
    }
    if from != to {
        let value = values.remove(from);
        values.insert(to, value);
    }
    Ok(())
}

/// The location of an ordered processor stack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "scope", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProcessorStack {
    /// Audio-clip processing or post-instrument event-clip processing.
    Clip {
        track_id: TrackId,
        clip_id: ClipId,
    },
    CompositionClip {
        track_id: TrackId,
        clip_id: ClipId,
    },
    Track {
        track_id: TrackId,
    },
    CompositionOutput {
        composition_id: CompositionId,
    },
}

/// Every canonical model edit is explicit and serializable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    SetProjectName {
        name: String,
    },
    SetProjectTempo {
        bpm: Bpm,
    },
    SetProjectTimeSignature {
        time_signature: TimeSignature,
    },
    SetProjectMetronome {
        enabled: bool,
    },
    SetProjectMasterVolume {
        volume: Decibels,
    },
    SetProjectSampleRate {
        sample_rate: SampleRate,
    },
    SetProjectSettings {
        settings: ProjectSettings,
    },

    SetAssetFolders {
        folders: Vec<AssetFolder>,
    },

    AddAsset {
        asset: AudioAsset,
    },
    UpdateAsset {
        asset: AudioAsset,
    },
    SetAssetTempo {
        asset_id: AssetId,
        tempo: Option<AssetTempo>,
    },
    SetAssetBpm {
        asset_id: AssetId,
        bpm: Option<Bpm>,
    },
    SetAssetFirstBeat {
        asset_id: AssetId,
        first_beat: Seconds,
    },
    AddAssetRevision {
        asset_id: AssetId,
        revision: AudioAssetRevision,
    },
    SetAssetCurrentRevision {
        asset_id: AssetId,
        revision_id: Option<AssetRevisionId>,
    },
    RemoveAsset {
        asset_id: AssetId,
    },

    AddEventData {
        event_data: EventData,
    },
    UpdateEventData {
        event_data: EventData,
    },
    RemoveEventData {
        event_data_id: EventDataId,
    },

    AddComposition {
        composition: Composition,
    },
    UpdateComposition {
        composition: Composition,
    },
    RemoveComposition {
        composition_id: CompositionId,
    },
    ReorderCompositionTracks {
        composition_id: CompositionId,
        from: usize,
        to: usize,
    },

    AddTrack {
        track: Track,
        index: usize,
    },
    UpdateTrack {
        track: Track,
    },
    SetTrackVolume {
        track_id: TrackId,
        volume_db: f32,
    },
    RemoveTrack {
        track_id: TrackId,
    },
    MoveTrack {
        track_id: TrackId,
        composition_id: CompositionId,
        index: usize,
    },

    AddClip {
        track_id: TrackId,
        clip: Clip,
    },
    UpdateClip {
        track_id: TrackId,
        clip: Clip,
    },
    RemoveClip {
        track_id: TrackId,
        clip_id: ClipId,
    },
    MoveClip {
        clip_id: ClipId,
        from_track_id: TrackId,
        to_track_id: TrackId,
    },

    SetTrackInstrument {
        track_id: TrackId,
        instrument: Option<Instrument>,
    },
    ApplySamplerPreset {
        track_id: TrackId,
        instrument_id: InstrumentId,
        preset: SamplerPreset,
    },

    InsertProcessor {
        stack: ProcessorStack,
        index: usize,
        processor: Processor,
    },
    UpdateProcessor {
        stack: ProcessorStack,
        processor: Processor,
    },
    RemoveProcessor {
        stack: ProcessorStack,
        processor_id: ProcessorId,
    },
    ReorderProcessor {
        stack: ProcessorStack,
        from: usize,
        to: usize,
    },
    InsertEffectPreset {
        stack: ProcessorStack,
        index: usize,
        processor_id: ProcessorId,
        preset: EffectPreset,
    },
    ApplyEffectPreset {
        stack: ProcessorStack,
        processor_id: ProcessorId,
        preset: EffectPreset,
    },

    AddAutomation {
        lane: AutomationLane,
    },
    UpdateAutomation {
        lane: AutomationLane,
    },
    RemoveAutomation {
        lane_id: AutomationLaneId,
    },
}

impl Command {
    /// Whether applying this command changes canonical rendered audio.
    /// Monitoring-only output controls are handled by the realtime host.
    pub const fn affects_render(&self) -> bool {
        !matches!(self, Self::SetProjectMasterVolume { .. })
    }

    /// Applies this command atomically and validates the resulting project.
    ///
    /// # Errors
    /// Returns a precondition or validation error without changing `project`.
    pub fn apply(&self, project: &mut Project) -> Result<(), DomainError> {
        apply_transaction(project, std::iter::once(self)).map(|_| ())
    }

    #[allow(clippy::too_many_lines)]
    fn apply_unvalidated(&self, project: &mut Project) -> Result<(), DomainError> {
        match self {
            Self::SetProjectName { name } => project.name.clone_from(name),
            Self::SetProjectTempo { bpm } => project.bpm = *bpm,
            Self::SetProjectTimeSignature { time_signature } => {
                project.time_signature = *time_signature;
            }
            Self::SetProjectMetronome { enabled } => {
                project.settings.metronome_enabled = *enabled;
            }
            Self::SetProjectMasterVolume { volume } => {
                project.settings.master_volume = *volume;
            }
            Self::SetProjectSampleRate { sample_rate } => project.sample_rate = *sample_rate,
            Self::SetProjectSettings { settings } => project.settings.clone_from(settings),
            Self::SetAssetFolders { folders } => project.asset_folders.clone_from(folders),

            Self::AddAsset { asset } => {
                add_unique(&mut project.assets, asset.clone(), |v| v.id, "asset")?;
            }
            Self::UpdateAsset { asset } => {
                replace_by_id(&mut project.assets, asset.clone(), |v| v.id, "asset")?;
            }
            Self::SetAssetTempo { asset_id, tempo } => {
                asset_mut(project, *asset_id)?.tempo = *tempo;
            }
            Self::SetAssetBpm { asset_id, bpm } => {
                let asset = asset_mut(project, *asset_id)?;
                asset.tempo = bpm.map(|bpm| AssetTempo {
                    bpm,
                    first_beat: asset.tempo.map_or_else(
                        || Seconds::new(0.0).expect("zero is valid"),
                        |value| value.first_beat,
                    ),
                });
            }
            Self::SetAssetFirstBeat {
                asset_id,
                first_beat,
            } => {
                let tempo = asset_mut(project, *asset_id)?
                    .tempo
                    .as_mut()
                    .ok_or_else(|| invalid("asset.tempo", "set asset BPM before its first beat"))?;
                tempo.first_beat = *first_beat;
            }
            Self::AddAssetRevision { asset_id, revision } => {
                let asset = asset_mut(project, *asset_id)?;
                add_unique(
                    &mut asset.revisions,
                    revision.clone(),
                    |value| value.id,
                    "asset revision",
                )?;
            }
            Self::SetAssetCurrentRevision {
                asset_id,
                revision_id,
            } => {
                asset_mut(project, *asset_id)?.current_revision_id = *revision_id;
            }
            Self::RemoveAsset { asset_id } => {
                project.asset_folders.retain_mut(|folder| {
                    let contained = folder.asset_ids.contains(asset_id);
                    folder.asset_ids.retain(|id| id != asset_id);
                    !contained || !folder.asset_ids.is_empty() || !folder.event_data_ids.is_empty()
                });
                remove_by_id(&mut project.assets, asset_id, |v| &v.id, "asset")?;
            }

            Self::AddEventData { event_data } => add_unique(
                &mut project.event_data,
                event_data.clone(),
                |v| v.id,
                "event data",
            )?,
            Self::UpdateEventData { event_data } => replace_by_id(
                &mut project.event_data,
                event_data.clone(),
                |v| v.id,
                "event data",
            )?,
            Self::RemoveEventData { event_data_id } => {
                project.asset_folders.retain_mut(|folder| {
                    let contained = folder.event_data_ids.contains(event_data_id);
                    folder.event_data_ids.retain(|id| id != event_data_id);
                    !contained || !folder.asset_ids.is_empty() || !folder.event_data_ids.is_empty()
                });
                remove_by_id(
                    &mut project.event_data,
                    event_data_id,
                    |v| &v.id,
                    "event data",
                )?;
            }

            Self::AddComposition { composition } => add_unique(
                &mut project.compositions,
                composition.clone(),
                |v| v.id,
                "composition",
            )?,
            Self::UpdateComposition { composition } => replace_by_id(
                &mut project.compositions,
                composition.clone(),
                |v| v.id,
                "composition",
            )?,
            Self::RemoveComposition { composition_id } => {
                if *composition_id == project.root_composition_id {
                    return Err(invalid(
                        "composition_id",
                        "the root composition cannot be removed",
                    ));
                }
                remove_by_id(
                    &mut project.compositions,
                    composition_id,
                    |v| &v.id,
                    "composition",
                )?;
            }
            Self::ReorderCompositionTracks {
                composition_id,
                from,
                to,
            } => {
                checked_move(
                    &mut composition_mut(project, *composition_id)?.track_ids,
                    *from,
                    *to,
                )?;
            }

            Self::AddTrack { track, index } => {
                if project.tracks.iter().any(|v| v.id == track.id) {
                    return Err(already_exists("track", track.id));
                }
                let composition = composition_mut(project, track.composition_id)?;
                if *index > composition.track_ids.len() {
                    return Err(DomainError::IndexOutOfBounds {
                        index: *index,
                        len: composition.track_ids.len(),
                    });
                }
                checked_insert(*index, &mut composition.track_ids, track.id)?;
                project.tracks.push(track.clone());
            }
            Self::UpdateTrack { track } => {
                let old = track_mut(project, track.id)?;
                if old.composition_id != track.composition_id {
                    return Err(invalid(
                        "track.composition_id",
                        "use move_track to change ownership",
                    ));
                }
                *old = track.clone();
            }
            Self::SetTrackVolume {
                track_id,
                volume_db,
            } => {
                track_mut(project, *track_id)?.volume_db = *volume_db;
            }
            Self::RemoveTrack { track_id } => {
                let composition_id = track(project, *track_id)?.composition_id;
                let track_ids = &mut composition_mut(project, composition_id)?.track_ids;
                let index = track_ids
                    .iter()
                    .position(|id| id == track_id)
                    .ok_or_else(|| dangling(composition_id, track_id))?;
                track_ids.remove(index);
                remove_by_id(&mut project.tracks, track_id, |v| &v.id, "track")?;
            }
            Self::MoveTrack {
                track_id,
                composition_id,
                index,
            } => {
                composition(project, *composition_id)?;
                let old_composition_id = track(project, *track_id)?.composition_id;
                let old_index = composition(project, old_composition_id)?
                    .track_ids
                    .iter()
                    .position(|id| id == track_id)
                    .ok_or_else(|| DomainError::DanglingReference {
                        from: old_composition_id.to_string(),
                        to: track_id.to_string(),
                    })?;
                let destination_len = composition(project, *composition_id)?.track_ids.len()
                    - usize::from(old_composition_id == *composition_id);
                if *index > destination_len {
                    return Err(DomainError::IndexOutOfBounds {
                        index: *index,
                        len: destination_len,
                    });
                }
                let removed = {
                    let ids = &mut composition_mut(project, old_composition_id)?.track_ids;
                    ids.remove(old_index)
                };
                checked_insert(
                    *index,
                    &mut composition_mut(project, *composition_id)?.track_ids,
                    removed,
                )?;
                track_mut(project, *track_id)?.composition_id = *composition_id;
            }

            Self::AddClip { track_id, clip } => {
                if project
                    .tracks
                    .iter()
                    .flat_map(|v| &v.clips)
                    .any(|v| v.id() == clip.id())
                {
                    return Err(already_exists("clip", clip.id()));
                }
                let track = track_mut(project, *track_id)?;
                let mut packed = clip.clone();
                pack_clip_on_track(track, &mut packed, None);
                track.clips.push(packed);
            }
            Self::UpdateClip { track_id, clip } => {
                let track = track_mut(project, *track_id)?;
                let mut packed = clip.clone();
                let packed_id = packed.id();
                pack_clip_on_track(track, &mut packed, Some(packed_id));
                replace_by_id(&mut track.clips, packed, Clip::id, "clip")?;
            }
            Self::RemoveClip { track_id, clip_id } => {
                let clips = &mut track_mut(project, *track_id)?.clips;
                let index = clips
                    .iter()
                    .position(|value| value.id() == *clip_id)
                    .ok_or_else(|| not_found("clip", clip_id))?;
                clips.remove(index);
            }
            Self::MoveClip {
                clip_id,
                from_track_id,
                to_track_id,
            } => {
                track(project, *to_track_id)?;
                let clip = {
                    let clips = &mut track_mut(project, *from_track_id)?.clips;
                    let index = clips
                        .iter()
                        .position(|v| v.id() == *clip_id)
                        .ok_or_else(|| not_found("clip", clip_id))?;
                    clips.remove(index)
                };
                // A following UpdateClip command carries the requested timing
                // for UI moves and records it in its replace delta. Keeping
                // this structural move timing-neutral preserves exact undo for
                // standalone MoveClip commands as well.
                track_mut(project, *to_track_id)?.clips.push(clip);
            }

            Self::SetTrackInstrument {
                track_id,
                instrument,
            } => {
                track_mut(project, *track_id)?
                    .instrument
                    .clone_from(instrument);
            }
            Self::ApplySamplerPreset {
                track_id,
                instrument_id,
                preset,
            } => {
                preset.validate().map_err(model_error)?;
                track_mut(project, *track_id)?.instrument =
                    Some(preset.clone().into_instrument(*instrument_id));
            }

            Self::InsertProcessor {
                stack,
                index,
                processor,
            } => {
                if all_processors(project).any(|value| value.id == processor.id) {
                    return Err(already_exists("processor", &processor.id));
                }
                checked_insert(
                    *index,
                    processor_stack_mut(project, stack)?,
                    processor.clone(),
                )?;
            }
            Self::UpdateProcessor { stack, processor } => {
                replace_by_id(
                    processor_stack_mut(project, stack)?,
                    processor.clone(),
                    |v| v.id.clone(),
                    "processor",
                )?;
            }
            Self::RemoveProcessor {
                stack,
                processor_id,
            } => {
                remove_by_id(
                    processor_stack_mut(project, stack)?,
                    processor_id,
                    |v| &v.id,
                    "processor",
                )?;
            }
            Self::ReorderProcessor { stack, from, to } => {
                checked_move(processor_stack_mut(project, stack)?, *from, *to)?;
            }
            Self::InsertEffectPreset {
                stack,
                index,
                processor_id,
                preset,
            } => {
                preset.validate().map_err(model_error)?;
                if all_processors(project).any(|value| value.id == *processor_id) {
                    return Err(already_exists("processor", processor_id));
                }
                checked_insert(
                    *index,
                    processor_stack_mut(project, stack)?,
                    preset.clone().into_processor(processor_id.clone()),
                )?;
            }
            Self::ApplyEffectPreset {
                stack,
                processor_id,
                preset,
            } => {
                preset.validate().map_err(model_error)?;
                replace_by_id(
                    processor_stack_mut(project, stack)?,
                    preset.clone().into_processor(processor_id.clone()),
                    |value| value.id.clone(),
                    "processor",
                )?;
            }

            Self::AddAutomation { lane } => add_unique(
                &mut project.automation,
                lane.clone(),
                |v| v.id,
                "automation lane",
            )?,
            Self::UpdateAutomation { lane } => replace_by_id(
                &mut project.automation,
                lane.clone(),
                |v| v.id,
                "automation lane",
            )?,
            Self::RemoveAutomation { lane_id } => {
                remove_by_id(
                    &mut project.automation,
                    lane_id,
                    |v| &v.id,
                    "automation lane",
                )?;
            }
        }
        Ok(())
    }
}

fn add_unique<T, I: Display + PartialEq>(
    values: &mut Vec<T>,
    value: T,
    id: impl Fn(&T) -> I,
    entity: &'static str,
) -> Result<(), DomainError> {
    let value_id = id(&value);
    if values.iter().any(|old| id(old) == value_id) {
        return Err(already_exists(entity, value_id));
    }
    values.push(value);
    Ok(())
}

fn replace_by_id<T, I: Display + PartialEq>(
    values: &mut [T],
    value: T,
    id: impl Fn(&T) -> I,
    entity: &'static str,
) -> Result<(), DomainError> {
    let value_id = id(&value);
    let old = values
        .iter_mut()
        .find(|old| id(old) == value_id)
        .ok_or_else(|| not_found(entity, &value_id))?;
    *old = value;
    Ok(())
}

fn remove_by_id<T, I: Display + PartialEq + ?Sized>(
    values: &mut Vec<T>,
    value_id: &I,
    id: impl Fn(&T) -> &I,
    entity: &'static str,
) -> Result<T, DomainError> {
    let index = values
        .iter()
        .position(|value| id(value) == value_id)
        .ok_or_else(|| not_found(entity, value_id))?;
    Ok(values.remove(index))
}

fn composition(project: &Project, id: CompositionId) -> Result<&Composition, DomainError> {
    project
        .compositions
        .iter()
        .find(|value| value.id == id)
        .ok_or_else(|| not_found("composition", id))
}

fn asset(project: &Project, id: AssetId) -> Result<&AudioAsset, DomainError> {
    project
        .assets
        .iter()
        .find(|value| value.id == id)
        .ok_or_else(|| not_found("asset", id))
}

fn asset_mut(project: &mut Project, id: AssetId) -> Result<&mut AudioAsset, DomainError> {
    project
        .assets
        .iter_mut()
        .find(|value| value.id == id)
        .ok_or_else(|| not_found("asset", id))
}

fn composition_mut(
    project: &mut Project,
    id: CompositionId,
) -> Result<&mut Composition, DomainError> {
    project
        .compositions
        .iter_mut()
        .find(|value| value.id == id)
        .ok_or_else(|| not_found("composition", id))
}

fn track(project: &Project, id: TrackId) -> Result<&Track, DomainError> {
    project
        .tracks
        .iter()
        .find(|value| value.id == id)
        .ok_or_else(|| not_found("track", id))
}

fn track_mut(project: &mut Project, id: TrackId) -> Result<&mut Track, DomainError> {
    project
        .tracks
        .iter_mut()
        .find(|value| value.id == id)
        .ok_or_else(|| not_found("track", id))
}

fn processor_stack_mut<'a>(
    project: &'a mut Project,
    location: &ProcessorStack,
) -> Result<&'a mut Vec<Processor>, DomainError> {
    match *location {
        ProcessorStack::Clip { track_id, clip_id } => {
            let clip = track_mut(project, track_id)?
                .clips
                .iter_mut()
                .find(|value| value.id() == clip_id)
                .ok_or_else(|| not_found("clip", clip_id))?;
            match clip {
                Clip::Audio(value) => Ok(&mut value.effects),
                Clip::Event(value) => Ok(&mut value.effects),
                Clip::Composition(_) => {
                    Err(invalid("stack", "stack is not an audio or event clip"))
                }
            }
        }
        ProcessorStack::CompositionClip { track_id, clip_id } => {
            let clip = track_mut(project, track_id)?
                .clips
                .iter_mut()
                .find(|value| value.id() == clip_id)
                .ok_or_else(|| not_found("clip", clip_id))?;
            match clip {
                Clip::Composition(value) => Ok(&mut value.effects),
                Clip::Audio(_) | Clip::Event(_) => {
                    Err(invalid("stack", "stack is not a composition clip"))
                }
            }
        }
        ProcessorStack::Track { track_id } => Ok(&mut track_mut(project, track_id)?.effects),
        ProcessorStack::CompositionOutput { composition_id } => {
            Ok(&mut composition_mut(project, composition_id)?.output_effects)
        }
    }
}

fn processor_stack<'a>(
    project: &'a Project,
    location: &ProcessorStack,
) -> Result<&'a Vec<Processor>, DomainError> {
    match *location {
        ProcessorStack::Clip { track_id, clip_id } => {
            let clip = track(project, track_id)?
                .clips
                .iter()
                .find(|value| value.id() == clip_id)
                .ok_or_else(|| not_found("clip", clip_id))?;
            match clip {
                Clip::Audio(value) => Ok(&value.effects),
                Clip::Event(value) => Ok(&value.effects),
                Clip::Composition(_) => {
                    Err(invalid("stack", "stack is not an audio or event clip"))
                }
            }
        }
        ProcessorStack::CompositionClip { track_id, clip_id } => {
            let clip = track(project, track_id)?
                .clips
                .iter()
                .find(|value| value.id() == clip_id)
                .ok_or_else(|| not_found("clip", clip_id))?;
            match clip {
                Clip::Composition(value) => Ok(&value.effects),
                Clip::Audio(_) | Clip::Event(_) => {
                    Err(invalid("stack", "stack is not a composition clip"))
                }
            }
        }
        ProcessorStack::Track { track_id } => Ok(&track(project, track_id)?.effects),
        ProcessorStack::CompositionOutput { composition_id } => {
            Ok(&composition(project, composition_id)?.output_effects)
        }
    }
}

fn all_processors(project: &Project) -> impl Iterator<Item = &Processor> {
    project
        .assets
        .iter()
        .flat_map(|asset| match &asset.definition {
            AudioAssetDefinition::Processed { effects, .. } => effects.as_slice(),
            _ => &[],
        })
        .chain(
            project
                .compositions
                .iter()
                .flat_map(|value| &value.output_effects),
        )
        .chain(project.tracks.iter().flat_map(|value| &value.effects))
        .chain(
            project
                .tracks
                .iter()
                .flat_map(|track| &track.clips)
                .flat_map(|clip| match clip {
                    Clip::Audio(value) => value.effects.as_slice(),
                    Clip::Composition(value) => value.effects.as_slice(),
                    Clip::Event(value) => value.effects.as_slice(),
                }),
        )
}

fn clip_end(clip: &Clip) -> f64 {
    clip.start().value()
        + match clip {
            Clip::Audio(value) => value.duration.value(),
            Clip::Event(value) => value.duration.value(),
            Clip::Composition(value) => value.duration.value(),
        }
}

fn set_clip_start(clip: &mut Clip, start: f64) {
    let start = Beats::new(start).expect("packed clip start is finite");
    match clip {
        Clip::Audio(value) => value.start = start,
        Clip::Event(value) => value.start = start,
        Clip::Composition(value) => value.start = start,
    }
}

/// Returns the nearest non-overlapping start for a requested clip position.
/// Existing clips are never moved; a colliding edit is packed against the
/// nearest neighboring clip with enough room. This keeps the invariant local
/// to the edited track and makes `AddClip` and `UpdateClip` behave identically.
/// Cross-track UI moves follow their structural `MoveClip` with an `UpdateClip`
/// so the same packing rule applies while preserving exact undo data.
pub fn packed_clip_start(track: &Track, clip: &Clip, excluded: Option<ClipId>) -> f64 {
    let duration = clip_end(clip) - clip.start().value();
    if !duration.is_finite() || duration <= 0.0 {
        return clip.start().value();
    }
    let requested = clip.start().value().max(0.0);
    let mut ranges = track
        .clips
        .iter()
        .filter(|other| Some(other.id()) != excluded)
        .map(|other| (other.start().value().max(0.0), clip_end(other)))
        .filter(|(start, end)| end > start)
        .collect::<Vec<_>>();
    ranges.sort_by(|left, right| left.0.total_cmp(&right.0));

    let mut best = None::<(f64, f64)>;
    let mut gap_start = 0.0;
    for (range_start, range_end) in ranges {
        if range_start > gap_start && range_start - gap_start >= duration {
            let candidate = requested.clamp(gap_start, range_start - duration);
            let distance = (candidate - requested).abs();
            if best.is_none_or(|(best_distance, _)| distance < best_distance) {
                best = Some((distance, candidate));
            }
        }
        gap_start = gap_start.max(range_end);
    }
    let candidate = requested.max(gap_start);
    let distance = (candidate - requested).abs();
    if best.is_none_or(|(best_distance, _)| distance < best_distance) {
        candidate
    } else {
        best.map_or(candidate, |(_, packed)| packed)
    }
}

fn pack_clip_on_track(track: &Track, clip: &mut Clip, excluded: Option<ClipId>) {
    let start = packed_clip_start(track, clip, excluded);
    if (start - clip.start().value()).abs() > f64::EPSILON {
        set_clip_start(clip, start);
    }
}

fn model_error(error: ModelError) -> DomainError {
    invalid("model", error)
}

fn dangling(from: impl Display, to: impl Display) -> DomainError {
    DomainError::DanglingReference {
        from: from.to_string(),
        to: to.to_string(),
    }
}

/// A named atomic group of typed edits. One transaction is one history entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Transaction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub commands: Vec<Command>,
}

impl Transaction {
    pub fn new(commands: impl IntoIterator<Item = Command>) -> Self {
        Self {
            label: None,
            commands: commands.into_iter().collect(),
        }
    }

    pub fn named(label: impl Into<String>, commands: impl IntoIterator<Item = Command>) -> Self {
        Self {
            label: Some(label.into()),
            commands: commands.into_iter().collect(),
        }
    }

    pub fn affects_render(&self) -> bool {
        self.commands.iter().any(Command::affects_render)
    }

    /// Applies atomically without recording history.
    ///
    /// # Errors
    /// Returns a command precondition or final project validation error.
    pub fn apply(&self, project: &mut Project) -> Result<(), DomainError> {
        apply_transaction(project, self.commands.iter()).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AudioClip, AutomationCurve, AutomationPoint, AutomationValue, BarTimelineGap, Beats,
        ChannelLayout, CompositionClip, ContentHash, Decibels, FrameCount, Hertz, ImportedAudio,
        ProjectPath, TrackGroup,
    };
    use crate::processors::{ChorusParameters, DelayParameters, GainParameters, ProcessorId};

    fn beats(value: f64) -> Beats {
        Beats::new(value).unwrap()
    }

    fn project() -> Project {
        let mut project = Project::new(
            "Test",
            Bpm::new(120.0).unwrap(),
            SampleRate::new(48_000).unwrap(),
        );
        project.compositions[0].length = beats(16.0);
        project
    }

    #[test]
    fn pristine_project_is_valid() {
        project().validate().unwrap();
    }

    #[test]
    fn composition_bar_timeline_gaps_are_strictly_validated() {
        let mut valid = project();
        valid.compositions[0].bar_timeline_gaps = vec![
            BarTimelineGap {
                start: beats(4.0),
                duration: beats(2.0),
            },
            BarTimelineGap {
                start: beats(8.0),
                duration: beats(1.0),
            },
        ];
        valid.validate().unwrap();

        let mut overlapping = valid.clone();
        overlapping.compositions[0].bar_timeline_gaps[1].start = beats(5.0);
        assert!(overlapping.validate().is_err());

        let mut zero_width = project();
        zero_width.compositions[0]
            .bar_timeline_gaps
            .push(BarTimelineGap {
                start: beats(4.0),
                duration: beats(0.0),
            });
        assert!(zero_width.validate().is_err());

        let mut out_of_bounds = project();
        out_of_bounds.compositions[0]
            .bar_timeline_gaps
            .push(BarTimelineGap {
                start: beats(15.0),
                duration: beats(2.0),
            });
        assert!(out_of_bounds.validate().is_err());
    }

    #[test]
    fn transaction_rolls_back_every_command_on_failure() {
        let mut project = project();
        let before = project.clone();
        let result = Transaction::new([
            Command::SetProjectName {
                name: "Changed".into(),
            },
            Command::RemoveTrack {
                track_id: TrackId::new(),
            },
        ])
        .apply(&mut project);
        assert!(matches!(result, Err(DomainError::NotFound { .. })));
        assert_eq!(project, before);
    }

    #[test]
    fn a_transaction_is_one_undo_entry_and_redo_is_exact() {
        let mut project = project();
        let before = project.clone();
        let mut history = EditHistory::default();
        history
            .apply(
                &mut project,
                &Transaction::new([
                    Command::SetProjectName {
                        name: "After".into(),
                    },
                    Command::SetProjectTempo {
                        bpm: Bpm::new(97.0).unwrap(),
                    },
                ]),
            )
            .unwrap();
        let after = project.clone();
        assert_eq!(history.undo_len(), 1);
        history.undo(&mut project).unwrap();
        assert_eq!(project, before);
        history.redo(&mut project).unwrap();
        assert_eq!(project, after);
    }

    #[test]
    fn removing_folder_members_preserves_unrelated_empty_folders_and_is_undoable() {
        let mut project = project();
        let asset = AudioAsset::imported(
            "Source",
            ImportedAudio {
                media_path: ProjectPath::new(format!("assets/media/{}.wav", "ab".repeat(32)))
                    .unwrap(),
                original_filename: "source.wav".into(),
                content_hash: ContentHash::new("ab".repeat(32)).unwrap(),
                sample_rate: SampleRate::new(48_000).unwrap(),
                layout: ChannelLayout::Stereo,
                frames: FrameCount(48_000),
            },
        );
        let event_data = EventData::new("Notes");
        let unrelated = AssetFolder {
            id: crate::AssetFolderId::new(),
            name: "Keep me".into(),
            asset_ids: vec![],
            event_data_ids: vec![],
        };
        project.assets.push(asset.clone());
        project.event_data.push(event_data.clone());
        project.asset_folders.extend([
            unrelated.clone(),
            AssetFolder {
                id: crate::AssetFolderId::new(),
                name: "Audio".into(),
                asset_ids: vec![asset.id],
                event_data_ids: vec![],
            },
            AssetFolder {
                id: crate::AssetFolderId::new(),
                name: "MIDI".into(),
                asset_ids: vec![],
                event_data_ids: vec![event_data.id],
            },
        ]);
        let before = project.clone();
        let mut history = EditHistory::default();

        history
            .apply(
                &mut project,
                &Transaction::new([
                    Command::RemoveAsset { asset_id: asset.id },
                    Command::RemoveEventData {
                        event_data_id: event_data.id,
                    },
                ]),
            )
            .unwrap();
        assert_eq!(project.asset_folders, [unrelated]);
        let after = project.clone();

        history.undo(&mut project).unwrap();
        assert_eq!(project, before);
        history.redo(&mut project).unwrap();
        assert_eq!(project, after);
    }

    #[test]
    fn time_signature_metronome_and_master_volume_are_undoable() {
        let mut project = project();
        let before = project.clone();
        let mut history = EditHistory::default();
        history
            .apply(
                &mut project,
                &Transaction::new([
                    Command::SetProjectTimeSignature {
                        time_signature: TimeSignature::new(7, 8).unwrap(),
                    },
                    Command::SetProjectMetronome { enabled: true },
                    Command::SetProjectMasterVolume {
                        volume: Decibels::new(-6.0).unwrap(),
                    },
                ]),
            )
            .unwrap();
        let after = project.clone();
        assert_eq!(after.time_signature, TimeSignature::new(7, 8).unwrap());
        assert!(after.settings.metronome_enabled);
        assert!((after.settings.master_volume.value() + 6.0).abs() < f64::EPSILON);
        history.undo(&mut project).unwrap();
        assert_eq!(project, before);
        history.redo(&mut project).unwrap();
        assert_eq!(project, after);
    }

    #[test]
    fn asset_folders_are_validated_and_exactly_undoable() {
        let mut project = project();
        let asset = AudioAsset::imported(
            "Kick",
            ImportedAudio {
                media_path: ProjectPath::new("assets/media/kick.wav").unwrap(),
                original_filename: "kick.wav".into(),
                content_hash: ContentHash::new("ab".repeat(32)).unwrap(),
                sample_rate: project.sample_rate,
                layout: ChannelLayout::Stereo,
                frames: FrameCount(48_000),
            },
        );
        let event_data = EventData::new("Beat");
        project.assets.push(asset.clone());
        project.event_data.push(event_data.clone());

        let before = project.clone();
        let folders = vec![AssetFolder {
            id: crate::AssetFolderId::new(),
            name: "Drums".into(),
            asset_ids: vec![asset.id],
            event_data_ids: vec![event_data.id],
        }];
        let mut history = EditHistory::default();
        history
            .apply(
                &mut project,
                &Transaction::new([Command::SetAssetFolders {
                    folders: folders.clone(),
                }]),
            )
            .unwrap();
        let after = project.clone();
        assert_eq!(project.asset_folders, folders);
        history.undo(&mut project).unwrap();
        assert_eq!(project, before);
        history.redo(&mut project).unwrap();
        assert_eq!(project, after);

        let duplicate = AssetFolder {
            id: crate::AssetFolderId::new(),
            name: "Also drums".into(),
            asset_ids: vec![asset.id],
            event_data_ids: vec![],
        };
        project.asset_folders.push(duplicate);
        assert!(matches!(
            project.validate(),
            Err(DomainError::Invalid {
                field: "asset_folder.asset_ids",
                ..
            })
        ));
        project.asset_folders.pop();
        project.asset_folders[0].name.clear();
        assert!(project.validate().is_err());
        project.asset_folders[0].name = "Drums".into();
        project.asset_folders[0].event_data_ids = vec![EventDataId::new()];
        assert!(matches!(
            project.validate(),
            Err(DomainError::DanglingReference { .. })
        ));
    }

    #[test]
    fn track_groups_are_scoped_and_memberships_are_unique() {
        let mut project = project();
        let root = project.root_composition_id;
        let first = Track::audio(root, "Kick");
        let second = Track::audio(root, "Snare");
        project.compositions[0].track_ids = vec![first.id, second.id];
        project.tracks = vec![first.clone(), second.clone()];
        project.compositions[0].track_groups = vec![TrackGroup {
            id: crate::TrackGroupId::new(),
            name: "Drums".into(),
            track_ids: vec![first.id, second.id],
            collapsed: true,
        }];
        project.validate().unwrap();

        project.compositions[0].track_groups.push(TrackGroup {
            id: crate::TrackGroupId::new(),
            name: "Duplicate".into(),
            track_ids: vec![first.id],
            collapsed: false,
        });
        assert!(matches!(
            project.validate(),
            Err(DomainError::Invalid {
                field: "track_group.track_ids",
                ..
            })
        ));
        project.compositions[0].track_groups.pop();
        project.compositions[0].track_groups[0].track_ids = vec![TrackId::new()];
        assert!(matches!(
            project.validate(),
            Err(DomainError::DanglingReference { .. })
        ));
    }

    #[test]
    fn project_validation_rejects_invalid_time_signatures() {
        let mut project = project();
        project.time_signature = TimeSignature {
            numerator: 0,
            denominator: 4,
        };
        assert!(matches!(
            project.validate(),
            Err(DomainError::Invalid {
                field: "project.time_signature",
                ..
            })
        ));

        project.time_signature = TimeSignature {
            numerator: 4,
            denominator: 3,
        };
        assert!(project.validate().is_err());
    }

    #[test]
    fn history_is_bounded_and_new_edits_clear_redo() {
        let mut project = project();
        let mut history = EditHistory::new(NonZeroUsize::new(3).unwrap());
        for index in 0..10 {
            history
                .apply(
                    &mut project,
                    &Transaction::new([Command::SetProjectName {
                        name: format!("v{index}"),
                    }]),
                )
                .unwrap();
        }
        assert_eq!(history.undo_len(), 3);
        for _ in 0..3 {
            history.undo(&mut project).unwrap();
        }
        assert_eq!(project.name, "v6");
        assert_eq!(history.undo(&mut project), Err(DomainError::NothingToUndo));
        history
            .apply(
                &mut project,
                &Transaction::new([Command::SetProjectName {
                    name: "fork".into(),
                }]),
            )
            .unwrap();
        assert_eq!(history.redo(&mut project), Err(DomainError::NothingToRedo));
    }

    #[test]
    fn rejects_composition_cycles() {
        let mut project = project();
        let root = project.root_composition_id;
        let child = Composition::new("Child", beats(16.0));
        let mut root_track = Track::audio(root, "Root track");
        root_track
            .clips
            .push(Clip::Composition(CompositionClip::new(
                child.id,
                beats(0.0),
                beats(1.0),
            )));
        let mut child_track = Track::audio(child.id, "Child track");
        child_track
            .clips
            .push(Clip::Composition(CompositionClip::new(
                root,
                beats(0.0),
                beats(1.0),
            )));
        project.compositions[0].track_ids.push(root_track.id);
        let mut child = child;
        child.track_ids.push(child_track.id);
        project.compositions.push(child);
        project.tracks.extend([root_track, child_track]);
        assert!(matches!(
            project.validate(),
            Err(DomainError::DependencyCycle { .. })
        ));
    }

    #[test]
    fn rejects_asset_dependency_cycles() {
        let mut project = project();
        let id = AssetId::new();
        project.assets.push(AudioAsset {
            id,
            name: "loop".into(),
            definition: AudioAssetDefinition::Processed {
                source_asset_id: id,
                transforms: vec![],
                effects: vec![],
            },
            tempo: None,
            revisions: vec![],
            current_revision_id: None,
        });
        assert!(matches!(
            project.validate(),
            Err(DomainError::DependencyCycle { .. })
        ));
    }

    #[test]
    fn source_ranges_and_tempo_sync_are_validated() {
        let mut project = project();
        let asset = AudioAsset::imported(
            "one second",
            ImportedAudio {
                media_path: ProjectPath::new("assets/media/test.wav").unwrap(),
                original_filename: "test.wav".into(),
                content_hash: ContentHash::new("ab".repeat(32)).unwrap(),
                sample_rate: SampleRate::new(48_000).unwrap(),
                layout: ChannelLayout::Stereo,
                frames: FrameCount(48_000),
            },
        );
        let root = project.root_composition_id;
        let mut track = Track::audio(root, "Audio");
        let mut clip = AudioClip::new(
            asset.id,
            beats(0.0),
            beats(1.0),
            SourceRange {
                start: Seconds::new(0.75).unwrap(),
                duration: Seconds::new(0.5).unwrap(),
            },
        );
        track.clips.push(Clip::Audio(clip.clone()));
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        project.assets.push(asset);
        assert!(matches!(
            project.validate(),
            Err(DomainError::Invalid { .. })
        ));

        clip.source.start = Seconds::new(0.25).unwrap();
        clip.tempo_sync = TempoSync::Stretch;
        project.tracks[0].clips[0] = Clip::Audio(clip);
        assert!(matches!(
            project.validate(),
            Err(DomainError::Invalid { .. })
        ));
        project.assets[0].tempo = Some(AssetTempo {
            bpm: Bpm::new(120.0).unwrap(),
            first_beat: Seconds::new(0.0).unwrap(),
        });
        project.validate().unwrap();
    }

    #[test]
    fn automation_units_must_match_introspected_parameters() {
        let mut project = project();
        let processor = Processor::new(
            ProcessorId::new("master_gain").unwrap(),
            ProcessorKind::Gain(GainParameters::default()),
        );
        project.compositions[0]
            .output_effects
            .push(processor.clone());
        project.automation.push(AutomationLane {
            id: AutomationLaneId::new(),
            composition_id: project.root_composition_id,
            name: "gain".into(),
            target: AutomationTarget::CompositionOutputProcessor {
                processor_id: processor.id,
                parameter_id: "gain_db".into(),
            },
            points: vec![AutomationPoint {
                time: beats(0.0),
                value: AutomationValue::Hertz(Hertz::new(440.0).unwrap()),
                curve: AutomationCurve::Linear,
            }],
        });
        assert!(matches!(
            project.validate(),
            Err(DomainError::Invalid { .. })
        ));
        project.automation[0].points[0].value =
            AutomationValue::Decibels(Decibels::new(-6.0).unwrap());
        project.validate().unwrap();
    }

    #[test]
    fn compound_automation_ranges_match_processor_validation() {
        let mut project = project();
        let processor = Processor::new(
            ProcessorId::new("chorus").unwrap(),
            ProcessorKind::Chorus(ChorusParameters::default()),
        );
        project.compositions[0]
            .output_effects
            .push(processor.clone());
        project.automation.push(AutomationLane {
            id: AutomationLaneId::new(),
            composition_id: project.root_composition_id,
            name: "rate".into(),
            target: AutomationTarget::CompositionOutputProcessor {
                processor_id: processor.id,
                parameter_id: "rate".into(),
            },
            points: vec![AutomationPoint {
                time: beats(0.0),
                value: AutomationValue::Hertz(Hertz::new(40.0).unwrap()),
                curve: AutomationCurve::Linear,
            }],
        });
        project.validate().unwrap();
        project.automation[0].points[0].value = AutomationValue::Hertz(Hertz::new(40.001).unwrap());
        assert!(project.validate().is_err());
        project.automation[0].points[0].value = AutomationValue::Beats(beats(1.0 / 64.0));
        project.validate().unwrap();
        project.automation[0].points[0].value = AutomationValue::Beats(beats(0.01));
        assert!(project.validate().is_err());

        let processor = Processor::new(
            ProcessorId::new("delay").unwrap(),
            ProcessorKind::Delay(DelayParameters::default()),
        );
        project.compositions[0].output_effects = vec![processor.clone()];
        project.automation[0].target = AutomationTarget::CompositionOutputProcessor {
            processor_id: processor.id,
            parameter_id: "time".into(),
        };
        project.automation[0].points[0].value = AutomationValue::Beats(beats(0.0));
        assert!(project.validate().is_err());
        project.automation[0].points[0].value = AutomationValue::Beats(beats(f64::EPSILON));
        project.validate().unwrap();
    }

    #[test]
    fn command_json_is_strict_and_round_trips() {
        let command = Command::SetProjectTempo {
            bpm: Bpm::new(128.0).unwrap(),
        };
        let json = serde_json::to_string(&command).unwrap();
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), command);
        assert!(
            serde_json::from_str::<Command>(
                r#"{"type":"set_project_name","name":"x","extra":true}"#
            )
            .is_err()
        );
    }

    #[test]
    fn processor_stack_has_exactly_the_four_v1_user_scopes() {
        let track_id = TrackId::new();
        let clip_id = ClipId::new();
        let composition_id = CompositionId::new();
        let cases = [
            (ProcessorStack::Clip { track_id, clip_id }, "clip"),
            (
                ProcessorStack::CompositionClip { track_id, clip_id },
                "composition_clip",
            ),
            (ProcessorStack::Track { track_id }, "track"),
            (
                ProcessorStack::CompositionOutput { composition_id },
                "composition_output",
            ),
        ];
        for (stack, scope) in cases {
            let json = serde_json::to_value(&stack).unwrap();
            assert_eq!(json["scope"], scope);
            assert_eq!(
                serde_json::from_value::<ProcessorStack>(json).unwrap(),
                stack
            );
        }
        assert!(
            serde_json::from_value::<ProcessorStack>(serde_json::json!({
                "scope": "asset",
                "asset_id": AssetId::new()
            }))
            .is_err()
        );
    }

    #[test]
    fn clip_stack_scopes_enforce_the_clip_kind() {
        let mut project = project();
        let asset = AudioAsset::imported(
            "audio",
            ImportedAudio {
                media_path: ProjectPath::new("assets/media/audio.wav").unwrap(),
                original_filename: "audio.wav".into(),
                content_hash: ContentHash::new("ab".repeat(32)).unwrap(),
                sample_rate: SampleRate::new(48_000).unwrap(),
                layout: ChannelLayout::Stereo,
                frames: FrameCount(48_000),
            },
        );
        let child = Composition::new("Child", beats(4.0));
        let mut track = Track::audio(project.root_composition_id, "Clips");
        let audio_clip = AudioClip::new(
            asset.id,
            beats(0.0),
            beats(1.0),
            SourceRange {
                start: Seconds::new(0.0).unwrap(),
                duration: Seconds::new(1.0).unwrap(),
            },
        );
        let composition_clip = CompositionClip::new(child.id, beats(1.0), beats(1.0));
        let audio_clip_id = audio_clip.id;
        let composition_clip_id = composition_clip.id;
        track
            .clips
            .extend([Clip::Audio(audio_clip), Clip::Composition(composition_clip)]);
        project.compositions[0].track_ids.push(track.id);
        project.assets.push(asset);
        project.compositions.push(child);
        project.tracks.push(track);
        project.validate().unwrap();

        let processor = Processor::new(
            ProcessorId::new("clip_gain").unwrap(),
            ProcessorKind::Gain(GainParameters::default()),
        );
        for stack in [
            ProcessorStack::Clip {
                track_id: project.tracks[0].id,
                clip_id: composition_clip_id,
            },
            ProcessorStack::CompositionClip {
                track_id: project.tracks[0].id,
                clip_id: audio_clip_id,
            },
        ] {
            assert!(matches!(
                Command::InsertProcessor {
                    stack,
                    index: 0,
                    processor: processor.clone(),
                }
                .apply(&mut project),
                Err(DomainError::Invalid { field: "stack", .. })
            ));
        }

        Command::InsertProcessor {
            stack: ProcessorStack::Clip {
                track_id: project.tracks[0].id,
                clip_id: audio_clip_id,
            },
            index: 0,
            processor: processor.clone(),
        }
        .apply(&mut project)
        .unwrap();
        let mut composition_processor = processor;
        composition_processor.id = ProcessorId::new("composition_gain").unwrap();
        Command::InsertProcessor {
            stack: ProcessorStack::CompositionClip {
                track_id: project.tracks[0].id,
                clip_id: composition_clip_id,
            },
            index: 0,
            processor: composition_processor,
        }
        .apply(&mut project)
        .unwrap();
    }

    fn event_clip_project() -> (Project, ProcessorStack) {
        let mut project = project();
        let events = EventData::new("notes");
        let clip = crate::EventClip::new(events.id, beats(0.0), beats(4.0));
        let mut legacy_json = serde_json::to_value(&clip).unwrap();
        legacy_json.as_object_mut().unwrap().remove("effects");
        assert_eq!(
            serde_json::from_value::<crate::EventClip>(legacy_json).unwrap(),
            clip
        );
        let mut track = Track::event(
            project.root_composition_id,
            "Sampler",
            Instrument::sampler("Sampler", crate::Sampler::new(8).unwrap()),
        );
        let stack = ProcessorStack::Clip {
            track_id: track.id,
            clip_id: clip.id,
        };
        track.clips.push(Clip::Event(clip));
        project.compositions[0].track_ids.push(track.id);
        project.event_data.push(events);
        project.tracks.push(track);
        (project, stack)
    }

    #[test]
    fn event_clip_effects_are_json_backed_and_undoable() {
        let (mut project, stack) = event_clip_project();
        let before = project.clone();
        let mut history = EditHistory::default();
        let processor = Processor::new(
            ProcessorId::new("event_gain").unwrap(),
            ProcessorKind::Gain(GainParameters::default()),
        );
        let second_id = ProcessorId::new("event_gain_2").unwrap();
        let preset = EffectPreset::new(
            "Quiet",
            ProcessorKind::Gain(GainParameters {
                gain_db: -6.0,
                ..GainParameters::default()
            }),
        );
        let transaction = Transaction::new([
            Command::InsertProcessor {
                stack: stack.clone(),
                index: 0,
                processor: processor.clone(),
            },
            Command::InsertEffectPreset {
                stack: stack.clone(),
                index: 1,
                processor_id: second_id.clone(),
                preset: preset.clone(),
            },
            Command::ReorderProcessor {
                stack: stack.clone(),
                from: 1,
                to: 0,
            },
            Command::ApplyEffectPreset {
                stack: stack.clone(),
                processor_id: processor.id.clone(),
                preset,
            },
        ]);
        history.apply(&mut project, &transaction).unwrap();
        assert_eq!(processor_stack(&project, &stack).unwrap()[0].id, second_id);
        assert_eq!(
            serde_json::from_value::<Project>(serde_json::to_value(&project).unwrap()).unwrap(),
            project
        );
        let after = project.clone();
        history.undo(&mut project).unwrap();
        assert_eq!(project, before);
        history.redo(&mut project).unwrap();
        assert_eq!(project, after);
    }

    #[test]
    fn event_clip_effects_validate_updates_and_automation_atomically() {
        let (mut project, stack) = event_clip_project();
        let processor = Processor::new(
            ProcessorId::new("event_gain").unwrap(),
            ProcessorKind::Gain(GainParameters::default()),
        );
        Command::InsertProcessor {
            stack: stack.clone(),
            index: 0,
            processor: processor.clone(),
        }
        .apply(&mut project)
        .unwrap();
        let after = project.clone();
        let mut invalid_processor = processor.clone();
        invalid_processor.kind = ProcessorKind::Gain(GainParameters {
            gain_db: 1_000.0,
            ..GainParameters::default()
        });
        assert!(
            Command::UpdateProcessor {
                stack: stack.clone(),
                processor: invalid_processor
            }
            .apply(&mut project)
            .is_err()
        );
        assert_eq!(project, after);
        assert!(
            Command::InsertProcessor {
                stack: stack.clone(),
                index: 0,
                processor: processor.clone()
            }
            .apply(&mut project)
            .is_err()
        );
        assert_eq!(project, after);
        let mut bypassed = processor.clone();
        bypassed.enabled = false;
        Command::UpdateProcessor {
            stack: stack.clone(),
            processor: bypassed,
        }
        .apply(&mut project)
        .unwrap();

        let lane = AutomationLane {
            id: AutomationLaneId::new(),
            composition_id: project.root_composition_id,
            name: "event gain".into(),
            target: AutomationTarget::AudioClipProcessor {
                track_id: project.tracks[0].id,
                clip_id: project.tracks[0].clips[0].id(),
                processor_id: processor.id.clone(),
                parameter_id: "gain_db".into(),
            },
            points: vec![AutomationPoint {
                time: beats(0.0),
                value: AutomationValue::Decibels(Decibels::new(-6.0).unwrap()),
                curve: AutomationCurve::Step,
            }],
        };
        Command::AddAutomation { lane: lane.clone() }
            .apply(&mut project)
            .unwrap();
        let automated = project.clone();
        assert!(
            Command::RemoveProcessor {
                stack: stack.clone(),
                processor_id: processor.id.clone()
            }
            .apply(&mut project)
            .is_err()
        );
        assert_eq!(project, automated);
        Transaction::new([
            Command::RemoveAutomation { lane_id: lane.id },
            Command::RemoveProcessor {
                stack: stack.clone(),
                processor_id: processor.id,
            },
        ])
        .apply(&mut project)
        .unwrap();
        assert!(processor_stack(&project, &stack).unwrap().is_empty());
    }

    #[test]
    fn processed_asset_effects_remain_internal_dependency_definitions() {
        let mut project = project();
        let source = AudioAsset::imported(
            "source",
            ImportedAudio {
                media_path: ProjectPath::new("assets/media/source.wav").unwrap(),
                original_filename: "source.wav".into(),
                content_hash: ContentHash::new("ab".repeat(32)).unwrap(),
                sample_rate: SampleRate::new(48_000).unwrap(),
                layout: ChannelLayout::Stereo,
                frames: FrameCount(48_000),
            },
        );
        let internal = Processor::new(
            ProcessorId::new("internal_gain").unwrap(),
            ProcessorKind::Gain(GainParameters::default()),
        );
        project.assets.push(AudioAsset {
            id: AssetId::new(),
            name: "processed".into(),
            definition: AudioAssetDefinition::Processed {
                source_asset_id: source.id,
                transforms: vec![],
                effects: vec![internal.clone()],
            },
            tempo: None,
            revisions: vec![],
            current_revision_id: None,
        });
        project.assets.insert(0, source);
        project.validate().unwrap();

        let json = serde_json::to_value(&project.assets[1]).unwrap();
        assert_eq!(json["definition"]["type"], "processed");
        assert_eq!(
            json["definition"]["data"]["effects"][0]["id"],
            "internal_gain"
        );

        project.compositions[0].output_effects.push(internal);
        assert!(matches!(
            project.validate(),
            Err(DomainError::AlreadyExists {
                entity: "processor",
                ..
            })
        ));
    }

    #[test]
    fn delta_history_restores_exact_nested_order_and_revision_state() {
        let mut project = project();
        let mut asset = AudioAsset::imported(
            "audio",
            ImportedAudio {
                media_path: ProjectPath::new("assets/media/audio.wav").unwrap(),
                original_filename: "audio.wav".into(),
                content_hash: ContentHash::new("ab".repeat(32)).unwrap(),
                sample_rate: SampleRate::new(48_000).unwrap(),
                layout: ChannelLayout::Stereo,
                frames: FrameCount(48_000),
            },
        );
        let revision = AudioAssetRevision {
            id: AssetRevisionId::new(),
            content_hash: ContentHash::new("cd".repeat(32)).unwrap(),
            definition_hash: ContentHash::new("ef".repeat(32)).unwrap(),
            dependency_revision_ids: vec![],
            render_context: crate::model::RenderContext {
                sample_rate: SampleRate::new(48_000).unwrap(),
                layout: ChannelLayout::Stereo,
                bpm: Bpm::new(120.0).unwrap(),
                requested_range: None,
                engine_version: "test".into(),
                random_seed: 0,
            },
            media_path: ProjectPath::new("assets/cache/revision.wav").unwrap(),
            frames: FrameCount(48_000),
        };
        asset.revisions.push(revision.clone());
        asset.current_revision_id = Some(revision.id);
        let mut first = Track::audio(project.root_composition_id, "First");
        let second = Track::audio(project.root_composition_id, "Second");
        first.clips.push(Clip::Audio(AudioClip::new(
            asset.id,
            beats(0.0),
            beats(1.0),
            SourceRange {
                start: Seconds::new(0.0).unwrap(),
                duration: Seconds::new(1.0).unwrap(),
            },
        )));
        let clip_id = first.clips[0].id();
        project.compositions[0]
            .track_ids
            .extend([first.id, second.id]);
        project.assets.push(asset);
        project.tracks.extend([first, second]);
        project.validate().unwrap();

        let before = project.clone();
        let mut history = EditHistory::default();
        history
            .apply(
                &mut project,
                &Transaction::new([
                    Command::MoveClip {
                        clip_id,
                        from_track_id: before.tracks[0].id,
                        to_track_id: before.tracks[1].id,
                    },
                    Command::ReorderCompositionTracks {
                        composition_id: before.root_composition_id,
                        from: 0,
                        to: 1,
                    },
                    Command::SetAssetCurrentRevision {
                        asset_id: before.assets[0].id,
                        revision_id: None,
                    },
                ]),
            )
            .unwrap();
        let after = project.clone();
        history.undo(&mut project).unwrap();
        assert_eq!(project, before);
        history.redo(&mut project).unwrap();
        assert_eq!(project, after);
    }

    #[test]
    fn failed_transaction_rolls_back_prior_deltas_without_cloning_payloads() {
        let mut project = project();
        let track = Track::audio(project.root_composition_id, "Track");
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        let before = project.clone();
        let track_id = project.tracks[0].id;
        let composition_id = project.root_composition_id;
        let mut history = EditHistory::default();
        let error = history.apply(
            &mut project,
            &Transaction::new([
                Command::SetProjectName {
                    name: "changed".into(),
                },
                Command::MoveTrack {
                    track_id,
                    composition_id,
                    index: 2,
                },
            ]),
        );
        assert!(matches!(error, Err(DomainError::IndexOutOfBounds { .. })));
        assert_eq!(project, before);
        assert_eq!(history.undo_len(), 0);
    }

    #[test]
    fn preset_commands_are_canonical_and_undoable() {
        let mut project = project();
        let instrument_id = InstrumentId::new();
        let track = Track::event(
            project.root_composition_id,
            "Sampler",
            Instrument::sampler("Initial", crate::model::Sampler::new(8).unwrap()),
        );
        let track_id = track.id;
        project.compositions[0].track_ids.push(track_id);
        project.tracks.push(track);
        let sampler_preset =
            SamplerPreset::new("Wide sampler", crate::model::Sampler::new(32).unwrap());
        let effect_preset = EffectPreset::new(
            "Quiet",
            ProcessorKind::Gain(GainParameters {
                gain_db: -12.0,
                ..GainParameters::default()
            }),
        );
        let processor_id = ProcessorId::new("preset_gain").unwrap();
        let transaction = Transaction::new([
            Command::ApplySamplerPreset {
                track_id,
                instrument_id,
                preset: sampler_preset,
            },
            Command::InsertEffectPreset {
                stack: ProcessorStack::CompositionOutput {
                    composition_id: project.root_composition_id,
                },
                index: 0,
                processor_id,
                preset: effect_preset,
            },
        ]);
        let json = serde_json::to_string(&transaction).unwrap();
        assert_eq!(
            serde_json::from_str::<Transaction>(&json).unwrap(),
            transaction
        );
        let before = project.clone();
        let mut history = EditHistory::default();
        history.apply(&mut project, &transaction).unwrap();
        let after = project.clone();
        history.undo(&mut project).unwrap();
        assert_eq!(project, before);
        history.redo(&mut project).unwrap();
        assert_eq!(project, after);
    }
}
