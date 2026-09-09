//! Canonical JSON schemas and agent-facing processor discovery.

use schemars::{JsonSchema, Schema, generate::SchemaSettings};
use serde_json::{Value, json};

use crate::{
    AnalyzerMeasurement, Command, EffectPreset, ParameterDescriptor, ParameterValueType, Processor,
    ProcessorKind, Project, SamplerPreset, Transaction,
};

/// Generates a self-contained Draft 2020-12 JSON Schema for a canonical type.
pub fn json_schema_for<T: JsonSchema>() -> Schema {
    SchemaSettings::draft2020_12()
        .into_generator()
        .into_root_schema_for::<T>()
}

/// Generates the schema for a complete project snapshot.
pub fn project_json_schema() -> Schema {
    processor_bearing_schema::<Project>()
}

/// Generates the schema for an individual typed edit command.
pub fn command_json_schema() -> Schema {
    processor_bearing_schema::<Command>()
}

/// Generates the schema for an atomic command transaction.
pub fn transaction_json_schema() -> Schema {
    processor_bearing_schema::<Transaction>()
}

/// Generates the schema for the complete built-in processor catalog.
pub fn processor_json_schema() -> Schema {
    processor_bearing_schema::<Processor>()
}

/// Generates the schema for a portable sampler preset document.
pub fn sampler_preset_json_schema() -> Schema {
    json_schema_for::<SamplerPreset>()
}

/// Generates the schema for a portable effect preset document.
pub fn effect_preset_json_schema() -> Schema {
    processor_bearing_schema::<EffectPreset>()
}

/// Generates the schema for ephemeral structured analyzer results.
pub fn analyzer_measurement_json_schema() -> Schema {
    json_schema_for::<AnalyzerMeasurement>()
}

fn processor_bearing_schema<T: JsonSchema>() -> Schema {
    let mut schema = json_schema_for::<T>();
    schema.insert("x-gaw-processor-catalog".into(), processor_catalog_json());
    schema
}

/// Machine-readable processor defaults, parameter bounds, and cross-field constraints.
///
/// Included in processor-bearing schemas so CLI and library clients discover the
/// same contract alongside the canonical model.
pub fn processor_catalog_json() -> Value {
    let processors = ProcessorKind::catalog_defaults()
        .iter()
        .map(processor_catalog_entry)
        .collect::<Vec<_>>();
    json!({
        "schema_version": 1,
        "description": "Authoritative canonical-validation contract for processor parameters. Numeric bounds are inclusive; unit_ranges apply to tagged time/rate values; constraints apply after per-parameter validation.",
        "processors": processors,
    })
}

fn processor_catalog_entry(kind: &ProcessorKind) -> Value {
    let parameters = kind
        .parameter_descriptors()
        .iter()
        .map(|descriptor| processor_parameter(kind, descriptor))
        .collect::<Vec<_>>();
    json!({
        "type": kind.type_id(),
        "analyzer": kind.is_analyzer(),
        "parameters": parameters,
        "constraints": processor_constraints(kind),
    })
}

