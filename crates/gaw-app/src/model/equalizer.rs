use super::{
    Command, Effect, ProcessorStack, ProjectViewModel, Selection, StableSelection, Transaction,
    effect_view, find_processor, processor_stack,
};
use gaw_core::{EqBand, EqShape, ParametricEqParameters, Processor, ProcessorKind};

pub(super) fn default_eq_parameters() -> ParametricEqParameters {
    let bands = [
        (EqShape::HighPass, 30.0, false),
        (EqShape::LowShelf, 80.0, true),
        (EqShape::Bell, 250.0, true),
        (EqShape::Bell, 600.0, true),
        (EqShape::Bell, 1_500.0, true),
        (EqShape::Bell, 4_000.0, true),
        (EqShape::HighShelf, 8_000.0, true),
        (EqShape::LowPass, 18_000.0, false),
    ]
    .into_iter()
    .map(|(shape, frequency_hz, enabled)| EqBand {
        shape,
        frequency_hz,
        enabled,
        ..EqBand::default()
    })
    .collect();
    ParametricEqParameters {
        bands,
        output_gain_db: 0.0,
    }
}

fn default_eq_parameters_for_max_hz(max_hz: f32) -> ParametricEqParameters {
    let mut parameters = default_eq_parameters();
    for band in &mut parameters.bands {
        band.frequency_hz = band.frequency_hz.min(max_hz);
    }
    parameters
}

#[derive(Clone, Debug)]
pub(super) struct EqEditGesture {
    stack: ProcessorStack,
    processor_id: gaw_core::ProcessorId,
    last_revision: Option<u64>,
}

impl ProjectViewModel {
    pub(crate) fn equalizer_max_hz(&self) -> f32 {
        (self.project.sample_rate.value() as f32 * 0.499).min(24_000.0)
    }

    pub(super) fn default_eq_parameters_for_project(&self) -> ParametricEqParameters {
        default_eq_parameters_for_max_hz(self.equalizer_max_hz())
    }

    pub(crate) fn begin_selected_eq_edit(&mut self) {
        let StableSelection::Effect {
            stack,
            processor_id,
        } = self.stable_selection()
        else {
            return;
        };
        if self
            .selected_processor()
            .is_some_and(|processor| matches!(processor.kind, ProcessorKind::ParametricEq(_)))
        {
            self.eq_edit_gesture = Some(EqEditGesture {
                stack,
                processor_id,
                last_revision: None,
            });
        }
    }

    pub(crate) fn end_selected_eq_edit(&mut self) {
        self.eq_edit_gesture = None;
    }

    pub(super) fn apply_transaction_history(
        &mut self,
        transaction: &Transaction,
        source: super::ChangeSource,
    ) -> Result<(), gaw_core::DomainError> {
        let gesture = self.eq_edit_gesture.as_ref().filter(|gesture| {
            source == super::ChangeSource::Ui
                && transaction.label.as_deref() == Some("Edit EQ")
                && matches!(transaction.commands.as_slice(), [Command::UpdateProcessor { stack, processor }]
                    if *stack == gesture.stack && processor.id == gesture.processor_id
                        && matches!(processor.kind, ProcessorKind::ParametricEq(_)))
                && gesture.last_revision.is_none_or(|revision| revision == self.revision())
        }).cloned();
        if let Some(gesture) = &gesture
            && gesture.last_revision.is_some()
        {
            self.engine.history.apply_coalescing_processor_update(
                &mut self.project,
                transaction,
                &gesture.stack,
                &gesture.processor_id,
            )?;
        } else {
            self.engine.history.apply(&mut self.project, transaction)?;
        }
        self.eq_edit_gesture = gesture.map(|mut gesture| {
            gesture.last_revision = Some(self.revision() + 1);
            gesture
        });
        Ok(())
    }

    pub(super) fn reset_signal_scope(&mut self) {
        self.end_selected_eq_edit();
        self.scoped_effect = None;
        self.signal_scope = None;
        self.signal_context = None;
    }

    pub(super) fn arrangement_context(&self) -> StableSelection {
        match self.selection {
            Selection::Clip { track, clip } | Selection::Effect { track, clip, .. } => self
                .clip_ids(track, clip)
                .map_or(StableSelection::None, |(track_id, clip_id)| {
                    StableSelection::Clip { track_id, clip_id }
                }),
            Selection::Track { track } | Selection::Sampler { track } => self
                .current_track_id(track)
                .map_or(StableSelection::None, StableSelection::Track),
            _ => StableSelection::None,
        }
    }

