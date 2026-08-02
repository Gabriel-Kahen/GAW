//! Background compilation of render plans into immutable real-time audio.
//!
//! This module is deliberately split at the real-time boundary. Asset reads,
//! processor preparation, hierarchy traversal, and allocation happen in
//! [`prepare_render_plan`]. [`PreparedComposition::render`] is only a bounded
//! positional copy from immutable memory.

use std::{collections::HashMap, fmt, sync::Arc};

use thiserror::Error;

use crate::{
    assets::{AssetError, AssetId, AssetRegistry, FrameSource},
    io::{RealtimeRender, RenderSnapshot, SampleBlock, SnapshotError},
    render::{ChannelLayout, ProcessorSpec, RenderComposition, RenderPlan, RenderSource},
};

const SOURCE_READ_CHUNK_FRAMES: usize = 4_096;

/// Resolves logical asset IDs on the background/control thread.
pub trait AssetSourceResolver: Send + Sync {
    fn resolve(&self, asset_id: &str) -> Option<Arc<dyn FrameSource>>;
}

/// Simple resolver useful for applications and deterministic tests.
#[derive(Clone, Debug, Default)]
pub struct AssetSourceMap {
    sources: HashMap<Arc<str>, Arc<dyn FrameSource>>,
}

impl AssetSourceMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        asset_id: impl Into<Arc<str>>,
        source: Arc<dyn FrameSource>,
    ) -> Option<Arc<dyn FrameSource>> {
        self.sources.insert(asset_id.into(), source)
    }

    #[must_use]
    pub fn with_source(
        mut self,
        asset_id: impl Into<Arc<str>>,
        source: Arc<dyn FrameSource>,
    ) -> Self {
        self.insert(asset_id, source);
        self
    }
}

impl AssetSourceResolver for AssetSourceMap {
    fn resolve(&self, asset_id: &str) -> Option<Arc<dyn FrameSource>> {
        self.sources.get(asset_id).cloned()
    }
}

impl AssetSourceResolver for AssetRegistry {
    fn resolve(&self, asset_id: &str) -> Option<Arc<dyn FrameSource>> {
        AssetRegistry::resolve(self, &AssetId::from(asset_id))
            .map(|resolved| Arc::clone(resolved.revision.source()))
    }
}

/// Background adapter for the currently available DSP implementation.
///
/// `input` and `output` have identical, frame-aligned lengths. Implementations
/// overwrite all of `output`. The supplied buffer includes the plan's bounded
/// tail, so a processor may emit decay after its input becomes silent.
pub trait ProcessorAdapter: fmt::Debug + Send + Sync {
    /// # Errors
    ///
    /// Returns a diagnostic when the processor cannot be prepared or evaluated.
    fn process(
        &self,
        processor: &ProcessorSpec,
        sample_rate: u32,
        layout: ChannelLayout,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<(), String>;
}

/// Metadata-faithful fallback used until a processor has a concrete DSP adapter.
///
/// It preserves samples, delays them by the processor's declared latency, and
/// leaves the declared tail as silence. Disabled processors are copied exactly.
#[derive(Clone, Copy, Debug, Default)]
pub struct PassthroughProcessorAdapter;

impl ProcessorAdapter for PassthroughProcessorAdapter {
    fn process(
        &self,
        processor: &ProcessorSpec,
        _sample_rate: u32,
        layout: ChannelLayout,
        input: &[f32],
        output: &mut [f32],
    ) -> Result<(), String> {
        output.fill(0.0);
        let latency = if processor.enabled {
            usize::try_from(processor.latency_frames).unwrap_or(usize::MAX)
        } else {
            0
        };
        let shift = latency.saturating_mul(layout.channels()).min(input.len());
        let copied = input.len().saturating_sub(shift).min(output.len() - shift);
        output[shift..shift + copied].copy_from_slice(&input[..copied]);
        Ok(())
    }
}

/// Fully materialized composition audio.
#[derive(Clone, Debug)]
pub struct PreparedComposition {
    id: Arc<str>,
    layout: ChannelLayout,
    main_frames: u64,
    tail_frames: u64,
    samples: Arc<[f32]>,
}

impl PreparedComposition {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub const fn layout(&self) -> ChannelLayout {
        self.layout
    }

