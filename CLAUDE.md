# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Keeping this file current

This project is evolving fast. Whenever a change alters something documented here — a new
crate/adapter, a changed `HostedPlugin`/core API, a new CLI flag, a roadmap phase completed, a
new real-time constraint or simplification — update the relevant section of this file as part of
that same piece of work, not as a follow-up. Treat a stale CLAUDE.md as a bug in the change.

## No backwards compatibility (yet)

Until there's a proper release, don't write compatibility code for older config files, saved
data, CLI flags or APIs — no migrations, serde aliases, or deprecated aliases. Change the format
and move on; the developer's own files get fixed by hand if needed.

## What this is

A cross-platform audio plugin host in Rust, optimized for live performance driven by a MIDI
keyboard — an "instrument rack" for a live set, not a DAW/timeline. Primary target is Linux,
with Windows/macOS as secondary goals. LV2, VST3 and CLAP are the hosted plugin formats so far (see
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
Nor does CLAP: `clap-sys` is plain Rust bindings to the CLAP headers.

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
# <id> as printed by --list: an LV2 URI, `vst3:` + class ID, or `clap:` + CLAP plugin id.
cargo run --release -- --plugin <id> --effect <id> --effect <id>   # MIDI effects → instrument → audio
                                               # effects; each --effect goes where its kind belongs

# GUI (live-keys-gui) — one window after the Penpot design (no tabs): a scenes pane on the left — per slot restore/clear/leave unchanged, searchable,
# renamable — then 6 rack slots in a 3×2 grid, each: a header (instrument photo and name, its GUI,
# mute and ✕-clear-with-undo buttons, a volume knob, its presets), then three tabs — the instrument's
# panel (setBfree's drawbars, then a row of five knobs/switches/levers, each picked by right-click
# from its profile's controls or any parameter) and the audio effects as squares; the slot presets (the whole chain with
# every plugin's settings, browsed by instrument); MIDI effects, the inputs it listens to, a MIDI
# channel/note-range/velocity-range filter and a transposition. Up to 8 MIDI effects → an
# instrument → up to 8 audio effects, added through a plugin picker popup (tabs per kind, search,
# pinned favourites); an effect's square shows its LV2 inline display if it has one, a click opens
# its GUI (or parameter window), a right-click its menu, dragging it reorders), then the right
# sidebar: a master transpose, a metronome (volume knob with play/stop under it, beat dots, BPM stepper, time
# signature, subdivision, Tap), the master output (volume slider, master audio effects, mute, output device), the MIDI
# inputs, Settings (a window of its own: plugin folders, Rescan, discovered plugins) and MIDI panic;
# a fullscreen button (and F11) top right; under the scenes and slots, the control surfaces panel
# (the MIDI controllers chosen, drawn as their faders/knobs/pads/buttons, moving as the hardware does)
cargo run -p live-keys-gui --release
SLINT_LIVE_PREVIEW=1 cargo run -p live-keys-gui --release --features slint/live-preview
  # the app with live reload: saving a .slint file swaps the UI in place, state kept (development
  # only; never add the feature to Cargo.toml). Changing a property/callback Rust uses needs a restart.
LIVE_KEYS_MOCK_DATA=1 cargo run -p live-keys-gui --release
  # on exit, writes the UI's current state to crates/gui/ui/mock_data.slint, which the editor's
  # preview (the Slint plugin's "Show Preview", slint-viewer) shows instead of empty lists;
  # the preview needs a slint-lsp at least as new as our Slint (README: "Previewing the UI")
# Settings persist to ~/.config/live-keys/config.json (audio device, MIDI inputs' devices, extra plugin
# folders, pinned plugin ids, favourite preset ids, master volume + mute + transpose, the metronome's tempo, time
# signature, subdivision and volume, the control surfaces panel's devices and whether it's collapsed,
# MIDI learn's mappings kept in every scene and its surface controls' settings,
# and per slot: MIDI inputs + filter, volume), saved as they change. User control surfaces are
# ~/.local/share/live-keys/surfaces/*.json.
# The rack itself (every slot's chain and the master chain, plugin settings, current slot preset and
# scene, open plugin windows) is the session in ~/.local/share/live-keys/session/, saved on exit and after changes
# once the rack is quiet, and restored on startup. Slot presets live in
# ~/.local/share/live-keys/slot-presets/, scenes in ~/.local/share/live-keys/scenes/.

# Manual LV2-UI-hosting smoke tests (not part of the app; see "LV2 plugin GUI hosting" below)
cargo run -p live-keys-lv2-ui --example test_helm    # External UI path
cargo run -p live-keys-lv2-ui --example test_surge   # X11UI path (needs Surge XT installed)
cargo run -p live-keys-lv2-ui --example test_setbfree --release -- [seconds] [plugin uri]
  # a plugin's GUI through a live engine (default setBfree, whose GUI uses atom ports only;
  # needs an audio device); prints how many messages crossed each way
cargo run -p live-keys-engine --example setbfree --release
  # setBfree's control messages headless through a live engine: its uiinit dump, a drawbar set
  # and read back, a state file restored (reinit + dump); RUST_LOG=live_keys_engine=trace
  # prints every message

# Plugin check: one representative of every class of plugin with a known quirk (see
# crates/plugin-check/src/cases.rs), each in a child process: load, play a note (heard?),
# set a parameter and read it back (and hear it, where one is a volume), open the GUI (drawn?),
# resize it (does the widget follow?), close it and unload at once. ~2 minutes; windows pop up.
# Needs an X display and audio output; plugins not installed are skipped.
cargo run -p live-keys-plugin-check --release              # all cases; exit code 1 on a failure
cargo run -p live-keys-plugin-check --release -- vst3 lsp  # cases whose name contains a filter
cargo run -p live-keys-plugin-check --release -- --info <plugin id>   # GUI kind + params (id, symbol, name,
                                               # kind, range), for a new case or a panel profile
# Run it after touching plugin hosting or GUI hosting, and add a case when a plugin shows a new quirk.

# Manual VST3 smoke tests (not part of the app)
cargo run -p live-keys-vst3-host --example smoke --release   # scan, then render a chord offline
                                                             # through every instrument found
SMOKE_WAIT_SECS=10 cargo run -p live-keys-vst3-host --example smoke --release
  # ^ first processes silence at real-time pace for 10 s — sample-based plugins (e.g. HISE ones)
  #   only finish loading while they see `process` running, and render silence until then
cargo run -p live-keys-vst3-ui --example open_editor --release -- "surge xt" 10
  # opens a VST3 plugin's editor (name substring, exact match preferred) for N seconds

# Manual CLAP smoke tests (not part of the app)
cargo run -p live-keys-clap-host --example smoke --release -- [name filter]
  # scan, then per instrument: render a chord offline (peak), parameter count, a state round
  # trip, its presets (preset discovery: Surge XT's ~3000 take ~2 s) and loading the first
cargo run -p live-keys-clap-ui --example open_gui --release -- "surge xt" 10
  # opens a CLAP plugin's GUI (name substring, exact match preferred) for N seconds

# Manual preset smoke tests (not part of the app; they create and delete a "Smoke Test" preset)
cargo run -p live-keys-lv2-host --example presets --release -- <lv2 uri>   # list/apply/save/delete
cargo run -p live-keys-engine --example presets --release -- [plugin id]...
  # the same through a live engine (needs an audio device), LV2 lending included

cargo test -p live-keys-core -p live-keys-lv2-host -p live-keys-engine -p live-keys-vst3-host -p live-keys-clap-host -p live-keys-gui -p live-keys-sfz -p live-keys-dsp -p live-keys-piano -p live-keys-builtin -p live-keys-surfaces
  # the crates with unit tests (MIDI routing, control surface files and MIDI reading, MIDI/audio effect-chain routing, .vstpreset and
  # CLAP state file round trips, MIDI device names, input-mask shifting, inline-display pixels,
  # SFZ parsing, curves, voice logic and streaming; filter responses, LFOs, envelopes;
  # instrument state files and presets)

# The SFZ engine on its own (not part of the app yet). Salamander, for testing, lives in
# ~/.local/share/live-keys/samples/salamander/ (git clone of sfzinstruments/SalamanderGrandPiano)
cargo run -p live-keys-sfz --example render --release -- <file.sfz> [out.wav]
  # loads it (time, memory), plays a fixed passage, writes a WAV, reports render speed
cargo run -p live-keys-sfz --example play --release -- <file.sfz> [midi port filter]
  # plays it live from every MIDI input (or the matching ones) on the default output
# render streams with --stream (at real-time pace; reports underruns and streamed memory) or
# --head <frames> (a smaller head, to stress the loaders); without, samples load whole.
# crates/instruments/sfz/examples/saw-pad.sfz needs no samples: a test patch for the filter,
# envelopes and LFOs (mod wheel = vibrato, CC 74 = cutoff).
# Both take --samples <dir>: resolve includes and samples there (an SFZ kept apart from its
# samples). The piano: crates/instruments/piano/salamander.sfz --samples ~/.local/share/live-keys/samples/salamander
```

## Licensing

The project is MIT (`LICENSE`; `license`/`publish = false` set once in `[workspace.package]`).
Slint is used under its royalty-free licence, whose one condition is the "Made with Slint"
attribution — the `AboutSlint` widget at the bottom of the Settings window; don't remove it.
`deny.toml` makes `cargo deny --workspace check` (cargo-deny; `--workspace`, or only the CLI's
dependencies are checked) reject any dependency licence that isn't permissive, with the Slint
crates as named exceptions. Run it after adding or updating a dependency; a new licence or a
new Slint crate is a decision for the developer, not something to just add to the list.

`vendor/livi` is `livi` 0.8.0 (MIT, its `LICENSE` kept) with one addition,
`Plugin::instantiate_with_features` (host features beyond livi's own — the inline display's
`queue_draw`), used through `[patch.crates-io]` and excluded from the workspace (not linted or
tested as ours). Updating livi means re-applying that addition to the new version (vendored
only, by decision; no upstream PR so far).

Assets compiled into the GUI (`crates/gui/ui/assets/`, embedded by Slint) carry their own
licences next to them: Source Sans 3 under the SIL OFL (`fonts/OFL.txt`), Lucide icons under ISC
(`icons/LICENSE`), and photos that are CC0 or public domain only, each listed with its source
page in `photos/CREDITS.md` — checked on the file's own page, not a search filter; no CC BY(-SA),
Unsplash/Pexels/Pixabay-licence images. A new asset gets the same treatment. Those three files ship with the release tarball
(`licenses/`) and the `.deb` (`usr/share/doc/live-keys/`); a new kind of asset adds its own.

Our own instruments' sample sets aren't compiled in or shipped: they're downloaded on request
(see `docs/design/own-plugins.md`). Their licence is CC0 or CC BY (no NonCommercial or
ShareAlike), credited. The piano's SFZ (`crates/instruments/piano/salamander.sfz`, compiled
into `live-keys-piano`) is adapted from sfzinstruments' Salamander SFZ, CC BY 3.0: its header
credits the authors and says what changed; `crates/instruments/piano/CREDITS.md` ships with
the release tarball (`licenses/piano-credits.md`) and the `.deb`
(`usr/share/doc/live-keys/piano-credits.md`), like the assets above.

There is no lint/format/test CI in this repo yet (the only workflow builds releases, see below).
Run `cargo clippy --workspace --all-targets` once a feature/request is otherwise complete and fix
what it flags — not continuously on every intermediate edit, which just burns time re-checking
half-finished code.

## Releases and dependency updates

- **Releases** (`.github/workflows/release.yml`): publishing a GitHub release (tag `vX.Y.Z`)
  builds `live-keys-gui` and `live-keys` on `ubuntu-24.04` (stripped, without the debug info
  the release profile keeps locally) and attaches `live-keys-<tag>-linux-x86_64.tar.gz` (both
  binaries, `LICENSE`, `README.md`, the bundled assets' licences in `licenses/`) plus its
  `.sha256`, and two `.deb`s, to the release. Linux
  x86_64 only. Its apt list is the libraries the binaries link at build time (lilv, lv2, suil,
  ALSA, fontconfig — checked with `ldd`; Wayland/X11/GL are dlopened): a new `-sys` dependency
  that links a system library needs its `-dev` package added there, and to the runtime list in
  the README.
  - **The `.deb`s** (package `live-keys`) are made by `cargo-deb` from the same build
    (`cargo deb -p live-keys-gui --no-build`; `[package.metadata.deb]` in
    `crates/gui/Cargo.toml`): both binaries, `crates/gui/live-keys.desktop` (theme icon
    `audio-x-generic`, there's no icon of our own) and `LICENSE` as the copyright file. Two
    variants, same package name and version, told apart by file name only:
    `live-keys_<ver>_amd64.deb` (`--variant apt-repository`, the one the README points to)
    also installs the APT repository as a source — `packaging/live-keys.sources` (deb822) as
    `/etc/apt/sources.list.d/live-keys.sources`, a conffile, so a user deleting it opts out
    for good, and `packaging/live-keys.gpg` (the repository's public key, binary) as
    `/usr/share/keyrings/live-keys.gpg` — and `live-keys_<ver>_amd64-no-repo.deb` doesn't.
    cargo-deb names both after the package, so the workflow passes each its file name. Their
    version is the tag minus the `v`, not Cargo.toml's (which may lag; nothing checks they
    match). `Depends` comes from `dpkg-shlibdeps` (`$auto`), so it names 24.04's library
    packages (`libasound2t64` etc.): the package is for Ubuntu 24.04 and newer.
  - **The APT repository** (`https://davvos11.github.io/live-keys`, suite `stable`, component
    `main`, amd64) is the `apt` job: it downloads the `live-keys_*_amd64.deb`s (the with-repository
    variant, so installing from the repository keeps the source) of the latest `APT_KEEP_RELEASES`
    (5) non-prerelease releases, builds the repository with `.github/scripts/build-apt-repo.sh`
    (`apt-ftparchive`, signed `InRelease`/`Release.gpg`, the public key as `live-keys.gpg`
    beside it) and force-pushes it to `gh-pages` as a single commit, which Pages serves. So the
    releases are its only state: a re-run, or a manual run (`workflow_dispatch`, which only
    does this job), rebuilds it whole, a deleted release drops out, and the branch never
    collects old packages in its history (Pages from an Actions artifact would have no state
    to keep older versions in, and would need the `github-pages` environment opened up for
    tags). A `concurrency` group keeps two runs off `gh-pages` at once. The signing key is
    the `APT_GPG_PRIVATE_KEY` secret (armored; `APT_GPG_PASSPHRASE` if it has one); without
    it the job only leaves a notice and the release still gets its assets. The public key is
    exported from that secret on each run (served as `live-keys.gpg`) and also committed as
    `packaging/live-keys.gpg`, which the packages ship; the job fails if their fingerprints
    differ. Changing the key means committing the new public key and every user getting a
    new package (from a release; a repository signed by the new key won't verify with the
    old one). Prereleases get assets but aren't
    in the repository. The script runs locally too (`APT_GPG_PRIVATE_KEY=… build-apt-repo.sh
    <dir> <debs…>`), e.g. to test the layout with `apt` in an `ubuntu:24.04` container.
