# Prompt: Nektar Impact LX88+ — make its factory settings and DAW mode work in live-keys

Paste the section below into a new session (in this repository). Everything it refers to is
either in the repo or in `~/Downloads/`.

---

## Goal

Make the developer's **Nektar Impact LX88+** work with live-keys with as many of its factory
settings as possible — not only a custom preset. Today the developer keeps the keyboard on a
custom preset made for Ardour, because the keyboard's built-in **Mixer** mode never worked in
Ardour. DAWs Nektar supports (Reaper, FL Studio, Logic…) get it fully working through Nektar's
own integration (a DLL / script per DAW). We want the equivalent in live-keys: the Mixer and
Inst(rument) modes, transport and navigation buttons, LEDs and the "UP"/"dn" soft-takeover
display, plus the five user presets for plain-MIDI use.

This builds on MIDI learn, which is done (phases 1–5 of `docs/design/midi-learn.md`): control
surfaces (`crates/surfaces`), the mapping thread with takeover (jump/glide/pick up) and value
layers (`crates/engine/src/mapping.rs`, `mapper.rs`), MIDI feedback to controllers (phase 4)
and the surface editor (phase 5). Read `CLAUDE.md`, `docs/design/midi-learn.md` and
**`docs/design/nektar-impact.md`** (the protocol reference) first.

## What the developer told us

- The **USB Port Setup (A2)** setting on the keyboard (Setup menu, key A2) switches which USB
  MIDI port / channel the control-surface values are sent on; there are a few modes.
- The original LPD8 now works fully (including pad lights). Lesson from it: pads/buttons that
  light themselves while held and go dark on release need their state sent again *after*
  their own release (already implemented: `Mappings::show_again`).
- Always test GUI work off-screen (Xephyr/Xvfb, scratch XDG dirs) — never XTest on the
  developer's display. Commit/push straight to `master`, but only when asked. No
  backwards-compatibility code. Keep `CLAUDE.md` and the design docs current. Run clippy when a
  feature is done.

## What we found (details and citations in `docs/design/nektar-impact.md`)

Sources, all in `~/Downloads/` (read as text only, never run; unpacking commands are at the top
of `nektar-impact.md`):

- `Impact_LX88+_User_Guide_ENG_v2_1.pdf` — user presets (p. 24-28), pads (p. 12-14), LED
  behaviour (p. 13, 18), Setup menu incl. USB Port Setup A2 (p. 15, 23).
- `Impact_LX+_Fl_Studio_Support_WIN_1.0.16.zip` — Nektar's FL Studio script (Python, obfuscated
  names): the clearest source for the handshake, control map, SysEx constants and checksum.
- `Impact_GX_LX+_SE49_Logic_Apple_support_OSX_1.4.9.zip` — Nektar's Logic device script for the
  LX88+ (Lua, minified, readable names): ports, SysEx command/target names, port modes, LEDs.
- `Impact_GX_LX+_SE49_Reaper_{Support,PortableInstallation}_Win_1.1.17.zip` — compiled Reaper
  DLLs; only their strings were looked at (reads "MIDIIN2", controls by CC).
- `/opt/Ardour-9.7.0/share/midi_maps/Nektar_Impact_LX.map` — Ardour's map for the *original*
  Impact LX: DAW-mode CCs on ch16 (faders 12-20, mute 21-29, pan 30-37, transport 102-107,
  bank 109/110) — the same Mixer numbers as the LX+ scripts.

From the user guide:

- Five user presets (faders F1-F9, fader buttons B1-B9, knobs K1-K8), plus read-only Mixer and
  Inst presets reserved for Nektar's DAW integration:
  - 1 GM Instrument: faders CC 73, 75, 72, 91, 92, 93, 94, 95, 7; buttons CC toggles 0, 2, 3,
    4, 6, 8, 9, 11, 65; knobs CC 74, 71, 5, 84, 78, 76, 77, 10; global channel.
  - 2 GM Mixer 1-8: faders CC 7 on channels 1-8 (F9 CC 7 global), buttons CC 12 toggles
    (mute) on channels 1-8 (B9 CC 65 global), knobs CC 10 (pan) on channels 1-8.
  - 3 GM Mixer 9-16: as 2 on channels 9-16.
  - 4 "Learn friendly" 1: faders CC 80-83, 85-88, 3; buttons CC toggles 66-69, 98-101, 65;
    knobs CC 89, 90, 96, 97, 116-119; global channel. (Our shipped surface follows this one
    but lacks the buttons B1-B9.)
  - 5 "Learn friendly" 2: as 4 with the buttons as trigger (momentary) CCs.
