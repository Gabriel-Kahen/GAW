# Monitor DSP latency audit

The effects' execution time and their intentional signal delay are different quantities.
Small callbacks cannot remove a pitch shifter's analysis window or a limiter's lookahead.
The live-input host reports the sum of active effects' declared delays separately from device
and software queue latency. These measurements do **not** measure Scarlett analog round-trip latency.

## Reproduce

```sh
cargo run -p gaw-dsp --release --example monitor_effect_latency
cargo test -p gaw-dsp
```

The example prepares all 21 DSP-constructor defaults plus pitch and catalog variants at 48 kHz, stereo, with
64-frame blocks: each block has a 1,333 µs real-time deadline. It measures declared signal delay,
the first impulse output above 1e-6, and mean/p99 execution time over 10 seconds of two-tone input
per effect. Timing includes processor validation and the clock calls. Run on an otherwise quiet
machine; these numbers are observations, not portable upper bounds or a replacement for xrun tests.

Results below are medians from five alternating before/after runs on a Ryzen 7 3700X, Rust 1.96.1,
release profile with thin LTO. The baseline used a temporary copy of the pre-optimization DSP
(including the existing Signalsmith integration), restoring per-sample pitch dispatch and the
original transient delay. The fractional ring-read boundary fix was retained in both to allow
the octave-up run to finish. No baseline checkout or project settings were changed.

## Results after optimization

| DSP-constructor default | Declared delay, frames | Delay, ms | Impulse onset, frames | Mean µs/block | p99 µs/block |
| --- | ---: | ---: | ---: | ---: | ---: |
| Gain | 0 | 0 | 0 | 0.63 | 0.78 |
| Stereo Tool | 0 | 0 | 0 | 0.34 | 0.42 |
| Filter | 0 | 0 | 0 | 1.67 | 2.06 |
| Parametric EQ | 0 | 0 | 0 | 1.11 | 1.38 |
| Compressor | 0 | 0 | 0 | 2.12 | 2.62 |
| Limiter | 150 | 3.125 | 150 | 3.07 | 3.74 |
| Gate | 0 | 0 | 0 | 1.31 | 1.61 |
| Expander | 0 | 0 | 0 | 2.02 | 2.47 |
| Transient Shaper | 0 | 0 | 0 | 1.69 | 2.07 |
| Saturator | 0 | 0 | 0 | 2.01 | 2.29 |
| Clipper | 0 | 0 | 0 | 1.75 | 2.15 |
| Bitcrusher | 0 | 0 | 0 | 1.18 | 1.28 |
| Delay | 0 | 0 | 0 | 2.58 | 3.02 |
| Reverb | 0 | 0 | 0 | 6.09 | 7.54 |
| Chorus | 0 | 0 | 0 | 11.15 | 14.30 |
| Flanger | 0 | 0 | 0 | 4.05 | 4.91 |
| Phaser | 0 | 0 | 0 | 7.48 | 9.33 |
| Tremolo/Autopan | 0 | 0 | 0 | 4.73 | 6.00 |
| Pitch Shift, Draft, zero shift | 1,200 | 25 | 1,200 | 4.27 | 5.80 |
| Rhythmic Gate | 0 | 0 | 0 | 1.33 | 1.63 |
| Beat Repeat | 0 | 0 | 0 | 2.30 | 2.96 |
| Pitch Shift, Draft, +12 | 1,200 | 25 | 600 | 4.82 | 5.68 |
| Pitch Shift, Signalsmith, zero shift | 5,760 | 120 | 5,760 | 18.00 | 386.72 |
| Pitch Shift, Signalsmith, +12 | 5,760 | 120 | 1,440 | 19.99 | 434.31 |

The app catalog uses different defaults for some effects. A separate release run with its
latency-bearing defaults measured:

