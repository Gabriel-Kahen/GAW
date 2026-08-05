use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufWriter, Read, Seek, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use gaw_core::{
    AssetTempo, AudioAsset, AudioAssetDefinition, Bpm, ChannelLayout, Command, CompositionId,
    ContentHash, EffectPreset, EventData, EventDataId, FrameCount, FrameRange, ImportedAudio,
    Project, SampleRate, SamplerPreset, Seconds, Transaction,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};
use symphonia::core::{
    codecs::audio::AudioDecoderOptions,
    errors::Error as DecodeError,
    formats::{FormatOptions, TrackType, probe::Hint},
    io::{MediaSourceStream, MediaSourceStreamOptions},
    meta::MetadataOptions,
};

use crate::{
    AssetIndex, CompositionBundle, Error, PresetId, ProjectManifest, ProjectPath, RecoveryRecord,
    Result, SCHEMA_VERSION, error::io, format, preset::PresetKind, recovery,
};

static TRANSACTION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static JSON_READS: std::cell::RefCell<Vec<(String, u64)>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StorageTransaction {
    schema_version: u32,
    operations: Vec<StorageOperation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum StorageOperation {
    Write { path: ProjectPath, document: Value },
    Delete { path: ProjectPath },
}

impl StorageOperation {
    fn path(&self) -> &ProjectPath {
        match self {
            Self::Write { path, .. } | Self::Delete { path } => path,
        }
    }
}

/// Result of importing media into the typed asset model.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportedMedia {
    pub asset_id: gaw_core::AssetId,
    pub content_hash: ContentHash,
    pub relative_path: gaw_core::ProjectPath,
    pub original_filename: String,
    pub byte_len: u64,
}

