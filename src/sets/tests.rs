//! Resolution, grid math, compile and validation against a fake library

use std::path::{Path, PathBuf};

use super::{
    compile::{Control, Curve, Deck, PlanItem},
    load,
    model::Pos,
    resolve::find_file,
    testenv::{grid, spec_env, FakeEnv},
    Loaded,
};

const SPEC: &str = r#"
# kaiku friday
set "kaiku friday"

track aft "Pneumatix - Aftertekno.flac"
  grid 112 @ 0.012s beat1 1      # optional: override the detected grid
  gain -1.5dB                    # optional: override auto-gain
  cue drop  bar 33
  cue outro bar 129
  loop break bar 97 len 8 bars

track smk "Aasi & Swooh - Smokin"   # a path, or a search resolved against the library
  cue intro bar 1

mix aft -> smk at aft.outro over 32 bars
  smk play from intro            # synced to aft by default ("unsynced" to opt out)
  bars 0..16   smk.low kill; smk.fader 0 -> 1
  bar 16       swap low          # bass swap: aft.low kill, smk.low back to 0
  bars 16..32  aft.filter 0 -> -0.6 smooth; aft.fader 1 -> 0
  bar 32       aft stop
"#;

fn run(src: &str) -> Loaded {
    load(Path::new("/sets/kaiku.codj"), src, &spec_env())
}

fn errors(src: &str) -> Vec<String> {
    run(src).diags.iter().map(|d| format!("{}:{}: {}", d.line, d.col, d.msg)).collect()
}

fn good(src: &str) -> super::Compiled {
    let l = run(src);
    assert!(l.diags.is_empty(), "{:#?}", l.diags);
    l.compiled.unwrap()
}

const HEAD: &str = "track aft \"Pneumatix - Aftertekno.flac\"\n  cue outro bar 129\n  cue drop bar 33\n  loop lp bar 5 len 2 bars\ntrack smk \"Smokin\"\n  cue intro bar 3\n  cue hook bar 9 beat 3\n  loop lp bar 2 beat 2 len 6 beats\n";

fn mix(body: &str) -> String {
    format!("{HEAD}mix aft -> smk at aft.outro over 32 bars\n{body}")
}

// ---- golden ----

#[test]
fn spec_example_compiles_to_the_expected_plan() {
    let c = good(SPEC);
    assert_eq!(c.set.name, "kaiku friday");
    let aft = c.set.track("aft").unwrap();
    assert_eq!(aft.gain_db, Some(-1.5));
    let g = aft.grid.as_ref().unwrap();
    assert_eq!((g.bpm, g.first_beat_s, g.downbeat_offset), (112., 0.012, 0));
    assert_eq!(c.set.track("smk").unwrap().file, PathBuf::from("/music/Aasi & Swooh - Smokin.flac"));

    assert_eq!(c.plans.len(), 1);
    let p = &c.plans[0];
    assert_eq!((p.from_track.as_str(), p.to_track.as_str()), ("aft", "smk"));
    assert_eq!(p.start_beat, 512.); // bar 129 of a grid whose downbeat is beat 0
    assert_eq!(p.length_beats, 128.);
    use Control::*;
    use Deck::*;
    let ramp = |deck, control, from, to, s, e, curve| PlanItem::Ramp {
        deck,
        control,
        from,
        to,
        start_beat_rel: s,
        end_beat_rel: e,
        curve,
    };
    let expected = vec![
        // smk's first downbeat is on its grid beat 2
        PlanItem::Play { from_beat: 2., synced: true, at_beat_rel: 0. },
        PlanItem::Set { deck: In, control: Low, value: -1., at_beat_rel: 0. },
        ramp(In, Fader, Some(0.), 1., 0., 64., Curve::Linear),
        PlanItem::Swap { band: Low, at_beat_rel: 64. },
        ramp(Out, Filter, Some(0.), -0.6, 64., 128., Curve::Smooth),
        ramp(Out, Fader, Some(1.), 0., 64., 128., Curve::Linear),
        PlanItem::Pause { deck: Out, at_beat_rel: 128. },
    ];
    assert_eq!(p.items, expected);
}

// ---- grid math ----

#[test]
fn bars_and_beats_count_from_the_first_downbeat() {
    let mut env = FakeEnv { root: "/m".into(), ..Default::default() };
    env.add("A", "One", "one.flac", Some(grid(120., 0.2, 3)));
    env.add("A", "Two", "two.flac", Some(grid(120., 0.2, 0)));
    let src = "track a \"one.flac\"\n cue c1 bar 1\n cue c2 bar 3 beat 2\ntrack b \"two.flac\"\nmix a -> b at a.c2 over 4 bars\n";
    let c = load(Path::new("x.codj"), src, &env).compiled.unwrap();
    let a = c.set.track("a").unwrap();
    assert_eq!(a.beat_of(&Pos { bar: 1, beat: 1. }), Some(3.));
    assert_eq!(a.beat_of(&Pos { bar: 3, beat: 2. }), Some(3. + 8. + 1.));
    assert_eq!(c.plans[0].start_beat, 12.);
    // incoming default: its first downbeat, here beat 0
    assert!(matches!(c.plans[0].items[0], PlanItem::Play { from_beat, synced: true, .. } if from_beat == 0.));
}

#[test]
fn grid_overrides_replace_or_scale_the_detected_grid() {
    let mut env = FakeEnv { root: "/m".into(), ..Default::default() };
    let p = env.add("A", "One", "one.flac", Some(grid(100., 0.5, 2)));
    let eff = |env: &FakeEnv, line: &str| {
        let src = format!("track a \"one.flac\"\n  {line}\n");
        let l = load(Path::new("x.codj"), &src, env);
        assert!(l.diags.is_empty(), "{line}: {:?}", l.diags);
        l.compiled.unwrap().set.tracks[0].grid.clone()
    };
    let key = |g: Option<crate::analysis::Grid>| {
        let g = g.unwrap();
        ((g.bpm * 1e6).round() / 1e6, g.first_beat_s, g.downbeat_offset)
    };
    assert_eq!(key(eff(&env, "gain 0")), (100., 0.5, 2));
    assert_eq!(key(eff(&env, "grid 128")), (128., 0.5, 2)); // tempo only: first beat and bars stay
    assert_eq!(key(eff(&env, "grid @ 0.25s")), (100., 0.25, 0));
    assert_eq!(key(eff(&env, "grid beat1 2")), (100., 0.5, 3));
    assert_eq!(key(eff(&env, "grid beat1 4")), (100., 0.5, 1));
    assert_eq!(key(eff(&env, "grid 90 @ 1.5s beat1 3")), (90., 1.5, 2));
    // scales reset the bar position, exactly like DjGridScale
    assert_eq!(key(eff(&env, "grid x2")), (200., 0.5, 0));
    assert_eq!(key(eff(&env, "grid /2")), (50., 0.5, 0));
    assert_eq!(key(eff(&env, "grid x1.5")), (150., 0.5, 0));
    assert_eq!(key(eff(&env, "grid /1.5 beat1 2")), (66.666667, 0.5, 3));
    // the hand correction applies first, the set's line on top of it
    env.factors.insert(p, 0.5);
    assert_eq!(key(eff(&env, "gain 0")), (50., 0.5, 0));
    assert_eq!(key(eff(&env, "grid x2")), (100., 0.5, 0));
    assert_eq!(key(eff(&env, "grid 77")), (77., 0.5, 0));
}

#[test]
fn a_tempo_line_makes_a_grid_where_the_analysis_has_none() {
    let mut env = FakeEnv { root: "/m".into(), ..Default::default() };
    env.add("A", "Raw", "raw.flac", None);
    env.add("A", "Other", "other.flac", Some(grid(120., 0., 0)));
    let l = |line: &str| {
        let src = format!("track a \"raw.flac\"\n  {line}\ntrack b \"other.flac\"\nmix a -> b at bar 1 over 4 bars\n");
        load(Path::new("x.codj"), &src, &env)
    };
    let c = l("grid 128 @ 0.1s beat1 2").compiled.unwrap();
    let g = c.set.tracks[0].grid.as_ref().unwrap();
    assert_eq!((g.bpm, g.first_beat_s, g.beats_per_bar, g.downbeat_offset), (128., 0.1, 4, 3));
    // without a tempo there's nothing to scale; the mix then complains
    let e = l("grid x2").diags;
    assert!(e[0].msg.contains("no beat grid for track a: analyze it or add a `grid` line"), "{e:?}");
}

// ---- files ----

