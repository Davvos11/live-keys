# Plugin integration: design notes

Status: **draft** — the direction is decided (see Decisions); the details below are still
options. Started 2026-11-10.

## Decisions

- **Instruments: a reusable core plus a live-keys front-end.** Each instrument we write has its
  sound engine in a crate of its own, with no host dependencies. It is built twice: as a
  standalone plugin that any host can load (Unix philosophy: small, separate, useful on its
  own — there may be a niche for simple instrument plugins), and as a built-in plugin inside
  live-keys with a panel that integrates neatly (Slint, our components). See Part A.
- **Audio effects: built in only.** They don't need to work in other hosts, so they're written
  for live-keys alone, with native, interactive inline views. See Part C.
- **"Which controls drive a plugin" means on-screen controls** — the slot's knobs, drawbars,
  buttons — set up per plugin. Hardware control (MIDI learn) comes in a later stage, with its
  own design.
- **No macros**: one on-screen control drives one parameter (or one MIDI CC).
- **Presets of our instruments are visible to other hosts**: they're stored in the standalone
  format's own preset mechanism, not only in live-keys.
- **No inline displays in instrument headers** for now; inline views are for effects.
- **Our own effects open a small in-app popup** from their FX square (see Part C).

## Goals

1. **Any plugin keeps working.** Every LV2/VST3 instrument and effect can be loaded, as now.
2. **A few plugins are integrated out of the box**: they come with good presets, a custom inline
   UI in the slot (like the drawbars in the Penpot design) or at least well-chosen parameters on
   the slot's knobs.
3. **Integration comes from plugins we write ourselves**, small and focused, rather than
   wrapping existing plugins and shoehorning them into our UI.
4. **The user can still set up their own plugins**: at the least, choose which controls drive
   which parameters.
5. **Effects get inline views** in their FX squares, like the ACE and LSP plugins show in
   Ardour's mixer strip (a compressor's curve and gain reduction, an EQ's response).

## Where we are now

- A slot's first tab shows the instrument's **first five continuous parameters** as knobs
  (`slot_view::knob_rows`). That's a placeholder: the first five are rarely the useful five
  (OT P1ANO S and setBfree show none at all).
- An effect's FX square shows its **name** over a curve guessed from that name
  (`slot_view::graph`: "compressor" → a transfer curve, "filter"/"EQ" → a resonance) — also a
  placeholder.
- **Presets** are each format's own (LV2 `pset:Preset`s, `.vstpreset` files) plus slot presets.
- The Penpot design already has a `Drawbar` component; the app doesn't use it yet.

## Part A — integrated instruments

Two separate questions: *what* makes a plugin feel integrated, and *how we package* our own.

### What "integrated" means

A plugin is integrated when the host knows, per plugin:

- **which parameters matter** and how to show them: a knob, a switch, a drawbar, a choice;
  grouped, with short labels ("Drive", not "Amp Drive Amount (dB)");
- **an inline panel layout** for the slot's first tab (five knobs, nine drawbars, a row of
  switches…), drawn by *our* UI from *our* components, so it looks like the rest of the app;
- **a curated preset list** shipped with it.

Call that description a **panel profile**. The key observation: a profile works the same for
a plugin we wrote and for a third-party plugin. So the integration mechanism doesn't depend on
how our own plugins are packaged — it's a host-side description, keyed by plugin id.

A profile is roughly:

```jsonc
{
  "plugin": "urn:live-keys:organ",          // PluginInfo::id it applies to
  "panel": "drawbars",                        // or "knobs", "switches", …
  "controls": [
    { "param": "drawbar-16", "label": "16'", "widget": "drawbar", "colour": "brown" },
    // …
  ],
  "presets": "presets/organ/"                 // shipped presets, in the format's own storage
}
```

Profiles we ship are compiled in. Profiles the user makes (Part B) go to
`<data dir>/live-keys/profiles/`, and a user profile overrides a shipped one.

### How to package our own instruments

| | A1 — built in (Rust, inside the app) | A2 — our own LV2 plugins | A3 — our own CLAP/VST3 (e.g. nih-plug) |
|---|---|---|---|
| How it's hosted | A third `PluginFormat`, implementing `HostedPlugin` directly | Like any LV2, through `lilv` | Through the VST3 host (CLAP would be a new format) |
| Inline UI | Slint, from the profile; no FFI | Slint, from the profile | Slint, from the profile |
| Usable in other hosts (Ardour, Carla) | No (unless also exported, see below) | Yes | Yes |
| Shipping | Inside the binary/.deb; nothing to install | Separate bundles to install to `LV2_PATH` | Separate bundles |
| Effort per plugin | Lowest: plain Rust, unit-testable, no plugin ABI | Medium: TTL metadata, the LV2 C API from Rust (`lv2` crate, little maintained) | Medium: framework does the ABI |
| Presets | App-level or LV2-style, our choice | LV2 presets (shared with Ardour) | `.vstpreset` / CLAP state |
| Licence notes | Ours | Ours | VST3 SDK is MIT now; check the framework's own bindings (nih-plug has used GPL/LGPL VST3 bindings) |

