use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::{Error, Result};

/// Portable filename key for a project-local preset document.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct PresetId(String);

impl PresetId {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(Error::InvalidPresetId(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PresetId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl<'de> Deserialize<'de> for PresetId {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy)]
pub(crate) enum PresetKind {
    Sampler,
    Effect,
}

impl PresetKind {
    pub(crate) const fn directory(self) -> &'static str {
        match self {
            Self::Sampler => "presets/samplers",
            Self::Effect => "presets/effects",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_portable_at_construction_and_deserialization() {
        assert_eq!(PresetId::new("wide-pad_2").unwrap().as_str(), "wide-pad_2");
        for invalid in ["", "../escape", "a/b", "a\\b", "C:drive", "space name"] {
            assert!(PresetId::new(invalid).is_err());
            assert!(serde_json::from_str::<PresetId>(&format!("\"{invalid}\"")).is_err());
        }
    }
}
