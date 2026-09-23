use super::*;

fn legacy_diff(before: &format::Documents, after: &format::Documents) -> StorageTransaction {
    let mut operations = Vec::new();
    for path in before.keys() {
        if !after.contains_key(path) {
            operations.push(StorageOperation::Delete { path: path.clone() });
        }
    }
    for (path, document) in after {
        if before.get(path) != Some(document) {
            operations.push(StorageOperation::Write {
                path: path.clone(),
                document: document.clone(),
            });
        }
    }
    StorageTransaction {
        schema_version: SCHEMA_VERSION,
        operations,
    }
}

#[test]
fn owned_document_diff_preserves_operations_and_reuses_written_values() {
    let paths: Vec<_> = (0..4)
        .map(|index| ProjectPath::new(format!("events/{index}.json")).unwrap())
        .collect();
    for before_mask in 0..16 {
        for after_mask in 0..16 {
            let before: format::Documents = paths
                .iter()
                .enumerate()
                .filter(|(index, _)| before_mask & (1 << index) != 0)
                .map(|(index, path)| (path.clone(), serde_json::json!({"value": index})))
                .collect();
            let after: format::Documents = paths
                .iter()
                .enumerate()
                .filter(|(index, _)| after_mask & (1 << index) != 0)
                .map(|(index, path)| {
                    (
                        path.clone(),
                        serde_json::json!({"value": index + index % 2}),
                    )
                })
                .collect();
            let expected = serde_json::to_value(legacy_diff(&before, &after)).unwrap();
            assert_eq!(
                serde_json::to_value(diff(&before, after)).unwrap(),
                expected
            );
        }
    }
    let document = serde_json::json!({"text": "keep this allocation".repeat(1_024)});
    let text_pointer = document["text"].as_str().unwrap().as_ptr();
    let transaction = diff(
        &format::Documents::new(),
        format::Documents::from([(paths[0].clone(), document)]),
    );
    let StorageOperation::Write { document, .. } = &transaction.operations[0] else {
        panic!("write expected")
    };
    assert_eq!(document["text"].as_str().unwrap().as_ptr(), text_pointer);
}

#[test]
#[ignore = "manual document diff performance measurement"]
fn benchmark_owned_document_diff() {
    use std::{hint::black_box, time::Instant};
    let path = ProjectPath::new("events/notes.json").unwrap();
    let before = format::Documents::from([(path.clone(), serde_json::json!({"events": []}))]);
    let after = format::Documents::from([(
        path,
        serde_json::json!({"events": (0..10_000).map(|index| {
        serde_json::json!({"start": f64::from(index) * 0.25, "duration": 0.125, "note": 60, "velocity": 100})
    }).collect::<Vec<_>>() }),
    )]);
    let expected = serde_json::to_value(legacy_diff(&before, &after)).unwrap();
    for owned in [false, true] {
        let mut timings = Vec::new();
        for _ in 0..9 {
            let input = after.clone();
            let started = Instant::now();
            let transaction = if owned {
                diff(black_box(&before), black_box(input))
            } else {
                let transaction = legacy_diff(black_box(&before), black_box(&input));
                drop(input);
                transaction
            };
            timings.push(started.elapsed());
            assert_eq!(serde_json::to_value(transaction).unwrap(), expected);
        }
        timings.sort_unstable();
        eprintln!(
            "10,000-note document diff owned={owned}: {:?} median",
            timings[4]
        );
    }
}

