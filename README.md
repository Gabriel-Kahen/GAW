# GAW

Gabe's Audio Workstation is an agent-native, hierarchical DAW built around transparent JSON projects, immutable audio assets, recursive compositions, and ordered first-party processing stacks.

## Live input monitoring

Click the sample-rate/buffer readout in the transport bar to open **Audio Settings**.
Choose an **Input Device** and **Input Channel**, enable **Input Monitoring**, and click **Apply**.
The selected mic or guitar input plays through both channels of the selected output device,
including while playback is stopped. **Monitor Level** controls its volume independently of
the project master volume; the **MONITOR ON/OFF** button in the transport bar toggles it quickly.

Monitoring does not record, create clips, change the project, or enter audio exports. Device,
channel, and level preferences are remembered; monitoring starts off when opening a project.
**Auto** starts with a 256-frame buffer for playback and requests the same size for capture,
then tries larger buffers if opening the output fails. At 48 kHz, 256 frames is 5.33 ms per
callback; total delay also includes the audio server, interface, queue, and effects.
Choose a larger buffer if you hear crackles. Explicit 32-, 64-, and 128-frame settings remain
available for lower latency. Use a release build for live playing.

With monitoring on, click **TUNER** beside the monitor meter to open the four-string bass tuner.
Play one open string at a time; it automatically follows standard **E1–A1–D2–G2** tuning
(A4 = 440 Hz), showing the detected frequency and cents flat or sharp. The center lights up
within ±5 cents. The tuner uses the selected input before Monitor Level, so lowering the
monitor volume does not affect detection. Turning monitoring off closes the tuner.

Click **LIVE FX** beside the monitor controls to build an effect chain for your live input.
Use **+ ADD EFFECT**, expand **PARAMETERS** to edit an effect, and use the arrows to change
the processing order. Each effect has an enable switch; **BYPASS CHAIN** lets you compare the
dry sound. The chain supports up to 16 built-in audio effects and is saved with your audio
preferences. Effects run before Monitor Level, independently of project playback; the tuner
continues to hear the dry input. Live effects do not enter the project or audio exports.
Turn off your interface's hardware **Direct Monitor** to hear only the software-processed sound.

The LIVE FX window includes buffer shortcuts and separate input/output callback, queue, and
FX delay readouts. **REDUCE FX LATENCY** switches active pitch shifters to Draft and removes
compressor/limiter lookahead; this trades some pitch quality and predictive dynamics for a
faster response. Neutral pitch effects and fully dry saturators automatically avoid their
unnecessary delay. Nonzero pitch shifting still has an analysis delay, even with small buffers.
See the [monitor latency report](docs/monitor-latency.md) for measurements and remaining limits.

## MIDI piano roll

Double-click a MIDI clip or the MIDI editor header to expand the piano roll into the middle
workspace, keeping transport and side panels available. Double-click the header again, click
**RESTORE**, or press **Esc** to return to the arrangement.

The piano roll opens in **DRAW** mode: click to place a note, or drag to set its length. New notes
reuse the last clicked or resized note's length; **LEN** resets it to the grid. A ghost note shows
where the next note will land. Drag a note to move it, or its right edge to resize it. Hold **Alt**
for unsnapped timing. Right-drag erases notes in one undoable gesture; **Ctrl-drag** selects a group.
**V/B** switch select/draw, arrows move selected notes, **Shift+Up/Down** transpose an octave, and
**Ctrl+D** duplicates the selected phrase after itself. **FIT** centers the notes and fits clip time;
scroll moves vertically, **Shift+scroll** pans horizontally, and **Ctrl+scroll** zooms.

## Computer keyboard piano and MIDI recording

Select a MIDI clip or sampler track and click **KEYS** in the MIDI editor, or press **Cmd/Ctrl+K**. The computer keyboard
plays that track's sampler even when transport is stopped. The piano panel shows the key mapping:
**A S D F G H J K L ;** are white keys; **W E T Y U O P** are black keys. **Z/X** shift down/up
an octave, and **Velocity** sets how hard new notes play. Hold multiple keys for chords.
The small selector switches between **Piano**, **4 × 12**, and **7EDO**. Both grids have four
octave rows, highest at the top; pitches rise from left to right. The chromatic grid uses these
physical key rows, with equal-sized cells and no black keys:

