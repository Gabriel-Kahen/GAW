# Memory review — September 23, 2026

This review found several avoidable sources of peak memory and retained data. The available
previous-boot journal contained no matching OOM-killer or GPU-fault entry, so these findings do
not establish the cause of the reported machine crash.

## Fixes

| Path | Previous behavior | Change |
| --- | --- | --- |
| MIDI clip preparation | Kept a full float audio buffer for every rendered event clip | Writes temporary WAVs and reads through bounded page caches; temporary files close with their sources |
| Sampler preparation | Materialized complete source recordings, even for short zones | Reads selected ranges, shares identical ranges, and rejects more than 256 MiB of selected sample data per instrument before allocating it |
| Keyboard sampler changes | Unbounded queues could retain project revisions and prepared instruments | Bounded queues plus one replaceable pending request |
| Asset preview | Each click launched another concurrent full-file decoder | One active decoder and one replaceable pending selection |
| Clip MP3 and CLI WAV export | Retained all rendered audio before writing output | Streams rendered pages; also drains MP3 encoder packets after each block |
| Waveforms | Worker cache kept removed assets for the controller's lifetime | Removes cache entries absent from the current project |
| Recovery journal | Loaded the complete journal, decoded every transaction, and reread all bytes on append | Scans one record at a time for metadata, append, checkpoints, startup, and replay |
| Stem splitting | Loaded every checkpoint in a stage concurrently; decoded whole files for setup and cleanup; copied the whole input just to pad the final chunk | Loads and releases one model per pass, inspects input metadata, streams output cleanup, and pads only the active inference batch |

At 48 kHz stereo, one hour of float32 audio is about 1.29 GiB. Avoiding full-duration copies is
therefore materially more useful than small allocation reductions in these paths.

## Verification and scope

Regression tests cover range-only sampler reads from a synthetic ten-hour source, oversized
sample rejection before reads, temporary event-render lifecycle, queued project retention,
waveform eviction, coalesced preview selection, page release and export equivalence, and
recovery-journal allocation.
Python tests cover bounded output reads, final partial inference batches, model release between
passes, output preservation on validation failure, and existing audio shape/format behavior.

A separate-process measurement of float32 stereo stem cleanup at 48 kHz measured **35.5 MiB
peak RSS** for both a **10-second** and a **600-second** input. This includes Python and NumPy;
it measures cleanup only, not neural inference.

Neural inference still holds full-song input/output arrays and can require substantial CPU/GPU
memory. Mixer pages with enabled effects replay from frame zero to preserve processor state;
their temporary mixing buffers can still grow with the requested timeline position. Streaming
exports bound retained pages, but do not solve that separate processor-history allocation.
Undo history is bounded by entry count rather than bytes. Canonical project documents,
individual journal records, and the explicit `pending_recovery()` inspection API can still be
large. The fixes do not impose a process-wide memory ceiling or prove that every workload fits
in available RAM. Disk-backed renders trade RAM for temporary disk space.
Individual previews still decode complete files with the existing 1 GiB limit. Generic
`compile_project` callers should select a disk-backed cache directory for large renders; the
default temporary directory may be tmpfs. Normal project-store playback uses `.gaw/cache/audio`.

Checks:

```sh
CARGO_BUILD_JOBS=2 cargo test --workspace --all-targets -- --test-threads=2
CARGO_BUILD_JOBS=2 cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
python -m unittest discover -s scripts -p 'test_gaw_xlance.py' -v
```

Run the Python tests in the existing X-LANCE environment with NumPy and SoundFile installed.
Full neural inference and physical-device listening are separate from these regression checks.

### Verification outcome

- Strict workspace Clippy, formatting, and whitespace checks passed.
- All 10 Python tests passed in the installed X-LANCE environment.
- The first workspace run exposed a test input that scrolled just above the equalizer's panel
  at a 320-pixel window width. Moving the simulated pointer into the scroll area fixed the test;
  no EQ layout change was needed.
- After that correction, the full workspace suite passed **799 tests**, with **26 ignored**
  benchmarks and no filtered tests. Every new memory regression passed.
