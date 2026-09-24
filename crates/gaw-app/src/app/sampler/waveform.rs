use egui::{Align2, CursorIcon, FontId, Id, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::model::WaveformPoint;
use crate::theme::{AUDIO_TONE, BORDER, CANVAS, DIM, TEXT};
use crate::timeline::paint_waveform;

#[derive(Clone, Copy, Debug)]
pub(super) struct SliceSource<'a> {
    pub waveform: &'a [WaveformPoint],
    pub duration: f64,
    pub min_length: f64,
    pub preview: Option<f64>,
}

#[derive(Debug, Default)]
pub(super) struct SliceResponse {
    pub changed: bool,
    pub finished: bool,
    pub seek: Option<f64>,
}

#[derive(Clone, Copy)]
enum DragKind {
    Start,
    End,
    Move,
    Select,
}

#[derive(Clone, Copy)]
struct Drag {
    kind: DragKind,
    anchor: f64,
    start: f64,
    end: f64,
}

#[derive(Clone, Default)]
struct View {
    offset: f64,
    span: f64,
    drag: Option<Drag>,
}

impl View {
    fn normalize(&mut self, duration: f64, minimum: f64) {
        if !self.span.is_finite() || self.span <= 0.0 {
            self.span = duration;
        }
        self.span = self.span.clamp(minimum, duration);
        if !self.offset.is_finite() {
            self.offset = 0.0;
        }
        self.offset = self.offset.clamp(0.0, duration - self.span);
    }

    fn time(&self, x: f32, rect: Rect, duration: f64) -> f64 {
        (self.offset + f64::from((x - rect.left()) / rect.width()) * self.span).clamp(0.0, duration)
    }

    fn x(&self, time: f64, rect: Rect) -> f32 {
        rect.left() + ((time - self.offset) / self.span) as f32 * rect.width()
    }

    fn zoom(&mut self, factor: f64, anchor: f64, duration: f64, minimum: f64) {
        let phase = ((anchor - self.offset) / self.span).clamp(0.0, 1.0);
        self.span = (self.span * factor).clamp(minimum, duration);
        self.offset = anchor - phase * self.span;
        self.normalize(duration, minimum);
    }
}

