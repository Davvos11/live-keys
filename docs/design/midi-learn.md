# MIDI learn and control surfaces: design notes

Status: **draft** — the direction is the developer's (2026-11-22, with a second round of
decisions the same day); the details are proposals, and the open questions at the end still
need deciding. Penpot boards (page 1, right of the
existing design):

1. **MIDI learn 1: control surfaces panel** — the window with the new bottom panel, normal use,
   and the fullscreen button (top right).
2. **MIDI learn 2: knob K1 selected, its targets** — learn mode: the targets list over the
   scenes pane (with K1's takeover mode, and per target whether it's kept with the scene),
   every possible target outlined in the slots and sidebar, K1's own targets filled and
   badged.
3. **MIDI learn 3: target settings per control kind** — the right-click popup on a slot
   control, a modulated knob, and the settings of a button's and a pad's targets.
4. **MIDI learn 4: editing a surface's layout** — the panel grown over the slots: the add
   menu, a bank's settings, learning a bank by pressing its controls.
5. **MIDI learn 5: a device without a surface** — what an unknown device shows.
6. **Surface controls** — knob, fader, pad and button in their states (default, mapped,
   selected, moving), and a knob and fader waiting to be picked up (the hardware's position as
   a ghost).
7. **MIDI learn 7a/7b: short window** — the panel collapsed into a button at the bottom of the
   scenes pane, and shown as an overlay over the slots.

## What it is

