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

### Processing scopes and order

Effect stacks exist at four explicit scopes:

1. **Audio clip stack**: processes one placement of an audio asset.
2. **Composition clip stack**: processes one placement of a rendered child composition in its parent.
3. **Track stack**: processes the sum of all clips on one track. An event track's instrument output enters here.
4. **Composition output stack**: processes the composition's final mono or stereo mix. The root composition's output stack is the project master stack.

The complete order is:

```text
Imported audio path
    audio asset
    -> source range / reverse / tempo synchronization / fades
    -> audio clip stack
    -> track mix
    -> track stack

Event path
    event clips
    -> instrument
    -> track mix
    -> track stack

Child composition path
    child composition output
    -> composition clip stack in the parent
    -> track mix
    -> track stack

Composition output
    sum of tracks
    -> composition output stack
    -> mono or stereo audio asset
```

This ordering is fixed and inspectable. There are no hidden pre-effects, post-effects, or implicit master processors.

### Built-in processor contract

Every built-in effect implements the same real-time contract:

```text
prepare(sample_rate, maximum_block_size, channel_layout)
process(input_frames, output_frames, parameter_events)
reset()
latency_frames() -> integer
tail_frames()    -> finite integer or capped estimate
```

`process` performs no filesystem access, locking, JSON parsing, or unbounded allocation. Processor state is deterministic from its JSON, input audio, parameter automation, context, and fixed random seed where applicable.

The engine compensates for declared processor latency so tracks remain aligned. Tail declarations participate in composition-tail calculation. Feedback-based effects must report a finite capped tail even if their mathematical decay is unbounded.

Every processor declares whether it accepts mono, stereo, or both and whether it can turn mono into stereo. Ordinary processors preserve their input layout. Spatial processors such as stereo delay, chorus, and reverb may produce stereo from mono. Stereo-to-mono conversion is always explicit through the stereo utility; it never happens silently.

### Parameter model

All parameters are typed and introspectable. Their descriptors include a stable ID, value type, unit, default, valid range or choices, automation support, and display hints. Agents use stable parameter IDs rather than human labels.

```json
{
  "id": "fx_delay_01",
  "type": "gaw.delay",
  "processor_version": 1,
  "enabled": true,
  "parameters": {
    "time": {
      "unit": "beats",
      "value": 0.5
    },
    "feedback": 0.35,
    "mix": 0.2
  }
}
```

Time parameters explicitly distinguish beats from seconds. Gain uses decibels, frequencies use hertz, note intervals use semitones or cents, and normalized ratios use the range `0.0` through `1.0`. Parameters that can cause clicks are smoothed by the processor. Discrete mode changes are not automatable unless the processor explicitly supports a click-free transition.

Bypass is universal. Wet/dry mix is provided only when it is musically meaningful; it is not forced onto utilities such as gain, EQ, or limiting. Presets are named JSON parameter sets and contain no opaque state.

### Core utility and tone effects

| Processor | Purpose | Essential parameters | Latency and tail |
| --- | --- | --- | --- |
| `gaw.gain` | Basic level and panorama control | `gain_db`, `pan`, `pan_law` | No latency or tail |
| `gaw.stereo_tool` | Explicit channel and stereo-image operations | `balance`, `width`, `mid_gain_db`, `side_gain_db`, `swap_channels`, `invert_left`, `invert_right`, `output_layout` | No latency or tail |
| `gaw.filter` | Focused resonant filtering | `mode` (`low_pass`, `high_pass`, `band_pass`, `notch`), `cutoff_hz`, `resonance_q`, `slope_db_per_octave`, `drive_db` | No intentional latency; negligible filter decay |
| `gaw.parametric_eq` | General corrective and creative EQ | Up to eight ordered bands with `enabled`, `shape`, `frequency_hz`, `gain_db`, `q`, and `slope_db_per_octave`, plus `output_gain_db` | Minimum-phase; no intentional latency or tail |

`gaw.stereo_tool` is the only built-in processor that performs explicit mono downmixing, channel swapping, or polarity inversion. Mono-to-stereo panning by `gaw.gain` is allowed because panorama is an explicit user action.

### Core dynamics effects

