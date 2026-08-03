#![allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BpmDetection {
    pub bpm: f32,
    pub confidence: f32,
    pub alternatives: [Option<f32>; 2],
}

/// Detects the dominant BPM of a canonical WAV using the pure-Rust
/// `bpm-finder-tools` onset/autocorrelation analyzer.
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
    let result = bpm_finder_tools::file::analyze_samples(&samples, spec.sample_rate, 60.0, 200.0)
        .map_err(|error| error.to_string())?;
    Ok(BpmDetection {
        bpm: result.bpm as f32,
        confidence: result.confidence as f32,
        alternatives: [
            octave_candidate(result.bpm / 2.0),
            octave_candidate(result.bpm * 2.0),
        ],
    })
}

fn octave_candidate(bpm: f64) -> Option<f32> {
    (40.0..=240.0).contains(&bpm).then_some(bpm as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(result.confidence > 0.5);
    }
}
