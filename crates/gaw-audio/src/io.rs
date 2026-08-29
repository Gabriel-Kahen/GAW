//! Real-time-safe device I/O and deterministic offline WAV rendering.

use std::{
    fmt,
    io::{Seek, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use audioadapter_buffers::direct::InterleavedSlice;
use cpal::{
    BufferSize, FromSample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use thiserror::Error;

use crate::{
    assets::{AssetError, FrameSource, MemoryFrameSource, WavFrameSource},
    device::{DeviceObservation, OutputDeviceSelection},
    render::ChannelLayout,
};

const MAX_MEMORY_SNAPSHOT_BYTES: usize = 1 << 30;

/// A mutable, interleaved block of `f32` audio.
///
/// Samples for frame `n` occupy `n * channels..(n + 1) * channels`.
#[derive(Debug)]
pub struct SampleBlock<'a> {
    samples: &'a mut [f32],
    layout: ChannelLayout,
}

impl<'a> SampleBlock<'a> {
    /// Wraps a complete number of interleaved frames.
    ///
    /// # Errors
    ///
    /// Returns [`BlockError::IncompleteFrame`] when the sample count is not a
    /// multiple of the layout's channel count.
    pub fn new(samples: &'a mut [f32], layout: ChannelLayout) -> Result<Self, BlockError> {
        let channels = channel_count(layout);
        if !samples.len().is_multiple_of(channels) {
            return Err(BlockError::IncompleteFrame {
                samples: samples.len(),
                channels,
            });
        }
        Ok(Self { samples, layout })
    }

    fn validated(samples: &'a mut [f32], layout: ChannelLayout) -> Self {
        debug_assert_eq!(samples.len() % channel_count(layout), 0);
        Self { samples, layout }
    }

    /// Interleaved samples.
    pub fn samples(&self) -> &[f32] {
        self.samples
    }

    /// Mutable interleaved samples.
    pub fn samples_mut(&mut self) -> &mut [f32] {
        self.samples
    }

    /// The block's channel layout.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Number of complete sample frames.
    pub fn frames(&self) -> usize {
        self.samples.len() / channel_count(self.layout)
    }

    /// Fill the block with silence.
    pub fn clear(&mut self) {
        self.samples.fill(0.0);
    }
}

/// A prepared renderer that is safe to invoke from an audio callback.
///
/// Implementations must overwrite every sample in `output` and must not lock,
/// access files, parse project data, or perform allocation with an unbounded
/// execution time. Calls are positional and must be deterministic.
pub trait RealtimeRender: Send + Sync + 'static {
    /// Render `output.frames()` frames beginning at `start_frame`.
    fn render(&self, start_frame: u64, output: &mut SampleBlock<'_>);

    /// Returns a prepared post-fader track peak at `frame`, when this renderer
    /// carries per-track metering sidecars.
    fn track_peak_at(&self, _track_id: &str, _frame: u64) -> Option<f32> {
        None
    }
}

impl RealtimeRender for MemoryFrameSource {
    fn render(&self, start_frame: u64, output: &mut SampleBlock<'_>) {
        output.clear();
        debug_assert_eq!(self.channel_layout(), output.layout());
        let _ = self.read_interleaved(start_frame, output.samples_mut());
    }
}

/// Immutable, prepared audio consumed by the real-time engine.
pub struct RenderSnapshot {
    revision: u64,
    sample_rate: u32,
    layout: ChannelLayout,
    main_frames: u64,
    tail_frames: u64,
    renderer: Arc<dyn RealtimeRender>,
}

impl RenderSnapshot {
    /// Creates a snapshot. `tail_frames` is included after `main_frames`.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero sample rate or if body plus tail overflows.
    pub fn new(
        revision: u64,
        sample_rate: u32,
        layout: ChannelLayout,
        main_frames: u64,
        tail_frames: u64,
        renderer: Arc<dyn RealtimeRender>,
    ) -> Result<Self, SnapshotError> {
        if sample_rate == 0 {
            return Err(SnapshotError::ZeroSampleRate);
        }
        main_frames
            .checked_add(tail_frames)
            .ok_or(SnapshotError::LengthOverflow)?;
        Ok(Self {
            revision,
            sample_rate,
            layout,
            main_frames,
            tail_frames,
            renderer,
        })
    }

    /// Monotonically changing render revision chosen by the caller.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Snapshot sample rate.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Snapshot channel layout.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Composition body length, excluding the tail.
    pub fn main_frames(&self) -> u64 {
        self.main_frames
    }

    /// Finite rendered tail length.
    pub fn tail_frames(&self) -> u64 {
        self.tail_frames
    }

    /// Total renderable frames, including the tail.
    pub fn total_frames(&self) -> u64 {
        self.main_frames + self.tail_frames
    }

    /// Returns the prepared post-track-effects, post-fader peak for the bin
    /// containing `frame`.
    ///
    /// Sparse snapshots return `None` when the frame is not resident or the
    /// track is not part of the rendered root composition.
    pub fn track_peak_at(&self, track_id: &str, frame: u64) -> Option<f32> {
        self.renderer.track_peak_at(track_id, frame)
    }

    pub(crate) fn render_native(&self, start_frame: u64, output: &mut [f32]) {
        output.fill(0.0);
        let channels = channel_count(self.layout);
        let requested_frames = output.len() / channels;
        let available = self.total_frames().saturating_sub(start_frame);
        let active_frames = usize::try_from(available)
            .unwrap_or(usize::MAX)
            .min(requested_frames);
        if active_frames == 0 {
            return;
        }
        let active_samples = active_frames * channels;
        let mut block = SampleBlock::validated(&mut output[..active_samples], self.layout);
        self.renderer.render(start_frame, &mut block);
    }
}

/// Fully decodes a WAV into immutable memory and builds a callback-safe snapshot.
///
/// This performs filesystem access and allocation and must run off the audio and UI threads.
/// Preview allocations are capped at one GiB so malformed metadata cannot request unbounded
/// process memory.
///
/// # Errors
/// Returns an error when the WAV is invalid or unreadable, its declared payload exceeds the
/// memory limit, allocation fails, decoding ends early, or snapshot metadata is invalid.
pub fn load_wav_memory_snapshot(
    path: impl Into<std::path::PathBuf>,
    revision: u64,
) -> Result<RenderSnapshot, MemorySnapshotError> {
    let source = WavFrameSource::open(path)?;
    let layout = source.channel_layout();
    let frames = source.frame_count();
    let channels = layout.channels();
    let sample_count = usize::try_from(frames)
        .ok()
        .and_then(|frames| frames.checked_mul(channels))
        .ok_or(MemorySnapshotError::AllocationTooLarge)?;
    let byte_count = sample_count
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or(MemorySnapshotError::AllocationTooLarge)?;
    if byte_count > MAX_MEMORY_SNAPSHOT_BYTES {
        return Err(MemorySnapshotError::AllocationTooLarge);
    }
    let mut samples = Vec::new();
    samples
        .try_reserve_exact(sample_count)
        .map_err(|_| MemorySnapshotError::AllocationFailed)?;
    samples.resize(sample_count, 0.0);
    let mut position = 0_u64;
    while position < frames {
        let start = usize::try_from(position)
            .map_err(|_| MemorySnapshotError::AllocationTooLarge)?
            .checked_mul(channels)
            .ok_or(MemorySnapshotError::AllocationTooLarge)?;
        let remaining_frames = usize::try_from(frames - position)
            .unwrap_or(usize::MAX)
            .min(16_384);
        let end = start
            .checked_add(remaining_frames * channels)
            .ok_or(MemorySnapshotError::AllocationTooLarge)?;
        let read = source.read_interleaved(position, &mut samples[start..end])?;
        if read == 0 {
            return Err(MemorySnapshotError::SourceEndedEarly(position));
        }
        if read > remaining_frames {
            return Err(MemorySnapshotError::SourceOverrun);
        }
        position += read as u64;
    }
    let sample_rate = source.sample_rate();
    let memory = Arc::new(MemoryFrameSource::new(layout, Arc::<[f32]>::from(samples))?);
    Ok(RenderSnapshot::new(
        revision,
        sample_rate,
        layout,
        frames,
        0,
        memory,
    )?)
}

impl fmt::Debug for RenderSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RenderSnapshot")
            .field("revision", &self.revision)
            .field("sample_rate", &self.sample_rate)
            .field("layout", &self.layout)
            .field("main_frames", &self.main_frames)
            .field("tail_frames", &self.tail_frames)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub enum MemorySnapshotError {
    #[error("preview audio exceeds the one GiB memory limit")]
    AllocationTooLarge,
    #[error("could not allocate memory for preview audio")]
    AllocationFailed,
    #[error("preview source stopped unexpectedly at frame {0}")]
    SourceEndedEarly(u64),
    #[error("preview source returned more audio than requested")]
    SourceOverrun,
    #[error(transparent)]
    Asset(#[from] AssetError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

/// Commands accepted by [`RealtimeEngine`].
#[derive(Debug)]
pub enum RealtimeCommand {
    /// Atomically activate one canonical timeline generation and its complete
    /// transport state. A missing snapshot advances the clock silently until
    /// the matching prepared render is activated.
    ActivateTimeline(TimelineActivation),
    /// Atomically make a prepared render revision current.
    /// Explicitly enters asset-preview playback with a non-timeline snapshot.
    ActivatePreview(Arc<RenderSnapshot>),
    /// Remove the current snapshot and output silence.
    ClearPreview,
    /// Begin playback at the current frame.
    Play,
    /// Pause without changing the current frame.
    Pause,
    /// Pause and return to frame zero.
    Stop,
    /// Move the playhead to an absolute frame.
    Seek(u64),
    /// Enable or disable a prevalidated sample-accurate loop range.
    SetLoop(Option<RealtimeLoopRange>),
    /// Set linear output gain. Non-finite values become silence.
    SetGain(f32),
    /// Configure the non-exported project metronome.
    SetMetronome(RealtimeMetronome),
}

/// One atomic callback-boundary activation of canonical timeline state.
#[derive(Debug)]
pub struct TimelineActivation {
    pub generation: u64,
    pub snapshot: Option<Arc<RenderSnapshot>>,
    /// Keep callback-owned transport state when replacing a snapshot for the
    /// already-active generation.
    pub preserve_transport: bool,
    pub sample_rate: u32,
    pub total_frames: u64,
    pub frame: u64,
    pub playing: bool,
    pub loop_range: Option<RealtimeLoopRange>,
    pub metronome: RealtimeMetronome,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RealtimeMetronome {
    pub enabled: bool,
    pub bpm: f64,
    pub numerator: u8,
    pub denominator: u8,
    /// Linear click gain in the range 0..=1.
    pub gain: f32,
}

impl Default for RealtimeMetronome {
    fn default() -> Self {
        Self {
            enabled: false,
            bpm: 120.0,
            numerator: 4,
            denominator: 4,
            gain: 0.7,
        }
    }
}

/// Prevalidated half-open snapshot-frame loop range `[start_frame, end_frame)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RealtimeLoopRange {
    pub start_frame: u64,
    pub end_frame: u64,
}

impl RealtimeLoopRange {
    /// Creates a non-empty loop range.
    ///
    /// # Errors
    ///
    /// Returns [`RealtimeLoopRangeError::EmptyOrReversed`] unless start is before end.
    pub const fn new(start_frame: u64, end_frame: u64) -> Result<Self, RealtimeLoopRangeError> {
        if start_frame >= end_frame {
            return Err(RealtimeLoopRangeError::EmptyOrReversed);
        }
        Ok(Self {
            start_frame,
            end_frame,
        })
    }

    pub const fn frames(self) -> u64 {
        self.end_frame - self.start_frame
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum RealtimeLoopRangeError {
    #[error("loop start must be before loop end")]
    EmptyOrReversed,
}

/// Copyable transport state owned by the audio callback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TransportState {
    /// Current absolute frame.
    pub frame: u64,
    /// Whether transport advances while rendering.
    pub playing: bool,
    /// Linear output gain.
    pub gain: f32,
    /// Active sample-accurate half-open loop range.
    pub loop_range: Option<RealtimeLoopRange>,
}

impl Default for TransportState {
    fn default() -> Self {
        Self {
            frame: 0,
            playing: false,
            gain: 1.0,
            loop_range: None,
        }
    }
}

/// Non-real-time endpoint of a bounded command queue.
#[derive(Debug)]
pub struct CommandSender {
    commands: Sender<RealtimeCommand>,
    retired: Receiver<Arc<RenderSnapshot>>,
    frame_position: Arc<AtomicU64>,
    output_peak: Arc<AtomicU32>,
    active_generation: Arc<AtomicU64>,
    audible_generation: Arc<AtomicU64>,
    desired_generation: Arc<AtomicU64>,
}

impl CommandSender {
    /// Immediately suppresses timeline audio older than `generation`.
    ///
    /// This atomic control-plane publication does not wait for command-queue
    /// space, so an edit cannot leak another stale timeline block while its
    /// complete activation is pending.
    pub fn invalidate_timeline(&self, generation: u64) {
        let previous = self
            .desired_generation
            .fetch_max(generation, Ordering::AcqRel);
        if generation > previous {
            self.audible_generation.store(0, Ordering::Release);
        }
    }

    /// Attempts to enqueue without waiting.
    ///
    /// # Errors
    ///
    /// Returns the unsent command when the queue is full or disconnected.
    pub fn try_send(&self, command: RealtimeCommand) -> Result<(), CommandSendError> {
        if let RealtimeCommand::ActivateTimeline(activation) = &command {
            self.invalidate_timeline(activation.generation);
        }
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(command) => CommandSendError::Full(command),
                TrySendError::Disconnected(command) => CommandSendError::Disconnected(command),
            })
    }

    /// Drops retired snapshots on the calling, non-real-time thread.
    pub fn reclaim_retired(&self) -> usize {
        let mut count = 0;
        while let Ok(snapshot) = self.retired.try_recv() {
            drop(snapshot);
            count += 1;
        }
        count
    }

    /// Latest callback-owned snapshot frame, readable from the app/control thread.
    pub fn frame_position(&self) -> u64 {
        self.frame_position.load(Ordering::Relaxed)
    }

    /// Takes the maximum absolute post-gain project sample published since the
    /// previous read. Monitoring-only metronome audio is excluded.
    pub fn output_peak(&self) -> f32 {
        f32::from_bits(self.output_peak.swap(0.0_f32.to_bits(), Ordering::Relaxed))
    }

    /// Latest atomically activated canonical timeline generation.
    pub fn active_generation(&self) -> u64 {
        self.active_generation.load(Ordering::Acquire)
    }

    /// Timeline generation currently backed by an installed render artifact.
    /// Zero means the canonical clock is active without timeline audio.
    pub fn audible_generation(&self) -> u64 {
        self.audible_generation.load(Ordering::Acquire)
    }
}

/// A command that could not be enqueued. The command is returned to the caller.
#[derive(Debug)]
pub enum CommandSendError {
    /// The bounded queue has no free slot.
    Full(RealtimeCommand),
    /// The audio engine has been dropped.
    Disconnected(RealtimeCommand),
}

/// Fixed callback configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RealtimeEngineConfig {
    /// Internal and device sample rate.
    pub sample_rate: u32,
    /// Interleaved device layout.
    pub output_layout: ChannelLayout,
    /// Largest block rendered in one callback iteration.
    pub maximum_block_frames: usize,
    /// Upper bound on commands applied per rendered block.
    pub maximum_commands_per_block: usize,
}

impl Default for RealtimeEngineConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            output_layout: ChannelLayout::Stereo,
            maximum_block_frames: 1_024,
            maximum_commands_per_block: 64,
        }
    }
}

