// Pixel, beat, and display-counter conversions are bounded by the visible demo canvas.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, TryRecvError},
    },
    time::Duration,
};

use eframe::egui;
use egui::{
    Align, Align2, Color32, CornerRadius, FontFamily, FontId, Layout, Margin, Pos2, Rect, RichText,
    Sense, Stroke, StrokeKind, Vec2,
};

use crate::model::{
    ClipKind, DemoViewModel, EditorKind, Intent, Parameter, RenderState, Selection,
};
use crate::stem_splitter::{Stem, StemSplitOptions};
use crate::theme::{
    AUDIO_TONE, BORDER, BORDER_STRONG, CANVAS, DIM, EVENT_TONE, HIGHLIGHT, NESTED_TONE, PANEL,
    PANEL_ALT, PANEL_RAISED, STATUS_NOTICE, TEXT,
};
use crate::timeline::{DraggedAsset, FIXED_COLUMN_WIDTH, TimelineState, paint_waveform, timeline};

const FOREHEAD_DEFAULT_HEIGHT: f32 = 82.0;
const FOREHEAD_MIN_HEIGHT: f32 = 64.0;
const FOREHEAD_MAX_HEIGHT: f32 = 168.0;
const EDITOR_DEFAULT_HEIGHT: f32 = 210.0;
const EDITOR_MIN_HEIGHT: f32 = 112.0;
const EDITOR_MAX_HEIGHT: f32 = 420.0;
const MIDDLE_MIN_HEIGHT: f32 = 180.0;
const ASSET_PANEL_WIDTH: f32 = FIXED_COLUMN_WIDTH;
const ASSET_PANEL_MIN_WIDTH: f32 = 190.0;
const SIGNAL_PANEL_WIDTH: f32 = 286.0;
const SIGNAL_PANEL_MIN_WIDTH: f32 = 250.0;
const COLLAPSED_PANEL_WIDTH: f32 = 28.0;
const COLLAPSED_PANEL_PULL_THRESHOLD: f32 = 8.0;
const MIDDLE_WORKSPACE_MIN_WIDTH: f32 = 348.0;
const COLUMN_HEADER_HEIGHT: f32 = 30.0;
const WORKSPACE_PANEL_MARGIN: f32 = 10.0;
const PIANO_LOW_PITCH: u8 = 36;
const PIANO_HIGH_PITCH: u8 = 84;
const TEMPO_LABEL: Color32 = Color32::from_rgb(218, 82, 82);
const TEMPO_MATCH_TOLERANCE_BPM: f32 = 0.1;
const TEMPO_REGION_PADDING_SECONDS: f64 = 2.0;

