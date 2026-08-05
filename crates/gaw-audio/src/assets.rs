//! Lazy audio assets, immutable render revisions, and background cache work.
//!
//! Nothing in this module's registry, materializer, waveform builder, or worker is
//! suitable for an audio callback: those APIs may lock, allocate, or access the
//! filesystem. [`MemoryFrameSource::read_interleaved`] is the exception; it is a
//! bounded copy from immutable memory and can be used while preparing/rendering an
//! already-published real-time snapshot.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    fs::{self, OpenOptions},
    hash::{Hash, Hasher},
    io::{self, BufReader},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use parking_lot::{Mutex, RwLock};
use thiserror::Error;

use crate::render::ChannelLayout;

const MATERIALIZE_CHUNK_FRAMES: usize = 4096;
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Errors produced by asset evaluation and background cache work.
#[derive(Debug, Error)]
pub enum AssetError {
    #[error("sample rate must be non-zero")]
    InvalidSampleRate,
    #[error("project BPM must be finite and greater than zero")]
    InvalidProjectBpm,
    #[error("source layout {source_layout:?} does not match render layout {context_layout:?}")]
    LayoutMismatch {
        source_layout: ChannelLayout,
        context_layout: ChannelLayout,
    },
    #[error("interleaved buffer length {samples} is not divisible by {channels} channels")]
    BufferNotFrameAligned { samples: usize, channels: usize },
    #[error("interleaved audio length {samples} is not divisible by {channels} channels")]
    InvalidMemoryLength { samples: usize, channels: usize },
    #[error("WAV source has unsupported channel count {0}; only mono and stereo are supported")]
    UnsupportedWavChannels(u16),
    #[error("WAV source uses unsupported {bits_per_sample}-bit {sample_format:?} encoding")]
    UnsupportedWavEncoding {
        bits_per_sample: u16,
        sample_format: hound::SampleFormat,
    },
    #[error("paged frame source page size must be non-zero")]
    InvalidPageFrames,
    #[error("paged frame source resident page capacity must be non-zero")]
    InvalidResidentPageCapacity,
    #[error("paged frame source page buffer size overflow")]
    PageSizeOverflow,
    #[error("frame source returned {actual} frames for a {requested}-frame buffer")]
    SourceOverrun { requested: usize, actual: usize },
    #[error("frame source stopped at frame {frame}, before its declared end")]
    SourceEndedEarly { frame: u64 },
    #[error("frame source failed: {0}")]
    Source(String),
    #[error("waveform bucket size must be non-zero")]
    InvalidWaveformResolution,
    #[error("asset background request queue is full")]
    QueueFull,
    #[error("asset background worker has stopped")]
    WorkerStopped,
    #[error("asset background worker thread could not be started: {0}")]
    WorkerSpawn(io::Error),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("WAV error: {0}")]
    Wav(#[from] hound::Error),
}

/// Stable logical identity of an audio-producing value.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct AssetId(Arc<str>);

impl AssetId {
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Hash for AssetId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Debug for AssetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("AssetId").field(&self.0).finish()
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<&str> for AssetId {
    fn from(value: &str) -> Self {
        Self::new(Arc::<str>::from(value))
    }
}

impl From<String> for AssetId {
    fn from(value: String) -> Self {
        Self::new(Arc::<str>::from(value))
    }
}

/// Content identity of one immutable evaluated asset revision.
#[derive(Clone, Eq, Ord, PartialEq, PartialOrd)]
pub struct RevisionId(Arc<str>);

impl RevisionId {
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Derives a deterministic revision key from definition bytes, ordered
    /// dependency revisions, render context, and the audio engine version.
    pub fn derive(
        definition: &[u8],
        dependencies: &[DependencyRevision],
        context: &RenderContext,
    ) -> Self {
        let mut digest = StableDigest::new();
        digest.field(definition);
        for dependency in dependencies {
            digest.field(dependency.asset_id.as_str().as_bytes());
            digest.field(dependency.revision_id.as_str().as_bytes());
        }
        digest.field(&context.sample_rate.to_le_bytes());
        digest.field(&[channel_layout_key(context.channel_layout)]);
        digest.field(&context.project_bpm.to_bits().to_le_bytes());
        match context.requested_range {
            Some(range) => {
                digest.field(&[1]);
                digest.field(&range.start_frame.to_le_bytes());
                digest.field(&range.frame_count.to_le_bytes());
            }
            None => digest.field(&[0]),
        }
        digest.field(&context.seed.to_le_bytes());
        digest.field(context.engine_version.as_bytes());
        Self::new(Arc::<str>::from(digest.finish()))
    }
}

impl Hash for RevisionId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl fmt::Debug for RevisionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("RevisionId").field(&self.0).finish()
    }
}

impl fmt::Display for RevisionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<&str> for RevisionId {
    fn from(value: &str) -> Self {
        Self::new(Arc::<str>::from(value))
    }
}

impl From<String> for RevisionId {
    fn from(value: String) -> Self {
        Self::new(Arc::<str>::from(value))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RequestedFrameRange {
    pub start_frame: u64,
    pub frame_count: u64,
}

/// Inputs that affect deterministic evaluation of an asset.
#[derive(Clone, Debug)]
pub struct RenderContext {
    pub sample_rate: u32,
    pub channel_layout: ChannelLayout,
    pub project_bpm: f64,
    pub requested_range: Option<RequestedFrameRange>,
    pub seed: u64,
    pub engine_version: Arc<str>,
}

impl PartialEq for RenderContext {
    fn eq(&self, other: &Self) -> bool {
        self.sample_rate == other.sample_rate
            && self.channel_layout == other.channel_layout
            && self.project_bpm.to_bits() == other.project_bpm.to_bits()
            && self.requested_range == other.requested_range
            && self.seed == other.seed
            && self.engine_version == other.engine_version
    }
}

impl Eq for RenderContext {}

impl Hash for RenderContext {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.sample_rate.hash(state);
        self.channel_layout.hash(state);
        self.project_bpm.to_bits().hash(state);
        self.requested_range.hash(state);
        self.seed.hash(state);
        self.engine_version.hash(state);
    }
}

impl RenderContext {
    /// Creates a render context.
    ///
    /// # Errors
    ///
    /// Returns [`AssetError::InvalidSampleRate`] when `sample_rate` is zero.
    pub fn new(
        sample_rate: u32,
        channel_layout: ChannelLayout,
        seed: u64,
        engine_version: impl Into<Arc<str>>,
    ) -> Result<Self, AssetError> {
        if sample_rate == 0 {
            return Err(AssetError::InvalidSampleRate);
        }
        Ok(Self {
            sample_rate,
            channel_layout,
            project_bpm: 120.0,
            requested_range: None,
            seed,
            engine_version: engine_version.into(),
        })
    }

    /// Adds project tempo and the optional evaluated frame range to cache identity.
    ///
    /// # Errors
    ///
    /// Returns [`AssetError::InvalidProjectBpm`] for a non-positive or non-finite BPM.
    pub fn with_timeline(
        mut self,
        project_bpm: f64,
        requested_range: Option<RequestedFrameRange>,
    ) -> Result<Self, AssetError> {
        if !project_bpm.is_finite() || project_bpm <= 0.0 {
            return Err(AssetError::InvalidProjectBpm);
        }
        self.project_bpm = project_bpm;
        self.requested_range = requested_range;
        Ok(self)
    }
}

/// One exact revision consumed by a derived asset.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DependencyRevision {
    pub asset_id: AssetId,
    pub revision_id: RevisionId,
}

impl DependencyRevision {
    pub fn new(asset_id: impl Into<AssetId>, revision_id: impl Into<RevisionId>) -> Self {
        Self {
            asset_id: asset_id.into(),
            revision_id: revision_id.into(),
        }
    }
}

