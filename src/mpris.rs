//! MPRIS2 (`org.mpris.MediaPlayer2.amc`): desktop media controls — GNOME/KDE
//! media bar, lock screen, `playerctl`, media keys.
//!
//! Shape: the UI thread publishes a small [`Snapshot`] of playback state into
//! a shared `parking_lot::Mutex` once per frame (a lock + struct move —
//! nothing else on the hot path). A dedicated thread owns the D-Bus
//! connection (zbus blocking API): it serves the root + player interfaces,
//! diffs each poll and emits `PropertiesChanged`/`Seeked` only on change,
//! and forwards remote commands over a `std::sync::mpsc` channel that
//! `App::update` drains into the same code paths as clicks/shortcuts.
//!
//! No session bus (bare Xvfb, VPS)? The thread logs once to stderr and
//! exits; the app runs exactly as before.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use zbus::zvariant::{Array, ObjectPath, Value};

pub const NAME: &str = "org.mpris.MediaPlayer2.amc";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_IFACE: &str = "org.mpris.MediaPlayer2.Player";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
/// Poll cadence for snapshot → `PropertiesChanged` diffing. Position is
/// served on `Get` (fresh up to one frame), never signal-spammed.
const POLL: std::time::Duration = std::time::Duration::from_millis(150);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

impl PlaybackStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Playing => "Playing",
            Self::Paused => "Paused",
            Self::Stopped => "Stopped",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoopStatus {
    None,
    Track,
    Playlist,
}

impl LoopStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Track => "Track",
            Self::Playlist => "Playlist",
        }
    }
}

/// D-Bus → UI intents; dispatched onto the same functions the UI buttons
/// and keyboard shortcuts use.
pub enum Command {
    Play,
    Pause,
    TogglePause,
    Next,
    Previous,
    /// Clear now-playing + queue (the queue panel's "clear", plus the track).
    Stop,
    /// Relative seek in µs (MPRIS `Seek`).
    Seek(i64),
    /// Absolute seek in µs (MPRIS `SetPosition`).
    SetPosition(u64),
    SetLoop(LoopStatus),
    SetShuffle(bool),
    SetVolume(f32),
    Raise,
    Quit,
}

/// Everything MPRIS exposes, re-published by the UI every frame.
#[derive(Clone)]
pub struct Snapshot {
    pub status: PlaybackStatus,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub art_url: Option<String>,
    /// Catalog id of the current track; empty when idle (mints the
    /// `mpris:trackid` object path).
    pub track_id: String,
    pub length: std::time::Duration,
    /// Same source as the progress bar (`App::playback_position`).
    pub position: std::time::Duration,
    pub volume: f32,
    pub shuffle: bool,
    pub loop_status: LoopStatus,
    pub has_track: bool,
    pub can_next: bool,
    pub can_prev: bool,
    /// Bumped by the UI on every explicit seek; triggers `Seeked`.
    pub seek_epoch: u64,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            status: PlaybackStatus::Stopped,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            art_url: None,
            track_id: String::new(),
            length: std::time::Duration::ZERO,
            position: std::time::Duration::ZERO,
            volume: 0.8,
            shuffle: false,
            loop_status: LoopStatus::None,
            has_track: false,
            can_next: false,
            can_prev: false,
            seek_epoch: 0,
        }
    }
}

struct Shared {
    state: Mutex<Snapshot>,
}

/// Handle held by the UI thread.
pub struct Mpris {
    pub rx: Receiver<Command>,
    shared: Arc<Shared>,
}

impl Mpris {
    /// Spawn the D-Bus server thread. Returns immediately — the connection
    /// is established on the thread, never on the UI thread. Returns `None`
    /// only if the OS refuses to spawn a thread; a missing session bus is
    /// handled inside the thread (log once, disable).
    pub fn spawn() -> Option<Self> {
        let (tx, rx) = channel();
        let shared = Arc::new(Shared {
            state: Mutex::new(Snapshot::default()),
        });
        let ok = std::thread::Builder::new()
            .name("mpris".into())
            .spawn({
                let shared = shared.clone();
                move || run(shared, tx)
            })
            .is_ok();
        ok.then_some(Self { rx, shared })
    }

