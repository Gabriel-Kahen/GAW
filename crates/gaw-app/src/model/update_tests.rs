//! Agent highlight and selection-lookup equivalence and opt-in timings.

use super::*;

fn legacy_update_agent_highlights(vm: &mut ProjectViewModel, changed_ids: &[String], now: f64) {
    for entity_id in changed_ids {
        if let Some(highlight) = vm
            .highlights
            .iter_mut()
            .find(|highlight| highlight.entity_id == *entity_id)
        {
            highlight.changed_at = now;
        } else {
            vm.highlights.push(Highlight {
                entity_id: entity_id.clone(),
                changed_at: now,
            });
        }
    }
    for asset in &mut vm.assets {
        if changed_ids.contains(&asset.id) {
            asset.changed_by_agent = true;
        }
    }
}

fn legacy_selection_for_clip(
    vm: &ProjectViewModel,
    track_id: TrackId,
    clip_id: ClipId,
    processor_id: Option<&ProcessorId>,
) -> Selection {
    let Some(track) = vm
        .current_composition()
        .tracks
        .iter()
        .position(|track| track.id == track_id.to_string())
    else {
        return Selection::None;
    };
    let Some(clip) = vm.current_composition().tracks[track]
        .clips
        .iter()
        .position(|clip| clip.id == clip_id.to_string())
    else {
        return Selection::None;
    };
    processor_id.map_or(Selection::Clip { track, clip }, |processor_id| {
        vm.current_composition().tracks[track].clips[clip]
            .effects
            .iter()
            .position(|effect| effect.id == processor_id.to_string())
            .map_or(Selection::Clip { track, clip }, |effect| {
                Selection::Effect {
                    track,
                    clip,
                    effect,
                }
            })
    })
}

fn highlight_fixture(count: usize) -> ProjectViewModel {
    let mut vm = ProjectViewModel::demo();
    let template = vm.assets[0].clone();
    vm.assets = (0..count)
        .map(|index| Asset {
            id: format!("asset-{index}"),
            changed_by_agent: index % 7 == 0,
            ..template.clone()
        })
        .collect();
    vm.highlights = (0..count)
        .map(|index| Highlight {
            entity_id: format!("asset-{index}"),
            changed_at: index as f64,
        })
        .collect();
    vm
}

fn highlight_state(vm: &ProjectViewModel) -> (Vec<(&str, u64)>, Vec<bool>) {
    (
        vm.highlights
            .iter()
            .map(|h| (h.entity_id.as_str(), h.changed_at.to_bits()))
            .collect(),
        vm.assets
            .iter()
            .map(|asset| asset.changed_by_agent)
            .collect(),
    )
}

#[test]
fn batched_agent_highlights_preserve_duplicates_order_flags_and_timestamps() {
    let mut original = highlight_fixture(4);
    original.highlights.push(Highlight {
        entity_id: "asset-0".into(),
        changed_at: f64::from_bits(0x7ff8_0000_0000_0001),
    });
    original.assets.push(original.assets[0].clone());
    for changed in [
        vec![],
        vec!["asset-0"],
        vec!["new"],
        vec!["asset-0", "asset-0"],
        vec!["new", "asset-2", "asset-0", "new", "", "é"],
        vec!["asset-0"; 24],
        ["new", "asset-2", "asset-0", "new", "", "é"].repeat(4),
    ] {
        let changed: Vec<_> = changed.into_iter().map(str::to_owned).collect();
        for now in [
            -0.0,
            12.0,
            f64::INFINITY,
            f64::from_bits(0x7ff8_0000_0000_0123),
        ] {
            for source in [
                ChangeSource::Agent,
                ChangeSource::Ui,
                ChangeSource::Undo,
                ChangeSource::Redo,
            ] {
                let mut expected = original.clone();
                if source == ChangeSource::Agent {
                    legacy_update_agent_highlights(&mut expected, &changed, now);
                }
                let mut actual = original.clone();
                actual.publish_update(source, "Test update", &changed, now, None, false);
                assert_eq!(highlight_state(&actual), highlight_state(&expected));
                let update = actual.updates.back().unwrap();
                assert_eq!(update.source, source);
                assert_eq!(update.changed_ids.as_ref(), changed);
                assert_eq!(update.label, "Test update");
                assert_eq!(update.revision, actual.revision());
                assert!(!update.audio_render_changed);
                assert!(update.transaction.is_none());
            }
        }
    }
}

fn selection_fixture(track_count: usize, clip_count: usize) -> (ProjectViewModel, TrackId, ClipId) {
    let mut vm = ProjectViewModel::demo();
    let track_id = vm.current_track_id(0).unwrap();
    let clip_id: ClipId = vm.current_composition().tracks[0].clips[0]
        .id
        .parse()
        .unwrap();
    let index = vm
        .project
        .compositions
        .iter()
        .position(|composition| composition.id == vm.current_composition_id())
        .unwrap();
    let template = vm.compositions[index].tracks[0].clone();
    vm.compositions[index].tracks = (0..track_count)
        .map(|_| Track {
            id: TrackId::new().to_string(),
            clips: Vec::new(),
            ..template.clone()
        })
        .collect();
    let track = vm.compositions[index].tracks.last_mut().unwrap();
    track.id = track_id.to_string();
    track.clips = (0..clip_count)
        .map(|_| Clip {
            id: ClipId::new().to_string(),
            ..template.clips[0].clone()
        })
        .collect();
    track.clips.last_mut().unwrap().id = clip_id.to_string();
    (vm, track_id, clip_id)
}

