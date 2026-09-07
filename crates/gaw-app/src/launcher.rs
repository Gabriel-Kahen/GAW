#![allow(clippy::too_many_lines)]

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
    thread::JoinHandle,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use egui::{Align, Color32, FontFamily, FontId, Layout, RichText, Sense, Stroke, Vec2};
use gaw_project::ProjectStore;

use crate::{
    GawApp, NativeStartup, RecoveryPolicy,
    project_library::{ProjectEntry, ProjectLibrary, ProjectStatus},
    settings::{AudioPreferences, SAMPLE_RATES},
    theme::{BORDER, CANVAS, DIM, HIGHLIGHT, PANEL, PANEL_ALT, PANEL_RAISED, STATUS_ERROR, TEXT},
};

#[derive(Debug)]
pub struct GawDesktop {
    mode: DesktopMode,
    library: ProjectLibrary,
    entries: Vec<ProjectEntry>,
    scan: Option<Receiver<(Vec<ProjectEntry>, Vec<String>)>>,
    rescan_requested: bool,
    audio_preferences: AudioPreferences,
    selected: Option<PathBuf>,
    search: String,
    notice: Option<String>,
    error: Option<String>,
    create: Option<NewProjectDraft>,
    manage_locations: bool,
}

#[derive(Debug)]
enum DesktopMode {
    Library,
    Pending(PendingProject),
    Editor(Box<GawApp>),
}