/// Lazily evaluates deterministic interleaved frames.
///
/// This is a background/control-plane adapter. Implementations are allowed to
/// decode files, allocate, or lock. A caller must never invoke an arbitrary
/// implementation from the real-time callback; materialize or prepare it first.
/// Samples for an absolute frame range must not depend on read buffer size, how
/// the range is partitioned, or the order in which ranges are requested.
pub trait FrameSource: fmt::Debug + Send + Sync + 'static {
    fn frame_count(&self) -> u64;
    fn channel_layout(&self) -> ChannelLayout;

    /// Writes up to `output.len() / channels` frames starting at `start_frame`.
    /// Returns the number of complete frames written.
    ///
    /// # Errors
    ///
    /// Returns an implementation-specific evaluation error, or an alignment
    /// error when the supplied buffer cannot hold whole frames.
    fn read_interleaved(&self, start_frame: u64, output: &mut [f32]) -> Result<usize, AssetError>;
}

/// Immutable in-memory source. Its read operation is allocation-, lock-, and
/// filesystem-free, making it safe to copy from during snapshot preparation.
#[derive(Clone, Debug)]
pub struct MemoryFrameSource {
    layout: ChannelLayout,
    samples: Arc<[f32]>,
}

impl MemoryFrameSource {
    /// Creates an immutable in-memory source.
    ///
    /// # Errors
    ///
    /// Returns [`AssetError::InvalidMemoryLength`] unless `samples` contains
    /// a whole number of interleaved frames.
    pub fn new(layout: ChannelLayout, samples: impl Into<Arc<[f32]>>) -> Result<Self, AssetError> {
        let samples = samples.into();
        let channels = channel_count(layout);
        if !samples.len().is_multiple_of(channels) {
            return Err(AssetError::InvalidMemoryLength {
                samples: samples.len(),
                channels,
            });
        }
        Ok(Self { layout, samples })
    }

    pub fn samples(&self) -> &[f32] {
        &self.samples
    }
}

impl FrameSource for MemoryFrameSource {
    fn frame_count(&self) -> u64 {
        (self.samples.len() / channel_count(self.layout)) as u64
    }

    fn channel_layout(&self) -> ChannelLayout {
        self.layout
    }

    fn read_interleaved(&self, start_frame: u64, output: &mut [f32]) -> Result<usize, AssetError> {
        let channels = channel_count(self.layout);
        ensure_frame_aligned(output, channels)?;
        let Ok(start_frame) = usize::try_from(start_frame) else {
            return Ok(0);
        };
        let start = start_frame.saturating_mul(channels);
        if start >= self.samples.len() {
            return Ok(0);
        }
        let sample_count = output.len().min(self.samples.len() - start);
        output[..sample_count].copy_from_slice(&self.samples[start..start + sample_count]);
        Ok(sample_count / channels)
    }
}

/// Positional, lazily decoded WAV audio.
///
/// Opening and reading this source access the filesystem and take an internal
/// lock. Both operations are background/control-plane only and must never run
/// in an audio callback.
pub struct WavFrameSource {
    path: PathBuf,
    sample_rate: u32,
    layout: ChannelLayout,
    frame_count: u64,
    spec: hound::WavSpec,
    reader: Mutex<hound::WavReader<BufReader<fs::File>>>,
}

impl WavFrameSource {
    /// Opens a mono or stereo WAV source without decoding its sample payload.
    ///
    /// Float32 and integer PCM with 1 through 32 valid bits are supported,
    /// matching the canonical project importer.
    ///
    /// # Errors
    ///
    /// Returns an I/O/WAV error for an unreadable file, or an asset error for
    /// an invalid sample rate, unsupported channel count, or unsupported
    /// encoding.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, AssetError> {
        let path = path.into();
        let file = fs::File::open(&path)?;
        Self::from_file(path, file)
    }

    /// Creates a lazy source from an already-open file, preserving the path for diagnostics.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::open`].
    pub fn from_file(path: impl Into<PathBuf>, file: fs::File) -> Result<Self, AssetError> {
        let path = path.into();
        let reader = hound::WavReader::new(BufReader::new(file))?;
        let spec = reader.spec();
        if spec.sample_rate == 0 {
            return Err(AssetError::InvalidSampleRate);
        }
        let layout = match spec.channels {
            1 => ChannelLayout::Mono,
            2 => ChannelLayout::Stereo,
            channels => return Err(AssetError::UnsupportedWavChannels(channels)),
        };
        if !matches!(
            (spec.sample_format, spec.bits_per_sample),
            (hound::SampleFormat::Float, 32) | (hound::SampleFormat::Int, 1..=32)
        ) {
            return Err(AssetError::UnsupportedWavEncoding {
                bits_per_sample: spec.bits_per_sample,
                sample_format: spec.sample_format,
            });
        }
        let frame_count = u64::from(reader.duration());
        Ok(Self {
            path,
            sample_rate: spec.sample_rate,
            layout,
            frame_count,
            spec,
            reader: Mutex::new(reader),
        })
    }

    /// Source file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// WAV sample rate.
    pub const fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

impl fmt::Debug for WavFrameSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WavFrameSource")
            .field("path", &self.path)
            .field("sample_rate", &self.sample_rate)
            .field("layout", &self.layout)
            .field("frame_count", &self.frame_count)
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

impl FrameSource for WavFrameSource {
    fn frame_count(&self) -> u64 {
        self.frame_count
    }

    fn channel_layout(&self) -> ChannelLayout {
        self.layout
    }

    #[allow(clippy::cast_precision_loss)]
    fn read_interleaved(&self, start_frame: u64, output: &mut [f32]) -> Result<usize, AssetError> {
        let channels = channel_count(self.layout);
        ensure_frame_aligned(output, channels)?;
        if start_frame >= self.frame_count || output.is_empty() {
            return Ok(0);
        }
        let requested = output.len() / channels;
        let frames = usize::try_from(
            (self.frame_count - start_frame).min(u64::try_from(requested).unwrap_or(u64::MAX)),
        )
        .unwrap_or(requested);
        let start = u32::try_from(start_frame).map_err(|_| {
            AssetError::Source(format!("WAV frame {start_frame} exceeds the format limit"))
        })?;
        let mut reader = self.reader.lock();
        reader.seek(start)?;
        let sample_count = frames
            .checked_mul(channels)
            .ok_or(AssetError::PageSizeOverflow)?;
        match self.spec.sample_format {
            hound::SampleFormat::Float => {
                let mut samples = reader.samples::<f32>();
                for (index, destination) in output[..sample_count].iter_mut().enumerate() {
                    let sample =
                        samples
                            .next()
                            .transpose()?
                            .ok_or(AssetError::SourceEndedEarly {
                                frame: start_frame + (index / channels) as u64,
                            })?;
                    if !sample.is_finite() {
                        return Err(AssetError::Source(format!(
                            "WAV source contains a non-finite sample at frame {}",
                            start_frame + (index / channels) as u64
                        )));
                    }
                    *destination = sample;
                }
            }
            hound::SampleFormat::Int => {
                let scale = 2_f32.powi(i32::from(self.spec.bits_per_sample).saturating_sub(1));
                let mut samples = reader.samples::<i32>();
                for (index, destination) in output[..sample_count].iter_mut().enumerate() {
                    let sample =
                        samples
                            .next()
                            .transpose()?
                            .ok_or(AssetError::SourceEndedEarly {
                                frame: start_frame + (index / channels) as u64,
                            })?;
                    *destination = sample as f32 / scale;
                }
            }
        }
        Ok(frames)
    }
}

#[derive(Debug)]
struct ResidentPage {
    samples: Arc<[f32]>,
}

#[derive(Debug, Default)]
struct PageCache {
    pages: HashMap<u64, Arc<ResidentPage>>,
    /// Least-recently used page first.
    recency: VecDeque<u64>,
}

/// Observable bounded-cache residency for a [`PagedFrameSource`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PagedFrameSourceResidency {
    /// Frames in every full page.
    pub page_frames: usize,
    /// Maximum number of resident pages.
    pub maximum_resident_pages: usize,
    /// Currently resident pages.
    pub resident_pages: usize,
    /// Currently resident frames, including a possibly short final page.
    pub resident_frames: usize,
    /// Resident page indices, ordered least- to most-recently used.
    pub page_indices: Vec<u64>,
}