fn legacy_hash_snapshot(documents: &format::Documents) -> Result<String> {
    let encoded = serde_json::to_vec(documents).map_err(|source| Error::Json {
        path: PathBuf::from("<project-snapshot>"),
        source,
    })?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

#[test]
fn streaming_snapshot_hash_preserves_canonical_bytes() {
    let mut documents = format::Documents::new();
    assert_eq!(
        hash_snapshot(&documents).unwrap(),
        legacy_hash_snapshot(&documents).unwrap()
    );
    for (index, length) in [0, 1, 63, 64, 65, 8_191, 8_192, 8_193, 65_537]
        .into_iter()
        .enumerate()
    {
        documents.insert(
            ProjectPath::new(format!("events/{index}.json")).unwrap(),
            serde_json::json!({
                "text": "\"\\\n\t🎹".repeat(length),
                "array": [null, {}, [], -0.0, i64::MIN, u64::MAX, 1e-30, 1e30],
            }),
        );
        assert_eq!(
            hash_snapshot(&documents).unwrap(),
            legacy_hash_snapshot(&documents).unwrap()
        );
    }
}

#[test]
#[ignore = "manual snapshot hashing performance measurement"]
fn benchmark_snapshot_hashing() {
    use std::{hint::black_box, time::Instant};
    for count in [0, 10_000, 100_000] {
        let documents = format::Documents::from([(
            ProjectPath::new("events/notes.json").unwrap(),
            serde_json::json!({"events": (0..count).map(|index| serde_json::json!({
                "start": f64::from(index) * 0.25, "duration": 0.125, "note": 60, "velocity": 100,
            })).collect::<Vec<_>>()}),
        )]);
        let expected = legacy_hash_snapshot(&documents).unwrap();
        let encoded_bytes = serde_json::to_vec(&documents).unwrap().len();
        for (label, hash) in [
            (
                "allocated",
                legacy_hash_snapshot as fn(&format::Documents) -> Result<String>,
            ),
            ("streaming", hash_snapshot),
        ] {
            let mut times = Vec::new();
            for _ in 0..9 {
                let started = Instant::now();
                let actual = hash(black_box(&documents)).unwrap();
                times.push(started.elapsed());
                assert_eq!(actual, expected);
            }
            times.sort_unstable();
            eprintln!(
                "snapshot hash {label}, {count} notes ({encoded_bytes} serialized bytes): {:?} median",
                times[4]
            );
        }
    }
}

fn legacy_write_json_file(path: &Path, document: &impl Serialize) -> Result<()> {
    make_parent(path)?;
    let mut file = File::create(path).map_err(|error| io(path, error))?;
    serde_json::to_writer_pretty(&mut file, document).map_err(|source| Error::Json {
        path: path.to_owned(),
        source,
    })?;
    file.write_all(b"\n").map_err(|error| io(path, error))?;
    file.sync_all().map_err(|error| io(path, error))
}

#[test]
fn buffered_json_preserves_bytes_and_serialization_failure() {
    struct PartialFailure;
    impl Serialize for PartialFailure {
        fn serialize<S: serde::Serializer>(
            &self,
            serializer: S,
        ) -> std::result::Result<S::Ok, S::Error> {
            use serde::ser::{Error as _, SerializeSeq as _};
            let mut sequence = serializer.serialize_seq(Some(2))?;
            sequence.serialize_element("already serialized")?;
            Err(S::Error::custom("intentional failure"))
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nested/document.json");
    let value = serde_json::json!({
        "escaped": "\"\n\t\\ résumé 🎹".repeat(2_048),
        "numbers": [null, true, -0.0, 1e-20, 1e20],
        "nested": [{"empty": []}, {}],
    });
    write_json_file(&path, &value).unwrap();
    let mut expected = serde_json::to_vec_pretty(&value).unwrap();
    expected.push(b'\n');
    assert_eq!(fs::read(&path).unwrap(), expected);
    let error = write_json_file(&path, &PartialFailure).unwrap_err();
    assert!(matches!(error, Error::Json { path: error_path, source }
        if error_path == path && source.to_string() == "intentional failure"));
    let failed_bytes = fs::read(&path).unwrap();
    assert!(legacy_write_json_file(&path, &PartialFailure).is_err());
    assert_eq!(failed_bytes, fs::read(&path).unwrap());
    assert!(matches!(
        write_json_file(directory.path(), &value),
        Err(Error::Io { .. })
    ));
}

#[test]
#[ignore = "manual durable JSON write performance measurement"]
fn benchmark_durable_json_writes() {
    use std::{hint::black_box, time::Instant};

    let document = serde_json::json!({"events": (0..10_000).map(|index| {
        serde_json::json!({"start": f64::from(index) * 0.25, "duration": 0.125,
            "note": 60, "velocity": 100, "release_velocity": 64})
    }).collect::<Vec<_>>()});
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("events.json");
    let mut expected = serde_json::to_vec_pretty(&document).unwrap();
    expected.push(b'\n');
    for (label, write) in [
        (
            "unbuffered",
            legacy_write_json_file as fn(&Path, &Value) -> Result<()>,
        ),
        ("buffered", write_json_file),
    ] {
        let mut times = Vec::new();
        for _ in 0..5 {
            let started = Instant::now();
            write(black_box(&path), black_box(&document)).unwrap();
            times.push(started.elapsed());
            assert_eq!(fs::read(&path).unwrap(), expected);
        }
        times.sort_unstable();
        eprintln!(
            "durable JSON {label}: {} bytes, {:?} median",
            expected.len(),
            times[2]
        );
    }
}
