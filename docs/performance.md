# Measured performance improvements

These measurements compare the code at `606c3c0` with the September 2026 optimization pass on the
same development machine. Fixtures are synthetic and use warm filesystem caches; they are not
end-to-end DAW latency guarantees. Each comparison uses the same build profile before and after.

| Workload | Profile | Before | After | Approximate speedup |
| --- | --- | ---: | ---: | ---: |
| Load a project containing 10,000 notes (1.87 MB event JSON) | Release | 691.65 ms | 18.60 ms | 37× |
| Commit a durable rename to that project | Release | 1,438.68 ms | 56.26 ms | 26× |
| Validate 10,000 selected piano-roll notes for one frame | Development, opt-level 1 | 22.30 ms | 0.40 ms | 56× |
| Install 128 waveform completions into 128 tracks / 1,024 clips | Development, opt-level 1 | 141.15 ms | 4.62 ms | 31× |
| Prepare a page with processor-history replay: 8 tracks × 64 stereo clips | Release | 44.02 ms | 5.51 ms | 8× |

Storage figures are medians of three runs. Piano-roll figures are per-call averages over eight
iterations. Waveform figures are medians of seven complete batches. Rendering was compared using
three alternating before/after binary runs, each reporting the median of five measured iterations
after warmup; the table shows the median batch result.

## Changes

- **Storage:** buffer file reads, deserialize canonical fragments from borrowed JSON trees, and
  remove the unused snapshot clone before applying storage operations. Validation, trailing-input
  rejection, write locking, recovery and atomic replacement are retained.
- **Piano roll:** build one set of live event indexes when checking a multiple-note selection,
  replacing a full note scan for every selected note. Empty and single selections keep the existing
  allocation-free path. Note identity, deletion and clip-switch semantics are unchanged.
- **Waveforms:** update only audio placements referencing the completed source. Avoid rebuilding
  unrelated notes, effects, selection and transport projections. Source hash checks remain in
  place, and completion preserves nested render status and existing waveform buffers elsewhere.
- **Background page rendering:** limit temporary clip audio and mixing to the active source window
  when clip effects and latency compensation are absent. Track/master processing and history replay
  are unchanged. Enabled clip effects and compensated clips retain their existing path.

No project schema, agent transaction format, primitive, or realtime callback contract changes.

## Reproduce

These benchmarks are ignored during ordinary tests and print timings when explicitly requested:

```sh
cargo test -p gaw-project --release --test storage_scale -- --ignored --nocapture
cargo test -p gaw-app --lib piano_roll::tests::prepare_clip_selection_scaling -- --ignored --nocapture
cargo test -p gaw-app benchmark_waveform_completions -- --ignored --nocapture
cargo test -p gaw-audio --release benchmark_dense_page -- --ignored --nocapture
```

The storage test checks the loaded and committed project contents. Regression tests also cover
strict fragment decoding and buffered trailing data, note selection changes, source-specific
waveform updates across compositions, and bitwise page/full-render equivalence. Realtime tests
continue to check allocation-free callbacks.

Asset-list virtualization remains potential follow-up work. Immutable project sharing was
implemented in the further cleanup below.

## Additional cleanup and scaling measurements

The following measurements compare against the working tree at the start of the subsequent
cleanup, including the improvements above. All use the development profile (`opt-level = 1`)
on the same machine; these are isolated workloads, not end-to-end responsiveness guarantees.

| Workload | Before | After | Improvement |
| --- | ---: | ---: | ---: |
| Rebuild UI projection: 2,048 assets/tracks, 16,384 clips | 29.33 ms | 4.50 ms | 6.5× faster |
| Prepare 8 seconds of stereo audio with gain and pan automation | 68.13 ms | 32.06 ms | 2.1× faster |
| Look up one frame of meters across 1,024 tracks | 1.77 ms | 0.101 ms | 17.5× faster |
| Allocate for a volume edit on a track with 2,048 clips, including validation | 904,573 bytes | 517,667 bytes | 43% less allocation |
| Volume-edit allocation above validation alone | 311,926 bytes | 620 bytes | 99.8% less allocation |

- **UI projection:** build ID indexes once per rebuild instead of scanning canonical vectors for
  every placement. Canonical ordering, first-match behavior, missing-reference fallbacks, waveform
  identity, and note-window boundaries are preserved. Timings are medians of nine rebuilds.
- **Audio preparation:** reuse planar sample buffers and automation event storage across the
  existing 4,096-frame blocks. Events retain their parameter IDs and sample offsets; their values
  are updated in the same order. Outputs are cleared before each block, and final partial blocks
  receive only active samples and events. Timings are the median of three alternating before/after
  batch medians, with one warmup and seven measured preparations per batch.
