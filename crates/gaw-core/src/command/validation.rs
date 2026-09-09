//! Cross-reference, ownership, timing, and parameter invariants for snapshots.

use super::{
    AssetId, AssetRevisionId, AudioAsset, AudioAssetDefinition, AudioAssetRevision, AudioTransform,
    AutomationLane, AutomationSupport, AutomationTarget, AutomationUnit, BTreeMap, BTreeSet, Clip,
    Composition, CompositionId, Display, DomainError, Event, EventData, EventDataId, Instrument,
    InstrumentId, InstrumentKind, ParameterDescriptor, ParameterRange, ParameterUnit,
    ParameterValueType, Processor, ProcessorId, ProcessorKind, Project, SourceRange, TempoSync,
    TimeSignature, Track, TrackId, TrackKind, Validate, all_processors, already_exists, clip_end,
    composition, dangling, invalid, model_error, not_found,
};

impl Validate for Project {
    #[allow(clippy::too_many_lines)]
    fn validate(&self) -> Result<(), DomainError> {
        if self.schema_version != crate::SCHEMA_VERSION {
            return Err(invalid("schema_version", "unsupported schema version"));
        }
        nonempty("project.name", &self.name)?;
        TimeSignature::new(
            self.time_signature.numerator,
            self.time_signature.denominator,
        )
        .map_err(|error| invalid("project.time_signature", error))?;
        if !(-120.0..=24.0).contains(&self.settings.master_volume.value()) {
            return Err(invalid(
                "project.settings.master_volume",
                "must be finite and between -120 dB and +24 dB",
            ));
        }
        unique(self.assets.iter().map(|value| value.id), "asset")?;
        unique(
            self.asset_folders.iter().map(|value| value.id),
            "asset folder",
        )?;
        unique(self.event_data.iter().map(|value| value.id), "event data")?;
        unique(
            self.compositions.iter().map(|value| value.id),
            "composition",
        )?;
        unique(self.tracks.iter().map(|value| value.id), "track")?;
        unique(
            self.automation.iter().map(|value| value.id),
            "automation lane",
        )?;
        unique(
            self.tracks
                .iter()
                .flat_map(|value| &value.clips)
                .map(Clip::id),
            "clip",
        )?;
        unique(
            all_processors(self).map(|value| value.id.as_str().to_owned()),
            "processor",
        )?;

        composition(self, self.root_composition_id)?;
        let assets: BTreeMap<_, _> = self.assets.iter().map(|value| (value.id, value)).collect();
        let events: BTreeMap<_, _> = self
            .event_data
            .iter()
            .map(|value| (value.id, value))
            .collect();
        let compositions: BTreeMap<_, _> = self
            .compositions
            .iter()
            .map(|value| (value.id, value))
            .collect();
        let tracks: BTreeMap<_, _> = self.tracks.iter().map(|value| (value.id, value)).collect();
        let mut graph = BTreeMap::<String, Vec<String>>::new();

        let mut folder_assets = BTreeSet::new();
        let mut folder_events = BTreeSet::new();
        for folder in &self.asset_folders {
            nonempty("asset_folder.name", &folder.name)?;
            for id in &folder.asset_ids {
                if !assets.contains_key(id) {
                    return Err(dangling(folder.id, id));
                }
                if !folder_assets.insert(*id) {
                    return Err(invalid(
                        "asset_folder.asset_ids",
                        format!("asset {id} belongs to more than one folder"),
                    ));
                }
            }
            for id in &folder.event_data_ids {
                if !events.contains_key(id) {
                    return Err(dangling(folder.id, id));
                }
                if !folder_events.insert(*id) {
                    return Err(invalid(
                        "asset_folder.event_data_ids",
                        format!("event data {id} belongs to more than one folder"),
                    ));
                }
            }
        }

        let mut revisions = BTreeMap::new();
        for asset in &self.assets {
            asset.validate().map_err(model_error)?;
            for revision in &asset.revisions {
                if revisions.insert(revision.id, revision).is_some() {
                    return Err(already_exists("asset revision", revision.id));
                }
            }
        }

        let mut track_owners = BTreeMap::new();
        let mut track_group_ids = BTreeSet::new();
        let mut grouped_tracks = BTreeSet::new();
        for composition in &self.compositions {
            nonempty("composition.name", &composition.name)?;
            unique(composition.track_ids.iter().copied(), "track reference")?;
            validate_processors(&composition.output_effects)?;
            let mut previous_gap_end = 0.0;
            for gap in &composition.bar_timeline_gaps {
                let start = gap.start.value();
                let duration = gap.duration.value();
                let end = start + duration;
                if duration <= 0.0 {
                    return Err(invalid(
                        "composition.bar_timeline_gaps.duration",
                        "must be greater than zero",
                    ));
                }
                if start < previous_gap_end {
                    return Err(invalid(
                        "composition.bar_timeline_gaps",
                        "must be ordered and non-overlapping",
                    ));
                }
                if end > composition.length.value() {
                    return Err(invalid(
                        "composition.bar_timeline_gaps",
                        "must fit within the composition",
                    ));
                }
                previous_gap_end = end;
            }
            graph.entry(composition_node(composition.id)).or_default();
            for group in &composition.track_groups {
                nonempty("track_group.name", &group.name)?;
                if !track_group_ids.insert(group.id) {
                    return Err(already_exists("track group", group.id));
                }
                for id in &group.track_ids {
                    let owned = tracks.get(id).ok_or_else(|| dangling(group.id, id))?;
                    if owned.composition_id != composition.id || !composition.track_ids.contains(id)
                    {
                        return Err(DomainError::CrossBoundary {
                            from: group.id.to_string(),
                            to: id.to_string(),
                        });
                    }
                    if !grouped_tracks.insert(*id) {
                        return Err(invalid(
                            "track_group.track_ids",
                            format!("track {id} belongs to more than one group"),
                        ));
                    }
                }
            }
            for id in &composition.track_ids {
                let owned = tracks.get(id).ok_or_else(|| dangling(composition.id, id))?;
                if owned.composition_id != composition.id {
                    return Err(DomainError::CrossBoundary {
                        from: composition.id.to_string(),
                        to: id.to_string(),
                    });
                }
                if let Some(other) = track_owners.insert(*id, composition.id) {
                    return Err(invalid(
                        "composition.track_ids",
                        format!("track {id} is owned by {other} and {}", composition.id),
                    ));
                }
            }
        }

        let mut instrument_owners = BTreeMap::new();
        let mut zone_ids = BTreeSet::new();
        let mut child_parents = BTreeMap::new();
        for track in &self.tracks {
            nonempty("track.name", &track.name)?;
            if !track.volume_db.is_finite() || !(-120.0..=24.0).contains(&track.volume_db) {
                return Err(invalid(
                    "track.volume_db",
                    "must be finite and between -120 dB and +24 dB",
                ));
            }
            let owner = compositions
                .get(&track.composition_id)
                .ok_or_else(|| dangling(track.id, track.composition_id))?;
            if track_owners.get(&track.id) != Some(&track.composition_id) {
                return Err(dangling(
                    track.id,
                    format!("composition {} track_ids", owner.id),
                ));
            }
            match track.kind {
                TrackKind::Audio
                    if track.instrument.is_some()
                        || track
                            .clips
                            .iter()
                            .any(|clip| matches!(clip, Clip::Event(_))) =>
                {
                    return Err(invalid(
                        "track.kind",
                        "audio tracks accept only audio/composition clips and no instrument",
                    ));
                }
                TrackKind::Event
                    if track.instrument.is_none()
                        || track
                            .clips
                            .iter()
                            .any(|clip| !matches!(clip, Clip::Event(_))) =>
                {
                    return Err(invalid(
                        "track.kind",
                        "event tracks require an instrument and only event clips",
                    ));
                }
                _ => {}
            }
            validate_processors(&track.effects)?;
            if let Some(instrument) = &track.instrument {
                nonempty("instrument.name", &instrument.name)?;
                if instrument_owners
                    .insert(instrument.id, track.composition_id)
                    .is_some()
                {
                    return Err(already_exists("instrument", instrument.id));
                }
                let InstrumentKind::Sampler(sampler) = &instrument.kind;
                sampler.validate().map_err(model_error)?;
                for zone in &sampler.zones {
                    if !zone_ids.insert(zone.id) {
                        return Err(already_exists("sampler zone", zone.id));
                    }
                    let asset = assets
                        .get(&zone.asset_id)
                        .copied()
                        .ok_or_else(|| dangling(zone.id, zone.asset_id))?;
                    validate_source_range(asset, zone.source, "sampler_zone.source")?;
                    add_edge(
                        &mut graph,
                        composition_node(track.composition_id),
                        asset_node(zone.asset_id),
                    );
                }
            }
            for clip in &track.clips {
                validate_clip(
                    clip,
                    track,
                    owner,
                    &assets,
                    &events,
                    &compositions,
                    &mut graph,
                    &mut child_parents,
                )?;
            }
            let mut clip_ranges = track
                .clips
                .iter()
                .map(|clip| (clip.start().value(), clip_end(clip)))
                .collect::<Vec<_>>();
            clip_ranges.sort_by(|left, right| left.0.total_cmp(&right.0));
            for pair in clip_ranges.windows(2) {
                if pair[1].0 < pair[0].1 {
                    return Err(invalid(
                        "track.clips",
                        "clips on one track must not overlap",
                    ));
                }
            }
        }

        validate_events(&self.event_data)?;
        validate_assets(
            self,
            &assets,
            &events,
            &compositions,
            &instrument_owners,
            &mut graph,
        )?;
        validate_revisions(&revisions, &mut graph)?;
        validate_automation(self, &compositions, &tracks)?;
        detect_cycles(&graph)?;
        if child_parents.contains_key(&self.root_composition_id) {
            return Err(invalid(
                "root_composition_id",
                "root composition cannot be a child",
            ));
        }
        for composition in &self.compositions {
            if composition.id != self.root_composition_id
                && !child_parents.contains_key(&composition.id)
            {
                return Err(invalid(
                    "composition hierarchy",
                    format!("composition {} has no parent", composition.id),
                ));
            }
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_clip(
    clip: &Clip,
    track: &Track,
    owner: &Composition,
    assets: &BTreeMap<AssetId, &AudioAsset>,
    events: &BTreeMap<EventDataId, &EventData>,
    compositions: &BTreeMap<CompositionId, &Composition>,
    graph: &mut BTreeMap<String, Vec<String>>,
    child_parents: &mut BTreeMap<CompositionId, CompositionId>,
) -> Result<(), DomainError> {
    let (start, duration) = match clip {
        Clip::Audio(v) => (v.start, v.duration),
        Clip::Event(v) => (v.start, v.duration),
        Clip::Composition(v) => (v.start, v.duration),
    };
    if duration.value() <= 0.0 || start.value() + duration.value() > owner.length.value() {
        return Err(invalid(
            "clip.duration",
            "must be positive and fit within its composition",
        ));
    }
    match clip {
        Clip::Audio(value) => {
            let asset = assets
                .get(&value.asset_id)
                .copied()
                .ok_or_else(|| dangling(value.id, value.asset_id))?;
            validate_source_range(asset, value.source, "audio_clip.source")?;
            if value.tempo_sync != TempoSync::None && asset.tempo.is_none() {
                return Err(invalid(
                    "audio_clip.tempo_sync",
                    "repitch and stretch require an asset BPM",
                ));
            }
            let fades = value.fade_in.map_or(0.0, |v| v.duration.value())
                + value.fade_out.map_or(0.0, |v| v.duration.value());
            if fades > value.source.duration.value() {
                return Err(invalid(
                    "audio_clip.fades",
                    "combined fades exceed source duration",
                ));
            }
            validate_processors(&value.effects)?;
            add_edge(
                graph,
                composition_node(track.composition_id),
                asset_node(value.asset_id),
            );
        }
        Clip::Event(value) => {
            if !events.contains_key(&value.event_data_id) {
                return Err(dangling(value.id, value.event_data_id));
            }
        }
        Clip::Composition(value) => {
            let child = compositions
                .get(&value.composition_id)
                .ok_or_else(|| dangling(value.id, value.composition_id))?;
            if value.source_start.value() + value.duration.value() > child.length.value() {
                return Err(invalid(
                    "composition_clip",
                    "source range exceeds child length",
                ));
            }
            if let Some(other) = child_parents.insert(value.composition_id, track.composition_id)
                && other != track.composition_id
            {
                return Err(invalid(
                    "composition hierarchy",
                    format!("composition {} has multiple parents", value.composition_id),
                ));
            }
            validate_processors(&value.effects)?;
            add_edge(
                graph,
                composition_node(track.composition_id),
                composition_node(value.composition_id),
            );
        }
    }
    Ok(())
}

fn validate_events(values: &[EventData]) -> Result<(), DomainError> {
    for data in values {
        nonempty("event_data.name", &data.name)?;
        let mut previous = 0.0;
        for (index, event) in data.events.iter().enumerate() {
            if index != 0 && event.time().value() < previous {
                return Err(invalid("event_data.events", "must be ordered by time"));
            }
            previous = event.time().value();
            match event {
                Event::Note(note) if note.duration.value() <= 0.0 => {
                    return Err(invalid("note.duration", "must be positive"));
                }
                Event::Control(control) => nonempty("control.controller", &control.controller)?,
                Event::Note(_) | Event::PitchBend(_) => {}
            }
        }
    }
    Ok(())
}

fn validate_assets(
    project: &Project,
    assets: &BTreeMap<AssetId, &AudioAsset>,
    events: &BTreeMap<EventDataId, &EventData>,
    compositions: &BTreeMap<CompositionId, &Composition>,
    instruments: &BTreeMap<InstrumentId, CompositionId>,
    graph: &mut BTreeMap<String, Vec<String>>,
) -> Result<(), DomainError> {
    for asset in &project.assets {
        let from = asset_node(asset.id);
        graph.entry(from.clone()).or_default();
        match &asset.definition {
            AudioAssetDefinition::Imported(value) => {
                nonempty("asset.original_filename", &value.original_filename)?;
                nonempty("asset.content_hash", value.content_hash.as_str())?;
                if value.frames.0 == 0 {
                    return Err(invalid("asset.frames", "imported audio must not be empty"));
                }
            }
            AudioAssetDefinition::InstrumentGenerated {
                instrument_id,
                event_data_id,
            } => {
                let owner = instruments
                    .get(instrument_id)
                    .ok_or_else(|| dangling(asset.id, instrument_id))?;
                if !events.contains_key(event_data_id) {
                    return Err(dangling(asset.id, event_data_id));
                }
                add_edge(graph, from, composition_node(*owner));
            }
            AudioAssetDefinition::CompositionGenerated { composition_id } => {
                if !compositions.contains_key(composition_id) {
                    return Err(dangling(asset.id, composition_id));
                }
                add_edge(graph, from, composition_node(*composition_id));
            }
            AudioAssetDefinition::Processed {
                source_asset_id,
                transforms,
                effects,
            } => {
                let source = assets
                    .get(source_asset_id)
                    .copied()
                    .ok_or_else(|| dangling(asset.id, source_asset_id))?;
                for transform in transforms {
                    if let AudioTransform::Trim(range) = transform {
                        validate_source_range(source, *range, "audio_transform.trim")?;
                    }
                }
                validate_processors(effects)?;
                add_edge(graph, from, asset_node(*source_asset_id));
            }
            AudioAssetDefinition::Materialized { revision_id } => {
                if !asset
                    .revisions
                    .iter()
                    .any(|revision| revision.id == *revision_id)
                {
                    return Err(dangling(asset.id, revision_id));
                }
            }
        }
        if let Some(tempo) = asset.tempo
            && let Some(duration) = asset_duration_seconds(asset)
            && tempo.first_beat.value() > duration
        {
            return Err(invalid(
                "asset.tempo.first_beat",
                "must fall within the asset duration",
            ));
        }
    }
    Ok(())
}

fn validate_source_range(
    asset: &AudioAsset,
    range: SourceRange,
    field: &'static str,
) -> Result<(), DomainError> {
    if range.duration.value() <= 0.0 {
        return Err(invalid(field, "duration must be positive"));
    }
    if let Some(duration) = asset_duration_seconds(asset)
        && range.start.value() + range.duration.value() > duration
    {
        return Err(invalid(field, "range exceeds the asset duration"));
    }
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn asset_duration_seconds(asset: &AudioAsset) -> Option<f64> {
    match &asset.definition {
        AudioAssetDefinition::Imported(imported) => {
            Some(imported.frames.0 as f64 / f64::from(imported.sample_rate.value()))
        }
        _ => asset.current_revision().map(|revision| {
            revision.frames.0 as f64 / f64::from(revision.render_context.sample_rate.value())
        }),
    }
}

fn validate_revisions(
    revisions: &BTreeMap<AssetRevisionId, &AudioAssetRevision>,
    graph: &mut BTreeMap<String, Vec<String>>,
) -> Result<(), DomainError> {
    for (id, revision) in revisions {
        nonempty("revision.content_hash", revision.content_hash.as_str())?;
        nonempty(
            "revision.definition_hash",
            revision.definition_hash.as_str(),
        )?;
        nonempty(
            "revision.engine_version",
            &revision.render_context.engine_version,
        )?;
        let from = revision_node(*id);
        graph.entry(from.clone()).or_default();
        for dependency in &revision.dependency_revision_ids {
            if !revisions.contains_key(dependency) {
                return Err(dangling(id, dependency));
            }
            add_edge(graph, from.clone(), revision_node(*dependency));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_automation(
    project: &Project,
    compositions: &BTreeMap<CompositionId, &Composition>,
    tracks: &BTreeMap<TrackId, &Track>,
) -> Result<(), DomainError> {
    for lane in &project.automation {
        lane.validate().map_err(model_error)?;
        nonempty("automation.name", &lane.name)?;
        let composition = compositions
            .get(&lane.composition_id)
            .ok_or_else(|| dangling(lane.id, lane.composition_id))?;
        if lane
            .points
            .iter()
            .any(|point| point.time.value() > composition.length.value())
        {
            return Err(invalid(
                "automation.points",
                "point lies past composition length",
            ));
        }
        if let Some(first) = lane.points.first()
            && lane
                .points
                .iter()
                .any(|point| point.value.unit() != first.value.unit())
        {
            return Err(invalid(
                "automation.points",
                "all values in a lane must have the same unit",
            ));
        }
        let processor_and_parameter = match &lane.target {
            AutomationTarget::AudioClipProcessor {
                track_id,
                clip_id,
                processor_id,
                parameter_id,
            } => {
                let track = automation_track(tracks, lane, *track_id)?;
                let clip = track
                    .clips
                    .iter()
                    .find(|v| v.id() == *clip_id)
                    .ok_or_else(|| dangling(lane.id, clip_id))?;
                let Clip::Audio(clip) = clip else {
                    return Err(invalid("automation.target", "target is not an audio clip"));
                };
                Some((processor(&clip.effects, processor_id)?, parameter_id))
            }
            AutomationTarget::CompositionClipProcessor {
                track_id,
                clip_id,
                processor_id,
                parameter_id,
            } => {
                let track = automation_track(tracks, lane, *track_id)?;
                let clip = track
                    .clips
                    .iter()
                    .find(|v| v.id() == *clip_id)
                    .ok_or_else(|| dangling(lane.id, clip_id))?;
                let Clip::Composition(clip) = clip else {
                    return Err(invalid(
                        "automation.target",
                        "target is not a composition clip",
                    ));
                };
                Some((processor(&clip.effects, processor_id)?, parameter_id))
            }
            AutomationTarget::TrackProcessor {
                track_id,
                processor_id,
                parameter_id,
            } => {
                let track = automation_track(tracks, lane, *track_id)?;
                Some((processor(&track.effects, processor_id)?, parameter_id))
            }
            AutomationTarget::CompositionOutputProcessor {
                processor_id,
                parameter_id,
            } => Some((
                processor(&composition.output_effects, processor_id)?,
                parameter_id,
            )),
            AutomationTarget::Instrument {
                track_id,
                instrument_id,
                parameter_id,
            } => {
                let track = automation_track(tracks, lane, *track_id)?;
                let instrument = track
                    .instrument
                    .as_ref()
                    .ok_or_else(|| dangling(lane.id, instrument_id))?;
                if instrument.id != *instrument_id {
                    return Err(dangling(lane.id, instrument_id));
                }
                validate_instrument_automation(instrument, parameter_id, lane)?;
                None
            }
        };
        if let Some((processor, parameter)) = processor_and_parameter {
            validate_automation_parameter(processor, parameter, lane)?;
        }
    }
    Ok(())
}

fn automation_track<'a>(
    tracks: &'a BTreeMap<TrackId, &Track>,
    lane: &AutomationLane,
    id: TrackId,
) -> Result<&'a Track, DomainError> {
    let track = tracks
        .get(&id)
        .copied()
        .ok_or_else(|| dangling(lane.id, id))?;
    if track.composition_id != lane.composition_id {
        return Err(DomainError::CrossBoundary {
            from: lane.composition_id.to_string(),
            to: id.to_string(),
        });
    }
    Ok(track)
}

fn validate_instrument_automation(
    instrument: &Instrument,
    parameter: &str,
    lane: &AutomationLane,
) -> Result<(), DomainError> {
    let InstrumentKind::Sampler(sampler) = &instrument.kind;
    let unit = if parameter == "output_gain_db" {
        AutomationUnit::Decibels
    } else {
        let mut path = parameter.split('.');
        let (Some("zones"), Some(zone_id), Some(name), None) =
            (path.next(), path.next(), path.next(), path.next())
        else {
            return Err(invalid(
                "automation.parameter_id",
                "unknown or discrete sampler parameter",
            ));
        };
        if !sampler
            .zones
            .iter()
            .any(|zone| zone.id.to_string() == zone_id)
        {
            return Err(invalid(
                "automation.parameter_id",
                format!("sampler zone {zone_id} does not exist"),
            ));
        }
        match name {
            "gain_db" => AutomationUnit::Decibels,
            "velocity_sensitivity" => AutomationUnit::Ratio,
            "attack_ms" | "release_ms" => AutomationUnit::Milliseconds,
            _ => {
                return Err(invalid(
                    "automation.parameter_id",
                    "unknown or discrete sampler zone parameter",
                ));
            }
        }
    };
    if lane.points.iter().all(|point| point.value.unit() == unit) {
        Ok(())
    } else {
        Err(invalid(
            "automation.points",
            format!("values for {parameter:?} must use {unit:?}"),
        ))
    }
}

fn processor<'a>(values: &'a [Processor], id: &ProcessorId) -> Result<&'a Processor, DomainError> {
    values
        .iter()
        .find(|value| value.id == *id)
        .ok_or_else(|| not_found("processor", id))
}

fn parameter_descriptor<'a>(kind: &'a ProcessorKind, id: &str) -> Option<&'a ParameterDescriptor> {
    let normalized = normalize_parameter_id(id);
    kind.parameter_descriptors()
        .iter()
        .find(|descriptor| descriptor.id == normalized)
}

fn validate_automation_parameter(
    processor: &Processor,
    id: &str,
    lane: &AutomationLane,
) -> Result<(), DomainError> {
    let descriptor = parameter_descriptor(&processor.kind, id).ok_or_else(|| {
        invalid(
            "automation.parameter_id",
            format!("{id:?} is not a parameter of {}", processor.kind.type_id()),
        )
    })?;
    if descriptor.automation != AutomationSupport::Continuous {
        return Err(invalid(
            "automation.parameter_id",
            format!("{id:?} is discrete or not automatable"),
        ));
    }
    validate_parameter_index(&processor.kind, id)?;
    for point in &lane.points {
        let unit = point.value.unit();
        let compatible = match descriptor.value_type {
            ParameterValueType::Time => {
                matches!(unit, AutomationUnit::Beats | AutomationUnit::Seconds)
            }
            ParameterValueType::Rate => {
                matches!(unit, AutomationUnit::Beats | AutomationUnit::Hertz)
            }
            _ => automation_unit(descriptor.unit) == Some(unit),
        };
        if !compatible {
            return Err(invalid(
                "automation.points",
                format!("unit {unit:?} is incompatible with parameter {id:?}"),
            ));
        }
        if let Some(range) = automation_parameter_range(&processor.kind, descriptor, unit)
            && !(range.minimum..=range.maximum).contains(&point.value.number())
        {
            return Err(invalid(
                "automation.points",
                format!("value for {id:?} is outside its valid range"),
            ));
        }
    }
    Ok(())
}

fn automation_parameter_range(
    kind: &ProcessorKind,
    descriptor: &ParameterDescriptor,
    unit: AutomationUnit,
) -> Option<ParameterRange> {
    match (descriptor.value_type, unit) {
        (ParameterValueType::Time, AutomationUnit::Beats | AutomationUnit::Seconds) => {
            Some(ParameterRange {
                minimum: if matches!(kind, ProcessorKind::Delay(_)) && descriptor.id == "time" {
                    f64::EPSILON
                } else {
                    0.0
                },
                maximum: 64.0,
            })
        }
        (ParameterValueType::Rate, AutomationUnit::Hertz) => Some(ParameterRange {
            minimum: 0.01,
            maximum: 40.0,
        }),
        (ParameterValueType::Rate, AutomationUnit::Beats) => Some(ParameterRange {
            minimum: 1.0 / 64.0,
            maximum: 64.0,
        }),
        _ => descriptor.range,
    }
}

fn automation_unit(unit: ParameterUnit) -> Option<AutomationUnit> {
    match unit {
        ParameterUnit::Unitless | ParameterUnit::Ratio => Some(AutomationUnit::Number),
        ParameterUnit::Decibels | ParameterUnit::Lufs => Some(AutomationUnit::Decibels),
        ParameterUnit::Hertz => Some(AutomationUnit::Hertz),
        ParameterUnit::Milliseconds => Some(AutomationUnit::Milliseconds),
        ParameterUnit::Seconds => Some(AutomationUnit::Seconds),
        ParameterUnit::Beats => Some(AutomationUnit::Beats),
        ParameterUnit::Normalized | ParameterUnit::PhaseCycles => Some(AutomationUnit::Ratio),
        ParameterUnit::Bipolar => Some(AutomationUnit::Bipolar),
        ParameterUnit::Semitones => Some(AutomationUnit::Semitones),
        ParameterUnit::Cents => Some(AutomationUnit::Cents),
        ParameterUnit::Bits | ParameterUnit::Count => None,
    }
}

fn validate_parameter_index(kind: &ProcessorKind, id: &str) -> Result<(), DomainError> {
    let mut segments = id.split('.');
    let (Some(collection), Some(index)) = (segments.next(), segments.next()) else {
        return Ok(());
    };
    let Ok(index) = index.parse::<usize>() else {
        return Ok(());
    };
    let length = match (kind, collection) {
        (ProcessorKind::ParametricEq(parameters), "bands") => parameters.bands.len(),
        (ProcessorKind::RhythmicGate(parameters), "steps") => parameters.steps.len(),
        _ => return Ok(()),
    };
    if index < length {
        Ok(())
    } else {
        Err(invalid(
            "automation.parameter_id",
            format!("index {index} is outside {collection} length {length}"),
        ))
    }
}

fn normalize_parameter_id(id: &str) -> String {
    let parts: Vec<_> = id.split('.').collect();
    if let [head @ ("bands" | "steps"), index, rest @ ..] = parts.as_slice()
        && index.parse::<usize>().is_ok()
        && !rest.is_empty()
    {
        format!("{head}[].{}", rest.join("."))
    } else {
        id.to_owned()
    }
}

fn validate_processors(values: &[Processor]) -> Result<(), DomainError> {
    unique(
        values.iter().map(|value| value.id.as_str().to_owned()),
        "processor in stack",
    )?;
    for value in values {
        value
            .validate()
            .map_err(|error| invalid("processor", error))?;
    }
    Ok(())
}

fn unique<T: Ord + Display>(
    values: impl IntoIterator<Item = T>,
    entity: &'static str,
) -> Result<(), DomainError> {
    let mut seen = BTreeSet::new();
    for value in values {
        let display = value.to_string();
        if !seen.insert(value) {
            return Err(already_exists(entity, display));
        }
    }
    Ok(())
}

fn nonempty(field: &'static str, value: &str) -> Result<(), DomainError> {
    if value.trim().is_empty() {
        Err(invalid(field, "must not be empty"))
    } else {
        Ok(())
    }
}

fn asset_node(id: AssetId) -> String {
    format!("asset:{id}")
}
fn composition_node(id: CompositionId) -> String {
    format!("composition:{id}")
}
fn revision_node(id: AssetRevisionId) -> String {
    format!("revision:{id}")
}

fn add_edge(graph: &mut BTreeMap<String, Vec<String>>, from: String, to: String) {
    graph.entry(from).or_default().push(to);
}

fn detect_cycles(graph: &BTreeMap<String, Vec<String>>) -> Result<(), DomainError> {
    fn visit(
        node: &str,
        graph: &BTreeMap<String, Vec<String>>,
        active: &mut Vec<String>,
        done: &mut BTreeSet<String>,
    ) -> Result<(), DomainError> {
        if let Some(index) = active.iter().position(|value| value == node) {
            let mut cycle = active[index..].to_vec();
            cycle.push(node.to_owned());
            return Err(DomainError::DependencyCycle {
                path: cycle.join(" -> "),
            });
        }
        if done.contains(node) {
            return Ok(());
        }
        active.push(node.to_owned());
        if let Some(next) = graph.get(node) {
            for dependency in next {
                visit(dependency, graph, active, done)?;
            }
        }
        active.pop();
        done.insert(node.to_owned());
        Ok(())
    }

    let mut active = Vec::new();
    let mut done = BTreeSet::new();
    for node in graph.keys() {
        visit(node, graph, &mut active, &mut done)?;
    }
    Ok(())
}
