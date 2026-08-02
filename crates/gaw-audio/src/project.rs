//! Compilation of validated canonical projects into prepared audio snapshots.
//!
//! Every operation in this module is control-plane work. Project validation,
//! source decoding, time conversion, stretching, sampler evaluation, DSP
//! preparation, allocation, and hashing all finish before a snapshot is
//! published to the audio callback.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_lines
)]

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    fs::{File, OpenOptions},
    io::{Read, Seek},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use audioadapter_buffers::direct::InterleavedSlice;
use gaw_core::{
    AudioAssetDefinition, AudioTransform, AutomationTarget, AutomationValue, Clip, Event, Fade,
    FadeCurve, InstrumentKind, Project, SamplerPlayback, TempoSync, TrackKind, Validate,
    VoiceStealing, processors::ProcessorKind,
};
use gaw_dsp::{
    AudioLayout as DspLayout, Instrument as _, PrepareSpec, ProcessContext,
    Processor as DspProcessor,
};
use gaw_project::ProjectStore;
use parking_lot::RwLock;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    AnalyzerChannelError, AnalyzerFrameRange, AnalyzerPublisher, AnalyzerReceiver, AssetSourceMap,
    AssetSourceResolver, Beat, ChannelLayout, ClipSourceSpec, ClipSpec, CompositionSpec,
    FrameSource, MemoryFrameSource, MixError, PagedFrameSource, PagedSnapshotBuilder, PreparedPage,
    PreparedRenderPlan, ProcessorAdapter, ProcessorSpec, RenderPlan, RenderPlanBuilder,
    RenderSnapshot, Tempo, TrackSpec, WavFrameSource, analyzer_channel,
    prepare_render_page_for_revision, prepare_render_plan,
};

const PROCESS_BLOCK_FRAMES: usize = 4_096;
const DERIVED_RESIDENT_PAGES: usize = 4;
static DERIVED_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const ANALYZER_NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// Replaceable pitch-preserving tempo engine used during project compilation.
pub trait TempoStretcher: fmt::Debug + Send + Sync {
    fn stretch(
        &self,
        input: &[f32],
        layout: ChannelLayout,
        sample_rate: u32,
        output_frames: usize,
    ) -> Result<Vec<f32>, String>;

    /// Streams one complete source to a bounded sink. Implementations must keep
    /// working storage independent of the source duration.
    fn stretch_source(
        &self,
        source: &dyn FrameSource,
        layout: ChannelLayout,
        sample_rate: u32,
        output_frames: usize,
        emit: &mut dyn FnMut(&[f32]) -> Result<(), String>,
    ) -> Result<(), String>;
}

/// Canonical Signalsmith implementation of [`TempoStretcher`].
#[derive(Clone, Copy, Debug, Default)]
pub struct CanonicalTempoStretcher;

impl TempoStretcher for CanonicalTempoStretcher {
    fn stretch(
        &self,
        input: &[f32],
        layout: ChannelLayout,
        sample_rate: u32,
        output_frames: usize,
    ) -> Result<Vec<f32>, String> {
        let channels = layout.channels();
        let mut stretcher = gaw_stretch::TimeStretcher::new(gaw_stretch::Config {
            channels: u8::try_from(channels).map_err(|error| error.to_string())?,
            sample_rate,
            quality: gaw_stretch::Quality::Canonical,
        })
        .map_err(|error| error.to_string())?;
        // Signalsmith exact rendering requires both a priming window and at
        // least two output-latency windows. Padding is preparation-only.
        let input_frames = input.len() / channels;
        let padded_frames = input_frames.max(stretcher.input_latency());
        let requested_frames = output_frames.max(stretcher.output_latency().saturating_mul(2));
        let mut padded = vec![0.0; padded_frames.saturating_mul(channels)];
        padded[..input.len()].copy_from_slice(input);
        let mut output = vec![0.0; requested_frames.saturating_mul(channels)];
        if !stretcher
            .exact(&padded, &mut output)
            .map_err(|error| error.to_string())?
        {
            return Err("Signalsmith rejected the requested exact stretch".into());
        }
        output.truncate(output_frames.saturating_mul(channels));
        Ok(output)
    }

    fn stretch_source(
        &self,
        source: &dyn FrameSource,
        layout: ChannelLayout,
        sample_rate: u32,
        output_frames: usize,
        emit: &mut dyn FnMut(&[f32]) -> Result<(), String>,
    ) -> Result<(), String> {
        let channels = layout.channels();
        let input_frames =
            usize::try_from(source.frame_count()).map_err(|error| error.to_string())?;
        if input_frames == 0 || output_frames == 0 {
            return Ok(());
        }
        let mut stretcher = gaw_stretch::TimeStretcher::new(gaw_stretch::Config {
            channels: u8::try_from(channels).map_err(|error| error.to_string())?,
            sample_rate,
            quality: gaw_stretch::Quality::Canonical,
        })
        .map_err(|error| error.to_string())?;
        let mut input = vec![0.0; PROCESS_BLOCK_FRAMES * channels];
        let maximum_output = ((PROCESS_BLOCK_FRAMES as f64 * output_frames as f64
            / input_frames as f64)
            .ceil() as usize)
            .saturating_add(stretcher.output_latency());
        let mut output = vec![0.0; maximum_output.max(1).saturating_mul(channels)];
        let mut consumed = 0_usize;
        let mut emitted = 0_usize;
        while consumed < input_frames {
            let frames = (input_frames - consumed).min(PROCESS_BLOCK_FRAMES);
            let read = source
                .read_interleaved(consumed as u64, &mut input[..frames * channels])
                .map_err(|error| error.to_string())?;
            if read != frames {
                return Err(format!("source ended at frame {}", consumed + read));
            }
            let next_consumed = consumed + frames;
            let target = ((next_consumed as u128 * output_frames as u128
                + input_frames as u128 / 2)
                / input_frames as u128) as usize;
            let produced = target.saturating_sub(emitted);
            stretcher
                .process(
                    &input[..frames * channels],
                    &mut output[..produced * channels],
                )
                .map_err(|error| error.to_string())?;
            emit(&output[..produced * channels])?;
            consumed = next_consumed;
            emitted = target;
        }
        Ok(())
    }
}

/// The immutable sidecar joining an old-format render plan to canonical DSP definitions.
#[derive(Debug)]
pub struct CompiledProject {
    plan: RenderPlan,
    sources: AssetSourceMap,
    processors: DspProcessorAdapter,
    revision: u64,
}

impl CompiledProject {
    pub const fn plan(&self) -> &RenderPlan {
        &self.plan
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Creates and attaches a bounded analyzer channel for this immutable revision.
    pub fn analyzer_channel(
        &self,
        capacity: usize,
    ) -> Result<AnalyzerReceiver, AnalyzerChannelError> {
        let (publisher, receiver) = analyzer_channel(capacity, self.revision)?;
        self.attach_analyzer_publisher(publisher);
        Ok(receiver)
    }

    /// Routes analyzer measurements produced by subsequent preparation work.
    pub fn attach_analyzer_publisher(&self, publisher: AnalyzerPublisher) {
        self.processors.set_analyzer_publisher(publisher);
    }

    /// Materializes child-first audio off the callback.
    pub fn prepare(&self) -> Result<PreparedRenderPlan, MixError> {
        prepare_render_plan(&self.plan, &self.sources, &self.processors)
    }

    /// Builds a snapshot using the deterministic project/dependency revision.
    pub fn snapshot(&self) -> Result<RenderSnapshot, MixError> {
        self.prepare()?.snapshot(self.revision)
    }

    /// Prepares one independently replaceable page without bouncing the project.
    pub fn prepare_page(&self, start_frame: u64, frames: usize) -> Result<PreparedPage, MixError> {
        prepare_render_page_for_revision(
            self.revision,
            &self.plan,
            start_frame,
            frames,
            &self.sources,
            &self.processors,
        )
    }

    /// Creates a sparse snapshot. Unprepared ranges are silent until a newer
    /// snapshot containing those pages is atomically published.
    pub fn paged_snapshot(
        &self,
        pages: impl IntoIterator<Item = PreparedPage>,
    ) -> Result<RenderSnapshot, MixError> {
        let mut builder = PagedSnapshotBuilder::for_revision(&self.plan, self.revision);
        for page in pages {
            builder.insert(page)?;
        }
        builder.snapshot(self.revision)
    }
}

/// Stateful configuration used to compile one or more project revisions.
#[derive(Debug)]
pub struct ProjectCompiler<'a> {
    stretcher: &'a dyn TempoStretcher,
    cache_directory: Option<PathBuf>,
}

impl<'a> ProjectCompiler<'a> {
    pub const fn new(stretcher: &'a dyn TempoStretcher) -> Self {
        Self {
            stretcher,
            cache_directory: None,
        }
    }

    /// Selects the disposable directory used for bounded derived-audio stages.
    #[must_use]
    pub fn with_cache_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.cache_directory = Some(directory.into());
        self
    }

    /// Validates and compiles a canonical project plus decoded source audio.
    pub fn compile(
        &self,
        project: &Project,
        decoded: &dyn AssetSourceResolver,
    ) -> Result<CompiledProject, CompileError> {
        project.validate()?;
        if let Some(lane) = project
            .automation
            .iter()
            .find(|lane| matches!(lane.target, AutomationTarget::Instrument { .. }))
        {
            return Err(CompileError::Unsupported(format!(
                "instrument automation lane `{}` cannot be mapped to the current sampler DSP API",
                lane.id
            )));
        }
        let sample_rate = project.sample_rate.value();
        let tempo = Tempo::new(project.bpm.value(), sample_rate)?;
        let tail_cap = seconds_to_frames(project.settings.maximum_tail.value(), sample_rate)?;
        let revision = project_revision(project)?;
        let processors = DspProcessorAdapter::new(
            project,
            project.bpm.value(),
            project.settings.random_seed,
            revision,
        );
        let cache_directory = self.cache_directory.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("gaw-audio-derived-{}", std::process::id()))
        });
        let mut assets = HashMap::new();
        let mut render_sources = HashMap::new();
        let mut visiting = Vec::new();
        let mut sources = AssetSourceMap::new();
        let mut builder = RenderPlanBuilder::new(tempo, tail_cap);
        for composition in &project.compositions {
            let layout = layout(composition.output_layout);
            let mut spec = CompositionSpec::new(
                composition.id.to_string(),
                Beat::new(composition.length.value())?,
                layout,
            );
            spec.processors = processor_specs(&processors, &composition.output_effects, layout)?;
            let owned_tracks: Vec<_> = composition
                .track_ids
                .iter()
                .map(|id| {
                    project
                        .tracks
                        .iter()
                        .find(|track| track.id == *id)
                        .expect("validated track reference")
                })
                .collect();
            let any_solo = owned_tracks.iter().any(|track| track.solo && !track.muted);
            for track in owned_tracks {
                if track.muted || (any_solo && !track.solo) {
                    continue;
                }
                let mut track_spec = TrackSpec::new(track.id.to_string());
                track_spec.processors = processor_specs(&processors, &track.effects, layout)?;
                for clip in &track.clips {
                    let clip_spec = match clip {
                        Clip::Audio(audio) => {
                            let source_id = format!("clip:{}", audio.id);
                            if !audio.muted {
                                if let Some(source) = lazy_audio_clip(project, audio, decoded)? {
                                    sources.insert(source_id.clone(), source);
                                } else {
                                    let source = resolve_asset_source(
                                        project,
                                        audio.asset_id,
                                        decoded,
                                        &processors,
                                        self.stretcher,
                                        &cache_directory,
                                        &mut render_sources,
                                        &mut visiting,
                                    )?;
                                    let rendered = render_audio_clip_source(
                                        project,
                                        audio,
                                        source,
                                        self.stretcher,
                                        &cache_directory,
                                    )?;
                                    sources.insert(source_id.clone(), rendered.source);
                                }
                            }
                            let mut value = ClipSpec::new(
                                audio.id.to_string(),
                                Beat::new(audio.start.value())?,
                                Beat::new(audio.duration.value())?,
                                ClipSourceSpec::audio(source_id, 0),
                            );
                            value.muted = audio.muted;
                            value.processors =
                                processor_specs(&processors, &audio.effects, layout)?;
                            value
                        }
                        Clip::Composition(child) => {
                            let source_offset = tempo
                                .frame_at(Beat::new(child.source_start.value())?)?
                                .get()
                                .cast_unsigned();
                            let mut value = ClipSpec::new(
                                child.id.to_string(),
                                Beat::new(child.start.value())?,
                                Beat::new(child.duration.value())?,
                                ClipSourceSpec::composition(
                                    child.composition_id.to_string(),
                                    source_offset,
                                ),
                            );
                            value.muted = child.muted;
                            value.processors =
                                processor_specs(&processors, &child.effects, layout)?;
                            value
                        }
                        Clip::Event(event) => {
                            let source_id = format!("event:{}", event.id);
                            let mut source_tail = 0;
                            if !event.muted {
                                let InstrumentKind::Sampler(sampler) = &track
                                    .instrument
                                    .as_ref()
                                    .expect("validated event instrument")
                                    .kind;
                                for zone in &sampler.zones {
                                    materialize_asset(
                                        project,
                                        zone.asset_id,
                                        decoded,
                                        &processors,
                                        self.stretcher,
                                        &mut assets,
                                        &mut visiting,
                                    )?;
                                }
                                let rendered = render_event_clip(
                                    project, track, event, &assets, layout, tail_cap,
                                )?;
                                source_tail = rendered
                                    .frames()
                                    .saturating_sub(beat_duration_frames(tempo, event.duration)?)
                                    as u64;
                                sources.insert(source_id.clone(), memory_source(&rendered)?);
                            }
                            let mut value = ClipSpec::new(
                                event.id.to_string(),
                                Beat::new(event.start.value())?,
                                Beat::new(event.duration.value())?,
                                ClipSourceSpec::audio(source_id, 0),
                            );
                            value.muted = event.muted;
                            value.source_tail_frames = source_tail;
                            value
                        }
                    };
                    if track.kind == TrackKind::Event {
                        debug_assert!(clip_spec.processors.is_empty());
                    }
                    track_spec.clips.push(clip_spec);
                }
                spec.tracks.push(track_spec);
            }
            builder.add_composition(spec);
        }
        let plan = builder.build(&project.root_composition_id.to_string())?;
        Ok(CompiledProject {
            plan,
            sources,
            processors,
            revision,
        })
    }
}

/// Compile with the canonical Signalsmith tempo engine.
pub fn compile_project(
    project: &Project,
    decoded: &dyn AssetSourceResolver,
) -> Result<CompiledProject, CompileError> {
    ProjectCompiler::new(&CanonicalTempoStretcher).compile(project, decoded)
}

/// Loads a canonical project store, verifies and decodes its imported WAVs,
/// and compiles the resulting immutable render graph.
pub fn compile_project_store(store: &ProjectStore) -> Result<CompiledProject, StoreCompileError> {
    let project = store.load_project()?;
    let media = StoreMediaResolver(store);
    let mut decoded = AssetSourceMap::new();
    for asset in &project.assets {
        let AudioAssetDefinition::Imported(imported) = &asset.definition else {
            continue;
        };
        let file = media.open_verified(&imported.media_path, &imported.content_hash)?;
        let source = WavFrameSource::from_file(PathBuf::from(imported.media_path.as_str()), file)?;
        let expected_channels = match imported.layout {
            gaw_core::ChannelLayout::Mono => 1,
            gaw_core::ChannelLayout::Stereo => 2,
        };
        let actual_channels = u16::try_from(source.channel_layout().channels()).unwrap_or(u16::MAX);
        if actual_channels != expected_channels
            || source.sample_rate() != imported.sample_rate.value()
            || source.frame_count() != imported.frames.0
        {
            return Err(StoreCompileError::Metadata {
                asset: asset.id.to_string(),
                expected_channels,
                actual_channels,
                expected_sample_rate: imported.sample_rate.value(),
                actual_sample_rate: source.sample_rate(),
                expected_frames: imported.frames.0,
                actual_frames: source.frame_count(),
            });
        }
        let source: Arc<dyn FrameSource> = Arc::new(source);
        let source = Arc::new(PagedFrameSource::new(source, PROCESS_BLOCK_FRAMES, 8)?);
        decoded.insert(asset.id.to_string(), source);
    }
    Ok(ProjectCompiler::new(&CanonicalTempoStretcher)
        .with_cache_directory(store.root().join(".gaw/cache/audio"))
        .compile(&project, &decoded)?)
}

trait VerifiedMediaResolver {
    fn open_verified(
        &self,
        path: &gaw_core::ProjectPath,
        expected_hash: &gaw_core::ContentHash,
    ) -> Result<File, StoreCompileError>;
}

struct StoreMediaResolver<'a>(&'a ProjectStore);

impl VerifiedMediaResolver for StoreMediaResolver<'_> {
    fn open_verified(
        &self,
        path: &gaw_core::ProjectPath,
        expected_hash: &gaw_core::ContentHash,
    ) -> Result<File, StoreCompileError> {
        let mut file = self.0.open_media(path, expected_hash)?;
        let target = PathBuf::from(path.as_str());
        let mut hasher = Sha256::new();
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|source| StoreCompileError::MediaIo {
                    path: target.clone(),
                    source,
                })?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let actual = format!("{:x}", hasher.finalize());
        if actual != expected_hash.as_str() {
            return Err(StoreCompileError::MediaHash(path.as_str().into()));
        }
        file.seek(std::io::SeekFrom::Start(0))
            .map_err(|source| StoreCompileError::MediaIo {
                path: target,
                source,
            })?;
        Ok(file)
    }
}

#[derive(Debug, Error)]
pub enum StoreCompileError {
    #[error(transparent)]
    Project(#[from] gaw_project::Error),
    #[error(transparent)]
    Wav(#[from] hound::Error),
    #[error(transparent)]
    Asset(#[from] crate::AssetError),
    #[error(transparent)]
    Compile(#[from] CompileError),
    #[error("project media I/O failed at {path}: {source}")]
    MediaIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("project media content hash does not match for `{0}`")]
    MediaHash(String),
    #[error(
        "asset `{asset}` WAV metadata mismatch (expected {expected_channels}ch/{expected_sample_rate}Hz/{expected_frames} frames, found {actual_channels}ch/{actual_sample_rate}Hz/{actual_frames} frames)"
    )]
    Metadata {
        asset: String,
        expected_channels: u16,
        actual_channels: u16,
        expected_sample_rate: u32,
        actual_sample_rate: u32,
        expected_frames: u64,
        actual_frames: u64,
    },
    #[error(
        "asset `{asset}` uses unsupported {bits_per_sample}-bit {sample_format:?} WAV encoding"
    )]
    UnsupportedWav {
        asset: String,
        bits_per_sample: u16,
        sample_format: hound::SampleFormat,
    },
    #[error("asset `{0}` WAV contains a non-finite sample")]
    NonFiniteSample(String),
}

