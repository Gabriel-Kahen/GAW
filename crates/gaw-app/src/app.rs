// Pixel, beat, and display-counter conversions are bounded by the visible demo canvas.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::time::Duration;

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, Pos2, Rect, RichText,
    Sense, Stroke, StrokeKind, Vec2,
};

use crate::model::{ClipKind, DemoViewModel, EditorKind, Intent, RenderState, Selection};
use crate::timeline::{TimelineState, paint_waveform, timeline};

const CANVAS: Color32 = Color32::from_rgb(13, 16, 21);
const PANEL: Color32 = Color32::from_rgb(20, 24, 31);
const PANEL_ALT: Color32 = Color32::from_rgb(25, 30, 39);
const BORDER: Color32 = Color32::from_rgb(44, 51, 64);
const TEXT: Color32 = Color32::from_rgb(230, 233, 239);
const DIM: Color32 = Color32::from_rgb(143, 153, 171);
const CYAN: Color32 = Color32::from_rgb(75, 209, 225);
const PURPLE: Color32 = Color32::from_rgb(172, 122, 239);
const ORANGE: Color32 = Color32::from_rgb(239, 151, 68);
const TRANSPORT_HEIGHT: f32 = 82.0;
const EDITOR_DEFAULT_HEIGHT: f32 = 210.0;
const EDITOR_MIN_HEIGHT: f32 = 150.0;
const EDITOR_MAX_HEIGHT: f32 = 340.0;
const ASSET_PANEL_WIDTH: f32 = 220.0;
const INSPECTOR_WIDTH: f32 = 286.0;

#[derive(Debug)]
pub struct GawApp {
    vm: DemoViewModel,
    timeline: TimelineState,
    last_time: Option<f64>,
}

impl GawApp {
    pub fn new(context: &eframe::CreationContext<'_>) -> Self {
        configure_style(&context.egui_ctx);
        Self {
            vm: DemoViewModel::demo(),
            timeline: TimelineState::default(),
            last_time: None,
        }
    }

