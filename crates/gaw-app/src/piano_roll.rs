//! Stateful MIDI piano-roll editing surface.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_arguments,
    clippy::too_many_lines
)]

use std::collections::BTreeSet;

use egui::{
    Align2, Color32, CornerRadius, FontId, PointerButton, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, Ui, Vec2,
};

use crate::model::{Clip, Intent, Note, NoteInsert, NoteUpdate};
use crate::theme::{
    BORDER, BORDER_STRONG, CANVAS, DIM, EVENT_TONE, HIGHLIGHT, PANEL, PANEL_ALT, PANEL_RAISED,
    PLAYHEAD, TEXT,
};

const KEY_WIDTH: f32 = 66.0;
const RULER_HEIGHT: f32 = 24.0;
const VELOCITY_HEIGHT: f32 = 68.0;
const MIN_GRID_HEIGHT: f32 = 96.0;
const MIN_PIXELS_PER_BEAT: f32 = 24.0;
const MAX_PIXELS_PER_BEAT: f32 = 480.0;
const MIN_ROW_HEIGHT: f32 = 9.0;
const MAX_ROW_HEIGHT: f32 = 32.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    Select,
    Draw,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum GridSize {
    Bar,
    Beat,
    Eighth,
    Sixteenth,
    ThirtySecond,
}

impl GridSize {
    const ALL: [Self; 5] = [
        Self::Bar,
        Self::Beat,
        Self::Eighth,
        Self::Sixteenth,
        Self::ThirtySecond,
    ];

