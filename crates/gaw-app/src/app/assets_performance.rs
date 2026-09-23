//! Asset-browser row equivalence and opt-in scaling measurements.

use super::*;

// Retain the previous algorithm as an independent row-order oracle.
fn legacy_asset_browser_rows(
    audio_assets: &[(usize, gaw_core::AssetId)],
    midi_assets: &[(usize, gaw_core::EventDataId)],
    folders: &[gaw_core::AssetFolder],
    collapsed_folders: &HashSet<gaw_core::AssetFolderId>,
) -> Vec<AssetBrowserRow> {
    let audio_folders = folders
        .iter()
        .flat_map(|folder| folder.asset_ids.iter().map(move |id| (*id, folder.id)))
        .collect::<HashMap<_, _>>();
    let midi_folders = folders
        .iter()
        .flat_map(|folder| folder.event_data_ids.iter().map(move |id| (*id, folder.id)))
        .collect::<HashMap<_, _>>();
    let mut rows = Vec::with_capacity(audio_assets.len() + midi_assets.len() + folders.len());
    rows.extend(
        audio_assets
            .iter()
            .filter(|(_, id)| !audio_folders.contains_key(id))
            .map(|(index, _)| AssetBrowserRow::Audio(*index)),
    );
    rows.extend(
        midi_assets
            .iter()
            .filter(|(_, id)| !midi_folders.contains_key(id))
            .map(|(index, _)| AssetBrowserRow::Midi(*index)),
    );
    for (index, folder) in folders.iter().enumerate() {
        let collapsed = collapsed_folders.contains(&folder.id);
        rows.push(AssetBrowserRow::Folder { index, collapsed });
        if collapsed {
            continue;
        }
        rows.extend(
            audio_assets
                .iter()
                .filter(|(_, id)| audio_folders.get(id) == Some(&folder.id))
                .map(|(index, _)| AssetBrowserRow::Audio(*index)),
        );
        rows.extend(
            midi_assets
                .iter()
                .filter(|(_, id)| midi_folders.get(id) == Some(&folder.id))
                .map(|(index, _)| AssetBrowserRow::Midi(*index)),
        );
    }
    rows
}

struct AssetBrowserFixture {
    audio: Vec<(usize, gaw_core::AssetId)>,
    midi: Vec<(usize, gaw_core::EventDataId)>,
    folders: Vec<gaw_core::AssetFolder>,
}

fn fixture(asset_count: usize, folder_count: usize) -> AssetBrowserFixture {
    let audio: Vec<_> = (0..asset_count)
        .map(|index| (index * 2, gaw_core::AssetId::new()))
        .collect();
    let midi: Vec<_> = (0..asset_count / 4)
        .map(|index| (index * 3, gaw_core::EventDataId::new()))
        .collect();
    let mut folders: Vec<_> = (0..folder_count)
        .map(|index| gaw_core::AssetFolder {
            id: gaw_core::AssetFolderId::new(),
            name: format!("Folder {index}"),
            asset_ids: Vec::new(),
            event_data_ids: Vec::new(),
        })
        .collect();
    if folder_count > 0 {
        for (offset, (_, id)) in audio
            .iter()
            .enumerate()
            .filter(|(offset, _)| offset % 7 != 0)
        {
            folders[offset % folder_count].asset_ids.push(*id);
        }
        for (offset, (_, id)) in midi
            .iter()
            .enumerate()
            .filter(|(offset, _)| offset % 3 != 0)
        {
            folders[offset % folder_count].event_data_ids.push(*id);
        }
    }
    AssetBrowserFixture {
        audio,
        midi,
        folders,
    }
}

#[test]
fn asset_browser_grouping_preserves_all_rows_and_folder_membership() {
    for duplicates in [false, true] {
        let AssetBrowserFixture {
            mut audio,
            mut midi,
            mut folders,
        } = fixture(24, 4);
        if duplicates {
            audio.push(audio[1]);
            midi.push(midi[1]);
            folders[0].asset_ids.extend([audio[2].1, audio[2].1]);
            folders[3].asset_ids.push(audio[2].1);
            folders[0].event_data_ids.push(midi[2].1);
            folders[3].event_data_ids.push(midi[2].1);
            folders[1].id = folders[0].id;
        }
        folders[0].asset_ids.push(gaw_core::AssetId::new());
        folders[0].event_data_ids.push(gaw_core::EventDataId::new());
        for mask in 0..16 {
            let collapsed = folders
                .iter()
                .enumerate()
                .filter(|(index, _)| mask & (1 << index) != 0)
                .map(|(_, folder)| folder.id)
                .collect();
            let rows = asset_browser_rows(&audio, &midi, &folders, &collapsed);
            assert_eq!(
                rows,
                legacy_asset_browser_rows(&audio, &midi, &folders, &collapsed)
            );
            let foldered_audio: HashSet<_> = folders
                .iter()
                .flat_map(|folder| folder.asset_ids.iter().copied())
                .collect();
            let foldered_midi: HashSet<_> = folders
                .iter()
                .flat_map(|folder| folder.event_data_ids.iter().copied())
                .collect();
            for row in rows {
                match row {
                    AssetBrowserRow::Folder { .. } => {}
                    AssetBrowserRow::Audio(index) => {
                        let id = audio.iter().find(|(i, _)| *i == index).unwrap().1;
                        assert_eq!(
                            foldered_audio.contains(&id),
                            folders.iter().any(|folder| folder.asset_ids.contains(&id))
                        );
                    }
                    AssetBrowserRow::Midi(index) => {
                        let id = midi.iter().find(|(i, _)| *i == index).unwrap().1;
                        assert_eq!(
                            foldered_midi.contains(&id),
                            folders
                                .iter()
                                .any(|folder| folder.event_data_ids.contains(&id))
                        );
                    }
                }
            }
        }
    }
    for (assets, folders) in [(0, 0), (0, 3), (8, 0)] {
        let AssetBrowserFixture {
            audio,
            midi,
            folders,
        } = fixture(assets, folders);
        assert_eq!(
            asset_browser_rows(&audio, &midi, &folders, &HashSet::new()),
            legacy_asset_browser_rows(&audio, &midi, &folders, &HashSet::new())
        );
    }
}

#[test]
#[ignore = "manual timing: cargo test -p gaw-app benchmark_asset_browser_rows -- --ignored --nocapture"]
fn benchmark_asset_browser_rows() {
    use std::{hint::black_box, time::Instant};
    for (assets, folders) in [(16, 4), (1_024, 128), (4_096, 256)] {
        let AssetBrowserFixture {
            audio,
            midi,
            folders,
        } = fixture(assets, folders);
        let collapsed = HashSet::new();
        let expected = legacy_asset_browser_rows(&audio, &midi, &folders, &collapsed);
        for (label, build) in [
            (
                "legacy",
                legacy_asset_browser_rows as fn(&[_], &[_], &[_], &_) -> _,
            ),
            ("grouped", asset_browser_rows),
        ] {
            let mut times = Vec::new();
            for _ in 0..9 {
                let start = Instant::now();
                let rows = build(
                    black_box(&audio),
                    black_box(&midi),
                    black_box(&folders),
                    black_box(&collapsed),
                );
                times.push(start.elapsed());
                assert_eq!(rows, expected);
                black_box(rows);
            }
            times.sort();
            eprintln!(
                "asset browser ({label}): {} assets, {} folders: {:?}",
                audio.len() + midi.len(),
                folders.len(),
                times[4]
            );
        }
    }
}
