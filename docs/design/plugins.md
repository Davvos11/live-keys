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
  own design: `midi-learn.md`.
- **No macros**: one on-screen control drives one parameter (or one MIDI CC).
- **Presets of our instruments are visible to other hosts**: they're stored in the standalone
  format's own preset mechanism, not only in live-keys.
- **No inline displays in instrument headers** for now; inline views are for effects.
- **Our own effects open a small in-app popup** from their FX square (see Part C).
- **Third-party plugins first**: panel profiles for existing plugins come before any plugin of
  our own. Profiles are **for instruments only** at first.
- **Third-party effects' FX squares** (2026-11-15): the square shows the plugin's Ardour inline
  display if it has one (filling the square, its name only on hover), else its name over a
  guessed curve. Clicking opens the plugin's own GUI, or its parameter window without one;
  clicking while either is open closes it. Right-clicking gives a menu (GUI, parameters,
  presets, move, remove) instead of the old popover, and dragging a square reorders its chain
  (the gaps between squares scroll the row). LV2 only: nothing comparable is implemented by
  any installed VST3 or CLAP plugin (see Part C).
- **setBfree** is driven by its own control messages (two-way), with the upper manual's
  drawbars only. The generic `cc` target comes later, with the customise UI.
- **Up to ten controls** on the slot's first tab, laid out zigzag over two rows; under an
  optional fixed section (drawbars), one row of at most five. The user picks which ones
  (a checklist) from the profile's pool, or from all parameters, through a context menu, and
  the choice is kept per plugin. The audio effects scroll sideways in one row. The first tab
  never scrolls vertically; the window's minimum is 1500×995 with the right sidebar (the slots'
  part is 1200×995; 2026-11-12).
