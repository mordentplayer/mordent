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
    /// Check GitHub Releases for a newer Mordent version. At most one
    /// unauthenticated request per day; see `update.rs`.
    pub check_updates: bool,
    /// Streaming quality: which HLS variant the player picks out of a track's
    /// master manifest (see `stream::pick_variant`). Absent from older configs
    /// → [`Quality::Lossless`], the behaviour every build before this key had.
    pub quality: Quality,
}

/// Playback quality — which audio variant of a master manifest to stream.
///
/// The wrapper offers the same set for every track observed so far: ALAC
/// lossless (44.1 kHz, 16- or 24-bit), AAC 256, AAC 128 and HE-AAC 64
/// (see the README's "Playback quality" section). All three levels degrade
/// gracefully when a variant is missing.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Quality {
    /// ALAC when the track offers it, else the highest-bitrate AAC. Default —
    /// this is what Mordent has always streamed.
    #[default]
    Lossless,
    /// Highest-bitrate AAC variant (typically AAC 256): roughly a quarter of
    /// the lossless bandwidth.
    High,
    /// Lowest-bitrate variant (typically HE-AAC 64): the smallest download,
    /// for slow links or metered data.
    DataSaver,
}

impl Quality {
    /// The `quality` key's spelling in `config.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lossless => "lossless",
            Self::High => "high",
            Self::DataSaver => "data_saver",
        }
    }

    /// Parse a `quality` value; `None` when it is not one of the known names.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "lossless" => Some(Self::Lossless),
            "high" => Some(Self::High),
            "data_saver" => Some(Self::DataSaver),
            _ => None,
        }
    }

    /// Human label for the settings window.
    pub fn label(self) -> &'static str {
        match self {
            Self::Lossless => "Lossless (ALAC)",
            Self::High => "High (AAC)",
            Self::DataSaver => "Data saver (HE-AAC)",
        }
    }

    /// One-line explanation shown under the radio group.
    pub fn detail(self) -> &'static str {
        match self {
            Self::Lossless => "ALAC 44.1 kHz; falls back to AAC when a track has no lossless variant",
            Self::High => "best AAC variant — much smaller than lossless",
            Self::DataSaver => "lowest-bitrate variant — smallest downloads",
        }
    }

    /// Every level, in settings-window order.
    pub const ALL: [Quality; 3] = [Self::Lossless, Self::High, Self::DataSaver];
}

impl std::fmt::Display for Quality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            storefront: "us".into(),
            wrapper_url: "http://127.0.0.1:12340".into(),
            media_user_token: None,
            cache_dir: default_cache_dir(),
            report_plays: true,
            cache_size_gb: 10,
            check_updates: true,
            quality: Quality::Lossless,
        }
    }
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

/// The `mordent` config directory (`~/.config/mordent`).
pub fn config_dir() -> PathBuf {
    home().join(".config/mordent")
}

/// The default `mordent` cache directory (`~/.cache/mordent`).
pub fn default_cache_dir() -> PathBuf {
    home().join(".cache/mordent")
}

/// Pre-rename config directory (`~/.config/amc`); see [`migrate_legacy_dirs`].
fn legacy_config_dir() -> PathBuf {
    home().join(".config/amc")
}

/// Pre-rename cache directory (`~/.cache/amc`); see [`migrate_legacy_dirs`].
fn legacy_cache_dir() -> PathBuf {
    home().join(".cache/amc")
}

/// What to do about one legacy→new directory pair (pure; unit-tested).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Migration {
    /// The legacy dir is absent — nothing to migrate.
    Absent,
    /// The new dir already exists — leave the legacy dir alone.
    AlreadyNew,
    /// Legacy exists and the new path is free — move it.
    Move,
}