/// A positional fixed-page LRU cache over another lazy frame source.
///
/// Cache misses allocate, may lock, and call the wrapped source, so reads remain
/// background/control-plane work. Cached pages are immutable and cache memory is
/// capped at `maximum_resident_pages * page_frames * channels` samples.
#[derive(Debug)]
pub struct PagedFrameSource {
    source: Arc<dyn FrameSource>,
    page_frames: usize,
    maximum_resident_pages: usize,
    cache: Mutex<PageCache>,
}

impl PagedFrameSource {
    /// Wraps a source in a bounded fixed-page cache.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero page size, zero resident-page capacity, or
    /// a page sample-count overflow.
    pub fn new(
        source: Arc<dyn FrameSource>,
        page_frames: usize,
        maximum_resident_pages: usize,
    ) -> Result<Self, AssetError> {
        if page_frames == 0 {
            return Err(AssetError::InvalidPageFrames);
        }
        if maximum_resident_pages == 0 {
            return Err(AssetError::InvalidResidentPageCapacity);
        }
        let page_samples = page_frames
            .checked_mul(channel_count(source.channel_layout()))
            .ok_or(AssetError::PageSizeOverflow)?;
        page_samples
            .checked_mul(maximum_resident_pages)
            .ok_or(AssetError::PageSizeOverflow)?;
        Ok(Self {
            source,
            page_frames,
            maximum_resident_pages,
            cache: Mutex::new(PageCache::default()),
        })
    }

    /// Wrapped source.
    pub fn source(&self) -> &Arc<dyn FrameSource> {
        &self.source
    }

    /// Frames in each full page.
    pub const fn page_frames(&self) -> usize {
        self.page_frames
    }

    /// Maximum number of resident pages.
    pub const fn maximum_resident_pages(&self) -> usize {
        self.maximum_resident_pages
    }

    /// Returns a point-in-time cache residency snapshot.
    ///
    /// This takes the cache lock and is background/control-plane only.
    pub fn residency(&self) -> PagedFrameSourceResidency {
        let cache = self.cache.lock();
        let resident_frames = cache
            .pages
            .values()
            .map(|page| page.samples.len() / channel_count(self.channel_layout()))
            .fold(0_usize, usize::saturating_add);
        PagedFrameSourceResidency {
            page_frames: self.page_frames,
            maximum_resident_pages: self.maximum_resident_pages,
            resident_pages: cache.pages.len(),
            resident_frames,
            page_indices: cache.recency.iter().copied().collect(),
        }
    }

    /// Drops every resident page.
    ///
    /// This takes the cache lock and is background/control-plane only.
    pub fn clear_resident(&self) {
        let mut cache = self.cache.lock();
        cache.pages.clear();
        cache.recency.clear();
    }

    fn page(&self, page_index: u64) -> Result<Arc<ResidentPage>, AssetError> {
        {
            let mut cache = self.cache.lock();
            if let Some(page) = cache.pages.get(&page_index).cloned() {
                touch_page(&mut cache.recency, page_index);
                return Ok(page);
            }
        }

        let page_start = page_index
            .checked_mul(self.page_frames as u64)
            .ok_or(AssetError::PageSizeOverflow)?;
        let frames = usize::try_from(
            self.frame_count()
                .saturating_sub(page_start)
                .min(self.page_frames as u64),
        )
        .map_err(|_| AssetError::PageSizeOverflow)?;
        let channels = channel_count(self.channel_layout());
        let samples = frames
            .checked_mul(channels)
            .ok_or(AssetError::PageSizeOverflow)?;
        let mut loaded = vec![0.0; samples];
        let mut read_frames = 0;
        while read_frames < frames {
            let read = self.source.read_interleaved(
                page_start + read_frames as u64,
                &mut loaded[read_frames * channels..],
            )?;
            validate_source_read(page_start + read_frames as u64, frames - read_frames, read)?;
            read_frames += read;
        }
        let loaded = Arc::new(ResidentPage {
            samples: loaded.into(),
        });

        let mut cache = self.cache.lock();
        if let Some(page) = cache.pages.get(&page_index).cloned() {
            touch_page(&mut cache.recency, page_index);
            return Ok(page);
        }
        while cache.pages.len() >= self.maximum_resident_pages {
            let Some(evicted) = cache.recency.pop_front() else {
                break;
            };
            cache.pages.remove(&evicted);
        }
        cache.pages.insert(page_index, Arc::clone(&loaded));
        cache.recency.push_back(page_index);
        Ok(loaded)
    }
}

impl FrameSource for PagedFrameSource {
    fn frame_count(&self) -> u64 {
        self.source.frame_count()
    }

    fn channel_layout(&self) -> ChannelLayout {
        self.source.channel_layout()
    }

    fn read_interleaved(&self, start_frame: u64, output: &mut [f32]) -> Result<usize, AssetError> {
        let channels = channel_count(self.channel_layout());
        ensure_frame_aligned(output, channels)?;
        if start_frame >= self.frame_count() || output.is_empty() {
            return Ok(0);
        }
        let requested = output.len() / channels;
        let frames = usize::try_from(
            (self.frame_count() - start_frame).min(u64::try_from(requested).unwrap_or(u64::MAX)),
        )
        .unwrap_or(requested);
        let mut copied = 0;
        while copied < frames {
            let position = start_frame + copied as u64;
            let page_index = position / self.page_frames as u64;
            let page_offset = usize::try_from(position % self.page_frames as u64)
                .map_err(|_| AssetError::PageSizeOverflow)?;
            let page = self.page(page_index)?;
            let page_frame_count = page.samples.len() / channels;
            let copy_frames = (frames - copied).min(page_frame_count - page_offset);
            let source_start = page_offset * channels;
            let destination_start = copied * channels;
            let sample_count = copy_frames * channels;
            output[destination_start..destination_start + sample_count]
                .copy_from_slice(&page.samples[source_start..source_start + sample_count]);
            copied += copy_frames;
        }
        Ok(copied)
    }
}

fn touch_page(recency: &mut VecDeque<u64>, page_index: u64) {
    if let Some(position) = recency
        .iter()
        .position(|candidate| *candidate == page_index)
    {
        recency.remove(position);
    }
    recency.push_back(page_index);
}

/// One immutable, context-specific render of a logical asset.
#[derive(Clone)]
pub struct AssetRevision {
    asset_id: AssetId,
    revision_id: RevisionId,
    context: RenderContext,
    dependencies: Arc<[DependencyRevision]>,
    source: Arc<dyn FrameSource>,
}

impl AssetRevision {
    /// Creates a concrete immutable asset revision.
    ///
    /// # Errors
    ///
    /// Returns [`AssetError::LayoutMismatch`] when the source does not produce
    /// the layout declared by the render context.
    pub fn new(
        asset_id: impl Into<AssetId>,
        revision_id: impl Into<RevisionId>,
        context: RenderContext,
        dependencies: impl Into<Arc<[DependencyRevision]>>,
        source: Arc<dyn FrameSource>,
    ) -> Result<Self, AssetError> {
        if source.channel_layout() != context.channel_layout {
            return Err(AssetError::LayoutMismatch {
                source_layout: source.channel_layout(),
                context_layout: context.channel_layout,
            });
        }
        Ok(Self {
            asset_id: asset_id.into(),
            revision_id: revision_id.into(),
            context,
            dependencies: dependencies.into(),
            source,
        })
    }

    pub fn asset_id(&self) -> &AssetId {
        &self.asset_id
    }

    pub fn revision_id(&self) -> &RevisionId {
        &self.revision_id
    }

    pub fn context(&self) -> &RenderContext {
        &self.context
    }

    pub fn dependencies(&self) -> &[DependencyRevision] {
        &self.dependencies
    }