/// One confirmed constant-tempo region to materialize from an imported asset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MediaRegion {
    pub range: FrameRange,
    pub bpm: Bpm,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ValidationIssue {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ValidationReport {
    pub errors: Vec<ValidationIssue>,
    pub warnings: Vec<ValidationIssue>,
}

impl ValidationReport {
    pub fn is_valid(&self) -> bool {
        self.errors.is_empty()
    }

    fn from_error(root: &Path, error: &Error) -> Self {
        Self {
            errors: vec![ValidationIssue {
                path: root.display().to_string(),
                message: error.to_string(),
            }],
            warnings: Vec::new(),
        }
    }
}

/// A directory-backed canonical GAW project.
#[derive(Clone, Debug)]
pub struct ProjectStore {
    root: PathBuf,
}

impl ProjectStore {
    /// Creates an empty directory-backed store from a validated typed project.
    pub fn create(root: impl AsRef<Path>, project: &Project) -> Result<Self> {
        let documents = format::encode(project)?;
        let root = root.as_ref();
        let created_root = !root.exists();
        if root.exists() {
            reject_symlink(root)?;
            if fs::read_dir(root)
                .map_err(|error| io(root, error))?
                .next()
                .transpose()
                .map_err(|error| io(root, error))?
                .is_some()
            {
                return Err(Error::DirectoryNotEmpty(root.to_owned()));
            }
        } else {
            fs::create_dir_all(root).map_err(|error| io(root, error))?;
        }
        let root = root.canonicalize().map_err(|error| io(root, error))?;
        let store = Self { root };
        let result = (|| {
            for directory in [
                "assets/media",
                "compositions",
                "events",
                "presets/samplers",
                "presets/effects",
                ".gaw/cache/audio",
                ".gaw/cache/waveforms",
                ".gaw/atomic",
            ] {
                let path = store.root.join(directory);
                fs::create_dir_all(&path).map_err(|error| io(&path, error))?;
            }
            store.apply_storage_unlocked(&diff(&BTreeMap::new(), &documents))
        })();
        if let Err(error) = result {
            if created_root {
                let _ = fs::remove_dir_all(&store.root);
            } else {
                for directory in ["assets", "compositions", "events", "presets", ".gaw"] {
                    let _ = fs::remove_dir_all(store.root.join(directory));
                }
            }
            return Err(error);
        }
        Ok(store)
    }

    pub fn create_default(
        root: impl AsRef<Path>,
        name: &str,
        bpm: f64,
        sample_rate: u32,
    ) -> Result<Self> {
        let bpm = gaw_core::Bpm::new(bpm)
            .map_err(|error| Error::InvalidTransaction(error.to_string()))?;
        let sample_rate = SampleRate::new(sample_rate)
            .map_err(|error| Error::InvalidTransaction(error.to_string()))?;
        Self::create(root, &Project::new(name, bpm, sample_rate))
    }

    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let supplied = root.as_ref();
        if !supplied.is_dir() {
            return Err(Error::ProjectNotFound(supplied.to_owned()));
        }
        reject_symlink(supplied)?;
        let root = supplied
            .canonicalize()
            .map_err(|error| io(supplied, error))?;
        let store = Self { root };
        let _write_lock = store.acquire_write_lock()?;
        store.recover_interrupted_write_unlocked()?;
        store.load_manifest_unlocked()?;
        Ok(store)
    }

    pub fn validate_path(root: impl AsRef<Path>) -> Result<ValidationReport> {
        let supplied = root.as_ref();
        if !supplied.is_dir() {
            return Err(Error::ProjectNotFound(supplied.to_owned()));
        }
        reject_symlink(supplied)?;
        let root = supplied
            .canonicalize()
            .map_err(|error| io(supplied, error))?;
        let store = Self { root };
        let _write_lock = store.acquire_write_lock()?;
        if let Err(error) = store.recover_interrupted_write_unlocked() {
            return Ok(ValidationReport::from_error(&store.root, &error));
        }
        store.validate_unlocked()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Reads only the strict project header and fragment manifest.
    ///
    /// This does not read or globally validate composition-local documents.
    pub fn load_manifest(&self) -> Result<ProjectManifest> {
        let _write_lock = self.acquire_write_lock()?;
        self.load_manifest_unlocked()
    }

    /// Reads only the strict asset index.
    ///
    /// Cross-asset and project references are validated by [`Self::load_project`].
    pub fn load_asset_index(&self) -> Result<AssetIndex> {
        let _write_lock = self.acquire_write_lock()?;
        let path = ProjectPath::new("assets/index.json")?;
        format::decode_asset_index(&self.read_json(&path)?)
    }

    /// Loads one structurally checked composition-local bundle.
    ///
    /// This performs one manifest read followed by exactly the composition,
    /// track, and automation reads declared for `composition_id`. It does not
    /// construct or globally validate a [`Project`].
    pub fn load_composition(&self, composition_id: CompositionId) -> Result<CompositionBundle> {
        let _write_lock = self.acquire_write_lock()?;
        let manifest = self.load_manifest_unlocked()?;
        let mut documents = format::Documents::new();
        for path in format::composition_paths(&manifest, composition_id)? {
            documents.insert(path.clone(), self.read_json(&path)?);
        }
        format::decode_composition_bundle(&manifest, composition_id, &documents)
    }

    /// Loads one strict, versioned event stream without reading other streams.
    pub fn load_event_data(&self, event_data_id: EventDataId) -> Result<EventData> {
        let _write_lock = self.acquire_write_lock()?;
        let manifest = self.load_manifest_unlocked()?;
        if !manifest.event_order.contains(&event_data_id) {
            return Err(Error::InvalidTransaction(format!(
                "project manifest does not contain event data {event_data_id}"
            )));
        }
        let path = ProjectPath::new(format!("events/{event_data_id}.json"))?;
        format::decode_event_data(&path, &self.read_json(&path)?)
    }

    /// Reassembles every canonical fragment into a fully validated core model.
    pub fn load_project(&self) -> Result<Project> {
        let _write_lock = self.acquire_write_lock()?;
        format::decode(&self.scan_documents_unlocked()?)
    }

    /// Saves a complete typed snapshot as one crash-atomic document diff.
    pub fn save_project(&self, project: &Project) -> Result<()> {
        let _write_lock = self.acquire_write_lock()?;
        self.reject_pending_recovery()?;
        let current = self.scan_documents_unlocked()?;
        let next = format::encode(project)?;
        self.apply_storage_unlocked(&diff(&current, &next))
    }

    /// Checkpoints a project that already includes every pending journal record.
    pub fn checkpoint_project(&self, project: &Project) -> Result<()> {
        let _write_lock = self.acquire_write_lock()?;
        let records = recovery::read(&self.recovery_path()?)?;
        let next = format::encode(project)?;
        if let Some(last) = records.last()
            && hash_snapshot(&next)? != last.after_snapshot_hash
        {
            return Err(Error::InvalidTransaction(
                "checkpoint does not include all pending recovery transactions".into(),
            ));
        }
        let current = self.scan_documents_unlocked()?;
        let current_hash = hash_snapshot(&current)?;
        if records
            .last()
            .is_some_and(|record| record.after_snapshot_hash == current_hash)
        {
            return recovery::clear(&self.recovery_path()?);
        }
        if let Some(first) = records.first()
            && current_hash != first.before_snapshot_hash
        {
            return Err(Error::InvalidTransaction(
                "canonical project changed while the editing session was open".into(),
            ));
        }
        self.apply_storage_unlocked(&diff(&current, &next))?;
        if records.is_empty() {
            Ok(())
        } else {
            recovery::clear(&self.recovery_path()?)
        }
    }

    /// Applies one core transaction and durably checkpoints one resulting state.
    pub fn commit_transaction(&self, transaction: &Transaction) -> Result<Project> {
        let _write_lock = self.acquire_write_lock()?;
        self.commit_transaction_unlocked(transaction)
    }

    fn commit_transaction_unlocked(&self, transaction: &Transaction) -> Result<Project> {
        if !recovery::read(&self.recovery_path()?)?.is_empty() {
            return Err(Error::InvalidTransaction(
                "pending recovery must be applied before committing a new transaction".into(),
            ));
        }
        let before_documents = self.scan_documents_unlocked()?;
        let mut after_project = format::decode(&before_documents)?;
        transaction.apply(&mut after_project)?;
        let after_documents = format::encode(&after_project)?;
        recovery::append(
            &self.recovery_path()?,
            transaction,
            hash_snapshot(&before_documents)?,
            hash_snapshot(&after_documents)?,
        )?;
        self.apply_storage_unlocked(&diff(&before_documents, &after_documents))?;
        recovery::clear(&self.recovery_path()?)?;
        Ok(after_project)
    }

    pub fn validate(&self) -> Result<ValidationReport> {
        let _write_lock = self.acquire_write_lock()?;
        self.validate_unlocked()
    }

    fn validate_unlocked(&self) -> Result<ValidationReport> {
        let project = match self
            .scan_documents_unlocked()
            .and_then(|documents| format::decode(&documents))
        {
            Ok(project) => project,
            Err(error) => return Ok(ValidationReport::from_error(&self.root, &error)),
        };
        let mut report = ValidationReport::default();
        self.validate_media(&project, &mut report)?;
        Ok(report)
    }

    /// Decodes audio into canonical WAV storage and adds or updates its typed asset.
    pub fn import_media(&self, source: impl AsRef<Path>) -> Result<ImportedMedia> {
        let _write_lock = self.acquire_write_lock()?;
        self.reject_pending_recovery()?;
        let project = format::decode(&self.scan_documents_unlocked()?)?;
        let source = source.as_ref();
        let original_filename = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::InvalidPath(source.display().to_string()))?
            .to_owned();
        let media_root = self.safe_target(&ProjectPath::new("assets/media")?)?;
        if !media_root.is_dir() {
            return Err(Error::InvalidPath(media_root.display().to_string()));
        }

        let mut temporary =
            tempfile::NamedTempFile::new_in(&media_root).map_err(|error| io(&media_root, error))?;
        transcode_to_canonical_wav(source, &mut temporary)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| io(temporary.path(), error))?;
        let byte_len = temporary
            .as_file()
            .metadata()
            .map_err(|error| io(temporary.path(), error))?
            .len();
        let content_hash = hash_file(temporary.path())?;

        let (sample_rate, layout, frames) = canonical_wav_metadata(temporary.path())?;

        let content_hash = ContentHash::new(content_hash)
            .map_err(|error| Error::InvalidMedia(error.to_string()))?;
        let relative_path =
            gaw_core::ProjectPath::new(format!("assets/media/{}.wav", content_hash.as_str()))
                .map_err(|error| Error::InvalidMedia(error.to_string()))?;
        let storage_path = ProjectPath::new(relative_path.as_str())?;
        let target = self.safe_target(&storage_path)?;
        match temporary.persist_noclobber(&target) {
            Ok(_) => sync_directory(&media_root)?,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                reject_symlink(&target)?;
                if !target.is_file() || hash_file(&target)? != content_hash.as_str() {
                    return Err(Error::InvalidTransaction(format!(
                        "existing media does not match content hash {content_hash}"
                    )));
                }
            }
            Err(error) => return Err(io(&target, error.error)),
        }

        let source_model = ImportedAudio {
            media_path: relative_path.clone(),
            original_filename: original_filename.clone(),
            content_hash: content_hash.clone(),
            sample_rate,
            layout,
            frames,
        };
        let (asset, command) = project
            .assets
            .iter()
            .find(|asset| {
                matches!(
                    &asset.definition,
                    AudioAssetDefinition::Imported(imported)
                        if imported.content_hash == content_hash
                )
            })
            .map_or_else(
                || {
                    let asset =
                        AudioAsset::imported(original_filename.clone(), source_model.clone());
                    (asset.clone(), Command::AddAsset { asset })
                },
                |existing| {
                    let mut asset = existing.clone();
                    asset.name.clone_from(&original_filename);
                    asset.definition = AudioAssetDefinition::Imported(source_model.clone());
                    (asset.clone(), Command::UpdateAsset { asset })
                },
            );
        self.commit_transaction_unlocked(&Transaction::named(
            format!("Import {original_filename}"),
            [command],
        ))?;
        Ok(ImportedMedia {
            asset_id: asset.id,
            content_hash,
            relative_path,
            original_filename,
            byte_len,
        })
    }

    /// Materializes confirmed regions as independent canonical WAV assets.
    ///
    /// The source asset is preserved. Regions must be non-empty, ordered, and
    /// contained by one imported canonical asset. Regions may overlap so a
    /// split can retain context around detected tempo boundaries.
    #[allow(clippy::too_many_lines)]
    pub fn split_imported_media(
        &self,
        asset_id: gaw_core::AssetId,
        regions: &[MediaRegion],
    ) -> Result<Vec<ImportedMedia>> {
        let _write_lock = self.acquire_write_lock()?;
        self.reject_pending_recovery()?;
        if regions.is_empty() {
            return Err(Error::InvalidMedia(
                "at least one tempo region is required".into(),
            ));
        }
        let project = format::decode(&self.scan_documents_unlocked()?)?;
        let source_asset = project
            .assets
            .iter()
            .find(|asset| asset.id == asset_id)
            .ok_or_else(|| Error::InvalidMedia(format!("audio asset {asset_id} does not exist")))?;
        let AudioAssetDefinition::Imported(source) = &source_asset.definition else {
            return Err(Error::InvalidMedia(
                "tempo regions can only be split from imported audio".into(),
            ));
        };
        validate_regions(regions, source.frames)?;

        let media_root = self.safe_target(&ProjectPath::new("assets/media")?)?;
        let source_path = self.safe_target(&ProjectPath::new(source.media_path.as_str())?)?;
        let mut reader = hound::WavReader::open(&source_path).map_err(invalid_media)?;
        let spec = reader.spec();
        let expected_channels = match source.layout {
            ChannelLayout::Mono => 1,
            ChannelLayout::Stereo => 2,
        };
        if spec.channels != expected_channels
            || spec.sample_rate != source.sample_rate.value()
            || spec.bits_per_sample != 32
            || spec.sample_format != hound::SampleFormat::Float
        {
            return Err(Error::InvalidMedia(
                "source asset is not a canonical float WAV".into(),
            ));
        }

        let channel_count = u64::from(spec.channels);
        let base_name = split_asset_base_name(&source_asset.name);
        let mut imported = Vec::with_capacity(regions.len());
        let mut assets = Vec::with_capacity(regions.len());

        for (index, region) in regions.iter().enumerate() {
            let start_frame = u32::try_from(region.range.start.0).map_err(|_| {
                Error::InvalidMedia("tempo region offset exceeds WAV limits".into())
            })?;
            reader.seek(start_frame).map_err(invalid_media)?;
            let mut samples = reader.samples::<f32>();

            let mut temporary = tempfile::NamedTempFile::new_in(&media_root)
                .map_err(|error| io(&media_root, error))?;
            let sample_count = region
                .range
                .length
                .0
                .checked_mul(channel_count)
                .ok_or_else(|| Error::InvalidMedia("tempo region length overflow".into()))?;
            {
                let mut writer = hound::WavWriter::new(
                    BufWriter::with_capacity(64 * 1024, temporary.as_file_mut()),
                    spec,
                )
                .map_err(invalid_media)?;
                for _ in 0..sample_count {
                    let sample = samples
                        .next()
                        .ok_or_else(|| Error::InvalidMedia("source WAV ended unexpectedly".into()))?
                        .map_err(invalid_media)?;
                    if !sample.is_finite() {
                        return Err(Error::InvalidMedia(
                            "source WAV contains a non-finite sample".into(),
                        ));
                    }
                    writer.write_sample(sample).map_err(invalid_media)?;
                }
                writer.finalize().map_err(invalid_media)?;
            }
            temporary
                .as_file()
                .sync_all()
                .map_err(|error| io(temporary.path(), error))?;

            let byte_len = temporary
                .as_file()
                .metadata()
                .map_err(|error| io(temporary.path(), error))?
                .len();
            let content_hash = ContentHash::new(hash_file(temporary.path())?)
                .map_err(|error| Error::InvalidMedia(error.to_string()))?;
            let relative_path =
                gaw_core::ProjectPath::new(format!("assets/media/{}.wav", content_hash.as_str()))
                    .map_err(|error| Error::InvalidMedia(error.to_string()))?;
            let target = self.safe_target(&ProjectPath::new(relative_path.as_str())?)?;
            match temporary.persist_noclobber(&target) {
                Ok(_) => sync_directory(&media_root)?,
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    reject_symlink(&target)?;
                    if !target.is_file() || hash_file(&target)? != content_hash.as_str() {
                        return Err(Error::InvalidTransaction(format!(
                            "existing media does not match content hash {content_hash}"
                        )));
                    }
                }
                Err(error) => return Err(io(&target, error.error)),
            }

            let name = format!("{base_name} {}", index + 1);
            let original_filename = format!("{name}.wav");
            let definition = ImportedAudio {
                media_path: relative_path.clone(),
                original_filename: original_filename.clone(),
                content_hash: content_hash.clone(),
                sample_rate: source.sample_rate,
                layout: source.layout,
                frames: region.range.length,
            };
            let mut asset = AudioAsset::imported(name, definition);
            asset.tempo = Some(AssetTempo {
                bpm: region.bpm,
                first_beat: Seconds::new(0.0).map_err(invalid_media)?,
            });
            imported.push(ImportedMedia {
                asset_id: asset.id,
                content_hash,
                relative_path,
                original_filename,
                byte_len,
            });
            assets.push(asset);
        }

        let commands = assets.into_iter().map(|asset| Command::AddAsset { asset });
        self.commit_transaction_unlocked(&Transaction::named(
            format!("Split {} into tempo regions", source_asset.name),
            commands,
        ))?;
        Ok(imported)
    }

    /// Opens one content-addressed media file without exposing arbitrary paths.
    ///
    /// The caller supplies the canonical asset reference and expected content
    /// hash. The path must be exactly `assets/media/<hash>.<extension>` and every
    /// component is checked for symlinks. Full byte hashing remains part of
    /// [`Self::validate`] rather than the latency-sensitive read path.
    pub fn open_media(
        &self,
        path: &gaw_core::ProjectPath,
        expected_hash: &ContentHash,
    ) -> Result<File> {
        let path = ProjectPath::new(path.as_str())?;
        let parts = path.as_str().split('/').collect::<Vec<_>>();
        let ["assets", "media", file] = parts.as_slice() else {
            return Err(Error::InvalidMedia(
                "media path must be inside assets/media".into(),
            ));
        };
        let Some(extension) = file
            .strip_prefix(expected_hash.as_str())
            .and_then(|suffix| suffix.strip_prefix('.'))
        else {
            return Err(Error::InvalidMedia(
                "media filename does not match its content hash".into(),
            ));
        };
        if extension.is_empty() || !extension.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(Error::InvalidMedia("invalid media extension".into()));
        }
        let target = self.safe_target(&path)?;
        let metadata = fs::metadata(&target).map_err(|error| io(&target, error))?;
        if !metadata.is_file() {
            return Err(Error::InvalidMedia(
                "media target must be a regular file".into(),
            ));
        }
        File::open(&target).map_err(|error| io(&target, error))
    }

    /// Lists sampler preset keys in deterministic order.
    pub fn list_sampler_presets(&self) -> Result<Vec<PresetId>> {
        self.list_presets(PresetKind::Sampler)
    }

    /// Loads and validates one strict sampler preset document.
    pub fn load_sampler_preset(&self, id: &PresetId) -> Result<SamplerPreset> {
        self.load_preset(PresetKind::Sampler, id, SamplerPreset::validate)
    }

    /// Atomically saves one validated sampler preset document.
    pub fn save_sampler_preset(&self, id: &PresetId, preset: &SamplerPreset) -> Result<()> {
        self.save_preset(PresetKind::Sampler, id, preset, SamplerPreset::validate)
    }

    /// Deletes one sampler preset, returning whether it existed.
    pub fn delete_sampler_preset(&self, id: &PresetId) -> Result<bool> {
        self.delete_preset(PresetKind::Sampler, id)
    }

    /// Lists effect preset keys in deterministic order.
    pub fn list_effect_presets(&self) -> Result<Vec<PresetId>> {
        self.list_presets(PresetKind::Effect)
    }

    /// Loads and validates one strict effect preset document.
    pub fn load_effect_preset(&self, id: &PresetId) -> Result<EffectPreset> {
        self.load_preset(PresetKind::Effect, id, EffectPreset::validate)
    }

    /// Atomically saves one validated effect preset document.
    pub fn save_effect_preset(&self, id: &PresetId, preset: &EffectPreset) -> Result<()> {
        self.save_preset(PresetKind::Effect, id, preset, EffectPreset::validate)
    }

    /// Deletes one effect preset, returning whether it existed.
    pub fn delete_effect_preset(&self, id: &PresetId) -> Result<bool> {
        self.delete_preset(PresetKind::Effect, id)
    }

    /// Journals a validated core transaction without applying it, for crash handoff.
    pub fn append_recovery(&self, transaction: &Transaction) -> Result<RecoveryRecord> {
        let _write_lock = self.acquire_write_lock()?;
        self.append_recovery_unlocked(None, transaction)
    }

    /// Journals a transaction only if the durable snapshot plus pending journal
    /// still represents the caller's in-memory project.
    pub fn append_recovery_for_project(
        &self,
        expected: &Project,
        transaction: &Transaction,
    ) -> Result<RecoveryRecord> {
        let _write_lock = self.acquire_write_lock()?;
        self.append_recovery_unlocked(Some(expected), transaction)
    }

    fn append_recovery_unlocked(
        &self,
        expected: Option<&Project>,
        transaction: &Transaction,
    ) -> Result<RecoveryRecord> {
        let documents = self.scan_documents_unlocked()?;
        let base_hash = hash_snapshot(&documents)?;
        let mut project = format::decode(&documents)?;
        let mut expected_hash = base_hash;
        for record in recovery::read(&self.recovery_path()?)? {
            if record.before_snapshot_hash != expected_hash {
                return Err(Error::InvalidTransaction(
                    "recovery journal does not belong to the current snapshot".into(),
                ));
            }
            record.transaction.apply(&mut project)?;
            expected_hash = hash_snapshot(&format::encode(&project)?)?;
            if record.after_snapshot_hash != expected_hash {
                return Err(Error::InvalidTransaction(
                    "recovery transaction does not produce its recorded snapshot".into(),
                ));
            }
        }
        let before_snapshot_hash = expected_hash;
        if let Some(expected) = expected
            && hash_snapshot(&format::encode(expected)?)? != before_snapshot_hash
        {
            return Err(Error::InvalidTransaction(
                "editing session is stale; reload the canonical project before retrying".into(),
            ));
        }
        transaction.apply(&mut project)?;
        let after_snapshot_hash = hash_snapshot(&format::encode(&project)?)?;
        recovery::append(
            &self.recovery_path()?,
            transaction,
            before_snapshot_hash,
            after_snapshot_hash,
        )
    }

    pub fn pending_recovery(&self) -> Result<Vec<RecoveryRecord>> {
        let _write_lock = self.acquire_write_lock()?;
        recovery::read(&self.recovery_path()?)
    }

    pub fn recover(&self) -> Result<usize> {
        let _write_lock = self.acquire_write_lock()?;
        let records = recovery::read(&self.recovery_path()?)?;
        let mut documents = self.scan_documents_unlocked()?;
        let mut project = format::decode(&documents)?;
        let mut current_hash = hash_snapshot(&documents)?;
        let start = if records
            .first()
            .is_none_or(|record| current_hash == record.before_snapshot_hash)
        {
            0
        } else if let Some(position) = records
            .iter()
            .rposition(|record| current_hash == record.after_snapshot_hash)
        {
            position + 1
        } else {
            return Err(Error::InvalidTransaction(
                "recovery journal does not belong to the current snapshot".into(),
            ));
        };
        for record in records.iter().skip(start) {
            if current_hash != record.before_snapshot_hash {
                return Err(Error::InvalidTransaction(
                    "recovery journal does not belong to the current snapshot".into(),
                ));
            }
            let mut next_project = project.clone();
            record.transaction.apply(&mut next_project)?;
            let next_documents = format::encode(&next_project)?;
            let next_hash = hash_snapshot(&next_documents)?;
            if next_hash != record.after_snapshot_hash {
                return Err(Error::InvalidTransaction(
                    "recovery transaction does not produce its recorded snapshot".into(),
                ));
            }
            documents = next_documents;
            project = next_project;
            current_hash = next_hash;
        }
        let current_documents = self.scan_documents_unlocked()?;
        self.apply_storage_unlocked(&diff(&current_documents, &documents))?;
        recovery::clear(&self.recovery_path()?)?;
        Ok(records.len().saturating_sub(start))
    }

    pub fn clear_recovery(&self) -> Result<()> {
        let _write_lock = self.acquire_write_lock()?;
        recovery::clear(&self.recovery_path()?)
    }

    fn load_manifest_unlocked(&self) -> Result<ProjectManifest> {
        let path = ProjectPath::new("project.json")?;
        format::decode_manifest(&self.read_json(&path)?)
    }

    fn scan_documents_unlocked(&self) -> Result<format::Documents> {
        let mut documents = BTreeMap::new();
        self.collect_json(Path::new(""), &mut documents)?;
        Ok(documents)
    }

    fn collect_json(&self, relative: &Path, documents: &mut format::Documents) -> Result<()> {
        let directory = self.root.join(relative);
        for entry in fs::read_dir(&directory).map_err(|error| io(&directory, error))? {
            let entry = entry.map_err(|error| io(&directory, error))?;
            let file_type = entry.file_type().map_err(|error| io(entry.path(), error))?;
            if file_type.is_symlink() {
                return Err(Error::Symlink(entry.path()));
            }
            let path = relative.join(entry.file_name());
            if path.starts_with(".gaw")
                || path.starts_with("assets/media")
                || path.starts_with("presets")
            {
                continue;
            }
            if file_type.is_dir() {
                self.collect_json(&path, documents)?;
            } else if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                let portable = path
                    .to_str()
                    .ok_or_else(|| Error::InvalidPath(path.display().to_string()))?
                    .replace(std::path::MAIN_SEPARATOR, "/");
                let project_path = ProjectPath::new(portable)?;
                if !project_path.is_canonical_json() {
                    return Err(Error::InvalidTransaction(format!(
                        "unexpected JSON document {project_path}"
                    )));
                }
                documents.insert(project_path.clone(), self.read_json(&project_path)?);
            }
        }
        Ok(())
    }

    fn read_json(&self, path: &ProjectPath) -> Result<Value> {
        let target = self.safe_target(path)?;
        #[cfg(test)]
        JSON_READS.with(|reads| {
            reads.borrow_mut().push((
                path.to_string(),
                fs::metadata(&target).map_or(0, |value| value.len()),
            ));
        });
        serde_json::from_reader(File::open(&target).map_err(|error| io(&target, error))?).map_err(
            |source| Error::Json {
                path: target,
                source,
            },
        )
    }

    fn apply_storage_unlocked(&self, transaction: &StorageTransaction) -> Result<()> {
        self.recover_interrupted_write_unlocked()?;
        format::check_schema(transaction.schema_version.into())?;
        if transaction.operations.is_empty() {
            return Ok(());
        }
        let current = self.scan_documents_unlocked()?;
        let mut prospective = current.clone();
        let mut seen = BTreeSet::new();
        for operation in &transaction.operations {
            let path = operation.path();
            if !path.is_canonical_json() || !seen.insert(path.clone()) {
                return Err(Error::InvalidTransaction(format!(
                    "invalid or duplicate canonical path {path}"
                )));
            }
            self.safe_target(path)?;
            match operation {
                StorageOperation::Write { path, document } => {
                    prospective.insert(path.clone(), document.clone());
                }
                StorageOperation::Delete { path } => {
                    prospective.remove(path);
                }
            }
        }
        format::decode(&prospective)?;

        let transaction_root = self.atomic_root()?.join(unique_suffix());
        let staged_root = transaction_root.join("staged");
        let backup_root = transaction_root.join("backup");
        fs::create_dir_all(&staged_root).map_err(|error| io(&staged_root, error))?;
        fs::create_dir_all(&backup_root).map_err(|error| io(&backup_root, error))?;
        let mut manifest = AtomicManifest {
            schema_version: SCHEMA_VERSION,
            entries: Vec::new(),
        };
        for operation in &transaction.operations {
            let path = operation.path();
            let target = self.safe_target(path)?;
            manifest.entries.push(AtomicEntry {
                path: path.clone(),
                existed: target.exists(),
                write: matches!(operation, StorageOperation::Write { .. }),
            });
            if let StorageOperation::Write { document, .. } = operation {
                write_json_file(&staged_root.join(path.as_path()), document)?;
            }
        }
        write_json_file(&transaction_root.join("manifest.json"), &manifest)?;
        sync_directory_tree(&transaction_root)?;
        sync_directory(&self.atomic_root()?)?;

        let result = (|| {
            for entry in &manifest.entries {
                let target = self.safe_target(&entry.path)?;
                if entry.existed {
                    let backup = backup_root.join(entry.path.as_path());
                    make_parent(&backup)?;
                    fs::rename(&target, &backup).map_err(|error| io(&target, error))?;
                }
                if entry.write {
                    let staged = staged_root.join(entry.path.as_path());
                    make_parent(&target)?;
                    fs::rename(&staged, &target).map_err(|error| io(&target, error))?;
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            let _ = self.rollback(&transaction_root, &manifest);
            return Err(error);
        }
        sync_directory_tree(&backup_root)?;
        let mut target_parents = BTreeSet::new();
        for entry in &manifest.entries {
            let mut parent = self.safe_target(&entry.path)?.parent().map(Path::to_owned);
            while let Some(directory) = parent {
                if !directory.starts_with(&self.root) {
                    break;
                }
                target_parents.insert(directory.clone());
                if directory == self.root {
                    break;
                }
                parent = directory.parent().map(Path::to_owned);
            }
        }
        for parent in target_parents {
            sync_directory(&parent)?;
        }
        let committed = transaction_root.with_extension("committed");
        let marker = File::create(&committed).map_err(|error| io(&committed, error))?;
        marker.sync_all().map_err(|error| io(&committed, error))?;
        sync_directory(&self.atomic_root()?)?;
        fs::remove_dir_all(&transaction_root).map_err(|error| io(&transaction_root, error))?;
        fs::remove_file(&committed).map_err(|error| io(&committed, error))?;
        sync_directory(&self.atomic_root()?)
    }

    fn recover_interrupted_write_unlocked(&self) -> Result<()> {
        let atomic_root = self.atomic_root()?;
        let entries = match fs::read_dir(&atomic_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io(&atomic_root, error)),
        };
        for entry in entries {
            let path = entry.map_err(|error| io(&atomic_root, error))?.path();
            if !path.exists() {
                continue;
            }
            if path
                .extension()
                .is_some_and(|extension| extension == "committed")
            {
                let transaction = path.with_extension("");
                if transaction.exists() {
                    fs::remove_dir_all(&transaction).map_err(|error| io(&transaction, error))?;
                }
                fs::remove_file(&path).map_err(|error| io(&path, error))?;
                continue;
            }
            if !path.is_dir() {
                return Err(Error::InvalidTransaction(format!(
                    "unexpected atomic-write artifact {}",
                    path.display()
                )));
            }
            let manifest_path = path.join("manifest.json");
            if !manifest_path.exists() {
                fs::remove_dir_all(&path).map_err(|error| io(&path, error))?;
                continue;
            }
            let manifest: AtomicManifest = serde_json::from_reader(
                File::open(&manifest_path).map_err(|error| io(&manifest_path, error))?,
            )
            .map_err(|source| Error::Json {
                path: manifest_path,
                source,
            })?;
            format::check_schema(manifest.schema_version.into())?;
            validate_atomic_manifest(&manifest)?;
            let committed = path.with_extension("committed");
            if committed.exists() {
                fs::remove_dir_all(&path).map_err(|error| io(&path, error))?;
                fs::remove_file(&committed).map_err(|error| io(&committed, error))?;
            } else {
                self.rollback(&path, &manifest)?;
            }
        }
        sync_directory(&atomic_root)
    }

    fn rollback(&self, transaction_root: &Path, manifest: &AtomicManifest) -> Result<()> {
        let backup_root = transaction_root.join("backup");
        for entry in manifest.entries.iter().rev() {
            let target = self.safe_target(&entry.path)?;
            let backup = backup_root.join(entry.path.as_path());
            if entry.existed && backup.exists() {
                if target.exists() {
                    fs::remove_file(&target).map_err(|error| io(&target, error))?;
                }
                make_parent(&target)?;
                fs::rename(&backup, &target).map_err(|error| io(&target, error))?;
            } else if !entry.existed && target.exists() {
                fs::remove_file(&target).map_err(|error| io(&target, error))?;
            }
        }
        fs::remove_dir_all(transaction_root).map_err(|error| io(transaction_root, error))
    }

    fn validate_media(&self, project: &Project, report: &mut ValidationReport) -> Result<()> {
        for asset in &project.assets {
            if let AudioAssetDefinition::Imported(imported) = &asset.definition {
                self.validate_media_reference(
                    &imported.media_path,
                    &imported.content_hash,
                    imported.sample_rate,
                    imported.layout,
                    imported.frames,
                    &mut report.errors,
                )?;
            }
            for revision in &asset.revisions {
                self.validate_media_reference(
                    &revision.media_path,
                    &revision.content_hash,
                    revision.render_context.sample_rate,
                    revision.render_context.layout,
                    revision.frames,
                    &mut report.errors,
                )?;
            }
        }
        Ok(())
    }

    fn validate_media_reference(
        &self,
        core_path: &gaw_core::ProjectPath,
        hash: &ContentHash,
        expected_sample_rate: SampleRate,
        expected_layout: ChannelLayout,
        expected_frames: FrameCount,
        errors: &mut Vec<ValidationIssue>,
    ) -> Result<()> {
        let path = ProjectPath::new(core_path.as_str())?;
        if !path.as_str().starts_with("assets/media/") {
            errors.push(ValidationIssue {
                path: path.to_string(),
                message: "media path is outside assets/media".into(),
            });
            return Ok(());
        }
        let target = self.safe_target(&path)?;
        if let Err(error) = self.open_media(core_path, hash) {
            errors.push(ValidationIssue {
                path: path.to_string(),
                message: error.to_string(),
            });
        } else if !target.is_file() {
            errors.push(ValidationIssue {
                path: path.to_string(),
                message: "referenced media file is missing".into(),
            });
        } else if hash_file(&target)? != hash.as_str() {
            errors.push(ValidationIssue {
                path: path.to_string(),
                message: "referenced media content hash does not match".into(),
            });
        } else {
            match hound::WavReader::open(&target) {
                Ok(reader) => {
                    let spec = reader.spec();
                    let actual_layout = match spec.channels {
                        1 => Some(ChannelLayout::Mono),
                        2 => Some(ChannelLayout::Stereo),
                        _ => None,
                    };
                    let actual_frames = FrameCount(u64::from(reader.duration()));
                    if spec.sample_rate != expected_sample_rate.value()
                        || actual_layout != Some(expected_layout)
                        || actual_frames != expected_frames
                    {
                        errors.push(ValidationIssue {
                            path: path.to_string(),
                            message: format!(
                                "WAV metadata does not match asset metadata: expected {} Hz, {:?}, {} frames; found {} Hz, {} channels, {} frames",
                                expected_sample_rate.value(),
                                expected_layout,
                                expected_frames.0,
                                spec.sample_rate,
                                spec.channels,
                                actual_frames.0,
                            ),
                        });
                    }
                }
                Err(error) => errors.push(ValidationIssue {
                    path: path.to_string(),
                    message: format!("referenced media is not a readable WAV: {error}"),
                }),
            }
        }
        Ok(())
    }

    fn safe_target(&self, path: &ProjectPath) -> Result<PathBuf> {
        let mut current = self.root.clone();
        for component in path.as_path().components() {
            current.push(component);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(Error::Symlink(current));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io(&current, error)),
            }
        }
        Ok(current)
    }

    fn recovery_path(&self) -> Result<PathBuf> {
        self.safe_target(&ProjectPath::new(".gaw/recovery.journal")?)
    }

    fn atomic_root(&self) -> Result<PathBuf> {
        self.safe_target(&ProjectPath::new(".gaw/atomic")?)
    }

    fn acquire_write_lock(&self) -> Result<File> {
        let runtime = self.root.join(".gaw");
        match fs::symlink_metadata(&runtime) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(Error::Symlink(runtime));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(Error::InvalidPath(runtime.display().to_string()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&runtime).map_err(|error| io(&runtime, error))?;
            }
            Err(error) => return Err(io(&runtime, error)),
        }
        let path = self.safe_target(&ProjectPath::new(".gaw/write.lock")?)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|error| io(&path, error))?;
        file.lock().map_err(|error| io(&path, error))?;
        Ok(file)
    }

    fn preset_path(kind: PresetKind, id: &PresetId) -> Result<ProjectPath> {
        ProjectPath::new(format!("{}/{}.json", kind.directory(), id.as_str()))
    }

    fn list_presets(&self, kind: PresetKind) -> Result<Vec<PresetId>> {
        let _write_lock = self.acquire_write_lock()?;
        let directory_path = ProjectPath::new(kind.directory())?;
        let directory = self.safe_target(&directory_path)?;
        if !directory.is_dir() {
            return Err(Error::InvalidPath(directory.display().to_string()));
        }
        let mut ids = Vec::new();
        for entry in fs::read_dir(&directory).map_err(|error| io(&directory, error))? {
            let entry = entry.map_err(|error| io(&directory, error))?;
            let file_type = entry.file_type().map_err(|error| io(entry.path(), error))?;
            if file_type.is_symlink() {
                return Err(Error::Symlink(entry.path()));
            }
            let file_name = entry
                .file_name()
                .into_string()
                .map_err(|name| Error::InvalidPreset(name.to_string_lossy().into_owned()))?;
            if file_name.starts_with(".gaw-preset-") {
                continue;
            }
            let Some(id) = file_name.strip_suffix(".json") else {
                return Err(Error::InvalidPreset(format!(
                    "unexpected preset-library entry {file_name}"
                )));
            };
            if !file_type.is_file() {
                return Err(Error::InvalidPreset(format!(
                    "preset {file_name} is not a regular file"
                )));
            }
            ids.push(PresetId::new(id)?);
        }
        ids.sort();
        Ok(ids)
    }

    fn load_preset<T>(
        &self,
        kind: PresetKind,
        id: &PresetId,
        validate: impl FnOnce(&T) -> std::result::Result<(), gaw_core::ModelError>,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let _write_lock = self.acquire_write_lock()?;
        let path = Self::preset_path(kind, id)?;
        let target = self.safe_target(&path)?;
        if !fs::metadata(&target)
            .map_err(|error| io(&target, error))?
            .is_file()
        {
            return Err(Error::InvalidPreset(format!(
                "{path} is not a regular file"
            )));
        }
        let preset =
            serde_json::from_reader(File::open(&target).map_err(|error| io(&target, error))?)
                .map_err(|source| Error::Json {
                    path: target,
                    source,
                })?;
        validate(&preset).map_err(|error| Error::InvalidPreset(error.to_string()))?;
        Ok(preset)
    }

    fn save_preset<T>(
        &self,
        kind: PresetKind,
        id: &PresetId,
        preset: &T,
        validate: impl FnOnce(&T) -> std::result::Result<(), gaw_core::ModelError>,
    ) -> Result<()>
    where
        T: Serialize,
    {
        validate(preset).map_err(|error| Error::InvalidPreset(error.to_string()))?;
        let _write_lock = self.acquire_write_lock()?;
        let path = Self::preset_path(kind, id)?;
        let target = self.safe_target(&path)?;
        let directory = target
            .parent()
            .ok_or_else(|| Error::InvalidPath(target.display().to_string()))?;
        if !directory.is_dir() {
            return Err(Error::InvalidPath(directory.display().to_string()));
        }
        let mut temporary = tempfile::Builder::new()
            .prefix(".gaw-preset-")
            .tempfile_in(directory)
            .map_err(|error| io(directory, error))?;
        serde_json::to_writer_pretty(temporary.as_file_mut(), preset).map_err(|source| {
            Error::Json {
                path: target.clone(),
                source,
            }
        })?;
        temporary
            .as_file_mut()
            .write_all(b"\n")
            .map_err(|error| io(temporary.path(), error))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| io(temporary.path(), error))?;
        temporary
            .persist(&target)
            .map_err(|error| io(&target, error.error))?;
        sync_directory(directory)
    }

    fn delete_preset(&self, kind: PresetKind, id: &PresetId) -> Result<bool> {
        let _write_lock = self.acquire_write_lock()?;
        let path = Self::preset_path(kind, id)?;
        let target = self.safe_target(&path)?;
        if !target.exists() {
            return Ok(false);
        }
        if !target.is_file() {
            return Err(Error::InvalidPreset(format!(
                "{path} is not a regular file"
            )));
        }
        fs::remove_file(&target).map_err(|error| io(&target, error))?;
        sync_directory(
            target
                .parent()
                .ok_or_else(|| Error::InvalidPath(target.display().to_string()))?,
        )?;
        Ok(true)
    }

    fn reject_pending_recovery(&self) -> Result<()> {
        if recovery::read(&self.recovery_path()?)?.is_empty() {
            Ok(())
        } else {
            Err(Error::InvalidTransaction(
                "pending recovery must be applied before mutating the project".into(),
            ))
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AtomicManifest {
    schema_version: u32,
    entries: Vec<AtomicEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AtomicEntry {
    path: ProjectPath,
    existed: bool,
    write: bool,
}

fn validate_atomic_manifest(manifest: &AtomicManifest) -> Result<()> {
    let mut paths = BTreeSet::new();
    for entry in &manifest.entries {
        if !entry.path.is_canonical_json() || !paths.insert(entry.path.clone()) {
            return Err(Error::InvalidTransaction(format!(
                "invalid or duplicate atomic-write path {}",
                entry.path
            )));
        }
    }
    Ok(())
}

fn diff(before: &format::Documents, after: &format::Documents) -> StorageTransaction {
    let mut operations = Vec::new();
    for path in before.keys() {
        if !after.contains_key(path) {
            operations.push(StorageOperation::Delete { path: path.clone() });
        }
    }
    for (path, document) in after {
        if before.get(path) != Some(document) {
            operations.push(StorageOperation::Write {
                path: path.clone(),
                document: document.clone(),
            });
        }
    }
    StorageTransaction {
        schema_version: SCHEMA_VERSION,
        operations,
    }
}

fn write_json_file(path: &Path, document: &impl Serialize) -> Result<()> {
    make_parent(path)?;
    let mut file = File::create(path).map_err(|error| io(path, error))?;
    serde_json::to_writer_pretty(&mut file, document).map_err(|source| Error::Json {
        path: path.to_owned(),
        source,
    })?;
    file.write_all(b"\n").map_err(|error| io(path, error))?;
    file.sync_all().map_err(|error| io(path, error))
}

fn make_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| io(parent, error))?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| io(path, error))
}