| Processor | Purpose | Essential parameters | Latency and tail |
| --- | --- | --- | --- |
| `gaw.compressor` | Reduce dynamic range and shape envelopes | `threshold_db`, `ratio`, `attack_ms`, `release_ms`, `knee_db`, `detector` (`peak` or `rms`), `lookahead_ms`, `makeup_gain_db`, `mix` | Optional lookahead latency; no meaningful tail |
| `gaw.limiter` | Prevent peaks from exceeding a ceiling | `ceiling_db`, `release_ms`, `lookahead_ms`, `true_peak`, `input_gain_db` | Declared lookahead/oversampling latency; no meaningful tail |
| `gaw.gate` | Attenuate audio below a threshold | `threshold_db`, `hysteresis_db`, `attack_ms`, `hold_ms`, `release_ms`, `range_db` | No intentional latency or tail |
| `gaw.expander` | Increase dynamic contrast below a threshold | `threshold_db`, `ratio`, `attack_ms`, `release_ms`, `knee_db`, `range_db` | No intentional latency or tail |
| `gaw.transient_shaper` | Emphasize or suppress attacks and sustain | `attack_amount`, `sustain_amount`, `sensitivity`, `response_ms`, `output_gain_db` | Small declared analysis latency; short finite tail |

Dynamics processors initially analyze their own input. External sidechain routing is not part of the initial design. A compressor may include high-pass and low-pass filters for its internal detector without exposing a second audio input.

### Core distortion and degradation effects

| Processor | Purpose | Essential parameters | Latency and tail |
| --- | --- | --- | --- |
| `gaw.saturator` | Continuous harmonic distortion | `curve` (`soft_clip`, `tanh`, `asymmetric`, `fold`), `drive_db`, `bias`, `tone_hz`, `output_gain_db`, `mix`, `oversampling` | Oversampling latency when enabled; no tail |
| `gaw.clipper` | Explicit peak clipping and loudness shaping | `threshold_db`, `softness`, `output_ceiling_db`, `oversampling` | Oversampling latency when enabled; no tail |
| `gaw.bitcrusher` | Digital resolution and sample-rate degradation | `bit_depth`, `sample_rate_ratio`, `dither`, `jitter`, `mix` | No intentional latency or tail |

Saturation and clipping remain separate: saturation is a continuous color effect with wet/dry mixing, while clipping is a peak-management operation with a defined ceiling.

### Core delay and space effects

| Processor | Purpose | Essential parameters | Latency and tail |
| --- | --- | --- | --- |
| `gaw.delay` | Tempo-synchronized or free-time echoes | `time` (beats or seconds), `feedback`, `stereo_mode` (`linked`, `offset`, `ping_pong`), `stereo_offset`, `low_cut_hz`, `high_cut_hz`, `modulation_rate_hz`, `modulation_depth`, `width`, `mix` | Dry path has no latency; feedback creates a capped calculated tail |
| `gaw.reverb` | Algorithmic room and ambience generation | `algorithm`, `size`, `decay_seconds`, `pre_delay` (beats or seconds), `diffusion`, `damping_hz`, `low_cut_hz`, `high_cut_hz`, `width`, `early_reflections`, `mix` | Algorithm-dependent latency; reported decay tail |

Delay feedback is constrained below unstable values in the initial implementation. Reverb algorithms and their versions are explicit so a project renders consistently after engine upgrades.

### Core modulation effects

| Processor | Purpose | Essential parameters | Latency and tail |
| --- | --- | --- | --- |
| `gaw.chorus` | Create thickness using modulated short delays | `rate` (hertz or beats), `depth`, `base_delay_ms`, `voices`, `stereo_phase`, `feedback`, `width`, `mix` | Short modulation latency and capped feedback tail |
| `gaw.flanger` | Create comb filtering using very short modulated delay | `rate` (hertz or beats), `depth`, `base_delay_ms`, `feedback`, `stereo_phase`, `mix` | Short modulation latency and capped feedback tail |
| `gaw.phaser` | Create moving phase cancellation | `rate` (hertz or beats), `depth`, `center_frequency_hz`, `frequency_span`, `stages`, `feedback`, `stereo_phase`, `mix` | No intentional dry-path latency; short feedback tail |
| `gaw.tremolo_autopan` | Rhythmically modulate amplitude or panorama | `mode` (`tremolo`, `autopan`), `rate` (hertz or beats), `depth`, `waveform`, `phase`, `stereo_phase`, `smoothing` | No latency or tail |

