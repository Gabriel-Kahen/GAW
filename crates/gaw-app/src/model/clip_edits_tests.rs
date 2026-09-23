//! Stable event deletion and folder-edit regressions, plus opt-in timing.

use super::*;
use gaw_core::{Beats, Event, NoteEvent};

fn event(index: usize) -> Event {
    Event::Note(
        NoteEvent::new(
            Beats::new(index as f64 * 0.125).unwrap(),
            Beats::new(0.125).unwrap(),
            60 + (index % 12) as u8,
            100,
        )
        .unwrap(),
    )
}

fn legacy_delete_event_indices(events: &mut Vec<Event>, deletions: BTreeSet<usize>) {
    for index in deletions.into_iter().rev() {
        events.remove(index);
    }
}

#[test]
fn compacted_note_deletions_match_original_order_and_keep_additions() {
    for count in 0..=8 {
        let mut original: Vec<_> = (0..count).map(event).collect();
        if count > 2 {
            original[2] = Event::Control(gaw_core::ControlEvent {
                time: Beats::new(0.25).unwrap(),
                controller: "sustain".into(),
                value: gaw_core::Ratio::new(0.5).unwrap(),
            });
        }
        for mask in 0..(1 << count) {
            let deletions: BTreeSet<_> = (0..count)
                .filter(|index| mask & (1 << index) != 0)
                .collect();
            let mut expected = original.clone();
            // Additions are appended before original indexes are removed.
            expected.extend((count..count + 3).map(event));
            let mut actual = expected.clone();
            legacy_delete_event_indices(&mut expected, deletions.clone());
            delete_event_indices(&mut actual, deletions);
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn mixed_note_edits_remain_undoable() {
    let mut vm = ProjectViewModel::demo();
    let before = vm.project.clone();
    let gaw_core::Clip::Event(clip) = &before.tracks[1].clips[0] else {
        panic!("event clip fixture");
    };
    let mut expected = before
        .event_data
        .iter()
        .find(|data| data.id == clip.event_data_id)
        .unwrap()
        .clone();
    let indices: Vec<_> = expected
        .events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| matches!(event, Event::Note(_)).then_some(index))
        .take(3)
        .collect();
    let Event::Note(original_note) = expected.events[indices[1]] else {
        unreachable!();
    };
    let mut updated = NoteEvent::new(
        Beats::new(clip.source_start.value() + 0.25).unwrap(),
        Beats::new(0.125).unwrap(),
        110,
        90,
    )
    .unwrap();
    updated.release_velocity = original_note.release_velocity;
    expected.events[indices[1]] = Event::Note(updated);
    expected.events.push(Event::Note(
        NoteEvent::new(
            Beats::new(clip.source_start.value() + 0.25).unwrap(),
            Beats::new(0.125).unwrap(),
            111,
            91,
        )
        .unwrap(),
    ));
    legacy_delete_event_indices(
        &mut expected.events,
        BTreeSet::from([indices[0], indices[2]]),
    );
    expected.sort();
    let revision = vm.revision();
    vm.edit_notes(
        1,
        0,
        [
            NoteEdit::Update {
                event_index: indices[0],
                start: 0.5,
                length: 0.25,
                pitch: 99,
                velocity: 100,
            },
            NoteEdit::Delete {
                event_index: indices[0],
            },
            NoteEdit::Update {
                event_index: indices[1],
                start: 0.25,
                length: 0.125,
                pitch: 110,
                velocity: 90,
            },
            NoteEdit::Add {
                start: 0.25,
                length: 0.125,
                pitch: 111,
                velocity: 91,
            },
            NoteEdit::Delete {
                event_index: indices[2],
            },
            NoteEdit::Delete {
                event_index: indices[0],
            },
        ],
    );
    assert_eq!(vm.revision(), revision + 1);
    assert_eq!(
        vm.project
            .event_data
            .iter()
            .find(|data| data.id == expected.id),
        Some(&expected)
    );
    let after = vm.project.clone();
    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.project, before);
    vm.apply(Intent::Redo(0.0));
    assert_eq!(vm.project, after);
}

#[test]
fn invalid_note_deletion_does_not_commit_partial_edits() {
    let mut vm = ProjectViewModel::demo();
    let before = vm.project.clone();
    let revision = vm.revision();
    vm.edit_notes(
        1,
        0,
        [
            NoteEdit::Delete { event_index: 0 },
            NoteEdit::Delete {
                event_index: usize::MAX,
            },
        ],
    );
    assert_eq!(vm.revision(), revision);
    assert_eq!(vm.project, before);
}

#[test]
fn folder_move_noops_and_same_folder_reordering_are_preserved() {
    let mut vm = ProjectViewModel::demo();
    let folder = vm
        .create_asset_folder_for_assets("Folder", &[0, 1], &[0])
        .unwrap();
    let before = vm.project.clone();
    let first = before.asset_folders[0].asset_ids[0];
    let first_index = before
        .assets
        .iter()
        .position(|asset| asset.id == first)
        .unwrap();
    let revision = vm.revision();
    vm.move_asset_to_folder(first_index, Some(folder));
    assert_eq!(vm.revision(), revision + 1);
    let mut reordered = before.asset_folders[0].asset_ids.clone();
    reordered.rotate_left(1);
    assert_eq!(vm.asset_folders()[0].asset_ids, reordered);
    vm.apply(Intent::Undo(0.0));
    assert_eq!(vm.project, before);

    let revision = vm.revision();
    vm.move_assets_to_folder(&[0, 1], &[0], Some(folder));
    vm.move_midi_asset_to_folder(0, Some(folder));
    vm.move_assets_to_folder(&[], &[], None);
    vm.move_asset_to_folder(0, Some(gaw_core::AssetFolderId::new()));
    assert_eq!(vm.revision(), revision);
    assert_eq!(vm.project, before);
}

#[test]
#[ignore = "manual timing: cargo test -p gaw-app benchmark_note_event_deletions -- --ignored --nocapture"]
fn benchmark_note_event_deletions() {
    use std::{hint::black_box, time::Instant};
    for count in [16, 1_024, 10_000] {
        let original: Vec<_> = (0..count).map(event).collect();
        for contiguous in [false, true] {
            let deletions: BTreeSet<_> = (0..count / 2)
                .map(|index| if contiguous { index } else { index * 2 })
                .collect();
            let mut expected = original.clone();
            legacy_delete_event_indices(&mut expected, deletions.clone());
            for (label, delete) in [
                (
                    "legacy",
                    legacy_delete_event_indices as fn(&mut Vec<Event>, BTreeSet<usize>),
                ),
                ("compacted", delete_event_indices),
            ] {
                let mut times = Vec::new();
                for _ in 0..9 {
                    let mut events = original.clone();
                    let selected = deletions.clone();
                    let start = Instant::now();
                    delete(black_box(&mut events), black_box(selected));
                    times.push(start.elapsed());
                    assert_eq!(events, expected);
                    black_box(events);
                }
                times.sort();
                eprintln!(
                    "event deletion ({label}): {count} notes, {} deleted, contiguous={contiguous}: {:?}",
                    deletions.len(),
                    times[4]
                );
            }
        }
    }
}
