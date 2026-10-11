//! A quick end-to-end check of every class of plugin this host knows a quirk of
//! (see `cases.rs`): for each, load it into a live engine, play a note and check
//! it's heard, change a parameter and read it back, open its GUI, resize it and
//! check the plugin's widget follows, then close the GUI and unload the plugin
//! right away. Every case runs in a child process of its own, so a plugin that
//! crashes or hangs fails its case instead of the run.
//!
//! `cargo run -p live-keys-plugin-check --release -- [case name filter]...`
//! (needs an X display, an audio output and a MIDI input port; plugins that
//! aren't installed are skipped). `--list` lists the cases, `--info <plugin id>`
//! prints what a plugin offers, for writing a new case.

mod cases;
mod x11;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use live_keys_engine::{Engine, MidiEvent, PluginEditor, PluginKey, Section};
use live_keys_lv2_ui::{LaunchUiRequest, PluginUiHandle, PortMessage, UiKind};
use live_keys_vst3_ui::Vst3Editor;

use cases::{CASES, Case, Resize, Role, Target};

/// How long one case may take before it counts as hung.
const CASE_TIMEOUT: Duration = Duration::from_secs(120);
/// Quietest peak that counts as hearing something (-80 dBFS).
const AUDIBLE: f32 = 1e-4;
/// How long a GUI gets to open and settle, and to follow a resize.
const GUI_SETTLE: Duration = Duration::from_secs(3);
/// The instrument in front of an effect under test.
const SOURCE: &str = "http://drobilla.net/plugins/mda/EPiano";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--case") => {
            let name = args.get(1).context("--case needs a name")?;
            let case = CASES.iter().find(|case| case.name == name).context("no such case")?;
            run_case(case)
        }
        Some("--info") => info(args.get(1).context("--info needs a plugin id")?),
        Some("--list") => {
            for case in CASES {
                println!("{:<18} {}\n{:<18} {}", case.name, case.plugin, "", case.quirks);
            }
            Ok(())
        }
        _ => run_all(&args),
    }
}

// ---- The parent: runs each case in a child process and sums up. ----

fn run_all(filters: &[String]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let (mut passed, mut failed, mut skipped) = (0, 0, 0);
    let mut failures = Vec::new();
    for case in CASES.iter().filter(|case| filters.is_empty() || filters.iter().any(|f| case.name.contains(f.as_str()))) {
        println!("\n{} — {}\n  {}", case.name, case.plugin, case.quirks);
        let outcome = run_child(&exe, case)?;
        match outcome {
            Outcome::Passed => passed += 1,
            Outcome::Skipped => skipped += 1,
            Outcome::Failed => {
                failed += 1;
                failures.push(case.name);
            }
        }
    }
    println!("\n{passed} passed, {failed} failed, {skipped} skipped");
    if !failures.is_empty() {
        println!("failed: {}", failures.join(", "));
        std::process::exit(1);
    }
    Ok(())
}

enum Outcome {
    Passed,
    Failed,
    Skipped,
}

