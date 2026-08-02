use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    Error, ProjectPath, RecoveryRecord, Result, SCHEMA_VERSION, error::io, path::valid_id, recovery,
};

static TRANSACTION_COUNTER: AtomicU64 = AtomicU64::new(0);

/// All canonical JSON documents in a project.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ProjectSnapshot {
    pub documents: BTreeMap<ProjectPath, Value>,
}

/// A crash-atomic group of canonical document replacements and removals.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JsonTransaction {
    pub schema_version: u32,
    pub operations: Vec<JsonOperation>,
}

impl Default for JsonTransaction {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            operations: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum JsonOperation {
    Write { path: ProjectPath, document: Value },
    Delete { path: ProjectPath },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportedMedia {
    pub asset_id: String,
    pub content_hash: String,
    pub relative_path: ProjectPath,
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
}

/// A directory-backed GAW project.
#[derive(Clone, Debug)]
pub struct ProjectStore {
    root: PathBuf,
}

impl ProjectStore {
    pub fn create(root: impl AsRef<Path>, project: &Value) -> Result<Self> {
        let root = root.as_ref();
        validate_schema_document(project)?;
        let composition_id = required_string(project, "project.json", "root_composition_id")?;
        require_id(composition_id, "cmp_", "project.json#root_composition_id")?;
        let name = required_string(project, "project.json", "name")?;
        let created_root = !root.exists();
        if root.exists() {
            reject_symlink(root)?;
            let mut entries = fs::read_dir(root).map_err(|error| io(root, error))?;
            if entries
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
        fs::create_dir_all(store.root.join("assets/media"))
            .map_err(|error| io(store.root.join("assets/media"), error))?;
        fs::create_dir_all(store.root.join("compositions"))
            .map_err(|error| io(store.root.join("compositions"), error))?;
        fs::create_dir_all(store.root.join(".gaw/cache/audio"))
            .map_err(|error| io(store.root.join(".gaw/cache/audio"), error))?;
        fs::create_dir_all(store.root.join(".gaw/cache/waveforms"))
            .map_err(|error| io(store.root.join(".gaw/cache/waveforms"), error))?;
        let initialization = store.apply_transaction(&JsonTransaction {
            schema_version: SCHEMA_VERSION,
            operations: vec![
                JsonOperation::Write {
                    path: ProjectPath::new("project.json")?,
                    document: project.clone(),
                },
                JsonOperation::Write {
                    path: ProjectPath::new("assets/index.json")?,
                    document: json!({"schema_version": SCHEMA_VERSION, "assets": {}}),
                },
                JsonOperation::Write {
                    path: ProjectPath::new(format!(
                        "compositions/{composition_id}/composition.json"
                    ))?,
                    document: json!({
                        "schema_version": SCHEMA_VERSION,
                        "id": composition_id,
                        "name": name,
                        "length": {"unit": "beats", "value": 16.0},
                        "output_layout": "stereo",
                        "track_ids": [],
                        "output_effects": []
                    }),
                },
            ],
        });
        if let Err(error) = initialization {
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
        if !bpm.is_finite() || bpm <= 0.0 || sample_rate == 0 {
            return Err(Error::InvalidTransaction(
                "bpm and sample rate must be positive; bpm must be finite".into(),
            ));
        }
        let suffix = unique_suffix();
        let project_id = format!("prj_{suffix}");
        let composition_id = format!("cmp_{suffix}");
        let initial_project = json!({
            "schema_version": SCHEMA_VERSION,
            "id": project_id,
            "name": name,
            "root_composition_id": composition_id,
            "bpm": bpm,
            "internal_sample_rate_hz": sample_rate,
            "settings": {}
        });
        Self::create(root, &initial_project)
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
        let project = store.read_json(&ProjectPath::new("project.json")?)?;
        validate_schema_document(&project)?;
        let snapshot = store.load_snapshot_unlocked()?;
        validate_snapshot_relationships(&snapshot.documents)?;
        Ok(store)
    }

    /// Validates a path without requiring it to be valid enough for `open`.
    ///
    /// This is primarily useful to tools that must report malformed projects as
    /// structured validation output instead of failing during strict opening.
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

    pub fn load_snapshot(&self) -> Result<ProjectSnapshot> {
        let _write_lock = self.acquire_write_lock()?;
        self.load_snapshot_unlocked()
    }

    fn load_snapshot_unlocked(&self) -> Result<ProjectSnapshot> {
        let mut documents = BTreeMap::new();
        self.collect_json(Path::new(""), &mut documents)?;
        for document in documents.values() {
            validate_schema_document(document)?;
        }
        Ok(ProjectSnapshot { documents })
    }

    pub fn save_snapshot(&self, snapshot: &ProjectSnapshot) -> Result<()> {
        let _write_lock = self.acquire_write_lock()?;
        let current = self.load_snapshot_unlocked()?;
        let mut operations = Vec::new();
        for path in current.documents.keys() {
            if !snapshot.documents.contains_key(path) {
                operations.push(JsonOperation::Delete { path: path.clone() });
            }
        }
        operations.extend(
            snapshot
                .documents
                .iter()
                .map(|(path, document)| JsonOperation::Write {
                    path: path.clone(),
                    document: document.clone(),
                }),
        );
        let transaction = JsonTransaction {
            schema_version: SCHEMA_VERSION,
            operations,
        };
        let pending = recovery::read(&self.recovery_path())?;
        if let Some(last) = pending.last()
            && hash_snapshot(&snapshot.documents)? != last.after_snapshot_hash
        {
            return Err(Error::InvalidTransaction(
                "snapshot does not include all pending recovery transactions".into(),
            ));
        }
        self.apply_transaction_unlocked(&transaction)?;
        if pending.is_empty() {
            Ok(())
        } else {
            recovery::clear(&self.recovery_path())
        }
    }

    pub fn apply_transaction(&self, transaction: &JsonTransaction) -> Result<()> {
        let _write_lock = self.acquire_write_lock()?;
        self.reject_pending_recovery()?;
        self.apply_transaction_unlocked(transaction)
    }

    fn apply_transaction_unlocked(&self, transaction: &JsonTransaction) -> Result<()> {
        self.recover_interrupted_write_unlocked()?;
        self.validate_transaction(transaction)?;
        if transaction.operations.is_empty() {
            return Ok(());
        }

        let transaction_root = self.root.join(".gaw/atomic").join(unique_suffix());
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
            let existed = target.exists();
            manifest.entries.push(AtomicEntry {
                path: path.clone(),
                existed,
                write: matches!(operation, JsonOperation::Write { .. }),
            });
            if let JsonOperation::Write { document, .. } = operation {
                let staged = staged_root.join(path.as_path());
                write_json_file(&staged, document)?;
            }
        }
        write_json_file(&transaction_root.join("manifest.json"), &manifest)?;
        sync_directory_tree(&transaction_root)?;
        sync_directory(transaction_root.parent().unwrap_or(&self.root))?;

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
            if let Some(parent) = self.safe_target(&entry.path)?.parent() {
                target_parents.insert(parent.to_owned());
            }
        }
        for parent in target_parents {
            sync_directory(&parent)?;
        }
        let committed = transaction_root.with_extension("committed");
        let marker = File::create(&committed).map_err(|error| io(&committed, error))?;
        marker.sync_all().map_err(|error| io(&committed, error))?;
        sync_directory(transaction_root.parent().unwrap_or(&self.root))?;
        fs::remove_dir_all(&transaction_root).map_err(|error| io(&transaction_root, error))?;
        fs::remove_file(&committed).map_err(|error| io(&committed, error))?;
        sync_directory(transaction_root.parent().unwrap_or(&self.root))?;
        Ok(())
    }