/// Real-time half of a command queue and its preallocated renderer state.
pub struct RealtimeEngine {
    config: RealtimeEngineConfig,
    commands: Receiver<RealtimeCommand>,
    retired: Sender<Arc<RenderSnapshot>>,
    pending_command: Option<RealtimeCommand>,
    snapshot: Option<Arc<RenderSnapshot>>,
    playback_source: PlaybackSource,
    transport: TransportState,
    metronome: RealtimeMetronome,
    source_position: f64,
    timeline_sample_rate: u32,
    timeline_total_frames: u64,
    native_scratch: Box<[f32]>,
    frame_position: Arc<AtomicU64>,
    output_peak: Arc<AtomicU32>,
    active_generation: Arc<AtomicU64>,
    audible_generation: Arc<AtomicU64>,
    desired_generation: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PlaybackSource {
    Timeline,
    Preview,
}

/// Creates the bounded command channel and its preallocated real-time engine.
///
/// # Errors
///
/// Returns an error when a capacity or fixed engine limit is zero, or when the
/// required scratch-buffer size cannot be represented.
pub fn command_queue(
    config: RealtimeEngineConfig,
    command_capacity: usize,
    retirement_capacity: usize,
) -> Result<(CommandSender, RealtimeEngine), EngineConfigError> {
    RealtimeEngine::new(config, command_capacity, retirement_capacity)
}

impl RealtimeEngine {
    /// Constructs an engine and its bounded non-real-time command endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when a capacity or fixed engine limit is zero, or when
    /// the required scratch-buffer size cannot be represented.
    pub fn new(
        config: RealtimeEngineConfig,
        command_capacity: usize,
        retirement_capacity: usize,
    ) -> Result<(CommandSender, Self), EngineConfigError> {
        if config.sample_rate == 0 {
            return Err(EngineConfigError::ZeroSampleRate);
        }
        if config.maximum_block_frames == 0 {
            return Err(EngineConfigError::ZeroMaximumBlockFrames);
        }
        if config.maximum_commands_per_block == 0 {
            return Err(EngineConfigError::ZeroMaximumCommands);
        }
        if command_capacity == 0 {
            return Err(EngineConfigError::ZeroCommandCapacity);
        }
        if retirement_capacity == 0 {
            return Err(EngineConfigError::ZeroRetirementCapacity);
        }

        let (command_tx, command_rx) = crossbeam_channel::bounded(command_capacity);
        let (retired_tx, retired_rx) = crossbeam_channel::bounded(retirement_capacity);
        let frame_position = Arc::new(AtomicU64::new(0));
        let output_peak = Arc::new(AtomicU32::new(0.0_f32.to_bits()));
        let active_generation = Arc::new(AtomicU64::new(0));
        let audible_generation = Arc::new(AtomicU64::new(0));
        let desired_generation = Arc::new(AtomicU64::new(0));
        // Supports callback-local adaptation from snapshots up to four times
        // the device rate, plus interpolation lookahead.
        let scratch_samples = config
            .maximum_block_frames
            .checked_mul(8)
            .and_then(|samples| samples.checked_add(4))
            .ok_or(EngineConfigError::ScratchSizeOverflow)?;
        let engine = Self {
            config,
            commands: command_rx,
            retired: retired_tx,
            pending_command: None,
            snapshot: None,
            playback_source: PlaybackSource::Timeline,
            transport: TransportState::default(),
            metronome: RealtimeMetronome::default(),
            source_position: 0.0,
            timeline_sample_rate: config.sample_rate,
            timeline_total_frames: 0,
            native_scratch: vec![0.0; scratch_samples].into_boxed_slice(),
            frame_position: Arc::clone(&frame_position),
            output_peak: Arc::clone(&output_peak),
            active_generation: Arc::clone(&active_generation),
            audible_generation: Arc::clone(&audible_generation),
            desired_generation: Arc::clone(&desired_generation),
        };
        let sender = CommandSender {
            commands: command_tx,
            retired: retired_rx,
            frame_position,
            output_peak,
            active_generation,
            audible_generation,
            desired_generation,
        };
        Ok((sender, engine))
    }

    /// Fixed engine configuration.
    pub fn config(&self) -> RealtimeEngineConfig {
        self.config
    }

    /// Current callback-owned state. Primarily useful for tests and offline hosts.
    pub fn transport(&self) -> TransportState {
        self.transport
    }

    /// Current snapshot revision, if installed.
    pub fn snapshot_revision(&self) -> Option<u64> {
        self.snapshot.as_ref().map(|snapshot| snapshot.revision())
    }

    /// Current canonical timeline generation, acknowledged by the callback.
    pub fn active_generation(&self) -> u64 {
        self.active_generation.load(Ordering::Acquire)
    }

    /// Fill one interleaved output block.
    ///
    /// This method allocates neither heap memory nor locks. Oversized or
    /// incomplete buffers are cleared and rejected.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    pub fn process(&mut self, output: &mut [f32]) -> ProcessStatus {
        let output_channels = channel_count(self.config.output_layout);
        if !output.len().is_multiple_of(output_channels) {
            output.fill(0.0);
            self.clear_output_peak();
            return ProcessStatus::IncompleteFrame;
        }
        let frames = output.len() / output_channels;
        if frames > self.config.maximum_block_frames {
            output.fill(0.0);
            self.clear_output_peak();
            return ProcessStatus::BlockTooLarge;
        }

        self.apply_commands();
        self.frame_position
            .store(self.transport.frame, Ordering::Relaxed);
        output.fill(0.0);
        if !self.transport.playing || frames == 0 {
            self.clear_output_peak();
            return ProcessStatus::Silence;
        }

        let ratio = f64::from(self.timeline_sample_rate) / f64::from(self.config.sample_rate);
        let mut output_frame = 0;
        let mut project_peak = 0.0_f32;
        while output_frame < frames {
            self.source_position =
                normalize_loop_position(self.source_position, self.transport.loop_range);
            let segment_frames = loop_segment_frames(
                self.source_position,
                ratio,
                frames - output_frame,
                self.transport.loop_range,
            );
            let output_start = output_frame * output_channels;
            let output_end = (output_frame + segment_frames) * output_channels;
            let generation_is_current = self.playback_source == PlaybackSource::Preview
                || self.active_generation.load(Ordering::Relaxed)
                    == self.desired_generation.load(Ordering::Acquire);
            if generation_is_current
                && let Some(snapshot) = self.snapshot.as_ref()
                && !render_realtime_segment(
                    snapshot,
                    &mut self.native_scratch,
                    self.config.output_layout,
                    self.source_position,
                    ratio,
                    self.transport.loop_range,
                    &mut output[output_start..output_end],
                )
            {
                output.fill(0.0);
                self.clear_output_peak();
                return ProcessStatus::SampleRateMismatch;
            }
            project_peak = project_peak.max(block_peak(&output[output_start..output_end]));
            if generation_is_current {
                mix_metronome_segment(
                    &mut output[output_start..output_end],
                    self.config.output_layout,
                    self.source_position,
                    ratio,
                    self.timeline_sample_rate,
                    self.metronome,
                );
            }
            self.source_position += segment_frames as f64 * ratio;
            output_frame += segment_frames;
        }
        apply_gain(output, self.transport.gain);
        self.publish_output_peak(project_peak * self.transport.gain);

        self.source_position =
            normalize_loop_position(self.source_position, self.transport.loop_range);
        self.transport.frame = self.source_position.floor() as u64;
        if self.transport.loop_range.is_none() && self.transport.frame >= self.timeline_total_frames
        {
            self.transport.frame = self.timeline_total_frames;
            self.source_position = self.transport.frame as f64;
            self.transport.playing = false;
        }
        self.frame_position
            .store(self.transport.frame, Ordering::Relaxed);
        ProcessStatus::Rendered
    }

    fn publish_output_peak(&self, peak: f32) {
        self.output_peak
            .fetch_max(peak.to_bits(), Ordering::Relaxed);
    }

    fn clear_output_peak(&self) {
        self.output_peak.store(0.0_f32.to_bits(), Ordering::Relaxed);
    }