- **Cross-platform is a future goal** for the app. For our own instruments: **CLAP is the
  reference format**, and **VST3 and LV2 are built too** (see "Formats for the standalone
  instruments").
- **Our first instrument is a sampled grand piano** built on the Salamander Grand Piano samples
  (CC-BY 3.0), in our own code (2026-11-16). It rebuilds the concept of OT P1ANO S (which is
  Salamander too) rather than porting it: OT P1ANO S is GPL-3. Ideas for this and later
  instruments are in `own-plugins.md`.
- **Third-party plugins are recommended, not bundled** (2026-11-16): the `.deb` lists the ones
  with profiles as `Recommends`, and an install window in the app installs whatever is missing,
  including plugins only available as a `.deb` of their own, and our instruments' samples. See
  Part D.

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
- An effect's FX square shows its LV2 **inline display** if it has one, else its **name** over a
  curve guessed from that name (`slot_view::graph`: "compressor" → a transfer curve,
  "filter"/"EQ" → a resonance) — a placeholder.
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
  "plugins": ["urn:live-keys:organ"],      // the PluginInfo::ids it applies to
  "panel": "drawbars",                        // or "knobs", "switches", …
  "controls": [
    { "param": "drawbar-16", "label": "16'", "widget": "drawbar", "colour": "brown" },
    // …
  ],
  "presets": "presets/organ/"                 // shipped presets, in the format's own storage
}
```

Profiles we ship are compiled in. Profiles the user makes (Part B) go to
`<data dir>/live-keys/profiles/`, and a user profile overrides a shipped one. The details are
in Part B, "Panel profiles", which comes first in the order of work.

### How to package our own instruments

| | A1 — built in (Rust, inside the app) | A2 — our own LV2 plugins | A3 — our own CLAP/VST3 (e.g. nih-plug) |
|---|---|---|---|
| How it's hosted | A new `PluginFormat`, implementing `HostedPlugin` directly | Like any LV2, through `lilv` | Through the CLAP or VST3 host |
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

### Formats for the standalone instruments

The app should be able to go cross-platform one day, which favours writing our instruments for
formats that exist everywhere. **CLAP is the reference format; VST3 and LV2 are built as well**
— with truce all three come from one source, so the extra formats cost little.

- **CLAP** (open, MIT, plain C headers, the cleanest design: polyphonic modulation, note
  expressions, a clear threading model). Supported by Bitwig and REAPER; FL Studio and Studio One
  reportedly (sources disagree); Cubase has announced it without a date; **not** Ableton Live,
  Logic or Pro Tools. Hosts only see its presets through the preset-discovery extension, which
  few support — so CLAP alone doesn't meet "other hosts see the presets".
- **VST3** for reach: Ableton Live (Windows/macOS), Cubase, FL Studio and most others; hosts
  show `.vstpreset` files. The SDK is MIT now.
- **LV2** for Linux hosts: Ardour and Carla, which show LV2 presets. (LV2 itself isn't
  Linux-only — the spec and lilv build on Windows and macOS, and Ardour hosts it there — but
  few LV2 plugins exist for those platforms.)

The truce spike should check that each format shows the presets in at least one real host
(CLAP: REAPER or Bitwig; VST3: REAPER or Carla; LV2: Ardour or Carla).

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

**The first integrated instrument is a Salamander-based piano** (decided 2026-11-16; the
organ is covered by setBfree's profile for now). Later ones: a modelled electric piano, a pad,
sampled instruments from freely licensed SFZ/SF2 sets. See `own-plugins.md`.

## Part B — setting up third-party plugins

Third-party plugins come first: profiles for the plugins people already use, before any
plugin of our own (2026-11-12).

### Panel profiles (draft, 2026-11-12)

A profile describes the slot's first tab for one plugin: which controls appear, how they're
laid out, and what each one drives. Shipped and user profiles have the same format.

**Targets: what a control drives.**

- **`param`**: a plugin parameter. A profile needs an id that is stable across plugin versions
  and sessions. VST3 `ParamID`s and CLAP parameter ids are. An LV2 `ParamInfo::id` is a port
  index, which can change when a plugin adds a port, so LV2 targets are **port symbols**.
  `ParamInfo` gets a `symbol: String` (the LV2 port symbol, or the VST3/CLAP id in decimal),
  and profiles match on it.
- **`cc`**: a MIDI CC on a channel. It's for plugins that only listen to fixed CCs. It's
  **one-way**: the host can't see the value change elsewhere (the plugin's own GUI, its
  programs, a hardware controller), so it shows the last value it sent. The CC goes straight to
  the instrument's MIDI input, after the slot's filter and MIDI effects, so a zone or an
  arpeggiator can't swallow it. That needs a new rack command; `Engine::send_midi` goes through
  every slot's router.
- **`setbfree`**: setBfree's own control messages. Its GUI and DSP exchange every setting as
  `sb3:controlmsg` objects (`sb3:controlkey`, a function name like `upper.drawbar16`;
  `sb3:controlval`, 0–127) on the `control` input and the `notify` output, and the DSP reports
  every change there, including program changes and state loads. So unlike `cc` this is
  **two-way**: the panel shows the real drawbar positions, through plumbing that already
  exists (`send_ui_atom`, `drain_ui_events`). It's plugin-specific code, but setBfree is the one
  CC-only plugin that matters, and no MIDI channel needs configuring. **Decided:** use this for
  setBfree, upper manual only (one row of drawbars, as in the design), and add `cc` with the
  customise UI (step 2), for other CC-only plugins.

**setBfree's protocol** (verified 2026-11-12 against v0.8.13, headless, through the engine; and
its source):

- **A control message** is an `atom:Object` with otype `sb3:controlmsg` (`sb3:` =
  `http://gareus.org/oss/lv2/b_synth#`). Its properties are `sb3:controlkey`, an `atom:String`
  function name, and `sb3:controlval`, an `atom:Int` from 0 to 127. It's the same shape both
  ways: written to `control` (port 0), reported on `notify` (port 1).
- **Reporting starts with `sb3:uiinit`.** setBfree reports nothing until an empty
  `sb3:uiinit` object arrives. That message then dumps every function that has been set at
  least once (35 on a fresh instance), followed by all 128 programs (`sb3:midipgm`). So the
  host sends `uiinit` when the plugin loads. Functions that never got a value aren't in the
  dump, so the profile needs defaults for those.
- **What gets reported, and what doesn't:**
  - Values set by the host aren't echoed back, and neither are values derived from them
    (`vibrato.upper` ↔ `vibrato.routing`), so the panel updates its own model.
  - Keyboard CCs and program changes are reported (only the values the program sets).
  - A state restore (slot preset, LV2 preset) reports `special.reinit` followed by a full dump,
    without being asked. Read `special.reinit` as "reset to defaults, the values follow".
- **Drawbars are inverted.** 0 means fully out (8) and 127 fully in (0):
  `level = rint((127 − v) · 8 / 127)`. The host sends `rint((8 − level) · 127 / 8)`.
