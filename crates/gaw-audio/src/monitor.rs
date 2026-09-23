//! Independent, bounded live input monitoring. Never part of a render snapshot.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering},
};

use cpal::{
    FromSample, Sample, SampleFormat, SizedSample,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use crossbeam_channel::{Receiver, Sender};
use thiserror::Error;

use crate::tuner::{BassTunerReading, Identity, Sample as TunerSample, Tuner};

mod queue;
use queue::{InputQueueReader, backlog_limit};

const CAPACITY: usize = 16_384;
const INPUT_BLOCK_FRAMES: usize = 128;
const QUEUE_BLOCKS: usize = CAPACITY / INPUT_BLOCK_FRAMES;
const EFFECT_BLOCK_FRAMES: usize = 512;
const MAX_JITTER_FRAMES: usize = 256;
pub const MAX_INPUT_EFFECTS: usize = 16;

#[derive(Clone, Copy, Debug)]
struct InputBlock {
    samples: [f32; INPUT_BLOCK_FRAMES],
    len: usize,
    epoch: u64,
    stream: u64,
}

#[derive(Debug)]
struct Shared {
    enabled: AtomicBool,
    tuner: Tuner,
    gain: AtomicU32,
    peak: AtomicU32,
    callback_frames: AtomicUsize,
    resampling: AtomicBool,
    queued_frames: AtomicUsize,
    in_flight_frames: AtomicUsize,
    publication_generation: AtomicU64,
    eviction_claim: AtomicBool,
    effect_latency_frames: AtomicU64,
    dropped_frames: AtomicU64,
    underrun_frames: AtomicU64,
    epoch: AtomicU64,
    stream: AtomicU64,
    sender: Sender<InputBlock>,
    receiver: Receiver<InputBlock>,
    effects_sender: Sender<PreparedInputEffects>,
    effects_receiver: Receiver<PreparedInputEffects>,
    retired_sender: Sender<PreparedInputEffects>,
    retired_receiver: Receiver<PreparedInputEffects>,
    effects_publish: parking_lot::Mutex<()>,
}

impl Shared {
    fn available_frames(&self) -> usize {
        // The reservation precedes publication. Read total first, then subtract
        // any still-unpublished frames so trimming cannot discard audible input
        // to make room for a packet the consumer cannot read yet.
        self.queued_frames
            .load(Ordering::Acquire)
            .saturating_sub(self.in_flight_frames.load(Ordering::Acquire))
    }
}

/// Thread-safe monitoring controls, shared with the input and output callbacks.
/// New controls start muted. Attach to just one output engine at a time.
#[derive(Clone, Debug)]
pub struct InputMonitorControl(Arc<Shared>);

/// Approximate callback/queue telemetry. Frame counts use the output sample rate;
/// these do not include the device/OS buffers and are not round-trip measurements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InputMonitorLatencyStatus {
    pub input_callback_frames: usize,
    pub queued_frames: usize,
    pub effect_latency_frames: u64,
    pub dropped_frames: u64,
    pub underrun_frames: u64,
}

impl Default for InputMonitorControl {
    fn default() -> Self {
        Self::new()
    }
}

impl InputMonitorControl {
    pub fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(QUEUE_BLOCKS);
        let (effects_sender, effects_receiver) = crossbeam_channel::bounded(1);
        let (retired_sender, retired_receiver) = crossbeam_channel::bounded(2);
        Self(Arc::new(Shared {
            enabled: AtomicBool::new(false),
            tuner: Tuner::new(),
            gain: AtomicU32::new(1.0_f32.to_bits()),
            peak: AtomicU32::new(0),
            callback_frames: AtomicUsize::new(0),
            resampling: AtomicBool::new(false),
            queued_frames: AtomicUsize::new(0),
            in_flight_frames: AtomicUsize::new(0),
            publication_generation: AtomicU64::new(0),
            eviction_claim: AtomicBool::new(false),
            effect_latency_frames: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            underrun_frames: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            stream: AtomicU64::new(0),
            sender,
            receiver,
            effects_sender,
            effects_receiver,
            retired_sender,
            retired_receiver,
            effects_publish: parking_lot::Mutex::new(()),
        }))
    }

    /// Disabling invalidates buffered samples, even if enabled again before
    /// either audio callback runs.
    pub fn set_enabled(&self, enabled: bool) {
        if enabled {
            self.0.enabled.store(true, Ordering::Release);
        } else {
            self.0.enabled.store(false, Ordering::Release);
            self.set_tuner_enabled(false);
            self.0.epoch.fetch_add(1, Ordering::AcqRel);
            self.0.peak.store(0, Ordering::Relaxed);
        }
    }

    pub fn enabled(&self) -> bool {
        self.0.enabled.load(Ordering::Acquire)
    }

    /// Enables a raw-input tap for the standard E1/A1/D2/G2 bass tuner.
    /// Monitoring must also be enabled; disabling monitoring closes the tuner.
    pub fn set_tuner_enabled(&self, enabled: bool) {
        self.0.tuner.set_enabled(enabled && self.enabled());
    }

    /// Analyzes recent input on the caller thread, throttled to approximately 22 Hz.
    /// Call from the UI/control thread, never an audio callback. Silence, disabled
    /// monitoring and stale or replaced streams return `None`.
    pub fn tuner_reading(&self) -> Option<BassTunerReading> {
        if !self.enabled() {
            return None;
        }
        let epoch = self.0.epoch.load(Ordering::Acquire);
        let stream = self.0.stream.load(Ordering::Acquire);
        let reading = self.0.tuner.reading(epoch, stream);
        if self.enabled()
            && self.0.epoch.load(Ordering::Acquire) == epoch
            && self.0.stream.load(Ordering::Acquire) == stream
        {
            reading
        } else {
            None
        }
    }

    /// Linear monitoring gain, limited to 0..=4. Invalid values mute the input.
    pub fn set_gain(&self, gain: f32) {
        let gain = if gain.is_finite() {
            gain.clamp(0.0, 4.0)
        } else {
            0.0
        };
        self.0.gain.store(gain.to_bits(), Ordering::Relaxed);
    }

    /// Takes the maximum post-monitor-gain input peak since the previous read.
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.0.peak.swap(0, Ordering::Relaxed))
    }

    pub fn latency_status(&self) -> InputMonitorLatencyStatus {
        InputMonitorLatencyStatus {
            input_callback_frames: self.0.callback_frames.load(Ordering::Relaxed),
            queued_frames: self.0.available_frames(),
            effect_latency_frames: self.0.effect_latency_frames.load(Ordering::Relaxed),
            dropped_frames: self.0.dropped_frames.load(Ordering::Relaxed),
            underrun_frames: self.0.underrun_frames.load(Ordering::Relaxed),
        }
    }

    fn start_stream(&self) -> u64 {
        self.0.peak.store(0, Ordering::Relaxed);
        self.0.callback_frames.store(0, Ordering::Relaxed);
        self.0.dropped_frames.store(0, Ordering::Relaxed);
        self.0.underrun_frames.store(0, Ordering::Relaxed);
        self.0.stream.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn finish_stream(&self, stream: u64) {
        if self
            .0
            .stream
            .compare_exchange(stream, stream + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.0.peak.store(0, Ordering::Relaxed);
        }
    }

    /// Prepares the complete live input chain on a control/worker thread.
    /// A newer request replaces a queued request; the current chain remains active
    /// if validation or preparation fails. Never call from an audio callback.
    ///
    /// # Errors
    /// Rejects invalid effects, analyzers, more than 16 effects, and invalid audio settings.
    pub fn configure_effects(
        &self,
        processors: &[gaw_core::Processor],
        bypass: bool,
        sample_rate: u32,
        tempo_bpm: f64,
    ) -> Result<(), String> {
        let prepared = PreparedInputEffects::new(processors, bypass, sample_rate, tempo_bpm)?;
        let _guard = self.0.effects_publish.lock();
        self.collect_retired_effects();
        // Superseded configurations are destroyed here, never in the callback.
        let _ = self.0.effects_receiver.try_recv();
        self.0
            .effects_sender
            .try_send(prepared)
            .map_err(|_| "Could not publish input effects".to_owned())
    }

    /// Reclaims replaced DSP chains on the caller thread. Call periodically from
    /// the input worker, including while monitoring is disabled.
    pub fn collect_retired_effects(&self) {
        for _ in 0..2 {
            if self.0.retired_receiver.try_recv().is_err() {
                break;
            }
        }
    }
}