    fn handle_keyboard(&mut self, context: &egui::Context, now: f64) {
        if context.text_edit_focused() {
            return;
        }
        let mut action = None;
        context.input_mut(|input| {
            if input.consume_key(egui::Modifiers::NONE, egui::Key::Space) {
                action = Some(Intent::TogglePlayback);
            } else if input.consume_key(egui::Modifiers::NONE, egui::Key::Home) {
                action = Some(Intent::Stop);
            } else if input.consume_key(egui::Modifiers::NONE, egui::Key::Backspace) {
                action = Some(Intent::Back);
            } else if input.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                action = Some(Intent::ClearSelection);
            } else if input.consume_key(egui::Modifiers::NONE, egui::Key::Enter) {
                if let Selection::Clip { track, clip } = self.vm.selection {
                    action = Some(Intent::EnterChild { track, clip });
                }
            } else if input.consume_key(egui::Modifiers::NONE, egui::Key::L) {
                action = Some(Intent::ToggleStructureLens);
            } else if input.consume_key(egui::Modifiers::COMMAND, egui::Key::R) {
                action = Some(Intent::SimulateAgentChange(now));
            }
        });
        if context
            .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::OpenBracket))
        {
            self.timeline.zoom_by(0.84);
        }
        if context
            .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::CloseBracket))
        {
            self.timeline.zoom_by(1.18);
        }
        if let Some(action) = action {
            self.vm.apply(action);
        }
    }

    fn transport_bar(&mut self, ui: &mut egui::Ui, now: f64) {
        egui::Frame::new()
            .fill(PANEL)
            .inner_margin(Margin::symmetric(14, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let back = ui.add_enabled(self.vm.can_navigate_back(), icon_button("‹", false));
                    if back.clicked() {
                        self.vm.apply(Intent::Back);
                    }
                    let breadcrumbs: Vec<_> = self
                        .vm
                        .breadcrumbs()
                        .enumerate()
                        .map(|(depth, composition)| (depth, composition.name.clone()))
                        .collect();
                    for (index, (depth, name)) in breadcrumbs.iter().enumerate() {
                        if index > 0 {
                            ui.label(RichText::new("/").color(DIM));
                        }
                        if ui
                            .add(
                                egui::Button::new(RichText::new(name).color(
                                    if index + 1 == breadcrumbs.len() {
                                        TEXT
                                    } else {
                                        DIM
                                    },
                                ))
                                .frame(false),
                            )
                            .clicked()
                        {
                            self.vm.apply(Intent::NavigateToDepth(*depth));
                        }
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("AGENT PULSE")
                                        .monospace()
                                        .size(9.0)
                                        .color(CYAN),
                                )
                                .fill(PANEL_ALT),
                            )
                            .on_hover_text("Simulate an external agent edit (Ctrl/Cmd+R)")
                            .clicked()
                        {
                            self.vm.apply(Intent::SimulateAgentChange(now));
                        }
                        let lens_text = if self.vm.structure_lens {
                            "STRUCTURE ON"
                        } else {
                            "STRUCTURE"
                        };
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(lens_text)
                                        .monospace()
                                        .size(9.0)
                                        .color(if self.vm.structure_lens { CYAN } else { DIM }),
                                )
                                .fill(PANEL_ALT),
                            )
                            .clicked()
                        {
                            self.vm.apply(Intent::ToggleStructureLens);
                        }
                    });
                });
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    if ui
                        .add(icon_button("■", false))
                        .on_hover_text("Stop · Home")
                        .clicked()
                    {
                        self.vm.apply(Intent::Stop);
                    }
                    let play_icon = if self.vm.transport.playing {
                        "Ⅱ"
                    } else {
                        "▶"
                    };
                    if ui
                        .add(icon_button(play_icon, self.vm.transport.playing))
                        .on_hover_text("Play / pause · Space")
                        .clicked()
                    {
                        self.vm.apply(Intent::TogglePlayback);
                    }
                    if ui
                        .add(icon_button("●", self.vm.transport.recording))
                        .on_hover_text("Record")
                        .clicked()
                    {
                        self.vm.apply(Intent::ToggleRecording);
                    }
                    if ui
                        .add(icon_button("↻", self.vm.transport.loop_enabled))
                        .on_hover_text("Loop")
                        .clicked()
                    {
                        self.vm.apply(Intent::ToggleLoop);
                    }
                    ui.add_space(12.0);
                    let beat = self.vm.transport.playhead;
                    ui.label(
                        RichText::new(format_position(beat))
                            .monospace()
                            .size(17.0)
                            .color(TEXT),
                    );
                    ui.add_space(18.0);
                    ui.label(RichText::new("BPM").monospace().size(9.0).color(DIM));
                    let mut bpm = self.vm.transport.bpm;
                    if ui
                        .add(
                            egui::DragValue::new(&mut bpm)
                                .range(40.0..=240.0)
                                .speed(0.2)
                                .fixed_decimals(1),
                        )
                        .changed()
                    {
                        self.vm.apply(Intent::SetBpm(bpm));
                    }
                    ui.add_space(18.0);
                    ui.label(RichText::new("4 / 4").monospace().size(10.0).color(DIM));
                    ui.add_space(18.0);
                    ui.label(
                        RichText::new("48 kHz · 128")
                            .monospace()
                            .size(9.0)
                            .color(DIM),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "ZOOM {:>3.0}%",
                                self.timeline.pixels_per_beat / 32.0 * 100.0
                            ))
                            .monospace()
                            .size(9.0)
                            .color(DIM),
                        );
                        if ui.small_button("+").clicked() {
                            self.timeline.zoom_by(1.18);
                        }
                        if ui.small_button("−").clicked() {
                            self.timeline.zoom_by(0.84);
                        }
                    });
                });
            });
    }

    fn asset_browser(&mut self, ui: &mut egui::Ui, now: f64) {
        panel_title(ui, "ASSETS", "05 sources");
        ui.add(
            egui::TextEdit::singleline(&mut String::new())
                .hint_text("⌕  Filter assets")
                .desired_width(f32::INFINITY)
                .interactive(false),
        );
        ui.add_space(7.0);
        egui::ScrollArea::vertical()
            .id_salt("assets")
            .show(ui, |ui| {
                let mut selected_asset = None;
                for (index, asset) in self.vm.assets.iter().enumerate() {
                    let selected = self.vm.selection == Selection::Asset(index);
                    let (rect, response) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), 68.0),
                        Sense::click_and_drag(),
                    );
                    let fill = if selected {
                        Color32::from_rgb(30, 48, 57)
                    } else {
                        PANEL_ALT
                    };
                    ui.painter().rect_filled(rect, CornerRadius::same(5), fill);
                    ui.painter().rect_stroke(
                        rect,
                        CornerRadius::same(5),
                        Stroke::new(
                            1.0,
                            if selected {
                                CYAN.gamma_multiply(0.7)
                            } else {
                                BORDER
                            },
                        ),
                        StrokeKind::Inside,
                    );
                    let wave_rect = Rect::from_min_size(
                        rect.left_top() + Vec2::new(8.0, 10.0),
                        Vec2::new(54.0, 42.0),
                    );
                    paint_waveform(
                        ui.painter(),
                        wave_rect,
                        &asset.waveform,
                        CYAN.gamma_multiply(0.8),
                    );
                    ui.painter().text(
                        rect.left_top() + Vec2::new(70.0, 9.0),
                        Align2::LEFT_TOP,
                        &asset.name,
                        FontId::proportional(11.5),
                        TEXT,
                    );
                    let channels = if asset.channels == 1 {
                        "MONO"
                    } else {
                        "STEREO"
                    };
                    ui.painter().text(
                        rect.left_top() + Vec2::new(70.0, 29.0),
                        Align2::LEFT_TOP,
                        format!("{:.2}s  ·  {channels}", asset.duration_seconds),
                        FontId::monospace(8.5),
                        DIM,
                    );
                    if let Some(bpm) = asset.bpm {
                        ui.painter().text(
                            rect.left_top() + Vec2::new(70.0, 45.0),
                            Align2::LEFT_TOP,
                            format!("{bpm:.0} BPM  SYNC READY"),
                            FontId::monospace(8.2),
                            Color32::from_rgb(142, 220, 206),
                        );
                    } else {
                        ui.painter().text(
                            rect.left_top() + Vec2::new(70.0, 45.0),
                            Align2::LEFT_TOP,
                            "ONE-SHOT",
                            FontId::monospace(8.2),
                            DIM,
                        );
                    }
                    let alpha = if asset.changed_by_agent {
                        self.vm.highlight_alpha(&asset.id, now)
                    } else {
                        0.0
                    };
                    if alpha > 0.0 {
                        ui.painter().rect_stroke(
                            rect.expand(1.0),
                            CornerRadius::same(6),
                            Stroke::new(1.5, CYAN.gamma_multiply(alpha)),
                            StrokeKind::Outside,
                        );
                    }
                    if response.clicked() {
                        selected_asset = Some(index);
                    }
                    if response.drag_started() {
                        self.timeline.dragging_asset = Some(index);
                    }
                    response.on_hover_text("Drag onto the arrangement to create an audio clip");
                    ui.add_space(5.0);
                }
                if let Some(index) = selected_asset {
                    self.vm.apply(Intent::Select(Selection::Asset(index)));
                }
            });
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        panel_title(ui, "SIGNAL", "top → bottom");
        let selection = self.vm.selection;
        match selection {
            Selection::None => Self::empty_inspector(ui),
            Selection::Asset(index) => self.asset_inspector(ui, index),
            Selection::Sampler { track } => self.sampler_inspector(ui, track),
            Selection::Clip { track, clip } | Selection::Effect { track, clip, .. } => {
                self.clip_inspector(ui, track, clip);
            }
        }
    }

    fn empty_inspector(ui: &mut egui::Ui) {
        ui.add_space(30.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("NO SELECTION")
                    .monospace()
                    .size(10.0)
                    .color(DIM),
            );
            ui.label(
                RichText::new("Select a clip, asset, or effect")
                    .size(11.0)
                    .color(DIM),
            );
        });
    }

    fn asset_inspector(&self, ui: &mut egui::Ui, index: usize) {
        let Some(asset) = self.vm.assets.get(index) else {
            return;
        };
        signal_node(ui, 1, "SOURCE ASSET", &asset.name, CYAN, true);
        property(ui, "Stable ID", &asset.id);
        if self.vm.structure_lens {
            property(ui, "JSON", &format!("assets/index.json#/{}", asset.id));
        }
        property(ui, "Media", "immutable / content-addressed");
        property(
            ui,
            "Layout",
            if asset.channels == 1 {
                "mono"
            } else {
                "stereo"
            },
        );
        if let Some(bpm) = asset.bpm {
            property(ui, "Asset tempo", &format!("{bpm:.1} BPM"));
        }
    }

    fn sampler_inspector(&self, ui: &mut egui::Ui, track: usize) {
        let name = self
            .vm
            .current_composition()
            .tracks
            .get(track)
            .map_or("Event track", |track| track.name.as_str());
        signal_node(ui, 1, "EVENT STREAM", name, PURPLE, true);
        if self.vm.structure_lens
            && let Some(track) = self.vm.current_composition().tracks.get(track)
        {
            property(ui, "Track ID", &track.id);
            property(
                ui,
                "JSON",
                &format!(
                    "compositions/{}/tracks/{}.json",
                    self.vm.current_composition().id,
                    track.id
                ),
            );
        }
        connector(ui);
        signal_node(ui, 2, "INSTRUMENT", "Slice Sampler", PURPLE, true);
        property(ui, "Polyphony", "12 voices");
        property(ui, "Mode", "one-shot · choke groups");
        connector(ui);
        signal_node(ui, 3, "TRACK OUTPUT", "stereo", CYAN, true);
    }

    fn clip_inspector(&mut self, ui: &mut egui::Ui, track_index: usize, clip_index: usize) {
        let Some(clip) = self
            .vm
            .current_composition()
            .tracks
            .get(track_index)
            .and_then(|track| track.clips.get(clip_index))
        else {
            return;
        };
        let source_label = match clip.kind {
            ClipKind::Audio { .. } => "AUDIO ASSET",
            ClipKind::Event { .. } => "EVENT DATA",
            ClipKind::Composition { .. } => "CHILD OUTPUT",
        };
        let source_color = match clip.kind {
            ClipKind::Audio { .. } => CYAN,
            ClipKind::Event { .. } => PURPLE,
            ClipKind::Composition { .. } => ORANGE,
        };
        let clip_name = clip.name.clone();
        let clip_id = clip.id.clone();
        let track_name = self.vm.current_composition().tracks[track_index]
            .name
            .clone();
        let composition_name = self.vm.current_composition().name.clone();
        let track_effects = self.vm.current_composition().tracks[track_index]
            .effects
            .clone();
        let output_effects = self.vm.current_composition().output_effects.clone();
        let gain_db = clip.gain_db;
        let kind = clip.kind.clone();
        let effects = clip.effects.clone();
        signal_node(ui, 1, source_label, &clip_name, source_color, true);
        if self.vm.structure_lens {
            property(ui, "ID", &clip_id);
            let track_id = &self.vm.current_composition().tracks[track_index].id;
            property(
                ui,
                "JSON",
                &format!(
                    "compositions/{}/tracks/{track_id}.json#/clips/{clip_id}",
                    self.vm.current_composition().id,
                ),
            );
        }
        connector(ui);
        match kind {
            ClipKind::Audio {
                asset,
                sync,
                source_bpm,
            } => {
                signal_node(
                    ui,
                    2,
                    "PLAYBACK TRANSFORMS",
                    "Source range → Reverse → Sync → Fades",
                    CYAN,
                    true,
                );
                property(ui, "Source range", "0.00s → 4.36s");
                if let Some(asset) = self.vm.assets.get(asset) {
                    property(ui, "Asset", &asset.id);
                }
                property(
                    ui,
                    "Reverse",
                    if clip_id == "clip_vocal" { "on" } else { "off" },
                );
                if let Some(source_bpm) = source_bpm {
                    property(
                        ui,
                        "Tempo",
                        &format!(
                            "{source_bpm:.0} → {:.0} {}",
                            self.vm.transport.bpm,
                            sync.label()
                        ),
                    );
                }
                property(ui, "Fades", "12ms in · 42ms out");
            }
            ClipKind::Event { .. } => {
                signal_node(ui, 2, "INSTRUMENT", "Slice Sampler", PURPLE, true);
                if ui.button("Open sampler zones").clicked() {
                    self.vm
                        .apply(Intent::Select(Selection::Sampler { track: track_index }));
                }
            }
            ClipKind::Composition { child, render, .. } => {
                let child_name = &self.vm.compositions[child].name;
                signal_node(
                    ui,
                    2,
                    "PARENT PLACEMENT",
                    "Mute → Gain → Fades",
                    ORANGE,
                    true,
                );
                property(ui, "Child", child_name);
                property(
                    ui,
                    "Render",
                    match render {
                        RenderState::Fresh => "current",
                        RenderState::Stale => "stale · last render playing",
                        RenderState::Rendering(_) => "rendering in background",
                    },
                );
                ui.label(
                    RichText::new("Child internals are edited inside the composition.")
                        .size(9.5)
                        .color(DIM),
                );
            }
        }
        property(ui, "Clip gain", &format!("{gain_db:+.1} dB"));
        if !effects.is_empty() {
            connector(ui);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("CLIP EFFECTS")
                        .monospace()
                        .size(9.0)
                        .color(DIM),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.small_button("+")
                        .on_hover_text("Insert processor (demo)");
                });
            });
        }
        for (effect_index, effect) in effects.iter().enumerate() {
            let selected = matches!(self.vm.selection, Selection::Effect { track, clip, effect } if track == track_index && clip == clip_index && effect == effect_index);
            let response = signal_node(
                ui,
                effect_index + 3,
                &effect.kind,
                &effect.name,
                CYAN,
                effect.enabled,
            );
            if response.clicked() {
                self.vm.apply(Intent::Select(Selection::Effect {
                    track: track_index,
                    clip: clip_index,
                    effect: effect_index,
                }));
            }
            ui.horizontal(|ui| {
                if ui
                    .small_button(if effect.enabled { "ON" } else { "OFF" })
                    .clicked()
                {
                    self.vm.apply(Intent::ToggleEffect {
                        track: track_index,
                        clip: clip_index,
                        effect: effect_index,
                    });
                }
                if ui
                    .add_enabled(effect_index > 0, egui::Button::new("↑").small())
                    .clicked()
                {
                    self.vm.apply(Intent::MoveEffect {
                        track: track_index,
                        clip: clip_index,
                        effect: effect_index,
                        delta: -1,
                    });
                }
                if ui
                    .add_enabled(
                        effect_index + 1 < effects.len(),
                        egui::Button::new("↓").small(),
                    )
                    .clicked()
                {
                    self.vm.apply(Intent::MoveEffect {
                        track: track_index,
                        clip: clip_index,
                        effect: effect_index,
                        delta: 1,
                    });
                }
                if selected {
                    ui.label(RichText::new("EDITING").monospace().size(8.0).color(CYAN));
                }
            });
            if self.vm.structure_lens {
                property(ui, "Processor ID", &effect.id);
            }
            if effect_index + 1 < effects.len() {
                connector(ui);
            }
        }
        connector(ui);
        signal_node(
            ui,
            effects.len() + 3,
            "TRACK MIX + STACK",
            &track_name,
            CYAN,
            true,
        );
        property(ui, "Order", "clip sum → track processors");
        for (index, effect) in track_effects.iter().enumerate() {
            connector(ui);
            signal_node(
                ui,
                effects.len() + 4 + index,
                "TRACK EFFECT",
                &effect.name,
                CYAN,
                effect.enabled,
            );
            if self.vm.structure_lens {
                property(ui, "Processor ID", &effect.id);
            }
        }
        connector(ui);
        signal_node(
            ui,
            effects.len() + track_effects.len() + 4,
            "COMPOSITION OUTPUT",
            &composition_name,
            ORANGE,
            true,
        );
        property(ui, "Order", "track sum → output stack");
        for (index, effect) in output_effects.iter().enumerate() {
            connector(ui);
            signal_node(
                ui,
                effects.len() + track_effects.len() + 5 + index,
                "OUTPUT EFFECT",
                &effect.name,
                ORANGE,
                effect.enabled,
            );
            if self.vm.structure_lens {
                property(ui, "Processor ID", &effect.id);
            }
        }
    }

    fn context_editor(&mut self, ui: &mut egui::Ui) {
        match self.vm.editor_kind() {
            EditorKind::Overview => self.overview_editor(ui),
            EditorKind::Waveform => self.waveform_editor(ui),
            EditorKind::PianoRoll => self.piano_roll_editor(ui),
            EditorKind::Sampler => self.sampler_editor(ui),
            EditorKind::Effect => self.effect_editor(ui),
        }
    }

    fn overview_editor(&self, ui: &mut egui::Ui) {
        panel_title(ui, "PROJECT OVERVIEW", "select something to edit");
        ui.horizontal(|ui| {
            metric(
                ui,
                "COMPOSITIONS",
                &self.vm.compositions.len().to_string(),
                ORANGE,
            );
            metric(ui, "ASSETS", &self.vm.assets.len().to_string(), CYAN);
            metric(
                ui,
                "TRACKS HERE",
                &self.vm.current_composition().tracks.len().to_string(),
                PURPLE,
            );
            metric(
                ui,
                "SAMPLE RATE",
                "48 kHz",
                Color32::from_rgb(134, 207, 124),
            );
        });
    }

    fn waveform_editor(&self, ui: &mut egui::Ui) {
        let (name, waveform, info) = match self.vm.selection {
            Selection::Asset(index) => self.vm.assets.get(index).map(|asset| {
                (
                    asset.name.as_str(),
                    asset.waveform.as_ref(),
                    "ASSET BPM · FIRST BEAT · SOURCE RANGE",
                )
            }),
            _ => self.vm.selected_clip().map(|(_, _, clip)| {
                (
                    clip.name.as_str(),
                    clip.waveform.as_ref(),
                    "TRIM · CHOP · FADE · REVERSE",
                )
            }),
        }
        .unwrap_or(("Waveform", &[], "SOURCE"));
        panel_title(ui, "WAVEFORM", name);
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), ui.available_height().max(90.0)),
            Sense::click_and_drag(),
        );
        ui.painter().rect_filled(rect, 4.0, CANVAS);
        let waveform_rect = rect.shrink2(Vec2::new(14.0, 26.0));
        paint_waveform(ui.painter(), waveform_rect, waveform, CYAN);
        ui.painter().hline(
            waveform_rect.x_range(),
            waveform_rect.center().y,
            Stroke::new(0.5, BORDER),
        );
        for fraction in [0.18, 0.47, 0.72] {
            let x = egui::lerp(waveform_rect.x_range(), fraction);
            ui.painter()
                .vline(x, waveform_rect.y_range(), Stroke::new(1.0, ORANGE));
            ui.painter()
                .circle_filled(Pos2::new(x, waveform_rect.top()), 3.0, ORANGE);
        }
        ui.painter().text(
            rect.left_top() + Vec2::new(12.0, 8.0),
            Align2::LEFT_TOP,
            info,
            FontId::monospace(8.5),
            DIM,
        );
        ui.painter().text(
            rect.right_top() + Vec2::new(-12.0, 8.0),
            Align2::RIGHT_TOP,
            "SNAP 1/16",
            FontId::monospace(8.5),
            DIM,
        );
    }

    fn piano_roll_editor(&self, ui: &mut egui::Ui) {
        let Some((_, _, clip)) = self.vm.selected_clip() else {
            return;
        };
        let ClipKind::Event { notes } = &clip.kind else {
            return;
        };
        panel_title(ui, "PIANO ROLL", &clip.name);
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), ui.available_height().max(90.0)),
            Sense::click_and_drag(),
        );
        ui.painter().rect_filled(rect, 4.0, CANVAS);
        let keys_width = 54.0;
        let grid = Rect::from_min_max(
            Pos2::new(rect.left() + keys_width, rect.top()),
            rect.right_bottom(),
        );
        for row in 0..12 {
            let y = grid.top() + row as f32 / 12.0 * grid.height();
            ui.painter()
                .hline(rect.x_range(), y, Stroke::new(0.5, BORDER));
            if row % 2 == 0 {
                ui.painter().text(
                    Pos2::new(rect.left() + 8.0, y + 6.0),
                    Align2::LEFT_CENTER,
                    format!("C{}", 6 - row / 2),
                    FontId::monospace(8.0),
                    DIM,
                );
            }
        }
        for beat in 0..=clip.length as u32 {
            let x = grid.left() + beat as f32 / clip.length * grid.width();
            ui.painter().vline(
                x,
                grid.y_range(),
                Stroke::new(if beat % 4 == 0 { 1.0 } else { 0.5 }, BORDER),
            );
        }
        for note in notes.iter() {
            let x = grid.left() + note.start / clip.length * grid.width();
            let width = (note.length / clip.length * grid.width()).max(4.0);
            let y = grid.bottom() - f32::from(note.pitch.saturating_sub(48)) / 36.0 * grid.height();
            ui.painter().rect_filled(
                Rect::from_min_size(Pos2::new(x, y - 5.0), Vec2::new(width, 8.0)),
                2.0,
                PURPLE.gamma_multiply(0.65 + note.velocity * 0.3),
            );
        }
    }

    fn sampler_editor(&self, ui: &mut egui::Ui) {
        panel_title(ui, "SLICE SAMPLER", "Vocal Air · 12 voices · one-shot");
        ui.horizontal(|ui| {
            let (wave_rect, _) = ui.allocate_exact_size(
                Vec2::new(
                    (ui.available_width() * 0.48).max(240.0),
                    ui.available_height().max(100.0),
                ),
                Sense::click(),
            );
            ui.painter().rect_filled(wave_rect, 4.0, CANVAS);
            paint_waveform(
                ui.painter(),
                wave_rect.shrink(12.0),
                &self.vm.assets[2].waveform,
                CYAN,
            );
            for index in 1..8 {
                let x = wave_rect.left() + index as f32 / 8.0 * wave_rect.width();
                ui.painter().vline(
                    x,
                    wave_rect.y_range(),
                    Stroke::new(1.0, ORANGE.gamma_multiply(0.8)),
                );
            }
            let keyboard = ui.available_rect_before_wrap();
            for key in 0..12 {
                let width = keyboard.width() / 12.0;
                let rect = Rect::from_min_size(
                    Pos2::new(keyboard.left() + key as f32 * width, keyboard.top()),
                    Vec2::new(width - 2.0, keyboard.height().min(122.0)),
                );
                let active = matches!(key, 0 | 3 | 7 | 10);
                ui.painter().rect_filled(
                    rect,
                    3.0,
                    if active {
                        PURPLE.gamma_multiply(0.48)
                    } else {
                        Color32::from_rgb(222, 224, 226)
                    },
                );
                ui.painter().text(
                    rect.center_bottom() - Vec2::new(0.0, 8.0),
                    Align2::CENTER_BOTTOM,
                    format!("{}", 48 + key),
                    FontId::monospace(8.0),
                    if active {
                        TEXT
                    } else {
                        Color32::from_rgb(35, 39, 45)
                    },
                );
            }
            ui.allocate_space(keyboard.size());
        });
    }

    fn effect_editor(&mut self, ui: &mut egui::Ui) {
        let Selection::Effect {
            track,
            clip,
            effect,
        } = self.vm.selection
        else {
            return;
        };
        let Some(current) = self
            .vm
            .current_composition()
            .tracks
            .get(track)
            .and_then(|track| track.clips.get(clip))
            .and_then(|clip| clip.effects.get(effect))
            .cloned()
        else {
            return;
        };
        panel_title(ui, &current.kind.to_uppercase(), &current.name);
        ui.horizontal_wrapped(|ui| {
            for (parameter_index, parameter) in current.parameters.iter().enumerate() {
                egui::Frame::new()
                    .fill(CANVAS)
                    .corner_radius(6)
                    .inner_margin(12)
                    .show(ui, |ui| {
                        ui.set_width(152.0);
                        ui.label(
                            RichText::new(&parameter.label)
                                .monospace()
                                .size(9.0)
                                .color(DIM),
                        );
                        let mut value = parameter.value;
                        if ui
                            .add(
                                egui::Slider::new(&mut value, parameter.min..=parameter.max)
                                    .show_value(false),
                            )
                            .changed()
                        {
                            self.vm.apply(Intent::SetEffectParameter {
                                track,
                                clip,
                                effect,
                                parameter: parameter_index,
                                value,
                            });
                        }
                        ui.label(
                            RichText::new(format!("{value:.2} {}", parameter.unit))
                                .size(18.0)
                                .color(TEXT),
                        );
                        if self.vm.structure_lens {
                            ui.label(
                                RichText::new(&parameter.id)
                                    .monospace()
                                    .size(8.0)
                                    .color(CYAN),
                            );
                        }
                    });
            }
        });
    }
}

