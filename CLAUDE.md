# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Keeping this file current

This project is evolving fast. Whenever a change alters something documented here — a new
crate/adapter, a changed `HostedPlugin`/core API, a new CLI flag, a roadmap phase completed, a
new real-time constraint or simplification — update the relevant section of this file as part of
that same piece of work, not as a follow-up. Treat a stale CLAUDE.md as a bug in the change.

## What this is

A cross-platform audio plugin host in Rust, optimized for live performance driven by a MIDI
keyboard — an "instrument rack" for a live set, not a DAW/timeline. Primary target is Linux,
with Windows/macOS as secondary goals. LV2 and VST3 are the hosted plugin formats so far (see
"Plugin format roadmap" below); a Slint GUI now exists alongside the original headless CLI.

## Dev dependencies

```shell
sudo apt install liblilv-dev lv2-dev libsuil-dev
```

- `liblilv-dev`/`lv2-dev`: LV2 plugin hosting (`livi`/`lilv`).
- `libsuil-dev`: hosting LV2 plugins' own native GUIs (X11UI embedding via `suil`).

Without these, `cargo check`/`cargo build` fails at a build script (`lilv-sys`'s or
`live-keys-lv2-ui`'s) with a `pkg-config` error, not a Rust compile error — if you see that, it's
this, not a code bug. VST3 needs no system packages: the `vst3` crate ships pre-generated
bindings (no Steinberg SDK at build time), and the editor host only needs `x11rb` + `libc`.

## Common commands

```shell
cargo check --workspace                        # fast type-check across the whole workspace
cargo build --release                          # plugin hosting is real-time audio; always test in release
# Note: the workspace's [profile.release] sets debug = true (keeps debug symbols in the
# optimized binary). Negligible perf/disk cost, and needed for useful gdb backtraces
# when diagnosing real-time-thread issues (e.g. a plugin's UI thread wedging).

# CLI (live-keys)
cargo run --release -- --list                  # list discovered instrument plugins (id, format, name)
cargo run --release -- --list-effects          # list discovered effects (id, MIDI/audio effect, format, name)
cargo run --release -- --list-midi             # list MIDI input port names
cargo run --release -- --list-audio            # list logical audio output devices
cargo run --release                            # run with defaults (Helm synth, first MIDI port, default output)
cargo run --release -- --plugin <id> --midi-port <substr> --audio-device <substr>
# <id> as printed by --list: an LV2 URI, or `vst3:` + class ID. `--plugin-uri` is an alias.
cargo run --release -- --plugin <id> --effect <id> --effect <id>   # MIDI effects → instrument → audio
                                               # effects; each --effect goes where its kind belongs

# GUI (live-keys-gui) — Settings tab (IO + plugin folders) and Live tab (3 side-by-side rack
# slots, each: up to 8 MIDI effects → an instrument → up to 8 audio effects — added through a
# plugin picker popup (tabs per kind, search, pinned favourites), removable, effects
# reorderable, any plugin's GUI and its parameter window openable, presets per plugin — and a MIDI
# channel/note-range/velocity-range filter, a volume fader, and slot presets: the whole chain
# with every plugin's settings, browsed by instrument)
cargo run -p live-keys-gui --release
# Settings persist to ~/.config/live-keys/config.json (audio/MIDI selection, extra plugin
# folders, pinned plugin ids, favourite preset ids, and per slot: last instrument id + MIDI/audio effect ids, MIDI
# filter, volume), loaded on startup. Slot presets live in ~/.local/share/live-keys/slot-presets/. Plugin ids are only recorded, not auto-loaded on startup; filters/volumes are applied.

# Manual LV2-UI-hosting smoke tests (not part of the app; see "LV2 plugin GUI hosting" below)
cargo run -p live-keys-lv2-ui --example test_helm    # External UI path
cargo run -p live-keys-lv2-ui --example test_surge   # X11UI path (needs Surge XT installed)
cargo run -p live-keys-lv2-ui --example test_setbfree --release -- [seconds] [plugin uri]
  # a plugin's GUI through a live engine (default setBfree, whose GUI uses atom ports only;
  # needs an audio device and a MIDI port); prints how many messages crossed each way

# Plugin check: one representative of every class of plugin with a known quirk (see
# crates/plugin-check/src/cases.rs), each in a child process: load, play a note (heard?),
# set a parameter and read it back (and hear it, where one is a volume), open the GUI (drawn?),
# resize it (does the widget follow?), close it and unload at once. ~2 minutes; windows pop up.
# Needs an X display, audio output and MIDI port; plugins not installed are skipped.
cargo run -p live-keys-plugin-check --release              # all cases; exit code 1 on a failure
cargo run -p live-keys-plugin-check --release -- vst3 lsp  # cases whose name contains a filter
cargo run -p live-keys-plugin-check --release -- --info <plugin id>   # GUI kind + params, for a new case
# Run it after touching plugin hosting or GUI hosting, and add a case when a plugin shows a new quirk.

# Manual VST3 smoke tests (not part of the app)
cargo run -p live-keys-vst3-host --example smoke --release   # scan, then render a chord offline
                                                             # through every instrument found
SMOKE_WAIT_SECS=10 cargo run -p live-keys-vst3-host --example smoke --release
  # ^ first processes silence at real-time pace for 10 s — sample-based plugins (e.g. HISE ones)
  #   only finish loading while they see `process` running, and render silence until then
cargo run -p live-keys-vst3-ui --example open_editor --release -- "surge xt" 10
  # opens a VST3 plugin's editor (name substring, exact match preferred) for N seconds

# Manual preset smoke tests (not part of the app; they create and delete a "Smoke Test" preset)
cargo run -p live-keys-lv2-host --example presets --release -- <lv2 uri>   # list/apply/save/delete
cargo run -p live-keys-engine --example presets --release -- [plugin id]...
  # the same through a live engine (needs an audio device and MIDI port), LV2 lending included

cargo test -p live-keys-core -p live-keys-engine -p live-keys-vst3-host   # the crates with unit
  # tests (MIDI routing, MIDI/audio effect-chain routing, .vstpreset round trip)
```

There is no lint/format CI configured in this repo yet. Run `cargo clippy --workspace --all-targets`
once a feature/request is otherwise complete and fix what it flags — not continuously on every
intermediate edit, which just burns time re-checking half-finished code.

## Testing philosophy

Keep it to small unit tests, and only for things that are genuinely non-trivial (real logic —
bit-packing, boundary conditions, parsing, a non-obvious invariant). Don't add a test for every
new function as a reflex; most of this codebase (adapters wiring an external API, CLI glue,
straightforward field plumbing) isn't worth a test on its own. When a test fails, look critically
at whether it caught a real bug or is just brittle/low-value before "fixing" it — a bad test is
worse than no test, and passing-by-construction tests (that can't actually fail on a real
regression) aren't worth keeping.

## Architecture

Cargo workspace with eight crates, isolating format-specific and UI-specific unsafe/FFI code
behind format-agnostic cores:

- **`crates/core`** (`live-keys-core`) — no system dependencies, compiles standalone. Defines:
  - `MidiEvent` (`src/midi.rs`) — fixed-size (3-byte) MIDI channel-voice message + frame offset.
    Fixed size is deliberate: it has to move through a lock-free ring buffer without allocation,
    so sysex/variable-length messages are dropped at `MidiEvent::from_raw`.
  - `ParamChange` (`src/param.rs`) — a parameter write request (`param_id` + `value`), carried
    the same way as `MidiEvent`. This is how a parameter slider or a plugin's own open GUI
    (turning a knob) reaches the audio thread.
  - `HostedPlugin` trait (`src/plugin.rs`) — the format-agnostic plugin abstraction:
    `process(midi_in, frames)` renders into the plugin's own internal buffers, then
    `audio_output(channel)` reads the result back. This split (render, then read) is intentional
    — it lets adapters hand real-time-safe *iterators* to their underlying plugin API instead of
    materializing a `Vec<&mut [f32]>`/slice-of-slices on every audio callback. Audio *input*
    mirrors that: `audio_input_count()`/`audio_input_mut(channel)` hand out the plugin's own
    pre-allocated input buffers for the host to fill before `process` (contents persist, so a
    host with nothing to feed must zero them). `midi_output()` (default: empty) is the MIDI
    the last `process` produced — a MIDI effect's output, fed to the next chain stage.
    `set_param` applies one `ParamChange`.
  - Format-agnostic metadata, also in `src/plugin.rs`: `PluginFormat` (`Lv2`/`Vst3`),
    `PluginKind` (`Instrument` = MIDI → audio, `MidiEffect` = MIDI → MIDI, `AudioEffect` =
    audio → audio and anything else), `PluginInfo` (`id`, name, format, kind — `id` is unique
    across formats: an LV2 URI, or `vst3:` + the VST3 class ID in hex), and `ParamInfo` (`id`,
    name, default/min/max, `kind: ParamKind`, `steps`, `logarithmic`, `unit`, `choices`).
    A `ParamInfo`'s id and values are in the format's own terms — an LV2 control-input port
    index and its real range, or a VST3 `ParamID` and its normalized `0.0..=1.0` — so the
    generic parameter editor needs no per-format logic. `ParamKind` (`Continuous`, `Toggle`,
    `Stepped`, `Choice`, `Note`) picks the control; `steps` counts the steps between min and max
    for stepped/note kinds; `choices` (`ParamChoice { value, label }`) are a choice's options,
    or labels for particular values of other kinds. `snap` moves a value onto one the parameter
    can take, `format_value` renders one as text (label, On/Off, note name via `note_name` —
    C4 = 60 — or a ~3-significant-digit number plus unit). Unit-tested.
  - `PresetInfo` (`src/plugin.rs`) — one preset of a plugin in its format's own storage
    (`id`: an LV2 preset URI or a `.vstpreset` path; `name`; `user`: saved by the user, so
    deletable). Presets are deliberately the formats' standard ones, not an app-specific
    format, so they're shared with Ardour/Carla/DAWs both ways.
  - `ring_buffer<T>()` (`src/ring.rs`) — generic thin wrapper over `rtrb::RingBuffer` (SPSC,
    lock-free); `midi_ring_buffer()`/`param_ring_buffer()` are the `MidiEvent`/`ParamChange`
    instantiations actually used.
  - `MidiFilter`/`MidiRouter` (`src/filter.rs`) — a slot's keyboard zone (channel or omni,
    inclusive note range, inclusive velocity range) and the fixed-size, allocation-free router
    that applies it on the audio thread. **Invariant:** note-offs are routed by tracking, not
    by the filter — the router keeps a `[u128; 16]` bitset of the (channel, note) pairs it
    passed note-ons for, and a note-off passes iff its note-on did. So changing a filter while
    keys are held never strands a note, and a note-off meant for another slot's zone never
    cuts this slot's voice. Velocity applies only to note-ons; poly aftertouch follows held
    notes; CCs/pitch bend/program change only check the channel. Unit-tested.

