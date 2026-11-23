//! MIDI learn's mappings as logic only — no threads, no queues, time passed in —
//! so it can be tested: which targets a surface control's messages move, to
//! where, and how a control takes over a target that's somewhere else (see
//! "Modes" and "Takeover and feedback" in `docs/design/midi-learn.md`). The
//! mapping thread ([`crate::mapper`]) runs it.
//!
//! Everything is in **positions**, 0..1 along a target's control as the GUI
//! draws it; turning a position into a value is the caller's business.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use live_keys_core::MidiEvent;
use live_keys_surfaces::{ControlKind, Read, Reader, Reading, Shown, Surface};

/// A target, as an index into the setup's targets.
pub type TargetId = usize;

/// How near a control has to come to its target's position to pick it up, and
/// how far a target has to move away from a control for the control to lose it.
const TOLERANCE: f32 = 0.02;

/// How a fader or knob takes over a target whose position differs from its own
/// (after a preset load, a mouse edit…). Pads and buttons always jump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Takeover {
    /// The first move sets the target to the control's position.
    Jump,
    /// The first move starts the target sliding to the control's position
    /// over the control's glide time, following it as it moves.
    #[default]
    Glide,
    /// Moves are ignored until the control reaches the target (or passes it),
    /// then it follows.
    PickUp,
}

/// What a control does to one target.
#[derive(Debug, Clone, PartialEq)]
pub enum Mode {
    /// Faders, knobs, pads: the target's base goes to `from + p·(to − from)`
    /// for the control at `p`. Mirrored is `from > to`.
    Range { from: f32, to: f32 },
    /// Faders, knobs, pads: the control adds an offset of `low + p·(high −
    /// low)` to the target's base (a pad: while held, by its velocity and
    /// pressure).
    Modulate { low: f32, high: f32 },
    /// Buttons, pads: each press moves the target's base to the level after
    /// the one nearest it, wrapping.
    Toggle { levels: Vec<f32> },
    /// Buttons, pads: while held, the target is at `value` (times the pad's
    /// velocity if `velocity`), whatever its base; let go, it's back at it.
    Momentary { value: f32, velocity: bool },
    /// Buttons, pads: while held, `offset` (times the velocity if
    /// `velocity`) is added to the target's base.
    MomentaryModulate { offset: f32, velocity: bool },
}

impl Mode {
    /// Whether it adds an offset rather than setting the base.
    pub fn modulates(&self) -> bool {
        matches!(self, Mode::Modulate { .. } | Mode::MomentaryModulate { .. })
    }

    /// The offsets it can add, lowest and highest (0 for other modes).
    pub fn reach(&self) -> (f32, f32) {
        match *self {
            Mode::Modulate { low, high } => (low.min(high), low.max(high)),
            Mode::MomentaryModulate { offset, .. } => (offset.min(0.0), offset.max(0.0)),
            _ => (0.0, 0.0),
        }
    }
}

/// One of a control's targets.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub target: TargetId,
    pub mode: Mode,
    /// What a control with feedback shows of it is turned around: a
    /// position p as 1 − p, lit as dark (a mute button lit while the slot
    /// plays).
    pub invert_feedback: bool,
}

/// One surface control and its targets.
#[derive(Debug, Clone, PartialEq)]
pub struct ControlSetup {
    /// Index into the setup's surfaces.
    pub surface: usize,
    /// Index into that surface's controls.
    pub control: usize,
    pub takeover: Takeover,
    pub glide: Duration,
    /// Its messages also reach the slots (otherwise they're the mappings' alone).
    pub pass_through: bool,
    /// A pad in [`Mode::Range`] goes back to 0 when let go.
    pub release_to_zero: bool,
    /// It shows its first target's value sent back to it (motor faders, LED
    /// rings, lit buttons): it never has to take a target over.
    pub feedback: bool,
    pub links: Vec<Link>,
}

/// A value to send back to a control that shows it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Feedback {
    pub surface: usize,
    pub control: usize,
    pub shown: Shown,
    /// Send it even if it's what was sent last: the control may have
    /// changed its own light meanwhile.
    pub again: bool,
}

/// A target's new effective position (what it gets), and its base under it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Write {
    pub target: TargetId,
    pub position: f32,
    pub base: f32,
}

/// A fader or knob whose target is elsewhere than the hardware (`at`: the
/// target, as a position of the control), or no longer is (`None`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Waiting {
    pub surface: usize,
    pub control: usize,
    pub at: Option<f32>,
}

/// A control waiting to pick its target up was moved: the target is above it
/// (`up`) or below (for a device that can say so, see `Driver::pick_up`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PickUp {
    pub surface: usize,
    pub control: usize,
    pub up: bool,
}

#[derive(Debug, Default)]
struct LinkState {
    /// The target follows the control.
    engaged: bool,
    /// Sliding to the control since then, from this position.
    glide: Option<(Instant, f32)>,
    /// Where the control last wanted the target's base.
    desired: Option<f32>,
    /// What a modulating link adds now.
    offset: f32,
}

