use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::Command,
};

use gaw_core::{
    AudioAssetDefinition, AudioClip, Beats, Bpm, Clip, Command as CoreCommand, EqBand, Event,
    EventData, NoteEvent, ParameterValueType, ProcessorKind, Project, Seconds, SourceRange, Track,
    Transaction, Validate,
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
    assert!(schema["x-gaw-processor-catalog"].is_object());
}

fn discovered_processor_schema() -> Value {
    let output = gaw(&["schema", "processor"]);
    assert!(output.status.success());
    serde_json::from_slice(&output.stdout).unwrap()
}

fn processor_entry<'a>(schema: &'a Value, type_id: &str) -> &'a Value {
    schema["x-gaw-processor-catalog"]["processors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["type"] == type_id)
        .unwrap()
}

fn parameter_entry<'a>(processor: &'a Value, id: &str) -> &'a Value {
    processor["parameters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == id)
        .unwrap()
}

fn with_parameter(kind: &ProcessorKind, id: &str, value: Value) -> Option<ProcessorKind> {
    let mut encoded = serde_json::to_value(kind).unwrap();
    let parameters = encoded["parameters"].as_object_mut().unwrap();
    if let Some((collection, field)) = id.split_once("[].") {
        let values = parameters[collection].as_array_mut().unwrap();
        if values.is_empty() && collection == "bands" {
            values.push(serde_json::to_value(EqBand::default()).unwrap());
        }
        values[0][field] = value;
    } else {
        parameters[id] = value;
    }
    serde_json::from_value(encoded).ok()
}

