use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize, Clone)]
#[serde(default)]
pub struct Config {
    /// Apple Music storefront country code.
    pub storefront: String,
    /// wrapper-lite HTTP endpoint.
    pub wrapper_url: String,
    /// Optional media-user-token for personalized API (library, playlists).
    pub media_user_token: Option<String>,
    /// Cache directory (bearer token, artwork, audio segments).
    pub cache_dir: PathBuf,
    /// Report plays to Apple's Universal Activity Service (Recently Played /
    /// play counts). Default true when the key is absent.
    pub report_plays: bool,
    /// Audio-cache budget in GiB. Whole track dirs are evicted
    /// oldest-last-played first. 0 disables the audio cache (the metadata
    /// cache is unaffected and TTL-cleaned on startup).
    pub cache_size_gb: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            storefront: "us".into(),
            wrapper_url: "http://127.0.0.1:12340".into(),
            media_user_token: None,
            cache_dir: home().join(".cache/mordent"),
            report_plays: true,
            cache_size_gb: 10,
        }
    }
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

fn config_path() -> PathBuf {
    home().join(".config/mordent/config.toml")
}

/// Path of the user config file (`~/.config/mordent/config.toml`).
pub fn config_file() -> PathBuf {
    config_path()
}

/// Whether a config file already exists. The setup wizard uses this to decide
/// first-run entry BEFORE [`Config::load`] auto-writes a default file.
pub fn config_exists() -> bool {
    config_path().exists()
}

impl Config {
    /// Load from `~/.config/mordent/config.toml`, creating a default file if absent.
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            let cfg = Config::default();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("creating {}", dir.display()))?;
            }
            std::fs::write(&path, default_toml(&cfg))
                .with_context(|| format!("writing {}", path.display()))?;
            return Ok(cfg);
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn ensure_cache_dir(&self) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.cache_dir).context("creating cache dir")?;
        Ok(self.cache_dir.clone())
    }
}

fn default_toml(cfg: &Config) -> String {
    format!(
        "# Mordent configuration\n\
         storefront = \"{}\"\n\
         wrapper_url = \"{}\"\n\
         media_user_token = \"\"\n\
         cache_dir = \"{}\"\n\
         report_plays = true\n\
         cache_size_gb = {}\n",
        cfg.storefront,
        cfg.wrapper_url,
        cfg.cache_dir.display(),
        cfg.cache_size_gb
    )
}

/// Keys the wizard collects and (re)writes; every other key already present in
/// the file is preserved verbatim, so a write never clobbers keys we did not
/// collect (e.g. a hand-added comment-bearing option or a future key).
const KNOWN_KEYS: [&str; 6] = [
    "storefront",
    "wrapper_url",
    "media_user_token",
    "cache_dir",
    "report_plays",
    "cache_size_gb",
];

/// Render a full `config.toml` body from `cfg`, merging over `existing` (parsed
/// as TOML; a missing or unparseable file is treated as empty). Pure, so the
/// wizard's exact write content is unit-tested without touching a HOME.
///
/// Comments in the existing file are not preserved (a TOML round-trip drops
/// them); unknown *keys* are. The wizard only ever writes whole files through
/// this path, so the header below is restored on every write.
pub fn merge_config_toml(existing: Option<&str>, cfg: &Config) -> Result<String> {
    let mut table: toml::Table = match existing {
        Some(text) => toml::from_str(text).unwrap_or_default(),
        None => toml::Table::new(),
    };
    table.insert("storefront".into(), cfg.storefront.clone().into());
    table.insert("wrapper_url".into(), cfg.wrapper_url.clone().into());
    table.insert(
        "media_user_token".into(),
        cfg.media_user_token.clone().unwrap_or_default().into(),
    );
    table.insert(
        "cache_dir".into(),
        cfg.cache_dir.display().to_string().into(),
    );
    table.insert("report_plays".into(), cfg.report_plays.into());
    table.insert("cache_size_gb".into(), (cfg.cache_size_gb as i64).into());

    let mut out = String::from(
        "# Mordent configuration — written by the setup wizard (`mordent setup`).\n\
         # Keys not listed here are preserved as-is across writes.\n",
    );
    for key in KNOWN_KEYS {
        if let Some(v) = table.remove(key) {
            out.push_str(&format!("{key} = {v}\n"));
        }
    }
    // Whatever is left was not collected by the wizard: keep it, sorted.
    for (key, v) in table {
        out.push_str(&format!("{key} = {v}\n"));
    }
    Ok(out)
}

