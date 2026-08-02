//! Portable, hardware-free output-device recovery policy.
//!
//! Stream callbacks only publish generation-tagged errors into a bounded
//! channel. Device discovery, stream construction, retries, and delays belong
//! to the non-real-time control plane.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use cpal::{DeviceId, HostId, StreamError};
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use thiserror::Error;

/// Identity assigned to one stream construction attempt.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StreamGeneration(u64);

impl StreamGeneration {
    /// Largest generation representable by the atomic fatal-state lane.
    pub const MAX_VALUE: u64 = (u64::MAX >> 2) - 1;

    /// Creates a generation from a caller-persisted counter.
    ///
    /// # Panics
    ///
    /// Panics when `value` exceeds [`Self::MAX_VALUE`].
    pub const fn new(value: u64) -> Self {
        assert!(
            value <= Self::MAX_VALUE,
            "stream generation exceeds maximum"
        );
        Self(value)
    }

    /// Numeric generation value.
    pub const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum FatalStreamState {
    StreamInvalidated = 1,
    BackendSpecific = 2,
    DeviceNotAvailable = 3,
}

impl FatalStreamState {
    const fn from_error(error: &StreamError) -> Option<Self> {
        match error {
            StreamError::BufferUnderrun => None,
            StreamError::StreamInvalidated => Some(Self::StreamInvalidated),
            StreamError::DeviceNotAvailable => Some(Self::DeviceNotAvailable),
            StreamError::BackendSpecific { .. } => Some(Self::BackendSpecific),
        }
    }

    fn into_error(self) -> StreamError {
        match self {
            Self::StreamInvalidated => StreamError::StreamInvalidated,
            Self::DeviceNotAvailable => StreamError::DeviceNotAvailable,
            Self::BackendSpecific => StreamError::BackendSpecific {
                err: cpal::BackendSpecificError {
                    description: "coalesced backend stream error".into(),
                },
            },
        }
    }
}

#[derive(Debug, Default)]
struct CoalescedFatal(AtomicU64);

impl CoalescedFatal {
    const STATE_MASK: u64 = 0b11;

