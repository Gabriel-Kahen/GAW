#![allow(clippy::float_cmp)]

use super::*;

struct Editor {
    ctx: egui::Context,
    state: PianoRollState,
    clip: Clip,
    notes: Vec<Note>,
    time: f64,
}

impl Editor {
    fn new(notes: Vec<Note>) -> Self {
        Self {
            ctx: egui::Context::default(),
            state: PianoRollState::default(),
            clip: Clip {
                id: "gestures".into(),
                name: "Test clip".into(),
                start: 0.0,
                length: 8.0,
                gain_db: 0.0,
                waveform: std::sync::Arc::from([]),
                kind: crate::model::ClipKind::Event {
                    notes: std::sync::Arc::from([]),
                },
                effects: Vec::new(),
            },
            notes,
            time: 0.0,
        }
    }

    fn grid() -> Rect {
        Rect::from_min_size(Pos2::new(80.0, 100.0), Vec2::new(600.0, 400.0))
    }

    fn frame(
        &mut self,
        events: Vec<egui::Event>,
        full_editor: bool,
    ) -> (Vec<Intent>, egui::FullOutput) {
        self.time += 0.05;
        let mut actions = Vec::new();
        let output = self.ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 700.0))),
                time: Some(self.time),
                events,
                ..Default::default()
            },
            |root| {
                egui::CentralPanel::default().show_inside(root, |ui| {
                    if full_editor {
                        actions.extend(show(
                            ui,
                            &mut self.state,
                            2,
                            3,
                            &self.clip,
                            &self.notes,
                            0.0,
                            4.0,
                            &mut 100,
                        ));
                    } else {
                        let response = ui.allocate_rect(Self::grid(), Sense::click_and_drag());
                        grid_interaction(
                            ui,
                            &mut self.state,
                            &response,
                            2,
                            3,
                            &self.clip,
                            &self.notes,
                            Self::grid(),
                            4.0,
                            100,
                            &mut actions,
                        );
                    }
                });
            },
        );
        (actions, output)
    }

    fn point(&self, beat: f32, pitch: u8) -> Pos2 {
        Pos2::new(
            beat_to_x(&self.state, Self::grid(), beat),
            Self::grid().top()
                + (self.state.top_pitch - f32::from(pitch) + 0.5) * self.state.row_height,
        )
    }

    fn pointer(
        &mut self,
        point: Pos2,
        button: PointerButton,
        pressed: bool,
        full: bool,
    ) -> Vec<Intent> {
        self.frame(
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ],
            full,
        )
        .0
    }

    fn hover(&mut self, point: Pos2) -> Vec<Intent> {
        self.frame(vec![egui::Event::PointerMoved(point)], false).0
    }
}

fn note(index: usize, start: f32) -> Note {
    Note {
        event_index: index,
        start,
        length: 1.0,
        pitch: 60,
        velocity: 0.75,
        cents: 0.0,
    }
}

#[test]
fn drawing_click_commits_once_on_release() {
    let mut editor = Editor::new(vec![]);
    let point = editor.point(1.1, 60);
    assert!(editor.hover(point).is_empty());
    assert!(
        editor
            .pointer(point, PointerButton::Primary, true, false)
            .is_empty()
    );
    assert!(editor.state.drawing.is_some());
    let actions = editor.pointer(point, PointerButton::Primary, false, false);
    assert!(matches!(
        actions.as_slice(),
        [Intent::AddNote {
            track: 2,
            clip: 3,
            start: 1.0,
            length: 0.25,
            pitch: 60,
            velocity: 100,
        }]
    ));
    assert!(editor.state.drawing.is_none());
    assert!(editor.hover(point).is_empty());
}

