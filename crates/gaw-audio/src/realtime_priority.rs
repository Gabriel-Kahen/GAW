//! Callback-thread scheduling fallback for Linux sessions without `RTKit`.

/// Best-effort promotion of the calling audio callback thread.
///
/// Call once at stream startup, on the callback thread itself. Existing realtime
/// policies are preserved. Linux uses the lowest FIFO priority so audio-server
/// threads retain precedence; denied requests silently keep normal scheduling.
/// No process-wide limits, scheduler settings, allocations, or logging are used.
/// Returns whether this helper established or observed realtime scheduling.
pub fn promote_audio_callback_thread() -> bool {
    #[cfg(target_os = "linux")]
    {
        use thread_priority::{
            RealtimeThreadSchedulePolicy, ThreadPriority, ThreadSchedulePolicy,
            set_thread_priority_and_policy, thread_native_id, thread_schedule_policy,
        };

        match thread_schedule_policy() {
            Ok(ThreadSchedulePolicy::Realtime(_)) => true,
            Ok(ThreadSchedulePolicy::Normal(_)) => set_thread_priority_and_policy(
                thread_native_id(),
                ThreadPriority::Min,
                ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo),
            )
            .is_ok(),
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    false
}