    pub fn source(&self) -> &Arc<dyn FrameSource> {
        &self.source
    }

    pub fn frame_count(&self) -> u64 {
        self.source.frame_count()
    }
}

impl fmt::Debug for AssetRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AssetRevision")
            .field("asset_id", &self.asset_id)
            .field("revision_id", &self.revision_id)
            .field("context", &self.context)
            .field("dependencies", &self.dependencies)
            .field("frame_count", &self.frame_count())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RevisionFreshness {
    Current,
    LastValid,
}

#[derive(Clone, Debug)]
pub struct ResolvedRevision {
    pub revision: Arc<AssetRevision>,
    pub freshness: RevisionFreshness,
}

#[derive(Debug, Default)]
struct RegistryState {
    assets: HashMap<AssetId, RevisionSlot>,
    dependents: HashMap<AssetId, HashSet<AssetId>>,
}

#[derive(Debug, Default)]
struct RevisionSlot {
    current: Option<Arc<AssetRevision>>,
    last_valid: Option<Arc<AssetRevision>>,
}

/// Control-plane revision registry with recursive dependency invalidation.
///
/// Its methods take a lock and must not be called by the real-time callback.
#[derive(Debug, Default)]
pub struct AssetRegistry {
    state: RwLock<RegistryState>,
}

impl AssetRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes a revision atomically and invalidates all transitive dependents
    /// when the logical asset's current revision changes. A completed render
    /// whose exact dependencies are no longer current is retained as fallback.
    pub fn publish(&self, revision: Arc<AssetRevision>) -> Vec<AssetId> {
        let mut state = self.state.write();
        let asset_id = revision.asset_id.clone();
        let dependencies_are_current = revision.dependencies().iter().all(|dependency| {
            state
                .assets
                .get(&dependency.asset_id)
                .and_then(|slot| slot.current.as_ref())
                .is_some_and(|current| current.revision_id == dependency.revision_id)
        });
        if !dependencies_are_current {
            let slot = state.assets.entry(asset_id).or_default();
            if slot.current.is_none() && slot.last_valid.is_none() {
                slot.last_valid = Some(revision);
            }
            return Vec::new();
        }
        let old_revision = state
            .assets
            .get(&asset_id)
            .and_then(|slot| slot.current.clone().or_else(|| slot.last_valid.clone()));

        remove_dependency_edges(&mut state, &asset_id, old_revision.as_deref());
        for dependency in revision.dependencies() {
            state
                .dependents
                .entry(dependency.asset_id.clone())
                .or_default()
                .insert(asset_id.clone());
        }

        let changed = old_revision
            .as_ref()
            .is_some_and(|old| old.revision_id != revision.revision_id);
        let slot = state.assets.entry(asset_id.clone()).or_default();
        if changed && let Some(previous) = slot.current.take() {
            slot.last_valid = Some(previous);
        }
        slot.current = Some(revision);

        if changed {
            invalidate_dependents(&mut state, &asset_id)
        } else {
            Vec::new()
        }
    }

    /// Invalidates an asset and its transitive dependents, retaining each
    /// current revision as a playable last-valid fallback.
    pub fn invalidate(&self, asset_id: &AssetId) -> Vec<AssetId> {
        let mut state = self.state.write();
        invalidate_from(&mut state, asset_id)
    }

    pub fn current(&self, asset_id: &AssetId) -> Option<Arc<AssetRevision>> {
        self.state
            .read()
            .assets
            .get(asset_id)
            .and_then(|slot| slot.current.clone())
    }

    pub fn last_valid(&self, asset_id: &AssetId) -> Option<Arc<AssetRevision>> {
        self.state
            .read()
            .assets
            .get(asset_id)
            .and_then(|slot| slot.last_valid.clone())
    }

    /// Resolves current audio, falling back to the last valid revision while a
    /// replacement is evaluated in the background.
    pub fn resolve(&self, asset_id: &AssetId) -> Option<ResolvedRevision> {
        let state = self.state.read();
        let slot = state.assets.get(asset_id)?;
        slot.current
            .clone()
            .map(|revision| ResolvedRevision {
                revision,
                freshness: RevisionFreshness::Current,
            })
            .or_else(|| {
                slot.last_valid.clone().map(|revision| ResolvedRevision {
                    revision,
                    freshness: RevisionFreshness::LastValid,
                })
            })
    }
}

/// Result of materializing one immutable revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedAsset {
    pub revision_id: RevisionId,
    pub path: PathBuf,
    pub sample_rate: u32,
    pub channel_layout: ChannelLayout,
    pub frame_count: u64,
}

/// Background-only deterministic WAV cache writer.
#[derive(Clone, Debug)]
pub struct Materializer {
    audio_cache_directory: PathBuf,
}

impl Materializer {
    pub fn new(audio_cache_directory: impl Into<PathBuf>) -> Self {
        Self {
            audio_cache_directory: audio_cache_directory.into(),
        }
    }

    pub fn cache_path(&self, revision: &AssetRevision) -> PathBuf {
        let mut digest = StableDigest::new();
        digest.field(revision.revision_id.as_str().as_bytes());
        digest.field(&revision.context.sample_rate.to_le_bytes());
        digest.field(&[channel_layout_key(revision.context.channel_layout)]);
        digest.field(&revision.context.project_bpm.to_bits().to_le_bytes());
        match revision.context.requested_range {
            Some(range) => {
                digest.field(&[1]);
                digest.field(&range.start_frame.to_le_bytes());
                digest.field(&range.frame_count.to_le_bytes());
            }
            None => digest.field(&[0]),
        }
        digest.field(&revision.context.seed.to_le_bytes());
        digest.field(revision.context.engine_version.as_bytes());
        self.audio_cache_directory
            .join(format!("{}.wav", digest.finish()))
    }