struct InputEffect {
    processor: Box<dyn gaw_dsp::Processor>,
    output_channels: usize,
}

struct PreparedInputEffects {
    effects: Vec<InputEffect>,
    bypass: bool,
    sample_rate: u32,
    tempo_bpm: f64,
    latency_frames: u64,
}

impl std::fmt::Debug for PreparedInputEffects {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedInputEffects")
            .field("count", &self.effects.len())
            .field("bypass", &self.bypass)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl PreparedInputEffects {
    fn new(
        processors: &[gaw_core::Processor],
        bypass: bool,
        sample_rate: u32,
        tempo_bpm: f64,
    ) -> Result<Self, String> {
        if processors.len() > MAX_INPUT_EFFECTS {
            return Err(format!(
                "Live input supports at most {MAX_INPUT_EFFECTS} effects"
            ));
        }
        let spec = gaw_dsp::PrepareSpec {
            sample_rate: f64::from(sample_rate),
            max_block_size: EFFECT_BLOCK_FRAMES,
            input_layout: gaw_core::ChannelLayout::Stereo,
            tempo_bpm,
        };
        spec.validate().map_err(|error| error.to_string())?;
        let mut effects = Vec::with_capacity(processors.len());
        let mut latency_frames = 0_u64;
        for definition in processors {
            definition.validate().map_err(|error| error.to_string())?;
            if definition.kind.is_analyzer() {
                return Err("Analyzers are not supported in live input effect chains".to_owned());
            }
            // Live chains are immutable snapshots: there is no automation that
            // could activate this neutral pitch/dry path after preparation.
            let neutral = match &definition.kind {
                gaw_core::processors::ProcessorKind::PitchShift(parameters) => {
                    i16::from(parameters.semitones) * 100 + parameters.cents == 0
                        || parameters.mix == 0.0
                }
                gaw_core::processors::ProcessorKind::Saturator(parameters) => parameters.mix == 0.0,
                _ => false,
            };
            if !definition.enabled || bypass || neutral {
                continue;
            }
            let mut processor =
                crate::project::create_processor(definition, sample_rate, tempo_bpm, 0)
                    .map_err(|error| error.to_string())?;
            let output_channels = processor
                .output_layout(spec.input_layout)
                .map_err(|error| error.to_string())?
                .channels();
            processor.prepare(spec).map_err(|error| error.to_string())?;
            latency_frames += u64::from(processor.latency_frames());
            effects.push(InputEffect {
                processor,
                output_channels,
            });
        }
        Ok(Self {
            effects,
            bypass,
            sample_rate,
            tempo_bpm,
            latency_frames,
        })
    }
}

/// Single output-callback owner of the live DSP state. All queue operations are
/// nonblocking; replacements wait if the worker has not reclaimed old chains.
#[derive(Debug)]
pub(crate) struct InputMonitorMixer {
    control: InputMonitorControl,
    sample_rate: u32,
    active: Option<PreparedInputEffects>,
    pending_retired: Option<PreparedInputEffects>,
    identity: (u64, u64),
    absolute_frame: u64,
    audio: [[[f32; EFFECT_BLOCK_FRAMES]; 2]; 2],
    reader: InputQueueReader,
    received_input: bool,
    jitter_frames: usize,
    underrunning: bool,
}

impl InputMonitorMixer {
    pub(crate) fn new(control: InputMonitorControl, sample_rate: u32) -> Self {
        Self {
            control,
            sample_rate,
            active: None,
            pending_retired: None,
            identity: (0, 0),
            absolute_frame: 0,
            audio: [[[0.0; EFFECT_BLOCK_FRAMES]; 2]; 2],
            reader: InputQueueReader::default(),
            received_input: false,
            jitter_frames: 0,
            underrunning: false,
        }
    }

    /// Called before starting the output callback after device negotiation.
    pub(crate) fn set_sample_rate(&mut self, sample_rate: u32) {
        if self.sample_rate != sample_rate {
            self.active = None;
            self.control
                .0
                .effect_latency_frames
                .store(0, Ordering::Relaxed);
            self.absolute_frame = 0;
            self.sample_rate = sample_rate;
        }
    }

    fn update_effects(&mut self) {
        if let Some(retired) = self.pending_retired.take()
            && let Err(error) = self.control.0.retired_sender.try_send(retired)
        {
            self.pending_retired = Some(error.into_inner());
            return;
        }
        if self.control.0.retired_sender.is_full() {
            return;
        }
        if let Ok(next) = self.control.0.effects_receiver.try_recv() {
            let retired = if next.sample_rate == self.sample_rate {
                self.control
                    .0
                    .effect_latency_frames
                    .store(next.latency_frames, Ordering::Relaxed);
                self.active.replace(next)
            } else {
                Some(next)
            };
            if let Some(retired) = retired
                && let Err(error) = self.control.0.retired_sender.try_send(retired)
            {
                self.pending_retired = Some(error.into_inner());
            }
        }
    }

    fn note_underrun(&mut self, had_input: bool, underrun: bool, output_frames: usize) {
        if had_input && underrun && !self.underrunning {
            // Each distinct capture gap earns one callback of scheduling room.
            // A continuing outage cannot inflate it; stable streams add none.
            self.jitter_frames = self
                .jitter_frames
                .saturating_add(output_frames)
                .min(output_frames.saturating_mul(4))
                .min(MAX_JITTER_FRAMES);
        }
        self.underrunning = underrun;
    }

