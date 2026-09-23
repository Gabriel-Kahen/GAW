use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use gaw_core::{
    AssetFolder, AudioAsset, AutomationLane, AutomationLaneId, Bpm, Composition, CompositionId,
    EventData, EventDataId, Project, ProjectId, ProjectSettings, SampleRate, TimeSignature, Track,
    TrackId, Validate,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{Error, ProjectPath, Result, SCHEMA_VERSION};

const ASSET_INDEX_SCHEMA_VERSION: u32 = 2;

pub(crate) type Documents = BTreeMap<ProjectPath, Value>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProjectDocument {
    schema_version: u32,
    id: ProjectId,
    name: String,
    root_composition_id: CompositionId,
    bpm: Bpm,
    #[serde(default)]
    time_signature: TimeSignature,
    sample_rate: SampleRate,
    settings: ProjectSettings,
    event_order: Vec<EventDataId>,
    composition_order: Vec<CompositionId>,
    track_order: Vec<TrackLocation>,
    automation_order: Vec<AutomationLocation>,
}

/// One track location declared by the project manifest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrackLocation {
    pub composition_id: CompositionId,
    pub id: TrackId,
}

/// One automation-lane location declared by the project manifest.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AutomationLocation {
    pub composition_id: CompositionId,
    pub id: AutomationLaneId,
}

/// Strictly decoded `assets/index.json` view.
///
/// Asset dependencies and folder membership references are fully checked only
/// by a complete [`Project`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetIndex {
    pub schema_version: u32,
    pub assets: Vec<AudioAsset>,
    #[serde(default)]
    pub folders: Vec<AssetFolder>,
}

/// Strictly decoded `project.json` header and fragment manifest.
///
/// This view validates the manifest's schema, IDs, locations, and strict JSON
/// shape, but it does not read composition-local files and is therefore not a
/// fully cross-reference-validated [`Project`]. Use [`crate::ProjectStore::load_project`]
/// when a validated canonical snapshot is required.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectManifest {
    pub schema_version: u32,
    pub id: ProjectId,
    pub name: String,
    pub root_composition_id: CompositionId,
    pub bpm: Bpm,
    pub time_signature: TimeSignature,
    pub sample_rate: SampleRate,
    pub settings: ProjectSettings,
    pub event_order: Vec<EventDataId>,
    pub composition_order: Vec<CompositionId>,
    pub track_order: Vec<TrackLocation>,
    pub automation_order: Vec<AutomationLocation>,
}

/// Strictly decoded files owned by one composition.
///
/// File paths, IDs, ownership, and the composition's track list are checked.
/// References to project-wide assets, events, or other compositions are not;
/// only a complete [`Project`] returned by [`crate::ProjectStore::load_project`]
/// has passed all core cross-reference validation.
#[derive(Clone, Debug, PartialEq)]
pub struct CompositionBundle {
    pub composition: Composition,
    pub tracks: Vec<Track>,
    pub automation: Vec<AutomationLane>,
}

