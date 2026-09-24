//! Computer-keyboard performance state. Notes are committed as one undoable take.
use super::{DIM, EVENT_TONE, GawApp, RichText, STATUS_ERROR, TEXT, egui};
use crate::model::{MidiRecordingTarget, RecordedMidiNote};
use crate::physical_keyboard::{self, PhysicalPianoKey as PianoKey};
use std::collections::HashMap;

mod layout;
use layout::Layout;

const KEYS: [(egui::Key, &str); 17] = [
    (egui::Key::A, "A"),
    (egui::Key::W, "W"),
    (egui::Key::S, "S"),
    (egui::Key::E, "E"),
    (egui::Key::D, "D"),
    (egui::Key::F, "F"),
    (egui::Key::T, "T"),
    (egui::Key::G, "G"),
    (egui::Key::Y, "Y"),
    (egui::Key::H, "H"),
    (egui::Key::U, "U"),
    (egui::Key::J, "J"),
    (egui::Key::K, "K"),
    (egui::Key::O, "O"),
    (egui::Key::L, "L"),
    (egui::Key::P, "P"),
    (egui::Key::Semicolon, ";"),
];

#[derive(Debug)]
struct HeldNote {
    pitch: u8,
    cents: f64,
    velocity: u8,
    start: Option<f64>,
}

#[derive(Debug)]
struct Take {
    target: MidiRecordingTarget,
    notes: Vec<RecordedMidiNote>,
    started: f64,
    start_beat: f64,
    bpm: f64,
    with_transport: bool,
    last_playhead: f32,
}

impl Take {
    fn beat(&self, now: f64) -> f64 {
        self.start_beat + (now - self.started).max(0.0) * self.bpm / 60.0
    }
}

#[derive(Debug)]
pub(super) struct KeyboardPiano {
    pub open: bool,
    root: u8,
    layout: Layout,
    velocity: u8,
    track: Option<gaw_core::TrackId>,
    held: HashMap<PianoKey, HeldNote>,
    take: Option<Take>,
    unsaved: Option<Take>,
    message: Option<String>,
}

impl Default for KeyboardPiano {
    fn default() -> Self {
        Self {
            open: false,
            root: 60,
            layout: Layout::Piano,
            velocity: 100,
            track: None,
            held: HashMap::new(),
            take: None,
            unsaved: None,
            message: None,
        }
    }
}

fn piano_offset(key: egui::Key) -> Option<u8> {
    KEYS.iter()
        .position(|(candidate, _)| *candidate == key)
        .map(|index| index as u8)
}

impl KeyboardPiano {
    fn release(&mut self, key: PianoKey, now: f64) -> Option<u8> {
        let held = self.held.remove(&key)?;
        if let (Some(start), Some(take)) = (held.start, &mut self.take) {
            take.notes.push(RecordedMidiNote {
                start,
                duration: (take.beat(now) - start).max(1.0 / 960.0),
                pitch: held.pitch,
                cents: held.cents,
                velocity: held.velocity,
            });
        }
        Some(held.pitch)
    }
}

impl GawApp {
    pub(super) fn toggle_keyboard_piano(&mut self, now: f64) {
        if self.keyboard_piano.open {
            self.finish_keyboard_take(now);
        }
        self.keyboard_piano.open = !self.keyboard_piano.open;
    }

    fn release_piano_keys(&mut self, now: f64) {
        let mut keys: Vec<_> = self.keyboard_piano.held.keys().copied().collect();
        keys.sort_unstable_by_key(|key| self.keyboard_piano.held[key].pitch);
        for key in keys {
            self.keyboard_piano.release(key, now);
        }
        if let Some(controller) = &mut self.controller {
            controller.keyboard_all_notes_off();
        }
    }

    pub(super) fn finish_keyboard_take(&mut self, now: f64) {
        self.release_piano_keys(now);
        self.vm.transport.recording = false;
        if let Some(take) = self.keyboard_piano.take.take() {
            self.save_keyboard_take(take);
        }
    }

    fn save_keyboard_take(&mut self, take: Take) {
        match self.vm.record_keyboard_take(&take.target, &take.notes) {
            Ok(()) => {
                self.keyboard_piano.message = None;
            }
            Err(error) => {
                self.keyboard_piano.message = Some(format!("Take not saved: {error}"));
                self.keyboard_piano.unsaved = Some(take);
            }
        }
    }

