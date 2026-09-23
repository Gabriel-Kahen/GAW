//! Read-only projection of canonical project data for the GUI.

use super::{
    Arc, Asset, BarTimelineGap, Clip, ClipKind, Composition, Effect, HashMap, MidiAsset, Note,
    Parameter, Project, RenderState, SamplerZone, SyncMode, Track, TrackKind, WaveformPoint,
    asset_duration,
};

/// Reused for every placement while building a projection; canonical vector order is unchanged.
struct ProjectionIndex {
    assets: HashMap<gaw_core::AssetId, usize>,
    tracks: HashMap<gaw_core::TrackId, usize>,
    event_data: HashMap<gaw_core::EventDataId, usize>,
    compositions: HashMap<gaw_core::CompositionId, usize>,
}

impl ProjectionIndex {
    fn new(project: &Project) -> Self {
        Self {
            assets: first_indexes(project.assets.iter().map(|asset| asset.id)),
            tracks: first_indexes(project.tracks.iter().map(|track| track.id)),
            event_data: first_indexes(project.event_data.iter().map(|data| data.id)),
            compositions: first_indexes(
                project
                    .compositions
                    .iter()
                    .map(|composition| composition.id),
            ),
        }
    }
}

fn first_indexes<Id: Eq + std::hash::Hash>(
    ids: impl ExactSizeIterator<Item = Id>,
) -> HashMap<Id, usize> {
    let mut indexes = HashMap::with_capacity(ids.len());
    for (index, id) in ids.enumerate() {
        // Preserve the first-match behavior of linear searches, even for duplicate IDs.
        indexes.entry(id).or_insert(index);
    }
    indexes
}

pub(crate) fn effect_view(processor: &gaw_core::Processor) -> Effect {
    let encoded = serde_json::to_value(processor).unwrap_or_default();
    let descriptors = processor.kind.parameter_descriptors();
    let parameters = descriptors
        .iter()
        .filter_map(|descriptor| {
            let value = encoded.get("parameters")?.get(descriptor.id)?.clone();
            Some(Parameter {
                id: descriptor.id.to_owned(),
                label: descriptor.id.replace('_', " "),
                value,
                value_type: descriptor.value_type,
                range: descriptor.range.map(|range| (range.minimum, range.maximum)),
                choices: descriptor.choices.iter().map(ToString::to_string).collect(),
                unit: format!("{:?}", descriptor.unit).to_lowercase(),
                automatable: descriptor.automation == gaw_core::AutomationSupport::Continuous
                    || descriptors.iter().any(|nested| {
                        nested
                            .id
                            .strip_prefix(descriptor.id)
                            .is_some_and(|suffix| suffix.starts_with("[]."))
                            && nested.automation == gaw_core::AutomationSupport::Continuous
                    }),
                display_hint: format!("{:?}", descriptor.display_hint).to_lowercase(),
            })
        })
        .collect();
    Effect {
        id: processor.id.to_string(),
        name: super::processor_name(processor.kind.type_id()),
        kind: processor.kind.type_id().to_owned(),
        enabled: processor.enabled,
        parameters,
    }
}

