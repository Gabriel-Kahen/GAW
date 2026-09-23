//! Shared graphical EQ editor. Colors identify band slots, never signal level.
use super::{
    Align, Align2, BORDER, CANVAS, Color32, DIM, FontId, GawApp, Layout, PANEL_ALT, Pos2, Rect,
    RichText, Sense, Stroke, StrokeKind, TEXT, Vec2, egui,
};
use gaw_core::{EqBand, EqShape, FilterSlope, ParametricEqParameters, ProcessorKind};

const BAND_COLORS: [Color32; 8] = [
    Color32::from_rgb(242, 100, 105),
    Color32::from_rgb(244, 160, 83),
    Color32::from_rgb(228, 204, 91),
    Color32::from_rgb(113, 205, 131),
    Color32::from_rgb(87, 204, 205),
    Color32::from_rgb(104, 160, 243),
    Color32::from_rgb(169, 134, 238),
    Color32::from_rgb(227, 131, 208),
];
const GAIN_RANGE: f32 = 24.0;

#[cfg(test)]
mod tests;

#[derive(Clone, Default)]
struct EqEditorState {
    selected: usize,
}

impl GawApp {
    pub(super) fn equalizer_window(&mut self, ctx: &egui::Context) {
        if self
            .vm
            .selected_processor_view()
            .is_none_or(|effect| effect.kind != "gaw.parametric_eq")
        {
            return;
        }
        let Some(stack) = self.vm.signal_stack() else {
            return;
        };
        let scope = self.vm.signal_scope_label(&stack);
        let bounds = ctx.content_rect();
        let available_width = (bounds.width() - 24.0).max(260.0);
        let available_height = (bounds.height() - 120.0).max(300.0);
        let width = (bounds.width() * 0.72)
            .clamp(620.0, 1_120.0)
            .min(available_width);
        let height = (bounds.height() * 0.58)
            .clamp(430.0, 640.0)
            .min(available_height);
        let mut open = true;
        egui::Window::new(format!("PARAMETRIC EQ  ·  {scope}"))
            .id(egui::Id::new("parametric_eq_window"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .movable(true)
            .anchor(Align2::CENTER_TOP, Vec2::new(0.0, 96.0))
            .default_size(Vec2::new(width, height))
            .min_size(Vec2::new(width.min(620.0), height.min(400.0)))
            .max_size(Vec2::new(available_width, available_height))
            .frame(
                egui::Frame::window(&ctx.global_style())
                    .fill(PANEL_ALT)
                    .stroke(Stroke::new(1.0, BORDER)),
            )
            .show(ctx, |ui| self.equalizer_editor(ui));
        if !open {
            self.vm.close_selected_processor_editor();
        }
    }

    pub(super) fn equalizer_editor(&mut self, ui: &mut egui::Ui) {
        let Some(processor) = self.vm.selected_processor() else {
            return;
        };
        let ProcessorKind::ParametricEq(mut parameters) = processor.kind else {
            return;
        };
        let Some(stack) = self.vm.signal_stack() else {
            return;
        };
        let id = ui.id().with(("eq_editor", processor.id.to_string()));
        let mut state = ui.data_mut(|data| data.get_temp::<EqEditorState>(id).unwrap_or_default());
        state.selected = state.selected.min(parameters.bands.len().saturating_sub(1));
        let original = parameters.clone();
        let sample_rate = self.vm.project().sample_rate.value();
        let mut remove = None;
        let mut gesture_started = false;
        let mut gesture_stopped = false;
        let mut bypass = false;
        let editor_width = ui.available_width();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Drag a node to shape the sound").color(DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                bypass = ui
                    .selectable_label(!processor.enabled, "BYPASS EQ")
                    .on_hover_text("Compare the unprocessed signal")
                    .clicked();
                let output = ui.add(
                    egui::DragValue::new(&mut parameters.output_gain_db)
                        .range(-24.0..=24.0)
                        .speed(0.1)
                        .fixed_decimals(1)
                        .prefix("OUTPUT  ")
                        .suffix(" dB"),
                );
                gesture_started |= output.drag_started();
                gesture_stopped |= output.drag_stopped();
            });
        });
        ui.separator();
        egui::ScrollArea::vertical().id_salt(id).auto_shrink([false, false]).show(ui, |ui| {
            let content_width = (editor_width - ui.spacing().scroll.bar_width - ui.spacing().scroll.bar_inner_margin).max(100.0);
            ui.set_width(content_width);
            egui::ScrollArea::horizontal()
                .id_salt(id.with("bands"))
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        for (index, band) in parameters.bands.iter_mut().enumerate() {
                        let color = BAND_COLORS[index];
                            let title = format!("{}  {}", index + 1, shape_short(band.shape));
                            let response = ui.add(egui::Button::new(RichText::new(title).monospace().size(11.0).color(if band.enabled { color } else { DIM }))
                            .selected(state.selected == index)
                            .fill(if state.selected == index { color.gamma_multiply(0.15) } else { CANVAS })
                            .stroke(Stroke::new(1.0, if state.selected == index { color } else { BORDER }))
                                .min_size(Vec2::new(60.0, 28.0)));
                            if response.clicked() {
                                state.selected = index;
                            }
                            response
                                .on_hover_text(format!(
                                    "Band {} · {} · {}{}",
                                    index + 1,
                                    shape_name(band.shape),
                                    frequency_label(band.frequency_hz),
                                    if band.enabled { "" } else { " · off" }
                                ))
                                .context_menu(|ui| {
                                    if ui
                                        .button(if band.enabled { "Turn band off" } else { "Turn band on" })
                                        .clicked()
                                    {
                                        band.enabled = !band.enabled;
                                        ui.close();
                                    }
                                    if ui.button("Remove band").clicked() {
                                        remove = Some(index);
                                        ui.close();
                                    }
                                });
                        }
                        if parameters.bands.len() < 8
                            && ui
                                .button("+ BAND")
                                .on_hover_text("Add a bell band")
                                .clicked()
                        {
                            parameters.bands.push(EqBand::default());
                            state.selected = parameters.bands.len() - 1;
                        }
                    });
                });
            ui.add_space(8.0);
            let graph_height = (ui.available_height() - 90.0).clamp(240.0, 360.0);
            let (rect, response) = ui.allocate_exact_size(Vec2::new(content_width, graph_height), Sense::click_and_drag());
            let plot = Rect::from_min_max(rect.min + Vec2::new(36.0, 9.0), rect.max - Vec2::new(36.0, 23.0));
            let mut changed_node = false;
            let max_hz = self.vm.equalizer_max_hz();
            if plot.width() > 0.0 && plot.height() > 0.0 {
                if response.drag_started() { ui.data_mut(|data| data.remove::<usize>(id.with("drag_band"))); }
                let press = if response.drag_started() { ui.input(|input| input.pointer.press_origin()) } else { response.interact_pointer_pos() };
                if (response.clicked() || response.drag_started()) && let Some(pointer) = press {
                    if let Some(index) = closest_band(&parameters.bands, plot, pointer, max_hz) {
                        state.selected = index;
                        if response.drag_started() {
                            ui.data_mut(|data| data.insert_temp(id.with("drag_band"), index));
                            gesture_started = true;
                        }
                    } else if response.double_clicked() && plot.contains(pointer) && parameters.bands.len() < 8 {
                        parameters.bands.push(EqBand { frequency_hz: frequency_at(plot, pointer.x, max_hz), gain_db: gain_at(plot, pointer.y), ..EqBand::default() });
                        state.selected = parameters.bands.len() - 1;
                    }
                }
                if response.dragged() && let Some(index) = ui.data(|data| data.get_temp::<usize>(id.with("drag_band")))
                    && let Some(pointer) = response.interact_pointer_pos()
                    && let Some(band) = parameters.bands.get_mut(index)
                {
                    if ui.input(|input| input.modifiers.shift) {
                        let vertical_delta = ui.input(|input| input.pointer.delta().y);
                        band.q = (band.q * (-vertical_delta * 0.018).exp()).clamp(0.1, 30.0);
                    } else {
                        band.frequency_hz = frequency_at(plot, pointer.x, max_hz);
                        if has_gain(band.shape) { band.gain_db = gain_at(plot, pointer.y); }
                    }
                    changed_node = true;
                }
                if response.drag_stopped() {
                    ui.data_mut(|data| data.remove::<usize>(id.with("drag_band")));
                    gesture_stopped = true;
                }
                paint_graph(ui.painter(), rect, plot, &parameters, processor.enabled, sample_rate, Some(state.selected));
                if let Some(pointer) = ui.input(|input| input.pointer.hover_pos())
                    && plot.contains(pointer)
                    && let Some(index) = closest_band(&parameters.bands, plot, pointer, max_hz)
                    && let Some(band) = parameters.bands.get(index)
                {
                    paint_node_readout(ui.painter(), plot, band, index, max_hz);
                    ui.ctx().set_cursor_icon(if has_gain(band.shape) {
                        egui::CursorIcon::Move
                    } else {
                        egui::CursorIcon::ResizeHorizontal
                    });
                }
            }
            if response.hovered()
                && !response.dragged()
                && ui
                    .input(|input| input.pointer.hover_pos())
                    .and_then(|pointer| {
                        closest_band(&parameters.bands, plot, pointer, max_hz)
                    })
                    .is_none()
            {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            }
            response.on_hover_text("Drag a node left/right for frequency and up/down for gain. Shift-drag vertically to adjust Q. Double-click empty space to add a band.");
            ui.add_space(8.0);
            egui::Frame::new().fill(CANVAS).inner_margin(10).show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if let Some(band) = parameters.bands.get_mut(state.selected) {
                        let color = BAND_COLORS[state.selected];
                        ui.vertical(|ui| {
                            ui.label(RichText::new(format!("BAND {}", state.selected + 1)).monospace().strong().color(color));
                            let enabled_label = if band.enabled { "ON" } else { "OFF" };
                            ui.toggle_value(&mut band.enabled, enabled_label);
                        });
                        ui.separator();
                        ui.vertical(|ui| {
                            control_label(ui, "TYPE");
                            egui::ComboBox::from_id_salt(id.with("shape")).width(92.0).selected_text(shape_name(band.shape)).show_ui(ui, |ui| {
                                for shape in [EqShape::Bell, EqShape::LowShelf, EqShape::HighShelf, EqShape::HighPass, EqShape::LowPass, EqShape::Notch] {
                                    ui.selectable_value(&mut band.shape, shape, shape_name(shape));
                                }
                            });
                        });
                        let frequency = labeled_drag_value(ui, "FREQUENCY", &mut band.frequency_hz, 10.0..=max_hz, band_frequency_speed(original.bands.get(state.selected)).max(0.1), 1, " Hz");
                        gesture_started |= frequency.drag_started();
                        gesture_stopped |= frequency.drag_stopped();
                        let gain = ui.add_enabled_ui(has_gain(band.shape), |ui| {
                            labeled_drag_value(ui, "GAIN", &mut band.gain_db, -24.0..=24.0, 0.1, 1, " dB")
                        }).inner;
                        gesture_started |= gain.drag_started();
                        gesture_stopped |= gain.drag_stopped();
                        let q = labeled_drag_value(ui, "Q / WIDTH", &mut band.q, 0.1..=30.0, 0.01, 2, "");
                        gesture_started |= q.drag_started();
                        gesture_stopped |= q.drag_stopped();
                        if matches!(band.shape, EqShape::HighPass | EqShape::LowPass) {
                            ui.vertical(|ui| {
                                control_label(ui, "SLOPE");
                                egui::ComboBox::from_id_salt(id.with("slope")).width(88.0).selected_text(format!("{} dB/oct", slope(band.slope_db_per_octave))).show_ui(ui, |ui| {
                                    for choice in [FilterSlope::Db12, FilterSlope::Db24, FilterSlope::Db48] {
                                        ui.selectable_value(&mut band.slope_db_per_octave, choice, format!("{} dB/oct", slope(choice)));
                                    }
                                });
                            });
                        }
                        if ui.button("REMOVE BAND").on_hover_text("Remove this band and keep later band automation aligned").clicked() {
                            remove = Some(state.selected);
                        }
                    } else {
                        ui.label(RichText::new("Add a band to shape the sound").color(DIM));
                    }
                });
            });
            if !processor.enabled {
                ui.label(RichText::new("EQ BYPASSED").size(10.0).color(DIM));
            }
            let automation = self.vm.selected_parameter_automation_lanes("bands") + self.vm.selected_parameter_automation_lanes("output_gain_db");
            if automation > 0 {
                ui.label(RichText::new(format!("{automation} AUTOMATION LANE(S) · curve shows base settings")).size(10.0).color(DIM));
            }
            if changed_node { ui.ctx().request_repaint(); }
        });
        if gesture_started {
            self.vm.begin_selected_eq_edit();
        }
        if parameters != original {
            self.vm.set_selected_eq_parameters(parameters);
        }
        if gesture_stopped {
            self.vm.end_selected_eq_edit();
        }
        if let Some(index) = remove {
            self.vm.remove_selected_eq_band(index);
            state.selected = index.saturating_sub(1);
        }
        if bypass
            && let Some(index) = self
                .vm
                .effects_at(&stack)
                .iter()
                .position(|effect| effect.id == processor.id.to_string())
        {
            self.vm.toggle_processor_at(stack, index);
        }
        ui.data_mut(|data| data.insert_temp(id, state));
    }
}

