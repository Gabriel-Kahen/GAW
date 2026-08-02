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
    ChannelLayout, CommandSender, CompiledProject, CpalOutput, DeviceRecoveryAction,
    DeviceRecoveryController, DeviceRecoveryPolicy, OutputDeviceInfo, OutputDeviceSelection,
    PreparedPage, RealtimeCommand, RealtimeEngineConfig, RealtimeLoopRange, RecoveryTarget,
    RenderSnapshot, StreamGeneration, StreamNotificationReceiver, StreamNotificationSender,
    command_queue, compile_project_store, enumerate_output_devices, stream_notification_channel,
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
const AUDIO_PREPARE_LEAD_PAGES: u64 = 8;
const DEVICE_RETRY: Duration = Duration::from_millis(500);

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
    Close(std::sync::mpsc::SyncSender<Result<(), ControllerError>>),
    Abandon,
}

#[derive(Debug)]
enum ProjectEvent {
    Journaled(u64),
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

    fn close(&mut self, clean: bool) -> Result<(), ControllerError> {
        let mut result = Ok(());
        if let Some(sender) = self.sender.take() {
            if clean {
                let (done, completed) = std::sync::mpsc::sync_channel(0);
                if sender.send(ProjectCommand::Close(done)).is_err() {
                    result = Err(ControllerError::new(
                        "persistence",
                        "project worker disconnected before clean close",
                    ));
                } else {
                    result = completed.recv().unwrap_or_else(|_| {
                        Err(ControllerError::new(
                            "persistence",
                            "project worker dropped its clean-close result",
                        ))
                    });
                }
            } else {
                let _ = sender.send(ProjectCommand::Abandon);
            }
        }
        if let Some(join) = self.join.take()
            && join.join().is_err()
            && result.is_ok()
        {
            result = Err(ControllerError::new(
                "persistence",
                "project worker panicked during close",
            ));
        }
        result
    }
}