#[derive(Debug, Error)]
pub enum CompileError {
    #[error(transparent)]
    InvalidProject(#[from] gaw_core::DomainError),
    #[error(transparent)]
    Model(#[from] gaw_core::ModelError),
    #[error(transparent)]
    Timeline(#[from] crate::TimelineError),
    #[error(transparent)]
    Plan(#[from] crate::PlanError),
    #[error(transparent)]
    Asset(#[from] crate::AssetError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Wav(#[from] hound::Error),
    #[error("decoded source for asset `{0}` is missing")]
    MissingDecodedAsset(String),
    #[error("asset `{asset}` layout is {actual:?}, expected {expected:?}")]
    AssetLayout {
        asset: String,
        actual: ChannelLayout,
        expected: ChannelLayout,
    },
    #[error("processor `{processor}` cannot be mapped: {message}")]
    Processor { processor: String, message: String },
    #[error("tempo processing failed: {0}")]
    Tempo(String),
    #[error("unsupported canonical behavior: {0}")]
    Unsupported(String),
    #[error("frame/sample storage overflow")]
    Overflow,
    #[error("project revision could not be encoded: {0}")]
    Revision(serde_json::Error),
}

#[derive(Clone, Debug)]
struct AudioBuffer {
    layout: ChannelLayout,
    samples: Vec<f32>,
}

#[derive(Clone, Debug)]
struct DerivedSource {
    key: String,
    source: Arc<dyn FrameSource>,
}

#[derive(Debug)]
struct ReverseFrameSource {
    source: Arc<dyn FrameSource>,
}

impl FrameSource for ReverseFrameSource {
    fn frame_count(&self) -> u64 {
        self.source.frame_count()
    }

    fn channel_layout(&self) -> ChannelLayout {
        self.source.channel_layout()
    }

    fn read_interleaved(
        &self,
        start_frame: u64,
        output: &mut [f32],
    ) -> Result<usize, crate::AssetError> {
        let channels = self.channel_layout().channels();
        if !output.len().is_multiple_of(channels) {
            return Err(crate::AssetError::BufferNotFrameAligned {
                samples: output.len(),
                channels,
            });
        }
        let frames = (output.len() / channels).min(
            usize::try_from(self.frame_count().saturating_sub(start_frame)).unwrap_or(usize::MAX),
        );
        if frames == 0 {
            return Ok(0);
        }
        let source_start = self
            .frame_count()
            .saturating_sub(start_frame)
            .saturating_sub(frames as u64);
        let read = self
            .source
            .read_interleaved(source_start, &mut output[..frames * channels])?;
        if read != frames {
            return Err(crate::AssetError::SourceEndedEarly {
                frame: source_start.saturating_add(read as u64),
            });
        }
        reverse_interleaved(&mut output[..read * channels], channels);
        Ok(read)
    }
}

#[derive(Debug)]
struct FadeFrameSource {
    source: Arc<dyn FrameSource>,
    fade: Fade,
    fade_in: bool,
    fade_frames: u64,
}

impl FrameSource for FadeFrameSource {
    fn frame_count(&self) -> u64 {
        self.source.frame_count()
    }

    fn channel_layout(&self) -> ChannelLayout {
        self.source.channel_layout()
    }

    fn read_interleaved(
        &self,
        start_frame: u64,
        output: &mut [f32],
    ) -> Result<usize, crate::AssetError> {
        let channels = self.channel_layout().channels();
        let read = self.source.read_interleaved(start_frame, output)?;
        let fade_frames = self.fade_frames.min(self.frame_count());
        for (offset, frame) in output[..read * channels]
            .chunks_exact_mut(channels)
            .enumerate()
        {
            let position = start_frame.saturating_add(offset as u64);
            let fade_index = if self.fade_in {
                position
            } else {
                self.frame_count()
                    .saturating_sub(position)
                    .saturating_sub(1)
            };
            if fade_index >= fade_frames {
                continue;
            }
            let t = if fade_frames <= 1 {
                0.0
            } else {
                fade_index as f32 / (fade_frames - 1) as f32
            };
            let gain = match self.fade.curve {
                FadeCurve::Linear => t,
                FadeCurve::EqualPower => (t * std::f32::consts::FRAC_PI_2).sin(),
                FadeCurve::Exponential => t * t * t,
            };
            for sample in frame {
                *sample *= gain;
            }
        }
        Ok(read)
    }
}

#[derive(Debug)]
struct ZeroPaddedFrameSource {
    source: Arc<dyn FrameSource>,
    frame_count: u64,
}

impl FrameSource for ZeroPaddedFrameSource {
    fn frame_count(&self) -> u64 {
        self.frame_count
    }

    fn channel_layout(&self) -> ChannelLayout {
        self.source.channel_layout()
    }

    fn read_interleaved(
        &self,
        start_frame: u64,
        output: &mut [f32],
    ) -> Result<usize, crate::AssetError> {
        let channels = self.channel_layout().channels();
        if !output.len().is_multiple_of(channels) {
            return Err(crate::AssetError::BufferNotFrameAligned {
                samples: output.len(),
                channels,
            });
        }
        let frames = (output.len() / channels).min(
            usize::try_from(self.frame_count.saturating_sub(start_frame)).unwrap_or(usize::MAX),
        );
        if frames == 0 {
            return Ok(0);
        }
        let destination = &mut output[..frames * channels];
        destination.fill(0.0);
        let source_frames = frames.min(
            usize::try_from(self.source.frame_count().saturating_sub(start_frame))
                .unwrap_or(usize::MAX),
        );
        if source_frames != 0 {
            let read = self
                .source
                .read_interleaved(start_frame, &mut destination[..source_frames * channels])?;
            if read != source_frames {
                return Err(crate::AssetError::SourceEndedEarly {
                    frame: start_frame.saturating_add(read as u64),
                });
            }
        }
        Ok(frames)
    }
}

fn reverse_interleaved(samples: &mut [f32], channels: usize) {
    let frames = samples.len() / channels;
    for left in 0..frames / 2 {
        let right = frames - 1 - left;
        for channel in 0..channels {
            samples.swap(left * channels + channel, right * channels + channel);
        }
    }
}

#[derive(Debug)]
struct SlicedFrameSource {
    source: Arc<dyn FrameSource>,
    start_frame: u64,
    frame_count: u64,
}

impl FrameSource for SlicedFrameSource {
    fn frame_count(&self) -> u64 {
        self.frame_count
    }

    fn channel_layout(&self) -> ChannelLayout {
        self.source.channel_layout()
    }

    fn read_interleaved(
        &self,
        start_frame: u64,
        output: &mut [f32],
    ) -> Result<usize, crate::AssetError> {
        let channels = self.channel_layout().channels();
        if !output.len().is_multiple_of(channels) {
            return Err(crate::AssetError::BufferNotFrameAligned {
                samples: output.len(),
                channels,
            });
        }
        let frames = (output.len() / channels).min(
            usize::try_from(self.frame_count.saturating_sub(start_frame)).unwrap_or(usize::MAX),
        );
        if frames == 0 {
            return Ok(0);
        }
        self.source.read_interleaved(
            self.start_frame.saturating_add(start_frame),
            &mut output[..frames * channels],
        )
    }
}

fn lazy_audio_clip(
    project: &Project,
    clip: &gaw_core::AudioClip,
    decoded: &dyn AssetSourceResolver,
) -> Result<Option<Arc<dyn FrameSource>>, CompileError> {
    if clip.reverse
        || clip.tempo_sync != TempoSync::None
        || clip.fade_in.is_some()
        || clip.fade_out.is_some()
    {
        return Ok(None);
    }
    let asset = project
        .assets
        .iter()
        .find(|asset| asset.id == clip.asset_id)
        .expect("validated asset");
    if matches!(asset.definition, AudioAssetDefinition::Processed { .. }) {
        return Ok(None);
    }
    if let AudioAssetDefinition::Imported(imported) = &asset.definition
        && imported.sample_rate != project.sample_rate
    {
        return Ok(None);
    }
    let source = decoded
        .resolve(&clip.asset_id.to_string())
        .ok_or_else(|| CompileError::MissingDecodedAsset(clip.asset_id.to_string()))?;
    if let AudioAssetDefinition::Imported(imported) = &asset.definition {
        let expected = layout(imported.layout);
        if source.channel_layout() != expected {
            return Err(CompileError::AssetLayout {
                asset: clip.asset_id.to_string(),
                actual: source.channel_layout(),
                expected,
            });
        }
    }
    let start_frame = seconds_to_frames(clip.source.start.value(), project.sample_rate.value())?;
    let frame_count = seconds_to_frames(clip.source.duration.value(), project.sample_rate.value())?;
    Ok(Some(Arc::new(SlicedFrameSource {
        source,
        start_frame,
        frame_count,
    })))
}

impl AudioBuffer {
    fn frames(&self) -> usize {
        self.samples.len() / self.layout.channels()
    }
}

fn memory_source(audio: &AudioBuffer) -> Result<Arc<dyn FrameSource>, crate::AssetError> {
    Ok(Arc::new(MemoryFrameSource::new(
        audio.layout,
        Arc::<[f32]>::from(audio.samples.clone()),
    )?))
}

fn layout(value: gaw_core::ChannelLayout) -> ChannelLayout {
    match value {
        gaw_core::ChannelLayout::Mono => ChannelLayout::Mono,
        gaw_core::ChannelLayout::Stereo => ChannelLayout::Stereo,
    }
}

fn dsp_layout(value: ChannelLayout) -> DspLayout {
    match value {
        ChannelLayout::Mono => DspLayout::Mono,
        ChannelLayout::Stereo => DspLayout::Stereo,
    }
}

fn seconds_to_frames(seconds: f64, sample_rate: u32) -> Result<u64, CompileError> {
    let frames = (seconds * f64::from(sample_rate)).ceil();
    if frames.is_finite() && frames <= u64::MAX as f64 {
        Ok(frames as u64)
    } else {
        Err(CompileError::Overflow)
    }
}

fn beat_duration_frames(tempo: Tempo, beats: gaw_core::Beats) -> Result<usize, CompileError> {
    usize::try_from(tempo.frame_at(Beat::new(beats.value())?)?.get())
        .map_err(|_| CompileError::Overflow)
}

fn derived_key(
    parent: &str,
    label: &str,
    value: &impl Serialize,
    project: &Project,
) -> Result<String, CompileError> {
    let mut digest = Sha256::new();
    digest.update(parent.as_bytes());
    digest.update(label.as_bytes());
    digest.update(serde_json::to_vec(value).map_err(CompileError::Revision)?);
    digest.update(project.sample_rate.value().to_le_bytes());
    digest.update(project.bpm.value().to_bits().to_le_bytes());
    digest.update(project.settings.random_seed.to_le_bytes());
    digest.update(b"gaw-audio-derived-v1");
    Ok(format!("{:x}", digest.finalize()))
}

fn imported_key(asset: &gaw_core::AudioAsset, project: &Project) -> Result<String, CompileError> {
    derived_key("imported", "asset", asset, project)
}

fn paged_source(source: Arc<dyn FrameSource>) -> Result<Arc<dyn FrameSource>, CompileError> {
    Ok(Arc::new(PagedFrameSource::new(
        source,
        PROCESS_BLOCK_FRAMES,
        DERIVED_RESIDENT_PAGES,
    )?))
}

fn cached_source(
    directory: &Path,
    key: &str,
    sample_rate: u32,
    layout: ChannelLayout,
    frames: u64,
    render: impl FnOnce(&Path) -> Result<(), CompileError>,
) -> Result<Arc<dyn FrameSource>, CompileError> {
    std::fs::create_dir_all(directory)?;
    let target = directory.join(format!("{key}.wav"));
    if !cached_source_matches(&target, sample_rate, layout, frames) {
        let sequence = DERIVED_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = directory.join(format!(".{key}.{}.{sequence}.tmp.wav", std::process::id()));
        let _ = std::fs::remove_file(&temporary);
        if let Err(error) = render(&temporary) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
        if cached_source_matches(&target, sample_rate, layout, frames) {
            std::fs::remove_file(&temporary)?;
        } else {
            if target.exists() {
                std::fs::remove_file(&target)?;
            }
            std::fs::rename(&temporary, &target)?;
        }
    }
    let source: Arc<dyn FrameSource> = Arc::new(WavFrameSource::open(target)?);
    paged_source(source)
}

fn cached_source_matches(
    path: &Path,
    sample_rate: u32,
    layout: ChannelLayout,
    frames: u64,
) -> bool {
    let Ok(reader) = hound::WavReader::open(path) else {
        return false;
    };
    let spec = reader.spec();
    spec.sample_rate == sample_rate
        && usize::from(spec.channels) == layout.channels()
        && spec.sample_format == hound::SampleFormat::Float
        && spec.bits_per_sample == 32
        && u64::from(reader.duration()) == frames
}

fn wav_writer(
    path: &Path,
    sample_rate: u32,
    layout: ChannelLayout,
) -> Result<hound::WavWriter<File>, CompileError> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    Ok(hound::WavWriter::new(
        file,
        hound::WavSpec {
            channels: u16::try_from(layout.channels()).unwrap_or(2),
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )?)
}

#[allow(clippy::too_many_arguments)]
fn resolve_asset_source(
    project: &Project,
    id: gaw_core::AssetId,
    decoded: &dyn AssetSourceResolver,
    processors: &DspProcessorAdapter,
    stretcher: &dyn TempoStretcher,
    cache_directory: &Path,
    cache: &mut HashMap<String, DerivedSource>,
    visiting: &mut Vec<gaw_core::AssetId>,
) -> Result<DerivedSource, CompileError> {
    let id_string = id.to_string();
    if let Some(source) = cache.get(&id_string) {
        return Ok(source.clone());
    }
    if visiting.contains(&id) {
        return Err(CompileError::Unsupported(format!(
            "asset dependency cycle at {id}"
        )));
    }
    visiting.push(id);
    let asset = project
        .assets
        .iter()
        .find(|asset| asset.id == id)
        .expect("validated asset");
    let mut result = match &asset.definition {
        AudioAssetDefinition::Imported(imported) => {
            let source = decoded
                .resolve(&id_string)
                .ok_or_else(|| CompileError::MissingDecodedAsset(id_string.clone()))?;
            let expected = layout(imported.layout);
            if source.channel_layout() != expected {
                return Err(CompileError::AssetLayout {
                    asset: id_string,
                    actual: source.channel_layout(),
                    expected,
                });
            }
            let mut result = DerivedSource {
                key: imported_key(asset, project)?,
                source,
            };
            if imported.sample_rate != project.sample_rate {
                result = repitch_source(
                    result,
                    f64::from(imported.sample_rate.value())
                        / f64::from(project.sample_rate.value()),
                    project,
                    cache_directory,
                )?;
            }
            result
        }
        AudioAssetDefinition::Processed {
            source_asset_id,
            transforms,
            effects,
        } => {
            let mut result = resolve_asset_source(
                project,
                *source_asset_id,
                decoded,
                processors,
                stretcher,
                cache_directory,
                cache,
                visiting,
            )?;
            for transform in transforms {
                result =
                    apply_source_transform(result, transform, project, stretcher, cache_directory)?;
            }
            apply_processor_chain_source(result, effects, processors, project, cache_directory)?
        }
        AudioAssetDefinition::Materialized { .. }
        | AudioAssetDefinition::InstrumentGenerated { .. }
        | AudioAssetDefinition::CompositionGenerated { .. } => {
            let source = decoded.resolve(&id_string).ok_or_else(|| {
                CompileError::Unsupported(format!(
                    "asset {id} requires a caller-supplied decoded logical source"
                ))
            })?;
            DerivedSource {
                key: derived_key("generated", "asset", asset, project)?,
                source,
            }
        }
    };
    result.source = paged_source(result.source)?;
    visiting.pop();
    cache.insert(id.to_string(), result.clone());
    Ok(result)
}

fn apply_source_transform(
    source: DerivedSource,
    transform: &AudioTransform,
    project: &Project,
    stretcher: &dyn TempoStretcher,
    cache_directory: &Path,
) -> Result<DerivedSource, CompileError> {
    let key = derived_key(&source.key, "transform", transform, project)?;
    let rate = project.sample_rate.value();
    let transformed: Arc<dyn FrameSource> = match transform {
        AudioTransform::Trim(range) => {
            let start_frame = seconds_to_frames(range.start.value(), rate)?;
            Arc::new(SlicedFrameSource {
                frame_count: seconds_to_frames(range.duration.value(), rate)?
                    .min(source.source.frame_count().saturating_sub(start_frame)),
                source: source.source,
                start_frame,
            })
        }
        AudioTransform::Reverse => Arc::new(ReverseFrameSource {
            source: source.source,
        }),
        AudioTransform::Repitch { ratio } => {
            return repitch_source_with_key(source, ratio.value(), project, cache_directory, key);
        }
        AudioTransform::Stretch { ratio } => {
            return stretch_source_with_key(
                source,
                ratio.value(),
                project,
                stretcher,
                cache_directory,
                key,
            );
        }
        AudioTransform::FadeIn(fade) | AudioTransform::FadeOut(fade) => Arc::new(FadeFrameSource {
            fade_frames: seconds_to_frames(fade.duration.value(), rate)?,
            source: source.source,
            fade: *fade,
            fade_in: matches!(transform, AudioTransform::FadeIn(_)),
        }),
    };
    Ok(DerivedSource {
        key,
        source: paged_source(transformed)?,
    })
}

fn repitch_source(
    source: DerivedSource,
    speed: f64,
    project: &Project,
    cache_directory: &Path,
) -> Result<DerivedSource, CompileError> {
    let key = derived_key(&source.key, "repitch", &speed.to_bits(), project)?;
    repitch_source_with_key(source, speed, project, cache_directory, key)
}

fn repitch_source_with_key(
    source: DerivedSource,
    speed: f64,
    project: &Project,
    cache_directory: &Path,
    key: String,
) -> Result<DerivedSource, CompileError> {
    if !speed.is_finite() || speed <= 0.0 {
        return Err(CompileError::Tempo(
            "playback speed must be finite and positive".into(),
        ));
    }
    let input_frames =
        usize::try_from(source.source.frame_count()).map_err(|_| CompileError::Overflow)?;
    if input_frames == 0 {
        return Ok(DerivedSource {
            key,
            source: source.source,
        });
    }
    let output_frames = ((input_frames as f64) / speed).ceil() as usize;
    let layout = source.source.channel_layout();
    let sample_rate = project.sample_rate.value();
    let rendered = cached_source(
        cache_directory,
        &key,
        sample_rate,
        layout,
        output_frames as u64,
        |path| {
            write_repitch_wav(
                path,
                sample_rate,
                source.source.as_ref(),
                speed,
                output_frames,
            )
        },
    )?;
    Ok(DerivedSource {
        key,
        source: rendered,
    })
}

fn write_repitch_wav(
    path: &Path,
    sample_rate: u32,
    source: &dyn FrameSource,
    speed: f64,
    expected_frames: usize,
) -> Result<(), CompileError> {
    let channels = source.channel_layout().channels();
    let ratio = 1.0 / speed;
    let chunk = usize::try_from(source.frame_count())
        .unwrap_or(usize::MAX)
        .clamp(64, 2_048);
    let mut resampler = Async::<f32>::new_sinc(
        ratio,
        1.0,
        &SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Cubic,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        },
        chunk,
        channels,
        FixedAsync::Input,
    )
    .map_err(|error| CompileError::Tempo(error.to_string()))?;
    let input_capacity = resampler.input_frames_max();
    let output_capacity = resampler.output_frames_max();
    let mut input = vec![0.0; input_capacity.saturating_mul(channels)];
    let mut output = vec![0.0; output_capacity.saturating_mul(channels)];
    let mut writer = wav_writer(path, sample_rate, source.channel_layout())?;
    let mut source_position = 0_usize;
    let input_total = usize::try_from(source.frame_count()).map_err(|_| CompileError::Overflow)?;
    let mut delay = resampler.output_delay();
    let mut written = 0_usize;

    while input_total.saturating_sub(source_position) > resampler.input_frames_next() {
        let frames = resampler.input_frames_next();
        let read =
            source.read_interleaved(source_position as u64, &mut input[..frames * channels])?;
        if read != frames {
            return Err(crate::AssetError::SourceEndedEarly {
                frame: (source_position + read) as u64,
            }
            .into());
        }
        let input_adapter = InterleavedSlice::new(&input, channels, frames)
            .map_err(|error| CompileError::Tempo(error.to_string()))?;
        let mut output_adapter = InterleavedSlice::new_mut(&mut output, channels, output_capacity)
            .map_err(|error| CompileError::Tempo(error.to_string()))?;
        let (consumed, produced) = resampler
            .process_into_buffer(&input_adapter, &mut output_adapter, None)
            .map_err(|error| CompileError::Tempo(error.to_string()))?;
        write_resampled_chunk(
            &mut writer,
            &output[..produced * channels],
            channels,
            &mut delay,
            &mut written,
            expected_frames,
        )?;
        source_position += consumed;
    }

    let remaining = input_total.saturating_sub(source_position);
    if remaining != 0 {
        input.fill(0.0);
        let read =
            source.read_interleaved(source_position as u64, &mut input[..remaining * channels])?;
        if read != remaining {
            return Err(crate::AssetError::SourceEndedEarly {
                frame: (source_position + read) as u64,
            }
            .into());
        }
        let input_adapter = InterleavedSlice::new(&input, channels, input_capacity)
            .map_err(|error| CompileError::Tempo(error.to_string()))?;
        let mut output_adapter = InterleavedSlice::new_mut(&mut output, channels, output_capacity)
            .map_err(|error| CompileError::Tempo(error.to_string()))?;
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(remaining),
            active_channels_mask: None,
        };
        let (_, produced) = resampler
            .process_into_buffer(&input_adapter, &mut output_adapter, Some(&indexing))
            .map_err(|error| CompileError::Tempo(error.to_string()))?;
        write_resampled_chunk(
            &mut writer,
            &output[..produced * channels],
            channels,
            &mut delay,
            &mut written,
            expected_frames,
        )?;
    }
    input.fill(0.0);
    while written < expected_frames {
        let input_adapter = InterleavedSlice::new(&input, channels, input_capacity)
            .map_err(|error| CompileError::Tempo(error.to_string()))?;
        let mut output_adapter = InterleavedSlice::new_mut(&mut output, channels, output_capacity)
            .map_err(|error| CompileError::Tempo(error.to_string()))?;
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: Some(0),
            active_channels_mask: None,
        };
        let (_, produced) = resampler
            .process_into_buffer(&input_adapter, &mut output_adapter, Some(&indexing))
            .map_err(|error| CompileError::Tempo(error.to_string()))?;
        if produced == 0 {
            return Err(CompileError::Tempo(
                "resampler stopped before exact output length".into(),
            ));
        }
        write_resampled_chunk(
            &mut writer,
            &output[..produced * channels],
            channels,
            &mut delay,
            &mut written,
            expected_frames,
        )?;
    }
    writer.finalize()?;
    Ok(())
}

fn write_resampled_chunk(
    writer: &mut hound::WavWriter<File>,
    samples: &[f32],
    channels: usize,
    delay: &mut usize,
    written: &mut usize,
    expected_frames: usize,
) -> Result<(), CompileError> {
    let frames = samples.len() / channels;
    let skipped = (*delay).min(frames);
    *delay -= skipped;
    let available = frames.saturating_sub(skipped);
    let emit = available.min(expected_frames.saturating_sub(*written));
    for &sample in &samples[skipped * channels..(skipped + emit) * channels] {
        writer.write_sample(sample)?;
    }
    *written += emit;
    Ok(())
}

fn stretch_source_with_key(
    source: DerivedSource,
    ratio: f64,
    project: &Project,
    stretcher: &dyn TempoStretcher,
    cache_directory: &Path,
    key: String,
) -> Result<DerivedSource, CompileError> {
    let input_frames =
        usize::try_from(source.source.frame_count()).map_err(|_| CompileError::Overflow)?;
    let output_frames = ((input_frames as f64) / ratio).round() as usize;
    let layout = source.source.channel_layout();
    let sample_rate = project.sample_rate.value();
    let rendered = cached_source(
        cache_directory,
        &key,
        sample_rate,
        layout,
        output_frames as u64,
        |path| {
            let mut writer = wav_writer(path, sample_rate, layout)?;
            let mut emitted = 0_usize;
            stretcher
                .stretch_source(
                    source.source.as_ref(),
                    layout,
                    sample_rate,
                    output_frames,
                    &mut |samples| {
                        let remaining = output_frames.saturating_sub(emitted);
                        let frames = (samples.len() / layout.channels()).min(remaining);
                        for &sample in &samples[..frames * layout.channels()] {
                            writer
                                .write_sample(sample)
                                .map_err(|error| error.to_string())?;
                        }
                        emitted += frames;
                        Ok(())
                    },
                )
                .map_err(CompileError::Tempo)?;
            if emitted != output_frames {
                return Err(CompileError::Tempo(format!(
                    "stretcher emitted {emitted} frames, expected {output_frames}"
                )));
            }
            writer.finalize()?;
            Ok(())
        },
    )?;
    Ok(DerivedSource {
        key,
        source: rendered,
    })
}

fn read_source(source: &dyn FrameSource) -> Result<AudioBuffer, CompileError> {
    let layout = source.channel_layout();
    let frames = usize::try_from(source.frame_count()).map_err(|_| CompileError::Overflow)?;
    let mut samples = vec![
        0.0;
        frames
            .checked_mul(layout.channels())
            .ok_or(CompileError::Overflow)?
    ];
    let mut offset = 0;
    while offset < frames {
        let count = (frames - offset).min(PROCESS_BLOCK_FRAMES);
        let range = offset * layout.channels()..(offset + count) * layout.channels();
        let read = source.read_interleaved(offset as u64, &mut samples[range])?;
        if read != count {
            return Err(crate::AssetError::SourceEndedEarly {
                frame: (offset + read) as u64,
            }
            .into());
        }
        offset += count;
    }
    Ok(AudioBuffer { layout, samples })
}

#[allow(clippy::too_many_arguments)]
fn materialize_asset(
    project: &Project,
    id: gaw_core::AssetId,
    decoded: &dyn AssetSourceResolver,
    processors: &DspProcessorAdapter,
    stretcher: &dyn TempoStretcher,
    cache: &mut HashMap<String, AudioBuffer>,
    visiting: &mut Vec<gaw_core::AssetId>,
) -> Result<AudioBuffer, CompileError> {
    let key = id.to_string();
    if let Some(audio) = cache.get(&key) {
        return Ok(audio.clone());
    }
    if visiting.contains(&id) {
        return Err(CompileError::Unsupported(format!(
            "asset dependency cycle at {id}"
        )));
    }
    visiting.push(id);
    let asset = project
        .assets
        .iter()
        .find(|asset| asset.id == id)
        .expect("validated asset");
    let mut audio = match &asset.definition {
        AudioAssetDefinition::Imported(imported) => {
            let source = decoded
                .resolve(&key)
                .ok_or_else(|| CompileError::MissingDecodedAsset(key.clone()))?;
            let audio = read_source(source.as_ref())?;
            let expected = layout(imported.layout);
            if audio.layout != expected {
                return Err(CompileError::AssetLayout {
                    asset: key,
                    actual: audio.layout,
                    expected,
                });
            }
            resample_to_project(
                audio,
                imported.sample_rate.value(),
                project.sample_rate.value(),
            )?
        }
        AudioAssetDefinition::Processed {
            source_asset_id,
            transforms,
            effects,
        } => {
            let mut audio = materialize_asset(
                project,
                *source_asset_id,
                decoded,
                processors,
                stretcher,
                cache,
                visiting,
            )?;
            for transform in transforms {
                audio = apply_transform(audio, transform, project, stretcher)?;
            }
            apply_processor_chain(audio, effects, processors)?
        }
        AudioAssetDefinition::Materialized { .. }
        | AudioAssetDefinition::InstrumentGenerated { .. }
        | AudioAssetDefinition::CompositionGenerated { .. } => {
            let source = decoded.resolve(&key).ok_or_else(|| {
                CompileError::Unsupported(format!(
                    "asset {id} requires a caller-supplied decoded logical source"
                ))
            })?;
            read_source(source.as_ref())?
        }
    };
    if audio.samples.iter().any(|sample| !sample.is_finite()) {
        audio.samples.iter_mut().for_each(|sample| {
            if !sample.is_finite() {
                *sample = 0.0;
            }
        });
    }
    visiting.pop();
    cache.insert(key, audio.clone());
    Ok(audio)
}

fn resample_to_project(
    audio: AudioBuffer,
    source_rate: u32,
    project_rate: u32,
) -> Result<AudioBuffer, CompileError> {
    if source_rate == project_rate {
        return Ok(audio);
    }
    repitch(audio, f64::from(source_rate) / f64::from(project_rate))
}

fn planar(audio: &AudioBuffer) -> Vec<Vec<f32>> {
    let channels = audio.layout.channels();
    let mut output = vec![Vec::with_capacity(audio.frames()); channels];
    for frame in audio.samples.chunks_exact(channels) {
        for (channel, sample) in frame.iter().enumerate() {
            output[channel].push(*sample);
        }
    }
    output
}

fn interleave(layout: ChannelLayout, channels: &[Vec<f32>]) -> AudioBuffer {
    let frames = channels.first().map_or(0, Vec::len);
    let mut samples = Vec::with_capacity(frames.saturating_mul(layout.channels()));
    for frame in 0..frames {
        for channel in channels {
            samples.push(channel[frame]);
        }
    }
    AudioBuffer { layout, samples }
}

fn repitch(audio: AudioBuffer, speed: f64) -> Result<AudioBuffer, CompileError> {
    let output = gaw_dsp::repitch_planar(&planar(&audio), speed)
        .map_err(|error| CompileError::Tempo(error.to_string()))?;
    Ok(interleave(audio.layout, &output))
}

fn apply_transform(
    mut audio: AudioBuffer,
    transform: &AudioTransform,
    project: &Project,
    stretcher: &dyn TempoStretcher,
) -> Result<AudioBuffer, CompileError> {
    let rate = project.sample_rate.value();
    match transform {
        AudioTransform::Trim(range) => {
            trim(&audio, range.start.value(), range.duration.value(), rate)
        }
        AudioTransform::Reverse => {
            reverse_frames(&mut audio);
            Ok(audio)
        }
        AudioTransform::Repitch { ratio } => repitch(audio, ratio.value()),
        AudioTransform::Stretch { ratio } => stretch_audio(audio, ratio.value(), rate, stretcher),
        AudioTransform::FadeIn(fade) => {
            apply_fade(&mut audio, *fade, true, rate);
            Ok(audio)
        }
        AudioTransform::FadeOut(fade) => {
            apply_fade(&mut audio, *fade, false, rate);
            Ok(audio)
        }
    }
}

fn render_audio_clip_source(
    project: &Project,
    clip: &gaw_core::AudioClip,
    mut source: DerivedSource,
    stretcher: &dyn TempoStretcher,
    cache_directory: &Path,
) -> Result<DerivedSource, CompileError> {
    let rate = project.sample_rate.value();
    let start_frame = seconds_to_frames(clip.source.start.value(), rate)?;
    let frame_count = seconds_to_frames(clip.source.duration.value(), rate)?
        .min(source.source.frame_count().saturating_sub(start_frame));
    let clip_key = derived_key(&source.key, "audio-clip", clip, project)?;
    source = DerivedSource {
        key: derived_key(
            &clip_key,
            "source-range",
            &(start_frame, frame_count),
            project,
        )?,
        source: paged_source(Arc::new(SlicedFrameSource {
            source: source.source,
            start_frame,
            frame_count,
        }))?,
    };
    if clip.reverse {
        source = DerivedSource {
            key: derived_key(&source.key, "reverse", &true, project)?,
            source: paged_source(Arc::new(ReverseFrameSource {
                source: source.source,
            }))?,
        };
    }
    if clip.tempo_sync != TempoSync::None {
        let asset = project
            .assets
            .iter()
            .find(|asset| asset.id == clip.asset_id)
            .expect("validated asset");
        let ratio = asset
            .tempo
            .expect("validated tempo sync")
            .playback_ratio(project.bpm)?
            .value();
        source = match clip.tempo_sync {
            TempoSync::None => source,
            TempoSync::Repitch => repitch_source(source, ratio, project, cache_directory)?,
            TempoSync::Stretch => {
                let key = derived_key(&source.key, "clip-stretch", &ratio.to_bits(), project)?;
                stretch_source_with_key(source, ratio, project, stretcher, cache_directory, key)?
            }
        };
    }
    for (fade, fade_in) in [(clip.fade_in, true), (clip.fade_out, false)] {
        let Some(fade) = fade else { continue };
        source = DerivedSource {
            key: derived_key(
                &source.key,
                if fade_in { "fade-in" } else { "fade-out" },
                &fade,
                project,
            )?,
            source: paged_source(Arc::new(FadeFrameSource {
                fade_frames: seconds_to_frames(fade.duration.value(), rate)?,
                source: source.source,
                fade,
                fade_in,
            }))?,
        };
    }
    source.key = clip_key;
    Ok(source)
}

fn apply_processor_chain_source(
    source: DerivedSource,
    effects: &[gaw_core::Processor],
    adapter: &DspProcessorAdapter,
    project: &Project,
    cache_directory: &Path,
) -> Result<DerivedSource, CompileError> {
    // DSP processors expose reset/seek but no state serialization contract, so
    // checkpointing cannot preserve arbitrary built-in state exactly. A new
    // immutable revision therefore makes one O(source frames) forward pass into
    // the cache. Its working memory is block-bounded; subsequent page reads use
    // the bounded WAV page cache without replaying that processor chain.
    if effects.iter().all(|effect| !effect.enabled) {
        return Ok(source);
    }
    let specs = processor_specs(adapter, effects, source.source.channel_layout())?;
    let latency = specs
        .iter()
        .filter(|spec| spec.enabled)
        .map(|spec| spec.latency_frames)
        .sum::<u64>();
    let tail = specs
        .iter()
        .filter(|spec| spec.enabled)
        .map(|spec| spec.tail_frames)
        .sum::<u64>();
    let working_frames = source
        .source
        .frame_count()
        .saturating_add(latency)
        .saturating_add(tail);
    let output_frames = working_frames.saturating_sub(latency);
    let key = derived_key(&source.key, "processor-chain", &effects, project)?;
    let layout = source.source.channel_layout();
    let sample_rate = project.sample_rate.value();
    let padded: Arc<dyn FrameSource> = Arc::new(ZeroPaddedFrameSource {
        source: source.source,
        frame_count: working_frames,
    });
    let rendered = cached_source(
        cache_directory,
        &key,
        sample_rate,
        layout,
        output_frames,
        |path| adapter.write_processor_chain_wav(path, padded.as_ref(), effects, latency),
    )?;
    Ok(DerivedSource {
        key,
        source: rendered,
    })
}

fn trim(
    audio: &AudioBuffer,
    start_seconds: f64,
    duration_seconds: f64,
    rate: u32,
) -> Result<AudioBuffer, CompileError> {
    let start = usize::try_from(seconds_to_frames(start_seconds, rate)?)
        .map_err(|_| CompileError::Overflow)?;
    let count = usize::try_from(seconds_to_frames(duration_seconds, rate)?)
        .map_err(|_| CompileError::Overflow)?;
    let end = start.saturating_add(count).min(audio.frames());
    let channels = audio.layout.channels();
    Ok(AudioBuffer {
        layout: audio.layout,
        samples: audio.samples[start.saturating_mul(channels)..end.saturating_mul(channels)]
            .to_vec(),
    })
}

fn reverse_frames(audio: &mut AudioBuffer) {
    let channels = audio.layout.channels();
    let frames = audio.frames();
    for left in 0..frames / 2 {
        let right = frames - 1 - left;
        for channel in 0..channels {
            audio
                .samples
                .swap(left * channels + channel, right * channels + channel);
        }
    }
}

fn stretch_audio(
    mut audio: AudioBuffer,
    ratio: f64,
    rate: u32,
    stretcher: &dyn TempoStretcher,
) -> Result<AudioBuffer, CompileError> {
    let input_frames = audio.frames();
    let output_frames = ((input_frames as f64) / ratio).round() as usize;
    let mut remaining = ratio;
    while remaining < 0.5 {
        let frames = audio.frames().saturating_mul(2);
        audio = stretch_stage(audio, rate, frames, stretcher)?;
        remaining /= 0.5;
    }
    while remaining > 2.0 {
        let frames = audio.frames().div_ceil(2);
        audio = stretch_stage(audio, rate, frames, stretcher)?;
        remaining /= 2.0;
    }
    if audio.frames() == output_frames {
        return Ok(audio);
    }
    stretch_stage(audio, rate, output_frames, stretcher)
}

fn stretch_stage(
    audio: AudioBuffer,
    rate: u32,
    output_frames: usize,
    stretcher: &dyn TempoStretcher,
) -> Result<AudioBuffer, CompileError> {
    let samples = stretcher
        .stretch(&audio.samples, audio.layout, rate, output_frames)
        .map_err(CompileError::Tempo)?;
    Ok(AudioBuffer {
        layout: audio.layout,
        samples,
    })
}

fn apply_fade(audio: &mut AudioBuffer, fade: Fade, fade_in: bool, rate: u32) {
    let frames =
        usize::try_from(seconds_to_frames(fade.duration.value(), rate).unwrap_or(u64::MAX))
            .unwrap_or(usize::MAX)
            .min(audio.frames());
    let channels = audio.layout.channels();
    for index in 0..frames {
        let t = if frames <= 1 {
            0.0
        } else {
            index as f32 / (frames - 1) as f32
        };
        let rising = match fade.curve {
            FadeCurve::Linear => t,
            FadeCurve::EqualPower => (t * std::f32::consts::FRAC_PI_2).sin(),
            FadeCurve::Exponential => t * t * t,
        };
        let frame = if fade_in {
            index
        } else {
            audio.frames() - 1 - index
        };
        for channel in 0..channels {
            audio.samples[frame * channels + channel] *= rising;
        }
    }
}

fn apply_processor_chain(
    mut audio: AudioBuffer,
    effects: &[gaw_core::Processor],
    adapter: &DspProcessorAdapter,
) -> Result<AudioBuffer, CompileError> {
    let specs = processor_specs(adapter, effects, audio.layout)?;
    let latency = specs
        .iter()
        .filter(|spec| spec.enabled)
        .map(|spec| spec.latency_frames)
        .sum::<u64>();
    let tail = specs
        .iter()
        .filter(|spec| spec.enabled)
        .map(|spec| spec.tail_frames)
        .sum::<u64>();
    let channels = audio.layout.channels();
    let extra = usize::try_from(latency.saturating_add(tail))
        .map_err(|_| CompileError::Overflow)?
        .checked_mul(channels)
        .ok_or(CompileError::Overflow)?;
    audio
        .samples
        .resize(audio.samples.len().saturating_add(extra), 0.0);
    let mut scratch = vec![0.0; audio.samples.len()];
    for spec in specs.iter().filter(|spec| spec.enabled) {
        adapter
            .process(
                spec,
                adapter.sample_rate,
                audio.layout,
                &audio.samples,
                &mut scratch,
            )
            .map_err(|message| CompileError::Processor {
                processor: spec.id.clone(),
                message,
            })?;
        std::mem::swap(&mut audio.samples, &mut scratch);
    }
    let crop = usize::try_from(latency)
        .unwrap_or(usize::MAX)
        .saturating_mul(channels)
        .min(audio.samples.len());
    audio.samples.drain(..crop);
    Ok(audio)
}

fn render_event_clip(
    project: &Project,
    track: &gaw_core::Track,
    clip: &gaw_core::EventClip,
    assets: &HashMap<String, AudioBuffer>,
    output_layout: ChannelLayout,
    tail_cap: u64,
) -> Result<AudioBuffer, CompileError> {
    let instrument = track.instrument.as_ref().expect("validated event track");
    let InstrumentKind::Sampler(config) = &instrument.kind;
    if config.voice_stealing != VoiceStealing::Oldest {
        return Err(CompileError::Unsupported(format!(
            "sampler `{}` ({}) requests {:?} voice stealing, but gaw-dsp exposes only oldest voice stealing",
            instrument.name, instrument.id, config.voice_stealing
        )));
    }
    if config.polyphony > 256 {
        return Err(CompileError::Unsupported(format!(
            "sampler `{}` ({}) requests polyphony {}, above gaw-dsp's exact limit of 256",
            instrument.name, instrument.id, config.polyphony
        )));
    }
    let rate = project.sample_rate.value();
    let mut sample_assets = Vec::new();
    let mut zones = Vec::new();
    for zone in &config.zones {
        let asset = assets
            .get(&zone.asset_id.to_string())
            .expect("validated sampler asset");
        if output_layout == ChannelLayout::Mono && asset.layout == ChannelLayout::Stereo {
            return Err(CompileError::Unsupported(format!(
                "sampler zone `{}` would implicitly downmix stereo to mono",
                zone.name
            )));
        }
        if !sample_assets
            .iter()
            .any(|asset: &gaw_dsp::SampleAsset| asset.id == zone.asset_id.to_string())
        {
            sample_assets.push(gaw_dsp::SampleAsset {
                id: zone.asset_id.to_string(),
                sample_rate: f64::from(rate),
                channels: planar(asset),
            });
        }
        let start = seconds_to_frames(zone.source.start.value(), rate)? as usize;
        let end =
            start.saturating_add(seconds_to_frames(zone.source.duration.value(), rate)? as usize);
        zones.push(gaw_dsp::SamplerZone {
            id: zone.id.to_string(),
            asset_id: zone.asset_id.to_string(),
            source_start_frame: start,
            source_end_frame: Some(end),
            root_note: zone.root_note.value(),
            low_note: zone.note_range.low.value(),
            high_note: zone.note_range.high.value(),
            low_velocity: zone.velocity_range.low.value(),
            high_velocity: zone.velocity_range.high.value(),
            playback_mode: match zone.playback {
                SamplerPlayback::OneShot => gaw_dsp::PlaybackMode::OneShot,
                SamplerPlayback::NoteGated => gaw_dsp::PlaybackMode::NoteGated,
            },
            gain_db: (zone.gain.value() + config.output_gain.value()) as f32,
            velocity_sensitivity: zone.velocity_sensitivity.value() as f32,
            attack_ms: zone.attack.value() as f32,
            release_ms: zone.release.value() as f32,
            reverse: zone.reverse,
            choke_group: zone.choke_group,
        });
    }
    let tempo = Tempo::new(project.bpm.value(), rate)?;
    let window_start = beat_duration_frames(tempo, clip.source_start)?;
    let body_frames = beat_duration_frames(tempo, clip.duration)?;
    let window_end = window_start.saturating_add(body_frames);
    let data = project
        .event_data
        .iter()
        .find(|data| data.id == clip.event_data_id)
        .expect("validated event data");
    let mut scheduled = Vec::<(usize, bool, u8, f32)>::new();
    let mut note_windows = HashMap::<u8, Vec<(usize, usize, bool)>>::new();
    let mut maximum_end = window_end;
    for event in &data.events {
        match event {
            Event::Note(note) => {
                if note.release_velocity.value() != 64 {
                    return Err(CompileError::Unsupported(format!(
                        "sampler `{}` ({}) note {} at beat {} has release velocity {}; gaw-dsp NoteOff has no release-velocity field",
                        instrument.name,
                        instrument.id,
                        note.note.value(),
                        note.start.value(),
                        note.release_velocity.value()
                    )));
                }
                let start = beat_duration_frames(tempo, note.start)?;
                let end = start.saturating_add(beat_duration_frames(tempo, note.duration)?);
                if start < window_end {
                    let velocity = note.velocity.value();
                    let note_gated = zones.iter().any(|zone| {
                        zone.playback_mode == gaw_dsp::PlaybackMode::NoteGated
                            && (zone.low_note..=zone.high_note).contains(&note.note.value())
                            && (zone.low_velocity..=zone.high_velocity).contains(&velocity)
                    });
                    if note_windows
                        .get(&note.note.value())
                        .into_iter()
                        .flatten()
                        .any(|&(other_start, other_end, other_gated)| {
                            other_start < end && other_end > start && (other_gated || note_gated)
                        })
                    {
                        return Err(CompileError::Unsupported(format!(
                            "sampler `{}` ({}) has overlapping note {} windows at beat {}; gaw-dsp NoteOff releases every active gated voice with that note",
                            instrument.name,
                            instrument.id,
                            note.note.value(),
                            note.start.value()
                        )));
                    }
                    note_windows
                        .entry(note.note.value())
                        .or_default()
                        .push((start, end, note_gated));
                    scheduled.push((
                        start,
                        true,
                        note.note.value(),
                        f32::from(note.velocity.value()) / 127.0,
                    ));
                    scheduled.push((end, false, note.note.value(), 0.0));
                    for zone in &zones {
                        if (zone.low_note..=zone.high_note).contains(&note.note.value()) {
                            let zone_frames = zone
                                .source_end_frame
                                .unwrap_or(zone.source_start_frame)
                                .saturating_sub(zone.source_start_frame);
                            let pitch = 2.0_f64.powf(
                                (f64::from(note.note.value()) - f64::from(zone.root_note)) / 12.0,
                            );
                            let one_shot =
                                start.saturating_add((zone_frames as f64 / pitch).ceil() as usize);
                            let release = end.saturating_add(
                                (f64::from(zone.release_ms) * f64::from(rate) / 1000.0).ceil()
                                    as usize,
                            );
                            maximum_end = maximum_end.max(
                                if zone.playback_mode == gaw_dsp::PlaybackMode::OneShot {
                                    one_shot
                                } else {
                                    release
                                },
                            );
                        }
                    }
                }
            }
            Event::Control(value) if beat_duration_frames(tempo, value.time)? < window_end => {
                return Err(CompileError::Unsupported(format!(
                    "sampler `{}` ({}) control `{}`={} at beat {} has no defined canonical-to-DSP parameter mapping",
                    instrument.name,
                    instrument.id,
                    value.controller,
                    value.value.value(),
                    value.time.value()
                )));
            }
            Event::PitchBend(value) if beat_duration_frames(tempo, value.time)? < window_end => {
                return Err(CompileError::Unsupported(format!(
                    "sampler `{}` ({}) pitch bend {} at beat {} cannot be mapped exactly: the project model has no bend range and gaw-dsp cannot retune active voices",
                    instrument.name,
                    instrument.id,
                    value.value.value(),
                    value.time.value()
                )));
            }
            Event::Control(_) | Event::PitchBend(_) => {}
        }
    }
    scheduled.sort_by_key(|event| (event.0, event.1));
    let cap_end = window_end.saturating_add(usize::try_from(tail_cap).unwrap_or(usize::MAX));
    let render_end = maximum_end.min(cap_end);
    let total_frames = render_end.saturating_sub(window_start);
    let mut sampler = gaw_dsp::Sampler::new(
        gaw_dsp::SamplerConfig {
            polyphony: usize::from(config.polyphony),
            zones,
        },
        sample_assets,
    )
    .map_err(|error| CompileError::Unsupported(error.to_string()))?;
    sampler
        .prepare(PrepareSpec {
            sample_rate: f64::from(rate),
            max_block_size: PROCESS_BLOCK_FRAMES,
            input_layout: dsp_layout(output_layout),
            tempo_bpm: project.bpm.value(),
        })
        .map_err(|error| CompileError::Unsupported(error.to_string()))?;
    let channels = output_layout.channels();
    let mut output_samples = vec![
        0.0;
        total_frames
            .checked_mul(channels)
            .ok_or(CompileError::Overflow)?
    ];
    for block_start in (0..render_end).step_by(PROCESS_BLOCK_FRAMES) {
        let frames = (render_end - block_start).min(PROCESS_BLOCK_FRAMES);
        let mut events = Vec::new();
        for &(frame, on, note, velocity) in scheduled
            .iter()
            .filter(|event| (block_start..block_start + frames).contains(&event.0))
        {
            let sample_offset = frame - block_start;
            events.push(if on {
                gaw_dsp::NoteEvent::NoteOn {
                    sample_offset,
                    note,
                    velocity,
                }
            } else {
                gaw_dsp::NoteEvent::NoteOff {
                    sample_offset,
                    note,
                }
            });
        }
        let mut block = vec![vec![0.0; frames]; channels];
        let mut outputs: Vec<_> = block.iter_mut().map(Vec::as_mut_slice).collect();
        sampler
            .process(
                &mut outputs,
                &events,
                ProcessContext {
                    absolute_frame: block_start as u64,
                    tempo_bpm: project.bpm.value(),
                },
            )
            .map_err(|error| CompileError::Unsupported(error.to_string()))?;
        let copy_start = block_start.max(window_start);
        let copy_end = (block_start + frames).min(render_end);
        for frame in copy_start..copy_end {
            let source_frame = frame - block_start;
            let output_frame = frame - window_start;
            for channel in 0..channels {
                output_samples[output_frame * channels + channel] = block[channel][source_frame];
            }
        }
    }
    Ok(AudioBuffer {
        layout: output_layout,
        samples: output_samples,
    })
}

fn processor_specs(
    adapter: &DspProcessorAdapter,
    effects: &[gaw_core::Processor],
    layout: ChannelLayout,
) -> Result<Vec<ProcessorSpec>, CompileError> {
    effects
        .iter()
        .map(|processor| adapter.spec(processor, layout))
        .collect()
}

#[derive(Clone, Debug)]
enum CanonicalAnalyzerConfig {
    Level(gaw_core::LevelMeterParameters),
    Loudness(gaw_core::LoudnessMeterParameters),
    Spectrum(gaw_core::SpectrumParameters),
    Oscilloscope(gaw_core::OscilloscopeParameters),
    Stereo(gaw_core::StereoMeterParameters),
    Tuner(gaw_core::TunerParameters),
}

#[derive(Clone, Copy, Debug, Default)]
struct LoudnessEnergyBlock {
    energy: f64,
    frames: usize,
}

#[derive(Debug, Default)]
enum CanonicalAnalyzerState {
    #[default]
    Empty,
    Level {
        samples: Vec<Vec<f32>>,
        write: usize,
        filled: usize,
        held_peak: [f32; 2],
        held_until: [u64; 2],
        latest_frame: u64,
    },
    Loudness {
        blocks: VecDeque<LoudnessEnergyBlock>,
        frames: usize,
        maximum_frames: usize,
    },
    Spectrum {
        capture: Vec<f32>,
        write: usize,
        previous_dbfs: Vec<f32>,
    },
    Oscilloscope {
        capture: Vec<f32>,
        write: usize,
        filled: usize,
        crossings: u64,
        analyzed_frames: u64,
    },
    Stereo {
        left: Vec<f32>,
        right: Vec<f32>,
        write: usize,
        filled: usize,
    },
    Tuner {
        capture: Vec<f32>,
        write: usize,
        filled: usize,
    },
}

struct CanonicalAnalyzerProcessor {
    inner: Box<dyn DspProcessor>,
    config: CanonicalAnalyzerConfig,
    state: CanonicalAnalyzerState,
    sample_rate: f64,
}

impl CanonicalAnalyzerProcessor {
    fn new(inner: Box<dyn DspProcessor>, config: CanonicalAnalyzerConfig) -> Self {
        Self {
            inner,
            config,
            state: CanonicalAnalyzerState::Empty,
            sample_rate: 48_000.0,
        }
    }

    fn duration_frames(sample_rate: f64, milliseconds: f32) -> usize {
        (sample_rate * f64::from(milliseconds) / 1_000.0)
            .round()
            .max(1.0) as usize
    }

    fn dbfs(amplitude: f32) -> f32 {
        if amplitude <= 1.0e-6 {
            -120.0
        } else {
            20.0 * amplitude.log10()
        }
    }

    fn initialize_state(&mut self, spec: PrepareSpec) {
        self.sample_rate = spec.sample_rate;
        self.state = match &self.config {
            CanonicalAnalyzerConfig::Level(parameters) => {
                let frames = Self::duration_frames(spec.sample_rate, parameters.window_ms);
                CanonicalAnalyzerState::Level {
                    samples: vec![vec![0.0; frames]; spec.input_layout.channels()],
                    write: 0,
                    filled: 0,
                    held_peak: [0.0; 2],
                    held_until: [0; 2],
                    latest_frame: 0,
                }
            }
            CanonicalAnalyzerConfig::Loudness(parameters) => {
                let maximum_frames = (spec.sample_rate * f64::from(parameters.integration_seconds))
                    .round()
                    .max(1.0) as usize;
                let capacity = maximum_frames
                    .div_ceil(spec.max_block_size)
                    .saturating_add(1);
                CanonicalAnalyzerState::Loudness {
                    blocks: VecDeque::with_capacity(capacity),
                    frames: 0,
                    maximum_frames,
                }
            }
            CanonicalAnalyzerConfig::Spectrum(parameters) => CanonicalAnalyzerState::Spectrum {
                capture: vec![0.0; fft_size(parameters.fft_size)],
                write: 0,
                previous_dbfs: Vec::new(),
            },
            CanonicalAnalyzerConfig::Oscilloscope(parameters) => {
                CanonicalAnalyzerState::Oscilloscope {
                    capture: vec![
                        0.0;
                        Self::duration_frames(spec.sample_rate, parameters.window_ms)
                    ],
                    write: 0,
                    filled: 0,
                    crossings: 0,
                    analyzed_frames: 0,
                }
            }
            CanonicalAnalyzerConfig::Stereo(parameters) => {
                let frames = Self::duration_frames(spec.sample_rate, parameters.window_ms);
                CanonicalAnalyzerState::Stereo {
                    left: vec![0.0; frames],
                    right: vec![0.0; frames],
                    write: 0,
                    filled: 0,
                }
            }
            CanonicalAnalyzerConfig::Tuner(parameters) => {
                let frames = (spec.sample_rate / f64::from(parameters.minimum_hz) * 2.0)
                    .ceil()
                    .max(spec.max_block_size as f64) as usize;
                CanonicalAnalyzerState::Tuner {
                    capture: vec![0.0; frames],
                    write: 0,
                    filled: 0,
                }
            }
        };
    }

    fn analyze_configuration(&mut self, input: &[&[f32]], context: ProcessContext) {
        match (&self.config, &mut self.state) {
            (
                CanonicalAnalyzerConfig::Level(parameters),
                CanonicalAnalyzerState::Level {
                    samples,
                    write,
                    filled,
                    held_peak,
                    held_until,
                    latest_frame,
                },
            ) => {
                let hold_frames = Self::duration_frames(self.sample_rate, parameters.peak_hold_ms)
                    .saturating_sub(1) as u64;
                let frames = input.first().map_or(0, |channel| channel.len());
                for frame in 0..frames {
                    let absolute_frame = context.absolute_frame.saturating_add(frame as u64);
                    for (channel_index, channel) in input.iter().take(2).enumerate() {
                        let amplitude = channel[frame].abs();
                        if absolute_frame > held_until[channel_index]
                            || amplitude >= held_peak[channel_index]
                        {
                            held_peak[channel_index] = amplitude;
                            held_until[channel_index] = absolute_frame.saturating_add(hold_frames);
                        }
                        samples[channel_index][*write] = channel[frame];
                    }
                    *write = (*write + 1) % samples[0].len();
                    *filled = (*filled + 1).min(samples[0].len());
                }
                *latest_frame = context.absolute_frame.saturating_add(frames as u64);
            }
            (
                CanonicalAnalyzerConfig::Loudness(_),
                CanonicalAnalyzerState::Loudness {
                    blocks,
                    frames,
                    maximum_frames,
                },
            ) => {
                let block_frames = input.first().map_or(0, |channel| channel.len());
                if block_frames == 0 {
                    return;
                }
                let channels = input.len().clamp(1, 2);
                let mut energy = 0.0_f64;
                for frame in 0..block_frames {
                    for channel in input.iter().take(2) {
                        energy += f64::from(channel[frame]) * f64::from(channel[frame]);
                    }
                }
                energy /= (block_frames * channels) as f64;
                while *frames > 0 && frames.saturating_add(block_frames) > *maximum_frames {
                    if let Some(discarded) = blocks.pop_front() {
                        *frames = frames.saturating_sub(discarded.frames);
                    }
                }
                blocks.push_back(LoudnessEnergyBlock {
                    energy,
                    frames: block_frames,
                });
                *frames = frames.saturating_add(block_frames);
            }
            (
                CanonicalAnalyzerConfig::Stereo(_),
                CanonicalAnalyzerState::Stereo {
                    left,
                    right,
                    write,
                    filled,
                },
            ) => {
                let frames = input.first().map_or(0, |channel| channel.len());
                for frame in 0..frames {
                    left[*write] = input[0][frame];
                    right[*write] = input.get(1).map_or(0.0, |channel| channel[frame]);
                    *write = (*write + 1) % left.len();
                    *filled = (*filled + 1).min(left.len());
                }
            }
            (
                CanonicalAnalyzerConfig::Spectrum(_),
                CanonicalAnalyzerState::Spectrum { capture, write, .. },
            ) => {
                let Some(first) = input.first() else { return };
                for frame in 0..first.len() {
                    capture[*write] = if input.len() == 1 {
                        first[frame]
                    } else {
                        (first[frame] + input[1][frame]) * 0.5
                    };
                    *write = (*write + 1) % capture.len();
                }
            }
            (
                CanonicalAnalyzerConfig::Oscilloscope(_),
                CanonicalAnalyzerState::Oscilloscope {
                    capture,
                    write,
                    filled,
                    crossings,
                    analyzed_frames,
                },
            ) => {
                let Some(first) = input.first() else { return };
                let mut previous = if *filled == 0 {
                    0.0
                } else {
                    capture[(*write + capture.len() - 1) % capture.len()]
                };
                for frame in 0..first.len() {
                    let sample = if input.len() == 1 {
                        first[frame]
                    } else {
                        (first[frame] + input[1][frame]) * 0.5
                    };
                    if (previous < 0.0 && sample >= 0.0) || (previous > 0.0 && sample <= 0.0) {
                        *crossings = crossings.saturating_add(1);
                    }
                    capture[*write] = sample;
                    *write = (*write + 1) % capture.len();
                    *filled = (*filled + 1).min(capture.len());
                    previous = sample;
                }
                *analyzed_frames = analyzed_frames.saturating_add(first.len() as u64);
            }
            (
                CanonicalAnalyzerConfig::Tuner(_),
                CanonicalAnalyzerState::Tuner {
                    capture,
                    write,
                    filled,
                },
            ) => {
                let Some(first) = input.first() else { return };
                for frame in 0..first.len() {
                    capture[*write] = if input.len() == 1 {
                        first[frame]
                    } else {
                        (first[frame] + input[1][frame]) * 0.5
                    };
                    *write = (*write + 1) % capture.len();
                    *filled = (*filled + 1).min(capture.len());
                }
            }
            _ => {}
        }
    }

    fn reset_configuration(&mut self) {
        match &mut self.state {
            CanonicalAnalyzerState::Empty => {}
            CanonicalAnalyzerState::Level {
                samples,
                write,
                filled,
                held_peak,
                held_until,
                latest_frame,
            } => {
                for channel in samples {
                    channel.fill(0.0);
                }
                *write = 0;
                *filled = 0;
                *held_peak = [0.0; 2];
                *held_until = [0; 2];
                *latest_frame = 0;
            }
            CanonicalAnalyzerState::Loudness { blocks, frames, .. } => {
                blocks.clear();
                *frames = 0;
            }
            CanonicalAnalyzerState::Spectrum {
                capture,
                write,
                previous_dbfs,
            } => {
                capture.fill(0.0);
                *write = 0;
                previous_dbfs.clear();
            }
            CanonicalAnalyzerState::Oscilloscope {
                capture,
                write,
                filled,
                crossings,
                analyzed_frames,
            } => {
                capture.fill(0.0);
                *write = 0;
                *filled = 0;
                *crossings = 0;
                *analyzed_frames = 0;
            }
            CanonicalAnalyzerState::Stereo {
                left,
                right,
                write,
                filled,
            } => {
                left.fill(0.0);
                right.fill(0.0);
                *write = 0;
                *filled = 0;
            }
            CanonicalAnalyzerState::Tuner {
                capture,
                write,
                filled,
            } => {
                capture.fill(0.0);
                *write = 0;
                *filled = 0;
            }
        }
    }

    fn configured_measurement(
        &mut self,
        measurement: gaw_core::AnalyzerMeasurement,
    ) -> gaw_core::AnalyzerMeasurement {
        match (&self.config, &mut self.state, measurement) {
            (
                CanonicalAnalyzerConfig::Level(parameters),
                CanonicalAnalyzerState::Level {
                    samples,
                    filled,
                    held_peak,
                    held_until,
                    latest_frame,
                    ..
                },
                gaw_core::AnalyzerMeasurement::LevelMeter(mut measurement),
            ) => {
                for (channel_index, channel) in samples.iter().enumerate() {
                    let active = &channel[..*filled];
                    let peak = active
                        .iter()
                        .fold(0.0_f32, |peak, sample| peak.max(sample.abs()));
                    let rms = if active.is_empty() {
                        0.0
                    } else {
                        (active.iter().map(|sample| sample * sample).sum::<f32>()
                            / active.len() as f32)
                            .sqrt()
                    };
                    measurement.sample_peak_dbfs[channel_index] = Self::dbfs(peak);
                    measurement.rms_dbfs[channel_index] = Self::dbfs(rms);
                    if parameters.true_peak {
                        measurement.true_peak_dbfs[channel_index] =
                            measurement.true_peak_dbfs[channel_index].max(Self::dbfs(peak));
                    } else {
                        measurement.true_peak_dbfs[channel_index] = Self::dbfs(peak);
                    }
                    if *latest_frame > held_until[channel_index] {
                        held_peak[channel_index] = peak;
                    }
                    measurement.peak_hold_dbfs[channel_index] =
                        Self::dbfs(held_peak[channel_index].max(peak));
                    measurement.clipping[channel_index] = peak >= 1.0;
                }
                gaw_core::AnalyzerMeasurement::LevelMeter(measurement)
            }
            (
                CanonicalAnalyzerConfig::Loudness(parameters),
                CanonicalAnalyzerState::Loudness { blocks, .. },
                gaw_core::AnalyzerMeasurement::LoudnessMeter(mut measurement),
            ) => {
                let mut weighted_energy = 0.0;
                let mut gated_frames = 0_usize;
                for block in blocks {
                    let lufs = if block.energy <= 1.0e-12 {
                        -120.0
                    } else {
                        -0.691 + 10.0 * block.energy.log10()
                    };
                    if lufs >= f64::from(parameters.absolute_gate_lufs) {
                        weighted_energy += block.energy * block.frames as f64;
                        gated_frames = gated_frames.saturating_add(block.frames);
                    }
                }
                measurement.integrated_lufs = if gated_frames == 0 {
                    -120.0
                } else {
                    (-0.691 + 10.0 * (weighted_energy / gated_frames as f64).log10()) as f32
                };
                gaw_core::AnalyzerMeasurement::LoudnessMeter(measurement)
            }
            (
                CanonicalAnalyzerConfig::Spectrum(parameters),
                CanonicalAnalyzerState::Spectrum {
                    capture,
                    write,
                    previous_dbfs,
                },
                gaw_core::AnalyzerMeasurement::Spectrum(mut measurement),
            ) => {
                let size = capture.len();
                let bin_count = (size / 2 + 1).min(512);
                measurement.bins.clear();
                for output_bin in 0..bin_count {
                    let dft_bin = if bin_count == 1 {
                        0
                    } else {
                        output_bin * (size / 2) / (bin_count - 1)
                    };
                    let frequency_hz = dft_bin as f32 * self.sample_rate as f32 / size as f32;
                    if frequency_hz < parameters.minimum_hz || frequency_hz > parameters.maximum_hz
                    {
                        continue;
                    }
                    let mut real = 0.0_f32;
                    let mut imaginary = 0.0_f32;
                    for index in 0..size {
                        let phase = std::f32::consts::TAU * index as f32 / size as f32;
                        let window = match parameters.window {
                            gaw_core::WindowFunction::Hann => 0.5 - 0.5 * phase.cos(),
                            gaw_core::WindowFunction::BlackmanHarris => {
                                0.358_75 - 0.488_29 * phase.cos() + 0.141_28 * (2.0 * phase).cos()
                                    - 0.011_68 * (3.0 * phase).cos()
                            }
                            gaw_core::WindowFunction::FlatTop => {
                                0.215_578_95 - 0.416_631_58 * phase.cos()
                                    + 0.277_263_16 * (2.0 * phase).cos()
                                    - 0.083_578_944 * (3.0 * phase).cos()
                                    + 0.006_947_368 * (4.0 * phase).cos()
                            }
                        };
                        let angle =
                            std::f32::consts::TAU * dft_bin as f32 * index as f32 / size as f32;
                        let sample = capture[(*write + index) % size] * window;
                        real += sample * angle.cos();
                        imaginary -= sample * angle.sin();
                    }
                    let magnitude =
                        (real.mul_add(real, imaginary * imaginary)).sqrt() / size as f32;
                    measurement.bins.push(gaw_core::SpectrumBin {
                        frequency_hz,
                        magnitude_dbfs: Self::dbfs(magnitude),
                    });
                }
                if previous_dbfs.len() != measurement.bins.len() {
                    previous_dbfs.resize(measurement.bins.len(), -120.0);
                }
                for (bin, previous) in measurement.bins.iter_mut().zip(previous_dbfs.iter_mut()) {
                    bin.magnitude_dbfs = parameters
                        .smoothing
                        .mul_add(*previous, (1.0 - parameters.smoothing) * bin.magnitude_dbfs);
                    *previous = bin.magnitude_dbfs;
                }
                measurement.peaks = measurement
                    .bins
                    .iter()
                    .max_by(|left, right| left.magnitude_dbfs.total_cmp(&right.magnitude_dbfs))
                    .map(|bin| gaw_core::SpectralPeak {
                        frequency_hz: bin.frequency_hz,
                        magnitude_dbfs: bin.magnitude_dbfs,
                    })
                    .into_iter()
                    .collect();
                let (weighted, magnitude) =
                    measurement
                        .bins
                        .iter()
                        .fold((0.0_f32, 0.0_f32), |(weighted, total), bin| {
                            let amplitude = 10.0_f32.powf(bin.magnitude_dbfs / 20.0);
                            (weighted + amplitude * bin.frequency_hz, total + amplitude)
                        });
                measurement.spectral_centroid_hz = if magnitude <= f32::EPSILON {
                    0.0
                } else {
                    weighted / magnitude
                };
                gaw_core::AnalyzerMeasurement::Spectrum(measurement)
            }
            (
                CanonicalAnalyzerConfig::Oscilloscope(parameters),
                CanonicalAnalyzerState::Oscilloscope {
                    capture,
                    write,
                    filled,
                    crossings,
                    analyzed_frames,
                },
                gaw_core::AnalyzerMeasurement::Oscilloscope(mut measurement),
            ) => {
                let samples: Vec<_> = if *filled == capture.len() {
                    capture[*write..]
                        .iter()
                        .chain(&capture[..*write])
                        .copied()
                        .collect()
                } else {
                    capture[..*filled].to_vec()
                };
                measurement.channel_samples = vec![samples];
                measurement.zero_crossing_rate_hz = vec![if *analyzed_frames == 0 {
                    0.0
                } else {
                    (*crossings as f64 * self.sample_rate / *analyzed_frames as f64) as f32
                }];
                for samples in &mut measurement.channel_samples {
                    let crossing = match parameters.trigger {
                        gaw_core::OscilloscopeTrigger::Free => None,
                        gaw_core::OscilloscopeTrigger::RisingZero => samples
                            .windows(2)
                            .position(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
                            .map(|index| index + 1),
                        gaw_core::OscilloscopeTrigger::FallingZero => samples
                            .windows(2)
                            .position(|pair| pair[0] > 0.0 && pair[1] <= 0.0)
                            .map(|index| index + 1),
                    };
                    if let Some(crossing) = crossing {
                        samples.rotate_left(crossing);
                    }
                }
                gaw_core::AnalyzerMeasurement::Oscilloscope(measurement)
            }
            (
                CanonicalAnalyzerConfig::Stereo(_),
                CanonicalAnalyzerState::Stereo {
                    left,
                    right,
                    filled,
                    ..
                },
                gaw_core::AnalyzerMeasurement::StereoMeter(mut measurement),
            ) => {
                let mut mid_energy = 0.0_f64;
                let mut side_energy = 0.0_f64;
                let mut left_energy = 0.0_f64;
                let mut right_energy = 0.0_f64;
                let mut product = 0.0_f64;
                for (&left, &right) in left[..*filled].iter().zip(&right[..*filled]) {
                    let mid = f64::from(left + right) * 0.5;
                    let side = f64::from(left - right) * 0.5;
                    mid_energy += mid * mid;
                    side_energy += side * side;
                    left_energy += f64::from(left) * f64::from(left);
                    right_energy += f64::from(right) * f64::from(right);
                    product += f64::from(left) * f64::from(right);
                }
                let frames = (*filled).max(1) as f64;
                measurement.mid_level_dbfs = Self::dbfs((mid_energy / frames).sqrt() as f32);
                measurement.side_level_dbfs = Self::dbfs((side_energy / frames).sqrt() as f32);
                measurement.correlation =
                    (product / (left_energy * right_energy).sqrt().max(f64::EPSILON)) as f32;
                measurement.stereo_width =
                    (side_energy / mid_energy.max(f64::EPSILON)).sqrt() as f32;
                gaw_core::AnalyzerMeasurement::StereoMeter(measurement)
            }
            (
                CanonicalAnalyzerConfig::Tuner(parameters),
                CanonicalAnalyzerState::Tuner {
                    capture,
                    write,
                    filled,
                },
                gaw_core::AnalyzerMeasurement::Tuner(mut measurement),
            ) => {
                let samples: Vec<_> = if *filled == capture.len() {
                    capture[*write..]
                        .iter()
                        .chain(&capture[..*write])
                        .copied()
                        .collect()
                } else {
                    capture[..*filled].to_vec()
                };
                let crossings: Vec<_> = samples
                    .windows(2)
                    .enumerate()
                    .filter_map(|(index, pair)| {
                        (pair[0] < 0.0 && pair[1] >= 0.0).then_some(index + 1)
                    })
                    .collect();
                let average_period = if crossings.len() < 2 {
                    None
                } else {
                    Some(
                        crossings
                            .windows(2)
                            .map(|pair| (pair[1] - pair[0]) as f64)
                            .sum::<f64>()
                            / (crossings.len() - 1) as f64,
                    )
                };
                measurement.fundamental_hz =
                    average_period.map_or(0.0, |period| (self.sample_rate / period) as f32);
                if measurement.fundamental_hz < parameters.minimum_hz
                    || measurement.fundamental_hz > parameters.maximum_hz
                    || measurement.fundamental_hz == 0.0
                {
                    measurement.fundamental_hz = 0.0;
                    "--".clone_into(&mut measurement.note_name);
                    measurement.cents_offset = 0.0;
                    measurement.confidence = 0.0;
                } else {
                    let midi = 69.0
                        + 12.0
                            * (measurement.fundamental_hz / parameters.reference_pitch_hz).log2();
                    let rounded = midi.round();
                    ANALYZER_NOTE_NAMES[usize::from((rounded as i16).rem_euclid(12) as u8)]
                        .clone_into(&mut measurement.note_name);
                    measurement.cents_offset = (midi - rounded) * 100.0;
                    measurement.confidence = 1.0;
                }
                gaw_core::AnalyzerMeasurement::Tuner(measurement)
            }
            (_, _, measurement) => measurement,
        }
    }
}

impl DspProcessor for CanonicalAnalyzerProcessor {
    fn type_id(&self) -> &'static str {
        self.inner.type_id()
    }

    fn version(&self) -> u32 {
        self.inner.version()
    }

    fn input_layouts(&self) -> &'static [DspLayout] {
        self.inner.input_layouts()
    }

    fn output_layout(&self, input: DspLayout) -> Result<DspLayout, gaw_dsp::ProcessError> {
        self.inner.output_layout(input)
    }

    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), gaw_dsp::ProcessError> {
        self.inner.prepare(spec)?;
        self.initialize_state(spec);
        Ok(())
    }

    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[gaw_dsp::ParameterEvent],
        context: ProcessContext,
    ) -> Result<(), gaw_dsp::ProcessError> {
        self.inner.process(input, output, events, context)?;
        if self.inner.enabled() {
            self.analyze_configuration(input, context);
        }
        Ok(())
    }

    fn reset(&mut self) {
        self.inner.reset();
        self.reset_configuration();
    }

    fn seek(&mut self, absolute_frame: u64) {
        self.inner.seek(absolute_frame);
        self.reset_configuration();
    }

    fn latency_frames(&self) -> u32 {
        self.inner.latency_frames()
    }

    fn tail_frames(&self) -> u64 {
        self.inner.tail_frames()
    }

    fn parameters(&self) -> &'static [gaw_dsp::ParameterDescriptor] {
        self.inner.parameters()
    }

    fn enabled(&self) -> bool {
        self.inner.enabled()
    }

    fn set_enabled(&mut self, enabled: bool) {
        self.inner.set_enabled(enabled);
    }

    fn analyzer_measurement(&mut self) -> Option<gaw_core::AnalyzerMeasurement> {
        let measurement = self.inner.analyzer_measurement()?;
        Some(self.configured_measurement(measurement))
    }
}

