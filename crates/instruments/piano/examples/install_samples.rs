//! Downloads and installs the piano's samples, as the app does, printing
//! progress. Into the default folder unless one is given.
//!
//! cargo run -p live-keys-piano --example install_samples --release -- [folder]

use live_keys_piano::samples::{self, Progress};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let dir = match std::env::args().nth(1) {
        Some(dir) => dir.into(),
        None => live_keys_piano::default_sample_dir().expect("no data folder"),
    };
    println!("installing into {}", dir.display());
    let progress = Arc::new(Progress::default());
    let watched = progress.clone();
    let started = std::time::Instant::now();
    let worker = std::thread::spawn(move || samples::install(&dir, &watched));
    while !worker.is_finished() {
        std::thread::sleep(std::time::Duration::from_secs(5));
        println!(
            "{}/{} files, {} MB",
            progress.files.load(Relaxed),
            samples::file_count(),
            progress.bytes.load(Relaxed) / 1_000_000
        );
    }
    worker.join().expect("install thread panicked")?;
    println!("done in {:.0} s", started.elapsed().as_secs_f64());
    Ok(())
}
