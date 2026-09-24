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
        if store.pending_recovery_count()? != 0 {
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
        self.project = self
            .store
            .apply_session_transaction(&self.project, transaction)?;
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
        if self.batch_started.is_none() {
            return Ok(());
        }
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
    use gaw_core::{Bpm, Command, TrackId, Transaction};

    use super::*;

    fn legacy_event_store() -> (tempfile::TempDir, ProjectStore, gaw_core::ProcessorStack) {
        let directory = tempfile::tempdir().unwrap();
        let mut project = Project::new(
            "Legacy",
            Bpm::new(120.0).unwrap(),
            gaw_core::SampleRate::new(48_000).unwrap(),
        );
        let data = gaw_core::EventData::new("Notes");
        let clip = gaw_core::EventClip::new(
            data.id,
            gaw_core::Beats::new(0.0).unwrap(),
            gaw_core::Beats::new(1.0).unwrap(),
        );
        let mut track = gaw_core::Track::event(
            project.root_composition_id,
            "Sampler",
            gaw_core::Instrument::sampler("Sampler", gaw_core::Sampler::new(8).unwrap()),
        );
        let stack = gaw_core::ProcessorStack::Clip {
            track_id: track.id,
            clip_id: clip.id,
        };
        track.clips.push(gaw_core::Clip::Event(clip));
        let relative = format!(
            "compositions/{}/tracks/{}.json",
            project.root_composition_id, track.id
        );
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        project.event_data.push(data);
        let store = ProjectStore::create(directory.path().join("song"), &project).unwrap();
        let path = store.root().join(relative);
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        json["clips"][0]["data"]
            .as_object_mut()
            .unwrap()
            .remove("effects");
        std::fs::write(path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
        (directory, store, stack)
    }

    #[test]
    fn legacy_event_effects_can_be_edited_checkpointed_and_recovered() {
        for recover in [false, true] {
            let (_directory, store, stack) = legacy_event_store();
            let mut session = ProjectSession::open(store.clone()).unwrap();
            let transaction = Transaction::new([Command::InsertProcessor {
                stack,
                index: 0,
                processor: gaw_core::Processor::new(
                    gaw_core::ProcessorId::new("legacy-pitch").unwrap(),
                    gaw_core::ProcessorKind::PitchShift(gaw_core::PitchShiftParameters {
                        semitones: 7,
                        ..gaw_core::PitchShiftParameters::default()
                    }),
                ),
            }]);
            let mut expected = session.project().clone();
            transaction.apply(&mut expected).unwrap();
            session.apply_transaction(&transaction).unwrap();
            assert_eq!(session.project(), &expected);
            assert_eq!(store.pending_recovery().unwrap().len(), 1);
            if recover {
                drop(session);
                session = ProjectSession::open(store.clone()).unwrap();
                assert_eq!(session.project(), &expected);
            }
            session.close().unwrap();
            assert_eq!(store.load_project().unwrap(), expected);
            assert!(store.pending_recovery().unwrap().is_empty());
        }
    }

    #[test]
    fn legacy_event_session_still_rejects_a_concurrent_tempo_change() {
        let (_directory, store, _) = legacy_event_store();
        let mut session = ProjectSession::open(store.clone()).unwrap();
        let before = session.project().clone();
        store
            .commit_transaction(&Transaction::new([Command::SetProjectTempo {
                bpm: Bpm::new(98.0).unwrap(),
            }]))
            .unwrap();
        assert!(
            session
                .apply_transaction(&Transaction::new([Command::SetProjectName {
                    name: "Stale".into()
                }]))
                .is_err()
        );
        assert_eq!(session.project(), &before);
        assert_eq!(store.load_project().unwrap().bpm, Bpm::new(98.0).unwrap());
        assert!(store.pending_recovery().unwrap().is_empty());
    }

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

    #[test]
    fn rejected_transaction_is_not_journaled() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            ProjectStore::create_default(directory.path().join("song"), "Before", 120.0, 48_000)
                .unwrap();
        let mut session = ProjectSession::open(store.clone()).unwrap();
        session
            .apply_transaction(&Transaction::new([Command::RemoveTrack {
                track_id: TrackId::new(),
            }]))
            .unwrap_err();
        assert!(store.pending_recovery().unwrap().is_empty());
    }

    #[test]
    fn stale_session_does_not_change_memory_or_journal() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            ProjectStore::create_default(directory.path().join("song"), "Before", 120.0, 48_000)
                .unwrap();
        let mut session = ProjectSession::open(store.clone()).unwrap();
        store
            .commit_transaction(&Transaction::new([Command::SetProjectName {
                name: "External".into(),
            }]))
            .unwrap();
        session
            .apply_transaction(&Transaction::new([Command::SetProjectName {
                name: "Stale".into(),
            }]))
            .unwrap_err();
        assert_eq!(session.project().name, "Before");
        assert_eq!(store.load_project().unwrap().name, "External");
        assert!(store.pending_recovery().unwrap().is_empty());
    }

    #[test]
    fn clean_close_does_not_overwrite_an_external_commit() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            ProjectStore::create_default(directory.path().join("song"), "Before", 120.0, 48_000)
                .unwrap();
        let session = ProjectSession::open(store.clone()).unwrap();
        store
            .commit_transaction(&Transaction::new([Command::SetProjectName {
                name: "External".into(),
            }]))
            .unwrap();
        session.close().unwrap();
        assert_eq!(store.load_project().unwrap().name, "External");
    }
}
