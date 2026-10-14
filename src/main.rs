mod acache;
mod api;
mod brand;
mod config;
mod fonts;
mod lyrics;
mod player;
mod prefetch;
mod queuemodel;
mod report;
mod session;
mod setup;
mod supervisor;
mod ui;
mod wm;
mod fairplay;
mod mpris;
mod stream;
mod theme;
mod update;
mod wrapper;

// Tray (collapse-to-tray) is Linux-only: ksni speaks StatusNotifierItem over
// D-Bus. On macOS/Windows the stub below keeps the callsites compiling — the
// window-close policy in `ui::update` still applies there (quit when nothing
// has played; hide the window once a track has).
#[cfg(target_os = "linux")]
mod tray;

/// Non-Linux tray stub: no StatusNotifierItem host exists there, so no tray
/// icon is registered. The window-close policy in `ui::update` is unchanged
/// (quit when nothing has played; hide once a track has).
#[cfg(not(target_os = "linux"))]
pub mod tray {
    use crate::session::Session;
    use crate::supervisor::Command;
    use parking_lot::Mutex;
    use std::sync::mpsc::Sender;
    use std::sync::Arc;

    pub fn spawn(
        _snap: Arc<Mutex<crate::supervisor::PbSnapshot>>,
        _cmd: Sender<Command>,
        _session: Session,
    ) {
        eprintln!("[tray] not supported on this platform — running without a tray icon; window close quits when nothing has played, and hides the window once a track has (the window is unmapped, not closed)");
    }
}

use anyhow::{anyhow, bail, Result};

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // Move any pre-rename `~/.config/amc` / `~/.cache/amc` into the `mordent`
    // names BEFORE the first config read — every entry point goes through here
    // (CLI subcommands, `ui`, and the `setup` wizard).
    config::migrate_legacy_dirs();
    // Detect FIRST-RUN before Config::load() auto-writes a default file: with
    // no config.toml, `mordent ui` opens the setup wizard instead of the app.
    let first_run = !config::config_exists();
    let cfg = config::Config::load()?;
    // TTL-clean the metadata cache once per process (audio cache is
    // budget-managed per play attempt).
    api::clean_meta_cache(&cfg);
    let mut args = std::env::args().skip(1);
    // No subcommand → the GUI, routed into the `"ui"` arm below (staged-update
    // finish, then the player — or the guided setup wizard on first run) rather
    // than a copy of it. This is what every graphical launcher does without
    // arguments: the installer's .desktop entry, a double-clicked `mordent.exe`,
    // the macOS .app bundle, and a bare `mordent` typed in a terminal.
    let cmd = args.next().unwrap_or_else(|| "ui".to_string());
    let rest: Vec<String> = args.collect();

    match cmd.as_str() {
        "-h" | "--help" | "help" | "--version" => {
            usage();
            return Ok(());
        }
        "token" => {
            let api = api::Api::new(cfg)?;
            println!("{}", api.bearer()?);
        }
        "search" => {
            let term = if rest.is_empty() {
                bail!("usage: mordent search <term>");
            } else {
                rest.join(" ")
            };
            cmd_search(&cfg, &term)?;
        }
        "album" => {
            let arg = rest.first().ok_or_else(|| anyhow::anyhow!("usage: mordent album <id|url>"))?;
            cmd_album(&cfg, arg)?;
        }
        "wrapper" => cmd_wrapper(&cfg)?,
        "decrypt" => {
            let mut it = rest.iter();
            let id = it.next().ok_or_else(|| anyhow::anyhow!("usage: mordent decrypt <songId> [outfile]"))?;
            let out = it.next().cloned().unwrap_or_else(|| "/tmp/mordent-decrypted.m4a".into());
            cmd_decrypt(&cfg, id, &out)?;
        }
        "decode" => {
            let path = rest.first().ok_or_else(|| anyhow::anyhow!("usage: mordent decode <file>"))?;
            decode_stats(path)?;
        }
        "ui" => {
            apply_staged_update();
            // No config.toml at launch → run the guided setup first.
            if first_run {
                ui::launch_setup(cfg)?;
            } else {
                ui::launch(cfg)?;
            }
        }
        "setup" => {
            apply_staged_update();
            ui::launch_setup(cfg)?;
        }
        "play" => {
            let term = if rest.is_empty() {
                bail!("usage: mordent play <term>");
            } else {
                rest.join(" ")
            };
            cmd_play(&cfg, &term)?;
        }
        "lyrics" => {
            let id = rest.first().ok_or_else(|| anyhow::anyhow!("usage: mordent lyrics <songId>"))?;
            let w = wrapper::Wrapper::new(&cfg.wrapper_url);
            let syllable = rest.iter().any(|a| a == "--syllable");
            let data = w.lyrics(id, syllable)?;
            match data.get("lyrics").and_then(|l| l.as_str()) {
                Some(l) => println!("{l}"),
                None => println!("{data}"),
            }
        }
        "library" => cmd_library(&cfg, &rest)?,
        _ => {
            usage();
            bail!("unknown command {cmd:?}");
        }
    }
    Ok(())
}