#[test]
fn files_resolve_by_path_or_search() {
    let mut env = FakeEnv { root: "/music".into(), ..Default::default() };
    env.add("Aasi & Swooh", "Smokin", "Aasi & Swooh - Smokin.flac", None);
    env.add("Pneumatix", "Aftertekno", "sub/pn-aft.flac", None);
    env.add("Pneumatix", "Aftertekno (Remix)", "pn-aft-remix.flac", None);
    env.add("Other", "Smokin", "other/smokin-x.mp3", None);
    env.add("X", "T1", "t1.mp3", None);
    for n in 2..9 {
        env.add("Many", &format!("Song {n}"), &format!("many/song{n}.mp3"), None);
    }
    // relative to the root, absolute
    assert_eq!(find_file(&env, "t1.mp3"), Ok(PathBuf::from("/music/t1.mp3")));
    assert_eq!(find_file(&env, "/music/t1.mp3"), Ok(PathBuf::from("/music/t1.mp3")));
    assert!(find_file(&env, "/music/nope.mp3").unwrap_err().contains("file not found"));
    // search: artist - title, file name or stem, any case
    assert_eq!(find_file(&env, "aasi & swooh - SMOKIN"), Ok(PathBuf::from("/music/Aasi & Swooh - Smokin.flac")));
    assert_eq!(find_file(&env, "PN-AFT.FLAC"), Ok(PathBuf::from("/music/sub/pn-aft.flac")));
    assert_eq!(find_file(&env, "pn-aft-remix"), Ok(PathBuf::from("/music/pn-aft-remix.flac")));
    // exact beats partial: "pneumatix - aftertekno" is also a prefix of the remix
    assert_eq!(find_file(&env, "Pneumatix - Aftertekno"), Ok(PathBuf::from("/music/sub/pn-aft.flac")));
    // a unique fragment
    assert_eq!(find_file(&env, "remix"), Ok(PathBuf::from("/music/pn-aft-remix.flac")));
    // ambiguous: lists candidates
    let e = find_file(&env, "smokin").unwrap_err();
    assert!(e.contains("matches 2 tracks") && e.contains("Aasi & Swooh - Smokin") && e.contains("Other - Smokin"), "{e}");
    // many hits: at most five are named
    let e = find_file(&env, "song").unwrap_err();
    assert!(e.contains("matches 7 tracks") && e.contains("and 2 more"), "{e}");
    assert_eq!(e.matches(" - Song ").count(), 5);
    // missing
    assert!(find_file(&env, "nothing like it").unwrap_err().contains("no track matching"));
}

#[test]
fn file_errors_point_at_the_string() {
    let e = errors("track a \"missing\"\n");
    assert_eq!(e.len(), 1);
    assert!(e[0].starts_with("1:9: no track matching \"missing\""), "{e:?}");
}

// ---- validation ----

#[test]
fn unknown_names_list_what_exists() {
    let e = errors(&mix("  bars 0..4 aft.fader 1 -> 0\n").replace("aft.outro", "aft.outr"));
    let at = format!("{}:", HEAD.lines().count() + 1);
    assert!(e[0].starts_with(&at), "{e:?}");
    assert!(e[0].ends_with("unknown cue \"outr\" on track aft (cues: outro, drop)"), "{e:?}");
    let e = errors(&mix("  smk play from intro2\n"));
    assert!(e[0].ends_with("unknown cue \"intro2\" on track smk (cues: intro, hook)"), "{e:?}");
    let e = errors(&mix("  bar 1 smk loop nope\n"));
    assert!(e[0].ends_with("unknown loop \"nope\" on track smk (loops: lp)"), "{e:?}");
    let e = errors(&mix("  bar 1 smk jump nope\n"));
    assert!(e[0].contains("unknown cue \"nope\""), "{e:?}");
    let e = errors(&mix("  bar 1 zzz stop\n"));
    assert!(e[0].contains("track \"zzz\" is not part of this mix (aft -> smk)"), "{e:?}");
    let e = errors(&format!("{HEAD}mix aft -> zzz at bar 1 over 4 bars\n"));
    assert!(e[0].contains("unknown track \"zzz\" (tracks: aft, smk)"), "{e:?}");
    let e = errors(&format!("{HEAD}mix aft -> aft at bar 1 over 4 bars\n"));
    assert!(e[0].contains("two different tracks"), "{e:?}");
    let e = errors(&format!("{HEAD}mix aft -> smk at smk.intro over 4 bars\n"));
    assert!(e[0].contains("starts on the outgoing track aft"), "{e:?}");
}

#[test]
fn value_ranges_and_times_are_validated() {
    for (body, needle) in [
        ("bars 0..4 aft.fader 0 -> 1.5", "fader 1.5 is outside 0..1"),
        ("bar 1 smk.fader = -0.1", "fader -0.1 is outside 0..1"),
        ("bar 1 smk.low = 2", "low 2 is outside -1..1"),
        ("bars 0..4 aft.filter 0 -> -2", "filter -2 is outside -1..1"),
        ("bar 1 xfade = 3", "crossfader 3 is outside -1..1"),
        ("bar 1 aft.gain = 20dB", "gain 20 is outside -48..12"),
        ("bar 1 aft.filter kill", "`kill` works on low, mid and high, not filter"),
        ("bar 1 swap filter", "can't swap \"filter\""),
        ("bar 1 aft.volume = 1", "unknown control \"volume\""),
        ("bar 40 aft stop", "bar 40 is outside the mix (0..32 bars)"),
        ("bars 30..33 aft.fader 1 -> 0", "bar 33 is after the end of the mix (32 bars)"),
        ("bar 1 aft.fader 1 -> 0", "a ramp needs a `bars <i>..<j>` line"),
        ("bar 1 xfade -1 -> 1", "a ramp needs a `bars <i>..<j>` line"),
        ("bars 0..4 aft.fader 1 -> 0 wobbly", "unknown curve \"wobbly\""),
        ("bar 1 smk loop 0 beats", "a loop needs a length above 0"),
        ("smk play from bar 1 beat 5", "beat 5 is outside the bar"),
        ("bar -1 aft stop", "bar -1 is outside the mix"),
    ] {
        let e = errors(&mix(&format!("  {body}\n")));
        assert!(e.iter().any(|m| m.contains(needle)), "{body}: wanted {needle:?}, got {e:?}");
    }
}

#[test]
fn bar_32_of_a_32_bar_mix_is_inside() {
    good(&mix("  bar 32 aft stop\n  bars 30..32 aft.fader 1 -> 0\n"));
}

#[test]
fn structure_errors() {
    let e = errors(&mix("  smk play\n  smk play unsynced\n"));
    assert!(e[0].contains("second `play` line"), "{e:?}");
    let e = errors(&mix("  aft play\n"));
    assert!(e[0].contains("aft` is the outgoing track"), "{e:?}");
    let e = errors(&format!("{HEAD}track aft \"x\"\n"));
    assert!(e[0].contains("track \"aft\" is declared twice"), "{e:?}");
    let e = errors("track a \"Smokin\"\n cue x bar 1\n cue x bar 2\n gain 99dB\n gain 1\n gain 2\n loop l bar 1 len 0 beats\n grid 5\n grid 90 beat1 7\n");
    for needle in ["cue \"x\" is declared twice", "gain 99dB is outside", "second `gain`", "above 0", "tempo 5 is outside", "second `grid`"] {
        assert!(e.iter().any(|m| m.contains(needle)), "{needle}: {e:?}");
    }
    let e = errors("track a \"Smokin\"\n cue x bar 1 beat 5\n");
    assert!(e[0].contains("beat 5 is outside the bar (track a has 4 beats per bar)"), "{e:?}");
    let e = errors("set \"a\"\nset \"b\"\n");
    assert!(e[0].contains("a second `set`"), "{e:?}");
    // a negative bar position
    let e = errors(&mix("").replace("aft.outro", "bar 1").replace("track smk", "track q \"x\"\ntrack smk").replace("track q \"x\"\n", ""));
    assert!(e.is_empty(), "{e:?}");
}

#[test]
fn a_mix_needs_grids() {
    let mut env = spec_env();
    env.add("No", "Grid", "nogrid.flac", None);
    let src = "track a \"nogrid\"\ntrack b \"Smokin\"\nmix a -> b at bar 1 over 4 bars\n";
    let l = load(Path::new("x.codj"), src, &env);
    assert_eq!(l.diags.len(), 1);
    assert_eq!((l.diags[0].line, l.diags[0].col), (3, 5));
    assert!(l.diags[0].msg.contains("no beat grid for track a: analyze it or add a `grid` line"));
    // a set without mixes is a crate: the missing grid is fine
    let l = load(Path::new("x.codj"), "track a \"nogrid\"\n", &env);
    assert!(l.diags.is_empty() && l.compiled.unwrap().plans.is_empty());
}

#[test]
fn syntax_errors_come_first_and_name_defaults_to_the_file() {
    let l = run("track a\ntrack aft \"Smokin\"\nmix\n");
    assert_eq!(l.name, "kaiku");
    assert!(l.compiled.is_none());
    assert_eq!(l.diags.len(), 2);
    let l = run("set \"mine\"\ntrack a\n");
    assert_eq!(l.name, "mine");
}

// ---- compile details ----

