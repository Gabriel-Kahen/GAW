use std::{collections::BTreeMap, fs, path::Path, process::Command};

use gaw_core::{
    AudioAssetDefinition, Bpm, Command as CoreCommand, Project, Track, Transaction, Validate,
};
use gaw_project::ProjectStore;
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
