mod api;
mod config;
mod fairplay;
mod stream;
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
        "decrypt" => {
            let mut it = rest.iter();
            let id = it.next().ok_or_else(|| anyhow::anyhow!("usage: amc decrypt <songId> [outfile]"))?;
            let out = it.next().cloned().unwrap_or_else(|| "/tmp/amc-decrypted.m4a".into());
            cmd_decrypt(&cfg, id, &out)?;
        }
        "decode" => {
            let path = rest.first().ok_or_else(|| anyhow::anyhow!("usage: amc decode <file>"))?;
            decode_stats(path)?;
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

fn cmd_decrypt(cfg: &config::Config, id: &str, out: &str) -> Result<()> {
    let w = wrapper::Wrapper::new(&cfg.wrapper_url);
    let agent = ureq::AgentBuilder::new().build();
    let t0 = std::time::Instant::now();
    let plan = stream::plan_for_track(&w, &agent, id)?;
    println!("plan ready in {:.1?}: {} ({} fragments)", t0.elapsed(), plan.file_url, plan.keys.len());
    let resolver = stream::KeyResolver::new(&w, id);
    let cache = std::path::PathBuf::from(out);
    let src = stream::ProgressiveSource::start_fairplay(plan, &resolver, cache, agent)?;
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

/// Probe + decode a file, printing codec/rate/samples. Returns decode time.
fn decode_stats(path: &str) -> Result<()> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("m4a");
    let probed = symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != symphonia::core::codecs::CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow::anyhow!("no audio track"))?;
    let params = track.codec_params.clone();
    println!(
        "codec: {:?}, sample_rate: {:?}, channels: {:?}, duration: {:?}",
        params.codec, params.sample_rate, params.channels, params.time_base
    );
    let mut decoder = symphonia::default::get_codecs().make(&params, &DecoderOptions::default())?;
    let mut samples_total = 0u64;
    let t0 = std::time::Instant::now();
    let mut buf: Option<SampleBuffer<f32>> = None;
    loop {
        match format.next_packet() {
            Ok(pkt) => match decoder.decode(&pkt) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let cap = (decoded.frames().max(1) as usize) * spec.channels.count();
                    samples_total += decoded.frames() as u64;
                    let sb = buf.get_or_insert_with(|| SampleBuffer::<f32>::new(cap as u64, spec));
                    if sb.capacity() < cap {
                        *sb = SampleBuffer::<f32>::new(cap as u64, spec);
                    }
                    sb.copy_interleaved_ref(decoded);
                }
                Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                Err(e) => return Err(e.into()),
            },
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break
            }
            Err(symphonia::core::errors::Error::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        }
    }
    let dt = t0.elapsed();
    println!(
        "decoded {} frames in {:.1?} (rate: {:.1}x realtime)",
        samples_total,
        dt,
        samples_total as f64
            / params.sample_rate.unwrap_or(44100) as f64
            / dt.as_secs_f64()
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
        "amc — Apple Music client (dev CLI)\n\n\
         commands:\n\
         \x20 token            print the cached web bearer token\n\
         \x20 search <term>    search catalog\n\
         \x20 album <id|url>   list album tracks\n\
         \x20 wrapper          wrapper-lite status\n\
         \x20 decrypt <songId> [out]  stream + FairPlay-decrypt a track, verify decode\n"
    );
}
