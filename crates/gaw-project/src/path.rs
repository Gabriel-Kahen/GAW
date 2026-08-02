use std::{fmt, path::Path};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// A normalized, project-root-relative path.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProjectPath(String);

impl ProjectPath {
    pub fn new(path: impl AsRef<str>) -> Result<Self> {
        let path = path.as_ref();
        if path.is_empty() || path.contains('\\') || path.contains('\0') {
            return Err(Error::InvalidPath(path.to_owned()));
        }
        let parsed = Path::new(path);
        if parsed.is_absolute()
            || path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == ".." || part.contains(':'))
        {
            return Err(Error::InvalidPath(path.to_owned()));
        }
        Ok(Self(path.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    pub(crate) fn is_canonical_json(&self) -> bool {
        if self.0 == "project.json" || self.0 == "assets/index.json" {
            return true;
        }
        let parts = self.0.split('/').collect::<Vec<_>>();
        match parts.as_slice() {
            ["compositions", id, "composition.json"] => valid_id(id, "cmp_"),
            ["compositions", composition, "tracks", file] => {
                valid_id(composition, "cmp_") && valid_json_id(file, "trk_")
            }
            ["compositions", composition, "automation", file] => {
                valid_id(composition, "cmp_") && valid_json_id(file, "lane_")
            }
            _ => false,
        }
    }
}

fn valid_json_id(file: &str, prefix: &str) -> bool {
    file.strip_suffix(".json")
        .is_some_and(|id| valid_id(id, prefix))
}

pub(crate) fn valid_id(id: &str, prefix: &str) -> bool {
    id.starts_with(prefix)
        && id.len() > prefix.len()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

impl fmt::Display for ProjectPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
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
        for path in ["", "/tmp/x", "../x", "a/../x", "a//x", "a\\x", "C:/x"] {
            assert!(ProjectPath::new(path).is_err(), "accepted {path}");
        }
        assert!(ProjectPath::new("compositions/cmp_1/composition.json").is_ok());
    }
}