    fn apply_commands(&mut self) {
        let limit = self.config.maximum_commands_per_block;
        for _ in 0..limit {
            let command = if let Some(command) = self.pending_command.take() {
                command
            } else {
                match self.commands.try_recv() {
                    Ok(command) => command,
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            };
            if let Err(command) = self.apply_command(command) {
                self.pending_command = Some(command);
                break;
            }
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    fn apply_command(&mut self, command: RealtimeCommand) -> Result<(), RealtimeCommand> {
        match command {
            RealtimeCommand::ActivateTimeline(mut activation) => {
                let active = self.active_generation.load(Ordering::Relaxed);
                let desired = self.desired_generation.load(Ordering::Acquire);
                if activation.generation < active || activation.generation < desired {
                    if let Some(snapshot) = activation.snapshot.take()
                        && let Err(
                            TrySendError::Full(snapshot) | TrySendError::Disconnected(snapshot),
                        ) = self.retired.try_send(snapshot)
                    {
                        activation.snapshot = Some(snapshot);
                        return Err(RealtimeCommand::ActivateTimeline(activation));
                    }
                    return Ok(());
                }
                if let Some(old_snapshot) = self.snapshot.take()
                    && let Err(TrySendError::Full(old) | TrySendError::Disconnected(old)) =
                        self.retired.try_send(old_snapshot)
                {
                    self.snapshot = Some(old);
                    return Err(RealtimeCommand::ActivateTimeline(activation));
                }
                let preserve_transport =
                    activation.preserve_transport && activation.generation == active;
                self.snapshot = activation.snapshot.take();
                self.playback_source = PlaybackSource::Timeline;
                self.timeline_sample_rate = activation.sample_rate.max(1);
                self.timeline_total_frames = activation.total_frames;
                if !preserve_transport {
                    self.transport.loop_range = activation.loop_range;
                    self.metronome = activation.metronome;
                    self.source_position = normalize_loop_position(
                        activation.frame.min(activation.total_frames) as f64,
                        activation.loop_range,
                    );
                    self.transport.frame = self.source_position.floor() as u64;
                    self.transport.playing = activation.playing;
                }
                self.active_generation
                    .store(activation.generation, Ordering::Release);
                self.audible_generation.store(
                    if self.snapshot.is_some() {
                        activation.generation
                    } else {
                        0
                    },
                    Ordering::Release,
                );
                Ok(())
            }
            RealtimeCommand::ActivatePreview(new_snapshot) => {
                let sample_rate = new_snapshot.sample_rate();
                let total_frames = new_snapshot.total_frames();
                if let Some(old_snapshot) = self.snapshot.take() {
                    match self.retired.try_send(old_snapshot) {
                        Ok(()) => self.snapshot = Some(new_snapshot),
                        Err(TrySendError::Full(old) | TrySendError::Disconnected(old)) => {
                            self.snapshot = Some(old);
                            return Err(RealtimeCommand::ActivatePreview(new_snapshot));
                        }
                    }
                } else {
                    self.snapshot = Some(new_snapshot);
                }
                self.timeline_sample_rate = sample_rate;
                self.timeline_total_frames = total_frames;
                self.playback_source = PlaybackSource::Preview;
                self.audible_generation.store(0, Ordering::Release);
                Ok(())
            }
            RealtimeCommand::ClearPreview => {
                if let Some(old_snapshot) = self.snapshot.take() {
                    match self.retired.try_send(old_snapshot) {
                        Ok(()) => {}
                        Err(TrySendError::Full(old) | TrySendError::Disconnected(old)) => {
                            self.snapshot = Some(old);
                            return Err(RealtimeCommand::ClearPreview);
                        }
                    }
                }
                self.audible_generation.store(0, Ordering::Release);
                Ok(())
            }
            RealtimeCommand::Play => {
                self.transport.playing = true;
                Ok(())
            }
            RealtimeCommand::Pause => {
                self.transport.playing = false;
                Ok(())
            }
            RealtimeCommand::Stop => {
                self.transport.playing = false;
                self.transport.frame = 0;
                self.source_position = 0.0;
                Ok(())
            }
            RealtimeCommand::Seek(frame) => {
                self.source_position =
                    normalize_loop_position(frame as f64, self.transport.loop_range);
                self.transport.frame = self.source_position.floor() as u64;
                Ok(())
            }
            RealtimeCommand::SetLoop(loop_range) => {
                self.transport.loop_range = loop_range;
                self.source_position = normalize_loop_position(self.source_position, loop_range);
                self.transport.frame = self.source_position.floor() as u64;
                Ok(())
            }
            RealtimeCommand::SetGain(gain) => {
                self.transport.gain = if gain.is_finite() { gain.max(0.0) } else { 0.0 };
                Ok(())
            }
            RealtimeCommand::SetMetronome(metronome) => {
                self.metronome = metronome;
                Ok(())
            }
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn mix_metronome_segment(
    output: &mut [f32],
    layout: ChannelLayout,
    source_position: f64,
    source_ratio: f64,
    project_sample_rate: u32,
    metronome: RealtimeMetronome,
) {
    if !metronome.enabled
        || !metronome.bpm.is_finite()
        || metronome.bpm <= 0.0
        || metronome.numerator == 0
        || metronome.numerator > 32
        || !metronome.denominator.is_power_of_two()
        || metronome.denominator > 32
        || !metronome.gain.is_finite()
        || !(0.0..=1.0).contains(&metronome.gain)
        || project_sample_rate == 0
        || !source_ratio.is_finite()
        || source_ratio <= 0.0
    {
        return;
    }
    let channels = channel_count(layout);
    let frames_per_tick = f64::from(project_sample_rate) * 60.0 / metronome.bpm * 4.0
        / f64::from(metronome.denominator);
    let click_frames = (f64::from(project_sample_rate) * 0.035).min(frames_per_tick * 0.75);
    for (output_frame, frame) in output.chunks_exact_mut(channels).enumerate() {
        let timeline_frame = source_position + output_frame as f64 * source_ratio;
        let tick = (timeline_frame / frames_per_tick).floor() as u64;
        let age = timeline_frame - tick as f64 * frames_per_tick;
        if age >= click_frames {
            continue;
        }
        let accented = tick.is_multiple_of(u64::from(metronome.numerator));
        let frequency = if accented { 1_760.0 } else { 1_120.0 };
        let amplitude = if accented { 0.24 } else { 0.15 };
        let envelope = (1.0 - age / click_frames).max(0.0).powi(3);
        let click = (std::f64::consts::TAU * frequency * age / f64::from(project_sample_rate)).sin()
            as f32
            * amplitude
            * metronome.gain
            * envelope as f32;
        for sample in frame {
            *sample += click;
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn loop_segment_frames(
    source_position: f64,
    ratio: f64,
    remaining: usize,
    loop_range: Option<RealtimeLoopRange>,
) -> usize {
    let Some(loop_range) = loop_range else {
        return remaining;
    };
    let distance = loop_range.end_frame as f64 - source_position;
    if distance <= 0.0 {
        return 1.min(remaining);
    }
    ((distance / ratio).ceil() as usize).clamp(1, remaining)
}

#[allow(clippy::cast_precision_loss)]
fn normalize_loop_position(position: f64, loop_range: Option<RealtimeLoopRange>) -> f64 {
    let Some(loop_range) = loop_range else {
        return position;
    };
    let end = loop_range.end_frame as f64;
    if position < end {
        return position;
    }
    let start = loop_range.start_frame as f64;
    start + (position - start).rem_euclid(loop_range.frames() as f64)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn render_realtime_segment(
    snapshot: &RenderSnapshot,
    native_scratch: &mut [f32],
    output_layout: ChannelLayout,
    source_position: f64,
    ratio: f64,
    loop_range: Option<RealtimeLoopRange>,
    output: &mut [f32],
) -> bool {
    let native_channels = channel_count(snapshot.layout());
    let output_frames = output.len() / channel_count(output_layout);
    let source_frames = ((output_frames as f64 * ratio + source_position.fract()).ceil() as usize)
        .saturating_add(1);
    let native_samples = source_frames.saturating_mul(native_channels);
    if native_samples > native_scratch.len() {
        return false;
    }
    render_looped_native(
        snapshot,
        source_position.floor() as u64,
        &mut native_scratch[..native_samples],
        loop_range,
    );
    resample_and_convert(
        &native_scratch[..native_samples],
        snapshot.layout(),
        output,
        output_layout,
        source_position.fract(),
        ratio,
    );
    true
}

fn render_looped_native(
    snapshot: &RenderSnapshot,
    mut start_frame: u64,
    output: &mut [f32],
    loop_range: Option<RealtimeLoopRange>,
) {
    let channels = channel_count(snapshot.layout());
    let mut written_frames = 0;
    let requested_frames = output.len() / channels;
    while written_frames < requested_frames {
        if let Some(loop_range) = loop_range
            && start_frame >= loop_range.end_frame
        {
            start_frame = loop_range.start_frame
                + start_frame.saturating_sub(loop_range.start_frame) % loop_range.frames();
        }
        let available = loop_range.map_or(requested_frames - written_frames, |range| {
            usize::try_from(range.end_frame.saturating_sub(start_frame))
                .unwrap_or(usize::MAX)
                .min(requested_frames - written_frames)
        });
        if available == 0 {
            break;
        }
        let sample_start = written_frames * channels;
        let sample_end = (written_frames + available) * channels;
        snapshot.render_native(start_frame, &mut output[sample_start..sample_end]);
        written_frames += available;
        start_frame = start_frame.saturating_add(available as u64);
    }
    output[written_frames * channels..].fill(0.0);
}

impl fmt::Debug for RealtimeEngine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RealtimeEngine")
            .field("config", &self.config)
            .field("snapshot_revision", &self.snapshot_revision())
            .field("transport", &self.transport)
            .finish_non_exhaustive()
    }
}

/// Result of a real-time process call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessStatus {
    /// Audio was rendered.
    Rendered,
    /// Transport is paused or has no snapshot.
    Silence,
    /// Buffer does not contain complete frames.
    IncompleteFrame,
    /// Buffer exceeds the configured maximum block size.
    BlockTooLarge,
    /// The installed render uses another sample rate.
    SampleRateMismatch,
}

/// Information about an opened device stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceStreamInfo {
    /// CPAL host API providing the stream (for example ALSA, JACK, or WASAPI).
    pub backend: cpal::HostId,
    /// Device sample rate.
    pub sample_rate: u32,
    /// Device channel layout.
    pub layout: ChannelLayout,
    /// Native device sample representation.
    pub sample_format: SampleFormat,
    /// Fixed buffer requested from the backend, or `None` for its default.
    pub requested_buffer_frames: Option<u32>,
}

/// Identity of the exact output stream that was successfully opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenedOutputDeviceInfo {
    /// CPAL host API providing the stream.
    pub backend: cpal::HostId,
    /// Stable identity of the opened device.
    pub id: cpal::DeviceId,
    /// Human-readable output-device name.
    pub name: String,
}

/// One output configuration range reported by CPAL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutputConfigInfo {
    /// Number of interleaved channels. GAW opens only mono or stereo ranges.
    pub channels: u16,
    /// Lowest supported sample rate, inclusive.
    pub minimum_sample_rate: u32,
    /// Highest supported sample rate, inclusive.
    pub maximum_sample_rate: u32,
    /// Native sample representation.
    pub sample_format: SampleFormat,
}

/// Stable identity and output capabilities for an enumerated device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputDeviceInfo {
    /// CPAL host API that owns this device.
    pub backend: cpal::HostId,
    /// Stable CPAL device identifier, suitable for reopening the device.
    pub id: cpal::DeviceId,
    /// Human-readable device name.
    pub name: String,
    /// Whether this was the backend's default output at enumeration time.
    pub is_default: bool,
    /// Output configuration ranges reported by the backend.
    pub configurations: Vec<OutputConfigInfo>,
}

/// Stable identity for an enumerated audio-input device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDeviceInfo {
    /// CPAL host API that owns this device.
    pub backend: cpal::HostId,
    /// Stable CPAL device identifier.
    pub id: cpal::DeviceId,
    /// Human-readable device name.
    pub name: String,
    /// Whether this was the backend's default input at enumeration time.
    pub is_default: bool,
}

/// Recommended non-real-time response to a CPAL stream error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamRecoveryAction {
    /// Keep the stream; an underrun may have caused a transient glitch.
    Continue,
    /// Drop and rebuild the stream with the same device/configuration.
    RebuildStream,
    /// Re-enumerate devices and open another device if necessary.
    ReopenDevice,
}

/// Classifies a CPAL callback error for a non-real-time recovery controller.
pub fn stream_recovery_action(error: &cpal::StreamError) -> StreamRecoveryAction {
    match error {
        cpal::StreamError::BufferUnderrun => StreamRecoveryAction::Continue,
        cpal::StreamError::StreamInvalidated => StreamRecoveryAction::RebuildStream,
        cpal::StreamError::DeviceNotAvailable | cpal::StreamError::BackendSpecific { .. } => {
            StreamRecoveryAction::ReopenDevice
        }
    }
}

/// CPAL backends currently available on this machine.
pub fn available_audio_backends() -> Vec<cpal::HostId> {
    cpal::available_hosts()
}

/// Enumerates output devices and their complete reported configuration ranges.
///
/// Call this once for each value returned by [`available_audio_backends`] so a
/// failure in one backend does not hide devices from another backend.
///
/// # Errors
///
/// Returns a backend, enumeration, identity, description, or configuration error.
pub fn enumerate_output_devices(
    backend: cpal::HostId,
) -> Result<Vec<OutputDeviceInfo>, DeviceError> {
    let host = cpal::host_from_id(backend).map_err(|_| DeviceError::HostUnavailable(backend))?;
    let default_id = host
        .default_output_device()
        .and_then(|device| device.id().ok());
    let devices = host
        .output_devices()
        .map_err(DeviceError::EnumerateDevices)?;
    let mut outputs = Vec::new();
    for device in devices {
        let id = device.id().map_err(DeviceError::DeviceId)?;
        let name = device
            .description()
            .map_err(DeviceError::DeviceDescription)?
            .name()
            .to_owned();
        // Device lists are inherently racy: an entry can disappear between
        // enumeration and querying its capabilities. Keep every healthy
        // output instead of failing the entire list because one went stale.
        let configurations = match device.supported_output_configs() {
            Ok(configurations) => configurations
                .map(|range| OutputConfigInfo {
                    channels: range.channels(),
                    minimum_sample_rate: range.min_sample_rate(),
                    maximum_sample_rate: range.max_sample_rate(),
                    sample_format: range.sample_format(),
                })
                .collect(),
            Err(cpal::SupportedStreamConfigsError::DeviceNotAvailable) => continue,
            Err(error) => return Err(DeviceError::SupportedConfigs(error)),
        };
        outputs.push(OutputDeviceInfo {
            backend,
            is_default: default_id.as_ref() == Some(&id),
            id,
            name,
            configurations,
        });
    }
    Ok(outputs)
}

/// Enumerates input devices for one audio backend.
///
/// # Errors
///
/// Returns a backend, enumeration, identity, or description error.
pub fn enumerate_input_devices(backend: cpal::HostId) -> Result<Vec<InputDeviceInfo>, DeviceError> {
    let host = cpal::host_from_id(backend).map_err(|_| DeviceError::HostUnavailable(backend))?;
    let default_id = host
        .default_input_device()
        .and_then(|device| device.id().ok());
    let devices = host
        .input_devices()
        .map_err(DeviceError::EnumerateInputDevices)?;
    let mut inputs = Vec::new();
    for device in devices {
        let id = device.id().map_err(DeviceError::DeviceId)?;
        let name = device
            .description()
            .map_err(DeviceError::DeviceDescription)?
            .name()
            .to_owned();
        inputs.push(InputDeviceInfo {
            backend,
            is_default: default_id.as_ref() == Some(&id),
            id,
            name,
        });
    }
    Ok(inputs)
}

/// Observes default and pinned-device identity without probing every device's
/// stream configurations.
///
/// # Errors
///
/// Returns an error when the backend or default-device identity is unavailable.
pub fn observe_output_devices(
    selection: &OutputDeviceSelection,
) -> Result<DeviceObservation, DeviceError> {
    let backend = match selection {
        OutputDeviceSelection::FollowDefault { backend } => *backend,
        OutputDeviceSelection::Pinned { device_id } => device_id.0,
    };
    let host = cpal::host_from_id(backend).map_err(|_| DeviceError::HostUnavailable(backend))?;
    let default_output = host
        .default_output_device()
        .map(|device| device.id().map_err(DeviceError::DeviceId))
        .transpose()?;
    let pinned_available = match selection {
        OutputDeviceSelection::FollowDefault { .. } => false,
        OutputDeviceSelection::Pinned { device_id } => host.device_by_id(device_id).is_some(),
    };
    Ok(DeviceObservation {
        default_output,
        pinned_available,
    })
}

/// Narrow CPAL mono/stereo output stream wrapper.
pub struct CpalOutput {
    stream: cpal::Stream,
    device_id: cpal::DeviceId,
    device_name: String,
    info: DeviceStreamInfo,
    callback_frames: Arc<AtomicU32>,
}

impl CpalOutput {
    /// Opens the default output device with the engine's exact rate and layout.
    ///
    /// The stream is returned paused. Call [`Self::play`] to start callbacks.
    ///
    /// # Errors
    ///
    /// Returns an error when no matching default device configuration exists
    /// or CPAL cannot build the stream.
    pub fn open_default<E>(engine: RealtimeEngine, error_callback: E) -> Result<Self, DeviceError>
    where
        E: FnMut(cpal::StreamError) + Send + 'static,
    {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(DeviceError::NoDefaultOutput)?;
        Self::open_on_device(&device, host.id(), engine, false, None, error_callback)
    }

