//! Typed, introspectable processor parameters.

use serde::{Deserialize, Serialize};

/// The storage and validation type of a processor parameter.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParameterKind {
    Float { min: f32, max: f32 },
    Integer { min: i32, max: i32 },
    Boolean,
    Choice(&'static [&'static str]),
}

/// Display and interchange unit for a parameter value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParameterUnit {
    None,
    Decibels,
    Hertz,
    Milliseconds,
    Seconds,
    Beats,
    Ratio,
    Percent,
    Semitones,
    Cents,
    Samples,
}

/// A typed parameter value. Choice values are indices into the descriptor's choices.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ParameterValue {
    Float(f32),
    Integer(i32),
    Bool(bool),
    Choice(u32),
}

impl ParameterValue {
    #[must_use]
    pub const fn as_float(self) -> Option<f32> {
        match self {
            Self::Float(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_integer(self) -> Option<i32> {
        match self {
            Self::Integer(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_bool(self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(value),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_choice(self) -> Option<u32> {
        match self {
            Self::Choice(value) => Some(value),
            _ => None,
        }
    }
}

/// Static metadata used by editors, automation, and agents.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ParameterDescriptor {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: ParameterKind,
    pub unit: ParameterUnit,
    pub default: ParameterValue,
    pub automatable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_hint: Option<&'static str>,
}

impl ParameterDescriptor {
    #[must_use]
    pub fn accepts(&self, value: ParameterValue) -> bool {
        match (self.kind, value) {
            (ParameterKind::Float { min, max }, ParameterValue::Float(value)) => {
                value.is_finite() && (min..=max).contains(&value)
            }
            (ParameterKind::Integer { min, max }, ParameterValue::Integer(value)) => {
                (min..=max).contains(&value)
            }
            (ParameterKind::Boolean, ParameterValue::Bool(_)) => true,
            (ParameterKind::Choice(choices), ParameterValue::Choice(value)) => {
                usize::try_from(value).is_ok_and(|value| value < choices.len())
            }
            _ => false,
        }
    }
}

/// A sample-accurate change within the current process block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParameterEvent {
    pub sample_offset: usize,
    pub id: String,
    pub value: ParameterValue,
}

impl ParameterEvent {
    #[must_use]
    pub fn new(sample_offset: usize, id: impl Into<String>, value: ParameterValue) -> Self {
        Self {
            sample_offset,
            id: id.into(),
            value,
        }
    }
}

#[must_use]
pub fn event_value_float(event: &ParameterEvent, id: &str) -> Option<f32> {
    (event.id == id).then(|| event.value.as_float()).flatten()
}

#[must_use]
pub fn event_value_integer(event: &ParameterEvent, id: &str) -> Option<i32> {
    (event.id == id).then(|| event.value.as_integer()).flatten()
}

#[must_use]
pub fn event_value_bool(event: &ParameterEvent, id: &str) -> Option<bool> {
    (event.id == id).then(|| event.value.as_bool()).flatten()
}

#[must_use]
pub fn event_value_choice(event: &ParameterEvent, id: &str) -> Option<u32> {
    (event.id == id).then(|| event.value.as_choice()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_rejects_wrong_type_and_out_of_range() {
        let descriptor = ParameterDescriptor {
            id: "gain",
            name: "Gain",
            kind: ParameterKind::Float {
                min: -12.0,
                max: 12.0,
            },
            unit: ParameterUnit::Decibels,
            default: ParameterValue::Float(0.0),
            automatable: true,
            display_hint: None,
        };
        assert!(descriptor.accepts(ParameterValue::Float(6.0)));
        assert!(!descriptor.accepts(ParameterValue::Float(f32::NAN)));
        assert!(!descriptor.accepts(ParameterValue::Float(13.0)));
        assert!(!descriptor.accepts(ParameterValue::Integer(6)));
    }
}