impl eframe::App for GawApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        let now = context.input(|input| input.time);
        let delta = self
            .last_time
            .map_or(0.0, |last| (now - last).clamp(0.0, 0.1) as f32);
        self.last_time = Some(now);
        self.vm.advance(delta);
        self.handle_keyboard(context, now);
        if self.vm.transport.playing || self.vm.has_active_highlights(now) {
            context.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        let now = context.input(|input| input.time);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(CANVAS))
            .show_inside(ui, |ui| {
                egui::Panel::top("transport")
                    .exact_size(TRANSPORT_HEIGHT)
                    .frame(
                        egui::Frame::new()
                            .fill(PANEL)
                            .stroke(Stroke::new(1.0, BORDER)),
                    )
                    .show_inside(ui, |ui| self.transport_bar(ui, now));
                egui::Panel::bottom("context_editor")
                    .resizable(true)
                    .default_size(EDITOR_DEFAULT_HEIGHT)
                    .size_range(EDITOR_MIN_HEIGHT..=EDITOR_MAX_HEIGHT)
                    .frame(
                        egui::Frame::new()
                            .fill(PANEL)
                            .stroke(Stroke::new(1.0, BORDER))
                            .inner_margin(10),
                    )
                    .show_inside(ui, |ui| self.context_editor(ui));
                egui::Panel::left("asset_browser")
                    .resizable(true)
                    .default_size(ASSET_PANEL_WIDTH)
                    .size_range(190.0..=310.0)
                    .frame(
                        egui::Frame::new()
                            .fill(PANEL)
                            .stroke(Stroke::new(1.0, BORDER))
                            .inner_margin(10),
                    )
                    .show_inside(ui, |ui| self.asset_browser(ui, now));
                egui::Panel::right("inspector")
                    .resizable(true)
                    .default_size(INSPECTOR_WIDTH)
                    .size_range(250.0..=380.0)
                    .frame(
                        egui::Frame::new()
                            .fill(PANEL)
                            .stroke(Stroke::new(1.0, BORDER))
                            .inner_margin(10),
                    )
                    .show_inside(ui, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| self.inspector(ui))
                    });
                for action in timeline(ui, &self.vm, &mut self.timeline, now) {
                    self.vm.apply(action);
                }
            });

        if self.vm.transport.playing || self.vm.has_active_highlights(now) {
            context.request_repaint_after(Duration::from_millis(16));
        }
    }
}