pub(crate) fn encode(project: &Project) -> Result<Documents> {
    project.validate()?;
    let mut documents = Documents::new();
    documents.insert(
        ProjectPath::new("project.json")?,
        to_value(&ProjectDocument {
            schema_version: project.schema_version,
            id: project.id,
            name: project.name.clone(),
            root_composition_id: project.root_composition_id,
            bpm: project.bpm,
            time_signature: project.time_signature,
            sample_rate: project.sample_rate,
            settings: project.settings.clone(),
            event_order: project.event_data.iter().map(|value| value.id).collect(),
            composition_order: project.compositions.iter().map(|value| value.id).collect(),
            track_order: project
                .tracks
                .iter()
                .map(|value| TrackLocation {
                    composition_id: value.composition_id,
                    id: value.id,
                })
                .collect(),
            automation_order: project
                .automation
                .iter()
                .map(|value| AutomationLocation {
                    composition_id: value.composition_id,
                    id: value.id,
                })
                .collect(),
        })?,
    );
    documents.insert(
        ProjectPath::new("assets/index.json")?,
        encode_asset_index(project)?,
    );
    for event_data in &project.event_data {
        documents.insert(
            ProjectPath::new(format!("events/{}.json", event_data.id))?,
            versioned_value(event_data)?,
        );
    }
    for composition in &project.compositions {
        documents.insert(
            ProjectPath::new(format!("compositions/{}/composition.json", composition.id))?,
            versioned_value(composition)?,
        );
    }
    for track in &project.tracks {
        documents.insert(
            ProjectPath::new(format!(
                "compositions/{}/tracks/{}.json",
                track.composition_id, track.id
            ))?,
            versioned_value(track)?,
        );
    }
    for lane in &project.automation {
        documents.insert(
            ProjectPath::new(format!(
                "compositions/{}/automation/{}.json",
                lane.composition_id, lane.id
            ))?,
            versioned_value(lane)?,
        );
    }
    Ok(documents)
}

fn encode_asset_index(project: &Project) -> Result<Value> {
    #[derive(Serialize)]
    struct AssetIndexRef<'a> {
        schema_version: u32,
        assets: &'a [AudioAsset],
        folders: &'a [AssetFolder],
    }
    to_value(&AssetIndexRef {
        schema_version: ASSET_INDEX_SCHEMA_VERSION,
        assets: &project.assets,
        folders: &project.asset_folders,
    })
}

pub(crate) fn decode(documents: &Documents) -> Result<Project> {
    let project_path = ProjectPath::new("project.json")?;
    let assets_path = ProjectPath::new("assets/index.json")?;
    let header = decode_header(
        documents
            .get(&project_path)
            .ok_or_else(|| Error::InvalidTransaction("project.json is missing".into()))?,
    )?;
    let assets = decode_asset_index(
        documents
            .get(&assets_path)
            .ok_or_else(|| Error::InvalidTransaction("assets/index.json is missing".into()))?,
    )?;

    let mut compositions = Vec::new();
    let mut tracks = Vec::new();
    let mut automation = Vec::new();
    let mut event_data = Vec::new();
    for (path, document) in documents {
        match path_parts(path).as_slice() {
            ["project.json"] | ["assets", "index.json"] => {}
            ["events", file] => {
                let value: EventData = from_versioned(path, document)?;
                ensure_file_id(path, file, &value.id.to_string())?;
                event_data.push(value);
            }
            ["compositions", composition_id, "composition.json"] => {
                let value: Composition = from_versioned(path, document)?;
                ensure_path_id(path, composition_id, &value.id.to_string())?;
                compositions.push(value);
            }
            ["compositions", composition_id, "tracks", file] => {
                let value: Track = from_versioned(path, document)?;
                ensure_path_id(path, composition_id, &value.composition_id.to_string())?;
                ensure_file_id(path, file, &value.id.to_string())?;
                tracks.push(value);
            }
            ["compositions", composition_id, "automation", file] => {
                let value: AutomationLane = from_versioned(path, document)?;
                ensure_path_id(path, composition_id, &value.composition_id.to_string())?;
                ensure_file_id(path, file, &value.id.to_string())?;
                automation.push(value);
            }
            _ => {
                return Err(Error::InvalidTransaction(format!(
                    "unexpected canonical document {path}"
                )));
            }
        }
    }

    order_fragments(
        &header,
        &mut event_data,
        &mut compositions,
        &mut tracks,
        &mut automation,
    )?;
    let project = Project {
        schema_version: header.schema_version,
        id: header.id,
        name: header.name,
        root_composition_id: header.root_composition_id,
        bpm: header.bpm,
        time_signature: header.time_signature,
        sample_rate: header.sample_rate,
        settings: header.settings,
        assets: assets.assets,
        asset_folders: assets.folders,
        event_data,
        compositions,
        tracks,
        automation,
    };
    project.validate()?;
    Ok(project)
}

