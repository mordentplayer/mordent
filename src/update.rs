//! Update checking and (where it is honest to do it) self-updating.
//!
//! Three separate concerns live here, each with a pure core that is unit-tested
//! without touching the network or the filesystem beyond a scratch dir:
//!
//! 1. **The checker** — compare this build's version against the newest
//!    GitHub release. Versions are CalVer tags `YYYY-MM-DD.N`; the Cargo
//!    version carries only the date (`2026.10.10`), so release builds embed
//!    the exact tag at compile time (`MORDENT_BUILD_TAG`, set in
//!    `.github/workflows/release.yml`) and fall back to the Cargo date with
//!    counter `0` when it is absent. A build with counter `0` therefore sorts
//!    *older* than any published `.N` tag of the same day.
//!    At most one unauthenticated `GET /releases/latest` per 24 h
//!    ([`should_check`]); a 403 (rate limit), a transport failure (offline) or
//!    a 404 (no releases yet) are all silent.
//!
//! 2. **The self-updater** — download the release asset for this platform,
//!    verify it against the release's `SHA256SUMS` asset (integrity, **not**
//!    authenticity: nothing here is signed), extract it, and *stage* it. The
//!    swap happens at the **next launch** ([`apply_pending`]), never mid-playback
//!    and never silently: the user presses a button first. Every replaced file
//!    is moved to a backup directory first, so a failed swap is recoverable by
//!    hand. Runtime-only directories (`rootfs/data` = the helper's Apple login
//!    state, `rootfs/dev`, `rootfs/proc` = mount points) are never touched.
//!
//! 3. **The helper payload check** — is the helper installed under
//!    `/opt/mordent-wrapper` built from the same payload as the bundle next to
//!    this binary? There is no version string anywhere in wrapper-lite
//!    (`/status` reports only `regions`), so the identity is a **SHA-256
//!    manifest hash** over the payload files (`wrapper-lite` +
//!    `rootfs/system/**`). `scripts/install-wrapper.sh` records it at install
//!    time in `<prefix>/payload-version`; when that file is missing (an older
//!    install) the same hash is computed by walking the tree, which is why the
//!    shell and Rust definitions of the manifest must agree exactly — see
//!    [`payload_manifest`].
//!
//! Nothing here runs on the UI thread: every entry point is blocking and is
//! called from a `std::thread` by the UI.

use anyhow::{Context, Result, anyhow, bail};
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Human-facing release page (shown when Mordent cannot update itself).
pub const RELEASE_PAGE: &str = "https://github.com/mordentplayer/mordent/releases";
/// `GET` target: the newest non-draft, non-prerelease release.
pub const LATEST_API: &str = "https://api.github.com/repos/mordentplayer/mordent/releases/latest";
/// Minimum seconds between two automatic checks (the "once a day" privacy line
/// in the README). Manual checks bypass it.
pub const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
/// The checksum asset published next to every release binary.
pub const SUMS_ASSET: &str = "SHA256SUMS";
/// `User-Agent` for the unauthenticated GitHub calls (the API rejects requests
/// without one).
const UA: &str = "mordent-update-check (+https://github.com/mordentplayer/mordent)";

// ---------------------------------------------------------------------------
// 1. Version identity
// ---------------------------------------------------------------------------

/// A release tag: `YYYY-MM-DD.N`, `N` = 1-based counter within that day.
///
/// `Ord` compares the date first, then the counter, so `2026-10-10.3` sorts
/// above `2026-10-10.2` and above `2026-10-10.0` (the date-only Cargo form).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tag {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    /// Counter within the day. `0` means "no counter known" — the shape the
    /// Cargo version has.
    pub n: u16,
}

impl Tag {
    fn from_parts(year: &str, month: &str, day: &str, n: Option<&str>) -> Option<Self> {
        if year.len() != 4 || month.len() > 2 || day.len() > 2 || month.is_empty() || day.is_empty()
        {
            return None;
        }
        let year: u16 = year.parse().ok()?;
        let month: u8 = month.parse().ok()?;
        let day: u8 = day.parse().ok()?;
        let n: u16 = match n {
            Some(n) if !n.is_empty() => n.parse().ok()?,
            _ => 0,
        };
        if !(2000..=2999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        Some(Self { year, month, day, n })
    }

    /// Parse a release tag: `YYYY-MM-DD` or `YYYY-MM-DD.N` (strict: nothing
    /// else is accepted, so a branch name like `main` or a ref like
    /// `refs/tags/…` can never masquerade as a version).
    pub fn parse_release(s: &str) -> Option<Self> {
        let s = s.trim();
        let (date, n) = match s.split_once('.') {
            Some((date, n)) => (date, Some(n)),
            None => (s, None),
        };
        let mut it = date.split('-');
        let (y, m, d) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        Self::from_parts(y, m, d, n)
    }

    /// Parse the Cargo version form `YYYY.MM.DD` (no counter).
    pub fn parse_cargo(s: &str) -> Option<Self> {
        let s = s.trim();
        let mut it = s.split('.');
        let (y, m, d) = (it.next()?, it.next()?, it.next()?);
        if it.next().is_some() {
            return None;
        }
        Self::from_parts(y, m, d, None)
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)?;
        if self.n > 0 {
            write!(f, ".{}", self.n)?;
        }
        Ok(())
    }
}

/// This build's version: the tag embedded by the release workflow, plus the
/// Cargo version it falls back to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// `MORDENT_BUILD_TAG` as compiled in, when it parsed as a release tag.
    pub embedded: Option<Tag>,
    /// `CARGO_PKG_VERSION` (CalVer date, dotted).
    pub cargo: &'static str,
}

impl Version {
    /// What the UI shows: the exact tag for a release build, else the Cargo
    /// version clearly marked as untagged.
    pub fn display(&self) -> String {
        match self.embedded {
            Some(t) => t.to_string(),
            None => format!("{} (no release tag)", self.cargo),
        }
    }

    /// The value compared against GitHub. `None` when neither source parses —
    /// then the checker reports the latest release but never claims this build
    /// is out of date.
    pub fn compare(&self) -> Option<Tag> {
        self.embedded
            .or_else(|| Tag::parse_cargo(self.cargo))
    }
}

/// The running build's version. `MORDENT_BUILD_TAG` is set by the release
/// workflow from the pushed tag; a test build (`test`) or a plain
/// `cargo build` has no tag and falls back to the Cargo version.
pub fn version() -> Version {
    Version {
        embedded: option_env!("MORDENT_BUILD_TAG").and_then(Tag::parse_release),
        cargo: env!("CARGO_PKG_VERSION"),
    }
}

/// Cached result of the last successful check (persisted in `state.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cached {
    /// Unix seconds of the last completed check (success *or* silent failure).
    pub last_check: Option<u64>,
    /// `tag_name` of the release seen then.
    pub last_seen_tag: Option<String>,
}

/// May a check run now? `force` is the user's "Check now" button, which
/// bypasses both the interval and the `check_updates` preference.
///
/// Clock skew (a `last_check` in the future, e.g. after a timezone/clock fix)
/// counts as "just checked": never hammer the API because the clock moved.
pub fn should_check(cached: &Cached, enabled: bool, now: u64, force: bool) -> bool {
    if force {
        return true;
    }
    if !enabled {
        return false;
    }
    match cached.last_check {
        None => true,
        // A `last_check` in the future (the clock moved back) counts as "just
        // checked": never hammer the API because the clock changed.
        Some(t) => t < now && now - t >= CHECK_INTERVAL_SECS,
    }
}

/// The size of the interval, for the "checked N hours ago" line.
pub fn age_secs(last_check: u64, now: u64) -> u64 {
    now.saturating_sub(last_check)
}

/// Human "last checked" text (`None` = never).
pub fn last_check_label(last_check: Option<u64>, now: u64) -> String {
    let Some(t) = last_check else {
        return "never checked".into();
    };
    let age = age_secs(t, now);
    match age {
        0..=59 => "checked just now".into(),
        60..=3599 => format!("checked {} min ago", age / 60),
        3600..=86_399 => format!("checked {} h ago", age / 3600),
        _ => format!("checked {} d ago", age / 86_400),
    }
}

// ---------------------------------------------------------------------------
// 2. The GitHub release lookup
// ---------------------------------------------------------------------------

