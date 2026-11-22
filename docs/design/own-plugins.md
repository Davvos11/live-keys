# Our own instruments: brainstorm

Status: **brainstorm**. Started 2026-11-16. Ideas for the instruments live-keys could ship
itself. How they're built and packaged (one core, a built-in front-end and a standalone
plugin) is decided in `plugins.md`, Part A. This file is about *what* to build.

Order of interest: the piano first, then the rest in no particular order.

## 1. A sampled grand piano (Salamander) — decided, first

**Why.** The piano is what gets played most in a band. The free pianos that sound good
enough either have no host-visible controls (OT P1ANO S: only Bypass, and a clunky UI) or
aren't free (Pianoteq). OT P1ANO S's sound is Salamander anyway (its README: "built using
Salamander Piano sample library by Alexander Holm"), so we can rebuild the concept from the
same recordings, in our own code, with our own panel.

**Not a port.** OT P1ANO S and its successor Creatools Piano XS are GPL-3 (HISE projects).
Porting their code would make ours GPL-3 as well, which doesn't match the project's MIT
licence. Most of their sound comes from HISE's sampler and modulators anyway, so a port would
mean rewriting the engine regardless. We use them as a reference for the concept and the
sound (listen side by side), not as code.

**Samples: Salamander Grand Piano** (v3) by Alexander Holm. A Yamaha C5, two AKG C414s, 48 kHz
24-bit, 16 velocity layers. **Licence: CC-BY 3.0**, so credit Alexander Holm (in the app's
credits and next to the samples). As far as known (to verify against the download): sampled
every minor third, with release samples, string-resonance/harmonics samples and pedal noises.
OT P1ANO S builds its layers from those: piano (three variants: normal, "eco", soft),
resonances, noises, pedals.

**Delivery.** Hundreds of MB to over a GB, depending on the version and format. They aren't in
the repo or compiled in: the app downloads them on request into
`<data dir>/live-keys/samples/salamander/` (checksummed), through the install window (see
`plugins.md`, "Installing recommended plugins and samples"). A `.deb` of the samples (in our own
APT repository) could come later. The standalone plugin build looks in the same folder.

**Engine: our own SFZ engine in Rust** (decided 2026-11-16; `crates/instruments/sfz`,
`live-keys-sfz`). Not only the piano's: the base for every sampled instrument here (the
e-pianos, the sampler library of section 4), and for instruments that mix samples with
modelled sound (see "Sampled and modelled together" below). Plain, testable, MIT, no FFI.
sfizz (BSD-2, C++ via FFI) was the alternative; reading SFZ ourselves keeps the panel's
controls a plain mapping onto SFZ controllers and the core free of a C++ build.

