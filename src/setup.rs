//! Guided first-run setup wizard.
//!
//! Two entry points, both rendering the same egui wizard:
//!   * `mordent setup` — always starts here (safe to re-run any time).
//!   * `mordent ui` with NO `~/.config/mordent/config.toml` — the wizard runs first,
//!     then the normal app (see `ui::launch`). `MORDENT_SELFTEST` never enters it.
//!
//! Debug hook: `MORDENT_DEMO_SETUP=<1..7>` lands directly on that step (and fires
//! that step's primary async action) for headless screenshots, alongside the
//! other `MORDENT_DEMO_*` hooks.
//!
//! # How the helper scripts are driven
//!
//! The wizard does not reimplement the install: it runs the same scripts a
//! manual setup would, through `pkexec` (so the desktop asks for the admin
//! password exactly once per action):
//!
//! ```text
//! pkexec <scripts>/install-wrapper.sh --src <bundle> --port 12340
//! pkexec <scripts>/login-wrapper.sh --email E --password-file F \
//!        --port 12340 --twofa-dir <prefix>/rootfs/data
//! pkexec sh -c 'printf %s "$1" > "$2"' mordent-setup-2fa <code> <2fa file>
//! ```
//!
//! `pkexec` is invoked BY NAME (PATH lookup), never by an absolute path, so a
//! fake `pkexec` shim on `PATH` can drive the whole wizard in tests.
//! `MORDENT_SETUP_SCRIPTS_DIR` overrides where the scripts are looked up (tests /
//! unusual bundles); `MORDENT_SETUP_PREFIX` overrides the helper install prefix.
//!
//! pkexec starts the target with a **minimal environment** (the caller's env —
//! including anything we might set — is discarded), so the Apple ID password
//! is handed over through a private 0600 temp file named by `--password-file`
//! rather than `$MORDENT_WRAPPER_PASSWORD`. The 2FA code is delivered the way
//! `login-wrapper.sh` expects: written to `<prefix>/rootfs/data/2fa.txt` by a
//! tiny root shell, since that file lives under a root-owned prefix.

use crate::config::Config;
use crate::theme::Theme;
use crate::ui::{is_loopback_url, wrapper_down_hint};
use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Default local wrapper URL (matches the wrapper-lite default port).
pub const DEFAULT_URL: &str = "http://127.0.0.1:12340";
/// Default wrapper-lite port.
pub const DEFAULT_PORT: u16 = 12340;
/// Default helper install prefix (`install-wrapper.sh --prefix`).
pub const DEFAULT_PREFIX: &str = "/opt/mordent-wrapper";

/// Storefront shortcuts offered by the picker (free text stays possible).
pub const COMMON_STOREFRONTS: &[(&str, &str)] = &[
    ("us", "United States"),
    ("ca", "Canada"),
    ("uk", "United Kingdom"),
    ("au", "Australia"),
    ("jp", "Japan"),
    ("de", "Germany"),
    ("fr", "France"),
];

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// Wizard step. `number()` is the 1-based position in the reference flow, not
/// a navigation index: the visited path skips steps (the local-install step is
/// Linux-only; the URL step jumps straight to preferences on some paths).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    Welcome,
    RemoteUrl,
    LocalInstall,
    Login,
    Prefs,
    Audio,
    Finish,
}

impl Step {
    pub fn number(self) -> u8 {
        match self {
            Step::Welcome => 1,
            Step::RemoteUrl => 2,
            Step::LocalInstall => 3,
            Step::Login => 4,
            Step::Prefs => 5,
            Step::Audio => 6,
            Step::Finish => 7,
        }
    }

    pub fn from_number(n: u8) -> Option<Self> {
        Some(match n {
            1 => Step::Welcome,
            2 => Step::RemoteUrl,
            3 => Step::LocalInstall,
            4 => Step::Login,
            5 => Step::Prefs,
            6 => Step::Audio,
            7 => Step::Finish,
            _ => return None,
        })
    }

    pub fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Welcome",
            Step::RemoteUrl => "Connect to a music helper",
            Step::LocalInstall => "Install the helper on this computer",
            Step::Login => "Sign in to Apple Music",
            Step::Prefs => "Preferences",
            Step::Audio => "Audio check",
            Step::Finish => "All set",
        }
    }

    fn progress(self) -> String {
        format!("step {} of 7 — {}", self.number(), self.title())
    }
}

/// Which path the user picked on the welcome screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Plan {
    Undecided,
    /// Use a wrapper URL (a friend's server, a VPS, or a local helper).
    Remote,
    /// Install the bundled helper locally (Linux only).
    Local,
    /// Jump straight to preferences (90-second previews only).
    Skip,
}

// ---------------------------------------------------------------------------
// Process specs (pure — the exact argv/env the wizard runs)
// ---------------------------------------------------------------------------

/// One child process to run, described without touching the OS. Tests assert
/// these shapes without spawning anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CmdSpec {
    pub program: String,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
}

/// The helper install prefix (`MORDENT_SETUP_PREFIX` override, else
/// [`DEFAULT_PREFIX`]).
pub fn helper_prefix() -> String {
    std::env::var("MORDENT_SETUP_PREFIX").unwrap_or_else(|_| DEFAULT_PREFIX.into())
}

/// `<prefix>/rootfs/data/2fa.txt` — the file `login-wrapper.sh` polls for the
/// one-time two-factor code.
pub fn twofa_file(prefix: &str) -> PathBuf {
    Path::new(prefix).join("rootfs/data/2fa.txt")
}

/// Directory handed to `login-wrapper.sh --twofa-dir`.
pub fn twofa_dir(prefix: &str) -> PathBuf {
    Path::new(prefix).join("rootfs/data")
}

/// Bundle root that ships `wrapper-lite` + `rootfs` next to `scripts/`.
pub fn bundle_dir(scripts_dir: &Path) -> PathBuf {
    match scripts_dir.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// `pkexec <scripts>/install-wrapper.sh --src <bundle> --port <port>`.
pub fn install_spec(scripts_dir: &Path, port: u16) -> CmdSpec {
    CmdSpec {
        program: "pkexec".into(),
        args: vec![
            scripts_dir
                .join("install-wrapper.sh")
                .to_string_lossy()
                .into_owned(),
            "--src".into(),
            bundle_dir(scripts_dir).to_string_lossy().into_owned(),
            "--port".into(),
            port.to_string(),
        ],
        envs: Vec::new(),
    }
}

/// `pkexec <scripts>/login-wrapper.sh --email E --password-file F --port P
///  --twofa-dir D`. The password travels in a file because pkexec discards the
/// caller's environment (`$MORDENT_WRAPPER_PASSWORD` would never arrive).
pub fn login_spec(
    scripts_dir: &Path,
    email: &str,
    password_file: &Path,
    port: u16,
    prefix: &str,
) -> CmdSpec {
    CmdSpec {
        program: "pkexec".into(),
        args: vec![
            scripts_dir
                .join("login-wrapper.sh")
                .to_string_lossy()
                .into_owned(),
            "--email".into(),
            email.to_string(),
            "--password-file".into(),
            password_file.to_string_lossy().into_owned(),
            "--port".into(),
            port.to_string(),
            "--twofa-dir".into(),
            twofa_dir(prefix).to_string_lossy().into_owned(),
        ],
        envs: Vec::new(),
    }
}

/// `pkexec sh -c 'printf %s "$1" > "$2"' mordent-setup-2fa <code> <2fa file>` —
/// the code and path are positional arguments, so neither is ever re-parsed by
/// a shell.
pub fn twofa_spec(file: &Path, code: &str) -> CmdSpec {
    CmdSpec {
        program: "pkexec".into(),
        args: vec![
            "sh".into(),
            "-c".into(),
            r#"printf %s "$1" > "$2""#.into(),
            "mordent-setup-2fa".into(),
            code.to_string(),
            file.to_string_lossy().into_owned(),
        ],
        envs: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Pure helpers (unit-tested)
// ---------------------------------------------------------------------------

/// Bundle-dir lookup for the "install locally" path: the scripts live either
/// next to the binary, one level up (an extracted bundle root), or beside the
/// current directory. Returns the first `scripts/` dir holding
/// `install-wrapper.sh`. `cwd` is passed in so the search stays pure/testable.
pub fn find_scripts_dir_in(exe_dir: &Path, cwd: &Path) -> Option<PathBuf> {
    let candidates = [
        exe_dir.join("scripts"),
        exe_dir.join("..").join("scripts"),
        cwd.join("scripts"),
    ];
    candidates
        .into_iter()
        .find(|d| d.join("install-wrapper.sh").is_file())
}

/// [`find_scripts_dir_in`] against the real binary location and cwd.
pub fn find_scripts_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    find_scripts_dir_in(exe_dir, &cwd)
}

/// Where the helper scripts come from: `MORDENT_SETUP_SCRIPTS_DIR` when it actually
/// contains `install-wrapper.sh`, else the bundle lookup.
pub fn resolve_scripts_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("MORDENT_SETUP_SCRIPTS_DIR") {
        let dir = PathBuf::from(dir);
        if dir.join("install-wrapper.sh").is_file() {
            return Some(dir);
        }
    }
    find_scripts_dir()
}

/// Normalize a wrapper URL the user typed: trim, assume `http://` when no
/// scheme is given, drop trailing slashes. `None` = unusable input (empty,
/// whitespace inside, a non-http(s) scheme, or no host).
pub fn normalize_url(input: &str) -> Option<String> {
    let t = input.trim();
    if t.is_empty() || t.chars().any(char::is_whitespace) {
        return None;
    }
    let with_scheme = if t.contains("://") {
        t.to_string()
    } else {
        format!("http://{t}")
    };
    let (scheme, rest) = with_scheme.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = rest.trim_end_matches('/');
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{rest}"))
}

/// True for a well-formed 2-letter storefront country code.
pub fn storefront_ok(code: &str) -> bool {
    let c = code.trim();
    c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic())
}

