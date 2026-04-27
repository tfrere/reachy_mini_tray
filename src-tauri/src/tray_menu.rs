//! Tray icon + tooltip + dynamic menu refresh.
//!
//! The menu is rebuilt from scratch on every state change rather than
//! mutated in place. This is required because the "Hugging Face account"
//! slot needs to flip between a flat `MenuItem` ("Sign in...") and a
//! `Submenu` ("@user · remote on") - a topology change `muda` doesn't
//! support cleanly. Refreshes are gated by the `last_signature` check in
//! `hf_auth::start_status_poller` and by daemon FSM transitions, so we
//! don't actually rebuild the menu on every poll tick.

use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Manager, Wry};

use crate::hf_auth;
use crate::state::{current_daemon_state, current_mode, AppState, DaemonState, IconCache, Mode};

pub(crate) const TRAY_ID: &str = "main";

pub(crate) const ID_TOGGLE: &str = "toggle";
pub(crate) const ID_MODE_SUBMENU: &str = "mode";
pub(crate) const ID_MODE_USB: &str = "mode_usb";
pub(crate) const ID_MODE_SIM: &str = "mode_sim";
/// Top-level submenu shown when the user is logged in. Its label is the
/// live account status (e.g. `@tfrere · remote on`); children are the
/// secondary actions (Reconnect, Sign out).
pub(crate) const ID_ACCOUNT_SUBMENU: &str = "account";
/// Flat top-level item shown when the user is logged out (or while OAuth
/// is in flight). Click triggers `hf_auth::start_oauth_flow`.
pub(crate) const ID_ACCOUNT_SIGNIN: &str = "account_signin";
pub(crate) const ID_ACCOUNT_SIGNOUT: &str = "account_signout";
pub(crate) const ID_ACCOUNT_REFRESH_RELAY: &str = "account_refresh_relay";
pub(crate) const ID_SHOW_LOGS: &str = "show_logs";
pub(crate) const ID_RESET_SETUP: &str = "reset_setup";
pub(crate) const ID_QUIT: &str = "quit";

/// Top-level entry point: re-render icon, tooltip and menu from the live
/// `AppState` + `AuthStatusStore` snapshot. Cheap; safe to spam.
pub(crate) fn refresh_status(app: &AppHandle) {
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

    // ---- Menu ----
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

/// Re-renders the live tray menu (icon, tooltip, all items) on demand.
/// Called by background workers in `hf_auth` after fetching a new snapshot
/// or completing an OAuth flow. Cheap; safe to spam.
pub fn request_menu_refresh(app: &AppHandle) {
    refresh_status(app);
}

/// Possible shapes for the "Hugging Face account" slot in the tray menu.
/// Each tray refresh re-derives this from `(state, snap)` and rebuilds
/// the menu accordingly. Topology changes (flat MenuItem <-> Submenu) are
/// not expressible via `set_text` alone.
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

    let refresh_relay = MenuItem::with_id(
        app,
        ID_ACCOUNT_REFRESH_RELAY,
        "Reconnect remote access",
        true,
        None::<&str>,
    )?;
    let signout = MenuItem::with_id(app, ID_ACCOUNT_SIGNOUT, "Sign out", true, None::<&str>)?;
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
pub(crate) fn build_tray_menu(
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

    let mut items: Vec<&dyn tauri::menu::IsMenuItem<Wry>> = vec![&toggle, &sep_top, &mode_submenu];

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
