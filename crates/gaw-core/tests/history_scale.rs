use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

use gaw_core::{
    AutomationCurve, AutomationLane, AutomationLaneId, AutomationPoint, AutomationTarget,
    AutomationValue, Beats, Bpm, Command, Decibels, EditHistory, Event, EventData, EventDataId,
    GainParameters, MidiNote, MidiVelocity, NoteEvent, Processor, ProcessorId, ProcessorKind,
    Project, SampleRate, TrackId, Transaction, Validate,
};

struct CountingAllocator;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATED_BYTES: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNTING.with(|enabled| {
            if enabled.get() {
                ALLOCATED_BYTES.with(|bytes| bytes.set(bytes.get().saturating_add(layout.size())));
            }
        });
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        COUNTING.with(|enabled| {
            if enabled.get() {
                ALLOCATED_BYTES.with(|bytes| bytes.set(bytes.get().saturating_add(new_size)));
            }
        });
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn allocated_bytes_during<T>(function: impl FnOnce() -> T) -> (T, usize) {
    ALLOCATED_BYTES.with(|bytes| bytes.set(0));
    COUNTING.with(|enabled| enabled.set(true));
    let result = function();
    COUNTING.with(|enabled| enabled.set(false));
    let bytes = ALLOCATED_BYTES.with(Cell::get);
    (result, bytes)
}

fn beats(value: f64) -> Beats {
    Beats::new(value).unwrap()
}

#[allow(clippy::cast_precision_loss)]
fn project_with_payload(point_count: usize) -> Project {
    let mut project = Project::new(
        "Scale fixture",
        Bpm::new(120.0).unwrap(),
        SampleRate::new(48_000).unwrap(),
    );
    project.compositions[0].length = beats(200_000.0);

    project.event_data.push(EventData {
        id: EventDataId::new(),
        name: "Dense notes".into(),
        events: (0..point_count)
            .map(|index| {
                Event::Note(NoteEvent {
                    start: beats(index as f64),
                    duration: beats(0.5),
                    note: MidiNote::new(60).unwrap(),
                    velocity: MidiVelocity::new(100).unwrap(),
                    release_velocity: MidiVelocity::new(64).unwrap(),
                })
            })
            .collect(),
    });

    let processor = Processor::new(
        ProcessorId::new("scale_gain").unwrap(),
        ProcessorKind::Gain(GainParameters::default()),
    );
    let processor_id = processor.id.clone();
    project.compositions[0].output_effects.push(processor);
    project.automation.push(AutomationLane {
        id: AutomationLaneId::new(),
        composition_id: project.root_composition_id,
        name: "Dense gain automation".into(),
        target: AutomationTarget::CompositionOutputProcessor {
            processor_id,
            parameter_id: "gain_db".into(),
        },
        points: (0..point_count)
            .map(|index| AutomationPoint {
                time: beats(index as f64),
                value: AutomationValue::Decibels(Decibels::new(-6.0).unwrap()),
                curve: AutomationCurve::Linear,
            })
            .collect(),
    });
    project.validate().unwrap();
    project
}

#[derive(Debug)]
struct Measurements {
    apply_bytes: usize,
    undo_bytes: usize,
    redo_bytes: usize,
    failed_bytes: usize,
    edit_elapsed: Duration,
    validation_elapsed: Duration,
}

fn exercise(mut project: Project, name: &str) -> Measurements {
    let event_pointer = project.event_data[0].events.as_ptr();
    let automation_pointer = project.automation[0].points.as_ptr();

    let validation_started = Instant::now();
    for _ in 0..3 {
        project.validate().unwrap();
    }
    let validation_elapsed = validation_started.elapsed();

    let transaction = Transaction::new([Command::SetProjectName { name: name.into() }]);
    let mut history = EditHistory::default();
    let edit_started = Instant::now();

    let (result, apply_bytes) =
        allocated_bytes_during(|| history.apply(&mut project, &transaction));
    result.unwrap();
    assert_eq!(project.name, name);
    assert_eq!(project.event_data[0].events.as_ptr(), event_pointer);
    assert_eq!(project.automation[0].points.as_ptr(), automation_pointer);

    let (result, undo_bytes) = allocated_bytes_during(|| history.undo(&mut project));
    result.unwrap();
    assert_eq!(project.name, "Scale fixture");
    assert_eq!(project.event_data[0].events.as_ptr(), event_pointer);
    assert_eq!(project.automation[0].points.as_ptr(), automation_pointer);

    let (result, redo_bytes) = allocated_bytes_during(|| history.redo(&mut project));
    result.unwrap();
    assert_eq!(project.name, name);
    assert_eq!(project.event_data[0].events.as_ptr(), event_pointer);
    assert_eq!(project.automation[0].points.as_ptr(), automation_pointer);

    let failed = Transaction::new([
        Command::SetProjectName {
            name: "must roll back".into(),
        },
        Command::RemoveTrack {
            track_id: TrackId::new(),
        },
    ]);
    let (result, failed_bytes) = allocated_bytes_during(|| history.apply(&mut project, &failed));
    assert!(result.is_err());
    assert_eq!(project.name, name);
    assert_eq!(history.undo_len(), 1);
    assert_eq!(history.redo_len(), 0);
    assert_eq!(project.event_data[0].events.as_ptr(), event_pointer);
    assert_eq!(project.automation[0].points.as_ptr(), automation_pointer);

    Measurements {
        apply_bytes,
        undo_bytes,
        redo_bytes,
        failed_bytes,
        edit_elapsed: edit_started.elapsed(),
        validation_elapsed,
    }
}

#[test]
fn ordinary_history_edits_do_not_clone_large_event_or_automation_payloads() {
    let small = exercise(project_with_payload(8), "Small edit");
    let large = exercise(project_with_payload(100_000), "Large edit");
    let fixed_overhead = 256 * 1024;

    assert!(
        large.apply_bytes <= small.apply_bytes + fixed_overhead,
        "apply allocated with payload size: small={small:?}, large={large:?}"
    );
    assert!(
        large.undo_bytes <= small.undo_bytes + fixed_overhead,
        "undo allocated with payload size: small={small:?}, large={large:?}"
    );
    assert!(
        large.redo_bytes <= small.redo_bytes + fixed_overhead,
        "redo allocated with payload size: small={small:?}, large={large:?}"
    );
    assert!(
        large.failed_bytes <= small.failed_bytes + fixed_overhead,
        "failed rollback allocated with payload size: small={small:?}, large={large:?}"
    );

    let latency_budget = large.validation_elapsed * 20 + Duration::from_millis(500);
    assert!(
        large.edit_elapsed <= latency_budget,
        "large edit took {:?}; three validations took {:?}",
        large.edit_elapsed,
        large.validation_elapsed
    );
}