    /// Adds the processed live input independently of transport and master gain.
    pub(crate) fn mix(&mut self, output: &mut [f32], channels: usize) -> bool {
        self.update_effects();
        let shared = &self.control.0;
        let identity = (
            shared.epoch.load(Ordering::Acquire),
            shared.stream.load(Ordering::Acquire),
        );
        if identity != self.identity {
            self.identity = identity;
            self.absolute_frame = 0;
            self.received_input = false;
            self.jitter_frames = 0;
            self.underrunning = false;
            if let Some(chain) = &mut self.active {
                for effect in &mut chain.effects {
                    effect.processor.reset();
                }
            }
        }
        if !self.control.enabled() || channels == 0 {
            return false;
        }
        let gain = f32::from_bits(shared.gain.load(Ordering::Relaxed));
        let output_frames = output.len() / channels;
        let input_frames = shared.callback_frames.load(Ordering::Relaxed);
        let keep = backlog_limit(
            input_frames,
            output_frames,
            shared.resampling.load(Ordering::Relaxed),
        )
        .saturating_add(self.jitter_frames)
        .min(CAPACITY);
        let mut budget = QUEUE_BLOCKS;
        self.reader.trim_backlog(shared, keep, &mut budget);
        let had_input = self.received_input;
        let (mut peak, mut underrun_frames) = (0.0_f32, 0);
        for block in output.chunks_mut(EFFECT_BLOCK_FRAMES * channels) {
            let frames = block.len() / channels;
            let mut current = 0;
            let [left, right] = &mut self.audio[current];
            left[..frames].fill(0.0);
            let copied = self
                .reader
                .read(shared, identity, &mut left[..frames], &mut budget);
            underrun_frames += frames - copied;
            self.received_input |= copied > 0;
            right[..frames].copy_from_slice(&left[..frames]);
            if let Some(chain) = &mut self.active {
                for effect in &mut chain.effects {
                    // Swap buffer roles, not 4 KiB arrays, after each effect.
                    let (first, second) = self.audio.split_at_mut(1);
                    let (audio, scratch) = if current == 0 {
                        (&first[0], &mut second[0])
                    } else {
                        (&second[0], &mut first[0])
                    };
                    let input = [&audio[0][..frames], &audio[1][..frames]];
                    let [left, right] = scratch;
                    let mut result = [&mut left[..frames], &mut right[..frames]];
                    if effect
                        .processor
                        .process(
                            &input,
                            &mut result[..effect.output_channels],
                            &[],
                            gaw_dsp::ProcessContext {
                                absolute_frame: self.absolute_frame,
                                tempo_bpm: chain.tempo_bpm,
                            },
                        )
                        .is_err()
                    {
                        // A DSP failure mutes this block without formatting or allocating.
                        scratch[0][..frames].fill(0.0);
                        scratch[1][..frames].fill(0.0);
                    } else if effect.output_channels == 1 {
                        let [left, right] = scratch;
                        right[..frames].copy_from_slice(&left[..frames]);
                    }
                    current ^= 1;
                }
            }
            for (index, frame) in block.chunks_exact_mut(channels).enumerate() {
                for (channel, sample) in frame.iter_mut().enumerate() {
                    let wet = if channels == 1 {
                        (self.audio[current][0][index] + self.audio[current][1][index]) * 0.5
                    } else {
                        self.audio[current][channel % 2][index]
                    };
                    let value = wet * gain;
                    let value = if value.is_finite() { value } else { 0.0 };
                    *sample += value;
                    peak = peak.max(value.abs());
                }
            }
            self.absolute_frame = self.absolute_frame.saturating_add(frames as u64);
        }
        shared
            .underrun_frames
            .fetch_add(underrun_frames as u64, Ordering::Relaxed);
        shared.peak.fetch_max(peak.to_bits(), Ordering::Relaxed);
        self.note_underrun(had_input, underrun_frames > 0, output_frames);
        peak > 0.0
    }
}

impl Drop for InputMonitorMixer {
    fn drop(&mut self) {
        self.reader.release(&self.control.0);
    }
}

/// Negotiated capture configuration. `channel` is zero based.
#[derive(Clone, Debug)]
pub struct InputMonitorInfo {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub channel: usize,
}

/// Owns an already-playing input stream. Dropping it invalidates queued audio.
pub struct CpalInputMonitor {
    _stream: cpal::Stream,
    info: InputMonitorInfo,
    control: InputMonitorControl,
    stream_id: u64,
    error: Arc<AtomicU32>,
}

impl std::fmt::Debug for CpalInputMonitor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CpalInputMonitor")
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

impl CpalInputMonitor {
    /// Opens and starts capture without changing the control's enabled state.
    ///
    /// # Errors
    /// Returns an error if the device, channel or stream cannot be opened.
    pub fn open(
        device_id: Option<&cpal::DeviceId>,
        output_sample_rate: u32,
        buffer_frames: Option<u32>,
        channel: usize,
        control: InputMonitorControl,
    ) -> Result<Self, InputMonitorError> {
        if output_sample_rate == 0 || buffer_frames == Some(0) {
            return Err(InputMonitorError::InvalidConfig);
        }
        let host = if let Some(id) = device_id {
            cpal::host_from_id(id.0).map_err(|_| InputMonitorError::DeviceUnavailable)?
        } else {
            cpal::default_host()
        };
        let device = if let Some(id) = device_id {
            host.device_by_id(id)
        } else {
            host.default_input_device()
        }
        .ok_or(InputMonitorError::DeviceUnavailable)?;
        let chosen = select_input_config(
            device.supported_input_configs()?,
            output_sample_rate,
            channel,
        )?;
        let info = InputMonitorInfo {
            device_name: device.description().map_or_else(
                |_| "Audio input".into(),
                |description| description.name().to_owned(),
            ),
            sample_rate: chosen.sample_rate(),
            channels: chosen.channels(),
            channel,
        };
        let mut config = chosen.config();
        // Output and input devices need not accept the same fixed buffer size.
        if let Some(frames) = buffer_frames {
            config.buffer_size = match chosen.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } => {
                    cpal::BufferSize::Fixed(frames.clamp(*min, *max))
                }
                cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Fixed(frames),
            };
        }
        let stream_id = control.start_stream();
        let error = Arc::new(AtomicU32::new(0));
        macro_rules! build {
            ($sample:ty) => {
                build_with_input_buffer_fallback(
                    &config,
                    matches!(chosen.buffer_size(), cpal::SupportedBufferSize::Unknown),
                    |config| {
                        build_input::<$sample>(
                            &device,
                            config,
                            channel,
                            InputProducer::new(
                                control.clone(),
                                stream_id,
                                info.sample_rate,
                                output_sample_rate,
                            ),
                            Arc::clone(&error),
                        )
                    },
                )
            };
        }
        let result = match chosen.sample_format() {
            SampleFormat::I8 => build!(i8),
            SampleFormat::I16 => build!(i16),
            SampleFormat::I24 => build!(cpal::I24),
            SampleFormat::I32 => build!(i32),
            SampleFormat::I64 => build!(i64),
            SampleFormat::U8 => build!(u8),
            SampleFormat::U16 => build!(u16),
            SampleFormat::U24 => build!(cpal::U24),
            SampleFormat::U32 => build!(u32),
            SampleFormat::U64 => build!(u64),
            SampleFormat::F32 => build!(f32),
            SampleFormat::F64 => build!(f64),
            _ => return Err(InputMonitorError::InvalidConfig),
        };
        let stream = result.inspect_err(|_| control.finish_stream(stream_id))?;
        stream
            .play()
            .inspect_err(|_| control.finish_stream(stream_id))?;
        Ok(Self {
            _stream: stream,
            info,
            control,
            stream_id,
            error,
        })
    }

    pub fn info(&self) -> &InputMonitorInfo {
        &self.info
    }

    /// Formats a coalesced stream error on the control thread, never in a callback.
    pub fn take_error(&self) -> Option<String> {
        match self.error.swap(0, Ordering::AcqRel) {
            0 => None,
            1 => Some("Input device disconnected or became unavailable".into()),
            2 => Some("Input stream was invalidated; restart monitoring".into()),
            _ => Some("Input audio stream failed; restart monitoring".into()),
        }
    }
}

impl Drop for CpalInputMonitor {
    fn drop(&mut self) {
        self.control.finish_stream(self.stream_id);
    }
}

#[derive(Debug, Error)]
pub enum InputMonitorError {
    #[error("the selected input device is unavailable")]
    DeviceUnavailable,
    #[error("invalid input monitoring configuration")]
    InvalidConfig,
    #[error(
        "the selected device has no usable audio capture format; try System default or another input device"
    )]
    NoUsableInputConfig,
    #[error("input device does not support input channel {channel}")]
    NoMatchingConfig { channel: usize },
    #[error("could not enumerate input configurations: {0}")]
    SupportedConfigs(#[from] cpal::SupportedStreamConfigsError),
    #[error("could not build input stream: {0}")]
    BuildStream(#[from] cpal::BuildStreamError),
    #[error("could not start input stream: {0}")]
    PlayStream(#[from] cpal::PlayStreamError),
}

/// Shared capability policy for the input catalog and monitor stream selection.
pub(crate) fn usable_input_config(config: &cpal::SupportedStreamConfigRange) -> bool {
    config.channels() > 0
        && config.min_sample_rate() > 0
        && config.min_sample_rate() <= config.max_sample_rate()
        && supported_format(config.sample_format())
}

fn select_input_config(
    configurations: impl Iterator<Item = cpal::SupportedStreamConfigRange>,
    output_sample_rate: u32,
    channel: usize,
) -> Result<cpal::SupportedStreamConfig, InputMonitorError> {
    let configurations: Vec<_> = configurations.filter(usable_input_config).collect();
    if configurations.is_empty() {
        return Err(InputMonitorError::NoUsableInputConfig);
    }
    if !configurations
        .iter()
        .any(|config| usize::from(config.channels()) > channel)
    {
        return Err(InputMonitorError::NoMatchingConfig {
            channel: channel.saturating_add(1),
        });
    }
    configurations
        .into_iter()
        .filter(|config| usize::from(config.channels()) > channel)
        .filter_map(|config| {
            let rate = output_sample_rate.clamp(config.min_sample_rate(), config.max_sample_rate());
            // Keep callback resampling work bounded even for pathological configurations.
            if f64::from(output_sample_rate) / f64::from(rate) > 16.0 {
                return None;
            }
            let rank = (
                rate.abs_diff(output_sample_rate),
                config.channels(),
                format_rank(config.sample_format()),
            );
            Some((rank, config.with_sample_rate(rate)))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, config)| config)
        .ok_or(InputMonitorError::NoUsableInputConfig)
}

fn supported_format(format: SampleFormat) -> bool {
    matches!(
        format,
        SampleFormat::I8
            | SampleFormat::I16
            | SampleFormat::I24
            | SampleFormat::I32
            | SampleFormat::I64
            | SampleFormat::U8
            | SampleFormat::U16
            | SampleFormat::U24
            | SampleFormat::U32
            | SampleFormat::U64
            | SampleFormat::F32
            | SampleFormat::F64
    )
}

fn format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::F32 => 0,
        SampleFormat::I16 => 1,
        _ => 2,
    }
}

