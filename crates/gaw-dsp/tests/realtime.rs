use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use gaw_dsp::{
    AnalyzerTap, AudioLayout, BeatRepeat, Bitcrusher, Chorus, Clipper, Compressor, Delay,
    EnergyMeter, Expander, Filter, Flanger, Gain, Gate, Instrument, LevelMeter, Limiter, NoteEvent,
    Oscilloscope, ParameterEvent, ParameterKind, ParameterValue, ParametricEq, Phaser, PitchShift,
    PlaybackMode, PrepareSpec, ProcessContext, Processor, Reverb, RhythmicGate, SampleAsset,
    Sampler, SamplerConfig, SamplerZone, Saturator, SpectrumAnalyzer, StereoMeter, StereoTool,
    TransientShaper, TremoloAutopan, Tuner,
};

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

#[test]
fn sampler_process_path_does_not_allocate_after_prepare() {
    let mut sampler = Sampler::new(SamplerConfig::default(), Vec::new()).unwrap();
    let spec = PrepareSpec {
        max_block_size: 128,
        ..PrepareSpec::default()
    };
    Instrument::prepare(&mut sampler, spec).unwrap();
    let mut left = [0.0; 128];
    let mut right = [0.0; 128];
    let allocations = allocations_during(|| {
        sampler
            .process(&mut [&mut left, &mut right], &[], ProcessContext::default())
            .unwrap();
    });
    assert_eq!(allocations, 0);
}

#[test]
fn active_mono_sampler_with_events_does_not_allocate_after_prepare() {
    let config = SamplerConfig {
        polyphony: 4,
        zones: vec![SamplerZone {
            id: "active-zone".into(),
            asset_id: "sample".into(),
            source_start_frame: 1,
            source_end_frame: Some(96),
            root_note: 60,
            low_note: 48,
            high_note: 72,
            low_velocity: 1,
            high_velocity: 127,
            playback_mode: PlaybackMode::NoteGated,
            gain_db: -3.0,
            velocity_sensitivity: 0.75,
            attack_ms: 2.0,
            release_ms: 10.0,
            reverse: true,
            choke_group: Some(1),
        }],
    };
    let mut sampler = Sampler::new(
        config,
        vec![SampleAsset {
            id: "sample".into(),
            sample_rate: 48_000.0,
            channels: vec![vec![0.25; 96], vec![-0.125; 96]],
        }],
    )
    .unwrap();
    Instrument::prepare(
        &mut sampler,
        PrepareSpec {
            sample_rate: 48_000.0,
            max_block_size: 128,
            input_layout: AudioLayout::Mono,
            tempo_bpm: 93.0,
        },
    )
    .unwrap();
    let events = [
        NoteEvent::NoteOn {
            sample_offset: 0,
            note: 60,
            velocity: 0.8,
        },
        NoteEvent::NoteOff {
            sample_offset: 64,
            note: 60,
        },
    ];
    let mut output = [0.0; 128];
    let allocations = allocations_during(|| {
        sampler
            .process(
                &mut [&mut output],
                &events,
                ProcessContext {
                    absolute_frame: 12_345,
                    tempo_bpm: 93.0,
                },
            )
            .unwrap();
    });
    assert_eq!(allocations, 0);
    assert!(output.iter().any(|sample| *sample != 0.0));
}

