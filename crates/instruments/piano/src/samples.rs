//! Installing the Salamander samples: downloads sfzinstruments' repository at
//! a pinned commit (GitHub's tarball of it), unpacks it into a staging folder
//! next to the target, checks every file against the SHA-256 list compiled in
//! (`salamander.sha256`; the tarball's own bytes aren't guaranteed stable, its
//! files are), and only then moves the folder into place. Blocking: run it on a
//! thread of its own, watching and cancelling it through a [`Progress`].

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

/// The commit of sfzinstruments/SalamanderGrandPiano the manifest is for.
pub const COMMIT: &str = "3382bf9496bba2486f5ab0de55a264d1dfc38404";
/// Roughly what's downloaded, and what it takes on disk.
pub const DOWNLOAD_MB: u32 = 720;
pub const INSTALLED_MB: u32 = 750;
/// Who to credit (CC BY 3.0).
pub const CREDIT: &str = "Salamander Grand Piano V3 by Alexander Holm (CC BY 3.0)";

/// `<sha256>  <path>` per file, as `sha256sum` writes it.
const MANIFEST: &str = include_str!("../salamander.sha256");

fn archive_url() -> String {
    format!("https://codeload.github.com/sfzinstruments/SalamanderGrandPiano/tar.gz/{COMMIT}")
}

/// The files and their hashes.
fn manifest() -> impl Iterator<Item = (&'static str, &'static str)> {
    MANIFEST.lines().filter_map(|line| line.split_once("  ").map(|(hash, path)| (path, hash)))
}

/// How many files an install writes.
pub fn file_count() -> u32 {
    manifest().count() as u32
}

/// Whether every sample file is in `dir` (a manual `git clone` counts too).
pub fn is_installed(dir: &Path) -> bool {
    manifest().all(|(path, _)| dir.join(path).is_file())
}

/// An install's progress, shared with whoever watches it.
#[derive(Debug, Default)]
pub struct Progress {
    /// Files written and checked so far, of [`file_count`].
    pub files: AtomicU32,
    /// Bytes downloaded so far.
    pub bytes: AtomicU64,
    /// Set to stop the install; it then fails with "cancelled".
    pub cancel: AtomicBool,
}

/// Downloads and installs the samples into `dir` (replacing whatever is
/// there once the new copy is complete and checked).
pub fn install(dir: &Path, progress: &Progress) -> Result<()> {
    let staging = sibling(dir, "download")?;
    if staging.exists() {
        std::fs::remove_dir_all(&staging).with_context(|| format!("removing {}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let result = unpack(&staging, progress);
    if let Err(err) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(err);
    }
    // Swap it in: the old copy aside first, so a failure leaves one of them.
    if dir.exists() {
        let old = sibling(dir, "old")?;
        let _ = std::fs::remove_dir_all(&old);
        std::fs::rename(dir, &old).with_context(|| format!("moving {} aside", dir.display()))?;
        std::fs::rename(&staging, dir).with_context(|| format!("moving the samples into {}", dir.display()))?;
        let _ = std::fs::remove_dir_all(&old);
    } else {
        std::fs::rename(&staging, dir).with_context(|| format!("moving the samples into {}", dir.display()))?;
    }
    Ok(())
}

/// `dir` with `.suffix` added: `…/salamander.download`.
fn sibling(dir: &Path, suffix: &str) -> Result<PathBuf> {
    let name = dir.file_name().context("no folder name")?.to_string_lossy();
    Ok(dir.with_file_name(format!("{name}.{suffix}")))
}

fn unpack(staging: &Path, progress: &Progress) -> Result<()> {
    let mut expected: HashMap<&str, &str> = manifest().collect();
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(std::sync::Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder().tls_config(tls).build().into();
    let response = agent.get(&archive_url()).call().context("downloading the samples")?;
    let body = Counting { inner: response.into_body().into_reader(), progress };
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(body));
    for entry in archive.entries().context("reading the download")? {
        let mut entry = entry.context("reading the download")?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        // Inside the tarball's one top folder (`SalamanderGrandPiano-<commit>/`).
        let path = entry.path().context("reading the download")?.into_owned();
        let relative: PathBuf = path.components().skip(1).collect();
        let Some(name) = relative.to_str().map(|p| p.replace('\\', "/")) else { continue };
        // Only listed files are written, so nothing lands outside `staging`.
        let Some(hash) = expected.remove(name.as_str()) else { continue };
        debug_assert!(relative.components().all(|c| matches!(c, Component::Normal(_))));
        let target = staging.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = std::fs::File::create(&target).with_context(|| format!("writing {}", target.display()))?;
        let mut writer = Hashing { inner: std::io::BufWriter::new(file), hasher: Sha256::new() };
        std::io::copy(&mut entry, &mut writer).with_context(|| format!("writing {}", target.display()))?;
        writer.inner.flush()?;
        let actual = format!("{:x}", writer.hasher.finalize());
        if actual != hash {
            bail!("{name} doesn't match its checksum (a damaged download?)");
        }
        progress.files.fetch_add(1, Relaxed);
    }
    if progress.cancel.load(Relaxed) {
        bail!("cancelled");
    }
    if let Some(missing) = expected.keys().next() {
        bail!("the download lacks {} files (e.g. {missing})", expected.len());
    }
    Ok(())
}

/// Counts what's read, and fails once cancelled.
struct Counting<'a, R> {
    inner: R,
    progress: &'a Progress,
}

impl<R: Read> Read for Counting<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.progress.cancel.load(Relaxed) {
            return Err(std::io::Error::other("cancelled"));
        }
        let n = self.inner.read(buf)?;
        self.progress.bytes.fetch_add(n as u64, Relaxed);
        Ok(n)
    }
}

struct Hashing<W> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_lists_the_sfz_data_and_samples() {
        let paths: Vec<&str> = manifest().map(|(path, _)| path).collect();
        assert_eq!(paths.len(), file_count() as usize);
        assert!(paths.contains(&"Data/notes.txt") && paths.contains(&"Samples/C4v16.flac"));
        assert!(manifest().all(|(path, hash)| hash.len() == 64 && !path.contains("..") && !path.starts_with('/')));
        // Every file the piano's SFZ includes is in it.
        for name in ["tune_nat", "tune_ret", "notes", "str_res", "hammer", "pedal", "region", "vel_01", "vel_16"] {
            assert!(paths.contains(&format!("Data/{name}.txt").as_str()), "{name}");
        }
    }
}
