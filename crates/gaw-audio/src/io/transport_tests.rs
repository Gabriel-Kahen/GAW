#![allow(clippy::cast_precision_loss, clippy::float_cmp)]

use super::*;

#[derive(Debug)]
struct Sides;

impl RealtimeRender for Sides {
    fn render(&self, start: u64, output: &mut SampleBlock<'_>) {
        let channels = output.layout().channels();
        for (index, frame) in output.samples_mut().chunks_exact_mut(channels).enumerate() {
            let sign = if start + index as u64 >= 10_000 {
                -1.0
            } else {
                1.0
            };
            for (channel, sample) in frame.iter_mut().enumerate() {
                *sample = sign * if channel == 0 { 0.8 } else { -0.4 };
            }
        }
    }
}

fn playing_engine(rate: u32, layout: ChannelLayout) -> (CommandSender, RealtimeEngine) {
    let (sender, engine) = RealtimeEngine::new(
        RealtimeEngineConfig {
            sample_rate: rate,
            output_layout: layout,
            ..RealtimeEngineConfig::default()
        },
        8,
        8,
    )
    .unwrap();
    let snapshot =
        Arc::new(RenderSnapshot::new(1, 48_000, layout, 100_000, 0, Arc::new(Sides)).unwrap());
    sender
        .try_send(RealtimeCommand::ActivateTimeline(TimelineActivation {
            generation: 1,
            snapshot: Some(snapshot),
            preserve_transport: false,
            sample_rate: 48_000,
            total_frames: 100_000,
            frame: 1_000,
            playing: true,
            loop_range: None,
            metronome: RealtimeMetronome::default(),
        }))
        .unwrap();
    (sender, engine)
}

fn render_chunks(engine: &mut RealtimeEngine, frames: usize, chunk: usize) -> Vec<f32> {
    let channels = engine.config().output_layout.channels();
    let mut output = vec![0.0; frames * channels];
    for block in output.chunks_mut(chunk * channels) {
        engine.process(block);
    }
    output
}

fn assert_bounded_steps(output: &[f32], channels: usize, max_step: f32) {
    for (previous, next) in output.iter().zip(&output[channels..]) {
        assert!((next - previous).abs() <= max_step + 1.0e-6);
    }
}

#[test]
fn start_pause_and_stop_are_continuous_at_device_rates_and_buffer_sizes() {
    for rate in [16_000_u32, 44_100, 48_000, 96_000] {
        for layout in [ChannelLayout::Mono, ChannelLayout::Stereo] {
            for chunk in [1, 17, 512] {
                for stop in [false, true] {
                    let (sender, mut engine) = playing_engine(rate, layout);
                    let frames = rate.div_ceil(200) as usize;
                    let channels = layout.channels();
                    let start = render_chunks(&mut engine, frames, chunk);
                    for channel in 0..channels {
                        let target = if channel == 0 { 0.8 } else { -0.4 };
                        assert_eq!(start[channel], 0.0);
                        assert_eq!(start[(frames - 1) * channels + channel], target);
                    }
                    assert_bounded_steps(&start, channels, 0.8 / (frames - 1) as f32);
                    let paused_frame = sender.frame_position();
                    sender
                        .try_send(if stop {
                            RealtimeCommand::Stop
                        } else {
                            RealtimeCommand::Pause
                        })
                        .unwrap();
                    // The app can send Stop and Seek together in one callback.
                    if stop {
                        sender.try_send(RealtimeCommand::Seek(0)).unwrap();
                    }
                    let release = render_chunks(&mut engine, frames + 10, chunk);
                    assert!(!engine.transport().playing);
                    assert_eq!(sender.frame_position(), if stop { 0 } else { paused_frame });
                    for channel in 0..channels {
                        assert_eq!(release[channel], start[(frames - 1) * channels + channel]);
                        assert_eq!(release[(frames - 1) * channels + channel], 0.0);
                    }
                    assert_bounded_steps(&release, channels, 0.8 / (frames - 1) as f32);
                    assert!(
                        release[frames * channels..]
                            .iter()
                            .all(|sample| *sample == 0.0)
                    );
                    assert_eq!(engine.process(&mut [0.0; 2]), ProcessStatus::Silence);
                }
            }
        }
    }
}

#[test]
fn seek_and_rapid_restarts_bridge_from_the_last_emitted_stereo_frame() {
    let (sender, mut engine) = playing_engine(48_000, ChannelLayout::Stereo);
    let mut previous = render_chunks(&mut engine, 256, 64);
    for commands in [
        vec![RealtimeCommand::Seek(20_000)],
        vec![RealtimeCommand::Pause],
        vec![RealtimeCommand::Play],
        vec![RealtimeCommand::Stop, RealtimeCommand::Seek(0)],
        vec![RealtimeCommand::Seek(1_000), RealtimeCommand::Play],
    ] {
        for command in commands {
            sender.try_send(command).unwrap();
        }
        let output = render_chunks(&mut engine, 17, 7);
        assert_eq!(&output[..2], &previous[previous.len() - 2..]);
        previous = output;
    }
    let settled = render_chunks(&mut engine, 256, 64);
    assert_eq!(&settled[settled.len() - 2..], &[0.8, -0.4]);
    sender.try_send(RealtimeCommand::Play).unwrap();
    sender
        .try_send(RealtimeCommand::Seek(sender.frame_position()))
        .unwrap();
    let unchanged = render_chunks(&mut engine, 8, 8);
    assert_eq!(unchanged, [0.8, -0.4].repeat(8));
}

#[test]
fn invalidation_discards_audible_release_before_snapshotless_activation() {
    let (sender, mut engine) = playing_engine(48_000, ChannelLayout::Mono);
    render_chunks(&mut engine, 256, 64);
    sender.try_send(RealtimeCommand::Pause).unwrap();
    assert!(
        render_chunks(&mut engine, 16, 16)
            .iter()
            .any(|sample| *sample > 0.0)
    );
    sender
        .try_send(RealtimeCommand::ActivateTimeline(TimelineActivation {
            generation: 2,
            snapshot: None,
            preserve_transport: false,
            sample_rate: 48_000,
            total_frames: 100_000,
            frame: 2_000,
            playing: true,
            loop_range: None,
            metronome: RealtimeMetronome::default(),
        }))
        .unwrap();
    assert_eq!(render_chunks(&mut engine, 256, 64), vec![0.0; 256]);
    assert_eq!(sender.active_generation(), 2);
}
