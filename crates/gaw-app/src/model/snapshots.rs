//! Immutable worker snapshots must follow every accepted canonical transition.

use super::*;

fn assert_current(vm: &ProjectViewModel, previous: &Arc<Project>) -> Arc<Project> {
    let current = vm.project_snapshot();
    assert_eq!(current.as_ref(), vm.project());
    assert!(!Arc::ptr_eq(&current, previous));
    assert!(Arc::ptr_eq(&current, &vm.project_snapshot()));
    current
}

#[test]
fn worker_snapshots_share_data_until_an_edit_and_survive_undo_redo() {
    let mut vm = ProjectViewModel::demo();
    let original = vm.project_snapshot();
    vm.apply(Intent::Select(Selection::Track { track: 0 }));
    vm.transport.playhead = 2.0;
    assert!(Arc::ptr_eq(&original, &vm.project_snapshot()));

    vm.apply(Intent::SetBpm(132.0));
    let edited = assert_current(&vm, &original);
    assert_ne!(original.bpm, edited.bpm);
    vm.apply(Intent::Undo(1.0));
    let undone = assert_current(&vm, &edited);
    assert_eq!(undone, original);
    vm.apply(Intent::Redo(2.0));
    let redone = assert_current(&vm, &undone);
    assert_eq!(redone, edited);
}

#[test]
fn worker_snapshots_follow_agent_edits_and_reload_but_ignore_failed_transactions() {
    let mut vm = ProjectViewModel::demo();
    let original = vm.project_snapshot();
    let invalid = Transaction::new([
        Command::SetProjectName {
            name: "Must roll back".into(),
        },
        Command::SetTrackVolume {
            track_id: TrackId::new(),
            volume_db: -6.0,
        },
    ]);
    assert!(vm.apply_agent_transaction(&invalid, [], 0.0).is_err());
    assert!(Arc::ptr_eq(&original, &vm.project_snapshot()));
    assert_eq!(original.as_ref(), vm.project());

    vm.apply_agent_transaction(
        &Transaction::new([Command::SetProjectName {
            name: "Renamed".into(),
        }]),
        [],
        0.0,
    )
    .unwrap();
    let renamed = assert_current(&vm, &original);
    assert_ne!(original.name, renamed.name);
    let mut external = vm.project().clone();
    external.name = "External".into();
    vm.replace_project_from_agent(external.clone(), [], 0.0)
        .unwrap();
    let reloaded = assert_current(&vm, &renamed);
    assert_eq!(reloaded.as_ref(), &external);
    external.compositions.clear();
    assert!(vm.replace_project_from_agent(external, [], 0.0).is_err());
    assert!(Arc::ptr_eq(&reloaded, &vm.project_snapshot()));
}

#[test]
fn worker_snapshots_follow_persisted_edits_even_when_revision_is_saturated() {
    let mut vm = ProjectViewModel::demo();
    vm.engine.revision = u64::MAX;
    let original = vm.project_snapshot();
    let transaction = Transaction::new([Command::SetProjectName {
        name: "Persisted".into(),
    }]);
    let mut expected = vm.project().clone();
    transaction.apply(&mut expected).unwrap();
    let selected = expected.assets[0].id;
    assert!(
        vm.accept_persisted_transaction(&transaction, &original, selected)
            .is_err()
    );
    assert!(Arc::ptr_eq(&original, &vm.project_snapshot()));
    vm.accept_persisted_transaction(&transaction, &expected, selected)
        .unwrap();
    let persisted = assert_current(&vm, &original);
    assert_eq!(persisted.as_ref(), &expected);
    assert_eq!(vm.revision(), u64::MAX);
}

#[test]
fn worker_snapshots_follow_persisted_stem_merges() {
    let mut vm = ProjectViewModel::demo();
    let original = vm.project_snapshot();
    let mut stem = original.assets[0].clone();
    stem.id = AssetId::new();
    let selected = stem.id;
    let mut expected = vm.project().clone();
    expected.assets.push(stem);
    let transaction = Transaction::named("Import stems", []);
    assert!(
        vm.accept_persisted_stem_split(&transaction, &expected, &[selected], selected)
            .is_err()
    );
    assert!(Arc::ptr_eq(&original, &vm.project_snapshot()));
    expected.asset_folders.push(AssetFolder {
        id: AssetFolderId::new(),
        name: "Stems".into(),
        asset_ids: vec![selected],
        event_data_ids: vec![],
    });
    vm.accept_persisted_stem_split(&transaction, &expected, &[selected], selected)
        .unwrap();
    let merged = assert_current(&vm, &original);
    assert_eq!(merged.as_ref(), &expected);
    assert_eq!(original.assets.len() + 1, merged.assets.len());
}

#[test]
#[ignore = "manual immutable project snapshot performance measurement"]
fn benchmark_shared_project_snapshots() {
    use std::{hint::black_box, time::Instant};

    let mut project = demo_project();
    project.event_data[0].events = (0..10_000)
        .map(|index| {
            gaw_core::Event::Note(
                gaw_core::NoteEvent::new(
                    gaw_core::Beats::new(f64::from(index) * 0.25).unwrap(),
                    gaw_core::Beats::new(0.25).unwrap(),
                    60,
                    100,
                )
                .unwrap(),
            )
        })
        .collect();
    let vm = ProjectViewModel::from_project(project).unwrap();
    let expected = vm.project_snapshot();
    for shared in [false, true] {
        let mut timings = Vec::new();
        for _ in 0..9 {
            let started = Instant::now();
            for _ in 0..1_000 {
                let snapshot = if shared {
                    vm.project_snapshot()
                } else {
                    Arc::new(black_box(vm.project()).clone())
                };
                black_box(snapshot);
            }
            timings.push(started.elapsed() / 1_000);
        }
        timings.sort_unstable();
        eprintln!(
            "project snapshot shared={shared}: {:?} per request",
            timings[4]
        );
    }
    assert_eq!(expected.as_ref(), vm.project());
}
