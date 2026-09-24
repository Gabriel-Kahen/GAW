use super::*;
use crate::model::{EditorKind, Intent, Selection};

fn fixture() -> (egui::Context, GawApp, usize) {
    let context = egui::Context::default();
    let mut app = GawApp::with_project_runtime(
        &context,
        crate::model::demo_project(),
        crate::settings::AudioPreferences::default(),
    )
    .unwrap();
    app.vm.apply(Intent::CreateMidiTrack { beat: 0.0 });
    let Selection::Clip { track, .. } = app.vm.selection else {
        panic!("new MIDI clip must remain selected");
    };
    (context, app, track)
}

fn frame(context: &egui::Context, app: &mut GawApp, size: egui::Vec2) {
    let _ = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            ..Default::default()
        },
        |_ui| app.sampler_window(context, 0.0),
    );
}

#[test]
fn modal_is_centered_and_preserves_midi_editor() {
    for size in [egui::vec2(1200.0, 800.0), egui::vec2(980.0, 640.0)] {
        let (context, mut app, track) = fixture();
        app.vm.add_sampler_zone(track);
        let selection = app.vm.selection;
        app.open_sampler(track, 0.0);
        for _ in 0..3 {
            frame(&context, &mut app, size);
        }
        let rect = context
            .memory(|memory| memory.area_rect(egui::Id::new("sampler-modal")))
            .unwrap();
        assert!(
            (rect.center() - (egui::Pos2::ZERO + size * 0.5)).length() < 2.0,
            "{rect:?}"
        );
        assert!(
            egui::Rect::from_min_size(egui::Pos2::ZERO, size).contains_rect(rect),
            "{rect:?}"
        );
        assert_eq!(app.vm.selection, selection);
        assert_eq!(app.vm.editor_kind(), EditorKind::PianoRoll);
    }
}

#[test]
fn deleted_layer_recovers_and_new_layer_stays_unselected() {
    let (_, mut app, track) = fixture();
    app.vm.add_sampler_zone(track);
    app.vm.add_sampler_zone(track);
    app.open_sampler(track, 0.0);
    let mut editor = app.sampler_editor.take().unwrap();
    app.vm.remove_sampler_zone(track, 0);
    editor.sync(&app.vm.current_composition().tracks[track]);
    assert_eq!(
        editor.draft,
        app.vm.current_composition().tracks[track]
            .sampler_zones
            .first()
            .cloned()
    );
    assert!(editor.zone_id.is_some());
    editor.select_zone(None);
    editor.sync(&app.vm.current_composition().tracks[track]);
    assert!(editor.zone_id.is_none());
    assert!(editor.draft.is_none());
    editor.select_zone(
        app.vm.current_composition().tracks[track]
            .sampler_zones
            .first(),
    );
    app.vm.remove_sampler_zone(track, 0);
    editor.sync(&app.vm.current_composition().tracks[track]);
    assert!(editor.zone_id.is_none());
    assert!(editor.draft.is_none());
}

#[test]
fn draft_survives_frames_until_an_external_edit() {
    let (_, mut app, track) = fixture();
    app.vm.add_sampler_zone(track);
    app.open_sampler(track, 0.0);
    let mut editor = app.sampler_editor.take().unwrap();
    let revision = editor.waveform_revision;
    editor.draft.as_mut().unwrap().source_start_seconds = 0.25;
    editor.sync(&app.vm.current_composition().tracks[track]);
    assert_eq!(editor.waveform_revision, revision);
    assert!((editor.draft.as_ref().unwrap().source_start_seconds - 0.25).abs() < f64::EPSILON);
    app.vm.toggle_first_sampler_zone_reverse(track);
    editor.sync(&app.vm.current_composition().tracks[track]);
    assert_eq!(editor.draft, editor.baseline);
    assert!(editor.draft.as_ref().unwrap().reverse);
    assert_ne!(editor.waveform_revision, revision);
}

