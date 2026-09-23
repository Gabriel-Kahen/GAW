//! Bounded live GAW monitor/FX queue probe with muted output and no recording.

use gaw_audio::{
    CpalInputMonitor, CpalOutput, InputMonitorControl, RealtimeEngine, RealtimeEngineConfig,
};
use gaw_core::processors::{
    Oversampling, Processor, ProcessorId, ProcessorKind, SaturatorParameters,
};
use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant},
};

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let frames: u32 = args.next().unwrap_or_else(|| "64".into()).parse()?;
    let seconds: u64 = args.next().unwrap_or_else(|| "15".into()).parse()?;
    if frames == 0 || frames > 8192 || seconds == 0 || seconds > 60 || args.next().is_some() {
        return Err("usage: monitor_pipeline_probe [frames 1..8192] [seconds 1..60]".into());
    }
    let control = InputMonitorControl::new();
    control.set_gain(0.0);
    control.set_enabled(true);
    control.configure_effects(
        &[Processor::new(
            ProcessorId::new("probe-saturator")?,
            ProcessorKind::Saturator(SaturatorParameters {
                oversampling: Oversampling::X4,
                ..SaturatorParameters::default()
            }),
        )],
        false,
        48_000,
        120.0,
    )?;
    let (_sender, mut engine) = RealtimeEngine::new(
        RealtimeEngineConfig {
            maximum_block_frames: 8192,
            ..RealtimeEngineConfig::default()
        },
        8,
        8,
    )?;
    engine.set_input_monitor(control.clone());
    let errors = Arc::new(AtomicU64::new(0));
    let output_errors = Arc::clone(&errors);
    let output =
        CpalOutput::open_default_negotiated_with_buffer(engine, Some(frames), move |_| {
            output_errors.fetch_add(1, Relaxed);
        })?;
    let input = CpalInputMonitor::open(
        None,
        output.info().sample_rate,
        Some(frames),
        0,
        control.clone(),
    )?;
    output.play()?;
    println!(
        "Pipeline pid={} requested={frames} rate={} seconds={seconds}; Saturator X4; gain=0; no recording",
        std::process::id(),
        output.info().sample_rate
    );
    std::thread::sleep(Duration::from_secs(1));
    let start = control.latency_status();
    let errors_start = errors.load(Relaxed);
    let mut max_queue = 0;
    let measurement_start = Instant::now();
    let deadline = measurement_start + Duration::from_secs(seconds);
    let mut interval_end = measurement_start + Duration::from_secs(10);
    let mut previous = start;
    let mut previous_errors = errors_start;
    while Instant::now() < deadline {
        max_queue = max_queue.max(control.latency_status().queued_frames);
        control.collect_retired_effects();
        std::thread::sleep(Duration::from_millis(5));
        if Instant::now() >= interval_end {
            let current = control.latency_status();
            let current_errors = errors.load(Relaxed);
            println!(
                "at {}s: queue={} dropped_delta={} underrun_delta={} output_error_delta={}",
                measurement_start.elapsed().as_secs(),
                current.queued_frames,
                current
                    .dropped_frames
                    .saturating_sub(previous.dropped_frames),
                current
                    .underrun_frames
                    .saturating_sub(previous.underrun_frames),
                current_errors.saturating_sub(previous_errors),
            );
            previous = current;
            previous_errors = current_errors;
            interval_end += Duration::from_secs(10);
        }
    }
    let end = control.latency_status();
    println!("after warmup: {start:?}");
    println!("final: {end:?}");
    println!(
        "output callback frames={:?}; max queue={max_queue}; dropped delta={}; underrun delta={}; output error delta={}; input error={:?}",
        output.callback_buffer_frames(),
        end.dropped_frames.saturating_sub(start.dropped_frames),
        end.underrun_frames.saturating_sub(start.underrun_frames),
        errors.load(Relaxed).saturating_sub(errors_start),
        input.take_error()
    );
    control.set_enabled(false);
    drop(input);
    drop(output);
    Ok(())
}