    /// Per-frame publish: lock + move. The D-Bus thread does all diffing.
    pub fn publish(&self, snap: Snapshot) {
        *self.shared.state.lock() = snap;
    }
}

/// Connect to the session bus, take the player name and serve both
/// interfaces. All failures (no bus, name taken) land here as one error.
fn build_conn(shared: &Arc<Shared>, tx: Sender<Command>) -> zbus::Result<zbus::blocking::Connection> {
    zbus::blocking::connection::Builder::session()?
        .name(NAME)?
        .replace_existing_names(true)
        .serve_at(PATH, Root { tx: tx.clone() })?
        .serve_at(PATH, Player { tx, shared: shared.clone() })?
        .build()
}

fn run(shared: Arc<Shared>, tx: Sender<Command>) {
    let conn = match build_conn(&shared, tx) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[mpris] no session bus ({e:#}) — desktop media controls disabled");
            return;
        }
    };
    eprintln!("[mpris] serving {NAME} at {PATH}");
    let mut emitted = Emitted::default();
    loop {
        std::thread::sleep(POLL);
        let snap = shared.state.lock().clone();
        let gone = |what: &str, e: zbus::Error| {
            eprintln!("[mpris] {what} failed ({e}) — bus gone, disabling");
        };

        let mut changed: HashMap<&str, Value> = HashMap::new();
        if snap.status.as_str() != emitted.status {
            changed.insert("PlaybackStatus", Value::new(snap.status.as_str()));
        }
        if snap.loop_status.as_str() != emitted.loop_status {
            changed.insert("LoopStatus", Value::new(snap.loop_status.as_str()));
        }
        if snap.shuffle != emitted.shuffle {
            changed.insert("Shuffle", Value::new(snap.shuffle));
        }
        if f64::from(snap.volume) != emitted.volume {
            changed.insert("Volume", Value::new(f64::from(snap.volume)));
        }
        if metadata_sig(&snap) != emitted.metadata_sig {
            changed.insert("Metadata", Value::new(metadata(&snap)));
        }
        if snap.has_track != emitted.has_track {
            changed.insert("CanSeek", Value::new(snap.has_track));
            changed.insert("CanPause", Value::new(snap.has_track));
        }
        if snap.can_next != emitted.can_next {
            changed.insert("CanGoNext", Value::new(snap.can_next));
        }
        if snap.can_prev != emitted.can_prev {
            changed.insert("CanGoPrevious", Value::new(snap.can_prev));
        }
        if !changed.is_empty() {
            // Vec (not a slice literal): keeps the `as` signature even when
            // empty — an empty serde seq would serialize as a bare BYTE.
            let invalidated: Vec<&str> = Vec::new();
            if let Err(e) = conn.emit_signal(
                None::<&str>,
                PATH,
                PROPS_IFACE,
                "PropertiesChanged",
                &(&PLAYER_IFACE, &changed, &invalidated),
            ) {
                gone("PropertiesChanged", e);
                return;
            }
        }

        // Position never rides PropertiesChanged (spec); a discontinuity
        // (user/MPRIS seek) is announced with Seeked instead.
        if snap.seek_epoch != emitted.seek_epoch {
            let pos = snap.position.as_micros() as i64;
            if let Err(e) = conn.emit_signal(None::<&str>, PATH, PLAYER_IFACE, "Seeked", &pos) {
                gone("Seeked", e);
                return;
            }
        }

        emitted = Emitted {
            status: snap.status.as_str(),
            loop_status: snap.loop_status.as_str(),
            shuffle: snap.shuffle,
            volume: f64::from(snap.volume),
            metadata_sig: metadata_sig(&snap),
            has_track: snap.has_track,
            can_next: snap.can_next,
            can_prev: snap.can_prev,
            seek_epoch: snap.seek_epoch,
        };
    }
}

/// Cheap change signature for the Metadata dict (the dict itself is only
/// built when this differs).
fn metadata_sig(s: &Snapshot) -> String {
    format!(
        "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
        s.track_id,
        s.title,
        s.artist,
        s.album,
        s.art_url.as_deref().unwrap_or(""),
        s.length.as_millis(),
    )
}

fn trackid_path(s: &Snapshot) -> String {
    let safe: String = s
        .track_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    format!("{PATH}/Track/{safe}")
}

