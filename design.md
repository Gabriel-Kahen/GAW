# GAW Design

GAW stands for **Gabe's Audio Workstation**. It is an agent-native digital audio workstation centered on importing audio, chopping it, transforming it, arranging it, and recursively composing larger pieces from smaller compositions.

This document is the current source of truth for the product. It distinguishes the product model from implementation details so the interface and engine can evolve without changing what a project means.

## Design principles

### Agent-native, not agent-added

The project is a structured collection of JSON documents and media files. The GUI is a human-oriented projection of that structure, not the source of truth. An agent must be able to inspect and modify every meaningful part of a project without operating the GUI or reverse-engineering opaque application state.

Every entity has a stable ID. Names are user-defined labels and never carry program-defined musical meaning. For example, `Chorus`, `Intro`, and `Vocal Texture` are ordinary composition names, not built-in composition types.

GUI actions and agent actions use the same typed command system. Commands are validated, atomic, and undoable. The file format has a versioned schema, explicit units, relative paths, and explicit relationships.

### Hierarchical composition

A project is a hierarchy of compositions. A composition can contain a composition clip that refers to a child composition. Double-clicking the clip enters the child rather than opening an unrelated project. Breadcrumbs expose the current location, such as:

```text
Song / Chorus / Vocal Texture
```

Composition ownership is hierarchical and cycles are not allowed.

### Audio-asset-first evaluation

GAW treats audio assets as the universal result of musical computation:

```text
instrument(events, context)  -> audio asset
composition(contents, context) -> audio asset
process(audio asset, effects) -> audio asset
```

An audio asset is a logical audio-producing value, not necessarily a WAV file already written to disk. It can stream frames in real time, render in the background, or resolve to a cached file. Materialization is an execution and caching decision, not a change in the project model.

## Core model

### Primitives

GAW has three fundamental musical primitives:

1. **Audio asset**: immutable source or generated audio.
2. **Event data**: timed musical events, represented on the timeline by an event clip.
3. **Composition**: a recursively nestable musical timeline.

An audio clip is not a primitive. It is a timeline placement that references an audio asset. A composition clip is similarly a timeline placement that references a child composition.

```text
Primitives
|- Audio asset
|- Event data
`- Composition

Timeline placements
|- Audio clip       -> audio asset
|- Event clip       -> event data
`- Composition clip -> child composition
```

### Audio assets

An imported audio asset points to an immutable audio file. Chopping, reversing, repitching, stretching, fading, and processing never modify that file. They create descriptions of derived audio.

Audio assets can be:

- **Imported**: backed directly by an audio file.
- **Instrument-generated**: produced from events by an instrument.
- **Composition-generated**: produced by a nested composition.
- **Processed**: produced from another audio asset by transformations or effects.
- **Materialized**: currently available as a cached rendered file.

A generated asset has a stable logical identity. Each concrete render is an immutable revision identified by its content and evaluation context. Replacing a child composition's render means atomically pointing its logical output at a new revision; it does not mean overwriting an immutable render file.

The engine exposes the conceptual operations:

```text
AudioAsset.read(context, time_range) -> audio frames
AudioAsset.materialize(context)      -> cached audio file
```

### Audio clips

An audio clip describes how an audio asset is used on a timeline. It can contain:

- Timeline start
- Source start and duration
- Fades
- Reverse state
- Tempo synchronization
- An ordered clip effect stack

Many audio clips can reuse one asset without duplicating its source file.

### Event clips

Event clips contain timed notes and control events. `MIDI` is an import, export, or device protocol; internally the model uses explicit event data rather than treating an opaque MIDI file as the canonical representation.

An event clip does not produce sound by itself. An instrument transforms its events into an audio asset.

### Compositions and composition clips

A composition owns a timeline, tracks, event clips, audio clips, child composition clips, instruments, and internal processing.

At the parent level, a child composition exposes one finished mono or stereo audio output. The parent can:

- Place the composition clip on its timeline.
- Mute or unmute the placement.
- Apply an ordered effect stack after the child's output.
- Open the child composition for editing.

