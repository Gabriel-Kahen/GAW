use std::{fs, path::Path, process::Command};

use gaw_project::{JsonOperation, JsonTransaction, ProjectPath, ProjectStore, SCHEMA_VERSION};
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

#[test]
fn create_inspect_import_apply_validate_and_recover() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("song");

    let created = gaw(&["create", utf8(&project), "--name", "First"]);
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let created: Value = serde_json::from_slice(&created.stdout).unwrap();
    assert_eq!(created["documents"]["project.json"]["name"], "First");

    let media = directory.path().join("Kick.WAV");
    fs::write(&media, b"audio bytes").unwrap();
    let imported = gaw(&["import", utf8(&project), utf8(&media)]);
    assert!(imported.status.success());
    let imported: Value = serde_json::from_slice(&imported.stdout).unwrap();
    assert_eq!(imported["original_filename"], "Kick.WAV");
    assert!(
        imported["relative_path"]
            .as_str()
            .unwrap()
            .starts_with("assets/media/")
    );

    let store = ProjectStore::open(&project).unwrap();
    let mut project_document = store.load_snapshot().unwrap().documents
        [&ProjectPath::new("project.json").unwrap()]
        .clone();
    project_document["name"] = Value::String("Second".into());
    let transaction_path = directory.path().join("transaction.json");
    fs::write(
        &transaction_path,
        serde_json::to_vec(&JsonTransaction {
            schema_version: SCHEMA_VERSION,
            operations: vec![JsonOperation::Write {
                path: ProjectPath::new("project.json").unwrap(),
                document: project_document,
            }],
        })
        .unwrap(),
    )
    .unwrap();
    let applied = gaw(&["apply", utf8(&project), utf8(&transaction_path)]);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&applied.stdout).unwrap()["documents"]["project.json"]["name"],
        "Second"
    );
    assert!(!project.join(".gaw/recovery.journal").exists());

    let store = ProjectStore::open(&project).unwrap();
    let mut project_document = store.load_snapshot().unwrap().documents
        [&ProjectPath::new("project.json").unwrap()]
        .clone();
    project_document["name"] = Value::String("Recovered".into());
    store
        .append_recovery(&JsonTransaction {
            schema_version: SCHEMA_VERSION,
            operations: vec![JsonOperation::Write {
                path: ProjectPath::new("project.json").unwrap(),
                document: project_document,
            }],
        })
        .unwrap();
    let pending = gaw(&["recover", utf8(&project), "--dry-run"]);
    assert!(pending.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&pending.stdout)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let recovered = gaw(&["recover", utf8(&project)]);
    assert!(recovered.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&recovered.stdout).unwrap()["recovered_transactions"],
        1
    );

    let validated = gaw(&["validate", utf8(&project)]);
    assert!(validated.status.success());
    assert!(
        serde_json::from_slice::<Value>(&validated.stdout).unwrap()["errors"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn invalid_project_and_transaction_fail_without_partial_change() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("song");
    assert!(gaw(&["create", utf8(&project)]).status.success());
    let before = fs::read(project.join("project.json")).unwrap();

    let transaction = directory.path().join("bad.json");
    fs::write(
        &transaction,
        json!({
            "schema_version": SCHEMA_VERSION,
            "operations": [],
            "unknown": true
        })
        .to_string(),
    )
    .unwrap();
    assert!(
        !gaw(&["apply", utf8(&project), utf8(&transaction)])
            .status
            .success()
    );
    assert_eq!(fs::read(project.join("project.json")).unwrap(), before);

    fs::write(project.join("project.json"), br#"{"schema_version":999}"#).unwrap();
    let validated = gaw(&["validate", utf8(&project)]);
    assert!(!validated.status.success());
    let report: Value = serde_json::from_slice(&validated.stdout).unwrap();
    assert!(!report["errors"].as_array().unwrap().is_empty());
}
