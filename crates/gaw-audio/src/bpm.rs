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
const ANALYSIS_RATE: u32 = 12_000;
const WINDOW_SECONDS: f64 = 16.0;
const WINDOW_HOP_SECONDS: f64 = 8.0;
const MIN_REGION_SECONDS: f64 = 16.0;
const RELIABLE_CONFIDENCE: f32 = 0.55;
const RELIABLE_MARGIN: f32 = 0.15;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BpmDetection {
    pub bpm: f32,
    /// Probability mass of the winning half/single/double-time family.
    pub confidence: f32,
    /// Probability mass of the strongest unrelated tempo family.
    pub runner_up_confidence: f32,
    pub alternatives: [Option<f32>; 2],
}

/// A stable, constant-tempo interval within an audio asset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoRegion {
    /// Inclusive start of the interval in seconds.
    pub start_seconds: f64,
    /// Exclusive end of the interval in seconds.
    pub end_seconds: f64,
    /// Dominant tempo family and its ambiguity within this interval.
    pub detection: BpmDetection,
}

/// A contiguous interval in a full-asset tempo map.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoSection {
    /// Inclusive start of the interval in seconds.
    pub start_seconds: f64,
    /// Exclusive end of the interval in seconds.
    pub end_seconds: f64,
    /// A trustworthy tempo family, or `None` when this interval is uncertain.
    pub detection: Option<BpmDetection>,
}

/// Why an asset did not yield one or more trustworthy constant-tempo regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TempoUnreliableReason {
    /// There was not enough repeatable rhythmic energy.
    WeakPulse,
    /// Two unrelated tempo families had similar probability mass.
    CompetingTempos,
    /// Local estimates changed too often to form sustained regions.
    UnstableTempo,
}

/// Details retained when tempo analysis is too ambiguous to apply.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TempoUnreliable {
    /// Best whole-asset estimate, when one could still be measured.
    pub best: Option<BpmDetection>,
    /// The conservative rejection reason.
    pub reason: TempoUnreliableReason,
}

/// Result of analyzing an entire asset for constant or changing tempo.
#[derive(Clone, Debug, PartialEq)]
pub enum TempoAnalysis {
    /// One tempo family remains stable for the complete asset.
    Stable(TempoRegion),
    /// A full-asset map containing detected and possibly uncertain intervals.
    Sections(Vec<TempoSection>),
    /// No result was reliable enough to apply automatically.
    Unreliable(TempoUnreliable),
}

/// Detects stable tempo regions across a canonical WAV.
///
/// The analyzer uses overlapping windows, groups half/double-time evidence,
/// removes isolated local changes, requires each proposed region to persist,
/// and moves boundaries toward nearby onsets.
///
/// # Errors
/// Returns an error when the WAV cannot be decoded or is too short to analyze.
pub fn detect_tempo_wav(path: &Path) -> Result<TempoAnalysis, String> {
    let audio = read_analysis_wav(path, None)?;
    analyze_tempo_regions(&audio.samples, audio.sample_rate, audio.duration_seconds)
}

/// Detects the dominant BPM family of a canonical WAV. Half-, single-, and
/// double-time peaks are grouped before confidence is calculated.
///
/// # Errors
/// Returns an error when the WAV cannot be read or is too short/ambiguous for
/// the analyzer to produce a result.
pub fn detect_bpm_wav(path: &Path) -> Result<BpmDetection, String> {
    const MAX_SECONDS: u64 = 120;
    let audio = read_analysis_wav(path, Some(MAX_SECONDS))?;
    detect_bpm_samples(&audio.samples, audio.sample_rate)
}

struct AnalysisAudio {
    samples: Vec<f32>,
    sample_rate: u32,
    duration_seconds: f64,
}