#[derive(Debug)]
struct ControlState {
    setup: ControlSetup,
    kind: ControlKind,
    /// The hardware's position, once it has sent one.
    hardware: Option<f32>,
    pressed: bool,
    links: Vec<LinkState>,
    /// As last reported.
    waiting: Option<f32>,
    /// What it was last sent (or is at, for a position it set itself).
    shown: Option<Shown>,
}

impl ControlState {
    fn jumps(&self) -> bool {
        matches!(self.kind, ControlKind::Pad | ControlKind::Button)
            || self.setup.takeover == Takeover::Jump
            || self.setup.feedback
    }
}

/// A target's layers (see "Values" in the design notes): the base the user
/// set, the offsets of modulating links and a momentary override on top,
/// and the effective position they make, which is what the target gets.
#[derive(Debug)]
struct TargetState {
    base: f32,
    /// Momentary links holding it, and the latest one's value.
    held: u32,
    hold: f32,
    effective: f32,
    /// The effective position and base last written.
    written: (f32, f32),
}

/// Every mapping of one setup, and their state.
pub struct Mappings {
    surfaces: Vec<Arc<Surface>>,
    /// Per surface, its page and shift.
    readers: Vec<Reader>,
    controls: Vec<ControlState>,
    /// `(surface, control)` → index into `controls`.
    by_control: HashMap<(usize, usize), usize>,
    targets: Vec<TargetState>,
    writes: Vec<Write>,
    waiting: Vec<Waiting>,
    pick_ups: Vec<PickUp>,
    feedback: Vec<Feedback>,
}

impl Mappings {
    /// `positions` are the targets' current positions; controls pointing at
    /// surfaces, controls or targets that don't exist are left out.
    pub fn new(surfaces: Vec<Arc<Surface>>, controls: Vec<ControlSetup>, positions: Vec<f32>) -> Self {
        let targets: Vec<TargetState> = positions
            .into_iter()
            .map(|position| {
                let position = position.clamp(0.0, 1.0);
                TargetState { base: position, held: 0, hold: 0.0, effective: position, written: (position, position) }
            })
            .collect();
        let mut by_control = HashMap::new();
        let mut states = Vec::new();
        for mut setup in controls {
            let Some(control) = surfaces.get(setup.surface).and_then(|surface| surface.controls.get(setup.control))
            else {
                continue;
            };
            setup.links.retain(|link| link.target < targets.len());
            if setup.links.is_empty() || by_control.contains_key(&(setup.surface, setup.control)) {
                continue;
            }
            by_control.insert((setup.surface, setup.control), states.len());
            states.push(ControlState {
                kind: control.kind,
                links: setup.links.iter().map(|_| LinkState::default()).collect(),
                setup,
                hardware: None,
                pressed: false,
                waiting: None,
                shown: None,
            });
        }
        Self {
            readers: surfaces.iter().map(|surface| Reader::new(surface)).collect(),
            surfaces,
            controls: states,
            by_control,
            targets,
            writes: Vec::new(),
            waiting: Vec::new(),
            pick_ups: Vec::new(),
            feedback: Vec::new(),
        }
    }

    /// Surface `surface`'s page and shift, to carry them over to new mappings.
    pub fn reader(&self, surface: usize) -> Option<&Reader> {
        self.readers.get(surface)
    }

    /// Carries surface `surface`'s page and shift over from old mappings.
    pub fn set_reader(&mut self, surface: usize, reader: Reader) {
        if let Some(slot) = self.readers.get_mut(surface) {
            *slot = reader;
        }
    }

    /// Applies a MIDI event from port `port` of surface `surface`; says what
    /// it was. A page change sends that page's controls (and those on no
    /// page) their values again: devices tend to clear their lights then.
    pub fn event(&mut self, surface: usize, port: u8, event: &MidiEvent, now: Instant) -> Read {
        let Some(device) = self.surfaces.get(surface).cloned() else { return Read::default() };
        let read = self.readers[surface].read(&device, port, event);
        if read.page_changed {
            let page = self.readers[surface].page();
            for index in 0..self.controls.len() {
                let setup = &self.controls[index].setup;
                let on_page = device.controls.get(setup.control).is_some_and(|control| control.page.is_none_or(|p| Some(p) == page));
                if setup.surface == surface && on_page {
                    self.controls[index].shown = None;
                    self.show(index, false);
                }
            }
        }
        if let Some((control, reading)) = read.control {
            if let Some(&index) = self.by_control.get(&(surface, control)) {
                self.reading(index, reading, now);
                // Moved while waiting to pick its target up: which way it is.
                let state = &self.controls[index];
                if let (Reading::Value(_), Takeover::PickUp, Some(at), Some(hardware)) =
                    (reading, state.setup.takeover, state.waiting, state.hardware)
                    && !state.jumps()
                {
                    self.pick_ups.push(PickUp { surface, control, up: at > hardware });
                }
            }
        } else if let Some((pads, pressure)) = read.pressure.clone() {
            // The held pads' pressure.
            for pad in pads {
                if let Some(&index) = self.by_control.get(&(surface, pad))
                    && self.controls[index].pressed
                {
                    self.reading(index, Reading::Value(pressure), now);
                }
            }
        }
        read
    }