#[allow(clippy::too_many_lines)]
pub(super) fn adapt_project(
    project: &Project,
    asset_waveforms: Option<&HashMap<String, Arc<[WaveformPoint]>>>,
    clip_waveforms: Option<&HashMap<String, Arc<[WaveformPoint]>>>,
) -> (Vec<Asset>, Vec<Composition>) {
    let index = ProjectionIndex::new(project);
    let assets = project
        .assets
        .iter()
        .map(|asset| {
            let id = asset.id.to_string();
            let revision = asset.current_revision();
            let (definition, media_path, content_hash, sample_rate, frames, channels, effects) =
                match &asset.definition {
                    gaw_core::AudioAssetDefinition::Imported(source) => (
                        "imported",
                        Some(source.media_path.as_str().to_owned()),
                        Some(source.content_hash.to_string()),
                        source.sample_rate.value(),
                        source.frames.0,
                        match source.layout {
                            gaw_core::ChannelLayout::Mono => 1,
                            gaw_core::ChannelLayout::Stereo => 2,
                        },
                        Vec::new(),
                    ),
                    definition => {
                        let (kind, effects) = match definition {
                            gaw_core::AudioAssetDefinition::InstrumentGenerated { .. } => {
                                ("instrument_generated", Vec::new())
                            }
                            gaw_core::AudioAssetDefinition::CompositionGenerated { .. } => {
                                ("composition_generated", Vec::new())
                            }
                            gaw_core::AudioAssetDefinition::Processed { effects, .. } => {
                                ("processed", effects.iter().map(effect_view).collect())
                            }
                            gaw_core::AudioAssetDefinition::Materialized { .. } => {
                                ("materialized", Vec::new())
                            }
                            gaw_core::AudioAssetDefinition::Imported(_) => unreachable!(),
                        };
                        (
                            kind,
                            revision.map(|value| value.media_path.as_str().to_owned()),
                            revision.map(|value| value.content_hash.to_string()),
                            revision.map_or(0, |value| value.render_context.sample_rate.value()),
                            revision.map_or(0, |value| value.frames.0),
                            revision
                                .map_or(0, |value| value.render_context.layout.channels() as u8),
                            effects,
                        )
                    }
                };
            let duration = asset_duration(asset).unwrap_or(0.0) as f32;
            Asset {
                waveform: asset_waveforms
                    .and_then(|cache| cache.get(&id).cloned())
                    .unwrap_or_else(|| Arc::from([])),
                id: id.clone(),
                name: asset.name.clone(),
                duration_seconds: duration,
                channels,
                bpm: asset.tempo.map(|tempo| tempo.bpm.value() as f32),
                first_beat_seconds: asset.tempo.map(|tempo| tempo.first_beat.value() as f32),
                changed_by_agent: false,
                definition: definition.to_owned(),
                media_path,
                content_hash,
                sample_rate,
                frames,
                revision_count: asset.revisions.len(),
                current_revision: asset.current_revision_id.map(|value| value.to_string()),
                effects,
                structure_path: format!("project.assets[id={id}]"),
            }
        })
        .collect::<Vec<_>>();

    let compositions = project
        .compositions
        .iter()
        .map(|composition| {
            let composition_id = composition.id.to_string();
            let tracks = composition
                .track_ids
                .iter()
                .filter_map(|track_id| index.tracks.get(track_id).map(|&i| &project.tracks[i]))
                .map(|track| {
                    let track_id = track.id.to_string();
                    let mut clips = track
                        .clips
                        .iter()
                        .map(|clip| {
                            adapt_clip(project, &index, clip, asset_waveforms, clip_waveforms)
                        })
                        .collect::<Vec<_>>();
                    clips.sort_by(|left, right| left.start.total_cmp(&right.start));
                    let composition_clips = clips
                        .iter()
                        .any(|clip| matches!(clip.kind, ClipKind::Composition { .. }));
                    let sampler_zones = track
                        .instrument
                        .as_ref()
                        .map(|instrument| match &instrument.kind {
                            gaw_core::InstrumentKind::Sampler(sampler) => sampler
                                .zones
                                .iter()
                                .map(|zone| SamplerZone {
                                    id: zone.id.to_string(),
                                    name: zone.name.clone(),
                                    asset_id: zone.asset_id.to_string(),
                                    root_note: zone.root_note.value(),
                                    low_note: zone.note_range.low.value(),
                                    high_note: zone.note_range.high.value(),
                                    low_velocity: zone.velocity_range.low.value(),
                                    high_velocity: zone.velocity_range.high.value(),
                                    source_start_seconds: zone.source.start.value(),
                                    source_duration_seconds: zone.source.duration.value(),
                                    gain_db: zone.gain.value() as f32,
                                    velocity_sensitivity: zone.velocity_sensitivity.value() as f32,
                                    attack_ms: zone.attack.value() as f32,
                                    release_ms: zone.release.value() as f32,
                                    one_shot: zone.playback == gaw_core::SamplerPlayback::OneShot,
                                    reverse: zone.reverse,
                                    choke_group: zone.choke_group,
                                    structure_path: format!(
                                        "project.tracks[id={track_id}].instrument.zones[id={}]",
                                        zone.id
                                    ),
                                })
                                .collect(),
                        })
                        .unwrap_or_default();
                    let (sampler_polyphony, sampler_voice_stealing, sampler_output_gain_db) = track
                        .instrument
                        .as_ref()
                        .map_or((None, None, None), |instrument| match &instrument.kind {
                            gaw_core::InstrumentKind::Sampler(sampler) => (
                                Some(sampler.polyphony),
                                Some(format!("{:?}", sampler.voice_stealing).to_lowercase()),
                                Some(sampler.output_gain.value() as f32),
                            ),
                        });
                    Track {
                        id: track_id.clone(),
                        name: track.name.clone(),
                        kind: if track.kind == gaw_core::TrackKind::Event {
                            TrackKind::Event
                        } else if composition_clips {
                            TrackKind::Composition
                        } else {
                            TrackKind::Audio
                        },
                        muted: track.muted,
                        solo: track.solo,
                        volume_db: track.volume_db,
                        level: 0.0,
                        max_visual_length: clips
                            .iter()
                            .map(|clip| {
                                clip.length
                                    + match clip.kind {
                                        ClipKind::Composition { tail_beats, .. } => tail_beats,
                                        _ => 0.0,
                                    }
                            })
                            .fold(0.0, f32::max),
                        clips,
                        effects: track.effects.iter().map(effect_view).collect(),
                        sampler_zones,
                        sampler_polyphony,
                        sampler_voice_stealing,
                        sampler_output_gain_db,
                        structure_path: format!("project.tracks[id={track_id}]"),
                    }
                })
                .collect();
            Composition {
                id: composition_id.clone(),
                name: composition.name.clone(),
                length_beats: composition.length.value() as f32,
                tracks,
                track_groups: composition.track_groups.clone(),
                bar_timeline_gaps: composition
                    .bar_timeline_gaps
                    .iter()
                    .map(|gap| BarTimelineGap {
                        start: gap.start.value() as f32,
                        duration: gap.duration.value() as f32,
                    })
                    .collect(),
                output_effects: composition.output_effects.iter().map(effect_view).collect(),
                structure_path: format!("project.compositions[id={composition_id}]"),
            }
        })
        .collect();
    (assets, compositions)
}

