//! Explicit non-persistent demo fixture, expressed in the canonical project JSON model.

use super::{Project, ProjectViewModel, WaveformPoint};
use std::sync::Arc;

/// Loads the bundled demo snapshot. Referenced audio files are illustrative, so
/// this fixture is for the non-persistent UI demo and domain tests only.
///
/// # Panics
/// Panics if the bundled fixture violates the canonical model.
pub fn demo_project() -> Project {
    use gaw_core::Validate as _;
    let project: Project = serde_json::from_str(include_str!("../../fixtures/demo-project.json"))
        .expect("bundled demo JSON must deserialize");
    project.validate().expect("bundled demo must be valid");
    project
}

#[allow(clippy::cast_precision_loss)]
fn waveform(seed: f32, len: usize) -> Arc<[WaveformPoint]> {
    (0..len)
        .map(|index| {
            let phase = index as f32 / len as f32;
            let body = (phase * 31.0 * seed).sin() * 0.55 + (phase * 73.0).sin() * 0.22;
            let envelope = (phase * std::f32::consts::PI).sin().powf(0.35);
            let amplitude = (body * envelope).abs().clamp(0.03, 0.96);
            WaveformPoint {
                minimum: -amplitude,
                maximum: amplitude,
            }
        })
        .collect::<Vec<_>>()
        .into()
}

fn id_seed(id: &str) -> f32 {
    let value = id.bytes().fold(17_u32, |state, byte| {
        state.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    (value % 97) as f32 / 17.0 + 0.7
}

impl ProjectViewModel {
    pub(crate) fn initialize_demo_waveforms(&mut self) {
        for asset in &mut self.assets {
            asset.waveform = waveform(id_seed(&asset.id), 256);
        }
        for clip in self
            .compositions
            .iter_mut()
            .flat_map(|composition| &mut composition.tracks)
            .flat_map(|track| &mut track.clips)
        {
            clip.waveform = waveform(id_seed(&clip.id), 320);
        }
    }
}