/// Adapter that recreates project-configured gaw-dsp processors during preparation.
#[derive(Debug)]
pub struct DspProcessorAdapter {
    definitions: HashMap<String, gaw_core::Processor>,
    automation: HashMap<String, Vec<gaw_core::AutomationLane>>,
    tempo_bpm: f64,
    project_seed: u64,
    sample_rate: u32,
    render_revision: u64,
    analyzer_publisher: RwLock<Option<AnalyzerPublisher>>,
}

impl DspProcessorAdapter {
    fn new(project: &Project, tempo_bpm: f64, project_seed: u64, render_revision: u64) -> Self {
        let definitions = all_processors(project)
            .map(|processor| (processor.id.to_string(), processor.clone()))
            .collect();
        let mut automation: HashMap<String, Vec<gaw_core::AutomationLane>> = HashMap::new();
        for lane in &project.automation {
            let processor_id = match &lane.target {
                AutomationTarget::AudioClipProcessor { processor_id, .. }
                | AutomationTarget::CompositionClipProcessor { processor_id, .. }
                | AutomationTarget::TrackProcessor { processor_id, .. }
                | AutomationTarget::CompositionOutputProcessor { processor_id, .. } => {
                    processor_id.to_string()
                }
                AutomationTarget::Instrument { .. } => continue,
            };
            automation
                .entry(processor_id)
                .or_default()
                .push(lane.clone());
        }
        Self {
            definitions,
            automation,
            tempo_bpm,
            project_seed,
            sample_rate: project.sample_rate.value(),
            render_revision,
            analyzer_publisher: RwLock::new(None),
        }
    }

