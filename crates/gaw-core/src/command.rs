//! Typed, atomic edits and project-wide integrity validation.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Display;
use std::num::NonZeroUsize;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::model::{
    AssetId, AssetRevisionId, AssetTempo, AudioAsset, AudioAssetDefinition, AudioAssetRevision,
    AudioTransform, AutomationLane, AutomationLaneId, AutomationTarget, AutomationUnit, Bpm, Clip,
    ClipId, Composition, CompositionId, Event, EventData, EventDataId, Instrument, InstrumentId,
    InstrumentKind, ModelError, Project, ProjectSettings, SampleRate, Seconds, SourceRange,
    TempoSync, Track, TrackId, TrackKind,
};
use crate::processors::{
    AutomationSupport, ParameterDescriptor, ParameterUnit, ParameterValueType, Processor,
    ProcessorId, ProcessorKind,
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
    Asset { asset_id: AssetId },
    CompositionOutput { composition_id: CompositionId },
    Track { track_id: TrackId },
    Clip { track_id: TrackId, clip_id: ClipId },
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
    SetProjectSampleRate {
        sample_rate: SampleRate,
    },
    SetProjectSettings {
        settings: ProjectSettings,
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
    /// Applies this command atomically and validates the resulting project.
    ///
    /// # Errors
    /// Returns a precondition or validation error without changing `project`.
    pub fn apply(&self, project: &mut Project) -> Result<(), DomainError> {
        Transaction::new([self.clone()]).apply(project)
    }

    #[allow(clippy::too_many_lines)]
    fn apply_unvalidated(&self, project: &mut Project) -> Result<(), DomainError> {
        match self {
            Self::SetProjectName { name } => project.name.clone_from(name),
            Self::SetProjectTempo { bpm } => project.bpm = *bpm,
            Self::SetProjectSampleRate { sample_rate } => project.sample_rate = *sample_rate,
            Self::SetProjectSettings { settings } => project.settings.clone_from(settings),

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
            Self::RemoveTrack { track_id } => {
                let composition_id = track(project, *track_id)?.composition_id;
                composition_mut(project, composition_id)?
                    .track_ids
                    .retain(|id| id != track_id);
                remove_by_id(&mut project.tracks, track_id, |v| &v.id, "track")?;
            }
            Self::MoveTrack {
                track_id,
                composition_id,
                index,
            } => {
                composition(project, *composition_id)?;
                let old_composition_id = track(project, *track_id)?.composition_id;
                let removed = {
                    let ids = &mut composition_mut(project, old_composition_id)?.track_ids;
                    let old_index = ids.iter().position(|id| id == track_id).ok_or_else(|| {
                        DomainError::DanglingReference {
                            from: old_composition_id.to_string(),
                            to: track_id.to_string(),
                        }
                    })?;
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
                track_mut(project, *track_id)?.clips.push(clip.clone());
            }
            Self::UpdateClip { track_id, clip } => {
                replace_by_id(
                    &mut track_mut(project, *track_id)?.clips,
                    clip.clone(),
                    Clip::id,
                    "clip",
                )?;
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
        ProcessorStack::Asset { asset_id } => match &mut asset_mut(project, asset_id)?.definition {
            AudioAssetDefinition::Processed { effects, .. } => Ok(effects),
            _ => Err(invalid(
                "stack",
                "only processed assets have an effect stack",
            )),
        },
        ProcessorStack::CompositionOutput { composition_id } => {
            Ok(&mut composition_mut(project, composition_id)?.output_effects)
        }
        ProcessorStack::Track { track_id } => Ok(&mut track_mut(project, track_id)?.effects),
        ProcessorStack::Clip { track_id, clip_id } => {
            let clip = track_mut(project, track_id)?
                .clips
                .iter_mut()
                .find(|value| value.id() == clip_id)
                .ok_or_else(|| not_found("clip", clip_id))?;
            match clip {
                Clip::Audio(value) => Ok(&mut value.effects),
                Clip::Composition(value) => Ok(&mut value.effects),
                Clip::Event(_) => Err(invalid("stack", "event clips are processed by their track")),
            }
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
                    Clip::Event(_) => &[],
                }),
        )
}

impl Validate for Project {
    #[allow(clippy::too_many_lines)]
    fn validate(&self) -> Result<(), DomainError> {
        if self.schema_version != crate::SCHEMA_VERSION {
            return Err(invalid("schema_version", "unsupported schema version"));
        }
        nonempty("project.name", &self.name)?;
        unique(self.assets.iter().map(|value| value.id), "asset")?;
        unique(self.event_data.iter().map(|value| value.id), "event data")?;
        unique(
            self.compositions.iter().map(|value| value.id),
            "composition",
        )?;
        unique(self.tracks.iter().map(|value| value.id), "track")?;
        unique(
            self.automation.iter().map(|value| value.id),
            "automation lane",
        )?;
        unique(
            self.tracks
                .iter()
                .flat_map(|value| &value.clips)
                .map(Clip::id),
            "clip",
        )?;
        unique(
            all_processors(self).map(|value| value.id.as_str().to_owned()),
            "processor",
        )?;

        composition(self, self.root_composition_id)?;
        let assets: BTreeMap<_, _> = self.assets.iter().map(|value| (value.id, value)).collect();
        let events: BTreeMap<_, _> = self
            .event_data
            .iter()
            .map(|value| (value.id, value))
            .collect();
        let compositions: BTreeMap<_, _> = self
            .compositions
            .iter()
            .map(|value| (value.id, value))
            .collect();
        let tracks: BTreeMap<_, _> = self.tracks.iter().map(|value| (value.id, value)).collect();
        let mut graph = BTreeMap::<String, Vec<String>>::new();

        let mut revisions = BTreeMap::new();
        for asset in &self.assets {
            asset.validate().map_err(model_error)?;
            for revision in &asset.revisions {
                if revisions.insert(revision.id, revision).is_some() {
                    return Err(already_exists("asset revision", revision.id));
                }
            }
        }

        let mut track_owners = BTreeMap::new();
        for composition in &self.compositions {
            nonempty("composition.name", &composition.name)?;
            unique(composition.track_ids.iter().copied(), "track reference")?;
            validate_processors(&composition.output_effects)?;
            graph.entry(composition_node(composition.id)).or_default();
            for id in &composition.track_ids {
                let owned = tracks.get(id).ok_or_else(|| dangling(composition.id, id))?;
                if owned.composition_id != composition.id {
                    return Err(DomainError::CrossBoundary {
                        from: composition.id.to_string(),
                        to: id.to_string(),
                    });
                }
                if let Some(other) = track_owners.insert(*id, composition.id) {
                    return Err(invalid(
                        "composition.track_ids",
                        format!("track {id} is owned by {other} and {}", composition.id),
                    ));
                }
            }
        }

        let mut instrument_owners = BTreeMap::new();
        let mut zone_ids = BTreeSet::new();
        let mut child_parents = BTreeMap::new();
        for track in &self.tracks {
            nonempty("track.name", &track.name)?;
            let owner = compositions
                .get(&track.composition_id)
                .ok_or_else(|| dangling(track.id, track.composition_id))?;
            if track_owners.get(&track.id) != Some(&track.composition_id) {
                return Err(dangling(
                    track.id,
                    format!("composition {} track_ids", owner.id),
                ));
            }
            match track.kind {
                TrackKind::Audio
                    if track.instrument.is_some()
                        || track
                            .clips
                            .iter()
                            .any(|clip| matches!(clip, Clip::Event(_))) =>
                {
                    return Err(invalid(
                        "track.kind",
                        "audio tracks accept only audio/composition clips and no instrument",
                    ));
                }
                TrackKind::Event
                    if track.instrument.is_none()
                        || track
                            .clips
                            .iter()
                            .any(|clip| !matches!(clip, Clip::Event(_))) =>
                {
                    return Err(invalid(
                        "track.kind",
                        "event tracks require an instrument and only event clips",
                    ));
                }
                _ => {}
            }
            validate_processors(&track.effects)?;
            if let Some(instrument) = &track.instrument {
                nonempty("instrument.name", &instrument.name)?;
                if instrument_owners
                    .insert(instrument.id, track.composition_id)
                    .is_some()
                {
                    return Err(already_exists("instrument", instrument.id));
                }
                let InstrumentKind::Sampler(sampler) = &instrument.kind;
                sampler.validate().map_err(model_error)?;
                for zone in &sampler.zones {
                    if !zone_ids.insert(zone.id) {
                        return Err(already_exists("sampler zone", zone.id));
                    }
                    let asset = assets
                        .get(&zone.asset_id)
                        .copied()
                        .ok_or_else(|| dangling(zone.id, zone.asset_id))?;
                    validate_source_range(asset, zone.source, "sampler_zone.source")?;
                    add_edge(
                        &mut graph,
                        composition_node(track.composition_id),
                        asset_node(zone.asset_id),
                    );
                }
            }
            for clip in &track.clips {
                validate_clip(
                    clip,
                    track,
                    owner,
                    &assets,
                    &events,
                    &compositions,
                    &mut graph,
                    &mut child_parents,
                )?;
            }
        }

        validate_events(&self.event_data)?;
        validate_assets(
            self,
            &assets,
            &events,
            &compositions,
            &instrument_owners,
            &mut graph,
        )?;
        validate_revisions(&revisions, &mut graph)?;
        validate_automation(self, &compositions, &tracks)?;
        detect_cycles(&graph)?;
        if child_parents.contains_key(&self.root_composition_id) {
            return Err(invalid(
                "root_composition_id",
                "root composition cannot be a child",
            ));
        }
        for composition in &self.compositions {
            if composition.id != self.root_composition_id
                && !child_parents.contains_key(&composition.id)
            {
                return Err(invalid(
                    "composition hierarchy",
                    format!("composition {} has no parent", composition.id),
                ));
            }
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_clip(
    clip: &Clip,
    track: &Track,
    owner: &Composition,
    assets: &BTreeMap<AssetId, &AudioAsset>,
    events: &BTreeMap<EventDataId, &EventData>,
    compositions: &BTreeMap<CompositionId, &Composition>,
    graph: &mut BTreeMap<String, Vec<String>>,
    child_parents: &mut BTreeMap<CompositionId, CompositionId>,
) -> Result<(), DomainError> {
    let (start, duration) = match clip {
        Clip::Audio(v) => (v.start, v.duration),
        Clip::Event(v) => (v.start, v.duration),
        Clip::Composition(v) => (v.start, v.duration),
    };
    if duration.value() <= 0.0 || start.value() + duration.value() > owner.length.value() {
        return Err(invalid(
            "clip.duration",
            "must be positive and fit within its composition",
        ));
    }
    match clip {
        Clip::Audio(value) => {
            let asset = assets
                .get(&value.asset_id)
                .copied()
                .ok_or_else(|| dangling(value.id, value.asset_id))?;
            validate_source_range(asset, value.source, "audio_clip.source")?;
            if value.tempo_sync != TempoSync::None && asset.tempo.is_none() {
                return Err(invalid(
                    "audio_clip.tempo_sync",
                    "repitch and stretch require an asset BPM",
                ));
            }
            let fades = value.fade_in.map_or(0.0, |v| v.duration.value())
                + value.fade_out.map_or(0.0, |v| v.duration.value());
            if fades > value.source.duration.value() {
                return Err(invalid(
                    "audio_clip.fades",
                    "combined fades exceed source duration",
                ));
            }
            validate_processors(&value.effects)?;
            add_edge(
                graph,
                composition_node(track.composition_id),
                asset_node(value.asset_id),
            );
        }
        Clip::Event(value) => {
            if !events.contains_key(&value.event_data_id) {
                return Err(dangling(value.id, value.event_data_id));
            }
        }
        Clip::Composition(value) => {
            let child = compositions
                .get(&value.composition_id)
                .ok_or_else(|| dangling(value.id, value.composition_id))?;
            if value.source_start.value() + value.duration.value() > child.length.value() {
                return Err(invalid(
                    "composition_clip",
                    "source range exceeds child length",
                ));
            }
            if let Some(other) = child_parents.insert(value.composition_id, track.composition_id)
                && other != track.composition_id
            {
                return Err(invalid(
                    "composition hierarchy",
                    format!("composition {} has multiple parents", value.composition_id),
                ));
            }
            validate_processors(&value.effects)?;
            add_edge(
                graph,
                composition_node(track.composition_id),
                composition_node(value.composition_id),
            );
        }
    }
    Ok(())
}

fn validate_events(values: &[EventData]) -> Result<(), DomainError> {
    for data in values {
        nonempty("event_data.name", &data.name)?;
        let mut previous = 0.0;
        for (index, event) in data.events.iter().enumerate() {
            if index != 0 && event.time().value() < previous {
                return Err(invalid("event_data.events", "must be ordered by time"));
            }
            previous = event.time().value();
            match event {
                Event::Note(note) if note.duration.value() <= 0.0 => {
                    return Err(invalid("note.duration", "must be positive"));
                }
                Event::Control(control) => nonempty("control.controller", &control.controller)?,
                Event::Note(_) | Event::PitchBend(_) => {}
            }
        }
    }
    Ok(())
}

fn validate_assets(
    project: &Project,
    assets: &BTreeMap<AssetId, &AudioAsset>,
    events: &BTreeMap<EventDataId, &EventData>,
    compositions: &BTreeMap<CompositionId, &Composition>,
    instruments: &BTreeMap<InstrumentId, CompositionId>,
    graph: &mut BTreeMap<String, Vec<String>>,
) -> Result<(), DomainError> {
    for asset in &project.assets {
        let from = asset_node(asset.id);
        graph.entry(from.clone()).or_default();
        match &asset.definition {
            AudioAssetDefinition::Imported(value) => {
                nonempty("asset.original_filename", &value.original_filename)?;
                nonempty("asset.content_hash", value.content_hash.as_str())?;
                if value.frames.0 == 0 {
                    return Err(invalid("asset.frames", "imported audio must not be empty"));
                }
            }
            AudioAssetDefinition::InstrumentGenerated {
                instrument_id,
                event_data_id,
            } => {
                let owner = instruments
                    .get(instrument_id)
                    .ok_or_else(|| dangling(asset.id, instrument_id))?;
                if !events.contains_key(event_data_id) {
                    return Err(dangling(asset.id, event_data_id));
                }
                add_edge(graph, from, composition_node(*owner));
            }
            AudioAssetDefinition::CompositionGenerated { composition_id } => {
                if !compositions.contains_key(composition_id) {
                    return Err(dangling(asset.id, composition_id));
                }
                add_edge(graph, from, composition_node(*composition_id));
            }
            AudioAssetDefinition::Processed {
                source_asset_id,
                transforms,
                effects,
            } => {
                let source = assets
                    .get(source_asset_id)
                    .copied()
                    .ok_or_else(|| dangling(asset.id, source_asset_id))?;
                for transform in transforms {
                    if let AudioTransform::Trim(range) = transform {
                        validate_source_range(source, *range, "audio_transform.trim")?;
                    }
                }
                validate_processors(effects)?;
                add_edge(graph, from, asset_node(*source_asset_id));
            }
            AudioAssetDefinition::Materialized { revision_id } => {
                if !asset
                    .revisions
                    .iter()
                    .any(|revision| revision.id == *revision_id)
                {
                    return Err(dangling(asset.id, revision_id));
                }
            }
        }
        if let Some(tempo) = asset.tempo
            && let Some(duration) = asset_duration_seconds(asset)
            && tempo.first_beat.value() > duration
        {
            return Err(invalid(
                "asset.tempo.first_beat",
                "must fall within the asset duration",
            ));
        }
    }
    Ok(())
}

fn validate_source_range(
    asset: &AudioAsset,
    range: SourceRange,
    field: &'static str,
) -> Result<(), DomainError> {
    if range.duration.value() <= 0.0 {
        return Err(invalid(field, "duration must be positive"));
    }
    if let Some(duration) = asset_duration_seconds(asset)
        && range.start.value() + range.duration.value() > duration
    {
        return Err(invalid(field, "range exceeds the asset duration"));
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn asset_duration_seconds(asset: &AudioAsset) -> Option<f64> {
    match &asset.definition {
        AudioAssetDefinition::Imported(imported) => {
            Some(imported.frames.0 as f64 / f64::from(imported.sample_rate.value()))
        }
        _ => asset.current_revision().map(|revision| {
            revision.frames.0 as f64 / f64::from(revision.render_context.sample_rate.value())
        }),
    }
}

fn validate_revisions(
    revisions: &BTreeMap<AssetRevisionId, &AudioAssetRevision>,
    graph: &mut BTreeMap<String, Vec<String>>,
) -> Result<(), DomainError> {
    for (id, revision) in revisions {
        nonempty("revision.content_hash", revision.content_hash.as_str())?;
        nonempty(
            "revision.definition_hash",
            revision.definition_hash.as_str(),
        )?;
        nonempty(
            "revision.engine_version",
            &revision.render_context.engine_version,
        )?;
        let from = revision_node(*id);
        graph.entry(from.clone()).or_default();
        for dependency in &revision.dependency_revision_ids {
            if !revisions.contains_key(dependency) {
                return Err(dangling(id, dependency));
            }
            add_edge(graph, from.clone(), revision_node(*dependency));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_automation(
    project: &Project,
    compositions: &BTreeMap<CompositionId, &Composition>,
    tracks: &BTreeMap<TrackId, &Track>,
) -> Result<(), DomainError> {
    for lane in &project.automation {
        lane.validate().map_err(model_error)?;
        nonempty("automation.name", &lane.name)?;
        let composition = compositions
            .get(&lane.composition_id)
            .ok_or_else(|| dangling(lane.id, lane.composition_id))?;
        if lane
            .points
            .iter()
            .any(|point| point.time.value() > composition.length.value())
        {
            return Err(invalid(
                "automation.points",
                "point lies past composition length",
            ));
        }
        if let Some(first) = lane.points.first()
            && lane
                .points
                .iter()
                .any(|point| point.value.unit() != first.value.unit())
        {
            return Err(invalid(
                "automation.points",
                "all values in a lane must have the same unit",
            ));
        }
        let processor_and_parameter = match &lane.target {
            AutomationTarget::AudioClipProcessor {
                track_id,
                clip_id,
                processor_id,
                parameter_id,
            } => {
                let track = automation_track(tracks, lane, *track_id)?;
                let clip = track
                    .clips
                    .iter()
                    .find(|v| v.id() == *clip_id)
                    .ok_or_else(|| dangling(lane.id, clip_id))?;
                let Clip::Audio(clip) = clip else {
                    return Err(invalid("automation.target", "target is not an audio clip"));
                };
                Some((processor(&clip.effects, processor_id)?, parameter_id))
            }
            AutomationTarget::CompositionClipProcessor {
                track_id,
                clip_id,
                processor_id,
                parameter_id,
            } => {
                let track = automation_track(tracks, lane, *track_id)?;
                let clip = track
                    .clips
                    .iter()
                    .find(|v| v.id() == *clip_id)
                    .ok_or_else(|| dangling(lane.id, clip_id))?;
                let Clip::Composition(clip) = clip else {
                    return Err(invalid(
                        "automation.target",
                        "target is not a composition clip",
                    ));
                };
                Some((processor(&clip.effects, processor_id)?, parameter_id))
            }
            AutomationTarget::TrackProcessor {
                track_id,
                processor_id,
                parameter_id,
            } => {
                let track = automation_track(tracks, lane, *track_id)?;
                Some((processor(&track.effects, processor_id)?, parameter_id))
            }
            AutomationTarget::CompositionOutputProcessor {
                processor_id,
                parameter_id,
            } => Some((
                processor(&composition.output_effects, processor_id)?,
                parameter_id,
            )),
            AutomationTarget::Instrument {
                track_id,
                instrument_id,
                parameter_id,
            } => {
                let track = automation_track(tracks, lane, *track_id)?;
                let instrument = track
                    .instrument
                    .as_ref()
                    .ok_or_else(|| dangling(lane.id, instrument_id))?;
                if instrument.id != *instrument_id {
                    return Err(dangling(lane.id, instrument_id));
                }
                validate_instrument_automation(instrument, parameter_id, lane)?;
                None
            }
        };
        if let Some((processor, parameter)) = processor_and_parameter {
            validate_automation_parameter(processor, parameter, lane)?;
        }
    }
    Ok(())
}

fn automation_track<'a>(
    tracks: &'a BTreeMap<TrackId, &Track>,
    lane: &AutomationLane,
    id: TrackId,
) -> Result<&'a Track, DomainError> {
    let track = tracks
        .get(&id)
        .copied()
        .ok_or_else(|| dangling(lane.id, id))?;
    if track.composition_id != lane.composition_id {
        return Err(DomainError::CrossBoundary {
            from: lane.composition_id.to_string(),
            to: id.to_string(),
        });
    }
    Ok(track)
}

fn validate_instrument_automation(
    instrument: &Instrument,
    parameter: &str,
    lane: &AutomationLane,
) -> Result<(), DomainError> {
    let InstrumentKind::Sampler(sampler) = &instrument.kind;
    let unit = if parameter == "output_gain_db" {
        AutomationUnit::Decibels
    } else {
        let mut path = parameter.split('.');
        let (Some("zones"), Some(zone_id), Some(name), None) =
            (path.next(), path.next(), path.next(), path.next())
        else {
            return Err(invalid(
                "automation.parameter_id",
                "unknown or discrete sampler parameter",
            ));
        };
        if !sampler
            .zones
            .iter()
            .any(|zone| zone.id.to_string() == zone_id)
        {
            return Err(invalid(
                "automation.parameter_id",
                format!("sampler zone {zone_id} does not exist"),
            ));
        }
        match name {
            "gain_db" => AutomationUnit::Decibels,
            "velocity_sensitivity" => AutomationUnit::Ratio,
            "attack_ms" | "release_ms" => AutomationUnit::Milliseconds,
            _ => {
                return Err(invalid(
                    "automation.parameter_id",
                    "unknown or discrete sampler zone parameter",
                ));
            }
        }
    };
    if lane.points.iter().all(|point| point.value.unit() == unit) {
        Ok(())
    } else {
        Err(invalid(
            "automation.points",
            format!("values for {parameter:?} must use {unit:?}"),
        ))
    }
}

fn processor<'a>(values: &'a [Processor], id: &ProcessorId) -> Result<&'a Processor, DomainError> {
    values
        .iter()
        .find(|value| value.id == *id)
        .ok_or_else(|| not_found("processor", id))
}

fn parameter_descriptor<'a>(kind: &'a ProcessorKind, id: &str) -> Option<&'a ParameterDescriptor> {
    let normalized = normalize_parameter_id(id);
    kind.parameter_descriptors()
        .iter()
        .find(|descriptor| descriptor.id == normalized)
}

fn validate_automation_parameter(
    processor: &Processor,
    id: &str,
    lane: &AutomationLane,
) -> Result<(), DomainError> {
    let descriptor = parameter_descriptor(&processor.kind, id).ok_or_else(|| {
        invalid(
            "automation.parameter_id",
            format!("{id:?} is not a parameter of {}", processor.kind.type_id()),
        )
    })?;
    if descriptor.automation != AutomationSupport::Continuous {
        return Err(invalid(
            "automation.parameter_id",
            format!("{id:?} is discrete or not automatable"),
        ));
    }
    validate_parameter_index(&processor.kind, id)?;
    for point in &lane.points {
        let unit = point.value.unit();
        let compatible = match descriptor.value_type {
            ParameterValueType::Time => {
                matches!(unit, AutomationUnit::Beats | AutomationUnit::Seconds)
            }
            ParameterValueType::Rate => {
                matches!(unit, AutomationUnit::Beats | AutomationUnit::Hertz)
            }
            _ => automation_unit(descriptor.unit) == Some(unit),
        };
        if !compatible {
            return Err(invalid(
                "automation.points",
                format!("unit {unit:?} is incompatible with parameter {id:?}"),
            ));
        }
        if let Some(range) = descriptor.range
            && !(range.minimum..=range.maximum).contains(&point.value.number())
        {
            return Err(invalid(
                "automation.points",
                format!("value for {id:?} is outside its valid range"),
            ));
        }
    }
    Ok(())
}

fn automation_unit(unit: ParameterUnit) -> Option<AutomationUnit> {
    match unit {
        ParameterUnit::Unitless | ParameterUnit::Ratio => Some(AutomationUnit::Number),
        ParameterUnit::Decibels | ParameterUnit::Lufs => Some(AutomationUnit::Decibels),
        ParameterUnit::Hertz => Some(AutomationUnit::Hertz),
        ParameterUnit::Milliseconds => Some(AutomationUnit::Milliseconds),
        ParameterUnit::Seconds => Some(AutomationUnit::Seconds),
        ParameterUnit::Beats => Some(AutomationUnit::Beats),
        ParameterUnit::Normalized | ParameterUnit::PhaseCycles => Some(AutomationUnit::Ratio),
        ParameterUnit::Bipolar => Some(AutomationUnit::Bipolar),
        ParameterUnit::Semitones => Some(AutomationUnit::Semitones),
        ParameterUnit::Cents => Some(AutomationUnit::Cents),
        ParameterUnit::Bits | ParameterUnit::Count => None,
    }
}

fn validate_parameter_index(kind: &ProcessorKind, id: &str) -> Result<(), DomainError> {
    let mut segments = id.split('.');
    let (Some(collection), Some(index)) = (segments.next(), segments.next()) else {
        return Ok(());
    };
    let Ok(index) = index.parse::<usize>() else {
        return Ok(());
    };
    let length = match (kind, collection) {
        (ProcessorKind::ParametricEq(parameters), "bands") => parameters.bands.len(),
        (ProcessorKind::RhythmicGate(parameters), "steps") => parameters.steps.len(),
        _ => return Ok(()),
    };
    if index < length {
        Ok(())
    } else {
        Err(invalid(
            "automation.parameter_id",
            format!("index {index} is outside {collection} length {length}"),
        ))
    }
}

fn normalize_parameter_id(id: &str) -> String {
    let parts: Vec<_> = id.split('.').collect();
    if let [head @ ("bands" | "steps"), index, rest @ ..] = parts.as_slice()
        && index.parse::<usize>().is_ok()
        && !rest.is_empty()
    {
        format!("{head}[].{}", rest.join("."))
    } else {
        id.to_owned()
    }
}

fn validate_processors(values: &[Processor]) -> Result<(), DomainError> {
    unique(
        values.iter().map(|value| value.id.as_str().to_owned()),
        "processor in stack",
    )?;
    for value in values {
        value
            .validate()
            .map_err(|error| invalid("processor", error))?;
    }
    Ok(())
}

fn unique<T: Ord + Display>(
    values: impl IntoIterator<Item = T>,
    entity: &'static str,
) -> Result<(), DomainError> {
    let mut seen = BTreeSet::new();
    for value in values {
        let display = value.to_string();
        if !seen.insert(value) {
            return Err(already_exists(entity, display));
        }
    }
    Ok(())
}

fn nonempty(field: &'static str, value: &str) -> Result<(), DomainError> {
    if value.trim().is_empty() {
        Err(invalid(field, "must not be empty"))
    } else {
        Ok(())
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

fn asset_node(id: AssetId) -> String {
    format!("asset:{id}")
}
fn composition_node(id: CompositionId) -> String {
    format!("composition:{id}")
}
fn revision_node(id: AssetRevisionId) -> String {
    format!("revision:{id}")
}

fn add_edge(graph: &mut BTreeMap<String, Vec<String>>, from: String, to: String) {
    graph.entry(from).or_default().push(to);
}

fn detect_cycles(graph: &BTreeMap<String, Vec<String>>) -> Result<(), DomainError> {
    fn visit(
        node: &str,
        graph: &BTreeMap<String, Vec<String>>,
        active: &mut Vec<String>,
        done: &mut BTreeSet<String>,
    ) -> Result<(), DomainError> {
        if let Some(index) = active.iter().position(|value| value == node) {
            let mut cycle = active[index..].to_vec();
            cycle.push(node.to_owned());
            return Err(DomainError::DependencyCycle {
                path: cycle.join(" -> "),
            });
        }
        if done.contains(node) {
            return Ok(());
        }
        active.push(node.to_owned());
        if let Some(next) = graph.get(node) {
            for dependency in next {
                visit(dependency, graph, active, done)?;
            }
        }
        active.pop();
        done.insert(node.to_owned());
        Ok(())
    }

    let mut active = Vec::new();
    let mut done = BTreeSet::new();
    for node in graph.keys() {
        visit(node, graph, &mut active, &mut done)?;
    }
    Ok(())
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

    /// Applies atomically without recording history.
    ///
    /// # Errors
    /// Returns a command precondition or final project validation error.
    pub fn apply(&self, project: &mut Project) -> Result<(), DomainError> {
        if self.commands.is_empty() {
            return Err(DomainError::EmptyTransaction);
        }
        let mut next = project.clone();
        for command in &self.commands {
            command.apply_unvalidated(&mut next)?;
        }
        next.validate()?;
        *project = next;
        Ok(())
    }
}

#[derive(Clone, Debug)]
struct HistoryEntry {
    before: Project,
    after: Project,
}

/// Bounded, in-memory transaction history. It is intentionally not serialized.
#[derive(Clone, Debug)]
pub struct EditHistory {
    limit: NonZeroUsize,
    undo: VecDeque<HistoryEntry>,
    redo: VecDeque<HistoryEntry>,
}

impl EditHistory {
    pub fn new(limit: NonZeroUsize) -> Self {
        Self {
            limit,
            undo: VecDeque::new(),
            redo: VecDeque::new(),
        }
    }

    /// Commits one validated transaction and records one undo entry.
    ///
    /// # Errors
    /// Returns a command precondition or final project validation error.
    pub fn apply(
        &mut self,
        project: &mut Project,
        transaction: &Transaction,
    ) -> Result<(), DomainError> {
        if transaction.commands.is_empty() {
            return Err(DomainError::EmptyTransaction);
        }
        let before = project.clone();
        let mut after = before.clone();
        for command in &transaction.commands {
            command.apply_unvalidated(&mut after)?;
        }
        after.validate()?;
        *project = after.clone();
        self.undo.push_back(HistoryEntry { before, after });
        if self.undo.len() > self.limit.get() {
            self.undo.pop_front();
        }
        self.redo.clear();
        Ok(())
    }

    /// Restores the snapshot before the latest transaction.
    ///
    /// # Errors
    /// Returns [`DomainError::NothingToUndo`] when history is empty.
    pub fn undo(&mut self, project: &mut Project) -> Result<(), DomainError> {
        let entry = self.undo.pop_back().ok_or(DomainError::NothingToUndo)?;
        *project = entry.before.clone();
        self.redo.push_back(entry);
        Ok(())
    }

    /// Restores the latest snapshot that was undone.
    ///
    /// # Errors
    /// Returns [`DomainError::NothingToRedo`] when redo history is empty.
    pub fn redo(&mut self, project: &mut Project) -> Result<(), DomainError> {
        let entry = self.redo.pop_back().ok_or(DomainError::NothingToRedo)?;
        *project = entry.after.clone();
        self.undo.push_back(entry);
        Ok(())
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    pub fn undo_len(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_len(&self) -> usize {
        self.redo.len()
    }
}

impl Default for EditHistory {
    fn default() -> Self {
        Self::new(NonZeroUsize::new(100).expect("100 is non-zero"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        AudioClip, AutomationCurve, AutomationPoint, AutomationValue, Beats, ChannelLayout,
        CompositionClip, ContentHash, Decibels, FrameCount, Hertz, ImportedAudio, ProjectPath,
    };
    use crate::processors::{GainParameters, ProcessorId};

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
}