- **Meters:** sort large private peak sidecars once during background preparation, then use binary
  search instead of rescanning every track for every meter. Up to 16 tracks retain linear lookup,
  which is faster for small lists. Stable sorting preserves first matches. The benchmark compares
  the previous linear lookup with the current lookup on the same data, averaging 1,000 frames.
- **Undo and validation:** volume edits store the previous volume instead of a complete track
  clone. Validation borrows processor IDs and formats duplicate diagnostics only when a duplicate
  exists. Allocation figures count requested bytes on the executing test thread, not peak resident
  memory. Undo, redo, rollback, and duplicate diagnostic order have regression coverage.

Audio regressions compare sample bits for automated gain, a stateful filter, channel conversion,
nonzero seek positions, empty input, and full/partial blocks. Meter regressions check unsorted track
IDs, missing IDs, peak-bin boundaries, page gaps, and unchanged rendered samples. The existing
allocation-free callback tests remain in the workspace suite.

```sh
cargo test -p gaw-app benchmark_project_projection -- --ignored --nocapture
cargo test -p gaw-audio --lib benchmark_automated_processor_preparation -- --ignored --nocapture
cargo test -p gaw-audio --lib benchmark_track_meter_lookups -- --ignored --nocapture
cargo test -p gaw-core --test history_scale -- --nocapture
```

## Further cleanup

These measurements start from the preceding cleanup's working tree and use the development
profile on the same machine.

| Workload | Before | After | Improvement |
| --- | ---: | ---: | ---: |
| Construct asset-browser rows: 5,120 audio/MIDI assets, 256 expanded folders | 40.35 ms | 0.529 ms | 76× faster |
| Validate a project with 2,048 composition clips | 815 µs | 447 µs | 45% faster |
| Allocation during that validation | 517,719 bytes | approximately 195,000 bytes | 62% less allocation |
| Compile 64 event clips sharing an 8-second stereo sampler asset | 194.23 ms | 181.83 ms | 6.4% faster |
| Obtain another worker snapshot of an unchanged project with 10,000 notes | 27.52 µs | 0.009 µs | Replaces a full clone with a shared reference |

- **Asset browser:** group rows by folder once, preserving unfiled-first ordering, audio/MIDI
  ordering, collapsed folders, and duplicate/missing-ID behavior. Folder indentation uses sets
  built once per frame and the same projected-ID parsing as before. Row timings are medians of
  nine runs of the old and new builders on identical inputs; they exclude painting and indentation
  membership-set construction. Regression tests compare exact row sequences and membership.
- **Validation:** keep dependency nodes as typed IDs and track active/done nodes in an ordered
  map. Node ordering matches the previous string keys; text is constructed only for errors.
  Tests compare exact cycle results against the previous traversal for every directed graph of
  four nodes (65,536 graphs). Validation timing is the median of seven batches of 200 validations.
- **Sampler preparation:** populate the materialization cache without returning unused sample
  copies, and transfer completed event-audio buffers into their frame sources. Processed assets
  still receive independent mutable samples. Timings are the median of five alternating batch
  medians, each with one warmup and five measured compiles; complete rendered-output checksums
  match. Tests cover exact sample bits, processed/source independence, and existing errors.
- **Worker snapshots:** the view model lazily shares one immutable project snapshot with playback
  and waveform jobs until the next accepted canonical edit. The first request still clones the
  project; repeat requests share it. One snapshot remains resident for reuse. Every projection
  refresh invalidates it, while existing jobs retain their original contents. This does not use
  the audio revision as a cache key, so metadata edits and saturated revision counters remain
  correct. Tests cover agent/UI edits, undo/redo, reloads, persisted imports/stem merges, and
  failed transitions. Timings are medians of nine batches of 1,000 repeat requests, excluding
  initial snapshot creation and worker execution.

```sh
cargo test -p gaw-app benchmark_asset_browser_rows -- --ignored --nocapture
cargo test -p gaw-core --test history_scale benchmark_clip_dependency_validation -- --ignored --nocapture
cargo test -p gaw-audio --lib benchmark_shared_sampler_asset_compilation -- --ignored --nocapture
cargo test -p gaw-app --lib benchmark_shared_project_snapshots -- --ignored --nocapture
```

## Timeline, compilation, and durable writes

Two further passes build on the working tree above. Measurements use the development profile
and synthetic fixtures on the same machine.

