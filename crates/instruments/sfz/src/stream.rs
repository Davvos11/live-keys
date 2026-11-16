//! Streaming samples from disk.
//!
//! Each streamed sample has a `Slot`. When a voice starts on it, the audio
//! thread counts itself as a user and, if the sample's full data isn't loaded,
//! asks for it over a lock-free ring. A dispatcher thread hands requests to a few
//! loader threads, which decode the whole file into a buffer and *publish* it as
//! they go (`StreamData::available`), so a voice can read past its head as soon
//! as those frames exist. Data nobody has used for `KEEP` is freed by the
//! dispatcher. The audio thread never allocates, frees, locks or waits.
//!
//! Freeing races with a voice starting: the voice increments `users` and then
//! reads `data`; the dispatcher, seeing no users, swaps `data` to null and then
//! checks `users` again, putting the data back if a user appeared. With
//! sequentially consistent operations, a voice that read the old pointer is
//! always seen by that second check, so data is never freed under a voice.

use crate::sample::{Decoder, PAD_BACK, PAD_FRONT};
use rtrb::{Consumer, Producer, RingBuffer};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long loaded data is kept after its last voice ends, in milliseconds
/// (a static so tests can shorten it).
static KEEP_MS: AtomicU64 = AtomicU64::new(5000);
/// How often the dispatcher looks for requests.
const POLL: Duration = Duration::from_millis(1);
const HOUSEKEEPING: Duration = Duration::from_millis(100);
const LOADER_THREADS: usize = 4;
/// Requests a synth can have queued at once.
const REQUEST_QUEUE: usize = 1024;

/// One sample's full data, being decoded or decoded: interleaved, padded like
/// a sample's head. Written by one loader thread, read by voices up to
/// `available` frames.
pub(crate) struct StreamData {
    ptr: *mut f32,
    len: usize,
    channels: usize,
    /// Frame slots (padding included) that are written and may be read.
    available: AtomicUsize,
}

// The buffer is written only beyond `available` (by the one loader) and read
// only below it, with `available` published by release/acquire.
unsafe impl Send for StreamData {}
unsafe impl Sync for StreamData {}

impl StreamData {
    fn new(channels: usize, frames: usize) -> Box<StreamData> {
        let buffer = vec![0.0f32; (PAD_FRONT + frames + PAD_BACK) * channels].into_boxed_slice();
        let len = buffer.len();
        let ptr = Box::into_raw(buffer) as *mut f32;
        Box::new(StreamData { ptr, len, channels, available: AtomicUsize::new(PAD_FRONT) })
    }

    /// The part written so far.
    #[inline]
    pub(crate) fn published(&self) -> &[f32] {
        let frames = self.available.load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: the first `frames` slots are written and never written again.
        unsafe { std::slice::from_raw_parts(self.ptr, (frames * self.channels).min(self.len)) }
    }

    /// Writes interleaved frames at frame slot `at` and publishes them.
    fn write(&self, at: usize, samples: &[f32]) {
        let start = at * self.channels;
        let count = samples.len().min(self.len.saturating_sub(start));
        // SAFETY: only this loader writes, and only past what's published.
        unsafe { std::ptr::copy_nonoverlapping(samples.as_ptr(), self.ptr.add(start), count) };
        self.available.store(at + count / self.channels, std::sync::atomic::Ordering::Release);
    }

    fn finish(&self) {
        self.available.store(self.len / self.channels, std::sync::atomic::Ordering::Release);
    }

    fn bytes(&self) -> usize {
        self.len * size_of::<f32>()
    }
}

impl Drop for StreamData {
    fn drop(&mut self) {
        // SAFETY: made from a boxed slice of this length in `new`.
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(self.ptr, self.len)) });
    }
}

#[cfg(test)]
pub(crate) fn set_keep_for_tests(keep: Duration) {
    KEEP_MS.store(keep.as_millis() as u64, SeqCst);
}

