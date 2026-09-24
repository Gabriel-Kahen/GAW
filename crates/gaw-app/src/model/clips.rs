use super::{
    AudioClipEdit, BTreeSet, ClipClipboard, Command, Intent, NoteEdit, ProjectViewModel, Selection,
    Transaction, clip_dependencies_exist, clip_duration, clip_is_compatible_with_track,
    clone_clip_automation, extend_composition_for_drop, fresh_clip_identity,
    is_clip_automation_target, set_clip_start,
};

impl ProjectViewModel {
    #[allow(clippy::too_many_lines)]
    /// Edits the selected audio placement through the canonical command engine.
    ///
    /// # Panics
    /// Panics if internal clip timing violates the canonical model.
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

    #[cfg(test)]
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
    pub(super) fn edit_clip_timing(
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
        let requested_start = gaw_core::Beats::new(f64::from(start)).expect("finite start");
        let requested_duration = gaw_core::Beats::new(f64::from(length)).expect("finite duration");
        let mut timing_candidate = clip.clone();
        match &mut timing_candidate {
            gaw_core::Clip::Audio(value) => {
                value.start = requested_start;
                value.duration = requested_duration;
            }
            gaw_core::Clip::Event(value) => {
                value.start = requested_start;
                value.duration = requested_duration;
            }
            gaw_core::Clip::Composition(value) => {
                value.start = requested_start;
                value.duration = requested_duration;
            }
        }
        let packed_start = gaw_core::packed_clip_start(
            to_track,
            &timing_candidate,
            (from_track_id == to_track_id).then_some(clip_id),
        );
        let start_delta = packed_start - original_start;
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
        let start = gaw_core::Beats::new(packed_start).expect("packed start is valid");
        let duration = requested_duration;
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
            self.apply(Intent::Select(Selection::Clip { track, clip }));
        }
    }

    pub(super) fn move_selected_clips(&mut self, requested_delta: f32) {
        let delta = self.selected_clip_move_delta(requested_delta);
        if delta.abs() <= f32::EPSILON {
            return;
        }
        let composition_id = *self
            .nav_path
            .last()
            .expect("current composition always exists");
        let selected_ids = self.selected_clip_ids.clone();
        let mut clips = self
            .project
            .tracks
            .iter()
            .filter(|track| track.composition_id == composition_id)
            .flat_map(|track| {
                track
                    .clips
                    .iter()
                    .filter(|clip| selected_ids.contains(&clip.id().to_string()))
                    .map(|clip| (track.id, clip.clone()))
            })
            .collect::<Vec<_>>();
        if clips.is_empty() {
            return;
        }
        for (_, clip) in &mut clips {
            set_clip_start(clip, clip.start().value() + f64::from(delta));
        }

        let mut commands = Vec::with_capacity(clips.len() * 2);
        for (track_id, clip) in &clips {
            commands.push(Command::RemoveClip {
                track_id: *track_id,
                clip_id: clip.id(),
            });
        }
        for (track_id, clip) in &clips {
            commands.push(Command::AddClip {
                track_id: *track_id,
                clip: clip.clone(),
            });
        }
        let changed_ids = clips
            .iter()
            .flat_map(|(track_id, clip)| [track_id.to_string(), clip.id().to_string()])
            .collect::<Vec<_>>();
        self.commit_ui(
            &Transaction::named("Move selected clips", commands),
            &changed_ids,
        );
    }

    pub(super) fn delete_selected_clips(&mut self) {
        let composition_id = self.current_composition_id();
        let selected_ids = &self.selected_clip_ids;
        let clips = self
            .project
            .tracks
            .iter()
            .filter(|track| track.composition_id == composition_id)
            .flat_map(|track| {
                track
                    .clips
                    .iter()
                    .filter(|clip| selected_ids.contains(&clip.id().to_string()))
                    .map(|clip| (track.id, clip.id()))
            })
            .collect::<Vec<_>>();
        if clips.is_empty() {
            return;
        }
        let mut commands = self
            .project
            .automation
            .iter()
            .filter(|lane| {
                clips.iter().any(|(track_id, clip_id)| {
                    is_clip_automation_target(&lane.target, *track_id, *clip_id)
                })
            })
            .map(|lane| Command::RemoveAutomation { lane_id: lane.id })
            .collect::<Vec<_>>();
        commands.extend(clips.iter().map(|(track_id, clip_id)| Command::RemoveClip {
            track_id: *track_id,
            clip_id: *clip_id,
        }));
        let changed_ids = clips
            .iter()
            .flat_map(|(track_id, clip_id)| [track_id.to_string(), clip_id.to_string()])
            .collect::<Vec<_>>();
        self.commit_ui(
            &Transaction::named("Delete selected clips", commands),
            &changed_ids,
        );
    }

    pub(super) fn clip_clipboard(
        &self,
        track_index: usize,
        clip_index: usize,
    ) -> Option<ClipClipboard> {
        let (track_id, clip_id) = self.clip_ids(track_index, clip_index)?;
        let clip = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)?
            .clips
            .iter()
            .find(|clip| clip.id() == clip_id)?
            .clone();
        let automation = self
            .project
            .automation
            .iter()
            .filter(|lane| is_clip_automation_target(&lane.target, track_id, clip_id))
            .cloned()
            .collect();
        Some(ClipClipboard {
            clip,
            automation,
            source_composition_id: self.current_composition_id(),
            source_track_id: track_id,
        })
    }

    pub(super) fn copy_clip(&mut self, track_index: usize, clip_index: usize) {
        if let Some(clipboard) = self.clip_clipboard(track_index, clip_index) {
            self.clip_clipboard = Some(clipboard);
        }
    }

    pub(super) fn cut_clip(&mut self, track_index: usize, clip_index: usize) {
        let Some(clipboard) = self.clip_clipboard(track_index, clip_index) else {
            return;
        };
        let revision = self.revision();
        self.delete_clip_with_label(track_index, clip_index, "Cut clip");
        if self.revision() != revision {
            self.clip_clipboard = Some(clipboard);
        }
    }

    pub(super) fn duplicate_clip(&mut self, track_index: usize, clip_index: usize) {
        let Some(clipboard) = self.clip_clipboard(track_index, clip_index) else {
            return;
        };
        let requested_start = clipboard.clip.start().value() + clip_duration(&clipboard.clip);
        self.insert_clipboard(clipboard, track_index, requested_start, "Duplicate clip");
    }

    pub(super) fn paste_clip(&mut self, requested_track: Option<usize>, beat: f32) {
        if !beat.is_finite() {
            return;
        }
        let Some(clipboard) = self.clip_clipboard.clone() else {
            return;
        };
        let source_track_id = clipboard.source_track_id.to_string();
        let selected_track = match self.selection {
            Selection::Track { track }
            | Selection::Clip { track, .. }
            | Selection::Effect { track, .. }
            | Selection::Sampler { track } => Some(track),
            Selection::None | Selection::Asset(_) | Selection::MidiAsset(_) => None,
        };
        let track_index = match requested_track {
            Some(track) if self.can_paste_clip_to(track) => track,
            Some(_) => return,
            None => {
                let Some(track) = selected_track
                    .filter(|track| self.can_paste_clip_to(*track))
                    .or_else(|| {
                        self.current_composition()
                            .tracks
                            .iter()
                            .position(|track| track.id == source_track_id)
                            .filter(|track| self.can_paste_clip_to(*track))
                    })
                    .or_else(|| {
                        (0..self.current_composition().tracks.len())
                            .find(|track| self.can_paste_clip_to(*track))
                    })
                else {
                    return;
                };
                track
            }
        };
        self.insert_clipboard(
            clipboard,
            track_index,
            f64::from(beat.max(0.0)),
            "Paste clip",
        );
    }

    pub(super) fn insert_clipboard(
        &mut self,
        clipboard: ClipClipboard,
        track_index: usize,
        requested_start: f64,
        label: &str,
    ) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(track) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
        else {
            return;
        };
        if !clip_is_compatible_with_track(&clipboard.clip, track.kind)
            || !clip_dependencies_exist(&self.project, &clipboard.clip)
        {
            return;
        }
        let composition_id = self.current_composition_id();
        let Some(composition) = self
            .project
            .compositions
            .iter()
            .find(|composition| composition.id == composition_id)
            .cloned()
        else {
            return;
        };
        let source_start = clipboard.clip.start().value();
        let (mut clip, processor_ids) = fresh_clip_identity(clipboard.clip);
        set_clip_start(&mut clip, requested_start);
        let packed_start = gaw_core::packed_clip_start(track, &clip, None);
        set_clip_start(&mut clip, packed_start);
        let clip_id = clip.id();
        let Some(automation) = clone_clip_automation(
            clipboard.automation,
            &processor_ids,
            composition_id,
            track_id,
            clip_id,
            packed_start - source_start,
        ) else {
            return;
        };
        let required_end = automation
            .iter()
            .flat_map(|lane| lane.points.iter().map(|point| point.time.value()))
            .fold(packed_start + clip_duration(&clip), f64::max);
        let mut commands = Vec::with_capacity(automation.len() + 2);
        extend_composition_for_drop(
            &composition,
            packed_start,
            required_end - packed_start,
            self.project.time_signature.quarter_notes_per_bar(),
            &mut commands,
        );
        commands.push(Command::AddClip { track_id, clip });
        commands.extend(
            automation
                .into_iter()
                .map(|lane| Command::AddAutomation { lane }),
        );
        let revision = self.revision();
        self.commit_ui(
            &Transaction::named(label, commands),
            &[track_id.to_string(), clip_id.to_string()],
        );
        if self.revision() != revision {
            let selection = self.selection_for_clip(track_id, clip_id, None);
            self.apply(Intent::Select(selection));
        }
    }

    pub(super) fn delete_clip(&mut self, track_index: usize, clip_index: usize) {
        self.delete_clip_with_label(track_index, clip_index, "Delete clip");
    }

    pub(super) fn delete_clip_with_label(
        &mut self,
        track_index: usize,
        clip_index: usize,
        label: &str,
    ) {
        let Some((track_id, clip_id)) = self.clip_ids(track_index, clip_index) else {
            return;
        };
        let mut commands = self
            .project
            .automation
            .iter()
            .filter(|lane| is_clip_automation_target(&lane.target, track_id, clip_id))
            .map(|lane| Command::RemoveAutomation { lane_id: lane.id })
            .collect::<Vec<_>>();
        commands.push(Command::RemoveClip { track_id, clip_id });
        self.commit_ui(
            &Transaction::named(label, commands),
            &[track_id.to_string(), clip_id.to_string()],
        );
    }

    pub(super) fn rename_clip(&mut self, track_index: usize, clip_index: usize, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let Some((track_id, clip_id)) = self.clip_ids(track_index, clip_index) else {
            return;
        };
        let Some(mut clip) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.clips.iter().find(|clip| clip.id() == clip_id))
            .cloned()
        else {
            return;
        };
        let current_name = match &clip {
            gaw_core::Clip::Audio(clip) => &clip.name,
            gaw_core::Clip::Event(clip) => &clip.name,
            gaw_core::Clip::Composition(clip) => &clip.name,
        };
        if current_name == name {
            return;
        }
        match &mut clip {
            gaw_core::Clip::Audio(clip) => name.clone_into(&mut clip.name),
            gaw_core::Clip::Event(clip) => name.clone_into(&mut clip.name),
            gaw_core::Clip::Composition(clip) => name.clone_into(&mut clip.name),
        }
        self.commit_ui(
            &Transaction::named("Rename clip", [Command::UpdateClip { track_id, clip }]),
            &[track_id.to_string(), clip_id.to_string()],
        );
    }

    pub(super) fn edit_note(&mut self, track_index: usize, clip_index: usize, edit: NoteEdit) {
        self.edit_notes(track_index, clip_index, std::iter::once(edit));
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn edit_notes(
        &mut self,
        track_index: usize,
        clip_index: usize,
        edits: impl IntoIterator<Item = NoteEdit>,
    ) {
        let edits = edits.into_iter().collect::<Vec<_>>();
        if edits.is_empty() {
            return;
        }
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
        let mut additions = Vec::new();
        let mut updates = Vec::new();
        let mut deletions = BTreeSet::new();
        for edit in edits {
            match edit {
                NoteEdit::Add {
                    cents,
                    start,
                    length,
                    pitch,
                    velocity,
                } => {
                    let Some(mut note) = make_note(start, length, pitch, velocity) else {
                        return;
                    };
                    let Ok(tuning) = gaw_core::Cents::new(cents) else {
                        return;
                    };
                    note.tuning = (cents != 0.0).then_some(tuning);
                    additions.push(note);
                }
                NoteEdit::Update {
                    event_index,
                    start,
                    length,
                    pitch,
                    velocity,
                } => {
                    let Some(gaw_core::Event::Note(original)) = events.events.get(event_index)
                    else {
                        return;
                    };
                    let Some(mut note) = make_note(start, length, pitch, velocity) else {
                        return;
                    };
                    note.release_velocity = original.release_velocity;
                    note.tuning = original.tuning;
                    updates.push((event_index, note));
                }
                NoteEdit::Delete { event_index } => {
                    if !matches!(
                        events.events.get(event_index),
                        Some(gaw_core::Event::Note(_))
                    ) {
                        return;
                    }
                    deletions.insert(event_index);
                }
            }
        }
        for (event_index, note) in updates {
            events.events[event_index] = gaw_core::Event::Note(note);
        }
        events
            .events
            .extend(additions.into_iter().map(gaw_core::Event::Note));
        delete_event_indices(&mut events.events, deletions);
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
                cents: 0.0,
                start: 0.0,
                length: 0.25,
                pitch: 60,
                velocity: 100,
            },
        );
    }
}

/// Removes validated original event indexes without shifting the tail for each deletion.
fn delete_event_indices(events: &mut Vec<gaw_core::Event>, deletions: BTreeSet<usize>) {
    let Some(&first) = deletions.first() else {
        return;
    };
    if deletions.len() == 1 {
        events.remove(first);
        return;
    }
    let last = *deletions.last().expect("nonempty deletion set");
    if last - first + 1 == deletions.len() {
        drop(events.drain(first..=last));
        return;
    }
    let mut deletions = deletions.into_iter().peekable();
    let mut event_index = 0;
    events.retain(|_| {
        let keep = deletions.next_if_eq(&event_index).is_none();
        event_index += 1;
        keep
    });
}

#[cfg(test)]
#[path = "clip_edits_tests.rs"]
mod tests;
