use super::*;
use gaw_core::Validate as _;

fn automated_clip_vm() -> (ProjectViewModel, TrackId, ClipId, ProcessorId) {
    let mut project = demo_project();
    let composition_id = project.root_composition_id;
    let track_id = project
        .compositions
        .iter()
        .find(|composition| composition.id == composition_id)
        .expect("root composition")
        .track_ids[0];
    let clip = project
        .tracks
        .iter_mut()
        .find(|track| track.id == track_id)
        .expect("audio track")
        .clips
        .first_mut()
        .expect("audio clip");
    let gaw_core::Clip::Audio(clip) = clip else {
        panic!("fixture clip should be audio");
    };
    clip.reverse = true;
    clip.source.start = gaw_core::Seconds::new(0.1).unwrap();
    clip.source.duration = gaw_core::Seconds::new(0.5).unwrap();
    clip.fade_in = Some(gaw_core::Fade {
        duration: gaw_core::Seconds::new(0.05).unwrap(),
        curve: gaw_core::FadeCurve::EqualPower,
    });
    let clip_id = clip.id;
    let processor_id = clip.effects[0].id.clone();
    project.automation.push(gaw_core::AutomationLane {
        id: gaw_core::AutomationLaneId::new(),
        composition_id,
        name: "Clip gain".into(),
        target: gaw_core::AutomationTarget::AudioClipProcessor {
            track_id,
            clip_id,
            processor_id: processor_id.clone(),
            parameter_id: "gain_db".into(),
        },
        points: vec![
            gaw_core::AutomationPoint {
                time: gaw_core::Beats::new(1.0).unwrap(),
                value: gaw_core::AutomationValue::Decibels(gaw_core::Decibels::new(-6.0).unwrap()),
                curve: gaw_core::AutomationCurve::Linear,
            },
            gaw_core::AutomationPoint {
                time: gaw_core::Beats::new(3.0).unwrap(),
                value: gaw_core::AutomationValue::Decibels(gaw_core::Decibels::new(0.0).unwrap()),
                curve: gaw_core::AutomationCurve::Smooth,
            },
        ],
    });
    (
        ProjectViewModel::from_project(project).unwrap(),
        track_id,
        clip_id,
        processor_id,
    )
}

#[test]
fn track_mute_and_solo_intents_update_canonical_state_and_are_undoable() {
    let mut vm = ProjectViewModel::demo();
    let track_id = vm.current_track_id(0).expect("demo track");

    vm.apply(Intent::ToggleMute(0));
    assert!(
        vm.project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("track remains present")
            .muted
    );
    assert!(vm.current_composition().tracks[0].muted);
    let mute_update = vm.take_updates().next().expect("mute update");
    assert_eq!(mute_update.source, ChangeSource::Ui);

    vm.apply(Intent::ToggleSolo(0));
    let track = vm
        .project
        .tracks
        .iter()
        .find(|track| track.id == track_id)
        .expect("track remains present");
    assert!(track.muted && track.solo);
    assert!(vm.current_composition().tracks[0].solo);

    vm.apply(Intent::Undo(0.0));
    assert!(!vm.current_composition().tracks[0].solo);
    assert!(vm.current_composition().tracks[0].muted);
    vm.apply(Intent::Undo(0.0));
    assert!(!vm.current_composition().tracks[0].muted);
}

#[test]
fn track_metronome_and_master_volume_intents_update_canonical_state() {
    let mut vm = ProjectViewModel::demo();
    let track_id = vm.current_track_id(0).expect("demo track");
    vm.compositions
        .iter_mut()
        .flat_map(|composition| &mut composition.tracks)
        .find(|track| track.id == track_id.to_string())
        .expect("projected track")
        .level = 0.42;

    vm.apply(Intent::SetTrackVolume {
        track: 0,
        volume_db: -12.0,
    });
    assert!(
        (vm.project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("track remains present")
            .volume_db
            + 12.0)
            .abs()
            < f32::EPSILON
    );
    assert!((vm.current_composition().tracks[0].level - 0.42).abs() < f32::EPSILON);

    vm.apply(Intent::SetMetronomeGain(0.35));
    assert!((vm.project.settings.metronome_gain.value() - 0.35).abs() < 1e-6);
    assert!((vm.transport.metronome_gain - 0.35).abs() < f32::EPSILON);

    vm.apply(Intent::SetMasterVolume(-6.0));
    assert!((vm.project.settings.master_volume.value() + 6.0).abs() < 1e-6);
    assert!((vm.transport.master_volume_db + 6.0).abs() < f32::EPSILON);

    vm.apply(Intent::Undo(0.0));
    assert!(vm.transport.master_volume_db.abs() < f32::EPSILON);
    vm.apply(Intent::Undo(0.0));
    assert!((vm.transport.metronome_gain - 0.7).abs() < f32::EPSILON);
    vm.apply(Intent::Undo(0.0));
    assert!(vm.current_composition().tracks[0].volume_db.abs() < f32::EPSILON);
}

#[test]
fn demo_data_exercises_core_surfaces() {
    let vm = ProjectViewModel::demo();
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
                !track.effects.is_empty() && track.clips.iter().all(|clip| clip.effects.is_empty())
            })
    );
}

#[test]
fn clip_selection_is_deduplicated_stable_and_supports_all_clip_kinds() {
    let mut vm = ProjectViewModel::demo();
    let audio_ids = vm.current_composition().tracks[0].clips[..2]
        .iter()
        .map(|clip| clip.id.clone())
        .collect::<Vec<_>>();
    let event_id = vm.current_composition().tracks[1].clips[0].id.clone();

    vm.apply(Intent::SelectClips(vec![
        audio_ids[1].clone(),
        event_id.clone(),
        audio_ids[0].clone(),
        audio_ids[0].clone(),
    ]));

    assert!(audio_ids.iter().all(|id| vm.is_clip_selected(id)));
    assert!(vm.is_clip_selected(&event_id));
    assert_eq!(vm.selected_clip_count(), 3);
    assert_eq!(vm.selection, Selection::Clip { track: 0, clip: 0 });

    let selection = vm.stable_selection();
    vm.refresh_projection(&selection);
    assert!(audio_ids.iter().all(|id| vm.is_clip_selected(id)));

    vm.apply(Intent::Select(Selection::Track { track: 0 }));
    assert!(audio_ids.iter().all(|id| !vm.is_clip_selected(id)));
}

#[test]
fn toggle_selection_builds_and_reduces_mixed_clip_and_asset_sets() {
    let mut vm = ProjectViewModel::demo();
    let audio_clip = vm.current_composition().tracks[0].clips[0].id.clone();
    let event_clip = vm.current_composition().tracks[1].clips[0].id.clone();

    vm.apply(Intent::Select(Selection::Clip { track: 0, clip: 0 }));
    vm.apply(Intent::ToggleClipSelection { track: 1, clip: 0 });

    assert_eq!(vm.selected_clip_count(), 2);
    assert!(vm.is_clip_selected(&audio_clip));
    assert!(vm.is_clip_selected(&event_clip));
    assert_eq!(vm.selection, Selection::Clip { track: 1, clip: 0 });

    vm.apply(Intent::ToggleClipSelection { track: 0, clip: 0 });
    assert_eq!(vm.selected_clip_count(), 1);
    assert!(!vm.is_clip_selected(&audio_clip));
    assert!(vm.is_clip_selected(&event_clip));

    vm.apply(Intent::Select(Selection::Asset(0)));
    vm.apply(Intent::ToggleAssetSelection(Selection::MidiAsset(0)));

    assert_eq!(vm.selected_clip_count(), 0);
    assert!(vm.is_audio_asset_selected(0));
    assert!(vm.is_midi_asset_selected(0));
    assert_eq!(vm.selection, Selection::MidiAsset(0));
    assert_eq!(
        vm.asset_action_indices(Selection::Asset(0)),
        (vec![0], vec![0])
    );
    assert_eq!(
        vm.asset_action_indices(Selection::Asset(1)),
        (vec![1], Vec::new())
    );

    let selection = vm.stable_selection();
    vm.refresh_projection(&selection);
    assert!(vm.is_audio_asset_selected(0));
    assert!(vm.is_midi_asset_selected(0));

    vm.apply(Intent::ToggleAssetSelection(Selection::Asset(0)));
    assert!(!vm.is_audio_asset_selected(0));
    assert!(vm.is_midi_asset_selected(0));

    vm.apply(Intent::ToggleAssetSelection(Selection::MidiAsset(0)));
    assert!(!vm.is_midi_asset_selected(0));
    assert_eq!(vm.selection, Selection::None);
}

