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
