use super::{ChangeSource, Command, ProjectViewModel, Selection, Transaction};
use gaw_core::{Beats, Cents, Clip, ClipId, CompositionId, Event, EventDataId, NoteEvent, TrackId};

/// The stable destination and timeline mapping captured when a take begins.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MidiRecordingTarget {
    pub composition_id: CompositionId,
    pub track_id: TrackId,
    pub clip_id: ClipId,
    pub event_data_id: EventDataId,
    pub clip_start: f64,
    pub source_start: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RecordedMidiNote {
    /// Beat position relative to the visible clip start, before source offset.
    pub start: f64,
    pub duration: f64,
    pub pitch: u8,
    pub velocity: u8,
    pub cents: f64,
}

impl ProjectViewModel {
    pub(crate) fn keyboard_track_id(&self) -> Option<TrackId> {
        let (Selection::Track { track: index }
        | Selection::Clip { track: index, .. }
        | Selection::Effect { track: index, .. }
        | Selection::Sampler { track: index }) = self.selection
        else {
            return None;
        };
        let id = self.current_track_id(index)?;
        self.project
            .tracks
            .iter()
            .find(|track| {
                track.id == id
                    && track.kind == gaw_core::TrackKind::Event
                    && track.instrument.is_some()
            })
            .map(|track| track.id)
    }

    /// Recording requires an explicitly selected MIDI clip.
    pub(crate) fn keyboard_recording_target(&self) -> Option<MidiRecordingTarget> {
        let (track_index, clip_index, _) = self.selected_clip()?;
        let (track_id, clip_id) = self.clip_ids(track_index, clip_index)?;
        let track = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)?;
        let Clip::Event(clip) = track.clips.iter().find(|clip| clip.id() == clip_id)? else {
            return None;
        };
        Some(MidiRecordingTarget {
            composition_id: self.current_composition_id(),
            track_id,
            clip_id,
            event_data_id: clip.event_data_id,
            clip_start: clip.start.value(),
            source_start: clip.source_start.value(),
        })
    }

    /// Adds a complete take as one undoable edit, preserving existing MIDI events.
    #[allow(clippy::float_cmp)] // Source mapping must match the captured take exactly.
    pub(crate) fn record_keyboard_take(
        &mut self,
        target: &MidiRecordingTarget,
        notes: &[RecordedMidiNote],
    ) -> Result<(), String> {
        if notes.is_empty() {
            return Ok(());
        }
        let track = self
            .project
            .tracks
            .iter()
            .find(|track| {
                track.id == target.track_id && track.composition_id == target.composition_id
            })
            .ok_or("The recording track no longer exists")?;
        let Some(Clip::Event(clip)) = track.clips.iter().find(|clip| clip.id() == target.clip_id)
        else {
            return Err("The recording clip no longer exists".into());
        };
        if clip.event_data_id != target.event_data_id
            || clip.start.value() != target.clip_start
            || clip.source_start.value() != target.source_start
        {
            return Err("The recording clip moved or changed source during the take".into());
        }
        let mut clip = clip.clone();
        let mut event_data = self
            .project
            .event_data
            .iter()
            .find(|data| data.id == target.event_data_id)
            .cloned()
            .ok_or("The recording MIDI data no longer exists")?;
        let mut duration = clip.duration.value();
        for note in notes {
            if !note.start.is_finite()
                || note.start < 0.0
                || !note.duration.is_finite()
                || note.duration <= 0.0
            {
                return Err(
                    "Recorded notes require a finite nonnegative start and positive duration"
                        .into(),
                );
            }
            let mut event = NoteEvent::new(
                Beats::new(target.source_start + note.start).map_err(|error| error.to_string())?,
                Beats::new(note.duration).map_err(|error| error.to_string())?,
                note.pitch,
                note.velocity,
            )
            .map_err(|error| error.to_string())?;
            let tuning = Cents::new(note.cents).map_err(|error| error.to_string())?;
            event.tuning = (note.cents != 0.0).then_some(tuning);
            duration = duration.max(note.start + note.duration);
            event_data.events.push(Event::Note(event));
        }
        if !(target.clip_start + duration).is_finite()
            || !(target.source_start + duration).is_finite()
        {
            return Err("The recorded take is too long".into());
        }
        event_data.sort();
        validate_gated_overlaps(&self.project, &event_data, target, duration)?;
        let mut commands = vec![Command::UpdateEventData { event_data }];
        if duration > clip.duration.value() {
            clip.duration = Beats::new(duration).map_err(|error| error.to_string())?;
            let extended_clip = Clip::Event(clip);
            if (gaw_core::packed_clip_start(track, &extended_clip, Some(target.clip_id))
                - target.clip_start)
                .abs()
                > f64::EPSILON
            {
                return Err("Recording would overlap the next clip; move that clip to make room and save the take again".into());
            }
            commands.push(Command::UpdateClip {
                track_id: target.track_id,
                clip: extended_clip,
            });
        }
        let composition = self
            .project
            .compositions
            .iter()
            .find(|composition| composition.id == target.composition_id)
            .ok_or("The recording composition no longer exists")?;
        if target.clip_start + duration > composition.length.value() {
            super::extend_composition_for_drop(
                composition,
                target.clip_start,
                duration,
                self.project.time_signature.quarter_notes_per_bar(),
                &mut commands,
            );
        }
        self.commit(
            &Transaction::named("Record computer keyboard MIDI", commands),
            ChangeSource::Ui,
            &[target.clip_id.to_string(), target.event_data_id.to_string()],
            0.0,
        )
        .map_err(|error| error.to_string())
    }
}

