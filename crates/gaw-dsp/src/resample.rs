//! Canonical high-quality resampling used by destructive renders and repitch playback.

use rubato::audioadapter::{Adapter, AdapterMut};
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use thiserror::Error;

/// Errors returned by the canonical Rubato resampling path.
#[derive(Debug, Error)]
pub enum ResampleError {
    /// The caller supplied no channels or channels of unequal length.
    #[error("audio must contain one or more equally sized channels")]
    InvalidAudio,
    /// The requested playback speed was invalid.
    #[error("playback speed must be finite and greater than zero")]
    InvalidRatio,
    /// Rubato rejected the configuration or input/output buffers.
    #[error("rubato resampling failed: {0}")]
    Rubato(String),
}

struct PlanarAdapter<'a> {
    channels: &'a [Vec<f32>],
    frames: usize,
}

impl<'a> Adapter<'a, f32> for PlanarAdapter<'a> {
    unsafe fn read_sample_unchecked(&self, channel: usize, frame: usize) -> f32 {
        // Deliberately retain bounds checks: this adapter is used off the audio thread.
        self.channels[channel][frame]
    }

    fn channels(&self) -> usize {
        self.channels.len()
    }

    fn frames(&self) -> usize {
        self.frames
    }
}

struct PlanarAdapterMut<'a> {
    channels: &'a mut [Vec<f32>],
    frames: usize,
}

impl<'a> Adapter<'a, f32> for PlanarAdapterMut<'a> {
    unsafe fn read_sample_unchecked(&self, channel: usize, frame: usize) -> f32 {
        self.channels[channel][frame]
    }

    fn channels(&self) -> usize {
        self.channels.len()
    }

    fn frames(&self) -> usize {
        self.frames
    }
}

impl<'a> AdapterMut<'a, f32> for PlanarAdapterMut<'a> {
    unsafe fn write_sample_unchecked(&mut self, channel: usize, frame: usize, value: &f32) -> bool {
        self.channels[channel][frame] = *value;
        false
    }
}

/// Repitch planar audio with Rubato's windowed-sinc resampler.
///
/// `playback_speed` is source frames consumed per output frame: values above one
/// shorten and raise the audio, while values below one lengthen and lower it.
/// This is an offline/materialization helper; streaming processors use preallocated
/// state through the [`crate::Processor`] contract.
pub fn repitch_planar(
    input: &[Vec<f32>],
    playback_speed: f64,
) -> Result<Vec<Vec<f32>>, ResampleError> {
    if input.is_empty()
        || input.iter().any(|channel| channel.len() != input[0].len())
        || input.len() > 2
    {
        return Err(ResampleError::InvalidAudio);
    }
    if !playback_speed.is_finite() || playback_speed <= 0.0 {
        return Err(ResampleError::InvalidRatio);
    }
    if input[0].is_empty() {
        return Ok(vec![Vec::new(); input.len()]);
    }

    let ratio = 1.0 / playback_speed;
    let chunk = input[0].len().clamp(64, 2048);
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
        input.len(),
        FixedAsync::Input,
    )
    .map_err(|error| ResampleError::Rubato(error.to_string()))?;

    let input_adapter = PlanarAdapter {
        channels: input,
        frames: input[0].len(),
    };
    let capacity = resampler.process_all_needed_output_len(input[0].len());
    let mut output = vec![vec![0.0; capacity]; input.len()];
    let mut output_adapter = PlanarAdapterMut {
        channels: &mut output,
        frames: capacity,
    };
    let (_, written) = resampler
        .process_all_into_buffer(&input_adapter, &mut output_adapter, input[0].len(), None)
        .map_err(|error| ResampleError::Rubato(error.to_string()))?;
    for channel in &mut output {
        channel.truncate(written);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repitch_changes_duration_in_the_expected_direction() {
        let input = vec![vec![0.0; 4_800]];
        let fast = repitch_planar(&input, 2.0).unwrap();
        let slow = repitch_planar(&input, 0.5).unwrap();
        assert!((2_350..=2_450).contains(&fast[0].len()));
        assert!((9_500..=9_700).contains(&slow[0].len()));
    }

    #[test]
    fn repitch_is_deterministic() {
        let input = vec![
            (0..2_000)
                .map(|index| (index as f32 * 0.03).sin())
                .collect(),
        ];
        assert_eq!(
            repitch_planar(&input, 1.1).unwrap(),
            repitch_planar(&input, 1.1).unwrap()
        );
    }
}
