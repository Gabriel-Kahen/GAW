//! Canonical, GUI-independent GAW project model and edit engine.
//!
//! All persisted types are strict JSON values with generated JSON Schema. Editing
//! is performed through [`Command`] and [`Transaction`], so human and agent
//! clients share validation, atomicity, and undo behavior.

#![forbid(unsafe_code)]

pub mod command;
pub mod model;
pub mod processors;

pub use command::*;
pub use model::*;
pub use processors::*;
use schemars::{JsonSchema, Schema, generate::SchemaSettings};

/// Current on-disk project schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Generates a self-contained Draft 2020-12 JSON Schema for a canonical type.
pub fn json_schema_for<T: JsonSchema>() -> Schema {
    SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<T>()
}

/// Generates the schema for a complete project snapshot.
pub fn project_json_schema() -> Schema {
    json_schema_for::<Project>()
}

/// Generates the schema for an individual typed edit command.
pub fn command_json_schema() -> Schema {
    json_schema_for::<Command>()
}

/// Generates the schema for an atomic command transaction.
pub fn transaction_json_schema() -> Schema {
    json_schema_for::<Transaction>()
}

/// Generates the schema for the complete built-in processor catalog.
pub fn processor_json_schema() -> Schema {
    json_schema_for::<Processor>()
}

/// Generates the schema for a portable sampler preset document.
pub fn sampler_preset_json_schema() -> Schema {
    json_schema_for::<SamplerPreset>()
}

/// Generates the schema for a portable effect preset document.
pub fn effect_preset_json_schema() -> Schema {
    json_schema_for::<EffectPreset>()
}

/// Generates the schema for ephemeral structured analyzer results.
pub fn analyzer_measurement_json_schema() -> Schema {
    json_schema_for::<AnalyzerMeasurement>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_schemas_are_serializable_draft_2020_12() {
        for schema in [
            project_json_schema(),
            command_json_schema(),
            transaction_json_schema(),
            processor_json_schema(),
            sampler_preset_json_schema(),
            effect_preset_json_schema(),
            analyzer_measurement_json_schema(),
        ] {
            let value = serde_json::to_value(schema).expect("schema is JSON");
            assert_eq!(
                value["$schema"],
                "https://json-schema.org/draft/2020-12/schema"
            );
            assert!(value.get("$defs").is_some());
        }

        let project = serde_json::to_value(project_json_schema()).unwrap();
        assert_eq!(project["$defs"]["Ratio"]["minimum"], 0.0);
        assert_eq!(project["$defs"]["Ratio"]["maximum"], 1.0);
        assert_eq!(project["$defs"]["ContentHash"]["pattern"], "^[0-9a-f]{64}$");
    }
}
