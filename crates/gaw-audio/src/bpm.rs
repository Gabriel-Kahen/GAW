#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

use std::path::Path;

use rustfft::{FftPlanner, num_complex::Complex};

const FRAME_SIZE: usize = 1_024;
const HOP_SIZE: usize = 256;
const MIN_SECONDS: usize = 3;
const MIN_BPM: f64 = 40.0;
const MAX_BPM: f64 = 240.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BpmDetection {
    pub bpm: f32,
    /// Probability mass of the winning half/single/double-time family.
    pub confidence: f32,
    /// Probability mass of the strongest unrelated tempo family.
    pub runner_up_confidence: f32,
    pub alternatives: [Option<f32>; 2],
}

/// Detects the dominant BPM family of a canonical WAV. Half-, single-, and
/// double-time peaks are grouped before confidence is calculated.
///
/// # Errors
/// Returns an error when the WAV cannot be read or is too short/ambiguous for
/// the analyzer to produce a result.
pub fn detect_bpm_wav(path: &Path) -> Result<BpmDetection, String> {
    const MAX_SECONDS: u64 = 120;
    let mut reader = hound::WavReader::open(path).map_err(|error| error.to_string())?;
    let spec = reader.spec();
    let channels = usize::from(spec.channels.max(1));
    let max_frames = usize::try_from(u64::from(spec.sample_rate) * MAX_SECONDS)
        .map_err(|_| "audio file is too large to analyze".to_owned())?;
    let mut samples = Vec::with_capacity(max_frames.min(1_000_000));
    let mut frame = Vec::with_capacity(channels);
    match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = 2_f32.powi(i32::from(spec.bits_per_sample.saturating_sub(1)));
            for sample in reader.samples::<i32>() {
                frame.push(sample.map_err(|error| error.to_string())? as f32 / scale);
                if frame.len() == channels {
                    samples.push(frame.iter().copied().sum::<f32>() / channels as f32);
                    frame.clear();
                    if samples.len() >= max_frames {
                        break;
                    }
                }
            }
        }
        hound::SampleFormat::Float => {
            for sample in reader.samples::<f32>() {
                frame.push(sample.map_err(|error| error.to_string())?);
                if frame.len() == channels {
                    samples.push(frame.iter().copied().sum::<f32>() / channels as f32);
                    frame.clear();
                    if samples.len() >= max_frames {
                        break;
                    }
                }
            }
        }
    }
    detect_bpm_samples(&samples, spec.sample_rate)
}

#[derive(Clone, Copy, Debug)]
struct Family {
    bpm: f64,
    score: f64,
}

fn detect_bpm_samples(samples: &[f32], sample_rate: u32) -> Result<BpmDetection, String> {
    if sample_rate == 0 || samples.len() < sample_rate as usize * MIN_SECONDS {
        return Err("audio is too short for BPM detection".to_owned());
    }
    let downsample_factor = (sample_rate / 12_000).max(1) as usize;
    let analysis_rate = f64::from(sample_rate) / downsample_factor as f64;
    let analysis_samples = samples
        .chunks(downsample_factor)
        .map(|chunk| chunk.iter().copied().sum::<f32>() / chunk.len() as f32)
        .collect::<Vec<_>>();
    let envelope = onset_envelope(&analysis_samples);
    if envelope.len() < 16 {
        return Err("audio is too short for BPM detection".to_owned());
    }
    let autocorrelation = autocorrelation(&envelope);
    let frame_rate = analysis_rate / HOP_SIZE as f64;
    let min_lag = ((60.0 * frame_rate) / MAX_BPM).floor().max(1.0) as usize;
    let max_lag = ((60.0 * frame_rate) / MIN_BPM).ceil() as usize;
    if max_lag + 1 >= autocorrelation.len() {
        return Err("audio is too short for BPM detection".to_owned());
    }
    let scores = (min_lag..=max_lag)
        .map(|lag| {
            let overlap = envelope.len() - lag;
            (autocorrelation[lag] / overlap as f64).max(0.0)
        })
        .collect::<Vec<_>>();
    let peak_score = scores.iter().copied().fold(0.0, f64::max);
    let zero_score = autocorrelation[0] / envelope.len() as f64;
    if peak_score <= f64::EPSILON || zero_score <= f64::EPSILON || peak_score / zero_score < 0.03 {
        return Err("no stable rhythmic pulse was found".to_owned());
    }
    let mut sorted_scores = scores.clone();
    sorted_scores.sort_by(f64::total_cmp);
    let baseline = sorted_scores[sorted_scores.len() / 2];
    let minimum_peak = peak_score * 0.10;
    let mut families = Vec::<Family>::new();
    for lag in min_lag..=max_lag {
        let index = lag - min_lag;
        let score = scores[index];
        let left = index.checked_sub(1).map_or(0.0, |value| scores[value]);
        let right = scores.get(index + 1).copied().unwrap_or(0.0);
        if score < minimum_peak || score < left || score < right {
            continue;
        }
        let weight = (score - baseline).max(0.0).powi(2);
        if weight <= f64::EPSILON {
            continue;
        }
        let bpm = normalize_family((60.0 * frame_rate) / lag as f64);
        add_family_evidence(&mut families, bpm, weight);
    }
    let total_score = families.iter().map(|family| family.score).sum::<f64>();
    families.sort_by(|left, right| right.score.total_cmp(&left.score));
    let family = families
        .first()
        .ok_or_else(|| "no stable rhythmic pulse was found".to_owned())?;
    if total_score <= f64::EPSILON {
        return Err("no stable rhythmic pulse was found".to_owned());
    }
    let bpm = family.bpm;
    Ok(BpmDetection {
        bpm: bpm as f32,
        confidence: (family.score / total_score).clamp(0.0, 1.0) as f32,
        runner_up_confidence: families
            .get(1)
            .map_or(0.0, |runner_up| runner_up.score / total_score)
            as f32,
        alternatives: [octave_candidate(bpm / 2.0), octave_candidate(bpm * 2.0)],
    })
}