    fn reading(&mut self, index: usize, reading: Reading, now: Instant) {
        let pad_or_button = matches!(self.controls[index].kind, ControlKind::Pad | ControlKind::Button);
        match reading {
            Reading::Value(value) => {
                // A pad's pressure counts only while it's held.
                if pad_or_button && !self.controls[index].pressed {
                    return;
                }
                // A control that shows values sending back what it was just
                // sent (a motor fader arriving) is its echo, not a move.
                let control = &mut self.controls[index];
                if let Some(Shown::Position(shown)) = control.shown
                    && control.setup.feedback
                    && (shown * 127.0).round() == (value * 127.0).round()
                {
                    control.hardware = Some(value);
                    return;
                }
                self.controls[index].hardware = Some(value);
                for link in 0..self.controls[index].links.len() {
                    match self.controls[index].setup.links[link].mode {
                        Mode::Range { .. } => self.range(index, link, value, now),
                        Mode::Modulate { low, high } => self.offset(index, link, low + value * (high - low)),
                        _ => {}
                    }
                }
            }
            Reading::Press(velocity) => {
                self.controls[index].pressed = true;
                self.controls[index].hardware = Some(velocity);
                for link in 0..self.controls[index].links.len() {
                    let target = self.controls[index].setup.links[link].target;
                    match self.controls[index].setup.links[link].mode.clone() {
                        Mode::Range { .. } => self.range(index, link, velocity, now),
                        Mode::Modulate { low, high } => self.offset(index, link, low + velocity * (high - low)),
                        Mode::Toggle { levels } => self.toggle(index, link, &levels),
                        Mode::Momentary { value, velocity: sensitive } => {
                            let state = &mut self.targets[target];
                            state.held += 1;
                            state.hold = if sensitive { value * velocity } else { value };
                            self.recompute(target);
                        }
                        Mode::MomentaryModulate { offset, velocity: sensitive } => {
                            self.offset(index, link, if sensitive { offset * velocity } else { offset });
                        }
                    }
                }
            }
            Reading::Release => {
                self.controls[index].pressed = false;
                for link in 0..self.controls[index].links.len() {
                    let target = self.controls[index].setup.links[link].target;
                    match self.controls[index].setup.links[link].mode {
                        Mode::Range { .. } if self.controls[index].setup.release_to_zero => {
                            self.controls[index].hardware = Some(0.0);
                            self.range(index, link, 0.0, now);
                        }
                        Mode::Modulate { .. } | Mode::MomentaryModulate { .. } => self.offset(index, link, 0.0),
                        Mode::Momentary { .. } => {
                            let state = &mut self.targets[target];
                            state.held = state.held.saturating_sub(1);
                            self.recompute(target);
                        }
                        _ => {}
                    }
                }
            }
        }
        self.update_waiting(index);
        self.show(index, true);
        // Pads and buttons light themselves while held and go dark when let
        // go (the LPD8 does): what they show goes to them again then.
        if reading == Reading::Release {
            self.show_again(index);
        }
    }

    /// Sends a pad or button what it shows, whether or not that changed.
    fn show_again(&mut self, index: usize) {
        if let Some(shown @ Shown::Lit(_)) = self.feedback_value(index) {
            let control = &mut self.controls[index];
            control.shown = Some(shown);
            self.feedback.push(Feedback {
                surface: control.setup.surface,
                control: control.setup.control,
                shown,
                again: true,
            });
        }
    }

    /// What control `index` should show, if it shows anything: its first
    /// target's base along its range, whether a toggle's target is on, or
    /// whether a momentary button is held.
    fn feedback_value(&self, index: usize) -> Option<Shown> {
        let control = &self.controls[index];
        if !control.setup.feedback {
            return None;
        }
        let link = control.setup.links.first()?;
        let base = self.targets[link.target].base;
        let shown = match link.mode {
            Mode::Range { from, to } => (to != from).then(|| Shown::Position(((base - from) / (to - from)).clamp(0.0, 1.0))),
            Mode::Toggle { .. } => Some(Shown::Lit(base > 1e-3)),
            Mode::Momentary { .. } | Mode::MomentaryModulate { .. } => Some(Shown::Lit(control.pressed)),
            // It's wherever it was turned to.
            Mode::Modulate { .. } => None,
        }?;
        Some(match shown {
            _ if !link.invert_feedback => shown,
            Shown::Position(position) => Shown::Position(1.0 - position),
            Shown::Lit(lit) => Shown::Lit(!lit),
        })
    }

