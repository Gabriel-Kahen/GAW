// Geometry conversions are clamped to the finite, visible arrangement canvas.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::ops::Range;

use egui::{
    Align2, Color32, CornerRadius, FontId, Id, PointerButton, Pos2, Rect, Response, Sense, Stroke,
    StrokeKind, Ui, Vec2,
};

use crate::model::{
    Clip, ClipKind, DemoViewModel, Intent, RenderState, Selection, SyncMode, TrackKind,
    WaveformPoint,
};
use crate::theme::{
    AUDIO_TONE, BORDER, DIM, EVENT_TONE, HIGHLIGHT, NESTED_TONE, PANEL, PANEL_ALT, PANEL_RAISED,
    PLAYHEAD, STATUS_ERROR, STATUS_NOTICE, TEXT,
};

pub const TRACK_HEIGHT: f32 = 72.0;
const RULER_HEIGHT: f32 = 30.0;
const TRACKS_DEFAULT_WIDTH: f32 = 138.0;
const TRACKS_MIN_WIDTH: f32 = 132.0;
const TRACKS_COLLAPSED_WIDTH: f32 = 28.0;
const TIMELINE_MIN_WIDTH: f32 = 320.0;
const TRACKS_RESIZE_HANDLE_WIDTH: f32 = 7.0;
const MIN_ARRANGEMENT_BEATS: f32 = 64.0;
const MIN_PIXELS_PER_BEAT: f32 = 4.0;
const MAX_PIXELS_PER_BEAT: f32 = 512.0;
const SNAP_BEATS: f32 = 0.25;
const MIN_CLIP_BEATS: f32 = 0.25;
const RESIZE_HANDLE_WIDTH: f32 = 7.0;
const MIN_GRID_SPACING: f32 = 7.0;
const FULL_GRID_SPACING: f32 = 18.0;
// Keep the dominant grid cadence comfortably readable. At overview scales this
// promotes 2/4/8/... bar groups instead of making every bar a major line.
const MAJOR_BAR_SPACING: f32 = 64.0;
const MIN_RULER_LABEL_SPACING: f32 = 48.0;
const MAX_SUBDIVISION_DEPTH: u8 = 8;
const ARRANGEMENT_SCROLL_SALT: &str = "arrangement_scroll";

const BG: Color32 = PANEL;
const GRID: Color32 = BORDER;
const TEXT_DIM: Color32 = DIM;
const AUDIO: Color32 = AUDIO_TONE;
const EVENT: Color32 = EVENT_TONE;
const NESTED: Color32 = NESTED_TONE;
const ACCENT: Color32 = HIGHLIGHT;

#[derive(Debug)]
pub struct TimelineState {
    pub pixels_per_beat: f32,
    pub dragging_asset: Option<gaw_core::AssetId>,
    clip_drag: Option<ClipDrag>,
    ruler_drag: Option<RulerDrag>,
    tracks_width: f32,
    tracks_expanded: bool,
    tracks_resize_start_width: Option<f32>,
}