#[test]
fn defaults_and_loops_and_jumps_in_the_plan() {
    use Deck::*;
    let c = good(&mix(
        "  bar 0 smk.low kill; smk.mid = -0.5; aft.gain = -3dB; xfade = -1\n\
         \x20 bars 0..8 xfade -1 -> 1; smk.high _ -> 0; smk.gain -6 -> 0; smk.filter 0.5 -> 0 exp\n\
         \x20 bar 8 smk loop lp; aft loop lp; aft loop off; smk jump hook; aft jump drop\n",
    ));
    let p = &c.plans[0];
    // aft: downbeat at 0, so bar 5 = beat 16, bar 33 = beat 128; len 2 bars = 8 beats
    // smk: downbeat at beat 2: `bar 2 beat 2` = 2 + 4 + 1, 6 beats; hook = bar 9 beat 3 = 2 + 32 + 2
    let find = |f: &dyn Fn(&PlanItem) -> bool| p.items.iter().filter(|i| f(i)).cloned().collect::<Vec<_>>();
    assert_eq!(find(&|i| matches!(i, PlanItem::Loop { .. })), [
        PlanItem::Loop { deck: In, start_beat: 7., beats: 6., at_beat_rel: 32. },
        PlanItem::Loop { deck: Out, start_beat: 16., beats: 8., at_beat_rel: 32. },
    ]);
    assert_eq!(find(&|i| matches!(i, PlanItem::Jump { .. })), [
        PlanItem::Jump { deck: In, to_beat: 36., at_beat_rel: 32. },
        PlanItem::Jump { deck: Out, to_beat: 128., at_beat_rel: 32. },
    ]);
    // curves: filter/bands smooth, fader/gain/crossfader linear unless written
    let curves: Vec<(Control, Curve)> = p
        .items
        .iter()
        .filter_map(|i| match i {
            PlanItem::Ramp { control, curve, .. } => Some((*control, *curve)),
            _ => None,
        })
        .collect();
    assert_eq!(curves, [
        (Control::Crossfader, Curve::Linear),
        (Control::High, Curve::Smooth),
        (Control::Gain, Curve::Linear),
        (Control::Filter, Curve::Exp),
    ]);
    assert!(p.items.contains(&PlanItem::Ramp {
        deck: In,
        control: Control::High,
        from: None,
        to: 0.,
        start_beat_rel: 0.,
        end_beat_rel: 32.,
        curve: Curve::Smooth
    }));
    assert!(p.items.contains(&PlanItem::Set { deck: Out, control: Control::Gain, value: -3., at_beat_rel: 0. }));
    assert!(p.items.contains(&PlanItem::Set { deck: Out, control: Control::Crossfader, value: -1., at_beat_rel: 0. }));
    // sorted by time
    assert!(p.items.windows(2).all(|w| w[0].start_beat_rel() <= w[1].start_beat_rel()));
}

#[test]
fn relative_loops_and_jumps_follow_the_deck_position() {
    use Deck::*;
    // the mix starts at aft beat 512; `bar 2` is 8 beats in
    let c = good(&mix(
        "  smk play from bar 3\n  bar 2 aft loop 4 beats; smk loop 2 beats; smk jump -8 beats\n  bar 3 smk loop 1 beats\n",
    ));
    let items = &c.plans[0].items;
    // aft: 512 + 8; smk starts at bar 3 = 2 + 8 = 10, +8 = 18
    assert!(items.contains(&PlanItem::Loop { deck: Out, start_beat: 520., beats: 4., at_beat_rel: 8. }));
    assert!(items.contains(&PlanItem::Loop { deck: In, start_beat: 18., beats: 2., at_beat_rel: 8. }));
    // the jump goes 8 beats back from 18 and takes the loop along: 10
    assert!(items.contains(&PlanItem::Jump { deck: In, to_beat: 10., at_beat_rel: 8. }));
    // a loop while one is on resizes it and keeps its start (as the loop button does)
    assert!(items.contains(&PlanItem::Loop { deck: In, start_beat: 10., beats: 1., at_beat_rel: 12. }));
    // with no loop on, the loop starts where the deck is: aft at 512 + 16
    let c = good(&mix("  smk play from bar 3\n  bar 2 smk loop 2 beats; smk loop off\n  bar 3 smk loop 1 beats\n"));
    assert!(c.plans[0].items.contains(&PlanItem::Loop { deck: In, start_beat: 22., beats: 1., at_beat_rel: 12. }));
}

#[test]
fn unsynced_incoming_advances_at_its_own_tempo() {
    let c = good(&mix("  smk play from bar 1 unsynced\n  bar 8 smk loop 4 beats\n"));
    let p = &c.plans[0];
    assert!(matches!(p.items[0], PlanItem::Play { synced: false, from_beat, .. } if from_beat == 2.));
    // 32 clock beats at 112 BPM; smk runs at 126: 32 * 126/112 = 36 beats on top of beat 2
    assert!(p.items.contains(&PlanItem::Loop { deck: Deck::In, start_beat: 38., beats: 4., at_beat_rel: 32. }));
}

#[test]
fn plans_serialize_to_json() {
    let c = good(SPEC);
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v["plans"][0]["items"][0]["kind"], "play");
    assert_eq!(v["plans"][0]["items"][2]["control"], "fader");
    assert_eq!(v["set"]["tracks"][0]["id"], "aft");
}

#[test]
fn the_demo_set_loads() {
    let mut env = FakeEnv { root: "/music".into(), ..Default::default() };
    env.add("Pneumatix", "Aftertekno", "pn/aftertekno.flac", Some(grid(124., 0.1, 0)));
    env.add("Aasi & Swooh", "Smokin", "smokin.flac", Some(grid(126., 0.2, 1)));
    env.add("Kaiku", "Noita", "noita.flac", Some(grid(132., 0.3, 0)));
    let l = load(Path::new("demo.codj"), include_str!("../../examples/sets/demo.codj"), &env);
    assert!(l.diags.is_empty(), "{:#?}", l.diags);
    let c = l.compiled.unwrap();
    assert_eq!(c.set.name, "demo: 3 tracks");
    assert_eq!(c.plans.len(), 2);
    // `grid /2`: detected as double time
    assert_eq!(c.set.track("peak").unwrap().grid.as_ref().unwrap().bpm, 66.);
}

// ---- recipes ----

use super::recipes;

/// like `HEAD`, with a `drop` cue on smk (bar 17: beat 2 + 64 = 66 on its grid)
const HEAD_DROP: &str = "track aft \"Pneumatix - Aftertekno.flac\"\n  cue outro bar 129\n  cue drop bar 33\ntrack smk \"Smokin\"\n  cue intro bar 3\n  cue drop bar 17\n";

fn recipe_mix(head: &str, header_rest: &str, body: &str) -> String {
    format!("{head}mix aft -> smk at aft.outro recipe {header_rest}\n{body}")
}

fn plan_of(src: &str) -> super::MixPlan {
    let mut c = good(src);
    assert_eq!(c.plans.len(), 1);
    c.plans.remove(0)
}

fn items_of(head: &str, header_rest: &str, body: &str) -> Vec<PlanItem> {
    plan_of(&recipe_mix(head, header_rest, body)).items
}

/// smk's first downbeat: grid beat 2 (its first beat is beat 2 of the bar)
const SMK0: f64 = 2.;

fn ramp(deck: Deck, control: Control, from: Option<f32>, to: f32, s: f64, e: f64, curve: Curve) -> PlanItem {
    PlanItem::Ramp { deck, control, from, to, start_beat_rel: s, end_beat_rel: e, curve }
}

#[test]
fn blend_compiles_to_the_expected_plan() {
    use Control::*;
    use Deck::*;
    let p = plan_of(&recipe_mix(HEAD, "blend", ""));
    // 32 bars of 4 beats, the swap in the middle
    assert_eq!(p.length_beats, 128.);
    assert_eq!(
        p.items,
        vec![
            PlanItem::Play { from_beat: SMK0, synced: true, at_beat_rel: 0. },
            PlanItem::Set { deck: In, control: Low, value: -1., at_beat_rel: 0. },
            ramp(In, Fader, Some(0.), 1., 0., 64., Curve::Linear),
            PlanItem::Swap { band: Low, at_beat_rel: 64. },
            ramp(Out, Filter, Some(0.), -0.6, 64., 128., Curve::Smooth),
            ramp(Out, Control::Fader, Some(1.), 0., 64., 128., Curve::Linear),
            PlanItem::Pause { deck: Out, at_beat_rel: 128. },
        ]
    );
    // the outgoing deck stops at the very end: nothing later to re-time
    assert!(p.in_clock.is_none());
}

#[test]
fn bassswap_compiles_to_the_expected_plan() {
    use Control::*;
    use Deck::*;
    let p = plan_of(&recipe_mix(HEAD, "bassswap", ""));
    assert_eq!(p.length_beats, 64.);
    assert_eq!(
        p.items,
        vec![
            PlanItem::Play { from_beat: SMK0, synced: true, at_beat_rel: 0. },
            PlanItem::Set { deck: In, control: Low, value: -1., at_beat_rel: 0. },
            ramp(In, Fader, Some(0.), 1., 0., 8., Curve::Linear), // full volume within 2 bars
            PlanItem::Swap { band: Low, at_beat_rel: 32. },       // bar 8 of 16
            ramp(Out, Filter, Some(0.), -0.5, 48., 64., Curve::Smooth),
            ramp(Out, Control::Fader, Some(1.), 0., 48., 64., Curve::Linear),
            PlanItem::Pause { deck: Out, at_beat_rel: 64. },
        ]
    );
}

