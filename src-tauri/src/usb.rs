//! USB detection of Reachy Mini robots.
//!
//! Wraps the `serialport` crate to enumerate USB-serial devices and
//! filter the ones whose USB descriptor matches Reachy Mini's CH340
//! (`VID=1a86`, `PID=55d3`).
//!
//! This mirrors `reachy_mini.daemon.utils.find_serial_port()` (which uses
//! `pyserial` on the daemon side). Doing the detection on the tray side
//! lets us:
//!
//! - list robots in the menu **before** the daemon is even running
//!   (the daemon needs a port to start, not to be probed);
//! - tell the user "no Reachy Mini detected" without spawning Python /
//!   downloading a venv first;
//! - pre-select a default port and pass it to the daemon as
//!   `--serialport <path>`, removing the daemon's auto-discovery
//!   ambiguity when several robots are plugged in.
//!
//! The tray never opens the serial port itself: that's exclusively the
//! daemon's job (sharing the FD with two processes is a recipe for
//! protocol corruption).

use std::time::Duration;

use serialport::SerialPortType;
use tauri::{AppHandle, Manager};

use crate::state::{
    current_daemon_state, current_serialport, set_serialport, set_usb_devices, AppState,
    DaemonState,
};

/// Reachy Mini lite uses the WCH CH340 USB-to-UART bridge.
///
/// VID `0x1a86` / PID `0x55d3`. Hard-coded here so we can filter the OS
/// port list without touching the daemon. If a future Reachy Mini
/// revision changes the bridge chip, this constant is the only thing to
/// update on the tray side.
pub(crate) const REACHY_VID: u16 = 0x1a86;
pub(crate) const REACHY_PID: u16 = 0x55d3;

/// One detected Reachy Mini accessible over USB.
///
/// Cheap to clone and `Eq` so we can diff successive scans and only
/// rebuild the tray menu when the list actually changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbDevice {
    /// Full device path the daemon expects on its `--serialport` flag.
    /// On Unix this is something like `/dev/cu.usbserial-2120` or
    /// `/dev/ttyUSB0`; on Windows it's `COM3`.
    ///
    /// Already canonicalised to match what `pyserial` returns from
    /// `ListPortInfo.device`, since the daemon ultimately hands this
    /// straight to pyserial.
    pub serialport: String,
    /// Best-effort human label for the menu (`Reachy Mini · cu.usbserial-2120`,
    /// or with a serial-number suffix when several devices are plugged
    /// in to disambiguate them).
    pub label: String,
    /// USB serial number, if the OS exposed one. Used to dedupe ports
    /// that show up under multiple names (the macOS `tty.*` /  `cu.*`
    /// twin) and to disambiguate two physical robots.
    pub serial_number: Option<String>,
}

/// List currently-plugged Reachy Mini devices.
///
/// Returns an empty `Vec` on any enumeration error: a noisy /proc on
/// Linux or a missing libudev shouldn't crash the tray. The error is
/// logged at `debug` level so it's still discoverable from the in-app
/// log window.
pub(crate) fn list_reachy_devices() -> Vec<UsbDevice> {
    let ports = match serialport::available_ports() {
        Ok(p) => p,
        Err(e) => {
            log::debug!("serialport enumeration failed: {}", e);
            return Vec::new();
        }
    };

    let mut devices: Vec<UsbDevice> = ports
        .into_iter()
        .filter_map(|p| match p.port_type {
            SerialPortType::UsbPort(info) if info.vid == REACHY_VID && info.pid == REACHY_PID => {
                Some((p.port_name, info))
            }
            _ => None,
        })
        // On macOS each USB-serial device shows up twice: once as
        // `/dev/tty.usbserial-...` (callin, blocks until DCD) and once as
        // `/dev/cu.usbserial-...` (callout). The daemon needs the `cu.*`
        // variant - it doesn't wait on DCD which Reachy never asserts.
        // We keep only one row per physical device, preferring `cu.*`
        // when both are present (matches what `pyserial` reports too).
        .filter(|(name, _)| !is_macos_tty_twin(name))
        .map(|(name, info)| {
            let canonical = canonicalize_port_name(&name);
            let label = build_label(&canonical, &info);
            UsbDevice {
                serialport: canonical,
                label,
                serial_number: info.serial_number.clone(),
            }
        })
        .collect();

    // Stable order so the menu doesn't reshuffle between scans (which
    // would defeat the "list unchanged -> skip menu rebuild" guard in
    // the periodic scanner).
    devices.sort_by(|a, b| a.serialport.cmp(&b.serialport));
    devices
}