    const fn beats(self, beats_per_bar: f32) -> f32 {
        match self {
            Self::Bar => beats_per_bar,
            Self::Beat => 1.0,
            Self::Eighth => 0.5,
            Self::Sixteenth => 0.25,
            Self::ThirtySecond => 0.125,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Bar => "1 BAR",
            Self::Beat => "1/4",
            Self::Eighth => "1/8",
            Self::Sixteenth => "1/16",
            Self::ThirtySecond => "1/32",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DragKind {
    Move,
    Resize,
}

#[derive(Clone, Copy, Debug)]
struct Marquee {
    anchor: Pos2,
    current: Pos2,
}

#[derive(Clone, Copy, Debug)]
struct VelocityDrag {
    event_index: usize,
    velocity: u8,
}

#[derive(Debug)]
pub(crate) struct PianoRollState {
    pub fullscreen: bool,
    active_clip: String,
    selected: BTreeSet<usize>,
    tool: Tool,
    grid: GridSize,
    pixels_per_beat: f32,
    row_height: f32,
    scroll_beat: f32,
    top_pitch: f32,
    velocity_lane: bool,
    velocity_drag: Option<VelocityDrag>,
    marquee: Option<Marquee>,
    fit_pitch_pending: bool,
}

impl Default for PianoRollState {
    fn default() -> Self {
        Self {
            fullscreen: false,
            active_clip: String::new(),
            selected: BTreeSet::new(),
            tool: Tool::Select,
            grid: GridSize::Sixteenth,
            pixels_per_beat: 72.0,
            row_height: 14.0,
            scroll_beat: 0.0,
            top_pitch: 84.0,
            velocity_lane: true,
            velocity_drag: None,
            marquee: None,
            fit_pitch_pending: true,
        }
    }
}

impl PianoRollState {
    pub(crate) fn begin_drawing(&mut self) {
        self.tool = Tool::Draw;
        self.selected.clear();
    }
}

impl PianoRollState {
    pub fn clear_focus(&mut self) {
        self.fullscreen = false;
        self.selected.clear();
        self.velocity_drag = None;
        self.marquee = None;
    }

    pub fn handle_escape(&mut self) -> bool {
        if self.fullscreen {
            self.fullscreen = false;
            true
        } else if self.selected.is_empty() {
            false
        } else {
            self.selected.clear();
            true
        }
    }

    fn prepare_clip(&mut self, clip: &Clip, notes: &[Note]) {
        if self.active_clip == clip.id {
            self.selected
                .retain(|index| notes.iter().any(|note| note.event_index == *index));
            return;
        }
        self.active_clip.clone_from(&clip.id);
        self.selected.clear();
        self.velocity_drag = None;
        self.scroll_beat = 0.0;
        self.fit_pitch_pending = true;
    }

    fn snap(&self, beat: f32, beats_per_bar: f32) -> f32 {
        let step = self.grid.beats(beats_per_bar);
        (beat / step).round() * step
    }

    fn selected_notes<'a>(&self, notes: &'a [Note]) -> impl Iterator<Item = &'a Note> {
        notes
            .iter()
            .filter(|note| self.selected.contains(&note.event_index))
    }
}

pub(crate) fn show(
    ui: &mut Ui,
    state: &mut PianoRollState,
    track_index: usize,
    clip_index: usize,
    clip: &Clip,
    notes: &[Note],
    playhead: f32,
    beats_per_bar: f32,
    new_note_velocity: &mut u8,
) -> Vec<Intent> {
    state.prepare_clip(clip, notes);
    let mut actions = Vec::new();

    toolbar(
        ui,
        state,
        track_index,
        clip_index,
        clip,
        notes,
        beats_per_bar,
        new_note_velocity,
        &mut actions,
    );

    let available = ui.available_size();
    let velocity_height = if state.velocity_lane && available.y >= 220.0 {
        VELOCITY_HEIGHT
    } else {
        0.0
    };
    let editor_height = (available.y - velocity_height).max(MIN_GRID_HEIGHT);
    let (editor_rect, editor_response) = ui.allocate_exact_size(
        Vec2::new(available.x, editor_height),
        Sense::click_and_drag(),
    );
    let ruler_rect = Rect::from_min_max(
        Pos2::new(editor_rect.left() + KEY_WIDTH, editor_rect.top()),
        Pos2::new(editor_rect.right(), editor_rect.top() + RULER_HEIGHT),
    );
    let keys_rect = Rect::from_min_max(
        Pos2::new(editor_rect.left(), ruler_rect.bottom()),
        Pos2::new(editor_rect.left() + KEY_WIDTH, editor_rect.bottom()),
    );
    let grid_rect = Rect::from_min_max(keys_rect.right_top(), editor_rect.right_bottom());
    fit_pitch_view(state, notes, grid_rect);

    handle_navigation(ui, state, editor_response.rect, grid_rect, clip.length);
    paint_grid(
        ui,
        state,
        clip,
        ruler_rect,
        keys_rect,
        grid_rect,
        beats_per_bar,
    );
    if editor_response.clicked()
        && let Some(pointer) = editor_response.interact_pointer_pos()
        && ruler_rect.contains(pointer)
    {
        actions.push(Intent::Seek(
            clip.start + x_to_beat(state, grid_rect, pointer.x).clamp(0.0, clip.length),
        ));
    }

    let note_under_pointer = notes_ui(
        ui,
        state,
        track_index,
        clip_index,
        clip,
        notes,
        grid_rect,
        beats_per_bar,
        &mut actions,
    );
    grid_interaction(
        ui,
        state,
        &editor_response,
        track_index,
        clip_index,
        clip,
        notes,
        grid_rect,
        beats_per_bar,
        *new_note_velocity,
        note_under_pointer,
        &mut actions,
    );
    paint_playhead(ui, state, clip, grid_rect, playhead);

    if velocity_height > 0.0 {
        let (velocity_rect, response) = ui.allocate_exact_size(
            Vec2::new(available.x, velocity_height),
            Sense::click_and_drag(),
        );
        velocity_lane(
            ui,
            state,
            track_index,
            clip_index,
            notes,
            velocity_rect,
            &response,
            &mut actions,
        );
    }

    keyboard_shortcuts(
        ui,
        state,
        track_index,
        clip_index,
        clip,
        notes,
        beats_per_bar,
        &mut actions,
    );
    actions
}

#[allow(clippy::too_many_arguments)]
fn toolbar(
    ui: &mut Ui,
    state: &mut PianoRollState,
    track: usize,
    clip_index: usize,
    clip: &Clip,
    notes: &[Note],
    beats_per_bar: f32,
    new_note_velocity: &mut u8,
    actions: &mut Vec<Intent>,
) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("MIDI EDITOR")
                .monospace()
                .size(10.0)
                .color(HIGHLIGHT),
        );
        ui.label(RichText::new(&clip.name).strong().color(TEXT));
        ui.label(
            RichText::new(format!("{} NOTES · {:.1} BEATS", notes.len(), clip.length))
                .monospace()
                .size(8.5)
                .color(DIM),
        );
        ui.add_space(8.0);
        ui.selectable_value(&mut state.tool, Tool::Select, "SELECT")
            .on_hover_text("Select and move notes (V)");
        ui.selectable_value(&mut state.tool, Tool::Draw, "DRAW")
            .on_hover_text("Click to draw notes (B)");
        egui::ComboBox::from_id_salt("midi_grid")
            .selected_text(format!("GRID {}", state.grid.label()))
            .width(84.0)
            .show_ui(ui, |ui| {
                for grid in GridSize::ALL {
                    ui.selectable_value(&mut state.grid, grid, grid.label());
                }
            });

        if ui.small_button("QUANTIZE").clicked() {
            quantize_selected(
                state,
                track,
                clip_index,
                clip.length,
                notes,
                beats_per_bar,
                actions,
            );
        }
        if ui
            .small_button("−1")
            .on_hover_text("Transpose down")
            .clicked()
        {
            transpose_selected(state, track, clip_index, notes, -1, actions);
        }
        if ui
            .small_button("+1")
            .on_hover_text("Transpose up")
            .clicked()
        {
            transpose_selected(state, track, clip_index, notes, 1, actions);
        }
        if ui
            .small_button("ZOOM −")
            .on_hover_text("Zoom out horizontally (Ctrl + wheel)")
            .clicked()
        {
            state.pixels_per_beat =
                (state.pixels_per_beat / 1.25).clamp(MIN_PIXELS_PER_BEAT, MAX_PIXELS_PER_BEAT);
        }
        if ui
            .small_button("+")
            .on_hover_text("Zoom in horizontally (Ctrl + wheel)")
            .clicked()
        {
            state.pixels_per_beat =
                (state.pixels_per_beat * 1.25).clamp(MIN_PIXELS_PER_BEAT, MAX_PIXELS_PER_BEAT);
        }
        if ui
            .small_button(if state.velocity_lane {
                "VELOCITY ✓"
            } else {
                "VELOCITY"
            })
            .clicked()
        {
            state.velocity_lane = !state.velocity_lane;
        }

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(if state.fullscreen {
                    "RESTORE"
                } else {
                    "EXPAND"
                })
                .on_hover_text(if state.fullscreen {
                    "Return to arrangement (Esc)"
                } else {
                    "Focus the MIDI editor"
                })
                .clicked()
            {
                state.fullscreen = !state.fullscreen;
            }
            ui.add(
                egui::DragValue::new(new_note_velocity)
                    .range(1..=127)
                    .prefix("VEL "),
            );
        });
    });
    ui.separator();
}

