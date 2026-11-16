//! Sample files, decoded into memory.
//!
//! For now every sample is decoded whole, as `f32`, which for Salamander is ~2 GB.
//! Streaming from disk (keeping only each sample's start in memory) is the next
//! step; `Sample::frame` is the one place voices read from, so it can change
//! underneath them.

use anyhow::{Context, Result, bail};
use std::path::Path;

/// Zero frames stored before and after the data, so 4-point interpolation can
/// read one frame back and two ahead without bounds checks.
pub(crate) const PAD_FRONT: usize = 1;
pub(crate) const PAD_BACK: usize = 3;

/// A decoded sample: mono or stereo (further channels are dropped).
#[derive(Debug, Clone)]
pub struct Sample {
    channels: usize,
    frames: usize,
    rate: f32,
    /// Interleaved, with `PAD_FRONT` and `PAD_BACK` silent frames around it.
    data: Vec<f32>,
}

impl Sample {
    /// From interleaved samples.
    pub fn from_interleaved(channels: usize, rate: f32, samples: &[f32]) -> Sample {
        assert!(channels == 1 || channels == 2);
        let frames = samples.len() / channels;
        let mut data = vec![0.0; (PAD_FRONT + frames + PAD_BACK) * channels];
        data[PAD_FRONT * channels..(PAD_FRONT + frames) * channels].copy_from_slice(&samples[..frames * channels]);
        Sample { channels, frames, rate, data }
    }

    pub fn load(path: &Path) -> Result<Sample> {
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        match extension.as_str() {
            "flac" => load_flac(path),
            "wav" | "wave" => load_wav(path),
            _ => bail!("unsupported sample format: {}", path.display()),
        }
        .with_context(|| format!("loading {}", path.display()))
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// Frames per second.
    pub fn rate(&self) -> f32 {
        self.rate
    }

    /// Bytes held in memory.
    pub fn memory(&self) -> usize {
        self.data.len() * size_of::<f32>()
    }

    /// The interleaved data starting at frame `index - 1`: at least 4 frames
    /// for `index` in `0..=frames`.
    #[inline]
    pub(crate) fn window(&self, index: usize) -> &[f32] {
        // Frame `index - 1` is stored at `index - 1 + PAD_FRONT` = `index`.
        &self.data[index * self.channels..]
    }
}

fn load_flac(path: &Path) -> Result<Sample> {
    let mut reader = claxon::FlacReader::open(path)?;
    let info = reader.streaminfo();
    let channels = info.channels as usize;
    let used = channels.min(2);
    let scale = 1.0 / (1u64 << (info.bits_per_sample - 1)) as f32;
    let mut samples = Vec::with_capacity(info.samples.unwrap_or(0) as usize * used);
    let mut blocks = reader.blocks();
    let mut buffer = Vec::new();
    while let Some(block) = blocks.read_next_or_eof(std::mem::take(&mut buffer))? {
        let left = block.channel(0);
        if used == 2 {
            let right = block.channel(1);
            for (&l, &r) in left.iter().zip(right) {
                samples.push(l as f32 * scale);
                samples.push(r as f32 * scale);
            }
        } else {
            samples.extend(left.iter().map(|&s| s as f32 * scale));
        }
        buffer = block.into_buffer();
    }
    Ok(Sample::from_interleaved(used, info.sample_rate as f32, &samples))
}

fn load_wav(path: &Path) -> Result<Sample> {
    let mut reader = hound::WavReader::open(path)?;
    let spec = reader.spec();
    let channels = spec.channels as usize;
    if channels == 0 {
        bail!("no channels");
    }
    let used = channels.min(2);
    let all: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.map(|s| s as f32 * scale)).collect::<Result<_, _>>()?
        }
    };
    let samples: Vec<f32> = if channels == used {
        all
    } else {
        all.chunks_exact(channels).flat_map(|frame| frame[..used].iter().copied()).collect()
    };
    Ok(Sample::from_interleaved(used, spec.sample_rate as f32, &samples))
}
