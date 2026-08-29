use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
};

use anyhow::{Context, bail};
use gaw_core::{AutomationTarget, Beats, Clip, ClipId, CompositionId, Project, TrackId, Validate};
use gaw_project::ProjectStore;

const EXPORT_PAGE_FRAMES: usize = 65_536;

#[derive(Clone, Debug)]
pub(crate) struct ClipExportJob {
    pub(crate) project: Project,
    pub(crate) composition_id: CompositionId,
    pub(crate) track_id: TrackId,
    pub(crate) clip_id: ClipId,
    pub(crate) destination: PathBuf,
}

pub(crate) fn export_clip_mp3(
    store: &ProjectStore,
    job: &ClipExportJob,
) -> anyhow::Result<PathBuf> {
    let project = isolated_clip_project(job)?;
    let mut compiler = gaw_audio::StorePlaybackCompiler::default();
    let render = compiler
        .compile(store, &project)
        .context("could not compile the isolated clip")?;
    let root = render.plan().root();
    let track_id = job.track_id.to_string();
    let clip_id = job.clip_id.to_string();
    let clip = root
        .tracks
        .iter()
        .find(|track| track.id.as_ref() == track_id)
        .and_then(|track| track.clips.iter().find(|clip| clip.id.as_ref() == clip_id))
        .context("the isolated render plan does not contain the clip")?;
    let start_frame = clip.start_frame;
    let end_frame = root
        .length_frames
        .checked_add(root.tail_frames)
        .context("clip export range overflow")?;
    let frames = end_frame.saturating_sub(start_frame);
    if frames == 0 {
        bail!("clip export range is empty");
    }
    let layout = root.output_layout;
    let mut pages = Vec::new();
    let mut page_start = start_frame;
    while page_start < end_frame {
        let page_frames = usize::try_from(end_frame - page_start)
            .unwrap_or(usize::MAX)
            .min(EXPORT_PAGE_FRAMES);
        pages.push(
            render
                .prepare_page(page_start, page_frames)
                .context("could not render a clip page")?,
        );
        page_start = page_start.saturating_add(page_frames as u64);
    }
    let snapshot = render
        .paged_snapshot(pages)
        .context("could not build the clip render")?;
    gaw_audio::render_mp3(
        &snapshot,
        &job.destination,
        gaw_audio::OfflineMp3Spec {
            start_frame,
            frames: Some(frames),
            layout,
            ..gaw_audio::OfflineMp3Spec::default()
        },
    )
    .with_context(|| format!("could not write {}", job.destination.display()))?;
    Ok(job.destination.clone())
}

