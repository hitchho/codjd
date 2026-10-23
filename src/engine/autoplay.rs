//! Autoplay: the music never runs out. With it on, whenever fewer than [`MIN_QUEUE`] tracks are
//! queued while the player plays, codjd queues the library track that blends best after the last
//! one: a close tempo (within 4 %, then 8 % if nothing fits), a compatible key on the Camelot
//! wheel, a steady beat grid, and not played recently (copies of a song count as one song). The
//! picks are ordinary queue entries: they show in the queue, can be removed or moved, and automix
//! mixes into them like anything else.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use tracing::info;

use super::{Engine, PlayingStatus};
use crate::types::Music;

/// Autoplay tops the queue up to this many tracks
pub const MIN_QUEUE: usize = 2;
/// Songs remembered as played, so the night doesn't loop
const HISTORY: usize = 200;
/// Tracks shorter than this are skipped (intros, skits, snippets)
const MIN_LENGTH: Duration = Duration::from_secs(120);
/// A pick is tried at most this often (the library is scanned for it)
const PICK_EVERY: Duration = Duration::from_secs(10);
/// Picks stay within this share of the set's tempo (the tempo when autoplay made its first pick):
/// small steps that each blend must not add up to another genre over an evening
const BAND: f64 = 0.10;

pub struct State {
    pub on: bool,
    /// songs played or queued lately, newest last
    recent: VecDeque<SongKey>,
    last_try: Option<Instant>,
    /// why the last pick was made, for `AutoplayState`
    pub last: String,
    /// the set's tempo: the track's when autoplay first picked; cleared when autoplay is turned on
    anchor: Option<f64>,
}

impl State {
    pub fn new(on: bool) -> State {
        State { on, recent: VecDeque::new(), last_try: None, last: String::new(), anchor: None }
    }

    fn remember(&mut self, key: SongKey) {
        if self.recent.back() != Some(&key) {
            self.recent.retain(|k| *k != key);
            self.recent.push_back(key);
            while self.recent.len() > HISTORY {
                self.recent.pop_front();
            }
        }
    }
}

/// One song whatever the copy: the title and the length to 6 s. Not the artist: the same release
/// is credited "Aasi" in one copy and "Aasi & Olympe 4000" in another
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SongKey(String, u64);

pub fn song_key(m: &Music) -> SongKey {
    SongKey(m.title.trim().to_lowercase(), (m.length.as_secs() + 3) / 6)
}

/// What the picker knows about a track
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub index: u32,
    pub key: SongKey,
    pub bpm: f64,
    /// Camelot, "" when unknown
    pub camelot: String,
    /// a grid that sync can trust: analysed, not variable, no half/double doubt
    pub steady: bool,
    pub length: Duration,
}

fn camelot(k: &str) -> Option<(u8, char)> {
    let (num, letter) = k.split_at(k.len().checked_sub(1)?);
    let n: u8 = num.parse().ok()?;
    let l = letter.chars().next()?.to_ascii_uppercase();
    ((1..=12).contains(&n) && (l == 'A' || l == 'B')).then_some((n, l))
}

/// Keys that mix without clashing: the same, one step around the wheel, or the relative
/// major/minor (same number, other letter)
pub fn keys_blend(a: &str, b: &str) -> bool {
    match (camelot(a), camelot(b)) {
        (Some((na, la)), Some((nb, lb))) if la == lb => {
            let d = (na as i16 - nb as i16).rem_euclid(12);
            d.min(12 - d) <= 1
        }
        (Some((na, _)), Some((nb, _))) => na == nb,
        _ => false,
    }
}