fn tempo_mismatch(asset_bpm: f32, project_bpm: f32) -> bool {
    (asset_bpm - project_bpm).abs() > TEMPO_MATCH_TOLERANCE_BPM
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DropTempoDecision {
    Prompt(f32),
    Apply(gaw_core::TempoSync),
}

fn drop_tempo_decision(asset_bpm: Option<f32>, project_bpm: f32) -> DropTempoDecision {
    match asset_bpm {
        Some(asset_bpm) if tempo_mismatch(asset_bpm, project_bpm) => {
            DropTempoDecision::Prompt(asset_bpm)
        }
        Some(_) => DropTempoDecision::Apply(gaw_core::TempoSync::Stretch),
        None => DropTempoDecision::Apply(gaw_core::TempoSync::None),
    }
}

fn side_panel_max_widths(
    shell_width: f32,
    assets_expanded: bool,
    signal_expanded: bool,
) -> (f32, f32) {
    let signal_reserve = if signal_expanded {
        SIGNAL_PANEL_MIN_WIDTH
    } else {
        COLLAPSED_PANEL_WIDTH
    };
    let assets_reserve = if assets_expanded {
        ASSET_PANEL_WIDTH
    } else {
        COLLAPSED_PANEL_WIDTH
    };
    let assets_available = shell_width - signal_reserve - MIDDLE_WORKSPACE_MIN_WIDTH;
    let signal_available = shell_width - assets_reserve - MIDDLE_WORKSPACE_MIN_WIDTH;
    (
        ASSET_PANEL_WIDTH
            .min(assets_available)
            .max(COLLAPSED_PANEL_WIDTH),
        (shell_width * 0.285)
            .clamp(SIGNAL_PANEL_MIN_WIDTH, 380.0)
            .min(signal_available)
            .max(COLLAPSED_PANEL_WIDTH),
    )
}

fn forehead_max_height(shell_height: f32) -> f32 {
    FOREHEAD_MAX_HEIGHT
        .min((shell_height - EDITOR_MIN_HEIGHT - MIDDLE_MIN_HEIGHT).max(FOREHEAD_MIN_HEIGHT))
}

fn chin_max_height(remaining_height: f32) -> f32 {
    EDITOR_MAX_HEIGHT.min((remaining_height - MIDDLE_MIN_HEIGHT).max(EDITOR_MIN_HEIGHT))
}

#[derive(Debug)]
pub struct GawApp {
    vm: DemoViewModel,
    controller: Option<crate::controller::NativeController>,
    timeline: TimelineState,
    timeline_actions: Vec<Intent>,
    last_time: Option<f64>,
    last_tempo_tap: Option<f64>,
    known_region_beats: f32,
    known_region_start: f32,
    known_region_end: f32,
    selected_note: Option<usize>,
    selected_sampler_zone: usize,
    new_note_pitch: u8,
    new_note_velocity: u8,
    asset_dialog: Option<AssetDialog>,
    pending_asset_drop: Option<PendingAssetDrop>,
    collapsed_asset_folders: HashSet<gaw_core::AssetFolderId>,
    assets_expanded: bool,
    signal_expanded: bool,
}

#[derive(Debug)]
struct PendingAssetDrop {
    asset_id: gaw_core::AssetId,
    asset_name: String,
    beat: f32,
    track: Option<usize>,
    asset_bpm: f32,
    project_bpm: f32,
}

#[derive(Debug)]
enum AssetDialog {
    Rename {
        index: usize,
        value: String,
        extension: String,
    },
    Bpm {
        index: usize,
        value: String,
        detection: Option<BpmDetectionState>,
    },
    StemSplitter {
        asset_id: String,
        selected: [bool; 8],
        denoise: bool,
        dereverb_vocals: bool,
    },
}

#[derive(Debug)]
struct BpmDetectionState {
    receiver: Receiver<Result<gaw_audio::TempoAnalysis, String>>,
    result: Option<Result<gaw_audio::TempoAnalysis, String>>,
    applied: bool,
    selected: usize,
    sections: Vec<TempoSectionDraft>,
    duration_seconds: f64,
}

#[derive(Clone, Copy, Debug)]
struct TempoSectionDraft {
    start_seconds: f64,
    end_seconds: f64,
    detection: Option<gaw_audio::BpmDetection>,
    selected: usize,
}

#[derive(Clone, Copy, Debug)]
enum AssetPreviewAction {
    Toggle,
    Stop,
    Seek(f64),
    PlayRange { start: f64, end: f64 },
}

impl TempoSectionDraft {
    fn bpm(self) -> Option<f32> {
        let detection = self.detection?;
        [
            Some(detection.bpm),
            detection.alternatives[0],
            detection.alternatives[1],
        ][self.selected]
            .or(Some(detection.bpm))
    }
}

impl BpmDetectionState {
    fn accept(&mut self, result: Result<gaw_audio::TempoAnalysis, String>) {
        if let Ok(analysis) = &result {
            self.sections = match analysis {
                gaw_audio::TempoAnalysis::Stable(region) => vec![TempoSectionDraft {
                    start_seconds: region.start_seconds,
                    end_seconds: region.end_seconds,
                    detection: Some(region.detection),
                    selected: 0,
                }],
                gaw_audio::TempoAnalysis::Sections(sections) => sections
                    .iter()
                    .map(|section| TempoSectionDraft {
                        start_seconds: section.start_seconds,
                        end_seconds: section.end_seconds,
                        detection: section.detection,
                        selected: 0,
                    })
                    .collect(),
                gaw_audio::TempoAnalysis::Unreliable(_) => vec![TempoSectionDraft {
                    start_seconds: 0.0,
                    end_seconds: self.duration_seconds,
                    detection: None,
                    selected: 0,
                }],
            };
        }
        self.result = Some(result);
    }
}

impl GawApp {
    /// Builds the explicit bundled demo/new-project fixture.
    ///
    /// # Panics
    /// Panics only if the compile-time demo fixture violates the canonical schema.
    pub fn new(context: &eframe::CreationContext<'_>) -> Self {
        Self::with_project(context, crate::model::demo_project())
            .expect("the bundled demo project is valid")
    }

    /// Builds the native shell around an existing canonical project.
    ///
    /// # Errors
    /// Returns a domain error when the supplied project is not valid.
    pub fn with_project(
        context: &eframe::CreationContext<'_>,
        project: gaw_core::Project,
    ) -> Result<Self, gaw_core::DomainError> {
        configure_style(&context.egui_ctx);
        Ok(Self {
            vm: DemoViewModel::from_project(project)?,
            controller: None,
            timeline: TimelineState::default(),
            timeline_actions: Vec::with_capacity(8),
            last_time: None,
            last_tempo_tap: None,
            known_region_beats: 8.0,
            known_region_start: 0.0,
            known_region_end: 4.0,
            selected_note: None,
            selected_sampler_zone: 0,
            new_note_pitch: 60,
            new_note_velocity: 100,
            asset_dialog: None,
            pending_asset_drop: None,
            collapsed_asset_folders: HashSet::new(),
            assets_expanded: true,
            signal_expanded: true,
        })
    }

    /// Builds the production native shell around an opened project session.
    ///
    /// # Errors
    /// Returns a domain error if the startup project is invalid.
    pub fn with_native_project(
        context: &eframe::CreationContext<'_>,
        startup: crate::NativeStartup,
    ) -> Result<Self, gaw_core::DomainError> {
        let project = startup.project().clone();
        let mut app = Self::with_project(context, project)?;
        app.vm.prepare_native_waveforms();
        let mut controller = crate::controller::NativeController::start(startup);
        controller.initialize_transport(&app.vm.transport);
        app.controller = Some(controller);
        Ok(app)
    }

    pub fn view_model(&self) -> &crate::ProjectViewModel {
        &self.vm
    }

    pub fn view_model_mut(&mut self) -> &mut crate::ProjectViewModel {
        &mut self.vm
    }

    fn pump_controller(&mut self, context: &egui::Context, now: f64) {
        if let Some(mut controller) = self.controller.take() {
            controller.pump(&mut self.vm, now);
            let preview_playing = controller
                .asset_preview_status()
                .is_some_and(|status| status.playing);
            self.controller = Some(controller);
            context.request_repaint_after(Duration::from_millis(if preview_playing {
                16
            } else {
                50
            }));
        }
    }

    fn handle_keyboard(&mut self, context: &egui::Context, now: f64) {
        if context.text_edit_focused() {
            return;
        }
        if matches!(self.asset_dialog, Some(AssetDialog::Bpm { .. })) {
            let preview_key = context.input_mut(|input| {
                if input.consume_key(egui::Modifiers::NONE, egui::Key::Space) {
                    Some(AssetPreviewAction::Toggle)
                } else if input.consume_key(egui::Modifiers::NONE, egui::Key::Home) {
                    Some(AssetPreviewAction::Stop)
                } else {
                    None
                }
            });
            if let Some(action) = preview_key {
                if let Some(controller) = &mut self.controller {
                    match action {
                        AssetPreviewAction::Toggle => controller.toggle_asset_preview(),
                        AssetPreviewAction::Stop => controller.stop_asset_preview(),
                        AssetPreviewAction::Seek(_) | AssetPreviewAction::PlayRange { .. } => {}
                    }
                }
                return;
            }
        }
        let mut action = None;
        context.input_mut(|input| {
            if input.consume_key(egui::Modifiers::NONE, egui::Key::Space) {
                action = Some(Intent::TogglePlayback);
            } else if input.consume_key(egui::Modifiers::NONE, egui::Key::Home) {
                action = Some(Intent::Stop);
            } else if input.consume_key(egui::Modifiers::NONE, egui::Key::Backspace) {
                action = Some(backspace_intent(self.vm.selection));
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
            } else if input.consume_key(
                egui::Modifiers::COMMAND.plus(egui::Modifiers::SHIFT),
                egui::Key::Z,
            ) {
                action = Some(Intent::Redo(now));
            } else if input.consume_key(egui::Modifiers::COMMAND, egui::Key::Z) {
                action = Some(Intent::Undo(now));
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
                        let sound_ready = self
                            .controller
                            .as_ref()
                            .is_some_and(crate::controller::NativeController::sound_ready);
                        ui.label(
                            RichText::new(if sound_ready {
                                "SOUND ●"
                            } else {
                                "SOUND ○"
                            })
                            .monospace()
                            .size(9.0)
                            .color(if sound_ready {
                                HIGHLIGHT
                            } else {
                                DIM
                            }),
                        )
                        .on_hover_text(if sound_ready {
                            "Project audio output is ready"
                        } else {
                            "Project audio output is unavailable"
                        });
                        ui.add_space(10.0);
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new("AGENT PULSE")
                                        .monospace()
                                        .size(9.0)
                                        .color(HIGHLIGHT),
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
                                    RichText::new(lens_text).monospace().size(9.0).color(
                                        if self.vm.structure_lens {
                                            HIGHLIGHT
                                        } else {
                                            DIM
                                        },
                                    ),
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
                    let metronome_response = ui
                        .add(icon_button("M", self.vm.transport.metronome_enabled))
                        .on_hover_text("Project metronome · right-click for volume");
                    if metronome_response.clicked() {
                        self.vm.apply(Intent::ToggleMetronome);
                    }
                    metronome_response.context_menu(|ui| {
                        ui.label(RichText::new("METRONOME VOLUME").monospace().size(9.0));
                        let mut gain = self.vm.transport.metronome_gain;
                        if ui
                            .add(egui::Slider::new(&mut gain, 0.0..=1.0).text("gain"))
                            .changed()
                        {
                            self.vm.apply(Intent::SetMetronomeGain(gain));
                        }
                    });
                    ui.add_space(12.0);
                    let beat = self.vm.transport.playhead;
                    ui.label(
                        RichText::new(format_position(beat, self.vm.transport.time_signature))
                            .monospace()
                            .size(17.0)
                            .color(TEXT),
                    );
                    ui.add_space(10.0);
                    ui.label(RichText::new("TIME").monospace().size(8.5).color(DIM));
                    ui.label(
                        RichText::new(format_playhead_time(beat, self.vm.transport.bpm))
                            .monospace()
                            .size(11.0)
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
                    let mut numerator = self.vm.transport.time_signature.numerator;
                    let mut denominator = self.vm.transport.time_signature.denominator;
                    let numerator_changed = ui
                        .add(
                            egui::DragValue::new(&mut numerator)
                                .range(1..=32)
                                .speed(0.1),
                        )
                        .changed();
                    ui.label(RichText::new("/").monospace().size(10.0).color(DIM));
                    let mut denominator_changed = false;
                    egui::ComboBox::from_id_salt("project-meter-denominator")
                        .selected_text(denominator.to_string())
                        .width(38.0)
                        .show_ui(ui, |ui| {
                            for value in [1, 2, 4, 8, 16, 32] {
                                denominator_changed |= ui
                                    .selectable_value(&mut denominator, value, value.to_string())
                                    .changed();
                            }
                        });
                    if numerator_changed || denominator_changed {
                        self.vm.apply(Intent::SetTimeSignature {
                            numerator,
                            denominator,
                        });
                    }
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
        let can_import = self.controller.is_some();
        let folders = self.vm.asset_folders().to_vec();
        let sidebar = ui.interact(
            ui.available_rect_before_wrap(),
            ui.id().with("asset-sidebar-context"),
            Sense::click(),
        );
        let mut asset_action = None;
        asset_context_menu(&sidebar, can_import, None, false, false, &mut asset_action);
        let source_count = self.vm.assets.len() + self.vm.midi_assets.len();
        if asset_column_title(ui, "ASSETS", &format!("{source_count} sources")) {
            reset_panel_size(ui.ctx(), "assets_collapsed");
            self.assets_expanded = false;
        }
        let filed_audio = folders
            .iter()
            .flat_map(|folder| folder.asset_ids.iter().copied())
            .collect::<HashSet<_>>();
        let filed_midi = folders
            .iter()
            .flat_map(|folder| folder.event_data_ids.iter().copied())
            .collect::<HashSet<_>>();
        egui::ScrollArea::vertical()
            .id_salt("assets")
            .show(ui, |ui| {
                for index in 0..self.vm.assets.len() {
                    if self
                        .vm
                        .asset_id(index)
                        .is_some_and(|id| !filed_audio.contains(&id))
                    {
                        self.audio_asset_row(ui, index, now, can_import, &mut asset_action);
                    }
                }
                for index in 0..self.vm.midi_assets.len() {
                    if self
                        .vm
                        .midi_asset_id(index)
                        .is_some_and(|id| !filed_midi.contains(&id))
                    {
                        self.midi_asset_row(ui, index, &mut asset_action);
                    }
                }
                for folder in &folders {
                    let collapsed = self.collapsed_asset_folders.contains(&folder.id);
                    if Self::asset_folder_row(ui, folder, collapsed)
                        && !self.collapsed_asset_folders.insert(folder.id)
                    {
                        self.collapsed_asset_folders.remove(&folder.id);
                    }
                    if collapsed {
                        continue;
                    }
                    for asset_id in &folder.asset_ids {
                        if let Some(index) = self
                            .vm
                            .assets
                            .iter()
                            .position(|asset| asset.id == asset_id.to_string())
                        {
                            self.audio_asset_row(ui, index, now, can_import, &mut asset_action);
                        }
                    }
                    for event_id in &folder.event_data_ids {
                        if let Some(index) = self
                            .vm
                            .midi_assets
                            .iter()
                            .position(|asset| asset.id == event_id.to_string())
                        {
                            self.midi_asset_row(ui, index, &mut asset_action);
                        }
                    }
                }
            });
        if let Some(action) = asset_action {
            self.handle_asset_action(action);
        }
        self.asset_dialog(ui.ctx());
    }

    fn audio_asset_row(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        now: f64,
        can_import: bool,
        action: &mut Option<AssetMenuAction>,
    ) {
        let Some(asset) = self.vm.assets.get(index).cloned() else {
            return;
        };
        ui.push_id(&asset.id, |ui| {
            let selected = self.vm.selection == Selection::Asset(index);
            let (rect, response) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), 58.0),
                Sense::click_and_drag(),
            );
            ui.painter().rect_filled(
                rect,
                CornerRadius::ZERO,
                if selected { PANEL_RAISED } else { PANEL_ALT },
            );
            ui.painter().rect_stroke(
                rect,
                CornerRadius::ZERO,
                Stroke::new(1.0, if selected { HIGHLIGHT } else { BORDER }),
                StrokeKind::Inside,
            );
            paint_ellipsized_text(
                ui.painter(),
                rect.left_top() + Vec2::new(8.0, 8.0),
                &asset.name,
                FontId::proportional(11.5),
                TEXT,
                rect.width() - 16.0,
            );
            let channels = if asset.channels == 1 {
                "MONO"
            } else {
                "STEREO"
            };
            ui.painter().text(
                rect.left_top() + Vec2::new(8.0, 26.0),
                Align2::LEFT_TOP,
                format!("{:.2}s  ·  {channels}", asset.duration_seconds),
                FontId::monospace(8.5),
                DIM,
            );
            ui.painter().text(
                rect.left_top() + Vec2::new(8.0, 42.0),
                Align2::LEFT_TOP,
                asset
                    .bpm
                    .map_or_else(|| "BPM NOT SET".to_owned(), |bpm| format!("{bpm:.0} BPM")),
                FontId::monospace(8.2),
                if asset.bpm.is_some() { TEXT } else { DIM },
            );
            let alpha = if asset.changed_by_agent {
                self.vm.highlight_alpha(&asset.id, now)
            } else {
                0.0
            };
            if alpha > 0.0 {
                ui.painter().rect_stroke(
                    rect.expand(1.0),
                    CornerRadius::ZERO,
                    Stroke::new(1.5, HIGHLIGHT.gamma_multiply(alpha)),
                    StrokeKind::Outside,
                );
            }
            if response.clicked() {
                self.vm.apply(Intent::Select(Selection::Asset(index)));
            }
            if response.drag_started() {
                self.timeline.dragging_asset = asset.id.parse().ok().map(DraggedAsset::Audio);
            }
            let (transcribing, splitting_stems) =
                asset.id.parse().ok().map_or((false, false), |asset_id| {
                    self.controller
                        .as_ref()
                        .map_or((false, false), |controller| {
                            (
                                controller.is_transcribing(asset_id),
                                controller.is_splitting_stems(asset_id),
                            )
                        })
                });
            asset_context_menu(
                &response,
                can_import && asset.media_path.is_some(),
                Some(index),
                transcribing,
                splitting_stems,
                action,
            );
            response.on_hover_text("Drag onto the arrangement to create an audio clip");
            ui.add_space(5.0);
        });
    }

    fn midi_asset_row(
        &mut self,
        ui: &mut egui::Ui,
        index: usize,
        action: &mut Option<AssetMenuAction>,
    ) {
        let Some(asset) = self.vm.midi_assets.get(index).cloned() else {
            return;
        };
        ui.push_id(&asset.id, |ui| {
            let selected = self.vm.selection == Selection::MidiAsset(index);
            let (rect, response) = ui.allocate_exact_size(
                Vec2::new(ui.available_width(), 58.0),
                Sense::click_and_drag(),
            );
            ui.painter().rect_filled(
                rect,
                CornerRadius::ZERO,
                if selected { PANEL_RAISED } else { PANEL_ALT },
            );
            ui.painter().rect_stroke(
                rect,
                CornerRadius::ZERO,
                Stroke::new(1.0, if selected { HIGHLIGHT } else { BORDER }),
                StrokeKind::Inside,
            );
            paint_ellipsized_text(
                ui.painter(),
                rect.left_top() + Vec2::new(8.0, 8.0),
                &asset.name,
                FontId::proportional(11.5),
                TEXT,
                rect.width() - 16.0,
            );
            ui.painter().text(
                rect.left_top() + Vec2::new(8.0, 27.0),
                Align2::LEFT_TOP,
                format!(
                    "{} NOTES  ·  {:.2} BEATS",
                    asset.note_count, asset.duration_beats
                ),
                FontId::monospace(8.5),
                DIM,
            );
            ui.painter().text(
                rect.left_top() + Vec2::new(8.0, 42.0),
                Align2::LEFT_TOP,
                "MIDI EVENT ASSET",
                FontId::monospace(8.2),
                HIGHLIGHT,
            );
            if response.clicked() {
                self.vm.apply(Intent::Select(Selection::MidiAsset(index)));
            }
            if response.drag_started() {
                self.timeline.dragging_asset = asset.id.parse().ok().map(DraggedAsset::Midi);
            }
            midi_asset_context_menu(&response, index, action);
            response.on_hover_text("Drag onto the arrangement to create an editable event clip");
            ui.add_space(5.0);
        });
    }

    fn asset_folder_row(
        ui: &mut egui::Ui,
        folder: &gaw_core::AssetFolder,
        collapsed: bool,
    ) -> bool {
        let mut clicked = false;
        ui.push_id(folder.id, |ui| {
            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
            ui.painter()
                .rect_filled(rect, CornerRadius::ZERO, PANEL_RAISED);
            ui.painter().rect_stroke(
                rect,
                CornerRadius::ZERO,
                Stroke::new(1.0, BORDER_STRONG),
                StrokeKind::Inside,
            );
            ui.painter().text(
                rect.left_center() + Vec2::new(8.0, 0.0),
                Align2::LEFT_CENTER,
                if collapsed { "›" } else { "⌄" },
                FontId::monospace(11.0),
                TEXT,
            );
            paint_ellipsized_text(
                ui.painter(),
                rect.left_top() + Vec2::new(24.0, 8.0),
                &folder.name,
                FontId::proportional(11.0),
                TEXT,
                rect.width() - 66.0,
            );
            ui.painter().text(
                rect.right_center() - Vec2::new(8.0, 0.0),
                Align2::RIGHT_CENTER,
                (folder.asset_ids.len() + folder.event_data_ids.len()).to_string(),
                FontId::monospace(8.5),
                DIM,
            );
            clicked = response.clicked();
            ui.add_space(5.0);
        });
        clicked
    }

    fn handle_asset_action(&mut self, action: AssetMenuAction) {
        match action {
            AssetMenuAction::Import => self.pick_audio_asset(),
            AssetMenuAction::AddToTimeline(index) => {
                if let Some(asset_id) = self.vm.asset_id(index) {
                    self.request_asset_drop(asset_id, self.vm.transport.playhead, None);
                }
            }
            AssetMenuAction::AddMidiToTimeline(index) => {
                if let Some(event_data_id) = self.vm.midi_asset_id(index) {
                    self.vm.apply(Intent::AddEventDataClip {
                        event_data_id,
                        beat: self.vm.transport.playhead,
                        track: None,
                    });
                }
            }
            AssetMenuAction::Rename(index) => {
                if let Some(asset) = self.vm.assets.get(index) {
                    self.asset_dialog = Some(AssetDialog::Rename {
                        index,
                        value: Path::new(&asset.name)
                            .file_stem()
                            .and_then(|stem| stem.to_str())
                            .unwrap_or(&asset.name)
                            .to_owned(),
                        extension: Path::new(&asset.name)
                            .extension()
                            .and_then(|extension| extension.to_str())
                            .unwrap_or_default()
                            .to_owned(),
                    });
                }
            }
            AssetMenuAction::SetBpm(index) => {
                if let Some(asset) = self.vm.assets.get(index) {
                    let media_path = asset.media_path.clone();
                    self.asset_dialog = Some(AssetDialog::Bpm {
                        index,
                        value: asset
                            .bpm
                            .map_or_else(String::new, |bpm| format!("{bpm:.2}")),
                        detection: None,
                    });
                    if let Some(media_path) = media_path
                        && let Some(controller) = &mut self.controller
                    {
                        controller.begin_asset_preview(&media_path);
                    }
                }
            }
            AssetMenuAction::ConvertToMidi(index) => {
                let Some(asset) = self.vm.assets.get(index).cloned() else {
                    return;
                };
                let Some(media_path) = asset.media_path.as_deref() else {
                    return;
                };
                let Ok(asset_id) = asset.id.parse() else {
                    return;
                };
                let bpm = f64::from(asset.bpm.unwrap_or(self.vm.transport.bpm));
                if let Some(controller) = &mut self.controller {
                    controller.convert_asset_to_midi(
                        asset_id,
                        media_path,
                        asset.content_hash,
                        &asset.name,
                        bpm,
                    );
                }
            }
            AssetMenuAction::StemSplitter(index) => {
                if let Some(asset) = self
                    .vm
                    .assets
                    .get(index)
                    .filter(|asset| asset.media_path.is_some())
                {
                    self.asset_dialog = Some(AssetDialog::StemSplitter {
                        asset_id: asset.id.clone(),
                        selected: [true; 8],
                        denoise: true,
                        dereverb_vocals: true,
                    });
                }
            }
            AssetMenuAction::Delete(index) => self.vm.remove_asset(index),
            AssetMenuAction::Reveal(index) => {
                if let Some(path) = self
                    .vm
                    .assets
                    .get(index)
                    .and_then(|asset| asset.media_path.as_deref())
                {
                    if let Some(controller) = &self.controller {
                        controller.reveal_media(path);
                    } else {
                        reveal_path(Path::new(path));
                    }
                }
            }
        }
    }

    fn request_asset_drop(&mut self, asset_id: gaw_core::AssetId, beat: f32, track: Option<usize>) {
        let asset = self
            .vm
            .assets
            .iter()
            .find(|asset| asset.id == asset_id.to_string());
        let asset_bpm = asset.and_then(|asset| asset.bpm);
        let project_bpm = self.vm.transport.bpm;
        match drop_tempo_decision(asset_bpm, project_bpm) {
            DropTempoDecision::Prompt(asset_bpm) => {
                self.pending_asset_drop = Some(PendingAssetDrop {
                    asset_id,
                    asset_name: asset
                        .map_or_else(|| "Audio asset".to_owned(), |asset| asset.name.clone()),
                    beat,
                    track,
                    asset_bpm,
                    project_bpm,
                });
            }
            DropTempoDecision::Apply(tempo_sync) => self.vm.apply(Intent::AddAssetClip {
                asset_id,
                beat,
                track,
                tempo_sync: Some(tempo_sync),
            }),
        }
    }

    fn handle_timeline_action(&mut self, action: Intent) {
        match action {
            Intent::AddAssetClip {
                asset_id,
                beat,
                track,
                tempo_sync: None,
            } => self.request_asset_drop(asset_id, beat, track),
            action => self.vm.apply(action),
        }
    }

    fn asset_drop_dialog(&mut self, ctx: &egui::Context) {
        let Some(pending) = self.pending_asset_drop.take() else {
            return;
        };
        let mut choice = None;
        let mut cancelled = false;
        egui::Window::new("TEMPO MISMATCH")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .default_width(480.0)
            .show(ctx, |ui| {
                ui.set_min_width(440.0);
                ui.label(RichText::new(&pending.asset_name).color(TEXT));
                ui.label(
                    RichText::new(format!(
                        "This asset is {:.1} BPM; the project is {:.1} BPM.",
                        pending.asset_bpm, pending.project_bpm
                    ))
                    .color(DIM),
                );
                ui.add_space(10.0);
                if ui
                    .add_sized(
                        [ui.available_width(), 34.0],
                        egui::Button::new("MATCH TEMPO AND REPITCH"),
                    )
                    .on_hover_text("Match the project tempo; pitch changes with playback speed")
                    .clicked()
                {
                    choice = Some(gaw_core::TempoSync::Repitch);
                }
                if ui
                    .add_sized(
                        [ui.available_width(), 34.0],
                        egui::Button::new("MATCH TEMPO"),
                    )
                    .on_hover_text("Match the project tempo while preserving pitch")
                    .clicked()
                {
                    choice = Some(gaw_core::TempoSync::Stretch);
                }
                if ui
                    .add_sized(
                        [ui.available_width(), 34.0],
                        egui::Button::new("KEEP ORIGINAL TEMPO"),
                    )
                    .on_hover_text("Play the audio unchanged at its recorded speed")
                    .clicked()
                {
                    choice = Some(gaw_core::TempoSync::None);
                }
                ui.add_space(4.0);
                if ui.button("CANCEL").clicked() {
                    cancelled = true;
                }
            });
        if let Some(tempo_sync) = choice {
            self.vm.apply(Intent::AddAssetClip {
                asset_id: pending.asset_id,
                beat: pending.beat,
                track: pending.track,
                tempo_sync: Some(tempo_sync),
            });
        } else if !cancelled {
            self.pending_asset_drop = Some(pending);
        }
    }

    fn asset_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dialog) = self.asset_dialog.take() else {
            return;
        };
        let tempo_dialog_open = matches!(dialog, AssetDialog::Bpm { .. });
        let title = match &dialog {
            AssetDialog::Rename { .. } => "RENAME ASSET",
            AssetDialog::Bpm { .. } => "ASSET TEMPO",
            AssetDialog::StemSplitter { .. } => "X-LANCE STEM SPLITTER",
        };
        let mut confirmed = false;
        let mut split_confirmed = false;
        let mut cancelled = false;
        let mut detect_requested = false;
        let mut preview_action = None;
        let can_split = self.controller.is_some();
        let preview_status = self
            .controller
            .as_ref()
            .and_then(crate::controller::NativeController::asset_preview_status);
        let tempo_waveform = match &dialog {
            AssetDialog::Bpm { index, .. } => self
                .vm
                .assets
                .get(*index)
                .map(|asset| Arc::clone(&asset.waveform)),
            AssetDialog::Rename { .. } | AssetDialog::StemSplitter { .. } => None,
        };
        if let AssetDialog::Bpm { detection, .. } = &mut dialog
            && let Some(state) = detection.as_mut()
            && state.result.is_none()
        {
            match state.receiver.try_recv() {
                Ok(result) => state.accept(result),
                Err(TryRecvError::Disconnected) => {
                    state.accept(Err("Tempo detection stopped unexpectedly".to_owned()));
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        egui::Window::new(title)
            .collapsible(false)
            .resizable(true)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .default_width(520.0)
            .max_height(ctx.content_rect().height() * 0.82)
            .show(ctx, |ui| {
                ui.set_min_width(480.0);
                match &mut dialog {
                    AssetDialog::Rename {
                        value, extension, ..
                    } => {
                        let response =
                            ui.add(egui::TextEdit::singleline(value).desired_width(460.0));
                        if response.lost_focus()
                            && ui.input(|input| input.key_pressed(egui::Key::Enter))
                        {
                            confirmed = true;
                        }
                        let extension_label = if extension.is_empty() {
                            "No extension".to_owned()
                        } else {
                            format!("Extension preserved: .{extension}")
                        };
                        ui.label(
                            RichText::new(extension_label)
                                .monospace()
                                .size(10.0)
                                .color(DIM),
                        );
                    }
                    AssetDialog::Bpm {
                        detection, value, ..
                    } => {
                        let has_sections = detection.as_ref().is_some_and(|state| {
                            matches!(
                                state.result,
                                Some(Ok(gaw_audio::TempoAnalysis::Sections(_)))
                            )
                        });
                        if !has_sections {
                            ui.label(RichText::new("MANUAL BPM").monospace().size(9.0).color(DIM));
                            let response =
                                ui.add(egui::TextEdit::singleline(value).desired_width(460.0));
                            if response.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter))
                            {
                                confirmed = true;
                            }
                        }
                        if ui.button("DETECT TEMPO").clicked() {
                            detect_requested = true;
                        }
                        if let Some(state) = detection {
                            if state.result.is_some()
                                && let Some(waveform) = tempo_waveform.as_deref()
                            {
                                if let Some(seconds) = tempo_map_editor(
                                    ui,
                                    waveform,
                                    &mut state.sections,
                                    preview_status.as_ref().map(|status| status.position_seconds),
                                ) {
                                    preview_action = Some(AssetPreviewAction::Seek(seconds));
                                }
                                ui.horizontal(|ui| {
                                    let label = if preview_status
                                        .as_ref()
                                        .is_some_and(|status| status.playing)
                                    {
                                        "Ⅱ PAUSE"
                                    } else {
                                        "▶ PLAY"
                                    };
                                    if ui
                                        .add_enabled(
                                            preview_status
                                                .as_ref()
                                                .is_some_and(|status| !status.loading),
                                            egui::Button::new(label),
                                        )
                                        .clicked()
                                    {
                                        preview_action = Some(AssetPreviewAction::Toggle);
                                    }
                                    if ui.button("■ STOP").clicked() {
                                        preview_action = Some(AssetPreviewAction::Stop);
                                    }
                                    let status_text = preview_status.as_ref().map_or_else(
                                        || "PREVIEW UNAVAILABLE".to_owned(),
                                        |status| {
                                            if status.loading {
                                                "LOADING AUDIO…".to_owned()
                                            } else if let Some(error) = &status.error {
                                                format!("PREVIEW ERROR · {error}")
                                            } else {
                                                format!(
                                                    "{} / {}",
                                                    format_preview_time(status.position_seconds),
                                                    format_preview_time(status.duration_seconds)
                                                )
                                            }
                                        },
                                    );
                                    ui.label(
                                        RichText::new(status_text)
                                            .monospace()
                                            .size(9.0)
                                            .color(DIM),
                                    );
                                });
                                ui.add_space(8.0);
                            }
                            match &state.result {
                                None => {
                                    ui.label(RichText::new("Analyzing audio…").color(DIM));
                                }
                                Some(Ok(gaw_audio::TempoAnalysis::Stable(region))) => {
                                    let result = region.detection;
                                    ui.label(
                                        RichText::new(format!(
                                            "Stable tempo · {:.1} BPM · {:.0}% confidence",
                                            result.bpm,
                                            result.confidence * 100.0
                                        ))
                                        .color(TEXT),
                                    );
                                    if !state.applied {
                                        *value = format!("{:.2}", result.bpm);
                                        state.applied = true;
                                    }
                                    let candidates = [
                                        Some(result.bpm),
                                        result.alternatives[0],
                                        result.alternatives[1],
                                    ];
                                    let labels = ["Single-time", "Half-time", "Double-time"];
                                    for (index, candidate) in candidates.into_iter().enumerate() {
                                        if let Some(bpm) = candidate
                                            && ui
                                                .radio(
                                                    state.selected == index,
                                                    format!("{} · {bpm:.1} BPM", labels[index]),
                                                )
                                                .clicked()
                                        {
                                            state.selected = index;
                                            if let Some(section) = state.sections.first_mut() {
                                                section.selected = index;
                                            }
                                            *value = format!("{bpm:.2}");
                                        }
                                    }
                                }
                                Some(Ok(gaw_audio::TempoAnalysis::Sections(_))) => {
                                    let detected = state
                                        .sections
                                        .iter()
                                        .filter(|section| section.detection.is_some())
                                        .count();
                                    ui.label(
                                        RichText::new(format!(
                                            "{detected} stable tempo region{} detected",
                                            if detected == 1 { "" } else { "s" }
                                        ))
                                        .color(TEXT),
                                    );
                                    ui.label(
                                        RichText::new(
                                            "Review the tempo map and adjust its boundaries before creating assets.",
                                        )
                                        .color(DIM),
                                    );
                                    egui::ScrollArea::vertical()
                                        .max_height(330.0)
                                        .show(ui, |ui| {
                                            if let Some((start, end)) =
                                                tempo_sections_editor(ui, &mut state.sections)
                                            {
                                                preview_action = Some(
                                                    AssetPreviewAction::PlayRange { start, end },
                                                );
                                            }
                                        });
                                    if !can_split {
                                        ui.label(
                                            RichText::new(
                                                "Open a saved project to materialize region assets.",
                                            )
                                            .color(DIM),
                                        );
                                    }
                                }
                                Some(Ok(gaw_audio::TempoAnalysis::Unreliable(unreliable))) => {
                                    ui.label(
                                        RichText::new(tempo_unreliable_message(*unreliable))
                                            .color(DIM),
                                    );
                                    ui.label(
                                        RichText::new(
                                            "Choose a BPM manually, or try a more rhythmically stable source.",
                                        )
                                        .color(TEXT),
                                    );
                                }
                                Some(Err(error)) => {
                                    ui.label(RichText::new(error).color(DIM));
                                }
                            }
                        }
                    }
                    AssetDialog::StemSplitter {
                        selected,
                        denoise,
                        dereverb_vocals,
                        ..
                    } => {
                        ui.label(
                            RichText::new(
                                "X-LANCE separates eight independent instrument categories. Select the stems to create.",
                            )
                            .color(TEXT),
                        );
                        ui.add_space(8.0);
                        egui::Grid::new("xlance-stems")
                            .num_columns(2)
                            .spacing([18.0, 6.0])
                            .show(ui, |ui| {
                                for (index, stem) in Stem::ALL.into_iter().enumerate() {
                                    ui.checkbox(&mut selected[index], stem.label());
                                    if index % 2 == 1 {
                                        ui.end_row();
                                    }
                                }
                            });
                        ui.add_space(8.0);
                        ui.checkbox(denoise, "Remove noise before splitting");
                        let vocals_selected = selected[0];
                        ui.add_enabled_ui(vocals_selected, |ui| {
                            ui.checkbox(dereverb_vocals, "Dereverberate vocals");
                        });
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new(
                                "Quality mode runs locally on CUDA, may download several gigabytes of model weights, and can take a long time. X-LANCE outputs are not guaranteed to sum exactly to the source mix.",
                            )
                            .size(9.5)
                            .color(DIM),
                        );
                    }
                }
                ui.horizontal(|ui| {
                    if ui.button("CANCEL").clicked() {
                        cancelled = true;
                    }
                    let regions_mode = matches!(
                        &dialog,
                        AssetDialog::Bpm {
                            detection: Some(BpmDetectionState {
                                result: Some(Ok(gaw_audio::TempoAnalysis::Sections(_))),
                                ..
                            }),
                            ..
                        }
                    );
                    if let AssetDialog::StemSplitter { selected, .. } = &dialog {
                        let count = selected.iter().filter(|selected| **selected).count();
                        if ui
                            .add_enabled(
                                count > 0 && self.controller.is_some(),
                                egui::Button::new(format!(
                                    "SPLIT INTO {count} STEM{}",
                                    if count == 1 { "" } else { "S" }
                                )),
                            )
                            .clicked()
                        {
                            confirmed = true;
                        }
                    } else if regions_mode {
                        if ui
                            .add_enabled(
                                can_split,
                                egui::Button::new("CREATE ASSETS FROM DETECTED SECTIONS"),
                            )
                            .clicked()
                        {
                            split_confirmed = true;
                        }
                    } else if ui.button("APPLY").clicked() {
                        confirmed = true;
                    }
                });
            });
        if let Some(action) = preview_action
            && let Some(controller) = &mut self.controller
        {
            match action {
                AssetPreviewAction::Toggle => controller.toggle_asset_preview(),
                AssetPreviewAction::Stop => controller.stop_asset_preview(),
                AssetPreviewAction::Seek(seconds) => controller.seek_asset_preview(seconds),
                AssetPreviewAction::PlayRange { start, end } => {
                    controller.play_asset_preview_range(start, end);
                }
            }
        }
        if split_confirmed && !cancelled {
            let mut submitted = false;
            if let AssetDialog::Bpm {
                index,
                detection: Some(state),
                ..
            } = &dialog
                && let Some(asset) = self.vm.assets.get(*index)
                && let Some(asset_id) = asset.id.parse().ok()
                && let Some(regions) = tempo_media_regions(asset, &state.sections)
                && let Some(controller) = &mut self.controller
            {
                submitted = controller.split_asset_regions(self.vm.revision(), asset_id, regions);
            }
            if !submitted {
                self.asset_dialog = Some(dialog);
            }
        } else if confirmed && !cancelled {
            match dialog {
                AssetDialog::Rename {
                    index,
                    value,
                    extension,
                } => {
                    let value = value.trim().to_owned();
                    let value = if extension.is_empty() {
                        value
                    } else {
                        let suffix = format!(".{extension}");
                        value.strip_suffix(&suffix).unwrap_or(&value).to_owned()
                    };
                    let name = if extension.is_empty() {
                        value
                    } else {
                        format!("{value}.{extension}")
                    };
                    self.vm.rename_asset(index, &name);
                }
                AssetDialog::Bpm { index, value, .. } => {
                    if let Ok(bpm) = value.trim().parse::<f32>()
                        && let Some(asset) = self.vm.assets.get(index)
                    {
                        self.vm.set_asset_tempo(
                            index,
                            Some(bpm),
                            asset.first_beat_seconds.unwrap_or(0.0),
                        );
                    }
                }
                AssetDialog::StemSplitter {
                    asset_id,
                    selected,
                    denoise,
                    dereverb_vocals,
                } => {
                    let Some(asset) = self
                        .vm
                        .assets
                        .iter()
                        .find(|asset| asset.id == asset_id)
                        .cloned()
                    else {
                        return;
                    };
                    let Some(media_path) = asset.media_path.as_deref() else {
                        return;
                    };
                    let Ok(asset_id) = asset.id.parse() else {
                        return;
                    };
                    let stems = Stem::ALL
                        .into_iter()
                        .enumerate()
                        .filter_map(|(index, stem)| selected[index].then_some(stem))
                        .collect();
                    if let Some(controller) = &mut self.controller {
                        controller.split_asset_stems(
                            asset_id,
                            media_path,
                            asset.content_hash,
                            &asset.name,
                            StemSplitOptions {
                                stems,
                                denoise,
                                dereverb_vocals,
                            },
                        );
                    }
                }
            }
        } else if detect_requested {
            if let AssetDialog::Bpm {
                index,
                ref mut detection,
                ..
            } = dialog
            {
                *detection = self.start_bpm_detection(index);
            }
            self.asset_dialog = Some(dialog);
        } else if !cancelled {
            self.asset_dialog = Some(dialog);
        }
        if tempo_dialog_open
            && self.asset_dialog.is_none()
            && let Some(controller) = &mut self.controller
        {
            controller.end_asset_preview(&self.vm);
        }
    }

    fn start_bpm_detection(&self, index: usize) -> Option<BpmDetectionState> {
        let asset = self.vm.assets.get(index)?;
        let media_path = asset.media_path.as_deref()?;
        let path = self.controller.as_ref().map_or_else(
            || PathBuf::from(media_path),
            |controller| controller.media_path(media_path),
        );
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(gaw_audio::detect_tempo_wav(&path));
        });
        Some(BpmDetectionState {
            receiver,
            result: None,
            applied: false,
            selected: 0,
            sections: Vec::new(),
            duration_seconds: f64::from(asset.duration_seconds),
        })
    }

    fn pick_audio_asset(&mut self) {
        let Some(controller) = &mut self.controller else {
            return;
        };
        if let Some(source) = rfd::FileDialog::new()
            .set_title("Add Audio Asset")
            .add_filter("Audio", gaw_project::IMPORT_AUDIO_EXTENSIONS)
            .pick_file()
        {
            controller.import_media(source);
        }
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        panel_title(ui, "SIGNAL", "top → bottom");
        let selection = self.vm.selection;
        match selection {
            Selection::None | Selection::Track { .. } => Self::empty_inspector(ui),
            Selection::Asset(index) => self.asset_inspector(ui, index),
            Selection::MidiAsset(index) => self.midi_asset_inspector(ui, index),
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

    fn asset_inspector(&mut self, ui: &mut egui::Ui, index: usize) {
        let Some(asset) = self.vm.assets.get(index).cloned() else {
            return;
        };
        signal_node(ui, 1, "SOURCE ASSET", &asset.name, AUDIO_TONE, true);
        property(ui, "Stable ID", &asset.id);
        if self.vm.structure_lens {
            property(ui, "Path", &asset.structure_path);
        }
        property(ui, "Definition", &asset.definition);
        property(
            ui,
            "Media",
            asset.media_path.as_deref().unwrap_or("not materialized"),
        );
        if let Some(hash) = &asset.content_hash {
            property(ui, "Content hash", hash);
        }
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
        ui.separator();
        ui.label(RichText::new("TEMPO MAP").monospace().size(9.0).color(DIM));
        let mut bpm = asset.bpm.unwrap_or(120.0);
        if ui
            .add(
                egui::DragValue::new(&mut bpm)
                    .range(20.0..=400.0)
                    .suffix(" BPM"),
            )
            .changed()
        {
            self.vm
                .set_asset_tempo(index, Some(bpm), asset.first_beat_seconds.unwrap_or(0.0));
        }
        let mut first_beat = asset.first_beat_seconds.unwrap_or(0.0);
        if ui
            .add(
                egui::DragValue::new(&mut first_beat)
                    .range(0.0..=asset.duration_seconds)
                    .suffix(" s first beat"),
            )
            .changed()
        {
            self.vm.set_asset_tempo(index, Some(bpm), first_beat);
        }
        ui.horizontal(|ui| {
            if ui.small_button("½").clicked() {
                self.vm.set_asset_tempo(index, Some(bpm / 2.0), first_beat);
            }
            if ui.small_button("2×").clicked() {
                self.vm.set_asset_tempo(index, Some(bpm * 2.0), first_beat);
            }
            if ui.small_button("TAP").clicked() {
                let now = ui.input(|input| input.time);
                if let Some(last) = self.last_tempo_tap {
                    let seconds = now - last;
                    if (0.15..=3.0).contains(&seconds) {
                        self.vm
                            .set_asset_tempo(index, Some((60.0 / seconds) as f32), first_beat);
                    }
                }
                self.last_tempo_tap = Some(now);
            }
            if ui.small_button("SET 120 (NO ANALYSIS)").clicked() {
                self.vm
                    .accept_asset_tempo_suggestion(index, 120.0, first_beat);
            }
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::DragValue::new(&mut self.known_region_start)
                    .range(0.0..=asset.duration_seconds)
                    .suffix(" s start"),
            );
            ui.add(
                egui::DragValue::new(&mut self.known_region_end)
                    .range(0.0..=asset.duration_seconds)
                    .suffix(" s end"),
            );
            ui.add(
                egui::DragValue::new(&mut self.known_region_beats)
                    .range(1.0..=128.0)
                    .suffix(" known beats"),
            );
            let region_seconds = self.known_region_end - self.known_region_start;
            if ui.small_button("FIT REGION").clicked() && region_seconds > 0.0 {
                let derived = self.known_region_beats / region_seconds * 60.0;
                self.vm.set_asset_tempo(index, Some(derived), first_beat);
            }
        });
        property(ui, "Sample rate", &format!("{} Hz", asset.sample_rate));
        property(ui, "Frames", &asset.frames.to_string());
        property(ui, "Revisions", &asset.revision_count.to_string());
        if let Some(revision) = &asset.current_revision {
            property(ui, "Current revision", revision);
        }
        let splitting_stems = asset.id.parse().ok().is_some_and(|asset_id| {
            self.controller
                .as_ref()
                .is_some_and(|controller| controller.is_splitting_stems(asset_id))
        });
        if ui
            .add_enabled(
                self.controller.is_some() && asset.media_path.is_some() && !splitting_stems,
                egui::Button::new(if splitting_stems {
                    "SPLITTING STEMS…"
                } else {
                    "STEM SPLITTER…"
                }),
            )
            .clicked()
        {
            self.asset_dialog = Some(AssetDialog::StemSplitter {
                asset_id: asset.id.clone(),
                selected: [true; 8],
                denoise: true,
                dereverb_vocals: true,
            });
        }
        if asset.definition == "processed" {
            ui.label(
                RichText::new("Derived processing is part of this asset's immutable definition.")
                    .size(9.5)
                    .color(DIM),
            );
        }
    }

    fn midi_asset_inspector(&self, ui: &mut egui::Ui, index: usize) {
        let Some(asset) = self.vm.midi_assets.get(index) else {
            return;
        };
        signal_node(ui, 1, "MIDI ASSET", &asset.name, HIGHLIGHT, true);
        property(ui, "Stable ID", &asset.id);
        if self.vm.structure_lens {
            property(ui, "Path", &asset.structure_path);
        }
        property(ui, "Notes", &asset.note_count.to_string());
        property(
            ui,
            "Duration",
            &format!("{:.2} beats", asset.duration_beats),
        );
        property(ui, "Storage", "canonical event data");
    }

    fn sampler_inspector(&self, ui: &mut egui::Ui, track: usize) {
        let selected_track = self.vm.current_composition().tracks.get(track);
        let name = selected_track.map_or("Event track", |track| track.name.as_str());
        signal_node(ui, 1, "EVENT STREAM", name, EVENT_TONE, true);
        if self.vm.structure_lens
            && let Some(track) = self.vm.current_composition().tracks.get(track)
        {
            property(ui, "Track ID", &track.id);
            property(ui, "Path", &track.structure_path);
        }
        connector(ui);
        signal_node(ui, 2, "INSTRUMENT", "Slice Sampler", EVENT_TONE, true);
        property(
            ui,
            "Zones",
            &selected_track
                .map_or(0, |track| track.sampler_zones.len())
                .to_string(),
        );
        if let Some(track) = selected_track {
            for zone in &track.sampler_zones {
                property(
                    ui,
                    &zone.name,
                    &format!(
                        "{} · root {} · notes {}–{} · velocity {}–{}",
                        zone.asset_id,
                        zone.root_note,
                        zone.low_note,
                        zone.high_note,
                        zone.low_velocity,
                        zone.high_velocity
                    ),
                );
                if self.vm.structure_lens {
                    property(ui, "Zone ID", &zone.id);
                    property(ui, "Path", &zone.structure_path);
                }
            }
        }
        connector(ui);
        signal_node(ui, 3, "TRACK OUTPUT", "stereo", HIGHLIGHT, true);
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
            ClipKind::Audio { .. } => AUDIO_TONE,
            ClipKind::Event { .. } => EVENT_TONE,
            ClipKind::Composition { .. } => NESTED_TONE,
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
        let is_composition = matches!(kind, ClipKind::Composition { .. });
        let effects = clip.effects.clone();
        let audio_details = self.vm.selected_audio_details();
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
                    HIGHLIGHT,
                    true,
                );
                if let Some((source_start, source_duration, reverse, fade_in, fade_out)) =
                    audio_details
                {
                    property(
                        ui,
                        "Source range",
                        &format!(
                            "{source_start:.2}s → {:.2}s",
                            source_start + source_duration
                        ),
                    );
                    property(ui, "Reverse", if reverse { "on" } else { "off" });
                    property(
                        ui,
                        "Fades",
                        &format!(
                            "in {} · out {}",
                            if fade_in { "on" } else { "off" },
                            if fade_out { "on" } else { "off" }
                        ),
                    );
                }
                if let Some(asset) = self.vm.assets.get(asset) {
                    property(ui, "Asset", &asset.id);
                }
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
            }
            ClipKind::Event { .. } => {
                signal_node(ui, 2, "INSTRUMENT", "Slice Sampler", EVENT_TONE, true);
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
                    "Mute → placement processor stack",
                    NESTED_TONE,
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
                if ui.button("OPEN COMPOSITION").clicked() {
                    self.vm.apply(Intent::EnterChild {
                        track: track_index,
                        clip: clip_index,
                    });
                }
            }
        }
        if !is_composition {
            property(ui, "Clip gain", &format!("{gain_db:+.1} dB"));
        }
        connector(ui);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if is_composition {
                    "PLACEMENT EFFECTS"
                } else {
                    "CLIP EFFECTS"
                })
                .monospace()
                .size(9.0)
                .color(DIM),
            );
            if let Some(stack) = self.vm.clip_stack(track_index, clip_index) {
                processor_chooser(ui, &mut self.vm, &stack, ("clip", &clip_id));
            }
        });
        for (effect_index, effect) in effects.iter().enumerate() {
            let selected = matches!(self.vm.selection, Selection::Effect { track, clip, effect } if track == track_index && clip == clip_index && effect == effect_index);
            let response = ui
                .push_id(&effect.id, |ui| {
                    signal_node(
                        ui,
                        effect_index + 3,
                        &effect.kind,
                        &effect.name,
                        HIGHLIGHT,
                        effect.enabled,
                    )
                })
                .inner;
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
                    ui.label(
                        RichText::new("EDITING")
                            .monospace()
                            .size(8.0)
                            .color(HIGHLIGHT),
                    );
                }
                if ui.small_button("×").clicked()
                    && let Some(stack) = self.vm.clip_stack(track_index, clip_index)
                {
                    self.vm.remove_processor_at(stack, effect_index);
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
            HIGHLIGHT,
            true,
        );
        property(ui, "Order", "clip sum → track processors");
        if let Some(track_id) = self.vm.current_track_id(track_index) {
            processor_chooser(
                ui,
                &mut self.vm,
                &gaw_core::ProcessorStack::Track { track_id },
                ("track", track_id),
            );
        }
        for (index, effect) in track_effects.iter().enumerate() {
            connector(ui);
            let response = ui
                .push_id(&effect.id, |ui| {
                    signal_node(
                        ui,
                        effects.len() + 4 + index,
                        "TRACK EFFECT",
                        &effect.name,
                        HIGHLIGHT,
                        effect.enabled,
                    )
                })
                .inner;
            if let Some(track_id) = self.vm.current_track_id(track_index) {
                let stack = gaw_core::ProcessorStack::Track { track_id };
                if response.clicked() {
                    self.vm.select_processor_at(stack.clone(), index);
                }
                if ui
                    .small_button(if effect.enabled { "ON" } else { "OFF" })
                    .clicked()
                {
                    self.vm.toggle_processor_at(stack.clone(), index);
                }
                if ui.small_button("↑").clicked() {
                    self.vm.move_processor_at(stack.clone(), index, -1);
                }
                if ui.small_button("↓").clicked() {
                    self.vm.move_processor_at(stack.clone(), index, 1);
                }
                if ui.small_button("×").clicked() {
                    self.vm.remove_processor_at(stack, index);
                }
            }
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
            NESTED_TONE,
            true,
        );
        property(ui, "Order", "track sum → output stack");
        if self.vm.structure_lens {
            property(ui, "Path", &self.vm.current_composition().structure_path);
        }
        let composition_id = self.vm.current_composition_id();
        processor_chooser(
            ui,
            &mut self.vm,
            &gaw_core::ProcessorStack::CompositionOutput { composition_id },
            ("output", composition_id),
        );
        for (index, effect) in output_effects.iter().enumerate() {
            connector(ui);
            let response = ui
                .push_id(&effect.id, |ui| {
                    signal_node(
                        ui,
                        effects.len() + track_effects.len() + 5 + index,
                        "OUTPUT EFFECT",
                        &effect.name,
                        NESTED_TONE,
                        effect.enabled,
                    )
                })
                .inner;
            let stack = gaw_core::ProcessorStack::CompositionOutput {
                composition_id: self.vm.current_composition_id(),
            };
            if response.clicked() {
                self.vm.select_processor_at(stack.clone(), index);
            }
            if ui
                .small_button(if effect.enabled { "ON" } else { "OFF" })
                .clicked()
            {
                self.vm.toggle_processor_at(stack.clone(), index);
            }
            if ui.small_button("↑").clicked() {
                self.vm.move_processor_at(stack.clone(), index, -1);
            }
            if ui.small_button("↓").clicked() {
                self.vm.move_processor_at(stack.clone(), index, 1);
            }
            if ui.small_button("×").clicked() {
                self.vm.remove_processor_at(stack, index);
            }
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
                NESTED_TONE,
            );
            metric(
                ui,
                "ASSETS",
                &(self.vm.assets.len() + self.vm.midi_assets.len()).to_string(),
                AUDIO_TONE,
            );
            metric(
                ui,
                "TRACKS HERE",
                &self.vm.current_composition().tracks.len().to_string(),
                EVENT_TONE,
            );
            metric(ui, "SAMPLE RATE", "48 kHz", HIGHLIGHT);
        });
    }

    fn waveform_editor(&mut self, ui: &mut egui::Ui) {
        let (name, waveform, info) = match self.vm.selection {
            Selection::Asset(index) => self.vm.assets.get(index).map(|asset| {
                (
                    asset.name.clone(),
                    Arc::clone(&asset.waveform),
                    "ASSET BPM · FIRST BEAT · SOURCE RANGE",
                )
            }),
            _ => self.vm.selected_clip().map(|(_, _, clip)| {
                (
                    clip.name.clone(),
                    Arc::clone(&clip.waveform),
                    "TRIM · CHOP · FADE · REVERSE",
                )
            }),
        }
        .unwrap_or_else(|| ("Waveform".into(), Arc::from([]), "SOURCE"));
        panel_title(ui, "WAVEFORM", &name);
        if self
            .vm
            .selected_clip()
            .is_some_and(|(_, _, clip)| matches!(clip.kind, ClipKind::Audio { .. }))
        {
            ui.horizontal(|ui| {
                for (label, edit) in [
                    ("TRIM +", crate::AudioClipEdit::TrimStart),
                    ("CHOP", crate::AudioClipEdit::Chop),
                    ("FADE IN", crate::AudioClipEdit::ToggleFadeIn),
                    ("FADE OUT", crate::AudioClipEdit::ToggleFadeOut),
                    ("REVERSE", crate::AudioClipEdit::ToggleReverse),
                ] {
                    if ui.small_button(label).clicked() {
                        self.vm.edit_selected_audio_clip(edit);
                    }
                }
            });
        }
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), ui.available_height().max(90.0)),
            Sense::click_and_drag(),
        );
        ui.painter().rect_filled(rect, CornerRadius::ZERO, CANVAS);
        let waveform_rect = rect.shrink2(Vec2::new(14.0, 26.0));
        paint_waveform(ui.painter(), waveform_rect, &waveform, AUDIO_TONE);
        ui.painter().hline(
            waveform_rect.x_range(),
            waveform_rect.center().y,
            Stroke::new(0.5_f32, BORDER),
        );
        for fraction in [0.18, 0.47, 0.72] {
            let x = egui::lerp(waveform_rect.x_range(), fraction);
            ui.painter().vline(
                x,
                waveform_rect.y_range(),
                Stroke::new(1.0_f32, NESTED_TONE),
            );
            ui.painter()
                .circle_filled(Pos2::new(x, waveform_rect.top()), 3.0, NESTED_TONE);
        }
        if let Selection::Asset(index) = self.vm.selection
            && let Some(asset) = self.vm.assets.get(index)
            && let Some(first_beat) = asset.first_beat_seconds
            && asset.duration_seconds > 0.0
        {
            let x = egui::lerp(
                waveform_rect.x_range(),
                (first_beat / asset.duration_seconds).clamp(0.0, 1.0),
            );
            ui.painter()
                .vline(x, waveform_rect.y_range(), Stroke::new(2.0_f32, EVENT_TONE));
            ui.painter().text(
                Pos2::new(x + 4.0, waveform_rect.top()),
                Align2::LEFT_TOP,
                "FIRST BEAT",
                FontId::monospace(8.0),
                EVENT_TONE,
            );
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

    fn piano_roll_editor(&mut self, ui: &mut egui::Ui) {
        let Some((track_index, clip_index, clip)) = self
            .vm
            .selected_clip()
            .map(|(track, clip, value)| (track, clip, value.clone()))
        else {
            return;
        };
        let ClipKind::Event { notes } = &clip.kind else {
            return;
        };
        panel_title(ui, "PIANO ROLL", &clip.name);
        ui.horizontal(|ui| {
            ui.label(RichText::new("NEW").monospace().size(8.0).color(DIM));
            ui.add(
                egui::DragValue::new(&mut self.new_note_pitch)
                    .range(0..=127)
                    .prefix("note "),
            );
            ui.add(
                egui::DragValue::new(&mut self.new_note_velocity)
                    .range(0..=127)
                    .prefix("velocity "),
            );
            if ui.small_button("+ AT PLAYHEAD").clicked() {
                self.vm.apply(Intent::AddNote {
                    track: track_index,
                    clip: clip_index,
                    start: (self.vm.transport.playhead - clip.start).clamp(0.0, clip.length),
                    length: 0.25,
                    pitch: self.new_note_pitch,
                    velocity: self.new_note_velocity,
                });
            }
            if let Some(event_index) = self.selected_note
                && let Some(note) = notes.iter().find(|note| note.event_index == event_index)
            {
                let mut velocity = (note.velocity * 127.0).round() as u8;
                if ui
                    .add(
                        egui::DragValue::new(&mut velocity)
                            .range(0..=127)
                            .prefix("selected velocity "),
                    )
                    .changed()
                {
                    self.vm.apply(Intent::EditNote {
                        track: track_index,
                        clip: clip_index,
                        event_index,
                        start: note.start,
                        length: note.length,
                        pitch: note.pitch,
                        velocity,
                    });
                }
                if ui.small_button("DELETE NOTE").clicked() {
                    self.vm.apply(Intent::DeleteNote {
                        track: track_index,
                        clip: clip_index,
                        event_index,
                    });
                    self.selected_note = None;
                }
            }
        });
        let (rect, grid_response) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), ui.available_height().max(120.0)),
            Sense::click_and_drag(),
        );
        ui.painter().rect_filled(rect, CornerRadius::ZERO, CANVAS);
        let keys_width = 54.0;
        let grid = Rect::from_min_max(
            Pos2::new(rect.left() + keys_width, rect.top()),
            rect.right_bottom(),
        );
        let pitch_rows = f32::from(PIANO_HIGH_PITCH - PIANO_LOW_PITCH + 1);
        let row_height = grid.height() / pitch_rows;
        for row in 0..=(PIANO_HIGH_PITCH - PIANO_LOW_PITCH) {
            let pitch = PIANO_HIGH_PITCH - row;
            let y = grid.top() + f32::from(row) * row_height;
            ui.painter()
                .hline(rect.x_range(), y, Stroke::new(0.5_f32, BORDER));
            if pitch.is_multiple_of(12) {
                ui.painter().text(
                    Pos2::new(rect.left() + 8.0, y + row_height * 0.5),
                    Align2::LEFT_CENTER,
                    format!("C{}", pitch / 12 - 1),
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
                Stroke::new(if beat % 4 == 0 { 1.0_f32 } else { 0.5_f32 }, BORDER),
            );
        }
        let mut note_under_pointer = false;
        for note in notes
            .iter()
            .filter(|note| (PIANO_LOW_PITCH..=PIANO_HIGH_PITCH).contains(&note.pitch))
        {
            let x = grid.left() + note.start / clip.length * grid.width();
            let width = (note.length / clip.length * grid.width()).max(4.0);
            let y = grid.top() + f32::from(PIANO_HIGH_PITCH - note.pitch) * row_height;
            let note_rect = Rect::from_min_size(
                Pos2::new(x, y + 1.0),
                Vec2::new(width, (row_height - 2.0).max(3.0)),
            );
            let selected = self.selected_note == Some(note.event_index);
            ui.painter().rect_filled(
                note_rect,
                CornerRadius::ZERO,
                if selected {
                    HIGHLIGHT
                } else {
                    EVENT_TONE.gamma_multiply(0.65 + note.velocity * 0.3)
                },
            );
            let response = ui.interact(
                note_rect,
                egui::Id::new(("piano_note", &clip.id, note.event_index)),
                Sense::click_and_drag(),
            );
            note_under_pointer |= response.hovered();
            if response.clicked() {
                self.selected_note = Some(note.event_index);
            }
            let resize_rect = Rect::from_min_max(
                Pos2::new(
                    (note_rect.right() - 6.0).max(note_rect.left()),
                    note_rect.top(),
                ),
                note_rect.right_bottom(),
            );
            let resize = ui.interact(
                resize_rect,
                egui::Id::new(("piano_note_resize", &clip.id, note.event_index)),
                Sense::drag(),
            );
            if resize.drag_stopped() {
                let delta = resize.drag_delta().x / grid.width() * clip.length;
                self.vm.apply(Intent::EditNote {
                    track: track_index,
                    clip: clip_index,
                    event_index: note.event_index,
                    start: note.start,
                    length: ((note.length + delta) * 4.0).round() / 4.0,
                    pitch: note.pitch,
                    velocity: (note.velocity * 127.0).round() as u8,
                });
                self.selected_note = None;
            } else if response.drag_stopped() {
                let delta = response.drag_delta();
                let beat =
                    ((note.start + delta.x / grid.width() * clip.length) * 4.0).round() / 4.0;
                let pitch_delta = (-delta.y / row_height).round() as i16;
                let pitch = (i16::from(note.pitch) + pitch_delta).clamp(0, 127) as u8;
                self.vm.apply(Intent::EditNote {
                    track: track_index,
                    clip: clip_index,
                    event_index: note.event_index,
                    start: beat,
                    length: note.length,
                    pitch,
                    velocity: (note.velocity * 127.0).round() as u8,
                });
                self.selected_note = None;
            }
        }
        if grid_response.double_clicked()
            && !note_under_pointer
            && let Some(pointer) = grid_response.interact_pointer_pos()
            && grid.contains(pointer)
        {
            let start =
                (((pointer.x - grid.left()) / grid.width() * clip.length) * 4.0).floor() / 4.0;
            let pitch = (i16::from(PIANO_HIGH_PITCH)
                - ((pointer.y - grid.top()) / row_height).floor() as i16)
                .clamp(0, 127) as u8;
            self.vm.apply(Intent::AddNote {
                track: track_index,
                clip: clip_index,
                start,
                length: 0.25,
                pitch,
                velocity: self.new_note_velocity,
            });
        }
        if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Delete))
            && let Some(event_index) = self.selected_note.take()
        {
            self.vm.apply(Intent::DeleteNote {
                track: track_index,
                clip: clip_index,
                event_index,
            });
        }
    }

    fn sampler_editor(&mut self, ui: &mut egui::Ui) {
        let Selection::Sampler { track: track_index } = self.vm.selection else {
            return;
        };
        let Some(track) = self
            .vm
            .current_composition()
            .tracks
            .get(track_index)
            .cloned()
        else {
            return;
        };
        let zone_count = track.sampler_zones.len();
        self.selected_sampler_zone = self.selected_sampler_zone.min(zone_count.saturating_sub(1));
        panel_title(
            ui,
            "SAMPLER ZONES",
            &format!("{zone_count} zones · canonical instrument state"),
        );
        let mut polyphony = track.sampler_polyphony.unwrap_or(1);
        let mut voice = track
            .sampler_voice_stealing
            .clone()
            .unwrap_or_else(|| "oldest".into());
        let mut output_gain = track.sampler_output_gain_db.unwrap_or(0.0);
        let mut settings_changed = false;
        ui.horizontal(|ui| {
            settings_changed |= ui
                .add(
                    egui::DragValue::new(&mut polyphony)
                        .range(1..=1024)
                        .prefix("polyphony "),
                )
                .changed();
            egui::ComboBox::from_id_salt(("sampler_voice", &track.id))
                .selected_text(&voice)
                .show_ui(ui, |ui| {
                    for choice in ["oldest", "quietest", "lowest_velocity"] {
                        settings_changed |= ui
                            .selectable_value(&mut voice, choice.into(), choice)
                            .changed();
                    }
                });
            settings_changed |= ui
                .add(
                    egui::DragValue::new(&mut output_gain)
                        .range(-120.0..=24.0)
                        .suffix(" dB output"),
                )
                .changed();
            if ui.small_button("+ ZONE").clicked() {
                self.vm.add_sampler_zone(track_index);
            }
        });
        if settings_changed {
            self.vm
                .update_sampler_settings(track_index, polyphony, &voice, output_gain);
        }
        if zone_count == 0 {
            ui.label(RichText::new("No zones. Add one to map an asset.").color(DIM));
            return;
        }
        let mut deleted_zone = false;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt(("sampler_zone", &track.id))
                .selected_text(&track.sampler_zones[self.selected_sampler_zone].name)
                .show_ui(ui, |ui| {
                    for (index, zone) in track.sampler_zones.iter().enumerate() {
                        ui.push_id(&zone.id, |ui| {
                            ui.selectable_value(&mut self.selected_sampler_zone, index, &zone.name);
                        });
                    }
                });
            if ui.small_button("DELETE ZONE").clicked() {
                self.vm
                    .remove_sampler_zone(track_index, self.selected_sampler_zone);
                self.selected_sampler_zone = self.selected_sampler_zone.saturating_sub(1);
                deleted_zone = true;
            }
        });
        if deleted_zone {
            return;
        }
        let mut zone = track.sampler_zones[self.selected_sampler_zone].clone();
        let zone_id = zone.id.clone();
        let asset_duration = self
            .vm
            .assets
            .iter()
            .find(|asset| asset.id == zone.asset_id)
            .map_or(1.0, |asset| f64::from(asset.duration_seconds));
        let mut changed = false;
        egui::ScrollArea::vertical()
            .id_salt(("sampler_zone_fields", &zone_id))
            .show(ui, |ui| {
                ui.push_id(&zone_id, |ui| {
                    changed |= ui.text_edit_singleline(&mut zone.name).changed();
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new("ASSET").monospace().size(8.0).color(DIM));
                        egui::ComboBox::from_id_salt("asset")
                            .selected_text(
                                self.vm
                                    .assets
                                    .iter()
                                    .find(|asset| asset.id == zone.asset_id)
                                    .map_or(zone.asset_id.as_str(), |asset| asset.name.as_str()),
                            )
                            .show_ui(ui, |ui| {
                                for asset in &self.vm.assets {
                                    changed |= ui
                                        .selectable_value(
                                            &mut zone.asset_id,
                                            asset.id.clone(),
                                            &asset.name,
                                        )
                                        .changed();
                                }
                            });
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut zone.source_start_seconds)
                                    .range(0.0..=asset_duration)
                                    .suffix(" s source start"),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut zone.source_duration_seconds)
                                    .range(0.001..=asset_duration)
                                    .suffix(" s duration"),
                            )
                            .changed();
                    });
                    ui.horizontal_wrapped(|ui| {
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut zone.root_note)
                                    .range(0..=127)
                                    .prefix("root "),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut zone.low_note)
                                    .range(0..=zone.high_note)
                                    .prefix("key low "),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut zone.high_note)
                                    .range(zone.low_note..=127)
                                    .prefix("high "),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut zone.low_velocity)
                                    .range(0..=zone.high_velocity)
                                    .prefix("velocity low "),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut zone.high_velocity)
                                    .range(zone.low_velocity..=127)
                                    .prefix("high "),
                            )
                            .changed();
                    });
                    ui.horizontal_wrapped(|ui| {
                        egui::ComboBox::from_id_salt("playback")
                            .selected_text(if zone.one_shot {
                                "one shot"
                            } else {
                                "note gated"
                            })
                            .show_ui(ui, |ui| {
                                changed |= ui
                                    .selectable_value(&mut zone.one_shot, true, "one shot")
                                    .changed();
                                changed |= ui
                                    .selectable_value(&mut zone.one_shot, false, "note gated")
                                    .changed();
                            });
                        changed |= ui.checkbox(&mut zone.reverse, "reverse").changed();
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut zone.gain_db)
                                    .range(-120.0..=24.0)
                                    .suffix(" dB gain"),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut zone.velocity_sensitivity)
                                    .range(0.0..=1.0)
                                    .suffix(" velocity"),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut zone.attack_ms)
                                    .range(0.0..=60_000.0)
                                    .suffix(" ms attack/fade"),
                            )
                            .changed();
                        changed |= ui
                            .add(
                                egui::DragValue::new(&mut zone.release_ms)
                                    .range(0.0..=60_000.0)
                                    .suffix(" ms release/fade"),
                            )
                            .changed();
                    });
                    ui.horizontal(|ui| {
                        let mut has_choke = zone.choke_group.is_some();
                        if ui.checkbox(&mut has_choke, "choke group").changed() {
                            zone.choke_group = has_choke.then_some(1);
                            changed = true;
                        }
                        if let Some(choke) = &mut zone.choke_group {
                            changed |= ui
                                .add(egui::DragValue::new(choke).range(0..=u16::MAX))
                                .changed();
                        }
                        if self.vm.structure_lens {
                            ui.label(
                                RichText::new(format!("{} · {}", zone.id, zone.structure_path))
                                    .monospace()
                                    .size(8.0)
                                    .color(HIGHLIGHT),
                            );
                        }
                    });
                });
            });
        if changed {
            let selected_asset_duration = self
                .vm
                .assets
                .iter()
                .find(|asset| asset.id == zone.asset_id)
                .map_or(1.0, |asset| f64::from(asset.duration_seconds));
            zone.source_start_seconds = zone
                .source_start_seconds
                .clamp(0.0, selected_asset_duration);
            zone.source_duration_seconds = zone.source_duration_seconds.clamp(
                0.001,
                (selected_asset_duration - zone.source_start_seconds).max(0.001),
            );
            self.vm
                .update_sampler_zone(track_index, self.selected_sampler_zone, &zone);
        }
    }

    fn effect_editor(&mut self, ui: &mut egui::Ui) {
        let Some(current) = self.vm.selected_processor_view() else {
            return;
        };
        panel_title(ui, &current.kind.to_uppercase(), &current.name);
        egui::ScrollArea::vertical()
            .id_salt(("processor_parameters", &current.id))
            .show(ui, |ui| {
                for (parameter_index, parameter) in current.parameters.iter().enumerate() {
                    ui.push_id(&parameter.id, |ui| {
                        egui::Frame::new()
                            .fill(CANVAS)
                            .corner_radius(0)
                            .inner_margin(10)
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(&parameter.label)
                                        .monospace()
                                        .size(9.0)
                                        .color(DIM),
                                );
                                if let Some(value) = parameter_widget(ui, parameter) {
                                    self.vm
                                        .set_selected_processor_parameter(parameter_index, value);
                                }
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(if parameter.automatable {
                                            "AUTOMATABLE"
                                        } else {
                                            "STATIC"
                                        })
                                        .monospace()
                                        .size(8.0)
                                        .color(
                                            if parameter.automatable {
                                                HIGHLIGHT
                                            } else {
                                                DIM
                                            },
                                        ),
                                    );
                                    let lanes =
                                        self.vm.selected_parameter_automation_lanes(&parameter.id);
                                    if lanes > 0 {
                                        ui.label(
                                            RichText::new(format!("{lanes} LANE(S)"))
                                                .monospace()
                                                .size(8.0)
                                                .color(NESTED_TONE),
                                        );
                                    }
                                });
                                if self.vm.structure_lens {
                                    ui.label(
                                        RichText::new(format!(
                                            "{} · {}",
                                            parameter.id, parameter.display_hint
                                        ))
                                        .monospace()
                                        .size(8.0)
                                        .color(HIGHLIGHT),
                                    );
                                }
                            });
                        ui.add_space(4.0);
                    });
                }
            });
    }
}