/// Decide the migration for a legacy→new directory pair. Only a legacy dir
/// whose new counterpart does not exist is moved; a live new dir is never
/// clobbered.
pub fn plan_migration(legacy: &Path, new: &Path) -> Migration {
    if !legacy.exists() {
        Migration::Absent
    } else if new.exists() {
        Migration::AlreadyNew
    } else {
        Migration::Move
    }
}

/// Recursive copy fallback for [`move_dir`] when `rename` fails (e.g. across
/// filesystems). Files are copied; directories are recreated.
fn copy_dir_all(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)
                .with_context(|| format!("copying {}", src.display()))?;
        }
    }
    Ok(())
}

/// Move `from` to `to`, falling back to copy + remove when `rename` fails.
fn move_dir(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_dir_all(from, to)?;
            std::fs::remove_dir_all(from)
                .with_context(|| format!("removing {}", from.display()))?;
            Ok(())
        }
    }
}

/// Migrate one legacy dir, logging exactly one line when it acts. Best-effort:
/// a failure is logged and swallowed (callers must not be blocked).
fn migrate_dir(label: &str, legacy: &Path, new: &Path) {
    if plan_migration(legacy, new) != Migration::Move {
        return;
    }
    match move_dir(legacy, new) {
        Ok(()) => eprintln!(
            "[migrate] {label}: moved {} -> {}",
            legacy.display(),
            new.display()
        ),
        Err(e) => eprintln!(
            "[migrate] {label}: could not move {} -> {} ({e:#})",
            legacy.display(),
            new.display()
        ),
    }
}

/// Pure core of [`rewrite_legacy_cache_dir`]: swap `old` → `new` in `text`,
/// returning the new body, or `None` when there is nothing to change.
fn swap_cache_dir(text: &str, old: &str, new: &str) -> Option<String> {
    if !text.contains(old) {
        return None;
    }
    let updated = text.replace(old, new);
    (updated != text).then_some(updated)
}

/// Rewrite a `cache_dir` value still pointing at the pre-rename cache path so
/// a migrated config keeps using the moved cache. Leaves the rest of the file
/// byte-for-byte intact (plain substring swap, no TOML round-trip).
fn rewrite_legacy_cache_dir() {
    let path = config_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let old = legacy_cache_dir();
    let old = old.to_string_lossy();
    let new = default_cache_dir();
    let Some(updated) = swap_cache_dir(&text, old.as_ref(), new.to_string_lossy().as_ref())
    else {
        return;
    };
    let tmp = tmp_path(&path);
    if std::fs::write(&tmp, updated).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        eprintln!(
            "[migrate] config: cache_dir {} -> {}",
            old,
            new.display()
        );
    }
}

/// Move the pre-rename `amc` directories to their `mordent` names. Idempotent
/// and safe to call from every entry point before the first config read:
///
/// * `~/.config/amc` → `~/.config/mordent` (then a stale `cache_dir` pointing
///   at the legacy cache path is rewritten in place);
/// * `~/.cache/amc` → `~/.cache/mordent` (best-effort — never blocks startup).
///
/// A legacy dir is only moved when the new path is free; an existing new dir is
/// never clobbered.
pub fn migrate_legacy_dirs() {
    migrate_dir("config", &legacy_config_dir(), &config_dir());
    rewrite_legacy_cache_dir();
    migrate_dir("cache", &legacy_cache_dir(), &default_cache_dir());
}

fn config_path() -> PathBuf {
    config_dir().join("config.toml")
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
         cache_size_gb = {}\n\
         check_updates = true\n\
         quality = \"{}\"\n",
        cfg.storefront,
        cfg.wrapper_url,
        cfg.cache_dir.display(),
        cfg.cache_size_gb,
        cfg.quality.as_str()
    )
}

