//! Render graph, transport, scheduling, device I/O, and materialization.
//!
//! The types in this crate deliberately form an adapter layer between the
//! canonical project model and the DSP implementation. Project files and JSON
//! are compiled into immutable render plans and in-memory asset revisions
//! before they can be observed by the real-time audio callback.

#![forbid(unsafe_code)]

pub mod assets;
pub mod io;
pub mod mixer;
pub mod render;
pub mod timeline;

pub use assets::{
    AssetError, AssetId, AssetProduct, AssetRegistry, AssetRequest, AssetRequestId, AssetResponse,
    AssetRevision, BackgroundAssetWorker, DependencyRevision, FrameSource, MaterializedAsset,
    Materializer, MemoryFrameSource, PeakBucket, RenderContext, ResolvedRevision,
    RevisionFreshness, RevisionId, Waveform, WaveformBucket, WaveformPeak,
};
pub use io::{
    BlockError, CommandSendError, CommandSender, CpalOutput, DeviceError, DeviceStreamInfo,
    EngineConfigError, OfflineRenderError, OfflineRenderReport, OfflineWavSpec, ProcessStatus,
    RealtimeCommand, RealtimeEngine, RealtimeEngineConfig, RealtimeRender, RenderSnapshot,
    SampleBlock, SnapshotError, TransportState as RealtimeTransportState, WavEncoding,
    command_queue, render_wav,
};
pub use mixer::{
    AssetSourceMap, AssetSourceResolver, MixError, PassthroughProcessorAdapter,
    PreparedComposition, PreparedRenderPlan, ProcessorAdapter, prepare_render_plan,
    prepare_snapshot,
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
