//! Captures and recordings as edits of the active set's file. The engine gathers what the decks
//! show ([`DeckSnap`]) and these functions turn it into an [`EditJob`]: a change of the file's
//! text that [`super::write::Editor`] applies, checks and versions. No engine here, so it is
//! tested without audio.

use std::path::Path;

use crate::analysis::Grid;

use super::{
    edit::{self, Slot},
    model::Track,
    record::{self, Recorder},
    runtime::same_file,
    write::EditJob,
    Compiled,
};

/// What a deck shows
#[derive(Debug, Clone)]
pub struct DeckSnap {
    pub path: std::path::PathBuf,
    pub title: String,
    /// the deck's effective grid
    pub grid: Grid,
    /// seconds into the track
    pub position: f64,
    /// the active loop (seconds), if any
    pub looping: Option<(f64, f64)>,
}

#[derive(Debug, Clone, Copy)]
pub enum Capture<'a> {
    Cue(&'a str),
    Loop(&'a str),
    Grid,
}

/// The track of the set whose file is `path`
fn find<'a>(set: &'a Compiled, path: &Path) -> Option<&'a Track> {
    set.set.tracks.iter().find(|t| same_file(&t.file, path))
}

/// How a `track` line names a file: relative to the library root when it is inside, else absolute
fn track_file(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).to_string_lossy().into_owned()
}

/// Puts `line` into the block of track `id` of `text`; if the file has no such block (it is new,
/// or the file changed), a new block is appended with an id from the title
fn put_line(text: &str, id: Option<&str>, title: &str, file: &str, slot: Slot, line: &str) -> Result<(String, String), String> {
    let ids = edit::track_ids(text);
    match id.filter(|id| ids.iter().any(|i| i == id)) {
        Some(id) => Ok((edit::upsert_track_line(text, id, slot, line)?, id.to_string())),
        None => {
            let id = edit::unique_id(title, &ids);
            Ok((edit::append_track(text, &id, file, &[line.to_string()]), id))
        }
    }
}

/// `SetCue`, `SetLoopCapture`, `SetGridCapture`: the line to write for what `deck` shows. The
/// track's grid in the set (a `grid` line may differ from the deck's) is what the line is
/// counted on, since that is the grid that reads it back.
pub fn capture(file: &Path, set: &Compiled, root: &Path, deck: &DeckSnap, quantize: bool, what: Capture) -> Result<EditJob, String> {
    let track = find(set, &deck.path);
    let grid = track.and_then(|t| t.grid.as_ref()).unwrap_or(&deck.grid);
    let (kind, line, label) = match what {
        Capture::Cue(name) => (Owned::Cue(name.to_string()), edit::cue_line(grid, deck.position, quantize, name)?, format!("cue {name}")),
        Capture::Loop(name) => {
            let (start, end) = deck.looping.ok_or("no active loop on this deck")?;
            (Owned::Loop(name.to_string()), edit::loop_line(grid, start, end, quantize, name)?, format!("loop {name}"))
        }
        // the grid the deck has, not the set's: that is what is being saved
        Capture::Grid => (Owned::Grid, edit::grid_line(&deck.grid), "grid".to_string()),
    };
    let id = track.map(|t| t.id.clone());
    let file_name = track_file(root, &deck.path);
    let title = deck.title.clone();
    let planned = id.clone().unwrap_or_else(|| edit::unique_id(&title, &set.set.tracks.iter().map(|t| t.id.clone()).collect::<Vec<_>>()));
    Ok(EditJob {
        file: file.to_path_buf(),
        what: format!("{label} on {planned}"),
        change: Box::new(move |text| {
            let slot = match &kind {
                Owned::Cue(n) => Slot::Cue(n),
                Owned::Loop(n) => Slot::Loop(n),
                Owned::Grid => Slot::Grid,
            };
            let (new, id) = put_line(text, id.as_deref(), &title, &file_name, slot, &line)?;
            Ok((new, format!("{line} on {id}")))
        }),
    })
}

enum Owned {
    Cue(String),
    Loop(String),
    Grid,
}

/// What the engine knows about a deck at the end of a recording
#[derive(Debug, Clone)]
pub struct RecDeck {
    pub path: std::path::PathBuf,
    pub title: String,
    pub grid: Grid,
}

