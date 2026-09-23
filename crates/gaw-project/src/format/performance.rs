use super::*;

fn legacy_asset_index(project: &Project) -> Result<Value> {
    to_value(&AssetIndex {
        schema_version: ASSET_INDEX_SCHEMA_VERSION,
        assets: project.assets.clone(),
        folders: project.asset_folders.clone(),
    })
}

fn asset_project(asset_count: usize, revisions: usize) -> Project {
    use gaw_core::{
        AssetFolderId, AssetRevisionId, AudioAssetDefinition, AudioAssetRevision, AudioTransform,
        ChannelLayout, ContentHash, FrameCount, ImportedAudio, RenderContext,
    };

    let mut project = Project::new(
        "Assets with render history",
        Bpm::new(120.0).unwrap(),
        SampleRate::new(48_000).unwrap(),
    );
    for index in 0..asset_count {
        let mut asset = AudioAsset::imported(
            format!("Audio {index} résumé 🎹"),
            ImportedAudio {
                media_path: gaw_core::ProjectPath::new(format!("assets/media/{index}.wav"))
                    .unwrap(),
                original_filename: format!("take-{index}.wav"),
                content_hash: ContentHash::new("ab".repeat(32)).unwrap(),
                sample_rate: project.sample_rate,
                layout: ChannelLayout::Stereo,
                frames: FrameCount(48_000),
            },
        );
        for revision in 0..revisions {
            asset.revisions.push(AudioAssetRevision {
                id: AssetRevisionId::new(),
                content_hash: ContentHash::new("cd".repeat(32)).unwrap(),
                definition_hash: ContentHash::new("ef".repeat(32)).unwrap(),
                dependency_revision_ids: asset
                    .revisions
                    .last()
                    .map(|previous| previous.id)
                    .into_iter()
                    .collect(),
                render_context: RenderContext {
                    sample_rate: project.sample_rate,
                    layout: ChannelLayout::Stereo,
                    bpm: project.bpm,
                    requested_range: None,
                    engine_version: "deterministic-test".into(),
                    random_seed: 42,
                },
                media_path: gaw_core::ProjectPath::new(format!(
                    "assets/cache/{index}-{revision}.wav"
                ))
                .unwrap(),
                frames: FrameCount(48_000),
            });
        }
        asset.current_revision_id = asset.revisions.last().map(|revision| revision.id);
        if index % 3 == 1 {
            asset.definition = AudioAssetDefinition::Processed {
                source_asset_id: project.assets[index - 1].id,
                transforms: vec![AudioTransform::Reverse],
                effects: vec![],
            };
        } else if index % 3 == 2
            && let Some(revision_id) = asset.current_revision_id
        {
            asset.definition = AudioAssetDefinition::Materialized { revision_id };
        }
        project.assets.push(asset);
    }
    project.asset_folders.push(AssetFolder {
        id: AssetFolderId::new(),
        name: "Takes\n\"Sources\"".into(),
        asset_ids: project.assets.iter().map(|asset| asset.id).collect(),
        event_data_ids: vec![],
    });
    project
}

#[test]
fn borrowed_asset_index_preserves_exact_json_and_round_trips() {
    for project in [
        asset_project(0, 0),
        asset_project(3, 0),
        asset_project(8, 4),
    ] {
        let expected = legacy_asset_index(&project).unwrap();
        let actual = encode_asset_index(&project).unwrap();
        assert_eq!(actual, expected);
        assert_eq!(
            serde_json::to_vec_pretty(&actual).unwrap(),
            serde_json::to_vec_pretty(&expected).unwrap()
        );
        let index = decode_asset_index(&actual).unwrap();
        assert_eq!(index.assets, project.assets);
        assert_eq!(index.folders, project.asset_folders);
        let documents = encode(&project).unwrap();
        assert_eq!(
            documents[&ProjectPath::new("assets/index.json").unwrap()],
            expected
        );
        assert_eq!(decode(&documents).unwrap(), project);
    }
}

#[test]
#[ignore = "manual asset index serialization performance measurement"]
fn benchmark_asset_index_serialization() {
    use std::{hint::black_box, time::Instant};

    let project = asset_project(128, 16);
    let expected = legacy_asset_index(&project).unwrap();
    let mut legacy = Vec::new();
    let mut borrowed = Vec::new();
    for _ in 0..9 {
        for (serialize, durations) in [
            (
                legacy_asset_index as fn(&Project) -> Result<Value>,
                &mut legacy,
            ),
            (encode_asset_index, &mut borrowed),
        ] {
            let started = Instant::now();
            let actual = serialize(black_box(&project)).unwrap();
            durations.push(started.elapsed());
            assert_eq!(actual, expected);
            black_box(actual);
        }
    }
    legacy.sort_unstable();
    borrowed.sort_unstable();
    eprintln!(
        "128 assets / 2,048 revisions: owned index {:?}, borrowed index {:?} median",
        legacy[4], borrowed[4]
    );
}