- **Values of the panel's controls:**

  | Function | Values |
  |---|---|
  | `upper.drawbar{16,513,8,4,223,2,135,113,1}` | inverted, as above |
  | `percussion.enable` / `.volume` / `.decay` / `.harmonic` | ≥ 64: on / soft / fast / 2nd (else off / normal / slow / 3rd) |
  | `vibrato.knob` | V1 C1 V2 C2 V3 C3 = 0, 23, 46, 69, 92, 115 (`v / 23`); no "off" |
  | `vibrato.upper` | ≥ 64 on (vibrato on the upper manual) |
  | `rotary.speed-preset` | slow / stop / fast = 0 / 64 / 127 (`v / 43`) |
  | `overdrive.enable`, `overdrive.character` | ≥ 64 on; 0–127 |
  | `swellpedal1` | volume, 0–127 (default 127) |
  | `reverb.mix` | 0–127 (default 12) |

- **Write only through `controlmsg`.** `sb3:cfgstr`/`loadcfg` rebuild the whole synth on the
  worker. `overdrive.inputgain`/`outputgain` print to stdout on every change.

**Widgets.**

| Widget | For | Notes |
|---|---|---|
| `knob` | continuous or stepped | the current `LabelledKnob`; ~56 px wide |
| `drawbar` | 9 positions (0–8) | the design's `Drawbar`, colour `brown`/`white`/`black`; ~30 px wide |
| `switch` | toggle | the design's `Toggle`: a 48×44 button with a short word on it ("Soft", "Fast") and an accent bar along its top when on, plus a caption below ("Volume", "Decay"). Same 56 px cell and caption line as a `LabelledKnob`, so the two mix in one row |
| `lever` | a choice of 3 (or 2) options | the design's `Lever`: a track with notches and a thumb, the current option's word below it ("Fast"), the caption under the button. Click a side (or the centre), drag left/right **or** up/down (right/up is towards the last option), or scroll, one step per notch. Modelled on the B3's half-moon Leslie switch |

A choice of 2–3 options can be a `lever`; any other choice or stepped parameter is a
**stepped knob**: a `knob` that snaps to its steps and
shows the current option's label (from the plugin, or the control's `choices`), like a B3's
vibrato selector. A `Select` or a segmented row of buttons doesn't fit in a 56 px cell.

A control can override what the plugin reports: `min`/`max` (a cutoff knob that only covers
the useful range), `steps` and `choices` (JUCE plugins report most choice parameters as
continuous; see CLAUDE.md), `invert`, and `default` (the value shown while setBfree hasn't
reported a function). `choices` are `{ "label", "value" }` objects in the target's terms, or bare
labels spread evenly across the target's range.

**Layout: up to ten controls, chosen as a list** (revised 2026-11-13 from the user's sketches;
first decided 2026-11-12 as one row of five). The slot's first tab has, from top to bottom,
spaced evenly, never scrolling vertically:

1. **An optional fixed section** that the profile defines and the user doesn't rearrange:
   setBfree's nine drawbars. Most profiles have none.
2. **The controls**: up to **ten**, or **five** under drawbars. Each is a 56 px cell (a knob,
   switch, lever or stepped knob, with a caption line), so the kinds mix. Under drawbars, or
   with three or fewer, they're one row spaced evenly; otherwise **zigzag** over two rows —
   the first half on top, the rest lower and between them (changed 2026-11-23 from the 1st,
   3rd, 5th… on top, so the panel reads in order, top row then bottom, as a keyboard's knobs
   do) — so ten fit the slot's width. The user never arranges rows: it's one ordered list.
3. **The audio effects** in one row of 80 px squares, three fitting the slot, **scrolling
   sideways** when there are more: drag, the mouse wheel, or a thin scrollbar under the row
   (its thumb draggable) that shows when they don't fit.

The profile offers a **pool** of curated controls (setBfree: percussion ×4, vibrato on and
mode, Leslie, drive on and amount, reverb, swell) and a **default** list. A control's context
menu (right-click), or the **+** shown after the last one while there's room, lists:

- the pool as a **checklist**: the shown controls are ticked. Choosing a ticked one takes it
  off (the rest move up). Choosing another adds it at the end, or, when the panel is full,
  puts it in place of the control whose menu it is;