#[test]
fn mixed_clip_selection_moves_and_deletes_as_one_undoable_edit() {
    let mut vm = ProjectViewModel::demo();
    let audio_id = vm.current_composition().tracks[0].clips[0].id.clone();
    let event_id = vm.current_composition().tracks[1].clips[0].id.clone();
    let audio_start = vm.current_composition().tracks[0].clips[0].start;
    let event_start = vm.current_composition().tracks[1].clips[0].start;
    vm.apply(Intent::SelectClips(vec![
        audio_id.clone(),
        event_id.clone(),
    ]));

    vm.apply(Intent::MoveSelectedClips { delta: 1.0 });

    let moved_audio = vm
        .current_composition()
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .find(|clip| clip.id == audio_id)
        .unwrap();
    let moved_event = vm
        .current_composition()
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .find(|clip| clip.id == event_id)
        .unwrap();
    assert!((moved_audio.start - audio_start - 1.0).abs() < f32::EPSILON);
    assert!((moved_event.start - event_start - 1.0).abs() < f32::EPSILON);

    vm.apply(Intent::Undo(1.0));
    vm.apply(Intent::DeleteSelectedClips);
    assert!([&audio_id, &event_id].into_iter().all(|id| {
        vm.current_composition()
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .all(|clip| &clip.id != id)
    }));

    vm.apply(Intent::Undo(2.0));
    assert!([&audio_id, &event_id].into_iter().all(|id| {
        vm.current_composition()
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .any(|clip| &clip.id == id)
    }));
}

#[test]
fn selected_audio_clips_move_as_one_undoable_block() {
    let mut vm = ProjectViewModel::demo();
    let audio_ids = vm.current_composition().tracks[0].clips[..2]
        .iter()
        .map(|clip| clip.id.clone())
        .collect::<Vec<_>>();
    let original_starts = vm.current_composition().tracks[0].clips[..2]
        .iter()
        .map(|clip| clip.start)
        .collect::<Vec<_>>();
    vm.apply(Intent::SelectClips(audio_ids.clone()));
    let revision = vm.revision();

    vm.apply(Intent::MoveSelectedClips { delta: 4.0 });

    let moved_starts = vm.current_composition().tracks[0].clips[..2]
        .iter()
        .map(|clip| clip.start)
        .collect::<Vec<_>>();
    assert_eq!(vm.revision(), revision + 1);
    assert!((moved_starts[0] - 4.0).abs() < f32::EPSILON);
    assert!((moved_starts[1] - 18.0).abs() < f32::EPSILON);
    assert!(
        ((moved_starts[1] - moved_starts[0]) - (original_starts[1] - original_starts[0])).abs()
            < f32::EPSILON
    );
    assert!(audio_ids.iter().all(|id| vm.is_clip_selected(id)));

    vm.apply(Intent::Undo(1.0));
    let undone_starts = vm.current_composition().tracks[0].clips[..2]
        .iter()
        .map(|clip| clip.start)
        .collect::<Vec<_>>();
    assert!(
        undone_starts
            .iter()
            .zip(&original_starts)
            .all(|(actual, expected)| (actual - expected).abs() < f32::EPSILON)
    );
    vm.apply(Intent::Redo(2.0));
    let redone_starts = vm.current_composition().tracks[0].clips[..2]
        .iter()
        .map(|clip| clip.start)
        .collect::<Vec<_>>();
    assert!(
        redone_starts
            .iter()
            .zip(&moved_starts)
            .all(|(actual, expected)| (actual - expected).abs() < f32::EPSILON)
    );
}

#[test]
fn selected_audio_clips_delete_as_one_undoable_batch() {
    let mut vm = ProjectViewModel::demo();
    let audio_ids = vm.current_composition().tracks[0].clips[..2]
        .iter()
        .map(|clip| clip.id.clone())
        .collect::<Vec<_>>();
    let original_count = vm.current_composition().tracks[0].clips.len();
    vm.apply(Intent::SelectClips(audio_ids.clone()));
    let revision = vm.revision();

    vm.apply(Intent::DeleteSelectedClips);

    assert_eq!(vm.revision(), revision + 1);
    assert_eq!(
        vm.current_composition().tracks[0].clips.len(),
        original_count - audio_ids.len()
    );
    assert!(audio_ids.iter().all(|id| {
        vm.current_composition()
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .all(|clip| clip.id != *id)
    }));
    assert_eq!(vm.selected_clip_count(), 0);
    assert_eq!(vm.selection, Selection::None);

    vm.apply(Intent::Undo(1.0));

    assert_eq!(
        vm.current_composition().tracks[0].clips.len(),
        original_count
    );
    assert!(audio_ids.iter().all(|id| {
        vm.current_composition()
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .any(|clip| clip.id == *id)
    }));
}

#[test]
fn selected_audio_block_packs_against_stationary_clips_with_one_delta() {
    let mut vm = ProjectViewModel::demo();
    let track = &vm.current_composition().tracks[0];
    let selected = vec![track.clips[0].id.clone(), track.clips[2].id.clone()];
    vm.apply(Intent::SelectClips(selected));

    assert!(vm.selected_clip_move_delta(-8.0).abs() < f32::EPSILON);
    assert!((vm.selected_clip_move_delta(5.0) - 2.0).abs() < f32::EPSILON);
}

