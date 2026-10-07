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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            storefront: "us".into(),
            wrapper_url: "http://127.0.0.1:12340".into(),
            media_user_token: None,
            cache_dir: home().join(".cache/amc"),
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
         cache_dir = \"{}\"\n",
        cfg.storefront,
        cfg.wrapper_url,
        cfg.cache_dir.display()
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
