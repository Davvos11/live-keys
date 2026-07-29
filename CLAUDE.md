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
with Windows/macOS as secondary goals. Currently in Phase 1: LV2 hosting only (see
"Plugin format roadmap" below).

## Dev dependencies

LV2 hosting requires system `lilv` headers before anything will build:

```shell
sudo apt install liblilv-dev lv2-dev
```

Without these, `cargo check`/`cargo build` fails at the `lilv-sys` build script with a
`pkg-config` error, not a Rust compile error — if you see that, it's this, not a code bug.

## Common commands

```shell
cargo check                                    # fast type-check across the whole workspace
cargo build --release                          # LV2 hosting is real-time audio; always test in release
cargo run --release -- --list                  # list discovered LV2 instrument plugins (uri + name)
cargo run --release -- --list-midi             # list MIDI input port names
cargo run --release -- --list-audio            # list logical audio output devices
cargo run --release                            # run with defaults (Helm synth, first MIDI port, default output)
cargo run --release -- --plugin-uri <uri> --midi-port <substr> --audio-device <substr>
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

Cargo workspace with three crates, isolating format-specific unsafe/FFI code behind a
format-agnostic core:

- **`crates/core`** (`live-keys-core`) — no system dependencies, compiles standalone. Defines:
  - `MidiEvent` (`src/midi.rs`) — fixed-size (3-byte) MIDI channel-voice message + frame offset.
    Fixed size is deliberate: it has to move through a lock-free ring buffer without allocation,
    so sysex/variable-length messages are dropped at `MidiEvent::from_raw`.
  - `HostedPlugin` trait (`src/plugin.rs`) — the format-agnostic plugin abstraction:
    `process(midi_in, frames)` renders into the plugin's own internal buffers, then
    `audio_output(channel)` reads the result back. This split (render, then read) is intentional
    — it lets adapters hand real-time-safe *iterators* to their underlying plugin API instead of
    materializing a `Vec<&mut [f32]>`/slice-of-slices on every audio callback.
  - `midi_ring_buffer()` (`src/ring.rs`) — thin wrapper over `rtrb::RingBuffer` (SPSC, lock-free)
    for passing MIDI events from the input thread into the real-time audio thread.

- **`crates/lv2-host`** (`live-keys-lv2-host`) — wraps `livi` (built on `lilv`) for LV2 plugin
  discovery/hosting. `Lv2Host` owns the `livi::World` and the shared real-time `Features`
  (URID map, block size bounds); `Lv2Host::instantiate` only accepts plugins with **zero audio
  input ports** — pure instruments driven by MIDI only. Effects (plugins with audio inputs)
  aren't supported yet; that needs a real audio graph, which doesn't exist yet either (see
  Roadmap). `Lv2Plugin::process` pushes MIDI into a persistent `LV2AtomSequence` and renders into
  persistent per-channel `Vec<f32>` buffers sized to `max_block_size` at instantiation — no
  allocation happens on the audio thread.

- **`src/main.rs`** (binary crate `live-keys`) — wires `cpal` (audio out) + `midir` (MIDI in) +
  the LV2 adapter together. No GUI. Uses `clap` (derive) for CLI args. Real-time audio callback
  (`build_output_stream`'s closure) does: drain the MIDI ring buffer into a pre-allocated
  `Vec` → `plugin.process(...)` → read back per-channel output and interleave into cpal's
  buffer. All buffers involved (`midi_batch`, per-channel scratch inside `Lv2Plugin`) are
  allocated once before `stream.play()`, not inside the callback.

### Real-time audio thread rules

The audio callback must never allocate, lock, or perform blocking syscalls. This is why:
- `MidiEvent` is fixed-size and moved via `rtrb` rather than `Mutex<Vec<_>>`.
- `HostedPlugin::process`/`audio_output` never materialize a slice-of-slices; adapters pass
  iterators straight through to the underlying plugin API.
- All buffers (MIDI batch, per-channel audio scratch) are pre-allocated outside the closure
  passed to `cpal`'s `build_output_stream`.

When touching the audio callback path (`src/main.rs`'s closure, or anything `HostedPlugin`
implementations do inside `process`), preserve this — no `Vec::new()`/`.collect()`/`format!()`/
`log::info!` (only `log::error!` on the rare error path is currently tolerated there).

### Known phase-1 simplifications (not bugs)

- MIDI events always get `frame_offset = 0` — `midir`'s callback doesn't hand back a
  sample-accurate offset relative to the audio clock, so events are applied at the start of
  whichever block consumes them. Fine for a keyboard-driven rack; would need revisiting for
  tight sequencer/clock sync.
- Only the default `F32` sample format is supported for audio output (`main.rs` bails if the
  device's default config isn't `F32`).
- "Logical" audio device filtering (`is_logical_output_device` in `main.rs`) excludes ALSA's
  raw `hw:`/`plughw:` numeric hardware nodes, keeping named/configured PCM devices instead. This
  still surfaces several same-named entries per sound card (e.g. `front`, `dmix`, `surround51`,
  `sysdefault` all share the card's display name) — these are genuinely different logical PCM
  configs, not filter bugs; cpal's `Display` just doesn't expose the distinguishing driver id.
- Only one plugin can be hosted at a time; there's no audio graph/rack/chaining yet.

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

## GUI (not started)

Planned: Slint (declarative `.slint` markup + Rust logic), chosen over egui for a more
native-feeling UI without a hard GTK/Qt dependency. Plugin editor GUIs (VST3/LV2 UI extension)
are a separate problem from the host's own UI — they need native window embedding via
`raw-window-handle`, independent of Slint. Slint UI work starts only after the LV2
MIDI-keyboard-through-speakers milestone is solid.