    pub(super) fn toggle_keyboard_recording(&mut self, now: f64) {
        self.keyboard_piano.open = true;
        if self.keyboard_piano.take.is_some() {
            self.finish_keyboard_take(now);
            return;
        }
        if self.keyboard_piano.unsaved.is_some() {
            self.keyboard_piano.message = Some("Save or discard the previous take first.".into());
            return;
        }
        let Some(target) = self.vm.keyboard_recording_target() else {
            self.keyboard_piano.message = Some("Select a MIDI clip to record.".into());
            return;
        };
        let playhead = f64::from(self.vm.transport.playhead);
        if self.vm.transport.playing && playhead < target.clip_start {
            self.keyboard_piano.message = Some("Move the cursor into the selected clip.".into());
            return;
        }
        self.release_piano_keys(now);
        self.keyboard_piano.track = self.vm.keyboard_track_id();
        self.keyboard_piano.message = None;
        self.keyboard_piano.take = Some(Take {
            start_beat: (playhead - target.clip_start).max(0.0),
            target,
            notes: Vec::new(),
            started: now,
            bpm: f64::from(self.vm.transport.bpm),
            with_transport: self.vm.transport.playing,
            last_playhead: self.vm.transport.playhead,
        });
        self.vm.transport.recording = true;
    }