/// One asset of a release.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// The fields of `GET /releases/latest` this app cares about.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Release {
    #[serde(default)]
    pub tag_name: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// Outcome of one check, ready to render.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// Newest release is not newer than this build.
    UpToDate { latest: Tag },
    /// A newer release exists (assets carried along for the self-updater).
    Newer {
        latest: Tag,
        html_url: String,
        assets: Vec<Asset>,
    },
    /// The latest release exists but this build has no comparable version
    /// (neither `MORDENT_BUILD_TAG` nor the Cargo version parsed).
    Uncomparable { latest: Tag },
    /// Network/API failure, with a human-readable reason. The UI shows this
    /// only for a check the user asked for.
    Failed { reason: String },
}

/// Agent for the small API calls (check + checksum file): short timeouts, so a
/// black-holed network never pins a thread.
pub fn api_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .user_agent(UA)
        .build()
}

/// Agent for release assets (tens of MB): generous overall deadline, same
/// connect timeout.
pub fn asset_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(30 * 60))
        .user_agent(UA)
        .build()
}

/// `GET /releases/latest`. Errors carry a short reason suitable for the UI
/// ("GitHub API HTTP 403 (rate limit)", "no releases published yet", …).
pub fn fetch_latest(agent: &ureq::Agent) -> Result<Release> {
    let resp = match agent
        .get(LATEST_API)
        .set("Accept", "application/vnd.github+json")
        .call()
    {
        Ok(r) => r,
        Err(ureq::Error::Status(403, _)) => bail!("GitHub API rate limit (HTTP 403)"),
        Err(ureq::Error::Status(404, _)) => bail!("no releases published yet (HTTP 404)"),
        Err(ureq::Error::Status(code, _)) => bail!("GitHub API HTTP {code}"),
        Err(e) => bail!("{e}"),
    };
    resp.into_json::<Release>()
        .context("parsing the GitHub release JSON")
}

/// One full check: fetch the newest release, parse its tag, compare.
pub fn check(agent: &ureq::Agent) -> Check {
    let rel = match fetch_latest(agent) {
        Ok(r) => r,
        Err(e) => {
            return Check::Failed {
                reason: format!("{e:#}"),
            };
        }
    };
    let latest = match Tag::parse_release(&rel.tag_name) {
        Some(t) => t,
        None => {
            return Check::Failed {
                reason: format!("unexpected release tag {:?}", rel.tag_name),
            };
        }
    };
    match version().compare() {
        None => Check::Uncomparable { latest },
        Some(current) if latest > current => Check::Newer {
            latest,
            html_url: rel.html_url,
            assets: rel.assets,
        },
        Some(_) => Check::UpToDate { latest },
    }
}

/// `GET` a small text asset (the `SHA256SUMS` file).
pub fn fetch_text(agent: &ureq::Agent, url: &str) -> Result<String> {
    agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?
        .into_string()
        .context("reading the response body")
}

/// One check plus — when a newer release exists — its parsed `SHA256SUMS`,
/// fetched here so the UI can stage an update without a second round trip.
/// A missing/unreadable checksum asset is reported as `None`, and the
/// self-updater then refuses to install anything (see [`plan`]).
pub fn check_with_sums(agent: &ureq::Agent) -> (Check, Option<Vec<(String, String)>>) {
    let check = check(agent);
    let sums = match &check {
        Check::Newer { assets, .. } => sums_asset(assets)
            .and_then(|a| fetch_text(agent, &a.browser_download_url).ok())
            .map(|text| parse_sums(&text)),
        _ => None,
    };
    (check, sums)
}

// ---------------------------------------------------------------------------
// 3. Platform identity + asset selection
// ---------------------------------------------------------------------------

/// How this copy of Mordent was installed — i.e. what a self-update can
/// honestly do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Unpacked release tarball (`mordent` + `scripts/` + payload next to it).
    /// The app, the scripts and the payload are plain files in one directory,
    /// so all of them can be swapped in place.
    Bundle,
    /// Running from an AppImage (`$APPIMAGE`): the image file itself can be
    /// replaced (a running AppImage is mounted from an unlinked-able file).
    AppImage,
    /// Running from `Mordent.app` on macOS: the bundle directory can be
    /// replaced (macOS allows unlinking a running executable).
    MacApp,
    /// Windows zip install: a running `.exe` cannot be overwritten, so this
    /// platform gets download + open-the-folder, not a swap.
    WindowsZip,
    /// A source build or an unrecognised layout: check only.
    Unknown,
}

impl Kind {
    /// Can this layout swap its own files at the next launch?
    pub fn supports_self_update(self) -> bool {
        matches!(self, Kind::Bundle | Kind::AppImage | Kind::MacApp)
    }

    /// Short label for the UI.
    pub fn label(self) -> &'static str {
        match self {
            Kind::Bundle => "tarball install",
            Kind::AppImage => "AppImage",
            Kind::MacApp => "Mordent.app",
            Kind::WindowsZip => "Windows zip",
            Kind::Unknown => "unrecognised install",
        }
    }

    /// The release asset this layout updates from.
    ///
    /// `os`/`arch` are `std::env::consts::{OS,ARCH}` at the call site — passed
    /// explicitly so every platform's mapping is unit-tested on any host.
    pub fn asset_name(self, os: &str, arch: &str, tag: &str) -> Option<String> {
        match self {
            Kind::Bundle => match os {
                "linux" => Some(format!("mordent-{tag}-linux-x86_64.tar.gz")),
                "macos" => Some(format!("mordent-{tag}-macos-{arch}.tar.gz")),
                _ => None,
            },
            Kind::AppImage => Some(format!("mordent-{tag}-x86_64.AppImage")),
            Kind::MacApp => Some(format!("Mordent-{tag}-macos-{arch}.zip")),
            Kind::WindowsZip => Some(format!("mordent-{tag}-windows-x86_64.zip")),
            Kind::Unknown => None,
        }
    }
}

/// `Mordent.app` bundle containing `exe`, if `exe` sits inside one
/// (`…/Mordent.app/Contents/MacOS/mordent`).
pub fn mac_app_bundle(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?; // …/Contents/MacOS
    let contents = macos.parent()?; // …/Contents
    let app = contents.parent()?; // …/Mordent.app
    (macos.file_name()?.eq_ignore_ascii_case("MacOS")
        && contents.file_name()?.eq_ignore_ascii_case("Contents")
        && app
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("app")))
    .then(|| app.to_path_buf())
}

/// Classify the running install. `exe` is `std::env::current_exe()`.
pub fn kind_of(exe: &Path, appimage: Option<&str>, bundle_dir: Option<&Path>) -> Kind {
    if appimage.is_some_and(|p| !p.is_empty()) {
        return Kind::AppImage;
    }
    if mac_app_bundle(exe).is_some() {
        return Kind::MacApp;
    }
    if bundle_dir.is_some() {
        return Kind::Bundle;
    }
    if cfg!(target_os = "windows") && exe.is_file() {
        return Kind::WindowsZip;
    }
    Kind::Unknown
}

/// Classify the running install (real environment).
pub fn kind() -> Kind {
    let exe = std::env::current_exe().unwrap_or_default();
    let appimage = std::env::var("APPIMAGE").ok();
    let scripts = crate::setup::resolve_scripts_dir();
    let bundle = scripts.as_deref().map(crate::setup::bundle_dir);
    kind_of(&exe, appimage.as_deref(), bundle.as_deref())
}

/// Where a staged update is applied for this install.
///
/// `scripts_dir` is the wizard's resolved `…/scripts` (see
/// `setup::resolve_scripts_dir`), needed for the tarball layout.
pub fn apply_target(exe: &Path, appimage: Option<&str>, scripts_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(img) = appimage.filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(img));
    }
    if let Some(app) = mac_app_bundle(exe) {
        return Some(app);
    }
    if let Some(dir) = scripts_dir {
        return Some(crate::setup::bundle_dir(dir));
    }
    if cfg!(target_os = "windows") {
        return exe.parent().map(Path::to_path_buf);
    }
    None
}

/// The asset of `assets` named `name`.
pub fn select_asset<'a>(assets: &'a [Asset], name: &str) -> Option<&'a Asset> {
    assets.iter().find(|a| a.name == name)
}

/// The `SHA256SUMS` asset of a release, if it was published.
pub fn sums_asset(assets: &[Asset]) -> Option<&Asset> {
    select_asset(assets, SUMS_ASSET)
}

/// Parse a `SHA256SUMS` body into `(lowercase hex, file name)` pairs.
/// Tolerates the `*` binary marker of `sha256sum -b` and blank lines.
pub fn parse_sums(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.split_whitespace();
        let (Some(hex), Some(name)) = (it.next(), it.next()) else {
            continue;
        };
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        out.push((
            hex.to_ascii_lowercase(),
            name.trim_start_matches('*').to_string(),
        ));
    }
    out
}