- **More parameters…**, a search over all of the plugin's parameters (adds the same way);
- **Reset row**, which goes back to the profile's default.

The user's choice is kept **per plugin** (every slot with setBfree shows the same), next to the
user profiles (see Storage), so a slot preset or a scene doesn't change it. The pool is ordered
as the menu lists it.

A plugin without a profile gets a pool of all its parameters, with its first ten continuous
parameters as the default. So the context menu is also the first version of Part B's
"Customise panel".

**A control** has

- `id`: stable within the profile; the saved row and a later MIDI-learn binding point at it;
- `target`: as above;
- `widget` and its options;
- `label`: the short caption under the control ("Drive", "16'");
- `name`: what the context menu lists ("Perc volume"), the label if absent;
- for a switch, `text`: the word on the button ("Soft"), lit when on.

```jsonc
{
  "plugins": ["http://gareus.org/oss/lv2/b_synth"],
  "fixed": "drawbars",                       // the fixed section's layout, if any
  "drawbars": [
    { "id": "db16", "label": "16'", "colour": "brown", "target": { "setbfree": "upper.drawbar16" } },
    // … 5⅓' brown, 8' white, 4' white, 2⅔' black, 2' white, 1⅗' black, 1⅓' black, 1' white
  ],
  "controls": [                              // the pool, in menu order
    { "id": "perc", "label": "Perc", "text": "On", "widget": "switch", "target": { "setbfree": "percussion.enable" } },
    { "id": "perc-vol", "name": "Perc volume", "label": "Volume", "text": "Soft", "widget": "switch",
      "target": { "setbfree": "percussion.volume" } },
    { "id": "perc-decay", "name": "Perc decay", "label": "Decay", "text": "Fast", "widget": "switch",
      "target": { "setbfree": "percussion.decay" } },
    { "id": "perc-harm", "name": "Perc harmonic", "label": "Harmonic", "text": "2nd", "widget": "switch",
      "target": { "setbfree": "percussion.harmonic" } },
    { "id": "vibrato", "label": "Vibrato", "text": "On", "widget": "switch", "target": { "setbfree": "vibrato.upper" } },
    { "id": "vib-mode", "name": "Vibrato mode", "label": "Mode", "widget": "knob", "target": { "setbfree": "vibrato.knob" },
      "choices": ["V1", "C1", "V2", "C2", "V3", "C3"] },   // a stepped knob, like a B3's selector
    { "id": "leslie", "label": "Leslie", "widget": "lever", "target": { "setbfree": "rotary.speed-preset" },
      // Fast on the left, as setBfree's own lever (and a B3's half-moon) has it
      "choices": [{ "label": "Fast", "value": 127 }, { "label": "Stop", "value": 64 }, { "label": "Slow", "value": 0 }] },
    { "id": "drive", "label": "Drive", "text": "On", "widget": "switch", "target": { "setbfree": "overdrive.enable" } },
    { "id": "drive-amt", "name": "Drive amount", "label": "Amount", "widget": "knob", "target": { "setbfree": "overdrive.character" } },
    { "id": "reverb", "label": "Reverb", "widget": "knob", "target": { "setbfree": "reverb.mix" } },
    { "id": "swell", "name": "Swell (volume)", "label": "Swell", "widget": "knob", "target": { "setbfree": "swellpedal1" } }
  ],
  "row": ["perc", "vibrato", "leslie", "drive", "reverb"]
}
```

A Surge XT profile has no
fixed section, a pool of M1–M8 and Global Volume (`{ "param": "<id>" }` targets), and M1–M5 as
its row.

**Values.** Parameter values are already recorded by the engine (`Engine::param_value`). `cc`
and `setbfree` values are recorded per plugin in the same way. `cc` values also go into the
session and slot presets (`slot.json`), so the panel shows them again after a reload. The
plugin's state restores the sound either way. setBfree's values come back from the plugin
itself.

**Storage.** Shipped profiles are JSON files in the repo, compiled in (`include_str!`). User
profiles are `<data dir>/live-keys/profiles/*.json`, one per plugin, with the plugin id inside
the file (ids contain `/` and `:`, so they don't make good file names). A user profile replaces
the shipped one wholesale, with no merging. The user's rows aren't profiles: they're one file,
`<data dir>/live-keys/profiles/rows.json` (left out when reading profiles), mapping a plugin id
to its row's control ids and the parameters added through "More parameters…" (which stay in
the pool until the row is reset). So a row choice doesn't freeze a shipped profile's pool,
and "Reset row" removes the plugin's entry. Profiles and rows are per plugin, so they're not
part of slot presets or scenes.