fn control_label(ui: &mut egui::Ui, label: &str) {
    ui.label(RichText::new(label).monospace().size(9.0).color(DIM));
}

fn labeled_drag_value(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    speed: f64,
    decimals: usize,
    suffix: &str,
) -> egui::Response {
    ui.vertical(|ui| {
        control_label(ui, label);
        ui.add(
            egui::DragValue::new(value)
                .range(range)
                .speed(speed)
                .max_decimals(decimals)
                .suffix(suffix),
        )
    })
    .inner
}

fn paint_node_readout(
    painter: &egui::Painter,
    plot: Rect,
    band: &EqBand,
    index: usize,
    max_hz: f32,
) {
    let gain = if has_gain(band.shape) {
        format!("  {:+.1} dB", band.gain_db)
    } else {
        String::new()
    };
    let text = format!(
        "{}  {}{}  Q {:.2}",
        index + 1,
        frequency_label(band.frequency_hz),
        gain,
        band.q
    );
    let galley = painter.layout_no_wrap(text, FontId::monospace(10.0), TEXT);
    let node = node_position(band, plot, max_hz);
    let size = galley.size() + Vec2::new(12.0, 8.0);
    let x = (node.x + 12.0).min(plot.right() - size.x);
    let y = (node.y - size.y - 10.0).max(plot.top() + 4.0);
    let rect = Rect::from_min_size(Pos2::new(x.max(plot.left() + 4.0), y), size);
    painter.rect_filled(rect, 0, PANEL_ALT);
    painter.rect_stroke(
        rect,
        0,
        Stroke::new(1.0, BAND_COLORS[index]),
        StrokeKind::Inside,
    );
    painter.galley(rect.min + Vec2::new(6.0, 4.0), galley, TEXT);
}

