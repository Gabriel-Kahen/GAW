fn main() -> eframe::Result {
    eframe::run_native(
        "GAW",
        eframe::NativeOptions::default(),
        Box::new(|_context| Ok(Box::<GawApp>::default())),
    )
}

#[derive(Debug, Default)]
struct GawApp;

impl eframe::App for GawApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show_inside(ui, |ui| {
            ui.heading("GAW");
            ui.label("Gabe's Audio Workstation");
        });
    }
}
