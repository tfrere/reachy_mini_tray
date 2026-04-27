// Reachy Mini tray launcher - MVP
//
// Validates: tray icon + real `uv-trampoline` sidecar lifecycle
// + transient first-run window with live bootstrap progress
// + USB / Simulation connection modes + live menu state updates
// + status icon variants (Idle / Starting / Running / Crashed)
// + HTTP healthcheck on `GET http://127.0.0.1:8000/daemon/status`
// + single-instance lock,
// on macOS, with no Dock entry (LSUIElement = true).
//
// Out of scope: auto-update, autostart-at-login, system sleep/wake
// reconciliation, Windows/Linux-specific signing & shortcuts.

mod hf_auth;
mod logs;
mod paths;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{
    image::Image,
    menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder, Wry,
};
use tauri_plugin_shell::{
    process::{CommandChild, CommandEvent},
    ShellExt,
};

use crate::logs::{LogEntry, LogStore};

/// Set to `true` only when the user explicitly chooses Quit from the tray
/// menu. All other exit requests (last window closed, Cmd+Q on first-run
/// window, ...) are intercepted and cancelled to keep the tray alive.
static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

const TRAY_ID: &str = "main";
const FIRST_RUN_WINDOW_LABEL: &str = "first-run";
const LOGS_WINDOW_LABEL: &str = "logs";

const ID_TOGGLE: &str = "toggle";
const ID_MODE_SUBMENU: &str = "mode";
const ID_MODE_USB: &str = "mode_usb";
const ID_MODE_SIM: &str = "mode_sim";
/// Top-level submenu shown when the user is logged in. Its label is the
/// live account status (e.g. `@tfrere · remote on`); children are the
/// secondary actions (Reconnect, Sign out).
const ID_ACCOUNT_SUBMENU: &str = "account";
/// Flat top-level item shown when the user is logged out (or while OAuth
/// is in flight). Click triggers `hf_auth::start_oauth_flow`.
const ID_ACCOUNT_SIGNIN: &str = "account_signin";
const ID_ACCOUNT_SIGNOUT: &str = "account_signout";
const ID_ACCOUNT_REFRESH_RELAY: &str = "account_refresh_relay";
const ID_SHOW_LOGS: &str = "show_logs";
const ID_RESET_SETUP: &str = "reset_setup";
const ID_QUIT: &str = "quit";

/// Daemon readiness endpoint. Returns 200 once the FastAPI app, the robot
/// backend and the IO layer are all initialised. Returns 503 / connection
/// refused before that.
///
/// Note the `/api` prefix: the daemon mounts every router under `/api/*`
/// (see `reachy_mini.daemon.app.main`). Hitting `/daemon/status` directly
/// returns 404 even when the daemon is live.
const HEALTHCHECK_URL: &str = "http://127.0.0.1:8000/api/daemon/status";

/// Poll cadence while the daemon is in `Starting` state.
const HEALTHCHECK_INTERVAL: Duration = Duration::from_millis(500);

/// Hard timeout for reaching `Running` after a Start. Sized to cover a fresh
/// `uv-trampoline` bootstrap on a slow first-run machine: uv download (~15 s)
/// + Python install (~30 s) + venv + reachy-mini install (~60 s) + GStreamer
/// pre-warm (~120 s) + headroom. Subsequent starts are typically <5 s.
const HEALTHCHECK_MAX_DURATION: Duration = Duration::from_secs(300);

/// Per-request HTTP timeout used while polling `/daemon/status`.
const HEALTHCHECK_HTTP_TIMEOUT: Duration = Duration::from_secs(2);

/// Minimum delay before we trust the first healthcheck success after a
/// daemon spawn. Defends against an "eager-zombie" race: if a previous
/// daemon's Python child was orphaned (e.g. user killed the tray during a
/// crash before we could clean its pgroup), it may still be listening on
/// 8000. A fresh `Start` would otherwise see that zombie answer 200 OK
/// within milliseconds and flip the tray to "Running USB" - except the
/// real new daemon we just spawned will crash 8 s later with `address
/// already in use`. The trampoline itself takes ~1 s to fork Python, and
/// Python takes another ~7 s to boot uvicorn, so anything succeeding
/// faster than this grace period is provably not us.
const HEALTHCHECK_GRACE: Duration = Duration::from_millis(1500);

/// Tauri event emitted with `BootstrapProgress` payload while
/// `uv-trampoline` is provisioning the venv. Listened to by the first-run
/// window to drive its progress bar.
const EVENT_SETUP_PROGRESS: &str = "setup:progress";

/// Tauri event emitted once the daemon transitions to `Running` (healthcheck
/// passed). The first-run window listens for this to flip into a "ready"
/// state and auto-close itself.
const EVENT_SETUP_DONE: &str = "setup:done";

// ============================================================================
// STATE
// ============================================================================

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Usb,
    Simulation,
}

impl Mode {
    fn as_str(&self) -> &'static str {
        match self {
            Mode::Usb => "usb",
            Mode::Simulation => "simulation",
        }
    }
    fn label(&self) -> &'static str {
        match self {
            Mode::Usb => "USB",
            Mode::Simulation => "Simulation",
        }
    }
}

/// Daemon lifecycle as projected to the user. Maps loosely to the FSM in
/// `reachy_mini_desktop_app::daemon::DaemonStatus` (Idle / Starting / Running /
/// Stopping / Crashed) - "Stopping" is collapsed into Idle for tray UX since
/// kill is fast (<200 ms in practice).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DaemonState {
    Idle,
    Starting,
    Running,
    Crashed,
}

pub struct AppState {
    /// Handle to the running `uv-trampoline` sidecar. `None` when daemon is
    /// idle/crashed/not-yet-started. We never hold this lock across a
    /// `.kill()` call: callers `take()` first then drop the guard.
    pub daemon: Mutex<Option<CommandChild>>,
    pub mode: Mutex<Mode>,
    pub state: Mutex<DaemonState>,
    /// Monotonically increases each time we (re)start a daemon. Used to
    /// discard late healthcheck / monitor callbacks from a previous run
    /// (e.g. user clicked Stop while bootstrap was still in progress).
    pub generation: Mutex<u64>,
}