Tempo-synchronized modulation stores its period in beats and follows the project tempo. Free-running modulation stores hertz. Every oscillator starts from a deterministic phase derived from timeline position, so seeking and offline rendering produce the same result.

### Core pitch and sample-creative effects

| Processor | Purpose | Essential parameters | Latency and tail |
| --- | --- | --- | --- |
| `gaw.pitch_shift` | Change pitch without changing duration | `semitones`, `cents`, `formant_mode`, `quality`, `mix` | Algorithm-dependent declared latency and short tail |
| `gaw.rhythmic_gate` | Apply a beat-synchronized amplitude pattern | `steps`, per-step `level`, `step_length_beats`, `attack_ms`, `release_ms`, `phase_offset_beats`, `mix` | No intentional latency or tail |
| `gaw.beat_repeat` | Capture and repeat a recent rhythmic slice | `interval_beats`, `slice_length_beats`, `repeat_count`, `gate`, `decay`, `pitch_step_semitones`, `reverse_probability`, `mix`, `seed` | Internal capture buffer; finite declared tail |

`gaw.pitch_shift` is distinct from tempo repitch. Tempo repitch is a privileged clip playback transform that changes pitch and duration together; `gaw.pitch_shift` is an ordinary reorderable effect that changes pitch while preserving timeline duration.

Probability never means nondeterminism. `gaw.beat_repeat` and any future stochastic processor store a seed and derive decisions from absolute musical time.

### Effect implementation order

The catalog is implemented in stages so a useful creative system exists early:

1. Gain and stereo tool, plus level-meter and oscilloscope diagnostics
2. Filter and parametric EQ, plus spectrum and stereo diagnostics
3. Saturator, clipper, and bitcrusher
4. Delay and algorithmic reverb
5. Compressor, limiter, gate, expander, and transient shaper
6. Chorus, flanger, phaser, and tremolo/autopan
7. Pitch shift, rhythmic gate, and beat repeat
8. Loudness meter and tuner

Effects that share DSP building blocks should reuse internal kernels without collapsing distinct musical operations into vague processor types. For example, saturator and clipper can share oversampled waveshaping infrastructure while retaining different parameter contracts and agent-visible intent.

### Built-in analyzers

Analyzers can appear in the same vertical stack but pass audio through unchanged. Their configuration is canonical; their measurements are ephemeral structured data available to both the GUI and agents.

| Analyzer | Measurements |
| --- | --- |
| `gaw.level_meter` | Sample peak, true peak, RMS, peak hold, clipping |
| `gaw.loudness_meter` | Momentary, short-term, and integrated loudness plus loudness range |
| `gaw.spectrum` | Configurable FFT spectrum, peaks, and spectral centroid |
| `gaw.oscilloscope` | Time-domain waveform and zero-crossing behavior |
| `gaw.stereo_meter` | Mid/side level, correlation, and stereo width |
| `gaw.tuner` | Fundamental pitch, note name, cents offset, and confidence |

Analyzer output is never mixed into audio, never creates a render tail, and never changes an asset's content hash. Analyzer configuration still participates in project state so the workspace reopens consistently.

### Deferred specialized effects

The following are coherent future first-party processors but are not required for the first complete effect set:

- Convolution reverb with an audio asset as its impulse response
- Dynamic EQ and multiband compression
- De-essing
- Granular processing
- Resonators and filter banks
- Frequency shifting and ring modulation
- Spectral freeze and spectral blur
- Noise reduction and source separation

These are deferred because they require substantially more DSP, analysis, latency management, or model design. They should not be approximated by misleading low-quality implementations merely to increase the effect count.

Playback transforms such as source range, reverse, tempo synchronization, and fades are shown before the reorderable effect stack. They are audio transformations, but they are not ordinary effects because they define how the source becomes the clip-level signal.

```text
Audio asset
    -> source range
    -> reverse
    -> tempo synchronization
    -> fades
    -> ordered clip effects
    -> composition
```

## Tempo and synchronization

The project has one shared musical tempo and time signature used throughout its composition hierarchy. Nested compositions are evaluated against that shared clock. Canonical timeline positions are measured in quarter-note beats; the time signature determines the beat unit and bar boundaries rather than changing that storage unit.

```json
{
  "bpm": 120,
  "time_signature": {
    "numerator": 4,
    "denominator": 4
  }
}
```