/// Finish an update the user staged in a previous session.
///
/// The swap happens HERE — at startup, before any window exists and while
/// nothing is playing — never mid-session: an update cannot interrupt playback,
/// and the files it replaces are not in use (a running Linux binary keeps its
/// own inode; this process has not loaded the new files). A failure is logged
/// and the launch continues on the current files: `update::apply_pending`
/// renames the marker aside so a broken download cannot retry forever.
///
/// Skipped under `MORDENT_SELFTEST` (which must never rewrite the checkout it
/// runs from).
fn apply_staged_update() {
    if std::env::var("MORDENT_SELFTEST").is_ok() {
        if update::read_pending().is_some() {
            eprintln!("[update] MORDENT_SELFTEST: leaving the staged update alone");
        }
        return;
    }
    let mut log = Vec::new();
    match update::apply_pending(&mut log) {
        Ok(None) => {}
        Ok(Some(tag)) => {
            for line in &log {
                eprintln!("[update] {line}");
            }
            eprintln!("[update] {tag} installed; restart Mordent to run it");
        }
        Err(e) => {
            for line in &log {
                eprintln!("[update] {line}");
            }
            eprintln!("[update] could not install the staged update: {e:#}");
        }
    }
}

fn cmd_search(cfg: &config::Config, term: &str) -> Result<()> {
    let api = api::Api::new(cfg.clone())?;
    let t0 = std::time::Instant::now();
    let res = api.search(term, 10)?;
    println!("searched in {:.0?}", t0.elapsed());

    if !res.tracks.is_empty() {
        println!("\nSONGS");
        for (i, t) in res.tracks.iter().enumerate() {
            println!(
                "{:>2}. {:<44} {:<28} {:>5}{} {}",
                i + 1,
                trunc(&t.name, 44),
                trunc(&t.artist, 28),
                fmt_dur(t.duration_ms),
                if t.lossless { " lossless" } else { "" },
                t.id
            );
        }
    }
    if !res.albums.is_empty() {
        println!("\nALBUMS");
        for (i, a) in res.albums.iter().enumerate() {
            println!(
                "{:>2}. {:<44} {:<28} {:>2} tracks {}",
                i + 1,
                trunc(&a.name, 44),
                trunc(&a.artist, 28),
                a.track_count,
                a.id
            );
        }
    }
    if !res.artists.is_empty() {
        println!("\nARTISTS");
        for (i, a) in res.artists.iter().enumerate() {
            println!("{:>2}. {:<44} {}", i + 1, trunc(&a.name, 44), trunc(&a.genres, 40));
        }
    }
    Ok(())
}

fn cmd_album(cfg: &config::Config, arg: &str) -> Result<()> {
    let id = api::id_from_arg(arg)?;
    let api = api::Api::new(cfg.clone())?;
    let tracks = api.album_tracks(&id)?;
    if tracks.is_empty() {
        bail!("no tracks for album {id}");
    }
    let album = tracks
        .first()
        .map(|t| t.album.clone())
        .unwrap_or_default();
    if !album.is_empty() {
        println!("{album}\n");
    }
    for t in &tracks {
        println!(
            "d{:02} t{:02}  {:<44} {:>5}{} {}",
            t.disc_no,
            t.track_no,
            trunc(&t.name, 44),
            fmt_dur(t.duration_ms),
            if t.lossless { " lossless" } else { "" },
            t.id
        );
    }
    Ok(())
}

fn cmd_wrapper(cfg: &config::Config) -> Result<()> {
    let w = wrapper::Wrapper::new(&cfg.wrapper_url);
    match w.regions() {
        Ok(regions) if regions.is_empty() => {
            println!("wrapper at {} is UP but NOT logged in", cfg.wrapper_url);
        }
        Ok(regions) => {
            println!("wrapper at {} is UP, regions: {}", cfg.wrapper_url, regions.join(","));
        }
        Err(e) => {
            println!("wrapper at {} unreachable: {e:#}", cfg.wrapper_url);
        }
    }
    Ok(())
}