fn sync_directory_tree(path: &Path) -> Result<()> {
    for entry in fs::read_dir(path).map_err(|error| io(path, error))? {
        let entry = entry.map_err(|error| io(path, error))?;
        if entry
            .file_type()
            .map_err(|error| io(entry.path(), error))?
            .is_dir()
        {
            sync_directory_tree(&entry.path())?;
        }
    }
    sync_directory(path)
}

fn reject_symlink(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).map_err(|error| io(path, error))?;
    if metadata.file_type().is_symlink() {
        Err(Error::Symlink(path.to_owned()))
    } else {
        Ok(())
    }
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = TRANSACTION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}{counter:x}")
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).map_err(|error| io(path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| io(path, error))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_regions(regions: &[MediaRegion], source_frames: FrameCount) -> Result<()> {
    let mut prior_start = 0_u64;
    for region in regions {
        let start = region.range.start.0;
        let end = start
            .checked_add(region.range.length.0)
            .ok_or_else(|| Error::InvalidMedia("tempo region range overflow".into()))?;
        if region.range.length.0 == 0 {
            return Err(Error::InvalidMedia(
                "tempo regions must contain audio".into(),
            ));
        }
        if start < prior_start {
            return Err(Error::InvalidMedia(
                "tempo regions must be ordered by source position".into(),
            ));
        }
        if end > source_frames.0 {
            return Err(Error::InvalidMedia(
                "tempo region exceeds the source asset".into(),
            ));
        }
        prior_start = start;
    }
    Ok(())
}

fn split_asset_base_name(name: &str) -> String {
    Path::new(name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.trim().is_empty())
        .unwrap_or("Region")
        .to_owned()
}

fn transcode_to_canonical_wav(
    source: &Path,
    temporary: &mut tempfile::NamedTempFile,
) -> Result<()> {
    let input = open_regular_file(source)?;

    let stream = MediaSourceStream::new(Box::new(input), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(extension) = source.extension().and_then(|value| value.to_str()) {
        hint.with_extension(extension);
    }
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            stream,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(invalid_media)?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| Error::InvalidMedia("source has no audio track".into()))?;
    let track_id = track.id;
    let codec_parameters = track
        .codec_params
        .as_ref()
        .and_then(|parameters| parameters.audio())
        .ok_or_else(|| Error::InvalidMedia("audio track has no codec parameters".into()))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(codec_parameters, &AudioDecoderOptions::default())
        .map_err(invalid_media)?;

    reset_temporary(temporary)?;

    let mut samples = Vec::<f32>::new();
    let (sample_rate, channels, first_frames) = loop {
        let packet = format
            .next_packet()
            .map_err(invalid_media)?
            .ok_or_else(|| Error::InvalidMedia("source contains no decodable audio".into()))?;
        if packet.track_id != track_id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(audio) => audio,
            Err(DecodeError::DecodeError(_) | DecodeError::IoError(_)) => continue,
            Err(error) => return Err(invalid_media(error)),
        };
        let channels = supported_channel_count(audio.spec().channels().count())?;
        let sample_rate = supported_sample_rate(audio.spec().rate(), channels)?;
        samples.resize(audio.samples_interleaved(), 0.0);
        audio.copy_to_slice_interleaved(&mut samples);
        break (sample_rate, channels, decoded_frame_count(audio.frames())?);
    };
    let mut writer = hound::WavWriter::new(
        BufWriter::with_capacity(64 * 1024, temporary.as_file_mut()),
        hound::WavSpec {
            channels: u16::try_from(channels).expect("mono or stereo fits in u16"),
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        },
    )
    .map_err(invalid_media)?;
    let mut decoded_frames = extend_decoded_frames(0, first_frames, channels)?;
    write_float_samples(&mut writer, &samples)?;

    while let Some(packet) = format.next_packet().map_err(invalid_media)? {
        if packet.track_id != track_id {
            continue;
        }
        let audio = match decoder.decode(&packet) {
            Ok(audio) => audio,
            Err(DecodeError::DecodeError(_) | DecodeError::IoError(_)) => continue,
            Err(error) => return Err(invalid_media(error)),
        };
        let current_channels = supported_channel_count(audio.spec().channels().count())?;
        let current_spec = (
            supported_sample_rate(audio.spec().rate(), current_channels)?,
            current_channels,
        );
        if current_spec != (sample_rate, channels) {
            return Err(Error::InvalidMedia(
                "sample rate or channel layout changes within the source".into(),
            ));
        }
        samples.resize(audio.samples_interleaved(), 0.0);
        audio.copy_to_slice_interleaved(&mut samples);
        decoded_frames = extend_decoded_frames(
            decoded_frames,
            decoded_frame_count(audio.frames())?,
            channels,
        )?;
        write_float_samples(&mut writer, &samples)?;
    }
    if decoded_frames == 0 {
        return Err(Error::InvalidMedia(
            "source contains no decodable audio".into(),
        ));
    }
    writer.finalize().map_err(invalid_media)?;
    Ok(())
}

