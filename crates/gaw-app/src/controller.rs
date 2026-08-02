#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::{
    collections::{VecDeque, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    path::Path,
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use gaw_audio::{
    ChannelLayout, CommandSender, CpalOutput, RealtimeCommand, RealtimeEngineConfig,
    RenderSnapshot, command_queue, compile_project_store,
};
use gaw_core::{Project, Transaction};
use gaw_project::{ProjectSession, ProjectStore};

use crate::model::{ChangeSource, DemoViewModel, RenderState, Transport};

const PROJECT_QUEUE: usize = 64;
const PROJECT_EVENTS: usize = 32;
const WATCH_INTERVAL: Duration = Duration::from_millis(150);
const WATCH_MAX_ENTRIES: usize = 4_096;
const WATCH_MAX_DEPTH: usize = 12;
const AUDIO_PAGE_FRAMES: usize = 65_536;
const AUDIO_PAGE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RecoveryPolicy {
    #[default]
    Recover,
    Discard,
    Abort,
}

#[derive(Debug)]
pub struct NativeStartup {
    project: Project,
    session: ProjectSession,
    recovered: usize,
    discarded: usize,
}

impl NativeStartup {
    /// Opens and fully validates a canonical project before the GUI starts.
    ///
    /// # Errors
    /// Returns a storage, validation, or recovery-policy error.
    pub fn open(root: impl AsRef<Path>, policy: RecoveryPolicy) -> anyhow::Result<Self> {
        let store = ProjectStore::open(root)?;
        let pending = store.pending_recovery()?.len();
        match policy {
            RecoveryPolicy::Discard if pending > 0 => store.clear_recovery()?,
            RecoveryPolicy::Abort if pending > 0 => anyhow::bail!(
                "project has {pending} recovery record(s); reopen with --recovery recover or --recovery discard"
            ),
            RecoveryPolicy::Recover | RecoveryPolicy::Discard | RecoveryPolicy::Abort => {}
        }
        let session = ProjectSession::open(store)?;
        Ok(Self {
            project: session.project().clone(),
            session,
            recovered: usize::from(policy == RecoveryPolicy::Recover) * pending,
            discarded: usize::from(policy == RecoveryPolicy::Discard) * pending,
        })
    }

    pub fn project(&self) -> &Project {
        &self.project
    }
}

#[derive(Debug)]
enum ProjectCommand {
    Apply {
        revision: u64,
        transaction: Arc<Transaction>,
    },
    ReplaceSnapshot {
        revision: u64,
        project: Project,
    },
    Save {
        revision: u64,
        project: Project,
    },
    #[cfg(test)]
    PollNow,
    #[cfg(test)]
    Barrier(std::sync::mpsc::SyncSender<()>),
    Close,
    Abandon,
}

#[derive(Debug)]
enum ProjectEvent {
    CanonicalReady(u64),
    External(Project),
    Saved(u64),
    Error(ControllerError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ControllerError {
    subsystem: &'static str,
    message: String,
}

impl ControllerError {
    fn new(subsystem: &'static str, error: impl std::fmt::Display) -> Self {
        Self {
            subsystem,
            message: error.to_string(),
        }
    }
}

#[derive(Debug)]
struct ProjectWorker {
    sender: Option<Sender<ProjectCommand>>,
    events: Receiver<ProjectEvent>,
    join: Option<JoinHandle<()>>,
}

impl ProjectWorker {
    fn spawn(session: ProjectSession) -> Self {
        let (sender, commands) = bounded(PROJECT_QUEUE);
        let (event_sender, events) = bounded(PROJECT_EVENTS);
        let join = thread::Builder::new()
            .name("gaw-project-controller".into())
            .spawn(move || project_worker(session, &commands, &event_sender))
            .expect("project controller thread should start");
        Self {
            sender: Some(sender),
            events,
            join: Some(join),
        }
    }

    #[allow(clippy::result_large_err)]
    fn try_send(&self, command: ProjectCommand) -> Result<(), ProjectCommand> {
        let Some(sender) = &self.sender else {
            return Err(command);
        };
        sender.try_send(command).map_err(|error| match error {
            TrySendError::Full(command) | TrySendError::Disconnected(command) => command,
        })
    }

    fn close(&mut self, clean: bool) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(if clean {
                ProjectCommand::Close
            } else {
                ProjectCommand::Abandon
            });
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ProjectWorker {
    fn drop(&mut self) {
        self.close(false);
    }
}

fn project_worker(
    mut session: ProjectSession,
    commands: &Receiver<ProjectCommand>,
    events: &Sender<ProjectEvent>,
) {
    let store = session.store().clone();
    let mut dirty = false;
    let mut failed = false;
    let mut revision = 0;
    let mut baseline = project_fingerprint(store.root()).ok();
    let mut next_watch = Instant::now() + WATCH_INTERVAL;
    let _ = events.try_send(ProjectEvent::CanonicalReady(0));
    loop {
        let wait = next_watch.saturating_duration_since(Instant::now());
        let command = commands.recv_timeout(wait.min(Duration::from_millis(25)));
        if let Ok(command) = command {
            match command {
                ProjectCommand::Apply { .. } if failed => send_error(
                    events,
                    "persistence",
                    "persistence is paused; use Save to retry an atomic snapshot",
                ),
                ProjectCommand::Apply {
                    revision: next,
                    transaction,
                } => match session.apply_transaction(&transaction) {
                    Ok(()) => {
                        dirty = true;
                        revision = revision.max(next);
                    }
                    Err(error) => {
                        failed = true;
                        send_error(events, "persistence", error);
                    }
                },
                ProjectCommand::ReplaceSnapshot {
                    revision: next,
                    project,
                } => {
                    let result = (|| -> gaw_project::Result<()> {
                        session.checkpoint()?;
                        store.save_project(&project)?;
                        session = ProjectSession::open(store.clone())?;
                        Ok(())
                    })();
                    match result {
                        Ok(()) => {
                            dirty = false;
                            failed = false;
                            revision = revision.max(next);
                            baseline = project_fingerprint(store.root()).ok();
                            let _ = events.try_send(ProjectEvent::CanonicalReady(next));
                        }
                        Err(error) => send_error(events, "persistence", error),
                    }
                }
                ProjectCommand::Save {
                    revision: next,
                    project,
                } => {
                    let result = (|| -> gaw_project::Result<()> {
                        session.checkpoint()?;
                        store.save_project(&project)?;
                        session = ProjectSession::open(store.clone())?;
                        Ok(())
                    })();
                    match result {
                        Ok(()) => {
                            dirty = false;
                            failed = false;
                            revision = revision.max(next);
                            baseline = project_fingerprint(store.root()).ok();
                            let _ = events.try_send(ProjectEvent::CanonicalReady(revision));
                            let _ = events.try_send(ProjectEvent::Saved(revision));
                        }
                        Err(error) => {
                            failed = true;
                            send_error(events, "persistence", error);
                        }
                    }
                }
                #[cfg(test)]
                ProjectCommand::PollNow => {
                    poll_external(&store, &mut session, dirty, &mut baseline, events);
                }
                #[cfg(test)]
                ProjectCommand::Barrier(done) => {
                    let _ = done.send(());
                }
                ProjectCommand::Close => {
                    if failed {
                        send_error(
                            events,
                            "persistence",
                            "uncleared persistence failure left recovery data for next launch",
                        );
                    } else if let Err(error) = session.close() {
                        send_error(events, "persistence", error);
                    }
                    break;
                }
                ProjectCommand::Abandon => break,
            }
        }

        if dirty && !failed {
            match session.checkpoint_if_idle() {
                Ok(true) => {
                    dirty = false;
                    baseline = project_fingerprint(store.root()).ok();
                    let _ = events.try_send(ProjectEvent::CanonicalReady(revision));
                }
                Ok(false) => {}
                Err(error) => send_error(events, "persistence", error),
            }
        }
        if Instant::now() >= next_watch {
            poll_external(&store, &mut session, dirty, &mut baseline, events);
            next_watch = Instant::now() + WATCH_INTERVAL;
        }
    }
}

fn send_error(
    events: &Sender<ProjectEvent>,
    subsystem: &'static str,
    error: impl std::fmt::Display,
) {
    let _ = events.try_send(ProjectEvent::Error(ControllerError::new(subsystem, error)));
}

fn poll_external(
    store: &ProjectStore,
    session: &mut ProjectSession,
    dirty: bool,
    baseline: &mut Option<u64>,
    events: &Sender<ProjectEvent>,
) {
    let fingerprint = match project_fingerprint(store.root()) {
        Ok(value) => value,
        Err(error) => {
            send_error(events, "watcher", error);
            return;
        }
    };
    if baseline.is_some_and(|value| value == fingerprint) {
        return;
    }
    if dirty {
        send_error(
            events,
            "watcher",
            "external canonical change arrived while GUI edits are pending",
        );
        return;
    }
    let result = ProjectSession::open(store.clone());
    match result {
        Ok(next) => {
            let changed = next.project() != session.project();
            let project = changed.then(|| next.project().clone());
            *session = next;
            *baseline = project_fingerprint(store.root()).ok();
            if let Some(project) = project {
                let _ = events.try_send(ProjectEvent::External(project));
            }
        }
        Err(error) => send_error(events, "external project", error),
    }
}

fn project_fingerprint(root: &Path) -> std::io::Result<u64> {
    let mut hasher = DefaultHasher::new();
    let mut pending = vec![(root.to_path_buf(), 0_usize)];
    let mut entries = 0_usize;
    while let Some((directory, depth)) = pending.pop() {
        if depth > WATCH_MAX_DEPTH {
            return Err(std::io::Error::other("canonical tree is too deep to watch"));
        }
        let mut children = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        children.sort_by_key(fs::DirEntry::file_name);
        for entry in children {
            entries += 1;
            if entries > WATCH_MAX_ENTRIES {
                return Err(std::io::Error::other(
                    "canonical tree exceeds watcher entry limit",
                ));
            }
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap_or(&path);
            if relative.starts_with(".gaw")
                || relative.starts_with("assets/media")
                || relative.starts_with("presets")
            {
                continue;
            }
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                pending.push((path, depth + 1));
            } else if file_type.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "json")
            {
                relative.hash(&mut hasher);
                let metadata = entry.metadata()?;
                metadata.len().hash(&mut hasher);
                metadata.modified()?.hash(&mut hasher);
            }
        }
    }
    Ok(hasher.finish())
}

#[derive(Debug)]
struct CompileJob {
    revision: u64,
    store: ProjectStore,
}

#[derive(Debug)]
struct CompileResult {
    revision: u64,
    result: Result<Arc<RenderSnapshot>, String>,
}

#[derive(Debug, Default)]
struct CompileState {
    pending: Option<CompileJob>,
    completed: Option<CompileResult>,
    closed: bool,
}

#[derive(Debug)]
struct CompileWorker {
    state: Arc<(Mutex<CompileState>, Condvar)>,
    join: Option<JoinHandle<()>>,
}

impl CompileWorker {
    fn spawn() -> Self {
        let state = Arc::new((Mutex::new(CompileState::default()), Condvar::new()));
        let worker_state = Arc::clone(&state);
        let join = thread::Builder::new()
            .name("gaw-audio-compiler".into())
            .spawn(move || compile_worker(&worker_state))
            .expect("audio compiler thread should start");
        Self {
            state,
            join: Some(join),
        }
    }

    fn request(&self, revision: u64, store: ProjectStore) {
        let (lock, ready) = &*self.state;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = Some(CompileJob { revision, store });
        ready.notify_one();
    }

    fn take_completed(&self) -> Option<CompileResult> {
        self.state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .completed
            .take()
    }
}

impl Drop for CompileWorker {
    fn drop(&mut self) {
        let (lock, ready) = &*self.state;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        ready.notify_one();
        // Compilation can be long; detaching avoids blocking application close.
        self.join.take();
    }
}

fn compile_worker(state: &Arc<(Mutex<CompileState>, Condvar)>) {
    loop {
        let job = {
            let (lock, ready) = &**state;
            let mut value = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            while value.pending.is_none() && !value.closed {
                value = ready
                    .wait(value)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
            if value.closed {
                return;
            }
            value.pending.take().expect("pending compile exists")
        };
        let result = compile_snapshot(&job.store).map(Arc::new);
        let mut value = state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        value.completed = Some(CompileResult {
            revision: job.revision,
            result,
        });
    }
}

fn compile_snapshot(store: &ProjectStore) -> Result<RenderSnapshot, String> {
    let compiled = compile_project_store(store).map_err(|error| error.to_string())?;
    let root = compiled.plan().root();
    let channels = root.output_layout.channels();
    let bytes_per_page = AUDIO_PAGE_FRAMES
        .saturating_mul(channels)
        .saturating_mul(size_of::<f32>());
    let maximum_pages = (AUDIO_PAGE_BYTES / bytes_per_page).max(1);
    let total = root.length_frames.saturating_add(root.tail_frames);
    let mut pages = Vec::new();
    let mut start = 0_u64;
    while start < total && pages.len() < maximum_pages {
        let frames = usize::try_from(total.saturating_sub(start))
            .unwrap_or(usize::MAX)
            .min(AUDIO_PAGE_FRAMES);
        pages.push(
            compiled
                .prepare_page(start, frames)
                .map_err(|error| error.to_string())?,
        );
        start = start.saturating_add(frames as u64);
    }
    compiled
        .paged_snapshot(pages)
        .map_err(|error| error.to_string())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TransportView {
    playing: bool,
    loop_enabled: bool,
    loop_start: f32,
    loop_end: f32,
    playhead: f32,
    bpm: f32,
}

impl From<&Transport> for TransportView {
    fn from(value: &Transport) -> Self {
        Self {
            playing: value.playing,
            loop_enabled: value.loop_enabled,
            loop_start: value.loop_start,
            loop_end: value.loop_end,
            playhead: value.playhead,
            bpm: value.bpm,
        }
    }
}

#[derive(Debug)]
struct AudioOutput {
    commands: CommandSender,
    _device: CpalOutput,
}

impl AudioOutput {
    fn open(sample_rate: u32, stream_error: Arc<Mutex<Option<String>>>) -> Result<Self, String> {
        let config = RealtimeEngineConfig {
            sample_rate,
            output_layout: ChannelLayout::Stereo,
            maximum_block_frames: 8_192,
            maximum_commands_per_block: 64,
        };
        let (commands, engine) = command_queue(config, 128, 8).map_err(|e| e.to_string())?;
        let device = CpalOutput::open_default_negotiated(engine, move |error| {
            *stream_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.to_string());
        })
        .map_err(|error| error.to_string())?;
        device.play().map_err(|error| error.to_string())?;
        Ok(Self {
            commands,
            _device: device,
        })
    }
}

#[derive(Debug)]
pub(crate) struct NativeController {
    store: ProjectStore,
    project: ProjectWorker,
    compiler: CompileWorker,
    audio: Option<AudioOutput>,
    pending_project: VecDeque<ProjectCommand>,
    pending_audio: VecDeque<RealtimeCommand>,
    latest_revision: u64,
    last_transport: TransportView,
    notice: Option<String>,
    error: Option<ControllerError>,
    stream_error: Arc<Mutex<Option<String>>>,
    closed: bool,
}

impl NativeController {
    pub(crate) fn start(startup: NativeStartup) -> Self {
        let store = startup.session.store().clone();
        let stream_error = Arc::new(Mutex::new(None));
        let audio = AudioOutput::open(
            startup.project.sample_rate.value(),
            Arc::clone(&stream_error),
        )
        .ok();
        let notice = if startup.recovered > 0 {
            Some(format!("Recovered {} journaled edit(s)", startup.recovered))
        } else if startup.discarded > 0 {
            Some(format!(
                "Discarded {} recovery record(s)",
                startup.discarded
            ))
        } else {
            None
        };
        let last_transport = TransportView {
            playing: false,
            loop_enabled: true,
            loop_start: 0.0,
            loop_end: 0.0,
            playhead: 0.0,
            bpm: startup.project.bpm.value() as f32,
        };
        Self {
            store,
            project: ProjectWorker::spawn(startup.session),
            compiler: CompileWorker::spawn(),
            audio,
            pending_project: VecDeque::new(),
            pending_audio: VecDeque::new(),
            latest_revision: 0,
            last_transport,
            notice,
            error: None,
            stream_error,
            closed: false,
        }
    }

    pub(crate) fn initialize_transport(&mut self, transport: &Transport) {
        self.last_transport = transport.into();
        if self.audio.is_none() {
            self.set_error("audio device", "no compatible default output device");
        }
    }

    pub(crate) fn pump(&mut self, vm: &mut DemoViewModel, now: f64) {
        self.flush_project();
        loop {
            match self.project.events.try_recv() {
                Ok(ProjectEvent::CanonicalReady(revision)) => {
                    if revision == self.latest_revision {
                        self.compiler.request(revision, self.store.clone());
                        set_render_state(vm, RenderState::Rendering(0));
                    }
                }
                Ok(ProjectEvent::External(project)) => {
                    let changed = changed_ids(&project);
                    match vm.replace_project_from_agent(project, changed, now) {
                        Ok(()) => {
                            self.latest_revision = vm.revision();
                            self.compiler
                                .request(self.latest_revision, self.store.clone());
                            set_render_state(vm, RenderState::Rendering(0));
                            self.notice = Some("Loaded external canonical change".into());
                        }
                        Err(error) => self.set_error("external project", error),
                    }
                }
                Ok(ProjectEvent::Saved(revision)) => {
                    self.notice = Some(format!("Saved revision {revision}"));
                }
                Ok(ProjectEvent::Error(error)) => self.record_error(error),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        let updates = vm.take_updates().collect::<Vec<_>>();
        for update in updates {
            self.latest_revision = self.latest_revision.max(update.revision);
            match (update.source, update.transaction) {
                (ChangeSource::Ui, Some(transaction)) => {
                    self.enqueue_project(ProjectCommand::Apply {
                        revision: update.revision,
                        transaction,
                    });
                    set_render_state(vm, RenderState::Stale);
                }
                (ChangeSource::Undo | ChangeSource::Redo, None) => {
                    self.enqueue_project(ProjectCommand::ReplaceSnapshot {
                        revision: update.revision,
                        project: vm.project().clone(),
                    });
                    set_render_state(vm, RenderState::Stale);
                }
                _ => {}
            }
        }

        if let Some(completed) = self.compiler.take_completed()
            && completion_is_current(completed.revision, self.latest_revision)
        {
            match completed.result {
                Ok(snapshot) => {
                    self.enqueue_audio(RealtimeCommand::InstallSnapshot(snapshot));
                    set_render_state(vm, RenderState::Fresh);
                    self.notice = Some(format!("Audio ready · r{}", completed.revision));
                }
                Err(error) => {
                    set_render_state(vm, RenderState::Stale);
                    self.set_error("audio compile", error);
                }
            }
        }
        let stream_error = self
            .stream_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(error) = stream_error {
            self.set_error("audio stream", error);
        }
        self.sync_transport(vm);
        self.flush_audio();
        if let Some(audio) = &self.audio {
            audio.commands.reclaim_retired();
        }
    }

    pub(crate) fn save(&mut self, revision: u64, project: Project) {
        self.enqueue_project(ProjectCommand::Save { revision, project });
    }

    pub(crate) fn paint_status(&self, context: &egui::Context) {
        let status = self.error.as_ref().map_or_else(
            || self.notice.as_deref(),
            |error| Some(error.message.as_str()),
        );
        let Some(status) = status else { return };
        egui::Area::new(egui::Id::new("native-controller-status"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -10.0))
            .show(context, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(egui::RichText::new(status).monospace().small().color(
                        if self.error.is_some() {
                            egui::Color32::LIGHT_RED
                        } else {
                            egui::Color32::LIGHT_GREEN
                        },
                    ));
                });
            });
    }

    pub(crate) fn close(&mut self) {
        if self.closed {
            return;
        }
        while let Some(command) = self.pending_project.pop_front() {
            let Some(sender) = &self.project.sender else {
                break;
            };
            if sender.send(command).is_err() {
                break;
            }
        }
        self.project.close(true);
        self.closed = true;
    }

    fn enqueue_project(&mut self, command: ProjectCommand) {
        if self.pending_project.len() >= PROJECT_QUEUE * 4 {
            self.set_error("persistence", "bounded persistence backlog is full");
            return;
        }
        self.pending_project.push_back(command);
        self.flush_project();
    }

    fn flush_project(&mut self) {
        while let Some(command) = self.pending_project.pop_front() {
            if let Err(command) = self.project.try_send(command) {
                self.pending_project.push_front(command);
                break;
            }
        }
    }

    fn enqueue_audio(&mut self, command: RealtimeCommand) {
        if self.pending_audio.len() < 128 {
            self.pending_audio.push_back(command);
        } else {
            self.set_error("audio", "bounded realtime command queue is full");
        }
    }

    fn flush_audio(&mut self) {
        let Some(audio) = &self.audio else {
            self.pending_audio.clear();
            return;
        };
        while let Some(command) = self.pending_audio.pop_front() {
            match audio.commands.try_send(command) {
                Ok(()) => {}
                Err(gaw_audio::CommandSendError::Full(command)) => {
                    self.pending_audio.push_front(command);
                    break;
                }
                Err(gaw_audio::CommandSendError::Disconnected(_)) => {
                    self.set_error("audio", "audio callback disconnected");
                    break;
                }
            }
        }
    }

    fn sync_transport(&mut self, vm: &DemoViewModel) {
        let current = TransportView::from(&vm.transport);
        if current.playing != self.last_transport.playing {
            self.enqueue_audio(if current.playing {
                RealtimeCommand::Play
            } else if current.playhead <= f32::EPSILON {
                RealtimeCommand::Stop
            } else {
                RealtimeCommand::Pause
            });
        }
        let jumped = (!current.playing
            && (current.playhead - self.last_transport.playhead).abs() > f32::EPSILON)
            || (current.playing && current.playhead + 0.01 < self.last_transport.playhead)
            || (current.playhead - self.last_transport.playhead).abs() > 0.5;
        if jumped {
            self.enqueue_audio(RealtimeCommand::Seek(beat_to_frame(
                current.playhead,
                current.bpm,
                vm.project().sample_rate.value(),
            )));
        }
        self.last_transport = current;
    }

    fn record_error(&mut self, error: ControllerError) {
        if self.error.as_ref() != Some(&error) {
            tracing::error!(subsystem = error.subsystem, message = %error.message);
            self.error = Some(error);
        }
    }

    fn set_error(&mut self, subsystem: &'static str, error: impl std::fmt::Display) {
        self.record_error(ControllerError::new(subsystem, error));
    }
}

impl Drop for NativeController {
    fn drop(&mut self) {
        if !self.closed {
            self.project.close(false);
        }
    }
}

fn beat_to_frame(beat: f32, bpm: f32, sample_rate: u32) -> u64 {
    if !(beat.is_finite() && bpm.is_finite() && bpm > 0.0) {
        return 0;
    }
    let frame = f64::from(beat.max(0.0)) * 60.0 / f64::from(bpm) * f64::from(sample_rate);
    if frame >= u64::MAX as f64 {
        u64::MAX
    } else {
        frame.round() as u64
    }
}

const fn completion_is_current(completed: u64, latest: u64) -> bool {
    completed == latest
}

fn changed_ids(project: &Project) -> Vec<String> {
    let mut ids = Vec::with_capacity(
        project.assets.len() + project.compositions.len() + project.tracks.len(),
    );
    ids.extend(project.assets.iter().map(|value| value.id.to_string()));
    ids.extend(
        project
            .compositions
            .iter()
            .map(|value| value.id.to_string()),
    );
    ids.extend(project.tracks.iter().map(|value| value.id.to_string()));
    ids.extend(
        project
            .tracks
            .iter()
            .flat_map(|track| &track.clips)
            .map(|clip| clip.id().to_string()),
    );
    ids
}

fn set_render_state(vm: &mut DemoViewModel, state: RenderState) {
    let ids = vm
        .compositions
        .iter()
        .flat_map(|composition| &composition.tracks)
        .flat_map(|track| &track.clips)
        .filter(|clip| matches!(clip.kind, crate::model::ClipKind::Composition { .. }))
        .map(|clip| clip.id.clone())
        .collect::<Vec<_>>();
    for id in ids {
        vm.set_composition_clip_render_state(&id, state);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use gaw_core::{Bpm, Command};

    use crate::model::Intent;

    use super::*;

    fn store() -> (tempfile::TempDir, ProjectStore) {
        let directory = tempfile::tempdir().unwrap();
        let store =
            ProjectStore::create_default(directory.path().join("song"), "Before", 120.0, 48_000)
                .unwrap();
        (directory, store)
    }

    fn barrier(worker: &ProjectWorker) {
        let (sent, received) = mpsc::sync_channel(0);
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(ProjectCommand::Barrier(sent))
            .unwrap();
        received.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn startup_detects_and_recovers_or_discards_journal() {
        let (_directory, store) = store();
        store
            .append_recovery(&Transaction::new([Command::SetProjectName {
                name: "Recovered".into(),
            }]))
            .unwrap();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        assert_eq!(startup.recovered, 1);
        assert_eq!(startup.project.name, "Recovered");
        assert!(store.pending_recovery().unwrap().is_empty());

        store
            .append_recovery(&Transaction::new([Command::SetProjectName {
                name: "Discarded".into(),
            }]))
            .unwrap();
        let discarded = NativeStartup::open(store.root(), RecoveryPolicy::Discard).unwrap();
        assert_eq!(discarded.discarded, 1);
        assert_eq!(discarded.project.name, "Recovered");
    }

    #[test]
    fn worker_journals_once_and_clean_close_checkpoints() {
        let (_directory, store) = store();
        let mut worker = ProjectWorker::spawn(ProjectSession::open(store.clone()).unwrap());
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(ProjectCommand::Apply {
                revision: 1,
                transaction: Arc::new(Transaction::new([Command::SetProjectTempo {
                    bpm: Bpm::new(98.0).unwrap(),
                }])),
            })
            .unwrap();
        barrier(&worker);
        assert_eq!(store.pending_recovery().unwrap().len(), 1);
        assert!((store.load_project().unwrap().bpm.value() - 120.0).abs() < f64::EPSILON);
        worker.close(true);
        assert!(store.pending_recovery().unwrap().is_empty());
        assert!((store.load_project().unwrap().bpm.value() - 98.0).abs() < f64::EPSILON);
    }

    #[test]
    fn ui_transaction_and_undo_persist_without_duplicate_history() {
        let (_directory, store) = store();
        let mut vm = DemoViewModel::from_project(store.load_project().unwrap()).unwrap();
        let mut worker = ProjectWorker::spawn(ProjectSession::open(store.clone()).unwrap());

        vm.apply(Intent::SetBpm(98.0));
        let update = vm.take_updates().next().unwrap();
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(ProjectCommand::Apply {
                revision: update.revision,
                transaction: update.transaction.unwrap(),
            })
            .unwrap();
        barrier(&worker);
        assert_eq!(store.pending_recovery().unwrap().len(), 1);

        vm.apply(Intent::Undo(1.0));
        assert!((vm.project().bpm.value() - 120.0).abs() < f64::EPSILON);
        let undo = vm.take_updates().next().unwrap();
        assert_eq!(undo.source, ChangeSource::Undo);
        assert!(undo.transaction.is_none());
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(ProjectCommand::ReplaceSnapshot {
                revision: undo.revision,
                project: vm.project().clone(),
            })
            .unwrap();
        barrier(&worker);
        assert!(store.pending_recovery().unwrap().is_empty());
        assert!((store.load_project().unwrap().bpm.value() - 120.0).abs() < f64::EPSILON);

        vm.apply(Intent::Undo(2.0));
        assert!(vm.last_error().is_some_and(|error| error.contains("undo")));
        worker.close(false);
    }

    #[test]
    fn abandoned_worker_leaves_recovery_journal() {
        let (_directory, store) = store();
        let mut worker = ProjectWorker::spawn(ProjectSession::open(store.clone()).unwrap());
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(ProjectCommand::Apply {
                revision: 1,
                transaction: Arc::new(Transaction::new([Command::SetProjectName {
                    name: "Dirty".into(),
                }])),
            })
            .unwrap();
        barrier(&worker);
        worker.close(false);
        assert_eq!(store.pending_recovery().unwrap().len(), 1);
    }

    #[test]
    fn failed_external_edit_preserves_last_valid_project_then_recovers() {
        let (_directory, store) = store();
        let mut worker = ProjectWorker::spawn(ProjectSession::open(store.clone()).unwrap());
        barrier(&worker);
        worker.events.try_iter().for_each(drop);
        let project_path = store.root().join("project.json");
        let valid = fs::read(&project_path).unwrap();
        fs::write(&project_path, b"{broken").unwrap();
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(ProjectCommand::PollNow)
            .unwrap();
        barrier(&worker);
        let invalid_events = worker.events.try_iter().collect::<Vec<_>>();
        assert!(
            invalid_events
                .iter()
                .any(|event| matches!(event, ProjectEvent::Error(_)))
        );
        assert!(
            !invalid_events
                .iter()
                .any(|event| matches!(event, ProjectEvent::External(_)))
        );
        fs::write(&project_path, valid).unwrap();
        let mut project = store.load_project().unwrap();
        project.name = "External".into();
        store.save_project(&project).unwrap();
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(ProjectCommand::PollNow)
            .unwrap();
        barrier(&worker);
        assert!(worker.events.try_iter().any(
            |event| matches!(event, ProjectEvent::External(project) if project.name == "External")
        ));
        worker.close(false);
    }

    #[test]
    fn stale_compile_completions_are_rejected_by_revision() {
        let (_directory, store) = store();
        let mut state = CompileState::default();
        for revision in 1..=128 {
            state.pending = Some(CompileJob {
                revision,
                store: store.clone(),
            });
        }
        assert_eq!(state.pending.as_ref().unwrap().revision, 128);
        assert!(!completion_is_current(127, 128));
        assert!(completion_is_current(128, 128));
    }

    #[test]
    fn beat_mapping_supports_seek_and_loop_wrap_commands() {
        assert_eq!(beat_to_frame(4.0, 120.0, 48_000), 96_000);
        assert_eq!(beat_to_frame(f32::NAN, 120.0, 48_000), 0);
    }
}