fn handle_navigation(
    ui: &mut Ui,
    state: &mut PianoRollState,
    editor_rect: Rect,
    grid_rect: Rect,
    clip_length: f32,
) {
    if !ui.rect_contains_pointer(editor_rect) {
        clamp_view(state, grid_rect, clip_length);
        return;
    }
    let (scroll, modifiers, zoom_delta, hover) = ui.input(|input| {
        (
            input.smooth_scroll_delta,
            input.modifiers,
            input.zoom_delta(),
            input.pointer.hover_pos(),
        )
    });
    if (modifiers.command || modifiers.ctrl) && (zoom_delta - 1.0).abs() > f32::EPSILON {
        let pointer_x = hover.map_or(grid_rect.center().x, |point| point.x);
        let anchor =
            state.scroll_beat + ((pointer_x - grid_rect.left()).max(0.0) / state.pixels_per_beat);
        state.pixels_per_beat =
            (state.pixels_per_beat * zoom_delta).clamp(MIN_PIXELS_PER_BEAT, MAX_PIXELS_PER_BEAT);
        state.scroll_beat = anchor - (pointer_x - grid_rect.left()) / state.pixels_per_beat;
    } else if modifiers.alt && scroll.y.abs() > f32::EPSILON {
        state.row_height =
            (state.row_height * (1.0 + scroll.y * 0.003)).clamp(MIN_ROW_HEIGHT, MAX_ROW_HEIGHT);
    } else if modifiers.shift || scroll.x.abs() > scroll.y.abs() {
        state.scroll_beat -= (scroll.x + scroll.y) / state.pixels_per_beat;
    } else if scroll.y.abs() > f32::EPSILON {
        state.top_pitch += scroll.y / state.row_height;
    }
    clamp_view(state, grid_rect, clip_length);
}

fn clamp_view(state: &mut PianoRollState, grid_rect: Rect, clip_length: f32) {
    let visible_beats = grid_rect.width().max(1.0) / state.pixels_per_beat;
    state.scroll_beat = state
        .scroll_beat
        .clamp(0.0, (clip_length - visible_beats).max(0.0));
    let visible_rows = grid_rect.height().max(1.0) / state.row_height;
    let minimum_top_pitch = (visible_rows - 1.0).clamp(0.0, 127.0);
    state.top_pitch = state.top_pitch.clamp(minimum_top_pitch, 127.0);
}

fn fit_pitch_view(state: &mut PianoRollState, notes: &[Note], grid: Rect) {
    if !state.fit_pitch_pending {
        return;
    }
    let mut pitches = notes.iter().map(|note| note.pitch).collect::<Vec<_>>();
    pitches.sort_unstable();
    let center = pitches
        .get(pitches.len() / 2)
        .map_or(60.0, |pitch| f32::from(*pitch));
    let visible_rows = grid.height().max(1.0) / state.row_height;
    let minimum_top_pitch = (visible_rows - 1.0).clamp(0.0, 127.0);
    state.top_pitch = (center + visible_rows * 0.5).clamp(minimum_top_pitch, 127.0);
    state.fit_pitch_pending = false;
}

fn paint_grid(
    ui: &Ui,
    state: &PianoRollState,
    clip: &Clip,
    ruler: Rect,
    keys: Rect,
    grid: Rect,
    beats_per_bar: f32,
) {
    let painter = ui.painter();
    painter.rect_filled(ruler, CornerRadius::ZERO, PANEL_ALT);
    painter.rect_filled(keys, CornerRadius::ZERO, PANEL_RAISED);
    painter.rect_filled(grid, CornerRadius::ZERO, CANVAS);

    let first_pitch = state.top_pitch.ceil() as i16;
    let visible_rows = (grid.height() / state.row_height).ceil() as i16 + 1;
    for row in 0..visible_rows {
        let pitch = first_pitch - row;
        if !(0..=127).contains(&pitch) {
            continue;
        }
        let y = grid.top() + (state.top_pitch - f32::from(pitch)) * state.row_height;
        let row_rect = Rect::from_min_size(
            Pos2::new(grid.left(), y),
            Vec2::new(grid.width(), state.row_height),
        );
        if is_black_key(pitch as u8) {
            painter.rect_filled(row_rect.intersect(grid), CornerRadius::ZERO, PANEL);
            painter.rect_filled(
                Rect::from_min_size(
                    Pos2::new(keys.left(), y),
                    Vec2::new(KEY_WIDTH * 0.64, state.row_height),
                )
                .intersect(keys),
                CornerRadius::ZERO,
                CANVAS,
            );
        }
        painter.hline(
            row_rect.x_range(),
            y,
            Stroke::new(if pitch % 12 == 0 { 1.0 } else { 0.5 }, BORDER),
        );
        if pitch % 12 == 0 {
            painter.text(
                Pos2::new(keys.right() - 7.0, y + state.row_height * 0.5),
                Align2::RIGHT_CENTER,
                note_name(pitch as u8),
                FontId::monospace(9.0),
                TEXT,
            );
        }
    }

    let minor = state.grid.beats(beats_per_bar);
    let first_line = (state.scroll_beat / minor).floor() as i32;
    let last_beat = (state.scroll_beat + grid.width() / state.pixels_per_beat).min(clip.length);
    let last_line = (last_beat / minor).ceil() as i32;
    for step in first_line..=last_line {
        let beat = step as f32 * minor;
        let x = beat_to_x(state, grid, beat);
        let on_bar = nearly_multiple(beat, beats_per_bar);
        let on_beat = nearly_multiple(beat, 1.0);
        let stroke = if on_bar {
            Stroke::new(1.4, BORDER_STRONG)
        } else if on_beat {
            Stroke::new(1.0, BORDER)
        } else {
            Stroke::new(0.5, BORDER)
        };
        painter.vline(x, grid.y_range(), stroke);
        if on_beat {
            painter.vline(x, ruler.y_range(), stroke);
            let bar = (beat / beats_per_bar).floor() as u32 + 1;
            let beat_in_bar = (beat % beats_per_bar).floor() as u32 + 1;
            painter.text(
                Pos2::new(x + 4.0, ruler.center().y),
                Align2::LEFT_CENTER,
                format!("{bar}.{beat_in_bar}"),
                FontId::monospace(8.5),
                if on_bar { TEXT } else { DIM },
            );
        }
    }
    painter.rect_stroke(
        grid,
        CornerRadius::ZERO,
        Stroke::new(1.0, BORDER),
        StrokeKind::Inside,
    );
}