/// Lower-cased storefront code, or `None` when malformed.
pub fn normalize_storefront(code: &str) -> Option<String> {
    storefront_ok(code).then(|| code.trim().to_ascii_lowercase())
}

/// The base URL a helper script prints on stdout (last `http(s)://…` line).
pub fn parse_base_url(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .map(str::trim)
        .rev()
        .find(|l| l.starts_with("http://") || l.starts_with("https://"))
        .map(|l| l.trim_end_matches('/').to_string())
}

/// Whether a helper log line means "the login is waiting for a 2FA code".
/// `login-wrapper.sh` logs this before it starts polling `2fa.txt`.
pub fn line_needs_twofa(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("2fa") || l.contains("two-factor") || l.contains("two factor") || l.contains("waiting up to")
}

/// Human explanation of a failed privileged action. Only 126/127 have a
/// pkexec-defined meaning; every other exit status is the target program's own
/// and is already spelled out in the streamed log.
pub fn explain_failure(code: Option<i32>) -> &'static str {
    match code {
        Some(126) => "authorization was dismissed (the admin-password dialog was closed)",
        Some(127) => "not authorized, or pkexec/polkit is not installed",
        Some(_) => "the command exited with an error (see the log above)",
        None => "the process could not be started",
    }
}

/// Ready-to-paste manual install command (shown when pkexec is unavailable or
/// was refused).
pub fn manual_install_line(scripts_dir: &Path) -> String {
    format!(
        "sudo {} --src {}",
        scripts_dir.join("install-wrapper.sh").display(),
        bundle_dir(scripts_dir).display()
    )
}

// ---------------------------------------------------------------------------
// Messages from background work
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tag {
    Install,
    Login,
    Twofa,
}

/// Result of a privileged child process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcResult {
    pub ok: bool,
    pub code: Option<i32>,
    /// Base URL parsed from stdout (install/login print it on success).
    pub base_url: Option<String>,
    /// Set when the process could not even be spawned (e.g. no `pkexec`).
    pub error: Option<String>,
}

impl ProcResult {
    fn failure(code: Option<i32>, error: Option<String>) -> Self {
        Self {
            ok: false,
            code,
            base_url: None,
            error,
        }
    }

    /// One-line summary for the wizard UI.
    pub fn summary(&self) -> String {
        if let Some(e) = &self.error {
            return e.clone();
        }
        if self.ok {
            return "done".into();
        }
        explain_failure(self.code).to_string()
    }
}

enum SetupMsg {
    UrlProbe {
        url: String,
        regions: Result<Vec<String>, String>,
    },
    Log {
        tag: Tag,
        line: String,
    },
    InstallDone(ProcResult),
    LoginDone(ProcResult),
    TwofaDone {
        ok: bool,
        message: String,
    },
    AudioDone {
        ok: bool,
        detail: String,
    },
    /// The Preferences step's own update check (see `crate::update`).
    UpdateChecked(Box<crate::update::Check>),
}

// ---------------------------------------------------------------------------
// Child-process runner
// ---------------------------------------------------------------------------

/// Handle to a running child: the wizard can kill it (leaving a step).
struct ProcHandle {
    child: Arc<Mutex<Option<std::process::Child>>>,
}

impl ProcHandle {
    fn kill(&self) {
        if let Some(c) = self.child.lock().unwrap().as_mut() {
            let _ = c.kill();
        }
    }
}

/// Spawn `spec`, streaming every stdout/stderr line as [`SetupMsg::Log`] and
/// sending `done(result, stdout)` when it exits. Returns the kill handle, or
/// the spawn error (`pkexec` missing is the common case).
fn spawn_proc(
    spec: &CmdSpec,
    tag: Tag,
    tx: Sender<SetupMsg>,
    done: Box<dyn FnOnce(ProcResult, String) -> SetupMsg + Send>,
) -> Result<ProcHandle, std::io::Error> {
    let mut cmd = std::process::Command::new(&spec.program);
    cmd.args(&spec.args);
    for (k, v) in &spec.envs {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // stdout is captured both for the log and for the base-URL parse.
    let buf = Arc::new(Mutex::new(String::new()));
    let mut out_join = None;
    if let Some(o) = stdout {
        out_join = Some(stream_lines(o, tag, tx.clone(), Some(buf.clone())));
    }
    if let Some(e) = stderr {
        let _ = stream_lines(e, tag, tx.clone(), None);
    }

    let handle = Arc::new(Mutex::new(Some(child)));
    let poll = handle.clone();
    std::thread::Builder::new()
        .name("mordent-setup-wait".into())
        .spawn(move || {
            let code = loop {
                let state = {
                    let mut guard = poll.lock().unwrap();
                    match guard.as_mut() {
                        Some(c) => c.try_wait(),
                        None => break None, // killed + taken
                    }
                };
                match state {
                    Ok(Some(status)) => break status.code(),
                    Ok(None) => std::thread::sleep(Duration::from_millis(60)),
                    Err(_) => break None,
                }
            };
            // Drain the last stdout lines before reading the buffer.
            if let Some(j) = out_join {
                let _ = j.join();
            }
            let text = buf.lock().unwrap().clone();
            let base_url = parse_base_url(&text);
            let result = ProcResult {
                ok: code == Some(0),
                code,
                base_url,
                error: None,
            };
            let _ = tx.send(done(result, text));
        })
        .ok();

    Ok(ProcHandle { child: handle })
}

/// Read a pipe line by line into `SetupMsg::Log` (and optionally `buf`).
fn stream_lines<R: std::io::Read + Send + 'static>(
    r: R,
    tag: Tag,
    tx: Sender<SetupMsg>,
    buf: Option<Arc<Mutex<String>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(r);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if let Some(b) = &buf {
                let mut g = b.lock().unwrap();
                g.push_str(&line);
                g.push('\n');
            }
            let _ = tx.send(SetupMsg::Log { tag, line });
        }
    })
}

// ---------------------------------------------------------------------------
// Wizard state
// ---------------------------------------------------------------------------

/// What the wizard asks the app to do after a frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    None,
    /// Write the collected config and leave the wizard.
    Finish,
}

