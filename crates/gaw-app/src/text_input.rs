pub(crate) fn focus_and_select_all(ui: &egui::Ui, response: &egui::Response, char_count: usize) {
    response.request_focus();
    let mut state = egui::TextEdit::load_state(ui.ctx(), response.id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::default(),
            egui::text::CCursor::new(char_count),
        )));
    state.store(ui.ctx(), response.id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focuses_and_selects_the_complete_text() {
        egui::__run_test_ui(|ui| {
            let mut value = "Héllo".to_owned();
            let char_count = value.chars().count();
            let response = ui.text_edit_singleline(&mut value);

            focus_and_select_all(ui, &response, char_count);

            assert!(response.has_focus());
            let state = egui::TextEdit::load_state(ui.ctx(), response.id).unwrap();
            let range = state.cursor.char_range().unwrap();
            assert_eq!(range.secondary.index, 0);
            assert_eq!(range.primary.index, char_count);
        });
    }
}