/// Payload emitted on `setup:progress` events. Consumed by the first-run
/// window to drive a progress bar + status label + live console.
///
/// All fields are optional so each daemon line can carry partial info:
/// - a milestone match emits `{ percent: Some(_), label: Some(_), line: Some(_) }`,
/// - a generic `[bootstrap]` line emits `{ label: Some(_), line: Some(_) }` (no
///   percent change),
/// - any other daemon line during the first-run window emits `{ line: Some(_) }`
///   only, just so the live console keeps streaming.
///
/// `percent` is otherwise monotonic on the receiving side: the HTML guards
/// against backwards moves. The special value `Some(255)` is reserved as
/// "indeterminate" (pulse animation on the bar) - used during the long
/// GStreamer plugin-registry scan where no fine-grained progress is
/// available.
#[derive(Clone, Default, serde::Serialize)]
pub struct BootstrapProgress {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
}

/// Pre-rendered tray icons. Generated once in `setup()` from the bundled
/// default icon. `Image<'static>` is cheap to clone (just a slice +
/// dimensions); the underlying RGBA bytes are leaked one-shot at startup.
pub struct IconCache {
    pub idle: Image<'static>,
    pub starting: Image<'static>,
    pub running_usb: Image<'static>,
    pub running_sim: Image<'static>,
    pub crashed: Image<'static>,
}

// ============================================================================
// SIDECAR LIFECYCLE (real `uv-trampoline`)
// ============================================================================

/// Build the args passed to `uv-trampoline`. Mirrors
/// `reachy_mini_desktop_app::python::build_daemon_args()` minus the bits we
/// don't need (no Avast SSL wrapper on Windows because we never ship there
/// in this MVP, no `--preload-datasets` until we want the extra startup time).
///
/// The trampoline interprets `args[0]` as the Python interpreter path
/// (relative to its data dir) and execs it with `args[1..]` once bootstrap
/// is complete. Hence the leading `.venv/bin/python3`.
fn build_daemon_args(mode: Mode) -> Vec<String> {
    #[cfg(target_os = "windows")]
    let python = ".venv\\Scripts\\python.exe";
    #[cfg(not(target_os = "windows"))]
    let python = ".venv/bin/python3";

    let mut args = vec![
        python.to_string(),
        "-m".to_string(),
        "reachy_mini.daemon.app.main".to_string(),
        // Marker flag: lets the daemon know it was spawned by a desktop app
        // wrapper (us). Used by `apps.py` for app-discovery routing.
        "--desktop-app-daemon".to_string(),
        // Service starts in "sleeping" state - no torque, no fan. The user
        // can wake the robot from a future menu item or via the daemon API.
        "--no-wake-up-on-start".to_string(),
    ];

    if matches!(mode, Mode::Simulation) {
        args.push("--mockup-sim".to_string());
    }

    args
}

/// Spawn the `uv-trampoline` sidecar for the requested connection mode.
/// Returns the live `CommandChild` (so we can `.kill()` it later) and side
/// effects: forks an async monitor task that consumes stdout/stderr/term
/// events from the child, pushes them to the in-app logs window, parses
/// bootstrap milestones for the first-run progress bar, and reacts to
/// process termination (Crashed transitions, etc).
fn spawn_real_daemon(app: &AppHandle, mode: Mode, generation: u64) -> Result<CommandChild, String> {
    let args = build_daemon_args(mode);
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    let cmd = app
        .shell()
        .sidecar("uv-trampoline")
        .map_err(|e| format!("sidecar lookup failed: {}", e))?
        .args(arg_refs)
        // Force UTF-8 in Python's print/logging so we never get mojibake in
        // the in-app logs window on locales where the system default isn't
        // UTF-8 (rare on macOS, common on Windows).
        .env("PYTHONIOENCODING", "utf-8");

    let (rx, child) = cmd.spawn().map_err(|e| format!("sidecar spawn failed: {}", e))?;

    let app_clone = app.clone();
    tauri::async_runtime::spawn(async move {
        monitor_daemon_output(app_clone, rx, generation).await;
    });

    Ok(child)
}

/// Async loop that drains the sidecar's event stream until process termination.
/// Owns the `Receiver<CommandEvent>` produced by `Command::spawn()` and is
/// the *only* place that reacts to `CommandEvent::Terminated`.
///
/// Generation guard: a stale Terminated event from a previous daemon run
/// (e.g. after a Stop -> Start sequence) must NOT crash the new one. We
/// compare the captured generation to `AppState.generation` and bail out
/// silently if they diverge.
async fn monitor_daemon_output(
    app: AppHandle,
    mut rx: tauri::async_runtime::Receiver<CommandEvent>,
    generation: u64,
) {
    while let Some(event) = rx.recv().await {
        match event {
            CommandEvent::Stdout(bytes) => {
                let line = String::from_utf8_lossy(&bytes).to_string();
                handle_daemon_line(&app, &line, "INFO");
            }
            CommandEvent::Stderr(bytes) => {
                let line = String::from_utf8_lossy(&bytes).to_string();
                // Default to WARN: most stderr noise is uvicorn / Python
                // warnings. `parse_line_level` upgrades to ERROR when an
                // explicit marker is present.
                handle_daemon_line(&app, &line, "WARN");
            }
            CommandEvent::Terminated(payload) => {
                log::info!(
                    "daemon terminated (gen={}, code={:?}, signal={:?})",
                    generation,
                    payload.code,
                    payload.signal
                );

                let app_state = app.state::<AppState>();
                if current_generation(&app_state) != generation {
                    log::debug!(
                        "ignoring stale Terminated event (captured_gen={}, current_gen={})",
                        generation,
                        current_generation(&app_state)
                    );
                    continue;
                }

                if let Ok(mut guard) = app_state.daemon.lock() {
                    guard.take();
                }

                // Only crash the FSM if we were actively running/starting.
                // If the user already clicked Stop, `kill_daemon` set state
                // to Idle and we should respect that.
                let cur = current_daemon_state(&app_state);
                if matches!(cur, DaemonState::Starting | DaemonState::Running) {
                    set_daemon_state(&app_state, DaemonState::Crashed);
                    refresh_status(&app);
                }
            }
            _ => {}
        }
    }
}

/// Single ingestion point for any line emitted by the trampoline / Python
/// daemon: pushes it to the in-app log window and (when applicable) emits
/// a `setup:progress` event for the first-run window.
///
/// While the first-run window is open, EVERY daemon line is forwarded as a
/// `setup:progress` event with at least a `line` field, so the window's
/// live mini-console keeps streaming even between known milestones (e.g.
/// during the 4-minute GStreamer plugin-registry scan, where the only
/// signal is the throttled `still working... 60s` heartbeat).
fn handle_daemon_line(app: &AppHandle, raw_line: &str, default_level: &str) {
    let trimmed = raw_line.trim_end();
    if trimmed.is_empty() {
        return;
    }
    let level = logs::parse_line_level(trimmed, default_level);
    logs::push_external(app, "daemon", &level, trimmed.to_string());

    let first_run_open = app.get_webview_window(FIRST_RUN_WINDOW_LABEL).is_some();
    if !first_run_open {
        return;
    }

    let mut progress = derive_bootstrap_event(trimmed);
    progress.line = Some(trimmed.to_string());
    if let Err(e) = app.emit(EVENT_SETUP_PROGRESS, &progress) {
        log::warn!("failed to emit setup:progress: {}", e);
    }
}

