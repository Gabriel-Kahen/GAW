//! The monitoring chain belongs to audio preferences, independently of project effects.
use super::{
    BORDER, DIM, GawApp, PANEL_ALT, RichText, STATUS_ERROR, STATUS_NOTICE, Stroke, Vec2, egui,
    parameter_widget,
};
use crate::model::{effect_view, set_parameter};
use gaw_core::{Processor, ProcessorId, ProcessorKind};

const MAX_EFFECTS: usize = gaw_audio::monitor::MAX_INPUT_EFFECTS;

impl GawApp {
    pub(super) fn live_input_effects_window(&mut self, ctx: &egui::Context) {
        if !self.live_input_effects_open {
            return;
        }
        let monitor = self
            .controller
            .as_ref()
            .map(crate::controller::NativeController::input_monitor_status);
        let latency = self.controller.as_ref().and_then(|controller| {
            controller
                .audio_status()
                .map(|output| (controller.input_latency_status(), output))
        });
        let previous_buffer = self.audio_preferences.buffer_frames;
        let (applying, error) = self
            .controller
            .as_ref()
            .map_or((false, None), |controller| {
                controller.input_effects_status()
            });
        egui::Window::new("LIVE INPUT EFFECTS")
            .id(egui::Id::new("live_input_effects_window"))
            .open(&mut self.live_input_effects_open)
            .collapsible(false)
            .default_size(Vec2::new(480.0, 520.0))
            .min_width(360.0)
            .frame(
                egui::Frame::window(&ctx.global_style())
                    .fill(PANEL_ALT)
                    .stroke(Stroke::new(1.0, BORDER)),
            )
            .show(ctx, |ui| {
                ui.label(RichText::new("INPUT → EFFECTS → HEADPHONES").monospace().size(11.0).color(DIM));
                if monitor.as_ref().is_none_or(|status| !status.enabled) {
                    ui.label(RichText::new("Monitor is off. Turn it on to hear this chain.").color(STATUS_NOTICE));
                } else if monitor.as_ref().is_some_and(|status| status.opening) {
                    ui.label(RichText::new("Opening input…").color(STATUS_NOTICE));
                }
                if applying {
                    ui.label(RichText::new("Applying effects…").color(STATUS_NOTICE));
                }
                if let Some(error) = error.as_ref().or(self.live_input_effects_error.as_ref()) {
                    ui.label(RichText::new(error).color(STATUS_ERROR));
                }
                latency_controls(ui, latency.as_ref(), &mut self.audio_preferences.buffer_frames);
                if self.audio_preferences.monitor_effects.iter().any(has_slower_live_settings)
                    && ui.button("REDUCE FX LATENCY")
                        .on_hover_text("Use Draft pitch shifting and zero compressor/limiter lookahead. This changes those effect settings; spectral pitch quality and predictive dynamics are traded for a faster response.")
                        .clicked()
                {
                    for effect in &mut self.audio_preferences.monitor_effects {
                        reduce_effect_latency(effect);
                    }
                }
                chain_contents(
                    ui,
                    &mut self.audio_preferences.monitor_effects,
                    &mut self.audio_preferences.monitor_effects_bypassed,
                    &mut self.live_input_effects_error,
                );
                ui.separator();
                ui.label(RichText::new("On your Scarlett, turn Direct Monitor OFF to hear only the software effects.").size(11.0).color(DIM));
            });
        if previous_buffer != self.audio_preferences.buffer_frames
            && let Some(controller) = &mut self.controller
        {
            controller.configure_audio(
                self.vm.project().sample_rate.value(),
                self.audio_preferences
                    .output_device
                    .as_ref()
                    .and_then(|device| device.id.parse().ok()),
                self.audio_preferences.buffer_frames,
            );
        }
    }
}

fn latency_controls(
    ui: &mut egui::Ui,
    status: Option<&(
        gaw_audio::monitor::InputMonitorLatencyStatus,
        crate::controller::ActiveAudioStatus,
    )>,
    buffer: &mut Option<u32>,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("BUFFER").monospace().size(10.0).color(DIM));
        for (frames, label) in [(None, "AUTO"), (Some(64), "64 · FAST"), (Some(128), "128"), (Some(256), "256")] {
            ui.selectable_value(buffer, frames, label)
                .on_hover_text("Smaller buffers reduce delay. Choose 128 or 256 if you hear crackles. Auto starts at 64 and falls back if the device cannot open it.");
        }
    });
    if let Some((input, output)) = status {
        let milliseconds = |frames: u64| frames as f64 * 1000.0 / f64::from(output.sample_rate);
        let capture = if input.input_callback_frames > 0 {
            format!("{:.2}", milliseconds(input.input_callback_frames as u64))
        } else {
            "—".into()
        };
        let playback = output.observed_buffer_frames.map_or_else(
            || "—".into(),
            |frames| format!("{:.2}", milliseconds(u64::from(frames))),
        );
        ui.label(RichText::new(format!(
            "Callbacks: in {capture} / out {playback} ms · Queue {:.2} ms · FX {:.2} ms",
            milliseconds(input.queued_frames as u64), milliseconds(input.effect_latency_frames),
        )).size(10.0).color(DIM))
            .on_hover_text("Observed callback sizes, current queued audio, and the active effects' declared delay. These are separate contributors, not a measured hardware round-trip latency.");
        if input.dropped_frames > 0 || input.underrun_frames > 0 {
            ui.label(RichText::new(format!("Queue recovery: {} skipped / {} missing frames", input.dropped_frames, input.underrun_frames)).size(10.0).color(DIM))
                .on_hover_text("Counters since monitoring started. Brief changes can occur on startup; continuing changes while playing suggest trying a larger buffer.");
        }
    }
}