#[test]
fn echo_out_loops_shrink_then_the_new_track_enters() {
    use Control::*;
    use Deck::*;
    let aft0 = 512.; // the mix starts on aft bar 129
    let tail = |enter: f64| {
        vec![
            PlanItem::Play { from_beat: SMK0, synced: true, at_beat_rel: 0. },
            PlanItem::Set { deck: In, control: Control::Fader, value: 0., at_beat_rel: 0. },
            // bar 5 of 8: loop 4 beats (it starts where the deck is, the next loops keep that start)
            PlanItem::Loop { deck: Out, start_beat: aft0 + 20., beats: 4., at_beat_rel: 20. },
            ramp(Out, Filter, Some(0.), 0.8, 20., 30., Curve::Exp),
            ramp(Out, Control::Fader, Some(1.), 0., 24., 30., Curve::Linear),
            PlanItem::Loop { deck: Out, start_beat: aft0 + 20., beats: 2., at_beat_rel: 28. },
            PlanItem::Loop { deck: Out, start_beat: aft0 + 20., beats: 1., at_beat_rel: 30. },
            PlanItem::LoopOff { deck: Out, at_beat_rel: 31. },
            PlanItem::Pause { deck: Out, at_beat_rel: 32. },
            PlanItem::Jump { deck: In, to_beat: enter, at_beat_rel: 32. },
            PlanItem::Set { deck: In, control: Control::Fader, value: 1., at_beat_rel: 32. },
        ]
    };
    // no `drop` cue on smk: back to its first downbeat (it has run silently for 32 beats)
    let p = plan_of(&recipe_mix(HEAD, "echo-out", ""));
    assert_eq!(p.length_beats, 32.);
    assert_eq!(p.items, tail(SMK0));
    // from the first loop on, the incoming deck keeps the time
    assert_eq!(p.in_clock.as_ref().unwrap().after, 20.);
    // with one: bar 17 is beat 2 + 64
    let p = plan_of(&recipe_mix(HEAD_DROP, "echo-out", ""));
    assert_eq!(p.items, tail(SMK0 + 64.));
}

#[test]
fn cut_starts_and_stops_on_the_same_beat() {
    use Deck::*;
    let p = plan_of(&recipe_mix(HEAD, "cut", ""));
    assert_eq!(p.length_beats, 4.);
    assert_eq!(
        p.items,
        vec![
            PlanItem::Play { from_beat: SMK0, synced: true, at_beat_rel: 0. },
            PlanItem::Set { deck: In, control: Control::Fader, value: 1., at_beat_rel: 0. },
            PlanItem::Pause { deck: Out, at_beat_rel: 0. },
        ]
    );
    // the outgoing clock stops at once; the one bar of the mix is counted on the incoming deck
    assert_eq!(p.in_clock.unwrap().after, 0.);
}

#[test]
fn filter_blend_crosses_the_filters_and_the_faders() {
    use Control::*;
    use Deck::*;
    let p = plan_of(&recipe_mix(HEAD, "filter-blend", ""));
    assert_eq!(p.length_beats, 64.);
    assert_eq!(
        p.items,
        vec![
            PlanItem::Play { from_beat: SMK0, synced: true, at_beat_rel: 0. },
            ramp(In, Filter, Some(0.7), 0., 0., 64., Curve::Smooth),
            ramp(Out, Filter, Some(0.), -0.7, 0., 64., Curve::Smooth),
            ramp(In, Fader, Some(0.), 1., 0., 64., Curve::Linear),
            ramp(Out, Control::Fader, Some(1.), 0., 0., 64., Curve::Linear),
            PlanItem::Pause { deck: Out, at_beat_rel: 64. },
        ]
    );
}

#[test]
fn the_header_forms_and_their_errors() {
    // the length is optional with a recipe, required without
    assert!(run(&recipe_mix(HEAD, "blend", "")).diags.is_empty());
    assert!(run(&recipe_mix(HEAD, "blend over 16 bars", "")).diags.is_empty());
    let e = errors(&format!("{HEAD}mix aft -> smk at aft.outro\n"));
    assert_eq!(e, ["9:28: expected `over <n> bars` or `recipe <name>`, found end of line"]);
    let e = errors(&recipe_mix(HEAD, "", ""));
    assert_eq!(e, ["9:35: expected a recipe name after `recipe`, found end of line"]);
    let e = errors(&recipe_mix(HEAD, "blnd", ""));
    assert_eq!(e, ["9:36: unknown recipe \"blnd\" (recipes: blend, bassswap, echo-out, cut, filter-blend, loop-roll, brake, eq-swap)"]);
    let e = errors(&recipe_mix(HEAD, "blend over 16", ""));
    assert!(e[0].contains("expected `bars` after `over 16`"), "{e:?}");
    // `at bar` works as well
    let l = run(&format!("{HEAD}mix aft -> smk at bar 129 recipe cut\n"));
    assert!(l.diags.is_empty(), "{:?}", l.diags);
}

#[test]
fn a_length_override_scales_the_recipe_and_minimums_apply() {
    use Control::*;
    use Deck::*;
    // blend over 16 bars: the swap at bar 8
    let items = items_of(HEAD, "blend over 16 bars", "");
    assert!(items.contains(&PlanItem::Swap { band: Low, at_beat_rel: 32. }));
    assert!(items.contains(&PlanItem::Pause { deck: Out, at_beat_rel: 64. }));
    assert!(items.contains(&ramp(In, Fader, Some(0.), 1., 0., 32., Curve::Linear)));
    // the shortest blend is 8 bars
    let items = items_of(HEAD, "blend over 8 bars", "");
    assert!(items.contains(&PlanItem::Swap { band: Low, at_beat_rel: 16. }));
    let e = errors(&recipe_mix(HEAD, "blend over 7 bars", ""));
    assert_eq!(e, ["9:47: recipe blend needs at least 8 bars (got 7)"]);
    for (name, min) in [("bassswap", 4), ("echo-out", 4), ("cut", 1), ("filter-blend", 8), ("loop-roll", 4), ("brake", 1), ("eq-swap", 12)] {
        let ok = errors(&recipe_mix(HEAD, &format!("{name} over {min} bars"), ""));
        assert!(ok.is_empty(), "{name} at its minimum: {ok:?}");
        if min > 1 {
            let bad = errors(&recipe_mix(HEAD, &format!("{name} over {} bars", min - 1), ""));
            assert_eq!(bad.len(), 1, "{name}: {bad:?}");
            assert!(bad[0].contains(&format!("recipe {name} needs at least {min} bars")), "{bad:?}");
        }
    }
    let e = errors(&recipe_mix(HEAD, "cut over 0 bars", ""));
    assert!(e[0].contains("above 0"), "{e:?}");
    // the longer the mix the later the echo: echo-out over 12 bars loops from bar 9
    let items = items_of(HEAD, "echo-out over 12 bars", "");
    assert!(items.iter().any(|i| matches!(i, PlanItem::Loop { at_beat_rel, beats, .. } if *at_beat_rel == 36. && *beats == 4.)));
    // every recipe's own mix stays valid at lengths that are not multiples of anything
    for r in recipes::RECIPES {
        for bars in [r.min_bars, r.min_bars + 1, r.default_bars + 3, 100] {
            let e = errors(&recipe_mix(HEAD, &format!("{} over {bars} bars", r.name), ""));
            assert!(e.is_empty(), "{} over {bars}: {e:?}", r.name);
        }
    }
}

#[test]
fn a_block_line_replaces_the_recipe_items_for_its_control() {
    use Control::*;
    use Deck::*;
    let base = items_of(HEAD, "blend", "");
    // smk.fader over the first 8 bars instead: the recipe's smk fader ramp goes, nothing else
    let items = items_of(HEAD, "blend", "  bars 0..8 smk.fader 0 -> 1\n");
    let fader_in = |i: &PlanItem| matches!(i, PlanItem::Ramp { deck: In, control: Fader, .. });
    assert_eq!(base.iter().filter(|i| fader_in(i)).count(), 1);
    assert_eq!(items.iter().filter(|i| fader_in(i)).count(), 1);
    assert!(items.contains(&ramp(In, Fader, Some(0.), 1., 0., 32., Curve::Linear)));
    assert!(!items.contains(&ramp(In, Fader, Some(0.), 1., 0., 64., Curve::Linear)));
    assert_eq!(items.len(), base.len());
    for it in &base {
        if !fader_in(it) {
            assert!(items.contains(it), "lost {it:?}");
        }
    }
    // a different time does not save the recipe's item: the control is what counts
    let items = items_of(HEAD, "blend", "  bar 30 smk.fader = 1\n");
    assert!(!items.iter().any(|i| matches!(i, PlanItem::Ramp { deck: In, control: Fader, .. })));
    assert!(items.contains(&PlanItem::Set { deck: In, control: Fader, value: 1., at_beat_rel: 120. }));
    // the swap is its own item: moving it keeps the kill of the incoming bass
    let items = items_of(HEAD, "blend", "  bar 20 swap low\n");
    assert_eq!(items.iter().filter(|i| matches!(i, PlanItem::Swap { .. })).count(), 1);
    assert!(items.contains(&PlanItem::Swap { band: Low, at_beat_rel: 80. }));
    assert!(items.contains(&PlanItem::Set { deck: In, control: Low, value: -1., at_beat_rel: 0. }));
    // the other band is not the recipe's swap
    let items = items_of(HEAD, "blend", "  bar 20 swap mid\n");
    assert_eq!(items.iter().filter(|i| matches!(i, PlanItem::Swap { .. })).count(), 2);
    // `stop` moves the stop; both decks' own lines stay apart
    let items = items_of(HEAD, "blend", "  bar 30 aft stop\n");
    let stops: Vec<_> = items.iter().filter(|i| matches!(i, PlanItem::Pause { .. })).collect();
    assert_eq!(stops, [&PlanItem::Pause { deck: Out, at_beat_rel: 120. }]);
    // the recipe's items come first at the same time, the block's after them
    let items = items_of(HEAD, "blend", "  bar 0 smk.mid = -0.5\n");
    let at0: Vec<_> = items.iter().filter(|i| i.start_beat_rel() == 0.).collect();
    assert_eq!(at0.last().unwrap(), &&PlanItem::Set { deck: In, control: Mid, value: -0.5, at_beat_rel: 0. });
    // a `play` line replaces the recipe's, a second one is still an error
    let items = items_of(HEAD, "blend", "  smk play from intro unsynced\n");
    assert_eq!(items[0], PlanItem::Play { from_beat: SMK0 + 8., synced: false, at_beat_rel: 0. });
    assert_eq!(items.iter().filter(|i| matches!(i, PlanItem::Play { .. })).count(), 1);
    let e = errors(&recipe_mix(HEAD, "blend", "  smk play\n  smk play\n"));
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains("a second `play`"), "{e:?}");
    // errors in the block's lines are reported at the block's lines
    let e = errors(&recipe_mix(HEAD, "blend", "  bar 40 aft stop\n"));
    assert_eq!(e, ["10:7: bar 40 is outside the mix (0..32 bars)"]);
    // the crossfader is one control
    let items = items_of(HEAD, "filter-blend", "  bars 0..16 xfade -1 -> 1\n");
    assert_eq!(items.iter().filter(|i| matches!(i, PlanItem::Ramp { control: Control::Crossfader, .. })).count(), 1);
}

