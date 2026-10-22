//! Recording the master to a file: what the room hears, never the headphones.
//!
//! The audio callback pushes each master frame into a lock-free ring while recording is on (a
//! full ring drops the frame and counts it, it never waits). A writer thread drains the ring into
//! a 16-bit stereo WAV file and rewrites the header's lengths every few seconds, so a file cut
//! short by a crash or a full disk still plays up to that point.

use std::{
    fs::File,
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

use tracing::{info, warn};

/// Frames the ring holds: about 2.7 s at 48 kHz, plenty for a writer that wakes every 20 ms
const RING_FRAMES: usize = 1 << 17;
/// How often the WAV header is brought up to date while recording
const HEADER_EVERY: Duration = Duration::from_secs(5);

/// The audio callback's side
pub struct Tap {
    frames: rtrb::Producer<(f32, f32)>,
    on: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
}

impl Tap {
    /// One master frame; nothing happens unless recording is on
    #[inline]
    pub fn push(&mut self, frame: (f32, f32)) {
        if self.on.load(Ordering::Relaxed) && self.frames.push(frame).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// What a recording is doing, for `RecordAudioState`
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecState {
    pub on: bool,
    /// the file being written, or the last one written
    pub path: String,
    pub seconds: f64,
    /// frames lost because the writer fell behind (or the disk was slow)
    pub dropped: u64,
    /// why the last recording stopped on its own, if it did
    pub error: String,
}

enum Msg {
    Start(PathBuf, mpsc::Sender<Result<(), String>>),
    Stop(mpsc::Sender<RecState>),
}

/// The control side: start and stop, and the state
pub struct Recorder {
    on: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    state: Arc<Mutex<RecState>>,
    msgs: mpsc::Sender<Msg>,
}

/// A recorder and the tap for the callback of an output running at `rate`
pub fn new(rate: u32) -> (Recorder, Tap) {
    let (frames, from_tap) = rtrb::RingBuffer::new(RING_FRAMES);
    let on = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicU64::new(0));
    let state = Arc::new(Mutex::new(RecState::default()));
    let (msgs, inbox) = mpsc::channel();
    {
        let (on, dropped, state) = (Arc::clone(&on), Arc::clone(&dropped), Arc::clone(&state));
        std::thread::Builder::new()
            .name("audio-record".into())
            .spawn(move || writer(rate, from_tap, inbox, &on, &dropped, &state))
            .expect("can't start the recording thread");
    }
    let tap = Tap { frames, on: Arc::clone(&on), dropped: Arc::clone(&dropped) };
    (Recorder { on, dropped, state, msgs }, tap)
}

impl Recorder {
    /// Starts writing the master to `path` (its folder is created); an error if already recording
    pub fn start(&self, path: &Path) -> Result<(), String> {
        if self.on.load(Ordering::SeqCst) {
            return Err(format!("already recording to {}", self.state().path));
        }
        let (tx, rx) = mpsc::channel();
        self.msgs.send(Msg::Start(path.to_path_buf(), tx)).map_err(|_| "the recording thread is gone".to_string())?;
        rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "the recording thread does not answer".to_string())?
    }

    /// Stops and closes the file; the final state (path, length). Nothing recording: the state.
    pub fn stop(&self) -> RecState {
        if !self.on.load(Ordering::SeqCst) {
            return self.state();
        }
        let (tx, rx) = mpsc::channel();
        if self.msgs.send(Msg::Stop(tx)).is_err() {
            return self.state();
        }
        rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| self.state())
    }

    pub fn state(&self) -> RecState {
        let mut s = self.state.lock().unwrap().clone();
        s.dropped = self.dropped.load(Ordering::Relaxed);
        s
    }
}

/// An open WAV file and how much of it is written
struct Wav {
    out: BufWriter<File>,
    frames: u64,
    header_at: Instant,
}

impl Wav {
    fn create(path: &Path, rate: u32) -> std::io::Result<Wav> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut out = BufWriter::new(File::create_new(path)?);
        out.write_all(&header(rate, 0))?;
        Ok(Wav { out, frames: 0, header_at: Instant::now() })
    }

    fn write(&mut self, frames: &[(f32, f32)]) -> std::io::Result<()> {
        let mut bytes = Vec::with_capacity(frames.len() * 4);
        for &(l, r) in frames {
            bytes.extend_from_slice(&pcm16(l).to_le_bytes());
            bytes.extend_from_slice(&pcm16(r).to_le_bytes());
        }
        self.out.write_all(&bytes)?;
        self.frames += frames.len() as u64;
        Ok(())
    }

    /// Writes the lengths into the header and goes back to the end
    fn update_header(&mut self, rate: u32) -> std::io::Result<()> {
        self.out.flush()?;
        let file = self.out.get_mut();
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header(rate, self.frames))?;
        file.seek(SeekFrom::End(0))?;
        self.header_at = Instant::now();
        Ok(())
    }

    fn finish(mut self, rate: u32) -> std::io::Result<()> {
        self.update_header(rate)?;
        self.out.get_mut().sync_all()
    }
}

fn pcm16(v: f32) -> i16 {
    (v.clamp(-1., 1.) * i16::MAX as f32).round() as i16
}

