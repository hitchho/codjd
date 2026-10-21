//! Sequencer state: pattern banks stored inline (fixed capacity, edited in place by
//! `patch_cell`, never allocated while running), scales, and the per-sample step logic of
//! `MonoSeq` and `GateSeq`.

use super::nodes::Env;

pub const MAX_PATTERNS: usize = 8;
pub const MAX_STEPS: usize = 64;
pub const MAX_ROWS_GATE: usize = 8;
/// "no note" in a `MonoSeq` step
pub const NO_NOTE: u8 = 255;
/// no queued pattern
pub const NONE: u8 = 255;

/// 8 patterns of up to 64 steps. `note` is the `MonoSeq` row of a step, `mask`/`vel` the
/// `GateSeq` rows (a bit per row) and their velocities (`MonoSeq` keeps its velocity in row 0).
pub struct Bank {
    pub steps: [u16; MAX_PATTERNS],
    pub note: [[u8; MAX_STEPS]; MAX_PATTERNS],
    pub mask: [[u8; MAX_STEPS]; MAX_PATTERNS],
    pub vel: [[[f32; MAX_ROWS_GATE]; MAX_STEPS]; MAX_PATTERNS],
}

impl Bank {
    pub fn new(default_steps: u16) -> Box<Bank> {
        Box::new(Bank {
            steps: [default_steps; MAX_PATTERNS],
            note: [[NO_NOTE; MAX_STEPS]; MAX_PATTERNS],
            mask: [[0; MAX_STEPS]; MAX_PATTERNS],
            vel: [[[1.; MAX_ROWS_GATE]; MAX_STEPS]; MAX_PATTERNS],
        })
    }
}

pub const SCALES: &[(&str, &[u8])] = &[
    ("major", &[0, 2, 4, 5, 7, 9, 11]),
    ("minor", &[0, 2, 3, 5, 7, 8, 10]),
    ("dorian", &[0, 2, 3, 5, 7, 9, 10]),
    ("phrygian", &[0, 1, 3, 5, 7, 8, 10]),
    ("lydian", &[0, 2, 4, 6, 7, 9, 11]),
    ("mixolydian", &[0, 2, 4, 5, 7, 9, 10]),
    ("locrian", &[0, 1, 3, 5, 6, 8, 10]),
    ("harmonic minor", &[0, 2, 3, 5, 7, 8, 11]),
    ("pentatonic major", &[0, 2, 4, 7, 9]),
    ("pentatonic minor", &[0, 3, 5, 7, 10]),
    ("chromatic", &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]),
];

const ALIASES: &[(&str, &str)] = &[
    ("ionian", "major"),
    ("natural minor", "minor"),
    ("aeolian", "minor"),
    ("major pentatonic", "pentatonic major"),
    ("minor pentatonic", "pentatonic minor"),
    ("pentatonic", "pentatonic minor"),
];

fn note_pc(tok: &str) -> Option<i32> {
    let mut c = tok.chars();
    let base: i32 = match c.next()? {
        'c' => 0,
        'd' => 2,
        'e' => 4,
        'f' => 5,
        'g' => 7,
        'a' => 9,
        'b' => 11,
        _ => return None,
    };
    let acc = match c.next() {
        None => 0,
        Some('#') => 1,
        Some('b') => -1,
        _ => return None,
    };
    c.next().is_none().then_some((base + acc).rem_euclid(12))
}

/// `"A minor"`, `"dorian"`, `"D dorian"`: the scale's intervals and the root note. A note name in
/// the text overrides the pitch class of `root` (the MIDI note), keeping the nearest such note.
pub fn parse_scale(text: &str, root: i32) -> Result<(i32, &'static [u8]), String> {
    let lower = text.trim().to_lowercase();
    let mut words: Vec<&str> = lower.split_whitespace().collect();
    let mut root = root;
    if words.len() > 1 {
        if let Some(pc) = note_pc(words[0]) {
            // nearest note with that pitch class (ties go down)
            let mut best = root;
            let mut dist = i32::MAX;
            for cand in (root - 6)..=(root + 6) {
                if cand.rem_euclid(12) == pc && (cand - root).abs() < dist {
                    best = cand;
                    dist = (cand - root).abs();
                }
            }
            root = best;
            words.remove(0);
        }
    }
    let mut name = words.join(" ");
    if name.is_empty() {
        name = "minor".into();
    }
    if let Some((_, to)) = ALIASES.iter().find(|(a, _)| *a == name) {
        name = (*to).to_string();
    }
    SCALES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| (root, *s))
        .ok_or_else(|| format!("unknown scale '{text}'"))
}