/// On macOS each USB-serial device is exposed twice under
/// `/dev/tty.*` and `/dev/cu.*`. Drop the `tty.*` variant (DCD-blocking,
/// not what the daemon wants).
#[cfg(target_os = "macos")]
fn is_macos_tty_twin(port_name: &str) -> bool {
    port_name.starts_with("/dev/tty.") || port_name.starts_with("tty.")
}

#[cfg(not(target_os = "macos"))]
fn is_macos_tty_twin(_port_name: &str) -> bool {
    false
}

/// Make sure the port name we hand to the daemon matches what
/// `pyserial.ListPortInfo.device` would return (Unix: absolute path,
/// Windows: bare `COM<n>` form). The `serialport` crate already
/// normalises this on every supported OS, so this is mostly a defensive
/// no-op + a single guard for the rare case where it returns a bare
/// `cu.usbserial-...` on macOS.
fn canonicalize_port_name(name: &str) -> String {
    #[cfg(unix)]
    {
        if name.starts_with("/dev/") {
            name.to_string()
        } else {
            format!("/dev/{}", name)
        }
    }

    #[cfg(not(unix))]
    {
        name.to_string()
    }
}

/// Build a short menu label like `Reachy Mini (cu.usbserial-2120)` or,
/// when several robots share the same product string, append the last 4
/// characters of the USB serial number for disambiguation.
fn build_label(serialport: &str, info: &serialport::UsbPortInfo) -> String {
    // Strip the leading `/dev/` (Unix) so labels stay short in the tray.
    let short = serialport
        .strip_prefix("/dev/")
        .unwrap_or(serialport)
        .to_string();
    match info.serial_number.as_deref() {
        Some(sn) if !sn.is_empty() => {
            // Most CH340 boards have an 8-char serial. Show the last 4 to
            // keep the label compact yet distinct.
            let tail = sn
                .chars()
                .rev()
                .take(4)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();
            format!("Reachy Mini ({} \u{00b7} {})", short, tail)
        }
        _ => format!("Reachy Mini ({})", short),
    }
}

/// Settle delay before we trust an enumeration after a hot-plug event.
///
/// On macOS / Linux a freshly plugged USB-serial device takes ~150-300 ms
/// to register a `/dev` node after the kernel sees it. Callers that want
/// to react to plug/unplug should wait at least this long between the
/// trigger event and the rescan to avoid flapping rows in the menu.
#[allow(dead_code)]
pub(crate) const HOTPLUG_SETTLE_DELAY: Duration = Duration::from_millis(300);

/// Cadence at which we rescan the USB bus while the daemon is `Idle` /
/// `Crashed`. Fast enough that hot-plugging a robot feels live; slow
/// enough that we don't burn a measurable amount of CPU on what is just
/// a `serialport::available_ports()` syscall.
const SCAN_INTERVAL_IDLE: Duration = Duration::from_secs(2);

/// Cadence while the daemon is `Starting` / `Running`. We don't show a
/// device picker in that state (the submenu is disabled), so there is
/// no UI reason to scan often. We still poll occasionally so that
/// returning to Idle picks up any unplug/replug that happened in the
/// meantime without a 2 s lag.
const SCAN_INTERVAL_BUSY: Duration = Duration::from_secs(15);

