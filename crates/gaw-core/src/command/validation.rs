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
            all_processors(self).map(|value| value.id.as_str()),
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
        let mut graph = DependencyGraph::new();

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
            graph
                .entry(DependencyNode::Composition(composition.id))
                .or_default();
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
                        DependencyNode::Composition(track.composition_id),
                        DependencyNode::Asset(zone.asset_id),
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
    graph: &mut DependencyGraph,
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
                DependencyNode::Composition(track.composition_id),
                DependencyNode::Asset(value.asset_id),
            );
        }
        Clip::Event(value) => {
            validate_processors(&value.effects)?;
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
                DependencyNode::Composition(track.composition_id),
                DependencyNode::Composition(value.composition_id),
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
    graph: &mut DependencyGraph,
) -> Result<(), DomainError> {
    for asset in &project.assets {
        let from = DependencyNode::Asset(asset.id);
        graph.entry(from).or_default();
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
                add_edge(graph, from, DependencyNode::Composition(*owner));
            }
            AudioAssetDefinition::CompositionGenerated { composition_id } => {
                if !compositions.contains_key(composition_id) {
                    return Err(dangling(asset.id, composition_id));
                }
                add_edge(graph, from, DependencyNode::Composition(*composition_id));
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
                add_edge(graph, from, DependencyNode::Asset(*source_asset_id));
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
    graph: &mut DependencyGraph,
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
        let from = DependencyNode::Revision(*id);
        graph.entry(from).or_default();
        for dependency in &revision.dependency_revision_ids {
            if !revisions.contains_key(dependency) {
                return Err(dangling(id, dependency));
            }
            add_edge(graph, from, DependencyNode::Revision(*dependency));
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
        // Lane validation has already established nonempty, strictly ordered points.
        if lane
            .points
            .last()
            .is_some_and(|point| point.time.value() > composition.length.value())
        {
            return Err(invalid(
                "automation.points",
                "point lies past composition length",
            ));
        }
        let unit = lane.points[0].value.unit();
        if lane.points.iter().any(|point| point.value.unit() != unit) {
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
                let effects = match clip {
                    Clip::Audio(clip) => &clip.effects,
                    Clip::Event(clip) => &clip.effects,
                    Clip::Composition(_) => {
                        return Err(invalid(
                            "automation.target",
                            "target is not an audio or event clip",
                        ));
                    }
                };
                Some((processor(effects, processor_id)?, parameter_id))
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
                validate_instrument_automation(instrument, parameter_id, unit)?;
                None
            }
        };
        if let Some((processor, parameter)) = processor_and_parameter {
            validate_automation_parameter(processor, parameter, lane, unit)?;
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
    lane_unit: AutomationUnit,
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
    if lane_unit == unit {
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
    unit: AutomationUnit,
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
        && lane
            .points
            .iter()
            .any(|point| !(range.minimum..=range.maximum).contains(&point.value.number()))
    {
        return Err(invalid(
            "automation.points",
            format!("value for {id:?} is outside its valid range"),
        ));
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

fn normalize_parameter_id(id: &str) -> std::borrow::Cow<'_, str> {
    let mut parts = id.splitn(3, '.');
    if let (Some(head @ ("bands" | "steps")), Some(index), Some(rest)) =
        (parts.next(), parts.next(), parts.next())
        && index.parse::<usize>().is_ok()
    {
        format!("{head}[].{rest}").into()
    } else {
        id.into()
    }
}

fn validate_processors(values: &[Processor]) -> Result<(), DomainError> {
    unique(
        values.iter().map(|value| value.id.as_str()),
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
        if let Some(duplicate) = seen.replace(value) {
            return Err(already_exists(entity, duplicate));
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

// Variant order matches the lexical order of the former string keys. UUID order
// also matches their fixed-width display, preserving which cycle is reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum DependencyNode {
    Asset(AssetId),
    Composition(CompositionId),
    Revision(AssetRevisionId),
}

impl Display for DependencyNode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Asset(id) => write!(formatter, "asset:{id}"),
            Self::Composition(id) => write!(formatter, "composition:{id}"),
            Self::Revision(id) => write!(formatter, "revision:{id}"),
        }
    }
}

type DependencyGraph = BTreeMap<DependencyNode, Vec<DependencyNode>>;

fn add_edge(graph: &mut DependencyGraph, from: DependencyNode, to: DependencyNode) {
    graph.entry(from).or_default().push(to);
}

fn detect_cycles(graph: &DependencyGraph) -> Result<(), DomainError> {
    #[derive(Clone, Copy)]
    enum VisitState {
        Visiting(usize),
        Done,
    }

    fn visit(
        node: DependencyNode,
        graph: &DependencyGraph,
        active: &mut Vec<DependencyNode>,
        states: &mut BTreeMap<DependencyNode, VisitState>,
    ) -> Result<(), DomainError> {
        match states.get(&node).copied() {
            Some(VisitState::Visiting(index)) => {
                let cycle: Vec<_> = active[index..]
                    .iter()
                    .chain(std::iter::once(&node))
                    .map(ToString::to_string)
                    .collect();
                return Err(DomainError::DependencyCycle {
                    path: cycle.join(" -> "),
                });
            }
            Some(VisitState::Done) => return Ok(()),
            None => {}
        }
        states.insert(node, VisitState::Visiting(active.len()));
        active.push(node);
        if let Some(next) = graph.get(&node) {
            for &dependency in next {
                visit(dependency, graph, active, states)?;
            }
        }
        active.pop();
        states.insert(node, VisitState::Done);
        Ok(())
    }

    let mut active = Vec::new();
    let mut states = BTreeMap::new();
    for &node in graph.keys() {
        visit(node, graph, &mut active, &mut states)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::too_many_lines)]
    fn automation_validation_matches_legacy_for_mixed_validity_lanes() {
        let mut project = Project::new(
            "Automation",
            crate::Bpm::new(120.0).unwrap(),
            crate::SampleRate::new(48_000).unwrap(),
        );
        project.compositions[0].length = crate::Beats::new(4.0).unwrap();
        let mut targets = Vec::new();
        for (index, kind) in [
            ProcessorKind::Gain(crate::GainParameters::default()),
            ProcessorKind::Delay(crate::DelayParameters::default()),
            ProcessorKind::TremoloAutopan(crate::TremoloAutopanParameters::default()),
            ProcessorKind::ParametricEq(crate::ParametricEqParameters::default()),
            ProcessorKind::RhythmicGate(crate::RhythmicGateParameters::default()),
        ]
        .into_iter()
        .enumerate()
        {
            let processor = Processor::new(
                ProcessorId::new(format!("processor-{index}")).unwrap(),
                kind,
            );
            for parameter_id in processor
                .kind
                .parameter_descriptors()
                .iter()
                .map(|descriptor| descriptor.id)
                .chain([
                    "missing",
                    "bands.0.gain_db",
                    "bands.999.gain_db",
                    "steps.999.level",
                ])
            {
                targets.push(AutomationTarget::CompositionOutputProcessor {
                    processor_id: processor.id.clone(),
                    parameter_id: parameter_id.into(),
                });
            }
            project.compositions[0].output_effects.push(processor);
        }
        let instrument = Instrument::sampler("Sampler", crate::Sampler::new(8).unwrap());
        let instrument_id = instrument.id;
        let track = Track::event(project.root_composition_id, "Notes", instrument);
        for parameter_id in ["output_gain_db", "zones.missing.gain_db", "missing"] {
            targets.push(AutomationTarget::Instrument {
                track_id: track.id,
                instrument_id,
                parameter_id: parameter_id.into(),
            });
        }
        project.compositions[0].track_ids.push(track.id);
        project.tracks.push(track);
        let values: Vec<_> = [
            AutomationUnit::Number,
            AutomationUnit::Decibels,
            AutomationUnit::Hertz,
            AutomationUnit::Seconds,
            AutomationUnit::Milliseconds,
            AutomationUnit::Beats,
            AutomationUnit::Ratio,
            AutomationUnit::Bipolar,
            AutomationUnit::Semitones,
            AutomationUnit::Cents,
        ]
        .into_iter()
        .flat_map(|unit| {
            [0.0, f64::EPSILON, 0.5, 65.0]
                .into_iter()
                .filter_map(move |value| crate::AutomationValue::from_unit(unit, value).ok())
        })
        .collect();
        let mut lane = AutomationLane {
            id: crate::AutomationLaneId::new(),
            composition_id: project.root_composition_id,
            name: "Lane".into(),
            target: targets[0].clone(),
            points: vec![],
        };
        for target in targets {
            lane.target = target;
            for (first_time, last_time) in [(0.0, 1.0), (1.0, 0.0), (1.0, 1.0), (0.0, 5.0)] {
                for (index, &value) in values.iter().enumerate() {
                    for second in [value, values[(index + 5) % values.len()]] {
                        lane.points = [(first_time, value), (last_time, second)]
                            .into_iter()
                            .map(|(time, value)| crate::AutomationPoint {
                                time: crate::Beats::new(time).unwrap(),
                                value,
                                curve: crate::AutomationCurve::Linear,
                            })
                            .collect();
                        project.automation = vec![lane.clone()];
                        assert_automation_matches_legacy(&project);
                    }
                }
            }
            for bad_field in 0..3 {
                let mut invalid = lane.clone();
                match bad_field {
                    0 => invalid.points.clear(),
                    1 => invalid.name.clear(),
                    _ => invalid.composition_id = CompositionId::new(),
                }
                project.automation = vec![invalid];
                assert_automation_matches_legacy(&project);
            }
        }
    }

    fn assert_automation_matches_legacy(project: &Project) {
        let compositions = project
            .compositions
            .iter()
            .map(|value| (value.id, value))
            .collect();
        let tracks = project
            .tracks
            .iter()
            .map(|value| (value.id, value))
            .collect();
        assert_eq!(
            validate_automation(project, &compositions, &tracks),
            legacy_automation::validate_automation(project, &compositions, &tracks),
            "{:?}",
            project.automation
        );
    }

    #[test]
    fn parameter_normalization_preserves_empty_segments_and_numeric_syntax() {
        for head in ["bands", "steps", "zones", "", "gain_db"] {
            for index in [
                "0",
                "+0",
                "01",
                "-1",
                "[]",
                "",
                "99999999999999999999999999",
            ] {
                for tail in ["", "gain_db", ".gain_db", "gain_db.", "one.two"] {
                    for id in [
                        head.to_owned(),
                        format!("{head}.{index}"),
                        format!("{head}.{index}.{tail}"),
                    ] {
                        let parts: Vec<_> = id.split('.').collect();
                        let expected = if let [head @ ("bands" | "steps"), index, rest @ ..] =
                            parts.as_slice()
                            && index.parse::<usize>().is_ok()
                            && !rest.is_empty()
                        {
                            format!("{head}[].{}", rest.join("."))
                        } else {
                            id.clone()
                        };
                        assert_eq!(normalize_parameter_id(&id), expected, "{id:?}");
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    mod legacy_automation {
        use super::*;
        pub(super) fn validate_automation(
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
                        let effects = match clip {
                            Clip::Audio(clip) => &clip.effects,
                            Clip::Event(clip) => &clip.effects,
                            Clip::Composition(_) => {
                                return Err(invalid(
                                    "automation.target",
                                    "target is not an audio or event clip",
                                ));
                            }
                        };
                        Some((processor(effects, processor_id)?, parameter_id))
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
    }

    #[test]
    fn dependency_nodes_preserve_string_key_order() {
        let mut nodes = Vec::new();
        for value in [u128::MAX, 0, 1, 0xff, 0x100, 1 << 64] {
            let id = uuid::Uuid::from_u128(value);
            nodes.extend([
                DependencyNode::Revision(AssetRevisionId(id)),
                DependencyNode::Composition(CompositionId(id)),
                DependencyNode::Asset(AssetId(id)),
            ]);
        }
        let mut names: Vec<_> = nodes.iter().map(ToString::to_string).collect();
        names.sort();
        nodes.sort();
        assert_eq!(
            nodes.iter().map(ToString::to_string).collect::<Vec<_>>(),
            names
        );
    }

    #[test]
    fn dependency_cycle_errors_match_string_traversal_for_all_small_graphs() {
        let first = uuid::Uuid::from_u128(1);
        let second = uuid::Uuid::from_u128(2);
        // Deliberately differ from sorted key order: edge order must remain intact.
        let nodes = [
            DependencyNode::Revision(AssetRevisionId(first)),
            DependencyNode::Asset(AssetId(second)),
            DependencyNode::Composition(CompositionId(first)),
            DependencyNode::Asset(AssetId(first)),
        ];
        for mask in 0_u32..(1 << 16) {
            let graph: DependencyGraph = nodes
                .iter()
                .enumerate()
                .map(|(from, &node)| {
                    let edges = nodes
                        .iter()
                        .enumerate()
                        .filter(|(to, _)| mask & (1 << (from * nodes.len() + to)) != 0)
                        .map(|(_, &dependency)| dependency)
                        .collect();
                    (node, edges)
                })
                .collect();
            let string_graph = graph
                .iter()
                .map(|(node, edges)| {
                    (
                        node.to_string(),
                        edges.iter().map(ToString::to_string).collect(),
                    )
                })
                .collect();
            assert_eq!(
                detect_cycles(&graph),
                string_cycle_reference(&string_graph),
                "graph {mask:04x}"
            );
        }
    }

    fn string_cycle_reference(graph: &BTreeMap<String, Vec<String>>) -> Result<(), DomainError> {
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

    #[test]
    fn duplicate_errors_follow_input_order_for_borrowed_processor_ids() {
        assert_eq!(
            unique(["z", "a", "z", "a"], "processor"),
            Err(already_exists("processor", "z"))
        );
        let processors: Vec<_> = ["z", "a", "z", "a"]
            .into_iter()
            .map(|id| {
                Processor::new(
                    ProcessorId::new(id).unwrap(),
                    ProcessorKind::Gain(crate::GainParameters::default()),
                )
            })
            .collect();
        assert_eq!(
            validate_processors(&processors),
            Err(already_exists("processor in stack", "z"))
        );
        let mut project = Project::new(
            "Duplicate processors",
            crate::Bpm::new(120.0).unwrap(),
            crate::SampleRate::new(48_000).unwrap(),
        );
        project.compositions[0].output_effects = processors;
        assert_eq!(project.validate(), Err(already_exists("processor", "z")));
    }
}