- **`crates/lv2-host`** (`live-keys-lv2-host`) — wraps `livi` (built on `lilv`) for LV2 plugin
  discovery/hosting. `Lv2Host` owns the `livi::World` and the shared real-time `Features`
  (URID map, block size bounds). `Lv2Host::instantiate` accepts plugins with audio input ports
  (e.g. effects, not just MIDI-only instruments) — see "Known simplifications" below for what
  feeds those inputs, since there's no real audio graph yet (see Roadmap). It also accepts
  plugins that write an atom-sequence or CV output port (e.g. an arpeggiator's MIDI-out, or a
  synth's combined MIDI/UI-state port — seen on Surge XT and the LSP samplers): each atom
  output is reset to an empty chunk before every `run` (as LV2 requires — the plugin learns
  its capacity from it), and afterwards its MIDI events are read back into a pre-allocated
  `midi_out` (`HostedPlugin::midi_output`) — that's how a MIDI effect's output reaches the
  next stage; for any other plugin it's simply ignored. Every event on an atom output (MIDI or
  not) also goes to the plugin's **GUI channel**: a plugin with atom ports gets a pair of
  `rtrb` rings of fixed-size `PortEvent`s (an atom body up to `PORT_EVENT_BYTES`, its type
  and port index; larger ones are dropped), whose control-thread end (`UiChannel`) is taken
  once with `Lv2Plugin::take_ui_channel`. `process` puts the GUI's atoms into the matching atom
  input at frame 0, ahead of the block's MIDI. Some GUIs talk to their DSP only this way —
  setBfree has no control ports at all: settings go GUI → `control` input, and its state and
  lit keys come back on `notify` (`ui:portNotification`). Atom buffers are
  `MIDI_EVENT_BUFFER_BYTES`, or a port's `rsz:minimumSize` if larger (x42-eq wants 4320). CV
  outputs go to a discarded scratch buffer. `plugin_kind` classifies at scan time: an instrument by class (or audio out + MIDI in
  and no audio in), a MIDI effect by having atom outputs (or the "MIDI Plugin" class) and no
  audio ports at all, an audio effect otherwise (onset triggers — audio in, MIDI out — included,
  since a MIDI chain has no audio to give them). `Lv2Plugin::process` pushes
  MIDI into a persistent `LV2AtomSequence` per atom-sequence input port — the same MIDI in each
  (most effects have none, unlike instruments) — and renders
  into persistent per-channel `Vec<f32>` buffers sized to `max_block_size` at instantiation — no
  allocation happens on the audio thread. Audio inputs read per-port `input_buffers` the host
  fills (`audio_input_mut`); CV inputs, which nothing routes, get a shared silent buffer.
  **GUI leases**: a plugin GUI with `instance-access` can still use the DSP instance while it
  closes on its own thread (Surge XT's does), so whoever opens one holds a `GuiLease`
  (`Lv2Plugin::gui_leases()`, a `Weak` the engine keeps in `PluginEditor::Lv2 { gui_leases }`)
  until the GUI is torn down, and `Lv2Plugin`'s `Drop` waits for its leases (the instance is a
  `ManuallyDrop`) — up to `GUI_TEARDOWN_TIMEOUT` (3 s), after which the instance is leaked rather
  than freed under a wedged GUI. Plugins are never dropped on the audio thread, so waiting is
  fine; it covers every way a plugin goes (removal, stream restart, app exit).
  `Lv2Plugin::params()` returns a `ParamInfo` (port
  index as id) for every control-input port, for a parameter-editor UI, read once at
  instantiation (`read_params`, via the raw `lilv` world/plugin since `livi::Port` lacks this
  metadata): `lv2:toggled` → toggle, `lv2:enumeration` + scale points → choice, the
  `units:midiNote` unit with a non-negative range → note (a negative range is a transposition:
  stepped), `lv2:integer` → stepped, `pprops:logarithmic` (min > 0) → log slider; the unit's
  `units:symbol` as `unit`; scale points as `choices`; `pprops:notOnGUI` ports left out;
  `lv2:sampleRate` bounds/default scaled to the sample rate. `min`/`max` always come back finite
  (an LV2 port with no declared range falls back to a span around its default) since a UI can't
  usefully expose an infinite slider.
  **Presets** (`preset.rs`): lilv's state API (`lilv_state_new_from_world`/`_restore`/
  `_new_from_instance`/`_save`/`_delete`) takes raw world/plugin/instance pointers that `livi`
  and the `lilv` crate keep private, so presets use a `PresetWorld` of their own — a raw
  `lilv-sys` world loaded from the same `LV2_PATH`, created lazily on first use (it parses every
  bundle again) — and a stand-in `LilvInstance` built from an `InstanceRef` (descriptor +
  handle, `Lv2Plugin::instance_ref`), since lilv only reads those two fields. URIDs come from
  the shared `livi::Features` map, so state keys match the instances'. `Lv2Host::presets` lists
  `pset:Preset`s related to the plugin (user = under `~/.lv2`); `restore_preset` returns the
  preset's port values for the caller to apply (lilv only restores the plugin's own state);
  `save_preset` writes `~/.lv2/<Plugin>_<Preset>.lv2/` (Ardour's layout) and reloads that
  bundle; `delete_preset` only touches user presets. `save_file`/`restore_file` do the same
  with a state bundle anywhere (`state.ttl`) that the world never loads, so it isn't listed as
  a preset — for slot presets. **Threading:** LV2 state save/restore must
  not overlap `run`, so both are `unsafe fn`s whose caller guarantees that for a plugin with
  state (`InstanceRef::has_state`, asked of the plugin's `extension_data`, since metadata
  doesn't always declare it); for a plugin without state they only read/report port values.
  `Lv2Plugin` also exposes `plugin()`/`features()`/`raw_handle()` — not used by audio processing
  itself, but needed by `live-keys-lv2-ui` to open a plugin's own GUI against the *same* running
  instance (see below). Re-exports `livi`/`lv2_raw` so downstream crates don't each need their
  own dependency on them.

- **`crates/vst3-host`** (`live-keys-vst3-host`) — VST3 hosting on the MIT-licensed `vst3`
  bindings crate (COM smart pointers `ComPtr`/`ComWrapper` from `com-scrape-types`; all calls
  `unsafe`). Modules:
  - `module.rs` — finds `.vst3` bundles (`default_search_paths()`: `~/.vst3`, `/usr/lib/vst3`,
    `/usr/local/lib/vst3`, plus extra folders; walks a few levels deep), resolves
    `<name>.vst3/Contents/<arch>-linux/<name>.so`, `dlopen`s it, calls `ModuleEntry(handle)` and
    `GetPluginFactory`, and lists the factory's "Audio Module Class" classes (instrument iff
    `PClassInfo2::subCategories` contains `Instrument`). Sub-categories can't identify a MIDI
    effect (JUCE ones claim `Fx|…`), so every non-instrument class is briefly created and
    initialized at scan time (`plugin::probe_kind`) to look at its buses: MIDI out and no
    audio in = MIDI effect. **Modules are loaded once per process
    and never unloaded** (`ModuleExit` is never called; a global cache keyed by path) — JUCE and
    similar frameworks keep process-wide state that doesn't survive unload/reload, and every
    rescan would otherwise do exactly that. Scanning has to load every module (VST3 has no
    metadata-only discovery), so a plugin that crashes on load crashes the scan.
  - `com.rs` — host-side COM objects implemented in Rust: `HostApplication` (the `initialize`
    context), `ComponentHandler` (receives `performEdit` from the plugin's editor and forwards
    it to a settable `EditCallback`), and the per-block `EventList`/`ParameterChanges`/
    `ParamValueQueue` — fixed-capacity, pre-allocated, never reallocating, using
    `UnsafeCell`/`Cell` with `unsafe impl Send/Sync` justified by only ever being touched by the
    thread running `process`. Every parameter change lands at sample offset 0 (no
    sample-accurate source exists), so each queue holds a single point; a later change in the
    same block overwrites it. Plus `MemoryStream` (`IBStream`) for the load-time
    `component.getState` → `controller.setComponentState` sync.
  - `plugin.rs` — `instantiate` creates and initializes the component, gets the edit controller
    (same object via `queryInterface`, or a separate class via `getControllerClassId`, then
    connected over `IConnectionPoint`), syncs state, installs the component handler, activates
    the main audio in/out and event in/out buses, and sets up 32-bit realtime processing. The result
    is split in two halves sharing an `Arc<Instance>` whose `Drop` disconnects and `terminate`s
    once neither half is left: **`Vst3Plugin`** (the `HostedPlugin`, moved to the audio thread;
    `unsafe impl Send`; its `Drop` switches processing/activation off) and **`Vst3Controller`**
    (main thread only, per the VST3 threading rules: `params()`, `set_param_normalized` to keep
    the plugin's editor in sync with host-side edits, `set_edit_callback`, `edit_controller()`
    for creating the editor). Input bus 0's channels get host-filled buffers
    (`audio_input_mut`); any further input bus (e.g. a sidechain) gets a shared silent buffer,
    re-zeroed every block. MIDI translation in `Vst3Plugin::process`: note on/off and poly
    pressure become VST3 events; CCs, channel pressure and pitch bend become parameter changes
    via the plugin's `IMidiMapping` (looked up once at load into a 16×130 table) — VST3 has no
    CC events, and JUCE plugins rely on this for sustain pedal etc. `Vst3Controller::params()`
    leaves out hidden/read-only parameters **and every `IMidiMapping` target**: JUCE plugins
    expose 2080 per-channel MIDI-CC stand-in parameters without flagging them hidden (Surge XT:
    2855 parameters → 775 real ones). Kinds come from `ParameterInfo`: `kIsBypass` or
    `stepCount == 1` → toggle, `kIsList` with 2–256 steps → choice (labels from the controller's
    `getParamStringByValue` for each step), other `stepCount > 1` → stepped; `units` → `unit`.
    JUCE plugins leave `stepCount` at 0 for most choice/int parameters (Surge XT: 774 of 775
    continuous), so those stay sliders — their value text still reads right, see
    `Vst3Controller::value_text`. The other direction, for MIDI effects: after each
    `process`, the output event list becomes `midi_output()` (`event_to_midi`: note on/off,
    poly pressure, and `kLegacyMIDICCOutEvent` → CC/channel pressure/pitch bend/program
    change).
  - `preset.rs` — `.vstpreset` files (48-byte header with the class ID in ASCII hex, chunk
    data, then a `List` of `Comp`/`Cont` chunks; little-endian): `find` scans
    `~/.vst3/presets`, `/usr/share/vst3/presets`, `/usr/local/share/vst3/presets` and matches by
    the header's class ID (folder names vary by host); new presets go to
    `~/.vst3/presets/<vendor>/<plugin>/<name>.vstpreset` (vendor from `PClassInfo2`/
    `PFactoryInfo`). `Vst3Controller::state`/`set_state` get/set the component's and the
    controller's state (`setState`, then `setComponentState`, then the controller's own) —
    allowed from the main thread while processing, so no lending needed. Round-trip unit-tested.
    `Vst3Host::save_file` writes a `.vstpreset` anywhere (for slot presets; `load_preset`
    reads it back).
  `Vst3Host` (`lib.rs`) owns one scan's results, instantiates by `PluginInfo::id`, and wraps
  presets (`presets`/`load_preset`/`save_preset`/`delete_preset`).

