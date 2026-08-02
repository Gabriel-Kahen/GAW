use std::{collections::BTreeMap, fs, path::Path, process::Command};

use gaw_core::{
    AudioAssetDefinition, AudioClip, Beats, Bpm, Clip, Command as CoreCommand, Event, EventData,
    NoteEvent, Project, Seconds, SourceRange, Track, Transaction, Validate,
};
use gaw_project::{ProjectStore, export_midi, import_midi};
use serde_json::{Value, json};

fn gaw(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_gaw"))
        .args(args)
        .output()
        .unwrap()
}

fn utf8(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn wav(path: &Path) {
    let mut writer = hound::WavWriter::create(
        path,
        hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        },
    )
    .unwrap();
    for _ in 0..256 {
        writer.write_sample(12_i16).unwrap();
        writer.write_sample(-12_i16).unwrap();
    }
    writer.finalize().unwrap();
}

fn arrange_imported_audio(project_path: &Path, transaction_path: &Path) {
    let project = ProjectStore::open(project_path)
        .unwrap()
        .load_project()
        .unwrap();
    let asset = &project.assets[0];
    let mut composition = project.compositions[0].clone();
    composition.length = Beats::new(0.02).unwrap();
    let mut track = Track::audio(project.root_composition_id, "Imported audio");
    track.clips.push(Clip::Audio(AudioClip::new(
        asset.id,
        Beats::new(0.0).unwrap(),
        Beats::new(0.02).unwrap(),
        SourceRange {
            start: Seconds::new(0.0).unwrap(),
            duration: Seconds::new(256.0 / 48_000.0).unwrap(),
        },
    )));
    let transaction = Transaction::new([
        CoreCommand::UpdateComposition { composition },
        CoreCommand::AddTrack { track, index: 0 },
    ]);
    fs::write(transaction_path, serde_json::to_vec(&transaction).unwrap()).unwrap();
    let applied = gaw(&["apply", utf8(project_path), utf8(transaction_path)]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
}

fn canonical_json(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.starts_with(root.join(".gaw")) || path.starts_with(root.join("assets/media")) {
                continue;
            }
            if path.is_dir() {
                visit(root, &path, files);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                files.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    visit(root, root, &mut files);
    files
}

#[test]
#[allow(clippy::too_many_lines)]
fn typed_end_to_end_create_import_apply_reopen_validate_and_recover() {
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory.path().join("song");

    let created = gaw(&["create", utf8(&project_path), "--name", "First"]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created: Project = serde_json::from_slice(&created.stdout).unwrap();
    assert_eq!(created.name, "First");
    assert!(created.validate().is_ok());
    assert!(project_path.join("project.json").is_file());
    assert!(project_path.join("assets/index.json").is_file());
    assert!(
        project_path
            .join(format!(
                "compositions/{}/composition.json",
                created.root_composition_id
            ))
            .is_file()
    );

    let media = directory.path().join("Kick.WAV");
    wav(&media);
    let imported = gaw(&["import", utf8(&project_path), utf8(&media)]);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let imported: Value = serde_json::from_slice(&imported.stdout).unwrap();
    assert_eq!(imported["original_filename"], "Kick.WAV");
    assert!(
        imported["relative_path"]
            .as_str()
            .unwrap()
            .starts_with("assets/media/")
    );
    let asset_index = fs::read_to_string(project_path.join("assets/index.json")).unwrap();
    assert!(!asset_index.contains(utf8(directory.path())));
    let after_import = ProjectStore::open(&project_path)
        .unwrap()
        .load_project()
        .unwrap();
    let AudioAssetDefinition::Imported(source) = &after_import.assets[0].definition else {
        panic!("import did not add a typed asset")
    };
    assert_eq!(source.frames.0, 256);

    let track = Track::audio(created.root_composition_id, "Agent Track");
    let transaction = Transaction::named(
        "agent state transition",
        [
            CoreCommand::SetProjectName {
                name: "Second".into(),
            },
            CoreCommand::AddTrack {
                track: track.clone(),
                index: 0,
            },
        ],
    );
    let transaction_path = directory.path().join("transaction.json");
    fs::write(&transaction_path, serde_json::to_vec(&transaction).unwrap()).unwrap();
    let applied = gaw(&["apply", utf8(&project_path), utf8(&transaction_path)]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let applied: Project = serde_json::from_slice(&applied.stdout).unwrap();
    assert_eq!(applied.name, "Second");
    assert_eq!(applied.tracks, vec![track]);
    assert!(!project_path.join(".gaw/recovery.journal").exists());

    let store = ProjectStore::open(&project_path).unwrap();
    store
        .append_recovery(&Transaction::new([CoreCommand::SetProjectTempo {
            bpm: Bpm::new(96.0).unwrap(),
        }]))
        .unwrap();
    let pending = gaw(&["recover", utf8(&project_path), "--dry-run"]);
    assert!(pending.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&pending.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(gaw(&["recover", utf8(&project_path)]).status.success());
    let recovered_bpm = ProjectStore::open(&project_path)
        .unwrap()
        .load_project()
        .unwrap()
        .bpm
        .value();
    assert!((recovered_bpm - 96.0).abs() < f64::EPSILON);

    let validated = gaw(&["validate", utf8(&project_path)]);
    assert!(validated.status.success());
    assert!(
        serde_json::from_slice::<Value>(&validated.stdout).unwrap()["errors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let inspected = gaw(&["inspect", utf8(&project_path)]);
    assert!(
        serde_json::from_slice::<Project>(&inspected.stdout)
            .unwrap()
            .validate()
            .is_ok()
    );
}

#[test]
fn typed_failures_are_strict_and_never_partially_persist() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("song");
    assert!(gaw(&["create", utf8(&project)]).status.success());
    let before = canonical_json(&project);

    let bad = directory.path().join("bad.json");
    let transaction = Transaction::new([
        CoreCommand::SetProjectName {
            name: "Partial".into(),
        },
        CoreCommand::RemoveAsset {
            asset_id: gaw_core::AssetId::new(),
        },
    ]);
    fs::write(&bad, serde_json::to_vec(&transaction).unwrap()).unwrap();
    assert!(!gaw(&["apply", utf8(&project), utf8(&bad)]).status.success());
    assert_eq!(canonical_json(&project), before);

    fs::write(&bad, json!({"commands": [], "unknown": true}).to_string()).unwrap();
    assert!(!gaw(&["apply", utf8(&project), utf8(&bad)]).status.success());
    assert_eq!(canonical_json(&project), before);

    fs::write(&bad, json!({"commands": []}).to_string()).unwrap();
    assert!(!gaw(&["apply", utf8(&project), utf8(&bad)]).status.success());
    assert_eq!(canonical_json(&project), before);
}

#[test]
fn transaction_schema_is_available_for_agents() {
    let output = gaw(&["schema", "transaction"]);
    assert!(output.status.success());
    let schema: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert!(schema["$defs"]["Command"].is_object());
}

#[test]
fn malformed_typed_project_reports_structured_validation_failure() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("song");
    assert!(gaw(&["create", utf8(&project)]).status.success());
    fs::write(project.join("project.json"), br#"{"schema_version":999}"#).unwrap();
    let validated = gaw(&["validate", utf8(&project)]);
    assert!(!validated.status.success());
    assert!(
        !serde_json::from_slice::<Value>(&validated.stdout).unwrap()["errors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn midi_is_explicit_interchange_not_canonical_file_state() {
    let directory = tempfile::tempdir().unwrap();
    let project_path = directory.path().join("song");
    assert!(gaw(&["create", utf8(&project_path)]).status.success());

    let source = directory.path().join("source.mid");
    let mut data = EventData::new("Keys");
    data.events.push(Event::Note(
        NoteEvent::new(Beats::new(0.25).unwrap(), Beats::new(1.5).unwrap(), 60, 100).unwrap(),
    ));
    export_midi(&data, Bpm::new(98.0).unwrap(), 960, &source).unwrap();

    let imported = gaw(&["midi-import", utf8(&project_path), utf8(&source)]);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let imported: Value = serde_json::from_slice(&imported.stdout).unwrap();
    let id = imported["event_data"][0]["id"].as_str().unwrap();
    let project = ProjectStore::open(&project_path)
        .unwrap()
        .load_project()
        .unwrap();
    assert_eq!(project.event_data.len(), 1);
    assert_eq!(project.event_data[0].events, data.events);
    assert!(!project_path.join("source.mid").exists());

    let destination = directory.path().join("export.mid");
    let exported = gaw(&[
        "midi-export",
        utf8(&project_path),
        id,
        utf8(&destination),
        "--ppqn",
        "480",
    ]);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let round_trip = import_midi(&destination).unwrap();
    assert_eq!(round_trip.event_data[0].events, data.events);
    assert!((round_trip.suggested_bpm.unwrap().value() - 120.0).abs() < 0.001);
}

#[test]
fn final_export_is_store_backed_range_checked_and_deterministic() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("song");
    let media = directory.path().join("source.wav");
    let transaction = directory.path().join("arrange.json");
    let first = directory.path().join("first.wav");
    let second = directory.path().join("second.wav");
    assert!(gaw(&["create", utf8(&project)]).status.success());
    wav(&media);
    assert!(
        gaw(&["import", utf8(&project), utf8(&media)])
            .status
            .success()
    );
    arrange_imported_audio(&project, &transaction);

    let export_args = |destination: &Path, block: &str| {
        gaw(&[
            "export",
            utf8(&project),
            utf8(destination),
            "--sample-rate",
            "24000",
            "--channels",
            "mono",
            "--tail",
            "exclude",
            "--encoding",
            "pcm16",
            "--block-frames",
            block,
        ])
    };
    let exported = export_args(&first, "1");
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let report: Value = serde_json::from_slice(&exported.stdout).unwrap();
    assert_eq!(report["kind"], "gaw.final_export");
    assert_eq!(report["source"]["sample_rate"], 48_000);
    assert_eq!(report["source"]["frames"], 480);
    assert_eq!(report["output"]["sample_rate"], 24_000);
    assert_eq!(report["output"]["layout"], "mono");
    assert_eq!(report["output"]["frames"], 240);

    drop(
        ProjectStore::open(&project)
            .unwrap()
            .load_project()
            .unwrap(),
    );
    let reopened = export_args(&second, "127");
    assert!(
        reopened.status.success(),
        "{}",
        String::from_utf8_lossy(&reopened.stderr)
    );
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());

    let invalid = gaw(&[
        "export",
        utf8(&project),
        utf8(&second),
        "--start-frame",
        "479",
        "--frames",
        "2",
        "--tail",
        "exclude",
    ]);
    assert!(!invalid.status.success());
    let error: Value = serde_json::from_slice(&invalid.stderr).unwrap();
    assert_eq!(error["kind"], "gaw.error");
    assert_eq!(error["code"], "audio.export_failed");

    let reopened = ProjectStore::open(&project)
        .unwrap()
        .load_project()
        .unwrap();
    let AudioAssetDefinition::Imported(imported) = &reopened.assets[0].definition else {
        unreachable!()
    };
    fs::write(project.join(imported.media_path.as_str()), b"corrupt").unwrap();
    let corrupt = export_args(&second, "64");
    assert!(!corrupt.status.success());
    let error: Value = serde_json::from_slice(&corrupt.stderr).unwrap();
    assert!(
        error["causes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cause| cause.as_str().unwrap().contains("content hash"))
    );
}