fn configure_style(context: &egui::Context) {
    let mut style = (*context.global_style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = PANEL;
    style.visuals.window_fill = PANEL;
    style.visuals.extreme_bg_color = CANVAS;
    style.visuals.faint_bg_color = PANEL_ALT;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    style.visuals.widgets.inactive.bg_fill = PANEL_ALT;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(35, 42, 53);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, CYAN.gamma_multiply(0.7));
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(37, 53, 63);
    style.visuals.selection.bg_fill = CYAN.gamma_multiply(0.36);
    style.spacing.item_spacing = Vec2::new(7.0, 7.0);
    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::new(11.0, FontFamily::Proportional),
    );
    context.set_global_style(style);
}

fn icon_button(text: &'static str, active: bool) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).size(13.0).color(if active {
        Color32::WHITE
    } else {
        DIM
    }))
    .fill(if active {
        CYAN.gamma_multiply(0.42)
    } else {
        PANEL_ALT
    })
    .min_size(Vec2::splat(29.0))
}

fn format_position(beat: f32) -> String {
    let bar = (beat / 4.0).floor() as u32 + 1;
    let in_bar = beat.rem_euclid(4.0);
    let beat_number = in_bar.floor() as u32 + 1;
    let ticks = (in_bar.fract() * 960.0).floor() as u32;
    format!("{bar:03} · {beat_number} · {ticks:03}")
}