#[test]
fn drawing_drag_previews_then_commits_length_and_remembers_it() {
    let mut editor = Editor::new(vec![]);
    let start = editor.point(1.0, 60);
    let end = editor.point(2.5, 60);
    editor.hover(start);
    assert!(
        editor
            .pointer(start, PointerButton::Primary, true, false)
            .is_empty()
    );
    assert!(editor.hover(end).is_empty());
    assert_eq!(editor.state.drawing.unwrap().length, 1.5);
    let actions = editor.pointer(end, PointerButton::Primary, false, false);
    assert!(matches!(
        actions.as_slice(),
        [Intent::AddNote {
            start: 1.0,
            length: 1.5,
            ..
        }]
    ));
    assert_eq!(editor.state.last_note_length, Some(1.5));
    assert!(editor.hover(end).is_empty());
}

#[test]
fn moving_selected_notes_keeps_selection_and_commits_one_batch() {
    let mut editor = Editor::new(vec![note(7, 1.0), note(11, 3.0)]);
    editor.state.selected.extend([7, 11]);
    let start = editor.point(1.3, 60);
    let end = start + Vec2::new(72.0, -14.0);
    editor.hover(start);
    assert!(
        editor
            .pointer(start, PointerButton::Primary, true, false)
            .is_empty()
    );
    assert!(editor.hover(end).is_empty());
    let actions = editor.pointer(end, PointerButton::Primary, false, false);
    let [
        Intent::EditNotes {
            track: 2,
            clip: 3,
            notes,
        },
    ] = actions.as_slice()
    else {
        panic!("expected exactly one batch edit: {actions:?}");
    };
    assert_eq!(notes.len(), 2);
    assert_eq!((notes[0].start, notes[1].start), (2.0, 4.0));
    assert!(notes.iter().all(|note| note.pitch == 61));
    assert_eq!(editor.state.selected, BTreeSet::from([7, 11]));
    assert!(editor.hover(end).is_empty());
}

#[test]
fn resizing_selected_notes_keeps_selection_and_commits_one_batch() {
    let mut editor = Editor::new(vec![note(7, 1.0), note(11, 3.0)]);
    editor.state.selected.extend([7, 11]);
    let rect = note_rect(&editor.state, Editor::grid(), &editor.notes[0]);
    let start = Pos2::new(rect.right() - 2.0, rect.center().y);
    let end = start + Vec2::new(36.0, 0.0);
    editor.hover(start);
    assert!(
        editor
            .pointer(start, PointerButton::Primary, true, false)
            .is_empty()
    );
    assert!(editor.hover(end).is_empty());
    let actions = editor.pointer(end, PointerButton::Primary, false, false);
    let [Intent::EditNotes { notes, .. }] = actions.as_slice() else {
        panic!("expected exactly one batch resize: {actions:?}");
    };
    assert_eq!(notes.len(), 2);
    assert!(notes.iter().all(|note| note.length == 1.5));
    assert_eq!(editor.state.selected, BTreeSet::from([7, 11]));
    assert_eq!(editor.state.last_note_length, Some(1.5));
}

#[test]
fn right_drag_erases_each_crossed_note_once_on_release() {
    let mut editor = Editor::new(vec![note(7, 1.0), note(11, 3.0)]);
    let first = editor.point(1.3, 60);
    let second = editor.point(3.3, 60);
    editor.hover(first);
    assert!(
        editor
            .pointer(first, PointerButton::Secondary, true, false)
            .is_empty()
    );
    assert!(editor.hover(second).is_empty());
    assert!(editor.hover(first).is_empty());
    let actions = editor.pointer(first, PointerButton::Secondary, false, false);
    let [
        Intent::DeleteNotes {
            track: 2,
            clip: 3,
            event_indices,
        },
    ] = actions.as_slice()
    else {
        panic!("expected exactly one deletion batch: {actions:?}");
    };
    assert_eq!(event_indices, &[7, 11]);
    assert!(editor.state.erasing.is_none());
    assert!(editor.hover(second).is_empty());
}

