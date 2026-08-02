// Geometry conversions are clamped to the finite, visible arrangement canvas.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::ops::Range;

use egui::{
    Align2, Color32, CornerRadius, FontId, Id, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui,
    Vec2,
};

use crate::model::{
    Clip, ClipKind, DemoViewModel, Intent, RenderState, Selection, SyncMode, TrackKind,
};

pub const TRACK_HEIGHT: f32 = 86.0;
const RULER_HEIGHT: f32 = 30.0;
const TRACK_HEADER_WIDTH: f32 = 138.0;
const MIN_PIXELS_PER_BEAT: f32 = 14.0;
const MAX_PIXELS_PER_BEAT: f32 = 96.0;

const BG: Color32 = Color32::from_rgb(15, 18, 24);
const GRID: Color32 = Color32::from_rgb(39, 44, 54);
const TEXT_DIM: Color32 = Color32::from_rgb(145, 154, 171);
const AUDIO: Color32 = Color32::from_rgb(57, 185, 173);
const EVENT: Color32 = Color32::from_rgb(175, 119, 238);
const NESTED: Color32 = Color32::from_rgb(239, 151, 68);
const ACCENT: Color32 = Color32::from_rgb(94, 210, 255);

#[derive(Debug)]
pub struct TimelineState {
    pub pixels_per_beat: f32,
    pub dragging_asset: Option<usize>,
}

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            pixels_per_beat: 32.0,
            dragging_asset: None,
        }
    }
}

