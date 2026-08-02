# GAW

Gabe's Audio Workstation is an agent-native, hierarchical DAW built around transparent JSON projects, immutable audio assets, recursive compositions, and ordered first-party processing stacks.

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
