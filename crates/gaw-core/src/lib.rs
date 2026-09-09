//! Canonical, GUI-independent GAW project model and edit engine.
//!
//! All persisted types are strict JSON values with generated JSON Schema. Editing
//! is performed through [`Command`] and [`Transaction`], so human and agent
//! clients share validation, atomicity, and undo behavior.

#![forbid(unsafe_code)]

pub mod command;
pub mod model;
pub mod processors;
pub mod schema;

pub use command::*;
pub use model::*;
pub use processors::*;
pub use schema::*;

/// Current on-disk project schema version.
pub const SCHEMA_VERSION: u32 = 1;

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