    fn set_analyzer_publisher(&self, publisher: AnalyzerPublisher) {
        *self.analyzer_publisher.write() = Some(publisher);
    }

    fn publish_measurement(
        &self,
        processor_id: &str,
        absolute_frame: u64,
        frames: usize,
        processor: &mut dyn DspProcessor,
    ) {
        let Some(measurement) = processor.analyzer_measurement() else {
            return;
        };
        let publisher = self.analyzer_publisher.read().clone();
        if let Some(publisher) = publisher {
            let _ = publisher.publish(
                processor_id,
                self.render_revision,
                AnalyzerFrameRange::new(absolute_frame, u64::try_from(frames).unwrap_or(u64::MAX)),
                measurement,
            );
        }
    }

    fn spec(
        &self,
        processor: &gaw_core::Processor,
        layout: ChannelLayout,
    ) -> Result<ProcessorSpec, CompileError> {
        let mut instance = self.instance(processor)?;
        let input = dsp_layout(layout);
        instance
            .output_layout(input)
            .map_err(|error| self.processor_error(processor, error.to_string()))?;
        // The render plan keeps the composition's declared layout. Processing
        // maps the DSP instance's actual output back into that fixed container.
        instance
            .prepare(PrepareSpec {
                sample_rate: f64::from(self.sample_rate),
                max_block_size: PROCESS_BLOCK_FRAMES,
                input_layout: input,
                tempo_bpm: self.tempo_bpm,
            })
            .map_err(|error| self.processor_error(processor, error.to_string()))?;
        let mut spec = ProcessorSpec::new(
            processor.id.to_string(),
            u64::from(instance.latency_frames()),
            instance.tail_frames(),
        );
        spec.enabled = processor.enabled;
        Ok(spec)
    }

