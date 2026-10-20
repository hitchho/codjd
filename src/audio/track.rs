//! A track decoded into RAM, readable by the audio thread while it's still being decoded.
//!
//! Samples are stereo `f32` frames at the file's own sample rate, stored in fixed-size chunks.
//! The decoder thread allocates a chunk, fills it, publishes its pointer, then advances
//! `decoded`; the audio thread only reads frames below `decoded`. No locks, and the audio thread
//! never allocates or frees (dropping the last `Arc<Track>` happens off the audio thread).

use std::{
    fs::File,
    path::Path,
    ptr,
    sync::{
        atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering},
        Arc,
    },
};

use symphonia::{
    core::{
        codecs::audio::AudioDecoderOptions,
        errors::Error,
        formats::{probe::Hint, TrackType},
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::MetadataOptions,
    },
    default::{get_codecs, get_probe},
};
use tracing::warn;

/// Frames per chunk (2^16, about 1.5 s at 44.1 kHz)
const CHUNK_SHIFT: usize = 16;
const CHUNK_FRAMES: usize = 1 << CHUNK_SHIFT;
/// Longest track we keep: 6 hours at 192 kHz
const MAX_CHUNKS: usize = (6 * 3600 * 192_000) / CHUNK_FRAMES + 1;

pub struct Track {
    /// sample rate of the file
    pub rate: u32,
    chunks: Box<[AtomicPtr<f32>]>,
    /// frames that are decoded and readable
    decoded: AtomicUsize,
    /// the whole file is decoded (or decoding stopped on an error)
    done: AtomicBool,
    /// set to stop a decode that's no longer needed
    cancel: AtomicBool,
}

impl Track {
    fn new(rate: u32) -> Self {
        Self {
            rate,
            chunks: (0..MAX_CHUNKS).map(|_| AtomicPtr::new(ptr::null_mut())).collect(),
            decoded: AtomicUsize::new(0),
            done: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
        }
    }

    /// Opens `path` and starts decoding it on its own thread; frames become readable as they're
    /// decoded. Fails only if the file can't be opened or has no playable audio.
    pub fn open(path: &Path) -> Result<Arc<Track>, String> {
        let file = File::open(path).map_err(|e| e.to_string())?;
        let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let mut format = get_probe()
            .probe(&hint, mss, Default::default(), MetadataOptions::default())
            .map_err(|e| e.to_string())?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or("no audio track")?;
        let track_id = track.id;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or("not an audio track")?
            .clone();
        let rate = params.sample_rate.ok_or("unknown sample rate")?;
        let mut decoder = get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|e| e.to_string())?;

        let out = Arc::new(Track::new(rate));
        let writer = Arc::clone(&out);
        let name = path.display().to_string();
        std::thread::Builder::new()
            .name("decode".into())
            .spawn(move || {
                let mut interleaved: Vec<f32> = Vec::new();
                let mut errors = 0;
                loop {
                    if writer.cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let packet = match format.next_packet() {
                        Ok(Some(packet)) => packet,
                        Ok(None) => break,
                        Err(e) => {
                            warn!(file = %name, error = %e, "decode stopped");
                            break;
                        }
                    };
                    if packet.track_id != track_id {
                        continue;
                    }
                    let decoded = match decoder.decode(&packet) {
                        Ok(decoded) => decoded,
                        Err(Error::DecodeError(_)) if errors < 10 => {
                            errors += 1;
                            continue;
                        }
                        Err(e) => {
                            warn!(file = %name, error = %e, "decode stopped");
                            break;
                        }
                    };
                    let channels = decoded.spec().channels().count().max(1);
                    interleaved.resize(decoded.samples_interleaved(), 0.);
                    decoded.copy_to_slice_interleaved(&mut interleaved);
                    writer.push(&interleaved, channels);
                }
                writer.done.store(true, Ordering::Release);
            })
            .map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// Appends interleaved frames with `channels` channels (mono is doubled, extra channels
    /// beyond the first two are dropped)
    fn push(&self, samples: &[f32], channels: usize) {
        let mut at = self.decoded.load(Ordering::Relaxed);
        for frame in samples.chunks_exact(channels) {
            let (l, r) = match frame {
                [m] => (*m, *m),
                [l, r, ..] => (*l, *r),
                [] => continue,
            };
            let chunk = at >> CHUNK_SHIFT;
            if chunk >= MAX_CHUNKS {
                return;
            }
            let mut data = self.chunks[chunk].load(Ordering::Acquire);
            if data.is_null() {
                let fresh: Box<[f32]> = vec![0.; CHUNK_FRAMES * 2].into_boxed_slice();
                data = Box::into_raw(fresh) as *mut f32;
                self.chunks[chunk].store(data, Ordering::Release);
            }
            let i = (at & (CHUNK_FRAMES - 1)) * 2;
            // SAFETY: the chunk has CHUNK_FRAMES*2 samples, `i` is within it, and readers only
            // look at frames below `decoded`, which is advanced after the write
            unsafe {
                *data.add(i) = l;
                *data.add(i + 1) = r;
            }
            at += 1;
        }
        self.decoded.store(at, Ordering::Release);
    }

    /// Frames decoded so far
    pub fn decoded(&self) -> usize {
        self.decoded.load(Ordering::Acquire)
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    /// Stops decoding (the track is being replaced)
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The stereo frame at `index`, if it's decoded
    #[inline]
    pub fn frame(&self, index: usize) -> Option<(f32, f32)> {
        if index >= self.decoded.load(Ordering::Acquire) {
            return None;
        }
        let data = self.chunks[index >> CHUNK_SHIFT].load(Ordering::Acquire);
        let i = (index & (CHUNK_FRAMES - 1)) * 2;
        // SAFETY: index < decoded, so its chunk is allocated, published and filled up to here
        unsafe { Some((*data.add(i), *data.add(i + 1))) }
    }

    /// A track made from samples in memory (tests, generated sounds)
    #[cfg(test)]
    pub fn from_frames(rate: u32, frames: &[(f32, f32)]) -> Arc<Track> {
        let track = Track::new(rate);
        let flat: Vec<f32> = frames.iter().flat_map(|(l, r)| [*l, *r]).collect();
        track.push(&flat, 2);
        track.done.store(true, Ordering::Release);
        Arc::new(track)
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        for chunk in self.chunks.iter() {
            let data = chunk.load(Ordering::Acquire);
            if !data.is_null() {
                // SAFETY: allocated in push() as a boxed slice of CHUNK_FRAMES*2 samples
                unsafe {
                    drop(Box::from_raw(ptr::slice_from_raw_parts_mut(
                        data,
                        CHUNK_FRAMES * 2,
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_cross_chunk_boundaries() {
        let n = CHUNK_FRAMES + 10;
        let frames: Vec<(f32, f32)> = (0..n).map(|i| (i as f32, -(i as f32))).collect();
        let t = Track::from_frames(44_100, &frames);
        assert_eq!(t.decoded(), n);
        assert_eq!(t.frame(0), Some((0., -0.)));
        assert_eq!(t.frame(CHUNK_FRAMES), Some((CHUNK_FRAMES as f32, -(CHUNK_FRAMES as f32))));
        assert_eq!(t.frame(n - 1), Some(((n - 1) as f32, -((n - 1) as f32))));
        assert_eq!(t.frame(n), None);
    }

    #[test]
    fn mono_is_doubled() {
        let t = Track::new(8000);
        t.push(&[0.5, 0.25], 1);
        assert_eq!(t.frame(0), Some((0.5, 0.5)));
        assert_eq!(t.frame(1), Some((0.25, 0.25)));
    }
}
