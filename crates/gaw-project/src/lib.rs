//! Portable project persistence and disposable runtime storage.
//!
//! Complete snapshots cross the persistence boundary as validated
//! [`gaw_core::Project`] values. Explicit manifest and composition-bundle views
//! support bounded lazy reads without claiming full cross-reference validation.

#![forbid(unsafe_code)]
#![allow(clippy::missing_errors_doc)]

mod cache;
mod error;
mod format;
mod midi;
mod path;
mod preset;
mod recovery;
mod session;
mod store;

pub use cache::{
    AnalysisCacheMetadata, CACHE_METADATA_VERSION, CacheEntry, CacheEviction, CacheIndex,
    CacheKind, CacheManager, CacheMetadata, CachePolicy, CacheScan, FileSystemSpace,
    RenderCacheMetadata, SpaceProbe, WaveformCacheMetadata,
};
pub use error::{Error, Result};
pub use format::{
    AssetIndex, AutomationLocation, CompositionBundle, ProjectManifest, TrackLocation,
};
pub use midi::{MidiError, MidiImport, export_midi, import_midi};
pub use path::ProjectPath;
pub use preset::PresetId;
pub use recovery::RecoveryRecord;
pub use session::{CHECKPOINT_WINDOW, ProjectSession};
pub use store::{ImportedMedia, ProjectStore, ValidationIssue, ValidationReport};

/// The only on-disk schema this version reads and writes.
pub const SCHEMA_VERSION: u32 = gaw_core::SCHEMA_VERSION;