    /// Durably journals, applies, and checkpoints one transaction.
    ///
    /// A crash after the journal append can be completed by `recover`; a clean
    /// checkpoint removes the temporary journal as required by the project
    /// format. The operations themselves are absolute document replacements,
    /// so replay after a crash between apply and clear is idempotent.
    pub fn commit_transaction(&self, transaction: &JsonTransaction) -> Result<()> {
        let _write_lock = self.acquire_write_lock()?;
        if !recovery::read(&self.recovery_path())?.is_empty() {
            return Err(Error::InvalidTransaction(
                "pending recovery must be applied before committing a new transaction".into(),
            ));
        }
        self.append_recovery_unlocked(transaction)?;
        self.apply_transaction_unlocked(transaction)?;
        recovery::clear(&self.recovery_path())
    }

    pub fn validate(&self) -> Result<ValidationReport> {
        let _write_lock = self.acquire_write_lock()?;
        self.validate_unlocked()
    }

    fn validate_unlocked(&self) -> Result<ValidationReport> {
        let mut report = ValidationReport::default();
        let snapshot = match self.load_snapshot_unlocked() {
            Ok(snapshot) => snapshot,
            Err(error) => {
                report.errors.push(ValidationIssue {
                    path: self.root.display().to_string(),
                    message: error.to_string(),
                });
                return Ok(report);
            }
        };
        for (path, document) in &snapshot.documents {
            if !document.is_object() {
                report.errors.push(ValidationIssue {
                    path: path.to_string(),
                    message: "canonical JSON document must be an object".into(),
                });
            }
            if let Err(error) = validate_schema_document(document) {
                report.errors.push(ValidationIssue {
                    path: path.to_string(),
                    message: error.to_string(),
                });
            }
        }
        for required in ["project.json", "assets/index.json"] {
            let path = ProjectPath::new(required)?;
            if !snapshot.documents.contains_key(&path) {
                report.errors.push(ValidationIssue {
                    path: required.into(),
                    message: "required document is missing".into(),
                });
            }
        }
        if let Err(error) = validate_snapshot_relationships(&snapshot.documents) {
            report.errors.push(ValidationIssue {
                path: self.root.display().to_string(),
                message: error.to_string(),
            });
        }
        self.validate_media(&mut report)?;
        Ok(report)
    }

    pub fn import_media(&self, source: impl AsRef<Path>) -> Result<ImportedMedia> {
        let _write_lock = self.acquire_write_lock()?;
        self.reject_pending_recovery()?;
        let source = source.as_ref();
        let original_filename = source
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| Error::InvalidPath(source.display().to_string()))?
            .to_owned();
        let extension = source
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
            .filter(|ext| {
                !ext.is_empty()
                    && ext.len() <= 16
                    && ext.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
            .unwrap_or_else(|| "bin".into());
        let media_root = self.root.join("assets/media");
        fs::create_dir_all(&media_root).map_err(|error| io(&media_root, error))?;
        reject_symlink(&media_root)?;

        let mut input = File::open(source).map_err(|error| io(source, error))?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(&media_root).map_err(|error| io(&media_root, error))?;
        let mut hasher = Sha256::new();
        let mut byte_len = 0_u64;
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            let read = input.read(&mut buffer).map_err(|error| io(source, error))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            temporary
                .write_all(&buffer[..read])
                .map_err(|error| io(temporary.path(), error))?;
            byte_len += u64::try_from(read).unwrap_or(u64::MAX);
        }
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| io(temporary.path(), error))?;
        let content_hash = format!("{:x}", hasher.finalize());
        let relative_path = ProjectPath::new(format!("assets/media/{content_hash}.{extension}"))?;
        let target = self.safe_target(&relative_path)?;
        match temporary.persist_noclobber(&target) {
            Ok(_) => sync_directory(&media_root)?,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                reject_symlink(&target)?;
                if !target.is_file() || hash_file(&target)? != content_hash {
                    return Err(Error::InvalidTransaction(format!(
                        "existing media does not match content hash {content_hash}"
                    )));
                }
            }
            Err(error) => return Err(io(&target, error.error)),
        }
        let asset_id = format!("ast_{}", &content_hash[..16]);
        let imported = ImportedMedia {
            asset_id: asset_id.clone(),
            content_hash,
            relative_path,
            original_filename,
            byte_len,
        };
        self.index_import(&imported)?;
        Ok(imported)
    }