/// Match the sampler renderer's frame-rounded overlap restriction, including
/// other clips sharing the edited MIDI asset and notes before a trimmed window.
fn validate_gated_overlaps(
    project: &gaw_core::Project,
    data: &gaw_core::EventData,
    target: &MidiRecordingTarget,
    duration: f64,
) -> Result<(), String> {
    let tempo = gaw_audio::Tempo::new(project.bpm.value(), project.sample_rate.value())
        .map_err(|error| error.to_string())?;
    let frames = |beats| -> Result<i64, String> {
        let beat = gaw_audio::Beat::new(beats).map_err(|error| error.to_string())?;
        tempo
            .frame_at(beat)
            .map(gaw_audio::Frame::get)
            .map_err(|error| error.to_string())
    };
    for track in &project.tracks {
        let Some(instrument) = &track.instrument else {
            continue;
        };
        let gaw_core::InstrumentKind::Sampler(sampler) = &instrument.kind;
        if !sampler
            .zones
            .iter()
            .any(|zone| zone.playback == gaw_core::SamplerPlayback::NoteGated)
        {
            continue;
        }
        for clip in &track.clips {
            let Clip::Event(clip) = clip else { continue };
            if clip.event_data_id != data.id {
                continue;
            }
            let duration = if clip.id == target.clip_id {
                duration
            } else {
                clip.duration.value()
            };
            let window_end = frames(clip.source_start.value())?.saturating_add(frames(duration)?);
            let mut windows = std::collections::HashMap::<u8, Vec<(i64, i64, bool)>>::new();
            for event in &data.events {
                let Event::Note(note) = event else { continue };
                let start = frames(note.start.value())?;
                if start >= window_end {
                    continue;
                }
                let end = start.saturating_add(frames(note.duration.value())?);
                let gated = sampler.zones.iter().any(|zone| {
                    zone.playback == gaw_core::SamplerPlayback::NoteGated
                        && (zone.note_range.low..=zone.note_range.high).contains(&note.note)
                        && (zone.velocity_range.low..=zone.velocity_range.high)
                            .contains(&note.velocity)
                });
                let previous = windows.entry(note.note.value()).or_default();
                if previous
                    .iter()
                    .any(|&(other_start, other_end, other_gated)| {
                        other_start < end && other_end > start && (gated || other_gated)
                    })
                {
                    return Err(format!(
                        "Note {} overlaps an existing note on a note-gated sampler. Remove the overlapping notes before saving this take again",
                        note.note.value()
                    ));
                }
                previous.push((start, end, gated));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // Exact beat offsets use binary-representable fixture values.
mod tests {
    use super::*;
    use crate::model::Intent;

    fn fixture() -> (ProjectViewModel, MidiRecordingTarget) {
        let mut vm = ProjectViewModel::demo();
        let start = vm.current_composition().length_beats;
        vm.apply(Intent::CreateMidiTrack { beat: start });
        let mut target = vm.keyboard_recording_target().unwrap();
        let track = vm
            .project
            .tracks
            .iter_mut()
            .find(|track| track.id == target.track_id)
            .unwrap();
        let Clip::Event(clip) = track.clips.first_mut().unwrap() else {
            unreachable!()
        };
        clip.source_start = Beats::new(8.0).unwrap();
        target.source_start = 8.0;
        (vm, target)
    }

    #[test]
    fn take_maps_trimmed_source_extends_and_undoes_atomically() {
        let (mut vm, target) = fixture();
        let data = vm
            .project
            .event_data
            .iter_mut()
            .find(|data| data.id == target.event_data_id)
            .unwrap();
        let existing = Event::Note(
            NoteEvent::new(Beats::new(8.0).unwrap(), Beats::new(0.5).unwrap(), 55, 80).unwrap(),
        );
        data.events.push(existing.clone());
        let before = vm.project.clone();
        vm.record_keyboard_take(
            &target,
            &[
                RecordedMidiNote {
                    start: 5.0,
                    duration: 2.0,
                    pitch: 64,
                    velocity: 100,
                    cents: 0.0,
                },
                RecordedMidiNote {
                    start: 0.5,
                    duration: 0.3,
                    pitch: 60,
                    velocity: 90,
                    cents: 0.0,
                },
            ],
        )
        .unwrap();
        let data = vm
            .project
            .event_data
            .iter()
            .find(|data| data.id == target.event_data_id)
            .unwrap();
        assert_eq!(data.events[0], existing);
        assert_eq!(data.events[1].time().value(), 8.5);
        assert_eq!(data.events[2].time().value(), 13.0);
        let track = vm
            .project
            .tracks
            .iter()
            .find(|track| track.id == target.track_id)
            .unwrap();
        assert_eq!(super::super::clip_duration(&track.clips[0]), 7.0);
        let composition = vm
            .project
            .compositions
            .iter()
            .find(|composition| composition.id == target.composition_id)
            .unwrap();
        assert!(composition.length.value() >= target.clip_start + 7.0);
        vm.apply(Intent::Undo(0.0));
        assert_eq!(vm.project, before);
    }

    #[test]
    fn seven_edo_take_preserves_fractional_tuning_through_save_and_undo() {
        let (mut vm, target) = fixture();
        let before = vm.project.clone();
        let cents = 1200.0 / 7.0 - 200.0;
        vm.record_keyboard_take(
            &target,
            &[RecordedMidiNote {
                start: 0.0,
                duration: 1.0,
                pitch: 62,
                velocity: 100,
                cents,
            }],
        )
        .unwrap();
        let encoded = serde_json::to_vec(vm.project()).unwrap();
        let reopened: gaw_core::Project = serde_json::from_slice(&encoded).unwrap();
        let data = reopened
            .event_data
            .iter()
            .find(|data| data.id == target.event_data_id)
            .unwrap();
        let Event::Note(note) = &data.events[0] else {
            panic!("recorded event must be a note")
        };
        assert_eq!(note.note.value(), 62);
        assert_eq!(note.tuning.unwrap().value(), cents);
        vm.apply(Intent::Undo(0.0));
        assert_eq!(vm.project, before);
    }

    #[test]
    fn invalid_tuning_does_not_partially_write_a_take() {
        let (mut vm, target) = fixture();
        let before = vm.project.clone();
        let note = RecordedMidiNote {
            start: 0.0,
            duration: 1.0,
            pitch: 60,
            velocity: 100,
            cents: 0.0,
        };
        assert!(
            vm.record_keyboard_take(
                &target,
                &[
                    note,
                    RecordedMidiNote {
                        start: 1.0,
                        cents: f64::NAN,
                        ..note
                    },
                ],
            )
            .is_err()
        );
        assert_eq!(vm.project, before);
    }

    #[test]
    fn piano_roll_edit_and_duplicate_preserve_recorded_tuning() {
        let (mut vm, target) = fixture();
        let cents = 1200.0 / 7.0 - 200.0;
        vm.record_keyboard_take(
            &target,
            &[RecordedMidiNote {
                start: 0.0,
                duration: 1.0,
                pitch: 62,
                velocity: 100,
                cents,
            }],
        )
        .unwrap();
        let (track, clip, selected) = vm.selected_clip().unwrap();
        let super::super::ClipKind::Event { notes } = &selected.kind else {
            panic!("recorded clip must contain notes")
        };
        let note = notes[0];
        assert_eq!(note.cents, cents);
        vm.apply(Intent::EditNote {
            track,
            clip,
            event_index: note.event_index,
            start: 0.25,
            length: 0.5,
            pitch: 74,
            velocity: 80,
        });
        vm.apply(Intent::AddNotes {
            track,
            clip,
            notes: vec![super::super::NoteInsert {
                start: 2.0,
                length: note.length,
                pitch: note.pitch,
                velocity: 100,
                cents: note.cents,
            }],
        });
        let data = vm
            .project
            .event_data
            .iter()
            .find(|data| data.id == target.event_data_id)
            .unwrap();
        assert_eq!(data.events.len(), 2);
        for event in &data.events {
            let Event::Note(note) = event else {
                panic!("expected note")
            };
            assert_eq!(note.tuning.unwrap().value(), cents);
        }
    }

    #[test]
    fn invalid_take_does_not_partially_write_and_empty_take_does_not_commit() {
        let (mut vm, target) = fixture();
        let before = vm.project.clone();
        let updates = vm.updates.len();
        vm.record_keyboard_take(&target, &[]).unwrap();
        assert_eq!(vm.updates.len(), updates);
        assert!(
            vm.record_keyboard_take(
                &target,
                &[
                    RecordedMidiNote {
                        start: 0.0,
                        duration: 1.0,
                        pitch: 60,
                        velocity: 100,
                        cents: 0.0,
                    },
                    RecordedMidiNote {
                        start: 1.0,
                        duration: f64::NAN,
                        pitch: 64,
                        velocity: 100,
                        cents: 0.0,
                    },
                ]
            )
            .is_err()
        );
        assert_eq!(vm.project, before);
    }

    #[test]
    fn target_survives_selection_changes_but_rejects_changed_clip_mapping() {
        let (mut vm, target) = fixture();
        vm.apply(Intent::Select(Selection::None));
        let notes = [RecordedMidiNote {
            start: 0.0,
            duration: 1.0,
            pitch: 60,
            velocity: 100,
            cents: 0.0,
        }];
        vm.record_keyboard_take(&target, &notes).unwrap();
        let before = vm.project.clone();
        let mut wrong_target = target;
        wrong_target.source_start = 0.0;
        assert!(vm.record_keyboard_take(&wrong_target, &notes).is_err());
        assert_eq!(vm.project, before);
    }

    #[test]
    fn monitoring_accepts_track_selection_but_recording_requires_a_clip() {
        let (mut vm, target) = fixture();
        let (track, _, _) = vm.selected_clip().unwrap();
        assert_eq!(vm.keyboard_track_id(), Some(target.track_id));
        vm.apply(Intent::Select(Selection::Track { track }));
        assert_eq!(vm.keyboard_track_id(), Some(target.track_id));
        assert!(vm.keyboard_recording_target().is_none());
        vm.apply(Intent::Select(Selection::None));
        assert!(vm.keyboard_track_id().is_none());
    }

    #[test]
    fn extending_into_another_clip_does_not_silently_move_the_recording() {
        let (mut vm, target) = fixture();
        let track = vm
            .project
            .tracks
            .iter_mut()
            .find(|track| track.id == target.track_id)
            .unwrap();
        track.clips.push(Clip::Event(gaw_core::EventClip::new(
            target.event_data_id,
            Beats::new(target.clip_start + 4.0).unwrap(),
            Beats::new(4.0).unwrap(),
        )));
        let before = vm.project.clone();
        assert!(
            vm.record_keyboard_take(
                &target,
                &[RecordedMidiNote {
                    start: 3.0,
                    duration: 2.0,
                    pitch: 60,
                    velocity: 100,
                    cents: 0.0,
                }]
            )
            .unwrap_err()
            .contains("overlap")
        );
        assert_eq!(vm.project, before);
    }

    #[test]
    fn gated_overdubs_reject_overlaps_but_one_shots_allow_them() {
        let (mut vm, target) = fixture();
        let (track, _, _) = vm.selected_clip().unwrap();
        vm.add_sampler_zone(track);
        let track = vm
            .project
            .tracks
            .iter_mut()
            .find(|track| track.id == target.track_id)
            .unwrap();
        let gaw_core::InstrumentKind::Sampler(sampler) =
            &mut track.instrument.as_mut().unwrap().kind;
        sampler.zones[0].playback = gaw_core::SamplerPlayback::NoteGated;
        let notes = [
            RecordedMidiNote {
                start: 0.0,
                duration: 1.0,
                pitch: 60,
                velocity: 100,
                cents: 0.0,
            },
            RecordedMidiNote {
                start: 0.5,
                duration: 1.0,
                pitch: 60,
                velocity: 100,
                cents: 0.0,
            },
        ];
        let before = vm.project.clone();
        assert!(
            vm.record_keyboard_take(&target, &notes)
                .unwrap_err()
                .contains("overlaps")
        );
        assert_eq!(vm.project, before);
        let track = vm
            .project
            .tracks
            .iter_mut()
            .find(|track| track.id == target.track_id)
            .unwrap();
        let gaw_core::InstrumentKind::Sampler(sampler) =
            &mut track.instrument.as_mut().unwrap().kind;
        sampler.zones[0].playback = gaw_core::SamplerPlayback::OneShot;
        vm.record_keyboard_take(&target, &notes).unwrap();
    }
}
