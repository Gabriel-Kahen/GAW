use super::{Command, ProjectViewModel, SamplerZone, Transaction, asset_duration};

impl ProjectViewModel {
    pub fn toggle_first_sampler_zone_reverse(&mut self, track_index: usize) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(mut instrument) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.clone())
        else {
            return;
        };
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        let Some(zone) = sampler.zones.first_mut() else {
            return;
        };
        let zone_id = zone.id;
        zone.reverse = !zone.reverse;
        let transaction = Transaction::named(
            "Edit sampler zone",
            [Command::SetTrackInstrument {
                track_id,
                instrument: Some(instrument),
            }],
        );
        self.commit_ui(&transaction, &[track_id.to_string(), zone_id.to_string()]);
    }

    pub fn update_sampler_zone(
        &mut self,
        track_index: usize,
        zone_index: usize,
        edited: &SamplerZone,
    ) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(mut instrument) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.clone())
        else {
            return;
        };
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        let Some(zone) = sampler.zones.get_mut(zone_index) else {
            return;
        };
        let Some(asset_id) = self
            .project
            .assets
            .iter()
            .find(|asset| asset.id.to_string() == edited.asset_id)
            .map(|asset| asset.id)
        else {
            return;
        };
        let Ok(source_start) = gaw_core::Seconds::new(edited.source_start_seconds.max(0.0)) else {
            return;
        };
        let Ok(source_duration) = gaw_core::Seconds::new(edited.source_duration_seconds.max(0.001))
        else {
            return;
        };
        let (
            Ok(root_note),
            Ok(note_range),
            Ok(velocity_range),
            Ok(gain),
            Ok(velocity_sensitivity),
            Ok(attack),
            Ok(release),
        ) = (
            gaw_core::MidiNote::new(edited.root_note),
            gaw_core::NoteRange::new(edited.low_note, edited.high_note),
            gaw_core::VelocityRange::new(edited.low_velocity, edited.high_velocity),
            gaw_core::Decibels::new(f64::from(edited.gain_db)),
            gaw_core::Ratio::new(f64::from(edited.velocity_sensitivity)),
            gaw_core::Milliseconds::new(f64::from(edited.attack_ms)),
            gaw_core::Milliseconds::new(f64::from(edited.release_ms)),
        )
        else {
            return;
        };
        zone.name.clone_from(&edited.name);
        zone.asset_id = asset_id;
        zone.source = gaw_core::SourceRange {
            start: source_start,
            duration: source_duration,
        };
        zone.root_note = root_note;
        zone.note_range = note_range;
        zone.velocity_range = velocity_range;
        zone.playback = if edited.one_shot {
            gaw_core::SamplerPlayback::OneShot
        } else {
            gaw_core::SamplerPlayback::NoteGated
        };
        zone.gain = gain;
        zone.velocity_sensitivity = velocity_sensitivity;
        zone.attack = attack;
        zone.release = release;
        zone.reverse = edited.reverse;
        zone.choke_group = edited.choke_group;
        let zone_id = zone.id;
        self.commit_ui(
            &Transaction::named(
                "Edit sampler zone",
                [Command::SetTrackInstrument {
                    track_id,
                    instrument: Some(instrument),
                }],
            ),
            &[track_id.to_string(), zone_id.to_string()],
        );
    }

    /// Adds a default zone referencing the first usable audio asset.
    ///
    /// # Panics
    /// Panics if built-in zone defaults violate the canonical model.
    pub fn add_sampler_zone(&mut self, track_index: usize) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(mut instrument) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.clone())
        else {
            return;
        };
        let Some(asset) = self.project.assets.first() else {
            return;
        };
        let duration = asset_duration(asset).unwrap_or(1.0).max(0.001);
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        let zone = gaw_core::SamplerZone {
            id: gaw_core::SamplerZoneId::new(),
            name: format!("Zone {}", sampler.zones.len() + 1),
            asset_id: asset.id,
            source: gaw_core::SourceRange {
                start: gaw_core::Seconds::new(0.0).expect("zero is valid"),
                duration: gaw_core::Seconds::new(duration).expect("asset duration is valid"),
            },
            root_note: gaw_core::MidiNote::new(60).expect("valid note"),
            note_range: gaw_core::NoteRange::new(60, 60).expect("valid range"),
            velocity_range: gaw_core::VelocityRange::new(0, 127).expect("valid range"),
            playback: gaw_core::SamplerPlayback::OneShot,
            gain: gaw_core::Decibels::new(0.0).expect("valid gain"),
            velocity_sensitivity: gaw_core::Ratio::new(1.0).expect("valid ratio"),
            attack: gaw_core::Milliseconds::new(0.0).expect("valid attack"),
            release: gaw_core::Milliseconds::new(50.0).expect("valid release"),
            reverse: false,
            choke_group: None,
        };
        let zone_id = zone.id;
        sampler.zones.push(zone);
        self.commit_ui(
            &Transaction::named(
                "Add sampler zone",
                [Command::SetTrackInstrument {
                    track_id,
                    instrument: Some(instrument),
                }],
            ),
            &[track_id.to_string(), zone_id.to_string()],
        );
    }

    pub fn remove_sampler_zone(&mut self, track_index: usize, zone_index: usize) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(mut instrument) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.clone())
        else {
            return;
        };
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        if zone_index >= sampler.zones.len() {
            return;
        }
        let zone_id = sampler.zones.remove(zone_index).id;
        self.commit_ui(
            &Transaction::named(
                "Remove sampler zone",
                [Command::SetTrackInstrument {
                    track_id,
                    instrument: Some(instrument),
                }],
            ),
            &[track_id.to_string(), zone_id.to_string()],
        );
    }

    pub fn update_sampler_settings(
        &mut self,
        track_index: usize,
        polyphony: u16,
        voice_stealing: &str,
        output_gain_db: f32,
    ) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(mut instrument) = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.clone())
        else {
            return;
        };
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        sampler.polyphony = polyphony.max(1);
        sampler.voice_stealing = match voice_stealing {
            "quietest" => gaw_core::VoiceStealing::Quietest,
            "lowestvelocity" | "lowest_velocity" => gaw_core::VoiceStealing::LowestVelocity,
            _ => gaw_core::VoiceStealing::Oldest,
        };
        let Ok(gain) = gaw_core::Decibels::new(f64::from(output_gain_db)) else {
            return;
        };
        sampler.output_gain = gain;
        let instrument_id = instrument.id;
        self.commit_ui(
            &Transaction::named(
                "Edit sampler settings",
                [Command::SetTrackInstrument {
                    track_id,
                    instrument: Some(instrument),
                }],
            ),
            &[track_id.to_string(), instrument_id.to_string()],
        );
    }
}