#[test]
fn off_phrase_recipe_mixes_get_a_warning_not_an_error() {
    let warn = |src: &str| {
        let l = run(src);
        assert!(l.diags.is_empty(), "{:?}", l.diags);
        assert!(l.compiled.is_some());
        l.warnings.iter().map(|d| format!("{}:{}: {}", d.line, d.col, d.msg)).collect::<Vec<_>>()
    };
    let mix_at = |at: &str| format!("{HEAD}mix aft -> smk at {at} recipe blend\n");
    assert!(warn(&mix_at("bar 33")).is_empty());
    assert!(warn(&mix_at("bar 1")).is_empty());
    assert!(warn(&mix_at("aft.outro")).is_empty()); // bar 129
    assert_eq!(
        warn(&mix_at("bar 37")),
        ["9:23: recipe blend starts at bar 37: mixes usually start on a phrase (bar 33 or 41)"]
    );
    assert_eq!(
        warn(&mix_at("bar 33 beat 3")),
        ["9:23: recipe blend starts at bar 33 beat 3: mixes usually start on a phrase (bar 33 or 41)"]
    );
    // a cue off the grid warns at the track name of `at`
    let src = format!("track aft \"Pneumatix - Aftertekno.flac\"\n  cue x bar 22\ntrack smk \"Smokin\"\nmix aft -> smk at aft.x recipe cut\n");
    assert_eq!(warn(&src), ["4:19: recipe cut starts at bar 22: mixes usually start on a phrase (bar 17 or 25)"]);
    // only recipes are held to it, and the warning does not touch the errors
    assert!(warn(&format!("{HEAD}mix aft -> smk at bar 37 over 8 bars\n")).is_empty());
    assert!(warn(&mix_at("bar 37")).len() == 1 && run(&mix_at("bar 37")).diags.is_empty());
}

#[test]
fn the_expanded_text_compiles_back_to_the_same_plan() {
    for r in recipes::RECIPES {
        for (head, drop) in [(HEAD, false), (HEAD_DROP, true)] {
            for bars in [0, r.min_bars, r.default_bars + 5] {
                let ctx = recipes::Ctx { a: "aft", b: "smk", b_has_drop: drop, beats_per_bar: 4. };
                let text = recipes::expand(r.name, "aft", "smk", bars, &ctx).unwrap();
                let n = if bars == 0 { r.default_bars } else { bars };
                // the text starts at bar 1; the recipe form is put at the same place
                let want = plan_of(&format!("{head}mix aft -> smk at bar 1 recipe {} over {n} bars\n", r.name));
                let got = plan_of(&format!("{head}{text}"));
                assert_eq!(got, want, "{} over {n} (drop {drop})\n{text}", r.name);
                // it is a block the editor can show: a header comment, one comment per group
                assert!(text.starts_with(&format!("# {}: {}\n", r.name, r.explain)), "{text}");
                assert!(text.contains("\n  # "), "{text}");
                assert!(text.lines().all(|l| l.is_empty() || l.starts_with('#') || l.starts_with("mix ") || l.starts_with("  ")));
            }
        }
    }
    let ctx = recipes::Ctx { a: "a", b: "b", b_has_drop: false, beats_per_bar: 4. };
    assert!(recipes::expand("nope", "a", "b", 0, &ctx).unwrap_err().contains("unknown recipe"));
    assert_eq!(recipes::expand("blend", "a", "b", 4, &ctx).unwrap_err(), "recipe blend needs at least 8 bars (got 4)");
    // a drop cue is used when the incoming track has one
    let with = recipes::expand("echo-out", "a", "b", 8, &recipes::Ctx { b_has_drop: true, ..ctx }).unwrap();
    assert!(with.contains("b jump drop") && !with.contains("jump -"), "{with}");
    let without = recipes::expand("echo-out", "a", "b", 8, &ctx).unwrap();
    assert!(without.contains("b jump -32 beats"), "{without}");
}

#[test]
fn recipes_are_described_for_a_beginner() {
    assert_eq!(recipes::RECIPES.map(|r| (r.name, r.default_bars)), [
        ("blend", 32),
        ("bassswap", 16),
        ("echo-out", 8),
        ("cut", 1),
        ("filter-blend", 16),
        ("loop-roll", 8),
        ("brake", 2),
        ("eq-swap", 24),
    ]);
    for r in recipes::RECIPES {
        assert!(!r.explain.contains('\n') && r.explain.ends_with('.'), "{}", r.name);
        assert!(r.min_bars >= 1 && r.min_bars <= r.default_bars);
    }
    assert_eq!(recipes::find("blend").unwrap().explain, "A long, smooth overlap: never two basslines at once.");
    assert!(recipes::find("cut").unwrap().explain.starts_with("Hard cut"));
}

#[test]
fn the_recipes_example_loads_without_warnings() {
    let mut env = FakeEnv { root: "/music".into(), ..Default::default() };
    env.add("Pneumatix", "Aftertekno", "pn/aftertekno.flac", Some(grid(124., 0.1, 0)));
    env.add("Aasi & Swooh", "Smokin", "smokin.flac", Some(grid(126., 0.2, 1)));
    env.add("Kaiku", "Noita", "noita.flac", Some(grid(132., 0.3, 0)));
    env.add("Sirkka", "Pakkanen", "pakkanen.flac", Some(grid(128., 0.1, 0)));
    env.add("Tuuli", "Myrsky", "myrsky.flac", Some(grid(130., 0.1, 0)));
    let l = load(Path::new("recipes.codj"), include_str!("../../examples/sets/recipes.codj"), &env);
    assert!(l.diags.is_empty(), "{:#?}", l.diags);
    assert!(l.warnings.is_empty(), "{:#?}", l.warnings);
    assert_eq!(l.compiled.unwrap().plans.len(), 5);
}

// ---- patch decks ----

const PATCHED: &str = r#"
patch acid "acid" slot 1
patch kit "drums" slot 3
patch pad "pad" slot 4

track aft "Pneumatix - Aftertekno.flac"
  cue outro bar 129
track smk "Smokin"

mix aft -> smk at aft.outro over 32 bars
  bars 0..16   acid.cutoff 0.2 -> 0.8 smooth; acid.fader 0 -> 0.7; kit.swing = 0.5
  bar 8        acid pattern B; kit.low kill
  bar 16       acid live; kit.gain = -3
  bar 24       acid cue; pad.mix = 1
  bars 16..32  acid.reso 0 -> 0.9
"#;

