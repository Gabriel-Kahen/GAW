//! Real-time processor contract and buffer validation.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::parameter::{ParameterDescriptor, ParameterEvent};

pub const MONO_AND_STEREO: &[AudioLayout] = &[AudioLayout::Mono, AudioLayout::Stereo];
pub const STEREO_ONLY: &[AudioLayout] = &[AudioLayout::Stereo];
pub const MONO_ONLY: &[AudioLayout] = &[AudioLayout::Mono];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioLayout {
    Mono,
    Stereo,
}

impl AudioLayout {
    #[must_use]
    pub const fn channels(self) -> usize {
        match self {
            Self::Mono => 1,
            Self::Stereo => 2,
        }
    }

    #[must_use]
    pub const fn channel_count(self) -> usize {
        self.channels()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PrepareSpec {
    pub sample_rate: f64,
    pub max_block_size: usize,
    pub input_layout: AudioLayout,
    pub tempo_bpm: f64,
}

impl Default for PrepareSpec {
    fn default() -> Self {
        Self {
            sample_rate: 48_000.0,
            max_block_size: 512,
            input_layout: AudioLayout::Stereo,
            tempo_bpm: 120.0,
        }
    }
}

impl PrepareSpec {
    pub fn validate(self) -> Result<(), ProcessError> {
        if !self.sample_rate.is_finite() || self.sample_rate <= 0.0 {
            return Err(ProcessError::InvalidSampleRate(self.sample_rate));
        }
        if self.max_block_size == 0 {
            return Err(ProcessError::InvalidMaxBlockSize);
        }
        if !self.tempo_bpm.is_finite() || self.tempo_bpm <= 0.0 {
            return Err(ProcessError::InvalidTempo(self.tempo_bpm));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ProcessContext {
    pub absolute_frame: u64,
    pub tempo_bpm: f64,
}

impl Default for ProcessContext {
    fn default() -> Self {
        Self {
            absolute_frame: 0,
            tempo_bpm: 120.0,
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum ProcessError {
    #[error("processor has not been prepared")]
    NotPrepared,
    #[error("unsupported input layout: {0:?}")]
    UnsupportedLayout(AudioLayout),
    #[error("invalid sample rate: {0}")]
    InvalidSampleRate(f64),
    #[error("maximum block size must be non-zero")]
    InvalidMaxBlockSize,
    #[error("invalid tempo: {0}")]
    InvalidTempo(f64),
    #[error("expected {expected} {kind} channels, got {actual}")]
    ChannelCount {
        kind: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("{kind} channel {channel} has {actual} frames, expected {expected}")]
    BufferLength {
        kind: &'static str,
        channel: usize,
        expected: usize,
        actual: usize,
    },
    #[error("block has {actual} frames, prepared maximum is {maximum}")]
    BlockTooLarge { actual: usize, maximum: usize },
    #[error("parameter events must be ordered by sample offset")]
    EventsOutOfOrder,
    #[error("parameter event offset {offset} is outside a {frames}-frame block")]
    EventOutOfRange { offset: usize, frames: usize },
    #[error("unknown parameter: {0}")]
    UnknownParameter(String),
    #[error("invalid value for parameter: {0}")]
    InvalidParameterValue(String),
    #[error("analyzers do not accept process parameter events")]
    AnalyzerEventUnsupported,
}

/// The object-safe processing interface shared by every built-in effect and analyzer.
pub trait Processor: Send {
    fn type_id(&self) -> &'static str;

    fn version(&self) -> u32 {
        1
    }

    fn input_layouts(&self) -> &'static [AudioLayout];

    fn output_layout(&self, input: AudioLayout) -> Result<AudioLayout, ProcessError>;

    /// Allocate and initialize all bounded processing storage.
    fn prepare(&mut self, spec: PrepareSpec) -> Result<(), ProcessError>;

    /// Process one planar audio block without allocation, locking, or I/O.
    fn process(
        &mut self,
        input: &[&[f32]],
        output: &mut [&mut [f32]],
        events: &[ParameterEvent],
        context: ProcessContext,
    ) -> Result<(), ProcessError>;

    fn reset(&mut self);

    /// Restore deterministic state at a timeline position.
    fn seek(&mut self, absolute_frame: u64);

    fn latency_frames(&self) -> u32;

    fn tail_frames(&self) -> u64;

    fn parameters(&self) -> &'static [ParameterDescriptor];

    fn enabled(&self) -> bool;

    fn set_enabled(&mut self, enabled: bool);
}

/// Validate planar buffers and event ordering without allocating.
pub fn validate_process_io(
    input: &[&[f32]],
    output: &[&mut [f32]],
    input_layout: AudioLayout,
    output_layout: AudioLayout,
    maximum_block_size: usize,
    events: &[ParameterEvent],
) -> Result<usize, ProcessError> {
    validate_channel_count(input.len(), input_layout.channels(), "input")?;
    validate_channel_count(output.len(), output_layout.channels(), "output")?;
    let frames = input.first().map_or(0, |channel| channel.len());
    if frames > maximum_block_size {
        return Err(ProcessError::BlockTooLarge {
            actual: frames,
            maximum: maximum_block_size,
        });
    }
    for (channel, buffer) in input.iter().enumerate() {
        validate_buffer_length(buffer.len(), frames, channel, "input")?;
    }
    for (channel, buffer) in output.iter().enumerate() {
        validate_buffer_length(buffer.len(), frames, channel, "output")?;
    }
    let mut prior = 0;
    for (index, event) in events.iter().enumerate() {
        if index != 0 && event.sample_offset < prior {
            return Err(ProcessError::EventsOutOfOrder);
        }
        if event.sample_offset >= frames {
            return Err(ProcessError::EventOutOfRange {
                offset: event.sample_offset,
                frames,
            });
        }
        prior = event.sample_offset;
    }
    Ok(frames)
}

fn validate_channel_count(
    actual: usize,
    expected: usize,
    kind: &'static str,
) -> Result<(), ProcessError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ProcessError::ChannelCount {
            kind,
            expected,
            actual,
        })
    }
}

fn validate_buffer_length(
    actual: usize,
    expected: usize,
    channel: usize,
    kind: &'static str,
) -> Result<(), ProcessError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ProcessError::BufferLength {
            kind,
            channel,
            expected,
            actual,
        })
    }
}

/// Copy bypass audio, explicitly mapping the declared mono/stereo layouts.
pub fn copy_or_map_bypass(input: &[&[f32]], output: &mut [&mut [f32]]) {
    match (input.len(), output.len()) {
        (1, 1) => output[0].copy_from_slice(input[0]),
        (1, 2) => {
            output[0].copy_from_slice(input[0]);
            output[1].copy_from_slice(input[0]);
        }
        (2, 1) => {
            for (sample, (left, right)) in output[0]
                .iter_mut()
                .zip(input[0].iter().zip(input[1].iter()))
            {
                *sample = 0.5 * (left + right);
            }
        }
        (2, 2) => {
            output[0].copy_from_slice(input[0]);
            output[1].copy_from_slice(input[1]);
        }
        _ => unreachable!("buffers have already been validated"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_mismatched_planar_buffers() {
        let input_data = [0.0; 8];
        let mut left = [0.0; 8];
        let mut short = [0.0; 7];
        let input = [&input_data[..]];
        let output = [&mut left[..], &mut short[..]];
        assert!(matches!(
            validate_process_io(
                &input,
                &output,
                AudioLayout::Mono,
                AudioLayout::Stereo,
                64,
                &[]
            ),
            Err(ProcessError::BufferLength { kind: "output", .. })
        ));
    }
}