/// `SetRecord(false)`: the mix block of a recording, plus the `track` blocks of tracks the set
/// does not have yet. The reply tail is `mix N: a -> b, X bars, Y items` (N counts from 0, like
/// `SetArm`).
pub fn finish_recording(file: &Path, set: &Compiled, root: &Path, rec: &Recorder, decks: &[RecDeck; 2]) -> Result<EditJob, String> {
    if let Some(why) = &rec.broken {
        return Err(format!("the recording is unusable: {why}"));
    }
    let taken: Vec<String> = set.set.tracks.iter().map(|t| t.id.clone()).collect();
    let mut known = taken.clone();
    // each deck's track id in the set; new ones get an id and a block
    let mut ids: Vec<String> = vec![];
    let mut added: Vec<(String, String)> = vec![]; // (id, file)
    let mut tracks: Vec<Option<&Track>> = vec![];
    for d in decks {
        let t = find(set, &d.path);
        let id = match t {
            Some(t) => t.id.clone(),
            None => {
                let id = edit::unique_id(&d.title, &known);
                known.push(id.clone());
                added.push((id.clone(), track_file(root, &d.path)));
                id
            }
        };
        ids.push(id);
        tracks.push(t);
    }
    if ids[0] == ids[1] {
        return Err("both decks have the same track".into());
    }
    // the grids as the set will read them
    let grids = [
        tracks[0].and_then(|t| t.grid.as_ref()).unwrap_or(&decks[0].grid),
        tracks[1].and_then(|t| t.grid.as_ref()).unwrap_or(&decks[1].grid),
    ];
    let cues: Vec<Vec<(String, f64)>> = tracks
        .iter()
        .map(|t| t.map(|t| t.cues.iter().filter_map(|c| Some((c.name.clone(), t.beat_of(&c.pos)?))).collect()).unwrap_or_default())
        .collect();
    let draft = record::build(&record::Input {
        out: rec.out,
        grids,
        ids: [&ids[0], &ids[1]],
        started_at: rec.started_at,
        initial: &rec.initial,
        in_at_start: rec.in_at_start,
        events: &rec.events,
        cues: [&cues[0], &cues[1]],
    })?;
    let block = draft.block();
    let (from, to) = (draft.from.clone(), draft.to.clone());
    let summary = format!("{}, {} bars, {} items", format!("{from} -> {to}"), draft.length_bars, draft.items);
    let check_ids = (taken, added.clone());
    Ok(EditJob {
        file: file.to_path_buf(),
        what: format!("recorded mix {from} -> {to}"),
        change: Box::new(move |text| {
            // the file may have changed since the recording started
            let now = edit::track_ids(text);
            let (taken, added) = check_ids;
            if let Some(gone) = taken.iter().find(|i| !now.contains(i) && (from == **i || to == **i)) {
                return Err(format!("track \"{gone}\" is gone from the set: it changed while recording, record again"));
            }
            if let Some((id, _)) = added.iter().find(|(id, _)| now.contains(id)) {
                return Err(format!("track \"{id}\" appeared in the set while recording, record again"));
            }
            let mut text = text.to_string();
            for (id, file) in &added {
                text = edit::append_track(&text, id, file, &[]);
            }
            let text = edit::replace_or_append_mix(&text, &from, &to, &block);
            let n = edit::mix_index(&text, &from, &to).unwrap_or(0);
            Ok((text, format!("mix {n}: {summary}")))
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sets::{
        load,
        model::Control,
        record::{Ev, RecEvent},
        store::Store,
        testenv::{grid, spec_env},
        write::Editor,
    };
    use std::{path::PathBuf, sync::Arc};

    const START: &str = "# friday\nset \"friday\"\n\ntrack aft \"Pneumatix - Aftertekno.flac\"   # opener\n  cue drop bar 33\n\n";
    const AFT: &str = "/music/Pneumatix - Aftertekno.flac";
    const SMK: &str = "/music/Aasi & Swooh - Smokin.flac";

    fn rig(tag: &str, text: &str) -> (Arc<Editor>, PathBuf, Compiled) {
        let dir = std::env::temp_dir().join(format!("codjd-capture-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ed = Editor::new(Store::new(dir.clone(), Arc::new(spec_env())));
        ed.write("friday", text, "", "human").unwrap();
        let file = dir.join("friday.codj");
        let c = ed.store().get("friday").unwrap();
        (ed, file, (*c).clone())
    }

    fn snap(path: &str, title: &str, g: Grid, position: f64) -> DeckSnap {
        DeckSnap { path: path.into(), title: title.into(), grid: g, position, looping: None }
    }

    fn read(f: &Path) -> String {
        std::fs::read_to_string(f).unwrap()
    }

    #[test]
    fn a_cue_lands_in_the_decks_track_and_replaces_by_name() {
        let (ed, file, set) = rig("cue", START);
        // aft: 112 BPM from 0.5 s; beat 64 = bar 17
        let at = 0.5 + 64. * 60. / 112. + 0.01;
        let d = snap(AFT, "Aftertekno", grid(112., 0.5, 0), at);
        let job = capture(&file, &set, Path::new("/music"), &d, true, Capture::Cue("hook")).unwrap();
        assert_eq!(job.what, "cue hook on aft");
        assert_eq!(ed.modify(job, "human").unwrap(), "cue hook bar 17 on aft");
        assert_eq!(read(&file), START.replace("cue drop bar 33\n", "cue drop bar 33\n  cue hook bar 17\n"));
        assert_eq!(ed.store().get("friday").unwrap().set.tracks[0].cues.len(), 2);

        // the same name again: replaced (and the quantize off keeps the offset)
        let set = (*ed.store().get("friday").unwrap()).clone();
        let job = capture(&file, &set, Path::new("/music"), &d, false, Capture::Cue("hook")).unwrap();
        ed.modify(job, "human").unwrap();
        assert!(read(&file).contains("  cue hook bar 17 beat 1.019\n"), "{}", read(&file));
        assert_eq!(ed.store().get("friday").unwrap().set.tracks[0].cues.len(), 2);

        // bad names and positions before the first bar are refused before anything is written
        assert!(capture(&file, &set, Path::new("/music"), &d, true, Capture::Cue("1x")).is_err());
        let early = snap(AFT, "Aftertekno", grid(112., 5., 0), 0.);
        assert!(capture(&file, &set, Path::new("/music"), &early, true, Capture::Cue("x")).is_err());
    }

    #[test]
    fn loops_and_grids_are_captured_and_a_new_track_gets_a_block() {
        let (ed, file, set) = rig("loop", START);
        let mut d = snap(AFT, "Aftertekno", grid(112., 0.5, 0), 40.);
        assert_eq!(capture(&file, &set, Path::new("/music"), &d, true, Capture::Loop("lp")).unwrap_err(), "no active loop on this deck");
        // 4 beats from beat 32
        let s = 0.5 + 32. * 60. / 112.;
        d.looping = Some((s, s + 4. * 60. / 112.));
        let job = capture(&file, &set, Path::new("/music"), &d, true, Capture::Loop("lp")).unwrap();
        assert_eq!(ed.modify(job, "human").unwrap(), "loop lp bar 9 len 1 bar on aft");

        let job = capture(&file, &set, Path::new("/music"), &d, true, Capture::Grid).unwrap();
        assert_eq!(ed.modify(job, "human").unwrap(), "grid 112 @ 0.5s beat1 1 on aft");
        let text = read(&file);
        assert!(text.contains("  loop lp bar 9 len 1 bar\n  grid 112 @ 0.5s beat1 1\n\n"), "{text}");
        // a second grid capture replaces the line
        let set = (*ed.store().get("friday").unwrap()).clone();
        let mut d2 = d.clone();
        d2.grid = grid(113., 0.25, 2);
        ed.modify(capture(&file, &set, Path::new("/music"), &d2, true, Capture::Grid).unwrap(), "human").unwrap();
        assert_eq!(read(&file).matches("grid ").count(), 1);
        assert!(read(&file).contains("grid 113 @ 0.25s beat1 3"));

        // smk is not in the set: a block with its relative path, an id from the title
        let set = (*ed.store().get("friday").unwrap()).clone();
        let s = snap(SMK, "Smokin", grid(126., 0.1, 2), 0.1 + 2. * 60. / 126.);
        let job = capture(&file, &set, Path::new("/music"), &s, true, Capture::Cue("start")).unwrap();
        assert_eq!(job.what, "cue start on smokin");
        assert_eq!(ed.modify(job, "agent:x").unwrap(), "cue start bar 1 on smokin");
        assert!(read(&file).ends_with("\ntrack smokin \"Aasi & Swooh - Smokin.flac\"\n  cue start bar 1\n"), "{}", read(&file));
        assert_eq!(ed.store().get("friday").unwrap().set.tracks.len(), 2);
        // and the next capture finds it
        let set = (*ed.store().get("friday").unwrap()).clone();
        let job = capture(&file, &set, Path::new("/music"), &s, true, Capture::Cue("again")).unwrap();
        assert_eq!(job.what, "cue again on smokin");
    }

    fn events_for_blend() -> Vec<RecEvent> {
        // aft (deck 0, 112 BPM, first beat 0.5 s) on air; smk (deck 1) comes in at aft's bar 5
        let at = |b: f64| 0.5 + b * 60. / 112.;
        let knob = |b: f64, deck, control, value| RecEvent { at: at(b), ev: Ev::Knob { deck, control, value } };
        vec![
            RecEvent { at: at(16.), ev: Ev::Play { deck: 1, pos: 0.1 + 2. * 60. / 126., synced: true } },
            knob(16., 1, Control::Fader, 0.5),
            knob(32., 0, Control::Low, -1.),
            knob(32.2, 1, Control::Low, 0.),
            RecEvent { at: at(40.), ev: Ev::Pause { deck: 0 } },
        ]
    }

    #[test]
    fn a_recording_adds_the_missing_tracks_and_the_mix_and_replaces_it_next_time() {
        let (ed, file, set) = rig("rec", START);
        let rec = Recorder {
            out: 0,
            started_at: 0.5 + 12. * 60. / 112.,
            initial: vec![(1, Control::Fader, 0.5), (1, Control::Low, -1.)],
            events: events_for_blend(),
            ..Default::default()
        };
        let decks = [
            RecDeck { path: AFT.into(), title: "Aftertekno".into(), grid: grid(112., 0.5, 0) },
            RecDeck { path: SMK.into(), title: "Smokin".into(), grid: grid(126., 0.1, 2) },
        ];
        let job = finish_recording(&file, &set, Path::new("/music"), &rec, &decks).unwrap();
        assert_eq!(job.what, "recorded mix aft -> smokin");
        let reply = ed.modify(job, "human").unwrap();
        assert_eq!(reply, "mix 0: aft -> smokin, 6 bars, 4 items");
        let text = read(&file);
        assert!(text.starts_with(START), "{text}");
        assert_eq!(
            &text[START.len()..],
            "track smokin \"Aasi & Swooh - Smokin.flac\"\n\n\
             mix aft -> smokin at bar 5 over 6 bars\n  smokin play from bar 1\n  \
             bar 0  smokin.fader = 0.5; smokin.low kill\n  bar 4  swap low\n  bar 6  aft stop\n"
        );
        let c = ed.store().get("friday").unwrap();
        assert_eq!(c.set.mixes.len(), 1);
        assert_eq!(c.set.mixes[0].items.len(), 4);

        // again: the mix is replaced in place, no second track block
        let set = (*c).clone();
        let job = finish_recording(&file, &set, Path::new("/music"), &rec, &decks).unwrap();
        assert_eq!(ed.modify(job, "human").unwrap(), "mix 0: aft -> smokin, 6 bars, 4 items");
        assert_eq!(read(&file), text);

        // a recording that can't be used
        let mut bad = Recorder { broken: Some("deck A was reloaded".into()), ..Default::default() };
        assert!(finish_recording(&file, &set, Path::new("/music"), &bad, &decks).unwrap_err().contains("reloaded"));
        bad.broken = None;
        assert!(finish_recording(&file, &set, Path::new("/music"), &bad, &decks).unwrap_err().contains("nothing recorded"));
        // a track that appeared meanwhile is a conflict, not a duplicate id
        let (ed2, file2, set2) = rig("rec2", START);
        let job = finish_recording(&file2, &set2, Path::new("/music"), &rec, &decks).unwrap();
        let sneaky = format!("{START}track smokin \"Smokin\"\n");
        let h = crate::sets::write::hash(START.as_bytes());
        ed2.write("friday", &sneaky, &h, "human").unwrap();
        assert!(ed2.modify(job, "human").unwrap_err().contains("appeared"));
        let _ = load; // the whole pipeline is what modify ran
    }
}
