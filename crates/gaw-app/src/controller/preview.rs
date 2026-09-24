use std::{
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread,
};

use crossbeam_channel::{Receiver, Sender, bounded};
use gaw_audio::RenderSnapshot;

type PreviewResult = Result<Arc<RenderSnapshot>, String>;

#[derive(Debug)]
struct Request {
    path: PathBuf,
    revision: u64,
    result: Sender<PreviewResult>,
}

#[derive(Debug, Default)]
struct State {
    pending: Option<Request>,
    closed: bool,
}

/// Serializes full-WAV decodes and retains only the latest pending selection.
#[derive(Debug)]
pub(super) struct PreviewWorker {
    state: Arc<(Mutex<State>, Condvar)>,
}

impl PreviewWorker {
    pub(super) fn spawn(
        mut load: impl FnMut(PathBuf, u64) -> PreviewResult + Send + 'static,
    ) -> Self {
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let worker = Arc::clone(&state);
        thread::Builder::new()
            .name("gaw-asset-preview".into())
            .spawn(move || {
                loop {
                    let request = {
                        let (lock, ready) = &*worker;
                        let mut state = lock
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        while state.pending.is_none() && !state.closed {
                            state = ready
                                .wait(state)
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                        }
                        if state.closed {
                            return;
                        }
                        state.pending.take().expect("preview requested")
                    };
                    let result = load(request.path, request.revision);
                    let _ = request.result.send(result);
                }
            })
            .expect("asset preview worker should start");
        Self { state }
    }

    pub(super) fn request(&self, path: PathBuf, revision: u64) -> Receiver<PreviewResult> {
        let (result, receiver) = bounded(1);
        let (lock, ready) = &*self.state;
        lock.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending = Some(Request {
            path,
            revision,
            result,
        });
        ready.notify_one();
        receiver
    }

    pub(super) fn cancel_pending(&self) {
        self.state
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending = None;
    }
}

impl Drop for PreviewWorker {
    fn drop(&mut self) {
        let (lock, ready) = &*self.state;
        let mut state = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = None;
        state.closed = true;
        ready.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn rapid_preview_selection_decodes_only_active_and_latest_requests() {
        let (started, starts) = bounded(2);
        let (release, released) = bounded(1);
        let worker = PreviewWorker::spawn(move |_, revision| {
            started.send(revision).unwrap();
            released.recv().unwrap();
            Err("test decoder".into())
        });
        let first = worker.request(PathBuf::from("first.wav"), 0);
        assert_eq!(starts.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
        let mut latest = None;
        for revision in 1..10_000 {
            latest = Some(worker.request(PathBuf::from("next.wav"), revision));
        }
        assert!(starts.is_empty(), "decodes must never overlap");
        release.send(()).unwrap();
        assert!(first.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        assert_eq!(starts.recv_timeout(Duration::from_secs(2)).unwrap(), 9_999);
        let cancelled = worker.request(PathBuf::from("cancelled.wav"), 10_000);
        worker.cancel_pending();
        assert!(cancelled.recv_timeout(Duration::from_secs(2)).is_err());
        release.send(()).unwrap();
        assert!(
            latest
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .is_err()
        );
        assert!(starts.is_empty());
    }
}