#[test]
fn clicking_audio_creates_a_playable_layer_and_reclick_preserves_trim() {
    let (context, mut app, track) = fixture();
    app.open_sampler(track, 0.0);
    let name = app.vm.assets[0].name.clone();
    let asset_id = app.vm.assets[0].id.clone();
    let mut render = |app: &mut GawApp, events| {
        context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |_ui| app.sampler_window(&context, 0.0),
        )
    };
    render(&mut app, vec![]);
    render(&mut app, vec![]);
    let output = render(&mut app, vec![]);
    let position = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == name => {
                Some(text.pos + egui::vec2(10.0, 5.0))
            }
            _ => None,
        })
        .expect("audio picker row is visible");
    let click =
        |app: &mut GawApp,
         render: &mut dyn FnMut(&mut GawApp, Vec<egui::Event>) -> egui::FullOutput| {
            for pressed in [true, false] {
                render(
                    app,
                    vec![
                        egui::Event::PointerMoved(position),
                        egui::Event::PointerButton {
                            pos: position,
                            button: egui::PointerButton::Primary,
                            pressed,
                            modifiers: egui::Modifiers::NONE,
                        },
                    ],
                );
            }
        };
    click(&mut app, &mut render);
    let zones = &app.vm.current_composition().tracks[track].sampler_zones;
    assert_eq!(zones.len(), 1);
    assert_eq!(zones[0].asset_id, asset_id);
    assert_eq!((zones[0].low_note, zones[0].high_note), (0, 127));
    let mut trimmed = zones[0].clone();
    trimmed.source_start_seconds = 0.1;
    trimmed.source_duration_seconds = 0.2;
    app.vm.update_sampler_zone(track, 0, &trimmed);
    render(&mut app, vec![]);
    let saved = app.vm.current_composition().tracks[track].sampler_zones[0].clone();
    click(&mut app, &mut render);
    assert_eq!(
        app.vm.current_composition().tracks[track].sampler_zones[0],
        saved
    );
}

#[test]
fn playback_stays_visible_with_a_long_source_name() {
    let (context, mut app, track) = fixture();
    app.vm.add_sampler_zone(track);
    let asset_id = app.vm.current_composition().tracks[track].sampler_zones[0]
        .asset_id
        .clone();
    app.vm
        .assets
        .iter_mut()
        .find(|asset| asset.id == asset_id)
        .unwrap()
        .name = "A very long source recording name ".repeat(20);
    app.open_sampler(track, 0.0);
    let size = egui::vec2(980.0, 640.0);
    frame(&context, &mut app, size);
    frame(&context, &mut app, size);
    let output = context.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            ..Default::default()
        },
        |_ui| app.sampler_window(&context, 0.0),
    );
    let modal = context
        .memory(|memory| memory.area_rect(egui::Id::new("sampler-modal")))
        .unwrap();
    let (clip, text) = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "Play" => {
                Some((shape.clip_rect, text))
            }
            _ => None,
        })
        .expect("Play is visible above the waveform");
    let bounds = egui::Rect::from_min_size(text.pos, text.galley.size());
    assert!(clip.contains_rect(bounds));
    assert!(modal.contains_rect(bounds));
}

#[test]
fn preview_space_shortcut_ignores_typing_and_key_repeat() {
    let context = egui::Context::default();
    let run = |typing, repeat| {
        let mut toggled = false;
        let _ = context.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: Some(egui::Key::Space),
                    pressed: true,
                    repeat,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| {
                let mut text = String::new();
                let field = ui.text_edit_singleline(&mut text);
                if typing {
                    field.request_focus();
                } else {
                    field.surrender_focus();
                }
                toggled = sampler_playback_controls(ui, true, false, false, 0.0, 1.0);
            },
        );
        toggled
    };
    assert!(run(false, false));
    assert!(!run(false, true));
    assert!(!run(true, false));
}

#[test]
fn all_keys_expands_a_legacy_c4_zone_without_changing_the_sample() {
    let (context, mut app, track) = fixture();
    app.vm.add_sampler_zone(track);
    let mut original = app.vm.current_composition().tracks[track].sampler_zones[0].clone();
    original.low_note = 60;
    original.high_note = 60;
    app.vm.update_sampler_zone(track, 0, &original);
    app.open_sampler(track, 0.0);
    let mut render = |events| {
        context.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |_ui| app.sampler_window(&context, 0.0),
        )
    };
    render(vec![]);
    render(vec![]);
    let output = render(vec![]);
    let pos = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "All keys" => {
                Some(text.pos + egui::vec2(5.0, 5.0))
            }
            _ => None,
        })
        .expect("All keys is available without opening Advanced");
    for pressed in [true, false] {
        render(vec![
            egui::Event::PointerMoved(pos),
            egui::Event::PointerButton {
                pos,
                pressed,
                button: egui::PointerButton::Primary,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
    }
    let mut expected = original.clone();
    expected.low_note = 0;
    expected.high_note = 127;
    assert_eq!(
        app.vm.current_composition().tracks[track].sampler_zones[0],
        expected
    );
    app.vm.apply(Intent::Undo(0.0));
    assert_eq!(
        app.vm.current_composition().tracks[track].sampler_zones[0],
        original
    );
}