#[allow(clippy::too_many_arguments)]
fn notes_ui(
    ui: &mut Ui,
    state: &mut PianoRollState,
    track: usize,
    clip_index: usize,
    clip: &Clip,
    notes: &[Note],
    grid: Rect,
    beats_per_bar: f32,
    actions: &mut Vec<Intent>,
) -> bool {
    let mut note_under_pointer = false;
    for note in notes {
        let y = grid.top() + (state.top_pitch - f32::from(note.pitch)) * state.row_height;
        let x = beat_to_x(state, grid, note.start);
        let width = (note.length * state.pixels_per_beat).max(5.0);
        let note_rect = Rect::from_min_size(
            Pos2::new(x, y + 1.0),
            Vec2::new(width, (state.row_height - 2.0).max(4.0)),
        );
        if !note_rect.intersects(grid) {
            continue;
        }
        let visible = note_rect.intersect(grid);
        let response = ui.interact(
            visible,
            egui::Id::new(("midi_note", &clip.id, note.event_index)),
            Sense::click_and_drag(),
        );
        note_under_pointer |= response.hovered();
        if response.clicked() {
            let additive = ui.input(|input| {
                input.modifiers.shift || input.modifiers.command || input.modifiers.ctrl
            });
            if additive {
                if !state.selected.insert(note.event_index) {
                    state.selected.remove(&note.event_index);
                }
            } else if !state.selected.contains(&note.event_index) {
                state.selected.clear();
                state.selected.insert(note.event_index);
            }
        }

        let resize_rect = Rect::from_min_max(
            Pos2::new(
                (note_rect.right() - 7.0).max(note_rect.left()),
                note_rect.top(),
            ),
            note_rect.right_bottom(),
        )
        .intersect(grid);
        let resize = ui.interact(
            resize_rect,
            egui::Id::new(("midi_resize", &clip.id, note.event_index)),
            Sense::drag(),
        );
        let drag_kind = if resize.dragged() || resize.drag_stopped() {
            Some(DragKind::Resize)
        } else if response.dragged_by(PointerButton::Primary) || response.drag_stopped() {
            Some(DragKind::Move)
        } else {
            None
        };
        let selected = state.selected.contains(&note.event_index);
        let mut preview_rect = note_rect;
        if let Some(kind) = drag_kind {
            let delta = if kind == DragKind::Resize {
                resize.drag_delta()
            } else {
                response.drag_delta()
            };
            match kind {
                DragKind::Resize => {
                    let maximum_right = beat_to_x(state, grid, clip.length);
                    let minimum_right =
                        (preview_rect.left() + 0.0625 * state.pixels_per_beat).min(maximum_right);
                    preview_rect.max.x =
                        (preview_rect.max.x + delta.x).clamp(minimum_right, maximum_right);
                }
                DragKind::Move => {
                    preview_rect = preview_rect.translate(Vec2::new(delta.x, delta.y));
                }
            }
            if resize.drag_stopped() {
                resize_selected(
                    state,
                    track,
                    clip_index,
                    clip.length,
                    notes,
                    note,
                    delta.x / state.pixels_per_beat,
                    beats_per_bar,
                    actions,
                );
            } else if response.drag_stopped() {
                move_selected(
                    state,
                    track,
                    clip_index,
                    clip,
                    notes,
                    note,
                    delta,
                    beats_per_bar,
                    actions,
                );
            }
        }

        let fill = if selected {
            HIGHLIGHT
        } else {
            EVENT_TONE.gamma_multiply(0.62 + note.velocity * 0.33)
        };
        ui.painter()
            .rect_filled(preview_rect.intersect(grid), CornerRadius::same(2), fill);
        ui.painter().rect_stroke(
            preview_rect.intersect(grid),
            CornerRadius::same(2),
            Stroke::new(1.0, fill.gamma_multiply(1.3)),
            StrokeKind::Inside,
        );
        if preview_rect.width() > 34.0 && state.row_height >= 14.0 {
            ui.painter().text(
                preview_rect.left_center() + Vec2::new(5.0, 0.0),
                Align2::LEFT_CENTER,
                note_name(note.pitch),
                FontId::monospace(8.0),
                Color32::from_gray(20),
            );
        }
        if response.hovered() {
            response.on_hover_text(format!(
                "{} · {:.2} beats · velocity {}",
                note_name(note.pitch),
                note.length,
                (note.velocity * 127.0).round() as u8
            ));
        }
    }
    note_under_pointer
}

