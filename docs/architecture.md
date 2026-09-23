# Architecture and agent contract

GAW has three musical primitives: immutable audio assets, explicit event data, and recursive
compositions. Clips place those values on a timeline; tracks and effect stacks describe how they
produce audio. The product requirements remain in [design.md](../design.md).

## Where behavior belongs

| Responsibility | Source |
| --- | --- |
| Canonical IDs, units, primitives, processor parameters | `crates/gaw-core/src/model.rs`, `processors.rs` |
| Typed edits and clip placement rules | `crates/gaw-core/src/command.rs` |
| Cross-reference and composition integrity | `crates/gaw-core/src/command/validation.rs` |
| Atomic rollback and bounded undo/redo deltas | `crates/gaw-core/src/command/history.rs` |
| Agent schemas and processor discovery | `crates/gaw-core/src/schema.rs` |
| JSON fragments, immutable media, journals and snapshots | `crates/gaw-project/src/` |
| Prepared playback, render dependency evaluation and device I/O | `crates/gaw-audio/src/` |
| Processor implementation and shared resampling policy | `crates/gaw-dsp/src/` |
| Isolated stretch backend | `crates/gaw-stretch/src/lib.rs` |
| UI state, selection and gesture dispatch | `crates/gaw-app/src/model.rs` |
| UI asset/clip/sampler edits and read-only projection | `crates/gaw-app/src/model/` |
| Application shell, inspectors and context editors | `crates/gaw-app/src/app.rs`, `app/` |
| Background persistence, playback and worker coordination | `crates/gaw-app/src/controller.rs` |
| CLI commands, argument parsing and JSON I/O | `crates/gaw-cli/src/` |

`ProjectViewModel` owns an accepted canonical `Project` and derived display values. Editor helpers
construct `gaw_core::Transaction` values and commit through the same command engine available to
agents. Display indexes, pixels, selection, meter readings and waveform caches are not project
identity or canonical musical state. Use stable IDs for external edits.

Channel layout is defined once in `gaw-core`; audio and DSP retain their existing public aliases.
The DSP crate forbids unsafe code. The shared sinc configuration keeps repitching and offline
render paths aligned. Analyzer adaptation is isolated from the project playback compiler without
changing analyzer calculations.

Effects have explicit clip, track, and composition-output ownership. Audio and event clips use the
same `ProcessorStack::Clip` address; event effects process the individual clip's instrument output
and release tail. Composition placements have their own stack after the child output. Signal's
scope controls expose independent stacks while preserving the more specific clip selection. The
root composition output is labeled Master, and its EQ shortcut always targets the root, even during
nested navigation. EQ uses one floating, resizable graphical panel at all scopes and the existing
`gaw.parametric_eq` model and DSP. Keeping EQ above the workspace leaves the bottom context editor
available for waveform, piano-roll, sampler, and auxiliary views. Closing the panel changes only UI
state. Its eight numbered band colors are UI identities; they do not change the canonical processor
format. See [clip-effects.md](clip-effects.md) for controls, DSP reuse and an agent transaction
example.

Live input uses its own monitor chain, persisted with audio preferences rather than in the
canonical project. The input worker prepares the same built-in DSP processors used by project
effects. A bounded queue transfers prepared chains to the output engine, and a retirement queue
returns replaced chains for destruction on the worker. The output callback owns mutable stereo
DSP state, processes input and effect tails independently of transport, then applies Monitor
Level and adds the result to project playback. The bass tuner taps dry capture before effects.
Effect edits preserve the capture stream; output recovery rebuilds for the negotiated sample
rate. Monitoring, live effects, and their tails never enter offline renders or exports.

## JSON remains authoritative

A saved project is a directory containing `project.json`, `assets/index.json`, event documents,
composition documents, track documents and automation documents. `gaw-project` maps the complete
canonical snapshot to those fragments, validates their relationships, and preserves explicit order.
`gaw inspect` returns the assembled project; that output is not the smaller on-disk manifest.

Source media is immutable and addressed by content hash. Generated audio still has a logical asset
identity and immutable render revisions. `.gaw/` contains replaceable runtime state, including the
cache index and recovery journal. Reusable presets are explicit JSON, not hidden processor state.

The non-persistent demo is loaded from `crates/gaw-app/fixtures/demo-project.json` using the same
canonical deserializer and validator. Its media paths are illustrative, not bundled recordings.
Synthetic waveform decoration is confined to demo initialization. Production waveforms must come
from matching source content; obsolete background results are discarded.

## Agent workflow

```sh
gaw schema project
gaw schema transaction
gaw schema processor
gaw inspect ./projects/my-song
gaw apply ./projects/my-song transaction.json
gaw validate ./projects/my-song
```

Use `-` as the transaction path to read JSON from standard input. Processor-bearing schemas include
`x-gaw-processor-catalog` with defaults, units, parameter bounds and cross-field constraints.
Rust clients can obtain the identical schemas from `gaw_core` without depending on the CLI or GUI.
Schemas aid construction; canonical validation still checks graph relationships and invariants.

One transaction is one atomic edit, including coordinated changes across multiple primitives.
Invalid or incomplete transaction JSON does not partially edit a project. For programmatic edits,
prefer typed transactions over GUI `Intent` values: intents describe gestures and contain display
indexes and floating-point UI inputs. The native controller observes accepted external project
changes and refreshes playback and display state.

Basic Pitch and X-LANCE are optional external computation workers. Their completed output becomes
ordinary event data or immutable audio through the project store. Temporary inference files,
processes and diagnostic logs never define a second project format.

## Review and verification

The September 2026 review retained the project schema version, CLI command vocabulary, and valid
input audio processing. It separated mixed responsibilities and removed duplicate path validation,
channel types, schema construction, session transaction execution, sinc settings, subprocess
handling, and demo construction through UI objects.

Regression coverage now includes positive-only unit schema bounds, manifest ownership, stale
session rejection, external transport settings, waveform content identity, generated mono assets,
malformed audio buffers, inference wrapper cancellation, bounded progress logs, and empty project
catalog roots. Existing tests cover JSON round trips, recovery, undo, CLI workflows, deterministic
rendering and allocation-free realtime paths.

The review passed all 568 Rust tests, strict workspace Clippy, formatting and whitespace checks,
plus 2 Python adapter tests in the installed X-LANCE runtime.

Run `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, and
`cargo fmt --all -- --check`. Python adapter tests require the X-LANCE runtime's numerical
dependencies. Automated verification does not replace a hardware playback/listening pass or a
full Basic Pitch/X-LANCE inference run.

Measured optimization results and repeatable benchmarks are in [performance.md](performance.md).
