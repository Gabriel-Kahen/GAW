//! Projection regression fixtures and opt-in development-profile measurements.

use std::{hint::black_box, sync::Arc, time::Instant};

use gaw_core::{
    AudioAsset, AudioClip, Beats, Bpm, Clip, CompositionId, ContentHash, FrameCount, ImportedAudio,
    Project, ProjectPath, SampleRate, Seconds, SourceRange, Track,
};

use super::{ProjectViewModel, WaveformPoint};

fn waveform_project(track_count: usize) -> Project {
    let mut project = Project::new(
        "Waveform completion benchmark",
        Bpm::new(120.0).unwrap(),
        SampleRate::new(48_000).unwrap(),
    );
    let composition_id: CompositionId = project.root_composition_id;
    for index in 0..track_count {
        let asset = AudioAsset::imported(
            format!("Source {index}"),
            ImportedAudio {
                media_path: ProjectPath::new(format!("assets/media/{index}.wav")).unwrap(),
                original_filename: format!("{index}.wav"),
                content_hash: ContentHash::new(format!("{index:064x}")).unwrap(),
                sample_rate: project.sample_rate,
                layout: gaw_core::ChannelLayout::Mono,
                frames: FrameCount(48_000),
            },
        );
        let mut track = Track::audio(composition_id, format!("Track {index}"));
        for clip_index in 0..8 {
            track.clips.push(Clip::Audio(AudioClip::new(
                asset.id,
                Beats::new(f64::from(clip_index) * 4.0).unwrap(),
                Beats::new(2.0).unwrap(),
                SourceRange {
                    start: Seconds::new(0.0).unwrap(),
                    duration: Seconds::new(1.0).unwrap(),
                },
            )));
        }
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        project.assets.push(asset);
    }
    project
}

#[test]
fn projection_preserves_reference_order_and_first_matching_ids() {
    let mut project = waveform_project(2);
    project.compositions[0].track_ids.reverse();
    project.compositions[0]
        .track_ids
        .push(gaw_core::TrackId::new());
    let mut duplicate_track = project.tracks[1].clone();
    duplicate_track.name = "Duplicate track must not win".into();
    project.tracks.push(duplicate_track);
    project.assets[1].tempo = Some(gaw_core::AssetTempo {
        bpm: Bpm::new(123.0).unwrap(),
        first_beat: Seconds::new(0.0).unwrap(),
    });
    let mut duplicate_asset = project.assets[1].clone();
    duplicate_asset.tempo = None;
    project.assets.push(duplicate_asset);
    let points: Arc<[WaveformPoint]> = Arc::from([WaveformPoint {
        minimum: -0.25,
        maximum: 0.75,
    }]);
    let waveforms = super::HashMap::from([(project.assets[1].id.to_string(), points)]);

    let (assets, compositions) = super::projection::adapt_project(&project, Some(&waveforms), None);
    assert_eq!(assets.len(), 3);
    let tracks = &compositions[0].tracks;
    assert_eq!(tracks.len(), 2);
    assert_eq!(tracks[0].name, "Track 1");
    assert_eq!(tracks[1].name, "Track 0");
    for clip in &tracks[0].clips {
        assert!(matches!(clip.kind, super::ClipKind::Audio {
            asset: 1, source_bpm: Some(bpm), ..
        } if bpm.to_bits() == 123.0_f32.to_bits()));
        assert_eq!(
            clip.waveform.as_ref(),
            waveforms[&project.assets[1].id.to_string()].as_ref()
        );
    }
}

#[test]
fn projection_preserves_missing_audio_reference_fallbacks() {
    let mut project = waveform_project(1);
    let Clip::Audio(audio) = &mut project.tracks[0].clips[0] else {
        unreachable!();
    };
    audio.asset_id = gaw_core::AssetId::new();
    let waveforms = super::HashMap::from([(
        project.assets[0].id.to_string(),
        Arc::from([WaveformPoint {
            minimum: -1.0,
            maximum: 1.0,
        }]),
    )]);
    let (_, compositions) = super::projection::adapt_project(&project, Some(&waveforms), None);
    let clip = &compositions[0].tracks[0].clips[0];
    assert!(matches!(clip.kind, super::ClipKind::Audio { asset: 0, .. }));
    assert!(clip.waveform.is_empty());

    project.assets.clear();
    let (_, compositions) = super::projection::adapt_project(&project, None, None);
    assert!(matches!(
        compositions[0].tracks[0].clips[0].kind,
        super::ClipKind::Audio {
            asset: 0,
            source_bpm: None,
            ..
        }
    ));
}