/// All wizard state, including the child processes it owns.
pub struct SetupState {
    step: Step,
    plan: Plan,
    /// Visited path, so Back retraces exactly where the user came from.
    history: Vec<Step>,
    scripts_dir: Option<PathBuf>,
    prefix: String,
    port: u16,

    tx: Sender<SetupMsg>,
    rx: Receiver<SetupMsg>,

    // step 2
    url: String,
    probing: bool,
    probe: Option<Result<Vec<String>, String>>,
    probe_url: Option<String>,

    // step 3
    install_log: Vec<String>,
    install_proc: Option<ProcHandle>,
    install_done: Option<ProcResult>,

    // step 4
    email: String,
    password: String,
    login_log: Vec<String>,
    login_proc: Option<ProcHandle>,
    login_started: Option<Instant>,
    login_done: Option<ProcResult>,
    needs_2fa: bool,
    twofa: String,
    twofa_note: Option<String>,
    pw_file: Option<PathBuf>,

    // step 5
    storefront: String,
    media_token: String,
    report_plays: bool,
    cache_size_gb: u64,
    check_updates: bool,
    /// Update check state for the Preferences line (`None` = not checked).
    update: Option<crate::update::Check>,
    update_checking: bool,
    /// Cached last check / tag seen (seeded by the app; adopted back at Finish).
    last_update_check: Option<u64>,
    last_seen_tag: Option<String>,

    // step 6
    audio_checked: bool,
    audio: Option<Result<(), String>>,

    error: Option<String>,

    /// `MORDENT_DEMO_SETUP` armed: fire the landing step's async action once.
    demo: bool,
    demo_fired: bool,
}

impl Drop for SetupState {
    fn drop(&mut self) {
        self.kill_procs();
        self.remove_pw_file();
    }
}

impl SetupState {
    /// Fresh wizard, starting on the welcome screen.
    pub fn new(cfg: &Config) -> Self {
        Self::with_step(Step::Welcome, cfg)
    }

    /// `MORDENT_DEMO_SETUP=<1..7>`: land on a step and fire its async action, for
    /// headless screenshots.
    pub fn demo(n: u8, cfg: &Config) -> Self {
        let step = Step::from_number(n).unwrap_or(Step::Welcome);
        let mut s = Self::with_step(step, cfg);
        s.demo = true;
        if matches!(step, Step::LocalInstall | Step::Login) {
            s.plan = Plan::Local;
        } else if matches!(step, Step::RemoteUrl | Step::Prefs) {
            s.plan = Plan::Remote;
        }
        s
    }

    fn with_step(step: Step, cfg: &Config) -> Self {
        let (tx, rx) = channel();
        let url = if cfg.wrapper_url.trim().is_empty() {
            DEFAULT_URL.to_string()
        } else {
            cfg.wrapper_url.clone()
        };
        let storefront = normalize_storefront(&cfg.storefront).unwrap_or_else(|| "us".into());
        Self {
            step,
            plan: Plan::Undecided,
            history: Vec::new(),
            scripts_dir: resolve_scripts_dir(),
            prefix: helper_prefix(),
            port: DEFAULT_PORT,
            tx,
            rx,
            url,
            probing: false,
            probe: None,
            probe_url: None,
            install_log: Vec::new(),
            install_proc: None,
            install_done: None,
            email: String::new(),
            password: String::new(),
            login_log: Vec::new(),
            login_proc: None,
            login_started: None,
            login_done: None,
            needs_2fa: false,
            twofa: String::new(),
            twofa_note: None,
            pw_file: None,
            storefront,
            media_token: cfg.media_user_token.clone().unwrap_or_default(),
            report_plays: cfg.report_plays,
            cache_size_gb: cfg.cache_size_gb,
            check_updates: cfg.check_updates,
            update: None,
            update_checking: false,
            last_update_check: None,
            last_seen_tag: None,
            audio_checked: false,
            audio: None,
            error: None,
            demo: false,
            demo_fired: false,
        }
    }

    // ---- transitions ----

    fn goto(&mut self, next: Step) {
        if next == self.step {
            return;
        }
        self.leave_step();
        self.history.push(self.step);
        self.step = next;
        self.demo_fired = false;
        self.enter_step(next);
    }

    fn back(&mut self) {
        if self.busy() {
            return;
        }
        if let Some(prev) = self.history.pop() {
            self.leave_step();
            self.step = prev;
            self.demo_fired = false;
            self.enter_step(prev);
        }
    }

    fn enter_step(&mut self, step: Step) {
        if step == Step::Audio {
            self.start_audio_check();
        }
    }

    fn leave_step(&mut self) {
        // Never leave a privileged child behind.
        self.kill_procs();
    }

    fn kill_procs(&mut self) {
        if let Some(p) = self.install_proc.take() {
            p.kill();
        }
        if let Some(p) = self.login_proc.take() {
            p.kill();
        }
        self.remove_pw_file();
    }

    /// True while a privileged child runs (navigation is blocked then).
    fn busy(&self) -> bool {
        self.install_proc.is_some() || self.login_proc.is_some()
    }

    /// Whether this machine can offer the local-install path (Linux + the
    /// bundled script found near the binary).
    pub fn local_install_offered(&self) -> bool {
        cfg!(target_os = "linux") && self.scripts_dir.is_some()
    }

    /// Whether the Apple-login step can run the login script for this URL.
    fn login_offered_for(&self, url: &str) -> bool {
        cfg!(target_os = "linux")
            && self.scripts_dir.is_some()
            && is_loopback_url(url)
    }

    /// The Next target of the current step (mirrors the rendered button). Kept
    /// separate from rendering so the guards are unit-testable.
    fn next_step(&self) -> Option<Step> {
        match self.step {
            Step::Welcome => None,
            Step::RemoteUrl => {
                if self.login_offered_for(&self.url) && self.probe_is_not_logged_in() {
                    Some(Step::Login)
                } else {
                    Some(Step::Prefs)
                }
            }
            Step::LocalInstall => Some(Step::Login),
            Step::Login => Some(Step::Prefs),
            Step::Prefs => Some(Step::Audio),
            Step::Audio => Some(Step::Finish),
            Step::Finish => None,
        }
    }

    /// A successful probe that returned no region ⇒ the helper is up but not
    /// signed in to Apple.
    fn probe_is_not_logged_in(&self) -> bool {
        matches!(&self.probe, Some(Ok(r)) if r.is_empty())
    }

    /// Can the current step advance? (pure guard, mirrors the rendered button)
    pub fn can_next(&self) -> bool {
        if self.busy() {
            return false;
        }
        match self.step {
            Step::Welcome | Step::Finish => false,
            Step::LocalInstall => self.install_done.as_ref().is_some_and(|r| r.ok),
            _ => true,
        }
    }

    fn advance(&mut self) {
        if !self.can_next() {
            return;
        }
        if let Some(next) = self.next_step() {
            self.goto(next);
        }
    }

    // ---- collected config ----

    /// `base` with the wizard's collected values applied. Uncollected fields
    /// (e.g. a custom `cache_dir`) are inherited from the loaded config, and
    /// the file writer preserves unknown keys on top of that.
    pub fn new_config(&self, base: &Config) -> Config {
        let mut cfg = base.clone();
        cfg.storefront = normalize_storefront(&self.storefront).unwrap_or_else(|| "us".into());
        cfg.wrapper_url = normalize_url(&self.url).unwrap_or_else(|| DEFAULT_URL.into());
        let tok = self.media_token.trim();
        cfg.media_user_token = (!tok.is_empty()).then(|| tok.to_string());
        cfg.report_plays = self.report_plays;
        cfg.cache_size_gb = self.cache_size_gb;
        cfg.check_updates = self.check_updates;
        cfg
    }

    /// Hand the wizard the app's cached check time, so the Preferences step can
    /// show "checked N h ago" and the one automatic check honours the 24 h gate.
    pub fn seed_update_cache(&mut self, last_check: Option<u64>, seen_tag: Option<String>) {
        self.last_update_check = last_check;
        self.last_seen_tag = seen_tag;
    }