/// Sentinel `percent` value meaning "indeterminate progress, please show a
/// pulse animation". Used during long opaque steps (GStreamer plugin
/// registry scan) where we have no granular signal but want to convey that
/// the daemon is still alive.
const PROGRESS_INDETERMINATE: u8 = 255;

/// Heuristic mapping from `uv-trampoline` and daemon log lines to a
/// progress percentage + a humanized step label.
///
/// Design choices:
/// - The percentages are **anchors**, not a true linear progress bar. They
///   exist purely to tell the user "we're not stuck" (the hard part of
///   first-run UX is the 4 min GStreamer scan, where no useful percentage
///   exists at all).
/// - Each `[prewarm:*]` sub-step gets its own label so the user always
///   sees something change at least once per minute.
/// - Lines with no recognized marker return `BootstrapProgress::default()`
///   (no percent, no label) - the caller will still attach `line` so the
///   live console shows the raw output.
///
/// Order matters: the most specific patterns must be checked first.
fn derive_bootstrap_event(line: &str) -> BootstrapProgress {
    let lower = line.to_ascii_lowercase();
    let mk = |percent: u8, label: &str| BootstrapProgress {
        percent: Some(percent),
        label: Some(label.to_string()),
        line: None,
    };
    let label_only = |label: &str| BootstrapProgress {
        percent: None,
        label: Some(label.to_string()),
        line: None,
    };

    // ---------- Bootstrap milestones (uv-trampoline) ----------
    if lower.contains("setup complete!") {
        return mk(98, "Setup complete - starting daemon\u{2026}");
    }
    if lower.contains("pre-warming complete") {
        return mk(95, "Pre-warm complete");
    }
    // Pre-warm sub-steps emitted by `[prewarm:.venv]` / `[prewarm:apps_venv]`.
    // Tracked individually because the registry scan alone takes ~4 min on
    // a fresh install and a single static label would feel frozen.
    if lower.contains("reachy_mini imported") {
        return mk(94, "Reachy Mini SDK loaded");
    }
    if lower.contains("importing reachy_mini") {
        return mk(92, "Loading Reachy Mini SDK\u{2026}");
    }
    if lower.contains("gstreamer ready") {
        return mk(90, "GStreamer ready");
    }
    if lower.contains("scanning plugin registry") || lower.contains("initializing gstreamer") {
        return mk(
            PROGRESS_INDETERMINATE,
            "Scanning GStreamer plugins (first run, ~3 min)\u{2026}",
        );
    }
    if lower.contains("[prewarm") && lower.contains("importing gi") {
        return mk(82, "Pre-warming Python bindings\u{2026}");
    }
    // The trampoline's `still working...` heartbeat (every 5s during pre-warm).
    // Don't move the bar, just keep the label informative without re-overwriting
    // it with anything more specific that the renderer might already show.
    if lower.contains("pre-warming python imports (still working") {
        return BootstrapProgress::default();
    }
    if lower.contains("pre-warming gstreamer") {
        return mk(80, "Pre-warming GStreamer\u{2026}");
    }
    if lower.contains("[bootstrap] pre-warming") {
        return mk(78, "Pre-warming runtime\u{2026}");
    }
    if lower.contains("[bootstrap] signing") {
        return mk(72, "Signing Python binaries\u{2026}");
    }
    if lower.contains("packages installed successfully") {
        return mk(65, "Packages installed");
    }
    if lower.contains("creating apps_venv") {
        return mk(55, "Creating apps environment\u{2026}");
    }
    if lower.contains("installing reachy-mini") || lower.contains("installing reachy_mini") {
        return mk(40, "Installing reachy-mini and dependencies\u{2026}");
    }
    if lower.contains("creating .venv") {
        return mk(30, "Creating virtual environment\u{2026}");
    }
    if lower.contains("installing python") {
        return mk(20, "Installing Python 3.12\u{2026}");
    }
    if lower.contains("downloading uv") {
        return mk(12, "Downloading package manager (uv)\u{2026}");
    }
    if lower.contains("uv downloaded successfully") {
        return mk(15, "Package manager ready");
    }
    if lower.contains("first run detected") {
        return mk(8, "First run detected, preparing environment\u{2026}");
    }

    // ---------- Daemon-runtime hints (after `Setup complete!`) ----------
    // These don't move the bar (we're at 98 %) but give the user a sense
    // that "starting daemon..." is not stuck either.
    if lower.contains("starting reachy mini daemon") {
        return label_only("Starting daemon\u{2026}");
    }
    if lower.contains("found reachy mini serial port") {
        return label_only("Reachy Mini detected on USB");
    }
    if lower.contains("creating robotbackend") {
        return label_only("Initializing robot backend\u{2026}");
    }
    if lower.contains("uvicorn") && lower.contains("started server process") {
        return label_only("HTTP server started");
    }

    BootstrapProgress::default()
}