/// The 44-byte header of a 16-bit stereo PCM WAV holding `frames` frames (sizes saturate at the
/// 4 GB the format can say; the samples go on past that, which players still read)
fn header(rate: u32, frames: u64) -> [u8; 44] {
    let data = (frames * 4).min(u32::MAX as u64 - 36) as u32;
    let mut h = [0u8; 44];
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    h[22..24].copy_from_slice(&2u16.to_le_bytes()); // stereo
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * 4).to_le_bytes());
    h[32..34].copy_from_slice(&4u16.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

fn writer(
    rate: u32,
    mut from_tap: rtrb::Consumer<(f32, f32)>,
    inbox: mpsc::Receiver<Msg>,
    on: &AtomicBool,
    dropped: &AtomicU64,
    state: &Mutex<RecState>,
) {
    let mut wav: Option<Wav> = None;
    let mut buf = Vec::with_capacity(RING_FRAMES);
    let drain = |from_tap: &mut rtrb::Consumer<(f32, f32)>, buf: &mut Vec<(f32, f32)>| {
        buf.clear();
        while let Ok(f) = from_tap.pop() {
            buf.push(f);
        }
    };
    loop {
        let msg = match inbox.recv_timeout(Duration::from_millis(20)) {
            Ok(m) => Some(m),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // codjd is going away: keep what was recorded
                on.store(false, Ordering::SeqCst);
                drain(&mut from_tap, &mut buf);
                if let Some(mut w) = wav.take() {
                    let _ = w.write(&buf);
                    let _ = w.finish(rate);
                }
                return;
            }
        };
        match msg {
            Some(Msg::Start(path, reply)) => {
                // what the ring held from before is not part of this recording
                drain(&mut from_tap, &mut buf);
                match Wav::create(&path, rate).map_err(|e| format!("can't write {}: {e}", path.display())) {
                    Ok(w) => {
                        wav = Some(w);
                        dropped.store(0, Ordering::SeqCst);
                        *state.lock().unwrap() =
                            RecState { on: true, path: path.display().to_string(), ..RecState::default() };
                        on.store(true, Ordering::SeqCst);
                        info!(path = %path.display(), "recording the master");
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Some(Msg::Stop(reply)) => {
                on.store(false, Ordering::SeqCst);
                drain(&mut from_tap, &mut buf);
                let mut s = state.lock().unwrap().clone();
                if let Some(mut w) = wav.take() {
                    let _ = w.write(&buf);
                    s.seconds = w.frames as f64 / rate as f64;
                    if let Err(e) = w.finish(rate) {
                        s.error = format!("closing the file: {e}");
                    }
                    info!(path = %s.path, seconds = s.seconds, "recording stopped");
                }
                s.on = false;
                s.dropped = dropped.load(Ordering::SeqCst);
                *state.lock().unwrap() = s.clone();
                let _ = reply.send(s);
            }
            None => {}
        }
        if let Some(w) = wav.as_mut() {
            drain(&mut from_tap, &mut buf);
            let result = w.write(&buf).and_then(|_| if w.header_at.elapsed() >= HEADER_EVERY { w.update_header(rate) } else { Ok(()) });
            let seconds = w.frames as f64 / rate as f64;
            match result {
                Ok(()) => state.lock().unwrap().seconds = seconds,
                Err(e) => {
                    // a full disk or a removed drive: stop, keep what is there
                    on.store(false, Ordering::SeqCst);
                    warn!(error = %e, "recording stopped: can't write");
                    let w = wav.take().unwrap();
                    let _ = w.finish(rate);
                    let mut s = state.lock().unwrap();
                    s.on = false;
                    s.seconds = seconds;
                    s.error = format!("stopped: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_what_is_pushed_into_a_playable_wav() {
        let dir = std::env::temp_dir().join(format!("codjd-rec-{}", std::process::id()));
        let path = dir.join("sub/mix.wav");
        let (rec, mut tap) = new(48_000);
        // nothing is kept while not recording
        tap.push((1., 1.));
        rec.start(&path).unwrap();
        assert!(rec.start(&path).unwrap_err().contains("already recording"));
        for i in 0..48_000 {
            tap.push(((i as f32 / 48_000.) - 0.5, 0.25));
        }
        std::thread::sleep(Duration::from_millis(100));
        let st = rec.stop();
        assert!(!st.on);
        assert_eq!(st.path, path.display().to_string());
        assert!((st.seconds - 1.0).abs() < 1e-9, "{}", st.seconds);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 44 + 48_000 * 4);
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 48_000 * 4);
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 48_000);
        // the right channel is 0.25 throughout
        assert_eq!(i16::from_le_bytes([bytes[46], bytes[47]]), pcm16(0.25));
        // after the stop, pushes are not recorded, and the same file is never overwritten
        tap.push((1., 1.));
        assert!(rec.start(&path).unwrap_err().contains("can't write"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn samples_clamp_instead_of_wrapping() {
        assert_eq!(pcm16(2.0), i16::MAX);
        assert_eq!(pcm16(-2.0), -i16::MAX);
        assert_eq!(pcm16(0.0), 0);
    }
}