| Workload | Before | After | Improvement |
| --- | ---: | ---: | ---: |
| Build timeline rows for 2,048 tracks / 128 expanded groups | 1.75 ms | 0.247 ms | 7.1× faster |
| Calculate a drag update for 10,000 selected notes | 3.025 ms | 1.281 ms | 2.36× faster |
| Validate one 100,000-point automation lane | 294 µs | 189 µs | 36% faster |
| Compile a focused composition with 10,000 tracks | 103.65 ms | 22.85 ms | 4.54× faster |
| Write and sync 1,310,581 bytes of pretty JSON | 268.33 ms | 4.26 ms | 63× faster |
| Append and sync a 10,000-note recovery transaction | 228.93 ms | 2.64 ms | 87× faster |

- **Timeline rows:** index first group membership and collect expanded group contents once.
  Empty-group and small-track paths avoid unnecessary indexing. The original algorithm remains
  a test oracle for row order, duplicate membership, collapsed groups, invalid IDs, and row-boundary
  hit/drop targets. Small grouped layouts measured around 0.1 µs slower; large layouts benefit
  from removing repeated scans.
- **Note dragging:** combine four independent selected-note bounds reductions into one traversal.
  Each field retains its original reduction order and fallback; snapping, clamping, and generated
  updates are unchanged. Tests compare action fields bitwise, including signed zeros, nonfinite
  values, stale selections, and duplicate event IDs. Existing egui interaction tests also pass.
- **Automation validation:** use the ordered lane's final timestamp after lane validation, and
  check homogeneous units and parameter ranges once per lane. Simple parameter IDs remain
  borrowed, and compound sampler IDs no longer need a temporary vector. A retained validator
  oracle checks exact error precedence across valid and malformed lanes.
- **Compilation:** resolve track and composition IDs through indexes while preserving all
  iteration and mixing order. Tests include order-sensitive floating-point cancellation, the
  first missing-source error, and nested/focused compilation.
- **Durable JSON:** buffer serialization's small writes, explicitly flush the JSON bytes, then
  write the same newline and perform the existing file/directory syncs. Flush errors retain
  JSON-error classification. Atomic staging, validation, recovery, and rollback are unchanged.
  Byte tests include escaped Unicode, multiple buffer lengths, and partial serialization failure.
- **Recovery:** apply the same buffering to journal records, flushing before the commit newline
  and existing syncs. Reading iterates complete lines directly instead of collecting a temporary
  line vector. Every truncation position of a two-record journal is tested, along with whitespace,
  exact appended bytes, and repair of a torn tail.

Timeline and drag measurements are medians of nine runs of the old/new functions on identical
inputs. Automation validation uses seven batches of 100 calls. Compilation uses three alternating
before/after batches, each with one warmup and seven measured compiles, built with the same core
validation code. Pretty-JSON writes use five runs per implementation with byte comparisons.
Journal timings are medians of three alternating batches of five appends. Storage fixtures use
warm caches and still sync durable data; these are isolated operations, not full save/import or
audio-latency measurements.

```sh
cargo test -p gaw-app benchmark_timeline_group_layout -- --ignored --nocapture
cargo test -p gaw-app benchmark_selected_note_drag -- --ignored --nocapture
cargo test -p gaw-core --test history_scale benchmark_dense_automation_validation -- --ignored --nocapture
cargo test -p gaw-audio --lib benchmark_many_track_compilation -- --ignored --nocapture
cargo test -p gaw-project --lib benchmark_durable_json_writes -- --ignored --nocapture
cargo test -p gaw-project --lib benchmark_dense_journal_append -- --ignored --nocapture
```

## Bulk edits, asset catalogs, and snapshot hashes

The next pass keeps the preceding improvements and compares against that working tree in the
development profile.

| Workload | Before | After | Improvement |
| --- | ---: | ---: | ---: |
| Remove 5,000 alternating events from a 10,000-event vector | 7.701 ms | 0.058 ms | 132× faster |
| Remove the first 5,000 events from that vector | 13.577 ms | 0.033 ms | 405× faster |
| Compile 10,000 audio clips referencing 10,000 assets | 105.08 ms | 52.48 ms | 2× faster |
| Serialize an asset index with 128 assets / 2,048 render revisions | 3.716 ms | 3.106 ms | 16% faster |
| Hash a canonical-document fixture containing 100,000 notes | 39.62 ms | 24.09 ms | 39% faster |

- **Note edits:** keep the single-deletion path, drain contiguous deletions together, and compact
  scattered deletions in one stable traversal. Validation, updates, additions, final event sorting,
  and transaction construction retain their order. Regression tests cover deletion subsets,
  additions, duplicate indexes, mixed edits, invalid operations, and undo/redo. Removal timings
  exclude the event clone, final sort, and commit, so they do not represent total edit latency.
- **Folders:** compare edited folder lists against their untouched canonical source instead of
  cloning a second comparison copy. No-op detection and same-folder reorder behavior are tested.