fn read_analysis_wav(path: &Path, max_seconds: Option<u64>) -> Result<AnalysisAudio, String> {
    let mut reader = hound::WavReader::open(path).map_err(|error| error.to_string())?;
    let spec = reader.spec();
    if spec.sample_rate == 0 {
        return Err("WAV has an invalid sample rate".to_owned());
    }
    let channels = usize::from(spec.channels.max(1));
    let factor = (spec.sample_rate / ANALYSIS_RATE).max(1) as usize;
    let analysis_rate = spec.sample_rate / u32::try_from(factor).unwrap_or(1);
    let max_frames =
        max_seconds.and_then(|seconds| usize::try_from(u64::from(spec.sample_rate) * seconds).ok());
    let mut samples = Vec::new();
    let mut frame = Vec::with_capacity(channels);
    let mut downsample_sum = 0.0_f32;
    let mut downsample_count = 0_usize;
    let mut source_frames = 0_usize;
    let mut push_sample = |sample: f32| {
        downsample_sum += sample;
        downsample_count += 1;
        source_frames += 1;
        if downsample_count == factor {
            samples.push(downsample_sum / downsample_count as f32);
            downsample_sum = 0.0;
            downsample_count = 0;
        }
        max_frames.is_some_and(|limit| source_frames >= limit)
    };
    match spec.sample_format {
        hound::SampleFormat::Int => {
            let scale = 2_f32.powi(i32::from(spec.bits_per_sample.saturating_sub(1)));
            for sample in reader.samples::<i32>() {
                frame.push(sample.map_err(|error| error.to_string())? as f32 / scale);
                if frame.len() == channels {
                    let stop = push_sample(frame.iter().copied().sum::<f32>() / channels as f32);
                    frame.clear();
                    if stop {
                        break;
                    }
                }
            }
        }
        hound::SampleFormat::Float => {
            for sample in reader.samples::<f32>() {
                frame.push(sample.map_err(|error| error.to_string())?);
                if frame.len() == channels {
                    let stop = push_sample(frame.iter().copied().sum::<f32>() / channels as f32);
                    frame.clear();
                    if stop {
                        break;
                    }
                }
            }
        }
    }
    if downsample_count != 0 {
        samples.push(downsample_sum / downsample_count as f32);
    }
    Ok(AnalysisAudio {
        samples,
        sample_rate: analysis_rate,
        duration_seconds: source_frames as f64 / f64::from(spec.sample_rate),
    })
}

#[derive(Clone, Copy, Debug)]
struct Family {
    bpm: f64,
    score: f64,
}

#[derive(Clone, Copy, Debug)]
struct WindowDetection {
    center_seconds: f64,
    detection: Option<BpmDetection>,
}

fn analyze_tempo_regions(
    samples: &[f32],
    sample_rate: u32,
    duration_seconds: f64,
) -> Result<TempoAnalysis, String> {
    if sample_rate == 0 || duration_seconds < MIN_SECONDS as f64 {
        return Err("audio is too short for tempo detection".to_owned());
    }
    let whole = detect_bpm_samples(samples, sample_rate).ok();
    if duration_seconds < WINDOW_SECONDS + WINDOW_HOP_SECONDS {
        return Ok(single_or_unreliable(whole, duration_seconds));
    }

    let window_frames = (WINDOW_SECONDS * f64::from(sample_rate)).round() as usize;
    let hop_frames = (WINDOW_HOP_SECONDS * f64::from(sample_rate)).round() as usize;
    let mut windows = Vec::new();
    for start in (0..=samples.len().saturating_sub(window_frames)).step_by(hop_frames.max(1)) {
        let detection = detect_bpm_samples(&samples[start..start + window_frames], sample_rate)
            .ok()
            .filter(is_locally_reliable);
        windows.push(WindowDetection {
            center_seconds: (start + window_frames / 2) as f64 / f64::from(sample_rate),
            detection,
        });
    }
    smooth_window_outliers(&mut windows);
    classify_window_runs(&mut windows);
    let detected_families = windows.iter().filter_map(|window| window.detection).fold(
        Vec::<f32>::new(),
        |mut families, detection| {
            if !families.iter().any(|bpm| same_family(*bpm, detection.bpm)) {
                families.push(detection.bpm);
            }
            families
        },
    );
    if detected_families.is_empty() {
        return Ok(TempoAnalysis::Unreliable(TempoUnreliable {
            best: whole,
            reason: whole.map_or(TempoUnreliableReason::WeakPulse, |detection| {
                if detection.confidence - detection.runner_up_confidence < RELIABLE_MARGIN {
                    TempoUnreliableReason::CompetingTempos
                } else {
                    TempoUnreliableReason::WeakPulse
                }
            }),
        }));
    }
    if windows.iter().all(|window| window.detection.is_some()) && detected_families.len() == 1 {
        let detection = aggregate_run(&windows);
        if is_reliable(&detection) {
            return Ok(TempoAnalysis::Stable(TempoRegion {
                start_seconds: 0.0,
                end_seconds: duration_seconds,
                detection,
            }));
        }
    }
    Ok(TempoAnalysis::Sections(build_sections(
        &windows,
        samples,
        sample_rate,
        duration_seconds,
    )))
}