- **Renovate** (`renovate.json`): the Mend Renovate app with the developer's shared preset
  (`github>Davvos11/.github`, the `dot-github` repo): weekly on Saturday morning, patch/digest
  updates and lockfile maintenance grouped and automerged, minors grouped in one PR, majors on
  request from the Dependency Dashboard. It covers `Cargo.toml`/`Cargo.lock` and the workflow's
  action versions (kept as major tags, like the other repos). After a Cargo update, run
  `cargo deny --workspace check` (see Licensing) — Renovate doesn't.

## Testing philosophy

Keep it to small unit tests, and only for things that are genuinely non-trivial (real logic —
bit-packing, boundary conditions, parsing, a non-obvious invariant). Don't add a test for every
new function as a reflex; most of this codebase (adapters wiring an external API, CLI glue,
straightforward field plumbing) isn't worth a test on its own. When a test fails, look critically
at whether it caught a real bug or is just brittle/low-value before "fixing" it — a bad test is
worse than no test, and passing-by-construction tests (that can't actually fail on a real
regression) aren't worth keeping.

## Architecture

Cargo workspace with fifteen crates (plus the vendored `livi`, see Licensing), isolating format-specific and UI-specific unsafe/FFI code
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
  - Format-agnostic metadata, also in `src/plugin.rs`: `PluginFormat` (`Lv2`/`Vst3`/`Clap`/`Builtin`),
    `PluginKind` (`Instrument` = MIDI → audio, `MidiEffect` = MIDI → MIDI, `AudioEffect` =
    audio → audio and anything else), `PluginInfo` (`id`, name, format, kind — `id` is unique
    across formats: an LV2 URI, `vst3:` + the VST3 class ID in hex, `clap:` + the CLAP
    plugin id, or `builtin:` + one of our instruments' id), and `ParamInfo` (`id`, `symbol` — a stable reference for panel profiles: the
    LV2 port symbol, or the VST3/CLAP id in decimal —
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
    (`id`: an LV2 preset URI, a `.vstpreset` path, or a CLAP preset's location — see
    `crates/clap-host`; `name`; `user`: saved by the user, so deletable). Presets are
    deliberately the formats' standard ones, not an app-specific format, so they're shared with
    Ardour/Carla/DAWs both ways — except CLAP user presets, which CLAP has no format for.
  - `ring_buffer<T>()` (`src/ring.rs`) — generic thin wrapper over `rtrb::RingBuffer` (SPSC,
    lock-free); `midi_ring_buffer()`/`param_ring_buffer()` are the `MidiEvent`/`ParamChange`
    instantiations actually used.
  - `MidiFilter`/`MidiRouter` (`src/filter.rs`) — a slot's keyboard zone (channel or omni,
    inclusive note range, inclusive velocity range, transposition in semitones) and the
    fixed-size, allocation-free router that applies it on the audio thread:
    `route(inputs, event)` returns the event to pass on (a note transposed) or `None`.
    **Invariant:** note-offs are routed by tracking, not by the filter — the router keeps a
    `[u128; 16]` bitset of the (channel, note) pairs it passed note-ons for, as played, plus
    the transposition each got (`[[i8; 128]; 16]`), and a note-off passes iff its note-on did,
    shifted as its note-on was. So changing a filter (or the transposition) while keys are
    held never strands a note, and a note-off meant for another slot's zone never cuts this
    slot's voice. The note range is about the key played, before transposing; a note-on
    transposed outside 0..=127 is dropped (and so is its note-off). Velocity applies only to
    note-ons; poly aftertouch follows held notes and their shift; CCs/pitch bend/program
    change only check the channel. The filter's `inputs` mask picks which of the host's MIDI
    inputs the slot listens to (`route` gets the mask an event arrived on; `ALL_INPUTS` =
    every one); note-offs follow tracking here too. Unit-tested.