fn panel_title(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(title)
                .monospace()
                .size(10.0)
                .strong()
                .color(TEXT),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(detail).monospace().size(8.5).color(DIM));
        });
    });
    ui.separator();
}

fn signal_node(
    ui: &mut egui::Ui,
    order: usize,
    kind: &str,
    name: &str,
    color: Color32,
    enabled: bool,
) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 49.0), Sense::click());
    let fill = if enabled {
        color.gamma_multiply(0.14)
    } else {
        PANEL_ALT
    };
    ui.painter().rect_filled(rect, CornerRadius::same(5), fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::same(5),
        Stroke::new(
            1.0,
            if enabled {
                color.gamma_multiply(0.6)
            } else {
                BORDER
            },
        ),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.left_center() + Vec2::new(10.0, 0.0),
        Align2::LEFT_CENTER,
        format!("{order:02}"),
        FontId::monospace(9.0),
        color,
    );
    ui.painter().text(
        rect.left_top() + Vec2::new(38.0, 8.0),
        Align2::LEFT_TOP,
        kind.to_uppercase(),
        FontId::monospace(8.0),
        DIM,
    );
    ui.painter().text(
        rect.left_bottom() + Vec2::new(38.0, -8.0),
        Align2::LEFT_BOTTOM,
        name,
        FontId::proportional(11.0),
        if enabled { TEXT } else { DIM },
    );
    response
}