#[test]
fn selection_lookup_keeps_first_matches_missing_fallbacks_and_exact_strings() {
    let (mut vm, track_id, clip_id) = selection_fixture(4, 4);
    let processor_id = ProcessorId::new(
        vm.current_composition().tracks[3].clips[3].effects[0]
            .id
            .clone(),
    )
    .unwrap();
    let missing = ProcessorId::new("missing").unwrap();
    let composition = vm
        .project
        .compositions
        .iter()
        .position(|c| c.id == vm.current_composition_id())
        .unwrap();
    for stage in 0..3 {
        if stage == 1 {
            let mut duplicate = vm.compositions[composition].tracks[3].clone();
            duplicate.clips.insert(0, duplicate.clips[3].clone());
            let effect = duplicate.clips[0].effects[0].clone();
            duplicate.clips[0].effects.insert(0, effect);
            vm.compositions[composition].tracks.insert(0, duplicate);
        } else if stage == 2 {
            for track in &mut vm.compositions[composition].tracks {
                track.id = track.id.to_uppercase();
            }
        }
        for track in [track_id, TrackId::new()] {
            for clip in [clip_id, ClipId::new()] {
                for processor in [None, Some(&processor_id), Some(&missing)] {
                    assert_eq!(
                        vm.selection_for_clip(track, clip, processor),
                        legacy_selection_for_clip(&vm, track, clip, processor)
                    );
                }
            }
        }
        for selection in [
            StableSelection::Track(track_id),
            StableSelection::Sampler { track_id },
        ] {
            let expected = vm
                .current_composition()
                .tracks
                .iter()
                .position(|track| track.id == track_id.to_string())
                .map_or(Selection::None, |track| {
                    if matches!(selection, StableSelection::Track(_)) {
                        Selection::Track { track }
                    } else {
                        Selection::Sampler { track }
                    }
                });
            vm.restore_selection(&selection);
            assert_eq!(vm.selection, expected);
        }
    }
}

#[test]
#[ignore = "manual timing: cargo test -p gaw-app benchmark_agent_highlights -- --ignored --nocapture"]
fn benchmark_agent_highlights() {
    use std::{hint::black_box, time::Instant};
    for count in [16, 1_024, 10_000] {
        let mut original = highlight_fixture(count);
        for (asset, highlight) in original.assets.iter_mut().zip(&mut original.highlights) {
            asset.id = AssetId::new().to_string();
            highlight.entity_id.clone_from(&asset.id);
        }
        let changed: Vec<_> = original
            .assets
            .iter()
            .enumerate()
            .map(|(index, asset)| {
                if index % 2 == 0 {
                    asset.id.clone()
                } else {
                    AssetId::new().to_string()
                }
            })
            .collect();
        let mut expected = original.clone();
        legacy_update_agent_highlights(&mut expected, &changed, 4.0);
        for (label, update) in [
            (
                "legacy",
                legacy_update_agent_highlights as fn(&mut ProjectViewModel, &[String], f64),
            ),
            ("batched", ProjectViewModel::update_agent_highlights),
        ] {
            let mut times = Vec::new();
            for _ in 0..9 {
                let mut vm = original.clone();
                let start = Instant::now();
                update(black_box(&mut vm), black_box(&changed), black_box(4.0));
                times.push(start.elapsed());
                assert_eq!(highlight_state(&vm), highlight_state(&expected));
                black_box(vm);
            }
            times.sort();
            eprintln!(
                "agent highlights ({label}): {count} existing highlights/assets and changed IDs: {:?}",
                times[4]
            );
        }
    }
}

#[test]
#[ignore = "manual timing: cargo test -p gaw-app benchmark_selection_lookup -- --ignored --nocapture"]
fn benchmark_selection_lookup() {
    use std::{hint::black_box, time::Instant};
    for count in [16, 256, 2_048] {
        let (vm, track, clip) = selection_fixture(count, 1_024);
        let expected = legacy_selection_for_clip(&vm, track, clip, None);
        for (label, lookup) in [
            (
                "legacy",
                legacy_selection_for_clip
                    as fn(&ProjectViewModel, TrackId, ClipId, Option<&ProcessorId>) -> Selection,
            ),
            ("hoisted", ProjectViewModel::selection_for_clip),
        ] {
            let mut times = Vec::new();
            for _ in 0..9 {
                let start = Instant::now();
                let actual = lookup(black_box(&vm), black_box(track), black_box(clip), None);
                times.push(start.elapsed());
                assert_eq!(actual, expected);
                black_box(actual);
            }
            times.sort();
            eprintln!(
                "selection lookup ({label}): {count} tracks, 1024 clips on target track: {:?}",
                times[4]
            );
        }
    }
}
