use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, ProjectPath, Result};

const GIB: u64 = 1024 * 1024 * 1024;
const CACHE_SCHEMA_VERSION: u32 = 1;
pub const CACHE_METADATA_VERSION: u32 = 1;

/// Disposable artifact category. Both kinds may be deleted and rebuilt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheKind {
    AudioRender,
    Waveform,
}

impl CacheKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::AudioRender => "audio_render",
            Self::Waveform => "waveform",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "audio_render" => Ok(Self::AudioRender),
            "waveform" => Ok(Self::Waveform),
            _ => Err(rusqlite::Error::InvalidColumnType(
                1,
                "kind".into(),
                rusqlite::types::Type::Text,
            )),
        }
    }
}

/// Noncanonical metadata for a materialized render or waveform.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheEntry {
    pub content_hash: String,
    pub kind: CacheKind,
    pub relative_path: ProjectPath,
    pub byte_len: u64,
    pub created_unix_ms: u64,
    pub last_access_unix_ms: u64,
    pub pinned: bool,
    pub metadata: CacheMetadata,
}

/// Versioned metadata needed to recognize or rebuild a disposable artifact.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "metadata_type", rename_all = "snake_case")]
pub enum CacheMetadata {
    AudioRender(RenderCacheMetadata),
    Waveform(WaveformCacheMetadata),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RenderCacheMetadata {
    pub metadata_version: u32,
    pub logical_asset_id: String,
    pub revision_key: String,
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub frame_count: u64,
    pub engine_version: String,
    #[serde(default)]
    pub context: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaveformCacheMetadata {
    pub metadata_version: u32,
    pub source_content_hash: String,
    pub channels: u16,
    pub frames_per_bucket: u32,
    pub bucket_count: u64,
}

/// Filesystem statistics supplied outside the real-time thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileSystemSpace {
    pub capacity_bytes: u64,
    pub free_bytes: u64,
}

/// Injectable disk-space probe, allowing platform-specific and test versions.
pub trait SpaceProbe {
    fn space(&self, containing_path: &Path) -> std::io::Result<FileSystemSpace>;
}

/// LRU budget and free-space protection settings.
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct CachePolicy {
    pub budget_bytes: Option<u64>,
    pub capacity_fraction: f64,
    pub minimum_budget_bytes: u64,
    pub maximum_budget_bytes: u64,
    pub minimum_free_bytes: u64,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            budget_bytes: None,
            capacity_fraction: 0.1,
            minimum_budget_bytes: 10 * GIB,
            maximum_budget_bytes: 100 * GIB,
            minimum_free_bytes: 5 * GIB,
        }
    }
}

impl CachePolicy {
    pub fn budget(&self, capacity_bytes: u64) -> u64 {
        self.budget_bytes.unwrap_or_else(|| {
            let fraction = if self.capacity_fraction.is_finite() && self.capacity_fraction >= 0.0 {
                self.capacity_fraction
            } else {
                0.1
            };
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_precision_loss,
                clippy::cast_sign_loss
            )]
            let proportional = (capacity_bytes as f64 * fraction) as u64;
            let low = self.minimum_budget_bytes.min(self.maximum_budget_bytes);
            let high = self.minimum_budget_bytes.max(self.maximum_budget_bytes);
            proportional.clamp(low, high)
        })
    }

    pub fn bytes_to_evict(&self, used_bytes: u64, space: FileSystemSpace) -> u64 {
        self.bytes_to_evict_for(used_bytes, 0, space)
    }

    /// Accounts for a pending write before allowing it into the cache.
    pub fn bytes_to_evict_for(
        &self,
        used_bytes: u64,
        incoming_bytes: u64,
        space: FileSystemSpace,
    ) -> u64 {
        let over_budget = used_bytes
            .saturating_add(incoming_bytes)
            .saturating_sub(self.budget(space.capacity_bytes));
        let projected_free = space.free_bytes.saturating_sub(incoming_bytes);
        let free_space_deficit = self.minimum_free_bytes.saturating_sub(projected_free);
        over_budget.max(free_space_deficit).min(used_bytes)
    }
}