fn tmp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "config.toml".into());
    path.with_file_name(format!(".{name}.tmp"))
}

/// Atomically write `cfg` to `path`, merging over any file already there.
///
/// The temp file is created in the same directory and renamed over the target
/// (atomic on one filesystem); it is created 0600 because the file can hold a
/// media-user-token.
pub fn write_config_at(path: &Path, cfg: &Config) -> Result<()> {
    let existing = std::fs::read_to_string(path).ok();
    let text = merge_config_toml(existing.as_deref(), cfg)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let tmp = tmp_path(path);
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        use std::io::Write;
        f.write_all(text.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Write `cfg` to the default `~/.config/mordent/config.toml` location.
pub fn write_config(cfg: &Config) -> Result<()> {
    write_config_at(&config_path(), cfg)
}

/// Convenience: expand a path under the cache dir, ensuring it exists.
pub fn cache_path(cfg: &Config, rel: &str) -> Result<PathBuf> {
    let p = cfg.cache_dir.join(rel);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(p)
}

/// True if file exists and is newer than `max_age_secs`.
pub fn fresh(path: &Path, max_age_secs: u64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    modified
        .elapsed()
        .map(|e| e.as_secs() < max_age_secs)
        .unwrap_or(false)
}

/// Unix seconds now. Injected as a plain `u64` everywhere a cache needs a
/// clock so freshness/eviction decisions stay pure and testable.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Stable 128-bit FNV-1a (two independent lanes) as hex — cache-file key
/// material for URLs / key URIs. Hand-rolled to avoid a hash crate.
pub fn fnv128(data: &[u8]) -> String {
    const PRIME: u64 = 0x1000_0000_01b3;
    let mut h1: u64 = 0xcbf2_9ce4_8422_2325;
    let mut h2: u64 = 0x2545_f491_4f6c_dd1d;
    for &b in data {
        h1 = (h1 ^ b as u64).wrapping_mul(PRIME);
        h2 = (h2 ^ (b as u64).rotate_left(17)).wrapping_mul(PRIME);
    }
    format!("{h1:016x}{h2:016x}")
}

#[cfg(test)]
mod wizard_write_tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mordent-config-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn cfg() -> Config {
        Config {
            storefront: "de".into(),
            wrapper_url: "http://music-box.local:12340".into(),
            media_user_token: Some("tok".into()),
            cache_dir: PathBuf::from("/tmp/mordent-cache"),
            report_plays: false,
            cache_size_gb: 7,
        }
    }

    #[test]
    fn merge_writes_every_collected_key() {
        let text = merge_config_toml(None, &cfg()).unwrap();
        assert!(text.contains("storefront = \"de\""));
        assert!(text.contains("wrapper_url = \"http://music-box.local:12340\""));
        assert!(text.contains("media_user_token = \"tok\""));
        assert!(text.contains("cache_dir = \"/tmp/mordent-cache\""));
        assert!(text.contains("report_plays = false"));
        assert!(text.contains("cache_size_gb = 7"));
        // round-trips through the real parser
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.storefront, "de");
        assert_eq!(back.cache_size_gb, 7);
        assert!(!back.report_plays);
    }

    #[test]
    fn merge_preserves_unknown_keys_and_overrides_known_ones() {
        let existing = "storefront = \"jp\"\nfuture_option = \"keep me\"\nnested = { a = 1 }\n";
        let text = merge_config_toml(Some(existing), &cfg()).unwrap();
        assert!(text.contains("storefront = \"de\""), "{text}");
        assert!(!text.contains("jp"), "{text}");
        assert!(text.contains("future_option = \"keep me\""), "{text}");
        assert!(text.contains("nested = { a = 1 }"), "{text}");
    }

    #[test]
    fn merge_tolerates_a_corrupt_existing_file() {
        let text = merge_config_toml(Some("this is not toml ==="), &cfg()).unwrap();
        assert!(text.contains("storefront = \"de\""));
        assert!(toml::from_str::<Config>(&text).is_ok());
    }

    #[test]
    fn write_config_at_is_atomic_and_0600() {
        let dir = tmpdir("write");
        let path = dir.join("config.toml");
        // pre-existing file with an unknown key must survive.
        std::fs::write(&path, "future_option = 42\nstorefront = \"jp\"\n").unwrap();
        write_config_at(&path, &cfg()).unwrap();
        let back: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.storefront, "de");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("future_option = 42"), "{raw}");
        // no temp file left behind
        assert!(!dir.join(".config.toml.tmp").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "config should be private");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