/// The best next track after one at `bpm` in `camelot`, and why, keeping to `anchor`'s tempo band.
/// Pure: the same inputs always give the same pick (ties go to the earlier track in the list).
pub fn pick(bpm: f64, camelot: &str, anchor: f64, candidates: &[Candidate], recent: &[SongKey]) -> Option<(u32, String)> {
    if bpm <= 0. || anchor <= 0. {
        return None;
    }
    let in_band = |b: f64| (b - anchor).abs() / anchor <= BAND;
    let fresh = |c: &&Candidate| c.steady && c.bpm > 0. && c.length >= MIN_LENGTH && !recent.contains(&c.key) && in_band(c.bpm);
    let gap = |c: &Candidate| (c.bpm - bpm).abs() / bpm;
    // a small lift beats a drop, the same key beats a neighbour
    let score = |c: &Candidate| gap(c) + if c.bpm >= bpm { 0. } else { 0.01 } + if c.camelot == camelot { 0. } else { 0.005 };
    let strict = candidates
        .iter()
        .filter(fresh)
        .filter(|c| gap(c) <= 0.04 && keys_blend(camelot, &c.camelot))
        .min_by(|a, b| score(a).total_cmp(&score(b)));
    if let Some(c) = strict {
        return Some((c.index, format!("{:.0} BPM, {} after {:.0} BPM, {}", c.bpm, c.camelot, bpm, camelot)));
    }
    // nothing blends strictly (or the last track is outside the set's band): the closest tempo in
    // the band, a compatible key first; automix keeps a far jump short
    let target = if in_band(bpm) { bpm } else { anchor };
    candidates
        .iter()
        .filter(fresh)
        .min_by(|a, b| {
            let rank = |c: &Candidate| (!keys_blend(camelot, &c.camelot), (c.bpm - target).abs());
            let (ka, da) = rank(a);
            let (kb, db) = rank(b);
            ka.cmp(&kb).then(da.total_cmp(&db))
        })
        .map(|c| (c.index, format!("{:.0} BPM, {} after {:.0} BPM, {} (nothing closer)", c.bpm, c.camelot, bpm, camelot)))
}

impl Engine {
    fn candidate(&self, index: u32, m: &Music) -> Option<Candidate> {
        let path = <&Path>::from(&m.path);
        let f = self.analyzer.as_ref()?.features(path)?;
        let g = f.grid()?;
        Some(Candidate {
            index,
            key: song_key(m),
            bpm: g.bpm * self.gridfix.factor(path),
            camelot: f.key().map(|k| k.camelot.clone()).unwrap_or_default(),
            steady: !g.variable && !g.octave_doubt && g.bpm > 0.,
            length: m.length,
        })
    }

    /// `Autoplay(on)`
    pub fn set_autoplay(&mut self, on: bool) {
        if on && !self.ap.on {
            // a new session: the set's tempo is taken from what plays when the first pick is made
            self.ap.anchor = None;
        }
        self.ap.on = on;
        info!(on, "autoplay");
    }

    /// (on, why the last track was picked)
    pub fn autoplay_state(&self) -> (bool, String) {
        (self.ap.on, self.ap.last.clone())
    }