    pub const fn main_frames(&self) -> u64 {
        self.main_frames
    }

    pub const fn tail_frames(&self) -> u64 {
        self.tail_frames
    }

    pub const fn total_frames(&self) -> u64 {
        self.main_frames + self.tail_frames
    }

    pub fn samples(&self) -> &[f32] {
        &self.samples
    }
}

impl RealtimeRender for PreparedComposition {
    fn render(&self, start_frame: u64, output: &mut SampleBlock<'_>) {
        output.clear();
        if output.layout() != self.layout {
            return;
        }
        let channels = self.layout.channels();
        let Ok(start_frame) = usize::try_from(start_frame) else {
            return;
        };
        let Some(start_sample) = start_frame.checked_mul(channels) else {
            return;
        };
        if start_sample >= self.samples.len() {
            return;
        }
        let count = output
            .samples()
            .len()
            .min(self.samples.len() - start_sample);
        output.samples_mut()[..count]
            .copy_from_slice(&self.samples[start_sample..start_sample + count]);
    }
}

/// All reachable compositions in the render plan, retaining child-first order.
#[derive(Clone, Debug)]
pub struct PreparedRenderPlan {
    sample_rate: u32,
    compositions: Arc<[Arc<PreparedComposition>]>,
    root_index: usize,
}

impl PreparedRenderPlan {
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn compositions(&self) -> &[Arc<PreparedComposition>] {
        &self.compositions
    }

    pub fn root(&self) -> &Arc<PreparedComposition> {
        &self.compositions[self.root_index]
    }

    /// Creates a lightweight immutable snapshot sharing the prepared samples.
    ///
    /// # Errors
    ///
    /// Returns [`MixError::Snapshot`] if the prepared snapshot metadata is invalid.
    pub fn snapshot(&self, revision: u64) -> Result<RenderSnapshot, MixError> {
        let root = Arc::clone(self.root());
        Ok(RenderSnapshot::new(
            revision,
            self.sample_rate,
            root.layout,
            root.main_frames,
            root.tail_frames,
            root,
        )?)
    }
}

/// Errors encountered while preparing a render plan off the audio thread.
#[derive(Debug, Error)]
pub enum MixError {
    #[error("asset `{0}` is not available")]
    MissingAsset(String),
    #[error(
        "cannot implicitly mix stereo source `{source_id}` into mono composition `{composition}`"
    )]
    ImplicitStereoToMono {
        source_id: String,
        composition: String,
    },
    #[error("render plan references child composition index {child_index} before it is prepared")]
    ChildNotPrepared { child_index: usize },
    #[error("sample storage is too large for composition `{0}`")]
    SampleCountOverflow(String),
    #[error("asset `{asset}` returned {actual} frames for a {requested}-frame read")]
    SourceOverrun {
        asset: String,
        requested: usize,
        actual: usize,
    },
    #[error("processor `{processor}` failed: {message}")]
    Processor { processor: String, message: String },
    #[error(transparent)]
    Asset(#[from] AssetError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotError),
}