/// Edits a local draft. Commit it only when `finished` is true.
pub(super) fn slice_waveform(
    ui: &mut egui::Ui,
    id: Id,
    source: SliceSource<'_>,
    start: &mut f64,
    end: &mut f64,
) -> SliceResponse {
    let mut result = SliceResponse::default();
    if !source.duration.is_finite() || source.duration <= 0.0 {
        ui.label(egui::RichText::new("No audio selected").color(DIM));
        return result;
    }
    let duration = source.duration;
    let minimum = if source.min_length.is_finite() && source.min_length > 0.0 {
        source.min_length.min(duration)
    } else {
        (1.0 / 48_000.0_f64).min(duration)
    };
    *start = if start.is_finite() { *start } else { 0.0 }.clamp(0.0, duration - minimum);
    *end = if end.is_finite() { *end } else { duration }.clamp(*start + minimum, duration);
    let mut view = ui.data_mut(|data| data.get_temp::<View>(id).unwrap_or_default());
    view.normalize(duration, minimum);

    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("Zoom").small().color(DIM));
        let center = view.offset + view.span * 0.5;
        if ui.small_button("−").on_hover_text("Zoom out").clicked() {
            view.zoom(2.0, center, duration, minimum);
        }
        if ui.small_button("+").on_hover_text("Zoom in").clicked() {
            view.zoom(0.5, center, duration, minimum);
        }
        if ui
            .small_button("Fit")
            .on_hover_text("Show all audio")
            .clicked()
        {
            view.offset = 0.0;
            view.span = duration;
        }
        if ui
            .small_button("Slice")
            .on_hover_text("Zoom to selected slice")
            .clicked()
        {
            let padding = (*end - *start) * 0.1;
            view.offset = *start - padding;
            view.span = *end - *start + padding * 2.0;
            view.normalize(duration, minimum);
        }
        if view.span < duration {
            ui.add(
                egui::Slider::new(&mut view.offset, 0.0..=duration - view.span).show_value(false),
            )
            .on_hover_text("Scroll through the audio");
        }
    });
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 180.0), Sense::hover());
    let body = rect.shrink2(Vec2::new(1.0, 18.0));
    let ruler = Rect::from_min_max(rect.min, Pos2::new(rect.right(), body.top()));
    let response = ui.interact(body, id.with("trim"), Sense::drag());
    let ruler_response = ui.interact(ruler, id.with("seek"), Sense::click());
    if ui.rect_contains_pointer(rect) {
        let (scroll, shift, pinch, pointer) = ui.input(|input| {
            (
                input.smooth_scroll_delta,
                input.modifiers.shift,
                input.zoom_delta(),
                input.pointer.hover_pos(),
            )
        });
        let anchor = pointer.map_or(view.offset + view.span * 0.5, |p| {
            view.time(p.x, body, duration)
        });
        if (pinch - 1.0).abs() > f32::EPSILON {
            view.zoom(1.0 / f64::from(pinch), anchor, duration, minimum);
        } else if !shift && scroll.y != 0.0 {
            view.zoom(
                f64::from((-scroll.y * 0.005).exp()),
                anchor,
                duration,
                minimum,
            );
        }
        let pan = scroll.x + if shift { scroll.y } else { 0.0 };
        if pan != 0.0 {
            view.offset -= f64::from(pan) * view.span / f64::from(body.width());
            view.normalize(duration, minimum);
        }
        // Keep waveform gestures from scrolling its enclosing settings panel.
        ui.input_mut(|input| {
            input.smooth_scroll_delta = Vec2::ZERO;
        });
    }
    if ruler_response.clicked()
        && let Some(pointer) = ruler_response.interact_pointer_pos()
    {
        result.seek = Some(view.time(pointer.x, body, duration));
    }
    if response.drag_started()
        && let Some(origin) = ui.input(|input| input.pointer.press_origin())
    {
        let start_distance = (origin.x - view.x(*start, body)).abs();
        let end_distance = (origin.x - view.x(*end, body)).abs();
        let anchor = view.time(origin.x, body, duration);
        let kind = if start_distance.min(end_distance) <= 9.0 {
            if start_distance <= end_distance {
                DragKind::Start
            } else {
                DragKind::End
            }
        } else if anchor > *start
            && anchor < *end
            && (*end - *start) < duration
            && !ui.input(|input| input.modifiers.shift)
        {
            DragKind::Move
        } else {
            DragKind::Select
        };
        view.drag = Some(Drag {
            kind,
            anchor,
            start: *start,
            end: *end,
        });
    }
    if let Some(drag) = view.drag
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let time = view.time(pointer.x, body, duration);
        let previous = (*start, *end);
        match drag.kind {
            DragKind::Start => *start = time.clamp(0.0, *end - minimum),
            DragKind::End => *end = time.clamp(*start + minimum, duration),
            DragKind::Move => {
                let length = drag.end - drag.start;
                *start = (drag.start + time - drag.anchor).clamp(0.0, duration - length);
                *end = *start + length;
            }
            DragKind::Select => {
                *start = drag.anchor.min(time).clamp(0.0, duration - minimum);
                *end = drag.anchor.max(time).clamp(*start + minimum, duration);
            }
        }
        result.changed = previous != (*start, *end);
    }
    if (response.drag_stopped() || !ui.input(|input| input.pointer.primary_down()))
        && let Some(drag) = view.drag.take()
    {
        result.finished = (drag.start, drag.end) != (*start, *end);
    }

    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 5.0, CANVAS);
    let slice = Rect::from_min_max(
        Pos2::new(view.x(*start, body), body.top()),
        Pos2::new(view.x(*end, body), body.bottom()),
    )
    .intersect(body);
    if slice.is_positive() {
        painter.rect_filled(slice, 0.0, AUDIO_TONE.gamma_multiply(0.13));
    }
    painter.hline(body.x_range(), body.center().y, Stroke::new(0.5, BORDER));
    let full_waveform = Rect::from_min_max(
        Pos2::new(view.x(0.0, body), body.top()),
        Pos2::new(view.x(duration, body), body.bottom()),
    );
    paint_waveform(
        &painter.with_clip_rect(body),
        full_waveform,
        source.waveform,
        DIM.gamma_multiply(0.4),
    );
    if slice.is_positive() {
        paint_waveform(
            &painter.with_clip_rect(slice),
            full_waveform,
            source.waveform,
            AUDIO_TONE,
        );
    }
    for boundary in [*start, *end] {
        let x = view.x(boundary, body);
        if body.x_range().contains(x) {
            painter.vline(x, body.y_range(), Stroke::new(2.0, AUDIO_TONE));
            painter.rect_filled(
                Rect::from_center_size(Pos2::new(x, body.center().y), Vec2::new(6.0, 26.0)),
                2.0,
                AUDIO_TONE,
            );
        }
    }
    if let Some(time) = source.preview.filter(|time| time.is_finite()) {
        let x = view.x(time, body);
        if body.x_range().contains(x) {
            painter.vline(x, body.y_range(), Stroke::new(1.0, TEXT));
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(x - 4.0, body.top() - 6.0),
                    Pos2::new(x + 4.0, body.top() - 6.0),
                    Pos2::new(x, body.top()),
                ],
                TEXT,
                Stroke::NONE,
            ));
        }
    }
    for index in 0..=4 {
        let phase = f64::from(index) / 4.0;
        let time = view.offset + view.span * phase;
        let align = if index == 0 {
            Align2::LEFT_BOTTOM
        } else if index == 4 {
            Align2::RIGHT_BOTTOM
        } else {
            Align2::CENTER_BOTTOM
        };
        painter.text(
            Pos2::new(view.x(time, body), body.top() - 2.0),
            align,
            format!("{time:.2}s"),
            FontId::monospace(10.0),
            DIM,
        );
    }
    painter.rect_stroke(rect, 5.0, Stroke::new(1.0, BORDER), StrokeKind::Inside);
    if let Some(pointer) = response.hover_pos() {
        let time = view.time(pointer.x, body, duration);
        let near_handle = (pointer.x - view.x(*start, body))
            .abs()
            .min((pointer.x - view.x(*end, body)).abs())
            <= 9.0;
        ui.ctx().set_cursor_icon(if near_handle {
            CursorIcon::ResizeHorizontal
        } else if time > *start
            && time < *end
            && (*end - *start) < duration
            && !ui.input(|input| input.modifiers.shift)
        {
            CursorIcon::Grab
        } else {
            CursorIcon::Crosshair
        });
    }
    ruler_response
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Click to listen from here");
    response.on_hover_text(
        "Drag edges to trim · Shift + drag to select\nScroll to zoom · Shift + scroll to move",
    );
    ui.data_mut(|data| data.insert_temp(id, view));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Editor {
        context: egui::Context,
        start: f64,
        end: f64,
        rect: Rect,
    }

    impl Editor {
        fn new(start: f64, end: f64) -> Self {
            let mut editor = Self {
                context: egui::Context::default(),
                start,
                end,
                rect: Rect::NOTHING,
            };
            editor.frame(Vec::new());
            editor
        }

        fn frame(&mut self, events: Vec<egui::Event>) -> SliceResponse {
            self.frame_with_modifiers(events, egui::Modifiers::NONE)
        }

        fn frame_with_modifiers(
            &mut self,
            events: Vec<egui::Event>,
            modifiers: egui::Modifiers,
        ) -> SliceResponse {
            let mut result = SliceResponse::default();
            let _ = self.context.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(600.0, 400.0))),
                    events,
                    modifiers,
                    ..Default::default()
                },
                |ui| {
                    let top = ui.cursor().top();
                    result = slice_waveform(
                        ui,
                        Id::new("waveform-test"),
                        SliceSource {
                            waveform: &[],
                            duration: 10.0,
                            min_length: 0.01,
                            preview: None,
                        },
                        &mut self.start,
                        &mut self.end,
                    );
                    self.rect = Rect::from_min_max(
                        Pos2::new(
                            ui.max_rect().left() + 1.0,
                            top + ui.spacing().interact_size.y + ui.spacing().item_spacing.y + 18.0,
                        ),
                        Pos2::new(
                            ui.max_rect().right() - 1.0,
                            ui.cursor().top() - ui.spacing().item_spacing.y - 18.0,
                        ),
                    );
                },
            );
            result
        }

        fn view(&self) -> View {
            self.context
                .data(|data| data.get_temp::<View>(Id::new("waveform-test")).unwrap())
        }

        fn point(&self, time: f64) -> Pos2 {
            Pos2::new(
                self.rect.left() + (time / 10.0) as f32 * self.rect.width(),
                self.rect.center().y,
            )
        }

        fn button(&mut self, point: Pos2, pressed: bool) -> SliceResponse {
            self.frame(vec![
                egui::Event::PointerMoved(point),
                egui::Event::PointerButton {
                    pos: point,
                    button: egui::PointerButton::Primary,
                    pressed,
                    modifiers: egui::Modifiers::NONE,
                },
            ])
        }

        fn drag(&mut self, from: f64, to: f64) -> SliceResponse {
            self.button(self.point(from), true);
            let point = self.point(to);
            let changed = self.frame(vec![egui::Event::PointerMoved(point)]);
            assert!(changed.changed);
            assert!(!changed.finished);
            self.button(point, false)
        }
    }

    #[test]
    fn dragging_empty_audio_selects_a_slice_and_commits_only_on_release() {
        let mut editor = Editor::new(2.0, 4.0);
        let response = editor.drag(8.0, 5.0);
        assert!(response.finished);
        assert!((editor.start - 5.0).abs() < 0.02);
        assert!((editor.end - 8.0).abs() < 0.02);
        assert!(!editor.frame(Vec::new()).finished);
    }

    #[test]
    fn dragging_inside_the_full_source_creates_a_slice() {
        let mut editor = Editor::new(0.0, 10.0);
        assert!(editor.drag(3.0, 7.0).finished);
        assert!((editor.start - 3.0).abs() < 0.02);
        assert!((editor.end - 7.0).abs() < 0.02);
    }

    #[test]
    fn handles_keep_a_nonempty_slice_and_clamp_to_source() {
        let mut editor = Editor::new(2.0, 4.0);
        assert!(editor.drag(2.0, 5.0).finished);
        assert!((editor.start - 3.99).abs() < 0.02);
        assert!((editor.end - 4.0).abs() < 0.02);
        let mut editor = Editor::new(2.0, 4.0);
        assert!(editor.drag(4.0, 12.0).finished);
        assert!((editor.end - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dragging_inside_moves_the_slice_without_changing_length() {
        let mut editor = Editor::new(2.0, 4.0);
        assert!(editor.drag(3.0, 9.5).finished);
        assert!((editor.start - 8.0).abs() < f64::EPSILON);
        assert!((editor.end - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn wheel_zooms_at_pointer_without_cropping_or_scrolling_parent() {
        let mut editor = Editor::new(2.0, 4.0);
        let point = editor.point(7.5);
        editor.frame(vec![
            egui::Event::PointerMoved(point),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: Vec2::new(0.0, 80.0),
                phase: egui::TouchPhase::Move,
                modifiers: egui::Modifiers::NONE,
            },
        ]);
        let view = editor.view();
        assert!(view.span < 10.0);
        assert!((view.time(point.x, editor.rect, 10.0) - 7.5).abs() < 0.02);
        assert_eq!((editor.start, editor.end), (2.0, 4.0));
        editor.context.input(|input| {
            assert_eq!(input.smooth_scroll_delta, Vec2::ZERO);
        });
    }

    #[test]
    fn shift_wheel_pans_without_zooming_or_cropping() {
        let mut editor = Editor::new(2.0, 4.0);
        editor.context.data_mut(|data| {
            data.insert_temp(
                Id::new("waveform-test"),
                View {
                    offset: 2.0,
                    span: 4.0,
                    drag: None,
                },
            );
        });
        let point = editor.point(5.0);
        editor.frame_with_modifiers(
            vec![
                egui::Event::PointerMoved(point),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: Vec2::new(0.0, -80.0),
                    phase: egui::TouchPhase::Move,
                    modifiers: egui::Modifiers::SHIFT,
                },
            ],
            egui::Modifiers::SHIFT,
        );
        let view = editor.view();
        assert!(view.offset > 2.0);
        assert!((view.span - 4.0).abs() < f64::EPSILON);
        assert_eq!((editor.start, editor.end), (2.0, 4.0));
    }

    #[test]
    fn ruler_click_requests_audition_without_changing_crop() {
        let mut editor = Editor::new(2.0, 4.0);
        let point = Pos2::new(editor.point(6.0).x, editor.rect.top() - 9.0);
        assert!(editor.button(point, true).seek.is_none());
        let response = editor.button(point, false);
        assert!((response.seek.unwrap() - 6.0).abs() < 0.02);
        assert!(!response.changed && !response.finished);
        assert_eq!((editor.start, editor.end), (2.0, 4.0));
        assert!(editor.frame(Vec::new()).seek.is_none());
    }
}
