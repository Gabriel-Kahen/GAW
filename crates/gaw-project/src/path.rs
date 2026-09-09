use std::{fmt, path::Path};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// A normalized, project-root-relative path.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProjectPath(gaw_core::ProjectPath);

impl ProjectPath {
    pub fn new(path: impl AsRef<str>) -> Result<Self> {
        let path = path.as_ref();
        gaw_core::ProjectPath::new(path)
            .map(Self)
            .map_err(|_| Error::InvalidPath(path.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn as_path(&self) -> &Path {
        Path::new(self.as_str())
    }

    pub(crate) fn is_canonical_json(&self) -> bool {
        if self.as_str() == "project.json" || self.as_str() == "assets/index.json" {
            return true;
        }
        let parts = self.as_str().split('/').collect::<Vec<_>>();
        match parts.as_slice() {
            ["events", file] => valid_json_id(file),
            ["compositions", id, "composition.json"] => valid_id(id),
            ["compositions", composition, "tracks" | "automation", file] => {
                valid_id(composition) && valid_json_id(file)
            }
            _ => false,
        }
    }
}

fn valid_json_id(file: &str) -> bool {
    file.strip_suffix(".json").is_some_and(valid_id)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

impl fmt::Display for ProjectPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(formatter)
    }
}

impl<'de> Deserialize<'de> for ProjectPath {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let path = String::deserialize(deserializer)?;
        Self::new(&path).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<&str> for ProjectPath {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self> {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_and_non_portable_paths() {
        for path in [
            "", "/tmp/x", "../x", "a/../x", "a//x", "a\\x", "C:/x", "a\0b",
        ] {
            assert!(ProjectPath::new(path).is_err(), "accepted {path}");
        }
        let path = "compositions/4f61ed9d/tracks/8d02.json";
        let stored = ProjectPath::new(path).unwrap();
        let core = gaw_core::ProjectPath::new(path).unwrap();
        assert_eq!(stored.as_str(), core.as_str());
        assert_eq!(
            serde_json::to_value(stored).unwrap(),
            serde_json::to_value(core).unwrap()
        );
    }
}
