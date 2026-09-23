# Linux monitoring latency investigation

## Reproduce the read-only diagnostic

Run `python3 scripts/monitor-latency-report.py` while Monitor is enabled, and again
with the intended live effects enabled. It reads PipeWire metadata, nodes, links,
ALSA hardware parameters/delay counters, and thread scheduling. It never opens an
audio device, records audio, changes routing, or changes server settings. Use
`--match 'gaw|Device Name'` for other hardware and `--seconds 10` for a longer sample.
The report includes local device names and process IDs; review before sharing.

`node.latency` is a requested graph period, not an end-to-end measurement.
`clock.quantum` in metadata is the configured default; `pw-top` shows the active
driver quantum. A large ALSA ring's capacity is also not its actual queued delay.
Hardware `status` delay varies with sampling phase and omits GAW's input queue,
effects, client-side buffers, and potentially unreported converter delay. Never
add independently sampled maxima and present that as measured round-trip latency.

## Baseline observed on 2026-09-10

The running build used CPAL 0.17.3 through the PipeWire ALSA compatibility plugin,
PipeWire 1.6.7, WirePlumber 0.5.15, and a Scarlett Solo 3rd Gen at 48 kHz.

| Component | Observed configuration | Implication |
| --- | --- | --- |
| GAW playback client | `node.latency = 512/48000` | 10.67 ms requested period |
| GAW capture client | `node.latency = 1024/48000` | 21.33 ms requested period |
| Scarlett playback hardware | ALSA period 256, reported headroom 256 frames | Device/server delay persists after lowering client buffers |
| Scarlett capture hardware | ALSA period 512, reported headroom 512 frames | Capture has the larger hardware period/headroom |
| Both hardware rings | Capacity 32768 frames | Capacity alone does not imply 683 ms latency |
| USB transport | High speed, 125 microsecond data packet interval | USB packet interval is much smaller than the current app periods |
| GAW CPAL input/output threads | `TS`, no realtime priority | Callback scheduling can cause dropouts under CPU load |
| GAW PipeWire plugin data threads | `FF`, priority 83 | Plugin realtime scheduling does not promote CPAL's separate callbacks |
| PipeWire daemon data thread | `TS` | Server scheduling needs separate investigation if low-buffer xruns persist |

Routing was Scarlett Input 2 Inst/Line → GAW capture, and GAW playback → Scarlett
Headphones / Line 1-2. Input 2 is a mono source, with a split node between it and
the stereo hardware capture node. Input and output were driven by separate
hardware nodes. Their equal nominal rate does not guarantee callback alignment.
No routing, profile, scheduler, or system setting was changed for this audit.

## App-level opportunities

1. Request a small fixed period for **both** capture and playback during Monitor.
   Lowering playback alone leaves a 1024-frame capture callback in this baseline.
   Try 64 frames (1.33 ms at 48 kHz), then progressively larger supported periods, and
   fall back explicitly if a device cannot run the request. GAW's global Auto
   buffer setting now chooses the smallest supported candidate for playback,
   and monitoring inherits that chosen size. Explicit user buffer settings
   remain respected; this is not a separate monitor-only override.
2. Enable CPAL's `audio_thread_priority` feature. Its ALSA implementation promotes
   the actual callback thread at startup; the feature is absent in the baseline.
   Verify `cpal_alsa_in` and `cpal_alsa_out` with the report after rebuilding.
   On this machine RTKit is unavailable, so the feature alone failed. GAW also
   uses a safe, direct Linux scheduler fallback once per callback thread, using
   existing session permissions and the minimum FIFO priority. Already realtime
   threads retain their original policy and priority.
3. Bound the capture-to-output queue by the callback sizes actually observed.
   Drop stale backlog after scheduling stalls. Do not allow an underrun or a
   render stall to create a permanent multi-block delay. Separate devices may
   require clock-drift correction to avoid periodic starvation or overflow.
4. Keep effects in the output callback and eliminate avoidable block staging,
   allocations, locks, and recompilation there. Fixed plugin/convolution latency
   needs a separate measurement: smaller device periods cannot remove it.