/// Rebuildable `SQLite` index for cache lookup and LRU eviction.
#[derive(Debug)]
pub struct CacheIndex {
    connection: Connection,
}

impl CacheIndex {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let new_index = path.metadata().map_or(true, |metadata| metadata.len() == 0);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| crate::error::io(parent, source))?;
        }
        let connection = Connection::open(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS cache_meta (
                key TEXT PRIMARY KEY NOT NULL,
                value INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS cache_entries (
                content_hash TEXT PRIMARY KEY NOT NULL,
                kind TEXT NOT NULL,
                relative_path TEXT NOT NULL UNIQUE,
                byte_len INTEGER NOT NULL CHECK(byte_len >= 0),
                created_unix_ms INTEGER NOT NULL CHECK(created_unix_ms >= 0),
                last_access_unix_ms INTEGER NOT NULL CHECK(last_access_unix_ms >= 0),
                pinned INTEGER NOT NULL DEFAULT 0 CHECK(pinned IN (0, 1)),
                metadata_json TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS cache_lru
                ON cache_entries(pinned, last_access_unix_ms);",
        )?;
        let found = connection
            .query_row(
                "SELECT value FROM cache_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, u32>(0),
            )
            .optional()?;
        match found {
            Some(found) if found != CACHE_SCHEMA_VERSION => {
                return Err(Error::UnsupportedCacheSchema {
                    found,
                    expected: CACHE_SCHEMA_VERSION,
                });
            }
            Some(_) => {}
            None if new_index => {
                connection.execute(
                    "INSERT INTO cache_meta(key, value) VALUES ('schema_version', ?1)",
                    [CACHE_SCHEMA_VERSION],
                )?;
            }
            None => {
                return Err(Error::UnsupportedCacheSchema {
                    found: 0,
                    expected: CACHE_SCHEMA_VERSION,
                });
            }
        }
        Ok(Self { connection })
    }

    /// Opens the disposable index, rebuilding only its `SQLite` files on damage
    /// or an unsupported cache schema.
    pub fn open_or_rebuild(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match Self::open(path) {
            Ok(index) => Ok(index),
            Err(Error::Sqlite(_) | Error::UnsupportedCacheSchema { .. }) => {
                remove_sqlite_files(path)?;
                Self::open(path)
            }
            Err(error) => Err(error),
        }
    }

    pub fn record(&self, entry: &CacheEntry) -> Result<()> {
        validate_cache_entry(entry)?;
        let metadata = serde_json::to_string(&entry.metadata).map_err(|source| Error::Json {
            path: entry.relative_path.as_path().to_owned(),
            source,
        })?;
        self.connection.execute(
            "INSERT INTO cache_entries
             (content_hash, kind, relative_path, byte_len, created_unix_ms,
              last_access_unix_ms, pinned, metadata_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(content_hash) DO UPDATE SET
               kind=excluded.kind, relative_path=excluded.relative_path,
               byte_len=excluded.byte_len, created_unix_ms=excluded.created_unix_ms,
               last_access_unix_ms=excluded.last_access_unix_ms,
               pinned=excluded.pinned, metadata_json=excluded.metadata_json",
            params![
                entry.content_hash,
                entry.kind.as_str(),
                entry.relative_path.as_str(),
                to_i64(entry.byte_len)?,
                to_i64(entry.created_unix_ms)?,
                to_i64(entry.last_access_unix_ms)?,
                entry.pinned,
                metadata,
            ],
        )?;
        Ok(())
    }

    pub fn get(&self, content_hash: &str) -> Result<Option<CacheEntry>> {
        self.connection
            .query_row(
                "SELECT content_hash, kind, relative_path, byte_len,
                        created_unix_ms, last_access_unix_ms, pinned, metadata_json
                 FROM cache_entries WHERE content_hash = ?1",
                [content_hash],
                row_to_entry,
            )
            .optional()
            .map_err(Error::from)
    }

    pub fn touch(&self, content_hash: &str) -> Result<bool> {
        let now = now_unix_ms();
        Ok(self.connection.execute(
            "UPDATE cache_entries SET last_access_unix_ms = ?1 WHERE content_hash = ?2",
            params![to_i64(now)?, content_hash],
        )? != 0)
    }

    pub fn set_pinned(&self, content_hash: &str, pinned: bool) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE cache_entries SET pinned = ?1 WHERE content_hash = ?2",
            params![pinned, content_hash],
        )? != 0)
    }

    pub fn remove(&self, content_hash: &str) -> Result<bool> {
        Ok(self.connection.execute(
            "DELETE FROM cache_entries WHERE content_hash = ?1",
            [content_hash],
        )? != 0)
    }

    pub fn used_bytes(&self) -> Result<u64> {
        let bytes: i64 = self.connection.query_row(
            "SELECT COALESCE(SUM(byte_len), 0) FROM cache_entries",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(bytes).unwrap_or(0))
    }

    /// Returns oldest unpinned entries sufficient to meet the policy target.
    /// The caller performs file deletion during idle work, then calls `remove`.
    pub fn eviction_plan(
        &self,
        policy: &CachePolicy,
        space: FileSystemSpace,
    ) -> Result<Vec<CacheEntry>> {
        self.eviction_plan_for(policy, space, 0)
    }

    /// Plans idle-time eviction before writing an artifact of `incoming_bytes`.
    pub fn eviction_plan_for(
        &self,
        policy: &CachePolicy,
        space: FileSystemSpace,
        incoming_bytes: u64,
    ) -> Result<Vec<CacheEntry>> {
        let required = policy.bytes_to_evict_for(self.used_bytes()?, incoming_bytes, space);
        if required == 0 {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT content_hash, kind, relative_path, byte_len,
                    created_unix_ms, last_access_unix_ms, pinned, metadata_json
             FROM cache_entries WHERE pinned = 0
             ORDER BY last_access_unix_ms ASC, content_hash ASC",
        )?;
        let candidates = statement.query_map([], row_to_entry)?;
        let mut selected = Vec::new();
        let mut bytes = 0_u64;
        for candidate in candidates {
            let candidate = candidate?;
            bytes = bytes.saturating_add(candidate.byte_len);
            selected.push(candidate);
            if bytes >= required {
                break;
            }
        }
        Ok(selected)
    }
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<CacheEntry> {
    let kind: String = row.get(1)?;
    let path: String = row.get(2)?;
    let byte_len: i64 = row.get(3)?;
    let created: i64 = row.get(4)?;
    let last_access: i64 = row.get(5)?;
    let metadata: String = row.get(7)?;
    Ok(CacheEntry {
        content_hash: row.get(0)?,
        kind: CacheKind::parse(&kind)?,
        relative_path: ProjectPath::new(&path).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
        byte_len: u64::try_from(byte_len).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        created_unix_ms: u64::try_from(created).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        last_access_unix_ms: u64::try_from(last_access).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        pinned: row.get(6)?,
        metadata: serde_json::from_str(&metadata).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
    })
}

