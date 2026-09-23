use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::Arc,
};

use gaw_audio::{
    ChannelLayout, RealtimeCommand, RealtimeEngine, RealtimeEngineConfig, RealtimeLoopRange,
    RealtimeRender, RenderSnapshot, SampleBlock, StreamGeneration, stream_notification_channel,
};

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    static DEALLOCATIONS: Cell<usize> = const { Cell::new(0) };
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
        COUNTING.with(|enabled| {
            if enabled.get() {
                DEALLOCATIONS.with(|count| count.set(count.get() + 1));
            }
        });
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

#[derive(Debug)]
struct Silence;

impl RealtimeRender for Silence {
    fn render(&self, _: u64, output: &mut SampleBlock<'_>) {
        output.clear();
    }
}

fn snapshot(revision: u64) -> Arc<RenderSnapshot> {
    Arc::new(
        RenderSnapshot::new(
            revision,
            48_000,
            ChannelLayout::Stereo,
            48_000,
            0,
            Arc::new(Silence),
        )
        .unwrap(),
    )
}

fn allocations_during(function: impl FnOnce()) -> usize {
    ALLOCATIONS.with(|count| count.set(0));
    COUNTING.with(|enabled| enabled.set(true));
    function();
    COUNTING.with(|enabled| enabled.set(false));
    ALLOCATIONS.with(Cell::get)
}

#[test]
fn callback_stays_allocation_free_when_retirement_and_command_queues_are_saturated() {
    let (sender, mut engine) = RealtimeEngine::new(
        RealtimeEngineConfig {
            maximum_block_frames: 64,
            maximum_commands_per_block: 8,
            ..RealtimeEngineConfig::default()
        },
        16,
        1,
    )
    .unwrap();
    sender
        .try_send(RealtimeCommand::ActivatePreview(snapshot(1)))
        .unwrap();
    sender.try_send(RealtimeCommand::Play).unwrap();
    let mut output = [0.0; 128];
    engine.process(&mut output);

    sender
        .try_send(RealtimeCommand::ActivatePreview(snapshot(2)))
        .unwrap();
    engine.process(&mut output);
    for revision in 3..=12 {
        sender
            .try_send(RealtimeCommand::ActivatePreview(snapshot(revision)))
            .unwrap();
    }

    let allocations = allocations_during(|| {
        for _ in 0..100 {
            engine.process(&mut output);
        }
    });
    assert_eq!(allocations, 0);
    assert_eq!(engine.snapshot_revision(), Some(2));
}

#[test]
fn saturated_stream_error_notification_is_allocation_free_and_bounded() {
    let (sender, receiver) = stream_notification_channel(1).unwrap();
    let generation = StreamGeneration::new(9);
    sender
        .try_send(generation, cpal::StreamError::BufferUnderrun)
        .unwrap();
    let mut callback = sender.callback(generation);
    let allocations = allocations_during(|| {
        callback(cpal::StreamError::DeviceNotAvailable);
    });
    assert_eq!(allocations, 0);
    let fatal = receiver.try_recv().unwrap();
    assert_eq!(fatal.generation.value(), 9);
    assert_eq!(fatal.error, cpal::StreamError::DeviceNotAvailable);
    assert_eq!(
        receiver.try_recv().unwrap().error,
        cpal::StreamError::BufferUnderrun
    );
}

#[test]
fn callback_loop_wrap_is_allocation_free() {
    let (sender, mut engine) = RealtimeEngine::new(
        RealtimeEngineConfig {
            output_layout: ChannelLayout::Stereo,
            maximum_block_frames: 64,
            ..RealtimeEngineConfig::default()
        },
        8,
        2,
    )
    .unwrap();
    sender
        .try_send(RealtimeCommand::ActivatePreview(snapshot(1)))
        .unwrap();
    sender
        .try_send(RealtimeCommand::SetLoop(Some(
            RealtimeLoopRange::new(1, 3).unwrap(),
        )))
        .unwrap();
    sender.try_send(RealtimeCommand::Play).unwrap();
    let mut output = [0.0; 128];
    let allocations = allocations_during(|| {
        for _ in 0..20 {
            assert_eq!(
                engine.process(&mut output),
                gaw_audio::ProcessStatus::Rendered
            );
        }
    });
    assert_eq!(allocations, 0);
}