- **Audio compilation:** reuse one asset-ID index and pass resolved assets into clip helpers and
  recursive source resolution. Mixing/traversal order and sample arithmetic remain unchanged.
  Tests cover bitwise processed/tempo audio, reordered assets, layout errors, and first failures.
  Each benchmark verifies all 20,000 output sample bits.
- **Asset serialization:** a private borrowed view serializes assets and folders without a deep
  clone. The public owned asset-index type and all decode paths remain unchanged. Tests compare
  exact JSON bytes, strict decoding, and full project round trips.
- **Recovery hashes:** serialize the same canonical bytes through a buffered SHA-256 writer
  instead of allocating a full serialized snapshot. The working serialization buffer is 8 KiB;
  hashing flushes it before finalization. Hashes match the previous algorithm for empty documents,
  numbers, escaped Unicode, and values crossing buffer boundaries. Existing journal/checkpoint
  hashes and durability behavior are preserved. Empty-fixture hashing measured about 0.13 µs
  slower, while large snapshots avoid the full serialized-buffer allocation.

Deletion and hashing timings are medians of nine runs on identical fixtures. Asset serialization
alternates old/new implementations over nine runs each. Audio compilation uses three alternating
batch pairs, each with one warmup and seven timed compiles against identical dependencies. These
are isolated workload measurements, not end-to-end GUI or hardware latency guarantees.

```sh
cargo test -p gaw-app benchmark_note_event_deletions -- --ignored --nocapture
cargo test -p gaw-audio --lib benchmark_many_audio_asset_compilation -- --ignored --nocapture
cargo test -p gaw-project --lib benchmark_asset_index_serialization -- --ignored --nocapture
cargo test -p gaw-project --lib benchmark_snapshot_hashing -- --ignored --nocapture
```

## Update bookkeeping, fragment ordering, and document transactions

This pass compares against the preceding working tree in the development profile.

| Workload | Before | After | Improvement |
| --- | ---: | ---: | ---: |
| Update 10,000 changed IDs against 10,000 highlights and assets | 444.442 ms | 1.865 ms | 238× faster |
| Find a selection across 2,048 tracks and 1,024 target-track clips | 116.642 µs | 11.382 µs | 10.2× faster |
| Reorder 512 track fragments | 214 µs | 102 µs | 2.1× faster |
| Reorder 4,096 track fragments | 5.14 ms | 1.13 ms | 4.6× faster |
| Build a transaction for one changed 10,000-note JSON document | 3.411 ms | 2.024 µs | Removes the deep copy |

- **Agent updates:** build changed-ID membership once for batches above 16 IDs. Preserve the
  original scan for small batches; 16-ID updates measured 1.764 µs before and 1.673 µs after.
  Tests compare highlight order, first-match handling of duplicates, asset flags, timestamp
  bits, and published update fields across both paths and all change sources.
- **Selection lookup:** format each target ID once outside search predicates, keeping exact
  string comparison and the first matching track, clip, and effect. Tests cover duplicates,
  missing targets, missing-effect fallbacks, and noncanonical strings.
- **Fragment ordering:** keep IDs and vector positions in tree nodes instead of full fragment
  payloads. Consume manifest ID iterators directly instead of collecting temporary vectors.
  An oracle compares 41,261 combinations, including duplicates, missing IDs, length mismatches,
  exact errors, and the resulting vector after failure.
- **Document transactions:** move changed paths and JSON values into write operations. Hashing
  and journal preparation still finish before those documents are consumed. Tests compare all
  operations and their order across 256 before/after document sets, and verify that a written
  string retains its original allocation. Existing recovery, checkpoint, and durability tests
  cover the callers.

UI timings use nine runs per implementation and UUID strings. Highlight fixtures update half
existing and half new IDs; timing excludes fixture cloning and update publication. Selection
fixtures place the target last. These timings do not measure a complete edit or UI frame.
Fragment timings alternate implementations over nine runs each, excluding fixture cloning.
Document-diff timings use nine runs per implementation, also excluding fixture cloning but
including disposal of the superseded input. This fixture replaces an empty event array with
10,000 notes; equal-length documents can require more equality-comparison work. The diff timing
excludes project encoding, validation, journaling, and file writes, so it is not a full-save
measurement.

```sh
cargo test -p gaw-app --lib benchmark_agent_highlights -- --ignored --nocapture
cargo test -p gaw-app --lib benchmark_selection_lookup -- --ignored --nocapture
cargo test -p gaw-project --lib benchmark_fragment_ordering -- --ignored --nocapture
cargo test -p gaw-project --lib benchmark_owned_document_diff -- --ignored --nocapture
```
