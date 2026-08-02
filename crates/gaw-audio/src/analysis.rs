//! Bounded, off-callback publication of ephemeral analyzer measurements.
//!
//! This module is control/background-plane infrastructure. Publishing may take
//! a short mutex and receiving/coalescing may allocate; neither API belongs in
//! an audio callback.

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError, bounded};
use gaw_core::AnalyzerMeasurement;
use parking_lot::Mutex;
use thiserror::Error;

/// Half-open timeline range measured by an analyzer publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnalyzerFrameRange {
    pub start_frame: u64,
    pub frame_count: u64,
}

impl AnalyzerFrameRange {
    pub const fn new(start_frame: u64, frame_count: u64) -> Self {
        Self {
            start_frame,
            frame_count,
        }
    }

    pub const fn end_frame(self) -> u64 {
        self.start_frame.saturating_add(self.frame_count)
    }
}

/// One immutable, revision-qualified analyzer result.
#[derive(Clone, Debug, PartialEq)]
pub struct AnalyzerPublication {
    pub processor_id: Arc<str>,
    pub render_revision: u64,
    pub range: AnalyzerFrameRange,
    /// Monotonic order assigned by this channel when the result is published.
    pub sequence: u64,
    pub measurement: AnalyzerMeasurement,
}

/// Failure to construct an analyzer publication channel.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum AnalyzerChannelError {
    #[error("analyzer publication capacity must be non-zero")]
    ZeroCapacity,
}

/// Result of a non-blocking analyzer publication attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnalyzerPublishStatus {
    Published {
        sequence: u64,
    },
    /// The result did not match the receiver's current render revision.
    Stale,
    /// The sole receiver has been dropped.
    ReceiverStopped,
    /// The channel exhausted its monotonic sequence space.
    SequenceExhausted,
}

#[derive(Debug)]
struct PublishState {
    sender: Sender<AnalyzerPublication>,
    evictor: Receiver<AnalyzerPublication>,
    next_sequence: Option<u64>,
}

/// Cloneable producer for a bounded analyzer channel.
///
/// On saturation, publication evicts the oldest queued result and inserts the
/// new one. It never waits for queue capacity, but it does serialize producers
/// with a control-plane mutex.
#[derive(Clone, Debug)]
pub struct AnalyzerPublisher {
    state: Arc<Mutex<PublishState>>,
    expected_revision: Arc<AtomicU64>,
    receiver_live: Arc<AtomicBool>,
}

impl AnalyzerPublisher {
    pub fn publish(
        &self,
        processor_id: impl Into<Arc<str>>,
        render_revision: u64,
        range: AnalyzerFrameRange,
        measurement: AnalyzerMeasurement,
    ) -> AnalyzerPublishStatus {
        if !self.receiver_live.load(Ordering::Acquire) {
            return AnalyzerPublishStatus::ReceiverStopped;
        }
        if self.expected_revision.load(Ordering::Acquire) != render_revision {
            return AnalyzerPublishStatus::Stale;
        }

        let mut state = self.state.lock();
        if !self.receiver_live.load(Ordering::Acquire) {
            return AnalyzerPublishStatus::ReceiverStopped;
        }
        if self.expected_revision.load(Ordering::Acquire) != render_revision {
            return AnalyzerPublishStatus::Stale;
        }
        let Some(sequence) = state.next_sequence else {
            return AnalyzerPublishStatus::SequenceExhausted;
        };
        state.next_sequence = sequence.checked_add(1);
        let publication = AnalyzerPublication {
            processor_id: processor_id.into(),
            render_revision,
            range,
            sequence,
            measurement,
        };

        let publication = match state.sender.try_send(publication) {
            Ok(()) => return AnalyzerPublishStatus::Published { sequence },
            Err(TrySendError::Disconnected(_)) => {
                return AnalyzerPublishStatus::ReceiverStopped;
            }
            Err(TrySendError::Full(publication)) => publication,
        };
        let _ = state.evictor.try_recv();
        match state.sender.try_send(publication) {
            Ok(()) => AnalyzerPublishStatus::Published { sequence },
            Err(TrySendError::Disconnected(_)) => AnalyzerPublishStatus::ReceiverStopped,
            Err(TrySendError::Full(_)) => {
                unreachable!("serialized producer could not refill a bounded queue")
            }
        }
    }
}