The parent cannot reach into the child to mix tracks, edit instruments, automate internal parameters, or otherwise manipulate its internals. Those changes require entering the child. GAW does not support multi-output routing; separate child compositions should be used when independently controllable groups are needed.

Each composition has an explicit musical length. Its rendered output can include an additional tail for note releases, delays, reverbs, and other decays. The main body and tail are visually distinct. Built-in processors report their required tail, subject to a finite project limit so feedback cannot create unbounded assets.

## Instruments

An instrument is strictly defined as a deterministic transformation from timed events into an audio asset:

```text
instrument(events, context) -> audio asset
```

The instrument consumes an event stream, not GUI objects or the event clip's JSON container. The context supplies the project tempo, sample rate, requested time range, channel configuration, and a stable random seed when needed.

The first built-in instrument is a sampler. Its configuration is transparent JSON and contains zones that map audio assets to notes or note ranges. The same sampler engine can represent:

- A drum kit, with a different asset on each note.
- A chromatic sampler, with one asset pitched across a range.
- A multisampler, with assets assigned by note and velocity range.
- A slice instrument, with different source regions triggered by different notes.

Initial sampler behavior should include:

- Source asset and source range
- Root note and note range
- One-shot and note-gated playback
- Gain and velocity sensitivity
- Attack and release
- Reverse
- Polyphony
- Choke groups

Instruments have no hidden state. Presets are reusable instrument JSON documents.

An event-based signal path is:

```text
Event clips
    -> instrument
    -> audio
    -> ordered audio effect stack
    -> output
```

## Effects and processing

GAW initially supports only built-in effects. Third-party plugin hosting is explicitly out of scope. This avoids opaque binary state, plugin scanning, crash isolation, incompatible interfaces, missing-plugin handling, and cross-platform plugin-format complexity.

Every built-in processor has a stable type, an enabled state, explicit parameters, documented units, defaults, and valid ranges. Its complete meaningful state is represented in JSON.

Effects are presented as a one-dimensional, top-to-bottom stack. Audio flows from the top of the stack to the bottom. Users and agents can insert, remove, bypass, reorder, and edit processors using the same command vocabulary. A node graph is not used for ordinary processing.

An initial built-in effect set may include:

- Gain and pan
- Fades
- Basic EQ and filters
- Compression
- Distortion or saturation
- Delay
- Reverb

Playback transforms such as source range, reverse, and tempo synchronization are shown before the reorderable effect stack. They are audio transformations, but they are not ordinary effects because they define how source time maps onto timeline time.

```text
Audio asset
    -> source range
    -> reverse
    -> tempo synchronization
    -> ordered clip effects
    -> composition
```

## Tempo and synchronization

The project has one shared musical tempo used throughout its composition hierarchy. Nested compositions are evaluated against that shared clock.

Every tempo-aware imported audio asset has at most one constant asset BPM. GAW does not model tempo drift or per-asset tempo maps. One-shot or non-rhythmic assets may have no BPM.

```json
{
  "bpm": 110,
  "first_beat_seconds": 0.0
}
```

`first_beat_seconds` identifies the first musical beat when the file contains leading audio or silence. This does not introduce variable-tempo warping.

The user can define asset BPM by:

- Entering it directly.
- Tapping tempo.
- Marking a region as a known number of beats.
- Accepting or correcting an automatically suggested BPM.
- Halving or doubling the current interpretation.
- Placing the first-beat marker on the waveform.

Agents use equivalent typed commands such as `set_asset_bpm` and `set_asset_first_beat`.

An audio clip chooses one tempo synchronization mode:

- **None**: preserve the asset's original duration and pitch.
- **Repitch**: change playback speed so asset beats match project beats; pitch changes with speed.
- **Stretch**: change duration so asset beats match project beats while preserving pitch.

For a constant project BPM and asset BPM:

```text
playback_ratio = project_bpm / asset_bpm
```

For example, an asset at 110 BPM in a 120 BPM project plays at approximately `1.0909x`. Repitch raises its pitch along with its speed; stretch preserves pitch.