    /// Sends control `index` its value if that changed. `own`: after its own
    /// move, a position it's at already isn't sent back (motor faders fight
    /// that), only noted.
    fn show(&mut self, index: usize, own: bool) {
        let Some(shown) = self.feedback_value(index) else { return };
        let control = &mut self.controls[index];
        if control.shown == Some(shown) {
            return;
        }
        control.shown = Some(shown);
        if !(own && matches!(shown, Shown::Position(_))) {
            self.feedback.push(Feedback {
                surface: control.setup.surface,
                control: control.setup.control,
                shown,
                again: false,
            });
        }
    }

    /// Sends the controls showing `target` (but `except`) its new value.
    fn show_target(&mut self, target: TargetId, except: Option<usize>) {
        for index in 0..self.controls.len() {
            if Some(index) != except && self.controls[index].setup.links.first().is_some_and(|link| link.target == target) {
                self.show(index, false);
            }
        }
    }

    /// Sends every control that shows values its value, changed or not (a
    /// device connected, a new setup).
    pub fn resync_feedback(&mut self) {
        for index in 0..self.controls.len() {
            self.controls[index].shown = None;
            self.show(index, false);
        }
    }

    /// The values to send back since the last call, oldest first.
    pub fn take_feedback(&mut self) -> Vec<Feedback> {
        std::mem::take(&mut self.feedback)
    }

    /// A modulating link now adds `offset`.
    fn offset(&mut self, index: usize, link: usize, offset: f32) {
        self.controls[index].links[link].offset = offset;
        let target = self.controls[index].setup.links[link].target;
        self.recompute(target);
    }

    /// The control at `hardware`, for its range link `link`.
    fn range(&mut self, index: usize, link: usize, hardware: f32, now: Instant) {
        let control = &self.controls[index];
        let Mode::Range { from, to } = control.setup.links[link].mode else { return };
        let target = control.setup.links[link].target;
        let jumps = control.jumps();
        let takeover = control.setup.takeover;
        let desired = from + hardware * (to - from);
        let current = self.targets[target].base;
        let state = &mut self.controls[index].links[link];
        let previous = state.desired.replace(desired);
        if state.engaged || jumps {
            state.engaged = true;
            state.glide = None;
            return self.set_base(target, desired, Some((index, link)));
        }
        match takeover {
            Takeover::Jump => {}
            Takeover::PickUp => {
                let crossed = previous.is_some_and(|previous| (previous - current) * (desired - current) <= 0.0);
                if crossed || (desired - current).abs() <= TOLERANCE {
                    state.engaged = true;
                    self.set_base(target, desired, Some((index, link)));
                }
            }
            Takeover::Glide => {
                state.glide.get_or_insert((now, current));
                self.glide(index, link, now);
            }
        }
    }

    /// Moves a gliding link's target on, to where it should be at `now`.
    fn glide(&mut self, index: usize, link: usize, now: Instant) {
        let length = self.controls[index].setup.glide;
        let target = self.controls[index].setup.links[link].target;
        let state = &mut self.controls[index].links[link];
        let (Some((start, from)), Some(desired)) = (state.glide, state.desired) else { return };
        let progress = if length.is_zero() { 1.0 } else { now.duration_since(start).as_secs_f32() / length.as_secs_f32() };
        if progress >= 1.0 {
            state.glide = None;
            state.engaged = true;
            self.set_base(target, desired, Some((index, link)));
        } else {
            self.set_base(target, from + (desired - from) * progress, Some((index, link)));
        }
    }

