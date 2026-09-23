use std::{sync::Arc, thread};

use crossbeam_channel::{Receiver, Sender, unbounded};
use gaw_audio::{
    ChannelLayout, PreparedLiveSampler, RealtimeCommand, RealtimeEngineConfig,
    StorePlaybackCompiler,
};
use gaw_core::{AudioAsset, Instrument, Project, TrackId, TrackKind};
use gaw_project::ProjectStore;

use super::NativeController;
use crate::model::ProjectViewModel;

#[derive(Clone, Debug, Default)]
pub(crate) struct KeyboardInstrumentStatus {
    pub(crate) loading: bool,
    pub(crate) ready: bool,
    pub(crate) error: Option<String>,
}

/// Deliberately excludes clips: recording a note must not interrupt held voices.
#[derive(Clone, Debug, PartialEq)]
struct InstrumentKey {
    track_id: TrackId,
    instrument: Instrument,
    assets: Vec<AudioAsset>,
    volume_db: f32,
    muted: bool,
    bpm: f64,
    random_seed: u64,
    sample_rate: u32,
}

impl InstrumentKey {
    fn from_project(project: &Project, track_id: TrackId, sample_rate: u32) -> Option<Self> {
        let track = project.tracks.iter().find(|track| track.id == track_id)?;
        if track.kind != TrackKind::Event {
            return None;
        }
        let instrument = track.instrument.clone()?;
        // Processed zones can depend on assets beyond their direct source IDs.
        let assets = project.assets.clone();
        let any_solo = project.tracks.iter().any(|other| {
            other.composition_id == track.composition_id && other.solo && !other.muted
        });
        Some(Self {
            track_id,
            instrument,
            assets,
            volume_db: track.volume_db,
            muted: track.muted || (any_solo && !track.solo),
            bpm: project.bpm.value(),
            random_seed: project.settings.random_seed,
            sample_rate,
        })
    }
}

struct PrepareRequest {
    generation: u64,
    project: Arc<Project>,
    track_id: TrackId,
    sample_rate: u32,
}

struct PrepareResult {
    generation: u64,
    result: Result<PreparedLiveSampler, String>,
}

#[derive(Debug)]
pub(super) struct KeyboardInstrument {
    track_id: Option<TrackId>,
    checked: Option<(u64, Option<TrackId>, Option<u32>)>,
    key: Option<InstrumentKey>,
    generation: u64,
    requests: Sender<PrepareRequest>,
    results: Receiver<PrepareResult>,
    status: KeyboardInstrumentStatus,
}

