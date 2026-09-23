//! Bounded capture tap and control-thread pitch detection for standard bass tuning.

use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;

const CAPACITY: usize = 2048;
const WINDOW: usize = 1024;
const STALE: Duration = Duration::from_millis(400);
const INTERVAL: Duration = Duration::from_millis(45);
const STRINGS: [f32; 4] = [41.203_445, 55.0, 73.416_19, 97.998_856];

/// Pitch relative to the closest standard four-string bass string (E1 A1 D2 G2).
#[derive(Clone, Copy, Debug)]
pub struct BassTunerReading {
    /// Zero-based string index: E, A, D, G.
    pub string_index: usize,
    pub frequency_hz: f32,
    /// Negative means flat; positive means sharp.
    pub cents: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub epoch: u64,
    pub stream: u64,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Sample {
    pub value: f32,
    pub identity: Identity,
    pub sequence: u64,
    pub rate: f32,
    pub captured: Instant,
}

#[derive(Debug)]
pub(crate) struct Tuner {
    enabled: AtomicBool,
    generation: AtomicU64,
    sender: Sender<Sample>,
    receiver: Receiver<Sample>,
    // Only tuner_reading (the control thread) acquires this lock.
    detector: Mutex<Detector>,
}

impl Tuner {
    pub fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::bounded(CAPACITY);
        Self {
            enabled: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            sender,
            receiver,
            detector: Mutex::new(Detector::default()),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        if enabled {
            self.enabled.store(true, Ordering::Release);
        } else {
            self.enabled.store(false, Ordering::Release);
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    pub fn generation(&self) -> Option<u64> {
        self.enabled
            .load(Ordering::Acquire)
            .then(|| self.generation.load(Ordering::Acquire))
    }

    pub fn push(&self, sample: Sample) {
        if self.sender.try_send(sample).is_err() {
            let _ = self.receiver.try_recv();
            let _ = self.sender.try_send(sample);
        }
    }

    pub fn reading(&self, epoch: u64, stream: u64) -> Option<BassTunerReading> {
        let generation = self.generation()?;
        let identity = Identity {
            epoch,
            stream,
            generation,
        };
        let mut detector = self.detector.lock();
        let now = Instant::now();
        if detector.identity != Some(identity) {
            *detector = Detector::default();
            detector.identity = Some(identity);
        }
        for _ in 0..CAPACITY {
            let Ok(sample) = self.receiver.try_recv() else {
                break;
            };
            if sample.identity != identity || now.duration_since(sample.captured) > STALE {
                continue;
            }
            if detector
                .sequence
                .is_some_and(|last| sample.sequence != last + 1)
            {
                detector.samples.clear();
                detector.reading = None;
            }
            if detector.samples.len() == WINDOW {
                detector.samples.pop_front();
            }
            detector.samples.push_back(sample.value);
            detector.sequence = Some(sample.sequence);
            detector.captured = Some(sample.captured);
            detector.rate = sample.rate;
            detector.dirty = true;
        }
        if detector
            .captured
            .is_none_or(|captured| now.duration_since(captured) > STALE)
        {
            detector.samples.clear();
            detector.reading = None;
        } else if detector.dirty
            && detector
                .analyzed
                .is_none_or(|last| now.duration_since(last) >= INTERVAL)
        {
            let rate = detector.rate;
            detector.reading = detect(detector.samples.make_contiguous(), rate);
            detector.analyzed = Some(now);
            detector.dirty = false;
        }
        // Invalidation may race a control-thread read; never publish the old generation.
        if self.generation() == Some(generation) {
            detector.reading
        } else {
            None
        }
    }
}

#[derive(Default, Debug)]
struct Detector {
    identity: Option<Identity>,
    samples: VecDeque<f32>,
    sequence: Option<u64>,
    captured: Option<Instant>,
    analyzed: Option<Instant>,
    rate: f32,
    dirty: bool,
    reading: Option<BassTunerReading>,
}

/// YIN cumulative normalized difference, with a conservative periodicity gate.
/// The tap runs near 4 kHz, providing at least eight low-E cycles per window.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn detect(samples: &[f32], rate: f32) -> Option<BassTunerReading> {
    if samples.len() < WINDOW || !rate.is_finite() || rate < 1000.0 {
        return None;
    }
    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    let power = samples
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f32>()
        / samples.len() as f32;
    if !power.is_finite() || power < 0.000_001 {
        return None;
    }
    let min_lag = (rate / 115.0).floor() as usize;
    let max_lag = ((rate / 32.0).ceil() as usize).min(samples.len() / 3);
    if min_lag >= max_lag {
        return None;
    }
    let comparison = samples.len() - max_lag - 1;
    let mut difference = vec![1.0_f32; max_lag + 2];
    let mut sum = 0.0;
    for lag in 1..difference.len() {
        let delta = (0..comparison)
            .map(|i| (samples[i] - samples[i + lag]).powi(2))
            .sum::<f32>();
        sum += delta;
        difference[lag] = if sum > 0.0 {
            delta * lag as f32 / sum
        } else {
            1.0
        };
    }
    let best = difference[min_lag..=max_lag]
        .iter()
        .copied()
        .fold(1.0_f32, f32::min);
    if best > 0.15 {
        return None;
    }
    // A stricter gate for clean signals avoids locking to a louder second harmonic.
    let threshold = (best * 2.0).clamp(0.02, 0.1);
    let lag = (min_lag..=max_lag).find(|&lag| {
        difference[lag] <= threshold
            && difference[lag] <= difference[lag - 1]
            && difference[lag] < difference[lag + 1]
    })?;
    let left = difference[lag - 1];
    let center = difference[lag];
    let right = difference[lag + 1];
    let denominator = left - 2.0 * center + right;
    let offset = if denominator.abs() > f32::EPSILON {
        (0.5 * (left - right) / denominator).clamp(-0.5, 0.5)
    } else {
        0.0
    };
    let frequency_hz = rate / (lag as f32 + offset);
    if !(32.0..=115.0).contains(&frequency_hz) {
        return None;
    }
    let (string_index, cents) = STRINGS
        .iter()
        .enumerate()
        .map(|(index, target)| (index, 1200.0 * (frequency_hz / target).log2()))
        .min_by(|left, right| left.1.abs().total_cmp(&right.1.abs()))?;
    Some(BassTunerReading {
        string_index,
        frequency_hz,
        cents,
    })
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::*;

    fn tone(frequency: f32, rate: f32, harmonics: bool) -> Vec<f32> {
        (0..WINDOW)
            .map(|i| {
                let phase = std::f32::consts::TAU * frequency * i as f32 / rate;
                if harmonics {
                    0.12 * phase.sin() + 0.6 * (2.0 * phase).sin() + 0.15 * (3.0 * phase).sin()
                } else {
                    0.5 * phase.sin()
                }
            })
            .collect()
    }

    fn feed(tuner: &Tuner, identity: Identity, frequency: f32, captured: Instant) {
        for (sequence, value) in tone(frequency, 4000.0, false).into_iter().enumerate() {
            tuner.push(Sample {
                value,
                identity,
                sequence: sequence as u64,
                rate: 4000.0,
                captured,
            });
        }
    }

    #[test]
    fn lifecycle_and_stale_capture_never_reuse_old_pitch() {
        let tuner = Tuner::new();
        tuner.set_enabled(true);
        let mut identity = Identity {
            epoch: 0,
            stream: 1,
            generation: tuner.generation().unwrap(),
        };
        feed(&tuner, identity, STRINGS[0], Instant::now());
        assert_eq!(tuner.reading(0, 1).unwrap().string_index, 0);
        tuner.detector.lock().captured = Some(
            Instant::now()
                .checked_sub(STALE + Duration::from_millis(1))
                .unwrap(),
        );
        assert!(tuner.reading(0, 1).is_none());
        feed(
            &tuner,
            identity,
            STRINGS[0],
            Instant::now()
                .checked_sub(STALE + Duration::from_millis(1))
                .unwrap(),
        );
        assert!(tuner.reading(0, 1).is_none());
        feed(&tuner, identity, STRINGS[0], Instant::now());
        tuner.set_enabled(false);
        assert!(tuner.reading(0, 1).is_none());
        tuner.set_enabled(true);
        assert!(tuner.reading(0, 1).is_none());
        identity.generation = tuner.generation().unwrap();
        feed(&tuner, identity, STRINGS[1], Instant::now());
        assert_eq!(tuner.reading(0, 1).unwrap().string_index, 1);
        assert!(tuner.reading(0, 2).is_none());
        feed(&tuner, identity, STRINGS[1], Instant::now());
        assert!(tuner.reading(0, 2).is_none());
        identity.stream = 2;
        feed(&tuner, identity, STRINGS[3], Instant::now());
        assert_eq!(tuner.reading(0, 2).unwrap().string_index, 3);
        assert!(tuner.reading(1, 2).is_none());
    }

    #[test]
    fn overflow_is_bounded_and_recent_contiguous_audio_wins() {
        let tuner = Tuner::new();
        tuner.set_enabled(true);
        let identity = Identity {
            epoch: 0,
            stream: 1,
            generation: tuner.generation().unwrap(),
        };
        for _ in 0..4 {
            feed(&tuner, identity, STRINGS[0], Instant::now());
        }
        feed(&tuner, identity, STRINGS[2], Instant::now());
        assert_eq!(tuner.receiver.len(), CAPACITY);
        assert_eq!(tuner.reading(0, 1).unwrap().string_index, 2);
    }

    #[test]
    fn detects_all_strings_and_detuning_with_dominant_harmonics() {
        for rate in [4000.0, 4410.0, 4800.0] {
            for (index, frequency) in STRINGS.into_iter().enumerate() {
                for cents in [-40.0_f32, 0.0, 35.0] {
                    for harmonics in [false, true] {
                        let frequency = frequency * 2.0_f32.powf(cents / 1200.0);
                        let reading = detect(&tone(frequency, rate, harmonics), rate).unwrap();
                        assert_eq!(
                            reading.string_index, index,
                            "{reading:?}, expected {index} at {frequency} Hz, sample rate {rate}, harmonics {harmonics}"
                        );
                        assert!(
                            (reading.cents - cents).abs() < 2.0,
                            "{reading:?}, expected {cents} cents at {rate} Hz"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_silence_dc_noise_and_insufficient_capture() {
        assert!(detect(&[0.0; WINDOW], 4000.0).is_none());
        assert!(detect(&[0.4; WINDOW], 4000.0).is_none());
        assert!(detect(&[f32::NAN; WINDOW], 4000.0).is_none());
        assert!(detect(&[0.1; WINDOW / 2], 4000.0).is_none());
        let mut random = 42_u32;
        let noise: Vec<_> = (0..WINDOW)
            .map(|_| {
                random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                random as f32 / u32::MAX as f32 - 0.5
            })
            .collect();
        assert!(detect(&noise, 4000.0).is_none());
    }
}