/// Keys the wizard collects and (re)writes; every other key already present in
/// the file is preserved verbatim, so a write never clobbers keys we did not
/// collect (e.g. a hand-added comment-bearing option or a future key).
const KNOWN_KEYS: [&str; 8] = [
    "storefront",
    "wrapper_url",
    "media_user_token",
    "cache_dir",
    "report_plays",
    "cache_size_gb",
    "check_updates",
    "quality",
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
    table.insert("check_updates".into(), cfg.check_updates.into());
    table.insert("quality".into(), cfg.quality.as_str().into());

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
            check_updates: false,
            quality: Quality::High,
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
        assert!(text.contains("check_updates = false"));
        assert!(text.contains("quality = \"high\""));
        // round-trips through the real parser
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.storefront, "de");
        assert_eq!(back.cache_size_gb, 7);
        assert!(!back.report_plays);
        assert_eq!(back.quality, Quality::High);
    }

    #[test]
    fn missing_quality_key_defaults_to_lossless() {
        // A config written before `quality` existed must load unchanged and
        // keep streaming lossless (the pre-key behaviour).
        let old = "storefront = \"us\"\nwrapper_url = \"http://127.0.0.1:12340\"\n";
        let cfg: Config = toml::from_str(old).unwrap();
        assert_eq!(cfg.quality, Quality::Lossless);
        assert_eq!(Quality::Lossless.as_str(), "lossless");
        // Every level round-trips through the key spelling.
        for q in Quality::ALL {
            let text = format!("quality = \"{}\"\n", q.as_str());
            assert_eq!(toml::from_str::<Config>(&text).unwrap().quality, q);
        }
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

#[cfg(test)]
mod migration_tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mordent-migration-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn plan_migration_only_moves_an_unshadowed_legacy_dir() {
        let root = tmpdir("plan");
        let legacy = root.join(".config/amc");
        let new = root.join(".config/mordent");

        assert_eq!(plan_migration(&legacy, &new), Migration::Absent);

        std::fs::create_dir_all(&legacy).unwrap();
        assert_eq!(plan_migration(&legacy, &new), Migration::Move);

        std::fs::create_dir_all(&new).unwrap();
        assert_eq!(plan_migration(&legacy, &new), Migration::AlreadyNew);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn migrate_dir_moves_legacy_contents_and_is_idempotent() {
        let root = tmpdir("move");
        let legacy = root.join("old");
        let new = root.join("new");
        std::fs::create_dir_all(legacy.join("meta")).unwrap();
        std::fs::write(legacy.join("meta/a.cache"), b"x").unwrap();
        std::fs::write(legacy.join("config.toml"), b"storefront = \"us\"\n").unwrap();

        migrate_dir("config", &legacy, &new);
        assert!(!legacy.exists(), "legacy dir should be gone");
        assert_eq!(std::fs::read(new.join("meta/a.cache")).unwrap(), b"x");
        assert!(new.join("config.toml").is_file());

        // A second run has nothing left to move.
        migrate_dir("config", &legacy, &new);
        assert!(new.join("config.toml").is_file());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn migrate_dir_keeps_both_when_new_exists() {
        let root = tmpdir("both");
        let legacy = root.join("old");
        let new = root.join("new");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("old-marker"), b"old").unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("new-marker"), b"new").unwrap();

        migrate_dir("cache", &legacy, &new);
        assert!(legacy.join("old-marker").is_file(), "legacy untouched");
        assert!(new.join("new-marker").is_file(), "new untouched");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn swap_cache_dir_rewrites_only_the_legacy_path() {
        let text = "storefront = \"us\"\ncache_dir = \"/home/a/.cache/amc\"\n";
        let got = swap_cache_dir(text, "/home/a/.cache/amc", "/home/a/.cache/mordent").unwrap();
        assert!(
            got.contains("cache_dir = \"/home/a/.cache/mordent\""),
            "{got}"
        );
        assert!(!got.contains(".cache/amc\""), "{got}");
        // Already-new configs are left alone.
        assert_eq!(
            swap_cache_dir(&got, "/home/a/.cache/amc", "/home/a/.cache/mordent"),
            None
        );
    }
}
