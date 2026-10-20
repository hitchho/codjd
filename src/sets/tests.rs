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
    // the jump goes 8 beats back from 18, and smk keeps counting from there: 4 beats later, 14
    assert!(items.contains(&PlanItem::Jump { deck: In, to_beat: 10., at_beat_rel: 8. }));
    assert!(items.contains(&PlanItem::Loop { deck: In, start_beat: 14., beats: 1., at_beat_rel: 12. }));
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
