use super::{
    ClipKind, DIM, GawApp, HIGHLIGHT, Intent, RichText, Selection, TEXT, collapsible_column_title,
    connector, egui, processor_chooser, reset_panel_size, signal_node,
};
use gaw_core::ProcessorStack;

impl GawApp {
    pub(super) fn inspector(&mut self, ui: &mut egui::Ui) {
        if collapsible_column_title(ui, "SIGNAL", "", "collapse_signal", "Collapse Signal")
            .clicked()
        {
            reset_panel_size(ui.ctx(), "signal_collapsed");
            self.signal_expanded = false;
            return;
        }
        self.signal_scope_switch(ui);
        egui::ScrollArea::vertical()
            .id_salt("signal_contents")
            .auto_shrink([false, false])
            .show(ui, |ui| self.signal_stack_inspector(ui));
    }

    fn signal_scope_switch(&mut self, ui: &mut egui::Ui) {
        let (track_index, clip_index) = match self.vm.selection {
            Selection::Clip { track, clip } | Selection::Effect { track, clip, .. } => {
                (Some(track), Some(clip))
            }
            Selection::Track { track } | Selection::Sampler { track } => (Some(track), None),
            _ => (None, None),
        };
        let clip = track_index
            .zip(clip_index)
            .and_then(|(track, clip)| self.vm.clip_stack(track, clip));
        let track = track_index
            .and_then(|index| self.vm.current_composition().tracks.get(index))
            .and_then(|track| track.id.parse().ok())
            .map(|track_id| ProcessorStack::Track { track_id });
        let output = self
            .vm
            .current_composition()
            .id
            .parse()
            .ok()
            .map(|composition_id| ProcessorStack::CompositionOutput { composition_id });
        let root = self.vm.project().root_composition_id;
        let output_label = if matches!(output, Some(ProcessorStack::CompositionOutput { composition_id }) if composition_id == root)
        {
            "MASTER"
        } else {
            "OUTPUT"
        };
        let selected = self.vm.signal_stack();
        ui.horizontal(|ui| {
            for (label, stack) in [("CLIP", clip), ("TRACK", track), (output_label, output)] {
                let response = ui.add_enabled(
                    stack.is_some(),
                    egui::Button::new(label).selected(stack.is_some() && stack == selected),
                );
                if response.clicked()
                    && let Some(stack) = stack
                {
                    self.vm.set_signal_stack(stack);
                }
            }
        });
        if output_label == "OUTPUT"
            && matches!(selected, Some(ProcessorStack::CompositionOutput { composition_id }) if composition_id == root)
        {
            ui.label(
                RichText::new("EDITING MASTER")
                    .monospace()
                    .size(10.0)
                    .color(TEXT),
            );
        }
        ui.add_space(10.0);
    }

    fn signal_stack_inspector(&mut self, ui: &mut egui::Ui) {
        let Some(stack) = self.vm.signal_stack() else {
            return;
        };
        let scope = self.vm.signal_scope_label(&stack);
        let (scope_kind, owner) = scope.split_once(" · ").unwrap_or(("SIGNAL", &scope));
        ui.label(RichText::new(owner).strong());
        ui.label(
            RichText::new(match &stack {
                ProcessorStack::Clip { .. } => "This clip only",
                ProcessorStack::CompositionClip { .. } => "This placement only",
                ProcessorStack::Track { .. } => "All clips on this track",
                ProcessorStack::CompositionOutput { composition_id }
                    if *composition_id == self.vm.project().root_composition_id =>
                {
                    "Whole song output"
                }
                ProcessorStack::CompositionOutput { .. } => "This composition's output",
            })
            .size(10.0)
            .color(DIM),
        );
        if let Selection::Clip { track, clip } | Selection::Effect { track, clip, .. } =
            self.vm.selection
            && matches!(stack, ProcessorStack::Clip { .. })
            && self
                .vm
                .current_composition()
                .tracks
                .get(track)
                .and_then(|track| track.clips.get(clip))
                .is_some_and(|clip| matches!(clip.kind, ClipKind::Event { .. }))
            && ui
                .small_button("Slice Sampler")
                .on_hover_text("Open sampler zones")
                .clicked()
        {
            self.vm.apply(Intent::Select(Selection::Sampler { track }));
        }
        ui.add_space(8.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!("{scope_kind} EFFECTS"))
                    .monospace()
                    .size(9.0)
                    .color(DIM),
            );
            if ui
                .small_button("EQ")
                .on_hover_text("Open EQ, or add a flat EQ if this scope has none")
                .clicked()
            {
                self.vm.open_equalizer(stack.clone());
            }
            processor_chooser(ui, &mut self.vm, &stack, ("signal", format!("{stack:?}")));
        });
        let effects = self.vm.effects_at(&stack);
        let selected_id = self.vm.selected_processor_view().map(|effect| effect.id);
        for (index, effect) in effects.iter().enumerate() {
            let selected = selected_id.as_deref() == Some(&effect.id);
            let response = ui
                .push_id(&effect.id, |ui| {
                    signal_node(
                        ui,
                        index + 1,
                        &effect.name,
                        if selected { HIGHLIGHT } else { TEXT },
                        effect.enabled,
                    )
                })
                .inner
                .on_hover_text(if effect.kind == "gaw.parametric_eq" {
                    "Open the floating EQ panel"
                } else {
                    "Open effect controls"
                });
            if response.clicked() {
                self.vm.select_processor_at(stack.clone(), index);
            }
            if effect.kind == "gaw.parametric_eq"
                && let Some(bands) = effect
                    .parameters
                    .iter()
                    .find(|parameter| parameter.id == "bands")
                    .and_then(|parameter| {
                        serde_json::from_value::<Vec<gaw_core::EqBand>>(parameter.value.clone())
                            .ok()
                    })
            {
                let gain = effect
                    .parameters
                    .iter()
                    .find(|p| p.id == "output_gain_db")
                    .and_then(|p| p.value.as_f64())
                    .unwrap_or(0.0) as f32;
                if super::equalizer::eq_thumbnail(
                    ui,
                    &bands,
                    gain,
                    effect.enabled,
                    self.vm.project().sample_rate.value(),
                )
                .clicked()
                {
                    self.vm.select_processor_at(stack.clone(), index);
                }
            }
            ui.horizontal(|ui| {
                if ui
                    .small_button(if effect.enabled { "ON" } else { "OFF" })
                    .clicked()
                {
                    self.vm.toggle_processor_at(stack.clone(), index);
                }
                if ui
                    .add_enabled(index > 0, egui::Button::new("↑").small())
                    .clicked()
                {
                    self.vm.move_processor_at(stack.clone(), index, -1);
                }
                if ui
                    .add_enabled(index + 1 < effects.len(), egui::Button::new("↓").small())
                    .clicked()
                {
                    self.vm.move_processor_at(stack.clone(), index, 1);
                }
                if ui
                    .small_button("×")
                    .on_hover_text("Remove effect")
                    .clicked()
                {
                    self.vm.remove_processor_at(stack.clone(), index);
                }
            });
            if index + 1 < effects.len() {
                connector(ui);
            }
        }
        if effects.is_empty() {
            ui.add_space(8.0);
            ui.label(RichText::new("No effects").size(10.0).color(DIM));
        }
    }
}
