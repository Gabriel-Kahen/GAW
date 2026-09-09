//! Canonical high-quality resampling used by destructive renders and repitch playback.

use audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use thiserror::Error;

/// Errors returned by the canonical Rubato resampling path.
#[derive(Debug, Error)]
pub enum ResampleError {
    /// The caller supplied unsupported channels or channels of unequal length.
    #[error("audio must contain one or two equally sized channels")]
    InvalidAudio,
    /// The requested playback speed was invalid.
    #[error("playback speed must be finite and greater than zero")]
    InvalidRatio,
    /// Rubato rejected the configuration or input/output buffers.
    #[error("rubato resampling failed: {0}")]
    Rubato(String),
}

/// Shared windowed-sinc settings for repitching, materialization, and offline export.
///
/// Keeping these settings together prevents render paths from silently choosing
/// different filter quality or frequency responses.
pub fn canonical_sinc_parameters() -> SincInterpolationParameters {
    SincInterpolationParameters {
        sinc_len: 128,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Cubic,
        oversampling_factor: 128,
        window: WindowFunction::BlackmanHarris2,
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
        &canonical_sinc_parameters(),
        chunk,
        input.len(),
        FixedAsync::Input,
    )
    .map_err(|error| ResampleError::Rubato(error.to_string()))?;

    let input_adapter = SequentialSliceOfVecs::new(input, input.len(), input[0].len())
        .map_err(|_| ResampleError::InvalidAudio)?;
    let capacity = resampler.process_all_needed_output_len(input[0].len());
    let mut output = vec![vec![0.0; capacity]; input.len()];
    let mut output_adapter = SequentialSliceOfVecs::new_mut(&mut output, input.len(), capacity)
        .map_err(|_| ResampleError::InvalidAudio)?;
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

    #[test]
    fn planar_resampling_preserves_channel_separation() {
        let signal: Vec<_> = (0..2_000)
            .map(|index| (index as f32 * 0.03).sin())
            .collect();
        let input = vec![signal.clone(), vec![0.0; signal.len()]];
        let stereo = repitch_planar(&input, 1.1).unwrap();
        let mono = repitch_planar(&[signal], 1.1).unwrap();
        assert_eq!(stereo[0], mono[0]);
        assert_eq!(stereo[0].len(), stereo[1].len());
        assert!(stereo[1].iter().all(|sample| *sample == 0.0));
    }
}