    /// Called from the engine's tick: remembers what plays, tops the queue up
    pub(super) fn autoplay_tick(&mut self) {
        if let Some(m) = self.playlist.musics.get(self.playlist.playing_index as usize) {
            if matches!(self.status(), PlayingStatus::Playing) {
                let key = song_key(m);
                self.ap.remember(key);
            }
        }
        if !self.ap.on || !matches!(self.status(), PlayingStatus::Playing) || self.queue.len() >= MIN_QUEUE {
            return;
        }
        if self.ap.last_try.is_some_and(|t| t.elapsed() < PICK_EVERY) {
            return;
        }
        self.ap.last_try = Some(Instant::now());
        // after the last queued track, else after the one playing
        let last = self.queue.back().cloned().or_else(|| self.playlist.musics.get(self.playlist.playing_index as usize).cloned());
        let Some(last) = last else { return };
        let Some(after) = self.candidate(0, &last) else { return };
        for q in &self.queue {
            let key = song_key(q);
            self.ap.remember(key);
        }
        let candidates: Vec<Candidate> =
            self.playlist.musics.iter().enumerate().filter_map(|(i, m)| self.candidate(i as u32, m)).collect();
        let recent: Vec<SongKey> = self.ap.recent.iter().cloned().collect();
        let anchor = *self.ap.anchor.get_or_insert(after.bpm);
        match pick(after.bpm, &after.camelot, anchor, &candidates, &recent) {
            Some((index, why)) => {
                let title = self.playlist.musics[index as usize].title.clone();
                info!(%title, %why, "autoplay queued");
                self.ap.last = format!("{title}: {why}");
                let key = song_key(&self.playlist.musics[index as usize]);
                self.ap.remember(key);
                self.queue_add(index, false);
            }
            None => self.ap.last = format!("nothing in the library blends after {}", last.title),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(index: u32, title: &str, bpm: f64, key: &str) -> Candidate {
        Candidate { index, key: SongKey(title.into(), 50), bpm, camelot: key.into(), steady: true, length: Duration::from_secs(300) }
    }

    #[test]
    fn copies_with_other_credits_are_one_song() {
        let m = |title: &str, artist: &str, secs: u64| Music { title: title.into(), artist: artist.into(), length: Duration::from_secs(secs), ..Default::default() };
        assert_eq!(song_key(&m("We Never Work", "Aasi", 297)), song_key(&m("We Never Work", "Aasi & Olympe 4000", 297)));
        assert_eq!(song_key(&m("We Never Work", "Aasi", 297)), song_key(&m(" we never work", "", 299)));
        assert_ne!(song_key(&m("We Never Work", "Aasi", 297)), song_key(&m("We Never Work (Remix)", "Aasi", 297)));
        assert_ne!(song_key(&m("Intro", "x", 60)), song_key(&m("Intro", "y", 200)), "same title, another length: another song");
    }

    #[test]
    fn camelot_neighbours() {
        assert!(keys_blend("5A", "5A") && keys_blend("5A", "4A") && keys_blend("5A", "6A") && keys_blend("5A", "5B"));
        assert!(keys_blend("12A", "1A"), "the wheel wraps");
        assert!(!keys_blend("5A", "10A") && !keys_blend("5A", "4B") && !keys_blend("5A", "") && !keys_blend("x", "5A"));
    }

    #[test]
    fn picks_close_tempo_and_compatible_key_and_skips_what_played() {
        let lib = [c(0, "far key", 160., "10A"), c(1, "drop", 156., "5A"), c(2, "lift", 162., "5B"), c(3, "same", 160., "5A")];
        // the same key at the same tempo wins
        assert_eq!(pick(160., "5A", 160., &lib, &[]).unwrap().0, 3);
        // played: next best is a small lift in the relative major, not a drop or a far key
        let played = [lib[3].key.clone()];
        assert_eq!(pick(160., "5A", 160., &lib, &played).unwrap().0, 2);
        // unsteady grids, short tracks and too far tempos are never picked
        let mut odd = c(4, "odd", 160., "5A");
        odd.steady = false;
        let mut short = c(5, "short", 160., "5A");
        short.length = Duration::from_secs(60);
        assert!(pick(160., "5A", 160., &[odd, short, c(6, "far", 190., "5A")], &[]).is_none());
        // nothing blends strictly: the closest tempo within 8 % in any key
        let (i, why) = pick(160., "5A", 160., &[c(7, "b", 170., "9B"), c(8, "a", 166., "2A")], &[]).unwrap();
        assert_eq!(i, 8);
        assert!(why.contains("nothing closer"), "{why}");
        assert!(pick(0., "5A", 160., &lib, &[]).is_none());
    }

    #[test]
    fn the_set_keeps_its_tempo_and_comes_back_after_a_drift() {
        // a ladder of small drops, each within 4 % of the last: without the anchor it walks the
        // set from 150 down to hip hop over an evening
        let lib = [c(0, "a", 146., "5A"), c(1, "b", 141., "5A"), c(2, "c", 136., "5A"), c(3, "d", 131., "5A"), c(4, "e", 126., "5A")];
        let mut played = Vec::new();
        let mut bpm = 150.;
        let mut picks = Vec::new();
        while let Some((i, _)) = pick(bpm, "5A", 150., &lib, &played) {
            picks.push(lib[i as usize].bpm);
            played.push(lib[i as usize].key.clone());
            bpm = lib[i as usize].bpm;
        }
        assert!(picks.iter().all(|b| (b - 150.).abs() / 150. <= BAND), "{picks:?}");
        // a track far outside the band (94 BPM after a drift): the pick steers back into it
        let lib = [c(0, "hiphop", 92., "3A"), c(1, "back", 150., "3A"), c(2, "other", 148., "9B")];
        assert_eq!(pick(94., "3A", 150., &lib, &[]).unwrap().0, 1);
    }
}