impl TimelineState {
    pub fn zoom_by(&mut self, amount: f32) {
        self.pixels_per_beat =
            (self.pixels_per_beat * amount).clamp(MIN_PIXELS_PER_BEAT, MAX_PIXELS_PER_BEAT);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimelineTransform {
    pub origin_x: f32,
    pub pixels_per_beat: f32,
}

impl TimelineTransform {
    pub fn beat_to_x(self, beat: f32) -> f32 {
        self.origin_x + beat * self.pixels_per_beat
    }

    pub fn x_to_beat(self, x: f32) -> f32 {
        (x - self.origin_x) / self.pixels_per_beat
    }
}

pub fn visible_track_range(view_top: f32, view_bottom: f32, count: usize) -> Range<usize> {
    if count == 0 || view_bottom <= RULER_HEIGHT {
        return 0..0;
    }
    let first = ((view_top - RULER_HEIGHT).max(0.0) / TRACK_HEIGHT).floor() as usize;
    let last = (((view_bottom - RULER_HEIGHT).max(0.0) / TRACK_HEIGHT).ceil() as usize).min(count);
    first.min(count)..last
}

fn clip_intersects_visible(clip: &Clip, start: f32, end: f32) -> bool {
    clip_visual_end(clip) > start && clip.start < end
}

fn clip_visual_end(clip: &Clip) -> f32 {
    clip.end()
        + match clip.kind {
            ClipKind::Composition { tail_beats, .. } => tail_beats,
            _ => 0.0,
        }
}

pub fn visible_clip_range(
    clips: &[Clip],
    max_visual_length: f32,
    start: f32,
    end: f32,
) -> Range<usize> {
    let earliest_start = start - max_visual_length;
    let first = clips.partition_point(|clip| clip.start < earliest_start);
    let last = clips.partition_point(|clip| clip.start < end);
    first.min(last)..last
}

pub fn timeline(
    ui: &mut Ui,
    vm: &DemoViewModel,
    state: &mut TimelineState,
    now: f64,
) -> Vec<Intent> {
    let mut actions = Vec::new();
    let composition = vm.current_composition();
    let content_width =
        TRACK_HEADER_WIDTH + composition.length_beats * state.pixels_per_beat + 120.0;
    let content_height = RULER_HEIGHT + composition.tracks.len() as f32 * TRACK_HEIGHT;
    let scroll = ui.input(|input| input.smooth_scroll_delta);
    if ui.rect_contains_pointer(ui.max_rect())
        && ui.input(|input| input.modifiers.command || input.modifiers.ctrl)
        && scroll.y != 0.0
    {
        state.zoom_by((1.0 + scroll.y * 0.004).clamp(0.75, 1.25));
        ui.input_mut(|input| input.smooth_scroll_delta = Vec2::ZERO);
    }

    egui::ScrollArea::both()
        .id_salt("arrangement_scroll")
        .auto_shrink([false, false])
        .show_viewport(ui, |ui, viewport| {
            let (canvas, canvas_response) = ui.allocate_exact_size(
                Vec2::new(content_width, content_height),
                Sense::click_and_drag(),
            );
            let painter = ui
                .painter()
                .with_clip_rect(canvas.intersect(ui.clip_rect()));
            painter.rect_filled(canvas, 0.0, BG);
            let transform = TimelineTransform {
                origin_x: canvas.left() + TRACK_HEADER_WIDTH,
                pixels_per_beat: state.pixels_per_beat,
            };
            let visible_start = transform
                .x_to_beat(canvas.left() + viewport.left())
                .max(0.0);
            let visible_end = transform
                .x_to_beat(canvas.left() + viewport.right())
                .min(composition.length_beats);
            paint_grid(
                &painter,
                canvas,
                viewport,
                transform,
                composition.length_beats,
            );

            let rows =
                visible_track_range(viewport.top(), viewport.bottom(), composition.tracks.len());
            for track_index in rows {
                let track = &composition.tracks[track_index];
                let track_top = canvas.top() + RULER_HEIGHT + track_index as f32 * TRACK_HEIGHT;
                let track_rect = Rect::from_min_max(
                    Pos2::new(canvas.left(), track_top),
                    Pos2::new(canvas.right(), track_top + TRACK_HEIGHT),
                );
                painter.hline(
                    track_rect.x_range(),
                    track_rect.bottom(),
                    Stroke::new(1.0, GRID),
                );
                let clip_range = visible_clip_range(
                    &track.clips,
                    track.max_visual_length,
                    visible_start,
                    visible_end,
                );
                for (offset, clip) in track.clips[clip_range.clone()].iter().enumerate() {
                    let clip_index = clip_range.start + offset;
                    if !clip_intersects_visible(clip, visible_start, visible_end) {
                        continue;
                    }
                    let clip_rect = Rect::from_min_max(
                        Pos2::new(transform.beat_to_x(clip.start), track_rect.top() + 8.0),
                        Pos2::new(transform.beat_to_x(clip.end()), track_rect.bottom() - 8.0),
                    );
                    paint_clip(
                        ui,
                        &painter,
                        vm,
                        clip,
                        clip_rect,
                        track_index,
                        clip_index,
                        now,
                        &mut actions,
                    );
                }
            }

            paint_sticky_headers(
                ui,
                &painter,
                vm,
                canvas,
                viewport,
                transform,
                composition.length_beats,
                &mut actions,
            );
            paint_playhead(&painter, canvas, viewport, transform, vm.transport.playhead);
            handle_canvas_interaction(
                &canvas_response,
                canvas,
                viewport,
                transform,
                composition.length_beats,
                composition.tracks.len(),
                state,
                &mut actions,
            );
        });
    actions
}

fn paint_grid(
    painter: &egui::Painter,
    canvas: Rect,
    viewport: Rect,
    transform: TimelineTransform,
    composition_length: f32,
) {
    let start = transform
        .x_to_beat(canvas.left() + viewport.left())
        .floor()
        .max(0.0) as u32;
    let end = transform
        .x_to_beat(canvas.left() + viewport.right())
        .ceil()
        .min(composition_length) as u32;
    for beat in start..=end {
        let x = transform.beat_to_x(beat as f32);
        let bar = beat % 4 == 0;
        painter.vline(
            x,
            canvas.y_range(),
            Stroke::new(
                if bar { 1.2 } else { 0.6 },
                if bar { GRID.gamma_multiply(1.5) } else { GRID },
            ),
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_clip(
    ui: &mut Ui,
    painter: &egui::Painter,
    vm: &DemoViewModel,
    clip: &Clip,
    rect: Rect,
    track_index: usize,
    clip_index: usize,
    now: f64,
    actions: &mut Vec<Intent>,
) {
    if !rect.is_positive() {
        return;
    }
    let tail_width = match clip.kind {
        ClipKind::Composition { tail_beats, .. } => {
            tail_beats / clip.length.max(f32::EPSILON) * rect.width()
        }
        _ => 0.0,
    };
    let visual_rect = Rect::from_min_max(
        rect.left_top(),
        Pos2::new(rect.right() + tail_width, rect.bottom()),
    );
    let body_clip = Rect::from_min_max(
        Pos2::new(
            ui.clip_rect().left() + TRACK_HEADER_WIDTH,
            ui.clip_rect().top() + RULER_HEIGHT,
        ),
        ui.clip_rect().right_bottom(),
    );
    let clip_painter = painter.with_clip_rect(visual_rect.intersect(body_clip));
    let painter = &clip_painter;
    let selected = matches!(vm.selection, Selection::Clip { track, clip } | Selection::Effect { track, clip, .. } if track == track_index && clip == clip_index);
    let color = match clip.kind {
        ClipKind::Audio { .. } => AUDIO,
        ClipKind::Event { .. } => EVENT,
        ClipKind::Composition { .. } => NESTED,
    };
    let fill = color.gamma_multiply(if selected { 0.43 } else { 0.29 });
    painter.rect_filled(rect, CornerRadius::same(5), fill);
    painter.rect_stroke(
        rect,
        CornerRadius::same(5),
        Stroke::new(
            if selected { 2.0 } else { 1.0 },
            if selected {
                ACCENT
            } else {
                color.gamma_multiply(0.8)
            },
        ),
        StrokeKind::Inside,
    );

    match &clip.kind {
        ClipKind::Audio { .. } => paint_waveform(
            painter,
            rect.shrink2(Vec2::new(4.0, 18.0)),
            &clip.waveform,
            color,
        ),
        ClipKind::Event { notes } => paint_notes(
            painter,
            rect.shrink2(Vec2::new(4.0, 17.0)),
            notes,
            clip.length,
            color,
        ),
        ClipKind::Composition { .. } => {
            paint_waveform(
                painter,
                rect.shrink2(Vec2::new(6.0, 18.0)),
                &clip.waveform,
                color,
            );
            painter.rect_stroke(
                rect.shrink(3.0),
                CornerRadius::same(3),
                Stroke::new(1.0, color.gamma_multiply(0.7)),
                StrokeKind::Inside,
            );
            let tail = Rect::from_min_max(
                rect.right_top(),
                Pos2::new(rect.right() + tail_width, rect.bottom()),
            );
            let tail_painter = painter.with_clip_rect(tail.intersect(painter.clip_rect()));
            tail_painter.rect_filled(tail, CornerRadius::same(4), color.gamma_multiply(0.16));
            tail_painter.rect_stroke(
                tail,
                CornerRadius::same(4),
                Stroke::new(1.0, color.gamma_multiply(0.55)),
                StrokeKind::Inside,
            );
            let mut x = tail.left() - tail.height();
            while x < tail.right() {
                tail_painter.line_segment(
                    [
                        Pos2::new(x, tail.bottom()),
                        Pos2::new(x + tail.height(), tail.top()),
                    ],
                    Stroke::new(1.0, color.gamma_multiply(0.55)),
                );
                x += 8.0;
            }
        }
    }

    painter.text(
        rect.left_top() + Vec2::new(7.0, 6.0),
        Align2::LEFT_TOP,
        &clip.name,
        FontId::proportional(11.5),
        Color32::WHITE,
    );
    paint_clip_status(painter, rect, clip, vm.transport.bpm);
    if vm.structure_lens {
        painter.text(
            rect.left_bottom() + Vec2::new(7.0, -5.0),
            Align2::LEFT_BOTTOM,
            &clip.id,
            FontId::monospace(9.0),
            color.gamma_multiply(0.8),
        );
    }
    let agent_alpha = vm.highlight_alpha(&clip.id, now);
    if agent_alpha > 0.0 {
        painter.rect_stroke(
            rect.expand(2.0),
            CornerRadius::same(7),
            Stroke::new(
                2.0,
                Color32::from_rgba_unmultiplied(114, 230, 255, (agent_alpha * 230.0) as u8),
            ),
            StrokeKind::Outside,
        );
    }

    let interaction_rect = rect.intersect(body_clip);
    if !interaction_rect.is_positive() {
        return;
    }
    let response = ui.interact(
        interaction_rect,
        Id::new(("clip", &clip.id)),
        Sense::click(),
    );
    if response.clicked() {
        actions.push(Intent::Select(Selection::Clip {
            track: track_index,
            clip: clip_index,
        }));
    }
    if response.double_clicked() && matches!(clip.kind, ClipKind::Composition { .. }) {
        actions.push(Intent::EnterChild {
            track: track_index,
            clip: clip_index,
        });
    }
    response.on_hover_text(match clip.kind {
        ClipKind::Composition { .. } => "Double-click to enter composition",
        _ => "Click to inspect",
    });
}

fn paint_clip_status(painter: &egui::Painter, rect: Rect, clip: &Clip, project_bpm: f32) {
    match clip.kind {
        ClipKind::Audio {
            sync,
            source_bpm: Some(source_bpm),
            ..
        } if sync != SyncMode::None => {
            painter.text(
                rect.right_bottom() - Vec2::new(6.0, 5.0),
                Align2::RIGHT_BOTTOM,
                format!("{source_bpm:.0} → {project_bpm:.0} {}", sync.label()),
                FontId::monospace(9.0),
                Color32::from_rgb(181, 242, 232),
            );
        }
        ClipKind::Composition {
            render: RenderState::Stale,
            ..
        } => badge(painter, rect, "STALE", Color32::from_rgb(238, 113, 87)),
        ClipKind::Composition {
            render: RenderState::Rendering(progress),
            ..
        } => {
            badge(
                painter,
                rect,
                &format!("RENDER {progress}%"),
                Color32::from_rgb(92, 182, 245),
            );
        }
        _ => {}
    }
}

fn badge(painter: &egui::Painter, rect: Rect, text: &str, color: Color32) {
    let badge_rect = Rect::from_min_size(
        rect.right_top() + Vec2::new(-75.0, 5.0),
        Vec2::new(69.0, 16.0),
    );
    painter.rect_filled(
        badge_rect,
        CornerRadius::same(3),
        color.gamma_multiply(0.38),
    );
    painter.text(
        badge_rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::monospace(8.5),
        Color32::WHITE,
    );
}

pub fn paint_waveform(painter: &egui::Painter, rect: Rect, waveform: &[f32], color: Color32) {
    if waveform.is_empty() || rect.width() < 1.0 || rect.height() < 2.0 {
        return;
    }
    let visible = rect.intersect(painter.clip_rect());
    if !visible.is_positive() {
        return;
    }
    let center = rect.center().y;
    let mut x = visible.left().floor();
    let right = visible.right().ceil();
    while x < right {
        let phase = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let sample_index = ((waveform.len() - 1) as f32 * phase) as usize;
        let amplitude = waveform[sample_index] * rect.height() * 0.45;
        painter.line_segment(
            [
                Pos2::new(x, center - amplitude),
                Pos2::new(x, center + amplitude),
            ],
            Stroke::new(1.0, color.gamma_multiply(0.9)),
        );
        x += 1.0;
    }
}

fn paint_notes(
    painter: &egui::Painter,
    rect: Rect,
    notes: &[crate::model::Note],
    clip_length: f32,
    color: Color32,
) {
    for note in notes {
        let x = rect.left() + note.start / clip_length * rect.width();
        let width = (note.length / clip_length * rect.width()).max(2.0);
        let pitch = f32::from(note.pitch.saturating_sub(36)) / 48.0;
        let y = rect.bottom() - pitch.clamp(0.0, 1.0) * rect.height();
        let note_rect = Rect::from_min_size(Pos2::new(x, y - 2.0), Vec2::new(width, 4.0));
        painter.rect_filled(
            note_rect,
            1.0,
            color.gamma_multiply(0.65 + note.velocity * 0.3),
        );
    }
}

fn paint_playhead(
    painter: &egui::Painter,
    canvas: Rect,
    viewport: Rect,
    transform: TimelineTransform,
    beat: f32,
) {
    let visible_body = Rect::from_min_max(
        Pos2::new(
            canvas.left() + viewport.left() + TRACK_HEADER_WIDTH,
            canvas.top() + viewport.top(),
        ),
        Pos2::new(
            canvas.left() + viewport.right(),
            canvas.top() + viewport.bottom(),
        ),
    );
    let painter = &painter.with_clip_rect(visible_body.intersect(painter.clip_rect()));
    let x = transform.beat_to_x(beat);
    painter.vline(
        x,
        canvas.y_range(),
        Stroke::new(1.5, Color32::from_rgb(255, 104, 124)),
    );
    let y = canvas.top() + viewport.top();
    painter.rect_filled(
        Rect::from_center_size(Pos2::new(x, y + 3.0), Vec2::new(7.0, 7.0)),
        CornerRadius::same(2),
        Color32::from_rgb(255, 104, 124),
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_sticky_headers(
    ui: &mut Ui,
    painter: &egui::Painter,
    vm: &DemoViewModel,
    canvas: Rect,
    viewport: Rect,
    transform: TimelineTransform,
    composition_length: f32,
    actions: &mut Vec<Intent>,
) {
    let sticky_x = canvas.left() + viewport.left();
    let sticky_y = canvas.top() + viewport.top();
    let ruler = Rect::from_min_size(
        Pos2::new(sticky_x, sticky_y),
        Vec2::new(viewport.width(), RULER_HEIGHT),
    );
    painter.rect_filled(ruler, 0.0, Color32::from_rgb(23, 27, 35));
    painter.hline(ruler.x_range(), ruler.bottom(), Stroke::new(1.0, GRID));
    let start = transform
        .x_to_beat(canvas.left() + viewport.left())
        .floor()
        .max(0.0) as u32;
    let end = transform
        .x_to_beat(canvas.left() + viewport.right())
        .ceil()
        .min(composition_length) as u32;
    for beat in start..=end {
        if beat % 4 == 0 {
            painter.text(
                Pos2::new(transform.beat_to_x(beat as f32) + 5.0, ruler.center().y),
                Align2::LEFT_CENTER,
                beat / 4 + 1,
                FontId::monospace(11.0),
                TEXT_DIM,
            );
        }
    }
    let rows = visible_track_range(
        viewport.top(),
        viewport.bottom(),
        vm.current_composition().tracks.len(),
    );
    for track_index in rows {
        let track = &vm.current_composition().tracks[track_index];
        let top = canvas.top() + RULER_HEIGHT + track_index as f32 * TRACK_HEIGHT;
        let header = Rect::from_min_size(
            Pos2::new(sticky_x, top),
            Vec2::new(TRACK_HEADER_WIDTH, TRACK_HEIGHT),
        );
        painter.rect_filled(header, 0.0, Color32::from_rgb(22, 26, 34));
        painter.vline(header.right(), header.y_range(), Stroke::new(1.0, GRID));
        painter.text(
            header.left_top() + Vec2::new(12.0, 13.0),
            Align2::LEFT_TOP,
            &track.name,
            FontId::proportional(11.0),
            Color32::from_rgb(224, 228, 236),
        );
        let kind = match track.kind {
            TrackKind::Audio => "AUDIO",
            TrackKind::Event => "EVENT",
            TrackKind::Composition => "NEST",
        };
        painter.text(
            header.left_top() + Vec2::new(12.0, 31.0),
            Align2::LEFT_TOP,
            kind,
            FontId::monospace(8.5),
            TEXT_DIM,
        );
        if vm.structure_lens {
            painter.text(
                header.left_bottom() + Vec2::new(12.0, -8.0),
                Align2::LEFT_BOTTOM,
                &track.id,
                FontId::monospace(8.5),
                TEXT_DIM,
            );
        }
        let mute_rect = Rect::from_min_size(
            header.left_top() + Vec2::new(77.0, 48.0),
            Vec2::new(22.0, 20.0),
        );
        let solo_rect = mute_rect.translate(Vec2::new(25.0, 0.0));
        let meter = Rect::from_min_size(
            header.right_top() + Vec2::new(-8.0, 12.0),
            Vec2::new(3.0, 56.0),
        );
        painter.rect_filled(meter, 1.0, GRID);
        let level_height = meter.height() * track.level.clamp(0.0, 1.0);
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(meter.left(), meter.bottom() - level_height),
                meter.right_bottom(),
            ),
            1.0,
            ACCENT.gamma_multiply(0.8),
        );
        paint_toggle(
            painter,
            mute_rect,
            "M",
            track.muted,
            Color32::from_rgb(236, 100, 87),
        );
        paint_toggle(
            painter,
            solo_rect,
            "S",
            track.solo,
            Color32::from_rgb(244, 194, 76),
        );
        if ui
            .interact(mute_rect, Id::new(("mute", &track.id)), Sense::click())
            .clicked()
        {
            actions.push(Intent::ToggleMute(track_index));
        }
        if ui
            .interact(solo_rect, Id::new(("solo", &track.id)), Sense::click())
            .clicked()
        {
            actions.push(Intent::ToggleSolo(track_index));
        }
    }
    let corner = Rect::from_min_size(
        Pos2::new(sticky_x, sticky_y),
        Vec2::new(TRACK_HEADER_WIDTH, RULER_HEIGHT),
    );
    painter.rect_filled(corner, 0.0, Color32::from_rgb(28, 33, 42));
    painter.text(
        corner.left_center() + Vec2::new(12.0, 0.0),
        Align2::LEFT_CENTER,
        "ARRANGEMENT",
        FontId::monospace(9.0),
        TEXT_DIM,
    );
}

fn paint_toggle(painter: &egui::Painter, rect: Rect, text: &str, active: bool, color: Color32) {
    painter.rect_filled(
        rect,
        CornerRadius::same(3),
        if active {
            color.gamma_multiply(0.55)
        } else {
            Color32::from_rgb(35, 40, 50)
        },
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::monospace(9.0),
        if active { Color32::WHITE } else { TEXT_DIM },
    );
}

#[allow(clippy::too_many_arguments)]
fn handle_canvas_interaction(
    response: &Response,
    canvas: Rect,
    viewport: Rect,
    transform: TimelineTransform,
    length: f32,
    track_count: usize,
    state: &mut TimelineState,
    actions: &mut Vec<Intent>,
) {
    if response.clicked()
        && let Some(pointer) = response.interact_pointer_pos()
        && pointer.x > canvas.left() + viewport.left() + TRACK_HEADER_WIDTH
    {
        actions.push(Intent::Seek(
            transform.x_to_beat(pointer.x).clamp(0.0, length),
        ));
    }
    let released = response.ctx.input(|input| input.pointer.any_released());
    if released
        && let Some(asset) = state.dragging_asset.take()
        && let Some(pointer) = response.ctx.pointer_hover_pos()
        && response.rect.contains(pointer)
        && pointer.x > canvas.left() + viewport.left() + TRACK_HEADER_WIDTH
    {
        actions.push(Intent::AddAssetClip {
            asset,
            beat: transform.x_to_beat(pointer.x).clamp(0.0, length),
            track: if pointer.y > canvas.top() + RULER_HEIGHT {
                Some(
                    (((pointer.y - canvas.top() - RULER_HEIGHT) / TRACK_HEIGHT).floor() as usize)
                        .min(track_count.saturating_sub(1)),
                )
            } else {
                None
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn clip(start: f32, length: f32) -> Clip {
        Clip {
            id: String::new(),
            name: String::new(),
            start,
            length,
            gain_db: 0.0,
            waveform: Arc::from([]),
            kind: ClipKind::Event {
                notes: Arc::from([]),
            },
            effects: Vec::new(),
        }
    }

    #[test]
    fn transform_round_trips() {
        let transform = TimelineTransform {
            origin_x: 137.0,
            pixels_per_beat: 38.0,
        };
        let beat = 23.75;
        assert!((transform.x_to_beat(transform.beat_to_x(beat)) - beat).abs() < 0.000_1);
    }

    #[test]
    fn visible_tracks_are_clamped_and_virtualized() {
        assert_eq!(visible_track_range(0.0, 200.0, 100), 0..2);
        assert_eq!(visible_track_range(500.0, 700.0, 100), 5..8);
        assert_eq!(visible_track_range(8_500.0, 9_000.0, 10), 10..10);
        assert_eq!(visible_track_range(0.0, 200.0, 0), 0..0);
    }

    #[test]
    fn clip_intersection_is_half_open() {
        assert!(!clip_intersects_visible(&clip(0.0, 4.0), 4.0, 8.0));
        assert!(clip_intersects_visible(&clip(3.0, 2.0), 4.0, 8.0));
        assert!(!clip_intersects_visible(&clip(8.0, 2.0), 4.0, 8.0));
    }

    #[test]
    fn visible_clip_range_culls_a_large_sorted_track() {
        let clips = (0..10_000)
            .map(|index| clip(index as f32 * 2.0, 1.0))
            .collect::<Vec<_>>();
        assert_eq!(
            visible_clip_range(&clips, 1.0, 8_000.0, 8_010.0),
            4_000..4_005
        );
    }

    #[test]
    fn zoom_is_clamped() {
        let mut state = TimelineState::default();
        state.zoom_by(100.0);
        assert!((state.pixels_per_beat - MAX_PIXELS_PER_BEAT).abs() < f32::EPSILON);
        state.zoom_by(0.001);
        assert!((state.pixels_per_beat - MIN_PIXELS_PER_BEAT).abs() < f32::EPSILON);
    }
}