fn open_regular_file(source: &Path) -> Result<File> {
    let input = File::open(source).map_err(|error| io(source, error))?;
    if input
        .metadata()
        .map_err(|error| io(source, error))?
        .is_file()
    {
        Ok(input)
    } else {
        Err(Error::InvalidMedia("source must be a regular file".into()))
    }
}

fn reset_temporary(temporary: &mut tempfile::NamedTempFile) -> Result<()> {
    temporary
        .as_file_mut()
        .set_len(0)
        .map_err(|error| io(temporary.path(), error))?;
    temporary
        .as_file_mut()
        .seek(std::io::SeekFrom::Start(0))
        .map_err(|error| io(temporary.path(), error))?;
    Ok(())
}

fn canonical_wav_metadata(path: &Path) -> Result<(SampleRate, ChannelLayout, FrameCount)> {
    let reader = hound::WavReader::open(path).map_err(invalid_media)?;
    let spec = reader.spec();
    let layout = match spec.channels {
        1 => ChannelLayout::Mono,
        2 => ChannelLayout::Stereo,
        channels => {
            return Err(Error::InvalidMedia(format!(
                "{channels}-channel WAV files are not supported"
            )));
        }
    };
    let sample_rate = SampleRate::new(spec.sample_rate).map_err(invalid_media)?;
    Ok((
        sample_rate,
        layout,
        FrameCount(u64::from(reader.duration())),
    ))
}