    fn publish(&self, generation: StreamGeneration, state: FatalStreamState) {
        let sequence = generation.value() + 1;
        let candidate = (sequence << 2) | state as u64;
        let mut current = self.0.load(Ordering::Relaxed);
        loop {
            let current_sequence = current >> 2;
            let current_state = current & Self::STATE_MASK;
            if sequence < current_sequence || (sequence == current_sequence && current_state == 0) {
                return;
            }
            let replacement = if sequence == current_sequence {
                candidate.max(current)
            } else {
                candidate
            };
            match self.0.compare_exchange_weak(
                current,
                replacement,
                Ordering::Release,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    fn take(&self) -> Option<StreamNotification> {
        let mut current = self.0.load(Ordering::Acquire);
        loop {
            let state = current & Self::STATE_MASK;
            if state == 0 {
                return None;
            }
            let consumed = current & !Self::STATE_MASK;
            match self.0.compare_exchange_weak(
                current,
                consumed,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    let generation = StreamGeneration::new((current >> 2) - 1);
                    let state = match state {
                        1 => FatalStreamState::StreamInvalidated,
                        2 => FatalStreamState::BackendSpecific,
                        3 => FatalStreamState::DeviceNotAvailable,
                        _ => unreachable!(),
                    };
                    return Some(StreamNotification {
                        generation,
                        error: state.into_error(),
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

/// One stream error tagged with the stream that emitted it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamNotification {
    pub generation: StreamGeneration,
    pub error: StreamError,
}

/// Callback-side endpoint of a bounded stream-notification channel.
#[derive(Clone, Debug)]
pub struct StreamNotificationSender {
    queue: Sender<StreamNotification>,
    fatal: Arc<CoalescedFatal>,
    receiver_live: Arc<AtomicBool>,
}

impl StreamNotificationSender {
    /// Builds the error callback passed to [`crate::CpalOutput`] open methods.
    /// The closure only performs bounded non-blocking queue or atomic work;
    /// recovery remains on the app/control thread.
    pub fn callback(
        &self,
        generation: StreamGeneration,
    ) -> impl FnMut(StreamError) + Send + 'static {
        let sender = self.clone();
        move |error| {
            let _ = sender.try_send(generation, error);
        }
    }

    /// Publishes without waiting, locking, formatting, or allocating.
    ///
    /// Fatal state is atomically coalesced once per generation. An underrun is
    /// returned if its bounded queue is full or the receiver has been dropped.
    ///
    /// # Errors
    ///
    /// Returns the notification when the channel is full or disconnected.
    pub fn try_send(
        &self,
        generation: StreamGeneration,
        error: StreamError,
    ) -> Result<(), StreamNotificationSendError> {
        if let Some(state) = FatalStreamState::from_error(&error) {
            if !self.receiver_live.load(Ordering::Acquire) {
                return Err(StreamNotificationSendError::Disconnected(
                    StreamNotification { generation, error },
                ));
            }
            self.fatal.publish(generation, state);
            return Ok(());
        }
        match self
            .queue
            .try_send(StreamNotification { generation, error })
        {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(notification)) => {
                Err(StreamNotificationSendError::Full(notification))
            }
            Err(TrySendError::Disconnected(notification)) => {
                Err(StreamNotificationSendError::Disconnected(notification))
            }
        }
    }
}

/// Control-plane endpoint of a bounded stream-notification channel.
#[derive(Debug)]
pub struct StreamNotificationReceiver {
    queue: Receiver<StreamNotification>,
    fatal: Arc<CoalescedFatal>,
    receiver_live: Arc<AtomicBool>,
}

impl StreamNotificationReceiver {
    /// Receives one queued notification without waiting.
    ///
    /// # Errors
    ///
    /// Returns whether the queue is empty or disconnected.
    pub fn try_recv(&self) -> Result<StreamNotification, StreamNotificationReceiveError> {
        if let Some(notification) = self.fatal.take() {
            return Ok(notification);
        }
        match self.queue.try_recv() {
            Ok(notification) => Ok(notification),
            Err(error) => Err(match error {
                TryRecvError::Empty => StreamNotificationReceiveError::Empty,
                TryRecvError::Disconnected => StreamNotificationReceiveError::Disconnected,
            }),
        }
    }
}

impl Drop for StreamNotificationReceiver {
    fn drop(&mut self) {
        self.receiver_live.store(false, Ordering::Release);
    }
}

/// Creates a preallocated bounded notification channel.
///
/// # Errors
///
/// Returns an error when `capacity` is zero.
pub fn stream_notification_channel(
    capacity: usize,
) -> Result<(StreamNotificationSender, StreamNotificationReceiver), StreamNotificationChannelError>
{
    if capacity == 0 {
        return Err(StreamNotificationChannelError::ZeroCapacity);
    }
    let (sender, receiver) = crossbeam_channel::bounded(capacity);
    let fatal = Arc::new(CoalescedFatal::default());
    let receiver_live = Arc::new(AtomicBool::new(true));
    Ok((
        StreamNotificationSender {
            queue: sender,
            fatal: Arc::clone(&fatal),
            receiver_live: Arc::clone(&receiver_live),
        },
        StreamNotificationReceiver {
            queue: receiver,
            fatal,
            receiver_live,
        },
    ))
}

/// A stream notification could not be published.
#[derive(Debug)]
pub enum StreamNotificationSendError {
    Full(StreamNotification),
    Disconnected(StreamNotification),
}

/// Non-blocking receive state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamNotificationReceiveError {
    Empty,
    Disconnected,
}

/// Notification-channel configuration errors.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum StreamNotificationChannelError {
    #[error("stream notification capacity must be nonzero")]
    ZeroCapacity,
}

/// Whether output tracks a backend default or a stable pinned device.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OutputDeviceSelection {
    FollowDefault { backend: HostId },
    Pinned { device_id: DeviceId },
}

impl OutputDeviceSelection {
    fn backend(&self) -> HostId {
        match self {
            Self::FollowDefault { backend } => *backend,
            Self::Pinned { device_id } => device_id.0,
        }
    }
}

/// Device target for the next control-plane open attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryTarget {
    Default { backend: HostId },
    Device { device_id: DeviceId },
}

/// One portable device observation made off the callback thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceObservation {
    /// Current backend default, if one exists.
    pub default_output: Option<DeviceId>,
    /// Whether the pinned device can currently be enumerated.
    /// Ignored for [`OutputDeviceSelection::FollowDefault`].
    pub pinned_available: bool,
}

/// Deterministic retry and fallback limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceRecoveryPolicy {
    /// Total open attempts for one target before fallback or exhaustion.
    pub maximum_attempts: u32,
    /// Delay after the first failed attempt.
    pub initial_retry_delay_millis: u64,
    /// Inclusive cap for exponential retry delays.
    pub maximum_retry_delay_millis: u64,
    /// Whether an unavailable pinned device falls back to its backend default.
    pub fallback_to_default: bool,
}

impl Default for DeviceRecoveryPolicy {
    fn default() -> Self {
        Self {
            maximum_attempts: 5,
            initial_retry_delay_millis: 100,
            maximum_retry_delay_millis: 5_000,
            fallback_to_default: true,
        }
    }
}

impl DeviceRecoveryPolicy {
    fn validate(self) -> Result<Self, DeviceRecoveryConfigError> {
        if self.maximum_attempts == 0 {
            return Err(DeviceRecoveryConfigError::ZeroMaximumAttempts);
        }
        if self.maximum_retry_delay_millis < self.initial_retry_delay_millis {
            return Err(DeviceRecoveryConfigError::RetryDelayOrder);
        }
        Ok(self)
    }

