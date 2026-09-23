//! Reproducible DSP-only audit: `cargo run -p gaw-dsp --release --example monitor_effect_latency`
//! Reports intentional processor delay separately from execution time. This is not hardware RTL.
use std::{hint::black_box, time::Instant};

use gaw_dsp::distortion::{ClipperConfig, Oversampling, SaturatorConfig};
use gaw_dsp::dynamics::LimiterConfig;
use gaw_dsp::{
    AudioLayout, BeatRepeat, Bitcrusher, Chorus, Clipper, Compressor, Delay, Expander, Filter,
    Flanger, Gain, Gate, Limiter, ParametricEq, Phaser, PitchQuality, PitchShift, PrepareSpec,
    ProcessContext, Processor, Reverb, RhythmicGate, Saturator, StereoTool, TransientShaper,
    TremoloAutopan,
};

fn processors() -> Vec<(&'static str, Box<dyn Processor>)> {
    let mut processors: Vec<(&str, Box<dyn Processor>)> = vec![
        ("gain", Box::new(Gain::default())),
        ("stereo", Box::new(StereoTool::default())),
        ("filter", Box::new(Filter::default())),
        ("eq", Box::new(ParametricEq::default())),
        ("compressor", Box::new(Compressor::default())),
        ("limiter", Box::new(Limiter::default())),
        ("gate", Box::new(Gate::default())),
        ("expander", Box::new(Expander::default())),
        ("transient", Box::new(TransientShaper::default())),
        ("saturator", Box::new(Saturator::default())),
        ("clipper", Box::new(Clipper::default())),
        ("bitcrusher", Box::new(Bitcrusher::default())),
        ("delay", Box::new(Delay::default())),
        ("reverb", Box::new(Reverb::default())),
        ("chorus", Box::new(Chorus::default())),
        ("flanger", Box::new(Flanger::default())),
        ("phaser", Box::new(Phaser::default())),
        ("tremolo", Box::new(TremoloAutopan::default())),
        ("pitch draft 0", Box::new(PitchShift::default())),
        ("rhythmic gate", Box::new(RhythmicGate::default())),
        ("beat repeat", Box::new(BeatRepeat::default())),
    ];
    for (label, quality, semitones) in [
        ("pitch draft +12", PitchQuality::Draft, 12.0),
        ("pitch spectral 0", PitchQuality::Signalsmith, 0.0),
        ("pitch spectral +12", PitchQuality::Signalsmith, 12.0),
    ] {
        let mut pitch = PitchShift::default();
        pitch.quality = quality;
        pitch.semitones = semitones;
        processors.push((label, Box::new(pitch)));
    }
    // Catalog defaults differ from DSP constructors, especially lookahead and
    // oversampling. Include the three latency-bearing catalog variants explicitly.
    let limiter = gaw_core::processors::LimiterParameters::default();
    processors.push((
        "limiter catalog",
        Box::new(Limiter::new(LimiterConfig {
            ceiling_db: limiter.ceiling_db,
            release_ms: limiter.release_ms,
            lookahead_ms: limiter.lookahead_ms,
            input_gain_db: limiter.input_gain_db,
            true_peak: limiter.true_peak,
        })),
    ));
    processors.push((
        "saturator catalog x2",
        Box::new(Saturator::new(SaturatorConfig {
            tone_hz: 8_000.0,
            oversampling: Oversampling::X2,
            ..SaturatorConfig::default()
        })),
    ));
    processors.push((
        "clipper catalog x4",
        Box::new(Clipper::new(ClipperConfig {
            output_ceiling_db: -1.0,
            oversampling: Oversampling::X4,
            ..ClipperConfig::default()
        })),
    ));
    processors
}

fn main() {
    const BLOCK: usize = 64;
    const RATE: u32 = 48_000;
    const BLOCKS: usize = 7_500;
    let spec = PrepareSpec {
        sample_rate: f64::from(RATE),
        max_block_size: BLOCK,
        input_layout: AudioLayout::Stereo,
        tempo_bpm: 120.0,
    };
    println!("48 kHz stereo, {BLOCK}-frame blocks (1.333 ms deadline), 10 s per effect");
    println!("effect | delay frames | delay ms | impulse onset | mean us/block | p99 us/block");
    for (name, mut processor) in processors() {
        processor.prepare(spec).unwrap();
        let mut input = [0.0; BLOCK];
        let mut left = [0.0; BLOCK];
        let mut right = [0.0; BLOCK];
        let mut onset = None;
        input[0] = 0.5;
        for block in 0..375 {
            processor
                .process(
                    &[&input, &input],
                    &mut [&mut left, &mut right],
                    &[],
                    ProcessContext::default(),
                )
                .unwrap();
            if onset.is_none() {
                onset = left
                    .iter()
                    .zip(&right)
                    .position(|(l, r)| l.abs().max(r.abs()) > 1e-6)
                    .map(|frame| block * BLOCK + frame);
            }
            input.fill(0.0);
        }
        processor.reset();
        let signal: Vec<f32> = (0..BLOCKS * BLOCK)
            .map(|frame| {
                #[allow(clippy::cast_precision_loss)]
                let phase = frame as f32 * std::f32::consts::TAU * 110.0 / RATE as f32;
                0.3 * phase.sin() + 0.1 * (phase * 2.37).sin()
            })
            .collect();
        let mut durations = Vec::with_capacity(BLOCKS);
        for (block, source) in signal.chunks_exact(BLOCK).enumerate() {
            let start = Instant::now();
            processor
                .process(
                    black_box(&[source, source]),
                    &mut [&mut left, &mut right],
                    &[],
                    ProcessContext {
                        absolute_frame: (block * BLOCK) as u64,
                        tempo_bpm: 120.0,
                    },
                )
                .unwrap();
            black_box((&left, &right));
            durations.push(start.elapsed().as_secs_f64() * 1e6);
        }
        durations.sort_by(f64::total_cmp);
        #[allow(clippy::cast_precision_loss)]
        let mean = durations.iter().sum::<f64>() / BLOCKS as f64;
        println!(
            "{name} | {} | {:.3} | {} | {mean:.2} | {:.2}",
            processor.latency_frames(),
            f64::from(processor.latency_frames()) * 1000.0 / f64::from(RATE),
            onset.map_or_else(|| "none".into(), |frame| frame.to_string()),
            durations[BLOCKS * 99 / 100]
        );
    }
}