#[test]
fn callback_transport_transitions_are_allocation_free() {
    let (sender, mut engine) = RealtimeEngine::new(RealtimeEngineConfig::default(), 8, 2).unwrap();
    sender
        .try_send(RealtimeCommand::ActivatePreview(snapshot(1)))
        .unwrap();
    let mut output = [0.0; 128];
    let allocations = allocations_during(|| {
        for _ in 0..10 {
            for command in [
                RealtimeCommand::Play,
                RealtimeCommand::Seek(100),
                RealtimeCommand::Pause,
                RealtimeCommand::Play,
                RealtimeCommand::Stop,
            ] {
                sender.try_send(command).unwrap();
                engine.process(&mut output);
            }
        }
    });
    assert_eq!(allocations, 0);
}

#[test]
fn live_effect_processing_replacement_and_reset_never_allocate_or_drop_dsp_in_callback() {
    let control = gaw_audio::InputMonitorControl::new();
    control.set_enabled(true);
    let (_, mut engine) = RealtimeEngine::new(RealtimeEngineConfig::default(), 8, 8).unwrap();
    engine.set_input_monitor(control.clone());
    let mut output = [0.0; 2048];
    for mut kind in gaw_core::processors::ProcessorKind::catalog_defaults()
        .into_iter()
        .filter(|kind| !kind.is_analyzer())
        .chain([gaw_core::ProcessorKind::PitchShift(
            gaw_core::PitchShiftParameters {
                quality: gaw_core::PitchQuality::Signalsmith,
                semitones: 12,
                ..Default::default()
            },
        )])
    {
        if let gaw_core::ProcessorKind::PitchShift(parameters) = &mut kind {
            // Neutral live pitch snapshots are optimized away; exercise both engines.
            parameters.semitones = 12;
        }
        let definition = gaw_core::Processor::new(
            gaw_core::ProcessorId::new("live-allocation-test").unwrap(),
            kind,
        );
        control
            .configure_effects(&[definition], false, 48_000, 120.0)
            .unwrap();
        DEALLOCATIONS.with(|count| count.set(0));
        let allocations = allocations_during(|| {
            engine.process(&mut output);
            control.set_enabled(false);
            control.set_enabled(true);
            engine.process(&mut output);
        });
        assert_eq!(allocations, 0);
        assert_eq!(DEALLOCATIONS.with(Cell::get), 0);
        assert!(output.iter().all(|sample| sample.is_finite()));
        control.collect_retired_effects();
    }
}

fn live_sampler(
    config: RealtimeEngineConfig,
    one_shot: bool,
) -> Box<gaw_audio::PreparedLiveSampler> {
    live_sampler_with_source(config, one_shot, vec![1.0; 8192])
}

fn live_sampler_with_source(
    config: RealtimeEngineConfig,
    one_shot: bool,
    source: Vec<f32>,
) -> Box<gaw_audio::PreparedLiveSampler> {
    let zone = gaw_dsp::SamplerZone {
        id: "zone".into(),
        asset_id: "sample".into(),
        source_start_frame: 0,
        source_end_frame: None,
        root_note: 60,
        low_note: 0,
        high_note: 127,
        low_velocity: 0,
        high_velocity: 127,
        playback_mode: if one_shot {
            gaw_dsp::PlaybackMode::OneShot
        } else {
            gaw_dsp::PlaybackMode::NoteGated
        },
        gain_db: 0.0,
        velocity_sensitivity: 1.0,
        attack_ms: 0.0,
        release_ms: 0.0,
        reverse: false,
        choke_group: None,
    };
    let sampler = gaw_dsp::Sampler::new(
        gaw_dsp::SamplerConfig {
            polyphony: 8,
            zones: vec![zone],
        },
        vec![gaw_dsp::SampleAsset {
            id: "sample".into(),
            sample_rate: f64::from(config.sample_rate),
            channels: vec![source],
        }],
    )
    .unwrap();
    Box::new(gaw_audio::PreparedLiveSampler::new(sampler, config, 120.0, 0.5).unwrap())
}

