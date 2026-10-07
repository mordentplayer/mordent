mod api;
mod config;
mod wrapper;

use anyhow::{bail, Result};

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cfg = config::Config::load()?;
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        usage();
        bail!("no command given");
    };
    let rest: Vec<String> = args.collect();

    match cmd.as_str() {
        "token" => {
            let api = api::Api::new(cfg)?;
            println!("{}", api.bearer()?);
        }
        "search" => {
            let term = if rest.is_empty() {
                bail!("usage: amc search <term>");
            } else {
                rest.join(" ")
            };
            cmd_search(&cfg, &term)?;
        }
        "album" => {
            let arg = rest.first().ok_or_else(|| anyhow::anyhow!("usage: amc album <id|url>"))?;
            cmd_album(&cfg, arg)?;
        }
        "wrapper" => cmd_wrapper(&cfg)?,
        "m3u8" => {
            let id = rest.first().ok_or_else(|| anyhow::anyhow!("usage: amc m3u8 <songId>"))?;
            cmd_m3u8(&cfg, id)?;
        }
        _ => {
            usage();
            bail!("unknown command {cmd:?}");
        }
    }
    Ok(())
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

fn cmd_m3u8(cfg: &config::Config, id: &str) -> Result<()> {
    let w = wrapper::Wrapper::new(&cfg.wrapper_url);
    let m = w.m3u8(id)?;
    println!("{m}");
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
        "amc — Apple Music client (dev CLI)\n\n\
         commands:\n\
         \x20 token            print the cached web bearer token\n\
         \x20 search <term>    search catalog\n\
         \x20 album <id|url>   list album tracks\n\
         \x20 wrapper          wrapper-lite status\n\
         \x20 m3u8 <songId>    fetch playback manifest via wrapper\n"
    );
}
