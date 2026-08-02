use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use gaw_dsp::{
    AnalyzerTap, AudioLayout, BeatRepeat, Bitcrusher, Chorus, Clipper, Compressor, Delay, Expander,
    Filter, Flanger, Gain, Gate, Instrument, LevelMeter, Limiter, ParametricEq, Phaser, PitchShift,
    PrepareSpec, ProcessContext, Processor, Reverb, RhythmicGate, Sampler, SamplerConfig,
    Saturator, StereoTool, TransientShaper, TremoloAutopan,
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

#[test]
fn built_in_process_paths_do_not_allocate_after_prepare() {
    let mut processors: Vec<Box<dyn Processor>> = vec![
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
    ];
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