**Decided: A1 and A2/A3 together, around one core.** Per instrument (the toolkit for the
standalone front-end is under investigation, see below):

```
crates/instruments/<name>/        the sound engine: parameters, voices, render(); no host code
  ├─ built-in front-end (A1)      in live-keys: a `HostedPlugin` of a new `PluginFormat::Builtin`,
  │                               plus its panel profile and, where a profile isn't enough,
  │                               a custom Slint panel (the drawbars)
  └─ standalone front-end (A2/A3) a plugin bundle for other hosts, with a plain generic or
                                  minimal GUI; format still open (see open questions)
```

- The core's parameter list is the single source for both front-ends, so ids, ranges and
  defaults match, and **the core owns its state format**: a preset saved in live-keys can be
  loaded by the standalone plugin and the other way round.
- Inside live-keys, the built-in front-end is always the one used. If the standalone build is
  also installed, the picker shows the instrument once (the built-in one) — the two need a way
  to recognise each other (a shared id scheme).
- Effort stays close to plain A1: the plugin ABI lives in one thin, shared front-end layer, not
  in each instrument.

### Toolkit for the standalone instruments (investigation, 2026-11-11)

nih-plug, the usual answer a few years ago, is **in maintenance mode**; its README points to a
community fork, now **nice-plug**. Candidates found:

| | truce | nice-plug | RAVET |
|---|---|---|---|
| Formats | CLAP, VST3 (default); LV2, VST2, AU v2/v3, AAX (opt-in); standalone | CLAP, VST3; standalone | CLAP, VST3, LV2, AU, AAX |
| Presets visible to hosts | **Yes**: one TOML `.preset` per preset becomes CLAP preset-discovery, `.vstpreset`, LV2 `pset:Preset` and AU factory presets | Not mentioned (only persisted state) | Not stated |
| GUI | Built-in widgets, egui, iced, **Slint**, vizia, raw window handle | egui, iced, Slint (work in progress), anything baseview-compatible; can build without any GUI | HTML in the platform's web view |
| MIDI | MIDI 2.0/UMP (opt-in), per-note expression across formats, sample-accurate automation | Note expression, CCs, pressure, pitch bend, SysEx, CLAP poly modulation | MIDI 1 short messages |
| Licence | "Truce License 1.0": Apache-2.0/MIT plus a rider that only applies to redistributing truce itself as a commercial framework or service | ISC | GPL-3.0 (or paid commercial) |
| Maturity | v6.3; ~1,870 commits, ~250 stars; examples (gain, EQ, synth, arpeggio…) | v0.4.2 (2026-09); ~2,550 commits (incl. nih-plug's), one main maintainer; calls itself experimental | v0.1.0 (2026-11), prototype adapters |

**Leaning: truce.** It's the only one that covers both parts of the brief: LV2 (the native
Linux format, which Ardour and Carla show presets for) alongside CLAP/VST3, and **presets
exported to every format from one source**, which is exactly "other hosts see the presets".
Its Slint support also means a standalone GUI could reuse our components. nice-plug is the
fallback (lineage, ISC licence, but no LV2 and presets would be ours to build per format);
RAVET is too young and GPL. Outside Rust, DPF (C++, LV2/VST3/CLAP, ISC) would be the
established choice, at the cost of a second language.

Before committing to truce, a **spike**: build a minimal sine instrument with two presets as
LV2 + CLAP + VST3, and check