/// One full scan + cache update + reconcile + opt-in menu refresh
/// cycle. Returns `true` when the device list changed (caller may want
/// to refresh derived UI even outside of the tray menu).
///
/// Side-effects on top of the cache update:
///
/// - When the user-selected serialport disappears from the bus (cable
///   unplugged), we reset the selection to `None` so the menu doesn't
///   keep highlighting a ghost row. The mode is preserved as `USB`
///   (auto), giving the daemon a chance to pick whatever is plugged in
///   next.
/// - When the user has not yet selected anything (`serialport == None`)
///   and exactly one Reachy is plugged in, we auto-select it. This is
///   deliberately conservative (we never override an explicit choice).
///
/// Both behaviours only trigger while the daemon is `Idle` / `Crashed`
/// to avoid mutating the target under a running daemon.
pub(crate) fn scan_and_apply(app: &AppHandle) -> bool {
    let app_state = app.state::<AppState>();
    let state = current_daemon_state(&app_state);
    let busy = matches!(state, DaemonState::Starting | DaemonState::Running);

    let devices = list_reachy_devices();
    reconcile_after_scan(&app_state, &devices, busy);
    let changed = set_usb_devices(&app_state, devices);
    if changed {
        log::debug!("USB device list changed, refreshing tray menu");
        crate::tray_menu::request_menu_refresh(app);
    }
    changed
}

/// Spawn a long-lived background thread that keeps `AppState.usb_devices`
/// in sync with the live USB bus and triggers a tray-menu refresh when
/// the device list changes. See [`scan_and_apply`] for the per-tick
/// semantics.
pub(crate) fn start_scanner(app: AppHandle) {
    std::thread::spawn(move || loop {
        scan_and_apply(&app);

        let app_state = app.state::<AppState>();
        let busy = matches!(
            current_daemon_state(&app_state),
            DaemonState::Starting | DaemonState::Running
        );
        let interval = if busy {
            SCAN_INTERVAL_BUSY
        } else {
            SCAN_INTERVAL_IDLE
        };
        std::thread::sleep(interval);
    });
}