    /// Opens the default device, negotiating the nearest mono/stereo PCM
    /// layout and sample rate when the requested configuration is unavailable.
    ///
    /// # Errors
    ///
    /// Returns an error when the device cannot be queried or opened.
    pub fn open_default_negotiated<E>(
        engine: RealtimeEngine,
        error_callback: E,
    ) -> Result<Self, DeviceError>
    where
        E: FnMut(cpal::StreamError) + Send + 'static,
    {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(DeviceError::NoDefaultOutput)?;
        Self::open_on_device(&device, host.id(), engine, true, None, error_callback)
    }

    /// Opens the default device with negotiated format and a requested callback buffer.
    /// `None` lets the backend choose its default buffer size.
    ///
    /// # Errors
    ///
    /// Returns an error when the device or requested stream configuration is unavailable.
    pub fn open_default_negotiated_with_buffer<E>(
        engine: RealtimeEngine,
        buffer_frames: Option<u32>,
        error_callback: E,
    ) -> Result<Self, DeviceError>
    where
        E: FnMut(cpal::StreamError) + Send + 'static,
    {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(DeviceError::NoDefaultOutput)?;
        Self::open_on_device(
            &device,
            host.id(),
            engine,
            true,
            buffer_frames,
            error_callback,
        )
    }

    /// Opens the default output from a specific CPAL backend.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend or device is unavailable or cannot be opened.
    pub fn open_default_on_backend<E>(
        backend: cpal::HostId,
        engine: RealtimeEngine,
        negotiate: bool,
        error_callback: E,
    ) -> Result<Self, DeviceError>
    where
        E: FnMut(cpal::StreamError) + Send + 'static,
    {
        let host =
            cpal::host_from_id(backend).map_err(|_| DeviceError::HostUnavailable(backend))?;
        let device = host
            .default_output_device()
            .ok_or(DeviceError::NoOutputOnBackend(backend))?;
        Self::open_on_device(&device, backend, engine, negotiate, None, error_callback)
    }

    /// Opens a backend's default output with an optional fixed callback buffer.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend, device, or requested configuration is unavailable.
    pub fn open_default_on_backend_with_buffer<E>(
        backend: cpal::HostId,
        engine: RealtimeEngine,
        negotiate: bool,
        buffer_frames: Option<u32>,
        error_callback: E,
    ) -> Result<Self, DeviceError>
    where
        E: FnMut(cpal::StreamError) + Send + 'static,
    {
        let host =
            cpal::host_from_id(backend).map_err(|_| DeviceError::HostUnavailable(backend))?;
        let device = host
            .default_output_device()
            .ok_or(DeviceError::NoOutputOnBackend(backend))?;
        Self::open_on_device(
            &device,
            backend,
            engine,
            negotiate,
            buffer_frames,
            error_callback,
        )
    }

    /// Opens an enumerated device by its stable CPAL identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend or device is unavailable or cannot be opened.
    pub fn open_device<E>(
        device_id: &cpal::DeviceId,
        engine: RealtimeEngine,
        negotiate: bool,
        error_callback: E,
    ) -> Result<Self, DeviceError>
    where
        E: FnMut(cpal::StreamError) + Send + 'static,
    {
        let backend = device_id.0;
        let host =
            cpal::host_from_id(backend).map_err(|_| DeviceError::HostUnavailable(backend))?;
        let device = host
            .device_by_id(device_id)
            .ok_or_else(|| DeviceError::DeviceUnavailable(device_id.clone()))?;
        Self::open_on_device(&device, backend, engine, negotiate, None, error_callback)
    }

    /// Opens an enumerated output with an optional fixed callback buffer.
    ///
    /// # Errors
    ///
    /// Returns an error when the device or requested stream configuration is unavailable.
    pub fn open_device_with_buffer<E>(
        device_id: &cpal::DeviceId,
        engine: RealtimeEngine,
        negotiate: bool,
        buffer_frames: Option<u32>,
        error_callback: E,
    ) -> Result<Self, DeviceError>
    where
        E: FnMut(cpal::StreamError) + Send + 'static,
    {
        let backend = device_id.0;
        let host =
            cpal::host_from_id(backend).map_err(|_| DeviceError::HostUnavailable(backend))?;
        let device = host
            .device_by_id(device_id)
            .ok_or_else(|| DeviceError::DeviceUnavailable(device_id.clone()))?;
        Self::open_on_device(
            &device,
            backend,
            engine,
            negotiate,
            buffer_frames,
            error_callback,
        )
    }

    #[allow(clippy::too_many_lines)]
    fn open_on_device<E>(
        device: &cpal::Device,
        backend: cpal::HostId,
        mut engine: RealtimeEngine,
        negotiate: bool,
        buffer_frames: Option<u32>,
        error_callback: E,
    ) -> Result<Self, DeviceError>
    where
        E: FnMut(cpal::StreamError) + Send + 'static,
    {
        let device_id = device.id().map_err(DeviceError::DeviceId)?;
        let device_name = device.description().map_or_else(
            |_| format!("{backend:?} output"),
            |description| description.name().to_owned(),
        );
        let requested = engine.config();
        let channels = u16::try_from(channel_count(requested.output_layout))
            .map_err(|_| DeviceError::UnsupportedLayout)?;
        let sample_rate = requested.sample_rate;
        let supported = device
            .supported_output_configs()
            .map_err(DeviceError::SupportedConfigs)?;
        let chosen =
            choose_output_config(supported, channels, sample_rate, buffer_frames, negotiate)
                .ok_or(DeviceError::NoMatchingConfig {
                    sample_rate: requested.sample_rate,
                    channels,
                })?;
        let negotiated_layout =
            layout_for_channels(chosen.channels()).ok_or(DeviceError::UnsupportedLayout)?;
        let negotiated_rate = chosen.sample_rate();
        engine.config.sample_rate = negotiated_rate;
        engine.config.output_layout = negotiated_layout;
        let sample_format = chosen.sample_format();
        let maximum_block_frames = requested.maximum_block_frames;
        let mut stream_config = chosen.config();
        configure_buffer_size(
            &mut stream_config,
            chosen.buffer_size(),
            buffer_frames,
            maximum_block_frames,
        )?;
        let (stream, callback_frames) = match sample_format {
            SampleFormat::I8 => build_output_stream::<i8, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            SampleFormat::I16 => build_output_stream::<i16, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            SampleFormat::I24 => build_output_stream::<cpal::I24, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            SampleFormat::I32 => build_output_stream::<i32, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            SampleFormat::I64 => build_output_stream::<i64, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            SampleFormat::U8 => build_output_stream::<u8, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            SampleFormat::U16 => build_output_stream::<u16, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            SampleFormat::U24 => build_output_stream::<cpal::U24, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            SampleFormat::U32 => build_output_stream::<u32, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            SampleFormat::U64 => build_output_stream::<u64, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            SampleFormat::F32 => build_output_stream::<f32, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            SampleFormat::F64 => build_output_stream::<f64, _>(
                device,
                &stream_config,
                engine,
                maximum_block_frames,
                error_callback,
            ),
            unsupported => return Err(DeviceError::UnsupportedSampleFormat(unsupported)),
        }
        .map_err(DeviceError::BuildStream)?;

        Ok(Self {
            stream,
            device_id,
            device_name,
            info: DeviceStreamInfo {
                backend,
                sample_rate: negotiated_rate,
                layout: negotiated_layout,
                sample_format,
                requested_buffer_frames: buffer_frames,
            },
            callback_frames,
        })
    }

    /// Selected device configuration.
    pub fn info(&self) -> DeviceStreamInfo {
        self.info
    }

    /// Most recent callback buffer size observed from the backend.
    pub fn callback_buffer_frames(&self) -> Option<u32> {
        let frames = self.callback_frames.load(Ordering::Relaxed);
        (frames > 0).then_some(frames)
    }

    /// Stable identity of the exact device used to build this stream.
    pub fn device_id(&self) -> &cpal::DeviceId {
        &self.device_id
    }

    /// Human-readable name of the exact device used to build this stream.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Identity of the exact stream that was opened, without re-enumerating
    /// other devices on the same backend.
    pub fn opened_device_info(&self) -> OpenedOutputDeviceInfo {
        OpenedOutputDeviceInfo {
            backend: self.info.backend,
            id: self.device_id.clone(),
            name: self.device_name.clone(),
        }
    }

    /// Starts or resumes the stream.
    ///
    /// # Errors
    ///
    /// Returns CPAL's error if the backend cannot start the stream.
    pub fn play(&self) -> Result<(), DeviceError> {
        self.stream.play().map_err(DeviceError::PlayStream)
    }

    /// Requests that the device stream pause.
    ///
    /// # Errors
    ///
    /// Returns CPAL's error if the backend cannot pause the stream.
    pub fn pause(&self) -> Result<(), DeviceError> {
        self.stream.pause().map_err(DeviceError::PauseStream)
    }
}

impl fmt::Debug for CpalOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CpalOutput")
            .field("device_id", &self.device_id)
            .field("device_name", &self.device_name)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

fn build_output_stream<T, E>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut engine: RealtimeEngine,
    maximum_block_frames: usize,
    error_callback: E,
) -> Result<(cpal::Stream, Arc<AtomicU32>), cpal::BuildStreamError>
where
    T: SizedSample + FromSample<f32>,
    E: FnMut(cpal::StreamError) + Send + 'static,
{
    let channels = usize::from(config.channels);
    let mut scratch = vec![0.0; maximum_block_frames * channels].into_boxed_slice();
    let callback_frames = Arc::new(AtomicU32::new(0));
    let observed_frames = Arc::clone(&callback_frames);
    let stream = device.build_output_stream::<T, _, _>(
        config,
        move |output, _| {
            observed_frames.store(
                u32::try_from(output.len() / channels).unwrap_or(u32::MAX),
                Ordering::Relaxed,
            );
            let chunk_samples = maximum_block_frames * channels;
            let mut chunks = output.chunks_exact_mut(chunk_samples);
            for chunk in &mut chunks {
                engine.process(&mut scratch);
                copy_as_sample(&scratch, chunk);
            }
            let remainder = chunks.into_remainder();
            let complete_samples = remainder.len() - remainder.len() % channels;
            if complete_samples > 0 {
                engine.process(&mut scratch[..complete_samples]);
                copy_as_sample(
                    &scratch[..complete_samples],
                    &mut remainder[..complete_samples],
                );
            }
            for sample in &mut remainder[complete_samples..] {
                *sample = T::from_sample_(0.0);
            }
        },
        error_callback,
        None,
    )?;
    Ok((stream, callback_frames))
}