    pub(super) fn handle_piano_keyboard(&mut self, context: &egui::Context, now: f64) {
        if !context.text_edit_focused()
            && self.asset_dialog.is_none()
            && self.audio_settings.is_none()
            && self.sampler_editor.is_none()
            && context.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::K))
        {
            self.toggle_keyboard_piano(now);
        }
        let track = self
            .keyboard_piano
            .open
            .then(|| self.vm.keyboard_track_id())
            .flatten();
        let focused = context.input(|input| input.focused);
        let blocked = context.text_edit_focused()
            || self.asset_dialog.is_some()
            || self.audio_settings.is_some()
            || self.sampler_editor.is_some()
            || self.pending_asset_drop.is_some();
        let interrupted = self.keyboard_piano.take.as_ref().is_some_and(|take| {
            !self.vm.transport.recording
                || self.vm.keyboard_recording_target().as_ref() != Some(&take.target)
                || (f64::from(self.vm.transport.bpm) - take.bpm).abs() > 0.001
                || (take.with_transport
                    && (!self.vm.transport.playing
                        || self.vm.transport.playhead + 0.05 < take.last_playhead
                        || (f64::from(self.vm.transport.playhead)
                            - take.target.clip_start
                            - take.beat(now))
                        .abs()
                            > 0.75))
                || (!take.with_transport && self.vm.transport.playing)
        });
        if track != self.keyboard_piano.track
            || ((!focused || blocked || interrupted)
                && (!self.keyboard_piano.held.is_empty() || self.keyboard_piano.take.is_some()))
        {
            self.finish_keyboard_take(now);
        }
        self.keyboard_piano.track = track;
        if let Some(controller) = &mut self.controller {
            controller.configure_keyboard_instrument(&self.vm, track);
        }
        if let Some(take) = &mut self.keyboard_piano.take {
            take.last_playhead = self.vm.transport.playhead;
        }
        let active = self.keyboard_piano.open && focused && !blocked && track.is_some();
        physical_keyboard::set_capture(
            context,
            active && self.keyboard_piano.layout != Layout::Piano,
        );
        let physical_events = physical_keyboard::take_events(context);
        if !active {
            return;
        }
        context.request_repaint_after(std::time::Duration::from_millis(16));
        let mut events = context.input_mut(|input| {
            let mut piano = Vec::new();
            input.events.retain(|event| {
                let egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    repeat,
                    modifiers,
                } = event
                else {
                    return true;
                };
                let key = physical_key.unwrap_or(*key);
                let physical = PianoKey::Key(key);
                let release = !pressed && self.keyboard_piano.held.contains_key(&physical);
                let mapped = self
                    .keyboard_piano
                    .layout
                    .pitch(physical, self.keyboard_piano.root)
                    .is_some()
                    || self.keyboard_piano.layout.octave_key(key).is_some()
                    || key == egui::Key::Escape;
                if release || (mapped && modifiers.is_none()) {
                    piano.push((physical, *pressed, *repeat));
                    false
                } else {
                    true
                }
            });
            piano
        });
        events.extend(
            physical_events
                .into_iter()
                .map(|event| (event.key, event.pressed, event.repeat)),
        );
        for (key, pressed, repeat) in events {
            self.handle_piano_key(key, pressed, repeat, now);
        }
    }

    fn change_piano_octave(&mut self, up: bool, now: f64) {
        self.release_piano_keys(now);
        self.keyboard_piano.root = if up {
            (self.keyboard_piano.root + 12).min(self.keyboard_piano.layout.max_root())
        } else {
            self.keyboard_piano.root.saturating_sub(12)
        };
    }

    fn set_piano_layout(&mut self, layout: Layout, now: f64) {
        if self.keyboard_piano.layout == layout {
            return;
        }
        self.release_piano_keys(now);
        self.keyboard_piano.layout = layout;
        self.keyboard_piano.root = self.keyboard_piano.root.min(layout.max_root());
    }

    fn handle_piano_key(&mut self, key: PianoKey, pressed: bool, repeat: bool, now: f64) {
        if !pressed {
            if let Some(note) = self.keyboard_piano.release(key, now)
                && let Some(controller) = &mut self.controller
            {
                controller.keyboard_note_off(note);
            }
            return;
        }
        if repeat || self.keyboard_piano.held.contains_key(&key) {
            return;
        }
        if let PianoKey::Key(key) = key {
            if key == egui::Key::Escape {
                self.finish_keyboard_take(now);
                return;
            }
            if let Some(up) = self.keyboard_piano.layout.octave_key(key) {
                self.change_piano_octave(up, now);
                return;
            }
        }
        let Some(pitch) = self
            .keyboard_piano
            .layout
            .pitch(key, self.keyboard_piano.root)
        else {
            return;
        };
        let velocity = self.keyboard_piano.velocity;
        if let Some(controller) = &mut self.controller
            && !controller.keyboard_note_on_tuned(pitch.note, velocity, pitch.cents)
        {
            if self.keyboard_piano.take.is_none() {
                return;
            }
            self.keyboard_piano.message = Some("Recording MIDI only · audio unavailable".into());
        }
        let start = self.keyboard_piano.take.as_ref().map(|take| take.beat(now));
        self.keyboard_piano.held.insert(
            key,
            HeldNote {
                pitch: pitch.note,
                cents: pitch.cents,
                velocity,
                start,
            },
        );
    }

    pub(super) fn keyboard_piano_window(&mut self, context: &egui::Context, now: f64) {
        if !self.keyboard_piano.open {
            physical_keyboard::set_capture(context, false);
            return;
        }
        let mut open = true;
        egui::Window::new("Keyboard")
            .id(egui::Id::new("keyboard-piano"))
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(468.0)
            .show(context, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 10.0);
                ui.horizontal(|ui| {
                    let mut layout = self.keyboard_piano.layout;
                    egui::ComboBox::from_id_salt("keyboard-layout")
                        .selected_text(layout.label()).width(76.0).show_ui(ui, |ui| {
                            for choice in Layout::ALL { ui.selectable_value(&mut layout, choice, choice.label()); }
                        });
                    self.set_piano_layout(layout, now);
                    if ui.small_button("−").on_hover_text("Octave down · Page Down").clicked() {
                        self.change_piano_octave(false, now);
                    }
                    ui.label(format!("C{}", i16::from(self.keyboard_piano.root) / 12 - 1))
                        .on_hover_text("Lowest octave");
                    if ui.small_button("+").on_hover_text("Octave up · Page Up").clicked() {
                        self.change_piano_octave(true, now);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().slider_width = 100.0;
                        ui.add(egui::Slider::new(&mut self.keyboard_piano.velocity, 1..=127).text("Velocity"));
                    });
                });
                if self.keyboard_piano.layout == Layout::Piano { self.paint_piano_keys(ui); }
                else { self.paint_grid_keys(ui); }
                ui.horizontal(|ui| {
                    let recording = self.keyboard_piano.take.is_some();
                    if ui.selectable_label(recording, if recording { "■ Finish" } else { "● Record" })
                        .on_hover_text("Record MIDI into the selected clip. Finish to save; one Undo removes the take.")
                        .clicked() {
                        self.toggle_keyboard_recording(now);
                    }
                    if ui.small_button("■").on_hover_text("All notes off · Esc").clicked() {
                        self.finish_keyboard_take(now);
                    }
                    if let Some(take) = &self.keyboard_piano.take {
                        ui.colored_label(STATUS_ERROR, format!("{:.1}s", now - take.started));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new("?").color(DIM)).on_hover_text(
                            "Page Up / Down: octave · Esc: all notes off · Cmd/Ctrl+K: close\nRecord from the cursor at project tempo, with playback running or stopped.\nLive sound bypasses clip, track, and master effects."
                        );
                        let track = self.vm.keyboard_track_id().and_then(|id| self.vm.project().tracks.iter().find(|track| track.id == id));
                        if let Some(track) = track {
                            let empty = track.instrument.as_ref().is_some_and(|instrument| {
                                let gaw_core::InstrumentKind::Sampler(sampler) = &instrument.kind;
                                sampler.zones.is_empty()
                            });
                            if empty {
                                if ui.small_button("Choose sample").clicked() {
                                    let track_index = self.vm.current_composition().tracks.iter().position(|candidate| candidate.id == track.id.to_string());
                                    if let Some(track_index) = track_index { self.open_sampler(track_index, now); }
                                }
                            } else if let Some(controller) = &self.controller {
                                let status = controller.keyboard_instrument_status();
                                if let Some(error) = status.error {
                                    ui.colored_label(STATUS_ERROR, "Audio unavailable").on_hover_text(error);
                                } else if status.loading {
                                    ui.spinner().on_hover_text("Loading instrument");
                                }
                            }
                        } else {
                            ui.label(RichText::new("Select a MIDI clip").color(DIM));
                        }
                    });
                });
                if self.keyboard_piano.unsaved.is_some() {
                    ui.horizontal(|ui| {
                        ui.colored_label(STATUS_ERROR, "Take not saved")
                            .on_hover_text(self.keyboard_piano.message.as_deref().unwrap_or_default());
                        if ui.small_button("Retry").clicked()
                            && let Some(take) = self.keyboard_piano.unsaved.take() {
                            self.save_keyboard_take(take);
                        }
                        if ui.small_button("Discard").clicked() {
                            self.keyboard_piano.unsaved = None;
                            self.keyboard_piano.message = None;
                        }
                    });
                } else if let Some(message) = &self.keyboard_piano.message {
                    ui.label(RichText::new(message).color(DIM));
                }
            });
        if !open {
            self.finish_keyboard_take(now);
            self.keyboard_piano.open = false;
        }
        physical_keyboard::set_capture(
            context,
            self.keyboard_piano.open
                && self.keyboard_piano.layout != Layout::Piano
                && self.vm.keyboard_track_id().is_some()
                && context.input(|input| input.focused)
                && !context.text_edit_focused()
                && self.asset_dialog.is_none()
                && self.audio_settings.is_none()
                && self.sampler_editor.is_none()
                && self.pending_asset_drop.is_none(),
        );
    }

    fn paint_grid_keys(&self, ui: &mut egui::Ui) {
        let columns = self.keyboard_piano.layout.columns();
        let (rect, _) = ui.allocate_exact_size(egui::vec2(468.0, 156.0), egui::Sense::hover());
        let width = rect.width() / columns as f32;
        let height = rect.height() / 4.0;
        for (row, keys) in layout::ROWS.iter().enumerate() {
            for (column, &(key, label)) in keys[..columns].iter().enumerate() {
                let key_rect = egui::Rect::from_min_size(
                    rect.min + egui::vec2(column as f32 * width, row as f32 * height),
                    egui::vec2(width - 3.0, height - 3.0),
                );
                let held = self.keyboard_piano.held.contains_key(&key);
                ui.painter().rect_filled(
                    key_rect,
                    2.0,
                    if held {
                        EVENT_TONE
                    } else {
                        super::PANEL_RAISED
                    },
                );
                ui.painter().text(
                    key_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    label,
                    egui::FontId::monospace(10.0),
                    TEXT,
                );
                if let Some(pitch) = self
                    .keyboard_piano
                    .layout
                    .pitch(key, self.keyboard_piano.root)
                {
                    const NAMES: [&str; 12] = [
                        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
                    ];
                    let note = format!(
                        "{}{}",
                        NAMES[usize::from(pitch.note % 12)],
                        i16::from(pitch.note / 12) - 1
                    );
                    let tooltip = if pitch.cents.abs() < 0.001 {
                        note
                    } else {
                        format!("{note} {:+.2} cents", pitch.cents)
                    };
                    ui.interact(
                        key_rect,
                        ui.id().with(("grid-key", row, column)),
                        egui::Sense::hover(),
                    )
                    .on_hover_text(tooltip);
                }
            }
        }
    }

    fn paint_piano_keys(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(468.0, 92.0), egui::Sense::hover());
        let whites = [0, 2, 4, 5, 7, 9, 11, 12, 14, 16];
        let width = rect.width() / whites.len() as f32;
        for black in [false, true] {
            for (offset, (key, label)) in KEYS.iter().enumerate() {
                let is_black = !whites.contains(&offset);
                if is_black != black {
                    continue;
                }
                let preceding = whites.iter().filter(|&&white| white < offset).count();
                let (x, w, h) = if black {
                    (preceding as f32 * width - width * 0.3, width * 0.6, 58.0)
                } else {
                    (preceding as f32 * width, width - 2.0, 90.0)
                };
                let key_rect =
                    egui::Rect::from_min_size(rect.min + egui::vec2(x, 0.0), egui::vec2(w, h));
                let held = self.keyboard_piano.held.contains_key(&PianoKey::Key(*key));
                let color = if held {
                    EVENT_TONE
                } else if black {
                    egui::Color32::from_gray(26)
                } else {
                    egui::Color32::from_gray(215)
                };
                ui.painter().rect_filled(key_rect, 3.0, color);
                ui.painter().text(
                    key_rect.center_bottom() - egui::vec2(0.0, 12.0),
                    egui::Align2::CENTER_CENTER,
                    label,
                    egui::FontId::monospace(13.0),
                    if black { TEXT } else { egui::Color32::BLACK },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (egui::Context, GawApp) {
        let context = egui::Context::default();
        let mut app = GawApp::with_project_runtime(
            &context,
            crate::model::demo_project(),
            crate::settings::AudioPreferences::default(),
        )
        .unwrap();
        app.vm
            .apply(crate::model::Intent::CreateMidiTrack { beat: 0.0 });
        (context, app)
    }

    fn key(key: egui::Key, pressed: bool, repeat: bool, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat,
            modifiers,
        }
    }

    fn frame(
        context: &egui::Context,
        app: &mut GawApp,
        now: f64,
        focused: bool,
        events: Vec<egui::Event>,
    ) {
        let _ = context.run_ui(
            egui::RawInput {
                time: Some(now),
                focused,
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1200.0, 800.0),
                )),
                ..Default::default()
            },
            |_ui| {
                app.handle_piano_keyboard(context, now);
                app.handle_keyboard(context, now);
                app.keyboard_piano_window(context, now);
            },
        );
    }

    #[test]
    fn record_from_closed_panel_captures_chord_timing_and_ignores_repeat() {
        let (context, mut app) = fixture();
        let target = app.vm.keyboard_recording_target().unwrap();
        let bpm = f64::from(app.vm.transport.bpm);
        app.toggle_keyboard_recording(1.0);
        frame(
            &context,
            &mut app,
            1.1,
            true,
            vec![
                key(egui::Key::A, true, false, egui::Modifiers::NONE),
                key(egui::Key::D, true, false, egui::Modifiers::NONE),
            ],
        );
        assert!(app.vm.transport.recording);
        assert_eq!(app.keyboard_piano.held.len(), 2);
        frame(
            &context,
            &mut app,
            1.2,
            true,
            vec![key(egui::Key::A, true, true, egui::Modifiers::NONE)],
        );
        // Releases still arrive after a modifier is held.
        frame(
            &context,
            &mut app,
            1.6,
            true,
            vec![key(egui::Key::A, false, false, egui::Modifiers::CTRL)],
        );
        app.finish_keyboard_take(2.1);
        let data = app
            .vm
            .project()
            .event_data
            .iter()
            .find(|data| data.id == target.event_data_id)
            .unwrap();
        assert_eq!(data.events.len(), 2);
        let gaw_core::Event::Note(first) = &data.events[0] else {
            panic!("expected note");
        };
        assert!((first.start.value() - 0.1 * bpm / 60.0).abs() < 1e-6);
        assert!((first.duration.value() - 0.5 * bpm / 60.0).abs() < 1e-6);
        assert_eq!(first.note.value(), 60);
        app.vm.apply(crate::model::Intent::Undo(2.2));
        assert!(
            app.vm
                .project()
                .event_data
                .iter()
                .find(|data| data.id == target.event_data_id)
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[test]
    fn focus_loss_finishes_take_and_releases_keys() {
        let (context, mut app) = fixture();
        app.toggle_keyboard_recording(1.0);
        frame(
            &context,
            &mut app,
            1.1,
            true,
            vec![key(egui::Key::A, true, false, egui::Modifiers::NONE)],
        );
        frame(&context, &mut app, 1.5, false, vec![]);
        assert!(app.keyboard_piano.held.is_empty());
        assert!(app.keyboard_piano.take.is_none());
        assert!(!app.vm.transport.recording);
        let target = app.vm.keyboard_recording_target().unwrap();
        assert_eq!(
            app.vm
                .project()
                .event_data
                .iter()
                .find(|data| data.id == target.event_data_id)
                .unwrap()
                .events
                .len(),
            1
        );
    }

    #[test]
    fn even_a_small_timeline_seek_finishes_the_take_before_moving() {
        let (context, mut app) = fixture();
        app.toggle_keyboard_recording(1.0);
        frame(
            &context,
            &mut app,
            1.1,
            true,
            vec![key(egui::Key::A, true, false, egui::Modifiers::NONE)],
        );
        app.handle_timeline_action(&context, crate::model::Intent::Seek(0.25));
        assert!(app.keyboard_piano.take.is_none());
        assert!(app.keyboard_piano.held.is_empty());
        assert!(!app.vm.transport.recording);
        assert!((app.vm.transport.playhead - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn text_focus_does_not_play_or_record_typing() {
        let (context, mut app) = fixture();
        app.toggle_keyboard_recording(1.0);
        let mut text = String::new();
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            ui.text_edit_singleline(&mut text).request_focus();
        });
        frame(
            &context,
            &mut app,
            1.1,
            true,
            vec![key(egui::Key::A, true, false, egui::Modifiers::NONE)],
        );
        assert!(app.keyboard_piano.take.is_none());
        assert!(app.keyboard_piano.held.is_empty());
    }

    #[test]
    fn piano_keys_take_priority_over_editor_shortcuts_only_while_enabled() {
        let (context, mut app) = fixture();
        let original_lens = app.vm.structure_lens;
        app.keyboard_piano.open = true;
        frame(
            &context,
            &mut app,
            1.0,
            true,
            vec![key(egui::Key::L, true, false, egui::Modifiers::NONE)],
        );
        assert_eq!(app.vm.structure_lens, original_lens);
        assert_eq!(app.keyboard_piano.held.len(), 1);
        app.keyboard_piano.open = false;
        frame(
            &context,
            &mut app,
            1.1,
            true,
            vec![key(egui::Key::L, false, false, egui::Modifiers::NONE)],
        );
        frame(
            &context,
            &mut app,
            1.2,
            true,
            vec![key(egui::Key::L, true, false, egui::Modifiers::NONE)],
        );
        assert_ne!(app.vm.structure_lens, original_lens);
        assert!(app.keyboard_piano.held.is_empty());
    }

    #[test]
    fn command_k_toggles_centered_piano_without_playing_a_note() {
        let (context, mut app) = fixture();
        frame(
            &context,
            &mut app,
            1.0,
            true,
            vec![key(egui::Key::K, true, false, egui::Modifiers::COMMAND)],
        );
        assert!(app.keyboard_piano.open);
        assert!(app.keyboard_piano.held.is_empty());
        frame(
            &context,
            &mut app,
            1.1,
            true,
            vec![key(egui::Key::K, false, false, egui::Modifiers::COMMAND)],
        );
        let rect = context
            .memory(|memory| memory.area_rect(egui::Id::new("keyboard-piano")))
            .unwrap();
        assert!(
            (rect.center() - egui::pos2(600.0, 400.0)).length() <= 2.0,
            "{rect:?}"
        );
        assert!(rect.height() < 250.0, "compact piano: {rect:?}");
        frame(
            &context,
            &mut app,
            1.2,
            true,
            vec![key(egui::Key::K, true, false, egui::Modifiers::COMMAND)],
        );
        assert!(!app.keyboard_piano.open);
        assert!(app.keyboard_piano.held.is_empty());
    }

    #[test]
    fn grid_recording_keeps_seven_edo_chords_and_layout_switch_releases_notes() {
        let (_context, mut app) = fixture();
        let target = app.vm.keyboard_recording_target().unwrap();
        app.set_piano_layout(Layout::SevenEdo, 0.0);
        app.toggle_keyboard_recording(1.0);
        app.handle_piano_key(PianoKey::ShiftLeft, true, false, 1.1);
        app.handle_piano_key(PianoKey::Key(egui::Key::Z), true, false, 1.1);
        assert_eq!(app.keyboard_piano.held.len(), 2);
        assert_eq!(
            app.keyboard_piano.root, 60,
            "Z is a grid note, not octave down"
        );
        app.set_piano_layout(Layout::Chromatic, 1.6);
        assert!(app.keyboard_piano.held.is_empty());
        app.handle_piano_key(PianoKey::ShiftRight, true, false, 1.7);
        app.finish_keyboard_take(2.0);
        let data = app
            .vm
            .project()
            .event_data
            .iter()
            .find(|data| data.id == target.event_data_id)
            .unwrap();
        assert_eq!(data.events.len(), 3);
        let pitches: Vec<_> = data
            .events
            .iter()
            .map(|event| {
                let gaw_core::Event::Note(note) = event else {
                    panic!("expected note");
                };
                f64::from(note.note.value())
                    + note.tuning.map_or(0.0, gaw_core::Cents::value) / 100.0
            })
            .collect();
        assert!((pitches[0] - 60.0).abs() < 1e-10);
        assert!((pitches[1] - (60.0 + 12.0 / 7.0)).abs() < 1e-10);
        assert!((pitches[2] - 71.0).abs() < 1e-10);
    }

    #[test]
    fn grid_windows_remain_compact_and_centered() {
        let (context, mut app) = fixture();
        app.keyboard_piano.open = true;
        for (index, layout) in [Layout::Chromatic, Layout::SevenEdo]
            .into_iter()
            .enumerate()
        {
            app.set_piano_layout(layout, index as f64);
            for pass in 0..2 {
                frame(
                    &context,
                    &mut app,
                    index as f64 + f64::from(pass) * 0.1,
                    true,
                    vec![],
                );
            }
            let rect = context
                .memory(|memory| memory.area_rect(egui::Id::new("keyboard-piano")))
                .unwrap();
            assert!(
                (rect.center() - egui::pos2(600.0, 400.0)).length() <= 2.0,
                "{rect:?}"
            );
            assert!(rect.width() < 530.0 && rect.height() < 300.0, "{rect:?}");
        }
    }

    #[test]
    fn keyboard_maps_chromatic_notes_and_reserves_octave_keys() {
        assert_eq!(piano_offset(egui::Key::A), Some(0));
        assert_eq!(piano_offset(egui::Key::W), Some(1));
        assert_eq!(piano_offset(egui::Key::K), Some(12));
        assert_eq!(piano_offset(egui::Key::P), Some(15));
        assert_eq!(piano_offset(egui::Key::Z), None);
        for root in (0..=108).step_by(12) {
            assert!(root + 15 <= 127);
        }
    }

    #[test]
    fn release_uses_note_at_press_time_and_is_idempotent() {
        let mut piano = KeyboardPiano::default();
        piano.held.insert(
            PianoKey::Key(egui::Key::A),
            HeldNote {
                pitch: 60,
                cents: 0.0,
                velocity: 100,
                start: None,
            },
        );
        piano.root = 72;
        assert_eq!(piano.release(PianoKey::Key(egui::Key::A), 1.0), Some(60));
        assert_eq!(piano.release(PianoKey::Key(egui::Key::A), 2.0), None);
    }
}