/// Apply the "auto-select single device / drop ghost selection" rules
/// after a fresh scan. Extracted from `start_scanner` so it can be
/// unit-tested without spawning a thread or a Tauri AppHandle.
///
/// Returns `()`; mutations are written back to `app_state.serialport`.
fn reconcile_after_scan(app_state: &AppState, devices: &[UsbDevice], busy: bool) {
    if busy {
        // Don't touch the selection while a daemon is running with it.
        return;
    }
    let selected = current_serialport(app_state);
    match (selected.as_deref(), devices) {
        // The user has not picked anything yet and there is exactly
        // one Reachy plugged in: pre-select it.
        (None, [only]) => {
            log::info!("auto-selecting single Reachy Mini on {}", only.serialport);
            set_serialport(app_state, Some(only.serialport.clone()));
        }
        // The previously selected port has disappeared (cable unplug)
        // and there is no obvious replacement.
        (Some(path), devs) if !devs.iter().any(|d| d.serialport == path) => {
            log::info!("USB device {} unplugged, clearing selection", path);
            set_serialport(app_state, None);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serialport::UsbPortInfo;

    fn fake_info(sn: Option<&str>) -> UsbPortInfo {
        UsbPortInfo {
            vid: REACHY_VID,
            pid: REACHY_PID,
            serial_number: sn.map(|s| s.to_string()),
            manufacturer: None,
            product: Some("USB2.0-Ser!".to_string()),
        }
    }

    #[test]
    fn label_without_serial_number_is_compact() {
        let info = fake_info(None);
        let label = build_label("/dev/cu.usbserial-2120", &info);
        assert_eq!(label, "Reachy Mini (cu.usbserial-2120)");
    }

    #[test]
    fn label_with_serial_number_shows_last_four_chars() {
        let info = fake_info(Some("ABCDEFGH"));
        let label = build_label("/dev/cu.usbserial-2120", &info);
        // Last 4 of "ABCDEFGH" is "EFGH".
        assert_eq!(label, "Reachy Mini (cu.usbserial-2120 \u{00b7} EFGH)");
    }

    #[test]
    fn label_with_short_serial_number_uses_full_value() {
        let info = fake_info(Some("AB"));
        let label = build_label("/dev/cu.usbserial-2120", &info);
        assert_eq!(label, "Reachy Mini (cu.usbserial-2120 \u{00b7} AB)");
    }

    #[test]
    fn canonicalize_port_name_prepends_dev_on_unix() {
        let canonical = canonicalize_port_name("cu.usbserial-2120");
        if cfg!(unix) {
            assert_eq!(canonical, "/dev/cu.usbserial-2120");
        } else {
            assert_eq!(canonical, "cu.usbserial-2120");
        }
    }

    #[test]
    fn canonicalize_port_name_keeps_absolute_paths() {
        let canonical = canonicalize_port_name("/dev/ttyUSB0");
        if cfg!(unix) {
            assert_eq!(canonical, "/dev/ttyUSB0");
        }
    }

    #[test]
    fn canonicalize_port_name_keeps_windows_com_unchanged() {
        let canonical = canonicalize_port_name("COM3");
        // On Unix this would get a "/dev/" prefix (defensive, never
        // happens in practice). On Windows the COM<n> form is preserved
        // verbatim, which is what pyserial reports.
        if cfg!(target_os = "windows") {
            assert_eq!(canonical, "COM3");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_tty_twin_is_filtered_out() {
        assert!(is_macos_tty_twin("/dev/tty.usbserial-2120"));
        assert!(is_macos_tty_twin("tty.usbserial-2120"));
        assert!(!is_macos_tty_twin("/dev/cu.usbserial-2120"));
        assert!(!is_macos_tty_twin("cu.usbserial-2120"));
    }

    #[test]
    fn list_reachy_devices_does_not_panic_in_test_env() {
        // Smoke test only: the host running the tests likely has no
        // Reachy plugged in. We only assert the call returns *something*
        // (an empty Vec is fine) without panicking.
        let _ = list_reachy_devices();
    }

    fn dev(path: &str) -> UsbDevice {
        UsbDevice {
            serialport: path.to_string(),
            label: format!("Reachy Mini ({})", path),
            serial_number: None,
        }
    }

    #[test]
    fn reconcile_auto_selects_single_device_when_unset() {
        let state = AppState::new();
        assert_eq!(current_serialport(&state), None);
        let only = dev("/dev/cu.usbserial-2120");
        reconcile_after_scan(&state, std::slice::from_ref(&only), false);
        assert_eq!(
            current_serialport(&state).as_deref(),
            Some("/dev/cu.usbserial-2120")
        );
    }

    #[test]
    fn reconcile_does_not_auto_select_when_multiple_devices() {
        let state = AppState::new();
        let devs = vec![dev("/dev/cu.usbserial-2120"), dev("/dev/cu.usbserial-1110")];
        reconcile_after_scan(&state, &devs, false);
        // Ambiguous: leave the choice to the user.
        assert_eq!(current_serialport(&state), None);
    }

    #[test]
    fn reconcile_clears_selection_when_device_unplugged() {
        let state = AppState::new();
        set_serialport(&state, Some("/dev/cu.usbserial-2120".to_string()));
        reconcile_after_scan(&state, &[], false);
        assert_eq!(current_serialport(&state), None);
    }

    #[test]
    fn reconcile_keeps_selection_when_still_present() {
        let state = AppState::new();
        set_serialport(&state, Some("/dev/cu.usbserial-2120".to_string()));
        let devs = vec![dev("/dev/cu.usbserial-2120"), dev("/dev/cu.usbserial-1110")];
        reconcile_after_scan(&state, &devs, false);
        assert_eq!(
            current_serialport(&state).as_deref(),
            Some("/dev/cu.usbserial-2120")
        );
    }

    #[test]
    fn reconcile_is_no_op_while_daemon_busy() {
        // The rule is: don't mutate the selection under a running daemon
        // (could crash mid-run). This holds even when the cabled device
        // disappears.
        let state = AppState::new();
        set_serialport(&state, Some("/dev/cu.usbserial-2120".to_string()));
        reconcile_after_scan(&state, &[], true);
        assert_eq!(
            current_serialport(&state).as_deref(),
            Some("/dev/cu.usbserial-2120")
        );
    }
}
