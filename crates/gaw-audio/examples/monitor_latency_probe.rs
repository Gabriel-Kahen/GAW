//! Bounded, silent CPAL buffer probe. Capture samples are discarded, never stored.
//! Usage: `cargo run -p gaw-audio --example monitor_latency_probe -- 128 10`
//! Uses the default input and output devices. Verify routes before interpreting results.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::{
    error::Error,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::Duration,
};

#[derive(Debug)]
struct Range {
    min: AtomicU64,
    max: AtomicU64,
    total: AtomicU64,
    count: AtomicU64,
}

impl Default for Range {
    fn default() -> Self {
        Self {
            min: AtomicU64::new(u64::MAX),
            max: AtomicU64::new(0),
            total: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }
}

impl Range {
    fn add(&self, value: u64) {
        self.min.fetch_min(value, Relaxed);
        self.max.fetch_max(value, Relaxed);
        self.total.fetch_add(value, Relaxed);
        self.count.fetch_add(1, Relaxed);
    }

    fn report(&self, label: &str) {
        let count = self.count.load(Relaxed);
        if let Some(mean) = self.total.load(Relaxed).checked_div(count) {
            println!(
                "{label}: n={count} min={} mean={} max={}",
                self.min.load(Relaxed),
                mean,
                self.max.load(Relaxed),
            );
        }
    }
}

#[derive(Debug, Default)]
struct Stats {
    frames: Range,
    delay_us: Range,
    gap_us: Range,
    underruns: AtomicU64,
    other_errors: AtomicU64,
}

impl Stats {
    fn callback(
        &self,
        frames: usize,
        callback: cpal::StreamInstant,
        delay: Option<Duration>,
        previous: &mut Option<cpal::StreamInstant>,
    ) {
        self.frames.add(u64::try_from(frames).unwrap_or(u64::MAX));
        if let Some(delay) = delay {
            self.delay_us.add(micros(delay));
        }
        if let Some(gap) = previous.and_then(|last| callback.duration_since(&last)) {
            self.gap_us.add(micros(gap));
        }
        *previous = Some(callback);
    }

    fn error(&self, error: &cpal::StreamError) {
        match error {
            cpal::StreamError::BufferUnderrun => &self.underruns,
            _ => &self.other_errors,
        }
        .fetch_add(1, Relaxed);
    }

    fn report(&self, direction: &str) {
        println!("{direction}");
        self.frames.report("  callback frames");
        self.delay_us
            .report("  driver timestamp delay us (estimate)");
        self.gap_us.report("  callback gap us");
        println!(
            "  underruns={} other_errors={}",
            self.underruns.load(Relaxed),
            self.other_errors.load(Relaxed),
        );
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let frames: u32 = args.next().unwrap_or_else(|| "128".into()).parse()?;
    let seconds: u64 = args.next().unwrap_or_else(|| "10".into()).parse()?;
    if frames == 0 || frames > 8192 || seconds == 0 || seconds > 60 || args.next().is_some() {
        return Err("usage: monitor_latency_probe [frames 1..8192] [seconds 1..60]".into());
    }
    let host = cpal::default_host();
    let output = host.default_output_device().ok_or("no default output")?;
    let input = host.default_input_device().ok_or("no default input")?;
    let config = cpal::StreamConfig {
        channels: 2,
        sample_rate: 48_000,
        buffer_size: cpal::BufferSize::Fixed(frames),
    };
    let input_config = cpal::StreamConfig {
        channels: 1,
        ..config
    };
    let out_stats = Arc::new(Stats::default());
    let in_stats = Arc::new(Stats::default());
    let stats = Arc::clone(&out_stats);
    let errors = Arc::clone(&out_stats);
    let mut previous = None;
    let mut priority_attempted = false;
    let output_stream = output.build_output_stream(
        &config,
        move |data: &mut [f32], info| {
            if !priority_attempted {
                gaw_audio::realtime_priority::promote_audio_callback_thread();
                priority_attempted = true;
            }
            data.fill(0.0);
            let timestamp = info.timestamp();
            stats.callback(
                data.len() / 2,
                timestamp.callback,
                timestamp.playback.duration_since(&timestamp.callback),
                &mut previous,
            );
        },
        move |error| errors.error(&error),
        None,
    )?;
    let stats = Arc::clone(&in_stats);
    let errors = Arc::clone(&in_stats);
    let mut previous = None;
    let mut priority_attempted = false;
    let input_stream = input.build_input_stream(
        &input_config,
        move |data: &[f32], info| {
            if !priority_attempted {
                gaw_audio::realtime_priority::promote_audio_callback_thread();
                priority_attempted = true;
            }
            let timestamp = info.timestamp();
            stats.callback(
                data.len(),
                timestamp.callback,
                timestamp.callback.duration_since(&timestamp.capture),
                &mut previous,
            );
        },
        move |error| errors.error(&error),
        None,
    )?;
    output_stream.play()?;
    input_stream.play()?;
    println!(
        "Probe pid={} requested={frames}/48000 duration={seconds}s",
        std::process::id()
    );
    println!("Output is silence. Capture is discarded. No analog latency is measured.");
    std::thread::sleep(Duration::from_secs(seconds));
    drop(input_stream);
    drop(output_stream);
    in_stats.report("CAPTURE");
    out_stats.report("PLAYBACK");
    Ok(())
}
