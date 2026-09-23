//! Centered sample picker and nondestructive waveform slicing.
use super::{AUDIO_TONE, DIM, GawApp, PANEL, PANEL_ALT, RichText, STATUS_ERROR, TEXT, egui};
use crate::model::{Asset, SamplerZone, Track};
use gaw_core::{AssetId, SamplerZoneId, TrackId};

mod waveform;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(super) struct SamplerEditor {
    track_id: TrackId,
    zone_id: Option<SamplerZoneId>,
    draft: Option<SamplerZone>,
    baseline: Option<SamplerZone>,
    query: String,
    preview_path: Option<String>,
    preview_pending: Option<(f64, f64)>,
    waveform_revision: u64,
    error: Option<String>,
}

impl SamplerEditor {
    fn sync(&mut self, track: &Track) {
        if self.zone_id.is_some_and(|id| {
            !track
                .sampler_zones
                .iter()
                .any(|zone| zone.id == id.to_string())
        }) {
            self.select_zone(track.sampler_zones.first());
            return;
        }
        let zone = self
            .zone_id
            .and_then(|id| {
                track
                    .sampler_zones
                    .iter()
                    .find(|zone| zone.id == id.to_string())
            })
            .cloned();
        if zone != self.baseline {
            self.draft.clone_from(&zone);
            self.baseline = zone;
            self.waveform_revision = self.waveform_revision.wrapping_add(1);
        }
    }

    fn select_zone(&mut self, zone: Option<&SamplerZone>) {
        self.zone_id = zone.and_then(|zone| zone.id.parse().ok());
        self.draft = zone.cloned();
        self.baseline = zone.cloned();
        self.waveform_revision = self.waveform_revision.wrapping_add(1);
        self.error = None;
    }
}

fn source_duration(asset: &Asset) -> f64 {
    if asset.frames > 0 && asset.sample_rate > 0 {
        asset.frames as f64 / f64::from(asset.sample_rate)
    } else {
        f64::from(asset.duration_seconds).max(0.0)
    }
}

fn sampler_playback_controls(
    ui: &mut egui::Ui,
    available: bool,
    active: bool,
    loading: bool,
    position: f64,
    duration: f64,
) -> bool {
    let shortcut = available
        && !ui.ctx().egui_wants_keyboard_input()
        && !egui::Popup::is_any_open(ui.ctx())
        && ui.input_mut(|input| {
            let pressed = input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::Space,
                        pressed: true,
                        repeat: false,
                        ..
                    }
                )
            });
            input.consume_key(egui::Modifiers::NONE, egui::Key::Space) && pressed
        });
    let clicked = ui
        .horizontal(|ui| {
            let response = ui.add_enabled(
                available,
                egui::Button::new(if active { "Stop" } else { "Play" }),
            );
            let clicked = response
                .on_hover_text("Play the selected slice · Space")
                .clicked();
            if loading {
                ui.spinner();
            }
            ui.label(
                RichText::new(format!("{position:.2} / {duration:.2} s"))
                    .monospace()
                    .small()
                    .color(DIM),
            );
            clicked
        })
        .inner;
    clicked || shortcut
}

fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        NAMES[usize::from(note % 12)],
        i16::from(note / 12) - 1
    )
}

impl GawApp {
    pub(super) fn open_sampler(&mut self, track_index: usize, now: f64) {
        let Some(track) = self.vm.current_composition().tracks.get(track_index) else {
            return;
        };
        if track.sampler_polyphony.is_none() {
            return;
        }
        let Ok(track_id) = track.id.parse() else {
            return;
        };
        let zone = track.sampler_zones.first().cloned();
        self.finish_keyboard_take(now);
        self.sampler_editor = Some(SamplerEditor {
            track_id,
            zone_id: zone.as_ref().and_then(|zone| zone.id.parse().ok()),
            draft: zone.clone(),
            baseline: zone,
            query: String::new(),
            preview_path: None,
            preview_pending: None,
            waveform_revision: 0,
            error: None,
        });
    }