fn has_gain(shape: EqShape) -> bool {
    matches!(
        shape,
        EqShape::Bell | EqShape::LowShelf | EqShape::HighShelf
    )
}
fn shape_short(shape: EqShape) -> &'static str {
    match shape {
        EqShape::Bell => "BELL",
        EqShape::LowShelf => "LO",
        EqShape::HighShelf => "HI",
        EqShape::HighPass => "HP",
        EqShape::LowPass => "LP",
        EqShape::Notch => "NOTCH",
    }
}
fn shape_name(shape: EqShape) -> &'static str {
    match shape {
        EqShape::Bell => "Bell",
        EqShape::LowShelf => "Low shelf",
        EqShape::HighShelf => "High shelf",
        EqShape::HighPass => "High pass",
        EqShape::LowPass => "Low pass",
        EqShape::Notch => "Notch",
    }
}
fn slope(value: FilterSlope) -> u32 {
    match value {
        FilterSlope::Db12 => 12,
        FilterSlope::Db24 => 24,
        FilterSlope::Db48 => 48,
    }
}
fn frequency_label(frequency: f32) -> String {
    if frequency >= 1000.0 {
        format!("{:.1} kHz", frequency / 1000.0)
    } else {
        format!("{frequency:.0} Hz")
    }
}
fn band_frequency_speed(band: Option<&EqBand>) -> f64 {
    band.map_or(1.0, |band| f64::from(band.frequency_hz) * 0.005)
}
fn frequency_at(rect: Rect, x: f32, max_hz: f32) -> f32 {
    10.0 * (max_hz / 10.0).powf(((x - rect.left()) / rect.width()).clamp(0.0, 1.0))
}
fn frequency_x(rect: Rect, frequency: f32, max_hz: f32) -> f32 {
    rect.left() + (frequency.clamp(10.0, max_hz) / 10.0).ln() / (max_hz / 10.0).ln() * rect.width()
}
fn gain_at(rect: Rect, y: f32) -> f32 {
    ((rect.center().y - y) / rect.height() * 2.0 * GAIN_RANGE).clamp(-GAIN_RANGE, GAIN_RANGE)
}
fn gain_y(rect: Rect, gain: f64) -> f32 {
    rect.center().y
        - gain.clamp(-f64::from(GAIN_RANGE), f64::from(GAIN_RANGE)) as f32 / (GAIN_RANGE * 2.0)
            * rect.height()
}
fn node_position(band: &EqBand, plot: Rect, max_hz: f32) -> Pos2 {
    Pos2::new(
        frequency_x(plot, band.frequency_hz, max_hz),
        gain_y(
            plot,
            if has_gain(band.shape) {
                f64::from(band.gain_db)
            } else {
                0.0
            },
        ),
    )
}
fn closest_band(bands: &[EqBand], plot: Rect, pointer: Pos2, max_hz: f32) -> Option<usize> {
    bands
        .iter()
        .enumerate()
        .map(|(index, band)| (index, node_position(band, plot, max_hz).distance(pointer)))
        .filter(|(_, distance)| *distance <= 18.0)
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(index, _)| index)
}
fn dsp_band(band: &EqBand) -> gaw_dsp::tone::EqBand {
    gaw_dsp::tone::EqBand {
        id: String::new(),
        enabled: band.enabled,
        shape: match band.shape {
            EqShape::Bell => gaw_dsp::tone::EqShape::Bell,
            EqShape::LowShelf => gaw_dsp::tone::EqShape::LowShelf,
            EqShape::HighShelf => gaw_dsp::tone::EqShape::HighShelf,
            EqShape::HighPass => gaw_dsp::tone::EqShape::HighPass,
            EqShape::LowPass => gaw_dsp::tone::EqShape::LowPass,
            EqShape::Notch => gaw_dsp::tone::EqShape::Notch,
        },
        frequency_hz: band.frequency_hz,
        gain_db: band.gain_db,
        q: band.q,
        slope_db_per_octave: slope(band.slope_db_per_octave) as f32,
    }
}