fn legacy_order_by<T, Id>(
    values: &mut Vec<T>,
    order: &[Id],
    id: impl Fn(&T) -> Id,
    entity: &str,
) -> Result<()>
where
    Id: Copy + Ord + std::fmt::Display,
{
    if values.len() != order.len() {
        return Err(Error::InvalidTransaction(format!(
            "{entity} order does not match stored documents"
        )));
    }
    let mut by_id = std::mem::take(values)
        .into_iter()
        .map(|value| (id(&value), value))
        .collect::<BTreeMap<_, _>>();
    let mut sorted = Vec::with_capacity(order.len());
    for expected in order {
        sorted.push(by_id.remove(expected).ok_or_else(|| {
            Error::InvalidTransaction(format!("{entity} order references missing {expected}"))
        })?);
    }
    *values = sorted;
    Ok(())
}

#[test]
fn indexed_fragment_order_preserves_duplicates_errors_and_error_state() {
    let sequences = |base: u32| {
        (0..=4)
            .flat_map(|len| {
                (0..base.pow(len)).map(move |mut encoded| {
                    (0..len)
                        .map(|_| {
                            let id = encoded % base;
                            encoded /= base;
                            id
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>()
    };
    let orders = sequences(4);
    for ids in sequences(3) {
        let original: Vec<_> = ids
            .iter()
            .copied()
            .enumerate()
            .map(|(payload, id)| (id, payload))
            .collect();
        for order in &orders {
            let mut expected = original.clone();
            let mut actual = original.clone();
            let legacy = legacy_order_by(&mut expected, order, |value| value.0, "fragment");
            let indexed = order_by(
                &mut actual,
                order.iter().copied(),
                |value| value.0,
                "fragment",
            );
            assert_eq!(
                indexed.map_err(|error| format!("{error:?}")),
                legacy.map_err(|error| format!("{error:?}")),
                "input={ids:?}, order={order:?}"
            );
            assert_eq!(actual, expected, "input={ids:?}, order={order:?}");
        }
        let mut expected = BTreeSet::new();
        let legacy = ids.iter().try_for_each(|id| {
            if expected.insert(*id) {
                Ok(())
            } else {
                Err(format!("project manifest contains duplicate fragment {id}"))
            }
        });
        let actual = unique(ids.iter().copied(), "fragment");
        match (actual, legacy) {
            (Ok(actual), Ok(())) => assert_eq!(actual, expected),
            (Err(Error::InvalidTransaction(actual)), Err(expected)) => assert_eq!(actual, expected),
            other => panic!("mismatched uniqueness result: {other:?}"),
        }
    }
}

#[test]
#[ignore = "manual fragment ordering performance measurement"]
fn benchmark_fragment_ordering() {
    use std::{hint::black_box, time::Instant};
    let composition_id = CompositionId::new();
    for count in [16, 512, 4_096] {
        let original: Vec<_> = (0..count)
            .map(|index| Track::audio(composition_id, format!("Track {index}")))
            .collect();
        let order: Vec<_> = original.iter().rev().map(|track| track.id).collect();
        let mut legacy = Vec::new();
        let mut indexed = Vec::new();
        for _ in 0..9 {
            for old in [true, false] {
                let mut tracks = original.clone();
                let started = Instant::now();
                if old {
                    legacy_order_by(
                        black_box(&mut tracks),
                        black_box(&order),
                        |track| track.id,
                        "track",
                    )
                    .unwrap();
                    legacy.push(started.elapsed());
                } else {
                    order_by(
                        black_box(&mut tracks),
                        black_box(&order).iter().copied(),
                        |track| track.id,
                        "track",
                    )
                    .unwrap();
                    indexed.push(started.elapsed());
                }
                assert!(
                    tracks
                        .iter()
                        .map(|track| track.id)
                        .eq(order.iter().copied())
                );
                black_box(tracks);
            }
        }
        legacy.sort_unstable();
        indexed.sort_unstable();
        eprintln!(
            "{count} track fragments: payload map {:?}, index map {:?} median",
            legacy[4], indexed[4]
        );
    }
}