#[test]
fn patch_lines_and_items_compile_to_plan_items() {
    let c = good(&PATCHED);
    assert_eq!(c.set.patches.len(), 3);
    let acid = c.set.patch("acid").unwrap();
    // `slot 1` is P1, engine slot 0
    assert_eq!((acid.name.as_str(), acid.slot), ("acid", 0));
    assert_eq!(acid.knobs, vec![("cutoff".to_string(), 0., 1.), ("reso".to_string(), 0., 0.95)]);
    assert_eq!(acid.sequencers, vec!["seq".to_string()]);
    assert_eq!(c.set.patch("kit").unwrap().slot, 2);
    assert_eq!(c.set.patch("pad").unwrap().slot, 3);

    let items = &c.plans[0].items;
    let has = |i: PlanItem| assert!(items.contains(&i), "{i:?} not in {items:#?}");
    // bars 0..16 is 64 beats (4 beats per bar), ramps over the range; the default curve of a knob is linear
    has(PlanItem::KnobRamp { slot: 0, knob: 0, name: "cutoff".into(), from: Some(0.2), to: 0.8, start_beat_rel: 0., end_beat_rel: 64., curve: Curve::Smooth });
    // the mixer of a patch deck: fader, with the control's own default curve
    has(PlanItem::SlotRamp { slot: 0, control: Control::Fader, from: Some(0.), to: 0.7, start_beat_rel: 0., end_beat_rel: 64., curve: Curve::Linear });
    has(PlanItem::KnobSet { slot: 2, knob: 0, name: "swing".into(), value: 0.5, at_beat_rel: 0. });
    // a pattern switch goes to every sequencer of the patch
    has(PlanItem::Pattern { slot: 0, seq: 0, name: "seq".into(), pattern: 1, at_beat_rel: 32. });
    // kill is a mixer band
    has(PlanItem::SlotSet { slot: 2, control: Control::Low, value: -1., at_beat_rel: 32. });
    has(PlanItem::Live { slot: 0, live: true, at_beat_rel: 64. });
    has(PlanItem::SlotSet { slot: 2, control: Control::Gain, value: -3., at_beat_rel: 64. });
    has(PlanItem::Live { slot: 0, live: false, at_beat_rel: 96. });
    has(PlanItem::KnobSet { slot: 3, knob: 0, name: "mix".into(), value: 1., at_beat_rel: 96. });
    // the plan stays sorted by time
    let starts: Vec<f64> = items.iter().map(|i| i.start_beat_rel()).collect();
    assert!(starts.windows(2).all(|w| w[0] <= w[1]), "{starts:?}");

    // a patch with several sequencers: one item each
    let c = good(&PATCHED.replace("acid pattern B", "kit pattern c"));
    let p: Vec<_> = c.plans[0].items.iter().filter(|i| matches!(i, PlanItem::Pattern { .. })).collect();
    assert_eq!(p.len(), 2);
    assert!(matches!(p[0], PlanItem::Pattern { slot: 2, seq: 0, pattern: 2, .. }));
    assert!(matches!(p[1], PlanItem::Pattern { slot: 2, seq: 1, pattern: 2, .. }));
}

#[test]
fn unknown_knobs_name_the_choices() {
    let e = errors(&PATCHED.replace("acid.cutoff 0.2", "acid.cutof 0.2"));
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(
        e[0].contains("unknown knob \"cutof\" on patch acid (knobs: cutoff, reso; mixer: fader, low, mid, high, filter, gain)"),
        "{e:?}"
    );
    // a patch without knobs says none
    let e = errors(&PATCHED.replace("pad.mix = 1", "pad.nope = 1"));
    assert!(e[0].contains("on patch pad (knobs: mix;"), "{e:?}");
}

#[test]
fn patch_item_errors() {
    let e = |from: &str, to: &str| errors(&PATCHED.replace(from, to));
    let one = |v: Vec<String>, msg: &str| {
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].contains(msg), "{v:?}");
    };
    // ranges come from the patch file (reso 0..0.95) and the mixer control
    one(e("kit.swing = 0.5", "acid.reso = 1"), "acid.reso 1 is outside 0..0.95");
    one(e("acid.fader 0 -> 0.7", "acid.fader 0 -> 2"), "acid.fader 2 is outside 0..1");
    one(e("kit.gain = -3", "kit.gain = -60"), "outside -48..12");
    one(e("kit.low kill", "kit.swing kill"), "`kill` works on low, mid and high, not swing");
    // a ramp needs a range line; a bad curve
    one(e("bar 8        acid pattern B", "bar 8        acid.cutoff 0 -> 1"), "a ramp needs a `bars <i>..<j>` line");
    one(e("0.2 -> 0.8 smooth", "0.2 -> 0.8 wavy"), "unknown curve \"wavy\"");
    // patterns are A-H; the patch needs a sequencer
    one(e("acid pattern B", "acid pattern J"), "patterns are A to H");
    one(e("acid pattern B", "acid pattern AB"), "patterns are A to H");
    one(e("acid pattern B", "pad pattern A"), "patch pad has no sequencer");
    // track-only actions
    one(e("acid live", "acid stop"), "`acid` is a patch: it takes");
    one(e("acid live", "acid loop 4 beats"), "`acid` is a patch");
    // an undeclared patch
    one(e("acid live", "lead live"), "no patch \"lead\" (declare it with `patch lead");
    one(e("acid pattern B", "lead pattern B"), "no patch \"lead\"");
    // ids and slots
    one(errors(&PATCHED.replace("patch kit \"drums\" slot 3", "patch kit \"drums\" slot 5")), "slots are 1..4 (P1..P4), got 5");
    let both = errors(&PATCHED.replace("patch kit \"drums\" slot 3", "patch kit \"drums\" slot 1"));
    assert!(both.iter().any(|m| m.contains("slot 1 is taken by patch acid")), "{both:?}");
    let dup = errors(&PATCHED.replace("patch kit \"drums\"", "patch aft \"drums\""));
    assert!(dup.iter().any(|m| m.contains("\"aft\" is already a track or a patch id")), "{dup:?}");
    let dup = errors(&PATCHED.replace("patch kit \"drums\"", "patch acid \"drums\""));
    assert!(dup.iter().any(|m| m.contains("\"acid\" is already a track or a patch id")), "{dup:?}");
    // a refused declaration doesn't bring an error for every item that uses it
    let bad = errors(&PATCHED.replace("slot 3", "slot 9"));
    assert_eq!(bad.len(), 1, "{bad:?}");
    // no such patch file
    let m = errors(&PATCHED.replace("\"drums\"", "\"drumz\""));
    assert!(m.iter().any(|x| x.contains("patch \"drumz\": no patch \"drumz\" (patches: acid, drums, pad)")), "{m:?}");
}

#[test]
fn patches_need_a_patch_folder_and_warn_about_shadowed_knobs() {
    let mut env = spec_env();
    env.has_patch_dir = false;
    let l = load(Path::new("/sets/x.codj"), PATCHED, &env);
    assert!(l.diags.iter().any(|d| d.msg.contains("patches are not available here")), "{:?}", l.diags);

    // a knob named like a mixer control (possible in files from before the rule): a warning
    let mut env = spec_env();
    env.patches.insert("acid".into(), super::PatchInfo { knobs: vec![("gain".into(), 0., 1.)], sequencers: vec![] });
    let l = load(Path::new("/sets/x.codj"), "patch acid \"acid\" slot 1\n", &env);
    assert!(l.diags.is_empty());
    assert!(l.warnings[0].msg.contains("knob `gain` of patch \"acid\" has the name of a mixer control"), "{:?}", l.warnings);

    // `rate` is a track deck's tempo, not a patch deck's: a knob called rate (an LFO's) stays a knob
    let mut env = spec_env();
    env.patches.insert("acid".into(), super::PatchInfo { knobs: vec![("rate".into(), 0., 10.)], sequencers: vec![] });
    let src = format!("patch acid \"acid\" slot 1\n{HEAD}mix aft -> smk at aft.outro over 8 bars\n  bars 0..4 acid.rate 1 -> 8\n");
    let l = load(Path::new("/sets/x.codj"), &src, &env);
    assert!(l.diags.is_empty() && l.warnings.is_empty(), "{:?} {:?}", l.diags, l.warnings);
    let items = &l.compiled.unwrap().plans[0].items;
    assert!(items.iter().any(|i| matches!(i, PlanItem::KnobRamp { name, to, .. } if name == "rate" && *to == 8.)), "{items:?}");
}

#[test]
fn patch_items_merge_with_a_recipe_like_track_items() {
    use super::resolve::merge;
    use super::model::{Action, MixItem};
    let item = |action| MixItem { at_bar: 0., end_bar: None, action };
    let recipe = vec![
        item(Action::KnobSet { slot: 0, knob: 0, name: "cutoff".into(), value: 0.1 }),
        item(Action::KnobSet { slot: 0, knob: 1, name: "reso".into(), value: 0.1 }),
        item(Action::Live { slot: 0, live: true }),
        item(Action::SlotSet { slot: 0, control: Control::Fader, value: 1. }),
    ];
    let own = vec![item(Action::KnobRamp { slot: 0, knob: 0, name: "cutoff".into(), from: None, to: 1., curve: Curve::Linear }), item(Action::Live { slot: 0, live: false })];
    let m = merge(recipe, own);
    // the recipe's cutoff and live are replaced, the others stay
    assert_eq!(m.len(), 4);
    assert!(matches!(m[0].action, Action::KnobSet { knob: 1, .. }));
    assert!(matches!(m[1].action, Action::SlotSet { .. }));
    assert!(matches!(m[2].action, Action::KnobRamp { .. }));
    assert!(matches!(m[3].action, Action::Live { live: false, .. }));
}

