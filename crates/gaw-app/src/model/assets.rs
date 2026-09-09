use super::{
    AssetFolder, AssetFolderId, AssetId, BTreeSet, Command, ProjectViewModel, Transaction,
};

impl ProjectViewModel {
    pub fn asset_folders(&self) -> &[AssetFolder] {
        &self.project.asset_folders
    }

    pub fn create_asset_folder(
        &mut self,
        name: &str,
        asset: Option<usize>,
    ) -> Option<AssetFolderId> {
        self.create_asset_folder_for_assets(name, &asset.into_iter().collect::<Vec<_>>(), &[])
    }

    pub fn create_asset_folder_for_assets(
        &mut self,
        name: &str,
        audio: &[usize],
        midi: &[usize],
    ) -> Option<AssetFolderId> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let asset_ids = audio
            .iter()
            .filter_map(|index| self.asset_id(*index))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let event_data_ids = midi
            .iter()
            .filter_map(|index| self.project.event_data.get(*index).map(|data| data.id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let folder = AssetFolder {
            id: AssetFolderId::new(),
            name: name.to_owned(),
            asset_ids,
            event_data_ids,
        };
        let folder_id = folder.id;
        let mut folders = self.project.asset_folders.clone();
        for existing in &mut folders {
            existing
                .asset_ids
                .retain(|candidate| !folder.asset_ids.contains(candidate));
            existing
                .event_data_ids
                .retain(|candidate| !folder.event_data_ids.contains(candidate));
        }
        folders.push(folder);
        let transaction = Transaction::named(
            "Create asset folder",
            [Command::SetAssetFolders { folders }],
        );
        self.commit_ui(&transaction, &[folder_id.to_string()]);
        self.last_error.is_none().then_some(folder_id)
    }

    pub fn rename_asset_folder(&mut self, folder_id: AssetFolderId, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let mut folders = self.project.asset_folders.clone();
        let Some(folder) = folders.iter_mut().find(|folder| folder.id == folder_id) else {
            return;
        };
        if folder.name == name {
            return;
        }
        name.clone_into(&mut folder.name);
        let transaction = Transaction::named(
            "Rename asset folder",
            [Command::SetAssetFolders { folders }],
        );
        self.commit_ui(&transaction, &[folder_id.to_string()]);
    }

    pub fn remove_asset_folder(&mut self, folder_id: AssetFolderId) {
        let mut folders = self.project.asset_folders.clone();
        let old_len = folders.len();
        folders.retain(|folder| folder.id != folder_id);
        if folders.len() == old_len {
            return;
        }
        let transaction = Transaction::named(
            "Delete asset folder",
            [Command::SetAssetFolders { folders }],
        );
        self.commit_ui(&transaction, &[folder_id.to_string()]);
    }

    pub fn move_asset_to_folder(&mut self, asset: usize, folder_id: Option<AssetFolderId>) {
        let Some(asset_id) = self.asset_id(asset) else {
            return;
        };
        self.move_asset_id_to_folder(asset_id, folder_id);
    }

    pub fn move_midi_asset_to_folder(&mut self, asset: usize, folder_id: Option<AssetFolderId>) {
        let Some(event_data_id) = self.project.event_data.get(asset).map(|data| data.id) else {
            return;
        };
        if folder_id.is_some_and(|folder_id| {
            self.project
                .asset_folders
                .iter()
                .all(|folder| folder.id != folder_id)
        }) {
            return;
        }
        let mut folders = self.project.asset_folders.clone();
        let old_folders = folders.clone();
        for folder in &mut folders {
            folder
                .event_data_ids
                .retain(|candidate| *candidate != event_data_id);
        }
        if let Some(folder_id) = folder_id
            && let Some(folder) = folders.iter_mut().find(|folder| folder.id == folder_id)
        {
            folder.event_data_ids.push(event_data_id);
        }
        if folders == old_folders {
            return;
        }
        let transaction = Transaction::named(
            "Move MIDI asset to folder",
            [Command::SetAssetFolders { folders }],
        );
        self.commit_ui(&transaction, &[event_data_id.to_string()]);
    }

    pub fn move_assets_to_folder(
        &mut self,
        audio: &[usize],
        midi: &[usize],
        folder_id: Option<AssetFolderId>,
    ) {
        if folder_id.is_some_and(|folder_id| {
            self.project
                .asset_folders
                .iter()
                .all(|folder| folder.id != folder_id)
        }) {
            return;
        }
        let asset_ids = audio
            .iter()
            .filter_map(|index| self.asset_id(*index))
            .collect::<BTreeSet<_>>();
        let event_data_ids = midi
            .iter()
            .filter_map(|index| self.project.event_data.get(*index).map(|data| data.id))
            .collect::<BTreeSet<_>>();
        if asset_ids.is_empty() && event_data_ids.is_empty() {
            return;
        }
        let mut folders = self.project.asset_folders.clone();
        let old_folders = folders.clone();
        for folder in &mut folders {
            folder
                .asset_ids
                .retain(|candidate| !asset_ids.contains(candidate));
            folder
                .event_data_ids
                .retain(|candidate| !event_data_ids.contains(candidate));
        }
        if let Some(folder_id) = folder_id
            && let Some(folder) = folders.iter_mut().find(|folder| folder.id == folder_id)
        {
            folder.asset_ids.extend(asset_ids.iter().copied());
            folder.event_data_ids.extend(event_data_ids.iter().copied());
        }
        if folders == old_folders {
            return;
        }
        let changed_ids = asset_ids
            .iter()
            .map(ToString::to_string)
            .chain(event_data_ids.iter().map(ToString::to_string))
            .collect::<Vec<_>>();
        self.commit_ui(
            &Transaction::named(
                "Move selected assets to folder",
                [Command::SetAssetFolders { folders }],
            ),
            &changed_ids,
        );
    }

    pub(super) fn move_asset_id_to_folder(
        &mut self,
        asset_id: AssetId,
        folder_id: Option<AssetFolderId>,
    ) {
        if folder_id.is_some_and(|folder_id| {
            self.project
                .asset_folders
                .iter()
                .all(|folder| folder.id != folder_id)
        }) {
            return;
        }
        let mut folders = self.project.asset_folders.clone();
        let old_folders = folders.clone();
        for folder in &mut folders {
            folder.asset_ids.retain(|candidate| *candidate != asset_id);
        }
        if let Some(folder_id) = folder_id
            && let Some(folder) = folders.iter_mut().find(|folder| folder.id == folder_id)
        {
            folder.asset_ids.push(asset_id);
        }
        if folders == old_folders {
            return;
        }
        let transaction = Transaction::named(
            "Move audio asset to folder",
            [Command::SetAssetFolders { folders }],
        );
        self.commit_ui(&transaction, &[asset_id.to_string()]);
    }

    pub fn set_asset_tempo(&mut self, index: usize, bpm: Option<f32>, first_beat_seconds: f32) {
        let Some(asset_id) = self.asset_id(index) else {
            return;
        };
        let tempo = bpm.and_then(|bpm| {
            Some(gaw_core::AssetTempo {
                bpm: gaw_core::Bpm::new(f64::from(bpm)).ok()?,
                first_beat: gaw_core::Seconds::new(f64::from(first_beat_seconds.max(0.0))).ok()?,
            })
        });
        if bpm.is_some() && tempo.is_none() {
            return;
        }
        let transaction = Transaction::named(
            "Set asset tempo",
            [Command::SetAssetTempo { asset_id, tempo }],
        );
        self.commit_ui(&transaction, &[asset_id.to_string()]);
    }

    /// Sets the tempo of selected audio assets in one transaction.
    ///
    /// # Panics
    /// Panics if a stored first-beat value violates the canonical model.
    pub fn set_assets_tempo(&mut self, indices: &[usize], bpm: f32) {
        let Ok(bpm) = gaw_core::Bpm::new(f64::from(bpm)) else {
            return;
        };
        let asset_ids = indices
            .iter()
            .filter_map(|index| self.project.assets.get(*index))
            .map(|asset| asset.id)
            .collect::<BTreeSet<_>>();
        if asset_ids.is_empty() {
            return;
        }
        let commands = asset_ids
            .iter()
            .filter_map(|asset_id| {
                let asset = self
                    .project
                    .assets
                    .iter()
                    .find(|asset| asset.id == *asset_id)?;
                Some(Command::SetAssetTempo {
                    asset_id: *asset_id,
                    tempo: Some(gaw_core::AssetTempo {
                        bpm,
                        first_beat: gaw_core::Seconds::new(
                            asset.tempo.map_or(0.0, |tempo| tempo.first_beat.value()),
                        )
                        .expect("stored first beat is valid"),
                    }),
                })
            })
            .collect::<Vec<_>>();
        let changed_ids = asset_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        self.commit_ui(
            &Transaction::named("Set selected asset tempos", commands),
            &changed_ids,
        );
    }

    pub fn rename_asset(&mut self, index: usize, name: &str) {
        let Some(asset) = self.project.assets.get(index).cloned() else {
            return;
        };
        let name = name.trim().to_owned();
        if name.is_empty() || name == asset.name {
            return;
        }
        let mut renamed = asset;
        renamed.name = name;
        let asset_id = renamed.id;
        let transaction =
            Transaction::named("Rename asset", [Command::UpdateAsset { asset: renamed }]);
        self.commit_ui(&transaction, &[asset_id.to_string()]);
    }

    pub fn remove_asset(&mut self, index: usize) {
        self.remove_assets(&[index], &[]);
    }

    pub fn remove_assets(&mut self, audio: &[usize], midi: &[usize]) {
        let asset_ids = audio
            .iter()
            .filter_map(|index| self.asset_id(*index))
            .collect::<BTreeSet<_>>();
        let event_data_ids = midi
            .iter()
            .filter_map(|index| self.project.event_data.get(*index).map(|data| data.id))
            .collect::<BTreeSet<_>>();
        if asset_ids.is_empty() && event_data_ids.is_empty() {
            return;
        }
        let commands = asset_ids
            .iter()
            .map(|asset_id| Command::RemoveAsset {
                asset_id: *asset_id,
            })
            .chain(
                event_data_ids
                    .iter()
                    .map(|event_data_id| Command::RemoveEventData {
                        event_data_id: *event_data_id,
                    }),
            )
            .collect::<Vec<_>>();
        let changed_ids = asset_ids
            .iter()
            .map(ToString::to_string)
            .chain(event_data_ids.iter().map(ToString::to_string))
            .collect::<Vec<_>>();
        self.commit_ui(
            &Transaction::named("Delete selected assets", commands),
            &changed_ids,
        );
    }

    pub fn accept_asset_tempo_suggestion(
        &mut self,
        index: usize,
        suggested_bpm: f32,
        first_beat_seconds: f32,
    ) {
        self.set_asset_tempo(index, Some(suggested_bpm), first_beat_seconds);
    }
}