#[allow(clippy::too_many_arguments)]
fn grid_interaction(
    ui: &mut Ui,
    state: &mut PianoRollState,
    response: &egui::Response,
    track: usize,
    clip_index: usize,
    clip: &Clip,
    notes: &[Note],
    grid: Rect,
    beats_per_bar: f32,
    velocity: u8,
    note_under_pointer: bool,
    actions: &mut Vec<Intent>,
) {
    let pointer = response.interact_pointer_pos();
    let in_grid = pointer.is_some_and(|point| grid.contains(point));
    let should_draw = (state.tool == Tool::Draw && response.clicked()) || response.double_clicked();
    if should_draw && in_grid && !note_under_pointer {
        let point = pointer.expect("checked");
        if let Some((start, length)) = new_note_range(
            state,
            x_to_beat(state, grid, point.x),
            clip.length,
            beats_per_bar,
        ) {
            let pitch = y_to_pitch(state, grid, point.y);
            actions.push(Intent::AddNote {
                track,
                clip: clip_index,
                start,
                length,
                pitch,
                velocity,
            });
            state.selected.clear();
        }
    } else if response.clicked() && in_grid && !note_under_pointer {
        state.selected.clear();
    }

    if state.tool == Tool::Select && !note_under_pointer && in_grid {
        if response.drag_started_by(PointerButton::Primary) {
            let point = pointer.expect("checked");
            state.marquee = Some(Marquee {
                anchor: point,
                current: point,
            });
        }
        if response.dragged_by(PointerButton::Primary)
            && let Some(marquee) = &mut state.marquee
            && let Some(point) = pointer
        {
            marquee.current = point;
        }
        if response.drag_stopped_by(PointerButton::Primary)
            && let Some(marquee) = state.marquee.take()
        {
            let selection_rect = Rect::from_two_pos(marquee.anchor, marquee.current);
            let additive = ui.input(|input| {
                input.modifiers.shift || input.modifiers.command || input.modifiers.ctrl
            });
            if !additive {
                state.selected.clear();
            }
            for note in notes {
                if note_rect(state, grid, note).intersects(selection_rect) {
                    state.selected.insert(note.event_index);
                }
            }
        }
    }
    if let Some(marquee) = state.marquee {
        ui.painter().rect_filled(
            Rect::from_two_pos(marquee.anchor, marquee.current).intersect(grid),
            CornerRadius::ZERO,
            HIGHLIGHT.gamma_multiply(0.12),
        );
        ui.painter().rect_stroke(
            Rect::from_two_pos(marquee.anchor, marquee.current).intersect(grid),
            CornerRadius::ZERO,
            Stroke::new(1.0, HIGHLIGHT),
            StrokeKind::Inside,
        );
    }
}

fn velocity_lane(
    ui: &mut Ui,
    state: &mut PianoRollState,
    track: usize,
    clip_index: usize,
    notes: &[Note],
    rect: Rect,
    response: &egui::Response,
    actions: &mut Vec<Intent>,
) {
    let label_rect = Rect::from_min_max(
        rect.left_top(),
        Pos2::new(rect.left() + KEY_WIDTH, rect.bottom()),
    );
    let lane = Rect::from_min_max(label_rect.right_top(), rect.right_bottom());
    ui.painter()
        .rect_filled(label_rect, CornerRadius::ZERO, PANEL_ALT);
    ui.painter().rect_filled(lane, CornerRadius::ZERO, PANEL);
    ui.painter().text(
        label_rect.left_center() + Vec2::new(8.0, 0.0),
        Align2::LEFT_CENTER,
        "VELOCITY",
        FontId::monospace(8.5),
        DIM,
    );
    for note in notes {
        let x = beat_to_x(state, lane, note.start) + 2.0;
        if !lane.x_range().contains(x) {
            continue;
        }
        let velocity = state
            .velocity_drag
            .filter(|preview| preview.event_index == note.event_index)
            .map_or(note.velocity, |preview| f32::from(preview.velocity) / 127.0);
        let top = lane.bottom() - velocity * (lane.height() - 8.0);
        let selected = state.selected.contains(&note.event_index);
        let bar = Rect::from_min_max(Pos2::new(x, top), Pos2::new(x + 5.0, lane.bottom() - 2.0));
        ui.painter().rect_filled(
            bar,
            CornerRadius::same(1),
            if selected { HIGHLIGHT } else { EVENT_TONE },
        );
    }
    let pointer = response.interact_pointer_pos();
    if response.clicked()
        && let Some(pointer) = pointer
        && lane.contains(pointer)
        && let Some(note) = velocity_target(state, notes, x_to_beat(state, lane, pointer.x))
    {
        state.selected.insert(note.event_index);
        actions.push(velocity_edit(
            track,
            clip_index,
            note,
            velocity_at_y(lane, pointer.y),
        ));
    }
    if response.drag_started()
        && let Some(pointer) = pointer
        && lane.contains(pointer)
        && let Some(note) = velocity_target(state, notes, x_to_beat(state, lane, pointer.x))
    {
        state.selected.insert(note.event_index);
        state.velocity_drag = Some(VelocityDrag {
            event_index: note.event_index,
            velocity: velocity_at_y(lane, pointer.y),
        });
    }
    if (response.dragged() || response.drag_stopped())
        && let Some(pointer) = pointer
        && let Some(preview) = &mut state.velocity_drag
    {
        preview.velocity = velocity_at_y(lane, pointer.y);
    }
    if response.drag_stopped()
        && let Some(preview) = state.velocity_drag.take()
        && let Some(note) = notes
            .iter()
            .find(|note| note.event_index == preview.event_index)
    {
        actions.push(velocity_edit(track, clip_index, *note, preview.velocity));
    }
    ui.painter().rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, BORDER),
        StrokeKind::Inside,
    );
}