    pub(super) fn selection_for_context(&self, context: &StableSelection) -> Selection {
        match context {
            StableSelection::Clip { track_id, clip_id } => {
                self.selection_for_clip(*track_id, *clip_id, None)
            }
            StableSelection::Track(track_id) => self
                .current_composition()
                .tracks
                .iter()
                .position(|track| track.id == track_id.to_string())
                .map_or(Selection::None, |track| Selection::Track { track }),
            _ => Selection::None,
        }
    }

    pub(crate) fn signal_stack(&self) -> Option<ProcessorStack> {
        if let Some(stack) = &self.signal_scope
            && processor_stack(&self.project, stack).is_some()
        {
            return Some(stack.clone());
        }
        match self.selection {
            Selection::Clip { track, clip } | Selection::Effect { track, clip, .. } => {
                self.clip_stack(track, clip)
            }
            Selection::Track { track } | Selection::Sampler { track } => self
                .current_track_id(track)
                .map(|track_id| ProcessorStack::Track { track_id }),
            _ => Some(ProcessorStack::CompositionOutput {
                composition_id: self.current_composition_id(),
            }),
        }
    }

    pub(crate) fn set_signal_stack(&mut self, stack: ProcessorStack) {
        self.end_selected_eq_edit();
        let Some(processors) = processor_stack(&self.project, &stack) else {
            return;
        };
        let index = processors
            .iter()
            .position(|processor| matches!(processor.kind, ProcessorKind::ParametricEq(_)))
            .or_else(|| (!processors.is_empty()).then_some(0));
        let context = self
            .signal_context
            .clone()
            .unwrap_or_else(|| self.arrangement_context());
        self.selection = self.selection_for_context(&context);
        self.signal_context = Some(context);
        self.scoped_effect = None;
        self.signal_scope = Some(stack.clone());
        if let Some(index) = index {
            self.select_processor_at(stack, index);
        }
    }

    pub(crate) fn effects_at(&self, stack: &ProcessorStack) -> Vec<Effect> {
        processor_stack(&self.project, stack)
            .unwrap_or_default()
            .iter()
            .map(effect_view)
            .collect()
    }

    pub(crate) fn signal_scope_label(&self, stack: &ProcessorStack) -> String {
        match stack {
            ProcessorStack::CompositionOutput { composition_id } => {
                let name = self
                    .project
                    .compositions
                    .iter()
                    .find(|composition| composition.id == *composition_id)
                    .map_or("Composition", |composition| composition.name.as_str());
                let scope = if *composition_id == self.project.root_composition_id {
                    "MASTER"
                } else {
                    "OUTPUT"
                };
                format!("{scope} · {name}")
            }
            ProcessorStack::Track { track_id } => {
                let name = self
                    .project
                    .tracks
                    .iter()
                    .find(|track| track.id == *track_id)
                    .map_or("Track", |track| track.name.as_str());
                format!("TRACK · {name}")
            }
            ProcessorStack::Clip { track_id, clip_id }
            | ProcessorStack::CompositionClip { track_id, clip_id } => {
                let name = self
                    .compositions
                    .iter()
                    .flat_map(|composition| &composition.tracks)
                    .find(|track| track.id == track_id.to_string())
                    .and_then(|track| {
                        track
                            .clips
                            .iter()
                            .find(|clip| clip.id == clip_id.to_string())
                    })
                    .map_or("Clip", |clip| clip.name.as_str());
                format!("CLIP · {name}")
            }
        }
    }

    pub(crate) fn selected_processor(&self) -> Option<Processor> {
        let StableSelection::Effect {
            stack,
            processor_id,
        } = self.stable_selection()
        else {
            return None;
        };
        find_processor(&self.project, &stack, &processor_id)
    }

    pub(crate) fn close_selected_processor_editor(&mut self) {
        let StableSelection::Effect { stack, .. } = self.stable_selection() else {
            return;
        };
        self.end_selected_eq_edit();
        self.scoped_effect = None;
        let context = self.signal_context.clone().unwrap_or(match stack {
            ProcessorStack::Clip { track_id, clip_id }
            | ProcessorStack::CompositionClip { track_id, clip_id } => {
                StableSelection::Clip { track_id, clip_id }
            }
            ProcessorStack::Track { track_id } => StableSelection::Track(track_id),
            ProcessorStack::CompositionOutput { .. } => StableSelection::None,
        });
        self.selection = self.selection_for_context(&context);
        self.signal_context = Some(context);
    }