pub(crate) fn decode_manifest(project_document: &Value) -> Result<ProjectManifest> {
    let header = decode_header(project_document)?;
    Ok(ProjectManifest {
        schema_version: header.schema_version,
        id: header.id,
        name: header.name,
        root_composition_id: header.root_composition_id,
        bpm: header.bpm,
        time_signature: header.time_signature,
        sample_rate: header.sample_rate,
        settings: header.settings,
        event_order: header.event_order,
        composition_order: header.composition_order,
        track_order: header.track_order,
        automation_order: header.automation_order,
    })
}

pub(crate) fn decode_asset_index(document: &Value) -> Result<AssetIndex> {
    let path = ProjectPath::new("assets/index.json")?;
    let index: AssetIndex = from_value(&path, document)?;
    if !(SCHEMA_VERSION..=ASSET_INDEX_SCHEMA_VERSION).contains(&index.schema_version) {
        return Err(Error::UnsupportedSchema {
            found: u64::from(index.schema_version),
            expected: ASSET_INDEX_SCHEMA_VERSION,
        });
    }
    unique(index.assets.iter().map(|value| value.id), "asset")?;
    Ok(index)
}

pub(crate) fn composition_paths(
    manifest: &ProjectManifest,
    id: CompositionId,
) -> Result<Vec<ProjectPath>> {
    if !manifest.composition_order.contains(&id) {
        return Err(Error::InvalidTransaction(format!(
            "project manifest does not contain composition {id}"
        )));
    }
    let mut paths = vec![ProjectPath::new(format!(
        "compositions/{id}/composition.json"
    ))?];
    for location in manifest
        .track_order
        .iter()
        .filter(|value| value.composition_id == id)
    {
        paths.push(ProjectPath::new(format!(
            "compositions/{id}/tracks/{}.json",
            location.id
        ))?);
    }
    for location in manifest
        .automation_order
        .iter()
        .filter(|value| value.composition_id == id)
    {
        paths.push(ProjectPath::new(format!(
            "compositions/{id}/automation/{}.json",
            location.id
        ))?);
    }
    Ok(paths)
}

pub(crate) fn decode_composition_bundle(
    manifest: &ProjectManifest,
    id: CompositionId,
    documents: &Documents,
) -> Result<CompositionBundle> {
    let composition_path = ProjectPath::new(format!("compositions/{id}/composition.json"))?;
    let composition: Composition = from_versioned(
        &composition_path,
        documents
            .get(&composition_path)
            .ok_or_else(|| Error::InvalidTransaction(format!("{composition_path} is missing")))?,
    )?;
    ensure_path_id(
        &composition_path,
        &id.to_string(),
        &composition.id.to_string(),
    )?;

    let mut tracks = Vec::new();
    for location in manifest
        .track_order
        .iter()
        .filter(|value| value.composition_id == id)
    {
        let path = ProjectPath::new(format!("compositions/{id}/tracks/{}.json", location.id))?;
        let track: Track = from_versioned(
            &path,
            documents
                .get(&path)
                .ok_or_else(|| Error::InvalidTransaction(format!("{path} is missing")))?,
        )?;
        ensure_path_id(&path, &id.to_string(), &track.composition_id.to_string())?;
        ensure_file_id(
            &path,
            &format!("{}.json", location.id),
            &track.id.to_string(),
        )?;
        tracks.push(track);
    }
    order_by(
        &mut tracks,
        composition.track_ids.iter().copied(),
        |value| value.id,
        "composition track",
    )?;

    let mut automation = Vec::new();
    let automation_order = manifest
        .automation_order
        .iter()
        .filter(|value| value.composition_id == id)
        .map(|value| value.id)
        .collect::<Vec<_>>();
    for lane_id in &automation_order {
        let path = ProjectPath::new(format!("compositions/{id}/automation/{lane_id}.json"))?;
        let lane: AutomationLane = from_versioned(
            &path,
            documents
                .get(&path)
                .ok_or_else(|| Error::InvalidTransaction(format!("{path} is missing")))?,
        )?;
        ensure_path_id(&path, &id.to_string(), &lane.composition_id.to_string())?;
        ensure_file_id(&path, &format!("{lane_id}.json"), &lane.id.to_string())?;
        automation.push(lane);
    }
    order_by(
        &mut automation,
        automation_order.iter().copied(),
        |value| value.id,
        "composition automation lane",
    )?;
    Ok(CompositionBundle {
        composition,
        tracks,
        automation,
    })
}

