//! Reachy Mini tray launcher.
//!
//! Cross-platform (macOS / Windows / Linux) Tauri 2 app whose only UI is a
//! tray icon + two transient webview windows (first-run setup + log viewer).
//!
//! What this crate owns:
//!
//! - Tray icon, dynamic menu, live status indicator (idle / starting /
//!   running / crashed), connection mode toggle (USB / Simulation).
//! - Lifecycle of the `uv-trampoline` sidecar (spawn / monitor / kill with
//!   process-group cleanup) and an HTTP healthcheck on
//!   `GET http://127.0.0.1:8000/api/daemon/status` to detect readiness.
//! - First-run window driven by `setup:progress` events derived from the
//!   trampoline's stdout milestones.
//! - Hugging Face account submenu (sign in / out / reconnect remote),
//!   delegating all OAuth + token storage to the daemon.
//! - Single-instance lock so two trays can't fight over the daemon.
//!
//! Module map:
//!
//! - [`state`]: shared FSM, `AppState`, accessors, `IconCache`.
//! - [`daemon`]: sidecar spawn / monitor / healthcheck / kill.
//! - [`tray_icon`]: pre-rendered status badges composed onto the bot face.
//! - [`tray_menu`]: dynamic menu construction + `refresh_status` entry point.
//! - [`commands`]: webview window helpers and Tauri IPC commands.
//! - [`hf_auth`]: Hugging Face OAuth orchestrator + status poller.
//! - [`logs`]: in-memory ring-buffer logger.
//! - [`paths`]: data-dir layout (shared with `reachy_mini_desktop_app`).
//!
//! Explicitly out of scope: auto-update, autostart-at-login, system
//! sleep/wake reconciliation, Windows / Linux code-signing pipelines.

mod commands;
mod daemon;
mod hf_auth;
mod logs;
mod paths;
mod state;
mod tray_icon;
mod tray_menu;

use std::sync::atomic::Ordering;

use tauri::tray::TrayIconBuilder;
use tauri::{Manager, RunEvent};

use crate::commands::{show_first_run_window, show_logs_window};
use crate::daemon::{kill_daemon, start_daemon, stop_daemon};
use crate::logs::LogStore;
use crate::state::{
    current_daemon_state, set_daemon_state, AppState, DaemonState, Mode, QUIT_REQUESTED,
};
use crate::tray_icon::build_icon_cache;
use crate::tray_menu::{
    build_tray_menu, refresh_status, ID_ACCOUNT_REFRESH_RELAY, ID_ACCOUNT_SIGNIN,
    ID_ACCOUNT_SIGNOUT, ID_ACCOUNT_SUBMENU, ID_MODE_SIM, ID_MODE_USB, ID_QUIT, ID_RESET_SETUP,
    ID_SHOW_LOGS, ID_TOGGLE, TRAY_ID,
};

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
        .manage(AppState::new())
        .manage(LogStore::new())
        .manage(hf_auth::AuthStatusStore::new())
        .invoke_handler(tauri::generate_handler![
            commands::close_first_run_window,
            commands::get_logs,
            commands::clear_logs
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
            // macOS: when the user closes the last window via the red dot,
            // do NOT terminate the process. Tray + LSUIElement keep us
            // headless.
            #[cfg(target_os = "macos")]
            RunEvent::Reopen { .. } => {
                log::debug!("reopen event received");
            }
            _ => {}
        });
}
