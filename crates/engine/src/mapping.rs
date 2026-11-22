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
use live_keys_surfaces::{ControlKind, Reading, Surface};

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
    /// Faders, knobs, pads: the target goes to `from + p·(to − from)` for the
    /// control at `p`. Mirrored is `from > to`.
    Range { from: f32, to: f32 },
    /// Buttons, pads: each press moves the target to the level after the one
    /// nearest where it is now, wrapping.
    Toggle { levels: Vec<f32> },
    /// Buttons, pads: while held, the target is at `value` (times the pad's
    /// velocity if `velocity`); let go, it goes back.
    Momentary { value: f32, velocity: bool },
}

/// One of a control's targets.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    pub target: TargetId,
    pub mode: Mode,
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
    pub links: Vec<Link>,
}

/// A position a mapping gave a target.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Write {
    pub target: TargetId,
    pub position: f32,
}

/// A fader or knob whose target is elsewhere than the hardware (`at`: the
/// target, as a position of the control), or no longer is (`None`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Waiting {
    pub surface: usize,
    pub control: usize,
    pub at: Option<f32>,
}

#[derive(Debug, Default)]
struct LinkState {
    /// The target follows the control.
    engaged: bool,
    /// Sliding to the control since then, from this position.
    glide: Option<(Instant, f32)>,
    /// Where the control last wanted the target.
    desired: Option<f32>,
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
}

impl ControlState {
    fn jumps(&self) -> bool {
        matches!(self.kind, ControlKind::Pad | ControlKind::Button) || self.setup.takeover == Takeover::Jump
    }
}

#[derive(Debug)]
struct TargetState {
    position: f32,
    /// While momentary buttons hold it: the position to go back to.
    base: Option<f32>,
    held: u32,
}

/// Every mapping of one setup, and their state.
pub struct Mappings {
    surfaces: Vec<Arc<Surface>>,
    controls: Vec<ControlState>,
    /// `(surface, control)` → index into `controls`.
    by_control: HashMap<(usize, usize), usize>,
    targets: Vec<TargetState>,
    writes: Vec<Write>,
    waiting: Vec<Waiting>,
}