impl KeyboardInstrument {
    pub(super) fn new(store: ProjectStore) -> Self {
        let (requests, receiver) = unbounded::<PrepareRequest>();
        let (sender, results) = unbounded();
        thread::Builder::new()
            .name("gaw-keyboard-sampler".into())
            .spawn(move || {
                let mut compiler = StorePlaybackCompiler::default();
                while let Ok(mut request) = receiver.recv() {
                    // Keep only the newest configuration while a previous decode was running.
                    while let Ok(newer) = receiver.try_recv() {
                        request = newer;
                    }
                    let result = compiler
                        .prepare_live_sampler(
                            &store,
                            &request.project,
                            request.track_id,
                            RealtimeEngineConfig {
                                sample_rate: request.sample_rate,
                                output_layout: ChannelLayout::Stereo,
                                maximum_block_frames: 8_192,
                                maximum_commands_per_block: 64,
                            },
                        )
                        .map_err(|error| error.to_string());
                    if sender
                        .send(PrepareResult {
                            generation: request.generation,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("keyboard sampler worker should start");
        Self {
            track_id: None,
            checked: None,
            key: None,
            generation: 0,
            requests,
            results,
            status: KeyboardInstrumentStatus::default(),
        }
    }

    pub(super) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.checked = None;
        self.key = None;
        self.status = KeyboardInstrumentStatus::default();
    }
}

pub(super) fn is_keyboard_command(command: &RealtimeCommand) -> bool {
    matches!(
        command,
        RealtimeCommand::InstallLiveSampler(_)
            | RealtimeCommand::LiveNoteOn { .. }
            | RealtimeCommand::LiveNoteOnTuned { .. }
            | RealtimeCommand::LiveNoteOff { .. }
            | RealtimeCommand::LiveAllNotesOff
    )
}

impl NativeController {
    pub(crate) fn configure_keyboard_instrument(
        &mut self,
        vm: &ProjectViewModel,
        track_id: Option<TrackId>,
    ) {
        self.keyboard.track_id = track_id;
        self.pump_keyboard(vm);
    }

    pub(crate) fn keyboard_instrument_status(&self) -> super::KeyboardInstrumentStatus {
        self.keyboard.status.clone()
    }

    pub(crate) fn keyboard_note_on_tuned(&mut self, note: u8, velocity: u8, cents: f64) -> bool {
        if !self.keyboard.status.ready
            || self.audio.is_none()
            || self.pending_audio.len() >= 128
            || note > 127
            || velocity == 0
            || !cents.is_finite()
            || cents.abs() > 100.0
        {
            return false;
        }
        self.enqueue_audio(RealtimeCommand::LiveNoteOnTuned {
            note,
            velocity: f32::from(velocity.min(127)) / 127.0,
            cents,
        });
        self.flush_audio();
        true
    }

    pub(crate) fn keyboard_note_off(&mut self, note: u8) {
        if self.audio.is_some() {
            if self.pending_audio.len() >= 128 {
                self.keyboard_all_notes_off();
                return;
            }
            self.enqueue_audio(RealtimeCommand::LiveNoteOff { note });
            self.flush_audio();
        }
    }

    pub(crate) fn keyboard_all_notes_off(&mut self) {
        self.pending_audio.retain(|command| {
            !matches!(
                command,
                RealtimeCommand::LiveNoteOn { .. }
                    | RealtimeCommand::LiveNoteOnTuned { .. }
                    | RealtimeCommand::LiveNoteOff { .. }
                    | RealtimeCommand::LiveAllNotesOff
            )
        });
        if self.audio.is_some() {
            // Reserve one emergency slot: losing a release can leave a stuck voice.
            self.pending_audio
                .push_back(RealtimeCommand::LiveAllNotesOff);
            self.flush_audio();
        }
    }

    pub(super) fn pump_keyboard(&mut self, vm: &ProjectViewModel) {
        let sample_rate = self
            .audio
            .as_ref()
            .map(|audio| audio.device.info().sample_rate);
        let checked = (vm.revision(), self.keyboard.track_id, sample_rate);
        if self.keyboard.checked != Some(checked) {
            self.keyboard.checked = Some(checked);
            let key = self
                .keyboard
                .track_id
                .zip(sample_rate)
                .and_then(|(track_id, rate)| {
                    InstrumentKey::from_project(vm.project(), track_id, rate)
                });
            if key != self.keyboard.key {
                self.keyboard.generation = self.keyboard.generation.wrapping_add(1);
                self.keyboard.key = key;
                self.keyboard.status = KeyboardInstrumentStatus::default();
                self.pending_audio
                    .retain(|command| !is_keyboard_command(command));
                if self.audio.is_some() {
                    self.pending_audio
                        .push_back(RealtimeCommand::InstallLiveSampler(None));
                }
                if let Some(key) = &self.keyboard.key {
                    self.keyboard.status.loading = true;
                    let request = PrepareRequest {
                        generation: self.keyboard.generation,
                        project: vm.project_snapshot(),
                        track_id: key.track_id,
                        sample_rate: key.sample_rate,
                    };
                    if self.keyboard.requests.send(request).is_err() {
                        self.keyboard.status.loading = false;
                        self.keyboard.status.error =
                            Some("Keyboard sampler worker disconnected".into());
                    }
                }
            }
        }
        while let Ok(completed) = self.keyboard.results.try_recv() {
            if completed.generation != self.keyboard.generation || self.keyboard.key.is_none() {
                continue;
            }
            self.keyboard.status.loading = false;
            match completed.result {
                Ok(sampler) => {
                    self.pending_audio.retain(|command| {
                        !matches!(command, RealtimeCommand::InstallLiveSampler(_))
                    });
                    self.pending_audio
                        .push_back(RealtimeCommand::InstallLiveSampler(Some(Box::new(sampler))));
                    self.keyboard.status.ready = true;
                    self.keyboard.status.error = None;
                }
                Err(error) => {
                    self.keyboard.status.ready = false;
                    self.keyboard.status.error = Some(error);
                }
            }
        }
        self.flush_audio();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gaw_core::{
        Beats, Bpm, Event, EventClip, EventData, NoteEvent, SampleRate, Sampler, Track,
    };

    fn project() -> (Project, TrackId) {
        let mut project = Project::new(
            "Keyboard test",
            Bpm::new(120.0).unwrap(),
            SampleRate::new(48_000).unwrap(),
        );
        let track = Track::event(
            project.root_composition_id,
            "Piano",
            Instrument::sampler("Piano", Sampler::new(16).unwrap()),
        );
        let id = track.id;
        project.compositions[0].track_ids.push(id);
        project.tracks.push(track);
        (project, id)
    }

    #[test]
    fn recording_notes_does_not_rebuild_the_live_instrument() {
        let (mut project, track_id) = project();
        let before = InstrumentKey::from_project(&project, track_id, 48_000);
        let mut data = EventData::new("Take");
        data.events.push(Event::Note(
            NoteEvent::new(Beats::new(0.0).unwrap(), Beats::new(1.0).unwrap(), 60, 100).unwrap(),
        ));
        project.tracks[0]
            .clips
            .push(gaw_core::Clip::Event(EventClip::new(
                data.id,
                Beats::new(0.0).unwrap(),
                Beats::new(4.0).unwrap(),
            )));
        project.event_data.push(data);
        assert_eq!(
            before,
            InstrumentKey::from_project(&project, track_id, 48_000)
        );
        project.tracks[0].volume_db = -6.0;
        assert_ne!(
            before,
            InstrumentKey::from_project(&project, track_id, 48_000)
        );
    }

    #[test]
    fn other_track_solo_and_output_rate_invalidate_live_instrument() {
        let (mut project, track_id) = project();
        let before = InstrumentKey::from_project(&project, track_id, 48_000);
        assert_ne!(
            before,
            InstrumentKey::from_project(&project, track_id, 44_100)
        );
        let mut other = Track::audio(project.root_composition_id, "Other");
        other.solo = true;
        project.tracks.push(other);
        assert_ne!(
            before,
            InstrumentKey::from_project(&project, track_id, 48_000)
        );
        project.tracks[1].muted = true;
        assert_eq!(
            before,
            InstrumentKey::from_project(&project, track_id, 48_000)
        );
    }
}