fn processor_parameter(kind: &ProcessorKind, descriptor: &ParameterDescriptor) -> Value {
    let default: Value = serde_json::from_str(descriptor.default_json)
        .expect("built-in processor defaults must be valid JSON");
    let mut parameter = serde_json::Map::from_iter([
        ("id".into(), Value::from(descriptor.id)),
        ("value_type".into(), json!(descriptor.value_type)),
        ("unit".into(), json!(descriptor.unit)),
        ("default".into(), default),
        ("automation".into(), json!(descriptor.automation)),
        ("display_hint".into(), json!(descriptor.display_hint)),
    ]);

    match descriptor.value_type {
        ParameterValueType::Number | ParameterValueType::Integer => {
            let range = descriptor
                .range
                .expect("numeric catalog parameters must declare a range");
            parameter.insert("minimum".into(), Value::from(range.minimum));
            parameter.insert("maximum".into(), Value::from(range.maximum));
            if matches!(kind, ProcessorKind::BeatRepeat(_)) && descriptor.id == "seed" {
                parameter.insert("maximum".into(), Value::from(u64::MAX));
            }
        }
        ParameterValueType::Choice => {
            parameter.insert("enum".into(), json!(descriptor.choices));
        }
        ParameterValueType::Time => {
            let minimum = if matches!(kind, ProcessorKind::Delay(_)) && descriptor.id == "time" {
                f64::EPSILON
            } else {
                0.0
            };
            parameter.insert(
                "unit_ranges".into(),
                json!({
                    "beats": { "minimum": minimum, "maximum": 64.0 },
                    "seconds": { "minimum": minimum, "maximum": 64.0 },
                }),
            );
        }
        ParameterValueType::Rate => {
            parameter.insert(
                "unit_ranges".into(),
                json!({
                    "hertz": { "minimum": 0.01, "maximum": 40.0 },
                    "beats": { "minimum": 1.0 / 64.0, "maximum": 64.0 },
                }),
            );
        }
        ParameterValueType::List => match kind {
            ProcessorKind::ParametricEq(_) if descriptor.id == "bands" => {
                parameter.insert("minItems".into(), Value::from(0));
                parameter.insert("maxItems".into(), Value::from(8));
            }
            ProcessorKind::RhythmicGate(_) if descriptor.id == "steps" => {
                parameter.insert("minItems".into(), Value::from(1));
                parameter.insert("maxItems".into(), Value::from(64));
            }
            _ => {}
        },
        ParameterValueType::Boolean => {}
    }
    Value::Object(parameter)
}

fn processor_constraints(kind: &ProcessorKind) -> Value {
    let ordered = |lower, upper| {
        json!([{
            "kind": "less_than",
            "lower": lower,
            "upper": upper,
        }])
    };
    match kind {
        ProcessorKind::Delay(_) | ProcessorKind::Reverb(_) => ordered("low_cut_hz", "high_cut_hz"),
        ProcessorKind::Spectrum(_) | ProcessorKind::Tuner(_) => ordered("minimum_hz", "maximum_hz"),
        ProcessorKind::Gain(_)
        | ProcessorKind::StereoTool(_)
        | ProcessorKind::Filter(_)
        | ProcessorKind::ParametricEq(_)
        | ProcessorKind::Compressor(_)
        | ProcessorKind::Limiter(_)
        | ProcessorKind::Gate(_)
        | ProcessorKind::Expander(_)
        | ProcessorKind::TransientShaper(_)
        | ProcessorKind::Saturator(_)
        | ProcessorKind::Clipper(_)
        | ProcessorKind::Bitcrusher(_)
        | ProcessorKind::Chorus(_)
        | ProcessorKind::Flanger(_)
        | ProcessorKind::Phaser(_)
        | ProcessorKind::TremoloAutopan(_)
        | ProcessorKind::PitchShift(_)
        | ProcessorKind::RhythmicGate(_)
        | ProcessorKind::BeatRepeat(_)
        | ProcessorKind::LevelMeter(_)
        | ProcessorKind::LoudnessMeter(_)
        | ProcessorKind::Oscilloscope(_)
        | ProcessorKind::StereoMeter(_) => json!([]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processor_schemas_share_the_core_discovery_contract() {
        let catalog = processor_catalog_json();
        for schema in [
            project_json_schema(),
            command_json_schema(),
            transaction_json_schema(),
            processor_json_schema(),
            effect_preset_json_schema(),
        ] {
            assert_eq!(schema.get("x-gaw-processor-catalog"), Some(&catalog));
        }
        for schema in [
            sampler_preset_json_schema(),
            analyzer_measurement_json_schema(),
        ] {
            assert!(schema.get("x-gaw-processor-catalog").is_none());
        }
    }
}
