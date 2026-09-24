//! Gesture output equivalence and opt-in development-profile measurements.

use super::*;

// Keep the original independent reductions as the action oracle.
fn legacy_move_selected(
    state: &mut PianoRollState,
    track: usize,
    clip_index: usize,
    clip: &Clip,
    notes: &[Note],
    dragged: &Note,
    delta: Vec2,
    beats_per_bar: f32,
    actions: &mut Vec<Intent>,
) {
    if !state.selected.contains(&dragged.event_index) {
        state.selected.clear();
        state.selected.insert(dragged.event_index);
    }
    let requested = delta.x / state.pixels_per_beat;
    let snapped_start = state.snap_edit(dragged.start + requested, beats_per_bar);
    let beat_delta = snapped_start - dragged.start;
    let requested_pitch = (-delta.y / state.row_height).round() as i16;
    let min_pitch = state
        .selected_notes(notes)
        .map(|note| i16::from(note.pitch))
        .min()
        .unwrap_or(0);
    let max_pitch = state
        .selected_notes(notes)
        .map(|note| i16::from(note.pitch))
        .max()
        .unwrap_or(127);
    let pitch_delta = requested_pitch.clamp(-min_pitch, 127 - max_pitch);
    let min_start = state
        .selected_notes(notes)
        .map(|note| note.start)
        .reduce(f32::min)
        .unwrap_or(0.0);
    let max_end = state
        .selected_notes(notes)
        .map(|note| note.start + note.length)
        .reduce(f32::max)
        .unwrap_or(clip.length);
    let beat_delta = clamp_move_delta(beat_delta, min_start, max_end, clip.length);
    if beat_delta.abs() <= f32::EPSILON && pitch_delta == 0 {
        return;
    }
    let updates = state
        .selected_notes(notes)
        .map(|note| NoteUpdate {
            event_index: note.event_index,
            start: note.start + beat_delta,
            length: note.length,
            pitch: (i16::from(note.pitch) + pitch_delta).clamp(0, 127) as u8,
            velocity: (note.velocity * 127.0).round() as u8,
        })
        .collect();
    push_updates(track, clip_index, updates, actions);
}

type MoveNotes =
    fn(&mut PianoRollState, usize, usize, &Clip, &[Note], &Note, Vec2, f32, &mut Vec<Intent>);

fn action_bits(actions: &[Intent]) -> Vec<u64> {
    let mut bits = Vec::new();
    for action in actions {
        let Intent::EditNotes { track, clip, notes } = action else {
            panic!("expected note update");
        };
        bits.extend([*track as u64, *clip as u64, notes.len() as u64]);
        for note in notes {
            bits.extend([
                note.event_index as u64,
                u64::from(note.start.to_bits()),
                u64::from(note.length.to_bits()),
                u64::from(note.pitch),
                u64::from(note.velocity),
            ]);
        }
    }
    bits
}

#[test]
fn move_bounds_preserve_bitwise_actions_and_selection() {
    let mut cases = vec![Vec::new()];
    for starts in [
        vec![0.0, 1.0, 3.5, 3.5],
        vec![-0.0, 0.0, -0.0, 0.0],
        vec![
            f32::from_bits(0x7fc0_0001),
            f32::from_bits(0x7fc0_0123),
            f32::INFINITY,
            f32::NEG_INFINITY,
        ],
        vec![f32::from_bits(0x7fc0_0001), f32::from_bits(0x7fc0_0123)],
    ] {
        let notes: Vec<_> = starts
            .into_iter()
            .enumerate()
            .map(|(index, start)| Note {
                event_index: index % 3,
                start,
                length: [0.5, -0.0, 0.0, -1.0][index % 4],
                pitch: [0, 60, 127][index % 3],
                velocity: [0.75, f32::NAN, -0.0, f32::INFINITY][index % 4],
                cents: 0.0,
            })
            .collect();
        cases.push(notes.clone());
        cases.push(notes.into_iter().rev().collect());
    }
    for notes in cases {
        for selected in [
            BTreeSet::new(),
            BTreeSet::from([0, 1, 2, 999]),
            BTreeSet::from([1, 999]),
        ] {
            for dragged_index in [0, 999] {
                let dragged = Note {
                    event_index: dragged_index,
                    start: 1.0,
                    length: 0.5,
                    pitch: 60,
                    velocity: 0.75,
                    cents: 0.0,
                };
                for bypass_snap in [false, true] {
                    for clip_length in [0.0, 4.0, -1.0, f32::INFINITY, f32::NAN] {
                        let mut clip = super::tests::event_clip();
                        clip.length = clip_length;
                        for delta in [
                            Vec2::ZERO,
                            Vec2::new(-720.0, 2_000.0),
                            Vec2::new(144.0, -28.0),
                            Vec2::new(f32::NAN, 14.0),
                        ] {
                            let run = |move_notes: MoveNotes| {
                                let mut state = PianoRollState {
                                    selected: selected.clone(),
                                    bypass_snap,
                                    ..PianoRollState::default()
                                };
                                let mut actions = Vec::new();
                                move_notes(
                                    &mut state,
                                    3,
                                    7,
                                    &clip,
                                    &notes,
                                    &dragged,
                                    delta,
                                    4.0,
                                    &mut actions,
                                );
                                (state.selected, action_bits(&actions))
                            };
                            assert_eq!(run(move_selected), run(legacy_move_selected));
                        }
                    }
                }
            }
        }
    }
}

#[test]
#[ignore = "manual timing: cargo test -p gaw-app benchmark_selected_note_drag -- --ignored --nocapture"]
fn benchmark_selected_note_drag() {
    use std::{hint::black_box, time::Instant};
    for count in [16, 1_024, 10_000] {
        let notes: Vec<_> = (0..count)
            .map(|index| Note {
                event_index: index,
                start: (index % 128) as f32 * 0.25,
                length: 0.125,
                pitch: 48 + (index % 24) as u8,
                velocity: 0.75,
                cents: 0.0,
            })
            .collect();
        let mut clip = super::tests::event_clip();
        clip.length = 64.0;
        let mut state = PianoRollState {
            selected: (0..count).collect(),
            ..PianoRollState::default()
        };
        let mut expected = Vec::new();
        legacy_move_selected(
            &mut state,
            0,
            0,
            &clip,
            &notes,
            &notes[0],
            Vec2::new(18.0, -14.0),
            4.0,
            &mut expected,
        );
        let expected = action_bits(&expected);
        for (label, move_notes) in [
            ("legacy", legacy_move_selected as MoveNotes),
            ("combined", move_selected),
        ] {
            let mut times = Vec::new();
            for _ in 0..9 {
                let mut actions = Vec::new();
                let start = Instant::now();
                move_notes(
                    black_box(&mut state),
                    0,
                    0,
                    black_box(&clip),
                    black_box(&notes),
                    black_box(&notes[0]),
                    black_box(Vec2::new(18.0, -14.0)),
                    4.0,
                    &mut actions,
                );
                times.push(start.elapsed());
                assert_eq!(action_bits(&actions), expected);
                black_box(actions);
            }
            times.sort();
            eprintln!(
                "selected-note drag ({label}): {count} notes, {:?}",
                times[4]
            );
        }
    }
}