    #[allow(clippy::unused_self)]
    fn processor_error(&self, processor: &gaw_core::Processor, message: String) -> CompileError {
        CompileError::Processor {
            processor: processor.id.to_string(),
            message,
        }
    }

    #[allow(clippy::too_many_lines)]
    fn instance(
        &self,
        processor: &gaw_core::Processor,
    ) -> Result<Box<dyn DspProcessor>, CompileError> {
        let enabled = processor.enabled;
        let mut instance: Box<dyn DspProcessor> = match &processor.kind {
            ProcessorKind::Gain(value) => {
                let mut json = serde_json::to_value(value).map_err(CompileError::Revision)?;
                json["pan_law"] = match value.pan_law {
                    gaw_core::PanLaw::MinusThreeDb => Value::from("equal_power"),
                    gaw_core::PanLaw::MinusSixDb => Value::from("linear"),
                };
                Box::new(
                    serde_json::from_value::<gaw_dsp::Gain>(json)
                        .map_err(CompileError::Revision)?,
                )
            }
            ProcessorKind::StereoTool(value) => boxed_from::<_, gaw_dsp::StereoTool>(value)?,
            ProcessorKind::Filter(value) => {
                let mut json = serde_json::to_value(value).map_err(CompileError::Revision)?;
                json["slope_db_per_octave"] = Value::from(filter_slope(value.slope_db_per_octave));
                Box::new(
                    serde_json::from_value::<gaw_dsp::Filter>(json)
                        .map_err(CompileError::Revision)?,
                )
            }
            ProcessorKind::ParametricEq(value) => {
                let mut json = serde_json::to_value(value).map_err(CompileError::Revision)?;
                if let Some(bands) = json["bands"].as_array_mut() {
                    for (json, band) in bands.iter_mut().zip(&value.bands) {
                        json["slope_db_per_octave"] =
                            Value::from(filter_slope(band.slope_db_per_octave));
                    }
                }
                Box::new(
                    serde_json::from_value::<gaw_dsp::ParametricEq>(json)
                        .map_err(CompileError::Revision)?,
                )
            }
            ProcessorKind::Compressor(value) => {
                Box::new(gaw_dsp::Compressor::new(from_value(value)?))
            }
            ProcessorKind::Limiter(value) => Box::new(gaw_dsp::Limiter::new(from_value(value)?)),
            ProcessorKind::Gate(value) => Box::new(gaw_dsp::Gate::new(from_value(value)?)),
            ProcessorKind::Expander(value) => Box::new(gaw_dsp::Expander::new(from_value(value)?)),
            ProcessorKind::TransientShaper(value) => {
                Box::new(gaw_dsp::TransientShaper::new(from_value(value)?))
            }
            ProcessorKind::Saturator(value) => {
                Box::new(gaw_dsp::Saturator::new(from_value(value)?))
            }
            ProcessorKind::Clipper(value) => Box::new(gaw_dsp::Clipper::new(from_value(value)?)),
            ProcessorKind::Bitcrusher(value) => {
                if value.bit_depth > 24 {
                    return Err(self.processor_error(
                        processor,
                        "bit depths above 24 are absent from gaw-dsp".into(),
                    ));
                }
                let mut json = serde_json::to_value(value).map_err(CompileError::Revision)?;
                json["seed"] = Value::from(stable_seed(self.project_seed, processor.id.as_str()));
                Box::new(gaw_dsp::Bitcrusher::new(
                    serde_json::from_value(json).map_err(CompileError::Revision)?,
                ))
            }
            ProcessorKind::Delay(value) => {
                let mut json = serde_json::to_value(value).map_err(CompileError::Revision)?;
                let base = time_seconds(value.time, self.tempo_bpm);
                let offset = time_seconds(value.stereo_offset, self.tempo_bpm);
                let relative = if base == 0.0 {
                    if offset == 0.0 {
                        0.0
                    } else {
                        return Err(self.processor_error(
                            processor,
                            "a nonzero stereo offset cannot accompany zero delay time".into(),
                        ));
                    }
                } else {
                    offset / base
                };
                if relative > 1.0 {
                    return Err(self.processor_error(
                        processor,
                        "stereo offset exceeds gaw-dsp's exact ±1x relative range".into(),
                    ));
                }
                json["stereo_offset"] = Value::from(relative);
                Box::new(
                    serde_json::from_value::<gaw_dsp::Delay>(json)
                        .map_err(CompileError::Revision)?,
                )
            }
            ProcessorKind::Reverb(value) => boxed_from::<_, gaw_dsp::Reverb>(value)?,
            ProcessorKind::Chorus(value) => boxed_from::<_, gaw_dsp::Chorus>(value)?,
            ProcessorKind::Flanger(value) => boxed_from::<_, gaw_dsp::Flanger>(value)?,
            ProcessorKind::Phaser(value) => boxed_from::<_, gaw_dsp::Phaser>(value)?,
            ProcessorKind::TremoloAutopan(value) => {
                boxed_from::<_, gaw_dsp::TremoloAutopan>(value)?
            }
            ProcessorKind::PitchShift(value) => boxed_from::<_, gaw_dsp::PitchShift>(value)?,
            ProcessorKind::RhythmicGate(value) => {
                let mut json = serde_json::to_value(value).map_err(CompileError::Revision)?;
                json["steps"] = Value::Array(
                    value
                        .steps
                        .iter()
                        .map(|step| Value::from(step.level))
                        .collect(),
                );
                Box::new(
                    serde_json::from_value::<gaw_dsp::RhythmicGate>(json)
                        .map_err(CompileError::Revision)?,
                )
            }
            ProcessorKind::BeatRepeat(value) => {
                let mut json = serde_json::to_value(value).map_err(CompileError::Revision)?;
                json["seed"] =
                    Value::from(value.seed ^ stable_seed(self.project_seed, processor.id.as_str()));
                Box::new(
                    serde_json::from_value::<gaw_dsp::BeatRepeat>(json)
                        .map_err(CompileError::Revision)?,
                )
            }
            ProcessorKind::LevelMeter(value) => Box::new(CanonicalAnalyzerProcessor::new(
                Box::new(gaw_dsp::AnalyzerTap::level_meter()),
                CanonicalAnalyzerConfig::Level(value.clone()),
            )),
            ProcessorKind::LoudnessMeter(value) => Box::new(CanonicalAnalyzerProcessor::new(
                Box::new(gaw_dsp::AnalyzerTap::loudness_meter()),
                CanonicalAnalyzerConfig::Loudness(value.clone()),
            )),
            ProcessorKind::Spectrum(value) => {
                let mut analyzer = gaw_dsp::AnalyzerTap::spectrum();
                let size = fft_size(value.fft_size);
                analyzer.analyzer_mut().config = gaw_dsp::SpectrumConfig {
                    fft_size: size,
                    bins: (size / 2 + 1).min(512),
                };
                Box::new(CanonicalAnalyzerProcessor::new(
                    Box::new(analyzer),
                    CanonicalAnalyzerConfig::Spectrum(value.clone()),
                ))
            }
            ProcessorKind::Oscilloscope(value) => {
                let mut analyzer = gaw_dsp::AnalyzerTap::oscilloscope();
                analyzer.analyzer_mut().config = gaw_dsp::OscilloscopeConfig {
                    capture_frames: CanonicalAnalyzerProcessor::duration_frames(
                        f64::from(self.sample_rate),
                        value.window_ms,
                    ),
                };
                Box::new(CanonicalAnalyzerProcessor::new(
                    Box::new(analyzer),
                    CanonicalAnalyzerConfig::Oscilloscope(value.clone()),
                ))
            }
            ProcessorKind::StereoMeter(value) => Box::new(CanonicalAnalyzerProcessor::new(
                Box::new(gaw_dsp::AnalyzerTap::stereo_meter()),
                CanonicalAnalyzerConfig::Stereo(value.clone()),
            )),
            ProcessorKind::Tuner(value) => Box::new(CanonicalAnalyzerProcessor::new(
                Box::new(gaw_dsp::AnalyzerTap::tuner()),
                CanonicalAnalyzerConfig::Tuner(value.clone()),
            )),
        };
        instance.set_enabled(enabled);
        Ok(instance)
    }
}

