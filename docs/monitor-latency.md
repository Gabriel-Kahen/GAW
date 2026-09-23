# Live monitoring latency

Use a release build for playing: `cargo run --release -p gaw-app -- ./projects/my-song`.
Open **LIVE FX** while Monitor is on. **AUTO** tries 64 frames first, followed by larger
sizes if opening the output fails; it requests the selected size on capture too. Explicit
buffer choices are preserved. Auto does not benchmark the chain or increase the buffer
in response to crackles: use **128** or **256** if the chosen chain needs more time.

The window separates observed input/output callback sizes, queued audio, and active FX
delay. These are not additive measurements of analog round-trip latency. Queue counters
include startup and recovery; continued increases while playing merit investigation.

## Changes

- **Both device buffers:** the audited Scarlett setup previously used 512-frame playback
  and 1024-frame capture callbacks (10.67/21.33 ms at 48 kHz). Probes successfully requested
  64 frames on both (1.33 ms each). Negotiated rates are also propagated to the live FX host.
- **Callback scheduling:** CPAL realtime priority is enabled. On Linux, if its promotion
  is unavailable, GAW tries the minimum FIFO priority using existing session permissions.
  Already realtime threads keep their policy. The actual Scarlett callback threads were
  verified at FIFO priority 1; no account or system service changes were needed.
- **Input handoff:** bounded packets replace per-sample queue operations. Partial packets
  are published at each callback, with no staging delay. Equal-rate capture skips
  unnecessary interpolation. Backlog is bounded using actual callback sizes; the queue
  retains extra room after timing jitter to avoid repeatedly dropping arriving audio.
  Each separate capture gap can add one callback of reserve, capped at four output callbacks
  and 256 frames (5.33 ms at 48 kHz); startup silence and a continuing outage do not inflate it.
- **Effect execution:** the chain swaps buffer roles without copying whole scratch arrays.
  Pitch shifting processes stable controls in blocks; saturation caches unchanged filter
  coefficients. Automation retains its original timing, and realtime processing remains
  allocation-free.
- **Avoidable effect delay:** the transient shaper no longer delays already-shaped audio
  by 32 frames (0.667 ms at 48 kHz). An enabled live pitch effect with zero shift or zero
  mix is omitted, as is a fully dry saturator. Editing the settings rebuilds the chain.
- **Explicit quality choices:** **REDUCE FX LATENCY** selects Draft pitch shifting and
  zero compressor/limiter lookahead. Oversampling, pitch amount, gain, and mix are preserved.
  This action changes sound/behavior and is optional.

## CPU measurements

Release measurements on the local Ryzen 7 3700X; these measure computation, not signal delay.

| Work | Before | After | Reduction |
| --- | ---: | ---: | ---: |
| Capture + dry monitor mix, 128 frames | 3.282 µs | 0.500 µs | 85% |
| Capture + one gain effect, 128 frames | 4.606 µs | 1.730 µs | 62% |
| Capture + 16 gain effects, 128 frames | 25.174 µs | 20.002 µs | 21% |
| Default DSP saturator, 64 stereo frames | 2.99 µs | 2.01 µs | 33% |
| Draft pitch shift +12, 64 stereo frames | 5.65 µs | 4.82 µs | 15% |
| Signalsmith pitch shift +12, 64 stereo frames | 21.49 µs | 19.99 µs | 7% |

The monitor benchmark measures 30,000 capture/mix callbacks after 1,000 warmup callbacks.
Reproduce it with:

```sh
cargo test -p gaw-audio --release monitor_callback_benchmark -- --ignored --nocapture
```

DSP figures are medians of five alternating before/after runs. Spectral pitch processing
is bursty: average execution time alone cannot establish that a large chain meets every
callback deadline. Detailed effect settings, delay measurements, and reproduction commands
are in the [DSP audit](monitor-effect-latency.md).

## Remaining delay

Nonzero pitch shifting intentionally retains its analysis window: Draft declares 25 ms,
Signalsmith 120 ms at 48 kHz. Shortening the window further compromises bass tracking.
Saturator/clipper oversampling adds 0.667 ms at 2x and 1 ms at 4x. Lookahead adds its
selected duration; a true-peak limiter retains a small filter delay even at zero lookahead.

The Scarlett's server-managed hardware periods/headroom are separate from GAW's callback
size. Requests for smaller hardware periods did not take effect on the already-open
device and were restored. Later, after the older streams closed, an explicitly routed
64-frame probe verified that the hardware reopened with 32-frame periods/headroom
on both sides, while requested hardware properties remained at their original defaults.
This is consistent with deriving small hardware periods when the device reopens; other
session changes prevent attributing the whole improvement solely to the app.
A future native duplex backend could reduce the separate ALSA
client queues, but requires implementation and routing validation. Raising the sample
rate or requesting 32 frames does not bypass this PipeWire ALSA plugin's 1.33 ms floor.

The [Linux investigation](monitor-linux-latency.md) records hardware probes, scheduling,
queue stress results, and the remaining backend work. No analog loopback measurement
was performed, so there is no measured total bass-to-headphones latency claim.

## Validation

`cargo test --workspace` passes 685 tests, with five manual benchmarks ignored.
`cargo clippy --workspace --all-targets -- -D warnings`, formatting, and whitespace
checks pass. Coverage includes callback phase changes, partial packets, concurrent
overflow accounting, publication races, bounded jitter recovery, rate negotiation,
buffer fallback, and allocation/deallocation-free live chain swaps and processing
with both active pitch engines. Device probes and their practical limits are
recorded separately in the Linux investigation.

The final explicitly routed Scarlett probe ran the actual monitor pipeline with
4x oversampled saturation for 60 seconds after a one-second startup baseline.
Input 2 → monitor → Scarlett stereo outputs was verified before the baseline and
every ten seconds through completion. Capture/output callbacks were 64 frames;
effect delay was 48 frames. Four 64-frame monitor underruns occurred in the first
ten measured seconds; the remaining 50 seconds had zero underruns, dropped frames,
or output backend errors. No dropped frames/output errors occurred over the whole
minute. This is a short stability observation after adaptive settling, not a
zero-glitch guarantee or analog latency measurement. Current session defaults
were preserved, and both hardware periods/headroom were 32 frames at start/end.

An earlier default-routed test had two brief gaps, but a session output change
made its hardware route uncertain. Compilation stress also produced more errors
on an earlier queue revision. Separate capture/playback scheduling remains a
backend constraint; clock drift has not been ruled out for independent devices.
Both debug and release application builds completed successfully.
