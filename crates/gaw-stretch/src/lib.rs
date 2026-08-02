//! Safe, single-owner wrapper around Signalsmith Stretch.

#![forbid(unsafe_code)]

use std::{cell::Cell, fmt, marker::PhantomData};

use signalsmith_stretch::Stretch;
use thiserror::Error;

/// Runtime quality policy for pitch-preserving time stretch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    /// Canonical playback and materialized renders.
    Canonical,
    /// Lower-cost, noncanonical scrub preview.
    Preview,
}

/// Validated construction parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub channels: u8,
    pub sample_rate: u32,
    pub quality: Quality,
}

/// Time-stretch configuration or buffer error.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    #[error("channel count must be mono or stereo, got {0}")]
    InvalidChannels(u8),
    #[error("sample rate must be nonzero")]
    InvalidSampleRate,
    #[error("{buffer} buffer length {length} is not divisible by {channels} channels")]
    MisalignedBuffer {
        buffer: &'static str,
        length: usize,
        channels: usize,
    },
    #[error("{name} must be finite and positive")]
    InvalidFactor { name: &'static str },
}

/// A configured stretcher owned by exactly one processing thread.
///
/// The marker deliberately prevents sharing this stateful C++ processor across
/// threads. Move it between threads only while it is idle.
pub struct TimeStretcher {
    inner: Stretch,
    config: Config,
    _not_sync: PhantomData<Cell<()>>,
}

impl fmt::Debug for TimeStretcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TimeStretcher")
            .field("config", &self.config)
            .field("input_latency", &self.input_latency())
            .field("output_latency", &self.output_latency())
            .finish_non_exhaustive()
    }
}

impl TimeStretcher {
    /// Creates a mono or stereo stretcher.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported channel count or zero sample rate.
    pub fn new(config: Config) -> Result<Self, Error> {
        if !matches!(config.channels, 1 | 2) {
            return Err(Error::InvalidChannels(config.channels));
        }
        if config.sample_rate == 0 {
            return Err(Error::InvalidSampleRate);
        }

        let inner = match config.quality {
            Quality::Canonical => {
                Stretch::preset_default(u32::from(config.channels), config.sample_rate)
            }
            Quality::Preview => {
                Stretch::preset_cheaper(u32::from(config.channels), config.sample_rate)
            }
        };

        Ok(Self {
            inner,
            config,
            _not_sync: PhantomData,
        })
    }

    pub const fn config(&self) -> Config {
        self.config
    }

    pub fn input_latency(&self) -> usize {
        self.inner.input_latency()
    }

    pub fn output_latency(&self) -> usize {
        self.inner.output_latency()
    }

    pub fn reset(&mut self) {
        self.inner.reset();
    }

    /// Sets the pitch multiplier without changing duration.
    ///
    /// # Errors
    ///
    /// Returns an error unless `factor` is finite and positive.
    pub fn set_pitch_factor(&mut self, factor: f32) -> Result<(), Error> {
        validate_factor("pitch factor", factor)?;
        self.inner.set_transpose_factor(factor, None);
        Ok(())
    }

    /// Sets the pitch offset in semitones without changing duration.
    ///
    /// # Errors
    ///
    /// Returns an error unless `semitones` is finite.
    pub fn set_pitch_semitones(&mut self, semitones: f32) -> Result<(), Error> {
        if !semitones.is_finite() {
            return Err(Error::InvalidFactor { name: "semitones" });
        }
        self.inner.set_transpose_factor_semitones(semitones, None);
        Ok(())
    }

    /// Sets a formant multiplier.
    ///
    /// # Errors
    ///
    /// Returns an error unless `factor` is finite and positive.
    pub fn set_formant_factor(&mut self, factor: f32, compensate_pitch: bool) -> Result<(), Error> {
        validate_factor("formant factor", factor)?;
        self.inner.set_formant_factor(factor, compensate_pitch);
        Ok(())
    }