- Transport buttons, wheels and the foot switch are global (not per preset).
- Pads: by default notes 36-43 (map 1) on the **global channel** — the same notes as keys C1-G1,
  so they can't be told apart from the keys (our shipped surface wrongly assumes channel 10).
  Pad LEDs can be driven by the host only for pads set to send CCs (data 1 = data 2): value 0
  off, 1-126 green, 127 red.
- Setup menu A2 = USB Port Setup; A#2 = MIDI out from USB.

From Nektar's scripts (the DAW protocol; **unconfirmed on the hardware**):

- **Ports:** in DAW mode the keyboard sends its DAW controls on its **2nd** USB MIDI port, keys
  on the 1st; the host sends to the keyboard on its 1st (only) input. Everything is on
  **MIDI channel 16**.
- **Handshake:** identity request `F0 7E 7F 06 01 F7` → reply `F0 7E 7F 06 02 00 01 77 67 48 18
  40 …` (model 8216 = LX88+); then USB port mode 2: `F0 00 01 77 7F 01 05 00 01 0F 01 02 67 F7`;
  on exit mode 0: `… 01 05 00 01 0F 01 00 69 F7`.
- **SysEx:** `F0 00 01 77 7F | CMD TGT IDX | ID_HI ID_LO LEN DATA | CS F7`, CMD set/get/revert =
  1/2/4, checksum `(128 − sum(CMD…last data) % 128) % 128` (some Logic constants disagree by
  one — the firmware may not check).
- **Input map (ch16, absolute):** Mixer mode faders CC 12-20, fader buttons 21-29, knobs 30-37;
  Instrument mode faders 38-46, buttons 47-55, knobs 56-63; mode buttons Mixer/Inst/Preset CC
  113/114/115; Click 101, Loop 102, Rewind 103, Forward 104, Stop 105, Play 106, Record 107,
  Shift 108, Track −/+ 109/110, Patch −/+ 111/112, Page 119, Function 65, Clips 117, Scenes 118;
  pads notes 60, 62, 64, 65, 67, 69, 71, 72.
- **Soft takeover is host-side:** the keyboard always sends raw positions; the host shows UP /
  dn / picked-up with `F0 00 01 77 7F 01 06 00 01 01 01 {02|03|01} cs F7`. That's the only
  display write either script does.
- **LEDs:** CCs back on ch16 (fader buttons 21-29 / 47-55, transport, Page `BF 77 …`); pads by
  note-on; exact on-values and the Clips/Scenes channel are unclear.
- Open questions are listed in `nektar-impact.md` §9.

## What to do

1. **Confirm on the hardware first** (the developer plugs the LX88+ in and stays at the
   keyboard to press things and report):
   - List the ports (`aconnect -l`, `amidi -l`); note both LX88+ port names and how
     `midi_device_name` reduces them.
   - Record both input ports (`aseqdump -p <client>:0` and `:1`, or `amidi -p hw:X,0,0 -d` /
     `hw:X,0,1`) while the developer presses Mixer/Inst/Preset and moves faders, knobs,
     fader buttons, transport, Track/Patch, Shift and a pad — first *without* any handshake
     (as it behaves today), then after sending the identity request and port mode 2 (send with
     `amidi -p hw:X,0,0 -S '…'`; read the identity reply on the input).
   - Try the UP/dn message, an LED or two (fader button, Play, Page), and a pad note-on on ch16.
   - Try whether a wrong checksum is ignored or rejected.
   - Restore the keyboard afterwards (port mode 0 / the developer's A2 setting) so Ardour keeps
     working.
   Update §9 of `nektar-impact.md` with what was found.
2. **Design** (add to `docs/design/midi-learn.md` or `nektar-impact.md`, agreeing it with the
   developer before building): how the surface format expresses
   - **modes/pages** of one physical control (the same fader sends different messages in
     Mixer, Inst and each user preset; a mapping stays on the physical control), with the
     active mode selected by the keyboard's own mode messages (CC 113-115) or chosen in the
     panel for plain presets;
   - **several ports per device** (DAW controls on port 2, keys on port 1 — today a surface is
     matched to one device name);
   - **device drivers**: a per-surface "driver" (handshake on connect, port mode, goodbye on
     exit, UP/dn via SysEx instead of ghosts only, LED messages) — generic enough for other
     controllers later, but Nektar first;
   - whether the DAW-mode controls should still reach the slots (they're ch16 and not musical
     — probably claimed by default) and the pad-vs-key clash in plain-MIDI mode.
3. **Build** it in phases like midi-learn: the shipped LX88+ surface with B1-B9, transport,
   Track/Patch etc. and the presets; the driver (handshake, port mode, display, LEDs on the
   mapping thread's output); Mixer/Inst mode switching; tests for the SysEx encoding/checksum
   and the mode switching; docs and `CLAUDE.md`. Test the GUI off-screen; test the protocol on
   the developer's keyboard with them present.
