//! The LV2 worker (`work:schedule`/`work:interface`): plugins hand slow jobs
//! from `run` (loading a sample or an impulse response, applying a state) to a
//! host thread, which calls their `work` and passes the response back to their
//! next `run`. livi queues the jobs and delivers the responses
//! (`livi::WorkerManager`); this is the thread that does the jobs. Every LSP
//! plugin, x42's zeroconvo and setBfree declare it.
//!
//! LV2 doesn't allow a plugin's `work` to run while its state is saved or
//! restored (both are in the instantiation threading class), so the state
//! calls hold [`Workers::pause`] meanwhile — briefly, for every plugin.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::Duration;

/// How often the thread looks for jobs. A job waits at most this long to start;
/// looking costs next to nothing when there are none.
const INTERVAL: Duration = Duration::from_millis(2);

/// The worker thread, running until dropped.
pub struct Workers {
    stop: Arc<AtomicBool>,
    /// Held while jobs run, and by [`Workers::pause`].
    running: Arc<Mutex<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Workers {
    pub fn start(manager: Arc<livi::WorkerManager>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(Mutex::new(()));
        let thread = {
            let (stop, running) = (stop.clone(), running.clone());
            std::thread::Builder::new()
                .name("lv2-worker".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        {
                            let _running = running.lock().unwrap_or_else(PoisonError::into_inner);
                            manager.run_workers();
                        }
                        std::thread::sleep(INTERVAL);
                    }
                })
                .map_err(|err| log::error!("couldn't start the LV2 worker thread: {err}"))
                .ok()
        };
        Self { stop, running, thread }
    }

    /// Keeps every plugin's `work` from running until the guard is dropped
    /// (waiting for a job that's running to finish).
    pub fn pause(&self) -> MutexGuard<'_, ()> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