/// The expected checksum for `name`, from [`parse_sums`] output.
pub fn sum_for<'a>(sums: &'a [(String, String)], name: &str) -> Option<&'a str> {
    sums.iter()
        .find(|(_, n)| n == name)
        .map(|(h, _)| h.as_str())
}

// ---------------------------------------------------------------------------
// 4. SHA-256 (no new dependencies; verified against the NIST vectors)
// ---------------------------------------------------------------------------

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Streaming SHA-256. Small and dependency-free on purpose: the only hashing
/// this app needs is checking a download and identifying the helper payload.
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
                0x5be0cd19,
            ],
            buf: [0; 64],
            buf_len: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let want = 64 - self.buf_len;
            let take = want.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let (block, rest) = data.split_at(64);
            let mut b = [0u8; 64];
            b.copy_from_slice(block);
            self.compress(&b);
            data = rest;
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (i, chunk) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h) = (
            self.state[0],
            self.state[1],
            self.state[2],
            self.state[3],
            self.state[4],
            self.state[5],
            self.state[6],
            self.state[7],
        );
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (s, v) in self
            .state
            .iter_mut()
            .zip([a, b, c, d, e, f, g, h])
        {
            *s = s.wrapping_add(v);
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        // `update` counted the padding byte; drop it from the length again.
        self.total = self.total.wrapping_sub(1);
        while self.buf_len != 56 {
            self.update(&[0]);
            self.total = self.total.wrapping_sub(1);
        }
        let mut tail = [0u8; 8];
        tail.copy_from_slice(&bits.to_be_bytes());
        self.update(&tail);
        let mut out = [0u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

/// Hex SHA-256 of a byte slice.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&sha256(data))
}

/// Raw SHA-256 of a byte slice.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finish()
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Streaming hex SHA-256 of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finish()))
}

// ---------------------------------------------------------------------------
// 5. Helper payload identity
// ---------------------------------------------------------------------------

/// Is `rel` (slash-separated, relative to the bundle/prefix root) part of the
/// *payload* — i.e. something an update may replace?
///
/// `rootfs/data` holds the helper's Apple login state (never shipped, never
/// touched); `rootfs/dev` and `rootfs/proc` are runtime mount points
/// (wrapper-lite bind-mounts `/dev/urandom` into the former and mounts proc
/// over the latter — copying "into" them would write to the host `/dev`!).
pub fn is_payload_rel(rel: &str) -> bool {
    let rel = rel.trim_start_matches("./");
    const SKIP: [&str; 3] = ["rootfs/data", "rootfs/dev", "rootfs/proc"];
    !SKIP
        .iter()
        .any(|s| rel == *s || rel.starts_with(&format!("{s}/")))
}

/// Lexically sorted files of a helper payload as `(rel, absolute)`.
///
/// Mirrors `find wrapper-lite rootfs/system -type f` in
/// `scripts/install-wrapper.sh`: the launcher is `wrapper-lite` (a file), the
/// rest of the payload is `rootfs/system/**`. Symbolic links are skipped
/// (`-type f` skips them too); neither tree currently has any.
pub fn payload_files(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let launcher = dir.join("wrapper-lite");
    if launcher.is_file() {
        out.push(("wrapper-lite".to_string(), launcher));
    }
    let mut stack = vec![dir.join("rootfs/system")];
    while let Some(d) = stack.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", d.display())),
        };
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let meta = entry.metadata()?;
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                let rel = path
                    .strip_prefix(dir)
                    .with_context(|| format!("{} is not under {}", path.display(), dir.display()))?
                    .to_string_lossy()
                    .replace('\\', "/");
                if is_payload_rel(&rel) {
                    out.push((rel, path));
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The payload **manifest**: one `"<sha256 of the file>  <relative path>\n"`
/// line per payload file, in lexicographic path order. Its SHA-256 is the
/// payload identity.
///
/// `scripts/install-wrapper.sh` builds the identical text with
/// `find … | sort -z | xargs -0 sha256sum | sha256sum`, which is why the format
/// is exactly coreutils' (`two spaces`, `LF`) and the path order is plain
/// bytewise. Keep the two definitions in sync.
pub fn payload_manifest(dir: &Path) -> Result<String> {
    let mut text = String::new();
    for (rel, path) in payload_files(dir)? {
        text.push_str(&sha256_file(&path)?);
        text.push_str("  ");
        text.push_str(&rel);
        text.push('\n');
    }
    Ok(text)
}

/// The payload identity (hex SHA-256 of the manifest text).
pub fn payload_hash(dir: &Path) -> Result<String> {
    Ok(sha256_hex(payload_manifest(dir)?.as_bytes()))
}

/// The recorded payload identity of an installed helper
/// (`<prefix>/payload-version`, written by `install-wrapper.sh`), if present.
pub fn read_payload_version(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("payload-version")).ok()?;
    let line = text.lines().next()?.trim().to_string();
    (line.len() == 64 && line.bytes().all(|b| b.is_ascii_hexdigit())).then(|| line.to_ascii_lowercase())
}

/// How the installed helper compares with the payload shipped in this build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperState {
    /// No helper at the prefix (or no prefix at all) — nothing to update.
    NotInstalled,
    /// Installed payload identity == the bundled one.
    Current,
    /// Different identities: the installed helper predates (or diverges from)
    /// the bundled payload, so re-running the installer is worth offering.
    Outdated { installed: String, bundled: String },
    /// Not comparable (unreadable tree, no bundled payload next to the app).
    Unknown(String),
}

/// Compare `prefix` (the installed helper) with `bundle` (this build's payload)
/// and report the comparison plus the bundled identity.
///
/// `bundled_memo` lets the caller hand in a previously computed bundled hash
/// (the bundle is immutable for a given build, so re-hashing 100+ MB on every
/// launch is pointless — persist [`HelperCheck::bundled`]).
///
/// Never fails: an unreadable tree becomes [`HelperState::Unknown`], because
/// this only feeds one line of the UI.
pub fn helper_check(
    bundle: Option<&Path>,
    prefix: &Path,
    bundled_memo: Option<&str>,
) -> HelperCheck {
    let Some(bundle) = bundle else {
        return HelperCheck {
            state: if prefix.join("wrapper-lite").is_file() {
                HelperState::Unknown("no helper payload next to this build".into())
            } else {
                HelperState::NotInstalled
            },
            bundled: String::new(),
        };
    };
    let bundled = match bundled_memo {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => match read_payload_version(bundle) {
            Some(v) => v,
            None => match payload_hash(bundle) {
                Ok(h) => h,
                Err(e) => {
                    return HelperCheck {
                        state: HelperState::Unknown(format!("no readable bundled payload ({e:#})")),
                        bundled: String::new(),
                    };
                }
            },
        },
    };
    if !prefix.join("wrapper-lite").is_file() {
        return HelperCheck {
            state: HelperState::NotInstalled,
            bundled,
        };
    }
    let state = match read_payload_version(prefix) {
        Some(v) => {
            if v == bundled {
                HelperState::Current
            } else {
                HelperState::Outdated { installed: v, bundled: bundled.clone() }
            }
        }
        None => match payload_hash(prefix) {
            Ok(h) if h == bundled => HelperState::Current,
            Ok(h) => HelperState::Outdated { installed: h, bundled: bundled.clone() },
            Err(e) => HelperState::Unknown(format!(
                "cannot identify the installed helper ({e:#}) — reinstall to make it comparable"
            )),
        },
    };
    HelperCheck { state, bundled }
}

/// The result of [`helper_check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperCheck {
    pub state: HelperState,
    /// Identity of this build's bundled payload; empty when there is none to
    /// read. Memoise it — computing it hashes the whole payload.
    pub bundled: String,
}

// ---------------------------------------------------------------------------
// 6. Download / verify / stage / apply
// ---------------------------------------------------------------------------

/// One release asset chosen for this install, with its expected checksum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub kind: Kind,
    pub tag: Tag,
    pub asset: Asset,
    /// Expected SHA-256 (from `SHA256SUMS`); always present — a release
    /// without the checksum asset is not updateable.
    pub sha256: String,
    /// Where the swap will happen (the install dir / the AppImage / the .app).
    pub target: PathBuf,
}