/// Spawn a dedicated thread that polls `/daemon/status` until the daemon
/// becomes ready, fails to come up within `HEALTHCHECK_MAX_DURATION`, or its
/// generation moves on (Stop / Restart). Transitions Starting -> Running on
/// the first 200 OK and Starting -> Crashed on hard timeout.
///
/// Plain blocking `reqwest`: HTTP is loopback, latency is sub-ms, and we
/// don't want to share a tokio runtime with the sidecar event loop.
fn start_healthcheck(app: AppHandle, generation: u64) {
    std::thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(HEALTHCHECK_HTTP_TIMEOUT)
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                log::error!("failed to build healthcheck client: {}", e);
                return;
            }
        };
        let started = Instant::now();

        loop {
            let app_state = app.state::<AppState>();

            // Generation moved on — Stop / Restart was issued. Bail out.
            if current_generation(&app_state) != generation {
                log::debug!("healthcheck thread exiting (gen mismatch)");
                return;
            }

            // FSM moved away from Starting — we're done one way or another.
            let cur = current_daemon_state(&app_state);
            if !matches!(cur, DaemonState::Starting) {
                log::debug!("healthcheck thread exiting (state={:?})", cur);
                return;
            }

            if started.elapsed() > HEALTHCHECK_MAX_DURATION {
                log::error!(
                    "healthcheck timed out after {:?}",
                    HEALTHCHECK_MAX_DURATION
                );
                set_daemon_state(&app_state, DaemonState::Crashed);
                refresh_status(&app);
                return;
            }

            match client.get(HEALTHCHECK_URL).send() {
                Ok(resp) if resp.status().is_success() => {
                    let elapsed = started.elapsed();
                    if elapsed < HEALTHCHECK_GRACE {
                        // Almost certainly a leftover daemon from a previous
                        // run still bound to 8000. Don't trust it: stay in
                        // Starting and keep polling. Our just-spawned Python
                        // hasn't even finished booting uvicorn yet.
                        log::warn!(
                            "healthcheck answered too fast ({:?} < {:?}) - probable zombie daemon, ignoring",
                            elapsed,
                            HEALTHCHECK_GRACE
                        );
                        std::thread::sleep(HEALTHCHECK_INTERVAL);
                        continue;
                    }
                    log::info!(
                        "daemon healthcheck OK (gen={}, latency={:?})",
                        generation,
                        elapsed
                    );
                    set_daemon_state(&app_state, DaemonState::Running);
                    refresh_status(&app);
                    // Tell the first-run window (if still open) it can flip
                    // into the "ready" state and auto-close itself. Safe to
                    // emit unconditionally; the event is a no-op when the
                    // window doesn't exist.
                    if let Err(e) = app.emit(EVENT_SETUP_DONE, ()) {
                        log::debug!("failed to emit {}: {}", EVENT_SETUP_DONE, e);
                    }
                    return;
                }
                Ok(resp) => {
                    log::debug!("healthcheck non-2xx: {}", resp.status());
                }
                Err(_) => {
                    // Connection refused / not-yet-listening / DNS — silent.
                    // The daemon's own logs will surface real failures.
                }
            }

            std::thread::sleep(HEALTHCHECK_INTERVAL);
        }
    });
}

/// Kill the running daemon (if any), best-effort. Idempotent.
///
/// On Unix, the trampoline `setpgid(0, 0)`s itself at startup, so its pid
/// equals its process group id. Killing the **group** (not just the
/// trampoline) is essential to also bring down the Python child it spawned;
/// otherwise Python would survive, keep `127.0.0.1:8000` bound and the USB
/// serial port held, breaking every subsequent restart.
///
/// We send SIGTERM first to give Python ~250 ms to flush logs and unbind
/// sockets cleanly, then SIGKILL the group as a hard fallback.
fn kill_daemon(state: &AppState) {
    let child = match state.daemon.lock() {
        Ok(mut g) => g.take(),
        Err(e) => {
            log::error!("daemon mutex poisoned: {}", e);
            return;
        }
    };
    let Some(child) = child else { return };
    let pid = child.pid();
    log::info!("killing daemon pid={}", pid);

    #[cfg(unix)]
    {
        // Phase 1: graceful SIGTERM to the whole subtree.
        // killpg() takes the *process group id*, which - thanks to setpgid
        // in the trampoline - is the trampoline pid itself.
        kill_process_group(pid as i32, libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(250));
    }

    // Phase 2: hard kill the trampoline (also delivers SIGKILL via the
    // shell-plugin's owned `tokio::process::Child` so it can reap properly
    // and we don't leak FDs from the piped stdout/stderr streams).
    if let Err(e) = child.kill() {
        log::warn!("kill failed (pid={}): {}", pid, e);
    }

    #[cfg(unix)]
    {
        // Phase 3: belt-and-braces SIGKILL on the group. Catches any
        // grandchild that survived because Python forked workers (rare,
        // but uvicorn's `--reload` mode does it).
        kill_process_group(pid as i32, libc::SIGKILL);
    }
}

/// Find any process currently bound to `127.0.0.1:8000` and kill it with
/// SIGKILL. Used as a pre-flight by `start_daemon` to clean up zombies left
/// over from crashed previous runs (the kind that show up as
/// `[Errno 48] address already in use` in the daemon logs).
///
/// We shell out to `lsof` because it's the only universally-available way
/// on macOS (no `/proc/net/tcp`) to map a TCP port to its owner pid without
/// adding a 2 MB networking crate. `lsof -nP -iTCP:8000 -sTCP:LISTEN -t`
/// prints one pid per line and exits 1 if nothing matches - both are fine.
///
/// Belt-and-braces: also nukes the killed pid's process group, so if the
/// zombie was itself a parent (e.g. an old trampoline), its Python child
/// goes down too.
#[cfg(unix)]
fn reap_orphaned_daemons() {
    let output = match std::process::Command::new("lsof")
        .args(["-nP", "-iTCP:8000", "-sTCP:LISTEN", "-t"])
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            log::debug!("lsof not available, skipping zombie sweep: {}", e);
            return;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pids: Vec<i32> = stdout
        .lines()
        .filter_map(|s| s.trim().parse::<i32>().ok())
        .collect();

    if pids.is_empty() {
        return;
    }

    log::warn!(
        "found {} orphan process(es) on TCP/8000, killing: {:?}",
        pids.len(),
        pids
    );

    for pid in pids {
        // SAFETY: kill(2)/killpg(2) are libc syscalls.
        unsafe {
            // First the whole group (catches forked workers).
            let pgid = libc::getpgid(pid);
            if pgid > 1 {
                libc::killpg(pgid, libc::SIGKILL);
            }
            // Then the pid itself, in case its pgid was the tray's own
            // group (unlikely but cheap to guard against).
            libc::kill(pid, libc::SIGKILL);
        }
    }

    // Give the OS ~200 ms to actually release the listening socket before
    // the caller tries to bind to it. Without this, the next `uvicorn`
    // start can still race-lose the port.
    std::thread::sleep(Duration::from_millis(200));
}

/// `killpg(pgid, sig)` wrapper that defends against the most embarrassing
/// failure mode: pgid == 0 ("our own process group"), which would kill the
/// tray app itself. Returns silently on any error.
#[cfg(unix)]
fn kill_process_group(pgid: i32, sig: i32) {
    if pgid <= 1 {
        log::warn!("refusing killpg with pgid={}", pgid);
        return;
    }
    // SAFETY: killpg(2) is a libc syscall, no Rust invariants to uphold.
    let rc = unsafe { libc::killpg(pgid, sig) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        // ESRCH "no such process group" is the happy path on the second
        // call (everything is already dead). Don't spam the log for it.
        if err.raw_os_error() != Some(libc::ESRCH) {
            log::debug!("killpg(pgid={}, sig={}) failed: {}", pgid, sig, err);
        }
    }
}

fn current_mode(state: &AppState) -> Mode {
    state.mode.lock().map(|g| *g).unwrap_or(Mode::Usb)
}