fn measured_sine_frequency(samples: &[f32], sample_rate: f64) -> f64 {
    let crossings: Vec<_> = samples
        .windows(2)
        .enumerate()
        .filter(|(_, pair)| pair[0] <= 0.0 && pair[1] > 0.0)
        .map(|(frame, pair)| {
            f64::from(u32::try_from(frame).unwrap())
                - f64::from(pair[0]) / f64::from(pair[1] - pair[0])
        })
        .collect();
    assert!(
        crossings.len() > 20,
        "The pitched sample must remain audible."
    );
    sample_rate * f64::from(u32::try_from(crossings.len() - 1).unwrap())
        / (crossings.last().unwrap() - crossings[0])
}

#[test]
fn live_keyboard_changes_pitch_without_changing_sample_duration() {
    const SOURCE_FRAMES: usize = 32_768;
    let config = RealtimeEngineConfig {
        maximum_block_frames: 256,
        ..Default::default()
    };
    let source_frequency = 261.625_565_300_598_6;
    let sample_rate = f64::from(config.sample_rate);
    for (note, cents) in [(48, 0.0), (60, 0.0), (72, 0.0), (62, -200.0 / 7.0)] {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "The bounded sine signal is intentionally stored as f32 audio."
        )]
        let source = (0..u32::try_from(SOURCE_FRAMES).unwrap())
            .map(|frame| {
                (std::f64::consts::TAU * source_frequency * f64::from(frame) / sample_rate).sin()
                    as f32
            })
            .collect();
        let (sender, mut engine) = RealtimeEngine::new(config, 16, 2).unwrap();
        sender
            .try_send(RealtimeCommand::InstallLiveSampler(Some(
                live_sampler_with_source(config, true, source),
            )))
            .unwrap();
        let command = if cents == 0.0 {
            RealtimeCommand::LiveNoteOn {
                note,
                velocity: 1.0,
            }
        } else {
            RealtimeCommand::LiveNoteOnTuned {
                note,
                velocity: 1.0,
                cents,
            }
        };
        sender.try_send(command).unwrap();
        let mut rendered = Vec::new();
        let mut block = [0.0; 512];
        for _ in 0..(SOURCE_FRAMES + 4096) / 256 {
            engine.process(&mut block);
            rendered.extend(block.chunks_exact(2).map(|frame| frame[0]));
        }
        // Pitch processors can change phase. Measure the steady-state waveform's
        // frequency instead of comparing samples against a phase-locked sine.
        let expected =
            source_frequency * 2.0_f64.powf((f64::from(note) - 60.0 + cents / 100.0) / 12.0);
        let actual = measured_sine_frequency(&rendered[8192..24_576], sample_rate);
        assert!(
            (actual / expected - 1.0).abs() < 0.003,
            "note {note}, cents {cents}: expected {expected} Hz, got {actual} Hz"
        );
        let ending_energy = rendered[SOURCE_FRAMES - 4096..SOURCE_FRAMES]
            .iter()
            .map(|sample| f64::from(*sample).powi(2))
            .sum::<f64>()
            / 4096.0;
        assert!(
            ending_energy > 0.01,
            "note {note}, cents {cents}: sample ended before the crop's duration"
        );
        assert!(
            rendered[SOURCE_FRAMES..]
                .iter()
                .all(|sample| *sample == 0.0),
            "note {note}, cents {cents}: sample exceeded the crop's duration"
        );
    }
}