- that the presets show up in Ardour and Carla (LV2), and in live-keys itself (LV2 and VST3);
- the licences of its per-format binding crates (`truce-lv2`, `truce-vst3`, `truce-clap`) and
  how `cargo deny` sees the Truce License (a new licence is the developer's decision, see
  CLAUDE.md's Licensing);
- whether the instrument core can stay free of truce types, so the built-in front-end doesn't
  depend on it (truce wraps the core; the core doesn't know truce);
- its state format: whether live-keys' built-in front-end can read a preset or state saved by
  the standalone plugin, and the other way round (needed for the next point).

### Recognising each other (investigation)

The built-in and standalone builds of one instrument have to be treated as the same instrument:

- **Ids.** The core declares its standalone ids (LV2 URI, CLAP id, VST3 class id); the built-in
  front-end's `PluginInfo::id` is derived from the same name. The picker then hides a scanned
  standalone plugin whose id a built-in claims.
- **Slot presets and sessions** that recorded the standalone id load the built-in instead (and
  a slot preset made in live-keys stays loadable if the built-in is removed one day?).
- **Presets.** The built-in front-end lists the standalone format's presets (the LV2 or
  `.vstpreset` files, wherever truce installs them) and saves new ones there, so both builds
  share one preset list. Depends on the state-format check above.

Candidates for a first integrated instrument: a **drawbar organ** (the design's drawbars; nine
sine partials plus percussion and a simple rotary is small enough to own), an **electric
piano**, a **pad**.

## Part B — setting up third-party plugins

- **Choosing the panel's controls.** A "Customise panel" mode on the slot's first tab: pick
  which parameters get a control (searchable, like the preset browser), which widget each is
  (knob, drawbar, switch/button), relabel and reorder them. Saved as a user panel profile for
  that plugin id, so every slot with that plugin gets it. Until a profile exists, keep the
  current fallback (first five continuous parameters as knobs).
- **On-screen only, for now.** Hardware controls (MIDI learn from e.g. the LPD8) are a later
  stage with their own design; profiles shouldn't preclude it (a control has a stable id that a
  binding could point at later).
- **Shipped profiles for popular free plugins** are cheap wins and need no plugin of our own:
  e.g. **setBfree**, whose drawbars are only reachable by MIDI CC (it has no control ports) —
  a drawbar panel that sends those CCs would give it the design's drawbars today.
  This needs the profile to address a **MIDI CC** as well as a parameter.

## Part C — inline views for effects

### The existing "standard": Ardour's LV2 inline display

- An LV2 extension by Harrison/Ardour, URI `http://harrisonconsoles.com/lv2/inlinedisplay`. It's
  **not part of the official LV2 spec** (lv2plug.in); it's a de-facto extension that Ardour/
  Mixbus and Carla host.
- **Plugin side:** extension data `…#interface` with one function,
  `render(instance, max_width, max_height) -> *LV2_Inline_Display_Image_Surface`
  (`data`, `width`, `height`, `stride`; ARGB32), the pixels owned by the plugin and valid from the
  first render until cleanup. Called by the host "in a non-realtime context, usually the main
  GUI thread". The plugin picks its own height up to the maximum.
- **Host side:** feature `…#queue_draw`, a `{ handle, queue_draw(handle) }` struct the plugin
  calls "from run()" to ask for a new render. (`…#in_gui` also exists, undocumented.)
- **It's display-only.** Ardour doesn't send input to it; interacting means opening the plugin's
  own GUI.
- **Who implements it:** ACE (Ardour's bundled plugins), x42 (installed here: `darc`
  compressor, `dpl` limiter, `fil4` EQ, `meters`, `tuna`), and LSP (`lsp-plugins.lv2`).
- **Other formats:** VST3 has no equivalent. CLAP has nothing like it either, as far as I could
  find (its drafts cover parameter indication and remote-control pages, not drawing).

### Hosting it in live-keys

Feasible within the current architecture:

1. `Lv2Host` offers the `queue_draw` feature; its callback only sets a per-plugin `AtomicBool`
   — nothing else, since it's called from `run()` on the audio thread.
2. At instantiation, ask the plugin's `extension_data` for the interface (like
   `InstanceRef::has_state`); keep the function pointer in `PluginEditor::Lv2`.