    /// Evaluates and atomically publishes a float WAV. This performs filesystem
    /// work and must only run on a background/control thread.
    ///
    /// # Errors
    ///
    /// Returns an evaluation, WAV encoding, or filesystem error. A failed write
    /// never publishes the temporary file as the immutable cache entry.
    pub fn materialize(&self, revision: &AssetRevision) -> Result<MaterializedAsset, AssetError> {
        let target = self.cache_path(revision);
        if cached_wav_matches(&target, revision) {
            return Ok(materialized_description(revision, target));
        }
        fs::create_dir_all(&self.audio_cache_directory)?;

        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = target.with_extension(format!("wav.tmp-{}-{sequence}", std::process::id()));
        let result = write_revision_wav(&temporary, revision).and_then(|()| {
            // A concurrent worker may have published the same immutable
            // revision first. Keeping that valid entry is equivalent.
            if cached_wav_matches(&target, revision) {
                fs::remove_file(&temporary)?;
                return Ok(());
            }
            if target.exists() {
                fs::remove_file(&target)?;
            }
            fs::rename(&temporary, &target)?;
            Ok(())
        });
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        Ok(materialized_description(revision, target))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaveformPeak {
    pub minimum: f32,
    pub maximum: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WaveformBucket {
    pub first_frame: u64,
    pub frame_count: u32,
    /// One peak per channel, in channel order.
    pub peaks: Vec<WaveformPeak>,
}

/// Compact public name for a waveform display bucket.
pub type PeakBucket = WaveformBucket;

#[derive(Clone, Debug, PartialEq)]
pub struct Waveform {
    pub revision_id: RevisionId,
    pub channel_layout: ChannelLayout,
    pub source_frame_count: u64,
    pub frames_per_bucket: u32,
    pub buckets: Vec<WaveformBucket>,
}

impl Waveform {
    /// Evaluates exact per-channel min/max peaks. Background only.
    ///
    /// # Errors
    ///
    /// Returns [`AssetError::InvalidWaveformResolution`] for zero-sized buckets,
    /// or propagates an error from the lazy frame source.
    pub fn generate(revision: &AssetRevision, frames_per_bucket: u32) -> Result<Self, AssetError> {
        if frames_per_bucket == 0 {
            return Err(AssetError::InvalidWaveformResolution);
        }
        let channels = channel_count(revision.context.channel_layout);
        let chunk_frames = MATERIALIZE_CHUNK_FRAMES
            .min(frames_per_bucket as usize)
            .max(1);
        let mut scratch = vec![0.0; chunk_frames * channels];
        let mut buckets = Vec::new();
        let mut position = 0_u64;
        let mut bucket_first = 0_u64;
        let mut bucket_frames = 0_u32;
        let mut minima = vec![f32::INFINITY; channels];
        let mut maxima = vec![f32::NEG_INFINITY; channels];

        while position < revision.frame_count() {
            let remaining = revision.frame_count() - position;
            let requested =
                usize::try_from(remaining.min(chunk_frames as u64)).unwrap_or(chunk_frames);
            let output = &mut scratch[..requested * channels];
            let read = revision.source.read_interleaved(position, output)?;
            validate_source_read(position, requested, read)?;

            for frame in output[..read * channels].chunks_exact(channels) {
                for (channel, sample) in frame.iter().copied().enumerate() {
                    minima[channel] = minima[channel].min(sample);
                    maxima[channel] = maxima[channel].max(sample);
                }
                bucket_frames += 1;
                if bucket_frames == frames_per_bucket {
                    push_peak_bucket(
                        &mut buckets,
                        bucket_first,
                        bucket_frames,
                        &mut minima,
                        &mut maxima,
                    );
                    bucket_first += u64::from(bucket_frames);
                    bucket_frames = 0;
                }
            }
            position += read as u64;
        }
        if bucket_frames != 0 {
            push_peak_bucket(
                &mut buckets,
                bucket_first,
                bucket_frames,
                &mut minima,
                &mut maxima,
            );
        }

        Ok(Self {
            revision_id: revision.revision_id.clone(),
            channel_layout: revision.context.channel_layout,
            source_frame_count: revision.frame_count(),
            frames_per_bucket,
            buckets,
        })
    }
}

pub type AssetRequestId = u64;

#[derive(Clone, Debug)]
pub enum AssetRequest {
    Materialize {
        request_id: AssetRequestId,
        revision: Arc<AssetRevision>,
    },
    GenerateWaveform {
        request_id: AssetRequestId,
        revision: Arc<AssetRevision>,
        frames_per_bucket: u32,
    },
    Publish {
        request_id: AssetRequestId,
        revision: Arc<AssetRevision>,
    },
    Invalidate {
        request_id: AssetRequestId,
        asset_id: AssetId,
    },
}

impl AssetRequest {
    fn request_id(&self) -> AssetRequestId {
        match self {
            Self::Materialize { request_id, .. }
            | Self::GenerateWaveform { request_id, .. }
            | Self::Publish { request_id, .. }
            | Self::Invalidate { request_id, .. } => *request_id,
        }
    }
}

#[derive(Clone, Debug)]
pub enum AssetProduct {
    Materialized(MaterializedAsset),
    Waveform(Waveform),
    Published { invalidated: Vec<AssetId> },
    Invalidated { assets: Vec<AssetId> },
}

#[derive(Debug)]
pub struct AssetResponse {
    pub request_id: AssetRequestId,
    pub result: Result<AssetProduct, AssetError>,
}

/// Handle to one bounded background asset worker.
///
/// `try_request` never waits. Responses are also bounded; the worker applies
/// backpressure away from the real-time thread until a response is received.
#[derive(Debug)]
pub struct BackgroundAssetWorker {
    requests: Option<Sender<AssetRequest>>,
    responses: Option<Receiver<AssetResponse>>,
    join: Option<JoinHandle<()>>,
}

impl BackgroundAssetWorker {
    /// Starts a worker with bounded request and response queues.
    ///
    /// # Errors
    ///
    /// Returns [`AssetError::WorkerSpawn`] if the operating system cannot start
    /// the background thread.
    pub fn spawn(
        materializer: Materializer,
        registry: Arc<AssetRegistry>,
        request_capacity: usize,
        response_capacity: usize,
    ) -> Result<Self, AssetError> {
        let (request_sender, request_receiver) = bounded(request_capacity);
        let (response_sender, response_receiver) = bounded(response_capacity);
        let join = thread::Builder::new()
            .name("gaw-asset-worker".into())
            .spawn(move || {
                worker_loop(
                    &request_receiver,
                    &response_sender,
                    &materializer,
                    &registry,
                );
            })
            .map_err(AssetError::WorkerSpawn)?;
        Ok(Self {
            requests: Some(request_sender),
            responses: Some(response_receiver),
            join: Some(join),
        })
    }

    /// Non-blocking and bounded, so it can be used by a callback to enqueue an
    /// already-prepared request. Dropping work on [`AssetError::QueueFull`] is
    /// intentional; callers may retry from the control thread.
    ///
    /// # Errors
    ///
    /// Returns [`AssetError::QueueFull`] under backpressure or
    /// [`AssetError::WorkerStopped`] after worker shutdown.
    pub fn try_request(&self, request: AssetRequest) -> Result<(), AssetError> {
        self.requests
            .as_ref()
            .ok_or(AssetError::WorkerStopped)?
            .try_send(request)
            .map_err(|error| match error {
                TrySendError::Full(_) => AssetError::QueueFull,
                TrySendError::Disconnected(_) => AssetError::WorkerStopped,
            })
    }

    /// Receives a response without waiting.
    ///
    /// # Errors
    ///
    /// Returns [`AssetError::WorkerStopped`] if the worker has disconnected.
    pub fn try_response(&self) -> Result<Option<AssetResponse>, AssetError> {
        match self
            .responses
            .as_ref()
            .ok_or(AssetError::WorkerStopped)?
            .try_recv()
        {
            Ok(response) => Ok(Some(response)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(AssetError::WorkerStopped),
        }
    }

    pub fn response_receiver(&self) -> Option<&Receiver<AssetResponse>> {
        self.responses.as_ref()
    }
}

impl Drop for BackgroundAssetWorker {
    fn drop(&mut self) {
        self.requests.take();
        self.responses.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn worker_loop(
    requests: &Receiver<AssetRequest>,
    responses: &Sender<AssetResponse>,
    materializer: &Materializer,
    registry: &AssetRegistry,
) {
    while let Ok(request) = requests.recv() {
        let request_id = request.request_id();
        let result = match request {
            AssetRequest::Materialize { revision, .. } => materializer
                .materialize(&revision)
                .map(AssetProduct::Materialized),
            AssetRequest::GenerateWaveform {
                revision,
                frames_per_bucket,
                ..
            } => Waveform::generate(&revision, frames_per_bucket).map(AssetProduct::Waveform),
            AssetRequest::Publish { revision, .. } => Ok(AssetProduct::Published {
                invalidated: registry.publish(revision),
            }),
            AssetRequest::Invalidate { asset_id, .. } => Ok(AssetProduct::Invalidated {
                assets: registry.invalidate(&asset_id),
            }),
        };
        if responses
            .send(AssetResponse { request_id, result })
            .is_err()
        {
            break;
        }
    }
}

fn channel_count(layout: ChannelLayout) -> usize {
    match layout {
        ChannelLayout::Mono => 1,
        ChannelLayout::Stereo => 2,
    }
}

const fn channel_layout_key(layout: ChannelLayout) -> u8 {
    match layout {
        ChannelLayout::Mono => 1,
        ChannelLayout::Stereo => 2,
    }
}

fn ensure_frame_aligned(output: &[f32], channels: usize) -> Result<(), AssetError> {
    if !output.len().is_multiple_of(channels) {
        return Err(AssetError::BufferNotFrameAligned {
            samples: output.len(),
            channels,
        });
    }
    Ok(())
}

fn validate_source_read(position: u64, requested: usize, read: usize) -> Result<(), AssetError> {
    if read > requested {
        return Err(AssetError::SourceOverrun {
            requested,
            actual: read,
        });
    }
    if read == 0 && requested != 0 {
        return Err(AssetError::SourceEndedEarly { frame: position });
    }
    Ok(())
}

fn remove_dependency_edges(
    state: &mut RegistryState,
    asset_id: &AssetId,
    old_revision: Option<&AssetRevision>,
) {
    let Some(old_revision) = old_revision else {
        return;
    };
    for dependency in old_revision.dependencies() {
        if let Some(dependents) = state.dependents.get_mut(&dependency.asset_id) {
            dependents.remove(asset_id);
            if dependents.is_empty() {
                state.dependents.remove(&dependency.asset_id);
            }
        }
    }
}

fn invalidate_from(state: &mut RegistryState, asset_id: &AssetId) -> Vec<AssetId> {
    let mut invalidated = Vec::new();
    let mut pending = VecDeque::from([asset_id.clone()]);
    let mut visited = HashSet::new();
    while let Some(next) = pending.pop_front() {
        if !visited.insert(next.clone()) {
            continue;
        }
        if let Some(slot) = state.assets.get_mut(&next)
            && let Some(current) = slot.current.take()
        {
            slot.last_valid = Some(current);
            invalidated.push(next.clone());
        }
        if let Some(dependents) = state.dependents.get(&next) {
            pending.extend(dependents.iter().cloned());
        }
    }
    invalidated
}

fn invalidate_dependents(state: &mut RegistryState, asset_id: &AssetId) -> Vec<AssetId> {
    let Some(direct) = state.dependents.get(asset_id).cloned() else {
        return Vec::new();
    };
    let mut invalidated = Vec::new();
    let mut visited = HashSet::new();
    let mut pending = VecDeque::from_iter(direct);
    while let Some(next) = pending.pop_front() {
        if !visited.insert(next.clone()) {
            continue;
        }
        if let Some(slot) = state.assets.get_mut(&next)
            && let Some(current) = slot.current.take()
        {
            slot.last_valid = Some(current);
            invalidated.push(next.clone());
        }
        if let Some(dependents) = state.dependents.get(&next) {
            pending.extend(dependents.iter().cloned());
        }
    }
    invalidated
}

fn materialized_description(revision: &AssetRevision, path: PathBuf) -> MaterializedAsset {
    MaterializedAsset {
        revision_id: revision.revision_id.clone(),
        path,
        sample_rate: revision.context.sample_rate,
        channel_layout: revision.context.channel_layout,
        frame_count: revision.frame_count(),
    }
}

fn cached_wav_matches(path: &Path, revision: &AssetRevision) -> bool {
    if !path.is_file() {
        return false;
    }
    let Ok(reader) = hound::WavReader::open(path) else {
        return false;
    };
    let spec = reader.spec();
    if !(spec.sample_rate == revision.context.sample_rate
        && usize::from(spec.channels) == channel_count(revision.context.channel_layout)
        && spec.sample_format == hound::SampleFormat::Float
        && spec.bits_per_sample == 32
        && u64::from(reader.duration()) == revision.frame_count())
    {
        return false;
    }
    let channels = channel_count(revision.context.channel_layout);
    let mut cached = reader.into_samples::<f32>();
    let mut scratch = vec![0.0; MATERIALIZE_CHUNK_FRAMES * channels];
    let mut position = 0_u64;
    while position < revision.frame_count() {
        let frames = usize::try_from(
            (revision.frame_count() - position).min(MATERIALIZE_CHUNK_FRAMES as u64),
        )
        .unwrap_or(MATERIALIZE_CHUNK_FRAMES);
        let expected = &mut scratch[..frames * channels];
        let Ok(read) = revision.source.read_interleaved(position, expected) else {
            return false;
        };
        if read != frames {
            return false;
        }
        for &expected in expected.iter() {
            let Some(Ok(actual)) = cached.next() else {
                return false;
            };
            if actual.to_bits() != expected.to_bits() {
                return false;
            }
        }
        position += frames as u64;
    }
    cached.next().is_none()
}

fn write_revision_wav(path: &Path, revision: &AssetRevision) -> Result<(), AssetError> {
    let channels = channel_count(revision.context.channel_layout);
    let spec = hound::WavSpec {
        channels: match revision.context.channel_layout {
            ChannelLayout::Mono => 1,
            ChannelLayout::Stereo => 2,
        },
        sample_rate: revision.context.sample_rate,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut writer = hound::WavWriter::new(file, spec)?;
    let mut scratch = vec![0.0_f32; MATERIALIZE_CHUNK_FRAMES * channels];
    let mut position = 0_u64;
    while position < revision.frame_count() {
        let remaining = revision.frame_count() - position;
        let requested = usize::try_from(remaining.min(MATERIALIZE_CHUNK_FRAMES as u64))
            .unwrap_or(MATERIALIZE_CHUNK_FRAMES);
        let output = &mut scratch[..requested * channels];
        let read = revision.source.read_interleaved(position, output)?;
        validate_source_read(position, requested, read)?;
        for &sample in &output[..read * channels] {
            writer.write_sample(sample)?;
        }
        position += read as u64;
    }
    writer.finalize()?;
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

fn push_peak_bucket(
    buckets: &mut Vec<WaveformBucket>,
    first_frame: u64,
    frame_count: u32,
    minima: &mut [f32],
    maxima: &mut [f32],
) {
    let peaks = minima
        .iter()
        .copied()
        .zip(maxima.iter().copied())
        .map(|(minimum, maximum)| WaveformPeak { minimum, maximum })
        .collect();
    buckets.push(WaveformBucket {
        first_frame,
        frame_count,
        peaks,
    });
    minima.fill(f32::INFINITY);
    maxima.fill(f32::NEG_INFINITY);
}

/// Small deterministic digest used only for stable cache names, not security.
#[derive(Debug)]
struct StableDigest {
    first: u64,
    second: u64,
}

impl StableDigest {
    fn new() -> Self {
        Self {
            first: 0xcbf2_9ce4_8422_2325,
            second: 0x8422_2325_cbf2_9ce4,
        }
    }

    fn field(&mut self, bytes: &[u8]) {
        self.mix(&(bytes.len() as u64).to_le_bytes());
        self.mix(bytes);
    }

    fn mix(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.first ^= u64::from(byte);
            self.first = self.first.wrapping_mul(0x0000_0100_0000_01b3);
            self.second ^= u64::from(byte).wrapping_add(0x9d);
            self.second = self.second.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn finish(self) -> String {
        format!("{:016x}{:016x}", self.first, self.second)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
        time::Duration,
    };

    fn context(layout: ChannelLayout) -> RenderContext {
        RenderContext::new(48_000, layout, 7, "test-engine").unwrap()
    }

    fn revision(
        asset_id: &str,
        revision_id: &str,
        layout: ChannelLayout,
        samples: &[f32],
        dependencies: Vec<DependencyRevision>,
    ) -> Arc<AssetRevision> {
        Arc::new(
            AssetRevision::new(
                asset_id,
                revision_id,
                context(layout),
                Arc::<[DependencyRevision]>::from(dependencies),
                Arc::new(MemoryFrameSource::new(layout, Arc::<[f32]>::from(samples)).unwrap()),
            )
            .unwrap(),
        )
    }

    fn temporary_directory(label: &str) -> PathBuf {
        let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "gaw-assets-{label}-{}-{sequence}",
            std::process::id()
        ))
    }

    #[test]
    fn memory_source_reads_frames_and_validates_alignment() {
        let source = MemoryFrameSource::new(
            ChannelLayout::Stereo,
            Arc::<[f32]>::from([1.0, 2.0, 3.0, 4.0]),
        )
        .unwrap();
        let mut output = [0.0; 4];
        assert_eq!(source.read_interleaved(1, &mut output).unwrap(), 1);
        assert_eq!(&output[..2], &[3.0, 4.0]);
        assert!(matches!(
            source.read_interleaved(0, &mut output[..3]),
            Err(AssetError::BufferNotFrameAligned { .. })
        ));
    }

    #[test]
    fn wav_source_reads_float_and_integer_pcm_positionally() {
        let directory = temporary_directory("wav-source");
        fs::create_dir_all(&directory).unwrap();
        let float_path = directory.join("float.wav");
        let mut writer = hound::WavWriter::create(
            &float_path,
            hound::WavSpec {
                channels: 2,
                sample_rate: 44_100,
                bits_per_sample: 32,
                sample_format: hound::SampleFormat::Float,
            },
        )
        .unwrap();
        for sample in [0.25_f32, -0.25, 0.5, -0.5, 0.75, -0.75] {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();

        let source = WavFrameSource::open(&float_path).unwrap();
        assert_eq!(source.path(), float_path);
        assert_eq!(source.sample_rate(), 44_100);
        assert_eq!(source.channel_layout(), ChannelLayout::Stereo);
        assert_eq!(source.frame_count(), 3);
        let mut output = [0.0; 4];
        assert_eq!(source.read_interleaved(1, &mut output).unwrap(), 2);
        assert_eq!(
            output.map(f32::to_bits),
            [0.5_f32, -0.5, 0.75, -0.75].map(f32::to_bits)
        );
        assert_eq!(source.read_interleaved(0, &mut output[..2]).unwrap(), 1);
        assert_eq!(
            output[..2]
                .iter()
                .copied()
                .map(f32::to_bits)
                .collect::<Vec<_>>(),
            [0.25_f32, -0.25].map(f32::to_bits)
        );

        let integer_path = directory.join("integer.wav");
        let mut writer = hound::WavWriter::create(
            &integer_path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 48_000,
                bits_per_sample: 24,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for sample in [-8_388_608_i32, 4_194_304, 8_388_607] {
            writer.write_sample(sample).unwrap();
        }
        writer.finalize().unwrap();
        let source = WavFrameSource::open(integer_path).unwrap();
        let mut output = [0.0; 3];
        assert_eq!(source.read_interleaved(0, &mut output).unwrap(), 3);
        assert_eq!(
            output.map(f32::to_bits),
            [-1.0_f32, 0.5, 8_388_607.0 / 8_388_608.0].map(f32::to_bits)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[derive(Debug)]
    struct CountedFrameSource {
        frames: u64,
        reads: AtomicUsize,
    }

    impl FrameSource for CountedFrameSource {
        fn frame_count(&self) -> u64 {
            self.frames
        }

        fn channel_layout(&self) -> ChannelLayout {
            ChannelLayout::Mono
        }

        fn read_interleaved(
            &self,
            start_frame: u64,
            output: &mut [f32],
        ) -> Result<usize, AssetError> {
            self.reads.fetch_add(1, AtomicOrdering::Relaxed);
            let frames = usize::try_from(
                self.frames
                    .saturating_sub(start_frame)
                    .min(output.len() as u64),
            )
            .unwrap_or(output.len());
            for (index, sample) in output[..frames].iter_mut().enumerate() {
                *sample = f32::from(u16::try_from(start_frame + index as u64).unwrap());
            }
            Ok(frames)
        }
    }

    #[test]
    fn paged_source_is_positional_and_evicts_least_recently_used_pages() {
        let underlying = Arc::new(CountedFrameSource {
            frames: 10,
            reads: AtomicUsize::new(0),
        });
        let source =
            PagedFrameSource::new(Arc::clone(&underlying) as Arc<dyn FrameSource>, 3, 2).unwrap();
        let mut sample = [0.0];
        source.read_interleaved(0, &mut sample).unwrap();
        source.read_interleaved(3, &mut sample).unwrap();
        assert_eq!(underlying.reads.load(AtomicOrdering::Relaxed), 2);
        assert_eq!(source.residency().page_indices, vec![0, 1]);

        source.read_interleaved(0, &mut sample).unwrap();
        assert_eq!(underlying.reads.load(AtomicOrdering::Relaxed), 2);
        assert_eq!(source.residency().page_indices, vec![1, 0]);
        source.read_interleaved(6, &mut sample).unwrap();
        let residency = source.residency();
        assert_eq!(residency.page_indices, vec![0, 2]);
        assert_eq!(residency.resident_pages, 2);
        assert_eq!(residency.resident_frames, 6);
        assert_eq!(underlying.reads.load(AtomicOrdering::Relaxed), 3);

        source.read_interleaved(3, &mut sample).unwrap();
        assert_eq!(source.residency().page_indices, vec![2, 1]);
        assert_eq!(underlying.reads.load(AtomicOrdering::Relaxed), 4);
        source.clear_resident();
        assert_eq!(source.residency().resident_pages, 0);
    }

    #[test]
    fn paged_source_reads_across_pages_and_caps_final_page_residency() {
        let underlying = Arc::new(CountedFrameSource {
            frames: 8,
            reads: AtomicUsize::new(0),
        });
        let source = PagedFrameSource::new(underlying, 3, 2).unwrap();
        let mut output = [0.0; 7];
        assert_eq!(source.read_interleaved(2, &mut output).unwrap(), 6);
        assert_eq!(&output[..6], &[2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        assert_eq!(source.residency().page_indices, vec![1, 2]);
        assert_eq!(source.residency().resident_frames, 5);
    }

    #[test]
    fn paged_source_rejects_unbounded_configuration() {
        let source =
            Arc::new(MemoryFrameSource::new(ChannelLayout::Mono, [0.0].as_slice()).unwrap());
        assert!(matches!(
            PagedFrameSource::new(Arc::clone(&source) as Arc<dyn FrameSource>, 0, 1),
            Err(AssetError::InvalidPageFrames)
        ));
        assert!(matches!(
            PagedFrameSource::new(source, 1, 0),
            Err(AssetError::InvalidResidentPageCapacity)
        ));
    }

    #[test]
    fn revision_keys_are_deterministic_and_context_sensitive() {
        let dependencies = [DependencyRevision::new("input", "abc")];
        let first = RevisionId::derive(b"gain=1", &dependencies, &context(ChannelLayout::Mono));
        let again = RevisionId::derive(b"gain=1", &dependencies, &context(ChannelLayout::Mono));
        let changed = RevisionId::derive(b"gain=2", &dependencies, &context(ChannelLayout::Mono));
        assert_eq!(first, again);
        assert_ne!(first, changed);
        let tempo = context(ChannelLayout::Mono)
            .with_timeline(90.0, None)
            .unwrap();
        let range = context(ChannelLayout::Mono)
            .with_timeline(
                120.0,
                Some(RequestedFrameRange {
                    start_frame: 64,
                    frame_count: 128,
                }),
            )
            .unwrap();
        assert_ne!(first, RevisionId::derive(b"gain=1", &dependencies, &tempo));
        assert_ne!(first, RevisionId::derive(b"gain=1", &dependencies, &range));
        assert_eq!(first.as_str().len(), 32);
    }

    #[test]
    fn registry_invalidates_transitive_dependents_and_keeps_fallbacks() {
        let registry = AssetRegistry::new();
        let source_v1 = revision("source", "v1", ChannelLayout::Mono, &[0.0], vec![]);
        let child = revision(
            "child",
            "child-v1",
            ChannelLayout::Mono,
            &[0.5],
            vec![DependencyRevision::new("source", "v1")],
        );
        let root = revision(
            "root",
            "root-v1",
            ChannelLayout::Mono,
            &[1.0],
            vec![DependencyRevision::new("child", "child-v1")],
        );
        registry.publish(source_v1);
        registry.publish(child.clone());
        registry.publish(root.clone());

        let source_v2 = revision("source", "v2", ChannelLayout::Mono, &[0.1], vec![]);
        let invalidated = registry.publish(source_v2);
        assert_eq!(invalidated.len(), 2);
        assert!(invalidated.contains(&AssetId::from("child")));
        assert!(invalidated.contains(&AssetId::from("root")));
        assert!(registry.current(&AssetId::from("child")).is_none());
        let resolved = registry.resolve(&AssetId::from("root")).unwrap();
        assert_eq!(resolved.freshness, RevisionFreshness::LastValid);
        assert!(Arc::ptr_eq(&resolved.revision, &root));

        registry.publish(revision(
            "child",
            "child-v2",
            ChannelLayout::Mono,
            &[0.6],
            vec![DependencyRevision::new("source", "v2")],
        ));
        assert_eq!(
            registry.resolve(&AssetId::from("child")).unwrap().freshness,
            RevisionFreshness::Current
        );
    }

    #[test]
    fn republishing_stale_asset_replaces_reverse_dependency_edges() {
        let registry = AssetRegistry::new();
        let input_a = revision("a", "a1", ChannelLayout::Mono, &[0.0], vec![]);
        let input_b = revision("b", "b1", ChannelLayout::Mono, &[0.0], vec![]);
        let derived_a = revision(
            "derived",
            "d1",
            ChannelLayout::Mono,
            &[0.0],
            vec![DependencyRevision::new("a", "a1")],
        );
        registry.publish(input_a);
        registry.publish(input_b);
        registry.publish(derived_a);
        registry.invalidate(&AssetId::from("a"));

        let derived_b = revision(
            "derived",
            "d2",
            ChannelLayout::Mono,
            &[0.0],
            vec![DependencyRevision::new("b", "b1")],
        );
        registry.publish(derived_b);
        registry.publish(revision("a", "a2", ChannelLayout::Mono, &[0.0], vec![]));
        assert!(registry.current(&AssetId::from("derived")).is_some());

        registry.publish(revision("b", "b2", ChannelLayout::Mono, &[0.0], vec![]));
        assert!(registry.current(&AssetId::from("derived")).is_none());
    }

    #[test]
    fn late_background_render_cannot_publish_stale_dependencies_as_current() {
        let registry = AssetRegistry::new();
        registry.publish(revision(
            "source",
            "v1",
            ChannelLayout::Mono,
            &[0.0],
            vec![],
        ));
        registry.publish(revision(
            "source",
            "v2",
            ChannelLayout::Mono,
            &[0.0],
            vec![],
        ));

        let late = revision(
            "derived",
            "derived-from-v1",
            ChannelLayout::Mono,
            &[0.5],
            vec![DependencyRevision::new("source", "v1")],
        );
        registry.publish(Arc::clone(&late));
        assert!(registry.current(&AssetId::from("derived")).is_none());
        assert!(Arc::ptr_eq(
            &registry.last_valid(&AssetId::from("derived")).unwrap(),
            &late
        ));
    }

    #[test]
    fn stale_publish_does_not_rewire_a_current_revision() {
        let registry = AssetRegistry::new();
        registry.publish(revision("a", "a1", ChannelLayout::Mono, &[0.0], vec![]));
        registry.publish(revision("b", "b1", ChannelLayout::Mono, &[0.0], vec![]));
        let current = revision(
            "derived",
            "current",
            ChannelLayout::Mono,
            &[0.0],
            vec![DependencyRevision::new("a", "a1")],
        );
        registry.publish(Arc::clone(&current));

        registry.publish(revision(
            "derived",
            "late",
            ChannelLayout::Mono,
            &[0.0],
            vec![DependencyRevision::new("b", "missing")],
        ));
        assert!(Arc::ptr_eq(
            &registry.current(&AssetId::from("derived")).unwrap(),
            &current
        ));

        registry.publish(revision("a", "a2", ChannelLayout::Mono, &[0.0], vec![]));
        assert!(registry.current(&AssetId::from("derived")).is_none());
    }

    #[test]
    fn stale_completion_does_not_replace_the_last_playable_fallback() {
        let registry = AssetRegistry::new();
        registry.publish(revision(
            "source",
            "v1",
            ChannelLayout::Mono,
            &[0.0],
            vec![],
        ));
        let fallback = revision(
            "derived",
            "from-v1",
            ChannelLayout::Mono,
            &[0.25],
            vec![DependencyRevision::new("source", "v1")],
        );
        registry.publish(Arc::clone(&fallback));
        registry.publish(revision(
            "source",
            "v2",
            ChannelLayout::Mono,
            &[0.0],
            vec![],
        ));
        registry.publish(revision(
            "derived",
            "late-from-missing",
            ChannelLayout::Mono,
            &[0.75],
            vec![DependencyRevision::new("source", "missing")],
        ));

        assert!(Arc::ptr_eq(
            &registry.last_valid(&AssetId::from("derived")).unwrap(),
            &fallback
        ));
    }

    #[test]
    fn waveform_preserves_channel_peaks_and_partial_final_bucket() {
        let revision = revision(
            "stereo",
            "v1",
            ChannelLayout::Stereo,
            &[1.0, -1.0, -0.5, 0.25, 0.75, 0.5],
            vec![],
        );
        let waveform = Waveform::generate(&revision, 2).unwrap();
        assert_eq!(waveform.buckets.len(), 2);
        assert_eq!(waveform.buckets[0].frame_count, 2);
        assert_eq!(
            waveform.buckets[0].peaks[0].minimum.to_bits(),
            (-0.5_f32).to_bits()
        );
        assert_eq!(
            waveform.buckets[0].peaks[0].maximum.to_bits(),
            1.0_f32.to_bits()
        );
        assert_eq!(
            waveform.buckets[0].peaks[1].minimum.to_bits(),
            (-1.0_f32).to_bits()
        );
        assert_eq!(
            waveform.buckets[0].peaks[1].maximum.to_bits(),
            0.25_f32.to_bits()
        );
        assert_eq!(waveform.buckets[1].frame_count, 1);
        assert_eq!(waveform.buckets[1].first_frame, 2);
    }

    #[test]
    fn materialization_is_deterministic_and_reuses_valid_wav() {
        let directory = temporary_directory("materialize");
        let materializer = Materializer::new(&directory);
        let revision = revision(
            "tone",
            "v1",
            ChannelLayout::Stereo,
            &[0.25, -0.25, 0.5, -0.5],
            vec![],
        );
        let first = materializer.materialize(&revision).unwrap();
        let bytes = fs::read(&first.path).unwrap();
        let second = materializer.materialize(&revision).unwrap();
        assert_eq!(first, second);
        assert_eq!(bytes, fs::read(&second.path).unwrap());

        let mut reader = hound::WavReader::open(&first.path).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.duration(), 2);
        let samples = reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(samples, vec![0.25, -0.25, 0.5, -0.5]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn materializer_replaces_same_shape_wav_with_wrong_content() {
        let directory = temporary_directory("content-identity");
        let materializer = Materializer::new(&directory);
        let revision = revision("tone", "v1", ChannelLayout::Mono, &[0.25, 0.5], vec![]);
        let result = materializer.materialize(&revision).unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&result.path, spec).unwrap();
        writer.write_sample(-0.25_f32).unwrap();
        writer.write_sample(-0.5_f32).unwrap();
        writer.finalize().unwrap();
        materializer.materialize(&revision).unwrap();
        let samples = hound::WavReader::open(&result.path)
            .unwrap()
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(samples, vec![0.25, 0.5]);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn bounded_worker_materializes_and_generates_waveforms() {
        let directory = temporary_directory("worker");
        let registry = Arc::new(AssetRegistry::new());
        let worker = BackgroundAssetWorker::spawn(
            Materializer::new(&directory),
            Arc::clone(&registry),
            2,
            2,
        )
        .unwrap();
        let revision = revision("tone", "v1", ChannelLayout::Mono, &[0.1, 0.2], vec![]);
        worker
            .try_request(AssetRequest::GenerateWaveform {
                request_id: 41,
                revision,
                frames_per_bucket: 1,
            })
            .unwrap();
        let response = worker
            .response_receiver()
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(response.request_id, 41);
        assert!(matches!(response.result, Ok(AssetProduct::Waveform(_))));
        drop(worker);
        let _ = fs::remove_dir_all(directory);
    }
}