#[derive(Debug)]
struct PendingProject {
    path: PathBuf,
    label: &'static str,
    receiver: Receiver<Result<NativeStartup, String>>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct NewProjectDraft {
    name: String,
    parent: PathBuf,
    folder: String,
    automatic_folder: String,
    bpm: String,
    sample_rate: u32,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct ProjectTableWidths {
    name: f32,
    location: f32,
    tempo: f32,
    rate: f32,
    last_opened: f32,
    status: f32,
}

impl ProjectTableWidths {
    fn new(width: f32) -> Self {
        let tempo = 65.0;
        let rate = 75.0;
        let last_opened = 95.0;
        let status = 78.0;
        let flexible = (width - tempo - rate - last_opened - status - 35.0).max(300.0);
        let name = (flexible * 0.36).clamp(150.0, 250.0);
        Self {
            name,
            location: (flexible - name).max(150.0),
            tempo,
            rate,
            last_opened,
            status,
        }
    }
}

impl NewProjectDraft {
    fn new(parent: PathBuf) -> Self {
        Self {
            name: String::new(),
            parent,
            folder: String::new(),
            automatic_folder: String::new(),
            bpm: "120".into(),
            sample_rate: 48_000,
            error: None,
        }
    }

    fn sync_folder(&mut self) {
        let generated = project_folder_name(&self.name);
        if self.folder.is_empty() || self.folder == self.automatic_folder {
            self.folder.clone_from(&generated);
        }
        self.automatic_folder = generated;
    }

    fn validate(&self) -> Result<(PathBuf, String, f64, u32), String> {
        let name = self.name.trim();
        if name.is_empty() {
            return Err("Enter a project name".into());
        }
        let folder = self.folder.trim();
        if folder.is_empty()
            || matches!(folder, "." | "..")
            || folder.contains(['/', '\\'])
            || folder.chars().any(char::is_control)
        {
            return Err("Enter a valid folder name without slashes".into());
        }
        let bpm = self
            .bpm
            .trim()
            .parse::<f64>()
            .map_err(|_| "Tempo must be a number".to_owned())?;
        if !bpm.is_finite() || bpm <= 0.0 {
            return Err("Tempo must be greater than zero".into());
        }
        let destination = self.parent.join(folder);
        if destination.exists() {
            return Err(format!("{} already exists", destination.display()));
        }
        Ok((destination, name.to_owned(), bpm, self.sample_rate))
    }
}

impl GawDesktop {
    pub fn new(context: &eframe::CreationContext<'_>) -> Self {
        crate::app::configure_style(&context.egui_ctx);
        let library = ProjectLibrary::load_default();
        let mut desktop = Self {
            mode: DesktopMode::Library,
            entries: Vec::new(),
            scan: None,
            rescan_requested: false,
            audio_preferences: AudioPreferences::load(context.storage),
            selected: None,
            search: String::new(),
            notice: None,
            error: library.warning.clone(),
            create: None,
            manage_locations: false,
            library,
        };
        desktop.start_scan(&context.egui_ctx);
        desktop
    }

    /// Starts in the editor while still recording the project in the launcher catalog.
    ///
    /// # Errors
    /// Returns a domain error if the startup project cannot populate the editor.
    pub fn with_startup(
        context: &eframe::CreationContext<'_>,
        startup: NativeStartup,
        path: &Path,
    ) -> Result<Self, gaw_core::DomainError> {
        let mut library = ProjectLibrary::load_default();
        let warning = library
            .remember(path, true)
            .err()
            .or(library.warning.clone());
        let audio_preferences = AudioPreferences::load(context.storage);
        let editor =
            GawApp::with_native_runtime(&context.egui_ctx, audio_preferences.clone(), startup)?;
        Ok(Self {
            mode: DesktopMode::Editor(Box::new(editor)),
            library,
            entries: Vec::new(),
            scan: None,
            rescan_requested: false,
            audio_preferences,
            selected: None,
            search: String::new(),
            notice: None,
            error: warning,
            create: None,
            manage_locations: false,
        })
    }

    fn start_scan(&mut self, context: &egui::Context) {
        if self.scan.is_some() {
            self.rescan_requested = true;
            return;
        }
        let library = self.library.clone();
        let repaint = context.clone();
        let (sender, receiver) = mpsc::channel();
        self.scan = Some(receiver);
        let _ = thread::Builder::new()
            .name("gaw-project-scan".into())
            .spawn(move || {
                let result = library.scan();
                let _ = sender.send(result);
                repaint.request_repaint();
            });
    }

    fn poll_scan(&mut self, context: &egui::Context) {
        let Some(receiver) = &self.scan else {
            return;
        };
        match receiver.try_recv() {
            Ok((entries, warnings)) => {
                self.entries = entries;
                self.scan = None;
                if !warnings.is_empty() {
                    self.error = Some(warnings.join("\n"));
                }
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|path| !self.entries.iter().any(|entry| &entry.path == path))
                {
                    self.selected = None;
                }
            }
            Err(TryRecvError::Disconnected) => {
                self.scan = None;
                self.error = Some("Project scan stopped unexpectedly".into());
            }
            Err(TryRecvError::Empty) => {}
        }
        if self.scan.is_none() && self.rescan_requested {
            self.rescan_requested = false;
            self.start_scan(context);
        }
    }

    fn poll_pending(&mut self, context: &egui::Context) {
        let result = match &self.mode {
            DesktopMode::Pending(pending) => match pending.receiver.try_recv() {
                Ok(result) => Some((pending.path.clone(), result)),
                Err(TryRecvError::Disconnected) => Some((
                    pending.path.clone(),
                    Err("Project operation stopped unexpectedly".into()),
                )),
                Err(TryRecvError::Empty) => None,
            },
            DesktopMode::Library | DesktopMode::Editor(_) => None,
        };
        let Some((path, result)) = result else {
            return;
        };
        if let DesktopMode::Pending(mut pending) =
            std::mem::replace(&mut self.mode, DesktopMode::Library)
            && let Some(worker) = pending.worker.take()
        {
            let _ = worker.join();
        }
        match result {
            Ok(startup) => {
                if let Err(error) = self.library.remember(&path, true) {
                    self.notice = Some(error);
                }
                match GawApp::with_native_runtime(context, self.audio_preferences.clone(), startup)
                {
                    Ok(editor) => self.mode = DesktopMode::Editor(Box::new(editor)),
                    Err(error) => {
                        self.error = Some(format!("Could not build the editor: {error}"));
                        self.start_scan(context);
                    }
                }
            }
            Err(error) => {
                self.error = Some(error);
                self.start_scan(context);
            }
        }
    }

    fn begin_open(&mut self, path: PathBuf, context: &egui::Context) {
        self.error = None;
        if let Err(error) = self.library.remember(&path, false) {
            self.notice = Some(error);
        }
        let repaint = context.clone();
        let (sender, receiver) = mpsc::channel();
        let worker_path = path.clone();
        let spawn = thread::Builder::new()
            .name("gaw-project-open".into())
            .spawn(move || {
                let result = NativeStartup::open(&worker_path, RecoveryPolicy::Recover)
                    .map_err(|error| format!("Could not open {}: {error}", worker_path.display()));
                let _ = sender.send(result);
                repaint.request_repaint();
            });
        match spawn {
            Ok(worker) => {
                self.mode = DesktopMode::Pending(PendingProject {
                    path,
                    label: "OPENING PROJECT",
                    receiver,
                    worker: Some(worker),
                });
            }
            Err(error) => self.error = Some(format!("Could not start project open: {error}")),
        }
    }

    fn begin_create(
        &mut self,
        path: PathBuf,
        name: String,
        bpm: f64,
        sample_rate: u32,
        context: &egui::Context,
    ) {
        self.error = None;
        let repaint = context.clone();
        let (sender, receiver) = mpsc::channel();
        let worker_path = path.clone();
        let spawn = thread::Builder::new()
            .name("gaw-project-create".into())
            .spawn(move || {
                let result = ProjectStore::create_default(&worker_path, &name, bpm, sample_rate)
                    .map_err(anyhow::Error::from)
                    .and_then(|_| NativeStartup::open(&worker_path, RecoveryPolicy::Recover))
                    .map_err(|error| {
                        format!("Could not create {}: {error}", worker_path.display())
                    });
                let _ = sender.send(result);
                repaint.request_repaint();
            });
        match spawn {
            Ok(worker) => {
                self.create = None;
                self.mode = DesktopMode::Pending(PendingProject {
                    path,
                    label: "CREATING PROJECT",
                    receiver,
                    worker: Some(worker),
                });
            }
            Err(error) => self.error = Some(format!("Could not start project creation: {error}")),
        }
    }

    fn open_picker(&mut self, context: &egui::Context) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Open GAW Project Folder")
            .pick_folder()
        {
            self.begin_open(path, context);
        }
    }