fn velocity_target(state: &PianoRollState, notes: &[Note], target_beat: f32) -> Option<Note> {
    state
        .selected_notes(notes)
        .min_by(|a, b| {
            (a.start - target_beat)
                .abs()
                .total_cmp(&(b.start - target_beat).abs())
        })
        .or_else(|| {
            notes.iter().min_by(|a, b| {
                (a.start - target_beat)
                    .abs()
                    .total_cmp(&(b.start - target_beat).abs())
            })
        })
        .copied()
}

fn velocity_at_y(lane: Rect, y: f32) -> u8 {
    (((lane.bottom() - y) / (lane.height() - 8.0)) * 127.0)
        .round()
        .clamp(1.0, 127.0) as u8
}

fn velocity_edit(track: usize, clip: usize, note: Note, velocity: u8) -> Intent {
    Intent::EditNote {
        track,
        clip,
        event_index: note.event_index,
        start: note.start,
        length: note.length,
        pitch: note.pitch,
        velocity,
    }
}

fn keyboard_shortcuts(
    ui: &mut Ui,
    state: &mut PianoRollState,
    track: usize,
    clip_index: usize,
    clip: &Clip,
    notes: &[Note],
    beats_per_bar: f32,
    actions: &mut Vec<Intent>,
) {
    if ui.ctx().text_edit_focused() {
        return;
    }
    let mut delete = false;
    let mut select_all = false;
    let mut duplicate = false;
    let mut transpose = 0;
    let mut nudge = 0.0;
    ui.input_mut(|input| {
        delete = input.consume_key(egui::Modifiers::NONE, egui::Key::Delete)
            || input.consume_key(egui::Modifiers::NONE, egui::Key::Backspace);
        select_all = input.consume_key(egui::Modifiers::COMMAND, egui::Key::A);
        duplicate = input.consume_key(egui::Modifiers::COMMAND, egui::Key::D);
        if input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowUp) {
            transpose = 12;
        } else if input.consume_key(egui::Modifiers::SHIFT, egui::Key::ArrowDown) {
            transpose = -12;
        } else if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
            transpose = 1;
        } else if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
            transpose = -1;
        }
        if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft) {
            nudge = -state.grid.beats(beats_per_bar);
        } else if input.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight) {
            nudge = state.grid.beats(beats_per_bar);
        }
        if input.consume_key(egui::Modifiers::NONE, egui::Key::V) {
            state.tool = Tool::Select;
        } else if input.consume_key(egui::Modifiers::NONE, egui::Key::B) {
            state.tool = Tool::Draw;
        }
    });
    if select_all {
        state.selected = notes.iter().map(|note| note.event_index).collect();
    }
    if delete && !state.selected.is_empty() {
        actions.push(Intent::DeleteNotes {
            track,
            clip: clip_index,
            event_indices: state.selected.iter().copied().collect(),
        });
        state.selected.clear();
    }
    if transpose != 0 {
        transpose_selected(state, track, clip_index, notes, transpose, actions);
    }
    if nudge != 0.0 {
        let updates = state
            .selected_notes(notes)
            .map(|note| NoteUpdate {
                event_index: note.event_index,
                start: (note.start + nudge).clamp(0.0, (clip.length - note.length).max(0.0)),
                length: note.length,
                pitch: note.pitch,
                velocity: (note.velocity * 127.0).round() as u8,
            })
            .collect::<Vec<_>>();
        push_updates(track, clip_index, updates, actions);
        state.selected.clear();
    }
    if duplicate {
        let step = state.grid.beats(beats_per_bar);
        let inserts = state
            .selected_notes(notes)
            .filter_map(|note| {
                let start = note.start + step;
                (start < clip.length).then_some(NoteInsert {
                    start,
                    length: note.length.min(clip.length - start),
                    pitch: note.pitch,
                    velocity: (note.velocity * 127.0).round() as u8,
                })
            })
            .collect::<Vec<_>>();
        if !inserts.is_empty() {
            actions.push(Intent::AddNotes {
                track,
                clip: clip_index,
                notes: inserts,
            });
            state.selected.clear();
        }
    }
}

fn quantize_selected(
    state: &mut PianoRollState,
    track: usize,
    clip: usize,
    clip_length: f32,
    notes: &[Note],
    beats_per_bar: f32,
    actions: &mut Vec<Intent>,
) {
    let updates = state
        .selected_notes(notes)
        .map(|note| NoteUpdate {
            event_index: note.event_index,
            start: state
                .snap(note.start, beats_per_bar)
                .clamp(0.0, (clip_length - note.length).max(0.0)),
            length: note.length,
            pitch: note.pitch,
            velocity: (note.velocity * 127.0).round() as u8,
        })
        .collect();
    push_updates(track, clip, updates, actions);
    state.selected.clear();
}

fn transpose_selected(
    state: &mut PianoRollState,
    track: usize,
    clip: usize,
    notes: &[Note],
    semitones: i16,
    actions: &mut Vec<Intent>,
) {
    let updates = state
        .selected_notes(notes)
        .map(|note| NoteUpdate {
            event_index: note.event_index,
            start: note.start,
            length: note.length,
            pitch: (i16::from(note.pitch) + semitones).clamp(0, 127) as u8,
            velocity: (note.velocity * 127.0).round() as u8,
        })
        .collect();
    push_updates(track, clip, updates, actions);
    state.selected.clear();
}

