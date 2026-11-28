//! A plugin's control output ports (meters, gain reduction, latency…) for its
//! GUI. LV2 GUIs learn them only from the host (`ui:portNotification` with
//! `ui:floatProtocol`): the audio thread stores each one after every `run`, and
//! the control thread hands the ones that changed to an open GUI.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Every control output of one plugin as last written: shared between the
/// audio thread, which stores them, and the control thread, which reads them.
#[derive(Clone)]
pub struct ControlOutputs(Arc<[Output]>);

struct Output {
    port: u32,
    /// The value's bits (`f32::to_bits`), so it fits an atomic.
    bits: AtomicU32,
}

impl ControlOutputs {
    /// For the control output ports `ports`, at their defaults.
    pub(crate) fn new(ports: impl IntoIterator<Item = (u32, f32)>) -> Self {
        Self(
            ports
                .into_iter()
                .map(|(port, value)| Output { port, bits: AtomicU32::new(value.to_bits()) })
                .collect(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Stores each output's value as `read` gives it (by port). Real-time safe.
    pub(crate) fn store(&self, mut read: impl FnMut(u32) -> Option<f32>) {
        for output in self.0.iter() {
            if let Some(value) = read(output.port) {
                output.bits.store(value.to_bits(), Ordering::Relaxed);
            }
        }
    }

    /// Every output's port and value as last stored.
    pub fn values(&self) -> impl Iterator<Item = (u32, f32)> + '_ {
        self.0.iter().map(|output| (output.port, f32::from_bits(output.bits.load(Ordering::Relaxed))))
    }

    /// A watcher handing over the values that change from now on (see
    /// [`ControlOutputWatcher::changed`]).
    pub fn watch(&self) -> ControlOutputWatcher {
        let last = self.values().map(|(_, value)| value.to_bits()).collect();
        ControlOutputWatcher { outputs: self.clone(), last }
    }
}

/// The control thread's view of a plugin's [`ControlOutputs`]: what it last
/// handed over, so only changes go to a GUI. Called at the GUI's pace (the poll
/// timer, ~60 Hz), that's also the rate meters are sent at, whatever the block
/// size; each GUI tick sees the value of the latest block.
pub struct ControlOutputWatcher {
    outputs: ControlOutputs,
    last: Vec<u32>,
}

impl ControlOutputWatcher {
    /// Calls `forward` with each output (port, value) that changed since the
    /// last call (or since the watcher was made).
    pub fn changed(&mut self, mut forward: impl FnMut(u32, f32)) {
        for ((port, value), last) in self.outputs.values().zip(&mut self.last) {
            // By bits: a NaN equals itself, and -0.0 differs from 0.0 (rare; harmless).
            if value.to_bits() != *last {
                *last = value.to_bits();
                forward(port, value);
            }
        }
    }

    /// Every output's current value, changed or not (for a GUI just opened).
    pub fn current(&self) -> impl Iterator<Item = (u32, f32)> + '_ {
        self.outputs.values()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changes(watcher: &mut ControlOutputWatcher) -> Vec<(u32, f32)> {
        let mut changed = Vec::new();
        watcher.changed(|port, value| changed.push((port, value)));
        changed
    }

    #[test]
    fn hands_over_only_what_changed() {
        let outputs = ControlOutputs::new([(4, 0.0), (7, 1.0), (9, f32::NAN)]);
        let mut watcher = outputs.watch();
        // Nothing stored yet: nothing changed (a NaN default included).
        assert!(changes(&mut watcher).is_empty());
        // The audio thread stores every port it can read; port 9 it can't.
        outputs.store(|port| match port {
            4 => Some(0.5),
            7 => Some(1.0),
            _ => None,
        });
        assert_eq!(changes(&mut watcher), [(4, 0.5)]);
        // The same values again: nothing new.
        outputs.store(|port| (port == 4).then_some(0.5));
        assert!(changes(&mut watcher).is_empty());
        // A GUI just opened gets them all, whatever changed.
        assert_eq!(watcher.current().count(), 3);
        outputs.store(|port| (port == 7).then_some(0.25));
        assert_eq!(changes(&mut watcher), [(7, 0.25)]);
    }
}