#[test]
fn full_editor_header_double_click_expands_and_expand_button_restores() {
    let mut editor = Editor::new(vec![note(7, 1.0)]);
    let header = Pos2::new(450.0, 18.0);
    editor.frame(vec![egui::Event::PointerMoved(header)], true);
    editor.pointer(header, PointerButton::Primary, true, true);
    editor.pointer(header, PointerButton::Primary, false, true);
    assert!(!editor.state.fullscreen);
    editor.pointer(header, PointerButton::Primary, true, true);
    editor.pointer(header, PointerButton::Primary, false, true);
    assert!(editor.state.fullscreen, "header double click should expand");

    let output = editor.frame(vec![], true).1;
    let restore = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Text(text) if text.galley.text() == "RESTORE" => {
                Some(text.pos + text.galley.size() * 0.5)
            }
            _ => None,
        })
        .expect("expanded editor should offer RESTORE");
    editor.frame(vec![egui::Event::PointerMoved(restore)], true);
    editor.pointer(restore, PointerButton::Primary, true, true);
    editor.pointer(restore, PointerButton::Primary, false, true);
    assert!(
        !editor.state.fullscreen,
        "restore button should own its click"
    );
}

#[test]
fn drawing_release_outside_grid_commits_once_and_clamps_to_clip_end() {
    let mut editor = Editor::new(vec![]);
    let start = editor.point(1.0, 60);
    let end = Pos2::new(900.0, start.y);
    editor.hover(start);
    editor.pointer(start, PointerButton::Primary, true, false);
    assert!(editor.hover(end).is_empty());
    let actions = editor.pointer(end, PointerButton::Primary, false, false);
    assert!(matches!(
        actions.as_slice(),
        [Intent::AddNote {
            start: 1.0,
            length: 7.0,
            ..
        }]
    ));
    assert!(editor.state.drawing.is_none());
    assert!(editor.hover(start).is_empty());
}

#[test]
fn full_editor_remaps_moved_selection_after_event_sorting() {
    let mut editor = Editor::new(vec![note(0, 1.0), note(2, 3.0)]);
    let output = editor.frame(vec![], true).1;
    let grid = output
        .shapes
        .iter()
        .find_map(|shape| match &shape.shape {
            egui::Shape::Rect(rect)
                if rect.fill == CANVAS
                    && rect.rect.width() > 500.0
                    && rect.rect.height() > 100.0 =>
            {
                Some(rect.rect)
            }
            _ => None,
        })
        .expect("editor paints the note grid");
    let rect = note_rect(&editor.state, grid, &editor.notes[0]);
    let start = rect.center();
    let end = start + Vec2::new(216.0, 0.0);
    editor.frame(vec![egui::Event::PointerMoved(start)], true);
    editor.pointer(start, PointerButton::Primary, true, true);
    assert!(
        editor
            .frame(vec![egui::Event::PointerMoved(end)], true)
            .0
            .is_empty()
    );
    let actions = editor.pointer(end, PointerButton::Primary, false, true);
    let [Intent::EditNotes { notes, .. }] = actions.as_slice() else {
        panic!("expected one moved note: {actions:?}");
    };
    assert_eq!(notes[0].start, 4.0);
    assert!(editor.state.pending_selection.is_some());
    // Canonical event sorting puts the untouched note first and renumbers the moved note.
    editor.notes = vec![
        note(0, 3.0),
        Note {
            event_index: 2,
            start: notes[0].start,
            length: notes[0].length,
            pitch: notes[0].pitch,
            velocity: f32::from(notes[0].velocity) / 127.0,
            cents: 0.0,
        },
    ];
    editor.frame(vec![], true);
    assert_eq!(editor.state.selected, BTreeSet::from([2]));
    assert!(editor.state.pending_selection.is_none());
}

#[test]
fn duplicate_shortcut_preserves_fractional_tuning() {
    let cents = 1200.0 / 7.0 - 200.0;
    let mut editor = Editor::new(vec![Note {
        cents,
        ..note(0, 1.0)
    }]);
    editor.frame(vec![], true);
    editor.state.selected.insert(0);
    let actions = editor
        .frame(
            vec![egui::Event::Key {
                key: egui::Key::D,
                physical_key: Some(egui::Key::D),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }],
            true,
        )
        .0;
    let [Intent::AddNotes { notes, .. }] = actions.as_slice() else {
        panic!("expected duplicated note: {actions:?}");
    };
    assert_eq!(notes[0].cents, cents);
}