fn copy_as_sample<T: FromSample<f32>>(source: &[f32], output: &mut [T]) {
    for (source, output) in source.iter().zip(output) {
        *output = T::from_sample_(sanitize_sample(*source));
    }
}

/// Encoding used by deterministic offline rendering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WavEncoding {
    /// IEEE 32-bit float.
    #[default]
    Float32,
    /// Signed 16-bit PCM.
    Pcm16,
    /// Signed 24-bit PCM.
    Pcm24,
}

/// Settings for a deterministic positional WAV render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfflineWavSpec {
    /// First snapshot frame written to the file.
    pub start_frame: u64,
    /// Number of frames written. `None` renders through the declared tail.
    pub frames: Option<u64>,
    /// Output layout. Choosing a different layout explicitly converts channels.
    pub layout: ChannelLayout,
    /// Output sample rate. `None` preserves the snapshot sample rate.
    pub sample_rate: Option<u32>,
    /// Bounded working block size; this does not affect sample values.
    pub block_frames: usize,
    /// WAV sample encoding.
    pub encoding: WavEncoding,
}

impl Default for OfflineWavSpec {
    fn default() -> Self {
        Self {
            start_frame: 0,
            frames: None,
            layout: ChannelLayout::Stereo,
            sample_rate: None,
            block_frames: 4_096,
            encoding: WavEncoding::Float32,
        }
    }
}

/// Render an immutable snapshot to a deterministic WAV file.
///
/// # Errors
///
/// Returns an error for an invalid block size, an unsupported layout, or any
/// failure while creating, writing, or finalizing the WAV file.
pub fn render_wav(
    snapshot: &RenderSnapshot,
    path: impl AsRef<Path>,
    spec: OfflineWavSpec,
) -> Result<OfflineRenderReport, OfflineRenderError> {
    if spec.block_frames == 0 {
        return Err(OfflineRenderError::ZeroBlockFrames);
    }
    let output_rate = spec.sample_rate.unwrap_or(snapshot.sample_rate());
    if output_rate == 0 {
        return Err(OfflineRenderError::ZeroSampleRate);
    }
    let available = snapshot.total_frames().saturating_sub(spec.start_frame);
    let source_frames = spec.frames.unwrap_or(available).min(available);
    let frames = resampled_frame_count(source_frames, snapshot.sample_rate(), output_rate)?;
    let output_channels = channel_count(spec.layout);
    let wav_spec = hound::WavSpec {
        channels: u16::try_from(output_channels)
            .map_err(|_| OfflineRenderError::UnsupportedLayout)?,
        sample_rate: output_rate,
        bits_per_sample: match spec.encoding {
            WavEncoding::Float32 => 32,
            WavEncoding::Pcm16 => 16,
            WavEncoding::Pcm24 => 24,
        },
        sample_format: match spec.encoding {
            WavEncoding::Float32 => hound::SampleFormat::Float,
            WavEncoding::Pcm16 | WavEncoding::Pcm24 => hound::SampleFormat::Int,
        },
    };
    let mut writer = hound::WavWriter::create(path, wav_spec)?;
    let written = if output_rate == snapshot.sample_rate() {
        render_wav_native(snapshot, &mut writer, spec, source_frames)?
    } else {
        render_wav_resampled(
            snapshot,
            &mut writer,
            spec,
            source_frames,
            frames,
            output_rate,
        )?
    };
    writer.finalize()?;
    Ok(OfflineRenderReport {
        start_frame: spec.start_frame,
        frames: written,
        sample_rate: output_rate,
        layout: spec.layout,
    })
}

const OFFLINE_RESAMPLE_CHUNK_FRAMES: usize = 2_048;

fn render_wav_native<W: Write + Seek>(
    snapshot: &RenderSnapshot,
    writer: &mut hound::WavWriter<W>,
    spec: OfflineWavSpec,
    source_frames: u64,
) -> Result<u64, OfflineRenderError> {
    let native_channels = channel_count(snapshot.layout());
    let output_channels = channel_count(spec.layout);
    let mut native = vec![0.0; checked_block_samples(spec.block_frames, native_channels)?];
    let mut output = vec![0.0; checked_block_samples(spec.block_frames, output_channels)?];
    let mut written = 0_u64;
    while written < source_frames {
        let block_frames = usize::try_from(source_frames - written)
            .unwrap_or(usize::MAX)
            .min(spec.block_frames);
        let native_samples = block_frames * native_channels;
        let output_samples = block_frames * output_channels;
        snapshot.render_native(
            spec.start_frame.saturating_add(written),
            &mut native[..native_samples],
        );
        convert_layout(
            &native[..native_samples],
            native_channels,
            &mut output[..output_samples],
            output_channels,
        );
        write_wav_samples(writer, &output[..output_samples], spec.encoding)?;
        written += block_frames as u64;
    }
    Ok(written)
}

#[allow(clippy::too_many_arguments)]
fn render_wav_resampled<W: Write + Seek>(
    snapshot: &RenderSnapshot,
    writer: &mut hound::WavWriter<W>,
    spec: OfflineWavSpec,
    source_frames: u64,
    output_frames: u64,
    output_rate: u32,
) -> Result<u64, OfflineRenderError> {
    if source_frames == 0 {
        return Ok(0);
    }
    let native_channels = channel_count(snapshot.layout());
    let output_channels = channel_count(spec.layout);
    let parameters = SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        oversampling_factor: 128,
        interpolation: SincInterpolationType::Cubic,
        window: WindowFunction::BlackmanHarris2,
    };
    let ratio = f64::from(output_rate) / f64::from(snapshot.sample_rate());
    let mut resampler = Async::<f32>::new_sinc(
        ratio,
        1.0,
        &parameters,
        OFFLINE_RESAMPLE_CHUNK_FRAMES,
        native_channels,
        FixedAsync::Input,
    )
    .map_err(|error| OfflineRenderError::Resample(error.to_string()))?;
    let input_capacity = resampler.input_frames_max();
    let resampled_capacity = resampler.output_frames_max();
    let mut input = vec![0.0; checked_block_samples(input_capacity, native_channels)?];
    let mut filtered = vec![0.0; checked_block_samples(resampled_capacity, native_channels)?];
    let mut converted = vec![0.0; checked_block_samples(resampled_capacity, output_channels)?];
    let mut source_position = 0_u64;
    let mut written = 0_u64;
    let mut delay = resampler.output_delay();
    let mut empty_flushes = 0_usize;
    while written < output_frames {
        let needed = resampler.input_frames_next();
        let available = usize::try_from(source_frames.saturating_sub(source_position))
            .unwrap_or(usize::MAX)
            .min(needed);
        input[..needed * native_channels].fill(0.0);
        if available != 0 {
            snapshot.render_native(
                spec.start_frame.saturating_add(source_position),
                &mut input[..available * native_channels],
            );
        }
        let input_adapter = InterleavedSlice::new(&input, native_channels, input_capacity)
            .map_err(|error| OfflineRenderError::Resample(error.to_string()))?;
        let mut output_adapter =
            InterleavedSlice::new_mut(&mut filtered, native_channels, resampled_capacity)
                .map_err(|error| OfflineRenderError::Resample(error.to_string()))?;
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: (available < needed).then_some(available),
            active_channels_mask: None,
        };
        let (_, produced) = resampler
            .process_into_buffer(&input_adapter, &mut output_adapter, Some(&indexing))
            .map_err(|error| OfflineRenderError::Resample(error.to_string()))?;
        source_position = source_position.saturating_add(available as u64);
        let skip = delay.min(produced);
        delay -= skip;
        let useful = produced.saturating_sub(skip);
        let wanted = usize::try_from(output_frames - written)
            .unwrap_or(usize::MAX)
            .min(useful);
        if wanted != 0 {
            let source = &filtered[skip * native_channels..(skip + wanted) * native_channels];
            let destination = &mut converted[..wanted * output_channels];
            convert_layout(source, native_channels, destination, output_channels);
            write_wav_samples(writer, destination, spec.encoding)?;
            written += wanted as u64;
            empty_flushes = 0;
        } else if source_position >= source_frames {
            empty_flushes += 1;
            if empty_flushes > 130 {
                return Err(OfflineRenderError::Resample(
                    "resampler did not finish after bounded zero flush".into(),
                ));
            }
        }
    }
    Ok(written)
}

fn checked_block_samples(frames: usize, channels: usize) -> Result<usize, OfflineRenderError> {
    frames
        .checked_mul(channels)
        .ok_or(OfflineRenderError::BlockSizeOverflow)
}

fn convert_layout(
    source: &[f32],
    source_channels: usize,
    output: &mut [f32],
    output_channels: usize,
) {
    for (source, output) in source
        .chunks_exact(source_channels)
        .zip(output.chunks_exact_mut(output_channels))
    {
        match (source_channels, output_channels) {
            (1, 1) => output[0] = source[0],
            (1, 2) => output.fill(source[0]),
            (2, 1) => output[0] = (source[0] + source[1]) * 0.5,
            (2, 2) => output.copy_from_slice(source),
            _ => unreachable!("mono and stereo only"),
        }
    }
}

fn write_wav_samples<W: Write + Seek>(
    writer: &mut hound::WavWriter<W>,
    samples: &[f32],
    encoding: WavEncoding,
) -> Result<(), hound::Error> {
    for &sample in samples {
        match encoding {
            WavEncoding::Float32 => writer.write_sample(sanitize_sample(sample))?,
            WavEncoding::Pcm16 => writer.write_sample(quantize_i16(sample))?,
            WavEncoding::Pcm24 => writer.write_sample(quantize_i24(sample))?,
        }
    }
    Ok(())
}