fn connector(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 10.0), Sense::hover());
    ui.painter()
        .vline(rect.center().x, rect.y_range(), Stroke::new(1.0, BORDER));
}

fn property(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(9.5).color(DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).monospace().size(8.8).color(TEXT));
        });
    });
}

fn metric(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    egui::Frame::new()
        .fill(CANVAS)
        .corner_radius(6)
        .inner_margin(12)
        .show(ui, |ui| {
            ui.set_width(140.0);
            ui.label(RichText::new(label).monospace().size(8.5).color(DIM));
            ui.label(RichText::new(value).size(24.0).color(color));
        });
}

#[cfg(test)]
#[derive(Clone, Copy, Debug)]
struct ShellLayout {
    top: Rect,
    bottom: Rect,
    left: Rect,
    center: Rect,
    right: Rect,
}

#[cfg(test)]
fn shell_layout(bounds: Rect, editor_height: f32) -> ShellLayout {
    let top_height = TRANSPORT_HEIGHT.min(bounds.height() * 0.18);
    let bottom_height = editor_height.clamp(
        EDITOR_MIN_HEIGHT,
        (bounds.height() - top_height - 180.0).max(EDITOR_MIN_HEIGHT),
    );
    let body = Rect::from_min_max(
        Pos2::new(bounds.left(), bounds.top() + top_height),
        Pos2::new(bounds.right(), bounds.bottom() - bottom_height),
    );
    let left_width = ASSET_PANEL_WIDTH.min(body.width() * 0.28);
    let right_width = INSPECTOR_WIDTH.min(body.width() * 0.34);
    ShellLayout {
        top: Rect::from_min_max(bounds.left_top(), Pos2::new(bounds.right(), body.top())),
        bottom: Rect::from_min_max(
            Pos2::new(bounds.left(), body.bottom()),
            bounds.right_bottom(),
        ),
        left: Rect::from_min_max(
            body.left_top(),
            Pos2::new(body.left() + left_width, body.bottom()),
        ),
        center: Rect::from_min_max(
            Pos2::new(body.left() + left_width, body.top()),
            Pos2::new(body.right() - right_width, body.bottom()),
        ),
        right: Rect::from_min_max(
            Pos2::new(body.right() - right_width, body.top()),
            body.right_bottom(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_format_is_musical() {
        assert_eq!(format_position(0.0), "001 · 1 · 000");
        assert_eq!(format_position(5.5), "002 · 2 · 480");
        assert_eq!(format_position(0.999_99), "001 · 1 · 959");
    }

    #[test]
    fn shell_layout_stays_contained_and_non_overlapping() {
        for size in [Vec2::new(1_440.0, 900.0), Vec2::new(980.0, 640.0)] {
            let bounds = Rect::from_min_size(Pos2::ZERO, size);
            let layout = shell_layout(bounds, 220.0);
            for region in [
                layout.top,
                layout.bottom,
                layout.left,
                layout.center,
                layout.right,
            ] {
                assert!(bounds.contains_rect(region));
                assert!(region.is_positive());
            }
            assert!(layout.left.right() <= layout.center.left());
            assert!(layout.center.right() <= layout.right.left());
            assert!(layout.top.bottom() <= layout.center.top());
            assert!(layout.center.bottom() <= layout.bottom.top());
        }
    }
}