#[test]
fn nested_navigation_and_breadcrumbs_are_bounded() {
    let mut vm = ProjectViewModel::demo();
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
    let mut vm = ProjectViewModel::demo();
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
    let mut vm = ProjectViewModel::demo();
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
    let mut vm = ProjectViewModel::demo();
    let first_asset = vm.project.assets[1].id;
    let second_asset = vm.project.assets[2].id;
    vm.apply(Intent::AddAssetClip {
        asset_id: first_asset,
        beat: 6.0,
        track: Some(0),
        tempo_sync: None,
    });
    vm.apply(Intent::AddAssetClip {
        asset_id: second_asset,
        beat: 10.0,
        track: Some(0),
        tempo_sync: None,
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
    assert!(selected.start >= 10.0);
    let selected_end = selected.start + selected.length;
    let same_track = &vm.current_composition().tracks[track].clips;
    assert!(same_track.iter().all(|other| {
        other.id == selected.id
            || selected_end <= other.start
            || other.start + other.length <= selected.start
    }));
    let expected_length = asset_timeline_duration(
        &vm.project.assets[2],
        &vm.project,
        gaw_core::TempoSync::Stretch,
    ) as f32;
    assert!((selected.length - expected_length).abs() < f32::EPSILON);
}

#[test]
fn asset_drop_choice_controls_sync_mode_and_timeline_length() {
    let project = demo_project();
    let asset_id = project.assets[1].id;
    let source_seconds = asset_duration(&project.assets[1]).expect("asset duration");
    let asset_bpm = project.assets[1].tempo.expect("asset tempo").bpm.value();
    let project_bpm = project.bpm.value();

    for (tempo_sync, expected_beats) in [
        (
            gaw_core::TempoSync::Repitch,
            source_seconds * asset_bpm / 60.0,
        ),
        (
            gaw_core::TempoSync::Stretch,
            source_seconds * asset_bpm / 60.0,
        ),
        (
            gaw_core::TempoSync::None,
            source_seconds * project_bpm / 60.0,
        ),
    ] {
        let mut vm = ProjectViewModel::from_project(project.clone()).unwrap();
        vm.apply(Intent::AddAssetClip {
            asset_id,
            beat: 3.25,
            track: None,
            tempo_sync: Some(tempo_sync),
        });
        let Selection::Clip { track, clip } = vm.selection else {
            panic!("dropped clip should be selected");
        };
        let dropped = &vm.current_composition().tracks[track].clips[clip];
        let ClipKind::Audio { sync, .. } = dropped.kind else {
            panic!("dropped clip should be audio");
        };
        let expected_sync = match tempo_sync {
            gaw_core::TempoSync::None => SyncMode::None,
            gaw_core::TempoSync::Repitch => SyncMode::Repitch,
            gaw_core::TempoSync::Stretch => SyncMode::Stretch,
        };
        assert_eq!(sync, expected_sync);
        assert!((f64::from(dropped.length) - expected_beats).abs() < 0.001);
        assert!((dropped.start - 3.25).abs() < f32::EPSILON);
        assert!(
            (vm.project.assets[1].tempo.expect("asset tempo").bpm.value() - asset_bpm).abs()
                < f64::EPSILON
        );
    }
}

#[test]
fn untargeted_asset_insertion_creates_a_new_track_at_the_requested_beat() {
    let mut vm = ProjectViewModel::demo();
    let original_track_ids = vm.project.compositions[0].track_ids.clone();
    let asset_id = vm.project.assets[0].id;

    vm.apply(Intent::AddAssetClip {
        asset_id,
        beat: 7.5,
        track: None,
        tempo_sync: None,
    });

    let composition = vm.current_composition();
    assert_eq!(composition.tracks.len(), original_track_ids.len() + 1);
    assert_eq!(
        &vm.project.compositions[0].track_ids[..original_track_ids.len()],
        original_track_ids.as_slice()
    );
    let new_track = composition.tracks.last().expect("new audio track");
    assert_eq!(new_track.kind, TrackKind::Audio);
    assert_eq!(new_track.clips.len(), 1);
    assert!((new_track.clips[0].start - 7.5).abs() < f32::EPSILON);
    assert_eq!(
        vm.selection,
        Selection::Clip {
            track: composition.tracks.len() - 1,
            clip: 0,
        }
    );
}

#[test]
fn audio_drop_creates_a_track_when_target_is_event_only() {
    let mut vm = ProjectViewModel::demo();
    vm.apply(Intent::EnterChild { track: 2, clip: 0 });
    vm.apply(Intent::EnterChild { track: 2, clip: 0 });
    let asset_id = vm.project.assets[0].id;
    vm.apply(Intent::AddAssetClip {
        asset_id,
        beat: 2.0,
        track: Some(0),
        tempo_sync: None,
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
fn audio_drop_extends_a_legacy_empty_composition_and_undoes_atomically() {
    let mut project = gaw_core::Project::new(
        "Legacy empty",
        gaw_core::Bpm::new(120.0).unwrap(),
        gaw_core::SampleRate::new(48_000).unwrap(),
    );
    project.compositions[0].length = gaw_core::Beats::new(0.0).unwrap();
    let asset = demo_project().assets[0].clone();
    let asset_id = asset.id;
    project.assets.push(asset);
    let mut vm = ProjectViewModel::from_project(project).unwrap();

    vm.apply(Intent::AddAssetClip {
        asset_id,
        beat: 12.0,
        track: None,
        tempo_sync: None,
    });
    assert!(
        (vm.current_composition().length_beats - gaw_core::DEFAULT_COMPOSITION_LENGTH_BEATS as f32)
            .abs()
            < f32::EPSILON
    );
    assert_eq!(vm.current_composition().tracks.len(), 1);
    assert_eq!(vm.current_composition().tracks[0].clips.len(), 1);

    vm.apply(Intent::Undo(0.0));
    assert!(vm.current_composition().length_beats.abs() < f32::EPSILON);
    assert!(vm.current_composition().tracks.is_empty());
}

#[test]
fn audio_drop_at_the_end_extends_to_a_bar_boundary() {
    let mut project = gaw_core::Project::new(
        "Short",
        gaw_core::Bpm::new(120.0).unwrap(),
        gaw_core::SampleRate::new(48_000).unwrap(),
    );
    project.compositions[0].length = gaw_core::Beats::new(4.0).unwrap();
    let asset = demo_project().assets[0].clone();
    let asset_id = asset.id;
    project.assets.push(asset);
    let mut vm = ProjectViewModel::from_project(project).unwrap();

    vm.apply(Intent::AddAssetClip {
        asset_id,
        beat: 4.0,
        track: None,
        tempo_sync: None,
    });
    assert!((vm.current_composition().length_beats - 8.0).abs() < f32::EPSILON);
    assert!((vm.current_composition().tracks[0].clips[0].start - 4.0).abs() < f32::EPSILON);
}

#[test]
fn loop_advance_preserves_overshoot() {
    let mut vm = ProjectViewModel::demo();
    vm.transport.playing = true;
    vm.transport.playhead = 95.0;
    vm.advance(1.0);
    assert!((vm.transport.playhead - 1.0).abs() < f32::EPSILON);
    vm.advance(100.0);
    assert!((vm.transport.playhead - 9.0).abs() < f32::EPSILON);
}

#[test]
fn deleting_then_drawing_a_loop_replaces_it() {
    let mut vm = ProjectViewModel::demo();
    vm.transport.loop_start = 3.25;
    vm.transport.loop_end = 11.5;

    vm.apply(Intent::DeleteLoop);
    assert!(!vm.transport.loop_enabled);

    vm.apply(Intent::SetLoopRange {
        start: 7.0,
        end: 15.0,
    });
    assert!(vm.transport.loop_enabled);
    assert_eq!(
        (vm.transport.loop_start, vm.transport.loop_end),
        (7.0, 15.0)
    );
}

#[test]
fn agent_highlight_fades_and_expires() {
    let mut vm = ProjectViewModel::demo();
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
    let mut vm = ProjectViewModel::demo();
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
fn project_sample_rate_is_a_canonical_undoable_edit() {
    let mut vm = ProjectViewModel::demo();
    let original = vm.project.sample_rate;
    vm.apply(Intent::SetProjectSampleRate(44_100));
    assert_eq!(vm.project.sample_rate.value(), 44_100);
    let update = vm.take_updates().next().expect("sample-rate update");
    assert!(
        update
            .transaction
            .expect("canonical transaction")
            .affects_render()
    );

    vm.apply(Intent::Undo(1.0));
    assert_eq!(vm.project.sample_rate, original);
    vm.apply(Intent::Redo(2.0));
    assert_eq!(vm.project.sample_rate.value(), 44_100);
}

#[test]
fn projection_sorts_clips_but_selection_resolves_canonical_id() {
    let mut project = demo_project();
    project.tracks[0].clips.swap(0, 2);
    let mut vm = ProjectViewModel::from_project(project).expect("reordered project is valid");
    vm.apply(Intent::Select(Selection::Clip { track: 0, clip: 0 }));
    let selected_view = vm.current_composition().tracks[0].clips[0].id.clone();
    let StableSelection::Clip { clip_id, .. } = vm.stable_selection() else {
        panic!("clip selection should resolve");
    };
    assert_eq!(clip_id.to_string(), selected_view);
}

#[test]
fn invalid_external_transaction_is_atomic_and_silent() {
    let mut vm = ProjectViewModel::demo();
    let before = vm.project.clone();
    let revision = vm.revision();
    let asset_id = vm.project.assets[0].id;
    let transaction = Transaction::named("invalid removal", [Command::RemoveAsset { asset_id }]);
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
    let mut vm = ProjectViewModel::demo();
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
    let mut vm = ProjectViewModel::demo();
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
    let mut vm = ProjectViewModel::demo();
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
    let mut vm = ProjectViewModel::demo();
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
    let mut vm = ProjectViewModel::demo();
    vm.set_asset_tempo(0, Some(98.0), 0.1);
    assert!((vm.project.assets[0].tempo.expect("tempo").bpm.value() - 98.0).abs() < f64::EPSILON);

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
fn bulk_note_add_is_one_undoable_edit() {
    let mut vm = ProjectViewModel::demo();
    let before = vm.project.clone();
    let revision = vm.revision();
    let note_count = match &vm.current_composition().tracks[1].clips[0].kind {
        ClipKind::Event { notes } => notes.len(),
        _ => panic!("event clip"),
    };

    vm.apply(Intent::AddNotes {
        track: 1,
        clip: 0,
        notes: vec![
            NoteInsert {
                start: 1.125,
                length: 0.375,
                pitch: 96,
                velocity: 73,
            },
            NoteInsert {
                start: 2.625,
                length: 0.5,
                pitch: 97,
                velocity: 84,
            },
        ],
    });

    let ClipKind::Event { notes } = &vm.current_composition().tracks[1].clips[0].kind else {
        panic!("event clip");
    };
    assert_eq!(vm.revision(), revision + 1);
    assert_eq!(notes.len(), note_count + 2);
    assert!(notes.iter().any(|note| note.pitch == 96));
    assert!(notes.iter().any(|note| note.pitch == 97));

    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.project, before);
}

#[test]
fn bulk_note_edits_use_original_event_indices_before_sorting() {
    let mut vm = ProjectViewModel::demo();
    let original = match &vm.current_composition().tracks[1].clips[0].kind {
        ClipKind::Event { notes } => [notes[0], notes[1]],
        _ => panic!("event clip"),
    };
    let revision = vm.revision();

    vm.apply(Intent::EditNotes {
        track: 1,
        clip: 0,
        notes: vec![
            NoteUpdate {
                event_index: original[0].event_index,
                start: 4.0,
                length: 0.75,
                pitch: 100,
                velocity: 61,
            },
            NoteUpdate {
                event_index: original[1].event_index,
                start: 0.125,
                length: 0.5,
                pitch: 101,
                velocity: 62,
            },
        ],
    });

    let ClipKind::Event { notes } = &vm.current_composition().tracks[1].clips[0].kind else {
        panic!("event clip");
    };
    assert_eq!(vm.revision(), revision + 1);
    assert!(notes.windows(2).all(|pair| pair[0].start <= pair[1].start));
    assert!(notes.iter().any(|note| {
        note.pitch == 100
            && (note.start - 4.0).abs() < f32::EPSILON
            && (note.length - 0.75).abs() < f32::EPSILON
    }));
    assert!(notes.iter().any(|note| {
        note.pitch == 101
            && (note.start - 0.125).abs() < f32::EPSILON
            && (note.length - 0.5).abs() < f32::EPSILON
    }));
}

#[test]
fn bulk_note_delete_uses_original_indices_and_deduplicates_them() {
    let mut vm = ProjectViewModel::demo();
    let before = vm.project.clone();
    let (note_count, indices) = match &vm.current_composition().tracks[1].clips[0].kind {
        ClipKind::Event { notes } => (
            notes.len(),
            vec![
                notes[0].event_index,
                notes[2].event_index,
                notes[0].event_index,
            ],
        ),
        _ => panic!("event clip"),
    };
    let revision = vm.revision();

    vm.apply(Intent::DeleteNotes {
        track: 1,
        clip: 0,
        event_indices: indices,
    });

    let remaining = match &vm.current_composition().tracks[1].clips[0].kind {
        ClipKind::Event { notes } => notes.len(),
        _ => panic!("event clip"),
    };
    assert_eq!(vm.revision(), revision + 1);
    assert_eq!(remaining, note_count - 2);

    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.project, before);
}

#[test]
fn invalid_bulk_note_edit_is_atomic() {
    let mut vm = ProjectViewModel::demo();
    let before = vm.project.clone();
    let revision = vm.revision();
    let event_index = match &vm.current_composition().tracks[1].clips[0].kind {
        ClipKind::Event { notes } => notes[0].event_index,
        _ => panic!("event clip"),
    };

    vm.apply(Intent::EditNotes {
        track: 1,
        clip: 0,
        notes: vec![
            NoteUpdate {
                event_index,
                start: 1.0,
                length: 1.0,
                pitch: 110,
                velocity: 100,
            },
            NoteUpdate {
                event_index: usize::MAX,
                start: 2.0,
                length: 1.0,
                pitch: 111,
                velocity: 100,
            },
        ],
    });

    assert_eq!(vm.revision(), revision);
    assert_eq!(vm.project, before);
}

#[test]
fn starter_clip_effect_workflow_preserves_owner_and_json() {
    let catalog = ProjectViewModel::processor_catalog();
    for track in 0..3 {
        let mut vm = ProjectViewModel::demo();
        let stack = vm.clip_stack(track, 0).expect("each clip kind has a stack");
        let before = vm.project.clone();
        let first = processor_stack(&vm.project, &stack).unwrap().len();
        for (offset, type_id) in ["gaw.pitch_shift", "gaw.saturator", "gaw.bitcrusher"]
            .into_iter()
            .enumerate()
        {
            let catalog_index = catalog.iter().position(|(id, _)| id == type_id).unwrap();
            vm.insert_processor(stack.clone(), catalog_index);
            assert_eq!(
                vm.selection,
                Selection::Effect {
                    track,
                    clip: 0,
                    effect: first + offset
                }
            );
            if offset == 0 {
                let parameter = vm
                    .selected_processor_view()
                    .unwrap()
                    .parameters
                    .iter()
                    .position(|parameter| parameter.id == "semitones")
                    .unwrap();
                vm.set_selected_processor_parameter(parameter, serde_json::json!(-7));
                let pitch =
                    serde_json::to_value(&processor_stack(&vm.project, &stack).unwrap()[first])
                        .unwrap();
                assert_eq!(pitch["parameters"]["semitones"], -7);
                assert_eq!(pitch["parameters"]["quality"], "signalsmith");
            }
        }
        let selected_id = vm.stable_selection();
        vm.move_processor_at(stack.clone(), first + 2, -1);
        assert_eq!(vm.stable_selection(), selected_id);
        assert_eq!(
            vm.selection,
            Selection::Effect {
                track,
                clip: 0,
                effect: first + 1
            }
        );
        vm.toggle_processor_at(stack.clone(), first + 1);
        assert!(!processor_stack(&vm.project, &stack).unwrap()[first + 1].enabled);
        let edited = vm.project.clone();
        vm.remove_processor_at(stack.clone(), first + 1);
        assert_eq!(vm.selection, Selection::Clip { track, clip: 0 });
        vm.apply(Intent::Undo(0.0));
        assert_eq!(vm.project, edited);
        let json = serde_json::to_vec(&vm.project).unwrap();
        let reopened: Project = serde_json::from_slice(&json).unwrap();
        reopened.validate().unwrap();
        assert_eq!(reopened, edited);
        // Three inserts, one pitch edit, one reorder and one bypass.
        for _ in 0..6 {
            vm.apply(Intent::Undo(0.0));
        }
        assert_eq!(vm.project, before);
        assert!(vm.last_error().is_none());
    }
}

#[test]
fn every_processor_scope_maps_and_uses_typed_commands() {
    let mut vm = ProjectViewModel::demo();
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
    let mut vm = ProjectViewModel::demo();

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

    let catalog = ProjectViewModel::processor_catalog();
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
        let mut vm = ProjectViewModel::demo();
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
    let mut vm = ProjectViewModel::demo();
    let asset_waveform = Arc::clone(&vm.assets[0].waveform);
    vm.apply(Intent::SetBpm(121.0));
    assert!(Arc::ptr_eq(&asset_waveform, &vm.assets[0].waveform));
    assert!(
        !vm.current_composition().tracks[0].clips[0]
            .waveform
            .is_empty()
    );
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

#[test]
fn audio_waveform_uses_source_range_and_reverse() {
    let project = demo_project();
    let asset = project
        .assets
        .iter()
        .find(|asset| {
            matches!(
                asset.definition,
                gaw_core::AudioAssetDefinition::Imported(_)
            )
        })
        .expect("imported demo asset");
    let duration = asset_duration(asset).expect("asset duration");
    let source_duration = duration / 4.0;
    let clip_beats = source_duration * project.bpm.value() / 60.0;
    let mut clip = gaw_core::AudioClip::new(
        asset.id,
        gaw_core::Beats::new(0.0).unwrap(),
        gaw_core::Beats::new(clip_beats).unwrap(),
        gaw_core::SourceRange {
            start: gaw_core::Seconds::new(duration / 4.0).unwrap(),
            duration: gaw_core::Seconds::new(source_duration).unwrap(),
        },
    );
    let waveform: Arc<[WaveformPoint]> = (0..8)
        .map(|index| WaveformPoint {
            minimum: -(index as f32),
            maximum: index as f32,
        })
        .collect::<Vec<_>>()
        .into();
    let forward = audio_clip_waveform(&project, asset, &clip, &waveform);
    assert_eq!(forward.as_ref(), &waveform[2..4]);
    clip.reverse = true;
    let reversed = audio_clip_waveform(&project, asset, &clip, &waveform);
    assert_eq!(reversed.as_ref(), &[waveform[3], waveform[2]]);
}

#[test]
fn transcribed_event_data_becomes_an_undoable_midi_asset() {
    let mut vm = ProjectViewModel::demo();
    let mut first = gaw_core::EventData::new("Guitar (MIDI)");
    first.events.push(gaw_core::Event::Note(
        gaw_core::NoteEvent::new(
            gaw_core::Beats::new(0.0).unwrap(),
            gaw_core::Beats::new(1.0).unwrap(),
            60,
            100,
        )
        .unwrap(),
    ));
    assert_eq!(
        vm.add_transcribed_event_data(first).unwrap(),
        "Guitar (MIDI)"
    );
    assert_eq!(vm.midi_assets.last().unwrap().note_count, 1);
    assert!(matches!(vm.selection, Selection::MidiAsset(_)));

    let second = gaw_core::EventData::new("Guitar (MIDI)");
    assert_eq!(
        vm.add_transcribed_event_data(second).unwrap(),
        "Guitar (MIDI 2)"
    );
    assert_eq!(vm.midi_assets.last().unwrap().name, "Guitar (MIDI 2)");

    vm.apply(Intent::Undo(1.0));
    assert!(
        vm.midi_assets
            .iter()
            .all(|asset| asset.name != "Guitar (MIDI 2)")
    );
}

#[test]
fn creating_midi_from_assets_and_tracks_preserves_asset_ownership() {
    let mut vm = ProjectViewModel::demo();
    let initial_asset_count = vm.project.event_data.len();
    let initial_track_count = vm.current_composition().tracks.len();
    let initial_clip_count = vm
        .current_composition()
        .tracks
        .iter()
        .map(|track| track.clips.len())
        .sum::<usize>();

    vm.apply(Intent::CreateMidiAsset);

    assert_eq!(vm.project.event_data.len(), initial_asset_count + 1);
    assert_eq!(vm.current_composition().tracks.len(), initial_track_count);
    assert_eq!(
        vm.current_composition()
            .tracks
            .iter()
            .map(|track| track.clips.len())
            .sum::<usize>(),
        initial_clip_count
    );
    assert_eq!(vm.midi_assets.last().unwrap().name, "MIDI 1");
    assert!(matches!(vm.selection, Selection::MidiAsset(_)));

    vm.apply(Intent::Undo(1.0));
    assert_eq!(vm.project.event_data.len(), initial_asset_count);

    let event_track = vm
        .current_composition()
        .tracks
        .iter()
        .position(|track| track.kind == TrackKind::Event)
        .expect("demo has an event track");
    let event_track_clips = vm.current_composition().tracks[event_track].clips.len();
    vm.apply(Intent::CreateMidiClip {
        beat: 80.0,
        track: event_track,
    });

    assert_eq!(vm.project.event_data.len(), initial_asset_count + 1);
    assert_eq!(vm.current_composition().tracks.len(), initial_track_count);
    assert_eq!(
        vm.current_composition().tracks[event_track].clips.len(),
        event_track_clips + 1
    );
    let Selection::Clip { track, clip } = vm.selection else {
        panic!("new MIDI clip should be selected");
    };
    assert_eq!(track, event_track);
    assert!((vm.current_composition().tracks[track].clips[clip].start - 80.0).abs() < f32::EPSILON);
    assert!(
        (vm.current_composition().tracks[track].clips[clip].length
            - vm.transport.time_signature.quarter_notes_per_bar() as f32)
            .abs()
            < f32::EPSILON
    );
    assert_eq!(vm.editor_kind(), EditorKind::PianoRoll);

    vm.apply(Intent::Undo(2.0));
    assert_eq!(vm.project.event_data.len(), initial_asset_count);
    assert_eq!(
        vm.current_composition().tracks[event_track].clips.len(),
        event_track_clips
    );

    vm.apply(Intent::CreateMidiTrack { beat: 16.0 });

    assert_eq!(vm.project.event_data.len(), initial_asset_count + 1);
    assert_eq!(
        vm.current_composition().tracks.len(),
        initial_track_count + 1
    );
    let Selection::Clip { track, clip } = vm.selection else {
        panic!("new MIDI track clip should be selected");
    };
    let new_track = &vm.current_composition().tracks[track];
    assert_eq!(new_track.kind, TrackKind::Event);
    assert_eq!(new_track.name, "MIDI 1");
    assert!(new_track.sampler_zones.is_empty());
    assert!(matches!(new_track.clips[clip].kind, ClipKind::Event { .. }));

    vm.apply(Intent::Undo(3.0));
    assert_eq!(vm.project.event_data.len(), initial_asset_count);
    assert_eq!(vm.current_composition().tracks.len(), initial_track_count);
}

#[test]
fn dropping_midi_assets_creates_an_editable_event_clip() {
    let mut vm = ProjectViewModel::demo();
    let event_data_id = vm.project.event_data[0].id;
    let event_track = vm
        .current_composition()
        .tracks
        .iter()
        .position(|track| track.kind == TrackKind::Event)
        .expect("demo has an event track");

    vm.apply(Intent::AddEventDataClip {
        event_data_id,
        beat: 8.0,
        track: Some(event_track),
    });

    let Selection::Clip { track, clip } = vm.selection else {
        panic!("dropped MIDI clip should be selected");
    };
    assert_eq!(track, event_track);
    assert!(matches!(
        vm.current_composition().tracks[track].clips[clip].kind,
        ClipKind::Event { .. }
    ));
    assert_eq!(vm.editor_kind(), EditorKind::PianoRoll);
}

#[test]
fn asset_folder_edits_persist_canonical_indices_and_are_undoable() {
    let mut vm = ProjectViewModel::demo();
    let asset_ids = vm
        .project
        .assets
        .iter()
        .map(|asset| asset.id)
        .collect::<Vec<_>>();
    let midi_id = vm.project.event_data[0].id;

    let folder_id = vm
        .create_asset_folder(" Drums ", Some(1))
        .expect("folder created");
    assert_eq!(vm.asset_folders()[0].name, "Drums");
    assert_eq!(vm.asset_folders()[0].asset_ids, vec![asset_ids[1]]);
    assert_eq!(
        vm.project
            .assets
            .iter()
            .map(|asset| asset.id)
            .collect::<Vec<_>>(),
        asset_ids
    );

    vm.move_midi_asset_to_folder(0, Some(folder_id));
    assert_eq!(vm.asset_folders()[0].event_data_ids, vec![midi_id]);
    vm.rename_asset_folder(folder_id, "Rhythm");
    assert_eq!(vm.asset_folders()[0].name, "Rhythm");

    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.asset_folders()[0].name, "Drums");
    vm.apply(Intent::Undo(0.0));
    assert!(vm.asset_folders()[0].event_data_ids.is_empty());
    vm.apply(Intent::Undo(0.0));
    assert!(vm.asset_folders().is_empty());
}

#[test]
fn creating_a_folder_moves_membership_and_filed_assets_can_be_deleted() {
    let mut vm = ProjectViewModel::demo();
    let mut unreferenced = vm.project.assets[0].clone();
    unreferenced.id = AssetId::new();
    unreferenced.name = "Unreferenced".into();
    let asset_id = unreferenced.id;
    vm.commit_ui(
        &Transaction::named(
            "Add test asset",
            [Command::AddAsset {
                asset: unreferenced,
            }],
        ),
        &[asset_id.to_string()],
    );
    let asset_index = vm.project.assets.len() - 1;
    let first = vm
        .create_asset_folder("First", Some(asset_index))
        .expect("first folder");
    let second = vm
        .create_asset_folder("Second", Some(asset_index))
        .expect("second folder");
    assert!(
        vm.asset_folders()
            .iter()
            .find(|folder| folder.id == first)
            .expect("first remains")
            .asset_ids
            .is_empty()
    );
    assert_eq!(
        vm.asset_folders()
            .iter()
            .find(|folder| folder.id == second)
            .expect("second remains")
            .asset_ids,
        vec![asset_id]
    );

    vm.remove_asset(asset_index);
    assert!(vm.project.assets.iter().all(|asset| asset.id != asset_id));
    assert!(
        vm.asset_folders()
            .iter()
            .all(|folder| !folder.asset_ids.contains(&asset_id))
    );
    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.asset_id(asset_index), Some(asset_id));
    assert!(
        vm.asset_folders()
            .iter()
            .find(|folder| folder.id == second)
            .expect("second restored")
            .asset_ids
            .contains(&asset_id)
    );
}

#[test]
fn mixed_asset_folder_move_and_delete_are_atomic_and_undoable() {
    let mut vm = ProjectViewModel::demo();
    let mut first_audio = vm.project.assets[0].clone();
    first_audio.id = AssetId::new();
    first_audio.name = "Bulk audio one".into();
    let first_audio_id = first_audio.id;
    let mut second_audio = vm.project.assets[0].clone();
    second_audio.id = AssetId::new();
    second_audio.name = "Bulk audio two".into();
    let second_audio_id = second_audio.id;
    vm.commit_ui(
        &Transaction::named(
            "Add bulk test assets",
            [
                Command::AddAsset { asset: first_audio },
                Command::AddAsset {
                    asset: second_audio,
                },
            ],
        ),
        &[first_audio_id.to_string(), second_audio_id.to_string()],
    );
    vm.apply(Intent::CreateMidiAsset);
    let midi_id = vm.project.event_data.last().expect("new MIDI asset").id;
    let audio = [vm.project.assets.len() - 2, vm.project.assets.len() - 1];
    let midi = [vm.project.event_data.len() - 1];

    let folder_id = vm
        .create_asset_folder_for_assets("Bulk", &audio, &midi)
        .expect("folder created");
    let folder = vm
        .asset_folders()
        .iter()
        .find(|folder| folder.id == folder_id)
        .expect("folder exists");
    assert_eq!(
        folder.asset_ids.iter().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([first_audio_id, second_audio_id])
    );
    assert_eq!(folder.event_data_ids, vec![midi_id]);

    let revision = vm.revision();
    vm.remove_assets(&audio, &midi);
    assert_eq!(vm.revision(), revision + 1);
    assert!(
        vm.project
            .assets
            .iter()
            .all(|asset| asset.id != first_audio_id && asset.id != second_audio_id)
    );
    assert!(vm.project.event_data.iter().all(|data| data.id != midi_id));
    assert!(vm.asset_folders().is_empty());

    vm.apply(Intent::Undo(0.0));
    assert!(
        vm.project
            .assets
            .iter()
            .any(|asset| asset.id == first_audio_id)
    );
    assert!(
        vm.project
            .assets
            .iter()
            .any(|asset| asset.id == second_audio_id)
    );
    assert!(vm.project.event_data.iter().any(|data| data.id == midi_id));
    let folder = &vm.asset_folders()[0];
    assert_eq!(
        folder.asset_ids.iter().copied().collect::<BTreeSet<_>>(),
        BTreeSet::from([first_audio_id, second_audio_id])
    );
    assert_eq!(folder.event_data_ids, vec![midi_id]);
}

#[test]
fn setting_selected_asset_tempos_is_one_undoable_edit() {
    let mut vm = ProjectViewModel::demo();
    let original = vm.project.assets[..2]
        .iter()
        .map(|asset| asset.tempo)
        .collect::<Vec<_>>();
    let revision = vm.revision();

    vm.set_assets_tempo(&[0, 1], 137.5);

    assert_eq!(vm.revision(), revision + 1);
    assert!(vm.project.assets[..2].iter().all(|asset| {
        asset
            .tempo
            .is_some_and(|tempo| (tempo.bpm.value() - 137.5).abs() < f64::EPSILON)
    }));
    vm.apply(Intent::Undo(0.0));
    assert_eq!(
        vm.project.assets[..2]
            .iter()
            .map(|asset| asset.tempo)
            .collect::<Vec<_>>(),
        original
    );
}

#[test]
fn track_group_intents_update_projection_and_are_undoable() {
    let mut vm = ProjectViewModel::demo();
    let first_track = vm.current_track_id(0).expect("first track");
    let second_track = vm.current_track_id(1).expect("second track");

    vm.apply(Intent::CreateTrackGroup {
        track: Some(0),
        name: " Rhythm ".into(),
    });
    let group_id = vm.current_composition().track_groups[0].id;
    assert_eq!(vm.current_composition().track_groups[0].name, "Rhythm");
    assert_eq!(
        vm.current_composition().track_groups[0].track_ids,
        vec![first_track]
    );

    vm.apply(Intent::ToggleTrackGroup { group_id });
    assert!(vm.current_composition().track_groups[0].collapsed);
    vm.apply(Intent::MoveTrackToGroup {
        track: 1,
        group_id: Some(group_id),
    });
    assert_eq!(
        vm.current_composition().track_groups[0].track_ids,
        vec![first_track, second_track]
    );

    vm.apply(Intent::Undo(0.0));
    assert_eq!(
        vm.current_composition().track_groups[0].track_ids,
        vec![first_track]
    );
    vm.apply(Intent::Undo(0.0));
    assert!(!vm.current_composition().track_groups[0].collapsed);
    vm.apply(Intent::Undo(0.0));
    assert!(vm.current_composition().track_groups.is_empty());

    vm.apply(Intent::CreateTrackGroup {
        track: None,
        name: "Empty".into(),
    });
    assert_eq!(vm.current_composition().track_groups[0].name, "Empty");
    assert!(
        vm.current_composition().track_groups[0]
            .track_ids
            .is_empty()
    );
    vm.apply(Intent::Undo(0.0));
    assert!(vm.current_composition().track_groups.is_empty());
}

#[test]
fn track_rename_reorder_and_delete_are_undoable() {
    let mut vm = ProjectViewModel::demo();
    let first_track = vm.current_track_id(0).expect("first track");
    let second_track = vm.current_track_id(1).expect("second track");
    let original_name = vm.current_composition().tracks[0].name.clone();

    vm.apply(Intent::RenameTrack {
        track: 0,
        name: "  Intro Drums  ".into(),
    });
    assert_eq!(vm.current_composition().tracks[0].name, "Intro Drums");
    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.current_composition().tracks[0].name, original_name);

    vm.apply(Intent::ReorderTrack { from: 0, to: 1 });
    assert_eq!(vm.current_track_id(0), Some(second_track));
    assert_eq!(vm.current_track_id(1), Some(first_track));
    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.current_track_id(0), Some(first_track));

    vm.apply(Intent::CreateTrackGroup {
        track: Some(0),
        name: "Rhythm".into(),
    });
    vm.apply(Intent::DeleteTrack { track: 0 });
    assert!(
        vm.project
            .tracks
            .iter()
            .all(|track| track.id != first_track)
    );
    assert!(
        vm.current_composition()
            .track_groups
            .iter()
            .all(|group| !group.track_ids.contains(&first_track))
    );
    assert!(vm.last_error.is_none());

    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.current_track_id(0), Some(first_track));
    assert_eq!(
        vm.current_composition().track_groups[0].track_ids,
        vec![first_track]
    );
}

#[test]
fn clip_rename_is_trimmed_projected_and_undoable() {
    let mut vm = ProjectViewModel::demo();
    let original = vm.current_composition().tracks[0].clips[0].name.clone();

    vm.apply(Intent::RenameClip {
        track: 0,
        clip: 0,
        name: "  Opening Hit  ".into(),
    });
    assert_eq!(
        vm.current_composition().tracks[0].clips[0].name,
        "Opening Hit"
    );
    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.current_composition().tracks[0].clips[0].name, original);

    vm.apply(Intent::RenameClip {
        track: 0,
        clip: 0,
        name: "   ".into(),
    });
    assert_eq!(vm.current_composition().tracks[0].clips[0].name, original);
}

