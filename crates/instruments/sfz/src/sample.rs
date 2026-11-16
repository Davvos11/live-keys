//! Sample files: decoding, and the part of each sample kept in memory.
//!
//! A sample is either held whole (short ones, or everything when preloading),
//! or only its first frames (its *head*), with the rest streamed from disk while
//! a voice plays it (see `stream.rs`). Both are laid out the same way —
//! interleaved, with `PAD_FRONT` silent frames before frame 0 and, once
//! complete, `PAD_BACK` after the last — so a voice reads either with the same
//! index: frame `i - 1` is stored at frame slot `i`.

use crate::stream::StreamData;
use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

/// Zero frames stored before and after the data, so 4-point interpolation can
/// read one frame back and two ahead without bounds checks.
pub(crate) const PAD_FRONT: usize = 1;
pub(crate) const PAD_BACK: usize = 3;

/// A sample as held in memory: mono or stereo (further channels are dropped).
#[derive(Debug, Clone)]
pub struct Sample {
    channels: usize,
    frames: usize,
    rate: f32,
    /// Interleaved, `PAD_FRONT` silent frames first; if `streamed` is false it
    /// holds every frame and `PAD_BACK` silent ones after.
    head: Vec<f32>,
    streamed: bool,
    /// The streamer's slot for it, if streamed.
    pub(crate) slot: u32,
}

impl Sample {
    /// A complete sample from interleaved samples.
    pub fn from_interleaved(channels: usize, rate: f32, samples: &[f32]) -> Sample {
        assert!(channels == 1 || channels == 2);
        let frames = samples.len() / channels;
        let mut head = vec![0.0; (PAD_FRONT + frames + PAD_BACK) * channels];
        head[PAD_FRONT * channels..(PAD_FRONT + frames) * channels].copy_from_slice(&samples[..frames * channels]);
        Sample { channels, frames, rate, head, streamed: false, slot: 0 }
    }

    /// Loads a sample: whole if `head_frames` is `None` or the file is no longer
    /// than that, else only its first `head_frames` (or a little more).
    pub fn load(path: &Path, head_frames: Option<usize>) -> Result<Sample> {
        Self::load_inner(path, head_frames).with_context(|| format!("loading {}", path.display()))
    }

