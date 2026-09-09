use super::{
    AUDIO_TONE, Align2, Arc, BORDER, CANVAS, ClipKind, CornerRadius, DIM, EVENT_TONE, EditorKind,
    FontId, GawApp, NESTED_TONE, Pos2, RichText, Selection, Sense, Stroke, TEXT, Vec2, egui,
    metric, paint_waveform, panel_title, parameter_widget,
};

impl GawApp {
    pub(super) fn context_editor(&mut self, ui: &mut egui::Ui) {
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
            metric(ui, "SAMPLE RATE", "48 kHz", TEXT);
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

    pub(super) fn piano_roll_editor(&mut self, ui: &mut egui::Ui) {
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
        let playhead = self.vm.transport.playhead;
        let beats_per_bar = self.vm.transport.time_signature.quarter_notes_per_bar() as f32;
        let actions = crate::piano_roll::show(
            ui,
            &mut self.piano_roll,
            track_index,
            clip_index,
            &clip,
            notes,
            playhead,
            beats_per_bar,
            &mut self.new_note_velocity,
        );
        for action in actions {
            self.vm.apply(action);
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
                                    .color(DIM),
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
                                                EVENT_TONE
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
                                        .color(DIM),
                                    );
                                }
                            });
                        ui.add_space(4.0);
                    });
                }
            });
    }
}
