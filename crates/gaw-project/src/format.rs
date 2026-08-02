use std::{collections::BTreeMap, path::PathBuf};

use gaw_core::{
    AudioAsset, AutomationLane, AutomationLaneId, Bpm, Composition, CompositionId, EventData,
    Project, ProjectId, ProjectSettings, SampleRate, Track, TrackId, Validate,
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
    sample_rate: SampleRate,
    settings: ProjectSettings,
    event_data: Vec<EventData>,
    composition_order: Vec<CompositionId>,
    track_order: Vec<TrackLocation>,
    automation_order: Vec<AutomationLocation>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TrackLocation {
    composition_id: CompositionId,
    id: TrackId,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AutomationLocation {
    composition_id: CompositionId,
    id: AutomationLaneId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssetIndex {
    schema_version: u32,
    assets: Vec<AudioAsset>,
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
            sample_rate: project.sample_rate,
            settings: project.settings.clone(),
            event_data: project.event_data.clone(),
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
    let header: ProjectDocument = from_value(
        &project_path,
        documents
            .get(&project_path)
            .ok_or_else(|| Error::InvalidTransaction("project.json is missing".into()))?,
    )?;
    check_schema(header.schema_version.into())?;
    let assets: AssetIndex = from_value(
        &assets_path,
        documents
            .get(&assets_path)
            .ok_or_else(|| Error::InvalidTransaction("assets/index.json is missing".into()))?,
    )?;
    check_schema(assets.schema_version.into())?;

    let mut compositions = Vec::new();
    let mut tracks = Vec::new();
    let mut automation = Vec::new();
    for (path, document) in documents {
        match path_parts(path).as_slice() {
            ["project.json"] | ["assets", "index.json"] => {}
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
        sample_rate: header.sample_rate,
        settings: header.settings,
        assets: assets.assets,
        event_data: header.event_data,
        compositions,
        tracks,
        automation,
    };
    project.validate()?;
    Ok(project)
}

/// Resolves the exact fragment set from the project manifest without walking the tree.
pub(crate) fn canonical_paths(project_document: &Value) -> Result<Vec<ProjectPath>> {
    let project_path = ProjectPath::new("project.json")?;
    let header: ProjectDocument = from_value(&project_path, project_document)?;
    check_schema(header.schema_version.into())?;
    let mut paths = vec![project_path, ProjectPath::new("assets/index.json")?];
    for id in header.composition_order {
        paths.push(ProjectPath::new(format!(
            "compositions/{id}/composition.json"
        ))?);
    }
    for location in header.track_order {
        paths.push(ProjectPath::new(format!(
            "compositions/{}/tracks/{}.json",
            location.composition_id, location.id
        ))?);
    }
    for location in header.automation_order {
        paths.push(ProjectPath::new(format!(
            "compositions/{}/automation/{}.json",
            location.composition_id, location.id
        ))?);
    }
    Ok(paths)
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
