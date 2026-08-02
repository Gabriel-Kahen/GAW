//! Immutable, prevalidated render-plan types.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use thiserror::Error;

use crate::timeline::{Beat, FrameRounding, Tempo, TimelineError};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelLayout {
    Mono,
    Stereo,
}

impl ChannelLayout {
    pub const fn channels(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessorSpec {
    pub id: String,
    pub enabled: bool,
    pub latency_frames: u64,
    pub tail_frames: u64,
}

impl ProcessorSpec {
    pub fn new(id: impl Into<String>, latency_frames: u64, tail_frames: u64) -> Self {
        Self {
            id: id.into(),
            enabled: true,
            latency_frames,
            tail_frames,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipSourceSpec {
    Audio {
        asset_id: String,
        source_offset_frames: u64,
    },
    Composition {
        composition_id: String,
        source_offset_frames: u64,
    },
}

impl ClipSourceSpec {
    pub fn audio(asset_id: impl Into<String>, source_offset_frames: u64) -> Self {
        Self::Audio {
            asset_id: asset_id.into(),
            source_offset_frames,
        }
    }

    pub fn composition(composition_id: impl Into<String>, source_offset_frames: u64) -> Self {
        Self::Composition {
            composition_id: composition_id.into(),
            source_offset_frames,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ClipSpec {
    pub id: String,
    pub start: Beat,
    pub duration: Beat,
    pub source: ClipSourceSpec,
    pub gain: f32,
    pub muted: bool,
    pub processors: Vec<ProcessorSpec>,
    /// Bounded audio emitted by the source after the scheduled clip body.
    pub source_tail_frames: u64,
}

impl ClipSpec {
    pub fn new(id: impl Into<String>, start: Beat, duration: Beat, source: ClipSourceSpec) -> Self {
        Self {
            id: id.into(),
            start,
            duration,
            source,
            gain: 1.0,
            muted: false,
            processors: Vec::new(),
            source_tail_frames: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackSpec {
    pub id: String,
    pub clips: Vec<ClipSpec>,
    pub processors: Vec<ProcessorSpec>,
}

impl TrackSpec {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            clips: Vec::new(),
            processors: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CompositionSpec {
    pub id: String,
    pub length: Beat,
    pub output_layout: ChannelLayout,
    pub tracks: Vec<TrackSpec>,
    pub processors: Vec<ProcessorSpec>,
}

impl CompositionSpec {
    pub fn new(id: impl Into<String>, length: Beat, output_layout: ChannelLayout) -> Self {
        Self {
            id: id.into(),
            length,
            output_layout,
            tracks: Vec::new(),
            processors: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderSource {
    Audio {
        asset_id: Arc<str>,
    },
    Composition {
        composition_index: usize,
        logical_id: Arc<str>,
    },
}

impl RenderSource {
    pub fn logical_id(&self) -> &str {
        match self {
            Self::Audio { asset_id } => asset_id,
            Self::Composition { logical_id, .. } => logical_id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClipMix {
    pub output_start_frame: u64,
    pub output_end_frame: u64,
    pub source_start_frame: u64,
    pub gain: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderClip {
    pub id: Arc<str>,
    /// Half-open scheduled range `[start_frame, end_frame)` before processor tail.
    pub start_frame: u64,
    pub end_frame: u64,
    /// End including the bounded source and clip-processor tail.
    pub audible_end_frame: u64,
    pub source_offset_frames: u64,
    pub gain: f32,
    pub muted: bool,
    pub source: RenderSource,
    pub processors: Arc<[ProcessorSpec]>,
    pub latency_frames: u64,
    pub latency_compensation_frames: u64,
    pub tail_frames: u64,
    /// Portion of `tail_frames` emitted by the source before clip processors.
    pub source_tail_frames: u64,
}

impl RenderClip {
    pub const fn overlaps(&self, start_frame: u64, end_frame: u64) -> bool {
        !self.muted && self.start_frame < end_frame && self.audible_end_frame > start_frame
    }

    /// Describes the direct, pre-tail source portion needed for an output window.
    pub fn mix_region(&self, start_frame: u64, end_frame: u64) -> Option<ClipMix> {
        if self.muted || start_frame >= end_frame {
            return None;
        }
        let output_start = start_frame.max(self.start_frame);
        let output_end = end_frame.min(self.end_frame);
        if output_start >= output_end {
            return None;
        }
        Some(ClipMix {
            output_start_frame: output_start,
            output_end_frame: output_end,
            source_start_frame: self
                .source_offset_frames
                .saturating_add(output_start - self.start_frame),
            gain: self.gain,
        })
    }

    /// Total delay needed to align this clip with all paths in its composition.
    pub const fn total_compensation_frames(&self, track: &RenderTrack) -> u64 {
        self.latency_compensation_frames
            .saturating_add(track.latency_compensation_frames)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderTrack {
    pub id: Arc<str>,
    pub clips: Arc<[RenderClip]>,
    pub processors: Arc<[ProcessorSpec]>,
    pub latency_frames: u64,
    /// Delay applied to the entire track after its processor chain.
    pub latency_compensation_frames: u64,
    pub tail_frames: u64,
}

impl RenderTrack {
    /// Returns scheduled clips whose bounded audible range intersects the half-open query range.
    pub fn clips_overlapping(
        &self,
        start_frame: u64,
        end_frame: u64,
    ) -> impl Iterator<Item = &RenderClip> {
        let prefix_end = self
            .clips
            .partition_point(|clip| clip.start_frame < end_frame);
        self.clips[..prefix_end]
            .iter()
            .filter(move |clip| clip.overlaps(start_frame, end_frame))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderComposition {
    pub id: Arc<str>,
    pub length_frames: u64,
    pub output_layout: ChannelLayout,
    pub tracks: Arc<[RenderTrack]>,
    pub processors: Arc<[ProcessorSpec]>,
    /// Maximum enabled processor latency along any input-to-output path.
    pub latency_frames: u64,
    /// Bounded output past `length_frames`.
    pub tail_frames: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderPlan {
    pub tempo: Tempo,
    /// Child compositions precede parents, allowing a single forward render pass.
    pub compositions: Arc<[RenderComposition]>,
    pub root_index: usize,
    pub tail_cap_frames: u64,
}

impl RenderPlan {
    pub fn root(&self) -> &RenderComposition {
        &self.compositions[self.root_index]
    }

    pub fn composition(&self, id: &str) -> Option<&RenderComposition> {
        self.compositions
            .iter()
            .find(|composition| &*composition.id == id)
    }

    pub fn composition_at(&self, index: usize) -> Option<&RenderComposition> {
        self.compositions.get(index)
    }
}

#[derive(Clone, Debug)]
pub struct RenderPlanBuilder {
    tempo: Tempo,
    tail_cap_frames: u64,
    compositions: Vec<CompositionSpec>,
}

impl RenderPlanBuilder {
    pub fn new(tempo: Tempo, tail_cap_frames: u64) -> Self {
        Self {
            tempo,
            tail_cap_frames,
            compositions: Vec::new(),
        }
    }

    pub fn add_composition(&mut self, composition: CompositionSpec) -> &mut Self {
        self.compositions.push(composition);
        self
    }

    #[must_use]
    pub fn with_composition(mut self, composition: CompositionSpec) -> Self {
        self.add_composition(composition);
        self
    }

    /// Resolves and validates the hierarchy into immutable, child-first arrays.
    ///
    /// # Errors
    ///
    /// Returns [`PlanError`] for invalid IDs, ranges, ownership, cycles, or overflow.
    pub fn build(self, root_id: &str) -> Result<RenderPlan, PlanError> {
        let mut specs = HashMap::new();
        for composition in &self.compositions {
            if specs.insert(composition.id.as_str(), composition).is_some() {
                return Err(PlanError::DuplicateComposition(composition.id.clone()));
            }
        }
        if !specs.contains_key(root_id) {
            return Err(PlanError::MissingRoot(root_id.to_owned()));
        }
        validate_ids_and_hierarchy(&self.compositions, &specs, root_id)?;

        let mut order = Vec::new();
        let mut states = HashMap::new();
        visit(root_id, &specs, &mut states, &mut Vec::new(), &mut order)?;

        let mut indices = HashMap::new();
        let mut rendered = Vec::with_capacity(order.len());
        for id in order {
            let index = rendered.len();
            indices.insert(id, index);
            let spec = specs[id];
            rendered.push(build_composition(
                spec,
                self.tempo,
                self.tail_cap_frames,
                &indices,
                &rendered,
            )?);
        }
        let root_index = indices[root_id];
        Ok(RenderPlan {
            tempo: self.tempo,
            compositions: rendered.into(),
            root_index,
            tail_cap_frames: self.tail_cap_frames,
        })
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("root composition `{0}` does not exist")]
    MissingRoot(String),
    #[error("duplicate composition id `{0}`")]
    DuplicateComposition(String),
    #[error("duplicate track id `{0}`")]
    DuplicateTrack(String),
    #[error("duplicate clip id `{0}`")]
    DuplicateClip(String),
    #[error("composition `{parent}` refers to missing child `{child}`")]
    MissingChild { parent: String, child: String },
    #[error("composition `{child}` has more than one owner: `{first}` and `{second}`")]
    MultipleParents {
        child: String,
        first: String,
        second: String,
    },
    #[error("root composition `{root}` is owned by `{parent}`")]
    RootHasParent { root: String, parent: String },
    #[error("composition cycle: {0:?}")]
    CompositionCycle(Vec<String>),
    #[error("clip `{0}` has an invalid start or duration")]
    InvalidClipRange(String),
    #[error("composition `{0}` has an invalid length")]
    InvalidCompositionLength(String),
    #[error("clip `{0}` has non-finite gain")]
    InvalidGain(String),
    #[error("frame arithmetic overflow while building `{0}`")]
    FrameOverflow(String),
    #[error(transparent)]
    Timeline(#[from] TimelineError),
}

fn validate_ids_and_hierarchy(
    compositions: &[CompositionSpec],
    specs: &HashMap<&str, &CompositionSpec>,
    root_id: &str,
) -> Result<(), PlanError> {
    let mut track_ids = HashSet::new();
    let mut clip_ids = HashSet::new();
    let mut owners: HashMap<&str, &str> = HashMap::new();
    for composition in compositions {
        if composition.length.get() < 0.0 {
            return Err(PlanError::InvalidCompositionLength(composition.id.clone()));
        }
        for track in &composition.tracks {
            if !track_ids.insert(track.id.as_str()) {
                return Err(PlanError::DuplicateTrack(track.id.clone()));
            }
            for clip in &track.clips {
                if !clip_ids.insert(clip.id.as_str()) {
                    return Err(PlanError::DuplicateClip(clip.id.clone()));
                }
                if clip.start.get() < 0.0 || clip.duration.get() <= 0.0 {
                    return Err(PlanError::InvalidClipRange(clip.id.clone()));
                }
                if !clip.gain.is_finite() {
                    return Err(PlanError::InvalidGain(clip.id.clone()));
                }
                if let ClipSourceSpec::Composition { composition_id, .. } = &clip.source {
                    if !specs.contains_key(composition_id.as_str()) {
                        return Err(PlanError::MissingChild {
                            parent: composition.id.clone(),
                            child: composition_id.clone(),
                        });
                    }
                    if let Some(first) = owners.insert(composition_id, &composition.id)
                        && first != composition.id
                    {
                        return Err(PlanError::MultipleParents {
                            child: composition_id.clone(),
                            first: first.to_owned(),
                            second: composition.id.clone(),
                        });
                    }
                }
            }
        }
    }
    // Validate cycles in disconnected definitions too; malformed project state must not compile.
    let mut states = HashMap::new();
    for composition in compositions {
        visit(
            &composition.id,
            specs,
            &mut states,
            &mut Vec::new(),
            &mut Vec::new(),
        )?;
    }
    if let Some(parent) = owners.get(root_id) {
        return Err(PlanError::RootHasParent {
            root: root_id.to_owned(),
            parent: (*parent).to_owned(),
        });
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Done,
}

fn visit<'a>(
    id: &'a str,
    specs: &HashMap<&'a str, &'a CompositionSpec>,
    states: &mut HashMap<&'a str, VisitState>,
    stack: &mut Vec<&'a str>,
    order: &mut Vec<&'a str>,
) -> Result<(), PlanError> {
    match states.get(id) {
        Some(VisitState::Done) => return Ok(()),
        Some(VisitState::Visiting) => {
            let first = stack.iter().position(|entry| *entry == id).unwrap_or(0);
            let mut cycle: Vec<String> = stack[first..]
                .iter()
                .map(|entry| (*entry).to_owned())
                .collect();
            cycle.push(id.to_owned());
            return Err(PlanError::CompositionCycle(cycle));
        }
        None => {}
    }
    states.insert(id, VisitState::Visiting);
    stack.push(id);
    for track in &specs[id].tracks {
        for clip in &track.clips {
            if let ClipSourceSpec::Composition { composition_id, .. } = &clip.source {
                visit(composition_id, specs, states, stack, order)?;
            }
        }
    }
    stack.pop();
    states.insert(id, VisitState::Done);
    order.push(id);
    Ok(())
}

fn enabled_latency(processors: &[ProcessorSpec], context: &str) -> Result<u64, PlanError> {
    processors
        .iter()
        .filter(|processor| processor.enabled)
        .try_fold(0_u64, |total, processor| {
            total
                .checked_add(processor.latency_frames)
                .ok_or_else(|| PlanError::FrameOverflow(context.to_owned()))
        })
}

fn enabled_tail(processors: &[ProcessorSpec], cap: u64) -> u64 {
    processors
        .iter()
        .filter(|processor| processor.enabled)
        .fold(0_u64, |total, processor| {
            total.saturating_add(processor.tail_frames).min(cap)
        })
}

fn beat_frame(
    tempo: Tempo,
    beat: Beat,
    rounding: FrameRounding,
    context: &str,
) -> Result<u64, PlanError> {
    let frame = tempo.beat_to_frame(beat, rounding)?.get();
    u64::try_from(frame).map_err(|_| PlanError::FrameOverflow(context.to_owned()))
}

#[allow(clippy::too_many_lines)]
fn build_composition(
    spec: &CompositionSpec,
    tempo: Tempo,
    tail_cap: u64,
    indices: &HashMap<&str, usize>,
    rendered: &[RenderComposition],
) -> Result<RenderComposition, PlanError> {
    let length_frames = beat_frame(tempo, spec.length, FrameRounding::Ceil, &spec.id)?;
    let output_latency = enabled_latency(&spec.processors, &spec.id)?;
    let output_tail = enabled_tail(&spec.processors, tail_cap);
    let mut tracks = Vec::with_capacity(spec.tracks.len());
    let mut maximum_path_latency = 0_u64;

    for track in &spec.tracks {
        let track_latency = enabled_latency(&track.processors, &track.id)?;
        let mut clips = Vec::with_capacity(track.clips.len());
        for clip in &track.clips {
            let end_beat = Beat::new(clip.start.get() + clip.duration.get())?;
            let start_frame = beat_frame(tempo, clip.start, FrameRounding::Floor, &clip.id)?;
            let end_frame = beat_frame(tempo, end_beat, FrameRounding::Ceil, &clip.id)?;
            let clip_latency = enabled_latency(&clip.processors, &clip.id)?;
            let clip_tail = enabled_tail(&clip.processors, tail_cap);
            let (source, source_offset_frames, inherited_source_tail) = match &clip.source {
                ClipSourceSpec::Audio {
                    asset_id,
                    source_offset_frames,
                } => (
                    RenderSource::Audio {
                        asset_id: Arc::from(asset_id.as_str()),
                    },
                    *source_offset_frames,
                    clip.source_tail_frames,
                ),
                ClipSourceSpec::Composition {
                    composition_id,
                    source_offset_frames,
                } => {
                    let child_index = indices[composition_id.as_str()];
                    let child = &rendered[child_index];
                    let scheduled_frames = end_frame.saturating_sub(start_frame);
                    let source_end = source_offset_frames.saturating_add(scheduled_frames);
                    let child_total = child.length_frames.saturating_add(child.tail_frames);
                    let remaining_tail = if source_end >= child.length_frames {
                        child_total.saturating_sub(source_end)
                    } else {
                        0
                    };
                    (
                        RenderSource::Composition {
                            composition_index: child_index,
                            logical_id: Arc::from(composition_id.as_str()),
                        },
                        *source_offset_frames,
                        remaining_tail.saturating_add(clip.source_tail_frames),
                    )
                }
            };
            let tail_frames = inherited_source_tail
                .saturating_add(clip_tail)
                .min(tail_cap);
            let audible_end_frame = end_frame
                .checked_add(tail_frames)
                .ok_or_else(|| PlanError::FrameOverflow(clip.id.clone()))?;
            let path_latency = clip_latency
                .checked_add(track_latency)
                .ok_or_else(|| PlanError::FrameOverflow(clip.id.clone()))?;
            if !clip.muted {
                maximum_path_latency = maximum_path_latency.max(path_latency);
            }
            clips.push(RenderClip {
                id: Arc::from(clip.id.as_str()),
                start_frame,
                end_frame,
                audible_end_frame,
                source_offset_frames,
                gain: clip.gain,
                muted: clip.muted,
                source,
                processors: clip.processors.clone().into(),
                latency_frames: clip_latency,
                latency_compensation_frames: 0,
                tail_frames,
                source_tail_frames: inherited_source_tail.min(tail_cap),
            });
        }
        clips.sort_by_key(|clip| clip.start_frame);
        tracks.push(RenderTrack {
            id: Arc::from(track.id.as_str()),
            clips: clips.into(),
            processors: track.processors.clone().into(),
            latency_frames: track_latency,
            latency_compensation_frames: 0,
            tail_frames: 0,
        });
    }

    let mut composition_tail = 0_u64;
    for track in &mut tracks {
        let maximum_clip_latency = track
            .clips
            .iter()
            .filter(|clip| !clip.muted)
            .map(|clip| clip.latency_frames)
            .max()
            .unwrap_or(0);
        let track_path_latency = track
            .latency_frames
            .checked_add(maximum_clip_latency)
            .ok_or_else(|| PlanError::FrameOverflow(track.id.to_string()))?;
        track.latency_compensation_frames = maximum_path_latency.saturating_sub(track_path_latency);
        let track_processor_tail = enabled_tail(&track.processors, tail_cap);
        let mut track_end = length_frames;
        let clips = Arc::make_mut(&mut track.clips);
        for clip in clips {
            clip.latency_compensation_frames =
                maximum_clip_latency.saturating_sub(clip.latency_frames);
            if !clip.muted {
                let processed_end = clip
                    .audible_end_frame
                    .checked_add(track_processor_tail)
                    .ok_or_else(|| PlanError::FrameOverflow(clip.id.to_string()))?;
                track_end = track_end.max(processed_end);
            }
        }
        track.tail_frames = track_end.saturating_sub(length_frames).min(tail_cap);
        composition_tail = composition_tail.max(track.tail_frames);
    }
    composition_tail = composition_tail.saturating_add(output_tail).min(tail_cap);
    length_frames
        .checked_add(composition_tail)
        .ok_or_else(|| PlanError::FrameOverflow(spec.id.clone()))?;
    let latency_frames = maximum_path_latency
        .checked_add(output_latency)
        .ok_or_else(|| PlanError::FrameOverflow(spec.id.clone()))?;

    Ok(RenderComposition {
        id: Arc::from(spec.id.as_str()),
        length_frames,
        output_layout: spec.output_layout,
        tracks: tracks.into(),
        processors: spec.processors.clone().into(),
        latency_frames,
        tail_frames: composition_tail,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn beat(value: f64) -> Beat {
        Beat::new(value).unwrap()
    }

    fn builder() -> RenderPlanBuilder {
        RenderPlanBuilder::new(Tempo::new(120.0, 48_000).unwrap(), 48_000)
    }

    #[test]
    fn builds_children_before_parent_and_resolves_indices() {
        let child = CompositionSpec::new("child", beat(2.0), ChannelLayout::Mono);
        let mut parent = CompositionSpec::new("root", beat(4.0), ChannelLayout::Stereo);
        let mut track = TrackSpec::new("track");
        track.clips.push(ClipSpec::new(
            "nested",
            beat(1.0),
            beat(2.0),
            ClipSourceSpec::composition("child", 0),
        ));
        parent.tracks.push(track);
        let plan = builder()
            .with_composition(parent)
            .with_composition(child)
            .build("root")
            .unwrap();
        assert_eq!(&*plan.compositions[0].id, "child");
        assert_eq!(plan.root_index, 1);
        assert!(matches!(
            plan.root().tracks[0].clips[0].source,
            RenderSource::Composition {
                composition_index: 0,
                ..
            }
        ));
    }

    #[test]
    fn rejects_cycles_missing_children_and_multiple_owners() {
        let mut a = CompositionSpec::new("a", beat(1.0), ChannelLayout::Mono);
        let mut at = TrackSpec::new("at");
        at.clips.push(ClipSpec::new(
            "ac",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::composition("b", 0),
        ));
        a.tracks.push(at);
        let mut b = CompositionSpec::new("b", beat(1.0), ChannelLayout::Mono);
        let mut bt = TrackSpec::new("bt");
        bt.clips.push(ClipSpec::new(
            "bc",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::composition("a", 0),
        ));
        b.tracks.push(bt);
        assert!(matches!(
            builder().with_composition(a).with_composition(b).build("a"),
            Err(PlanError::RootHasParent { .. } | PlanError::CompositionCycle(_))
        ));

        let mut missing = CompositionSpec::new("root", beat(1.0), ChannelLayout::Mono);
        let mut track = TrackSpec::new("track");
        track.clips.push(ClipSpec::new(
            "clip",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::composition("nope", 0),
        ));
        missing.tracks.push(track);
        assert!(matches!(
            builder().with_composition(missing).build("root"),
            Err(PlanError::MissingChild { .. })
        ));
    }

    #[test]
    fn schedules_half_open_ranges_and_overlap_queries() {
        let mut root = CompositionSpec::new("root", beat(8.0), ChannelLayout::Stereo);
        let mut track = TrackSpec::new("track");
        track.clips.push(ClipSpec::new(
            "late",
            beat(2.0),
            beat(1.0),
            ClipSourceSpec::audio("b", 7),
        ));
        track.clips.push(ClipSpec::new(
            "early",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("a", 100),
        ));
        root.tracks.push(track);
        let plan = builder().with_composition(root).build("root").unwrap();
        let track = &plan.root().tracks[0];
        assert_eq!(&*track.clips[0].id, "early");
        assert_eq!(track.clips[0].end_frame, 24_000);
        assert_eq!(track.clips_overlapping(24_000, 48_000).count(), 0);
        let mix = track.clips[0].mix_region(12_000, 20_000).unwrap();
        assert_eq!(mix.source_start_frame, 12_100);
    }

    #[test]
    fn computes_path_compensation_and_processor_tail_cap() {
        let mut root = CompositionSpec::new("root", beat(1.0), ChannelLayout::Stereo);
        root.processors
            .push(ProcessorSpec::new("master", 3, 40_000));
        let mut fast = TrackSpec::new("fast");
        fast.processors
            .push(ProcessorSpec::new("fast-track", 5, 30_000));
        let mut fast_clip = ClipSpec::new(
            "fast-clip",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("a", 0),
        );
        fast_clip
            .processors
            .push(ProcessorSpec::new("fast-fx", 2, 30_000));
        fast.clips.push(fast_clip);
        let mut slow = TrackSpec::new("slow");
        slow.processors
            .push(ProcessorSpec::new("slow-track", 10, 0));
        let mut slow_clip = ClipSpec::new(
            "slow-clip",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::audio("b", 0),
        );
        slow_clip
            .processors
            .push(ProcessorSpec::new("slow-fx", 20, 0));
        slow.clips.push(slow_clip);
        root.tracks.extend([fast, slow]);
        let plan = builder().with_composition(root).build("root").unwrap();
        assert_eq!(plan.root().latency_frames, 33);
        assert_eq!(
            plan.root().tracks[0].clips[0].latency_compensation_frames,
            0
        );
        assert_eq!(plan.root().tracks[0].latency_compensation_frames, 23);
        assert_eq!(
            plan.root().tracks[0].clips[0].total_compensation_frames(&plan.root().tracks[0]),
            23
        );
        assert_eq!(plan.root().tail_frames, 48_000);
    }

    #[test]
    fn bypassed_processors_do_not_add_latency_or_tail() {
        let mut root = CompositionSpec::new("root", beat(1.0), ChannelLayout::Mono);
        let mut processor = ProcessorSpec::new("off", 100, 100);
        processor.enabled = false;
        root.processors.push(processor);
        let plan = builder().with_composition(root).build("root").unwrap();
        assert_eq!(plan.root().latency_frames, 0);
        assert_eq!(plan.root().tail_frames, 0);
    }

    #[test]
    fn nested_tail_only_follows_the_end_of_the_child_body() {
        let mut child = CompositionSpec::new("child", beat(2.0), ChannelLayout::Mono);
        child.processors.push(ProcessorSpec::new("tail", 0, 1_000));
        let mut root = CompositionSpec::new("root", beat(1.0), ChannelLayout::Mono);
        let mut track = TrackSpec::new("track");
        track.clips.push(ClipSpec::new(
            "trimmed",
            beat(0.0),
            beat(1.0),
            ClipSourceSpec::composition("child", 0),
        ));
        root.tracks.push(track);

        let plan = builder()
            .with_composition(root)
            .with_composition(child)
            .build("root")
            .unwrap();
        assert_eq!(plan.root().tracks[0].clips[0].tail_frames, 0);
        assert_eq!(plan.root().tail_frames, 0);
    }
}
