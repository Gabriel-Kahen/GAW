//! Render graph, transport, scheduling, device I/O, and materialization.
//!
//! The types in this crate deliberately form an adapter layer between the
//! canonical project model and the DSP implementation. Project files and JSON
//! are compiled into immutable render plans and in-memory asset revisions
//! before they can be observed by the real-time audio callback.

#![forbid(unsafe_code)]

pub mod analysis;
pub mod assets;
pub mod bpm;
pub mod device;
pub mod io;
pub mod mixer;
pub mod project;
pub mod render;
pub mod timeline;

pub use analysis::{
    AnalyzerChannelError, AnalyzerFrameRange, AnalyzerPublication, AnalyzerPublishStatus,
    AnalyzerPublisher, AnalyzerReceiver, analyzer_channel,
};
pub use assets::{
    AssetError, AssetId, AssetProduct, AssetRegistry, AssetRequest, AssetRequestId, AssetResponse,
    AssetRevision, BackgroundAssetWorker, DependencyRevision, FrameSource, MaterializedAsset,
    Materializer, MemoryFrameSource, PagedFrameSource, PagedFrameSourceResidency, PeakBucket,
    RenderContext, RequestedFrameRange, ResolvedRevision, RevisionFreshness, RevisionId,
    WavFrameSource, Waveform, WaveformBucket, WaveformPeak,
};
pub use bpm::{BpmDetection, detect_bpm_wav};
pub use device::{
    DeviceObservation, DeviceRecoveryAction, DeviceRecoveryConfigError, DeviceRecoveryController,
    DeviceRecoveryPolicy, OutputDeviceSelection, RecoveryTarget, StreamGeneration,
    StreamNotification, StreamNotificationChannelError, StreamNotificationReceiveError,
    StreamNotificationReceiver, StreamNotificationSendError, StreamNotificationSender,
    stream_notification_channel,
};
pub use io::{
    BlockError, CommandSendError, CommandSender, CpalOutput, DeviceError, DeviceStreamInfo,
    EngineConfigError, OfflineRenderError, OfflineRenderReport, OfflineWavSpec,
    OpenedOutputDeviceInfo, OutputConfigInfo, OutputDeviceInfo, ProcessStatus, RealtimeCommand,
    RealtimeEngine, RealtimeEngineConfig, RealtimeLoopRange, RealtimeLoopRangeError,
    RealtimeMetronome, RealtimeRender, RenderSnapshot, SampleBlock, SnapshotError,
    StreamRecoveryAction, TransportState as RealtimeTransportState, WavEncoding,
    available_audio_backends, command_queue, enumerate_output_devices, observe_output_devices,
    render_wav, stream_recovery_action,
};
pub use mixer::{
    AssetSourceMap, AssetSourceResolver, MixError, PagedSnapshotBuilder,
    PassthroughProcessorAdapter, PreparedComposition, PreparedPage, PreparedPageCache,
    PreparedPageCacheInsert, PreparedPageCacheStats, PreparedRenderPlan, ProcessorAdapter,
    prepare_render_page, prepare_render_page_for_revision, prepare_render_plan, prepare_snapshot,
};
pub use project::{
    CanonicalTempoStretcher, CompileError, CompiledProject, DspProcessorAdapter, ProjectCompiler,
    StoreCompileError, TempoStretcher, compile_project, compile_project_in_store,
    compile_project_store,
};
pub use render::{
    ChannelLayout, ClipMix, ClipSourceSpec, ClipSpec, CompositionSpec, PlanError, ProcessorSpec,
    RenderClip, RenderComposition, RenderPlan, RenderPlanBuilder, RenderSource, RenderTrack,
    TrackSpec,
};
pub use timeline::{
    Beat, Frame, FrameRounding, LoopRegion, Tempo, TempoError, TimelineError, Transport,
    TransportAdvance, TransportEvent, TransportState,
};
