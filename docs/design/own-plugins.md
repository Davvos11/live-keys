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

**Engine (open).** Two options:

- **Our own sampler core in Rust** (`crates/instruments/piano`). Only the features a piano
  needs: pitch-shift between sampled notes (at most ±1 semitone for minor thirds), velocity
  layers with crossfades, release samples, the sustain pedal (half-pedal later), resonance and
  pedal noise layers. Plain, testable, MIT, no FFI.
- **sfizz's C library** (BSD-2, passes `cargo deny`) with a Salamander SFZ. Proven sample
  streaming, but FFI, a C++ build, and the panel's controls would have to map onto SFZ CCs.

Either way: **stream from disk**, keeping only each sample's start in memory (like sfizz and
HISE do). Fully decoded, the whole set takes several GB of RAM.

**Panel (first ideas, for the slot's first tab).** No drawbars, so up to ten controls:

- Dynamics (velocity curve), Brightness (tone), Release (release-sample level/length),
  Resonance (sympathetic strings), Pedal noise, Key noise (key-off), Width (stereo), Reverb;
- a character choice (a `lever` or stepped knob): Normal / Soft / Bright, after OT P1ANO S's
  three piano variants;
- Tune / stretch tuning, Polyphony (in the pool, not shown by default).

Presets ship in the core's own state format (so the standalone build sees them, see Part A).

**Open questions.** Which Salamander release to start from (the original v3, sfzinstruments'
remap, or Accurate-Salamander, also CC-BY), and how close the sound should be to OT P1ANO S's
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