fn run_child(exe: &std::path::Path, case: &Case) -> Result<Outcome> {
    let mut child = Command::new(exe)
        .args(["--case", case.name])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (line_tx, line_rx) = mpsc::channel::<String>();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    std::thread::spawn(move || {
        for line in stdout.lines().map_while(Result::ok) {
            let _ = line_tx.send(line);
        }
    });
    let stderr = BufReader::new(child.stderr.take().unwrap());
    let stderr_lines = std::thread::spawn(move || stderr.lines().map_while(Result::ok).collect::<Vec<_>>());

    let started = Instant::now();
    let (mut any_fail, mut any_pass, mut last_step) = (false, false, String::from("start"));
    let show = |line: String, any_fail: &mut bool, any_pass: &mut bool, last_step: &mut String| {
        let mut parts = line.splitn(4, '\t');
        if parts.next() != Some("STEP") {
            return;
        }
        let (step, status, detail) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        *any_fail |= status == "FAIL";
        *any_pass |= status == "PASS";
        *last_step = step.to_string();
        println!("  {status:<4}  {step:<7} {detail}");
        let _ = std::io::stdout().flush();
    };
    let status = loop {
        while let Ok(line) = line_rx.try_recv() {
            show(line, &mut any_fail, &mut any_pass, &mut last_step);
        }
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if started.elapsed() > CASE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // The reader thread may still hold the last lines.
    std::thread::sleep(Duration::from_millis(50));
    while let Ok(line) = line_rx.try_recv() {
        show(line, &mut any_fail, &mut any_pass, &mut last_step);
    }
    let crashed = match status {
        None => Some(format!("hung after \"{last_step}\" (killed after {CASE_TIMEOUT:?})")),
        Some(status) if status.success() => None,
        Some(status) => {
            use std::os::unix::process::ExitStatusExt;
            Some(match status.signal() {
                Some(signal) => format!("crashed after \"{last_step}\" (signal {signal})"),
                None => format!("exited with {status} after \"{last_step}\""),
            })
        }
    };
    if let Some(crash) = &crashed {
        println!("  FAIL  process {crash}");
    }
    // What the plugin (or the host's log) complained about, even in a passing
    // case: a warning is often the first sign of a quirk.
    let mut stderr = stderr_lines.join().unwrap_or_default();
    // LSP plugins log their font cache statistics.
    stderr.retain(|line| !line.starts_with("[INF]") && !line.starts_with("[DBG]"));
    stderr.dedup();
    let shown = if any_fail || crashed.is_some() { 12 } else { 6 };
    for line in &stderr[stderr.len().saturating_sub(shown)..] {
        println!("        | {line}");
    }
    if any_fail || crashed.is_some() {
        return Ok(Outcome::Failed);
    }
    Ok(if any_pass { Outcome::Passed } else { Outcome::Skipped })
}

// ---- The child: one case. ----

fn report(step: &str, status: &str, detail: impl std::fmt::Display) {
    println!("STEP\t{step}\t{status}\t{detail}");
    let _ = std::io::stdout().flush();
}

fn report_result(step: &str, result: Result<String>) {
    match result {
        Ok(detail) => report(step, "PASS", detail),
        Err(err) => report(step, "FAIL", format!("{err:#}")),
    }
}

fn run_case(case: &Case) -> Result<()> {
    let mut engine = Engine::new(8192);
    if !engine.list_plugins().iter().any(|plugin| plugin.id == case.plugin) {
        report("load", "SKIP", "not installed");
        return Ok(());
    }
    engine.start(None, None)?;

    let started = Instant::now();
    let key = match load(&mut engine, case) {
        Ok(key) => key,
        Err(err) => {
            report("load", "FAIL", format!("{err:#}"));
            return Ok(());
        }
    };
    report("load", "PASS", format!("{} ms", started.elapsed().as_millis()));
    std::thread::sleep(case.warmup);

    let peak = play_note(&mut engine, 60);
    match (case.sound, peak > AUDIBLE) {
        (true, true) => report("note", "PASS", format!("peak {}", db(peak))),
        (true, false) => report("note", "FAIL", format!("silent (peak {})", db(peak))),
        (false, _) => report("note", "SKIP", format!("silent by design (peak {})", db(peak))),
    }

    match check_param(&mut engine, key, case, peak) {
        Ok(detail) => report("param", "PASS", detail),
        Err(err) => match err.downcast::<SkipError>() {
            Ok(skip) => report("param", "SKIP", skip.0),
            Err(err) => report("param", "FAIL", format!("{err:#}")),
        },
    }

    check_gui(&mut engine, key, case);
    report("exit", "PASS", "dropping the engine");
    drop(engine);
    Ok(())
}

/// A step that doesn't apply, as an error so it can short-circuit with `?`.
#[derive(Debug)]
struct SkipError(String);
impl std::fmt::Display for SkipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SkipError {}

fn skip(reason: impl Into<String>) -> anyhow::Error {
    SkipError(reason.into()).into()
}

fn load(engine: &mut Engine, case: &Case) -> Result<PluginKey> {
    match case.role {
        Role::Instrument => engine.load_instrument(0, case.plugin),
        Role::AudioEffect | Role::MidiEffect => {
            engine.load_instrument(0, SOURCE).context("loading the source instrument")?;
            let section = if matches!(case.role, Role::MidiEffect) { Section::Midi } else { Section::Audio };
            engine.add_effect(0, section, case.plugin)
        }
    }
}

fn unload(engine: &mut Engine, case: &Case) {
    match case.role {
        Role::Instrument => engine.unload_instrument(0),
        Role::AudioEffect => engine.remove_effect(0, Section::Audio, 0),
        Role::MidiEffect => engine.remove_effect(0, Section::Midi, 0),
    }
}

/// Plays `note` for half a second and returns slot 0's peak meanwhile.
fn play_note(engine: &mut Engine, note: u8) -> f32 {
    let midi = |bytes: [u8; 3]| MidiEvent::from_raw(0, &bytes).unwrap();
    engine.take_slot_peak(0);
    let _ = engine.send_midi(midi([0x90, note, 100]));
    std::thread::sleep(Duration::from_millis(500));
    let peak = engine.take_slot_peak(0);
    let _ = engine.send_midi(midi([0x80, note, 0]));
    // Let it ring out, so the next measurement starts quiet.
    std::thread::sleep(Duration::from_millis(700));
    engine.collect_retired();
    peak
}

fn db(peak: f32) -> String {
    if peak <= 0.0 { "-inf dB".into() } else { format!("{:.1} dB", 20.0 * peak.log10()) }
}

fn check_param(engine: &mut Engine, key: PluginKey, case: &Case, peak_before: f32) -> Result<String> {
    let Some(check) = &case.param else {
        return Err(skip("no parameter to check"));
    };
    let meta = engine.plugin(key).context("plugin gone")?;
    let param = meta
        .params
        .iter()
        .find(|param| param.name.eq_ignore_ascii_case(check.name))
        .with_context(|| format!("no parameter named \"{}\" (has {})", check.name, meta.params.len()))?
        .clone();
    let value = param.snap(match check.target {
        Target::Min => param.min_value,
        Target::Max => param.max_value,
    });
    engine.set_param(key, param.id, value);
    let recorded = engine.param_value(key, param.id).context("no recorded value")?;
    if recorded != value {
        bail!("recorded {recorded}, sent {value}");
    }
    let mut detail = format!("\"{}\" = {}", param.name, engine.param_text(key, param.id, value));
    // A VST3 controller can be asked; an LV2 plugin's input ports are ours.
    if let Some(PluginEditor::Vst3(controller)) = engine.plugin(key).map(|meta| &meta.editor) {
        let read = controller.param_normalized(param.id);
        if (read - value).abs() > 1e-3 {
            bail!("controller reads {read} after setting {value}");
        }
        detail += &format!(", controller reads {read:.3}");
    }
    if let Some(quieter_db) = check.quieter_db {
        // Give the audio thread the change before playing.
        std::thread::sleep(Duration::from_millis(50));
        let peak = play_note(engine, 60);
        if peak > peak_before * 10f32.powf(-quieter_db / 20.0) {
            bail!("{detail}, but a note still peaks at {} (was {}, expected {quieter_db} dB less)", db(peak), db(peak_before));
        }
        detail += &format!(", a note now peaks at {}", db(peak));
        engine.set_param(key, param.id, param.default_value);
    }
    Ok(detail)
}

enum Editor {
    Lv2(PluginUiHandle),
    Vst3(Vst3Editor),
}

impl Editor {
    fn is_closed(&self) -> bool {
        match self {
            Editor::Lv2(handle) => handle.is_closed(),
            Editor::Vst3(editor) => editor.is_closed(),
        }
    }

    fn window(&self) -> Option<u32> {
        match self {
            Editor::Lv2(handle) => handle.window(),
            Editor::Vst3(editor) => Some(editor.window()),
        }
    }
}

enum Edit {
    Param(u32, f32),
    Atom(u32, Vec<u8>),
}

#[derive(Default)]
struct Traffic {
    to_dsp: usize,
    to_gui: usize,
}

fn open_editor(engine: &Engine, key: PluginKey, tx: mpsc::Sender<Edit>) -> Result<(Editor, &'static str)> {
    let meta = engine.plugin(key).context("plugin gone")?;
    match &meta.editor {
        PluginEditor::Lv2 { plugin, features, raw_handle, gui_leases, .. } => {
            let kind = match live_keys_lv2_ui::best_supported_ui(plugin) {
                Some((_, UiKind::X11Ui)) => "LV2 X11 UI",
                Some((_, UiKind::ExternalUi)) => "LV2 External UI",
                None => return Err(skip("no GUI this host can show")),
            };
            let handle = live_keys_lv2_ui::launch_ui(LaunchUiRequest {
                plugin,
                features: features.clone(),
                instance_handle: *raw_handle,
                port_write: Box::new(move |message| {
                    let _ = tx.send(match message {
                        PortMessage::Control { port, value } => Edit::Param(port, value),
                        PortMessage::Atom { port, atom } => Edit::Atom(port, atom),
                    });
                }),
                keep_alive: gui_leases.lease().map(|lease| Box::new(lease) as Box<dyn Send>),
            })?;
            // As the app does: a new LV2 GUI is told every control value.
            for param in &meta.params {
                let value = engine.param_value(key, param.id).unwrap_or(param.default_value);
                handle.notify(PortMessage::Control { port: param.id, value });
            }
            Ok((Editor::Lv2(handle), kind))
        }
        PluginEditor::Vst3(controller) => {
            let tx = std::sync::Mutex::new(tx);
            let editor = Vst3Editor::open(
                controller,
                &meta.name,
                Box::new(move |id, value| {
                    let _ = tx.lock().unwrap().send(Edit::Param(id, value));
                }),
            )?;
            Ok((Editor::Vst3(editor), "VST3 editor"))
        }
    }
}

/// Runs the GUI's side of things for `duration`, as the app's poll timer does.
fn pump(engine: &mut Engine, key: PluginKey, editor: &mut Editor, rx: &mpsc::Receiver<Edit>, traffic: &mut Traffic, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        if let Editor::Vst3(editor) = editor {
            editor.idle();
        }
        while let Ok(edit) = rx.try_recv() {
            traffic.to_dsp += 1;
            match edit {
                Edit::Param(id, value) => engine.send_param_change(key, id, value),
                Edit::Atom(port, atom) => engine.send_ui_atom(key, port, &atom),
            }
        }
        engine.drain_ui_events(|from, port, event| {
            if from == key
                && let Editor::Lv2(handle) = editor
            {
                traffic.to_gui += 1;
                handle.notify(PortMessage::Atom { port, atom: event.to_atom() });
            }
        });
        engine.collect_retired();
        std::thread::sleep(Duration::from_millis(16));
    }
}