#[test]
#[allow(clippy::too_many_lines)]
fn clip_copy_and_repeated_paste_clone_effects_automation_and_identity() {
    let (mut vm, track_id, source_id, source_processor_id) = automated_clip_vm();
    let source = vm
        .project
        .tracks
        .iter()
        .find(|track| track.id == track_id)
        .unwrap()
        .clips[0]
        .clone();
    let source_lane_id = vm.project.automation[0].id;

    vm.apply(Intent::CopyClip { track: 0, clip: 0 });
    assert_eq!(vm.revision(), 0);
    assert!(vm.has_clip_clipboard());
    vm.apply(Intent::PasteClip {
        track: Some(0),
        beat: 32.0,
    });

    assert_eq!(vm.revision(), 1);
    let StableSelection::Clip {
        clip_id: first_copy_id,
        ..
    } = vm.stable_selection()
    else {
        panic!("pasted clip should be selected");
    };
    assert_ne!(first_copy_id, source_id);
    let first_copy = vm
        .project
        .tracks
        .iter()
        .find(|track| track.id == track_id)
        .unwrap()
        .clips
        .iter()
        .find(|clip| clip.id() == first_copy_id)
        .unwrap();
    let (gaw_core::Clip::Audio(source), gaw_core::Clip::Audio(first_copy)) = (&source, first_copy)
    else {
        panic!("audio copy should remain audio");
    };
    assert_eq!(first_copy.name, source.name);
    assert_eq!(first_copy.duration, source.duration);
    assert_eq!(first_copy.source, source.source);
    assert_eq!(first_copy.fade_in, source.fade_in);
    assert_eq!(first_copy.reverse, source.reverse);
    assert_eq!(first_copy.tempo_sync, source.tempo_sync);
    assert_eq!(first_copy.effects[0].kind, source.effects[0].kind);
    assert_ne!(first_copy.effects[0].id, source_processor_id);
    let first_processor_id = first_copy.effects[0].id.clone();
    let copied_lane = vm
        .project
        .automation
        .iter()
        .find(|lane| is_clip_automation_target(&lane.target, track_id, first_copy_id))
        .expect("copied automation");
    assert_ne!(copied_lane.id, source_lane_id);
    assert!(matches!(
        &copied_lane.target,
        gaw_core::AutomationTarget::AudioClipProcessor {
            processor_id,
            ..
        } if processor_id == &first_processor_id
    ));
    assert!((copied_lane.points[0].time.value() - 33.0).abs() < f64::EPSILON);
    assert!((copied_lane.points[1].time.value() - 35.0).abs() < f64::EPSILON);

    vm.apply(Intent::PasteClip {
        track: Some(0),
        beat: 72.0,
    });
    let StableSelection::Clip {
        clip_id: second_copy_id,
        ..
    } = vm.stable_selection()
    else {
        panic!("second pasted clip should be selected");
    };
    assert_ne!(second_copy_id, first_copy_id);
    let second_processor_id = vm
        .project
        .tracks
        .iter()
        .find(|track| track.id == track_id)
        .unwrap()
        .clips
        .iter()
        .find(|clip| clip.id() == second_copy_id)
        .and_then(|clip| match clip {
            gaw_core::Clip::Audio(clip) => clip.effects.first(),
            gaw_core::Clip::Event(_) | gaw_core::Clip::Composition(_) => None,
        })
        .unwrap()
        .id
        .clone();
    assert_ne!(second_processor_id, first_processor_id);
    vm.project.validate().unwrap();

    vm.apply(Intent::Undo(0.0));
    assert!(
        vm.project
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .all(|clip| clip.id() != second_copy_id)
    );
    vm.apply(Intent::Redo(0.0));
    assert!(
        vm.project
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .any(|clip| clip.id() == second_copy_id)
    );
}