fn backspace_intent(selection: Selection) -> Intent {
    match selection {
        Selection::Clip { track, clip } => Intent::DeleteClip { track, clip },
        Selection::None
        | Selection::Asset(_)
        | Selection::MidiAsset(_)
        | Selection::Track { .. }
        | Selection::Effect { .. }
        | Selection::Sampler { .. } => Intent::Back,
    }
}

impl eframe::App for GawApp {
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        let now = context.input(|input| input.time);
        let delta = self
            .last_time
            .map_or(0.0, |last| (now - last).clamp(0.0, 0.1) as f32);
        self.last_time = Some(now);
        if self.controller.is_none() {
            self.vm.advance(delta);
        }
        if context.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::S)) {
            let revision = self.vm.revision();
            let project = self.vm.project().clone();
            if let Some(controller) = &mut self.controller {
                controller.save(revision, project);
            }
        }
        self.handle_keyboard(context, now);
        self.pump_controller(context, now);
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
                let shell_width = ui.available_width();
                let shell_height = ui.available_height();
                let (asset_panel_max, signal_panel_max) =
                    side_panel_max_widths(shell_width, self.assets_expanded, self.signal_expanded);
                let forehead_max = forehead_max_height(shell_height);
                egui::Panel::top("forehead")
                    .resizable(true)
                    .default_size(FOREHEAD_DEFAULT_HEIGHT)
                    .size_range(FOREHEAD_MIN_HEIGHT..=forehead_max)
                    .frame(
                        egui::Frame::new()
                            .fill(PANEL)
                            .stroke(Stroke::new(1.0_f32, BORDER)),
                    )
                    .show_inside(ui, |ui| self.transport_bar(ui, now));
                let chin_max = chin_max_height(ui.available_height());
                egui::Panel::bottom("context_editor")
                    .resizable(true)
                    .default_size(EDITOR_DEFAULT_HEIGHT)
                    .size_range(EDITOR_MIN_HEIGHT..=chin_max)
                    .frame(
                        egui::Frame::new()
                            .fill(PANEL)
                            .stroke(Stroke::new(1.0_f32, BORDER))
                            .inner_margin(10),
                    )
                    .show_inside(ui, |ui| self.context_editor(ui));

                if self.assets_expanded && asset_panel_max >= ASSET_PANEL_WIDTH {
                    egui::Panel::left("assets_expanded")
                        .exact_size(ASSET_PANEL_WIDTH)
                        .frame(workspace_panel_frame())
                        .show_inside(ui, |ui| self.asset_browser(ui, now));
                } else {
                    if self.assets_expanded {
                        reset_panel_size(ui.ctx(), "assets_collapsed");
                        self.assets_expanded = false;
                    }
                    let assets = egui::Panel::left("assets_collapsed")
                        .exact_size(COLLAPSED_PANEL_WIDTH)
                        .frame(collapsed_panel_frame())
                        .show_inside(ui, |ui| {
                            collapsed_panel_tab(ui, "A\nS\nS\nE\nT\nS", "›", "Open Assets")
                        });
                    if assets.inner || should_expand_collapsed_column(assets.response.rect.width())
                    {
                        reset_panel_size(ui.ctx(), "assets_expanded");
                        self.assets_expanded = true;
                        if shell_width
                            < ASSET_PANEL_MIN_WIDTH
                                + SIGNAL_PANEL_MIN_WIDTH
                                + MIDDLE_WORKSPACE_MIN_WIDTH
                        {
                            reset_panel_size(ui.ctx(), "signal_collapsed");
                            self.signal_expanded = false;
                        }
                    }
                }

                if self.signal_expanded {
                    let signal = egui::Panel::right("signal_expanded")
                        .resizable(true)
                        .default_size(SIGNAL_PANEL_WIDTH.min(signal_panel_max))
                        .size_range(COLLAPSED_PANEL_WIDTH..=signal_panel_max)
                        .frame(workspace_panel_frame())
                        .show_inside(ui, |ui| {
                            egui::ScrollArea::vertical().show(ui, |ui| self.inspector(ui));
                        });
                    if should_collapse_column(signal.response.rect.width(), SIGNAL_PANEL_MIN_WIDTH)
                    {
                        self.signal_expanded = false;
                    }
                } else {
                    let signal = egui::Panel::right("signal_collapsed")
                        .exact_size(COLLAPSED_PANEL_WIDTH)
                        .frame(collapsed_panel_frame())
                        .show_inside(ui, |ui| {
                            collapsed_panel_tab(ui, "S\nI\nG\nN\nA\nL", "‹", "Open Signal")
                        });
                    if signal.inner {
                        reset_panel_size(ui.ctx(), "signal_expanded");
                        self.signal_expanded = true;
                        if shell_width
                            < ASSET_PANEL_MIN_WIDTH
                                + SIGNAL_PANEL_MIN_WIDTH
                                + MIDDLE_WORKSPACE_MIN_WIDTH
                        {
                            reset_panel_size(ui.ctx(), "assets_collapsed");
                            self.assets_expanded = false;
                        }
                    }
                }

                timeline(
                    ui,
                    &self.vm,
                    &mut self.timeline,
                    now,
                    &mut self.timeline_actions,
                );
                let mut actions = std::mem::take(&mut self.timeline_actions);
                actions.reverse();
                while let Some(action) = actions.pop() {
                    self.handle_timeline_action(action);
                }
                self.timeline_actions = actions;
            });

        self.asset_drop_dialog(&context);

        self.pump_controller(&context, now);
        if let Some(controller) = &self.controller {
            controller.paint_status(&context);
        }

        if self.vm.transport.playing || self.vm.has_active_highlights(now) {
            context.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn on_exit(&mut self) {
        if let Some(controller) = &mut self.controller {
            controller.close(&mut self.vm);
        }
    }
}