impl DspProcessorAdapter {
    fn write_processor_chain_wav(
        &self,
        path: &Path,
        source: &dyn FrameSource,
        effects: &[gaw_core::Processor],
        latency_frames: u64,
    ) -> Result<(), CompileError> {
        let layout = source.channel_layout();
        let channels = layout.channels();
        let mut instances = Vec::<(String, Box<dyn DspProcessor>)>::new();
        for definition in effects.iter().filter(|effect| effect.enabled) {
            let mut processor = self.instance(definition)?;
            processor
                .prepare(PrepareSpec {
                    sample_rate: f64::from(self.sample_rate),
                    max_block_size: PROCESS_BLOCK_FRAMES,
                    input_layout: dsp_layout(layout),
                    tempo_bpm: self.tempo_bpm,
                })
                .map_err(|error| CompileError::Processor {
                    processor: definition.id.to_string(),
                    message: error.to_string(),
                })?;
            instances.push((definition.id.to_string(), processor));
        }

        let mut writer = wav_writer(path, self.sample_rate, layout)?;
        let mut input = vec![0.0; PROCESS_BLOCK_FRAMES * channels];
        let mut scratch = vec![0.0; PROCESS_BLOCK_FRAMES * channels];
        let mut position = 0_u64;
        let mut crop = latency_frames;
        while position < source.frame_count() {
            let frames =
                usize::try_from((source.frame_count() - position).min(PROCESS_BLOCK_FRAMES as u64))
                    .unwrap_or(PROCESS_BLOCK_FRAMES);
            let samples = frames * channels;
            let read = source.read_interleaved(position, &mut input[..samples])?;
            if read != frames {
                return Err(crate::AssetError::SourceEndedEarly {
                    frame: position.saturating_add(read as u64),
                }
                .into());
            }
            for (processor_id, processor) in &mut instances {
                self.process_stream_block(
                    processor_id,
                    processor.as_mut(),
                    layout,
                    position,
                    &input[..samples],
                    &mut scratch[..samples],
                )?;
                std::mem::swap(&mut input, &mut scratch);
            }
            let skipped = usize::try_from(crop.min(frames as u64)).unwrap_or(frames);
            crop -= skipped as u64;
            for &sample in &input[skipped * channels..samples] {
                writer.write_sample(if sample.is_finite() { sample } else { 0.0 })?;
            }
            position += frames as u64;
        }
        for (processor_id, processor) in &mut instances {
            self.publish_measurement(
                processor_id,
                0,
                usize::try_from(source.frame_count()).unwrap_or(usize::MAX),
                processor.as_mut(),
            );
        }
        writer.finalize()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn process_stream_block(
        &self,
        processor_id: &str,
        processor: &mut dyn DspProcessor,
        layout: ChannelLayout,
        absolute_frame: u64,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<(), CompileError> {
        let channels = layout.channels();
        let processor_layout = processor
            .output_layout(dsp_layout(layout))
            .map_err(|error| CompileError::Processor {
                processor: processor_id.to_owned(),
                message: error.to_string(),
            })?;
        let output_channels = processor_layout.channels();
        let frames = input.len() / channels;
        let mut in_planar = vec![vec![0.0; frames]; channels];
        let mut out_planar = vec![vec![0.0; frames]; output_channels];
        for frame in 0..frames {
            for channel in 0..channels {
                in_planar[channel][frame] = input[frame * channels + channel];
            }
        }
        let automated: Vec<_> = self
            .automation
            .get(processor_id)
            .into_iter()
            .flatten()
            .map(|lane| {
                let parameter_id = automation_parameter_id(&lane.target);
                let descriptor = processor
                    .parameters()
                    .iter()
                    .find(|descriptor| parameter_ids_match(descriptor.id, parameter_id))
                    .ok_or_else(|| CompileError::Processor {
                        processor: processor_id.to_owned(),
                        message: format!("DSP parameter `{parameter_id}` is missing"),
                    })?;
                if !descriptor.automatable {
                    return Err(CompileError::Processor {
                        processor: processor_id.to_owned(),
                        message: format!("DSP parameter `{parameter_id}` is not automatable"),
                    });
                }
                Ok((lane, descriptor, parameter_id))
            })
            .collect::<Result<_, CompileError>>()?;
        let mut events = Vec::with_capacity(frames.saturating_mul(automated.len()));
        for sample_offset in 0..frames {
            let frame = absolute_frame.saturating_add(sample_offset as u64);
            let beats = frame as f64 * self.tempo_bpm / (60.0 * f64::from(self.sample_rate));
            let time = gaw_core::Beats::new(beats)?;
            for &(lane, descriptor, parameter_id) in &automated {
                let value = lane.value_at(time).ok_or_else(|| CompileError::Processor {
                    processor: processor_id.to_owned(),
                    message: format!("automation lane `{}` has no value", lane.id),
                })?;
                let value = dsp_automation_value(value, descriptor).map_err(|message| {
                    CompileError::Processor {
                        processor: processor_id.to_owned(),
                        message,
                    }
                })?;
                events.push(gaw_dsp::ParameterEvent::new(
                    sample_offset,
                    parameter_id,
                    value,
                ));
            }
        }
        let inputs: Vec<&[f32]> = in_planar.iter().map(Vec::as_slice).collect();
        let mut outputs: Vec<&mut [f32]> = out_planar.iter_mut().map(Vec::as_mut_slice).collect();
        processor
            .process(
                &inputs,
                &mut outputs,
                &events,
                ProcessContext {
                    absolute_frame,
                    tempo_bpm: self.tempo_bpm,
                },
            )
            .map_err(|error| CompileError::Processor {
                processor: processor_id.to_owned(),
                message: error.to_string(),
            })?;
        for frame in 0..frames {
            for channel in 0..channels {
                output[frame * channels + channel] = match (channels, output_channels) {
                    (2, 1) => out_planar[0][frame],
                    (1, 2) => (out_planar[0][frame] + out_planar[1][frame]) * 0.5,
                    _ => out_planar[channel][frame],
                };
            }
        }
        Ok(())
    }
}

impl ProcessorAdapter for DspProcessorAdapter {
    fn process(
        &self,
        spec: &ProcessorSpec,
        sample_rate: u32,
        layout: ChannelLayout,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<(), String> {
        self.process_at(spec, sample_rate, layout, 0, input, output)
    }

    fn process_at(
        &self,
        spec: &ProcessorSpec,
        sample_rate: u32,
        layout: ChannelLayout,
        absolute_frame: u64,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<(), String> {
        let definition = self
            .definitions
            .get(&spec.id)
            .ok_or_else(|| "processor definition is missing".to_owned())?;
        let mut processor = self
            .instance(definition)
            .map_err(|error| error.to_string())?;
        let channels = layout.channels();
        processor
            .prepare(PrepareSpec {
                sample_rate: f64::from(sample_rate),
                max_block_size: PROCESS_BLOCK_FRAMES,
                input_layout: dsp_layout(layout),
                tempo_bpm: self.tempo_bpm,
            })
            .map_err(|error| error.to_string())?;
        let processor_layout = processor
            .output_layout(dsp_layout(layout))
            .map_err(|error| error.to_string())?;
        let output_channels = processor_layout.channels();
        processor.seek(absolute_frame);
        let automated: Vec<_> = self
            .automation
            .get(&spec.id)
            .into_iter()
            .flatten()
            .map(|lane| {
                let parameter_id = automation_parameter_id(&lane.target);
                let descriptor = processor
                    .parameters()
                    .iter()
                    .find(|descriptor| parameter_ids_match(descriptor.id, parameter_id))
                    .ok_or_else(|| format!("DSP parameter `{parameter_id}` is missing"))?;
                if !descriptor.automatable {
                    return Err(format!("DSP parameter `{parameter_id}` is not automatable"));
                }
                Ok((lane, descriptor, parameter_id))
            })
            .collect::<Result<_, String>>()?;
        output.fill(0.0);
        for start in (0..input.len() / channels).step_by(PROCESS_BLOCK_FRAMES) {
            let frames = (input.len() / channels - start).min(PROCESS_BLOCK_FRAMES);
            let mut in_planar = vec![vec![0.0; frames]; channels];
            let mut out_planar = vec![vec![0.0; frames]; output_channels];
            for frame in 0..frames {
                for channel in 0..channels {
                    in_planar[channel][frame] = input[(start + frame) * channels + channel];
                }
            }
            let inputs: Vec<&[f32]> = in_planar.iter().map(Vec::as_slice).collect();
            let mut outputs: Vec<&mut [f32]> =
                out_planar.iter_mut().map(Vec::as_mut_slice).collect();
            let mut events = Vec::with_capacity(frames.saturating_mul(automated.len()));
            for sample_offset in 0..frames {
                let frame = absolute_frame
                    .saturating_add(start as u64)
                    .saturating_add(sample_offset as u64);
                let beats = frame as f64 * self.tempo_bpm / (60.0 * f64::from(sample_rate));
                let time = gaw_core::Beats::new(beats).map_err(|error| error.to_string())?;
                for &(lane, descriptor, parameter_id) in &automated {
                    let value = lane
                        .value_at(time)
                        .ok_or_else(|| format!("automation lane `{}` has no value", lane.id))?;
                    let value = dsp_automation_value(value, descriptor)?;
                    events.push(gaw_dsp::ParameterEvent::new(
                        sample_offset,
                        parameter_id,
                        value,
                    ));
                }
            }
            processor
                .process(
                    &inputs,
                    &mut outputs,
                    &events,
                    ProcessContext {
                        absolute_frame: absolute_frame.saturating_add(start as u64),
                        tempo_bpm: self.tempo_bpm,
                    },
                )
                .map_err(|error| error.to_string())?;
            for frame in 0..frames {
                for channel in 0..channels {
                    output[(start + frame) * channels + channel] = match (channels, output_channels)
                    {
                        (2, 1) => out_planar[0][frame],
                        (1, 2) => (out_planar[0][frame] + out_planar[1][frame]) * 0.5,
                        _ => out_planar[channel][frame],
                    };
                }
            }
        }
        self.publish_measurement(
            &spec.id,
            absolute_frame,
            input.len() / channels,
            processor.as_mut(),
        );
        Ok(())
    }
}

fn automation_parameter_id(target: &AutomationTarget) -> &str {
    match target {
        AutomationTarget::AudioClipProcessor { parameter_id, .. }
        | AutomationTarget::CompositionClipProcessor { parameter_id, .. }
        | AutomationTarget::TrackProcessor { parameter_id, .. }
        | AutomationTarget::CompositionOutputProcessor { parameter_id, .. }
        | AutomationTarget::Instrument { parameter_id, .. } => parameter_id,
    }
}

fn normalized_parameter_id(id: &str) -> String {
    let mut parts = id.split('.');
    let Some(head @ ("bands" | "steps")) = parts.next() else {
        return id.to_owned();
    };
    let Some(index) = parts.next() else {
        return id.to_owned();
    };
    let rest = parts.collect::<Vec<_>>();
    if (index == "[]" || index.parse::<usize>().is_ok()) && !rest.is_empty() {
        format!("{head}[].{}", rest.join("."))
    } else {
        id.to_owned()
    }
}

fn parameter_ids_match(descriptor: &str, requested: &str) -> bool {
    descriptor == requested
        || normalized_parameter_id(descriptor) == normalized_parameter_id(requested)
}

fn dsp_automation_value(
    value: AutomationValue,
    descriptor: &gaw_dsp::ParameterDescriptor,
) -> Result<gaw_dsp::ParameterValue, String> {
    use gaw_dsp::{ParameterKind, ParameterValue};
    let value = match descriptor.kind {
        ParameterKind::Float { .. } => ParameterValue::Float(value.number() as f32),
        ParameterKind::Time { .. } => match value {
            AutomationValue::Seconds(value) => ParameterValue::Seconds(value.value() as f32),
            AutomationValue::Beats(value) => ParameterValue::Beats(value.value() as f32),
            _ => {
                return Err(format!(
                    "parameter `{}` requires seconds or beats",
                    descriptor.id
                ));
            }
        },
        ParameterKind::Rate { .. } => match value {
            AutomationValue::Hertz(value) => ParameterValue::Hertz(value.value() as f32),
            AutomationValue::Beats(value) => ParameterValue::Beats(value.value() as f32),
            _ => {
                return Err(format!(
                    "parameter `{}` requires hertz or beats",
                    descriptor.id
                ));
            }
        },
        ParameterKind::Integer { .. }
        | ParameterKind::UnsignedInteger { .. }
        | ParameterKind::Boolean
        | ParameterKind::Choice(_) => {
            return Err(format!("parameter `{}` is discrete", descriptor.id));
        }
    };
    descriptor.accepts(value).then_some(value).ok_or_else(|| {
        format!(
            "automation value is invalid for DSP parameter `{}`",
            descriptor.id
        )
    })
}

fn boxed_from<S: Serialize, P: DspProcessor + DeserializeOwned + 'static>(
    source: &S,
) -> Result<Box<dyn DspProcessor>, CompileError> {
    Ok(Box::new(from_value::<S, P>(source)?))
}

fn from_value<S: Serialize, T: DeserializeOwned>(source: &S) -> Result<T, CompileError> {
    serde_json::from_value(serde_json::to_value(source).map_err(CompileError::Revision)?)
        .map_err(CompileError::Revision)
}

fn time_seconds(value: gaw_core::TimeValue, bpm: f64) -> f64 {
    match value {
        gaw_core::TimeValue::Seconds(value) => value,
        gaw_core::TimeValue::Beats(value) => value * 60.0 / bpm,
    }
}

fn filter_slope(value: gaw_core::FilterSlope) -> u32 {
    match value {
        gaw_core::FilterSlope::Db12 => 12,
        gaw_core::FilterSlope::Db24 => 24,
        gaw_core::FilterSlope::Db48 => 48,
    }
}

fn all_processors(project: &Project) -> impl Iterator<Item = &gaw_core::Processor> {
    project
        .assets
        .iter()
        .filter_map(|asset| match &asset.definition {
            AudioAssetDefinition::Processed { effects, .. } => Some(effects.as_slice()),
            _ => None,
        })
        .flatten()
        .chain(
            project
                .compositions
                .iter()
                .flat_map(|composition| &composition.output_effects),
        )
        .chain(project.tracks.iter().flat_map(|track| &track.effects))
        .chain(
            project
                .tracks
                .iter()
                .flat_map(|track| &track.clips)
                .filter_map(|clip| match clip {
                    Clip::Audio(value) => Some(value.effects.as_slice()),
                    Clip::Composition(value) => Some(value.effects.as_slice()),
                    Clip::Event(_) => None,
                })
                .flatten(),
        )
}

const fn fft_size(value: gaw_core::FftSize) -> usize {
    match value {
        gaw_core::FftSize::N256 => 256,
        gaw_core::FftSize::N512 => 512,
        gaw_core::FftSize::N1024 => 1_024,
        gaw_core::FftSize::N2048 => 2_048,
        gaw_core::FftSize::N4096 => 4_096,
        gaw_core::FftSize::N8192 => 8_192,
        gaw_core::FftSize::N16384 => 16_384,
    }
}

fn stable_seed(seed: u64, id: &str) -> u64 {
    id.bytes().fold(seed ^ 0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    })
}

fn project_revision(project: &Project) -> Result<u64, CompileError> {
    let bytes = serde_json::to_vec(project).map_err(CompileError::Revision)?;
    Ok(bytes.into_iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3)
    }))
}

#[cfg(test)]
#[allow(
    clippy::cast_precision_loss,
    clippy::field_reassign_with_default,
    clippy::float_cmp,
    clippy::similar_names
)]
mod tests {
    use super::*;
    use gaw_core::{
        AssetId as CoreAssetId, AssetTempo, AudioAsset, AutomationCurve, AutomationLane,
        AutomationLaneId, AutomationPoint, Beats, Bpm, Composition, CompositionClip, ContentHash,
        Decibels, EventClip, EventData, Fade, FrameCount, ImportedAudio, Instrument, MidiNote,
        Milliseconds, NoteEvent as CoreNoteEvent, NoteRange, ProcessorId, ProjectPath, Ratio,
        SampleRate, Sampler, SamplerZone, SamplerZoneId, Seconds, SourceRange, Track,
        VelocityRange,
    };

    fn seconds(value: f64) -> Seconds {
        Seconds::new(value).unwrap()
    }

    fn beats(value: f64) -> Beats {
        Beats::new(value).unwrap()
    }

    fn project(sample_rate: u32, bpm: f64, length: f64) -> Project {
        let mut project = Project::new(
            "fixture",
            Bpm::new(bpm).unwrap(),
            SampleRate::new(sample_rate).unwrap(),
        );
        project.compositions[0].length = beats(length);
        project.compositions[0].output_layout = gaw_core::ChannelLayout::Stereo;
        project.settings.maximum_tail = seconds(0.1);
        project
    }

    fn add_asset(project: &mut Project, frames: u64, tempo: Option<f64>) -> gaw_core::AssetId {
        let imported = ImportedAudio {
            media_path: ProjectPath::new("audio/source.wav").unwrap(),
            original_filename: "source.wav".into(),
            content_hash: ContentHash::new("0".repeat(64)).unwrap(),
            sample_rate: project.sample_rate,
            layout: gaw_core::ChannelLayout::Stereo,
            frames: FrameCount(frames),
        };
        let mut asset = AudioAsset::imported("source", imported);
        asset.tempo = tempo.map(|bpm| AssetTempo {
            bpm: Bpm::new(bpm).unwrap(),
            first_beat: seconds(0.0),
        });
        let id = asset.id;
        project.assets.push(asset);
        id
    }

    fn decoded(id: gaw_core::AssetId, samples: Vec<f32>) -> AssetSourceMap {
        AssetSourceMap::new().with_source(
            id.to_string(),
            Arc::new(
                MemoryFrameSource::new(ChannelLayout::Stereo, Arc::<[f32]>::from(samples)).unwrap(),
            ),
        )
    }

    fn gain(id: &str, gain_db: f32) -> gaw_core::Processor {
        let mut parameters = gaw_core::GainParameters::default();
        parameters.gain_db = gain_db;
        gaw_core::Processor::new(
            ProcessorId::new(id).unwrap(),
            ProcessorKind::Gain(parameters),
        )
    }