The numerator is between 1 and 32. The denominator is a power of two from 1 through 32. A bar occupies `numerator * 4 / denominator` canonical quarter-note beats, so 6/8 occupies three quarter-note beats. Projects created before time signatures were added load as 4/4.

The project metronome is a persisted on/off transport setting with a persisted monitoring gain. During interactive playback it produces one click per notated beat at the project BPM, with a distinct accent on the first beat of each bar. It is scheduled against the project sample clock so play, seek, and loop remain aligned. Right-clicking the transport `M` control exposes the gain slider. It is a monitoring aid only: metronome clicks are never part of a composition render, materialized asset, or export.

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
- Running **Detect tempo** and accepting or correcting its result.
- Halving or doubling the current interpretation.
- Placing the first-beat marker on the waveform.

Agents use equivalent typed commands such as `set_asset_bpm` and `set_asset_first_beat`.

**Detect tempo** analyzes overlapping windows across the complete asset and groups related half-, single-, and double-time estimates into octave-equivalent tempo families. Family distance is continuous across octave boundaries, and global family discovery is deterministic with no fixed limit on the number of groups. Every family has a bounded diameter, so intermediate transition estimates cannot chain two distinct sustained tempos into one group. A transition-aware sequence decoder classifies the whole asset at once, preserving sustained sequences such as A–B–C and A–B–A while suppressing isolated excursions. One ambiguous window may inherit a matching tempo supported on both sides; longer or conflicting ambiguity remains explicitly uncertain. A new region is proposed only when its pooled evidence remains dominant for a meaningful duration; its boundary is refined toward a nearby strong transient. Family confidence combines the evidence for all equivalent half- and double-time interpretations rather than treating them as competitors.

Detection has three explicit outcomes:

- **Stable**: one reliable tempo family describes the asset. The user chooses its half-, single-, or double-time interpretation before applying it.
- **Sections**: the full waveform is divided into reliable tempo regions and uncertain ranges. GAW highlights every section directly over the waveform, labels reliable sections with BPM and confidence, labels uncertain sections **No BPM detected**, and presents editable boundaries plus half-/single-/double-time choices for reliable sections.
- **Unreliable**: the evidence is weak, competing, gradually drifting, or too unstable to describe as a small set of constant-tempo regions. No BPM or split is applied.

Confirming a Sections result creates a separate canonical WAV audio asset for each detected range and assigns each result its selected constant BPM. Each detected range includes up to two seconds of source context on both sides, clamped to the source boundaries; neighboring derived assets may therefore intentionally overlap at section boundaries. Uncertain ranges are displayed but are not materialized automatically. The original asset is preserved. All new media and asset-index entries are materialized and committed atomically in one undoable command, so cancellation or failure cannot leave a partial split. Detection never introduces a tempo map: every confirmed result still obeys the one-BPM-per-asset rule.

The Asset Tempo modal can audition the untouched source audio before a split is accepted. Its waveform supports click-to-seek, displays a preview playhead, and offers play/pause, stop, and per-section audition controls for both detected and uncertain sections. Preview playback is temporary and exclusive: it does not create a timeline clip, apply tempo stretching, move the project playhead, or change the project's prior play/pause state. Closing the modal restores project playback exactly.

An audio clip chooses one tempo synchronization mode:

- **None**: preserve the asset's original duration and pitch.
- **Repitch**: change playback speed so asset beats match project beats; pitch changes with speed.
- **Stretch**: change duration so asset beats match project beats while preserving pitch.

When an asset with a known BPM is added to a timeline whose project BPM differs by more than display-rounding tolerance, GAW pauses the insertion and offers **Match Tempo and Repitch**, **Match Tempo**, or **Keep Original Tempo**. This applies equally to dragging and **Add to Timeline**. The choice belongs to the new clip and never changes the asset's BPM metadata. Both matching modes preserve the asset's musical beat count; **Match Tempo** preserves pitch while **Match Tempo and Repitch** changes pitch with playback speed. Original-tempo playback measures the unchanged source duration against the project clock. Assets without BPM metadata use original tempo without prompting, while matching-BPM assets remain synchronized without an unnecessary prompt. Cancelling creates no clip.

For a constant project BPM and asset BPM:

```text
playback_ratio = project_bpm / asset_bpm
```