fn processor_chooser(
    ui: &mut egui::Ui,
    vm: &mut DemoViewModel,
    stack: &gaw_core::ProcessorStack,
    id_source: impl std::hash::Hash,
) {
    egui::ComboBox::from_id_salt(("processor_chooser", id_source))
        .selected_text("+ PROCESSOR")
        .width(150.0)
        .show_ui(ui, |ui| {
            for (index, (type_id, name)) in DemoViewModel::processor_catalog().iter().enumerate() {
                if ui
                    .selectable_label(false, name)
                    .on_hover_text(type_id)
                    .clicked()
                {
                    vm.insert_processor(stack.clone(), index);
                    ui.close();
                }
            }
        });
}

fn parameter_widget(ui: &mut egui::Ui, parameter: &Parameter) -> Option<serde_json::Value> {
    use gaw_core::ParameterValueType;

    let mut value = parameter.value.clone();
    let mut changed = false;
    match parameter.value_type {
        ParameterValueType::Number => {
            let mut number = value.as_f64()?;
            let (minimum, maximum) = parameter.range.unwrap_or((-1_000_000.0, 1_000_000.0));
            let mut slider = egui::Slider::new(&mut number, minimum..=maximum)
                .text(&parameter.unit)
                .max_decimals(4);
            if parameter.display_hint == "logarithmic" && minimum > 0.0 {
                slider = slider.logarithmic(true);
            }
            changed = ui.add(slider).changed();
            value = serde_json::json!(number);
        }
        ParameterValueType::Integer => {
            if let Some(mut number) = value.as_u64() {
                let (minimum, maximum) = parameter.range.unwrap_or((0.0, u64::MAX as f64));
                changed = ui
                    .add(
                        egui::DragValue::new(&mut number)
                            .range((minimum.max(0.0) as u64)..=(maximum.max(0.0) as u64))
                            .suffix(format!(" {}", parameter.unit)),
                    )
                    .changed();
                value = serde_json::json!(number);
            } else {
                let mut number = value.as_i64()?;
                let (minimum, maximum) = parameter
                    .range
                    .unwrap_or((i64::MIN as f64, i64::MAX as f64));
                changed = ui
                    .add(
                        egui::DragValue::new(&mut number)
                            .range((minimum as i64)..=(maximum as i64))
                            .suffix(format!(" {}", parameter.unit)),
                    )
                    .changed();
                value = serde_json::json!(number);
            }
        }
        ParameterValueType::Boolean => {
            let mut enabled = value.as_bool()?;
            changed = ui.checkbox(&mut enabled, "enabled").changed();
            value = serde_json::json!(enabled);
        }
        ParameterValueType::Choice => {
            let mut choice = value.as_str()?.to_owned();
            egui::ComboBox::from_id_salt("choice")
                .selected_text(choice.replace('_', " "))
                .show_ui(ui, |ui| {
                    for candidate in &parameter.choices {
                        changed |= ui
                            .selectable_value(
                                &mut choice,
                                candidate.clone(),
                                candidate.replace('_', " "),
                            )
                            .changed();
                    }
                });
            value = serde_json::json!(choice);
        }
        ParameterValueType::Time | ParameterValueType::Rate => {
            let mut unit = value.get("unit")?.as_str()?.to_owned();
            let mut number = value.get("value")?.as_f64()?;
            let units: &[&str] = if parameter.value_type == ParameterValueType::Time {
                &["beats", "seconds"]
            } else {
                &["hertz", "beats"]
            };
            let range = if parameter.value_type == ParameterValueType::Rate {
                if unit == "hertz" {
                    0.01..=40.0
                } else {
                    (1.0 / 64.0)..=64.0
                }
            } else {
                let (minimum, maximum) = parameter.range.unwrap_or((0.0, 64.0));
                minimum..=maximum
            };
            ui.horizontal(|ui| {
                changed |= ui
                    .add(egui::DragValue::new(&mut number).speed(0.01).range(range))
                    .changed();
                egui::ComboBox::from_id_salt("unit")
                    .selected_text(&unit)
                    .show_ui(ui, |ui| {
                        for candidate in units {
                            changed |= ui
                                .selectable_value(&mut unit, (*candidate).to_owned(), *candidate)
                                .changed();
                        }
                    });
            });
            value = serde_json::json!({ "unit": unit, "value": number });
        }
        ParameterValueType::List => {
            changed = structured_list_widget(ui, &parameter.id, &mut value);
        }
    }
    changed.then_some(value)
}

