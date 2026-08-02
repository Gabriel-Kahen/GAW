//! Portable project persistence and disposable runtime storage.
//!
//! The public persistence boundary is the canonical [`gaw_core::Project`] and
//! [`gaw_core::Transaction`] model. JSON splitting is an internal storage detail.

#![forbid(unsafe_code)]
#![allow(clippy::missing_errors_doc)]

mod cache;
mod error;
mod format;
mod midi;
mod path;
mod recovery;
mod session;
mod store;

pub use cache::{
    AnalysisCacheMetadata, CACHE_METADATA_VERSION, CacheEntry, CacheEviction, CacheIndex,
    CacheKind, CacheManager, CacheMetadata, CachePolicy, CacheScan, FileSystemSpace,
    RenderCacheMetadata, SpaceProbe, WaveformCacheMetadata,
};
pub use error::{Error, Result};
pub use midi::{MidiError, MidiImport, export_midi, import_midi};
pub use path::ProjectPath;
pub use recovery::RecoveryRecord;
pub use session::{CHECKPOINT_WINDOW, ProjectSession};
pub use store::{ImportedMedia, ProjectStore, ValidationIssue, ValidationReport};

/// The only on-disk schema this version reads and writes.
pub const SCHEMA_VERSION: u32 = gaw_core::SCHEMA_VERSION;