fn validate_cache_entry(entry: &CacheEntry) -> Result<()> {
    let expected_prefix = match entry.kind {
        CacheKind::AudioRender => ".gaw/cache/audio/",
        CacheKind::Waveform => ".gaw/cache/waveforms/",
    };
    if !entry.relative_path.as_str().starts_with(expected_prefix) {
        return Err(Error::InvalidPath(entry.relative_path.to_string()));
    }
    if entry.content_hash.len() != 64
        || !entry
            .content_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(Error::InvalidTransaction(
            "cache content hash must be 64 lowercase hexadecimal characters".into(),
        ));
    }
    match (&entry.kind, &entry.metadata) {
        (CacheKind::AudioRender, CacheMetadata::AudioRender(metadata))
            if metadata.metadata_version == CACHE_METADATA_VERSION => {}
        (CacheKind::Waveform, CacheMetadata::Waveform(metadata))
            if metadata.metadata_version == CACHE_METADATA_VERSION => {}
        (CacheKind::AudioRender, CacheMetadata::AudioRender(_))
        | (CacheKind::Waveform, CacheMetadata::Waveform(_)) => {
            return Err(Error::InvalidTransaction(
                "unsupported cache metadata version".into(),
            ));
        }
        _ => {
            return Err(Error::InvalidTransaction(
                "cache kind does not match its metadata type".into(),
            ));
        }
    }
    Ok(())
}