    #[test]
    fn compiles_nested_child_first_and_maps_every_timeline_effect_scope() {
        let mut project = project(4, 60.0, 2.0);
        let asset_id = add_asset(&mut project, 8, None);
        let processed_id = CoreAssetId::new();
        project.assets.push(AudioAsset {
            id: processed_id,
            name: "processed".into(),
            definition: AudioAssetDefinition::Processed {
                source_asset_id: asset_id,
                transforms: Vec::new(),
                effects: vec![gain("asset-fx", 0.0)],
            },
            tempo: None,
            revisions: Vec::new(),
            current_revision_id: None,
        });
        let root_id = project.root_composition_id;
        let mut child = Composition::new("child", beats(1.0));
        child.output_layout = gaw_core::ChannelLayout::Stereo;
        child.output_effects.push(gain("child-output", 0.0));
        let child_id = child.id;
        let mut child_track = Track::audio(child_id, "child-track");
        child_track.effects.push(gain("child-track-fx", 0.0));
        let mut audio = gaw_core::AudioClip::new(
            processed_id,
            beats(0.0),
            beats(1.0),
            SourceRange {
                start: seconds(0.25),
                duration: seconds(1.0),
            },
        );
        audio.effects.push(gain("audio-clip-fx", 0.0));
        child_track.clips.push(Clip::Audio(audio));
        child.track_ids.push(child_track.id);

        let mut parent_track = Track::audio(root_id, "parent-track");
        parent_track.effects.push(gain("parent-track-fx", 0.0));
        let mut nested = CompositionClip::new(child_id, beats(0.5), beats(1.0));
        nested.effects.push(gain("composition-clip-fx", 0.0));
        parent_track.clips.push(Clip::Composition(nested));
        project.compositions[0].track_ids.push(parent_track.id);
        project.compositions[0]
            .output_effects
            .push(gain("root-output", 0.0));
        project.compositions.push(child);
        project.tracks.extend([parent_track, child_track]);

        let sources = decoded(asset_id, (0..16).map(|value| value as f32).collect());
        let compiled = compile_project(&project, &sources).unwrap();
        assert_eq!(
            compiled.plan().compositions[0].id.as_ref(),
            child_id.to_string()
        );
        let child = &compiled.plan().compositions[0];
        assert_eq!(child.tracks[0].clips[0].processors[0].id, "audio-clip-fx");
        assert_eq!(child.tracks[0].processors[0].id, "child-track-fx");
        assert_eq!(child.processors[0].id, "child-output");
        let root = compiled.plan().root();
        assert_eq!(
            root.tracks[0].clips[0].processors[0].id,
            "composition-clip-fx"
        );
        assert_eq!(root.tracks[0].processors[0].id, "parent-track-fx");
        assert_eq!(root.processors[0].id, "root-output");
        assert!(compiled.processors.definitions.contains_key("asset-fx"));
        let prepared = compiled.prepare().unwrap();
        for (actual, expected) in prepared.root().samples()[4..12]
            .iter()
            .zip([2.0_f32, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0])
        {
            assert!((actual - expected).abs() < 0.000_01);
        }
    }

    #[derive(Debug)]
    struct ExactStub;

    impl TempoStretcher for ExactStub {
        fn stretch(
            &self,
            input: &[f32],
            layout: ChannelLayout,
            _: u32,
            output_frames: usize,
        ) -> Result<Vec<f32>, String> {
            let channels = layout.channels();
            let input_frames = input.len() / channels;
            let mut output = vec![0.0; output_frames * channels];
            for frame in 0..output_frames {
                let source = frame.saturating_mul(input_frames) / output_frames.max(1);
                output[frame * channels..(frame + 1) * channels]
                    .copy_from_slice(&input[source * channels..(source + 1) * channels]);
            }
            Ok(output)
        }

        fn stretch_source(
            &self,
            source: &dyn FrameSource,
            layout: ChannelLayout,
            _: u32,
            output_frames: usize,
            emit: &mut dyn FnMut(&[f32]) -> Result<(), String>,
        ) -> Result<(), String> {
            let channels = layout.channels();
            let input_frames =
                usize::try_from(source.frame_count()).map_err(|error| error.to_string())?;
            let mut block = vec![0.0; PROCESS_BLOCK_FRAMES * channels];
            let mut output_position = 0_usize;
            while output_position < output_frames {
                let frames = (output_frames - output_position).min(PROCESS_BLOCK_FRAMES);
                for frame in 0..frames {
                    let source_frame = (output_position + frame).saturating_mul(input_frames)
                        / output_frames.max(1);
                    let read = source
                        .read_interleaved(
                            source_frame as u64,
                            &mut block[frame * channels..(frame + 1) * channels],
                        )
                        .map_err(|error| error.to_string())?;
                    if read != 1 {
                        return Err(format!("source ended at frame {source_frame}"));
                    }
                }
                emit(&block[..frames * channels])?;
                output_position += frames;
            }
            Ok(())
        }
    }

    fn tempo_project(mode: TempoSync) -> (Project, AssetSourceMap) {
        let mut project = project(100, 120.0, 1.0);
        let asset_id = add_asset(&mut project, 100, Some(60.0));
        let root = project.root_composition_id;
        let mut track = Track::audio(root, "track");
        let mut clip = gaw_core::AudioClip::new(
            asset_id,
            beats(0.0),
            beats(1.0),
            SourceRange {
                start: seconds(0.0),
                duration: seconds(1.0),
            },
        );
        clip.tempo_sync = mode;
        clip.reverse = true;
        clip.fade_in = Some(Fade {
            duration: seconds(0.01),
            curve: FadeCurve::Linear,
        });
        track.clips.push(Clip::Audio(clip));
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        let samples: Vec<f32> = (0..100)
            .flat_map(|frame| [frame as f32 / 100.0; 2])
            .collect();
        (project, decoded(asset_id, samples))
    }

    #[test]
    fn compiles_none_repitch_and_replaceable_stretch_tempo_modes() {
        for mode in [TempoSync::None, TempoSync::Repitch, TempoSync::Stretch] {
            let (project, sources) = tempo_project(mode);
            let project_compiler = ProjectCompiler::new(&ExactStub);
            let compiled_project = project_compiler.compile(&project, &sources).unwrap();
            let prepared = compiled_project.prepare().unwrap();
            assert_eq!(prepared.root().main_frames(), 50);
            assert_eq!(prepared.root().samples()[0], 0.0);
            assert!(
                prepared
                    .root()
                    .samples()
                    .iter()
                    .all(|sample| sample.is_finite())
            );
        }
    }

    #[test]
    fn processor_automation_uses_units_curves_and_absolute_time() {
        let mut project = project(100, 60.0, 1.0);
        let asset_id = add_asset(&mut project, 100, None);
        let root = project.root_composition_id;
        let mut track = Track::audio(root, "track");
        track.clips.push(Clip::Audio(gaw_core::AudioClip::new(
            asset_id,
            beats(0.0),
            beats(1.0),
            SourceRange {
                start: seconds(0.0),
                duration: seconds(1.0),
            },
        )));
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        let processor = gain("automated-gain", 0.0);
        let processor_id = processor.id.clone();
        project.compositions[0].output_effects.push(processor);
        project.automation.push(AutomationLane {
            id: AutomationLaneId::new(),
            composition_id: root,
            name: "fade up".into(),
            target: AutomationTarget::CompositionOutputProcessor {
                processor_id,
                parameter_id: "gain_db".into(),
            },
            points: vec![
                AutomationPoint {
                    time: beats(0.0),
                    value: AutomationValue::Decibels(Decibels::new(-12.0).unwrap()),
                    curve: AutomationCurve::Linear,
                },
                AutomationPoint {
                    time: beats(1.0),
                    value: AutomationValue::Decibels(Decibels::new(0.0).unwrap()),
                    curve: AutomationCurve::Linear,
                },
            ],
        });
        let prepared = compile_project(&project, &decoded(asset_id, vec![1.0; 200]))
            .unwrap()
            .prepare()
            .unwrap();
        assert!(prepared.root().samples()[20] < prepared.root().samples()[180]);
        assert!(prepared.root().samples()[20] < 0.5);
        assert!(prepared.root().samples()[180] > 0.8);
    }

    #[test]
    fn staged_stretch_is_deterministic_beyond_two_times() {
        let audio = AudioBuffer {
            layout: ChannelLayout::Mono,
            samples: (0..20).map(|value| value as f32).collect(),
        };
        let first = stretch_audio(audio.clone(), 0.25, 48_000, &ExactStub).unwrap();
        let second = stretch_audio(audio, 0.25, 48_000, &ExactStub).unwrap();
        assert_eq!(first.frames(), 80);
        assert_eq!(first.samples, second.samples);
    }

    #[test]
    fn tempo_sync_preserves_a_nonzero_first_beat_marker_phase() {
        let mut project = project(100, 120.0, 1.0);
        let asset_id = add_asset(&mut project, 100, Some(60.0));
        project.assets[0].tempo.as_mut().unwrap().first_beat = seconds(0.2);
        let root = project.root_composition_id;
        let mut track = Track::audio(root, "track");
        let mut clip = gaw_core::AudioClip::new(
            asset_id,
            beats(0.0),
            beats(1.0),
            SourceRange {
                start: seconds(0.0),
                duration: seconds(1.0),
            },
        );
        clip.tempo_sync = TempoSync::Stretch;
        track.clips.push(Clip::Audio(clip));
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        let mut samples = vec![0.0; 200];
        samples[40] = 1.0;
        samples[41] = 1.0;
        let prepared = ProjectCompiler::new(&ExactStub)
            .compile(&project, &decoded(asset_id, samples))
            .unwrap()
            .prepare()
            .unwrap();
        assert_eq!(&prepared.root().samples()[20..22], &[1.0, 1.0]);
    }

    #[test]
    fn sampler_events_render_with_gain_release_and_stereo_rules() {
        let mut project = project(1_000, 60.0, 0.02);
        let asset_id = add_asset(&mut project, 64, None);
        let root = project.root_composition_id;
        let zone = SamplerZone {
            id: SamplerZoneId::new(),
            name: "zone".into(),
            asset_id,
            source: SourceRange {
                start: seconds(0.0),
                duration: seconds(0.064),
            },
            root_note: MidiNote::new(60).unwrap(),
            note_range: NoteRange::new(60, 60).unwrap(),
            velocity_range: VelocityRange::new(0, 127).unwrap(),
            playback: SamplerPlayback::NoteGated,
            gain: Decibels::new(0.0).unwrap(),
            velocity_sensitivity: Ratio::new(1.0).unwrap(),
            attack: Milliseconds::new(0.0).unwrap(),
            release: Milliseconds::new(1.0).unwrap(),
            reverse: false,
            choke_group: None,
        };
        let mut sampler = Sampler::new(4).unwrap();
        sampler.zones.push(zone);
        let instrument = Instrument::sampler("sampler", sampler);
        let mut events = EventData::new("notes");
        events.events.push(Event::Note(
            CoreNoteEvent::new(beats(0.005), beats(0.003), 60, 127).unwrap(),
        ));
        let mut track = Track::event(root, "event", instrument);
        track.clips.push(Clip::Event(EventClip::new(
            events.id,
            beats(0.0),
            beats(0.02),
        )));
        project.event_data.push(events);
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        let sources = decoded(asset_id, vec![1.0; 128]);
        let rendered = compile_project(&project, &sources)
            .unwrap()
            .prepare()
            .unwrap();
        assert!(
            rendered.root().samples()[..10]
                .iter()
                .all(|sample| *sample == 0.0)
        );
        assert_eq!(&rendered.root().samples()[10..16], &[1.0; 6]);
        assert!(
            rendered.root().samples()[18..]
                .iter()
                .all(|sample| *sample == 0.0)
        );

        let Clip::Event(chased) = &mut project.tracks[0].clips[0] else {
            unreachable!()
        };
        chased.source_start = beats(0.006);
        let chased = compile_project(&project, &sources)
            .unwrap()
            .prepare()
            .unwrap();
        assert!(
            chased.root().samples()[0..4]
                .iter()
                .all(|sample| *sample > 0.0)
        );
    }

    #[test]
    fn project_edits_change_revision_without_mutating_old_snapshot() {
        let (mut project, sources) = tempo_project(TempoSync::None);
        let first = ProjectCompiler::new(&ExactStub)
            .compile(&project, &sources)
            .unwrap();
        let first_revision = first.revision();
        let old = first.prepare().unwrap();
        let old_samples = old.root().samples().to_vec();
        project.settings.random_seed = 99;
        let second = ProjectCompiler::new(&ExactStub)
            .compile(&project, &sources)
            .unwrap();
        assert_ne!(first_revision, second.revision());
        assert_eq!(old.root().samples(), old_samples);
    }