/// Build the update plan for `tag`, or explain why this install cannot update
/// itself from it.
pub fn plan(
    kind: Kind,
    os: &str,
    arch: &str,
    target: Option<PathBuf>,
    tag: Tag,
    assets: &[Asset],
    sums: &[(String, String)],
) -> Result<Plan> {
    let name = kind.asset_name(os, arch, &tag.to_string()).ok_or_else(|| {
        anyhow!(
            "this build's install layout ({}) has no release asset",
            kind.label()
        )
    })?;
    let asset = select_asset(assets, &name)
        .cloned()
        .ok_or_else(|| anyhow!("release {tag} has no asset named {name}"))?;
    let sha256 = sum_for(sums, &name)
        .ok_or_else(|| {
            anyhow!("release {tag} has no checksum for {name} (nothing is installed without one)")
        })?
        .to_string();
    let target = target
        .or_else(|| {
            let exe = std::env::current_exe().unwrap_or_default();
            let appimage = std::env::var("APPIMAGE").ok();
            let scripts = crate::setup::resolve_scripts_dir();
            apply_target(&exe, appimage.as_deref(), scripts.as_deref())
        })
        .or_else(|| (kind == Kind::WindowsZip).then(|| PathBuf::from(".")))
        .ok_or_else(|| anyhow!("cannot tell where this build lives — download manually"))?;
    // Follow symlinks once, here: swapping a symlinked AppImage must replace
    // the image itself, not the link.
    let target = std::fs::canonicalize(&target).unwrap_or(target);
    Ok(Plan {
        kind,
        tag,
        asset,
        sha256,
        target,
    })
}

/// A downloaded, verified, extracted update waiting for the next launch.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Pending {
    pub tag: String,
    /// `"bundle" | "appimage" | "macapp"` (see [`Kind`]).
    pub kind: String,
    pub asset: String,
    /// The downloaded archive (kept for the AppImage swap).
    pub archive: String,
    /// Extracted payload: the bundle root, or `Mordent.app`.
    pub content: String,
    /// What the swap replaces.
    pub target: String,
}

impl Pending {
    pub fn kind(&self) -> Kind {
        match self.kind.as_str() {
            "bundle" => Kind::Bundle,
            "appimage" => Kind::AppImage,
            "macapp" => Kind::MacApp,
            _ => Kind::Unknown,
        }
    }
}

fn kind_slug(kind: Kind) -> &'static str {
    match kind {
        Kind::Bundle => "bundle",
        Kind::AppImage => "appimage",
        Kind::MacApp => "macapp",
        _ => "unknown",
    }
}

/// `<cache>/update` — download scratch, the pending marker and the backups.
pub fn update_dir() -> PathBuf {
    crate::config::default_cache_dir().join("update")
}

fn pending_path() -> PathBuf {
    update_dir().join("pending.toml")
}

/// Record a staged update for the next launch (atomic write).
pub fn write_pending(p: &Pending) -> Result<()> {
    let dir = update_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let text = toml::to_string(p).context("serialising the pending update")?;
    let tmp = dir.join("pending.toml.tmp");
    std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, pending_path()).context("renaming the pending marker")?;
    Ok(())
}

/// The staged update recorded for the next launch, if any.
pub fn read_pending() -> Option<Pending> {
    let text = std::fs::read_to_string(pending_path()).ok()?;
    toml::from_str(&text).ok()
}

/// Forget the pending marker (after a successful apply, or on refusal).
pub fn clear_pending() {
    let _ = std::fs::remove_file(pending_path());
}

/// `dest` with `.part` appended (`foo.tar.gz` → `foo.tar.gz.part`; never
/// replace an extension, so two assets can never share a scratch name).
fn part_path(dest: &Path) -> PathBuf {
    let mut name = dest
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_else(|| "download".into());
    name.push(".part");
    dest.with_file_name(name)
}

/// Download `url` to `dest` (via `dest.part`), verifying the SHA-256 of the
/// bytes against `expected`. A mismatch deletes the file and fails: nothing
/// unverified is ever left on disk under the final name.
pub fn download_verified(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    expected: &str,
    log: &mut dyn FnMut(String),
) -> Result<()> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let part = part_path(dest);
    log(format!("downloading {}", dest.file_name().unwrap_or_default().to_string_lossy()));
    let resp = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut reader = resp.into_reader();
    let mut file = std::fs::File::create(&part)
        .with_context(|| format!("creating {}", part.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading the response body of {url}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", part.display()))?;
        total += n as u64;
    }
    file.sync_all().ok();
    drop(file);
    let got = hex(&hasher.finish());
    if !got.eq_ignore_ascii_case(expected) {
        let _ = std::fs::remove_file(&part);
        bail!("checksum mismatch for {} (expected {expected}, got {got})", dest.display());
    }
    std::fs::rename(&part, dest).with_context(|| format!("renaming {}", part.display()))?;
    log(format!("verified {} ({} KiB)", dest.file_name().unwrap_or_default().to_string_lossy(), total / 1024));
    Ok(())
}