- **`crates/vst3-ui-host`** (`live-keys-vst3-ui`) — hosts a VST3 plugin's own editor
  (`IPlugView`, platform type `X11EmbedWindowID`) in an `x11rb`-created top-level window. Unlike
  the LV2 UI host, it **runs on the main (GUI) thread**, not a thread of its own: the VST3
  threading rules bind the editor, like the edit controller, to the thread the plugin was
  created on. `Vst3Editor` does no work by itself — the host calls `Vst3Editor::idle()` every
  poll tick (~16 ms), which pumps the window's X11 events (`WM_DELETE_WINDOW` → close;
  `ConfigureNotify` → `checkSizeConstraint`/`onSize` when the view `canResize`) and everything
  the plugin registered with the host's run loop. That run loop is the same Rust COM object as
  the `IPlugFrame` (`PlugFrame` implements both `IPlugFrame` and `Steinberg::Linux::IRunLoop`,
  since plugins — JUCE included — find the run loop by `queryInterface` on their frame): it
  records registered fds and timers and services them non-blockingly in `idle()` (`poll` with
  zero timeout; due timers fire once, missed ticks skipped). Handler lists are snapshotted
  before dispatch since callbacks may (un)register re-entrantly. `resizeView` from the plugin
  resizes the window, then calls `onSize`. Non-resizable editors get min=max WM size hints.
  `Vst3Editor::window()` is its X11 window (for the plugin check).
  Confirmed with Surge XT (JUCE) and OT P1ANO S (HISE/JUCE).