    /// Process differently sized interleaved blocks without allocating.
    ///
    /// # Errors
    ///
    /// Returns an error when either buffer is not channel-aligned.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> Result<(), Error> {
        self.validate_buffer("input", input.len())?;
        self.validate_buffer("output", output.len())?;
        self.inner.process(input, output);
        Ok(())
    }

    /// Render a complete interleaved buffer to an exact output length.
    ///
    /// Returns `false` when the input is too short for the configured analysis
    /// window. Callers can fall back to repitch or zero-padded streaming.
    ///
    /// # Errors
    ///
    /// Returns an error when either buffer is not channel-aligned.
    pub fn exact(&mut self, input: &[f32], output: &mut [f32]) -> Result<bool, Error> {
        self.validate_buffer("input", input.len())?;
        self.validate_buffer("output", output.len())?;
        Ok(self.inner.exact(input, output))
    }

    /// Primes the processor around a new input position.
    ///
    /// # Errors
    ///
    /// Returns an error for a misaligned buffer or invalid playback rate.
    pub fn seek(&mut self, input: &[f32], playback_rate: f64) -> Result<(), Error> {
        self.validate_buffer("input", input.len())?;
        if !playback_rate.is_finite() || playback_rate <= 0.0 {
            return Err(Error::InvalidFactor {
                name: "playback rate",
            });
        }
        self.inner.seek(input, playback_rate);
        Ok(())
    }

    /// Flushes pending output after finite input.
    ///
    /// # Errors
    ///
    /// Returns an error when `output` is not channel-aligned.
    pub fn flush(&mut self, output: &mut [f32]) -> Result<(), Error> {
        self.validate_buffer("output", output.len())?;
        self.inner.flush(output);
        Ok(())
    }

    fn validate_buffer(&self, buffer: &'static str, length: usize) -> Result<(), Error> {
        let channels = usize::from(self.config.channels);
        if !length.is_multiple_of(channels) {
            return Err(Error::MisalignedBuffer {
                buffer,
                length,
                channels,
            });
        }
        Ok(())
    }
}

fn validate_factor(name: &'static str, factor: f32) -> Result<(), Error> {
    if factor.is_finite() && factor > 0.0 {
        Ok(())
    } else {
        Err(Error::InvalidFactor { name })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono() -> TimeStretcher {
        TimeStretcher::new(Config {
            channels: 1,
            sample_rate: 48_000,
            quality: Quality::Canonical,
        })
        .unwrap()
    }

    #[test]
    fn validates_configuration_and_interleaving() {
        assert_eq!(
            TimeStretcher::new(Config {
                channels: 3,
                sample_rate: 48_000,
                quality: Quality::Preview,
            })
            .unwrap_err(),
            Error::InvalidChannels(3)
        );

        let mut stretch = TimeStretcher::new(Config {
            channels: 2,
            sample_rate: 48_000,
            quality: Quality::Preview,
        })
        .unwrap();
        let error = stretch.process(&[0.0; 3], &mut [0.0; 4]).unwrap_err();
        assert!(matches!(
            error,
            Error::MisalignedBuffer {
                buffer: "input",
                ..
            }
        ));
    }

    #[test]
    fn exact_tempo_match_is_finite() {
        let mut stretch = mono();
        let input: Vec<f32> = (0..48_000_u16)
            .map(|frame| {
                let phase = f32::from(frame) * 440.0 * std::f32::consts::TAU / 48_000.0;
                phase.sin() * 0.25
            })
            .collect();
        let mut output = vec![0.0; 44_000];

        assert!(stretch.exact(&input, &mut output).unwrap());
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().any(|sample| sample.abs() > 0.01));
    }

    #[test]
    fn rejects_invalid_factors() {
        let mut stretch = mono();
        assert!(stretch.set_pitch_factor(0.0).is_err());
        assert!(stretch.set_pitch_semitones(f32::NAN).is_err());
        assert!(stretch.seek(&[0.0; 1024], -1.0).is_err());
    }
}