impl Mappings {
    /// `positions` are the targets' current positions; controls pointing at
    /// surfaces, controls or targets that don't exist are left out.
    pub fn new(surfaces: Vec<Arc<Surface>>, controls: Vec<ControlSetup>, positions: Vec<f32>) -> Self {
        let targets: Vec<TargetState> = positions
            .into_iter()
            .map(|position| TargetState { position: position.clamp(0.0, 1.0), base: None, held: 0 })
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
            });
        }
        Self { surfaces, controls: states, by_control, targets, writes: Vec::new(), waiting: Vec::new() }
    }


    /// Applies a MIDI event from surface `surface`.
    pub fn event(&mut self, surface: usize, event: &MidiEvent, now: Instant) {
        let Some(device) = self.surfaces.get(surface).cloned() else { return };
        if let Some((control, reading)) = device.read(event) {
            if let Some(&index) = self.by_control.get(&(surface, control)) {
                self.reading(index, reading, now);
            }
        } else if let Some((pads, pressure)) = device.pad_pressure(event) {
            // The held pads' pressure.
            for pad in pads {
                if let Some(&index) = self.by_control.get(&(surface, pad))
                    && self.controls[index].pressed
                {
                    self.reading(index, Reading::Value(pressure), now);
                }
            }
        }
    }

    fn reading(&mut self, index: usize, reading: Reading, now: Instant) {
        let pad_or_button = matches!(self.controls[index].kind, ControlKind::Pad | ControlKind::Button);
        match reading {
            Reading::Value(value) => {
                // A pad's pressure counts only while it's held.
                if pad_or_button && !self.controls[index].pressed {
                    return;
                }
                self.controls[index].hardware = Some(value);
                for link in 0..self.controls[index].links.len() {
                    if let Mode::Range { .. } = self.controls[index].setup.links[link].mode {
                        self.range(index, link, value, now);
                    }
                }
            }
            Reading::Press(velocity) => {
                self.controls[index].pressed = true;
                self.controls[index].hardware = Some(velocity);
                for link in 0..self.controls[index].links.len() {
                    match self.controls[index].setup.links[link].mode.clone() {
                        Mode::Range { .. } => self.range(index, link, velocity, now),
                        Mode::Toggle { levels } => self.toggle(index, link, &levels),
                        Mode::Momentary { value, velocity: sensitive } => {
                            let target = self.controls[index].setup.links[link].target;
                            let state = &mut self.targets[target];
                            if state.held == 0 {
                                state.base = Some(state.position);
                            }
                            state.held += 1;
                            let position = if sensitive { value * velocity } else { value };
                            self.set(target, position, Some((index, link)));
                        }
                    }
                }
            }
            Reading::Release => {
                self.controls[index].pressed = false;
                for link in 0..self.controls[index].links.len() {
                    match self.controls[index].setup.links[link].mode {
                        Mode::Range { .. } if self.controls[index].setup.release_to_zero => {
                            self.controls[index].hardware = Some(0.0);
                            self.range(index, link, 0.0, now);
                        }
                        Mode::Momentary { .. } => {
                            let target = self.controls[index].setup.links[link].target;
                            let state = &mut self.targets[target];
                            state.held = state.held.saturating_sub(1);
                            if state.held == 0
                                && let Some(base) = state.base.take()
                            {
                                self.set(target, base, Some((index, link)));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        self.update_waiting(index);
    }

    /// The control at `hardware`, for its range link `link`.
    fn range(&mut self, index: usize, link: usize, hardware: f32, now: Instant) {
        let control = &self.controls[index];
        let Mode::Range { from, to } = control.setup.links[link].mode else { return };
        let target = control.setup.links[link].target;
        let jumps = control.jumps();
        let takeover = control.setup.takeover;
        let desired = from + hardware * (to - from);
        let current = self.targets[target].position;
        let state = &mut self.controls[index].links[link];
        let previous = state.desired.replace(desired);
        if state.engaged || jumps {
            state.engaged = true;
            state.glide = None;
            return self.write(target, desired, (index, link));
        }
        match takeover {
            Takeover::Jump => {}
            Takeover::PickUp => {
                let crossed = previous.is_some_and(|previous| (previous - current) * (desired - current) <= 0.0);
                if crossed || (desired - current).abs() <= TOLERANCE {
                    state.engaged = true;
                    self.write(target, desired, (index, link));
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
            self.write(target, desired, (index, link));
        } else {
            self.write(target, from + (desired - from) * progress, (index, link));
        }
    }

    fn toggle(&mut self, index: usize, link: usize, levels: &[f32]) {
        let target = self.controls[index].setup.links[link].target;
        let state = &self.targets[target];
        let current = state.base.unwrap_or(state.position);
        let Some(nearest) = levels
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| (*a - current).abs().total_cmp(&(*b - current).abs()))
            .map(|(nearest, _)| nearest)
        else {
            return;
        };
        self.write(target, levels[(nearest + 1) % levels.len()], (index, link));
    }

    /// A mapping moves `target`: while a momentary button holds it, that's
    /// where it goes back to; otherwise it goes there.
    fn write(&mut self, target: TargetId, position: f32, by: (usize, usize)) {
        let state = &mut self.targets[target];
        if state.held > 0 {
            state.base = Some(position.clamp(0.0, 1.0));
        } else {
            self.set(target, position, Some(by));
        }
    }

    /// Puts `target` at `position`. Other controls' links to it lose it (it
    /// moved away from them).
    fn set(&mut self, target: TargetId, position: f32, by: Option<(usize, usize)>) {
        let position = position.clamp(0.0, 1.0);
        self.targets[target].position = position;
        self.writes.push(Write { target, position });
        self.lose(target, by);
    }

    /// Every link to `target` but `except` loses it, unless the control wants
    /// it where it is anyway.
    fn lose(&mut self, target: TargetId, except: Option<(usize, usize)>) {
        let position = self.targets[target].position;
        for index in 0..self.controls.len() {
            let mut touches = false;
            let control = &mut self.controls[index];
            for (link, state) in control.links.iter_mut().enumerate() {
                if control.setup.links[link].target != target || except == Some((index, link)) {
                    continue;
                }
                touches = true;
                if state.desired.is_none_or(|desired| (desired - position).abs() > TOLERANCE) {
                    state.engaged = false;
                    state.glide = None;
                }
            }
            if touches {
                self.update_waiting(index);
            }
        }
    }

    /// `target` was moved elsewhere (the mouse, a preset…) to `position`.
    pub fn set_position(&mut self, target: TargetId, position: f32) {
        let Some(state) = self.targets.get_mut(target) else { return };
        let position = position.clamp(0.0, 1.0);
        if state.held > 0 {
            state.base = Some(position);
            return;
        }
        if (state.position - position).abs() < 1e-6 {
            return;
        }
        state.position = position;
        self.lose(target, None);
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

    /// The positions given since the last call, oldest first.
    pub fn take_writes(&mut self) -> Vec<Write> {
        std::mem::take(&mut self.writes)
    }

    /// The controls whose [`Waiting`] changed since the last call.
    pub fn take_waiting(&mut self) -> Vec<Waiting> {
        std::mem::take(&mut self.waiting)
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
                let at = ((self.targets[link.target].position - from) / (to - from)).clamp(0.0, 1.0);
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
            links: vec![Link { target: 0, mode }],
        }
    }

    fn mappings(controls: Vec<ControlSetup>, position: f32) -> Mappings {
        Mappings::new(vec![surface()], controls, vec![position])
    }

    fn cc(mappings: &mut Mappings, value: u8, now: Instant) -> Vec<f32> {
        mappings.event(0, &MidiEvent::from_raw(0, &[0xB0, 1, value]).unwrap(), now);
        mappings.take_writes().iter().map(|write| write.position).collect()
    }

    fn note(mappings: &mut Mappings, note: u8, velocity: u8) -> Vec<f32> {
        mappings.event(0, &MidiEvent::from_raw(0, &[0x90, note, velocity]).unwrap(), Instant::now());
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
        assert!(cc(&mut mappings, 40, now).is_empty());
        // Past it: picked up.
        assert_eq!(cc(&mut mappings, 80, now), [80.0 / 127.0]);
        assert_eq!(mappings.take_waiting()[0].at, None);
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
        assert_eq!(cc(&mut mappings, 127, start), [0.0]);
        assert!(mappings.gliding());
        mappings.tick(start + Duration::from_millis(50));
        assert_eq!(mappings.take_writes(), [Write { target: 0, position: 0.5 }]);
        // It follows the control while sliding: 3/4 of the way to the middle.
        let moved = cc(&mut mappings, 64, start + Duration::from_millis(75));
        assert!((moved[0] - 0.75 * 64.0 / 127.0).abs() < 1e-3, "{moved:?}");
        mappings.tick(start + Duration::from_millis(100));
        assert_eq!(mappings.take_writes(), [Write { target: 0, position: 64.0 / 127.0 }]);
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
        // Set elsewhere while held: that's where it goes back to.
        mappings.set_position(0, 0.6);
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
        assert_eq!(cc(&mut mappings, 0, now), [0.0]);
        assert_eq!(cc(&mut mappings, 30, now).len(), 1);
        // The button moves it away: the fader has to pick it up again.
        assert_eq!(note(&mut mappings, 36, 100), [1.0]);
        assert!(cc(&mut mappings, 40, now).is_empty());
    }
}