fn paint_graph(
    painter: &egui::Painter,
    rect: Rect,
    plot: Rect,
    params: &ParametricEqParameters,
    enabled: bool,
    sample_rate: u32,
    selected: Option<usize>,
) {
    painter.rect_filled(rect, 0, CANVAS);
    let detailed = selected.is_some();
    let max_hz = (sample_rate as f32 * 0.499).min(24_000.0);
    if detailed {
        for db in [-24, -12, 0, 12, 24] {
            let y = gain_y(plot, f64::from(db));
            painter.hline(
                plot.x_range(),
                y,
                Stroke::new(1.0, if db == 0 { DIM } else { BORDER }),
            );
            painter.text(
                Pos2::new(plot.left() - 6.0, y),
                Align2::RIGHT_CENTER,
                format!("{db:+}"),
                FontId::monospace(9.0),
                DIM,
            );
        }
        let sparse = plot.width() < 600.0;
        for (freq, label) in [
            (20.0, "20"),
            (50.0, "50"),
            (100.0, "100"),
            (200.0, "200"),
            (500.0, "500"),
            (1000.0, "1k"),
            (2000.0, "2k"),
            (5000.0, "5k"),
            (10000.0, "10k"),
            (20000.0, "20k"),
        ] {
            if freq > max_hz {
                continue;
            }
            if sparse && matches!(freq as u32, 50 | 200 | 1000 | 5000 | 20000) {
                continue;
            }
            let x = frequency_x(plot, freq, max_hz);
            painter.vline(x, plot.y_range(), Stroke::new(1.0, BORDER));
            painter.text(
                Pos2::new(x, plot.bottom() + 6.0),
                Align2::CENTER_TOP,
                label,
                FontId::monospace(9.0),
                DIM,
            );
        }
        painter.text(
            Pos2::new(rect.right() - 3.0, plot.bottom() + 6.0),
            Align2::RIGHT_TOP,
            "Hz",
            FontId::monospace(9.0),
            DIM,
        );
        painter.text(
            Pos2::new(rect.right() - 3.0, plot.top()),
            Align2::RIGHT_TOP,
            "dB",
            FontId::monospace(9.0),
            DIM,
        );
    } else {
        painter.hline(plot.x_range(), plot.center().y, Stroke::new(1.0, BORDER));
    }
    let clipped = painter.with_clip_rect(plot);
    let samples = (plot.width() as usize).clamp(80, 480);
    let mut total = vec![
        if enabled {
            f64::from(params.output_gain_db)
        } else {
            0.0
        };
        samples + 1
    ];
    for (index, band) in params.bands.iter().enumerate().take(8) {
        let dsp = dsp_band(band);
        if band.enabled && enabled {
            let points: Vec<_> = (0..=samples)
                .map(|i| {
                    let x = plot.left() + i as f32 / samples as f32 * plot.width();
                    let db = dsp.response_db(
                        f64::from(frequency_at(plot, x, max_hz)),
                        f64::from(sample_rate),
                    );
                    total[i] += db;
                    Pos2::new(x, gain_y(plot, db))
                })
                .collect();
            if selected == Some(index) {
                let mut mesh = egui::Mesh::default();
                for point in &points {
                    mesh.colored_vertex(
                        Pos2::new(point.x, plot.center().y),
                        BAND_COLORS[index].gamma_multiply(0.10),
                    );
                    mesh.colored_vertex(*point, BAND_COLORS[index].gamma_multiply(0.22));
                }
                for i in 0..samples as u32 {
                    let v = i * 2;
                    mesh.add_triangle(v, v + 1, v + 2);
                    mesh.add_triangle(v + 1, v + 2, v + 3);
                }
                clipped.add(mesh);
            }
            if detailed {
                clipped.add(egui::Shape::line(
                    points,
                    Stroke::new(1.0, BAND_COLORS[index].gamma_multiply(0.65)),
                ));
            }
        }
    }
    let points = total
        .iter()
        .enumerate()
        .map(|(i, db)| {
            Pos2::new(
                plot.left() + i as f32 / samples as f32 * plot.width(),
                gain_y(plot, *db),
            )
        })
        .collect();
    clipped.add(egui::Shape::line(
        points,
        Stroke::new(
            if detailed { 2.0 } else { 1.2 },
            if enabled { TEXT } else { DIM },
        ),
    ));
    if detailed {
        for (index, band) in params.bands.iter().enumerate().take(8) {
            let point = node_position(band, plot, max_hz);
            let color = BAND_COLORS[index];
            if selected == Some(index) {
                painter.circle_stroke(point, 11.0, Stroke::new(1.0, color));
            }
            painter.circle(
                point,
                7.5,
                if band.enabled && enabled {
                    color
                } else {
                    CANVAS
                },
                Stroke::new(1.5, color),
            );
            painter.text(
                point,
                Align2::CENTER_CENTER,
                (index + 1).to_string(),
                FontId::monospace(10.0),
                if band.enabled && enabled {
                    CANVAS
                } else {
                    color
                },
            );
        }
    }
    painter.rect_stroke(plot, 0, Stroke::new(1.0, BORDER), StrokeKind::Inside);
}

pub(super) fn eq_thumbnail(
    ui: &mut egui::Ui,
    bands: &[EqBand],
    gain_db: f32,
    enabled: bool,
    sample_rate: u32,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 32.0), Sense::click());
    paint_graph(
        ui.painter(),
        rect,
        rect.shrink(3.0),
        &ParametricEqParameters {
            bands: bands.to_vec(),
            output_gain_db: gain_db,
        },
        enabled,
        sample_rate,
        None,
    );
    response.on_hover_text("Open graphical EQ")
}