Hardware controls (the knobs, faders, pads and buttons on a keyboard or a pad controller) drive
things on screen: plugin parameters, the panel's knobs and drawbars, volumes, the metronome.
Hardware control was left for a later stage of its own in `plugins.md` ("which controls drive
a plugin means on-screen controls"); this is that stage.

Three parts:

- **Control surfaces**: a picture of each piece of hardware in a panel at the bottom of the
  window, made of banks of controls that know what MIDI each control sends. Known devices
  come with one; any other can be laid out by hand or learned by moving its controls.
- **Mappings**: each surface control has any number of **targets**, each with a **mode**
  (direct, mirrored, modulate, toggle, momentary, momentary modulate) and its range.
- **MIDI learn mode**: click a surface control (or move it), and its targets are listed where
  the scenes are; every control that can be a target lights up; clicking one assigns it,
  right-clicking one sets up just that target.

## Decisions (developer's, 2026-11-22)

- A **bottom panel** shows a configurable picture of the control surfaces in use.
- **Clicking a surface control enters MIDI learn**: its targets are listed and can be edited
  or deleted, possible targets (any visible slot control) are highlighted, and clicking one
  assigns it.
- **Target modes**: 1 direct, 2 mirrored (direct, inverted), 3 modulate (the controller adds an
  offset within a range to a value that can still be set separately), 4 toggle (cycles through
  levels, default off/on), 5 momentary toggle (a value while held), 6 momentary modulate (an
  offset while held).
- **Control kinds**: faders, buttons and pads (knobs count as faders). Modes 1–3 apply to
  faders and pads, 4–6 to buttons and pads.
- The **targets list overlays the scenes pane**; in learn mode, **right-clicking a target**
  sets up that target only.
- The panel lets the user **pick any number of MIDI devices** and recognises their controls;
  **unknown controllers are set up by the user** with an add menu (banks of n faders, n×m
  knobs, pads or buttons; single ones), each configured by entering its MIDI message or by
  moving the control.
- **Panel layout by window height** (2026-11-22): expanded and docked when the window is tall
  enough — the goal is a 1080p screen minus ~60 px for title bar and taskbar, so 1020 px; if
  it's only a little short, the slots' padding shrinks first; shorter still, the panel
  collapses into a button that shows it as an overlay. See "The bottom panel".
- **A fullscreen button** in the window's top-right corner, and F11.
- **Panel-knob targets**: both "this parameter" and "this panel position" (whatever the slot's
  panel shows there), default this parameter.
- **Each target chooses whether it's kept with the scene or in every scene.**
- **Values are layered** so scenes and presets store the value the user set (the base), never
  a modulated one: see "Values: base, modulation, effective".
- **Mappings are applied off the GUI thread**, as immediately as possible: a mapping thread
  of its own, straight to the audio thread. See "How it runs".
- **A mapped CC no longer reaches the slots** by default; per control, "also pass to the slots"
  sends it to both.
- **Keeping hardware and targets in step** (takeover): feedback to the hardware where it can
  show values (motor faders, LED rings, lit buttons and pads), otherwise one of jump, glide
  (default for controls without feedback) or pick up — with the hardware's position shown as a
  ghost in the panel while they differ. See "Takeover and feedback".

## Is there a database of controller layouts?

Looked for on 2026-11-22; **none fits**, so we keep our own (shipped + user, see below):

- [MIDI Guide](https://midi.guide/) ([API](https://api.midi.guide/docs/)) is an open database
  of CC/NRPN implementations, with a JSON API — but of the devices that *receive* MIDI
  (synths, effects), not of controllers' layouts. Useful later, perhaps, for hardware synths as
  targets; not for this.
- [Mixxx's controller mappings](https://github.com/mixxxdj/mixxx/wiki/Midi-Controller-Mapping-File-Format)
  are the largest community collection, but they're XML + JavaScript mapping each control to a
  DJ-software function, with no layout, and GPL (we're MIT).
- A [JSON schema for MIDI maps](https://cdm.link/midi-map-open-source-schema-json/) describes
  what a device *receives*, like MIDI Guide.
- Ardour's MIDI maps, Bitwig/Ableton controller scripts: per-DAW functions again, GPL or
  proprietary.

What a controller sends is a fact in its maker's published MIDI chart, so our shipped surfaces
are written from those charts (no one's mapping files copied), and checked on the hardware
where we have it. As shipped in phase 1: the LX88+'s faders and pots follow Nektar's user
guide; its pads (notes 36–43, channel 10) are a guess still to be checked on the hardware.
The LPD8 (original; pads notes 36–43, knobs CC 1–8, channel 1, pads lit by the same notes)
was checked on the developer's (2026-11-22). The first two are the developer's own: **Nektar Impact LX88+** and **Akai
LPD8**. More come from users' own surfaces (an "export" is just the JSON file).

## Concepts

- **Surface**: one piece of hardware as drawn in the panel — a name, the MIDI device it
  belongs to (`midi_device_name`, like the MIDI inputs; a pattern for shipped ones, since names
  vary: "Impact LX88+ MIDI1", "LPD8 mk2"), and its banks. A device has at most one surface;
  a keyboard with knobs is one surface whose keys aren't drawn.
- **Bank**: a grid of one kind of control (rows × columns; a single control is a 1×1 bank),
  with a name, a position in the surface, and how its controls are numbered and what they send.
  Its controls are labelled from a prefix and their number (F1…F9, K1…K8, P1…P8), or by name
  for single controls (Play, Stop).
- **Control**: kind (fader, knob, pad, button), label, and its **message**:
  - fader/knob: a CC (absolute 0–127; or relative — an endless encoder, in one of the three
    usual encodings; or 14-bit, as a CC pair), pitch bend, or channel pressure;
  - button: a CC (press = value > 63, release = ≤ 63; or a hardware toggle: each press flips
    between 127 and 0), a note (note-on/off), or a program change (press only);
  - pad: a note with velocity (press with its velocity, release), optionally its poly or
    channel pressure while held; or a CC.
  Plus the channel. The message is what identifies the control: one `(channel, kind, number)`
  per control within a device.
- **Mapping**: a control's list of targets.
- **Target**: what a control drives (see "Targets"), with a mode and the mode's settings.

## The bottom panel (boards 1, 7)

Under the scenes pane and the slots (the sidebar keeps the full height), 140 px:

- **Left column** (as wide as the scenes pane): "Control surfaces" with a collapse button;
  "MIDI learn" (stays lit as "Learning…" in learn mode) and "Edit layout"; and a dropdown
  summarising the surfaces shown, opening a checklist of every MIDI device present plus the
  remembered ones "(not connected)", like a MIDI input's (`MidiInputItem`).
- **The surfaces**, side by side, scrolling sideways when they don't fit (like the FX strip: a
  thin scrollbar under them, the wheel, dragging in the gaps). Each has its name over its banks.
  Controls show what they last sent (a knob's arc, a fader's cap, a pad or button lit while
  held), so the panel doubles as a "is my controller talking" check. **Mapped** controls have
  their value in the primary colour and a dot; **unmapped** ones are grey.
- **By window height** (the slots need ~470 px a row at their natural padding, so the window
  at its natural size is ~1135 px with the panel):
  1. **Tall enough**: the panel is docked and expanded (unless the user collapsed it — the
     collapse button; remembered in the config).
  2. **A little short** (down to 1020 px, a maximised window on a 1080p screen): the slots
     give up padding first — the header photo's height, the gaps around the tab content and
     the FX strip — down to a floor, and the panel stays docked.
  3. **Shorter** (down to the window's minimum, 995 px — unchanged): the panel collapses to
     its one row (44 px: the title, an activity dot, the device dropdown, ▴), and ▴ shows the
     expanded panel as an overlay over the bottom of the slots (shadowed, with ✕ instead of
     collapse). The row is always there in this tier, so the device choice stays at hand; it
     replaced the separate "Control surfaces" button of board 7a when built. The dot lights
     while a shown device sends something. Entering learn mode from a slot control's menu or
     by moving hardware won't open the overlay (targets under it would be hidden); the button
     does, and closing it again keeps learn mode going.
  As built (phase 1): docked and expanded from 1019 px; the slots squeeze over the 116 px
  between 1135 and 1019 (header up to 30 px shorter, the first tab's padding and spacing).
- **Fullscreen**: an icon button (Lucide `maximize`/`minimize`) at the right end of the
  sidebar's transpose row, and F11, toggle the window's fullscreen (`Window::set_fullscreen`).
  A fullscreen 1080p screen has room for the docked panel.
- Clicking a control enters learn mode with it selected; hovering shows its message and
  targets in a tooltip ("K1 · CC 74 ch 1 → Attack (Slot 1), Reverb (Slot 2), Detune (Slot 1)").

## MIDI learn mode (board 2)

Entered from "MIDI learn", by clicking a surface control, or by the context menu of a slot
control ("Learn MIDI…", which selects nothing until a hardware control moves). Left by
Done, Escape, or clicking "Learning…" again.

- **Selecting a control**: click it in the panel, or **move it on the hardware** — the last
  control moved becomes the selected one (unless "Lock" — see open question 7). The selected
  control is outlined in the panel.
- **The targets pane** covers the scenes pane: the control's label, kind, surface and message;
  its takeover mode and an "Also pass to the slots" switch (off by default; see "How it runs");
  a hint; its targets as rows (target name, slot and plugin, and a chip with mode and range —
  "Mirrored 20–80 %", "Modulate ±20 %"); clicking a row expands it to its settings (as
  board 3) — one expanded at a time; ✕ removes it. "Remove all targets" and "Done" at the
  bottom. Without a selected control: "Move a control or click one below."
- **Possible targets** are outlined (dashed, primary colour) wherever they're visible: every
  panel control (knobs, switches, levers, drawbars), each slot's volume knob and mute, the
  master volume and mute, transpose, the metronome's level, play and BPM (see "Targets" for the
  full list). The selected control's own targets are filled and carry its label as a badge.
  Other controls don't respond to clicks in learn mode (they'd change values otherwise).
- **Clicking a possible target** adds it with the default mode for the control's kind (fader
  or knob: direct, full range; button: toggle; pad: momentary at the full value); clicking one already assigned removes it. Each click shows in
  the list at once, so a run of clicks maps one knob to several things.
- **Right-clicking a target** opens the popup of board 3a: the same settings as its row, for
  that target alone, with "Assign" (or "Save" when already assigned) and "Remove".
- Targets in slots or tabs that aren't visible are only reachable in the list (as they're
  already assigned), not added from there; switch the slot's tab to see more. Plugin
  parameters that aren't on the panel are added from the plugin's parameter window, whose
  rows are targets too while learn mode is on.

## Modes

A target's value is handled as a **position** 0..1 along its control, as the panel does
(`profiles::Control::read`/`write`, `slot_view::position`; log-scaled parameters along their
curve, choices and steps snapped) — so "20 %" means the same thing on a frequency knob and a
volume. A controller's value is also a position 0..1 (a 7-bit CC: v/127; 14-bit: v/16383;
pitch bend from its 14 bits; a relative encoder accumulates into a position of its own per
target).

- **1 Direct** (fader, pad): target = from + p·(to − from). Default range 0–100 %. From > to
  is allowed and is how mirrored is stored.
- **2 Mirrored** (fader, pad): direct with the range reversed: target = to − p·(to − from).
  Kept as its own mode in the UI since that's how it's thought about.
- **3 Modulate** (fader, pad): offset = lo + p·(hi − lo), default ±20 %; target = clamp(base +
  Σ offsets). The **base** is the value set anywhere else — the on-screen knob, the plugin's
  GUI, a preset, a direct mapping — so turning the on-screen knob still works and the
  modulation rides on it. Several modulators of one target add up. The knob shows both (board
  3a): its arc is the base, a thin outer arc the modulation range around it, a dot where it
  is now.
- **4 Toggle** (button, pad): each press steps to the next of a list of levels, wrapping.
  Default two levels, 0 % and 100 % (off/on for a switch); levels can be added, removed and
  edited; for a target with choices (the Leslie lever) each level shows the choice's label,
  and the default levels are all of its choices. Which level is current follows the target:
  a press goes to the level after the one nearest its present value, so a target changed
  elsewhere doesn't make the next press look skipped.
- **5 Momentary** (button, pad): press → the held value (default 100 %; for a pad, scaled by
  velocity if "Velocity sensitive" is on), release → back to its base (the held
  value is an override, see "Values"; a change made elsewhere while held sets the base, so
  it's what the target returns to).
- **6 Momentary modulate** (button, pad): press adds an offset (default +20 %), release takes
  it off; it's a modulator like mode 3, so it combines with the base and any others.

Pads used as a fader (modes 1–3) send their velocity on press, then their pressure if they
send any (aftertouch), and on release go back to 0 for direct/mirrored if "Release to zero" is
on (default on for pads — a pad isn't a fader you can leave somewhere).

What happens when the hardware's position and a target's value disagree is under "Takeover
and feedback".

## Values: base, modulation, effective

Every target value the mappings touch is three layers, kept by the `Mapper` (see "How it
runs") for each target that has a mapping:

- **Base** — the value the user set: by mouse, the plugin's own GUI, a preset, a scene, a
  direct/mirrored mapping, a toggle. This is the target's value as far as saving goes.
- **Modulation** — the sum of the active offsets (modulate, momentary modulate), and a
  momentary override while a momentary button is held. Never saved.
- **Effective** = the override if any, else clamp(base + Σ offsets) — what the plugin gets and
  the on-screen control's needle shows (its arc shows the base, board 3a).

So saving never captures a modulated value: **slot presets, scenes and the session store the
base values** of every mapped target, in `slot.json` beside each plugin's state file
(`"values": { "<param symbol>": base }`), and restoring applies them after the state file —
they win over whatever was modulated when the state was written. This doesn't need any format
to save a different value than it's playing (LV2 state, VST3/CLAP chunks), only one list of
overrides per plugin. Plugin presets (the plugin's own format, shared with other hosts) still
hold what the plugin had; saving one while modulated warns in the status line.

Edits from anywhere set the base and re-send the effective value: a mouse drag on a modulated
knob moves the base while the modulation keeps riding on it.

## Takeover and feedback

After a preset or scene load, a mouse edit, or a second target on the same control, a
target's value and the hardware's position disagree. How others handle it:

- **Ableton Live** ([manual](https://www.ableton.com/live-manual/12/midi-and-key-remote-control/),
  "Takeover Mode", global): *None* (the parameter jumps to the control), *Pick-Up* (nothing
  until the control reaches the value, then 1:1), *Value Scaling* (the two converge as the
  control moves, then 1:1). With a controller's output port enabled for "Remote", Live sends
  values back so motor faders and LEDs follow.
- **Ardour** (generic MIDI, per binding): a controller not marked *motorized* is ignored until
  it comes within the *smoothing* distance of the parameter's value — pick-up with a
  tolerance ([forum](https://discourse.ardour.org/t/what-does-smoothing-mean-in-generic-midi-controller/88154));
  *motorized* means jump, and feedback is sent back to the controller.
- **Bitwig**: a global takeover mode, toggled per controller; *Immediate* when off
  ([user guide](https://bitwig.com/userguide/latest/default_controller_documentation)).
- Relative encoders sidestep the problem everywhere: they only say "a bit more".

Ours, per control (the targets pane's header, board 2; a surface can set a default per bank):

- **Feedback** — for controls that can show a value (motor faders, encoders with LED rings,
  buttons and pads with lights): whenever a target's effective value changes, its position is
  sent back to the device (see below), so the hardware is always where the target is and
  there's nothing to take over. Used automatically when the surface says the control has
  feedback.
- **Jump** — the first move sets the target to the control's value at once.
- **Glide** — default for controls without feedback: the first move after a mismatch starts
  the target sliding from its value to the control's over a short time (200 ms, adjustable per
  control; the slide follows the control while it keeps moving), then it follows 1:1. No dead
  zone like pick up, no click like a jump.
- **Pick up** (as Ardour's, Ableton's Pick-Up) — moves are ignored until the control passes
  the target's value (or comes within a small tolerance, for controls that skip values when
  moved fast), then it follows 1:1.

While a control and its target disagree (pick up waiting, or glide sliding), **the panel shows
both** (board 6): the control drawn at the target's value with a white mark, and a **ghost** —
a translucent pointer dot or fader cap, dashed — where the hardware is, so the user sees which
way to move it. With several targets, the mark is the first target's.

Mismatch is per target: each of a control's targets has its own value, and on a range or
mirrored target "the control's position for this value" is the value mapped back through the
range (outside the range = never picked up; jump and glide take it). Modulate targets have no
mismatch — the offset simply is the control's position — and toggles and momentary modes don't
either.

**Feedback** needs a MIDI output to the device (the output port with the same device name;
`midir` output connections, opened while the surface is shown). A control's feedback is its
own message with the value: a fader/knob's CC with the first target's position (encoders with
LED rings take the same CC on most devices), a button's or pad's note/CC with on (127, or the
surface's "lit" velocity — some pads choose a colour by it) when its first target is at a
non-zero level, off otherwise; momentary buttons light while held. Values are sent only when
they change, at most every 10 ms per control, and not straight back to a control that just
sent the same value (motor faders fight otherwise). On connecting a device and after loading
a scene, every control with feedback is sent its value.

## Targets

What a control can drive (all addressed so they survive the chain being reordered):

- **A plugin parameter** — any plugin in any slot or the master chain, by its `PluginKey` at
  runtime; on disk by its position in the chain and its `ParamInfo::symbol`. Clicking a panel
  knob makes this kind for whatever parameter the knob shows.
- **A panel position** — "slot 2, panel control 3": whatever that slot's panel shows there,
  whichever instrument is loaded. Chosen in the target's settings ("This parameter" / "This
  panel position") for targets added from the panel; default this parameter.
- **A setBfree function** (drawbars, percussion, vibrato, Leslie — the panel's
  `Target::Setbfree`): like a parameter, through `Engine::send_setbfree`.
- **A slot's** volume, mute, transpose; the **master**'s volume, mute, transpose; the
  **metronome**'s level, play/stop, BPM, tap.
- **Recalling a scene** (button/pad: a given scene, or next/previous in the list) and **MIDI
  panic** — buttons only, no value. Not in the first version unless cheap.

**Where a mapping is kept is chosen per target** ("Mapping kept: With the scene / In every
scene", board 2):

- **With the scene** (default for plugin parameters): in the slot preset's `slot.json`, per
  plugin, as `(surface id, control label) → mode + settings`. They travel with the chain into
  slot presets, scenes and the session, and a slot loaded from a preset brings its knob
  mappings along — the way a live patch is expected to behave. A scene recall replaces them
  with the scene's.
- **In every scene** (default for rack targets — slot volume, mute, transpose, the master,
  metronome — and panel positions): in `config.json` (`AppConfig.mappings`), untouched by
  scene recalls. A plugin parameter kept this way is addressed by slot, plugin id and symbol,
  so it applies whenever that plugin is in that slot, and is shown greyed otherwise.

If a control has the same target both ways, the scene's wins while that scene is loaded.

A target whose surface isn't connected, or whose control no longer exists, is kept and shown
greyed in the list (its surface's name in the row).

## Surfaces on disk

Shipped surfaces are JSON in `crates/gui/surfaces/*.json`, compiled in like panel profiles;
user surfaces in `<data dir>/live-keys/surfaces/*.json`, the same format, replacing a shipped
one with the same id wholesale. Changing a shipped surface in the editor saves a user copy.
Which devices show a surface (the panel's checklist) and whether the panel is collapsed are in
`config.json`.

An example (its message numbers are illustrative, not the LPD8's actual defaults):

```json
{
  "id": "akai-lpd8",
  "name": "Akai LPD8",
  "devices": ["LPD8"],
  "banks": [
    { "name": "Pads", "kind": "pad", "x": 0, "y": 0, "rows": 2, "columns": 4,
      "numbering": "bottom-first", "prefix": "P",
      "controls": [ { "note": 36, "channel": 10, "pressure": "channel" }, "…one per control" ] },
    { "name": "Knobs", "kind": "knob", "x": 208, "y": 0, "rows": 2, "columns": 4, "prefix": "K",
      "controls": [ { "cc": 70, "channel": 1 }, "…" ] }
  ]
}
```

Feedback (phase 4): `"feedback": true` on a bank or control, and for pads/buttons `"lit": n`.

`devices` are substrings of `midi_device_name`; the first surface (user before shipped) one of
them matches is the device's. Each control's message is stored on its own (the bank editor's
"first + step" only fills them in), since real controllers aren't always contiguous. `x`/`y`
are in panel pixels at the default size; banks have a fixed control size per kind. Positions
are what dragging a bank in the editor changes.

## Editing a surface (boards 4 and 5)

"Edit layout" grows the panel upward over the slots (no targets need to be visible while
editing) and turns the left column into the surface's settings: name, MIDI device, "+ Add",
"Learn controls…", "Delete surface", "Done". With several surfaces shown, clicking one picks it.

- **+ Add** opens a menu: a bank of faders, knobs, pads or buttons, or a single fader, knob,
  pad or button. A new bank appears at the right end, selected, with its settings open.
- **Feedback** per bank: none, or what the hardware shows (motor faders, LED rings, lit
  buttons/pads, with the velocity that lights a pad), and the bank's default takeover;
  per control, a different feedback message where a device needs one (rare).
- **Banks** have a frame with a drag handle and their name and size; dragging moves the bank
  (snapping to a 4 px grid and to other banks' edges); clicking selects it and opens its
  settings popup.
- **Bank settings**: rows × columns (for a fader bank, just the count), numbering (top row
  first or bottom row first — the LPD8 starts bottom-left), what it sends (a dropdown of the
  kinds above for its control kind; for knobs: absolute, relative ×3, 14-bit), channel, first
  number and step — which fill in every control's message — and **learning**: "Learn bank"
  listens, and each control pressed or moved in turn takes the next control's place ("Listening:
  press P3 on the LPD8 · 3 of 8"), with Skip, and Stop. What a moved control sends also sets
  the bank's message kind (a pad that sends pressure gets it, a knob sending 1/127 around
  a centre is relative). Clicking a single control while the popup is open edits just that
  control's message, or learns just that one.
- **Learn controls…** (for a whole device, board 5's "Learn by moving controls"): the user
  moves each control in turn, and banks are made from what arrives: controls of the same kind
  and message type with consecutive numbers are grouped into one bank as they come (a knob
  moved goes into the knob bank the previous knob started, if its number follows). Everything
  lands in one row; the user arranges and names the banks afterwards.
- A device in the checklist with no surface shows **board 5**: its name, a line of text, and
  "Learn by moving controls", "Lay out…" (an empty surface in edit mode) or "Hide" (untick it).

## How it runs

**As immediate as possible** (decided 2026-11-22): listening and applying happen on a
**mapping thread** of their own, neither the GUI thread (whose 16 ms poll tick would be the
latency, and which stalls whenever the UI is busy) nor the audio thread (no room there for
the bookkeeping, feedback output or glides). A hardware move reaches the plugin in the next
audio block after the mapping thread wakes — tens of microseconds plus at most one block —
the same as a note. The GUI only shows what happened, at its own pace.

```
midir callback ──(surface events)──► mapping thread ──(mapper commands)──► audio thread
      │  claim table                    │   │                                  (Rack)
      └─(unclaimed / passed through)────┼───┼──────────(MIDI ring)───────────►
                                        │   └──(feedback)──► midir output → device
GUI thread ◄──(applied values, raw events for display/learn)─┘
GUI thread ──(mappings, surfaces, base edits)──► mapping thread
```

- **Input.** `MidiInputs`'s callback (midir's thread) looks each event up in a per-device
  **claim table** (`midi::Claims`, atomics: per channel, which CCs and notes belong to mapped
  controls, plus pitch bend and channel pressure), set by `Engine::set_mappings` from each
  setup (controls that pass through claim nothing). Every event from a device with a surface
  goes onto the mapping thread's queue (a bounded `std::sync::mpsc` channel it blocks on;
  full = dropped). A **claimed** event goes only there, unless its control is set to **also pass
  to the slots** (per control, default off — a sustain pedal or mod wheel mapped *and* played);
  unclaimed events reach the slots' MIDI ring directly, as now, so keys on a keyboard that's
  also a surface lose nothing. Notes are never claimed unless a pad's note is, on that device
  and channel.
- **The mapping thread** (`crates/engine/src/mapper.rs` around the pure logic of `mapping.rs`,
  in the engine so the CLI could use it too) owns the
  mappings, the value layers and the takeover state of every mapped target. It blocks until
  events arrive (or, while a glide runs, wakes every ~2 ms to advance it), applies each
  target's mode, and sends the effective value:
  - **To the audio thread** over a ring of its own (`MapperCommand`, a second SPSC queue the
    `Rack` drains next to the engine's), so it never shares the engine's producer. Plugin
    parameters are addressed by `PluginKey`, which the `Rack` resolves to a chain position on
    the audio thread (a scan of at most 6 × 17 entries, allocation-free; an unknown key — a
    plugin removed meanwhile — is dropped), so a change can't land on whatever replaced its
    plugin, whatever order the two queues are drained in. Slot and master volumes are mapper
    commands too (the rack keeps mute apart from volume, so a fader can't unmute a muted
    slot). **As built**, mute, transpose and the metronome (volume, play, tempo) go through
    the GUI thread instead — one poll tick (16 ms) later, through the same handlers as the
    on-screen controls: they're buttons and steps, where that doesn't show, and the GUI owns
    their state (config, the slots' `MidiFilter` with the master transposition).
  - **setBfree's functions** travel as atoms on the plugin's control input; every LV2 plugin
    with atom inputs has a second input ring (`Lv2Plugin::take_control_channel`) whose
    producer is the mapping thread's (`SetbfreeSink`; the GUI channel belongs to the GUI
    thread).
  - **To the GUI thread**, afterwards, a ring of applied values and the raw events: the GUI
    updates the panel, on-screen controls, the record (`Engine::param_value`), an open VST3
    editor (`set_param_normalized` must be on the main thread; the processor already has the
    value) and an open LV2 GUI. Coalesced: the latest value per target per tick.
  - **To the hardware**: feedback, through `midir` output connections the mapping thread owns.
  - Things that aren't values — recalling a scene, starting the metronome, panic — are
    forwarded to the GUI thread, which already does them.
- **Edits from elsewhere** (mouse, a plugin's GUI, presets, scenes). **As built**: an edit
  takes the usual path (the plugin gets the new value at once); the GUI's poll timer reads
  every mapped target's position and tells the mapping thread of any that moved since it last
  saw it (`Engine::set_mapped_position`). The mapping thread takes that as the target's new
  **base**, works out the effective value again (base + offsets, or a momentary override)
  and sends it if there's modulation on top — so a mouse edit on a modulated knob moves the
  base and the modulation keeps riding on it, a block or a tick later rather than the
  same block. Pick up and glide compare against the base. A position a mapping set is
  noted as seen first, so it doesn't count. Every report of a mapping carries the target's
  base and effective value; the GUI keeps the bases (for knobs and for saving) rather than
  asking the thread.
- **Learn mode** reads the raw events the GUI gets: the last control moved is selected.
- **Setups**: the GUI resolves every mapping against what's loaded (`src/mappings.rs`) into a
  `MapperSetup` — the shown surfaces, each mapped control's settings and links, each
  target's output, its position and how a position becomes its value (a closure from the
  panel control's own mapping, so a mapped knob covers what the on-screen one does) — and
  hands it over whole when mappings, surfaces or the chains change; one that does the same
  as the last isn't handed over, so the takeover state survives. Each has a generation:
  target numbers in reports from an older setup are ignored.
- **Unit tests** for the `Mapper`'s logic (modes, ranges, layering, pick-up crossing, glide
  timing, toggle levels following the target): pure functions of events and time, kept apart
  from the threads.

## Phases

1. **Done (2026-11-22).** Surfaces only: the format, shipped LX88+/LPD8, the bottom panel with live values, the device
   checklist, the surface ring and display; the panel's three height tiers and the fullscreen
   button. No mappings yet; worth having alone to see a controller talking.
2. **Done (2026-11-22).** Mappings, direct/mirrored/toggle/momentary, to plugin parameters
   (panel controls, drawbars), panel positions and rack targets; learn mode with the targets
   pane, highlighting, click to assign, the right-click popup; the mapping thread, its audio
   ring and the claim table with pass-through; storage with the scene (`slot.json`) or in
   every scene (config); jump, glide and pick up with the ghost. Left for later: the
   parameter window's rows as targets (so an effect's parameters can't be mapped yet), "Learn
   MIDI…" in a slot control's context menu, scene and panic targets, selection lock. Learn
   mode is entered from the panel's "MIDI learn" button or by clicking a surface control, and
   left with Done or Escape; the popup of a target not mapped yet has "Assign to <control>"
   (its settings so far apply), one mapped edits it live.
3. **Done (2026-11-22).** The value layers: modulate and momentary modulate, the knob's
   modulation display, bases saved in `slot.json`. As built:
   - The layers live in the mapping thread (`mapping::TargetState`: base, a momentary
     override, offsets per modulating link, the effective value); a modulating link's
     offset is 0 until its control moves.
   - A panel knob whose target is modulated shows its base (pointer and arc), a thin outer
     arc for the modulation's reach (the sum of its links' offset ranges) and a dot at the
     effective value. Only knobs without choices; switches, levers and drawbars just show
     the effective value. Dragging such a knob sets the base, shown at once.
   - Slot presets, scenes and the session save, per plugin, the bases of its modulated
     targets (`"values"`: target and value) beside its mappings; loading puts them back
     after the state file. setBfree applies a loaded state on its next run and reports it
     afterwards, so its values are sent again when that report comes
     (`Engine::restore_setbfree`).
   - Volumes: the config (and so scenes) keeps a slot's or the master's base volume; the
     engine plays the effective one. Mute, transposition and the metronome are set by the
     GUI at their effective value, so a scene saved while one is modulated keeps that.
   - Not done: the warning when saving a plugin preset while modulated.
4. **Done (2026-11-22).** Feedback: output connections, per-bank feedback in the surface
   format. As built:
   - A bank (or a control, overriding it) with `"feedback": true` shows values sent back
     as its own message; pads and buttons are lit with `lit` (velocity or CC value, default
     127) and unlit with 0 / a note-off. Shipped: the LPD8's pads (checked on the hardware);
     the LX88+ has none yet (unknown which of its controls light up from outside).
   - A control with feedback always jumps (the learn pane says so instead of offering
     takeover). It's sent its first target's base along its range, a toggle's on/off, a
     momentary button's held state; a modulating control shows nothing (it's where it was
     turned). Its own moves aren't sent back; an incoming value equal to what it was last
     sent is taken as its echo (a motor fader arriving) and ignored. Pads and buttons that
     light themselves while held and go dark when let go (the original LPD8 does; it lights
     pad n on note n, channel 1) are sent their state again after their own release, past
     the no-repeats rule.
   - The mapping thread opens the output port with the device's name on the first value
     (`midir`), sends at most every 10 ms per control (the latest value, later) and never
     the same message twice in a row. A new setup, or a change in the devices shown or
     plugged in (`Engine::resync_feedback`), reopens outputs and sends every control its
     value. The panel draws a control where it was sent.
5. **Done (2026-11-22).** The surface editor: add menu, bank settings, dragging, learning a
   bank and a whole device. As built:
   - Opened by "Edit" next to a shown surface's name, or for a device without one by
     "Learn by moving controls" (listening straight away) or "Lay out…" on its card. It
     covers the bottom of the slots (460 px); Save or Cancel close it. Saving writes the
     surface as the user's (`<id>.json` in the user folder, replacing any file with that
     id), reloads the library and shows the device; "Delete surface" removes the user's
     file (a shipped surface with that id comes back).
   - The left column: name, "+ Add" (a bank of, or a single, fader/knob/pad/button),
     "Learn controls…", a status line, and the selected bank's settings — name, kind,
     rows, columns, numbering, what it sends (CC, note, pitch bend) with channel, first
     number and step (which renumber every control), a pad's pressure, feedback and lit
     velocity, "Learn bank" and "Delete bank". The settings sit beside the surface
     rather than in a popup.
   - Banks are framed, with a handle above that selects and drags them (4 px grid; no
     snapping to other banks yet). Clicking a control learns just that one.
   - Learning a bank fills its controls in turn (Skip, Stop); a control already sending
     the learned message swaps with it. The first control learned sets the bank's channel.
     Learning a device groups consecutive numbers of one message type and channel into a
     bank as they come (CCs as knobs, notes as pads, pitch bend as a fader); controls the
     surface has already are ignored. A control sending a stream counts once.
   - Labels must be unique on a surface (mappings name controls by them): a second bank
     of a kind gets prefix `K2.` etc., and a file with two controls of one label is
     rejected.
   - Not done: per-bank default takeover, a per-control feedback message, relative and
     14-bit knobs, inferring pressure or relative encoders while learning. The panel is
     still 140 px high: a surface laid out taller is cut off there.
6. Smoothing (decided 2026-11-22, a later phase): where a target is continuous, the audio
   thread interpolates between a control's successive values (per block, like the gain
   ramps), so 7-bit steps don't zipper. Not for stepped, choice or toggle targets.
7. Later: relative/14-bit encoders, scene targets, shift layers.

## Open questions

1. **Selecting by moving** can pick the wrong control when the user nudges something while
   assigning. A lock (pin icon in the targets pane) that keeps the selection?
2. **Pages/shift layers**: many controllers have a shift button that changes what the knobs
   send, or banks switched on the device. As separate banks, with the same labels? Later.
3. **Value scaling** (Ableton's third mode) wasn't asked for; glide covers the same need (no
   jump, no dead zone). Add it only if someone misses it.
4. **Glide time**: per control or per target? Per control is simpler to show; start there.