struct InputProducer {
    control: InputMonitorControl,
    stream: u64,
    epoch: u64,
    previous: Option<f32>,
    phase: f64,
    step: f64,
    same_rate: bool,
    tuner_generation: Option<u64>,
    tuner_sum: f32,
    tuner_count: u32,
    tuner_divisor: u32,
    tuner_rate: f32,
    tuner_sequence: u64,
}

impl InputProducer {
    #[allow(clippy::cast_precision_loss)]
    fn new(control: InputMonitorControl, stream: u64, input_rate: u32, output_rate: u32) -> Self {
        let epoch = control.0.epoch.load(Ordering::Acquire);
        Self {
            control,
            stream,
            epoch,
            previous: None,
            phase: 0.0,
            step: f64::from(input_rate) / f64::from(output_rate),
            same_rate: input_rate == output_rate,
            tuner_generation: None,
            tuner_sum: 0.0,
            tuner_count: 0,
            tuner_divisor: (input_rate / 4000).max(1),
            tuner_rate: input_rate as f32 / (input_rate / 4000).max(1) as f32,
            tuner_sequence: 0,
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss
    )]
    fn capture<T: SizedSample>(&mut self, data: &[T], channels: usize, channel: usize)
    where
        f32: FromSample<T>,
    {
        if !self.control.enabled() || self.control.0.stream.load(Ordering::Acquire) != self.stream {
            self.previous = None;
            self.phase = 0.0;
            return;
        }
        let epoch = self.control.0.epoch.load(Ordering::Acquire);
        if epoch != self.epoch {
            self.epoch = epoch;
            self.previous = None;
            self.phase = 0.0;
        }
        let tuner_generation = self.control.0.tuner.generation();
        if tuner_generation != self.tuner_generation {
            self.tuner_generation = tuner_generation;
            self.tuner_sum = 0.0;
            self.tuner_count = 0;
        }
        let captured = tuner_generation.map(|_| std::time::Instant::now());
        let callback_frames = ((data.len() / channels) as f64 / self.step).ceil() as usize;
        self.control
            .0
            .resampling
            .store(!self.same_rate, Ordering::Relaxed);
        self.control
            .0
            .callback_frames
            .store(callback_frames.min(CAPACITY), Ordering::Relaxed);
        let mut block = InputBlock {
            samples: [0.0; INPUT_BLOCK_FRAMES],
            len: 0,
            epoch,
            stream: self.stream,
        };
        for frame in data.chunks_exact(channels) {
            let value = f32::from_sample(frame[channel]);
            let value = if value.is_finite() {
                value.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            if let (Some(generation), Some(captured)) = (tuner_generation, captured) {
                self.tuner_sum += value;
                self.tuner_count += 1;
                if self.tuner_count == self.tuner_divisor {
                    self.control.0.tuner.push(TunerSample {
                        value: self.tuner_sum / self.tuner_divisor as f32,
                        identity: Identity {
                            epoch,
                            stream: self.stream,
                            generation,
                        },
                        sequence: self.tuner_sequence,
                        rate: self.tuner_rate,
                        captured,
                    });
                    self.tuner_sequence = self.tuner_sequence.wrapping_add(1);
                    self.tuner_sum = 0.0;
                    self.tuner_count = 0;
                }
            }
            if self.same_rate {
                // Identical rates need neither interpolation nor its one-sample wait.
                self.push_sample(&mut block, value);
            } else {
                if let Some(previous) = self.previous {
                    while self.phase < 1.0 {
                        let interpolated = previous + (value - previous) * self.phase as f32;
                        self.push_sample(&mut block, interpolated);
                        self.phase += self.step;
                    }
                    self.phase -= 1.0;
                }
                self.previous = Some(value);
            }
        }
        // Never hold a partial packet for the next capture callback.
        if block.len > 0 {
            self.publish(&block);
        }
    }

    fn push_sample(&self, block: &mut InputBlock, value: f32) {
        block.samples[block.len] = value;
        block.len += 1;
        if block.len == INPUT_BLOCK_FRAMES {
            self.publish(block);
            block.len = 0;
        }
    }

    fn publish(&self, block: &InputBlock) {
        let shared = &self.control.0;
        // Account before publication so a concurrent reader cannot underflow.
        shared.publication_generation.fetch_add(1, Ordering::AcqRel);
        shared
            .in_flight_frames
            .fetch_add(block.len, Ordering::AcqRel);
        shared.queued_frames.fetch_add(block.len, Ordering::AcqRel);
        if let Err(error) = shared.sender.try_send(*block) {
            if shared
                .eviction_claim
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                if let Ok(old) = shared.receiver.try_recv() {
                    shared.queued_frames.fetch_sub(old.len, Ordering::AcqRel);
                    shared
                        .dropped_frames
                        .fetch_add(old.len as u64, Ordering::Relaxed);
                }
                shared.eviction_claim.store(false, Ordering::Release);
            }
            if shared.sender.try_send(error.into_inner()).is_err() {
                shared.queued_frames.fetch_sub(block.len, Ordering::AcqRel);
                shared
                    .dropped_frames
                    .fetch_add(block.len as u64, Ordering::Relaxed);
            }
        }
        shared.publication_generation.fetch_add(1, Ordering::AcqRel);
        shared
            .in_flight_frames
            .fetch_sub(block.len, Ordering::AcqRel);
    }
}

/// An unknown buffer range is worth trying, but a backend may only accept its
/// default. Retry that specific rejection once; never hide device/stream errors.
fn build_with_input_buffer_fallback<T>(
    requested: &cpal::StreamConfig,
    unknown_range: bool,
    mut build: impl FnMut(&cpal::StreamConfig) -> Result<T, cpal::BuildStreamError>,
) -> Result<T, cpal::BuildStreamError> {
    let result = build(requested);
    if unknown_range
        && matches!(requested.buffer_size, cpal::BufferSize::Fixed(_))
        && matches!(
            result,
            Err(cpal::BuildStreamError::StreamConfigNotSupported)
        )
    {
        let mut fallback = requested.clone();
        fallback.buffer_size = cpal::BufferSize::Default;
        build(&fallback)
    } else {
        result
    }
}

fn build_input<T: SizedSample>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channel: usize,
    mut producer: InputProducer,
    error: Arc<AtomicU32>,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels);
    let control = producer.control.clone();
    let stream = producer.stream;
    let mut priority_initialized = false;
    device.build_input_stream(
        config,
        move |data: &[T], _| {
            if !priority_initialized {
                crate::realtime_priority::promote_audio_callback_thread();
                priority_initialized = true;
            }
            producer.capture(data, channels, channel);
        },
        move |failure| {
            let code = match failure {
                cpal::StreamError::BufferUnderrun => return,
                cpal::StreamError::DeviceNotAvailable => 1,
                cpal::StreamError::StreamInvalidated => 2,
                cpal::StreamError::BackendSpecific { .. } => 3,
            };
            control.finish_stream(stream);
            error.store(code, Ordering::Release);
        },
        None,
    )
}