pub(super) fn adapt_midi_assets(project: &Project) -> Vec<MidiAsset> {
    project
        .event_data
        .iter()
        .map(|data| {
            let note_count = data
                .events
                .iter()
                .filter(|event| matches!(event, gaw_core::Event::Note(_)))
                .count();
            let duration_beats = data.events.iter().fold(0.0_f64, |duration, event| {
                let end = match event {
                    gaw_core::Event::Note(note) => note.start.value() + note.duration.value(),
                    gaw_core::Event::Control(control) => control.time.value(),
                    gaw_core::Event::PitchBend(bend) => bend.time.value(),
                };
                duration.max(end)
            }) as f32;
            MidiAsset {
                id: data.id.to_string(),
                name: data.name.clone(),
                note_count,
                duration_beats,
                structure_path: format!("project.event_data[id={}]", data.id),
            }
        })
        .collect()
}

#[allow(clippy::too_many_lines)]
fn adapt_clip(
    project: &Project,
    index: &ProjectionIndex,
    clip: &gaw_core::Clip,
    asset_waveforms: Option<&HashMap<String, Arc<[WaveformPoint]>>>,
    clip_waveforms: Option<&HashMap<String, Arc<[WaveformPoint]>>>,
) -> Clip {
    let (id, name, start, length, gain_db, kind, effects) = match clip {
        gaw_core::Clip::Audio(clip) => {
            let asset_index = index.assets.get(&clip.asset_id).copied().unwrap_or(0);
            let asset = project.assets.get(asset_index);
            (
                clip.id,
                clip.name.clone(),
                clip.start.value(),
                clip.duration.value(),
                processor_gain(&clip.effects),
                ClipKind::Audio {
                    asset: asset_index,
                    sync: match clip.tempo_sync {
                        gaw_core::TempoSync::None => SyncMode::None,
                        gaw_core::TempoSync::Repitch => SyncMode::Repitch,
                        gaw_core::TempoSync::Stretch => SyncMode::Stretch,
                    },
                    source_bpm: asset
                        .and_then(|asset| asset.tempo)
                        .map(|tempo| tempo.bpm.value() as f32),
                },
                clip.effects.iter().map(effect_view).collect(),
            )
        }
        gaw_core::Clip::Event(clip) => {
            let notes = index
                .event_data
                .get(&clip.event_data_id)
                .map(|&i| {
                    project.event_data[i]
                        .events
                        .iter()
                        .enumerate()
                        .filter_map(|(event_index, event)| match event {
                            gaw_core::Event::Note(note)
                                if note.start.value() >= clip.source_start.value()
                                    && note.start.value()
                                        < clip.source_start.value() + clip.duration.value() =>
                            {
                                Some(Note {
                                    cents: note.tuning.map_or(0.0, gaw_core::Cents::value),
                                    event_index,
                                    start: (note.start.value() - clip.source_start.value()) as f32,
                                    length: note.duration.value() as f32,
                                    pitch: note.note.value(),
                                    velocity: f32::from(note.velocity.value()) / 127.0,
                                })
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            (
                clip.id,
                clip.name.clone(),
                clip.start.value(),
                clip.duration.value(),
                processor_gain(&clip.effects),
                ClipKind::Event {
                    notes: Arc::from(notes),
                },
                clip.effects.iter().map(effect_view).collect(),
            )
        }
        gaw_core::Clip::Composition(clip) => {
            let child = index
                .compositions
                .get(&clip.composition_id)
                .copied()
                .unwrap_or(0);
            (
                clip.id,
                clip.name.clone(),
                clip.start.value(),
                clip.duration.value(),
                processor_gain(&clip.effects),
                ClipKind::Composition {
                    child,
                    render: RenderState::Fresh,
                    tail_beats: 0.0,
                },
                clip.effects.iter().map(effect_view).collect(),
            )
        }
    };
    let id = id.to_string();
    let projected_waveform = match clip {
        gaw_core::Clip::Audio(audio) => index
            .assets
            .get(&audio.asset_id)
            .and_then(|&i| {
                let asset = &project.assets[i];
                asset_waveforms?
                    .get(&asset.id.to_string())
                    .map(|waveform| audio_clip_waveform(project, asset, audio, waveform))
            })
            .unwrap_or_else(|| Arc::from([])),
        gaw_core::Clip::Event(_) | gaw_core::Clip::Composition(_) => clip_waveforms
            .and_then(|cache| cache.get(&id).cloned())
            .unwrap_or_else(|| Arc::from([])),
    };
    Clip {
        waveform: projected_waveform,
        id,
        name,
        start: start as f32,
        length: length as f32,
        gain_db,
        kind,
        effects,
    }
}

#[allow(clippy::cast_sign_loss)]
pub(super) fn audio_clip_waveform(
    project: &Project,
    asset: &gaw_core::AudioAsset,
    clip: &gaw_core::AudioClip,
    waveform: &Arc<[WaveformPoint]>,
) -> Arc<[WaveformPoint]> {
    let Some(asset_seconds) = asset_duration(asset).filter(|duration| *duration > 0.0) else {
        return Arc::clone(waveform);
    };
    if waveform.is_empty() {
        return Arc::clone(waveform);
    }
    let ratio = if clip.tempo_sync == gaw_core::TempoSync::None {
        1.0
    } else {
        asset
            .tempo
            .and_then(|tempo| tempo.playback_ratio(project.bpm).ok())
            .map_or(1.0, gaw_core::PlaybackRatio::value)
    };
    let timeline_seconds = clip.duration.value() * 60.0 / project.bpm.value();
    let source_seconds = clip.source.duration.value().min(timeline_seconds * ratio);
    let start_phase = (clip.source.start.value() / asset_seconds).clamp(0.0, 1.0);
    let end_phase = ((clip.source.start.value() + source_seconds) / asset_seconds).clamp(0.0, 1.0);
    let start = (start_phase * waveform.len() as f64).floor() as usize;
    let end = ((end_phase * waveform.len() as f64).ceil() as usize)
        .max(start.saturating_add(1))
        .min(waveform.len());
    if start >= end {
        return Arc::from([]);
    }
    let mut points = waveform[start..end].to_vec();
    if clip.reverse {
        points.reverse();
    }
    let output_source_seconds = clip.source.duration.value() / ratio;
    let visible_seconds = timeline_seconds.min(output_source_seconds);
    let point_count = points.len();
    for (index, point) in points.iter_mut().enumerate() {
        let time = visible_seconds * (index as f64 + 0.5) / point_count as f64;
        let fade_in = clip
            .fade_in
            .map_or(1.0, |fade| (time / fade.duration.value()).clamp(0.0, 1.0));
        let fade_out = clip.fade_out.map_or(1.0, |fade| {
            ((output_source_seconds - time) / fade.duration.value()).clamp(0.0, 1.0)
        });
        let gain = (fade_in * fade_out) as f32;
        point.minimum *= gain;
        point.maximum *= gain;
    }
    points.into()
}

fn processor_gain(effects: &[gaw_core::Processor]) -> f32 {
    effects
        .iter()
        .find_map(|processor| match &processor.kind {
            gaw_core::ProcessorKind::Gain(parameters) => Some(parameters.gain_db),
            _ => None,
        })
        .unwrap_or(0.0)
}
