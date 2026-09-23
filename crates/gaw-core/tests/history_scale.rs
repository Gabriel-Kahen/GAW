use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

use gaw_core::{
    AutomationCurve, AutomationLane, AutomationLaneId, AutomationPoint, AutomationTarget,
    AutomationValue, Beats, Bpm, Clip, Command, Composition, CompositionClip, Decibels,
    DomainError, EditHistory, Event, EventData, EventDataId, GainParameters, MidiNote,
    MidiVelocity, NoteEvent, Processor, ProcessorId, ProcessorKind, Project, SampleRate, Track,
    TrackId, Transaction, Validate,
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

#[test]
#[ignore = "manual dense automation validation performance measurement"]
fn benchmark_dense_automation_validation() {
    use std::hint::black_box;

    let mut project = project_with_payload(100_000);
    project.event_data.clear();
    let mut durations = Vec::new();
    for _ in 0..7 {
        let started = Instant::now();
        for _ in 0..100 {
            black_box(&project).validate().unwrap();
        }
        durations.push(started.elapsed() / 100);
    }
    durations.sort_unstable();
    eprintln!(
        "100,000-point automation validation: {:?} median",
        durations[3]
    );
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

fn project_with_clips() -> Project {
    let mut project = project_with_payload(8);
    let child = Composition::new("Child", beats(1.0));
    let mut track = Track::audio(project.root_composition_id, "Many clips");
    track.clips = (0..2_048)
        .map(|index| {
            Clip::Composition(CompositionClip::new(
                child.id,
                beats(f64::from(index)),
                beats(1.0),
            ))
        })
        .collect();
    project.compositions[0].track_ids.push(track.id);
    project.compositions.push(child);
    project.tracks.push(track);
    project
}

#[test]
#[ignore = "manual dependency validation performance measurement"]
fn benchmark_clip_dependency_validation() {
    use std::hint::black_box;

    let project = project_with_clips();
    let (result, allocated_bytes) = allocated_bytes_during(|| project.validate());
    result.unwrap();
    let mut durations = Vec::new();
    for _ in 0..7 {
        let start = Instant::now();
        for _ in 0..200 {
            black_box(&project).validate().unwrap();
        }
        durations.push(start.elapsed() / 200);
    }
    durations.sort_unstable();
    eprintln!(
        "2,048-clip validation: {:?} median, {allocated_bytes} allocated bytes",
        durations[durations.len() / 2]
    );
}

#[test]
fn repeated_clip_dependencies_have_bounded_validation_allocations() {
    let project = project_with_clips();
    let (result, allocated_bytes) = allocated_bytes_during(|| project.validate());
    result.unwrap();
    assert!(
        allocated_bytes < 320 * 1024,
        "2,048-clip validation allocated {allocated_bytes} bytes"
    );
}

#[test]
fn track_volume_history_does_not_clone_clip_payloads() {
    let mut project = project_with_clips();
    let before = project.clone();
    let clip_pointer = project.tracks[0].clips.as_ptr();
    let track_id = project.tracks[0].id;
    let (result, validation_bytes) = allocated_bytes_during(|| project.validate());
    result.unwrap();
    let mut history = EditHistory::default();
    let transaction = Transaction::new([Command::SetTrackVolume {
        track_id,
        volume_db: -6.0,
    }]);
    let (result, apply_bytes) =
        allocated_bytes_during(|| history.apply(&mut project, &transaction));
    result.unwrap();
    eprintln!("track volume allocation: validate={validation_bytes}, apply={apply_bytes}");
    assert!(
        apply_bytes <= validation_bytes + 16 * 1024,
        "volume history cloned track payload: validate={validation_bytes}, apply={apply_bytes}"
    );
    let after = project.clone();
    history.undo(&mut project).unwrap();
    assert_eq!(project, before);
    assert_eq!(project.tracks[0].clips.as_ptr(), clip_pointer);
    history.redo(&mut project).unwrap();
    assert_eq!(project, after);
    assert_eq!(project.tracks[0].clips.as_ptr(), clip_pointer);

    let failed = Transaction::new([Command::SetTrackVolume {
        track_id,
        volume_db: 25.0,
    }]);
    let (error, failed_bytes) = allocated_bytes_during(|| history.apply(&mut project, &failed));
    assert!(matches!(
        error,
        Err(DomainError::Invalid {
            field: "track.volume_db",
            ..
        })
    ));
    assert!(failed_bytes <= validation_bytes + 16 * 1024);
    assert_eq!(project, after);
    assert_eq!(project.tracks[0].clips.as_ptr(), clip_pointer);
    assert_eq!(history.undo_len(), 1);
    assert_eq!(history.redo_len(), 0);
}

#[test]
fn volume_history_restores_tracks_through_replacement_and_removal() {
    let mut project = project_with_clips();
    let before = project.clone();
    let track_id = project.tracks[0].id;
    let mut replacement = project.tracks[0].clone();
    replacement.name = "Replacement".into();
    replacement.volume_db = -12.0;
    let mut history = EditHistory::default();
    history
        .apply(
            &mut project,
            &Transaction::new([
                Command::SetTrackVolume {
                    track_id,
                    volume_db: -6.0,
                },
                Command::UpdateTrack {
                    track: replacement.clone(),
                },
                Command::SetTrackVolume {
                    track_id,
                    volume_db: -3.0,
                },
                Command::RemoveTrack { track_id },
                Command::AddTrack {
                    track: replacement,
                    index: 0,
                },
                Command::SetTrackVolume {
                    track_id,
                    volume_db: -1.0,
                },
            ]),
        )
        .unwrap();
    let after = project.clone();
    for _ in 0..2 {
        history.undo(&mut project).unwrap();
        assert_eq!(project, before);
        history.redo(&mut project).unwrap();
        assert_eq!(project, after);
    }
}
