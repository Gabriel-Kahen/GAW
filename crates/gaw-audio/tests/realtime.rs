use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::Arc,
};

use gaw_audio::{
    ChannelLayout, RealtimeCommand, RealtimeEngine, RealtimeEngineConfig, RealtimeRender,
    RenderSnapshot, SampleBlock,
};

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
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
        .try_send(RealtimeCommand::InstallSnapshot(snapshot(1)))
        .unwrap();
    sender.try_send(RealtimeCommand::Play).unwrap();
    let mut output = [0.0; 128];
    engine.process(&mut output);

    sender
        .try_send(RealtimeCommand::InstallSnapshot(snapshot(2)))
        .unwrap();
    engine.process(&mut output);
    for revision in 3..=12 {
        sender
            .try_send(RealtimeCommand::InstallSnapshot(snapshot(revision)))
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