For example, an asset at 110 BPM in a 120 BPM project plays at approximately `1.0909x`. Repitch raises its pitch along with its speed; stretch preserves pitch.

Tempo exactness is a rendering invariant, not a visual approximation. Detection uses a high-resolution onset analysis with sub-bin autocorrelation peak interpolation, so fractional beat periods are retained instead of being rounded to an analysis frame. The stored BPM and the render ratio remain floating-point values; matching playback uses one affine source-to-timeline mapping (`project_bpm / asset_bpm`) and rounds only absolute sample boundaries. It must not round each beat independently, because those per-beat errors accumulate into audible drift. The same ratio and boundary mapping are used by waveform previews, live playback, cached renders, and export.

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
|- events/
|  `- <event-data-id>.json
|- compositions/
|  |- cmp_7f3a/
|  |  |- composition.json
|  |  |- tracks/
|  |  |  |- trk_12ab.json
|  |  |  `- trk_93cd.json
|  |  `- automation/
|  |     `- lane_31ef.json
|  `- cmp_8b21/
|- presets/
|  |- samplers/
|  |  `- <preset-id>.json
|  `- effects/
|     `- <preset-id>.json
`- .gaw/
   |- recovery.journal
   `- cache/
      |- index.sqlite
      |- audio/
      `- waveforms/
```

Directory names use stable IDs rather than user names so renaming an entity never moves files or breaks references. Paths stored in project data are relative to the project root.

`project.json` is the strict project manifest. It contains the schema version, project identity, root composition ID, project BPM and time signature, internal sample rate, project-wide settings, and stable ordering/location metadata for canonical fragments. It does not embed bulk event, track, or automation payloads. `assets/index.json` contains asset metadata and maps asset IDs to content-addressed media files. An imported file is copied into `assets/media/` by default using a copy-on-write clone when the filesystem supports it and a normal copy otherwise. GAW does not use external media references initially. The original filename is retained as metadata, but absolute source paths are not canonical project data.

Each event stream has its own `events/<event-data-id>.json` document. Event clips reference those stable IDs, allowing intentional reuse while ensuring that a large piano-roll edit rewrites neither `project.json` nor unrelated event streams. Each `composition.json` contains the composition's name, length, mono or stereo output layout, ordered track IDs, and composition-output effect stack. Each track file contains that track's clips, optional instrument, and ordered track effect stack. Clips are embedded in their track file rather than split into thousands of tiny documents. Each automation lane has its own file so dense or frequently edited automation does not rewrite an entire track.

This granularity allows the manifest and one composition bundle to load without parsing the entire project while avoiding excessive filesystem overhead. A partial composition bundle is explicitly a structurally decoded view; only a full project load claims global cross-reference validation. Dense automation is represented as curves and segments, not sampled parameter values. If a single lane becomes unusually large, it can be chunked by time range without changing the logical schema.

The project-local `presets/` library stores strict, named sampler and effect preset documents. Presets contain no opaque state and are not canonical song fragments: saving or deleting a reusable preset does not change the current composition unless an explicit typed command applies it.

The `.gaw/` directory contains only replaceable runtime state. Its SQLite index accelerates dependency lookup and cache access but is never canonical. Waveforms, analysis results, and rendered revisions are safe to rebuild.

All canonical JSON is strict JSON validated against a versioned schema. Quantities use explicit units such as beats, frames, seconds, hertz, and decibels. Writes are validated and atomically replaced.

## Undo and redo

GAW has undo and redo, but no creative version-history system.

The running application maintains a bounded in-memory stack of typed operations with enough before-and-after information to invert them. It also appends committed commands to `.gaw/recovery.journal` using group commits no more than 250 milliseconds apart. Canonical JSON snapshots are written after short idle periods and on explicit save. After a clean snapshot and close, the journal is removed. On an unclean restart, the user can recover commands newer than the last valid snapshot.

The journal is not a creative history feature and is never treated as canonical musical data. Persisted undo history does not survive a clean close.

One agent transaction is one undoable operation, even when it contains multiple coordinated commands.

## Rendering, caching, and invalidation

GAW evaluates audio assets lazily. Interactive playback can generate frames in memory; background work can materialize the same logical result into a cache.

