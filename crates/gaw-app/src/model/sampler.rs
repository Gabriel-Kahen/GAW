use super::{ChangeSource, Command, ProjectViewModel, SamplerZone, Transaction, asset_duration};

/// Clamp the slice to actual sample frames, including files shorter than 1 ms.
#[allow(clippy::cast_precision_loss)]
fn sampler_source_range(
    asset: &gaw_core::AudioAsset,
    start: f64,
    duration: f64,
) -> Option<gaw_core::SourceRange> {
    if !start.is_finite() || !duration.is_finite() {
        return None;
    }
    let (frames, rate) = if let gaw_core::AudioAssetDefinition::Imported(source) = &asset.definition
    {
        (source.frames.0, source.sample_rate.value())
    } else {
        let revision = asset.current_revision()?;
        (
            revision.frames.0,
            revision.render_context.sample_rate.value(),
        )
    };
    if frames == 0 {
        return None;
    }
    let rate = f64::from(rate);
    let frames = frames as f64;
    let start_frame = (start * rate).round().clamp(0.0, frames - 1.0);
    let end_frame = (start_frame + (duration * rate).round().max(1.0)).min(frames);
    let start = start_frame / rate;
    let mut duration = end_frame / rate - start;
    if start + duration > frames / rate {
        duration = duration.next_down();
    }
    Some(gaw_core::SourceRange {
        start: gaw_core::Seconds::new(start).ok()?,
        duration: gaw_core::Seconds::new(duration).ok()?,
    })
}

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
        let Some(asset) = self
            .project
            .assets
            .iter()
            .find(|asset| asset.id.to_string() == edited.asset_id)
        else {
            return;
        };
        let Some(source) = sampler_source_range(
            asset,
            edited.source_start_seconds,
            edited.source_duration_seconds,
        ) else {
            return;
        };
        let asset_id = asset.id;
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
        zone.source = source;
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

    /// Assigns a sample in one undoable edit. A new zone is immediately playable.
    pub(crate) fn set_sampler_zone_asset(
        &mut self,
        track_id: gaw_core::TrackId,
        zone_id: Option<gaw_core::SamplerZoneId>,
        asset_id: gaw_core::AssetId,
    ) -> Result<gaw_core::SamplerZoneId, String> {
        let asset = self
            .project
            .assets
            .iter()
            .find(|asset| asset.id == asset_id)
            .ok_or("The selected audio no longer exists.")?;
        let duration =
            asset_duration(asset).ok_or("Render this audio before using it as a sample.")?;
        let source = sampler_source_range(asset, 0.0, duration)
            .ok_or("The selected audio has no sample frames.")?;
        let mut instrument = self
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.instrument.clone())
            .ok_or("The selected sampler no longer exists.")?;
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        let selected_zone_id = if let Some(zone_id) = zone_id {
            let zone = sampler
                .zones
                .iter_mut()
                .find(|zone| zone.id == zone_id)
                .ok_or("The selected sample zone no longer exists.")?;
            zone.asset_id = asset_id;
            zone.source = source;
            zone_id
        } else {
            let zone = gaw_core::SamplerZone {
                id: gaw_core::SamplerZoneId::new(),
                name: asset.name.clone(),
                asset_id,
                source,
                root_note: gaw_core::MidiNote::new(60).expect("valid note"),
                note_range: gaw_core::NoteRange::new(0, 127).expect("valid range"),
                velocity_range: gaw_core::VelocityRange::new(0, 127).expect("valid range"),
                playback: gaw_core::SamplerPlayback::NoteGated,
                gain: gaw_core::Decibels::new(0.0).expect("valid gain"),
                velocity_sensitivity: gaw_core::Ratio::new(1.0).expect("valid ratio"),
                attack: gaw_core::Milliseconds::new(0.0).expect("valid attack"),
                release: gaw_core::Milliseconds::new(50.0).expect("valid release"),
                reverse: false,
                choke_group: None,
            };
            let id = zone.id;
            sampler.zones.push(zone);
            id
        };
        self.commit(
            &Transaction::named(
                "Choose sampler audio",
                [Command::SetTrackInstrument {
                    track_id,
                    instrument: Some(instrument),
                }],
            ),
            ChangeSource::Ui,
            &[track_id.to_string(), selected_zone_id.to_string()],
            0.0,
        )
        .map_err(|error| error.to_string())?;
        Ok(selected_zone_id)
    }

    /// Adds a playable zone referencing the first usable audio asset.
    pub fn add_sampler_zone(&mut self, track_index: usize) {
        let Some(track_id) = self.current_track_id(track_index) else {
            return;
        };
        let Some(asset_id) = self.project.assets.iter().find_map(|asset| {
            sampler_source_range(asset, 0.0, asset_duration(asset)?).map(|_| asset.id)
        }) else {
            return;
        };
        if let Err(error) = self.set_sampler_zone_asset(track_id, None, asset_id) {
            self.last_error = Some(error);
        }
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

#[cfg(test)]
#[allow(clippy::float_cmp)] // Source ranges should exactly match canonical asset bounds.
mod tests {
    use super::*;
    use gaw_core::{AssetId, AudioAssetDefinition, SamplerZoneId, TrackId, Validate as _};

    fn fixture() -> (ProjectViewModel, TrackId, AssetId) {
        let vm = ProjectViewModel::demo();
        let track_id = vm.current_track_id(1).unwrap();
        let asset_id = vm.project.assets[0].id;
        (vm, track_id, asset_id)
    }

    fn zone(
        vm: &ProjectViewModel,
        track_id: TrackId,
        zone_id: SamplerZoneId,
    ) -> gaw_core::SamplerZone {
        let instrument = vm
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .unwrap()
            .instrument
            .as_ref()
            .unwrap();
        let gaw_core::InstrumentKind::Sampler(sampler) = &instrument.kind;
        sampler
            .zones
            .iter()
            .find(|zone| zone.id == zone_id)
            .unwrap()
            .clone()
    }

    #[test]
    fn choosing_first_sample_creates_playable_zone_and_one_undo_restores_project() {
        let (vm, track_id, asset_id) = fixture();
        let mut project = vm.project;
        let instrument = project
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .unwrap()
            .instrument
            .as_mut()
            .unwrap();
        let gaw_core::InstrumentKind::Sampler(sampler) = &mut instrument.kind;
        sampler.zones.clear();
        let mut vm = ProjectViewModel::from_project(project).unwrap();
        let before = vm.project.clone();
        let zone_id = vm.set_sampler_zone_asset(track_id, None, asset_id).unwrap();
        let created = zone(&vm, track_id, zone_id);
        assert_eq!(
            created.note_range,
            gaw_core::NoteRange::new(0, 127).unwrap()
        );
        assert_eq!(created.playback, gaw_core::SamplerPlayback::NoteGated);
        assert_eq!(created.source.start.value(), 0.0);
        assert_eq!(
            created.source.duration.value(),
            asset_duration(&before.assets[0]).unwrap()
        );
        assert_eq!(vm.project.assets, before.assets);
        vm.project.validate().unwrap();
        vm.undo(0.0);
        assert_eq!(vm.project, before);
    }

    #[test]
    fn replacing_sample_resets_slice_to_short_asset_and_preserves_mapping() {
        let (mut vm, track_id, asset_id) = fixture();
        let mut short_asset = vm.project.assets[0].clone();
        short_asset.id = AssetId::new();
        short_asset.name = "One frame".into();
        let AudioAssetDefinition::Imported(source) = &mut short_asset.definition else {
            panic!("fixture asset must be imported");
        };
        source.frames = gaw_core::FrameCount(1);
        let short_id = short_asset.id;
        vm.project.assets.push(short_asset);
        let mut vm = ProjectViewModel::from_project(vm.project).unwrap();
        let zone_id = vm.set_sampler_zone_asset(track_id, None, asset_id).unwrap();
        let before = vm.project.clone();
        let old_zone = zone(&vm, track_id, zone_id);
        assert_eq!(
            vm.set_sampler_zone_asset(track_id, Some(zone_id), short_id)
                .unwrap(),
            zone_id
        );
        let replaced = zone(&vm, track_id, zone_id);
        let mut expected = old_zone;
        expected.asset_id = short_id;
        expected.source =
            sampler_source_range(vm.project.assets.last().unwrap(), 0.0, 1.0).unwrap();
        assert_eq!(replaced, expected);
        assert!(replaced.source.duration.value() < 0.001);
        assert_eq!(vm.project.assets, before.assets);
        vm.project.validate().unwrap();
        vm.undo(0.0);
        assert_eq!(vm.project, before);
    }

    #[test]
    fn trimming_clamps_to_frames_and_rejects_nonfinite_input() {
        let (mut vm, track_id, _) = fixture();
        let before = vm.project.assets.clone();
        let mut edited = vm.current_composition().tracks[1].sampler_zones[0].clone();
        edited.source_start_seconds = 1e20;
        edited.source_duration_seconds = 1e20;
        vm.update_sampler_zone(1, 0, &edited);
        assert!(vm.last_error.is_none());
        let instrument = vm
            .project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .unwrap()
            .instrument
            .as_ref()
            .unwrap();
        let gaw_core::InstrumentKind::Sampler(sampler) = &instrument.kind;
        let trimmed = &sampler.zones[0];
        let asset = vm
            .project
            .assets
            .iter()
            .find(|asset| asset.id == trimmed.asset_id)
            .unwrap();
        assert!(
            trimmed.source.start.value() + trimmed.source.duration.value()
                <= asset_duration(asset).unwrap()
        );
        assert!(trimmed.source.duration.value() > 0.0);
        assert_eq!(vm.project.assets, before);
        vm.project.validate().unwrap();
        let project = vm.project.clone();
        edited.source_start_seconds = f64::NAN;
        vm.update_sampler_zone(1, 0, &edited);
        assert_eq!(vm.project, project);
    }

    #[test]
    fn stale_zone_id_does_not_replace_another_zone() {
        let (mut vm, track_id, asset_id) = fixture();
        let before = vm.project.clone();
        assert!(
            vm.set_sampler_zone_asset(track_id, Some(SamplerZoneId::new()), asset_id)
                .is_err()
        );
        assert_eq!(vm.project, before);
    }
}
