//! Typed, introspectable processor parameters.

use serde::{Deserialize, Serialize};

/// The storage and validation type of a processor parameter.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ParameterKind {
    Float {
        min: f32,
        max: f32,
    },
    Integer {
        min: i32,
        max: i32,
    },
    UnsignedInteger {
        min: u64,
        max: u64,
    },
    Boolean,
    Choice(&'static [&'static str]),
    /// A musical time whose serialized value carries either seconds or beats.
    Time {
        seconds_min: f32,
        seconds_max: f32,
        beats_min: f32,
        beats_max: f32,
    },
    /// A modulation rate expressed as either hertz or a period in beats.
    Rate {
        hertz_min: f32,
        hertz_max: f32,
        beats_min: f32,
        beats_max: f32,
    },
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
    UnsignedInteger(u64),
    Bool(bool),
    Choice(u32),
    Seconds(f32),
    Beats(f32),
    Hertz(f32),
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

    #[must_use]
    pub const fn as_unsigned_integer(self) -> Option<u64> {
        match self {
            Self::UnsignedInteger(value) => Some(value),
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
            (
                ParameterKind::UnsignedInteger { min, max },
                ParameterValue::UnsignedInteger(value),
            ) => (min..=max).contains(&value),
            (ParameterKind::Boolean, ParameterValue::Bool(_)) => true,
            (ParameterKind::Choice(choices), ParameterValue::Choice(value)) => {
                usize::try_from(value).is_ok_and(|value| value < choices.len())
            }
            (
                ParameterKind::Time {
                    seconds_min,
                    seconds_max,
                    ..
                },
                ParameterValue::Seconds(value),
            ) => value.is_finite() && (seconds_min..=seconds_max).contains(&value),
            (
                ParameterKind::Time {
                    beats_min,
                    beats_max,
                    ..
                }
                | ParameterKind::Rate {
                    beats_min,
                    beats_max,
                    ..
                },
                ParameterValue::Beats(value),
            ) => value.is_finite() && (beats_min..=beats_max).contains(&value),
            (
                ParameterKind::Rate {
                    hertz_min,
                    hertz_max,
                    ..
                },
                ParameterValue::Hertz(value),
            ) => value.is_finite() && (hertz_min..=hertz_max).contains(&value),
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

    #[test]
    fn structured_time_rate_and_seed_values_are_typed() {
        let time = ParameterDescriptor {
            id: "time",
            name: "Time",
            kind: ParameterKind::Time {
                seconds_min: 0.1,
                seconds_max: 2.0,
                beats_min: 0.25,
                beats_max: 4.0,
            },
            unit: ParameterUnit::None,
            default: ParameterValue::Beats(1.0),
            automatable: true,
            display_hint: None,
        };
        assert!(time.accepts(ParameterValue::Seconds(0.5)));
        assert!(time.accepts(ParameterValue::Beats(2.0)));
        assert!(!time.accepts(ParameterValue::Hertz(2.0)));

        let rate = ParameterDescriptor {
            id: "rate",
            name: "Rate",
            kind: ParameterKind::Rate {
                hertz_min: 0.01,
                hertz_max: 20.0,
                beats_min: 0.25,
                beats_max: 4.0,
            },
            unit: ParameterUnit::None,
            default: ParameterValue::Hertz(1.0),
            automatable: true,
            display_hint: None,
        };
        assert!(rate.accepts(ParameterValue::Hertz(2.0)));
        assert!(rate.accepts(ParameterValue::Beats(2.0)));
        assert!(!rate.accepts(ParameterValue::Seconds(0.5)));

        let seed = ParameterDescriptor {
            id: "seed",
            name: "Seed",
            kind: ParameterKind::UnsignedInteger {
                min: 0,
                max: u64::MAX,
            },
            unit: ParameterUnit::None,
            default: ParameterValue::UnsignedInteger(0),
            automatable: false,
            display_hint: None,
        };
        assert!(seed.accepts(ParameterValue::UnsignedInteger(u64::MAX)));
        assert!(!seed.accepts(ParameterValue::Integer(0)));
    }
}