| Catalog setting | Declared delay, frames | Delay, ms | Impulse onset, frames | Mean µs/block | p99 µs/block |
| --- | ---: | ---: | ---: | ---: | ---: |
| Limiter, 1 ms lookahead, true peak | 54 | 1.125 | 54 | 3.06 | 3.39 |
| Saturator, 8 kHz tone, 2x oversampling | 32 | 0.667 | 7 | 10.40 | 13.92 |
| Clipper, 4x oversampling | 48 | 1.000 | 23 | 23.61 | 31.83 |

These catalog variants are also included in the example. The approximate 33% Saturator CPU
saving above applies to the DSP default without oversampling; the additional FIR work in the
catalog's 2x setting reduces its relative benefit.

Pitch-shifted impulse onset depends on read-head position/windowing and is not a substitute for
reported alignment latency. FFT work is bursty, explaining the spectral processor's higher p99.
Delay/reverb/modulation defaults have immediate dry output; their later wet energy is intentional.
Filtering can change phase without introducing an explicit buffer delay.

## Changes with unchanged sound

- Stable pitch controls now process the available block together, rather than calling the engine
  once per sample. Signalsmith uses preallocated interleaved buffers and one FFI call per stable
  segment. No block is accumulated or delayed. The original sample-by-sample path remains during
  parameter smoothing. Mean Draft +12 time fell from 5.65 to 4.82 µs (about 15%); Signalsmith +12
  fell from 21.49 to 19.99 µs (about 7%). Spectral windows, dry/wet alignment and quality are unchanged.
- Saturator caches its tone coefficient while the smoothed cutoff is unchanged, avoiding two
  exponential calculations per frame. Its bias waveshaper is evaluated once per frame. Default
  execution time fell from 2.99 to 2.01 µs (about 33%). Automated cutoff changes still update
  every sample, and reset/reprepare invalidates the cache.
- Transient Shaper's 32-frame output delay served no lookahead purpose: its gain had already been
  computed and applied before entering the delay. Removing it saves 0.667 ms at 48 kHz and leaves
  the shaped waveform unchanged. An independent comparison against the original implementation
  checked 12,668 stereo frames with non-neutral settings and mid-block automation, bit-for-bit
  after aligning the old output by 32 frames. The host's normal compensation uses the new zero
  reported latency.
- The sustained octave-up benchmark exposed a fractional pitch-ring read that could panic when
  floating-point remainder rounded a tiny negative position to exactly the buffer length. The
  read now wraps that boundary to sample zero.

Tests cover mono/stereo pitch block equivalence through pitch and mix automation, unchanged
pitch dry/wet alignment and pitch accuracy, fractional boundary wrapping, immediate transient
output, saturator cache equivalence during automation and sample-rate changes, and allocation-free
processing/reset/seek. The full DSP suite and strict all-target clippy pass.

## Remaining latency and explicit tradeoffs

Signalsmith canonical pitch shifting adds 120 ms; Draft adds 25 ms at 48 kHz. The monitor host
can omit an immutable pitch snapshot with zero effective shift or zero mix and rebuild it when
controls change. Generic timeline DSP keeps stable declared latency for automation. Active
nonzero pitch quality should only change through an explicit user action. Shortening Draft's
50 ms window further would trade away low-frequency quality: one low-E bass cycle is about
24 ms, and low B is about 32 ms.

The DSP Limiter constructor adds 144 lookahead frames plus 6 true-peak FIR frames. The app
catalog starts at 1 ms lookahead: 48 + 6 frames, or 1.125 ms at 48 kHz. Setting its lookahead
to zero explicitly saves 1 ms while retaining true-peak protection and its 0.125 ms delay
(3 ms saved for the DSP-constructor setting). Compressor
lookahead is zero by default, but any user-selected lookahead also contributes to its delay.

Saturator/Clipper oversampling adds 32 frames at 2x and 48 frames at 4x (0.667/1 ms at 48 kHz).
These delays preserve wet/dry alignment and anti-alias quality; disabling oversampling changes
sound. An immutable Saturator snapshot at zero mix can be omitted by the live host without
paying that delay. Disabling a whole effect likewise avoids its processing and declared delay.