fn check_gui(engine: &mut Engine, key: PluginKey, case: &Case) {
    let (tx, rx) = mpsc::channel();
    let (mut editor, kind) = match open_editor(engine, key, tx) {
        Ok(opened) => opened,
        Err(err) => {
            // VST3 says "no editor" only by failing to create one.
            match (case.resize, err.downcast::<SkipError>()) {
                (Resize::NoGui, Ok(skip)) => report("gui", "SKIP", skip.0),
                (Resize::NoGui, Err(err)) => report("gui", "SKIP", format!("{err:#}")),
                (_, Ok(skip)) => report("gui", "FAIL", format!("expected a GUI: {}", skip.0)),
                (_, Err(err)) => report("gui", "FAIL", format!("{err:#}")),
            }
            unload_checked(engine, case);
            return;
        }
    };
    let mut traffic = Traffic::default();
    pump(engine, key, &mut editor, &rx, &mut traffic, GUI_SETTLE);
    let x = x11::Display::connect();

    let opened = (|| -> Result<String> {
        if editor.is_closed() {
            bail!("{kind} closed by itself");
        }
        let Some(window) = editor.window() else {
            return Ok(format!("{kind}, its own window"));
        };
        let x = x.as_ref().map_err(|err| anyhow::anyhow!("{err:#}"))?;
        let (width, height) = x.size(window)?;
        let colours = x.colours(window)?;
        if colours <= 2 {
            bail!("{kind} {width}×{height} is blank ({colours} colours)");
        }
        Ok(format!("{kind} {width}×{height}, {colours}+ colours"))
    })();
    report_result("gui", opened);

    let resized = (|| -> Result<String> {
        let Some(window) = editor.window() else {
            return Err(skip("the plugin owns its window"));
        };
        let x = x.as_ref().map_err(|err| anyhow::anyhow!("{err:#}"))?;
        let before = x.size(window)?;
        let widget = x.child(window)?.context("nothing embedded in the window")?;
        let widget_before = x.size(widget)?;
        let asked = (before.0 * 5 / 4, before.1 * 5 / 4);
        x.resize(window, asked)?;
        pump(engine, key, &mut editor, &rx, &mut traffic, GUI_SETTLE);
        if editor.is_closed() {
            bail!("closed after resizing");
        }
        let after = x.size(window)?;
        let widget_after = x.size(widget)?;
        let sizes = format!(
            "window {}×{} → {}×{} (asked {}×{}), widget {}×{} → {}×{}",
            before.0, before.1, after.0, after.1, asked.0, asked.1, widget_before.0, widget_before.1, widget_after.0, widget_after.1
        );
        match case.resize {
            Resize::Follows if after != before && widget_after == after => {}
            Resize::Follows => bail!("the widget should fill the resized window: {sizes}"),
            Resize::Fixed if widget_after == widget_before => {}
            Resize::Fixed => bail!("a fixed-size GUI changed size: {sizes}"),
            Resize::NotByHost | Resize::NoGui => bail!("expected no host-resizable window: {sizes}"),
        }
        let colours = x.colours(window)?;
        if colours <= 2 {
            bail!("blank after resizing ({colours} colours): {sizes}");
        }
        Ok(sizes)
    })();
    match resized {
        Ok(detail) => report("resize", "PASS", detail),
        Err(err) => match err.downcast::<SkipError>() {
            Ok(skip) if case.resize == Resize::NotByHost => report("resize", "SKIP", skip.0),
            Ok(skip) => report("resize", "FAIL", skip.0),
            Err(err) => report("resize", "FAIL", format!("{err:#}")),
        },
    }

    let counts = format!("{} messages GUI → plugin, {} plugin → GUI", traffic.to_dsp, traffic.to_gui);
    match (case.atoms, traffic.to_dsp > 0 && traffic.to_gui > 0) {
        (true, true) => report("atoms", "PASS", counts),
        (true, false) => report("atoms", "FAIL", counts),
        (false, _) => report("atoms", "SKIP", counts),
    }

    // Close the GUI and unload the plugin at once: the plugin must outlive the
    // GUI's teardown (Surge XT's reads it while closing).
    match editor {
        Editor::Lv2(handle) => drop(handle),
        Editor::Vst3(mut editor) => editor.close(),
    }
    unload_checked(engine, case);
}

