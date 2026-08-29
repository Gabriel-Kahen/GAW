#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]

use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::DefaultHasher},
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{
    Receiver, RecvTimeoutError, Sender, TryRecvError, TrySendError, bounded, unbounded,
};
use gaw_audio::{
    AssetRevision, ChannelLayout, CommandSender, CompiledProject, CpalOutput, DependencyRevision,
    DeviceObservation, DeviceRecoveryAction, DeviceRecoveryController, DeviceRecoveryPolicy,
    FrameSource, OpenedOutputDeviceInfo, OutputDeviceSelection, PreparedPage, RealtimeCommand,
    RealtimeEngineConfig, RealtimeLoopRange, RealtimeMetronome, RecoveryTarget, RenderContext,
    RenderSnapshot, StorePlaybackCompiler, StreamGeneration, StreamNotificationReceiver,
    StreamNotificationSender, TimelineActivation, WavFrameSource, Waveform, command_queue,
    load_wav_memory_snapshot, observe_output_devices, stream_notification_channel,
    track_peak_sidecar_bytes,
};
use gaw_core::{AssetId, Command, CompositionId, Project, Transaction};
use gaw_project::{MediaRegion, ProjectSession, ProjectStore};

use crate::clip_export::{ClipExportJob, export_clip_mp3 as run_clip_export};
use crate::model::{ChangeSource, DemoViewModel, RenderState, Transport, WaveformPoint};
use crate::stem_splitter::{
    StemSplitJob, StemSplitOptions, StemSplitOutput, StemSplitResult, split as split_stems,
};
use crate::transcription::{TranscriptionJob, TranscriptionResult, transcribe};