    #[test]
    fn every_catalog_default_compiles_and_exports_in_a_fixed_mono_composition() {
        let mut project = project(48_000, 120.0, 1.0);
        project.compositions[0].output_layout = gaw_core::ChannelLayout::Mono;
        for (index, kind) in ProcessorKind::catalog_defaults().into_iter().enumerate() {
            project.compositions[0]
                .output_effects
                .push(gaw_core::Processor::new(
                    ProcessorId::new(format!("processor-{index}")).unwrap(),
                    kind,
                ));
        }
        let compiled = compile_project(&project, &AssetSourceMap::new()).unwrap();
        assert_eq!(compiled.plan().root().processors.len(), 27);
        let snapshot = compiled.snapshot().unwrap();
        assert_eq!(snapshot.layout(), ChannelLayout::Mono);

        let sequence = DERIVED_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gaw-audio-mono-catalog-{}-{sequence}.wav",
            std::process::id()
        ));
        let report = crate::render_wav(
            &snapshot,
            &path,
            crate::OfflineWavSpec {
                frames: Some(32),
                layout: ChannelLayout::Mono,
                ..crate::OfflineWavSpec::default()
            },
        )
        .unwrap();
        assert_eq!(report.frames, 32);
        assert_eq!(report.layout, ChannelLayout::Mono);
        assert_eq!(hound::WavReader::open(&path).unwrap().spec().channels, 1);
        std::fs::remove_file(path).unwrap();
    }

    fn project_with_level_analyzer(seed: u64) -> (Project, AssetSourceMap) {
        let mut project = project(100, 60.0, 1.0);
        project.settings.random_seed = seed;
        let asset_id = add_asset(&mut project, 100, None);
        let root = project.root_composition_id;
        let mut track = Track::audio(root, "analyzed");
        track.clips.push(Clip::Audio(gaw_core::AudioClip::new(
            asset_id,
            beats(0.0),
            beats(1.0),
            SourceRange {
                start: seconds(0.0),
                duration: seconds(1.0),
            },
        )));
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        project.compositions[0]
            .output_effects
            .push(gaw_core::Processor::new(
                ProcessorId::new("stable-meter").unwrap(),
                ProcessorKind::LevelMeter(gaw_core::LevelMeterParameters::default()),
            ));
        (project, decoded(asset_id, vec![0.5; 200]))
    }

    fn configured_analyzer_measurement(
        kind: ProcessorKind,
        sample_rate: u32,
        layout: DspLayout,
        blocks: &[Vec<Vec<f32>>],
    ) -> gaw_core::AnalyzerMeasurement {
        kind.validate().unwrap();
        let mut project = project(sample_rate, 120.0, 1.0);
        let processor = gaw_core::Processor::new(ProcessorId::new("configured").unwrap(), kind);
        project.compositions[0]
            .output_effects
            .push(processor.clone());
        let adapter = DspProcessorAdapter::new(&project, 120.0, 7, 11);
        let mut analyzer = adapter.instance(&processor).unwrap();
        analyzer
            .prepare(PrepareSpec {
                sample_rate: f64::from(sample_rate),
                max_block_size: blocks
                    .iter()
                    .filter_map(|block| block.first().map(Vec::len))
                    .max()
                    .unwrap_or(1),
                input_layout: layout,
                tempo_bpm: 120.0,
            })
            .unwrap();
        let mut absolute_frame = 0_u64;
        for block in blocks {
            assert_eq!(block.len(), layout.channels());
            let inputs: Vec<&[f32]> = block.iter().map(Vec::as_slice).collect();
            let mut output = block
                .iter()
                .map(|channel| vec![f32::NAN; channel.len()])
                .collect::<Vec<_>>();
            let mut outputs: Vec<&mut [f32]> = output.iter_mut().map(Vec::as_mut_slice).collect();
            analyzer
                .process(
                    &inputs,
                    &mut outputs,
                    &[],
                    ProcessContext {
                        absolute_frame,
                        tempo_bpm: 120.0,
                    },
                )
                .unwrap();
            assert_eq!(&output, block, "analyzer must be sample-transparent");
            absolute_frame = absolute_frame.saturating_add(block[0].len() as u64);
        }
        analyzer.analyzer_measurement().unwrap()
    }

    #[test]
    fn level_and_loudness_payloads_change_measurement_configuration() {
        let signal = vec![vec![
            (0..20)
                .map(|frame| if frame == 0 { 1.0 } else { 0.0 })
                .collect(),
        ]];
        let mut short = gaw_core::LevelMeterParameters::default();
        short.window_ms = 5.0;
        short.peak_hold_ms = 0.0;
        short.true_peak = false;
        let mut wide = short.clone();
        wide.window_ms = 20.0;
        let mut held = short.clone();
        held.peak_hold_ms = 1_000.0;
        let gaw_core::AnalyzerMeasurement::LevelMeter(short_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::LevelMeter(short),
                1_000,
                DspLayout::Mono,
                &signal,
            )
        else {
            panic!("level measurement")
        };
        let gaw_core::AnalyzerMeasurement::LevelMeter(wide_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::LevelMeter(wide),
                1_000,
                DspLayout::Mono,
                &signal,
            )
        else {
            panic!("level measurement")
        };
        let gaw_core::AnalyzerMeasurement::LevelMeter(held_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::LevelMeter(held),
                1_000,
                DspLayout::Mono,
                &signal,
            )
        else {
            panic!("level measurement")
        };
        assert_eq!(short_measurement.sample_peak_dbfs, vec![-120.0]);
        assert_eq!(short_measurement.peak_hold_dbfs, vec![-120.0]);
        assert_eq!(short_measurement.true_peak_dbfs, vec![-120.0]);
        assert_eq!(wide_measurement.sample_peak_dbfs, vec![0.0]);
        assert_eq!(held_measurement.sample_peak_dbfs, vec![-120.0]);
        assert_eq!(held_measurement.peak_hold_dbfs, vec![0.0]);

        let true_peak_signal = vec![vec![
            (0..256)
                .map(|frame| (std::f32::consts::TAU * 0.24 * (frame as f32 + 0.5)).sin())
                .collect(),
        ]];
        let mut sample_peak = gaw_core::LevelMeterParameters::default();
        sample_peak.window_ms = 300.0;
        sample_peak.peak_hold_ms = 0.0;
        sample_peak.true_peak = false;
        let mut true_peak = sample_peak.clone();
        true_peak.true_peak = true;
        let gaw_core::AnalyzerMeasurement::LevelMeter(sample_peak_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::LevelMeter(sample_peak),
                1_000,
                DspLayout::Mono,
                &true_peak_signal,
            )
        else {
            panic!("level measurement")
        };
        let gaw_core::AnalyzerMeasurement::LevelMeter(true_peak_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::LevelMeter(true_peak),
                1_000,
                DspLayout::Mono,
                &true_peak_signal,
            )
        else {
            panic!("level measurement")
        };
        assert_eq!(
            sample_peak_measurement.true_peak_dbfs,
            sample_peak_measurement.sample_peak_dbfs
        );
        assert!(
            true_peak_measurement.true_peak_dbfs[0] > sample_peak_measurement.true_peak_dbfs[0]
        );

        let blocks = vec![vec![vec![0.5; 10]], vec![vec![0.0; 10]]];
        let mut short = gaw_core::LoudnessMeterParameters::default();
        short.integration_seconds = 0.1;
        short.absolute_gate_lufs = -100.0;
        let mut long = short.clone();
        long.integration_seconds = 1.0;
        let mut gated = long.clone();
        gated.absolute_gate_lufs = -3.0;
        let gaw_core::AnalyzerMeasurement::LoudnessMeter(short_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::LoudnessMeter(short),
                100,
                DspLayout::Mono,
                &blocks,
            )
        else {
            panic!("loudness measurement")
        };
        let gaw_core::AnalyzerMeasurement::LoudnessMeter(long_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::LoudnessMeter(long),
                100,
                DspLayout::Mono,
                &blocks,
            )
        else {
            panic!("loudness measurement")
        };
        let gaw_core::AnalyzerMeasurement::LoudnessMeter(gated_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::LoudnessMeter(gated),
                100,
                DspLayout::Mono,
                &blocks,
            )
        else {
            panic!("loudness measurement")
        };
        assert_eq!(short_measurement.integrated_lufs, -120.0);
        assert!(long_measurement.integrated_lufs > -20.0);
        assert_eq!(gated_measurement.integrated_lufs, -120.0);
    }

    #[test]
    fn spectrum_and_oscilloscope_payloads_change_measurement_configuration() {
        let tone = vec![vec![
            (0..512)
                .map(|frame| (std::f32::consts::TAU * 1_000.0 * frame as f32 / 8_000.0).sin())
                .collect(),
        ]];
        let mut narrow = gaw_core::SpectrumParameters::default();
        narrow.fft_size = gaw_core::FftSize::N256;
        narrow.window = gaw_core::WindowFunction::Hann;
        narrow.smoothing = 0.0;
        narrow.minimum_hz = 900.0;
        narrow.maximum_hz = 1_100.0;
        let mut wide = narrow.clone();
        wide.fft_size = gaw_core::FftSize::N512;
        wide.minimum_hz = 20.0;
        wide.maximum_hz = 3_000.0;
        let mut flat_top = narrow.clone();
        flat_top.window = gaw_core::WindowFunction::FlatTop;
        let mut smoothed = narrow.clone();
        smoothed.smoothing = 0.75;
        let gaw_core::AnalyzerMeasurement::Spectrum(narrow_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::Spectrum(narrow),
                8_000,
                DspLayout::Mono,
                &tone,
            )
        else {
            panic!("spectrum measurement")
        };
        let gaw_core::AnalyzerMeasurement::Spectrum(flat_top_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::Spectrum(flat_top),
                8_000,
                DspLayout::Mono,
                &tone,
            )
        else {
            panic!("spectrum measurement")
        };
        let gaw_core::AnalyzerMeasurement::Spectrum(smoothed_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::Spectrum(smoothed),
                8_000,
                DspLayout::Mono,
                &tone,
            )
        else {
            panic!("spectrum measurement")
        };
        let gaw_core::AnalyzerMeasurement::Spectrum(wide_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::Spectrum(wide),
                8_000,
                DspLayout::Mono,
                &tone,
            )
        else {
            panic!("spectrum measurement")
        };
        assert!(
            narrow_measurement
                .bins
                .iter()
                .all(|bin| (900.0..=1_100.0).contains(&bin.frequency_hz))
        );
        assert!(wide_measurement.bins.len() > narrow_measurement.bins.len());
        assert_ne!(
            flat_top_measurement.peaks[0].magnitude_dbfs,
            narrow_measurement.peaks[0].magnitude_dbfs
        );
        assert!(
            smoothed_measurement.peaks[0].magnitude_dbfs
                < narrow_measurement.peaks[0].magnitude_dbfs
        );

        let waveform = vec![vec![vec![
            -1.0, -0.5, 0.5, 1.0, 0.5, -0.5, -1.0, 0.5, -1.0, -0.5, 0.5, 1.0, 0.5, -0.5, -1.0, 0.5,
            -1.0, -0.5, 0.5, 1.0,
        ]]];
        let mut free = gaw_core::OscilloscopeParameters::default();
        free.window_ms = 8.0;
        free.trigger = gaw_core::OscilloscopeTrigger::Free;
        let mut triggered = free.clone();
        triggered.window_ms = 20.0;
        triggered.trigger = gaw_core::OscilloscopeTrigger::RisingZero;
        let mut long_free = triggered.clone();
        long_free.trigger = gaw_core::OscilloscopeTrigger::Free;
        let gaw_core::AnalyzerMeasurement::Oscilloscope(free_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::Oscilloscope(free),
                1_000,
                DspLayout::Mono,
                &waveform,
            )
        else {
            panic!("oscilloscope measurement")
        };
        let gaw_core::AnalyzerMeasurement::Oscilloscope(triggered_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::Oscilloscope(triggered),
                1_000,
                DspLayout::Mono,
                &waveform,
            )
        else {
            panic!("oscilloscope measurement")
        };
        let gaw_core::AnalyzerMeasurement::Oscilloscope(long_free_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::Oscilloscope(long_free),
                1_000,
                DspLayout::Mono,
                &waveform,
            )
        else {
            panic!("oscilloscope measurement")
        };
        assert_eq!(long_free_measurement.channel_samples[0][0], -1.0);
        assert_eq!(triggered_measurement.channel_samples[0][0], 0.5);
        assert_eq!(free_measurement.channel_samples[0].len(), 8);
        assert_eq!(triggered_measurement.channel_samples[0].len(), 20);
    }

    #[test]
    fn stereo_and_tuner_payloads_change_measurement_configuration() {
        let left = vec![1.0; 20];
        let right = (0..20)
            .map(|frame| if frame < 15 { 1.0 } else { -1.0 })
            .collect();
        let stereo = vec![vec![left, right]];
        let mut short = gaw_core::StereoMeterParameters::default();
        short.window_ms = 5.0;
        let mut long = short.clone();
        long.window_ms = 20.0;
        let gaw_core::AnalyzerMeasurement::StereoMeter(short_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::StereoMeter(short),
                1_000,
                DspLayout::Stereo,
                &stereo,
            )
        else {
            panic!("stereo measurement")
        };
        let gaw_core::AnalyzerMeasurement::StereoMeter(long_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::StereoMeter(long),
                1_000,
                DspLayout::Stereo,
                &stereo,
            )
        else {
            panic!("stereo measurement")
        };
        assert!(short_measurement.correlation < -0.99);
        assert!(long_measurement.correlation > 0.49 && long_measurement.correlation < 0.51);

        let high_tone = vec![vec![
            (0..8_000)
                .map(|frame| (std::f32::consts::TAU * 1_500.0 * frame as f32 / 8_000.0).sin())
                .collect(),
        ]];
        let mut accepted = gaw_core::TunerParameters::default();
        accepted.minimum_hz = 1_400.0;
        accepted.maximum_hz = 1_600.0;
        accepted.reference_pitch_hz = 440.0;
        let mut retuned = accepted.clone();
        retuned.reference_pitch_hz = 432.0;
        let mut rejected = accepted.clone();
        rejected.minimum_hz = 1_600.0;
        rejected.maximum_hz = 2_000.0;
        let gaw_core::AnalyzerMeasurement::Tuner(accepted_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::Tuner(accepted),
                8_000,
                DspLayout::Mono,
                &high_tone,
            )
        else {
            panic!("tuner measurement")
        };
        let gaw_core::AnalyzerMeasurement::Tuner(retuned_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::Tuner(retuned),
                8_000,
                DspLayout::Mono,
                &high_tone,
            )
        else {
            panic!("tuner measurement")
        };
        let gaw_core::AnalyzerMeasurement::Tuner(rejected_measurement) =
            configured_analyzer_measurement(
                ProcessorKind::Tuner(rejected),
                8_000,
                DspLayout::Mono,
                &high_tone,
            )
        else {
            panic!("tuner measurement")
        };
        assert!(accepted_measurement.fundamental_hz > 1_400.0);
        assert_ne!(
            accepted_measurement.cents_offset,
            retuned_measurement.cents_offset
        );
        assert_eq!(rejected_measurement.fundamental_hz, 0.0);
        assert_eq!(rejected_measurement.note_name, "--");
    }

    #[test]
    fn preparation_automatically_publishes_real_transparent_analyzer_measurements() {
        let (project, sources) = project_with_level_analyzer(1);
        let compiled = compile_project(&project, &sources).unwrap();
        let receiver = compiled.analyzer_channel(4).unwrap();
        let page = compiled.prepare_page(0, 100).unwrap();
        let snapshot = compiled.paged_snapshot([page]).unwrap();
        let mut output = vec![0.0; 200];
        snapshot.render_native(0, &mut output);
        assert_eq!(output, vec![0.5; 200]);

        let publication = receiver.try_recv().expect("automatic analyzer result");
        assert_eq!(publication.processor_id.as_ref(), "stable-meter");
        assert_eq!(publication.render_revision, compiled.revision());
        assert_eq!(publication.range, AnalyzerFrameRange::new(0, 100));
        let gaw_core::AnalyzerMeasurement::LevelMeter(measurement) = publication.measurement else {
            panic!("expected a level-meter measurement");
        };
        for peak in measurement.sample_peak_dbfs {
            assert!((peak + 6.020_600_3).abs() < 0.001);
        }
        for rms in measurement.rms_dbfs {
            assert!((rms + 6.020_600_3).abs() < 0.001);
        }
    }

    #[test]
    fn automatic_analyzer_publication_suppresses_late_old_revision_results() {
        let (old_project, old_sources) = project_with_level_analyzer(1);
        let (new_project, new_sources) = project_with_level_analyzer(2);
        let old = compile_project(&old_project, &old_sources).unwrap();
        let new = compile_project(&new_project, &new_sources).unwrap();
        assert_ne!(old.revision(), new.revision());

        let (publisher, receiver) = analyzer_channel(4, old.revision()).unwrap();
        old.attach_analyzer_publisher(publisher.clone());
        receiver.set_expected_revision(new.revision());
        old.prepare_page(0, 100).unwrap();
        assert!(receiver.try_recv().is_none());

        new.attach_analyzer_publisher(publisher);
        new.prepare_page(0, 100).unwrap();
        let publication = receiver.try_recv().expect("current analyzer result");
        assert_eq!(publication.processor_id.as_ref(), "stable-meter");
        assert_eq!(publication.render_revision, new.revision());
        assert!(receiver.try_recv().is_none());
    }

    #[test]
    fn panned_gain_preserves_true_stereo_in_a_fixed_stereo_composition() {
        let mut project = project(48_000, 120.0, 1.0);
        let mut parameters = gaw_core::GainParameters::default();
        parameters.pan = -0.5;
        let processor = gaw_core::Processor::new(
            ProcessorId::new("stereo-pan").unwrap(),
            ProcessorKind::Gain(parameters),
        );
        project.compositions[0]
            .output_effects
            .push(processor.clone());
        let adapter = DspProcessorAdapter::new(&project, 120.0, 1, 7);
        let spec = adapter.spec(&processor, ChannelLayout::Stereo).unwrap();
        let mut output = [0.0; 4];
        adapter
            .process(
                &spec,
                48_000,
                ChannelLayout::Stereo,
                &[1.0, 0.0, 0.0, 1.0],
                &mut output,
            )
            .unwrap();

        assert!(output[0] > 0.0);
        assert_eq!(output[1], 0.0);
        assert_eq!(output[2], 0.0);
        assert!(output[3] > 0.0);
        assert_ne!(output[0], output[3]);
    }

    #[test]
    fn stereo_tool_mono_downmix_is_preserved_in_a_fixed_stereo_container() {
        let mut project = project(48_000, 120.0, 1.0);
        project.compositions[0].output_layout = gaw_core::ChannelLayout::Stereo;
        let mut parameters = gaw_core::StereoToolParameters::default();
        parameters.output_layout = gaw_core::ChannelLayout::Mono;
        let processor = gaw_core::Processor::new(
            ProcessorId::new("downmix").unwrap(),
            ProcessorKind::StereoTool(parameters),
        );
        project.compositions[0]
            .output_effects
            .push(processor.clone());
        project.validate().unwrap();

        let adapter = DspProcessorAdapter::new(&project, 120.0, 1, 7);
        let spec = adapter.spec(&processor, ChannelLayout::Stereo).unwrap();
        let mut output = [0.0; 4];
        adapter
            .process(
                &spec,
                48_000,
                ChannelLayout::Stereo,
                &[1.0, 0.0, 0.0, 1.0],
                &mut output,
            )
            .unwrap();
        assert_eq!(output[0], output[1]);
        assert_eq!(output[2], output[3]);
        assert!(output.iter().all(|sample| *sample > 0.0));
    }

    #[derive(Debug)]
    struct CountedLongSource {
        frames: u64,
        reads: std::sync::atomic::AtomicUsize,
        maximum_read: std::sync::atomic::AtomicUsize,
    }

    impl FrameSource for CountedLongSource {
        fn frame_count(&self) -> u64 {
            self.frames
        }
        fn channel_layout(&self) -> ChannelLayout {
            ChannelLayout::Stereo
        }
        fn read_interleaved(
            &self,
            start_frame: u64,
            output: &mut [f32],
        ) -> Result<usize, crate::AssetError> {
            let frames = (output.len() / 2).min(
                usize::try_from(self.frames.saturating_sub(start_frame)).unwrap_or(usize::MAX),
            );
            for (offset, frame) in output[..frames * 2].chunks_exact_mut(2).enumerate() {
                let sample = ((start_frame + offset as u64) % 997) as f32 / 997.0;
                frame.fill(sample);
            }
            self.reads
                .fetch_add(frames, std::sync::atomic::Ordering::Relaxed);
            self.maximum_read
                .fetch_max(frames, std::sync::atomic::Ordering::Relaxed);
            Ok(frames)
        }
    }

    #[test]
    fn compiler_keeps_neutral_long_clips_lazy_for_paged_playback() {
        let mut project = project(48_000, 60.0, 600.0);
        let asset_id = add_asset(&mut project, 48_000 * 600, None);
        let root = project.root_composition_id;
        let mut track = Track::audio(root, "long");
        track.clips.push(Clip::Audio(gaw_core::AudioClip::new(
            asset_id,
            beats(0.0),
            beats(600.0),
            SourceRange {
                start: seconds(0.0),
                duration: seconds(600.0),
            },
        )));
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        let source = Arc::new(CountedLongSource {
            frames: 48_000 * 600,
            reads: std::sync::atomic::AtomicUsize::new(0),
            maximum_read: std::sync::atomic::AtomicUsize::new(0),
        });
        let decoded = AssetSourceMap::new().with_source(
            asset_id.to_string(),
            Arc::clone(&source) as Arc<dyn FrameSource>,
        );
        let compiled = compile_project(&project, &decoded).unwrap();
        assert_eq!(source.reads.load(std::sync::atomic::Ordering::Relaxed), 0);
        let page = compiled.prepare_page(48_000 * 300, 4_096).unwrap();
        assert_eq!(page.memory_bytes(), 4_096 * 2 * size_of::<f32>());
        assert_eq!(
            source.reads.load(std::sync::atomic::Ordering::Relaxed),
            4_096
        );
    }

    #[test]
    fn long_processed_asset_materializes_in_bounded_blocks_and_pages_exactly() {
        let sample_rate = 1_000;
        let frames = u64::from(sample_rate) * 600;
        let mut project = project(sample_rate, 60.0, 600.0);
        let imported_id = add_asset(&mut project, frames, None);
        let processed_id = CoreAssetId::new();
        project.assets.push(AudioAsset {
            id: processed_id,
            name: "bounded-processed".into(),
            definition: AudioAssetDefinition::Processed {
                source_asset_id: imported_id,
                transforms: vec![
                    AudioTransform::Trim(SourceRange {
                        start: seconds(0.0),
                        duration: seconds(600.0),
                    }),
                    AudioTransform::Reverse,
                    AudioTransform::FadeIn(Fade {
                        duration: seconds(1.0),
                        curve: FadeCurve::Linear,
                    }),
                    AudioTransform::FadeOut(Fade {
                        duration: seconds(1.0),
                        curve: FadeCurve::Linear,
                    }),
                ],
                effects: vec![gain("bounded-gain", 0.0)],
            },
            tempo: None,
            revisions: Vec::new(),
            current_revision_id: None,
        });
        let root = project.root_composition_id;
        let mut track = Track::audio(root, "processed");
        track.clips.push(Clip::Audio(gaw_core::AudioClip::new(
            processed_id,
            beats(0.0),
            beats(600.0),
            SourceRange {
                start: seconds(0.0),
                duration: seconds(600.0),
            },
        )));
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);

        let source = Arc::new(CountedLongSource {
            frames,
            reads: std::sync::atomic::AtomicUsize::new(0),
            maximum_read: std::sync::atomic::AtomicUsize::new(0),
        });
        let decoded = AssetSourceMap::new().with_source(
            imported_id.to_string(),
            Arc::clone(&source) as Arc<dyn FrameSource>,
        );
        let cache_directory = std::env::temp_dir().join(format!(
            "gaw-audio-bounded-processed-{}-{}",
            std::process::id(),
            SamplerZoneId::new()
        ));
        let compiled = ProjectCompiler::new(&CanonicalTempoStretcher)
            .with_cache_directory(&cache_directory)
            .compile(&project, &decoded)
            .unwrap();
        assert!(source.reads.load(std::sync::atomic::Ordering::Relaxed) >= frames as usize);
        assert!(
            source
                .maximum_read
                .load(std::sync::atomic::Ordering::Relaxed)
                <= PROCESS_BLOCK_FRAMES
        );
        let first_pass_reads = source.reads.load(std::sync::atomic::Ordering::Relaxed);
        ProjectCompiler::new(&CanonicalTempoStretcher)
            .with_cache_directory(&cache_directory)
            .compile(&project, &decoded)
            .unwrap();
        assert_eq!(
            source.reads.load(std::sync::atomic::Ordering::Relaxed),
            first_pass_reads,
            "the deterministic revision key should reuse its valid cached WAV"
        );

        let start = 30_000_u64;
        let page = compiled.prepare_page(start, PROCESS_BLOCK_FRAMES).unwrap();
        assert_eq!(
            page.memory_bytes(),
            PROCESS_BLOCK_FRAMES * 2 * size_of::<f32>()
        );
        let snapshot = compiled.paged_snapshot([page]).unwrap();
        let mut output = vec![0.0; PROCESS_BLOCK_FRAMES * 2];
        snapshot.render_native(start, &mut output);
        for (offset, frame) in output.chunks_exact(2).enumerate() {
            let reversed = frames - 1 - start - offset as u64;
            let expected = (reversed % 997) as f32 / 997.0;
            assert!((frame[0] - expected).abs() < 0.000_01);
            assert!((frame[1] - expected).abs() < 0.000_01);
        }
        std::fs::remove_dir_all(cache_directory).unwrap();
    }

    #[test]
    fn long_wav_compile_and_page_keep_decoding_residency_bounded() {
        let sample_rate = 48_000;
        let frames = u64::from(sample_rate) * 10;
        let path = std::env::temp_dir().join(format!(
            "gaw-audio-long-wav-{}-{}.wav",
            std::process::id(),
            SamplerZoneId::new()
        ));
        let mut writer = hound::WavWriter::create(
            &path,
            hound::WavSpec {
                channels: 2,
                sample_rate,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for frame in 0..frames {
            let sample = (frame % 997) as f32 / 997.0;
            writer.write_sample(sample).unwrap();
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();

        let mut project = project(sample_rate, 60.0, 10.0);
        let asset_id = add_asset(&mut project, frames, None);
        let root = project.root_composition_id;
        let mut track = Track::audio(root, "long-wav");
        track.clips.push(Clip::Audio(gaw_core::AudioClip::new(
            asset_id,
            beats(0.0),
            beats(10.0),
            SourceRange {
                start: seconds(0.0),
                duration: seconds(10.0),
            },
        )));
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);

        let wav: Arc<dyn FrameSource> = Arc::new(WavFrameSource::open(&path).unwrap());
        let paged = Arc::new(PagedFrameSource::new(wav, 4_096, 2).unwrap());
        let decoded = AssetSourceMap::new().with_source(
            asset_id.to_string(),
            Arc::clone(&paged) as Arc<dyn FrameSource>,
        );
        let compiled = compile_project(&project, &decoded).unwrap();
        assert_eq!(paged.residency().resident_frames, 0);
        let page = compiled
            .prepare_page(u64::from(sample_rate) * 5, 4_096)
            .unwrap();
        assert_eq!(page.memory_bytes(), 4_096 * 2 * size_of::<f32>());
        let residency = paged.residency();
        assert!(residency.resident_pages <= 2);
        assert!(residency.resident_frames <= 8_192);
        assert!(residency.resident_frames as u64 * 20 < frames);
        std::fs::remove_file(path).unwrap();
    }
}