#[test]
fn the_patch_examples_are_valid_and_the_example_set_loads() {
    use crate::patch::{doc, store::info_of};
    let files = [
        ("acid", include_str!("../../examples/patches/acid.json")),
        ("drums", include_str!("../../examples/patches/drums.json")),
        ("pad", include_str!("../../examples/patches/pad.json")),
    ];
    let mut env = FakeEnv { root: "/music".into(), has_patch_dir: true, ..Default::default() };
    for (name, text) in files {
        let errors = doc::check(text);
        assert!(errors.is_empty(), "{name}: {errors:#?}");
        // formatted like our own writer would (two-space indent is a convention, not a rule)
        assert!(doc::parse(text).unwrap().to_json().contains("\"codj_patch\": 1"));
        env.patches.insert(name.to_string(), info_of(text).unwrap());
    }
    // what the example set addresses
    let acid = &env.patches["acid"];
    assert_eq!(acid.knobs.iter().map(|k| k.0.as_str()).collect::<Vec<_>>(), ["decay", "cutoff", "reso", "envmod"]);
    assert_eq!(acid.sequencers, ["seq"]);
    assert_eq!(env.patches["drums"].sequencers, ["kit"]);
    assert!(env.patches["pad"].sequencers.is_empty());

    env.add("Pneumatix", "Aftertekno", "aftertekno.flac", Some(grid(124., 0.1, 0)));
    env.add("Aasi & Swooh", "Smokin", "smokin.flac", Some(grid(126., 0.2, 1)));
    let l = load(Path::new("patches.codj"), include_str!("../../examples/sets/patches.codj"), &env);
    assert!(l.diags.is_empty(), "{:#?}", l.diags);
    assert!(l.warnings.is_empty(), "{:#?}", l.warnings);
    let c = l.compiled.unwrap();
    assert_eq!(c.set.patches.iter().map(|p| (p.id.as_str(), p.slot)).collect::<Vec<_>>(), [("kit", 0), ("acid", 1), ("pad", 2)]);
    let items = &c.plans[0].items;
    // pattern B on the kit's one sequencer and the acid's
    assert_eq!(items.iter().filter(|i| matches!(i, PlanItem::Pattern { pattern: 1, .. })).count(), 2);
    assert!(items.iter().any(|i| matches!(i, PlanItem::Live { slot: 1, live: true, .. })));
}

// ---- the track model behind `<track>.<name>` (docs/track-sheets.md, "Sets") ----

mod track_model {
    use super::*;
    use crate::{
        analysis::Grid,
        sets::model::Action,
        tracks::{
            sections::{Kind, Section},
            store::{check_text, ReadSheet},
            view::{view, Observations},
            Observation, Value,
        },
    };

    const AFT: &str = "/music/Pneumatix - Aftertekno.flac";
    const SMK: &str = "/music/Aasi & Swooh - Smokin.flac";
    const ID: &str = "a1:00000000000000000000000000000001";

    /// a measured section from bar `a` for `len` bars on grid `g`
    fn section(g: &Grid, name: &str, kind: Kind, a: u32, len: u32, confidence: f64) -> Section {
        let bar = 60. / g.bpm * g.beats_per_bar as f64;
        let t0 = g.first_beat_s + g.downbeat_offset as f64 * 60. / g.bpm;
        Section {
            name: name.into(),
            kind,
            start_s: t0 + (a - 1) as f64 * bar,
            end_s: t0 + (a - 1 + len) as f64 * bar,
            start_bar: a,
            len_bars: len,
            energy: [5, 5, 5],
            confidence,
        }
    }

    /// spec_env with views: aft has an intro, a drop at bar 33 and an outro at bar 129 (shaky);
    /// smk an intro and a drop at bar 17; `sheets` per file
    fn env(aft_sheet: Option<&str>, smk_sheet: Option<&str>) -> FakeEnv {
        let mut e = spec_env();
        let ga = e.grids[Path::new(AFT)].clone();
        let gs = e.grids[Path::new(SMK)].clone();
        let mut add = |file: &str, g: &Grid, sections: Vec<Section>, sheet: Option<&str>| {
            let obs = Observations {
                id: ID.into(),
                duration_s: Some(600.),
                loudness: Some((-9., -2.)),
                grid: Some(g.clone()),
                grid_factor: 1.,
                key: None,
                sections: Some(Observation { producer: crate::tracks::sections::producer(), value: Value::Known(sections) }),
            };
            let read = sheet.map(|t| {
                let text = format!("version 1\naudio {ID}\n{t}");
                let sheet = check_text("x.codj", &text, None);
                assert!(sheet.is_ok(), "{sheet:?}");
                ReadSheet { file: "/tracks/x.codj".into(), sheet, others: vec![] }
            });
            e.views.insert(PathBuf::from(file), view(&obs, read.as_ref(), &[]));
        };
        add(
            AFT,
            &ga,
            vec![
                section(&ga, "intro", Kind::Intro, 1, 32, 0.9),
                section(&ga, "drop", Kind::Drop, 33, 32, 0.9),
                section(&ga, "outro", Kind::Outro, 129, 32, 0.3),
            ],
            aft_sheet,
        );
        add(SMK, &gs, vec![section(&gs, "intro", Kind::Intro, 1, 16, 0.9), section(&gs, "drop", Kind::Drop, 17, 32, 0.8)], smk_sheet);
        e
    }

    fn load_with(e: &FakeEnv, src: &str) -> Loaded {
        load(Path::new("/sets/k.codj"), src, e)
    }

    fn ok(e: &FakeEnv, src: &str) -> (super::super::Compiled, Vec<String>) {
        let l = load_with(e, src);
        assert!(l.diags.is_empty(), "{:#?}", l.diags);
        (l.compiled.unwrap(), l.warnings.iter().map(|d| format!("{}:{}: {}", d.line, d.col, d.msg)).collect())
    }

    const TRACKS: &str = "track aft \"Pneumatix - Aftertekno.flac\"\ntrack smk \"Smokin\"\n";

    #[test]
    fn measured_sections_need_no_hand_placed_cues() {
        let (c, w) = ok(&env(None, None), &format!("{TRACKS}mix aft -> smk at aft.outro recipe blend\n"));
        // bar 129 on aft's grid
        assert_eq!(c.set.mixes[0].start_beat, 512.);
        // the outro was measured with confidence 0.3: worth a look
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].starts_with("3:23: aft.outro is a measured outro with confidence 0.3"), "{w:?}");
        // play from a measured section of the incoming track, jump to one
        let (c, w) = ok(&env(None, None), &format!("{TRACKS}mix aft -> smk at aft.intro over 8 bars\n  smk play from drop\n  bar 4 aft jump drop\n"));
        assert!(w.is_empty(), "{w:?}");
        let m = &c.set.mixes[0];
        // smk's bar 17: grid beat 2 + 64
        assert_eq!(m.play_from_beat, 66.);
        assert!(m.items.iter().any(|i| i.action == Action::JumpTo { deck: Deck::Out, to_beat: 128. }), "{:?}", m.items);
    }

    #[test]
    fn the_set_then_the_sheet_then_the_analysis() {
        // the sheet's cue `outro` replaces the measured section's name
        let e = env(Some("cue outro @ 200s\n"), None);
        let (c, w) = ok(&e, &format!("{TRACKS}mix aft -> smk at aft.outro over 8 bars\n"));
        assert_eq!(c.set.mixes[0].start_beat, ((200. - 0.5) * 112. / 60. * 1000. as f64).round() / 1000.);
        assert!(w.is_empty(), "{w:?}");
        // and the set's own cue wins over both
        let src = "track aft \"Pneumatix - Aftertekno.flac\"\n  cue outro bar 100\ntrack smk \"Smokin\"\nmix aft -> smk at aft.outro over 8 bars\n";
        assert_eq!(ok(&e, src).0.set.mixes[0].start_beat, 99. * 4.);
        // a mark is a name too, and a sheet loop is a loop
        let e = env(Some("mark c @ 30.5s\nloop roll @ 100s len 2 bars\n"), None);
        let (c, _) = ok(&e, &format!("{TRACKS}mix aft -> smk at aft.c over 8 bars\n  bar 2 aft loop roll\n"));
        let m = &c.set.mixes[0];
        assert_eq!(m.start_beat, 56.);
        let start = ((100. - 0.5) * 112. / 60. * 1000. as f64).round() / 1000.;
        assert!(m.items.iter().any(|i| i.action == Action::Loop { deck: Deck::Out, start_beat: start, beats: 8. }), "{:?}", m.items);
        // an unknown name lists what there is
        let l = load_with(&env(None, None), &format!("{TRACKS}mix aft -> smk at aft.nope over 8 bars\n"));
        assert!(l.diags[0].msg.contains("unknown cue \"nope\" on track aft (cues: intro, drop, outro)"), "{:?}", l.diags);
    }

    #[test]
    fn the_sheet_grid_and_gain_count_and_its_bars_keep_their_time() {
        let e = env(Some("grid 120 @ 1s beat1 1\ngain -4dB\ncue hook bar 9\n"), None);
        let (c, _) = ok(&e, &format!("{TRACKS}mix aft -> smk at aft.hook over 8 bars\n"));
        let aft = c.set.track("aft").unwrap();
        let g = aft.grid.as_ref().unwrap();
        assert_eq!((g.bpm, g.first_beat_s), (120., 1.));
        assert_eq!(aft.gain_db, Some(-4.));
        assert_eq!(c.set.mixes[0].start_beat, 32.);
        // the set's own grid line wins, and the sheet's bar 9 stays where it is in time (17 s),
        // it is not bar 9 of the set's grid
        let src = "track aft \"Pneumatix - Aftertekno.flac\"\n  grid 112 @ 0.5s\n  gain -1dB\ntrack smk \"Smokin\"\nmix aft -> smk at aft.hook over 8 bars\n";
        let (c, _) = ok(&e, src);
        assert_eq!(c.set.mixes[0].start_beat, 30.8);
        assert_eq!(c.set.track("aft").unwrap().gain_db, Some(-1.));
        // without a sheet the analysis gain stays the deck's auto-gain
        assert_eq!(ok(&env(None, None), &format!("{TRACKS}mix aft -> smk at aft.drop over 8 bars\n")).0.set.track("aft").unwrap().gain_db, None);
    }

    #[test]
    fn warnings_for_drops_and_vocals() {
        // starting in the middle of the drop
        let (_, w) = ok(&env(None, None), &format!("{TRACKS}mix aft -> smk at bar 41 over 8 bars\n"));
        assert_eq!(w, ["3:23: the mix starts inside aft's drop (analysis, bars 33..64 of the track): the drop is cut off"]);
        // at its start is fine
        assert!(ok(&env(None, None), &format!("{TRACKS}mix aft -> smk at aft.drop over 8 bars\n")).1.is_empty());
        // both tracks sing at once: aft from 75 s (bar ~2.75 of the mix), smk over its first 15 bars
        let e = env(Some("vocals @ 75s..90s\n"), Some("vocals @ 0s..30s\n"));
        let (_, w) = ok(&e, &format!("{TRACKS}mix aft -> smk at bar 33 over 16 bars\n"));
        assert_eq!(w, ["3:12: the vocals of aft and smk overlap in this mix (bars 2.75..10 of the mix)"]);
        // one of them without vocals: nothing to say
        let e = env(Some("vocals @ 75s..90s\n"), Some("vocals none\n"));
        assert!(ok(&e, &format!("{TRACKS}mix aft -> smk at aft.drop over 16 bars\n")).1.is_empty());
        // unknown vocals are not "none" but say nothing either
        let e = env(Some("vocals @ 75s..90s\n"), None);
        assert!(ok(&e, &format!("{TRACKS}mix aft -> smk at aft.drop over 16 bars\n")).1.is_empty());
    }
}