The accepted in-memory project model is the visual timeline's source of truth; canonical JSON is its durable representation. Timeline playback is identified by a monotonic generation, the exact project revision, and the currently visible composition ID. Every non-metronome timeline sample must come from a render artifact with that exact identity. An older render, approximate arrangement, or synthetic silent render must never be relabeled as current.

An accepted edit immediately publishes the new desired generation to the audio callback, independently of command-queue capacity. This suppresses older timeline audio and its old metronome configuration before another callback block can leak. The transport clock continues advancing while the new artifact is prepared, so the playhead never freezes merely because audio is unavailable. The worker publishes the page containing the playhead first, then incrementally adds forward and loop-anchor pages. It never waits for a large resident window before making current audio available.

Timeline snapshot, transport frame, play/pause state, loop, and metronome configuration activate together at an audio-block boundary. Completion in the background worker is not considered audible until the callback acknowledges the same generation. Late, unrequested, wrong-composition, and superseded completions are discarded. Asset-preview playback is an explicit, separate callback mode and cannot masquerade as canonical timeline audio.

When a dependency changes, downstream generated assets become invalid. Render revisions are keyed by the asset definition, dependency revisions, render context, and audio-engine version. Source media handles are reused across revisions. Tempo-synchronized audio is cached at asset-and-ratio scope before clip source-range slicing, so moving or trimming a clip does not restretch the complete asset.

The initial project uses one internal sample rate. Export at another sample rate converts the final output. Cached audio and analysis are disposable and garbage-collected when no longer referenced by the current project or undo state.

The cache uses content hashes and a derived SQLite index. Eviction is least-recently-used among unpinned entries. The default cache budget is 10% of the containing filesystem's capacity, clamped between 10 GiB and 100 GiB, and GAW begins aggressive eviction before free space falls below 5 GiB. The budget is user-configurable. Cache cleanup runs during idle time and never on the real-time audio thread.

## Human interface

The primary window is divided into three independent horizontal bands. The top **Forehead** contains project navigation and transport controls. The bottom **Chin** contains the context editor. Between them is the main workspace, divided into four columns from left to right: **Assets**, **Tracks**, **Timeline**, and **Signal**.

```text
+ Forehead: Song / Chorus / Vocal Texture -- Play -- 120 BPM -+
+-----------+----------+--------------------------+------------+
| Assets    | Tracks   | Timeline                 | Signal     |
|           |          |                          |            |
| kick.wav  | Track 1  | [audio waveform]         | Source     |
| loop.wav  | Track 2  | [event notes]            | -> Trim    |
| vocal.wav | Track 3  | [child composition]      | -> Sync    |
|           |          |                          | -> Effects |
+-----------+----------+--------------------------+------------+
| Chin: waveform / piano roll / sampler zones / parameters    |
+-------------------------------------------------------------+
```

The Forehead and Chin span the full window width and are vertically resizable independently of the middle workspace. Their drag limits preserve enough space for their contents and for a usable workspace between them.

Signal is horizontally resizable. Assets and Tracks share one fixed expanded width so their boundary and controls remain stable; each can still be collapsed and reopened. Each collapsible column has a useful expanded minimum width rather than shrinking into an unusable sliver. Timeline is the protected center of the application: it cannot be collapsed, and side-column sizing must always reserve a usable minimum width for it. Column resizing affects only the middle workspace; it must not move, scroll, or visually overlap neighboring columns.

**Signal** is the canonical name for the right column. It contains the selection inspector and ordered signal stack described below, so it is broader than an effects-only panel.

### Visual language

GAW uses a strictly achromatic interface: every surface, control, state, clip, meter, and highlight is rendered only in shades of gray. Hierarchy comes from luminance, contrast, labels, borders, waveforms, note marks, and texture rather than hue. The overall reference is the dense, restrained, professional hierarchy of Logic Pro, adapted to GAW's own structure rather than copied literally.

Rectangles remain sharp. Windows, menus, panels, clips, cards, buttons, fields, badges, meters, and selection outlines have square corners. Hover and active states use brighter neutral fills and borders; no toolkit-default blue, warning orange, error red, or colored hyperlink may leak into the interface.

### Navigation

- Breadcrumbs show the composition hierarchy.
- Double-clicking a composition clip enters it.
- Back navigation returns to the parent.
- The transition should feel like zooming into structure rather than opening an unrelated document.

### Asset browser