#[test]
fn live_keyboard_is_polyphonic_while_stopped_and_respects_velocity_gain_and_note_off() {
    let config = RealtimeEngineConfig {
        maximum_block_frames: 64,
        ..Default::default()
    };
    let (sender, mut engine) = RealtimeEngine::new(config, 16, 2).unwrap();
    sender
        .try_send(RealtimeCommand::InstallLiveSampler(Some(live_sampler(
            config, false,
        ))))
        .unwrap();
    let (reference_sender, mut reference_engine) = RealtimeEngine::new(config, 16, 2).unwrap();
    reference_sender
        .try_send(RealtimeCommand::InstallLiveSampler(Some(live_sampler(
            config, false,
        ))))
        .unwrap();
    reference_sender
        .try_send(RealtimeCommand::LiveNoteOn {
            note: 64,
            velocity: 1.0,
        })
        .unwrap();
    sender.try_send(RealtimeCommand::SetGain(0.5)).unwrap();
    sender
        .try_send(RealtimeCommand::LiveNoteOn {
            note: 60,
            velocity: 1.0,
        })
        .unwrap();
    sender
        .try_send(RealtimeCommand::LiveNoteOn {
            note: 64,
            velocity: 0.5,
        })
        .unwrap();
    let mut output = [0.0; 128];
    assert_eq!(
        engine.process(&mut output),
        gaw_audio::ProcessStatus::Rendered
    );
    let mut reference_output = [0.0; 128];
    reference_engine.process(&mut reference_output);
    // The root note contributes exactly 0.25. Compare the transposed voice
    // against the same DSP at full velocity/gain, independently of its phase.
    assert!(
        output
            .iter()
            .zip(&reference_output)
            .all(|(actual, reference)| { (*actual - (0.25 + 0.25 * reference)).abs() < 0.000_01 })
    );
    assert_eq!(engine.transport().frame, 0);
    assert!(!engine.transport().playing);
    sender
        .try_send(RealtimeCommand::LiveNoteOff { note: 60 })
        .unwrap();
    engine.process(&mut output);
    reference_engine.process(&mut reference_output);
    assert!(
        output
            .iter()
            .zip(&reference_output)
            .all(|(actual, reference)| { (*actual - 0.25 * reference).abs() < 0.000_01 })
    );
    assert!(output.iter().any(|sample| sample.abs() > 0.01));
    sender
        .try_send(RealtimeCommand::LiveNoteOff { note: 64 })
        .unwrap();
    assert_eq!(
        engine.process(&mut output),
        gaw_audio::ProcessStatus::Silence
    );
    assert!(output.iter().all(|sample| *sample == 0.0));
}

#[test]
fn live_keyboard_panic_stops_one_shots_and_ignores_invalid_input() {
    let config = RealtimeEngineConfig {
        maximum_block_frames: 64,
        ..Default::default()
    };
    let (sender, mut engine) = RealtimeEngine::new(config, 16, 2).unwrap();
    sender
        .try_send(RealtimeCommand::InstallLiveSampler(Some(live_sampler(
            config, true,
        ))))
        .unwrap();
    sender
        .try_send(RealtimeCommand::LiveNoteOn {
            note: 60,
            velocity: 1.0,
        })
        .unwrap();
    let mut output = [0.0; 128];
    engine.process(&mut output);
    sender
        .try_send(RealtimeCommand::LiveNoteOff { note: 60 })
        .unwrap();
    engine.process(&mut output);
    assert!(output.iter().any(|sample| *sample > 0.0));
    sender.try_send(RealtimeCommand::LiveAllNotesOff).unwrap();
    sender
        .try_send(RealtimeCommand::LiveNoteOn {
            note: 128,
            velocity: 1.0,
        })
        .unwrap();
    sender
        .try_send(RealtimeCommand::LiveNoteOn {
            note: 60,
            velocity: f32::NAN,
        })
        .unwrap();
    engine.process(&mut output);
    assert!(output.iter().all(|sample| *sample == 0.0));
}

#[test]
fn live_keyboard_processing_and_saturated_replacement_neither_allocate_nor_free() {
    let config = RealtimeEngineConfig {
        maximum_block_frames: 64,
        ..Default::default()
    };
    let (sender, mut engine) = RealtimeEngine::new(config, 16, 1).unwrap();
    for _ in 0..3 {
        sender
            .try_send(RealtimeCommand::InstallLiveSampler(Some(live_sampler(
                config, false,
            ))))
            .unwrap();
        sender
            .try_send(RealtimeCommand::LiveNoteOnTuned {
                note: 60,
                velocity: 1.0,
                cents: -200.0 / 7.0,
            })
            .unwrap();
    }
    let mut output = [0.0; 128];
    DEALLOCATIONS.with(|count| count.set(0));
    let allocations = allocations_during(|| {
        for _ in 0..20 {
            engine.process(&mut output);
        }
    });
    assert_eq!(allocations, 0);
    assert_eq!(DEALLOCATIONS.with(Cell::get), 0);
    assert_eq!(sender.reclaim_retired(), 1);
    engine.process(&mut output);
    assert!(output.iter().any(|sample| *sample > 0.0));
    sender
        .try_send(RealtimeCommand::InstallLiveSampler(None))
        .unwrap();
    sender.reclaim_retired();
    engine.process(&mut output);
    assert!(output.iter().all(|sample| *sample == 0.0));
}