    pub(super) fn sampler_window(&mut self, context: &egui::Context, now: f64) {
        let Some(mut editor) = self.sampler_editor.take() else {
            return;
        };
        // Stable IDs keep an external reorder from retargeting edits to another instrument.
        let Some(track_index) = self
            .vm
            .current_composition()
            .tracks
            .iter()
            .position(|track| track.id == editor.track_id.to_string())
        else {
            self.end_sampler_preview(&editor);
            return;
        };
        let track = self.vm.current_composition().tracks[track_index].clone();
        let previous_asset = editor.baseline.as_ref().map(|zone| zone.asset_id.clone());
        editor.sync(&track);
        if previous_asset != editor.baseline.as_ref().map(|zone| zone.asset_id.clone()) {
            self.stop_sampler_preview(&mut editor);
        }
        crate::physical_keyboard::set_capture(context, false);
        if let Some(controller) = &mut self.controller
            && editor.preview_pending.is_some()
            && let Some(status) = controller.asset_preview_status()
            && !status.loading
        {
            if let Some(error) = status.error {
                editor.error = Some(error);
                editor.preview_pending = None;
            } else if let Some((start, end)) = editor.preview_pending.take() {
                controller.play_asset_preview_range(start, end);
            }
        }
        let width = (context.content_rect().width() - 48.0).clamp(520.0, 920.0);
        let body_height = (context.content_rect().height() - 180.0).clamp(280.0, 520.0);
        let mut close = false;
        let response = egui::Modal::new(egui::Id::new("sampler-modal"))
            .frame(
                egui::Frame::popup(&context.global_style())
                    .fill(PANEL)
                    .inner_margin(18),
            )
            .show(context, |ui| {
                ui.set_width(width);
                ui.spacing_mut().item_spacing = egui::vec2(10.0, 10.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Sample").size(19.0).strong());
                    ui.label(RichText::new(&track.name).color(DIM));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        close = ui.button("Done").clicked();
                        if ui
                            .small_button("+")
                            .on_hover_text("Add a sample layer")
                            .clicked()
                        {
                            self.stop_sampler_preview(&mut editor);
                            editor.select_zone(None);
                        }
                        if track.sampler_zones.len() > 1
                            || (!track.sampler_zones.is_empty() && editor.zone_id.is_none())
                        {
                            let mut chosen = editor.zone_id;
                            egui::ComboBox::from_id_salt("sample-layer")
                                .selected_text(
                                    editor
                                        .draft
                                        .as_ref()
                                        .map_or("New layer", |zone| zone.name.as_str()),
                                )
                                .width(140.0)
                                .show_ui(ui, |ui| {
                                    for zone in &track.sampler_zones {
                                        ui.selectable_value(
                                            &mut chosen,
                                            zone.id.parse().ok(),
                                            &zone.name,
                                        );
                                    }
                                });
                            if chosen != editor.zone_id {
                                self.stop_sampler_preview(&mut editor);
                                editor.select_zone(track.sampler_zones.iter().find(|zone| {
                                    chosen.is_some_and(|id| zone.id == id.to_string())
                                }));
                            }
                        }
                    });
                });
                ui.separator();
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(220.0, body_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(220.0);
                            self.sampler_asset_picker(ui, &mut editor, body_height);
                        },
                    );
                    ui.separator();
                    let right_width = (ui.available_width() - 4.0).max(250.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(right_width, body_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(right_width);
                            egui::ScrollArea::vertical()
                                .id_salt("sample-controls")
                                .max_height(body_height)
                                .show(ui, |ui| {
                                    self.sampler_sample_controls(
                                        ui,
                                        &mut editor,
                                        track_index,
                                        &track,
                                        now,
                                    );
                                });
                        },
                    );
                });
                if let Some(error) = &editor.error {
                    ui.colored_label(STATUS_ERROR, error);
                }
            });
        if close || response.should_close() {
            self.end_sampler_preview(&editor);
        } else {
            self.sampler_editor = Some(editor);
        }
    }

    fn sampler_asset_picker(&mut self, ui: &mut egui::Ui, editor: &mut SamplerEditor, height: f32) {
        ui.horizontal(|ui| {
            ui.add_sized(
                [150.0, 24.0],
                egui::TextEdit::singleline(&mut editor.query).hint_text("Find audio…"),
            );
            if ui
                .add_enabled(self.controller.is_some(), egui::Button::new("Import"))
                .clicked()
            {
                self.pick_audio_asset();
            }
        });
        let query = editor.query.to_lowercase();
        let assets: Vec<_> = self
            .vm
            .assets
            .iter()
            .filter(|asset| asset.name.to_lowercase().contains(&query))
            .cloned()
            .collect();
        egui::ScrollArea::vertical()
            .id_salt("sample-picker")
            .max_height(height - 42.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if assets.is_empty() {
                    ui.label(
                        RichText::new(if self.vm.assets.is_empty() {
                            "Import audio to get started."
                        } else {
                            "No matches"
                        })
                        .color(DIM),
                    );
                }
                for asset in &assets {
                    let selected = editor
                        .draft
                        .as_ref()
                        .is_some_and(|zone| zone.asset_id == asset.id);
                    let available = source_duration(asset) > 0.0;
                    let row = egui::Frame::new()
                        .fill(if selected {
                            AUDIO_TONE.gamma_multiply(0.14)
                        } else {
                            PANEL_ALT
                        })
                        .inner_margin(8)
                        .show(ui, |ui| {
                            ui.set_width((ui.available_width() - 16.0).max(100.0));
                            ui.add(
                                egui::Label::new(RichText::new(&asset.name).color(if selected {
                                    AUDIO_TONE
                                } else {
                                    TEXT
                                }))
                                .truncate(),
                            );
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("{:.2} s", source_duration(asset)))
                                        .small()
                                        .color(DIM),
                                );
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width().max(20.0), 18.0),
                                    egui::Sense::hover(),
                                );
                                super::paint_waveform(
                                    ui.painter(),
                                    rect,
                                    &asset.waveform,
                                    AUDIO_TONE,
                                );
                            });
                        });
                    let response = ui.interact(
                        row.response.rect,
                        ui.id().with(("sample-asset", &asset.id)),
                        egui::Sense::click(),
                    );
                    if response.on_hover_text(&asset.name).clicked() && available && !selected {
                        self.stop_sampler_preview(editor);
                        let Ok(asset_id) = asset.id.parse::<AssetId>() else {
                            continue;
                        };
                        match self.vm.set_sampler_zone_asset(
                            editor.track_id,
                            editor.zone_id,
                            asset_id,
                        ) {
                            Ok(zone_id) => {
                                editor.zone_id = Some(zone_id);
                                editor.baseline = None;
                                editor.draft = None;
                                editor.error = None;
                            }
                            Err(error) => editor.error = Some(error),
                        }
                    }
                    ui.add_space(3.0);
                }
            });
    }

    fn sampler_sample_controls(
        &mut self,
        ui: &mut egui::Ui,
        editor: &mut SamplerEditor,
        track_index: usize,
        track: &Track,
        _now: f64,
    ) {
        // Picker changes can install a new zone during this frame.
        editor.sync(&self.vm.current_composition().tracks[track_index]);
        let Some(mut zone) = editor.draft.clone() else {
            ui.add_space(70.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("Choose a sound").size(19.0));
            });
            return;
        };
        let Some(asset) = self
            .vm
            .assets
            .iter()
            .find(|asset| asset.id == zone.asset_id)
            .cloned()
        else {
            return;
        };
        let duration = source_duration(&asset);
        if duration <= 0.0 {
            ui.label("Audio is not available yet.");
            return;
        }
        let min_length = (1.0 / f64::from(asset.sample_rate.max(1))).min(duration);
        let mut start = zone.source_start_seconds;
        let mut end = (start + zone.source_duration_seconds).min(duration);
        let status = self
            .controller
            .as_ref()
            .and_then(super::super::controller::NativeController::asset_preview_status);
        let matching_preview =
            editor.preview_path.is_some() && editor.preview_path == asset.media_path;
        let playing = matching_preview && status.as_ref().is_some_and(|status| status.playing);
        let loading = matching_preview && status.as_ref().is_some_and(|status| status.loading);
        let active = playing || editor.preview_pending.is_some();
        let available = asset.media_path.is_some() && self.controller.is_some();
        ui.add(egui::Label::new(RichText::new(&asset.name).strong()).truncate());
        let toggle = sampler_playback_controls(
            ui,
            available,
            active,
            loading,
            if matching_preview {
                status
                    .as_ref()
                    .map_or(start, |status| status.position_seconds)
            } else {
                start
            },
            duration,
        );
        if toggle {
            if active {
                self.stop_sampler_preview(editor);
            } else {
                self.preview_sampler_slice(editor, &asset, start, end);
            }
        }
        let preview = matching_preview
            .then(|| status.as_ref().map(|status| status.position_seconds))
            .flatten();
        let slice = waveform::slice_waveform(
            ui,
            ui.id().with((
                "sampler-waveform",
                &zone.id,
                &asset.id,
                editor.waveform_revision,
            )),
            waveform::SliceSource {
                waveform: &asset.waveform,
                duration,
                min_length,
                preview,
            },
            &mut start,
            &mut end,
        );
        if let Some(position) = slice.seek {
            let until = if position >= start && position < end {
                end
            } else {
                duration
            };
            self.preview_sampler_slice(
                editor,
                &asset,
                position.min(until - min_length).max(0.0),
                until,
            );
        }
        let mut changed = slice.finished;
        ui.horizontal(|ui| {
            let from = ui.add(
                egui::DragValue::new(&mut start)
                    .range(0.0..=(end - min_length).max(0.0))
                    .speed(0.01)
                    .fixed_decimals(3)
                    .prefix("Start ")
                    .suffix(" s"),
            );
            let to = ui.add(
                egui::DragValue::new(&mut end)
                    .range((start + min_length)..=duration)
                    .speed(0.01)
                    .fixed_decimals(3)
                    .prefix("End ")
                    .suffix(" s"),
            );
            changed |= from.drag_stopped()
                || to.drag_stopped()
                || (from.changed() && !from.dragged())
                || (to.changed() && !to.dragged());
            if ui
                .small_button("Full")
                .on_hover_text("Use the whole recording")
                .clicked()
            {
                start = 0.0;
                end = duration;
                changed = true;
            }
        });
        zone.source_start_seconds = start;
        zone.source_duration_seconds = end - start;
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label("Root").on_hover_text("The note that plays the sample at its original pitch. Other mapped notes are repitched.");
            egui::ComboBox::from_id_salt("sample-root")
                .selected_text(note_name(zone.root_note))
                .width(62.0)
                .show_ui(ui, |ui| {
                    for note in 0..=127 {
                        changed |= ui
                            .selectable_value(&mut zone.root_note, note, note_name(note))
                            .changed();
                    }
                });
            changed |= ui
                .selectable_value(&mut zone.one_shot, false, "Held")
                .on_hover_text("Release when the MIDI note ends")
                .changed();
            changed |= ui
                .selectable_value(&mut zone.one_shot, true, "One shot")
                .on_hover_text("Play the complete slice")
                .changed();
            changed |= ui.checkbox(&mut zone.reverse, "Reverse").changed();
            if (zone.low_note, zone.high_note) != (0, 127)
                && ui.small_button("All keys")
                    .on_hover_text("Repitch this sample across the whole keyboard")
                    .clicked()
            {
                zone.low_note = 0;
                zone.high_note = 127;
                changed = true;
            }
        });
        ui.collapsing("Advanced", |ui| {
            changed |= ui.text_edit_singleline(&mut zone.name).changed();
            ui.horizontal_wrapped(|ui| {
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut zone.low_note)
                            .range(0..=zone.high_note)
                            .prefix("Keys "),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut zone.high_note)
                            .range(zone.low_note..=127)
                            .prefix("to "),
                    )
                    .changed();
                if ui.small_button("All keys").clicked() {
                    zone.low_note = 0;
                    zone.high_note = 127;
                    changed = true;
                }
            });
            ui.horizontal_wrapped(|ui| {
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut zone.gain_db)
                            .range(-120.0..=24.0)
                            .speed(0.2)
                            .prefix("Gain ")
                            .suffix(" dB"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut zone.attack_ms)
                            .range(0.0..=60_000.0)
                            .prefix("Attack ")
                            .suffix(" ms"),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut zone.release_ms)
                            .range(0.0..=60_000.0)
                            .prefix("Release ")
                            .suffix(" ms"),
                    )
                    .changed();
            });
            ui.horizontal_wrapped(|ui| {
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut zone.low_velocity)
                            .range(0..=zone.high_velocity)
                            .prefix("Velocity "),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut zone.high_velocity)
                            .range(zone.low_velocity..=127)
                            .prefix("to "),
                    )
                    .changed();
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut zone.velocity_sensitivity)
                            .range(0.0..=1.0)
                            .speed(0.01)
                            .prefix("Sensitivity "),
                    )
                    .changed();
            });
            ui.horizontal(|ui| {
                let mut choke = zone.choke_group.is_some();
                if ui.checkbox(&mut choke, "Choke group").changed() {
                    zone.choke_group = choke.then_some(1);
                    changed = true;
                }
                if let Some(group) = &mut zone.choke_group {
                    changed |= ui.add(egui::DragValue::new(group)).changed();
                }
            });
            ui.separator();
            let mut polyphony = track.sampler_polyphony.unwrap_or(32);
            let mut voice = track
                .sampler_voice_stealing
                .clone()
                .unwrap_or_else(|| "oldest".into());
            let mut output = track.sampler_output_gain_db.unwrap_or(0.0);
            let mut settings_changed = false;
            ui.horizontal_wrapped(|ui| {
                settings_changed |= ui
                    .add(
                        egui::DragValue::new(&mut polyphony)
                            .range(1..=1024)
                            .prefix("Voices "),
                    )
                    .changed();
                settings_changed |= ui
                    .add(
                        egui::DragValue::new(&mut output)
                            .range(-120.0..=24.0)
                            .speed(0.2)
                            .prefix("Output ")
                            .suffix(" dB"),
                    )
                    .changed();
                egui::ComboBox::from_id_salt("sample-voice-stealing")
                    .selected_text(&voice)
                    .show_ui(ui, |ui| {
                        for choice in ["oldest", "quietest", "lowest_velocity"] {
                            settings_changed |= ui
                                .selectable_value(&mut voice, choice.into(), choice)
                                .changed();
                        }
                    })
                    .response
                    .on_hover_text("Voice to replace when all voices are playing");
            });
            if settings_changed {
                self.vm
                    .update_sampler_settings(track_index, polyphony, &voice, output);
            }
            if ui.small_button("Remove layer").clicked()
                && let Some(index) = self.vm.current_composition().tracks[track_index]
                    .sampler_zones
                    .iter()
                    .position(|current| current.id == zone.id)
            {
                self.vm.remove_sampler_zone(track_index, index);
                editor.select_zone(
                    self.vm.current_composition().tracks[track_index]
                        .sampler_zones
                        .first(),
                );
                self.stop_sampler_preview(editor);
            }
        });
        if editor.zone_id.is_some_and(|id| id.to_string() == zone.id) {
            editor.draft = Some(zone.clone());
            if changed
                && let Some(index) = self.vm.current_composition().tracks[track_index]
                    .sampler_zones
                    .iter()
                    .position(|current| current.id == zone.id)
            {
                self.vm.update_sampler_zone(track_index, index, &zone);
                editor.error = self.vm.last_error().map(str::to_owned);
                // A local trim/property edit keeps the current waveform zoom.
                let saved =
                    self.vm.current_composition().tracks[track_index].sampler_zones[index].clone();
                editor.draft = Some(saved.clone());
                editor.baseline = Some(saved);
            }
        }
    }

    fn preview_sampler_slice(
        &mut self,
        editor: &mut SamplerEditor,
        asset: &Asset,
        start: f64,
        end: f64,
    ) {
        let Some(path) = &asset.media_path else {
            return;
        };
        let Some(controller) = &mut self.controller else {
            return;
        };
        editor.error = None;
        if editor.preview_path.as_ref() != Some(path)
            || controller
                .asset_preview_status()
                .is_none_or(|status| status.error.is_some())
        {
            controller.begin_asset_preview(path);
            editor.preview_path = Some(path.clone());
        }
        let status = controller.asset_preview_status();
        if let Some(error) = status.as_ref().and_then(|status| status.error.clone()) {
            editor.preview_pending = None;
            editor.error = Some(error);
        } else if status.is_some_and(|status| !status.loading) {
            editor.preview_pending = None;
            controller.play_asset_preview_range(start, end);
        } else {
            editor.preview_pending = Some((start, end));
        }
    }

    fn stop_sampler_preview(&mut self, editor: &mut SamplerEditor) {
        editor.preview_pending = None;
        if let Some(controller) = &mut self.controller {
            controller.stop_asset_preview();
        }
    }

    fn end_sampler_preview(&mut self, editor: &SamplerEditor) {
        if editor.preview_path.is_some()
            && let Some(controller) = &mut self.controller
        {
            controller.end_asset_preview(&self.vm);
        }
    }
}