fn cmd_library(cfg: &config::Config, rest: &[String]) -> Result<()> {
    let api = api::Api::new(cfg.clone())?;
    if cfg.media_user_token.as_deref().unwrap_or("").is_empty() {
        bail!("no media_user_token in ~/.config/mordent/config.toml");
    }
    let sub = rest.first().map(String::as_str).unwrap_or("songs");
    match sub {
        "songs" => {
            let (songs, next) = api.library_songs(50, 0)?;
            for (i, t) in songs.iter().enumerate() {
                println!(
                    "{:>3}. {:<40} {:<26} {:>5} {}",
                    i + 1,
                    trunc(&t.name, 40),
                    trunc(&t.artist, 26),
                    fmt_dur(t.duration_ms),
                    t.id
                );
            }
            if next.is_some() {
                println!("    … more pages available (offset {:?})", next);
            }
        }
        "albums" => {
            for (i, a) in api.library_albums(50, 0)?.iter().enumerate() {
                println!(
                    "{:>3}. {:<40} {:<26} {:>3} tracks {}",
                    i + 1,
                    trunc(&a.name, 40),
                    trunc(&a.artist, 26),
                    a.track_count,
                    a.id
                );
            }
        }
        term => {
            let _ = term;
            let q = rest.join(" ");
            let res = api.library_search(&q, 25)?;
            println!("library search {q:?}: {} songs", res.tracks.len());
            for (i, t) in res.tracks.iter().enumerate() {
                println!(
                    "{:>3}. {:<40} {:<26} {:>5} {}",
                    i + 1,
                    trunc(&t.name, 40),
                    trunc(&t.artist, 26),
                    fmt_dur(t.duration_ms),
                    t.id
                );
            }
        }
    };
    // a stale-cache hint set while serving the page above (offline mode)
    if let Some(note) = api.take_notice() {
        eprintln!("note: {note}");
    }
    Ok(())
}

fn cmd_play(cfg: &config::Config, term: &str) -> Result<()> {
    use std::io::Write;

    // direct id / URL: play that track instead of searching (6+ digit ids —
    // shorter all-digit queries stay searches)
    let looks_id = term.contains("music.apple.com")
        || (term.chars().all(|c| c.is_ascii_digit()) && term.len() >= 6);
    let track = if looks_id {
        let id = api::id_from_arg(term)?;
        api::Api::new(cfg.clone())?
            .search_by_id(&id)?
            .unwrap_or_else(|| api::Track {
                id: id.clone(),
                name: format!("track {id}"),
                artist: String::new(),
                album: String::new(),
                duration_ms: 0,
                track_no: 0,
                disc_no: 0,
                artwork: None,
                lossless: false,
            })
    } else {
        api::Api::new(cfg.clone())?
            .search(term, 1)?
            .tracks
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("no results for {term:?}"))?
    };
    println!("▶ {} — {} [{}]", track.name, track.artist, track.id);

    let t0 = std::time::Instant::now();
    let (prog, _mode) = stream::open_track(cfg, &track.id, std::slice::from_ref(&track.id))?;

    let player = player::Player::new();
    let mut src = player::PacketSource::new(Box::new(prog))?;

    if player.decode_only {
        // headless: decode and report time-to-music
        let (rate, ch) = (src.sample_rate, src.channels.max(1));
        let one_sec = rate as usize * ch;
        let first: u64 = src.by_ref().take(one_sec).count() as u64;
        let ttm = t0.elapsed();
        println!("  ♪ time to first second of audio: {:.2?} ({} Hz, {} ch)", ttm, rate, ch);
        // drain the rest
        let rest_frames = src.by_ref().count() as u64;
        let total = first + rest_frames;
        println!(
            "  decoded {} frames ({}s of audio) in {:.1?} — {:.1}x realtime",
            total / ch as u64,
            total / ch as u64 / rate as u64,
            t0.elapsed(),
            total as f64 / rate as f64 / t0.elapsed().as_secs_f64()
        );
        let _ = std::io::stdout().flush();
    } else {
        player.enqueue(src);
        if let Some(sink) = player.sink() {
            sink.set_volume(0.8);
            sink.sleep_until_end();
        }
    }
    Ok(())
}