**No profile:** the current fallback (the first five continuous parameters as knobs).

**Shipped profiles to start with** (investigated 2026-11-12 with `plugin-check --info`; the
raw lists are not kept):

- **setBfree**: as above.
The lists below were proposed as rows before the one-row decision. Read the first group as
the default row and the rest as the pool.

- **mda EPiano** (LV2, 12 ports, all 0–1). Knobs:
  - default row: Decay `env_decay`, Release `env_release`, Hardness `hardness`, Treble
    `treble_boost`, Drive `overdrive`;
  - also in the pool: Trem/Pan `modulation`, Rate `lfo_rate`, Velocity `vel_sense`, Width
    `stereo_width`, Detune `random_tuning`.

  Hardness, Treble and Trem/Pan are centred (0.5 = neutral), which argues for a bipolar knob
  style.
- **Helm** (LV2 only, 139 ports; symbol = name). The JUCE LV2 wrapper reports everything as
  continuous 0–1, so `*_on` switches need `steps: 1` and type parameters need `steps` +
  `choices`. Pool:
  - default row (filter): `cutoff`, `resonance`, `fil_env_depth`, `filter_drive`, `keytrack`;
  - amp: `amp_attack`/`decay`/`sustain`/`release`, `velocity_track`;
  - FX: `delay_on`, `reverb_on`, `reverb_dry_wet`, `portamento`, `volume`.
- **Surge XT** (target **CLAP**: preset discovery, and values read back after loads). Its LV2
  build exposes no control ports at all (its parameters are `lv2:Parameter`s over atoms), and
  VST3 ids are the CLAP ids `& 0x7FFFFFFF`. Profile: the 8 macros (M1–M8) and Global Volume,
  since many patches use scene B, where fixed scene-A knobs do nothing. Surge puts a patch's
  macro names in the parameter names ("M1: Brightness") and asks for a rescan on patch load.
  We ignore that rescan today; supporting it would give the knobs live labels.
- **Vital** (CLAP; ids identical in VST3): Macro 1–4 (the macro knobs of its own UI, which
  factory presets assign) and Volume, then Filter 1
  Cutoff/Resonance/Drive and Envelope 1 Attack/Release. Its CLAP build loses the stepped
  flags that its VST3 build reports.
- **OT P1ANO S**: **no profile possible.** It exposes only Bypass. Its 159 controls live in
  HISE's script and aren't host parameters, which is why it shows no knobs today. Its slot
  should show something useful (its GUI, presets) instead of an empty knob row.

Everything these JUCE plugins report is normalized 0–1, so range overrides are in normalized
units and knob text comes from the plugin's own value text (`param_text`).

- **Choosing the panel's controls.** A "Customise panel" mode on the slot's first tab: pick
  which parameters get a control (searchable, like the preset browser), which widget each is
  (knob, drawbar, switch/button), relabel and reorder them. Saved as a user panel profile for
  that plugin id, so every slot with that plugin gets it. Until a profile exists, keep the
  current fallback (first five continuous parameters as knobs).
- **On-screen only, for now.** Hardware controls (MIDI learn from e.g. the LPD8) are a later
  stage with their own design; profiles shouldn't preclude it (a control has a stable id that a
  binding could point at later).
- **Shipped profiles for popular free plugins** are cheap wins and need no plugin of our own:
  e.g. **setBfree**, which has no control ports: its drawbars are reachable by MIDI CC or by
  its own control messages (see "Panel profiles": the `setbfree` target), so a drawbar panel
  gives it the design's drawbars today.

## Part C — inline views for effects

### The existing "standard": Ardour's LV2 inline display

- An LV2 extension by Harrison/Ardour, URI `http://harrisonconsoles.com/lv2/inlinedisplay`. It's
  **not part of the official LV2 spec** (lv2plug.in); it's a de-facto extension that Ardour/
  Mixbus and Carla host.
- **Plugin side:** extension data `…#interface` with one function,
  `render(instance, max_width, max_height) -> *LV2_Inline_Display_Image_Surface`
  (`data`, `width`, `height`, `stride`; ARGB32), the pixels owned by the plugin and valid from the
  first render until cleanup. Called by the host "in a non-realtime context, usually the main
  GUI thread". The plugin picks its own size within the maximum — usually the full width and a
  height from its own aspect ratio: x42-eq 16:9, tuna 3:1, x42-comp and dpl a thin bar (w/10),
  LSP's compressor square.