- **`crates/engine`** (`live-keys-engine`) — the real-time wiring (`cpal` + `midir` + an
  `Lv2Host` + a `Vst3Host`) shared by both the CLI and the GUI, so there's exactly one implementation instead of
  two drifting copies. It's a fixed rack of `SLOT_COUNT` (3) slots sharing one output stream;
  each slot is a serial **chain** in two `Section`s around an instrument: up to
  `MAX_MIDI_EFFECTS` (8) MIDI effects in front of it, each fed the previous one's MIDI, and up
  to `MAX_EFFECTS` (8) audio effects after it, each fed the previous plugin's audio. Split in
  two:
  - `rack.rs` — the audio-thread side. `Rack` owns every slot's `SlotRuntime`: the MIDI
    chain as a fixed `[Option<BoxedPlugin>; MAX_MIDI_EFFECTS]`, the audio chain as a fixed
    `[Option<BoxedPlugin>; 1 + MAX_EFFECTS]` (index 0 = instrument, effects contiguous from 1),
    the slot's `MidiRouter`, pre-allocated MIDI buffers, and its gain. `Rack::apply` takes one
    `RackCommand` (`SetInstrument`, `InsertEffect`/`RemoveEffect`/`MoveEffect` with a
    `Section`, `Param { slot, position: ChainPosition, .. }`, `Filter`, `Gain`) —
    insert/remove/move are `rotate_left`/`rotate_right` on the section's slice of the fixed
    array, so no allocation; out-of-range indices are ignored rather than trusted (a panic
    would kill the audio thread). `Rack::render` drains MIDI, runs it through each slot's
    router, then per sub-chunk runs the MIDI chain (`process_midi_chain`: every MIDI effect
    runs every sub-chunk, even with no input, since arpeggiators/delays emit MIDI on their own;
    each one's `midi_output` is copied into a scratch buffer as the next one's input) and then
    the audio chain in order — every audio stage gets the MIDI chain's output (effects too: most
    ignore it, some need it) and its inputs
    filled from the previous stage's outputs by `feed_inputs` (channels wrap around: mono →
    stereo duplicates, stereo → mono takes the left; zeroed when there's no previous stage,
    e.g. effects in a slot without an instrument) — and *sums* the last stage's output, times
    the slot's gain, into cpal's interleaved buffer. Gain changes ramp linearly across one block
    so fader moves don't click. Each slot's output peak goes into a shared atomic
    (`SlotPeaks`, `fetch_max` on the float bits), read and reset by `Engine::take_slot_peak` —
    a meter. **Whenever the MIDI chain changes** (insert/remove/move), the
    instrument's next block is preceded by CC 123 (all notes off) on all 16 channels: notes a
    removed transposer shifted, or an arpeggiator started, would otherwise never get their
    note-off — the router's note-off tracking only covers what enters the MIDI chain. Unit-tested
    with fake plugins whose output reveals processing order.
  - `engine.rs` — the control side. `Engine::start` builds the MIDI/command/retired ring
    buffers, connects MIDI input, and starts the `cpal` output stream whose callback is just
    "apply pending commands, then `rack.render`" (re-instantiating every already-loaded plugin
    against the new stream, since the sample rate may change — the GUI uses this when the
    device selection changes). `load_instrument`/`unload_instrument` (a slot's effects stay)
    and `add_effect(slot, section, id)` (appends)/`remove_effect`/`move_effect` change the
    *running* chain without restarting it; `section_for(id)` gives a plugin's natural section
    from its kind (used by the CLI's `--effect`). The engine keeps a mirror of every chain
    (`SlotChain { midi_effects, instrument, audio_effects }` of `LoadedPluginMeta`), changed only after the corresponding command was successfully
    queued (`push_structural`), so its indices always match the audio side's. One queue carries
    everything, so commands apply in order — a param change queued for a plugin can never land
    on whatever replaced it. Every loaded plugin gets a `PluginKey` (a process-unique `u64`) that
    stays valid however the chain is reordered; param changes and plugin GUIs address plugins by
    key, resolved to `(slot, ChainPosition)` on the control thread at send time — consistent
    with the queue order since the mirror is updated as commands are queued. Per-slot
    `SlotSettings` (filter + linear gain, set via `set_slot_filter`/`set_slot_gain`) belong to
    the slot, not its plugins: the engine keeps them even while stopped or empty and seeds each
    new stream with them.
  A plugin taken out of a chain is never dropped on the audio thread — it's pushed onto a
  "retired" ring buffer back to the control thread, which drops it in
  `Engine::collect_retired` (called on every structural change and from the GUI's poll timer);
  if that queue is ever full the plugin is leaked rather than torn down in the callback.
  **Lending** (`RackCommand::Lend`/`Return`, `Engine::with_lent`): for work that mustn't overlap
  a plugin's processing (LV2 state save/restore), the callback takes the plugin out of its place
  — left empty, which the chain already skips: an instrument's slot goes silent, an effect is
  bypassed — and hands it over on a "lent" ring; the control thread waits for it (up to
  `LEND_TIMEOUT`), does the work, and queues it back (an instrument then gets all-notes-off,
  since it missed note-offs). No structural command can interleave: the control thread is
  blocked meanwhile. A plugin handed over after the timeout is put back by `collect_retired`.
  `Engine`'s `running` field is declared before the hosts on purpose: fields drop in order, and
  an LV2 plugin still in the stream or retired queue must not outlive its world (this crashed on
  exit before). The callback's
  `data` buffer can span more frames than `max_block_size` in one call — observed with a
  Bluetooth output device, whose PipeWire-negotiated period can reach/exceed it — so the callback
  loops over `data` in `max_block_size`-sized sub-chunks rather than assuming one `process` call
  always covers the whole buffer; only the first sub-chunk gets that callback's freshly drained
  MIDI, since no further real time has passed for the rest (consistent with the frame_offset
  simplification below). Getting this wrong previously left the tail of any oversized callback
  buffer unwritten — heard as short bursts of correct audio cutting to stale/silent output on
  outputs with a large negotiated period. All buffers involved are pre-allocated before
  `stream.play()`, never inside the callback. `Engine::list_plugins` merges both formats (LV2
  first); instantiation dispatches on the id (`vst3:` prefix → `Vst3Host`, else `Lv2Host`).
  `Engine::rescan_plugins` rebuilds the whole `Lv2Host` with `LV2_PATH` extended by extra
  folders (see `paths.rs`) and rescans VST3 with the same extra folders — it stops the stream and
  empties every slot first, since an `Lv2Plugin` shouldn't outlive the `World` it came from.
  `default_plugin_search_paths()` lists both formats' default folders (for display);
  `build_lv2_path` uses only the LV2 ones. `Engine::slot(slot)` returns that slot's `SlotChain`
  and `Engine::plugin(key)` one `LoadedPluginMeta` (key, id, name, format, `params` —
  `ParamInfo`s for a parameter-editor UI — and `editor: PluginEditor`, all captured at load time
  since the running plugin itself lives in the audio callback). `PluginEditor` is what opening
  the plugin's own GUI or handling its presets needs, per format: `Lv2 { plugin, features,
  raw_handle, instance }` or `Vst3(Vst3Controller)`. The meta also records the last value sent
  per parameter (`Engine::param_value`, falling back to the default) — every value change goes
  through `send_param_change`, and preset loads record theirs — since the running plugin can't
  be asked. Presets: `presets(key)`, `load_preset(key, id)` (LV2 with state: restored while
  lent, port values set on the lent plugin; LV2 without: port values queued as param changes,
  no gap; VST3: `set_state` on the controller, then every parameter read back with
  `getParamNormalized`), `save_preset(key, name)` (LV2 with state lent; port values from the
  record), `delete_preset(key, id)`; `save_state_file(key, dir, stem)`/`load_state_file(key,
  path)` are the same for a state file outside the plugin's presets (`<stem>.lv2/` or
  `<stem>.vstpreset`), which slot presets are made of. Both loads go through one `restore`
  (`StateSource`), both LV2 saves through `save_lv2` (lending, port values). Two ways to change a parameter: `send_param_change(key, id, value)`
  only queues it to the audio thread — for edits that *came from* the plugin's own GUI — while
  `set_param` (host-side edits, e.g. a slider) also tells a VST3 controller, so the plugin's
  open editor follows (an open LV2 GUI is told by the GUI crate instead, see below).
  `send_midi(event)` plays MIDI as if from the input port (the MIDI queue's producer is shared
  with the input callback behind a `Mutex`, neither being the audio thread);
  `take_slot_peak(slot)` is the slot's loudest sample since the last call.
  `send_ui_atom(key, port, atom)` passes an atom from an LV2 plugin's GUI to the plugin, and
  `drain_ui_events` hands over everything LV2 plugins wrote to their atom outputs (keyed by
  plugin), emptying their channels (`PluginEditor::Lv2 { ui_channel }`). `param_text(key, id, value)` is
  a value as display text: a VST3 plugin's own (`Vst3Controller::value_text`, unit appended
  unless already there; main thread only), else `ParamInfo::format_value`.
  `audio.rs`/`midi.rs` hold device/port discovery and substring-based selection
  (`find_output_device`/`find_midi_port`), reused by `--audio-device`/`--midi-port` in the CLI and
  the Settings tab in the GUI.

- **`crates/lv2-ui-host`** (`live-keys-lv2-ui`) — hosts an LV2 *plugin's own* GUI (not this
  app's GUI). Deliberately isolated from audio hosting: it's X11-specific unsafe FFI that a future
  Windows/macOS UI-hosting backend would replace wholesale. `best_supported_ui`/`launch_ui`
  (`discovery.rs`) pick between two mechanisms, preferring whichever needs less from the host:
  - **External UI** (`external_ui.rs`) — the kxstudio "external-ui" extension. The plugin manages
    its own top-level window entirely; hosting is `dlopen` (via `libloading`) + finding
    `lv2ui_descriptor` + calling `show`/`run`/`hide`/`cleanup`. No window creation needed on our
    side. Requires the `instance-access` feature (the DSP instance's raw `LV2_Handle`, from
    `Lv2Plugin::raw_handle()`) — confirmed via Helm.
    **Gotcha:** `lv2_raw`'s binding for `LV2_External_UI_Host::ui_closed` types it as
    `extern "C" fn(...) -> libc::c_void` — impossible to honestly implement, since `c_void` has
    no legally constructible value. This callback fires on the plugin's *own* thread the moment
    the user closes its window (the normal path, not an edge case — confirmed via Helm's JUCE
    wrapper calling it from its own timer thread), so panicking to satisfy the type checker
    (e.g. `unreachable!()`) aborts the whole process the instant anyone closes a plugin window.
    The fix: implement the real function with a plain `()` return, then `transmute` the
    *function pointer* (never a value) to the field's declared type when building the struct —
    function pointers differing only in a `void`/`()` return are ABI-compatible on every
    platform this project targets.
  - **X11UI** (`x11_ui.rs`, `suil_sys.rs`) — a plain X11 window the host must create and embed
    the plugin's widget into. Uses hand-written bindings to `suil` (the reference LV2
    UI-wrapping library; no Rust crate exists, hence `suil_sys.rs` instead of a `suil-sys`
    crate — linked via `build.rs` + the `pkg-config` crate) to load/wrap the UI, and `x11rb` to
    create/manage the parent window (mapped, `WM_DELETE_WINDOW`-aware). Sizing follows
    `ui:resize` both ways: the host offers the feature (a UI's size request is recorded in a
    `Cell` and applied by the idle loop, since it can come while the UI is still being
    created), the window starts at the requested size or else the widget's geometry, and when
    the user resizes the window, the widget is resized to match and the UI's own `ui:resize`
    interface, if any, is told — called with the UI's own handle (`suil_instance_get_handle`),
    like the idle interface, not the struct's `handle` field, which JUCE leaves null (passing
    it crashed Surge XT). Confirmed via Surge XT and setBfree.

  Both mechanisms run their instantiate→show→idle-loop→cleanup lifecycle synchronously on a
  **dedicated background thread per open UI** (not the audio thread, not the GUI's own event-loop
  thread), and report closure back via `PluginUiHandle::is_closed()`/an internal `Arc<AtomicBool>`.
  Traffic between DSP and GUI is a `PortMessage`: `Control { port, value }`
  (`ui:floatProtocol`) or `Atom { port, atom }` (`atom:eventTransfer`; the `LV2_Atom` header
  plus body). A UI's writes reach the caller's `port_write: Fn(PortMessage) + Send` (parsed by
  `port_message`) — it does *not* touch the audio engine directly; the caller is expected to
  forward it across a channel to whichever single thread drives the engine (see
  `crates/gui/src/main.rs`'s `poll_timer` for the pattern), since `rtrb`'s producer side isn't
  safe to call from more than one thread. The other way, `PluginUiHandle::notify` queues a
  `PortMessage` for the hosting thread, which delivers it on its next idle tick
  (`suil_instance_port_event`, or the External UI descriptor's `port_event`, read as an
  `Option` since C allows NULL there although `lv2_raw` types it non-null). `LaunchUiRequest`'s
  `keep_alive` (a `GuiLease`, see lv2-host) is dropped by the hosting thread only after the UI is
  torn down. UI binaries are first `dlopen`ed with `RTLD_NODELETE` (`pin_library`) so they're
  never unmapped: x42's UIs leave GLib thread-local data with destructors in their own code on
  the hosting thread, which ran at thread exit after suil's `dlclose` and crashed the process.
  `PluginUiHandle::window()` is an X11 UI's window (for the plugin check's resizing).

- **`crates/gui`** (`live-keys-gui`, binary `live-keys-gui`) — the Slint app. `build.rs`
  (`slint-build`) compiles `ui/app.slint`, which composes `settings_tab.slint` + `live_tab.slint`
  via `TabWidget`/`Tab` from `std-widgets.slint`; `globals.slint` defines the host/UI interop
  contract (`AppState` = state Rust pushes in, `AppLogic` = actions the UI asks Rust to handle),
  per the project's Slint skill guidance. `config.rs` handles settings persistence (`dirs` +
  `serde_json`). `main.rs` wires it all to `live-keys-engine`/`live-keys-lv2-ui`/
  `live-keys-vst3-ui`. Plugins are added through one **plugin picker** popup
  (`ui/plugin_picker.slint`: `PluginPicker`, a `PopupWindow` over the whole window, wrapping
  `PickerPanel`) with a tab per `PluginKind` (`Category` global: 0 instrument, 1 MIDI effect,
  2 audio effect), a search field (focused on open; Enter picks the first match) and a star per
  row to pin it; each slot section's "+ Add"/"+ Choose"/"Replace" button opens it on that
  section's tab. A popup's own properties can't be set from outside it, so its slot and tab
  live in the `PickerTarget` global. The list is filtered in Rust (`Picker`: every plugin
  sorted by name; `AppLogic.filter-picker(category, query)` refills `AppState.picker-rows` —
  every search word must appear in name + format, pinned first, the first unpinned row flagged
  `after-pinned` for a divider); pins are `AppConfig.pinned_plugins`, toggled by `toggle-pin`.
  Picking calls `pick-plugin(slot, category, id)`, which loads an instrument (replacing) or
  appends to the matching effect section. The Settings tab lists every plugin with its kind
  and format. The Live tab is `SLOT_COUNT` equal-width `SlotPanel`s side by side, one per
  `AppState.slots` entry (`SlotState`: `instrument: EffectRow` — key -1 when empty —
  `midi-effects`/`audio-effects: [EffectRow]`, status, routing; an `EffectRow` is one
  plugin's name/key, whether its GUI/parameter window is open, and its current preset),
  laid out top to bottom in signal order: MIDI filter, MIDI effects,
  instrument, audio effects, volume. The instrument and each effect render as a `ChainRow`:
  the name, a GUI toggle and a Params toggle (highlighted while open), ↑/↓
  (effects only), ✕ to remove — and below it a `PresetBar` (`ui/preset_bar.slint`): the
  current preset (with a grey "changed" once a parameter moved since it was loaded or saved
  — `Rack::mark_changed`, on host-side edits and on plugin-GUI edits that actually change a
  value, so GUIs echoing values back don't count; LV2 plugin state changed outside its ports
  isn't seen), which opens an inline browser — a search field (every word must appear in the
  name; Enter loads the first match) over a list with a star per row to make it a favourite
  (`AppConfig.pinned_presets`, by `PresetInfo::id`, listed first) — plus "Save…" (an inline
  name field, prefilled with the current user preset's name — saving the same name
  overwrites) and "Delete" (the current preset, user presets only, confirmed inline). Only
  one browser is open at a time: it lives in `AppState` (`preset-browser-key`,
  `preset-rows`, `preset-count`, `preset-query`) and in Rust as `Rack::browser`
  (`PresetBrowser`: the plugin's presets, read when it opens, the query and the shown ones),
  filtered in Rust like the plugin picker. Preset errors go to the slot's status line.
  **Slot presets** (`src/slot_presets.rs`, `ui/slot_presets.slint`, at the bottom of each
  slot, filling the rest of it): a slot's whole chain under a name — a folder
  `<data dir>/live-keys/slot-presets/<instrument name>/<name>/` with `slot.json` (each
  plugin's id, name, state file, and its plugin preset + changed flag, so its preset bar shows
  them again) and one state file per plugin from `Engine::save_state_file` (saved into a
  temporary folder and swapped in, so a failed save keeps the old one; the same name for the
  same instrument overwrites). The browser groups them by instrument id (`SlotFolder`: all
  folders, or one — `Of(None)` for slots without an instrument); a newly loaded instrument
  opens its folder, an empty slot shows all folders (`Rack::last_instruments` notices the
  change in `refresh_slot`). Clicking a preset loads it at once (`load_slot_preset`): the
  instrument is kept if it's already the same plugin, a section's effects are kept if their
  ids match in order, otherwise replaced; then every plugin's state is restored; plugins that
  fail to load are skipped and reported in the slot's status line. A grey "changed" shows
  once the chain changes, any of its plugins' parameters moves, or a plugin preset is loaded
  (`Rack::mark_slot_changed`). Slot presets don't include the MIDI filter or volume (those are
  about the keyboard split and the mix, which scenes will cover). `AppLogic` actions take the slot index for structural changes
  (`pick-plugin`, `remove-effect(slot, section, index)`, `move-effect(slot, section, from,
  to)` — `section` 0 = MIDI, 1 = audio, …) and a `PluginKey` (as `int`) for per-plugin ones
  (`open-plugin-gui`, `toggle-params`, `set-param`, `save-preset`, `delete-preset`); the
  preset browser's actions act on the open browser's plugin. The engine stream is started lazily by the
  first load (on the devices chosen in Settings) and restarted — closing all plugin GUIs first,
  since their instance handles go stale (and keys change) — when the device selection changes
  while running. Replacing/removing a plugin likewise closes its GUI and parameter window
  first (`Rack::forget`). On the Rust side, `Rack` owns the slots' `VecModel`s, every open GUI
  keyed by `PluginKey` as an `OpenEditor` (`Lv2(PluginUiHandle)` — its own thread — or
  `Vst3(Vst3Editor)` — main thread), every open parameter window (`ParamWindow`: the window and
  its rows model), and the preset last loaded/saved per plugin (`current_presets`, with its
  changed flag); the poll timer calls
  `Rack::poll_uis`, which pumps open VST3 editors (`idle`) and forgets any GUI or parameter
  window that has closed (a window closed by its title bar is only hidden, so it's noticed by
  `is_visible`). Each slot's effect models are created once and refilled in place, changing only rows
  that differ (`sync_model`), so refreshing a slot doesn't rebuild its rows (and lose a
  half-typed preset name). The parameter editor is a separate
  top-level window per plugin (`ParamsWindow` in `ui/params_window.slint`, exported from
  `app.slint` so Rust can create it; `Rack::open_params`). Every Slint window has its own
  instance of each global, so it can't call `AppLogic` itself: its `set-param` is forwarded by
  Rust (`with_logic`) to the main window's `AppLogic`. Loading a preset rebuilds every row of
  an open window from `Engine::param_value`. It lists one `ParamControl`
  (`ParamRow`) per parameter, by kind: a `Switch` (toggle), a `ComboBox` beside the name (choice), a slider with
  `step` (stepped/note), a 0..1 slider mapped exponentially (logarithmic), or a plain slider —
  each with the value text (`param_text`) beside the name. **Slint gotcha:** a widget that sets its
  own property on interaction (`Slider.value`, `Switch.checked`, `ComboBox.current-index`,
  `Button.checked` when `checkable`) replaces that property's binding for good, so later model
  updates stop reaching it. Controls that must follow changes from elsewhere (the plugin's own
  GUI, a window closing) re-assign it from a `changed source =>` handler on a bound helper
  property, or use a property the widget never writes (`Button.primary` for the GUI/Params
  toggles). Snapping is host-side: `set-param`
  snaps via `ParamInfo::snap` and skips values equal to the last one sent, so a stepped slider
  only sends when it crosses a step. Sending a value calls `Engine::set_param`, while edits from a plugin's own GUI arrive over the
  `GuiEdit` channel (`Param`, or `Atom` for an LV2 plugin's atom input → `Engine::send_ui_atom`)
  and go through `Engine::send_param_change`; the engine records the value either way, and
  `Rack::show_param_value` updates an open window's row, so a window shows current values, not
  defaults. An open LV2 GUI is told host-side changes (`Rack::notify_ui`; every value on opening
  and after a preset load, in `refresh_param_rows`), and the poll timer forwards
  `Engine::drain_ui_events` to open LV2 GUIs (dropping events of plugins without one).
  Every structural change re-records the slot's chain into the config (`record_chain`). Meant as the base for
  future macro knobs/MIDI-bound parameters. Each slot also has channel (`ComboBox`, index 0 =
  omni), note-range and velocity-range (each one two-handle `RangeSlider` — a custom component
  in `ui/range_slider.slint`, since std-widgets has none; note names shown using C4 = 60) and a
  volume fader in dB (`MIN_VOLUME_DB` = -60 means muted,
  up to +6; converted to linear gain in `config::SlotConfig::gain`). These live in
  `AppConfig.slots` (`SlotConfig`) as the GUI's source of truth and are pushed to the engine on
  startup and on every edit; `Rack::refresh_slot` only touches a row's plugin/GUI fields,
  `Rack::set_routing` its filter/volume fields. Edits that fire continuously (fader drags)
  mark the config dirty and the poll timer saves it, rather than writing the file per step.

- **`crates/plugin-check`** (`live-keys-plugin-check`, binary `plugin-check`) — an end-to-end
  check against real installed plugins, not part of the app (see Common commands). `cases.rs`
  is the table of plugin classes and their quirks (`Case`: plugin id, role — instrument, or an
  effect around `mda EPiano` — whether a note should sound, a `ParamCheck` by exact name with an
  optional dB drop it must cause, what resizing should do — `Resize::Follows`/`Fixed`/
  `NotByHost`/`NoGui` — whether atoms should cross both ways, a warm-up). `main.rs` runs each
  case as `plugin-check --case <name>` in a child (the child prints `STEP\t<step>\t<PASS|FAIL|
  SKIP>\t<detail>` lines; a crash, hang — `CASE_TIMEOUT` — or nonzero exit fails the case, and
  the child's stderr is shown). Sound is measured with `Engine::take_slot_peak`, GUI windows
  are looked at and resized from outside through X11 (`x11.rs`: size, first child = the
  plugin's widget, distinct colours in a pixel sample — at most 2 means a blank window). The
  GUI side is pumped like the app's poll timer (edits forwarded, `drain_ui_events` to the GUI).

- **`src/main.rs`** (binary crate `live-keys`) — the original headless CLI, now a thin wrapper
  around `live-keys-engine` (loads its one `--plugin` into slot 0). Uses `clap` (derive) for CLI args.

### Real-time audio thread rules

The audio callback must never allocate, lock, or perform blocking syscalls. This is why:
- `MidiEvent`/`ParamChange` are fixed-size and moved via `rtrb` rather than `Mutex<Vec<_>>`.
- `HostedPlugin::process`/`audio_output` never materialize a slice-of-slices; adapters pass
  iterators straight through to the underlying plugin API.
- All buffers (MIDI batch, per-channel audio scratch) are pre-allocated outside the closure
  passed to `cpal`'s `build_output_stream`.
- Plugins enter the callback already instantiated (boxed on the control thread) and leave it
  via the retired ring buffer, so neither construction nor teardown runs on the audio thread.
- Chains are fixed-size arrays rearranged by rotation, stage-to-stage audio is copied into
  each plugin's own pre-allocated input buffers, and stage-to-stage MIDI into a pre-allocated
  per-slot scratch buffer (capacity-bounded: overflow is dropped, never grown) — adding,
  removing or reordering effects never allocates on the audio thread.

When touching the audio callback path (`Engine::start`'s closure, everything in
`crates/engine/src/rack.rs`, or anything `HostedPlugin` implementations do inside
`process`/`set_param`), preserve this — no `Vec::new()`/`.collect()`/`format!()`/`log::info!`
(only `log::error!`/`log::warn!` on rare error paths are currently tolerated there — see
`rack.rs` and the adapters for the reasoning on each specific one).

### Known simplifications (not bugs)

- Restarting the stream (device change while running) re-instantiates every loaded plugin at
  default parameter values — plugin state isn't carried over (presets could do this now, but
  don't yet).
- After a VST3 state load (preset or slot preset), the parameter values read back from the
  controller can be stale: Surge XT applies a loaded state on its audio thread, and its JUCE
  controller only catches up asynchronously, which doesn't happen without its editor open
  (confirmed: the component's state after loading equals the saved one byte for byte, while
  `getParamNormalized` keeps the old value). The sound is right; the parameter window and the
  recorded values may not be. Surge also calls `restartComponent(kParamValuesChanged)`, which
  is ignored. Restarting the stream doesn't carry slot presets' state over either.
- Settings an LV2 plugin only exchanges with its GUI as atoms (setBfree's drawbars etc.) aren't
  recorded host-side: they don't show in the parameter window or mark presets "changed" —
  though plugin and slot presets do capture them, through the plugin's state.
- MIDI events always get `frame_offset = 0` — `midir`'s callback doesn't hand back a
  sample-accurate offset relative to the audio clock, so events are applied at the start of
  whichever block consumes them. Fine for a keyboard-driven rack; would need revisiting for
  tight sequencer/clock sync.
- Only the default `F32` sample format is supported for audio output (`Engine::start`
  bails if the device's default config isn't `F32`).
- "Logical" audio device filtering (`is_logical_output_device` in `crates/engine/src/audio.rs`)
  keeps a device only if its ALSA driver id names a real sound card (`CARD=`) or is one of a
  small allowlist of card-less names that genuinely route to real output (`default`, `pulse`,
  `pipewire`, `jack`) — everything else (raw `hw:`/`plughw:` numeric nodes, and generic
  plugin-*type* registrations like `null`/`samplerate`/`speexrate`/`oss`/`upmix` that ALSA
  advertises regardless of whether they're usable endpoints) is excluded. This still surfaces
  several same-named entries per sound card (e.g. `front`, `dmix`, `surround51`, `sysdefault` all
  share the card's display name) — these are genuinely different logical PCM configs, not a
  filter bug; cpal's `Display` just doesn't expose the distinguishing driver id.
- The rack is a fixed `SLOT_COUNT` (3) parallel slots, each a serial MIDI effects → instrument
  → audio effects chain (max 8 of each kind of effect) with its own MIDI filter and volume,
  summed into one output.
  Not a general audio graph: no routing between slots, no sends/parallel branches, no
  sidechains, no per-effect bypass or dry/wet (beyond what an effect offers itself), no latency
  compensation, no master/limiter stage (several loud slots can clip), no MIDI
  transpose/velocity curves. It follows the intended **in-process** direction: every loaded plugin's audio is mixed in our own code,
  with only the final mix reaching `cpal` — not one independent OS audio stream per plugin
  (which would only work via OS-level mixing like PipeWire/JACK, isn't cross-platform,
  and would make routing plugin output through our own EQ/compression/mixing code
  impossible once the OS has already mixed it). This was decided explicitly when
  weighing out-of-process plugin isolation (see below) against this direction.
  An effect's main audio inputs get the previous chain stage's output; LV2 CV inputs and
  VST3 secondary input buses (sidechains) are still fed silence, since nothing routes to them.
  MIDI effects run in block steps on the audio thread: output events keep their frame offset
  within the block, but there's no transport (tempo-synced MIDI effects see the fixed 120 BPM,
  not playing). Only 3-byte channel messages pass between stages (no sysex); a MIDI effect's
  audio output, if any, is ignored, and so is any plugin's MIDI output outside the MIDI
  section (an instrument's MIDI out goes nowhere).
- VST3 hosting, deliberately minimal so far: no `IMessage`/`IAttributeList` (`createInstance`
  returns `kNotImplemented`) — plugins whose separate controller talks to its processor through
  `IConnectionPoint` messages won't get those messages; every plugin tested is JUCE-based and
  works without them. `restartComponent` is acknowledged but ignored (no re-setup for latency/IO
  changes, no parameter-list refresh). Only the main audio buses are activated (others get
  buffers but stay inactive; only output bus 0 is read). The processor's
  `outputParameterChanges`/output events are discarded. No transport (the `ProcessContext` is a
  fixed 120 BPM 4/4, not playing). Presets are `.vstpreset` files only — factory programs
  exposed through `IUnitInfo` program lists aren't listed yet.
- A VST3 editor runs on the GUI's main thread (see `crates/vst3-ui-host`), so — unlike an LV2
  GUI, which wedges only its own thread — a VST3 editor that hangs inside a call freezes the
  whole app. There's no way around it within the VST3 threading rules short of out-of-process
  hosting (see the out-of-process note below).
- LV2 UI hosting: Gtk/Qt-wrapped UIs may work via suil's adapter modules but aren't the
  validated/primary target (bare X11UI and External UI are); only `ui:floatProtocol` and
  `atom:eventTransfer` are handled (not `ui:peakProtocol` and the like), every atom output
  goes to the GUI whether it asked for that port or not, and no `ui:portSubscribe`. An External
  UI can't be resized by the host (it owns its window).
- `PluginUiHandle` never joins its hosting thread (see its doc comment in
  `crates/lv2-ui-host/src/lib.rs`) — closing/reopening a plugin's GUI must never block the
  caller (typically the GUI's main thread) on that plugin's own teardown code finishing.
  (Dropping the *plugin* does wait for that, up to 3 s — see GUI leases under lv2-host.)
  Observed concretely with Helm's LV2 build: closing its window very soon after opening
  (before its JUCE wrapper finishes initializing) can wedge its internal message pump inside
  our `run_fn` call — the window goes permanently unresponsive/black and the hosting thread
  never returns from that call. This is a bug in Helm's own JUCE-based UI wrapper, not
  something the host can reach in or fix; the mitigation here is only that it no longer takes
  the rest of the app down with it. A wedged thread like this can still make the *whole
  process* hang on exit later, if a library's `atexit`/global-destructor logic tries to
  synchronize with it — not addressed here.
- **Accepted limitation, decided against fixing further**: JUCE keeps one `MessageManager`
  lock shared by every instance of a JUCE plugin loaded from the same `.so` in a process, not
  per-instance. Once Helm's UI wedges (see above), that shared lock is poisoned — *any* later
  attempt to instantiate Helm again (even fresh DSP with no UI involved) hangs inside
  `JuceLv2Wrapper`'s constructor for the rest of the process's life; only restarting the whole
  app recovers it. No other loaded plugin is affected (each has its own `.so`/lock), and audio
  already playing is unaffected. The obvious full fix — one OS process per loaded plugin, with
  `SIGKILL` as an unconditional last resort (REAPER ships exactly this for crash isolation) —
  was designed in detail and then explicitly rejected: it requires each plugin to own an
  independent OS audio stream (mixed only via the OS/audio server), which conflicts with the
  in-process mixing/FX direction above, isn't cross-platform, and is disproportionate effort
  for what may be specific to this one LV2 port (the VST3 build of Helm, once that phase
  exists, is a plausible workaround). Don't re-propose out-of-process isolation reactively
  against a single plugin bug — if it's revisited, design it together with the in-process
  mixing engine above, not standalone.
- Because a plugin's own UI can wedge (see above) and — for External UI specifically — the host
  has *no* channel to detect a closed window except the plugin itself calling `ui_closed`, a
  wedged plugin can never report closed on its own. `AppState.plugin-gui-open` is therefore
  tracked independently of that: the GUI's "Close Plugin GUI" button
  (`on_close_plugin_gui` in `crates/gui/src/main.rs`) sends a best-effort close request and then
  unconditionally clears the local `open_plugin_ui` state, regardless of whether the hosting
  thread ever actually finishes. Confirmed via `/proc/<pid>/task/<tid>/stat` CPU-time snapshots
  (frozen solid across multiple checks = genuinely stuck, not just slow) that a wedged hosting
  thread does *not* block the GUI's own main thread (which keeps cycling normally) — the button
  existing is purely to recover the ability to open a *fresh* session, since nothing else can.

## Plugin format roadmap

1. **Phase 1 (done) — LV2** via `livi`/`lilv`. Validated the audio graph, MIDI routing, and
   real-time threading model end-to-end before any other format was touched.
2. **Phase 2 (done, core) — VST3** via the MIT-licensed `vst3` bindings crate (no mature safe
   Rust VST3 host crate exists): DSP, parameters, MIDI-CC mapping and the plugin's own editor
   (see `crates/vst3-host`/`crates/vst3-ui-host`). Open follow-ups are under "Known
   simplifications" (IMessage, restartComponent, state save/restore, transport).
3. **Phase 3 — VST2/LXVST** via `vst2-sys`/bindgen once phases 1–2 are stable. "LXVST" is just
   the Linux-native naming convention for VST2 `.so` builds (same ABI as Windows/Mac VST2,
   scanned from Linux-specific paths like `~/.lxvst/`) — not a separate format. Lowest priority;
   deprecated/unlicensable for new distribution, mainly for loading legacy plugins.

Each format gets its own adapter crate implementing `HostedPlugin`, keeping per-format unsafe/FFI
code isolated from the core audio graph and MIDI routing.

## GUI

Slint (declarative `.slint` markup + Rust logic) — chosen over egui for a more native-feeling UI
without a hard GTK/Qt dependency. See **`crates/gui`** in Architecture above for the two-tab
structure and interop pattern, and **`crates/lv2-ui-host`**/**`crates/vst3-ui-host`** for how a
*plugin's* own GUI (as opposed to this app's GUI) gets hosted — that part needed native window
embedding via X11 (`x11rb`, plus `suil` for LV2), independent of Slint, exactly as anticipated
before either was built.