fn current_daemon_state(state: &AppState) -> DaemonState {
    state.state.lock().map(|g| *g).unwrap_or(DaemonState::Idle)
}

fn set_daemon_state(state: &AppState, new_state: DaemonState) {
    if let Ok(mut g) = state.state.lock() {
        *g = new_state;
    }
}

fn next_generation(state: &AppState) -> u64 {
    let mut guard = state.generation.lock().expect("generation mutex");
    *guard = guard.wrapping_add(1);
    *guard
}

fn current_generation(state: &AppState) -> u64 {
    *state.generation.lock().expect("generation mutex")
}

// ============================================================================
// ICON COMPOSITION (status pill in the bottom-right corner)
// ============================================================================

/// Composite a colored status disc onto a copy of the base RGBA buffer.
///
/// Renders an anti-aliased disc + an outer ring of the menu-bar background
/// (transparent) that gives the pill a subtle "lift" off the bot face.
/// Non-template (set `icon_as_template(false)` on the tray when using the
/// result).
fn compose_with_dot(base_rgba: &[u8], width: u32, height: u32, color: [u8; 3]) -> Vec<u8> {
    let mut pixels = base_rgba.to_vec();
    let w = width as f32;
    let h = height as f32;
    // ~26 % diameter: small enough to read as a badge but visible at the
    // 16 px effective menu-bar size on retina. Inspired by Slack / Linear /
    // Things status badges.
    let radius = w.min(h) * 0.13;
    // 1 px ring of transparency around the disc to detach it cleanly.
    let ring = (w.min(h) * 0.025).max(1.0);
    let pad = w.min(h) * 0.05 + ring;
    let cx = w - radius - pad;
    let cy = h - radius - pad;
    let r_outer = radius;
    let r_ring_outer = radius + ring;

    for y in 0..height {
        for x in 0..width {
            let dx = x as f32 + 0.5 - cx;
            let dy = y as f32 + 0.5 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let idx = ((y * width + x) * 4) as usize;

            if dist >= r_ring_outer {
                continue;
            }

            // Coverage of the disc: 1.0 inside, 0.0 outside, smoothed over a
            // 1 px AA band. Coverage of the ring: 1.0 in [r_outer, r_outer+ring].
            let disc_cov = ((r_outer + 0.5 - dist) / 1.0).clamp(0.0, 1.0);
            let ring_cov = if dist > r_outer {
                ((r_ring_outer - dist) / 1.0).clamp(0.0, 1.0) * (1.0 - disc_cov)
            } else {
                0.0
            };

            // Disc: alpha-composite colour over background.
            if disc_cov > 0.0 {
                let a = (disc_cov * 255.0) as u32;
                let inv = 255 - a;
                let bg_a = pixels[idx + 3] as u32;
                pixels[idx] =
                    ((color[0] as u32 * a + pixels[idx] as u32 * inv) / 255) as u8;
                pixels[idx + 1] =
                    ((color[1] as u32 * a + pixels[idx + 1] as u32 * inv) / 255) as u8;
                pixels[idx + 2] =
                    ((color[2] as u32 * a + pixels[idx + 2] as u32 * inv) / 255) as u8;
                pixels[idx + 3] = (a + (bg_a * inv) / 255).min(255) as u8;
            }

            // Ring: erase background alpha to "punch" a clean separation.
            if ring_cov > 0.0 {
                let keep = ((1.0 - ring_cov) * 255.0) as u32;
                pixels[idx + 3] = ((pixels[idx + 3] as u32 * keep) / 255) as u8;
            }
        }
    }

    pixels
}

fn into_static_image(rgba: Vec<u8>, width: u32, height: u32) -> Image<'static> {
    let leaked: &'static [u8] = Box::leak(rgba.into_boxed_slice());
    Image::new(leaked, width, height)
}

fn build_icon_cache(base: &Image<'_>) -> IconCache {
    let base_rgba = base.rgba().to_vec();
    let w = base.width();
    let h = base.height();

    // Apple system semantic colours (dark-mode variants, slightly more vivid;
    // they read fine on light menu bars too).
    let orange = [0xFF, 0x9F, 0x0A];
    let green = [0x30, 0xD1, 0x58];
    let blue = [0x0A, 0x84, 0xFF];
    let red = [0xFF, 0x45, 0x3A];

    IconCache {
        idle: into_static_image(base_rgba.clone(), w, h),
        starting: into_static_image(compose_with_dot(&base_rgba, w, h, orange), w, h),
        running_usb: into_static_image(compose_with_dot(&base_rgba, w, h, green), w, h),
        running_sim: into_static_image(compose_with_dot(&base_rgba, w, h, blue), w, h),
        crashed: into_static_image(compose_with_dot(&base_rgba, w, h, red), w, h),
    }
}

// ============================================================================
// TRAY: TOOLTIP + LIVE MENU + ICON
// ============================================================================

fn refresh_status(app: &AppHandle) {
    let app_state = app.state::<AppState>();
    let state = current_daemon_state(&app_state);
    let mode = current_mode(&app_state);

    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };

    // ---- Icon ----
    let cache = app.state::<IconCache>();
    let (icon, template) = match (state, mode) {
        (DaemonState::Idle, _) => (cache.idle.clone(), true),
        (DaemonState::Starting, _) => (cache.starting.clone(), false),
        (DaemonState::Running, Mode::Usb) => (cache.running_usb.clone(), false),
        (DaemonState::Running, Mode::Simulation) => (cache.running_sim.clone(), false),
        (DaemonState::Crashed, _) => (cache.crashed.clone(), false),
    };
    if let Err(e) = tray.set_icon(Some(icon)) {
        log::warn!("set_icon failed: {}", e);
    }
    if let Err(e) = tray.set_icon_as_template(template) {
        log::warn!("set_icon_as_template failed: {}", e);
    }

    // ---- Tooltip ----
    let tooltip = match state {
        DaemonState::Idle => format!("Reachy Mini - Idle ({})", mode.label()),
        DaemonState::Starting => format!("Reachy Mini - Starting ({})...", mode.label()),
        DaemonState::Running => format!("Reachy Mini - Running ({})", mode.label()),
        DaemonState::Crashed => format!("Reachy Mini - Crashed ({})", mode.label()),
    };
    if let Err(e) = tray.set_tooltip(Some(&tooltip)) {
        log::warn!("set_tooltip failed: {}", e);
    }

    // ---- Menu (rebuilt from current daemon + auth state) ----
    //
    // We rebuild the whole menu instead of mutating individual items so
    // the account slot can flip between a flat "Sign in..." MenuItem
    // and a "@user · remote on" Submenu without juggling visibility
    // tricks. Refresh frequency is gated by signature comparisons in
    // `start_status_poller` and by daemon-state transitions, so we don't
    // hammer this path on idle.
    let snap = app
        .try_state::<hf_auth::AuthStatusStore>()
        .map(|s| s.snapshot())
        .unwrap_or_default();
    match build_tray_menu(app, state, mode, &snap) {
        Ok(menu) => {
            if let Err(e) = tray.set_menu(Some(menu)) {
                log::warn!("set_menu failed: {}", e);
            }
        }
        Err(e) => log::warn!("build_tray_menu failed: {}", e),
    }
}