/// Materializes each composition in child-first topological order.
///
/// This function may allocate and invoke arbitrary asset/processor adapters and
/// must therefore run outside the real-time callback.
///
/// # Errors
///
/// Returns [`MixError`] when an asset is unavailable, a layout conversion is
/// invalid, an adapter fails, or the requested sample storage cannot be represented.
pub fn prepare_render_plan(
    plan: &RenderPlan,
    assets: &dyn AssetSourceResolver,
    processors: &dyn ProcessorAdapter,
) -> Result<PreparedRenderPlan, MixError> {
    let mut prepared = Vec::with_capacity(plan.compositions.len());
    for composition in plan.compositions.iter() {
        let rendered = prepare_composition(
            composition,
            plan.tempo.sample_rate(),
            assets,
            processors,
            &prepared,
        )?;
        prepared.push(Arc::new(rendered));
    }
    if plan.root_index >= prepared.len() {
        return Err(MixError::ChildNotPrepared {
            child_index: plan.root_index,
        });
    }
    Ok(PreparedRenderPlan {
        sample_rate: plan.tempo.sample_rate(),
        compositions: prepared.into(),
        root_index: plan.root_index,
    })
}

/// Prepares a plan and wraps its root in a real-time render snapshot.
///
/// # Errors
///
/// Returns any error from plan preparation or snapshot construction.
pub fn prepare_snapshot(
    revision: u64,
    plan: &RenderPlan,
    assets: &dyn AssetSourceResolver,
    processors: &dyn ProcessorAdapter,
) -> Result<RenderSnapshot, MixError> {
    prepare_render_plan(plan, assets, processors)?.snapshot(revision)
}

fn prepare_composition(
    composition: &RenderComposition,
    sample_rate: u32,
    assets: &dyn AssetSourceResolver,
    processors: &dyn ProcessorAdapter,
    children: &[Arc<PreparedComposition>],
) -> Result<PreparedComposition, MixError> {
    let published_samples_len = sample_count(
        composition.length_frames,
        composition.tail_frames,
        composition.output_layout,
        &composition.id,
    )?;
    let working_tail = composition
        .tail_frames
        .checked_add(composition.latency_frames)
        .ok_or_else(|| MixError::SampleCountOverflow(composition.id.to_string()))?;
    let working_samples_len = sample_count(
        composition.length_frames,
        working_tail,
        composition.output_layout,
        &composition.id,
    )?;
    let mut composition_mix = vec![0.0; working_samples_len];
    for track in composition.tracks.iter() {
        let mut track_mix = vec![0.0; working_samples_len];
        for clip in track.clips.iter().filter(|clip| !clip.muted) {
            let mut clip_audio = vec![0.0; working_samples_len];
            fill_clip_source(&mut clip_audio, composition, clip, assets, children)?;
            apply_processors(
                &mut clip_audio,
                &clip.processors,
                sample_rate,
                composition.output_layout,
                processors,
            )?;
            delay_in_place(
                &mut clip_audio,
                clip.latency_compensation_frames,
                composition.output_layout,
            );
            mix(&mut track_mix, &clip_audio, clip.gain);
        }
        apply_processors(
            &mut track_mix,
            &track.processors,
            sample_rate,
            composition.output_layout,
            processors,
        )?;
        delay_in_place(
            &mut track_mix,
            track.latency_compensation_frames,
            composition.output_layout,
        );
        mix(&mut composition_mix, &track_mix, 1.0);
    }
    apply_processors(
        &mut composition_mix,
        &composition.processors,
        sample_rate,
        composition.output_layout,
        processors,
    )?;
    crop_latency(
        &mut composition_mix,
        composition.latency_frames,
        composition.output_layout,
        published_samples_len,
    );
    Ok(PreparedComposition {
        id: Arc::clone(&composition.id),
        layout: composition.output_layout,
        main_frames: composition.length_frames,
        tail_frames: composition.tail_frames,
        samples: composition_mix.into(),
    })
}

fn sample_count(
    main_frames: u64,
    tail_frames: u64,
    layout: ChannelLayout,
    id: &str,
) -> Result<usize, MixError> {
    let frames = main_frames
        .checked_add(tail_frames)
        .ok_or_else(|| MixError::SampleCountOverflow(id.to_owned()))?;
    usize::try_from(frames)
        .ok()
        .and_then(|frames| frames.checked_mul(layout.channels()))
        .ok_or_else(|| MixError::SampleCountOverflow(id.to_owned()))
}

