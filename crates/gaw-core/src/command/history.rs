//! Reversible edit deltas and bounded in-memory transaction history.

use super::{
    AssetFolder, AssetId, AssetRevisionId, AssetTempo, AudioAsset, AudioAssetRevision,
    AutomationLane, Bpm, Clip, ClipId, Command, Composition, CompositionId, Decibels, DomainError,
    EventData, Instrument, NonZeroUsize, Processor, ProcessorId, ProcessorStack, Project,
    ProjectSettings, SampleRate, TimeSignature, Track, TrackId, Transaction, Validate, VecDeque,
    asset, asset_mut, composition, composition_mut, dangling, not_found, processor_stack,
    processor_stack_mut, track, track_mut,
};

#[derive(Clone, Debug)]
pub(super) enum VecDelta<T> {
    Replace {
        index: usize,
        value: T,
    },
    Splice {
        index: usize,
        value: Option<T>,
    },
    Move {
        from: usize,
        to: usize,
        applied: bool,
    },
}

impl<T> VecDelta<T> {
    fn toggle(&mut self, values: &mut Vec<T>) {
        match self {
            Self::Replace { index, value } => std::mem::swap(&mut values[*index], value),
            Self::Splice { index, value } => {
                if let Some(value) = value.take() {
                    values.insert(*index, value);
                } else {
                    *value = Some(values.remove(*index));
                }
            }
            Self::Move { from, to, applied } => {
                let (source, destination) = if *applied { (*to, *from) } else { (*from, *to) };
                let value = values.remove(source);
                values.insert(destination, value);
                *applied = !*applied;
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum Delta {
    ProjectName(String),
    ProjectTempo(Bpm),
    ProjectTimeSignature(TimeSignature),
    ProjectMetronome(bool),
    ProjectMasterVolume(Decibels),
    ProjectSampleRate(SampleRate),
    ProjectSettings(ProjectSettings),
    AssetFolders(Vec<AssetFolder>),
    Assets(VecDelta<AudioAsset>),
    AssetTempo {
        asset_id: AssetId,
        value: Option<AssetTempo>,
    },
    AssetCurrentRevision {
        asset_id: AssetId,
        value: Option<AssetRevisionId>,
    },
    AssetRevisions {
        asset_id: AssetId,
        change: VecDelta<AudioAssetRevision>,
    },
    EventData(VecDelta<EventData>),
    Compositions(VecDelta<Composition>),
    CompositionTracks {
        composition_id: CompositionId,
        change: VecDelta<TrackId>,
    },
    Tracks(VecDelta<Track>),
    TrackVolume {
        index: usize,
        value: f32,
    },
    TrackComposition {
        track_id: TrackId,
        value: CompositionId,
    },
    TrackInstrument {
        track_id: TrackId,
        value: Option<Instrument>,
    },
    Clips {
        track_id: TrackId,
        change: VecDelta<Clip>,
    },
    MoveClip {
        clip_id: ClipId,
        from_track_id: TrackId,
        from: usize,
        to_track_id: TrackId,
        to: usize,
        applied: bool,
    },
    Processors {
        stack: ProcessorStack,
        change: VecDelta<Processor>,
    },
    Automation(VecDelta<AutomationLane>),
}

impl Delta {
    fn toggle(&mut self, project: &mut Project) {
        match self {
            Self::ProjectName(value) => std::mem::swap(&mut project.name, value),
            Self::ProjectTempo(value) => std::mem::swap(&mut project.bpm, value),
            Self::ProjectTimeSignature(value) => {
                std::mem::swap(&mut project.time_signature, value);
            }
            Self::ProjectMetronome(value) => {
                std::mem::swap(&mut project.settings.metronome_enabled, value);
            }
            Self::ProjectMasterVolume(value) => {
                std::mem::swap(&mut project.settings.master_volume, value);
            }
            Self::ProjectSampleRate(value) => std::mem::swap(&mut project.sample_rate, value),
            Self::ProjectSettings(value) => std::mem::swap(&mut project.settings, value),
            Self::AssetFolders(value) => std::mem::swap(&mut project.asset_folders, value),
            Self::Assets(change) => change.toggle(&mut project.assets),
            Self::AssetTempo { asset_id, value } => {
                std::mem::swap(
                    &mut asset_mut(project, *asset_id).expect("asset exists").tempo,
                    value,
                );
            }
            Self::AssetCurrentRevision { asset_id, value } => {
                std::mem::swap(
                    &mut asset_mut(project, *asset_id)
                        .expect("asset exists")
                        .current_revision_id,
                    value,
                );
            }
            Self::AssetRevisions { asset_id, change } => change.toggle(
                &mut asset_mut(project, *asset_id)
                    .expect("asset exists")
                    .revisions,
            ),
            Self::EventData(change) => change.toggle(&mut project.event_data),
            Self::Compositions(change) => change.toggle(&mut project.compositions),
            Self::CompositionTracks {
                composition_id,
                change,
            } => change.toggle(
                &mut composition_mut(project, *composition_id)
                    .expect("composition exists")
                    .track_ids,
            ),
            Self::Tracks(change) => change.toggle(&mut project.tracks),
            Self::TrackVolume { index, value } => {
                std::mem::swap(&mut project.tracks[*index].volume_db, value);
            }
            Self::TrackComposition { track_id, value } => std::mem::swap(
                &mut track_mut(project, *track_id)
                    .expect("track exists")
                    .composition_id,
                value,
            ),
            Self::TrackInstrument { track_id, value } => std::mem::swap(
                &mut track_mut(project, *track_id)
                    .expect("track exists")
                    .instrument,
                value,
            ),
            Self::Clips { track_id, change } => {
                change.toggle(&mut track_mut(project, *track_id).expect("track exists").clips);
            }
            Self::MoveClip {
                clip_id,
                from_track_id,
                from,
                to_track_id,
                to,
                applied,
            } => {
                let (source_track, source_index, destination_track, destination_index) = if *applied
                {
                    (*to_track_id, *to, *from_track_id, *from)
                } else {
                    (*from_track_id, *from, *to_track_id, *to)
                };
                let clip = track_mut(project, source_track)
                    .expect("source track exists")
                    .clips
                    .remove(source_index);
                debug_assert_eq!(clip.id(), *clip_id);
                track_mut(project, destination_track)
                    .expect("destination track exists")
                    .clips
                    .insert(destination_index, clip);
                *applied = !*applied;
            }
            Self::Processors { stack, change } => {
                change.toggle(processor_stack_mut(project, stack).expect("processor stack exists"));
            }
            Self::Automation(change) => change.toggle(&mut project.automation),
        }
    }
}

fn position<T>(
    values: &[T],
    predicate: impl Fn(&T) -> bool,
    error: DomainError,
) -> Result<usize, DomainError> {
    values.iter().position(predicate).ok_or(error)
}

#[allow(clippy::too_many_lines)]
fn deltas_for(command: &Command, project: &Project) -> Result<Vec<Delta>, DomainError> {
    let deltas = match command {
        Command::SetProjectName { .. } => vec![Delta::ProjectName(project.name.clone())],
        Command::SetProjectTempo { .. } => vec![Delta::ProjectTempo(project.bpm)],
        Command::SetProjectTimeSignature { .. } => {
            vec![Delta::ProjectTimeSignature(project.time_signature)]
        }
        Command::SetProjectMetronome { .. } => {
            vec![Delta::ProjectMetronome(project.settings.metronome_enabled)]
        }
        Command::SetProjectMasterVolume { .. } => {
            vec![Delta::ProjectMasterVolume(project.settings.master_volume)]
        }
        Command::SetProjectSampleRate { .. } => {
            vec![Delta::ProjectSampleRate(project.sample_rate)]
        }
        Command::SetProjectSettings { .. } => {
            vec![Delta::ProjectSettings(project.settings.clone())]
        }
        Command::SetAssetFolders { .. } => {
            vec![Delta::AssetFolders(project.asset_folders.clone())]
        }
        Command::AddAsset { .. } => vec![Delta::Assets(VecDelta::Splice {
            index: project.assets.len(),
            value: None,
        })],
        Command::UpdateAsset { asset: value } => {
            let index = position(
                &project.assets,
                |old| old.id == value.id,
                not_found("asset", value.id),
            )?;
            vec![Delta::Assets(VecDelta::Replace {
                index,
                value: project.assets[index].clone(),
            })]
        }
        Command::SetAssetTempo { asset_id, .. }
        | Command::SetAssetBpm { asset_id, .. }
        | Command::SetAssetFirstBeat { asset_id, .. } => vec![Delta::AssetTempo {
            asset_id: *asset_id,
            value: asset(project, *asset_id)?.tempo,
        }],
        Command::AddAssetRevision { asset_id, .. } => vec![Delta::AssetRevisions {
            asset_id: *asset_id,
            change: VecDelta::Splice {
                index: asset(project, *asset_id)?.revisions.len(),
                value: None,
            },
        }],
        Command::SetAssetCurrentRevision { asset_id, .. } => {
            vec![Delta::AssetCurrentRevision {
                asset_id: *asset_id,
                value: asset(project, *asset_id)?.current_revision_id,
            }]
        }
        Command::RemoveAsset { asset_id } => {
            let index = position(
                &project.assets,
                |value| value.id == *asset_id,
                not_found("asset", asset_id),
            )?;
            vec![
                Delta::AssetFolders(project.asset_folders.clone()),
                Delta::Assets(VecDelta::Splice {
                    index,
                    value: Some(project.assets[index].clone()),
                }),
            ]
        }
        Command::AddEventData { .. } => vec![Delta::EventData(VecDelta::Splice {
            index: project.event_data.len(),
            value: None,
        })],
        Command::UpdateEventData { event_data } => {
            let index = position(
                &project.event_data,
                |old| old.id == event_data.id,
                not_found("event data", event_data.id),
            )?;
            vec![Delta::EventData(VecDelta::Replace {
                index,
                value: project.event_data[index].clone(),
            })]
        }
        Command::RemoveEventData { event_data_id } => {
            let index = position(
                &project.event_data,
                |value| value.id == *event_data_id,
                not_found("event data", event_data_id),
            )?;
            vec![
                Delta::AssetFolders(project.asset_folders.clone()),
                Delta::EventData(VecDelta::Splice {
                    index,
                    value: Some(project.event_data[index].clone()),
                }),
            ]
        }
        Command::AddComposition { .. } => vec![Delta::Compositions(VecDelta::Splice {
            index: project.compositions.len(),
            value: None,
        })],
        Command::UpdateComposition { composition: value } => {
            let index = position(
                &project.compositions,
                |old| old.id == value.id,
                not_found("composition", value.id),
            )?;
            vec![Delta::Compositions(VecDelta::Replace {
                index,
                value: project.compositions[index].clone(),
            })]
        }
        Command::RemoveComposition { composition_id } => {
            let index = position(
                &project.compositions,
                |value| value.id == *composition_id,
                not_found("composition", composition_id),
            )?;
            vec![Delta::Compositions(VecDelta::Splice {
                index,
                value: Some(project.compositions[index].clone()),
            })]
        }
        Command::ReorderCompositionTracks {
            composition_id,
            from,
            to,
        } => vec![Delta::CompositionTracks {
            composition_id: *composition_id,
            change: VecDelta::Move {
                from: *from,
                to: *to,
                applied: true,
            },
        }],
        Command::AddTrack {
            track: value,
            index,
        } => vec![
            Delta::CompositionTracks {
                composition_id: value.composition_id,
                change: VecDelta::Splice {
                    index: *index,
                    value: None,
                },
            },
            Delta::Tracks(VecDelta::Splice {
                index: project.tracks.len(),
                value: None,
            }),
        ],
        Command::UpdateTrack { track: value } => {
            let index = position(
                &project.tracks,
                |old| old.id == value.id,
                not_found("track", value.id),
            )?;
            vec![Delta::Tracks(VecDelta::Replace {
                index,
                value: project.tracks[index].clone(),
            })]
        }
        Command::SetTrackVolume { track_id, .. } => {
            let index = position(
                &project.tracks,
                |track| track.id == *track_id,
                not_found("track", track_id),
            )?;
            vec![Delta::TrackVolume {
                index,
                value: project.tracks[index].volume_db,
            }]
        }
        Command::RemoveTrack { track_id } => {
            let track_index = position(
                &project.tracks,
                |value| value.id == *track_id,
                not_found("track", track_id),
            )?;
            let value = &project.tracks[track_index];
            let composition = composition(project, value.composition_id)?;
            let composition_index = position(
                &composition.track_ids,
                |id| id == track_id,
                dangling(value.composition_id, track_id),
            )?;
            vec![
                Delta::CompositionTracks {
                    composition_id: value.composition_id,
                    change: VecDelta::Splice {
                        index: composition_index,
                        value: Some(*track_id),
                    },
                },
                Delta::Tracks(VecDelta::Splice {
                    index: track_index,
                    value: Some(value.clone()),
                }),
            ]
        }
        Command::MoveTrack {
            track_id,
            composition_id,
            index,
        } => {
            let value = track(project, *track_id)?;
            let source = composition(project, value.composition_id)?;
            let from = position(
                &source.track_ids,
                |id| id == track_id,
                dangling(value.composition_id, track_id),
            )?;
            vec![
                Delta::CompositionTracks {
                    composition_id: value.composition_id,
                    change: VecDelta::Splice {
                        index: from,
                        value: Some(*track_id),
                    },
                },
                Delta::CompositionTracks {
                    composition_id: *composition_id,
                    change: VecDelta::Splice {
                        index: *index,
                        value: None,
                    },
                },
                Delta::TrackComposition {
                    track_id: *track_id,
                    value: value.composition_id,
                },
            ]
        }
        Command::AddClip { track_id, .. } => vec![Delta::Clips {
            track_id: *track_id,
            change: VecDelta::Splice {
                index: track(project, *track_id)?.clips.len(),
                value: None,
            },
        }],
        Command::UpdateClip { track_id, clip } => {
            let clips = &track(project, *track_id)?.clips;
            let index = position(
                clips,
                |value| value.id() == clip.id(),
                not_found("clip", clip.id()),
            )?;
            vec![Delta::Clips {
                track_id: *track_id,
                change: VecDelta::Replace {
                    index,
                    value: clips[index].clone(),
                },
            }]
        }
        Command::RemoveClip { track_id, clip_id } => {
            let clips = &track(project, *track_id)?.clips;
            let index = position(
                clips,
                |value| value.id() == *clip_id,
                not_found("clip", clip_id),
            )?;
            vec![Delta::Clips {
                track_id: *track_id,
                change: VecDelta::Splice {
                    index,
                    value: Some(clips[index].clone()),
                },
            }]
        }
        Command::MoveClip {
            clip_id,
            from_track_id,
            to_track_id,
        } => {
            track(project, *to_track_id)?;
            let clips = &track(project, *from_track_id)?.clips;
            let from = position(
                clips,
                |value| value.id() == *clip_id,
                not_found("clip", clip_id),
            )?;
            let to = track(project, *to_track_id)?.clips.len()
                - usize::from(from_track_id == to_track_id);
            vec![Delta::MoveClip {
                clip_id: *clip_id,
                from_track_id: *from_track_id,
                from,
                to_track_id: *to_track_id,
                to,
                applied: true,
            }]
        }
        Command::SetTrackInstrument { track_id, .. }
        | Command::ApplySamplerPreset { track_id, .. } => vec![Delta::TrackInstrument {
            track_id: *track_id,
            value: track(project, *track_id)?.instrument.clone(),
        }],
        Command::InsertProcessor { stack, index, .. }
        | Command::InsertEffectPreset { stack, index, .. } => vec![Delta::Processors {
            stack: stack.clone(),
            change: VecDelta::Splice {
                index: *index,
                value: None,
            },
        }],
        Command::UpdateProcessor {
            stack,
            processor: value,
        } => {
            let processors = processor_stack(project, stack)?;
            let index = position(
                processors,
                |old| old.id == value.id,
                not_found("processor", &value.id),
            )?;
            vec![Delta::Processors {
                stack: stack.clone(),
                change: VecDelta::Replace {
                    index,
                    value: processors[index].clone(),
                },
            }]
        }
        Command::ApplyEffectPreset {
            stack,
            processor_id,
            ..
        } => {
            let processors = processor_stack(project, stack)?;
            let index = position(
                processors,
                |old| old.id == *processor_id,
                not_found("processor", processor_id),
            )?;
            vec![Delta::Processors {
                stack: stack.clone(),
                change: VecDelta::Replace {
                    index,
                    value: processors[index].clone(),
                },
            }]
        }
        Command::RemoveProcessor {
            stack,
            processor_id,
        } => {
            let processors = processor_stack(project, stack)?;
            let index = position(
                processors,
                |value| value.id == *processor_id,
                not_found("processor", processor_id),
            )?;
            vec![Delta::Processors {
                stack: stack.clone(),
                change: VecDelta::Splice {
                    index,
                    value: Some(processors[index].clone()),
                },
            }]
        }
        Command::ReorderProcessor { stack, from, to } => vec![Delta::Processors {
            stack: stack.clone(),
            change: VecDelta::Move {
                from: *from,
                to: *to,
                applied: true,
            },
        }],
        Command::AddAutomation { .. } => vec![Delta::Automation(VecDelta::Splice {
            index: project.automation.len(),
            value: None,
        })],
        Command::UpdateAutomation { lane } => {
            let index = position(
                &project.automation,
                |old| old.id == lane.id,
                not_found("automation lane", lane.id),
            )?;
            vec![Delta::Automation(VecDelta::Replace {
                index,
                value: project.automation[index].clone(),
            })]
        }
        Command::RemoveAutomation { lane_id } => {
            let index = position(
                &project.automation,
                |value| value.id == *lane_id,
                not_found("automation lane", lane_id),
            )?;
            vec![Delta::Automation(VecDelta::Splice {
                index,
                value: Some(project.automation[index].clone()),
            })]
        }
    };
    Ok(deltas)
}

fn rollback(project: &mut Project, deltas: &mut [Delta]) {
    for delta in deltas.iter_mut().rev() {
        delta.toggle(project);
    }
}

pub(super) fn apply_transaction<'a>(
    project: &mut Project,
    commands: impl IntoIterator<Item = &'a Command>,
) -> Result<Vec<Delta>, DomainError> {
    let commands: Vec<_> = commands.into_iter().collect();
    if commands.is_empty() {
        return Err(DomainError::EmptyTransaction);
    }
    let mut deltas = Vec::with_capacity(commands.len());
    for command in commands {
        let mut command_deltas = match deltas_for(command, project) {
            Ok(deltas) => deltas,
            Err(error) => {
                rollback(project, &mut deltas);
                return Err(error);
            }
        };
        if let Err(error) = command.apply_unvalidated(project) {
            rollback(project, &mut deltas);
            return Err(error);
        }
        deltas.append(&mut command_deltas);
    }
    if let Err(error) = project.validate() {
        rollback(project, &mut deltas);
        return Err(error);
    }
    Ok(deltas)
}

#[derive(Clone, Debug)]
struct HistoryEntry {
    deltas: Vec<Delta>,
    affects_render: bool,
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
        self.apply_with_coalescing(project, transaction, None)
    }

    /// Commits a continuing processor gesture as part of its preceding undo entry.
    /// Only consecutive transactions containing one replacement of the same
    /// processor, stack, and index are combined. Other edits keep separate entries.
    /// Call [`Self::apply`] for the first edit of each gesture.
    ///
    /// Coalescing happens before history trimming, including at a capacity of one.
    /// An existing redo branch prevents merging with an older gesture.
    ///
    /// # Errors
    /// Returns a command precondition or final project validation error.
    pub fn apply_coalescing_processor_update(
        &mut self,
        project: &mut Project,
        transaction: &Transaction,
        stack: &ProcessorStack,
        processor_id: &ProcessorId,
    ) -> Result<(), DomainError> {
        self.apply_with_coalescing(project, transaction, Some((stack, processor_id)))
    }

    fn apply_with_coalescing(
        &mut self,
        project: &mut Project,
        transaction: &Transaction,
        target: Option<(&ProcessorStack, &ProcessorId)>,
    ) -> Result<(), DomainError> {
        let deltas = apply_transaction(project, transaction.commands.iter())?;
        let entry = HistoryEntry {
            deltas,
            affects_render: transaction.affects_render(),
        };
        let can_merge = self.redo.is_empty()
            && target.is_some_and(|(stack, processor_id)| {
                self.undo.back().is_some_and(|previous| {
                    let replacement_index = |entry: &HistoryEntry| match entry.deltas.as_slice() {
                        [
                            Delta::Processors {
                                stack: prior_stack,
                                change: VecDelta::Replace { index, value },
                            },
                        ] if prior_stack == stack && &value.id == processor_id => Some(*index),
                        _ => None,
                    };
                    matches!(
                        (replacement_index(previous), replacement_index(&entry)),
                        (Some(prior), Some(next)) if prior == next
                    )
                })
            });
        if can_merge {
            // Keeping the original value lets undo swap it with the final value;
            // that same delta then restores the final value on redo.
            self.undo
                .back_mut()
                .expect("matching history entry")
                .affects_render |= entry.affects_render;
        } else {
            self.undo.push_back(entry);
        }
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
        let mut entry = self.undo.pop_back().ok_or(DomainError::NothingToUndo)?;
        rollback(project, &mut entry.deltas);
        self.redo.push_back(entry);
        Ok(())
    }

    pub fn undo_affects_render(&self) -> Option<bool> {
        self.undo.back().map(|entry| entry.affects_render)
    }

    /// Restores the latest snapshot that was undone.
    ///
    /// # Errors
    /// Returns [`DomainError::NothingToRedo`] when redo history is empty.
    pub fn redo(&mut self, project: &mut Project) -> Result<(), DomainError> {
        let mut entry = self.redo.pop_back().ok_or(DomainError::NothingToRedo)?;
        for delta in &mut entry.deltas {
            delta.toggle(project);
        }
        self.undo.push_back(entry);
        Ok(())
    }

    pub fn redo_affects_render(&self) -> Option<bool> {
        self.redo.back().map(|entry| entry.affects_render)
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
    use crate::processors::{GainParameters, ProcessorKind};

    fn fixture() -> (Project, ProcessorStack, Processor) {
        let mut project = Project::new(
            "Original",
            Bpm::new(120.0).unwrap(),
            SampleRate::new(48_000).unwrap(),
        );
        let stack = ProcessorStack::CompositionOutput {
            composition_id: project.root_composition_id,
        };
        let processor = Processor::new(
            ProcessorId::new("gain").unwrap(),
            ProcessorKind::Gain(GainParameters::default()),
        );
        project.compositions[0]
            .output_effects
            .push(processor.clone());
        (project, stack, processor)
    }

    fn update(stack: &ProcessorStack, processor: &Processor, gain_db: f32) -> Command {
        let mut processor = processor.clone();
        processor.kind = ProcessorKind::Gain(GainParameters {
            gain_db,
            ..GainParameters::default()
        });
        Command::UpdateProcessor {
            stack: stack.clone(),
            processor,
        }
    }

    #[test]
    fn processor_gesture_preserves_original_and_final_at_capacity() {
        for limit in [1, 3] {
            let (mut project, stack, processor) = fixture();
            let mut history = EditHistory::new(NonZeroUsize::new(limit).unwrap());
            for name in ["First", "Second"] {
                history
                    .apply(
                        &mut project,
                        &Transaction::new([Command::SetProjectName { name: name.into() }]),
                    )
                    .unwrap();
            }
            history
                .apply(
                    &mut project,
                    &Transaction::new([update(&stack, &processor, 1.0)]),
                )
                .unwrap();
            for gain in 2_i16..=10 {
                history
                    .apply_coalescing_processor_update(
                        &mut project,
                        &Transaction::new([update(&stack, &processor, f32::from(gain))]),
                        &stack,
                        &processor.id,
                    )
                    .unwrap();
                assert_eq!(history.undo_len(), limit);
            }
            let final_processor = project.compositions[0].output_effects[0].clone();
            assert_eq!(history.undo_affects_render(), Some(true));
            history.undo(&mut project).unwrap();
            assert_eq!(project.compositions[0].output_effects[0], processor);
            history.redo(&mut project).unwrap();
            assert_eq!(project.compositions[0].output_effects[0], final_processor);
            history.undo(&mut project).unwrap();
            if limit == 3 {
                history.undo(&mut project).unwrap();
                assert_eq!(project.name, "First");
                history.undo(&mut project).unwrap();
                assert_eq!(project.name, "Original");
            }
            assert_eq!(history.undo_len(), 0);
        }
    }

    #[test]
    fn processor_coalescing_refuses_unrelated_or_mixed_transactions() {
        for scenario in 0..5 {
            let (mut project, stack, processor) = fixture();
            let mut other = processor.clone();
            other.id = ProcessorId::new("other").unwrap();
            project.compositions[0].output_effects.push(other.clone());
            let mut history = EditHistory::default();
            let commands = match scenario {
                0 => vec![Command::SetProjectName {
                    name: "Renamed".into(),
                }],
                1 => vec![update(&stack, &other, 3.0)],
                2 => vec![
                    update(&stack, &processor, 3.0),
                    Command::SetProjectName {
                        name: "Renamed".into(),
                    },
                ],
                3 => vec![Command::ReorderProcessor {
                    stack: stack.clone(),
                    from: 0,
                    to: 1,
                }],
                _ => vec![update(&stack, &processor, 3.0)],
            };
            history
                .apply(&mut project, &Transaction::new(commands))
                .unwrap();
            let requested_id = if scenario == 4 {
                &other.id
            } else {
                &processor.id
            };
            history
                .apply_coalescing_processor_update(
                    &mut project,
                    &Transaction::new([update(&stack, &processor, 6.0)]),
                    &stack,
                    requested_id,
                )
                .unwrap();
            assert_eq!(history.undo_len(), 2, "scenario {scenario}");
            history.undo(&mut project).unwrap();
            history.undo(&mut project).unwrap();
            assert_eq!(project.name, "Original");
            assert_eq!(
                project.compositions[0].output_effects,
                vec![processor, other]
            );
        }
    }

    #[test]
    fn processor_coalescing_does_not_cross_redo_branch_or_gesture_boundary() {
        let (mut project, stack, processor) = fixture();
        let mut history = EditHistory::default();
        for gain in [1.0, 2.0] {
            history
                .apply(
                    &mut project,
                    &Transaction::new([update(&stack, &processor, gain)]),
                )
                .unwrap();
        }
        assert_eq!(history.undo_len(), 2);
        history.undo(&mut project).unwrap();
        history
            .apply_coalescing_processor_update(
                &mut project,
                &Transaction::new([update(&stack, &processor, 6.0)]),
                &stack,
                &processor.id,
            )
            .unwrap();
        assert_eq!(history.undo_len(), 2);
        assert_eq!(history.redo_len(), 0);
        history.undo(&mut project).unwrap();
        let Command::UpdateProcessor {
            processor: expected,
            ..
        } = update(&stack, &processor, 1.0)
        else {
            unreachable!();
        };
        assert_eq!(project.compositions[0].output_effects[0], expected);
    }
}