fn structured_list_widget(
    ui: &mut egui::Ui,
    parameter_id: &str,
    value: &mut serde_json::Value,
) -> bool {
    let Some(items) = value.as_array_mut() else {
        return false;
    };
    let mut changed = false;
    let mut remove = None;
    let default_open = items.len() <= 8;
    for (index, item) in items.iter_mut().enumerate() {
        ui.push_id(index, |ui| {
            egui::CollapsingHeader::new(format!(
                "{} {}",
                parameter_id.trim_end_matches('s'),
                index + 1
            ))
            .default_open(default_open)
            .show(ui, |ui| {
                if let Some(fields) = item.as_object_mut() {
                    let keys = fields.keys().cloned().collect::<Vec<_>>();
                    for key in keys {
                        let Some(field) = fields.get_mut(&key) else {
                            continue;
                        };
                        ui.horizontal(|ui| {
                            ui.label(key.replace('_', " "));
                            changed |= structured_field_widget(ui, &key, field);
                        });
                    }
                } else {
                    changed |= structured_field_widget(ui, "value", item);
                }
                if ui.small_button("REMOVE").clicked() {
                    remove = Some(index);
                }
            });
        });
    }
    if let Some(index) = remove {
        items.remove(index);
        changed = true;
    }
    let maximum = if parameter_id == "bands" { 8 } else { 64 };
    if items.len() < maximum && ui.small_button("+ ITEM").clicked() {
        items.push(if parameter_id == "bands" {
            serde_json::json!({
                "enabled": true,
                "shape": "bell",
                "frequency_hz": 1000.0,
                "gain_db": 0.0,
                "q": 0.707,
                "slope_db_per_octave": "db12"
            })
        } else {
            serde_json::json!({ "level": 1.0 })
        });
        changed = true;
    }
    changed
}