First step, done (2026-11-16): the parser and player, enough for the whole Salamander SFZ
(sfzinstruments' version, see below) — `#define`/`#include`, the header hierarchy, curves,
key/velocity/controller/random/round-robin conditions, keyswitches, release triggers with
`rt_decay`, controller-triggered pedal noises, `off_by` and (note) polyphony, velocity curves
and crossfades, controller modulation, the amplitude envelope, sustain pedal. Loads in ~1.4 s
(641 FLACs decoded on all cores), plays at ~85× real time with 60+ voices. Two examples:
offline render to WAV, and live play from MIDI. Details in CLAUDE.md.

Next steps, roughly in order:

- **Streaming from disk** (done, 2026-11-16): each sample's first 8192 frames stay in memory
  (58 MB for Salamander), and a voice starting on a sample has loader threads decode the whole
  file into memory, readable as it's decoded, freed 5 s after its last voice. No underruns
  rendering at real-time pace, even with heads of 64 frames (on an NVMe disk); peak ~190 MB
  for the test passage. Details in CLAUDE.md. Later, if disks turn out slower: decode only
  what's ahead of each voice instead of whole files.
- **Filters, modulation envelopes, LFOs** (done, 2026-11-16): reusable building blocks in
  `crates/instruments/dsp` (`live-keys-dsp`) — state-variable filters in every mode, LFOs,
  DAHDSR envelopes — for the SFZ engine now and our own synthesis later. The SFZ engine uses
  them for `fil_type`/`cutoff`/`resonance` with tracking, `fileg_*`/`pitcheg_*` and SFZ v1's
  `amplfo_*`/`fillfo_*`/`pitchlfo_*`, all controllable by CCs (Brightness can be `cutoff_onccN`
  on a gentle low-pass or shelf).
- **The built-in front-end** (done, 2026-11-16): `PluginFormat::Builtin` (`crates/builtin`),
  the piano as `builtin:piano` ("Grand Piano") in the plugin picker, with a panel profile.
  Each control drives one SFZ controller of our SFZ (Salamander's own: string resonance, key
  noise, pedal noise, release, velocity tracking; ours: brightness, tune, width) or the
  natural/retuned keyswitch. Five shipped presets; user presets and state files in the
  core's own text format (`symbol = value`). Next: the standalone build (truce), which should
  read the same state files.
- **Shipping the samples** (done, 2026-11-22): picking the piano without its samples offers
  to download them (GitHub's tarball of sfzinstruments' repository at a pinned commit, every
  file checked against SHA-256s compiled in), with progress, then loads it. About 25 s on a
  fast connection. A samples `.deb` in our APT repository was ruled out: GitHub Pages refuses
  files over 100 MB.
- SFZ v2's numbered LFOs and envelopes (`lfoN_*`, `egN_*`) if an instrument needs them; loop
  points read from the file (`smpl` chunk) for instruments that loop; OGG samples.

**Sampled and modelled together.** SFZ's generators (`sample=*sine`, `*saw`, `*noise`) already
play through the same regions, envelopes and controllers as samples, so a layer can be
synthesised next to sampled ones. That's the hook for modelled sound: our own generators
(a tine, a reed, sympathetic resonance) with per-voice state, chosen per region like a sample.
The e-piano could be a sampled attack with a modelled sustain; the piano's sympathetic
resonance could be modelled instead of sampled.

**Panel (built, 2026-11-16):** Touch (dynamics), Release, Key noise, Width and a
Natural/Retuned lever on top; Brightness, String resonance, Pedal noise and Tune below. No
reverb or EQ of its own: those are the slot's effects. The first ideas were:

**Panel (first ideas, for the slot's first tab).** No drawbars, so up to ten controls:

- Dynamics (velocity curve), Brightness (tone), Release (release-sample level/length),
  Resonance (sympathetic strings), Pedal noise, Key noise (key-off), Width (stereo), Reverb;
- a character choice (a `lever` or stepped knob): Normal / Soft / Bright, after OT P1ANO S's
  three piano variants;
- Tune / stretch tuning, Polyphony (in the pool, not shown by default).

Presets ship in the core's own state format (so the standalone build sees them, see Part A).

**Samples, as found.** Development uses sfzinstruments' version (716 MB of FLAC, 48 kHz
24-bit stereo, kept in `~/.local/share/live-keys/samples/salamander/`): 30 notes (every minor
third) × 16 velocity layers, 88 key-release noises, 30 release string-resonance (harmonics)
samples, pedal-down/up noises with two variations each, and two tunings behind a keyswitch
(natural, retuned).

**Open questions.** Whether to stay on sfzinstruments' remap or move to Accurate-Salamander
(also CC-BY), and how close the sound should be to OT P1ANO S's
(it adds EQ, compression and convolution reverb).

## 2. A modelled electric piano

Rhodes (tine), Wurlitzer (reed) and a Yamaha CP (CP-70/80: an electric grand with strings and
pickups), as one instrument with a model switch, or as three. **Modelled**, not sampled: small,
expressive, and nothing to download. Panel: the model, then tone, bell/bark, drive, tremolo and
auto-pan, and maybe a built-in chorus/phaser.

The CP is the hardest to model (real strings), so it could start as the odd one out.

References to study (check each one's licence before reading code): OpenWurli (a physically
modelled Wurlitzer 200A, CLAP/VST3), mda EPiano (sampled, already has a profile).

## 3. A quick pad synth

A pad that sounds good from the first preset, with a few big controls instead of a synth's
full set: e.g. Brightness, Movement, Width, Attack, Release, Shimmer/Reverb. A PADsynth-style
engine (Nasca's algorithm: wide-spectrum wavetables, lush without needing a chorus) fits.
Presets as starting points (warm, airy, dark, string-ish).

## 4. Other sampled instruments

Orchestral sounds (strings, brass, woodwinds, choir) and others, built from freely licensed
SFZ or SF2 sets. Candidates to check one by one (licence, quality, size):

- **VSCO 2 Community Edition** (orchestral, believed CC0);
- **Sonatina Symphonic Orchestra** (orchestral, Creative Commons Sampling Plus — check
  whether that allows redistribution in our case);
- **sfzinstruments.github.io** and **FreePats** (various, licence per instrument);
- Karoryfer and similar (various licences).

Only **CC0 / CC-BY** (or similar, without NonCommercial or ShareAlike strings attached to our
code) sets get shipped. Delivered like the piano's samples: downloaded on request.

This could be one built-in **sampler instrument** with a curated library (a choice of sound,
then a few controls per sound), sharing the piano's sampler core, rather than one plugin per
sound. The engine question from the piano decides this too. Note: FluidSynth, the usual SF2
player, is LGPL-2.1, which `cargo deny` would refuse; SF2 files would be converted, or read by
our own core.

## Sources

- OT P1ANO S: [repository](https://github.com/ilirbajri/ORCHESTOOLS-PIANO-S) (GPL-3; samples
  from Salamander), [HISE forum post](https://forum.hise.audio/post/28100); Creatools Piano XS:
  [SourceForge](https://sourceforge.net/projects/creatools-piano-xs/) (GPL-3)
- Salamander: [sfzinstruments page](https://sfzinstruments.github.io/pianos/salamander),
  [repository](https://github.com/sfzinstruments/SalamanderGrandPiano) (CC-BY 3.0),
  [FreePats](https://freepats.zenvoid.org/Piano/acoustic-grand-piano.html),
  [Accurate-Salamander](https://www.ir.isas.jaxa.jp/~cyamauch/AccurateSalamander/)
- Modelled pianos: [OpenWurli](https://github.com/hal0zer0/openwurli),
  [FigBug/Piano](https://github.com/figbug/piano)
