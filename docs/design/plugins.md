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

**Decided: A1 and A2/A3 together, around one core.** Per instrument:

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
4. **Built-in format** (`PluginFormat::Builtin`) and the first instrument core (organ) with its
   built-in front-end and shipped presets.
5. The **standalone front-end** for that core, once its format is chosen.
6. First built-in effects with interactive inline views.

Later, separately: MIDI learn for hardware controls.

## Open questions

- **The standalone format** for instrument cores: LV2 (Linux-native, presets shared with
  Ardour/Carla, but the Rust bindings are little maintained), CLAP (MIT, simple, growing host
  support; nih-plug exports it), VST3 (widest host support; the SDK is MIT now, but check the
  Rust bindings' licence), or more than one through a framework.
- **Presets** for our plugins: since the core owns its state, where do shipped presets live, and
  in which storage do user presets go — the standalone format's (so other hosts see them) or
  the app's own? The preset browser only needs `PresetInfo`s either way.
- How the built-in and standalone builds **recognise each other** (shared id scheme), so
  live-keys lists an instrument once and slot presets work with either.
- Inline displays: also in **instrument** headers (a synth's oscilloscope), or only effects?

## Sources

- Ardour API docs, inline display: [`LV2_Inline_Display_Interface`](https://ardour.org/files/doxygen/structLV2__Inline__Display__Interface.html),
  [`LV2_Inline_Display`](https://ardour.org/files/doxygen/structLV2__Inline__Display.html),
  [`lv2_extensions.h`](https://ardour.org/files/doxygen/lv2__extensions_8h_source.html)
- Ardour commits: [implementing the extension](https://git.ardour.org/ardour/livetrax/commit/c32824e4523a920c9e541cdc40ebaef47e17da29),
  [dropping the Cairo dependency](https://git.ardour.org/ardour/livetrax/commit/93bc9b972888f5a8c08115eccb66a35d4f3853a6)
- Carla's host support: [Carla commit 856bbf60](https://git.kx.studio/falkTX/Carla/commit/856bbf6015720cdccbe379508b0fd80606523d27)
- CLAP extensions: [free-audio/clap](https://github.com/free-audio/clap), [CLAP two years later (LibreArts)](https://librearts.org/2024/11/clap-api-two-years-later/)
- Rust plugin frameworks and licensing: [nih-plug](https://github.com/robbert-vdh/nih-plug),
  [nice-plug](https://github.com/sekwah41/nice-plug), [nih-plug issue #135](https://www.github.com/robbert-vdh/nih-plug/issues/135),
  [clap-wrapper](https://docs.rs/clap-wrapper) (cites the VST3 SDK's MIT licence),
  [VST3 becomes open-source (discuss.pixls.us)](https://discuss.pixls.us/t/vst3-becomes-open-source-asio-goes-gpl-compatible/53917)