fn cmd_decrypt(cfg: &config::Config, id: &str, out: &str) -> Result<()> {
    let w = wrapper::Wrapper::new(&cfg.wrapper_url);
    let agent = ureq::AgentBuilder::new().build();
    let t0 = std::time::Instant::now();
    let plan = stream::plan_for_track(&w, &agent, id)?;
    println!("plan ready in {:.1?}: {} ({} fragments)", t0.elapsed(), plan.file_url, plan.keys.len());
    let resolver = stream::KeyResolver::new(&w, id);
    let cache = std::path::PathBuf::from(out);
    let src = stream::ProgressiveSource::start_fairplay(plan, &resolver, cache, agent, None)?;
    println!("first bytes in {:.1?}", t0.elapsed());

    // wait for EOF by polling the frontier
    let mut last = 0u64;
    loop {
        if src.is_done()? {
            break;
        }
        let d = src.downloaded();
        if d / (1024 * 1024) != last / (1024 * 1024) {
            println!("  ... {} MB decrypted ({:.1?})", d / (1024 * 1024), t0.elapsed());
        }
        last = d;
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    println!(
        "download+decrypt total: {:.1?} ({} KB)",
        t0.elapsed(),
        src.downloaded() / 1024
    );

    // verify decodability with symphonia
    decode_stats(out)
}

/// Decode a decrypted fragmented file by feeding ALAC packets directly to
/// symphonia (bypassing its mp4 demuxer). Prints codec/rate/frames stats.
fn decode_stats(path: &str) -> Result<()> {
    use symphonia::core::audio::Channels;
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::formats::Packet;
    use std::io::BufReader;

    let mut f = BufReader::new(std::fs::File::open(path)?);

    // init: ftyp + moov
    let mut init_raw = Vec::new();
    for _ in 0..2 {
        let (h, payload) = fairplay::read_box_pub(&mut f)?;
        init_raw.extend_from_slice(&h.assemble_pub(&payload));
    }
    let alac = fairplay::find_alac_cookie(&init_raw)
        .ok_or_else(|| anyhow!("no ALAC cookie in init"))?;
    // the raw 24-byte ALACSpecificConfig, as the decoder's extradata
    let extra = fairplay::alac_extra_data(&init_raw);
    println!(
        "codec: alac, sample_rate: {}, channels: {}, bit_depth: {}, frame_len: {}",
        alac.sample_rate, alac.channels, alac.bit_depth, alac.frame_len
    );

    let channels = if alac.channels == 1 {
        Channels::FRONT_LEFT
    } else {
        Channels::FRONT_LEFT | Channels::FRONT_RIGHT
    };
    let mut params = CodecParameters::new();
    params
        .for_codec(symphonia::core::codecs::CODEC_TYPE_ALAC)
        .with_sample_rate(alac.sample_rate)
        .with_channels(channels)
        .with_max_frames_per_packet(alac.frame_len as u64);
    if let Some(x) = extra {
        params.with_extra_data(x);
    }
    let mut decoder = symphonia::default::get_codecs().make(&params, &Default::default())?;

    let mut frames_total = 0u64;
    let mut packets_total = 0u64;
    let t0 = std::time::Instant::now();
    loop {
        match fairplay::read_fragment(&mut f) {
            Ok(Some(frag)) => {
                let mut ts = frag.base_time;
                for (i, sample) in frag.samples.iter().enumerate() {
                    let dur = (*frag.durations.get(i).unwrap_or(&0)).max(1) as u64;
                    let pkt = Packet::new_from_boxed_slice(
                        1,
                        ts,
                        dur,
                        sample.clone().into_boxed_slice(),
                    );
                    ts += dur;
                    match decoder.decode(&pkt) {
                        Ok(decoded) => frames_total += decoded.frames() as u64,
                        Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                        Err(e) => return Err(e.into()),
                    }
                    packets_total += 1;
                }
            }
            Ok(None) => break,
            Err(e) => return Err(e),
        }
    }
    let dt = t0.elapsed();
    println!(
        "decoded {} frames / {} packets in {:.1?} (rate: {:.1}x realtime)",
        frames_total,
        packets_total,
        dt,
        frames_total as f64 / alac.sample_rate as f64 / dt.as_secs_f64()
    );
    Ok(())
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn fmt_dur(ms: u64) -> String {
    format!("{}:{:02}", ms / 60000, (ms % 60000) / 1000)
}

fn usage() {
    eprintln!(
        "Mordent — Apple Music client (dev CLI)\n\n\
         commands:\n\
         \x20 (no command)     open the app (same as `ui`)\n\
         \x20 token             print the cached web bearer token\n\
         \x20 search <term>     search catalog\n\
         \x20 album <id|url>    list album tracks\n\
         \x20 wrapper           wrapper-lite status\n\
         \x20 decrypt <songId> [out]  stream + FairPlay-decrypt a track to file\n\
         \x20 decode <file>     decode a decrypted fragmented file (verify)\n\
         \x20 ui                the app (first run: guided setup wizard)\n\
         \x20 setup             guided setup wizard (install helper, log in, prefs)\n"
    );
}
