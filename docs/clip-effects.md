# Effects and EQ

Select an audio, event/MIDI, or composition clip on the timeline and use **+ EFFECT** in Signal.
Parametric EQ, Pitch Shift, Distortion and Bitcrusher lead the menu; **More effects** contains the rest of the
built-in catalog. Click a non-EQ effect card to edit its main controls in the bottom context
editor. **ADVANCED** exposes the remaining canonical parameters. EQ opens in a floating panel
over the upper workspace so the bottom context area can stay focused on waveforms, piano roll,
sampler zones, and other supporting information. Signal supports bypass, reorder and remove;
these edits participate in undo/redo. Long stacks scroll below the Signal header.

Signal provides **Clip / Track / Master** scope buttons in the root composition and
**Clip / Track / Output** inside a child composition. Selecting a clip initially shows its own
stack; selecting a track header shows the track stack. Switching scope preserves the clip
selection and edits an independent stack: it never moves or copies processors. The scope and
owner name stay visible in the editor. The **EQ** shortcut beside the master fader always opens
EQ on the root composition output, including while viewing a child composition.

Drag a parameter control or click its number to type an exact value. Pitch Shift starts at zero:
set Semitones to `+12` or `-12` for an octave up or down, and use Fine tune for cents
(100 cents = one semitone). Keep the effect ON and Mix above zero to hear processing.
Audition through timeline playback; asset-browser preview plays the source without a clip's effects.

Each clip owns an ordered `effects` array embedded in its track JSON. Effects process audio after
playback transforms, after that event clip's instrument voices, or after a child composition's
output. Moving the clip keeps the stack. Copying it gives its effects fresh IDs and remaps copied
automation, while preserving the source asset/event/composition reference. Editing an effect
never changes that referenced source.

| GUI name | Canonical type | Main controls | DSP implementation |
| --- | --- | --- | --- |
| Parametric EQ | `gaw.parametric_eq` | Graph, frequency, gain, Q, shape, slope, output gain | Existing GAW parametric EQ |
| Pitch Shift | `gaw.pitch_shift` | Semitones, cents, mix | Existing Signalsmith Stretch integration |
| Distortion | `gaw.saturator` | Drive, curve, tone, output, mix | Existing GAW waveshaper |
| Bitcrusher | `gaw.bitcrusher` | Bit depth, sample-rate ratio, mix | Existing GAW resolution/rate reduction |

Signalsmith is an [MIT-licensed streaming pitch/time library](https://github.com/signalsmith-audio/signalsmith-stretch).
New GUI pitch effects explicitly store `quality: "signalsmith"`. The canonical default remains
`"draft"` so loading an older project does not change its algorithm. Signalsmith preserves clip
duration, reports latency for compensation, and aligns the dry signal for mixing. Both qualities
shift formants with pitch; neither currently preserves vocal formants independently.

## Graphical EQ

EQ is an ordinary ordered effect at clip, track, and composition-output scope. Its Signal card
opens the same floating, resizable panel everywhere, positioned near the top of the workspace.
The panel stays associated with the selected EQ while it is open and closes from its close button
or Escape; closing it does not bypass or remove the effect. The root output is labeled **Master**;
nested outputs show their composition name. Processing accumulates in order: clip EQ, track EQ,
then composition output EQ, continuing through any parent composition. No EQ is implicitly
inserted in every stack.

The Logic-inspired panel gives most of its space to a neutral frequency graph with eight numbered,
colored band slots. A band's node, individual response curve, and compact contextual controls share
its color; the total response stays neutral. Colors identify band slots rather than frequency
order, so moving a node across another band does not swap its identity. Numbers also identify
bands without relying on color. The header keeps the scope and owner visible beside whole-effect
bypass and close controls. Only the selected band's essential values are shown below the graph,
keeping the panel readable while preserving exact entry.

New GUI EQs start flat with eight bands: a disabled high-pass, a low shelf, four bells, a high shelf,
and a disabled low-pass. Drag a node horizontally for frequency and vertically for gain on shapes
that support gain; Shift-drag vertically to adjust Q. Click a node or its colored band button to
select it, then use the compact controls for exact frequency, gain, Q, shape, cut-filter slope,
enable/disable, or removal. Double-click empty
graph space to add a band, up to eight. Bypass the whole EQ to compare processing with the original
signal. The graph displays filter response, not a live spectrum analyzer. These controls use the
existing `gaw.parametric_eq` parameters and DSP; panel position, size, and band colors are
presentation state, not a new audio-processing format.
Each node or numeric-control drag is one undoable edit. Removing a band also removes its automation
and remaps later bands' automation in the same undoable transaction.

## Agent commands

Use `gaw schema processor` for types, versions, defaults, units, valid ranges and automation support.
Use `gaw schema transaction` for the complete command schema. All GUI effect edits use those same
typed transactions. Processor IDs must be unique throughout the project.

For audio and event clips, the stack address is `{"scope":"clip","track_id":"…","clip_id":"…"}`.
For composition placements, use `"scope":"composition_clip"` with the same ID fields. For example,
replace the two UUIDs below with IDs from `gaw inspect`, choose a unique processor ID, and apply
this transaction with `gaw apply ./my-project transaction.json`:

```json
{
  "label": "Pitch this clip down an octave",
  "commands": [
    {
      "type": "insert_processor",
      "stack": {
        "scope": "clip",
        "track_id": "00000000-0000-0000-0000-000000000001",
        "clip_id": "00000000-0000-0000-0000-000000000002"
      },
      "index": 0,
      "processor": {
        "id": "fx-pitch-octave-down",
        "processor_version": 1,
        "enabled": true,
        "type": "gaw.pitch_shift",
        "parameters": {
          "semitones": -12,
          "cents": 0,
          "formant_mode": "shift",
          "quality": "signalsmith",
          "mix": 1.0
        }
      }
    }
  ]
}
```

`update_processor` replaces the processor with the same ID, including its `enabled` state.
`reorder_processor` moves an entry by `from`/`to` indexes; `remove_processor` uses `processor_id`.
Preset insertion and application also work on all three clip kinds.

Automation for audio and event clip processors retains the historical `audio_clip_processor`
scope, addressed by composition, track, clip, processor and parameter IDs. For an event clip it
controls the audio after that clip's instrument. Composition placements use
`composition_clip_processor`. Automation and generated DSP state remain distinct: lane points
are canonical JSON; delay buffers, FFT state and caches are rebuilt for playback.

## Compatibility

Old event clips without an `effects` field load with an empty stack. Existing track and
composition-output stacks remain supported by the canonical model, commands and renderer.
Signal exposes these stacks through its scope buttons, including graphical EQ editing.
Existing EQ parameters keep their saved values; the flat eight-band layout is a new-GUI-EQ default.
The three musical primitives and fragmented JSON project format are unchanged.
