use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use gaw_core::{
    AudioAsset, AudioAssetDefinition, ChannelLayout, Command, ContentHash, FrameCount,
    ImportedAudio, Project, SampleRate, Transaction,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    Error, ProjectPath, RecoveryRecord, Result, SCHEMA_VERSION, error::io, format, recovery,
};

static TRANSACTION_COUNTER: AtomicU64 = AtomicU64::new(0);

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
                for directory in ["assets", "compositions", ".gaw"] {
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
        format::decode(&store.scan_documents_unlocked()?)?;
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

    /// Reassembles all canonical fragments into the core model and validates it.
    pub fn load_project(&self) -> Result<Project> {
        let _write_lock = self.acquire_write_lock()?;
        format::decode(&self.load_documents_unlocked()?)
    }

    /// Saves a complete typed snapshot as one crash-atomic document diff.
    pub fn save_project(&self, project: &Project) -> Result<()> {
        let _write_lock = self.acquire_write_lock()?;
        self.reject_pending_recovery()?;
        let current = self.load_documents_unlocked()?;
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
        let current = self.load_documents_unlocked()?;
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
        let before_documents = self.load_documents_unlocked()?;
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

    /// Imports a WAV into content-addressed storage and adds or updates its typed asset.
    pub fn import_media(&self, source: impl AsRef<Path>) -> Result<ImportedMedia> {
        let _write_lock = self.acquire_write_lock()?;
        self.reject_pending_recovery()?;
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
        let (content_hash, byte_len) = copy_and_hash(source, &mut temporary)?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| io(temporary.path(), error))?;

        let reader = hound::WavReader::open(temporary.path())
            .map_err(|error| Error::InvalidMedia(error.to_string()))?;
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
        let frames = FrameCount(u64::from(reader.duration()));
        let sample_rate = SampleRate::new(spec.sample_rate)
            .map_err(|error| Error::InvalidMedia(error.to_string()))?;
        drop(reader);

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

        let project = format::decode(&self.load_documents_unlocked()?)?;
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

    /// Journals a validated core transaction without applying it, for crash handoff.
    pub fn append_recovery(&self, transaction: &Transaction) -> Result<RecoveryRecord> {
        let _write_lock = self.acquire_write_lock()?;
        self.append_recovery_unlocked(transaction)
    }

    fn append_recovery_unlocked(&self, transaction: &Transaction) -> Result<RecoveryRecord> {
        let documents = self.load_documents_unlocked()?;
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
        let mut documents = self.load_documents_unlocked()?;
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
        let current_documents = self.load_documents_unlocked()?;
        self.apply_storage_unlocked(&diff(&current_documents, &documents))?;
        recovery::clear(&self.recovery_path()?)?;
        Ok(records.len().saturating_sub(start))
    }

    pub fn clear_recovery(&self) -> Result<()> {
        let _write_lock = self.acquire_write_lock()?;
        recovery::clear(&self.recovery_path()?)
    }

    fn load_documents_unlocked(&self) -> Result<format::Documents> {
        let mut documents = BTreeMap::new();
        let project_path = ProjectPath::new("project.json")?;
        if !self.root.join(project_path.as_path()).exists() {
            return Ok(documents);
        }
        let project_document = self.read_json(&project_path)?;
        for path in format::canonical_paths(&project_document)? {
            let document = if path == project_path {
                project_document.clone()
            } else {
                self.read_json(&path)?
            };
            documents.insert(path, document);
        }
        Ok(documents)
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
            if path.starts_with(".gaw") || path.starts_with("assets/media") {
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
        let current = self.load_documents_unlocked()?;
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
                    &mut report.errors,
                )?;
            }
            for revision in &asset.revisions {
                self.validate_media_reference(
                    &revision.media_path,
                    &revision.content_hash,
                    &mut report.errors,
                )?;
            }
        }
        Ok(())
    }

    fn validate_media_reference(
        &self,
        path: &gaw_core::ProjectPath,
        hash: &ContentHash,
        errors: &mut Vec<ValidationIssue>,
    ) -> Result<()> {
        let path = ProjectPath::new(path.as_str())?;
        if !path.as_str().starts_with("assets/media/") {
            errors.push(ValidationIssue {
                path: path.to_string(),
                message: "media path is outside assets/media".into(),
            });
            return Ok(());
        }
        let target = self.safe_target(&path)?;
        if !target.is_file() {
            errors.push(ValidationIssue {
                path: path.to_string(),
                message: "referenced media file is missing".into(),
            });
        } else if hash_file(&target)? != hash.as_str() {
            errors.push(ValidationIssue {
                path: path.to_string(),
                message: "referenced media content hash does not match".into(),
            });
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
struct AtomicManifest {
    schema_version: u32,
    entries: Vec<AtomicEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct AtomicEntry {
    path: ProjectPath,
    existed: bool,
    write: bool,
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

fn copy_and_hash(source: &Path, temporary: &mut tempfile::NamedTempFile) -> Result<(String, u64)> {
    let mut input = File::open(source).map_err(|error| io(source, error))?;
    if !input
        .metadata()
        .map_err(|error| io(source, error))?
        .is_file()
    {
        return Err(Error::InvalidMedia("source must be a regular file".into()));
    }
    #[cfg(target_os = "linux")]
    let reflinked = rustix::fs::ioctl_ficlone(temporary.as_file(), &input).is_ok();
    #[cfg(not(target_os = "linux"))]
    let reflinked = false;

    if !reflinked {
        temporary
            .as_file_mut()
            .set_len(0)
            .map_err(|error| io(temporary.path(), error))?;
        temporary
            .as_file_mut()
            .seek(std::io::SeekFrom::Start(0))
            .map_err(|error| io(temporary.path(), error))?;
        input
            .seek(std::io::SeekFrom::Start(0))
            .map_err(|error| io(source, error))?;
        std::io::copy(&mut input, temporary).map_err(|error| io(source, error))?;
    }
    let byte_len = temporary
        .as_file()
        .metadata()
        .map_err(|error| io(temporary.path(), error))?
        .len();
    Ok((hash_file(temporary.path())?, byte_len))
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
        let before = store.load_documents_unlocked().unwrap();
        let transaction = Transaction::new([
            Command::SetProjectName {
                name: "Changed".into(),
            },
            Command::RemoveAsset {
                asset_id: gaw_core::AssetId::new(),
            },
        ]);
        assert!(store.commit_transaction(&transaction).is_err());
        assert_eq!(store.load_documents_unlocked().unwrap(), before);
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
        assert!(ProjectStore::open(store.root()).is_err());
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
        assert!(
            !serde_json::to_string(&project)
                .unwrap()
                .contains(directory.path().to_str().unwrap())
        );
        fs::write(store.root.join(first.relative_path.as_str()), b"corrupt").unwrap();
        assert!(!store.validate().unwrap().is_valid());
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
        symlink(&outside, store.root.join(".gaw/recovery.journal")).unwrap();
        assert!(matches!(store.pending_recovery(), Err(Error::Symlink(_))));
    }
}
