use std::time::{Duration, Instant};

use gaw_core::{Project, Transaction};

use crate::{ProjectStore, Result};

/// Maximum interval during which journaled edits share one canonical checkpoint.
pub const CHECKPOINT_WINDOW: Duration = Duration::from_millis(250);

/// An editing lifecycle that journals transactions immediately and snapshots on idle/close.
#[derive(Debug)]
pub struct ProjectSession {
    store: ProjectStore,
    project: Project,
    batch_started: Option<Instant>,
}

impl ProjectSession {
    /// Opens a session, replaying any journal left by a prior crashed session.
    pub fn open(store: ProjectStore) -> Result<Self> {
        if !store.pending_recovery()?.is_empty() {
            store.recover()?;
        }
        let project = store.load_project()?;
        Ok(Self {
            store,
            project,
            batch_started: None,
        })
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    /// Applies one atomic model transition and immediately appends it to recovery.
    pub fn apply_transaction(&mut self, transaction: &Transaction) -> Result<()> {
        let now = Instant::now();
        if self
            .batch_started
            .is_some_and(|started| now.duration_since(started) >= CHECKPOINT_WINDOW)
        {
            self.checkpoint()?;
        }
        self.store.append_recovery(transaction)?;
        transaction.apply(&mut self.project)?;
        self.batch_started.get_or_insert(now);
        Ok(())
    }

    /// Writes one grouped snapshot after the bounded journal window becomes idle.
    pub fn checkpoint_if_idle(&mut self) -> Result<bool> {
        if self
            .batch_started
            .is_some_and(|started| started.elapsed() >= CHECKPOINT_WINDOW)
        {
            self.checkpoint()?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Explicitly writes the current project and clears its fully represented journal.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.store.checkpoint_project(&self.project)?;
        self.batch_started = None;
        Ok(())
    }

    /// Clean-close checkpoint. Dropping a session without this leaves recovery records.
    pub fn close(mut self) -> Result<()> {
        self.checkpoint()
    }

    pub fn store(&self) -> &ProjectStore {
        &self.store
    }
}

#[cfg(test)]
mod tests {
    use gaw_core::{Bpm, Command, Transaction};

    use super::*;

    #[test]
    fn journal_group_checkpoints_on_explicit_clean_close() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            ProjectStore::create_default(directory.path().join("song"), "Before", 120.0, 48_000)
                .unwrap();
        let mut session = ProjectSession::open(store.clone()).unwrap();
        session
            .apply_transaction(&Transaction::new([Command::SetProjectName {
                name: "After".into(),
            }]))
            .unwrap();
        session
            .apply_transaction(&Transaction::new([Command::SetProjectTempo {
                bpm: Bpm::new(98.0).unwrap(),
            }]))
            .unwrap();
        assert_eq!(store.pending_recovery().unwrap().len(), 2);
        assert_eq!(store.load_project().unwrap().name, "Before");
        session.close().unwrap();
        assert!(store.pending_recovery().unwrap().is_empty());
        let project = store.load_project().unwrap();
        assert_eq!(project.name, "After");
        assert!((project.bpm.value() - 98.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dropped_dirty_session_is_recovered_on_next_open() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            ProjectStore::create_default(directory.path().join("song"), "Before", 120.0, 48_000)
                .unwrap();
        let mut session = ProjectSession::open(store.clone()).unwrap();
        session
            .apply_transaction(&Transaction::new([Command::SetProjectName {
                name: "Recovered".into(),
            }]))
            .unwrap();
        drop(session);
        assert_eq!(store.load_project().unwrap().name, "Before");
        let recovered = ProjectSession::open(store).unwrap();
        assert_eq!(recovered.project().name, "Recovered");
    }
}