fn remove_sqlite_files(path: &Path) -> Result<()> {
    for target in [
        path.to_owned(),
        sqlite_sidecar(path, "-wal"),
        sqlite_sidecar(path, "-shm"),
    ] {
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(crate::error::io(target, error)),
        }
    }
    Ok(())
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = OsString::from(path.as_os_str());
    value.push(suffix);
    PathBuf::from(value)
}

fn to_i64(value: u64) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| Error::InvalidTransaction("cache value exceeds SQLite integer range".into()))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(hash_character: char, accessed: u64, pinned: bool) -> CacheEntry {
        CacheEntry {
            content_hash: hash_character.to_string().repeat(64),
            kind: CacheKind::Waveform,
            relative_path: ProjectPath::new(format!(
                ".gaw/cache/waveforms/{}.json",
                hash_character.to_string().repeat(64)
            ))
            .unwrap(),
            byte_len: 100,
            created_unix_ms: accessed,
            last_access_unix_ms: accessed,
            pinned,
            metadata: CacheMetadata::Waveform(WaveformCacheMetadata {
                metadata_version: CACHE_METADATA_VERSION,
                source_content_hash: hash_character.to_string().repeat(64),
                channels: 2,
                frames_per_bucket: 256,
                bucket_count: 512,
            }),
        }
    }

    #[test]
    fn default_policy_is_clamped_and_protects_free_space() {
        let policy = CachePolicy::default();
        assert_eq!(policy.budget(20 * GIB), 10 * GIB);
        assert_eq!(policy.budget(2_000 * GIB), 100 * GIB);
        assert_eq!(
            policy.bytes_to_evict(
                8 * GIB,
                FileSystemSpace {
                    capacity_bytes: 200 * GIB,
                    free_bytes: GIB,
                }
            ),
            4 * GIB
        );
        assert_eq!(
            CachePolicy {
                budget_bytes: Some(1_000),
                minimum_free_bytes: 500,
                ..CachePolicy::default()
            }
            .bytes_to_evict_for(
                800,
                400,
                FileSystemSpace {
                    capacity_bytes: 10_000,
                    free_bytes: 600,
                },
            ),
            300
        );
    }

    #[test]
    fn lru_plan_skips_pinned_entries() {
        let directory = tempfile::tempdir().unwrap();
        let index = CacheIndex::open(directory.path().join("index.sqlite")).unwrap();
        index.record(&entry('a', 1, true)).unwrap();
        index.record(&entry('b', 2, false)).unwrap();
        index.record(&entry('c', 3, false)).unwrap();
        let policy = CachePolicy {
            budget_bytes: Some(150),
            minimum_free_bytes: 0,
            ..CachePolicy::default()
        };
        let plan = index
            .eviction_plan(
                &policy,
                FileSystemSpace {
                    capacity_bytes: 1_000,
                    free_bytes: 1_000,
                },
            )
            .unwrap();
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].content_hash, "b".repeat(64));
        assert_eq!(plan[1].content_hash, "c".repeat(64));
    }

    #[test]
    fn disposable_index_rebuilds_after_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("index.sqlite");
        std::fs::write(&path, b"not sqlite").unwrap();
        let index = CacheIndex::open_or_rebuild(&path).unwrap();
        index.record(&entry('d', 1, false)).unwrap();
        assert!(index.get(&"d".repeat(64)).unwrap().is_some());
    }

    #[test]
    fn rejects_metadata_that_does_not_match_cache_kind() {
        let directory = tempfile::tempdir().unwrap();
        let index = CacheIndex::open(directory.path().join("index.sqlite")).unwrap();
        let mut invalid = entry('e', 1, false);
        invalid.metadata = CacheMetadata::AudioRender(RenderCacheMetadata {
            metadata_version: CACHE_METADATA_VERSION,
            logical_asset_id: "ast_example".into(),
            revision_key: "e".repeat(64),
            sample_rate_hz: 48_000,
            channels: 2,
            frame_count: 10,
            engine_version: "test".into(),
            context: Value::Null,
        });
        assert!(index.record(&invalid).is_err());
    }
}