/// Extract `archive` into `dest` with the system `tar`.
///
/// BSdtar (macOS, Windows 10+, and the default on Linux) reads both `.tar.gz`
/// and `.zip`, so one invocation covers every release asset and no
/// decompression crate is needed. The archive has already been checksum-
/// verified, and the caller still sanity-checks the extracted layout.
pub fn extract(archive: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let out = std::process::Command::new("tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .output()
        .with_context(|| format!("running tar on {}", archive.display()))?;
    if !out.status.success() {
        bail!(
            "tar failed on {}: {}",
            archive.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Download, verify, extract and stage `plan`; on success the swap is recorded
/// for the next launch.
///
/// `log` receives one line per step (shown in the UI).
pub fn stage(plan: &Plan, log: &mut dyn FnMut(String)) -> Result<Pending> {
    if !plan.kind.supports_self_update() {
        bail!("{} installs cannot update themselves", plan.kind.label());
    }
    let work = update_dir().join(plan.tag.to_string());
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;

    let archive = work.join(&plan.asset.name);
    download_verified(&asset_agent(), &plan.asset.browser_download_url, &archive, &plan.sha256, log)?;

    let content = match plan.kind {
        Kind::AppImage => archive.clone(),
        Kind::Bundle => {
            let x = work.join("x");
            extract(&archive, &x)?;
            let root = if x.join("mordent").is_file() {
                x.clone()
            } else {
                // The tarball wraps everything in `mordent-<tag>-<os>-<arch>/`.
                let mut found = None;
                for entry in std::fs::read_dir(&x)? {
                    let entry = entry?;
                    if entry.file_type()?.is_dir() && entry.path().join("mordent").is_file() {
                        found = Some(entry.path());
                        break;
                    }
                }
                found.ok_or_else(|| anyhow!("archive has no mordent binary — refusing to stage"))?
            };
            log(format!("extracted {}", root.display()));
            root
        }
        Kind::MacApp => {
            let x = work.join("x");
            extract(&archive, &x)?;
            let app = x.join("Mordent.app");
            if !app.join("Contents/MacOS/mordent").is_file() {
                bail!("archive has no Mordent.app bundle — refusing to stage");
            }
            app
        }
        _ => bail!("{} installs cannot update themselves", plan.kind.label()),
    };

    let pending = Pending {
        tag: plan.tag.to_string(),
        kind: kind_slug(plan.kind).to_string(),
        asset: plan.asset.name.clone(),
        archive: archive.display().to_string(),
        content: content.display().to_string(),
        target: plan.target.display().to_string(),
    };
    write_pending(&pending)?;
    log(format!("staged {} for the next launch", plan.tag));
    Ok(pending)
}

/// Move `path` aside into `backup_dir/rel`, creating parents. Missing files
/// are fine (nothing to back up).
fn backup_then(backup_dir: &Path, rel: &str, path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let dest = backup_dir.join(rel);
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(&dest);
    std::fs::rename(path, &dest).with_context(|| {
        format!("backing up {} -> {}", path.display(), dest.display())
    })?;
    Ok(())
}

/// Copy `from` over `to` (parents created), then restore the executable bits
/// the archive carried.
fn install_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(to);
    std::fs::copy(from, to).with_context(|| format!("copying {} -> {}", from.display(), to.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(from)?.permissions().mode();
        std::fs::set_permissions(to, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Swap a staged tarball bundle (`content`) into the install directory
/// (`target`): every payload file, with the replaced original moved to
/// `backup/<tag>/…` first. Runtime directories (`rootfs/data`, `rootfs/dev`,
/// `rootfs/proc`) are never touched.
fn apply_bundle(content: &Path, target: &Path, tag: &str, log: &mut Vec<String>) -> Result<()> {
    let backup = update_dir().join(format!("backup-{tag}"));
    let files = payload_bundle_files(content)?;
    if files.is_empty() {
        bail!("staged bundle has no files to install");
    }
    std::fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    for (rel, from) in files {
        if !is_payload_rel(&rel) {
            continue;
        }
        let to = target.join(&rel);
        backup_then(&backup, &rel, &to)?;
        install_file(&from, &to)?;
        log.push(format!("installed {rel}"));
    }
    Ok(())
}

/// Files of an extracted *bundle* (`mordent`, `wrapper-lite`, `rootfs/**`,
/// `scripts/**`, docs), minus the runtime-only subtrees.
fn payload_bundle_files(root: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).with_context(|| format!("reading {}", d.display()))? {
            let entry = entry?;
            let path = entry.path();
            let meta = entry.metadata()?;
            let rel = path
                .strip_prefix(root)
                .with_context(|| format!("{} is not under {}", path.display(), root.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            if !is_payload_rel(&rel) {
                continue;
            }
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                out.push((rel, path));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Apply the staged update: swap the AppImage file, the `Mordent.app` bundle,
/// or every file of the tarball bundle. Called once, at startup, by
/// `main::run` — before any window exists.
pub fn apply(p: &Pending, log: &mut Vec<String>) -> Result<()> {
    let content = PathBuf::from(&p.content);
    let target = PathBuf::from(&p.target);
    match p.kind() {
        Kind::AppImage => {
            let archive = PathBuf::from(&p.archive);
            if !archive.is_file() {
                bail!("staged AppImage {} is gone", archive.display());
            }
            let bak = appimage_backup_path(&target);
            let _ = std::fs::remove_file(&bak);
            if target.exists() {
                std::fs::rename(&target, &bak)
                    .with_context(|| format!("moving {} aside", target.display()))?;
            }
            if let Err(e) = std::fs::rename(&archive, &target) {
                // Roll back: never leave the user without the image.
                if bak.exists() {
                    let _ = std::fs::rename(&bak, &target);
                }
                bail!(
                    "replacing {} failed ({e}); the previous image was restored",
                    target.display()
                );
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
            }
            log.push(format!("replaced {}", target.display()));
        }
        Kind::MacApp => {
            if !content.join("Contents/MacOS/mordent").is_file() {
                bail!("staged app bundle is gone ({})", content.display());
            }
            let bak = sibling_backup_path(&target, &p.tag);
            let _ = std::fs::remove_dir_all(&bak);
            std::fs::rename(&target, &bak)
                .with_context(|| format!("moving {} aside", target.display()))?;
            if let Err(e) = std::fs::rename(&content, &target) {
                // Roll back so the app is never left without a bundle.
                let _ = std::fs::rename(&bak, &target);
                return Err(anyhow!(
                    "installing {} failed ({}); the previous bundle was restored",
                    target.display(),
                    e
                ));
            }
            log.push(format!("replaced {} (previous kept at {})", target.display(), bak.display()));
        }
        Kind::Bundle => apply_bundle(&content, &target, &p.tag, log)?,
        _ => bail!("this install cannot update itself"),
    }
    clear_pending();
    // The staged copy is no longer needed (the AppImage/app bundles were moved
    // out of it, the tarball's files were copied). Failures keep it for a retry.
    let _ = std::fs::remove_dir_all(update_dir().join(&p.tag));
    Ok(())
}

/// `<image>.bak-<tag>` next to an AppImage.
fn appimage_backup_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mordent.AppImage".into());
    name.push_str(".bak");
    target.with_file_name(name)
}

/// `<dir>.bak-<tag>` next to a bundle directory.
fn sibling_backup_path(target: &Path, tag: &str) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Mordent.app".into());
    name.push_str(&format!(".bak-{tag}"));
    target.with_file_name(name)
}

/// Download + verify + unpack a release asset **without** staging a swap.
///
/// This is what the platforms that cannot replace their own files get (Windows:
/// a running `.exe` is locked; an unrecognised layout: nothing to replace):
/// the checksum-verified files, unpacked under the cache, ready for the user to
/// move into place. Returns the unpacked directory.
pub fn fetch_for_manual(plan: &Plan, log: &mut dyn FnMut(String)) -> Result<PathBuf> {
    let work = update_dir().join(format!("manual-{}", plan.tag));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    let archive = work.join(&plan.asset.name);
    download_verified(
        &asset_agent(),
        &plan.asset.browser_download_url,
        &archive,
        &plan.sha256,
        log,
    )?;
    if plan.kind == Kind::Unknown {
        return Ok(work);
    }
    let unpacked = work.join("unpacked");
    extract(&archive, &unpacked)?;
    log(format!("unpacked to {}", unpacked.display()));
    Ok(unpacked)
}

/// Show `path` in the desktop's file manager (best effort; the child is reaped
/// off-thread so a slow file manager never holds up a frame).
pub fn reveal(path: &Path) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(program)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("running {program}"))?;
    std::thread::Builder::new()
        .name("mordent-reveal".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .ok();
    Ok(())
}

/// Apply the update staged for this launch, if any. Returns the tag applied.
///
/// Failure is not fatal: the marker is renamed to `pending.failed.toml` (so a
/// broken download cannot retry forever) and the launch continues on the
/// current files.
pub fn apply_pending(log: &mut Vec<String>) -> Result<Option<String>> {
    let Some(pending) = read_pending() else {
        return Ok(None);
    };
    log.push(format!("applying the staged update to {}", pending.tag));
    match apply(&pending, log) {
        Ok(()) => {
            mark_applied(&pending.tag);
            Ok(Some(pending.tag))
        }
        Err(e) => {
            let _ = std::fs::rename(pending_path(), update_dir().join("pending.failed.toml"));
            Err(e)
        }
    }
}

/// `<cache>/update/applied` — a one-shot note for the UI ("we just updated").
fn applied_path() -> PathBuf {
    update_dir().join("applied")
}

fn mark_applied(tag: &str) {
    let _ = std::fs::write(applied_path(), format!("{tag}\n"));
}

/// Read *and consume* the "an update was just applied" note.
pub fn take_applied() -> Option<String> {
    let text = std::fs::read_to_string(applied_path()).ok()?;
    let _ = std::fs::remove_file(applied_path());
    let tag = text.trim().to_string();
    (!tag.is_empty()).then_some(tag)
}

// ---------------------------------------------------------------------------
// 7. Helper (wrapper payload) install refresh
// ---------------------------------------------------------------------------

/// Progress of a pkexec helper reinstall driven from the running app.
#[derive(Debug, Clone)]
pub enum HelperEvent {
    Line(String),
    Done(Result<(), String>),
}

/// Re-run the shipped installer with this build's payload, through pkexec (so
/// the desktop shows its authentication dialog). The argv comes from
/// `setup::install_spec`, i.e. the wizard's own install command — the script
/// is idempotent (stops the service, replaces the payload, restarts it and
/// waits for `/status`).
pub fn spawn_helper_install(
    scripts_dir: &Path,
    port: u16,
    tx: std::sync::mpsc::Sender<HelperEvent>,
) -> Result<(), String> {
    let spec = crate::setup::install_spec(scripts_dir, port);
    let mut cmd = std::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("could not run {} ({e})", spec.program))?;
    let mut pumps = Vec::new();
    if let Some(out) = child.stdout.take() {
        pumps.push(pump_lines(out, tx.clone()));
    }
    if let Some(err) = child.stderr.take() {
        pumps.push(pump_lines(err, tx.clone()));
    }
    std::thread::Builder::new()
        .name("mordent-helper-update".into())
        .spawn(move || {
            let status = child.wait();
            for p in pumps {
                let _ = p.join();
            }
            let ev = match status {
                Ok(s) if s.success() => HelperEvent::Done(Ok(())),
                Ok(s) => HelperEvent::Done(Err(format!(
                    "the installer exited with {}",
                    s.code().map_or("a signal".to_string(), |c| format!("status {c}"))
                ))),
                Err(e) => HelperEvent::Done(Err(format!("waiting for the installer failed: {e}"))),
            };
            let _ = tx.send(ev);
        })
        .map_err(|e| format!("could not start the installer thread ({e})"))?;
    Ok(())
}

fn pump_lines<R: Read + Send + 'static>(
    r: R,
    tx: std::sync::mpsc::Sender<HelperEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(r).lines() {
            let Ok(line) = line else { break };
            if tx.send(HelperEvent::Line(line)).is_err() {
                break;
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mordent-update-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // ---- tags ----

    #[test]
    fn release_tags_parse_and_order() {
        let a = Tag::parse_release("2026-10-10.3").unwrap();
        assert_eq!(a, Tag { year: 2026, month: 10, day: 10, n: 3 });
        assert_eq!(a.to_string(), "2026-10-10.3");
        let b = Tag::parse_release("2026-10-10.12").unwrap();
        assert!(b > a, "counter 12 is newer than 3");
        let d = Tag::parse_release("2026-10-11.1").unwrap();
        assert!(d > b, "the next day wins over a big counter");
        let no = Tag::parse_release("2026-10-10").unwrap();
        assert!(no.n == 0 && no < a, "a date-only build is older than .1");
    }

    #[test]
    fn cargo_version_is_the_date_only_tag() {
        let c = Tag::parse_cargo("2026.10.10").unwrap();
        assert_eq!(c, Tag { year: 2026, month: 10, day: 10, n: 0 });
        assert_eq!(c.to_string(), "2026-10-10");
    }

    #[test]
    fn junk_is_not_a_version() {
        for s in [
            "", "test", "main", "v1.2.3", "refs/tags/2026-10-10.1", "2026-10-10.1-rc1",
            "2026-13-01.1", "2026-10-32.1", "26-10-10.1", "2026-10-10.x", "latest",
        ] {
            assert!(Tag::parse_release(s).is_none(), "{s:?} must not parse as a tag");
        }
        assert!(Tag::parse_cargo("2026.10").is_none());
        assert!(Tag::parse_cargo("1.2.3").is_none());
    }

    #[test]
    fn embedded_tag_wins_over_cargo_and_falls_back() {
        let v = Version { embedded: Tag::parse_release("2026-10-10.3"), cargo: "2026.10.10" };
        assert_eq!(v.display(), "2026-10-10.3");
        assert_eq!(v.compare(), Tag::parse_release("2026-10-10.3"));
        let v = Version { embedded: None, cargo: "2026.10.10" };
        assert_eq!(v.display(), "2026.10.10 (no release tag)");
        assert_eq!(v.compare(), Tag::parse_release("2026-10-10"));
    }

    #[test]
    fn running_version_has_a_comparable_tag() {
        // The Cargo version is CalVer, so even an untagged build compares.
        let v = version();
        assert!(v.compare().is_some(), "{v:?} must compare");
    }

    // ---- cache policy ----

    #[test]
    fn check_runs_when_never_checked_or_after_a_day() {
        let now = 1_700_000_000;
        assert!(should_check(&Cached::default(), true, now, false));
        let fresh = Cached { last_check: Some(now - 60), last_seen_tag: None };
        assert!(!should_check(&fresh, true, now, false), "inside the 24 h window");
        let edge = Cached { last_check: Some(now - CHECK_INTERVAL_SECS), last_seen_tag: None };
        assert!(should_check(&edge, true, now, false), "exactly at the boundary");
        let old = Cached { last_check: Some(now - CHECK_INTERVAL_SECS - 1), last_seen_tag: None };
        assert!(should_check(&old, true, now, false));
    }

    #[test]
    fn check_respects_the_preference_and_the_manual_override() {
        let now = 1_700_000_000;
        let just = Cached { last_check: Some(now), last_seen_tag: None };
        assert!(!should_check(&just, false, now, false), "disabled");
        assert!(should_check(&just, false, now, true), "Check now works while disabled");
        assert!(should_check(&just, true, now, true), "Check now bypasses the interval");
    }

    #[test]
    fn a_future_last_check_never_hammers_the_api() {
        let now = 1_700_000_000;
        let skewed = Cached { last_check: Some(now + 90_000), last_seen_tag: None };
        assert!(!should_check(&skewed, true, now, false));
    }

    #[test]
    fn last_check_label_reads_naturally() {
        let now = 1_700_000_000;
        assert_eq!(last_check_label(None, now), "never checked");
        assert_eq!(last_check_label(Some(now - 5), now), "checked just now");
        assert_eq!(last_check_label(Some(now - 600), now), "checked 10 min ago");
        assert_eq!(last_check_label(Some(now - 7200), now), "checked 2 h ago");
        assert_eq!(last_check_label(Some(now - 3 * 86_400), now), "checked 3 d ago");
    }

    // ---- SHA-256 ----

    #[test]
    fn sha256_matches_the_nist_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // 55/56/64/65 bytes: the padding boundaries.
        assert_eq!(
            sha256_hex(&[b'a'; 55]),
            "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 56]),
            "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"
        );
        assert_eq!(
            sha256_hex(&[b'a'; 64]),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    #[test]
    fn sha256_update_chunks_match_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        let one = sha256(&data);
        let mut h = Sha256::new();
        for chunk in data.chunks(7) {
            h.update(chunk);
        }
        assert_eq!(hex(&h.finish()), hex(&one));
    }

    #[test]
    fn sha256_file_streams() {
        let d = tmp("sha-file");
        let p = d.join("f.bin");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            sha256_file(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(sha256_file(&d.join("nope")).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }

    // ---- checksum file ----

    #[test]
    fn sums_parse_and_lookup() {
        let text = "# comment\n\
            ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad  a.tar.gz\n\
            248D6A61D20638B8E5C026930C3E6039A33CE45964FF2167F6ECEDD419DB06C1 *b.AppImage\n\
            not-a-hash  c\n";
        let sums = parse_sums(text);
        assert_eq!(sums.len(), 2, "{sums:?}");
        assert_eq!(sum_for(&sums, "a.tar.gz"), Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"));
        // upper-case hex is normalised, the `*` binary marker stripped
        assert_eq!(
            sum_for(&sums, "b.AppImage"),
            Some("248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1")
        );
        assert_eq!(sum_for(&sums, "missing"), None);
    }

    // ---- platform mapping ----

    #[test]
    fn asset_names_follow_the_release_layout() {
        let t = "2026-10-11.1";
        assert_eq!(
            Kind::Bundle.asset_name("linux", "x86_64", t).as_deref(),
            Some("mordent-2026-10-11.1-linux-x86_64.tar.gz")
        );
        assert_eq!(
            Kind::Bundle.asset_name("macos", "aarch64", t).as_deref(),
            Some("mordent-2026-10-11.1-macos-aarch64.tar.gz")
        );
        assert_eq!(
            Kind::AppImage.asset_name("linux", "x86_64", t).as_deref(),
            Some("mordent-2026-10-11.1-x86_64.AppImage")
        );
        assert_eq!(
            Kind::MacApp.asset_name("macos", "aarch64", t).as_deref(),
            Some("Mordent-2026-10-11.1-macos-aarch64.zip")
        );
        assert_eq!(
            Kind::WindowsZip.asset_name("windows", "x86_64", t).as_deref(),
            Some("mordent-2026-10-11.1-windows-x86_64.zip")
        );
        assert_eq!(Kind::Unknown.asset_name("linux", "x86_64", t), None);
    }

    #[test]
    fn self_update_is_offered_only_where_it_is_safe() {
        assert!(Kind::Bundle.supports_self_update());
        assert!(Kind::AppImage.supports_self_update());
        assert!(Kind::MacApp.supports_self_update());
        assert!(!Kind::WindowsZip.supports_self_update(), "a running .exe is locked");
        assert!(!Kind::Unknown.supports_self_update());
    }

    #[test]
    fn install_kind_is_detected_from_the_environment() {
        let exe = Path::new("/home/u/mordent-2026-10-10-linux-x86_64/mordent");
        let scripts = Path::new("/home/u/mordent-2026-10-10-linux-x86_64/scripts");
        assert_eq!(kind_of(exe, None, Some(scripts)), Kind::Bundle);
        assert_eq!(kind_of(exe, Some("/home/u/Mordent.AppImage"), Some(scripts)), Kind::AppImage);
        let mac = Path::new("/Applications/Mordent.app/Contents/MacOS/mordent");
        assert_eq!(kind_of(mac, None, None), Kind::MacApp);
        assert_eq!(
            mac_app_bundle(mac),
            Some(PathBuf::from("/Applications/Mordent.app"))
        );
        assert_eq!(mac_app_bundle(exe), None);
        assert_eq!(kind_of(exe, None, None), Kind::Unknown);
    }

    #[test]
    fn apply_targets_point_at_the_install() {
        let exe = Path::new("/home/u/bundle/mordent");
        let scripts = Path::new("/home/u/bundle/scripts");
        assert_eq!(
            apply_target(exe, None, Some(scripts)),
            Some(PathBuf::from("/home/u/bundle"))
        );
        assert_eq!(
            apply_target(exe, Some("/home/u/Mordent.AppImage"), Some(scripts)),
            Some(PathBuf::from("/home/u/Mordent.AppImage"))
        );
        let mac = Path::new("/Applications/Mordent.app/Contents/MacOS/mordent");
        assert_eq!(
            apply_target(mac, None, None),
            Some(PathBuf::from("/Applications/Mordent.app"))
        );
    }

    // ---- plan ----

    #[test]
    fn plan_needs_the_asset_and_its_checksum() {
        let tag = Tag::parse_release("2026-10-11.1").unwrap();
        let assets = vec![
            Asset {
                name: "mordent-2026-10-11.1-linux-x86_64.tar.gz".into(),
                browser_download_url: "https://example.invalid/linux.tar.gz".into(),
                size: 10,
            },
            Asset {
                name: SUMS_ASSET.into(),
                browser_download_url: "https://example.invalid/SHA256SUMS".into(),
                size: 1,
            },
        ];
        let sums = parse_sums(&format!("{}  mordent-2026-10-11.1-linux-x86_64.tar.gz\n", "a".repeat(64)));
        let p = plan(
            Kind::Bundle,
            "linux",
            "x86_64",
            Some(PathBuf::from("/opt/mordent")),
            tag,
            &assets,
            &sums,
        )
        .unwrap();
        assert_eq!(p.target, PathBuf::from("/opt/mordent"));
        assert_eq!(p.sha256, "a".repeat(64));
        assert_eq!(sums_asset(&assets).map(|a| a.name.as_str()), Some(SUMS_ASSET));

        // No checksum published for the asset → refuse (integrity first).
        assert!(
            plan(Kind::Bundle, "linux", "x86_64", None, tag, &assets, &[]).is_err()
        );
        // No matching asset → refuse with a clear message.
        let err = plan(Kind::MacApp, "macos", "aarch64", None, tag, &assets, &sums)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Mordent-2026-10-11.1-macos-aarch64.zip"), "{err}");
        // Unknown layout: nothing to update.
        assert!(plan(Kind::Unknown, "linux", "x86_64", None, tag, &assets, &sums).is_err());
    }

    // ---- payload identity ----

    /// A stand-in helper payload: the launcher, one payload file, and the
    /// login state that must never count.
    fn fake_payload(root: &Path) {
        std::fs::create_dir_all(root.join("rootfs/system/bin")).unwrap();
        std::fs::create_dir_all(root.join("rootfs/data")).unwrap();
        std::fs::write(root.join("wrapper-lite"), b"launcher").unwrap();
        std::fs::write(root.join("rootfs/system/bin/lite"), b"lite").unwrap();
        std::fs::write(root.join("rootfs/data/MUSIC_TOKEN"), b"secret").unwrap();
    }

    #[test]
    fn payload_files_skip_login_state_and_sort() {
        let d = tmp("payload-files");
        fake_payload(&d);
        let files = payload_files(&d).unwrap();
        let rels: Vec<&str> = files.iter().map(|(r, _)| r.as_str()).collect();
        assert_eq!(rels, vec!["rootfs/system/bin/lite", "wrapper-lite"], "{rels:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn payload_manifest_is_the_documented_text() {
        let d = tmp("payload-manifest");
        fake_payload(&d);
        let manifest = payload_manifest(&d).unwrap();
        let launcher = sha256_hex(b"launcher");
        let lite = sha256_hex(b"lite");
        let want = format!("{lite}  rootfs/system/bin/lite\n{launcher}  wrapper-lite\n");
        assert_eq!(manifest, want);
        assert_eq!(payload_hash(&d).unwrap(), sha256_hex(want.as_bytes()));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn payload_hash_ignores_login_state_changes() {
        let d = tmp("payload-stable");
        fake_payload(&d);
        let before = payload_hash(&d).unwrap();
        std::fs::write(d.join("rootfs/data/MUSIC_TOKEN"), b"rotated").unwrap();
        assert_eq!(payload_hash(&d).unwrap(), before);
        std::fs::write(d.join("rootfs/system/bin/lite"), b"updated").unwrap();
        assert_ne!(payload_hash(&d).unwrap(), before);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn payload_version_file_is_read_strictly() {
        let d = tmp("payload-version");
        assert_eq!(read_payload_version(&d), None);
        std::fs::write(d.join("payload-version"), format!("{}\n", "AB".repeat(32))).unwrap();
        assert_eq!(read_payload_version(&d), Some("ab".repeat(32)));
        std::fs::write(d.join("payload-version"), "not a hash\n").unwrap();
        assert_eq!(read_payload_version(&d), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn helper_check_reports_current_outdated_and_absent() {
        let bundle = tmp("helper-bundle");
        let prefix = tmp("helper-prefix");
        fake_payload(&bundle);
        let bundle_hash = payload_hash(&bundle).unwrap();
        let absent = helper_check(Some(&bundle), &prefix, Some(&bundle_hash));
        assert_eq!(absent.state, HelperState::NotInstalled);
        assert_eq!(absent.bundled, bundle_hash, "the identity is still reported");
        fake_payload(&prefix);
        assert_eq!(
            helper_check(Some(&bundle), &prefix, None).state,
            HelperState::Current
        );
        // An install that recorded its identity: compared without hashing.
        std::fs::write(prefix.join("payload-version"), format!("{bundle_hash}\n")).unwrap();
        assert_eq!(
            helper_check(Some(&bundle), &prefix, Some(&bundle_hash)).state,
            HelperState::Current
        );
        // Older install (no payload-version, older payload) → outdated.
        std::fs::remove_file(prefix.join("payload-version")).unwrap();
        std::fs::write(prefix.join("rootfs/system/bin/lite"), b"old").unwrap();
        let rep = helper_check(Some(&bundle), &prefix, None);
        assert_eq!(rep.bundled, bundle_hash);
        match rep.state {
            HelperState::Outdated { bundled, installed } => {
                assert_eq!(bundled, bundle_hash);
                assert_ne!(installed, bundled);
            }
            other => panic!("expected Outdated, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&bundle);
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn helper_check_without_a_bundle_says_so() {
        let prefix = tmp("helper-nobundle");
        assert_eq!(
            helper_check(None, &prefix, None).state,
            HelperState::NotInstalled
        );
        fake_payload(&prefix);
        match helper_check(None, &prefix, None).state {
            HelperState::Unknown(msg) => assert!(msg.contains("no helper payload"), "{msg}"),
            other => panic!("expected Unknown, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&prefix);
    }

    #[test]
    fn payload_rel_guards_the_runtime_dirs() {
        for rel in [
            "rootfs/data",
            "rootfs/data/MUSIC_TOKEN",
            "rootfs/dev",
            "rootfs/dev/urandom",
            "rootfs/proc",
            "rootfs/proc/1",
        ] {
            assert!(!is_payload_rel(rel), "{rel} must never be written");
        }
        for rel in ["mordent", "wrapper-lite", "rootfs/system/bin/lite", "scripts/install-wrapper.sh"] {
            assert!(is_payload_rel(rel), "{rel} is payload");
        }
    }

    // ---- staged swap ----

    /// Stage a fake bundle update by hand (no network) and apply it.
    fn stage_bundle(content: &Path, target: &Path, tag: &str) -> Pending {
        Pending {
            tag: tag.into(),
            kind: "bundle".into(),
            asset: format!("mordent-{tag}-linux-x86_64.tar.gz"),
            archive: content.with_extension("tar.gz").display().to_string(),
            content: content.display().to_string(),
            target: target.display().to_string(),
        }
    }

    #[test]
    fn apply_bundle_swaps_payload_and_keeps_login_state() {
        let root = tmp("apply-bundle");
        let target = root.join("install");
        let content = root.join("new");
        fake_payload(&target);
        fake_payload(&content);
        std::fs::write(target.join("mordent"), b"old app").unwrap();
        std::fs::write(target.join("README.md"), b"old readme").unwrap();
        std::fs::write(content.join("mordent"), b"new app").unwrap();
        std::fs::write(content.join("README.md"), b"new readme").unwrap();
        // A file that only exists in the old install must survive.
        std::fs::write(target.join("rootfs/data/MUSIC_TOKEN"), b"keep me").unwrap();

        let pending = stage_bundle(&content, &target, "2026-10-11.1");
        let mut log = Vec::new();
        apply_bundle(&content, &target, &pending.tag, &mut log).unwrap();

        assert_eq!(std::fs::read(target.join("mordent")).unwrap(), b"new app");
        assert_eq!(std::fs::read(target.join("README.md")).unwrap(), b"new readme");
        assert_eq!(std::fs::read(target.join("wrapper-lite")).unwrap(), b"launcher");
        assert_eq!(
            std::fs::read(target.join("rootfs/data/MUSIC_TOKEN")).unwrap(),
            b"keep me",
            "login state must never be touched"
        );
        // The replaced originals are in the backup dir.
        let backup = update_dir().join("backup-2026-10-11.1");
        assert_eq!(std::fs::read(backup.join("mordent")).unwrap(), b"old app");
        assert_eq!(std::fs::read(backup.join("README.md")).unwrap(), b"old readme");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(update_dir().join("backup-2026-10-11.1"));
    }

    /// `download_verified` against a local one-shot HTTP server (the network
    /// path, without the network): streaming hash, refusal of a bad checksum,
    /// and the `.part` file never surviving a failure.
    #[test]
    fn download_verified_hashes_validates_and_cleans_up() {
        use std::io::{Read as _, Write as _};
        let body = b"mordent payload bytes\n".to_vec();
        let len = body.len();
        let want = sha256_hex(&body);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Serve every request until told to stop: polling `accept` means the
        // helper thread can never outlive the test (a fixed request count would
        // hang here if ureq reused a connection).
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_server = stop.clone();
        let served = body.clone();
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            while !stop_server.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        let mut req = [0u8; 2048];
                        let _ = sock.read(&mut req);
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
                        );
                        let _ = sock.write_all(head.as_bytes());
                        let _ = sock.write_all(&served);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let dir = tmp("download");
        let url = format!("http://127.0.0.1:{port}/asset.bin");
        let agent = api_agent();

        let good = dir.join("good.bin");
        let mut lines = Vec::new();
        download_verified(&agent, &url, &good, &want, &mut |l| lines.push(l))
            .unwrap_or_else(|e| panic!("download failed: {e:#}"));
        assert_eq!(std::fs::read(&good).unwrap(), body);
        assert_eq!(sha256_file(&good).unwrap(), want);
        assert!(lines.iter().any(|l| l.starts_with("verified")), "{lines:?}");
        assert!(!part_path(&good).exists(), "the .part was not renamed away");

        // Upper-case hex (as a SHA256SUMS file may carry it) is accepted; a
        // wrong checksum is refused and leaves nothing behind.
        let upper = want.to_ascii_uppercase();
        let case = dir.join("case.bin");
        download_verified(&agent, &url, &case, &upper, &mut |_| {})
            .unwrap_or_else(|e| panic!("upper-case checksum rejected: {e:#}"));
        let bad = dir.join("bad.bin");
        let err = download_verified(&agent, &url, &bad, &"0".repeat(64), &mut |_| {})
            .unwrap_err()
            .to_string();
        assert!(err.contains("checksum mismatch"), "{err}");
        assert!(!bad.exists(), "a rejected download must not be kept");
        assert!(!part_path(&bad).exists(), "no .part left behind");

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- live network checks (ignored: the suite must run offline) ----
    //
    //   cargo test --locked -- --ignored update:: --nocapture
    //
    // They touch api.github.com and download one *small* published asset
    // (compose.yaml) to exercise the real checksum path. The tarball itself is
    // never fetched here (117 MB); `stage` shares this download code.

    #[ignore = "live network: queries api.github.com/releases/latest"]
    #[test]
    fn live_latest_release_is_newer_than_the_cargo_version() {
        let (check, sums) = check_with_sums(&api_agent());
        match check {
            Check::Newer {
                latest,
                ref html_url,
                ref assets,
            } => {
                println!(
                    "LIVE check: running {} → latest {latest} ({html_url}), {} assets",
                    version().display(),
                    assets.len()
                );
                // The first published release must still be visible: this pins
                // the CalVer parsing against the real tag_name.
                assert!(
                    latest >= Tag::parse_release("2026-10-10.3").unwrap(),
                    "{latest} is older than the first release?"
                );
                let name = Kind::Bundle
                    .asset_name("linux", "x86_64", &latest.to_string())
                    .unwrap();
                let asset = select_asset(assets, &name)
                    .unwrap_or_else(|| panic!("release {latest} has no {name}"));
                println!(
                    "LIVE asset: {} ({} bytes) → {}",
                    name, asset.size, asset.browser_download_url
                );
                match sums.as_ref().and_then(|s| sum_for(s, "compose.yaml")) {
                    Some(sum) => println!("LIVE SHA256SUMS: compose.yaml = {sum}"),
                    None => println!(
                        "LIVE SHA256SUMS: not published for {latest} \
                         (releases before the checksum asset existed) — the self-updater \
                         correctly refuses those"
                    ),
                }
            }
            other => panic!("expected a newer release to exist, got {other:?}"),
        }
    }

    #[ignore = "live network: downloads one small release asset"]
    #[test]
    fn live_download_is_checksum_verified() {
        let agent = api_agent();
        let release = fetch_latest(&agent).expect("the release lookup must work");
        let Some(sums_asset) = sums_asset(&release.assets) else {
            println!(
                "LIVE download: SKIPPED — release {} predates the SHA256SUMS asset",
                release.tag_name
            );
            return;
        };
        let text = fetch_text(&agent, &sums_asset.browser_download_url).expect("SHA256SUMS");
        let sums = parse_sums(&text);
        println!("LIVE download: {} sums in {}", sums.len(), release.tag_name);

        // The smallest published file: proof that download + verify + reject
        // all work against the real release, not just a fixture.
        let name = "compose.yaml";
        let asset = select_asset(&release.assets, name)
            .unwrap_or_else(|| panic!("release {} has no {name}", release.tag_name));
        let want = sum_for(&sums, name).expect("a checksum for compose.yaml");
        let dir = tmp("live-download");
        let dest = dir.join(name);
        let mut lines = Vec::new();
        download_verified(&agent, &asset.browser_download_url, &dest, want, &mut |l| lines.push(l))
            .expect("the published checksum must match the published bytes");
        assert_eq!(sha256_file(&dest).unwrap(), want);
        assert!(std::fs::metadata(&dest).unwrap().len() > 0);

        // …and a wrong checksum is rejected, leaving nothing behind.
        let tampered = dir.join("tampered");
        assert!(
            download_verified(&agent, &asset.browser_download_url, &tampered, &"0".repeat(64), &mut |_| {})
                .is_err()
        );
        assert!(!tampered.exists(), "a rejected download must not be kept");
        assert!(!tampered.with_extension("part").exists());
        println!("LIVE download: verified {name} ({} bytes) and rejected a bad checksum", std::fs::metadata(&dest).unwrap().len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The shell copy of the manifest definition, byte-for-byte the body of
    /// `payload_id()` in `scripts/install-wrapper.sh`. The two *must* agree:
    /// the installer records the identity and this module compares against it.
    #[cfg(target_os = "linux")]
    const SHELL_PAYLOAD_ID: &str = r#"
payload_id() {
  local dir="$1"
  (
    cd "$dir" || exit 1
    { [ -f wrapper-lite ] && printf 'wrapper-lite\0'; find rootfs/system -type f -print0; } |
      LC_ALL=C sort -z |
      xargs -0 -r sha256sum |
      sha256sum |
      awk '{print $1}'
  )
}
"#;

    /// Linux-only: the script is the Linux/GNU-tools install path anyway.
    #[cfg(target_os = "linux")]
    #[test]
    fn rust_and_shell_payload_identity_agree() {
        let d = tmp("payload-shell");
        fake_payload(&d);
        // A name with a space, to pin the "one record per file" parsing.
        std::fs::create_dir_all(d.join("rootfs/system/lib64")).unwrap();
        std::fs::write(d.join("rootfs/system/lib64/a b.so"), b"so").unwrap();

        let script = format!("set -eu{SHELL_PAYLOAD_ID}\npayload_id \"$1\"\n");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .arg("sh")
            .arg(&d)
            .output()
            .expect("sh must exist");
        assert!(out.status.success(), "sh failed: {}", String::from_utf8_lossy(&out.stderr));
        let shell = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(shell.len(), 64, "{shell:?}");
        assert_eq!(shell, payload_hash(&d).unwrap());

        // …and the shipped script really does carry that definition.
        let installer = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/install-wrapper.sh"),
        )
        .expect("scripts/install-wrapper.sh");
        for needle in ["payload_id()", "xargs -0 -r sha256sum", "payload-version"] {
            assert!(installer.contains(needle), "install-wrapper.sh lost {needle:?}");
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}

