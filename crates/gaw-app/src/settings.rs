use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
};

use serde::{Deserialize, Serialize};

pub(crate) const STORAGE_KEY: &str = "gaw.audio-settings.v1";
pub(crate) const SAMPLE_RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];
pub(crate) const BUFFER_SIZES: [u32; 7] = [32, 64, 128, 256, 512, 1_024, 2_048];

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub(crate) struct AudioPreferences {
    pub(crate) output_device: Option<SavedDevice>,
    pub(crate) input_device: Option<SavedDevice>,
    pub(crate) input_channel: usize,
    pub(crate) monitor_gain_db: i32,
    pub(crate) monitor_effects: Vec<gaw_core::Processor>,
    pub(crate) monitor_effects_bypassed: bool,
    pub(crate) buffer_frames: Option<u32>,
    pub(crate) audio_assets_directory: Option<PathBuf>,
}

impl AudioPreferences {
    pub(crate) fn load(storage: Option<&dyn eframe::Storage>) -> Self {
        storage
            .and_then(|storage| storage.get_string(STORAGE_KEY))
            .and_then(|value| serde_json::from_str::<Self>(&value).ok())
            .unwrap_or_default()
            .normalized()
    }

    pub(crate) fn save(&self, storage: &mut dyn eframe::Storage) {
        if let Ok(value) = serde_json::to_string(self) {
            storage.set_string(STORAGE_KEY, value);
        }
    }

    fn normalized(mut self) -> Self {
        self.input_channel = self.input_channel.min(127);
        self.monitor_gain_db = self.monitor_gain_db.clamp(-60, 12);
        self.monitor_effects
            .truncate(gaw_audio::monitor::MAX_INPUT_EFFECTS);
        self.monitor_effects
            .retain(|effect| !effect.kind.is_analyzer() && effect.validate().is_ok());
        if self
            .buffer_frames
            .is_some_and(|frames| !BUFFER_SIZES.contains(&frames))
        {
            self.buffer_frames = None;
        }
        self
    }

    pub(crate) fn available_audio_assets_directory(&self) -> Option<&Path> {
        self.audio_assets_directory
            .as_deref()
            .filter(|path| path.is_dir())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct SavedDevice {
    pub(crate) id: String,
    pub(crate) name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeviceChoice {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) backend: String,
    pub(crate) is_default: bool,
}

impl DeviceChoice {
    pub(crate) fn saved(&self) -> SavedDevice {
        SavedDevice {
            id: self.id.clone(),
            name: self.name.clone(),
        }
    }

    pub(crate) fn label(&self) -> String {
        let default = if self.is_default { " · default" } else { "" };
        format!("{} · {}{}", self.name, self.backend, default)
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct DeviceCatalog {
    pub(crate) outputs: Vec<DeviceChoice>,
    pub(crate) inputs: Vec<DeviceChoice>,
    pub(crate) errors: Vec<String>,
}

pub(crate) fn scan_devices() -> Receiver<DeviceCatalog> {
    let (sender, receiver) = mpsc::channel();
    let _ = thread::Builder::new()
        .name("gaw-device-scan".into())
        .spawn(move || {
            let mut catalog = DeviceCatalog::default();
            for backend in gaw_audio::available_audio_backends() {
                match gaw_audio::enumerate_output_devices(backend) {
                    Ok(devices) => {
                        catalog
                            .outputs
                            .extend(devices.into_iter().map(|device| DeviceChoice {
                                id: device.id.to_string(),
                                name: device.name,
                                backend: backend.to_string(),
                                is_default: device.is_default,
                            }));
                    }
                    Err(error) => catalog.errors.push(error.to_string()),
                }
                match gaw_audio::enumerate_input_devices(backend) {
                    Ok(devices) => {
                        catalog
                            .inputs
                            .extend(devices.into_iter().map(|device| DeviceChoice {
                                id: device.id.to_string(),
                                name: device.name,
                                backend: backend.to_string(),
                                is_default: device.is_default,
                            }));
                    }
                    Err(error) => catalog.errors.push(error.to_string()),
                }
            }
            sort_and_deduplicate(&mut catalog.outputs);
            sort_and_deduplicate(&mut catalog.inputs);
            let _ = sender.send(catalog);
        });
    receiver
}

fn sort_and_deduplicate(devices: &mut Vec<DeviceChoice>) {
    devices.sort_by_cached_key(|device| {
        (
            device.name.to_lowercase(),
            device.backend.clone(),
            device.id.clone(),
        )
    });
    devices.dedup_by(|left, right| left.id == right.id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferences_round_trip_and_invalid_buffer_falls_back_to_auto() {
        let settings = AudioPreferences {
            output_device: Some(SavedDevice {
                id: "Alsa:device".into(),
                name: "Speakers".into(),
            }),
            input_device: None,
            input_channel: 1,
            monitor_gain_db: -6,
            monitor_effects: Vec::new(),
            monitor_effects_bypassed: false,
            buffer_frames: Some(128),
            audio_assets_directory: Some(PathBuf::from("/audio/library")),
        };
        let encoded = serde_json::to_string(&settings).unwrap();
        assert_eq!(
            serde_json::from_str::<AudioPreferences>(&encoded).unwrap(),
            settings
        );

        let invalid = AudioPreferences {
            buffer_frames: Some(3),
            ..AudioPreferences::default()
        };
        assert_eq!(invalid.normalized().buffer_frames, None);
    }

    #[test]
    fn older_preferences_default_the_audio_assets_directory() {
        let settings: AudioPreferences = serde_json::from_str(
            r#"{"output_device":null,"input_device":null,"buffer_frames":128}"#,
        )
        .unwrap();

        assert_eq!(settings.audio_assets_directory, None);
        assert_eq!(settings.input_channel, 0);
        assert_eq!(settings.monitor_gain_db, 0);
        assert!(settings.monitor_effects.is_empty());
        assert!(!settings.monitor_effects_bypassed);
    }

    #[test]
    fn live_effects_preserve_order_parameters_and_bypass_in_preferences() {
        let mut effects: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|id| {
                gaw_core::Processor::new(
                    gaw_core::ProcessorId::new(id).unwrap(),
                    gaw_core::ProcessorKind::Gain(gaw_core::GainParameters::default()),
                )
            })
            .collect();
        effects[0].enabled = false;
        let settings = AudioPreferences {
            monitor_effects: effects,
            monitor_effects_bypassed: true,
            ..AudioPreferences::default()
        };
        let restored: AudioPreferences =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored.normalized(), settings);
    }

    #[test]
    fn monitoring_preferences_are_bounded_and_do_not_persist_enabled_state() {
        let settings = AudioPreferences {
            input_channel: usize::MAX,
            monitor_gain_db: 100,
            ..AudioPreferences::default()
        }
        .normalized();
        assert_eq!(settings.input_channel, 127);
        assert_eq!(settings.monitor_gain_db, 12);
        let encoded = serde_json::to_value(settings).unwrap();
        assert!(encoded.get("monitor_enabled").is_none());
    }

    #[test]
    fn only_available_audio_assets_directories_are_used() {
        let directory = tempfile::tempdir().unwrap();
        let available = AudioPreferences {
            audio_assets_directory: Some(directory.path().to_owned()),
            ..AudioPreferences::default()
        };
        assert_eq!(
            available.available_audio_assets_directory(),
            Some(directory.path())
        );

        let unavailable = AudioPreferences {
            audio_assets_directory: Some(directory.path().join("missing")),
            ..AudioPreferences::default()
        };
        assert_eq!(unavailable.available_audio_assets_directory(), None);
        assert!(unavailable.audio_assets_directory.is_some());
    }
}
