use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use gaw_core::{
    AudioAsset, AutomationLane, AutomationLaneId, Bpm, Composition, CompositionId, EventData,
    EventDataId, Project, ProjectId, ProjectSettings, SampleRate, TimeSignature, Track, TrackId,
    Validate,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::{Error, ProjectPath, Result, SCHEMA_VERSION};

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
/// Asset dependency references are fully checked only by a complete [`Project`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetIndex {
    pub schema_version: u32,
    pub assets: Vec<AudioAsset>,
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
        to_value(&AssetIndex {
            schema_version: project.schema_version,
            assets: project.assets.clone(),
        })?,
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

    order_by(
        &mut event_data,
        &header.event_order,
        |value| value.id,
        "event data",
    )?;
    order_by(
        &mut compositions,
        &header.composition_order,
        |value| value.id,
        "composition",
    )?;
    order_by(
        &mut tracks,
        &header
            .track_order
            .iter()
            .map(|value| value.id)
            .collect::<Vec<_>>(),
        |value| value.id,
        "track",
    )?;
    order_by(
        &mut automation,
        &header
            .automation_order
            .iter()
            .map(|value| value.id)
            .collect::<Vec<_>>(),
        |value| value.id,
        "automation lane",
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
    check_schema(index.schema_version.into())?;
    let ids = index
        .assets
        .iter()
        .map(|value| value.id)
        .collect::<Vec<_>>();
    unique(&ids, "asset")?;
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
        &composition.track_ids,
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
        &automation_order,
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

fn decode_header(project_document: &Value) -> Result<ProjectDocument> {
    let path = ProjectPath::new("project.json")?;
    let header: ProjectDocument = from_value(&path, project_document)?;
    check_schema(header.schema_version.into())?;
    if header.name.trim().is_empty() {
        return Err(Error::InvalidTransaction(
            "project name must not be empty".into(),
        ));
    }
    let compositions = unique(&header.composition_order, "composition")?;
    if !compositions.contains(&header.root_composition_id) {
        return Err(Error::InvalidTransaction(
            "root composition is missing from project manifest".into(),
        ));
    }
    unique(&header.event_order, "event data")?;
    let track_ids = header
        .track_order
        .iter()
        .map(|value| value.id)
        .collect::<Vec<_>>();
    unique(&track_ids, "track")?;
    let automation_ids = header
        .automation_order
        .iter()
        .map(|value| value.id)
        .collect::<Vec<_>>();
    unique(&automation_ids, "automation lane")?;
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

fn unique<T>(values: &[T], entity: &str) -> Result<BTreeSet<T>>
where
    T: Copy + Ord + std::fmt::Display,
{
    let mut seen = BTreeSet::new();
    for value in values {
        if !seen.insert(*value) {
            return Err(Error::InvalidTransaction(format!(
                "project manifest contains duplicate {entity} {value}"
            )));
        }
    }
    Ok(seen)
}

fn order_by<T, Id>(
    values: &mut Vec<T>,
    order: &[Id],
    id: impl Fn(&T) -> Id,
    entity: &str,
) -> Result<()>
where
    Id: Copy + Eq + std::fmt::Display,
{
    if values.len() != order.len() {
        return Err(Error::InvalidTransaction(format!(
            "{entity} order does not match stored documents"
        )));
    }
    let mut sorted = Vec::with_capacity(values.len());
    for expected in order {
        let index = values
            .iter()
            .position(|value| id(value) == *expected)
            .ok_or_else(|| {
                Error::InvalidTransaction(format!("{entity} order references missing {expected}"))
            })?;
        sorted.push(values.remove(index));
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
    let mut value = value.clone();
    let object = value
        .as_object_mut()
        .ok_or_else(|| Error::InvalidTransaction(format!("{path} must be an object")))?;
    let schema = object
        .remove("schema_version")
        .and_then(|value| value.as_u64())
        .ok_or(Error::MissingSchemaVersion)?;
    check_schema(schema)?;
    from_value(path, &value)
}

fn to_value<T: Serialize>(value: &T) -> Result<Value> {
    serde_json::to_value(value).map_err(|source| Error::Json {
        path: PathBuf::from("<project>"),
        source,
    })
}

fn from_value<T: DeserializeOwned>(path: &ProjectPath, value: &Value) -> Result<T> {
    serde_json::from_value(value.clone()).map_err(|source| Error::Json {
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

        let decoded = decode(&documents).unwrap();
        assert_eq!(decoded.time_signature, TimeSignature::default());
        assert!(!decoded.settings.metronome_enabled);
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
}