#[test]
fn projection_preserves_event_windows_and_composition_references() {
    let mut project = waveform_project(1);
    let beats = |value| Beats::new(value).unwrap();
    let mut data = gaw_core::EventData::new("Notes");
    data.events = (0..4)
        .map(|start| {
            gaw_core::Event::Note(
                gaw_core::NoteEvent::new(beats(f64::from(start)), beats(0.5), 60 + start, 100)
                    .unwrap(),
            )
        })
        .collect();
    let mut event_clip = gaw_core::EventClip::new(data.id, beats(0.0), beats(2.0));
    event_clip.source_start = beats(1.0);
    project.event_data.push(data.clone());
    data.events.clear();
    project.event_data.push(data);
    let child = gaw_core::Composition::new("Child", beats(4.0));
    let child_clip = gaw_core::CompositionClip::new(child.id, beats(0.0), beats(4.0));
    project.compositions.extend([child.clone(), child]);
    project.tracks[0].clips = vec![
        Clip::Event(event_clip),
        Clip::Event(gaw_core::EventClip::new(
            gaw_core::EventDataId::new(),
            beats(0.0),
            beats(2.0),
        )),
        Clip::Composition(child_clip),
        Clip::Composition(gaw_core::CompositionClip::new(
            gaw_core::CompositionId::new(),
            beats(0.0),
            beats(4.0),
        )),
    ];
    let (_, compositions) = super::projection::adapt_project(&project, None, None);
    let clips = &compositions[0].tracks[0].clips;
    let super::ClipKind::Event { notes } = &clips[0].kind else {
        panic!("event clip");
    };
    assert_eq!(
        notes
            .iter()
            .map(|note| (note.event_index, note.start, note.pitch))
            .collect::<Vec<_>>(),
        [(1, 0.0, 61), (2, 1.0, 62)]
    );
    assert!(matches!(&clips[1].kind, super::ClipKind::Event { notes } if notes.is_empty()));
    assert!(matches!(
        clips[2].kind,
        super::ClipKind::Composition { child: 1, .. }
    ));
    assert!(matches!(
        clips[3].kind,
        super::ClipKind::Composition { child: 0, .. }
    ));
}

#[test]
#[ignore = "manual timing: cargo test -p gaw-app benchmark_waveform_completions -- --ignored --nocapture"]
fn benchmark_waveform_completions() {
    for tracks in [16, 64, 128] {
        let mut vm = ProjectViewModel::from_project(waveform_project(tracks)).unwrap();
        let points: Arc<[WaveformPoint]> = Arc::from(vec![
            WaveformPoint {
                minimum: -0.5,
                maximum: 0.5
            };
            128
        ]);
        let inputs: Vec<_> = vm
            .assets
            .iter()
            .map(|asset| (asset.id.clone(), asset.content_hash.clone().unwrap()))
            .collect();
        let mut elapsed = Vec::new();
        for _ in 0..7 {
            let started = Instant::now();
            for (id, hash) in &inputs {
                vm.install_asset_waveform(black_box(id), black_box(hash), Arc::clone(&points));
            }
            elapsed.push(started.elapsed());
            black_box(&vm);
        }
        elapsed.sort();
        eprintln!(
            "waveform completion batch: {tracks} assets/tracks, {} clips, median {:?}",
            tracks * 8,
            elapsed[3]
        );
    }
}

#[test]
#[ignore = "manual timing: cargo test -p gaw-app benchmark_project_projection -- --ignored --nocapture"]
fn benchmark_project_projection() {
    for tracks in [16, 128, 512, 2_048] {
        let project = waveform_project(tracks);
        let mut elapsed = Vec::new();
        for _ in 0..9 {
            let started = Instant::now();
            let (assets, compositions) =
                super::projection::adapt_project(black_box(&project), None, None);
            elapsed.push(started.elapsed());
            assert_eq!(assets.len(), tracks);
            assert_eq!(compositions[0].tracks.len(), tracks);
            for (index, track) in compositions[0].tracks.iter().enumerate() {
                assert_eq!(track.clips.len(), 8);
                for clip in &track.clips {
                    assert!(
                        matches!(clip.kind, super::ClipKind::Audio { asset, .. } if asset == index)
                    );
                }
            }
            black_box((assets, compositions));
        }
        elapsed.sort();
        eprintln!(
            "project projection: {tracks} assets/tracks, {} clips, median {:?}",
            tracks * 8,
            elapsed[4]
        );
    }
}
