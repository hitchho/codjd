//! Transition recipes: `mix a -> b at <pos> recipe blend`. A recipe is a block of ordinary mix
//! lines written in the set language itself, generated for the two track ids and a length. The
//! resolver puts them in front of the block's own lines (see `docs/set-language.md`), and
//! `SetRecipeText` prints the same lines with a comment per group, so the expanded text always
//! compiles to the same plan as the one-line form.

/// What a recipe is called, what it teaches, and how long it runs
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Recipe {
    pub name: &'static str,
    /// one line for someone who has never DJ'd
    pub explain: &'static str,
    pub default_bars: u32,
    /// shorter than this and the recipe's parts don't fit
    pub min_bars: u32,
    /// the new track comes in at the end of the mix, not at its start: the end is what should
    /// sit on a phrase line (`brake`)
    pub lands_at_end: bool,
}

pub const RECIPES: [Recipe; 8] = [
    Recipe {
        name: "blend",
        explain: "A long, smooth overlap: never two basslines at once.",
        default_bars: 32,
        min_bars: 8,
        lands_at_end: false,
    },
    Recipe {
        name: "bassswap",
        explain: "A short overlap where the bass changes hands on one beat, so the groove never loses its low end.",
        default_bars: 16,
        min_bars: 4,
        lands_at_end: false,
    },
    Recipe {
        name: "echo-out",
        explain: "The old track stutters away in shrinking loops while it turns thin, then the new one hits.",
        default_bars: 8,
        min_bars: 4,
        lands_at_end: false,
    },
    Recipe {
        name: "cut",
        explain: "Hard cut: works on a big drop.",
        default_bars: 1,
        min_bars: 1,
        lands_at_end: false,
    },
    Recipe {
        name: "filter-blend",
        explain: "Muffle the old track while the new one opens up from thin to full.",
        default_bars: 16,
        min_bars: 8,
        lands_at_end: false,
    },
    Recipe {
        name: "loop-roll",
        explain: "The old track rolls: a loop of its last bar halves again and again while it turns thin, and the new one starts on the beat it stops.",
        default_bars: 8,
        min_bars: 4,
        lands_at_end: false,
    },
    Recipe {
        name: "brake",
        explain: "A turntable stop: the old track slows down to nothing, then the new one starts clean.",
        default_bars: 2,
        min_bars: 1,
        lands_at_end: true,
    },
    Recipe {
        name: "eq-swap",
        explain: "Both tracks at full volume while the sound changes hands band by band: the highs first, then the mids, the bass last.",
        default_bars: 24,
        min_bars: 12,
        lands_at_end: false,
    },
];

pub fn find(name: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|r| r.name == name)
}

pub fn names() -> String {
    RECIPES.iter().map(|r| r.name).collect::<Vec<_>>().join(", ")
}

/// Mix lines that belong together, with the reason they exist
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub why: &'static str,
    pub lines: Vec<String>,
}

/// What a recipe needs to know about the two tracks
#[derive(Debug, Clone, Copy)]
pub struct Ctx<'a> {
    /// track ids of the outgoing and incoming track
    pub a: &'a str,
    pub b: &'a str,
    /// the incoming track has a cue named `drop`
    pub b_has_drop: bool,
    /// beats per bar of the outgoing track (the mix clock)
    pub beats_per_bar: f64,
}