    fn load_inner(path: &Path, head_frames: Option<usize>) -> Result<Sample> {
        let mut decoder = Decoder::open(path)?;
        let channels = decoder.channels;
        let rate = decoder.rate;
        // Without a known length the stream couldn't be laid out: load it whole.
        let limit = match (head_frames, decoder.frames) {
            (Some(head), Some(_)) => head,
            _ => usize::MAX,
        };
        let mut data = vec![0.0; PAD_FRONT * channels];
        let mut decoded = 0;
        let mut complete = false;
        while decoded < limit {
            let n = decoder.read(&mut data)?;
            if n == 0 {
                complete = true;
                break;
            }
            decoded += n;
        }
        if !complete && decoder.frames.is_some_and(|f| decoded >= f) {
            complete = true;
        }
        if complete {
            data.resize((PAD_FRONT + decoded + PAD_BACK) * channels, 0.0);
            data.shrink_to_fit();
            return Ok(Sample { channels, frames: decoded, rate, head: data, streamed: false, slot: 0 });
        }
        data.shrink_to_fit();
        Ok(Sample { channels, frames: decoder.frames.unwrap_or(decoded), rate, head: data, streamed: true, slot: 0 })
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

    /// Whether only the head is held, and the rest is streamed.
    pub fn is_streamed(&self) -> bool {
        self.streamed
    }

    /// Bytes held in memory (the head, or the whole sample).
    pub fn memory(&self) -> usize {
        self.head.len() * size_of::<f32>()
    }

    /// The interleaved data from frame `index - 1` on, at least 4 frames of it,
    /// for `index` in `0..=frames`: from the head, or else from the streamed
    /// data if it has been decoded that far. `None` when it hasn't (an underrun).
    #[inline]
    pub(crate) fn window<'a>(&'a self, index: usize, stream: Option<&'a StreamData>) -> Option<&'a [f32]> {
        let start = index * self.channels;
        let end = start + 4 * self.channels;
        if end <= self.head.len() {
            return Some(&self.head[start..]);
        }
        let data = stream?.published();
        (end <= data.len()).then(|| &data[start..])
    }
}

/// Decodes a sample file block by block, as interleaved `f32` (at most two
/// channels).
pub(crate) struct Decoder {
    kind: DecoderKind,
    /// Channels in the file, and kept (at most 2).
    file_channels: usize,
    pub(crate) channels: usize,
    pub(crate) rate: f32,
    /// Total frames, if the file says.
    pub(crate) frames: Option<usize>,
    scale: f32,
}

enum DecoderKind {
    Flac { reader: claxon::FlacReader<File>, buffer: Vec<i32> },
    Wav { reader: hound::WavReader<BufReader<File>>, float: bool },
}

/// Frames a WAV read returns at most (FLAC returns one block).
const WAV_CHUNK: usize = 4096;

impl Decoder {
    pub(crate) fn open(path: &Path) -> Result<Decoder> {
        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        match extension.as_str() {
            "flac" => {
                let reader = claxon::FlacReader::open(path)?;
                let info = reader.streaminfo();
                let file_channels = info.channels as usize;
                Ok(Decoder {
                    file_channels,
                    channels: file_channels.min(2),
                    rate: info.sample_rate as f32,
                    frames: info.samples.filter(|&n| n > 0).map(|n| n as usize),
                    scale: 1.0 / (1u64 << (info.bits_per_sample - 1)) as f32,
                    kind: DecoderKind::Flac { reader, buffer: Vec::new() },
                })
            }
            "wav" | "wave" => {
                let reader = hound::WavReader::open(path)?;
                let spec = reader.spec();
                let file_channels = spec.channels as usize;
                if file_channels == 0 {
                    bail!("no channels");
                }
                let float = spec.sample_format == hound::SampleFormat::Float;
                Ok(Decoder {
                    file_channels,
                    channels: file_channels.min(2),
                    rate: spec.sample_rate as f32,
                    frames: Some(reader.duration() as usize),
                    scale: if float { 1.0 } else { 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32 },
                    kind: DecoderKind::Wav { reader, float },
                })
            }
            _ => bail!("unsupported sample format: {}", path.display()),
        }
    }

    /// Appends the next frames to `out`; returns how many (0 at the end).
    pub(crate) fn read(&mut self, out: &mut Vec<f32>) -> Result<usize> {
        let scale = self.scale;
        match &mut self.kind {
            DecoderKind::Flac { reader, buffer } => {
                let Some(block) = reader.blocks().read_next_or_eof(std::mem::take(buffer))? else {
                    return Ok(0);
                };
                let frames = block.duration() as usize;
                let left = block.channel(0);
                if self.channels == 2 {
                    let right = block.channel(1);
                    out.reserve(frames * 2);
                    for (&l, &r) in left.iter().zip(right) {
                        out.push(l as f32 * scale);
                        out.push(r as f32 * scale);
                    }
                } else {
                    out.extend(left.iter().map(|&s| s as f32 * scale));
                }
                *buffer = block.into_buffer();
                Ok(frames)
            }
            DecoderKind::Wav { reader, float } => {
                let wanted = WAV_CHUNK * self.file_channels;
                let before = out.len();
                let mut index = 0;
                let mut push = |value: f32, out: &mut Vec<f32>| {
                    if index % self.file_channels < self.channels {
                        out.push(value);
                    }
                    index += 1;
                };
                if *float {
                    for s in reader.samples::<f32>().take(wanted) {
                        push(s?, out);
                    }
                } else {
                    for s in reader.samples::<i32>().take(wanted) {
                        push(s? as f32 * scale, out);
                    }
                }
                Ok((out.len() - before) / self.channels)
            }
        }
    }
}