pub(crate) fn decode_event_data(path: &ProjectPath, document: &Value) -> Result<EventData> {
    let value: EventData = from_versioned(path, document)?;
    let parts = path_parts(path);
    let ["events", file] = parts.as_slice() else {
        return Err(Error::InvalidPath(path.to_string()));
    };
    ensure_file_id(path, file, &value.id.to_string())?;
    Ok(value)
}

fn order_fragments(
    header: &ProjectDocument,
    event_data: &mut Vec<EventData>,
    compositions: &mut Vec<Composition>,
    tracks: &mut Vec<Track>,
    automation: &mut Vec<AutomationLane>,
) -> Result<()> {
    order_by(
        event_data,
        header.event_order.iter().copied(),
        |value| value.id,
        "event data",
    )?;
    order_by(
        compositions,
        header.composition_order.iter().copied(),
        |value| value.id,
        "composition",
    )?;
    order_by(
        tracks,
        header.track_order.iter().map(|value| value.id),
        |value| value.id,
        "track",
    )?;
    order_by(
        automation,
        header.automation_order.iter().map(|value| value.id),
        |value| value.id,
        "automation lane",
    )?;
    for (track, location) in tracks.iter().zip(&header.track_order) {
        ensure_manifest_owner(
            "track",
            track.id,
            track.composition_id,
            location.composition_id,
        )?;
    }
    for (lane, location) in automation.iter().zip(&header.automation_order) {
        ensure_manifest_owner(
            "automation lane",
            lane.id,
            lane.composition_id,
            location.composition_id,
        )?;
    }
    Ok(())
}

fn ensure_manifest_owner(
    entity: &str,
    id: impl std::fmt::Display,
    actual: CompositionId,
    listed: CompositionId,
) -> Result<()> {
    if actual != listed {
        return Err(Error::InvalidTransaction(format!(
            "project manifest places {entity} {id} in composition {listed}, but its fragment belongs to {actual}"
        )));
    }
    Ok(())
}

fn decode_header(project_document: &Value) -> Result<ProjectDocument> {
    let path = ProjectPath::new("project.json")?;
    let header: ProjectDocument = from_value(&path, project_document)?;
    check_schema(header.schema_version.into())?;
    if header.name.trim().is_empty() {
        return Err(Error::InvalidTransaction(
            "project name must not be empty".into(),
        ));
    }
    let compositions = unique(header.composition_order.iter().copied(), "composition")?;
    if !compositions.contains(&header.root_composition_id) {
        return Err(Error::InvalidTransaction(
            "root composition is missing from project manifest".into(),
        ));
    }
    unique(header.event_order.iter().copied(), "event data")?;
    unique(header.track_order.iter().map(|value| value.id), "track")?;
    unique(
        header.automation_order.iter().map(|value| value.id),
        "automation lane",
    )?;
    for owner in header
        .track_order
        .iter()
        .map(|value| value.composition_id)
        .chain(
            header
                .automation_order
                .iter()
                .map(|value| value.composition_id),
        )
    {
        if !compositions.contains(&owner) {
            return Err(Error::InvalidTransaction(format!(
                "project manifest location references missing composition {owner}"
            )));
        }
    }
    Ok(header)
}