    fn library_logic(&mut self, context: &egui::Context) {
        self.poll_scan(context);
        if context.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::N)) {
            self.create = Some(NewProjectDraft::new(self.library.primary_root().to_owned()));
        }
        if context.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
            self.open_picker(context);
        }
        if context.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F5)) {
            self.start_scan(context);
        }
        let dropped = context.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .find_map(|file| file.path.clone())
        });
        if let Some(mut path) = dropped {
            if path.file_name().is_some_and(|name| name == "project.json") {
                path.pop();
            }
            self.begin_open(path, context);
        }
    }

    fn library_ui(&mut self, ui: &mut egui::Ui) {
        let context = ui.ctx().clone();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(CANVAS).inner_margin(28))
            .show_inside(ui, |ui| {
                ui.set_width(ui.available_width().min(1180.0));
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("GAW")
                            .font(FontId::new(24.0, FontFamily::Monospace))
                            .color(HIGHLIGHT),
                    );
                    ui.label(
                        RichText::new("/ PROJECTS")
                            .font(FontId::new(12.0, FontFamily::Monospace))
                            .color(DIM),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("PROJECT LOCATIONS").clicked() {
                            self.manage_locations = true;
                        }
                    });
                });
                ui.add_space(42.0);
                ui.label(RichText::new("PROJECTS").size(34.0).color(TEXT));
                ui.label(
                    RichText::new(
                        "Open a GAW project folder, or create a new one in your managed library.",
                    )
                    .size(13.0)
                    .color(DIM),
                );
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    if primary_button(ui, "+  NEW PROJECT").clicked() {
                        self.create =
                            Some(NewProjectDraft::new(self.library.primary_root().to_owned()));
                    }
                    if ui.button("OPEN PROJECT FOLDER…").clicked() {
                        self.open_picker(&context);
                    }
                    if ui.button("↻  REFRESH").clicked() {
                        self.start_scan(&context);
                    }
                });

                if let Some(error) = &self.error {
                    ui.add_space(12.0);
                    status_panel(ui, error, STATUS_ERROR);
                } else if let Some(notice) = &self.notice {
                    ui.add_space(12.0);
                    status_panel(ui, notice, HIGHLIGHT);
                }

                ui.add_space(22.0);
                ui.horizontal(|ui| {
                    ui.add(mono_label("SEARCH"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.search)
                            .hint_text("Project name or path")
                            .desired_width(320.0),
                    );
                    if self.scan.is_some() {
                        ui.spinner();
                        ui.label(RichText::new("SCANNING…").monospace().small().color(DIM));
                    }
                });
                ui.add_space(10.0);
                self.project_table(ui);
            });
        self.new_project_window(&context);
        self.locations_window(&context);
    }

    fn project_table(&mut self, ui: &mut egui::Ui) {
        let query = self.search.trim().to_lowercase();
        let visible = self
            .entries
            .iter()
            .filter(|entry| query.is_empty() || entry.searchable_text().contains(&query))
            .cloned()
            .collect::<Vec<_>>();
        if visible.is_empty() {
            egui::Frame::new()
                .fill(PANEL)
                .stroke(Stroke::new(1.0, BORDER))
                .inner_margin(32)
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        let title = if self.scan.is_some() {
                            "LOOKING FOR PROJECTS…"
                        } else if query.is_empty() {
                            "NO PROJECTS YET"
                        } else {
                            "NO MATCHING PROJECTS"
                        };
                        ui.label(RichText::new(title).monospace().color(TEXT));
                        ui.label(
                            RichText::new("Create a blank project or open an existing folder.")
                                .color(DIM),
                        );
                    });
                });
            return;
        }

        egui::Frame::new()
            .fill(PANEL)
            .stroke(Stroke::new(1.0, BORDER))
            .show(ui, |ui| {
                let widths = ProjectTableWidths::new(ui.available_width() - 16.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.add_sized([widths.name, 24.0], mono_label("NAME"));
                    ui.add_sized([widths.location, 24.0], mono_label("LOCATION"));
                    ui.add_sized([widths.tempo, 24.0], mono_label("TEMPO"));
                    ui.add_sized([widths.rate, 24.0], mono_label("RATE"));
                    ui.add_sized([widths.last_opened, 24.0], mono_label("LAST OPENED"));
                    ui.add_sized([widths.status, 24.0], mono_label("STATUS"));
                });
                ui.separator();
                egui::ScrollArea::vertical()
                    .max_height(390.0)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for entry in visible {
                            self.project_row(ui, &entry, widths);
                        }
                    });
            });

        let selected = self
            .selected
            .as_ref()
            .and_then(|path| self.entries.iter().find(|entry| &entry.path == path))
            .cloned();
        if let Some(entry) = selected {
            ui.add_space(10.0);
            egui::Frame::new()
                .fill(PANEL_ALT)
                .stroke(Stroke::new(1.0, BORDER))
                .inner_margin(12)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(entry.path.display().to_string()).monospace());
                        ui.add_space((ui.available_width() - 230.0).max(0.0));
                        if ui
                            .add_enabled(
                                matches!(entry.status, ProjectStatus::Available),
                                egui::Button::new("OPEN"),
                            )
                            .clicked()
                        {
                            self.begin_open(entry.path.clone(), ui.ctx());
                        }
                        if !entry.managed && ui.button("REMOVE FROM LIST").clicked() {
                            match self.library.forget(&entry.path) {
                                Ok(()) => {
                                    self.selected = None;
                                    self.start_scan(ui.ctx());
                                    self.notice = Some(
                                        "Removed from the list. Project files were not deleted."
                                            .into(),
                                    );
                                }
                                Err(error) => self.error = Some(error),
                            }
                        }
                    });
                    if let Some(project_id) = &entry.project_id {
                        ui.label(
                            RichText::new(format!("PROJECT ID  {project_id}"))
                                .monospace()
                                .small()
                                .color(DIM),
                        );
                    }
                    if let ProjectStatus::Invalid(error) = &entry.status {
                        ui.label(RichText::new(error).color(STATUS_ERROR).small());
                    }
                });
        }
    }

    fn project_row(&mut self, ui: &mut egui::Ui, entry: &ProjectEntry, widths: ProjectTableWidths) {
        let selected = self.selected.as_ref() == Some(&entry.path);
        let response = egui::Frame::new()
            .fill(if selected { PANEL_RAISED } else { PANEL })
            .stroke(Stroke::new(
                if selected { 2.0 } else { 0.0 },
                if selected {
                    HIGHLIGHT
                } else {
                    Color32::TRANSPARENT
                },
            ))
            .inner_margin(8)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [widths.name, 30.0],
                        egui::Label::new(RichText::new(&entry.name).size(14.0).color(TEXT)),
                    );
                    ui.add_sized(
                        [widths.location, 30.0],
                        egui::Label::new(
                            RichText::new(entry.path.display().to_string())
                                .monospace()
                                .small()
                                .color(DIM),
                        ),
                    );
                    ui.add_sized(
                        [widths.tempo, 30.0],
                        egui::Label::new(entry.bpm.map_or_else(|| "—".into(), format_bpm)),
                    );
                    ui.add_sized(
                        [widths.rate, 30.0],
                        egui::Label::new(
                            entry
                                .sample_rate
                                .map_or_else(|| "—".into(), format_sample_rate),
                        ),
                    );
                    ui.add_sized(
                        [widths.last_opened, 30.0],
                        egui::Label::new(last_opened_label(entry.last_opened_unix_ms)),
                    );
                    let (status, color) = match &entry.status {
                        ProjectStatus::Available => ("FOUND", DIM),
                        ProjectStatus::Missing => ("MISSING", STATUS_ERROR),
                        ProjectStatus::Invalid(_) => ("CAN'T OPEN", STATUS_ERROR),
                    };
                    ui.add_sized(
                        [widths.status, 30.0],
                        egui::Label::new(RichText::new(status).monospace().small().color(color)),
                    );
                });
            })
            .response
            .interact(Sense::click());
        if response.clicked() {
            self.selected = Some(entry.path.clone());
        }
        if response.double_clicked() && matches!(entry.status, ProjectStatus::Available) {
            self.begin_open(entry.path.clone(), ui.ctx());
        }
    }

    fn new_project_window(&mut self, context: &egui::Context) {
        let Some(mut draft) = self.create.take() else {
            return;
        };
        let mut open = true;
        let mut cancel = false;
        let mut create_request = None;
        egui::Window::new("NEW PROJECT")
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(520.0)
            .show(context, |ui| {
                ui.add(mono_label("PROJECT NAME"));
                let name = ui.add(
                    egui::TextEdit::singleline(&mut draft.name)
                        .hint_text("Untitled Project")
                        .desired_width(f32::INFINITY),
                );
                if name.changed() {
                    draft.sync_folder();
                }
                ui.add_space(8.0);
                ui.add(mono_label("LOCATION"));
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [390.0, 24.0],
                        egui::Label::new(
                            RichText::new(draft.parent.display().to_string())
                                .monospace()
                                .small(),
                        ),
                    );
                    if ui.button("CHOOSE…").clicked()
                        && let Some(parent) = rfd::FileDialog::new()
                            .set_title("Choose Project Location")
                            .pick_folder()
                    {
                        draft.parent = parent;
                    }
                });
                ui.add_space(8.0);
                ui.add(mono_label("FOLDER"));
                ui.add(
                    egui::TextEdit::singleline(&mut draft.folder)
                        .hint_text("project-folder")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.add(mono_label("TEMPO"));
                        ui.add(egui::TextEdit::singleline(&mut draft.bpm).desired_width(110.0));
                    });
                    ui.add_space(14.0);
                    ui.vertical(|ui| {
                        ui.add(mono_label("SAMPLE RATE"));
                        egui::ComboBox::from_id_salt("new_project_sample_rate")
                            .selected_text(format!("{} Hz", draft.sample_rate))
                            .show_ui(ui, |ui| {
                                for rate in SAMPLE_RATES {
                                    ui.selectable_value(
                                        &mut draft.sample_rate,
                                        rate,
                                        format!("{rate} Hz"),
                                    );
                                }
                            });
                    });
                    ui.add_space(14.0);
                    ui.vertical(|ui| {
                        ui.add(mono_label("TIME SIGNATURE"));
                        ui.label("4 / 4");
                    });
                });
                ui.add_space(10.0);
                ui.add(mono_label("DESTINATION"));
                ui.label(
                    RichText::new(draft.parent.join(&draft.folder).display().to_string())
                        .monospace()
                        .small()
                        .color(DIM),
                );
                if let Some(error) = &draft.error {
                    ui.add_space(8.0);
                    ui.label(RichText::new(error).color(STATUS_ERROR));
                }
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if primary_button(ui, "CREATE PROJECT").clicked() {
                        match draft.validate() {
                            Ok(request) => create_request = Some(request),
                            Err(error) => draft.error = Some(error),
                        }
                    }
                    if ui.button("CANCEL").clicked() {
                        cancel = true;
                    }
                });
            });
        open &= !cancel;
        if let Some((path, name, bpm, sample_rate)) = create_request {
            self.begin_create(path, name, bpm, sample_rate, context);
        } else if open {
            self.create = Some(draft);
        }
    }

    fn locations_window(&mut self, context: &egui::Context) {
        if !self.manage_locations {
            return;
        }
        let mut open = self.manage_locations;
        let roots = self.library.roots().to_vec();
        let mut remove = None;
        egui::Window::new("PROJECT LOCATIONS")
            .open(&mut open)
            .collapsible(false)
            .default_width(620.0)
            .show(context, |ui| {
                ui.label(
                    RichText::new(
                        "GAW scans direct child folders in these locations. Removing a location never deletes projects.",
                    )
                    .color(DIM),
                );
                ui.add_space(10.0);
                for root in &roots {
                    egui::Frame::new()
                        .fill(PANEL_ALT)
                        .inner_margin(8)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(root.display().to_string()).monospace());
                                ui.add_space(ui.available_width() - 90.0);
                                if roots.len() > 1 && ui.button("REMOVE").clicked() {
                                    remove = Some(root.clone());
                                }
                            });
                        });
                }
                ui.add_space(10.0);
                if ui.button("+ ADD LOCATION…").clicked()
                    && let Some(root) = rfd::FileDialog::new()
                        .set_title("Add GAW Project Location")
                        .pick_folder()
                {
                    match self.library.add_root(&root) {
                        Ok(()) => self.start_scan(context),
                        Err(error) => self.error = Some(error),
                    }
                }
            });
        if let Some(root) = remove {
            match self.library.remove_root(&root) {
                Ok(()) => self.start_scan(context),
                Err(error) => self.error = Some(error),
            }
        }
        self.manage_locations = open;
    }

    fn pending_ui(ui: &mut egui::Ui, pending: &PendingProject) {
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(CANVAS))
            .show_inside(ui, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.spinner();
                        ui.add_space(12.0);
                        ui.label(RichText::new(pending.label).monospace().color(HIGHLIGHT));
                        ui.label(
                            RichText::new(pending.path.display().to_string())
                                .monospace()
                                .small()
                                .color(DIM),
                        );
                    });
                });
            });
    }
}