    /// What the Preferences step learned about updates, for the app to adopt
    /// when the wizard finishes (so the same request is not repeated).
    pub fn update_adoption(&self) -> crate::ui::UpdateAdoption {
        crate::ui::UpdateAdoption {
            last_check: self.last_update_check,
            seen_tag: self.last_seen_tag.clone(),
            check: self.update.clone(),
        }
    }

    /// Run the one automatic check the Preferences step shows. Called by the
    /// app (never by step entry), so unit tests and demo/screenshot runs never
    /// touch the network.
    pub fn auto_check_updates(&mut self) {
        if self.demo || self.update_checking {
            return;
        }
        self.check_updates_now(false);
    }

    /// Spawn one check. `force` = the "Check now" button: it bypasses both the
    /// 24 h gate and the preference, exactly like the app's own button.
    fn check_updates_now(&mut self, force: bool) {
        if self.update_checking {
            return;
        }
        let now = crate::config::unix_now();
        let cached = crate::update::Cached {
            last_check: self.last_update_check,
            last_seen_tag: self.last_seen_tag.clone(),
        };
        if !crate::update::should_check(&cached, self.check_updates, now, force) {
            return;
        }
        self.update_checking = true;
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("mordent-setup-update".into())
            .spawn(move || {
                // The wizard only needs the comparison; the app fetches the
                // checksums when it actually stages an update.
                let (check, _sums) = crate::update::check_with_sums(&crate::update::api_agent());
                let _ = tx.send(SetupMsg::UpdateChecked(Box::new(check)));
            })
            .ok();
    }

    pub fn set_error(&mut self, msg: String) {
        self.error = Some(msg);
    }

    // ---- async actions ----

    fn probe_url(&mut self) {
        let Some(url) = normalize_url(&self.url) else {
            self.probe = Some(Err("that does not look like a URL — try http://host:12340".into()));
            return;
        };
        self.url = url.clone();
        self.probing = true;
        self.probe = None;
        self.probe_url = Some(url.clone());
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("mordent-setup-probe".into())
            .spawn(move || {
                let regions = crate::wrapper::Wrapper::new(&url)
                    .regions()
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send(SetupMsg::UrlProbe { url, regions });
            })
            .ok();
    }

    fn start_install(&mut self) {
        let Some(scripts_dir) = self.scripts_dir.clone() else {
            self.install_done = Some(ProcResult::failure(
                None,
                Some("helper scripts not found next to the app".into()),
            ));
            return;
        };
        let spec = install_spec(&scripts_dir, self.port);
        self.install_log.clear();
        self.install_done = None;
        let tx = self.tx.clone();
        match spawn_proc(
            &spec,
            Tag::Install,
            tx,
            Box::new(|r: ProcResult, _out: String| SetupMsg::InstallDone(r)),
        ) {
            Ok(handle) => self.install_proc = Some(handle),
            Err(e) => {
                let msg = format!(
                    "could not run pkexec ({e}). Install the helper by hand:\n  {}",
                    manual_install_line(&scripts_dir)
                );
                self.install_log.push(msg.clone());
                self.install_done = Some(ProcResult::failure(None, Some(msg)));
            }
        }
    }

    fn start_login(&mut self) {
        let Some(scripts_dir) = self.scripts_dir.clone() else {
            self.login_done = Some(ProcResult::failure(
                None,
                Some("helper scripts not found next to the app".into()),
            ));
            return;
        };
        if self.email.trim().is_empty() || self.password.is_empty() {
            self.error = Some("enter your Apple ID email and password first".into());
            return;
        }
        let pw_file = match self.write_pw_file() {
            Ok(p) => p,
            Err(e) => {
                self.error = Some(format!("could not stage the password: {e}"));
                return;
            }
        };
        let spec = login_spec(
            &scripts_dir,
            self.email.trim(),
            &pw_file,
            self.port,
            &self.prefix,
        );
        self.login_log.clear();
        self.login_done = None;
        self.needs_2fa = false;
        self.twofa_note = None;
        self.login_started = Some(Instant::now());
        let tx = self.tx.clone();
        match spawn_proc(
            &spec,
            Tag::Login,
            tx,
            Box::new(|r: ProcResult, _out: String| SetupMsg::LoginDone(r)),
        ) {
            Ok(handle) => self.login_proc = Some(handle),
            Err(e) => {
                let msg = format!(
                    "could not run pkexec ({e}); run the login by hand:\n  \
                     sudo {}/login-wrapper.sh --email {}",
                    scripts_dir.display(),
                    self.email.trim()
                );
                self.login_log.push(msg.clone());
                self.login_done = Some(ProcResult::failure(None, Some(msg)));
                self.login_proc = None;
                self.remove_pw_file();
            }
        }
    }

    fn submit_twofa(&mut self) {
        let code: String = self
            .twofa
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if code.is_empty() {
            self.error = Some("enter the six-digit code from your Apple device".into());
            return;
        }
        let file = twofa_file(&self.prefix);
        let spec = twofa_spec(&file, &code);
        let file_label = file.display().to_string();
        self.twofa_note = Some("sending the code…".into());
        let tx = self.tx.clone();
        match spawn_proc(
            &spec,
            Tag::Twofa,
            tx,
            Box::new(move |r: ProcResult, _out: String| SetupMsg::TwofaDone {
                ok: r.ok,
                message: if r.ok {
                    format!("code sent to {file_label}")
                } else {
                    r.summary()
                },
            }),
        ) {
            Ok(_) => {
                self.twofa.clear();
            }
            Err(e) => {
                self.twofa_note = Some(format!("could not run pkexec ({e})"));
            }
        }
    }

    fn start_audio_check(&mut self) {
        if self.audio_checked || self.audio.is_some() {
            return;
        }
        self.audio_checked = true;
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("mordent-setup-audio".into())
            .spawn(move || {
                let res = rodio::OutputStream::try_default();
                let (ok, detail) = match res {
                    Ok(_) => (true, String::new()),
                    Err(e) => (false, e.to_string()),
                };
                let _ = tx.send(SetupMsg::AudioDone { ok, detail });
            })
            .ok();
    }

    // ---- password staging (0600 temp file, deleted when done) ----

    fn pw_path(&self) -> PathBuf {
        std::env::temp_dir().join(format!("mordent-setup-{}-password", std::process::id()))
    }

    fn write_pw_file(&mut self) -> std::io::Result<PathBuf> {
        use std::io::Write;
        let path = self.pw_path();
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&path)?;
        f.write_all(self.password.as_bytes())?;
        f.sync_all().ok();
        self.pw_file = Some(path.clone());
        Ok(path)
    }

    fn remove_pw_file(&mut self) {
        if let Some(p) = self.pw_file.take() {
            let _ = std::fs::remove_file(p);
        }
    }

    // ---- message pump ----

    fn drain(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                SetupMsg::UrlProbe { url, regions } => {
                    if self.probe_url.as_deref() == Some(url.as_str()) {
                        self.probing = false;
                        self.probe = Some(regions);
                    }
                }
                SetupMsg::Log { tag, line } => match tag {
                    Tag::Install => push_log(&mut self.install_log, line),
                    Tag::Login => {
                        if line_needs_twofa(&line) {
                            self.needs_2fa = true;
                        }
                        push_log(&mut self.login_log, line);
                    }
                    Tag::Twofa => {}
                },
                SetupMsg::InstallDone(r) => {
                    self.install_proc = None;
                    if let Some(url) = &r.base_url {
                        self.url = url.clone();
                    }
                    self.install_done = Some(r);
                }
                SetupMsg::LoginDone(r) => {
                    self.login_proc = None;
                    self.remove_pw_file();
                    if let Some(url) = &r.base_url {
                        self.url = url.clone();
                    }
                    if r.ok {
                        // The session lives in the helper now; do not keep the
                        // password around (nor show the 2FA block again).
                        self.password.clear();
                        self.needs_2fa = false;
                        self.twofa.clear();
                    }
                    self.login_done = Some(r);
                }
                SetupMsg::TwofaDone { ok, message } => {
                    self.twofa_note = Some(if ok {
                        message
                    } else {
                        format!("could not deliver the code: {message}")
                    });
                }
                SetupMsg::AudioDone { ok, detail } => {
                    self.audio = Some(if ok { Ok(()) } else { Err(detail) });
                }
                SetupMsg::UpdateChecked(check) => {
                    self.update_checking = false;
                    self.last_update_check = Some(crate::config::unix_now());
                    match &*check {
                        crate::update::Check::Newer { latest, .. }
                        | crate::update::Check::UpToDate { latest }
                        | crate::update::Check::Uncomparable { latest } => {
                            self.last_seen_tag = Some(latest.to_string());
                        }
                        crate::update::Check::Failed { .. } => {}
                    }
                    self.update = Some(*check);
                }
            }
        }
    }

    // ---- demo autorun ----

    fn demo_autorun(&mut self) {
        if !self.demo || self.demo_fired || self.busy() {
            return;
        }
        let fired = match self.step {
            Step::RemoteUrl => {
                if !self.probing && self.probe.is_none() {
                    self.probe_url();
                    true
                } else {
                    false
                }
            }
            Step::LocalInstall => {
                if self.install_done.is_none() {
                    self.start_install();
                    true
                } else {
                    false
                }
            }
            Step::Login => {
                if self.login_done.is_none() {
                    if self.email.is_empty() {
                        self.email = "demo@example.com".into();
                    }
                    if self.password.is_empty() {
                        self.password = "demo-password".into();
                    }
                    self.start_login();
                    true
                } else {
                    false
                }
            }
            Step::Audio => {
                self.start_audio_check();
                true
            }
            _ => false,
        };
        if fired {
            self.demo_fired = true;
        }
    }
}