/// A bar number the way a person writes it: `8`, `7.5`, `0.25`, `7.875`
pub fn n(v: f64) -> String {
    let s = format!("{:.3}", (v * 1000.).round() / 1000.);
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The lines of `recipe` for a mix of `bars` bars (already checked against the minimum)
pub fn groups(recipe: &Recipe, c: &Ctx, bars: f64) -> Vec<Group> {
    let (a, b) = (c.a, c.b);
    let g = |why, lines: Vec<String>| Group { why, lines };
    let one = |why, line: String| Group { why, lines: vec![line] };
    let play = || one("start the new track from its first downbeat, beat-matched to the old one", format!("{b} play"));
    match recipe.name {
        "blend" => {
            let half = (bars / 2.).floor().max(1.);
            vec![
                play(),
                one("keep the new track's bass out until the swap: two basslines at once turn to mud", format!("bar 0 {b}.low kill")),
                one("bring the new track in slowly over the first half", format!("bars 0..{} {b}.fader 0 -> 1", n(half))),
                one("the bass swap: the old bass goes out and the new one comes in on the same beat", format!("bar {} swap low", n(half))),
                one(
                    "now take the old track away: the filter dulls it, the fader fades it",
                    format!("bars {}..{} {a}.filter 0 -> -0.6 smooth; {a}.fader 1 -> 0", n(half), n(bars)),
                ),
                one("the old track is done", format!("bar {} {a} stop", n(bars))),
            ]
        }
        "bassswap" => {
            let half = (bars / 2.).floor().max(1.);
            let quick = (bars / 4.).floor().clamp(1., 2.);
            let fade = (bars * 3. / 4.).floor().max(half);
            vec![
                play(),
                one("the new track's bass waits for the swap", format!("bar 0 {b}.low kill")),
                one("the new track is at full volume within a couple of bars", format!("bars 0..{} {b}.fader 0 -> 1", n(quick))),
                one("the bass swap: the old bass goes out and the new one comes in on the same beat", format!("bar {} swap low", n(half))),
                one(
                    "keep both tracks playing without their bass, then fade the old one out",
                    format!("bars {}..{} {a}.filter 0 -> -0.5 smooth; {a}.fader 1 -> 0", n(fade), n(bars)),
                ),
                one("the old track is done", format!("bar {} {a} stop", n(bars))),
            ]
        }
        "echo-out" => {
            // the outgoing deck's clock stands still inside a loop; the runtime times what follows
            // on the incoming deck (see docs), so the tail can be written in plain bars
            let end = bars;
            let (l4, l2, l1, off) = (end - 3., end - 1., end - 0.5, end - 0.25);
            let (why, jump) = if c.b_has_drop {
                ("the new track jumps to its drop on the beat the echo ends", format!("bar {} {b} jump drop", n(end)))
            } else {
                (
                    "the new track has no `drop` cue: jump back to its first downbeat as the echo ends (add a cue named drop to land on it instead)",
                    format!("bar {} {b} jump -{} beats", n(end), n(end * c.beats_per_bar)),
                )
            };
            vec![
                g(
                    "the new track runs silently from its start; it comes in when the echo ends",
                    vec![format!("{b} play"), format!("bar 0 {b}.fader = 0")],
                ),
                one(
                    "echo = repeat the last bit: loop 4 beats, then 2, then 1 (there is no echo effect yet)",
                    format!("bar {} {a} loop 4 beats", n(l4)),
                ),
                one("tighter", format!("bar {} {a} loop 2 beats", n(l2))),
                one("tightest", format!("bar {} {a} loop 1 beat", n(l1))),
                one("a high-pass sweep thins the old track out as the loops tighten", format!("bars {}..{} {a}.filter 0 -> 0.8 exp", n(l4), n(l1))),
                one("fade it out under the loops", format!("bars {}..{} {a}.fader 1 -> 0", n(end - 2.), n(l1))),
                one("end the loop a beat before the stop (a loop-off on the stop's beat can get stuck)", format!("bar {} {a} loop off", n(off))),
                one("silence, then the drop", format!("bar {} {a} stop", n(end))),
                g(why, vec![jump, format!("bar {} {b}.fader = 1", n(end))]),
            ]
        }
        "cut" => vec![
            play(),
            one("full volume from the first beat", format!("bar 0 {b}.fader = 1")),
            one("the old track stops on the same beat: a cut, no overlap", format!("bar 0 {a} stop")),
        ],
        "filter-blend" => {
            let end = n(bars);
            vec![
                play(),
                one(
                    "the new track starts thin (high-pass on) and opens up to full",
                    format!("bars 0..{end} {b}.filter 0.7 -> 0 smooth"),
                ),
                one("the old track is muffled (low-pass) more and more", format!("bars 0..{end} {a}.filter 0 -> -0.7 smooth")),
                one("while the faders crossfade", format!("bars 0..{end} {b}.fader 0 -> 1; {a}.fader 1 -> 0")),
                one("the old track is done", format!("bar {end} {a} stop")),
            ]
        }
        "loop-roll" => {
            // like echo-out, the outgoing deck's clock stands still in the loop and the tail is
            // timed on the incoming deck; the roll keeps its volume, the filter thins it
            let end = bars;
            let bar = |beats: f64| beats / c.beats_per_bar;
            let (l4, l2, l1, lh, off) = (end - bar(8.), end - bar(4.), end - bar(2.), end - bar(1.), end - bar(0.5));
            let sweep = (bars / 2.).floor().min(l4);
            vec![
                g(
                    "the new track runs silently from its start; it comes in when the roll ends",
                    vec![format!("{b} play"), format!("bar 0 {b}.fader = 0")],
                ),
                one("a high-pass filter rises on the old track: it gets thinner and thinner", format!("bars {}..{} {a}.filter 0 -> 0.7 exp", n(sweep), n(off))),
                one("the roll: loop the last bar (4 beats) ...", format!("bar {} {a} loop 4 beats", n(l4))),
                one("... then half of it", format!("bar {} {a} loop 2 beats", n(l2))),
                one("... half again", format!("bar {} {a} loop 1 beat", n(l1))),
                one("... and half a beat: the roll", format!("bar {} {a} loop 0.5 beats", n(lh))),
                one("end the loop half a beat before the stop (a loop-off on the stop's beat can get stuck)", format!("bar {} {a} loop off", n(off))),
                one("the old track stops dead on the downbeat", format!("bar {} {a} stop", n(end))),
                g(
                    "and the new track starts from its first downbeat at full volume on that beat",
                    vec![format!("bar {} {b} jump -{} beats", n(end), n(end * c.beats_per_bar)), format!("bar {} {b}.fader = 1", n(end))],
                ),
            ]
        }
        "brake" => {
            // the brake slows the outgoing deck's clock down to nothing: from its start the mix is
            // timed on the incoming deck (see docs), which runs silently from bar 0
            let end = bars;
            let from = end - 2. / c.beats_per_bar;
            vec![
                g(
                    "the new track runs silently from its start; it comes in when the old one has stopped",
                    vec![format!("{b} play"), format!("bar 0 {b}.fader = 0")],
                ),
                one(
                    "the brake: over the last two beats the old track slows down to a standstill, like a turntable switched off",
                    format!("bars {}..{} {a}.rate _ -> 0 linear", n(from), n(end)),
                ),
                g(
                    "the old track is done: pause it and put its tempo back for whatever plays on that deck next",
                    vec![format!("bar {} {a} stop", n(end)), format!("bar {} {a}.rate = 1", n(end))],
                ),
                g(
                    "the new track starts clean from its first downbeat, at full volume",
                    vec![format!("bar {} {b} jump -{} beats", n(end), n(end * c.beats_per_bar)), format!("bar {} {b}.fader = 1", n(end))],
                ),
            ]
        }
        "eq-swap" => {
            let third = (bars / 3.).floor().max(1.);
            let (t1, t2) = (third, 2. * third);
            vec![
                play(),
                one(
                    "the new track plays at full volume with its bass and mids killed: only its highs will come in first",
                    format!("bar 0 {b}.low kill; {b}.mid kill"),
                ),
                one(
                    "first phrase: the highs change hands (hats and cymbals of the new track, out with the old ones)",
                    format!("bars 0..{} {b}.high -1 -> 0; {a}.high 0 -> -1", n(t1)),
                ),
                one(
                    "second phrase: the mids change hands (chords, melodies, vocals)",
                    format!("bars {}..{} {b}.mid -1 -> 0; {a}.mid 0 -> -1", n(t1), n(t2)),
                ),
                one(
                    "third phrase line: the bass swaps on one beat, two basslines at once turn to mud",
                    format!("bar {} swap low", n(bars)),
                ),
                one("the old track is done", format!("bar {} {a} stop", n(bars))),
            ]
        }
        other => unreachable!("recipe {other}"),
    }
}

/// The recipe as the lines of a mix block, groups in order (no comments)
pub fn lines(recipe: &Recipe, c: &Ctx, bars: f64) -> Vec<String> {
    groups(recipe, c, bars).into_iter().flat_map(|g| g.lines).collect()
}

/// `SetRecipeText`: the named recipe as text, `bars` 0 = its default; an error names what is
/// wrong (unknown recipe, too short)
pub fn expand(recipe: &str, from: &str, to: &str, bars: u32, c: &Ctx) -> Result<String, String> {
    let r = find(recipe).ok_or_else(|| format!("unknown recipe \"{recipe}\" (recipes: {})", names()))?;
    let bars = if bars == 0 { r.default_bars } else { bars };
    if bars < r.min_bars {
        return Err(format!("recipe {} needs at least {} bars (got {bars})", r.name, r.min_bars));
    }
    Ok(text(r, from, to, bars as f64, c))
}

/// The recipe as a self-explaining, editable block of text
fn text(recipe: &Recipe, from: &str, to: &str, bars: f64, c: &Ctx) -> String {
    let mut out = format!("# {}: {}\nmix {from} -> {to} at bar 1 over {} bars\n", recipe.name, recipe.explain, n(bars));
    for g in groups(recipe, c, bars) {
        out.push_str(&format!("  # {}\n", g.why));
        for l in g.lines {
            out.push_str(&format!("  {l}\n"));
        }
    }
    out
}
