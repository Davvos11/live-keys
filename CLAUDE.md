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
with Windows/macOS as secondary goals. LV2 is the only hosted plugin format so far (see "Plugin
format roadmap" below); a Slint GUI now exists alongside the original headless CLI.

## Dev dependencies

```shell
sudo apt install liblilv-dev lv2-dev libsuil-dev
```

- `liblilv-dev`/`lv2-dev`: LV2 plugin hosting (`livi`/`lilv`).
- `libsuil-dev`: hosting LV2 plugins' own native GUIs (X11UI embedding via `suil`).

Without these, `cargo check`/`cargo build` fails at a build script (`lilv-sys`'s or
`live-keys-lv2-ui`'s) with a `pkg-config` error, not a Rust compile error — if you see that, it's
this, not a code bug.

## Common commands

```shell
cargo check --workspace                        # fast type-check across the whole workspace
cargo build --release                          # LV2 hosting is real-time audio; always test in release

# CLI (live-keys)
cargo run --release -- --list                  # list discovered LV2 instrument plugins (uri + name)
cargo run --release -- --list-midi             # list MIDI input port names
cargo run --release -- --list-audio            # list logical audio output devices
cargo run --release                            # run with defaults (Helm synth, first MIDI port, default output)
cargo run --release -- --plugin-uri <uri> --midi-port <substr> --audio-device <substr>

# GUI (live-keys-gui) — Settings tab (IO + plugin folders) and Live tab (load + open plugin GUI)
cargo run -p live-keys-gui --release
# Settings persist to ~/.config/live-keys/config.json (audio/MIDI selection, extra plugin
# folders, last-loaded plugin URI), loaded on startup.

# Manual LV2-UI-hosting smoke tests (not part of the app; see "LV2 plugin GUI hosting" below)
cargo run -p live-keys-lv2-ui --example test_helm    # External UI path
cargo run -p live-keys-lv2-ui --example test_surge   # X11UI path (needs Surge XT installed)

cargo test -p live-keys-core                   # only crate with meaningful unit tests today
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

Cargo workspace with five crates, isolating format-specific and UI-specific unsafe/FFI code
behind format-agnostic cores:

- **`crates/core`** (`live-keys-core`) — no system dependencies, compiles standalone. Defines:
  - `MidiEvent` (`src/midi.rs`) — fixed-size (3-byte) MIDI channel-voice message + frame offset.
    Fixed size is deliberate: it has to move through a lock-free ring buffer without allocation,
    so sysex/variable-length messages are dropped at `MidiEvent::from_raw`.
  - `ParamChange` (`src/param.rs`) — a control-port write request (`port_index` + `value`),
    carried the same way as `MidiEvent`. This is how a plugin's own open GUI (turning a knob, on
    its own hosting thread) reaches the audio thread.
  - `HostedPlugin` trait (`src/plugin.rs`) — the format-agnostic plugin abstraction:
    `process(midi_in, frames)` renders into the plugin's own internal buffers, then
    `audio_output(channel)` reads the result back. This split (render, then read) is intentional
    — it lets adapters hand real-time-safe *iterators* to their underlying plugin API instead of
    materializing a `Vec<&mut [f32]>`/slice-of-slices on every audio callback. `set_control`
    applies one `ParamChange` to a control-port input.
  - `ring_buffer<T>()` (`src/ring.rs`) — generic thin wrapper over `rtrb::RingBuffer` (SPSC,
    lock-free); `midi_ring_buffer()`/`param_ring_buffer()` are the `MidiEvent`/`ParamChange`
    instantiations actually used.

- **`crates/lv2-host`** (`live-keys-lv2-host`) — wraps `livi` (built on `lilv`) for LV2 plugin
  discovery/hosting. `Lv2Host` owns the `livi::World` and the shared real-time `Features`
  (URID map, block size bounds); `Lv2Host::instantiate` only accepts plugins with **zero audio
  input ports** — pure instruments driven by MIDI only. Effects (plugins with audio inputs)
  aren't supported yet; that needs a real audio graph, which doesn't exist yet either (see
  Roadmap). `Lv2Plugin::process` pushes MIDI into a persistent `LV2AtomSequence` and renders into
  persistent per-channel `Vec<f32>` buffers sized to `max_block_size` at instantiation — no
  allocation happens on the audio thread. `Lv2Plugin` also exposes `plugin()`/`features()`/
  `raw_handle()` — not used by audio processing itself, but needed by `live-keys-lv2-ui` to open
  a plugin's own GUI against the *same* running instance (see below). Re-exports `livi`/`lv2_raw`
  so downstream crates don't each need their own dependency on them.

- **`crates/engine`** (`live-keys-engine`) — the real-time wiring (`cpal` + `midir` + an
  `Lv2Host`) shared by both the CLI and the GUI, so there's exactly one implementation instead of
  two drifting copies. `Engine::load_and_start` builds the MIDI and param ring buffers, connects
  MIDI input, and starts the `cpal` output stream, whose callback: drains MIDI → drains param
  changes → `plugin.process(...)` → reads back per-channel output and interleaves it into cpal's
  buffer. All buffers involved are pre-allocated before `stream.play()`, never inside the
  callback. `Engine::rescan_plugins` rebuilds the whole `Lv2Host` with `LV2_PATH` extended by
  extra folders (see `paths.rs`) — it stops any running plugin first, since an `Lv2Plugin`
  shouldn't outlive the `World` it came from. `Engine::send_param_change`/`loaded_plugin_meta`
  are the two hooks a plugin-GUI integration needs (see `live-keys-lv2-ui` below).
  `audio.rs`/`midi.rs` hold device/port discovery and substring-based selection
  (`find_output_device`/`find_midi_port`), reused by `--audio-device`/`--midi-port` in the CLI and
  the Settings tab in the GUI.

- **`crates/lv2-ui-host`** (`live-keys-lv2-ui`) — hosts a *plugin's own* GUI (not this app's
  GUI). Deliberately isolated from audio hosting: it's X11-specific unsafe FFI that a future
  Windows/macOS UI-hosting backend would replace wholesale. `best_supported_ui`/`launch_ui`
  (`discovery.rs`) pick between two mechanisms, preferring whichever needs less from the host:
  - **External UI** (`external_ui.rs`) — the kxstudio "external-ui" extension. The plugin manages
    its own top-level window entirely; hosting is `dlopen` (via `libloading`) + finding
    `lv2ui_descriptor` + calling `show`/`run`/`hide`/`cleanup`. No window creation needed on our
    side. Requires the `instance-access` feature (the DSP instance's raw `LV2_Handle`, from
    `Lv2Plugin::raw_handle()`) — confirmed via Helm.
  - **X11UI** (`x11_ui.rs`, `suil_sys.rs`) — a plain X11 window the host must create and embed
    the plugin's widget into. Uses hand-written bindings to `suil` (the reference LV2
    UI-wrapping library; no Rust crate exists, hence `suil_sys.rs` instead of a `suil-sys`
    crate — linked via `build.rs` + the `pkg-config` crate) to load/wrap the UI, and `x11rb` to
    create/manage the parent window (mapped, `WM_DELETE_WINDOW`-aware, resized to fit the
    widget's actual geometry once known since there's no `ui:resize` protocol support).
    Confirmed via Surge XT.

  Both mechanisms run their instantiate→show→idle-loop→cleanup lifecycle synchronously on a
  **dedicated background thread per open UI** (not the audio thread, not the GUI's own event-loop
  thread), and report closure back via `PluginUiHandle::is_closed()`/an internal `Arc<AtomicBool>`.
  A UI's `port_write`/`write_function` callback is a plain `Fn(u32, f32) + Send` the caller
  supplies — it does *not* touch the audio engine directly; the caller is expected to forward it
  across a channel to whichever single thread actually drives `Engine::send_param_change` (see
  `crates/gui/src/main.rs`'s `poll_timer` for the pattern), since `rtrb`'s producer side isn't
  safe to call from more than one thread.

- **`crates/gui`** (`live-keys-gui`, binary `live-keys-gui`) — the Slint app. `build.rs`
  (`slint-build`) compiles `ui/app.slint`, which composes `settings_tab.slint` + `live_tab.slint`
  via `TabWidget`/`Tab` from `std-widgets.slint`; `globals.slint` defines the host/UI interop
  contract (`AppState` = state Rust pushes in, `AppLogic` = actions the UI asks Rust to handle),
  per the project's Slint skill guidance. `config.rs` handles settings persistence (`dirs` +
  `serde_json`). `main.rs` wires it all to `live-keys-engine`/`live-keys-lv2-ui`.

- **`src/main.rs`** (binary crate `live-keys`) — the original headless CLI, now a thin wrapper
  around `live-keys-engine`. Uses `clap` (derive) for CLI args.

### Real-time audio thread rules

The audio callback must never allocate, lock, or perform blocking syscalls. This is why:
- `MidiEvent`/`ParamChange` are fixed-size and moved via `rtrb` rather than `Mutex<Vec<_>>`.
- `HostedPlugin::process`/`audio_output` never materialize a slice-of-slices; adapters pass
  iterators straight through to the underlying plugin API.
- All buffers (MIDI batch, per-channel audio scratch) are pre-allocated outside the closure
  passed to `cpal`'s `build_output_stream`.

When touching the audio callback path (`Engine::load_and_start`'s closure, or anything
`HostedPlugin` implementations do inside `process`/`set_control`), preserve this — no
`Vec::new()`/`.collect()`/`format!()`/`log::info!` (only `log::error!`/`log::warn!` on rare error
paths are currently tolerated there — see `crates/engine/src/engine.rs` for the reasoning on
each specific one).

### Known simplifications (not bugs)

- MIDI events always get `frame_offset = 0` — `midir`'s callback doesn't hand back a
  sample-accurate offset relative to the audio clock, so events are applied at the start of
  whichever block consumes them. Fine for a keyboard-driven rack; would need revisiting for
  tight sequencer/clock sync.
- Only the default `F32` sample format is supported for audio output (`Engine::load_and_start`
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
- Only one plugin can be hosted (and one plugin GUI open) at a time; there's no audio
  graph/rack/chaining yet.
- LV2 UI hosting: no `ui:resize` protocol support (X11UI windows are sized once, from the
  widget's initial geometry, not kept in sync afterward); Gtk/Qt-wrapped UIs may work via suil's
  adapter modules but aren't the validated/primary target (bare X11UI and External UI are); only
  `ui:floatProtocol` (plain float control-port writes) is handled, not other port protocols.

## Plugin format roadmap

1. **Phase 1 (current) — LV2** via `livi`/`lilv`. Validates the audio graph, MIDI routing, and
   real-time threading model end-to-end before any other format is touched.
2. **Phase 2 — VST3** via `vst3-sys`/the MIT-licensed `vst3` bindings crate (no mature safe
   Rust VST3 host crate exists). `HelgeSverre/rust-vst3-host` is a useful architectural
   reference for the COM interface plumbing (`IComponent`, `IAudioProcessor`,
   `IEditController`) — treat it as a learning reference/fork base, not a dependency. Expect
   substantial `unsafe` here.
3. **Phase 3 — VST2/LXVST** via `vst2-sys`/bindgen once phases 1–2 are stable. "LXVST" is just
   the Linux-native naming convention for VST2 `.so` builds (same ABI as Windows/Mac VST2,
   scanned from Linux-specific paths like `~/.lxvst/`) — not a separate format. Lowest priority;
   deprecated/unlicensable for new distribution, mainly for loading legacy plugins.

Each format gets its own adapter crate implementing `HostedPlugin`, keeping per-format unsafe/FFI
code isolated from the core audio graph and MIDI routing.

## GUI

Slint (declarative `.slint` markup + Rust logic) — chosen over egui for a more native-feeling UI
without a hard GTK/Qt dependency. See **`crates/gui`** in Architecture above for the two-tab
structure and interop pattern, and **`crates/lv2-ui-host`** for how a *plugin's* own GUI (as
opposed to this app's GUI) gets hosted — that part needed native window embedding via X11
(`x11rb` + `suil`), independent of Slint, exactly as anticipated before either was built.