fn structured_field_widget(ui: &mut egui::Ui, key: &str, value: &mut serde_json::Value) -> bool {
    if let Some(mut enabled) = value.as_bool() {
        let changed = ui.checkbox(&mut enabled, "").changed();
        *value = serde_json::json!(enabled);
        return changed;
    }
    if let Some(mut number) = value.as_f64() {
        let range = match key {
            "frequency_hz" => 10.0..=24_000.0,
            "gain_db" => -24.0..=24.0,
            "q" => 0.1..=30.0,
            "level" => 0.0..=1.0,
            _ => -1_000_000.0..=1_000_000.0,
        };
        let changed = ui
            .add(egui::DragValue::new(&mut number).range(range))
            .changed();
        *value = serde_json::json!(number);
        return changed;
    }
    if let Some(current) = value.as_str() {
        let choices: &[&str] = match key {
            "shape" => &[
                "bell",
                "low_shelf",
                "high_shelf",
                "low_pass",
                "high_pass",
                "band_pass",
                "notch",
            ],
            "slope_db_per_octave" => &["db12", "db24", "db36", "db48"],
            _ => &[],
        };
        let mut selected = current.to_owned();
        let mut changed = false;
        if choices.is_empty() {
            changed = ui.text_edit_singleline(&mut selected).changed();
        } else {
            egui::ComboBox::from_id_salt(key)
                .selected_text(selected.replace('_', " "))
                .show_ui(ui, |ui| {
                    for choice in choices {
                        changed |= ui
                            .selectable_value(&mut selected, (*choice).to_owned(), *choice)
                            .changed();
                    }
                });
        }
        *value = serde_json::json!(selected);
        return changed;
    }
    false
}

