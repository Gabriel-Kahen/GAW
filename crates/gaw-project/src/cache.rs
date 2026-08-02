use std::{
    collections::HashSet,
    ffi::OsString,
    fmt::Write as _,
    fs::{self, File},
    io::{Read, Write as _},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

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
    Analysis,
}

impl CacheKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::AudioRender => "audio_render",
            Self::Waveform => "waveform",
            Self::Analysis => "analysis",
        }
    }

    fn parse(value: &str) -> rusqlite::Result<Self> {
        match value {
            "audio_render" => Ok(Self::AudioRender),
            "waveform" => Ok(Self::Waveform),
            "analysis" => Ok(Self::Analysis),
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
#[serde(tag = "metadata_type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CacheMetadata {
    AudioRender(RenderCacheMetadata),
    Waveform(WaveformCacheMetadata),
    Analysis(AnalysisCacheMetadata),
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

/// Versioned description of a disposable analysis result.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisCacheMetadata {
    pub metadata_version: u32,
    pub source_content_hash: String,
    pub analyzer_id: String,
    pub analyzer_version: String,
    #[serde(default)]
    pub parameters: Value,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ArtifactManifest {
    content_hash: String,
    kind: CacheKind,
    relative_path: ProjectPath,
    byte_len: u64,
    created_unix_ms: u64,
    metadata: CacheMetadata,
}

impl From<&CacheEntry> for ArtifactManifest {
    fn from(entry: &CacheEntry) -> Self {
        Self {
            content_hash: entry.content_hash.clone(),
            kind: entry.kind,
            relative_path: entry.relative_path.clone(),
            byte_len: entry.byte_len,
            created_unix_ms: entry.created_unix_ms,
            metadata: entry.metadata.clone(),
        }
    }
}

impl ArtifactManifest {
    fn into_entry(self) -> CacheEntry {
        CacheEntry {
            content_hash: self.content_hash,
            kind: self.kind,
            relative_path: self.relative_path,
            byte_len: self.byte_len,
            created_unix_ms: self.created_unix_ms,
            last_access_unix_ms: self.created_unix_ms,
            pinned: false,
            metadata: self.metadata,
        }
    }
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

/// Result of scanning durable artifact manifests into the disposable index.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheScan {
    pub registered: usize,
    pub stale_manifests: usize,
}

/// Result of applying the cache policy outside the real-time thread.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CacheEviction {
    pub deleted_entries: usize,
    pub deleted_bytes: u64,
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

    fn replace_pins(&mut self, content_hashes: &HashSet<String>) -> Result<()> {
        let transaction = self.connection.transaction()?;
        transaction.execute("UPDATE cache_entries SET pinned = 0", [])?;
        {
            let mut statement = transaction
                .prepare("UPDATE cache_entries SET pinned = 1 WHERE content_hash = ?1")?;
            for content_hash in content_hashes {
                statement.execute([content_hash])?;
            }
        }
        transaction.commit()?;
        Ok(())
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

/// Filesystem-enforcing facade for the disposable cache index.
///
/// Artifact manifests are durable enough to rebuild the derived `SQLite` index,
/// but both manifests and artifacts remain disposable project runtime state.
#[derive(Debug)]
pub struct CacheManager {
    cache_root: PathBuf,
    manifests_root: PathBuf,
    index: CacheIndex,
    policy: CachePolicy,
}

impl CacheManager {
    pub fn open(project_root: impl AsRef<Path>, policy: CachePolicy) -> Result<Self> {
        let project_root = canonical_directory(project_root.as_ref())?;
        let cache_root = ensure_cache_directories(&project_root)?;
        let manifests_root = cache_root.join("manifests");
        let index = CacheIndex::open_or_rebuild(cache_root.join("index.sqlite"))?;
        let mut manager = Self {
            cache_root,
            manifests_root,
            index,
            policy,
        };
        manager.scan_artifacts()?;
        Ok(manager)
    }

    /// Deletes only the derived index and repopulates it from artifact manifests.
    pub fn rebuild(project_root: impl AsRef<Path>, policy: CachePolicy) -> Result<Self> {
        let project_root = canonical_directory(project_root.as_ref())?;
        let cache_root = ensure_cache_directories(&project_root)?;
        remove_sqlite_files(&cache_root.join("index.sqlite"))?;
        Self::open(project_root, policy)
    }

    pub fn get(&self, content_hash: &str) -> Result<Option<CacheEntry>> {
        self.index.get(content_hash)
    }

    pub fn used_bytes(&self) -> Result<u64> {
        self.index.used_bytes()
    }

    /// Registers an already materialized cache artifact after checking its path,
    /// size, and SHA-256 content hash, then writes rebuild metadata atomically.
    pub fn register(&mut self, entry: &CacheEntry) -> Result<()> {
        validate_cache_entry(entry)?;
        let artifact = self.artifact_path(&entry.relative_path, true)?;
        let metadata =
            fs::symlink_metadata(&artifact).map_err(|error| crate::error::io(&artifact, error))?;
        if !metadata.is_file() || metadata.len() != entry.byte_len {
            return Err(Error::InvalidTransaction(
                "cache artifact is not a regular file of the declared size".into(),
            ));
        }
        if sha256_file(&artifact)? != entry.content_hash {
            return Err(Error::InvalidTransaction(
                "cache artifact content does not match its content hash".into(),
            ));
        }
        self.write_manifest(entry)?;
        self.index.record(entry)
    }

    /// Scans the bounded manifest directory and repairs index entries. Manifests
    /// whose artifacts disappeared are discarded as stale disposable state.
    pub fn scan_artifacts(&mut self) -> Result<CacheScan> {
        let mut result = CacheScan::default();
        for item in fs::read_dir(&self.manifests_root)
            .map_err(|error| crate::error::io(&self.manifests_root, error))?
        {
            let item = item.map_err(|error| crate::error::io(&self.manifests_root, error))?;
            let path = item.path();
            let file_type = item
                .file_type()
                .map_err(|error| crate::error::io(&path, error))?;
            if file_type.is_symlink() {
                return Err(Error::Symlink(path));
            }
            if !file_type.is_file() || path.extension().is_none_or(|value| value != "json") {
                continue;
            }
            let bytes = fs::read(&path).map_err(|error| crate::error::io(&path, error))?;
            let manifest: ArtifactManifest =
                serde_json::from_slice(&bytes).map_err(|source| Error::Json {
                    path: path.clone(),
                    source,
                })?;
            let mut entry = manifest.into_entry();
            validate_cache_entry(&entry)?;
            if path.file_name().and_then(|value| value.to_str())
                != Some(&format!("{}.json", entry.content_hash))
            {
                return Err(Error::InvalidTransaction(
                    "cache manifest name does not match its content hash".into(),
                ));
            }
            let artifact = self.artifact_path(&entry.relative_path, false)?;
            let artifact_metadata = match fs::symlink_metadata(&artifact) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::remove_file(&path).map_err(|source| crate::error::io(&path, source))?;
                    self.index.remove(&entry.content_hash)?;
                    result.stale_manifests += 1;
                    continue;
                }
                Err(error) => return Err(crate::error::io(&artifact, error)),
            };
            if artifact_metadata.file_type().is_symlink() {
                return Err(Error::Symlink(artifact));
            }
            if !artifact_metadata.is_file()
                || artifact_metadata.len() != entry.byte_len
                || sha256_file(&artifact)? != entry.content_hash
            {
                return Err(Error::InvalidTransaction(
                    "cache artifact does not match its manifest".into(),
                ));
            }
            if let Some(indexed) = self.index.get(&entry.content_hash)? {
                entry.last_access_unix_ms = indexed.last_access_unix_ms;
                entry.pinned = indexed.pinned;
            }
            self.index.record(&entry)?;
            result.registered += 1;
        }
        Ok(result)
    }

    /// Replaces the pin set with the union of current-project and undo references.
    pub fn pin_references<C, U, CS, US>(&mut self, current: C, undo: U) -> Result<()>
    where
        C: IntoIterator<Item = CS>,
        U: IntoIterator<Item = US>,
        CS: AsRef<str>,
        US: AsRef<str>,
    {
        let content_hashes = current
            .into_iter()
            .map(|value| value.as_ref().to_owned())
            .chain(undo.into_iter().map(|value| value.as_ref().to_owned()))
            .collect();
        self.index.replace_pins(&content_hashes)
    }

    /// Probes the containing filesystem, plans LRU eviction, and safely deletes
    /// the selected unpinned artifacts and their manifests.
    pub fn enforce<P: SpaceProbe>(
        &mut self,
        probe: &P,
        incoming_bytes: u64,
    ) -> Result<CacheEviction> {
        let space = probe
            .space(&self.cache_root)
            .map_err(|error| crate::error::io(&self.cache_root, error))?;
        let plan = self
            .index
            .eviction_plan_for(&self.policy, space, incoming_bytes)?;
        self.delete_planned(&plan)
    }

    pub fn delete_planned(&mut self, plan: &[CacheEntry]) -> Result<CacheEviction> {
        let mut result = CacheEviction::default();
        for planned in plan {
            let Some(indexed) = self.index.get(&planned.content_hash)? else {
                continue;
            };
            if indexed.pinned {
                continue;
            }
            if indexed.relative_path != planned.relative_path {
                return Err(Error::InvalidTransaction(
                    "cache eviction plan no longer matches the index".into(),
                ));
            }
            let artifact = self.artifact_path(&indexed.relative_path, false)?;
            remove_regular_file_if_present(&artifact)?;
            let manifest = self.manifest_path(&indexed.content_hash)?;
            remove_regular_file_if_present(&manifest)?;
            self.index.remove(&indexed.content_hash)?;
            result.deleted_entries += 1;
            result.deleted_bytes = result.deleted_bytes.saturating_add(indexed.byte_len);
        }
        Ok(result)
    }

    fn artifact_path(&self, relative_path: &ProjectPath, must_exist: bool) -> Result<PathBuf> {
        let relative = relative_path
            .as_path()
            .strip_prefix(".gaw/cache")
            .map_err(|_| Error::InvalidPath(relative_path.to_string()))?;
        safe_descendant(&self.cache_root, relative, must_exist)
    }

    fn manifest_path(&self, content_hash: &str) -> Result<PathBuf> {
        safe_descendant(
            &self.manifests_root,
            Path::new(&format!("{content_hash}.json")),
            false,
        )
    }

    fn write_manifest(&self, entry: &CacheEntry) -> Result<()> {
        let destination = self.manifest_path(&entry.content_hash)?;
        let mut temporary = tempfile::NamedTempFile::new_in(&self.manifests_root)
            .map_err(|error| crate::error::io(&self.manifests_root, error))?;
        serde_json::to_writer(&mut temporary, &ArtifactManifest::from(entry)).map_err(
            |source| Error::Json {
                path: destination.clone(),
                source,
            },
        )?;
        temporary
            .write_all(b"\n")
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|error| crate::error::io(&destination, error))?;
        temporary
            .persist(&destination)
            .map_err(|error| crate::error::io(&destination, error.error))?;
        File::open(&self.manifests_root)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| crate::error::io(&self.manifests_root, error))?;
        Ok(())
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

fn canonical_directory(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path).map_err(|error| crate::error::io(path, error))?;
    if metadata.file_type().is_symlink() {
        return Err(Error::Symlink(path.to_owned()));
    }
    if !metadata.is_dir() {
        return Err(Error::InvalidPath(path.display().to_string()));
    }
    path.canonicalize()
        .map_err(|error| crate::error::io(path, error))
}