fn push_log(log: &mut Vec<String>, line: String) {
    log.push(line);
    if log.len() > 500 {
        let drop = log.len() - 500;
        log.drain(0..drop);
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn primary_button(ui: &mut egui::Ui, theme: &Theme, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(egui::RichText::new(label).color(egui::Color32::WHITE).strong())
            .fill(theme.accent)
            .corner_radius(6.0),
    )
}

fn step_body(ui: &mut egui::Ui, text: impl Into<egui::RichText>) {
    ui.label(text.into().size(13.0));
}

fn hint(ui: &mut egui::Ui, theme: &Theme, text: &str) {
    ui.label(egui::RichText::new(text).size(11.0).color(theme.weak));
}

fn log_view(ui: &mut egui::Ui, theme: &Theme, lines: &[String]) {
    egui::ScrollArea::vertical()
        .max_height(170.0)
        .auto_shrink(false)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for l in lines {
                ui.label(
                    egui::RichText::new(l)
                        .size(11.0)
                        .monospace()
                        .color(theme.weak),
                );
            }
        });
}

impl SetupState {
    /// Render one frame and report what the app should do next.
    pub fn ui(&mut self, ui: &mut egui::Ui, theme: &Theme) -> Action {
        self.drain();
        self.demo_autorun();

        let mut action = Action::None;
        ui.add_space(14.0);
        ui.vertical_centered(|ui| {
            ui.label(
                egui::RichText::new("mordent setup")
                    .size(22.0)
                    .strong()
                    .color(theme.text),
            );
            hint(ui, theme, &self.step.progress());
        });
        ui.add_space(10.0);

        let width = (ui.available_width() - 32.0).clamp(280.0, 660.0);
        ui.vertical_centered(|ui| {
            egui::Frame::default()
                .fill(theme.panel)
                .stroke(egui::Stroke::new(1.0_f32, theme.weak))
                .inner_margin(16.0)
                .corner_radius(8.0)
                .show(ui, |ui| {
                    ui.set_width(width);
                    ui.vertical(|ui| {
                        action = self.step_ui(ui, theme);
                    });
                });
        });
        action
    }