    pub fn append_recovery(&self, transaction: &JsonTransaction) -> Result<RecoveryRecord> {
        let _write_lock = self.acquire_write_lock()?;
        self.append_recovery_unlocked(transaction)
    }

    pub fn pending_recovery(&self) -> Result<Vec<RecoveryRecord>> {
        recovery::read(&self.recovery_path())
    }

    pub fn recover(&self) -> Result<usize> {
        let _write_lock = self.acquire_write_lock()?;
        let records = recovery::read(&self.recovery_path())?;
        let mut documents = self.load_snapshot_unlocked()?.documents;
        let mut current_hash = hash_snapshot(&documents)?;
        let start = if records
            .first()
            .is_none_or(|record| current_hash == record.before_snapshot_hash)
        {
            0
        } else if let Some(position) = records
            .iter()
            .position(|record| current_hash == record.after_snapshot_hash)
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
            let prospective = self.validate_transaction_against(&record.transaction, &documents)?;
            let prospective_hash = hash_snapshot(&prospective)?;
            if prospective_hash != record.after_snapshot_hash {
                return Err(Error::InvalidTransaction(
                    "recovery transaction does not produce its recorded snapshot".into(),
                ));
            }
            self.apply_transaction_unlocked(&record.transaction)?;
            documents = prospective;
            current_hash = prospective_hash;
        }
        recovery::clear(&self.recovery_path())?;
        Ok(records.len())
    }

    pub fn clear_recovery(&self) -> Result<()> {
        let _write_lock = self.acquire_write_lock()?;
        recovery::clear(&self.recovery_path())
    }

    fn append_recovery_unlocked(&self, transaction: &JsonTransaction) -> Result<RecoveryRecord> {
        let records = recovery::read(&self.recovery_path())?;
        let mut documents = self.load_snapshot_unlocked()?.documents;
        let base_hash = hash_snapshot(&documents)?;
        let mut expected_hash = base_hash;
        for record in &records {
            if record.before_snapshot_hash != expected_hash {
                return Err(Error::InvalidTransaction(
                    "recovery journal does not belong to the current snapshot".into(),
                ));
            }
            documents = self.validate_transaction_against(&record.transaction, &documents)?;
            expected_hash = hash_snapshot(&documents)?;
            if record.after_snapshot_hash != expected_hash {
                return Err(Error::InvalidTransaction(
                    "recovery transaction does not produce its recorded snapshot".into(),
                ));
            }
        }
        let before_snapshot_hash = expected_hash;
        let prospective = self.validate_transaction_against(transaction, &documents)?;
        let after_snapshot_hash = hash_snapshot(&prospective)?;
        recovery::append(
            &self.recovery_path(),
            transaction,
            before_snapshot_hash,
            after_snapshot_hash,
        )
    }

    fn validate_transaction(&self, transaction: &JsonTransaction) -> Result<()> {
        let current = self.load_snapshot_unlocked()?.documents;
        self.validate_transaction_against(transaction, &current)?;
        Ok(())
    }

    fn validate_transaction_against(
        &self,
        transaction: &JsonTransaction,
        current: &BTreeMap<ProjectPath, Value>,
    ) -> Result<BTreeMap<ProjectPath, Value>> {
        check_schema(u64::from(transaction.schema_version))?;
        let mut seen = BTreeSet::new();
        for operation in &transaction.operations {
            let path = operation.path();
            if !path.is_canonical_json() {
                return Err(Error::InvalidTransaction(format!(
                    "{path} is not a canonical JSON path"
                )));
            }
            if !seen.insert(path) {
                return Err(Error::InvalidTransaction(format!(
                    "{path} occurs more than once"
                )));
            }
            self.safe_target(path)?;
            if let JsonOperation::Write { document, .. } = operation {
                validate_schema_document(document)?;
            }
            if matches!(operation, JsonOperation::Delete { .. }) && path.as_str() == "project.json"
            {
                return Err(Error::InvalidTransaction(
                    "project.json cannot be deleted".into(),
                ));
            }
        }
        let mut prospective = current.clone();
        for operation in &transaction.operations {
            match operation {
                JsonOperation::Write { path, document } => {
                    prospective.insert(path.clone(), document.clone());
                }
                JsonOperation::Delete { path } => {
                    prospective.remove(path);
                }
            }
        }
        for required in ["project.json", "assets/index.json"] {
            let required = ProjectPath::new(required)?;
            if !prospective.contains_key(&required) {
                return Err(Error::InvalidTransaction(format!(
                    "transaction would leave required document {required} missing"
                )));
            }
        }
        validate_snapshot_relationships(&prospective)?;
        Ok(prospective)
    }

    fn collect_json(
        &self,
        relative: &Path,
        documents: &mut BTreeMap<ProjectPath, Value>,
    ) -> Result<()> {
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
                if project_path.is_canonical_json() {
                    documents.insert(project_path.clone(), self.read_json(&project_path)?);
                }
            }
        }
        Ok(())
    }

    fn read_json(&self, path: &ProjectPath) -> Result<Value> {
        let target = self.safe_target(path)?;
        let file = File::open(&target).map_err(|error| io(&target, error))?;
        serde_json::from_reader(file).map_err(|source| Error::Json {
            path: target,
            source,
        })
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

    fn recovery_path(&self) -> PathBuf {
        self.root.join(".gaw/recovery.journal")
    }

    fn recover_interrupted_write_unlocked(&self) -> Result<()> {
        let atomic_root = self.root.join(".gaw/atomic");
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
            if !path.exists() {
                continue;
            }
            if path
                .extension()
                .is_some_and(|extension| extension == "committed")
            {
                if !path.exists() {
                    continue;
                }
                let transaction = path.with_extension("");
                if transaction.exists() {
                    fs::remove_dir_all(&transaction).map_err(|error| io(&transaction, error))?;
                }
                fs::remove_file(&path).map_err(|error| io(&path, error))?;
                sync_directory(&atomic_root)?;
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
            check_schema(manifest.schema_version.into())?;
            let committed = path.with_extension("committed");
            if committed.exists() {
                fs::remove_dir_all(&path).map_err(|error| io(&path, error))?;
                fs::remove_file(&committed).map_err(|error| io(&committed, error))?;
                sync_directory(&atomic_root)?;
            } else {
                self.rollback(&path, &manifest)?;
                sync_directory(&atomic_root)?;
            }
        }
        Ok(())
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
        let path = runtime.join("write.lock");
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
        if recovery::read(&self.recovery_path())?.is_empty() {
            Ok(())
        } else {
            Err(Error::InvalidTransaction(
                "pending recovery must be applied before mutating the project".into(),
            ))
        }
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

    fn index_import(&self, imported: &ImportedMedia) -> Result<()> {
        let path = ProjectPath::new("assets/index.json")?;
        let mut index = self.read_json(&path)?;
        let assets = index
            .as_object_mut()
            .and_then(|object| object.get_mut("assets"))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                Error::InvalidTransaction("assets/index.json must contain an assets object".into())
            })?;
        assets.insert(
            imported.asset_id.clone(),
            json!({
                "id": imported.asset_id,
                "kind": "imported",
                "content_hash": imported.content_hash,
                "media_path": imported.relative_path,
                "original_filename": imported.original_filename,
                "byte_len": imported.byte_len
            }),
        );
        self.apply_transaction_unlocked(&JsonTransaction {
            schema_version: SCHEMA_VERSION,
            operations: vec![JsonOperation::Write {
                path,
                document: index,
            }],
        })
    }

    fn validate_media(&self, report: &mut ValidationReport) -> Result<()> {
        let media_root = self.root.join("assets/media");
        let index_path = ProjectPath::new("assets/index.json")?;
        if let Ok(index) = self.read_json(&index_path)
            && let Some(assets) = index.get("assets").and_then(Value::as_object)
        {
            for (asset_id, asset) in assets {
                let Some(path) = asset.get("media_path").and_then(Value::as_str) else {
                    continue;
                };
                let Ok(path) = ProjectPath::new(path) else {
                    report.errors.push(ValidationIssue {
                        path: format!("assets/index.json#{asset_id}"),
                        message: "invalid media_path".into(),
                    });
                    continue;
                };
                if !path.as_str().starts_with("assets/media/") {
                    report.errors.push(ValidationIssue {
                        path: format!("assets/index.json#{asset_id}"),
                        message: "media_path is outside assets/media".into(),
                    });
                    continue;
                }
                let target = self.safe_target(&path)?;
                if !target.is_file() {
                    report.errors.push(ValidationIssue {
                        path: path.to_string(),
                        message: "referenced media file is missing".into(),
                    });
                    continue;
                }
                let expected = asset.get("content_hash").and_then(Value::as_str);
                if let Some(expected) = expected
                    && hash_file(&target)? != expected
                {
                    report.errors.push(ValidationIssue {
                        path: path.to_string(),
                        message: "referenced media content hash does not match".into(),
                    });
                }
                if let Some(expected) = asset.get("byte_len").and_then(Value::as_u64)
                    && target.metadata().map_err(|error| io(&target, error))?.len() != expected
                {
                    report.errors.push(ValidationIssue {
                        path: path.to_string(),
                        message: "referenced media byte length does not match".into(),
                    });
                }
            }
        }
        for entry in fs::read_dir(&media_root).map_err(|error| io(&media_root, error))? {
            let entry = entry.map_err(|error| io(&media_root, error))?;
            let file_type = entry.file_type().map_err(|error| io(entry.path(), error))?;
            if file_type.is_symlink() || !file_type.is_file() {
                report.errors.push(ValidationIssue {
                    path: entry.path().display().to_string(),
                    message: "media entry must be a regular file".into(),
                });
                continue;
            }
            let expected = entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default()
                .to_owned();
            let actual = hash_file(&entry.path())?;
            if expected != actual {
                report.errors.push(ValidationIssue {
                    path: entry.path().display().to_string(),
                    message: "media filename does not match its SHA-256 content hash".into(),
                });
            }
        }
        Ok(())
    }
}

