# Nektar Impact LX+ (LX88+) DAW protocol — as implemented by Nektar's scripts

Status (2026-11-22): **worked out from Nektar's own DAW scripts, not yet confirmed on the
hardware** (see §9 and the hardware checklist in `nektar-impact-prompt.md`). Only protocol
facts are recorded here; none of Nektar's code is copied into this repository.

**Where the sources come from** (Nektar's downloads for registered owners, in
`~/Downloads/`; read as text only, never run):

```shell
N=/tmp/nektar && mkdir -p $N/fl $N/logic && cd $N
# FL Studio script (Python, obfuscated names): unpack the NSIS installer with 7-Zip
unzip -q ~/Downloads/Impact_LX+_Fl_Studio_Support_WIN_1.0.16.zip -x '__MACOSX/*' -d fl
7z x -y -oflx "fl/Impact_GX_LX+_SE49_Image_Line_Support_WIN_1.0.16/Impact_GX_LX+_SE49_Image-Line_Support_WIN_1.0.16.exe"
#   → flx/Impact_LX+_25_49_61_88/ (device_impact_lx_plus.py + two obfuscated folders)
# Logic script (Lua, minified, readable names): dmg → pkg → LX88+ payload
unzip -q ~/Downloads/Impact_GX_LX+_SE49_Logic_Apple_support_OSX_1.4.9.zip -x '__MACOSX/*' -d logic
7z x -y -odmgx "logic/Impact_GX_LX+_SE49_Logic_Apple_support_OSX_1.4.9/Impact_GX_LX+_SE49_Apple_Support_OSX_1.4.9.dmg"
7z x -y -opkgx "dmgx/Impact GX LX+ SE49 Apple Support OSX 1.4.9/Impact GX LX+ SE49 Apple Support OSX.pkg"
mkdir lx88 && (cd lx88 && gunzip -dc "../pkgx/Impact_LX88+_Apple_Support_Installer.pkg/Payload" | cpio -idm)
tr ';' '\n' < "lx88/Library/Audio/MIDI Device Scripts/Nektar/Impact LX88+.device/config.lua" > logic.lua
#   logic.lua line numbers below are of this ';'-split file
```

The Reaper support (`Impact_GX_LX+_SE49_Reaper_*_Win_1.1.17.zip`) is a compiled control
surface DLL (`reaper_csurf_ImpactLX88+ReaperDAWControl64.dll`); its strings show it reads the
keyboard's second port ("MIDIIN2") and identifies controls by CC, consistent with the below.
The LX88+ user guide (`Impact_LX88+_User_Guide_ENG_v2_1.pdf`) covers the user presets (pages
24-28), pads (12-14) and the Setup menu incl. USB Port Setup (A2, page 23).

Sources (read as text only, nothing executed):

- **FL** = FL Studio script v1.0.16, `flx/Impact_LX+_25_49_61_88/` (see above). Short names used below:
  - `entry` = `device_impact_lx_plus.py`
  - `ctl` = `X11197dedb3d6beabe/X210c1840d3e412aea.py` (the LX controller logic)
  - `desc` = `X11197dedb3d6beabe/Xb113254193dd0a3f5.py` (control table)
  - `midi` = `Xd115426483daeda26/X012417a3c3cc11c17.py` (SysEx constants, checksum)
  - `dev` = `Xd115426483daeda26/X1110f38b13df3b787.py` (vendor/product ids)
  - `base` = `Xd115426483daeda26/Xa116a13943d98883e.py` (generic controller base)
  - `takeover` = `Xd115426483daeda26/Xe119a81743d68f2a3.py` (soft takeover state machine)
  - `fl` = `Xd115426483daeda26/X6113331e23dcfb12d.py` (FL API glue)
  - `control` = `Xd115426483daeda26/control.py`

  Line numbers are the files' own (they have very long lines; one line can hold a whole method).
