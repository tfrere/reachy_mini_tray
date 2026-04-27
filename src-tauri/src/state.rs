//! Shared application state and lifecycle types.
//!
//! Holds the daemon FSM (`DaemonState`), the current connection mode
//! (`Mode`), the per-restart generation counter, the pre-rendered tray icon
//! cache (`IconCache`) and the tray-only `QUIT_REQUESTED` flag.
//!
//! ## Lock ordering
//!
//! `AppState` exposes several `Mutex`es. None of the current call sites
//! holds two of them at once, but if a future change ever needs to, locks
//! MUST be acquired in this order to avoid deadlocks:
//!
//! 1. `state.daemon`
//! 2. `state.mode`
//! 3. `state.state`
//! 4. `state.generation`
//!
//! Accessors below each take a single lock and release it before returning,
//! so external callers should generally not need to lock anything by hand.

use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

use tauri::image::Image;
use tauri_plugin_shell::process::CommandChild;

/// Set to `true` only when the user explicitly chooses Quit from the tray
/// menu. All other exit requests (last window closed, Cmd+Q on first-run
/// window, ...) are intercepted and cancelled to keep the tray alive.
pub(crate) static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// User-selected connection mode. Drives `--mockup-sim` on the daemon
/// command line and the colour of the tray status badge.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Usb,
    Simulation,
}

impl Mode {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Mode::Usb => "usb",
            Mode::Simulation => "simulation",
        }
    }
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Mode::Usb => "USB",
            Mode::Simulation => "Simulation",
        }
    }
}

/// Daemon lifecycle as projected to the user. Maps loosely to the FSM in
/// `reachy_mini_desktop_app::daemon::DaemonStatus` (Idle / Starting /
/// Running / Stopping / Crashed) - "Stopping" is collapsed into Idle for
/// tray UX since kill is fast (<200 ms in practice).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DaemonState {
    Idle,
    Starting,
    Running,
    Crashed,
}

/// Tauri-managed app state. Everything mutable that outlives a single
/// command call lives here.
pub struct AppState {
    /// Handle to the running `uv-trampoline` sidecar. `None` when daemon is
    /// idle / crashed / not-yet-started. We never hold this lock across a
    /// `.kill()` call: callers `take()` first then drop the guard.
    pub daemon: Mutex<Option<CommandChild>>,
    pub mode: Mutex<Mode>,
    pub state: Mutex<DaemonState>,
    /// Monotonically increases each time we (re)start a daemon. Used to
    /// discard late healthcheck / monitor callbacks from a previous run
    /// (e.g. user clicked Stop while bootstrap was still in progress).
    pub generation: Mutex<u64>,
}

impl AppState {
    pub(crate) fn new() -> Self {
        Self {
            daemon: Mutex::new(None),
            mode: Mutex::new(Mode::Usb),
            state: Mutex::new(DaemonState::Idle),
            generation: Mutex::new(0),
        }
    }
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

// ----- Accessors -----
//
// All accessors take a single lock and release it before returning. They
// favour a sane fallback over panicking on poisoning, except for
// `next_generation` / `current_generation` where a wrong reading would
// silently break the stale-event guard - panicking there surfaces a real
// bug instead of letting it corrupt user state.

pub(crate) fn current_mode(state: &AppState) -> Mode {
    state.mode.lock().map(|g| *g).unwrap_or(Mode::Usb)
}

pub(crate) fn current_daemon_state(state: &AppState) -> DaemonState {
    state.state.lock().map(|g| *g).unwrap_or(DaemonState::Idle)
}

pub(crate) fn set_daemon_state(state: &AppState, new_state: DaemonState) {
    if let Ok(mut g) = state.state.lock() {
        *g = new_state;
    }
}

pub(crate) fn next_generation(state: &AppState) -> u64 {
    let mut guard = state.generation.lock().expect("generation mutex");
    *guard = guard.wrapping_add(1);
    *guard
}

pub(crate) fn current_generation(state: &AppState) -> u64 {
    *state.generation.lock().expect("generation mutex")
}