#[test]
fn cutting_an_automated_clip_is_atomic_undoable_and_pasteable() {
    let (mut vm, track_id, clip_id, _) = automated_clip_vm();
    let lane_id = vm.project.automation[0].id;

    vm.apply(Intent::CutClip { track: 0, clip: 0 });

    assert_eq!(vm.revision(), 1);
    assert!(vm.has_clip_clipboard());
    assert!(
        vm.project
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .all(|clip| clip.id() != clip_id)
    );
    assert!(vm.project.automation.iter().all(|lane| lane.id != lane_id));
    vm.apply(Intent::Undo(0.0));
    assert!(
        vm.project
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .any(|clip| clip.id() == clip_id)
    );
    assert!(vm.project.automation.iter().any(|lane| lane.id == lane_id));

    vm.apply(Intent::PasteClip {
        track: Some(0),
        beat: 32.0,
    });
    assert!(vm.project.tracks.iter().any(|track| {
        track.id == track_id && track.clips.iter().any(|clip| clip.id() != clip_id)
    }));
    assert!(vm.last_error().is_none());
}

#[test]
fn event_clip_copy_remaps_effect_automation_after_reopening() {
    let mut vm = ProjectViewModel::demo();
    let stack = vm.clip_stack(1, 0).unwrap();
    let gain = ProjectViewModel::processor_catalog()
        .iter()
        .position(|(id, _)| id == "gaw.gain")
        .unwrap();
    vm.insert_processor(stack.clone(), gain);
    let ProcessorStack::Clip { track_id, clip_id } = stack else {
        panic!("event clip scope")
    };
    let processor_id = processor_stack(&vm.project, &stack).unwrap()[0].id.clone();
    let mut project = vm.project.clone();
    project.automation.push(gaw_core::AutomationLane {
        id: gaw_core::AutomationLaneId::new(),
        composition_id: project.root_composition_id,
        name: "Event output gain".into(),
        target: gaw_core::AutomationTarget::AudioClipProcessor {
            track_id,
            clip_id,
            processor_id: processor_id.clone(),
            parameter_id: "gain_db".into(),
        },
        points: vec![gaw_core::AutomationPoint {
            time: gaw_core::Beats::new(1.0).unwrap(),
            value: gaw_core::AutomationValue::Decibels(gaw_core::Decibels::new(-6.0).unwrap()),
            curve: gaw_core::AutomationCurve::Linear,
        }],
    });
    let mut vm = ProjectViewModel::from_project(project.clone()).unwrap();
    // Reopening resets session revision; adding the same type must still get a fresh ID.
    vm.insert_processor(stack.clone(), gain);
    assert_ne!(
        processor_stack(&vm.project, &stack).unwrap()[1].id,
        processor_id
    );
    vm.apply(Intent::Undo(0.0));
    vm.apply(Intent::CopyClip { track: 1, clip: 0 });
    vm.apply(Intent::PasteClip {
        track: Some(1),
        beat: 72.0,
    });
    let Selection::Clip { track, clip } = vm.selection else {
        panic!("pasted clip selected")
    };
    let copy_stack = vm.clip_stack(track, clip).unwrap();
    let ProcessorStack::Clip {
        clip_id: copy_id, ..
    } = copy_stack
    else {
        panic!("event clip scope")
    };
    let copy_processor = &processor_stack(&vm.project, &copy_stack).unwrap()[0];
    assert_ne!(copy_processor.id, processor_id);
    assert_eq!(
        copy_processor.kind,
        processor_stack(&project, &stack).unwrap()[0].kind
    );
    assert!(vm.project.automation.iter().any(|lane| matches!(
        &lane.target,
        gaw_core::AutomationTarget::AudioClipProcessor { clip_id, processor_id, .. }
            if *clip_id == copy_id && *processor_id == copy_processor.id
    )));
    let clips = &vm
        .project
        .tracks
        .iter()
        .find(|track| track.id == track_id)
        .unwrap()
        .clips;
    let event_source = |id| {
        clips
            .iter()
            .find_map(|clip| match clip {
                gaw_core::Clip::Event(clip) if clip.id == id => Some(clip.event_data_id),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(event_source(clip_id), event_source(copy_id));
    vm.project.validate().unwrap();
    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.project, project);
    vm.apply(Intent::Redo(0.0));
    vm.project.validate().unwrap();
    assert!(vm.last_error().is_none());
}

#[test]
fn duplicate_supports_every_clip_kind_and_extends_the_composition() {
    for (track, clip) in [(0, 0), (1, 0), (2, 0)] {
        let mut vm = ProjectViewModel::demo();
        let original_id = vm.current_composition().tracks[track].clips[clip]
            .id
            .clone();
        vm.apply(Intent::DuplicateClip { track, clip });
        assert_eq!(vm.revision(), 1);
        let Selection::Clip {
            track: pasted_track,
            clip: pasted_clip,
        } = vm.selection
        else {
            panic!("duplicate should be selected");
        };
        assert_eq!(pasted_track, track);
        assert_ne!(
            vm.current_composition().tracks[pasted_track].clips[pasted_clip].id,
            original_id
        );
        assert!(vm.last_error().is_none());
    }

    let mut project = demo_project();
    project
        .compositions
        .iter_mut()
        .find(|composition| composition.id == project.root_composition_id)
        .unwrap()
        .length = gaw_core::Beats::new(80.0).unwrap();
    let mut vm = ProjectViewModel::from_project(project).unwrap();
    vm.apply(Intent::DuplicateClip { track: 0, clip: 2 });
    assert!((vm.current_composition().length_beats - 96.0).abs() < f32::EPSILON);
    vm.apply(Intent::Undo(0.0));
    assert!((vm.current_composition().length_beats - 80.0).abs() < f32::EPSILON);
}

#[test]
fn incompatible_clip_paste_is_a_no_op() {
    let mut vm = ProjectViewModel::demo();
    vm.apply(Intent::CopyClip { track: 0, clip: 0 });
    assert!(!vm.can_paste_clip_to(1));
    vm.apply(Intent::PasteClip {
        track: Some(1),
        beat: 0.0,
    });
    assert_eq!(vm.revision(), 0);
}

#[test]
fn keyboard_paste_after_cut_returns_to_the_source_track() {
    let mut vm = ProjectViewModel::demo();
    vm.apply(Intent::Select(Selection::Clip { track: 3, clip: 0 }));
    vm.apply(Intent::CutClip { track: 3, clip: 0 });
    vm.apply(Intent::PasteClip {
        track: None,
        beat: 80.0,
    });

    assert!(matches!(vm.selection, Selection::Clip { track: 3, .. }));
    assert!(vm.last_error().is_none());
}

#[test]
fn bar_timeline_gap_edits_are_projected_and_undoable() {
    let mut vm = ProjectViewModel::demo();
    vm.apply(Intent::AddBarTimelineGap {
        start: 4.0,
        duration: 2.0,
    });
    assert_eq!(
        vm.current_composition().bar_timeline_gaps,
        vec![BarTimelineGap {
            start: 4.0,
            duration: 2.0,
        }]
    );
    assert!((vm.current_composition().counted_beat_at(5.0) - 4.0).abs() < f32::EPSILON);
    assert!((vm.current_composition().counted_beat_at(8.0) - 6.0).abs() < f32::EPSILON);

    vm.apply(Intent::UpdateBarTimelineGap {
        index: 0,
        start: 3.0,
        duration: 4.0,
    });
    assert!((vm.current_composition().bar_timeline_gaps[0].start - 3.0).abs() < f32::EPSILON);
    assert!((vm.current_composition().bar_timeline_gaps[0].duration - 4.0).abs() < f32::EPSILON);

    vm.apply(Intent::Undo(0.0));
    assert!((vm.current_composition().bar_timeline_gaps[0].start - 4.0).abs() < f32::EPSILON);
    vm.apply(Intent::DeleteBarTimelineGap { index: 0 });
    assert!(vm.current_composition().bar_timeline_gaps.is_empty());
    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.current_composition().bar_timeline_gaps.len(), 1);
}

#[test]
fn overlapping_bar_timeline_gap_is_rejected_atomically() {
    let mut vm = ProjectViewModel::demo();
    vm.apply(Intent::AddBarTimelineGap {
        start: 4.0,
        duration: 4.0,
    });
    vm.apply(Intent::AddBarTimelineGap {
        start: 6.0,
        duration: 2.0,
    });
    assert_eq!(vm.current_composition().bar_timeline_gaps.len(), 1);
    assert!(vm.last_error().is_some());
}

#[test]
fn external_reload_syncs_canonical_transport_settings_and_preserves_playback() {
    let mut vm = ProjectViewModel::demo();
    vm.transport.playing = true;
    vm.transport.playhead = 2.0;
    let mut project = vm.project().clone();
    project.time_signature = gaw_core::TimeSignature::new(3, 4).unwrap();
    project.settings.metronome_enabled = true;
    project.settings.metronome_gain = gaw_core::Ratio::new(0.25).unwrap();
    project.settings.master_volume = gaw_core::Decibels::new(-12.0).unwrap();
    vm.replace_project_from_agent(project.clone(), [], 1.0)
        .unwrap();
    assert_eq!(vm.transport.time_signature, project.time_signature);
    assert!(vm.transport.metronome_enabled);
    assert!((vm.transport.metronome_gain - 0.25).abs() < f32::EPSILON);
    assert!((vm.transport.master_volume_db + 12.0).abs() < f32::EPSILON);
    assert!(vm.transport.playing);
    assert!((vm.transport.playhead - 2.0).abs() < f32::EPSILON);
}

#[test]
fn native_projection_never_invents_waveforms_and_rejects_stale_results() {
    let mut vm = ProjectViewModel::from_project(demo_project()).unwrap();
    assert!(vm.assets.iter().all(|asset| asset.waveform.is_empty()));
    assert!(
        vm.compositions
            .iter()
            .flat_map(|c| &c.tracks)
            .flat_map(|t| &t.clips)
            .all(|clip| clip.waveform.is_empty())
    );
    let id = vm.assets[0].id.clone();
    let hash = vm.assets[0].content_hash.clone().unwrap();
    let points: Arc<[WaveformPoint]> = Arc::from([WaveformPoint {
        minimum: -0.2,
        maximum: 0.5,
    }]);
    vm.install_asset_waveform(&id, &hash, Arc::clone(&points));
    assert!(Arc::ptr_eq(&vm.assets[0].waveform, &points));

    let mut project = vm.project().clone();
    let gaw_core::AudioAssetDefinition::Imported(source) = &mut project.assets[0].definition else {
        panic!("expected imported fixture asset");
    };
    source.content_hash = gaw_core::ContentHash::new("a".repeat(64)).unwrap();
    vm.replace_project_from_agent(project, [], 1.0).unwrap();
    assert!(vm.assets[0].waveform.is_empty());
    vm.install_asset_waveform(&id, &hash, points);
    assert!(vm.assets[0].waveform.is_empty());
    assert!(
        vm.current_composition().tracks[0].clips[0]
            .waveform
            .is_empty()
    );
}

#[test]
fn generated_asset_projection_uses_revision_channel_layout() {
    let mut project = demo_project();
    let revision = gaw_core::AudioAssetRevision {
        id: gaw_core::AssetRevisionId::new(),
        content_hash: gaw_core::ContentHash::new("a".repeat(64)).unwrap(),
        definition_hash: gaw_core::ContentHash::new("b".repeat(64)).unwrap(),
        dependency_revision_ids: vec![],
        render_context: gaw_core::RenderContext {
            sample_rate: project.sample_rate,
            layout: gaw_core::ChannelLayout::Mono,
            bpm: project.bpm,
            requested_range: None,
            engine_version: "test".into(),
            random_seed: 0,
        },
        media_path: gaw_core::ProjectPath::new("assets/media/render.wav").unwrap(),
        frames: gaw_core::FrameCount(48_000),
    };
    let asset = project.assets.last_mut().unwrap();
    assert!(matches!(
        asset.definition,
        gaw_core::AudioAssetDefinition::Processed { .. }
    ));
    asset.publish_revision(revision).unwrap();
    let vm = ProjectViewModel::from_project(project).unwrap();
    let projected = vm.assets.last().unwrap();
    assert_eq!(projected.channels, 1);
    assert_eq!(projected.sample_rate, 48_000);
    assert!((projected.duration_seconds - 1.0).abs() < f32::EPSILON);
}

#[test]
fn waveform_completion_only_updates_placements_of_the_matching_source() {
    let mut project = demo_project();
    let mut shared = project
        .tracks
        .iter()
        .flat_map(|track| &track.clips)
        .find_map(|clip| match clip {
            gaw_core::Clip::Audio(clip) if clip.asset_id == project.assets[0].id => {
                Some(clip.clone())
            }
            _ => None,
        })
        .unwrap();
    shared.id = ClipId::new();
    shared.start = gaw_core::Beats::new(0.0).unwrap();
    shared.effects.clear();
    let mut track = gaw_core::Track::audio(project.compositions[1].id, "Shared source");
    track.clips.push(gaw_core::Clip::Audio(shared));
    project.compositions[1].track_ids.push(track.id);
    project.tracks.push(track);
    let mut vm = ProjectViewModel::from_project(project.clone()).unwrap();
    vm.initialize_demo_waveforms();
    let source = &project.assets[0];
    let id = source.id.to_string();
    let hash = vm.assets[0].content_hash.clone().unwrap();
    vm.selection = Selection::Clip { track: 0, clip: 0 };
    let selection = vm.stable_selection();
    vm.compositions[0].tracks[0].level = 0.37;
    for clip in vm
        .compositions
        .iter_mut()
        .flat_map(|c| &mut c.tracks)
        .flat_map(|t| &mut t.clips)
    {
        if let ClipKind::Composition { render, .. } = &mut clip.kind {
            *render = RenderState::Rendering(42);
        }
    }
    let before: HashMap<_, _> = vm
        .compositions
        .iter()
        .flat_map(|c| &c.tracks)
        .flat_map(|t| &t.clips)
        .map(|clip| (clip.id.clone(), Arc::clone(&clip.waveform)))
        .collect();
    let points: Arc<[WaveformPoint]> = Arc::from([
        WaveformPoint {
            minimum: -0.2,
            maximum: 0.5,
        },
        WaveformPoint {
            minimum: -0.4,
            maximum: 0.8,
        },
    ]);
    vm.install_asset_waveform(&id, &hash, Arc::clone(&points));

    let mut changed = 0;
    for clip in vm
        .compositions
        .iter()
        .flat_map(|c| &c.tracks)
        .flat_map(|t| &t.clips)
    {
        let canonical = project
            .tracks
            .iter()
            .flat_map(|t| &t.clips)
            .find(|canonical| canonical.id().to_string() == clip.id)
            .unwrap();
        if let gaw_core::Clip::Audio(audio) = canonical
            && audio.asset_id == source.id
        {
            assert_eq!(
                clip.waveform,
                audio_clip_waveform(&project, source, audio, &points)
            );
            changed += 1;
        } else {
            assert!(Arc::ptr_eq(&clip.waveform, &before[&clip.id]));
        }
        if let ClipKind::Composition { render, .. } = clip.kind {
            assert_eq!(render, RenderState::Rendering(42));
        }
    }
    assert!(changed > 1, "shared source placements must all update");
    assert_eq!(vm.stable_selection(), selection);
    assert!((vm.compositions[0].tracks[0].level - 0.37).abs() < f32::EPSILON);
    assert_eq!(vm.project(), &project);
    assert_eq!(vm.revision(), 0);
    assert_eq!(vm.take_updates().count(), 0);
}
