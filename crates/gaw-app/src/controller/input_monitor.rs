use std::{thread, time::Duration};

use crossbeam_channel::{Receiver, Sender, unbounded};
use gaw_audio::{CpalInputMonitor, InputMonitorControl};

#[derive(Clone, Debug)]
pub(crate) struct InputMonitorStatus {
    pub(crate) enabled: bool,
    pub(crate) opening: bool,
    pub(crate) device_name: Option<String>,
    pub(crate) channels: Option<u16>,
    pub(crate) peak: f32,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct OutputFormat {
    pub(super) sample_rate: u32,
    pub(super) buffer_frames: Option<u32>,
}

#[derive(Debug)]
struct OpenRequest {
    generation: u64,
    device: Option<cpal::DeviceId>,
    channel: usize,
    output: Option<OutputFormat>,
    effects: EffectsConfig,
}

#[derive(Debug)]
struct ReadyInput {
    device_name: String,
    channels: u16,
    effects_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct EffectsConfig {
    processors: Vec<gaw_core::Processor>,
    bypassed: bool,
    tempo_bpm: f64,
}

impl Default for EffectsConfig {
    fn default() -> Self {
        Self {
            processors: Vec::new(),
            bypassed: false,
            tempo_bpm: 120.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CaptureConfig {
    device: Option<cpal::DeviceId>,
    channel: usize,
    output: OutputFormat,
}

#[derive(Debug)]
struct OpenResult {
    generation: u64,
    result: Result<ReadyInput, String>,
}

#[derive(Debug)]
pub(super) struct InputMonitoring {
    pub(super) control: InputMonitorControl,
    requests: Sender<OpenRequest>,
    results: Receiver<OpenResult>,
    generation: u64,
    device: Option<cpal::DeviceId>,
    channel: usize,
    enabled: bool,
    output: Option<OutputFormat>,
    ready: Option<ReadyInput>,
    peak: f32,
    error: Option<String>,
    effects: EffectsConfig,
    effects_pending: bool,
    effects_error: Option<String>,
}

impl InputMonitoring {
    pub(super) fn new() -> Self {
        let control = InputMonitorControl::new();
        let (requests, receiver) = unbounded();
        let (sender, results) = unbounded();
        let worker_control = control.clone();
        thread::Builder::new()
            .name("gaw-input-monitor".into())
            .spawn(move || run_worker(&receiver, &sender, &worker_control))
            .expect("input monitor worker should start");
        Self {
            control,
            requests,
            results,
            generation: 0,
            device: None,
            channel: 0,
            enabled: false,
            output: None,
            ready: None,
            peak: 0.0,
            error: None,
            effects: EffectsConfig::default(),
            effects_pending: false,
            effects_error: None,
        }
    }

    pub(super) fn configure(
        &mut self,
        device: Option<cpal::DeviceId>,
        channel: usize,
        gain: f32,
        enabled: bool,
    ) {
        self.control.set_gain(gain);
        if self.device == device && self.channel == channel && self.enabled == enabled {
            return;
        }
        self.device = device;
        self.channel = channel;
        self.enabled = enabled;
        self.error = None;
        self.suspend();
    }

    pub(super) fn configure_effects(
        &mut self,
        processors: &[gaw_core::Processor],
        bypassed: bool,
        tempo_bpm: f64,
    ) {
        if self.effects.processors == processors
            && self.effects.bypassed == bypassed
            && (self.effects.tempo_bpm - tempo_bpm).abs() < f64::EPSILON
        {
            return;
        }
        self.effects = EffectsConfig {
            processors: processors.to_vec(),
            bypassed,
            tempo_bpm,
        };
        self.effects_error = None;
        if self.output.is_some() {
            self.request(self.output);
        }
    }

    pub(super) fn effects_status(&self) -> (bool, Option<String>) {
        (self.effects_pending, self.effects_error.clone())
    }

    /// Stop audibility immediately, including when a previous open is still pending.
    /// Keep the user's session intent so an output recovery can reopen capture.
    pub(super) fn suspend(&mut self) {
        self.control.set_enabled(false);
        self.peak = 0.0;
        self.ready = None;
        self.output = None;
        self.request(None);
    }

    fn request(&mut self, output: Option<OutputFormat>) {
        self.generation = self.generation.wrapping_add(1);
        self.effects_pending = output.is_some();
        if self
            .requests
            .send(OpenRequest {
                generation: self.generation,
                device: self.device.clone(),
                channel: self.channel,
                output,
                effects: self.effects.clone(),
            })
            .is_err()
        {
            self.enabled = false;
            self.effects_pending = false;
            self.error = Some("Input monitor worker stopped; reopen the project to retry".into());
        }
    }

    pub(super) fn pump(&mut self, output: Option<OutputFormat>) {
        let output = output.filter(|_| self.enabled);
        if self.output != output {
            self.control.set_enabled(false);
            self.ready = None;
            self.output = output;
            self.request(output);
        }
        while let Ok(completed) = self.results.try_recv() {
            if completed.generation != self.generation || !self.enabled {
                continue;
            }
            match completed.result {
                Ok(mut info) => {
                    self.effects_pending = false;
                    self.effects_error = info.effects_error.take();
                    self.ready = Some(info);
                    self.control.set_enabled(true);
                }
                Err(error) => {
                    self.enabled = false;
                    self.suspend();
                    self.error = Some(format!("{error}. Toggle monitoring on to retry."));
                }
            }
        }
        self.peak = if self.control.enabled() {
            self.control.peak()
        } else {
            0.0
        };
    }

    pub(super) fn status(&self) -> InputMonitorStatus {
        InputMonitorStatus {
            enabled: self.enabled,
            opening: self.enabled && self.ready.is_none(),
            device_name: self.ready.as_ref().map(|info| info.device_name.clone()),
            channels: self.ready.as_ref().map(|info| info.channels),
            peak: self.peak,
            error: self.error.clone(),
        }
    }
}

impl Drop for InputMonitoring {
    fn drop(&mut self) {
        self.control.set_enabled(false);
    }
}

struct ActiveInput {
    generation: u64,
    config: CaptureConfig,
    stream: CpalInputMonitor,
}

fn apply_open_request(
    active: &mut Option<ActiveInput>,
    request: &OpenRequest,
    control: &InputMonitorControl,
) -> Result<ReadyInput, String> {
    let output = request.output.expect("capture requests require an output");
    let config = CaptureConfig {
        device: request.device.clone(),
        channel: request.channel,
        output,
    };
    if active.as_ref().is_none_or(|active| active.config != config) {
        *active = None;
        let stream = CpalInputMonitor::open(
            request.device.as_ref(),
            output.sample_rate,
            output.buffer_frames,
            request.channel,
            control.clone(),
        )
        .map_err(|error| error.to_string())?;
        *active = Some(ActiveInput {
            generation: request.generation,
            config,
            stream,
        });
    }
    let effects_error = control
        .configure_effects(
            &request.effects.processors,
            request.effects.bypassed,
            output.sample_rate,
            request.effects.tempo_bpm,
        )
        .err();
    let active = active.as_mut().expect("capture is open");
    active.generation = request.generation;
    let info = active.stream.info();
    Ok(ReadyInput {
        device_name: info.device_name.clone(),
        channels: info.channels,
        effects_error,
    })
}

fn run_worker(
    requests: &Receiver<OpenRequest>,
    results: &Sender<OpenResult>,
    control: &InputMonitorControl,
) {
    let mut active: Option<ActiveInput> = None;
    loop {
        control.collect_retired_effects();
        match requests.recv_timeout(Duration::from_millis(30)) {
            Ok(mut request) => {
                // Device enumeration/open can take time; skip superseded requests.
                while let Ok(newer) = requests.try_recv() {
                    request = newer;
                }
                if request.output.is_none() {
                    active = None;
                    continue;
                }
                let result = apply_open_request(&mut active, &request, control);
                if results
                    .send(OpenResult {
                        generation: request.generation,
                        result,
                    })
                    .is_err()
                {
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
        if let Some(input) = &mut active
            && let Some(error) = input.stream.take_error()
        {
            if results
                .send(OpenResult {
                    generation: input.generation,
                    result: Err(error),
                })
                .is_err()
            {
                break;
            }
            active = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor() -> (InputMonitoring, Receiver<OpenRequest>, Sender<OpenResult>) {
        let (requests, receiver) = unbounded();
        let (sender, results) = unbounded();
        (
            InputMonitoring {
                control: InputMonitorControl::new(),
                requests,
                results,
                generation: 0,
                device: None,
                channel: 0,
                enabled: false,
                output: None,
                ready: None,
                peak: 0.0,
                error: None,
                effects: EffectsConfig::default(),
                effects_pending: false,
                effects_error: None,
            },
            receiver,
            sender,
        )
    }

    fn output(sample_rate: u32) -> OutputFormat {
        OutputFormat {
            sample_rate,
            buffer_frames: Some(128),
        }
    }

    fn opened(generation: u64) -> OpenResult {
        OpenResult {
            generation,
            result: Ok(ReadyInput {
                device_name: "Guitar".into(),
                channels: 2,
                effects_error: None,
            }),
        }
    }

    #[test]
    fn monitoring_waits_for_output_and_uses_negotiated_format() {
        let (mut monitor, requests, results) = monitor();
        assert!(!monitor.status().enabled);
        monitor.configure(None, 1, 1.0, true);
        assert!(requests.recv().unwrap().output.is_none());
        monitor.pump(None);
        assert!(monitor.status().opening);
        assert!(!monitor.control.enabled());
        assert!(requests.is_empty());
        monitor.pump(Some(output(48_000)));
        let request = requests.recv().unwrap();
        assert_eq!(request.output, Some(output(48_000)));
        assert_eq!(request.channel, 1);
        results.send(opened(request.generation)).unwrap();
        monitor.pump(Some(output(48_000)));
        assert!(monitor.control.enabled());
        assert!(!monitor.status().opening);
        assert_eq!(monitor.status().device_name.as_deref(), Some("Guitar"));
    }

    #[test]
    fn disabling_rejects_an_open_that_finishes_late() {
        let (mut monitor, requests, results) = monitor();
        monitor.configure(None, 0, 1.0, true);
        monitor.pump(Some(output(48_000)));
        let pending = requests.try_iter().last().unwrap();
        monitor.configure(None, 0, 1.0, false);
        assert!(!monitor.control.enabled());
        results.send(opened(pending.generation)).unwrap();
        monitor.pump(Some(output(48_000)));
        assert!(!monitor.status().enabled);
        assert!(!monitor.status().opening);
        assert!(!monitor.control.enabled());
        assert!(monitor.status().device_name.is_none());
    }

    #[test]
    fn changing_input_while_opening_rejects_old_device_and_keeps_exact_selection() {
        let (mut monitor, requests, results) = monitor();
        let device = cpal::DeviceId(cpal::ALL_HOSTS[0], "guitar-interface".into());
        monitor.configure(None, 0, 1.0, true);
        monitor.pump(Some(output(48_000)));
        let original = requests.try_iter().last().unwrap();
        monitor.configure(Some(device.clone()), 1, 1.0, true);
        monitor.pump(Some(output(48_000)));
        let replacement = requests.try_iter().last().unwrap();
        assert_eq!(replacement.device, Some(device));
        assert_eq!(replacement.channel, 1);
        results.send(opened(original.generation)).unwrap();
        monitor.pump(Some(output(48_000)));
        assert!(!monitor.control.enabled());
        assert!(monitor.status().opening);
        results.send(opened(replacement.generation)).unwrap();
        monitor.pump(Some(output(48_000)));
        assert!(monitor.control.enabled());
    }

    #[test]
    fn adjusting_gain_does_not_reopen_an_active_input() {
        let (mut monitor, requests, results) = monitor();
        monitor.configure(None, 0, 1.0, true);
        monitor.pump(Some(output(48_000)));
        let original = requests.try_iter().last().unwrap();
        results.send(opened(original.generation)).unwrap();
        monitor.pump(Some(output(48_000)));
        monitor.configure(None, 0, 0.5, true);
        monitor.pump(Some(output(48_000)));
        assert!(requests.is_empty());
        assert_eq!(monitor.generation, original.generation);
        assert!(monitor.control.enabled());
        assert!(!monitor.status().opening);
    }

    #[test]
    fn effect_edits_keep_monitoring_active_and_ignore_superseded_results() {
        let (mut monitor, requests, results) = monitor();
        monitor.configure(None, 1, 1.0, true);
        monitor.pump(Some(output(48_000)));
        let original = requests.try_iter().last().unwrap();
        results.send(opened(original.generation)).unwrap();
        monitor.pump(Some(output(48_000)));
        let effects = vec![gaw_core::Processor::new(
            gaw_core::ProcessorId::new("live_gain").unwrap(),
            gaw_core::ProcessorKind::Gain(gaw_core::GainParameters::default()),
        )];
        monitor.configure_effects(&effects, false, 120.0);
        let first = requests.recv().unwrap();
        assert!(monitor.control.enabled());
        assert!(!monitor.status().opening);
        assert!(monitor.effects_status().0);
        assert_eq!(first.effects.processors, effects);
        assert_eq!(first.channel, original.channel);
        assert_eq!(first.output, original.output);
        monitor.configure_effects(&effects, true, 90.0);
        let latest = requests.recv().unwrap();
        results.send(opened(first.generation)).unwrap();
        monitor.pump(Some(output(48_000)));
        assert!(monitor.effects_status().0);
        let mut completed = opened(latest.generation);
        completed.result.as_mut().unwrap().effects_error = Some("Could not prepare effect".into());
        results.send(completed).unwrap();
        monitor.pump(Some(output(48_000)));
        assert!(!monitor.effects_status().0);
        assert!(monitor.effects_status().1.is_some());
        assert!(monitor.control.enabled());
        assert!(monitor.status().error.is_none());
        monitor.configure_effects(&effects, true, 90.0);
        assert!(
            requests.is_empty(),
            "unchanged effects must not be prepared again"
        );
    }

    #[test]
    fn effects_wait_for_output_and_survive_monitor_and_sample_rate_changes() {
        let (mut monitor, requests, results) = monitor();
        monitor.configure_effects(&[], true, 100.0);
        assert!(requests.is_empty());
        monitor.configure(None, 0, 1.0, true);
        monitor.pump(Some(output(44_100)));
        let original = requests.try_iter().last().unwrap();
        assert!(original.effects.bypassed);
        results.send(opened(original.generation)).unwrap();
        monitor.pump(Some(output(44_100)));
        monitor.suspend();
        assert!(!monitor.effects_status().0);
        monitor.pump(Some(output(48_000)));
        let recovered = requests.try_iter().last().unwrap();
        assert_eq!(recovered.effects, original.effects);
        assert_eq!(recovered.output, Some(output(48_000)));
    }

    #[test]
    fn output_recovery_mutes_and_reopens_at_new_rate() {
        let (mut monitor, requests, results) = monitor();
        monitor.configure(None, 0, 1.0, true);
        monitor.pump(Some(output(44_100)));
        let original = requests.try_iter().last().unwrap();
        results.send(opened(original.generation)).unwrap();
        monitor.pump(Some(output(44_100)));
        assert!(monitor.control.enabled());
        monitor.suspend();
        assert!(!monitor.control.enabled());
        assert!(monitor.status().enabled);
        monitor.pump(Some(output(48_000)));
        let replacement = requests.try_iter().last().unwrap();
        assert_eq!(replacement.output, Some(output(48_000)));
        results.send(opened(original.generation)).unwrap();
        monitor.pump(Some(output(48_000)));
        assert!(!monitor.control.enabled());
        results.send(opened(replacement.generation)).unwrap();
        monitor.pump(Some(output(48_000)));
        assert!(monitor.control.enabled());
    }

    #[test]
    fn failed_input_requires_explicit_retry_and_stale_errors_are_ignored() {
        let (mut monitor, requests, results) = monitor();
        monitor.configure(None, 0, 1.0, true);
        monitor.pump(Some(output(48_000)));
        let pending = requests.try_iter().last().unwrap();
        results
            .send(OpenResult {
                generation: pending.generation,
                result: Err("Input disconnected".into()),
            })
            .unwrap();
        monitor.pump(Some(output(48_000)));
        assert!(!monitor.status().enabled);
        assert!(!monitor.control.enabled());
        assert!(
            monitor
                .status()
                .error
                .unwrap()
                .contains("Toggle monitoring on to retry")
        );
        monitor.configure(None, 0, 1.0, true);
        monitor.pump(Some(output(48_000)));
        let retry = requests.try_iter().last().unwrap();
        results
            .send(OpenResult {
                generation: pending.generation,
                result: Err("Old failure".into()),
            })
            .unwrap();
        results.send(opened(retry.generation)).unwrap();
        monitor.pump(Some(output(48_000)));
        assert!(monitor.status().enabled);
        assert!(monitor.status().error.is_none());
        assert!(monitor.control.enabled());
    }
}
