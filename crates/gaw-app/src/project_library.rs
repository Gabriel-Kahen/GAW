use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use gaw_project::ProjectStore;
use serde::{Deserialize, Serialize};

const REGISTRY_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct RegistryFile {
    schema_version: u32,
    roots: Vec<PathBuf>,
    projects: Vec<KnownProject>,
}

impl Default for RegistryFile {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            roots: Vec::new(),
            projects: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct KnownProject {
    path: PathBuf,
    name: String,
    project_id: Option<String>,
    last_opened_unix_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectLibrary {
    registry_path: PathBuf,
    registry: RegistryFile,
    pub(crate) warning: Option<String>,
    writable: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectEntry {
    pub(crate) path: PathBuf,
    pub(crate) name: String,
    pub(crate) project_id: Option<String>,
    pub(crate) bpm: Option<f64>,
    pub(crate) sample_rate: Option<u32>,
    pub(crate) last_opened_unix_ms: Option<u64>,
    pub(crate) status: ProjectStatus,
    pub(crate) managed: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum ProjectStatus {
    Available,
    Missing,
    Invalid(String),
}

impl ProjectEntry {
    pub(crate) fn searchable_text(&self) -> String {
        format!("{} {}", self.name, self.path.display()).to_lowercase()
    }
}

impl ProjectLibrary {
    pub(crate) fn load_default() -> Self {
        Self::load(registry_path(), default_projects_root())
    }

    fn load(registry_path: PathBuf, default_root: PathBuf) -> Self {
        let (mut registry, warning, writable) = match fs::read(&registry_path) {
            Ok(bytes) => match serde_json::from_slice::<RegistryFile>(&bytes) {
                Ok(value) if value.schema_version == REGISTRY_SCHEMA_VERSION => (value, None, true),
                Ok(value) => (
                    RegistryFile::default(),
                    Some(format!(
                        "Project catalog version {} is not supported. It was preserved and catalog changes are disabled",
                        value.schema_version
                    )),
                    false,
                ),
                Err(error) => (
                    RegistryFile::default(),
                    Some(format!(
                        "Could not read the project catalog. It was preserved and catalog changes are disabled: {error}"
                    )),
                    false,
                ),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (RegistryFile::default(), None, true)
            }
            Err(error) => (
                RegistryFile::default(),
                Some(format!(
                    "Could not read the project catalog. Catalog changes are disabled: {error}"
                )),
                false,
            ),
        };
        if registry.roots.is_empty() {
            registry.roots.push(default_root);
        }
        normalize_registry(&mut registry);
        Self {
            registry_path,
            registry,
            warning,
            writable,
        }
    }

    pub(crate) fn primary_root(&self) -> &Path {
        &self.registry.roots[0]
    }

    pub(crate) fn roots(&self) -> &[PathBuf] {
        &self.registry.roots
    }

    pub(crate) fn scan(&self) -> (Vec<ProjectEntry>, Vec<String>) {
        let mut candidates = BTreeMap::<PathBuf, KnownProject>::new();
        for project in &self.registry.projects {
            candidates.insert(project.path.clone(), project.clone());
        }

        let mut warnings = Vec::new();
        for root in &self.registry.roots {
            if !root.exists() {
                continue;
            }
            let children = match fs::read_dir(root) {
                Ok(children) => children,
                Err(error) => {
                    warnings.push(format!("Could not scan {}: {error}", root.display()));
                    continue;
                }
            };
            for child in children.flatten() {
                let path = child.path();
                let hidden = child
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'));
                if !hidden && path.is_dir() && path.join("project.json").is_file() {
                    let path = canonical_or_absolute(&path);
                    candidates
                        .entry(path.clone())
                        .or_insert_with(|| KnownProject {
                            path,
                            ..KnownProject::default()
                        });
                }
            }
        }

        let mut entries = candidates
            .into_values()
            .map(|known| self.inspect_known(known))
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            right
                .last_opened_unix_ms
                .cmp(&left.last_opened_unix_ms)
                .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                .then_with(|| left.path.cmp(&right.path))
        });
        (entries, warnings)
    }

    fn inspect_known(&self, known: KnownProject) -> ProjectEntry {
        let managed = self
            .registry
            .roots
            .iter()
            .any(|root| known.path.starts_with(root));
        if !known.path.is_dir() {
            return ProjectEntry {
                path: known.path,
                name: fallback_name(&known.name, None),
                project_id: known.project_id,
                bpm: None,
                sample_rate: None,
                last_opened_unix_ms: known.last_opened_unix_ms,
                status: ProjectStatus::Missing,
                managed,
            };
        }
        match ProjectStore::probe_manifest(&known.path) {
            Ok(manifest) => ProjectEntry {
                path: canonical_or_absolute(&known.path),
                name: manifest.name,
                project_id: Some(manifest.id.to_string()),
                bpm: Some(manifest.bpm.value()),
                sample_rate: Some(manifest.sample_rate.value()),
                last_opened_unix_ms: known.last_opened_unix_ms,
                status: ProjectStatus::Available,
                managed,
            },
            Err(error) => ProjectEntry {
                path: known.path.clone(),
                name: fallback_name(&known.name, known.path.file_name()),
                project_id: known.project_id,
                bpm: None,
                sample_rate: None,
                last_opened_unix_ms: known.last_opened_unix_ms,
                status: ProjectStatus::Invalid(error.to_string()),
                managed,
            },
        }
    }

    pub(crate) fn remember(&mut self, path: &Path, opened: bool) -> Result<(), String> {
        let normalized = canonical_or_absolute(path);
        let manifest = ProjectStore::probe_manifest(&normalized).ok();
        let now = opened.then(now_unix_ms);
        self.mutate(move |registry| {
            if let Some(project) = registry
                .projects
                .iter_mut()
                .find(|project| project.path == normalized)
            {
                if let Some(manifest) = manifest {
                    project.name = manifest.name;
                    project.project_id = Some(manifest.id.to_string());
                }
                if now.is_some() {
                    project.last_opened_unix_ms = now;
                }
            } else {
                registry.projects.push(KnownProject {
                    path: normalized,
                    name: manifest
                        .as_ref()
                        .map_or_else(String::new, |value| value.name.clone()),
                    project_id: manifest.map(|value| value.id.to_string()),
                    last_opened_unix_ms: now,
                });
            }
            Ok(())
        })
    }

    pub(crate) fn forget(&mut self, path: &Path) -> Result<(), String> {
        let normalized = canonical_or_absolute(path);
        self.mutate(move |registry| {
            registry
                .projects
                .retain(|project| project.path != normalized);
            Ok(())
        })
    }

    pub(crate) fn add_root(&mut self, root: &Path) -> Result<(), String> {
        let root = canonical_or_absolute(root);
        self.mutate(move |registry| {
            if !registry.roots.contains(&root) {
                registry.roots.push(root);
            }
            Ok(())
        })
    }

    pub(crate) fn remove_root(&mut self, root: &Path) -> Result<(), String> {
        let root = canonical_or_absolute(root);
        self.mutate(move |registry| {
            if registry.roots.len() == 1 {
                return Err("GAW needs at least one project location".into());
            }
            registry.roots.retain(|candidate| candidate != &root);
            Ok(())
        })
    }

    fn mutate(
        &mut self,
        mutation: impl FnOnce(&mut RegistryFile) -> Result<(), String>,
    ) -> Result<(), String> {
        if !self.writable {
            return Err(self.warning.clone().unwrap_or_else(|| {
                "The project catalog is read-only; the existing file was preserved".into()
            }));
        }
        let parent = self
            .registry_path
            .parent()
            .ok_or_else(|| "project catalog path has no parent".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create {}: {error}", parent.display()))?;
        let lock_path = self.registry_path.with_extension("lock");
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| format!("Could not lock the project catalog: {error}"))?;
        FileExt::lock_exclusive(&lock)
            .map_err(|error| format!("Could not lock the project catalog: {error}"))?;
        let mut registry = match fs::read(&self.registry_path) {
            Ok(bytes) => {
                let registry = serde_json::from_slice::<RegistryFile>(&bytes)
                    .map_err(|error| format!("Could not reload the project catalog: {error}"))?;
                if registry.schema_version != REGISTRY_SCHEMA_VERSION {
                    return Err(format!(
                        "Project catalog version {} is not supported; the file was preserved",
                        registry.schema_version
                    ));
                }
                registry
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.registry.clone(),
            Err(error) => return Err(format!("Could not reload the project catalog: {error}")),
        };
        // Reloading an empty catalog must retain the roots normalized by load().
        if registry.roots.is_empty() {
            registry.roots.clone_from(&self.registry.roots);
        }
        mutation(&mut registry)?;
        normalize_registry(&mut registry);
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|error| format!("Could not stage the project catalog: {error}"))?;
        serde_json::to_writer_pretty(&mut temporary, &registry)
            .map_err(|error| format!("Could not encode the project catalog: {error}"))?;
        temporary
            .write_all(b"\n")
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|error| format!("Could not sync the project catalog: {error}"))?;
        temporary
            .persist(&self.registry_path)
            .map_err(|error| format!("Could not publish the project catalog: {}", error.error))?;
        self.registry = registry;
        Ok(())
    }
}

fn normalize_registry(registry: &mut RegistryFile) {
    for root in &mut registry.roots {
        *root = canonical_or_absolute(root);
    }
    let mut roots = BTreeSet::new();
    registry.roots.retain(|root| roots.insert(root.clone()));
    for project in &mut registry.projects {
        project.path = canonical_or_absolute(&project.path);
    }
    registry
        .projects
        .sort_by(|left, right| left.path.cmp(&right.path));
    registry
        .projects
        .dedup_by(|left, right| left.path == right.path);
}

fn canonical_or_absolute(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        }
    })
}

