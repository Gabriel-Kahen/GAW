use std::path::PathBuf;

/// Persistence errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid project path `{0}`")]
    InvalidPath(String),
    #[error("project path traverses a symbolic link: {0}")]
    Symlink(PathBuf),
    #[error("invalid JSON in {path}: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("unsupported project schema version {found}; expected {expected}")]
    UnsupportedSchema { found: u64, expected: u32 },
    #[error("project.json is missing an integer schema_version")]
    MissingSchemaVersion,
    #[error("project directory is not empty: {0}")]
    DirectoryNotEmpty(PathBuf),
    #[error("project does not exist: {0}")]
    ProjectNotFound(PathBuf),
    #[error("transaction is invalid: {0}")]
    InvalidTransaction(String),
    #[error("cache index error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("unsupported disposable cache schema version {found}; expected {expected}")]
    UnsupportedCacheSchema { found: u32, expected: u32 },
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Error {
    Error::Io {
        path: path.into(),
        source,
    }
}
