mod app;
mod clip_export;
mod controller;
mod launcher;
mod meter;
mod model;
pub mod physical_keyboard;
mod piano_roll;
mod project_library;
mod settings;
mod stem_splitter;
mod subprocess;
mod text_input;
mod theme;
mod timeline;
mod transcription;

pub use app::GawApp;
pub use controller::{NativeStartup, RecoveryPolicy};
pub use launcher::GawDesktop;
pub use model::{
    Asset, AudioClipEdit, ChangeSource, Clip, Composition, Effect, Intent, MidiAsset, Parameter,
    ProjectUpdate, ProjectViewModel, SamplerZone, StableSelection, Track, demo_project,
};