/// Unloads the plugin under test and reports how long dropping it took — a
/// GUI that never finishes closing makes it wait out the timeout and leak.
fn unload_checked(engine: &mut Engine, case: &Case) {
    let started = Instant::now();
    unload(engine, case);
    // The audio thread retires it on its next callback.
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(20));
        engine.collect_retired();
    }
    let took = started.elapsed().saturating_sub(Duration::from_secs(1));
    if took > Duration::from_millis(2500) {
        report("unload", "FAIL", format!("waited {} ms for the GUI to close", took.as_millis()));
    } else {
        report("unload", "PASS", format!("dropped within {} ms of closing", took.as_millis()));
    }
}

// ---- --info ----

fn info(id: &str) -> Result<()> {
    let mut engine = Engine::new(8192);
    let plugin = engine.list_plugins().into_iter().find(|plugin| plugin.id == id).context("not installed")?;
    println!("{} ({:?}, {:?})", plugin.name, plugin.format, plugin.kind);
    engine.start(None, None)?;
    let key = match engine.section_for(id) {
        _ if plugin.kind == live_keys_engine::PluginKind::Instrument => engine.load_instrument(0, id)?,
        section => engine.add_effect(0, section, id)?,
    };
    let meta = engine.plugin(key).unwrap();
    if let PluginEditor::Lv2 { plugin, .. } = &meta.editor {
        println!("GUI: {:?}", live_keys_lv2_ui::best_supported_ui(plugin).map(|(_, kind)| kind));
    }
    for param in &meta.params {
        println!(
            "  {:>5} {:<32} {:?} {}..{} (default {})",
            param.id, param.name, param.kind, param.min_value, param.max_value, param.default_value
        );
    }
    Ok(())
}