const PROJECT_QUEUE: usize = 64;
const WATCH_INTERVAL: Duration = Duration::from_millis(150);
const WATCH_MAX_ENTRIES: usize = 4_096;
const WATCH_MAX_DEPTH: usize = 12;
const AUDIO_PAGE_FRAMES: usize = 65_536;
const AUDIO_PAGE_BYTES: usize = 32 * 1024 * 1024;
const AUDIO_PREPARE_LEAD_PAGES: u64 = 8;
const AUDIO_COMPILE_RETRY: Duration = Duration::from_millis(250);
const DEVICE_RETRY: Duration = Duration::from_millis(500);
const DEVICE_OBSERVE_INTERVAL: Duration = Duration::from_millis(250);
const DEVICE_NOTIFICATION_CAPACITY: usize = 8;
const WAVEFORM_BASE_BUCKET_FRAMES: u64 = 128;
const WAVEFORM_MAX_BUCKETS: u64 = 262_144;
const TRANSCRIPTION_QUEUE: usize = 4;
const STEM_SPLIT_QUEUE: usize = 2;

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
    ImportMedia {
        revision: u64,
        source: PathBuf,
    },
    SplitImportedMedia {
        revision: u64,
        asset_id: AssetId,
        regions: Vec<MediaRegion>,
    },
    ImportStemSplit {
        revision: u64,
        asset_id: AssetId,
        expected_content_hash: String,
        output: StemSplitOutput,
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
    Imported {
        revision: u64,
        transaction: Arc<Transaction>,
        project: Project,
        asset_id: AssetId,
        original_filename: String,
    },
    MediaSplit {
        revision: u64,
        transaction: Arc<Transaction>,
        project: Project,
        asset_ids: Vec<AssetId>,
    },
    StemSplitImported {
        revision: u64,
        source_asset_id: AssetId,
        transaction: Arc<Transaction>,
        project: Project,
        asset_ids: Vec<AssetId>,
        folder_name: String,
    },
    StemSplitImportFailed {
        asset_id: AssetId,
        error: ControllerError,
    },
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
        let (event_sender, events) = unbounded();
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
                ProjectCommand::ImportMedia {
                    revision: next,
                    source,
                } => {
                    let result = (|| -> gaw_project::Result<_> {
                        session.checkpoint()?;
                        let before = session.project().clone();
                        let imported = store.import_media(source)?;
                        let project = store.load_project()?;
                        let asset = project
                            .assets
                            .iter()
                            .find(|asset| asset.id == imported.asset_id)
                            .cloned()
                            .ok_or_else(|| {
                                gaw_project::Error::InvalidTransaction(
                                    "imported asset is missing after commit".into(),
                                )
                            })?;
                        let command = if before
                            .assets
                            .iter()
                            .any(|candidate| candidate.id == imported.asset_id)
                        {
                            Command::UpdateAsset { asset }
                        } else {
                            Command::AddAsset { asset }
                        };
                        let transaction = Transaction::named(
                            format!("Import {}", imported.original_filename),
                            [command],
                        );
                        let mut expected = before;
                        transaction.apply(&mut expected).map_err(|error| {
                            gaw_project::Error::InvalidTransaction(error.to_string())
                        })?;
                        if expected != project {
                            return Err(gaw_project::Error::InvalidTransaction(
                                "project changed concurrently during media import".into(),
                            ));
                        }
                        let next_session = ProjectSession::open(store.clone())?;
                        Ok((imported, Arc::new(transaction), project, next_session))
                    })();
                    match result {
                        Ok((imported, transaction, project, next_session)) => {
                            dirty = false;
                            failed = false;
                            revision = revision.max(next);
                            let event = ProjectEvent::Imported {
                                revision: next,
                                transaction,
                                project,
                                asset_id: imported.asset_id,
                                original_filename: imported.original_filename,
                            };
                            if events.send_timeout(event, WATCH_INTERVAL).is_ok() {
                                session = next_session;
                                baseline = project_fingerprint(store.root()).ok();
                            }
                        }
                        Err(error) => send_error(events, "asset import", error),
                    }
                }
                ProjectCommand::SplitImportedMedia {
                    revision: next,
                    asset_id,
                    regions,
                } => {
                    let result = (|| -> gaw_project::Result<_> {
                        session.checkpoint()?;
                        let before = session.project().clone();
                        let created = store.split_imported_media(asset_id, &regions)?;
                        let project = store.load_project()?;
                        let asset_ids = created
                            .iter()
                            .map(|imported| imported.asset_id)
                            .collect::<Vec<_>>();
                        let commands = asset_ids
                            .iter()
                            .map(|asset_id| {
                                project
                                    .assets
                                    .iter()
                                    .find(|asset| asset.id == *asset_id)
                                    .cloned()
                                    .map(|asset| Command::AddAsset { asset })
                                    .ok_or_else(|| {
                                        gaw_project::Error::InvalidTransaction(
                                            "split asset is missing after commit".into(),
                                        )
                                    })
                            })
                            .collect::<gaw_project::Result<Vec<_>>>()?;
                        let transaction = Transaction::named("Create tempo regions", commands);
                        let mut expected = before;
                        transaction.apply(&mut expected).map_err(|error| {
                            gaw_project::Error::InvalidTransaction(error.to_string())
                        })?;
                        if expected != project {
                            return Err(gaw_project::Error::InvalidTransaction(
                                "project changed concurrently during tempo region split".into(),
                            ));
                        }
                        let next_session = ProjectSession::open(store.clone())?;
                        Ok((Arc::new(transaction), project, asset_ids, next_session))
                    })();
                    match result {
                        Ok((transaction, project, asset_ids, next_session)) => {
                            dirty = false;
                            failed = false;
                            revision = revision.max(next);
                            let event = ProjectEvent::MediaSplit {
                                revision: next,
                                transaction,
                                project,
                                asset_ids,
                            };
                            if events.send_timeout(event, WATCH_INTERVAL).is_ok() {
                                session = next_session;
                                baseline = project_fingerprint(store.root()).ok();
                            }
                        }
                        Err(error) => send_error(events, "tempo region split", error),
                    }
                }
                ProjectCommand::ImportStemSplit {
                    revision: next,
                    asset_id,
                    expected_content_hash,
                    output,
                } => {
                    let result = (|| -> gaw_project::Result<_> {
                        session.checkpoint()?;
                        let before = session.project().clone();
                        let stems = output
                            .files
                            .iter()
                            .map(|file| gaw_project::StemMedia {
                                instrument: file.stem.label().to_owned(),
                                source: file.path.clone(),
                            })
                            .collect::<Vec<_>>();
                        let imported = store.import_stem_split(
                            asset_id,
                            &expected_content_hash,
                            &before,
                            &stems,
                        )?;
                        let asset_ids = imported
                            .media
                            .iter()
                            .map(|media| media.asset_id)
                            .collect::<Vec<_>>();
                        let transaction = Arc::new(imported.transaction);
                        let project = imported.project;
                        let next_session = ProjectSession::open(store.clone())?;
                        Ok((
                            transaction,
                            project,
                            asset_ids,
                            imported.folder_name,
                            next_session,
                        ))
                    })();
                    match result {
                        Ok((transaction, project, asset_ids, folder_name, next_session)) => {
                            dirty = false;
                            failed = false;
                            revision = revision.max(next);
                            let event = ProjectEvent::StemSplitImported {
                                revision: next,
                                source_asset_id: asset_id,
                                transaction,
                                project,
                                asset_ids,
                                folder_name,
                            };
                            if events.send_timeout(event, WATCH_INTERVAL).is_ok() {
                                session = next_session;
                                baseline = project_fingerprint(store.root()).ok();
                            }
                        }
                        Err(error) => {
                            let _ = events.send_timeout(
                                ProjectEvent::StemSplitImportFailed {
                                    asset_id,
                                    error: ControllerError::new("stem split import", error),
                                },
                                WATCH_INTERVAL,
                            );
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
    generation: u64,
    revision: u64,
    composition_id: CompositionId,
    store: ProjectStore,
    project: Arc<Project>,
    focus_frame: u64,
    secondary_frame: Option<u64>,
}

#[derive(Debug)]
struct CompileResult {
    generation: u64,
    revision: u64,
    composition_id: CompositionId,
    focus_frame: u64,
    secondary_frame: Option<u64>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingSeek {
    target: u64,
    observed_before: u64,
}

impl PageWindow {
    fn contains(self, frame: u64) -> bool {
        let frame = if self.total_frames == 0 {
            return false;
        } else {
            frame.min(self.total_frames - 1)
        };
        (self.start_frame..self.end_frame).contains(&frame)
            || (self.secondary_start_frame..self.secondary_end_frame).contains(&frame)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AudioRequest {
    generation: u64,
    revision: u64,
    composition_id: CompositionId,
    focus_frame: u64,
    secondary_frame: Option<u64>,
}

#[derive(Debug)]
struct ReadyTimeline {
    generation: u64,
    revision: u64,
    composition_id: CompositionId,
    window: PageWindow,
    snapshot: Arc<RenderSnapshot>,
}

/// Audio state for the canonical timeline. `ready` is either the exact target
/// revision or absent; an older arrangement is never presented as current.
#[derive(Debug, Default)]
struct AuthoritativePlayback {
    generation: u64,
    target_revision: u64,
    composition_id: Option<CompositionId>,
    ready: Option<ReadyTimeline>,
    request: Option<AudioRequest>,
}

impl AuthoritativePlayback {
    fn invalidate(&mut self, revision: u64, composition_id: CompositionId) {
        self.generation = self.generation.saturating_add(1);
        self.target_revision = revision;
        self.composition_id = Some(composition_id);
        self.ready = None;
        self.request = None;
    }

    fn begin_request(&mut self, request: AudioRequest) {
        debug_assert_eq!(request.revision, self.target_revision);
        self.request = Some(request);
    }

    fn request_matches(&self, result: &CompileResult) -> bool {
        self.request
            == Some(AudioRequest {
                generation: result.generation,
                revision: result.revision,
                composition_id: result.composition_id,
                focus_frame: result.focus_frame,
                secondary_frame: result.secondary_frame,
            })
    }

    fn install(
        &mut self,
        generation: u64,
        revision: u64,
        composition_id: CompositionId,
        window: PageWindow,
        snapshot: Arc<RenderSnapshot>,
    ) {
        debug_assert_eq!(self.generation, generation);
        debug_assert_eq!(self.target_revision, revision);
        debug_assert_eq!(self.composition_id, Some(composition_id));
        self.request = None;
        self.ready = Some(ReadyTimeline {
            generation,
            revision,
            composition_id,
            window,
            snapshot,
        });
    }

    fn ready_for_target(&self) -> Option<&ReadyTimeline> {
        self.ready.as_ref().filter(|ready| {
            ready.generation == self.generation
                && ready.revision == self.target_revision
                && Some(ready.composition_id) == self.composition_id
        })
    }
}

#[derive(Debug)]
struct PreparedProject {
    generation: u64,
    revision: u64,
    composition_id: CompositionId,
    compiled: CompiledProject,
    pages: Vec<PreparedPage>,
}

#[derive(Debug, Default)]
struct CompileState {
    pending: Option<CompileJob>,
    completed: VecDeque<CompileResult>,
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

    fn request(&self, job: CompileJob) {
        let (lock, ready) = &*self.state;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = Some(job);
        state.completed.clear();
        ready.notify_one();
    }

    fn take_completed(&self) -> Option<CompileResult> {
        self.state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .completed
            .pop_front()
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
    let mut compiler = StorePlaybackCompiler::default();
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
        let result = prepare_snapshot_window(state, &job, &mut compiler, &mut prepared);
        let Some((window, result)) = result else {
            continue;
        };
        let mut value = state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !request_supersedes(value.pending.as_ref(), &job) {
            value.completed.push_back(CompileResult {
                generation: job.generation,
                revision: job.revision,
                composition_id: job.composition_id,
                focus_frame: job.focus_frame,
                secondary_frame: job.secondary_frame,
                window,
                result: result.map(Arc::new),
            });
        }
    }
}

fn prepare_snapshot_window(
    state: &Arc<(Mutex<CompileState>, Condvar)>,
    job: &CompileJob,
    compiler: &mut StorePlaybackCompiler,
    prepared: &mut Option<PreparedProject>,
) -> Option<(PageWindow, Result<RenderSnapshot, String>)> {
    if prepared.as_ref().is_none_or(|value| {
        value.generation != job.generation
            || value.revision != job.revision
            || value.composition_id != job.composition_id
    }) {
        let render_project =
            match compiler.compile_live_composition(&job.store, &job.project, job.composition_id) {
                Ok(project) => project,
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
            generation: job.generation,
            revision: job.revision,
            composition_id: job.composition_id,
            compiled: render_project,
            pages: Vec::new(),
        });
    }
    let value = prepared.as_mut().expect("project was prepared");
    let root = value.compiled.plan().root();
    let channels = root.output_layout.channels();
    let track_meter_bytes = track_peak_sidecar_bytes(
        root.tracks.iter().map(|track| track.id.as_ref()),
        AUDIO_PAGE_FRAMES,
    );
    let total = root.length_frames.saturating_add(root.tail_frames);
    let desired = page_window(
        total,
        channels,
        track_meter_bytes,
        job.focus_frame,
        job.secondary_frame,
    );
    value
        .pages
        .retain(|page| desired.contains(page.start_frame()));
    let page_frames = AUDIO_PAGE_FRAMES as u64;
    let focus_page = if total == 0 {
        0
    } else {
        job.focus_frame.min(total - 1) / page_frames * page_frames
    };
    let mut priorities = Vec::new();
    if focus_page < total {
        priorities.push(focus_page);
    }
    if let Some(secondary) = job.secondary_frame.filter(|_| total > 0) {
        let secondary = secondary.min(total - 1) / page_frames * page_frames;
        if desired.contains(secondary) && !priorities.contains(&secondary) {
            priorities.push(secondary);
        }
    }
    let mut forward = focus_page.saturating_add(page_frames);
    while forward < desired.end_frame {
        priorities.push(forward);
        forward = forward.saturating_add(page_frames);
    }
    let mut backward = focus_page;
    while backward > desired.start_frame {
        backward = backward.saturating_sub(page_frames);
        priorities.push(backward);
    }
    if let Some(start) = priorities
        .into_iter()
        .find(|start| value.pages.iter().all(|page| page.start_frame() != *start))
    {
        let frames = usize::try_from(total.saturating_sub(start))
            .unwrap_or(usize::MAX)
            .min(AUDIO_PAGE_FRAMES);
        match value.compiled.prepare_page(start, frames) {
            Ok(page) => value.pages.push(page),
            Err(error) => return Some((desired, Err(error.to_string()))),
        }
        if compile_request_is_superseded(state, job) {
            return None;
        }
    }
    value.pages.sort_by_key(PreparedPage::start_frame);
    let window = prepared_page_window(total, job.focus_frame, job.secondary_frame, &value.pages);
    let result = value
        .compiled
        .paged_snapshot(value.pages.iter().cloned())
        .map_err(|error| error.to_string());
    Some((window, result))
}

fn prepared_page_window(
    total_frames: u64,
    focus_frame: u64,
    secondary_frame: Option<u64>,
    pages: &[PreparedPage],
) -> PageWindow {
    fn contiguous_range(pages: &[PreparedPage], frame: u64) -> (u64, u64) {
        let Some(anchor) = pages.iter().position(|page| {
            let end = page.start_frame().saturating_add(page.frames() as u64);
            (page.start_frame()..end).contains(&frame)
        }) else {
            return (0, 0);
        };
        let mut first = anchor;
        let mut last = anchor;
        while first > 0 {
            let previous = &pages[first - 1];
            let previous_end = previous
                .start_frame()
                .saturating_add(previous.frames() as u64);
            if previous_end != pages[first].start_frame() {
                break;
            }
            first -= 1;
        }
        while last + 1 < pages.len() {
            let end = pages[last]
                .start_frame()
                .saturating_add(pages[last].frames() as u64);
            if end != pages[last + 1].start_frame() {
                break;
            }
            last += 1;
        }
        (
            pages[first].start_frame(),
            pages[last]
                .start_frame()
                .saturating_add(pages[last].frames() as u64),
        )
    }

    if total_frames == 0 {
        return PageWindow {
            start_frame: 0,
            end_frame: 0,
            secondary_start_frame: 0,
            secondary_end_frame: 0,
            total_frames,
        };
    }
    let focus = focus_frame.min(total_frames - 1);
    let (start_frame, end_frame) = contiguous_range(pages, focus);
    let (secondary_start_frame, secondary_end_frame) = secondary_frame
        .map(|frame| frame.min(total_frames - 1))
        .filter(|frame| !(start_frame..end_frame).contains(frame))
        .map_or((0, 0), |frame| contiguous_range(pages, frame));
    PageWindow {
        start_frame,
        end_frame,
        secondary_start_frame,
        secondary_end_frame,
        total_frames,
    }
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
        pending.generation != active.generation
            || pending.revision != active.revision
            || pending.composition_id != active.composition_id
            || pending.focus_frame != active.focus_frame
            || pending.secondary_frame != active.secondary_frame
    })
}

fn page_window(
    total_frames: u64,
    channels: usize,
    track_meter_bytes: usize,
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
        .saturating_mul(size_of::<f32>())
        .saturating_add(track_meter_bytes);
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

#[derive(Debug, Default)]
struct WaveformState {
    pending: Option<Project>,
    closed: bool,
}

#[derive(Clone, Debug)]
struct WaveformResult {
    asset_id: String,
    points: Result<Arc<[WaveformPoint]>, String>,
}

#[derive(Debug)]
struct WaveformWorker {
    state: Arc<(Mutex<WaveformState>, Condvar)>,
    results: Receiver<WaveformResult>,
    join: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct TranscriptionWorker {
    sender: Option<Sender<TranscriptionJob>>,
    results: Option<Receiver<TranscriptionResult>>,
    cancelled: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct StemSplitWorker {
    sender: Option<Sender<StemSplitJob>>,
    results: Option<Receiver<StemSplitResult>>,
    cancelled: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

#[derive(Debug)]
struct PendingClipExport {
    receiver: Receiver<Result<PathBuf, ControllerError>>,
    name: String,
}

#[derive(Debug)]
struct PendingStemSplit {
    cancelled: Arc<AtomicBool>,
    completed_stems: Arc<AtomicUsize>,
    installing: Arc<AtomicBool>,
    total_stems: usize,
}

impl StemSplitWorker {
    fn spawn() -> Self {
        let (sender, jobs) = bounded::<StemSplitJob>(STEM_SPLIT_QUEUE);
        let (result_sender, results) = bounded(STEM_SPLIT_QUEUE);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let join = thread::Builder::new()
            .name("gaw-xlance".into())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    let output = split_stems(&job, &worker_cancelled);
                    if result_sender.send(StemSplitResult { job, output }).is_err() {
                        break;
                    }
                }
            })
            .expect("stem split worker thread should start");
        Self {
            sender: Some(sender),
            results: Some(results),
            cancelled,
            join: Some(join),
        }
    }

    fn try_send(&self, job: StemSplitJob) -> Result<(), ()> {
        let Some(sender) = &self.sender else {
            return Err(());
        };
        sender.try_send(job).map_err(|_| ())
    }

    fn try_recv(&self) -> Result<StemSplitResult, TryRecvError> {
        self.results
            .as_ref()
            .map_or(Err(TryRecvError::Disconnected), Receiver::try_recv)
    }
}

impl Drop for StemSplitWorker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.sender.take();
        self.results.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl TranscriptionWorker {
    fn spawn() -> Self {
        let (sender, jobs) = bounded::<TranscriptionJob>(TRANSCRIPTION_QUEUE);
        let (result_sender, results) = bounded(TRANSCRIPTION_QUEUE);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let join = thread::Builder::new()
            .name("gaw-basic-pitch".into())
            .spawn(move || {
                while let Ok(job) = jobs.recv() {
                    let event_data = transcribe(&job, &worker_cancelled);
                    if result_sender
                        .send(TranscriptionResult { job, event_data })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("transcription worker thread should start");
        Self {
            sender: Some(sender),
            results: Some(results),
            cancelled,
            join: Some(join),
        }
    }

    fn try_send(&self, job: TranscriptionJob) -> Result<(), TranscriptionJob> {
        let Some(sender) = &self.sender else {
            return Err(job);
        };
        sender.try_send(job).map_err(|error| match error {
            TrySendError::Full(job) | TrySendError::Disconnected(job) => job,
        })
    }

    fn try_recv(&self) -> Result<TranscriptionResult, TryRecvError> {
        self.results
            .as_ref()
            .map_or(Err(TryRecvError::Disconnected), Receiver::try_recv)
    }
}

impl Drop for TranscriptionWorker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        self.sender.take();
        self.results.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl WaveformWorker {
    fn spawn(store: ProjectStore) -> Self {
        let state = Arc::new((Mutex::new(WaveformState::default()), Condvar::new()));
        let (sender, results) = bounded(16);
        let worker_state = Arc::clone(&state);
        let join = thread::Builder::new()
            .name("gaw-waveform-builder".into())
            .spawn(move || waveform_worker(&worker_state, &sender, &store))
            .expect("waveform worker thread should start");
        Self {
            state,
            results,
            join: Some(join),
        }
    }

    fn request(&self, project: Project) {
        let (lock, ready) = &*self.state;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = Some(project);
        ready.notify_one();
    }
}

impl Drop for WaveformWorker {
    fn drop(&mut self) {
        let (lock, ready) = &*self.state;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed = true;
        ready.notify_one();
        // A large source may still be scanning; detaching keeps app close instant.
        self.join.take();
    }
}

fn waveform_worker(
    state: &Arc<(Mutex<WaveformState>, Condvar)>,
    sender: &Sender<WaveformResult>,
    store: &ProjectStore,
) {
    let mut cache = HashMap::<String, Arc<[WaveformPoint]>>::new();
    loop {
        let project = {
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
            value
                .pending
                .take()
                .expect("pending waveform project exists")
        };
        for asset in &project.assets {
            let gaw_core::AudioAssetDefinition::Imported(imported) = &asset.definition else {
                continue;
            };
            let content_hash = imported.content_hash.to_string();
            let points = cache.get(&content_hash).cloned().map_or_else(
                || {
                    generate_asset_waveform(store, asset, imported).inspect(|points| {
                        cache.insert(content_hash.clone(), Arc::clone(points));
                    })
                },
                Ok,
            );
            if sender
                .send(WaveformResult {
                    asset_id: asset.id.to_string(),
                    points,
                })
                .is_err()
            {
                return;
            }
            let value = state
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if value.closed {
                return;
            }
            if value.pending.is_some() {
                break;
            }
        }
    }
}

fn generate_asset_waveform(
    store: &ProjectStore,
    asset: &gaw_core::AudioAsset,
    imported: &gaw_core::ImportedAudio,
) -> Result<Arc<[WaveformPoint]>, String> {
    let file = store
        .open_media(&imported.media_path, &imported.content_hash)
        .map_err(|error| error.to_string())?;
    let source = WavFrameSource::from_file(PathBuf::from(imported.media_path.as_str()), file)
        .map_err(|error| error.to_string())?;
    let layout = match imported.layout {
        gaw_core::ChannelLayout::Mono => ChannelLayout::Mono,
        gaw_core::ChannelLayout::Stereo => ChannelLayout::Stereo,
    };
    let context = RenderContext::new(imported.sample_rate.value(), layout, 0, "gaw-waveform-v1")
        .map_err(|error| error.to_string())?;
    let source: Arc<dyn FrameSource> = Arc::new(source);
    let revision = AssetRevision::new(
        asset.id.to_string(),
        imported.content_hash.to_string(),
        context,
        Arc::<[DependencyRevision]>::from([]),
        source,
    )
    .map_err(|error| error.to_string())?;
    let minimum_bucket = revision.frame_count().div_ceil(WAVEFORM_MAX_BUCKETS);
    let frames_per_bucket = WAVEFORM_BASE_BUCKET_FRAMES
        .max(minimum_bucket)
        .min(u64::from(u32::MAX)) as u32;
    let waveform =
        Waveform::generate(&revision, frames_per_bucket).map_err(|error| error.to_string())?;
    Ok(waveform
        .buckets
        .into_iter()
        .map(|bucket| WaveformPoint {
            minimum: bucket
                .peaks
                .iter()
                .map(|peak| peak.minimum)
                .fold(f32::INFINITY, f32::min),
            maximum: bucket
                .peaks
                .iter()
                .map(|peak| peak.maximum)
                .fold(f32::NEG_INFINITY, f32::max),
        })
        .collect::<Vec<_>>()
        .into())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TransportView {
    playing: bool,
    loop_enabled: bool,
    loop_start: f32,
    loop_end: f32,
    playhead: f32,
    bpm: f32,
    metronome_enabled: bool,
    metronome_gain: f32,
    master_volume_db: f32,
    meter_numerator: u8,
    meter_denominator: u8,
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
            metronome_enabled: value.metronome_enabled,
            metronome_gain: value.metronome_gain,
            master_volume_db: value.master_volume_db,
            meter_numerator: value.time_signature.numerator,
            meter_denominator: value.time_signature.denominator,
        }
    }
}

#[derive(Debug)]
struct AudioOutput {
    commands: CommandSender,
    device: CpalOutput,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct AudioConfiguration {
    pub(crate) output_device: Option<cpal::DeviceId>,
    pub(crate) buffer_frames: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ActiveAudioStatus {
    pub(crate) device_name: String,
    pub(crate) sample_rate: u32,
    pub(crate) requested_buffer_frames: Option<u32>,
    pub(crate) observed_buffer_frames: Option<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct AssetPreviewStatus {
    pub(crate) loading: bool,
    pub(crate) playing: bool,
    pub(crate) position_seconds: f64,
    pub(crate) duration_seconds: f64,
    pub(crate) error: Option<String>,
}

#[derive(Debug)]
struct AssetPreview {
    result: Option<Receiver<Result<Arc<RenderSnapshot>, String>>>,
    snapshot: Option<Arc<RenderSnapshot>>,
    playing: bool,
    position_seconds: f64,
    duration_seconds: f64,
    range_end_seconds: Option<f64>,
    telemetry_seek: Option<u64>,
    error: Option<String>,
}

impl AssetPreview {
    fn clamp_seconds(&self, seconds: f64) -> f64 {
        if !seconds.is_finite() {
            return 0.0;
        }
        if self.duration_seconds > 0.0 {
            seconds.clamp(0.0, self.duration_seconds)
        } else {
            seconds.max(0.0)
        }
    }
}

impl AudioOutput {
    fn open(
        sample_rate: u32,
        buffer_frames: Option<u32>,
        generation: StreamGeneration,
        target: Option<&RecoveryTarget>,
        notifications: &StreamNotificationSender,
    ) -> Result<(Self, OpenedOutputDeviceInfo), String> {
        let config = RealtimeEngineConfig {
            sample_rate,
            output_layout: ChannelLayout::Stereo,
            maximum_block_frames: 8_192,
            maximum_commands_per_block: 64,
        };
        let (commands, engine) = command_queue(config, 128, 8).map_err(|e| e.to_string())?;
        let callback = notifications.callback(generation);
        let device = match target {
            None => {
                CpalOutput::open_default_negotiated_with_buffer(engine, buffer_frames, callback)
            }
            Some(RecoveryTarget::Default { backend }) => {
                CpalOutput::open_default_on_backend_with_buffer(
                    *backend,
                    engine,
                    true,
                    buffer_frames,
                    callback,
                )
            }
            Some(RecoveryTarget::Device { device_id }) => CpalOutput::open_device_with_buffer(
                device_id,
                engine,
                true,
                buffer_frames,
                callback,
            ),
        }
        .map_err(|error| error.to_string())?;
        let selected = device.opened_device_info();
        device.play().map_err(|error| error.to_string())?;
        Ok((Self { commands, device }, selected))
    }

    fn status(&self) -> ActiveAudioStatus {
        let info = self.device.info();
        ActiveAudioStatus {
            device_name: self.device.opened_device_info().name,
            sample_rate: info.sample_rate,
            requested_buffer_frames: info.requested_buffer_frames,
            observed_buffer_frames: self.device.callback_buffer_frames(),
        }
    }
}

#[derive(Clone, Debug)]
struct DeviceOpenJob {
    sample_rate: u32,
    buffer_frames: Option<u32>,
    generation: StreamGeneration,
    target: Option<RecoveryTarget>,
    notifications: StreamNotificationSender,
}

#[derive(Debug)]
struct DeviceOpenResult {
    generation: StreamGeneration,
    target: Option<RecoveryTarget>,
    result: Result<(AudioOutput, OpenedOutputDeviceInfo), String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DeviceObservationResult {
    generation: StreamGeneration,
    observation: DeviceObservation,
}

fn output_selection(
    target: Option<&RecoveryTarget>,
    selected: &OpenedOutputDeviceInfo,
) -> OutputDeviceSelection {
    match target {
        Some(RecoveryTarget::Device { device_id }) => OutputDeviceSelection::Pinned {
            device_id: device_id.clone(),
        },
        None | Some(RecoveryTarget::Default { .. }) => OutputDeviceSelection::FollowDefault {
            backend: selected.backend,
        },
    }
}

#[derive(Debug)]
struct DeviceWatch {
    generation: StreamGeneration,
    selection: OutputDeviceSelection,
    last_sent: Option<DeviceObservation>,
}

#[derive(Debug)]
struct DeviceWorker {
    requests: Option<Sender<DeviceOpenJob>>,
    results: Receiver<DeviceOpenResult>,
    observations: Receiver<DeviceObservationResult>,
    join: Option<JoinHandle<()>>,
}

impl DeviceWorker {
    fn spawn() -> Self {
        let (requests, receiver) = bounded::<DeviceOpenJob>(1);
        let (sender, results) = bounded::<DeviceOpenResult>(1);
        let (observation_sender, observations) = bounded::<DeviceObservationResult>(1);
        let join = thread::Builder::new()
            .name("gaw-device-controller".into())
            .spawn(move || {
                let mut watch: Option<DeviceWatch> = None;
                loop {
                    match receiver.recv_timeout(DEVICE_OBSERVE_INTERVAL) {
                        Ok(job) => {
                            let result = AudioOutput::open(
                                job.sample_rate,
                                job.buffer_frames,
                                job.generation,
                                job.target.as_ref(),
                                &job.notifications,
                            );
                            let next_watch =
                                result.as_ref().ok().map(|(_, selected)| DeviceWatch {
                                    generation: job.generation,
                                    selection: output_selection(job.target.as_ref(), selected),
                                    last_sent: None,
                                });
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
                            watch = next_watch;
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            if let Some(watch) = &mut watch {
                                poll_device_observation(watch, &observation_sender);
                            }
                        }
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("device controller thread should start");
        Self {
            requests: Some(requests),
            results,
            observations,
            join: Some(join),
        }
    }

    fn request(&self, job: DeviceOpenJob) -> bool {
        self.requests
            .as_ref()
            .is_some_and(|sender| sender.try_send(job).is_ok())
    }
}

fn poll_device_observation(watch: &mut DeviceWatch, sender: &Sender<DeviceObservationResult>) {
    let Ok(observation) = observe_output_devices(&watch.selection) else {
        return;
    };
    if watch.last_sent.as_ref() == Some(&observation) {
        return;
    }
    if sender
        .try_send(DeviceObservationResult {
            generation: watch.generation,
            observation: observation.clone(),
        })
        .is_ok()
    {
        watch.last_sent = Some(observation);
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
    waveforms: WaveformWorker,
    transcriptions: TranscriptionWorker,
    pending_transcriptions: HashSet<AssetId>,
    stem_splits: StemSplitWorker,
    pending_stem_splits: HashMap<AssetId, PendingStemSplit>,
    importing_stem_splits: HashSet<AssetId>,
    clip_export: Option<PendingClipExport>,
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
    output_device: Option<cpal::DeviceId>,
    buffer_frames: Option<u32>,
    reconfigure_after_open: bool,
    playback: AuthoritativePlayback,
    announced_audio_generation: Option<u64>,
    asset_preview: Option<AssetPreview>,
    next_preview_revision: u64,
    pending_project: VecDeque<ProjectCommand>,
    deferred_project: Option<(u64, Project)>,
    pending_audio: VecDeque<RealtimeCommand>,
    latest_revision: u64,
    audio_revision: u64,
    submitted_revision: u64,
    compile_retry_at: Option<Instant>,
    telemetry_seek: Option<PendingSeek>,
    last_transport: TransportView,
    notice: Option<String>,
    error: Option<ControllerError>,
    closed: bool,
}

impl NativeController {
    pub(crate) fn media_path(&self, media_path: &str) -> PathBuf {
        self.store.root().join(media_path)
    }

    pub(crate) fn reveal_media(&self, media_path: &str) {
        let path = self.media_path(media_path);
        let directory = path.parent().unwrap_or(path.as_path());
        #[cfg(target_os = "linux")]
        let _ = std::process::Command::new("xdg-open")
            .arg(directory)
            .spawn();
        #[cfg(target_os = "macos")]
        let _ = std::process::Command::new("open").arg(directory).spawn();
        #[cfg(target_os = "windows")]
        let _ = std::process::Command::new("explorer")
            .arg(directory)
            .spawn();
    }

    #[cfg(test)]
    pub(crate) fn start(startup: NativeStartup) -> Self {
        Self::start_with_audio(startup, AudioConfiguration::default())
    }

    pub(crate) fn start_with_audio(
        startup: NativeStartup,
        audio_configuration: AudioConfiguration,
    ) -> Self {
        let store = startup.session.store().clone();
        let sample_rate = startup.project.sample_rate.value();
        let (notifications, notification_events) =
            stream_notification_channel(DEVICE_NOTIFICATION_CAPACITY)
                .expect("nonzero device notification capacity");
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
            metronome_enabled: startup.project.settings.metronome_enabled,
            metronome_gain: startup.project.settings.metronome_gain.value() as f32,
            master_volume_db: startup.project.settings.master_volume.value() as f32,
            meter_numerator: startup.project.time_signature.numerator,
            meter_denominator: startup.project.time_signature.denominator,
        };
        let waveforms = WaveformWorker::spawn(store.clone());
        waveforms.request(startup.project.clone());
        let mut controller = Self {
            store,
            project: ProjectWorker::spawn(startup.session),
            compiler: CompileWorker::spawn(),
            waveforms,
            transcriptions: TranscriptionWorker::spawn(),
            pending_transcriptions: HashSet::new(),
            stem_splits: StemSplitWorker::spawn(),
            pending_stem_splits: HashMap::new(),
            importing_stem_splits: HashSet::new(),
            clip_export: None,
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
            output_device: audio_configuration.output_device,
            buffer_frames: audio_configuration.buffer_frames,
            reconfigure_after_open: false,
            playback: AuthoritativePlayback::default(),
            announced_audio_generation: None,
            asset_preview: None,
            next_preview_revision: u64::MAX,
            pending_project: VecDeque::new(),
            deferred_project: None,
            pending_audio: VecDeque::new(),
            latest_revision: 0,
            audio_revision: 0,
            submitted_revision: 0,
            compile_retry_at: None,
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

    pub(crate) fn audio_status(&self) -> Option<ActiveAudioStatus> {
        self.audio.as_ref().map(AudioOutput::status)
    }

    pub(crate) fn configure_audio(
        &mut self,
        sample_rate: u32,
        output_device: Option<cpal::DeviceId>,
        buffer_frames: Option<u32>,
    ) {
        if self.sample_rate == sample_rate
            && self.output_device == output_device
            && self.buffer_frames == buffer_frames
        {
            return;
        }
        self.sample_rate = sample_rate;
        self.output_device = output_device;
        self.buffer_frames = buffer_frames;
        self.audio = None;
        self.recovery = None;
        self.next_device_open = Instant::now();
        if self.device_opening {
            self.reconfigure_after_open = true;
        } else {
            self.request_bootstrap_device();
        }
    }

    pub(crate) fn pump(&mut self, vm: &mut DemoViewModel, now: f64) {
        self.pump_waveforms(vm);
        self.pump_transcriptions(vm);
        self.pump_stem_splits(vm);
        self.pump_clip_export();
        self.flush_project();
        loop {
            match self.project.events.try_recv() {
                Ok(ProjectEvent::Journaled(revision)) => {
                    self.notice = Some(format!("Edit journaled · r{revision}"));
                }
                Ok(ProjectEvent::CanonicalReady(revision)) => {
                    self.notice = Some(format!("Canonical revision ready · r{revision}"));
                }
                Ok(ProjectEvent::External(project)) => {
                    let changed = changed_ids(&project);
                    match vm.replace_project_from_agent(project, changed, now) {
                        Ok(()) => {
                            self.waveforms.request(vm.project().clone());
                            self.notice = Some("Loaded external canonical change".into());
                        }
                        Err(error) => self.set_error("external project", error),
                    }
                }
                Ok(ProjectEvent::Saved(revision)) => {
                    self.notice = Some(format!("Saved revision {revision}"));
                }
                Ok(ProjectEvent::Imported {
                    revision,
                    transaction,
                    project,
                    asset_id,
                    original_filename,
                }) => match vm.accept_persisted_transaction(&transaction, &project, asset_id) {
                    Ok(()) => {
                        self.waveforms.request(vm.project().clone());
                        self.latest_revision = vm.revision().max(revision);
                        self.audio_revision = self.latest_revision;
                        self.invalidate_and_request_timeline(vm);
                        self.notice = Some(format!("Imported {original_filename}"));
                    }
                    Err(error) => self.set_error("asset import", error),
                },
                Ok(ProjectEvent::MediaSplit {
                    revision,
                    transaction,
                    project,
                    asset_ids,
                }) => {
                    let Some(selected) = asset_ids.first().copied() else {
                        self.set_error("tempo region split", "split produced no assets");
                        continue;
                    };
                    match vm.accept_persisted_transaction(&transaction, &project, selected) {
                        Ok(()) => {
                            self.waveforms.request(vm.project().clone());
                            self.latest_revision = vm.revision().max(revision);
                            self.audio_revision = self.latest_revision;
                            self.invalidate_and_request_timeline(vm);
                            self.notice = Some(format!(
                                "Created {} tempo region{}",
                                asset_ids.len(),
                                if asset_ids.len() == 1 { "" } else { "s" }
                            ));
                        }
                        Err(error) => self.set_error("tempo region split", error),
                    }
                }
                Ok(ProjectEvent::StemSplitImported {
                    revision,
                    source_asset_id,
                    transaction,
                    project,
                    asset_ids,
                    folder_name,
                }) => {
                    self.pending_stem_splits.remove(&source_asset_id);
                    self.importing_stem_splits.remove(&source_asset_id);
                    let Some(selected) = asset_ids.first().copied() else {
                        self.set_error("stem split import", "split produced no stems");
                        continue;
                    };
                    match vm.accept_persisted_stem_split(
                        &transaction,
                        &project,
                        &asset_ids,
                        selected,
                    ) {
                        Ok(()) => {
                            self.waveforms.request(vm.project().clone());
                            self.latest_revision = vm.revision().max(revision);
                            self.audio_revision = self.latest_revision;
                            self.invalidate_and_request_timeline(vm);
                            self.notice = Some(format!(
                                "Created {} stems in {folder_name}",
                                asset_ids.len()
                            ));
                        }
                        Err(error) => self.set_error("stem split import", error),
                    }
                }
                Ok(ProjectEvent::StemSplitImportFailed { asset_id, error }) => {
                    self.pending_stem_splits.remove(&asset_id);
                    self.importing_stem_splits.remove(&asset_id);
                    tracing::error!(subsystem = error.subsystem, message = %error.message);
                    self.notice = Some(format!("Stem import failed · {}", error.message));
                }
                Ok(ProjectEvent::Error(error)) => self.record_error(error),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }

        self.accept_updates(vm);
        self.ensure_playback_target(vm);

        if let Some(completed) = self.compiler.take_completed() {
            self.accept_compile_completion(vm, completed);
        }
        let project_sample_rate = vm.project().sample_rate.value();
        if project_sample_rate != self.sample_rate {
            self.configure_audio(
                project_sample_rate,
                self.output_device.clone(),
                self.buffer_frames,
            );
        }
        self.pump_device(vm);
        self.pump_asset_preview();
        self.sync_master_output(vm);
        if self.asset_preview.is_none() {
            self.sync_transport(vm);
            self.schedule_audio_pages(vm);
        }
        self.flush_audio();
        if let Some(audio) = &self.audio {
            audio.commands.reclaim_retired();
        }
        self.sync_playback_ack(vm);
        if self.asset_preview.is_none() {
            self.sync_callback_playhead(vm);
        }
        self.pump_meter_levels(vm);
    }

    fn pump_meter_levels(&mut self, vm: &mut DemoViewModel) {
        let (master_target, frame, timeline_audible) =
            self.audio.as_ref().map_or((0.0, 0, false), |audio| {
                (
                    audio.commands.output_peak(),
                    audio.commands.frame_position().saturating_sub(1),
                    audio.commands.audible_generation() == self.playback.generation,
                )
            });
        vm.transport.master_level = meter_level(master_target);

        let ready = self.playback.ready_for_target();
        let active_composition = ready.map(|ready| ready.composition_id.to_string());
        for composition in &mut vm.compositions {
            let active = timeline_audible
                && vm.transport.playing
                && self.asset_preview.is_none()
                && active_composition.as_deref() == Some(composition.id.as_str());
            for track in &mut composition.tracks {
                let target = if active {
                    ready
                        .and_then(|ready| ready.snapshot.track_peak_at(&track.id, frame))
                        .unwrap_or(0.0)
                } else {
                    0.0
                };
                track.level = meter_level(target);
            }
        }
    }

    fn pump_waveforms(&mut self, vm: &mut DemoViewModel) {
        while let Ok(result) = self.waveforms.results.try_recv() {
            match result.points {
                Ok(points) => vm.install_asset_waveform(&result.asset_id, points),
                Err(error) => {
                    self.notice = Some(format!("Waveform unavailable · {error}"));
                }
            }
        }
    }

    fn pump_transcriptions(&mut self, vm: &mut DemoViewModel) {
        while let Ok(result) = self.transcriptions.try_recv() {
            self.pending_transcriptions.remove(&result.job.asset_id);
            let current_asset = vm
                .assets
                .iter()
                .find(|asset| asset.id == result.job.asset_id.to_string());
            let current_hash = current_asset.and_then(|asset| asset.content_hash.as_deref());
            let metadata_matches = current_asset.is_some_and(|asset| {
                let current_bpm = f64::from(asset.bpm.unwrap_or(vm.transport.bpm));
                asset.name == result.job.source_name
                    && (current_bpm - result.job.bpm).abs() <= f64::EPSILON
            });
            if current_hash != result.job.content_hash.as_deref() || !metadata_matches {
                self.notice = Some(format!(
                    "Discarded stale MIDI conversion for {}",
                    result.job.source_name
                ));
                continue;
            }
            match result.event_data {
                Ok(event_data) => {
                    let note_count = event_data.events.len();
                    match vm.add_transcribed_event_data(event_data) {
                        Ok(name) => {
                            self.notice = Some(format!("Created {name} · {note_count} notes"));
                        }
                        Err(error) => {
                            tracing::error!(subsystem = "MIDI conversion", message = %error);
                            self.notice = Some(format!("Could not add MIDI asset · {error}"));
                        }
                    }
                }
                Err(error) => {
                    tracing::error!(subsystem = "MIDI conversion", message = %error);
                    self.notice = Some(format!("MIDI conversion failed · {error}"));
                }
            }
        }
    }

    fn pump_stem_splits(&mut self, vm: &mut DemoViewModel) {
        while let Ok(result) = self.stem_splits.try_recv() {
            if result.job.cancelled.load(Ordering::Acquire) {
                self.pending_stem_splits.remove(&result.job.asset_id);
                self.notice = Some(format!(
                    "Cancelled stem split for {}",
                    result.job.source_name
                ));
                continue;
            }
            let current_asset = vm
                .assets
                .iter()
                .find(|asset| asset.id == result.job.asset_id.to_string());
            let current_hash = current_asset.and_then(|asset| asset.content_hash.as_deref());
            if current_hash != result.job.content_hash.as_deref() || current_asset.is_none() {
                self.pending_stem_splits.remove(&result.job.asset_id);
                self.notice = Some(format!(
                    "Discarded stale stem split for {}",
                    result.job.source_name
                ));
                continue;
            }
            match result.output {
                Ok(output) => {
                    let Some(expected_content_hash) = result.job.content_hash else {
                        self.pending_stem_splits.remove(&result.job.asset_id);
                        self.notice = Some("Source audio is not materialized".into());
                        continue;
                    };
                    self.accept_updates(vm);
                    let revision = vm.revision();
                    if self.enqueue_project(ProjectCommand::ImportStemSplit {
                        revision,
                        asset_id: result.job.asset_id,
                        expected_content_hash,
                        output,
                    }) {
                        self.importing_stem_splits.insert(result.job.asset_id);
                        self.notice = Some(format!(
                            "Importing X-LANCE stems for {}…",
                            result.job.source_name
                        ));
                    } else {
                        self.pending_stem_splits.remove(&result.job.asset_id);
                        self.notice = Some("Stem import queue is full".into());
                    }
                }
                Err(error) => {
                    self.pending_stem_splits.remove(&result.job.asset_id);
                    tracing::error!(subsystem = "stem splitter", message = %error);
                    self.notice = Some(format!("Stem split failed · {error}"));
                }
            }
        }
    }

    pub(crate) fn is_transcribing(&self, asset_id: AssetId) -> bool {
        self.pending_transcriptions.contains(&asset_id)
    }

    pub(crate) fn is_splitting_stems(&self, asset_id: AssetId) -> bool {
        self.pending_stem_splits.contains_key(&asset_id)
    }

    pub(crate) fn stem_split_progress(
        &self,
        asset_id: AssetId,
    ) -> Option<(usize, usize, bool, bool)> {
        let progress = self.pending_stem_splits.get(&asset_id)?;
        Some((
            progress.completed_stems.load(Ordering::Acquire),
            progress.total_stems,
            progress.cancelled.load(Ordering::Acquire),
            progress.installing.load(Ordering::Acquire),
        ))
    }

    pub(crate) fn cancel_stem_split(&mut self, asset_id: AssetId) {
        if self.importing_stem_splits.contains(&asset_id) {
            self.notice = Some("X-LANCE finished; stems are already being imported".into());
            return;
        }
        if let Some(progress) = self.pending_stem_splits.get(&asset_id) {
            progress.cancelled.store(true, Ordering::Release);
            self.notice = Some("Cancelling X-LANCE stem split…".into());
        }
    }

    pub(crate) fn split_asset_stems(
        &mut self,
        asset_id: AssetId,
        media_path: &str,
        content_hash: Option<String>,
        source_name: &str,
        options: StemSplitOptions,
    ) {
        if self.pending_stem_splits.contains_key(&asset_id) {
            return;
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let completed_stems = Arc::new(AtomicUsize::new(0));
        let installing = Arc::new(AtomicBool::new(false));
        let total_stems = options.stems.len();
        let job = StemSplitJob {
            asset_id,
            content_hash,
            source_path: self.media_path(media_path),
            workspace_root: self.store.root().join(".gaw/xlance"),
            source_name: source_name.to_owned(),
            options,
            cancelled: Arc::clone(&cancelled),
            completed_stems: Arc::clone(&completed_stems),
            installing: Arc::clone(&installing),
        };
        match self.stem_splits.try_send(job) {
            Ok(()) => {
                self.pending_stem_splits.insert(
                    asset_id,
                    PendingStemSplit {
                        cancelled,
                        completed_stems,
                        installing,
                        total_stems,
                    },
                );
                self.notice = Some(format!("Splitting {source_name} with X-LANCE…"));
            }
            Err(()) => self.notice = Some("X-LANCE queue is full".into()),
        }
    }

    pub(crate) fn convert_asset_to_midi(
        &mut self,
        asset_id: AssetId,
        media_path: &str,
        content_hash: Option<String>,
        source_name: &str,
        bpm: f64,
    ) {
        if self.pending_transcriptions.contains(&asset_id) {
            return;
        }
        let job = TranscriptionJob {
            asset_id,
            content_hash,
            source_path: self.media_path(media_path),
            source_name: source_name.to_owned(),
            bpm,
        };
        match self.transcriptions.try_send(job) {
            Ok(()) => {
                self.pending_transcriptions.insert(asset_id);
                self.notice = Some(format!("Converting {source_name} to MIDI…"));
            }
            Err(_) => self.notice = Some("MIDI conversion queue is full".into()),
        }
    }

    pub(crate) fn save(&mut self, revision: u64, project: Project) {
        if !self.enqueue_project(ProjectCommand::Save { revision, project }) {
            self.set_error("persistence", "bounded save backlog is full");
        }
    }

    pub(crate) fn import_media(&mut self, source: PathBuf) {
        let revision = self.latest_revision.saturating_add(1);
        let name = source.file_name().map_or_else(
            || source.display().to_string(),
            |name| name.to_string_lossy().into(),
        );
        match self
            .project
            .try_send(ProjectCommand::ImportMedia { revision, source })
        {
            Ok(()) => self.notice = Some(format!("Importing {name}…")),
            Err(_) => self.set_error("asset import", "bounded project queue is full"),
        }
    }

    pub(crate) fn export_clip_mp3(&mut self, job: ClipExportJob) {
        if self.clip_export.is_some() {
            self.notice = Some("A clip export is already running".into());
            return;
        }
        let name = job.destination.file_name().map_or_else(
            || job.destination.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        let store = self.store.clone();
        let (sender, receiver) = bounded(1);
        let spawn = thread::Builder::new()
            .name("gaw-clip-export".into())
            .spawn(move || {
                let result = run_clip_export(&store, &job)
                    .map_err(|error| ControllerError::new("clip export", error));
                let _ = sender.send(result);
            });
        match spawn {
            Ok(_) => {
                self.clip_export = Some(PendingClipExport {
                    receiver,
                    name: name.clone(),
                });
                self.notice = Some(format!("Exporting {name}…"));
            }
            Err(error) => self.set_error("clip export", error),
        }
    }

    pub(crate) fn split_asset_regions(
        &mut self,
        revision: u64,
        asset_id: AssetId,
        regions: Vec<MediaRegion>,
    ) -> bool {
        let count = regions.len();
        if self
            .project
            .try_send(ProjectCommand::SplitImportedMedia {
                revision,
                asset_id,
                regions,
            })
            .is_ok()
        {
            self.notice = Some(format!(
                "Creating {count} tempo region{}…",
                if count == 1 { "" } else { "s" }
            ));
            true
        } else {
            self.set_error("tempo region split", "bounded project queue is full");
            false
        }
    }

    pub(crate) fn begin_asset_preview(&mut self, media_path: &str) {
        let path = self.media_path(media_path);
        let revision = self.next_preview_revision;
        self.next_preview_revision = self.next_preview_revision.saturating_sub(1);
        let (sender, receiver) = bounded(1);
        let spawn = thread::Builder::new()
            .name("gaw-asset-preview".into())
            .spawn(move || {
                let result = load_wav_memory_snapshot(path, revision)
                    .map(Arc::new)
                    .map_err(|error| error.to_string());
                let _ = sender.send(result);
            });
        self.pending_audio.clear();
        self.pending_audio.push_back(RealtimeCommand::Pause);
        self.pending_audio.push_back(RealtimeCommand::ClearPreview);
        let (result, error) = match spawn {
            Ok(_) => (Some(receiver), None),
            Err(error) => (None, Some(error.to_string())),
        };
        self.asset_preview = Some(AssetPreview {
            result,
            snapshot: None,
            playing: false,
            position_seconds: 0.0,
            duration_seconds: 0.0,
            range_end_seconds: None,
            telemetry_seek: None,
            error,
        });
    }

    pub(crate) fn toggle_asset_preview(&mut self) {
        let command = {
            let Some(preview) = &mut self.asset_preview else {
                return;
            };
            if preview.error.is_some() {
                return;
            }
            preview.playing = !preview.playing;
            preview.range_end_seconds = None;
            preview.snapshot.as_ref().map(|_| {
                if preview.playing {
                    RealtimeCommand::Play
                } else {
                    RealtimeCommand::Pause
                }
            })
        };
        if let Some(command) = command {
            self.enqueue_audio(command);
        }
    }

    pub(crate) fn stop_asset_preview(&mut self) {
        let Some(preview) = &mut self.asset_preview else {
            return;
        };
        preview.playing = false;
        preview.position_seconds = 0.0;
        preview.range_end_seconds = None;
        preview.telemetry_seek = Some(0);
        if preview.snapshot.is_some() {
            self.enqueue_audio(RealtimeCommand::Stop);
        }
    }

    pub(crate) fn seek_asset_preview(&mut self, seconds: f64) {
        let Some(preview) = &mut self.asset_preview else {
            return;
        };
        let seconds = preview.clamp_seconds(seconds);
        preview.position_seconds = seconds;
        preview.range_end_seconds = None;
        if let Some(snapshot) = &preview.snapshot {
            let frame = seconds_to_frame(seconds, snapshot.sample_rate(), snapshot.total_frames());
            preview.telemetry_seek = Some(frame);
            self.enqueue_audio(RealtimeCommand::Seek(frame));
        }
    }

    pub(crate) fn play_asset_preview_range(&mut self, start: f64, end: f64) {
        let Some(preview) = &mut self.asset_preview else {
            return;
        };
        let start = preview.clamp_seconds(start);
        let end = preview.clamp_seconds(end);
        if end <= start || preview.error.is_some() {
            return;
        }
        preview.position_seconds = start;
        preview.range_end_seconds = Some(end);
        preview.playing = true;
        if let Some(snapshot) = &preview.snapshot {
            let frame = seconds_to_frame(start, snapshot.sample_rate(), snapshot.total_frames());
            preview.telemetry_seek = Some(frame);
            self.enqueue_audio(RealtimeCommand::Seek(frame));
            self.enqueue_audio(RealtimeCommand::Play);
        }
    }

    pub(crate) fn asset_preview_status(&self) -> Option<AssetPreviewStatus> {
        self.asset_preview
            .as_ref()
            .map(|preview| AssetPreviewStatus {
                loading: preview.result.is_some(),
                playing: preview.playing,
                position_seconds: preview.position_seconds,
                duration_seconds: preview.duration_seconds,
                error: preview.error.clone(),
            })
    }

    pub(crate) fn end_asset_preview(&mut self, vm: &DemoViewModel) {
        if self.asset_preview.take().is_some() {
            self.restore_audio(vm);
            self.last_transport = (&vm.transport).into();
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
                            crate::theme::STATUS_ERROR
                        } else {
                            crate::theme::STATUS_NOTICE
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
        if matches!(command, RealtimeCommand::ActivateTimeline(_)) {
            self.pending_audio
                .retain(|pending| matches!(pending, RealtimeCommand::SetGain(_)));
            self.pending_audio.push_front(command);
            return;
        }
        if matches!(command, RealtimeCommand::ActivatePreview(_))
            && let Some(index) = self
                .pending_audio
                .iter()
                .rposition(|pending| matches!(pending, RealtimeCommand::ActivatePreview(_)))
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
        let target = self
            .output_device
            .clone()
            .map(|device_id| RecoveryTarget::Device { device_id });
        self.request_device(generation, target);
    }

    fn request_device(&mut self, generation: StreamGeneration, target: Option<RecoveryTarget>) {
        self.device_opening = self.devices.request(DeviceOpenJob {
            sample_rate: self.sample_rate,
            buffer_frames: self.buffer_frames,
            generation,
            target,
            notifications: self.notifications.clone(),
        });
    }

    fn pump_device(&mut self, vm: &DemoViewModel) {
        if let Ok(completed) = self.devices.results.try_recv() {
            self.device_opening = false;
            if self.reconfigure_after_open {
                self.reconfigure_after_open = false;
                self.request_bootstrap_device();
            } else {
                match completed.result {
                    Ok((audio, selected)) => {
                        if let Some(recovery) = &mut self.recovery {
                            let _ =
                                recovery.stream_started(completed.generation, selected.id.clone());
                        } else {
                            self.recovery = DeviceRecoveryController::new(
                                output_selection(completed.target.as_ref(), &selected),
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
                        if completed.target.is_some() && self.recovery.is_some() {
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
        }
        if !self.device_opening {
            for _ in 0..DEVICE_NOTIFICATION_CAPACITY {
                let Ok(notification) = self.notification_events.try_recv() else {
                    break;
                };
                if let Some(recovery) = &mut self.recovery {
                    let action = recovery.handle_notification(&notification);
                    self.handle_device_action(action);
                }
            }
        }
        while let Ok(observed) = self.devices.observations.try_recv() {
            if let Some(recovery) = &mut self.recovery {
                let action = apply_device_observation(recovery, &observed);
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
                self.next_generation = self
                    .next_generation
                    .max(generation.value().saturating_add(1));
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
        self.pending_audio
            .push_back(RealtimeCommand::SetGain(decibels_to_gain(
                vm.transport.master_volume_db,
            )));
        if let Some(preview) = &mut self.asset_preview {
            if let Some(snapshot) = &preview.snapshot {
                self.pending_audio
                    .push_back(RealtimeCommand::ActivatePreview(Arc::clone(snapshot)));
                self.pending_audio.push_back(RealtimeCommand::SetLoop(None));
                self.pending_audio
                    .push_back(RealtimeCommand::SetMetronome(RealtimeMetronome::default()));
                let frame = seconds_to_frame(
                    preview.position_seconds,
                    snapshot.sample_rate(),
                    snapshot.total_frames(),
                );
                preview.telemetry_seek = Some(frame);
                self.pending_audio.push_back(RealtimeCommand::Seek(frame));
                if preview.playing {
                    self.pending_audio.push_back(RealtimeCommand::Play);
                }
            } else {
                self.pending_audio.push_back(RealtimeCommand::Pause);
                self.pending_audio.push_back(RealtimeCommand::ClearPreview);
            }
            self.flush_audio();
            return;
        }
        let snapshot = self
            .playback
            .ready_for_target()
            .map(|ready| Arc::clone(&ready.snapshot));
        self.activate_timeline(vm, snapshot, false);
        self.flush_audio();
    }

    fn activate_timeline(
        &mut self,
        vm: &DemoViewModel,
        snapshot: Option<Arc<RenderSnapshot>>,
        preserve_callback_transport: bool,
    ) {
        let mut activation = timeline_activation(vm, self.playback.generation, snapshot);
        activation.preserve_transport = preserve_callback_transport;
        if !preserve_callback_transport {
            self.begin_telemetry_seek(activation.frame);
        }
        self.enqueue_audio(RealtimeCommand::ActivateTimeline(activation));
    }

    fn begin_telemetry_seek(&mut self, target: u64) {
        let observed_before = self
            .audio
            .as_ref()
            .map_or(target, |audio| audio.commands.frame_position());
        self.telemetry_seek = Some(PendingSeek {
            target,
            observed_before,
        });
    }

    fn accept_compile_completion(&mut self, vm: &mut DemoViewModel, completed: CompileResult) {
        if !completion_is_current(completed.revision, self.audio_revision) {
            return;
        }
        if !self.playback.request_matches(&completed) {
            return;
        }
        match completed.result {
            Ok(snapshot) => {
                self.compile_retry_at = None;
                self.playback.install(
                    completed.generation,
                    completed.revision,
                    completed.composition_id,
                    completed.window,
                    Arc::clone(&snapshot),
                );
                if self.asset_preview.is_none() {
                    self.activate_timeline(vm, Some(snapshot), true);
                }
                set_render_state(vm, RenderState::Rendering(100));
                self.notice = Some(format!(
                    "Audio prepared · activating r{}",
                    completed.revision
                ));
            }
            Err(error) => {
                self.playback.request = None;
                self.compile_retry_at = Some(Instant::now() + AUDIO_COMPILE_RETRY);
                set_render_state(vm, RenderState::Stale);
                self.set_error("audio compile", error);
            }
        }
    }

    fn pump_asset_preview(&mut self) {
        let Some(preview) = &mut self.asset_preview else {
            return;
        };
        let completed = preview
            .result
            .as_ref()
            .and_then(|receiver| match receiver.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Disconnected) => {
                    Some(Err("asset preview stopped unexpectedly".into()))
                }
                Err(TryRecvError::Empty) => None,
            });
        let mut commands = Vec::new();
        if let Some(completed) = completed {
            preview.result = None;
            match completed {
                Ok(snapshot) => {
                    preview.duration_seconds =
                        snapshot.total_frames() as f64 / f64::from(snapshot.sample_rate());
                    preview.position_seconds = preview.clamp_seconds(preview.position_seconds);
                    let frame = seconds_to_frame(
                        preview.position_seconds,
                        snapshot.sample_rate(),
                        snapshot.total_frames(),
                    );
                    preview.telemetry_seek = Some(frame);
                    commands.push(RealtimeCommand::ActivatePreview(Arc::clone(&snapshot)));
                    commands.push(RealtimeCommand::SetLoop(None));
                    commands.push(RealtimeCommand::SetMetronome(RealtimeMetronome::default()));
                    commands.push(RealtimeCommand::Seek(frame));
                    if preview.playing {
                        commands.push(RealtimeCommand::Play);
                    }
                    preview.snapshot = Some(snapshot);
                }
                Err(error) => {
                    preview.playing = false;
                    preview.error = Some(error);
                }
            }
        }
        if let (Some(snapshot), Some(audio)) = (&preview.snapshot, &self.audio) {
            let frame = audio.commands.frame_position().min(snapshot.total_frames());
            if let Some(target) = preview.telemetry_seek {
                if frame.abs_diff(target) <= 8_192 {
                    preview.telemetry_seek = None;
                }
            } else {
                preview.position_seconds = frame as f64 / f64::from(snapshot.sample_rate());
            }
            let reached_range = preview
                .range_end_seconds
                .is_some_and(|end| preview.position_seconds >= end);
            let reached_end = frame >= snapshot.total_frames();
            if preview.playing && (reached_range || reached_end) {
                preview.playing = false;
                preview.range_end_seconds = None;
                commands.push(RealtimeCommand::Pause);
            }
        }
        for command in commands {
            self.enqueue_audio(command);
        }
    }

    fn sync_playback_ack(&mut self, vm: &mut DemoViewModel) {
        if self.asset_preview.is_some() {
            return;
        }
        let Some(audio) = &self.audio else { return };
        if self.playback.ready_for_target().is_some()
            && audio.commands.audible_generation() == self.playback.generation
        {
            set_render_state(vm, RenderState::Fresh);
            if self.announced_audio_generation != Some(self.playback.generation) {
                self.announced_audio_generation = Some(self.playback.generation);
                self.notice = Some(format!("Audio active · r{}", self.latest_revision));
            }
        }
    }

    fn device_millis(&self) -> u64 {
        u64::try_from(self.device_clock.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn sync_transport(&mut self, vm: &DemoViewModel) {
        let current = TransportView::from(&vm.transport);
        let loop_changed = current.loop_enabled != self.last_transport.loop_enabled
            || (current.loop_start - self.last_transport.loop_start).abs() > f32::EPSILON
            || (current.loop_end - self.last_transport.loop_end).abs() > f32::EPSILON
            || (current.bpm - self.last_transport.bpm).abs() > f32::EPSILON;
        if loop_changed {
            self.enqueue_audio(RealtimeCommand::SetLoop(realtime_loop(vm)));
        }
        if current.metronome_enabled != self.last_transport.metronome_enabled
            || (current.metronome_gain - self.last_transport.metronome_gain).abs() > f32::EPSILON
            || (current.bpm - self.last_transport.bpm).abs() > f32::EPSILON
            || current.meter_numerator != self.last_transport.meter_numerator
            || current.meter_denominator != self.last_transport.meter_denominator
        {
            self.enqueue_audio(RealtimeCommand::SetMetronome(realtime_metronome(vm)));
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
            self.begin_telemetry_seek(frame);
            self.enqueue_audio(RealtimeCommand::Seek(frame));
            self.retarget_audio_for_discontinuity(vm, frame);
        } else if loop_changed {
            self.retarget_audio_for_discontinuity(vm, transport_frame(vm));
        }
        self.last_transport = current;
    }

    fn sync_master_output(&mut self, vm: &DemoViewModel) {
        let volume_db = vm.transport.master_volume_db;
        if (volume_db - self.last_transport.master_volume_db).abs() > f32::EPSILON {
            self.enqueue_audio(RealtimeCommand::SetGain(decibels_to_gain(volume_db)));
            self.last_transport.master_volume_db = volume_db;
        }
    }

    /// Retarget page preparation only for an explicit transport discontinuity.
    /// Normal playback movement must never restart a potentially expensive compile.
    fn retarget_audio_for_discontinuity(&mut self, vm: &DemoViewModel, frame: u64) {
        let secondary = loop_anchor(vm);
        let resident_covers_transport = self.playback.ready_for_target().is_some_and(|ready| {
            ready.window.contains(frame)
                && secondary.is_none_or(|anchor| ready.window.contains(anchor))
        });
        if resident_covers_transport {
            return;
        }
        let request = AudioRequest {
            generation: self.playback.generation,
            revision: self.audio_revision,
            composition_id: self
                .playback
                .composition_id
                .expect("playback target exists"),
            focus_frame: frame,
            secondary_frame: secondary,
        };
        if self.playback.request != Some(request) {
            self.request_audio_window(self.audio_revision, vm.project(), frame, secondary);
        }
    }

    fn sync_callback_playhead(&mut self, vm: &mut DemoViewModel) {
        let Some(audio) = &self.audio else { return };
        if audio.commands.active_generation() != self.playback.generation {
            return;
        }
        let frame = audio.commands.frame_position();
        if let Some(pending) = self.telemetry_seek {
            if !seek_was_observed(pending, frame) {
                return;
            }
            self.telemetry_seek = None;
        }
        let beat = frame_to_beat(frame, vm.transport.bpm, vm.project().sample_rate.value());
        vm.transport.playhead = beat.min(vm.current_composition().length_beats);
        self.last_transport.playhead = vm.transport.playhead;
    }

    fn accept_updates(&mut self, vm: &mut DemoViewModel) {
        if !self.importing_stem_splits.is_empty() {
            return;
        }
        let updates = vm.take_updates().collect::<Vec<_>>();
        if updates.is_empty() {
            return;
        }
        let can_keep_current_audio = updates.iter().all(|update| {
            !update.audio_render_changed
                || update.transaction.as_ref().is_some_and(|transaction| {
                    !transaction.commands.is_empty()
                        && transaction
                            .commands
                            .iter()
                            .all(|command| matches!(command, Command::SetTrackVolume { .. }))
                })
        });
        if updates
            .first()
            .is_some_and(|update| update.revision > self.submitted_revision.saturating_add(1))
        {
            let requires_audio_rebuild = updates.iter().any(|update| update.audio_render_changed);
            self.defer_project(vm.revision(), vm.project().clone());
            if requires_audio_rebuild {
                self.audio_revision = self.latest_revision;
                if can_keep_current_audio {
                    self.request_timeline_replacement(vm);
                } else {
                    self.invalidate_and_request_timeline(vm);
                }
            }
            return;
        }
        let mut coalescing = self.deferred_project.is_some();
        let mut requires_audio_rebuild = false;
        for update in updates {
            self.latest_revision = self.latest_revision.max(update.revision);
            self.submitted_revision = self.submitted_revision.max(update.revision);
            requires_audio_rebuild |= update.audio_render_changed;
            if coalescing {
                self.defer_project(update.revision, vm.project().clone());
                continue;
            }
            match (update.source, update.transaction) {
                (_, Some(transaction)) => {
                    if !self.enqueue_project(ProjectCommand::Apply {
                        revision: update.revision,
                        transaction,
                    }) {
                        self.defer_project(update.revision, vm.project().clone());
                        coalescing = true;
                    }
                    if requires_audio_rebuild {
                        set_render_state(vm, RenderState::Stale);
                    }
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
        if requires_audio_rebuild {
            self.audio_revision = self.latest_revision;
            if can_keep_current_audio {
                self.request_timeline_replacement(vm);
            } else {
                self.invalidate_and_request_timeline(vm);
            }
        }
    }

    fn request_timeline_replacement(&mut self, vm: &mut DemoViewModel) {
        self.playback
            .invalidate(self.audio_revision, vm.current_composition_id());
        self.pending_audio.retain(|command| {
            !matches!(
                command,
                RealtimeCommand::ActivatePreview(_) | RealtimeCommand::ActivateTimeline(_)
            )
        });
        self.request_audio_window(
            self.audio_revision,
            vm.project(),
            transport_frame(vm),
            loop_anchor(vm),
        );
        set_render_state(vm, RenderState::Rendering(0));
    }

    fn invalidate_and_request_timeline(&mut self, vm: &mut DemoViewModel) {
        self.playback
            .invalidate(self.audio_revision, vm.current_composition_id());
        if let Some(audio) = &self.audio {
            audio.commands.invalidate_timeline(self.playback.generation);
        }
        self.pending_audio.retain(|command| {
            !matches!(
                command,
                RealtimeCommand::ActivatePreview(_) | RealtimeCommand::ActivateTimeline(_)
            )
        });
        if self.asset_preview.is_none() {
            self.activate_timeline(vm, None, false);
        }
        self.request_audio_window(
            self.audio_revision,
            vm.project(),
            transport_frame(vm),
            loop_anchor(vm),
        );
        set_render_state(vm, RenderState::Rendering(0));
    }

    fn ensure_playback_target(&mut self, vm: &mut DemoViewModel) {
        let composition_id = vm.current_composition_id();
        if self.playback.target_revision != self.audio_revision
            || self.playback.composition_id != Some(composition_id)
        {
            self.invalidate_and_request_timeline(vm);
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
        project: &Project,
        focus_frame: u64,
        secondary_frame: Option<u64>,
    ) {
        self.compile_retry_at = None;
        let composition_id = self
            .playback
            .composition_id
            .expect("playback target is initialized before compilation");
        self.compiler.request(CompileJob {
            generation: self.playback.generation,
            revision,
            composition_id,
            store: self.store.clone(),
            project: Arc::new(project.clone()),
            focus_frame,
            secondary_frame,
        });
        self.playback.begin_request(AudioRequest {
            generation: self.playback.generation,
            revision,
            composition_id,
            focus_frame,
            secondary_frame,
        });
    }

    fn schedule_audio_pages(&mut self, vm: &DemoViewModel) {
        if self
            .compile_retry_at
            .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            return;
        }
        let frame = transport_frame(vm);
        let lead = AUDIO_PREPARE_LEAD_PAGES.saturating_mul(AUDIO_PAGE_FRAMES as u64);
        let secondary = loop_anchor(vm);
        // A current-revision compile owns its target until it completes. The playhead
        // advances while the silent clock is installed; using that movement as a new
        // target repeatedly supersedes slow renders and can leave playback silent
        // forever. Explicit seeks and loop moves retarget in `sync_transport`.
        if self
            .playback
            .request
            .is_some_and(|request| request.revision == self.audio_revision)
        {
            return;
        }
        let needs_window = self.playback.ready_for_target().is_none_or(|ready| {
            !ready.window.contains(frame)
                || secondary.is_some_and(|anchor| !ready.window.contains(anchor))
                || (vm.transport.playing
                    && ready.window.end_frame < ready.window.total_frames
                    && ready.window.end_frame.saturating_sub(frame) <= lead)
        });
        if needs_window {
            self.request_audio_window(self.audio_revision, vm.project(), frame, secondary);
        }
    }

    fn record_error(&mut self, error: ControllerError) {
        if self.error.as_ref() != Some(&error) {
            tracing::error!(subsystem = error.subsystem, message = %error.message);
            self.error = Some(error);
        }
    }

    fn pump_clip_export(&mut self) {
        let Some(export) = &self.clip_export else {
            return;
        };
        let result = match export.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(ControllerError::new(
                "clip export",
                format!("{} stopped unexpectedly", export.name),
            ))),
        };
        let Some(result) = result else {
            return;
        };
        self.clip_export = None;
        match result {
            Ok(path) => self.notice = Some(format!("Exported {}", path.display())),
            Err(error) => self.record_error(error),
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

fn apply_device_observation(
    recovery: &mut DeviceRecoveryController,
    observed: &DeviceObservationResult,
) -> DeviceRecoveryAction {
    if observed.generation != recovery.active_generation() {
        return DeviceRecoveryAction::None;
    }
    recovery.observe(&observed.observation)
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

fn decibels_to_gain(decibels: f32) -> f32 {
    if decibels.is_finite() {
        10.0_f32.powf(decibels / 20.0)
    } else {
        0.0
    }
}

fn meter_level(measured: f32) -> f32 {
    if measured.is_finite() {
        measured.max(0.0)
    } else {
        0.0
    }
}

fn seconds_to_frame(seconds: f64, sample_rate: u32, total_frames: u64) -> u64 {
    if !seconds.is_finite() || seconds <= 0.0 || sample_rate == 0 {
        return 0;
    }
    let frame = seconds * f64::from(sample_rate);
    if frame >= total_frames as f64 {
        total_frames
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
    vm.transport.loop_enabled.then(|| {
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

fn realtime_metronome(vm: &DemoViewModel) -> RealtimeMetronome {
    RealtimeMetronome {
        enabled: vm.transport.metronome_enabled,
        bpm: f64::from(vm.transport.bpm),
        numerator: vm.transport.time_signature.numerator,
        denominator: vm.transport.time_signature.denominator,
        gain: vm.transport.metronome_gain,
    }
}

fn timeline_activation(
    vm: &DemoViewModel,
    generation: u64,
    snapshot: Option<Arc<RenderSnapshot>>,
) -> TimelineActivation {
    let total_frames = snapshot.as_ref().map_or_else(
        || {
            beat_to_frame(
                vm.current_composition().length_beats,
                vm.transport.bpm,
                vm.project().sample_rate.value(),
            )
        },
        |snapshot| snapshot.total_frames(),
    );
    TimelineActivation {
        generation,
        snapshot,
        preserve_transport: false,
        sample_rate: vm.project().sample_rate.value(),
        total_frames,
        frame: transport_frame(vm),
        playing: vm.transport.playing,
        loop_range: realtime_loop(vm),
        metronome: realtime_metronome(vm),
    }
}

fn seek_was_observed(pending: PendingSeek, observed: u64) -> bool {
    const SEEK_ACK_TOLERANCE_FRAMES: u64 = 8_192;
    if observed.abs_diff(pending.target) <= SEEK_ACK_TOLERANCE_FRAMES {
        return true;
    }
    match pending.target.cmp(&pending.observed_before) {
        std::cmp::Ordering::Less => observed < pending.observed_before,
        std::cmp::Ordering::Greater => observed >= pending.target,
        std::cmp::Ordering::Equal => observed != pending.observed_before,
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

    #[test]
    fn meter_levels_follow_measurements_without_release_latency() {
        assert!((meter_level(0.8) - 0.8).abs() < f32::EPSILON);
        assert!(meter_level(0.0).abs() < f32::EPSILON);
        assert!(meter_level(-1.0).abs() < f32::EPSILON);
        assert!(meter_level(f32::NAN).abs() < f32::EPSILON);
    }

    #[test]
    fn master_volume_edit_is_classified_as_monitoring_only() {
        let master = Transaction::new([Command::SetProjectMasterVolume {
            volume: gaw_core::Decibels::new(-6.0).unwrap(),
        }]);
        assert!(!master.affects_render());

        let tempo = Transaction::new([Command::SetProjectTempo {
            bpm: Bpm::new(128.0).unwrap(),
        }]);
        assert!(tempo.affects_render());
    }

    #[test]
    fn master_volume_edit_and_undo_keep_the_audio_revision_live() {
        let (_directory, store) = store();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        let mut controller = NativeController::start(startup);
        controller
            .playback
            .invalidate(0, vm.current_composition_id());
        let generation = controller.playback.generation;

        vm.apply(Intent::SetMasterVolume(-6.0));
        controller.accept_updates(&mut vm);
        assert_eq!(controller.latest_revision, 1);
        assert_eq!(controller.audio_revision, 0);
        assert_eq!(controller.playback.generation, generation);

        vm.apply(Intent::Undo(0.0));
        controller.accept_updates(&mut vm);
        assert_eq!(controller.latest_revision, 2);
        assert_eq!(controller.audio_revision, 0);
        assert_eq!(controller.playback.generation, generation);
    }

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

    fn write_test_wav(path: &Path) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for sample in 0_i16..64 {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn write_audible_test_wav(path: &Path, frames: usize) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        for _ in 0..frames {
            writer.write_sample(12_000_i16).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn accept_exact_test_render(
        controller: &mut NativeController,
        vm: &mut DemoViewModel,
        store: &ProjectStore,
    ) -> Arc<RenderSnapshot> {
        let request = controller.playback.request.expect("active render request");
        let mut compiler = StorePlaybackCompiler::default();
        let snapshot = Arc::new(
            compiler
                .compile_live_composition(store, vm.project(), request.composition_id)
                .unwrap()
                .snapshot()
                .unwrap(),
        );
        let window = PageWindow {
            start_frame: 0,
            end_frame: snapshot.total_frames(),
            secondary_start_frame: 0,
            secondary_end_frame: 0,
            total_frames: snapshot.total_frames(),
        };
        controller.accept_compile_completion(
            vm,
            CompileResult {
                generation: request.generation,
                revision: request.revision,
                composition_id: request.composition_id,
                focus_frame: request.focus_frame,
                secondary_frame: request.secondary_frame,
                window,
                result: Ok(Arc::clone(&snapshot)),
            },
        );
        snapshot
    }

    fn snapshot_is_audible_at(snapshot: Arc<RenderSnapshot>, frame: u64) -> bool {
        let generation = 1;
        let total_frames = snapshot.total_frames();
        let (sender, mut engine) = command_queue(RealtimeEngineConfig::default(), 8, 8).unwrap();
        sender
            .try_send(RealtimeCommand::ActivateTimeline(TimelineActivation {
                generation,
                sample_rate: snapshot.sample_rate(),
                snapshot: Some(snapshot),
                preserve_transport: false,
                total_frames,
                frame,
                playing: true,
                loop_range: None,
                metronome: RealtimeMetronome::default(),
            }))
            .unwrap();
        let mut output = vec![0.0_f32; 128 * 2];
        engine.process(&mut output);
        output.iter().any(|sample| sample.abs() > 0.001)
    }

    fn flush_test_audio(controller: &mut NativeController, sender: &CommandSender) {
        while let Some(command) = controller.pending_audio.pop_front() {
            sender.try_send(command).unwrap();
        }
    }

    #[test]
    fn canonical_waveform_uses_real_signed_source_peaks() {
        let (directory, store) = store();
        let source = directory.path().join("waveform.wav");
        write_test_wav(&source);
        let imported_media = store.import_media(source).unwrap();
        let project = store.load_project().unwrap();
        let asset = project
            .assets
            .iter()
            .find(|asset| asset.id == imported_media.asset_id)
            .unwrap();
        let gaw_core::AudioAssetDefinition::Imported(imported) = &asset.definition else {
            panic!("import created a canonical audio asset");
        };
        let waveform = generate_asset_waveform(&store, asset, imported).unwrap();
        assert_eq!(waveform.len(), 1);
        assert!(waveform[0].minimum.abs() < f32::EPSILON);
        assert!(waveform[0].maximum > 0.0);
    }

    #[test]
    fn worker_import_is_canonical_immediate_and_undoable_without_duplicate_persistence() {
        let (directory, store) = store();
        let source = directory.path().join("kick.wav");
        write_test_wav(&source);
        let before = store.load_project().unwrap();
        let mut worker = ProjectWorker::spawn(ProjectSession::open(store.clone()).unwrap());
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(ProjectCommand::ImportMedia {
                revision: 1,
                source,
            })
            .unwrap();
        let (transaction, project, asset_id) = loop {
            match worker.events.recv_timeout(Duration::from_secs(2)).unwrap() {
                ProjectEvent::Imported {
                    transaction,
                    project,
                    asset_id,
                    original_filename,
                    ..
                } => {
                    assert_eq!(original_filename, "kick.wav");
                    break (transaction, project, asset_id);
                }
                ProjectEvent::CanonicalReady(_) => {}
                event => panic!("unexpected project event: {event:?}"),
            }
        };
        assert_eq!(store.load_project().unwrap(), project);
        assert!(store.pending_recovery().unwrap().is_empty());

        let mut vm = DemoViewModel::from_project(before).unwrap();
        vm.accept_persisted_transaction(&transaction, &project, asset_id)
            .unwrap();
        assert_eq!(vm.project(), &project);
        assert!(vm.take_updates().next().is_none());
        assert!(
            matches!(vm.stable_selection(), crate::StableSelection::Asset(id) if id == asset_id)
        );
        vm.apply(Intent::Undo(1.0));
        assert!(vm.project().assets.is_empty());
        assert_eq!(vm.take_updates().next().unwrap().source, ChangeSource::Undo);
        worker.close(true).unwrap();
    }

    #[test]
    fn worker_splits_tempo_regions_in_one_undoable_transition() {
        let (directory, store) = store();
        let source = directory.path().join("tempo-change.wav");
        write_test_wav(&source);
        let imported = store.import_media(source).unwrap();
        let before = store.load_project().unwrap();
        let mut worker = ProjectWorker::spawn(ProjectSession::open(store.clone()).unwrap());
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(ProjectCommand::SplitImportedMedia {
                revision: 1,
                asset_id: imported.asset_id,
                regions: vec![
                    MediaRegion {
                        range: gaw_core::FrameRange {
                            start: gaw_core::FramePosition(0),
                            length: gaw_core::FrameCount(32),
                        },
                        bpm: Bpm::new(90.0).unwrap(),
                    },
                    MediaRegion {
                        range: gaw_core::FrameRange {
                            start: gaw_core::FramePosition(32),
                            length: gaw_core::FrameCount(32),
                        },
                        bpm: Bpm::new(128.0).unwrap(),
                    },
                ],
            })
            .unwrap();

        let (transaction, project, asset_ids) = loop {
            match worker.events.recv_timeout(Duration::from_secs(2)).unwrap() {
                ProjectEvent::MediaSplit {
                    transaction,
                    project,
                    asset_ids,
                    ..
                } => break (transaction, project, asset_ids),
                ProjectEvent::CanonicalReady(_) => {}
                event => panic!("unexpected project event: {event:?}"),
            }
        };
        assert_eq!(asset_ids.len(), 2);
        assert_eq!(project.assets.len(), before.assets.len() + 2);
        assert!(
            project
                .assets
                .iter()
                .any(|asset| asset.id == imported.asset_id)
        );
        assert_eq!(store.load_project().unwrap(), project);

        let mut expected = before;
        transaction.apply(&mut expected).unwrap();
        assert_eq!(expected, project);
        worker.close(true).unwrap();
    }

    #[test]
    fn worker_imports_a_stem_folder_as_one_undoable_transition() {
        let (directory, store) = store();
        let source = directory.path().join("song.wav");
        let stem = directory.path().join("stem.wav");
        write_test_wav(&source);
        write_test_wav(&stem);
        let imported = store.import_media(source).unwrap();
        let before = store.load_project().unwrap();
        let output = StemSplitOutput::from_test_files(&[
            (crate::stem_splitter::Stem::Vocals, &stem),
            (crate::stem_splitter::Stem::Drums, &stem),
        ]);
        let mut worker = ProjectWorker::spawn(ProjectSession::open(store.clone()).unwrap());
        worker
            .sender
            .as_ref()
            .unwrap()
            .send(ProjectCommand::ImportStemSplit {
                revision: 1,
                asset_id: imported.asset_id,
                expected_content_hash: imported.content_hash.to_string(),
                output,
            })
            .unwrap();

        let (transaction, project, asset_ids) = loop {
            match worker.events.recv_timeout(Duration::from_secs(2)).unwrap() {
                ProjectEvent::StemSplitImported {
                    transaction,
                    project,
                    asset_ids,
                    ..
                } => break (transaction, project, asset_ids),
                ProjectEvent::CanonicalReady(_) => {}
                event => panic!("unexpected project event: {event:?}"),
            }
        };
        assert_eq!(asset_ids.len(), 2);
        assert_eq!(project.asset_folders.len(), 1);
        assert_eq!(store.load_project().unwrap(), project);

        let mut vm = DemoViewModel::from_project(before.clone()).unwrap();
        vm.accept_persisted_stem_split(&transaction, &project, &asset_ids, asset_ids[0])
            .unwrap();
        assert_eq!(vm.project(), &project);
        vm.apply(Intent::Undo(1.0));
        assert_eq!(vm.project(), &before);
        vm.apply(Intent::Redo(2.0));
        assert_eq!(vm.project(), &project);

        let mut ahead_vm = DemoViewModel::from_project(before).unwrap();
        ahead_vm
            .apply_agent_transaction(
                &Transaction::new([Command::SetProjectName {
                    name: "Edited while splitting".into(),
                }]),
                ["project".to_owned()],
                1.0,
            )
            .unwrap();
        ahead_vm
            .accept_persisted_stem_split(&transaction, &project, &asset_ids, asset_ids[0])
            .unwrap();
        assert_eq!(ahead_vm.project().name, "Edited while splitting");
        assert_eq!(ahead_vm.project().asset_folders.len(), 1);
        assert_eq!(ahead_vm.project().assets.len(), project.assets.len());
        worker.close(true).unwrap();
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
        let project = Arc::new(store.load_project().unwrap());
        let composition_id = project.root_composition_id;
        let mut state = CompileState::default();
        for revision in 1..=128 {
            state.pending = Some(CompileJob {
                generation: revision,
                revision,
                composition_id,
                store: store.clone(),
                project: Arc::clone(&project),
                focus_frame: revision * AUDIO_PAGE_FRAMES as u64,
                secondary_frame: None,
            });
        }
        assert_eq!(state.pending.as_ref().unwrap().revision, 128);
        assert_eq!(state.pending.as_ref().unwrap().generation, 128);
        assert!(!completion_is_current(127, 128));
        assert!(completion_is_current(128, 128));
    }

    #[test]
    fn failed_current_compile_retries_without_clearing_a_newer_focus_request() {
        let (_directory, store) = store();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        let mut controller = NativeController::start(startup);
        controller.latest_revision = 7;
        controller.audio_revision = 7;
        let composition_id = vm.current_composition_id();
        controller.playback.invalidate(7, composition_id);
        let generation = controller.playback.generation;
        controller.playback.begin_request(AudioRequest {
            generation,
            revision: 7,
            composition_id,
            focus_frame: 456,
            secondary_frame: None,
        });
        let empty_window = PageWindow {
            start_frame: 0,
            end_frame: 0,
            secondary_start_frame: 0,
            secondary_end_frame: 0,
            total_frames: 0,
        };

        controller.accept_compile_completion(
            &mut vm,
            CompileResult {
                generation,
                revision: 7,
                composition_id,
                focus_frame: 123,
                secondary_frame: None,
                window: empty_window,
                result: Err("old request failed".into()),
            },
        );
        assert_eq!(controller.playback.request.unwrap().focus_frame, 456);
        assert!(controller.compile_retry_at.is_none());

        controller.accept_compile_completion(
            &mut vm,
            CompileResult {
                generation,
                revision: 7,
                composition_id,
                focus_frame: 456,
                secondary_frame: None,
                window: empty_window,
                result: Err("current request failed".into()),
            },
        );
        assert!(controller.playback.request.is_none());
        controller.compile_retry_at = Instant::now().checked_sub(Duration::from_millis(1));
        controller.schedule_audio_pages(&vm);
        assert_eq!(
            controller.playback.request.map(|value| value.revision),
            Some(7)
        );
        controller.close(&mut vm);
    }

    #[test]
    fn playback_regression_unrequested_completion_cannot_become_audible() {
        let (_directory, store) = store();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        let mut controller = NativeController::start(startup);
        let composition_id = vm.current_composition_id();
        controller.latest_revision = 7;
        controller.audio_revision = 7;
        controller.playback.invalidate(7, composition_id);
        let generation = controller.playback.generation;
        let renderer =
            gaw_audio::MemoryFrameSource::new(ChannelLayout::Stereo, vec![0.5_f32; 128]).unwrap();
        let snapshot = Arc::new(
            RenderSnapshot::new(7, 48_000, ChannelLayout::Stereo, 64, 0, Arc::new(renderer))
                .unwrap(),
        );
        controller.accept_compile_completion(
            &mut vm,
            CompileResult {
                generation,
                revision: 7,
                composition_id,
                focus_frame: 0,
                secondary_frame: None,
                window: PageWindow {
                    start_frame: 0,
                    end_frame: 64,
                    secondary_start_frame: 0,
                    secondary_end_frame: 0,
                    total_frames: 64,
                },
                result: Ok(snapshot),
            },
        );

        assert!(controller.playback.ready.is_none());
        assert!(controller.pending_audio.is_empty());
        controller.close(&mut vm);
    }

    #[test]
    fn accepted_edit_immediately_invalidates_and_requests_timeline_audio() {
        let (_directory, store) = store();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        let mut controller = NativeController::start(startup);
        controller
            .playback
            .invalidate(0, vm.current_composition_id());
        let old_generation = controller.playback.generation;

        vm.apply(Intent::SetBpm(98.0));
        controller.accept_updates(&mut vm);

        assert_eq!(controller.latest_revision, vm.revision());
        assert!(controller.playback.ready.is_none());
        assert!(controller.playback.generation > old_generation);
        let request = controller
            .playback
            .request
            .expect("exact revision requested");
        assert_eq!(request.revision, vm.revision());
        assert_eq!(request.focus_frame, transport_frame(&vm));
        assert_eq!(request.secondary_frame, loop_anchor(&vm));
        assert!(controller.pending_audio.iter().any(|command| matches!(
            command,
            RealtimeCommand::ActivateTimeline(TimelineActivation {
                generation,
                snapshot: None,
                ..
            }) if *generation == controller.playback.generation
        )));
        controller.close(&mut vm);
    }

    #[test]
    fn track_volume_edit_keeps_current_audio_until_replacement_is_ready() {
        let directory = tempfile::tempdir().unwrap();
        let project = DemoViewModel::demo().project().clone();
        let store = ProjectStore::create(directory.path().join("song"), &project).unwrap();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        let mut controller = NativeController::start(startup);
        controller
            .playback
            .invalidate(0, vm.current_composition_id());
        controller.pending_audio.clear();
        let old_generation = controller.playback.generation;

        vm.apply(Intent::SetTrackVolume {
            track: 0,
            volume_db: -12.0,
        });
        controller.accept_updates(&mut vm);

        assert!(controller.playback.generation > old_generation);
        assert_eq!(
            controller.playback.request.map(|request| request.revision),
            Some(vm.revision())
        );
        assert!(!controller.pending_audio.iter().any(|command| matches!(
            command,
            RealtimeCommand::ActivateTimeline(TimelineActivation { snapshot: None, .. })
        )));
        controller.close(&mut vm);
    }

    #[test]
    fn playback_regression_visual_timeline_sequence_is_authoritative() {
        let (directory, store) = store();
        let source = directory.path().join("authoritative.wav");
        write_audible_test_wav(&source, 4_800);
        let imported = store.import_media(source).unwrap();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        let mut controller = NativeController::start(startup);
        controller.ensure_playback_target(&mut vm);
        controller.pending_audio.clear();

        vm.apply(Intent::AddAssetClip {
            asset_id: imported.asset_id,
            beat: 0.0,
            track: None,
            tempo_sync: Some(gaw_core::TempoSync::None),
        });
        controller.accept_updates(&mut vm);
        let added = accept_exact_test_render(&mut controller, &mut vm, &store);
        assert!(snapshot_is_audible_at(
            added,
            beat_to_frame(0.1, 120.0, 48_000)
        ));

        let crate::model::Selection::Clip { track, clip } = vm.selection else {
            panic!("added clip should be selected");
        };
        let length = vm.current_composition().tracks[track].clips[clip].length;
        vm.apply(Intent::EditClip {
            track,
            clip,
            start: 4.0,
            length,
            target_track: track,
        });
        controller.accept_updates(&mut vm);
        let moved = accept_exact_test_render(&mut controller, &mut vm, &store);
        assert!(!snapshot_is_audible_at(
            Arc::clone(&moved),
            beat_to_frame(0.1, 120.0, 48_000)
        ));
        assert!(snapshot_is_audible_at(
            moved,
            beat_to_frame(4.1, 120.0, 48_000)
        ));

        vm.apply(Intent::EditClip {
            track,
            clip,
            start: 4.05,
            length: length - 0.05,
            target_track: track,
        });
        controller.accept_updates(&mut vm);
        let trimmed = accept_exact_test_render(&mut controller, &mut vm, &store);
        assert!(!snapshot_is_audible_at(
            Arc::clone(&trimmed),
            beat_to_frame(4.01, 120.0, 48_000)
        ));
        assert!(snapshot_is_audible_at(
            trimmed,
            beat_to_frame(4.1, 120.0, 48_000)
        ));

        vm.apply(Intent::DeleteClip { track, clip });
        controller.accept_updates(&mut vm);
        let deleted = accept_exact_test_render(&mut controller, &mut vm, &store);
        assert!(!snapshot_is_audible_at(
            deleted,
            beat_to_frame(4.1, 120.0, 48_000)
        ));
        controller.close(&mut vm);
    }

    #[test]
    fn playback_regression_live_edit_sequence_reaches_one_persistent_callback() {
        let (directory, store) = store();
        let source = directory.path().join("persistent.wav");
        write_audible_test_wav(&source, 4_800);
        let imported = store.import_media(source).unwrap();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        vm.transport.playing = true;
        let mut controller = NativeController::start(startup);
        controller.initialize_transport(&vm.transport);
        let (sender, mut engine) = command_queue(RealtimeEngineConfig::default(), 32, 32).unwrap();

        vm.apply(Intent::AddAssetClip {
            asset_id: imported.asset_id,
            beat: 0.0,
            track: None,
            tempo_sync: Some(gaw_core::TempoSync::None),
        });
        controller.accept_updates(&mut vm);
        let generation = controller.playback.generation;
        flush_test_audio(&mut controller, &sender);
        let mut output = vec![1.0_f32; 128 * 2];
        engine.process(&mut output);
        assert_eq!(sender.active_generation(), generation);
        assert_eq!(sender.audible_generation(), 0);
        assert!(sender.frame_position() > 0);
        assert!(output.iter().all(|sample| sample.abs() < f32::EPSILON));

        accept_exact_test_render(&mut controller, &mut vm, &store);
        flush_test_audio(&mut controller, &sender);
        engine.process(&mut output);
        assert_eq!(sender.audible_generation(), generation);
        assert!(output.iter().any(|sample| sample.abs() > 0.001));

        let crate::model::Selection::Clip { track, clip } = vm.selection else {
            panic!("added clip should be selected");
        };
        let length = vm.current_composition().tracks[track].clips[clip].length;
        vm.apply(Intent::EditClip {
            track,
            clip,
            start: 4.0,
            length,
            target_track: track,
        });
        controller.accept_updates(&mut vm);
        let moved_generation = controller.playback.generation;
        flush_test_audio(&mut controller, &sender);
        output.fill(1.0);
        engine.process(&mut output);
        assert_eq!(sender.active_generation(), moved_generation);
        assert_eq!(sender.audible_generation(), 0);
        assert!(output.iter().all(|sample| sample.abs() < f32::EPSILON));

        accept_exact_test_render(&mut controller, &mut vm, &store);
        flush_test_audio(&mut controller, &sender);
        engine.process(&mut output);
        assert_eq!(sender.audible_generation(), moved_generation);
        assert!(output.iter().all(|sample| sample.abs() < f32::EPSILON));
        sender
            .try_send(RealtimeCommand::Seek(beat_to_frame(4.1, 120.0, 48_000)))
            .unwrap();
        engine.process(&mut output);
        assert!(output.iter().any(|sample| sample.abs() > 0.001));

        vm.apply(Intent::EditClip {
            track,
            clip,
            start: 4.05,
            length: length - 0.05,
            target_track: track,
        });
        controller.accept_updates(&mut vm);
        flush_test_audio(&mut controller, &sender);
        engine.process(&mut output);
        accept_exact_test_render(&mut controller, &mut vm, &store);
        flush_test_audio(&mut controller, &sender);
        sender
            .try_send(RealtimeCommand::Seek(beat_to_frame(4.01, 120.0, 48_000)))
            .unwrap();
        output.fill(1.0);
        engine.process(&mut output);
        assert!(output.iter().all(|sample| sample.abs() < f32::EPSILON));
        sender
            .try_send(RealtimeCommand::Seek(beat_to_frame(4.1, 120.0, 48_000)))
            .unwrap();
        engine.process(&mut output);
        assert!(output.iter().any(|sample| sample.abs() > 0.001));

        vm.apply(Intent::DeleteClip { track, clip });
        controller.accept_updates(&mut vm);
        flush_test_audio(&mut controller, &sender);
        engine.process(&mut output);
        accept_exact_test_render(&mut controller, &mut vm, &store);
        flush_test_audio(&mut controller, &sender);
        output.fill(1.0);
        engine.process(&mut output);
        assert!(output.iter().all(|sample| sample.abs() < f32::EPSILON));
        controller.close(&mut vm);
    }

    #[test]
    fn playback_regression_stretched_trim_is_audible_at_its_visual_position() {
        let (directory, store) = store();
        let source = directory.path().join("stretch.wav");
        write_audible_test_wav(&source, 48_000);
        let imported = store.import_media(source).unwrap();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        let mut controller = NativeController::start(startup);
        vm.set_asset_tempo(0, Some(60.0), 0.0);
        vm.apply(Intent::AddAssetClip {
            asset_id: imported.asset_id,
            beat: 0.0,
            track: None,
            tempo_sync: Some(gaw_core::TempoSync::Stretch),
        });
        controller.accept_updates(&mut vm);
        let added = accept_exact_test_render(&mut controller, &mut vm, &store);
        assert!(snapshot_is_audible_at(
            added,
            beat_to_frame(0.2, 120.0, 48_000)
        ));

        let crate::model::Selection::Clip { track, clip } = vm.selection else {
            panic!("added clip should be selected");
        };
        let length = vm.current_composition().tracks[track].clips[clip].length;
        vm.apply(Intent::EditClip {
            track,
            clip,
            start: 0.25,
            length: length - 0.25,
            target_track: track,
        });
        controller.accept_updates(&mut vm);
        let trimmed = accept_exact_test_render(&mut controller, &mut vm, &store);
        assert!(!snapshot_is_audible_at(
            Arc::clone(&trimmed),
            beat_to_frame(0.1, 120.0, 48_000)
        ));
        assert!(snapshot_is_audible_at(
            trimmed,
            beat_to_frame(0.4, 120.0, 48_000)
        ));
        controller.close(&mut vm);
    }

    #[test]
    fn playback_regression_worker_publishes_playhead_page_before_prefetch() {
        let (directory, store) = store();
        let source = directory.path().join("paged.wav");
        write_audible_test_wav(&source, AUDIO_PAGE_FRAMES * 3);
        let imported = store.import_media(source).unwrap();
        let mut vm = DemoViewModel::from_project(store.load_project().unwrap()).unwrap();
        vm.apply(Intent::AddAssetClip {
            asset_id: imported.asset_id,
            beat: 0.0,
            track: None,
            tempo_sync: Some(gaw_core::TempoSync::None),
        });
        let job = CompileJob {
            generation: 1,
            revision: vm.revision(),
            composition_id: vm.current_composition_id(),
            store: store.clone(),
            project: Arc::new(vm.project().clone()),
            focus_frame: 0,
            secondary_frame: None,
        };
        let state = Arc::new((Mutex::new(CompileState::default()), Condvar::new()));
        let mut compiler = StorePlaybackCompiler::default();
        let mut prepared = None;

        let (first_window, first) =
            prepare_snapshot_window(&state, &job, &mut compiler, &mut prepared).unwrap();
        assert_eq!(first_window.start_frame, 0);
        assert_eq!(first_window.end_frame, AUDIO_PAGE_FRAMES as u64);
        assert_eq!(prepared.as_ref().unwrap().pages.len(), 1);
        assert!(snapshot_is_audible_at(Arc::new(first.unwrap()), 128));

        let (second_window, second) =
            prepare_snapshot_window(&state, &job, &mut compiler, &mut prepared).unwrap();
        assert_eq!(
            second_window.end_frame,
            (AUDIO_PAGE_FRAMES as u64).saturating_mul(2)
        );
        assert_eq!(prepared.as_ref().unwrap().pages.len(), 2);
        assert!(snapshot_is_audible_at(
            Arc::new(second.unwrap()),
            AUDIO_PAGE_FRAMES as u64 + 128
        ));
    }

    #[test]
    fn playback_regression_navigation_changes_the_audio_composition_key() {
        let (_directory, store) = store();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::demo();
        let mut controller = NativeController::start(startup);
        controller.ensure_playback_target(&mut vm);
        let root_generation = controller.playback.generation;
        let root = vm.current_composition_id();

        vm.apply(Intent::EnterChild { track: 2, clip: 0 });
        let child = vm.current_composition_id();
        assert_ne!(child, root);
        controller.ensure_playback_target(&mut vm);

        assert!(controller.playback.generation > root_generation);
        assert_eq!(controller.playback.composition_id, Some(child));
        assert_eq!(
            controller
                .playback
                .request
                .expect("child compile")
                .composition_id,
            child
        );
        controller.close(&mut vm);
    }

    #[test]
    fn playback_regression_seek_retargets_same_revision_compile() {
        let (_directory, store) = store();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        let mut controller = NativeController::start(startup);
        controller.latest_revision = 7;
        controller.audio_revision = 7;
        let composition_id = vm.current_composition_id();
        controller.playback.invalidate(7, composition_id);
        controller.playback.begin_request(AudioRequest {
            generation: controller.playback.generation,
            revision: 7,
            composition_id,
            focus_frame: AUDIO_PAGE_FRAMES as u64 * 100,
            secondary_frame: None,
        });
        controller.last_transport.playhead = 100.0;
        vm.transport.playhead = 0.0;
        vm.transport.playing = false;

        controller.sync_transport(&vm);

        assert_eq!(controller.playback.request.unwrap().focus_frame, 0);
        controller.close(&mut vm);
    }

    #[test]
    fn playback_regression_advancing_clock_does_not_starve_compile() {
        let (_directory, store) = store();
        let startup = NativeStartup::open(store.root(), RecoveryPolicy::Recover).unwrap();
        let mut vm = DemoViewModel::from_project(startup.project().clone()).unwrap();
        let mut controller = NativeController::start(startup);
        controller.latest_revision = 7;
        controller.audio_revision = 7;
        let composition_id = vm.current_composition_id();
        controller.playback.invalidate(7, composition_id);
        vm.transport.playing = true;
        controller.last_transport = TransportView::from(&vm.transport);
        let original = AudioRequest {
            generation: controller.playback.generation,
            revision: 7,
            composition_id,
            focus_frame: 0,
            secondary_frame: loop_anchor(&vm),
        };
        controller.playback.begin_request(original);

        // More than the old 8-page (~10.9 second) retarget threshold, reached through
        // small callback-style updates rather than an explicit transport jump.
        for step in 1..=64 {
            vm.transport.playhead = step as f32 * 0.25;
            controller.sync_transport(&vm);
            controller.schedule_audio_pages(&vm);
        }

        assert_eq!(controller.playback.request, Some(original));
        controller.close(&mut vm);
    }

    #[test]
    fn page_windows_move_beyond_initial_budget_and_stay_bounded() {
        let page = AUDIO_PAGE_FRAMES as u64;
        let total = page * 200 + 17;
        let focus = page * 140;
        let window = page_window(total, 2, 0, focus, None);
        let bytes_per_page = AUDIO_PAGE_FRAMES * 2 * size_of::<f32>();
        let pages = window.end_frame.div_ceil(page) - window.start_frame / page;
        assert!(window.contains(focus));
        assert!(window.start_frame > 0);
        assert!(pages as usize * bytes_per_page <= AUDIO_PAGE_BYTES);

        let end = page_window(total, 2, 0, u64::MAX, None);
        assert!(end.contains(total - 1));
        assert!(end.contains(total));
        assert_eq!(end.end_frame, total);
        assert_eq!(page_window(0, 2, 0, u64::MAX, None).end_frame, 0);
    }

    #[test]
    fn distant_loop_start_is_reserved_inside_the_same_page_budget() {
        let page = AUDIO_PAGE_FRAMES as u64;
        let total = page * 200;
        let focus = page * 150;
        let loop_start = page * 2;
        let window = page_window(total, 2, 0, focus, Some(loop_start));
        let primary = (window.end_frame - window.start_frame).div_ceil(page);
        let secondary = (window.secondary_end_frame - window.secondary_start_frame).div_ceil(page);
        assert!(window.contains(focus));
        assert!(window.contains(loop_start));
        assert!(primary + secondary <= 64);
    }

    #[test]
    fn playback_lead_window_still_contains_the_current_playhead() {
        let page = AUDIO_PAGE_FRAMES as u64;
        let current = page * 100;
        let focus = current + AUDIO_PREPARE_LEAD_PAGES * page;
        let window = page_window(page * 200, 2, 0, focus, Some(page * 2));
        assert!(window.contains(current));
        assert!(window.contains(focus));
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
    fn clean_close_ack_is_independent_of_an_undrained_event_queue() {
        let (_directory, store) = store();
        let mut worker = ProjectWorker::spawn(ProjectSession::open(store.clone()).unwrap());
        for revision in 1..=40 {
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
    fn observed_default_change_opens_a_new_generation_without_stream_error() {
        let backend = cpal::ALL_HOSTS[0];
        let initial = cpal::DeviceId(backend, "default-a".into());
        let mut recovery = DeviceRecoveryController::new(
            OutputDeviceSelection::FollowDefault { backend },
            DeviceRecoveryPolicy::default(),
            StreamGeneration::new(11),
            initial,
        )
        .unwrap();

        assert_eq!(
            apply_device_observation(
                &mut recovery,
                &DeviceObservationResult {
                    generation: StreamGeneration::new(10),
                    observation: DeviceObservation {
                        default_output: Some(cpal::DeviceId(backend, "default-b".into())),
                        pinned_available: false,
                    },
                },
            ),
            DeviceRecoveryAction::None
        );
        let action = apply_device_observation(
            &mut recovery,
            &DeviceObservationResult {
                generation: StreamGeneration::new(11),
                observation: DeviceObservation {
                    default_output: Some(cpal::DeviceId(backend, "default-b".into())),
                    pinned_available: false,
                },
            },
        );
        assert_eq!(
            action,
            DeviceRecoveryAction::Open {
                generation: StreamGeneration::new(12),
                target: RecoveryTarget::Default { backend },
                attempt: 1,
            }
        );
    }

    #[test]
    fn saturated_fatal_notification_enters_existing_recovery_policy_once() {
        let backend = cpal::ALL_HOSTS[0];
        let generation = StreamGeneration::new(5);
        let mut recovery = DeviceRecoveryController::new(
            OutputDeviceSelection::FollowDefault { backend },
            DeviceRecoveryPolicy::default(),
            generation,
            cpal::DeviceId(backend, "default".into()),
        )
        .unwrap();
        let (sender, receiver) = stream_notification_channel(1).unwrap();
        sender
            .try_send(generation, cpal::StreamError::BufferUnderrun)
            .unwrap();
        sender
            .try_send(generation, cpal::StreamError::DeviceNotAvailable)
            .unwrap();

        assert_eq!(
            recovery.handle_notification(&receiver.try_recv().unwrap()),
            DeviceRecoveryAction::Open {
                generation: StreamGeneration::new(6),
                target: RecoveryTarget::Default { backend },
                attempt: 1,
            }
        );
        assert_eq!(
            recovery.handle_notification(&receiver.try_recv().unwrap()),
            DeviceRecoveryAction::None
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn replacement_error_waits_for_its_generation_to_be_promoted() {
        let backend = cpal::ALL_HOSTS[0];
        let mut recovery = DeviceRecoveryController::new(
            OutputDeviceSelection::FollowDefault { backend },
            DeviceRecoveryPolicy::default(),
            StreamGeneration::new(5),
            cpal::DeviceId(backend, "default-a".into()),
        )
        .unwrap();
        let DeviceRecoveryAction::Open { generation, .. } = recovery.observe(&DeviceObservation {
            default_output: Some(cpal::DeviceId(backend, "default-b".into())),
            pinned_available: false,
        }) else {
            panic!("default change should open a replacement")
        };
        let (sender, receiver) = stream_notification_channel(1).unwrap();
        sender
            .try_send(generation, cpal::StreamError::DeviceNotAvailable)
            .unwrap();

        assert!(recovery.stream_started(generation, cpal::DeviceId(backend, "default-b".into())));
        assert_eq!(
            recovery.handle_notification(&receiver.try_recv().unwrap()),
            DeviceRecoveryAction::Open {
                generation: StreamGeneration::new(7),
                target: RecoveryTarget::Default { backend },
                attempt: 1,
            }
        );
    }

    #[test]
    fn beat_mapping_supports_seek_and_loop_wrap_commands() {
        assert_eq!(beat_to_frame(4.0, 120.0, 48_000), 96_000);
        assert_eq!(beat_to_frame(f32::NAN, 120.0, 48_000), 0);
        assert!((frame_to_beat(96_000, 120.0, 48_000) - 4.0).abs() < f32::EPSILON);
    }

    #[test]
    fn timeline_activation_reasserts_transport_atomically() {
        let (_directory, store) = store();
        let mut vm = DemoViewModel::from_project(store.load_project().unwrap()).unwrap();
        vm.transport.playhead = 2.0;
        vm.transport.playing = true;

        let activation = timeline_activation(&vm, 7, None);
        assert_eq!(activation.generation, 7);
        assert_eq!(activation.frame, 48_000);
        assert!(activation.playing);
        assert!(activation.snapshot.is_none());

        vm.transport.playing = false;
        assert!(!timeline_activation(&vm, 8, None).playing);
    }

    #[test]
    fn snapshotless_authoritative_timeline_keeps_the_clock_advancing() {
        let (_directory, store) = store();
        let mut vm = DemoViewModel::from_project(store.load_project().unwrap()).unwrap();
        vm.transport.playing = true;
        let (sender, mut engine) = command_queue(RealtimeEngineConfig::default(), 8, 2).unwrap();
        sender
            .try_send(RealtimeCommand::ActivateTimeline(timeline_activation(
                &vm, 7, None,
            )))
            .unwrap();
        let mut output = vec![1.0; 256 * 2];

        engine.process(&mut output);

        assert_eq!(sender.frame_position(), 256);
        assert_eq!(sender.active_generation(), 7);
        assert_eq!(sender.audible_generation(), 0);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn seek_acknowledgement_cannot_freeze_after_playback_passes_the_target() {
        let forward = PendingSeek {
            target: 48_000,
            observed_before: 0,
        };
        assert!(seek_was_observed(forward, 60_000));

        let backward = PendingSeek {
            target: 12_000,
            observed_before: 96_000,
        };
        assert!(seek_was_observed(backward, 24_000));
        assert!(!seek_was_observed(backward, 100_000));
    }

    #[test]
    fn preview_seconds_mapping_rounds_and_clamps() {
        assert_eq!(seconds_to_frame(-1.0, 48_000, 96_000), 0);
        assert_eq!(seconds_to_frame(f64::NAN, 48_000, 96_000), 0);
        assert_eq!(seconds_to_frame(0.5, 48_000, 96_000), 24_000);
        assert_eq!(seconds_to_frame(3.0, 48_000, 96_000), 96_000);
    }
}