impl Drop for ProjectWorker {
    fn drop(&mut self) {
        let _ = self.close(false);
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
                    "persistence is paused; use Save to retry the latest atomic snapshot",
                ),
                ProjectCommand::Apply {
                    revision: next,
                    transaction,
                } => match session.apply_transaction(&transaction) {
                    Ok(()) => {
                        dirty = true;
                        revision = revision.max(next);
                        let _ = events.try_send(ProjectEvent::Journaled(next));
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
                        Err(error) => {
                            failed = true;
                            send_error(events, "persistence", error);
                        }
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
                ProjectCommand::Close(done) => {
                    let close_result = session
                        .close()
                        .map_err(|error| ControllerError::new("persistence", error));
                    let result = if failed && close_result.is_ok() {
                        Err(ControllerError::new(
                            "persistence",
                            "one or more accepted edits could not be persisted before close",
                        ))
                    } else {
                        close_result
                    };
                    let _ = done.send(result);
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
    focus_frame: u64,
    secondary_frame: Option<u64>,
}

#[derive(Debug)]
struct CompileResult {
    revision: u64,
    window: PageWindow,
    result: Result<Arc<RenderSnapshot>, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PageWindow {
    start_frame: u64,
    end_frame: u64,
    secondary_start_frame: u64,
    secondary_end_frame: u64,
    total_frames: u64,
}

impl PageWindow {
    fn contains(self, frame: u64) -> bool {
        (self.start_frame..self.end_frame).contains(&frame)
            || (self.secondary_start_frame..self.secondary_end_frame).contains(&frame)
    }
}

#[derive(Debug)]
struct PreparedProject {
    revision: u64,
    compiled: CompiledProject,
    pages: Vec<PreparedPage>,
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

    fn request(
        &self,
        revision: u64,
        store: ProjectStore,
        focus_frame: u64,
        secondary_frame: Option<u64>,
    ) {
        let (lock, ready) = &*self.state;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = Some(CompileJob {
            revision,
            store,
            focus_frame,
            secondary_frame,
        });
        if state
            .completed
            .as_ref()
            .is_some_and(|completed| completed.revision != revision)
        {
            state.completed = None;
        }
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
    let mut prepared = None;
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
        let result = prepare_snapshot_window(state, &job, &mut prepared);
        let Some((window, result)) = result else {
            continue;
        };
        let mut value = state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !request_supersedes(value.pending.as_ref(), &job) {
            value.completed = Some(CompileResult {
                revision: job.revision,
                window,
                result: result.map(Arc::new),
            });
        }
    }
}

fn prepare_snapshot_window(
    state: &Arc<(Mutex<CompileState>, Condvar)>,
    job: &CompileJob,
    prepared: &mut Option<PreparedProject>,
) -> Option<(PageWindow, Result<RenderSnapshot, String>)> {
    if prepared.as_ref().map(|value| value.revision) != Some(job.revision) {
        let compiled = match compile_project_store(&job.store) {
            Ok(compiled) => compiled,
            Err(error) => {
                return Some((
                    PageWindow {
                        start_frame: 0,
                        end_frame: 0,
                        secondary_start_frame: 0,
                        secondary_end_frame: 0,
                        total_frames: 0,
                    },
                    Err(error.to_string()),
                ));
            }
        };
        *prepared = Some(PreparedProject {
            revision: job.revision,
            compiled,
            pages: Vec::new(),
        });
    }
    let value = prepared.as_mut().expect("project was prepared");
    let root = value.compiled.plan().root();
    let channels = root.output_layout.channels();
    let total = root.length_frames.saturating_add(root.tail_frames);
    let window = page_window(total, channels, job.focus_frame, job.secondary_frame);
    value
        .pages
        .retain(|page| window.contains(page.start_frame()));
    for (range_start, range_end) in [
        (window.start_frame, window.end_frame),
        (window.secondary_start_frame, window.secondary_end_frame),
    ] {
        let mut start = range_start;
        while start < range_end {
            if value.pages.iter().all(|page| page.start_frame() != start) {
                let frames = usize::try_from(range_end.saturating_sub(start))
                    .unwrap_or(usize::MAX)
                    .min(AUDIO_PAGE_FRAMES);
                match value.compiled.prepare_page(start, frames) {
                    Ok(page) => value.pages.push(page),
                    Err(error) => return Some((window, Err(error.to_string()))),
                }
            }
            if compile_request_is_superseded(state, job) {
                return None;
            }
            let frames = usize::try_from(total.saturating_sub(start))
                .unwrap_or(usize::MAX)
                .min(AUDIO_PAGE_FRAMES);
            start = start.saturating_add(frames as u64);
        }
    }
    value.pages.sort_by_key(PreparedPage::start_frame);
    let result = value
        .compiled
        .paged_snapshot(value.pages.iter().cloned())
        .map_err(|error| error.to_string());
    Some((window, result))
}

fn compile_request_is_superseded(
    state: &Arc<(Mutex<CompileState>, Condvar)>,
    job: &CompileJob,
) -> bool {
    let value = state
        .0
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    value.closed || request_supersedes(value.pending.as_ref(), job)
}

fn request_supersedes(pending: Option<&CompileJob>, active: &CompileJob) -> bool {
    pending.is_some_and(|pending| {
        pending.revision != active.revision
            || pending.focus_frame != active.focus_frame
            || pending.secondary_frame != active.secondary_frame
    })
}

fn page_window(
    total_frames: u64,
    channels: usize,
    focus_frame: u64,
    secondary_frame: Option<u64>,
) -> PageWindow {
    if total_frames == 0 {
        return PageWindow {
            start_frame: 0,
            end_frame: 0,
            secondary_start_frame: 0,
            secondary_end_frame: 0,
            total_frames,
        };
    }
    let bytes_per_page = AUDIO_PAGE_FRAMES
        .saturating_mul(channels)
        .saturating_mul(size_of::<f32>());
    let maximum_pages =
        u64::try_from((AUDIO_PAGE_BYTES / bytes_per_page).max(1)).unwrap_or(u64::MAX);
    let page_frames = AUDIO_PAGE_FRAMES as u64;
    let total_pages = total_frames.div_ceil(page_frames);
    let focus_page = focus_frame.min(total_frames - 1) / page_frames;
    let secondary_page = secondary_frame.map(|frame| frame.min(total_frames - 1) / page_frames);
    let full_start = focus_page
        .saturating_sub(maximum_pages / 4)
        .min(total_pages.saturating_sub(maximum_pages));
    let full_end = full_start.saturating_add(maximum_pages).min(total_pages);
    let secondary_pages = secondary_page
        .filter(|page| !(full_start..full_end).contains(page))
        .map_or(0, |_| maximum_pages.min(4));
    let primary_pages = maximum_pages.saturating_sub(secondary_pages).max(1);
    let maximum_start = total_pages.saturating_sub(primary_pages);
    let start_page = focus_page
        .saturating_sub(primary_pages / 4)
        .min(maximum_start);
    let end_page = start_page.saturating_add(primary_pages).min(total_pages);
    let (secondary_start_page, secondary_end_page) = secondary_page.map_or((0, 0), |page| {
        if secondary_pages == 0 {
            (0, 0)
        } else {
            let start = page
                .saturating_sub(1)
                .min(total_pages.saturating_sub(secondary_pages));
            (
                start,
                start.saturating_add(secondary_pages).min(total_pages),
            )
        }
    });
    PageWindow {
        start_frame: start_page.saturating_mul(page_frames),
        end_frame: end_page.saturating_mul(page_frames).min(total_frames),
        secondary_start_frame: secondary_start_page.saturating_mul(page_frames),
        secondary_end_frame: secondary_end_page
            .saturating_mul(page_frames)
            .min(total_frames),
        total_frames,
    }
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
    fn open(
        sample_rate: u32,
        generation: StreamGeneration,
        target: Option<&RecoveryTarget>,
        notifications: &StreamNotificationSender,
    ) -> Result<(Self, OutputDeviceInfo), String> {
        let config = RealtimeEngineConfig {
            sample_rate,
            output_layout: ChannelLayout::Stereo,
            maximum_block_frames: 8_192,
            maximum_commands_per_block: 64,
        };
        let (commands, engine) = command_queue(config, 128, 8).map_err(|e| e.to_string())?;
        let callback = notifications.callback(generation);
        let device = match target {
            None => CpalOutput::open_default_negotiated(engine, callback),
            Some(RecoveryTarget::Default { backend }) => {
                CpalOutput::open_default_on_backend(*backend, engine, true, callback)
            }
            Some(RecoveryTarget::Device { device_id }) => {
                CpalOutput::open_device(device_id, engine, true, callback)
            }
        }
        .map_err(|error| error.to_string())?;
        let info = device.info();
        let devices = enumerate_output_devices(info.backend).map_err(|error| error.to_string())?;
        let selected = match target {
            Some(RecoveryTarget::Device { device_id }) => {
                devices.into_iter().find(|device| &device.id == device_id)
            }
            None | Some(RecoveryTarget::Default { .. }) => {
                devices.into_iter().find(|device| device.is_default)
            }
        }
        .ok_or_else(|| "opened output device disappeared during enumeration".to_owned())?;
        device.play().map_err(|error| error.to_string())?;
        Ok((
            Self {
                commands,
                _device: device,
            },
            selected,
        ))
    }
}

#[derive(Clone, Debug)]
struct DeviceOpenJob {
    sample_rate: u32,
    generation: StreamGeneration,
    target: Option<RecoveryTarget>,
    notifications: StreamNotificationSender,
}

#[derive(Debug)]
struct DeviceOpenResult {
    generation: StreamGeneration,
    target: Option<RecoveryTarget>,
    result: Result<(AudioOutput, OutputDeviceInfo), String>,
}

#[derive(Debug)]
struct DeviceWorker {
    requests: Option<Sender<DeviceOpenJob>>,
    results: Receiver<DeviceOpenResult>,
    join: Option<JoinHandle<()>>,
}

impl DeviceWorker {
    fn spawn() -> Self {
        let (requests, receiver) = bounded::<DeviceOpenJob>(1);
        let (sender, results) = bounded::<DeviceOpenResult>(1);
        let join = thread::Builder::new()
            .name("gaw-device-controller".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    let result = AudioOutput::open(
                        job.sample_rate,
                        job.generation,
                        job.target.as_ref(),
                        &job.notifications,
                    );
                    if sender
                        .send(DeviceOpenResult {
                            generation: job.generation,
                            target: job.target,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("device controller thread should start");
        Self {
            requests: Some(requests),
            results,
            join: Some(join),
        }
    }

    fn request(&self, job: DeviceOpenJob) -> bool {
        self.requests
            .as_ref()
            .is_some_and(|sender| sender.try_send(job).is_ok())
    }
}

impl Drop for DeviceWorker {
    fn drop(&mut self) {
        self.requests.take();
        self.join.take();
    }
}

#[derive(Debug)]
pub(crate) struct NativeController {
    store: ProjectStore,
    project: ProjectWorker,
    compiler: CompileWorker,
    devices: DeviceWorker,
    audio: Option<AudioOutput>,
    notifications: StreamNotificationSender,
    notification_events: StreamNotificationReceiver,
    recovery: Option<DeviceRecoveryController>,
    device_opening: bool,
    next_device_open: Instant,
    next_generation: u64,
    device_clock: Instant,
    sample_rate: u32,
    latest_snapshot: Option<Arc<RenderSnapshot>>,
    pending_project: VecDeque<ProjectCommand>,
    deferred_project: Option<(u64, Project)>,
    pending_audio: VecDeque<RealtimeCommand>,
    latest_revision: u64,
    submitted_revision: u64,
    resident_window: Option<(u64, PageWindow)>,
    requested_window: Option<(u64, u64)>,
    telemetry_seek: Option<u64>,
    last_transport: TransportView,
    notice: Option<String>,
    error: Option<ControllerError>,
    closed: bool,
}

impl NativeController {
    pub(crate) fn start(startup: NativeStartup) -> Self {
        let store = startup.session.store().clone();
        let sample_rate = startup.project.sample_rate.value();
        let (notifications, notification_events) =
            stream_notification_channel(8).expect("nonzero device notification capacity");
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
        let mut controller = Self {
            store,
            project: ProjectWorker::spawn(startup.session),
            compiler: CompileWorker::spawn(),
            devices: DeviceWorker::spawn(),
            audio: None,
            notifications,
            notification_events,
            recovery: None,
            device_opening: false,
            next_device_open: Instant::now(),
            next_generation: 0,
            device_clock: Instant::now(),
            sample_rate,
            latest_snapshot: None,
            pending_project: VecDeque::new(),
            deferred_project: None,
            pending_audio: VecDeque::new(),
            latest_revision: 0,
            submitted_revision: 0,
            resident_window: None,
            requested_window: None,
            telemetry_seek: None,
            last_transport,
            notice,
            error: None,
            closed: false,
        };
        controller.request_bootstrap_device();
        controller
    }

    pub(crate) fn initialize_transport(&mut self, transport: &Transport) {
        self.last_transport = transport.into();
    }

    pub(crate) fn pump(&mut self, vm: &mut DemoViewModel, now: f64) {
        self.flush_project();
        loop {
            match self.project.events.try_recv() {
                Ok(ProjectEvent::Journaled(revision)) => {
                    self.notice = Some(format!("Edit journaled · r{revision}"));
                }
                Ok(ProjectEvent::CanonicalReady(revision)) => {
                    if revision == self.latest_revision {
                        self.request_audio_window(revision, transport_frame(vm), loop_anchor(vm));
                        set_render_state(vm, RenderState::Rendering(0));
                    }
                }
                Ok(ProjectEvent::External(project)) => {
                    let changed = changed_ids(&project);
                    match vm.replace_project_from_agent(project, changed, now) {
                        Ok(()) => {
                            self.latest_revision = vm.revision();
                            self.resident_window = None;
                            self.request_audio_window(
                                self.latest_revision,
                                transport_frame(vm),
                                loop_anchor(vm),
                            );
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

        self.accept_updates(vm);

        if let Some(completed) = self.compiler.take_completed()
            && completion_is_current(completed.revision, self.latest_revision)
        {
            match completed.result {
                Ok(snapshot) => {
                    self.requested_window = None;
                    self.resident_window = Some((completed.revision, completed.window));
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
        self.pump_device(vm);
        self.sync_transport(vm);
        self.schedule_audio_pages(vm);
        self.flush_audio();
        if let Some(audio) = &self.audio {
            audio.commands.reclaim_retired();
        }
        self.sync_callback_playhead(vm);
    }

    pub(crate) fn save(&mut self, revision: u64, project: Project) {
        if !self.enqueue_project(ProjectCommand::Save { revision, project }) {
            self.set_error("persistence", "bounded save backlog is full");
        }
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

    pub(crate) fn close(&mut self, vm: &mut DemoViewModel) {
        if self.closed {
            return;
        }
        self.accept_updates(vm);
        while let Some(command) = self.pending_project.pop_front() {
            let Some(sender) = &self.project.sender else {
                break;
            };
            if sender.send(command).is_err() {
                break;
            }
        }
        if let Some((revision, project)) = self.deferred_project.take()
            && let Some(sender) = &self.project.sender
        {
            let _ = sender.send(ProjectCommand::ReplaceSnapshot { revision, project });
        }
        if let Err(error) = self.project.close(true) {
            self.record_error(error);
        }
        self.closed = true;
    }

    fn enqueue_project(&mut self, command: ProjectCommand) -> bool {
        if self.pending_project.len() >= PROJECT_QUEUE * 4 {
            return false;
        }
        self.pending_project.push_back(command);
        self.flush_project();
        true
    }

    fn flush_project(&mut self) {
        while let Some(command) = self.pending_project.pop_front() {
            if let Err(command) = self.project.try_send(command) {
                self.pending_project.push_front(command);
                break;
            }
        }
        if self.pending_project.is_empty()
            && let Some((revision, project)) = self.deferred_project.take()
            && let Err(ProjectCommand::ReplaceSnapshot { project, .. }) = self
                .project
                .try_send(ProjectCommand::ReplaceSnapshot { revision, project })
        {
            self.deferred_project = Some((revision, project));
        }
    }

    fn enqueue_audio(&mut self, command: RealtimeCommand) {
        if let RealtimeCommand::InstallSnapshot(snapshot) = &command {
            self.latest_snapshot = Some(Arc::clone(snapshot));
        }
        if matches!(command, RealtimeCommand::InstallSnapshot(_))
            && let Some(index) = self
                .pending_audio
                .iter()
                .rposition(|pending| matches!(pending, RealtimeCommand::InstallSnapshot(_)))
        {
            self.pending_audio.remove(index);
        }
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

    fn request_bootstrap_device(&mut self) {
        if self.device_opening || Instant::now() < self.next_device_open {
            return;
        }
        let generation = StreamGeneration::new(self.next_generation);
        self.next_generation = self.next_generation.saturating_add(1);
        self.request_device(generation, None);
    }

    fn request_device(&mut self, generation: StreamGeneration, target: Option<RecoveryTarget>) {
        self.device_opening = self.devices.request(DeviceOpenJob {
            sample_rate: self.sample_rate,
            generation,
            target,
            notifications: self.notifications.clone(),
        });
    }

    fn pump_device(&mut self, vm: &DemoViewModel) {
        if let Ok(completed) = self.devices.results.try_recv() {
            self.device_opening = false;
            match completed.result {
                Ok((audio, selected)) => {
                    if let Some(recovery) = &mut self.recovery {
                        let _ = recovery.stream_started(completed.generation, selected.id.clone());
                    } else {
                        self.recovery = DeviceRecoveryController::new(
                            OutputDeviceSelection::FollowDefault {
                                backend: selected.backend,
                            },
                            DeviceRecoveryPolicy::default(),
                            completed.generation,
                            selected.id.clone(),
                        )
                        .ok();
                    }
                    self.audio = Some(audio);
                    self.restore_audio(vm);
                    self.notice = Some(format!("Audio device ready · {}", selected.name));
                    if self
                        .error
                        .as_ref()
                        .is_some_and(|error| error.subsystem == "audio device")
                    {
                        self.error = None;
                    }
                }
                Err(error) => {
                    self.set_error("audio device", error);
                    if completed.target.is_some() {
                        let now = self.device_millis();
                        if let Some(recovery) = &mut self.recovery {
                            let action = recovery.open_failed(completed.generation, now);
                            self.handle_device_action(action);
                        }
                    } else {
                        self.next_device_open = Instant::now() + DEVICE_RETRY;
                    }
                }
            }
        }
        while let Ok(notification) = self.notification_events.try_recv() {
            if let Some(recovery) = &mut self.recovery {
                let action = recovery.handle_notification(&notification);
                self.handle_device_action(action);
            }
        }
        let now = self.device_millis();
        if let Some(recovery) = &mut self.recovery {
            let action = recovery.poll(now);
            self.handle_device_action(action);
        } else {
            self.request_bootstrap_device();
        }
    }

    fn handle_device_action(&mut self, action: DeviceRecoveryAction) {
        match action {
            DeviceRecoveryAction::Open {
                generation, target, ..
            } => {
                self.audio = None;
                self.request_device(generation, Some(target));
            }
            DeviceRecoveryAction::Exhausted { attempts, .. } => {
                self.audio = None;
                self.recovery = None;
                self.next_device_open = Instant::now() + DEVICE_RETRY;
                self.set_error(
                    "audio device",
                    format!("device recovery exhausted after {attempts} attempt(s); retrying"),
                );
            }
            DeviceRecoveryAction::None
            | DeviceRecoveryAction::Continue
            | DeviceRecoveryAction::WaitUntil { .. }
            | DeviceRecoveryAction::StaleNotification { .. } => {}
        }
    }

    fn restore_audio(&mut self, vm: &DemoViewModel) {
        self.pending_audio.clear();
        if let Some(snapshot) = &self.latest_snapshot {
            self.pending_audio
                .push_back(RealtimeCommand::InstallSnapshot(Arc::clone(snapshot)));
        }
        self.pending_audio
            .push_back(RealtimeCommand::SetLoop(realtime_loop(vm)));
        let frame = transport_frame(vm);
        self.telemetry_seek = Some(frame);
        self.pending_audio.push_back(RealtimeCommand::Seek(frame));
        if vm.transport.playing {
            self.pending_audio.push_back(RealtimeCommand::Play);
        }
        self.flush_audio();
    }

    fn device_millis(&self) -> u64 {
        u64::try_from(self.device_clock.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn sync_transport(&mut self, vm: &DemoViewModel) {
        let current = TransportView::from(&vm.transport);
        if current.loop_enabled != self.last_transport.loop_enabled
            || (current.loop_start - self.last_transport.loop_start).abs() > f32::EPSILON
            || (current.loop_end - self.last_transport.loop_end).abs() > f32::EPSILON
            || (current.bpm - self.last_transport.bpm).abs() > f32::EPSILON
        {
            self.enqueue_audio(RealtimeCommand::SetLoop(realtime_loop(vm)));
        }
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
            let frame = beat_to_frame(
                current.playhead,
                current.bpm,
                vm.project().sample_rate.value(),
            );
            self.telemetry_seek = Some(frame);
            self.enqueue_audio(RealtimeCommand::Seek(frame));
        }
        self.last_transport = current;
    }

    fn sync_callback_playhead(&mut self, vm: &mut DemoViewModel) {
        let Some(audio) = &self.audio else { return };
        let frame = audio.commands.frame_position();
        if let Some(target) = self.telemetry_seek {
            if frame.abs_diff(target) > 8_192 {
                return;
            }
            self.telemetry_seek = None;
        }
        let beat = frame_to_beat(frame, vm.transport.bpm, vm.project().sample_rate.value());
        vm.transport.playhead = beat.min(vm.current_composition().length_beats);
        self.last_transport.playhead = vm.transport.playhead;
    }

    fn accept_updates(&mut self, vm: &mut DemoViewModel) {
        let updates = vm.take_updates().collect::<Vec<_>>();
        if updates
            .first()
            .is_some_and(|update| update.revision > self.submitted_revision.saturating_add(1))
        {
            self.defer_project(vm.revision(), vm.project().clone());
            set_render_state(vm, RenderState::Stale);
            return;
        }
        let mut coalescing = self.deferred_project.is_some();
        for update in updates {
            self.latest_revision = self.latest_revision.max(update.revision);
            self.submitted_revision = self.submitted_revision.max(update.revision);
            if coalescing {
                self.defer_project(update.revision, vm.project().clone());
                continue;
            }
            match (update.source, update.transaction) {
                (ChangeSource::Ui, Some(transaction)) => {
                    if !self.enqueue_project(ProjectCommand::Apply {
                        revision: update.revision,
                        transaction,
                    }) {
                        self.defer_project(update.revision, vm.project().clone());
                        coalescing = true;
                    }
                    set_render_state(vm, RenderState::Stale);
                }
                (ChangeSource::Undo | ChangeSource::Redo, None) => {
                    if !self.enqueue_project(ProjectCommand::ReplaceSnapshot {
                        revision: update.revision,
                        project: vm.project().clone(),
                    }) {
                        self.defer_project(update.revision, vm.project().clone());
                        coalescing = true;
                    }
                    set_render_state(vm, RenderState::Stale);
                }
                _ => {}
            }
        }
    }

    fn defer_project(&mut self, revision: u64, project: Project) {
        self.latest_revision = self.latest_revision.max(revision);
        self.submitted_revision = self.submitted_revision.max(revision);
        self.deferred_project = Some((revision, project));
        self.flush_project();
    }

    fn request_audio_window(
        &mut self,
        revision: u64,
        focus_frame: u64,
        secondary_frame: Option<u64>,
    ) {
        self.compiler
            .request(revision, self.store.clone(), focus_frame, secondary_frame);
        self.requested_window = Some((revision, focus_frame));
    }

    fn schedule_audio_pages(&mut self, vm: &DemoViewModel) {
        if self
            .requested_window
            .is_some_and(|(revision, _)| revision == self.latest_revision)
        {
            return;
        }
        let frame = transport_frame(vm);
        let lead = AUDIO_PREPARE_LEAD_PAGES.saturating_mul(AUDIO_PAGE_FRAMES as u64);
        let target = if vm.transport.playing {
            frame.saturating_add(lead)
        } else {
            frame
        };
        let needs_window = self.resident_window.is_none_or(|(revision, window)| {
            revision != self.latest_revision
                || !window.contains(frame)
                || (vm.transport.playing
                    && window.end_frame < window.total_frames
                    && window.end_frame.saturating_sub(frame) <= lead)
        });
        if needs_window {
            self.request_audio_window(self.latest_revision, target, loop_anchor(vm));
        }
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
            let _ = self.project.close(false);
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

fn transport_frame(vm: &DemoViewModel) -> u64 {
    beat_to_frame(
        vm.transport.playhead,
        vm.transport.bpm,
        vm.project().sample_rate.value(),
    )
}

fn frame_to_beat(frame: u64, bpm: f32, sample_rate: u32) -> f32 {
    if !(bpm.is_finite() && bpm > 0.0) || sample_rate == 0 {
        return 0.0;
    }
    (frame as f64 * f64::from(bpm) / 60.0 / f64::from(sample_rate)) as f32
}

fn loop_anchor(vm: &DemoViewModel) -> Option<u64> {
    (vm.transport.playing && vm.transport.loop_enabled).then(|| {
        beat_to_frame(
            vm.transport.loop_start,
            vm.transport.bpm,
            vm.project().sample_rate.value(),
        )
    })
}

fn realtime_loop(vm: &DemoViewModel) -> Option<RealtimeLoopRange> {
    vm.transport.loop_enabled.then(|| {
        RealtimeLoopRange::new(
            beat_to_frame(
                vm.transport.loop_start,
                vm.transport.bpm,
                vm.project().sample_rate.value(),
            ),
            beat_to_frame(
                vm.transport.loop_end,
                vm.transport.bpm,
                vm.project().sample_rate.value(),
            ),
        )
        .ok()
    })?
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
        worker.close(true).unwrap();
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
        worker.close(false).unwrap();
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
        worker.close(false).unwrap();
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
        worker.close(false).unwrap();
    }

    #[test]
    fn stale_compile_completions_are_rejected_by_revision() {
        let (_directory, store) = store();
        let mut state = CompileState::default();
        for revision in 1..=128 {
            state.pending = Some(CompileJob {
                revision,
                store: store.clone(),
                focus_frame: revision * AUDIO_PAGE_FRAMES as u64,
                secondary_frame: None,
            });
        }
        assert_eq!(state.pending.as_ref().unwrap().revision, 128);
        assert!(!completion_is_current(127, 128));
        assert!(completion_is_current(128, 128));
    }

    #[test]
    fn page_windows_move_beyond_initial_budget_and_stay_bounded() {
        let page = AUDIO_PAGE_FRAMES as u64;
        let total = page * 200 + 17;
        let focus = page * 140;
        let window = page_window(total, 2, focus, None);
        let bytes_per_page = AUDIO_PAGE_FRAMES * 2 * size_of::<f32>();
        let pages = window.end_frame.div_ceil(page) - window.start_frame / page;
        assert!(window.contains(focus));
        assert!(window.start_frame > 0);
        assert!(pages as usize * bytes_per_page <= AUDIO_PAGE_BYTES);

        let end = page_window(total, 2, u64::MAX, None);
        assert!(end.contains(total - 1));
        assert_eq!(end.end_frame, total);
        assert_eq!(page_window(0, 2, u64::MAX, None).end_frame, 0);
    }

    #[test]
    fn distant_loop_start_is_reserved_inside_the_same_page_budget() {
        let page = AUDIO_PAGE_FRAMES as u64;
        let total = page * 200;
        let focus = page * 150;
        let loop_start = page * 2;
        let window = page_window(total, 2, focus, Some(loop_start));
        let primary = (window.end_frame - window.start_frame).div_ceil(page);
        let secondary = (window.secondary_end_frame - window.secondary_start_frame).div_ceil(page);
        assert!(window.contains(focus));
        assert!(window.contains(loop_start));
        assert!(primary + secondary <= 64);
    }

    #[test]
    fn journal_ack_follows_the_durable_append() {
        let (_directory, store) = store();
        let mut worker = ProjectWorker::spawn(ProjectSession::open(store.clone()).unwrap());
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(ProjectCommand::Apply {
                revision: 7,
                transaction: Arc::new(Transaction::new([Command::SetProjectName {
                    name: "Journaled".into(),
                }])),
            })
            .unwrap();
        barrier(&worker);
        assert_eq!(store.pending_recovery().unwrap().len(), 1);
        assert!(
            worker
                .events
                .try_iter()
                .any(|event| matches!(event, ProjectEvent::Journaled(7)))
        );
        worker.close(true).unwrap();
    }

    #[test]
    fn clean_close_ack_is_independent_of_a_full_event_queue() {
        let (_directory, store) = store();
        let mut worker = ProjectWorker::spawn(ProjectSession::open(store.clone()).unwrap());
        for revision in 1..=PROJECT_EVENTS as u64 + 8 {
            worker
                .sender
                .as_ref()
                .unwrap()
                .send(ProjectCommand::Apply {
                    revision,
                    transaction: Arc::new(Transaction::new([Command::SetProjectName {
                        name: format!("Edit {revision}"),
                    }])),
                })
                .unwrap();
        }
        worker.close(true).unwrap();
        assert_eq!(store.load_project().unwrap().name, "Edit 40");
        assert!(store.pending_recovery().unwrap().is_empty());
    }

    #[test]
    fn controller_close_drains_updates_not_seen_by_a_pump() {
        let (_directory, store) = store();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        let mut controller = NativeController::start(startup);
        vm.apply(Intent::SetBpm(91.0));
        controller.close(&mut vm);
        assert!((store.load_project().unwrap().bpm.value() - 91.0).abs() < f64::EPSILON);
        assert!(store.pending_recovery().unwrap().is_empty());
    }

    #[test]
    fn revision_gap_coalesces_to_the_latest_bounded_snapshot_on_close() {
        let (_directory, store) = store();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        let mut controller = NativeController::start(startup);
        for index in 0..300 {
            vm.apply(Intent::SetBpm(80.0 + (index % 100) as f32));
        }
        controller.close(&mut vm);
        assert!((store.load_project().unwrap().bpm.value() - 179.0).abs() < f64::EPSILON);
        assert!(store.pending_recovery().unwrap().is_empty());
    }

    #[test]
    fn beat_mapping_supports_seek_and_loop_wrap_commands() {
        assert_eq!(beat_to_frame(4.0, 120.0, 48_000), 96_000);
        assert_eq!(beat_to_frame(f32::NAN, 120.0, 48_000), 0);
        assert!((frame_to_beat(96_000, 120.0, 48_000) - 4.0).abs() < f32::EPSILON);
    }
}