// Exact binary fractions deliberately verify sample routing and silence without rounding.
#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::{ProcessStatus, RealtimeCommand, RealtimeEngine, RealtimeEngineConfig};

    fn input_range(
        channels: u16,
        min: u32,
        max: u32,
        format: SampleFormat,
    ) -> cpal::SupportedStreamConfigRange {
        cpal::SupportedStreamConfigRange::new(
            channels,
            min,
            max,
            cpal::SupportedBufferSize::Unknown,
            format,
        )
    }

    #[test]
    fn unusable_capture_routes_do_not_report_a_channel_error() {
        let invalid = [
            input_range(0, 48_000, 48_000, SampleFormat::F32),
            input_range(2, 0, 48_000, SampleFormat::F32),
            input_range(2, 48_000, 44_100, SampleFormat::F32),
            input_range(2, 48_000, 48_000, SampleFormat::DsdU8),
        ];
        for ranges in [Vec::new(), invalid.to_vec()] {
            assert!(!ranges.iter().any(usable_input_config));
            assert!(matches!(
                select_input_config(ranges.into_iter(), 48_000, 0),
                Err(InputMonitorError::NoUsableInputConfig)
            ));
        }
    }

    #[test]
    fn capture_channel_error_requires_a_usable_device_format() {
        let range = input_range(2, 44_100, 48_000, SampleFormat::F32);
        assert!(usable_input_config(&range));
        assert!(matches!(
            select_input_config([range].into_iter(), 48_000, 2),
            Err(InputMonitorError::NoMatchingConfig { channel: 3 })
        ));
        let selected = select_input_config([range].into_iter(), 48_000, 1).unwrap();
        assert_eq!(selected.channels(), 2);
        assert_eq!(selected.sample_rate(), 48_000);
    }

    #[test]
    fn capture_selection_skips_invalid_ranges_and_preserves_format_ranking() {
        let ranges = [
            input_range(1, 48_000, 44_100, SampleFormat::F32),
            input_range(1, 44_100, 44_100, SampleFormat::F32),
            input_range(2, 48_000, 48_000, SampleFormat::F32),
            input_range(1, 48_000, 48_000, SampleFormat::I16),
            input_range(1, 48_000, 48_000, SampleFormat::F32),
        ];
        let selected = select_input_config(ranges.into_iter(), 48_000, 0).unwrap();
        assert_eq!(selected.sample_rate(), 48_000);
        assert_eq!(selected.channels(), 1);
        assert_eq!(selected.sample_format(), SampleFormat::F32);
    }

    #[test]
    fn capture_selection_rejects_excessive_resampling_without_channel_blame() {
        let range = input_range(1, 1, 1, SampleFormat::F32);
        assert!(matches!(
            select_input_config([range].into_iter(), 48_000, 0),
            Err(InputMonitorError::NoUsableInputConfig)
        ));
    }

    #[test]
    fn unknown_input_buffer_retries_only_unsupported_fixed_configuration_once() {
        let config = cpal::StreamConfig {
            channels: 1,
            sample_rate: 48_000,
            buffer_size: cpal::BufferSize::Fixed(64),
        };
        let mut attempts = Vec::new();
        let result = build_with_input_buffer_fallback(&config, true, |config| {
            attempts.push(config.buffer_size);
            if matches!(config.buffer_size, cpal::BufferSize::Fixed(_)) {
                Err(cpal::BuildStreamError::StreamConfigNotSupported)
            } else {
                Ok(())
            }
        });
        assert!(result.is_ok());
        assert_eq!(
            attempts,
            [cpal::BufferSize::Fixed(64), cpal::BufferSize::Default]
        );
        for (unknown, error) in [
            (false, cpal::BuildStreamError::StreamConfigNotSupported),
            (true, cpal::BuildStreamError::DeviceNotAvailable),
        ] {
            let mut calls = 0;
            let result: Result<(), _> = build_with_input_buffer_fallback(&config, unknown, |_| {
                calls += 1;
                Err(error.clone())
            });
            assert!(result.is_err());
            assert_eq!(calls, 1);
        }
    }

    fn fixture(input_rate: u32, output_rate: u32) -> (InputMonitorControl, InputProducer) {
        let control = InputMonitorControl::new();
        let stream = control.start_stream();
        control.set_enabled(true);
        let producer = InputProducer::new(control.clone(), stream, input_rate, output_rate);
        (control, producer)
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn tuner_uses_selected_raw_channel_and_resets_with_monitor_lifecycle() {
        for rate in [44_100, 48_000, 96_000] {
            let (control, mut producer) = fixture(rate, 48_000);
            control.set_tuner_enabled(true);
            control.set_gain(0.0);
            control
                .configure_effects(
                    &[effect(gaw_core::processors::ProcessorKind::PitchShift(
                        gaw_core::processors::PitchShiftParameters {
                            semitones: 12,
                            ..Default::default()
                        },
                    ))],
                    false,
                    48_000,
                    120.0,
                )
                .unwrap();
            let data: Vec<f32> = (0..rate / 3)
                .flat_map(|frame| {
                    let phase = std::f32::consts::TAU * 41.203_445 * frame as f32 / rate as f32;
                    [0.0, 0.15 * phase.sin() + 0.6 * (2.0 * phase).sin()]
                })
                .collect();
            let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
            for block in data.chunks(512) {
                producer.capture(block, 2, 1);
                let mut monitored = [0.0; 1024];
                mixer.mix(&mut monitored, 2);
                assert_eq!(monitored, [0.0; 1024]);
            }
            let reading = control.tuner_reading().unwrap();
            assert_eq!(reading.string_index, 0);
            assert!(reading.cents.abs() < 2.0, "{reading:?}");
            assert_eq!(control.peak(), 0.0);
            control.set_enabled(false);
            control.set_enabled(true);
            control.set_tuner_enabled(true);
            assert!(control.tuner_reading().is_none());
            producer.capture(&data, 2, 1);
            assert!(control.tuner_reading().is_some());
            control.finish_stream(producer.stream);
            assert!(control.tuner_reading().is_none());
        }
    }

    #[derive(Debug)]
    struct Constant;
    impl crate::RealtimeRender for Constant {
        fn render(&self, _: u64, output: &mut crate::SampleBlock<'_>) {
            output.samples_mut().fill(0.25);
        }
    }

    #[test]
    fn live_audio_mixes_with_project_and_continues_after_project_end() {
        use gaw_core::processors::{ProcessorKind, StereoToolParameters};

        let (control, mut producer) = fixture(48_000, 48_000);
        let (sender, mut engine) =
            RealtimeEngine::new(RealtimeEngineConfig::default(), 8, 8).unwrap();
        control
            .configure_effects(
                &[effect(ProcessorKind::StereoTool(StereoToolParameters {
                    invert_right: true,
                    ..Default::default()
                }))],
                false,
                48_000,
                120.0,
            )
            .unwrap();
        control.set_gain(0.5);
        engine.set_input_monitor(control.clone());
        let snapshot = crate::RenderSnapshot::new(
            1,
            48_000,
            crate::ChannelLayout::Stereo,
            2,
            0,
            Arc::new(Constant),
        )
        .unwrap();
        sender
            .try_send(RealtimeCommand::ActivatePreview(Arc::new(snapshot)))
            .unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();
        producer.capture(&[0.5_f32; 8], 1, 0);
        let mut output = [0.0; 8];
        engine.process(&mut output);
        let faded_project = 0.25 / 239.0;
        assert_eq!(
            output,
            [
                0.25,
                -0.25,
                0.25 + faded_project,
                -0.25 + faded_project,
                0.25,
                -0.25,
                0.25,
                -0.25
            ]
        );
        assert_eq!(sender.output_peak(), 0.25);
        assert_eq!(control.peak(), 0.25);
        assert!(!engine.transport().playing);
        engine.process(&mut output);
        assert_eq!(output, [0.25, -0.25, 0.25, -0.25, 0.25, -0.25, 0.25, -0.25]);
        assert_eq!(sender.frame_position(), 2);
    }

    #[test]
    fn rejected_project_sample_rate_does_not_interrupt_live_audio() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let config = RealtimeEngineConfig {
            maximum_block_frames: 8,
            ..RealtimeEngineConfig::default()
        };
        let (sender, mut engine) = RealtimeEngine::new(config, 8, 8).unwrap();
        engine.set_input_monitor(control);
        let snapshot = crate::RenderSnapshot::new(
            1,
            960_000,
            crate::ChannelLayout::Stereo,
            1000,
            0,
            Arc::new(Constant),
        )
        .unwrap();
        sender
            .try_send(RealtimeCommand::ActivatePreview(Arc::new(snapshot)))
            .unwrap();
        sender.try_send(RealtimeCommand::Play).unwrap();
        producer.capture(&[0.5_f32; 8], 1, 0);
        let mut output = [0.0; 16];
        assert_eq!(
            engine.process(&mut output),
            ProcessStatus::SampleRateMismatch
        );
        assert_eq!(output, [0.5; 16]);
    }
    #[test]
    fn selected_channel_is_centered_and_underruns_leave_silence() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        producer.capture(&[0.9_f32, 0.25, 0.9, -0.5, 0.9, 0.0], 2, 1);
        let mut output = [0.0; 8];
        assert!(mixer.mix(&mut output, 2));
        assert_eq!(output, [0.25, 0.25, -0.5, -0.5, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(control.peak(), 0.5);
        assert_eq!(control.peak(), 0.0);
    }

    #[test]
    fn monitoring_is_independent_of_stopped_paused_and_muted_transport() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let (sender, mut engine) =
            RealtimeEngine::new(RealtimeEngineConfig::default(), 8, 8).unwrap();
        engine.set_input_monitor(control);
        for command in [
            RealtimeCommand::Stop,
            RealtimeCommand::Pause,
            RealtimeCommand::SetGain(0.0),
            RealtimeCommand::Play,
        ] {
            sender.try_send(command).unwrap();
            producer.capture(&[0.5_f32; 8], 1, 0);
            let mut output = [0.0; 16];
            assert_eq!(engine.process(&mut output), ProcessStatus::Rendered);
            assert_eq!(output, [0.5; 16]);
            assert_eq!(sender.output_peak(), 0.0);
            assert_eq!(sender.frame_position(), 0);
        }
    }

    #[test]
    fn mute_and_reenable_without_callbacks_discards_old_audio() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        producer.capture(&[1.0_f32; 8], 1, 0);
        control.set_enabled(false);
        let mut output = [0.0; 4];
        assert!(!mixer.mix(&mut output, 1));
        control.set_enabled(true);
        producer.capture(&[0.25_f32; 4], 1, 0);
        mixer.mix(&mut output, 1);
        assert_eq!(output, [0.25; 4]);
    }

    #[test]
    fn replaced_or_dropped_streams_cannot_leak_old_audio() {
        let (control, mut old) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        old.capture(&[1.0_f32; 8], 1, 0);
        let stream = control.start_stream();
        let mut current = InputProducer::new(control.clone(), stream, 48_000, 48_000);
        control.finish_stream(old.stream);
        old.capture(&[1.0_f32; 8], 1, 0);
        current.capture(&[0.125_f32; 4], 1, 0);
        let mut output = [0.0; 4];
        mixer.mix(&mut output, 1);
        assert_eq!(output, [0.125; 4]);
        current.capture(&[0.5_f32; 4], 1, 0);
        control.finish_stream(stream);
        output.fill(0.0);
        assert!(!mixer.mix(&mut output, 1));
        assert_eq!(output, [0.0; 4]);
    }

    #[test]
    fn sample_rate_conversion_keeps_fraction_across_callbacks() {
        let (control, mut producer) = fixture(24_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        producer.capture(&[0.0_f32, 0.25], 1, 0);
        producer.capture(&[0.5_f32, 0.75], 1, 0);
        let mut output = [0.0; 6];
        mixer.mix(&mut output, 1);
        assert_eq!(output, [0.0, 0.125, 0.25, 0.375, 0.5, 0.625]);
        let (control, mut producer) = fixture(48_000, 24_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 24_000);
        producer.capture(&[0.0_f32, 0.25, 0.5, 0.75, 1.0], 1, 0);
        let mut output = [0.0; 3];
        mixer.mix(&mut output, 1);
        assert_eq!(output, [0.0, 0.5, 0.0]);
    }

    #[test]
    fn pcm_conversion_and_monitor_gain_are_applied() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        control.set_gain(0.5);
        producer.capture(&[i16::MIN, 0, 0], 1, 0);
        let mut output = [0.25; 2];
        mixer.mix(&mut output, 1);
        assert_eq!(output, [-0.25, 0.25]);
        assert_eq!(control.peak(), 0.5);
    }

    #[test]
    fn invalid_samples_and_gain_cannot_poison_the_output() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        producer.capture(&[f32::NAN, f32::INFINITY, -f32::INFINITY, 0.0], 1, 0);
        let mut output = [0.0; 3];
        mixer.mix(&mut output, 1);
        assert_eq!(output, [0.0; 3]);
        control.set_gain(f32::NAN);
        producer.capture(&[1.0_f32; 4], 1, 0);
        mixer.mix(&mut output, 1);
        assert_eq!(output, [0.0; 3]);
    }

    #[test]
    fn overflowing_capture_preserves_recent_audio_with_bounded_storage() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        producer.capture(&vec![0.5_f32; CAPACITY + 10], 1, 0);
        assert_eq!(control.0.receiver.len(), QUEUE_BLOCKS);
        assert!(control.latency_status().queued_frames <= CAPACITY);
        producer.capture(&[0.25_f32; 8], 1, 0);
        let mut output = [0.0; 8];
        mixer.mix(&mut output, 1);
        assert_eq!(output, [0.25; 8]);
        assert_eq!(control.latency_status().queued_frames, 0);
        assert_eq!(
            control.latency_status().dropped_frames,
            (CAPACITY + 10) as u64
        );
    }

    #[test]
    fn callback_size_mismatch_preserves_a_whole_input_block() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        producer.capture(&[0.5_f32; 1024], 1, 0);
        for _ in 0..16 {
            let mut output = [0.0; 64];
            mixer.mix(&mut output, 1);
            assert_eq!(output, [0.5; 64]);
        }
        assert_eq!(control.latency_status().queued_frames, 0);
    }
    fn effect(kind: gaw_core::processors::ProcessorKind) -> gaw_core::Processor {
        gaw_core::Processor::new(gaw_core::ProcessorId::new("input-test").unwrap(), kind)
    }

    fn gain(db: f32) -> gaw_core::Processor {
        effect(gaw_core::processors::ProcessorKind::Gain(
            gaw_core::processors::GainParameters {
                gain_db: db,
                ..Default::default()
            },
        ))
    }

    fn hard_clipper() -> gaw_core::Processor {
        effect(gaw_core::processors::ProcessorKind::Clipper(
            gaw_core::processors::ClipperParameters {
                threshold_db: -6.0,
                output_ceiling_db: -6.0,
                oversampling: gaw_core::processors::Oversampling::Off,
                ..Default::default()
            },
        ))
    }

    fn render_effects(
        effects: &[gaw_core::Processor],
        bypass: bool,
        monitor_gain: f32,
    ) -> Vec<f32> {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        control
            .configure_effects(effects, bypass, 48_000, 120.0)
            .unwrap();
        control.set_gain(monitor_gain);
        producer.capture(&[0.5_f32; 1024], 1, 0);
        let mut output = vec![0.0; 2048];
        mixer.mix(&mut output, 2);
        output
    }

    #[test]
    fn effect_order_individual_bypass_and_post_effect_monitor_gain() {
        let first = render_effects(&[gain(12.0), hard_clipper()], false, 1.0);
        let reversed = render_effects(&[hard_clipper(), gain(12.0)], false, 1.0);
        assert!(reversed[2046] > first[2046] * 2.0);
        let quiet = render_effects(&[gain(12.0), hard_clipper()], false, 0.25);
        assert!((quiet[2046] - first[2046] * 0.25).abs() < 1e-6);
        assert_eq!(
            render_effects(&[gain(12.0), hard_clipper()], true, 1.0),
            vec![0.5; 2048]
        );
        let mut disabled = gain(12.0);
        disabled.enabled = false;
        assert_eq!(render_effects(&[disabled], false, 1.0), vec![0.5; 2048]);
    }

    #[test]
    fn live_chain_preserves_stereo_and_maps_mono_effects() {
        use gaw_core::processors::{ProcessorKind, StereoToolParameters};
        let invert = effect(ProcessorKind::StereoTool(StereoToolParameters {
            invert_right: true,
            ..Default::default()
        }));
        let stereo = render_effects(std::slice::from_ref(&invert), false, 1.0);
        assert_eq!(&stereo[2046..], &[0.5, -0.5]);
        let mono = effect(ProcessorKind::StereoTool(StereoToolParameters {
            output_layout: gaw_core::ChannelLayout::Mono,
            ..Default::default()
        }));
        let folded = render_effects(&[invert, mono], false, 1.0);
        assert!(folded.iter().all(|sample| sample.abs() < 1e-6));
    }

    #[test]
    fn delay_tail_continues_without_capture_or_transport_and_resets_on_disable() {
        use gaw_core::processors::{DelayParameters, ProcessorKind, TimeValue};
        let (control, mut producer) = fixture(48_000, 48_000);
        control
            .configure_effects(
                &[effect(ProcessorKind::Delay(DelayParameters {
                    time: TimeValue::Seconds(0.01),
                    feedback: 0.0,
                    mix: 1.0,
                    ..Default::default()
                }))],
                false,
                48_000,
                120.0,
            )
            .unwrap();
        let (_, mut engine) = RealtimeEngine::new(RealtimeEngineConfig::default(), 8, 8).unwrap();
        engine.set_input_monitor(control.clone());
        producer.capture(&[1.0_f32, 0.0], 1, 0);
        let mut output = [0.0; 256];
        engine.process(&mut output);
        assert_eq!(output, [0.0; 256]);
        engine.process(&mut output);
        engine.process(&mut output);
        engine.process(&mut output);
        assert!(output.iter().any(|sample| sample.abs() > 0.01));
        assert!(!engine.transport().playing);
        control.set_enabled(false);
        control.set_enabled(true);
        engine.process(&mut output);
        assert_eq!(output, [0.0; 256]);
    }

    #[test]
    fn every_audio_effect_prepares_and_analyzers_and_invalid_chains_are_rejected() {
        use gaw_core::processors::ProcessorKind;
        let control = InputMonitorControl::new();
        for kind in ProcessorKind::catalog_defaults() {
            let analyzer = kind.is_analyzer();
            let definition = effect(kind);
            let result = control.configure_effects(&[definition], false, 48_000, 120.0);
            assert_eq!(result.is_err(), analyzer, "{result:?}");
        }
        assert!(
            control
                .configure_effects(
                    &vec![gain(0.0); MAX_INPUT_EFFECTS + 1],
                    false,
                    48_000,
                    120.0
                )
                .is_err()
        );
        assert!(control.configure_effects(&[], false, 0, 120.0).is_err());
        assert!(
            control
                .configure_effects(&[], false, 48_000, f64::NAN)
                .is_err()
        );
    }

    #[test]
    fn latest_chain_wins_failed_edits_preserve_sound_and_wrong_rates_are_retired() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        control
            .configure_effects(&[gain(12.0)], false, 48_000, 120.0)
            .unwrap();
        control
            .configure_effects(&[], false, 48_000, 120.0)
            .unwrap();
        assert!(
            control
                .configure_effects(&[gain(f32::NAN)], false, 48_000, 120.0)
                .is_err()
        );
        producer.capture(&[0.5_f32; 4], 1, 0);
        let mut output = [0.0; 8];
        mixer.mix(&mut output, 2);
        assert_eq!(output, [0.5; 8]);
        control
            .configure_effects(&[gain(12.0)], false, 96_000, 120.0)
            .unwrap();
        producer.capture(&[0.5_f32; 4], 1, 0);
        output.fill(0.0);
        mixer.mix(&mut output, 2);
        assert_eq!(output, [0.5; 8]);
        assert_eq!(control.0.retired_receiver.len(), 1);
        control.collect_retired_effects();
        assert!(control.0.retired_receiver.is_empty());
    }

    #[test]
    fn saturated_retirement_defers_replacement_without_dropping_chain() {
        let control = InputMonitorControl::new();
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        control
            .configure_effects(&[], false, 48_000, 120.0)
            .unwrap();
        mixer.mix(&mut [0.0; 2], 2);
        control
            .configure_effects(&[gain(6.0)], false, 48_000, 120.0)
            .unwrap();
        for _ in 0..2 {
            control
                .0
                .retired_sender
                .try_send(PreparedInputEffects::new(&[], false, 48_000, 120.0).unwrap())
                .unwrap();
        }
        mixer.mix(&mut [0.0; 2], 2);
        assert!(mixer.active.as_ref().unwrap().effects.is_empty());
        assert_eq!(control.0.effects_receiver.len(), 1);
        control.collect_retired_effects();
        mixer.mix(&mut [0.0; 2], 2);
        assert_eq!(mixer.active.as_ref().unwrap().effects.len(), 1);
        assert_eq!(control.0.retired_receiver.len(), 1);
    }
    #[test]
    fn equal_rate_impulse_and_partial_packets_have_no_staging_delay() {
        for frames in [1, 31, 64, 128, 129, 512, 1024] {
            let (control, mut producer) = fixture(48_000, 48_000);
            let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
            let mut input = vec![0.0_f32; frames];
            input[0] = 1.0;
            input[frames - 1] = 0.5;
            producer.capture(&input, 1, 0);
            assert_eq!(control.latency_status().queued_frames, frames);
            let mut output = vec![0.0; frames];
            mixer.mix(&mut output, 1);
            assert_eq!(output, input, "callback of {frames} frames");
            assert_eq!(control.latency_status().queued_frames, 0);
            assert_eq!(control.latency_status().underrun_frames, 0);
        }
    }

    #[test]
    fn stalled_equal_callbacks_trim_to_latest_block_not_two_blocks() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        producer.capture(&[0.0_f32; 128], 1, 0);
        let mut impulse = [0.0_f32; 128];
        impulse[0] = 1.0;
        producer.capture(&impulse, 1, 0);
        let mut output = [0.0; 128];
        mixer.mix(&mut output, 1);
        assert_eq!(output, impulse);
        let status = control.latency_status();
        assert_eq!(status.input_callback_frames, 128);
        assert_eq!(status.queued_frames, 0);
        assert_eq!(status.dropped_frames, 128);
        assert_eq!(status.underrun_frames, 0);
    }

    #[test]
    fn unequal_periodic_callbacks_preserve_all_input_at_every_phase() {
        for (input_frames, output_frames) in [
            (64, 1024),
            (1024, 64),
            (96, 64),
            (64, 96),
            (511, 128),
            (128, 511),
        ] {
            for phase in [0, 1, 31, 63] {
                let (control, mut producer) = fixture(48_000, 48_000);
                let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
                let input = vec![0.25_f32; input_frames];
                let mut output = vec![0.0_f32; output_frames];
                for time in 0..8192 {
                    if time % input_frames == 0 {
                        producer.capture(&input, 1, 0);
                    }
                    if time >= phase && (time - phase) % output_frames == 0 {
                        output.fill(0.0);
                        mixer.mix(&mut output, 1);
                    }
                }
                assert_eq!(
                    control.latency_status().dropped_frames,
                    0,
                    "input={input_frames} output={output_frames} phase={phase}"
                );
            }
        }
    }

    #[test]
    fn resampled_periodic_callbacks_keep_fractional_remainders_without_drops() {
        for input_rate in [44_100, 96_000] {
            let (control, mut producer) = fixture(input_rate, 48_000);
            let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
            let mut input_time = 0_u64;
            let mut output_time = 0_u64;
            for _ in 0..2_000 {
                if input_time <= output_time {
                    producer.capture(&[0.25_f32; 128], 1, 0);
                    input_time += 128 * 48_000;
                } else {
                    mixer.mix(&mut [0.0; 128], 1);
                    output_time += 128 * u64::from(input_rate);
                }
            }
            assert_eq!(
                control.latency_status().dropped_frames,
                0,
                "rate={input_rate}"
            );
        }
    }

    #[test]
    fn callback_order_flip_recovers_one_buffer_of_jitter_headroom() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        // Initial device startup is silent and must not force added latency.
        mixer.mix(&mut [0.0; 64], 1);
        assert_eq!(mixer.jitter_frames, 0);
        producer.capture(&[0.25_f32; 64], 1, 0);
        mixer.mix(&mut [0.0; 64], 1);
        // This output callback arrives before its capture callback.
        mixer.mix(&mut [0.0; 64], 1);
        assert_eq!(mixer.jitter_frames, 64);
        let underruns = control.latency_status().underrun_frames;
        // The delayed input and next input arrive before the next output.
        producer.capture(&[0.25_f32; 64], 1, 0);
        producer.capture(&[0.5_f32; 64], 1, 0);
        let mut output = [0.0; 64];
        mixer.mix(&mut output, 1);
        assert_eq!(output, [0.25; 64]);
        output.fill(0.0);
        mixer.mix(&mut output, 1);
        assert_eq!(output, [0.5; 64]);
        assert_eq!(control.latency_status().dropped_frames, 0);
        assert_eq!(control.latency_status().underrun_frames, underruns);
        control.set_enabled(false);
        control.set_enabled(true);
        mixer.mix(&mut [0.0; 64], 1);
        assert_eq!(mixer.jitter_frames, 0);
    }

    #[test]
    fn separate_capture_gaps_grow_bounded_reserve_but_one_outage_does_not() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control, 48_000);
        producer.capture(&[0.25_f32; 64], 1, 0);
        mixer.mix(&mut [0.0; 64], 1);
        for expected in [64, 128, 192, 256, 256] {
            mixer.mix(&mut [0.0; 64], 1);
            assert_eq!(mixer.jitter_frames, expected);
            for _ in 0..8 {
                mixer.mix(&mut [0.0; 64], 1);
                assert_eq!(mixer.jitter_frames, expected);
            }
            producer.capture(&[0.25_f32; 64], 1, 0);
            mixer.mix(&mut [0.0; 64], 1);
        }
    }

    #[test]
    fn unpublished_packet_cannot_trigger_discard_of_readable_input() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        producer.capture(&[0.25_f32; 64], 1, 0);
        // Pause a second publication after its reservation, before try_send.
        control.0.in_flight_frames.fetch_add(64, Ordering::AcqRel);
        control.0.queued_frames.fetch_add(64, Ordering::AcqRel);
        assert_eq!(control.latency_status().queued_frames, 64);
        let mut output = [0.0; 64];
        mixer.mix(&mut output, 1);
        assert_eq!(output, [0.25; 64]);
        assert_eq!(control.latency_status().dropped_frames, 0);
        assert_eq!(control.latency_status().underrun_frames, 0);
        control.0.queued_frames.fetch_sub(64, Ordering::AcqRel);
        control.0.in_flight_frames.fetch_sub(64, Ordering::AcqRel);
    }

    #[test]
    fn negotiated_output_rate_accepts_effects_prepared_for_the_device() {
        let (control, _) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 44_100);
        mixer.set_sample_rate(48_000);
        control
            .configure_effects(&[gain(6.0)], false, 48_000, 120.0)
            .unwrap();
        mixer.mix(&mut [0.0; 128], 1);
        assert_eq!(mixer.active.as_ref().unwrap().sample_rate, 48_000);
        assert_eq!(mixer.active.as_ref().unwrap().effects.len(), 1);
    }

    #[test]
    fn concurrent_capture_and_overflow_account_for_every_frame() {
        const CALLBACKS: usize = 2_000;
        const FRAMES: usize = 129;
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        let done = Arc::new(AtomicBool::new(false));
        let finished = Arc::clone(&done);
        let producer_thread = std::thread::spawn(move || {
            for _ in 0..CALLBACKS {
                producer.capture(&[0.25_f32; FRAMES], 1, 0);
            }
            finished.store(true, Ordering::Release);
        });
        let mut received = 0_usize;
        loop {
            let mut output = [0.0; 64];
            mixer.mix(&mut output, 1);
            received += output.iter().filter(|&&sample| sample == 0.25).count();
            if done.load(Ordering::Acquire) && control.latency_status().queued_frames == 0 {
                break;
            }
        }
        producer_thread.join().unwrap();
        assert_eq!(
            received as u64 + control.latency_status().dropped_frames,
            (CALLBACKS * FRAMES) as u64
        );
    }

    #[test]
    fn completely_dry_saturator_skips_oversampling_latency() {
        use gaw_core::processors::{ProcessorKind, SaturatorParameters};
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        control
            .configure_effects(
                &[effect(ProcessorKind::Saturator(SaturatorParameters {
                    mix: 0.0,
                    ..Default::default()
                }))],
                false,
                48_000,
                120.0,
            )
            .unwrap();
        producer.capture(&[1.0_f32], 1, 0);
        let mut output = [0.0];
        mixer.mix(&mut output, 1);
        assert_eq!(output, [1.0]);
        assert_eq!(control.latency_status().effect_latency_frames, 0);
    }

    #[test]
    fn replacing_mixer_releases_its_partial_packet_accounting() {
        let (control, mut producer) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        producer.capture(&[0.5_f32; 128], 1, 0);
        mixer.mix(&mut [0.0; 32], 1);
        assert_eq!(control.latency_status().queued_frames, 96);
        drop(mixer);
        assert_eq!(control.latency_status().queued_frames, 0);
    }

    #[test]
    fn neutral_live_pitch_has_zero_latency_and_active_chain_reports_latency() {
        use gaw_core::processors::{PitchShiftParameters, ProcessorKind};
        for parameters in [
            PitchShiftParameters::default(),
            PitchShiftParameters {
                semitones: 12,
                mix: 0.0,
                ..Default::default()
            },
            PitchShiftParameters {
                semitones: 1,
                cents: -100,
                ..Default::default()
            },
        ] {
            let (control, mut producer) = fixture(48_000, 48_000);
            let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
            control
                .configure_effects(
                    &[effect(ProcessorKind::PitchShift(parameters))],
                    false,
                    48_000,
                    120.0,
                )
                .unwrap();
            producer.capture(&[1.0_f32], 1, 0);
            let mut output = [0.0];
            mixer.mix(&mut output, 1);
            assert_eq!(output, [1.0]);
            assert_eq!(control.latency_status().effect_latency_frames, 0);
        }
        let (control, _) = fixture(48_000, 48_000);
        let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
        let shifted = effect(ProcessorKind::PitchShift(PitchShiftParameters {
            semitones: 12,
            ..Default::default()
        }));
        let expected =
            PreparedInputEffects::new(std::slice::from_ref(&shifted), false, 48_000, 120.0)
                .unwrap()
                .latency_frames;
        assert!(expected > 0);
        control
            .configure_effects(&[shifted.clone(), shifted], false, 48_000, 120.0)
            .unwrap();
        mixer.mix(&mut [0.0; 128], 1);
        assert_eq!(control.latency_status().effect_latency_frames, expected * 2);
        control.configure_effects(&[], true, 48_000, 120.0).unwrap();
        mixer.mix(&mut [0.0; 128], 1);
        assert_eq!(control.latency_status().effect_latency_frames, 0);
    }

    /// Reproducible callback CPU benchmark; run release/ignored/nocapture.
    #[test]
    #[ignore = "manual release-mode callback CPU benchmark"]
    fn monitor_callback_benchmark() {
        for effect_count in [0, 1, 16] {
            let (control, mut producer) = fixture(48_000, 48_000);
            let mut mixer = InputMonitorMixer::new(control.clone(), 48_000);
            control
                .configure_effects(&vec![gain(0.0); effect_count], false, 48_000, 120.0)
                .unwrap();
            let input = [0.25_f32; 128];
            let mut output = [0.0_f32; 256];
            for _ in 0..1_000 {
                producer.capture(&input, 1, 0);
                output.fill(0.0);
                std::hint::black_box(mixer.mix(std::hint::black_box(&mut output), 2));
            }
            let start = std::time::Instant::now();
            for _ in 0..30_000 {
                producer.capture(std::hint::black_box(&input), 1, 0);
                output.fill(0.0);
                std::hint::black_box(mixer.mix(std::hint::black_box(&mut output), 2));
            }
            println!(
                "monitor 128 frames, {effect_count} gain effects: {:.3} us/callback",
                start.elapsed().as_secs_f64() * 1e6 / 30_000.0
            );
        }
    }
}
