use super::{
    AUDIO_TONE, AssetActivity, AssetDialog, ClipKind, DIM, EVENT_TONE, GawApp, HIGHLIGHT, Intent,
    NESTED_TONE, RenderState, RichText, Selection, collapsible_column_title, connector, egui,
    loading_activity, processor_chooser, property, reset_panel_size, signal_node,
};

impl GawApp {
    pub(super) fn inspector(&mut self, ui: &mut egui::Ui) {
        if collapsible_column_title(
            ui,
            "SIGNAL",
            "top → bottom",
            "collapse_signal",
            "Collapse Signal",
        )
        .clicked()
        {
            reset_panel_size(ui.ctx(), "signal_collapsed");
            self.signal_expanded = false;
            return;
        }
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
        let asset_id = asset.id.parse().ok();
        let (transcribing, progress) = asset_id.map_or((false, None), |asset_id| {
            self.controller
                .as_ref()
                .map_or((false, None), |controller| {
                    (
                        controller.is_transcribing(asset_id),
                        controller.stem_split_progress(asset_id),
                    )
                })
        });
        if let Some(activity) = AssetActivity::current(transcribing, progress) {
            loading_activity(ui, &activity.label());
        }
        if let Some((asset_id, (_, _, cancelling, _))) = asset_id.zip(progress) {
            if ui
                .add_enabled(
                    !cancelling,
                    egui::Button::new(if cancelling {
                        "CANCELLING…"
                    } else {
                        "CANCEL STEM SPLIT"
                    }),
                )
                .clicked()
                && let Some(controller) = &mut self.controller
            {
                controller.cancel_stem_split(asset_id);
            }
        } else if ui
            .add_enabled(
                self.controller.is_some() && asset.media_path.is_some(),
                egui::Button::new("STEM SPLITTER…"),
            )
            .clicked()
        {
            self.asset_dialog = Some(AssetDialog::StemSplitter {
                asset_ids: vec![asset.id.clone()],
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
        signal_node(ui, 1, "MIDI ASSET", &asset.name, EVENT_TONE, true);
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
        signal_node(ui, 3, "TRACK OUTPUT", "stereo", EVENT_TONE, true);
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
                    AUDIO_TONE,
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
                        if selected { HIGHLIGHT } else { source_color },
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
            source_color,
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
                        source_color,
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
}