    fn toggle(&mut self, index: usize, link: usize, levels: &[f32]) {
        let target = self.controls[index].setup.links[link].target;
        let current = self.targets[target].base;
        let Some(nearest) = levels
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| (*a - current).abs().total_cmp(&(*b - current).abs()))
            .map(|(nearest, _)| nearest)
        else {
            return;
        };
        self.set_base(target, levels[(nearest + 1) % levels.len()], Some((index, link)));
    }

    /// Puts `target`'s base at `position`. Other controls' links to it lose
    /// it (it moved away from them).
    fn set_base(&mut self, target: TargetId, position: f32, by: Option<(usize, usize)>) {
        self.targets[target].base = position.clamp(0.0, 1.0);
        self.recompute(target);
        self.lose(target, by);
        self.show_target(target, by.map(|(index, _)| index));
    }

    /// Works out `target`'s effective position from its layers, and writes
    /// it if it or the base changed.
    fn recompute(&mut self, target: TargetId) {
        let offsets: f32 = self
            .controls
            .iter()
            .flat_map(|control| control.setup.links.iter().zip(&control.links))
            .filter(|(link, _)| link.target == target && link.mode.modulates())
            .map(|(_, state)| state.offset)
            .sum();
        let state = &mut self.targets[target];
        state.effective = if state.held > 0 { state.hold } else { state.base + offsets }.clamp(0.0, 1.0);
        let (effective, base) = state.written;
        if (state.effective - effective).abs() > 1e-6 || (state.base - base).abs() > 1e-6 {
            state.written = (state.effective, state.base);
            self.writes.push(Write { target, position: state.effective, base: state.base });
        }
    }

    /// Every link to `target` but `except` loses it, unless the control wants
    /// its base where it is anyway.
    fn lose(&mut self, target: TargetId, except: Option<(usize, usize)>) {
        let base = self.targets[target].base;
        for index in 0..self.controls.len() {
            let mut touches = false;
            let control = &mut self.controls[index];
            for (link, state) in control.links.iter_mut().enumerate() {
                let mapped = &control.setup.links[link];
                if mapped.target != target || mapped.mode.modulates() || except == Some((index, link)) {
                    continue;
                }
                touches = true;
                if state.desired.is_none_or(|desired| (desired - base).abs() > TOLERANCE) {
                    state.engaged = false;
                    state.glide = None;
                }
            }
            if touches {
                self.update_waiting(index);
            }
        }
    }

    /// `target` was set elsewhere (the mouse, a preset…) to `position`: that's
    /// its base now. The target has it already; with modulation on top, it
    /// gets the effective position again.
    pub fn set_position(&mut self, target: TargetId, position: f32) {
        let Some(state) = self.targets.get_mut(target) else { return };
        let position = position.clamp(0.0, 1.0);
        if (state.base - position).abs() < 1e-6 && (state.effective - position).abs() < 1e-6 {
            return;
        }
        state.base = position;
        state.effective = position;
        state.written = (position, position);
        self.recompute(target);
        self.lose(target, None);
        self.show_target(target, None);
    }

    /// Moves gliding targets on.
    pub fn tick(&mut self, now: Instant) {
        for index in 0..self.controls.len() {
            let gliding = self.controls[index].links.iter().any(|state| state.glide.is_some());
            for link in 0..self.controls[index].links.len() {
                if self.controls[index].links[link].glide.is_some() {
                    self.glide(index, link, now);
                }
            }
            if gliding {
                self.update_waiting(index);
            }
        }
    }

    /// Whether a target is gliding: [`Mappings::tick`] has work.
    pub fn gliding(&self) -> bool {
        self.controls.iter().any(|control| control.links.iter().any(|state| state.glide.is_some()))
    }

    /// The effective positions given since the last call, oldest first.
    pub fn take_writes(&mut self) -> Vec<Write> {
        std::mem::take(&mut self.writes)
    }

    /// The controls whose [`Waiting`] changed since the last call.
    pub fn take_waiting(&mut self) -> Vec<Waiting> {
        std::mem::take(&mut self.waiting)
    }

    /// The pick-up hints since the last call, oldest first.
    pub fn take_pick_ups(&mut self) -> Vec<PickUp> {
        std::mem::take(&mut self.pick_ups)
    }

    /// Which of surface `surface`'s controls (as many as it has) drive
    /// something.
    pub fn mapped(&self, surface: usize) -> Vec<bool> {
        let count = self.surfaces.get(surface).map_or(0, |surface| surface.controls.len());
        let mut mapped = vec![false; count];
        for control in self.controls.iter().filter(|control| control.setup.surface == surface) {
            if let Some(slot) = mapped.get_mut(control.setup.control) {
                *slot = true;
            }
        }
        mapped
    }

    /// Notes whether control `index`'s first range target is somewhere else
    /// than the hardware, for its ghost in the panel.
    fn update_waiting(&mut self, index: usize) {
        let control = &self.controls[index];
        let at = (!control.jumps())
            .then(|| {
                let hardware = control.hardware?;
                let (link, state) = control
                    .setup
                    .links
                    .iter()
                    .zip(&control.links)
                    .find(|(link, _)| matches!(link.mode, Mode::Range { .. }))?;
                let Mode::Range { from, to } = link.mode else { return None };
                if state.engaged || to == from {
                    return None;
                }
                let at = ((self.targets[link.target].base - from) / (to - from)).clamp(0.0, 1.0);
                (state.glide.is_some() || (at - hardware).abs() > TOLERANCE).then_some(at)
            })
            .flatten();
        let control = &mut self.controls[index];
        let changed = match (control.waiting, at) {
            (Some(a), Some(b)) => (a - b).abs() > 0.001,
            (a, b) => a.is_some() != b.is_some(),
        };
        if changed {
            control.waiting = at;
            self.waiting.push(Waiting { surface: control.setup.surface, control: control.setup.control, at });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fader (CC 1), a button (note 36) and a pad (note 37), channel 1.
    fn surface() -> Arc<Surface> {
        let json = r#"{ "id": "test", "name": "Test", "devices": ["Test"], "banks": [
            { "name": "F", "kind": "fader", "x": 0, "y": 0, "rows": 1, "columns": 1, "prefix": "F",
              "channel": 1, "controls": [{ "cc": 1 }] },
            { "name": "B", "kind": "button", "x": 0, "y": 0, "rows": 1, "columns": 1, "prefix": "B",
              "channel": 1, "controls": [{ "note": 36 }] },
            { "name": "P", "kind": "pad", "x": 0, "y": 0, "rows": 1, "columns": 1, "prefix": "P",
              "channel": 1, "controls": [{ "note": 37 }] }
        ] }"#;
        Arc::new(Surface::from_json(json).unwrap())
    }

    const FADER: usize = 0;
    const BUTTON: usize = 1;
    const PAD: usize = 2;

    fn setup(control: usize, takeover: Takeover, mode: Mode) -> ControlSetup {
        ControlSetup {
            surface: 0,
            control,
            takeover,
            glide: Duration::from_millis(100),
            pass_through: false,
            release_to_zero: true,
            feedback: false,
            links: vec![Link { target: 0, mode, invert_feedback: false }],
        }
    }

    fn mappings(controls: Vec<ControlSetup>, position: f32) -> Mappings {
        Mappings::new(vec![surface()], controls, vec![position])
    }

    fn cc(mappings: &mut Mappings, value: u8, now: Instant) -> Vec<f32> {
        mappings.event(0, 1, &MidiEvent::from_raw(0, &[0xB0, 1, value]).unwrap(), now);
        mappings.take_writes().iter().map(|write| write.position).collect()
    }

    fn note(mappings: &mut Mappings, note: u8, velocity: u8) -> Vec<f32> {
        mappings.event(0, 1, &MidiEvent::from_raw(0, &[0x90, note, velocity]).unwrap(), Instant::now());
        mappings.take_writes().iter().map(|write| write.position).collect()
    }

    const FULL: Mode = Mode::Range { from: 0.0, to: 1.0 };

    #[test]
    fn jump_and_mirrored_ranges() {
        let now = Instant::now();
        let mut jump = mappings(vec![setup(FADER, Takeover::Jump, FULL)], 0.9);
        assert_eq!(cc(&mut jump, 0, now), [0.0]);
        let mut mirrored =
            mappings(vec![setup(FADER, Takeover::Jump, Mode::Range { from: 0.8, to: 0.2 })], 0.5);
        let moved = cc(&mut mirrored, 127, now);
        assert!((moved[0] - 0.2).abs() < 1e-6, "{moved:?}");
    }

    #[test]
    fn pick_up_waits_for_the_control_to_reach_the_target() {
        let now = Instant::now();
        let mut mappings = mappings(vec![setup(FADER, Takeover::PickUp, FULL)], 0.5);
        assert!(cc(&mut mappings, 10, now).is_empty());
        assert_eq!(mappings.take_waiting()[0].at, Some(0.5));
        // The target is above: "move it up", each move.
        assert_eq!(mappings.take_pick_ups(), [PickUp { surface: 0, control: FADER, up: true }]);
        assert!(cc(&mut mappings, 40, now).is_empty());
        assert_eq!(mappings.take_pick_ups().len(), 1);
        // Past it: picked up, no more hints.
        assert_eq!(cc(&mut mappings, 80, now), [80.0 / 127.0]);
        assert_eq!(mappings.take_waiting()[0].at, None);
        assert!(mappings.take_pick_ups().is_empty());
        assert_eq!(cc(&mut mappings, 20, now), [20.0 / 127.0]);
        // Moved elsewhere: lost again until reached.
        mappings.set_position(0, 0.9);
        assert!(cc(&mut mappings, 30, now).is_empty());
        assert_eq!(cc(&mut mappings, 115, now), [115.0 / 127.0]);
    }

    #[test]
    fn glide_slides_to_the_control_then_follows() {
        let start = Instant::now();
        let mut mappings = mappings(vec![setup(FADER, Takeover::Glide, FULL)], 0.0);
        // It starts where the target is: nothing to write yet.
        assert!(cc(&mut mappings, 127, start).is_empty());
        assert!(mappings.gliding());
        mappings.tick(start + Duration::from_millis(50));
        assert_eq!(mappings.take_writes(), [Write { target: 0, position: 0.5, base: 0.5 }]);
        // It follows the control while sliding: 3/4 of the way to the middle.
        let moved = cc(&mut mappings, 64, start + Duration::from_millis(75));
        assert!((moved[0] - 0.75 * 64.0 / 127.0).abs() < 1e-3, "{moved:?}");
        mappings.tick(start + Duration::from_millis(100));
        assert_eq!(mappings.take_writes(), [Write { target: 0, position: 64.0 / 127.0, base: 64.0 / 127.0 }]);
        assert!(!mappings.gliding());
        assert_eq!(cc(&mut mappings, 127, start + Duration::from_millis(110)), [1.0]);
    }

    #[test]
    fn toggle_steps_from_where_the_target_is() {
        let levels = vec![0.0, 0.5, 1.0];
        let mut mappings = mappings(vec![setup(BUTTON, Takeover::Glide, Mode::Toggle { levels })], 0.0);
        assert_eq!(note(&mut mappings, 36, 100), [0.5]);
        assert!(note(&mut mappings, 36, 0).is_empty());
        mappings.set_position(0, 0.45);
        assert_eq!(note(&mut mappings, 36, 100), [1.0]);
        note(&mut mappings, 36, 0);
        assert_eq!(note(&mut mappings, 36, 100), [0.0]);
    }

    #[test]
    fn momentary_goes_back_to_the_latest_base() {
        let mode = Mode::Momentary { value: 1.0, velocity: true };
        let mut mappings = mappings(vec![setup(PAD, Takeover::Glide, mode)], 0.3);
        assert_eq!(note(&mut mappings, 37, 127), [1.0]);
        // Set elsewhere while held: that's the base now, which it goes back
        // to; while held, the button still wins.
        mappings.set_position(0, 0.6);
        assert_eq!(mappings.take_writes().iter().map(|w| w.position).collect::<Vec<_>>(), [1.0]);
        assert_eq!(note(&mut mappings, 37, 0), [0.6]);
    }

    #[test]
    fn pads_jump_and_follow_pressure_while_held() {
        let mut mappings = mappings(vec![setup(PAD, Takeover::PickUp, FULL)], 0.9);
        assert_eq!(note(&mut mappings, 37, 127), [1.0]);
        assert_eq!(note(&mut mappings, 37, 0), [0.0]);
    }

    #[test]
    fn a_control_moving_a_target_takes_it_from_another() {
        let now = Instant::now();
        let mut second = setup(BUTTON, Takeover::Glide, Mode::Toggle { levels: vec![0.0, 1.0] });
        second.links[0].target = 0;
        let mut mappings = mappings(vec![setup(FADER, Takeover::PickUp, FULL), second], 0.0);
        assert!(cc(&mut mappings, 0, now).is_empty());
        assert_eq!(cc(&mut mappings, 30, now).len(), 1);
        // The button moves it away: the fader has to pick it up again.
        assert_eq!(note(&mut mappings, 36, 100), [1.0]);
        assert!(cc(&mut mappings, 40, now).is_empty());
    }

    fn writes(mappings: &mut Mappings) -> Vec<(f32, f32)> {
        mappings.take_writes().iter().map(|w| (w.position, w.base)).collect()
    }

    fn close(a: &[f32], b: &[f32]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| (a - b).abs() < 1e-5)
    }

    #[test]
    fn modulation_rides_on_the_base() {
        let now = Instant::now();
        let modulate = setup(FADER, Takeover::Glide, Mode::Modulate { low: -0.2, high: 0.2 });
        let mut push = setup(BUTTON, Takeover::Glide, Mode::MomentaryModulate { offset: 0.1, velocity: false });
        push.links[0].target = 0;
        let mut mappings = mappings(vec![modulate, push], 0.5);
        assert!(close(&cc(&mut mappings, 127, now), &[0.7]));
        // A new base: the modulation stays on top of it.
        mappings.set_position(0, 0.4);
        let written = writes(&mut mappings);
        assert!(close(&[written[0].0, written[0].1], &[0.6, 0.4]), "{written:?}");
        // Offsets add up, and clamp.
        assert!(close(&note(&mut mappings, 36, 100), &[0.7]));
        mappings.set_position(0, 0.95);
        assert_eq!(writes(&mut mappings), [(1.0, 0.95)]);
        // Still clamped without the button's offset: only the base was new.
        assert!(note(&mut mappings, 36, 0).is_empty());
    }

    #[test]
    fn a_range_moves_the_base_under_a_momentary_override() {
        let now = Instant::now();
        let mut hold = setup(BUTTON, Takeover::Jump, Mode::Momentary { value: 1.0, velocity: false });
        hold.links[0].target = 0;
        let mut mappings = mappings(vec![setup(FADER, Takeover::Jump, FULL), hold], 0.0);
        assert_eq!(note(&mut mappings, 36, 100), [1.0]);
        // The fader moves the base while the button holds the target.
        assert_eq!(cc(&mut mappings, 127, now), [1.0]);
        assert_eq!(mappings.targets[0].base, 1.0);
        assert_eq!(note(&mut mappings, 36, 0), Vec::<f32>::new());
        assert!(close(&cc(&mut mappings, 64, now), &[64.0 / 127.0]));
    }

    #[test]
    fn feedback_shows_changes_from_elsewhere_only() {
        let now = Instant::now();
        let surface = || -> Arc<Surface> {
            let json = r#"{ "id": "t", "name": "t", "banks": [
                { "name": "F", "kind": "fader", "columns": 1, "prefix": "F", "feedback": true, "controls": [{ "cc": 1 }] },
                { "name": "B", "kind": "button", "columns": 1, "prefix": "B", "feedback": true, "controls": [{ "note": 36 }] }
            ] }"#;
            Arc::new(Surface::from_json(json).unwrap())
        };
        let mut fader = setup(FADER, Takeover::PickUp, FULL);
        fader.feedback = true;
        let mut button = setup(BUTTON, Takeover::Jump, Mode::Toggle { levels: vec![0.0, 1.0] });
        button.feedback = true;
        let mut mappings = Mappings::new(vec![surface()], vec![fader, button], vec![0.5]);
        let shown = |mappings: &mut Mappings| -> Vec<(usize, Shown)> {
            mappings.take_feedback().iter().map(|f| (f.control, f.shown)).collect()
        };
        mappings.resync_feedback();
        assert_eq!(shown(&mut mappings), [(0, Shown::Position(0.5)), (1, Shown::Lit(true))]);
        // With feedback, a fader jumps (no pick up); its own move isn't sent back.
        assert_eq!(cc(&mut mappings, 0, now), [0.0]);
        assert_eq!(shown(&mut mappings), [(1, Shown::Lit(false))]);
        // The button turns it on: the fader is sent there.
        note(&mut mappings, 36, 100);
        assert_eq!(shown(&mut mappings), [(0, Shown::Position(1.0)), (1, Shown::Lit(true))]);
        mappings.set_position(0, 0.25);
        assert_eq!(shown(&mut mappings), [(0, Shown::Position(0.25))]);
        // Let go, a button is sent its light again (it turned it off itself).
        note(&mut mappings, 36, 0);
        assert_eq!(shown(&mut mappings), [(1, Shown::Lit(true))]);
    }

    #[test]
    fn a_feedback_echo_isnt_a_move() {
        let now = Instant::now();
        let surface = Arc::new(
            Surface::from_json(
                r#"{ "id": "t", "name": "t", "banks": [
                    { "name": "F", "kind": "fader", "columns": 2, "feedback": true, "controls": [{ "cc": 1 }, { "cc": 2 }] }
                ] }"#,
            )
            .unwrap(),
        );
        let mut sent = setup(FADER, Takeover::Jump, FULL);
        sent.feedback = true;
        let mut mover = setup(1, Takeover::Jump, FULL);
        mover.feedback = true;
        let mut mappings = Mappings::new(vec![surface], vec![sent, mover], vec![0.0]);
        let move_second = |mappings: &mut Mappings, value: u8| {
            mappings.event(0, 1, &MidiEvent::from_raw(0, &[0xB0, 2, value]).unwrap(), now);
            mappings.take_writes().len()
        };
        assert_eq!(move_second(&mut mappings, 100), 1);
        assert_eq!(mappings.take_feedback().len(), 1);
        // The first fader echoes the 100 it was sent: nothing happens.
        assert!(cc(&mut mappings, 100, now).is_empty());
        assert!(mappings.take_feedback().is_empty());
        // Moved for real, it takes the target.
        assert_eq!(cc(&mut mappings, 20, now), [20.0 / 127.0]);
    }

    #[test]
    fn a_page_change_shows_that_pages_lights_again() {
        let surface = Arc::new(
            Surface::from_json(
                r#"{ "id": "t", "name": "t",
                    "pages": [{ "id": "a", "name": "A", "button": "To A" }, { "id": "b", "name": "B", "button": "To B" }],
                    "banks": [
                    { "name": "a", "kind": "button", "columns": 1, "page": "a", "feedback": true, "controls": [{ "cc": 21 }] },
                    { "name": "b", "kind": "button", "columns": 1, "page": "b", "feedback": true, "controls": [{ "cc": 47 }] },
                    { "name": "p", "kind": "button", "columns": 2, "controls": [{ "cc": 113, "label": "To A" }, { "cc": 114, "label": "To B" }] }
                ] }"#,
            )
            .unwrap(),
        );
        let toggle = |control| {
            let mut setup = setup(control, Takeover::Jump, Mode::Toggle { levels: vec![0.0, 1.0] });
            setup.feedback = true;
            setup
        };
        let mut mappings = Mappings::new(vec![surface], vec![toggle(0), toggle(1)], vec![1.0]);
        mappings.resync_feedback();
        mappings.take_feedback();
        let mut press = |cc: u8| {
            mappings.event(0, 1, &MidiEvent::from_raw(0, &[0xB0, cc, 127]).unwrap(), Instant::now());
            mappings.take_feedback().iter().map(|f| f.control).collect::<Vec<_>>()
        };
        // On page A already: nothing changes.
        assert!(press(113).is_empty());
        assert_eq!(press(114), [1]);
        assert_eq!(press(113), [0]);
    }
}