impl JsonOperation {
    fn path(&self) -> &ProjectPath {
        match self {
            Self::Write { path, .. } | Self::Delete { path } => path,
        }
    }
}

impl ValidationReport {
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

pub(crate) fn check_schema(found: u64) -> Result<()> {
    if found == u64::from(SCHEMA_VERSION) {
        Ok(())
    } else {
        Err(Error::UnsupportedSchema {
            found,
            expected: SCHEMA_VERSION,
        })
    }
}

fn validate_schema_document(document: &Value) -> Result<()> {
    let found = document
        .as_object()
        .and_then(|object| object.get("schema_version"))
        .and_then(Value::as_u64)
        .ok_or(Error::MissingSchemaVersion)?;
    check_schema(found)
}

fn validate_snapshot_relationships(documents: &BTreeMap<ProjectPath, Value>) -> Result<()> {
    let root_id = validate_project_header(documents)?;
    validate_asset_index(documents)?;
    let root_path = ProjectPath::new(format!("compositions/{root_id}/composition.json"))?;
    if !documents.contains_key(&root_path) {
        return Err(Error::InvalidTransaction(format!(
            "root composition {root_id} is missing"
        )));
    }

    let mut composition_edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (path, document) in documents {
        if path.as_str().ends_with("/composition.json") {
            validate_composition(path, document, documents, &mut composition_edges)?;
        } else if path.as_str().contains("/tracks/") {
            validate_track(path, document, documents, &mut composition_edges)?;
        } else if path.as_str().contains("/automation/") {
            let lane_id = path
                .as_str()
                .rsplit('/')
                .next()
                .and_then(|file| file.strip_suffix(".json"))
                .unwrap_or_default();
            let document_id = required_string(document, path.as_str(), "id")?;
            require_id(document_id, "lane_", &format!("{path}#id"))?;
            if document_id != lane_id {
                return invalid_field(&format!("{path}#id"), "must match its filename");
            }
            required_string(document, path.as_str(), "target")?;
            required_array(document, path.as_str(), "segments")?;
        }
    }
    reject_composition_cycles(&composition_edges, &root_id)?;
    Ok(())
}

fn validate_project_header(documents: &BTreeMap<ProjectPath, Value>) -> Result<String> {
    let project_path = ProjectPath::new("project.json")?;
    let project = documents
        .get(&project_path)
        .ok_or_else(|| Error::InvalidTransaction("project.json is missing".into()))?;
    let project_id = required_string(project, "project.json", "id")?;
    require_id(project_id, "prj_", "project.json#id")?;
    required_string(project, "project.json", "name")?;
    let root_id = required_string(project, "project.json", "root_composition_id")?;
    require_id(root_id, "cmp_", "project.json#root_composition_id")?;
    let bpm = required_number(project, "project.json", "bpm")?;
    if !bpm.is_finite() || bpm <= 0.0 {
        return invalid_field("project.json#bpm", "must be finite and greater than zero");
    }
    let sample_rate = required_value(project, "project.json", "internal_sample_rate_hz")?
        .as_u64()
        .ok_or_else(|| {
            Error::InvalidTransaction(
                "project.json#internal_sample_rate_hz must be a positive integer".into(),
            )
        })?;
    if sample_rate == 0 {
        return invalid_field(
            "project.json#internal_sample_rate_hz",
            "must be greater than zero",
        );
    }
    required_object(project, "project.json", "settings")?;
    Ok(root_id.into())
}

fn validate_asset_index(documents: &BTreeMap<ProjectPath, Value>) -> Result<()> {
    let assets_path = ProjectPath::new("assets/index.json")?;
    let assets_index = documents
        .get(&assets_path)
        .ok_or_else(|| Error::InvalidTransaction("assets/index.json is missing".into()))?;
    for (asset_id, asset) in required_object(assets_index, "assets/index.json", "assets")? {
        require_id(asset_id, "ast_", "assets/index.json asset key")?;
        if required_string(asset, "asset entry", "id")? != asset_id {
            return invalid_field("asset entry#id", "must match its assets map key");
        }
        if required_string(asset, "asset entry", "kind")? == "imported" {
            let hash = required_string(asset, "imported asset", "content_hash")?;
            require_hash(hash, "imported asset#content_hash")?;
            let media_path =
                ProjectPath::new(required_string(asset, "imported asset", "media_path")?)?;
            if !media_path.as_str().starts_with("assets/media/") {
                return invalid_field("imported asset#media_path", "must be inside assets/media");
            }
            required_string(asset, "imported asset", "original_filename")?;
            required_value(asset, "imported asset", "byte_len")?
                .as_u64()
                .ok_or_else(|| {
                    Error::InvalidTransaction(
                        "imported asset#byte_len must be a nonnegative integer".into(),
                    )
                })?;
        }
    }
    Ok(())
}

fn validate_composition(
    path: &ProjectPath,
    document: &Value,
    documents: &BTreeMap<ProjectPath, Value>,
    composition_edges: &mut BTreeMap<String, BTreeSet<String>>,
) -> Result<()> {
    let directory_id = path.as_str().split('/').nth(1).unwrap_or_default();
    let document_id = required_string(document, path.as_str(), "id")?;
    require_id(document_id, "cmp_", &format!("{path}#id"))?;
    if document_id != directory_id {
        return Err(Error::InvalidTransaction(format!(
            "composition id {document_id} does not match directory {directory_id}"
        )));
    }
    required_string(document, path.as_str(), "name")?;
    let length = required_object(document, path.as_str(), "length")?;
    if length.get("unit").and_then(Value::as_str) != Some("beats")
        || length
            .get("value")
            .and_then(Value::as_f64)
            .is_none_or(|value| !value.is_finite() || value < 0.0)
    {
        return invalid_field(
            &format!("{path}#length"),
            "must be a nonnegative beat quantity",
        );
    }
    if !matches!(
        required_string(document, path.as_str(), "output_layout")?,
        "mono" | "stereo"
    ) {
        return invalid_field(&format!("{path}#output_layout"), "must be mono or stereo");
    }
    required_array(document, path.as_str(), "output_effects")?;
    let mut seen_tracks = BTreeSet::new();
    for track_id in required_array(document, path.as_str(), "track_ids")? {
        let track_id = track_id.as_str().ok_or_else(|| {
            Error::InvalidTransaction(format!("{path}#track_ids must contain only strings"))
        })?;
        require_id(track_id, "trk_", &format!("{path}#track_ids"))?;
        if !seen_tracks.insert(track_id) {
            return invalid_field(&format!("{path}#track_ids"), "must not contain duplicates");
        }
        let track = ProjectPath::new(format!(
            "compositions/{directory_id}/tracks/{track_id}.json"
        ))?;
        if !documents.contains_key(&track) {
            return Err(Error::InvalidTransaction(format!(
                "composition {directory_id} references missing track {track_id}"
            )));
        }
    }
    composition_edges.entry(directory_id.into()).or_default();
    Ok(())
}

fn validate_track(
    path: &ProjectPath,
    document: &Value,
    documents: &BTreeMap<ProjectPath, Value>,
    composition_edges: &mut BTreeMap<String, BTreeSet<String>>,
) -> Result<()> {
    let parts = path.as_str().split('/').collect::<Vec<_>>();
    let composition_id = parts[1];
    let track_id = parts[3].strip_suffix(".json").unwrap_or_default();
    let document_id = required_string(document, path.as_str(), "id")?;
    require_id(document_id, "trk_", &format!("{path}#id"))?;
    if document_id != track_id {
        return invalid_field(&format!("{path}#id"), "must match its filename");
    }
    required_string(document, path.as_str(), "name")?;
    required_array(document, path.as_str(), "effects")?;
    for clip in required_array(document, path.as_str(), "clips")? {
        if clip.get("kind").and_then(Value::as_str) == Some("composition") {
            let child = clip
                .get("composition_id")
                .or_else(|| clip.get("child_composition_id"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::InvalidTransaction(format!(
                        "{path} composition clip is missing composition_id"
                    ))
                })?;
            require_id(child, "cmp_", &format!("{path} composition clip"))?;
            let child_path = ProjectPath::new(format!("compositions/{child}/composition.json"))?;
            if !documents.contains_key(&child_path) {
                return Err(Error::InvalidTransaction(format!(
                    "{path} references missing child composition {child}"
                )));
            }
            composition_edges
                .entry(composition_id.into())
                .or_default()
                .insert(child.into());
        }
    }
    Ok(())
}

fn reject_composition_cycles(
    edges: &BTreeMap<String, BTreeSet<String>>,
    root_id: &str,
) -> Result<()> {
    fn visit(
        node: &str,
        edges: &BTreeMap<String, BTreeSet<String>>,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
    ) -> Result<()> {
        if visiting.contains(node) {
            return Err(Error::InvalidTransaction(format!(
                "composition ownership cycle includes {node}"
            )));
        }
        if !visited.insert(node.into()) {
            return Ok(());
        }
        visiting.insert(node.into());
        if let Some(children) = edges.get(node) {
            for child in children {
                visit(child, edges, visiting, visited)?;
            }
        }
        visiting.remove(node);
        Ok(())
    }

    let mut parents: BTreeMap<&str, &str> = BTreeMap::new();
    for (parent, children) in edges {
        for child in children {
            if child == root_id {
                return invalid_field(
                    "project.json#root_composition_id",
                    "cannot be owned by another composition",
                );
            }
            if let Some(previous) = parents.insert(child, parent)
                && previous != parent
            {
                return Err(Error::InvalidTransaction(format!(
                    "composition {child} is owned by both {previous} and {parent}"
                )));
            }
        }
    }
    for composition in edges.keys() {
        if composition != root_id && !parents.contains_key(composition.as_str()) {
            return Err(Error::InvalidTransaction(format!(
                "composition {composition} is not owned by the root hierarchy"
            )));
        }
    }

    let mut visited = BTreeSet::new();
    for node in edges.keys() {
        visit(node, edges, &mut BTreeSet::new(), &mut visited)?;
    }
    Ok(())
}

fn required_value<'a>(document: &'a Value, path: &str, field: &str) -> Result<&'a Value> {
    document
        .as_object()
        .and_then(|object| object.get(field))
        .ok_or_else(|| {
            Error::InvalidTransaction(format!("{path} is missing required field {field}"))
        })
}