#[derive(Default)]
struct Slot {
    data: AtomicPtr<StreamData>,
    /// Voices playing the sample, plus a loader decoding it.
    users: AtomicU32,
    /// A request is queued (so a voice doesn't queue another).
    requested: AtomicBool,
    loading: AtomicBool,
}

/// A streamed sample's file and layout.
pub(crate) struct StreamFile {
    pub path: PathBuf,
    pub channels: usize,
    pub frames: usize,
}

struct Shared {
    files: Vec<StreamFile>,
    slots: Vec<Slot>,
    queues: Mutex<Vec<Consumer<u32>>>,
    stop: AtomicBool,
    underruns: AtomicU64,
    loaded_bytes: AtomicUsize,
}

/// The loader threads for one instrument. Dropping it stops them and frees all
/// streamed data; that waits for the threads, so not on an audio thread.
pub(crate) struct Streamer {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl Streamer {
    /// For the instrument's streamed samples; slot `i` is `files[i]`.
    pub(crate) fn new(files: Vec<StreamFile>) -> Streamer {
        let slots = files.iter().map(|_| Slot::default()).collect();
        let shared = Arc::new(Shared {
            files,
            slots,
            queues: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            loaded_bytes: AtomicUsize::new(0),
        });
        let (job_tx, job_rx) = mpsc::channel::<u32>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let mut threads = Vec::new();
        for i in 0..LOADER_THREADS {
            let shared = shared.clone();
            let job_rx = job_rx.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("sfz-loader-{i}"))
                    .spawn(move || loader(&shared, &job_rx))
                    .expect("spawning a sample loader thread"),
            );
        }
        let dispatcher_shared = shared.clone();
        threads.push(
            std::thread::Builder::new()
                .name("sfz-stream".into())
                .spawn(move || dispatcher(&dispatcher_shared, job_tx))
                .expect("spawning the sample stream thread"),
        );
        Streamer { shared, threads }
    }

    /// A client for one synth (each needs its own request queue).
    pub(crate) fn client(&self) -> StreamClient {
        let (producer, consumer) = RingBuffer::new(REQUEST_QUEUE);
        self.shared.queues.lock().unwrap().push(consumer);
        StreamClient { shared: self.shared.clone(), requests: producer }
    }

    pub(crate) fn underruns(&self) -> u64 {
        self.shared.underruns.load(SeqCst)
    }

    pub(crate) fn loaded_bytes(&self) -> usize {
        self.shared.loaded_bytes.load(SeqCst)
    }
}

impl Drop for Streamer {
    fn drop(&mut self) {
        self.shared.stop.store(true, SeqCst);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        for slot in &self.shared.slots {
            let data = slot.data.swap(std::ptr::null_mut(), SeqCst);
            if !data.is_null() {
                // SAFETY: published by a loader from `Box::into_raw`; the synths
                // that could read it are gone (they hold the instrument).
                drop(unsafe { Box::from_raw(data) });
            }
        }
    }
}

/// The audio thread's side, owned by one synth.
pub(crate) struct StreamClient {
    shared: Arc<Shared>,
    requests: Producer<u32>,
}

impl StreamClient {
    /// A voice starts on slot `slot`: count it, and ask for the data if it isn't
    /// there. Returns the data as far as it's loaded (or null).
    pub(crate) fn acquire(&mut self, slot: u32) -> *const StreamData {
        let s = &self.shared.slots[slot as usize];
        s.users.fetch_add(1, SeqCst);
        let data = s.data.load(SeqCst);
        if data.is_null() {
            self.request(slot);
        }
        data
    }

    /// The data, if loaded by now (asking again if not).
    pub(crate) fn poll(&mut self, slot: u32) -> *const StreamData {
        let data = self.shared.slots[slot as usize].data.load(SeqCst);
        if data.is_null() {
            self.request(slot);
        }
        data
    }

    /// A voice on slot `slot` ended.
    pub(crate) fn release(&self, slot: u32) {
        self.shared.slots[slot as usize].users.fetch_sub(1, SeqCst);
    }

