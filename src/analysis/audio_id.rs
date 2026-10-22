//! `AudioId`: the identity of the encoded audio of a file (`docs/track-sheets.md`, "Identity").
//!
//! SHA-256 over a fixed header made of the default audio track's codec parameters, then the
//! data of every packet of that track in order, as symphonia's demuxer hands them out. Nothing
//! is decoded, and container metadata (tags, cover art) is not in the packets, so a rename, a
//! move or a tag edit keeps the id. Written `a1:` + the first 128 bits in hex.
//!
//! The header is, all integers u32 little-endian, an absent value written as 0:
//! `b"codj-audio-id-1"`, sample rate, channel count, bits per sample, extra-data length, then
//! the extra data. The codec id is left out on purpose: symphonia numbers codecs itself and a
//! renumbering must not change every id; the packet bytes already differ between codecs.
//! Packets are concatenated without lengths, so a demuxer that cuts raw PCM into packets of a
//! different size gives the same id.
//!
//! The index keeps `(path, size, mtime) -> id`, so a file is hashed again only when it
//! changes; `id -> paths` is derived from it.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use symphonia::{
    core::{
        codecs::audio::AudioCodecParameters,
        formats::{probe::Hint, TrackType},
        io::{MediaSourceStream, MediaSourceStreamOptions},
        meta::MetadataOptions,
    },
    default::get_probe,
};

/// The scheme prefix: `a1:` + 32 hex digits
pub const PREFIX: &str = "a1:";

