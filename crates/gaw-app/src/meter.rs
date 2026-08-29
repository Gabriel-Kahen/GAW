#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Stroke, StrokeKind};

use crate::theme::{BORDER, PANEL_ALT};

const GREEN: Color32 = Color32::from_rgb(44, 190, 90);
const YELLOW: Color32 = Color32::from_rgb(226, 188, 52);
const RED: Color32 = Color32::from_rgb(230, 67, 57);
const FLOOR_DB: f32 = -60.0;

#[derive(Clone, Copy)]
pub(crate) enum MeterOrientation {
    Horizontal,
    Vertical,
}

pub(crate) fn level_db(peak: f32) -> f32 {
    if peak.is_finite() && peak > 0.0 {
        (20.0 * peak.log10()).max(FLOOR_DB)
    } else {
        FLOOR_DB
    }
}

pub(crate) fn paint_level_meter(
    painter: &Painter,
    rect: Rect,
    peak: f32,
    orientation: MeterOrientation,
) {
    painter.rect_filled(rect, CornerRadius::ZERO, PANEL_ALT);
    let fraction = ((level_db(peak) - FLOOR_DB) / -FLOOR_DB).clamp(0.0, 1.0);
    let segments = match orientation {
        MeterOrientation::Horizontal => (rect.width() / 4.0).floor().max(1.0) as usize,
        MeterOrientation::Vertical => (rect.height() / 4.0).floor().max(1.0) as usize,
    };
    let lit = (fraction * segments as f32).ceil() as usize;
    for index in 0..lit.min(segments) {
        let start = index as f32 / segments as f32;
        let end = (index + 1) as f32 / segments as f32;
        let db = FLOOR_DB + end * -FLOOR_DB;
        let color = if db >= -3.0 {
            RED
        } else if db >= -12.0 {
            YELLOW
        } else {
            GREEN
        };
        let segment = match orientation {
            MeterOrientation::Horizontal => Rect::from_min_max(
                Pos2::new(rect.left() + start * rect.width(), rect.top()),
                Pos2::new(rect.left() + end * rect.width() - 1.0, rect.bottom()),
            ),
            MeterOrientation::Vertical => Rect::from_min_max(
                Pos2::new(rect.left(), rect.bottom() - end * rect.height()),
                Pos2::new(rect.right(), rect.bottom() - start * rect.height() - 1.0),
            ),
        };
        if segment.is_positive() {
            painter.rect_filled(segment, CornerRadius::ZERO, color);
        }
    }
    painter.rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, BORDER),
        StrokeKind::Inside,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_scale_has_a_silent_floor_and_zero_db_ceiling() {
        assert!((level_db(0.0) - FLOOR_DB).abs() < f32::EPSILON);
        assert!((level_db(1.0) - 0.0).abs() < f32::EPSILON);
        assert!((level_db(0.1) + 20.0).abs() < 0.001);
    }
}
