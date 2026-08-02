//! Portable project persistence and disposable runtime storage.
//!
//! Canonical project data is deliberately represented as JSON here. The domain
//! crate owns its schema; this crate owns safe paths, durable replacement and
//! recovery. Keeping that boundary narrow also lets old schema adapters live at
//! the edge instead of leaking into storage.

#![forbid(unsafe_code)]
#![allow(clippy::missing_errors_doc)]

mod cache;
mod error;
mod path;
mod recovery;
mod store;

pub use cache::{
    CACHE_METADATA_VERSION, CacheEntry, CacheIndex, CacheKind, CacheMetadata, CachePolicy,
    FileSystemSpace, RenderCacheMetadata, SpaceProbe, WaveformCacheMetadata,
};
pub use error::{Error, Result};
pub use path::ProjectPath;
pub use recovery::RecoveryRecord;
pub use store::{
    ImportedMedia, JsonOperation, JsonTransaction, ProjectSnapshot, ProjectStore, ValidationIssue,
    ValidationReport,
};

/// The only on-disk schema this version reads and writes.
pub const SCHEMA_VERSION: u32 = gaw_core::SCHEMA_VERSION;