fn unique<T>(values: impl IntoIterator<Item = T>, entity: &str) -> Result<BTreeSet<T>>
where
    T: Copy + Ord + std::fmt::Display,
{
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(value) {
            return Err(Error::InvalidTransaction(format!(
                "project manifest contains duplicate {entity} {value}"
            )));
        }
    }
    Ok(seen)
}

fn order_by<T, Id>(
    values: &mut Vec<T>,
    order: impl ExactSizeIterator<Item = Id>,
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
    // Keep large fragment payloads out of the tree nodes. Repeated IDs retain
    // their last position, matching the original map's replacement behavior.
    let mut source = std::mem::take(values)
        .into_iter()
        .map(Some)
        .collect::<Vec<_>>();
    let mut by_id = source
        .iter()
        .enumerate()
        .map(|(index, value)| (id(value.as_ref().expect("unmoved fragment")), index))
        .collect::<BTreeMap<_, _>>();
    let mut sorted = Vec::with_capacity(order.len());
    for expected in order {
        let index = by_id.remove(&expected).ok_or_else(|| {
            Error::InvalidTransaction(format!("{entity} order references missing {expected}"))
        })?;
        sorted.push(source[index].take().expect("fragment is moved only once"));
    }
    *values = sorted;
    Ok(())
}

fn versioned_value<T: Serialize>(value: &T) -> Result<Value> {
    let mut value = to_value(value)?;
    value
        .as_object_mut()
        .ok_or_else(|| Error::InvalidTransaction("canonical document must be an object".into()))?
        .insert("schema_version".into(), Value::from(SCHEMA_VERSION));
    Ok(value)
}

fn from_versioned<T: DeserializeOwned>(path: &ProjectPath, value: &Value) -> Result<T> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::InvalidTransaction(format!("{path} must be an object")))?;
    let schema = object
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or(Error::MissingSchemaVersion)?;
    check_schema(schema)?;
    // The envelope field is not part of the strict core model. Borrow its
    // remaining entries so large note/automation trees are never cloned.
    let fields = object
        .iter()
        .filter(|(key, _)| key.as_str() != "schema_version")
        .map(|(key, value)| (key.as_str(), value));
    T::deserialize(serde::de::value::MapDeserializer::new(fields)).map_err(|source| Error::Json {
        path: PathBuf::from(path.as_str()),
        source,
    })
}

fn to_value<T: Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(|source| Error::Json {
        path: PathBuf::from("<project>"),
        source,
    })
}

fn from_value<T: DeserializeOwned>(path: &ProjectPath, value: &Value) -> Result<T> {
    T::deserialize(value).map_err(|source| Error::Json {
        path: PathBuf::from(path.as_str()),
        source,
    })
}

fn path_parts(path: &ProjectPath) -> Vec<&str> {
    path.as_str().split('/').collect()
}

fn ensure_path_id(path: &ProjectPath, found: &str, expected: &str) -> Result<()> {
    if found == expected {
        Ok(())
    } else {
        Err(Error::InvalidTransaction(format!(
            "{path} directory id {found} does not match document id {expected}"
        )))
    }
}

fn ensure_file_id(path: &ProjectPath, file: &str, expected: &str) -> Result<()> {
    if file.strip_suffix(".json") == Some(expected) {
        Ok(())
    } else {
        Err(Error::InvalidTransaction(format!(
            "{path} filename does not match document id {expected}"
        )))
    }
}

pub(crate) fn check_schema(found: u64) -> Result<()> {
    if found == u64::from(SCHEMA_VERSION) {
        Ok(())
    } else {
        Err(Error::UnsupportedSchema {
            found,
            expected: SCHEMA_VERSION,
        })
    }
}