fn add_family_evidence(families: &mut Vec<Family>, bpm: f64, weight: f64) {
    if let Some(family) = families
        .iter_mut()
        .find(|family| ((family.bpm - bpm) / family.bpm).abs() <= 0.025)
    {
        family.bpm = (family.bpm * family.score + bpm * weight) / (family.score + weight);
        family.score += weight;
    } else {
        families.push(Family { bpm, score: weight });
    }
}

fn onset_envelope(samples: &[f32]) -> Vec<f64> {
    if samples.len() < FRAME_SIZE {
        return Vec::new();
    }
    let mut planner = FftPlanner::<f64>::new();
    let fft = planner.plan_fft_forward(FRAME_SIZE);
    let window = (0..FRAME_SIZE)
        .map(|index| {
            0.5 - 0.5 * (std::f64::consts::TAU * index as f64 / (FRAME_SIZE - 1) as f64).cos()
        })
        .collect::<Vec<_>>();
    let mut spectrum = vec![Complex::new(0.0, 0.0); FRAME_SIZE];
    let mut previous = vec![0.0; FRAME_SIZE / 2 + 1];
    let mut envelope = Vec::with_capacity((samples.len() - FRAME_SIZE) / HOP_SIZE + 1);
    for frame in samples.windows(FRAME_SIZE).step_by(HOP_SIZE) {
        for ((value, sample), weight) in spectrum.iter_mut().zip(frame).zip(&window) {
            *value = Complex::new(f64::from(*sample) * weight, 0.0);
        }
        fft.process(&mut spectrum);
        let mut flux = 0.0;
        for (prior, bin) in previous.iter_mut().zip(&spectrum) {
            let magnitude = bin.norm().ln_1p();
            flux += (magnitude - *prior).max(0.0);
            *prior = magnitude;
        }
        envelope.push(flux);
    }
    let mean = envelope.iter().sum::<f64>() / envelope.len() as f64;
    envelope
        .into_iter()
        .map(|value| (value - mean).max(0.0))
        .collect()
}

fn autocorrelation(signal: &[f64]) -> Vec<f64> {
    let fft_len = (signal.len() * 2).next_power_of_two();
    let mut values = vec![Complex::new(0.0, 0.0); fft_len];
    for (value, sample) in values.iter_mut().zip(signal) {
        value.re = *sample;
    }
    let mut planner = FftPlanner::<f64>::new();
    planner.plan_fft_forward(fft_len).process(&mut values);
    for value in &mut values {
        *value = Complex::new(value.norm_sqr(), 0.0);
    }
    planner.plan_fft_inverse(fft_len).process(&mut values);
    let scale = 1.0 / fft_len as f64;
    values
        .into_iter()
        .take(signal.len())
        .map(|value| value.re * scale)
        .collect()
}

fn normalize_family(mut bpm: f64) -> f64 {
    while bpm < 80.0 {
        bpm *= 2.0;
    }
    while bpm > 160.0 {
        bpm /= 2.0;
    }
    bpm
}

fn octave_candidate(bpm: f64) -> Option<f32> {
    (40.0..=240.0).contains(&bpm).then_some(bpm as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mixed_clicks(bpms: &[f64]) -> Vec<f32> {
        let sample_rate = 48_000_u32;
        (0..sample_rate * 10)
            .map(|frame| {
                let time = f64::from(frame) / f64::from(sample_rate);
                let hits = bpms
                    .iter()
                    .filter(|bpm| (time * **bpm / 60.0).fract() < 0.01)
                    .count();
                hits as f32 / bpms.len() as f32
            })
            .collect()
    }

    #[test]
    fn detects_a_regular_click_track() {
        let path = std::env::temp_dir().join(format!("gaw-bpm-test-{}.wav", std::process::id()));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).expect("create test wav");
        for frame in 0..(spec.sample_rate * 8) {
            let beat_frame = frame % (spec.sample_rate / 2);
            let sample = if beat_frame < 240 { i16::MAX } else { 0 };
            writer.write_sample(sample).expect("write test sample");
        }
        writer.finalize().expect("finalize test wav");
        let result = detect_bpm_wav(&path).expect("detect test BPM");
        std::fs::remove_file(path).expect("remove test wav");
        assert!((result.bpm - 120.0).abs() < 2.0);
        assert!(result.confidence > 0.65);
        assert!(result.confidence - result.runner_up_confidence > 0.15);
    }

    #[test]
    fn half_single_and_double_time_share_probability_mass() {
        let mut families = Vec::new();
        for (bpm, probability) in [(60.0, 0.25), (120.0, 0.35), (240.0, 0.20), (93.0, 0.20)] {
            add_family_evidence(&mut families, normalize_family(bpm), probability);
        }
        families.sort_by(|left, right| right.score.total_cmp(&left.score));
        assert_eq!(families.len(), 2);
        assert!((families[0].bpm - 120.0).abs() < f64::EPSILON);
        assert!((families[0].score - 0.80).abs() < f64::EPSILON);
        assert!((families[1].score - 0.20).abs() < f64::EPSILON);
    }

    #[test]
    fn unrelated_pulse_families_remain_ambiguous() {
        let result = detect_bpm_samples(&mixed_clicks(&[100.0, 127.0]), 48_000)
            .expect("mixed pulse analysis");
        assert!(result.confidence < 0.55 || result.confidence - result.runner_up_confidence < 0.15);
    }
}