fn configure_style(context: &egui::Context) {
    let mut style = (*context.global_style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.weak_text_color = Some(DIM);
    style.visuals.panel_fill = PANEL;
    style.visuals.window_fill = PANEL;
    style.visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.window_corner_radius = CornerRadius::ZERO;
    style.visuals.menu_corner_radius = CornerRadius::ZERO;
    style.visuals.extreme_bg_color = CANVAS;
    style.visuals.faint_bg_color = PANEL_ALT;
    style.visuals.code_bg_color = CANVAS;
    style.visuals.text_edit_bg_color = Some(CANVAS);
    style.visuals.hyperlink_color = HIGHLIGHT;
    style.visuals.warn_fg_color = STATUS_NOTICE;
    style.visuals.error_fg_color = TEXT;
    style.visuals.text_cursor.stroke = Stroke::new(2.0_f32, HIGHLIGHT);
    style.visuals.selection.bg_fill = BORDER_STRONG;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, TEXT);
    style.visuals.widgets.noninteractive.bg_fill = PANEL;
    style.visuals.widgets.noninteractive.weak_bg_fill = PANEL;
    style.visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    style.visuals.widgets.inactive.bg_fill = PANEL_ALT;
    style.visuals.widgets.inactive.weak_bg_fill = PANEL_ALT;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    style.visuals.widgets.hovered.bg_fill = PANEL_RAISED;
    style.visuals.widgets.hovered.weak_bg_fill = PANEL_RAISED;
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, BORDER_STRONG);
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, TEXT);
    style.visuals.widgets.active.bg_fill = BORDER_STRONG;
    style.visuals.widgets.active.weak_bg_fill = BORDER_STRONG;
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, HIGHLIGHT);
    style.visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, TEXT);
    style.visuals.widgets.open = style.visuals.widgets.active;
    for widget in [
        &mut style.visuals.widgets.noninteractive,
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::ZERO;
    }
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
    .fill(if active { BORDER_STRONG } else { PANEL_ALT })
    .min_size(Vec2::splat(29.0))
}

fn format_position(beat: f32, meter: gaw_core::TimeSignature) -> String {
    let unit = 4.0 / f32::from(meter.denominator);
    let bar_length = f32::from(meter.numerator) * unit;
    let bar = (beat / bar_length).floor() as u32 + 1;
    let in_bar = beat.rem_euclid(bar_length) / unit;
    let beat_number = in_bar.floor() as u32 + 1;
    let ticks = (in_bar.fract() * 960.0).floor() as u32;
    format!("{bar:03} · {beat_number} · {ticks:03}")
}

fn format_playhead_time(beat: f32, bpm: f32) -> String {
    let total_milliseconds = if bpm.is_finite() && bpm > 0.0 {
        (beat.max(0.0) * 60_000.0 / bpm).round() as u64
    } else {
        0
    };
    let hours = total_milliseconds / 3_600_000;
    let minutes = total_milliseconds / 60_000 % 60;
    let seconds = total_milliseconds / 1_000 % 60;
    let milliseconds = total_milliseconds % 1_000;
    if hours == 0 {
        format!("{minutes:02}:{seconds:02}.{milliseconds:03}")
    } else {
        format!("{hours:02}:{minutes:02}:{seconds:02}.{milliseconds:03}")
    }
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

fn tempo_map_editor(
    ui: &mut egui::Ui,
    waveform: &[crate::model::WaveformPoint],
    sections: &mut [TempoSectionDraft],
    preview_seconds: Option<f64>,
) -> Option<f64> {
    if sections.is_empty() {
        return None;
    }
    let duration = sections
        .last()
        .map_or(0.0, |section| section.end_seconds)
        .max(f64::EPSILON);
    let (rect, waveform_response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 150.0), Sense::click());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, CANVAS);
    let waveform_rect = rect.shrink2(Vec2::new(8.0, 25.0));
    for (index, section) in sections.iter().copied().enumerate() {
        let left = egui::lerp(
            waveform_rect.x_range(),
            (section.start_seconds / duration).clamp(0.0, 1.0) as f32,
        );
        let right = egui::lerp(
            waveform_rect.x_range(),
            (section.end_seconds / duration).clamp(0.0, 1.0) as f32,
        );
        let section_rect = Rect::from_x_y_ranges(left..=right, waveform_rect.y_range());
        let detected = section.detection.is_some();
        let shade = if detected {
            if index % 2 == 0 { 58 } else { 68 }
        } else {
            27
        };
        ui.painter()
            .rect_filled(section_rect, CornerRadius::ZERO, Color32::from_gray(shade));
        let clipped = ui.painter().with_clip_rect(section_rect);
        paint_waveform(
            &clipped,
            waveform_rect,
            waveform,
            if detected { TEXT } else { DIM },
        );
        let label = match (section.bpm(), section.detection) {
            (Some(bpm), Some(detection)) => {
                format!("{bpm:.1} BPM  ·  {:.0}%", detection.confidence * 100.0)
            }
            _ => "NO BPM DETECTED".to_owned(),
        };
        clipped.text(
            Pos2::new(section_rect.center().x, section_rect.top() + 6.0),
            Align2::CENTER_TOP,
            label,
            FontId::monospace(9.0),
            if detected { TEMPO_LABEL } else { DIM },
        );
    }
    ui.painter().rect_stroke(
        waveform_rect,
        CornerRadius::ZERO,
        Stroke::new(1.0, BORDER_STRONG),
        StrokeKind::Inside,
    );
    let preview_x = preview_seconds.map(|seconds| {
        let x = egui::lerp(
            waveform_rect.x_range(),
            (seconds / duration).clamp(0.0, 1.0) as f32,
        );
        ui.painter()
            .vline(x, waveform_rect.y_range(), Stroke::new(1.5, Color32::WHITE));
        ui.painter().add(egui::Shape::convex_polygon(
            vec![
                Pos2::new(x - 4.0, waveform_rect.top()),
                Pos2::new(x + 4.0, waveform_rect.top()),
                Pos2::new(x, waveform_rect.top() + 6.0),
            ],
            Color32::WHITE,
            Stroke::NONE,
        ));
        x
    });
    let mut boundary_active = false;
    for index in 0..sections.len().saturating_sub(1) {
        let x = egui::lerp(
            waveform_rect.x_range(),
            (sections[index].end_seconds / duration).clamp(0.0, 1.0) as f32,
        );
        let handle_rect = Rect::from_center_size(
            Pos2::new(x, waveform_rect.center().y),
            Vec2::new(10.0, waveform_rect.height()),
        );
        let response = ui.interact(
            handle_rect,
            ui.id().with(("tempo_boundary", index)),
            Sense::drag(),
        );
        boundary_active |= response.dragged() || response.drag_started();
        ui.painter().vline(
            x,
            waveform_rect.y_range(),
            Stroke::new(if response.hovered() { 2.0 } else { 1.0 }, Color32::WHITE),
        );
        if response.dragged()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let minimum = sections[index].start_seconds + 1.0;
            let maximum = (sections[index + 1].end_seconds - 1.0).max(minimum);
            let boundary = (f64::from(
                ((pointer.x - waveform_rect.left()) / waveform_rect.width()).clamp(0.0, 1.0),
            ) * duration)
                .clamp(minimum, maximum);
            sections[index].end_seconds = boundary;
            sections[index + 1].start_seconds = boundary;
        }
    }
    if let Some(x) = preview_x {
        let playhead_handle = Rect::from_center_size(
            Pos2::new(x, waveform_rect.center().y),
            Vec2::new(14.0, waveform_rect.height()),
        );
        let response = ui
            .interact(
                playhead_handle,
                ui.id().with("tempo_preview_playhead"),
                Sense::drag(),
            )
            .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        if response.dragged()
            && let Some(pointer) = response.interact_pointer_pos()
        {
            let fraction =
                ((pointer.x - waveform_rect.left()) / waveform_rect.width()).clamp(0.0, 1.0);
            return Some(f64::from(fraction) * duration);
        }
    }
    if !boundary_active
        && waveform_response.clicked()
        && let Some(pointer) = waveform_response.interact_pointer_pos()
    {
        let fraction = ((pointer.x - waveform_rect.left()) / waveform_rect.width()).clamp(0.0, 1.0);
        return Some(f64::from(fraction) * duration);
    }
    None
}

fn tempo_sections_editor(
    ui: &mut egui::Ui,
    sections: &mut [TempoSectionDraft],
) -> Option<(f64, f64)> {
    let mut audition = None;
    for index in 0..sections.len() {
        let draft = sections[index];
        egui::Frame::new()
            .fill(PANEL_ALT)
            .stroke(Stroke::new(1.0, BORDER))
            .inner_margin(8)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("SECTION {}", index + 1))
                            .monospace()
                            .size(9.0)
                            .color(DIM),
                    );
                    ui.label(format!(
                        "{} – {}",
                        format_audio_time(draft.start_seconds),
                        format_audio_time(draft.end_seconds)
                    ));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let status = draft.detection.map_or_else(
                            || "NO BPM DETECTED".to_owned(),
                            |detection| format!("{:.0}% confidence", detection.confidence * 100.0),
                        );
                        ui.label(RichText::new(status).monospace().size(8.5).color(DIM));
                    });
                });
                if let Some(detection) = draft.detection {
                    let candidates = [
                        Some(detection.bpm),
                        detection.alternatives[0],
                        detection.alternatives[1],
                    ];
                    let labels = ["Single-time", "Half-time", "Double-time"];
                    egui::ComboBox::from_id_salt(("section_tempo", index))
                        .selected_text(format!("{:.1} BPM", draft.bpm().unwrap_or(detection.bpm)))
                        .show_ui(ui, |ui| {
                            for (candidate_index, candidate) in candidates.into_iter().enumerate() {
                                if let Some(bpm) = candidate
                                    && ui
                                        .selectable_label(
                                            sections[index].selected == candidate_index,
                                            format!("{} · {bpm:.1} BPM", labels[candidate_index]),
                                        )
                                        .clicked()
                                {
                                    sections[index].selected = candidate_index;
                                }
                            }
                        });
                }
                if ui.small_button("▶ AUDITION SECTION").clicked() {
                    audition = Some((draft.start_seconds, draft.end_seconds));
                }
                if index + 1 < sections.len() {
                    let minimum = draft.start_seconds + 1.0;
                    let maximum = (sections[index + 1].end_seconds - 1.0).max(minimum);
                    let mut boundary = draft.end_seconds.clamp(minimum, maximum);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("SPLIT AT").monospace().size(8.5).color(DIM));
                        if ui
                            .add(
                                egui::DragValue::new(&mut boundary)
                                    .range(minimum..=maximum)
                                    .speed(0.1)
                                    .suffix(" s"),
                            )
                            .changed()
                        {
                            sections[index].end_seconds = boundary;
                            sections[index + 1].start_seconds = boundary;
                        }
                    });
                }
            });
        ui.add_space(6.0);
    }
    audition
}

fn format_audio_time(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

fn format_preview_time(seconds: f64) -> String {
    let milliseconds = (seconds.max(0.0) * 1_000.0).round() as u64;
    let minutes = milliseconds / 60_000;
    let seconds = milliseconds / 1_000 % 60;
    let tenths = milliseconds / 100 % 10;
    format!("{minutes}:{seconds:02}.{tenths}")
}

fn tempo_unreliable_message(unreliable: gaw_audio::TempoUnreliable) -> String {
    let reason = match unreliable.reason {
        gaw_audio::TempoUnreliableReason::WeakPulse => "no stable rhythmic pulse was found",
        gaw_audio::TempoUnreliableReason::CompetingTempos => {
            "multiple unrelated tempo families remain similarly likely"
        }
        gaw_audio::TempoUnreliableReason::UnstableTempo => {
            "the tempo changes too often to form stable regions"
        }
    };
    unreliable.best.map_or_else(
        || format!("Tempo could not be detected reliably: {reason}."),
        |best| {
            format!(
                "Tempo could not be detected reliably: {reason} ({:.0}% family confidence; {:.0}% competing).",
                best.confidence * 100.0,
                best.runner_up_confidence * 100.0
            )
        },
    )
}

fn tempo_media_regions(
    asset: &crate::model::Asset,
    drafts: &[TempoSectionDraft],
) -> Option<Vec<gaw_project::MediaRegion>> {
    let sample_rate = f64::from(asset.sample_rate);
    if sample_rate <= 0.0 || asset.frames == 0 {
        return None;
    }
    let mut previous_start = 0_u64;
    let regions = drafts
        .iter()
        .filter_map(|draft| {
            let bpm = draft.bpm()?;
            let start = ((draft.start_seconds - TEMPO_REGION_PADDING_SECONDS) * sample_rate)
                .round()
                .clamp(0.0, asset.frames as f64) as u64;
            let end = ((draft.end_seconds + TEMPO_REGION_PADDING_SECONDS) * sample_rate)
                .round()
                .clamp(0.0, asset.frames as f64) as u64;
            if start < previous_start || end <= start {
                return Some(None);
            }
            previous_start = start;
            Some(Some(gaw_project::MediaRegion {
                range: gaw_core::FrameRange {
                    start: gaw_core::FramePosition(start),
                    length: gaw_core::FrameCount(end - start),
                },
                bpm: gaw_core::Bpm::new(f64::from(bpm)).ok()?,
            }))
        })
        .collect::<Option<Vec<_>>>()?;
    (!regions.is_empty()).then_some(regions)
}

fn paint_ellipsized_text(
    painter: &egui::Painter,
    position: Pos2,
    text: &str,
    font: FontId,
    color: Color32,
    max_width: f32,
) {
    let full = painter.layout_no_wrap(text.to_owned(), font.clone(), color);
    if full.size().x <= max_width {
        painter.galley(position, full, color);
        return;
    }

    let boundaries = text
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let mut low = 0;
    let mut high = boundaries.len() - 1;
    while low < high {
        let middle = (low + high).div_ceil(2);
        let candidate = format!("{}…", &text[..boundaries[middle]]);
        if painter
            .layout_no_wrap(candidate, font.clone(), color)
            .size()
            .x
            <= max_width
        {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    let candidate = format!("{}…", &text[..boundaries[low]]);
    let galley = painter.layout_no_wrap(candidate, font, color);
    if galley.size().x <= max_width {
        painter.galley(position, galley, color);
    }
}

#[derive(Clone, Copy, Debug)]
enum AssetMenuAction {
    Import,
    AddToTimeline(usize),
    AddMidiToTimeline(usize),
    Rename(usize),
    Delete(usize),
    SetBpm(usize),
    ConvertToMidi(usize),
    StemSplitter(usize),
    Reveal(usize),
}

fn midi_asset_context_menu(
    response: &egui::Response,
    index: usize,
    action: &mut Option<AssetMenuAction>,
) {
    response.context_menu(|ui| {
        if ui.button("ADD TO TIMELINE").clicked() {
            *action = Some(AssetMenuAction::AddMidiToTimeline(index));
            ui.close();
        }
    });
}

fn asset_context_menu(
    response: &egui::Response,
    enabled: bool,
    asset_index: Option<usize>,
    transcribing: bool,
    splitting_stems: bool,
    action: &mut Option<AssetMenuAction>,
) {
    response.context_menu(|ui| {
        if let Some(index) = asset_index {
            if ui.button("ADD TO TIMELINE").clicked() {
                *action = Some(AssetMenuAction::AddToTimeline(index));
                ui.close();
            }
            if ui.button("RENAME…").clicked() {
                *action = Some(AssetMenuAction::Rename(index));
                ui.close();
            }
            if ui.button("SET TEMPO…").clicked() {
                *action = Some(AssetMenuAction::SetBpm(index));
                ui.close();
            }
            let convert = ui
                .add_enabled(
                    enabled && !transcribing,
                    egui::Button::new(if transcribing {
                        "CONVERTING TO MIDI…"
                    } else {
                        "CONVERT TO MIDI"
                    }),
                )
                .on_disabled_hover_text(if transcribing {
                    "Basic Pitch is already converting this asset"
                } else {
                    "This audio asset is not materialized"
                });
            if convert.clicked() {
                *action = Some(AssetMenuAction::ConvertToMidi(index));
                ui.close();
            }
            let split = ui
                .add_enabled(
                    enabled && !splitting_stems,
                    egui::Button::new(if splitting_stems {
                        "SPLITTING STEMS…"
                    } else {
                        "STEM SPLITTER…"
                    }),
                )
                .on_disabled_hover_text(if splitting_stems {
                    "X-LANCE is already splitting this asset"
                } else {
                    "This audio asset is not materialized"
                });
            if split.clicked() {
                *action = Some(AssetMenuAction::StemSplitter(index));
                ui.close();
            }
            if ui.button("REVEAL IN FILE MANAGER").clicked() {
                *action = Some(AssetMenuAction::Reveal(index));
                ui.close();
            }
            ui.separator();
            if ui.button("DELETE").clicked() {
                *action = Some(AssetMenuAction::Delete(index));
                ui.close();
            }
        } else {
            let add = ui
                .add_enabled(enabled, egui::Button::new("ADD AUDIO ASSET…"))
                .on_disabled_hover_text("Open a persistent project to import audio");
            if add.clicked() {
                *action = Some(AssetMenuAction::Import);
                ui.close();
            }
        }
    });
}

fn reveal_path(path: &Path) {
    let directory = path.parent().unwrap_or(path);
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open")
        .arg(directory)
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(directory).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer")
        .arg(directory)
        .spawn();
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
    ui.painter().rect_filled(rect, CornerRadius::ZERO, fill);
    ui.painter().rect_stroke(
        rect,
        CornerRadius::ZERO,
        Stroke::new(
            1.0_f32,
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
    ui.painter().vline(
        rect.center().x,
        rect.y_range(),
        Stroke::new(1.0_f32, BORDER),
    );
}

fn property(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(9.5).color(DIM));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).monospace().size(8.8).color(TEXT));
        });
    });
}