#[cfg(test)]
mod performance;

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> Project {
        Project::new(
            "Format test",
            Bpm::new(120.0).unwrap(),
            SampleRate::new(48_000).unwrap(),
        )
    }

    #[test]
    fn versioned_decode_preserves_input_and_rejects_unknown_model_fields() {
        let mut events = EventData::new("Notes");
        events.events.push(gaw_core::Event::Note(
            gaw_core::NoteEvent::new(
                gaw_core::Beats::new(0.0).unwrap(),
                gaw_core::Beats::new(1.0).unwrap(),
                60,
                100,
            )
            .unwrap(),
        ));
        let path = ProjectPath::new(format!("events/{}.json", events.id)).unwrap();
        let document = versioned_value(&events).unwrap();
        let original = document.clone();
        assert_eq!(decode_event_data(&path, &document).unwrap(), events);
        assert_eq!(document, original);

        for nested in [false, true] {
            let mut malformed = document.clone();
            if nested {
                malformed["events"][0]["data"]["unexpected"] = Value::Bool(true);
            } else {
                malformed["unexpected"] = Value::Bool(true);
            }
            assert!(matches!(
                decode_event_data(&path, &malformed),
                Err(Error::Json { .. })
            ));
        }
        for schema in [Value::Null, Value::from("1"), Value::from(1.5)] {
            let mut malformed = document.clone();
            malformed["schema_version"] = schema;
            assert!(matches!(
                decode_event_data(&path, &malformed),
                Err(Error::MissingSchemaVersion)
            ));
        }
        let mut unsupported = document;
        unsupported["schema_version"] = Value::from(SCHEMA_VERSION + 1);
        assert!(matches!(
            decode_event_data(&path, &unsupported),
            Err(Error::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn legacy_project_document_defaults_to_four_four_with_metronome_off() {
        let mut documents = encode(&project()).unwrap();
        let document = documents
            .get_mut(&ProjectPath::new("project.json").unwrap())
            .unwrap();
        let object = document.as_object_mut().unwrap();
        object.remove("time_signature");
        object
            .get_mut("settings")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("metronome_enabled");
        object
            .get_mut("settings")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("master_volume");

        let decoded = decode(&documents).unwrap();
        assert_eq!(decoded.time_signature, TimeSignature::default());
        assert!(!decoded.settings.metronome_enabled);
        assert!(decoded.settings.master_volume.value().abs() < f64::EPSILON);
    }

    #[test]
    fn organization_metadata_round_trips_and_defaults_for_legacy_documents() {
        let mut project = project();
        let event_data = EventData::new("Pattern");
        project.event_data.push(event_data.clone());
        project.asset_folders.push(AssetFolder {
            id: gaw_core::AssetFolderId::new(),
            name: "Patterns".into(),
            asset_ids: vec![],
            event_data_ids: vec![event_data.id],
        });
        let track = Track::audio(project.root_composition_id, "Drums");
        project.compositions[0].track_ids.push(track.id);
        project.compositions[0]
            .track_groups
            .push(gaw_core::TrackGroup {
                id: gaw_core::TrackGroupId::new(),
                name: "Rhythm".into(),
                track_ids: vec![track.id],
                collapsed: true,
            });
        project.tracks.push(track);

        let documents = encode(&project).unwrap();
        assert_eq!(decode(&documents).unwrap(), project);
        assert_eq!(
            documents[&ProjectPath::new("assets/index.json").unwrap()]["folders"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let legacy_project = Project::new(
            "Legacy",
            Bpm::new(120.0).unwrap(),
            SampleRate::new(48_000).unwrap(),
        );
        let composition_path = ProjectPath::new(format!(
            "compositions/{}/composition.json",
            legacy_project.root_composition_id
        ))
        .unwrap();
        let mut legacy = encode(&legacy_project).unwrap();
        legacy
            .get_mut(&ProjectPath::new("assets/index.json").unwrap())
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("folders");
        legacy
            .get_mut(&composition_path)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("track_groups");
        let decoded = decode(&legacy).unwrap();
        assert!(decoded.asset_folders.is_empty());
        assert!(decoded.compositions[0].track_groups.is_empty());
    }

    #[test]
    fn project_document_rejects_invalid_time_signatures() {
        for time_signature in [
            serde_json::json!({"numerator": 0, "denominator": 4}),
            serde_json::json!({"numerator": 4, "denominator": 3}),
            serde_json::json!({"numerator": 4, "denominator": 64}),
        ] {
            let mut documents = encode(&project()).unwrap();
            documents
                .get_mut(&ProjectPath::new("project.json").unwrap())
                .unwrap()["time_signature"] = time_signature;
            assert!(decode(&documents).is_err());
        }
    }

    #[test]
    fn bar_timeline_gaps_round_trip_and_default_for_legacy_compositions() {
        let mut project = project();
        project.compositions[0]
            .bar_timeline_gaps
            .push(gaw_core::BarTimelineGap {
                start: gaw_core::Beats::new(4.0).unwrap(),
                duration: gaw_core::Beats::new(2.0).unwrap(),
            });
        let documents = encode(&project).unwrap();
        assert_eq!(decode(&documents).unwrap(), project);

        let composition_path = ProjectPath::new(format!(
            "compositions/{}/composition.json",
            project.root_composition_id
        ))
        .unwrap();
        let mut legacy = documents;
        legacy
            .get_mut(&composition_path)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("bar_timeline_gaps");
        assert!(
            decode(&legacy).unwrap().compositions[0]
                .bar_timeline_gaps
                .is_empty()
        );
    }

    #[test]
    fn event_clip_effects_round_trip_in_track_json_and_default_for_legacy_files() {
        let mut project = project();
        let events = EventData::new("Notes");
        let mut clip = gaw_core::EventClip::new(
            events.id,
            gaw_core::Beats::new(0.0).unwrap(),
            gaw_core::Beats::new(1.0).unwrap(),
        );
        clip.effects.push(gaw_core::Processor::new(
            gaw_core::ProcessorId::new("event-bitcrusher").unwrap(),
            gaw_core::ProcessorKind::Bitcrusher(gaw_core::BitcrusherParameters::default()),
        ));
        let mut track = Track::event(
            project.root_composition_id,
            "Sampler",
            gaw_core::Instrument::sampler("Sampler", gaw_core::Sampler::new(8).unwrap()),
        );
        track.clips.push(gaw_core::Clip::Event(clip));
        let path = ProjectPath::new(format!(
            "compositions/{}/tracks/{}.json",
            project.root_composition_id, track.id
        ))
        .unwrap();
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        project.event_data.push(events);
        let mut documents = encode(&project).unwrap();
        assert_eq!(decode(&documents).unwrap(), project);
        documents.get_mut(&path).unwrap()["clips"][0]["data"]
            .as_object_mut()
            .unwrap()
            .remove("effects");
        let decoded = decode(&documents).unwrap();
        let gaw_core::Clip::Event(clip) = &decoded.tracks[0].clips[0] else {
            unreachable!()
        };
        assert!(clip.effects.is_empty());
    }

    #[test]
    fn asset_folders_round_trip_and_default_for_legacy_indexes() {
        let mut project = project();
        project.asset_folders.push(AssetFolder {
            id: gaw_core::AssetFolderId::new(),
            name: "Stem splits".into(),
            asset_ids: vec![],
            event_data_ids: vec![],
        });
        let documents = encode(&project).unwrap();
        assert_eq!(decode(&documents).unwrap(), project);

        let mut legacy = documents;
        let legacy_index = legacy
            .get_mut(&ProjectPath::new("assets/index.json").unwrap())
            .unwrap()
            .as_object_mut()
            .unwrap();
        legacy_index.insert("schema_version".into(), Value::from(SCHEMA_VERSION));
        legacy_index.remove("folders");
        assert!(decode(&legacy).unwrap().asset_folders.is_empty());
    }
}