    pub(crate) fn count_underrun(&self) {
        self.shared.underruns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    fn request(&mut self, slot: u32) {
        let s = &self.shared.slots[slot as usize];
        if !s.requested.swap(true, SeqCst) && self.requests.push(slot).is_err() {
            s.requested.store(false, SeqCst); // queue full: try again next block
        }
    }
}

fn dispatcher(shared: &Shared, jobs: mpsc::Sender<u32>) {
    let mut idle_since: Vec<Option<Instant>> = vec![None; shared.slots.len()];
    let mut last_housekeeping = Instant::now();
    while !shared.stop.load(SeqCst) {
        {
            let mut queues = shared.queues.lock().unwrap();
            queues.retain(|q| !q.is_abandoned() || !q.is_empty());
            for queue in queues.iter_mut() {
                while let Ok(i) = queue.pop() {
                    let slot = &shared.slots[i as usize];
                    slot.requested.store(false, SeqCst);
                    if slot.data.load(SeqCst).is_null() && !slot.loading.swap(true, SeqCst) {
                        slot.users.fetch_add(1, SeqCst); // the loader's
                        if jobs.send(i).is_err() {
                            return;
                        }
                    }
                }
            }
        }
        if last_housekeeping.elapsed() >= HOUSEKEEPING {
            last_housekeeping = Instant::now();
            free_idle(shared, &mut idle_since);
        }
        std::thread::sleep(POLL);
    }
}

fn free_idle(shared: &Shared, idle_since: &mut [Option<Instant>]) {
    let now = Instant::now();
    for (slot, idle) in shared.slots.iter().zip(idle_since) {
        if slot.data.load(SeqCst).is_null() || slot.users.load(SeqCst) != 0 {
            *idle = None;
            continue;
        }
        let since = *idle.get_or_insert(now);
        if now - since < Duration::from_millis(KEEP_MS.load(SeqCst)) {
            continue;
        }
        let data = slot.data.swap(std::ptr::null_mut(), SeqCst);
        if slot.users.load(SeqCst) != 0 {
            slot.data.store(data, SeqCst); // a voice just started on it
            continue;
        }
        *idle = None;
        // SAFETY: from `Box::into_raw` in `loader`; no voice holds it (see the
        // module docs) and new ones see null.
        let data = unsafe { Box::from_raw(data) };
        shared.loaded_bytes.fetch_sub(data.bytes(), SeqCst);
    }
}

fn loader(shared: &Shared, jobs: &Mutex<mpsc::Receiver<u32>>) {
    loop {
        let job = jobs.lock().unwrap().recv();
        let Ok(i) = job else { return };
        let slot = &shared.slots[i as usize];
        load(shared, i as usize);
        slot.loading.store(false, SeqCst);
        slot.users.fetch_sub(1, SeqCst);
    }
}

fn load(shared: &Shared, i: usize) {
    let file = &shared.files[i];
    let slot = &shared.slots[i];
    let data = StreamData::new(file.channels, file.frames);
    shared.loaded_bytes.fetch_add(data.bytes(), SeqCst);
    let data = Box::into_raw(data);
    slot.data.store(data, SeqCst);
    // SAFETY: stays valid while this loader counts as a user.
    let data = unsafe { &*data };
    let result = (|| -> anyhow::Result<()> {
        let mut decoder = Decoder::open(&file.path)?;
        let mut buffer = Vec::new();
        let mut at = PAD_FRONT;
        while !shared.stop.load(SeqCst) {
            buffer.clear();
            let frames = decoder.read(&mut buffer)?;
            if frames == 0 {
                break;
            }
            data.write(at, &buffer);
            at += frames;
        }
        Ok(())
    })();
    match result {
        Ok(()) => data.finish(),
        // What decoded stays readable; the rest underruns into silence.
        Err(err) => log::warn!("sfz: streaming {}: {err:#}", file.path.display()),
    }
}