fn required_string<'a>(document: &'a Value, path: &str, field: &str) -> Result<&'a str> {
    required_value(document, path, field)?
        .as_str()
        .ok_or_else(|| Error::InvalidTransaction(format!("{path}#{field} must be a string")))
}

fn required_number(document: &Value, path: &str, field: &str) -> Result<f64> {
    required_value(document, path, field)?
        .as_f64()
        .ok_or_else(|| Error::InvalidTransaction(format!("{path}#{field} must be a number")))
}

fn required_object<'a>(
    document: &'a Value,
    path: &str,
    field: &str,
) -> Result<&'a serde_json::Map<String, Value>> {
    required_value(document, path, field)?
        .as_object()
        .ok_or_else(|| Error::InvalidTransaction(format!("{path}#{field} must be an object")))
}

fn required_array<'a>(document: &'a Value, path: &str, field: &str) -> Result<&'a Vec<Value>> {
    required_value(document, path, field)?
        .as_array()
        .ok_or_else(|| Error::InvalidTransaction(format!("{path}#{field} must be an array")))
}

fn require_id(value: &str, prefix: &str, path: &str) -> Result<()> {
    if valid_id(value, prefix) {
        Ok(())
    } else {
        invalid_field(path, &format!("must be a portable {prefix} identifier"))
    }
}

