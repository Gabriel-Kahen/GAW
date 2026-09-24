use super::{
    Align2, BORDER, CANVAS, DIM, FontId, GawApp, NESTED_TONE, PANEL_ALT, Pos2, Rect, RichText,
    STATUS_NOTICE, Sense, Stroke, TEXT, Vec2, egui,
};
use gaw_audio::BassTunerReading;

const STRINGS: [(&str, f32); 4] = [
    ("E1", 41.203_445),
    ("A1", 55.0),
    ("D2", 73.416_19),
    ("G2", 97.998_856),
];
const IN_TUNE_CENTS: f32 = 5.0;

impl GawApp {
    pub(super) fn bass_tuner_window(&mut self, ctx: &egui::Context) {
        let Some(controller) = &self.controller else {
            self.tuner_open = false;
            return;
        };
        let status = controller.input_monitor_status();
        self.tuner_open &= status.enabled;
        controller.set_tuner_enabled(self.tuner_open);
        if !self.tuner_open {
            return;
        }
        let reading = controller.tuner_reading();
        egui::Window::new("BASS TUNER")
            .id(egui::Id::new("bass_tuner_window"))
            .open(&mut self.tuner_open)
            .collapsible(false)
            .resizable(false)
            .default_width(360.0)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                egui::Frame::window(&ctx.global_style())
                    .fill(PANEL_ALT)
                    .stroke(Stroke::new(1.0, BORDER)),
            )
            .show(ctx, |ui| {
                tuner_contents(ui, reading, status.opening);
                if let Some(device) = &status.device_name {
                    ui.separator();
                    ui.label(
                        RichText::new(format!(
                            "{device} · Input {}",
                            self.audio_preferences.input_channel + 1
                        ))
                        .size(10.0)
                        .color(DIM),
                    );
                }
            });
        controller.set_tuner_enabled(self.tuner_open);
        ctx.request_repaint_after(std::time::Duration::from_millis(33));
    }
}

fn tuner_contents(ui: &mut egui::Ui, reading: Option<BassTunerReading>, opening: bool) {
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new("4 STRINGS · STANDARD · A4 = 440 Hz")
                .size(10.0)
                .color(DIM),
        );
        ui.add_space(12.0);
        ui.columns(4, |columns| {
            for (index, (name, frequency)) in STRINGS.iter().enumerate() {
                let active = reading.is_some_and(|reading| reading.string_index == index);
                columns[index].vertical_centered(|ui| {
                    ui.label(
                        RichText::new(*name)
                            .monospace()
                            .size(24.0)
                            .color(if active { TEXT } else { DIM }),
                    );
                    ui.label(
                        RichText::new(format!("{frequency:.2} Hz"))
                            .size(10.0)
                            .color(DIM),
                    );
                });
            }
        });
        ui.add_space(16.0);
        let in_tune = reading.is_some_and(|reading| reading.cents.abs() <= IN_TUNE_CENTS);
        let color = if in_tune { NESTED_TONE } else { STATUS_NOTICE };
        if let Some(reading) = reading {
            ui.label(
                RichText::new(format!("{:+.1} cents", reading.cents))
                    .monospace()
                    .size(28.0)
                    .color(color),
            );
            ui.label(
                RichText::new(format!("{:.2} Hz", reading.frequency_hz))
                    .monospace()
                    .color(TEXT),
            );
        } else {
            ui.label(RichText::new("— cents").monospace().size(28.0).color(DIM));
            ui.label(RichText::new("— Hz").monospace().color(DIM));
        }
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 62.0), Sense::hover());
        let rail = Rect::from_min_max(
            rect.min + Vec2::new(16.0, 12.0),
            rect.max - Vec2::new(16.0, 24.0),
        );
        let painter = ui.painter();
        painter.rect_filled(rail, 3.0, CANVAS);
        let x_at = |cents: f32| rail.center().x + cents.clamp(-50.0, 50.0) / 100.0 * rail.width();
        let center = Rect::from_min_max(
            Pos2::new(x_at(-IN_TUNE_CENTS), rail.top()),
            Pos2::new(x_at(IN_TUNE_CENTS), rail.bottom()),
        );
        painter.rect_filled(center, 0.0, NESTED_TONE.gamma_multiply(0.25));
        for cents in [-50.0, -25.0, 0.0, 25.0, 50.0] {
            let x = x_at(cents);
            painter.line_segment(
                [Pos2::new(x, rail.top()), Pos2::new(x, rail.bottom())],
                Stroke::new(1.0, BORDER),
            );
            painter.text(
                Pos2::new(x, rail.bottom() + 5.0),
                Align2::CENTER_TOP,
                format!("{cents:+.0}"),
                FontId::monospace(9.0),
                DIM,
            );
        }
        if let Some(reading) = reading {
            let x = x_at(reading.cents);
            painter.line_segment(
                [
                    Pos2::new(x, rail.top() - 4.0),
                    Pos2::new(x, rail.bottom() + 3.0),
                ],
                Stroke::new(3.0, color),
            );
        }
        let guidance = reading.map_or(
            if opening {
                "Opening input…"
            } else {
                "Play one open string at a time"
            },
            |reading| {
                if in_tune {
                    "IN TUNE"
                } else if reading.cents < 0.0 {
                    "FLAT · Tighten the string"
                } else {
                    "SHARP · Loosen the string"
                }
            },
        );
        ui.label(RichText::new(guidance).color(if reading.is_some() { color } else { DIM }));
        ui.add_space(6.0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tuner_feedback_fits_the_panel_for_silence_and_detuned_strings() {
        for (cents, opening, expected) in [
            (None, true, "Opening input…"),
            (None, false, "Play one open string at a time"),
            (Some(-140.0), false, "FLAT · Tighten the string"),
            (Some(140.0), false, "SHARP · Loosen the string"),
            (Some(0.0), false, "IN TUNE"),
        ] {
            let ctx = egui::Context::default();
            let reading = cents.map(|cents| BassTunerReading {
                string_index: 0,
                frequency_hz: STRINGS[0].1 * 2.0_f32.powf(cents / 1200.0),
                cents,
            });
            let output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(380.0, 350.0))),
                    ..Default::default()
                },
                |ui| tuner_contents(ui, reading, opening),
            );
            let mut labels = Vec::new();
            for shape in output.shapes {
                if let egui::Shape::Text(text) = shape.shape {
                    let bounds = text.galley.rect.translate(text.pos.to_vec2());
                    assert!(
                        shape.clip_rect.contains_rect(bounds),
                        "clipped label: {}",
                        text.galley.text()
                    );
                    labels.push(text.galley.text().to_owned());
                }
            }
            assert!(labels.iter().any(|label| label == expected));
            for (name, _) in STRINGS {
                assert!(labels.iter().any(|label| label == name));
            }
        }
    }
}