    fn step_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) -> Action {
        let mut action = Action::None;
        match self.step {
            Step::Welcome => self.welcome_ui(ui, theme),
            Step::RemoteUrl => self.url_ui(ui, theme),
            Step::LocalInstall => self.install_ui(ui, theme),
            Step::Login => self.login_ui(ui, theme),
            Step::Prefs => self.prefs_ui(ui, theme),
            Step::Audio => self.audio_ui(ui, theme),
            Step::Finish => action = self.finish_ui(ui, theme),
        }
        if let Some(e) = self.error.clone() {
            ui.add_space(8.0);
            ui.label(
                egui::RichText::new(e)
                    .size(12.0)
                    .color(egui::Color32::from_rgb(0xff, 0x9a, 0xa0)),
            );
        }
        action
    }

    /// Back / Next row shared by the linear steps.
    fn nav_row(&mut self, ui: &mut egui::Ui, next_label: &str) {
        ui.add_space(14.0);
        ui.separator();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let can_back = !self.history.is_empty() && !self.busy();
            if ui
                .add_enabled(can_back, egui::Button::new("← Back"))
                .clicked()
            {
                self.back();
            }
            ui.add_space(8.0);
            let can_next = self.can_next() && self.next_step().is_some();
            if ui
                .add_enabled(can_next, egui::Button::new(next_label))
                .clicked()
            {
                self.advance();
            }
        });
    }

    // ---- 1. welcome ----

    fn welcome_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        step_body(
            ui,
            "Mordent plays your Apple Music library natively. Full-quality (lossless) \
             playback needs a small local decryption helper — pick how you want to \
             set yours up. This takes about two minutes and you can change it later.",
        );
        ui.add_space(14.0);
        if primary_button(ui, theme, "Connect to a music helper").clicked() {
            self.plan = Plan::Remote;
            self.goto(Step::RemoteUrl);
        }
        hint(
            ui,
            theme,
            "Point Mordent at a helper URL — on this computer, a friend's server, or your own VPS.",
        );
        ui.add_space(10.0);
        if self.local_install_offered() {
            if ui
                .add(egui::Button::new("Install the helper on this computer"))
                .clicked()
            {
                self.plan = Plan::Local;
                self.goto(Step::LocalInstall);
            }
            hint(
                ui,
                theme,
                "Linux: installs and starts the bundled helper here (needs your admin password, once).",
            );
        } else if cfg!(target_os = "linux") {
            hint(
                ui,
                theme,
                "Install-here is unavailable in this build (helper scripts not found next to the app) — use a helper URL instead.",
            );
        }
        ui.add_space(10.0);
        if ui
            .add(egui::Button::new("Skip for now (90-second previews)"))
            .clicked()
        {
            self.plan = Plan::Skip;
            self.goto(Step::Prefs);
        }
        hint(
            ui,
            theme,
            "Previews play without a helper, but every track is cut to 90 seconds.",
        );
    }

    // ---- 2. wrapper URL ----

    fn url_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        step_body(
            ui,
            "Mordent talks to a small \"wrapper\" service that decrypts Apple Music audio.",
        );
        hint(
            ui,
            theme,
            "Using a friend's server or your own VPS? Paste its URL (e.g. http://music-box.local:12340). \
             Otherwise the default is the helper on this computer.",
        );
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label("Wrapper URL:");
            ui.add(
                egui::TextEdit::singleline(&mut self.url)
                    .desired_width(330.0)
                    .hint_text(DEFAULT_URL),
            );
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.probing, egui::Button::new("Test connection"))
                .clicked()
            {
                self.probe_url();
            }
            if self.probing {
                ui.spinner();
                ui.label("contacting the helper…");
            }
        });
        ui.add_space(8.0);
        match self.probe.clone() {
            Some(Ok(regions)) if regions.is_empty() => {
                ui.label(
                    egui::RichText::new("Connected — but the helper is not signed in to Apple yet.")
                        .size(12.0)
                        .color(egui::Color32::from_rgb(0xf0, 0xc0, 0x60)),
                );
                ui.label("Sign in from the next step to enable lossless playback.");
            }
            Some(Ok(regions)) => {
                ui.label(
                    egui::RichText::new(format!(
                        "Connected — regions available: {}",
                        regions.join(", ")
                    ))
                    .size(12.0)
                    .color(theme.lossless),
                );
                ui.horizontal(|ui| {
                    hint(ui, theme, "Use as storefront:");
                    for code in regions.iter().take(6) {
                        if ui.small_button(code).clicked() {
                            self.storefront = code.to_ascii_lowercase();
                        }
                    }
                });
            }
            Some(Err(_)) => {
                ui.label(
                    egui::RichText::new(wrapper_down_hint(&self.url))
                        .size(12.0)
                        .color(egui::Color32::from_rgb(0xf0, 0xc0, 0x60)),
                );
            }
            None => {}
        }
        self.nav_row(ui, "Next →");
    }

    // ---- 3. local install ----

    fn install_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        step_body(
            ui,
            "This installs the bundled helper as a system service (mordent-wrapper) so it \
             starts with your computer and keeps your Apple session alive. It is a \
             one-time step and needs your admin password.",
        );
        if let Some(dir) = &self.scripts_dir {
            hint(ui, theme, &format!("Installing from: {}", dir.display()));
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.install_proc.is_none() && self.install_done.as_ref().is_none_or(|r| !r.ok),
                    egui::Button::new(if self.install_done.is_some() {
                        "Retry install"
                    } else {
                        "Install & start helper"
                    }),
                )
                .clicked()
            {
                self.start_install();
            }
            if self.install_proc.is_some() {
                ui.spinner();
                ui.label("installing… (a system password prompt may appear)");
            }
        });
        ui.add_space(8.0);
        if self.install_done.as_ref().is_some_and(|r| r.ok) {
            let at = self
                .install_done
                .as_ref()
                .and_then(|r| r.base_url.clone())
                .unwrap_or_else(|| DEFAULT_URL.into());
            ui.label(
                egui::RichText::new(format!("Helper installed and running at {at}."))
                    .size(12.0)
                    .color(theme.lossless),
            );
        } else if let Some(r) = self.install_done.clone() {
            ui.label(
                egui::RichText::new(format!("Install failed — {}", r.summary()))
                    .size(12.0)
                    .color(egui::Color32::from_rgb(0xff, 0x9a, 0xa0)),
            );
            if let Some(dir) = &self.scripts_dir {
                ui.add_space(4.0);
                hint(ui, theme, "You can run it yourself:");
                ui.label(
                    egui::RichText::new(manual_install_line(dir))
                        .size(11.0)
                        .monospace(),
                );
            }
            hint(
                ui,
                theme,
                "See README → Setup (or docs/macos-container/ for a Mac / home server).",
            );
        }
        if !self.install_log.is_empty() {
            ui.add_space(6.0);
            log_view(ui, theme, &self.install_log);
        }
        self.nav_row(ui, "Next →");
    }

    // ---- 4. Apple login ----

    fn login_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        step_body(
            ui,
            "Your helper needs your Apple Music session once. Your password goes \
             straight to the helper (never stored) and only the login tokens remain.",
        );
        ui.add_space(6.0);
        let running = self.login_proc.is_some();
        let signed_in = self.login_done.as_ref().is_some_and(|r| r.ok);
        if signed_in {
            ui.label(
                egui::RichText::new("Signed in — full-quality playback is enabled.")
                    .size(12.0)
                    .color(theme.lossless),
            );
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Apple ID email:");
            ui.add(
                egui::TextEdit::singleline(&mut self.email)
                    .desired_width(260.0)
                    .hint_text("you@example.com"),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Password:        ");
            ui.add(
                egui::TextEdit::singleline(&mut self.password)
                    .desired_width(260.0)
                    .password(true),
            );
        });
        ui.add_space(8.0);
        if ui
            .add_enabled(!running, egui::Button::new("Log in"))
            .clicked()
        {
            self.start_login();
        }
        let waiting = !signed_in
            && (self.needs_2fa
                || self
                    .login_started
                    .is_some_and(|t| running && t.elapsed() > Duration::from_secs(6)));
        if waiting {
            ui.add_space(12.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new("Two-factor code")
                    .size(14.0)
                    .strong()
                    .color(theme.text),
            );
            hint(
                ui,
                theme,
                "Apple asked for a verification code — approve the prompt on your iPhone/Mac, or type the 6-digit code here.",
            );
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.twofa)
                        .desired_width(120.0)
                        .hint_text("123456"),
                );
                if ui.button("Send code").clicked() {
                    self.submit_twofa();
                }
            });
            if let Some(n) = &self.twofa_note {
                hint(ui, theme, n);
            }
        }
        if running {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("waiting for Apple…");
            });
        }
        if let Some(r) = self.login_done.clone()
            && !r.ok
        {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!("Login failed — {}", r.summary()))
                    .size(12.0)
                    .color(egui::Color32::from_rgb(0xff, 0x9a, 0xa0)),
            );
        }
        if !self.login_log.is_empty() {
            ui.add_space(6.0);
            log_view(ui, theme, &self.login_log);
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let can_back = !self.history.is_empty() && !self.busy();
            if ui
                .add_enabled(can_back, egui::Button::new("← Back"))
                .clicked()
            {
                self.back();
            }
            ui.add_space(8.0);
            if ui
                .add_enabled(!running, egui::Button::new("Skip — log in later"))
                .on_hover_text(
                    "Without a login the helper cannot decrypt audio: tracks will only play as 90-second previews until you sign in.",
                )
                .clicked()
            {
                self.goto(Step::Prefs);
            }
            ui.add_space(8.0);
            if ui
                .add_enabled(
                    !running && self.login_done.as_ref().is_some_and(|r| r.ok),
                    egui::Button::new("Next →"),
                )
                .clicked()
            {
                self.advance();
            }
        });
    }

    // ---- 5. preferences ----

    fn prefs_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        step_body(ui, "A couple of choices. Every one of these is editable later.");
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label("Storefront:");
            egui::ComboBox::from_id_salt("mordent-setup-storefront")
                .selected_text(self.storefront.to_ascii_uppercase())
                .show_ui(ui, |ui| {
                    for (code, name) in COMMON_STOREFRONTS {
                        ui.selectable_value(
                            &mut self.storefront,
                            code.to_string(),
                            format!("{code} — {name}"),
                        );
                    }
                });
            ui.add(
                egui::TextEdit::singleline(&mut self.storefront)
                    .desired_width(50.0)
                    .char_limit(2),
            );
        });
        hint(
            ui,
            theme,
            "Your Apple Music country code (\"us\" if unsure) — the helper reports what it can serve.",
        );
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label("media-user-token:");
            ui.add(
                egui::TextEdit::singleline(&mut self.media_token)
                    .desired_width(330.0)
                    .hint_text("optional"),
            );
        });
        hint(
            ui,
            theme,
            "Optional — unlocks your library, playlists and saving plays. Leave empty to skip.",
        );
        ui.add_space(10.0);
        ui.checkbox(&mut self.report_plays, "Report plays to Apple");
        hint(
            ui,
            theme,
            "Keeps Recently Played and play counts in sync while Mordent plays.",
        );
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label("Audio cache:");
            ui.add(egui::DragValue::new(&mut self.cache_size_gb).range(0..=500));
            ui.label("GB   (0 disables the cache)");
        });
        ui.add_space(10.0);
        ui.checkbox(&mut self.check_updates, "Check for new Mordent versions");
        hint(
            ui,
            theme,
            "One anonymous request to api.github.com per day; nothing about you or your \
             library is sent.",
        );
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("Mordent {}", crate::update::version().display()))
                    .size(12.0)
                    .strong(),
            );
            ui.separator();
            match &self.update {
                Some(crate::update::Check::Newer { latest, .. }) => {
                    ui.label(
                        egui::RichText::new(format!("{latest} is available"))
                            .size(12.0)
                            .color(theme.accent),
                    );
                }
                Some(crate::update::Check::UpToDate { latest }) => {
                    ui.weak(format!("up to date ({latest} is newest)"));
                }
                Some(crate::update::Check::Uncomparable { latest }) => {
                    ui.weak(format!("newest release: {latest}"));
                }
                Some(crate::update::Check::Failed { reason }) => {
                    ui.weak(format!("check failed: {reason}"));
                }
                None if self.update_checking => {}
                None => {
                    ui.weak("no check yet");
                }
            }
        });
        ui.horizontal(|ui| {
            if self.update_checking {
                ui.spinner();
                ui.label("checking…");
            } else if ui.small_button("Check now").clicked() {
                self.check_updates_now(true);
            }
            ui.weak(crate::update::last_check_label(
                self.last_update_check,
                crate::config::unix_now(),
            ));
        });
        hint(
            ui,
            theme,
            "Installing an update happens in the player (open the ? panel there).",
        );
        self.nav_row(ui, "Next →");
    }

    // ---- 6. audio check ----

    fn audio_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        step_body(ui, "Checking that Mordent can open an audio output device.");
        ui.add_space(12.0);
        match self.audio.clone() {
            Some(Ok(())) => {
                ui.label(
                    egui::RichText::new("Audio output: OK")
                        .size(14.0)
                        .color(theme.lossless),
                );
            }
            Some(Err(detail)) => {
                ui.label(
                    egui::RichText::new("No audio device found — nothing will be audible.")
                        .size(13.0)
                        .color(egui::Color32::from_rgb(0xf0, 0xc0, 0x60)),
                );
                hint(
                    ui,
                    theme,
                    "Mordent still runs and tracks advance silently. Plug in / enable an output and restart.",
                );
                if !detail.is_empty() {
                    hint(ui, theme, &detail);
                }
            }
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("probing the default output…");
                });
            }
        }
        self.nav_row(ui, "Next →");
    }

    // ---- 7. finish ----

    fn finish_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) -> Action {
        step_body(ui, "Here is what Mordent will use. These go to ~/.config/mordent/config.toml.");
        ui.add_space(10.0);
        let token = if self.media_token.trim().is_empty() {
            "not set".to_string()
        } else {
            "set".to_string()
        };
        let url = normalize_url(&self.url).unwrap_or_else(|| DEFAULT_URL.into());
        let path = match self.plan {
            Plan::Local => "installed helper on this computer",
            Plan::Skip => "previews only — no helper",
            Plan::Remote | Plan::Undecided => "helper URL",
        };
        for (k, v) in [
            ("playback", path.to_string()),
            ("wrapper", url),
            ("storefront", self.storefront.to_ascii_lowercase()),
            ("media-user-token", token),
            (
                "report plays",
                if self.report_plays { "on" } else { "off" }.to_string(),
            ),
            ("audio cache", format!("{} GB", self.cache_size_gb)),
        ] {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("{k}:")).color(theme.weak));
                ui.label(egui::RichText::new(v).strong());
            });
        }
        ui.add_space(14.0);
        let mut action = Action::None;
        if primary_button(ui, theme, "Start listening").clicked() {
            action = Action::Finish;
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!self.history.is_empty(), egui::Button::new("← Back"))
                .clicked()
            {
                self.back();
            }
            hint(ui, theme, "  re-run any time with:  mordent setup");
        });
        action
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mordent-setup-test-{}-{tag}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch(path: &Path) {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(path, "#!/bin/sh\n").unwrap();
    }

    // ---- bundle-dir lookup ----

    #[test]
    fn scripts_dir_prefers_exe_dir_scripts() {
        let root = tmpdir("scripts-a");
        let exe = root.join("bin");
        touch(&exe.join("scripts/install-wrapper.sh"));
        touch(&root.join("scripts/install-wrapper.sh")); // also present higher up
        let found = find_scripts_dir_in(&exe, &root).unwrap();
        assert_eq!(found, exe.join("scripts"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scripts_dir_falls_back_to_parent_then_cwd() {
        let root = tmpdir("scripts-b");
        let exe = root.join("bundle/usr/bin");
        // only <exe>/../scripts exists → one level up (the extracted bundle)
        std::fs::create_dir_all(&exe).unwrap();
        touch(&root.join("bundle/usr/scripts/install-wrapper.sh"));
        let found = find_scripts_dir_in(&exe, &tmpdir("nowhere")).unwrap();
        assert_eq!(found, exe.join("../scripts"));
        assert!(found.join("install-wrapper.sh").is_file());

        // nothing near the binary → the ./scripts fallback off the cwd
        let exe2 = root.join("empty/bin");
        std::fs::create_dir_all(&exe2).unwrap();
        let cwd = root.join("cwd");
        touch(&cwd.join("scripts/install-wrapper.sh"));
        let found = find_scripts_dir_in(&exe2, &cwd).unwrap();
        assert_eq!(found, cwd.join("scripts"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scripts_dir_none_when_no_script_anywhere() {
        let root = tmpdir("scripts-c");
        let exe = root.join("bin");
        std::fs::create_dir_all(exe.join("scripts")).unwrap(); // dir, no script
        assert!(find_scripts_dir_in(&exe, &root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bundle_dir_is_the_scripts_parent() {
        assert_eq!(bundle_dir(Path::new("/opt/mordent/scripts")), PathBuf::from("/opt/mordent"));
        assert_eq!(bundle_dir(Path::new("scripts")), PathBuf::from("."));
    }

    // ---- URL normalization ----

    #[test]
    fn normalize_url_fixes_scheme_and_slashes() {
        assert_eq!(normalize_url(" 127.0.0.1:12340 ").unwrap(), "http://127.0.0.1:12340");
        assert_eq!(normalize_url("http://127.0.0.1:12340/").unwrap(), "http://127.0.0.1:12340");
        assert_eq!(
            normalize_url("https://music-box.local:12340//").unwrap(),
            "https://music-box.local:12340"
        );
        assert_eq!(normalize_url("localhost").unwrap(), "http://localhost");
        assert_eq!(
            normalize_url("http://user@host:12340/base/").unwrap(),
            "http://user@host:12340/base"
        );
    }

    #[test]
    fn normalize_url_rejects_junk() {
        assert!(normalize_url("").is_none());
        assert!(normalize_url("   ").is_none());
        assert!(normalize_url("ftp://host").is_none());
        assert!(normalize_url("http://").is_none());
        assert!(normalize_url("http:///path").is_none());
        assert!(normalize_url("two words").is_none());
    }

    // ---- storefront ----

    #[test]
    fn storefront_validation() {
        assert_eq!(normalize_storefront("US").unwrap(), "us");
        assert_eq!(normalize_storefront(" jp ").unwrap(), "jp");
        assert!(storefront_ok("uk"));
        assert!(!storefront_ok("usa"));
        assert!(!storefront_ok("u1"));
        assert!(!storefront_ok(""));
        assert!(normalize_storefront("1").is_none());
    }

    // ---- base URL parse ----

    #[test]
    fn parse_base_url_takes_the_last_http_line() {
        let out = "[mordent-install] starting\nhttp://127.0.0.1:12340\nnoise trailing\n";
        assert_eq!(parse_base_url(out).unwrap(), "http://127.0.0.1:12340");
        assert_eq!(
            parse_base_url("https://box:12340/\n").unwrap(),
            "https://box:12340"
        );
        assert!(parse_base_url("nothing here\n").is_none());
    }

    // ---- 2FA detection ----

    #[test]
    fn twofa_line_detection() {
        assert!(line_needs_twofa(
            "[mordent-login] waiting up to 300s for the code"
        ));
        assert!(line_needs_twofa("Enter your 2FA code into /data/2fa.txt"));
        assert!(line_needs_twofa("two-factor authentication required"));
        assert!(!line_needs_twofa("[mordent-login] stopping mordent-wrapper for login"));
        assert!(!line_needs_twofa("[mordent-install] installing wrapper-lite"));
    }

    // ---- process specs: exact argv, program looked up by name ----

    // Unix-only: these assert the exact POSIX argv (paths joined with `/`) the
    // wizard hands to pkexec. On Windows `Path::join` yields `\`, so the string
    // comparison fails; the local-install/login steps are Linux-only by design.
    #[cfg(unix)]
    #[test]
    fn install_spec_argv_and_name_lookup() {
        let s = install_spec(Path::new("/bundle/scripts"), 12340);
        assert_eq!(s.program, "pkexec", "must be a bare name so PATH shims work");
        assert!(!s.program.starts_with('/'));
        assert_eq!(
            s.args,
            vec![
                "/bundle/scripts/install-wrapper.sh",
                "--src",
                "/bundle",
                "--port",
                "12340",
            ]
        );
        assert!(s.envs.is_empty());
    }

    // Unix-only: same POSIX-argv-string reason as `install_spec_argv_and_name_lookup`.
    #[cfg(unix)]
    #[test]
    fn login_spec_passes_password_by_file_not_env() {
        let s = login_spec(
            Path::new("/bundle/scripts"),
            "you@example.com",
            Path::new("/tmp/pw"),
            12340,
            DEFAULT_PREFIX,
        );
        assert_eq!(s.program, "pkexec");
        assert_eq!(
            s.args,
            vec![
                "/bundle/scripts/login-wrapper.sh",
                "--email",
                "you@example.com",
                "--password-file",
                "/tmp/pw",
                "--port",
                "12340",
                "--twofa-dir",
                "/opt/mordent-wrapper/rootfs/data",
            ]
        );
        assert!(
            s.envs.iter().all(|(k, _)| k != "MORDENT_WRAPPER_PASSWORD"),
            "pkexec discards the environment, so the password must not ride in it"
        );
    }

    #[test]
    fn twofa_spec_writes_the_polled_file() {
        let s = twofa_spec(Path::new("/opt/mordent-wrapper/rootfs/data/2fa.txt"), "123456");
        assert_eq!(s.program, "pkexec");
        assert_eq!(s.args[0], "sh");
        assert_eq!(s.args[1], "-c");
        assert!(s.args[2].contains("printf"));
        assert_eq!(s.args[4], "123456");
        assert_eq!(s.args[5], "/opt/mordent-wrapper/rootfs/data/2fa.txt");
        // no shell interpolation of the code: it is a positional $1
        assert!(!s.args[2].contains("123456"));
    }

    #[test]
    fn twofa_file_layout() {
        assert_eq!(
            twofa_file("/opt/mordent-wrapper"),
            PathBuf::from("/opt/mordent-wrapper/rootfs/data/2fa.txt")
        );
    }

    // ---- guards / transitions ----

    fn state() -> SetupState {
        // resolve_scripts_dir() reads the real environment; the guards below
        // do not depend on it.
        SetupState::new(&Config::default())
    }

    #[test]
    fn step_numbers_round_trip() {
        for n in 1..=7u8 {
            assert_eq!(Step::from_number(n).unwrap().number(), n);
        }
        assert!(Step::from_number(0).is_none());
        assert!(Step::from_number(8).is_none());
    }

    #[test]
    fn welcome_has_no_next_but_every_other_linear_step_does() {
        let mut s = state();
        assert!(!s.can_next());
        assert!(s.next_step().is_none());
        s.goto(Step::Prefs);
        assert!(s.can_next());
        assert_eq!(s.next_step(), Some(Step::Audio));
        s.goto(Step::Audio);
        assert_eq!(s.next_step(), Some(Step::Finish));
    }

    #[test]
    fn local_install_gate_needs_a_successful_install() {
        let mut s = state();
        s.goto(Step::LocalInstall);
        assert!(!s.can_next(), "cannot advance before the install succeeds");
        s.install_done = Some(ProcResult::failure(Some(126), None));
        assert!(!s.can_next());
        s.install_done = Some(ProcResult {
            ok: true,
            code: Some(0),
            base_url: Some(DEFAULT_URL.into()),
            error: None,
        });
        assert!(s.can_next());
        assert_eq!(s.next_step(), Some(Step::Login));
    }

    #[test]
    fn url_step_goes_to_login_only_for_an_unlogged_localhost() {
        let mut s = state();
        s.goto(Step::RemoteUrl);
        // remote URL → preferences
        s.url = "http://music-box.local:12340".into();
        s.probe = Some(Ok(vec!["us".into()]));
        assert_eq!(s.next_step(), Some(Step::Prefs));
        // localhost with regions → preferences too (it is signed in)
        if cfg!(target_os = "linux") {
            s.url = DEFAULT_URL.into();
            s.probe = Some(Ok(vec!["us".into()]));
            assert_eq!(s.next_step(), Some(Step::Prefs));
            // localhost, reachable but not signed in → the login step
            s.probe = Some(Ok(vec![]));
            assert_eq!(s.next_step(), Some(Step::Login));
        }
    }

    #[test]
    fn back_retraces_the_visited_path() {
        let mut s = state();
        s.goto(Step::RemoteUrl);
        s.goto(Step::Prefs);
        s.goto(Step::Audio);
        assert_eq!(s.step, Step::Audio);
        s.back();
        assert_eq!(s.step, Step::Prefs);
        s.back();
        assert_eq!(s.step, Step::RemoteUrl);
        s.back();
        assert_eq!(s.step, Step::Welcome);
        s.back(); // nothing left
        assert_eq!(s.step, Step::Welcome);
    }

    // ---- collected config ----

    #[test]
    fn new_config_applies_collected_values_and_keeps_uncollected() {
        let base = Config {
            cache_dir: PathBuf::from("/custom/cache"),
            storefront: "jp".into(),
            ..Default::default()
        };
        let mut s = SetupState::new(&base);
        s.url = "music-box.local:12340".into();
        s.storefront = "CA".into();
        s.media_token = "  ".into(); // blank → skipped
        s.report_plays = false;
        s.cache_size_gb = 3;
        let cfg = s.new_config(&base);
        assert_eq!(cfg.wrapper_url, "http://music-box.local:12340");
        assert_eq!(cfg.storefront, "ca");
        assert_eq!(cfg.media_user_token, None);
        assert!(!cfg.report_plays);
        assert_eq!(cfg.cache_size_gb, 3);
        // not collected by the wizard → inherited untouched
        assert_eq!(cfg.cache_dir, PathBuf::from("/custom/cache"));
    }

    // ---- the runner itself (fake program, no PATH tricks) ----

    // Unix-only: spawning a `#!/bin/sh` script relies on shebang exec, which
    // has no Windows equivalent. The local-install path this covers is
    // Linux-only by design (the wizard shows that step only on Linux).
    #[cfg(unix)]
    #[test]
    fn runner_streams_lines_and_parses_base_url() {
        let dir = tmpdir("runner");
        let script = dir.join("fake.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'hello stdout'\necho 'hello stderr' >&2\necho 'http://127.0.0.1:12340'\n",
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let (tx, rx) = channel();
        let spec = CmdSpec {
            program: script.to_string_lossy().into_owned(),
            args: vec![],
            envs: vec![],
        };
        let _h = spawn_proc(
            &spec,
            Tag::Install,
            tx,
            Box::new(|r: ProcResult, _out: String| SetupMsg::InstallDone(r)),
        )
        .unwrap();

        let mut lines = Vec::new();
        let done = loop {
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(SetupMsg::Log { line, .. }) => lines.push(line),
                Ok(SetupMsg::InstallDone(r)) => break r,
                Ok(_) => {}
                Err(e) => panic!("no done message: {e}"),
            }
        };
        assert!(lines.iter().any(|l| l == "hello stdout"));
        assert!(lines.iter().any(|l| l == "hello stderr"));
        assert!(done.ok);
        assert_eq!(done.code, Some(0));
        assert_eq!(done.base_url.as_deref(), Some("http://127.0.0.1:12340"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn runner_reports_missing_program() {
        let (tx, _rx) = channel();
        let spec = CmdSpec {
            program: "mordent-definitely-not-a-program-xyz".into(),
            args: vec![],
            envs: vec![],
        };
        let err = spawn_proc(
            &spec,
            Tag::Install,
            tx,
            Box::new(|r: ProcResult, _out: String| SetupMsg::InstallDone(r)),
        );
        assert!(err.is_err(), "missing program must surface as a spawn error");
    }

    #[test]
    fn failure_messages_are_actionable() {
        assert!(explain_failure(Some(126)).contains("dismissed"));
        assert!(explain_failure(Some(127)).contains("pkexec"));
        assert!(!explain_failure(Some(1)).is_empty());
    }
}