    fn delay(self, failed_attempt: u32) -> u64 {
        let shift = failed_attempt.saturating_sub(1).min(63);
        self.initial_retry_delay_millis
            .saturating_mul(1_u64 << shift)
            .min(self.maximum_retry_delay_millis)
    }
}

/// Recovery-controller configuration errors.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum DeviceRecoveryConfigError {
    #[error("maximum recovery attempts must be nonzero")]
    ZeroMaximumAttempts,
    #[error("maximum retry delay must be at least the initial retry delay")]
    RetryDelayOrder,
}

/// One action for the non-real-time stream owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceRecoveryAction {
    /// No device work is necessary.
    None,
    /// A transient underrun does not invalidate the current stream.
    Continue,
    /// Open the target and tag its error callback with `generation`.
    Open {
        generation: StreamGeneration,
        target: RecoveryTarget,
        attempt: u32,
    },
    /// Do not retry before this monotonic millisecond deadline.
    WaitUntil { monotonic_millis: u64 },
    /// Ignore an error emitted by a superseded stream.
    StaleNotification {
        notification: StreamGeneration,
        active: StreamGeneration,
    },
    /// The configured attempts and fallback have been exhausted.
    Exhausted {
        target: RecoveryTarget,
        attempts: u32,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RecoveryState {
    Running,
    Opening {
        generation: StreamGeneration,
        target: RecoveryTarget,
        attempt: u32,
    },
    Waiting {
        target: RecoveryTarget,
        attempt: u32,
        not_before: u64,
    },
    Exhausted {
        target: RecoveryTarget,
        attempts: u32,
    },
}

/// Pure control-plane state machine for portable CPAL recovery.
#[derive(Debug)]
pub struct DeviceRecoveryController {
    selection: OutputDeviceSelection,
    policy: DeviceRecoveryPolicy,
    active_generation: StreamGeneration,
    next_generation: u64,
    active_device: DeviceId,
    active_target: RecoveryTarget,
    state: RecoveryState,
}

impl DeviceRecoveryController {
    /// Starts in the running state for an already-open stream.
    ///
    /// # Errors
    ///
    /// Returns an error when retry limits or delays are inconsistent.
    pub fn new(
        selection: OutputDeviceSelection,
        policy: DeviceRecoveryPolicy,
        active_generation: StreamGeneration,
        active_device: DeviceId,
    ) -> Result<Self, DeviceRecoveryConfigError> {
        let policy = policy.validate()?;
        let active_target = match &selection {
            OutputDeviceSelection::FollowDefault { backend } => {
                RecoveryTarget::Default { backend: *backend }
            }
            OutputDeviceSelection::Pinned { device_id } if *device_id == active_device => {
                RecoveryTarget::Device {
                    device_id: device_id.clone(),
                }
            }
            OutputDeviceSelection::Pinned { .. } => RecoveryTarget::Default {
                backend: selection.backend(),
            },
        };
        Ok(Self {
            selection,
            policy,
            active_generation,
            next_generation: active_generation.value().saturating_add(1),
            active_device,
            active_target,
            state: RecoveryState::Running,
        })
    }

    /// Generation of the currently running stream.
    pub const fn active_generation(&self) -> StreamGeneration {
        self.active_generation
    }

    /// Stable ID of the currently running device.
    pub fn active_device(&self) -> &DeviceId {
        &self.active_device
    }

    /// Applies one callback notification.
    pub fn handle_notification(
        &mut self,
        notification: &StreamNotification,
    ) -> DeviceRecoveryAction {
        if notification.generation != self.active_generation {
            return DeviceRecoveryAction::StaleNotification {
                notification: notification.generation,
                active: self.active_generation,
            };
        }
        if self.state != RecoveryState::Running {
            return DeviceRecoveryAction::None;
        }
        match &notification.error {
            StreamError::BufferUnderrun => DeviceRecoveryAction::Continue,
            StreamError::StreamInvalidated => self.open(self.active_target.clone(), 1),
            StreamError::DeviceNotAvailable => {
                let target = self.unavailable_target();
                self.open(target, 1)
            }
            StreamError::BackendSpecific { .. } => {
                let target = self.selection_target();
                self.open(target, 1)
            }
        }
    }

    /// Applies a polled default/pinned-device observation.
    pub fn observe(&mut self, observation: &DeviceObservation) -> DeviceRecoveryAction {
        if self.state != RecoveryState::Running {
            return DeviceRecoveryAction::None;
        }
        let target = match &self.selection {
            OutputDeviceSelection::FollowDefault { backend } => {
                (observation.default_output.as_ref() != Some(&self.active_device))
                    .then_some(RecoveryTarget::Default { backend: *backend })
            }
            OutputDeviceSelection::Pinned { device_id } if observation.pinned_available => {
                (&self.active_device != device_id).then(|| RecoveryTarget::Device {
                    device_id: device_id.clone(),
                })
            }
            OutputDeviceSelection::Pinned { device_id } => {
                if self.policy.fallback_to_default {
                    let default_changed =
                        observation.default_output.as_ref() != Some(&self.active_device);
                    (self.active_target
                        != RecoveryTarget::Default {
                            backend: device_id.0,
                        }
                        || default_changed)
                        .then_some(RecoveryTarget::Default {
                            backend: device_id.0,
                        })
                } else {
                    Some(RecoveryTarget::Device {
                        device_id: device_id.clone(),
                    })
                }
            }
        };
        target.map_or(DeviceRecoveryAction::None, |target| self.open(target, 1))
    }

    /// Reports that an `Open` action successfully started a stream.
    pub fn stream_started(&mut self, generation: StreamGeneration, device_id: DeviceId) -> bool {
        let RecoveryState::Opening {
            generation: expected,
            target,
            ..
        } = &self.state
        else {
            return false;
        };
        if generation != *expected {
            return false;
        }
        self.active_generation = generation;
        self.active_device = device_id;
        self.active_target = target.clone();
        self.state = RecoveryState::Running;
        true
    }

    /// Reports that an `Open` action failed on the control plane.
    pub fn open_failed(
        &mut self,
        generation: StreamGeneration,
        monotonic_millis: u64,
    ) -> DeviceRecoveryAction {
        let RecoveryState::Opening {
            generation: expected,
            target,
            attempt,
        } = &self.state
        else {
            return DeviceRecoveryAction::None;
        };
        if generation != *expected {
            return DeviceRecoveryAction::None;
        }
        let target = target.clone();
        let attempt = *attempt;
        if attempt < self.policy.maximum_attempts {
            let not_before = monotonic_millis.saturating_add(self.policy.delay(attempt));
            self.state = RecoveryState::Waiting {
                target,
                attempt: attempt + 1,
                not_before,
            };
            return DeviceRecoveryAction::WaitUntil {
                monotonic_millis: not_before,
            };
        }
        if let Some(fallback) = self.fallback_target(&target) {
            return self.open(fallback, 1);
        }
        self.state = RecoveryState::Exhausted {
            target: target.clone(),
            attempts: attempt,
        };
        DeviceRecoveryAction::Exhausted {
            target,
            attempts: attempt,
        }
    }

    /// Advances a scheduled retry when its monotonic deadline has arrived.
    pub fn poll(&mut self, monotonic_millis: u64) -> DeviceRecoveryAction {
        match &self.state {
            RecoveryState::Waiting {
                target,
                attempt,
                not_before,
            } if monotonic_millis >= *not_before => self.open(target.clone(), *attempt),
            RecoveryState::Waiting { not_before, .. } => DeviceRecoveryAction::WaitUntil {
                monotonic_millis: *not_before,
            },
            RecoveryState::Exhausted { target, attempts } => DeviceRecoveryAction::Exhausted {
                target: target.clone(),
                attempts: *attempts,
            },
            RecoveryState::Running | RecoveryState::Opening { .. } => DeviceRecoveryAction::None,
        }
    }

    fn open(&mut self, target: RecoveryTarget, attempt: u32) -> DeviceRecoveryAction {
        let generation = StreamGeneration::new(self.next_generation);
        self.next_generation = generation.value().saturating_add(1);
        self.state = RecoveryState::Opening {
            generation,
            target: target.clone(),
            attempt,
        };
        DeviceRecoveryAction::Open {
            generation,
            target,
            attempt,
        }
    }

    fn selection_target(&self) -> RecoveryTarget {
        match &self.selection {
            OutputDeviceSelection::FollowDefault { backend } => {
                RecoveryTarget::Default { backend: *backend }
            }
            OutputDeviceSelection::Pinned { device_id } => RecoveryTarget::Device {
                device_id: device_id.clone(),
            },
        }
    }

    fn unavailable_target(&self) -> RecoveryTarget {
        match &self.selection {
            OutputDeviceSelection::Pinned { device_id } if self.policy.fallback_to_default => {
                RecoveryTarget::Default {
                    backend: device_id.0,
                }
            }
            _ => self.selection_target(),
        }
    }

    fn fallback_target(&self, failed: &RecoveryTarget) -> Option<RecoveryTarget> {
        let OutputDeviceSelection::Pinned { device_id } = &self.selection else {
            return None;
        };
        (self.policy.fallback_to_default
            && *failed
                == (RecoveryTarget::Device {
                    device_id: device_id.clone(),
                }))
        .then_some(RecoveryTarget::Default {
            backend: device_id.0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(backend: HostId, name: &str) -> DeviceId {
        DeviceId(backend, name.to_owned())
    }

    fn backend() -> HostId {
        cpal::ALL_HOSTS[0]
    }

    fn open(action: DeviceRecoveryAction) -> (StreamGeneration, RecoveryTarget, u32) {
        let DeviceRecoveryAction::Open {
            generation,
            target,
            attempt,
        } = action
        else {
            panic!("expected open action")
        };
        (generation, target, attempt)
    }

    fn controller(selection: OutputDeviceSelection) -> DeviceRecoveryController {
        let backend = selection.backend();
        let active = match &selection {
            OutputDeviceSelection::Pinned { device_id } => device_id.clone(),
            OutputDeviceSelection::FollowDefault { .. } => device(backend, "default-a"),
        };
        DeviceRecoveryController::new(
            selection,
            DeviceRecoveryPolicy::default(),
            StreamGeneration::new(7),
            active,
        )
        .unwrap()
    }

    #[test]
    fn notification_channel_is_bounded_and_non_blocking() {
        let (sender, receiver) = stream_notification_channel(1).unwrap();
        sender
            .try_send(StreamGeneration::new(1), StreamError::BufferUnderrun)
            .unwrap();
        assert!(matches!(
            sender.try_send(StreamGeneration::new(1), StreamError::BufferUnderrun),
            Err(StreamNotificationSendError::Full(StreamNotification {
                error: StreamError::BufferUnderrun,
                ..
            }))
        ));
        assert_eq!(
            receiver.try_recv().unwrap(),
            StreamNotification {
                generation: StreamGeneration::new(1),
                error: StreamError::BufferUnderrun,
            }
        );
        assert_eq!(
            receiver.try_recv().unwrap_err(),
            StreamNotificationReceiveError::Empty
        );
    }

    #[test]
    fn fatal_notification_is_coalesced_once_when_queue_is_saturated() {
        let (sender, receiver) = stream_notification_channel(1).unwrap();
        let generation = StreamGeneration::new(4);
        sender
            .try_send(generation, StreamError::BufferUnderrun)
            .unwrap();
        sender
            .try_send(generation, StreamError::DeviceNotAvailable)
            .unwrap();
        sender
            .try_send(generation, StreamError::StreamInvalidated)
            .unwrap();

        assert_eq!(
            receiver.try_recv().unwrap(),
            StreamNotification {
                generation,
                error: StreamError::DeviceNotAvailable,
            }
        );
        assert_eq!(
            receiver.try_recv().unwrap().error,
            StreamError::BufferUnderrun
        );
        assert_eq!(
            receiver.try_recv().unwrap_err(),
            StreamNotificationReceiveError::Empty
        );

        sender
            .try_send(generation, StreamError::BufferUnderrun)
            .unwrap();
        sender
            .try_send(generation, StreamError::DeviceNotAvailable)
            .unwrap();
        assert_eq!(
            receiver.try_recv().unwrap().error,
            StreamError::BufferUnderrun
        );
        assert_eq!(
            receiver.try_recv().unwrap_err(),
            StreamNotificationReceiveError::Empty
        );
    }

    #[test]
    fn stale_notifications_are_suppressed_and_underruns_continue() {
        let backend = backend();
        let mut controller = controller(OutputDeviceSelection::FollowDefault { backend });
        assert!(matches!(
            controller.handle_notification(&StreamNotification {
                generation: StreamGeneration::new(6),
                error: StreamError::DeviceNotAvailable,
            }),
            DeviceRecoveryAction::StaleNotification { .. }
        ));
        assert_eq!(
            controller.handle_notification(&StreamNotification {
                generation: StreamGeneration::new(7),
                error: StreamError::BufferUnderrun,
            }),
            DeviceRecoveryAction::Continue
        );
    }

    #[test]
    fn invalidation_rebuilds_the_same_target_with_a_new_generation() {
        let backend = backend();
        let mut controller = controller(OutputDeviceSelection::FollowDefault { backend });
        let (generation, target, attempt) =
            open(controller.handle_notification(&StreamNotification {
                generation: StreamGeneration::new(7),
                error: StreamError::StreamInvalidated,
            }));
        assert_eq!(generation, StreamGeneration::new(8));
        assert_eq!(target, RecoveryTarget::Default { backend });
        assert_eq!(attempt, 1);
    }

    #[test]
    fn following_default_reopens_when_the_observed_id_changes() {
        let backend = backend();
        let mut controller = controller(OutputDeviceSelection::FollowDefault { backend });
        assert_eq!(
            controller.observe(&DeviceObservation {
                default_output: Some(device(backend, "default-a")),
                pinned_available: false,
            }),
            DeviceRecoveryAction::None
        );
        let (_, target, _) = open(controller.observe(&DeviceObservation {
            default_output: Some(device(backend, "default-b")),
            pinned_available: false,
        }));
        assert_eq!(target, RecoveryTarget::Default { backend });
    }

    #[test]
    fn pinned_device_ignores_default_changes_until_it_disappears() {
        let backend = backend();
        let pinned = device(backend, "pinned");
        let mut controller = controller(OutputDeviceSelection::Pinned {
            device_id: pinned.clone(),
        });
        assert_eq!(
            controller.observe(&DeviceObservation {
                default_output: Some(device(backend, "default-b")),
                pinned_available: true,
            }),
            DeviceRecoveryAction::None
        );
        let (_, target, _) = open(controller.observe(&DeviceObservation {
            default_output: Some(device(backend, "default-b")),
            pinned_available: false,
        }));
        assert_eq!(target, RecoveryTarget::Default { backend });
    }

    #[test]
    fn retries_back_off_deterministically_then_exhaust() {
        let backend = backend();
        let policy = DeviceRecoveryPolicy {
            maximum_attempts: 3,
            initial_retry_delay_millis: 10,
            maximum_retry_delay_millis: 15,
            fallback_to_default: false,
        };
        let mut controller = DeviceRecoveryController::new(
            OutputDeviceSelection::FollowDefault { backend },
            policy,
            StreamGeneration::new(1),
            device(backend, "default"),
        )
        .unwrap();
        let (first, _, _) = open(controller.handle_notification(&StreamNotification {
            generation: StreamGeneration::new(1),
            error: StreamError::DeviceNotAvailable,
        }));
        assert_eq!(
            controller.open_failed(first, 100),
            DeviceRecoveryAction::WaitUntil {
                monotonic_millis: 110
            }
        );
        assert_eq!(
            controller.poll(109),
            DeviceRecoveryAction::WaitUntil {
                monotonic_millis: 110
            }
        );
        let (second, _, attempt) = open(controller.poll(110));
        assert_eq!(attempt, 2);
        assert_eq!(
            controller.open_failed(second, 110),
            DeviceRecoveryAction::WaitUntil {
                monotonic_millis: 125
            }
        );
        let (third, target, attempt) = open(controller.poll(125));
        assert_eq!(attempt, 3);
        assert_eq!(
            controller.open_failed(third, 125),
            DeviceRecoveryAction::Exhausted {
                target,
                attempts: 3
            }
        );
    }

    #[test]
    fn pinned_retry_cap_falls_back_to_default() {
        let backend = backend();
        let pinned = device(backend, "pinned");
        let policy = DeviceRecoveryPolicy {
            maximum_attempts: 1,
            ..DeviceRecoveryPolicy::default()
        };
        let mut controller = DeviceRecoveryController::new(
            OutputDeviceSelection::Pinned {
                device_id: pinned.clone(),
            },
            policy,
            StreamGeneration::new(3),
            pinned.clone(),
        )
        .unwrap();
        let (generation, target, _) = open(controller.handle_notification(&StreamNotification {
            generation: StreamGeneration::new(3),
            error: StreamError::BackendSpecific {
                err: cpal::BackendSpecificError {
                    description: "failed".into(),
                },
            },
        }));
        assert_eq!(target, RecoveryTarget::Device { device_id: pinned });
        let (_, fallback, _) = open(controller.open_failed(generation, 0));
        assert_eq!(fallback, RecoveryTarget::Default { backend });
    }

    #[test]
    fn successful_fallback_returns_to_pinned_when_it_reappears() {
        let backend = backend();
        let pinned = device(backend, "pinned");
        let fallback = device(backend, "default");
        let mut controller = controller(OutputDeviceSelection::Pinned {
            device_id: pinned.clone(),
        });
        let (generation, _, _) = open(controller.handle_notification(&StreamNotification {
            generation: StreamGeneration::new(7),
            error: StreamError::DeviceNotAvailable,
        }));
        assert!(controller.stream_started(generation, fallback));
        let (_, target, _) = open(controller.observe(&DeviceObservation {
            default_output: Some(device(backend, "default")),
            pinned_available: true,
        }));
        assert_eq!(target, RecoveryTarget::Device { device_id: pinned });
    }
}