/// Whether `s` is written like an AudioId (`a1:` + 32 lowercase hex digits)
pub fn is_audio_id(s: &str) -> bool {
    s.strip_prefix(PREFIX)
        .is_some_and(|h| h.len() == 32 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
}

/// Feeds the header and the packets of one track
pub struct IdHasher(Sha256);

impl IdHasher {
    pub fn new(params: &AudioCodecParameters) -> Self {
        let mut h = Sha256::new();
        h.update(b"codj-audio-id-1");
        let word = |h: &mut Sha256, v: u32| h.update(v.to_le_bytes());
        word(&mut h, params.sample_rate.unwrap_or(0));
        word(&mut h, params.channels.as_ref().map_or(0, |c| c.count() as u32));
        word(&mut h, params.bits_per_sample.unwrap_or(0));
        let extra = params.extra_data.as_deref().unwrap_or(&[]);
        word(&mut h, extra.len() as u32);
        h.update(extra);
        Self(h)
    }

    pub fn packet(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    pub fn finish(self) -> String {
        let digest = self.0.finalize();
        let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
        format!("{PREFIX}{hex}")
    }
}

/// The AudioId of `path`, demuxing only (no decoding). Stops at the end of the stream or at a
/// demuxer error after the first packet, like the analysis pass.
pub fn compute(path: &Path) -> Result<String, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), MediaSourceStreamOptions::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut format = get_probe()
        .probe(&hint, mss, Default::default(), MetadataOptions::default())
        .map_err(|e| e.to_string())?;
    let track = format.default_track(TrackType::Audio).ok_or("no audio track")?;
    let track_id = track.id;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or("not an audio track")?.clone();
    let mut hasher = IdHasher::new(&params);
    let mut packets = 0u64;
    loop {
        match format.next_packet() {
            Ok(Some(p)) => {
                if p.track_id == track_id {
                    hasher.packet(&p.data);
                    packets += 1;
                }
            }
            Ok(None) => break,
            Err(_) if packets > 0 => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    if packets == 0 {
        return Err("no audio packets".into());
    }
    Ok(hasher.finish())
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Entry {
    size: u64,
    mtime: u64,
    id: String,
}

/// `(path, size, mtime) -> AudioId`, and the paths of an id
#[derive(Serialize, Deserialize, Default, Debug)]
pub struct IdIndex {
    files: BTreeMap<PathBuf, Entry>,
    #[serde(skip)]
    by_id: BTreeMap<String, BTreeSet<PathBuf>>,
    #[serde(skip)]
    dirty: bool,
}

impl IdIndex {
    /// Reads the index file; a missing or unreadable one is an empty index
    pub fn load(file: Option<&Path>) -> Self {
        let mut index: Self = file
            .and_then(|f| std::fs::read(f).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        index.files.retain(|_, e| is_audio_id(&e.id));
        index.rebuild();
        index
    }

    fn rebuild(&mut self) {
        self.by_id.clear();
        for (p, e) in &self.files {
            self.by_id.entry(e.id.clone()).or_default().insert(p.clone());
        }
    }

    /// The id of `path` if it was hashed at this size and mtime
    pub fn get(&self, path: &Path, (size, mtime): (u64, u64)) -> Option<&str> {
        self.files.get(path).filter(|e| e.size == size && e.mtime == mtime).map(|e| e.id.as_str())
    }

    pub fn insert(&mut self, path: &Path, (size, mtime): (u64, u64), id: String) {
        if let Some(old) = self.files.get(path) {
            if old.size == size && old.mtime == mtime && old.id == id {
                return;
            }
            if let Some(set) = self.by_id.get_mut(&old.id) {
                set.remove(path);
                if set.is_empty() {
                    self.by_id.remove(&old.id);
                }
            }
        }
        self.by_id.entry(id.clone()).or_default().insert(path.to_path_buf());
        self.files.insert(path.to_path_buf(), Entry { size, mtime, id });
        self.dirty = true;
    }

    /// Every path recorded for `id` (whether the file is still unchanged is the caller's check)
    pub fn paths(&self, id: &str) -> Vec<PathBuf> {
        self.by_id.get(id).map(|s| s.iter().cloned().collect()).unwrap_or_default()
    }

    /// Writes the index atomically if it changed since the last save
    pub fn save(&mut self, file: &Path) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, file)?;
        self.dirty = false;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A 16-bit mono WAV of `samples`, with an optional LIST/INFO chunk (a tag) before the data
    pub fn write_wav(path: &Path, samples: &[i16], tag: Option<&str>) {
        let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut list = Vec::new();
        if let Some(t) = tag {
            let mut text = t.as_bytes().to_vec();
            text.push(0);
            if text.len() % 2 == 1 {
                text.push(0);
            }
            list.extend_from_slice(b"LIST");
            list.extend_from_slice(&(4 + 8 + text.len() as u32).to_le_bytes());
            list.extend_from_slice(b"INFOINAM");
            list.extend_from_slice(&(text.len() as u32).to_le_bytes());
            list.extend_from_slice(&text);
        }
        let rate = 22050u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(4 + 24 + list.len() as u32 + 8 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(&list);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        std::fs::write(path, out).unwrap();
    }

    pub fn noise(n: usize, seed: u32) -> Vec<i16> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1103515245).wrapping_add(12345);
                ((s >> 16) as i16) / 4
            })
            .collect()
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("codjd-audioid-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn tags_and_names_keep_the_id_and_the_audio_changes_it() {
        let dir = tmp("ids");
        let samples = noise(22050, 7);
        let a = dir.join("a.wav");
        write_wav(&a, &samples, None);
        let id = compute(&a).unwrap();
        assert!(is_audio_id(&id), "{id}");
        assert_eq!(id.len(), 3 + 32);
        // the same audio with a tag, under another name
        let b = dir.join("renamed copy.wav");
        write_wav(&b, &samples, Some("Lollipops & Limousines"));
        assert_ne!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        assert_eq!(compute(&b).unwrap(), id);
        // one sample different
        let mut other = samples.clone();
        other[1000] = other[1000].wrapping_add(1);
        let c = dir.join("c.wav");
        write_wav(&c, &other, None);
        assert_ne!(compute(&c).unwrap(), id);
        // not audio
        let d = dir.join("d.wav");
        std::fs::write(&d, b"nope").unwrap();
        assert!(compute(&d).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn id_syntax() {
        assert!(is_audio_id("a1:3f9c27d0e1b44a58c0de11aa22bb33cc"));
        assert!(!is_audio_id("a1:3f9c27d0e1b44a58c0de11aa22bb33c"));
        assert!(!is_audio_id("a1:3F9C27D0E1B44A58C0DE11AA22BB33CC"));
        assert!(!is_audio_id("a2:3f9c27d0e1b44a58c0de11aa22bb33cc"));
    }

    #[test]
    fn the_index_follows_changes_and_survives_a_reload() {
        let dir = tmp("index");
        let file = dir.join("audio-ids.json");
        let id1 = format!("{PREFIX}{}", "1".repeat(32));
        let id2 = format!("{PREFIX}{}", "2".repeat(32));
        let mut ix = IdIndex::load(Some(&file));
        ix.insert(Path::new("/m/a.flac"), (10, 1), id1.clone());
        ix.insert(Path::new("/m/copy.flac"), (10, 2), id1.clone());
        assert_eq!(ix.get(Path::new("/m/a.flac"), (10, 1)), Some(id1.as_str()));
        // a changed file is not known until hashed again
        assert_eq!(ix.get(Path::new("/m/a.flac"), (11, 1)), None);
        assert_eq!(ix.paths(&id1), [PathBuf::from("/m/a.flac"), PathBuf::from("/m/copy.flac")]);
        ix.insert(Path::new("/m/a.flac"), (11, 3), id2.clone());
        assert_eq!(ix.paths(&id1), [PathBuf::from("/m/copy.flac")]);
        ix.save(&file).unwrap();
        let back = IdIndex::load(Some(&file));
        assert_eq!(back.paths(&id2), [PathBuf::from("/m/a.flac")]);
        assert_eq!(back.get(Path::new("/m/copy.flac"), (10, 2)), Some(id1.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