fn ensure_cache_directories(project_root: &Path) -> Result<PathBuf> {
    let mut directory = project_root.to_owned();
    for component in [".gaw", "cache"] {
        directory.push(component);
        ensure_directory(&directory)?;
    }
    let cache_root = directory;
    for component in ["audio", "waveforms", "analysis", "manifests"] {
        ensure_directory(&cache_root.join(component))?;
    }
    cache_root
        .canonicalize()
        .map_err(|error| crate::error::io(&cache_root, error))
}

fn ensure_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Symlink(path.to_owned())),
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(Error::InvalidPath(path.display().to_string())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|source| crate::error::io(path, source))
        }
        Err(error) => Err(crate::error::io(path, error)),
    }
}

fn safe_descendant(root: &Path, relative: &Path, must_exist: bool) -> Result<PathBuf> {
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        return Err(Error::InvalidPath(relative.display().to_string()));
    }
    let mut target = root.to_owned();
    let components = relative.components().collect::<Vec<_>>();
    for (position, component) in components.iter().enumerate() {
        let std::path::Component::Normal(component) = component else {
            return Err(Error::InvalidPath(relative.display().to_string()));
        };
        target.push(component);
        match fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::Symlink(target));
            }
            Ok(metadata) if position + 1 != components.len() && !metadata.is_dir() => {
                return Err(Error::InvalidPath(target.display().to_string()));
            }
            Ok(_) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && position + 1 == components.len()
                    && !must_exist => {}
            Err(error) => return Err(crate::error::io(&target, error)),
        }
    }
    Ok(target)
}