fn supported_channel_count(channels: usize) -> Result<usize> {
    if matches!(channels, 1 | 2) {
        Ok(channels)
    } else {
        Err(Error::InvalidMedia(format!(
            "{channels}-channel audio is not supported"
        )))
    }
}

fn supported_sample_rate(sample_rate: u32, channels: usize) -> Result<u32> {
    let channels = u32::try_from(channels)
        .map_err(|error| Error::InvalidMedia(format!("channel count is too large: {error}")))?;
    let bytes_per_frame = 4_u32
        .checked_mul(channels)
        .ok_or_else(|| Error::InvalidMedia("WAV frame size overflow".into()))?;
    if sample_rate > 0 && sample_rate.checked_mul(bytes_per_frame).is_some() {
        Ok(sample_rate)
    } else {
        Err(Error::InvalidMedia(format!(
            "unsupported sample rate {sample_rate} Hz"
        )))
    }
}

fn decoded_frame_count(frames: usize) -> Result<u64> {
    u64::try_from(frames)
        .map_err(|error| Error::InvalidMedia(format!("decoded frame count is too large: {error}")))
}

fn extend_decoded_frames(current: u64, additional: u64, channels: usize) -> Result<u64> {
    const MAX_WAV_DATA_BYTES: u64 = u32::MAX as u64 - 128;
    let total = current
        .checked_add(additional)
        .ok_or_else(|| Error::InvalidMedia("decoded frame count overflow".into()))?;
    let data_bytes = total
        .checked_mul(u64::try_from(channels).map_err(invalid_media)?)
        .and_then(|samples| samples.checked_mul(4))
        .ok_or_else(|| Error::InvalidMedia("canonical WAV size overflow".into()))?;
    if data_bytes <= MAX_WAV_DATA_BYTES {
        Ok(total)
    } else {
        Err(Error::InvalidMedia(
            "canonical WAV exceeds the 4 GiB WAV limit".into(),
        ))
    }
}