fn single_or_unreliable(whole: Option<BpmDetection>, duration_seconds: f64) -> TempoAnalysis {
    match whole {
        Some(detection) if is_reliable(&detection) => TempoAnalysis::Stable(TempoRegion {
            start_seconds: 0.0,
            end_seconds: duration_seconds,
            detection,
        }),
        best => TempoAnalysis::Unreliable(TempoUnreliable {
            reason: best.map_or(TempoUnreliableReason::WeakPulse, |detection| {
                if detection.confidence - detection.runner_up_confidence < RELIABLE_MARGIN {
                    TempoUnreliableReason::CompetingTempos
                } else {
                    TempoUnreliableReason::WeakPulse
                }
            }),
            best,
        }),
    }
}

fn is_reliable(detection: &BpmDetection) -> bool {
    detection.confidence >= RELIABLE_CONFIDENCE
        && detection.confidence - detection.runner_up_confidence >= RELIABLE_MARGIN
}

fn is_locally_reliable(detection: &BpmDetection) -> bool {
    detection.confidence >= 0.40 && detection.confidence - detection.runner_up_confidence >= 0.08
}

fn same_family(left: f32, right: f32) -> bool {
    let left = normalize_family(f64::from(left));
    let right = normalize_family(f64::from(right));
    ((left - right) / left).abs() <= 0.025
}