The imported file remains unchanged. Tempo synchronization produces a derived logical audio asset that can stream immediately and be cached later.

## Project files

A GAW project is a directory, not one monolithic file. A starting layout is:

```text
project/
|- project.json
|- assets/
|  |- index.json
|  `- media/
|     `- <content-hash>.<extension>
|- compositions/
|  |- cmp_7f3a/
|  |  |- composition.json
|  |  |- tracks/
|  |  |  |- trk_12ab.json
|  |  |  `- trk_93cd.json
|  |  `- automation/
|  |     `- lane_31ef.json
|  `- cmp_8b21/
`- .gaw/
   |- recovery.journal
   `- cache/
      |- index.sqlite
      |- audio/
      `- waveforms/
```

Directory names use stable IDs rather than user names so renaming an entity never moves files or breaks references. Paths stored in project data are relative to the project root.

`project.json` contains the schema version, project identity, root composition ID, project BPM, internal sample rate, and project-wide settings. `assets/index.json` contains asset metadata and maps asset IDs to content-addressed media files. An imported file is copied into `assets/media/` by default using a copy-on-write clone when the filesystem supports it and a normal copy otherwise. GAW does not use external media references initially. The original filename is retained as metadata, but absolute source paths are not canonical project data.

Each `composition.json` contains the composition's name, length, mono or stereo output layout, ordered track IDs, and composition-output effect stack. Each track file contains that track's clips, optional instrument, and ordered track effect stack. Clips are embedded in their track file rather than split into thousands of tiny documents. Each automation lane has its own file so dense or frequently edited automation does not rewrite an entire track.

This granularity allows one composition to load without parsing the entire project while avoiding excessive filesystem overhead. Dense automation is represented as curves and segments, not sampled parameter values. If a single lane becomes unusually large, it can be chunked by time range without changing the logical schema.

The `.gaw/` directory contains only replaceable runtime state. Its SQLite index accelerates dependency lookup and cache access but is never canonical. Waveforms, analysis results, and rendered revisions are safe to rebuild.

All canonical JSON is strict JSON validated against a versioned schema. Quantities use explicit units such as beats, frames, seconds, hertz, and decibels. Writes are validated and atomically replaced.

## Undo and redo

GAW has undo and redo, but no creative version-history system.

The running application maintains a bounded in-memory stack of typed operations with enough before-and-after information to invert them. It also appends committed commands to `.gaw/recovery.journal` using group commits no more than 250 milliseconds apart. Canonical JSON snapshots are written after short idle periods and on explicit save. After a clean snapshot and close, the journal is removed. On an unclean restart, the user can recover commands newer than the last valid snapshot.

The journal is not a creative history feature and is never treated as canonical musical data. Persisted undo history does not survive a clean close.

One agent transaction is one undoable operation, even when it contains multiple coordinated commands.

## Rendering, caching, and invalidation

GAW evaluates audio assets lazily. Interactive playback can generate frames in memory; background work can materialize the same logical result into a cache.

When a dependency changes, downstream generated assets become invalid. The last valid render may continue playing while a replacement is generated. Render revisions are keyed by the asset definition, dependency revisions, render context, and audio-engine version.

The initial project uses one internal sample rate. Export at another sample rate converts the final output. Cached audio and analysis are disposable and garbage-collected when no longer referenced by the current project or undo state.

The cache uses content hashes and a derived SQLite index. Eviction is least-recently-used among unpinned entries. The default cache budget is 10% of the containing filesystem's capacity, clamped between 10 GiB and 100 GiB, and GAW begins aggressive eviction before free space falls below 5 GiB. The budget is user-configurable. Cache cleanup runs during idle time and never on the real-time audio thread.

## Human interface

The primary window has four working regions:

```text
+ Song / Chorus / Vocal Texture -------- Play -- 120 BPM ----+
| Assets       | Timeline                         | Inspector  |
|              |                                  |            |
| kick.wav     | Track 1  [audio waveform]        | Source     |
| loop.wav     | Track 2  [event notes]           | -> Trim    |
| vocal.wav    | Track 3  [child composition]     | -> Sync    |
|              |                                  | -> Effects |
+--------------+----------------------------------+------------+
| Context editor: waveform / piano roll / sampler zones       |
+-------------------------------------------------------------+
```

### Navigation

- Breadcrumbs show the composition hierarchy.
- Double-clicking a composition clip enters it.
- Back navigation returns to the parent.
- The transition should feel like zooming into structure rather than opening an unrelated document.

### Asset browser

Assets show a compact waveform, name, duration, mono or stereo layout, asset BPM, and synchronization status. Dragging an asset to the timeline creates an audio clip.

### Timeline

- Audio clips display waveforms.
- Event clips display miniature notes.
- Composition clips display their latest rendered waveform with distinctive nested-composition styling.
- A synchronized clip displays a compact status such as `110 -> 120 REPITCH`.
- Stale and currently rendering composition outputs are visible without obstructing editing.

### Inspector and stack

The inspector presents the complete ordered signal hierarchy for the current selection. Structural playback transforms appear first and reorderable effects follow. A composition clip inspector exposes only its child output and parent-level effects; internal child controls never leak into the parent.

### Context editor

- Audio selection opens a large waveform and chopping editor.
- Event selection opens a piano roll.
- Sampler selection opens its note and zone mapping.
- Effect selection opens detailed parameters.

### Agent visibility

Agent changes appear immediately in the GUI and briefly highlight affected objects. An optional structure view exposes stable IDs and JSON locations. The DAW does not need to become a chat interface; its primary agent surface is the shared project model and typed command system.

## Implementation direction

The current implementation direction is Rust for the project model, command engine, audio engine, built-in DSP, rendering, caching, persistence, and native GUI.

The audio callback must not parse JSON, perform filesystem access, wait on locks, or do unpredictable allocation. It consumes prepared state and communicates with other subsystems through bounded queues. Project persistence and asset rendering happen off the real-time thread.

A likely initial stack is:

- CPAL for low-level cross-platform audio I/O.
- `egui` for application UI and custom-painted timeline and waveform widgets.
- Serde for strict JSON serialization and deserialization.
- Rubato for high-quality Rust-native resampling and repitch playback.
- Signalsmith Stretch behind a small isolated C++ interoperability layer for pitch-preserving time stretch.

The implementation should be divided so the file model and typed command API remain independent of the chosen GUI framework.

Rubato's sinc resampler is used for canonical repitch playback and rendering. Signalsmith Stretch is selected because it is MIT-licensed, supports streaming and exact fixed-buffer stretching, and is designed for the modest stretch ratios typical of BPM matching. Its integration is hidden behind a Rust `TimeStretchEngine` interface so it can be replaced without changing the project model. Normal playback and materialization use the same canonical stretch configuration; a cheaper mode may be used only for scrubbing previews and is never cached as a final render.

GAW targets operating systems in this order:

1. Linux, using CPAL with PipeWire, ALSA, and JACK as available.
2. macOS, using CoreAudio through CPAL.
3. Windows, using WASAPI and eventually ASIO through CPAL.

Linux is the development and correctness target for the first usable release. Platform-specific audio code remains behind a narrow device-I/O abstraction so later ports do not affect the renderer or project model.

## Explicit non-goals for the initial design

- Third-party audio plugins
- Multi-output instruments or compositions
- Surround audio
- Node-graph effect routing
- Per-asset tempo drift or warp-marker maps
- Cross-boundary control of child composition internals
- Creative version history beyond undo, redo, and crash recovery

## Chosen implementation policies

- Canonical state uses project-, asset-index-, composition-, track-, and automation-lane-level JSON files.
- Imported media is copied into the project and addressed by content hash; external references are not supported initially.
- Repitch uses Rubato sinc resampling. Pitch-preserving stretch uses Signalsmith Stretch through an isolated interface.
- Undo is in memory; crash recovery uses a temporary grouped command journal that is removed after a clean close.
- Derived caches use content hashes, a noncanonical SQLite index, bounded least-recently-used eviction, and free-space protection.
- Linux is first, macOS second, and Windows third.
