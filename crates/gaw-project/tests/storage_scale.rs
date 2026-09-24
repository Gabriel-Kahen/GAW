//! Reproduce storage latency with `cargo test -p gaw-project --release --test
//! storage_scale -- --ignored --nocapture`.

use std::{hint::black_box, time::Instant};

use gaw_core::{
    Beats, Bpm, Command, Event, EventData, MidiNote, MidiVelocity, NoteEvent, Project, SampleRate,
    Transaction,
};
use gaw_project::ProjectStore;

#[test]
#[ignore = "manual storage performance measurement"]
fn dense_event_storage_latency() {
    let mut project = Project::new(
        "Storage scale",
        Bpm::new(120.0).unwrap(),
        SampleRate::new(48_000).unwrap(),
    );
    let mut events = EventData::new("Dense notes");
    events.events = (0..10_000)
        .map(|index| {
            Event::Note(NoteEvent {
                start: Beats::new(f64::from(index) / 4.0).unwrap(),
                duration: Beats::new(0.125).unwrap(),
                note: MidiNote::new(60).unwrap(),
                velocity: MidiVelocity::new(100).unwrap(),
                release_velocity: MidiVelocity::new(64).unwrap(),
                tuning: None,
            })
        })
        .collect();
    let event_id = events.id;
    project.event_data.push(events);
    let directory = tempfile::tempdir().unwrap();
    let store = ProjectStore::create(directory.path(), &project).unwrap();
    let bytes = std::fs::metadata(directory.path().join(format!("events/{event_id}.json")))
        .unwrap()
        .len();
    for iteration in 0..3 {
        let started = Instant::now();
        let events = black_box(store.load_event_data(event_id).unwrap());
        let event_elapsed = started.elapsed();
        assert_eq!(events, project.event_data[0]);

        let started = Instant::now();
        let loaded = black_box(store.load_project().unwrap());
        let project_elapsed = started.elapsed();
        assert_eq!(loaded, project);

        let name = format!("Storage scale {iteration}");
        let started = Instant::now();
        let committed = black_box(
            store
                .commit_transaction(&Transaction::new([Command::SetProjectName {
                    name: name.clone(),
                }]))
                .unwrap(),
        );
        let commit_elapsed = started.elapsed();
        project.name = name;
        assert_eq!(committed, project);
        println!(
            "{bytes} bytes: event={event_elapsed:?}, project={project_elapsed:?}, commit={commit_elapsed:?}"
        );
    }
}
