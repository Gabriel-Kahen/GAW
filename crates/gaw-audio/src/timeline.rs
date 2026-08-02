//! Musical-time conversion and allocation-free transport state.

use std::fmt;

use thiserror::Error;

/// A position or duration in quarter-note beats.
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
pub struct Beat(f64);

impl Beat {
    pub const ZERO: Self = Self(0.0);

    /// Creates a finite beat value.
    ///
    /// # Errors
    ///
    /// Returns [`TimelineError::NonFiniteBeat`] for NaN or infinity.
    pub fn new(value: f64) -> Result<Self, TimelineError> {
        if value.is_finite() {
            Ok(Self(value))
        } else {
            Err(TimelineError::NonFiniteBeat)
        }
    }

    pub const fn get(self) -> f64 {
        self.0
    }
}

/// An integral frame position on the project timeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Frame(i64);

impl Frame {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameRounding {
    Floor,
    Nearest,
    Ceil,
}

/// The project-wide constant tempo and internal sample rate.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tempo {
    bpm: f64,
    sample_rate: u32,
}

impl Tempo {
    /// Creates a constant project tempo.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-positive/non-finite BPM or zero sample rate.
    pub fn new(bpm: f64, sample_rate: u32) -> Result<Self, TimelineError> {
        if !bpm.is_finite() || bpm <= 0.0 {
            return Err(TimelineError::InvalidBpm);
        }
        if sample_rate == 0 {
            return Err(TimelineError::InvalidSampleRate);
        }
        Ok(Self { bpm, sample_rate })
    }

    pub const fn bpm(self) -> f64 {
        self.bpm
    }

    pub const fn sample_rate(self) -> u32 {
        self.sample_rate
    }

    pub fn frames_per_beat(self) -> f64 {
        f64::from(self.sample_rate) * 60.0 / self.bpm
    }