/// Sole consumer for a bounded analyzer channel.
#[derive(Debug)]
pub struct AnalyzerReceiver {
    receiver: Receiver<AnalyzerPublication>,
    expected_revision: Arc<AtomicU64>,
    receiver_live: Arc<AtomicBool>,
}

impl AnalyzerReceiver {
    /// Changes the accepted render revision. Queued results from older
    /// revisions remain bounded and are discarded while receiving.
    pub fn set_expected_revision(&self, render_revision: u64) {
        self.expected_revision
            .store(render_revision, Ordering::Release);
    }

    pub fn expected_revision(&self) -> u64 {
        self.expected_revision.load(Ordering::Acquire)
    }

    /// Receives one current result without waiting, suppressing queued stale
    /// revisions.
    pub fn try_recv(&self) -> Option<AnalyzerPublication> {
        loop {
            let publication = match self.receiver.try_recv() {
                Ok(publication) => publication,
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
            };
            let expected = self.expected_revision.load(Ordering::Acquire);
            if publication.render_revision == expected
                && self.expected_revision.load(Ordering::Acquire) == expected
            {
                return Some(publication);
            }
        }
    }

    /// Drains all currently queued results and returns only the newest current
    /// publication for each processor, ordered by processor ID.
    pub fn drain_latest(&self) -> Vec<AnalyzerPublication> {
        let expected = self.expected_revision.load(Ordering::Acquire);
        let mut latest = BTreeMap::<Arc<str>, AnalyzerPublication>::new();
        loop {
            match self.receiver.try_recv() {
                Ok(publication) if publication.render_revision == expected => {
                    let replace = latest
                        .get(&publication.processor_id)
                        .is_none_or(|old| old.sequence < publication.sequence);
                    if replace {
                        latest.insert(Arc::clone(&publication.processor_id), publication);
                    }
                }
                Ok(_) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        let current = self.expected_revision.load(Ordering::Acquire);
        if current != expected {
            return Vec::new();
        }
        latest.into_values().collect()
    }

    /// Number of bounded queue slots currently occupied, including stale
    /// publications not yet suppressed by a receive operation.
    pub fn pending_len(&self) -> usize {
        self.receiver.len()
    }
}

impl Drop for AnalyzerReceiver {
    fn drop(&mut self) {
        self.receiver_live.store(false, Ordering::Release);
    }
}

/// Creates one bounded latest-wins analyzer publication channel.
///
/// # Errors
///
/// Returns [`AnalyzerChannelError::ZeroCapacity`] when `capacity` is zero.
pub fn analyzer_channel(
    capacity: usize,
    expected_revision: u64,
) -> Result<(AnalyzerPublisher, AnalyzerReceiver), AnalyzerChannelError> {
    if capacity == 0 {
        return Err(AnalyzerChannelError::ZeroCapacity);
    }
    let (sender, receiver) = bounded(capacity);
    let expected_revision = Arc::new(AtomicU64::new(expected_revision));
    let receiver_live = Arc::new(AtomicBool::new(true));
    Ok((
        AnalyzerPublisher {
            state: Arc::new(Mutex::new(PublishState {
                sender,
                evictor: receiver.clone(),
                next_sequence: Some(1),
            })),
            expected_revision: Arc::clone(&expected_revision),
            receiver_live: Arc::clone(&receiver_live),
        },
        AnalyzerReceiver {
            receiver,
            expected_revision,
            receiver_live,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gaw_core::StereoMeasurement;

    fn measurement(width: f32) -> AnalyzerMeasurement {
        AnalyzerMeasurement::StereoMeter(StereoMeasurement {
            mid_level_dbfs: -12.0,
            side_level_dbfs: -18.0,
            correlation: 0.5,
            stereo_width: width,
        })
    }

    #[test]
    fn rejects_zero_capacity() {
        assert_eq!(
            analyzer_channel(0, 1).unwrap_err(),
            AnalyzerChannelError::ZeroCapacity
        );
    }

    #[test]
    fn publication_preserves_stable_identity_revision_range_and_sequence() {
        let (publisher, receiver) = analyzer_channel(2, 41).unwrap();
        assert_eq!(
            publisher.publish(
                "meter",
                41,
                AnalyzerFrameRange::new(128, 64),
                measurement(1.0),
            ),
            AnalyzerPublishStatus::Published { sequence: 1 }
        );
        let result = receiver.try_recv().unwrap();
        assert_eq!(result.processor_id.as_ref(), "meter");
        assert_eq!(result.render_revision, 41);
        assert_eq!(result.range, AnalyzerFrameRange::new(128, 64));
        assert_eq!(result.range.end_frame(), 192);
        assert_eq!(result.sequence, 1);
        assert_eq!(result.measurement, measurement(1.0));
    }

    #[test]
    fn saturated_queue_evicts_the_oldest_publication() {
        let (publisher, receiver) = analyzer_channel(2, 7).unwrap();
        for (processor, width) in [("first", 1.0), ("second", 2.0), ("third", 3.0)] {
            assert!(matches!(
                publisher.publish(
                    processor,
                    7,
                    AnalyzerFrameRange::new(0, 32),
                    measurement(width),
                ),
                AnalyzerPublishStatus::Published { .. }
            ));
        }
        assert_eq!(receiver.pending_len(), 2);
        let results = receiver.drain_latest();
        assert_eq!(
            results
                .iter()
                .map(|result| result.processor_id.as_ref())
                .collect::<Vec<_>>(),
            ["second", "third"]
        );
    }

    #[test]
    fn revision_change_rejects_late_and_suppresses_queued_results() {
        let (publisher, receiver) = analyzer_channel(4, 1).unwrap();
        assert!(matches!(
            publisher.publish(
                "old-queued",
                1,
                AnalyzerFrameRange::new(0, 8),
                measurement(1.0),
            ),
            AnalyzerPublishStatus::Published { .. }
        ));
        receiver.set_expected_revision(2);
        assert_eq!(
            publisher.publish(
                "old-late",
                1,
                AnalyzerFrameRange::new(8, 8),
                measurement(2.0),
            ),
            AnalyzerPublishStatus::Stale
        );
        assert!(matches!(
            publisher.publish(
                "current",
                2,
                AnalyzerFrameRange::new(16, 8),
                measurement(3.0),
            ),
            AnalyzerPublishStatus::Published { .. }
        ));
        let results = receiver.drain_latest();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].processor_id.as_ref(), "current");
        assert_eq!(results[0].render_revision, 2);
    }

    #[test]
    fn drain_coalesces_to_latest_result_per_processor() {
        let (publisher, receiver) = analyzer_channel(4, 9).unwrap();
        publisher.publish("meter", 9, AnalyzerFrameRange::new(0, 32), measurement(1.0));
        publisher.publish("other", 9, AnalyzerFrameRange::new(0, 32), measurement(2.0));
        publisher.publish(
            "meter",
            9,
            AnalyzerFrameRange::new(32, 32),
            measurement(3.0),
        );
        let results = receiver.drain_latest();
        assert_eq!(results.len(), 2);
        let meter = results
            .iter()
            .find(|result| result.processor_id.as_ref() == "meter")
            .unwrap();
        assert_eq!(meter.sequence, 3);
        assert_eq!(meter.range, AnalyzerFrameRange::new(32, 32));
        assert_eq!(meter.measurement, measurement(3.0));
    }

    #[test]
    fn reports_when_the_receiver_is_gone() {
        let (publisher, receiver) = analyzer_channel(1, 3).unwrap();
        drop(receiver);
        assert_eq!(
            publisher.publish("meter", 3, AnalyzerFrameRange::new(0, 1), measurement(1.0),),
            AnalyzerPublishStatus::ReceiverStopped
        );
    }
}
