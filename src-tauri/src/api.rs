//! Daemon API constants and shared HTTP client factory.
//!
//! Every HTTP call from the tray targets the daemon's loopback FastAPI
//! server (`127.0.0.1:8000`). Centralising the base URL and the
//! `reqwest::blocking::Client` builder here:
//!
//! - removes per-call duplication (timeouts, builder errors handling);
//! - gives us a single place to flip if we ever change the bind port or
//!   add `User-Agent` / `Accept` defaults;
//! - keeps tests trivially shareable with the production code path.
//!
//! The clients we hand out are always blocking. HTTP traffic is loopback,
//! latency is sub-millisecond and the call sites (status poller, OAuth
//! flow, healthcheck) each run on their own dedicated `std::thread`.

use std::time::Duration;

use reqwest::blocking::Client;

/// Daemon FastAPI base URL. The daemon mounts every router under `/api/*`
/// (see `reachy_mini.daemon.app.main`), so callers append things like
/// `/daemon/status` or `/hf-auth/status` to this prefix.
pub(crate) const DAEMON_BASE_URL: &str = "http://127.0.0.1:8000/api";

/// Build a blocking HTTP client with the given total-request timeout and
/// no other defaults. Returns the underlying `reqwest::Error` as a
/// `String` so callers can surface it through their existing
/// `Result<_, String>` plumbing without depending on `reqwest::Error`
/// directly.
pub(crate) fn local_client(timeout: Duration) -> Result<Client, String> {
    Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("reqwest build: {}", e))
}