fn workspace_panel_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(PANEL)
        .inner_margin(WORKSPACE_PANEL_MARGIN)
}

fn collapsed_panel_frame() -> egui::Frame {
    egui::Frame::new().fill(PANEL)
}

fn collapsed_panel_tab(ui: &mut egui::Ui, label: &str, arrow: &str, hover: &str) -> bool {
    let (rect, response) = ui.allocate_exact_size(ui.available_size(), Sense::click());
    let header = Rect::from_min_max(
        rect.left_top(),
        Pos2::new(rect.right(), (rect.top() + 30.0).min(rect.bottom())),
    );
    ui.painter()
        .rect_filled(header, CornerRadius::ZERO, PANEL_ALT);
    ui.painter().text(
        header.center(),
        Align2::CENTER_CENTER,
        arrow,
        FontId::monospace(13.0),
        DIM,
    );
    ui.painter().text(
        Pos2::new(rect.center().x, header.bottom() + 14.0),
        Align2::CENTER_TOP,
        label,
        FontId::monospace(9.0),
        DIM,
    );
    let response = response.on_hover_text(hover);
    response.clicked()
}

fn asset_column_title(ui: &mut egui::Ui, title: &str, detail: &str) -> bool {
    let (content_rect, _) = ui.allocate_exact_size(
        Vec2::new(
            ui.available_width(),
            COLUMN_HEADER_HEIGHT - WORKSPACE_PANEL_MARGIN,
        ),
        Sense::hover(),
    );
    let header = Rect::from_min_max(
        Pos2::new(
            content_rect.left() - WORKSPACE_PANEL_MARGIN,
            content_rect.top() - WORKSPACE_PANEL_MARGIN,
        ),
        Pos2::new(
            content_rect.right() + WORKSPACE_PANEL_MARGIN,
            content_rect.bottom(),
        ),
    );
    let painter = ui.painter();
    painter.rect_filled(header, CornerRadius::ZERO, PANEL_ALT);
    painter.hline(header.x_range(), header.bottom(), Stroke::new(1.0, BORDER));
    painter.text(
        header.left_center() + Vec2::new(12.0, 0.0),
        Align2::LEFT_CENTER,
        title,
        FontId::monospace(9.0),
        DIM,
    );
    painter.text(
        header.right_center() - Vec2::new(12.0, 0.0),
        Align2::RIGHT_CENTER,
        detail,
        FontId::monospace(8.5),
        DIM,
    );
    ui.interact(header, ui.id().with("collapse_assets"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Collapse Assets")
        .clicked()
}

fn should_collapse_column(width: f32, useful_minimum: f32) -> bool {
    width + 0.5 < useful_minimum
}

fn should_expand_collapsed_column(width: f32) -> bool {
    width > COLLAPSED_PANEL_WIDTH + COLLAPSED_PANEL_PULL_THRESHOLD
}

fn reset_panel_size(context: &egui::Context, id: &'static str) {
    context.data_mut(|data| data.remove::<egui::PanelState>(egui::Id::new(id)));
}

fn metric(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    egui::Frame::new()
        .fill(CANVAS)
        .corner_radius(0)
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
    let forehead_max = forehead_max_height(bounds.height());
    let top_height = FOREHEAD_DEFAULT_HEIGHT.clamp(FOREHEAD_MIN_HEIGHT, forehead_max);
    let chin_max = chin_max_height(bounds.height() - top_height);
    let bottom_height = editor_height.clamp(
        EDITOR_MIN_HEIGHT,
        chin_max.min((bounds.height() - top_height - MIDDLE_MIN_HEIGHT).max(EDITOR_MIN_HEIGHT)),
    );
    let body = Rect::from_min_max(
        Pos2::new(bounds.left(), bounds.top() + top_height),
        Pos2::new(bounds.right(), bounds.bottom() - bottom_height),
    );
    let left_width = ASSET_PANEL_WIDTH.min(body.width() * 0.28);
    let right_width = SIGNAL_PANEL_WIDTH.min(body.width() * 0.34);
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

    fn assert_grayscale(color: Color32) {
        assert_eq!(color.r(), color.g());
        assert_eq!(color.g(), color.b());
    }

    #[test]
    fn configured_style_is_square_and_grayscale() {
        let context = egui::Context::default();
        configure_style(&context);
        let style = context.global_style();
        let visuals = &style.visuals;
        for color in [
            visuals.panel_fill,
            visuals.window_fill,
            visuals.window_stroke.color,
            visuals.extreme_bg_color,
            visuals.faint_bg_color,
            visuals.code_bg_color,
            visuals.hyperlink_color,
            visuals.warn_fg_color,
            visuals.error_fg_color,
            visuals.selection.bg_fill,
            visuals.selection.stroke.color,
            visuals.text_cursor.stroke.color,
        ] {
            assert_grayscale(color);
        }
        assert_eq!(visuals.window_corner_radius, CornerRadius::ZERO);
        assert_eq!(visuals.menu_corner_radius, CornerRadius::ZERO);
        for widget in [
            visuals.widgets.noninteractive,
            visuals.widgets.inactive,
            visuals.widgets.hovered,
            visuals.widgets.active,
            visuals.widgets.open,
        ] {
            assert_eq!(widget.corner_radius, CornerRadius::ZERO);
            for color in [
                widget.bg_fill,
                widget.weak_bg_fill,
                widget.bg_stroke.color,
                widget.fg_stroke.color,
            ] {
                assert_grayscale(color);
            }
        }
    }

    #[test]
    fn position_format_is_musical() {
        let common = gaw_core::TimeSignature::default();
        assert_eq!(format_position(0.0, common), "001 · 1 · 000");
        assert_eq!(format_position(5.5, common), "002 · 2 · 480");
        assert_eq!(format_position(0.999_99, common), "001 · 1 · 959");

        let three_four = gaw_core::TimeSignature::new(3, 4).unwrap();
        assert_eq!(format_position(3.0, three_four), "002 · 1 · 000");
        assert_eq!(format_position(5.5, three_four), "002 · 3 · 480");

        let six_eight = gaw_core::TimeSignature::new(6, 8).unwrap();
        assert_eq!(format_position(0.5, six_eight), "001 · 2 · 000");
        assert_eq!(format_position(3.0, six_eight), "002 · 1 · 000");
    }

    #[test]
    fn playhead_time_format_tracks_project_tempo() {
        assert_eq!(format_playhead_time(0.0, 120.0), "00:00.000");
        assert_eq!(format_playhead_time(5.0, 120.0), "00:02.500");
        assert_eq!(format_playhead_time(7_200.0, 120.0), "01:00:00.000");
        assert_eq!(format_playhead_time(-1.0, 120.0), "00:00.000");
    }

    #[test]
    fn tempo_prompt_ignores_rounding_noise_but_catches_real_mismatches() {
        assert!(!tempo_mismatch(120.0, 120.0));
        assert!(!tempo_mismatch(119.95, 120.0));
        assert!(tempo_mismatch(119.0, 120.0));
        assert!(tempo_mismatch(60.0, 120.0));
    }

    #[test]
    fn tempo_prompt_only_opens_for_a_known_mismatch() {
        assert_eq!(
            drop_tempo_decision(None, 120.0),
            DropTempoDecision::Apply(gaw_core::TempoSync::None)
        );
        assert_eq!(
            drop_tempo_decision(Some(120.0), 120.0),
            DropTempoDecision::Apply(gaw_core::TempoSync::Stretch)
        );
        assert_eq!(
            drop_tempo_decision(Some(119.95), 120.0),
            DropTempoDecision::Apply(gaw_core::TempoSync::Stretch)
        );
        assert_eq!(
            drop_tempo_decision(Some(110.0), 120.0),
            DropTempoDecision::Prompt(110.0)
        );
    }

    #[test]
    fn preview_time_format_is_compact_and_stable() {
        assert_eq!(format_preview_time(0.0), "0:00.0");
        assert_eq!(format_preview_time(82.73), "1:22.7");
        assert_eq!(format_preview_time(3_661.09), "61:01.0");
        assert_eq!(format_preview_time(f64::NAN), "0:00.0");
    }

    #[test]
    fn backspace_deletes_the_selected_clip_and_otherwise_navigates_back() {
        assert!(matches!(
            backspace_intent(Selection::Clip { track: 2, clip: 3 }),
            Intent::DeleteClip { track: 2, clip: 3 }
        ));
        assert!(matches!(backspace_intent(Selection::None), Intent::Back));
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

    #[test]
    fn side_panels_leave_room_for_a_split_screen_arrangement() {
        let shell_width = 952.0;
        let (assets, inspector) = side_panel_max_widths(shell_width, true, true);
        assert_eq!(assets, ASSET_PANEL_WIDTH);
        assert!(shell_width - assets - inspector >= 460.0);

        let (narrow_assets, narrow_signal) = side_panel_max_widths(700.0, true, true);
        assert!(narrow_assets < ASSET_PANEL_MIN_WIDTH);
        assert!(narrow_signal < SIGNAL_PANEL_MIN_WIDTH);

        let (reopened_assets, _) = side_panel_max_widths(700.0, true, false);
        assert_eq!(reopened_assets, ASSET_PANEL_WIDTH);

        let (_, reopened_signal) = side_panel_max_widths(700.0, false, true);
        assert!(reopened_signal >= SIGNAL_PANEL_MIN_WIDTH);
    }

    #[test]
    fn vertical_panels_preserve_a_useful_middle() {
        for shell_height in [640.0, 900.0, 1_200.0] {
            let forehead_max = forehead_max_height(shell_height);
            let chin_max = chin_max_height(shell_height - forehead_max);
            assert!(forehead_max >= FOREHEAD_MIN_HEIGHT);
            assert!(chin_max >= EDITOR_MIN_HEIGHT);
            assert!(shell_height - forehead_max - chin_max >= MIDDLE_MIN_HEIGHT);
        }
    }

    #[test]
    fn side_columns_collapse_only_below_their_useful_minimum() {
        assert!(!should_collapse_column(
            ASSET_PANEL_MIN_WIDTH,
            ASSET_PANEL_MIN_WIDTH
        ));
        assert!(should_collapse_column(
            ASSET_PANEL_MIN_WIDTH - 1.0,
            ASSET_PANEL_MIN_WIDTH
        ));
        assert!(!should_collapse_column(
            SIGNAL_PANEL_MIN_WIDTH,
            SIGNAL_PANEL_MIN_WIDTH
        ));
        assert!(!should_expand_collapsed_column(COLLAPSED_PANEL_WIDTH));
        assert!(should_expand_collapsed_column(
            COLLAPSED_PANEL_WIDTH + COLLAPSED_PANEL_PULL_THRESHOLD + 1.0
        ));
    }

    #[test]
    fn tempo_sections_materialize_detected_ranges_and_skip_uncertain_audio() {
        let mut asset = DemoViewModel::demo().assets[0].clone();
        asset.sample_rate = 48_000;
        asset.frames = 480_000;
        let detection = gaw_audio::BpmDetection {
            bpm: 120.0,
            confidence: 0.8,
            runner_up_confidence: 0.1,
            alternatives: [Some(60.0), Some(240.0)],
        };
        let drafts = [
            TempoSectionDraft {
                start_seconds: 0.0,
                end_seconds: 4.0,
                detection: Some(detection),
                selected: 0,
            },
            TempoSectionDraft {
                start_seconds: 4.0,
                end_seconds: 6.0,
                detection: None,
                selected: 0,
            },
            TempoSectionDraft {
                start_seconds: 6.0,
                end_seconds: 10.0,
                detection: Some(detection),
                selected: 1,
            },
        ];
        let regions = tempo_media_regions(&asset, &drafts).expect("valid regions");
        assert_eq!(regions[0].range.start.0, 0);
        assert_eq!(regions[0].range.length.0, 288_000);
        assert_eq!(regions[1].range.start.0, 192_000);
        assert_eq!(regions[1].range.length.0, 288_000);
        assert!((regions[1].bpm.value() - 60.0).abs() < f64::EPSILON);
    }
}