fn isolated_clip_project(job: &ClipExportJob) -> anyhow::Result<Project> {
    let root = job
        .project
        .compositions
        .iter()
        .find(|composition| composition.id == job.composition_id)
        .context("clip composition no longer exists")?;
    let track = job
        .project
        .tracks
        .iter()
        .find(|track| track.id == job.track_id && track.composition_id == root.id)
        .context("clip track no longer exists")?;
    let clip = track
        .clips
        .iter()
        .find(|clip| clip.id() == job.clip_id)
        .cloned()
        .context("clip no longer exists")?;

    let clip_end = clip.start().value() + clip_duration(&clip);
    let mut isolated_root = root.clone();
    isolated_root.length = Beats::new(clip_end).context("clip end is invalid")?;
    isolated_root.track_ids = vec![track.id];
    isolated_root.track_groups.clear();

    let mut isolated_track = track.clone();
    isolated_track.muted = false;
    isolated_track.solo = false;
    isolated_track.clips = vec![unmuted(clip.clone())];

    let mut included_compositions = HashSet::from([root.id]);
    let mut pending = VecDeque::new();
    if let Clip::Composition(clip) = &clip {
        pending.push_back(clip.composition_id);
    }
    while let Some(composition_id) = pending.pop_front() {
        if !included_compositions.insert(composition_id) {
            continue;
        }
        let composition = job
            .project
            .compositions
            .iter()
            .find(|composition| composition.id == composition_id)
            .context("nested clip composition no longer exists")?;
        for track_id in &composition.track_ids {
            let nested_track = job
                .project
                .tracks
                .iter()
                .find(|track| track.id == *track_id)
                .context("nested clip track no longer exists")?;
            pending.extend(nested_track.clips.iter().filter_map(|clip| match clip {
                Clip::Composition(clip) => Some(clip.composition_id),
                Clip::Audio(_) | Clip::Event(_) => None,
            }));
        }
    }

    let mut project = job.project.clone();
    project.root_composition_id = root.id;
    project.compositions.retain(|composition| {
        included_compositions.contains(&composition.id) && composition.id != root.id
    });
    project.compositions.push(isolated_root);
    let included_tracks = project
        .compositions
        .iter()
        .flat_map(|composition| composition.track_ids.iter().copied())
        .collect::<HashSet<_>>();
    project
        .tracks
        .retain(|candidate| included_tracks.contains(&candidate.id) && candidate.id != track.id);
    project.tracks.push(isolated_track);
    project.automation.retain(|lane| {
        if !included_compositions.contains(&lane.composition_id) {
            return false;
        }
        if lane.composition_id != root.id {
            return true;
        }
        match lane.target {
            AutomationTarget::AudioClipProcessor {
                track_id, clip_id, ..
            }
            | AutomationTarget::CompositionClipProcessor {
                track_id, clip_id, ..
            } => track_id == track.id && clip_id == clip.id(),
            AutomationTarget::TrackProcessor { track_id, .. }
            | AutomationTarget::Instrument { track_id, .. } => track_id == track.id,
            AutomationTarget::CompositionOutputProcessor { .. } => true,
        }
    });
    project
        .validate()
        .context("isolated clip project is invalid")?;
    Ok(project)
}

fn clip_duration(clip: &Clip) -> f64 {
    match clip {
        Clip::Audio(clip) => clip.duration.value(),
        Clip::Event(clip) => clip.duration.value(),
        Clip::Composition(clip) => clip.duration.value(),
    }
}

fn unmuted(mut clip: Clip) -> Clip {
    match &mut clip {
        Clip::Audio(clip) => clip.muted = false,
        Clip::Event(clip) => clip.muted = false,
        Clip::Composition(clip) => clip.muted = false,
    }
    clip
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolated_project_keeps_only_the_target_root_track_and_clip() {
        let project = crate::model::demo_project();
        let composition_id = project.root_composition_id;
        let composition = project
            .compositions
            .iter()
            .find(|composition| composition.id == composition_id)
            .unwrap();
        let track_id = composition.track_ids[0];
        let track = project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .unwrap();
        let original_root = composition.clone();
        let original_track = track.clone();
        let expected_clip = unmuted(track.clips[0].clone());
        let clip_id = track.clips[0].id();
        let expected_end = track.clips[0].start().value() + clip_duration(&track.clips[0]);
        let job = ClipExportJob {
            project,
            composition_id,
            track_id,
            clip_id,
            destination: PathBuf::from("clip.mp3"),
        };

        let isolated = isolated_clip_project(&job).unwrap();
        let root = isolated
            .compositions
            .iter()
            .find(|composition| composition.id == composition_id)
            .unwrap();
        assert_eq!(root.track_ids, vec![track_id]);
        assert!((root.length.value() - expected_end).abs() < f64::EPSILON);
        assert_eq!(root.output_effects, original_root.output_effects);
        let track = isolated
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .unwrap();
        assert_eq!(track.clips.len(), 1);
        assert_eq!(track.clips[0], expected_clip);
        assert!(!track.muted);
        assert!(!track.solo);
        assert!((track.volume_db - original_track.volume_db).abs() < f32::EPSILON);
        assert_eq!(track.effects, original_track.effects);
        assert_eq!(track.instrument, original_track.instrument);
    }
}