#[allow(clippy::too_many_arguments)]
fn move_selected(
    state: &mut PianoRollState,
    track: usize,
    clip_index: usize,
    clip: &Clip,
    notes: &[Note],
    dragged: &Note,
    delta: Vec2,
    beats_per_bar: f32,
    actions: &mut Vec<Intent>,
) {
    if !state.selected.contains(&dragged.event_index) {
        state.selected.clear();
        state.selected.insert(dragged.event_index);
    }
    let requested = delta.x / state.pixels_per_beat;
    let snapped_start = state.snap(dragged.start + requested, beats_per_bar);
    let beat_delta = snapped_start - dragged.start;
    let pitch_delta = (-delta.y / state.row_height).round() as i16;
    let min_start = state
        .selected_notes(notes)
        .map(|note| note.start)
        .reduce(f32::min)
        .unwrap_or(0.0);
    let max_end = state
        .selected_notes(notes)
        .map(|note| note.start + note.length)
        .reduce(f32::max)
        .unwrap_or(clip.length);
    let beat_delta = clamp_move_delta(beat_delta, min_start, max_end, clip.length);
    let updates = state
        .selected_notes(notes)
        .map(|note| NoteUpdate {
            event_index: note.event_index,
            start: note.start + beat_delta,
            length: note.length,
            pitch: (i16::from(note.pitch) + pitch_delta).clamp(0, 127) as u8,
            velocity: (note.velocity * 127.0).round() as u8,
        })
        .collect();
    push_updates(track, clip_index, updates, actions);
    state.selected.clear();
}

fn clamp_move_delta(requested: f32, min_start: f32, max_end: f32, clip_length: f32) -> f32 {
    let minimum = -min_start;
    let maximum = clip_length - max_end;
    if minimum <= maximum {
        requested.clamp(minimum, maximum)
    } else {
        0.0
    }
}

#[allow(clippy::too_many_arguments)]
fn resize_selected(
    state: &mut PianoRollState,
    track: usize,
    clip_index: usize,
    clip_length: f32,
    notes: &[Note],
    dragged: &Note,
    delta: f32,
    beats_per_bar: f32,
    actions: &mut Vec<Intent>,
) {
    if !state.selected.contains(&dragged.event_index) {
        state.selected.clear();
        state.selected.insert(dragged.event_index);
    }
    let length = state
        .snap(dragged.length + delta, beats_per_bar)
        .max(0.0625);
    let requested_delta = length - dragged.length;
    let minimum_delta = state
        .selected_notes(notes)
        .map(|note| 0.0625_f32.min((clip_length - note.start).max(0.0)) - note.length)
        .reduce(f32::max)
        .unwrap_or(0.0);
    let maximum_delta = state
        .selected_notes(notes)
        .map(|note| clip_length - note.start - note.length)
        .reduce(f32::min)
        .unwrap_or(0.0);
    let length_delta = if minimum_delta <= maximum_delta {
        requested_delta.clamp(minimum_delta, maximum_delta)
    } else {
        0.0
    };
    let updates = state
        .selected_notes(notes)
        .map(|note| NoteUpdate {
            event_index: note.event_index,
            start: note.start,
            length: clamp_note_length(note.length + length_delta, note.start, clip_length),
            pitch: note.pitch,
            velocity: (note.velocity * 127.0).round() as u8,
        })
        .collect();
    push_updates(track, clip_index, updates, actions);
    state.selected.clear();
}

fn push_updates(track: usize, clip: usize, notes: Vec<NoteUpdate>, actions: &mut Vec<Intent>) {
    if !notes.is_empty() {
        actions.push(Intent::EditNotes { track, clip, notes });
    }
}

fn new_note_range(
    state: &PianoRollState,
    beat: f32,
    clip_length: f32,
    beats_per_bar: f32,
) -> Option<(f32, f32)> {
    if !(0.0..clip_length).contains(&beat) {
        return None;
    }
    let start = state.snap(beat, beats_per_bar);
    if !(0.0..clip_length).contains(&start) {
        return None;
    }
    let length = state.grid.beats(beats_per_bar).min(clip_length - start);
    (length > 0.0).then_some((start, length))
}

fn clamp_note_length(length: f32, start: f32, clip_length: f32) -> f32 {
    let maximum = (clip_length - start).max(0.0);
    length.clamp(0.0625_f32.min(maximum), maximum)
}

fn paint_playhead(ui: &Ui, state: &PianoRollState, clip: &Clip, grid: Rect, playhead: f32) {
    let local = playhead - clip.start;
    if (0.0..=clip.length).contains(&local) {
        let x = beat_to_x(state, grid, local);
        if grid.x_range().contains(x) {
            ui.painter()
                .vline(x, grid.y_range(), Stroke::new(1.5, PLAYHEAD));
            ui.painter()
                .circle_filled(Pos2::new(x, grid.top()), 3.0, PLAYHEAD);
        }
    }
}

fn beat_to_x(state: &PianoRollState, grid: Rect, beat: f32) -> f32 {
    grid.left() + (beat - state.scroll_beat) * state.pixels_per_beat
}

fn x_to_beat(state: &PianoRollState, grid: Rect, x: f32) -> f32 {
    state.scroll_beat + (x - grid.left()) / state.pixels_per_beat
}

fn y_to_pitch(state: &PianoRollState, grid: Rect, y: f32) -> u8 {
    (state.top_pitch - (y - grid.top()) / state.row_height)
        .round()
        .clamp(0.0, 127.0) as u8
}

