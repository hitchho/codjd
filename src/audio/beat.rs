//! Beat grid maths for the real-time side. Pure functions on plain numbers: no allocation, no
//! locks, nothing that can panic (all divisions are by validated, non-zero values).
//!
//! Everything is in *track frames* (the file's own sample rate), at playback rate 1.

/// A constant grid: beat `k` is at `first + k * beat` frames.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BeatGrid {
    /// frame of the first grid beat (at or after 0, extrapolated if the music starts later)
    pub first: f64,
    /// frames per beat
    pub beat: f64,
    pub per_bar: u32,
    /// beats `k` with `k % per_bar == offset` are bar starts (beat 1)
    pub offset: u32,
    /// the grid's tempo
    pub bpm: f64,
}

/// What the control side knows about a grid: seconds, as the analysis stores it
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridSpec {
    pub bpm: f64,
    pub first_beat_s: f64,
    pub downbeat_offset: u32,
    pub beats_per_bar: u32,
}

impl BeatGrid {
    /// None for a grid that can't be right (non-finite or absurd tempo)
    pub fn new(spec: GridSpec, track_rate: u32) -> Option<BeatGrid> {
        let ok = spec.bpm.is_finite()
            && (20. ..=400.).contains(&spec.bpm)
            && spec.first_beat_s.is_finite()
            && spec.first_beat_s > -3600.
            && spec.first_beat_s < 36_000.
            && track_rate > 0;
        if !ok {
            return None;
        }
        let per_bar = spec.beats_per_bar.clamp(1, 16);
        Some(BeatGrid {
            first: spec.first_beat_s * track_rate as f64,
            beat: 60. / spec.bpm * track_rate as f64,
            per_bar,
            offset: spec.downbeat_offset % per_bar,
            bpm: spec.bpm,
        })
    }

    /// Position in beats since beat 0 (fractional)
    #[inline]
    pub fn beats_at(&self, pos: f64) -> f64 {
        (pos - self.first) / self.beat
    }

    /// Frame of the (fractional) beat number `beat`
    #[inline]
    pub fn frame_at_beat(&self, beat: f64) -> f64 {
        self.first + beat * self.beat
    }

    /// Index of the beat that `pos` is in (the last beat boundary at or before it)
    #[inline]
    pub fn index_at(&self, pos: f64) -> i64 {
        self.beats_at(pos).floor() as i64
    }

    /// Frame of beat `k`
    #[inline]
    pub fn frame_of(&self, k: i64) -> f64 {
        self.first + k as f64 * self.beat
    }

    /// 0..1 within the current beat
    #[inline]
    pub fn phase_at(&self, pos: f64) -> f64 {
        let b = self.beats_at(pos);
        let p = b - b.floor();
        // a rounding result of exactly 1.0 would read as the next beat
        if p >= 1. {
            0.
        } else {
            p
        }
    }

    /// 1..=per_bar
    #[inline]
    pub fn beat_in_bar(&self, pos: f64) -> u32 {
        let k = self.index_at(pos);
        k.wrapping_sub(self.offset as i64).rem_euclid(self.per_bar as i64) as u32 + 1
    }

    /// Frame of the beat boundary nearest to `pos`, never before 0
    #[inline]
    pub fn nearest(&self, pos: f64) -> f64 {
        self.frame_of(self.beats_at(pos).round() as i64).max(0.)
    }

    /// First beat boundary at or after `pos`
    #[inline]
    pub fn next_at_or_after(&self, pos: f64) -> f64 {
        self.frame_of(self.beats_at(pos).ceil() as i64)
    }
}

/// How a deck follows the other deck's tempo
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyncPlan {
    /// playback rate for the follower
    pub rate: f64,
    /// master's effective tempo divided by the follower's: 1, 2 (follower plays half-time
    /// against the master) or 0.5 (double-time)
    pub octave: f64,
}

/// Sane range of the follower's rate
pub const SYNC_RATE_MIN: f64 = 0.5;
pub const SYNC_RATE_MAX: f64 = 2.0;
/// A ratio at least this far from 1 in either direction is read as a half/double grid
const OCTAVE_HI: f64 = 1.8;
const OCTAVE_LO: f64 = 1. / OCTAVE_HI;