impl eframe::App for GawDesktop {
    fn logic(&mut self, context: &egui::Context, frame: &mut eframe::Frame) {
        match &mut self.mode {
            DesktopMode::Library => self.library_logic(context),
            DesktopMode::Pending(_) => {
                self.poll_pending(context);
                context.request_repaint_after(Duration::from_millis(40));
            }
            DesktopMode::Editor(editor) => eframe::App::logic(editor.as_mut(), context, frame),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        match &mut self.mode {
            DesktopMode::Library => self.library_ui(ui),
            DesktopMode::Pending(pending) => Self::pending_ui(ui, pending),
            DesktopMode::Editor(editor) => eframe::App::ui(editor.as_mut(), ui, frame),
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if let DesktopMode::Editor(editor) = &mut self.mode {
            eframe::App::save(editor.as_mut(), storage);
        } else {
            self.audio_preferences.save(storage);
        }
    }

    fn on_exit(&mut self) {
        if let DesktopMode::Editor(editor) = &mut self.mode {
            eframe::App::on_exit(editor.as_mut());
        }
    }
}

impl Drop for GawDesktop {
    fn drop(&mut self) {
        if let DesktopMode::Pending(pending) = &mut self.mode
            && let Some(worker) = pending.worker.take()
        {
            let _ = worker.join();
        }
    }
}

fn project_folder_name(name: &str) -> String {
    let mut result = String::new();
    let mut separator = false;
    for character in name.trim().chars() {
        if character.is_alphanumeric() || matches!(character, '-' | '_') {
            if separator && !result.is_empty() {
                result.push('-');
            }
            separator = false;
            result.extend(character.to_lowercase());
        } else {
            separator = true;
        }
    }
    result.trim_matches('-').to_owned()
}

fn last_opened_label(timestamp: Option<u64>) -> String {
    let Some(timestamp) = timestamp else {
        return "Never".into();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        });
    let days = now.saturating_sub(timestamp) / 86_400_000;
    match days {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        value => format!("{value}d ago"),
    }
}

fn format_bpm(bpm: f64) -> String {
    if bpm.fract().abs() < f64::EPSILON {
        format!("{bpm:.0}")
    } else {
        format!("{bpm:.1}")
    }
}

fn format_sample_rate(rate: u32) -> String {
    if rate.is_multiple_of(1_000) {
        format!("{} kHz", rate / 1_000)
    } else {
        format!("{:.1} kHz", f64::from(rate) / 1_000.0)
    }
}

fn mono_label(text: &'static str) -> egui::Label {
    egui::Label::new(RichText::new(text).monospace().small().color(DIM))
}

fn primary_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(
        egui::Button::new(RichText::new(text).monospace().color(Color32::WHITE))
            .fill(HIGHLIGHT.gamma_multiply(0.55))
            .stroke(Stroke::new(1.0, HIGHLIGHT))
            .min_size(Vec2::new(142.0, 32.0)),
    )
}

fn status_panel(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(PANEL_ALT)
        .stroke(Stroke::new(1.0, color))
        .inner_margin(9)
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(color));
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_folder_names_are_portable_and_stable() {
        assert_eq!(project_folder_name("  Night Drive 03  "), "night-drive-03");
        assert_eq!(project_folder_name("Synth___Lab"), "synth___lab");
        assert_eq!(project_folder_name("A/B: Test"), "a-b-test");
    }

    #[test]
    fn sample_rates_keep_fractional_kilohertz() {
        assert_eq!(format_sample_rate(44_100), "44.1 kHz");
        assert_eq!(format_sample_rate(48_000), "48 kHz");
    }

    #[test]
    fn new_project_validation_rejects_collisions_and_bad_tempo() {
        let directory = tempfile::tempdir().unwrap();
        let mut draft = NewProjectDraft::new(directory.path().to_owned());
        draft.name = "Song".into();
        draft.folder = "song".into();
        draft.bpm = "nope".into();
        assert_eq!(draft.validate().unwrap_err(), "Tempo must be a number");
        draft.bpm = "120".into();
        std::fs::create_dir(directory.path().join("song")).unwrap();
        assert!(draft.validate().unwrap_err().contains("already exists"));
    }
}