/// Completed offline render metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OfflineRenderReport {
    /// First rendered snapshot frame.
    pub start_frame: u64,
    /// Frames written.
    pub frames: u64,
    /// WAV sample rate.
    pub sample_rate: u32,
    /// WAV layout.
    pub layout: ChannelLayout,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum BlockError {
    #[error("{samples} samples do not form complete {channels}-channel frames")]
    IncompleteFrame { samples: usize, channels: usize },
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SnapshotError {
    #[error("sample rate must be nonzero")]
    ZeroSampleRate,
    #[error("snapshot body and tail length overflow u64")]
    LengthOverflow,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EngineConfigError {
    #[error("sample rate must be nonzero")]
    ZeroSampleRate,
    #[error("maximum block frames must be nonzero")]
    ZeroMaximumBlockFrames,
    #[error("maximum commands per block must be nonzero")]
    ZeroMaximumCommands,
    #[error("command queue capacity must be nonzero")]
    ZeroCommandCapacity,
    #[error("retirement queue capacity must be nonzero")]
    ZeroRetirementCapacity,
    #[error("scratch buffer size overflow")]
    ScratchSizeOverflow,
}

#[derive(Debug, Error)]
pub enum DeviceError {
    #[error("audio backend {0} is unavailable")]
    HostUnavailable(cpal::HostId),
    #[error("there is no default output device")]
    NoDefaultOutput,
    #[error("audio backend {0} has no default output device")]
    NoOutputOnBackend(cpal::HostId),
    #[error("failed to enumerate output devices: {0}")]
    EnumerateDevices(cpal::DevicesError),
    #[error("failed to enumerate input devices: {0}")]
    EnumerateInputDevices(cpal::DevicesError),
    #[error("failed to read output device ID: {0}")]
    DeviceId(cpal::DeviceIdError),
    #[error("failed to read output device description: {0}")]
    DeviceDescription(cpal::DeviceNameError),
    #[error("output device `{0:?}` is unavailable")]
    DeviceUnavailable(cpal::DeviceId),
    #[error("failed to enumerate output configurations: {0}")]
    SupportedConfigs(cpal::SupportedStreamConfigsError),
    #[error("no mono/stereo output supports {sample_rate} Hz and {channels} channels")]
    NoMatchingConfig { sample_rate: u32, channels: u16 },
    #[error("unsupported output channel layout")]
    UnsupportedLayout,
    #[error("unsupported output sample format: {0}")]
    UnsupportedSampleFormat(SampleFormat),
    #[error("audio buffer size must be between 1 and {maximum} frames, got {requested}")]
    InvalidBufferSize { requested: u32, maximum: usize },
    #[error("output device supports buffers from {minimum} to {maximum} frames, not {requested}")]
    UnsupportedBufferSize {
        requested: u32,
        minimum: u32,
        maximum: u32,
    },
    #[error("failed to build output stream: {0}")]
    BuildStream(cpal::BuildStreamError),
    #[error("failed to start output stream: {0}")]
    PlayStream(cpal::PlayStreamError),
    #[error("failed to pause output stream: {0}")]
    PauseStream(cpal::PauseStreamError),
}

#[derive(Debug, Error)]
pub enum OfflineRenderError {
    #[error("offline block frames must be nonzero")]
    ZeroBlockFrames,
    #[error("offline output sample rate must be nonzero")]
    ZeroSampleRate,
    #[error("offline block buffer size overflow")]
    BlockSizeOverflow,
    #[error("offline output frame count overflow")]
    OutputLengthOverflow,
    #[error("unsupported output channel layout")]
    UnsupportedLayout,
    #[error("band-limited sample-rate conversion failed: {0}")]
    Resample(String),
    #[error("WAV output failed: {0}")]
    Wav(#[from] hound::Error),
}

fn channel_count(layout: ChannelLayout) -> usize {
    match layout {
        ChannelLayout::Mono => 1,
        ChannelLayout::Stereo => 2,
    }
}

fn resample_and_convert(
    source: &[f32],
    source_layout: ChannelLayout,
    output: &mut [f32],
    output_layout: ChannelLayout,
    initial_fraction: f64,
    ratio: f64,
) {
    let source_channels = channel_count(source_layout);
    resample_to_layout(
        source,
        source_channels,
        output,
        output_layout,
        initial_fraction,
        ratio,
    );
}

fn resampled_frame_count(
    source_frames: u64,
    source_rate: u32,
    output_rate: u32,
) -> Result<u64, OfflineRenderError> {
    let frames =
        (u128::from(source_frames) * u128::from(output_rate)).div_ceil(u128::from(source_rate));
    u64::try_from(frames).map_err(|_| OfflineRenderError::OutputLengthOverflow)
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn resample_to_layout(
    source: &[f32],
    source_channels: usize,
    output: &mut [f32],
    output_layout: ChannelLayout,
    initial_fraction: f64,
    ratio: f64,
) {
    let output_channels = channel_count(output_layout);
    for (frame_index, frame) in output.chunks_exact_mut(output_channels).enumerate() {
        let position = initial_fraction + frame_index as f64 * ratio;
        let lower = position.floor() as usize;
        let upper = lower.saturating_add(1);
        let fraction = (position - lower as f64) as f32;
        let sample = |channel: usize| {
            let channel = channel.min(source_channels - 1);
            let a = source
                .get(lower * source_channels + channel)
                .copied()
                .unwrap_or(0.0);
            let b = source
                .get(upper * source_channels + channel)
                .copied()
                .unwrap_or(a);
            a + (b - a) * fraction
        };
        match (source_channels, output_channels) {
            (1, 1) => frame[0] = sample(0),
            (1, 2) => frame.fill(sample(0)),
            (2, 1) => frame[0] = (sample(0) + sample(1)) * 0.5,
            (2, 2) => {
                frame[0] = sample(0);
                frame[1] = sample(1);
            }
            _ => unreachable!("mono and stereo only"),
        }
    }
}

fn apply_gain(samples: &mut [f32], gain: f32) {
    for sample in samples {
        *sample *= gain;
    }
}

fn block_peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0_f32, |peak, sample| {
        if sample.is_finite() {
            peak.max(sample.abs())
        } else {
            peak
        }
    })
}

fn sanitize_sample(sample: f32) -> f32 {
    if sample.is_finite() {
        sample.clamp(-1.0, 1.0)
    } else {
        0.0
    }
}

#[allow(clippy::cast_possible_truncation)]
fn quantize_i16(sample: f32) -> i16 {
    let sample = sanitize_sample(sample);
    let scale = if sample < 0.0 { 32_768.0 } else { 32_767.0 };
    (sample * scale).round() as i16
}

#[allow(clippy::cast_possible_truncation)]
fn quantize_i24(sample: f32) -> i32 {
    let sample = sanitize_sample(sample);
    let scale = if sample < 0.0 {
        8_388_608.0
    } else {
        8_388_607.0
    };
    (sample * scale).round() as i32
}

fn is_pcm_format(format: SampleFormat) -> bool {
    !matches!(
        format,
        SampleFormat::DsdU8 | SampleFormat::DsdU16 | SampleFormat::DsdU32
    )
}

fn sample_format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 0,
        SampleFormat::I16 => 1,
        SampleFormat::U16 => 2,
        SampleFormat::F64 => 3,
        _ => 4,
    }
}

fn layout_for_channels(channels: u16) -> Option<ChannelLayout> {
    match channels {
        1 => Some(ChannelLayout::Mono),
        2 => Some(ChannelLayout::Stereo),
        _ => None,
    }
}

fn choose_output_config(
    ranges: impl IntoIterator<Item = cpal::SupportedStreamConfigRange>,
    requested_channels: u16,
    requested_rate: u32,
    requested_buffer: Option<u32>,
    negotiate: bool,
) -> Option<cpal::SupportedStreamConfig> {
    ranges
        .into_iter()
        .filter(|range| {
            layout_for_channels(range.channels()).is_some()
                && is_pcm_format(range.sample_format())
                && requested_buffer.is_none_or(|frames| match range.buffer_size() {
                    cpal::SupportedBufferSize::Range { min, max } => {
                        (*min..=*max).contains(&frames)
                    }
                    cpal::SupportedBufferSize::Unknown => true,
                })
                && (negotiate
                    || (range.channels() == requested_channels
                        && (range.min_sample_rate()..=range.max_sample_rate())
                            .contains(&requested_rate)))
        })
        .map(|range| {
            let rate = requested_rate.clamp(range.min_sample_rate(), range.max_sample_rate());
            let key = (
                u8::from(range.channels() != requested_channels),
                requested_rate.abs_diff(rate),
                sample_format_rank(range.sample_format()),
                range.channels(),
                rate,
            );
            (key, range.with_sample_rate(rate))
        })
        .min_by_key(|(key, _)| *key)
        .map(|(_, config)| config)
}

fn configure_buffer_size(
    config: &mut cpal::StreamConfig,
    supported: &cpal::SupportedBufferSize,
    requested: Option<u32>,
    maximum_block_frames: usize,
) -> Result<(), DeviceError> {
    let Some(requested) = requested else {
        config.buffer_size = BufferSize::Default;
        return Ok(());
    };
    if requested == 0
        || usize::try_from(requested).map_or(true, |value| value > maximum_block_frames)
    {
        return Err(DeviceError::InvalidBufferSize {
            requested,
            maximum: maximum_block_frames,
        });
    }
    if let cpal::SupportedBufferSize::Range { min, max } = *supported
        && !(min..=max).contains(&requested)
    {
        return Err(DeviceError::UnsupportedBufferSize {
            requested,
            minimum: min,
            maximum: max,
        });
    }
    config.buffer_size = BufferSize::Fixed(requested);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::float_cmp)]
