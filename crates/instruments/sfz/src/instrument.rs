//! A loaded SFZ instrument: its regions, samples and curves. Immutable once
//! loaded, and shared (`Arc`) by the voices that play it.

use crate::curve::Curve;
use crate::parse::{self, Section};
use crate::region::{Applied, Generator, ParseContext, Region, Source, Trigger, float, int, split_number};
use crate::sample::Sample;
use anyhow::{Context, Result};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Controllers tracked: MIDI's 128 plus room for SFZ's extended ones.
pub const CC_COUNT: usize = 256;

pub struct Instrument {
    pub(crate) regions: Vec<Region>,
    pub(crate) samples: Vec<Sample>,
    /// By curve index; built-ins at 0..=6.
    pub(crate) curves: Vec<Option<Curve>>,
    /// Per region, its `amp_velcurve_N` points as a curve (`None`: velocity²).
    pub(crate) velcurves: Vec<Option<Curve>>,
    /// Controller values (normalized) before any MIDI arrives: MIDI's usual
    /// defaults (volume 100, pan and expression centred/full), then `set_ccN`.
    pub(crate) initial_cc: [f32; CC_COUNT],
    /// Per key, the regions a note-on can start (attack, first, legato).
    pub(crate) attack_by_key: Vec<Vec<u32>>,
    /// Per key, the regions a note-off can start.
    pub(crate) release_by_key: Vec<Vec<u32>>,
    /// Regions started by a controller (`on_loccN`).
    pub(crate) cc_triggered: Vec<u32>,
    /// The keyswitch range, if any region has one, and the keyswitch to start with.
    pub(crate) keyswitch_range: Option<(u8, u8)>,
    pub(crate) keyswitch_default: Option<u8>,
    cc_labels: Vec<(u16, String)>,
}

impl Instrument {
    /// Loads an `.sfz` file and every sample it uses (in parallel). Regions whose
    /// sample can't be loaded are left out, with a warning.
    pub fn load(path: &Path) -> Result<Instrument> {
        let sections = parse::parse_file(path)?;
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Self::build(&sections, Some(&base))
    }

    /// From SFZ text; only generators (`sample=*sine`) work without a `base`
    /// folder to find sample files in.
    pub fn from_sfz(text: &str, base: Option<&Path>) -> Result<Instrument> {
        let sections = parse::parse_str(text, base)?;
        Self::build(&sections, base)
    }

    pub fn region_count(&self) -> usize {
        self.regions.len()
    }

    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Bytes of sample data held in memory.
    pub fn sample_memory(&self) -> usize {
        self.samples.iter().map(Sample::memory).sum()
    }

    /// The instrument's names for its controllers (`label_ccN`).
    pub fn cc_labels(&self) -> &[(u16, String)] {
        &self.cc_labels
    }

    /// A controller's initial value, normalized.
    pub fn initial_cc(&self, cc: u16) -> f32 {
        self.initial_cc.get(cc as usize).copied().unwrap_or(0.0)
    }

    pub(crate) fn curve(&self, index: u16) -> &Curve {
        self.curves.get(index as usize).and_then(Option::as_ref).unwrap_or_else(|| self.curves[0].as_ref().unwrap())
    }