pub fn midi_hz(note: f32) -> f32 {
    440. * ((note - 69.) / 12.).exp2()
}

pub struct SeqState {
    pub mono: bool,
    /// the `Clock` node this sequencer is clocked by (through any `ClockDiv`s), and the product
    /// of those dividers
    pub clock: Option<usize>,
    pub div: i64,
    pub cur: u8,
    pub queued: u8,
    pub pos: u16,
    count: u32,
    last_bar: i64,
    prev_clock: bool,
    tick: u64,
    last_edge: u64,
    est: f32,
    // gates: a countdown in samples, the height, and a retrigger waiting for one sample of gap
    left: [f32; MAX_ROWS_GATE],
    height: [f32; MAX_ROWS_GATE],
    pend_len: [f32; MAX_ROWS_GATE],
    pend_vel: [f32; MAX_ROWS_GATE],
    pub freq: f32,
    // MonoSeq pitch
    pub root: i32,
    pub scale: &'static [u8],
    pub bank: Box<Bank>,
}

impl SeqState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        mono: bool,
        cur: u8,
        root: i32,
        scale: &'static [u8],
        bank: Box<Bank>,
        sr: f32,
    ) -> Self {
        Self {
            mono,
            clock: None,
            div: 1,
            cur,
            queued: NONE,
            pos: 0,
            count: 0,
            last_bar: i64::MIN,
            prev_clock: false,
            tick: 0,
            last_edge: u64::MAX,
            est: 0.125 * sr,
            left: [0.; MAX_ROWS_GATE],
            height: [1.; MAX_ROWS_GATE],
            pend_len: [0.; MAX_ROWS_GATE],
            pend_vel: [0.; MAX_ROWS_GATE],
            freq: midi_hz(root as f32),
            root,
            scale,
            bank,
        }
    }

    pub fn len(&self) -> u16 {
        self.bank.steps[self.cur as usize].clamp(1, MAX_STEPS as u16)
    }

    fn note_hz(&self, row: u8) -> f32 {
        let n = self.scale.len();
        let r = row as usize;
        let semis = self.scale[r % n] as i32 + 12 * (r / n) as i32;
        midi_hz((self.root + semis) as f32)
    }

    pub fn set_cell(&mut self, pattern: usize, step: usize, row: u8, on: bool, velocity: f32) {
        if pattern >= MAX_PATTERNS || step >= MAX_STEPS {
            return;
        }
        let vel = if velocity.is_finite() { velocity.clamp(0., 1.) } else { 1. };
        let b = &mut self.bank;
        if self.mono {
            if on {
                b.note[pattern][step] = row.min(254);
                b.vel[pattern][step][0] = vel;
            } else if b.note[pattern][step] == row {
                b.note[pattern][step] = NO_NOTE;
            }
        } else if (row as usize) < MAX_ROWS_GATE {
            if on {
                b.mask[pattern][step] |= 1 << row;
                b.vel[pattern][step][row as usize] = vel;
            } else {
                b.mask[pattern][step] &= !(1 << row);
            }
        }
    }

    pub fn set_pattern(&mut self, pattern: usize, now: bool) {
        if pattern >= MAX_PATTERNS {
            return;
        }
        if now {
            self.cur = pattern as u8;
            self.queued = NONE;
            self.pos = self.pos.min(self.len() - 1);
        } else {
            self.queued = pattern as u8;
        }
    }

    /// Copies what a structural recompile keeps: where the sequencer is and what it is playing
    pub fn adopt(&mut self, old: &SeqState) {
        self.cur = old.cur;
        self.queued = old.queued;
        self.count = old.count;
        self.last_bar = old.last_bar;
        self.prev_clock = old.prev_clock;
        self.tick = old.tick;
        self.last_edge = old.last_edge;
        self.est = old.est;
        self.left = old.left;
        self.height = old.height;
        self.pend_len = old.pend_len;
        self.pend_vel = old.pend_vel;
        self.freq = old.freq;
        self.pos = old.pos.min(self.len() - 1);
    }

    /// One sample. `clock` and `gate_t` are the input values; the outputs go to `out` (freq,
    /// gate for a `MonoSeq`; gate0..7 for a `GateSeq`)
    #[inline]
    pub fn tick(&mut self, clock: f32, gate_t: f32, out: &mut [f32], env: &Env) {
        self.tick += 1;
        let high = clock > 0.5;
        let rising = high && !self.prev_clock;
        self.prev_clock = high;
        let gate_t = if gate_t.is_finite() { gate_t.clamp(0.01, 1.) } else { 0.5 };
        let gap = if rising { self.on_edge(gate_t, env) } else { 0 };
        let n = if self.mono { 1 } else { MAX_ROWS_GATE };
        let mut gates = [0f32; MAX_ROWS_GATE];
        for r in 0..n {
            // a retrigger leaves one sample of gap, so an envelope sees a new rising edge
            if self.left[r] <= 0. && self.pend_len[r] > 0. && gap & (1 << r) == 0 {
                self.left[r] = self.pend_len[r];
                self.height[r] = self.pend_vel[r];
                self.pend_len[r] = 0.;
            }
            if self.left[r] > 0. {
                gates[r] = self.height[r];
                self.left[r] -= 1.;
            }
        }
        if self.mono {
            out[0] = self.freq;
            out[1] = gates[0];
        } else {
            out[..MAX_ROWS_GATE].copy_from_slice(&gates);
        }
    }

    /// A rising clock edge: find the step, apply a queued pattern at a bar, start the gates.
    /// Returns the rows that are in their retrigger gap on this sample.
    fn on_edge(&mut self, gate_t: f32, env: &Env) -> u8 {
        if self.last_edge != u64::MAX {
            let d = (self.tick - self.last_edge) as f32;
            if d >= 2. && d < env.sr * 8. {
                self.est = d;
            }
        }
        self.last_edge = self.tick;
        let info = self.clock.map(|c| env.clocks[c]).filter(|c| c.valid);
        let new_bar = match info {
            Some(c) => {
                let nb = c.bar != self.last_bar;
                self.last_bar = c.bar;
                nb
            }
            None => false,
        };
        let step_before = self.pos;
        let pos = match info {
            Some(c) => {
                if new_bar && self.queued != NONE {
                    self.cur = self.queued;
                    self.queued = NONE;
                }
                (c.step.div_euclid(self.div)).rem_euclid(self.len() as i64) as u16
            }
            None => {
                // counting edges; a wrap is a bar
                let next = if self.count == 0 { 0 } else { step_before + 1 };
                let wrapped = next >= self.len();
                if (wrapped || self.count == 0) && self.queued != NONE {
                    self.cur = self.queued;
                    self.queued = NONE;
                }
                self.count = self.count.wrapping_add(1).max(1);
                if wrapped {
                    0
                } else {
                    next
                }
            }
        };
        self.pos = pos;
        let len = gate_t * self.est;
        let step = pos as usize;
        let cur = self.cur as usize;
        let mut gap = 0u8;
        if self.mono {
            let row = self.bank.note[cur][step];
            if row != NO_NOTE {
                self.freq = self.note_hz(row);
                let vel = 1.;
                if self.left[0] > 0. {
                    self.left[0] = 0.;
                    gap |= 1;
                }
                self.pend_len[0] = len;
                self.pend_vel[0] = vel;
            }
        } else {
            let mask = self.bank.mask[cur][step];
            for r in 0..MAX_ROWS_GATE {
                if mask & (1 << r) != 0 {
                    if self.left[r] > 0. {
                        self.left[r] = 0.;
                        gap |= 1 << r;
                    }
                    self.pend_len[r] = len;
                    self.pend_vel[r] = self.bank.vel[cur][step][r];
                }
            }
        }
        gap
    }
}