fn note_rect(state: &PianoRollState, grid: Rect, note: &Note) -> Rect {
    Rect::from_min_size(
        Pos2::new(
            beat_to_x(state, grid, note.start),
            grid.top() + (state.top_pitch - f32::from(note.pitch)) * state.row_height + 1.0,
        ),
        Vec2::new(
            (note.length * state.pixels_per_beat).max(5.0),
            (state.row_height - 2.0).max(4.0),
        ),
    )
}

fn is_black_key(pitch: u8) -> bool {
    matches!(pitch % 12, 1 | 3 | 6 | 8 | 10)
}

fn note_name(pitch: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B",
    ];
    format!(
        "{}{}",
        NAMES[usize::from(pitch % 12)],
        i16::from(pitch / 12) - 1
    )
}

fn nearly_multiple(value: f32, step: f32) -> bool {
    let remainder = value.rem_euclid(step);
    remainder < 0.001 || step - remainder < 0.001
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn note_names_cover_octaves_and_accidentals() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(61), "C♯4");
        assert_eq!(note_name(0), "C-1");
        assert_eq!(note_name(127), "G9");
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn grid_sizes_map_to_quarter_note_beats() {
        assert_eq!(GridSize::Bar.beats(4.0), 4.0);
        assert_eq!(GridSize::Beat.beats(4.0), 1.0);
        assert_eq!(GridSize::Sixteenth.beats(4.0), 0.25);
        assert_eq!(GridSize::ThirtySecond.beats(4.0), 0.125);
    }

    #[test]
    fn black_key_detection_matches_pitch_classes() {
        assert!(!is_black_key(60));
        assert!(is_black_key(61));
        assert!(!is_black_key(64));
        assert!(is_black_key(70));
    }

    #[test]
    fn escape_unwinds_focus_then_note_selection() {
        let mut state = PianoRollState {
            fullscreen: true,
            ..PianoRollState::default()
        };
        state.selected.insert(4);

        assert!(state.handle_escape());
        assert!(!state.fullscreen);
        assert!(!state.selected.is_empty());
        assert!(state.handle_escape());
        assert!(state.selected.is_empty());
        assert!(!state.handle_escape());
    }

    #[test]
    fn tall_pitch_views_clamp_without_panicking() {
        let grid = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 2_000.0));
        let mut state = PianoRollState {
            top_pitch: 60.0,
            fit_pitch_pending: true,
            ..PianoRollState::default()
        };

        clamp_view(&mut state, grid, 16.0);
        assert_eq!(state.top_pitch, 127.0);

        fit_pitch_view(&mut state, &[], grid);
        assert_eq!(state.top_pitch, 127.0);
    }

    #[test]
    fn drawing_rejects_clip_end_and_outside_positions() {
        let state = PianoRollState::default();

        assert_eq!(new_note_range(&state, 4.0, 4.0, 4.0), None);
        assert_eq!(new_note_range(&state, 4.5, 4.0, 4.0), None);
        assert_eq!(new_note_range(&state, -0.1, 4.0, 4.0), None);
        assert_eq!(new_note_range(&state, 3.9, 4.0, 4.0), None);
        assert_eq!(new_note_range(&state, 3.8, 4.0, 4.0), Some((3.75, 0.25)));
    }

    #[test]
    fn quantize_keeps_note_end_inside_clip() {
        let note = Note {
            event_index: 7,
            start: 3.8,
            length: 0.5,
            pitch: 60,
            velocity: 0.75,
        };
        let mut state = PianoRollState::default();
        state.selected.insert(note.event_index);
        let mut actions = Vec::new();

        quantize_selected(&mut state, 1, 2, 4.0, &[note], 4.0, &mut actions);

        let Intent::EditNotes { notes, .. } = &actions[0] else {
            panic!("expected bulk note edit");
        };
        assert_eq!(notes[0].start, 3.5);
        assert!(notes[0].start + notes[0].length <= 4.0);
    }

    #[test]
    fn resize_keeps_every_selected_note_inside_clip() {
        let notes = [
            Note {
                event_index: 1,
                start: 1.0,
                length: 1.0,
                pitch: 60,
                velocity: 0.75,
            },
            Note {
                event_index: 2,
                start: 3.0,
                length: 0.5,
                pitch: 64,
                velocity: 0.75,
            },
        ];
        let mut state = PianoRollState::default();
        state.selected.extend([1, 2]);
        let mut actions = Vec::new();

        resize_selected(
            &mut state,
            1,
            2,
            4.0,
            &notes,
            &notes[0],
            4.0,
            4.0,
            &mut actions,
        );

        let Intent::EditNotes { notes, .. } = &actions[0] else {
            panic!("expected bulk note edit");
        };
        assert!(notes.iter().all(|note| note.start + note.length <= 4.0));
        assert_eq!(notes[0].length, 1.5);
        assert_eq!(notes[1].length, 1.0);
    }

    #[test]
    fn move_delta_is_safe_when_selection_cannot_fit_clip() {
        assert_eq!(clamp_move_delta(1.0, 0.0, 5.0, 4.0), 0.0);
        assert_eq!(clamp_move_delta(1.0, 1.0, 4.5, 4.0), -0.5);
    }

    #[test]
    fn velocity_preview_values_are_bounded() {
        let lane = Rect::from_min_size(Pos2::ZERO, Vec2::new(200.0, VELOCITY_HEIGHT));

        assert_eq!(velocity_at_y(lane, lane.top() - 20.0), 127);
        assert_eq!(velocity_at_y(lane, lane.bottom() + 20.0), 1);
    }
}