fn remove_regular_file_if_present(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(Error::Symlink(path.to_owned())),
        Ok(metadata) if !metadata.is_file() => Err(Error::InvalidPath(path.display().to_string())),
        Ok(_) => fs::remove_file(path).map_err(|error| crate::error::io(path, error)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(crate::error::io(path, error)),
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|error| crate::error::io(path, error))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| crate::error::io(path, error))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let mut encoded = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    Ok(encoded)
}

fn validate_cache_entry(entry: &CacheEntry) -> Result<()> {
    let expected_prefix = match entry.kind {
        CacheKind::AudioRender => ".gaw/cache/audio/",
        CacheKind::Waveform => ".gaw/cache/waveforms/",
        CacheKind::Analysis => ".gaw/cache/analysis/",
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
        (CacheKind::Analysis, CacheMetadata::Analysis(metadata))
            if metadata.metadata_version == CACHE_METADATA_VERSION => {}
        (CacheKind::AudioRender, CacheMetadata::AudioRender(_))
        | (CacheKind::Waveform, CacheMetadata::Waveform(_))
        | (CacheKind::Analysis, CacheMetadata::Analysis(_)) => {
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

    struct FixedSpace(FileSystemSpace);

    impl SpaceProbe for FixedSpace {
        fn space(&self, _containing_path: &Path) -> std::io::Result<FileSystemSpace> {
            Ok(self.0)
        }
    }

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

    fn analysis_entry(root: &Path, bytes: &[u8], accessed: u64) -> CacheEntry {
        let content_hash = {
            let mut digest = Sha256::new();
            digest.update(bytes);
            format!("{:x}", digest.finalize())
        };
        let relative_path =
            ProjectPath::new(format!(".gaw/cache/analysis/{content_hash}.bin")).unwrap();
        let path = root.join(relative_path.as_path());
        fs::write(path, bytes).unwrap();
        CacheEntry {
            content_hash: content_hash.clone(),
            kind: CacheKind::Analysis,
            relative_path,
            byte_len: bytes.len().try_into().unwrap(),
            created_unix_ms: accessed,
            last_access_unix_ms: accessed,
            pinned: false,
            metadata: CacheMetadata::Analysis(AnalysisCacheMetadata {
                metadata_version: CACHE_METADATA_VERSION,
                source_content_hash: "a".repeat(64),
                analyzer_id: "test.loudness".into(),
                analyzer_version: "1".into(),
                parameters: Value::Null,
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

    #[test]
    fn manager_registers_and_rebuilds_analysis_artifacts() {
        let directory = tempfile::tempdir().unwrap();
        let mut manager = CacheManager::open(directory.path(), CachePolicy::default()).unwrap();
        let entry = analysis_entry(directory.path(), b"analysis-result", 7);
        manager.register(&entry).unwrap();
        assert_eq!(manager.used_bytes().unwrap(), entry.byte_len);
        drop(manager);

        fs::write(
            directory.path().join(".gaw/cache/index.sqlite"),
            b"broken sqlite",
        )
        .unwrap();
        let manager = CacheManager::open(directory.path(), CachePolicy::default()).unwrap();
        let rebuilt = manager.get(&entry.content_hash).unwrap().unwrap();
        assert_eq!(rebuilt.relative_path, entry.relative_path);
        assert!(matches!(rebuilt.metadata, CacheMetadata::Analysis(_)));
    }

    #[test]
    fn manager_pins_references_and_enforces_policy() {
        let directory = tempfile::tempdir().unwrap();
        let policy = CachePolicy {
            budget_bytes: Some(4),
            minimum_free_bytes: 0,
            ..CachePolicy::default()
        };
        let mut manager = CacheManager::open(directory.path(), policy).unwrap();
        let current = analysis_entry(directory.path(), b"keep", 1);
        let disposable = analysis_entry(directory.path(), b"delete", 2);
        manager.register(&current).unwrap();
        manager.register(&disposable).unwrap();
        manager
            .pin_references([current.content_hash.as_str()], std::iter::empty::<&str>())
            .unwrap();

        let result = manager
            .enforce(
                &FixedSpace(FileSystemSpace {
                    capacity_bytes: 1_000,
                    free_bytes: 1_000,
                }),
                0,
            )
            .unwrap();
        assert_eq!(result.deleted_entries, 1);
        assert_eq!(result.deleted_bytes, disposable.byte_len);
        assert!(
            directory
                .path()
                .join(current.relative_path.as_path())
                .exists()
        );
        assert!(
            !directory
                .path()
                .join(disposable.relative_path.as_path())
                .exists()
        );
        assert!(manager.get(&current.content_hash).unwrap().unwrap().pinned);
        assert!(manager.get(&disposable.content_hash).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn manager_refuses_to_delete_an_artifact_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let outside = directory.path().join("outside");
        fs::write(&outside, b"outside").unwrap();
        let mut manager = CacheManager::open(directory.path(), CachePolicy::default()).unwrap();
        let entry = analysis_entry(directory.path(), b"artifact", 1);
        manager.register(&entry).unwrap();
        let artifact = directory.path().join(entry.relative_path.as_path());
        fs::remove_file(&artifact).unwrap();
        symlink(&outside, &artifact).unwrap();

        assert!(matches!(
            manager.delete_planned(std::slice::from_ref(&entry)),
            Err(Error::Symlink(_))
        ));
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert!(manager.get(&entry.content_hash).unwrap().is_some());
    }
}