Assets show their name, duration, mono or stereo layout, and asset BPM without a miniature waveform or redundant synchronization label. Right-clicking anywhere in the asset sidebar exposes an `ADD AUDIO ASSET` action backed by the native file picker. Common WAV, MP3, FLAC, Ogg Vorbis, M4A/MP4, AIFF, and CAF sources are decoded in a bounded streaming pass and stored internally as deterministic mono/stereo 32-bit float WAV files. The project preserves the source filename as metadata, while identity and deduplication use the canonical WAV's content hash. Importing runs through the canonical content-addressed project store and remains undoable. Dragging an asset to the timeline creates an audio clip.

### Timeline

- Audio clips display waveforms.
- Imported-audio waveforms use signed min/max peaks decoded from the canonical WAV in a bounded background job. Clip views follow their source range, tempo mapping, fades, and reverse state; zoomed-out pixels aggregate every covered peak bucket so transients are not skipped. Results are reused in memory by immutable content hash.
- Event clips display miniature notes.
- Composition clips display their latest rendered waveform with distinctive nested-composition styling.
- Clips on one track never overlap. Moving, resizing, or inserting a clip into an occupied range packs it into the nearest available position while leaving other clips in place.
- Tracks are first-class selectable targets: clicking a track row, empty space in that track, or one of its clips lights the matching track column. Clip selection remains more specific for editing, while the track highlight follows it.
- New root compositions begin with an explicit 64-beat working length. The arrangement always paints at least a 64-beat grid and fills its visible viewport, including when it has no tracks. Dropping an asset into an empty or end-of-composition region creates an audio track when needed and extends the explicit composition length to the next bar in the same undoable transaction.
- Asset drags carry stable asset IDs rather than sidebar positions. The empty arrangement identifies itself as a drop target, and an active drag highlights the target with a snapped insertion marker.
- Primary-button dragging from empty arrangement space pans the timeline horizontally. A drag beginning on a clip, loop control, playhead, or incoming asset retains that editor interaction instead of panning.
- Adaptive bar, beat, and subdivision lines continue through the ruler and loop strip so musical positions remain vertically aligned at every zoom level.
- A synchronized clip displays a compact status such as `110 -> 120 REPITCH`.
- Stale and currently rendering composition outputs are visible without obstructing editing.
- Each track has a dedicated post-track volume control, persisted in the canonical track JSON and applied after that track's effect stack. The control is a compact gray meter with a white fill indicating level and direct click/drag editing; it is independent of the reorderable effects, mute, and solo controls.

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

Rubato's sinc resampler is used for canonical repitch playback and rendering. Signalsmith Stretch is selected because it is MIT-licensed, supports streaming, and is designed for the modest stretch ratios typical of BPM matching. Its integration is hidden behind a Rust `TimeStretchEngine` interface so it can be replaced without changing the project model. Interactive pitch-preserving stretch runs at full quality as one persistent stateful stream that generates immutable fixed-size pages continuously; the processor is never reset at a page boundary. An uncached seek deterministically replays that stream from the source start until the requested page, while a bounded page cache avoids repeated work for nearby playback. The same absolute range therefore produces the same samples regardless of read size, partitioning, or request order. Extreme expansions that require randomized phase handling use one continuous materialized pass instead. Canonical materialization and export use the same full-quality deterministic configuration. Live pages are disposable and are never cached as final renders.

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
- External sidechains and arbitrary send/return routing
- Per-asset tempo drift or warp-marker maps
- Cross-boundary control of child composition internals
- Creative version history beyond undo, redo, and crash recovery

## Chosen implementation policies

- Canonical state uses project-, asset-index-, composition-, track-, and automation-lane-level JSON files.
- Imported media is copied into the project and addressed by content hash; external references are not supported initially.
- Repitch uses Rubato sinc resampling. Pitch-preserving stretch uses Signalsmith Stretch through an isolated interface.
- Playback artifacts are keyed by generation, project revision, and visible composition; stale audio is suppressed immediately while the independent transport clock continues.
- Timeline state activates atomically in the callback, and playhead-first pages are published incrementally.
- Undo is in memory; crash recovery uses a temporary grouped command journal that is removed after a clean close.
- Derived caches use content hashes, a noncanonical SQLite index, bounded least-recently-used eviction, and free-space protection.
- Linux is first, macOS second, and Windows third.