/// Possible shapes for the "Hugging Face account" slot in the tray menu.
/// Each tray refresh re-derives this from `(state, snap)` and rebuilds
/// the menu accordingly. Topology changes (flat MenuItem <-> Submenu) are
/// not expressible via `set_text` alone, so a full rebuild is the simpler
/// honest path; refreshes are gated by `start_status_poller`'s signature
/// check, so we don't actually rebuild on every poll tick.
enum AccountSlot {
    /// Daemon not running: no row at all.
    Hidden,
    /// Logged out (or OAuth in flight). Flat top-level `MenuItem`.
    Flat(MenuItem<Wry>),
    /// Logged in. Submenu whose label is the live account status; its
    /// children are the secondary actions.
    Sub(Submenu<Wry>),
}

/// Build the user-visible label for the "logged in" submenu.
///
/// Format: `@{user} · {relay summary}`. The relay summary is intentionally
/// short so the submenu title fits in a typical macOS tray menu width.
fn logged_in_label(snap: &hf_auth::AuthSnapshot) -> String {
    let user = snap
        .auth
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("?");

    let relay = match snap.relay.as_ref() {
        Some(r) if r.is_connected => "remote on".to_string(),
        Some(r) => match r.state.as_str() {
            "connecting" | "reconnecting" => "reconnecting\u{2026}".to_string(),
            "unavailable" => "remote unavailable".to_string(),
            "" => "remote unknown".to_string(),
            other => format!("remote {}", other),
        },
        None => "remote unknown".to_string(),
    };

    format!("@{} \u{00b7} {}", user, relay)
}

/// Pick the right account slot for the current `(state, auth snapshot)`.
fn account_slot(
    app: &AppHandle,
    state: DaemonState,
    snap: &hf_auth::AuthSnapshot,
) -> tauri::Result<AccountSlot> {
    if !matches!(state, DaemonState::Running) {
        return Ok(AccountSlot::Hidden);
    }

    if snap.oauth_in_flight {
        // Disabled flat row that doubles as a status indicator while the
        // OAuth round-trip is in flight in the user's browser.
        let item = MenuItem::with_id(
            app,
            ID_ACCOUNT_SIGNIN,
            "Signing in\u{2026} (check your browser)",
            false,
            None::<&str>,
        )?;
        return Ok(AccountSlot::Flat(item));
    }

    if !snap.auth.is_logged_in {
        let item = MenuItem::with_id(
            app,
            ID_ACCOUNT_SIGNIN,
            "Sign in with Hugging Face\u{2026}",
            true,
            None::<&str>,
        )?;
        return Ok(AccountSlot::Flat(item));
    }

    // Logged in: build the submenu.
    let refresh_relay = MenuItem::with_id(
        app,
        ID_ACCOUNT_REFRESH_RELAY,
        "Reconnect remote access",
        true,
        None::<&str>,
    )?;
    let signout = MenuItem::with_id(
        app,
        ID_ACCOUNT_SIGNOUT,
        "Sign out",
        true,
        None::<&str>,
    )?;
    let label = logged_in_label(snap);
    let sub = Submenu::with_id_and_items(
        app,
        ID_ACCOUNT_SUBMENU,
        &label,
        true,
        &[&refresh_relay, &signout],
    )?;
    Ok(AccountSlot::Sub(sub))
}

/// Build a fresh tray menu reflecting the given `(state, mode, snap)`.
///
/// We rebuild instead of mutating so the topology of the account slot can
/// flip between a flat `MenuItem` ("Sign in...") and a `Submenu` ("@user
/// · remote on" with children) without juggling visibility hacks (which
/// `muda` doesn't support cleanly).
fn build_tray_menu(
    app: &AppHandle,
    state: DaemonState,
    mode: Mode,
    snap: &hf_auth::AuthSnapshot,
) -> tauri::Result<Menu<Wry>> {
    // ---- Toggle (Start / Stop / Restart) ----
    let (toggle_text, toggle_enabled) = match state {
        DaemonState::Idle => ("Start daemon", true),
        DaemonState::Starting => ("Starting\u{2026}", false),
        DaemonState::Running => ("Stop daemon", true),
        DaemonState::Crashed => ("Restart daemon", true),
    };
    let toggle = MenuItem::with_id(app, ID_TOGGLE, toggle_text, toggle_enabled, None::<&str>)?;

    // ---- Connection mode submenu ----
    let busy = matches!(state, DaemonState::Starting | DaemonState::Running);
    let mode_usb = CheckMenuItem::with_id(
        app,
        ID_MODE_USB,
        "USB (default)",
        true,
        mode == Mode::Usb,
        None::<&str>,
    )?;
    let mode_sim = CheckMenuItem::with_id(
        app,
        ID_MODE_SIM,
        "Simulation",
        true,
        mode == Mode::Simulation,
        None::<&str>,
    )?;
    let mode_submenu = Submenu::with_id_and_items(
        app,
        ID_MODE_SUBMENU,
        "Connection mode",
        !busy,
        &[&mode_usb, &mode_sim],
    )?;

    // ---- Account slot ----
    let account = account_slot(app, state, snap)?;

    // ---- Footer ----
    let show_logs = MenuItem::with_id(app, ID_SHOW_LOGS, "Show logs\u{2026}", true, None::<&str>)?;
    // Reset setup wipes the daemon's data dir; doing it while the daemon
    // is `Starting` or `Running` would race against open file handles
    // (venv binaries currently exec'd, sqlite locks, serial port, etc.).
    // Only allow it from `Idle` / `Crashed`.
    let reset_setup = MenuItem::with_id(
        app,
        ID_RESET_SETUP,
        "Reset setup\u{2026}",
        !busy,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, ID_QUIT, "Quit", true, None::<&str>)?;

    // Predefined separators are cheap and `muda` requires distinct
    // instances per insertion site. Build all we might need up-front.
    let sep_top = PredefinedMenuItem::separator(app)?;
    let sep_account = PredefinedMenuItem::separator(app)?;
    let sep_footer = PredefinedMenuItem::separator(app)?;
    let sep_quit = PredefinedMenuItem::separator(app)?;

    let mut items: Vec<&dyn tauri::menu::IsMenuItem<Wry>> =
        vec![&toggle, &sep_top, &mode_submenu];

    match &account {
        AccountSlot::Flat(item) => {
            items.push(&sep_account);
            items.push(item);
        }
        AccountSlot::Sub(sub) => {
            items.push(&sep_account);
            items.push(sub);
        }
        AccountSlot::Hidden => {}
    }

    items.push(&sep_footer);
    items.push(&show_logs);
    items.push(&reset_setup);
    items.push(&sep_quit);
    items.push(&quit);

    Menu::with_items(app, &items)
}