fn require_hash(value: &str, path: &str) -> Result<()> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        invalid_field(path, "must be a 64-character lowercase SHA-256 hash")
    }
}

fn invalid_field<T>(path: &str, message: &str) -> Result<T> {
    Err(Error::InvalidTransaction(format!("{path} {message}")))
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

fn hash_snapshot(documents: &BTreeMap<ProjectPath, Value>) -> Result<String> {
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

    #[test]
    fn creates_reopens_and_validates_a_complete_snapshot() {
        let (_directory, store) = project();
        assert_eq!(store.load_snapshot().unwrap().documents.len(), 3);
        assert!(store.validate().unwrap().is_valid());
        ProjectStore::open(store.root()).unwrap();
    }

    #[test]
    fn rejected_transaction_does_not_partially_write() {
        let (_directory, store) = project();
        let track = ProjectPath::new("compositions/cmp_test/tracks/trk_one.json").unwrap();
        let transaction = JsonTransaction {
            schema_version: SCHEMA_VERSION,
            operations: vec![
                JsonOperation::Write {
                    path: track.clone(),
                    document: json!({"schema_version": SCHEMA_VERSION}),
                },
                JsonOperation::Delete {
                    path: ProjectPath::new("assets/index.json").unwrap(),
                },
            ],
        };
        assert!(store.apply_transaction(&transaction).is_err());
        assert!(!store.root().join(track.as_path()).exists());
        assert!(store.root().join("assets/index.json").exists());
    }

    #[test]
    fn import_deduplicates_and_detects_corruption() {
        let (directory, store) = project();
        let source = directory.path().join("Kick.WAV");
        fs::write(&source, b"audio bytes").unwrap();
        let first = store.import_media(&source).unwrap();
        let second = store.import_media(&source).unwrap();
        assert_eq!(first.relative_path, second.relative_path);
        assert_eq!(
            fs::read_dir(store.root().join("assets/media"))
                .unwrap()
                .count(),
            1
        );
        fs::write(store.root().join(first.relative_path.as_path()), b"corrupt").unwrap();
        assert!(store.import_media(&source).is_err());
        assert!(!store.validate().unwrap().is_valid());
    }

    #[test]
    fn concurrent_imports_do_not_lose_index_entries() {
        use std::sync::{Arc, Barrier};

        let (directory, store) = project();
        let first = directory.path().join("first.wav");
        let second = directory.path().join("second.wav");
        fs::write(&first, b"first audio").unwrap();
        fs::write(&second, b"second audio").unwrap();
        let store = Arc::new(store);
        let barrier = Arc::new(Barrier::new(3));
        let handles = [first, second].map(|source| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store.import_media(source).unwrap();
            })
        });
        barrier.wait();
        for handle in handles {
            handle.join().unwrap();
        }
        let index = &store.load_snapshot().unwrap().documents
            [&ProjectPath::new("assets/index.json").unwrap()];
        assert_eq!(index["assets"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn recovery_ignores_torn_tail_and_replays_complete_record() {
        let (_directory, store) = project();
        let (path, mut document) = store
            .load_snapshot()
            .unwrap()
            .documents
            .into_iter()
            .find(|(path, _)| path.as_str().ends_with("composition.json"))
            .unwrap();
        document["name"] = Value::String("Recovered".into());
        store
            .append_recovery(&JsonTransaction {
                schema_version: SCHEMA_VERSION,
                operations: vec![JsonOperation::Write {
                    path: path.clone(),
                    document,
                }],
            })
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(store.recovery_path())
            .unwrap()
            .write_all(br#"{"torn":"#)
            .unwrap();
        assert_eq!(store.pending_recovery().unwrap().len(), 1);
        assert_eq!(store.recover().unwrap(), 1);
        assert_eq!(
            store.load_snapshot().unwrap().documents[&path]["name"],
            "Recovered"
        );
        assert!(store.pending_recovery().unwrap().is_empty());
    }

    #[test]
    fn recovery_hash_chain_handles_multiple_and_already_applied_records() {
        let (_directory, store) = project();
        let (path, mut document) = store
            .load_snapshot()
            .unwrap()
            .documents
            .into_iter()
            .find(|(path, _)| path.as_str().ends_with("composition.json"))
            .unwrap();
        document["name"] = Value::String("One".into());
        let first = JsonTransaction {
            schema_version: SCHEMA_VERSION,
            operations: vec![JsonOperation::Write {
                path: path.clone(),
                document: document.clone(),
            }],
        };
        store.append_recovery(&first).unwrap();
        document["name"] = Value::String("Two".into());
        store
            .append_recovery(&JsonTransaction {
                schema_version: SCHEMA_VERSION,
                operations: vec![JsonOperation::Write {
                    path: path.clone(),
                    document,
                }],
            })
            .unwrap();
        assert_eq!(store.recover().unwrap(), 2);
        assert_eq!(
            store.load_snapshot().unwrap().documents[&path]["name"],
            "Two"
        );

        store.append_recovery(&first).unwrap();
        {
            let _write_lock = store.acquire_write_lock().unwrap();
            store.apply_transaction_unlocked(&first).unwrap();
        }
        assert_eq!(store.recover().unwrap(), 1);
        assert!(store.pending_recovery().unwrap().is_empty());
    }

    #[test]
    fn recovery_rejects_a_journal_for_a_different_snapshot() {
        let (_directory, store) = project();
        let mut snapshot = store.load_snapshot().unwrap();
        let project_path = ProjectPath::new("project.json").unwrap();
        let mut intended = snapshot.documents[&project_path].clone();
        intended["name"] = Value::String("Intended".into());
        store
            .append_recovery(&JsonTransaction {
                schema_version: SCHEMA_VERSION,
                operations: vec![JsonOperation::Write {
                    path: project_path.clone(),
                    document: intended,
                }],
            })
            .unwrap();

        snapshot.documents.get_mut(&project_path).unwrap()["name"] =
            Value::String("Unrelated".into());
        write_json_file(
            &store.root.join(project_path.as_path()),
            &snapshot.documents[&project_path],
        )
        .unwrap();
        assert!(store.recover().is_err());
        assert_eq!(store.pending_recovery().unwrap().len(), 1);
    }

    #[test]
    fn rejects_schema_mismatch_in_any_document() {
        let (_directory, store) = project();
        let path = store
            .load_snapshot()
            .unwrap()
            .documents
            .into_keys()
            .find(|path| path.as_str().ends_with("composition.json"))
            .unwrap();
        fs::write(
            store.root().join(path.as_path()),
            br#"{"schema_version":999}"#,
        )
        .unwrap();
        assert!(matches!(
            ProjectStore::open(store.root()),
            Err(Error::UnsupportedSchema { .. })
        ));
        let report = ProjectStore::validate_path(store.root()).unwrap();
        assert!(!report.is_valid());
    }

    #[test]
    fn rejects_structurally_incomplete_canonical_documents() {
        let (_directory, store) = project();
        write_json_file(
            &store.root.join("project.json"),
            &json!({"schema_version": SCHEMA_VERSION}),
        )
        .unwrap();
        assert!(matches!(
            ProjectStore::open(store.root()),
            Err(Error::InvalidTransaction(_))
        ));
        assert!(
            !ProjectStore::validate_path(store.root())
                .unwrap()
                .is_valid()
        );
    }

    #[test]
    fn rejects_composition_ownership_cycles_atomically() {
        let (_directory, store) = project();
        let (root_path, mut root) = store
            .load_snapshot()
            .unwrap()
            .documents
            .into_iter()
            .find(|(path, _)| path.as_str().ends_with("composition.json"))
            .unwrap();
        let root_id = root["id"].as_str().unwrap().to_owned();
        root["track_ids"] = json!(["trk_root"]);
        let child_path = ProjectPath::new("compositions/cmp_child/composition.json").unwrap();
        let transaction = JsonTransaction {
            schema_version: SCHEMA_VERSION,
            operations: vec![
                JsonOperation::Write {
                    path: root_path,
                    document: root,
                },
                JsonOperation::Write {
                    path: ProjectPath::new(format!("compositions/{root_id}/tracks/trk_root.json"))
                        .unwrap(),
                    document: json!({
                        "schema_version": SCHEMA_VERSION,
                        "id": "trk_root", "name": "Root", "effects": [],
                        "clips": [{"kind": "composition", "composition_id": "cmp_child"}]
                    }),
                },
                JsonOperation::Write {
                    path: child_path.clone(),
                    document: json!({
                        "schema_version": SCHEMA_VERSION,
                        "id": "cmp_child", "name": "Child",
                        "length": {"unit": "beats", "value": 4.0},
                        "output_layout": "stereo", "track_ids": ["trk_child"],
                        "output_effects": []
                    }),
                },
                JsonOperation::Write {
                    path: ProjectPath::new("compositions/cmp_child/tracks/trk_child.json").unwrap(),
                    document: json!({
                        "schema_version": SCHEMA_VERSION,
                        "id": "trk_child", "name": "Child", "effects": [],
                        "clips": [{"kind": "composition", "composition_id": root_id}]
                    }),
                },
            ],
        };
        assert!(store.apply_transaction(&transaction).is_err());
        assert!(!store.root.join(child_path.as_path()).exists());
        assert!(store.validate().unwrap().is_valid());
    }

    #[test]
    fn transaction_envelope_is_strict_and_versioned() {
        assert!(
            serde_json::from_value::<JsonTransaction>(json!({
                "operations": []
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<JsonTransaction>(json!({
                "schema_version": SCHEMA_VERSION,
                "operations": [],
                "unexpected": true
            }))
            .is_err()
        );

        let (_directory, store) = project();
        assert!(matches!(
            store.apply_transaction(&JsonTransaction {
                schema_version: SCHEMA_VERSION + 1,
                operations: Vec::new(),
            }),
            Err(Error::UnsupportedSchema { .. })
        ));

        let incomplete = JsonTransaction {
            schema_version: SCHEMA_VERSION,
            operations: vec![JsonOperation::Write {
                path: ProjectPath::new("project.json").unwrap(),
                document: json!({"schema_version": SCHEMA_VERSION}),
            }],
        };
        assert!(store.apply_transaction(&incomplete).is_err());
        assert!(store.validate().unwrap().is_valid());
    }

    #[test]
    fn new_commit_does_not_discard_pending_recovery() {
        let (directory, store) = project();
        let transaction = JsonTransaction::default();
        store.append_recovery(&transaction).unwrap();
        assert!(store.commit_transaction(&transaction).is_err());
        assert!(store.apply_transaction(&transaction).is_err());
        let source = directory.path().join("pending.wav");
        fs::write(&source, b"pending audio").unwrap();
        assert!(store.import_media(source).is_err());
        assert_eq!(store.pending_recovery().unwrap().len(), 1);
        assert_eq!(store.recover().unwrap(), 1);
    }

    #[test]
    fn interrupted_atomic_write_rolls_back_without_commit_marker() {
        let (_directory, store) = project();
        let (path, old_document) = store
            .load_snapshot()
            .unwrap()
            .documents
            .into_iter()
            .find(|(path, _)| path.as_str().ends_with("composition.json"))
            .unwrap();
        let transaction_root = store.root.join(".gaw/atomic/interrupted");
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
        assert_eq!(
            reopened.load_snapshot().unwrap().documents[&path],
            old_document
        );
    }

    #[test]
    fn interrupted_cleanup_keeps_snapshot_with_commit_marker() {
        let (_directory, store) = project();
        let (path, old_document) = store
            .load_snapshot()
            .unwrap()
            .documents
            .into_iter()
            .find(|(path, _)| path.as_str().ends_with("composition.json"))
            .unwrap();
        let transaction_root = store.root.join(".gaw/atomic/committed");
        let backup = transaction_root.join("backup").join(path.as_path());
        make_parent(&backup).unwrap();
        let target = store.root.join(path.as_path());
        fs::rename(&target, &backup).unwrap();
        let mut new_document = old_document;
        new_document["name"] = Value::String("Committed".into());
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
        write_json_file(&transaction_root.with_extension("committed"), &json!({})).unwrap();

        let reopened = ProjectStore::open(store.root()).unwrap();
        assert_eq!(
            reopened.load_snapshot().unwrap().documents[&path]["name"],
            "Committed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_in_canonical_tree() {
        use std::os::unix::fs::symlink;

        let (directory, store) = project();
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, store.root().join("compositions/cmp_link")).unwrap();
        assert!(matches!(store.load_snapshot(), Err(Error::Symlink(_))));
    }
}