3. The GUI's poll timer, for plugins whose flag is set and whose square is visible, calls
   `render(handle, 160, 160)` (80px squares at 2× scale) on the main thread, holding a
   `GuiLease` like an open plugin GUI (the instance must outlive the call), and turns the surface
   into a `slint::Image` (`Image::from_rgba8_premultiplied`, after reordering ARGB32's bytes).
   Throttled to ~30 fps.
4. `FxSlot` shows that image instead of the name-based curve; the fallback stays for plugins
   without it (every VST3, most LV2).

To verify first: whether plugins expect `render` to run **concurrently with `run()`** (Ardour
calls it from its GUI thread while processing continues, so implementing plugins presumably
synchronise internally — check Ardour's host code and x42/LSP sources), and whether the pixels
are **premultiplied, native-endian Cairo ARGB32** (the extension started as a Cairo surface,
which suggests so; Ardour's docs only say "ARGB32").

### Our own effects

The same split as for instruments:

- **Built in (A1-style):** we draw the inline view natively in Slint from the plugin's live
  values (gain reduction, EQ bands), and it can be **interactive** — drag an EQ band, the
  compressor's threshold — which the Ardour extension can't do. No extension needed.

### The effect popup (decided direction, from a sketch)

Clicking a built-in effect's FX square opens a small popup over the slot (replacing today's
generic effect popover for these effects):

```
┌ Compressor Stereo ───────────────────────────┐
│ ┌──────────────────┐ ▐   (Attack) (Release)  │  a large, interactive graph (transfer curve
│ │      ╱────────   │ ▐                       │  with the threshold marker); an output/gain
│ │    ╱             │ ▐   (Ratio)  (Makeup)   │  reduction meter beside it, an input meter
│ │  ╱               │ ▐                       │  below; the effect's main knobs (2×2) on
│ └──────────────────┘                         │  the right
│ ▬▬▬▬▬▬▬▬▬▬▬▬▬▬▬ in                           │
│ [GUI] [Params]                 [‹] [›] [🗑]  │  the same footer as now: own GUI (if any),
└──────────────────────────────────────────────┘  parameter window, move, remove
```

- The **FX square** of a built-in effect shows only its live graph — no name text (the name is
  in the popup's title).
- Third-party effects keep the generic popover; their square shows the Ardour inline display if
  they have one, else the name (and, until profiles exist, the guessed curve).
- Built-in effects may not need a "GUI" button at all; the popup *is* their GUI.
- **Our own LV2 plugins (A2-style):** implementing the Ardour extension makes them look good in
  Ardour too, but in live-keys they'd be limited to a picture, unless we also add our own
  interactive layer.

**Decided:** our own effects are **built in only** (they needn't work in other hosts), with
native, interactive Slint views — so they never go through the extension. **Leaning:** still
host the Ardour extension for third-party LV2 effects (cheap, and LSP/x42 look great
immediately); a plain fallback (name, or a guessed curve) for everything else.

## Possible order of work

1. **Panel profiles** in the host (data model, shipped + user profiles, the slot tab rendering a
   profile with knobs/drawbars/switches). Replaces the "first five parameters" placeholder.
2. **Customise panel** UI for third-party plugins, plus a few shipped profiles (setBfree drawbars
   via CC).
3. **LV2 inline display** hosting in the FX squares.
4. **The truce spike** (can run in parallel with 1–3).
5. **Built-in format** (`PluginFormat::Builtin`) and the first instrument core (organ) with its
   built-in front-end, its standalone build and shipped presets.
6. First built-in effect (a compressor) with the effect popup and its interactive graph.

Later, separately: MIDI learn for hardware controls.

## Open questions

- **Toolkit**: confirm truce with the spike above (or fall back to nice-plug).
- **Recognising each other**: the id scheme and shared presets above, once the spike shows
  truce's state and preset formats.
- Which **formats** to build the standalone instruments as by default: LV2 + CLAP (+ VST3)?

## Sources

- Ardour API docs, inline display: [`LV2_Inline_Display_Interface`](https://ardour.org/files/doxygen/structLV2__Inline__Display__Interface.html),
  [`LV2_Inline_Display`](https://ardour.org/files/doxygen/structLV2__Inline__Display.html),
  [`lv2_extensions.h`](https://ardour.org/files/doxygen/lv2__extensions_8h_source.html)
- Ardour commits: [implementing the extension](https://git.ardour.org/ardour/livetrax/commit/c32824e4523a920c9e541cdc40ebaef47e17da29),
  [dropping the Cairo dependency](https://git.ardour.org/ardour/livetrax/commit/93bc9b972888f5a8c08115eccb66a35d4f3853a6)
- Carla's host support: [Carla commit 856bbf60](https://git.kx.studio/falkTX/Carla/commit/856bbf6015720cdccbe379508b0fd80606523d27)
- CLAP extensions: [free-audio/clap](https://github.com/free-audio/clap), [CLAP two years later (LibreArts)](https://librearts.org/2024/11/clap-api-two-years-later/)
- Toolkits: [truce](https://github.com/truce-audio/truce), [nice-plug](https://codeberg.org/RustAudio/nice-plug),
  [nih-plug's hand-over issue](https://github.com/robbert-vdh/nih-plug/issues/265),
  [RAVET 0.1.0](https://users.rust-lang.org/t/ravet-0-1-0-audio-plug-ins-and-standalone-apps-in-rust/143550),
  [DPF licensing](https://git.kx.studio/DISTRHO/DPF/src/commit/4100e47d4c13dc7e5fc6b087be442ac3c1237896/LICENSING.md),
  [CLAP preset discovery discussion](https://github.com/free-audio/clap/discussions/308)
- Rust plugin frameworks and licensing: [nih-plug](https://github.com/robbert-vdh/nih-plug),
  [nice-plug](https://github.com/sekwah41/nice-plug), [nih-plug issue #135](https://www.github.com/robbert-vdh/nih-plug/issues/135),
  [clap-wrapper](https://docs.rs/clap-wrapper) (cites the VST3 SDK's MIT licence),
  [VST3 becomes open-source (discuss.pixls.us)](https://discuss.pixls.us/t/vst3-becomes-open-source-asio-goes-gpl-compatible/53917)