/// Re-renders the live tray menu (icon, tooltip, all items) on demand.
/// Called by background workers in the `hf_auth` module after fetching a
/// new snapshot or completing an OAuth flow. Cheap; safe to spam.
pub fn request_menu_refresh(app: &AppHandle) {
    refresh_status(app);
}

// ============================================================================
// WINDOWS (first-run + logs)
// ============================================================================

fn show_first_run_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(FIRST_RUN_WINDOW_LABEL) {
        existing.show()?;
        existing.set_focus()?;
        return Ok(());
    }

    WebviewWindowBuilder::new(
        app,
        FIRST_RUN_WINDOW_LABEL,
        WebviewUrl::App("index.html".into()),
    )
    .title("Reachy Mini - First-time setup")
    .inner_size(520.0, 460.0)
    .min_inner_size(440.0, 380.0)
    .resizable(true)
    .center()
    .visible(true)
    .build()?;
    Ok(())
}

fn show_logs_window(app: &AppHandle) -> tauri::Result<()> {
    if let Some(existing) = app.get_webview_window(LOGS_WINDOW_LABEL) {
        existing.show()?;
        existing.set_focus()?;
        return Ok(());
    }

    WebviewWindowBuilder::new(
        app,
        LOGS_WINDOW_LABEL,
        WebviewUrl::App("logs.html".into()),
    )
    .title("Reachy Mini - Logs")
    .inner_size(720.0, 480.0)
    .min_inner_size(420.0, 240.0)
    .resizable(true)
    .center()
    .visible(true)
    .build()?;
    Ok(())
}

// ============================================================================
// TAURI COMMANDS (called from windows via IPC)
// ============================================================================

#[tauri::command]
fn close_first_run_window(app: AppHandle) {
    // Click on Done is just a UI dismissal. The "bootstrap is done" signal
    // is the presence of `.venv/bin/python3` on disk (see
    // `paths::is_bootstrap_done`), written by `uv-trampoline` once the venv
    // is fully provisioned. Closing the window early without a complete
    // venv simply means the next launch reopens it.
    if let Some(win) = app.get_webview_window(FIRST_RUN_WINDOW_LABEL) {
        let _ = win.close();
    }
}

#[tauri::command]
fn get_logs(store: State<'_, LogStore>) -> Vec<LogEntry> {
    store.snapshot()
}

#[tauri::command]
fn clear_logs(store: State<'_, LogStore>) {
    store.clear();
}

// ============================================================================
// START / STOP HELPERS
// ============================================================================

fn start_daemon(app: &AppHandle) {
    let app_state = app.state::<AppState>();
    if matches!(
        current_daemon_state(&app_state),
        DaemonState::Starting | DaemonState::Running
    ) {
        log::info!("daemon already busy, ignoring Start");
        return;
    }
    let mode = current_mode(&app_state);

    // Pre-flight: kill any pre-existing daemon left over from a crash or a
    // previous version of the tray that didn't use process-group cleanup.
    // Without this, our just-spawned Python would die with `address already
    // in use` on port 8000 ~8 s after the user clicks Start.
    #[cfg(unix)]
    reap_orphaned_daemons();

    set_daemon_state(&app_state, DaemonState::Starting);
    let gen = next_generation(&app_state);
    refresh_status(app);

    match spawn_real_daemon(app, mode, gen) {
        Ok(child) => {
            let pid = child.pid();
            log::info!(
                "daemon spawned pid={} mode={} gen={}",
                pid,
                mode.as_str(),
                gen
            );
            if let Ok(mut guard) = app_state.daemon.lock() {
                *guard = Some(child);
            }
        }
        Err(e) => {
            log::error!("failed to spawn daemon: {}", e);
            set_daemon_state(&app_state, DaemonState::Crashed);
            refresh_status(app);
            return;
        }
    }

    // Real readiness probe: poll `GET /daemon/status` every
    // HEALTHCHECK_INTERVAL until 200 OK, max HEALTHCHECK_MAX_DURATION.
    // Generation guard inside the thread filters out stale signals after
    // Stop / Restart sequences.
    start_healthcheck(app.clone(), gen);
}

fn stop_daemon(app: &AppHandle) {
    let app_state = app.state::<AppState>();
    next_generation(&app_state);
    kill_daemon(&app_state);
    set_daemon_state(&app_state, DaemonState::Idle);
    refresh_status(app);
}