#[test]
fn loop_roll_halves_one_loop_down_to_half_a_beat_and_lands_on_the_downbeat() {
    use Deck::*;
    let p = plan_of(&recipe_mix(HEAD, "loop-roll", ""));
    assert_eq!(p.length_beats, 32.);
    // the loops: 4, 2, 1, 1/2 beats, two bars, one bar, half a bar and a beat before the end,
    // all from the start of the first one (a resize keeps the start)
    let loops: Vec<(f64, f64, f64)> = p
        .items
        .iter()
        .filter_map(|i| match i {
            PlanItem::Loop { deck: Out, start_beat, beats, at_beat_rel } => Some((*at_beat_rel, *beats, *start_beat)),
            _ => None,
        })
        .collect();
    assert_eq!(loops.iter().map(|l| (l.0, l.1)).collect::<Vec<_>>(), [(16., 4.), (24., 2.), (28., 1.), (30., 0.5)]);
    assert!(loops.iter().all(|l| l.2 == loops[0].2), "{loops:?}");
    // the loop ends half a beat before the stop, the clock moves to the incoming deck at the
    // first loop, the filter rises from the middle
    assert!(p.items.contains(&PlanItem::LoopOff { deck: Out, at_beat_rel: 31.5 }));
    assert!(p.items.contains(&PlanItem::Pause { deck: Out, at_beat_rel: 32. }));
    assert_eq!(p.in_clock.as_ref().map(|c| c.after), Some(16.));
    assert!(p.items.contains(&ramp(Out, Control::Filter, Some(0.), 0.7, 16., 31.5, Curve::Exp)));
    // the new track runs silently and jumps back to its first downbeat at full volume
    assert!(p.items.contains(&PlanItem::Set { deck: In, control: Control::Fader, value: 0., at_beat_rel: 0. }));
    assert!(p.items.contains(&PlanItem::Jump { deck: In, to_beat: SMK0, at_beat_rel: 32. }));
    assert!(p.items.contains(&PlanItem::Set { deck: In, control: Control::Fader, value: 1., at_beat_rel: 32. }));
    // no fade: the roll keeps its volume
    assert!(!p.items.iter().any(|i| matches!(i, PlanItem::Ramp { deck: Out, control: Control::Fader, .. })));
}

#[test]
fn brake_slows_the_old_deck_to_a_standstill_timed_on_the_new_one() {
    use Deck::*;
    let p = plan_of(&recipe_mix(HEAD, "brake", ""));
    assert_eq!(p.length_beats, 8.);
    // the last two beats: the rate from wherever it is down to 0, linear in time
    assert!(p.items.contains(&ramp(Out, Control::Rate, None, 0., 6., 8., Curve::Linear)));
    // the outgoing clock can't time a ramp that slows it: from the ramp on, the incoming deck does
    assert_eq!(p.in_clock.as_ref().map(|c| c.after), Some(6.));
    let at8: Vec<_> = p.items.iter().filter(|i| i.start_beat_rel() == 8.).collect();
    assert_eq!(at8, [
        &PlanItem::Pause { deck: Out, at_beat_rel: 8. },
        &PlanItem::Set { deck: Out, control: Control::Rate, value: 1., at_beat_rel: 8. },
        &PlanItem::Jump { deck: In, to_beat: SMK0, at_beat_rel: 8. },
        &PlanItem::Set { deck: In, control: Control::Fader, value: 1., at_beat_rel: 8. },
    ]);
    // mapped to the decks: the brake and all after it run on the incoming deck's beats
    let items = super::runtime::map_plan(&p, 0, 1, 7);
    let brake = items.iter().find(|i| matches!(i.kind, crate::audio::AutoKind::Ramp { control: crate::audio::Control::Rate, .. })).unwrap();
    assert_eq!(brake.clock, 1);
    assert!(matches!(brake.kind, crate::audio::AutoKind::Ramp { deck: 0, from: None, to, .. } if to == 0.));
    assert!(items.iter().all(|i| i.is_sane(crate::audio::DECKS)));
    // a brake lands at its end: that is what should sit on a phrase line
    let warn = |at: &str| {
        let l = run(&format!("{HEAD}mix aft -> smk at {at} recipe brake\n"));
        assert!(l.diags.is_empty(), "{:?}", l.diags);
        l.warnings.iter().map(|d| d.msg.clone()).collect::<Vec<_>>()
    };
    assert!(warn("bar 127").is_empty());
    assert_eq!(warn("bar 129"), ["recipe brake ends at bar 131: mixes usually end on a phrase (bar 129 or 137)"]);
}

#[test]
fn eq_swap_hands_over_highs_then_mids_then_the_bass() {
    use Control::*;
    use Deck::*;
    let p = plan_of(&recipe_mix(HEAD, "eq-swap", ""));
    assert_eq!(p.length_beats, 96.);
    assert!(p.items.contains(&PlanItem::Set { deck: In, control: Low, value: -1., at_beat_rel: 0. }));
    assert!(p.items.contains(&PlanItem::Set { deck: In, control: Mid, value: -1., at_beat_rel: 0. }));
    assert!(p.items.contains(&ramp(In, High, Some(-1.), 0., 0., 32., Curve::Smooth)));
    assert!(p.items.contains(&ramp(Out, High, Some(0.), -1., 0., 32., Curve::Smooth)));
    assert!(p.items.contains(&ramp(In, Mid, Some(-1.), 0., 32., 64., Curve::Smooth)));
    assert!(p.items.contains(&ramp(Out, Mid, Some(0.), -1., 32., 64., Curve::Smooth)));
    assert!(p.items.contains(&PlanItem::Swap { band: Low, at_beat_rel: 96. }));
    assert!(p.items.contains(&PlanItem::Pause { deck: Out, at_beat_rel: 96. }));
    // faders full: nothing moves them
    assert!(!p.items.iter().any(|i| matches!(i, PlanItem::Ramp { control: Fader, .. } | PlanItem::Set { control: Fader, .. })));
    assert!(p.in_clock.is_none());
}

#[test]
fn rate_is_a_deck_control_and_a_patch_knob_name() {
    // in range, with or without a start value; a set; out of range and on the crossfader line
    let body = "  bars 0..1 aft.rate _ -> 0.5 exp\n  bar 2 smk.rate = 1.02\n";
    let items = plan_of(&mix(body)).items;
    assert!(items.contains(&ramp(Deck::Out, Control::Rate, None, 0.5, 0., 4., Curve::Exp)));
    assert!(items.contains(&PlanItem::Set { deck: Deck::In, control: Control::Rate, value: 1.02, at_beat_rel: 8. }));
    // linear unless a curve is named
    let items = plan_of(&mix("  bars 0..1 aft.rate 1 -> 0\n")).items;
    assert!(items.contains(&ramp(Deck::Out, Control::Rate, Some(1.), 0., 0., 4., Curve::Linear)));
    let e = errors(&mix("  bar 1 aft.rate = 5\n"));
    assert_eq!(e.len(), 1, "{e:?}");
    assert!(e[0].contains("rate"), "{e:?}");
    let e = errors(&mix("  bar 1 aft.pitch = 1\n"));
    assert!(e[0].contains("(fader, low, mid, high, filter, gain, rate)"), "{e:?}");
    // the incoming deck's rate does not move the clock
    assert!(plan_of(&mix("  bar 1 smk.rate = 1\n")).in_clock.is_none());
    assert_eq!(plan_of(&mix("  bar 1 aft.rate = 1\n")).in_clock.map(|c| c.after), Some(4.));
}