fn metadata(s: &Snapshot) -> HashMap<String, Value<'static>> {
    let mut md: HashMap<String, Value<'static>> = HashMap::new();
    if !s.has_track {
        return md; // empty dict when nothing is playing
    }
    md.insert(
        "mpris:trackid".into(),
        Value::new(ObjectPath::from_string_unchecked(trackid_path(s))),
    );
    md.insert(
        "mpris:length".into(),
        Value::new(s.length.as_micros() as i64),
    );
    if let Some(art) = &s.art_url {
        md.insert("mpris:artUrl".into(), Value::new(art.clone()));
    }
    md.insert("xesam:title".into(), Value::new(s.title.clone()));
    md.insert(
        "xesam:artist".into(),
        Value::new(Array::from(vec![s.artist.clone()])),
    );
    md.insert("xesam:album".into(), Value::new(s.album.clone()));
    md
}

// ---- org.mpris.MediaPlayer2 (root interface) ----

struct Root {
    tx: Sender<Command>,
}

/// Last values signalled via PropertiesChanged — the diff baseline.
#[derive(Default)]
struct Emitted {
    status: &'static str,
    loop_status: &'static str,
    shuffle: bool,
    volume: f64,
    metadata_sig: String,
    has_track: bool,
    can_next: bool,
    can_prev: bool,
    seek_epoch: u64,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {
        let _ = self.tx.send(Command::Raise);
    }

    fn quit(&self) {
        let _ = self.tx.send(Command::Quit);
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn fullscreen(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> String {
        "amc".to_string()
    }

    #[zbus(property)]
    fn desktop_entry(&self) -> String {
        "amc".to_string()
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

// ---- org.mpris.MediaPlayer2.Player ----

struct Player {
    tx: Sender<Command>,
    shared: Arc<Shared>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) {
        let _ = self.tx.send(Command::Next);
    }

    fn previous(&self) {
        let _ = self.tx.send(Command::Previous);
    }

    fn pause(&self) {
        let _ = self.tx.send(Command::Pause);
    }

    fn play_pause(&self) {
        let _ = self.tx.send(Command::TogglePause);
    }

    fn stop(&self) {
        let _ = self.tx.send(Command::Stop);
    }

    fn play(&self) {
        let _ = self.tx.send(Command::Play);
    }

    fn seek(&self, offset: i64) {
        let _ = self.tx.send(Command::Seek(offset));
    }

    fn set_position(&self, track_id: zbus::zvariant::OwnedObjectPath, position: i64) {
        // Spec: ignore positions addressed at another track.
        let s = self.shared.state.lock();
        let matches = track_id.as_str() == trackid_path(&s);
        drop(s);
        if matches && position >= 0 {
            let _ = self.tx.send(Command::SetPosition(position as u64));
        }
    }

    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.shared.state.lock().status.as_str().to_string()
    }

    #[zbus(property)]
    fn loop_status(&self) -> String {
        self.shared.state.lock().loop_status.as_str().to_string()
    }

    #[zbus(property)]
    fn set_loop_status(&self, status: &str) {
        let l = match status {
            "None" => LoopStatus::None,
            "Track" => LoopStatus::Track,
            "Playlist" => LoopStatus::Playlist,
            _ => return,
        };
        let _ = self.tx.send(Command::SetLoop(l));
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn set_rate(&self, _rate: f64) {
        // 1.0 is the only supported rate; anything else is ignored.
    }

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        self.shared.state.lock().shuffle
    }

    #[zbus(property)]
    fn set_shuffle(&self, shuffle: bool) {
        let _ = self.tx.send(Command::SetShuffle(shuffle));
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, Value<'static>> {
        metadata(&self.shared.state.lock())
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        f64::from(self.shared.state.lock().volume)
    }

    #[zbus(property)]
    fn set_volume(&self, volume: f64) {
        let _ = self.tx.send(Command::SetVolume(volume.clamp(0.0, 1.0) as f32));
    }

    #[zbus(property)]
    fn position(&self) -> i64 {
        self.shared.state.lock().position.as_micros() as i64
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.shared.state.lock().can_next
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.shared.state.lock().can_prev
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.shared.state.lock().has_track
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.shared.state.lock().has_track
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}