    pub(crate) fn open_equalizer(&mut self, stack: ProcessorStack) {
        let Some(processors) = processor_stack(&self.project, &stack) else {
            return;
        };
        if let Some(index) = processors
            .iter()
            .position(|processor| matches!(processor.kind, ProcessorKind::ParametricEq(_)))
        {
            self.select_processor_at(stack, index);
        } else if let Some(index) = Self::processor_catalog()
            .iter()
            .position(|(type_id, _)| type_id == "gaw.parametric_eq")
        {
            self.insert_processor(stack, index);
        }
    }

    pub(crate) fn set_selected_eq_parameters(&mut self, parameters: ParametricEqParameters) {
        let StableSelection::Effect {
            stack,
            processor_id,
        } = self.stable_selection()
        else {
            return;
        };
        let Some(mut processor) = find_processor(&self.project, &stack, &processor_id) else {
            return;
        };
        let ProcessorKind::ParametricEq(current) = &processor.kind else {
            return;
        };
        if current == &parameters {
            return;
        }
        processor.kind = ProcessorKind::ParametricEq(parameters);
        self.commit_ui(
            &Transaction::named("Edit EQ", [Command::UpdateProcessor { stack, processor }]),
            &[processor_id.to_string()],
        );
    }

    /// Removing a band also removes its automation and shifts later band targets.
    pub(crate) fn remove_selected_eq_band(&mut self, index: usize) {
        let StableSelection::Effect {
            stack,
            processor_id,
        } = self.stable_selection()
        else {
            return;
        };
        let Some(mut processor) = find_processor(&self.project, &stack, &processor_id) else {
            return;
        };
        let ProcessorKind::ParametricEq(parameters) = &mut processor.kind else {
            return;
        };
        if index >= parameters.bands.len() {
            return;
        }
        parameters.bands.remove(index);
        let mut commands = Vec::new();
        for original in &self.project.automation {
            let mut lane = original.clone();
            let (id, parameter) = match &mut lane.target {
                gaw_core::AutomationTarget::AudioClipProcessor {
                    processor_id,
                    parameter_id,
                    ..
                }
                | gaw_core::AutomationTarget::CompositionClipProcessor {
                    processor_id,
                    parameter_id,
                    ..
                }
                | gaw_core::AutomationTarget::TrackProcessor {
                    processor_id,
                    parameter_id,
                    ..
                }
                | gaw_core::AutomationTarget::CompositionOutputProcessor {
                    processor_id,
                    parameter_id,
                } => (processor_id, parameter_id),
                gaw_core::AutomationTarget::Instrument { .. } => continue,
            };
            if *id != processor_id {
                continue;
            }
            let Some((band, suffix)) = parameter
                .strip_prefix("bands.")
                .and_then(|path| path.split_once('.'))
            else {
                continue;
            };
            let Ok(band) = band.parse::<usize>() else {
                continue;
            };
            if band == index {
                commands.push(Command::RemoveAutomation { lane_id: lane.id });
            } else if band > index {
                *parameter = format!("bands.{}.{suffix}", band - 1);
                commands.push(Command::UpdateAutomation { lane });
            }
        }
        commands.push(Command::UpdateProcessor { stack, processor });
        self.commit_ui(
            &Transaction::named("Remove EQ band", commands),
            &[processor_id.to_string()],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Intent;
    use gaw_core::{AutomationLane, AutomationLaneId, AutomationTarget};

    fn parameters(vm: &ProjectViewModel) -> ParametricEqParameters {
        let ProcessorKind::ParametricEq(parameters) = vm.selected_processor().unwrap().kind else {
            panic!("EQ selected")
        };
        parameters
    }

    #[test]
    fn all_scopes_edit_independently_and_roundtrip_history() {
        let fixture = ProjectViewModel::demo();
        let mut scopes = (0..3)
            .map(|track| fixture.clip_stack(track, 0).unwrap())
            .collect::<Vec<_>>();
        scopes.extend(
            fixture
                .project
                .tracks
                .iter()
                .map(|track| ProcessorStack::Track { track_id: track.id }),
        );
        scopes.extend(fixture.project.compositions.iter().map(|composition| {
            ProcessorStack::CompositionOutput {
                composition_id: composition.id,
            }
        }));
        for stack in scopes {
            let mut vm = ProjectViewModel::demo();
            vm.apply(Intent::Select(Selection::Clip { track: 0, clip: 0 }));
            let before = vm.project.clone();
            let original_count = vm.effects_at(&stack).len();
            vm.open_equalizer(stack.clone());
            assert_eq!(vm.effects_at(&stack).len(), original_count + 1);
            assert_eq!(vm.signal_stack(), Some(stack.clone()));
            let owner_selection = vm.arrangement_context();
            let mut changed = parameters(&vm);
            assert_eq!(changed, default_eq_parameters());
            changed.bands[3].gain_db = -6.0;
            vm.set_selected_eq_parameters(changed.clone());
            assert_eq!(parameters(&vm), changed);
            assert_eq!(vm.arrangement_context(), owner_selection);
            let edited = vm.project.clone();
            let selected_id = vm.selected_processor().unwrap().id;
            vm.open_equalizer(stack.clone());
            assert_eq!(vm.selected_processor().unwrap().id, selected_id);
            assert_eq!(vm.effects_at(&stack).len(), original_count + 1);
            vm.apply(Intent::Undo(0.0));
            assert_eq!(parameters(&vm), default_eq_parameters());
            assert_eq!(vm.arrangement_context(), owner_selection);
            vm.apply(Intent::Redo(0.0));
            assert_eq!(vm.project, edited);
            assert_eq!(vm.arrangement_context(), owner_selection);
            vm.apply(Intent::Undo(0.0));
            vm.apply(Intent::Undo(0.0));
            assert_eq!(
                vm.project, before,
                "only the requested stack changed: {stack:?}"
            );
            assert_eq!(vm.signal_stack(), Some(stack));
            assert_eq!(vm.arrangement_context(), owner_selection);
            assert!(vm.last_error().is_none());
        }
    }

    #[test]
    fn inserted_eq_defaults_stay_below_project_nyquist_margin() {
        let mut vm = ProjectViewModel::demo();
        vm.apply(Intent::SetProjectSampleRate(16_000));
        assert!((vm.equalizer_max_hz() - 7_984.0).abs() < f32::EPSILON);

        vm.open_equalizer(vm.signal_stack().unwrap());
        let parameters = parameters(&vm);
        assert!(
            parameters
                .bands
                .iter()
                .all(|band| band.frequency_hz <= 7_984.0)
        );
        assert!((parameters.bands.last().unwrap().frequency_hz - 7_984.0).abs() < f32::EPSILON);
    }

    #[test]
    fn scope_switching_preserves_clip_context_and_navigation_resets_it() {
        let mut vm = ProjectViewModel::demo();
        vm.apply(Intent::Select(Selection::Clip { track: 0, clip: 0 }));
        let context = vm.arrangement_context();
        let clip = vm.signal_stack().unwrap();
        let track = ProcessorStack::Track {
            track_id: vm.current_track_id(0).unwrap(),
        };
        let root = ProcessorStack::CompositionOutput {
            composition_id: vm.project.root_composition_id,
        };
        for stack in [track, root, clip] {
            vm.set_signal_stack(stack.clone());
            assert_eq!(vm.signal_stack(), Some(stack));
            assert_eq!(vm.arrangement_context(), context);
        }
        vm.apply(Intent::Select(Selection::Track { track: 1 }));
        assert_eq!(
            vm.signal_stack(),
            Some(ProcessorStack::Track {
                track_id: vm.current_track_id(1).unwrap()
            })
        );
        vm.apply(Intent::EnterChild { track: 2, clip: 0 });
        let child = vm.current_composition_id();
        assert_ne!(child, vm.project.root_composition_id);
        assert_eq!(
            vm.signal_stack(),
            Some(ProcessorStack::CompositionOutput {
                composition_id: child
            })
        );
        assert!(
            vm.signal_scope_label(&vm.signal_stack().unwrap())
                .starts_with("OUTPUT · ")
        );
        vm.open_equalizer(vm.signal_stack().unwrap());
        vm.apply(Intent::Back);
        assert!(vm.selected_processor().is_none());
        assert!(
            vm.signal_scope_label(&vm.signal_stack().unwrap())
                .starts_with("MASTER · ")
        );
    }

    #[test]
    fn eq_uses_the_owner_context_in_the_bottom_editor() {
        for (track, expected) in [
            (0, super::super::EditorKind::Waveform),
            (1, super::super::EditorKind::PianoRoll),
            (2, super::super::EditorKind::Waveform),
        ] {
            let mut vm = ProjectViewModel::demo();
            vm.apply(Intent::Select(Selection::Clip { track, clip: 0 }));
            let stack = vm.signal_stack().unwrap();
            vm.open_equalizer(stack.clone());
            assert_eq!(vm.editor_kind(), expected);

            vm.close_selected_processor_editor();
            assert_eq!(vm.selection, Selection::Clip { track, clip: 0 });
            assert_eq!(vm.signal_stack(), Some(stack));
            assert!(vm.selected_processor().is_none());
        }

        let mut vm = ProjectViewModel::demo();
        vm.apply(Intent::Select(Selection::Clip { track: 0, clip: 0 }));
        let owner = vm.arrangement_context();
        let stacks = [
            ProcessorStack::Track {
                track_id: vm.current_track_id(0).unwrap(),
            },
            ProcessorStack::CompositionOutput {
                composition_id: vm.project.root_composition_id,
            },
        ];
        for stack in stacks {
            vm.open_equalizer(stack.clone());
            assert_eq!(vm.editor_kind(), super::super::EditorKind::Overview);
            vm.close_selected_processor_editor();
            assert_eq!(vm.arrangement_context(), owner);
            assert_eq!(vm.signal_stack(), Some(stack));
            assert!(vm.selected_processor().is_none());
        }
    }

    #[test]
    fn multiple_selected_clips_survive_scoped_edits_and_external_reload() {
        let mut vm = ProjectViewModel::demo();
        let selected = vm
            .current_composition()
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .take(2)
            .map(|clip| clip.id.clone())
            .collect::<Vec<_>>();
        vm.apply(Intent::SelectClips(selected.clone()));
        let context = vm.arrangement_context();
        let stack = ProcessorStack::CompositionOutput {
            composition_id: vm.project.root_composition_id,
        };
        vm.open_equalizer(stack.clone());
        assert_eq!(vm.selected_clip_count(), 2);
        let mut changed = parameters(&vm);
        changed.output_gain_db = -3.0;
        vm.set_selected_eq_parameters(changed);
        assert_eq!(vm.selected_clip_count(), 2);
        vm.replace_project_from_agent(vm.project.clone(), [], 0.0)
            .unwrap();
        assert_eq!(vm.selected_clip_count(), 2);
        assert_eq!(vm.arrangement_context(), context);
        assert_eq!(vm.signal_stack(), Some(stack));
        assert!(vm.selected_processor().is_some());
    }

    #[test]
    fn deleting_selected_processor_keeps_owner_and_invalid_stack_is_ignored() {
        let mut vm = ProjectViewModel::demo();
        vm.apply(Intent::Select(Selection::Clip { track: 0, clip: 0 }));
        let context = vm.arrangement_context();
        let stack = ProcessorStack::Track {
            track_id: vm.current_track_id(0).unwrap(),
        };
        vm.open_equalizer(stack.clone());
        vm.remove_processor_at(stack.clone(), vm.effects_at(&stack).len() - 1);
        assert!(vm.selected_processor().is_none());
        assert_eq!(vm.arrangement_context(), context);
        assert_eq!(vm.signal_stack(), Some(stack.clone()));
        vm.set_signal_stack(ProcessorStack::Track {
            track_id: gaw_core::TrackId::new(),
        });
        assert_eq!(vm.signal_stack(), Some(stack));
    }

    #[test]
    fn creating_and_pasting_clips_resets_master_eq_scope() {
        let mut vm = ProjectViewModel::demo();
        let master = ProcessorStack::CompositionOutput {
            composition_id: vm.project.root_composition_id,
        };
        let asset_id = vm.project.assets[0].id;
        vm.open_equalizer(master.clone());
        vm.apply(Intent::AddAssetClip {
            asset_id,
            beat: 80.0,
            track: Some(0),
            tempo_sync: None,
        });
        let Selection::Clip { track, clip } = vm.selection else {
            panic!("new clip selected")
        };
        assert_eq!(vm.signal_stack(), vm.clip_stack(track, clip));
        assert!(vm.selected_processor().is_none());
        vm.apply(Intent::CopyClip { track, clip });
        vm.open_equalizer(master.clone());
        vm.apply(Intent::PasteClip {
            track: Some(track),
            beat: 84.0,
        });
        let Selection::Clip { track, clip } = vm.selection else {
            panic!("pasted clip selected")
        };
        assert_eq!(vm.signal_stack(), vm.clip_stack(track, clip));
        assert!(vm.selected_processor().is_none());
        vm.open_equalizer(master);
        vm.apply(Intent::CreateMidiAsset);
        assert!(matches!(vm.selection, Selection::MidiAsset(_)));
        assert!(vm.selected_processor().is_none());
        assert!(vm.last_error().is_none());
    }

    #[test]
    fn deleted_scope_falls_back_to_output_without_retargeting_another_track() {
        let mut vm = ProjectViewModel::demo();
        vm.apply(Intent::Select(Selection::Track { track: 0 }));
        let stack = vm.signal_stack().unwrap();
        vm.open_equalizer(stack);
        vm.apply(Intent::DeleteTrack { track: 0 });
        assert!(vm.last_error().is_none());
        assert!(vm.selected_processor().is_none());
        assert_eq!(vm.selection, Selection::None);
        assert_eq!(
            vm.signal_stack(),
            Some(ProcessorStack::CompositionOutput {
                composition_id: vm.project.root_composition_id,
            })
        );
        vm.apply(Intent::Undo(0.0));
        assert!(vm.selected_processor().is_none());
    }

    #[test]
    fn removing_band_remaps_automation_atomically_and_undo_restores_it() {
        let mut vm = ProjectViewModel::demo();
        vm.open_equalizer(vm.signal_stack().unwrap());
        let processor_id = vm.selected_processor().unwrap().id;
        for index in [2, 4] {
            let lane = AutomationLane {
                id: AutomationLaneId::new(),
                composition_id: vm.project.root_composition_id,
                name: format!("Band {index}"),
                target: AutomationTarget::CompositionOutputProcessor {
                    processor_id: processor_id.clone(),
                    parameter_id: format!("bands.{index}.gain_db"),
                },
                points: vec![gaw_core::AutomationPoint {
                    time: gaw_core::Beats::new(0.0).unwrap(),
                    value: gaw_core::AutomationValue::Decibels(
                        gaw_core::Decibels::new(-3.0).unwrap(),
                    ),
                    curve: gaw_core::AutomationCurve::Linear,
                }],
            };
            vm.commit_ui(
                &Transaction::named("Add automation", [Command::AddAutomation { lane }]),
                &[],
            );
        }
        assert!(vm.last_error().is_none(), "{:?}", vm.last_error());
        let before = vm.project.clone();
        vm.remove_selected_eq_band(2);
        assert_eq!(parameters(&vm).bands.len(), 7);
        assert_eq!(vm.selected_parameter_automation_lanes("bands.2.gain_db"), 0);
        assert_eq!(vm.selected_parameter_automation_lanes("bands.3.gain_db"), 1);
        vm.apply(Intent::Undo(0.0));
        assert_eq!(vm.project, before);
        vm.apply(Intent::Redo(0.0));
        assert_eq!(parameters(&vm).bands.len(), 7);
    }

    #[test]
    fn eq_drag_is_one_undo_and_unrelated_edits_break_grouping() {
        let mut vm = ProjectViewModel::demo();
        vm.open_equalizer(vm.signal_stack().unwrap());
        let before = vm.project.clone();
        vm.begin_selected_eq_edit();
        for gain in [1.0, 2.0, 3.0] {
            let mut changed = parameters(&vm);
            changed.bands[3].gain_db = gain;
            vm.set_selected_eq_parameters(changed);
        }
        vm.end_selected_eq_edit();
        let edited = vm.project.clone();
        vm.apply(Intent::Undo(0.0));
        assert_eq!(vm.project, before);
        vm.apply(Intent::Redo(0.0));
        assert_eq!(vm.project, edited);
        vm.begin_selected_eq_edit();
        let mut changed = parameters(&vm);
        changed.bands[3].gain_db = 5.0;
        vm.set_selected_eq_parameters(changed.clone());
        vm.apply(Intent::SetMasterVolume(-4.0));
        changed.bands[3].gain_db = 6.0;
        vm.set_selected_eq_parameters(changed);
        vm.apply(Intent::Undo(0.0));
        assert_eq!(parameters(&vm).bands[3].gain_db, 5.0);
        assert_eq!(vm.transport.master_volume_db, -4.0);
    }
}