mod tests {
    use std::{
        fs,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use super::*;

    #[derive(Debug)]
    struct Ramp;

    impl RealtimeRender for Ramp {
        fn render(&self, start_frame: u64, output: &mut SampleBlock<'_>) {
            let channels = channel_count(output.layout());
            for (index, frame) in output.samples_mut().chunks_exact_mut(channels).enumerate() {
                let value = (start_frame + index as u64) as f32 / 10.0;
                for sample in frame {
                    *sample = value;
                }
            }
        }
    }

    fn snapshot(revision: u64, layout: ChannelLayout, frames: u64) -> Arc<RenderSnapshot> {
        Arc::new(RenderSnapshot::new(revision, 48_000, layout, frames, 0, Arc::new(Ramp)).unwrap())
    }

    fn engine(layout: ChannelLayout) -> (CommandSender, RealtimeEngine) {
        RealtimeEngine::new(
            RealtimeEngineConfig {
                output_layout: layout,
                maximum_block_frames: 8,
                maximum_commands_per_block: 8,
                ..RealtimeEngineConfig::default()
            },
            8,
            8,
        )
        .unwrap()
    }

    fn activation(
        generation: u64,
        snapshot: Option<Arc<RenderSnapshot>>,
        frame: u64,
    ) -> RealtimeCommand {
        RealtimeCommand::ActivateTimeline(TimelineActivation {
            generation,
            snapshot,
            preserve_transport: false,
            sample_rate: 48_000,
            total_frames: 64,
            frame,
            playing: true,
            loop_range: None,
            metronome: RealtimeMetronome::default(),
        })
    }

    #[test]
    fn authoritative_clock_advances_without_a_render_artifact() {
        let (sender, mut engine) = engine(ChannelLayout::Stereo);
        sender.try_send(activation(7, None, 3)).unwrap();
        let mut output = [1.0_f32; 8];

        assert_eq!(engine.process(&mut output), ProcessStatus::Rendered);

        assert_eq!(sender.active_generation(), 7);
        assert_eq!(sender.audible_generation(), 0);
        assert_eq!(sender.frame_position(), 7);
        assert!(output.iter().all(|sample| sample.abs() < f32::EPSILON));
    }

    #[test]
    fn timeline_snapshot_and_transport_activate_in_one_callback_command() {
        let (sender, mut engine) = RealtimeEngine::new(
            RealtimeEngineConfig {
                output_layout: ChannelLayout::Stereo,
                maximum_block_frames: 8,
                maximum_commands_per_block: 1,
                ..RealtimeEngineConfig::default()
            },
            8,
            8,
        )
        .unwrap();
        sender
            .try_send(activation(
                9,
                Some(snapshot(91, ChannelLayout::Stereo, 64)),
                3,
            ))
            .unwrap();
        let mut output = [0.0_f32; 8];

        assert_eq!(engine.process(&mut output), ProcessStatus::Rendered);

        assert_eq!(sender.active_generation(), 9);
        assert_eq!(sender.audible_generation(), 9);
        assert_eq!(sender.frame_position(), 7);
        assert!((output[0] - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn snapshotless_generation_upgrades_to_audio_without_stopping_its_clock() {
        let (sender, mut engine) = engine(ChannelLayout::Stereo);
        sender.try_send(activation(5, None, 3)).unwrap();
        let mut output = [1.0_f32; 4];
        engine.process(&mut output);
        assert_eq!(sender.frame_position(), 5);
        assert_eq!(sender.audible_generation(), 0);
        assert!(output.iter().all(|sample| sample.abs() < f32::EPSILON));

        sender
            .try_send(activation(
                5,
                Some(snapshot(50, ChannelLayout::Stereo, 64)),
                sender.frame_position(),
            ))
            .unwrap();
        engine.process(&mut output);

        assert_eq!(sender.active_generation(), 5);
        assert_eq!(sender.audible_generation(), 5);
        assert_eq!(sender.frame_position(), 7);
        assert!(engine.transport().playing);
        assert!((output[0] - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn same_generation_snapshot_swap_preserves_callback_transport() {
        let (sender, mut engine) = RealtimeEngine::new(
            RealtimeEngineConfig {
                sample_rate: 44_100,
                output_layout: ChannelLayout::Stereo,
                maximum_block_frames: 8,
                maximum_commands_per_block: 8,
            },
            8,
            8,
        )
        .unwrap();
        sender
            .try_send(activation(
                5,
                Some(snapshot(50, ChannelLayout::Stereo, 64)),
                2,
            ))
            .unwrap();
        let mut output = [0.0_f32; 4];
        engine.process(&mut output);
        assert_eq!(sender.frame_position(), 4);

        let RealtimeCommand::ActivateTimeline(mut replacement) =
            activation(5, Some(snapshot(51, ChannelLayout::Stereo, 96)), 2)
        else {
            unreachable!()
        };
        replacement.preserve_transport = true;
        replacement.playing = false;
        replacement.loop_range = RealtimeLoopRange::new(10, 12).ok();
        replacement.metronome.enabled = true;
        sender
            .try_send(RealtimeCommand::ActivateTimeline(replacement))
            .unwrap();
        engine.process(&mut output);

        assert_eq!(engine.snapshot_revision(), Some(51));
        assert_eq!(engine.transport().loop_range, None);
        assert!(engine.transport().playing);
        assert_eq!(sender.frame_position(), 6);
        let preserved_position = 2.0 + 2.0 * (48_000.0 / 44_100.0);
        assert!((f64::from(output[0]) - preserved_position / 10.0).abs() < 1.0e-6);
    }

    #[test]
    fn stale_timeline_activation_cannot_replace_newer_audio() {
        let (sender, mut engine) = engine(ChannelLayout::Stereo);
        sender
            .try_send(activation(
                11,
                Some(snapshot(11, ChannelLayout::Stereo, 64)),
                2,
            ))
            .unwrap();
        let mut first = [0.0_f32; 4];
        engine.process(&mut first);
        sender.try_send(activation(10, None, 0)).unwrap();
        let mut second = [0.0_f32; 4];
        engine.process(&mut second);

        assert_eq!(sender.active_generation(), 11);
        assert_eq!(sender.audible_generation(), 11);
        assert!(second.iter().any(|sample| sample.abs() > f32::EPSILON));
    }

    #[test]
    fn queued_new_generation_suppresses_old_audio_before_activation() {
        let (sender, mut engine) = RealtimeEngine::new(
            RealtimeEngineConfig {
                output_layout: ChannelLayout::Stereo,
                maximum_block_frames: 8,
                maximum_commands_per_block: 1,
                ..RealtimeEngineConfig::default()
            },
            1,
            8,
        )
        .unwrap();
        sender
            .try_send(activation(
                1,
                Some(snapshot(1, ChannelLayout::Stereo, 64)),
                2,
            ))
            .unwrap();
        let mut output = [0.0_f32; 4];
        engine.process(&mut output);
        assert!(output.iter().any(|sample| sample.abs() > f32::EPSILON));

        sender.try_send(RealtimeCommand::SetGain(1.0)).unwrap();
        let error = sender.try_send(activation(2, None, 4)).unwrap_err();
        assert!(matches!(error, CommandSendError::Full(_)));
        output.fill(1.0);
        engine.process(&mut output);

        assert_eq!(sender.active_generation(), 1);
        assert_eq!(sender.audible_generation(), 0);
        assert!(output.iter().all(|sample| sample.abs() < f32::EPSILON));
    }

    #[test]
    fn direct_generation_invalidation_suppresses_audio_and_metronome_without_queueing() {
        let (sender, mut engine) = engine(ChannelLayout::Stereo);
        let RealtimeCommand::ActivateTimeline(mut current) =
            activation(1, Some(snapshot(1, ChannelLayout::Stereo, 64)), 2)
        else {
            unreachable!()
        };
        current.metronome.enabled = true;
        sender
            .try_send(RealtimeCommand::ActivateTimeline(current))
            .unwrap();
        let mut output = [0.0_f32; 8];
        engine.process(&mut output);
        assert!(output.iter().any(|sample| sample.abs() > f32::EPSILON));

        sender.invalidate_timeline(2);
        output.fill(1.0);
        engine.process(&mut output);

        assert_eq!(sender.active_generation(), 1);
        assert_eq!(sender.audible_generation(), 0);
        assert!(output.iter().all(|sample| sample.abs() < f32::EPSILON));
    }

    #[test]
    fn wav_memory_snapshot_preserves_native_audio_and_metadata() {
        let path = std::env::temp_dir().join(format!(
            "gaw-memory-preview-{}-{:p}.wav",
            std::process::id(),
            &Ramp
        ));
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let expected = [-0.75_f32, 0.25, 0.5, -0.125, 1.0, -1.0];
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for sample in expected {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();

        let snapshot = load_wav_memory_snapshot(&path, 77).unwrap();
        assert_eq!(snapshot.revision(), 77);
        assert_eq!(snapshot.sample_rate(), 44_100);
        assert_eq!(snapshot.layout(), ChannelLayout::Stereo);
        assert_eq!(snapshot.total_frames(), 3);
        let mut rendered = [0.0; 6];
        snapshot.render_native(0, &mut rendered);
        assert_eq!(rendered, expected);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn sample_block_rejects_partial_frames() {
        let mut samples = [0.0; 3];
        assert_eq!(
            SampleBlock::new(&mut samples, ChannelLayout::Stereo).unwrap_err(),
            BlockError::IncompleteFrame {
                samples: 3,
                channels: 2
            }
        );
    }

    #[test]
    fn snapshot_zero_fills_past_declared_tail() {
        let snapshot =
            RenderSnapshot::new(1, 48_000, ChannelLayout::Mono, 2, 1, Arc::new(Ramp)).unwrap();
        let mut output = [9.0; 4];
        snapshot.render_native(1, &mut output);
        assert_eq!(output, [0.1, 0.2, 0.0, 0.0]);
    }

    #[test]
    fn bounded_queue_returns_command_when_full() {
        let (sender, _engine) = RealtimeEngine::new(RealtimeEngineConfig::default(), 1, 1).unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();
        assert!(matches!(
            sender.try_send(RealtimeCommand::Pause),
            Err(CommandSendError::Full(RealtimeCommand::Pause))
        ));
    }

    #[test]
    fn engine_renders_and_advances_transport() {
        let (sender, mut engine) = engine(ChannelLayout::Stereo);
        sender
            .try_send(RealtimeCommand::ActivatePreview(snapshot(
                1,
                ChannelLayout::Mono,
                4,
            )))
            .unwrap();
        sender.try_send(RealtimeCommand::Seek(1)).unwrap();
        sender.try_send(RealtimeCommand::SetGain(0.5)).unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();
        let mut output = [0.0; 4];
        assert_eq!(engine.process(&mut output), ProcessStatus::Rendered);
        assert_eq!(output, [0.05, 0.05, 0.1, 0.1]);
        assert_eq!(engine.transport().frame, 3);
        assert_eq!(sender.frame_position(), 3);
    }

    #[test]
    fn output_peak_reports_and_consumes_the_post_gain_block() {
        let (sender, mut engine) = engine(ChannelLayout::Mono);
        sender
            .try_send(RealtimeCommand::ActivatePreview(snapshot(
                1,
                ChannelLayout::Mono,
                8,
            )))
            .unwrap();
        sender.try_send(RealtimeCommand::Seek(1)).unwrap();
        sender.try_send(RealtimeCommand::SetGain(0.5)).unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();

        assert_eq!(engine.process(&mut [0.0; 4]), ProcessStatus::Rendered);
        assert!((sender.output_peak() - 0.2).abs() < f32::EPSILON);
        assert_eq!(sender.output_peak(), 0.0);
    }

    #[test]
    fn output_peak_excludes_the_audible_metronome() {
        let (sender, mut engine) = engine(ChannelLayout::Mono);
        sender.try_send(activation(7, None, 0)).unwrap();
        sender
            .try_send(RealtimeCommand::SetMetronome(RealtimeMetronome {
                enabled: true,
                ..RealtimeMetronome::default()
            }))
            .unwrap();

        let mut output = [0.0; 8];
        assert_eq!(engine.process(&mut output), ProcessStatus::Rendered);
        assert!(output.iter().any(|sample| sample.abs() > f32::EPSILON));
        assert_eq!(sender.output_peak(), 0.0);
    }

    #[test]
    fn output_peak_holds_the_loudest_block_until_consumed() {
        let (sender, mut engine) = engine(ChannelLayout::Mono);
        sender
            .try_send(RealtimeCommand::ActivatePreview(snapshot(
                1,
                ChannelLayout::Mono,
                8,
            )))
            .unwrap();
        sender.try_send(RealtimeCommand::Seek(4)).unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();
        assert_eq!(engine.process(&mut [0.0; 2]), ProcessStatus::Rendered);
        sender.try_send(RealtimeCommand::Seek(1)).unwrap();
        assert_eq!(engine.process(&mut [0.0; 2]), ProcessStatus::Rendered);

        assert!((sender.output_peak() - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn output_peak_resets_when_the_latest_block_is_silent_or_rejected() {
        let (sender, mut engine) = engine(ChannelLayout::Stereo);
        sender
            .try_send(RealtimeCommand::ActivatePreview(snapshot(
                1,
                ChannelLayout::Mono,
                8,
            )))
            .unwrap();
        sender.try_send(RealtimeCommand::Seek(1)).unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();
        assert_eq!(engine.process(&mut [0.0; 4]), ProcessStatus::Rendered);
        assert!(sender.output_peak() > 0.0);

        sender.try_send(RealtimeCommand::Pause).unwrap();
        assert_eq!(engine.process(&mut [1.0; 4]), ProcessStatus::Silence);
        assert_eq!(sender.output_peak(), 0.0);

        let mut partial = [1.0; 3];
        assert_eq!(engine.process(&mut partial), ProcessStatus::IncompleteFrame);
        assert_eq!(sender.output_peak(), 0.0);
    }

    fn metronome_energy(samples: &[f32]) -> f32 {
        samples.iter().map(|sample| sample * sample).sum()
    }

    fn render_metronome(
        source_position: f64,
        frames: usize,
        metronome: RealtimeMetronome,
    ) -> Vec<f32> {
        let mut output = vec![0.0; frames];
        mix_metronome_segment(
            &mut output,
            ChannelLayout::Mono,
            source_position,
            1.0,
            8_000,
            metronome,
        );
        output
    }

    #[test]
    fn disabled_metronome_is_silent() {
        let output = render_metronome(0.0, 4_000, RealtimeMetronome::default());
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn metronome_gain_controls_click_level() {
        let quiet = RealtimeMetronome {
            enabled: true,
            gain: 0.0,
            ..RealtimeMetronome::default()
        };
        let output = render_metronome(0.0, 4_000, quiet);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn metronome_uses_bpm_and_denominator_for_tick_spacing() {
        let metronome = RealtimeMetronome {
            enabled: true,
            bpm: 240.0,
            numerator: 4,
            denominator: 4,
            gain: 1.0,
        };
        // At 8 kHz and 240 quarter notes/minute, ticks are exactly 2,000 frames apart.
        let first = render_metronome(0.0, 300, metronome);
        let between = render_metronome(1_000.0, 300, metronome);
        let second = render_metronome(2_000.0, 300, metronome);
        assert!(metronome_energy(&first) > 0.0);
        assert_eq!(metronome_energy(&between), 0.0);
        assert!(metronome_energy(&second) > 0.0);

        let eighth_notes = RealtimeMetronome {
            denominator: 8,
            ..metronome
        };
        let eighth_tick = render_metronome(1_000.0, 300, eighth_notes);
        assert!(metronome_energy(&eighth_tick) > 0.0);
    }

    #[test]
    fn metronome_accents_the_measure_downbeat() {
        let metronome = RealtimeMetronome {
            enabled: true,
            bpm: 240.0,
            numerator: 3,
            denominator: 4,
            gain: 1.0,
        };
        let downbeat = render_metronome(0.0, 280, metronome);
        let ordinary = render_metronome(2_000.0, 280, metronome);
        let next_downbeat = render_metronome(6_000.0, 280, metronome);
        assert!(metronome_energy(&downbeat) > metronome_energy(&ordinary));
        assert!((metronome_energy(&downbeat) - metronome_energy(&next_downbeat)).abs() < 0.000_1);
    }

    #[test]
    fn metronome_phase_is_stable_across_seeked_segments() {
        let metronome = RealtimeMetronome {
            enabled: true,
            bpm: 240.0,
            numerator: 4,
            denominator: 4,
            gain: 1.0,
        };
        let complete = render_metronome(0.0, 300, metronome);
        let seeked = render_metronome(125.0, 175, metronome);
        assert_eq!(seeked, complete[125..]);
    }

    #[test]
    fn metronome_maps_project_frames_to_a_different_output_rate() {
        let metronome = RealtimeMetronome {
            enabled: true,
            bpm: 240.0,
            numerator: 4,
            denominator: 4,
            gain: 1.0,
        };
        let mut output = vec![0.0; 1_150];
        // A ratio of two models an 8 kHz project timeline played by a 4 kHz device.
        // The 2,000-project-frame tick must therefore begin at device frame 1,000.
        mix_metronome_segment(&mut output, ChannelLayout::Mono, 0.0, 2.0, 8_000, metronome);
        assert_eq!(metronome_energy(&output[300..900]), 0.0);
        assert!(metronome_energy(&output[1_000..]) > 0.0);
    }

    #[test]
    fn callback_loop_wraps_sample_accurately_across_multiple_boundaries() {
        assert_eq!(
            RealtimeLoopRange::new(3, 3),
            Err(RealtimeLoopRangeError::EmptyOrReversed)
        );
        let (sender, mut engine) = engine(ChannelLayout::Mono);
        sender
            .try_send(RealtimeCommand::ActivatePreview(snapshot(
                1,
                ChannelLayout::Mono,
                8,
            )))
            .unwrap();
        sender
            .try_send(RealtimeCommand::SetLoop(Some(
                RealtimeLoopRange::new(1, 3).unwrap(),
            )))
            .unwrap();
        sender.try_send(RealtimeCommand::Seek(1)).unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();
        let mut output = [0.0; 6];
        assert_eq!(engine.process(&mut output), ProcessStatus::Rendered);
        assert_eq!(output, [0.1, 0.2, 0.1, 0.2, 0.1, 0.2]);
        assert_eq!(engine.transport().frame, 1);
        assert!(engine.transport().playing);

        sender.try_send(RealtimeCommand::SetLoop(None)).unwrap();
        let mut unlooped = [0.0; 2];
        engine.process(&mut unlooped);
        assert_eq!(unlooped, [0.1, 0.2]);
        assert_eq!(engine.transport().frame, 3);
    }

    #[test]
    fn engine_explicitly_downmixes_to_mono() {
        #[derive(Debug)]
        struct Sides;
        impl RealtimeRender for Sides {
            fn render(&self, _: u64, output: &mut SampleBlock<'_>) {
                for frame in output.samples_mut().chunks_exact_mut(2) {
                    frame.copy_from_slice(&[1.0, -0.5]);
                }
            }
        }
        let stereo = Arc::new(
            RenderSnapshot::new(1, 48_000, ChannelLayout::Stereo, 2, 0, Arc::new(Sides)).unwrap(),
        );
        let (sender, mut engine) = engine(ChannelLayout::Mono);
        sender
            .try_send(RealtimeCommand::ActivatePreview(stereo))
            .unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();
        let mut output = [0.0; 2];
        engine.process(&mut output);
        assert_eq!(output, [0.25, 0.25]);
    }

    #[test]
    fn engine_adapts_snapshot_sample_rate_without_callback_reconfiguration() {
        let snapshot = Arc::new(
            RenderSnapshot::new(7, 24_000, ChannelLayout::Mono, 8, 0, Arc::new(Ramp)).unwrap(),
        );
        let (sender, mut engine) = engine(ChannelLayout::Mono);
        sender
            .try_send(RealtimeCommand::ActivatePreview(snapshot))
            .unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();
        let mut output = [0.0; 4];
        assert_eq!(engine.process(&mut output), ProcessStatus::Rendered);
        assert_eq!(output, [0.0, 0.05, 0.1, 0.15]);
    }

    #[test]
    fn realtime_and_offline_float_render_are_sample_identical() {
        let snapshot = snapshot(17, ChannelLayout::Mono, 6);
        let (sender, mut engine) = engine(ChannelLayout::Mono);
        sender
            .try_send(RealtimeCommand::ActivatePreview(Arc::clone(&snapshot)))
            .unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();
        let mut realtime = [0.0; 6];
        assert_eq!(engine.process(&mut realtime[..4]), ProcessStatus::Rendered);
        assert_eq!(engine.process(&mut realtime[4..]), ProcessStatus::Rendered);

        let directory =
            std::env::temp_dir().join(format!("gaw-realtime-offline-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("render.wav");
        render_wav(
            snapshot.as_ref(),
            &path,
            OfflineWavSpec {
                layout: ChannelLayout::Mono,
                block_frames: 3,
                ..OfflineWavSpec::default()
            },
        )
        .unwrap();
        let offline = hound::WavReader::open(&path)
            .unwrap()
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(realtime.as_slice(), offline);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn transport_stops_at_end_including_tail() {
        let (sender, mut engine) = engine(ChannelLayout::Mono);
        let with_tail = Arc::new(
            RenderSnapshot::new(1, 48_000, ChannelLayout::Mono, 2, 2, Arc::new(Ramp)).unwrap(),
        );
        sender
            .try_send(RealtimeCommand::ActivatePreview(with_tail))
            .unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();
        let mut output = [0.0; 8];
        engine.process(&mut output);
        assert_eq!(&output[..4], &[0.0, 0.1, 0.2, 0.3]);
        assert_eq!(&output[4..], &[0.0; 4]);
        assert_eq!(engine.transport().frame, 4);
        assert!(!engine.transport().playing);
    }

    #[test]
    fn command_work_per_block_is_bounded() {
        let (sender, mut engine) = RealtimeEngine::new(
            RealtimeEngineConfig {
                output_layout: ChannelLayout::Mono,
                maximum_block_frames: 1,
                maximum_commands_per_block: 1,
                ..RealtimeEngineConfig::default()
            },
            4,
            4,
        )
        .unwrap();
        sender.try_send(RealtimeCommand::Seek(2)).unwrap();
        sender.try_send(RealtimeCommand::Seek(7)).unwrap();
        engine.process(&mut [0.0]);
        assert_eq!(engine.transport().frame, 2);
        engine.process(&mut [0.0]);
        assert_eq!(engine.transport().frame, 7);
    }

    #[test]
    fn replaced_snapshots_are_reclaimed_off_callback() {
        #[derive(Debug)]
        struct DropRender(Arc<AtomicUsize>);
        impl Drop for DropRender {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        impl RealtimeRender for DropRender {
            fn render(&self, _: u64, output: &mut SampleBlock<'_>) {
                output.clear();
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let make = |revision| {
            Arc::new(
                RenderSnapshot::new(
                    revision,
                    48_000,
                    ChannelLayout::Mono,
                    1,
                    0,
                    Arc::new(DropRender(Arc::clone(&drops))),
                )
                .unwrap(),
            )
        };
        let (sender, mut engine) = engine(ChannelLayout::Mono);
        sender
            .try_send(RealtimeCommand::ActivatePreview(make(1)))
            .unwrap();
        sender
            .try_send(RealtimeCommand::ActivatePreview(make(2)))
            .unwrap();
        engine.process(&mut [0.0]);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(sender.reclaim_retired(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn oversized_and_partial_blocks_are_silenced() {
        let (_sender, mut engine) = engine(ChannelLayout::Stereo);
        let mut partial = [1.0; 3];
        assert_eq!(engine.process(&mut partial), ProcessStatus::IncompleteFrame);
        assert_eq!(partial, [0.0; 3]);
        let mut oversized = [1.0; 18];
        assert_eq!(engine.process(&mut oversized), ProcessStatus::BlockTooLarge);
        assert_eq!(oversized, [0.0; 18]);
    }

    #[test]
    fn pcm_quantization_is_symmetric_and_sanitized() {
        assert_eq!(quantize_i16(-1.0), i16::MIN);
        assert_eq!(quantize_i16(1.0), i16::MAX);
        assert_eq!(quantize_i16(f32::NAN), 0);
        assert_eq!(quantize_i24(-1.0), -8_388_608);
        assert_eq!(quantize_i24(1.0), 8_388_607);
    }

    #[test]
    fn device_config_selection_is_exact_or_explicitly_negotiated() {
        let ranges = || {
            vec![
                cpal::SupportedStreamConfigRange::new(
                    2,
                    44_100,
                    48_000,
                    cpal::SupportedBufferSize::Unknown,
                    SampleFormat::F32,
                ),
                cpal::SupportedStreamConfigRange::new(
                    1,
                    96_000,
                    96_000,
                    cpal::SupportedBufferSize::Unknown,
                    SampleFormat::I16,
                ),
            ]
        };
        let exact = choose_output_config(ranges(), 2, 48_000, None, false).unwrap();
        assert_eq!(exact.channels(), 2);
        assert_eq!(exact.sample_rate(), 48_000);
        assert!(choose_output_config(ranges(), 2, 96_000, None, false).is_none());
        let negotiated = choose_output_config(ranges(), 2, 96_000, None, true).unwrap();
        assert_eq!(negotiated.channels(), 2);
        assert_eq!(negotiated.sample_rate(), 48_000);
    }

    #[test]
    fn callback_buffer_configuration_supports_auto_and_validates_fixed_sizes() {
        let mut config = cpal::StreamConfig {
            channels: 2,
            sample_rate: 48_000,
            buffer_size: BufferSize::Fixed(999),
        };
        let supported = cpal::SupportedBufferSize::Range { min: 64, max: 512 };
        configure_buffer_size(&mut config, &supported, None, 8_192).unwrap();
        assert_eq!(config.buffer_size, BufferSize::Default);

        configure_buffer_size(&mut config, &supported, Some(128), 8_192).unwrap();
        assert_eq!(config.buffer_size, BufferSize::Fixed(128));

        assert!(matches!(
            configure_buffer_size(&mut config, &supported, Some(1_024), 8_192),
            Err(DeviceError::UnsupportedBufferSize { .. })
        ));
        configure_buffer_size(
            &mut config,
            &cpal::SupportedBufferSize::Unknown,
            Some(256),
            8_192,
        )
        .unwrap();
        assert_eq!(config.buffer_size, BufferSize::Fixed(256));
    }

    #[test]
    fn offline_float_wav_is_deterministic_across_block_sizes() {
        let snapshot = snapshot(3, ChannelLayout::Mono, 6);
        let directory = std::env::temp_dir().join(format!(
            "gaw-audio-io-test-{}-{}",
            std::process::id(),
            Arc::as_ptr(&snapshot) as usize
        ));
        fs::create_dir_all(&directory).unwrap();
        let first = directory.join("one.wav");
        let second = directory.join("two.wav");
        let base = OfflineWavSpec {
            layout: ChannelLayout::Stereo,
            sample_rate: Some(44_100),
            encoding: WavEncoding::Float32,
            ..OfflineWavSpec::default()
        };
        let report = render_wav(
            &snapshot,
            &first,
            OfflineWavSpec {
                block_frames: 1,
                ..base
            },
        )
        .unwrap();
        render_wav(
            &snapshot,
            &second,
            OfflineWavSpec {
                block_frames: 4,
                ..base
            },
        )
        .unwrap();
        assert_eq!(report.frames, 6);
        assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
        let reader = hound::WavReader::open(&first).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.duration(), 6);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn band_limited_export_rejects_downsampling_aliases() {
        #[derive(Debug)]
        struct Sine {
            frequency: f32,
        }
        impl RealtimeRender for Sine {
            fn render(&self, start_frame: u64, output: &mut SampleBlock<'_>) {
                for (offset, sample) in output.samples_mut().iter_mut().enumerate() {
                    let frame = start_frame + offset as u64;
                    *sample = (std::f32::consts::TAU * self.frequency * frame as f32 / 48_000.0)
                        .sin()
                        * 0.5;
                }
            }
        }
        let render = |frequency: f32, path: &Path| {
            let snapshot = RenderSnapshot::new(
                u64::from(frequency.to_bits()),
                48_000,
                ChannelLayout::Mono,
                48_000,
                0,
                Arc::new(Sine { frequency }),
            )
            .unwrap();
            render_wav(
                &snapshot,
                path,
                OfflineWavSpec {
                    layout: ChannelLayout::Mono,
                    sample_rate: Some(16_000),
                    ..OfflineWavSpec::default()
                },
            )
            .unwrap();
            hound::WavReader::open(path)
                .unwrap()
                .samples::<f32>()
                .map(Result::unwrap)
                .collect::<Vec<_>>()
        };
        let directory = std::env::temp_dir().join(format!(
            "gaw-audio-alias-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::create_dir_all(&directory).unwrap();
        let pass = render(1_000.0, &directory.join("pass.wav"));
        let stop = render(12_000.0, &directory.join("stop.wav"));
        let rms = |samples: &[f32]| {
            let stable = &samples[1_000..samples.len() - 1_000];
            (stable
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>()
                / stable.len() as f64)
                .sqrt()
        };
        assert!(rms(&stop) < rms(&pass) * 0.01);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn offline_pcm_honors_range_and_tail() {
        let snapshot =
            RenderSnapshot::new(1, 48_000, ChannelLayout::Mono, 3, 2, Arc::new(Ramp)).unwrap();
        let path = std::env::temp_dir().join(format!(
            "gaw-audio-range-{}-{:p}.wav",
            std::process::id(),
            &snapshot
        ));
        let report = render_wav(
            &snapshot,
            &path,
            OfflineWavSpec {
                start_frame: 2,
                frames: Some(99),
                layout: ChannelLayout::Mono,
                sample_rate: None,
                block_frames: 2,
                encoding: WavEncoding::Pcm16,
            },
        )
        .unwrap();
        assert_eq!(report.frames, 3);
        let samples: Vec<i16> = hound::WavReader::open(&path)
            .unwrap()
            .samples::<i16>()
            .map(Result::unwrap)
            .collect();
        assert_eq!(samples, vec![6_553, 9_830, 13_107]);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn resampled_range_keeps_exact_ceil_length_without_filter_tail() {
        let snapshot =
            RenderSnapshot::new(1, 48_000, ChannelLayout::Mono, 3, 2, Arc::new(Ramp)).unwrap();
        let path = std::env::temp_dir().join(format!(
            "gaw-audio-resampled-range-{}-{:p}.wav",
            std::process::id(),
            &snapshot
        ));
        let report = render_wav(
            &snapshot,
            &path,
            OfflineWavSpec {
                start_frame: 2,
                frames: Some(99),
                layout: ChannelLayout::Stereo,
                sample_rate: Some(32_000),
                block_frames: 1,
                encoding: WavEncoding::Float32,
            },
        )
        .unwrap();
        assert_eq!(report.frames, 2);
        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.duration(), 2);
        assert_eq!(reader.spec().channels, 2);
        fs::remove_file(path).unwrap();
    }
}