fn smooth_window_outliers(windows: &mut [WindowDetection]) {
    if windows.len() < 3 {
        return;
    }
    let original = windows.to_vec();
    for index in 1..windows.len() - 1 {
        let (Some(left), Some(right)) =
            (original[index - 1].detection, original[index + 1].detection)
        else {
            continue;
        };
        if !same_family(left.bpm, right.bpm) {
            continue;
        }
        let Some(middle) = original[index].detection else {
            continue;
        };
        if !same_family(left.bpm, middle.bpm) {
            windows[index].detection = Some(merge_detections(&[left, right]));
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct WindowRun {
    start: usize,
    end: usize,
}

fn classify_window_runs(windows: &mut [WindowDetection]) {
    let mut runs = Vec::<WindowRun>::new();
    let mut index = 0;
    while index < windows.len() {
        let Some(detection) = windows[index].detection else {
            index += 1;
            continue;
        };
        let start = index;
        index += 1;
        while index < windows.len()
            && windows[index]
                .detection
                .is_some_and(|next| same_family(detection.bpm, next.bpm))
        {
            index += 1;
        }
        runs.push(WindowRun {
            start,
            end: index - 1,
        });
    }
    let minimum_observations = (MIN_REGION_SECONDS / WINDOW_HOP_SECONDS).ceil() as usize;
    for run in runs {
        let detection = aggregate_run(&windows[run.start..=run.end]);
        let keep = run.end + 1 - run.start >= minimum_observations && is_reliable(&detection);
        for window in &mut windows[run.start..=run.end] {
            window.detection = keep.then_some(detection);
        }
    }
}

fn build_sections(
    windows: &[WindowDetection],
    samples: &[f32],
    sample_rate: u32,
    duration_seconds: f64,
) -> Vec<TempoSection> {
    let envelope = onset_envelope(samples);
    let mut boundaries = Vec::with_capacity(windows.len().saturating_sub(1));
    for pair in windows.windows(2) {
        let nominal = (pair[0].center_seconds + pair[1].center_seconds) * 0.5;
        let different_detected_families = match (pair[0].detection, pair[1].detection) {
            (Some(left), Some(right)) => !same_family(left.bpm, right.bpm),
            _ => false,
        };
        boundaries.push(if different_detected_families {
            refine_boundary_to_onset(nominal, &envelope, sample_rate, duration_seconds)
        } else {
            nominal.clamp(0.0, duration_seconds)
        });
    }

    let mut sections = Vec::<TempoSection>::new();
    for (index, window) in windows.iter().enumerate() {
        let start_seconds = index.checked_sub(1).map_or(0.0, |prior| boundaries[prior]);
        let end_seconds = boundaries.get(index).copied().unwrap_or(duration_seconds);
        let merge =
            sections
                .last()
                .is_some_and(|previous| match (previous.detection, window.detection) {
                    (None, None) => true,
                    (Some(left), Some(right)) => same_family(left.bpm, right.bpm),
                    _ => false,
                });
        if merge {
            sections.last_mut().expect("section exists").end_seconds = end_seconds;
        } else {
            sections.push(TempoSection {
                start_seconds,
                end_seconds,
                detection: window.detection,
            });
        }
    }
    sections
}

fn aggregate_run(windows: &[WindowDetection]) -> BpmDetection {
    let detections = windows
        .iter()
        .filter_map(|window| window.detection)
        .collect::<Vec<_>>();
    merge_detections(&detections)
}

fn merge_detections(detections: &[BpmDetection]) -> BpmDetection {
    let count = detections.len().max(1) as f32;
    let bpm = detections
        .iter()
        .map(|detection| detection.bpm * detection.confidence)
        .sum::<f32>()
        / detections
            .iter()
            .map(|detection| detection.confidence)
            .sum::<f32>()
            .max(f32::EPSILON);
    let confidence = detections
        .iter()
        .map(|detection| detection.confidence)
        .sum::<f32>()
        / count;
    let runner_up_confidence = detections
        .iter()
        .map(|detection| detection.runner_up_confidence)
        .sum::<f32>()
        / count;
    BpmDetection {
        bpm,
        confidence,
        runner_up_confidence,
        alternatives: [
            octave_candidate(f64::from(bpm) / 2.0),
            octave_candidate(f64::from(bpm) * 2.0),
        ],
    }
}

fn refine_boundary_to_onset(
    nominal_seconds: f64,
    envelope: &[f64],
    sample_rate: u32,
    duration_seconds: f64,
) -> f64 {
    if envelope.is_empty() {
        return nominal_seconds.clamp(0.0, duration_seconds);
    }
    let envelope_rate = f64::from(sample_rate) / HOP_SIZE as f64;
    let center = (nominal_seconds * envelope_rate).round() as usize;
    let radius = (1.5 * envelope_rate).round() as usize;
    let start = center.saturating_sub(radius);
    let end = center.saturating_add(radius).min(envelope.len() - 1);
    let local_mean = envelope[start..=end].iter().sum::<f64>() / (end + 1 - start) as f64;
    let Some((offset, peak)) = envelope[start..=end]
        .iter()
        .enumerate()
        .max_by(|left, right| left.1.total_cmp(right.1))
    else {
        return nominal_seconds;
    };
    if *peak < local_mean * 1.5 {
        nominal_seconds
    } else {
        ((start + offset) as f64 / envelope_rate).clamp(0.0, duration_seconds)
    }
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

    fn changing_clicks(first_bpm: f64, second_bpm: f64, seconds: u32) -> Vec<f32> {
        let sample_rate = 12_000_u32;
        let midpoint = f64::from(seconds) * 0.5;
        (0..sample_rate * seconds)
            .map(|frame| {
                let time = f64::from(frame) / f64::from(sample_rate);
                let (bpm, phase_time) = if time < midpoint {
                    (first_bpm, time)
                } else {
                    (second_bpm, time - midpoint)
                };
                if (phase_time * bpm / 60.0).fract() < 0.015 {
                    1.0
                } else {
                    0.0
                }
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

    #[test]
    fn reports_one_stable_region_for_a_constant_tempo() {
        let samples = changing_clicks(120.0, 120.0, 48);
        let result = analyze_tempo_regions(&samples, 12_000, 48.0).expect("tempo analysis");
        let TempoAnalysis::Stable(region) = result else {
            panic!("expected one stable region, got {result:?}");
        };
        assert!(region.start_seconds.abs() < f64::EPSILON);
        assert!((region.end_seconds - 48.0).abs() < f64::EPSILON);
        assert!((region.detection.bpm - 120.0).abs() < 2.0);
    }

    #[test]
    fn detects_two_sustained_unrelated_tempo_sections() {
        let samples = changing_clicks(100.0, 128.0, 64);
        let result = analyze_tempo_regions(&samples, 12_000, 64.0).expect("tempo analysis");
        let TempoAnalysis::Sections(sections) = result else {
            panic!("expected tempo sections, got {result:?}");
        };
        let detected = sections
            .iter()
            .filter_map(|section| section.detection.map(|detection| (section, detection)))
            .collect::<Vec<_>>();
        assert_eq!(detected.len(), 2);
        assert!((detected[0].1.bpm - 100.0).abs() < 3.0);
        assert!((detected[1].1.bpm - 128.0).abs() < 3.0);
        assert!(sections[0].start_seconds.abs() < f64::EPSILON);
        assert!((sections.last().unwrap().end_seconds - 64.0).abs() < f64::EPSILON);
        assert!(
            sections
                .windows(2)
                .all(|pair| { (pair[0].end_seconds - pair[1].start_seconds).abs() < f64::EPSILON })
        );
    }

    #[test]
    fn isolated_tempo_window_does_not_create_a_region() {
        let detection = |bpm| {
            Some(BpmDetection {
                bpm,
                confidence: 0.8,
                runner_up_confidence: 0.1,
                alternatives: [None, None],
            })
        };
        let mut windows = [
            WindowDetection {
                center_seconds: 8.0,
                detection: detection(120.0),
            },
            WindowDetection {
                center_seconds: 16.0,
                detection: detection(97.0),
            },
            WindowDetection {
                center_seconds: 24.0,
                detection: detection(120.0),
            },
        ];
        smooth_window_outliers(&mut windows);
        assert!(
            windows
                .iter()
                .all(|window| same_family(window.detection.unwrap().bpm, 120.0))
        );
    }

    #[test]
    fn uncertain_windows_remain_explicit_sections() {
        let detection = |bpm| {
            Some(BpmDetection {
                bpm,
                confidence: 0.8,
                runner_up_confidence: 0.1,
                alternatives: [None, None],
            })
        };
        let mut windows = [
            WindowDetection {
                center_seconds: 8.0,
                detection: detection(120.0),
            },
            WindowDetection {
                center_seconds: 16.0,
                detection: detection(120.0),
            },
            WindowDetection {
                center_seconds: 24.0,
                detection: None,
            },
            WindowDetection {
                center_seconds: 32.0,
                detection: detection(128.0),
            },
            WindowDetection {
                center_seconds: 40.0,
                detection: detection(128.0),
            },
        ];
        smooth_window_outliers(&mut windows);
        classify_window_runs(&mut windows);
        let sections = build_sections(&windows, &[], 12_000, 48.0);

        assert_eq!(sections.len(), 3);
        assert!(sections[0].detection.is_some());
        assert!(sections[1].detection.is_none());
        assert!(sections[2].detection.is_some());
        assert!(sections[0].start_seconds.abs() < f64::EPSILON);
        assert!((sections[2].end_seconds - 48.0).abs() < f64::EPSILON);
        assert!(
            sections
                .windows(2)
                .all(|pair| { (pair[0].end_seconds - pair[1].start_seconds).abs() < f64::EPSILON })
        );
    }
}