fn has_slower_live_settings(effect: &Processor) -> bool {
    if !effect.enabled {
        return false;
    }
    match &effect.kind {
        ProcessorKind::PitchShift(parameters) => {
            parameters.quality == gaw_core::PitchQuality::Signalsmith
                && parameters.mix > 0.0
                && i32::from(parameters.semitones) * 100 + i32::from(parameters.cents) != 0
        }
        ProcessorKind::Compressor(parameters) => parameters.lookahead_ms > 0.0,
        ProcessorKind::Limiter(parameters) => parameters.lookahead_ms > 0.0,
        _ => false,
    }
}

fn reduce_effect_latency(effect: &mut Processor) {
    if !effect.enabled {
        return;
    }
    match &mut effect.kind {
        ProcessorKind::PitchShift(parameters) => parameters.quality = gaw_core::PitchQuality::Draft,
        ProcessorKind::Compressor(parameters) => parameters.lookahead_ms = 0.0,
        ProcessorKind::Limiter(parameters) => parameters.lookahead_ms = 0.0,
        _ => {}
    }
}

fn new_effect(kind: ProcessorKind) -> Processor {
    let id = ProcessorId::new(format!("live-fx-{}", gaw_core::ClipId::new()))
        .expect("UUID-backed processor id is valid");
    Processor::new(id, kind)
}

fn set_live_parameter(
    processor: &mut Processor,
    parameter: &str,
    value: serde_json::Value,
) -> Result<(), String> {
    let mut updated = processor.clone();
    if !set_parameter(&mut updated, parameter, value) {
        return Err("That parameter value could not be applied.".into());
    }
    updated.validate().map_err(|error| error.to_string())?;
    *processor = updated;
    Ok(())
}

#[derive(Clone, Copy)]
enum ChainAction {
    Move(usize, usize),
    Remove(usize),
}

fn apply_chain_action(effects: &mut Vec<Processor>, action: ChainAction) {
    match action {
        ChainAction::Move(from, to) if from < effects.len() && to < effects.len() => {
            let effect = effects.remove(from);
            effects.insert(to, effect);
        }
        ChainAction::Remove(index) if index < effects.len() => {
            effects.remove(index);
        }
        _ => {}
    }
}