/// The rate that makes a track with grid tempo `own_bpm` run at `master_bpm` (the master's grid
/// tempo times its rate), folded by an octave when the two grids are about 2x apart. None when
/// there is no sane rate.
pub fn sync_plan(master_bpm: f64, own_bpm: f64) -> Option<SyncPlan> {
    if !(master_bpm.is_finite() && own_bpm.is_finite() && master_bpm > 0. && own_bpm > 0.) {
        return None;
    }
    let mut rate = master_bpm / own_bpm;
    let mut octave = 1.;
    if rate >= OCTAVE_HI {
        rate /= 2.;
        octave = 2.;
    } else if rate <= OCTAVE_LO {
        rate *= 2.;
        octave = 0.5;
    }
    (SYNC_RATE_MIN..=SYNC_RATE_MAX)
        .contains(&rate)
        .then_some(SyncPlan { rate, octave })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> BeatGrid {
        BeatGrid::new(
            GridSpec { bpm: 120., first_beat_s: 0.25, downbeat_offset: 1, beats_per_bar: 4 },
            1000,
        )
        .unwrap()
    }

    #[test]
    fn beat_maths() {
        let g = grid();
        assert_eq!(g.first, 250.);
        assert_eq!(g.beat, 500.);
        assert_eq!(g.index_at(250.), 0);
        assert_eq!(g.index_at(749.), 0);
        assert_eq!(g.index_at(750.), 1);
        assert_eq!(g.index_at(0.), -1);
        assert!((g.phase_at(500.) - 0.5).abs() < 1e-12);
        // k = 1 is a bar start (offset 1): beat 1 there, then 2, 3, 4, and k = 0 is beat 4
        assert_eq!(g.beat_in_bar(760.), 1);
        assert_eq!(g.beat_in_bar(1260.), 2);
        assert_eq!(g.beat_in_bar(300.), 4);
        assert_eq!(g.beat_in_bar(0.), 3);
        assert_eq!(g.nearest(480.), 250.);
        assert_eq!(g.nearest(520.), 750.);
        assert_eq!(g.nearest(10.), 250.);
        assert_eq!(g.next_at_or_after(251.), 750.);
        assert_eq!(g.next_at_or_after(250.), 250.);
    }

    #[test]
    fn rejects_nonsense() {
        let s = GridSpec { bpm: 0., first_beat_s: 0., downbeat_offset: 0, beats_per_bar: 4 };
        assert!(BeatGrid::new(s, 44100).is_none());
        assert!(BeatGrid::new(GridSpec { bpm: f64::NAN, ..s }, 44100).is_none());
        assert!(BeatGrid::new(GridSpec { bpm: 128., first_beat_s: f64::INFINITY, ..s }, 44100).is_none());
        // offset beyond the bar wraps instead of indexing out of range
        let g = BeatGrid::new(GridSpec { bpm: 128., downbeat_offset: 9, ..s }, 44100).unwrap();
        assert_eq!(g.offset, 1);
    }

    #[test]
    fn sync_plan_octaves() {
        let p = sync_plan(128., 126.).unwrap();
        assert!((p.rate - 128. / 126.).abs() < 1e-12 && p.octave == 1.);
        // a 70 BPM grid against 140: play at normal speed, half-time
        let p = sync_plan(140., 70.).unwrap();
        assert_eq!((p.rate, p.octave), (1., 2.));
        // and a 140 grid against 70
        let p = sync_plan(70., 140.).unwrap();
        assert_eq!((p.rate, p.octave), (1., 0.5));
        // slightly off 2x still folds towards 1.0
        let p = sync_plan(134., 70.).unwrap();
        assert!((p.rate - 134. / 140.).abs() < 1e-12 && p.octave == 2.);
        // 3:2 is not an octave: matched as is
        assert_eq!(sync_plan(135., 90.).unwrap().rate, 1.5);
        assert!(sync_plan(300., 50.).is_none());
        assert!(sync_plan(0., 120.).is_none());
        assert!(sync_plan(f64::NAN, 120.).is_none());
    }
}