- **Logic** = Nektar's Logic Pro script for the LX88+ (`MODEL_NAME="Impact LX88+"`, `VERSION="1.4.9"`,
  `DEVICE_FAMILY="LX+"`, logic.lua:1), split on `;` into `logic.lua` (see above); cited
  as `logic:<line>`. It's a generic multi-device engine (Panorama/GX/LX Mini code paths are in
  it too), so only what is reachable for `DEVICE_FAMILY=="LX+"` is taken as LX88+ behaviour.

Notation: hex unless stated. "ch16" = MIDI channel 16 (status nibble `F`): CC = `BF cc vv`,
note-on = `9F nn vv`. All DAW-mode traffic is on ch16.

---

## 1. Ports

- Logic for the LX+ family: `Const.keyboardPort=1`, `Const.dawCtrlPort=2`,
  `ctrlPortName="MIDI2"` (the port the DAW controls arrive **from** the keyboard),
  `keysPortName="MIDI1"` (keys/wheels/pedal), `ctrlOutPortName="MIDI1"` (the port the host
  **sends to** the keyboard on) — logic:1106-1108. The override at logic:2630 (ctrl on MIDI1)
  is inside `if DEVICE_FAMILY~="LX+"` (logic:2609-2630), so it does not apply to the LX+.
  logic:2631 switches the output to MIDI2 only for GXP or when the device has 2 MIDI outs.
- FL: the controller description's default "port" argument is `2` (desc:7), used to pick the
  USB-port-mode value sent at init (ctl:48-54, see §2.3). FL itself is wherever the user
  assigned the script.
- **Summary:** keyboard → host DAW controls on the keyboard's **2nd** USB MIDI port; notes on the
  1st; host → keyboard (SysEx, LEDs) on the keyboard's **1st** (only) input port.

## 2. Connection / handshake

### 2.1 Sequence (FL)

1. `OnInit` → controller init (entry:8, fl:346, ctl:346-348). On non-macOS
   (`X6107409923e8e2158` true; false only on macOS+FL, ctl:22-23) it sends the **universal
   identity request** `F0 7E 7F 06 01 F7` (midi:38, ctl:437) and enters state "waiting".
2. `OnIdle` while waiting (ctl:351-352, 438-443): resends the identity request, at most
   `X81095efc83e6d9d75 = 2` more times (base:12, ctl:22). The 500 ms spacing constant
   (base:12) is compared against a timestamp that is never updated, so in practice the two
   retries go out on the next two idle calls; after that the state becomes "failed" and later
   replies are ignored. (On macOS FL matches the device name instead, ctl:456-462.)
3. Identity reply (`OnMidiIn` while waiting, ctl:373-374, 444-455). Only SysEx is looked at:
   - bytes [8],[9] → family = `[9]<<7 | [8]` must be `9319` (dev:5);
   - bytes [10],[11] → model = `[11]<<7 | [10]` must be one of the configured products
     (entry:7 passes LX25+/49+/61+/88+), dev:6, dev:18:

     | model | id (dec) | bytes [10] [11] |
     |---|---|---|
     | Impact LX49+ | 8213 | 15 40 |
     | Impact LX25+ | 8214 | 16 40 |
     | Impact LX61+ | 8215 | 17 40 |
     | **Impact LX88+** | **8216** | **18 40** |
     | Impact LX Mini | 8217 | 19 40 |

     Logic gives the full expected reply for the LX88+:
     `F0 7E 7F 06 02 00 01 77 67 48 18 40 xx xx xx xx F7` (logic:2641-2643) — manufacturer
     `00 01 77`, family `67 48` (= 9319), model `18 40`, then 4 version bytes. Neither script
     reads the version bytes (firmware not checked).
4. On a match, `_X610cd23fa3e355e9f` (ctl:24-56): binds the control table, then sends the
   **USB port mode SysEx** with value `2` for FL (`1` if the host is Ableton) —
   ctl:48-54, midi:41/40:

       F0 00 01 77 7F 01 05 00 01 0F 01 02 67 F7     (FL, "mode 2")
       F0 00 01 77 7F 01 05 00 01 0F 01 01 68 F7     ("mode 1", the Ableton variant)

   then clears the Clips/Scenes LEDs (via the user-page setter, ctl:91-102 → ctl:306-307,
   see §4.2), and, for FL, primes soft-takeover targets (ctl:55-56).