    /// Converts a beat using the caller's boundary-rounding policy.
    ///
    /// # Errors
    ///
    /// Returns [`TimelineError::FrameOverflow`] when the result does not fit in a frame.
    #[allow(clippy::cast_possible_truncation)]
    pub fn beat_to_frame(
        self,
        beat: Beat,
        rounding: FrameRounding,
    ) -> Result<Frame, TimelineError> {
        let exact = beat.get() * self.frames_per_beat();
        let rounded = match rounding {
            FrameRounding::Floor => exact.floor(),
            FrameRounding::Nearest => exact.round(),
            FrameRounding::Ceil => exact.ceil(),
        };
        if !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&rounded) {
            return Err(TimelineError::FrameOverflow);
        }
        Ok(Frame(rounded as i64))
    }

    /// Converts a beat boundary to the nearest frame.
    ///
    /// # Errors
    ///
    /// Returns [`TimelineError::FrameOverflow`] when the result does not fit in a frame.
    pub fn frame_at(self, beat: Beat) -> Result<Frame, TimelineError> {
        self.beat_to_frame(beat, FrameRounding::Nearest)
    }

    #[allow(clippy::cast_precision_loss)]
    pub fn frame_to_beat(self, frame: Frame) -> Beat {
        Beat(frame.get() as f64 / self.frames_per_beat())
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TimelineError {
    #[error("beat must be finite")]
    NonFiniteBeat,
    #[error("tempo must be finite and greater than zero")]
    InvalidBpm,
    #[error("sample rate must be greater than zero")]
    InvalidSampleRate,
    #[error("beat position is outside the frame range")]
    FrameOverflow,
    #[error("transport positions cannot be negative")]
    NegativePosition,
    #[error("loop end must be after loop start")]
    InvalidLoop,
}

/// Backward-compatible name for errors produced by tempo/time conversion.
pub type TempoError = TimelineError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoopRegion {
    pub start: Frame,
    pub end: Frame,
}

impl LoopRegion {
    /// Creates a non-empty, non-negative half-open loop region.
    ///
    /// # Errors
    ///
    /// Returns an error for negative boundaries or when `end <= start`.
    pub fn new(start: Frame, end: Frame) -> Result<Self, TimelineError> {
        if start.get() < 0 || end.get() < 0 {
            return Err(TimelineError::NegativePosition);
        }
        if end <= start {
            return Err(TimelineError::InvalidLoop);
        }
        Ok(Self { start, end })
    }

    pub fn length_frames(self) -> u64 {
        (self.end.get() - self.start.get()).cast_unsigned()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportEvent {
    None,
    Started,
    Paused,
    Stopped,
    Seeked { from: Frame, to: Frame },
}

/// Result of advancing the transport. `wraps` can exceed one for large offline blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportAdvance {
    pub start: Frame,
    pub end: Frame,
    pub advanced_frames: u64,
    pub wraps: u64,
}

/// Small, allocation-free transport state suitable for ownership by the audio thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Transport {
    state: TransportState,
    position: Frame,
    loop_region: Option<LoopRegion>,
}

impl Default for Transport {
    fn default() -> Self {
        Self {
            state: TransportState::Stopped,
            position: Frame::ZERO,
            loop_region: None,
        }
    }
}

impl Transport {
    pub const fn state(&self) -> TransportState {
        self.state
    }

    pub const fn position(&self) -> Frame {
        self.position
    }

    pub const fn loop_region(&self) -> Option<LoopRegion> {
        self.loop_region
    }

    pub fn play(&mut self) -> TransportEvent {
        if self.state == TransportState::Playing {
            TransportEvent::None
        } else {
            self.state = TransportState::Playing;
            TransportEvent::Started
        }
    }

    pub fn pause(&mut self) -> TransportEvent {
        if self.state == TransportState::Playing {
            self.state = TransportState::Paused;
            TransportEvent::Paused
        } else {
            TransportEvent::None
        }
    }

    /// Stops playback and returns the playhead to frame zero.
    pub fn stop(&mut self) -> TransportEvent {
        if self.state == TransportState::Stopped && self.position == Frame::ZERO {
            TransportEvent::None
        } else {
            self.state = TransportState::Stopped;
            self.position = Frame::ZERO;
            TransportEvent::Stopped
        }
    }

    /// Moves the playhead without changing play/pause state.
    ///
    /// # Errors
    ///
    /// Returns [`TimelineError::NegativePosition`] for a negative frame.
    pub fn seek(&mut self, position: Frame) -> Result<TransportEvent, TimelineError> {
        if position.get() < 0 {
            return Err(TimelineError::NegativePosition);
        }
        let from = self.position;
        self.position = position;
        Ok(if from == position {
            TransportEvent::None
        } else {
            TransportEvent::Seeked { from, to: position }
        })
    }

    pub fn set_loop(&mut self, region: Option<LoopRegion>) {
        self.loop_region = region;
    }

    /// Advances without allocating. When looping, the returned end is the wrapped playhead.
    pub fn advance(&mut self, frames: u64) -> TransportAdvance {
        let start = self.position;
        if self.state != TransportState::Playing || frames == 0 {
            return TransportAdvance {
                start,
                end: start,
                advanced_frames: 0,
                wraps: 0,
            };
        }

        let (end, wraps) = if let Some(region) = self.loop_region {
            let loop_start = region.start.get();
            let loop_end = region.end.get();
            let length = region.length_frames();
            let position = self.position.get();
            if position < loop_start {
                let before_loop = (loop_start - position).cast_unsigned();
                if frames <= before_loop {
                    let delta = i64::try_from(frames).unwrap_or(i64::MAX);
                    let end = Frame(position.saturating_add(delta));
                    self.position = end;
                    return TransportAdvance {
                        start,
                        end,
                        advanced_frames: frames,
                        wraps: 0,
                    };
                }
                let total = u128::from(frames - before_loop);
                let wraps = u64::try_from((total / u128::from(length)).min(u128::from(u64::MAX)))
                    .unwrap_or(u64::MAX);
                let wrapped = i64::try_from(total % u128::from(length)).unwrap_or(i64::MAX);
                (Frame(loop_start.saturating_add(wrapped)), wraps)
            } else {
                let initial = if position >= loop_end {
                    loop_start
                } else {
                    position
                };
                let offset = (initial - loop_start).cast_unsigned();
                let total = u128::from(offset) + u128::from(frames);
                let wraps = u64::try_from((total / u128::from(length)).min(u128::from(u64::MAX)))
                    .unwrap_or(u64::MAX);
                let wrapped = i64::try_from(total % u128::from(length)).unwrap_or(i64::MAX);
                (Frame(loop_start.saturating_add(wrapped)), wraps)
            }
        } else {
            let delta = i64::try_from(frames).unwrap_or(i64::MAX);
            (Frame(self.position.get().saturating_add(delta)), 0)
        };
        self.position = end;
        TransportAdvance {
            start,
            end,
            advanced_frames: frames,
            wraps,
        }
    }
}

impl fmt::Display for Beat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_beats_and_frames_at_constant_tempo() {
        let tempo = Tempo::new(120.0, 48_000).unwrap();
        assert_eq!(
            tempo.frame_at(Beat::new(1.0).unwrap()).unwrap(),
            Frame(24_000)
        );
        assert_eq!(
            tempo.frame_at(Beat::new(-0.5).unwrap()).unwrap(),
            Frame(-12_000)
        );
        assert_eq!(tempo.frame_to_beat(Frame(72_000)), Beat(3.0));
    }

    #[test]
    fn exposes_explicit_boundary_rounding() {
        let tempo = Tempo::new(60.0, 10).unwrap();
        let beat = Beat::new(0.15).unwrap();
        assert_eq!(
            tempo.beat_to_frame(beat, FrameRounding::Floor).unwrap(),
            Frame(1)
        );
        assert_eq!(
            tempo.beat_to_frame(beat, FrameRounding::Nearest).unwrap(),
            Frame(2)
        );
        assert_eq!(
            tempo.beat_to_frame(beat, FrameRounding::Ceil).unwrap(),
            Frame(2)
        );
    }

    #[test]
    fn transport_play_pause_stop_and_seek_semantics() {
        let mut transport = Transport::default();
        assert_eq!(transport.play(), TransportEvent::Started);
        assert_eq!(transport.advance(128).end, Frame(128));
        assert_eq!(transport.pause(), TransportEvent::Paused);
        assert_eq!(transport.advance(128).advanced_frames, 0);
        assert!(matches!(
            transport.seek(Frame(64)),
            Ok(TransportEvent::Seeked { .. })
        ));
        assert_eq!(transport.stop(), TransportEvent::Stopped);
        assert_eq!(transport.position(), Frame::ZERO);
    }

    #[test]
    fn looping_handles_boundary_and_multiple_wraps() {
        let mut transport = Transport::default();
        transport.set_loop(Some(LoopRegion::new(Frame(10), Frame(20)).unwrap()));
        transport.seek(Frame(18)).unwrap();
        transport.play();
        let advance = transport.advance(25);
        assert_eq!(advance.end, Frame(13));
        assert_eq!(advance.wraps, 3);
    }

    #[test]
    fn rejects_invalid_units_and_regions() {
        assert_eq!(Tempo::new(0.0, 48_000), Err(TimelineError::InvalidBpm));
        assert_eq!(Beat::new(f64::NAN), Err(TimelineError::NonFiniteBeat));
        assert_eq!(
            LoopRegion::new(Frame(3), Frame(3)),
            Err(TimelineError::InvalidLoop)
        );
        assert_eq!(
            Transport::default().seek(Frame(-1)),
            Err(TimelineError::NegativePosition)
        );
    }
}