fn chain_contents(
    ui: &mut egui::Ui,
    effects: &mut Vec<Processor>,
    bypassed: &mut bool,
    error: &mut Option<String>,
) {
    ui.horizontal(|ui| {
        ui.checkbox(bypassed, "BYPASS CHAIN")
            .on_hover_text("Hear the dry input without changing your effects");
        ui.add_enabled_ui(effects.len() < MAX_EFFECTS, |ui| {
            ui.menu_button("+ ADD EFFECT", |ui| {
                egui::ScrollArea::vertical()
                    .max_height(360.0)
                    .show(ui, |ui| {
                        for kind in ProcessorKind::catalog_defaults()
                            .into_iter()
                            .filter(|kind| !kind.is_analyzer())
                        {
                            let effect = new_effect(kind);
                            if ui.button(effect_view(&effect).name).clicked() {
                                effects.push(effect);
                                *error = None;
                                ui.close();
                            }
                        }
                    });
            });
        });
        ui.label(
            RichText::new(format!("{}/{MAX_EFFECTS}", effects.len()))
                .size(11.0)
                .color(DIM),
        );
    });
    ui.separator();
    if effects.is_empty() {
        ui.add_space(12.0);
        ui.label("Your live input is dry.");
        ui.label(
            RichText::new("Add an effect to start. Effects run from top to bottom.").color(DIM),
        );
        ui.add_space(12.0);
        return;
    }
    let mut action = None;
    let count = effects.len();
    egui::ScrollArea::vertical()
        .id_salt("live_effect_chain")
        .max_height(600.0)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            for (index, effect) in effects.iter_mut().enumerate() {
                let view = effect_view(effect);
                ui.push_id(&view.id, |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut effect.enabled, "")
                            .on_hover_text("Enable this effect");
                        ui.label(format!("{}. {}", index + 1, view.name));
                        if ui
                            .add_enabled(index > 0, egui::Button::new("↑").small())
                            .on_hover_text("Move earlier")
                            .clicked()
                        {
                            action = Some(ChainAction::Move(index, index - 1));
                        }
                        if ui
                            .add_enabled(index + 1 < count, egui::Button::new("↓").small())
                            .on_hover_text("Move later")
                            .clicked()
                        {
                            action = Some(ChainAction::Move(index, index + 1));
                        }
                        if ui.small_button("REMOVE").clicked() {
                            action = Some(ChainAction::Remove(index));
                        }
                    });
                    egui::CollapsingHeader::new("PARAMETERS")
                        .default_open(true)
                        .show(ui, |ui| {
                            for parameter in &view.parameters {
                                ui.push_id(&parameter.id, |ui| {
                                    ui.label(
                                        RichText::new(parameter.label.to_uppercase())
                                            .monospace()
                                            .size(10.0)
                                            .color(DIM),
                                    );
                                    if let Some(value) = parameter_widget(ui, parameter) {
                                        *error =
                                            set_live_parameter(effect, &parameter.id, value).err();
                                    }
                                });
                            }
                        });
                    ui.separator();
                });
            }
        });
    if let Some(action) = action {
        apply_chain_action(effects, action);
        *error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reducing_effect_latency_preserves_sound_controls_and_disabled_effects() {
        let mut pitch = new_effect(ProcessorKind::PitchShift(gaw_core::PitchShiftParameters {
            semitones: -12,
            quality: gaw_core::PitchQuality::Signalsmith,
            mix: 0.7,
            ..Default::default()
        }));
        assert!(has_slower_live_settings(&pitch));
        reduce_effect_latency(&mut pitch);
        let ProcessorKind::PitchShift(parameters) = &pitch.kind else {
            unreachable!()
        };
        assert_eq!(parameters.quality, gaw_core::PitchQuality::Draft);
        assert_eq!(parameters.semitones, -12);
        assert!((parameters.mix - 0.7).abs() < f32::EPSILON);
        assert!(!has_slower_live_settings(&pitch));
        pitch.validate().unwrap();
        let mut limiter = new_effect(ProcessorKind::Limiter(
            gaw_core::LimiterParameters::default(),
        ));
        assert!(has_slower_live_settings(&limiter));
        limiter.enabled = false;
        let original = limiter.clone();
        reduce_effect_latency(&mut limiter);
        assert_eq!(limiter, original);
        limiter.enabled = true;
        reduce_effect_latency(&mut limiter);
        assert!(!has_slower_live_settings(&limiter));
        limiter.validate().unwrap();
        let mut compressor =
            new_effect(ProcessorKind::Compressor(gaw_core::CompressorParameters {
                lookahead_ms: 10.0,
                threshold_db: -18.0,
                ratio: 6.0,
                ..Default::default()
            }));
        assert!(has_slower_live_settings(&compressor));
        reduce_effect_latency(&mut compressor);
        assert!(!has_slower_live_settings(&compressor));
        let ProcessorKind::Compressor(parameters) = &compressor.kind else {
            unreachable!()
        };
        assert!((parameters.threshold_db + 18.0).abs() < f32::EPSILON);
        assert!((parameters.ratio - 6.0).abs() < f32::EPSILON);
        compressor.validate().unwrap();
    }

    #[test]
    fn reorder_and_remove_preserve_effect_identity_and_settings() {
        let first = new_effect(ProcessorKind::Gain(gaw_core::GainParameters::default()));
        let mut second = new_effect(ProcessorKind::Delay(gaw_core::DelayParameters::default()));
        second.enabled = false;
        let mut chain = vec![first.clone(), second.clone()];
        apply_chain_action(&mut chain, ChainAction::Move(1, 0));
        assert_eq!(chain, vec![second.clone(), first]);
        apply_chain_action(&mut chain, ChainAction::Remove(1));
        assert_eq!(chain, vec![second]);
    }

    #[test]
    fn invalid_live_parameter_keeps_the_previous_valid_effect() {
        let mut effect = new_effect(ProcessorKind::Gain(gaw_core::GainParameters::default()));
        assert!(set_live_parameter(&mut effect, "gain_db", serde_json::json!(-12.0)).is_ok());
        let previous = effect.clone();
        assert!(set_live_parameter(&mut effect, "gain_db", serde_json::json!(1000.0)).is_err());
        assert_eq!(effect, previous);
        assert!(set_live_parameter(&mut effect, "missing", serde_json::json!(0.0)).is_err());
    }

    #[test]
    fn empty_chain_explains_signal_flow_and_has_add_and_bypass_controls() {
        let ctx = egui::Context::default();
        let mut effects = Vec::new();
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(480.0, 300.0),
                )),
                ..Default::default()
            },
            |ui| chain_contents(ui, &mut effects, &mut false, &mut None),
        );
        let labels: Vec<_> = output
            .shapes
            .iter()
            .filter_map(|shape| {
                if let egui::Shape::Text(text) = &shape.shape {
                    assert!(
                        shape
                            .clip_rect
                            .contains_rect(text.galley.rect.translate(text.pos.to_vec2())),
                        "clipped label: {}",
                        text.galley.text()
                    );
                    Some(text.galley.text())
                } else {
                    None
                }
            })
            .collect();
        assert!(labels.contains(&"+ ADD EFFECT"));
        assert!(labels.contains(&"BYPASS CHAIN"));
        assert!(labels.contains(&"Your live input is dry."));
    }
}