5. `OnDeInit` (entry:9, fl:347, ctl:349-350, 433-436): sends USB port mode **0**:

       F0 00 01 77 7F 01 05 00 01 0F 01 00 69 F7     (midi:39)

   and turns Clips/Scenes LEDs off (ctl:436 → ctl:306-307).

### 2.2 Sequence (Logic, LX88+)

- Start (logic:3430): `START_UP_SYSEX_MSG` = `F0 00 01 77 7F 01 05 00 01 0F 01 02 <cs> F7`
  (logic:2634-2637; note its "checksum" is the plain sum `0x19`, see §3.2).
- Identity request `F0 7E 7F 06 01 F7` is sent whenever the engine sets `sendDeviceInquiry`
  (logic:2926, set from many places, e.g. logic:523, 983, 3438, 3830) — this looks like a
  Logic-specific way to provoke traffic, not part of the device handshake (FL never repeats it).
- Instrument change in Instrument mode (logic:376-382): port mode `3` if the instrument is
  "Komplete Kontrol" (`requiresPort1`, logic:383-385), else `1`; and
  `SysSetBrowseButtonsDualPort(1|0)` = `01 05 00 70 02 01 v` (logic:1417) for KK only.
- Shutdown (logic:3434, 2638-2640): for LX+ first a "SAL close"
  `F0 00 01 77 7F 01 09 00 00 00 01 00 75 F7`, then port mode 0
  `F0 00 01 77 7F 01 05 00 01 0F 01 00 69 F7`.

### 2.3 USB port mode (global element 0x0F, Logic name `USB_SET=15`, logic:2582)

Values seen: **0** on shutdown (both), **1** Ableton/FL-variant (ctl:49-51) and Logic for
non-KK instruments, **2** FL Studio (ctl:52) and Logic at start-up, **3** Logic when the
instrument is Komplete Kontrol (function named `requiresPort1`).
**Uncertain:** the code never says what each value routes where. Plausibly 0 = DAW mode off
(controls back to standalone), non-zero = DAW mode with a port arrangement; FL, the simplest
host, uses **2**, which fits the port layout of §1 (DAW controls on port 2). Treat 2 as "the"
value for our host; the meanings of 1 and 3 are not determinable from the code.

## 3. Nektar SysEx format

### 3.1 Layout

    F0 00 01 77 7F  CMD TGT IDX  ID_HI ID_LO  LEN  DATA[LEN]  CS  F7
    \_ header ____/ \_______________ payload _______________/

- Header `F0 00 01 77 7F` (midi:36; logic:2581 `SYX_HDR`). `00 01 77` = Nektar's
  manufacturer id, `7F` = all devices.
- `CMD`: `CMD_SET=1, CMD_GET=2, CMD_REVERT=4` (logic:2582). Every message either script sends
  uses 1.
- `TGT`: `TGT_PRESET=1, TGT_GLOBAL=5, TGT_DISPLAY=6, TGT_SAL=9, TGT_LOOPBACK=15,
  TGT_CONFIGURE=18` (logic:2582). FL also sends TGT `0x0B` (keystroke, §5).
- `IDX`: for TGT_DISPLAY a page/template (`IDX_CUR=0, IDX_MSG=1, IDX_MIXER=2, …`, logic:2582);
  0 for global settings.
- `ID_HI ID_LO LEN DATA`: element id (two bytes), data length, data. Inferred from every
  builder having this shape, e.g. soft takeover `00 | 01 01 | 01 | v` (logic:1462-1463), pad
  colour `00 | 00 pad | 01 | colour` (logic:1448), USB mode `00 | 01 0F | 01 | v`
  (logic:1416). Several (element, len, data) groups can follow one another in one message
  (logic:1446, 1454). This field split is **inferred**, not named in the code.

