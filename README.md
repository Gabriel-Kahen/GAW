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
cargo run -p gaw-app
```