fn fill_clip_source(
    output: &mut [f32],
    composition: &RenderComposition,
    clip: &crate::render::RenderClip,
    assets: &dyn AssetSourceResolver,
    children: &[Arc<PreparedComposition>],
) -> Result<(), MixError> {
    let scheduled_frames = clip.end_frame.saturating_sub(clip.start_frame);
    match &clip.source {
        RenderSource::Audio { asset_id } => {
            let source = assets
                .resolve(asset_id)
                .ok_or_else(|| MixError::MissingAsset(asset_id.to_string()))?;
            copy_frame_source(
                output,
                composition,
                clip.start_frame,
                scheduled_frames,
                clip.source_offset_frames,
                asset_id,
                source.as_ref(),
            )
        }
        RenderSource::Composition {
            composition_index,
            logical_id,
        } => {
            let child = children
                .get(*composition_index)
                .ok_or(MixError::ChildNotPrepared {
                    child_index: *composition_index,
                })?;
            ensure_layout(
                child.layout,
                composition.output_layout,
                logical_id,
                &composition.id,
            )?;
            let source_end = clip.source_offset_frames.saturating_add(scheduled_frames);
            let child_total = child.main_frames.saturating_add(child.tail_frames);
            let remaining_tail = if source_end >= child.main_frames {
                child_total.saturating_sub(source_end)
            } else {
                0
            };
            let wanted = scheduled_frames.saturating_add(remaining_tail);
            copy_memory_source(
                output,
                composition.output_layout,
                clip.start_frame,
                wanted,
                clip.source_offset_frames,
                child.layout,
                &child.samples,
            );
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn copy_frame_source(
    output: &mut [f32],
    composition: &RenderComposition,
    output_start: u64,
    wanted_frames: u64,
    source_start: u64,
    asset_id: &str,
    source: &dyn FrameSource,
) -> Result<(), MixError> {
    ensure_layout(
        source.channel_layout(),
        composition.output_layout,
        asset_id,
        &composition.id,
    )?;
    let available = source.frame_count().saturating_sub(source_start);
    let output_available = u64::try_from(output.len() / composition.output_layout.channels())
        .unwrap_or(u64::MAX)
        .saturating_sub(output_start);
    let frame_count = wanted_frames.min(available).min(output_available);
    let source_channels = source.channel_layout().channels();
    let mut position = 0_u64;
    let mut scratch = vec![0.0; SOURCE_READ_CHUNK_FRAMES * source_channels];
    while position < frame_count {
        let request = usize::try_from(frame_count - position)
            .unwrap_or(usize::MAX)
            .min(SOURCE_READ_CHUNK_FRAMES);
        let sample_count = request * source_channels;
        let read = source.read_interleaved(
            source_start.saturating_add(position),
            &mut scratch[..sample_count],
        )?;
        if read > request {
            return Err(MixError::SourceOverrun {
                asset: asset_id.to_owned(),
                requested: request,
                actual: read,
            });
        }
        if read == 0 {
            return Err(AssetError::SourceEndedEarly {
                frame: source_start.saturating_add(position),
            }
            .into());
        }
        copy_memory_source(
            output,
            composition.output_layout,
            output_start.saturating_add(position),
            u64::try_from(read).unwrap_or(u64::MAX),
            0,
            source.channel_layout(),
            &scratch[..read * source_channels],
        );
        position = position.saturating_add(u64::try_from(read).unwrap_or(u64::MAX));
    }
    Ok(())
}

fn ensure_layout(
    source: ChannelLayout,
    destination: ChannelLayout,
    source_id: &str,
    composition_id: &str,
) -> Result<(), MixError> {
    if source == ChannelLayout::Stereo && destination == ChannelLayout::Mono {
        return Err(MixError::ImplicitStereoToMono {
            source_id: source_id.to_owned(),
            composition: composition_id.to_owned(),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn copy_memory_source(
    output: &mut [f32],
    output_layout: ChannelLayout,
    output_start: u64,
    wanted_frames: u64,
    source_start: u64,
    source_layout: ChannelLayout,
    source: &[f32],
) {
    let output_channels = output_layout.channels();
    let source_channels = source_layout.channels();
    let Ok(output_start) = usize::try_from(output_start) else {
        return;
    };
    let Ok(source_start) = usize::try_from(source_start) else {
        return;
    };
    let output_frames = output.len() / output_channels;
    let source_frames = source.len() / source_channels;
    if output_start >= output_frames || source_start >= source_frames {
        return;
    }
    let wanted = usize::try_from(wanted_frames).unwrap_or(usize::MAX);
    let frames = wanted
        .min(output_frames - output_start)
        .min(source_frames - source_start);
    for frame in 0..frames {
        let source_index = (source_start + frame) * source_channels;
        let output_index = (output_start + frame) * output_channels;
        match (source_layout, output_layout) {
            (ChannelLayout::Mono, ChannelLayout::Mono) => {
                output[output_index] = source[source_index];
            }
            (ChannelLayout::Mono, ChannelLayout::Stereo) => {
                output[output_index] = source[source_index];
                output[output_index + 1] = source[source_index];
            }
            (ChannelLayout::Stereo, ChannelLayout::Stereo) => {
                output[output_index..output_index + 2]
                    .copy_from_slice(&source[source_index..source_index + 2]);
            }
            (ChannelLayout::Stereo, ChannelLayout::Mono) => unreachable!("validated above"),
        }
    }
}

fn apply_processors(
    audio: &mut Vec<f32>,
    specs: &[ProcessorSpec],
    sample_rate: u32,
    layout: ChannelLayout,
    adapter: &dyn ProcessorAdapter,
) -> Result<(), MixError> {
    if specs.iter().all(|processor| !processor.enabled) {
        return Ok(());
    }
    let mut scratch = vec![0.0; audio.len()];
    for processor in specs.iter().filter(|processor| processor.enabled) {
        adapter
            .process(processor, sample_rate, layout, audio, &mut scratch)
            .map_err(|message| MixError::Processor {
                processor: processor.id.clone(),
                message,
            })?;
        std::mem::swap(audio, &mut scratch);
    }
    Ok(())
}

fn delay_in_place(audio: &mut [f32], delay_frames: u64, layout: ChannelLayout) {
    let delay = usize::try_from(delay_frames)
        .unwrap_or(usize::MAX)
        .saturating_mul(layout.channels())
        .min(audio.len());
    if delay == 0 {
        return;
    }
    audio.copy_within(..audio.len() - delay, delay);
    audio[..delay].fill(0.0);
}

fn crop_latency(
    audio: &mut Vec<f32>,
    latency_frames: u64,
    layout: ChannelLayout,
    published_samples_len: usize,
) {
    let latency_samples = usize::try_from(latency_frames)
        .unwrap_or(usize::MAX)
        .saturating_mul(layout.channels())
        .min(audio.len());
    let available = audio.len().saturating_sub(latency_samples);
    let copied = published_samples_len.min(available);
    audio.copy_within(latency_samples..latency_samples + copied, 0);
    if copied < published_samples_len {
        audio[copied..published_samples_len].fill(0.0);
    }
    audio.truncate(published_samples_len);
}

fn mix(output: &mut [f32], input: &[f32], gain: f32) {
    for (output, input) in output.iter_mut().zip(input) {
        *output += *input * gain;
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::{
        assets::MemoryFrameSource,
        render::{ClipSourceSpec, ClipSpec, CompositionSpec, RenderPlanBuilder, TrackSpec},
        timeline::{Beat, Tempo},
    };

    fn beat(value: f64) -> Beat {
        Beat::new(value).unwrap()
    }

    fn source(layout: ChannelLayout, samples: &[f32]) -> Arc<dyn FrameSource> {
        Arc::new(MemoryFrameSource::new(layout, Arc::<[f32]>::from(samples)).unwrap())
    }

    fn plan(compositions: Vec<CompositionSpec>, root: &str) -> RenderPlan {
        let mut builder = RenderPlanBuilder::new(Tempo::new(60.0, 4).unwrap(), 16);
        for composition in compositions {
            builder.add_composition(composition);
        }
        builder.build(root).unwrap()
    }

    #[test]
    fn schedules_offsets_gain_overlap_and_mute() {
        let mut root = CompositionSpec::new("root", beat(2.0), ChannelLayout::Mono);
        let mut track = TrackSpec::new("track");
        let mut first = ClipSpec::new(
            "first",
            beat(0.5),
            beat(1.0),
            ClipSourceSpec::audio("source", 1),
        );
        first.gain = 0.5;
        track.clips.push(first);
        track.clips.push(ClipSpec::new(
            "overlap",
            beat(1.0),
            beat(0.5),
            ClipSourceSpec::audio("source", 0),
        ));
        let mut muted = ClipSpec::new(
            "muted",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("missing", 0),
        );
        muted.muted = true;
        track.clips.push(muted);
        root.tracks.push(track);
        let assets = AssetSourceMap::new().with_source(
            "source",
            source(ChannelLayout::Mono, &[1.0, 2.0, 3.0, 4.0, 5.0]),
        );
        let prepared = prepare_render_plan(
            &plan(vec![root], "root"),
            &assets,
            &PassthroughProcessorAdapter,
        )
        .unwrap();
        assert_eq!(
            prepared.root().samples(),
            &[0.0, 0.0, 1.0, 1.5, 3.0, 4.5, 0.0, 0.0]
        );
    }

    #[test]
    fn upmixes_mono_and_rejects_implicit_downmix() {
        let mut stereo = CompositionSpec::new("stereo", beat(1.0), ChannelLayout::Stereo);
        let mut track = TrackSpec::new("track");
        track.clips.push(ClipSpec::new(
            "clip",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("mono", 0),
        ));
        stereo.tracks.push(track);
        let assets = AssetSourceMap::new()
            .with_source("mono", source(ChannelLayout::Mono, &[1.0, 2.0, 3.0, 4.0]));
        let rendered = prepare_render_plan(
            &plan(vec![stereo], "stereo"),
            &assets,
            &PassthroughProcessorAdapter,
        )
        .unwrap();
        assert_eq!(
            rendered.root().samples(),
            &[1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0]
        );

        let mut mono = CompositionSpec::new("mono", beat(1.0), ChannelLayout::Mono);
        let mut track = TrackSpec::new("mono-track");
        track.clips.push(ClipSpec::new(
            "stereo-clip",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("stereo-source", 0),
        ));
        mono.tracks.push(track);
        let assets = AssetSourceMap::new()
            .with_source("stereo-source", source(ChannelLayout::Stereo, &[1.0, 2.0]));
        assert!(matches!(
            prepare_render_plan(
                &plan(vec![mono], "mono"),
                &assets,
                &PassthroughProcessorAdapter
            ),
            Err(MixError::ImplicitStereoToMono { .. })
        ));
    }

    #[test]
    fn renders_child_before_parent_and_places_its_tail() {
        let mut child = CompositionSpec::new("child", beat(1.0), ChannelLayout::Mono);
        let mut child_track = TrackSpec::new("child-track");
        child_track.clips.push(ClipSpec::new(
            "child-audio",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("pulse", 0),
        ));
        child.tracks.push(child_track);
        child
            .processors
            .push(ProcessorSpec::new("child-tail", 0, 2));

        let mut parent = CompositionSpec::new("parent", beat(2.0), ChannelLayout::Stereo);
        let mut parent_track = TrackSpec::new("parent-track");
        parent_track.clips.push(ClipSpec::new(
            "nested",
            beat(0.5),
            beat(1.0),
            ClipSourceSpec::composition("child", 0),
        ));
        parent.tracks.push(parent_track);
        let assets = AssetSourceMap::new()
            .with_source("pulse", source(ChannelLayout::Mono, &[1.0, 2.0, 3.0, 4.0]));
        let rendered = prepare_render_plan(
            &plan(vec![parent, child], "parent"),
            &assets,
            &PassthroughProcessorAdapter,
        )
        .unwrap();
        assert_eq!(rendered.compositions()[0].id(), "child");
        assert_eq!(rendered.root().tail_frames(), 0);
        assert_eq!(
            &rendered.root().samples()[4..12],
            &[1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 4.0, 4.0]
        );
    }

    #[test]
    fn trimming_a_child_before_its_body_end_does_not_leak_body_or_tail() {
        let mut child = CompositionSpec::new("child", beat(2.0), ChannelLayout::Mono);
        child.processors.push(ProcessorSpec::new("echo", 0, 1));
        let mut child_track = TrackSpec::new("child-track");
        child_track.clips.push(ClipSpec::new(
            "child-audio",
            beat(0.0),
            beat(2.0),
            ClipSourceSpec::audio("source", 0),
        ));
        child.tracks.push(child_track);

        let mut parent = CompositionSpec::new("parent", beat(1.0), ChannelLayout::Mono);
        let mut parent_track = TrackSpec::new("parent-track");
        parent_track.clips.push(ClipSpec::new(
            "trimmed-child",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::composition("child", 0),
        ));
        parent.tracks.push(parent_track);
        let assets = AssetSourceMap::new().with_source(
            "source",
            source(
                ChannelLayout::Mono,
                &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0],
            ),
        );
        let rendered = prepare_render_plan(
            &plan(vec![parent, child], "parent"),
            &assets,
            &PassthroughProcessorAdapter,
        )
        .unwrap();
        assert_eq!(rendered.root().tail_frames(), 0);
        assert_eq!(rendered.root().samples(), &[1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn applies_latency_compensation_and_keeps_bounded_silent_tail() {
        let mut root = CompositionSpec::new("root", beat(1.0), ChannelLayout::Mono);
        root.processors.push(ProcessorSpec::new("master", 1, 2));
        let mut fast = TrackSpec::new("fast");
        let mut fast_clip = ClipSpec::new(
            "fast-clip",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("fast", 0),
        );
        fast_clip
            .processors
            .push(ProcessorSpec::new("fast-fx", 1, 0));
        fast.clips.push(fast_clip);
        let mut slow = TrackSpec::new("slow");
        let mut slow_clip = ClipSpec::new(
            "slow-clip",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("slow", 0),
        );
        slow_clip
            .processors
            .push(ProcessorSpec::new("slow-fx", 3, 0));
        slow.clips.push(slow_clip);
        root.tracks.extend([fast, slow]);
        let assets = AssetSourceMap::new()
            .with_source("fast", source(ChannelLayout::Mono, &[1.0, 0.0, 0.0, 0.0]))
            .with_source("slow", source(ChannelLayout::Mono, &[1.0, 0.0, 0.0, 0.0]));
        let rendered = prepare_render_plan(
            &plan(vec![root], "root"),
            &assets,
            &PassthroughProcessorAdapter,
        )
        .unwrap();
        assert_eq!(rendered.root().tail_frames(), 2);
        assert_eq!(rendered.root().samples(), &[2.0, 0.0, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn latency_crop_preserves_an_impulse_on_the_last_body_frame() {
        let mut root = CompositionSpec::new("root", beat(1.0), ChannelLayout::Mono);
        root.processors.push(ProcessorSpec::new("lookahead", 3, 0));
        let mut track = TrackSpec::new("track");
        track.clips.push(ClipSpec::new(
            "clip",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("impulse", 0),
        ));
        root.tracks.push(track);
        let assets = AssetSourceMap::new().with_source(
            "impulse",
            source(ChannelLayout::Mono, &[0.0, 0.0, 0.0, 1.0]),
        );
        let rendered = prepare_render_plan(
            &plan(vec![root], "root"),
            &assets,
            &PassthroughProcessorAdapter,
        )
        .unwrap();
        assert_eq!(rendered.root().samples(), &[0.0, 0.0, 0.0, 1.0]);
    }

    #[derive(Debug)]
    struct EchoAdapter;

    impl ProcessorAdapter for EchoAdapter {
        fn process(
            &self,
            _processor: &ProcessorSpec,
            _sample_rate: u32,
            _layout: ChannelLayout,
            input: &[f32],
            output: &mut [f32],
        ) -> Result<(), String> {
            output.copy_from_slice(input);
            for index in 1..output.len() {
                output[index] += input[index - 1] * 0.5;
            }
            Ok(())
        }
    }

    #[test]
    fn processor_adapter_can_generate_audio_in_declared_tail() {
        let mut root = CompositionSpec::new("root", beat(1.0), ChannelLayout::Mono);
        root.processors.push(ProcessorSpec::new("echo", 0, 1));
        let mut track = TrackSpec::new("track");
        track.clips.push(ClipSpec::new(
            "clip",
            beat(0.75),
            beat(0.25),
            ClipSourceSpec::audio("hit", 0),
        ));
        root.tracks.push(track);
        let assets = AssetSourceMap::new().with_source("hit", source(ChannelLayout::Mono, &[1.0]));
        let rendered =
            prepare_render_plan(&plan(vec![root], "root"), &assets, &EchoAdapter).unwrap();
        assert_eq!(rendered.root().samples(), &[0.0, 0.0, 0.0, 1.0, 0.5]);
    }

    #[test]
    fn prepared_renderer_is_positional_and_snapshot_shares_it() {
        let mut root = CompositionSpec::new("root", beat(1.0), ChannelLayout::Mono);
        let mut track = TrackSpec::new("track");
        track.clips.push(ClipSpec::new(
            "clip",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("audio", 0),
        ));
        root.tracks.push(track);
        let assets = AssetSourceMap::new()
            .with_source("audio", source(ChannelLayout::Mono, &[1.0, 2.0, 3.0, 4.0]));
        let prepared = prepare_render_plan(
            &plan(vec![root], "root"),
            &assets,
            &PassthroughProcessorAdapter,
        )
        .unwrap();
        let mut samples = [9.0; 4];
        let mut block = SampleBlock::new(&mut samples, ChannelLayout::Mono).unwrap();
        prepared.root().render(2, &mut block);
        assert_eq!(samples, [3.0, 4.0, 0.0, 0.0]);
        let snapshot = prepared.snapshot(42).unwrap();
        assert_eq!(snapshot.revision(), 42);
        assert_eq!(snapshot.main_frames(), 4);
    }

    #[derive(Debug)]
    struct PrematureSource;

    impl FrameSource for PrematureSource {
        fn frame_count(&self) -> u64 {
            4
        }

        fn channel_layout(&self) -> ChannelLayout {
            ChannelLayout::Mono
        }

        fn read_interleaved(
            &self,
            _start_frame: u64,
            _output: &mut [f32],
        ) -> Result<usize, AssetError> {
            Ok(0)
        }
    }

    #[test]
    fn rejects_a_source_that_ends_before_its_declared_length() {
        let mut root = CompositionSpec::new("root", beat(1.0), ChannelLayout::Mono);
        let mut track = TrackSpec::new("track");
        track.clips.push(ClipSpec::new(
            "clip",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("broken", 0),
        ));
        root.tracks.push(track);
        let assets = AssetSourceMap::new().with_source("broken", Arc::new(PrematureSource));
        assert!(matches!(
            prepare_render_plan(
                &plan(vec![root], "root"),
                &assets,
                &PassthroughProcessorAdapter
            ),
            Err(MixError::Asset(AssetError::SourceEndedEarly { frame: 0 }))
        ));
    }
}