- **`crates/lv2-host`** (`live-keys-lv2-host`) — wraps `livi` (built on `lilv`) for LV2 plugin
  discovery/hosting. `Lv2Host` owns the `livi::World` and the shared real-time `Features`
  (URID map, block size bounds). Building the world is the slow part (~3 s with LSP installed:
  livi reads every plugin's ports up front), so it's split out as `Lv2Scan::run` — `Send`,
  unlike the host — and `Lv2Host::from_scan` makes the host from it. `Lv2Host::instantiate` accepts plugins with audio input ports
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
  input at frame 0, ahead of the block's MIDI. A plugin with atom inputs has a second input
  ring too, whose producer `Lv2Plugin::take_control_channel` hands out once (MIDI learn's
  mapping thread sends setBfree's control messages on it); its atoms follow the GUI's. Some GUIs talk to their DSP only this way —
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
  `units:symbol` as `unit`; scale points as `choices`; the port symbol as `symbol`;
  `pprops:notOnGUI` ports left out;
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
  **Inline displays** (`inline_display.rs`; Ardour's `http://harrisonconsoles.com/lv2/inlinedisplay`,
  see Part C of `docs/design/plugins.md`): every plugin is offered the `queue_draw` feature (via
  the vendored livi's `instantiate_with_features`), whose callback — called from `run()`, on the
  audio thread — only sets a per-plugin `AtomicBool`; its data lives in the `Lv2Plugin` (leaked
  with a leaked instance). `Lv2Plugin::inline_display()` is the plugin's `render` (from
  `extension_data`), its handle and that flag, as a `Send` `InlineDisplay`: `is_dirty`, and
  `unsafe render(max_w, max_h)` → an `InlinePicture` (premultiplied RGBA; the plugin's surface
  is premultiplied native-endian Cairo ARGB32, reordered by `argb32_to_rgba`, unit-tested).
  `render` may run while the plugin processes (that's how Ardour calls it; x42 and LSP
  synchronise themselves), but not after the instance is dropped, nor on two threads at once.
  The flag starts set, so a display is drawn once at load.

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
    audio in = MIDI effect. That's slow (~45 ms a class; LSP has ~190, which made startup
    ~11 s), so results are cached (`kind_cache.rs`: `~/.cache/live-keys/vst3-kinds.tsv`,
    keyed by binary path + size + mtime + class ID, rewritten after each scan) and only new
    or changed binaries are probed. **Modules are loaded once per process
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
  presets (`presets`/`load_preset`/`save_preset`/`delete_preset`). The scan itself is
  `Vst3Scan`, run a step at a time (`step`: one bundle loaded or one class looked at;
  `finish` does the rest) — it has to stay on the main thread, where the plugins' editors will
  run, so a GUI interleaves it with drawing; `Vst3Host::new` runs it in one go.

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

- **`crates/clap-host`** (`live-keys-clap-host`) — CLAP hosting on `clap-sys` (MIT/Apache
  bindings to the CLAP headers; all calls `unsafe`). Modules:
  - `module.rs` — finds `.clap` files (`default_search_paths()`: `CLAP_PATH`'s entries, `~/.clap`,
    `/usr/lib/clap`, `/usr/local/lib/clap`, plus extra folders; a few levels deep; on Linux a
    `.clap` is a plain shared library), loads them, calls `clap_entry.init` and reads the plugin
    factory's descriptors. Like VST3 modules, **loaded once per process and never unloaded**
    (`deinit` never called; a global cache by path). No probing: a descriptor's features say the
    kind (`instrument` → instrument, `note-effect` without `audio-effect` → MIDI effect, else
    audio effect). `ClapScan` loads a file per `step`, on the main thread.
  - `host.rs` — one `HostShared` per plugin instance (boxed; the `clap_host` it was created with
    has `host_data` pointing back at it) and the host extensions, as `static` tables:
    `thread-check` (main = the thread that created the plugin; audio = a thread-local flag set
    around `start_processing`/`process`/`stop_processing`, `as_audio_thread`), `log` (to `log`),
    `params` (`rescan` with values flags sets `values_changed`; nothing else), `gui`
    (`request_resize`/`closed` recorded in `GuiRequests` for the editor), `timer-support` and
    `posix-fd-support` (registries the controller's `idle` services), `state`, `note-ports`
    (dialects CLAP + MIDI), `audio-ports`, `latency` (no-ops). `request_callback` sets a flag
    for `idle`; `request_restart`/`request_process` are ignored.
  - `plugin.rs` — `instantiate` creates, `init`s and `activate`s the plugin (main thread; `1 ..=
    max_block_size` frames) and splits it, like VST3, into halves sharing an `Arc<Instance>`:
    **`ClapPlugin`** (the `HostedPlugin`; `start_processing` lazily on its first `process`,
    `stop_processing` in its `Drop`, both as the audio thread) and **`ClapController`** (main
    thread only). `Instance`'s `Drop` `deactivate`s and `destroy`s — on the main thread, or, if
    the last owner goes on another thread (a stream dropped on a thread of its own), it's queued
    for `destroy_pending()`, which `Engine::collect_retired` calls. Audio ports: every port gets
    buffers, input port 0 the host-filled ones, others a shared silent buffer; output port 0 is
    read back. Events (`events.rs`, fixed capacity): `set_param` pushes a `PARAM_VALUE` (time 0,
    plain value, no cookie) and `process` appends the block's MIDI after it — note on/off as
    CLAP note events if the first note input supports the CLAP dialect, everything else (and
    notes otherwise) as `CLAP_EVENT_MIDI` if it supports MIDI; a plugin without note input
    gets none. Output events: note on/off and MIDI become `midi_output()` (a MIDI effect's), and
    `PARAM_VALUE`s — **the plugin's own GUI's edits, which the plugin has already applied** —
    go over a ring to the controller. Transport is null (free-running). Parameters
    (`read_params`, once at load): hidden/read-only left out; bypass → toggle, stepped + enum
    (≤ 256 values) → choice labelled by `value_to_text`, stepped spanning 1 → toggle, other
    stepped → stepped; plain min/max/default. `ClapController`: `params`, `value_text` (the
    plugin's, unit included), `param_value` (`get_value` — CLAP plugins can be asked),
    `state`/`set_state` (`clap.state`, allowed while processing — no lending), `has_gui`/`gui`
    (`ClapGui`: the `clap.gui` calls with X11 as the API, holding the instance alive), and
    **`idle(on_edit) -> bool`**: runs `on_main_thread` if asked for, fires due timers (missed
    ticks skipped), polls registered fds without blocking, hands over the GUI's edits, and
    returns `true` when values should be re-read — after `params.rescan`, or `STATE_SETTLE`
    (100 ms) after a state/preset load, since Surge XT applies a loaded state on its next audio
    block without saying so.
  - `preset.rs` — presets: **preset discovery** (`clap.preset-discovery-factory/2` or its draft
    id): each provider is created with our indexer, its declared locations are walked (a
    file location's folder for the declared extensions; a plugin location asked once) and each
    file's metadata collected for presets naming this plugin's id (`user` from the
    `IS_USER_CONTENT` flag); loaded with `clap.preset-load`'s `from_location`. Surge XT finds
    ~3000 in ~2 s, so `ClapHost` caches them per plugin. CLAP has no shared user-preset file
    format, so **user presets saved here are state files** (`state.rs`: `LKCLAPS1`, the plugin
    id, then its state; `.clapstate`) in `<data dir>/live-keys/clap-presets/<plugin id>/`;
    discovered user presets that are a file of their own can be deleted too. Preset ids are
    `state\t<path>` or `clap-preset\t<kind>\t<location>\t<load key>`.
  `ClapHost` (`lib.rs`) owns one scan's results, instantiates by `PluginInfo::id`, and wraps
  presets (`presets`/`load_preset`/`save_preset`/`delete_preset`) and state files
  (`save_file`/`load_file`, for slot presets).

- **`crates/clap-ui-host`** (`live-keys-clap-ui`) — hosts a CLAP plugin's own GUI. Like VST3
  editors, on the main thread: `ClapEditor::open(controller, title)` embeds it (`create("x11",
  false)`, `set_parent` on an `x11rb` window of ours, sized from `get_size` once embedded, since
  LSP's only knows its size then; min=max WM hints when `can_resize` is false), or, for a
  plugin that only offers that, lets it open a floating window of its own. `ClapEditor::idle`
  pumps the window's X11 events (`WM_DELETE_WINDOW` → close; a user resize → `adjust_size` +
  `set_size`) and applies `request_resize`; the plugin's timers and fds are pumped by
  `ClapController::idle`, GUI open or not. `close` hides, destroys, then drops the window.
  Confirmed with Surge XT, Vital and LSP.

- **`crates/engine`** (`live-keys-engine`) — the real-time wiring (`cpal` + `midir` + an
  `Lv2Host` + a `Vst3Host`) shared by both the CLI and the GUI, so there's exactly one implementation instead of
  two drifting copies. It's a fixed rack of `SLOT_COUNT` (6) slots sharing one output stream;
  each slot is a serial **chain** in two `Section`s around an instrument: up to
  `MAX_MIDI_EFFECTS` (8) MIDI effects in front of it, each fed the previous one's MIDI, and up
  to `MAX_EFFECTS` (8) audio effects after it, each fed the previous plugin's audio. After
  the slots comes the **master chain**: up to `MAX_EFFECTS` audio effects fed the slots' mix,
  then the master gain (0 when muted), then the metronome's click. It's addressed as slot
  `MASTER_SLOT` (= `SLOT_COUNT`) wherever a slot index goes — `add_effect` (audio effects
  only)/`remove_effect`/`move_effect`, `Engine::slot`, `RackCommand`s — so presets, plugin
  GUIs, lending and stream restarts work for its plugins unchanged; `load_instrument`,
  `set_slot_filter`/`set_slot_gain` and `take_slot_peak` reject it. Split in two:
  - `rack.rs` — the audio-thread side. `Rack` owns every slot's `SlotRuntime`: the MIDI
    chain as a fixed `[Option<BoxedPlugin>; MAX_MIDI_EFFECTS]`, the audio chain as a fixed
    `[Option<BoxedPlugin>; 1 + MAX_EFFECTS]` (index 0 = instrument, effects contiguous from 1),
    the slot's `MidiRouter`, pre-allocated MIDI buffers, and its gain. `Rack::apply` takes one
    `RackCommand` (`SetInstrument`, `InsertEffect`/`RemoveEffect`/`MoveEffect` with a
    `Section`, `Param { slot, position: ChainPosition, .. }`, `Filter`, `Gain`/`Mute`,
    `MasterGain`/`MasterMute` — volume and mute are apart, so a volume set from elsewhere can't
    unmute —, `Metronome`, `Panic`) —
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
    note-off — the router's note-off tracking only covers what enters the MIDI chain. **MIDI
    panic** (`RackCommand::Panic`, `Engine::panic`) sends `PANIC` — CC 64, 120, 121 and 123,
    value 0, on all 16 channels — to every slot's instrument (and to its first MIDI effect, if
    any) and clears the routers' held notes. After the slots, `render_master` feeds the mix
    (interleaved, channels wrapped like `feed_inputs`) to the master chain's first effect,
    copies the last one's output back, and ramps the master gain like a slot's. Unit-tested
    with fake plugins whose output reveals processing order (master chain included).
  - `metronome.rs` — the metronome: `MetronomeSettings` (playing, BPM within
    `MIN_BPM`..=`MAX_BPM` (20–300), beats per bar, `Subdivision`, linear level) and `Click`,
    which the rack runs after the master gain (so neither the master volume nor its effects
    touch it; the GUI zeroes its level while the master is muted). It counts the position in
    the bar in beats (`f64`, per sample) and starts a decaying sine on each crossing: a high
    accented one on the bar's first beat, a lower one on the other beats, a soft one on
    subdivision clicks (positions measured from the bar's start, so dotted and triplet
    subdivisions realign every bar). Starting it starts a bar. The beat it's on goes to the GUI
    through an atomic (`Engine::metronome_beat`). Allocation-free; unit-tested (click onsets).
    Plugins don't see its tempo (see Known simplifications).
  - `midi.rs` — **MIDI inputs**: up to `MAX_MIDI_INPUTS` (8) numbered inputs, each fed by a set
    of devices remembered by `midi_device_name` (the port name minus ALSA's trailing
    `client:port` address, which changes when a device is plugged in again). `MidiInputs`
    (owned by the engine, independent of the stream) keeps one `midir` connection per present
    port that's on any input; its callback tags each event with the port's inputs as a mask
    (`RoutedMidi`, an `AtomicU32` updated in place when assignments change) and pushes it into
    the shared MIDI producer — `None` while no stream runs, so events are dropped then.
    `Engine::set_midi_inputs(devices per input)` sets the assignments; `Engine::sync_midi_inputs`
    (the GUI calls it every second) connects devices that appeared and drops ones that went
    away; both return the ports present. So a missing device never fails a plugin load.
    **Control surfaces**: `Engine::set_surface_devices(devices)` names the devices the GUI's
    surfaces panel shows; they're connected too (on an input or not), and the callback also
    sends their events (`SurfaceEvent`: device name + `MidiEvent`) to the mapping thread
    (below; a bounded `mpsc` channel, full = dropped). Events of mapped controls are
    **claimed** (`Claims`: per device, atomic bits per channel for CCs, notes, pitch bend and
    channel pressure) and don't reach the slots, unless their control passes through.
  - **MIDI learn** (`docs/design/midi-learn.md`): `mapping.rs` is the logic alone, unit-tested
    — `Mappings` from surfaces and `ControlSetup`s (a control, its `Takeover` — jump, glide,
    pick up —, glide time, pass-through, and `Link`s to targets with a `Mode`: a range —
    mirrored is from > to —, modulate, toggle levels, momentary, momentary modulate), all in
    positions 0..1. Each target has **layers**: a base (set by ranges, toggles and edits
    from elsewhere), the offsets of its modulating links, a momentary override, and the
    effective value they make, which is what it gets. `event` turns a surface event into
    effective values (`take_writes`, each with its base), `set_position` is a target set
    elsewhere — its new base — (links whose control is elsewhere lose it: pick up waits for
    a crossing, glide slides over the glide time, `tick` moving it on), and `take_waiting`
    reports faders and knobs whose target's base is elsewhere, for the GUI's ghost. `mapper.rs` runs it on the
    `midi-mapping` thread, which blocks on its input channel (2 ms ticks while gliding):
    `Engine::set_mappings(MapperSetup)` (surfaces, controls, `TargetSpec`s: an `Output` —
    `Param { key, param }`, `Setbfree { key, function }`, `Volume { slot }`, or `Gui` — a
    `ToValue` closure from position to value, and the position now) sets the claims and
    hands the setup over, returning its generation; a target's values go out at once —
    parameters and volumes as `MapperCommand`s on a ring of their own to the audio thread
    (`SharedMapperTx`, set while a stream runs; the `Rack` resolves a parameter's
    `PluginKey` itself, so it can't land on a plugin that replaced it), setBfree functions
    through `SetbfreeSink` on the plugin's second atom ring — and as `MapperReport`s (every
    surface event, `Applied` with the base, `Waiting`) to `Engine::take_mapper_reports`, which records
    applied values (`param_value`, setBfree's record, the slots' gains; a VST3 controller is
    told) before handing them on. `Engine::set_mapped_position` tells the thread of a target
    moved elsewhere.
  - `engine.rs` — the control side. `Engine::start` builds the MIDI/command/retired ring
    buffers, hands the MIDI producer to `MidiInputs`, and starts the `cpal` output stream
    (`build_stream`) whose callback is just "apply pending commands, then `rack.render`"
    (re-instantiating every already-loaded plugin against the new stream, at default
    settings, since the sample rate may change). The callback owns an `AudioState` (the
    `Rack` plus its command and MIDI consumers), which **`Engine::switch_output`** moves to
    another output device without reloading anything: it opens the device at the current
    sample rate (`output_config_at`; `Ok(false)` if it can't, nothing changed), queues
    `RackCommand::Detach` — on which the callback hands the boxed state over a one-slot
    ring and plays silence from then on — waits for it (`HANDOFF_TIMEOUT`), sets the new
    channel count and builds the new stream around it (the old one is dropped on a thread).
    Keys, open plugin GUIs and every setting stay; the gap is tens of milliseconds. A
    failed hand-over is an error (the state may be stuck in the old stream, or kept in
    `Engine::orphaned` until the next teardown), after which `start` must follow. The GUI
    tries this first when the device selection changes, falling back to `start` with the
    session saved before and restored after. `load_instrument`/`unload_instrument` (a slot's effects stay)
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
    `SlotSettings` (filter, linear gain and mute, set via `set_slot_filter`/`set_slot_gain`/
    `set_slot_muted`) belong to
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
  `stream.play()`, never inside the callback. `Engine::list_plugins` merges every format (our
  built-in instruments, then LV2, VST3, CLAP); instantiation dispatches on the id (`builtin:`
  prefix → `BuiltinHost`, `vst3:` → `Vst3Host`, `clap:` → `ClapHost`, else `Lv2Host`). The
  `BuiltinHost` needs no scan; it's made with the others when a scan is installed.
  Scanning is a `PluginScan`: `PluginScan::start(extra_dirs)` sets `LV2_PATH` to the default
  folders plus the extra ones (see `paths.rs`), builds the LV2 world on a thread of its own and
  starts a `Vst3Scan` and a `ClapScan` with the same folders, which `step` advances (VST3,
  then CLAP; `true` once all are done);
  `Engine::install_plugins(scan)` finishes it and swaps the hosts in — it stops the stream and
  empties every slot first, since an `Lv2Plugin` shouldn't outlive the `World` it came from.
  `Engine::new` scans the default folders before returning (CLI, examples);
  `Engine::unscanned` knows no plugins until a scan is installed (the GUI).
  `default_plugin_search_paths()` lists every format's default folders (for display);
  `build_lv2_path` uses only the LV2 ones. `Engine::slot(slot)` returns that slot's `SlotChain`
  and `Engine::plugin(key)` one `LoadedPluginMeta` (key, id, name, format, `params` —
  `ParamInfo`s for a parameter-editor UI — and `editor: PluginEditor`, all captured at load time
  since the running plugin itself lives in the audio callback). `PluginEditor` is what opening
  the plugin's own GUI or handling its presets needs, per format: `Lv2 { plugin, features,
  raw_handle, instance }`, `Vst3(Vst3Controller)`, `Clap(ClapController)` or `Builtin` (no
  GUI of its own — `gui_supported` is false, so the parameter window and the slot's panel are
  its UI; presets and state files through `BuiltinHost`: loading queues every parameter's
  value as a param change, saving writes `Engine::param_value`s — no lending, the recorded
  values are its whole state). The meta also
  records the last value sent per parameter (`Engine::param_value`, falling back to the
  default) — every value change goes through `send_param_change`/`set_param`, and preset loads
  record theirs — since the running plugin can't be asked (a CLAP one can, on the main thread:
  its record is seeded from `ClapController::param_value` at load, since Surge XT reports every
  default as 0). Presets: `presets(key)`, `load_preset(key, id)` (LV2 with state: restored while
  lent, port values set on the lent plugin; LV2 without: port values queued as param changes,
  no gap; VST3: `set_state` on the controller, then every parameter read back with
  `getParamNormalized`; CLAP: preset discovery's `from_location` or a state file's
  `set_state`, then every parameter read back with `get_value` — and again once it settles,
  see `idle_plugins`), `save_preset(key, name)` (LV2 with state lent; port values from the
  record), `delete_preset(key, id)`; `save_state_file(key, dir, stem)`/`load_state_file(key,
  path)` are the same for a state file outside the plugin's presets (`<stem>.lv2/` or
  `<stem>.vstpreset` or `<stem>.clapstate`), which slot presets are made of. Both loads go through one `restore`
  (`StateSource`), both LV2 saves through `save_lv2` (lending, port values). Two ways to change a parameter: `send_param_change(key, id, value)`
  only queues it to the audio thread — for edits that *came from* the plugin's own GUI; for a
  CLAP plugin, which applied its GUI's edit itself, it only records it — while
  `set_param` (host-side edits, e.g. a slider) also tells a VST3 controller, so the plugin's
  open editor follows (an open LV2 GUI is told by the GUI crate instead, see below; a CLAP
  plugin tells its own GUI). **`idle_plugins(on_edit) -> Vec<PluginKey>`** runs every CLAP
  controller's `idle` — call it every ~16 ms from the main thread (the GUI's poll timer, the
  CLI's loop, plugin-check's pump) — handing `on_edit` their GUIs' edits (for
  `send_param_change`) and returning the plugins whose values it re-read (a late-settling
  state), for refreshing what shows them.
  `send_midi(event)` plays MIDI as if from every MIDI input (the MIDI queue's producer is shared
  with the input callback behind a `Mutex`, neither being the audio thread);
  `take_slot_peak(slot)` is the slot's loudest sample since the last call.
  `render_inline_display(key, max_w, max_h)` renders an LV2 plugin's inline display and
  `inline_display_dirty(key)` says whether it asked to be (main thread only; safe since a
  plugin is only dropped after its meta is gone, and lending is on the main thread too).
  `send_ui_atom(key, port, atom)` passes an atom from an LV2 plugin's GUI to the plugin, and
  `drain_ui_events` hands over everything LV2 plugins wrote to their atom outputs (keyed by
  plugin), emptying their channels (`PluginEditor::Lv2 { ui_channel }`).
  **setBfree** (`setbfree.rs`; `SETBFREE_URI`) has no control ports: every setting is an
  `sb3:controlmsg` atom object (`sb3:controlkey` string, e.g. `upper.drawbar16`; `sb3:controlval`
  int 0..=127) on its `control` input and `notify` output (see "setBfree's protocol" in
  `docs/design/plugins.md`). A loaded setBfree gets a `Setbfree` in its `LoadedPluginMeta`
  (URIDs from the instance's own `livi::Features` map, the recorded values), and `instantiate`
  queues an empty `sb3:uiinit` on its GUI channel at once — setBfree reports nothing before
  that, then dumps every function set so far — so every load (stream restarts included) fills
  the values. `send_setbfree(key, function, value)` queues a control message and records it
  (setBfree doesn't echo the host's), returning the messages as the plugin would report them
  on `notify`, which the GUI passes to setBfree's open GUI so that follows host edits — for
  `rotary.speed-preset` also the `rotary.speed-select` it implies, since setBfree's GUI draws
  its Leslie levers from that and ignores the preset; `drain_ui_events` decodes the plugin's reports into the
  record before forwarding them (`special.reinit`, sent on a state restore before a full dump,
  clears it; other `special.*` keys are ignored) and marks the plugin for
  `take_setbfree_changes`; `send_ui_atom` records setBfree's own GUI's messages the same way.
  `setbfree_value(key, function)` reads the record — `None` for functions never set (the dump
  only has those). All control-thread work; the atoms travel like a GUI's. `param_text(key, id, value)` is
  a value as display text: a VST3 plugin's own (`Vst3Controller::value_text`, unit appended
  unless already there; main thread only), a CLAP plugin's own (`ClapController::value_text`,
  unit included), else `ParamInfo::format_value`.
  `audio.rs`/`midi.rs` hold device/port discovery and substring-based selection
  (`find_output_device`/`find_midi_port`), reused by `--audio-device`/`--midi-port` in the CLI
  (whose one port becomes MIDI input 1) and the Settings tab in the GUI.

- **`crates/lv2-ui-host`** (`live-keys-lv2-ui`) — hosts an LV2 *plugin's own* GUI (not this
  app's GUI). Deliberately isolated from audio hosting: it's X11-specific unsafe FFI that a future
  Windows/macOS UI-hosting backend would replace wholesale. `best_supported_ui`/`launch_ui`
  (`discovery.rs`) pick between two mechanisms, preferring whichever needs less from the host:
  - **External UI** (`external_ui.rs`) — the kxstudio "external-ui" extension. The plugin manages
    its own top-level window entirely; hosting is `dlopen` (via `libloading`) + finding
    `lv2ui_descriptor` + calling `show`/`run`/`hide`/`cleanup`. No window creation needed on our
    side. Requires the `instance-access` feature (the DSP instance's raw `LV2_Handle`, from
    `Lv2Plugin::raw_handle()`) — confirmed via Helm.
    **Gotcha:** `lv2_raw`'s `LV2_External_UI_Host` binding is unusable, so `external_ui.rs`
    declares its own `#[repr(C)] ExternalUiHost`. `lv2_raw` types `ui_closed` as
    `extern "C" fn(...) -> libc::c_void` — impossible to honestly implement, since `c_void` has
    no legally constructible value. This callback fires on the plugin's *own* thread the moment
    the user closes its window (the normal path, not an edge case — confirmed via Helm's JUCE
    wrapper calling it from its own timer thread), so panicking to satisfy the type checker
    (e.g. `unreachable!()`) would abort the whole process the instant anyone closes a plugin
    window. And since 0.3 it types `plugin_human_id` as `Option<*const c_char>`, which isn't
    laid out like a C pointer (raw pointers have no null niche, so the `Option` carries a tag).
    Check new `lv2_raw` versions for `Option<*…>` fields before using a struct from it.
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
    it crashed Surge XT). ConfigureNotify events with another size than the one we last
    gave the window are ignored until it's confirmed (or `RESIZE_SETTLE`, 500 ms, passes):
    KWin reports the initial 800x600 after we've resized it, and passing that on made LSP's
    UI, which answers `ui:resize` by asking for the size it was given, flip the window
    between two sizes until it was moved. Only sizes we didn't ask for (the user's) are
    passed to the UI's `ui:resize`. Confirmed via Surge XT, setBfree and LSP.

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
  (`slint-build`) compiles `ui/app.slint`: one window, no tabs — `live_tab.slint` (the scenes
  and slots) over the **control surfaces panel** (`surfaces.slint`), with the **right
  sidebar** (`sidebar.slint`, 300 wide) beside them. The window opens at, and can't shrink
  below, 1500×995 (the design's "Live (window minimum)" board is 1200×995, its "Right sidebar"
  300×995). Its layout is pinned to the window (`x`/`y`/`width`/`height`) because the panel's
  room is computed from the window's height (`AppWindow`'s `fits-expanded`/`squeeze`): tall
  enough (≥ 1019 px), the panel is docked and expanded (140 px, `SurfaceSizes`) unless the user
  collapsed it (`AppConfig.surfaces.collapsed`, `set-surfaces-collapsed`); otherwise it's one
  44 px row whose button shows it expanded over the bottom of the slots. Whatever the panel
  takes beyond the 995 px the slots need, the slots give up as padding: `SlotPanel.squeeze`
  (0..1, through `LiveTab`) shrinks the header by up to 30 px and the first tab's padding and
  spacing. F11 and the button at the end of the sidebar's transpose row toggle `full-screen`;
  Escape leaves MIDI learn — both in the `key-pressed` of a `FocusScope` around the whole
  window (key events go up from the focused element through its parents; `KeyBinding`s in it
  didn't fire), which gets the focus back when learn mode starts or stops. **The surfaces panel**
  (`src/surfaces.rs`: `SurfacesUi`): the devices shown are `AppConfig.surfaces.devices`, or,
  until the user picks (`None`), every device plugged in that has a known surface; a
  checklist like a MIDI input's (`AppState.surface-devices`, `toggle-surface-device`) picks
  them, and `sync_surfaces` hands the list to `Engine::set_surface_devices`. Each shown device
  is a `SurfaceView` of `SurfaceControlView`s (kind, label, cell, last value, pressed/moving),
  drawn by `SurfaceDrawing` (`SurfaceFader`/`Knob`/`Pad`/`Button`), or a note that no surface
  is known for it; the poll timer's `learn::Learn::poll` feeds the surface events of
  `Engine::take_mapper_reports` to `SurfacesUi::apply` (`Surface::read`; a pad's channel
  pressure goes to the held pads) and `tick` (faders/knobs stop showing as moving after
  250 ms; `AppState.surface-activity` lights the row's dot). **MIDI learn** (see
  `docs/design/midi-learn.md`): `src/mappings.rs` (`MappingsUi`, in `Rack::mappings`) keeps
  the mappings — with the scene: `SceneMapping`s per plugin key, saved in each plugin's
  `PluginState.mappings` in `slot.json` (slot presets, scenes, the session, undo; through
  `Rack::plugin_extras`) and replaced by `load_slot_preset`, with the bases of the plugin's
  modulated targets (`PluginState.values`, `MappingsUi::saved_values`, put back after its
  state file by `restore_values`; setBfree's through `Engine::restore_setbfree`, which sends
  them again once the plugin reports its loaded state); in every scene:
  `GlobalMapping`s (`RackTarget`: a plugin by slot and id, a panel position, a slot's or the
  master's volume, mute or transposition, the metronome's volume, play or tempo) and
  `ControlSettings` per `surface/label`, in `AppConfig.surfaces` (`MappingsUi::store`
  writes them back) — resolves them against what's loaded (`Concrete`; a plugin target's
  conversions go through its panel control, `profiles::Control::to_value`/`position`/
  `steps`/`text_at`, or a plain one) into the engine's setup (`build_setup`; not handed over
  if it does what the last did, so takeover state survives; rebuilt when `setup_dirty`, set
  by `refresh_slot`, panel rebuilds, surface changes and edits), and runs learn mode: the
  selected control (clicked in the panel — `learn-select` — or moved), the targets pane's
  rows (`LearnTargetRow`s), the right-click popup (a draft until "Assign"), and `LearnFlags`
  — per on-screen target 0/1/2 (not a target, possible, the selected control's), in
  `SlotState`'s `cell-learn`/`drawbar-learn`/`volume-learn`/`mute-learn`/`transpose-learn`
  and `AppState`'s master and metronome fields. `src/learn.rs` (`Learn`) is the glue: each
  poll it applies the reports (`Applied`: shows a plugin value like a mouse edit would, sets
  a volume in the config without sending it again, and does mute/transpose/metronome
  through their `AppLogic` handlers; `Waiting`: the panel's ghost), rebuilds the setup if
  needed, tells the engine of targets moved elsewhere (`sync_positions`, against what was
  last seen), and refreshes the view; it also handles every `learn-*` callback. A
  modulated target's base is kept per setup target (`MappingsUi::reported`, and `edited`
  for a knob dragged on screen); `MappingsUi::decorate` shows it on the panel's knobs
  (`PanelCell.modulated`/`mod-low`/`mod-high`/`mod-value`: `Knob`'s outer arc and dot),
  and a volume's base is what the config keeps.
  `ui/learn.slint`: `LearnPane` (over the scenes pane while `AppState.learning`),
  `TargetSettings` (mode, range, levels, momentary value, kept with the scene or in every
  scene, this parameter or this panel position), and `LearnTarget`, laid over each target
  widget while learning (outlined; filled and badged when the selected control's; click
  adds or removes, right-click opens its popup). **Look:** the Live view and sidebar follow the Penpot
  design (its components have Slint counterparts): `ui/theme.slint` has `Theme` (the design's
  greys in dark mode, a derived light variant otherwise — following `Palette.color-scheme` —
  with `Palette.accent-background`/`-foreground` as the primary colour, so the style's accent
  theming keeps working; sizes; the font), `Icons` (Lucide SVGs, recoloured with `colorize`)
  and `Artwork` (header photos). Bundled assets live in `ui/assets/`: Source Sans 3
  (`fonts/`, imported in `theme.slint`, the window's `default-font-family`), Lucide icons
  (`icons/`, lucide-static 0.460.0 — add new ones from the same version) and CC0/public-domain
  photos (`photos/`, 600×300, with `CREDITS.md`). `ui/widgets.slint` has the design's
  components: `KButton` (icon and/or label; `active` = the primary colour), `TextField`,
  `DropdownButton`, `Select` (a ComboBox look-alike on a `PopupWindow`), `ListItem`, `TabBar`,
  `Stepper`, `FxSlot` (an 80px square: the effect's inline display, `EffectRow.display`/
  `has-display`, fitted, with the name only on hover; else the name over a compressor/filter
  curve — `EffectRow.graph`, guessed from the name by `slot_view::graph` — or a faint +; it
  reports a click, or a drag once the pointer moves 4 px: `drag-moved`/`dropped`/
  `drag-cancelled`, `pressed-x/y`) and `ScrollIndicator`; `Knob`/
  `LabelledKnob` in `knob.slint`; `RangeSlider` and `SingleSlider` (the design's one-handle
  Single slider, never setting its own value) in `range_slider.slint`. The Settings window,
  plugin picker and parameter window still use
  std-widgets (with the new default font). `globals.slint` defines the host/UI interop
  contract (`AppState` = state Rust pushes in, `AppLogic` = actions the UI asks Rust to handle),
  per the project's Slint skill guidance; the structs exchanged live in `types.slint`
  (re-exported by `globals.slint`). **Preview data:** every `AppState` property the host fills,
  and `ParamsWindow`'s, defaults to `Platform.uses-mock-data ? MockData.x : …` — true only in
  the editor's preview/`slint-viewer`, and compiled away in the app — with `MockData` in the
  generated `ui/mock_data.slint`: a real session's UI state, written on exit when the app runs
  with `LIVE_KEYS_MOCK_DATA` (`src/mock_data.rs`: a `ToSlint` trait writing values as Slint
  literals, implemented per struct by `slint_struct!` from a field list — add new struct fields
  there, and new `AppState` properties to `generate` and to `MockData`). **Panel profiles**
  (`src/profiles.rs`, the slot's first tab; see "Panel profiles" in `docs/design/plugins.md`):
  `Profiles::panel(plugin id, params)` gives a `Panel` — an optional fixed section of drawbars,
  a pool of `Control`s (id, name, label, switch text, `Widget` knob/switch/lever/drawbar,
  `Target::Param(id)` or `Target::Setbfree(function)`, `choices`, `Overrides`
  min/max/steps/invert/default) and the shown controls (`row`: pool indices, at most `max` — `MAX_CONTROLS` (10), or
  `MAX_CONTROLS_WITH_DRAWBARS` (5) under drawbars). `Control::read`/`write`
  convert between a target's value and the UI's `CellValue` (`Position` 0..1 — log-scaled
  through `slot_view::position`/`knob_value` —, `On`, or `Index`: the nearest choice, or a
  drawbar's level 0..=8; setBfree's drawbars are inverted, `level = rint((127 − v)·8/127)`), and
  `value_text` shows it. Shipped profiles are `crates/gui/profiles/*.json` (setBfree, mda
  EPiano, Helm, Surge XT and Vital — the CLAP builds — and our piano, `builtin:piano`), compiled in (`SHIPPED`; add new ones
  there); a control's `{ "param": "<symbol>" }` target matches `ParamInfo::symbol`, and is left
  out if the plugin lacks it. User profiles are `<data dir>/live-keys/profiles/*.json` (same
  format, replacing a shipped one wholesale), and `rows.json` there is the user's row per
  plugin (`UserRow`: the shown controls' ids, plus `param:<id>` controls added via "More
  parameters…", which stay in the pool), written by `add_control` (appends; replaces the
  control at a given index when full) and `remove_control`, and removed by `reset_row`. A plugin without a profile gets every parameter
  (`param_control`) as its pool and its first ten continuous ones shown; a default
  whose controls went missing is filled from the pool, up to its own length; a user's choice
  just loses the missing ones. Unit-tested (value mappings, row resolution and adding, the
  shipped files parsing). `config.rs` handles settings persistence (`dirs` +
  `serde_json`). `main.rs` wires it all to `live-keys-engine`/`live-keys-lv2-ui`/
  `live-keys-vst3-ui`. **MIDI inputs** (`AppConfig.midi_inputs`, never empty; input 1 can't be
  removed; on a first run, every device present but Midi Through becomes input 1's devices): the sidebar lists them
  (`MidiInputItem` in `sidebar.slint`: its number, then a dropdown summarising its devices that opens
  a `PopupWindow` checklist of every device present plus the input's remembered ones marked
  "(not connected)" (`src/midi_inputs.rs`: `MidiInputsUi`, its per-input device models refilled
  in place so an open checklist survives updates). Devices stay assigned until unticked. Every
  `MIDI_PORT_POLL_INTERVAL` (1 s) the GUI calls `Engine::sync_midi_inputs`, which reconnects
  assigned devices that reappeared. Each slot has a numbered toggle button per input
  (`SlotConfig.midi_inputs` mask, default input 1; `SlotState.midi-inputs: [bool]`); a new
  input starts off in every slot, and removing one shifts later inputs' bits down
  (`remove_input_bit`). Changes go through `apply_midi_inputs`. Plugins are added through one **plugin picker** popup
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
  appends to the matching effect section; for `AppState.master-slot` the picker shows audio
  effects only (no tabs). **The sidebar** (`ui/sidebar.slint`, after the design's "Right
  sidebar"): the master transposition (`TransposeSection`: a `Stepper`,
  `set-master-transpose` → `AppConfig.master_transpose`, ±`MAX_TRANSPOSE`, added to every
  slot's own in its `MidiFilter` — `SlotConfig::filter(master_transpose)` — and not part of
  scenes), the metronome (`MetronomeSection`: a volume `LabelledKnob`, `BeatDots` from
  `AppState.metronome-beats`/`-beat` — which the poll timer sets from `Engine::metronome_beat`
  — a BPM `Stepper` (`signed: false`; Enter/Escape leave its field, and it shows values set
  from elsewhere, like Tap's, even while focused), play/stop under the knob, then a
  time-signature button and a subdivision button (♫) opening `MenuPopup`s, and Tap), the master output (`MasterSection`: a
  `SingleSlider` in dB like a slot's volume, its value shown beside the title while hovered
  or dragged, an `FxStrip` of `AppState.master-effects`
  (exported from `slot_panel.slint`, as slot `AppState.master-slot`) with its own
  `PresetPopup`, a mute button and
  the output device `Select`), the MIDI inputs, then Settings and MIDI panic
  (`midi-panic` → `Engine::panic`). The metronome's settings are `AppConfig.metronome`
  (`src/metronome.rs`: `MetronomeConfig` — BPM, time signature and subdivision as indices into
  `SIGNATURES`/`Subdivision::ALL`, volume 0..1, squared into the click's level — plus
  `TapTempo`: the average of up to five taps, starting over after a 2 s pause; unit-tested);
  whether it plays isn't saved (it starts stopped), and playing starts the output stream if no
  plugin has. Every metronome edit goes through `apply_metronome` in `main`. Master volume and
  mute are `AppConfig.master_volume_db`/`master_muted` (`AppConfig::master_gain`). The master
  chain lives in the session, not in scenes or slot presets. On the Rust side it's slot
  `MASTER_SLOT` too: `slot_of` finds its plugins, and `Rack::refresh_slot` hands it to
  `refresh_master` (its effect rows and `AppState.master-status`, through the window handle in
  `Rack::app`); `mark_slot_changed` and `set_current_slot_preset` only note a session change
  for it. **The Settings window** (`ui/settings_window.slint`: `SettingsWindow`, created once
  at startup, shown by `open-settings`) has the plugin folders (add/remove), Rescan, every
  discovered plugin with its kind and format, and the `AboutSlint` attribution; being a window
  of its own (own globals), its properties are set and callbacks handled from Rust
  (`refresh_plugin_folders`, `refresh_plugin_list`, `set_scanning`). **Plugin scans** run after the window is up, so it shows at once: `Scanner`
  holds the `PluginScan` and a timer that runs it in `SCAN_SLICE`s every `SCAN_TICK` (the VST3
  half is on this thread) and installs it once done — at startup with the configured extra
  folders, later from Rescan, which `AppState.scanning` replaces by a spinner meanwhile (the plugin
  picker shows one instead of its list, and is refiltered once the scan is installed). The
  plugins already loaded keep playing until a rescan is installed. At startup nothing is
  loaded until the first scan is installed and the session restored, so until then
  `AppState.starting` lays a spinner over the Live tab (over it, not instead of it, so the
  window still sizes itself to the tab), and the session is neither autosaved nor saved on
  exit (`started` in `main`) — that would overwrite the last session with an empty rack. The Live tab (`live_tab.slint`) is the scenes pane, then
  one `SlotPanel` (`ui/slot_panel.slint`) per `AppState.slots` entry in a grid of
  `SlotGrid.columns` (3) equal cells (`SlotState`: `instrument: EffectRow` — key -1 when empty —
  `midi-effects`/`audio-effects: [EffectRow]`, the panel's `drawbars`/`cells`/`pool`, `artwork`, `muted`,
  `closed-name`, status, routing; an `EffectRow` is one plugin's name/key, whether its
  GUI/parameter window is open, its FX-square curve and its current preset). A slot with
  nothing in it is a big + (picks an instrument) with a "Slot presets…" popup; one just cleared
  by ✕ shows 'Closed "<name>"…' with Undo and Dismiss. Otherwise: a **header** — the photo
  (`Artwork.photos[artwork]`, `slot_view::artwork` matching words in the instrument's name)
  under a dark gradient, buttons for the instrument's own GUI (gear), mute (`toggle-slot-mute`:
  `SlotConfig.muted`, silencing the slot whatever its volume; part of `SlotRouting`, so
  scenes recall it; ✕ unmutes) and ✕ (`clear-slot`), the instrument's name (click: a menu
  to replace or remove it or open its parameter window; "+ Instrument" without one), a volume
  `Knob` (dB, value shown while hovered), and the instrument's preset `DropdownButton`. Then a
  `TabBar` (the selected tab is per slot, in the UI only) over a scrolling area
  (`Flickable` + `ScrollIndicator`; tab 1 is laid out to fit and never scrolls): tab 1 the
  instrument's **panel** (`ui/panel.slint`, from
  its profile, see `src/profiles.rs`) — an optional fixed section of `Drawbar`s
  (`DrawbarSection`; `SlotState.drawbars: [DrawbarCell]`, level 0..=8, dragged vertically or
  scrolled) and the controls, 56 px `PanelCellView`s in a `PanelControls` (`SlotState.cells:
  [PanelCell]`) — one row spaced evenly under drawbars or when there are 3 or fewer, else
  zigzag over two rows (1st, 3rd, 5th… on top, the others below between them), so up to ten
  fit; each: a `LabelledKnob` (position 0..1; `steps` for stepped parameters), a stepped
  knob (a control with choices: snaps to them, shows the option's label), a `Toggle` (the
  design's switch) or a `Lever` (2–3 options: click a part, drag sideways or up/down, scroll).
  While there's room, a last empty cell (`widget` -1) shows a + that opens its menu. The tab
  spaces its parts (the drawbars, the controls, the FX strip) evenly down the slot
  (`LayoutAlignment.space-evenly`, the content exactly the visible height), and the drawbars
  evenly across. Right-clicking a cell (`ContextMenuArea`; any click on the +) lists the pool
  (`SlotState.pool: [PanelPoolRow]` — at most 16 plus the shown ones) as a checklist: shown
  ones ticked, choosing one takes it off, choosing another adds it — instead of this cell's
  when full (`set-panel-cell`), "More parameters…" (`ParamSearchPopup`: a search
  over every parameter, filtered in Rust — `open-param-search`/`filter-param-search`/
  `pick-param-search`, `AppState.param-search-rows`) and "Reset row" (`reset-panel-row`);
  the choice is per plugin (`Profiles::add_control`/`remove_control`/`reset_row`), so `Rack::rebuild_panels` rebuilds
  every slot with that instrument. `Rack` keeps each slot's `profiles::Panel`
  (`Rack::panels`, built by `refresh_panel` when the instrument changes) and turns it into
  rows with `slot_view::panel_cells`/`panel_drawbars`/`panel_pool`; edits
  (`set-cell-position`/`-on`/`-index`, `set-drawbar`) go through `Control::write`
  (`Rack::write_control`, which then shows the value like `set-param` does and marks presets
  changed), and values are re-read (`Control::read`, `Rack::refresh_panel_of`) on every
  parameter change, preset load and `idle_plugins` re-read, and for each plugin in
  `Engine::take_setbfree_changes` after `drain_ui_events` — and the audio
  effects as an `FxStrip`: one row of `EffectSquare`s (+ appends one), three fitting a slot,
  scrolling sideways by dragging in the gaps between squares, the mouse wheel (anywhere on
  it) or a thin scrollbar under it (shown when they don't fit; its thumb can be dragged);
  tab 2 `SlotPresets`; tab 3 the
  MIDI effects (another `FxGrid`), input toggles, channel `Select`, note and velocity
  `RangeSlider`s, and a transpose `Stepper` (`set-slot-transpose`: `SlotConfig.transpose`,
  ±`MAX_TRANSPOSE` (48) semitones, into the slot's `MidiFilter`; part of `SlotRouting`, so
  scenes recall it). **An effect's square** (`EffectSquare`, an `FxSlot` with the chain row):
  clicking opens its own GUI (`gui-supported`), else its parameter window, and clicking while
  either is open closes it (the square has the primary colour's outline while one is);
  right-clicking (`ContextMenuArea`) gives Open/Close GUI, Parameters, Presets… (the
  slot's `PresetPopup`), Move left/right and Remove; dragging it moves it within its chain
  (`FxStrip`/`FxGrid`: the original fades, an opaque copy follows the pointer, a `DropLine`
  marks the gap it lands in — before the square under its centre when moving back, after it
  when moving forward — and dropping calls `move-effect`; in a strip, holding it within 24 px
  of either end scrolls the row). **Inline displays** (`src/inline_displays.rs`): the poll
  timer's `Rack::render_inline_displays` renders, for every row of a section whose tab shows
  (a slot's audio effects on tab 1, MIDI effects on tab 3 — `set-slot-tab` reports the
  slot's tab — the master chain always), the displays that asked (`Engine::
  inline_display_dirty`), at most every 33 ms each, at 78 px × the window's scale factor
  (redrawn when that changes), into `EffectRow.display`; `plugin_row` takes the last one. **Clear with undo:** `clear-slot` first saves the chain as a slot
  preset in `<data dir>/live-keys/undo/slot-<n>/` (`slot_view::snapshot`) and keeps it with the
  routing and the slot preset shown in `Rack::closed`; `undo-clear` loads it back through
  `load_slot_preset` and restores those; `dismiss-closed`, or anything loading into the slot
  (`refresh_slot` sees plugins), forgets it. Scene recalls that clear a slot don't keep one.
  Each plugin's presets (instrument from its dropdown, an effect's from its popup) open
  `PresetPopup` (`ui/preset_bar.slint`, a `PopupWindow` closing itself when the browser closes
  in Rust): the current preset (with a grey "changed" once a parameter moved since it was loaded or saved
  — `Rack::mark_changed`, on host-side edits and on plugin-GUI edits that actually change a
  value, so GUIs echoing values back don't count; LV2 plugin state changed outside its ports
  isn't seen), and the browser — a search field (every word must appear in the
  name; Enter loads the first match) over a list with a star per row to make it a favourite
  (`AppConfig.pinned_presets`, by `PresetInfo::id`, listed first) — plus "Save…" (an inline
  name field, prefilled with the current user preset's name — saving the same name
  overwrites) and "Delete" (the current preset, user presets only, confirmed inline). Only
  one browser is open at a time: it lives in `AppState` (`preset-browser-key`,
  `preset-rows`, `preset-count`, `preset-query`) and in Rust as `Rack::browser`
  (`PresetBrowser`: the plugin's presets, read when it opens, the query and the shown ones),
  filtered in Rust like the plugin picker. Preset errors go to the slot's status line.
  **Slot presets** (`src/slot_presets.rs`, `ui/slot_presets.slint`, a slot's second tab): a
  slot's whole chain under a name — a folder
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
  (`Rack::mark_slot_changed`). Slot presets don't include the MIDI routing or volume (those
  are about the keyboard split and the mix, which scenes cover).
  **Scenes** (`src/scenes.rs`, `ui/scenes.slint`: `ScenesPane`, a fixed-width pane left of the
  slots — a search field, then a list starting with "+ New scene", which opens the save form):
  what recalling does to each slot — `SlotMode` restore, clear (`clear_slot`: remove
  its plugins and reset its routing and volume to the defaults) or leave unchanged — under a
  name. A folder
  `<data dir>/live-keys/scenes/<name>/` with `scene.json` (per slot: its index, mode, and,
  if it was saved as restored, a `SavedSlot`: its `config::SlotRouting` — MIDI inputs mask,
  channel, note/velocity ranges, transposition, volume, mute — the library slot preset it showed, and its chain
  subfolder) plus per saved slot a subfolder `slot-<n>/` that is a complete slot preset
  (`slot_presets::write`; saved via `slot_presets::replace_dir`, the same temp-folder swap).
  Save… and Edit share one form (`Rack::scene_form_slots`, a `SceneSlotChoice` per slot: mode
  and whether restore is possible), a segmented Restore/Clear/Unchanged per slot. Saving
  defaults to the current scene's modes, else restore for slots with plugins; the same name
  overwrites. Edit (`scenes::update`) renames — refusing a name another scene has — and
  changes modes; a slot without saved data can't be set to restore, one with data keeps it
  whatever its mode, so it can go back to restore (overwriting a scene copies the saved data
  of its non-restored slots into the new save). The list is in the user's order — rows are
  dragged (`SceneItem`'s touch area: past 4 px a press becomes a drag, with a drop line;
  `move-scene`) and the order kept in `<root>/order.json` (`scenes::move_to`; new scenes go
  last). Right-clicking a row (`ContextMenuArea`, on top of the touch area, which would
  otherwise take the right-click) offers Load, Save (overwrite: the slots' current setups,
  keeping its modes), Edit…, Duplicate (`scenes::duplicate`: "<name> copy", "copy 2", …,
  placed right after it) and Delete…; edit and delete act on `Rack::scene_target` (set by
  `begin-scene-edit`/`begin-scene-delete`; -1 would mean the current scene). Each row shows a boxed slot number per
  restored slot and a crossed-out one per cleared slot (`SceneRow.modes`, `SlotHint`).
  Clicking a scene recalls it (`load_scene`): restored slots go through `load_slot_preset`
  (plugins already in place are kept) and get their routing and volume back, cleared ones go
  through `clear_slot` (also behind each slot's ✕), the rest are left alone. A
  grey "changed" shows once a restored or cleared slot's chain, any of its plugins'
  parameters, a plugin or slot preset, its routing or its volume changes
  (`Rack::mark_scene_changed`, called from `mark_slot_changed` and the routing handlers).
  Scene state lives in `Rack` (`scene_store`, `scene_query`, `current_scene`, `scene_rows`)
  and is pushed to `AppState` by `Rack::refresh_scenes`, which the poll timer runs whenever
  `scenes_dirty` is set, since most
  changes happen where there's no window handle.
  **Session** (`src/session.rs`): the rack as last saved — per slot a complete slot preset
  in `<data dir>/live-keys/session/slot-<n>/`, and the master chain likewise in `master/`
  (`slot_presets::write`, the folder swapped in
  via `replace_dir`), and `session.json` with each slot's library slot preset, which plugins
  had their GUI or parameter window open (`session::Position`, from
  `SlotChain::position_of`), and the current scene with its changed flag. Routing, volumes,
  devices etc. aren't in it: the config has them. `save_session` runs on exit (always — some
  plugin settings, like setBfree's atoms, change unseen — unless the last session wasn't
  restored and nothing changed since) and from the autosave timer: once
  `Rack::session_changed_since` (set by `session_changed`, from the change markers, slot and
  plugin preset changes and scene changes) is `AUTOSAVE_DELAY` old and every slot has been
  silent (`Engine::take_slot_peak` ≤ `SILENCE`) for `AUTOSAVE_QUIET` — saving lends LV2
  plugins with state, which silences them, so never while playing. `restore_session` runs
  once the first plugin scan is installed: each slot through `load_slot_preset`, then
  the slot presets and scene shown, then the plugin windows reopened (`invoke_open_plugin_gui`,
  `invoke_toggle_params`). A marker file (`session.restoring`) is written for the duration:
  if it's there at startup, the last restore crashed or hung, so this one is skipped (with
  a note in slot 1's status line) and the marker cleared, so the next start tries again.
  Closing the main window quits the app (`on_close_requested` → `quit_event_loop`), even
  with parameter windows open, so the exit save always runs. `AppLogic` actions take the slot index for structural changes
  (`pick-plugin`, `remove-effect(slot, section, index)`, `move-effect(slot, section, from,
  to)` — `section` 0 = MIDI, 1 = audio, …) and a `PluginKey` (as `int`) for per-plugin ones
  (`open-plugin-gui`, `toggle-params`, `set-param`, `save-preset`, `delete-preset`); the
  preset browser's actions act on the open browser's plugin. The engine stream is started lazily by the
  first load (on the audio device chosen in Settings) and restarted — closing all plugin GUIs first,
  since their instance handles go stale (and keys change) — when the device selection changes
  while running. Replacing/removing a plugin likewise closes its GUI and parameter window
  first (`Rack::forget`). On the Rust side, `Rack` owns the slots' `VecModel`s, every open GUI
  keyed by `PluginKey` as an `OpenEditor` (`Lv2(PluginUiHandle)` — its own thread — or
  `Vst3(Vst3Editor)`/`Clap(ClapEditor)` — main thread), every open parameter window (`ParamWindow`: the window and
  its rows model), and the preset last loaded/saved per plugin (`current_presets`, with its
  changed flag); the poll timer calls
  `Rack::poll_uis`, which pumps open VST3 editors and CLAP GUIs (`idle`) and forgets any GUI or parameter
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
  Meant as the base for
  future macro knobs/MIDI-bound parameters. Each slot also has channel (`Select`, index 0 =
  omni), note-range and velocity-range (each one two-handle `RangeSlider` — a custom component
  in `ui/range_slider.slint`, since std-widgets has none; note names shown using C4 = 60) and a
  volume knob in dB (`MIN_VOLUME_DB` = -60 means muted,
  up to +6; converted to linear gain in `config::SlotConfig::gain`). These live in
  `AppConfig.slots` (`SlotConfig`) as the GUI's source of truth and are pushed to the engine on
  startup and on every edit; `Rack::refresh_slot` only touches a row's plugin/GUI fields,
  `Rack::set_routing` its filter/volume fields. Since a recalled scene changes them from Rust,
  the channel `Select`, both `RangeSlider`s, the volume `Knob` and the panel's knobs follow
  their values through `changed source =>` helpers (dragging replaces their bindings); the
  panel's `Toggle`, `Lever` and `Drawbar` never set their own value, only report the new one. Edits that fire continuously (knob drags)
  mark the config dirty and the poll timer saves it, rather than writing the file per step.

- **`crates/surfaces`** (`live-keys-surfaces`) — control surfaces: MIDI controllers as banks
  of faders, knobs, pads and buttons, each control knowing its message (CC, note, pitch bend,
  channel pressure) and channel; see `docs/design/midi-learn.md`. JSON (format in the crate's
  doc comment), shipped ones compiled in (`SHIPPED`: Nektar Impact LX88+ and Akai LPD8 — the
  LX88+'s faders/pots from Nektar's guide, its pads and the LPD8 still guesses to check on the
  hardware) and the user's in `<data dir>/live-keys/surfaces/` replacing a shipped one by `id`
  (`Library::load`). `Surface::from_json` places every control (`ControlKind::cell_size`,
  `Numbering` top/bottom-first) and indexes them by `(channel, message)`; `Surface::matches`
  picks a surface for a device name (case-insensitive substrings), `Surface::read` turns a
  `MidiEvent` into `(control, Reading)` (a position, a press with velocity, a release), and
  `pad_pressure` finds the pads a channel-pressure message may belong to. Unit-tested.

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
  GUI side is pumped like the app's poll timer (edits forwarded, `drain_ui_events` to the GUI,
  `idle_plugins` for CLAP). A CLAP case's parameter check also asks the plugin for the value.

- **`crates/instruments/sfz`** (`live-keys-sfz`) — our own SFZ sampler engine, the sound core
  for our sampled (and partly modelled) instruments; see `docs/design/own-plugins.md`. No host
  code and no dependency on the other crates: a front-end (the built-in `PluginFormat`, a
  standalone plugin) wraps it. `parse.rs` preprocesses (`#define`s applied in order — Salamander
  redefines `$VEL` between includes —, `#include` relative to the root file, also mid-line;
  comments) and tokenizes into `Section`s (a value runs to the next opcode, header or line end,
  so paths can have spaces). `region.rs`: `Region` with typed fields, built by applying the
  `<global>`, `<master>`, `<group>` and `<region>` opcodes in order (`Region::apply`; unknown
  opcodes are collected and logged once, not fatal); `*_onccN`/`*_ccN`/`*ccN`/`*_curveccN`
  become `CcMod`s (`Target`: amplitude multiplies, the rest add; pitch/volume/pan/width/position
  follow the controller live, the rest are read at note-on). `curve.rs`: built-in curves 0–6
  and `<curve>`s. `sample.rs`: a `Decoder` (FLAC via claxon, WAV via hound; block by block,
  `f32`, at most 2 channels) and `Sample`: interleaved with zero padding around it for 4-point
  interpolation, held whole or only its **head**. **Streaming** (`stream.rs`, `Preload`): by
  default each sample keeps its first `DEFAULT_HEAD_FRAMES` (8192) frames — plus the furthest
  `offset` a region can start it at — in memory (Salamander: 58 MB instead of 2.6 GB). A voice
  starting on a sample counts itself as a user of its `Slot` and, if the full data isn't
  loaded, queues a request (one `rtrb` ring per synth, `StreamClient`); a dispatcher thread
  hands it to one of 4 loader threads, which decode the whole file into a `StreamData` buffer
  and publish it as they go (`available`, release/acquire), so voices read past the head as
  soon as frames exist (`Sample::window`). Not there yet = an underrun: silence, counted
  (`Instrument::underruns`). Data with no users for 5 s is freed by the dispatcher (swap the
  pointer to null, re-check users, put it back if one appeared — SeqCst, so a voice holding it
  is always seen); the audio thread never allocates, frees or waits. `Preload::Whole` loads
  everything (offline rendering). Dropping the `Instrument` joins its threads: not on the
  audio thread. `instrument.rs`: `Instrument::load`/`load_with`/`from_sfz_with` (samples
  loaded once each, on all cores; regions whose sample fails are dropped with a warning;
  `load_with_base` for an SFZ kept apart from its samples), per-key indexes of
  attack and release regions, controller-triggered regions, keyswitch range, initial CCs (MIDI
  defaults, then `set_cc`/`set_hdcc`). `synth.rs`: `Synth` — real-time safe (fixed voice
  pool, no allocation): `midi`/`note_on`/`note_off`/`control_change`/`set_cc`/`pitch_bend`,
  `render(left, right)` (overwrites). Conditions: key, velocity, channel, `locc`/`hicc`,
  `lorand`/`hirand`, round robin, keyswitches (`sw_last`); `trigger`
  attack/release/release_key/first/legato;
  `on_loccN` regions fire when the controller *enters* the range (pedal noises; played at full
  velocity). **Sustain pedal:** a key let go while it's down keeps sounding, and its release
  (and its release-trigger regions, with `rt_decay` over the time held) come when the pedal
  lifts — except `trigger=release_key` regions (key-up noises) and `sustain_sw=off` ones, which
  play at key-up regardless. `off_by`, `polyphony` (per group) and `note_polyphony` (per key and group) fade the
  oldest voices (`off_mode`/`off_time`); a full pool steals the oldest fading, then released,
  then sounding voice. Gains: velocity curve with `amp_veltrack` (sfizz's formula), velocity
  crossfades, `amp_keytrack`, then a 2×2 width/pan matrix ramped per block. **Modulation**
  (from `live-keys-dsp`): one filter per voice when `cutoff` is given (`fil_type`, `resonance`,
  `fil_gain`, key/velocity tracking), the filter and pitch envelopes (`fileg_*`/`pitcheg_*`,
  depth in cents; sustain defaults to 0 as in SFZ) and SFZ v1's three LFOs (`amplfo_*` in dB,
  `fillfo_*`/`pitchlfo_*` in cents; sine, per voice; `freq`, `delay`, `fade`), with CC targets
  for cutoff, resonance, the envelopes' depths and the LFOs' depths and rates. `render_voice`
  works in `CONTROL_BLOCK`s (32 frames): pitch, cutoff and the amplitude LFO (ramped) are set
  per block from `Live` (what follows the controllers, read once per `render`), then per
  frame source (`next_frame`) → filter → amplitude envelope × LFO → matrix. Generators
  (`sample=*sine`, `*saw`, `*noise`…) are the hook for modelled layers. Not yet: SFZ v2's
  numbered `lfoN_*`/`egN_*`, a second filter, effects, loop points from the file.

- **`crates/instruments/sfz` controls** — `controls.rs`: `Control` (symbol, name, range,
  default, unit, choices, `ControlTarget::Cc(n)` — scaled linearly onto 0..1 — or
  `Keyswitch(keys)`), `Control::apply(synth, value)` (real-time safe), and the instruments'
  state format: `write_state`/`read_state`, `symbol = value` lines, `#` comments, unknown
  symbols ignored, missing ones at their defaults. `Synth::set_keyswitch` selects a keyswitch
  directly; `fil_gain` is a CC target too. A bell or shelf at 0 dB is skipped
  (`Filter::is_identity`).

- **`crates/instruments/dsp`** (`live-keys-dsp`) — DSP building blocks for our instruments,
  shared by the SFZ engine and any synth later; no dependencies, nothing SFZ-specific, all
  `Copy` and real-time safe. `filter.rs`: `Filter` — Simper's trapezoidal SVF (stable under
  modulation) with every mode as a mix of its outputs (low/high/band-pass, notch, all-pass,
  bell, shelves), one-pole low/high-pass, and 4-/6-pole cascades (Butterworth Qs, resonance on
  the last stage); `set(cutoff, resonance_db, gain_db, rate)` each control block, `process(channel,
  x)` per sample, two channels of state. `lfo.rs`: `Lfo` (sine, triangle, saws, square,
  sample-and-hold; delay, fade-in, start phase; `advance(n)` at control rate). `envelope.rs`:
  `Dahdsr` settings and `Envelope` (linear attack, exponential decay/release to −60 dB in the
  given time, done at −80 dB; `tick` per sample, `advance(n)` for modulation). `convert.rs`: dB,
  cents, note ↔ Hz. Unit-tested (filter responses measured on sines, LFO timing, envelope).

- **`crates/instruments/piano`** (`live-keys-piano`) — the Salamander piano, host-agnostic: its
  own SFZ (`salamander.sfz`, compiled in as `SFZ`; Salamander's file with the key-release noises
  on `release_key`, plus controllers 23–25 for brightness — a ±12 dB high shelf at 1.5 kHz —,
  tune and width, neutral by default), `CONTROLS` (nine `live_keys_sfz::Control`s, each an SFZ
  controller of that file or the natural/retuned keyswitch; their defaults equal the SFZ's
  `set_hdcc`s, unit-tested), `PRESETS` (shipped: name + the controls that differ from their
  defaults), `default_sample_dir()` (`<data dir>/live-keys/samples/salamander/`) and
  `load(sample_dir)` (an error saying the samples aren't installed if they aren't).

- **`crates/builtin`** (`live-keys-builtin`) — our instruments as plugins of the built-in format
  (`PluginFormat::Builtin`), the only crate that knows both an instrument core and live-keys'
  plugin API. `DEFINITIONS` lists the cores (id, name, controls, presets, voices, loader);
  `BuiltinHost` lists them (`builtin:` + id), instantiates a `BuiltinPlugin` (a `HostedPlugin`
  around a `Synth`: renders between MIDI events' frame offsets; `set_param` = the control's
  `apply`), loading an instrument once and sharing it (a `Weak` per core) while any instance
  holds it, and handles presets: shipped ones (`shipped\t<name>` ids) and user ones as files in
  the core's state format (`<data dir>/live-keys/presets/<core id>/<name>.lkpreset`, `file\t<path>`
  ids); state files (`.lkstate`, for slot presets and the session) are the same format.
  Parameters are the controls: id = index, symbol = the control's symbol, values in its range
  (a choice: 0, 1, …).

- **`src/main.rs`** (binary crate `live-keys`) — the original headless CLI, now a thin wrapper
  around `live-keys-engine` (loads its one `--plugin` into slot 0), then loops calling
  `idle_plugins` and `collect_retired`. Uses `clap` (derive) for CLI args — the argument
  parser, not the plugin format.

### Real-time audio thread rules

The audio callback must never allocate, lock, or perform blocking syscalls. This is why:
- `MidiEvent`/`ParamChange` are fixed-size and moved via `rtrb` rather than `Mutex<Vec<_>>`.
- `HostedPlugin::process`/`audio_output` never materialize a slice-of-slices; adapters pass
  iterators straight through to the underlying plugin API.
- All buffers (MIDI batch, per-channel audio scratch) are pre-allocated outside the closure
  passed to `cpal`'s `build_output_stream`.
- MIDI learn's values come on a ring of their own (`MapperCommand`, drained after the
  engine's commands), so the mapping thread never shares the engine's producer.
- Plugins enter the callback already instantiated (boxed on the control thread) and leave it
  via the retired ring buffer, so neither construction nor teardown runs on the audio thread.
  A new stream's callback picks its `AudioState` up with one `try_lock` (never waits; nobody
  else holds that lock once the stream runs) and hands it back by ring on `Detach`.
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

- Changing the output device to one that can't run at the current sample rate reloads every
  plugin (LV2 fixes the rate at instantiation); their settings are carried over through the
  session, so only as faithfully as each plugin saves its state, and slots are silent while
  they reload. Rescanning plugins empties the rack outright.
- After a VST3 state load (preset or slot preset), the parameter values read back from the
  controller can be stale: Surge XT applies a loaded state on its audio thread, and its JUCE
  controller only catches up asynchronously, which doesn't happen without its editor open
  (confirmed: the component's state after loading equals the saved one byte for byte, while
  `getParamNormalized` keeps the old value). The sound is right; the parameter window and the
  recorded values may not be. Surge also calls `restartComponent(kParamValuesChanged)`, which
  is ignored.
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
- The rack is a fixed `SLOT_COUNT` (6) parallel slots, each a serial MIDI effects → instrument
  → audio effects chain (max 8 of each kind of effect) with its own MIDI filter and volume,
  summed into one output, then through the master chain and gain.
  Not a general audio graph: no routing between slots, no sends/parallel branches, no
  sidechains, no per-effect bypass or dry/wet (beyond what an effect offers itself), no latency
  compensation, no limiter (several loud slots can clip; the master chain can hold one), no MIDI
  velocity curves. It follows the intended **in-process** direction: every loaded plugin's audio is mixed in our own code,
  with only the final mix reaching `cpal` — not one independent OS audio stream per plugin
  (which would only work via OS-level mixing like PipeWire/JACK, isn't cross-platform,
  and would make routing plugin output through our own EQ/compression/mixing code
  impossible once the OS has already mixed it). This was decided explicitly when
  weighing out-of-process plugin isolation (see below) against this direction.
  An effect's main audio inputs get the previous chain stage's output; LV2 CV inputs and
  VST3 secondary input buses (sidechains) are still fed silence, since nothing routes to them.
  MIDI effects run in block steps on the audio thread: output events keep their frame offset
  within the block, but there's no transport (tempo-synced MIDI effects see the fixed 120 BPM,
  not playing — the metronome's tempo doesn't reach plugins; passing it on, as VST3's
  `ProcessContext`, CLAP's transport and LV2's `time:Position`, is a possible next step). Only 3-byte channel messages pass between stages (no sysex); a MIDI effect's
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
- CLAP hosting, minimal like VST3's: `request_restart` is ignored (no deactivate/reactivate for
  new latency or ports), `params.rescan` only re-reads values (no new parameters or metadata),
  no `audio-ports-config`/`audio-ports-activation`, `thread-pool`, `note-name`, `voice-info`,
  `remote-controls`, `context-menu`, `track-info`, `preset-load`'s host callbacks; no
  transport (null — free-running); no note expressions, polyphonic modulation, note ids,
  parameter cookies, MIDI 2 or sysex; only output port 0 is read. The plugin's `PARAM_VALUE`
  output events are taken as its GUI's edits (recorded and shown), gesture events ignored.
  Preset discovery reads every preset file the first time a plugin's presets are listed (~2 s
  for Surge XT, on the main thread: the app stalls once). User presets are this host's own state
  files, since CLAP has no shared format for them (other hosts won't list them). After a state
  load, values are re-read 100 ms later; a plugin that takes longer shows stale values.
- A VST3 editor or CLAP GUI runs on the GUI's main thread (see `crates/vst3-ui-host`,
  `crates/clap-ui-host`), so — unlike an LV2
  GUI, which wedges only its own thread — one that hangs inside a call freezes the
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
3. **Phase 2b (done, core) — CLAP** via the `clap-sys` bindings: DSP, parameters, notes/MIDI,
   state, preset discovery and the plugin's own GUI (see `crates/clap-host`/`crates/clap-ui-host`).
   Open follow-ups are under "Known simplifications".
4. **Phase 3 — VST2/LXVST** via `vst2-sys`/bindgen once phases 1–2 are stable. "LXVST" is just
   the Linux-native naming convention for VST2 `.so` builds (same ABI as Windows/Mac VST2,
   scanned from Linux-specific paths like `~/.lxvst/`) — not a separate format. Lowest priority;
   deprecated/unlicensable for new distribution, mainly for loading legacy plugins.

Each format gets its own adapter crate implementing `HostedPlugin`, keeping per-format unsafe/FFI
code isolated from the core audio graph and MIDI routing. Our own instruments are a format of
their own, `Builtin` (`crates/builtin`), plain Rust with no FFI.

Plans beyond the formats — integrated first-party plugins, per-plugin panel profiles, user
mapping of third-party plugins, and inline effect views (Ardour's LV2 inline-display
extension), recommended third-party plugins and their install window — are in
`docs/design/plugins.md` (a draft; check it before working on any of these, and keep it in step
with what's decided). Ideas for our own instruments (the Salamander piano first) are in
`docs/design/own-plugins.md`. MIDI learn — control surfaces in a bottom panel, hardware controls mapped to
on-screen targets — is designed in `docs/design/midi-learn.md` (phases 1 and 2 built: the
panel, mappings and learn mode; the rest still a draft).

## GUI

Slint (declarative `.slint` markup + Rust logic) — chosen over egui for a more native-feeling UI
without a hard GTK/Qt dependency. See **`crates/gui`** in Architecture above for the two-tab
structure and interop pattern, and **`crates/lv2-ui-host`**/**`crates/vst3-ui-host`** for how a
*plugin's* own GUI (as opposed to this app's GUI) gets hosted — that part needed native window
embedding via X11 (`x11rb`, plus `suil` for LV2), independent of Slint, exactly as anticipated
before either was built.

**Wayland (decided, keep as is):** the app's own windows are native Wayland (Slint's winit
backend picks it when `WAYLAND_DISPLAY` is set; nothing forces X11), while every plugin GUI —
LV2 X11 UIs with our `x11rb` host window, VST3 editors, External UIs — is an X11 window
through Xwayland. That's forced by the formats: VST3 on Linux defines only an X11 editor
window type (the SDK's newer optional Wayland interface is practically unadopted), LV2 has no
established Wayland UI type, and X11 windows can't be embedded in Wayland ones — which costs
nothing, since plugin GUIs are separate top-level windows anyway. Known costs of the mix:
plugin windows can't be tied to the main window (transient/always-on-top/minimise together),
can look blurry under fractional scaling (unless the compositor lets X11 apps scale
themselves), and focus between the two can feel slightly off. Forcing the whole app onto X11
(`WAYLAND_DISPLAY` unset) would allow tying windows together but loses crisp HiDPI for the
main window — not worth it. CLAP defines a Wayland window API, but its GUIs are hosted
through X11 here too (`crates/clap-ui-host`), as every installed CLAP plugin offers. Revisit
if plugins adopt CLAP's or VST3's Wayland interfaces. (Test runs that take screenshots unset
`WAYLAND_DISPLAY` to force X11, so the windows can be captured with X11 tools.)