    fn build(sections: &[Section], base: Option<&Path>) -> Result<Instrument> {
        let mut regions = Vec::new();
        let mut curves: Vec<Option<Curve>> = (0..7).map(Curve::builtin).collect();
        let mut initial_cc = [0.0; CC_COUNT];
        initial_cc[7] = 100.0 / 127.0;
        initial_cc[10] = 0.5;
        initial_cc[11] = 1.0;
        let mut cc_labels = Vec::new();
        let mut ctx = ParseContext::default();
        let mut default_path = String::new();
        let mut unknown: BTreeSet<String> = BTreeSet::new();
        let mut bad: BTreeSet<String> = BTreeSet::new();

        // The opcodes in effect at each level of the hierarchy.
        let mut global: Vec<(String, String)> = Vec::new();
        let mut master: Vec<(String, String)> = Vec::new();
        let mut group: Vec<(String, String)> = Vec::new();

        for section in sections {
            match section.header.as_str() {
                "control" => {
                    for (name, value) in &section.opcodes {
                        let (base_name, number) = split_number(name);
                        match (base_name, number) {
                            ("default_path", None) => default_path = value.replace('\\', "/"),
                            ("note_offset", None) => ctx.note_offset += int(value).unwrap_or(0) as i32,
                            ("octave_offset", None) => ctx.note_offset += 12 * int(value).unwrap_or(0) as i32,
                            ("set_cc", Some(cc)) if (cc as usize) < CC_COUNT => {
                                initial_cc[cc as usize] = float(value).unwrap_or(0.0) / 127.0;
                            }
                            ("set_hdcc" | "set_realcc", Some(cc)) if (cc as usize) < CC_COUNT => {
                                initial_cc[cc as usize] = float(value).unwrap_or(0.0);
                            }
                            ("label_cc", Some(cc)) => cc_labels.push((cc, value.clone())),
                            _ => {
                                unknown.insert(name.clone());
                            }
                        }
                    }
                }
                "global" => {
                    global = section.opcodes.clone();
                    master.clear();
                    group.clear();
                }
                "master" => {
                    master = section.opcodes.clone();
                    group.clear();
                }
                "group" => group = section.opcodes.clone(),
                "region" => {
                    let mut region = Region { default_path: default_path.clone(), ..Region::default() };
                    for (name, value) in global.iter().chain(&master).chain(&group).chain(&section.opcodes) {
                        match region.apply(name, value, &ctx) {
                            Applied::Yes => {}
                            Applied::BadValue => {
                                bad.insert(format!("{name}={value}"));
                            }
                            Applied::Unknown => {
                                unknown.insert(name.clone());
                            }
                        }
                    }
                    if region.sample.is_empty() {
                        log::warn!("sfz: a region without a sample, left out");
                        continue;
                    }
                    regions.push(region);
                }
                "curve" => {
                    let mut index = None;
                    let mut points = Vec::new();
                    for (name, value) in &section.opcodes {
                        match split_number(name) {
                            ("curve_index", None) => index = int(value),
                            ("v", Some(i)) if i < 128 => {
                                if let Some(v) = float(value) {
                                    points.push((i as u8, v));
                                }
                            }
                            _ => {
                                unknown.insert(name.clone());
                            }
                        }
                    }
                    match index {
                        Some(i) if (0..1024).contains(&i) => {
                            let i = i as usize;
                            if curves.len() <= i {
                                curves.resize(i + 1, None);
                            }
                            curves[i] = Some(Curve::from_points(&points));
                        }
                        _ => log::warn!("sfz: a <curve> without a usable curve_index, left out"),
                    }
                }
                other => {
                    unknown.insert(format!("<{other}>"));
                }
            }
        }
        if !unknown.is_empty() {
            log::info!("sfz: ignored opcodes/headers: {}", unknown.into_iter().collect::<Vec<_>>().join(" "));
        }
        if !bad.is_empty() {
            log::warn!("sfz: unreadable values: {}", bad.into_iter().collect::<Vec<_>>().join(" "));
        }

        let samples = resolve_sources(&mut regions, base)?;
        regions.retain(|r| r.source != Source::None);
        let velcurves = regions
            .iter()
            .map(|r| (!r.amp_velcurve.is_empty()).then(|| Curve::from_points(&r.amp_velcurve)))
            .collect();

        let mut attack_by_key = vec![Vec::new(); 128];
        let mut release_by_key = vec![Vec::new(); 128];
        let mut cc_triggered = Vec::new();
        let mut keyswitch_range: Option<(u8, u8)> = None;
        let mut keyswitch_default = None;
        for (i, region) in regions.iter().enumerate() {
            let i = i as u32;
            if !region.cc_triggers.is_empty() {
                cc_triggered.push(i);
            } else if let Some((lo, hi)) = region.keys {
                let by_key = if region.trigger == Trigger::Release { &mut release_by_key } else { &mut attack_by_key };
                for key in lo..=hi {
                    by_key[key as usize].push(i);
                }
            }
            if let (Some(lo), Some(hi)) = (region.sw_lokey, region.sw_hikey) {
                keyswitch_range = Some(match keyswitch_range {
                    Some((l, h)) => (l.min(lo), h.max(hi)),
                    None => (lo, hi),
                });
            }
            keyswitch_default = keyswitch_default.or(region.sw_default);
        }

        Ok(Instrument {
            regions,
            samples,
            curves,
            velcurves,
            initial_cc,
            attack_by_key,
            release_by_key,
            cc_triggered,
            keyswitch_range,
            keyswitch_default,
            cc_labels,
        })
    }
}

/// Sets every region's source, loading each sample file once (in parallel).
fn resolve_sources(regions: &mut [Region], base: Option<&Path>) -> Result<Vec<Sample>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut index_of: HashMap<PathBuf, u32> = HashMap::new();
    let mut region_paths: Vec<Option<u32>> = Vec::with_capacity(regions.len());
    for region in regions.iter_mut() {
        if let Some(generator) = Generator::from_name(region.sample.trim()) {
            region.source = Source::Generator(generator);
            region_paths.push(None);
            continue;
        }
        let Some(base) = base else {
            log::warn!("sfz: sample {} without a folder to find it in", region.sample);
            region_paths.push(None);
            continue;
        };
        let relative = format!("{}{}", region.default_path, region.sample).replace('\\', "/");
        let path = base.join(relative);
        let index = *index_of.entry(path.clone()).or_insert_with(|| {
            paths.push(path);
            (paths.len() - 1) as u32
        });
        region_paths.push(Some(index));
    }

    let loaded = load_parallel(&paths);
    let mut samples = Vec::with_capacity(paths.len());
    // Index in `paths` → index in `samples` (failed loads are left out).
    let mut remap: Vec<Option<u32>> = Vec::with_capacity(paths.len());
    for result in loaded {
        match result {
            Ok(sample) => {
                remap.push(Some(samples.len() as u32));
                samples.push(sample);
            }
            Err(err) => {
                log::warn!("sfz: {err:#}");
                remap.push(None);
            }
        }
    }
    for (region, path_index) in regions.iter_mut().zip(region_paths) {
        if let Some(i) = path_index {
            region.source = remap[i as usize].map_or(Source::None, Source::Sample);
        }
    }
    if samples.is_empty() && !paths.is_empty() {
        return Err(anyhow::anyhow!("none of the instrument's {} samples could be loaded", paths.len()))
            .context("loading samples");
    }
    Ok(samples)
}

fn load_parallel(paths: &[PathBuf]) -> Vec<Result<Sample>> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(paths.len().max(1));
    let next = AtomicUsize::new(0);
    let mut results: Vec<Option<Result<Sample>>> = (0..paths.len()).map(|_| None).collect();
    let done: Vec<Vec<(usize, Result<Sample>)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut out = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = paths.get(i) else { break };
                        out.push((i, Sample::load(path)));
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("sample loading thread panicked")).collect()
    });
    for (i, result) in done.into_iter().flatten() {
        results[i] = Some(result);
    }
    results.into_iter().map(|r| r.expect("every sample was attempted")).collect()
}
