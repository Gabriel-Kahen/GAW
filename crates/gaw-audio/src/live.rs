//! Prepared computer-keyboard monitoring. All allocation happens before installation.

use gaw_dsp::{Instrument as _, NoteEvent, PrepareSpec, ProcessContext, Sampler};

use crate::{ChannelLayout, RealtimeEngineConfig};

const MAX_EVENTS: usize = 4_096;

/// An exclusively owned, prepared sampler transferred to the audio callback.
#[derive(Debug)]
pub struct PreparedLiveSampler {
    sampler: Sampler,
    sample_rate: u32,
    layout: ChannelLayout,
    left: Box<[f32]>,
    right: Box<[f32]>,
    events: Box<[NoteEvent]>,
    event_count: usize,
    gain: f32,
    frame: u64,
    tempo_bpm: f64,
}

impl PreparedLiveSampler {
    /// Prepares voices and scratch storage on the control/worker thread.
    ///
    /// # Errors
    /// Returns the sampler's configuration/preparation error.
    pub fn new(
        mut sampler: Sampler,
        config: RealtimeEngineConfig,
        tempo_bpm: f64,
        gain: f32,
    ) -> Result<Self, gaw_dsp::InstrumentError> {
        sampler.prepare(PrepareSpec {
            sample_rate: f64::from(config.sample_rate),
            max_block_size: config.maximum_block_frames,
            input_layout: config.output_layout,
            tempo_bpm,
        })?;
        Ok(Self {
            sampler,
            sample_rate: config.sample_rate,
            layout: config.output_layout,
            left: vec![0.0; config.maximum_block_frames].into_boxed_slice(),
            right: vec![0.0; config.maximum_block_frames].into_boxed_slice(),
            events: vec![
                NoteEvent::NoteOff {
                    sample_offset: 0,
                    note: 0
                };
                MAX_EVENTS
            ]
            .into_boxed_slice(),
            event_count: 0,
            gain: if gain.is_finite() { gain.max(0.0) } else { 0.0 },
            frame: 0,
            tempo_bpm,
        })
    }

    pub(crate) fn event(&mut self, event: NoteEvent) -> bool {
        if self.event_count == self.events.len() {
            return false;
        }
        self.events[self.event_count] = event;
        self.event_count += 1;
        true
    }

    pub(crate) fn reset(&mut self) {
        self.sampler.reset();
        self.event_count = 0;
    }

    pub(crate) fn mix(
        &mut self,
        output: &mut [f32],
        config: RealtimeEngineConfig,
        master_gain: f32,
    ) -> f32 {
        let channels = self.layout.channels();
        let frames = output.len() / channels;
        if frames == 0 {
            return 0.0;
        }
        if config.sample_rate != self.sample_rate
            || config.output_layout != self.layout
            || frames > self.left.len()
        {
            self.reset();
            return 0.0;
        }
        let mut stereo = [&mut self.left[..frames], &mut self.right[..frames]];
        let result = self.sampler.process(
            &mut stereo[..channels],
            &self.events[..self.event_count],
            ProcessContext {
                absolute_frame: self.frame,
                tempo_bpm: self.tempo_bpm,
            },
        );
        self.event_count = 0;
        self.frame = self.frame.saturating_add(frames as u64);
        if result.is_err() {
            self.reset();
            return 0.0;
        }
        let gain = self.gain * master_gain;
        let mut peak = 0.0_f32;
        for (index, frame) in output.chunks_exact_mut(channels).enumerate() {
            for (channel, sample) in frame.iter_mut().enumerate() {
                let value = stereo[channel][index] * gain;
                let value = if value.is_finite() { value } else { 0.0 };
                *sample += value;
                peak = peak.max(value.abs());
            }
        }
        peak
    }
}