- **Host side:** feature `…#queue_draw`, a `{ handle, queue_draw(handle) }` struct the plugin
  calls "from run()" to ask for a new render. (`…#in_gui` also exists, undocumented; no
  installed plugin declares it.)
- **It's display-only.** Ardour doesn't send input to it; interacting means opening the plugin's
  own GUI.
- **Who implements it:** ACE (Ardour's bundled plugins), x42 (installed here: `darc`
  compressor, `dpl` limiter, `fil4` EQ, `meters`, `tuna`), and LSP (194 of `lsp-plugins.lv2`'s
  plugins).

**Verified against the sources** (2026-11-15; Ardour `libs/ardour/lv2_plugin.cc`,
`gtk2_ardour/plugin_display.cc`; x42 `fil4.lv2`, `tuna.lv2`, `darc.lv2`; LSP
`lsp-plugin-fw`'s LV2 wrapper):

- **`render` runs concurrently with `run()`.** Ardour calls it from its GUI thread with no lock
  against processing; the plugins synchronise their own state for it (tuna with a pair of
  sequence counters, fil4 reading its filter coefficients as they are, LSP turning a flag into
  `queue_draw` in `run()` and drawing from its meshes).
- **The pixels are premultiplied, native-endian Cairo ARGB32**: Ardour `memcpy`s them into a
  `CAIRO_FORMAT_ARGB32` surface, and x42 and LSP draw them with Cairo (both link libcairo).
- **`queue_draw` must be offered**, not just `render` polled: tuna and x42-comp only take the
  snapshot they draw while the feature is there, so without it their pictures never change.
  `livi` has no way to add host features (0.8 nor 0.9), so live-keys vendors it with an
  `instantiate_with_features` (`vendor/livi`).

**Other formats:** VST3 has only PreSonus's `IGainReductionInfo` (one dB value for a meter);
CLAP has two drafts, `clap.mini-curve-display/3` (the plugin fills curves — an EQ's response, a
compressor's transfer curve or gain reduction over time — which the host draws) and
`clap.gain-adjustment-metering`. None of the installed VST3 or CLAP plugins (Surge XT, Vital,
LSP, OT P1ANO S) implement any of them, so they're left for later; mini-curve-display would be
the one to host, drawn with our own style.

### Hosting it in live-keys (done, 2026-11-15)

1. `Lv2Host::instantiate` offers every plugin the `queue_draw` feature; its callback only sets
   a per-plugin `AtomicBool` (it's called from `run()` on the audio thread).
2. At instantiation the plugin's `extension_data` is asked for the interface; an
   `InlineDisplay` (the `render` function, the handle, the flag) goes into
   `PluginEditor::Lv2`.
3. The GUI's poll timer renders, on the main thread, the displays that asked to be redrawn,
   at most ~30 times a second each, only for squares on a tab that shows (a slot's first tab
   for audio effects, its MIDI tab for MIDI effects, the master chain always), at the
   square's size in physical pixels (78 px inside the border × the scale factor — not a
   larger image scaled down, which blurs x42's 1 px lines), and turns the surface into a
   `slint::Image` (reordered to RGBA, still premultiplied). The flag starts set, so each
   display is drawn once at load; a changed scale factor redraws it. A plugin is never
   dropped while its meta exists, and lending happens on the main thread too, so a render
   never meets a missing or lent-out instance.
4. `FxSlot` shows that image (fitted, centred) instead of the name-based curve, the name only
   while hovered; the fallback stays for plugins without one (every VST3 and CLAP, most LV2).

### Our own effects

The same split as for instruments:

- **Built in (A1-style):** we draw the inline view natively in Slint from the plugin's live
  values (gain reduction, EQ bands), and it can be **interactive** — drag an EQ band, the
  compressor's threshold — which the Ardour extension can't do. No extension needed.

### The effect popup (decided direction, from a sketch)

Clicking a built-in effect's FX square opens a small popup over the slot (instead of the
plugin GUI a third-party effect opens):

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
- Third-party effects open their own GUI (or parameter window) and have the right-click menu;
  their square shows the Ardour inline display if they have one, else the name (and, until
  profiles exist, the guessed curve).
- Built-in effects may not need a "GUI" button at all; the popup *is* their GUI.
- **Our own LV2 plugins (A2-style):** implementing the Ardour extension makes them look good in
  Ardour too, but in live-keys they'd be limited to a picture, unless we also add our own
  interactive layer.

**Decided:** our own effects are **built in only** (they needn't work in other hosts), with
native, interactive Slint views — so they never go through the extension. **Leaning:** still
host the Ardour extension for third-party LV2 effects (cheap, and LSP/x42 look great
immediately); a plain fallback (name, or a guessed curve) for everything else.

## Part D — recommended plugins and the install window

Decided 2026-11-16. live-keys doesn't copy third-party plugins into its own packages. The
plugins it has profiles for are **recommended**, and the app can install them.

### Licences

| Plugin | Licence | Where it comes from on Ubuntu 24.04 |
|---|---|---|
| setBfree | GPL-2+ | Ubuntu (`setbfree`, 0.8.13: the version the profile was verified with) |
| mda-lv2 (EPiano) | GPL-3+ | Ubuntu (`mda-lv2`) |
| x42-plugins | GPL-2+ | Ubuntu (`x42-plugins`; inline displays) |
| lsp-plugins | LGPL-3+ | Ubuntu (`lsp-plugins-lv2`; inline displays) |
| Surge XT | GPL-3 | Not in Ubuntu; a `.deb` on Surge's GitHub releases |
| Helm | GPL-3 | Not in Ubuntu; an old (2014–2019) `.deb` from the developer |
| Vital | The free binary is **proprietary**; its 1.0.6 source is GPL-3 under a trademark condition | Not redistributable; only a renamed fork (Vitalium, Vial) could be |

Copying a GPL plugin into our packages would be allowed (live-keys is MIT, which is
GPL-compatible, so even the FSF's view that an in-process plugin forms a combined work causes
no conflict), but it brings the duty to ship or offer each plugin's source, and versions to
keep up. Recommending them avoids both.

### The `.deb`'s `Recommends`

`[package.metadata.deb]` in `crates/gui/Cargo.toml` gets a `recommends` line with the plugins
in Ubuntu: `setbfree, mda-lv2, x42-plugins, lsp-plugins-lv2`. apt installs recommended packages
by default, so a normal install brings them along, and a user can still leave them out
(`--no-install-recommends`) or remove them.

### Installing recommended plugins and samples (a window in the app)

A window (from Settings, and suggested once on a first run when something's missing) lists
everything live-keys recommends, each with its state (installed, missing, update available),
its licence, its size, and an Install button (plus "Install all"):

- **Packages from the distribution** (the `Recommends` above, for users who installed without
  them or not from the `.deb`);
- **Plugins only available as a `.deb` of their own** (Surge XT; Helm perhaps): downloaded from
  their release page, checked, and installed as a package;
- **Our instruments' samples** (Salamander for the piano): downloaded into
  `<data dir>/live-keys/samples/`, checksummed, no root needed. (A first version is done,
  2026-11-22: picking the piano without its samples offers the download in a dialog; the
  window listing everything comes later.)

After an install, the app rescans plugins.

Open:

- **How to install packages as a normal user.** PackageKit (its D-Bus API, which the desktop's
  software centre uses too, and which asks for authorisation itself: `InstallPackages`,
  `InstallFiles` for a `.deb`), or `pkexec apt-get install`. PackageKit is the cleaner one;
  check that it's on a default Ubuntu desktop.
- **Where the list comes from**: compiled in (simple, changes with releases) or a small file
  fetched from our repository (download URLs and checksums can change between releases).
- Hosting Surge XT's `.deb` in our own APT repository instead (allowed with its source; then
  it's a plain `Recommends` too).
- Other distributions and platforms: the samples part works everywhere; packages differ per
  distribution, and Windows/macOS would need installers or downloads of their own.

## Possible order of work

1. **Panel profiles** in the host (data model, `param` and `setbfree` targets, shipped + user
   profiles, the slot tab rendering the fixed section and up to ten knobs/switches,
   choosing a cell's control from its context menu), with setBfree's profile and a few more
   shipped ones, and the window's minimum size (done: 1200×1027, now 1500×995 with the sidebar). Replaces the "first five
   parameters" placeholder.
2. **Customise panel** beyond the context menu (relabelling, a widget per parameter, overrides)
   for third-party plugins, and the `cc` target.
3. **LV2 inline display** hosting in the FX squares (done, with click-to-open, the right-click
   menu and drag-to-reorder).
4. **The truce spike** (can run in parallel with 1–3).
5. **Built-in format** (`PluginFormat::Builtin`) and the first instrument core (the Salamander
   piano, see `own-plugins.md`) with its built-in front-end, its standalone build and shipped
   presets. (Done, 2026-11-16, but the standalone build: `crates/builtin`, the SFZ engine and
   the piano in `crates/instruments/`.)
6. First built-in effect (a compressor) with the effect popup and its interactive graph.
7. **Recommended plugins** (Part D): the `.deb`'s `Recommends` (any time), then the install
   window (needed by 5 for the piano's samples).

Later, separately: MIDI learn for hardware controls; non-X11 backends for VST3 editors and
CLAP GUIs once the app runs on Windows/macOS. (**Hosting CLAP** is done — `crates/clap-host`
— for third-party plugins; our own instruments don't need it, since live-keys uses their
built-in front-end.)

## Open questions

- **Panel profiles for effects** (later): whether a profile also picks a third-party effect's
  popup knobs and FX-square curve. Revisit with the effect popup.

- **Toolkit**: confirm truce with the spike above (or fall back to nice-plug).
- **Recognising each other**: the id scheme and shared presets above, once the spike shows
  truce's state and preset formats.
- Ardour's CLAP support (unconfirmed), and so whether Linux users would need the LV2 build for
  Ardour at all.

## Sources

- Ardour API docs, inline display: [`LV2_Inline_Display_Interface`](https://ardour.org/files/doxygen/structLV2__Inline__Display__Interface.html),
  [`LV2_Inline_Display`](https://ardour.org/files/doxygen/structLV2__Inline__Display.html),
  [`lv2_extensions.h`](https://ardour.org/files/doxygen/lv2__extensions_8h_source.html)
- Ardour commits: [implementing the extension](https://git.ardour.org/ardour/livetrax/commit/c32824e4523a920c9e541cdc40ebaef47e17da29),
  [dropping the Cairo dependency](https://git.ardour.org/ardour/livetrax/commit/93bc9b972888f5a8c08115eccb66a35d4f3853a6)
- Carla's host support: [Carla commit 856bbf60](https://git.kx.studio/falkTX/Carla/commit/856bbf6015720cdccbe379508b0fd80606523d27)
- CLAP extensions: [free-audio/clap](https://github.com/free-audio/clap), [CLAP two years later (LibreArts)](https://librearts.org/2024/11/clap-api-two-years-later/)
- CLAP host support: [CLAP vs VST3 (2026), Spectral Colors](https://spectral-colors.com/news/clap-vs-vst3-2026/),
  [VST3 vs AU vs AAX vs CLAP (2026), Antares](https://www.antarestech.com/blog/vst3-au-aax-clap-plugin-formats-2026),
  [CLever Audio Plug-in, Wikipedia](https://en.wikipedia.org/wiki/CLever_Audio_Plug-in)
- Toolkits: [truce](https://github.com/truce-audio/truce), [nice-plug](https://codeberg.org/RustAudio/nice-plug),
  [nih-plug's hand-over issue](https://github.com/robbert-vdh/nih-plug/issues/265),
  [RAVET 0.1.0](https://users.rust-lang.org/t/ravet-0-1-0-audio-plug-ins-and-standalone-apps-in-rust/143550),
  [DPF licensing](https://git.kx.studio/DISTRHO/DPF/src/commit/4100e47d4c13dc7e5fc6b087be442ac3c1237896/LICENSING.md),
  [CLAP preset discovery discussion](https://github.com/free-audio/clap/discussions/308)
- Rust plugin frameworks and licensing: [nih-plug](https://github.com/robbert-vdh/nih-plug),
  [nice-plug](https://github.com/sekwah41/nice-plug), [nih-plug issue #135](https://www.github.com/robbert-vdh/nih-plug/issues/135),
  [clap-wrapper](https://docs.rs/clap-wrapper) (cites the VST3 SDK's MIT licence),
  [VST3 becomes open-source (discuss.pixls.us)](https://discuss.pixls.us/t/vst3-becomes-open-source-asio-goes-gpl-compatible/53917)
