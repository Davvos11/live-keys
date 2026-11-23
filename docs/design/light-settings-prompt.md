# Prompt: light settings per target (what a lit control shows)

Paste the section below into a new session (in this repository).

---

## Goal

Controls that show a value sent back to them (MIDI feedback: lit buttons and pads, LED
rings, motor faders) are set up in two places today, and the wrong one holds the choices.
**Move what a light shows to the mapping's target settings** (the learn pane), and leave
the surface (the surface editor and the surface file) describing only what the hardware
*can* do.

The developer agreed (2026-11-23) to the whole of this:

- **Surface** (surface editor, surface file): only *whether* a control shows a value sent
  back, and in which message (its own CC or note). That's a fact about the device.
- **Target** (learn pane, per mapping): what the light shows for this mapping:
  1. **whether the light follows this target** at all (a control can have several targets;
     today the first one always drives it);
  2. **invert**: already built ("Invert light", see below);
  3. for controls whose device has **colour pairs** (the Nektar Impact LX88+'s pads: an
     unlit and a lit colour, 16 pairs), **which pair**, chosen per target from the device's
     palette (e.g. red for a mute, green for Drive, orange as "mapped but off");
  4. for single-colour controls, **the "lit" value** (a note's velocity or a CC's value;
     some devices pick a colour by it: the LX88+'s pads in CC mode and fader buttons show
     1–126 green, 127 red).
- With that, the surface editor's **"lit" field goes**, and so does the Nektar driver's
  fixed orange/green for every mapped pad. A surface or driver may still propose a
  default.

Read `CLAUDE.md`, `docs/design/midi-learn.md` ("Takeover and feedback", phase 4 and later)
and `docs/design/nektar-impact.md` (§5.2, §9 "Pads (SAL)", §10.4) first.

## What exists now (as of commit e4c2f20 and the uncommitted work after it, see git status)

- **Surface format and messages** (`crates/surfaces/src/lib.rs`): a bank or control with
  `"feedback": true` has a `Feedback { lit }` (`lit` from the bank/control, default 127);
  `Surface::feedback(control, Shown)` builds the message: a CC with the position or
  `lit`/0, a note-on with `lit` or **a note-on with velocity 0 for off** (not a note-off:
  the LX88+'s pads ignore a real note-off, tested 2026-11-23), pitch bend. `BankSpec`/
  `ControlSpec` have `feedback` and `lit`. Shipped surfaces: `akai-lpd8.json` (pads with
  feedback), `nektar-impact-lx88plus.json` (fader buttons on Mixer/Inst, pads, Page,
  Clips, Scenes).
- **Drivers** (`crates/surfaces/src/driver.rs`): `Driver::mapped(surface, &[bool])` is
  told which controls are mapped; `NektarLxPlus` gives mapped pads the colour pair
  `ORANGE_GREEN` (10) and the others `OFF_YELLOW` (0), sending only the pads whose code
  changed (`pad_colour`: `F0 00 01 77 7F 01 06 00 00 <pad 1-8> 01 <code> cs F7`).
  `Driver::feedback` overrides a control's own message (the Page button's two LEDs). The
  16 colour codes and their names are in `nektar-impact.md` §9 ("Pads (SAL)": 0 off/yellow,
  1 red/yellow, … 15 yellow/orange; unlit colour first, lit second).
- **Engine** (`crates/engine/src/mapping.rs`, `mapper.rs`): `ControlSetup::feedback`;
  `Link { target, mode, invert_feedback }` (added by the other agent); `feedback_value`
  shows the **first link's** state (`Shown::Position` for ranges, `Lit` for toggles,
  held state for momentary), inverted if that link says so. The mapping thread
  (`State::flush`) sends a driver's override or `Surface::feedback`, and
  `State::sync_drivers` calls `Driver::mapped(.., &self.mappings.mapped(index))`.
- **GUI** (`crates/gui/src/mappings.rs`, `learn.rs`, `ui/learn.slint`): `SceneMapping` and
  `GlobalMapping` have `invert_light: Option<bool>` (None = the target's default,
  `RackTarget::inverts_light`: mutes are lit while the slot plays); the target settings
  show an "Invert light" switch when the selected control has feedback
  (`AppLogic.learn-set-invert-light`, `MappingsUi::set_invert_light`). The surface
  editor's bank settings have "feedback" and "lit" (`surface_editor.rs`,
  `EditBankSettings`, `learn.rs` `refresh_editor`).

## What to build

1. **Format**: remove `lit` from `BankSpec`/`ControlSpec`/`Feedback` and from the
   editor (no backwards compatibility: fix the developer's files by hand if needed, see
   `CLAUDE.md`). Keep `feedback` as the capability.
2. **Per-mapping light settings** in `SceneMapping`/`GlobalMapping` (serde defaults, so
   they're optional in the files), next to `invert_light`: `follows: bool` (default true),
   `colour: Option<u8>` (a palette code), `lit: Option<u8>` (1–127). Decide sensibly what
   "follows" means with several targets: the light shows the first target that follows;
   none following = the control is never sent feedback (and then takeover applies again,
   as for controls without feedback — check `ControlState::jumps`).
3. **Palette from the driver**: e.g. `Driver::palette(surface, control) -> Option<&[(name,
   code)]>` and a default pair; `Driver::mapped` takes each control's chosen pair (or
   `None` when unmapped) instead of `&[bool]`. The Nektar pads' palette is the 16 pairs.
4. **Engine**: carry the chosen lit value (and which link drives the light) in
   `ControlSetup`/`Link`, so `Surface::feedback` (or its caller) uses it; the driver
   gets the colours with each setup (only changed pads are sent, as now).
5. **Learn pane** (`ui/learn.slint`, `TargetSettings`): a "Light" section when the
   selected control has feedback: "Light follows this target", "Invert light" (exists),
   and either a colour-pair choice (a `Select` of the palette, the two colours drawn as
   swatches) or a lit-value stepper. Defaults as today (colour: the driver's default,
   orange/green on the LX88+; lit: 127).
6. **Tests** (small, real logic only; see `CLAUDE.md` "Testing philosophy"): which link
   drives the light, invert with lit values, the driver sending changed colours only.
7. **Docs**: `midi-learn.md` (feedback section and a phase entry), `nektar-impact.md`
   §10, `CLAUDE.md`; the surfaces crate's format doc comment.
8. **Penpot** (the developer's rule: every GUI change is drawn in the design file too):
   "MIDI learn 3: target settings per control kind" gets the Light section; "MIDI learn 4"
   (surface editor) loses the lit field. Read the memory note on Penpot quirks first.

## How to work

- Test the GUI off-screen only (Xephyr `:5`, scratch `XDG_CONFIG_HOME`/`XDG_DATA_HOME`;
  never synthetic input on the developer's display). The developer's LX88+ can be
  plugged in: try the pad colours per target on it with them present (a webcam pointed
  at the pads helped before: `ffmpeg -f v4l2 -input_format mjpeg -video_size 1920x1080 -i
  /dev/video0 -frames:v 1 shot.jpg`).
- The developer's LPD8 lights pads by itself while held; recheck it if it's plugged in
  (its lights were last checked with a note-off for "off"; now it's a note-on with
  velocity 0).
- Run `cargo clippy --workspace --all-targets` and the tests when done. Commit/push to
  `master` only when asked. No backwards-compatibility code.
