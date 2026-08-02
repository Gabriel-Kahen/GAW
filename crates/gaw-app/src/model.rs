use std::sync::Arc;

pub const MIN_BPM: f32 = 40.0;
pub const MAX_BPM: f32 = 240.0;
pub const HIGHLIGHT_SECONDS: f64 = 2.4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    None,
    Repitch,
    Stretch,
}

impl SyncMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::None => "FREE",
            Self::Repitch => "REPITCH",
            Self::Stretch => "STRETCH",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderState {
    Fresh,
    Stale,
    Rendering(u8),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Note {
    pub start: f32,
    pub length: f32,
    pub pitch: u8,
    pub velocity: f32,
}

#[derive(Clone, Debug)]
pub enum ClipKind {
    Audio {
        asset: usize,
        sync: SyncMode,
        source_bpm: Option<f32>,
    },
    Event {
        notes: Arc<[Note]>,
    },
    Composition {
        child: usize,
        render: RenderState,
        tail_beats: f32,
    },
}

#[derive(Clone, Debug)]
pub struct Effect {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub enabled: bool,
    pub parameters: Vec<Parameter>,
}

#[derive(Clone, Debug)]
pub struct Parameter {
    pub id: String,
    pub label: String,
    pub value: f32,
    pub min: f32,
    pub max: f32,
    pub unit: String,
}

#[derive(Clone, Debug)]
pub struct Clip {
    pub id: String,
    pub name: String,
    pub start: f32,
    pub length: f32,
    pub gain_db: f32,
    pub waveform: Arc<[f32]>,
    pub kind: ClipKind,
    pub effects: Vec<Effect>,
}

impl Clip {
    pub fn end(&self) -> f32 {
        self.start + self.length
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackKind {
    Audio,
    Event,
    Composition,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: String,
    pub name: String,
    pub kind: TrackKind,
    pub muted: bool,
    pub solo: bool,
    pub level: f32,
    pub max_visual_length: f32,
    pub clips: Vec<Clip>,
    pub effects: Vec<Effect>,
}

#[derive(Clone, Debug)]
pub struct Composition {
    pub id: String,
    pub name: String,
    pub length_beats: f32,
    pub tracks: Vec<Track>,
    pub output_effects: Vec<Effect>,
}

#[derive(Clone, Debug)]
pub struct Asset {
    pub id: String,
    pub name: String,
    pub duration_seconds: f32,
    pub channels: u8,
    pub bpm: Option<f32>,
    pub waveform: Arc<[f32]>,
    pub changed_by_agent: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    None,
    Asset(usize),
    Clip {
        track: usize,
        clip: usize,
    },
    Effect {
        track: usize,
        clip: usize,
        effect: usize,
    },
    Sampler {
        track: usize,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorKind {
    Overview,
    Waveform,
    PianoRoll,
    Sampler,
    Effect,
}

#[derive(Clone, Debug)]
pub struct Transport {
    pub playing: bool,
    pub recording: bool,
    pub loop_enabled: bool,
    pub playhead: f32,
    pub bpm: f32,
}

#[derive(Clone, Debug)]
struct Highlight {
    entity_id: String,
    changed_at: f64,
}

#[derive(Clone, Copy, Debug)]
pub enum Intent {
    TogglePlayback,
    ToggleRecording,
    Stop,
    ToggleLoop,
    Seek(f32),
    SetBpm(f32),
    Select(Selection),
    ClearSelection,
    EnterChild {
        track: usize,
        clip: usize,
    },
    NavigateToDepth(usize),
    Back,
    ToggleMute(usize),
    ToggleSolo(usize),
    ToggleEffect {
        track: usize,
        clip: usize,
        effect: usize,
    },
    MoveEffect {
        track: usize,
        clip: usize,
        effect: usize,
        delta: isize,
    },
    SetEffectParameter {
        track: usize,
        clip: usize,
        effect: usize,
        parameter: usize,
        value: f32,
    },
    AddAssetClip {
        asset: usize,
        beat: f32,
        track: Option<usize>,
    },
    ToggleStructureLens,
    SimulateAgentChange(f64),
}

#[derive(Clone, Debug)]
pub struct DemoViewModel {
    pub compositions: Vec<Composition>,
    pub assets: Vec<Asset>,
    pub transport: Transport,
    pub selection: Selection,
    pub structure_lens: bool,
    nav_path: Vec<usize>,
    highlights: Vec<Highlight>,
    next_clip: u32,
    next_track: u32,
}

impl Default for DemoViewModel {
    fn default() -> Self {
        Self::demo()
    }
}

impl DemoViewModel {
    pub fn demo() -> Self {
        let assets = demo_assets();
        let compositions = demo_compositions();
        Self {
            compositions,
            assets,
            transport: Transport {
                playing: false,
                recording: false,
                loop_enabled: true,
                playhead: 13.25,
                bpm: 120.0,
            },
            selection: Selection::Clip { track: 0, clip: 1 },
            structure_lens: false,
            nav_path: vec![0],
            highlights: vec![
                Highlight {
                    entity_id: "clip_vocal".into(),
                    changed_at: 0.0,
                },
                Highlight {
                    entity_id: "ast_vocal".into(),
                    changed_at: 0.0,
                },
            ],
            next_clip: 1,
            next_track: 1,
        }
    }

    pub fn current_composition(&self) -> &Composition {
        &self.compositions[*self.nav_path.last().expect("root composition exists")]
    }

    pub fn current_composition_mut(&mut self) -> &mut Composition {
        let index = *self.nav_path.last().expect("root composition exists");
        &mut self.compositions[index]
    }

    pub fn breadcrumbs(&self) -> impl Iterator<Item = &Composition> {
        self.nav_path.iter().map(|index| &self.compositions[*index])
    }

    pub fn can_navigate_back(&self) -> bool {
        self.nav_path.len() > 1
    }

    pub fn editor_kind(&self) -> EditorKind {
        match self.selection {
            Selection::None => EditorKind::Overview,
            Selection::Asset(_) => EditorKind::Waveform,
            Selection::Sampler { .. } => EditorKind::Sampler,
            Selection::Effect { .. } => EditorKind::Effect,
            Selection::Clip { track, clip } => self
                .current_composition()
                .tracks
                .get(track)
                .and_then(|track| track.clips.get(clip))
                .map_or(EditorKind::Overview, |clip| match clip.kind {
                    ClipKind::Audio { .. } | ClipKind::Composition { .. } => EditorKind::Waveform,
                    ClipKind::Event { .. } => EditorKind::PianoRoll,
                }),
        }
    }

    pub fn selected_clip(&self) -> Option<(usize, usize, &Clip)> {
        let (Selection::Clip {
            track: track_index,
            clip: clip_index,
        }
        | Selection::Effect {
            track: track_index,
            clip: clip_index,
            ..
        }) = self.selection
        else {
            return None;
        };
        let clip = self
            .current_composition()
            .tracks
            .get(track_index)?
            .clips
            .get(clip_index)?;
        Some((track_index, clip_index, clip))
    }

    #[allow(clippy::cast_possible_truncation)]
    pub fn highlight_alpha(&self, entity_id: &str, now: f64) -> f32 {
        self.highlights
            .iter()
            .find(|highlight| highlight.entity_id == entity_id)
            .map_or(0.0, |highlight| {
                let elapsed = now - highlight.changed_at;
                if (0.0..HIGHLIGHT_SECONDS).contains(&elapsed) {
                    (1.0 - elapsed / HIGHLIGHT_SECONDS) as f32
                } else {
                    0.0
                }
            })
    }

    pub fn has_active_highlights(&self, now: f64) -> bool {
        self.highlights
            .iter()
            .any(|highlight| now - highlight.changed_at < HIGHLIGHT_SECONDS)
    }

    pub fn advance(&mut self, seconds: f32) {
        if !self.transport.playing {
            return;
        }
        let beats_per_second = self.transport.bpm / 60.0;
        let length = self.current_composition().length_beats;
        let next = self.transport.playhead + seconds * beats_per_second;
        if next >= length {
            self.transport.playhead = if self.transport.loop_enabled && length > 0.0 {
                next.rem_euclid(length)
            } else {
                length
            };
            if !self.transport.loop_enabled {
                self.transport.playing = false;
            }
        } else {
            self.transport.playhead = next;
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn apply(&mut self, intent: Intent) {
        match intent {
            Intent::TogglePlayback => self.transport.playing = !self.transport.playing,
            Intent::ToggleRecording => self.transport.recording = !self.transport.recording,
            Intent::Stop => {
                self.transport.playing = false;
                self.transport.recording = false;
                self.transport.playhead = 0.0;
            }
            Intent::ToggleLoop => self.transport.loop_enabled = !self.transport.loop_enabled,
            Intent::Seek(beat) => {
                self.transport.playhead = beat.clamp(0.0, self.current_composition().length_beats);
            }
            Intent::SetBpm(bpm) => self.transport.bpm = bpm.clamp(MIN_BPM, MAX_BPM),
            Intent::Select(selection) => self.selection = selection,
            Intent::ClearSelection => self.selection = Selection::None,
            Intent::EnterChild { track, clip } => {
                let child = self
                    .current_composition()
                    .tracks
                    .get(track)
                    .and_then(|track| track.clips.get(clip))
                    .and_then(|clip| match clip.kind {
                        ClipKind::Composition { child, .. } => Some(child),
                        _ => None,
                    });
                if let Some(child) = child {
                    self.nav_path.push(child);
                    self.selection = Selection::None;
                    self.transport.playhead = 0.0;
                }
            }
            Intent::NavigateToDepth(depth) => {
                if depth < self.nav_path.len() {
                    self.nav_path.truncate(depth + 1);
                    self.selection = Selection::None;
                    self.transport.playhead = 0.0;
                }
            }
            Intent::Back => {
                if self.nav_path.len() > 1 {
                    self.nav_path.pop();
                    self.selection = Selection::None;
                    self.transport.playhead = 0.0;
                }
            }
            Intent::ToggleMute(track) => {
                if let Some(track) = self.current_composition_mut().tracks.get_mut(track) {
                    track.muted = !track.muted;
                }
            }
            Intent::ToggleSolo(track) => {
                if let Some(track) = self.current_composition_mut().tracks.get_mut(track) {
                    track.solo = !track.solo;
                }
            }
            Intent::ToggleEffect {
                track,
                clip,
                effect,
            } => {
                if let Some(effect) = self.effect_mut(track, clip, effect) {
                    effect.enabled = !effect.enabled;
                }
            }
            Intent::MoveEffect {
                track,
                clip,
                effect,
                delta,
            } => {
                let selected_effect = self.move_effect(track, clip, effect, delta);
                if let Some(effect) = selected_effect {
                    self.selection = Selection::Effect {
                        track,
                        clip,
                        effect,
                    };
                }
            }
            Intent::SetEffectParameter {
                track,
                clip,
                effect,
                parameter,
                value,
            } => {
                if let Some(parameter) = self
                    .effect_mut(track, clip, effect)
                    .and_then(|effect| effect.parameters.get_mut(parameter))
                {
                    parameter.value = value.clamp(parameter.min, parameter.max);
                }
            }
            Intent::AddAssetClip { asset, beat, track } => {
                self.add_asset_clip(asset, beat, track);
            }
            Intent::ToggleStructureLens => self.structure_lens = !self.structure_lens,
            Intent::SimulateAgentChange(now) => {
                if let Some(asset) = self.assets.get_mut(2) {
                    asset.changed_by_agent = true;
                }
                for entity_id in ["clip_vocal", "ast_vocal"] {
                    if let Some(highlight) = self
                        .highlights
                        .iter_mut()
                        .find(|highlight| highlight.entity_id == entity_id)
                    {
                        highlight.changed_at = now;
                    } else {
                        self.highlights.push(Highlight {
                            entity_id: entity_id.into(),
                            changed_at: now,
                        });
                    }
                }
            }
        }
    }

    fn effect_mut(&mut self, track: usize, clip: usize, effect: usize) -> Option<&mut Effect> {
        self.current_composition_mut()
            .tracks
            .get_mut(track)?
            .clips
            .get_mut(clip)?
            .effects
            .get_mut(effect)
    }

    fn move_effect(
        &mut self,
        track: usize,
        clip: usize,
        effect: usize,
        delta: isize,
    ) -> Option<usize> {
        let effects = &mut self
            .current_composition_mut()
            .tracks
            .get_mut(track)?
            .clips
            .get_mut(clip)?
            .effects;
        let target = effect.checked_add_signed(delta)?;
        if effect >= effects.len() || target >= effects.len() {
            return None;
        }
        effects.swap(effect, target);
        Some(target)
    }

    fn add_asset_clip(&mut self, asset_index: usize, beat: f32, requested_track: Option<usize>) {
        let Some(asset) = self.assets.get(asset_index) else {
            return;
        };
        let asset_name = asset.name.clone();
        let waveform = Arc::clone(&asset.waveform);
        let bpm = asset.bpm;
        let project_bpm = self.transport.bpm;
        let id = format!("clip_drop_{:04}", self.next_clip);
        let inserted_id = id.clone();
        let effect_id = format!("{id}_gain");
        self.next_clip += 1;
        let track_index = self.audio_drop_track(requested_track);
        let length = if let Some(source_bpm) = bpm {
            8.0 * project_bpm / source_bpm
        } else {
            4.0
        };
        let clip = Clip {
            id,
            name: asset_name,
            start: beat.max(0.0),
            length,
            gain_db: 0.0,
            waveform,
            kind: ClipKind::Audio {
                asset: asset_index,
                sync: if bpm.is_some() {
                    SyncMode::Stretch
                } else {
                    SyncMode::None
                },
                source_bpm: bpm,
            },
            effects: vec![gain_effect(&effect_id)],
        };
        let track = &mut self.current_composition_mut().tracks[track_index];
        track.max_visual_length = track.max_visual_length.max(length);
        let clips = &mut track.clips;
        clips.push(clip);
        clips.sort_by(|left, right| left.start.total_cmp(&right.start));
        let clip_index = clips
            .iter()
            .position(|clip| clip.id == inserted_id)
            .unwrap_or(0);
        self.selection = Selection::Clip {
            track: track_index,
            clip: clip_index,
        };
    }

    fn audio_drop_track(&mut self, requested_track: Option<usize>) -> usize {
        if let Some(index) = requested_track
            && self
                .current_composition()
                .tracks
                .get(index)
                .is_some_and(|track| track.kind == TrackKind::Audio)
        {
            return index;
        }
        if requested_track.is_none()
            && let Some(index) = self
                .current_composition()
                .tracks
                .iter()
                .position(|track| track.kind == TrackKind::Audio)
        {
            return index;
        }
        let track_count = self.current_composition().tracks.len();
        let insert_at = requested_track
            .map_or(track_count, |index| index + 1)
            .min(track_count);
        let id = format!("trk_drop_{:04}", self.next_track);
        let effect_id = format!("{id}_gain");
        self.next_track += 1;
        self.current_composition_mut().tracks.insert(
            insert_at,
            Track {
                id,
                name: "DROPPED AUDIO".into(),
                kind: TrackKind::Audio,
                muted: false,
                solo: false,
                level: 0.8,
                max_visual_length: 0.0,
                clips: Vec::new(),
                effects: vec![gain_effect(&effect_id)],
            },
        );
        insert_at
    }
}

#[allow(clippy::cast_precision_loss)]
fn waveform(seed: f32, len: usize) -> Arc<[f32]> {
    (0..len)
        .map(|index| {
            let phase = index as f32 / len as f32;
            let body = (phase * 31.0 * seed).sin() * 0.55 + (phase * 73.0).sin() * 0.22;
            let envelope = (phase * std::f32::consts::PI).sin().powf(0.35);
            (body * envelope).abs().clamp(0.03, 0.96)
        })
        .collect::<Vec<_>>()
        .into()
}

fn parameter(id: &str, label: &str, value: f32, min: f32, max: f32, unit: &str) -> Parameter {
    Parameter {
        id: id.into(),
        label: label.into(),
        value,
        min,
        max,
        unit: unit.into(),
    }
}

fn gain_effect(id: &str) -> Effect {
    Effect {
        id: id.into(),
        name: "Gain & Pan".into(),
        kind: "gaw.gain".into(),
        enabled: true,
        parameters: vec![
            parameter("gain_db", "Gain", -1.5, -24.0, 12.0, "dB"),
            parameter("pan", "Pan", 0.0, -1.0, 1.0, ""),
        ],
    }
}

fn delay_effect(id: &str) -> Effect {
    Effect {
        id: id.into(),
        name: "Echo Space".into(),
        kind: "gaw.delay".into(),
        enabled: true,
        parameters: vec![
            parameter("time", "Time", 0.5, 0.0625, 2.0, "beats"),
            parameter("feedback", "Feedback", 0.34, 0.0, 0.92, ""),
            parameter("mix", "Mix", 0.22, 0.0, 1.0, ""),
        ],
    }
}

fn demo_assets() -> Vec<Asset> {
    vec![
        Asset {
            id: "ast_kick".into(),
            name: "Soft Kick 04".into(),
            duration_seconds: 0.72,
            channels: 1,
            bpm: None,
            waveform: waveform(1.2, 128),
            changed_by_agent: false,
        },
        Asset {
            id: "ast_loop".into(),
            name: "Dust Loop".into(),
            duration_seconds: 4.36,
            channels: 2,
            bpm: Some(110.0),
            waveform: waveform(1.8, 256),
            changed_by_agent: false,
        },
        Asset {
            id: "ast_vocal".into(),
            name: "Vocal Air".into(),
            duration_seconds: 6.18,
            channels: 2,
            bpm: Some(120.0),
            waveform: waveform(2.4, 256),
            changed_by_agent: true,
        },
        Asset {
            id: "ast_hat".into(),
            name: "Porcelain Hat".into(),
            duration_seconds: 0.31,
            channels: 1,
            bpm: None,
            waveform: waveform(3.1, 96),
            changed_by_agent: false,
        },
        Asset {
            id: "ast_texture".into(),
            name: "Tape Garden".into(),
            duration_seconds: 12.8,
            channels: 2,
            bpm: Some(90.0),
            waveform: waveform(0.8, 320),
            changed_by_agent: false,
        },
    ]
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
fn demo_compositions() -> Vec<Composition> {
    let melody_notes: Arc<[Note]> = (0..32)
        .map(|index| Note {
            start: index as f32 * 0.5,
            length: if index % 4 == 3 { 0.42 } else { 0.28 },
            pitch: 55 + ((index * 5) % 17) as u8,
            velocity: 0.55 + (index % 4) as f32 * 0.1,
        })
        .collect::<Vec<_>>()
        .into();
    let drum_notes: Arc<[Note]> = (0..48)
        .map(|index| Note {
            start: index as f32 * 0.25,
            length: 0.12,
            pitch: [36, 42, 42, 38][index % 4],
            velocity: if index % 4 == 0 { 0.95 } else { 0.62 },
        })
        .collect::<Vec<_>>()
        .into();

    let root = Composition {
        id: "cmp_song".into(),
        name: "Glasshouse".into(),
        length_beats: 96.0,
        tracks: vec![
            Track {
                id: "trk_drums".into(),
                name: "DRUM PRINT".into(),
                kind: TrackKind::Audio,
                muted: false,
                solo: false,
                level: 0.82,
                max_visual_length: 24.0,
                clips: vec![
                    Clip {
                        id: "clip_kick".into(),
                        name: "Kick bed".into(),
                        start: 0.0,
                        length: 12.0,
                        gain_db: -2.0,
                        waveform: waveform(1.1, 320),
                        kind: ClipKind::Audio {
                            asset: 0,
                            sync: SyncMode::None,
                            source_bpm: None,
                        },
                        effects: vec![gain_effect("fx_kick_gain")],
                    },
                    Clip {
                        id: "clip_dust".into(),
                        name: "Dust Loop".into(),
                        start: 14.0,
                        length: 18.0,
                        gain_db: -3.5,
                        waveform: waveform(1.8, 360),
                        kind: ClipKind::Audio {
                            asset: 1,
                            sync: SyncMode::Repitch,
                            source_bpm: Some(110.0),
                        },
                        effects: vec![gain_effect("fx_dust_gain"), delay_effect("fx_dust_delay")],
                    },
                    Clip {
                        id: "clip_dust_b".into(),
                        name: "Dust Loop / B".into(),
                        start: 48.0,
                        length: 24.0,
                        gain_db: -4.0,
                        waveform: waveform(2.0, 420),
                        kind: ClipKind::Audio {
                            asset: 1,
                            sync: SyncMode::Stretch,
                            source_bpm: Some(110.0),
                        },
                        effects: vec![gain_effect("fx_dust_b_gain")],
                    },
                ],
                effects: vec![gain_effect("fx_drums_track")],
            },
            Track {
                id: "trk_synth".into(),
                name: "GLASS KEYS".into(),
                kind: TrackKind::Event,
                muted: false,
                solo: false,
                level: 0.72,
                max_visual_length: 16.0,
                clips: vec![
                    Clip {
                        id: "clip_keys".into(),
                        name: "Folded melody".into(),
                        start: 8.0,
                        length: 16.0,
                        gain_db: 0.0,
                        waveform: Arc::from([]),
                        kind: ClipKind::Event {
                            notes: Arc::clone(&melody_notes),
                        },
                        effects: Vec::new(),
                    },
                    Clip {
                        id: "clip_keys_b".into(),
                        name: "Melody variation".into(),
                        start: 40.0,
                        length: 16.0,
                        gain_db: 0.0,
                        waveform: Arc::from([]),
                        kind: ClipKind::Event {
                            notes: melody_notes,
                        },
                        effects: Vec::new(),
                    },
                ],
                effects: vec![gain_effect("fx_keys_gain"), delay_effect("fx_keys_delay")],
            },
            Track {
                id: "trk_chorus".into(),
                name: "CHORUS NEST".into(),
                kind: TrackKind::Composition,
                muted: false,
                solo: false,
                level: 0.9,
                max_visual_length: 19.0,
                clips: vec![
                    Clip {
                        id: "clip_chorus".into(),
                        name: "Chorus".into(),
                        start: 24.0,
                        length: 16.0,
                        gain_db: -0.8,
                        waveform: waveform(2.8, 360),
                        kind: ClipKind::Composition {
                            child: 1,
                            render: RenderState::Stale,
                            tail_beats: 2.5,
                        },
                        effects: vec![
                            gain_effect("fx_chorus_gain"),
                            delay_effect("fx_chorus_delay"),
                        ],
                    },
                    Clip {
                        id: "clip_chorus_render".into(),
                        name: "Chorus / lift".into(),
                        start: 64.0,
                        length: 16.0,
                        gain_db: -0.8,
                        waveform: waveform(3.3, 360),
                        kind: ClipKind::Composition {
                            child: 1,
                            render: RenderState::Rendering(67),
                            tail_beats: 3.0,
                        },
                        effects: vec![gain_effect("fx_chorus_lift_gain")],
                    },
                ],
                effects: vec![gain_effect("fx_chorus_track")],
            },
            Track {
                id: "trk_vocal".into(),
                name: "VOCAL AIR".into(),
                kind: TrackKind::Audio,
                muted: false,
                solo: false,
                level: 0.66,
                max_visual_length: 11.0,
                clips: vec![Clip {
                    id: "clip_vocal".into(),
                    name: "Vocal Air / reverse".into(),
                    start: 34.0,
                    length: 11.0,
                    gain_db: -5.5,
                    waveform: waveform(2.4, 280),
                    kind: ClipKind::Audio {
                        asset: 2,
                        sync: SyncMode::None,
                        source_bpm: Some(120.0),
                    },
                    effects: vec![gain_effect("fx_vocal_gain"), delay_effect("fx_vocal_delay")],
                }],
                effects: vec![gain_effect("fx_vocal_track")],
            },
        ],
        output_effects: vec![gain_effect("fx_song_output")],
    };

    let chorus = Composition {
        id: "cmp_chorus".into(),
        name: "Chorus".into(),
        length_beats: 16.0,
        tracks: vec![
            Track {
                id: "trk_chorus_drums".into(),
                name: "DRUM KIT".into(),
                kind: TrackKind::Event,
                muted: false,
                solo: false,
                level: 0.85,
                max_visual_length: 12.0,
                clips: vec![Clip {
                    id: "clip_chorus_drums".into(),
                    name: "Chorus kit".into(),
                    start: 0.0,
                    length: 12.0,
                    gain_db: 0.0,
                    waveform: Arc::from([]),
                    kind: ClipKind::Event { notes: drum_notes },
                    effects: Vec::new(),
                }],
                effects: vec![gain_effect("fx_kit_gain")],
            },
            Track {
                id: "trk_texture".into(),
                name: "TEXTURE".into(),
                kind: TrackKind::Audio,
                muted: false,
                solo: false,
                level: 0.65,
                max_visual_length: 16.0,
                clips: vec![Clip {
                    id: "clip_texture".into(),
                    name: "Tape Garden".into(),
                    start: 0.0,
                    length: 16.0,
                    gain_db: -7.0,
                    waveform: waveform(0.8, 320),
                    kind: ClipKind::Audio {
                        asset: 4,
                        sync: SyncMode::Stretch,
                        source_bpm: Some(90.0),
                    },
                    effects: vec![
                        gain_effect("fx_texture_gain"),
                        delay_effect("fx_texture_delay"),
                    ],
                }],
                effects: vec![gain_effect("fx_texture_track")],
            },
            Track {
                id: "trk_vocal_texture".into(),
                name: "VOCAL TEXTURE".into(),
                kind: TrackKind::Composition,
                muted: false,
                solo: false,
                level: 0.78,
                max_visual_length: 9.25,
                clips: vec![Clip {
                    id: "clip_vocal_texture".into(),
                    name: "Vocal Texture".into(),
                    start: 4.0,
                    length: 8.0,
                    gain_db: -2.0,
                    waveform: waveform(3.7, 240),
                    kind: ClipKind::Composition {
                        child: 2,
                        render: RenderState::Fresh,
                        tail_beats: 1.25,
                    },
                    effects: vec![gain_effect("fx_nested_gain")],
                }],
                effects: Vec::new(),
            },
        ],
        output_effects: vec![gain_effect("fx_chorus_output")],
    };

    let vocal_texture = Composition {
        id: "cmp_vocal_texture".into(),
        name: "Vocal Texture".into(),
        length_beats: 8.0,
        tracks: vec![Track {
            id: "trk_slices".into(),
            name: "SLICE SAMPLER".into(),
            kind: TrackKind::Event,
            muted: false,
            solo: false,
            level: 0.82,
            max_visual_length: 8.0,
            clips: vec![Clip {
                id: "clip_slices".into(),
                name: "Air slices".into(),
                start: 0.0,
                length: 8.0,
                gain_db: 0.0,
                waveform: Arc::from([]),
                kind: ClipKind::Event {
                    notes: Arc::from([
                        Note {
                            start: 0.0,
                            length: 0.4,
                            pitch: 60,
                            velocity: 0.8,
                        },
                        Note {
                            start: 1.5,
                            length: 0.6,
                            pitch: 64,
                            velocity: 0.65,
                        },
                        Note {
                            start: 3.0,
                            length: 0.8,
                            pitch: 67,
                            velocity: 0.9,
                        },
                        Note {
                            start: 5.0,
                            length: 1.2,
                            pitch: 72,
                            velocity: 0.72,
                        },
                    ]),
                },
                effects: Vec::new(),
            }],
            effects: vec![
                gain_effect("fx_slices_gain"),
                delay_effect("fx_slices_delay"),
            ],
        }],
        output_effects: vec![gain_effect("fx_texture_output")],
    };
    vec![root, chorus, vocal_texture]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_data_exercises_core_surfaces() {
        let vm = DemoViewModel::demo();
        let clips = vm
            .compositions
            .iter()
            .flat_map(|composition| composition.tracks.iter())
            .flat_map(|track| &track.clips);
        let mut audio = false;
        let mut event = false;
        let mut nested = false;
        let mut stale = false;
        let mut rendering = false;
        for clip in clips {
            match clip.kind {
                ClipKind::Audio { .. } => audio = true,
                ClipKind::Event { .. } => event = true,
                ClipKind::Composition { render, .. } => {
                    nested = true;
                    stale |= render == RenderState::Stale;
                    rendering |= matches!(render, RenderState::Rendering(_));
                }
            }
        }
        assert!(audio && event && nested && stale && rendering);
        assert!(vm.assets.iter().any(|asset| asset.bpm.is_some()));
        assert!(vm.compositions.len() >= 3);
        assert!(
            vm.compositions
                .iter()
                .all(|composition| !composition.output_effects.is_empty())
        );
        assert!(
            vm.compositions
                .iter()
                .flat_map(|composition| &composition.tracks)
                .filter(|track| track.kind == TrackKind::Event)
                .all(|track| {
                    !track.effects.is_empty()
                        && track.clips.iter().all(|clip| clip.effects.is_empty())
                })
        );
    }

    #[test]
    fn nested_navigation_and_breadcrumbs_are_bounded() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::EnterChild { track: 2, clip: 0 });
        assert_eq!(
            vm.breadcrumbs()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            ["Glasshouse", "Chorus"]
        );
        vm.apply(Intent::Back);
        vm.apply(Intent::Back);
        assert_eq!(vm.current_composition().id, "cmp_song");
    }

    #[test]
    fn selection_derives_all_context_editors() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::Select(Selection::Asset(0)));
        assert_eq!(vm.editor_kind(), EditorKind::Waveform);
        vm.apply(Intent::Select(Selection::Clip { track: 1, clip: 0 }));
        assert_eq!(vm.editor_kind(), EditorKind::PianoRoll);
        vm.apply(Intent::Select(Selection::Sampler { track: 1 }));
        assert_eq!(vm.editor_kind(), EditorKind::Sampler);
        vm.apply(Intent::Select(Selection::Effect {
            track: 0,
            clip: 1,
            effect: 0,
        }));
        assert_eq!(vm.editor_kind(), EditorKind::Effect);
    }

    #[test]
    fn transport_and_effect_actions_clamp_and_preserve_identity() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::SetBpm(500.0));
        vm.apply(Intent::Seek(500.0));
        assert!((vm.transport.bpm - MAX_BPM).abs() < f32::EPSILON);
        assert!((vm.transport.playhead - 96.0).abs() < f32::EPSILON);
        let id = vm.compositions[0].tracks[0].clips[1].effects[0].id.clone();
        vm.apply(Intent::MoveEffect {
            track: 0,
            clip: 1,
            effect: 0,
            delta: 1,
        });
        assert_eq!(vm.compositions[0].tracks[0].clips[1].effects[1].id, id);
        assert_eq!(
            vm.selection,
            Selection::Effect {
                track: 0,
                clip: 1,
                effect: 1
            }
        );
    }

    #[test]
    fn dropping_assets_creates_and_selects_the_exact_clip() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::AddAssetClip {
            asset: 1,
            beat: 6.0,
            track: Some(0),
        });
        vm.apply(Intent::AddAssetClip {
            asset: 2,
            beat: 10.0,
            track: Some(0),
        });
        let Selection::Clip { track, clip } = vm.selection else {
            panic!("dropped clip should be selected");
        };
        let selected = &vm.current_composition().tracks[track].clips[clip];
        assert_eq!(selected.id, "clip_drop_0002");
        assert!((selected.start - 10.0).abs() < f32::EPSILON);
    }

    #[test]
    fn audio_drop_creates_a_track_when_target_is_event_only() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::EnterChild { track: 2, clip: 0 });
        vm.apply(Intent::EnterChild { track: 2, clip: 0 });
        vm.apply(Intent::AddAssetClip {
            asset: 0,
            beat: 2.0,
            track: Some(0),
        });
        let Selection::Clip { track, clip } = vm.selection else {
            panic!("dropped clip should be selected");
        };
        assert_eq!(
            vm.current_composition().tracks[track].kind,
            TrackKind::Audio
        );
        assert!(matches!(
            vm.current_composition().tracks[track].clips[clip].kind,
            ClipKind::Audio { .. }
        ));
    }

    #[test]
    fn loop_advance_preserves_overshoot() {
        let mut vm = DemoViewModel::demo();
        vm.transport.playing = true;
        vm.transport.playhead = 95.0;
        vm.advance(1.0);
        assert!((vm.transport.playhead - 1.0).abs() < f32::EPSILON);
        vm.advance(100.0);
        assert!((vm.transport.playhead - 9.0).abs() < f32::EPSILON);
    }

    #[test]
    fn agent_highlight_fades_and_expires() {
        let mut vm = DemoViewModel::demo();
        vm.apply(Intent::SimulateAgentChange(10.0));
        assert!((vm.highlight_alpha("clip_vocal", 10.0) - 1.0).abs() < f32::EPSILON);
        assert!((vm.highlight_alpha("ast_vocal", 10.0) - 1.0).abs() < f32::EPSILON);
        assert!(vm.highlight_alpha("ast_loop", 10.0).abs() < f32::EPSILON);
        assert!(vm.highlight_alpha("clip_vocal", 11.2) > 0.45);
        assert!(vm.highlight_alpha("clip_vocal", 13.0).abs() < f32::EPSILON);
    }
}