// ============================================================================
// ENTRY POINT
// ============================================================================

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    logs::init();

    tauri::Builder::default()
        // Single-instance MUST be the first plugin registered (per the
        // plugin's documentation), otherwise rival processes can grab
        // resources before the lock is acquired. The callback fires in the
        // *first* (alive) instance whenever a second one is launched - we
        // log it and surface the existing process by opening the logs window.
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            log::warn!(
                "single-instance: rejected duplicate launch (args={:?}, cwd={})",
                args,
                cwd
            );
            if let Err(e) = show_logs_window(app) {
                log::warn!("single-instance: failed to surface logs window: {}", e);
            }
        }))
        .plugin(tauri_plugin_shell::init())
        .manage(AppState {
            daemon: Mutex::new(None),
            mode: Mutex::new(Mode::Usb),
            state: Mutex::new(DaemonState::Idle),
            generation: Mutex::new(0),
        })
        .manage(LogStore::new())
        .manage(hf_auth::AuthStatusStore::new())
        .invoke_handler(tauri::generate_handler![
            close_first_run_window,
            get_logs,
            clear_logs
        ])
        .setup(|app| {
            let app_handle = app.handle().clone();

            // Wire the global logger to the running app so subsequent
            // log records also feed the in-app logs window.
            logs::bind_app_handle(&app_handle);

            // ---- Initial tray menu ----
            //
            // The full menu is rebuilt on every `refresh_status` call, so
            // this is just the boot-time snapshot (Idle daemon + USB +
            // empty auth). Topology and labels will adjust as soon as
            // state changes.
            let initial_snap = hf_auth::AuthSnapshot::default();
            let menu = build_tray_menu(
                &app.handle().clone(),
                DaemonState::Idle,
                Mode::Usb,
                &initial_snap,
            )?;

            // ---- Icon cache ----
            let base_icon = app
                .default_window_icon()
                .ok_or("missing default window icon")?
                .clone();
            let cache = build_icon_cache(&base_icon);
            let initial_icon = cache.idle.clone();
            app.manage(cache);

            // ---- Tray icon ----
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(initial_icon)
                .icon_as_template(true)
                .tooltip("Reachy Mini - Idle (USB)")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    ID_TOGGLE => {
                        let app_state = app.state::<AppState>();
                        match current_daemon_state(&app_state) {
                            DaemonState::Idle | DaemonState::Crashed => start_daemon(app),
                            DaemonState::Running => stop_daemon(app),
                            // Disabled while Starting; defensive no-op.
                            DaemonState::Starting => {}
                        }
                    }
                    ID_MODE_USB => {
                        let app_state = app.state::<AppState>();
                        if matches!(
                            current_daemon_state(&app_state),
                            DaemonState::Starting | DaemonState::Running
                        ) {
                            log::info!("ignored mode change: daemon busy");
                            return;
                        }
                        if let Ok(mut guard) = app_state.mode.lock() {
                            *guard = Mode::Usb;
                        }
                        log::info!("mode set to USB");
                        refresh_status(app);
                    }
                    ID_MODE_SIM => {
                        let app_state = app.state::<AppState>();
                        if matches!(
                            current_daemon_state(&app_state),
                            DaemonState::Starting | DaemonState::Running
                        ) {
                            log::info!("ignored mode change: daemon busy");
                            return;
                        }
                        if let Ok(mut guard) = app_state.mode.lock() {
                            *guard = Mode::Simulation;
                        }
                        log::info!("mode set to Simulation");
                        refresh_status(app);
                    }
                    ID_ACCOUNT_SIGNIN => {
                        hf_auth::start_oauth_flow(app.clone());
                    }
                    ID_ACCOUNT_SIGNOUT => {
                        hf_auth::sign_out(app);
                    }
                    ID_ACCOUNT_REFRESH_RELAY => {
                        hf_auth::refresh_relay(app);
                    }
                    ID_ACCOUNT_SUBMENU => {
                        // The "@user · remote on" item is a Submenu; macOS
                        // routes its click to its children, but on some
                        // platforms / older muda builds the submenu itself
                        // can fire a hover event. Defensive no-op.
                    }
                    ID_SHOW_LOGS => {
                        if let Err(e) = show_logs_window(app) {
                            log::warn!("failed to show logs window: {}", e);
                        }
                    }
                    ID_RESET_SETUP => {
                        // Stop the daemon first, otherwise removing the venv
                        // it depends on yields confusing errors in the logs.
                        let app_state = app.state::<AppState>();
                        kill_daemon(&app_state);
                        set_daemon_state(&app_state, DaemonState::Idle);
                        refresh_status(app);
                        match paths::reset_bootstrap() {
                            Ok(()) => log::info!("venv wiped; relaunching first-run setup"),
                            Err(e) => log::warn!("failed to wipe venv: {}", e),
                        }
                        if let Err(e) = show_first_run_window(app) {
                            log::warn!("failed to show first-run window: {}", e);
                        }
                        // Kick the bootstrap immediately, same as the
                        // first-launch path. Without this the venv stays
                        // empty and the first-run window sits at 0% forever
                        // until the user manually clicks Start daemon -
                        // surprising UX for a "Reset" action.
                        start_daemon(app);
                    }
                    ID_QUIT => {
                        log::info!("quit requested");
                        QUIT_REQUESTED.store(true, Ordering::SeqCst);
                        let app_state = app.state::<AppState>();
                        kill_daemon(&app_state);
                        app.exit(0);
                    }
                    other => log::warn!("unknown menu event: {}", other),
                })
                .build(app)?;

            // ---- HF account status poller ----
            //
            // Single long-lived blocking thread that polls the daemon's
            // `/api/hf-auth/status` and `/api/hf-auth/relay-status` while
            // it's `Running` and refreshes the account submenu labels.
            // Idles (slower cadence, no HTTP) when the daemon is down.
            hf_auth::start_status_poller(app_handle.clone());

            // ---- First-run window + auto-bootstrap ----
            //
            // Detection rule mirrors `uv_wrapper::venv_exists()` from the
            // desktop app: if `.venv/bin/python3` exists in the shared data
            // dir, bootstrap is considered complete and we boot straight
            // into the tray (idle state, daemon not started; the user picks
            // when to launch it from the menu).
            //
            // A partial bootstrap (interrupted by quit/crash/power loss)
            // leaves no Python binary, so the next launch automatically
            // restarts setup. No sentinel file is needed.
            //
            // On first launch we auto-call `start_daemon()`: the same
            // `uv-trampoline` binary that runs the Python module is also
            // responsible for bootstrapping the venv when missing. So a
            // first-run "Start" implicitly downloads uv, installs Python,
            // creates the venv, installs reachy-mini and finally starts
            // the daemon - all observable through the progress bar in the
            // first-run window via `setup:progress` events.
            if paths::is_bootstrap_done() {
                log::info!(
                    "bootstrap detected, skipping setup window ({})",
                    paths::venv_python_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<no path>".into())
                );
            } else {
                log::info!(
                    "first launch detected, opening setup window (data dir: {})",
                    paths::data_dir()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "<no path>".into())
                );
                show_first_run_window(&app_handle)?;
                // Kick off the bootstrap immediately. Default mode is USB;
                // the user can switch to Simulation later from the tray
                // menu (which will trigger a restart of the daemon).
                start_daemon(&app_handle);
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| match event {
            // Block the default "exit when last window is closed" behaviour.
            // The tray must keep running until the user explicitly clicks Quit.
            RunEvent::ExitRequested { api, code, .. } => {
                if QUIT_REQUESTED.load(Ordering::SeqCst) || code.is_some() {
                    log::info!("exit confirmed - cleaning up daemon");
                    let state = app_handle.state::<AppState>();
                    kill_daemon(&state);
                } else {
                    log::debug!("exit request intercepted - tray stays alive");
                    api.prevent_exit();
                }
            }
            // macOS: when the user closes the last window via the red dot, do
            // NOT terminate the process. Tray + LSUIElement keep us headless.
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => {
                log::debug!("reopen event received");
            }
            _ => {}
        });
}