fn write_float_samples<W: Write + Seek>(
    writer: &mut hound::WavWriter<W>,
    samples: &[f32],
) -> Result<()> {
    for sample in samples {
        writer
            .write_sample(if sample.is_finite() { *sample } else { 0.0 })
            .map_err(invalid_media)?;
    }
    Ok(())
}

fn invalid_media(error: impl std::fmt::Display) -> Error {
    Error::InvalidMedia(error.to_string())
}

fn hash_snapshot(documents: &format::Documents) -> Result<String> {
    let encoded = serde_json::to_vec(documents).map_err(|source| Error::Json {
        path: PathBuf::from("<project-snapshot>"),
        source,
    })?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    fn project() -> (tempfile::TempDir, ProjectStore) {
        let directory = tempfile::tempdir().unwrap();
        let store =
            ProjectStore::create_default(directory.path().join("song"), "Song", 120.0, 48_000)
                .unwrap();
        (directory, store)
    }

    fn wav(path: &Path, value: i16) {
        let mut writer = hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 44_100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for _ in 0..128 {
            writer.write_sample(value).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn ramp_wav(path: &Path) {
        let mut writer = hound::WavWriter::create(
            path,
            hound::WavSpec {
                channels: 1,
                sample_rate: 100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();
        for sample in 0_i16..100 {
            writer.write_sample(sample * 100).unwrap();
        }
        writer.finalize().unwrap();
    }

    fn tiny_mp3() -> Vec<u8> {
        const ENCODED: &str = "SUQzBAAAAAAAI1RTU0UAAAAPAAADTGF2ZjYyLjEyLjEwMgAAAAAAAAAAAAAA//sQxAAABIQVWVRggDCqCKiDNlAAAAGgS4BgAmTT2AQAABCxOD5d7gQOfqBAEHS4Ph/EAIRI7//0A0KBNpABgMRIDCSI04PcIFdF0PJKFgzlUf5eAoF8BRIPfh4FTvUDQl+dUi5pc0z/+xLEAgPFEB0iHeAAKKiEJEK8AAQHAJREA4YAIGRm7tgmVoMeYYgOpgpAZmAyBMYEIDxgSgLF4upXVL7mAyAuYAgIRggBOGfYuKZq415huhImDSBKYB4G5gWgTmBWBGXDn7eFAA/PDDD/+xDEAoAFGENXOYKAAJQGpuuSMATAAAAAyFCUw55oJjcV30TS6TGd7v383lk+8DCv48WL4GO/CqgBHsLgAAALAkBoNNKikKgZmFQRDLlmioIigoCsYwp3iW6VBbiVTEFNRTMuMTAxIA==";
        fn value(byte: u8) -> u8 {
            match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => 0,
            }
        }
        let mut decoded = Vec::with_capacity(ENCODED.len() / 4 * 3);
        for chunk in ENCODED.as_bytes().chunks_exact(4) {
            let bits = (u32::from(value(chunk[0])) << 18)
                | (u32::from(value(chunk[1])) << 12)
                | (u32::from(value(chunk[2])) << 6)
                | u32::from(value(chunk[3]));
            decoded.push(u8::try_from((bits >> 16) & 0xff).unwrap());
            if chunk[2] != b'=' {
                decoded.push(u8::try_from((bits >> 8) & 0xff).unwrap());
            }
            if chunk[3] != b'=' {
                decoded.push(u8::try_from(bits & 0xff).unwrap());
            }
        }
        decoded
    }

    fn beats(value: f64) -> gaw_core::Beats {
        gaw_core::Beats::new(value).unwrap()
    }

    fn project_with_child_and_dense_lane(
        store: &ProjectStore,
        point_count: u32,
    ) -> (CompositionId, CompositionId) {
        let mut project = store.load_project().unwrap();
        let root_id = project.root_composition_id;
        project.compositions[0].length = beats(f64::from(point_count.max(16)));
        let mut child = gaw_core::Composition::new("Child", beats(f64::from(point_count.max(16))));
        let child_id = child.id;
        let mut root_track = gaw_core::Track::audio(root_id, "Root track");
        root_track
            .clips
            .push(gaw_core::Clip::Composition(gaw_core::CompositionClip::new(
                child_id,
                beats(0.0),
                beats(1.0),
            )));
        let child_track = gaw_core::Track::audio(child_id, "Child track");
        project.compositions[0].track_ids.push(root_track.id);
        child.track_ids.push(child_track.id);
        let processor = gaw_core::Processor::new(
            gaw_core::ProcessorId::new("dense_gain").unwrap(),
            gaw_core::ProcessorKind::Gain(gaw_core::GainParameters::default()),
        );
        let processor_id = processor.id.clone();
        child.output_effects.push(processor);
        project.compositions.push(child);
        project.tracks.extend([root_track, child_track]);
        project.automation.push(gaw_core::AutomationLane {
            id: gaw_core::AutomationLaneId::new(),
            composition_id: child_id,
            name: "Dense lane".into(),
            target: gaw_core::AutomationTarget::CompositionOutputProcessor {
                processor_id,
                parameter_id: "gain_db".into(),
            },
            points: (0..point_count)
                .map(|index| gaw_core::AutomationPoint {
                    time: beats(f64::from(index)),
                    value: gaw_core::AutomationValue::Decibels(
                        gaw_core::Decibels::new(-6.0).unwrap(),
                    ),
                    curve: gaw_core::AutomationCurve::Linear,
                })
                .collect(),
        });
        store.save_project(&project).unwrap();
        (root_id, child_id)
    }

    fn reset_json_reads() {
        JSON_READS.with(|reads| reads.borrow_mut().clear());
    }

    fn json_reads() -> Vec<(String, u64)> {
        JSON_READS.with(|reads| reads.borrow().clone())
    }

    #[test]
    fn typed_project_round_trips_through_local_fragments() {
        let (_directory, store) = project();
        let loaded = store.load_project().unwrap();
        assert_eq!(loaded.name, "Song");
        assert!(store.validate().unwrap().is_valid());
        assert!(store.root.join("assets/index.json").is_file());
        assert!(
            store
                .root
                .join(format!(
                    "compositions/{}/composition.json",
                    loaded.root_composition_id
                ))
                .is_file()
        );
        assert_eq!(
            ProjectStore::open(store.root())
                .unwrap()
                .load_project()
                .unwrap(),
            loaded
        );
    }

    #[test]
    fn failed_core_transaction_leaves_every_document_unchanged() {
        let (_directory, store) = project();
        let before = store.scan_documents_unlocked().unwrap();
        let transaction = Transaction::new([
            Command::SetProjectName {
                name: "Changed".into(),
            },
            Command::RemoveAsset {
                asset_id: gaw_core::AssetId::new(),
            },
        ]);
        assert!(store.commit_transaction(&transaction).is_err());
        assert_eq!(store.scan_documents_unlocked().unwrap(), before);
        assert_eq!(store.load_project().unwrap().name, "Song");
    }

    #[test]
    fn every_fragment_is_strictly_decoded_as_its_core_type() {
        let (_directory, store) = project();
        let project = store.load_project().unwrap();
        let track = gaw_core::Track::audio(project.root_composition_id, "Track");
        store
            .commit_transaction(&Transaction::new([Command::AddTrack {
                track: track.clone(),
                index: 0,
            }]))
            .unwrap();
        let path = ProjectPath::new(format!(
            "compositions/{}/tracks/{}.json",
            track.composition_id, track.id
        ))
        .unwrap();
        let mut document = store.read_json(&path).unwrap();
        document["unexpected_nested_model_field"] = Value::Bool(true);
        write_json_file(&store.root.join(path.as_path()), &document).unwrap();
        assert!(!store.validate().unwrap().is_valid());
        assert!(ProjectStore::open(store.root()).is_ok());
        assert!(store.load_composition(project.root_composition_id).is_err());
        assert!(store.load_project().is_err());
    }

    #[test]
    fn updating_a_track_rewrites_only_its_composition_local_fragment() {
        let (_directory, store) = project();
        let project = store.load_project().unwrap();
        let track = gaw_core::Track::audio(project.root_composition_id, "Before");
        store
            .commit_transaction(&Transaction::new([Command::AddTrack {
                track: track.clone(),
                index: 0,
            }]))
            .unwrap();
        let project_before = fs::read(store.root.join("project.json")).unwrap();
        let composition_path = store.root.join(format!(
            "compositions/{}/composition.json",
            project.root_composition_id
        ));
        let composition_before = fs::read(&composition_path).unwrap();
        let mut updated = track;
        updated.name = "After".into();
        store
            .commit_transaction(&Transaction::new([Command::UpdateTrack { track: updated }]))
            .unwrap();
        assert_eq!(
            fs::read(store.root.join("project.json")).unwrap(),
            project_before
        );
        assert_eq!(fs::read(composition_path).unwrap(), composition_before);
    }

    #[test]
    fn lazy_open_and_bundle_do_not_parse_unrelated_large_fragments() {
        let (_directory, store) = project();
        let (root_id, child_id) = project_with_child_and_dense_lane(&store, 10_000);
        let manifest = store.load_manifest().unwrap();
        let child_track = manifest
            .track_order
            .iter()
            .find(|value| value.composition_id == child_id)
            .unwrap();
        let child_lane = manifest
            .automation_order
            .iter()
            .find(|value| value.composition_id == child_id)
            .unwrap();
        let track_path = store.root.join(format!(
            "compositions/{child_id}/tracks/{}.json",
            child_track.id
        ));
        let lane_path = store.root.join(format!(
            "compositions/{child_id}/automation/{}.json",
            child_lane.id
        ));
        let composition_path = store
            .root
            .join(format!("compositions/{child_id}/composition.json"));
        let lane_bytes = fs::metadata(&lane_path).unwrap().len();
        assert!(lane_bytes > 500_000);
        fs::write(&composition_path, b"{also not json").unwrap();
        fs::write(&track_path, b"{not json").unwrap();
        fs::write(&lane_path, vec![b'x'; usize::try_from(lane_bytes).unwrap()]).unwrap();

        reset_json_reads();
        let reopened = ProjectStore::open(store.root()).unwrap();
        assert_eq!(
            json_reads(),
            vec![(
                "project.json".into(),
                fs::metadata(store.root.join("project.json")).unwrap().len()
            )]
        );

        reset_json_reads();
        let bundle = reopened.load_composition(root_id).unwrap();
        assert_eq!(bundle.composition.id, root_id);
        assert_eq!(bundle.tracks.len(), 1);
        let reads = json_reads();
        assert_eq!(reads.len(), 3);
        assert_eq!(reads[0].0, "project.json");
        assert!(
            reads
                .iter()
                .all(|(path, _)| !path.contains(&child_id.to_string()))
        );
        assert!(reads.iter().map(|(_, bytes)| bytes).sum::<u64>() < lane_bytes);

        assert!(reopened.load_project().is_err());
        assert!(!reopened.validate().unwrap().is_valid());
    }

    #[test]
    fn invalid_manifest_is_rejected_without_reading_fragments() {
        let (_directory, store) = project();
        let project_path = ProjectPath::new("project.json").unwrap();
        let mut document = store.read_json(&project_path).unwrap();
        let duplicate = document["composition_order"][0].clone();
        document["composition_order"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        write_json_file(&store.root.join("project.json"), &document).unwrap();
        reset_json_reads();
        assert!(ProjectStore::open(store.root()).is_err());
        assert_eq!(json_reads().len(), 1);
        assert_eq!(json_reads()[0].0, "project.json");
    }

    #[test]
    fn event_stream_edit_rewrites_only_its_fragment() {
        let (_directory, store) = project();
        let mut project = store.load_project().unwrap();
        let first = gaw_core::EventData::new("First");
        let second = gaw_core::EventData::new("Second");
        project.event_data.extend([first.clone(), second.clone()]);
        store.save_project(&project).unwrap();
        let header_before = fs::read(store.root.join("project.json")).unwrap();
        let second_path = store.root.join(format!("events/{}.json", second.id));
        let second_before = fs::read(&second_path).unwrap();

        let mut updated = first.clone();
        updated.name = "Updated".into();
        store
            .commit_transaction(&Transaction::new([Command::UpdateEventData {
                event_data: updated.clone(),
            }]))
            .unwrap();

        assert_eq!(
            fs::read(store.root.join("project.json")).unwrap(),
            header_before
        );
        assert_eq!(fs::read(second_path).unwrap(), second_before);
        assert_eq!(store.load_event_data(first.id).unwrap(), updated);
    }

    #[test]
    fn typed_preset_library_is_strict_atomic_and_outside_song_state() {
        let (_directory, store) = project();
        let before = store.load_project().unwrap();
        let sampler_id = PresetId::new("z_sampler").unwrap();
        let sampler_id_first = PresetId::new("a_sampler").unwrap();
        let mut sampler = SamplerPreset::new("Sampler", gaw_core::Sampler::new(16).unwrap());
        let effect_id = PresetId::new("gain").unwrap();
        let effect = EffectPreset::new(
            "Gain",
            gaw_core::ProcessorKind::Gain(gaw_core::GainParameters::default()),
        );

        store.save_sampler_preset(&sampler_id, &sampler).unwrap();
        store
            .save_sampler_preset(&sampler_id_first, &sampler)
            .unwrap();
        store.save_effect_preset(&effect_id, &effect).unwrap();
        assert_eq!(
            store.list_sampler_presets().unwrap(),
            vec![sampler_id_first.clone(), sampler_id.clone()]
        );
        assert_eq!(store.load_effect_preset(&effect_id).unwrap(), effect);
        sampler.name = "Replaced".into();
        store.save_sampler_preset(&sampler_id, &sampler).unwrap();
        assert_eq!(store.load_sampler_preset(&sampler_id).unwrap(), sampler);
        assert_eq!(store.load_project().unwrap(), before);

        let sampler_path = store
            .root
            .join(format!("presets/samplers/{sampler_id}.json"));
        let mut invalid: Value = serde_json::from_slice(&fs::read(&sampler_path).unwrap()).unwrap();
        invalid["opaque_state"] = Value::Bool(true);
        write_json_file(&sampler_path, &invalid).unwrap();
        assert!(store.load_sampler_preset(&sampler_id).is_err());
        assert!(store.delete_sampler_preset(&sampler_id).unwrap());
        assert!(!store.delete_sampler_preset(&sampler_id).unwrap());
        assert!(store.delete_effect_preset(&effect_id).unwrap());
    }

    #[test]
    fn content_addressed_media_opens_only_matching_safe_references() {
        let (directory, store) = project();
        let source = directory.path().join("media.wav");
        wav(&source, 9);
        let imported = store.import_media(source).unwrap();
        assert_eq!(store.load_asset_index().unwrap().assets.len(), 1);
        let mut file = store
            .open_media(&imported.relative_path, &imported.content_hash)
            .unwrap();
        let mut header = [0_u8; 4];
        file.read_exact(&mut header).unwrap();
        assert_eq!(&header, b"RIFF");
        let wrong = ContentHash::new("0".repeat(64)).unwrap();
        assert!(store.open_media(&imported.relative_path, &wrong).is_err());
        let outside =
            gaw_core::ProjectPath::new(format!("presets/{}.wav", imported.content_hash)).unwrap();
        assert!(store.open_media(&outside, &imported.content_hash).is_err());
    }

    #[test]
    fn one_typed_transaction_is_one_hash_chained_recovery_transition() {
        let (_directory, store) = project();
        let root = store.load_project().unwrap().root_composition_id;
        let track = gaw_core::Track::audio(root, "Recovered Track");
        let transaction = Transaction::named(
            "agent edit",
            [
                Command::SetProjectName {
                    name: "Recovered".into(),
                },
                Command::AddTrack {
                    track: track.clone(),
                    index: 0,
                },
            ],
        );
        let record = store.append_recovery(&transaction).unwrap();
        assert_ne!(record.before_snapshot_hash, record.after_snapshot_hash);
        assert_eq!(store.pending_recovery().unwrap().len(), 1);
        assert_eq!(store.recover().unwrap(), 1);
        let reopened = ProjectStore::open(store.root()).unwrap();
        let loaded = reopened.load_project().unwrap();
        assert_eq!(loaded.name, "Recovered");
        assert_eq!(loaded.tracks, vec![track.clone()]);
        assert!(
            reopened
                .root
                .join(format!(
                    "compositions/{}/tracks/{}.json",
                    track.composition_id, track.id
                ))
                .is_file()
        );
        assert_eq!(reopened.recover().unwrap(), 0);
    }

    #[test]
    fn checkpoint_retry_clears_an_already_committed_journal() {
        let (_directory, store) = project();
        let transaction = Transaction::new([Command::SetProjectName {
            name: "Committed".into(),
        }]);
        store.append_recovery(&transaction).unwrap();
        let current = store.scan_documents_unlocked().unwrap();
        let mut project = format::decode(&current).unwrap();
        transaction.apply(&mut project).unwrap();
        let next = format::encode(&project).unwrap();
        store
            .apply_storage_unlocked(&diff(&current, &next))
            .unwrap();

        store.checkpoint_project(&project).unwrap();
        assert!(store.pending_recovery().unwrap().is_empty());
        assert_eq!(store.load_project().unwrap().name, "Committed");
    }

    #[test]
    fn recovery_ignores_a_torn_tail() {
        let (_directory, store) = project();
        store
            .append_recovery(&Transaction::new([Command::SetProjectName {
                name: "Recovered".into(),
            }]))
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(store.recovery_path().unwrap())
            .unwrap()
            .write_all(br#"{"torn":"#)
            .unwrap();
        assert_eq!(store.pending_recovery().unwrap().len(), 1);
        assert_eq!(store.recover().unwrap(), 1);
        assert_eq!(store.load_project().unwrap().name, "Recovered");
    }

    #[test]
    fn wav_import_is_content_addressed_typed_and_deduplicated() {
        let (directory, store) = project();
        let source = directory.path().join("Kick.WAV");
        wav(&source, 42);
        let first = store.import_media(&source).unwrap();
        let second = store.import_media(&source).unwrap();
        assert_eq!(first.asset_id, second.asset_id);
        assert_eq!(first.relative_path, second.relative_path);
        assert_eq!(
            fs::read_dir(store.root.join("assets/media"))
                .unwrap()
                .count(),
            1
        );
        let project = store.load_project().unwrap();
        assert_eq!(project.assets.len(), 1);
        let AudioAssetDefinition::Imported(imported) = &project.assets[0].definition else {
            panic!("expected imported asset")
        };
        assert_eq!(imported.sample_rate.value(), 44_100);
        assert_eq!(imported.layout, ChannelLayout::Mono);
        assert_eq!(imported.frames.0, 128);
        let canonical = hound::WavReader::open(store.root.join(first.relative_path.as_str()))
            .unwrap()
            .spec();
        assert_eq!(canonical.sample_format, hound::SampleFormat::Float);
        assert_eq!(canonical.bits_per_sample, 32);
        assert!(
            !serde_json::to_string(&project)
                .unwrap()
                .contains(directory.path().to_str().unwrap())
        );
        fs::write(store.root.join(first.relative_path.as_str()), b"corrupt").unwrap();
        assert!(!store.validate().unwrap().is_valid());
    }

    #[test]
    fn mp3_import_is_decoded_to_canonical_wav() {
        let (directory, store) = project();
        let source = directory.path().join("tone.mp3");
        fs::write(&source, tiny_mp3()).unwrap();

        let imported = store.import_media(&source).unwrap();
        assert_eq!(imported.original_filename, "tone.mp3");
        assert_eq!(
            Path::new(imported.relative_path.as_str()).extension(),
            Some(std::ffi::OsStr::new("wav"))
        );
        let mut canonical =
            hound::WavReader::open(store.root.join(imported.relative_path.as_str())).unwrap();
        assert_eq!(canonical.spec().sample_rate, 44_100);
        assert_eq!(canonical.spec().channels, 1);
        assert_eq!(canonical.spec().sample_format, hound::SampleFormat::Float);
        assert_eq!(canonical.spec().bits_per_sample, 32);
        let duration = canonical.duration();
        let samples = canonical
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let project = store.load_project().unwrap();
        let AudioAssetDefinition::Imported(definition) = &project.assets[0].definition else {
            panic!("expected imported asset")
        };
        assert_eq!(u64::from(duration), definition.frames.0);
        assert!(!samples.is_empty());
        assert!(samples.iter().all(|sample| sample.is_finite()));
        assert!(store.validate().unwrap().is_valid());
    }

    #[test]
    fn confirmed_tempo_regions_materialize_as_independent_assets() {
        let (directory, store) = project();
        let source_path = directory.path().join("changing tempo.wav");
        ramp_wav(&source_path);
        let source = store.import_media(&source_path).unwrap();

        let split = store
            .split_imported_media(
                source.asset_id,
                &[
                    MediaRegion {
                        range: FrameRange {
                            start: gaw_core::FramePosition(0),
                            length: FrameCount(40),
                        },
                        bpm: Bpm::new(90.0).unwrap(),
                    },
                    MediaRegion {
                        range: FrameRange {
                            start: gaw_core::FramePosition(40),
                            length: FrameCount(60),
                        },
                        bpm: Bpm::new(128.0).unwrap(),
                    },
                ],
            )
            .unwrap();

        assert_eq!(split.len(), 2);
        let project = store.load_project().unwrap();
        assert_eq!(project.assets.len(), 3);
        assert!(
            project
                .assets
                .iter()
                .any(|asset| asset.id == source.asset_id)
        );
        let regions = split
            .iter()
            .map(|created| {
                project
                    .assets
                    .iter()
                    .find(|asset| asset.id == created.asset_id)
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(regions[0].name, "changing tempo 1");
        assert_eq!(regions[1].name, "changing tempo 2");
        assert!((regions[0].tempo.unwrap().bpm.value() - 90.0).abs() < f64::EPSILON);
        assert!((regions[1].tempo.unwrap().bpm.value() - 128.0).abs() < f64::EPSILON);
        for (created, expected_frames) in split.iter().zip([40_u32, 60]) {
            let reader =
                hound::WavReader::open(store.root.join(created.relative_path.as_str())).unwrap();
            assert_eq!(reader.duration(), expected_frames);
            assert_eq!(reader.spec().sample_format, hound::SampleFormat::Float);
            assert_eq!(reader.spec().bits_per_sample, 32);
        }
        assert!(store.validate().unwrap().is_valid());
    }

    #[test]
    fn tempo_region_split_materializes_overlapping_context_ranges() {
        let (directory, store) = project();
        let source_path = directory.path().join("source.wav");
        ramp_wav(&source_path);
        let source = store.import_media(&source_path).unwrap();
        let split = store
            .split_imported_media(
                source.asset_id,
                &[
                    MediaRegion {
                        range: FrameRange {
                            start: gaw_core::FramePosition(0),
                            length: FrameCount(60),
                        },
                        bpm: Bpm::new(90.0).unwrap(),
                    },
                    MediaRegion {
                        range: FrameRange {
                            start: gaw_core::FramePosition(50),
                            length: FrameCount(50),
                        },
                        bpm: Bpm::new(128.0).unwrap(),
                    },
                ],
            )
            .unwrap();

        assert_eq!(split.len(), 2);
        let first = hound::WavReader::open(store.root.join(split[0].relative_path.as_str()))
            .unwrap()
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let second = hound::WavReader::open(store.root.join(split[1].relative_path.as_str()))
            .unwrap()
            .samples::<f32>()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(first.len(), 60);
        assert_eq!(second.len(), 50);
        assert!((first[0] - 0.0).abs() < 1e-6);
        assert!((second[0] - (50.0 * 100.0 / 32_768.0)).abs() < 1e-6);
        assert!(store.validate().unwrap().is_valid());
    }

    #[test]
    fn validation_rejects_media_metadata_and_noncanonical_filename() {
        let (directory, store) = project();
        let source = directory.path().join("sample.wav");
        wav(&source, 42);
        let imported = store.import_media(&source).unwrap();

        let mut project = store.load_project().unwrap();
        match &mut project.assets[0].definition {
            AudioAssetDefinition::Imported(definition) => definition.frames = FrameCount(999),
            _ => panic!("expected imported asset"),
        }
        store.save_project(&project).unwrap();
        let report = store.validate().unwrap();
        assert!(
            report
                .errors
                .iter()
                .any(|issue| issue.message.contains("WAV metadata does not match"))
        );

        let canonical = store.root.join(imported.relative_path.as_str());
        let alias = store.root.join("assets/media/alias.wav");
        fs::rename(canonical, &alias).unwrap();
        match &mut project.assets[0].definition {
            AudioAssetDefinition::Imported(definition) => {
                definition.frames = FrameCount(128);
                definition.media_path =
                    gaw_core::ProjectPath::new("assets/media/alias.wav").unwrap();
            }
            _ => panic!("expected imported asset"),
        }
        store.save_project(&project).unwrap();
        let report = store.validate().unwrap();
        assert!(report.errors.iter().any(|issue| {
            issue
                .message
                .contains("media filename does not match its content hash")
        }));
    }

    #[test]
    fn import_rejects_non_audio_without_mutating_assets() {
        let (directory, store) = project();
        let source = directory.path().join("not-audio.wav");
        fs::write(&source, b"this is not a WAV file").unwrap();
        assert!(matches!(
            store.import_media(source),
            Err(Error::InvalidMedia(_))
        ));
        assert!(store.load_project().unwrap().assets.is_empty());
        assert_eq!(
            fs::read_dir(store.root.join("assets/media"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn canonical_wav_limits_reject_invalid_headers_before_writing() {
        assert!(supported_sample_rate(0, 1).is_err());
        assert!(supported_sample_rate(u32::MAX, 2).is_err());
        let maximum_frames = (u64::from(u32::MAX) - 128) / 4;
        assert_eq!(
            extend_decoded_frames(0, maximum_frames, 1).unwrap(),
            maximum_frames
        );
        assert!(extend_decoded_frames(0, maximum_frames + 1, 1).is_err());
    }

    #[test]
    fn interrupted_storage_write_rolls_back_before_open() {
        let (_directory, store) = project();
        let path = ProjectPath::new("project.json").unwrap();
        let old_document = store.read_json(&path).unwrap();
        let transaction_root = store.atomic_root().unwrap().join("interrupted");
        let backup = transaction_root.join("backup").join(path.as_path());
        make_parent(&backup).unwrap();
        let target = store.root.join(path.as_path());
        fs::rename(&target, &backup).unwrap();
        let mut new_document = old_document.clone();
        new_document["name"] = Value::String("Uncommitted".into());
        write_json_file(&target, &new_document).unwrap();
        write_json_file(
            &transaction_root.join("manifest.json"),
            &AtomicManifest {
                schema_version: SCHEMA_VERSION,
                entries: vec![AtomicEntry {
                    path: path.clone(),
                    existed: true,
                    write: true,
                }],
            },
        )
        .unwrap();
        let reopened = ProjectStore::open(store.root()).unwrap();
        assert_eq!(reopened.read_json(&path).unwrap(), old_document);
    }

    #[test]
    fn committed_storage_write_survives_interrupted_cleanup() {
        let (_directory, store) = project();
        let path = ProjectPath::new("project.json").unwrap();
        let transaction_root = store.atomic_root().unwrap().join("committed");
        let backup = transaction_root.join("backup").join(path.as_path());
        make_parent(&backup).unwrap();
        let target = store.root.join(path.as_path());
        let mut committed = store.read_json(&path).unwrap();
        fs::rename(&target, &backup).unwrap();
        committed["name"] = Value::String("Committed".into());
        write_json_file(&target, &committed).unwrap();
        write_json_file(
            &transaction_root.join("manifest.json"),
            &AtomicManifest {
                schema_version: SCHEMA_VERSION,
                entries: vec![AtomicEntry {
                    path,
                    existed: true,
                    write: true,
                }],
            },
        )
        .unwrap();
        File::create(transaction_root.with_extension("committed"))
            .unwrap()
            .sync_all()
            .unwrap();
        let reopened = ProjectStore::open(store.root()).unwrap();
        assert_eq!(reopened.load_project().unwrap().name, "Committed");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_in_canonical_and_runtime_trees() {
        use std::os::unix::fs::symlink;

        let (directory, store) = project();
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, store.root.join("compositions/link")).unwrap();
        assert!(!store.validate().unwrap().is_valid());
        fs::remove_file(store.root.join("compositions/link")).unwrap();

        let outside_file = outside.join("file");
        fs::write(&outside_file, b"outside").unwrap();
        let hash = ContentHash::new("0".repeat(64)).unwrap();
        let media_path =
            gaw_core::ProjectPath::new(format!("assets/media/{}.wav", hash.as_str())).unwrap();
        symlink(&outside_file, store.root.join(media_path.as_str())).unwrap();
        assert!(matches!(
            store.open_media(&media_path, &hash),
            Err(Error::Symlink(_))
        ));
        fs::remove_file(store.root.join(media_path.as_str())).unwrap();

        let preset_id = PresetId::new("linked").unwrap();
        symlink(
            &outside_file,
            store.root.join("presets/samplers/linked.json"),
        )
        .unwrap();
        assert!(matches!(
            store.load_sampler_preset(&preset_id),
            Err(Error::Symlink(_))
        ));
        fs::remove_file(store.root.join("presets/samplers/linked.json")).unwrap();

        symlink(&outside, store.root.join(".gaw/recovery.journal")).unwrap();
        assert!(matches!(store.pending_recovery(), Err(Error::Symlink(_))));
    }
}