#[test]
fn malformed_sampler_events_are_rejected_without_allocating() {
    let mut sampler = Sampler::new(SamplerConfig::default(), Vec::new()).unwrap();
    Instrument::prepare(
        &mut sampler,
        PrepareSpec {
            max_block_size: 8,
            input_layout: AudioLayout::Mono,
            ..PrepareSpec::default()
        },
    )
    .unwrap();
    let events = [NoteEvent::NoteOn {
        sample_offset: 0,
        note: 60,
        velocity: f32::NAN,
    }];
    let mut output = [0.0; 8];
    let allocations = allocations_during(|| {
        assert!(
            sampler
                .process(&mut [&mut output], &events, ProcessContext::default())
                .is_err()
        );
    });
    assert_eq!(allocations, 0);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNTING.with(|enabled| {
            if enabled.get() {
                ALLOCATIONS.with(|count| count.set(count.get() + 1));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        COUNTING.with(|enabled| {
            if enabled.get() {
                ALLOCATIONS.with(|count| count.set(count.get() + 1));
            }
        });
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn allocations_during(function: impl FnOnce()) -> usize {
    ALLOCATIONS.with(|count| count.set(0));
    COUNTING.with(|enabled| enabled.set(true));
    function();
    COUNTING.with(|enabled| enabled.set(false));
    ALLOCATIONS.with(Cell::get)
}

fn built_ins() -> Vec<Box<dyn Processor>> {
    vec![
        Box::new(Gain::default()),
        Box::new(StereoTool::default()),
        Box::new(Filter::default()),
        Box::new(ParametricEq::default()),
        Box::new(Compressor::default()),
        Box::new(Limiter::default()),
        Box::new(Gate::default()),
        Box::new(Expander::default()),
        Box::new(TransientShaper::default()),
        Box::new(Saturator::default()),
        Box::new(Clipper::default()),
        Box::new(Bitcrusher::default()),
        Box::new(Delay::default()),
        Box::new(Reverb::default()),
        Box::new(Chorus::default()),
        Box::new(Flanger::default()),
        Box::new(Phaser::default()),
        Box::new(TremoloAutopan::default()),
        Box::new(PitchShift::default()),
        Box::new(RhythmicGate::default()),
        Box::new(BeatRepeat::default()),
        Box::new(AnalyzerTap::<LevelMeter>::level_meter()),
        Box::new(AnalyzerTap::<EnergyMeter>::energy_meter()),
        Box::new(AnalyzerTap::<SpectrumAnalyzer>::spectrum()),
        Box::new(AnalyzerTap::<Oscilloscope>::oscilloscope()),
        Box::new(AnalyzerTap::<StereoMeter>::stereo_meter()),
        Box::new(AnalyzerTap::<Tuner>::tuner()),
    ]
}

fn non_default_automation_event(processor: &dyn Processor) -> Option<ParameterEvent> {
    processor
        .parameters()
        .iter()
        .find(|descriptor| descriptor.automatable)
        .map(|descriptor| {
            let value = match descriptor.kind {
                ParameterKind::Float { min, max } => {
                    let default = descriptor.default.as_float().unwrap();
                    ParameterValue::Float(if default.to_bits() == min.to_bits() {
                        max
                    } else {
                        min
                    })
                }
                ParameterKind::Integer { min, max } => {
                    let default = descriptor.default.as_integer().unwrap();
                    ParameterValue::Integer(if default == min { max } else { min })
                }
                ParameterKind::UnsignedInteger { min, max } => {
                    let default = descriptor.default.as_unsigned_integer().unwrap();
                    ParameterValue::UnsignedInteger(if default == min { max } else { min })
                }
                ParameterKind::Boolean => {
                    ParameterValue::Bool(!descriptor.default.as_bool().unwrap())
                }
                ParameterKind::Choice(choices) => ParameterValue::Choice(
                    (descriptor.default.as_choice().unwrap() + 1)
                        % u32::try_from(choices.len()).unwrap(),
                ),
                ParameterKind::Time {
                    seconds_min,
                    seconds_max,
                    beats_min,
                    beats_max,
                } => match descriptor.default {
                    ParameterValue::Seconds(default) => {
                        ParameterValue::Seconds(if default.to_bits() == seconds_min.to_bits() {
                            seconds_max
                        } else {
                            seconds_min
                        })
                    }
                    ParameterValue::Beats(default) => {
                        ParameterValue::Beats(if default.to_bits() == beats_min.to_bits() {
                            beats_max
                        } else {
                            beats_min
                        })
                    }
                    _ => unreachable!("time descriptor must have a time default"),
                },
                ParameterKind::Rate {
                    hertz_min,
                    hertz_max,
                    beats_min,
                    beats_max,
                } => match descriptor.default {
                    ParameterValue::Hertz(default) => {
                        ParameterValue::Hertz(if default.to_bits() == hertz_min.to_bits() {
                            hertz_max
                        } else {
                            hertz_min
                        })
                    }
                    ParameterValue::Beats(default) => {
                        ParameterValue::Beats(if default.to_bits() == beats_min.to_bits() {
                            beats_max
                        } else {
                            beats_min
                        })
                    }
                    _ => unreachable!("rate descriptor must have a rate default"),
                },
            };
            ParameterEvent::new(31, descriptor.id, value)
        })
}

#[test]
fn built_in_process_paths_do_not_allocate_after_prepare() {
    let mut processors = built_ins();
    let spec = PrepareSpec {
        sample_rate: 48_000.0,
        max_block_size: 128,
        input_layout: AudioLayout::Stereo,
        tempo_bpm: 120.0,
    };
    let input_left = [0.1; 128];
    let input_right = [-0.1; 128];

    for processor in &mut processors {
        processor.prepare(spec).unwrap();
        let mut output_left = [0.0; 128];
        let mut output_right = [0.0; 128];
        let allocations = allocations_during(|| {
            processor
                .process(
                    &[&input_left, &input_right],
                    &mut [&mut output_left, &mut output_right],
                    &[],
                    ProcessContext::default(),
                )
                .unwrap();
        });
        assert_eq!(
            allocations,
            0,
            "{} allocated in process",
            processor.type_id()
        );
    }
}

#[test]
fn built_in_parameter_descriptors_have_unique_ids_and_valid_defaults() {
    for processor in built_ins() {
        let descriptors = processor.parameters();
        for (index, descriptor) in descriptors.iter().enumerate() {
            assert!(
                descriptor.accepts(descriptor.default),
                "{} has an invalid default for {}",
                processor.type_id(),
                descriptor.id
            );
            assert!(
                descriptors[..index]
                    .iter()
                    .all(|prior| prior.id != descriptor.id),
                "{} repeats parameter ID {}",
                processor.type_id(),
                descriptor.id
            );
        }
    }
}

#[test]
fn built_in_mono_automation_paths_do_not_allocate_after_prepare() {
    let mut processors = built_ins();
    let spec = PrepareSpec {
        sample_rate: 44_100.0,
        max_block_size: 127,
        input_layout: AudioLayout::Mono,
        tempo_bpm: 93.0,
    };
    let input = [0.1; 127];

    for processor in &mut processors {
        processor.prepare(spec).unwrap();
        let event = non_default_automation_event(processor.as_ref());
        let events = event.as_slice();
        let mut left = [0.0; 127];
        let mut right = [0.0; 127];
        let output_layout = processor.output_layout(AudioLayout::Mono).unwrap();
        let allocations = match output_layout {
            AudioLayout::Mono => allocations_during(|| {
                processor
                    .process(
                        &[&input],
                        &mut [&mut left],
                        events,
                        ProcessContext {
                            absolute_frame: 7_777,
                            tempo_bpm: 93.0,
                        },
                    )
                    .unwrap();
            }),
            AudioLayout::Stereo => allocations_during(|| {
                processor
                    .process(
                        &[&input],
                        &mut [&mut left, &mut right],
                        events,
                        ProcessContext {
                            absolute_frame: 7_777,
                            tempo_bpm: 93.0,
                        },
                    )
                    .unwrap();
            }),
        };
        assert_eq!(
            allocations,
            0,
            "{} allocated in mono automation process",
            processor.type_id()
        );
    }
}

#[test]
fn malformed_and_configuration_events_are_rejected_without_allocating() {
    let spec = PrepareSpec {
        max_block_size: 16,
        input_layout: AudioLayout::Mono,
        ..PrepareSpec::default()
    };
    let input = [0.0; 16];

    for processor in &mut built_ins() {
        processor.prepare(spec).unwrap();
        let output_layout = processor.output_layout(AudioLayout::Mono).unwrap();
        let unknown = ParameterEvent::new(0, "not_a_parameter", ParameterValue::Float(0.0));
        let configuration: Vec<_> = processor
            .parameters()
            .iter()
            .filter(|descriptor| !descriptor.automatable)
            .map(|descriptor| ParameterEvent::new(0, descriptor.id, descriptor.default))
            .collect();

        for event in std::iter::once(&unknown).chain(&configuration) {
            let mut left = [0.0; 16];
            let mut right = [0.0; 16];
            let mut rejected = false;
            let allocations = allocations_during(|| {
                let result = match output_layout {
                    AudioLayout::Mono => processor.process(
                        &[&input],
                        &mut [&mut left],
                        std::slice::from_ref(event),
                        ProcessContext::default(),
                    ),
                    AudioLayout::Stereo => processor.process(
                        &[&input],
                        &mut [&mut left, &mut right],
                        std::slice::from_ref(event),
                        ProcessContext::default(),
                    ),
                };
                rejected = result.is_err();
            });
            assert!(
                rejected,
                "{} accepted malformed/configuration event {}",
                processor.type_id(),
                event.id
            );
            assert_eq!(
                allocations,
                0,
                "{} allocated while rejecting {}",
                processor.type_id(),
                event.id
            );
        }
    }
}

#[test]
fn built_in_mono_automation_is_deterministic_after_seek() {
    let mut processors = built_ins();
    let spec = PrepareSpec {
        sample_rate: 44_100.0,
        max_block_size: 127,
        input_layout: AudioLayout::Mono,
        tempo_bpm: 93.0,
    };
    let mut input = [0.0; 127];
    for (frame, sample) in input.iter_mut().enumerate() {
        let frame = f32::from(u16::try_from(frame).unwrap());
        *sample = ((frame * 0.37).sin() * 0.5).clamp(-1.0, 1.0);
    }
    let context = ProcessContext {
        absolute_frame: 7_777,
        tempo_bpm: 93.0,
    };

    for processor in &mut processors {
        processor.prepare(spec).unwrap();
        let event = non_default_automation_event(processor.as_ref());
        let events = event.as_slice();
        let output_layout = processor.output_layout(AudioLayout::Mono).unwrap();
        // Establish the automated parameter value before comparing seeks. The host owns
        // the parameter snapshot at a seek boundary; seek restores DSP history from it.
        let mut priming_left = [0.0; 127];
        let mut priming_right = [0.0; 127];
        match output_layout {
            AudioLayout::Mono => processor
                .process(&[&input], &mut [&mut priming_left], events, context)
                .unwrap(),
            AudioLayout::Stereo => processor
                .process(
                    &[&input],
                    &mut [&mut priming_left, &mut priming_right],
                    events,
                    context,
                )
                .unwrap(),
        }
        let mut first_left = [0.0; 127];
        let mut first_right = [0.0; 127];
        processor.seek(context.absolute_frame);
        match output_layout {
            AudioLayout::Mono => processor
                .process(&[&input], &mut [&mut first_left], events, context)
                .unwrap(),
            AudioLayout::Stereo => processor
                .process(
                    &[&input],
                    &mut [&mut first_left, &mut first_right],
                    events,
                    context,
                )
                .unwrap(),
        }

        let mut second_left = [0.0; 127];
        let mut second_right = [0.0; 127];
        processor.seek(context.absolute_frame);
        match output_layout {
            AudioLayout::Mono => processor
                .process(&[&input], &mut [&mut second_left], events, context)
                .unwrap(),
            AudioLayout::Stereo => processor
                .process(
                    &[&input],
                    &mut [&mut second_left, &mut second_right],
                    events,
                    context,
                )
                .unwrap(),
        }

        assert!(
            first_left
                .iter()
                .zip(second_left)
                .all(|(first, second)| first.to_bits() == second.to_bits()),
            "{} left output changed after seek",
            processor.type_id()
        );
        assert!(
            first_right
                .iter()
                .zip(second_right)
                .all(|(first, second)| first.to_bits() == second.to_bits()),
            "{} right output changed after seek",
            processor.type_id()
        );
    }
}
