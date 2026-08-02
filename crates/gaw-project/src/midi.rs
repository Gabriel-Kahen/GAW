//! Standard MIDI File interchange. MIDI is never canonical project state.

use std::{
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

use gaw_core::{
    Beats, Bipolar, Bpm, ControlEvent, Event, EventData, MidiVelocity, NoteEvent, PitchBendEvent,
    Ratio,
};
use midly::{
    Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind,
    num::{u4, u7, u15, u24, u28},
};
use serde::Serialize;

/// Parsed musical streams plus a non-authoritative tempo suggestion.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MidiImport {
    pub event_data: Vec<EventData>,
    pub suggested_bpm: Option<Bpm>,
}

#[derive(Debug, thiserror::Error)]
pub enum MidiError {
    #[error("could not read MIDI file: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid MIDI file: {0}")]
    Parse(String),
    #[error("SMPTE-timed MIDI is unsupported; use metrical PPQN timing")]
    Timecode,
    #[error("MIDI track {track} ends with note {note} on channel {channel} still active")]
    UnclosedNote { track: usize, channel: u8, note: u8 },
    #[error("MIDI track {track} contains a zero-length note {note} at tick {tick}")]
    ZeroLengthNote { track: usize, note: u8, tick: u64 },
    #[error("event data contains unsupported controller name {0:?}")]
    UnsupportedController(String),
    #[error("MIDI timing exceeds the file format's 28-bit delta limit")]
    DeltaOverflow,
    #[error("MIDI output path has no parent directory")]
    MissingParent,
    #[error("invalid event data: {0}")]
    Model(#[from] gaw_core::ModelError),
}

/// Imports all event-bearing SMF tracks. Track/channel routing remains an
/// interchange concern; canonical GAW state stores explicit event streams.
#[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
pub fn import_midi(path: impl AsRef<Path>) -> Result<MidiImport, MidiError> {
    let path = path.as_ref();
    let bytes = std::fs::read(path)?;
    let smf = Smf::parse(&bytes).map_err(|error| MidiError::Parse(error.to_string()))?;
    let Timing::Metrical(ppqn) = smf.header.timing else {
        return Err(MidiError::Timecode);
    };
    let ppqn = u64::from(ppqn.as_int());
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("MIDI");
    let mut suggested_bpm = None;
    let mut streams = Vec::new();

    for (track_index, track) in smf.tracks.iter().enumerate() {
        let mut tick = 0_u64;
        let mut name = None;
        let mut events = Vec::new();
        let mut active = BTreeMap::<(u8, u8), VecDeque<(u64, u8)>>::new();

        for item in track {
            tick = tick.saturating_add(u64::from(item.delta.as_int()));
            match item.kind {
                TrackEventKind::Meta(MetaMessage::TrackName(value)) => {
                    name = Some(String::from_utf8_lossy(value).into_owned());
                }
                TrackEventKind::Meta(MetaMessage::Tempo(micros)) if suggested_bpm.is_none() => {
                    let micros = f64::from(micros.as_int());
                    if micros > 0.0 {
                        suggested_bpm = Some(Bpm::new(60_000_000.0 / micros)?);
                    }
                }
                TrackEventKind::Midi { channel, message } => match message {
                    MidiMessage::NoteOn { key, vel } if vel.as_int() != 0 => active
                        .entry((channel.as_int(), key.as_int()))
                        .or_default()
                        .push_back((tick, vel.as_int())),
                    MidiMessage::NoteOn { key, vel } | MidiMessage::NoteOff { key, vel } => {
                        let key_id = (channel.as_int(), key.as_int());
                        let Some((start, velocity)) =
                            active.get_mut(&key_id).and_then(VecDeque::pop_front)
                        else {
                            continue;
                        };
                        if start == tick {
                            return Err(MidiError::ZeroLengthNote {
                                track: track_index,
                                note: key.as_int(),
                                tick,
                            });
                        }
                        events.push(Event::Note(NoteEvent {
                            start: beats(start, ppqn)?,
                            duration: beats(tick - start, ppqn)?,
                            note: key.as_int().try_into()?,
                            velocity: velocity.try_into()?,
                            release_velocity: MidiVelocity::new(vel.as_int())?,
                        }));
                    }
                    MidiMessage::Controller { controller, value } => {
                        events.push(Event::Control(ControlEvent {
                            time: beats(tick, ppqn)?,
                            controller: format!("midi.cc.{}", controller.as_int()),
                            value: Ratio::new(f64::from(value.as_int()) / 127.0)?,
                        }));
                    }
                    MidiMessage::PitchBend { bend } => {
                        events.push(Event::PitchBend(PitchBendEvent {
                            time: beats(tick, ppqn)?,
                            value: Bipolar::new(f64::from(bend.as_int()) / 8192.0)?,
                        }));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        if let Some((&(channel, note), _)) = active.iter().find(|(_, notes)| !notes.is_empty()) {
            return Err(MidiError::UnclosedNote {
                track: track_index,
                channel,
                note,
            });
        }
        if !events.is_empty() {
            let mut data =
                EventData::new(name.filter(|v| !v.trim().is_empty()).unwrap_or_else(|| {
                    if smf.tracks.len() == 1 {
                        stem.to_owned()
                    } else {
                        format!("{stem} {}", track_index + 1)
                    }
                }));
            data.events = events;
            data.sort();
            streams.push(data);
        }
    }
    if streams.is_empty() {
        streams.push(EventData::new(stem));
    }
    Ok(MidiImport {
        event_data: streams,
        suggested_bpm,
    })
}

/// Exports one canonical event stream as a deterministic format-0 SMF.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub fn export_midi(
    data: &EventData,
    bpm: Bpm,
    ppqn: u16,
    path: impl AsRef<Path>,
) -> Result<(), MidiError> {
    let ppqn = ppqn.clamp(1, 0x7fff);
    let mut timed = Vec::new();
    let tempo = (60_000_000.0 / bpm.value())
        .round()
        .clamp(1.0, 16_777_215.0) as u32;
    timed.push(Timed {
        tick: 0,
        priority: 0,
        kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(tempo))),
    });
    for event in &data.events {
        match event {
            Event::Note(note) => {
                let start = ticks(note.start, ppqn);
                let end = start.saturating_add(ticks(note.duration, ppqn).max(1));
                timed.push(Timed {
                    tick: start,
                    priority: 2,
                    kind: midi(MidiMessage::NoteOn {
                        key: u7::new(note.note.value()),
                        vel: u7::new(note.velocity.value()),
                    }),
                });
                timed.push(Timed {
                    tick: end,
                    priority: 1,
                    kind: midi(MidiMessage::NoteOff {
                        key: u7::new(note.note.value()),
                        vel: u7::new(note.release_velocity.value()),
                    }),
                });
            }
            Event::Control(control) => {
                let controller = control
                    .controller
                    .strip_prefix("midi.cc.")
                    .and_then(|value| value.parse::<u8>().ok())
                    .filter(|value| *value <= 127)
                    .ok_or_else(|| MidiError::UnsupportedController(control.controller.clone()))?;
                timed.push(Timed {
                    tick: ticks(control.time, ppqn),
                    priority: 1,
                    kind: midi(MidiMessage::Controller {
                        controller: u7::new(controller),
                        value: u7::new((control.value.value() * 127.0).round() as u8),
                    }),
                });
            }
            Event::PitchBend(bend) => timed.push(Timed {
                tick: ticks(bend.time, ppqn),
                priority: 1,
                kind: midi(MidiMessage::PitchBend {
                    bend: midly::PitchBend::from_int(
                        (bend.value.value() * 8192.0).round().clamp(-8192.0, 8191.0) as i16,
                    ),
                }),
            }),
        }
    }
    timed.sort_by_key(|item| (item.tick, item.priority));
    let mut previous = 0_u64;
    let mut track = Vec::with_capacity(timed.len() + 1);
    for item in timed {
        let delta = item.tick.saturating_sub(previous);
        let delta = u32::try_from(delta)
            .ok()
            .filter(|value| *value <= 0x0fff_ffff)
            .ok_or(MidiError::DeltaOverflow)?;
        track.push(TrackEvent {
            delta: u28::new(delta),
            kind: item.kind,
        });
        previous = item.tick;
    }
    track.push(TrackEvent {
        delta: u28::new(0),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });
    let smf = Smf {
        header: Header::new(Format::SingleTrack, Timing::Metrical(u15::new(ppqn))),
        tracks: vec![track],
    };

    let path = path.as_ref();
    let parent = path.parent().ok_or(MidiError::MissingParent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        smf.write_std(&mut writer)
            .map_err(|error| MidiError::Io(std::io::Error::other(error)))?;
        writer.flush()?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    if let Ok(directory) = File::open(parent) {
        directory.sync_all()?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Timed {
    tick: u64,
    priority: u8,
    kind: TrackEventKind<'static>,
}

const fn midi(message: MidiMessage) -> TrackEventKind<'static> {
    TrackEventKind::Midi {
        channel: u4::new(0),
        message,
    }
}

#[allow(clippy::cast_precision_loss)]
fn beats(ticks: u64, ppqn: u64) -> Result<Beats, gaw_core::ModelError> {
    Beats::new(ticks as f64 / ppqn as f64)
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ticks(beats: Beats, ppqn: u16) -> u64 {
    (beats.value() * f64::from(ppqn)).round().max(0.0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use gaw_core::{MidiNote, Validate};

    #[test]
    fn round_trip_notes_control_pitch_and_tempo() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.mid");
        let mut data = EventData::new("Lead");
        data.events = vec![
            Event::Note(
                NoteEvent::new(Beats::new(1.25).unwrap(), Beats::new(0.5).unwrap(), 64, 111)
                    .unwrap(),
            ),
            Event::Control(ControlEvent {
                time: Beats::new(0.5).unwrap(),
                controller: "midi.cc.74".into(),
                value: Ratio::new(0.75).unwrap(),
            }),
            Event::PitchBend(PitchBendEvent {
                time: Beats::new(0.75).unwrap(),
                value: Bipolar::new(-0.25).unwrap(),
            }),
        ];
        data.sort();
        export_midi(&data, Bpm::new(123.0).unwrap(), 960, &path).unwrap();
        let imported = import_midi(&path).unwrap();
        assert_eq!(imported.event_data.len(), 1);
        assert_eq!(imported.event_data[0].events.len(), data.events.len());
        let Event::Control(control) = &imported.event_data[0].events[0] else {
            panic!("first event was not controller data");
        };
        assert!((control.value.value() - 95.0 / 127.0).abs() < f64::EPSILON);
        assert_eq!(imported.event_data[0].events[1..], data.events[1..]);
        assert!((imported.suggested_bpm.unwrap().value() - 123.0).abs() < 0.001);
        assert!(imported.event_data[0].events.iter().any(|event| matches!(
            event,
            Event::Note(note) if note.note == MidiNote::new(64).unwrap()
        )));
    }

    #[test]
    fn rejects_timecode_and_unclosed_notes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("broken.mid");
        let smf = Smf {
            header: Header::new(Format::SingleTrack, Timing::Metrical(u15::new(96))),
            tracks: vec![vec![TrackEvent {
                delta: u28::new(0),
                kind: midi(MidiMessage::NoteOn {
                    key: u7::new(60),
                    vel: u7::new(100),
                }),
            }]],
        };
        smf.save(&path).unwrap();
        assert!(matches!(
            import_midi(&path),
            Err(MidiError::UnclosedNote { .. })
        ));
    }

    #[test]
    fn imported_data_satisfies_project_event_invariants() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.mid");
        Smf {
            header: Header::new(Format::SingleTrack, Timing::Metrical(u15::new(480))),
            tracks: vec![vec![]],
        }
        .save(&path)
        .unwrap();
        let imported = import_midi(&path).unwrap();
        let mut project = gaw_core::Project::new(
            "MIDI",
            Bpm::new(120.0).unwrap(),
            gaw_core::SampleRate::new(48_000).unwrap(),
        );
        project.event_data = imported.event_data;
        assert!(project.validate().is_ok());
    }
}