fn fallback_name(cached: &str, file_name: Option<&std::ffi::OsStr>) -> String {
    if cached.trim().is_empty() {
        file_name
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("Unknown project")
            .to_owned()
    } else {
        cached.to_owned()
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
        })
}

fn registry_path() -> PathBuf {
    if let Some(root) = std::env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(root).join("gaw/projects-v1.json");
    }
    if let Some(root) = std::env::var_os("APPDATA") {
        return PathBuf::from(root).join("GAW/projects-v1.json");
    }
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(".gaw/projects-v1.json"),
        |home| PathBuf::from(home).join(".local/share/gaw/projects-v1.json"),
    )
}

fn default_projects_root() -> PathBuf {
    if let Some(root) = std::env::var_os("GAW_PROJECTS_DIR") {
        return PathBuf::from(root);
    }
    if let Ok(current) = std::env::current_dir() {
        let local = current.join("projects");
        if contains_project(&local) {
            return canonical_or_absolute(&local);
        }
    }
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from("GAW Projects"),
        |home| PathBuf::from(home).join("Documents/GAW Projects"),
    )
}

fn contains_project(root: &Path) -> bool {
    fs::read_dir(root).is_ok_and(|children| {
        children
            .flatten()
            .any(|child| child.path().join("project.json").is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library(directory: &tempfile::TempDir) -> ProjectLibrary {
        ProjectLibrary::load(
            directory.path().join("data/projects-v1.json"),
            directory.path().join("projects"),
        )
    }

    #[test]
    fn scans_managed_projects_and_reads_manifest_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let mut library = library(&directory);
        let song = library.primary_root().join("song");
        ProjectStore::create_default(&song, "Night Drive", 128.0, 48_000).unwrap();

        let (entries, warnings) = library.scan();

        assert!(warnings.is_empty());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Night Drive");
        assert_eq!(entries[0].bpm, Some(128.0));
        assert!(entries[0].managed);
        library.remember(&song, true).unwrap();
        assert!(library.registry_path.is_file());
    }

    #[test]
    fn missing_external_project_stays_in_the_catalog_until_forgotten() {
        let directory = tempfile::tempdir().unwrap();
        let mut library = library(&directory);
        let song = directory.path().join("external/song");
        ProjectStore::create_default(&song, "Portable", 90.0, 44_100).unwrap();
        library.remember(&song, true).unwrap();
        fs::remove_dir_all(&song).unwrap();

        let (entries, _) = library.scan();

        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].status, ProjectStatus::Missing));
        assert_eq!(entries[0].name, "Portable");
        library.forget(&song).unwrap();
        assert!(library.scan().0.is_empty());
    }

    #[test]
    fn registry_round_trips_roots_and_recent_projects() {
        let directory = tempfile::tempdir().unwrap();
        let mut library = library(&directory);
        let second_root = directory.path().join("second");
        library.add_root(&second_root).unwrap();
        let song = directory.path().join("song");
        ProjectStore::create_default(&song, "Song", 120.0, 96_000).unwrap();
        library.remember(&song, true).unwrap();

        let loaded = ProjectLibrary::load(library.registry_path.clone(), PathBuf::from("unused"));

        assert!(
            loaded
                .roots()
                .contains(&canonical_or_absolute(&second_root))
        );
        assert_eq!(loaded.scan().0[0].name, "Song");
    }

    #[test]
    fn invalid_project_is_isolated_to_its_own_entry() {
        let directory = tempfile::tempdir().unwrap();
        let mut library = library(&directory);
        let broken = directory.path().join("broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join("project.json"), b"not json").unwrap();
        library.remember(&broken, false).unwrap();

        let entries = library.scan().0;

        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].status, ProjectStatus::Invalid(_)));
    }

    #[test]
    fn mutations_reload_the_catalog_while_holding_the_lock() {
        let directory = tempfile::tempdir().unwrap();
        let mut first = library(&directory);
        let mut second = library(&directory);
        let first_root = directory.path().join("first");
        let second_root = directory.path().join("second");

        first.add_root(&first_root).unwrap();
        second.add_root(&second_root).unwrap();

        let loaded = library(&directory);
        assert!(loaded.roots().contains(&canonical_or_absolute(&first_root)));
        assert!(
            loaded
                .roots()
                .contains(&canonical_or_absolute(&second_root))
        );
    }

    #[test]
    fn mutations_preserve_the_default_root_when_the_catalog_has_no_roots() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data/projects-v1.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::to_vec(&RegistryFile::default()).unwrap()).unwrap();
        let mut library = library(&directory);
        let root = canonical_or_absolute(&directory.path().join("projects"));
        assert_eq!(library.primary_root(), root);

        let missing = directory.path().join("missing-song");
        library.remember(&missing, false).unwrap();
        assert_eq!(library.primary_root(), root);
        let mut saved: RegistryFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.roots, vec![root.clone()]);
        assert_eq!(saved.projects.len(), 1);

        // A concurrent catalog edit can clear roots before the next mutation.
        saved.roots.clear();
        fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
        library.forget(&missing).unwrap();
        assert_eq!(library.primary_root(), root);
        let saved: RegistryFile = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.roots, vec![root]);
        assert!(saved.projects.is_empty());
    }

    #[test]
    fn corrupt_catalog_is_preserved_and_changes_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data/projects-v1.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"broken catalog").unwrap();
        let mut library = ProjectLibrary::load(path.clone(), directory.path().join("projects"));

        assert!(library.add_root(&directory.path().join("other")).is_err());
        assert_eq!(fs::read(path).unwrap(), b"broken catalog");
    }
}
