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
    collections::HashMap,
    fmt,
    fs::File,
    io::{Read, Seek},
    path::PathBuf,
    sync::Arc,
};

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
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    AssetSourceMap, AssetSourceResolver, Beat, ChannelLayout, ClipSourceSpec, ClipSpec,
    CompositionSpec, FrameSource, MemoryFrameSource, MixError, PagedFrameSource,
    PagedSnapshotBuilder, PreparedPage, PreparedRenderPlan, ProcessorAdapter, ProcessorSpec,
    RenderPlan, RenderPlanBuilder, RenderSnapshot, Tempo, TrackSpec, WavFrameSource,
    prepare_render_page_for_revision, prepare_render_plan,
};

const PROCESS_BLOCK_FRAMES: usize = 4_096;

/// Replaceable pitch-preserving tempo engine used during project compilation.
pub trait TempoStretcher: fmt::Debug + Send + Sync {
    fn stretch(
        &self,
        input: &[f32],
        layout: ChannelLayout,
        sample_rate: u32,
        output_frames: usize,
    ) -> Result<Vec<f32>, String>;
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
}

impl<'a> ProjectCompiler<'a> {
    pub const fn new(stretcher: &'a dyn TempoStretcher) -> Self {
        Self { stretcher }
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
        let processors =
            DspProcessorAdapter::new(project, project.bpm.value(), project.settings.random_seed);
        let mut assets = HashMap::new();
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
                                    materialize_asset(
                                        project,
                                        audio.asset_id,
                                        decoded,
                                        &processors,
                                        self.stretcher,
                                        &mut assets,
                                        &mut visiting,
                                    )?;
                                    let rendered =
                                        render_audio_clip(project, audio, &assets, self.stretcher)?;
                                    sources.insert(source_id.clone(), memory_source(&rendered)?);
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
            revision: project_revision(project)?,
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
    Ok(compile_project(&project, &decoded)?)
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
    let AudioAssetDefinition::Imported(imported) = &asset.definition else {
        return Ok(None);
    };
    if imported.sample_rate != project.sample_rate {
        return Ok(None);
    }
    let source = decoded
        .resolve(&clip.asset_id.to_string())
        .ok_or_else(|| CompileError::MissingDecodedAsset(clip.asset_id.to_string()))?;
    let expected = layout(imported.layout);
    if source.channel_layout() != expected {
        return Err(CompileError::AssetLayout {
            asset: clip.asset_id.to_string(),
            actual: source.channel_layout(),
            expected,
        });
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

fn render_audio_clip(
    project: &Project,
    clip: &gaw_core::AudioClip,
    assets: &HashMap<String, AudioBuffer>,
    stretcher: &dyn TempoStretcher,
) -> Result<AudioBuffer, CompileError> {
    let source = assets
        .get(&clip.asset_id.to_string())
        .expect("materialized asset");
    let rate = project.sample_rate.value();
    let mut audio = trim(
        source,
        clip.source.start.value(),
        clip.source.duration.value(),
        rate,
    )?;
    if clip.reverse {
        reverse_frames(&mut audio);
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
        audio = match clip.tempo_sync {
            TempoSync::None => audio,
            TempoSync::Repitch => repitch(audio, ratio)?,
            TempoSync::Stretch => stretch_audio(audio, ratio, rate, stretcher)?,
        };
    }
    if let Some(fade) = clip.fade_in {
        apply_fade(&mut audio, fade, true, rate);
    }
    if let Some(fade) = clip.fade_out {
        apply_fade(&mut audio, fade, false, rate);
    }
    Ok(audio)
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

/// Adapter that recreates project-configured gaw-dsp processors during preparation.
#[derive(Debug)]
pub struct DspProcessorAdapter {
    definitions: HashMap<String, gaw_core::Processor>,
    automation: HashMap<String, Vec<gaw_core::AutomationLane>>,
    tempo_bpm: f64,
    project_seed: u64,
    sample_rate: u32,
}

impl DspProcessorAdapter {
    fn new(project: &Project, tempo_bpm: f64, project_seed: u64) -> Self {
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
        }
    }

    fn spec(
        &self,
        processor: &gaw_core::Processor,
        layout: ChannelLayout,
    ) -> Result<ProcessorSpec, CompileError> {
        let mut instance = self.instance(processor)?;
        let input = dsp_layout(layout);
        let output = instance
            .output_layout(input)
            .map_err(|error| self.processor_error(processor, error.to_string()))?;
        if output != input && !matches!(&processor.kind, ProcessorKind::StereoTool(_)) {
            return Err(self.processor_error(processor, format!("changes {input:?} to {output:?}; fixed-layout plan requires an explicit stereo composition")));
        }
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
                    gaw_core::PanLaw::MinusFourPointFiveDb => {
                        return Err(self.processor_error(
                            processor,
                            "-4.5 dB pan law is absent from gaw-dsp".into(),
                        ));
                    }
                };
                Box::new(
                    serde_json::from_value::<gaw_dsp::Gain>(json)
                        .map_err(CompileError::Revision)?,
                )
            }
            ProcessorKind::StereoTool(value) => boxed_from::<_, gaw_dsp::StereoTool>(value)?,
            ProcessorKind::Filter(value) => {
                if value.slope_db_per_octave == gaw_core::FilterSlope::Db36 {
                    return Err(self.processor_error(
                        processor,
                        "36 dB/octave filters are absent from gaw-dsp".into(),
                    ));
                }
                let mut json = serde_json::to_value(value).map_err(CompileError::Revision)?;
                json["slope_db_per_octave"] = Value::from(filter_slope(value.slope_db_per_octave));
                Box::new(
                    serde_json::from_value::<gaw_dsp::Filter>(json)
                        .map_err(CompileError::Revision)?,
                )
            }
            ProcessorKind::ParametricEq(value) => {
                if value.bands.iter().any(|band| {
                    band.shape == gaw_core::EqShape::BandPass
                        || band.slope_db_per_octave == gaw_core::FilterSlope::Db36
                }) {
                    return Err(self.processor_error(
                        processor,
                        "band-pass EQ or 36 dB/octave EQ slopes are absent from gaw-dsp".into(),
                    ));
                }
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
                if value.oversampling == gaw_core::Oversampling::X8 {
                    return Err(self.processor_error(
                        processor,
                        "8x oversampling is absent from gaw-dsp".into(),
                    ));
                }
                Box::new(gaw_dsp::Saturator::new(from_value(value)?))
            }
            ProcessorKind::Clipper(value) => {
                if value.oversampling == gaw_core::Oversampling::X8 {
                    return Err(self.processor_error(
                        processor,
                        "8x oversampling is absent from gaw-dsp".into(),
                    ));
                }
                Box::new(gaw_dsp::Clipper::new(from_value(value)?))
            }
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
            ProcessorKind::Reverb(value) => {
                if value.algorithm == gaw_core::ReverbAlgorithm::ChamberV1 {
                    return Err(
                        self.processor_error(processor, "ChamberV1 is absent from gaw-dsp".into())
                    );
                }
                boxed_from::<_, gaw_dsp::Reverb>(value)?
            }
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
            ProcessorKind::LevelMeter(_) => Box::new(gaw_dsp::AnalyzerTap::level_meter()),
            ProcessorKind::LoudnessMeter(_) => Box::new(gaw_dsp::AnalyzerTap::loudness_meter()),
            ProcessorKind::Spectrum(_) => Box::new(gaw_dsp::AnalyzerTap::spectrum()),
            ProcessorKind::Oscilloscope(_) => Box::new(gaw_dsp::AnalyzerTap::oscilloscope()),
            ProcessorKind::StereoMeter(_) => Box::new(gaw_dsp::AnalyzerTap::stereo_meter()),
            ProcessorKind::Tuner(_) => Box::new(gaw_dsp::AnalyzerTap::tuner()),
        };
        instance.set_enabled(enabled);
        Ok(instance)
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
        gaw_core::FilterSlope::Db36 => 36,
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
    fn every_audio_processor_default_maps_to_a_prepared_dsp_instance() {
        let mut project = project(48_000, 120.0, 1.0);
        let kinds = vec![
            ProcessorKind::Gain(gaw_core::GainParameters::default()),
            ProcessorKind::StereoTool(gaw_core::StereoToolParameters::default()),
            ProcessorKind::Filter(gaw_core::FilterParameters::default()),
            ProcessorKind::ParametricEq(gaw_core::ParametricEqParameters::default()),
            ProcessorKind::Compressor(gaw_core::CompressorParameters::default()),
            ProcessorKind::Limiter(gaw_core::LimiterParameters::default()),
            ProcessorKind::Gate(gaw_core::GateParameters::default()),
            ProcessorKind::Expander(gaw_core::ExpanderParameters::default()),
            ProcessorKind::TransientShaper(gaw_core::TransientShaperParameters::default()),
            ProcessorKind::Saturator(gaw_core::SaturatorParameters::default()),
            ProcessorKind::Clipper(gaw_core::ClipperParameters::default()),
            ProcessorKind::Bitcrusher(gaw_core::BitcrusherParameters::default()),
            ProcessorKind::Delay(gaw_core::DelayParameters::default()),
            ProcessorKind::Reverb(gaw_core::ReverbParameters::default()),
            ProcessorKind::Chorus(gaw_core::ChorusParameters::default()),
            ProcessorKind::Flanger(gaw_core::FlangerParameters::default()),
            ProcessorKind::Phaser(gaw_core::PhaserParameters::default()),
            ProcessorKind::TremoloAutopan(gaw_core::TremoloAutopanParameters::default()),
            ProcessorKind::PitchShift(gaw_core::PitchShiftParameters::default()),
            ProcessorKind::RhythmicGate(gaw_core::RhythmicGateParameters::default()),
            ProcessorKind::BeatRepeat(gaw_core::BeatRepeatParameters::default()),
            ProcessorKind::LevelMeter(gaw_core::LevelMeterParameters::default()),
            ProcessorKind::LoudnessMeter(gaw_core::LoudnessMeterParameters::default()),
            ProcessorKind::Spectrum(gaw_core::SpectrumParameters::default()),
            ProcessorKind::Oscilloscope(gaw_core::OscilloscopeParameters::default()),
            ProcessorKind::StereoMeter(gaw_core::StereoMeterParameters::default()),
            ProcessorKind::Tuner(gaw_core::TunerParameters::default()),
        ];
        for (index, kind) in kinds.into_iter().enumerate() {
            project.compositions[0]
                .output_effects
                .push(gaw_core::Processor::new(
                    ProcessorId::new(format!("processor-{index}")).unwrap(),
                    kind,
                ));
        }
        let compiled = compile_project(&project, &AssetSourceMap::new()).unwrap();
        assert_eq!(compiled.plan().root().processors.len(), 27);
        compiled.prepare().unwrap();
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

        let adapter = DspProcessorAdapter::new(&project, 120.0, 1);
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
    }

    impl FrameSource for CountedLongSource {
        fn frame_count(&self) -> u64 {
            self.frames
        }
        fn channel_layout(&self) -> ChannelLayout {
            ChannelLayout::Stereo
        }
        fn read_interleaved(&self, _: u64, output: &mut [f32]) -> Result<usize, crate::AssetError> {
            let frames = output.len() / 2;
            output.fill(0.25);
            self.reads
                .fetch_add(frames, std::sync::atomic::Ordering::Relaxed);
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