fn discovered_number(value_type: ParameterValueType, value: f64, exact_u64_max: bool) -> Value {
    if value_type == ParameterValueType::Integer {
        if exact_u64_max {
            Value::from(u64::MAX)
        } else {
            serde_json::from_str(&format!("{value:.0}")).unwrap()
        }
    } else {
        Value::from(value)
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn processor_catalog_exhaustively_matches_core_descriptors() {
    let schema = discovered_processor_schema();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["x-gaw-processor-catalog"]["schema_version"], 1);

    let discovered = schema["x-gaw-processor-catalog"]["processors"]
        .as_array()
        .unwrap();
    let catalog = ProcessorKind::catalog_defaults();
    assert_eq!(discovered.len(), catalog.len());
    let mut type_ids = BTreeSet::new();
    let mut analyzer_count = 0;
    for kind in &catalog {
        let entry = processor_entry(&schema, kind.type_id());
        assert!(type_ids.insert(kind.type_id()));
        assert_eq!(entry["analyzer"], kind.is_analyzer(), "{}", kind.type_id());
        analyzer_count += usize::from(kind.is_analyzer());
        let parameters = entry["parameters"].as_array().unwrap();
        assert_eq!(parameters.len(), kind.parameter_descriptors().len());
        for descriptor in kind.parameter_descriptors() {
            let parameter = parameter_entry(entry, descriptor.id);
            assert_eq!(
                parameter["value_type"],
                serde_json::to_value(descriptor.value_type).unwrap()
            );
            assert_eq!(
                parameter["unit"],
                serde_json::to_value(descriptor.unit).unwrap()
            );
            assert_eq!(
                parameter["automation"],
                serde_json::to_value(descriptor.automation).unwrap()
            );
            assert_eq!(
                parameter["display_hint"],
                serde_json::to_value(descriptor.display_hint).unwrap()
            );
            assert_eq!(
                parameter["default"],
                serde_json::from_str::<Value>(descriptor.default_json).unwrap()
            );
            match descriptor.value_type {
                ParameterValueType::Number | ParameterValueType::Integer => {
                    let range = descriptor.range.unwrap();
                    assert_eq!(parameter["minimum"], range.minimum);
                    if !(kind.type_id() == "gaw.beat_repeat" && descriptor.id == "seed") {
                        assert_eq!(parameter["maximum"], range.maximum);
                    }
                }
                ParameterValueType::Choice => {
                    assert_eq!(
                        parameter["enum"],
                        serde_json::to_value(descriptor.choices).unwrap()
                    );
                }
                ParameterValueType::Time | ParameterValueType::Rate => {
                    assert!(parameter["unit_ranges"].is_object());
                }
                ParameterValueType::List => {
                    let expected = match kind {
                        ProcessorKind::ParametricEq(_) => (0, 8),
                        ProcessorKind::RhythmicGate(_) => (1, 64),
                        _ => panic!(
                            "unexpected list descriptor {}.{}",
                            kind.type_id(),
                            descriptor.id
                        ),
                    };
                    assert_eq!(parameter["minItems"], expected.0);
                    assert_eq!(parameter["maxItems"], expected.1);
                }
                ParameterValueType::Boolean => {}
            }
        }
        let expected_constraints = match kind {
            ProcessorKind::Delay(_) | ProcessorKind::Reverb(_) => {
                json!([{"kind":"less_than", "lower":"low_cut_hz", "upper":"high_cut_hz"}])
            }
            ProcessorKind::Spectrum(_) | ProcessorKind::Tuner(_) => {
                json!([{"kind":"less_than", "lower":"minimum_hz", "upper":"maximum_hz"}])
            }
            _ => json!([]),
        };
        assert_eq!(entry["constraints"], expected_constraints);
    }
    assert_eq!(analyzer_count, 6);
    assert_eq!(type_ids.len(), 27);

    let delay = processor_entry(&schema, "gaw.delay");
    assert_eq!(
        parameter_entry(delay, "time")["unit_ranges"]["beats"]["minimum"],
        f64::EPSILON
    );
    let chorus = processor_entry(&schema, "gaw.chorus");
    assert_eq!(
        parameter_entry(chorus, "rate")["unit_ranges"]["hertz"]["maximum"],
        40.0
    );
    assert_eq!(
        parameter_entry(chorus, "rate")["unit_ranges"]["beats"]["minimum"],
        1.0 / 64.0
    );
    let pitch = processor_entry(&schema, "gaw.pitch_shift");
    assert_eq!(
        parameter_entry(pitch, "formant_mode")["enum"],
        json!(["shift"])
    );
    assert_eq!(parameter_entry(pitch, "formant_mode")["automation"], "none");

    for schema_kind in ["project", "command", "transaction", "effect-preset"] {
        let output = gaw(&["schema", schema_kind]);
        assert!(output.status.success(), "{schema_kind}");
        let schema: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            schema["x-gaw-processor-catalog"]["processors"]
                .as_array()
                .unwrap()
                .len(),
            27,
            "{schema_kind}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn every_discovered_processor_value_obeys_validation() {
    let schema = discovered_processor_schema();
    for kind in ProcessorKind::catalog_defaults() {
        let entry = processor_entry(&schema, kind.type_id());
        let constraints = entry["constraints"].as_array().unwrap();
        for parameter in entry["parameters"].as_array().unwrap() {
            let id = parameter["id"].as_str().unwrap();
            let value_type: ParameterValueType = kind
                .parameter_descriptors()
                .iter()
                .find(|descriptor| descriptor.id == id)
                .unwrap()
                .value_type;
            let lower = constraints.iter().any(|rule| rule["lower"] == id);
            let upper = constraints.iter().any(|rule| rule["upper"] == id);
            match value_type {
                ParameterValueType::Number | ParameterValueType::Integer => {
                    let minimum = parameter["minimum"].as_f64().unwrap();
                    let maximum = parameter["maximum"].as_f64().unwrap();
                    let exact_u64_max = kind.type_id() == "gaw.beat_repeat" && id == "seed";
                    let mut accepted = Vec::new();
                    if !upper {
                        accepted.push(minimum);
                    }
                    if !lower {
                        accepted.push(maximum);
                    }
                    for endpoint in accepted {
                        let candidate = with_parameter(
                            &kind,
                            id,
                            discovered_number(value_type, endpoint, exact_u64_max),
                        )
                        .unwrap_or_else(|| panic!("{}.{} endpoint decodes", kind.type_id(), id));
                        candidate
                            .validate()
                            .unwrap_or_else(|error| panic!("{}.{}: {error}", kind.type_id(), id));
                    }
                    let step = if value_type == ParameterValueType::Integer {
                        1.0
                    } else {
                        (maximum - minimum).abs().max(1.0) * 1.0e-4
                    };
                    let invalid_values = if kind.type_id() == "gaw.beat_repeat" && id == "seed" {
                        vec![minimum - step]
                    } else {
                        vec![minimum - step, maximum + step]
                    };
                    for invalid in invalid_values {
                        if let Some(candidate) =
                            with_parameter(&kind, id, discovered_number(value_type, invalid, false))
                        {
                            assert!(
                                candidate.validate().is_err(),
                                "{}.{} {invalid}",
                                kind.type_id(),
                                id
                            );
                        }
                    }
                }
                ParameterValueType::Choice => {
                    for choice in parameter["enum"].as_array().unwrap() {
                        let candidate = with_parameter(&kind, id, choice.clone()).unwrap();
                        candidate
                            .validate()
                            .unwrap_or_else(|error| panic!("{}.{}: {error}", kind.type_id(), id));
                    }
                    assert!(with_parameter(&kind, id, json!("not_in_catalog")).is_none());
                }
                ParameterValueType::Time | ParameterValueType::Rate => {
                    for (unit, range) in parameter["unit_ranges"].as_object().unwrap() {
                        let minimum = range["minimum"].as_f64().unwrap();
                        let maximum = range["maximum"].as_f64().unwrap();
                        for endpoint in [minimum, maximum] {
                            let candidate =
                                with_parameter(&kind, id, json!({"unit": unit, "value": endpoint}))
                                    .unwrap();
                            candidate.validate().unwrap_or_else(|error| {
                                panic!("{}.{} {unit}: {error}", kind.type_id(), id)
                            });
                        }
                        let below_minimum = if minimum == 0.0 {
                            -f64::EPSILON
                        } else {
                            minimum / 2.0
                        };
                        for invalid in [below_minimum, maximum.next_up()] {
                            let candidate =
                                with_parameter(&kind, id, json!({"unit": unit, "value": invalid}))
                                    .unwrap();
                            assert!(
                                candidate.validate().is_err(),
                                "{}.{} {unit} {invalid}",
                                kind.type_id(),
                                id
                            );
                        }
                    }
                }
                ParameterValueType::List | ParameterValueType::Boolean => {}
            }
        }

        for constraint in constraints {
            let lower = constraint["lower"].as_str().unwrap();
            let upper = constraint["upper"].as_str().unwrap();
            let equal = with_parameter(&kind, lower, json!(100.0)).unwrap();
            let equal = with_parameter(&equal, upper, json!(100.0)).unwrap();
            assert!(
                equal.validate().is_err(),
                "{} ordered equality",
                kind.type_id()
            );
            let reversed = with_parameter(&kind, lower, json!(101.0)).unwrap();
            let reversed = with_parameter(&reversed, upper, json!(100.0)).unwrap();
            assert!(
                reversed.validate().is_err(),
                "{} ordered reversal",
                kind.type_id()
            );
        }
    }

    let eq = ProcessorKind::ParametricEq(gaw_core::ParametricEqParameters {
        bands: vec![EqBand::default(); 8],
        ..Default::default()
    });
    assert!(eq.validate().is_ok());
    let too_many = ProcessorKind::ParametricEq(gaw_core::ParametricEqParameters {
        bands: vec![EqBand::default(); 9],
        ..Default::default()
    });
    assert!(too_many.validate().is_err());
    for length in [1, 64] {
        let gate = ProcessorKind::RhythmicGate(gaw_core::RhythmicGateParameters {
            steps: vec![gaw_core::GateStep::default(); length],
            ..Default::default()
        });
        assert!(gate.validate().is_ok());
    }
    for length in [0, 65] {
        let gate = ProcessorKind::RhythmicGate(gaw_core::RhythmicGateParameters {
            steps: vec![gaw_core::GateStep::default(); length],
            ..Default::default()
        });
        assert!(gate.validate().is_err());
    }
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