5. Capture callback timestamps and playback timestamps can estimate callback
   age and output scheduling delay without recording the user's signal. Report
   this as an estimate, distinct from analog round-trip latency.

### What CPAL actually negotiates

In the installed `cpal-0.17.3/src/host/alsa/mod.rs`,
`set_hw_params_from_format` requests period `N` and ALSA client buffer `2*N` for
`BufferSize::Fixed(N)`. ALSA may negotiate a nearby size. `Default` initially
accepts the plugin period, then constrains its ring to twice that period.
`set_sw_params_from_format` uses playback start threshold `2*period` and
`avail_min = period`. Thus a 128-frame CPAL request is not a guarantee of
128-frame analog round-trip latency, but it removes substantial client buffering
relative to this baseline. Requesting only a period count can accidentally make
periods enormous in an ALSA plugin; always constrain the frame size as well.

PipeWire 1.6.7's `snd_pcm_pipewire_prepare` additionally clamps the period to
`64 * sample_rate / 48000`, then writes `node.latency` from that value. This
backend therefore has a 1.33 ms minimum client period: 64 frames at 48 kHz,
128 at 96 kHz. A 32-frame request or raising the rate alone does not bypass
that floor. A `node.latency` environment override is also overwritten here;
the ALSA stream configuration is the appropriate control point.
[PipeWire ALSA plugin source](https://github.com/PipeWire/pipewire/blob/1.6.7/pipewire-alsa/alsa-plugins/pcm_pipewire.c#L475-L511)

## Remaining Linux/backend work

Scarlett's hardware headroom/period is configured by the server, independently of
the app's ALSA client buffer. WirePlumber documents period and headroom tuning,
including extra buffering for USB batch devices. A device-specific, reversible
experiment should compare smaller periods and headroom against underrun counts;
do not assume zero headroom or disabling batch handling is stable. The displayed
runtime period/headroom may already include adjustments, so do not double-count
batch compensation. [WirePlumber ALSA documentation](https://pipewire.pages.freedesktop.org/wireplumber/daemon/configuration/alsa.html)

A native full-duplex PipeWire or JACK processing node can remove separate ALSA
client queues and process capture/effects/playback in graph order. This needs a
backend implementation and routing work. Simply enabling CPAL's JACK feature
still creates separate input/output clients. On the audited system `libjack.so`
belongs to **jack2**, with no `pw-jack` or `jack_iodelay` command installed; a
working PipeWire JACK setup cannot be assumed. PipeWire JACK's per-client
`PIPEWIRE_LATENCY` requests a maximum quantum, whereas `PIPEWIRE_QUANTUM` forces
the shared graph; avoid silently applying a graph-wide override.
[PipeWire JACK configuration](https://pipewire.pages.freedesktop.org/pipewire/page_man_pipewire-jack_conf_5.html)

Graph edges also matter: asynchronous links add a cycle, while synchronous
processing can traverse multiple effects in one cycle. Adding effects as
independent asynchronous nodes would therefore be counterproductive.
[PipeWire graph scheduling](https://pipewire.pages.freedesktop.org/pipewire/page_scheduling.html)

## Verify actual round-trip latency

Software impulse tests should measure the exact number of delayed frames through
GAW's queue and effects, covering dry, bypass, convolution, reorder, and rate
conversion cases. They isolate algorithmic delay but exclude hardware.

For analog latency, use a physical cable and a dedicated loopback measurement
tool such as `jack_iodelay`, with a known low-level test signal. First measure
the device/backend loop, then include the GAW monitor path and compare dry and
wet chains. Correctly route the measurement so the monitor output cannot feed
back into itself, and keep the test signal out of headphones. This audit did not
perform that test or inject any audio. Converter delay is often absent from
reported node latency unless calibrated; zero `ProcessLatency` is not proof of
zero converter delay. [PipeWire latency parameters](https://pipewire.pages.freedesktop.org/pipewire/group__spa__param.html)

For each buffer size, track app underruns/overruns and graph ERR deltas over a
sustained session with the heaviest intended FX chain, UI interaction, and CPU
load. Compare cold starts, toggling Monitor/FX, device reconnection, and normal
playback after Monitor is disabled. Select the lowest period that stays stable;
one clean startup is insufficient evidence of stability.

## Follow-up live probes

`cargo run -p gaw-audio --example monitor_latency_probe -- 64 10` opens bounded
48 kHz capture/playback streams for ten seconds. Playback is silence; capture
samples are discarded without storage. This actively opens devices and may
temporarily lower the attached graph's quantum, unlike the read-only report.
It prints actual callback frame sizes, driver timestamp estimates, callback
intervals, and error counts. Use a connected test device intentionally.

On the Scarlett, repeated 64-frame probes and 128-frame probes delivered their
requested callback sizes with zero CPAL underrun/error reports in ten-second
runs. The current user GAW process remained open at its older 512/1024 periods.
The probe followed the Scarlett's default **Input 1 Mic** source, while GAW's
existing live route stayed on **Input 2 Inst/Line**. Both sources share the same
stereo capture hardware. These tests verify device/backend negotiation and
scheduling; they do not validate a full bass-through-FX signal path under load.

Enabling CPAL's priority feature reported
`org.freedesktop.DBus.Error.ServiceUnknown` because RTKit was absent. Existing
session `RLIMIT_RTPRIO` was 99. With GAW's fallback, `ps -T` confirmed both probe
CPAL threads changed from normal `TS` to `FF` priority **1**, while their PipeWire
plugin data threads remained `FF` priority 83. No account permissions or
system services were changed.

Across short probe snapshots, physical playback delay medians fell from the
baseline 1788 frames (37.25 ms) to approximately 924–1102 frames (19.25–22.96 ms).
Capture delay varied widely with driver phase and competing graph activity.
These are hardware status counters, not full analog round-trip measurements.
There were occasional long callback intervals even with zero CPAL error reports;
an absence of reported xruns alone is not a stable-latency guarantee.

A reversible runtime experiment set Scarlett playback and capture parent node
`Props` to request `api.alsa.period-size=64` and `api.alsa.headroom=0`. Both
requests were accepted, but the already-open hardware retained its actual
256-frame playback / 512-frame capture periods and associated headroom. The
original requested values were both zero and were restored afterward, verified
by a fresh graph dump. No node/server was forcibly restarted, no device profile
was changed, and the user's routes/defaults were preserved. Hardware tuning
therefore remains unverified until it can be tested across a device reopen;
there is no demonstrated justification for persisting such a rule yet.

### Monitor queue probe

`cargo run -p gaw-audio --example monitor_pipeline_probe -- 64 15` exercises the
actual `RealtimeEngine`, live capture queue, output stream, and an X4 Saturator.
Monitor gain is zero after the FX stage, so processing runs while output remains
silent. It takes a one-second startup baseline and reports queue/drop/underrun
deltas over the requested measurement interval. Compare with `128 15`.

Initial fifteen-second runs exposed occasional monitor queue starvation even
though CPAL reported no output stream errors: 512 underrun frames at 64-frame callbacks,
and 1408 at 128-frame callbacks. This is why a backend-only silent probe is
insufficient to approve the queue policy. Larger buffers alone did not eliminate
the behavior; queue burst handling and callback phase need validation together.
The X4 Saturator reported 48 frames of algorithmic delay (1 ms at 48 kHz).
The production input handler does not count transient backend underruns, so the
pipeline probe's `input error=None` does not establish zero capture backend xruns.

### Default-device sustained result

After bounded adaptive jitter buffering, the final release build ran the actual
64-frame pipeline for **60 seconds after the unchanged one-second warmup**, with
the X4 Saturator active and all compilation/benchmarks paused. Input and output
callbacks were exactly 64 frames, the effect reported 48 frames of latency, and
the largest sampled input queue was 128 frames.

**Routing limitation:** the final audit found the session's default output had
changed externally to `sink-sunshine-stereo` and the previously running GAW
process had exited. The probe uses CPAL defaults, and its route was not captured
during this final run. These counts are valid for the observed pipeline, but
cannot establish that this sixty-second test remained on the Scarlett output.
Earlier short probes include graph snapshots confirming Scarlett routing.
The investigation did not switch the default sink or close GAW, and the final
external changes were left intact.

| Measurement interval | Monitor underrun frames | Dropped input frames | Output backend errors |
| --- | ---: | ---: | ---: |
| 0–10 s | 0 | 0 | 0 |
| 10–20 s | 64 | 0 | 0 |
| 20–30 s | 0 | 0 | 0 |
| 30–40 s | 0 | 0 | 0 |
| 40–50 s | 0 | 0 | 0 |
| 50–60 s | 64 | 0 | 0 |

The two brief monitor gaps totalled 128 frames (2.67 ms spread across a minute).
This is a large improvement, but **not a completely dropout-free result**. The
adaptive reserve reacted to occasional callback phase/jitter changes; normal
PipeWire daemon scheduling remains an external constraint. Startup itself had
2624 underrun frames while streams were opening and synchronizing, excluded
only by the explicitly reported one-second baseline.

Earlier debug probes overlapping workspace tests were substantially worse:
64-frame monitoring counted 12480 underrun frames and 12 output errors over
15 seconds; 128 frames counted 3584 and one output error. Those runs used an
earlier adaptive queue revision and combine code differences with heavy CPU
load, so they are not a controlled debug/release comparison. The final policy
has not been revalidated under that load. Use larger explicit buffers when
needed and do not equate successful stream negotiation with proven stability.

### Final explicit Scarlett validation

To resolve the routing uncertainty, a subsequent **60-second** release probe
explicitly connected Scarlett **Input 2 Inst/Line** to the probe capture and
both probe output channels to Scarlett **Headphones / Line 1-2**. Only the
probe's three links were changed. The default Sunshine sink and other apps were
left intact. Link verification completed 0.272 seconds after process launch,
before the unchanged one-second warmup ended, and repeated every ten seconds
through 60.4 seconds immediately before stream cleanup.

| Measurement interval | Monitor underrun frames | Dropped input frames | Output backend errors |
| --- | ---: | ---: | ---: |
| 0–10 s after warmup | 256 | 0 | 0 |
| 10–60 s after warmup | 0 | 0 | 0 |

Both callbacks were exactly 64 frames; the X4 Saturator reported 48 frames of
latency. The largest sampled queue was 320 frames (6.67 ms), reflecting the
bounded adaptive reserve. The first ten seconds contained 256 missing monitor
frames in total (5.33 ms); **the remaining fifty seconds were clean by the
reported monitor and output-backend counters**. This supports stability after
settling for this tested chain and quiet session, not a guarantee for all loads
or zero capture-backend xruns. Startup counts at the one-second baseline were
1088 dropped and 1344 underrun frames; those are separate from the table.

Hardware now opened with **32-frame ALSA periods and 32-frame headroom on both
capture and playback**, verified at the start and end. The requested period and
headroom settings remained at their original zero/automatic values. The older
GAW process had exited externally before this run, allowing a device reopen.
It is reasonable to infer that the smaller client request then allowed the
automatic USB batch settings to shrink; no persistent hardware tuning was
needed. This was not a controlled restart experiment, so the inference remains
distinct from the directly observed hardware values.

A two-second hardware status sample during this verified route found capture
delay min/median/max **36/74/144 frames** (0.75/1.54/3.00 ms) and playback
**428/476/512 frames** (8.92/9.92/10.67 ms). Compare baseline medians of 1072 and
1788 frames respectively, while accounting for differing callback phase and
other session activity. These counters still are **not analog round-trip
latency** and must not be represented as such.

Final verification found identical default-device metadata before and after
the explicit probe, no remaining probe nodes, and unchanged zero/automatic
Scarlett period/headroom requests.
