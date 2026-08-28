# GAW

Gabe's Audio Workstation is an agent-native, hierarchical DAW built around transparent JSON projects, immutable audio assets, recursive compositions, and ordered first-party processing stacks.

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
`gaw midi-export <project> <event-data-id> <destination.mid>`; the stable ID is shown in the asset
inspector.

Basic Pitch's CSV represents pitch bends per detected note, while GAW's canonical event stream uses
one track-wide bend lane. GAW currently imports note pitch, timing, and velocity and omits those
per-note bends rather than merging overlapping bends incorrectly.

The product source of truth is [design.md](design.md).

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

Create and open a persistent project:

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
