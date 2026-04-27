# reachy_mini_tray

A lightweight Tauri 2 menu-bar app that runs the [Reachy Mini](https://www.pollen-robotics.com/) Python daemon as a background service. It is deliberately tray-only (no main window after first-run setup) and is intended as a slimmer alternative to `reachy_mini_desktop_app` for users who only need to keep the daemon running so the robot is reachable from the [mobile app](https://github.com/pollen-robotics/reachy_mini_mobile_app) over Hugging Face's WebRTC relay.

## Features

- **Menu-bar only** on macOS (`LSUIElement = true`): no Dock icon, no Cmd-Tab entry.
- **Single Start / Stop / Restart toggle** with a live status indicator (idle / starting / running / crashed).
- **First-run bootstrap window** that streams `uv` / Python venv install progress and auto-closes once the daemon is up.
- **Hugging Face account integration**: sign in via OAuth, see `Signed in as @user · remote on/off` directly in the menu, force-refresh the central relay, sign out.
- **USB-only by design**: no Bluetooth, no LAN scanning. Mode toggle between `USB` and `Simulation`.
- **Built-in log viewer** (last 2000 lines, daemon + tray).
- **Reset setup**: wipes the data dir and re-bootstraps from scratch (only enabled while the daemon is idle/crashed).
- **Process-group lifecycle**: the trampoline is its own session leader (`setpgid`), so killing the tray cleanly takes down the entire Python subtree (no orphaned uvicorn, no stale port 8000).

## Architecture

```
┌────────────────────────────┐
│ reachy_mini_tray (Rust)    │  Tauri 2.x, no main window
│  • tray menu               │
│  • first-run window        │  WebView (UI in ./ui/)
│  • log viewer window       │
│  • HF OAuth orchestrator   │
└──────────────┬─────────────┘
               │ spawns + monitors (process group)
               ▼
┌────────────────────────────┐
│ uv-trampoline (sidecar)    │  ../reachy_mini_desktop_app/uv-wrapper
│  • bootstraps uv + venv    │
│  • installs reachy-mini    │
│  • exec's python -m daemon │
└──────────────┬─────────────┘
               │
               ▼
┌────────────────────────────┐
│ reachy_mini.daemon (Py)    │  HTTP API on 127.0.0.1:8000
│  • motors / kinematics     │
│  • WebRTC + central relay  │
│  • HF auth                 │
└────────────────────────────┘
```

The daemon is the same upstream package used by the desktop app and the wireless robot itself; this tray app just owns its lifecycle on the user's machine.

## Requirements

- macOS 12+ (Apple Silicon and Intel). Windows support is planned but not yet shipped.
- Rust toolchain (`rustup`), Node 18+, and the Tauri prerequisites listed at <https://tauri.app/start/prerequisites/>.
- A sibling checkout of [`reachy_mini_desktop_app`](https://github.com/pollen-robotics/reachy_mini_desktop_app) next to this repo. The `uv-trampoline` sidecar is built from `reachy_mini_desktop_app/uv-wrapper/` so both apps share a single bootstrap implementation.

```
parent/
├── reachy_mini_desktop_app/   ← required for the sidecar build
└── reachy_mini_tray/          ← this repo
```

## Quick start

```bash
# 1. Install JS deps
npm install

# 2. Build the uv-trampoline sidecar (default: latest reachy-mini from PyPI)
npm run build:sidecar

# 3. Run in dev mode
npm run dev
```

The first launch opens a setup window, downloads `uv`, creates a Python 3.12 venv in `~/Library/Application Support/com.pollen-robotics.reachy-mini/.venv`, installs `reachy-mini`, then closes itself. From then on the app is tray-only.

### Pinning a specific daemon version

The `reachy-mini` Python package can be baked-in at build time via the sidecar script:

```bash
# PyPI release pin
REACHY_MINI_VERSION=1.6.4 npm run build:sidecar

# A development branch on pollen-robotics/reachy_mini
npm run build:sidecar:develop
npm run build:sidecar:mobile-umbrella     # integration/mobile-app-daemon
npm run build:sidecar:branch <any-branch>
```

The marker `src-tauri/binaries/.reachy_mini_spec` records the chosen spec; running the script with a different spec will trigger an in-place venv upgrade on the next launch (or use `Reset setup…` from the tray for a clean reinstall).

## Daemon bootstrap smoke test

The tray has no main window to drive with `tauri-driver`, so instead of a
GUI E2E suite we ship a headless test that exercises the whole bootstrap
pipeline (uv download → Python install → venv → `pip install reachy-mini`
→ daemon HTTP up). It is the exact equivalent of "did the daemon install
work?" in the desktop app's E2E.

```bash
npm run test:daemon-bootstrap
# or, in CI:
bash ./scripts/test/test-daemon-bootstrap.sh
pwsh -File .\scripts\test\test-daemon-bootstrap.ps1   # Windows
```

The script redirects the platform's data dir to a fresh temp folder, so
it always starts from a clean install and never touches the real user
state. The daemon is launched in `--mockup-sim --no-media` mode (no
hardware, no GStreamer / portaudio at runtime). Cold-cache run is
~5 minutes; subsequent runs of the same script reuse nothing (each run
is fully isolated) so plan accordingly.

This same script runs on Linux, macOS, and Windows in
`.github/workflows/daemon-smoke.yml` on every push to `main` / `develop`.

## Build a release bundle

```bash
npm run build:sidecar
npm run build      # invokes `tauri build`
```

The `.app` bundle ends up in `src-tauri/target/release/bundle/macos/`. Code-signing and notarization use the standard Tauri configuration; see `src-tauri/tauri.conf.json`.

## Layout

```
reachy_mini_tray/
├── ui/                          # Static frontend
│   ├── index.html              # First-run bootstrap window
│   └── logs.html               # Log viewer window
├── scripts/
│   └── build-sidecar.sh        # Builds uv-trampoline → src-tauri/binaries/
└── src-tauri/
    ├── Cargo.toml
    ├── tauri.conf.json
    ├── build.rs
    ├── Info.plist               # LSUIElement = true
    ├── icons/
    ├── capabilities/
    ├── python-entitlements.plist
    └── src/
        ├── main.rs
        ├── lib.rs               # Tray + daemon lifecycle + first-run window
        ├── hf_auth.rs           # OAuth + status polling
        ├── logs.rs              # Ring buffer + log window IPC
        └── paths.rs             # Per-OS data-dir helpers
```

## Out of scope

Auto-update, autostart-at-login, system sleep/wake handling, Linux support, Windows code-signing pipeline. These are tracked separately and may land in future releases.

## License

MIT.
