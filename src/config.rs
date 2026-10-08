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
            cache_dir: home().join(".cache/amc"),
            report_plays: true,
            cache_size_gb: 10,
        }
    }
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

fn config_path() -> PathBuf {
    home().join(".config/amc/config.toml")
}

impl Config {
    /// Load from `~/.config/amc/config.toml`, creating a default file if absent.
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
        "# amc configuration\n\
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