impl Default for TimelineState {
    fn default() -> Self {
        Self {
            pixels_per_beat: 32.0,
            dragging_asset: None,
            clip_drag: None,
            ruler_drag: None,
            tracks_width: TRACKS_DEFAULT_WIDTH,
            tracks_expanded: true,
            tracks_resize_start_width: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipDragKind {
    Move,
    ResizeLeft,
    ResizeRight,
}

#[derive(Debug)]
struct ClipDrag {
    clip_id: String,
    track: usize,
    clip: usize,
    original_start: f32,
    original_length: f32,
    pointer_start: Pos2,
    kind: ClipDragKind,
    event_clip: bool,
    start: f32,
    length: f32,
    target_track: usize,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum RulerDragKind {
    Range,
    Start,
    End,
    Move,
}

#[derive(Clone, Copy, Debug)]
struct RulerDrag {
    kind: RulerDragKind,
    anchor: f32,
    current: f32,
    original_start: f32,
    original_end: f32,
}

impl TimelineState {
    pub fn zoom_by(&mut self, amount: f32) {
        self.pixels_per_beat =
            (self.pixels_per_beat * amount).clamp(MIN_PIXELS_PER_BEAT, MAX_PIXELS_PER_BEAT);
    }
}

fn zoomed_scroll_offset(
    scroll_offset: f32,
    pointer_from_left: f32,
    old_pixels_per_beat: f32,
    new_pixels_per_beat: f32,
) -> f32 {
    let beat_under_pointer = (scroll_offset + pointer_from_left) / old_pixels_per_beat;
    (beat_under_pointer * new_pixels_per_beat - pointer_from_left).max(0.0)
}

fn timeline_zoom_factor(input_zoom_factor: f32) -> f32 {
    input_zoom_factor.clamp(0.75, 1.25)
}

fn horizontal_timeline_scroll(delta: Vec2) -> Vec2 {
    // egui subtracts wheel deltas from the scroll offset. Negating the
    // vertical component therefore makes wheel-up travel right/later.
    Vec2::new(delta.x - delta.y, 0.0)
}

fn arrangement_scroll_id(ui: &Ui) -> Id {
    // ScrollArea::id_salt first turns the salt into an Id, then hashes that Id
    // through the parent Ui. Mirror both steps when accessing its state.
    ui.make_persistent_id(Id::new(ARRANGEMENT_SCROLL_SALT))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimelineTransform {
    pub origin_x: f32,
    pub pixels_per_beat: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TimelineSections {
    ruler: Rect,
    body: Rect,
    timeline: Rect,
}

fn timeline_sections(visible: Rect) -> TimelineSections {
    let ruler_bottom = (visible.top() + RULER_HEIGHT).min(visible.bottom());
    TimelineSections {
        ruler: Rect::from_min_max(visible.left_top(), Pos2::new(visible.right(), ruler_bottom)),
        body: Rect::from_min_max(
            Pos2::new(visible.left(), ruler_bottom),
            visible.right_bottom(),
        ),
        timeline: visible,
    }
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

fn arrangement_content_size(
    available: Vec2,
    composition_length: f32,
    track_count: usize,
    pixels_per_beat: f32,
) -> (Vec2, f32) {
    let display_length = composition_length.max(MIN_ARRANGEMENT_BEATS);
    let width = (display_length * pixels_per_beat + 120.0).max(available.x);
    let height = (RULER_HEIGHT + track_count as f32 * TRACK_HEIGHT).max(available.y);
    (Vec2::new(width, height), display_length)
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
    actions: &mut Vec<Intent>,
) {
    actions.clear();
    let composition = vm.current_composition();
    let time_signature = vm.transport.time_signature;
    let (workspace, _) = ui.allocate_exact_size(ui.available_size(), Sense::hover());
    let tracks_width = effective_tracks_width(state, workspace.width());
    let tracks_rect = Rect::from_min_max(
        workspace.left_top(),
        Pos2::new(workspace.left() + tracks_width, workspace.bottom()),
    );
    let timeline_rect = Rect::from_min_max(tracks_rect.right_top(), workspace.right_bottom());
    let mut scrolled_canvas_top = timeline_rect.top();
    let mut scrolled_viewport = Rect::from_min_size(Pos2::ZERO, timeline_rect.size());

    {
        let mut timeline_ui = ui.new_child(
            egui::UiBuilder::new()
                .id_salt("timeline_pane")
                .max_rect(timeline_rect),
        );
        let ui = &mut timeline_ui;
        let timeline_hovered = ui.rect_contains_pointer(timeline_rect);
        let zoom_modifier = ui.input(|input| input.modifiers.command || input.modifiers.ctrl);
        let input_zoom_factor = ui.input(egui::InputState::zoom_delta);
        if timeline_hovered && zoom_modifier && (input_zoom_factor - 1.0).abs() > f32::EPSILON {
            let old_pixels_per_beat = state.pixels_per_beat;
            state.zoom_by(timeline_zoom_factor(input_zoom_factor));
            if let Some(pointer) = ui.ctx().pointer_hover_pos() {
                let scroll_id = arrangement_scroll_id(ui);
                let mut scroll_state =
                    egui::scroll_area::State::load(ui.ctx(), scroll_id).unwrap_or_default();
                scroll_state.offset.x = zoomed_scroll_offset(
                    scroll_state.offset.x,
                    pointer.x - timeline_rect.left(),
                    old_pixels_per_beat,
                    state.pixels_per_beat,
                );
                scroll_state.store(ui.ctx(), scroll_id);
            }
        }
        if timeline_hovered && !zoom_modifier {
            ui.input_mut(|input| {
                input.smooth_scroll_delta = horizontal_timeline_scroll(input.smooth_scroll_delta);
            });
        }
        let (content_size, display_length) = arrangement_content_size(
            timeline_rect.size(),
            composition.length_beats,
            composition.tracks.len(),
            state.pixels_per_beat,
        );

        egui::ScrollArea::both()
            .id_salt(ARRANGEMENT_SCROLL_SALT)
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                let (canvas, canvas_response) =
                    ui.allocate_exact_size(content_size, Sense::click_and_drag());
                scrolled_canvas_top = canvas.top();
                scrolled_viewport = viewport;
                let painter = ui
                    .painter()
                    .with_clip_rect(canvas.intersect(ui.clip_rect()));
                // `viewport` is expressed in scrolling content coordinates. Fixed
                // chrome must instead use the ScrollArea's actual screen-space
                // clip rectangle; adding two independently rounded scrolling
                // coordinates can otherwise make it wobble by a pixel.
                let sections = timeline_sections(painter.clip_rect());
                let body_painter =
                    painter.with_clip_rect(sections.body.intersect(painter.clip_rect()));
                painter.rect_filled(canvas, 0.0, BG);
                let transform = TimelineTransform {
                    origin_x: canvas.left(),
                    pixels_per_beat: state.pixels_per_beat,
                };
                if let Some(pointer) = ui.ctx().pointer_interact_pos() {
                    update_clip_drag(
                        state,
                        pointer,
                        canvas,
                        transform,
                        composition.length_beats,
                        &composition.tracks,
                    );
                }
                let visible_start = transform.x_to_beat(sections.timeline.left()).max(0.0);
                let visible_end = transform
                    .x_to_beat(sections.timeline.right())
                    .min(display_length);
                paint_grid(
                    &body_painter,
                    canvas,
                    sections,
                    transform,
                    display_length,
                    time_signature,
                );
                paint_drop_guidance(
                    ui,
                    &body_painter,
                    sections.body,
                    transform,
                    composition.tracks.len(),
                    state.dragging_asset.is_some(),
                );

                let rows = visible_track_range(
                    viewport.top(),
                    viewport.bottom(),
                    composition.tracks.len(),
                );
                for track_index in rows {
                    let track = &composition.tracks[track_index];
                    let track_top = canvas.top() + RULER_HEIGHT + track_index as f32 * TRACK_HEIGHT;
                    let row_rect = Rect::from_min_max(
                        Pos2::new(canvas.left(), track_top),
                        Pos2::new(canvas.right(), track_top + TRACK_HEIGHT),
                    );
                    body_painter.hline(
                        row_rect.x_range(),
                        row_rect.bottom(),
                        Stroke::new(1.0_f32, GRID),
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
                        let (display_start, display_length, display_track) = state
                            .clip_drag
                            .as_ref()
                            .filter(|drag| drag.clip_id == clip.id)
                            .map_or((clip.start, clip.length, track_index), |drag| {
                                (drag.start, drag.length, drag.target_track)
                            });
                        let display_top =
                            canvas.top() + RULER_HEIGHT + display_track as f32 * TRACK_HEIGHT;
                        let clip_rect = Rect::from_min_max(
                            Pos2::new(transform.beat_to_x(display_start), display_top + 8.0),
                            Pos2::new(
                                transform.beat_to_x(display_start + display_length),
                                display_top + TRACK_HEIGHT - 8.0,
                            ),
                        );
                        let waveform_rect = state
                            .clip_drag
                            .as_ref()
                            .filter(|drag| drag.clip_id == clip.id)
                            .map_or(clip_rect, |drag| waveform_preview_rect(clip_rect, drag));
                        paint_clip(
                            ui,
                            &body_painter,
                            vm,
                            state,
                            clip,
                            clip_rect,
                            waveform_rect,
                            track_index,
                            clip_index,
                            now,
                            actions,
                        );
                    }
                }

                paint_sticky_headers(
                    ui,
                    &painter,
                    vm,
                    sections,
                    transform,
                    composition.length_beats,
                    display_length,
                    time_signature,
                    state,
                    actions,
                );
                paint_playhead(&painter, canvas, sections, transform, vm.transport.playhead);
                handle_canvas_interaction(
                    &canvas_response,
                    canvas,
                    sections,
                    transform,
                    composition.length_beats,
                    display_length,
                    composition.tracks.len(),
                    state,
                    actions,
                );
                if ui.input(|input| input.pointer.any_released())
                    && let Some(drag) = state.clip_drag.take()
                {
                    actions.push(Intent::EditClip {
                        track: drag.track,
                        clip: drag.clip,
                        start: drag.start,
                        length: drag.length,
                        target_track: drag.target_track,
                    });
                }
            });
    }

    paint_tracks_pane(
        ui,
        vm,
        state,
        tracks_rect,
        scrolled_canvas_top,
        scrolled_viewport,
        actions,
    );
    handle_tracks_resize(ui, state, workspace, tracks_rect);
}

fn effective_tracks_width(state: &mut TimelineState, available_width: f32) -> f32 {
    let max_width = (available_width - TIMELINE_MIN_WIDTH).max(TRACKS_COLLAPSED_WIDTH);
    if state.tracks_expanded && max_width >= TRACKS_MIN_WIDTH {
        state.tracks_width = state.tracks_width.clamp(TRACKS_MIN_WIDTH, max_width);
        state.tracks_width
    } else {
        if max_width < TRACKS_MIN_WIDTH {
            state.tracks_expanded = false;
        }
        TRACKS_COLLAPSED_WIDTH.min(available_width)
    }
}

fn paint_tracks_pane(
    ui: &mut Ui,
    vm: &DemoViewModel,
    state: &mut TimelineState,
    pane: Rect,
    canvas_top: f32,
    viewport: Rect,
    actions: &mut Vec<Intent>,
) {
    let painter = ui.painter().with_clip_rect(pane.intersect(ui.clip_rect()));
    painter.rect_filled(pane, CornerRadius::ZERO, PANEL);
    painter.vline(pane.right(), pane.y_range(), Stroke::new(1.0, GRID));

    if !state.tracks_expanded {
        let reopen = pane.shrink2(Vec2::new(TRACKS_RESIZE_HANDLE_WIDTH, 0.0));
        if ui
            .interact(reopen, Id::new("reopen_tracks"), Sense::click())
            .clicked()
        {
            state.tracks_expanded = true;
        }
        painter.rect_filled(
            Rect::from_min_max(
                pane.left_top(),
                Pos2::new(pane.right(), pane.top() + RULER_HEIGHT),
            ),
            CornerRadius::ZERO,
            PANEL_ALT,
        );
        painter.text(
            Pos2::new(pane.center().x, pane.top() + RULER_HEIGHT * 0.5),
            Align2::CENTER_CENTER,
            "›",
            FontId::monospace(13.0),
            TEXT_DIM,
        );
        painter.text(
            Pos2::new(pane.center().x, pane.top() + RULER_HEIGHT + 14.0),
            Align2::CENTER_TOP,
            "T",
            FontId::monospace(9.0),
            TEXT_DIM,
        );
        return;
    }

    let rows = visible_track_range(
        viewport.top(),
        viewport.bottom(),
        vm.current_composition().tracks.len(),
    );
    for track_index in rows {
        let track = &vm.current_composition().tracks[track_index];
        let top = canvas_top + RULER_HEIGHT + track_index as f32 * TRACK_HEIGHT;
        let header = Rect::from_min_size(
            Pos2::new(pane.left(), top),
            Vec2::new(pane.width(), TRACK_HEIGHT),
        );
        painter.rect_filled(header, CornerRadius::ZERO, PANEL);
        painter.hline(header.x_range(), header.bottom(), Stroke::new(1.0, GRID));
        painter.text(
            header.left_top() + Vec2::new(12.0, 13.0),
            Align2::LEFT_TOP,
            &track.name,
            FontId::proportional(11.0),
            TEXT,
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
        painter.rect_filled(meter, CornerRadius::ZERO, GRID);
        let level_height = meter.height() * track.level.clamp(0.0, 1.0);
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(meter.left(), meter.bottom() - level_height),
                meter.right_bottom(),
            ),
            CornerRadius::ZERO,
            ACCENT.gamma_multiply(0.8),
        );
        paint_toggle(&painter, mute_rect, "M", track.muted, STATUS_ERROR);
        paint_toggle(&painter, solo_rect, "S", track.solo, STATUS_NOTICE);
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

    let corner = Rect::from_min_size(pane.left_top(), Vec2::new(pane.width(), RULER_HEIGHT));
    painter.rect_filled(corner, CornerRadius::ZERO, PANEL_ALT);
    painter.hline(corner.x_range(), corner.bottom(), Stroke::new(1.0, GRID));
    painter.text(
        corner.left_center() + Vec2::new(12.0, 0.0),
        Align2::LEFT_CENTER,
        "TRACKS",
        FontId::monospace(9.0),
        TEXT_DIM,
    );
}

fn handle_tracks_resize(ui: &mut Ui, state: &mut TimelineState, workspace: Rect, pane: Rect) {
    let handle = Rect::from_center_size(
        Pos2::new(pane.right(), pane.center().y),
        Vec2::new(TRACKS_RESIZE_HANDLE_WIDTH, pane.height()),
    )
    .intersect(workspace);
    let response = ui
        .interact(handle, Id::new("tracks_resize"), Sense::click_and_drag())
        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    if response.double_clicked() {
        state.tracks_expanded = !state.tracks_expanded;
        state.tracks_resize_start_width = None;
        return;
    }
    if response.drag_started() {
        state.tracks_resize_start_width = Some(if state.tracks_expanded {
            state.tracks_width
        } else {
            TRACKS_COLLAPSED_WIDTH
        });
    }
    if response.dragged()
        && let Some(start_width) = state.tracks_resize_start_width
        && let Some(total_delta) = response.total_drag_delta()
    {
        let max_width = (workspace.width() - TIMELINE_MIN_WIDTH).max(TRACKS_COLLAPSED_WIDTH);
        if let Some(width) = resized_tracks_width(start_width, total_delta.x, max_width) {
            state.tracks_expanded = true;
            state.tracks_width = width;
        } else {
            state.tracks_expanded = false;
        }
    }
    if response.drag_stopped() {
        state.tracks_resize_start_width = None;
    }
}

fn resized_tracks_width(start_width: f32, total_drag_delta: f32, max_width: f32) -> Option<f32> {
    let proposed = start_width + total_drag_delta;
    (proposed >= TRACKS_MIN_WIDTH && max_width >= TRACKS_MIN_WIDTH)
        .then(|| proposed.clamp(TRACKS_MIN_WIDTH, max_width))
}

fn paint_drop_guidance(
    ui: &Ui,
    painter: &egui::Painter,
    body: Rect,
    transform: TimelineTransform,
    track_count: usize,
    dragging: bool,
) {
    let body = body.intersect(painter.clip_rect());
    if !body.is_positive() {
        return;
    }
    if dragging {
        painter.rect_filled(body, CornerRadius::ZERO, ACCENT.gamma_multiply(0.055));
        painter.rect_stroke(
            body.shrink(1.0),
            CornerRadius::ZERO,
            Stroke::new(1.0_f32, ACCENT.gamma_multiply(0.55)),
            StrokeKind::Inside,
        );
        if let Some(pointer) = ui.ctx().input(|input| input.pointer.latest_pos())
            && body.contains(pointer)
        {
            let x = transform.beat_to_x(snap_beat(transform.x_to_beat(pointer.x)));
            painter.vline(x, body.y_range(), Stroke::new(1.5_f32, ACCENT));
        }
    }
    if track_count == 0 {
        painter.text(
            body.center() + Vec2::new(0.0, -8.0),
            Align2::CENTER_CENTER,
            "EMPTY ARRANGEMENT",
            FontId::monospace(10.0),
            if dragging { TEXT } else { TEXT_DIM },
        );
        painter.text(
            body.center() + Vec2::new(0.0, 11.0),
            Align2::CENTER_CENTER,
            if dragging {
                "RELEASE TO CREATE AN AUDIO TRACK"
            } else {
                "DRAG AN AUDIO ASSET HERE"
            },
            FontId::monospace(9.0),
            TEXT_DIM,
        );
    }
}

fn paint_grid(
    painter: &egui::Painter,
    canvas: Rect,
    sections: TimelineSections,
    transform: TimelineTransform,
    composition_length: f32,
    time_signature: gaw_core::TimeSignature,
) {
    let visible_start = transform
        .x_to_beat(sections.timeline.left())
        .floor()
        .max(0.0);
    let visible_end = transform
        .x_to_beat(sections.timeline.right())
        .ceil()
        .min(composition_length);
    let lod = GridLod::new(transform.pixels_per_beat, time_signature);

    // Paint fine divisions first so their parent beat and bar lines remain crisp.
    // Visual strength is determined by on-screen spacing, not semantic depth:
    // a quarter beat at high zoom should look like a bar at the same pixel scale.
    for depth in (0..=lod.deepest_subdivision).rev() {
        let spacing = lod.meter_unit / f32::from(1_u16 << depth);
        let opacity = grid_line_opacity(spacing * transform.pixels_per_beat);
        if opacity <= 0.0 {
            continue;
        }
        let (start, end) = indexed_line_range(visible_start, visible_end, spacing);
        let ticks_per_bar = u64::from(time_signature.numerator) << depth;
        for line in start..=end {
            // Deeper layers only contribute the lines absent from their parent.
            if (depth > 0 && line.is_multiple_of(2)) || line.is_multiple_of(ticks_per_bar) {
                continue;
            }
            painter.vline(
                transform.beat_to_x(line as f32 * spacing),
                canvas.y_range(),
                Stroke::new(
                    grid_line_width(spacing * transform.pixels_per_beat),
                    GRID.gamma_multiply(opacity),
                ),
            );
        }
    }

    let mut stride = 1_u32;
    while stride < lod.bar_stride {
        let spacing = lod.bar_length * stride as f32;
        let pixel_spacing = spacing * transform.pixels_per_beat;
        let opacity = grid_line_opacity(pixel_spacing);
        if opacity > 0.0 {
            let (start, end) = indexed_line_range(visible_start, visible_end, spacing);
            for bar in start..=end {
                if !bar.is_multiple_of(2) {
                    painter.vline(
                        transform.beat_to_x(bar as f32 * spacing),
                        canvas.y_range(),
                        Stroke::new(grid_line_width(pixel_spacing), GRID.gamma_multiply(opacity)),
                    );
                }
            }
        }
        stride *= 2;
    }

    let major_spacing = lod.bar_length * lod.bar_stride as f32;
    let major_pixels = major_spacing * transform.pixels_per_beat;
    let (start, end) = indexed_line_range(visible_start, visible_end, major_spacing);
    for bar in start..=end {
        painter.vline(
            transform.beat_to_x(bar as f32 * major_spacing),
            canvas.y_range(),
            Stroke::new(
                (grid_line_width(major_pixels) + 0.3).min(1.3),
                GRID.gamma_multiply((grid_line_opacity(major_pixels) + 0.12).min(1.0)),
            ),
        );
    }
}

fn begin_clip_drag(
    state: &mut TimelineState,
    response: &Response,
    clip: &Clip,
    track: usize,
    clip_index: usize,
    kind: ClipDragKind,
) {
    if response.drag_started()
        && let Some(pointer_start) = response.interact_pointer_pos()
    {
        state.clip_drag = Some(ClipDrag {
            clip_id: clip.id.clone(),
            track,
            clip: clip_index,
            original_start: clip.start,
            original_length: clip.length,
            pointer_start,
            kind,
            event_clip: matches!(clip.kind, ClipKind::Event { .. }),
            start: clip.start,
            length: clip.length,
            target_track: track,
        });
    }
}

fn update_clip_drag(
    state: &mut TimelineState,
    pointer: Pos2,
    canvas: Rect,
    transform: TimelineTransform,
    composition_length: f32,
    tracks: &[crate::model::Track],
) {
    let Some(drag) = &mut state.clip_drag else {
        return;
    };
    let delta = (pointer.x - drag.pointer_start.x) / transform.pixels_per_beat;
    (drag.start, drag.length) = edit_clip_bounds(
        drag.kind,
        drag.original_start,
        drag.original_length,
        delta,
        composition_length,
    );
    if drag.kind != ClipDragKind::Move {
        drag.target_track = drag.track;
        return;
    }
    let Some(target) = track_at_y(pointer.y, canvas.top(), tracks.len()) else {
        return;
    };
    if clip_can_target(drag.event_clip, tracks[target].kind) {
        drag.target_track = target;
    }
}

fn clip_can_target(event_clip: bool, target: TrackKind) -> bool {
    (target == TrackKind::Event) == event_clip
}

fn edit_clip_bounds(
    kind: ClipDragKind,
    original_start: f32,
    original_length: f32,
    delta: f32,
    composition_length: f32,
) -> (f32, f32) {
    let original_end = original_start + original_length;
    match kind {
        ClipDragKind::Move => {
            let max_start = (composition_length - original_length).max(0.0);
            (
                (original_start + delta).clamp(0.0, max_start),
                original_length.min(composition_length),
            )
        }
        ClipDragKind::ResizeLeft => {
            let max_start = (original_end - MIN_CLIP_BEATS).max(0.0);
            let start = (original_start + delta).clamp(0.0, max_start);
            (start, original_end - start)
        }
        ClipDragKind::ResizeRight => {
            let min_end = original_start + MIN_CLIP_BEATS;
            let end =
                (original_end + delta).clamp(min_end.min(composition_length), composition_length);
            (original_start, end - original_start)
        }
    }
}

fn snap_beat(beat: f32) -> f32 {
    (beat / SNAP_BEATS).round() * SNAP_BEATS
}

fn waveform_preview_rect(clip_rect: Rect, drag: &ClipDrag) -> Rect {
    if drag.kind == ClipDragKind::Move || drag.length <= f32::EPSILON {
        return clip_rect;
    }
    let pixels_per_beat = clip_rect.width() / drag.length;
    let original_left = clip_rect.left() + (drag.original_start - drag.start) * pixels_per_beat;
    Rect::from_min_max(
        Pos2::new(original_left, clip_rect.top()),
        Pos2::new(
            original_left + drag.original_length * pixels_per_beat,
            clip_rect.bottom(),
        ),
    )
}

fn preview_loop_range(
    vm: &DemoViewModel,
    state: &TimelineState,
    composition_length: f32,
) -> (f32, f32) {
    state.ruler_drag.map_or_else(
        || {
            normalized_loop_range(
                vm.transport.loop_start,
                vm.transport.loop_end,
                composition_length,
            )
        },
        |drag| ruler_drag_range(drag, composition_length),
    )
}

fn ruler_drag_range(drag: RulerDrag, composition_length: f32) -> (f32, f32) {
    match drag.kind {
        RulerDragKind::Range => {
            normalized_loop_range(drag.anchor, drag.current, composition_length)
        }
        RulerDragKind::Start => {
            normalized_loop_range(drag.current, drag.anchor, composition_length)
        }
        RulerDragKind::End => normalized_loop_range(drag.anchor, drag.current, composition_length),
        RulerDragKind::Move => moved_loop_range(
            drag.original_start,
            drag.original_end,
            drag.current - drag.anchor,
            composition_length,
        ),
    }
}

fn moved_loop_range(
    original_start: f32,
    original_end: f32,
    delta: f32,
    composition_length: f32,
) -> (f32, f32) {
    let length = (original_end - original_start)
        .max(0.0)
        .min(composition_length);
    let start = (original_start + delta).clamp(0.0, composition_length - length);
    (start, start + length)
}

fn ruler_body_drag_kind(ctrl: bool, beat: f32, loop_range: (f32, f32)) -> RulerDragKind {
    if ctrl && (loop_range.0..=loop_range.1).contains(&beat) {
        RulerDragKind::Move
    } else {
        RulerDragKind::Range
    }
}

fn finish_ruler_drag(drag: RulerDrag, composition_length: f32) -> Intent {
    let (start, end) = ruler_drag_range(drag, composition_length);
    Intent::SetLoopRange { start, end }
}

fn normalized_loop_range(first: f32, second: f32, composition_length: f32) -> (f32, f32) {
    let mut start = first.min(second).clamp(0.0, composition_length);
    let mut end = first.max(second).clamp(0.0, composition_length);
    if end - start < SNAP_BEATS {
        if start + SNAP_BEATS <= composition_length {
            end = start + SNAP_BEATS;
        } else {
            start = (end - SNAP_BEATS).max(0.0);
        }
    }
    (start, end)
}

fn track_at_y(y: f32, canvas_top: f32, track_count: usize) -> Option<usize> {
    let relative = y - canvas_top - RULER_HEIGHT;
    if relative < 0.0 || track_count == 0 {
        return None;
    }
    let track = (relative / TRACK_HEIGHT).floor() as usize;
    (track < track_count).then_some(track)
}

#[allow(clippy::too_many_arguments)]
fn paint_clip(
    ui: &mut Ui,
    painter: &egui::Painter,
    vm: &DemoViewModel,
    state: &mut TimelineState,
    clip: &Clip,
    rect: Rect,
    waveform_rect: Rect,
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
    let body_clip = painter.clip_rect();
    let clip_painter = painter.with_clip_rect(visual_rect.intersect(body_clip));
    let painter = &clip_painter;
    let content_painter = painter.with_clip_rect(rect.intersect(body_clip));
    let selected = matches!(vm.selection, Selection::Clip { track, clip } | Selection::Effect { track, clip, .. } if track == track_index && clip == clip_index);
    let color = match clip.kind {
        ClipKind::Audio { .. } => AUDIO,
        ClipKind::Event { .. } => EVENT,
        ClipKind::Composition { .. } => NESTED,
    };
    let fill = color.gamma_multiply(if selected { 0.43 } else { 0.29 });
    painter.rect_filled(rect, CornerRadius::ZERO, fill);
    painter.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(
            if selected { 2.0_f32 } else { 1.0_f32 },
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
            &content_painter,
            waveform_rect.shrink2(Vec2::new(4.0, 18.0)),
            &clip.waveform,
            color,
        ),
        ClipKind::Event { notes } => paint_notes(
            &content_painter,
            waveform_rect.shrink2(Vec2::new(4.0, 17.0)),
            notes,
            clip.length,
            color,
        ),
        ClipKind::Composition { .. } => {
            paint_waveform(
                &content_painter,
                waveform_rect.shrink2(Vec2::new(6.0, 18.0)),
                &clip.waveform,
                color,
            );
            painter.rect_stroke(
                rect.shrink(3.0),
                CornerRadius::ZERO,
                Stroke::new(1.0_f32, color.gamma_multiply(0.7)),
                StrokeKind::Inside,
            );
            let tail = Rect::from_min_max(
                rect.right_top(),
                Pos2::new(rect.right() + tail_width, rect.bottom()),
            );
            let tail_painter = painter.with_clip_rect(tail.intersect(painter.clip_rect()));
            tail_painter.rect_filled(tail, CornerRadius::ZERO, color.gamma_multiply(0.16));
            tail_painter.rect_stroke(
                tail,
                CornerRadius::ZERO,
                Stroke::new(1.0_f32, color.gamma_multiply(0.55)),
                StrokeKind::Inside,
            );
            let mut x = tail.left() - tail.height();
            while x < tail.right() {
                tail_painter.line_segment(
                    [
                        Pos2::new(x, tail.bottom()),
                        Pos2::new(x + tail.height(), tail.top()),
                    ],
                    Stroke::new(1.0_f32, color.gamma_multiply(0.55)),
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
            CornerRadius::ZERO,
            Stroke::new(
                2.0_f32,
                Color32::from_rgba_unmultiplied(238, 238, 238, (agent_alpha * 230.0) as u8),
            ),
            StrokeKind::Outside,
        );
    }

    let interaction_rect = rect.intersect(body_clip);
    if !interaction_rect.is_positive() {
        return;
    }
    let handle_width = RESIZE_HANDLE_WIDTH.min(interaction_rect.width() / 3.0);
    let left_handle = Rect::from_min_max(
        interaction_rect.left_top(),
        Pos2::new(
            interaction_rect.left() + handle_width,
            interaction_rect.bottom(),
        ),
    );
    let right_handle = Rect::from_min_max(
        Pos2::new(
            interaction_rect.right() - handle_width,
            interaction_rect.top(),
        ),
        interaction_rect.right_bottom(),
    );
    let body = Rect::from_min_max(
        Pos2::new(left_handle.right(), interaction_rect.top()),
        Pos2::new(right_handle.left(), interaction_rect.bottom()),
    );
    let left_response = ui.interact(
        left_handle,
        Id::new(("clip_resize_left", &clip.id)),
        Sense::drag(),
    );
    let right_response = ui.interact(
        right_handle,
        Id::new(("clip_resize_right", &clip.id)),
        Sense::drag(),
    );
    let response = ui.interact(body, Id::new(("clip", &clip.id)), Sense::click_and_drag());
    begin_clip_drag(
        state,
        &left_response,
        clip,
        track_index,
        clip_index,
        ClipDragKind::ResizeLeft,
    );
    begin_clip_drag(
        state,
        &right_response,
        clip,
        track_index,
        clip_index,
        ClipDragKind::ResizeRight,
    );
    begin_clip_drag(
        state,
        &response,
        clip,
        track_index,
        clip_index,
        ClipDragKind::Move,
    );
    if response.clicked() {
        response.request_focus();
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
    if response.has_focus()
        && ui.input_mut(|input| {
            input.consume_key(egui::Modifiers::NONE, egui::Key::Delete)
                || input.consume_key(egui::Modifiers::NONE, egui::Key::Backspace)
        })
    {
        actions.push(Intent::DeleteClip {
            track: track_index,
            clip: clip_index,
        });
    }
    response.context_menu(|ui| {
        if ui.button("Delete clip").clicked() {
            actions.push(Intent::DeleteClip {
                track: track_index,
                clip: clip_index,
            });
            ui.close();
        }
    });
    left_response.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    right_response.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    response.on_hover_text(match clip.kind {
        ClipKind::Composition { .. } => "Double-click to enter composition",
        _ => "Drag to move · drag edges to resize",
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
                TEXT,
            );
        }
        ClipKind::Composition {
            render: RenderState::Stale,
            ..
        } => badge(painter, rect, "STALE", STATUS_ERROR),
        ClipKind::Composition {
            render: RenderState::Rendering(progress),
            ..
        } => {
            badge(painter, rect, &format!("RENDER {progress}%"), STATUS_NOTICE);
        }
        _ => {}
    }
}

fn badge(painter: &egui::Painter, rect: Rect, text: &str, color: Color32) {
    let badge_rect = Rect::from_min_size(
        rect.right_top() + Vec2::new(-75.0, 5.0),
        Vec2::new(69.0, 16.0),
    );
    painter.rect_filled(badge_rect, CornerRadius::ZERO, color.gamma_multiply(0.38));
    painter.text(
        badge_rect.center(),
        Align2::CENTER_CENTER,
        text,
        FontId::monospace(8.5),
        Color32::WHITE,
    );
}

pub fn paint_waveform(
    painter: &egui::Painter,
    rect: Rect,
    waveform: &[WaveformPoint],
    color: Color32,
) {
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
        let start_phase = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let end_phase = ((x + 1.0 - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let peak = waveform_peak(waveform, start_phase, end_phase);
        let scale = rect.height() * 0.45;
        painter.line_segment(
            [
                Pos2::new(x, center - peak.maximum.clamp(-1.0, 1.0) * scale),
                Pos2::new(x, center - peak.minimum.clamp(-1.0, 1.0) * scale),
            ],
            Stroke::new(1.0_f32, color.gamma_multiply(0.9)),
        );
        x += 1.0;
    }
}

fn waveform_peak(waveform: &[WaveformPoint], start_phase: f32, end_phase: f32) -> WaveformPoint {
    let start = (start_phase * waveform.len() as f32).floor() as usize;
    let end = ((end_phase * waveform.len() as f32).ceil() as usize)
        .max(start.saturating_add(1))
        .min(waveform.len());
    waveform[start.min(waveform.len() - 1)..end]
        .iter()
        .copied()
        .fold(
            WaveformPoint {
                minimum: f32::INFINITY,
                maximum: f32::NEG_INFINITY,
            },
            |peak, point| WaveformPoint {
                minimum: peak.minimum.min(point.minimum),
                maximum: peak.maximum.max(point.maximum),
            },
        )
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
            CornerRadius::ZERO,
            color.gamma_multiply(0.65 + note.velocity * 0.3),
        );
    }
}

fn paint_playhead(
    painter: &egui::Painter,
    canvas: Rect,
    sections: TimelineSections,
    transform: TimelineTransform,
    beat: f32,
) {
    let painter = &painter.with_clip_rect(sections.timeline.intersect(painter.clip_rect()));
    let x = transform.beat_to_x(beat);
    painter.vline(x, canvas.y_range(), Stroke::new(1.5_f32, PLAYHEAD));
    let y = sections.timeline.top();
    painter.rect_filled(
        Rect::from_center_size(Pos2::new(x, y + 3.0), Vec2::new(7.0, 7.0)),
        CornerRadius::ZERO,
        PLAYHEAD,
    );
}

#[allow(clippy::too_many_arguments)]
fn paint_sticky_headers(
    ui: &mut Ui,
    painter: &egui::Painter,
    vm: &DemoViewModel,
    sections: TimelineSections,
    transform: TimelineTransform,
    composition_length: f32,
    display_length: f32,
    time_signature: gaw_core::TimeSignature,
    state: &mut TimelineState,
    actions: &mut Vec<Intent>,
) {
    let timeline_painter = painter.with_clip_rect(sections.timeline.intersect(painter.clip_rect()));
    let ruler = sections.ruler;
    timeline_painter.rect_filled(ruler, 0.0, PANEL_ALT);
    timeline_painter.hline(ruler.x_range(), ruler.bottom(), Stroke::new(1.0_f32, GRID));
    if let Some(pointer) = ui.ctx().pointer_interact_pos()
        && let Some(drag) = &mut state.ruler_drag
    {
        drag.current = snap_beat(
            transform
                .x_to_beat(pointer.x)
                .clamp(0.0, composition_length),
        );
    }
    let loop_range = preview_loop_range(vm, state, composition_length);
    let loop_rect = Rect::from_min_max(
        Pos2::new(transform.beat_to_x(loop_range.0), ruler.top() + 2.0),
        Pos2::new(transform.beat_to_x(loop_range.1), ruler.bottom() - 2.0),
    )
    .intersect(ruler);
    let (loop_fill_alpha, loop_edge_alpha) = loop_visual_alpha(vm.transport.loop_enabled);
    timeline_painter.rect_filled(
        loop_rect,
        CornerRadius::ZERO,
        ACCENT.gamma_multiply(loop_fill_alpha),
    );
    timeline_painter.hline(
        loop_rect.x_range(),
        loop_rect.bottom(),
        Stroke::new(2.0_f32, ACCENT.gamma_multiply(loop_edge_alpha)),
    );
    let visible_start = transform
        .x_to_beat(sections.timeline.left())
        .floor()
        .max(0.0);
    let visible_end = transform
        .x_to_beat(sections.timeline.right())
        .ceil()
        .min(display_length);
    let lod = GridLod::new(transform.pixels_per_beat, time_signature);
    let label_spacing = lod.bar_length * lod.label_stride as f32;
    let (start, end) = indexed_line_range(visible_start, visible_end, label_spacing);
    for label in start..=end {
        timeline_painter.text(
            Pos2::new(
                transform.beat_to_x(label as f32 * label_spacing) + 5.0,
                ruler.center().y,
            ),
            Align2::LEFT_CENTER,
            label * u64::from(lod.label_stride) + 1,
            FontId::monospace(11.0),
            TEXT_DIM,
        );
    }
    let timeline_ruler = Rect::from_min_max(
        Pos2::new(
            sections.timeline.left().max(transform.beat_to_x(0.0)),
            ruler.top(),
        ),
        ruler.right_bottom(),
    );
    let ruler_response = ui.interact(
        timeline_ruler,
        Id::new(("timeline_ruler", &vm.current_composition().id)),
        Sense::click_and_drag(),
    );
    if ruler_response.secondary_clicked()
        && ruler_response
            .interact_pointer_pos()
            .is_some_and(|pointer| loop_rect.contains(pointer))
    {
        actions.push(Intent::ToggleLoop);
    }
    if ruler_response.clicked()
        && let Some(pointer) = ruler_response.interact_pointer_pos()
    {
        actions.push(Intent::Seek(
            snap_beat(transform.x_to_beat(pointer.x)).clamp(0.0, composition_length),
        ));
    }
    if ruler_response.drag_started_by(PointerButton::Primary)
        && let Some(pointer) = ruler_response.interact_pointer_pos()
    {
        let beat = snap_beat(
            transform
                .x_to_beat(pointer.x)
                .clamp(0.0, composition_length),
        );
        let kind = ruler_body_drag_kind(ui.input(|input| input.modifiers.ctrl), beat, loop_range);
        state.ruler_drag = Some(RulerDrag {
            kind,
            anchor: beat,
            current: beat,
            original_start: loop_range.0,
            original_end: loop_range.1,
        });
    }
    let (loop_start, loop_end) = loop_range;
    for (kind, beat, anchor) in [
        (RulerDragKind::Start, loop_start, loop_end),
        (RulerDragKind::End, loop_end, loop_start),
    ] {
        let handle = Rect::from_center_size(
            Pos2::new(transform.beat_to_x(beat), ruler.center().y),
            Vec2::new(9.0, ruler.height()),
        )
        .intersect(timeline_ruler);
        let response = ui.interact(
            handle,
            Id::new(("loop_handle", &vm.current_composition().id, kind)),
            Sense::drag(),
        );
        if response.drag_started_by(PointerButton::Primary) {
            state.ruler_drag = Some(RulerDrag {
                kind,
                anchor,
                current: beat,
                original_start: loop_start,
                original_end: loop_end,
            });
        }
        response.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    }
    let playhead_handle = Rect::from_center_size(
        Pos2::new(transform.beat_to_x(vm.transport.playhead), ruler.center().y),
        Vec2::new(11.0, ruler.height()),
    )
    .intersect(timeline_ruler);
    let playhead_response = ui.interact(
        playhead_handle,
        Id::new(("playhead_handle", &vm.current_composition().id)),
        Sense::drag(),
    );
    if playhead_response.drag_started() || playhead_response.dragged() {
        state.ruler_drag = None;
        if let Some(pointer) = playhead_response.interact_pointer_pos() {
            actions.push(Intent::Seek(
                snap_beat(transform.x_to_beat(pointer.x)).clamp(0.0, composition_length),
            ));
        }
    }
    playhead_response.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    if ui.input(|input| input.pointer.button_released(PointerButton::Primary))
        && let Some(drag) = state.ruler_drag.take()
    {
        actions.push(finish_ruler_drag(drag, composition_length));
    }
}

fn loop_visual_alpha(enabled: bool) -> (f32, f32) {
    if enabled { (0.18, 0.8) } else { (0.055, 0.22) }
}

fn meter_unit_beats(time_signature: gaw_core::TimeSignature) -> f32 {
    4.0 / f32::from(time_signature.denominator)
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct GridLod {
    meter_unit: f32,
    bar_length: f32,
    bar_stride: u32,
    label_stride: u32,
    deepest_subdivision: u8,
}

impl GridLod {
    fn new(pixels_per_beat: f32, time_signature: gaw_core::TimeSignature) -> Self {
        let meter_unit = meter_unit_beats(time_signature);
        let bar_length = meter_unit * f32::from(time_signature.numerator);
        let bar_pixels = bar_length * pixels_per_beat;
        let bar_stride = spacing_stride(bar_pixels, MAJOR_BAR_SPACING);
        let label_stride = spacing_stride(bar_pixels, MIN_RULER_LABEL_SPACING);
        let meter_unit_pixels = meter_unit * pixels_per_beat;
        let deepest_subdivision = (0..=MAX_SUBDIVISION_DEPTH)
            .take_while(|depth| {
                let spacing = meter_unit_pixels / f32::from(1_u16 << depth);
                grid_line_opacity(spacing) > 0.0
            })
            .last()
            .unwrap_or(0);
        Self {
            meter_unit,
            bar_length,
            bar_stride,
            label_stride,
            deepest_subdivision,
        }
    }
}

fn spacing_stride(spacing: f32, minimum: f32) -> u32 {
    if !spacing.is_finite() || spacing <= 0.0 {
        return 1;
    }
    let mut stride = 1_u32;
    while spacing * (stride as f32) < minimum && stride < (1 << 20) {
        stride *= 2;
    }
    stride
}

fn grid_line_opacity(pixel_spacing: f32) -> f32 {
    let t = ((pixel_spacing - MIN_GRID_SPACING) / (FULL_GRID_SPACING - MIN_GRID_SPACING))
        .clamp(0.0, 1.0);
    let smooth = t * t * (3.0 - 2.0 * t);
    // Once a layer is legible, its prominence grows with its screen-space
    // cadence. This creates the same hierarchy at every zoom level regardless
    // of whether the visible layers represent bars, beats, or fractions.
    let prominence =
        (0.32 + 0.18 * (pixel_spacing / FULL_GRID_SPACING).log2().max(0.0)).clamp(0.32, 0.82);
    smooth * prominence
}

fn grid_line_width(pixel_spacing: f32) -> f32 {
    (0.55 + 0.12 * (pixel_spacing / FULL_GRID_SPACING).log2().max(0.0)).clamp(0.55, 1.0)
}

fn indexed_line_range(visible_start: f32, visible_end: f32, spacing: f32) -> (u64, u64) {
    (
        (visible_start.max(0.0) / spacing).floor() as u64,
        (visible_end.max(0.0) / spacing).ceil() as u64,
    )
}

fn paint_toggle(painter: &egui::Painter, rect: Rect, text: &str, active: bool, color: Color32) {
    painter.rect_filled(
        rect,
        CornerRadius::ZERO,
        if active {
            color.gamma_multiply(0.55)
        } else {
            PANEL_RAISED
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
    sections: TimelineSections,
    transform: TimelineTransform,
    length: f32,
    display_length: f32,
    track_count: usize,
    state: &mut TimelineState,
    actions: &mut Vec<Intent>,
) {
    if response.clicked()
        && let Some(pointer) = response.interact_pointer_pos()
        && sections.timeline.contains(pointer)
    {
        actions.push(Intent::Seek(
            transform.x_to_beat(pointer.x).clamp(0.0, length),
        ));
    }
    let released = response.ctx.input(|input| input.pointer.any_released());
    if released
        && let Some(asset) = state.dragging_asset.take()
        && let Some(pointer) = response.ctx.input(|input| input.pointer.latest_pos())
        && response.rect.contains(pointer)
        && sections.body.contains(pointer)
    {
        actions.push(Intent::AddAssetClip {
            asset_id: asset,
            beat: snap_beat(transform.x_to_beat(pointer.x)).clamp(0.0, display_length),
            track: track_at_y(pointer.y, canvas.top(), track_count),
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
    fn ruler_and_body_partition_the_timeline() {
        let visible = Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 500.0));
        let sections = timeline_sections(visible);

        assert_eq!(sections.timeline, visible);
        assert_eq!(sections.ruler.x_range(), visible.x_range());
        assert_eq!(sections.body.x_range(), visible.x_range());
        assert!((sections.ruler.bottom() - RULER_HEIGHT).abs() < f32::EPSILON);
        assert!((sections.body.top() - RULER_HEIGHT).abs() < f32::EPSILON);
        assert!((sections.ruler.bottom() - sections.body.top()).abs() < f32::EPSILON);
    }

    #[test]
    fn fixed_sections_ignore_fractional_content_scroll_offsets() {
        let visible = Rect::from_min_size(Pos2::new(17.25, 31.5), Vec2::new(800.0, 500.0));
        let expected = timeline_sections(visible);

        for offset in [0.0, 0.125, 0.5, 73.375, 700.875] {
            let canvas = Rect::from_min_size(
                Pos2::new(visible.left() - offset, visible.top() - 144.625),
                Vec2::new(2_000.0, 900.0),
            );
            let viewport = Rect::from_min_size(Pos2::new(offset, 144.625), visible.size());

            // Canvas and viewport coordinates move with scrolling. The fixed
            // section geometry is derived only from the stable screen clip.
            assert!((canvas.left() - viewport.left()).abs() > f32::EPSILON);
            assert_eq!(timeline_sections(visible), expected);
        }
    }

    #[test]
    fn grid_geometry_follows_arbitrary_time_signatures() {
        let three_four = gaw_core::TimeSignature::new(3, 4).unwrap();
        let lod = GridLod::new(32.0, three_four);
        assert!((lod.meter_unit - 1.0).abs() < f32::EPSILON);
        assert!((lod.bar_length - 3.0).abs() < f32::EPSILON);

        let six_eight = gaw_core::TimeSignature::new(6, 8).unwrap();
        let lod = GridLod::new(32.0, six_eight);
        assert!((lod.meter_unit - 0.5).abs() < f32::EPSILON);
        assert!((lod.bar_length - 3.0).abs() < f32::EPSILON);

        let three_two = gaw_core::TimeSignature::new(3, 2).unwrap();
        let lod = GridLod::new(32.0, three_two);
        assert!((lod.meter_unit - 2.0).abs() < f32::EPSILON);
        assert!((lod.bar_length - 6.0).abs() < f32::EPSILON);
    }

    #[test]
    fn grid_lod_hides_beats_when_crowded_and_adds_fractions_when_zoomed() {
        let four_four = gaw_core::TimeSignature::new(4, 4).unwrap();
        let overview = GridLod::new(4.0, four_four);
        assert!(grid_line_opacity(overview.meter_unit * 4.0).abs() < f32::EPSILON);
        assert_eq!(overview.bar_stride, 4);

        let normal = GridLod::new(32.0, four_four);
        let detailed = GridLod::new(512.0, four_four);
        assert!(normal.deepest_subdivision >= 1);
        assert!(detailed.deepest_subdivision > normal.deepest_subdivision);
        assert!(detailed.deepest_subdivision >= 5);
    }

    #[test]
    fn grid_lod_opacity_transitions_smoothly_with_pixel_spacing() {
        let hidden = grid_line_opacity(MIN_GRID_SPACING);
        let entering = grid_line_opacity((MIN_GRID_SPACING + FULL_GRID_SPACING) * 0.5);
        let visible = grid_line_opacity(FULL_GRID_SPACING);
        assert!(hidden.abs() < f32::EPSILON);
        assert!(entering > hidden);
        assert!(visible > entering);
    }

    #[test]
    fn grid_strength_depends_on_screen_spacing_not_subdivision_depth() {
        let beat_at_overview = grid_line_opacity(16.0);
        let fraction_at_detail = grid_line_opacity(16.0);
        assert!((beat_at_overview - fraction_at_detail).abs() < f32::EPSILON);
        assert!(grid_line_opacity(64.0) > beat_at_overview);
    }

    #[test]
    fn overview_grid_promotes_power_of_two_bar_groups() {
        let four_four = gaw_core::TimeSignature::new(4, 4).unwrap();
        assert_eq!(GridLod::new(4.0, four_four).bar_stride, 4);
        assert_eq!(GridLod::new(8.0, four_four).bar_stride, 2);
        assert_eq!(GridLod::new(16.0, four_four).bar_stride, 1);

        let three_eight = gaw_core::TimeSignature::new(3, 8).unwrap();
        assert_eq!(GridLod::new(4.0, three_eight).bar_stride, 16);
    }

    #[test]
    fn ruler_labels_are_thinned_to_avoid_overlap() {
        let four_four = gaw_core::TimeSignature::new(4, 4).unwrap();
        let overview = GridLod::new(4.0, four_four);
        assert_eq!(overview.label_stride, 4);
        assert!(overview.bar_length * overview.label_stride as f32 * 4.0 >= 48.0);

        let detailed = GridLod::new(64.0, four_four);
        assert_eq!(detailed.label_stride, 1);
    }

    #[test]
    fn finest_grid_layer_keeps_visible_work_bounded() {
        for (scale, signature) in [
            (
                MIN_PIXELS_PER_BEAT,
                gaw_core::TimeSignature::new(1, 32).unwrap(),
            ),
            (32.0, gaw_core::TimeSignature::new(4, 4).unwrap()),
            (
                MAX_PIXELS_PER_BEAT,
                gaw_core::TimeSignature::new(12, 8).unwrap(),
            ),
        ] {
            let lod = GridLod::new(scale, signature);
            let viewport_beats = 1_000.0 / scale;
            let finest_spacing = lod.meter_unit / f32::from(1_u16 << lod.deepest_subdivision);
            if grid_line_opacity(finest_spacing * scale) <= f32::EPSILON {
                continue;
            }
            let (_, end) = indexed_line_range(0.0, viewport_beats, finest_spacing);
            assert!(end <= 160, "scale {scale} generated {end} finest lines");
        }
    }

    #[test]
    fn waveform_pixel_aggregation_preserves_signed_transients() {
        let waveform = [
            WaveformPoint {
                minimum: -0.1,
                maximum: 0.2,
            },
            WaveformPoint {
                minimum: -0.9,
                maximum: 0.3,
            },
            WaveformPoint {
                minimum: -0.2,
                maximum: 1.0,
            },
            WaveformPoint {
                minimum: -0.1,
                maximum: 0.1,
            },
        ];
        assert_eq!(
            waveform_peak(&waveform, 0.0, 1.0),
            WaveformPoint {
                minimum: -0.9,
                maximum: 1.0,
            }
        );
        assert_eq!(waveform_peak(&waveform, 0.0, 0.25), waveform[0]);
    }

    #[test]
    fn visible_tracks_are_clamped_and_virtualized() {
        assert_eq!(visible_track_range(0.0, 200.0, 100), 0..3);
        assert_eq!(visible_track_range(500.0, 700.0, 100), 6..10);
        assert_eq!(visible_track_range(8_500.0, 9_000.0, 10), 10..10);
        assert_eq!(visible_track_range(0.0, 200.0, 0), 0..0);
    }

    #[test]
    fn empty_arrangement_fills_the_viewport_and_exposes_a_working_range() {
        let available = Vec2::new(460.0, 620.0);
        let (content, display_length) = arrangement_content_size(available, 0.0, 0, 32.0);
        assert!(content.x >= available.x);
        assert!((content.y - available.y).abs() < f32::EPSILON);
        assert!((display_length - MIN_ARRANGEMENT_BEATS).abs() < f32::EPSILON);
        assert!(content.x > 120.0);
    }

    #[test]
    fn tracks_column_preserves_a_minimum_timeline_width() {
        let mut state = TimelineState {
            tracks_width: 500.0,
            ..TimelineState::default()
        };
        let width = effective_tracks_width(&mut state, 700.0);
        assert!((width - 380.0).abs() < f32::EPSILON);
        assert!((700.0 - width - TIMELINE_MIN_WIDTH).abs() < f32::EPSILON);
    }

    #[test]
    fn tracks_column_collapses_when_the_workspace_cannot_fit_both_minima() {
        let mut state = TimelineState::default();
        let width = effective_tracks_width(&mut state, TIMELINE_MIN_WIDTH + TRACKS_MIN_WIDTH - 1.0);
        assert!((width - TRACKS_COLLAPSED_WIDTH).abs() < f32::EPSILON);
        assert!(!state.tracks_expanded);
    }

    #[test]
    fn tracks_resize_uses_total_pointer_travel_from_the_drag_origin() {
        assert_eq!(resized_tracks_width(160.0, 24.0, 300.0), Some(184.0));
        assert_eq!(resized_tracks_width(160.0, -20.0, 300.0), Some(140.0));
        assert_eq!(resized_tracks_width(160.0, -40.0, 300.0), None);
        assert_eq!(resized_tracks_width(160.0, 200.0, 300.0), Some(300.0));
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

    #[test]
    fn zoom_offset_keeps_the_beat_under_the_pointer() {
        let old_offset = 240.0;
        let pointer = 360.0;
        let old_scale = 32.0;
        let new_scale = 48.0;
        let beat = (old_offset + pointer) / old_scale;
        let new_offset = zoomed_scroll_offset(old_offset, pointer, old_scale, new_scale);
        let anchored_beat = (new_offset + pointer) / new_scale;
        assert!((beat - anchored_beat).abs() < f32::EPSILON);
    }

    #[test]
    fn zoom_offset_never_scrolls_before_the_timeline() {
        assert!(zoomed_scroll_offset(0.0, 0.0, 32.0, 96.0).abs() < f32::EPSILON);
        assert!(zoomed_scroll_offset(0.0, 20.0, 96.0, 32.0) >= 0.0);
    }

    #[test]
    fn arrangement_state_id_matches_the_scroll_area_id() {
        let context = egui::Context::default();
        let mut predicted = None;
        let mut actual = None;
        let _ = context.run_ui(egui::RawInput::default(), |ui| {
            predicted = Some(arrangement_scroll_id(ui));
            actual = Some(
                egui::ScrollArea::both()
                    .id_salt(ARRANGEMENT_SCROLL_SALT)
                    .show(ui, |_| {})
                    .id,
            );
        });
        assert_eq!(predicted, actual);
    }

    #[test]
    fn vertical_wheel_is_remapped_to_the_requested_timeline_direction() {
        assert_eq!(
            horizontal_timeline_scroll(Vec2::new(0.0, 12.0)),
            Vec2::new(-12.0, 0.0)
        );
        assert_eq!(
            horizontal_timeline_scroll(Vec2::new(0.0, -12.0)),
            Vec2::new(12.0, 0.0)
        );
    }

    #[test]
    fn horizontal_trackpad_input_is_preserved_while_vertical_input_is_remapped() {
        assert_eq!(
            horizontal_timeline_scroll(Vec2::new(5.0, 12.0)),
            Vec2::new(-7.0, 0.0)
        );
    }

    #[test]
    fn input_zoom_factor_preserves_wheel_direction_and_is_bounded() {
        assert!(timeline_zoom_factor(1.1) > 1.0);
        assert!(timeline_zoom_factor(0.9) < 1.0);
        assert!((timeline_zoom_factor(10.0) - 1.25).abs() < f32::EPSILON);
        assert!((timeline_zoom_factor(0.01) - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn beat_snapping_is_quarter_beat_and_deterministic() {
        assert!((snap_beat(1.12) - 1.0).abs() < f32::EPSILON);
        assert!((snap_beat(1.13) - 1.25).abs() < f32::EPSILON);
        assert!((snap_beat(7.875) - 8.0).abs() < f32::EPSILON);
    }

    #[test]
    fn move_and_resize_bounds_are_continuous_and_clamped() {
        assert_eq!(
            edit_clip_bounds(ClipDragKind::Move, 2.0, 4.0, 1.13, 12.0),
            (3.13, 4.0)
        );
        assert_eq!(
            edit_clip_bounds(ClipDragKind::Move, 2.0, 4.0, 99.0, 12.0),
            (8.0, 4.0)
        );
        assert_eq!(
            edit_clip_bounds(ClipDragKind::ResizeLeft, 2.0, 4.0, 1.13, 12.0),
            (3.13, 2.87)
        );
        assert_eq!(
            edit_clip_bounds(ClipDragKind::ResizeLeft, 2.0, 4.0, -99.0, 12.0),
            (0.0, 6.0)
        );
        assert_eq!(
            edit_clip_bounds(ClipDragKind::ResizeRight, 2.0, 4.0, -99.0, 12.0),
            (2.0, MIN_CLIP_BEATS)
        );
        assert_eq!(
            edit_clip_bounds(ClipDragKind::ResizeRight, 2.0, 4.0, 99.0, 12.0),
            (2.0, 10.0)
        );
    }

    #[test]
    fn clip_drag_preserves_sub_snap_precision_at_fine_zoom() {
        let delta = 1.0 / MAX_PIXELS_PER_BEAT;
        let (move_start, move_length) = edit_clip_bounds(ClipDragKind::Move, 2.0, 4.0, delta, 12.0);
        assert!((move_start - (2.0 + delta)).abs() <= f32::EPSILON);
        assert!((move_length - 4.0).abs() <= f32::EPSILON);

        let (left_start, left_length) =
            edit_clip_bounds(ClipDragKind::ResizeLeft, 2.0, 4.0, delta, 12.0);
        assert!((left_start - (2.0 + delta)).abs() <= f32::EPSILON);
        assert!((left_start + left_length - 6.0).abs() <= f32::EPSILON * 4.0);

        let (right_start, right_length) =
            edit_clip_bounds(ClipDragKind::ResizeRight, 2.0, 4.0, delta, 12.0);
        assert!((right_start - 2.0).abs() <= f32::EPSILON);
        assert!((right_length - (4.0 + delta)).abs() <= f32::EPSILON);
    }

    #[test]
    fn resize_preview_keeps_clip_content_at_its_original_timeline_position() {
        let current = Rect::from_min_max(Pos2::new(130.0, 10.0), Pos2::new(160.0, 50.0));
        let mut drag = ClipDrag {
            clip_id: "clip".into(),
            track: 0,
            clip: 0,
            original_start: 2.0,
            original_length: 4.0,
            pointer_start: Pos2::ZERO,
            kind: ClipDragKind::ResizeLeft,
            event_clip: false,
            start: 3.0,
            length: 3.0,
            target_track: 0,
        };
        assert_eq!(
            waveform_preview_rect(current, &drag),
            Rect::from_min_max(Pos2::new(120.0, 10.0), Pos2::new(160.0, 50.0))
        );

        drag.kind = ClipDragKind::ResizeRight;
        drag.start = 2.0;
        let current = Rect::from_min_max(Pos2::new(120.0, 10.0), Pos2::new(150.0, 50.0));
        assert_eq!(
            waveform_preview_rect(current, &drag),
            Rect::from_min_max(Pos2::new(120.0, 10.0), Pos2::new(160.0, 50.0))
        );

        drag.kind = ClipDragKind::ResizeLeft;
        drag.start = 1.0;
        drag.length = 5.0;
        let expanded = Rect::from_min_max(Pos2::new(110.0, 10.0), Pos2::new(160.0, 50.0));
        assert_eq!(
            waveform_preview_rect(expanded, &drag),
            Rect::from_min_max(Pos2::new(120.0, 10.0), Pos2::new(160.0, 50.0))
        );
    }

    #[test]
    fn clip_targets_respect_core_track_compatibility() {
        assert!(clip_can_target(true, TrackKind::Event));
        assert!(!clip_can_target(true, TrackKind::Audio));
        assert!(!clip_can_target(true, TrackKind::Composition));
        assert!(clip_can_target(false, TrackKind::Audio));
        assert!(clip_can_target(false, TrackKind::Composition));
        assert!(!clip_can_target(false, TrackKind::Event));
    }

    #[test]
    fn track_hit_testing_excludes_ruler_and_overflow() {
        assert_eq!(track_at_y(RULER_HEIGHT - 1.0, 0.0, 3), None);
        assert_eq!(track_at_y(RULER_HEIGHT, 0.0, 3), Some(0));
        assert_eq!(track_at_y(RULER_HEIGHT + TRACK_HEIGHT, 0.0, 3), Some(1));
        assert_eq!(track_at_y(RULER_HEIGHT + TRACK_HEIGHT * 3.0, 0.0, 3), None);
    }

    #[test]
    fn loop_ranges_normalize_and_keep_a_minimum_length() {
        assert_eq!(normalized_loop_range(6.0, 2.0, 12.0), (2.0, 6.0));
        assert_eq!(normalized_loop_range(4.0, 4.0, 12.0), (4.0, 4.25));
        assert_eq!(normalized_loop_range(12.0, 12.0, 12.0), (11.75, 12.0));
        assert_eq!(
            ruler_drag_range(
                RulerDrag {
                    kind: RulerDragKind::Start,
                    anchor: 8.0,
                    current: 3.0,
                    original_start: 2.0,
                    original_end: 8.0,
                },
                12.0,
            ),
            (3.0, 8.0)
        );
    }

    #[test]
    fn moving_loop_preserves_length_and_clamps_to_composition() {
        assert_eq!(moved_loop_range(2.0, 6.0, 1.5, 12.0), (3.5, 7.5));
        assert_eq!(moved_loop_range(2.0, 6.0, -5.0, 12.0), (0.0, 4.0));
        assert_eq!(moved_loop_range(2.0, 6.0, 20.0, 12.0), (8.0, 12.0));

        let drag = RulerDrag {
            kind: RulerDragKind::Move,
            anchor: 3.0,
            current: 5.5,
            original_start: 2.0,
            original_end: 6.0,
        };
        let Intent::SetLoopRange { start, end } = finish_ruler_drag(drag, 12.0) else {
            panic!("ruler drag must emit a loop-range intent");
        };
        assert_eq!((start, end), (4.5, 8.5));
    }

    #[test]
    fn ctrl_primary_drag_only_moves_when_started_inside_loop_body() {
        assert_eq!(
            ruler_body_drag_kind(true, 4.0, (2.0, 6.0)),
            RulerDragKind::Move
        );
        assert_eq!(
            ruler_body_drag_kind(true, 8.0, (2.0, 6.0)),
            RulerDragKind::Range
        );
        assert_eq!(
            ruler_body_drag_kind(false, 4.0, (2.0, 6.0)),
            RulerDragKind::Range
        );
    }

    #[test]
    fn disabled_loop_range_remains_visible_but_faded() {
        let enabled = loop_visual_alpha(true);
        let disabled = loop_visual_alpha(false);
        assert!(disabled.0 > 0.0);
        assert!(disabled.1 > 0.0);
        assert!(disabled.0 < enabled.0);
        assert!(disabled.1 < enabled.1);
    }
}