```text
`     1  2  3  4  5  6  7  8  9  0  -
Tab   Q  W  E  R  T  Y  U  I  O  P  [
Hyper A  S  D  F  G  H  J  K  L  ;  '
LShift Z X  C  V  B  N  M  ,  .  /  RShift
```

**7EDO** uses the first seven keys of each row, equally dividing each octave into seven steps
(1200/7 cents). Per-note tuning survives recording, editing, duplication, and MIDI export; MIDI
exports use separate pitch-bend channels for different tunings. **Page Up/Down** or the **−/+**
buttons shift either grid by an octave; Z and X play notes in grids. The octave label identifies
the lowest row. Caps/Hyper and the two Shift keys are read physically while the grid is active.
Desktop-reserved shortcuts may still take precedence over Hyper chords.

To choose a sound, click **SAMPLE** in the MIDI editor (or **Sampler** in the inspector).
The centered modal has a searchable audio list and **Import** for new files. Select audio, then
drag across its waveform or move the slice edges to trim it. **Play** or **Space** auditions the
selected region; click the waveform's top ruler to listen from that point. Scroll over the waveform
to zoom, Shift-scroll to pan, or use **− / +**, **Fit**, and **Slice**. **Done** returns to the MIDI editor. Trimming leaves the original audio untouched.
New sounds play across all keys, with C4 as the root and **Held** playback by default.
Changing notes shifts pitch while keeping the selected sample's length; **Held** still releases when you let go.
For an older sample mapped to a single note, click **All keys** beside Root to enable the full keyboard.
Use **+** for another layer; key ranges, envelopes, and other settings are under **Advanced**.

Select the destination MIDI clip and click **Record** (or the transport record button).
Recording starts at the cursor, or the clip start if the cursor is earlier, using the project tempo.
You can record freely with transport stopped, or start playback first to play along with the song.
Click **Finish** or Stop to add the notes; the clip grows as needed, and one Undo removes the
whole take. Notes stay editable in the piano roll and export through the existing MIDI exporter.
Timing is unquantized. Computer keys use fixed velocity, not pressure sensitivity.

Closing KEYS, changing selection, typing into a text field, losing app focus, changing tempo,
or stopping/seeking/looping playback finishes the take and releases held keys. **Esc** immediately
silences all voices and finishes recording. Existing editor shortcuts return when KEYS is closed;
modified shortcuts such as Ctrl+S still work. If a take cannot fit before another clip or conflicts
with overlapping gated notes, it stays in the panel for **Retry** after resolving the
conflict. Live audition includes sampler settings and processed source audio, track volume/mute/solo,
and master volume; it bypasses clip, track, and composition effects. Saved MIDI playback uses those
normal effect chains. For lower latency, use a release build and a small audio buffer.

## Audio-to-MIDI transcription

GAW can convert a materialized audio asset into editable MIDI event data with
[Spotify Basic Pitch](https://github.com/spotify/basic-pitch). Install the Basic Pitch CLI in the
environment used to launch GAW:

```sh
uv tool install --python 3.11 --with 'setuptools<81' basic-pitch==0.4.0
```

The setuptools pin works around Basic Pitch 0.4.0's use of the deprecated `pkg_resources` API.

Right-click an audio asset and choose `CONVERT TO MIDI`. GAW runs transcription in the background
and adds `<source> (MIDI)` to the Assets sidebar without changing the source audio. If the executable
is not on `PATH`, set `GAW_BASIC_PITCH` to its path before launching GAW.

Drag the resulting MIDI asset onto an event track to create a piano-roll clip. Dropping it elsewhere
creates a new event track with an empty sampler, ready for you to assign sounds. To export the
canonical notes as a Standard MIDI File, use
`gaw midi-export <project> <event-data-id> <destination.mid>`; find the event-data ID with `gaw inspect <project>`.

Basic Pitch's CSV represents pitch bends per detected note, while GAW's canonical event stream uses
one track-wide bend lane. GAW currently imports note pitch, timing, and velocity and omits those
per-note bends rather than merging overlapping bends incorrectly.

## X-LANCE stem splitting

GAW can split a materialized audio asset into the eight targets provided by
[X-LANCE MSR](https://github.com/ModistAndrew/xlance-msr): vocals, guitars, keyboards, bass,
synthesizers, drums, percussions, and orchestral elements. Select an audio asset and choose
`STEM SPLITTER…` in its context menu. The generated assets are added atomically under
`SPLIT - <original file name>` and retain immutable, content-addressed WAV storage.

The bundled integration currently supports Linux. It prefers an NVIDIA CUDA GPU, uses a dedicated
AMD ROCm runtime for gfx1010 cards such as the Radeon RX 5700 XT, and otherwise falls back to CPU
inference. CPU output uses the same model weights but can take roughly 40–50 minutes for all eight
stems from a 100-second clip on a Ryzen 7 3700X. On the first stem split, GAW uses `uv` to create a
pinned Python 3.12 runtime in its own application-data directory and reuses it across projects. Install
[`uv`](https://docs.astral.sh/uv/getting-started/installation/) once, or set `GAW_UV` to its
executable; no project-specific Python environment is needed. The initial setup downloads several
gigabytes and is shown as `INSTALLING X-LANCE…` in the asset sidebar.

The bundled adapter and hashed dependency lock pin the upstream code, Python packages, and checkpoint
revisions. On the first split it clones
the pinned X-LANCE source and downloads only the selected checkpoints from
[the official checkpoint repository](https://huggingface.co/chenxie95/xlance-msr-ckpt). A complete
eight-stem setup uses about 4.3 GB of model weights. Set `GAW_XLANCE_CACHE` to relocate the cache,
`GAW_XLANCE_REPO` to use an existing checkout, or `GAW_XLANCE` to replace the bundled adapter with a
compatible executable. Long recordings use a project-local staging area under `.gaw/xlance`; set
`GAW_XLANCE_PYTHON` to use an existing Python environment, `GAW_XLANCE_RUNTIME_ROOT` to relocate the
managed runtime, or `GAW_XLANCE_TIMEOUT_HOURS` to change the default six-hour job limit.
Set `GAW_XLANCE_DEVICE` to `cpu`, `cuda`, or `rocm` to override automatic selection, and
`GAW_XLANCE_CPU_THREADS` to tune CPU inference concurrency.

The product source of truth is [design.md](design.md).
The implementation map and shared agent contract are in [docs/architecture.md](docs/architecture.md).

## Workspace

- `gaw-core`: canonical domain model, IDs, time, commands, and validation
- `gaw-project`: project storage, media import, recovery journal, and derived cache
- `gaw-dsp`: instruments, effects, analyzers, and render-safe processor contracts
- `gaw-stretch`: safe single-owner Signalsmith Stretch backend
- `gaw-audio`: render graph, transport, scheduling, device I/O, and background rendering
- `gaw-app`: native `egui` application
- `gaw-cli`: structured agent and developer command-line interface

## Development

```sh
cargo test --workspace
cargo run -p gaw-cli -- --help
```

Launch the native app and choose or create a project from the project manager:

```sh
cargo run -p gaw-app
```

By default, GAW uses an existing `./projects` library when launched from a workspace that has one;
otherwise new projects live in `~/Documents/GAW Projects`. Set `GAW_PROJECTS_DIR` to choose a
different initial managed location. The app can also open project folders anywhere on disk and
remembers them in its project catalog.

## Agent usage

Before constructing an edit, inspect the machine-readable Draft 2020-12 schemas:

```sh
gaw schema transaction
gaw schema processor
```

Processor-bearing schemas include a top-level `x-gaw-processor-catalog` extension. It is the
authoritative catalog for defaults, exact numeric and unit-specific bounds, array limits, enum
choices, automation support, indexed band/step paths, and cross-field constraints to satisfy before
`gaw apply`.

Projects can still be created and opened directly from scripts:

```sh
cargo run -p gaw-cli -- create ./projects/my-song --name "My Song" --bpm 120 --sample-rate 48000
cargo run -p gaw-app -- ./projects/my-song
```

The app recovers a pending crash journal by default. Recovery can also be inspected or replayed
explicitly before startup, and startup can instead discard or reject pending recovery:

```sh
cargo run -p gaw-cli -- recover ./projects/my-song --dry-run
cargo run -p gaw-cli -- recover ./projects/my-song
cargo run -p gaw-app -- ./projects/my-song --recovery recover
cargo run -p gaw-app -- ./projects/my-song --recovery discard
cargo run -p gaw-app -- ./projects/my-song --recovery abort
```

Run the bundled non-persistent UI fixture with:

```sh
cargo run -p gaw-app -- --demo
```