### 3.2 Checksum

The checksum is the 7-bit two's complement of the sum of the payload bytes **from CMD up to
the last data byte** (the header, incl. `7F`, is not summed):

    cs = (128 - (sum(payload) % 128)) % 128

- FL: `X111a302453d5f1ee6` (midi:91-99) sums `data[start .. len-2)`; with start = 5 (after the
  header) it reproduces every hard-coded FL constant: e.g. `01 05 00 01 10 01 01` sum 25 → `67`
  (midi:42), `01 06 00 01 01 01 04` sum 14 → `72` (midi:48), `01 09 00 00 00 01 00` sum 11 → `75`
  (midi:52), `01 0B 00 00 01 02 45 04` sum 88 → `28` (midi:53), port mode 0/1/2 → `69/68/67`
  (midi:39-41). All 17 FL constants check out.
- Logic: `checksum(a4)` (logic:1322-1326) is the same formula over the payload; used by every
  dynamically built message (port mode, soft takeover, pad/LED colours, …).
- **Inconsistency (uncertain how the device validates):** some of Logic's hard-coded strings
  are off by one — e.g. soft takeover `… 01 01 01 03 74` (logic:2560-2562) where the formula
  gives `73` (FL's own constant for the same message is `73`, midi:49), `SAL_ON` `3F 36`
  (logic:1327, formula `35`) — consistent with summing the header's `7F` too; others in the
  same file match the formula (logic:2582-2584). And Logic's start-up message appends the
  plain, uncomplemented sum `0x19` (logic:2634-2636) where FL sends `0x67`. Since both shipping
  scripts send mismatching checksums for identical payloads, the firmware either ignores the
  checksum or accepts both; the code can't tell. **Use the formula above** (it's what FL's
  constants and both scripts' generic builders use).

## 4. Input map (keyboard → host, DAW mode, port 2, ch16)

All are CCs on ch16 (status `BF`) unless noted. Values: faders and knobs are **absolute
0–127** (every FL control is `is_absolute=True`, control:8; FL scales `/127`, base:18, ctl:172;
Logic's `isRelative` returns false for this device, logic:2666). Buttons: non-zero = press,
`0` = release (FL tests `data2 != 0`, midi:78-79, and keeps the last value per CC to know what
is held, base:17, 23; Logic: "Shift On" `BF 6C 7F`, "Shift Off" `BF 6C 00`, logic:3269).

### 4.1 Mixer vs Instrument controls (desc:9; same numbers in Logic's `Const.CC`, logic:1138)

The keyboard sends **different CC ranges per mode**, so the host learns the mode from which
range moves; FL switches its internal mode that way (Mixer: ctl:194, 216-217; Instrument:
ctl:170) and on the mode buttons (§4.2). FL never tells the keyboard which mode to be in.

| control | Mixer mode | Instrument mode |
|---|---|---|
| faders 1–8 | CC 12–19 | CC 38–45 |
| fader 9 | CC 20 | CC 46 |
| fader buttons 1–8 | CC 21–28 | CC 47–54 |
| fader button 9 | CC 29 | CC 55 |
| knobs 1–8 | CC 30–37 | CC 56–63 |

- desc:9: Inst fader buttons `range(47,55+1)`, Mixer buttons `range(21,29+1)`, Inst knobs
  `range(56,63+1)`, Mixer knobs `range(30,37+1)`, Inst faders `range(38,47)` (= 38–46), Mixer
  faders `range(12,20+1)`; fader CC 20 carries the "master/selected" flag (desc:9 `cc==20`).
- Logic names CC 20 the selected-track fader (`…_EXCLUSIVE`/`selectedTrack`, logic:3818) and
  CC 29 `XC_TRACK_BUTTON_MODIFIER` (logic:1138). FL: holding button 9 (CC 29) turns buttons
  1–8 from mute into solo (ctl:202-203); pressing it re-targets fader 9 to master (ctl:201).
- LX Mini differs (only knobs + one fader, desc:11); irrelevant for the LX88+.

### 4.2 Buttons (desc:7, ch16)

| CC | control | notes |
|---|---|---|
| 101 | Click | |
| 102 | Loop (cycle) | |
| 103 | Rewind | FL acts on press and on release (release stops scrubbing, ctl:141) |
| 104 | Fast Forward | as Rewind |
| 105 | Stop | |
| 106 | Play | |
| 107 | Record | |
| 108 | Shift | held state tracked; combos below |
| 109 / 110 | Track − / Track + | |
| 111 / 112 | Patch − / Patch + | |
| 113 | S1 = **Mixer** button | FL: → mixer mode (ctl:161) |
| 114 | S2 = **Inst** button | FL: → instrument mode (ctl:162) |
| 115 | S3 = **Preset** button | FL: → preset mode (ctl:163) |
| 119 | Page | FL toggles its "user page" (ctl:293-295) |
| 65 | Function | (FL binds nothing for LX+ beyond group handler) |
| 117 / 118 | Clips / Scenes | listed in desc:7; FL binds no action for LX+ |
| 100 / 99 | Instrument / Internal | LX Mini only (ctl:283, 292; logic:2620-2623) |

Logic: for `DEVICE_FAMILY=="LX+"` `XC_S1..S3` are nil and CC 113/114/115 are its
Mixer/Instrument/Preset mode buttons (logic:2609-2616 move them only for other families;
logic:3270-3272 "Instrument"/"Preset" controls; logic:2719-2750 mode handling).

Pads: desc:7 lists note controls 60, 62, 64, 65, 67, 69, 71, 72 (C major scale from C4); Logic's
pad note table is `{60,62,64,65,67,69,71,72,…}` on ch16 (logic:1139, used with `0x90+MIDI_CH`).
FL binds nothing to them (only CC status is dispatched, base:16). **Uncertain:** whether the
pads send these notes on ch16 only in a DAW pad mode (Logic switches that with the "SAL"
message, §6) or always.

Shift combinations (FL, host-side meaning only): Shift + mixer button 1/2 (CC 21/22) = bank
down/up by 8 (ctl:197-199; Logic same, logic:~2805); Shift + Click = undo (ctl:247), Shift +
transport = alternative functions (ctl:244-279).

## 5. Output (host → keyboard, port 1)

### 5.1 Soft takeover ("null") indicator — the only display write in either script

    F0 00 01 77 7F 01 06 00 01 01 01 vv cs F7          (TGT_DISPLAY, IDX_CUR, element 01 01)

| vv | cs | meaning | FL | Logic |
|---|---|---|---|---|
| 01 | 75 | clear (control has picked up) | midi:51, sent for state "synced" (ctl:418) | `ACTIVE` / `""` (logic:1462-1463, 2562) |
| 02 | 74 | show **"UP"** — move the control up | midi:50, state "hw below target" (ctl:417) | `LOW` → "Up" (logic:1463, 2561, 2813) |
| 03 | 73 | show **"dn"** — move the control down | midi:49, state "hw above target" (ctl:416) | `HIGH` → "Down" (logic:1463, 2560, 2813) |
| 04 | 72 | unknown | defined at midi:48, never sent | — |

(cs per the formula; Logic's hard-coded copies say 76/75/74, see §3.2.)

**How soft takeover works:** it's entirely **host-side**. The keyboard always sends the raw
absolute position; the host compares it with the parameter's current value and (a) ignores
the control until it crosses/matches, (b) sends the indicator above whenever the state changes.
The keyboard is **not** sent the parameter value for the "null" itself in FL.
- FL (`takeover` file): per control keep hw value `h` and target `t` (0..1). State =
  synced if `|h−t| ≤ 0.03` (or `≤ 0.02` from the initial "unknown" state); else
  `h < t` → "below" (send `02` UP), else "above" (send `03` dn) (takeover:11-17). Once synced it
  stays synced while the control keeps moving (300 ms grace, takeover:7, 13). Host edits to
  the target re-evaluate (takeover:19-30) — but a target jumping 0↔127 while synced doesn't
  unsync (takeover:24). The FL value is only written while synced (ctl:210-243). The SysEx is
  sent on every state change (takeover:40-42 → base:13 → ctl:415-418).
- Logic: window of 3 (in 0..127) (logic:2554), state machine logic:2567-2576: unknown/muted →
  within window = active, `h < t` = LOW (UP), else HIGH (dn); LOW becomes active when `h > t`
  (or t=127 and h≥t), HIGH when `h < t` (or t=0) — i.e. pick-up on crossing. Reset to "muted"
  on mode/page change (logic:2577-2579, 2900). Only re-sent when the state or the control
  changes (logic:2813-2815); Logic replaces a queued indicator instead of queueing two
  (logic:1463-1466).

Logic also echoes values back as CCs (`BF cc value`, `updateCtrlValue`, logic:907-910;
`resendCtrlValue`, logic:914; after modifier changes logic:948). **Uncertain** what the LX+ does
with an incoming fader/knob CC (it has no motorised faders or rings); FL never does this for
faders/knobs. Logic also defines global elements `MIX_NULL=13`, `INST_NULL=14` (logic:2582) —
suggestive of a device-side null setting — but nothing ever sends them.

### 5.2 LEDs (CCs to the keyboard)

| what | message | source |
|---|---|---|
| Page LEDs (two LEDs in one CC) | `BF 77 (0+a)` then `BF 77 (16+b)`, `a,b ∈ {0,1}` | FL ctl:308-312 (sends (1,16)=A on, (0,17)=B on, (0,16)=both off); Logic `setDefaultUserLEDStatus(cn,co)` logic:2713-2714 with `DEFAULT_USER_LED=119` (logic:1138): Instrument default page → (1,0), user page → (0,1) or (1,1); Mixer Pan → (1,0), Smart → (0,1) (logic:2704-2711) |
| Clips / Scenes LEDs | CC 117 / 118, value 0 = off, 127 (FL) or 1 (Logic) = on | FL ctl:306-307 sends on **channel 1** (`B0 75 vv`, `B0 76 vv` — the helper's default channel is 0, ctl:305); Logic sends on ch16 (`BF 75/76`, logic:2733, 2783-2784, 2646). **Discrepancy — which channel the firmware expects is uncertain.** |
| Fader-button LEDs (mute/solo/arm/select state) | Mixer: `BF (21+i-1) v`; Instrument: `BF (47+i-1) v` | Logic logic:1557-1559 (value = the button state). FL sends none. |
| Transport LEDs | `BF 6A v` Play (106), `BF 6B v` Record (107), `BF 66 v` Loop (102), `BF 65 v` Click (101) | Logic `updateTransport` logic:621-626 with values from `updateSwitchValue` (logic:912-913). FL sends none. Exact on value (1 vs 127) not visible; Logic uses 1 for the loop-record button (logic:2026, 2040) and 127/0 for its "disabled" blink (logic:964-966). |
| Mode confirmation | `BF 71 01` on entering Mixer mode, `BF 72 01` on Instrument | Logic logic:2900-2901. **Uncertain** whether this lights the button or sets the device's mode. |
| Pad lights | note-on `9F nn v` on the pad's note (60, 62, 64, …), v = 0 off | Logic logic:1335-1339 (note helper), logic:1657 (all off), 1667, 1698. Colour SysEx builders exist (`01 06 00 00 pad 01 colour`, logic:1448; palette logic:1139) but whether the LX+ pads take colours is **not determinable**. |
| Shift "consumed" | FL: `B0 6C 7F` (CC 108 on **channel 1**, value 127) after a Shift combo was used (ctl:179, 200, 243, 296); Logic: `BF 6C 01` when another control arrives while Shift needs confirmation (logic:3439) | **Uncertain** purpose — looks like telling the keyboard Shift was used as a modifier so it doesn't do its own Shift action. Channel/value differ between the two scripts. |

No blinking in hardware: Logic toggles CCs/notes itself per beat (logic:639-640, 959-960).

### 5.3 Display text, numbers, track/bank numbers

Neither script writes text or numbers to the 3-character display. The only display message is
the soft-takeover indicator (§5.1). Logic has a generic text builder `SysMsg`
(`01 06 01 00 00 len chars… 04 cs`, logic:1342-1344) but never calls it; FL nothing. Bank /
track numbers are never sent (FL's bank is host-side only, ctl:57-84). **Not determinable**
from the code; the keyboard presumably shows values itself.

## 6. Other messages seen

- **SAL (TGT 9)**: `01 09 <dawID> 01 00 01 v` (logic:1340; Logic's dawID = 4, logic:167),
  `setPadsActive`: v = 3 pads active, 2 inactive (logic:2644-2647); `SAL_OFF/close`
  `01 09 00 00 00 01 00` (logic:1341, 2638; FL midi:52, sent by FL only on its Ableton paths,
  ctl:430, 435). Probably switches the pads between local and DAW use; **not needed** for the
  FL-equivalent behaviour.
- **Keystrokes (TGT 0x0B)**: FL `AltF12` = `F0 00 01 77 7F 01 0B 00 00 01 02 45 04 28 F7`,
  `CtrlF12` = `… 45 01 2B F7` (midi:53-54), sent to the keyboard to close FL plugin windows
  (fl:217-218; ctl:279, 365). Presumably the keyboard types them as a USB computer keyboard —
  **uncertain**, irrelevant for us.
- Global elements 0x10, 0x11, 0x12 (`01 05 00 01 1x 01 {0,1}`, midi:42-47) are defined in FL but
  never sent; meaning unknown.

## 7. Keep-alive, refresh, idle

- No keep-alive: FL's `OnIdle` only does the identity retries (§2.1), transport scrub repeat
  (fl:312-321, host-side) and LX Mini long-press handling (ctl:353-368). Nothing is sent
  periodically.
- `OnRefresh` (fl:327-336) and `OnDirtyMixerTrack` (fl:337-343) only update host-side
  soft-takeover targets and the Page LEDs (ctl:397-398 → §5.2). No state dump is requested
  from the keyboard at any point.

## 8. Minimal recipe for our host

1. Open the LX88+'s **2nd** input port for DAW controls (ch16) and the **1st** output port.
2. Send `F0 7E 7F 06 01 F7`; accept `F0 7E 7F 06 02 00 01 77 67 48 18 40 …` (retry twice).
3. Send `F0 00 01 77 7F 01 05 00 01 0F 01 02 67 F7` (DAW mode, FL's value).
4. Read §4's CCs (absolute; buttons non-zero/0); infer Mixer vs Instrument from the CC range.
5. Do pick-up host-side and show it with `… 01 06 00 01 01 01 {01|02|03} cs F7`.
6. Optionally set Page LEDs (`BF 77 a`, `BF 77 16+b`), Clips/Scenes, fader-button and transport
   LEDs as Logic does.
7. On exit send `F0 00 01 77 7F 01 05 00 01 0F 01 00 69 F7`.

## 9. Open questions (to test on the hardware)

- Meaning of USB port modes 1 and 3; whether mode 2 is right on Linux (which ALSA port gets what).
- Whether the firmware checks the SysEx checksum (§3.2).
- Channel for the Clips/Scenes LEDs and the Shift "consumed" message (FL ch1 vs Logic ch16).
- What the keyboard does with CC values echoed for faders/knobs; what `BF 71/72 01` does.
- On-values for transport and fader-button LEDs (1 vs 127).
- Whether pads send/light on ch16 only after a SAL message; pad colours on the LX+.
- What soft-takeover value `04` shows; any way to write the display beyond UP/dn.
